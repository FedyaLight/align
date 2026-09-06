//! Clip-order constraints for rejecting chronologically crossing matches.
//!
//! Strict ordering is evaluated between each pair of source tracks. Stronger
//! matches are admitted first; a weaker edge is rejected when it would invert
//! the chosen order on both tracks. One long clip may still match several
//! consecutive clips because equal ranks do not constitute a crossing.

use std::collections::{HashMap, HashSet};

use crate::allocator::source_key_for_clip;
use crate::matcher::PairwiseMatch;
use crate::model::{Clip, ClipId, ImportedTimeline, MatchEvidence};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipOrder {
    #[default]
    Auto,
    AlternateAuto,
    AsImported,
    ByDateTime,
    ByFileName,
    Ignore,
}

/// Whether clips belonging to one source track may synchronize together.
/// `Auto` follows imported NLE tracks as Linear and leaves raw media open,
/// because raw input has no authoritative track assignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackContent {
    #[default]
    Auto,
    Linear,
    Takes,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackContentPolicy {
    pub default: TrackContent,
    pub modes: HashMap<ClipId, TrackContent>,
}

impl TrackContentPolicy {
    pub fn resolve(&self, id: &ClipId) -> TrackContent {
        self.modes.get(id).copied().unwrap_or(self.default)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipOrderPolicy {
    pub default: ClipOrder,
    pub modes: HashMap<ClipId, ClipOrder>,
}

impl ClipOrderPolicy {
    pub fn resolve(&self, id: &ClipId) -> ClipOrder {
        self.modes.get(id).copied().unwrap_or(self.default)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OrderEntry {
    group: String,
    as_imported: usize,
    by_date_time: usize,
    by_file_name: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipOrderContext {
    entries: HashMap<ClipId, OrderEntry>,
    imported_groups: HashSet<String>,
}

impl ClipOrderContext {
    pub fn from_clips(clips: &[Clip], timeline: Option<&ImportedTimeline>) -> Self {
        let mut group_and_order: HashMap<ClipId, (String, usize)> = HashMap::new();
        let mut imported_groups = HashSet::new();
        if let Some(timeline) = timeline {
            let mut tracks: HashMap<String, Vec<_>> = HashMap::new();
            for edit in &timeline.edits {
                tracks
                    .entry(format!("{}:{}", edit.media_type.as_str(), edit.track_index))
                    .or_default()
                    .push(edit);
            }
            for (group, mut edits) in tracks {
                imported_groups.insert(group.clone());
                edits.sort_by(|a, b| {
                    a.timeline_start
                        .as_seconds()
                        .total_cmp(&b.timeline_start.as_seconds())
                        .then_with(|| a.id.cmp(&b.id))
                });
                let mut rank = 0;
                for edit in edits {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        group_and_order.entry(edit.clip_id.clone())
                    {
                        entry.insert((group.clone(), rank));
                        rank += 1;
                    }
                }
            }
        }

        for (index, clip) in clips.iter().enumerate() {
            group_and_order.entry(clip.id.clone()).or_insert_with(|| {
                let source = source_key_for_clip(
                    &clip.url,
                    clip.source_identifier.as_deref(),
                    clip.media_span
                        .as_ref()
                        .map(|span| span.identifier.as_str()),
                );
                (format!("{}:{source}", clip.kind.as_str()), index)
            });
        }

        let clips_by_id: HashMap<_, _> = clips.iter().map(|clip| (&clip.id, clip)).collect();
        let mut groups: HashMap<String, Vec<ClipId>> = HashMap::new();
        for (id, (group, _)) in &group_and_order {
            groups.entry(group.clone()).or_default().push(id.clone());
        }

        let mut entries = HashMap::new();
        for (group, members) in groups {
            let mut by_date = members.clone();
            by_date.sort_by(|a, b| {
                date_key(clips_by_id[a])
                    .cmp(&date_key(clips_by_id[b]))
                    .then_with(|| a.0.cmp(&b.0))
            });
            let date_ranks: HashMap<_, _> = by_date
                .into_iter()
                .enumerate()
                .map(|(rank, id)| (id, rank))
                .collect();

            let mut by_name = members.clone();
            by_name.sort_by(|a, b| {
                file_key(clips_by_id[a])
                    .cmp(&file_key(clips_by_id[b]))
                    .then_with(|| a.0.cmp(&b.0))
            });
            let name_ranks: HashMap<_, _> = by_name
                .into_iter()
                .enumerate()
                .map(|(rank, id)| (id, rank))
                .collect();

            for id in members {
                let imported_rank = group_and_order[&id].1;
                entries.insert(
                    id.clone(),
                    OrderEntry {
                        group: group.clone(),
                        as_imported: imported_rank,
                        by_date_time: date_ranks[&id],
                        by_file_name: name_ranks[&id],
                    },
                );
            }
        }
        Self {
            entries,
            imported_groups,
        }
    }
}

fn file_key(clip: &Clip) -> String {
    clip.url
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

fn date_key(clip: &Clip) -> (u8, i64, String) {
    if let Some(timestamp) = clip.recorded_at {
        return (0, timestamp, file_key(clip));
    }
    if let Some(timecode) = clip.source_timecode() {
        return (
            1,
            (timecode.as_seconds() * 1_000_000.0).round() as i64,
            file_key(clip),
        );
    }
    (2, 0, file_key(clip))
}

fn rank(entry: &OrderEntry, mode: ClipOrder) -> usize {
    match mode {
        ClipOrder::Auto | ClipOrder::AsImported | ClipOrder::Ignore => entry.as_imported,
        ClipOrder::AlternateAuto | ClipOrder::ByDateTime => entry.by_date_time,
        ClipOrder::ByFileName => entry.by_file_name,
    }
}

struct OrderedEdge<'a> {
    low_group: &'a str,
    high_group: &'a str,
    low_rank: usize,
    high_rank: usize,
}

fn ordered_edge<'a>(
    pair: &'a PairwiseMatch,
    policy: &ClipOrderPolicy,
    context: &'a ClipOrderContext,
) -> Option<OrderedEdge<'a>> {
    let left = context.entries.get(&pair.left)?;
    let right = context.entries.get(&pair.right)?;
    if left.group == right.group {
        return None;
    }
    let left_mode = policy.resolve(&pair.left);
    let right_mode = policy.resolve(&pair.right);
    if left_mode == ClipOrder::Ignore
        || right_mode == ClipOrder::Ignore
        || (left_mode == ClipOrder::Auto && right_mode == ClipOrder::Auto)
    {
        return None;
    }
    let left_rank = rank(left, left_mode);
    let right_rank = rank(right, right_mode);
    if left.group < right.group {
        Some(OrderedEdge {
            low_group: &left.group,
            high_group: &right.group,
            low_rank: left_rank,
            high_rank: right_rank,
        })
    } else {
        Some(OrderedEdge {
            low_group: &right.group,
            high_group: &left.group,
            low_rank: right_rank,
            high_rank: left_rank,
        })
    }
}

/// Keep the strongest non-crossing set for each pair of ordered tracks.
/// Spanned-media links are structural and always bypass order filtering.
pub fn enforce_clip_order(
    matches: &[PairwiseMatch],
    policy: &ClipOrderPolicy,
    context: &ClipOrderContext,
) -> Vec<PairwiseMatch> {
    let mut ranked: Vec<_> = matches.iter().enumerate().collect();
    ranked.sort_by(|(left_index, left), (right_index, right)| {
        right
            .confidence
            .total_cmp(&left.confidence)
            .then_with(|| left_index.cmp(right_index))
    });

    let mut accepted_indices = Vec::new();
    let mut accepted_edges = Vec::new();
    for (index, candidate) in ranked {
        if candidate.evidence == MatchEvidence::SpannedMetadata {
            accepted_indices.push(index);
            continue;
        }
        let Some(edge) = ordered_edge(candidate, policy, context) else {
            accepted_indices.push(index);
            continue;
        };
        let crosses = accepted_edges.iter().any(|other: &OrderedEdge<'_>| {
            other.low_group == edge.low_group
                && other.high_group == edge.high_group
                && ((other.low_rank < edge.low_rank && other.high_rank > edge.high_rank)
                    || (other.low_rank > edge.low_rank && other.high_rank < edge.high_rank))
        });
        if !crosses {
            accepted_indices.push(index);
            accepted_edges.push(edge);
        }
    }
    accepted_indices.sort_unstable();
    accepted_indices
        .into_iter()
        .map(|index| matches[index].clone())
        .collect()
}

/// Apply Syncaila-style source-track semantics before graph solving.
/// Structural spanned-media links are parts of one recording and bypass the
/// filter; every other same-track edge is rejected when either side is Linear.
pub fn enforce_track_content(
    matches: &[PairwiseMatch],
    policy: &TrackContentPolicy,
    context: &ClipOrderContext,
) -> Vec<PairwiseMatch> {
    matches
        .iter()
        .filter(|pair| {
            if pair.evidence == MatchEvidence::SpannedMetadata {
                return true;
            }
            let Some(left) = context.entries.get(&pair.left) else {
                return true;
            };
            let Some(right) = context.entries.get(&pair.right) else {
                return true;
            };
            if left.group != right.group {
                return true;
            }
            let left_mode = policy.resolve(&pair.left);
            let right_mode = policy.resolve(&pair.right);
            if left_mode == TrackContent::Linear || right_mode == TrackContent::Linear {
                return false;
            }
            if left_mode == TrackContent::Takes || right_mode == TrackContent::Takes {
                return true;
            }
            !context.imported_groups.contains(&left.group)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::PairwiseMatch;
    use crate::model::{ImportedTimeline, MediaKind, MediaTime, TimelineEdit};
    use std::path::PathBuf;

    fn clip(id: &str, path: &str, recorded_at: i64) -> Clip {
        Clip {
            id: ClipId::new(id),
            url: PathBuf::from(path),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(10.0),
            audio: Vec::new(),
            video: None,
            recorded_at: Some(recorded_at),
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        }
    }

    fn edge(left: &str, right: &str, confidence: f64) -> PairwiseMatch {
        PairwiseMatch {
            left: ClipId::new(left),
            right: ClipId::new(right),
            rate: 1.0,
            offset: 0.0,
            confidence,
            anchors: 10,
            left_start_seconds: 0.0,
            left_end_seconds: 10.0,
            covered_seconds: 10.0,
            residual_seconds: 0.0,
            alignment_points: Vec::new(),
            evidence: MatchEvidence::Waveform,
        }
    }

    fn spanned_edge(left: &str, right: &str) -> PairwiseMatch {
        PairwiseMatch {
            evidence: MatchEvidence::SpannedMetadata,
            ..edge(left, right, 1.0)
        }
    }

    fn edit(id: &str, clip_id: &str, track_index: usize, start: f64) -> TimelineEdit {
        TimelineEdit {
            id: id.to_string(),
            name: None,
            clip_id: ClipId::new(clip_id),
            media_type: MediaKind::Audio,
            source_in: MediaTime::seconds(0.0),
            source_out: MediaTime::seconds(10.0),
            timeline_start: MediaTime::seconds(start),
            timeline_end: MediaTime::seconds(start + 10.0),
            playback_rate: 1.0,
            plays_backward: false,
            fcp7_time_remap_xml: None,
            fcp7_filter_xmls: Vec::new(),
            fcp7_retime_in: None,
            fcp7_retime_out: None,
            fcp7_retime_duration: None,
            fcp7_labels_xml: None,
            audio_source_channel: None,
            fcpxml_audio_role: None,
            track_index,
            audio_track_index: None,
            enabled: true,
            track_enabled: true,
            track_locked: false,
            audio_enabled: None,
            audio_track_enabled: None,
            audio_track_locked: None,
            transition_after: None,
            audio_transition_after: None,
            linked_audio_edit: None,
        }
    }

    #[test]
    fn strict_filename_order_rejects_weaker_crossing() {
        let clips = [
            clip("a1", "/a/001.wav", 1),
            clip("a2", "/a/002.wav", 2),
            clip("b1", "/b/001.wav", 1),
            clip("b2", "/b/002.wav", 2),
        ];
        let context = ClipOrderContext::from_clips(&clips, None);
        let policy = ClipOrderPolicy {
            default: ClipOrder::ByFileName,
            modes: HashMap::new(),
        };
        let kept = enforce_clip_order(
            &[edge("a1", "b2", 0.70), edge("a2", "b1", 0.90)],
            &policy,
            &context,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].left, ClipId::new("a2"));
    }

    #[test]
    fn alternate_auto_uses_metadata_order_and_match_strength() {
        let clips = [
            clip("a1", "/a/z.wav", 1),
            clip("a2", "/a/a.wav", 2),
            clip("b1", "/b/z.wav", 1),
            clip("b2", "/b/a.wav", 2),
        ];
        let context = ClipOrderContext::from_clips(&clips, None);
        let crossing = [edge("a1", "b2", 0.70), edge("a2", "b1", 0.90)];
        assert_eq!(
            enforce_clip_order(&crossing, &ClipOrderPolicy::default(), &context).len(),
            2
        );
        let policy = ClipOrderPolicy {
            default: ClipOrder::AlternateAuto,
            modes: HashMap::new(),
        };
        let kept = enforce_clip_order(&crossing, &policy, &context);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].left, ClipId::new("a2"));
    }

    #[test]
    fn ignore_keeps_crossings_and_equal_rank_fanout() {
        let clips = [
            clip("a1", "/a/001.wav", 1),
            clip("a2", "/a/002.wav", 2),
            clip("b1", "/b/001.wav", 1),
            clip("b2", "/b/002.wav", 2),
        ];
        let context = ClipOrderContext::from_clips(&clips, None);
        let crossing = [edge("a1", "b2", 0.70), edge("a2", "b1", 0.90)];
        assert_eq!(
            enforce_clip_order(&crossing, &ClipOrderPolicy::default(), &context).len(),
            2
        );
        let strict = ClipOrderPolicy {
            default: ClipOrder::ByDateTime,
            modes: HashMap::new(),
        };
        assert_eq!(
            enforce_clip_order(
                &[edge("a1", "b1", 0.90), edge("a1", "b2", 0.80)],
                &strict,
                &context,
            )
            .len(),
            2
        );
    }

    #[test]
    fn imported_order_uses_timeline_positions_not_input_or_filename_order() {
        let clips = [
            clip("a2", "/a/001.wav", 2),
            clip("a1", "/a/002.wav", 1),
            clip("b2", "/b/001.wav", 2),
            clip("b1", "/b/002.wav", 1),
        ];
        let timeline = ImportedTimeline {
            name: "Order".to_string(),
            frame_duration: MediaTime::new(1, 25),
            edits: vec![
                edit("a1-edit", "a1", 1, 0.0),
                edit("a2-edit", "a2", 1, 10.0),
                edit("b1-edit", "b1", 2, 0.0),
                edit("b2-edit", "b2", 2, 10.0),
            ],
        };
        let context = ClipOrderContext::from_clips(&clips, Some(&timeline));
        let policy = ClipOrderPolicy {
            default: ClipOrder::AsImported,
            modes: HashMap::new(),
        };
        let kept = enforce_clip_order(
            &[edge("a1", "b2", 0.70), edge("a2", "b1", 0.90)],
            &policy,
            &context,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].left, ClipId::new("a2"));
    }

