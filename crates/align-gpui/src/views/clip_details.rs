//! The hover card of a timeline clip: its name, how it got its place, and
//! what the result knows about it. The card reads the current state each
//! time it draws, so the timeline builds nothing for it, and an open card
//! follows the analysis as it settles.

use align_core::{MatchEvidence, SourceTimecode};

use super::controls::{clip_status_icon, fmt_duration, fmt_timeline_position};
use super::{
    AlignApp, AnyView, App, Context, Entity, IntoElement, ParentElement, Render, Styled, Theme,
    Window, div, middle_ellipsis, px, rgb, svg_icon,
};
use crate::lane::{BarMatchState, BarVisual};
use crate::state::{AppData, ClipState};
use gpui::AppContext as _;

/// Below this the source range of a clip is its whole file.
const SOURCE_EDGE: f64 = 0.02;
/// The card's width. A definite width lets GPUI measure the notes it wraps.
const CARD_WIDTH: f32 = 340.0;
/// Characters of the path that fit on the card's one path line.
const PATH_CHARS: usize = 52;

/// What the card of one bar says. Rows hold only data the clip has.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ClipDetails {
    pub(super) name: String,
    /// The mark the bar carries in its corner.
    pub(super) icon: Option<&'static str>,
    /// How the clip got its place on the timeline.
    pub(super) method: String,
    pub(super) rows: Vec<(&'static str, String)>,
    /// What the camera clock says about the clip.
    pub(super) notes: Vec<String>,
    pub(super) path: String,
}

impl ClipDetails {
    /// The card of the bar `bar_id` as the timeline shows it now.
    pub(super) fn of(data: &AppData, bar_id: &str) -> Option<Self> {
        let bar = data
            .lanes
            .iter()
            .flat_map(|lane| &lane.clips)
            .find(|bar| bar.id == bar_id)?;
        Some(Self::for_bar(data, bar))
    }

    fn for_bar(data: &AppData, bar: &BarVisual) -> Self {
        let state = bar.match_state;
        let synchronized_by = (state == BarMatchState::Matched).then_some(bar.evidence);
        let method = match (state, synchronized_by) {
            (BarMatchState::Pending, _) => "Analyzing: place not confirmed yet".to_string(),
            (BarMatchState::Refined, _) => "Analyzing: match found, not confirmed yet".to_string(),
            (_, Some(Some(MatchEvidence::Waveform))) => "Synchronized by waveform".to_string(),
            (_, Some(Some(MatchEvidence::Timecode))) => "Synchronized by timecode".to_string(),
            (_, Some(Some(MatchEvidence::SpannedMetadata))) => {
                "Synchronized by file span".to_string()
            }
            (_, Some(None)) => "Synchronized".to_string(),
            (BarMatchState::CameraClock, _) => "Placed approximately by camera clock".to_string(),
            _ => "Not synchronized".to_string(),
        };
        let mut rows = Vec::new();
        // Timecode and file spans carry a fixed confidence and no drift of
        // their own, so only a waveform match states them.
        if synchronized_by == Some(Some(MatchEvidence::Waveform)) {
            rows.push(("Confidence", percent(bar.confidence)));
            if let Some(ppm) = drift_ppm(data, bar) {
                rows.push(("Drift", format!("{ppm} ppm")));
            }
        }
        if state.unsettled() {
            // The live preview packs groups side by side: no timeline
            // position is known yet.
            rows.push(("Duration", fmt_duration(bar.duration)));
        } else {
            rows.push(("Timeline", timeline_range(data, bar)));
        }
        if let Some(timecode) = source_timecode(data, bar) {
            rows.push(("Source TC", timecode));
        }
        if let Some(range) = source_range(data, bar) {
            rows.push(("Source", range));
        }
        if let Some(uses) = uses(data, bar) {
            rows.push(("Use", uses));
        }
        Self {
            name: bar.name.clone(),
            icon: clip_status_icon(state),
            method,
            rows,
            notes: camera_clock_notes(data, bar),
            path: bar.url.display().to_string(),
        }
    }
}

fn percent(confidence: f64) -> String {
    format!("{:.0}%", (confidence * 100.0).round().clamp(0.0, 100.0))
}

/// Drift of a matched clip the list has measured, when it is worth stating.
fn drift_ppm(data: &AppData, bar: &BarVisual) -> Option<i64> {
    data.clips
        .iter()
        .find(|row| row.clip_id.as_ref() == Some(&bar.clip_id))
        .and_then(|row| match row.state {
            ClipState::Synchronized { drift_ppm, .. } if drift_ppm.abs() >= 1.0 => {
                Some(drift_ppm.round() as i64)
            }
            _ => None,
        })
}

