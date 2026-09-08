//! Timeline lane visuals: port of the `AppModel` layout half
//! (`TimelineLaneVisual`, `layout`, provisional BFS groups, chronology,
//! source names). Both the live provisional preview and the final result
//! render through this one builder, so pre- and post-sync timelines agree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use align_core::{
    ClipId, MatchEvidence, MatchPreview, MatchPreviewStage, MediaKind,
    allocator::{TimelineTrackRequest, allocate, source_key_for_url},
    export_model::ExportTimeline,
    model::file_name,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarMatchState {
    Pending,
    Matched,
    Unmatched,
}

#[derive(Clone, Debug)]
pub struct BarVisual {
    pub id: String,
    pub clip_id: ClipId,
    pub url: PathBuf,
    pub source_key: String,
    pub name: String,
    pub kind: MediaKind,
    pub start: f64,
    pub duration: f64,
    pub confidence: f64,
    pub match_state: BarMatchState,
}

#[derive(Clone, Debug)]
pub struct LaneVisual {
    pub id: String,
    pub kind: MediaKind,
    pub number: usize,
    pub source_key: String,
    pub source_name: String,
    pub stream_channels: Vec<usize>,
    pub clips: Vec<BarVisual>,
}

#[derive(Clone, Debug)]
pub struct LiveClip {
    pub url: PathBuf,
    pub kind: MediaKind,
    pub duration: f64,
}

#[derive(Clone, Debug)]
pub struct LiveMatch {
    pub left: ClipId,
    pub right: ClipId,
    pub offset: f64,
    pub confidence: f64,
    pub refined: bool,
}

impl LiveMatch {
    pub fn id(left: &ClipId, right: &ClipId) -> String {
        if left.0 < right.0 {
            format!("{}–{}", left.0, right.0)
        } else {
            format!("{}–{}", right.0, left.0)
        }
    }
}

/// One finished group: bars plus optional ruler timecode when the first
/// group is timecode-chronologied (mirrors `timelineStartTimecode`).
pub struct FinalTimeline {
    pub bars: Vec<BarVisual>,
    pub states: Vec<(ClipId, f64, MatchEvidence)>,
    pub ruler_timecode: Option<f64>,
}

/// Provisional groups from live previews: BFS over candidate edges,
/// island offsets propagated along match direction (mirror
/// `rebuildProvisionalTimeline`). Returns (clip_id, start) groups.
pub fn provisional_groups(
    clips: &HashMap<ClipId, LiveClip>,
    matches: &HashMap<String, LiveMatch>,
) -> Vec<Vec<(ClipId, f64)>> {
    let mut adjacency: HashMap<&ClipId, Vec<&LiveMatch>> = HashMap::new();
    for edge in matches.values() {
        adjacency.entry(&edge.left).or_default().push(edge);
        adjacency.entry(&edge.right).or_default().push(edge);
    }
    let mut ids: Vec<&ClipId> = clips.keys().collect();
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    let mut visited: HashSet<&ClipId> = HashSet::new();
    let mut groups = Vec::new();
    for id in ids {
        if visited.contains(id) {
            continue;
        }
        // FIFO queue = BFS, like Swift's `removeFirst`.
        let mut queue = std::collections::VecDeque::from([id]);
        let mut starts: HashMap<&ClipId, f64> = [(id, 0.0)].into_iter().collect();
        visited.insert(id);
        while let Some(current) = queue.pop_front() {
            let Some(edges) = adjacency.get(current) else {
                continue;
            };
            for edge in edges {
                let next = if *current == edge.left {
                    &edge.right
                } else {
                    &edge.left
                };
                if visited.contains(next) {
                    continue;
                }
                let base = starts.get(current).copied().unwrap_or(0.0);
                starts.insert(
                    next,
                    if *current == edge.left {
                        base - edge.offset
                    } else {
                        base + edge.offset
                    },
                );
                visited.insert(next);
                queue.push_back(next);
            }
        }
        let minimum = starts.values().fold(f64::INFINITY, |a, b| a.min(*b));
        let minimum = if minimum.is_finite() { minimum } else { 0.0 };
        groups.push(
            starts
                .into_iter()
                .map(|(id, start)| ((*id).clone(), start - minimum))
                .collect(),
        );
    }
    groups
}

