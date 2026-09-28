//! Direct linear-PCM reads from WAVE (RIFF, RF64, BW64) and QuickTime/MP4.
//!
//! Uncompressed camera audio (Sony, Panasonic, Canon, Blackmagic, ARRI) is
//! stored one frame per sample. Generic demuxing then yields one packet per
//! audio frame; reading whole chunks through the sample tables instead is
//! sample-accurate, seek-free and allocation-light. Stream indices count
//! every sound track in file order, like the header probe.
//!
//! Recorder files are read the same way. This also covers RF64/BW64, which
//! Symphonia does not demux and which Align itself writes for renders over
//! 4 GiB, so no external decoder is needed for any uncompressed WAVE.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::DecodeError;
use crate::isobmff::{self, TrackKind};

/// Frames converted per callback block.
const BLOCK_FRAMES: usize = 32_768;

pub struct PcmTrack {
    file: File,
    pub sample_rate: f64,
    pub channels: usize,
    bits: u32,
    float: bool,
    big_endian: bool,
    /// Presentation start in frames (edit list).
    lead: u64,
    /// (file offset, first frame, frame count) per chunk.
    chunks: Vec<(u64, u64, u64)>,
    total: u64,
}

impl PcmTrack {
    /// `None` when the stream is not plain interleaved PCM in a WAVE,
    /// QuickTime or MP4 file (callers then use the general decoder).
    pub fn open(path: &Path, stream_index: usize) -> Option<Self> {
        if let Some(wave) = Self::open_wave(path) {
            return (stream_index == 0).then_some(wave);
        }
        if !isobmff::is_candidate(path) {
            return None;
        }
        let movie = isobmff::read(path)?;
        let track = movie
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .nth(stream_index)?;
        let audio = track.audio.as_ref()?;
        let (bits, float) = (audio.bit_depth?, audio.is_float?);
        if !matches!((bits, float), (16 | 24 | 32, false) | (32 | 64, true))
            || !one_frame_per_sample(&track.sample_entry)
            || audio.sample_rate.fract() != 0.0
        {
            return None;
        }
        let bytes_per_frame = audio.channels as u64 * bits as u64 / 8;
        let mut chunks = Vec::with_capacity(track.chunk_offsets.len());
        let mut frame = 0u64;
        for (index, &offset) in track.chunk_offsets.iter().enumerate() {
            let number = index as u32 + 1;
            let run = track
                .chunk_runs
                .iter()
                .rev()
                .find(|run| run.first_chunk <= number)?;
            let frames = run.samples_per_chunk as u64;
            chunks.push((offset, frame, frames));
            frame += frames;
        }
        let file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        // Every chunk must lie inside the file; anything else is not a
        // layout this reader understands.
        if chunks
            .iter()
            .any(|&(offset, _, frames)| offset + frames * bytes_per_frame > length)
            || frame != track.sample_count()
        {
            return None;
        }
        let lead = if track.timescale > 0 {
            (track.media_start as f64 * audio.sample_rate / track.timescale as f64).round() as u64
        } else {
            0
        };
        Some(Self {
            file,
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            bits,
            float,
            big_endian: audio.big_endian,
            lead,
            chunks,
            total: frame,
        })
    }

