//! Timecode-overlap matching.
//!
//! Clips carrying a trusted source timecode but no waveform/spanned match
//! join by timecode overlap: each unmatched timecoded clip pairs with the
//! maximum-overlap partner — an already-matched anchor when one exists,
//! else the best fellow unmatched clip. Recording dates within 16 h gate
//! the pairing; metadata picks at most one partner per clip and never
//! overrides a rejected pair/alignment.

use std::collections::HashSet;

use crate::matcher::PairwiseMatch;
use crate::model::{Clip, ClipId, MatchEvidence, SyncConstraint, TemporalPolicy};

/// Clips with timecode but no existing match pair with maximum overlap.
pub fn analyze_timecodes(
    clips: &[Clip],
    existing_matches: &[PairwiseMatch],
    constraints: &[SyncConstraint],
    policy: &TemporalPolicy,
) -> Vec<PairwiseMatch> {
    let matched_ids: HashSet<&ClipId> = existing_matches
        .iter()
        .flat_map(|m| [&m.left, &m.right])
        .collect();
    let timecoded: Vec<&Clip> = clips
        .iter()
        .filter(|clip| policy.anchors(clip).1.is_some())
        .collect();
    if timecoded.is_empty() {
        return Vec::new();
    }
    let unmatched: Vec<&Clip> = timecoded
        .iter()
        .filter(|c| !matched_ids.contains(&c.id))
        .copied()
        .collect();
    if unmatched.is_empty() {
        return Vec::new();
    }
    let anchors: Vec<&Clip> = timecoded
        .iter()
        .filter(|c| matched_ids.contains(&c.id))
        .copied()
        .collect();
    let pool: Vec<&Clip> = anchors.iter().chain(unmatched.iter()).copied().collect();

    let mut new_matches = Vec::new();
    let mut matched_pairs: HashSet<String> = HashSet::new();

    for candidate in unmatched {
        let Some(candidate_start) = policy.anchors(candidate).1 else {
            continue;
        };
        let candidate_end = candidate_start + candidate.duration.as_seconds();

        let mut best: Option<(&Clip, f64, f64)> = None; // (target, target_start, overlap)
        for target in &pool {
            if target.id == candidate.id
                || SyncConstraint::rejects_pair(constraints, &target.id, &candidate.id)
            {
                continue;
            }
            let Some(target_start) = policy.anchors(target).1 else {
                continue;
            };
            let key = pair_key(&candidate.id, &target.id);
            if matched_pairs.contains(&key) {
                continue;
            }
            if let (Some(cd), Some(td)) = (candidate.recorded_at, target.recorded_at) {
                if (cd - td).abs() >= 57_600 {
                    continue;
                }
            }
            let target_end = target_start + target.duration.as_seconds();
            let overlap =
                overlap_start_end(candidate_start, candidate_end, target_start, target_end);
            let target_is_anchor = matched_ids.contains(&target.id);
            if overlap > 0.0
                && best.is_none_or(|(previous, _, b)| {
                    let previous_is_anchor = matched_ids.contains(&previous.id);
                    (target_is_anchor && !previous_is_anchor)
                        || (target_is_anchor == previous_is_anchor && overlap > b)
                })
            {
                best = Some((target, target_start, overlap));
            }
        }

        let Some((target, target_tc, overlap)) = best else {
            continue;
        };
        let key = pair_key(&candidate.id, &target.id);
        if matched_pairs.contains(&key) {
            continue;
        }
        matched_pairs.insert(key);

        let candidate_tc = candidate_start;
        // Prefer anchored targets on the left, otherwise use clip-ID order.
        let anchor_target = matched_ids.contains(&target.id);
        let (left, left_tc, right, right_tc) = if anchor_target || target.id.0 < candidate.id.0 {
            (target, target_tc, candidate, candidate_tc)
        } else {
            (candidate, candidate_tc, target, target_tc)
        };
        let offset = left_tc - right_tc;
        if SyncConstraint::rejects_alignment(constraints, &left.id, &right.id, offset) {
            continue;
        }
        new_matches.push(PairwiseMatch {
            left: left.id.clone(),
            right: right.id.clone(),
            rate: 1.0,
            offset,
            confidence: 0.95,
            anchors: 1,
            left_start_seconds: 0.0f64.max(right_tc - left_tc),
            left_end_seconds: (left.duration.as_seconds())
                .min(right_tc + right.duration.as_seconds() - left_tc),
            covered_seconds: overlap,
            residual_seconds: 0.0,
            alignment_points: Vec::new(),
            evidence: MatchEvidence::Timecode,
        });
    }
    new_matches
}

fn pair_key(a: &ClipId, b: &ClipId) -> String {
    if a.0 < b.0 {
        format!("{}-{}", a.0, b.0)
    } else {
        format!("{}-{}", b.0, a.0)
    }
}

