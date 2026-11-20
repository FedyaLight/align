//! GCC-PHAT alignment with parabolic sub-sample peak interpolation.
//!
//! Accepted windows contain 16K–128K samples. Peak/RMS must be at least 4
//! and prominence at least 1.015. FFT plans are shared across refine workers.

use std::sync::{Arc, OnceLock};

use num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// FFT plans and analysis window for one window size. Immutable, so every
/// refine worker shares them.
struct SizeTables {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    hann: Vec<f32>,
}

/// Only four power-of-two sizes (16K–128K) pass the gate in [`align`].
static TABLES: [OnceLock<SizeTables>; 4] = [const { OnceLock::new() }; 4];

/// The shared tables for `count`, a power of two from 16K to 128K.
fn tables(count: usize) -> &'static SizeTables {
    debug_assert!(count.is_power_of_two() && (16_384..=131_072).contains(&count));
    TABLES[count.trailing_zeros() as usize - 14].get_or_init(|| {
        let mut planner = RealFftPlanner::<f32>::new();
        SizeTables {
            forward: planner.plan_fft_forward(count),
            inverse: planner.plan_fft_inverse(count),
            hann: (0..count)
                .map(|i| {
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (count - 1) as f32).cos()
                })
                .collect(),
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GccPhatResult {
    pub lag_samples: f64,
    pub quality: f64,
}

#[derive(Default)]
struct Buffers {
    time: Vec<f32>,
    left: Vec<Complex32>,
    right: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

thread_local! {
    static BUFFERS: std::cell::RefCell<Buffers> = Default::default();
}

pub fn align(left: &[f32], right: &[f32], maximum_lag: usize) -> Option<GccPhatResult> {
    let available = left.len().min(right.len());
    let count = 131_072.min(prev_pow2(available));
    if count < 16_384 || maximum_lag == 0 || maximum_lag >= count / 2 - 1 {
        return None;
    }

    BUFFERS.with_borrow_mut(|buffers| {
        let tables = tables(count);
        let Buffers {
            time,
            left: lb,
            right: rb,
            scratch,
        } = buffers;
        time.resize(count, 0.0);
        lb.resize(count / 2 + 1, Complex32::default());
        rb.resize(lb.len(), Complex32::default());
        scratch.resize(
            tables
                .forward
                .get_scratch_len()
                .max(tables.inverse.get_scratch_len()),
            Complex32::default(),
        );
        for (signal, spectrum) in [(left, &mut *lb), (right, &mut *rb)] {
            preprocess(&signal[..count], &tables.hann, time);
            if rms(time) <= 1e-5 {
                return None;
            }
            tables
                .forward
                .process_with_scratch(time, spectrum, scratch)
                .ok()?;
        }
        // conj(left) * right gives a positive lag for a delayed right channel.
        for (l, r) in lb.iter_mut().zip(rb.iter()) {
            let c = *r * l.conj();
            let mag = c.norm();
            *l = if mag > 1e-12 {
                c / mag
            } else {
                Complex32::default()
            };
        }
        tables
            .inverse
            .process_with_scratch(lb, time, scratch)
            .ok()?;
        // Normalize only the samples used by the peak and confidence tests.
        peak(time, maximum_lag)
    })
}

fn peak(samples: &[f32], maximum_lag: usize) -> Option<GccPhatResult> {
    let count = samples.len();
    let correlation = |lag| samples[idx(lag, count)] / count as f32;
    let mut best_lag = 0isize;
    let mut best_val = f32::MIN;
    for lag in -(maximum_lag as isize)..=(maximum_lag as isize) {
        let v = correlation(lag).abs();
        if v > best_val {
            best_val = v;
            best_lag = lag;
        }
    }
    let mut second = 0.0f32;
    let mut sum_sq = 0.0f64;
    let mut n = 0usize;
    for lag in -(maximum_lag as isize)..=(maximum_lag as isize) {
        let v = correlation(lag).abs();
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

    let prev = correlation(best_lag - 1);
    let center = correlation(best_lag);
    let next = correlation(best_lag + 1);
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

fn preprocess(input: &[f32], window: &[f32], out: &mut [f32]) {
    for (i, &v) in input.iter().enumerate() {
        // Hann (NORM scale-insensitive here) * first-order pre-emphasis.
        let p = if i > 0 { v - 0.97 * input[i - 1] } else { 0.0 };
        out[i] = p * window[i];
    }
}

fn rms(v: &[f32]) -> f32 {
    let sum: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum / v.len() as f64).sqrt() as f32
}

pub(crate) fn prev_pow2(v: usize) -> usize {
    v.checked_ilog2().map_or(0, |n| 1usize << n)
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
        for size in [16_384, 32_768, 65_536, 131_072, 32_768] {
            let base = pseudo_noise(size, 0x1234);
            let shift = 1_234;
            let delayed = delayed(&base, shift);
            let r = align(&base, &delayed, 4_000).expect("should align");
            // Signed convention: right lags left → positive.
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
    }

    #[test]
    fn half_sample_delay_at_the_edge_of_the_search_window() {
        let base = pseudo_noise(32_768, 5);
        let mut right = vec![0.0; base.len()];
        for i in 126..right.len() {
            right[i] = (base[i - 125] + base[i - 126]) * 0.5;
        }
        for (left, right, sign) in [(&base, &right, 1.), (&right, &base, -1.)] {
            let result = align(left, right, 125).unwrap();
            assert!(
                (result.lag_samples - sign * 125.5).abs() < 0.03,
                "{result:?}"
            );
            // The established confidence for this truncated search is ~0.899.
            assert!((0.89..0.91).contains(&result.quality), "{result:?}");
        }
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
