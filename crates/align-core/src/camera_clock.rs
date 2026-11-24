//! Approximate placement of unmatched clips by their camera's own clock.
//!
//! A camera that records its serial number and the time each recording
//! started says how far apart two of its recordings began. When an unmatched
//! clip started, on that clock, between two clips of the same camera that
//! sound synchronized in one group, its offset from the camera clock is
//! interpolated between theirs. The result is an estimate, not a match:
//! nothing here is saved with a result, every estimate is recomputed from the
//! active stage and its time-source policy, and an estimate is used only where
//! the user applies it.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::model::{
    Clip, ClipId, ImportedTimeline, MatchEvidence, MediaKind, RecordingTimestampSource,
    SyncConstraint, SyncResult, TemporalMode, file_name,
};
use crate::piecewise::{MapPoint, PiecewiseTimeMapping};

/// Recording times are whole seconds. Two times rounded the same way differ
/// from the true difference by less than this.
pub const STAMP_RESOLUTION: f64 = 1.0;

/// The largest difference between the clock offsets of the two anchors that
/// still counts as agreement: one second of rounding, and one more for
/// varying start latency and clock drift (50 ppm accumulates one second in
/// five and a half hours). Larger differences mean the clock was changed or
/// a synchronization is wrong, so nothing between them is estimated.
pub const ANCHOR_AGREEMENT: f64 = 2.0;

/// Where the camera clock puts one unmatched clip.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraClockEstimate {
    pub clip_id: ClipId,
    /// The synchronization group of both anchors.
    pub island: usize,
    /// The clip's first sample in the group's coordinates, in seconds.
    pub start: f64,
    /// The nearest clips of the same camera recorded before and after it,
    /// both synchronized by sound.
    pub before: ClipId,
    pub after: ClipId,
    /// Expected bound on the error of `start`, in seconds: the rounding of
    /// whole-second recording times plus the disagreement between the two
    /// anchors. An expectation, not a guarantee.
    pub tolerance: f64,
}

/// Why an unmatched clip with a camera serial number has no estimate.
#[derive(Clone, Debug, PartialEq)]
pub enum CameraClockRefusal {
    /// Its time source is Timecode, which excludes the recording time.
    TimecodeOnly,
    /// The camera recorded no start time, or only the file date is known.
    NoRecordingTime,
    /// No clip of the camera synchronized by sound on that side of it.
    NoAnchor { before: bool, after: bool },
    /// The camera's clips around it are in different groups.
    AmbiguousGroup,
    /// The anchors' synchronized positions disagree with the camera clock.
    AnchorConflict {
        before: ClipId,
        after: ClipId,
        disagreement: f64,
    },
    /// By the clock it would overlap one of the anchors.
    OutsideGap { before: ClipId, after: ClipId },
    /// It would overlap another synchronized clip of the same camera.
    OverlapsSynchronized(ClipId),
    /// It would overlap another estimated clip of the same camera.
    OverlapsEstimate(ClipId),
    /// The user rejected its pairing or alignment with this clip.
    RejectedAlignment(ClipId),
}

impl CameraClockRefusal {
    /// A sentence for the user, naming clips with `name`.
    pub fn describe(&self, name: &dyn Fn(&ClipId) -> String) -> String {
        match self {
            Self::TimecodeOnly => {
                "Its time source is Timecode, so the recording time is not used.".into()
            }
            Self::NoRecordingTime => "The camera recorded no start time for it.".into(),
            Self::NoAnchor { before, after } => {
                let side = match (before, after) {
                    (true, true) => "before or after it",
                    (true, false) => "before it",
                    _ => "after it",
                };
                format!("No clip of this camera recorded {side} is synchronized by sound.")
            }
            Self::AmbiguousGroup => {
                "This camera's clips recorded around it are in different groups.".into()
            }
            Self::AnchorConflict {
                before,
                after,
                disagreement,
            } => format!(
                "The camera clock disagrees with the synchronized {} and {} by {disagreement:.1} s.",
                name(before),
                name(after)
            ),
            Self::OutsideGap { before, after } => format!(
                "By the camera clock it does not fit between {} and {}.",
                name(before),
                name(after)
            ),
            Self::OverlapsSynchronized(other) => format!(
                "It would overlap {}, synchronized from the same camera.",
                name(other)
            ),
            Self::OverlapsEstimate(other) => format!(
                "It would overlap {}, also placed by the camera clock.",
                name(other)
            ),
            Self::RejectedAlignment(other) => {
                format!("Its alignment with {} was rejected.", name(other))
            }
        }
    }
}

