//! Sparse spectral landmark fingerprints.
//! Port of Sources/AlignCore/Fingerprint.swift (vDSP -> realfft).
//!
//! Default parameters remain identical to Swift:
//! 8 kHz mono, 1024-sample frames, 512 hop, 5 bands, top-2 peaks per frame
//! with score >= 2.5, target deltas [8, 20, 36].
//! Search levels select 1–5 peaks per frame without changing quality gates.
//! FFT numerics differ in the last ulp from vDSP, so the on-disk cache
//! version is bumped (see `cache.rs`) — matching behaviour is unchanged.

use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Search work budget: more spectral bands generate more candidate landmarks.
/// Match confidence, repeated-take rejection and fine waveform validation
/// remain identical at every level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchAccuracy {
    Fast,
    #[default]
    Balanced,
    Thorough,
    Deep,
    Exhaustive,
}

impl SearchAccuracy {
    pub const ALL: [Self; 5] = [
        Self::Fast,
        Self::Balanced,
        Self::Thorough,
        Self::Deep,
        Self::Exhaustive,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Thorough => "Thorough",
            Self::Deep => "Deep",
            Self::Exhaustive => "Exhaustive",
        }
    }

    fn peak_count(self) -> usize {
        match self {
            Self::Fast => 1,
            Self::Balanced => 2,
            Self::Thorough => 3,
            Self::Deep => 4,
            Self::Exhaustive => 5,
        }
    }

    pub fn is_balanced(&self) -> bool {
        *self == Self::Balanced
    }

    pub fn cache_key(self, source: &str) -> String {
        // Preserve the existing cache for the unchanged default extractor.
        if self == Self::Balanced {
            source.to_owned()
        } else {
            format!("{source}-spectral-peaks-{}-v1", self.peak_count())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hash: u64,
    pub frame: u32,
}

pub const SAMPLE_RATE: u32 = 8_000;
pub const FRAME_SIZE: usize = 1_024;
pub const HOP_SIZE: usize = 512;
const HALF: usize = FRAME_SIZE / 2;

#[derive(Clone, Copy, Debug)]
struct Peak {
    frame: u32,
    bin: u16,
}

pub struct FingerprintExtractor {
    accuracy: SearchAccuracy,
    forward: Arc<dyn RealToComplex<f32>>,
    spectrum: Vec<num_complex::Complex32>,
    magnitudes: Vec<f32>,
    window: Vec<f32>,
    scratch: Vec<f32>,
    pending: Vec<f32>,
    consumed: usize,
    frame_number: u32,
    peaks: Vec<Peak>,
}

impl Default for FingerprintExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl FingerprintExtractor {
    pub fn new() -> Self {
        Self::with_accuracy(SearchAccuracy::Balanced)
    }

    pub fn with_accuracy(accuracy: SearchAccuracy) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FRAME_SIZE);
        let spectrum = forward.make_output_vec();
        // vDSP_HANN_NORM equivalent: 0.5 * (1 - cos(2*pi*n/(N-1))).
        // Relative peak picking is insensitive to the NORM scale factor.
        let window: Vec<f32> = (0..FRAME_SIZE)
            .map(|n| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (FRAME_SIZE - 1) as f32).cos()
            })
            .collect();
        let pending = Vec::with_capacity(FRAME_SIZE * 16);
        Self {
            accuracy,
            forward,
            spectrum,
            magnitudes: vec![0.0; HALF],
            window,
            scratch: vec![0.0; FRAME_SIZE],
            pending,
            consumed: 0,
            frame_number: 0,
            peaks: Vec::new(),
        }
    }

    /// Streaming ingest. Bounded memory: the pending buffer is compacted
    /// every 8 frames, exactly like the Swift version.
    pub fn consume(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        self.process_available();
    }

    pub fn finish(mut self) -> Vec<Fingerprint> {
        self.process_available();
        Self::make_fingerprints(&self.peaks)
    }

    fn process_available(&mut self) {
        while self.pending.len() - self.consumed >= FRAME_SIZE {
            for i in 0..FRAME_SIZE {
                self.scratch[i] = self.pending[self.consumed + i] * self.window[i];
            }
            // realfft processes in place; disjoint field borrows, no alloc.
            self.forward
                .process(&mut self.scratch, &mut self.spectrum)
                .expect("fft");
            for (i, c) in self.spectrum.iter().take(HALF).enumerate() {
                // vDSP_zvmags = squared magnitude.
                self.magnitudes[i] = c.re * c.re + c.im * c.im;
            }
            self.analyze_frame();
            self.consumed += HOP_SIZE;
            self.frame_number = self.frame_number.wrapping_add(1);
        }
        if self.consumed >= FRAME_SIZE * 8 {
            self.pending.drain(..self.consumed);
            self.consumed = 0;
        }
    }

    fn analyze_frame(&mut self) {
        const BANDS: [std::ops::Range<usize>; 5] = [10..32, 32..64, 64..128, 128..256, 256..486];
        let mut candidates: [(f32, usize); 5] = [(0.0, 0); 5];
        for (slot, band) in BANDS.iter().enumerate() {
            let mut sum = 0.0f32;
            let mut max = 0.0f32;
            let mut max_bin = band.start;
            for bin in band.start..band.end {
                let v = self.magnitudes[bin];
                sum += v;
                if v > max && v >= self.magnitudes[bin - 1] && v >= self.magnitudes[bin + 1] {
                    max = v;
                    max_bin = bin;
                }
            }
            let mean = sum / band.len() as f32;
            candidates[slot] = (max / mean.max(1e-12), max_bin);
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        for (score, bin) in candidates.iter().take(self.accuracy.peak_count()) {
            if *score >= 2.5 {
                self.peaks.push(Peak {
                    frame: self.frame_number,
                    bin: (*bin / 2) as u16,
                });
            }
        }
    }

    fn make_fingerprints(peaks: &[Peak]) -> Vec<Fingerprint> {
        const DELTAS: [u32; 3] = [8, 20, 36];
        let mut by_frame: HashMap<u32, Vec<Peak>> = HashMap::new();
        for p in peaks {
            by_frame.entry(p.frame).or_default().push(*p);
        }
        let mut out = Vec::with_capacity(peaks.len() * DELTAS.len() * 2);
        for anchor in peaks {
            for delta in DELTAS {
                if let Some(targets) = by_frame.get(&anchor.frame.wrapping_add(delta)) {
                    for t in targets {
                        let hash = (anchor.bin as u64) << 32 | (t.bin as u64) << 16 | delta as u64;
                        out.push(Fingerprint {
                            hash,
                            frame: anchor.frame,
                        });
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        (0..(secs * SAMPLE_RATE as f32) as usize)
            .map(|n| (2.0 * std::f32::consts::PI * freq * n as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn deeper_search_recovers_shared_band_under_independent_tones() {
        use crate::{ClipFingerprints, ClipId, match_fingerprints};
        // Two recordings share a changing middle-band tone. Independent
        // narrow-band interference dominates the two highest peak scores.
        // Ground truth is a 16-hop delay, independent of the matcher.
        fn recording(seed: u64, shared: bool) -> Vec<f32> {
            let mut phases = [0.0f64; 5];
            (0..SAMPLE_RATE as usize * 30)
                .map(|n| {
                    let slot = n / 2048;
                    let mut sample = 0.0;
                    for (band, phase) in phases.iter_mut().enumerate() {
                        let (lo, width) =
                            [(12, 17), (35, 26), (68, 55), (133, 116), (262, 215)][band];
                        let key = if band == 2 && shared {
                            17
                        } else {
                            seed + band as u64 * 71
                        };
                        let mut h = (slot as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15)
                            ^ key.wrapping_mul(0xbf58476d1ce4e5b9);
                        h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                        h ^= h >> 27;
                        let bin = lo + h as usize % width;
                        *phase += 2.0 * std::f64::consts::PI * bin as f64 / FRAME_SIZE as f64;
                        sample += phase.sin() as f32 * if band == 2 { 0.3 } else { 0.1 };
                    }
                    sample
                })
                .collect()
        }
        let a = recording(123, true);
        let mut b = vec![0.0; HOP_SIZE * 16];
        b.extend(recording(456, true));
        let other = recording(789, false);
        let run = |accuracy, right: &[f32]| {
            let features = [&a[..], right]
                .into_iter()
                .enumerate()
                .map(|(i, samples)| {
                    let mut ex = FingerprintExtractor::with_accuracy(accuracy);
                    ex.consume(samples);
                    ClipFingerprints {
                        clip_id: ClipId(format!("{i}")),
                        fingerprints: ex.finish(),
                    }
                })
                .collect();
            match_fingerprints(features, None, &[])
        };
        let balanced = run(SearchAccuracy::Balanced, &b);
        let deep = run(SearchAccuracy::Exhaustive, &b);
        assert!(
            balanced.is_empty(),
            "default unexpectedly recovered fixture: {balanced:?}"
        );
        assert_eq!(deep.len(), 1, "deeper search must recover shared signal");
        assert!(
            (deep[0].offset - 16.0 * HOP_SIZE as f64 / SAMPLE_RATE as f64).abs()
                < HOP_SIZE as f64 / SAMPLE_RATE as f64,
            "{deep:?}"
        );
        struct Provider<'a> {
            left: &'a [f32],
            right: &'a [f32],
        }
        impl crate::WindowProvider for Provider<'_> {
            fn window(
                &self,
                id: &ClipId,
                start: f64,
                duration: f64,
                _: crate::AudioAnalysisSource,
            ) -> Option<crate::AudioWindow> {
                let source = if id.0 == "0" { self.left } else { self.right };
                let first = (start * crate::FINE_SAMPLE_RATE).round() as usize;
                let count = (duration * crate::FINE_SAMPLE_RATE).round() as usize;
                let samples = (first..first + count)
                    .map(|i| {
                        let a = source.get(i / 2).copied().unwrap_or(0.0);
                        if i % 2 == 0 {
                            a
                        } else {
                            (a + source.get(i / 2 + 1).copied().unwrap_or(0.0)) * 0.5
                        }
                    })
                    .collect();
                Some(crate::AudioWindow {
                    sample_rate: crate::FINE_SAMPLE_RATE,
                    start: first as f64 / crate::FINE_SAMPLE_RATE,
                    samples,
                })
            }
        }
        let durations = [
            (ClipId::new("0"), a.len() as f64 / SAMPLE_RATE as f64),
            (ClipId::new("1"), b.len() as f64 / SAMPLE_RATE as f64),
        ]
        .into_iter()
        .collect();
        let refined = crate::refine_forest(
            &durations,
            &deep,
            &crate::MatchPolicy::default(),
            &HashMap::new(),
            &Provider {
                left: &a,
                right: &b,
            },
            &std::sync::atomic::AtomicBool::new(false),
            None,
        );
        assert_eq!(
            refined.len(),
            1,
            "shared signal must survive waveform verification"
        );
        assert!(
            (refined[0].offset - 1.024).abs() < 1.0 / crate::FINE_SAMPLE_RATE,
            "{refined:?}"
        );

        for level in SearchAccuracy::ALL {
            assert!(
                run(level, &other).is_empty(),
                "unrelated audio matched at {level:?}"
            );
        }
    }

    #[test]
    fn silence_produces_no_fingerprints() {
        let mut ex = FingerprintExtractor::new();
        ex.consume(&vec![0.0; 32_768]);
        assert!(ex.finish().is_empty());
    }

    #[test]
    fn tone_produces_stable_hashes() {
        // Determinism across OS/CPU is a hard requirement: same samples must
        // give same hashes on macOS / Windows / Linux.
        let run = || {
            let mut ex = FingerprintExtractor::new();
            ex.consume(&sine(440.0, 4.0));
            ex.finish()
        };
        let a = run();
        let b = run();
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn shifted_tone_shares_hashes_with_frame_offset() {
        let mut a = FingerprintExtractor::new();
        a.consume(&sine(440.0, 6.0));
        let fa = a.finish();
        let mut b = FingerprintExtractor::new();
        b.consume(&vec![0.0; SAMPLE_RATE as usize]); // 1 s of silence prefix
        b.consume(&sine(440.0, 6.0));
        let fb = b.finish();
        let ha: std::collections::HashSet<u64> = fa.iter().map(|f| f.hash).collect();
        let hb: std::collections::HashSet<u64> = fb.iter().map(|f| f.hash).collect();
        let shared = ha.intersection(&hb).count();
        assert!(shared > 10, "expected shared landmarks, got {shared}");
    }
}
