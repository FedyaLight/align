//! Native QuickTime / ISO base media (MOV, MP4, M4V) header reader.
//!
//! Reads the `moov` box only: track kinds, audio sample descriptions, video
//! dimensions, sample tables, edit-free presentation timing, and the first
//! QuickTime timecode sample. Media data is never scanned, so a probe costs
//! one small read instead of an external process and a packet walk.
//!
//! Anything this reader does not understand (fragmented or compressed movie
//! headers, malformed tables) returns `None`, and callers fall back to the
//! FFmpeg tier. The same tables drive the stream-copy writer in
//! [`crate::remux`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use align_core::{MediaTime, SourceTimecode, canonical_frame_duration};

use crate::backend::{AudioStreamProbe, ProbeReport, VideoProbe};
use crate::ff::PacketTiming;

/// Largest movie header accepted (sample tables of multi-hour takes are a
/// few MiB; anything larger is not a camera file).
const MAX_MOOV_BYTES: u64 = 256 * 1024 * 1024;

pub fn is_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["mov", "mp4", "m4v"].contains(&e.to_ascii_lowercase().as_str()))
}

// ------------------------------------------------------------ box cursor

/// Minimal big-endian reader over an in-memory box payload.
#[derive(Clone, Copy)]
pub(crate) struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn fourcc(&mut self) -> Option<[u8; 4]> {
        self.take(4)?.try_into().ok()
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// One child box: type, full bytes (header included), and payload.
#[derive(Clone, Copy)]
pub(crate) struct Atom<'a> {
    pub kind: [u8; 4],
    pub raw: &'a [u8],
    pub body: &'a [u8],
}

/// Iterate the boxes packed in `data`. Stops at the first malformed header.
pub(crate) fn atoms(data: &[u8]) -> impl Iterator<Item = Atom<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let mut b = Bytes::new(data.get(pos..)?);
        let size = b.u32()? as u64;
        let kind = b.fourcc()?;
        let (header, size) = match size {
            0 => (8, (data.len() - pos) as u64),
            1 => (16, b.u64()?),
            n => (8, n),
        };
        let size = usize::try_from(size).ok()?;
        if size < header {
            return None;
        }
        let raw = data.get(pos..pos.checked_add(size)?)?;
        pos += size;
        Some(Atom {
            kind,
            raw,
            body: &raw[header..],
        })
    })
}

pub(crate) fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<Atom<'a>> {
    atoms(data).find(|a| &a.kind == kind)
}

fn path<'a>(data: &'a [u8], kinds: &[&[u8; 4]]) -> Option<Atom<'a>> {
    let (first, rest) = kinds.split_first()?;
    let mut atom = child(data, first)?;
    for kind in rest {
        atom = child(atom.body, kind)?;
    }
    Some(atom)
}

/// Full-box version/flags, then the remaining payload.
fn full_box(body: &[u8]) -> Option<(u8, Bytes<'_>)> {
    let mut b = Bytes::new(body);
    let version = b.u8()?;
    b.skip(3)?;
    Some((version, b))
}

// ------------------------------------------------------------ model

#[derive(Clone, Debug, PartialEq)]
pub struct AudioEntry {
    pub sample_rate: f64,
    pub channels: usize,
    /// Present for linear PCM only (compressed codecs have no bit depth).
    pub bit_depth: Option<u32>,
    pub is_float: Option<bool>,
    /// Byte order of linear PCM samples.
    pub big_endian: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimecodeEntry {
    pub drop_frame: bool,
    pub timescale: u32,
    pub frame_duration: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Timecode,
    Other,
}

/// Sample-to-chunk run: chunks from `first_chunk` (1-based) hold
/// `samples_per_chunk` samples each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRun {
    pub first_chunk: u32,
    pub samples_per_chunk: u32,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    /// First sample description: its four-character code and full bytes.
    pub codec: [u8; 4],
    pub sample_entry: Vec<u8>,
    pub timescale: u32,
    pub media_duration: u64,
    /// Presented duration from the edit list, in the movie timescale.
    pub edited_duration: Option<u64>,
    /// Media time where presentation starts (first non-empty edit), in the
    /// track timescale. AAC encoder priming is skipped this way.
    pub media_start: u64,
    pub width: u32,
    pub height: u32,
    pub audio: Option<AudioEntry>,
    pub timecode: Option<TimecodeEntry>,
    /// (count, delta) decode-time runs.
    pub time_to_sample: Vec<(u32, u32)>,
    /// (count, offset) composition offsets; empty when absent.
    pub composition_offsets: Vec<(u32, i32)>,
    pub sample_sizes: SampleSizes,
    pub chunk_runs: Vec<ChunkRun>,
    pub chunk_offsets: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SampleSizes {
    Constant { size: u32, count: u32 },
    Table(Vec<u32>),
}

impl SampleSizes {
    pub fn count(&self) -> usize {
        match self {
            Self::Constant { count, .. } => *count as usize,
            Self::Table(sizes) => sizes.len(),
        }
    }
    pub fn size(&self, index: usize) -> u32 {
        match self {
            Self::Constant { size, .. } => *size,
            Self::Table(sizes) => sizes[index],
        }
    }
}

impl Track {
    pub fn sample_count(&self) -> u64 {
        self.time_to_sample.iter().map(|&(n, _)| n as u64).sum()
    }

