//! QuickTime stream copy with replacement audio, without FFmpeg.
//!
//! The camera's first video track (and its timecode track) is copied sample
//! for sample: the original sample descriptions and timing tables are kept
//! and only chunk offsets are rewritten. The replacement audio becomes one
//! 48 kHz, 32-bit float linear PCM track (`lpcm`, sound description v2).
//! Media data is streamed chunk by chunk; nothing is held in memory beyond
//! one chunk and the movie header.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::backend::MediaBackend;
use crate::isobmff::{self, Atom, TrackKind, atoms, child};
use crate::render::RenderError;

/// Output audio clock, the NLE-native rate.
pub const OUTPUT_RATE: u32 = 48_000;
/// Audio frames per written chunk (one second).
const AUDIO_CHUNK_FRAMES: usize = OUTPUT_RATE as usize;

#[derive(Debug)]
pub enum RemuxError {
    /// The source is not a QuickTime/MP4 file this writer can copy; the
    /// caller may use another muxer.
    Unsupported(&'static str),
    Render(RenderError),
    Io(std::io::Error),
}

impl std::fmt::Display for RemuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "unsupported camera file: {reason}"),
            Self::Render(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl From<std::io::Error> for RemuxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<RenderError> for RemuxError {
    fn from(error: RenderError) -> Self {
        Self::Render(error)
    }
}

/// Replacement audio: `duration` seconds of `source` from `source_in`,
/// delayed by `leading_silence`, padded or cut to `total` seconds.
pub struct ReplacementAudio<'a> {
    pub backend: &'a dyn MediaBackend,
    pub source: &'a Path,
    pub source_in: f64,
    pub duration: f64,
    pub leading_silence: f64,
    pub total: f64,
}