    /// RIFF/RF64/BW64 WAVE with 16/24/32-bit integer or 32/64-bit float
    /// samples (`WAVE_FORMAT_EXTENSIBLE` included).
    fn open_wave(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        let mut header = [0u8; 12];
        file.read_exact(&mut header).ok()?;
        let large = match &header[..4] {
            b"RIFF" => false,
            b"RF64" | b"BW64" => true,
            _ => return None,
        };
        if &header[8..] != b"WAVE" {
            return None;
        }
        let mut ds64_data = None;
        let mut format = None;
        let mut offset = 12u64;
        while offset + 8 <= length {
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut chunk = [0u8; 8];
            file.read_exact(&mut chunk).ok()?;
            let size = u32::from_le_bytes(chunk[4..].try_into().ok()?) as u64;
            let body = offset + 8;
            match &chunk[..4] {
                b"ds64" if large && size >= 24 => {
                    let mut sizes = [0u8; 16];
                    file.read_exact(&mut sizes).ok()?;
                    ds64_data = Some(u64::from_le_bytes(sizes[8..].try_into().ok()?));
                }
                b"fmt " if size >= 16 => {
                    let mut fmt = vec![0u8; size.min(40) as usize];
                    file.read_exact(&mut fmt).ok()?;
                    format = Some(fmt);
                }
                b"data" => {
                    let fmt = format?;
                    let field = |at: usize| u16::from_le_bytes([fmt[at], fmt[at + 1]]);
                    let mut tag = field(0);
                    if tag == 0xfffe && fmt.len() >= 26 {
                        tag = field(24); // SubFormat GUID starts with the tag
                    }
                    let channels = field(2) as usize;
                    let sample_rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
                    let block_align = field(12) as u64;
                    if channels == 0 || sample_rate == 0 || block_align % channels as u64 != 0 {
                        return None;
                    }
                    let bits = (block_align / channels as u64 * 8) as u32;
                    let float = match (tag, bits) {
                        (1, 16 | 24 | 32) => false,
                        (3, 32 | 64) => true,
                        _ => return None,
                    };
                    let declared = match (large, size) {
                        (true, 0xffff_ffff) => ds64_data?,
                        _ => size,
                    };
                    // Interrupted recordings keep a header larger than the file.
                    let frames = declared.min(length.saturating_sub(body)) / block_align;
                    return Some(Self {
                        file,
                        sample_rate: sample_rate as f64,
                        channels,
                        bits,
                        float,
                        big_endian: false,
                        lead: 0,
                        chunks: vec![(body, 0, frames)],
                        total: frames,
                    });
                }
                _ => {}
            }
            let size = match (&chunk[..4], large, size) {
                (b"data", true, 0xffff_ffff) => ds64_data?,
                _ => size,
            };
            offset = body + size + (size & 1);
        }
        None
    }

    /// Stream format and duration without decoding.
    pub fn probe(&self) -> crate::backend::ProbeReport {
        crate::backend::ProbeReport {
            duration_seconds: self.total.saturating_sub(self.lead) as f64 / self.sample_rate,
            audio_streams: vec![crate::backend::AudioStreamProbe {
                sample_rate: self.sample_rate,
                channels: self.channels,
                bit_depth: Some(self.bits),
                is_float: Some(self.float),
            }],
            has_video: false,
            video: None,
        }
    }

    /// Stream presented frames `[start, start + count)` as planar blocks,
    /// converting each sample with `convert` (monomorphized per layout).
    fn read_with<T: Copy, F: Fn(&[u8]) -> T>(
        &mut self,
        start: u64,
        count: Option<u64>,
        convert: F,
        consume: &mut dyn FnMut(Vec<Vec<T>>) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        let first = start + self.lead;
        let end = count.map_or(self.total, |n| (first + n).min(self.total));
        if first >= end {
            return Ok(());
        }
        let width = (self.bits / 8) as usize;
        let frame_bytes = width * self.channels;
        let index = self
            .chunks
            .partition_point(|&(_, chunk_first, frames)| chunk_first + frames <= first);
        let mut bytes = Vec::new();
        let mut frame = first;
        for &(offset, chunk_first, frames) in &self.chunks[index..] {
            if frame >= end {
                break;
            }
            let skip = frame - chunk_first;
            let mut remaining = (frames - skip).min(end - frame) as usize;
            self.file
                .seek(SeekFrom::Start(offset + skip * frame_bytes as u64))?;
            while remaining > 0 {
                let take = remaining.min(BLOCK_FRAMES);
                bytes.resize(take * frame_bytes, 0);
                self.file.read_exact(&mut bytes)?;
                let planes = (0..self.channels)
                    .map(|channel| {
                        bytes[channel * width..]
                            .chunks(frame_bytes)
                            .map(|frame| convert(&frame[..width]))
                            .collect()
                    })
                    .collect();
                consume(planes)?;
                remaining -= take;
                frame += take as u64;
            }
        }
        Ok(())
    }