    /// Byte ranges of each chunk in file order: (offset, length, samples).
    pub fn chunks(&self) -> Option<Vec<(u64, u64, u32)>> {
        let mut out = Vec::with_capacity(self.chunk_offsets.len());
        let mut sample = 0usize;
        for (index, &offset) in self.chunk_offsets.iter().enumerate() {
            let number = index as u32 + 1;
            let run = self
                .chunk_runs
                .iter()
                .rev()
                .find(|run| run.first_chunk <= number)?;
            let mut length = 0u64;
            for _ in 0..run.samples_per_chunk {
                if sample >= self.sample_sizes.count() {
                    return None;
                }
                length += self.sample_sizes.size(sample) as u64;
                sample += 1;
            }
            out.push((offset, length, run.samples_per_chunk));
        }
        (sample == self.sample_sizes.count()).then_some(out)
    }

    /// Nominal rate as FFmpeg derives it: samples per summed decode time.
    fn average_rate(&self) -> Option<f64> {
        let total: u64 = self
            .time_to_sample
            .iter()
            .map(|&(n, d)| n as u64 * d as u64)
            .sum();
        (total > 0 && self.timescale > 0)
            .then(|| self.sample_count() as f64 * self.timescale as f64 / total as f64)
    }

    /// Presentation timing without decoding: decode deltas plus
    /// composition offsets, classified exactly like the FFmpeg packet walk.
    fn timing_mode(&self) -> align_core::VideoFrameRateMode {
        let scale = self.timescale as f64;
        let mut timing = PacketTiming::default();
        let mut offsets = self
            .composition_offsets
            .iter()
            .flat_map(|&(n, o)| std::iter::repeat_n(o, n as usize));
        let mut dts = 0i64;
        for &(count, delta) in &self.time_to_sample {
            for _ in 0..count {
                let pts = dts + offsets.next().unwrap_or(0) as i64;
                timing.observe(pts as f64 / scale, delta as f64 / scale);
                dts += delta as i64;
            }
        }
        timing.mode()
    }
}

#[derive(Clone, Debug)]
pub struct Movie {
    pub timescale: u32,
    pub duration: u64,
    pub tracks: Vec<Track>,
    /// First timecode sample (big-endian frame counter), when present.
    pub timecode_frame: Option<u32>,
}

impl Movie {
    pub fn duration_seconds(&self) -> f64 {
        if self.timescale == 0 {
            return 0.0;
        }
        let movie = self.duration as f64 / self.timescale as f64;
        // Some writers leave mvhd empty; fall back to the longest track.
        let longest = self
            .tracks
            .iter()
            .filter(|t| t.timescale > 0)
            .map(|t| t.media_duration as f64 / t.timescale as f64)
            .fold(0.0, f64::max);
        if movie > 0.0 { movie } else { longest }
    }

    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    pub fn audio(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.kind == TrackKind::Audio)
    }

    pub fn source_timecode(&self) -> Option<SourceTimecode> {
        let entry = self
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Timecode)?
            .timecode
            .as_ref()?;
        if entry.timescale == 0 || entry.frame_duration == 0 {
            return None;
        }
        let duration = MediaTime::new(entry.frame_duration as i64, entry.timescale as i32);
        SourceTimecode::from_frame_number(self.timecode_frame? as i64, duration, entry.drop_frame)
    }

    pub fn probe(&self) -> Option<ProbeReport> {
        let audio_streams: Vec<AudioStreamProbe> = self
            .audio()
            .filter_map(|t| t.audio.as_ref())
            .map(|a| AudioStreamProbe {
                sample_rate: a.sample_rate,
                channels: a.channels,
                bit_depth: a.bit_depth,
                is_float: a.is_float,
            })
            .collect();
        let video = self.video().map(|track| VideoProbe {
            width: track.width,
            height: track.height,
            frame_duration: track
                .average_rate()
                .and_then(|fps| canonical_frame_duration(fps, None)),
            mode: track.timing_mode(),
            source_timecode: self.source_timecode(),
        });
        let duration_seconds = self.duration_seconds();
        if (audio_streams.is_empty() && video.is_none()) || duration_seconds <= 0.0 {
            return None;
        }
        Some(ProbeReport {
            duration_seconds,
            audio_streams,
            has_video: video.is_some(),
            video,
        })
    }
}

