//! Sample-accurate refinement. Port of Sources/AlignCore/FineMatcher.swift.
//!
//! A coarse pair is re-observed through narrow 16 kHz mono windows
//! (audio-only: ≤ 8.3 s × 16 kHz ≈ 0.5 MiB resident per window) at several
//! span fractions; each window is aligned with GCC-PHAT (sub-sample).
//! - 1–3 observations agree (residual ≤ 12 ms) → 2-point affine map;
//! - otherwise (span ≥ 30 s) → 6 more windows + binary-search discontinuity
//!   localization, then a confirmed monotonic piecewise map (3–8 knots,
//!   residual ≤ 6 ms). Unconfirmed jumps are rejected, never guessed.
//! - short-span clock drift requires agreement with two held-out windows;
//!   long-span rate estimation uses 600 s / 50 anchors of coarse evidence.
//! - the forest pass applies the requested match threshold, then accepts
//!   refined pairs unless they close a cycle with confidence < 0.85.
//!
//! Audio access goes through [`WindowProvider`] so this crate stays
//! backend-agnostic: production wires it to `MediaBackend::decode_window_16k`
//! (either engine), tests use synthetic clips. Four parallel workers return
//! indexed results, so acceptance remains deterministic.

use std::collections::HashMap;

use crate::gccphat;
use crate::matcher::{MatchPolicy, PairAlignmentPoint, PairwiseMatch};
use crate::model::{AudioAnalysisSource, ClipId, MatchEvidence};
use crate::piecewise::PiecewiseTimeMapping;

pub const FINE_SAMPLE_RATE: f64 = 16_000.0;
/// Rate gate shared with the coarse matcher policy.
pub const RATE_SPAN_SECONDS: f64 = 600.0;
pub const RATE_MIN_ANCHORS: usize = 50;

// ------------------------------------------------------------ provider

#[derive(Clone, Debug, PartialEq)]
pub struct AudioWindow {
    /// Actual start in seconds (may differ from requested after clamping).
    pub start: f64,
    pub sample_rate: f64,
    pub samples: Vec<f32>,
}

/// 16 kHz mono window source. `None` = undecodable/cancelled (mirrors
/// Swift `try? await decoder.decodeWindow`).
pub trait WindowProvider: Send + Sync {
    fn window(
        &self,
        clip: &ClipId,
        start: f64,
        duration: f64,
        source: AudioAnalysisSource,
    ) -> Option<AudioWindow>;
}

// ------------------------------------------------------------ forest

