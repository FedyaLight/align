use super::{
    AudioAnalysisSource, ClipId, FINE_SAMPLE_RATE, MatchEvidence, MatchPolicy, Pair,
    PairAlignmentPoint, PairwiseMatch, WindowProvider, fit,
};
use std::collections::HashMap;

// Search for a whole short take in a longer recording. Exclude the first hop
// (the extractor starts with an empty previous spectrum). Pearson correlation
// removes differences in flux level between microphones.
fn unique_flux_peak(query: &[u8], reference: &[u8]) -> Option<f64> {
    let query = query.get(1..)?;
    let n = query.len() as f64;
    let sum = query.iter().map(|&v| f64::from(v)).sum::<f64>();
    let energy = query.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() - sum * sum / n;
    if energy < 100. {
        return None;
    }
    let mut scores = Vec::with_capacity(reference.len() - query.len() + 1);
    let mut best = (0, 0.);
    for (i, window) in reference.windows(query.len()).enumerate() {
        let (mut ys, mut yy, mut xy) = (0u32, 0u32, 0u32);
        for (&a, &b) in query.iter().zip(window) {
            let b = u32::from(b);
            ys += b;
            yy += b * b;
            xy += u32::from(a) * b;
        }
        let ys = f64::from(ys);
        let variance = f64::from(yy) - ys * ys / n;
        let score = if variance > 100. {
            (f64::from(xy) - sum * ys / n) / (energy * variance).sqrt()
        } else {
            0.
        };
        if score > best.1 {
            best = (i, score);
        }
        scores.push(score);
    }
    (best.1 >= 0.55
        && scores
            .iter()
            .enumerate()
            .all(|(i, &s)| i.abs_diff(best.0) <= 2 || s + 0.10 < best.1))
    .then_some((best.0 as f64 - 1.) * 0.064)
}

/// Rescue only whole short Balanced queries, after landmark refinement. Three
/// disjoint waveform observations must support one offset across the take.
/// Longer or partial overlaps remain the responsibility of the landmark path.
pub fn recover_short_takes(
    envelopes: &[(ClipId, Vec<u8>)],
    refined: &[PairwiseMatch],
    constraints: &[crate::SyncConstraint],
    policy: &MatchPolicy,
    sources: &HashMap<ClipId, AudioAnalysisSource>,
    provider: &dyn WindowProvider,
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<PairwiseMatch> {
    let mut placed = crate::matcher::matched_ids(refined);
    let mut recovered = Vec::new();
    let mut budget = 100_000_000usize;
    for (id, query) in envelopes {
        if placed.contains(id) || !(110..=500).contains(&query.len()) {
            continue;
        }
        for (other, reference) in envelopes {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return recovered;
            }
            if other == id
                || reference.len() < query.len()
                || crate::SyncConstraint::rejects_pair(constraints, id, other)
            {
                continue;
            }
            let cost = query
                .len()
                .saturating_mul(reference.len() - query.len() + 2);
            if cost > budget {
                continue;
            }
            budget -= cost;
            let Some(offset) = unique_flux_peak(query, reference) else {
                continue;
            };
            if crate::SyncConstraint::rejects_alignment(constraints, id, other, offset) {
                continue;
            }
            let span = (query.len() - 1) as f64 * 0.064;
            let mut m = PairwiseMatch {
                left: id.clone(),
                right: other.clone(),
                rate: 1.,
                offset,
                confidence: 0.,
                anchors: 0,
                left_start_seconds: 0.,
                left_end_seconds: span,
                covered_seconds: span,
                residual_seconds: 0.,
                alignment_points: Vec::new(),
                evidence: MatchEvidence::Waveform,
            };
            let pair = Pair {
                m: &m,
                left_duration: span,
                right_duration: (reference.len() - 1) as f64 * 0.064,
                left_source: sources.get(id).copied().unwrap_or_default(),
                right_source: sources.get(other).copied().unwrap_or_default(),
                provider,
            };
            let obs = pair.observe(&[0.16, 0.5, 0.84], 2.1);
            if obs.len() != 3
                || obs.iter().any(|o| o.quality < 0.65)
                || obs
                    .windows(2)
                    .any(|w| w[1].left - w[0].left < 2.1 || w[1].right - w[0].right < 2.1)
            {
                continue;
            }
            let fit = fit(&obs, false);
            if obs
                .iter()
                .any(|o| (o.right - o.left - fit.offset).abs() > 0.002)
                || crate::SyncConstraint::rejects_alignment(constraints, id, other, fit.offset)
            {
                continue;
            }
            m.confidence = obs.iter().map(|o| o.quality).fold(0.95, f64::min);
            if m.confidence < policy.minimum_confidence(id, other) {
                continue;
            }
            m.offset = fit.offset;
            m.anchors = obs.len();
            m.left_start_seconds = obs[0].left;
            m.left_end_seconds = obs[2].left;
            m.covered_seconds = m.left_end_seconds - m.left_start_seconds;
            m.residual_seconds = fit.residual.max(1. / FINE_SAMPLE_RATE);
            m.alignment_points = [m.left_start_seconds, m.left_end_seconds]
                .map(|left| PairAlignmentPoint {
                    left,
                    right: left + m.offset,
                })
                .to_vec();
            recovered.push(m);
            placed.insert(id.clone());
            placed.insert(other.clone());
            break;
        }
    }
    recovered
}
