//! Direct linear-PCM reads from QuickTime/MP4 camera files.
//!
//! Uncompressed camera audio (Sony, Panasonic, Canon, Blackmagic, ARRI) is
//! stored one frame per sample. Generic demuxing then yields one packet per
//! audio frame; reading whole chunks through the sample tables instead is
//! sample-accurate, seek-free and allocation-light. Stream indices count
//! every sound track in file order, like the header probe.

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
    /// `None` when the stream is not plain interleaved PCM in a QuickTime
    /// or MP4 file (callers then use the general decoder).
    pub fn open(path: &Path, stream_index: usize) -> Option<Self> {
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

    /// Stream presented frames `[start, start + count)` as planar blocks of
    /// raw integer or float samples, converted by `convert`.
    fn read<T: Copy + Default>(
        &mut self,
        start: u64,
        count: Option<u64>,
        convert: impl Fn(&[u8], u32, bool, bool) -> T,
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
                let mut planes = vec![Vec::with_capacity(take); self.channels];
                for frame_bytes in bytes.chunks_exact(frame_bytes) {
                    for (plane, sample) in planes.iter_mut().zip(frame_bytes.chunks_exact(width)) {
                        plane.push(convert(sample, self.bits, self.float, self.big_endian));
                    }
                }
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
        self.read(start, count, sample_f32, consume)
    }

    pub fn read_i32(
        &mut self,
        start: u64,
        count: Option<u64>,
        consume: &mut dyn FnMut(Vec<Vec<i32>>) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        self.read(start, count, sample_i32, consume)
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

/// Signed integer sample widened to the top of an i32.
fn integer(sample: &[u8], big_endian: bool) -> i32 {
    let mut word = [0u8; 4];
    let width = sample.len();
    if big_endian {
        word[..width].copy_from_slice(sample);
    } else {
        for (slot, byte) in word[..width].iter_mut().zip(sample.iter().rev()) {
            *slot = *byte;
        }
    }
    i32::from_be_bytes(word)
}

fn float(sample: &[u8], big_endian: bool) -> f64 {
    match (sample.len(), big_endian) {
        (4, true) => f32::from_be_bytes(sample.try_into().unwrap_or_default()) as f64,
        (4, false) => f32::from_le_bytes(sample.try_into().unwrap_or_default()) as f64,
        (8, true) => f64::from_be_bytes(sample.try_into().unwrap_or_default()),
        _ => f64::from_le_bytes(sample.try_into().unwrap_or_default()),
    }
}

fn sample_f32(sample: &[u8], _bits: u32, is_float: bool, big_endian: bool) -> f32 {
    if is_float {
        float(sample, big_endian) as f32
    } else {
        integer(sample, big_endian) as f32 / 2_147_483_648.0
    }
}

fn sample_i32(sample: &[u8], _bits: u32, is_float: bool, big_endian: bool) -> i32 {
    if is_float {
        (float(sample, big_endian) * 2_147_483_648.0)
            .round()
            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
    } else {
        integer(sample, big_endian)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_widths_and_byte_orders_widen_to_i32() {
        assert_eq!(integer(&[0x01, 0x80], false), i32::MIN + 0x0001_0000);
        assert_eq!(integer(&[0x80, 0x01], true), i32::MIN + 0x0001_0000);
        assert_eq!(integer(&[0xff, 0xff, 0x7f], false), 0x7fff_ff00);
        assert_eq!(integer(&[0x7f, 0xff, 0xff], true), 0x7fff_ff00);
        assert_eq!(sample_f32(&[0x00, 0x40], 16, false, false), 0.5);
        assert_eq!(sample_i32(&0.25f32.to_be_bytes(), 32, true, true), 1 << 29);
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