    #[test]
    fn one_strict_track_constrains_an_auto_track() {
        let clips = [
            clip("a1", "/a/001.wav", 1),
            clip("a2", "/a/002.wav", 2),
            clip("b1", "/b/001.wav", 1),
            clip("b2", "/b/002.wav", 2),
        ];
        let context = ClipOrderContext::from_clips(&clips, None);
        let policy = ClipOrderPolicy {
            default: ClipOrder::Auto,
            modes: [
                (ClipId::new("a1"), ClipOrder::ByFileName),
                (ClipId::new("a2"), ClipOrder::ByFileName),
            ]
            .into_iter()
            .collect(),
        };
        let kept = enforce_clip_order(
            &[edge("a1", "b2", 0.70), edge("a2", "b1", 0.90)],
            &policy,
            &context,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].left, ClipId::new("a2"));
    }

    #[test]
    fn linear_rejects_same_track_matches_but_takes_and_auto_keep_them() {
        let clips = [
            clip("a1", "/a/001.wav", 1),
            clip("a2", "/a/002.wav", 2),
            clip("b1", "/b/001.wav", 1),
        ];
        let context = ClipOrderContext::from_clips(&clips, None);
        let matches = [edge("a1", "a2", 0.9), edge("a1", "b1", 0.8)];

        assert_eq!(
            enforce_track_content(&matches, &TrackContentPolicy::default(), &context),
            matches
        );
        let takes = TrackContentPolicy {
            default: TrackContent::Takes,
            modes: HashMap::new(),
        };
        assert_eq!(enforce_track_content(&matches, &takes, &context), matches);

        let linear = TrackContentPolicy {
            default: TrackContent::Auto,
            modes: [(ClipId::new("a1"), TrackContent::Linear)]
                .into_iter()
                .collect(),
        };
        assert_eq!(
            enforce_track_content(&matches, &linear, &context),
            vec![edge("a1", "b1", 0.8)]
        );
    }

    #[test]
    fn linear_keeps_structural_spanned_links() {
        let clips = [clip("a1", "/a/001.wav", 1), clip("a2", "/a/002.wav", 2)];
        let context = ClipOrderContext::from_clips(&clips, None);
        let linear = TrackContentPolicy {
            default: TrackContent::Linear,
            modes: HashMap::new(),
        };
        let matches = [spanned_edge("a1", "a2")];
        assert_eq!(enforce_track_content(&matches, &linear, &context), matches);
    }

    #[test]
    fn automatic_is_linear_for_imported_tracks_and_takes_for_raw_media() {
        let clips = [clip("a1", "/a/001.wav", 1), clip("a2", "/a/002.wav", 2)];
        let pair = [edge("a1", "a2", 0.9)];
        let raw = ClipOrderContext::from_clips(&clips, None);
        assert_eq!(
            enforce_track_content(&pair, &TrackContentPolicy::default(), &raw),
            pair
        );

        let timeline = ImportedTimeline {
            name: "Linear".to_string(),
            frame_duration: MediaTime::new(1, 25),
            edits: vec![
                edit("a1-edit", "a1", 1, 0.0),
                edit("a2-edit", "a2", 1, 10.0),
            ],
        };
        let imported = ClipOrderContext::from_clips(&clips, Some(&timeline));
        assert!(enforce_track_content(&pair, &TrackContentPolicy::default(), &imported).is_empty());
        let takes = TrackContentPolicy {
            default: TrackContent::Takes,
            modes: HashMap::new(),
        };
        assert_eq!(enforce_track_content(&pair, &takes, &imported), pair);
    }
}