#[derive(Clone, Debug, PartialEq)]
pub enum RefineStage {
    Rejected,
    Refined,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefineEvent {
    pub completed: usize,
    pub total: usize,
    pub left: ClipId,
    pub right: ClipId,
    pub stage: RefineStage,
}

/// Refine all candidates; `clips` are all known ids (disjoint-set universe).
/// Progress mirrors Swift `.refine` phase events (rejected non-usable first,
/// then one event per candidate in index order).
/// Refine all candidates with up to 4 parallel workers (mirrors Swift's
/// `withTaskGroup` reader gate); indexed results keep input order, so
/// outcomes are identical to sequential execution. Progress events carry a
/// monotonic completion count; disjoint-set acceptance stays sequential
/// and deterministic.
pub fn refine_forest(
    durations: &HashMap<ClipId, f64>,
    candidates: &[PairwiseMatch],
    match_policy: &MatchPolicy,
    sources: &HashMap<ClipId, AudioAnalysisSource>,
    provider: &dyn WindowProvider,
    cancel: &std::sync::atomic::AtomicBool,
    progress: Option<&(dyn Fn(RefineEvent) + Send + Sync)>,
) -> Vec<PairwiseMatch> {
    use std::sync::atomic::Ordering;
    let usable: Vec<&PairwiseMatch> = candidates.iter().filter(|m| is_usable(m)).collect();
    if let Some(report) = &progress {
        for m in candidates.iter().filter(|m| !is_usable(m)) {
            report(RefineEvent {
                completed: 0,
                total: usable.len(),
                left: m.left.clone(),
                right: m.right.clone(),
                stage: RefineStage::Rejected,
            });
        }
    }
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let total = usable.len();
    let refined: Vec<Option<PairwiseMatch>> = crate::parallel::par_map(&usable, 4, |candidate| {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let result = refine(
            candidate,
            match_policy.minimum_confidence(&candidate.left, &candidate.right),
            durations,
            sources,
            provider,
        );
        if let Some(report) = &progress {
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            report(RefineEvent {
                completed: done,
                total,
                left: candidate.left.clone(),
                right: candidate.right.clone(),
                stage: if result.is_some() {
                    RefineStage::Refined
                } else {
                    RefineStage::Rejected
                },
            });
        }
        result
    });

    let clips: Vec<_> = durations.keys().cloned().collect();
    let mut components = FineDisjointSet::new(&clips);
    let mut accepted = Vec::new();
    for (candidate, result) in usable.iter().zip(refined) {
        let closes_cycle = components.connected(&candidate.left, &candidate.right);
        let Some(result) = result else { continue };
        if closes_cycle && candidate.confidence < 0.85 {
            continue;
        }
        if !closes_cycle {
            components.union(&candidate.left, &candidate.right);
        }
        accepted.push(result);
    }
    accepted
}

pub fn can_estimate_rate(m: &PairwiseMatch) -> bool {
    m.covered_seconds >= RATE_SPAN_SECONDS && m.anchors >= RATE_MIN_ANCHORS
}

fn is_usable(m: &PairwiseMatch) -> bool {
    m.confidence >= 0.45 && m.anchors >= 3 && m.covered_seconds >= 3.0 && m.residual_seconds <= 0.1
}

// ------------------------------------------------------------ refine

#[derive(Clone, Copy, Debug)]
struct Observation {
    left: f64,
    right: f64,
    quality: f64,
}

fn refine(
    m: &PairwiseMatch,
    minimum_confidence: f64,
    durations: &HashMap<ClipId, f64>,
    sources: &HashMap<ClipId, AudioAnalysisSource>,
    provider: &dyn WindowProvider,
) -> Option<PairwiseMatch> {
    let (Some(&left_dur), Some(&right_dur)) = (durations.get(&m.left), durations.get(&m.right))
    else {
        return None;
    };
    let left_source = sources
        .get(&m.left)
        .copied()
        .unwrap_or(AudioAnalysisSource::Automatic);
    let right_source = sources
        .get(&m.right)
        .copied()
        .unwrap_or(AudioAnalysisSource::Automatic);

    let fractions: &[f64] = if m.covered_seconds < 20.0 {
        &[0.5]
    } else {
        &[0.15, 0.5, 0.85]
    };
    let window_duration = m.covered_seconds.mul_add(0.75, 0.0).clamp(2.1, 8.3);
    let mut observations = observe(
        m,
        left_dur,
        right_dur,
        fractions,
        window_duration,
        left_source,
        right_source,
        provider,
    );
    let needed = if fractions.len() == 1 { 1 } else { 2 };
    if observations.len() < needed {
        return None;
    }
    let allow_rate = observations.len() >= 3 && can_estimate_rate(m);
    let mut affine = fit(&observations, allow_rate);
    if !allow_rate && observations.len() >= 3 && m.covered_seconds >= 30.0 {
        let candidate = fit(&observations, true);
        // A short recording can accumulate audible drift too. Do not infer
        // it from three points alone: require material slip, a substantially
        // better fit, and predictions confirmed at independent positions.
        if (0.98..=1.02).contains(&candidate.rate)
            && candidate.residual <= 0.001
            && candidate.residual * 4.0 < affine.residual
            && crate::drift::needs_correction_rate(m.covered_seconds, candidate.rate)
        {
            let checks = observe(
                m,
                left_dur,
                right_dur,
                &[0.3, 0.7],
                2.1,
                left_source,
                right_source,
                provider,
            );
            if checks.len() == 2
                && checks.iter().all(|o| {
                    o.quality >= 0.6
                        && (o.right - (candidate.rate * o.left + candidate.offset)).abs() <= 0.002
                })
            {
                observations.extend(checks);
                affine = fit(&observations, true);
            }
        }
    }
    if !(0.98..=1.02).contains(&affine.rate) {
        return None;
    }

    let (alignment_points, residual) = if affine.residual <= 0.012 {
        (
            [m.left_start_seconds, m.left_end_seconds]
                .map(|s| PairAlignmentPoint {
                    left: s,
                    right: affine.rate * s + affine.offset,
                })
                .to_vec(),
            affine.residual,
        )
    } else {
        if m.covered_seconds < 30.0 {
            return None;
        }
        observations.extend(observe(
            m,
            left_dur,
            right_dur,
            &[0.06, 0.25, 0.35, 0.65, 0.75, 0.94],
            2.1,
            left_source,
            right_source,
            provider,
        ));
        observations.extend(refine_step(
            m,
            left_dur,
            right_dur,
            &observations,
            left_source,
            right_source,
            provider,
        ));
        let piecewise = piecewise_fit(&observations)?;
        (piecewise.points, piecewise.residual)
    };

    let average_quality =
        observations.iter().map(|o| o.quality).sum::<f64>() / observations.len() as f64;
    let confidence = (0.65 * m.confidence + 0.35 * average_quality).min(0.999);
    if confidence < minimum_confidence {
        return None;
    }
    Some(PairwiseMatch {
        left: m.left.clone(),
        right: m.right.clone(),
        rate: affine.rate,
        offset: affine.offset,
        confidence,
        anchors: m.anchors,
        left_start_seconds: m.left_start_seconds,
        left_end_seconds: m.left_end_seconds,
        covered_seconds: m.covered_seconds,
        residual_seconds: residual.max(1.0 / FINE_SAMPLE_RATE),
        alignment_points,
        // Coarse evidence is waveform-only at this stage (mirrors Swift,
        // which constructs the refined match with the default evidence).
        evidence: MatchEvidence::Waveform,
    })
}

#[allow(clippy::too_many_arguments)]
fn observe(
    m: &PairwiseMatch,
    left_dur: f64,
    right_dur: f64,
    fractions: &[f64],
    window_duration: f64,
    left_source: AudioAnalysisSource,
    right_source: AudioAnalysisSource,
    provider: &dyn WindowProvider,
) -> Vec<Observation> {
    let mut out = Vec::new();
    for &fraction in fractions {
        let left_center = m.left_start_seconds + fraction * m.covered_seconds;
        if let Some(o) = observe_one(
            m,
            left_dur,
            right_dur,
            left_center,
            window_duration,
            left_source,
            right_source,
            provider,
        ) {
            out.push(o);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn observe_one(
    m: &PairwiseMatch,
    left_dur: f64,
    right_dur: f64,
    left_center: f64,
    window_duration: f64,
    left_source: AudioAnalysisSource,
    right_source: AudioAnalysisSource,
    provider: &dyn WindowProvider,
) -> Option<Observation> {
    let right_center = m.rate * left_center + m.offset;
    let left_start = bounded_window_start(left_center, window_duration, left_dur);
    let right_start = bounded_window_start(right_center, window_duration, right_dur);
    let left_window = provider.window(&m.left, left_start, window_duration, left_source)?;
    let right_window = provider.window(&m.right, right_start, window_duration, right_source)?;
    let alignment = gccphat::align(
        &left_window.samples,
        &right_window.samples,
        (0.25 * FINE_SAMPLE_RATE) as usize,
    )?;
    let common = 131_072.min(prev_pow2(
        left_window.samples.len().min(right_window.samples.len()),
    ));
    if common == 0 {
        return None;
    }
    Some(Observation {
        left: left_window.start + (common / 2) as f64 / FINE_SAMPLE_RATE,
        right: right_window.start
            + ((common / 2) as f64 + alignment.lag_samples) / FINE_SAMPLE_RATE,
        quality: alignment.quality,
    })
}

/// Binary-search localization of a confirmed monotonic discontinuity
/// (dropped/duplicated samples): up to 6 probes while the bracket spans
/// more than a second; each probe must itself align well (quality ≥ 0.6).
fn refine_step(
    m: &PairwiseMatch,
    left_dur: f64,
    right_dur: f64,
    input: &[Observation],
    left_source: AudioAnalysisSource,
    right_source: AudioAnalysisSource,
    provider: &dyn WindowProvider,
) -> Vec<Observation> {
    let mut points = input.to_vec();
    points.sort_by(|a, b| a.left.total_cmp(&b.left));
    if points.len() < 6 {
        return Vec::new();
    }
    let offsets: Vec<f64> = points.iter().map(|p| p.right - m.rate * p.left).collect();
    let mut best_index = None;
    let mut best_jump = 0.0;
    for index in 1..points.len() {
        let jump = (offsets[index] - offsets[index - 1]).abs();
        let stable_before = index < 2 || (offsets[index - 1] - offsets[index - 2]).abs() < 0.01;
        let stable_after =
            index + 1 >= points.len() || (offsets[index + 1] - offsets[index]).abs() < 0.01;
        if jump >= 0.03 && stable_before && stable_after && jump > best_jump {
            best_jump = jump;
            best_index = Some(index);
        }
    }
    let Some(split) = best_index else {
        return Vec::new();
    };
    let (mut lower, mut upper) = (points[split - 1], points[split]);
    let (lower_offset, upper_offset) = (offsets[split - 1], offsets[split]);
    let mut refined = Vec::new();
    for _ in 0..6 {
        if upper.left - lower.left <= 1.0 {
            break;
        }
        let center = (lower.left + upper.left) / 2.0;
        let Some(o) = observe_one(
            m,
            left_dur,
            right_dur,
            center,
            1.1,
            left_source,
            right_source,
            provider,
        ) else {
            break;
        };
        if o.quality < 0.6 {
            break;
        }
        refined.push(o);
        let offset = o.right - m.rate * o.left;
        if (offset - lower_offset).abs() <= (offset - upper_offset).abs() {
            lower = o;
        } else {
            upper = o;
        }
    }
    refined
}

struct Piecewise {
    points: Vec<PairAlignmentPoint>,
    residual: f64,
}

/// Confirmed-jump fit: strictly monotone observations, sane segment rates,
/// ≥ 20 ms of proven offset spread, 3–8 simplified knots, residual ≤ 6 ms.
fn piecewise_fit(input: &[Observation]) -> Option<Piecewise> {
    let mut points = input.to_vec();
    points.sort_by(|a, b| a.left.total_cmp(&b.left));
    let mut deduped: Vec<Observation> = Vec::with_capacity(points.len());
    for p in points {
        match deduped.last_mut() {
            Some(last) if (last.left - p.left).abs() < 0.1 => {
                if p.quality > last.quality {
                    *last = p;
                }
            }
            _ => deduped.push(p),
        }
    }
    if deduped.len() < 5 {
        return None;
    }
    for pair in deduped.windows(2) {
        if !(pair[1].left > pair[0].left && pair[1].right > pair[0].right) {
            return None;
        }
        let rate = (pair[1].right - pair[0].right) / (pair[1].left - pair[0].left);
        if !(0.75..=1.25).contains(&rate) {
            return None;
        }
    }
    let offsets: Vec<f64> = deduped.iter().map(|p| p.right - p.left).collect();
    let (mut minimum, mut maximum) = (f64::INFINITY, f64::NEG_INFINITY);
    for o in offsets {
        minimum = minimum.min(o);
        maximum = maximum.max(o);
    }
    if maximum - minimum < 0.02 {
        return None;
    }
    let mapping = PiecewiseTimeMapping::with_tolerance(
        deduped
            .iter()
            .map(|p| crate::piecewise::MapPoint::new(p.left, p.right))
            .collect(),
        0.003,
    );
    if !(3..=8).contains(&mapping.points.len()) {
        return None;
    }
    let residual = (deduped
        .iter()
        .map(|p| {
            let e = p.right - mapping.value_at(p.left);
            e * e
        })
        .sum::<f64>()
        / deduped.len() as f64)
        .sqrt();
    if residual > 0.006 {
        return None;
    }
    Some(Piecewise {
        points: mapping
            .points
            .iter()
            .map(|p| PairAlignmentPoint {
                left: p.source,
                right: p.island,
            })
            .collect(),
        residual,
    })
}

struct Affine {
    rate: f64,
    offset: f64,
    residual: f64,
}

fn fit(points: &[Observation], allow_rate: bool) -> Affine {
    debug_assert!(!points.is_empty());
    let (rate, offset) = if allow_rate {
        let n = points.len() as f64;
        let mean_left = points.iter().map(|p| p.left).sum::<f64>() / n;
        let mean_right = points.iter().map(|p| p.right).sum::<f64>() / n;
        let den = points
            .iter()
            .map(|p| (p.left - mean_left).powi(2))
            .sum::<f64>();
        let rate = if den > 0.0 {
            points
                .iter()
                .map(|p| (p.left - mean_left) * (p.right - mean_right))
                .sum::<f64>()
                / den
        } else {
            1.0
        };
        (rate, mean_right - rate * mean_left)
    } else {
        let mut values: Vec<f64> = points.iter().map(|p| p.right - p.left).collect();
        values.sort_by(|a, b| a.total_cmp(b));
        (1.0, values[values.len() / 2])
    };
    let residual = (points
        .iter()
        .map(|p| (p.right - (rate * p.left + offset)).powi(2))
        .sum::<f64>()
        / points.len() as f64)
        .sqrt();
    Affine {
        rate,
        offset,
        residual,
    }
}

fn bounded_window_start(center: f64, duration: f64, clip_duration: f64) -> f64 {
    (center - duration / 2.0).clamp(0.0, (clip_duration - duration).max(0.0))
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

// ------------------------------------------------------------ disjoint set

struct FineDisjointSet {
    parent: HashMap<ClipId, ClipId>,
}

impl FineDisjointSet {
    fn new(clips: &[ClipId]) -> Self {
        Self {
            parent: clips.iter().map(|c| (c.clone(), c.clone())).collect(),
        }
    }

    fn find(&mut self, value: &ClipId) -> ClipId {
        let direct = self
            .parent
            .get(value)
            .cloned()
            .unwrap_or_else(|| value.clone());
        if direct == *value {
            return direct;
        }
        let root = self.find(&direct);
        self.parent.insert(value.clone(), root.clone());
        root
    }

    fn connected(&mut self, left: &ClipId, right: &ClipId) -> bool {
        self.find(left) == self.find(right)
    }

    /// Returns false when already united.
    fn union(&mut self, left: &ClipId, right: &ClipId) -> bool {
        let (lr, rr) = (self.find(left), self.find(right));
        if lr == rr {
            return false;
        }
        self.parent.insert(rr, lr);
        true
    }
}

// Re-exported for the future parallel scheduler (same indexed contract).
pub use crate::matcher::matched_ids;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MediaKind, MediaTime};

    fn id(s: &str) -> ClipId {
        ClipId::new(s)
    }

    /// LCG noise — bit-identical on all OSes, no RNG dependency.
    fn noise(n: usize, seed: u64) -> Vec<f32> {
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

    struct SynthProvider {
        clips: HashMap<ClipId, Vec<f32>>,
        sample_rate: f64,
    }

    impl WindowProvider for SynthProvider {
        fn window(
            &self,
            clip: &ClipId,
            start: f64,
            duration: f64,
            _source: AudioAnalysisSource,
        ) -> Option<AudioWindow> {
            let data = self.clips.get(clip)?;
            let s0 = (start.max(0.0) * self.sample_rate) as usize;
            let n = (duration * self.sample_rate) as usize;
            if s0 >= data.len() {
                return None;
            }
            let end = (s0 + n).min(data.len());
            Some(AudioWindow {
                start: s0 as f64 / self.sample_rate,
                sample_rate: self.sample_rate,
                samples: data[s0..end].to_vec(),
            })
        }
    }

    fn coarse(left: &str, right: &str, offset: f64, confidence: f64) -> PairwiseMatch {
        PairwiseMatch {
            left: id(left),
            right: id(right),
            rate: 1.0,
            offset,
            confidence,
            anchors: 20,
            left_start_seconds: 0.0,
            left_end_seconds: 120.0,
            covered_seconds: 120.0,
            residual_seconds: 0.005,
            alignment_points: vec![],
            evidence: MatchEvidence::Waveform,
        }
    }

    /// A delayed 130 s at 16 kHz; B lags A by 800 samples (50 ms).
    fn delayed_pair() -> (SynthProvider, HashMap<ClipId, f64>) {
        let sr = FINE_SAMPLE_RATE as usize;
        let a = noise(130 * sr, 0xC0FFEE);
        let mut b = vec![0.0; a.len()];
        b[800..].copy_from_slice(&a[..a.len() - 800]);
        let provider = SynthProvider {
            clips: [(id("a"), a), (id("b"), b)].into_iter().collect(),
            sample_rate: FINE_SAMPLE_RATE,
        };
        let durations = [(id("a"), 130.0), (id("b"), 130.0)].into_iter().collect();
        (provider, durations)
    }

    #[test]
    fn refines_to_sample_accuracy() {
        let (provider, durations) = delayed_pair();
        let m = coarse("a", "b", 800.0 / FINE_SAMPLE_RATE, 0.9);
        let r = refine(&m, 0.55, &durations, &HashMap::new(), &provider).expect("refined");
        // True offset 50 ms; expect ±2 samples (±125 µs).
        assert!(
            (r.offset - 0.05).abs() < 2.0 / FINE_SAMPLE_RATE,
            "off={}",
            r.offset
        );
        assert_eq!(r.rate, 1.0);
        assert_eq!(r.alignment_points.len(), 2);
        assert!(r.confidence >= 0.55 && r.confidence <= 0.999);
        assert!(r.residual_seconds >= 1.0 / FINE_SAMPLE_RATE);
    }

    #[test]
    fn refine_respects_requested_match_threshold() {
        let (provider, durations) = delayed_pair();
        let m = coarse("a", "b", 800.0 / FINE_SAMPLE_RATE, 0.5);
        assert!(refine(&m, 0.55, &durations, &HashMap::new(), &provider).is_some());
        assert!(refine(&m, 0.70, &durations, &HashMap::new(), &provider).is_none());
    }

    #[test]
    fn forest_accepts_chain_rejects_low_conf_cycle() {
        let sr = FINE_SAMPLE_RATE as usize;
        let a = noise(130 * sr, 11);
        let mut b = vec![0.0; a.len()];
        b[800..].copy_from_slice(&a[..a.len() - 800]);
        let mut c = vec![0.0; a.len()];
        c[1600..].copy_from_slice(&a[..a.len() - 1600]);
        let provider = SynthProvider {
            clips: [(id("a"), a), (id("b"), b), (id("c"), c)]
                .into_iter()
                .collect(),
            sample_rate: FINE_SAMPLE_RATE,
        };
        let durations: HashMap<ClipId, f64> =
            [(id("a"), 130.0), (id("b"), 130.0), (id("c"), 130.0)]
                .into_iter()
                .collect();
        let mut ab = coarse("a", "b", 800.0 / FINE_SAMPLE_RATE, 0.9);
        let mut bc = coarse("b", "c", 800.0 / FINE_SAMPLE_RATE, 0.9);
        let mut ac = coarse("a", "c", 1600.0 / FINE_SAMPLE_RATE, 0.5);
        for m in [&mut ab, &mut bc, &mut ac] {
            m.covered_seconds = 120.0;
        }
        let events = std::sync::Mutex::new(Vec::new());
        let report = |e: RefineEvent| {
            events
                .lock()
                .expect("test lock")
                .push((e.completed, e.total, e.stage));
        };
        let accepted = refine_forest(
            &durations,
            &[ab, bc, ac],
            &MatchPolicy::default(),
            &HashMap::new(),
            &provider,
            &std::sync::atomic::AtomicBool::new(false),
            Some(&report),
        );
        // a–b and b–c accepted; a–c closes the cycle below 0.85 → dropped.
        assert_eq!(accepted.len(), 2);
        assert!(
            accepted
                .iter()
                .any(|m| m.left == id("a") && m.right == id("b"))
        );
        assert!(
            accepted
                .iter()
                .any(|m| m.left == id("b") && m.right == id("c"))
        );
        let events = events.lock().expect("test lock");
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(_, t, _)| *t == 3));
        // Completion counts are monotonic despite parallel workers.
        let mut done: Vec<usize> = events.iter().map(|(c, _, _)| *c).collect();
        done.sort_unstable();
        assert_eq!(done, vec![1, 2, 3]);
    }

    #[test]
    fn parallel_refine_is_deterministic() {
        use std::sync::atomic::AtomicBool;
        let (provider, durations) = delayed_pair();
        let run = || {
            refine_forest(
                &durations,
                &[coarse("a", "b", 800.0 / FINE_SAMPLE_RATE, 0.9)],
                &MatchPolicy::default(),
                &HashMap::new(),
                &provider,
                &AtomicBool::new(false),
                None,
            )
        };
        let (a, b) = (run(), run());
        assert_eq!(a.len(), 1);
        assert!((a[0].offset - b[0].offset).abs() < 1e-12);
        assert!((a[0].confidence - b[0].confidence).abs() < 1e-12);
    }

    #[test]
    fn pre_cancelled_forest_returns_empty() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (provider, durations) = delayed_pair();
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let accepted = refine_forest(
            &durations,
            &[coarse("a", "b", 800.0 / FINE_SAMPLE_RATE, 0.9)],
            &MatchPolicy::default(),
            &HashMap::new(),
            &provider,
            &cancel,
            None,
        );
        assert!(accepted.is_empty());
    }

    #[test]
    fn unusable_candidates_reported_rejected() {
        let (provider, durations) = delayed_pair();
        let mut weak = coarse("a", "b", 0.05, 0.4); // below 0.45
        weak.covered_seconds = 120.0;
        let stages = std::sync::Mutex::new(Vec::new());
        let report = |e: RefineEvent| stages.lock().expect("test lock").push(e.stage);
        let accepted = refine_forest(
            &durations,
            &[weak],
            &MatchPolicy::default(),
            &HashMap::new(),
            &provider,
            &std::sync::atomic::AtomicBool::new(false),
            Some(&report),
        );
        assert!(accepted.is_empty());
        assert_eq!(
            *stages.lock().expect("test lock"),
            vec![RefineStage::Rejected]
        );
    }

    #[test]
    fn piecewise_gates_reject_garbage() {
        // Non-monotone.
        let bad: Vec<Observation> = [
            (0.0, 0.0, 1.0),
            (1.0, 0.5, 1.0),
            (2.0, 0.4, 1.0),
            (3.0, 0.6, 1.0),
            (4.0, 0.8, 1.0),
        ]
        .into_iter()
        .map(|(left, right, quality)| Observation {
            left,
            right,
            quality,
        })
        .collect();
        assert!(piecewise_fit(&bad).is_none());
        // Flat: no proven spread.
        let flat: Vec<Observation> = (0..6)
            .map(|k| Observation {
                left: k as f64,
                right: k as f64 + 0.001,
                quality: 1.0,
            })
            .collect();
        assert!(piecewise_fit(&flat).is_none());
        // Too few after dedupe.
        let few: Vec<Observation> = (0..3)
            .map(|k| Observation {
                left: k as f64 * 0.05,
                right: k as f64,
                quality: 1.0,
            })
            .collect();
        assert!(piecewise_fit(&few).is_none());
    }

    #[test]
    fn fit_pins_rate_without_span() {
        let obs: Vec<Observation> = [1.0, 1.1, 0.9, 1.05, 0.95]
            .into_iter()
            .enumerate()
            .map(|(k, o)| Observation {
                left: k as f64 * 10.0,
                right: k as f64 * 10.0 + o,
                quality: 1.0,
            })
            .collect();
        let f = fit(&obs, false);
        assert_eq!(f.rate, 1.0);
        assert!((f.offset - 1.0).abs() < 1e-12); // median of diffs
    }

    #[test]
    fn disjoint_set_unions_and_cycles() {
        let mut ds = FineDisjointSet::new(&[id("a"), id("b"), id("c")]);
        assert!(!ds.connected(&id("a"), &id("b")));
        assert!(ds.union(&id("a"), &id("b")));
        assert!(!ds.union(&id("a"), &id("b")));
        assert!(ds.connected(&id("a"), &id("b")));
        assert!(!ds.connected(&id("b"), &id("c")));
    }

    #[test]
    fn window_start_clamps_to_clip() {
        assert_eq!(bounded_window_start(5.0, 4.0, 10.0), 3.0);
        assert_eq!(bounded_window_start(1.0, 4.0, 10.0), 0.0);
        assert_eq!(bounded_window_start(9.5, 4.0, 10.0), 6.0);
        assert_eq!(bounded_window_start(5.0, 20.0, 10.0), 0.0);
    }

    #[test]
    fn media_time_seconds_roundtrip() {
        let t = MediaTime::seconds(6.4);
        assert!((t.as_seconds() - 6.4).abs() < 1e-9);
        let _ = MediaKind::Audio;
    }
}