/// Provisional bars: cursor-packed groups (1 s gaps), refined edges green.
pub fn provisional_bars(
    clips: &HashMap<ClipId, LiveClip>,
    matches: &HashMap<String, LiveMatch>,
    confidences: &HashMap<ClipId, f64>,
) -> Vec<BarVisual> {
    let groups = provisional_groups(clips, matches);
    let mut cursor = 0.0;
    let mut out = Vec::new();
    for items in &groups {
        let span = items
            .iter()
            .map(|(id, start)| start + clips.get(id).map_or(0.0, |c| c.duration))
            .fold(0.0f64, f64::max);
        for (id, start) in items {
            let Some(clip) = clips.get(id) else {
                continue;
            };
            let matched = matches
                .values()
                .any(|m| m.refined && (m.left == *id || m.right == *id));
            out.push(BarVisual {
                id: id.0.clone(),
                clip_id: id.clone(),
                url: clip.url.clone(),
                source_key: source_key_for_url(&clip.url),
                name: file_name(&clip.url),
                kind: clip.kind,
                start: cursor + start,
                duration: clip.duration,
                confidence: confidences.get(id).copied().unwrap_or(0.0),
                match_state: if matched {
                    BarMatchState::Matched
                } else {
                    BarMatchState::Pending
                },
            });
        }
        cursor += span + 1.0;
    }
    out
}

/// Final bars from the same assembled timeline used by every exporter.
/// This keeps timestamp/timecode placement, gaps and overlaps identical in
/// the UI and the NLE output.
pub fn final_bars(result: &align_core::SyncResult) -> FinalTimeline {
    let unmatched: HashSet<ClipId> = result.unmatched.iter().cloned().collect();
    let Ok(timeline) = ExportTimeline::from_result(result, true) else {
        return FinalTimeline {
            bars: Vec::new(),
            states: Vec::new(),
            ruler_timecode: None,
        };
    };
    let ruler_timecode = timeline.ruler_timecode_start();
    let preview = timeline.preview_items(&unmatched);
    let bars = preview
        .iter()
        .map(|item| BarVisual {
            id: item.id.clone(),
            clip_id: item.clip_id.clone(),
            url: item.url.clone(),
            source_key: item.source_key.clone(),
            name: item.name.clone(),
            kind: item.kind,
            start: item.start,
            duration: item.duration,
            confidence: item.confidence,
            match_state: if item.matched {
                BarMatchState::Matched
            } else {
                BarMatchState::Unmatched
            },
        })
        .collect();
    let states = preview
        .iter()
        .filter(|item| item.matched)
        .map(|item| {
            let evidence = result
                .matches
                .iter()
                .filter(|m| m.left == item.clip_id || m.right == item.clip_id)
                .filter_map(|m| m.evidence)
                .min_by_key(|evidence| match evidence {
                    MatchEvidence::Waveform => 0,
                    MatchEvidence::SpannedMetadata => 1,
                    MatchEvidence::Timecode => 2,
                })
                .unwrap_or(MatchEvidence::Waveform);
            (item.clip_id.clone(), item.confidence, evidence)
        })
        .collect();
    FinalTimeline {
        bars,
        states,
        ruler_timecode,
    }
}