    pub fn read_f32(
        &mut self,
        start: u64,
        count: Option<u64>,
        consume: &mut dyn FnMut(Vec<Vec<f32>>) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        const SCALE: f32 = 1.0 / 2_147_483_648.0;
        macro_rules! int {
            ($w:literal, $be:literal) => {
                self.read_with(
                    start,
                    count,
                    |s| integer::<$w, $be>(s) as f32 * SCALE,
                    consume,
                )
            };
        }
        match (self.bits, self.float, self.big_endian) {
            (16, false, false) => int!(2, false),
            (16, false, true) => int!(2, true),
            (24, false, false) => int!(3, false),
            (24, false, true) => int!(3, true),
            (32, false, false) => int!(4, false),
            (32, false, true) => int!(4, true),
            (_, true, big_endian) => {
                self.read_with(start, count, |s| float(s, big_endian) as f32, consume)
            }
            _ => Err(DecodeError::InvalidPcm),
        }
    }

    pub fn read_i32(
        &mut self,
        start: u64,
        count: Option<u64>,
        consume: &mut dyn FnMut(Vec<Vec<i32>>) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        match (self.bits, self.float, self.big_endian) {
            (16, false, false) => self.read_with(start, count, integer::<2, false>, consume),
            (16, false, true) => self.read_with(start, count, integer::<2, true>, consume),
            (24, false, false) => self.read_with(start, count, integer::<3, false>, consume),
            (24, false, true) => self.read_with(start, count, integer::<3, true>, consume),
            (32, false, false) => self.read_with(start, count, integer::<4, false>, consume),
            (32, false, true) => self.read_with(start, count, integer::<4, true>, consume),
            (_, true, big_endian) => self.read_with(
                start,
                count,
                |s| {
                    (float(s, big_endian) * 2_147_483_648.0)
                        .round()
                        .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
                },
                consume,
            ),
            _ => Err(DecodeError::InvalidPcm),
        }
    }
}

