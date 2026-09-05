//! Shared streaming chain: discrete-channel mono selection + fixed-block
//! resampling. Used by every decode path (Symphonia packets, FFmpeg pipe,
//! both engines on macOS) so channel choice and resample numerics are
//! identical everywhere.
//!
//! Mirrors Swift `consumeAdaptiveMono` + `MonoSampleRateConverter`:
//! - explicit channel override wins; otherwise the loudest channel per
//!   fixed 512-source-frame window, keeping the previous one while within 90% (hysteresis) —
//!   opposite-phase stereo never cancels out, and no FFmpeg-side downmix
//!   is ever used for automatic analysis; an explicit mixed mode averages
//!   every channel for sources where programme audio is distributed;
//! - fixed 4096-frame input chunks through `rubato::FftFixedIn` (sinc);
//!   equal rates bypass the resampler entirely (bit-exact passthrough,
//!   like Swift's `converter == nil`);
//! - `finish()` flushes the filter tail like Swift's `endOfStream` loop,
//!   so stream length is exact and no rate-dependent truncation bias
//!   appears between files.
//!
//! Group-delay note: the sinc is linear-phase with a ratio-dependent
//! constant delay (`output_delay()`). Every stream drops exactly that many
//! leading output frames, so output sample `n` is true time `n/target_rate`
//! on *every* rate pair — no cross-rate bias between files (this is stricter
//! than Swift, which does not compensate `AVAudioConverter` latency).

use rubato::{FftFixedIn, Resampler};

use crate::DecodeError;

const CHUNK: usize = 4096;

/// Interleaved → planar (render path; decode chains stay interleaved for
/// zero-copy streaming).
pub(crate) fn deinterleave_f32(frames: &[f32], channels: usize) -> Vec<Vec<f32>> {
    let n = frames.len() / channels.max(1);
    (0..channels)
        .map(|ch| (0..n).map(|i| frames[i * channels + ch]).collect())
        .collect()
}

pub(crate) fn deinterleave_i32(frames: &[i32], channels: usize) -> Vec<Vec<i32>> {
    let n = frames.len() / channels.max(1);
    (0..channels)
        .map(|ch| (0..n).map(|i| frames[i * channels + ch]).collect())
        .collect()
}

pub struct MonoPipe<F> {
    channels: usize,
    explicit: Option<usize>,
    mix_channels: bool,
    selected: Option<usize>,
    adaptive_pending: Vec<f32>,
    resampler: Option<FftFixedIn<f32>>,
    source_rate: f64,
    target_rate: f64,
    /// Leading output frames still to drop (group-delay compensation).
    skip: usize,
    /// Emission cap during flush (content length); `None` while streaming.
    cap: Option<usize>,
    pending: Vec<f32>,
    in_total: u64,
    out_total: usize,
    consume: F,
}