/// Pack bars into V/A numbered lanes (mirror `layout`).
pub fn layout_bars(
    bars: Vec<BarVisual>,
    stream_channels: &HashMap<ClipId, Vec<usize>>,
) -> Vec<LaneVisual> {
    let mut lanes = Vec::new();
    for kind in [MediaKind::Video, MediaKind::Audio] {
        let segments: Vec<BarVisual> = bars.iter().filter(|b| b.kind == kind).cloned().collect();
        let requests: Vec<TimelineTrackRequest> = segments
            .iter()
            .map(|b| {
                TimelineTrackRequest::new(b.id.clone(), b.source_key.clone(), b.start, b.duration)
            })
            .collect();
        let assignments = allocate(&requests);
        let mut by_lane: HashMap<usize, Vec<BarVisual>> = HashMap::new();
        for bar in segments {
            by_lane
                .entry(assignments.get(&bar.id).copied().unwrap_or(0))
                .or_default()
                .push(bar);
        }
        let mut indices: Vec<usize> = by_lane.keys().copied().collect();
        indices.sort_unstable();
        for index in indices {
            let mut sorted = by_lane.remove(&index).unwrap_or_default();
            sorted.sort_by(|a, b| {
                a.start
                    .total_cmp(&b.start)
                    .then_with(|| a.name.cmp(&b.name))
            });
            let source_key = sorted
                .first()
                .map(|b| b.source_key.clone())
                .unwrap_or_default();
            let single_source = sorted.iter().all(|b| b.source_key == source_key);
            let source_name = if single_source {
                source_name_for(&source_key)
            } else {
                String::new()
            };
            let layouts: Vec<Vec<usize>> = sorted
                .iter()
                .filter_map(|b| stream_channels.get(&b.clip_id))
                .filter(|v| !v.is_empty())
                .cloned()
                .collect();
            let stream_count = layouts.iter().map(|v| v.len()).min().unwrap_or(0);
            let channels: Vec<usize> = (0..stream_count)
                .map(|stream| {
                    layouts
                        .iter()
                        .filter_map(|v| v.get(stream).copied())
                        .min()
                        .unwrap_or(0)
                })
                .collect();
            let kind_name = match kind {
                MediaKind::Video => "video",
                MediaKind::Audio => "audio",
            };
            lanes.push(LaneVisual {
                id: format!("{kind_name}-{index}"),
                kind,
                number: index + 1,
                source_key,
                source_name,
                stream_channels: channels,
                clips: sorted,
            });
        }
    }
    lanes
}

/// `device:a:b:c` → `a b c`, `span:` → Linked BWF, else folder name.
pub fn source_name_for(key: &str) -> String {
    if let Some(number) = key
        .strip_prefix("imported-video-")
        .and_then(|raw| raw.split('-').next_back()?.parse::<i64>().ok())
    {
        return format!("Imported V{number}");
    }
    if let Some(number) = key
        .strip_prefix("imported-audio-")
        .and_then(|raw| raw.split('-').next_back()?.parse::<i64>().ok())
    {
        return format!("Imported A{number}");
    }
    if let Some(rest) = key.strip_prefix("device:") {
        return rest.replace(':', " ");
    }
    if key.starts_with("span:") {
        return "Linked BWF".to_string();
    }
    Path::new(key)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}
// ------------------------------------------------------------ view geometry
// Pure layout math used by the timeline renderer, unit-tested apart from
// GPUI so bar positions are provable without a display server.

/// Width of the label column (mirrors Swift `labelWidth`).
pub const LABEL_WIDTH: f32 = 56.0;
/// Minimum on-screen bar width (mirrors Swift `max(2.0, …)`).
pub const MIN_BAR_WIDTH: f64 = 2.0;

/// Fit-zoom scale: at `zoom` 1.0 the whole `duration` spans `fit_width`
/// (mirrors Swift's `availableWidth`, i.e. viewport minus label column).
pub fn timeline_scale(duration: f64, zoom: f64, fit_width: f32) -> (f64, f32) {
    let duration = duration.max(1.0);
    let fit = fit_width.max(100.0);
    let px_per_sec = fit as f64 * zoom / duration;
    (px_per_sec, (duration * px_per_sec) as f32)
}

/// Per-bar `(x, width)` in px for one lane row, mirroring Swift's
/// `ZStack` + `offset(x:)` absolute placement: `x` is exact
/// (`start * px_per_sec`), never accumulated, and `width` is clamped to
/// the next bar's start (like Swift's `min(availableWidth, …)`), so
/// min-width bars can never shift — or visually cover — their neighbours.
/// `bars` must be sorted by `start` (as `layout_bars` emits them).
pub fn bar_row_geometry(bars: &[BarVisual], px_per_sec: f64) -> Vec<(f32, f32)> {
    bars.iter()
        .enumerate()
        .map(|(index, bar)| {
            let x = (bar.start * px_per_sec) as f32;
            let natural = (bar.duration * px_per_sec).max(MIN_BAR_WIDTH) as f32;
            let width = match bars.get(index + 1) {
                Some(next) => natural.min(((next.start * px_per_sec) as f32 - x).max(0.0)),
                None => natural,
            };
            (x, width)
        })
        .collect()
}