/// QuickTime v1/v2 sound descriptions must describe one frame per sample.
fn one_frame_per_sample(entry: &[u8]) -> bool {
    let body = entry.get(8..).unwrap_or_default();
    let field = |at: usize| {
        body.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match body.get(8..10).map(|v| u16::from_be_bytes([v[0], v[1]])) {
        Some(0) => true,
        Some(1) => field(28) == Some(1),
        Some(2) => field(60) == Some(1),
        _ => false,
    }
}

/// Signed `W`-byte integer sample widened to the top of an i32.
#[inline(always)]
fn integer<const W: usize, const BIG_ENDIAN: bool>(sample: &[u8]) -> i32 {
    let mut word = [0u8; 4];
    for (index, slot) in word[..W].iter_mut().enumerate() {
        *slot = if BIG_ENDIAN {
            sample[index]
        } else {
            sample[W - 1 - index]
        };
    }
    i32::from_be_bytes(word)
}

#[inline(always)]
fn float(sample: &[u8], big_endian: bool) -> f64 {
    match (sample.len(), big_endian) {
        (4, true) => f32::from_be_bytes(sample.try_into().unwrap_or_default()) as f64,
        (4, false) => f32::from_le_bytes(sample.try_into().unwrap_or_default()) as f64,
        (8, true) => f64::from_be_bytes(sample.try_into().unwrap_or_default()),
        _ => f64::from_le_bytes(sample.try_into().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_widths_and_byte_orders_widen_to_i32() {
        assert_eq!(integer::<2, false>(&[0x01, 0x80]), i32::MIN + 0x0001_0000);
        assert_eq!(integer::<2, true>(&[0x80, 0x01]), i32::MIN + 0x0001_0000);
        assert_eq!(integer::<3, false>(&[0xff, 0xff, 0x7f]), 0x7fff_ff00);
        assert_eq!(integer::<3, true>(&[0x7f, 0xff, 0xff]), 0x7fff_ff00);
        assert_eq!(integer::<4, false>(&[1, 0, 0, 0x80]), i32::MIN + 1);
        assert_eq!(float(&0.25f32.to_be_bytes(), true), 0.25);
        assert_eq!(float(&(-0.5f64).to_le_bytes(), false), -0.5);
    }

    fn wave(large: bool, extensible: bool, samples: &[i32]) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| s.to_le_bytes()[1..].to_vec())
            .collect();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&(if extensible { 0xfffeu16 } else { 1 }).to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&48_000u32.to_le_bytes());
        fmt.extend_from_slice(&(48_000u32 * 6).to_le_bytes());
        fmt.extend_from_slice(&6u16.to_le_bytes());
        fmt.extend_from_slice(&24u16.to_le_bytes());
        if extensible {
            fmt.extend_from_slice(&22u16.to_le_bytes());
            fmt.extend_from_slice(&24u16.to_le_bytes());
            fmt.extend_from_slice(&3u32.to_le_bytes());
            fmt.extend_from_slice(&1u16.to_le_bytes());
            fmt.extend_from_slice(&[0; 14]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(if large { b"RF64" } else { b"RIFF" });
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        if large {
            out.extend_from_slice(b"ds64");
            out.extend_from_slice(&28u32.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        let size = if large { u32::MAX } else { data.len() as u32 };
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn wave_variants_read_the_same_samples() {
        let dir = tempfile::tempdir().unwrap();
        let samples: Vec<i32> = (0..2_000)
            .map(|i| (i * 40_503 % 16_777_216 - 8_388_608) << 8)
            .collect();
        for (name, large, extensible) in [
            ("riff.wav", false, false),
            ("extensible.wav", false, true),
            ("rf64.wav", true, false),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, wave(large, extensible, &samples)).unwrap();
            let mut track = PcmTrack::open(&path, 0).unwrap_or_else(|| panic!("{name}"));
            assert!(PcmTrack::open(&path, 1).is_none());
            let probe = track.probe();
            assert_eq!(probe.audio_streams[0].channels, 2);
            assert_eq!(probe.audio_streams[0].bit_depth, Some(24));
            assert!((probe.duration_seconds - 1_000.0 / 48_000.0).abs() < 1e-12);
            let mut read = Vec::new();
            track
                .read_i32(100, Some(300), &mut |planes| {
                    read.extend(planes[0].iter().zip(&planes[1]).flat_map(|(l, r)| [*l, *r]));
                    Ok(())
                })
                .unwrap();
            assert_eq!(read, samples[200..800], "{name}");
        }
        // A truncated recording reads what exists instead of failing.
        let path = dir.path().join("truncated.wav");
        let bytes = wave(false, false, &samples);
        std::fs::write(&path, &bytes[..bytes.len() - 600]).unwrap();
        assert_eq!(PcmTrack::open(&path, 0).unwrap().total, 900);
    }

    /// Every layout FFmpeg writes must decode to the same samples through
    /// the direct reader and through Symphonia/FFmpeg.
    #[test]
    fn direct_reads_match_reference_decoders() {
        let Some(ffmpeg) = crate::ff::ffmpeg_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        for codec in [
            "pcm_s16le",
            "pcm_s16be",
            "pcm_s24le",
            "pcm_s24be",
            "pcm_s32le",
            "pcm_f32le",
            "pcm_f32be",
            "pcm_f64le",
        ] {
            let clip = dir.path().join(format!("{codec}.mov"));
            let ok = std::process::Command::new(&ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=64x36:r=25:d=2",
                ])
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "aevalsrc='sin(2*PI*440*t)*0.7|sin(2*PI*660*t)*-0.4':s=48000:d=2",
                ])
                .args(["-c:v", "mpeg4", "-c:a", codec, "-shortest"])
                .arg(&clip)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                eprintln!("SKIP {codec}");
                continue;
            }
            let mut track =
                PcmTrack::open(&clip, 0).unwrap_or_else(|| panic!("{codec} not direct"));
            assert_eq!(track.channels, 2);
            let mut direct = Vec::new();
            track
                .read_f32(12_345, Some(4_000), &mut |planes| {
                    direct.extend(planes[0].iter().zip(&planes[1]).map(|(l, r)| (*l, *r)));
                    Ok(())
                })
                .unwrap();
            let mut reference = Vec::new();
            let mut pipe = crate::ff::AudioPipe::spawn(
                &clip,
                0,
                Some((12_345.0 / 48_000.0, 4_000.0 / 48_000.0)),
                2,
            )
            .unwrap();
            pipe.pump(&mut |frames| {
                reference.extend(frames.chunks_exact(2).map(|f| (f[0], f[1])));
                Ok(())
            })
            .unwrap();
            assert_eq!(direct.len(), 4_000, "{codec}");
            for (index, (a, b)) in direct.iter().zip(&reference).enumerate() {
                assert!(
                    (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6,
                    "{codec} frame {index}: {a:?} vs {b:?}"
                );
            }
        }
    }
}
