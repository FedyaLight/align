//! Shared streaming channel selection and resampling.
//!
//! Explicit channel selection takes precedence. Automatic selection chooses
//! the loudest channel per 512-source-frame window and keeps the previous
//! channel while it remains within 90% of the loudest. Mixed mode averages
//! channels explicitly; automatic mode avoids opposite-phase cancellation.
//!
//! 4608-frame chunks pass through `rubato::FftFixedIn`; equal sample rates
//! bypass resampling. The filter tail is flushed and its group delay removed
//! so output sample positions remain aligned across sample-rate pairs.

use rubato::{FftFixedIn, Resampler};

use crate::DecodeError;

/// Input frames per resampler call. Split in two sub-chunks, 4608 gives
/// FFT lengths with only small prime factors for the common rate pairs
/// (48 kHz → 8 kHz: 384 × 6, → 16 kHz: 768 × 3; 44.1 kHz: 6 × 441). With
/// 4096, 48 → 16 kHz needed a 683-point prime FFT (Rader's algorithm) that
/// dominated analysis time.
const CHUNK: usize = 4608;

// All FFTs here process one already selected/mixed channel, CHUNK frames
// and two sub-chunks. Only retain common analysis geometries, so media with
// unusual rates cannot leave oversized FFT plans in a long-lived process.
struct IdleResampler {
    rates: (usize, usize),
    fft: FftFixedIn<f32>,
}
static IDLE_FFT: std::sync::Mutex<Vec<IdleResampler>> = std::sync::Mutex::new(Vec::new());

fn pooled(rates: (usize, usize)) -> bool {
    matches!(rates, (44_100 | 48_000 | 96_000, 8_000 | 16_000))
}

fn resampler(rates: (usize, usize)) -> Result<FftFixedIn<f32>, DecodeError> {
    if pooled(rates) {
        let mut idle = IDLE_FFT.lock().unwrap();
        if let Some(i) = idle.iter().rposition(|entry| entry.rates == rates) {
            return Ok(idle.swap_remove(i).fft);
        }
    }
    FftFixedIn::new(rates.0, rates.1, CHUNK, 2, 1).map_err(|e| DecodeError::Resample(e.to_string()))
}

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
    energies: Vec<f64>,
    resampled: Vec<Vec<f32>>,
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