/// Where the bar starts and ends, in the ruler's clock, and how long it is.
fn timeline_range(data: &AppData, bar: &BarVisual) -> String {
    let from = fmt_timeline_position(data.ruler_timecode, bar.start);
    let to = fmt_timeline_position(data.ruler_timecode, bar.start + bar.duration);
    format!("{from} – {to} · {}", fmt_duration(bar.duration))
}

fn result_clip<'a>(data: &'a AppData, bar: &BarVisual) -> Option<&'a align_core::Clip> {
    data.result
        .as_ref()?
        .project
        .clips
        .iter()
        .find(|clip| clip.id == bar.clip_id)
}

/// The source's own timecode where this bar starts playing it.
fn source_timecode(data: &AppData, bar: &BarVisual) -> Option<String> {
    let Some(timecode) = result_clip(data, bar).and_then(|clip| clip.source_timecode()) else {
        // Before the first result, the probe's label of the file's start.
        return (bar.source_in < SOURCE_EDGE)
            .then(|| {
                data.clips
                    .iter()
                    .find(|row| row.clip_id.as_ref() == Some(&bar.clip_id))?
                    .timecode
                    .clone()
            })
            .flatten();
    };
    Some(timecode_at(timecode, bar.source_in))
}

/// `timecode` advanced by `offset` source seconds, or the file's start label
/// when its clock has no frames to count, as a recorder's sample count.
fn timecode_at(timecode: &SourceTimecode, offset: f64) -> String {
    let frame = timecode.frame_duration.as_seconds();
    let frames = if frame > 0.0 {
        (offset / frame).round() as i64
    } else {
        0
    };
    if frames == 0 {
        return timecode.text.clone();
    }
    SourceTimecode::from_frame_number(
        timecode.frame_number + frames,
        timecode.frame_duration,
        timecode.drop_frame,
    )
    .map_or_else(
        || format!("{} at the file's start", timecode.text),
        |at| at.text,
    )
}

/// The part of the source this bar plays, when it is not the whole file.
fn source_range(data: &AppData, bar: &BarVisual) -> Option<String> {
    let length = result_clip(data, bar).map(|clip| clip.duration.as_seconds())?;
    let partial = bar.source_in >= SOURCE_EDGE || bar.source_out <= length - SOURCE_EDGE;
    partial.then(|| {
        format!(
            "{} – {} of {}",
            fmt_duration(bar.source_in),
            fmt_duration(bar.source_out),
            fmt_duration(length)
        )
    })
}

/// Which of several uses of one source this bar is, in timeline order.
fn uses(data: &AppData, bar: &BarVisual) -> Option<String> {
    let mut all: Vec<&BarVisual> = data
        .lanes
        .iter()
        .flat_map(|lane| &lane.clips)
        .filter(|other| other.clip_id == bar.clip_id)
        .collect();
    if all.len() < 2 {
        return None;
    }
    all.sort_by(|a, b| a.start.total_cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    let index = all.iter().position(|other| other.id == bar.id)? + 1;
    Some(format!("{index} of {} on this timeline", all.len()))
}

fn camera_clock_notes(data: &AppData, bar: &BarVisual) -> Vec<String> {
    let name = |clip| data.clip_name(clip);
    if let Some(mark) = &bar.camera_clock {
        return vec![format!(
            "Between {} and {}, within about ±{:.1} s. Not synchronized.",
            name(&mark.before),
            name(&mark.after),
            mark.tolerance
        )];
    }
    let imported = data
        .result
        .as_ref()
        .is_some_and(|result| result.project.imported_timeline.is_some());
    if imported {
        // An imported edit keeps its place; the camera clock only checks it.
        return data
            .camera_clock_hints()
            .iter()
            .find(|hint| format!("imported-{}-{}", hint.media.as_str(), hint.edit_id) == bar.id)
            .map(|hint| {
                let deviation = hint.deviation();
                if hint.agrees() {
                    format!("Camera clock agrees within ±{:.1} s.", hint.tolerance)
                } else {
                    format!(
                        "Starts {:.1} s {} than the camera clock (±{:.1} s).",
                        deviation.abs(),
                        if deviation > 0.0 { "later" } else { "earlier" },
                        hint.tolerance
                    )
                }
            })
            .into_iter()
            .collect();
    }
    if bar.match_state != BarMatchState::Unmatched || data.camera_clock_applied(&bar.clip_id) {
        return Vec::new();
    }
    data.camera_clock
        .estimate(&bar.clip_id)
        .map(|estimate| {
            format!(
                "Camera clock can place it between {} and {}, within about ±{:.1} s.",
                name(&estimate.before),
                name(&estimate.after),
                estimate.tolerance
            )
        })
        .into_iter()
        .collect()
}

/// The card shown over a clip.
struct ClipCard {
    app: Entity<AlignApp>,
    bar_id: String,
    theme: Theme,
}

impl Render for ClipCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let Some(details) = ClipDetails::of(&self.app.read(cx).data, &self.bar_id) else {
            // The bar has left the timeline.
            return div();
        };
        let mut method = div().flex().flex_row().items_center().gap_1();
        if let Some(icon) = details.icon {
            method = method.child(svg_icon(icon, 11.0, theme.icon));
        }
        let mut card = div()
            .w(px(CARD_WIDTH))
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme.border))
            .bg(rgb(theme.panel))
            .text_color(rgb(theme.text))
            .text_xs()
            .child(
                div()
                    .font_weight(gpui::FontWeight(600.0))
                    .child(details.name.clone()),
            )
            .child(method.child(details.method.clone()));
        for (label, value) in &details.rows {
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .w(px(76.))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(rgb(theme.dim))
                            .child(*label),
                    )
                    .child(div().child(value.clone())),
            );
        }
        for note in &details.notes {
            card = card.child(div().child(note.clone()));
        }
        card.child(
            div()
                .text_color(rgb(theme.dim))
                .truncate()
                .child(middle_ellipsis(&details.path, PATH_CHARS)),
        )
    }
}