// ------------------------------------------------------------ file access

/// Locate and load the top-level `moov` box without reading media data.
pub(crate) fn read_moov(file: &mut File) -> Option<Vec<u8>> {
    let length = file.metadata().ok()?.len();
    let mut offset = 0u64;
    while offset + 8 <= length {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let (size, header_len) = match u32::from_be_bytes(header[..4].try_into().ok()?) {
            0 => (length - offset, 8),
            1 => {
                file.read_exact(&mut header[8..]).ok()?;
                (u64::from_be_bytes(header[8..].try_into().ok()?), 16)
            }
            n => (n as u64, 8),
        };
        if size < header_len || offset.checked_add(size)? > length {
            return None;
        }
        if &kind == b"moov" {
            if size > MAX_MOOV_BYTES {
                return None;
            }
            let mut moov = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset)).ok()?;
            file.read_exact(&mut moov).ok()?;
            return Some(moov);
        }
        offset += size;
    }
    None
}

pub fn read(path: &Path) -> Option<Movie> {
    let mut file = File::open(path).ok()?;
    let moov = read_moov(&mut file)?;
    let body = atoms(&moov).next()?.body;
    let mut movie = parse_moov(body)?;
    if let Some(track) = movie.tracks.iter().find(|t| t.kind == TrackKind::Timecode) {
        let (offset, length, _) = *track.chunks()?.first()?;
        if length >= 4 {
            let mut word = [0u8; 4];
            file.seek(SeekFrom::Start(offset)).ok()?;
            file.read_exact(&mut word).ok()?;
            movie.timecode_frame = Some(u32::from_be_bytes(word));
        }
    }
    Some(movie)
}

/// Header probe for the portable backend; `None` means "ask FFmpeg".
pub fn inspect(path: &Path) -> Option<ProbeReport> {
    if !is_candidate(path) {
        return None;
    }
    read(path)?.probe()
}

/// Audio formats only: skips the video timing classification that decode
/// calls never need.
pub fn inspect_audio(path: &Path) -> Option<ProbeReport> {
    if !is_candidate(path) {
        return None;
    }
    let mut movie = read(path)?;
    let has_video = movie.video().is_some();
    movie.tracks.retain(|t| t.kind == TrackKind::Audio);
    let mut report = movie.probe()?;
    report.has_video = has_video;
    Some(report)
}

// ------------------------------------------------------------ AAF linking

/// Stream description in the `ffprobe -show_format -show_streams` shape the
/// AAF module's AMA linker reads (codec, rates, lengths, pixel layout). The
/// picture rate is the canonical broadcast rate when one applies, matching
/// the edit rate the manifest declares.
pub fn ama_metadata(path: &Path) -> Option<serde_json::Value> {
    use serde_json::json;
    if !is_candidate(path) {
        return None;
    }
    let movie = read(path)?;
    let mut streams = Vec::new();
    for track in &movie.tracks {
        match track.kind {
            TrackKind::Video => {
                let rate = track
                    .average_rate()
                    .and_then(|fps| canonical_frame_duration(fps, None))
                    .map(|d| format!("{}/{}", d.timescale, d.value))
                    .or_else(|| track.average_rate_fraction())?;
                let picture = picture_format(track);
                let mut stream = json!({
                    "codec_type": "video",
                    "codec_name": picture.codec_name,
                    "width": track.width,
                    "height": track.height,
                    "avg_frame_rate": rate,
                    "nb_frames": track.sample_count().to_string(),
                    "pix_fmt": picture.pix_fmt,
                });
                if let Some(profile) = picture.profile {
                    stream["profile"] = json!(profile);
                }
                streams.push(stream);
            }
            TrackKind::Audio => {
                let audio = track.audio.as_ref()?;
                let (codec_name, sample_fmt) = sound_format(&track.codec, audio);
                let seconds = match track.edited_duration {
                    Some(edited) if movie.timescale > 0 => edited as f64 / movie.timescale as f64,
                    _ => track.media_duration as f64 / track.timescale.max(1) as f64,
                };
                let bit_rate = match audio.bit_depth {
                    Some(bits) => audio.sample_rate as u64 * audio.channels as u64 * bits as u64,
                    None if seconds > 0.0 => {
                        let bytes: u64 = (0..track.sample_sizes.count())
                            .map(|i| track.sample_sizes.size(i) as u64)
                            .sum();
                        (bytes as f64 * 8.0 / seconds).round() as u64
                    }
                    None => 0,
                };
                // Stream duration in the audio sample clock.
                let duration_ts = (seconds * audio.sample_rate).round() as u64;
                streams.push(json!({
                    "codec_type": "audio",
                    "codec_name": codec_name,
                    "sample_rate": format!("{}", audio.sample_rate.round() as u64),
                    "channels": audio.channels,
                    "sample_fmt": sample_fmt,
                    "bit_rate": bit_rate.to_string(),
                    "duration_ts": duration_ts,
                }));
            }
            _ => {}
        }
    }
    Some(json!({
        "format": {
            "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
            "format_long_name": "QuickTime / MOV",
            "duration": format!("{:.6}", movie.duration_seconds()),
        },
        "streams": streams,
    }))
}

