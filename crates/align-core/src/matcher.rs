//! Coarse waveform matching and pairwise alignment evidence.
//!
//! Matching stages:
//! 1. global inverted index over landmark hashes (groups > `max(48, 2.2×clips)`
//!    are ubiquitous-noise and skipped; singletons cannot vote);
//! 2. clip-run pairs vote into frame-delta histograms (8-frame buckets,
//!    winner needs ≥ 12 votes); normal peak selection scans without sorting;
//! 3. repeated-take guard: two near-equal distant peaks stay unmatched unless
//!    a trusted timecode/recording timestamp selects one (metadata never
//!    creates a match, it only disambiguates proven waveform peaks);
//!    a broad ambiguous ridge can instead be validated as a long clock-drift
//!    line, with independent coverage and competing-line rejection;
//! 4. a monotonic window recovers the last anchor around the winning bucket
//!    (±2), one per 40-frame bucket, preserving exhaustive traversal results;
//!    then robust affine fit + least-squares refine;
//! 5. confidence blend 0.25/0.20/0.25/0.20/0.10; drift rate reported only past
//!    600 s span with 50 anchors, otherwise exactly 1.0.
//!
//! Memory: fingerprint records, sparse vote histograms, and anchor maps.
//! Decoded audio is streamed through and never stored — see the audio-only
//! invariant in `align-decode/src/backend.rs`.

mod ridge;

use std::collections::{HashMap, HashSet};

use crate::fingerprint::{Fingerprint, HOP_SIZE, SAMPLE_RATE};
use crate::model::{Clip, ClipId, MatchEvidence, SyncConstraint, TemporalPolicy};

// ---------------------------------------------------------------- types

/// Fingerprints of one clip selected for matching (one audio source variant).
/// Taken by value and drained by the matcher.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipFingerprints {
    pub clip_id: ClipId,
    pub fingerprints: Vec<Fingerprint>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PairAlignmentPoint {
    pub left: f64,
    pub right: f64,
}

/// rightTime = rate × leftTime + offset.
#[derive(Clone, Debug, PartialEq)]
pub struct PairwiseMatch {
    pub left: ClipId,
    pub right: ClipId,
    pub rate: f64,
    pub offset: f64,
    pub confidence: f64,
    pub anchors: usize,
    pub left_start_seconds: f64,
    pub left_end_seconds: f64,
    pub covered_seconds: f64,
    pub residual_seconds: f64,
    pub alignment_points: Vec<PairAlignmentPoint>,
    pub evidence: MatchEvidence,
}

/// Trusted metadata hints. May only disambiguate proven waveform peaks —
/// never create a match on its own (mirrors `ClipTimingHints`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipTimingHints {
    pub recording_starts: HashMap<ClipId, f64>,
    pub source_timecodes: HashMap<ClipId, f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatchThreshold {
    Permissive,
    #[default]
    Balanced,
    Conservative,
}