/// Estimates and refusals for every unmatched clip with a camera serial
/// number, in the order of the result's unmatched clips.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraClockReport {
    pub estimates: Vec<CameraClockEstimate>,
    pub refusals: Vec<(ClipId, CameraClockRefusal)>,
}

impl CameraClockReport {
    pub fn estimate(&self, clip: &ClipId) -> Option<&CameraClockEstimate> {
        self.estimates.iter().find(|e| &e.clip_id == clip)
    }

    pub fn refusal(&self, clip: &ClipId) -> Option<&CameraClockRefusal> {
        self.refusals
            .iter()
            .find_map(|(id, refusal)| (id == clip).then_some(refusal))
    }

    /// The estimates of the chosen clips. A choice without an estimate in
    /// this report applies nothing.
    pub fn chosen(&self, clips: &BTreeSet<ClipId>) -> Vec<CameraClockEstimate> {
        self.estimates
            .iter()
            .filter(|e| clips.contains(&e.clip_id))
            .cloned()
            .collect()
    }
}

/// How an item placed by the camera clock appears in an assembled timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraClockMark {
    pub before: ClipId,
    pub after: ClipId,
    pub tolerance: f64,
    /// Where the ordinary assembly put this clip on the combined timeline.
    /// Tracks are allocated as if it were still there, then only this clip
    /// moves, so no other clip changes track. `None` before combining.
    pub assembled_start: Option<f64>,
}

/// Prefix of the names of exported clips placed by the camera clock.
pub const CAMERA_CLOCK_LABEL: &str = "[CAMERA CLOCK]";

/// A placed clip of some camera, in its group's coordinates.
struct Placed<'a> {
    clip: &'a Clip,
    island: usize,
    start: f64,
    end: f64,
    /// Start on the camera clock, when the policy allows it.
    clock: Option<f64>,
    /// Synchronized by sound with another clip of its group.
    sound: bool,
}

/// The camera-clock start of a clip: its embedded recording time read by
/// its time-source mode. File dates and Timecode never count.
fn clock_start(result: &SyncResult, clip: &Clip) -> Option<f64> {
    let policy = &result.temporal_policy;
    if policy.resolve(&clip.id) == TemporalMode::Timecode
        || clip.recorded_at_source != Some(RecordingTimestampSource::EmbeddedMetadata)
    {
        return None;
    }
    policy.anchors(clip).0
}