impl Track {
    fn average_rate_fraction(&self) -> Option<String> {
        let total: u64 = self
            .time_to_sample
            .iter()
            .map(|&(n, d)| n as u64 * d as u64)
            .sum();
        let frames = self.sample_count() * self.timescale as u64;
        let divisor = gcd(frames, total);
        (divisor > 0).then(|| format!("{}/{}", frames / divisor, total / divisor))
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

struct PictureFormat {
    codec_name: &'static str,
    pix_fmt: String,
    profile: Option<&'static str>,
}

fn picture_format(track: &Track) -> PictureFormat {
    // Visual sample entry: 8-byte header + 78 bytes of fixed fields.
    let children = track.sample_entry.get(86..).unwrap_or_default();
    let pix = |chroma: u32, depth: u32| {
        let layout = match chroma {
            0 => "gray",
            2 => "yuv422p",
            3 => "yuv444p",
            _ => "yuv420p",
        };
        if depth > 8 {
            format!("{layout}{depth}le")
        } else {
            layout.to_string()
        }
    };
    let (codec_name, pix_fmt, profile) = match &track.codec {
        b"avc1" | b"avc3" => {
            let avc = child(children, b"avcC").and_then(|a| avc_format(a.body));
            let (chroma, depth, profile) = avc.unwrap_or((1, 8, None));
            ("h264", pix(chroma, depth), profile)
        }
        b"hvc1" | b"hev1" => {
            let hvc = child(children, b"hvcC").and_then(|a| {
                let b = a.body;
                Some((u32::from(*b.get(16)? & 3), u32::from(*b.get(17)? & 7) + 8))
            });
            let (chroma, depth) = hvc.unwrap_or((1, 8));
            ("hevc", pix(chroma, depth), None)
        }
        b"apch" | b"apcn" | b"apcs" | b"apco" => ("prores", pix(2, 10), None),
        b"ap4h" | b"ap4x" => ("prores", pix(3, 12), None),
        b"AVdn" | b"AVdh" => ("dnxhd", pix(2, 8), None),
        b"mp4v" => ("mpeg4", pix(1, 8), None),
        b"jpeg" | b"mjpa" | b"mjpb" => ("mjpeg", "yuvj422p".to_string(), None),
        b"xd5a" | b"xd5b" | b"xd5c" | b"xd5d" | b"xd5e" | b"xd5f" | b"xd59" | b"xdvc" => {
            ("mpeg2video", pix(2, 8), None)
        }
        b"av01" => ("av1", pix(1, 8), None),
        b"vp09" => ("vp9", pix(1, 8), None),
        _ => ("unknown", pix(1, 8), None),
    };
    PictureFormat {
        codec_name,
        pix_fmt,
        profile,
    }
}

/// Chroma format, luma depth and FFmpeg profile name from an `avcC` box.
fn avc_format(avcc: &[u8]) -> Option<(u32, u32, Option<&'static str>)> {
    let profile_idc = *avcc.get(1)?;
    let constraints = *avcc.get(2)?;
    let mut chroma = 1;
    let mut depth = 8;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        // High profiles code chroma and depth in the SPS.
        let sps_count = (*avcc.get(5)? & 0x1f) as usize;
        if sps_count > 0 {
            let length = u16::from_be_bytes([*avcc.get(6)?, *avcc.get(7)?]) as usize;
            let sps = avcc.get(8..8 + length)?;
            if let Some((c, d)) = sps_chroma_depth(sps) {
                (chroma, depth) = (c, d);
            }
        }
    }
    let intra = constraints & 0x10 != 0;
    let profile = match profile_idc {
        66 if constraints & 0x40 != 0 => "Constrained Baseline",
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 if intra => "High 10 Intra",
        110 => "High 10",
        122 if intra => "High 4:2:2 Intra",
        122 => "High 4:2:2",
        244 if intra => "High 4:4:4 Intra",
        244 => "High 4:4:4 Predictive",
        44 => "CAVLC 4:4:4",
        _ => return Some((chroma, depth, None)),
    };
    Some((chroma, depth, Some(profile)))
}

/// `chroma_format_idc` and luma bit depth from a High-profile SPS NAL.
fn sps_chroma_depth(nal: &[u8]) -> Option<(u32, u32)> {
    // Drop emulation-prevention bytes (00 00 03 → 00 00).
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal.get(1..)? {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        rbsp.push(byte);
    }
    let mut bits = BitReader {
        data: &rbsp,
        pos: 24,
    };
    bits.golomb()?; // seq_parameter_set_id
    let chroma = bits.golomb()?;
    if chroma == 3 {
        bits.bit()?; // separate_colour_plane_flag
    }
    let depth = bits.golomb()? + 8;
    Some((chroma, depth))
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }
    fn golomb(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 1u64;
        for _ in 0..zeros {
            value = value << 1 | self.bit()? as u64;
        }
        u32::try_from(value - 1).ok()
    }
}