/// Tooltip builder for one bar. It holds the app and the bar's id only; the
/// card reads the bar when the pointer rests on it.
pub(super) fn clip_tip(
    app: Entity<AlignApp>,
    bar_id: String,
    theme: &Theme,
) -> impl Fn(&mut Window, &mut App) -> AnyView + use<> {
    let theme = *theme;
    move |_, cx| -> AnyView {
        let (app, bar_id) = (app.clone(), bar_id.clone());
        cx.new(|_| ClipCard { app, bar_id, theme }).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Operation;
    use align_core::{
        AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MatchSummary, MediaKind,
        MediaTime, SyncIsland, SyncProject, SyncResult, TimeMap, VideoSummary,
    };
    use std::path::{Path, PathBuf};

    fn card(data: &AppData, bar: &str) -> ClipDetails {
        ClipDetails::of(data, bar).unwrap_or_else(|| panic!("no bar {bar}"))
    }

    fn row<'a>(card: &'a ClipDetails, label: &str) -> Option<&'a str> {
        card.rows
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| value.as_str())
    }

    fn timecode(label: &str) -> Option<SourceTimecode> {
        SourceTimecode::from_label(label, MediaTime::new(1, 25))
    }

    /// The camera-clock shoot with C1 running 12 ppm fast and C3 carrying
    /// its camera's timecode.
    fn shoot() -> SyncResult {
        let mut shoot = crate::project::tests::camera_clock_shoot(Path::new("/shoot"));
        let fast = |placement: &mut ClipPlacement| {
            if placement.clip_id.0 == "C1" {
                placement.mapping.points[1].island = MediaTime::seconds(160.0 + 60.0 * 12e-6);
            }
        };
        shoot
            .islands
            .iter_mut()
            .flat_map(|island| &mut island.placements)
            .for_each(fast);
        shoot
            .stages
            .iter_mut()
            .flat_map(|stage| &mut stage.islands)
            .flat_map(|island| &mut island.placements)
            .for_each(fast);
        let c3 = &mut shoot.project.clips[3];
        assert_eq!(c3.id.0, "C3");
        c3.video.as_mut().unwrap().source_timecode = timecode("14:30:00:00");
        shoot
    }

    #[test]
    fn cards_state_how_each_clip_was_placed_and_only_measured_values() {
        let mut data = AppData::default();
        data.apply_result(shoot());

        let waveform = card(&data, "C1");
        assert_eq!(waveform.method, "Synchronized by waveform");
        assert_eq!(waveform.icon, Some("icons/link.svg"));
        assert_eq!(row(&waveform, "Confidence"), Some("90%"));
        assert_eq!(row(&waveform, "Drift"), Some("12 ppm"));
        assert_eq!(
            row(&waveform, "Timeline"),
            Some("00:01:40 – 00:02:40 · 0:01:00")
        );
        assert_eq!(row(&waveform, "Source TC"), None);
        assert_eq!(waveform.path.replace('\\', "/"), "/shoot/C1.MP4");
        data.ruler_timecode = Some(3603.6);
        assert_eq!(
            row(&card(&data, "C1"), "Timeline"),
            Some("01:01:43.600 – 01:02:43.600 · 0:01:00")
        );

        // Timecode's fixed confidence and its absent drift are not data.
        let timecoded = card(&data, "C3");
        assert_eq!(timecoded.method, "Synchronized by timecode");
        assert_eq!(row(&timecoded, "Confidence"), None);
        assert_eq!(row(&timecoded, "Drift"), None);
        assert_eq!(row(&timecoded, "Source TC"), Some("14:30:00:00"));
        assert!(timecoded.notes.is_empty());

        // The waveform stage leaves C3 unmatched, with an estimate to offer.
        assert!(data.select_sync_stage(0));
        let unmatched = card(&data, "C3");
        assert_eq!(unmatched.method, "Not synchronized");
        assert_eq!(unmatched.icon, Some("icons/warn.svg"));
        assert_eq!(row(&unmatched, "Confidence"), None);
        assert_eq!(
            unmatched.notes,
            ["Camera clock can place it between C2.MP4 and C4.MP4, within about ±1.4 s."]
        );

        assert!(data.set_camera_clock(&[ClipId::new("C3")], true));
        let placed = card(&data, "C3");
        assert_eq!(placed.name, "[CAMERA CLOCK] C3.MP4");
        assert_eq!(placed.method, "Placed approximately by camera clock");
        assert_eq!(placed.icon, Some("icons/clock.svg"));
        assert_eq!(
            placed.notes,
            ["Between C2.MP4 and C4.MP4, within about ±1.4 s. Not synchronized."]
        );
        assert_eq!(row(&placed, "Source TC"), Some("14:30:00:00"));
    }

    /// An imported edit plays one camera file twice against a recorder
    /// synchronized by timecode. The recorder also has a weak waveform
    /// candidate with a clip the solve left unmatched.
    #[test]
    fn imported_cards_tell_uses_of_one_source_apart() {
        let file = |name: &str, kind: MediaKind, label: Option<&str>| Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/edit/{name}")),
            kind,
            duration: MediaTime::seconds(60.0),
            audio: vec![AudioSummary {
                sample_rate: 48_000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: (kind == MediaKind::Video).then(|| VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: label.and_then(timecode),
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let placement = |name: &str| ClipPlacement {
            clip_id: ClipId::new(name),
            mapping: TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(0.0),
                    },
                    MappingPoint {
                        source: MediaTime::seconds(60.0),
                        island: MediaTime::seconds(60.0),
                    },
                ],
            },
            confidence: 0.95,
        };
        let pair = |left: &str, right: &str, confidence, evidence| MatchSummary {
            left: ClipId::new(left),
            right: ClipId::new(right),
            drift_ppm: 0.0,
            offset: MediaTime::seconds(0.0),
            confidence,
            anchors: 1,
            covered: MediaTime::seconds(60.0),
            residual: MediaTime::seconds(0.0),
            evidence: Some(evidence),
        };
        let edit = |id: &str, clip: &str, kind, track, source: (f64, f64), at: f64| {
            align_core::TimelineEdit {
                id: id.into(),
                name: None,
                clip_id: ClipId::new(clip),
                media_type: kind,
                source_in: MediaTime::seconds(source.0),
                source_out: MediaTime::seconds(source.1),
                timeline_start: MediaTime::seconds(at),
                timeline_end: MediaTime::seconds(at + source.1 - source.0),
                retime: Default::default(),
                fcp7: Default::default(),
                audio_source_channel: None,
                fcpxml_audio_role: None,
                track_index: track,
                audio_track_index: None,
                flags: Default::default(),
                audio_enabled: None,
                audio_track_enabled: None,
                audio_track_locked: None,
                transition_after: None,
                audio_transition_after: None,
                linked_audio_edit: None,
            }
        };
        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: vec![
                    file("A.MOV", MediaKind::Video, Some("10:00:00:00")),
                    file("R.WAV", MediaKind::Audio, None),
                    file("X.WAV", MediaKind::Audio, None),
                ],
                warnings: Vec::new(),
                imported_timeline: Some(align_core::ImportedTimeline {
                    aaf_track_numbers: Vec::new(),
                    name: "Edit".into(),
                    frame_duration: MediaTime::new(1, 25),
                    edits: vec![
                        edit("a1", "A.MOV", MediaKind::Video, 1, (0.0, 10.0), 0.0),
                        edit("a2", "A.MOV", MediaKind::Video, 1, (20.0, 30.0), 15.0),
                        edit("r1", "R.WAV", MediaKind::Audio, 1, (0.0, 30.0), 0.0),
                        edit("x1", "X.WAV", MediaKind::Audio, 2, (0.0, 60.0), 40.0),
                    ],
                }),
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement("A.MOV"), placement("R.WAV")],
            }],
            unmatched: vec![ClipId::new("X.WAV")],
            matches: vec![
                pair("A.MOV", "R.WAV", 0.95, MatchEvidence::Timecode),
                pair("R.WAV", "X.WAV", 0.2, MatchEvidence::Waveform),
            ],
            temporal_policy: Default::default(),
        };
        let mut data = AppData::default();
        data.apply_result(result);

        let first = card(&data, "imported-video-a1");
        let second = card(&data, "imported-video-a2");
        for use_of in [&first, &second] {
            assert_eq!(use_of.name, "A.MOV");
            assert_eq!(use_of.method, "Synchronized by timecode");
            assert_eq!(row(use_of, "Confidence"), None);
        }
        assert_eq!(row(&first, "Source TC"), Some("10:00:00:00"));
        assert_eq!(row(&first, "Source"), Some("0:00:00 – 0:00:10 of 0:01:00"));
        assert_eq!(row(&first, "Use"), Some("1 of 2 on this timeline"));
        assert_eq!(row(&second, "Source TC"), Some("10:00:20:00"));
        assert_eq!(row(&second, "Source"), Some("0:00:20 – 0:00:30 of 0:01:00"));
        assert_eq!(row(&second, "Use"), Some("2 of 2 on this timeline"));
        assert_ne!(row(&first, "Timeline"), row(&second, "Timeline"));

        // The candidate to a clip outside the group did not place the
        // recorder, so its bar still reads as synchronized by timecode.
        let recorder = card(&data, "imported-audio-r1");
        assert_eq!(recorder.method, "Synchronized by timecode");
        assert_eq!(row(&recorder, "Use"), None);
        let bar = |id: &str| {
            data.lanes
                .iter()
                .flat_map(|lane| &lane.clips)
                .find(|bar| bar.id == id)
                .unwrap()
                .evidence
        };
        assert_eq!(bar("imported-audio-r1"), Some(MatchEvidence::Timecode));
        let unlinked = card(&data, "imported-audio-x1");
        assert_eq!(unlinked.method, "Not synchronized");
        assert_eq!(bar("imported-audio-x1"), None);
    }

    #[test]
    fn cards_during_analysis_claim_no_place_or_match() {
        use align_core::{MatchPreview, MatchPreviewStage};
        let clip = |name: &str, label: Option<&str>| Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/live/{name}")),
            kind: MediaKind::Video,
            duration: MediaTime::seconds(42.0),
            audio: Vec::new(),
            video: Some(VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: label.and_then(timecode),
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let clips = [
            clip("a.mov", Some("09:59:58:00")),
            clip("b.mov", None),
            clip("c.mov", None),
        ];
        let mut data = AppData::default();
        data.inputs = clips.iter().map(|clip| clip.url.clone()).collect();
        data.operation = Operation::Synchronizing;
        data.begin_sequence_progress();
        for clip in &clips {
            data.apply_progress("inspect", 0, 3, None, Some(clip), None);
        }
        let preview = MatchPreview {
            left: ClipId::new("a.mov"),
            right: ClipId::new("b.mov"),
            rate: 1.0,
            offset: 2.0,
            confidence: 0.87,
            stage: MatchPreviewStage::Refined,
        };
        data.apply_progress("refine", 0, 1, None, None, Some(&preview));

        let refined = card(&data, "a.mov");
        assert_eq!(refined.method, "Analyzing: match found, not confirmed yet");
        assert_eq!(refined.icon, None);
        assert_eq!(
            refined.rows,
            [
                ("Duration", "0:00:42".to_string()),
                ("Source TC", "09:59:58:00".to_string())
            ]
        );
        let pending = card(&data, "c.mov");
        assert_eq!(pending.method, "Analyzing: place not confirmed yet");
        assert_eq!(pending.rows, [("Duration", "0:00:42".to_string())]);
    }
}