fn overlaps(a: (f64, f64), b: (f64, f64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Estimates every unmatched clip of a camera with a serial number from the
/// active stage of `result`. `constraints` are the user's rejected pairs and
/// alignments for this sequence.
pub fn camera_clock(result: &SyncResult, constraints: &[SyncConstraint]) -> CameraClockReport {
    let clips: HashMap<&ClipId, &Clip> = result.project.clips.iter().map(|c| (&c.id, c)).collect();
    let island_of: HashMap<&ClipId, usize> = result
        .islands
        .iter()
        .flat_map(|island| island.placements.iter().map(|p| (&p.clip_id, island.id)))
        .collect();
    let sound: HashSet<&ClipId> = result
        .matches
        .iter()
        .filter(|m| m.evidence.is_none_or(|e| e == MatchEvidence::Waveform))
        .filter(|m| {
            island_of
                .get(&m.left)
                .is_some_and(|island| island_of.get(&m.right) == Some(island))
        })
        .flat_map(|m| [&m.left, &m.right])
        .collect();
    let placed: Vec<Placed> = result
        .islands
        .iter()
        .flat_map(|island| island.placements.iter().map(move |p| (island.id, p)))
        .filter_map(|(island, placement)| {
            let clip = *clips.get(&placement.clip_id)?;
            let mapping = PiecewiseTimeMapping::new(
                placement
                    .mapping
                    .points
                    .iter()
                    .map(|p| MapPoint::new(p.source.as_seconds(), p.island.as_seconds()))
                    .collect(),
            );
            Some(Placed {
                clip,
                island,
                start: mapping.value_at(0.0),
                end: mapping.value_at(clip.duration.as_seconds()),
                clock: clock_start(result, clip),
                sound: sound.contains(&clip.id),
            })
        })
        .collect();

    let mut report = CameraClockReport::default();
    for clip in result.unmatched.iter().filter_map(|id| clips.get(id)) {
        let Some(camera) = clip.source_identifier.as_deref() else {
            continue;
        };
        let camera_clips: Vec<&Placed> = placed
            .iter()
            .filter(|p| p.clip.source_identifier.as_deref() == Some(camera))
            .collect();
        match estimate_one(result, constraints, clip, &camera_clips, &placed) {
            Ok(estimate) => report.estimates.push(estimate),
            Err(refusal) => report.refusals.push((clip.id.clone(), refusal)),
        }
    }

    // Two estimates of one camera cannot both be right where they overlap.
    let spans: Vec<(usize, (f64, f64))> = report
        .estimates
        .iter()
        .map(|e| {
            let duration = clips[&e.clip_id].duration.as_seconds();
            (e.island, (e.start, e.start + duration))
        })
        .collect();
    let camera = |id: &ClipId| clips[id].source_identifier.as_deref();
    let mut overlapping: Vec<(usize, ClipId)> = Vec::new();
    for (i, a) in report.estimates.iter().enumerate() {
        let other = report.estimates.iter().enumerate().find(|(j, b)| {
            *j != i
                && camera(&a.clip_id) == camera(&b.clip_id)
                && spans[i].0 == spans[*j].0
                && overlaps(spans[i].1, spans[*j].1)
        });
        if let Some((_, b)) = other {
            overlapping.push((i, b.clip_id.clone()));
        }
    }
    for (index, other) in overlapping.iter().rev() {
        let estimate = report.estimates.remove(*index);
        report.refusals.push((
            estimate.clip_id,
            CameraClockRefusal::OverlapsEstimate(other.clone()),
        ));
    }
    let order: HashMap<&ClipId, usize> = result
        .unmatched
        .iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    report
        .refusals
        .sort_by_key(|(id, _)| order.get(id).copied());
    report
}

fn estimate_one(
    result: &SyncResult,
    constraints: &[SyncConstraint],
    clip: &Clip,
    camera_clips: &[&Placed],
    placed: &[Placed],
) -> Result<CameraClockEstimate, CameraClockRefusal> {
    if result.temporal_policy.resolve(&clip.id) == TemporalMode::Timecode {
        return Err(CameraClockRefusal::TimecodeOnly);
    }
    let clock = clock_start(result, clip).ok_or(CameraClockRefusal::NoRecordingTime)?;
    let anchors = camera_clips
        .iter()
        .filter_map(|p| Some((p.clock?, *p)))
        .filter(|(_, p)| p.sound);
    let before = anchors
        .clone()
        .filter(|(at, _)| *at <= clock)
        .max_by(|a, b| a.0.total_cmp(&b.0));
    let after = anchors
        .filter(|(at, _)| *at > clock)
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let (Some((before_clock, before)), Some((after_clock, after))) = (before, after) else {
        return Err(CameraClockRefusal::NoAnchor {
            before: before.is_none(),
            after: after.is_none(),
        });
    };
    let island = before.island;
    let elsewhere = camera_clips.iter().any(|p| {
        p.island != island
            && p.clock
                .is_some_and(|at| (before_clock..=after_clock).contains(&at))
    });
    if after.island != island || elsewhere {
        return Err(CameraClockRefusal::AmbiguousGroup);
    }
    // How much more time sound puts between the anchors than the camera
    // clock does: zero for a clock that runs with the recorder's.
    // Differences keep the arithmetic far from the magnitude of Unix times.
    let elapsed = clock - before_clock;
    let drift = (after.start - before.start) - (after_clock - before_clock);
    let disagreement = drift.abs();
    if disagreement > ANCHOR_AGREEMENT {
        return Err(CameraClockRefusal::AnchorConflict {
            before: before.clip.id.clone(),
            after: after.clip.id.clone(),
            disagreement,
        });
    }
    let start = before.start + elapsed + drift * elapsed / (after_clock - before_clock);
    let span = (start, start + clip.duration.as_seconds());
    if span.0 < before.end || span.1 > after.start {
        return Err(CameraClockRefusal::OutsideGap {
            before: before.clip.id.clone(),
            after: after.clip.id.clone(),
        });
    }
    if let Some(other) = camera_clips
        .iter()
        .find(|p| p.island == island && overlaps(span, (p.start, p.end)))
    {
        return Err(CameraClockRefusal::OverlapsSynchronized(
            other.clip.id.clone(),
        ));
    }
    // A rejected pairing with an anchor or with any clip it would play
    // alongside, or a rejected alignment at this offset, rules it out.
    for other in placed.iter().filter(|p| p.island == island) {
        let alongside = overlaps(span, (other.start, other.end));
        let anchor = other.clip.id == before.clip.id || other.clip.id == after.clip.id;
        let rejected = ((alongside || anchor)
            && SyncConstraint::rejects_pair(constraints, &clip.id, &other.clip.id))
            || (alongside
                && SyncConstraint::rejects_alignment(
                    constraints,
                    &other.clip.id,
                    &clip.id,
                    other.start - start,
                ));
        if rejected {
            return Err(CameraClockRefusal::RejectedAlignment(other.clip.id.clone()));
        }
    }
    Ok(CameraClockEstimate {
        clip_id: clip.id.clone(),
        island,
        start,
        before: before.clip.id.clone(),
        after: after.clip.id.clone(),
        tolerance: STAMP_RESOLUTION + disagreement,
    })
}

/// An imported edit of a clip with an estimate: where its first frame
/// plays in the imported sequence, and where the camera clock would put
/// that frame. Diagnostic only; the edit keeps its placement.
#[derive(Clone, Debug, PartialEq)]
pub struct EditClockHint {
    pub clip_id: ClipId,
    pub edit_id: String,
    pub media: MediaKind,
    /// Zero-based imported track.
    pub track_index: usize,
    /// Seconds in the imported sequence's own time.
    pub timeline_start: f64,
    pub expected_start: f64,
    pub tolerance: f64,
}

impl EditClockHint {
    /// Positive when the edit plays later than the camera clock suggests.
    pub fn deviation(&self) -> f64 {
        self.timeline_start - self.expected_start
    }

    pub fn agrees(&self) -> bool {
        self.deviation().abs() <= self.tolerance
    }

    /// `V1`, `A2`: the track as the editor numbers it.
    pub fn track_label(&self) -> String {
        let kind = match self.media {
            MediaKind::Video => 'V',
            MediaKind::Audio => 'A',
        };
        format!("{kind}{}", self.track_index + 1)
    }
}

/// One played side of an imported edit: its media, track, and the frame
/// at its timeline start.
struct EditSide<'a> {
    clip: &'a ClipId,
    id: &'a str,
    media: MediaKind,
    track: usize,
    source_in: f64,
    timeline_start: f64,
    backward: bool,
}

fn edit_sides(imported: &ImportedTimeline) -> Vec<EditSide<'_>> {
    let mut sides = Vec::new();
    for edit in &imported.edits {
        sides.push(EditSide {
            clip: &edit.clip_id,
            id: &edit.id,
            media: edit.media_type,
            track: edit.track_index,
            source_in: edit.source_in.as_seconds(),
            timeline_start: edit.timeline_start.as_seconds(),
            backward: edit.retime.plays_backward,
        });
        if let Some(audio) = &edit.linked_audio_edit {
            sides.push(EditSide {
                clip: &edit.clip_id,
                id: &audio.id,
                media: MediaKind::Audio,
                track: audio.track_index,
                source_in: audio.source_in.as_seconds(),
                timeline_start: audio.timeline_start.as_seconds(),
                backward: audio.retime.plays_backward,
            });
        }
    }
    sides
}