/// Write `output` as `video`'s picture (and timecode) plus `audio`.
pub fn replace_audio(
    video: &Path,
    output: &Path,
    audio: &ReplacementAudio<'_>,
    cancel: &AtomicBool,
) -> Result<(), RemuxError> {
    if !isobmff::is_candidate(video) {
        return Err(RemuxError::Unsupported("not a QuickTime or MP4 file"));
    }
    let mut input = File::open(video)?;
    let moov = isobmff::read_moov(&mut input).ok_or(RemuxError::Unsupported("no movie header"))?;
    let moov_body = atoms(&moov)
        .next()
        .ok_or(RemuxError::Unsupported("empty movie header"))?
        .body;
    let movie =
        isobmff::parse_moov(moov_body).ok_or(RemuxError::Unsupported("unreadable tracks"))?;
    let traks: Vec<Atom<'_>> = atoms(moov_body).filter(|a| &a.kind == b"trak").collect();
    let pick = |kind: TrackKind| {
        movie
            .tracks
            .iter()
            .position(|t| t.kind == kind)
            .map(|i| (&movie.tracks[i], traks[i]))
    };
    let (video_track, video_trak) =
        pick(TrackKind::Video).ok_or(RemuxError::Unsupported("no video track"))?;
    let timecode = pick(TrackKind::Timecode);
    for (_, trak) in std::iter::once((video_track, video_trak)).chain(timecode) {
        if !self_contained(trak.body) {
            return Err(RemuxError::Unsupported("media stored in another file"));
        }
    }

    let mut out = BufWriter::with_capacity(1 << 20, File::create(output)?);
    // ftyp: QuickTime brand (linear PCM is a QuickTime sample description).
    out.write_all(&boxed(
        b"ftyp",
        &[b"qt  ".as_slice(), &0x0200_0000u32.to_be_bytes(), b"qt  "].concat(),
    ))?;
    let mdat_start = out.stream_position()?;
    out.write_all(&1u32.to_be_bytes())?;
    out.write_all(b"mdat")?;
    out.write_all(&0u64.to_be_bytes())?; // 64-bit size, patched below

    let mut copied = Vec::new();
    for (track, trak) in std::iter::once((video_track, video_trak)).chain(timecode) {
        let chunks = track
            .chunks()
            .ok_or(RemuxError::Unsupported("inconsistent sample tables"))?;
        let mut offsets = Vec::with_capacity(chunks.len());
        let mut buffer = Vec::new();
        for (offset, length, _) in chunks {
            crate::render::check_cancel(cancel)?;
            offsets.push(out.stream_position()?);
            buffer.resize(length as usize, 0);
            input.seek(SeekFrom::Start(offset))?;
            input.read_exact(&mut buffer)?;
            out.write_all(&buffer)?;
        }
        copied.push((trak, offsets));
    }

    let total_frames = (audio.total.max(0.0) * OUTPUT_RATE as f64).round() as u64;
    let delay_frames =
        ((audio.leading_silence.max(0.0) * OUTPUT_RATE as f64).round() as u64).min(total_frames);
    let channels = audio
        .backend
        .inspect(audio.source)
        .map_err(RenderError::from)?
        .audio_streams
        .first()
        .map(|stream| stream.channels)
        .filter(|&channels| channels > 0)
        .ok_or_else(|| RenderError::NoAudio(align_core::model::file_name(audio.source)))?;
    let mut sink = AudioSink::new(&mut out, channels);
    sink.silence(delay_frames)?;
    crate::render::stream_at_rate(
        audio.backend,
        audio.source,
        audio.source_in,
        audio
            .duration
            .max(0.0)
            .min((total_frames - delay_frames) as f64 / OUTPUT_RATE as f64),
        OUTPUT_RATE as f64,
        cancel,
        &mut |planes| sink.planar(&planes).map_err(|_| RenderError::CannotWrite),
    )?;
    let written = sink.frames;
    sink.silence(total_frames.saturating_sub(written))?;
    let (audio_chunks, audio_frames) = sink.finish()?;

    let mdat_end = out.stream_position()?;
    out.seek(SeekFrom::Start(mdat_start + 8))?;
    out.write_all(&(mdat_end - mdat_start).to_be_bytes())?;
    out.seek(SeekFrom::Start(mdat_end))?;

    // Movie header: source mvhd with the new duration and next track id.
    let movie_scale = movie.timescale.max(1);
    let video_seconds = video_track.media_duration as f64 / video_track.timescale.max(1) as f64;
    let seconds = video_seconds.max(audio_frames as f64 / OUTPUT_RATE as f64);
    let audio_id = movie.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    let mut moov_out = Vec::new();
    let mvhd = child(moov_body, b"mvhd").ok_or(RemuxError::Unsupported("no mvhd"))?;
    moov_out.extend(patch_mvhd(
        mvhd.raw,
        (seconds * movie_scale as f64).ceil() as u64,
        audio_id + 1,
    )?);
    for (trak, offsets) in &copied {
        moov_out.extend(rewrite_offsets(trak.raw, offsets)?);
    }
    moov_out.extend(audio_trak(
        audio_id,
        channels,
        audio_frames,
        (audio_frames as f64 / OUTPUT_RATE as f64 * movie_scale as f64).round() as u64,
        &audio_chunks,
    ));
    // Global metadata (camera make, dates, reel names) stays with the file.
    for atom in atoms(moov_body).filter(|a| matches!(&a.kind, b"udta" | b"meta")) {
        moov_out.extend_from_slice(atom.raw);
    }
    out.write_all(&boxed(b"moov", &moov_out))?;
    out.flush()?;
    Ok(())
}

/// Every data reference of the track points into this file.
fn self_contained(trak: &[u8]) -> bool {
    let Some(dref) = [b"mdia", b"minf", b"dinf", b"dref"]
        .iter()
        .try_fold(trak, |data, kind| child(data, kind).map(|a| a.body))
    else {
        return true; // no references: implicitly this file
    };
    dref.get(8..)
        .map(|entries| atoms(entries).all(|entry| entry.body.get(3).is_some_and(|f| f & 1 == 1)))
        .unwrap_or(false)
}

/// Streams interleaved f32le frames into fixed one-second chunks.
struct AudioSink<'a, W: Write + Seek> {
    out: &'a mut W,
    chunks: Vec<(u64, u32)>,
    current: u32,
    frames: u64,
    channels: usize,
    bytes: Vec<u8>,
}

impl<'a, W: Write + Seek> AudioSink<'a, W> {
    fn new(out: &'a mut W, channels: usize) -> Self {
        Self {
            out,
            chunks: Vec::new(),
            current: 0,
            frames: 0,
            channels,
            bytes: Vec::with_capacity(AUDIO_CHUNK_FRAMES * channels * 4),
        }
    }