impl MatchThreshold {
    pub fn minimum_confidence(self) -> f64 {
        match self {
            Self::Permissive => 0.45,
            Self::Balanced => policy::GRAPH_MIN_CONFIDENCE,
            Self::Conservative => 0.70,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchPolicy {
    pub default: MatchThreshold,
    pub thresholds: HashMap<ClipId, MatchThreshold>,
}

impl MatchPolicy {
    pub fn resolve(&self, id: &ClipId) -> MatchThreshold {
        self.thresholds.get(id).copied().unwrap_or(self.default)
    }

    pub fn minimum_confidence(&self, left: &ClipId, right: &ClipId) -> f64 {
        self.resolve(left)
            .minimum_confidence()
            .max(self.resolve(right).minimum_confidence())
    }
}

impl ClipTimingHints {
    /// Anchors resolved through the session's [`TemporalPolicy`]: Auto
    /// uses file timestamps and prefers timecode when both clips have it;
    /// other modes substitute exactly the selected evidence.
    pub fn from_clips(clips: &[Clip], policy: &TemporalPolicy) -> Self {
        let mut recording_starts = HashMap::new();
        let mut source_timecodes = HashMap::new();
        for clip in clips {
            let (recorded, timecode) = policy.anchors(clip);
            if let Some(t) = recorded {
                recording_starts.insert(clip.id.clone(), t);
            }
            if let Some(t) = timecode {
                source_timecodes.insert(clip.id.clone(), t);
            }
        }
        Self {
            recording_starts,
            source_timecodes,
        }
    }

    pub fn expected_offset(&self, left: &ClipId, right: &ClipId) -> Option<f64> {
        match (
            self.source_timecodes.get(left),
            self.source_timecodes.get(right),
        ) {
            (Some(l), Some(r)) => Some(l - r),
            _ => match (
                self.recording_starts.get(left),
                self.recording_starts.get(right),
            ) {
                (Some(l), Some(r)) => Some(l - r),
                _ => None,
            },
        }
    }
}

/// Fixed signal-quality gates shared by coarse matching and graph solving.
pub mod policy {
    /// Minimum histogram votes for a winning offset bucket.
    pub const VOTE_THRESHOLD: usize = 12;
    /// Runner-up vote level that triggers the repeated-take guard.
    pub const REPEATED_TAKE_VOTES: usize = 12;
    /// Near-equal peaks below this margin need a timing hint.
    pub const REPEATED_TAKE_MARGIN: f64 = 1.35;
    /// Graph admission: minimum confidence / anchors / span, max residual.
    pub const GRAPH_MIN_CONFIDENCE: f64 = 0.55;
    pub const GRAPH_MIN_ANCHORS: usize = 3;
    pub const GRAPH_MIN_COVERED_SECONDS: f64 = 3.0;
    pub const GRAPH_MAX_RESIDUAL_SECONDS: f64 = 0.1;
    /// Drift rate is only reported past this span with this many anchors.
    pub const DRIFT_MIN_SPAN_SECONDS: f64 = 600.0;
    pub const DRIFT_MIN_ANCHORS: usize = 50;
    /// IRLS iterations in the graph solve.
    pub const IRLS_ITERATIONS: usize = 4;
}

// ---------------------------------------------------------------- internals

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Record {
    hash: u64,
    clip: u32,
    frame: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PairKey {
    left: u32,
    right: u32,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    left: f64,
    right: f64,
}

const BUCKET_WIDTH_FRAMES: i64 = 8;
const ANCHOR_BUCKET_FRAMES: i64 = 40;
const SECONDS_PER_FRAME: f64 = HOP_SIZE as f64 / SAMPLE_RATE as f64;

fn bucket_offset(bucket: i64) -> f64 {
    (bucket * BUCKET_WIDTH_FRAMES) as f64 * SECONDS_PER_FRAME
}

struct Fit {
    rate: f64,
    offset: f64,
    residual: f64,
    inliers: usize,
    start: f64,
    end: f64,
    span: f64,
    points: Vec<Anchor>,
}

// ---------------------------------------------------------------- match

/// Coarse-match all clip pairs. Input is consumed (fingerprints are moved
/// into the inverted index, then dropped) to bound peak RAM.
pub fn match_fingerprints(
    clips: Vec<ClipFingerprints>,
    timing_hints: Option<&ClipTimingHints>,
    constraints: &[SyncConstraint],
) -> Vec<PairwiseMatch> {
    let mut clip_ids = Vec::with_capacity(clips.len());
    let mut records: Vec<Record> =
        Vec::with_capacity(clips.iter().map(|c| c.fingerprints.len()).sum());
    for (index, clip) in clips.into_iter().enumerate() {
        clip_ids.push(clip.clip_id);
        records.extend(clip.fingerprints.into_iter().map(|f| Record {
            hash: f.hash,
            frame: f.frame,
            clip: index as u32,
        }));
    }
    records.sort_unstable_by_key(|record| record.hash);

    let max_group_size = 48.max((clip_ids.len() as f64 * 2.2) as usize);
    let groups: Vec<(usize, usize)> = {
        let mut out = Vec::new();
        let mut start = 0;
        while start < records.len() {
            let mut end = start + 1;
            while end < records.len() && records[end].hash == records[start].hash {
                end += 1;
            }
            if end - start >= 2 && end - start <= max_group_size {
                records[start..end].sort_unstable_by_key(|record| (record.clip, record.frame));
                out.push((start, end));
            }
            start = end;
        }
        out
    };

    // Hash groups are ordered by clip, then frame. Look up each pair's
    // histogram once per group; occurrences from one clip cannot vote.
    let mut votes: HashMap<PairKey, HashMap<i64, usize>> = HashMap::new();
    for &(start, end) in &groups {
        for_each_clip_pair(&records[start..end], |key, left, right| {
            let histogram = votes.entry(key).or_default();
            for l in left {
                for r in right {
                    let delta = r.frame as i64 - l.frame as i64;
                    *histogram
                        .entry(delta.div_euclid(BUCKET_WIDTH_FRAMES))
                        .or_default() += 1;
                }
            }
        });
    }

    // Winners + repeated-take guard.
    let mut winning: HashMap<PairKey, (i64, usize, usize)> = HashMap::new();
    let mut drifting = HashMap::new();
    for (pair, histogram) in &votes {
        let left_id = &clip_ids[pair.left as usize];
        let right_id = &clip_ids[pair.right as usize];
        if SyncConstraint::rejects_pair(constraints, left_id, right_id) {
            continue;
        }
        let mut ranked: Vec<(i64, usize)> = histogram
            .iter()
            .filter(|(bucket, _)| {
                !SyncConstraint::rejects_alignment(
                    constraints,
                    left_id,
                    right_id,
                    bucket_offset(**bucket),
                )
            })
            .map(|(b, v)| (*b, *v))
            .collect();
        let Some(mut selected) = ranked
            .iter()
            .copied()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        else {
            continue;
        };
        if selected.1 < policy::VOTE_THRESHOLD {
            continue;
        }
        let mut runner_up = ranked
            .iter()
            .filter(|(b, _)| (*b - selected.0).abs() > 2)
            .map(|(_, v)| *v)
            .max()
            .unwrap_or(0);
        let margin = selected.1 as f64 / runner_up.max(1) as f64;
        if runner_up >= policy::REPEATED_TAKE_VOTES && margin < policy::REPEATED_TAKE_MARGIN {
            // Preserve the original tie order for metadata disambiguation.
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let hinted = timing_hints
                .and_then(|h| h.expected_offset(left_id, right_id))
                .and_then(|expected| hinted_peak(&ranked, expected));
            let Some(hinted) = hinted else {
                if let Some(candidate) = ridge::Candidate::new(&ranked) {
                    drifting.insert(*pair, candidate);
                }
                continue;
            };
            selected = hinted;
            runner_up = ranked
                .iter()
                .find(|(b, _)| (*b - selected.0).abs() > 2)
                .map(|(_, v)| *v)
                .unwrap_or(0);
        }
        winning.insert(*pair, (selected.0, selected.1, runner_up));
    }

    drop(votes);

    // Only the last eligible right occurrence survives each anchor write.
    // Its upper bound moves monotonically through the sorted right frames,
    // so recovering anchors takes O(left.len() + right.len()) per clip pair.
    let mut anchors: HashMap<PairKey, HashMap<i64, Anchor>> = HashMap::new();
    for &(start, end) in &groups {
        for_each_clip_pair(&records[start..end], |key, left, right| {
            if let Some(candidate) = drifting.get_mut(&key) {
                candidate.observe(left, right);
            }
            let Some(winner) = winning.get(&key) else {
                return;
            };
            let points = anchors.entry(key).or_default();
            let mut upper = 0;
            for l in left {
                let low = l.frame as i64 + (winner.0 - 2) * BUCKET_WIDTH_FRAMES;
                let high = l.frame as i64 + (winner.0 + 3) * BUCKET_WIDTH_FRAMES;
                while upper < right.len() && (right[upper].frame as i64) < high {
                    upper += 1;
                }
                if upper > 0 && right[upper - 1].frame as i64 >= low {
                    points.insert(
                        l.frame as i64 / ANCHOR_BUCKET_FRAMES,
                        Anchor {
                            left: l.frame as f64 * SECONDS_PER_FRAME,
                            right: right[upper - 1].frame as f64 * SECONDS_PER_FRAME,
                        },
                    );
                }
            }
        });
    }
    drop(records);

    let mut out: Vec<PairwiseMatch> = winning
        .iter()
        .filter_map(|(key, winner)| {
            let mut points: Vec<Anchor> = anchors
                .remove(key)
                .unwrap_or_default()
                .into_values()
                .collect();
            points.sort_by(|a, b| a.left.total_cmp(&b.left));
            let fit = robust_fit(&points)?;
            let margin = winner.1 as f64 / winner.2.max(1) as f64;
            Some(fitted_match(*key, fit, margin, &clip_ids))
        })
        .collect();
    out.extend(drifting.into_iter().filter_map(|(key, candidate)| {
        let (fit, margin) = candidate.fit()?;
        Some(fitted_match(key, fit, margin, &clip_ids))
    }));
    // Drift can move allowed histogram buckets back to a rejected affine
    // offset. Apply the user's constraint to the fitted result as well.
    out.retain(|m| !SyncConstraint::rejects_alignment(constraints, &m.left, &m.right, m.offset));
    // Total order: deterministic regardless of hash-map iteration order.
    out.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then_with(|| a.left.0.cmp(&b.left.0))
            .then_with(|| a.right.0.cmp(&b.right.0))
    });
    out
}

fn fitted_match(key: PairKey, fit: Fit, margin: f64, clip_ids: &[ClipId]) -> PairwiseMatch {
    let expected_buckets = 1.0f64.max(fit.span / (ANCHOR_BUCKET_FRAMES as f64 * SECONDS_PER_FRAME));
    let density = fit.inliers as f64 / expected_buckets;
    let confidence = 0.25 * (fit.inliers as f64 / 100.0).min(1.0)
        + 0.20 * (fit.span / 300.0).min(1.0)
        + 0.25 * (density / 0.35).min(1.0)
        + 0.20 * ((0.12 - fit.residual) / 0.10).clamp(0.0, 1.0)
        + 0.10 * ((margin - 1.0) / 2.0).clamp(0.0, 1.0);
    let rate =
        if fit.span >= policy::DRIFT_MIN_SPAN_SECONDS && fit.inliers >= policy::DRIFT_MIN_ANCHORS {
            fit.rate
        } else {
            1.0
        };
    PairwiseMatch {
        left: clip_ids[key.left as usize].clone(),
        right: clip_ids[key.right as usize].clone(),
        rate,
        offset: fit.offset,
        confidence,
        anchors: fit.inliers,
        left_start_seconds: fit.start,
        left_end_seconds: fit.end,
        covered_seconds: fit.span,
        residual_seconds: fit.residual,
        alignment_points: fit
            .points
            .iter()
            .map(|p| PairAlignmentPoint {
                left: p.left,
                right: p.right,
            })
            .collect(),
        evidence: MatchEvidence::Waveform,
    }
}

/// Visit distinct clip runs in a hash group, preserving frame order.
fn for_each_clip_pair(records: &[Record], mut visit: impl FnMut(PairKey, &[Record], &[Record])) {
    let mut remaining = records;
    while let Some(first) = remaining.first() {
        let end = remaining
            .iter()
            .position(|r| r.clip != first.clip)
            .unwrap_or(remaining.len());
        let (left, rest) = remaining.split_at(end);
        for right in rest.chunk_by(|a, b| a.clip == b.clip) {
            visit(
                PairKey {
                    left: first.clip,
                    right: right[0].clip,
                },
                left,
                right,
            );
        }
        remaining = rest;
    }
}

/// Nearest local-maximum peak to the trusted offset: within 2 buckets and at
/// least 5 (bucket-offset units) ahead of the next candidate.
fn hinted_peak(ranked: &[(i64, usize)], expected_offset: f64) -> Option<(i64, usize)> {
    let lookup: HashMap<i64, usize> = ranked.iter().copied().collect();
    let mut peaks: Vec<(i64, usize)> = ranked
        .iter()
        .copied()
        .filter(|(bucket, votes)| {
            if *votes < policy::VOTE_THRESHOLD {
                return false;
            }
            for neighbor in (*bucket - 2)..=(*bucket + 2) {
                if neighbor != *bucket && lookup.get(&neighbor).is_some_and(|v| *v > *votes) {
                    return false;
                }
            }
            true
        })
        .collect();
    peaks.sort_by(|a, b| {
        let le = (bucket_offset(a.0) - expected_offset).abs();
        let re = (bucket_offset(b.0) - expected_offset).abs();
        le.total_cmp(&re).then_with(|| b.1.cmp(&a.1))
    });
    let nearest = *peaks.first()?;
    let nearest_error = (bucket_offset(nearest.0) - expected_offset).abs();
    let next_error = peaks
        .iter()
        .skip(1)
        .map(|(b, _)| (bucket_offset(*b) - expected_offset).abs())
        .fold(f64::INFINITY, f64::min);
    if nearest_error <= 2.0 && next_error - nearest_error >= 5.0 {
        Some(nearest)
    } else {
        None
    }
}

fn robust_fit(points: &[Anchor]) -> Option<Fit> {
    if points.len() < 3 {
        return None;
    }
    let (first, last) = (points[0], points[points.len() - 1]);
    if last.left - first.left < 3.0 {
        return None;
    }
    let edge_count = (points.len() / 4).clamp(1, 24);
    let mut slopes: Vec<f64> = Vec::new();
    for left in points.iter().take(edge_count) {
        for right in points.iter().rev().take(edge_count) {
            if right.left - left.left >= 3.0 {
                let slope = (right.right - left.right) / (right.left - left.left);
                if (0.98..=1.02).contains(&slope) {
                    slopes.push(slope);
                }
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(|a, b| a.total_cmp(b));
    let mut rate = slopes[slopes.len() / 2];
    let mut offsets: Vec<f64> = points.iter().map(|p| p.right - rate * p.left).collect();
    offsets.sort_by(|a, b| a.total_cmp(b));
    let mut offset = offsets[offsets.len() / 2];

    let mut inliers: Vec<Anchor> = points
        .iter()
        .copied()
        .filter(|p| (p.right - (rate * p.left + offset)).abs() <= 0.16)
        .collect();
    if inliers.len() < 3 {
        return None;
    }
    let n = inliers.len() as f64;
    let mean_left = inliers.iter().map(|p| p.left).sum::<f64>() / n;
    let mean_right = inliers.iter().map(|p| p.right).sum::<f64>() / n;
    let (num, den) = inliers.iter().fold((0.0, 0.0), |(num, den), p| {
        (
            num + (p.left - mean_left) * (p.right - mean_right),
            den + (p.left - mean_left) * (p.left - mean_left),
        )
    });
    if den <= 0.0 {
        return None;
    }
    rate = num / den;
    offset = mean_right - rate * mean_left;
    inliers.retain(|p| (p.right - (rate * p.left + offset)).abs() <= 0.16);
    if inliers.len() < 3 {
        return None;
    }
    let residual = (inliers
        .iter()
        .map(|p| {
            let e = p.right - (rate * p.left + offset);
            e * e
        })
        .sum::<f64>()
        / inliers.len() as f64)
        .sqrt();
    let (start, end) = (inliers[0].left, inliers[inliers.len() - 1].left);
    Some(Fit {
        rate,
        offset,
        residual,
        inliers: inliers.len(),
        start,
        end,
        span: end - start,
        points: inliers,
    })
}

/// Clip IDs touched by `matches` (helper for graph/solve callers).
pub fn matched_ids(matches: &[PairwiseMatch]) -> HashSet<ClipId> {
    matches
        .iter()
        .flat_map(|m| [m.left.clone(), m.right.clone()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecordingTimestampSource;

    fn id(s: &str) -> ClipId {
        ClipId::new(s)
    }

    /// Two clips sharing `n` landmarks at a fixed frame delta. Anchor frames
    /// sit on distinct 40-frame buckets so every landmark becomes an anchor.
    fn synth_pair(
        a: &str,
        b: &str,
        delta_frames: i64,
        n: usize,
        salt: u64,
    ) -> Vec<ClipFingerprints> {
        let mut fa = Vec::with_capacity(n);
        let mut fb = Vec::with_capacity(n);
        for k in 0..n {
            let hash = ((k as u64) + 1) << 32 | salt;
            let frame_a = k as u32 * 40;
            fa.push(Fingerprint {
                hash,
                frame: frame_a,
            });
            fb.push(Fingerprint {
                hash,
                frame: frame_a.wrapping_add(delta_frames as u32),
            });
        }
        vec![
            ClipFingerprints {
                clip_id: id(a),
                fingerprints: fa,
            },
            ClipFingerprints {
                clip_id: id(b),
                fingerprints: fb,
            },
        ]
    }

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn finds_known_offset() {
        let matches = match_fingerprints(synth_pair("a", "b", 100, 20, 0xBEEF), None, &[]);
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.left, id("a"));
        assert_eq!(m.right, id("b"));
        // 100 frames × 64 ms.
        assert!(approx(m.offset, 6.4, 1e-9), "offset={}", m.offset);
        assert_eq!(m.rate, 1.0); // span < 600 s: rate pinned to 1.
        assert_eq!(m.anchors, 20);
        assert!(approx(m.covered_seconds, 19.0 * 40.0 * 0.064, 1e-9));
        assert!(m.residual_seconds < 1e-9);
        assert!(approx(m.confidence, 0.63, 0.05), "conf={}", m.confidence);
        assert_eq!(m.evidence, MatchEvidence::Waveform);
    }

    #[test]
    fn repeated_landmarks_keep_last_in_window_anchor_for_both_offset_signs() {
        for delta in [-100i64, 100] {
            let mut clips = vec![
                ClipFingerprints {
                    clip_id: id("a"),
                    fingerprints: Vec::new(),
                },
                ClipFingerprints {
                    clip_id: id("b"),
                    fingerprints: Vec::new(),
                },
            ];
            for k in 0..20u32 {
                let frame = 1000 + k * 40;
                let hash = k as u64;
                for shift in [0, 1] {
                    clips[0].fingerprints.push(Fingerprint {
                        hash,
                        frame: frame + shift,
                    });
                }
                // The distant occurrence must not replace an in-window anchor.
                for shift in [0, 1, 2, 800] {
                    clips[1].fingerprints.push(Fingerprint {
                        hash,
                        frame: (frame as i64 + delta + shift) as u32,
                    });
                }
            }
            // Input order must not affect which occurrence survives.
            for clip in &mut clips {
                clip.fingerprints.reverse();
            }
            let matches = match_fingerprints(clips, None, &[]);
            assert_eq!(matches.len(), 1);
            let matched = &matches[0];
            assert_eq!(matched.anchors, 20);
            assert!(approx(matched.offset, (delta + 1) as f64 * 0.064, 1e-9));
            for (k, point) in matched.alignment_points.iter().enumerate() {
                let left = (1001 + k * 40) as f64 * 0.064;
                assert!(approx(point.left, left, 1e-9));
                assert!(approx(point.right, left + (delta + 1) as f64 * 0.064, 1e-9));
            }
        }
    }

    fn drift_pair(rate: f64, copies: usize) -> Vec<ClipFingerprints> {
        let mut clips = vec![
            ClipFingerprints {
                clip_id: id("a"),
                fingerprints: Vec::new(),
            },
            ClipFingerprints {
                clip_id: id("b"),
                fingerprints: Vec::new(),
            },
        ];
        for k in 0..1200u32 {
            let frame = 1000 + k * 40;
            clips[0].fingerprints.push(Fingerprint {
                hash: k as u64,
                frame,
            });
            for copy in 0..copies {
                clips[1].fingerprints.push(Fingerprint {
                    hash: k as u64,
                    frame: (frame as f64 * rate + 104.0 + copy as f64 * 800.0).round() as u32,
                });
            }
        }
        clips
    }

    #[test]
    fn long_drift_ridge_recovers_both_clock_directions_but_not_repeated_takes() {
        for rate in [0.999, 1.001] {
            let clips = drift_pair(rate, 1);
            let found = match_fingerprints(clips.clone(), None, &[]);
            assert_eq!(found.len(), 1, "rate={rate}");
            let m = &found[0];
            assert!(approx(m.rate, rate, 0.000002), "{}", m.rate);
            assert!(approx(m.offset, 104.0 * 0.064, 0.01), "{}", m.offset);
            assert!(m.anchors >= 1000 && m.covered_seconds > 3000.0);
            assert_eq!(found, match_fingerprints(clips.clone(), None, &[]));
            let mut damaged = clips.clone();
            damaged[1].fingerprints.retain(|p| p.hash % 3 != 0);
            for p in &mut damaged[1].fingerprints {
                p.frame += (p.hash % 3) as u32;
            }
            let recovered = match_fingerprints(damaged, None, &[]);
            assert_eq!(recovered.len(), 1);
            assert!(approx(recovered[0].rate, rate, 0.000002));

            assert!(match_fingerprints(drift_pair(rate, 2), None, &[]).is_empty());
            let mut partial_repeat = drift_pair(rate, 2);
            partial_repeat[1].fingerprints = partial_repeat[1]
                .fingerprints
                .iter()
                .enumerate()
                .filter(|(i, _)| i % 2 == 0 || i / 2 < 960)
                .map(|(_, p)| *p)
                .collect();
            assert!(match_fingerprints(partial_repeat, None, &[]).is_empty());
            for constraint in [
                SyncConstraint::rejecting_pair(id("b"), id("a")),
                SyncConstraint::rejecting_alignment(id("b"), id("a"), -104.0 * 0.064),
            ] {
                assert!(match_fingerprints(clips.clone(), None, &[constraint]).is_empty());
            }
        }
    }

    #[test]
    fn diffuse_offset_histogram_without_a_clock_line_is_rejected() {
        for seed in 0..16u64 {
            let mut random = seed + 1;
            let mut clips = drift_pair(1.001, 1);
            for p in &mut clips[1].fingerprints {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                p.frame = 1000 + p.hash as u32 * 40 + 104 + ((random >> 32) % 56) as u32;
            }
            assert!(
                match_fingerprints(clips, None, &[]).is_empty(),
                "seed={seed}"
            );
        }
    }

    #[test]
    fn deterministic_across_runs() {
        let run = || match_fingerprints(synth_pair("a", "b", 100, 20, 7), None, &[]);
        assert_eq!(run(), run());
    }

    #[test]
    fn too_few_votes_rejected() {
        let matches = match_fingerprints(synth_pair("a", "b", 100, 5, 1), None, &[]);
        assert!(matches.is_empty());
    }

    #[test]
    fn rejected_pair_stays_unmatched() {
        let clips = synth_pair("a", "b", 100, 20, 1);
        let constraints = vec![SyncConstraint::rejecting_pair(id("b"), id("a"))];
        assert!(match_fingerprints(clips, None, &constraints).is_empty());
    }

    #[test]
    fn rejected_alignment_drops_bucket() {
        let clips = synth_pair("a", "b", 100, 20, 1);
        // Winning bucket center: 12 × 8 × 0.064 = 6.144 s.
        let constraints = vec![SyncConstraint::rejecting_alignment(id("a"), id("b"), 6.144)];
        assert!(match_fingerprints(clips, None, &constraints).is_empty());
    }

    #[test]
    fn competing_peaks_need_hint_then_resolve() {
        // Two near-equal distant hypotheses: buckets 12 and 42, 20 votes each.
        let mut clips = synth_pair("a", "b", 100, 20, 0xA);
        let extra = synth_pair("a", "b", 100 + 30 * 8, 20, 0xB);
        clips[0]
            .fingerprints
            .extend(extra[0].fingerprints.iter().copied());
        clips[1]
            .fingerprints
            .extend(extra[1].fingerprints.iter().copied());

        // Without metadata: ambiguous, stays unmatched (no guessing takes).
        assert!(match_fingerprints(clips.clone(), None, &[]).is_empty());

        // With a trusted timecode hint at the first hypothesis: resolved.
        let hints = ClipTimingHints {
            recording_starts: HashMap::new(),
            source_timecodes: [(id("a"), 0.0), (id("b"), -6.4)].into_iter().collect(),
        };
        let matches = match_fingerprints(clips, Some(&hints), &[]);
        assert_eq!(matches.len(), 1);
        assert!(
            approx(matches[0].offset, 6.4, 1e-9),
            "offset={}",
            matches[0].offset
        );
    }

    #[test]
    fn hints_from_clips_prefer_timecode_then_use_filesystem() {
        use std::path::PathBuf;
        let mk = |ident: &str, tc: Option<f64>, fs: bool| Clip {
            id: ClipId::new(ident),
            url: PathBuf::from(format!("/tmp/{ident}.wav")),
            kind: crate::model::MediaKind::Audio,
            duration: crate::model::MediaTime::seconds(60.0),
            audio: vec![crate::model::AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: tc.map(|s| crate::model::SourceTimecode {
                    text: "00:00:00:00".into(),
                    frame_number: (s * 25.0).round() as i64,
                    frame_duration: crate::model::MediaTime::new(1, 25),
                    drop_frame: false,
                }),
            }],
            video: None,
            recorded_at: Some(1_700_000_000),
            recorded_at_source: Some(if fs {
                RecordingTimestampSource::FileSystem
            } else {
                RecordingTimestampSource::EmbeddedMetadata
            }),
            source_identifier: None,
            media_span: None,
        };
        let a = mk("a", Some(10.0), false);
        let b = mk("b", Some(4.0), false);
        let hints = ClipTimingHints::from_clips(&[a, b], &TemporalPolicy::default());
        assert!(approx(
            hints.expected_offset(&id("a"), &id("b")).unwrap(),
            6.0,
            1e-9
        ));
        // Without timecode, Auto uses the file timestamps as REC START.
        let c = mk("c", None, true);
        let d = mk("d", None, true);
        let hints2 = ClipTimingHints::from_clips(&[c, d], &TemporalPolicy::default());
        assert!(approx(
            hints2.expected_offset(&id("c"), &id("d")).unwrap(),
            0.0,
            1e-9
        ));
    }

    #[test]
    fn temporal_policy_selects_hint_evidence() {
        use crate::model::{MediaKind, MediaTime, TemporalMode};
        use std::path::PathBuf;
        let mk = |ident: &str, recorded: i64, dur: f64| Clip {
            id: ClipId::new(ident),
            url: PathBuf::from(format!("/tmp/{ident}.wav")),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(dur),
            audio: vec![crate::model::AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: Some(crate::model::SourceTimecode {
                    text: "00:00:00:00".into(),
                    frame_number: 2500,
                    frame_duration: MediaTime::new(1, 25),
                    drop_frame: false,
                }),
            }],
            video: None,
            recorded_at: Some(recorded),
            recorded_at_source: Some(RecordingTimestampSource::EmbeddedMetadata),
            source_identifier: None,
            media_span: None,
        };
        // Same timecode (offset 0), different stamps: Auto follows timecode.
        let clips = vec![mk("a", 1000, 60.0), mk("b", 1120, 60.0)];
        let auto = ClipTimingHints::from_clips(&clips, &TemporalPolicy::default());
        assert!(approx(
            auto.expected_offset(&id("a"), &id("b")).unwrap(),
            0.0,
            1e-9
        ));
        // REC START follows stamps (1000 vs 1120 → −120).
        let start = TemporalPolicy {
            default: TemporalMode::RecStart,
            modes: HashMap::new(),
        };
        let hints = ClipTimingHints::from_clips(&clips, &start);
        assert!(approx(
            hints.expected_offset(&id("a"), &id("b")).unwrap(),
            -120.0,
            1e-9
        ));
        // REC STOP subtracts exact durations (940 vs 1060 → −120).
        let stop = TemporalPolicy {
            default: TemporalMode::RecStop,
            modes: HashMap::new(),
        };
        let hints = ClipTimingHints::from_clips(&clips, &stop);
        assert!(approx(
            hints.expected_offset(&id("a"), &id("b")).unwrap(),
            -120.0,
            1e-9
        ));
        // Mixed modes with no shared evidence: no hint, waveform decides.
        let mixed = TemporalPolicy {
            default: TemporalMode::Auto,
            modes: [
                (id("a"), TemporalMode::RecStart),
                (id("b"), TemporalMode::Timecode),
            ]
            .into_iter()
            .collect(),
        };
        let hints = ClipTimingHints::from_clips(&clips, &mixed);
        assert!(hints.expected_offset(&id("a"), &id("b")).is_none());
    }
}
