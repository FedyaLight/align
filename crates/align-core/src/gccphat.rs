//! GCC-PHAT fine alignment with sub-sample interpolation.
//! Port of Sources/AlignCore/GCCPHAT.swift (vDSP DFT -> rustfft).
//!
//! Thresholds are identical: min 16k samples, max 128k, peak/RMS >= 4,
//! prominence >= 1.015, parabolic sub-sample refinement. Quality blend
//! 0.7 * peakToRMS + 0.3 * prominence is unchanged.

use num_complex::Complex32;
use rustfft::FftPlanner;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GccPhatResult {
    pub lag_samples: f64,
    pub quality: f64,
}

pub fn align(left: &[f32], right: &[f32], maximum_lag: usize) -> Option<GccPhatResult> {
    let available = left.len().min(right.len());
    let count = 131_072.min(prev_pow2(available));
    if count < 16_384 || maximum_lag == 0 || maximum_lag + 1 >= count / 2 {
        return None;
    }

    let l = preprocess(&left[..count]);
    let r = preprocess(&right[..count]);
    let l_rms = rms(&l);
    let r_rms = rms(&r);
    if l_rms <= 1e-5 || r_rms <= 1e-5 {
        return None;
    }

    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(count);
    let inverse = planner.plan_fft_inverse(count);

    let mut lb: Vec<Complex32> = l.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    let mut rb: Vec<Complex32> = r.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    forward.process(&mut lb);
    forward.process(&mut rb);

    // Cross-spectrum with PHAT weighting, Swift-identical conjugation:
    // conj(left) * right. The sign of the resulting lag is load-bearing
    // (observation timestamps add it directly), so it must match Swift
    // exactly — a flip here biases every refined offset by twice the
    // in-window lag with a deceptively tiny residual.
    let mut cross: Vec<Complex32> = Vec::with_capacity(count);
    for i in 0..count {
        let c = rb[i] * lb[i].conj();
        let mag = c.norm();
        if mag > 1e-12 {
            cross.push(c / mag);
        } else {
            cross.push(Complex32::new(0.0, 0.0));
        }
    }
    inverse.process(&mut cross);
    // rustfft inverse is unnormalized; scale cancels in relative metrics.
    let correlation: Vec<f32> = cross.iter().map(|c| c.re / count as f32).collect();

    let mut best_lag = 0isize;
    let mut best_val = f32::MIN;
    for lag in -(maximum_lag as isize)..=(maximum_lag as isize) {
        let v = correlation[idx(lag, count)].abs();
        if v > best_val {
            best_val = v;
            best_lag = lag;
        }
    }
    let mut second = 0.0f32;
    let mut sum_sq = 0.0f64;
    let mut n = 0usize;
    for lag in -(maximum_lag as isize)..=(maximum_lag as isize) {
        let v = correlation[idx(lag, count)].abs();
        sum_sq += (v as f64) * (v as f64);
        n += 1;
        if (lag - best_lag).abs() > 32 && v > second {
            second = v;
        }
    }
    let rms_all = (sum_sq / n as f64).sqrt();
    let peak_to_rms = best_val as f64 / rms_all.max(1e-12);
    let prominence = best_val as f64 / (second as f64).max(1e-12);
    if peak_to_rms < 4.0 || prominence < 1.015 {
        return None;
    }

    let prev = correlation[idx(best_lag - 1, count)];
    let center = correlation[idx(best_lag, count)];
    let next = correlation[idx(best_lag + 1, count)];
    let denom = prev - 2.0 * center + next;
    let frac = if denom.abs() > 1e-12 {
        0.5 * (prev - next) / denom
    } else {
        0.0
    };
    let quality = 0.7 * (0.0f64.max((peak_to_rms - 4.0) / 12.0).min(1.0))
        + 0.3 * (0.0f64.max((prominence - 1.0) / 0.5).min(1.0));
    Some(GccPhatResult {
        lag_samples: best_lag as f64 + frac as f64,
        quality,
    })
}

fn preprocess(input: &[f32]) -> Vec<f32> {
    let n = input.len();
    let mut out = vec![0.0f32; n];
    for (i, &v) in input.iter().enumerate() {
        // Hann (NORM scale-insensitive here) * first-order pre-emphasis.
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
        let p = if i > 0 { v - 0.97 * input[i - 1] } else { 0.0 };
        out[i] = p * w;
    }
    out
}

fn rms(v: &[f32]) -> f32 {
    let sum: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum / v.len() as f64).sqrt() as f32
}

fn prev_pow2(mut v: usize) -> usize {
    if v == 0 {
        return 0;
    }
    v |= v >> 1;
    v |= v >> 2;
    v |= v >> 4;
    v |= v >> 8;
    v |= v >> 16;
    v |= v >> 32;
    v - (v >> 1)
}

fn idx(lag: isize, count: usize) -> usize {
    let m = lag % count as isize;
    if m >= 0 {
        m as usize
    } else {
        (count as isize + m) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delayed(noise: &[f32], delay: usize) -> Vec<f32> {
        let mut out = vec![0.0; noise.len()];
        out[delay..].copy_from_slice(&noise[..noise.len() - delay]);
        out
    }

    /// Simple LCG so the test needs no RNG dependency and is bit-identical
    /// on all three OSes.
    fn pseudo_noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn finds_known_delay() {
        let base = pseudo_noise(32_768, 0x1234);
        let shift = 1_234;
        let delayed = delayed(&base, shift);
        let r = align(&base, &delayed, 4_000).expect("should align");
        // Signed convention (Swift-identical): right lags left → positive.
        // The sign is load-bearing downstream — never abs() it here.
        assert!(
            (r.lag_samples - shift as f64).abs() < 2.0,
            "lag={}",
            r.lag_samples
        );
        assert!(r.quality > 0.0);
        // And the mirror: swapped inputs flip the sign.
        let r2 = align(&delayed, &base, 4_000).expect("should align");
        assert!(
            (r2.lag_samples + shift as f64).abs() < 2.0,
            "lag={}",
            r2.lag_samples
        );
    }

    #[test]
    fn silence_rejected() {
        assert!(align(&vec![0.0; 32_768], &vec![0.0; 32_768], 1000).is_none());
    }

    #[test]
    fn too_short_rejected() {
        assert!(align(&vec![0.1; 1024], &vec![0.1; 1024], 100).is_none());
    }
}