/// FFmpeg codec and decoded sample format names for an audio track.
fn sound_format(codec: &[u8; 4], audio: &AudioEntry) -> (String, &'static str) {
    let pcm_format = |bits: u32, float: bool| match (float, bits) {
        (true, 64) => "dbl",
        (true, _) => "flt",
        (false, 8) => "u8",
        (false, 16) => "s16",
        _ => "s32",
    };
    match (audio.bit_depth, audio.is_float) {
        (Some(bits), Some(float)) => {
            let endian = match (bits, audio.big_endian) {
                (8, _) => "",
                (_, true) => "be",
                (_, false) => "le",
            };
            let kind = if float {
                "f"
            } else if bits == 8 {
                "u"
            } else {
                "s"
            };
            (format!("pcm_{kind}{bits}{endian}"), pcm_format(bits, float))
        }
        _ => match codec {
            b"alac" => ("alac".into(), "s32p"),
            b"ac-3" => ("ac3".into(), "fltp"),
            b"ec-3" => ("eac3".into(), "fltp"),
            b"Opus" => ("opus".into(), "fltp"),
            _ => ("aac".into(), "fltp"),
        },
    }
}

// ------------------------------------------------------------ parsing

pub(crate) fn parse_moov(moov: &[u8]) -> Option<Movie> {
    // Fragmented and compressed headers keep their samples elsewhere.
    if child(moov, b"mvex").is_some() || child(moov, b"cmov").is_some() {
        return None;
    }
    let (version, mut mvhd) = full_box(child(moov, b"mvhd")?.body)?;
    let (timescale, duration) = if version == 1 {
        mvhd.skip(16)?;
        (mvhd.u32()?, mvhd.u64()?)
    } else {
        mvhd.skip(8)?;
        (mvhd.u32()?, mvhd.u32()? as u64)
    };
    let tracks = atoms(moov)
        .filter(|a| &a.kind == b"trak")
        .map(|trak| parse_trak(trak.body))
        .collect::<Option<Vec<_>>>()?;
    Some(Movie {
        timescale,
        duration,
        tracks,
        timecode_frame: None,
    })
}