impl<F> MonoPipe<F>
where
    F: FnMut(&[f32]) -> Result<(), DecodeError>,
{
    pub fn new(
        source_rate: f64,
        target_rate: f64,
        channels: usize,
        explicit: Option<usize>,
        mix_channels: bool,
        consume: F,
    ) -> Result<Self, DecodeError> {
        // NaN/infinite rates are rejected (plain `<=` would let NaN through).
        if !source_rate.is_finite()
            || source_rate <= 0.0
            || !target_rate.is_finite()
            || target_rate <= 0.0
            || channels == 0
        {
            return Err(DecodeError::InvalidPcm);
        }
        if let Some(ch) = explicit {
            if ch >= channels {
                return Err(DecodeError::InvalidPcm);
            }
        }
        let resampler = if (source_rate - target_rate).abs() < 0.001 {
            None
        } else {
            Some(
                FftFixedIn::new(
                    source_rate.round() as usize,
                    target_rate.round() as usize,
                    CHUNK,
                    2,
                    1,
                )
                .map_err(|e| DecodeError::Resample(e.to_string()))?,
            )
        };
        let skip = resampler.as_ref().map(|r| r.output_delay()).unwrap_or(0);
        Ok(Self {
            channels,
            explicit,
            mix_channels: mix_channels && explicit.is_none(),
            selected: None,
            adaptive_pending: Vec::new(),
            resampler,
            source_rate,
            target_rate,
            skip,
            cap: None,
            pending: Vec::new(),
            in_total: 0,
            out_total: 0,
            consume: (consume),
        })
    }

    /// Push planar channels (Symphonia `AudioBuffer.chan(i)` slices).
    pub fn push_planar(&mut self, planes: &[&[f32]]) -> Result<(), DecodeError> {
        if planes.len() != self.channels {
            return Err(DecodeError::InvalidPcm);
        }
        if planes.iter().any(|p| p.len() != planes[0].len()) {
            // Ragged trailing packet: process the common prefix only.
            let n = planes.iter().map(|p| p.len()).min().unwrap_or(0);
            if n == 0 {
                return Ok(());
            }
            return self.push_planar_truncated(planes, n);
        }
        let n = planes[0].len();
        self.push_planar_truncated(planes, n)
    }

    fn push_planar_truncated(&mut self, planes: &[&[f32]], n: usize) -> Result<(), DecodeError> {
        if self.mix_channels {
            let scale = 1.0 / self.channels as f32;
            return self.push_mono(
                (0..n).map(|i| planes.iter().map(|plane| plane[i]).sum::<f32>() * scale),
            );
        }
        if self.explicit.is_none() && self.channels > 1 {
            for start in (0..n).step_by(512) {
                let end = (start + 512).min(n);
                let interleaved: Vec<f32> = (start..end)
                    .flat_map(|i| planes.iter().map(move |plane| plane[i]))
                    .collect();
                self.push_adaptive(&interleaved)?;
            }
            return Ok(());
        }
        let channel = self.pick(planes, n, |planes, ch| {
            planes[ch][..n].iter().map(|&v| v as f64 * v as f64).sum()
        });
        self.push_mono(planes[channel][..n].iter().copied())
    }

    /// Push interleaved frames (FFmpeg `f32le` pipe). Never a downmix:
    /// discrete channels are kept for hysteresis selection.
    pub fn push_interleaved(&mut self, frames: &[f32]) -> Result<(), DecodeError> {
        let channels = self.channels;
        let n = frames.len() / channels;
        if n == 0 {
            return Ok(());
        }
        if self.mix_channels {
            let scale = 1.0 / channels as f32;
            return self.push_mono((0..n).map(move |i| {
                frames[i * channels..(i + 1) * channels].iter().sum::<f32>() * scale
            }));
        }
        if self.explicit.is_none() && channels > 1 {
            return self.push_adaptive(&frames[..n * channels]);
        }
        let channel = self.pick_channel_interleaved(frames, n);
        self.push_mono((0..n).map(move |i| frames[i * channels + channel]))
    }

    // Decoder packet sizes vary between reads and backends. Select channels
    // on fixed 512-source-frame windows anchored at the decode origin.
    fn push_adaptive(&mut self, frames: &[f32]) -> Result<(), DecodeError> {
        let quantum = 512 * self.channels;
        let mut remaining = frames;
        while !remaining.is_empty() {
            let take = (quantum - self.adaptive_pending.len()).min(remaining.len());
            self.adaptive_pending.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if self.adaptive_pending.len() == quantum {
                self.flush_adaptive()?;
            }
        }
        Ok(())
    }

    fn flush_adaptive(&mut self) -> Result<(), DecodeError> {
        if self.adaptive_pending.is_empty() {
            return Ok(());
        }
        let block = std::mem::take(&mut self.adaptive_pending);
        let channels = self.channels;
        let n = block.len() / channels;
        let channel = self.pick_channel_interleaved(&block, n);
        self.push_mono((0..n).map(|i| block[i * channels + channel]))?;
        self.adaptive_pending = block;
        self.adaptive_pending.clear();
        Ok(())
    }

    fn pick_channel_interleaved(&mut self, frames: &[f32], n: usize) -> usize {
        if let Some(ch) = self.explicit {
            self.selected = Some(ch);
            return ch;
        }
        let mut energies = vec![0.0f64; self.channels];
        for i in 0..n {
            for ch in 0..self.channels {
                let v = frames[i * self.channels + ch] as f64;
                energies[ch] += v * v;
            }
        }
        let selected = align_core::select_mono_channel(&energies, self.selected);
        self.selected = Some(selected);
        selected
    }

    fn pick(
        &mut self,
        planes: &[&[f32]],
        _n: usize,
        energy: impl Fn(&[&[f32]], usize) -> f64,
    ) -> usize {
        if let Some(ch) = self.explicit {
            self.selected = Some(ch);
            return ch;
        }
        let energies: Vec<f64> = (0..self.channels).map(|ch| energy(planes, ch)).collect();
        let selected = align_core::select_mono_channel(&energies, self.selected);
        self.selected = Some(selected);
        selected
    }

    fn push_mono(&mut self, samples: impl Iterator<Item = f32>) -> Result<(), DecodeError> {
        match self.resampler.as_mut() {
            None => {
                // Passthrough: collect to one slice per call (still streaming).
                let v: Vec<f32> = samples.collect();
                if !v.is_empty() {
                    self.out_total += v.len();
                    (self.consume)(&v)?;
                }
                Ok(())
            }
            Some(_) => {
                let before = self.pending.len();
                self.pending.extend(samples);
                self.in_total += (self.pending.len() - before) as u64;
                while self.pending.len() >= CHUNK {
                    let chunk: Vec<f32> = self.pending.drain(..CHUNK).collect();
                    self.emit_chunk(&chunk)?;
                }
                Ok(())
            }
        }
    }

    fn emit(&mut self, frames: &[f32]) -> Result<(), DecodeError> {
        let mut frames = frames;
        if self.skip > 0 {
            let drop = self.skip.min(frames.len());
            frames = &frames[drop..];
            self.skip -= drop;
        }
        // During flush the last quantum is trimmed to content length so
        // output length is exact (ring-down zeros past it carry no signal).
        if let Some(cap) = self.cap {
            let allow = cap.saturating_sub(self.out_total);
            frames = &frames[..frames.len().min(allow)];
        }
        if !frames.is_empty() {
            self.out_total += frames.len();
            (self.consume)(frames)?;
        }
        Ok(())
    }

    fn emit_chunk(&mut self, chunk: &[f32]) -> Result<(), DecodeError> {
        let resampler = self
            .resampler
            .as_mut()
            .expect("resampler present: guarded by caller");
        let out = resampler
            .process(&[chunk], None)
            .map_err(|e| DecodeError::Resample(e.to_string()))?;
        self.emit(&out[0])
    }

    pub fn finish(mut self) -> Result<(), DecodeError> {
        self.flush_adaptive()?;
        if self.resampler.is_none() {
            return Ok(());
        }
        if !self.pending.is_empty() {
            let rem: Vec<f32> = std::mem::take(&mut self.pending);
            let resampler = self
                .resampler
                .as_mut()
                .expect("resampler present: guarded by caller");
            let out = resampler
                .process_partial(Some(&[rem.as_slice()]), None)
                .map_err(|e| DecodeError::Resample(e.to_string()))?;
            self.emit(&out[0])?;
        }
        // Flush the filter tail until emitted (post-drop) output reaches
        // content length. Zero-padded rounds past that are ring-down
        // silence, not signal. (Swift's endOfStream loop stops on converter
        // state; rubato has no end flag, so the bound is counted instead
        // of sensed.)
        let expected = (self.in_total as f64 * self.target_rate / self.source_rate).ceil() as usize;
        self.cap = Some(expected);
        for _ in 0..64 {
            if self.out_total >= expected {
                break;
            }
            let resampler = self
                .resampler
                .as_mut()
                .expect("resampler present: guarded by caller");
            let out: Vec<Vec<f32>> = resampler
                .process_partial::<&[f32]>(None, None)
                .map_err(|e| DecodeError::Resample(e.to_string()))?;
            if out[0].is_empty() {
                break;
            }
            self.emit(&out[0])?;
        }
        Ok(())
    }

    /// Currently selected discrete channel (for tests/diagnostics).
    pub fn selected_channel(&self) -> Option<usize> {
        self.selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_is_exact() {
        let mut got = Vec::new();
        let mut pipe = MonoPipe::new(8000.0, 8000.0, 2, None, false, |s: &[f32]| {
            got.extend_from_slice(s);
            Ok(())
        })
        .unwrap();
        // Left loud, right silent → selects 0.
        let frames: Vec<f32> = (0..1000).flat_map(|i| [i as f32, 0.0]).collect();
        pipe.push_interleaved(&frames).unwrap();
        assert_eq!(pipe.selected_channel(), Some(0));
        pipe.finish().unwrap();
        assert_eq!(got.len(), 1000);
        assert!((got[999] - 999.0).abs() < 1e-6);
    }

    #[test]
    fn automatic_channel_is_independent_of_decoder_packet_sizes() {
        let frames: Vec<f32> = (0..12345)
            .flat_map(|i| {
                if i % 2300 < 1100 {
                    [0.9, 0.1]
                } else {
                    [0.1, 0.8]
                }
            })
            .collect();
        let run = |packet_frames: usize| {
            let mut got = Vec::new();
            let mut pipe = MonoPipe::new(48000.0, 16000.0, 2, None, false, |s: &[f32]| {
                got.extend_from_slice(s);
                Ok(())
            })
            .unwrap();
            for packet in frames.chunks(packet_frames * 2) {
                pipe.push_interleaved(packet).unwrap();
            }
            pipe.finish().unwrap();
            got
        };
        assert_eq!(run(512), run(4096));
        assert_eq!(run(1), run(12345));
        let mut planar = Vec::new();
        let mut pipe = MonoPipe::new(48000.0, 16000.0, 2, None, false, |s: &[f32]| {
            planar.extend_from_slice(s);
            Ok(())
        })
        .unwrap();
        for packet in frames.chunks(137 * 2) {
            let left: Vec<f32> = packet.chunks_exact(2).map(|frame| frame[0]).collect();
            let right: Vec<f32> = packet.chunks_exact(2).map(|frame| frame[1]).collect();
            pipe.push_planar(&[&left, &right]).unwrap();
        }
        pipe.finish().unwrap();
        assert_eq!(planar, run(512));
    }

    #[test]
    fn hysteresis_prefers_previous_channel() {
        let mut got = Vec::new();
        let mut pipe = MonoPipe::new(8000.0, 8000.0, 2, None, false, |s: &[f32]| {
            got.extend_from_slice(s);
            Ok(())
        })
        .unwrap();
        // Block 1: left loud → selects 0.
        let b1: Vec<f32> = (0..512).flat_map(|_| [1.0f32, 0.1]).collect();
        pipe.push_interleaved(&b1).unwrap();
        assert_eq!(pipe.selected_channel(), Some(0));
        // Block 2: right slightly louder but within 90% → keeps 0.
        let b2: Vec<f32> = (0..512).flat_map(|_| [1.0f32, 1.05]).collect();
        pipe.push_interleaved(&b2).unwrap();
        assert_eq!(pipe.selected_channel(), Some(0));
        // Block 3: right clearly louder → switches.
        let b3: Vec<f32> = (0..512).flat_map(|_| [0.1f32, 1.0]).collect();
        pipe.push_interleaved(&b3).unwrap();
        assert_eq!(pipe.selected_channel(), Some(1));
        pipe.finish().unwrap();
    }

    #[test]
    fn explicit_mix_averages_channels_without_changing_length() {
        let mut got = Vec::new();
        let mut pipe = MonoPipe::new(8000.0, 8000.0, 2, None, true, |s: &[f32]| {
            got.extend_from_slice(s);
            Ok(())
        })
        .unwrap();
        pipe.push_planar(&[&[1.0, 0.5, -1.0], &[1.0, -0.5, 1.0]])
            .unwrap();
        pipe.push_interleaved(&[0.25, 0.75, -0.25, -0.75]).unwrap();
        assert_eq!(pipe.selected_channel(), None);
        pipe.finish().unwrap();
        assert_eq!(got, vec![1.0, 0.0, 0.0, 0.5, -0.5]);
    }

    #[test]
    fn resample_changes_rate_approximately() {
        let mut got = Vec::new();
        let mut pipe = MonoPipe::new(44100.0, 8000.0, 1, Some(0), false, |s: &[f32]| {
            got.extend_from_slice(s);
            Ok(())
        })
        .unwrap();
        let one_sec: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin())
            .collect();
        for chunk in one_sec.chunks(4096) {
            pipe.push_planar(&[chunk]).unwrap();
        }
        pipe.finish().unwrap();
        // Content length ± one FFT quantum of (silent) ring-down tail.
        // Exact sample counts are not promised — determinism, bounded
        // emission and delay compensation are; see module docs.
        assert!((got.len() as isize - 8000).abs() < 800, "len={}", got.len());
        assert!(got.len() >= 8000 - 64, "lost content: len={}", got.len());
        // Tone survives resampling with healthy amplitude.
        let peak = got.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.5, "peak={peak}");
    }

    #[test]
    fn explicit_channel_validated() {
        assert!(MonoPipe::new(8000.0, 8000.0, 2, Some(2), false, |_: &[f32]| Ok(())).is_err());
        assert!(MonoPipe::new(8000.0, 8000.0, 0, None, false, |_: &[f32]| Ok(())).is_err());
    }

    #[test]
    fn cross_rate_impulses_share_true_time() {
        // Same acoustic events (impulses at t=1 s and t=3 s) sampled at
        // 44.1 kHz and 48 kHz must land on the same output milliseconds.
        // This is what makes offsets comparable between files recorded at
        // different rates — group-delay compensation, not luck.
        for (rate, secs) in [(44100.0, 5.0), (48000.0, 5.0)] {
            let n = (rate * secs) as usize;
            let mut input = vec![0.0f32; n];
            input[(rate * 1.0) as usize] = 1.0;
            input[(rate * 3.0) as usize] = 1.0;
            let mut got = Vec::new();
            let mut pipe = MonoPipe::new(rate, 8000.0, 1, Some(0), false, |s: &[f32]| {
                got.extend_from_slice(s);
                Ok(())
            })
            .unwrap();
            for chunk in input.chunks(4096) {
                pipe.push_planar(&[chunk]).unwrap();
            }
            pipe.finish().unwrap();
            for &t in &[1.0, 3.0] {
                let center = (t * 8000.0) as usize;
                let peak = (center.saturating_sub(200)..(center + 200).min(got.len()))
                    .max_by(|&a, &b| got[a].abs().total_cmp(&got[b].abs()))
                    .unwrap();
                let err_ms = (peak as f64 / 8000.0 - t).abs() * 1000.0;
                assert!(
                    err_ms < 1.0,
                    "rate={rate} t={t} peak={peak} err={err_ms:.2}ms"
                );
            }
        }
    }
}