/// Correction options for one clip: refined waveform matches involving it,
/// sorted by partner name (mirror `correctionOptions`).
#[derive(Clone, Debug)]
pub struct CorrectionOption {
    pub id: String,
    pub left: ClipId,
    pub right: ClipId,
    pub other_name: String,
    pub offset: f64,
}

pub fn correction_options(
    clip_id: &ClipId,
    matches: &[LiveMatch],
    names: &HashMap<ClipId, String>,
) -> Vec<CorrectionOption> {
    let mut out: Vec<CorrectionOption> = matches
        .iter()
        .filter(|m| m.refined && (m.left == *clip_id || m.right == *clip_id))
        .map(|m| {
            let other = if m.left == *clip_id {
                &m.right
            } else {
                &m.left
            };
            CorrectionOption {
                id: LiveMatch::id(&m.left, &m.right),
                left: m.left.clone(),
                right: m.right.clone(),
                other_name: names.get(other).cloned().unwrap_or_else(|| other.0.clone()),
                offset: m.offset,
            }
        })
        .collect();
    out.sort_by(|a, b| a.other_name.cmp(&b.other_name));
    out
}

/// Apply a live preview event to the match map.
pub fn apply_preview(matches: &mut HashMap<String, LiveMatch>, preview: &MatchPreview) {
    match preview.stage {
        MatchPreviewStage::Rejected => {
            matches.remove(&LiveMatch::id(&preview.left, &preview.right));
        }
        MatchPreviewStage::Candidate | MatchPreviewStage::Refined => {
            let id = LiveMatch::id(&preview.left, &preview.right);
            matches.insert(
                id,
                LiveMatch {
                    left: preview.left.clone(),
                    right: preview.right.clone(),
                    offset: preview.offset,
                    confidence: preview.confidence,
                    refined: preview.stage == MatchPreviewStage::Refined,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip_id(s: &str) -> ClipId {
        ClipId::new(s)
    }

    fn live_clip(id: &str) -> (ClipId, LiveClip) {
        let cid = clip_id(id);
        (
            cid.clone(),
            LiveClip {
                url: PathBuf::from(format!("/v/{id}.wav")),
                kind: MediaKind::Audio,
                duration: 10.0,
            },
        )
    }

    #[test]
    fn provisional_chain_propagates_offsets() {
        let clips: HashMap<ClipId, LiveClip> =
            ["a", "b", "c"].iter().map(|id| live_clip(id)).collect();
        let mut matches = HashMap::new();
        matches.insert(
            LiveMatch::id(&clip_id("a"), &clip_id("b")),
            LiveMatch {
                left: clip_id("a"),
                right: clip_id("b"),
                offset: 2.0,
                confidence: 0.9,
                refined: true,
            },
        );
        matches.insert(
            LiveMatch::id(&clip_id("b"), &clip_id("c")),
            LiveMatch {
                left: clip_id("b"),
                right: clip_id("c"),
                offset: 3.0,
                confidence: 0.8,
                refined: false,
            },
        );
        let groups = provisional_groups(&clips, &matches);
        assert_eq!(groups.len(), 1);
        let mut starts: HashMap<String, f64> =
            groups[0].iter().map(|(id, s)| (id.0.clone(), *s)).collect();
        // a=2, b=0, c=-3 → shifted by +3.
        assert!((starts.remove("a").unwrap() - 5.0).abs() < 1e-9);
        assert!((starts.remove("b").unwrap() - 3.0).abs() < 1e-9);
        assert!((starts.remove("c").unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn source_names_mirror_swift() {
        assert_eq!(source_name_for("device:Sony:A:123"), "Sony A 123");
        assert_eq!(source_name_for("span:abc"), "Linked BWF");
        assert_eq!(source_name_for("imported-video-000003"), "Imported V3");
        assert_eq!(source_name_for("imported-audio-000007"), "Imported A7");
        assert_eq!(source_name_for("/Volumes/Ed/Cam"), "Cam");
    }

    #[test]
    fn layout_packs_same_source() {
        let bars = vec![
            BarVisual {
                id: "a".to_string(),
                clip_id: clip_id("a"),
                url: PathBuf::from("/v/a.wav"),
                source_key: "/v".to_string(),
                name: "a".to_string(),
                kind: MediaKind::Audio,
                start: 0.0,
                duration: 10.0,
                confidence: 1.0,
                match_state: BarMatchState::Matched,
            },
            BarVisual {
                id: "b".to_string(),
                clip_id: clip_id("b"),
                url: PathBuf::from("/v/b.wav"),
                source_key: "/v".to_string(),
                name: "b".to_string(),
                kind: MediaKind::Audio,
                start: 20.0,
                duration: 10.0,
                confidence: 1.0,
                match_state: BarMatchState::Matched,
            },
        ];
        let lanes = layout_bars(bars, &HashMap::new());
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].clips.len(), 2);
        assert_eq!(lanes[0].source_name, "v");
    }

    #[test]
    fn final_bars_orders_islands_by_temporal_policy() {
        use align_core::model::MappingPoint;
        use align_core::{
            AudioSummary, Clip, ClipPlacement, MediaTime, RecordingTimestampSource, SourceTimecode,
            SyncIsland, SyncProject, SyncResult, TemporalMode, TemporalPolicy, TimeMap,
        };

        let clip = |name: &str, recorded: i64, tc_secs: f64| Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/v/{name}.wav")),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(10.0),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: Some(SourceTimecode {
                    text: "tc".into(),
                    frame_number: (tc_secs * 25.0) as i64,
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
        let island = |id: usize, name: &str| SyncIsland {
            id,
            placements: vec![ClipPlacement {
                clip_id: ClipId::new(name),
                mapping: TimeMap {
                    points: vec![
                        MappingPoint {
                            source: MediaTime::seconds(0.0),
                            island: MediaTime::seconds(0.0),
                        },
                        MappingPoint {
                            source: MediaTime::seconds(10.0),
                            island: MediaTime::seconds(10.0),
                        },
                    ],
                },
                confidence: 0.9,
            }],
        };
        // Recorded order B(1000) < C(1500) < A(2000); timecode order reversed.
        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: vec![
                    clip("a", 2000, 100.0),
                    clip("b", 1000, 200.0),
                    clip("c", 1500, 150.0),
                ],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: vec![island(0, "a"), island(1, "b")],
            unmatched: vec![ClipId::new("c")],
            matches: Vec::new(),
            temporal_policy: TemporalPolicy::default(),
        };
        let order =
            |bars: &[BarVisual]| bars.iter().map(|b| b.clip_id.0.clone()).collect::<Vec<_>>();
        let automatic = final_bars(&result);
        assert_eq!(order(&automatic.bars), vec!["b", "c", "a"]);
        let starts: HashMap<_, _> = automatic
            .bars
            .iter()
            .map(|bar| (bar.clip_id.0.as_str(), bar.start))
            .collect();
        assert_eq!(starts["b"], 0.0);
        assert_eq!(starts["c"], 500.0);
        assert_eq!(starts["a"], 1_000.0);
        let timed = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            temporal_policy: TemporalPolicy {
                default: TemporalMode::Timecode,
                modes: HashMap::new(),
            },
            ..result
        };
        assert_eq!(order(&final_bars(&timed).bars), vec!["a", "c", "b"]);
    }

    #[test]
    fn row_geometry_never_overlaps_and_fills_fit() {
        // Shape of the real corpus: long sequential takes over hours.
        let mk = |id: &str, start: f64, duration: f64| BarVisual {
            id: id.to_string(),
            clip_id: ClipId::new(id),
            url: PathBuf::from(format!("/v/{id}.wav")),
            source_key: "/v".to_string(),
            name: id.to_string(),
            kind: MediaKind::Audio,
            start,
            duration,
            confidence: 1.0,
            match_state: BarMatchState::Matched,
        };
        let bars = vec![
            mk("a", 10.0, 2050.0),
            mk("b", 2061.0, 4.4),
            mk("c", 2068.0, 4768.0),
            mk("d", 6850.0, 711.0),
        ];
        let total = 7561.0;
        // Stand-in viewport width (the live view passes the real one).
        let fit_width = 900.0;
        let (px_per_sec, timeline_w) = timeline_scale(total, 1.0, fit_width);
        assert!((timeline_w - fit_width).abs() < 0.01);
        let geometry = bar_row_geometry(&bars, px_per_sec);
        // Exact absolute positions (no accumulation drift); widths follow
        // Swift's `max(0, min(available, max(2, natural)))`: slivers narrower
        // than 2 px are allowed when the next bar starts sooner, but bars
        // never overlap and the last bar ends exactly at the Fit edge.
        for (i, ((x, width), bar)) in geometry.iter().zip(bars.iter()).enumerate() {
            assert!(
                (*x - (bar.start * px_per_sec) as f32).abs() < 0.01,
                "bar {} misplaced: x={x}",
                bar.name
            );
            let natural = ((bar.duration * px_per_sec).max(MIN_BAR_WIDTH)) as f32;
            let available = bars
                .get(i + 1)
                .map(|next| ((next.start * px_per_sec) as f32 - x).max(0.0))
                .unwrap_or(f32::INFINITY);
            assert!(
                (*width - natural.min(available)).abs() < 0.01,
                "bar {} width {width} != min({natural}, {available})",
                bar.name
            );
            if let Some((next_x, _)) = geometry.get(i + 1) {
                assert!(x + width <= next_x + 0.01, "bar {} overlaps next", bar.name);
            }
        }
        let (last_x, last_w) = geometry.last().copied().expect("bars");
        assert!((last_x + last_w - timeline_w).abs() < 2.5, "trailing gap");
        // Zoom 8x scales everything uniformly.
        let (zoomed_pps, zoomed_w) = timeline_scale(total, 8.0, fit_width);
        assert!((zoomed_pps / px_per_sec - 8.0).abs() < 1e-9);
        assert!((zoomed_w - fit_width * 8.0).abs() < 0.1);
    }

    #[test]
    fn row_geometry_clamps_to_next_bar_start() {
        // Dense pack where the natural width would cover the neighbour:
        // mirrors Swift's `min(availableWidth, max(2.0, naturalWidth))`.
        let mk = |id: &str, start: f64, duration: f64| BarVisual {
            id: id.to_string(),
            clip_id: ClipId::new(id),
            url: PathBuf::from(format!("/v/{id}.wav")),
            source_key: "/v".to_string(),
            name: id.to_string(),
            kind: MediaKind::Audio,
            start,
            duration,
            confidence: 1.0,
            match_state: BarMatchState::Matched,
        };
        let bars = vec![mk("a", 0.0, 10.0), mk("b", 5.0, 10.0)];
        let geometry = bar_row_geometry(&bars, 10.0);
        assert_eq!(geometry.len(), 2);
        // First bar is cut at the second bar's start …
        assert!((geometry[0].0 - 0.0).abs() < 0.01);
        assert!((geometry[0].1 - 50.0).abs() < 0.01);
        // … while the last bar keeps its full natural width.
        assert!((geometry[1].0 - 50.0).abs() < 0.01);
        assert!((geometry[1].1 - 100.0).abs() < 0.01);
    }

    #[test]
    fn apply_preview_adds_and_removes() {
        let mut matches = HashMap::new();
        apply_preview(
            &mut matches,
            &MatchPreview {
                left: clip_id("a"),
                right: clip_id("b"),
                rate: 1.0,
                offset: 1.0,
                confidence: 0.5,
                stage: MatchPreviewStage::Candidate,
            },
        );
        assert_eq!(matches.len(), 1);
        apply_preview(
            &mut matches,
            &MatchPreview {
                left: clip_id("a"),
                right: clip_id("b"),
                rate: 1.0,
                offset: 1.0,
                confidence: 0.5,
                stage: MatchPreviewStage::Rejected,
            },
        );
        assert!(matches.is_empty());
    }
}