    fn frame(&mut self, samples: impl Iterator<Item = f32>) -> std::io::Result<()> {
        if self.current == 0 {
            self.chunks.push((self.out.stream_position()?, 0));
        }
        for sample in samples {
            self.bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.current += 1;
        self.frames += 1;
        if self.current as usize == AUDIO_CHUNK_FRAMES {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> std::io::Result<()> {
        self.out.write_all(&self.bytes)?;
        self.bytes.clear();
        if let Some(last) = self.chunks.last_mut() {
            last.1 = self.current;
        }
        self.current = 0;
        Ok(())
    }

    fn planar(&mut self, planes: &[Vec<f32>]) -> std::io::Result<()> {
        if planes.len() != self.channels {
            return Err(std::io::Error::other("channel layout changed"));
        }
        let frames = planes.first().map_or(0, Vec::len);
        for index in 0..frames {
            self.frame(planes.iter().map(|plane| plane[index]))?;
        }
        Ok(())
    }

    fn silence(&mut self, frames: u64) -> std::io::Result<()> {
        for _ in 0..frames {
            self.frame(std::iter::repeat_n(0.0, self.channels))?;
        }
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<(Vec<(u64, u32)>, u64)> {
        if self.current > 0 {
            self.flush_chunk()?;
        }
        Ok((self.chunks, self.frames))
    }
}

// ------------------------------------------------------------ boxes

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16);
    match u32::try_from(body.len() + 8) {
        Ok(size) => out.extend_from_slice(&size.to_be_bytes()),
        Err(_) => {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(&(body.len() as u64 + 16).to_be_bytes());
            out.extend_from_slice(body);
            return out;
        }
    }
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = ((version as u32) << 24 | flags).to_be_bytes().to_vec();
    payload.extend_from_slice(body);
    boxed(kind, &payload)
}

fn patch_mvhd(raw: &[u8], duration: u64, next_track: u32) -> Result<Vec<u8>, RemuxError> {
    let mut out = raw.to_vec();
    let header = if u32::from_be_bytes(raw[..4].try_into().unwrap_or_default()) == 1 {
        16
    } else {
        8
    };
    let version = *out
        .get(header)
        .ok_or(RemuxError::Unsupported("short mvhd"))?;
    let duration_at = header + 4 + if version == 1 { 20 } else { 12 };
    if version == 1 {
        out.get_mut(duration_at..duration_at + 8)
            .ok_or(RemuxError::Unsupported("short mvhd"))?
            .copy_from_slice(&duration.to_be_bytes());
    } else {
        out.get_mut(duration_at..duration_at + 4)
            .ok_or(RemuxError::Unsupported("short mvhd"))?
            .copy_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    }
    let len = out.len();
    out.get_mut(len - 4..)
        .ok_or(RemuxError::Unsupported("short mvhd"))?
        .copy_from_slice(&next_track.to_be_bytes());
    Ok(out)
}

/// Copy a `trak` box, replacing its chunk offset table with `offsets`.
fn rewrite_offsets(trak: &[u8], offsets: &[u64]) -> Result<Vec<u8>, RemuxError> {
    fn rebuild(atom: Atom<'_>, offsets: &[u64]) -> Vec<u8> {
        match &atom.kind {
            b"trak" | b"mdia" | b"minf" | b"stbl" => {
                let body: Vec<u8> = atoms(atom.body)
                    .flat_map(|child| rebuild(child, offsets))
                    .collect();
                boxed(&atom.kind, &body)
            }
            b"stco" | b"co64" => chunk_offsets(offsets),
            _ => atom.raw.to_vec(),
        }
    }
    let atom = atoms(trak)
        .next()
        .ok_or(RemuxError::Unsupported("empty track"))?;
    Ok(rebuild(atom, offsets))
}

fn chunk_offsets(offsets: &[u64]) -> Vec<u8> {
    let mut body = (offsets.len() as u32).to_be_bytes().to_vec();
    for offset in offsets {
        body.extend_from_slice(&offset.to_be_bytes());
    }
    full(b"co64", 0, 0, &body)
}

fn audio_trak(
    id: u32,
    channels: usize,
    frames: u64,
    movie_duration: u64,
    chunks: &[(u64, u32)],
) -> Vec<u8> {
    const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];
    let mut tkhd = Vec::new();
    tkhd.extend_from_slice(&[0u8; 8]); // creation, modification
    tkhd.extend_from_slice(&id.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 4]);
    tkhd.extend_from_slice(&(movie_duration.min(u32::MAX as u64) as u32).to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
    tkhd.extend_from_slice(&1u16.to_be_bytes()); // alternate group
    tkhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    tkhd.extend_from_slice(&[0u8; 2]);
    for value in MATRIX {
        tkhd.extend_from_slice(&value.to_be_bytes());
    }
    tkhd.extend_from_slice(&[0u8; 8]); // width, height
    let tkhd = full(b"tkhd", 0, 0x7, &tkhd);

    let mut mdhd = vec![0u8; 8];
    mdhd.extend_from_slice(&OUTPUT_RATE.to_be_bytes());
    mdhd.extend_from_slice(&(frames.min(u32::MAX as u64) as u32).to_be_bytes());
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // "und"
    mdhd.extend_from_slice(&[0u8; 2]);
    let mdhd = if frames > u32::MAX as u64 {
        let mut long = vec![0u8; 16];
        long.extend_from_slice(&OUTPUT_RATE.to_be_bytes());
        long.extend_from_slice(&frames.to_be_bytes());
        long.extend_from_slice(&0x55c4u16.to_be_bytes());
        long.extend_from_slice(&[0u8; 2]);
        full(b"mdhd", 1, 0, &long)
    } else {
        full(b"mdhd", 0, 0, &mdhd)
    };
    let hdlr = full(
        b"hdlr",
        0,
        0,
        &[b"mhlr".as_slice(), b"soun", &[0u8; 12], b"\x0cSoundHandler"].concat(),
    );

    let bytes_per_frame = channels as u32 * 4;
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]);
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    entry.extend_from_slice(&2u16.to_be_bytes()); // sound description v2
    entry.extend_from_slice(&[0u8; 6]); // revision, vendor
    entry.extend_from_slice(&3u16.to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes());
    entry.extend_from_slice(&0xfffeu16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    entry.extend_from_slice(&72u32.to_be_bytes()); // sizeOfStructOnly
    entry.extend_from_slice(&(OUTPUT_RATE as f64).to_bits().to_be_bytes());
    entry.extend_from_slice(&(channels as u32).to_be_bytes());
    entry.extend_from_slice(&0x7f00_0000u32.to_be_bytes());
    entry.extend_from_slice(&32u32.to_be_bytes()); // bits per channel
    entry.extend_from_slice(&(1u32 | 8).to_be_bytes()); // float | packed, little-endian
    entry.extend_from_slice(&bytes_per_frame.to_be_bytes());
    entry.extend_from_slice(&1u32.to_be_bytes()); // frames per packet
    let stsd = full(
        b"stsd",
        0,
        0,
        &[1u32.to_be_bytes().as_slice(), &boxed(b"lpcm", &entry)].concat(),
    );
    let stts = full(
        b"stts",
        0,
        0,
        &[
            1u32.to_be_bytes(),
            (frames as u32).to_be_bytes(),
            1u32.to_be_bytes(),
        ]
        .concat(),
    );
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for (index, &(_, count)) in chunks.iter().enumerate() {
        if runs.last().is_none_or(|&(_, previous)| previous != count) {
            runs.push((index as u32 + 1, count));
        }
    }
    let mut stsc = (runs.len() as u32).to_be_bytes().to_vec();
    for (first, count) in runs {
        for value in [first, count, 1] {
            stsc.extend_from_slice(&value.to_be_bytes());
        }
    }
    let stsc = full(b"stsc", 0, 0, &stsc);
    let stsz = full(
        b"stsz",
        0,
        0,
        &[bytes_per_frame.to_be_bytes(), (frames as u32).to_be_bytes()].concat(),
    );
    let offsets: Vec<u64> = chunks.iter().map(|&(offset, _)| offset).collect();
    let stbl = boxed(
        b"stbl",
        &[stsd, stts, stsc, stsz, chunk_offsets(&offsets)].concat(),
    );
    let dref = full(
        b"dref",
        0,
        0,
        &[1u32.to_be_bytes().as_slice(), &full(b"alis", 0, 1, &[])].concat(),
    );
    let minf = boxed(
        b"minf",
        &[full(b"smhd", 0, 0, &[0u8; 4]), boxed(b"dinf", &dref), stbl].concat(),
    );
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    boxed(b"trak", &[tkhd, mdia].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg(args: &[&str], output: &Path) -> bool {
        let Some(bin) = crate::ff::ffmpeg_bin() else {
            return false;
        };
        Command::new(bin)
            .args(["-y", "-v", "error"])
            .args(args)
            .arg(output)
            .status()
            .is_ok_and(|s| s.success())
    }

    fn stream_md5(path: &Path, map: &str) -> Vec<u8> {
        Command::new(crate::ff::ffmpeg_bin().unwrap())
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", map, "-c", "copy", "-f", "md5", "-"])
            .output()
            .unwrap()
            .stdout
    }

    #[test]
    fn b_frame_camera_keeps_picture_timecode_and_gets_resampled_audio() {
        let dir = tempfile::tempdir().unwrap();
        let camera = dir.path().join("camera.mp4");
        let recorder = dir.path().join("recorder.wav");
        let camera_ok = ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=160x90:r=30000/1001:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=sample_rate=48000:duration=3",
                "-c:v",
                "libx264",
                "-bf",
                "2",
                "-c:a",
                "aac",
                "-timecode",
                "01:00:00;00",
                "-shortest",
            ],
            &camera,
        );
        // Stereo 44.1 kHz recorder: a click at exactly 1 s on both channels.
        let recorder_ok = ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "aevalsrc='if(between(t,1,1.001),0.9,0)|if(between(t,1,1.001),-0.9,0)':s=44100:d=4",
                "-c:a",
                "pcm_s24le",
            ],
            &recorder,
        );
        if !(camera_ok && recorder_ok) {
            eprintln!("SKIP: fixtures unavailable");
            return;
        }
        let output = dir.path().join("out.mov");
        let audio = ReplacementAudio {
            backend: &crate::portable::PortableBackend,
            source: &recorder,
            source_in: 0.5,
            duration: 2.0,
            leading_silence: 0.25,
            total: 3.0,
        };
        replace_audio(&camera, &output, &audio, &AtomicBool::new(false)).unwrap();

        // Picture packets are byte-identical and the file decodes cleanly.
        assert_eq!(stream_md5(&camera, "0:v:0"), stream_md5(&output, "0:v:0"));
        let check = Command::new(crate::ff::ffmpeg_bin().unwrap())
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            check.status.success() && check.stderr.is_empty(),
            "{check:?}"
        );

        // Native reader: timecode survives, one float stereo track at 48 kHz.
        let probe = isobmff::inspect(&output).expect("probe output");
        let source = isobmff::inspect(&camera).unwrap();
        assert_eq!(
            probe.video.as_ref().unwrap().source_timecode,
            source.video.as_ref().unwrap().source_timecode
        );
        assert_eq!(probe.audio_streams.len(), 1);
        let stream = &probe.audio_streams[0];
        assert_eq!((stream.sample_rate, stream.channels), (48_000.0, 2));
        assert_eq!((stream.bit_depth, stream.is_float), (Some(32), Some(true)));
        assert_eq!(
            probe.video.unwrap().frame_duration,
            source.video.unwrap().frame_duration
        );

        // Click: source 1.0 s − in 0.5 s + delay 0.25 s = 0.75 s (36000).
        let decoded = Command::new(crate::ff::ffmpeg_bin().unwrap())
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
            .output()
            .unwrap()
            .stdout;
        let samples: Vec<f32> = decoded
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(samples.len(), 3 * 48_000 * 2);
        let first = samples.iter().position(|s| s.abs() > 0.3).unwrap() / 2;
        assert!((35_990..36_010).contains(&first), "click at {first}");
        let peak = samples[first * 2 + 1];
        assert!(peak < -0.3, "channels kept apart: {peak}");
        // Nothing after the 2 s selection (ends at 2.25 s).
        assert!(
            samples[2 * 48_000 * 2 + 12_100 * 2..]
                .iter()
                .all(|s| s.abs() < 1e-3)
        );

        // Portable decode reads the written track back.
        let native = crate::sym::decode_window(
            &output,
            0.7,
            0.1,
            align_core::AudioAnalysisSource::default(),
        );
        assert!(native.is_ok(), "{native:?}");
    }

    #[test]
    fn cancellation_and_unsupported_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let camera = dir.path().join("camera.mov");
        let recorder = dir.path().join("recorder.wav");
        if !ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=64x36:r=25:d=1",
                "-c:v",
                "mpeg4",
            ],
            &camera,
        ) || !ffmpeg(
            &["-f", "lavfi", "-i", "sine=sample_rate=48000:duration=1"],
            &recorder,
        ) {
            eprintln!("SKIP: fixtures unavailable");
            return;
        }
        let audio = ReplacementAudio {
            backend: &crate::portable::PortableBackend,
            source: &recorder,
            source_in: 0.0,
            duration: 1.0,
            leading_silence: 0.0,
            total: 1.0,
        };
        let output = dir.path().join("out.mov");
        let cancelled = replace_audio(&camera, &output, &audio, &AtomicBool::new(true));
        assert!(matches!(
            cancelled,
            Err(RemuxError::Render(RenderError::Cancelled))
        ));
        let mts = dir.path().join("clip.mts");
        std::fs::write(&mts, b"not quicktime").unwrap();
        assert!(matches!(
            replace_audio(&mts, &output, &audio, &AtomicBool::new(false)),
            Err(RemuxError::Unsupported(_))
        ));
    }
}