fn parse_trak(trak: &[u8]) -> Option<Track> {
    let (version, mut tkhd) = full_box(child(trak, b"tkhd")?.body)?;
    tkhd.skip(if version == 1 { 16 } else { 8 })?;
    let id = tkhd.u32()?;
    // reserved, duration, reserved[2], layer, group, volume, reserved, matrix
    tkhd.skip(4 + if version == 1 { 8 } else { 4 } + 8 + 2 + 2 + 2 + 2 + 36)?;
    let width = tkhd.u32()? >> 16;
    let height = tkhd.u32()? >> 16;

    let mdia = child(trak, b"mdia")?.body;
    let (version, mut mdhd) = full_box(child(mdia, b"mdhd")?.body)?;
    let (timescale, media_duration) = if version == 1 {
        mdhd.skip(16)?;
        (mdhd.u32()?, mdhd.u64()?)
    } else {
        mdhd.skip(8)?;
        (mdhd.u32()?, mdhd.u32()? as u64)
    };
    let (_, mut hdlr) = full_box(child(mdia, b"hdlr")?.body)?;
    hdlr.skip(4)?;
    let kind = match &hdlr.fourcc()? {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"tmcd" => TrackKind::Timecode,
        _ => TrackKind::Other,
    };
    let stbl = path(mdia, &[b"minf", b"stbl"])?.body;
    let entry = {
        let (_, mut stsd) = full_box(child(stbl, b"stsd")?.body)?;
        let count = stsd.u32()?;
        (count > 0).then_some(())?;
        let rest = &stsd.data[stsd.pos..];
        atoms(rest).next()?
    };
    let audio = if kind == TrackKind::Audio {
        Some(parse_audio_entry(entry)?)
    } else {
        None
    };
    let timecode = if kind == TrackKind::Timecode && &entry.kind == b"tmcd" {
        let mut b = Bytes::new(entry.body);
        b.skip(8 + 4)?; // sample entry header, reserved
        let flags = b.u32()?;
        let timescale = b.u32()?;
        let frame_duration = b.u32()?;
        Some(TimecodeEntry {
            drop_frame: flags & 1 != 0,
            timescale,
            frame_duration,
        })
    } else {
        None
    };

    let (_, mut stts) = full_box(child(stbl, b"stts")?.body)?;
    let entries = stts.u32()? as usize;
    (entries * 8 <= stts.remaining()).then_some(())?;
    let time_to_sample = (0..entries)
        .map(|_| Some((stts.u32()?, stts.u32()?)))
        .collect::<Option<Vec<_>>>()?;

    let composition_offsets = match child(stbl, b"ctts") {
        Some(ctts) => {
            let (_, mut b) = full_box(ctts.body)?;
            let entries = b.u32()? as usize;
            (entries * 8 <= b.remaining()).then_some(())?;
            (0..entries)
                .map(|_| Some((b.u32()?, b.u32()? as i32)))
                .collect::<Option<Vec<_>>>()?
        }
        None => Vec::new(),
    };

    let (_, mut stsz) = full_box(child(stbl, b"stsz")?.body)?;
    let size = stsz.u32()?;
    let count = stsz.u32()?;
    let sample_sizes = if size != 0 {
        SampleSizes::Constant { size, count }
    } else {
        (count as usize * 4 <= stsz.remaining()).then_some(())?;
        SampleSizes::Table((0..count).map(|_| stsz.u32()).collect::<Option<Vec<_>>>()?)
    };

    let (_, mut stsc) = full_box(child(stbl, b"stsc")?.body)?;
    let entries = stsc.u32()? as usize;
    (entries * 12 <= stsc.remaining()).then_some(())?;
    let chunk_runs = (0..entries)
        .map(|_| {
            let first_chunk = stsc.u32()?;
            let samples_per_chunk = stsc.u32()?;
            stsc.skip(4)?;
            Some(ChunkRun {
                first_chunk,
                samples_per_chunk,
            })
        })
        .collect::<Option<Vec<_>>>()?;

    let chunk_offsets = if let Some(stco) = child(stbl, b"stco") {
        let (_, mut b) = full_box(stco.body)?;
        let entries = b.u32()? as usize;
        (entries * 4 <= b.remaining()).then_some(())?;
        (0..entries)
            .map(|_| b.u32().map(u64::from))
            .collect::<Option<Vec<_>>>()?
    } else {
        let (_, mut b) = full_box(child(stbl, b"co64")?.body)?;
        let entries = b.u32()? as usize;
        (entries * 8 <= b.remaining()).then_some(())?;
        (0..entries).map(|_| b.u64()).collect::<Option<Vec<_>>>()?
    };

    let edits = path(trak, &[b"edts", b"elst"]).and_then(|elst| {
        let (version, mut b) = full_box(elst.body)?;
        let entries = b.u32()?;
        let mut total = 0u64;
        let mut start = None;
        for _ in 0..entries {
            let (duration, media_time) = if version == 1 {
                (b.u64()?, b.u64()? as i64)
            } else {
                (b.u32()? as u64, b.u32()? as i32 as i64)
            };
            b.skip(4)?; // media rate
            if media_time >= 0 {
                total += duration;
                start.get_or_insert(media_time as u64);
            }
        }
        Some((total, start.unwrap_or(0)))
    });
    let edited_duration = edits.map(|(total, _)| total);
    let media_start = edits.map_or(0, |(_, start)| start);

    Some(Track {
        id,
        kind,
        edited_duration,
        media_start,
        codec: entry.kind,
        sample_entry: entry.raw.to_vec(),
        timescale,
        media_duration,
        width,
        height,
        audio,
        timecode,
        time_to_sample,
        composition_offsets,
        sample_sizes,
        chunk_runs,
        chunk_offsets,
    })
}