/// Hints for the imported edits of estimated clips. A track relates to a
/// group through its synchronized edits of that group's clips: each says
/// where the group's time is on the track. A hint is given only where the
/// track has such edits and they agree within a frame; an edit played
/// backward starts on its last frame and gets none.
pub fn imported_edit_hints(
    result: &SyncResult,
    estimates: &[CameraClockEstimate],
) -> Vec<EditClockHint> {
    let Some(imported) = &result.project.imported_timeline else {
        return Vec::new();
    };
    let unmatched: HashSet<&ClipId> = result.unmatched.iter().collect();
    let mappings: HashMap<&ClipId, (usize, PiecewiseTimeMapping)> = result
        .islands
        .iter()
        .flat_map(|island| island.placements.iter().map(move |p| (island.id, p)))
        .map(|(island, placement)| {
            let mapping = PiecewiseTimeMapping::new(
                placement
                    .mapping
                    .points
                    .iter()
                    .map(|p| MapPoint::new(p.source.as_seconds(), p.island.as_seconds()))
                    .collect(),
            );
            (&placement.clip_id, (island, mapping))
        })
        .collect();
    let sides = edit_sides(imported);
    // Group time minus sequence time, per (media, track, group).
    let mut offsets: HashMap<(MediaKind, usize, usize), Vec<f64>> = HashMap::new();
    for side in sides.iter().filter(|side| !side.backward) {
        if unmatched.contains(side.clip) {
            continue;
        }
        if let Some((island, mapping)) = mappings.get(side.clip) {
            offsets
                .entry((side.media, side.track, *island))
                .or_default()
                .push(mapping.value_at(side.source_in) - side.timeline_start);
        }
    }
    let frame = imported.frame_duration.as_seconds();
    let mut hints = Vec::new();
    for estimate in estimates {
        for side in sides
            .iter()
            .filter(|s| *s.clip == estimate.clip_id && !s.backward)
        {
            let Some(values) = offsets.get(&(side.media, side.track, estimate.island)) else {
                continue;
            };
            let (low, high) = values
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| {
                    (l.min(*v), h.max(*v))
                });
            if high - low > frame + crate::model::TIME_TOLERANCE {
                continue;
            }
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            let offset = sorted[sorted.len() / 2];
            hints.push(EditClockHint {
                clip_id: estimate.clip_id.clone(),
                edit_id: side.id.to_string(),
                media: side.media,
                track_index: side.track,
                timeline_start: side.timeline_start,
                expected_start: estimate.start + side.source_in - offset,
                tolerance: estimate.tolerance,
            });
        }
    }
    hints
}