fn overlap_start_end(c0: f64, c1: f64, t0: f64, t1: f64) -> f64 {
    // max(0, min ends − max starts).
    (c1.min(t1) - c0.max(t0)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioSummary, MediaKind, MediaTime, SourceTimecode, TemporalMode};
    use std::path::PathBuf;

    fn clip(id: &str, tc_start: Option<f64>, seconds: f64, recorded: Option<i64>) -> Clip {
        Clip {
            id: ClipId::new(id),
            url: PathBuf::from(format!("/v/{id}.mov")),
            kind: MediaKind::Video,
            duration: MediaTime::seconds(seconds),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 2,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: Some(crate::model::VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: tc_start.map(|s| SourceTimecode {
                    text: "tc".into(),
                    frame_number: (s * 25.0) as i64,
                    frame_duration: MediaTime::new(1, 25),
                    drop_frame: false,
                }),
                frame_rate_mode: None,
            }),
            recorded_at: recorded,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        }
    }

    #[test]
    fn overlapping_timecodes_pair() {
        // A[100..160], B[120..180] → overlap 40, offset A−B = −20.
        let clips = vec![
            clip("a", Some(100.0), 60.0, None),
            clip("b", Some(120.0), 60.0, None),
        ];
        let matches = analyze_timecodes(&clips, &[], &[], &TemporalPolicy::default());
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.evidence, MatchEvidence::Timecode);
        assert!((m.offset + 20.0).abs() < 1e-9, "offset={}", m.offset);
        assert!((m.covered_seconds - 40.0).abs() < 1e-9);
        assert_eq!(m.confidence, 0.95);
    }

    #[test]
    fn unrelated_matched_group_does_not_block_timecode_pair() {
        let clips = vec![
            clip("a", Some(100.0), 60.0, None),
            clip("b", Some(110.0), 60.0, None),
            clip("c", Some(500.0), 60.0, None),
            clip("d", Some(510.0), 60.0, None),
        ];
        let existing = analyze_timecodes(&clips[..2], &[], &[], &TemporalPolicy::default());
        let rescued = analyze_timecodes(&clips, &existing, &[], &TemporalPolicy::default());
        assert_eq!(rescued.len(), 1);
        assert_eq!(rescued[0].left, ClipId::new("c"));
        assert_eq!(rescued[0].right, ClipId::new("d"));
    }

    #[test]
    fn disjoint_timecodes_stay_unmatched() {
        let clips = vec![
            clip("a", Some(0.0), 10.0, None),
            clip("b", Some(100.0), 10.0, None),
        ];
        assert!(analyze_timecodes(&clips, &[], &[], &TemporalPolicy::default()).is_empty());
    }

    #[test]
    fn conflicting_timecode_never_overrides_waveform() {
        use crate::matcher::PairAlignmentPoint;
        // Waveform says a−b offset is +5 s; overlapping timecodes imply −20 s.
        // Timecode is hint/rescue only: the matched pair is left alone and
        // no second edge is minted for it.
        let clips = vec![
            clip("a", Some(100.0), 60.0, None),
            clip("b", Some(120.0), 60.0, None),
        ];
        let existing = vec![PairwiseMatch {
            left: ClipId::new("a"),
            right: ClipId::new("b"),
            rate: 1.0,
            offset: 5.0,
            confidence: 0.9,
            anchors: 10,
            left_start_seconds: 0.0,
            left_end_seconds: 60.0,
            covered_seconds: 60.0,
            residual_seconds: 0.01,
            alignment_points: Vec::<PairAlignmentPoint>::new(),
            evidence: MatchEvidence::Waveform,
        }];
        assert!(analyze_timecodes(&clips, &existing, &[], &TemporalPolicy::default()).is_empty());
    }

    #[test]
    fn midnight_wrap_without_dates_stays_unmatched() {
        // A[86390..86450] and B[10..70]: same wall-clock footage on
        // consecutive days would overlap, but without recording dates the
        // 24 h gap is unprovable — no guessing across the wrap.
        let clips = vec![
            clip("a", Some(86390.0), 60.0, None),
            clip("b", Some(10.0), 60.0, None),
        ];
        assert!(analyze_timecodes(&clips, &[], &[], &TemporalPolicy::default()).is_empty());
    }

    #[test]
    fn sixteen_hour_recording_gate() {
        let clips = vec![
            clip("a", Some(100.0), 60.0, Some(1_700_000_000)),
            clip("b", Some(120.0), 60.0, Some(1_700_000_000 + 57_600)),
        ];
        assert!(analyze_timecodes(&clips, &[], &[], &TemporalPolicy::default()).is_empty());
        let clips = vec![
            clip("a", Some(100.0), 60.0, Some(1_700_000_000)),
            clip("b", Some(120.0), 60.0, Some(1_700_000_000 + 57_599)),
        ];
        assert_eq!(
            analyze_timecodes(&clips, &[], &[], &TemporalPolicy::default()).len(),
            1
        );
    }

    #[test]
    fn already_matched_clips_are_skipped() {
        use crate::matcher::PairAlignmentPoint;
        let clips = vec![
            clip("a", Some(100.0), 60.0, None),
            clip("b", Some(120.0), 60.0, None),
        ];
        let existing = vec![PairwiseMatch {
            left: ClipId::new("a"),
            right: ClipId::new("b"),
            rate: 1.0,
            offset: 0.0,
            confidence: 0.9,
            anchors: 10,
            left_start_seconds: 0.0,
            left_end_seconds: 60.0,
            covered_seconds: 60.0,
            residual_seconds: 0.01,
            alignment_points: Vec::<PairAlignmentPoint>::new(),
            evidence: MatchEvidence::Waveform,
        }];
        assert!(analyze_timecodes(&clips, &existing, &[], &TemporalPolicy::default()).is_empty());
    }

    #[test]
    fn explicit_non_timecode_mode_disables_timecode_rescue() {
        let clips = vec![
            clip("a", Some(100.0), 60.0, Some(1_000)),
            clip("b", Some(120.0), 60.0, Some(1_020)),
        ];
        let rec_start = TemporalPolicy {
            default: TemporalMode::RecStart,
            modes: Default::default(),
        };
        assert!(analyze_timecodes(&clips, &[], &[], &rec_start).is_empty());

        let timecode = TemporalPolicy {
            default: TemporalMode::Timecode,
            modes: Default::default(),
        };
        assert_eq!(analyze_timecodes(&clips, &[], &[], &timecode).len(), 1);
    }
}
