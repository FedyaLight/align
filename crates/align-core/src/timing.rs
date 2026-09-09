//! Video timing classification shared by both media backends.
//!
//! Sample durations or presentation deltas varying beyond max(10 µs, 0.5%
//! of the minimum) over at least two observations indicate variable timing.
//! Nominal frame rates within 0.1% of a broadcast rate use its canonical
//! rational duration.

use crate::model::{MediaTime, VideoFrameRateMode};

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoTimingInspection {
    pub frame_duration: Option<MediaTime>,
    pub mode: VideoFrameRateMode,
}

#[derive(Clone, Debug, Default)]
pub struct RangeAccumulator {
    count: usize,
    minimum: f64,
    maximum: f64,
}

impl RangeAccumulator {
    pub fn new() -> Self {
        Self {
            count: 0,
            minimum: f64::INFINITY,
            maximum: 0.0,
        }
    }

    pub fn observe(&mut self, value: f64) {
        if !value.is_finite() || value <= 0.0 {
            return;
        }
        self.count += 1;
        self.minimum = self.minimum.min(value);
        self.maximum = self.maximum.max(value);
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn is_variable(&self) -> bool {
        self.count >= 2 && self.maximum - self.minimum > 0.000_01f64.max(self.minimum * 0.005)
    }
}

pub fn classify(sample_durations: &[f64], presentation_deltas: &[f64]) -> VideoFrameRateMode {
    let mut durations = RangeAccumulator::new();
    let mut deltas = RangeAccumulator::new();
    for &v in sample_durations {
        durations.observe(v);
    }
    for &v in presentation_deltas {
        deltas.observe(v);
    }
    if durations.count().max(deltas.count()) < 2 {
        return VideoFrameRateMode::Unknown;
    }
    if durations.is_variable() || deltas.is_variable() {
        VideoFrameRateMode::Variable
    } else {
        VideoFrameRateMode::Constant
    }
}

pub fn canonical_frame_duration(
    nominal_fps: f64,
    fallback: Option<MediaTime>,
) -> Option<MediaTime> {
    const STANDARDS: [(f64, i64, i32); 11] = [
        (23.976, 1_001, 24_000),
        (24.0, 1, 24),
        (25.0, 1, 25),
        (29.97, 1_001, 30_000),
        (30.0, 1, 30),
        (50.0, 1, 50),
        (59.94, 1_001, 60_000),
        (60.0, 1, 60),
        (100.0, 1, 100),
        (119.88, 1_001, 120_000),
        (120.0, 1, 120),
    ];
    if nominal_fps > 0.0 {
        let mut best: Option<(f64, MediaTime)> = None;
        for (fps, value, timescale) in STANDARDS {
            let err = (fps - nominal_fps).abs();
            if best.is_none_or(|(e, _)| err < e) {
                best = Some((err, MediaTime::new(value, timescale)));
            }
        }
        if let Some((err, duration)) = best {
            // Nearest standard within 0.1%; ties select the first listed rate.
            let fps = duration.timescale as f64 / duration.value as f64;
            if err / fps < 0.001 {
                return Some(duration);
            }
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_track_classifies() {
        let dts = vec![0.04; 100];
        let deltas = vec![0.04; 99];
        assert_eq!(classify(&dts, &deltas), VideoFrameRateMode::Constant);
    }

    #[test]
    fn jittered_deltas_mean_variable() {
        let dts = vec![0.04; 100];
        let mut deltas = vec![0.04; 99];
        deltas[50] = 0.08;
        assert_eq!(classify(&dts, &deltas), VideoFrameRateMode::Variable);
    }

    #[test]
    fn tiny_jitter_within_tolerance_stays_constant() {
        // 0.5 % of 40 ms = 200 µs tolerance; 10 µs floor.
        let dts = vec![0.04, 0.040_05];
        assert_eq!(classify(&dts, &[]), VideoFrameRateMode::Constant);
    }

    #[test]
    fn sparse_evidence_is_unknown() {
        assert_eq!(classify(&[0.04], &[]), VideoFrameRateMode::Unknown);
        assert_eq!(classify(&[], &[]), VideoFrameRateMode::Unknown);
    }

    #[test]
    fn canonical_snaps_standards() {
        assert_eq!(
            canonical_frame_duration(25.0, None),
            Some(MediaTime::new(1, 25))
        );
        assert_eq!(
            canonical_frame_duration(29.97, None),
            Some(MediaTime::new(1_001, 30_000))
        );
        assert_eq!(
            canonical_frame_duration(23.976, None),
            Some(MediaTime::new(1_001, 24_000))
        );
        // Off-standard falls back.
        let fb = MediaTime::new(7, 199);
        assert_eq!(canonical_frame_duration(28.4, Some(fb)), Some(fb));
        assert_eq!(canonical_frame_duration(0.0, Some(fb)), Some(fb));
        assert_eq!(canonical_frame_duration(0.0, None), None);
    }
}