/// QuickTime sound description versions 0–2 and ISO audio sample entries.
fn parse_audio_entry(entry: Atom<'_>) -> Option<AudioEntry> {
    let mut b = Bytes::new(entry.body);
    b.skip(6 + 2)?; // reserved, data reference index
    let version = b.u16()?;
    b.skip(2 + 4)?; // revision, vendor
    let codec = &entry.kind;
    if version == 2 {
        b.skip(2 + 2 + 2 + 2 + 4 + 4)?;
        let sample_rate = f64::from_bits(b.u64()?);
        let channels = b.u32()? as usize;
        b.skip(4)?;
        let bits = b.u32()?;
        let flags = b.u32()?;
        let lpcm = codec == b"lpcm";
        return valid_audio(AudioEntry {
            sample_rate,
            channels,
            bit_depth: lpcm.then_some(bits),
            is_float: lpcm.then_some(flags & 1 != 0),
            big_endian: flags & 2 != 0,
        });
    }
    let channels = b.u16()? as usize;
    let sample_size = b.u16()? as u32;
    b.skip(2 + 2)?; // compression id, packet size
    let sample_rate = b.u32()? as f64 / 65536.0;
    let (bit_depth, is_float) = match codec {
        b"sowt" | b"twos" => (Some(sample_size.max(8)), Some(false)),
        b"raw " => (Some(8), Some(false)),
        b"in24" => (Some(24), Some(false)),
        b"in32" => (Some(32), Some(false)),
        b"fl32" => (Some(32), Some(true)),
        b"fl64" => (Some(64), Some(true)),
        _ => (None, None),
    };
    // QuickTime `enda` (inside `wave` or directly) overrides the default
    // big-endian order of the `in24`/`in32`/`fl32`/`fl64` codes.
    let extensions = entry
        .body
        .get(if version == 1 { 44 } else { 28 }..)
        .unwrap_or_default();
    let enda = child(extensions, b"enda")
        .or_else(|| child(child(extensions, b"wave")?.body, b"enda"))
        .and_then(|a| a.body.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]])));
    let big_endian = match codec {
        b"twos" => true,
        b"in24" | b"in32" | b"fl32" | b"fl64" => enda != Some(1),
        _ => false,
    };
    valid_audio(AudioEntry {
        sample_rate,
        channels,
        bit_depth,
        is_float,
        big_endian,
    })
}