/// A short description of an estimate for the user.
pub fn describe_estimate(
    estimate: &CameraClockEstimate,
    name: &dyn Fn(&ClipId) -> String,
) -> String {
    format!(
        "Between {} and {} by the camera clock, within about ±{:.1} s.",
        name(&estimate.before),
        name(&estimate.after),
        estimate.tolerance
    )
}

/// File names of a result's clips, for messages.
pub fn clip_names(result: &SyncResult) -> HashMap<ClipId, String> {
    result
        .project
        .clips
        .iter()
        .map(|clip| (clip.id.clone(), file_name(&clip.url)))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{
        ClipPlacement, MappingPoint, MatchSummary, MediaTime, SyncIsland, SyncProject,
        TemporalPolicy, TimeMap,
    };
    use std::path::PathBuf;

    pub(crate) const CAMERA: &str = "device:Sony:ILME-FX3:4711";
    /// 2025-03-04 10:00:00 UTC.
    pub(crate) const T0: i64 = 1_741_082_400;

    pub(crate) fn camera_clip(
        name: &str,
        camera: Option<&str>,
        recorded: i64,
        duration: f64,
    ) -> Clip {
        Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/card/{name}.MP4")),
            kind: MediaKind::Video,
            duration: MediaTime::seconds(duration),
            audio: vec![crate::model::AudioSummary {
                sample_rate: 48_000.0,
                channels: 2,
                bit_depth: Some(16),
                is_float: Some(false),
                source_timecode: None,
            }],
            video: Some(crate::model::VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            }),
            recorded_at: Some(recorded),
            recorded_at_source: Some(RecordingTimestampSource::EmbeddedMetadata),
            source_identifier: camera.map(str::to_string),
            media_span: None,
        }
    }

    pub(crate) fn recorder(name: &str, duration: f64) -> Clip {
        Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/recorder/{name}.WAV")),
            kind: MediaKind::Audio,
            video: None,
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            ..camera_clip(name, None, 0, duration)
        }
    }

    pub(crate) fn placement(clip: &Clip, start: f64) -> ClipPlacement {
        let duration = clip.duration.as_seconds();
        ClipPlacement {
            clip_id: clip.id.clone(),
            mapping: TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(start),
                    },
                    MappingPoint {
                        source: MediaTime::seconds(duration),
                        island: MediaTime::seconds(start + duration),
                    },
                ],
            },
            confidence: 0.9,
        }
    }

    pub(crate) fn sound(left: &Clip, right: &Clip, evidence: MatchEvidence) -> MatchSummary {
        MatchSummary {
            left: left.id.clone(),
            right: right.id.clone(),
            drift_ppm: 0.0,
            offset: MediaTime::seconds(0.0),
            confidence: 0.9,
            anchors: 3,
            covered: MediaTime::seconds(10.0),
            residual: MediaTime::seconds(0.0),
            evidence: Some(evidence),
        }
    }

    /// Groups of `(clip, group start)`; every camera clip of a group matches
    /// its first clip by sound.
    pub(crate) fn result(
        groups: Vec<Vec<(Clip, f64)>>,
        unmatched: Vec<Clip>,
        temporal: TemporalPolicy,
    ) -> SyncResult {
        let mut clips = Vec::new();
        let mut islands = Vec::new();
        let mut matches = Vec::new();
        for (id, group) in groups.into_iter().enumerate() {
            let first = group[0].0.clone();
            for (clip, _) in group.iter().skip(1) {
                matches.push(sound(&first, clip, MatchEvidence::Waveform));
            }
            islands.push(SyncIsland {
                id,
                placements: group.iter().map(|(c, s)| placement(c, *s)).collect(),
            });
            clips.extend(group.into_iter().map(|(clip, _)| clip));
        }
        let unmatched_ids = unmatched.iter().map(|c| c.id.clone()).collect();
        clips.extend(unmatched);
        SyncResult {
            search_overrides: HashMap::new(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips,
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands,
            unmatched: unmatched_ids,
            matches,
            temporal_policy: temporal,
        }
    }

    /// A recorder takes the whole shoot; the camera's C1 at 100 s, C2 at
    /// 400 s and C4 at 900 s are synchronized, and C3 (recorded 600 s after
    /// C1) is unmatched. The camera clock runs 0.4 s late by C4.
    pub(crate) fn shoot() -> SyncResult {
        let c1 = camera_clip("C1", Some(CAMERA), T0, 60.0);
        let c2 = camera_clip("C2", Some(CAMERA), T0 + 300, 60.0);
        let c4 = camera_clip("C4", Some(CAMERA), T0 + 800, 60.0);
        let c3 = camera_clip("C3", Some(CAMERA), T0 + 600, 90.0);
        result(
            vec![vec![
                (recorder("R", 1200.0), 0.0),
                (c1, 100.0),
                (c2, 400.0),
                (c4, 899.6),
            ]],
            vec![c3],
            TemporalPolicy::default(),
        )
    }

    #[test]
    fn interpolates_between_the_nearest_sound_anchors() {
        let report = camera_clock(&shoot(), &[]);
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let estimate = &report.estimates[0];
        assert_eq!(
            (&estimate.before.0[..], &estimate.after.0[..]),
            ("C2", "C4")
        );
        // C2's offset is 100 s, C4's 99.6 s; C3 is 60 % of the way.
        assert!((estimate.start - (600.0 + 100.0 - 0.4 * 0.6)).abs() < 1e-9);
        assert!((estimate.tolerance - 1.4).abs() < 1e-9);
    }

    #[test]
    fn rec_stop_reads_the_time_as_the_end_of_each_clip() {
        let mut result = shoot();
        // Stamps written when recording stopped: each one duration later.
        for clip in &mut result.project.clips {
            if let Some(at) = clip.recorded_at.as_mut() {
                *at += clip.duration.as_seconds() as i64;
            }
        }
        // Read as start times, C3's longer take lands 30 s late.
        let misread = camera_clock(&result, &[]);
        assert!((misread.estimates[0].start - 729.736).abs() < 1e-9);
        result.temporal_policy.default = TemporalMode::RecStop;
        let report = camera_clock(&result, &[]);
        assert!((report.estimates[0].start - 699.76).abs() < 1e-9);
        // A per-clip override is the clip's mode.
        result.temporal_policy.default = TemporalMode::Auto;
        for id in ["C1", "C2", "C3", "C4"] {
            result
                .temporal_policy
                .modes
                .insert(ClipId::new(id), TemporalMode::RecStop);
        }
        assert_eq!(camera_clock(&result, &[]), report);
    }

    #[test]
    fn refuses_without_reliable_evidence() {
        fn c3(result: &mut SyncResult) -> &mut Clip {
            result
                .project
                .clips
                .iter_mut()
                .find(|c| c.id.0 == "C3")
                .unwrap()
        }
        fn clip<'a>(result: &'a mut SyncResult, name: &str) -> &'a mut Clip {
            result
                .project
                .clips
                .iter_mut()
                .find(|c| c.id.0 == name)
                .unwrap()
        }
        type Change = fn(&mut SyncResult);
        let cases: Vec<(&str, Change, CameraClockRefusal)> = vec![
            (
                "timecode-only",
                |r| {
                    r.temporal_policy
                        .modes
                        .insert(ClipId::new("C3"), TemporalMode::Timecode);
                },
                CameraClockRefusal::TimecodeOnly,
            ),
            (
                "file date",
                |r| c3(r).recorded_at_source = Some(RecordingTimestampSource::FileSystem),
                CameraClockRefusal::NoRecordingTime,
            ),
            (
                "anchor date excluded by timecode mode",
                |r| {
                    r.temporal_policy
                        .modes
                        .insert(ClipId::new("C4"), TemporalMode::Timecode);
                },
                CameraClockRefusal::NoAnchor {
                    before: false,
                    after: true,
                },
            ),
            (
                "anchor placed only by timecode",
                |r| {
                    r.matches.retain(|m| m.right.0 != "C4");
                    let (recorder, c4) = (clip(r, "R").clone(), clip(r, "C4").clone());
                    r.matches
                        .push(sound(&recorder, &c4, MatchEvidence::Timecode));
                },
                CameraClockRefusal::NoAnchor {
                    before: false,
                    after: true,
                },
            ),
            (
                "camera clock changed between anchors",
                |r| clip(r, "C4").recorded_at = Some(T0 + 805),
                CameraClockRefusal::AnchorConflict {
                    before: ClipId::new("C2"),
                    after: ClipId::new("C4"),
                    disagreement: 5.4,
                },
            ),
            (
                "does not fit the gap",
                |r| c3(r).duration = MediaTime::seconds(320.0),
                CameraClockRefusal::OutsideGap {
                    before: ClipId::new("C2"),
                    after: ClipId::new("C4"),
                },
            ),
            (
                "same model, other serial",
                |r| c3(r).source_identifier = Some("device:Sony:ILME-FX3:4712".into()),
                CameraClockRefusal::NoAnchor {
                    before: true,
                    after: true,
                },
            ),
        ];
        for (name, change, expected) in cases {
            let mut result = shoot();
            change(&mut result);
            let report = camera_clock(&result, &[]);
            assert!(report.estimates.is_empty(), "{name}");
            let refusal = report.refusal(&ClipId::new("C3")).expect(name);
            match (refusal, &expected) {
                (
                    CameraClockRefusal::AnchorConflict { disagreement, .. },
                    CameraClockRefusal::AnchorConflict {
                        disagreement: wanted,
                        ..
                    },
                ) => assert!((disagreement - wanted).abs() < 1e-9, "{name}"),
                _ => assert_eq!(refusal, &expected, "{name}"),
            }
        }
    }

    #[test]
    fn refuses_where_another_group_or_clip_competes() {
        // C2 and C4 in different groups: no group to place C3 in.
        let c1 = camera_clip("C1", Some(CAMERA), T0, 60.0);
        let c2 = camera_clip("C2", Some(CAMERA), T0 + 300, 60.0);
        let c4 = camera_clip("C4", Some(CAMERA), T0 + 800, 60.0);
        let c3 = camera_clip("C3", Some(CAMERA), T0 + 600, 90.0);
        let split = result(
            vec![
                vec![(recorder("R1", 500.0), 0.0), (c1, 100.0), (c2, 400.0)],
                vec![(recorder("R2", 500.0), 0.0), (c4.clone(), 99.6)],
            ],
            vec![c3.clone()],
            TemporalPolicy::default(),
        );
        assert_eq!(
            camera_clock(&split, &[]).refusal(&c3.id),
            Some(&CameraClockRefusal::AmbiguousGroup)
        );

        // A clip of the camera synchronized by timecode in that gap.
        let mut result = shoot();
        let tc = Clip {
            recorded_at_source: None,
            ..camera_clip("TC", Some(CAMERA), 0, 30.0)
        };
        result.project.clips.push(tc.clone());
        result.islands[0].placements.push(placement(&tc, 750.0));
        let report = camera_clock(&result, &[]);
        assert_eq!(
            report.refusal(&c3.id),
            Some(&CameraClockRefusal::OverlapsSynchronized(tc.id.clone()))
        );

        // Two estimates of the camera on top of each other.
        let mut result = shoot();
        let late = camera_clip("C3b", Some(CAMERA), T0 + 650, 30.0);
        result.unmatched.push(late.id.clone());
        result.project.clips.push(late.clone());
        let report = camera_clock(&result, &[]);
        assert!(report.estimates.is_empty());
        assert_eq!(
            report.refusals,
            vec![
                (
                    c3.id.clone(),
                    CameraClockRefusal::OverlapsEstimate(late.id.clone())
                ),
                (late.id.clone(), CameraClockRefusal::OverlapsEstimate(c3.id)),
            ]
        );
    }

    fn edit(
        id: &str,
        clip: &str,
        media: MediaKind,
        track: usize,
        source_in: f64,
        start: f64,
    ) -> crate::model::TimelineEdit {
        crate::model::TimelineEdit {
            id: id.into(),
            name: None,
            clip_id: ClipId::new(clip),
            media_type: media,
            source_in: MediaTime::seconds(source_in),
            source_out: MediaTime::seconds(source_in + 20.0),
            timeline_start: MediaTime::seconds(start),
            timeline_end: MediaTime::seconds(start + 20.0),
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
    }

    /// An edit of the shoot in a sequence starting at one hour, its picture
    /// and sound tracks slipped differently against the group.
    #[test]
    fn imported_hints_compare_the_edit_with_the_clock_in_sequence_time() {
        use crate::model::{ImportedTimeline, TimelineLinkedAudioEdit};
        let mut result = shoot();
        let estimate = camera_clock(&result, &[]).estimates;
        assert_eq!(estimate.len(), 1);
        // V1: C2 from 5 s at 3620 s, so the group's time is 3215 s less.
        // A1: the recorder from 300 s at 3600 s, 3300 s less.
        let mut c3 = edit("c3", "C3", MediaKind::Video, 0, 10.0, 3927.0);
        c3.retime.playback_rate = 0.5;
        let audio = edit("c3-a", "C3", MediaKind::Audio, 0, 10.0, 4009.5);
        c3.linked_audio_edit = Some(TimelineLinkedAudioEdit {
            id: audio.id,
            source_in: audio.source_in,
            source_out: audio.source_out,
            timeline_start: audio.timeline_start,
            timeline_end: audio.timeline_end,
            retime: Default::default(),
            fcp7: Default::default(),
            audio_source_channel: None,
            fcpxml_audio_role: None,
            track_index: 0,
            flags: Default::default(),
            transition_after: None,
        });
        let mut backward = edit("c3-reverse", "C3", MediaKind::Video, 0, 0.0, 4100.0);
        backward.retime.plays_backward = true;
        result.project.imported_timeline = Some(ImportedTimeline {
            name: "Edit".into(),
            frame_duration: MediaTime::new(1, 25),
            edits: vec![
                edit("c2", "C2", MediaKind::Video, 0, 5.0, 3620.0),
                edit("r", "R", MediaKind::Audio, 0, 300.0, 3600.0),
                c3,
                backward,
                // V2 holds no synchronized clip of the group.
                edit("c3-v2", "C3", MediaKind::Video, 1, 0.0, 5000.0),
            ],
        });
        let hints = imported_edit_hints(&result, &estimate);
        let found: Vec<_> = hints
            .iter()
            .map(|h| {
                (
                    h.edit_id.as_str(),
                    h.track_label(),
                    (h.deviation() * 1000.0).round(),
                    h.agrees(),
                )
            })
            .collect();
        // C3 at 699.76 s in the group: its 10 s frame at 3924.76 s on V1
        // and 4009.76 s on A1.
        assert_eq!(
            found,
            vec![
                ("c3", "V1".to_string(), 2240.0, false),
                ("c3-a", "A1".to_string(), -260.0, true),
            ]
        );
    }

    #[test]
    fn estimates_are_not_anchors_for_other_estimates() {
        let mut result = shoot();
        let later = camera_clip("C3c", Some(CAMERA), T0 + 720, 20.0);
        result.unmatched.push(later.id.clone());
        result.project.clips.push(later.clone());
        let report = camera_clock(&result, &[]);
        let later = report.estimate(&later.id).unwrap();
        assert_eq!((&later.before.0[..], &later.after.0[..]), ("C2", "C4"));
    }

    #[test]
    fn honours_rejected_alignments() {
        let result = shoot();
        let (c3, r) = (ClipId::new("C3"), ClipId::new("R"));
        let estimate = camera_clock(&result, &[]).estimates[0].clone();
        let at_estimate =
            SyncConstraint::rejecting_alignment(r.clone(), c3.clone(), -estimate.start);
        let elsewhere =
            SyncConstraint::rejecting_alignment(r.clone(), c3.clone(), -estimate.start + 30.0);
        let pair = SyncConstraint::rejecting_pair(c3.clone(), ClipId::new("C2"));
        for (constraint, refused) in [(at_estimate, true), (elsewhere, false), (pair, true)] {
            let report = camera_clock(&result, std::slice::from_ref(&constraint));
            assert_eq!(report.estimates.is_empty(), refused, "{constraint:?}");
        }
    }
}