impl<F> Drop for MonoPipe<F> {
    fn drop(&mut self) {
        let rates = (
            self.source_rate.round() as usize,
            self.target_rate.round() as usize,
        );
        if pooled(rates)
            && let Some(mut fft) = self.resampler.take()
        {
            fft.reset();
            let mut idle = IDLE_FFT.lock().unwrap();
            if idle.len() == 12 {
                idle.remove(0);
            }
            idle.push(IdleResampler { rates, fft });
        }
    }
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
        if let Some(ch) = explicit
            && ch >= channels
        {
            return Err(DecodeError::InvalidPcm);
        }
        let resampler = if (source_rate - target_rate).abs() < 0.001 {
            None
        } else {
            Some(resampler((
                source_rate.round() as usize,
                target_rate.round() as usize,
            ))?)
        };
        let skip = resampler.as_ref().map(|r| r.output_delay()).unwrap_or(0);
        let resampled = resampler
            .as_ref()
            .map_or_else(Vec::new, |r| r.output_buffer_allocate(true));
        Ok(Self {
            channels,
            explicit,
            mix_channels: mix_channels && explicit.is_none(),
            selected: None,
            adaptive_pending: Vec::new(),
            energies: vec![0.; channels],
            resampled,
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

    /// Push owned planar channels.
    pub fn push_owned(&mut self, planes: &[Vec<f32>]) -> Result<(), DecodeError> {
        self.planar_from(planes, 0)
    }

    /// Push planar channels (Symphonia AudioBuffer channel slices).
    pub fn push_planar(&mut self, planes: &[&[f32]]) -> Result<(), DecodeError> {
        self.planar_from(planes, 0)
    }

    pub(crate) fn planar_from<P: AsRef<[f32]>>(
        &mut self,
        planes: &[P],
        mut start: usize,
    ) -> Result<(), DecodeError> {
        if planes.len() != self.channels {
            return Err(DecodeError::InvalidPcm);
        }
        let n = planes.iter().map(|p| p.as_ref().len()).min().unwrap_or(0);
        if self.mix_channels {
            let scale = 1.0 / self.channels as f32;
            return self.push_mono(
                (start..n)
                    .map(|i| planes.iter().map(|plane| plane.as_ref()[i]).sum::<f32>() * scale),
            );
        }
        if self.explicit.is_some() || self.channels == 1 {
            let channel = self.explicit.unwrap_or(0);
            self.selected = Some(channel);
            return self.push_mono(planes[channel].as_ref()[start..n].iter().copied());
        }
        while start < n {
            let needed = 512 - self.adaptive_pending.len() / self.channels;
            let end = (start + needed).min(n);
            if end - start == 512 {
                // An intact window: analyse the planes and copy only the winner.
                for (energy, plane) in self.energies.iter_mut().zip(planes) {
                    *energy = plane.as_ref()[start..end]
                        .iter()
                        .map(|&v| v as f64 * v as f64)
                        .sum();
                }
                let channel = align_core::select_mono_channel(&self.energies, self.selected);
                self.selected = Some(channel);
                self.push_mono(planes[channel].as_ref()[start..end].iter().copied())?;
            } else {
                // Keep only the packet boundary in the common interleaved buffer.
                for i in start..end {
                    self.adaptive_pending
                        .extend(planes.iter().map(|p| p.as_ref()[i]));
                }
                if end - start == needed {
                    self.flush_adaptive()?;
                }
            }
            start = end;
        }
        Ok(())
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
        self.energies.fill(0.0);
        for i in 0..n {
            for ch in 0..self.channels {
                let v = frames[i * self.channels + ch] as f64;
                self.energies[ch] += v * v;
            }
        }
        let selected = align_core::select_mono_channel(&self.energies, self.selected);
        self.selected = Some(selected);
        selected
    }

    fn push_mono(&mut self, samples: impl Iterator<Item = f32>) -> Result<(), DecodeError> {
        let mut pending = std::mem::take(&mut self.pending);
        let before = pending.len();
        pending.extend(samples);
        if self.resampler.is_none() {
            self.emit(&pending)?;
            pending.clear();
        } else {
            self.in_total += (pending.len() - before) as u64;
            let (chunks, tail) = pending.as_chunks::<CHUNK>();
            for chunk in chunks {
                self.emit_chunk(chunk)?;
            }
            let consumed = pending.len() - tail.len();
            pending.drain(..consumed);
        }
        self.pending = pending;
        Ok(())
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
        let mut out = std::mem::take(&mut self.resampled);
        let (_, written) = resampler
            .process_into_buffer(&[chunk], &mut out, None)
            .map_err(|e| DecodeError::Resample(e.to_string()))?;
        let result = self.emit(&out[0][..written]);
        self.resampled = out;
        result
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
        // silence, not signal. Rubato has no end flag, so stop at the
        // expected output length.
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
            let (stereo, _) = packet.as_chunks::<2>();
            let left: Vec<f32> = stereo.iter().map(|frame| frame[0]).collect();
            let right: Vec<f32> = stereo.iter().map(|frame| frame[1]).collect();
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
    fn cancelled_resampling_cannot_leave_audio_in_the_next_clip() {
        let mut loud = MonoPipe::new(48000., 16000., 1, None, false, |_| {
            Err(DecodeError::Cancelled)
        })
        .unwrap();
        assert!(matches!(
            loud.push_planar(&[&[1.0; CHUNK * 2]]),
            Err(DecodeError::Cancelled)
        ));
        drop(loud);
        let mut frames = 0;
        let mut quiet = MonoPipe::new(48000., 16000., 1, None, false, |samples| {
            assert!(samples.iter().all(|&s| s == 0.));
            frames += samples.len();
            Ok(())
        })
        .unwrap();
        quiet.push_planar(&[&[0.; 48000]]).unwrap();
        quiet.finish().unwrap();
        assert!(frames >= 16000);
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