fn valid_audio(entry: AudioEntry) -> Option<AudioEntry> {
    (entry.sample_rate.is_finite() && entry.sample_rate > 0.0 && entry.channels > 0)
        .then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use align_core::VideoFrameRateMode;

    fn ffmpeg(args: &[&str], output: &Path) -> bool {
        let Some(bin) = crate::ff::ffmpeg_bin() else {
            return false;
        };
        std::process::Command::new(bin)
            .args(["-y", "-v", "error"])
            .args(args)
            .arg(output)
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The native probe must agree with the FFmpeg tier it replaces.
    fn assert_matches_ffprobe(path: &Path) {
        let native = inspect(path).expect("native probe");
        let reference = crate::ff::inspect_full(path).expect("ffprobe");
        assert!(
            (native.duration_seconds - reference.duration_seconds).abs() < 0.05,
            "{native:?} vs {reference:?}"
        );
        assert_eq!(native.has_video, reference.has_video);
        assert_eq!(native.audio_streams.len(), reference.audio_streams.len());
        for (a, b) in native.audio_streams.iter().zip(&reference.audio_streams) {
            assert_eq!(a.sample_rate, b.sample_rate);
            assert_eq!(a.channels, b.channels);
            if a.bit_depth.is_some() {
                assert_eq!(a.bit_depth, b.bit_depth, "{path:?}");
                assert_eq!(a.is_float, b.is_float, "{path:?}");
            }
        }
        match (&native.video, &reference.video) {
            (Some(a), Some(b)) => {
                assert_eq!((a.width, a.height), (b.width, b.height));
                assert_eq!(a.frame_duration, b.frame_duration);
                assert_eq!(a.mode, b.mode);
                assert_eq!(a.source_timecode, b.source_timecode, "{path:?}");
            }
            (None, None) => {}
            other => panic!("video mismatch: {other:?}"),
        }
    }

    #[test]
    fn native_probe_matches_ffprobe_across_camera_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let video = "testsrc2=s=192x108:r=30000/1001:d=3";
        let cases: &[(&str, &[&str])] = &[
            (
                "pcm16.mov",
                &[
                    "-c:v",
                    "libx264",
                    "-bf",
                    "2",
                    "-c:a",
                    "pcm_s16le",
                    "-timecode",
                    "01:00:00;00",
                ],
            ),
            (
                "pcm24.mov",
                &[
                    "-c:v",
                    "mpeg4",
                    "-c:a",
                    "pcm_s24le",
                    "-timecode",
                    "10:20:30:04",
                ],
            ),
            ("float.mov", &["-c:v", "mpeg4", "-c:a", "pcm_f32le"]),
            (
                "aac.mp4",
                &["-c:v", "libx264", "-c:a", "aac", "-timecode", "00:59:59:00"],
            ),
            ("big-endian.mov", &["-c:v", "mpeg4", "-c:a", "pcm_s16be"]),
        ];
        let mut checked = 0;
        for (name, codec) in cases {
            let output = dir.path().join(name);
            let mut args = vec![
                "-f",
                "lavfi",
                "-i",
                video,
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=3",
                "-ac",
                "2",
                "-shortest",
            ];
            args.extend_from_slice(codec);
            if !ffmpeg(&args, &output) {
                eprintln!("SKIP {name}: ffmpeg cannot build fixture");
                continue;
            }
            assert_matches_ffprobe(&output);
            checked += 1;
        }
        if checked > 0 {
            let tc = inspect(&dir.path().join("pcm16.mov"))
                .and_then(|r| r.video?.source_timecode)
                .expect("drop-frame timecode");
            assert_eq!(tc.text, "01:00:00;00");
            assert!(tc.drop_frame);
        }
    }

    #[test]
    fn ama_metadata_agrees_with_ffprobe_fields() {
        let Some(ffprobe) = crate::ff::ffprobe_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(&str, &[&str])] = &[
            (
                "high.mov",
                &[
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "pcm_s24le",
                ],
            ),
            (
                "high422.mp4",
                &["-c:v", "libx264", "-pix_fmt", "yuv422p10le", "-c:a", "aac"],
            ),
            (
                "baseline.mov",
                &[
                    "-c:v",
                    "libx264",
                    "-profile:v",
                    "baseline",
                    "-c:a",
                    "pcm_s16le",
                ],
            ),
            (
                "hevc.mp4",
                &["-c:v", "libx265", "-pix_fmt", "yuv420p10le", "-c:a", "aac"],
            ),
            ("prores.mov", &["-c:v", "prores_ks", "-c:a", "pcm_f32le"]),
        ];
        for (name, codec) in cases {
            let output = dir.path().join(name);
            let mut args = vec![
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=128x72:r=25:d=1",
                "-f",
                "lavfi",
                "-i",
                "sine=sample_rate=48000:duration=1",
                "-shortest",
            ];
            args.extend_from_slice(codec);
            if !ffmpeg(&args, &output) {
                eprintln!("SKIP {name}: encoder unavailable");
                continue;
            }
            let native = ama_metadata(&output).expect("native metadata");
            let probe = std::process::Command::new(&ffprobe)
                .args([
                    "-v",
                    "error",
                    "-show_format",
                    "-show_streams",
                    "-of",
                    "json",
                ])
                .arg(&output)
                .output()
                .unwrap();
            let reference: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            assert_eq!(
                native["format"]["format_long_name"],
                reference["format"]["format_long_name"]
            );
            let streams = |v: &serde_json::Value| v["streams"].as_array().unwrap().clone();
            let (native, reference) = (streams(&native), streams(&reference));
            assert_eq!(native.len(), reference.len(), "{name}");
            for (a, b) in native.iter().zip(&reference) {
                let text = |v: &serde_json::Value| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut keys = vec!["codec_type", "codec_name"];
                if a["codec_type"] == "video" {
                    keys.extend([
                        "width",
                        "height",
                        "avg_frame_rate",
                        "nb_frames",
                        "pix_fmt",
                        "profile",
                    ]);
                } else {
                    keys.extend(["sample_rate", "channels", "sample_fmt", "duration_ts"]);
                }
                for key in keys {
                    if a.get(key).is_none() && key == "profile" && b["codec_name"] != "h264" {
                        continue;
                    }
                    assert_eq!(text(&a[key]), text(&b[key]), "{name} {key}");
                }
            }
        }
    }

    #[test]
    fn variable_frame_timing_is_classified_from_sample_tables() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("vfr.mp4");
        let ok = ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=64x36:r=30:d=2",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=mono",
                "-vf",
                "setpts='if(lt(N,20),N/30/TB,(N+N/3)/30/TB)'",
                "-fps_mode",
                "vfr",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-shortest",
            ],
            &output,
        );
        if !ok {
            eprintln!("SKIP: ffmpeg cannot build VFR fixture");
            return;
        }
        let native = inspect(&output).expect("native");
        assert_eq!(native.video.unwrap().mode, VideoFrameRateMode::Variable);
        assert_matches_ffprobe(&output);
    }

    #[test]
    fn video_only_and_non_quicktime_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("silent.mov");
        if ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=64x36:r=25:d=1",
                "-c:v",
                "mpeg4",
            ],
            &output,
        ) {
            let report = inspect(&output).expect("video-only probe");
            assert!(report.has_video && report.audio_streams.is_empty());
        }
        let junk = dir.path().join("junk.mov");
        std::fs::write(&junk, b"\0\0\0\x10ftypqt  \0\0\0\0garbage").unwrap();
        assert!(inspect(&junk).is_none());
        assert!(inspect(Path::new("clip.mxf")).is_none());
    }

    #[test]
    fn truncated_boxes_never_panic() {
        let mut moov = Vec::new();
        for size in [0u32, 1, 7, 8, 9, 0xffff_ffff] {
            moov.extend_from_slice(&size.to_be_bytes());
            moov.extend_from_slice(b"trak");
        }
        for cut in 0..moov.len() {
            let _ = parse_moov(&moov[..cut]);
            let _ = atoms(&moov[..cut]).count();
        }
    }
}
