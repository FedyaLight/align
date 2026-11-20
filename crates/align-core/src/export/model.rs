//! Writer-neutral timeline assembly and export options.
//!
//! Transforms solved placements and imported edits into tracks, clips,
//! transitions, and media references consumed by sibling format writers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::allocator::source_key_for_clip;
use crate::model::{
    Clip, ClipId, ImportedTimeline, MappingPoint, MediaKind, MediaTime, RATE_TOLERANCE, SyncIsland,
    SyncResult, TemporalPolicy, TimelineEdit, TimelineLinkedAudioEdit, TimelinePreviewItem,
    TimelineTransitionKind, file_name,
};
use crate::piecewise::{MapPoint, PiecewiseTimeMapping};
use crate::review::{ReviewSession, ReviewValidationIssue, ReviewValidationReason};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportTransitionKind {
    CrossDissolve,
    AudioTransition,
}

/// Sequence frame rate in frames/second for FCP7 placement math.
///
/// Mirrors the writer's NTSC snap exactly (shared so sidecar planning can
/// never disagree with the writer about which integer frame a start floors
/// to): rates within 0.02 of a standard NTSC rate use base/1.001.
pub fn sequence_fps(frame_duration: MediaTime) -> f64 {
    let fps = 1.0 / frame_duration.as_seconds();
    for (standard, base) in [(23.976, 24), (29.97, 30), (59.94, 60), (119.88, 120)] {
        if (fps - standard).abs() < 0.02 {
            return base as f64 / 1.001;
        }
    }
    frame_duration.as_seconds().recip().max(1.0)
}

/// Collapse a sub-sample fractional remainder (float dust, solver noise)
/// to the adjacent integer frame. Returns `(floor_adjustment,
/// effective_frac)`: `(0, 0.0)` just below an integer, `(1, 0.0)` just
/// above the next one, `(0, frac)` for genuine remainders. Genuine
/// single-sample offsets (~1/1920 frame at 25 fps) are far above the
/// half-sample threshold and never snapped.
pub fn snap_sub_sample_frac(frac_frames: f64, samples_per_frame: f64) -> (i64, f64) {
    if samples_per_frame <= 0.0 {
        return (0, frac_frames);
    }
    if frac_frames <= 0.5 / samples_per_frame {
        (0, 0.0)
    } else if 1.0 - frac_frames <= 0.5 / samples_per_frame {
        (1, 0.0)
    } else {
        (0, frac_frames)
    }
}

/// The 1/80-frame `<subframeoffset>` of a fractional remainder. Premiere
/// and Resolve ignore it on import; other FCP 7 XML readers may not.
pub fn subframe_offset_80(frac_frames: f64) -> i64 {
    (frac_frames * 80.0).round() as i64
}

impl ExportTransitionKind {
    pub fn from_model(kind: TimelineTransitionKind) -> Self {
        match kind {
            TimelineTransitionKind::CrossDissolve => Self::CrossDissolve,
            TimelineTransitionKind::AudioTransition => Self::AudioTransition,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportTransition {
    pub kind: ExportTransitionKind,
    pub right_instance_id: String,
    pub start: f64,
    pub end: f64,
    pub alignment: String,
    pub fcp7_effect_xml: String,
    pub fcp7_transition_xml: Option<String>,
    pub is_otio_portable: bool,
}

impl ExportTransition {
    pub fn offset_by(&self, value: f64) -> Self {
        let mut out = self.clone();
        out.start += value;
        out.end += value;
        out
    }
}

#[derive(Clone, Debug)]
pub struct ExportLinkedAudio {
    pub start: f64,
    pub source_in: f64,
    pub source_out: f64,
    pub timeline_duration: f64,
    pub playback_rate: f64,
    pub plays_backward: bool,
    pub fcp7_time_remap_xml: Option<String>,
    pub fcp7_filter_xmls: Vec<String>,
    pub fcp7_retime_in: Option<i64>,
    pub fcp7_retime_out: Option<i64>,
    pub fcp7_retime_duration: Option<i64>,
    pub fcp7_labels_xml: Option<String>,
    pub audio_source_channel: Option<usize>,
    pub fcpxml_audio_role: Option<String>,
    pub preferred_source_key: String,
    pub enabled: bool,
    pub track_enabled: bool,
    pub track_locked: bool,
    pub transition_after: Option<ExportTransition>,
}

impl ExportLinkedAudio {
    pub fn offset_by(&self, value: f64) -> Self {
        let mut out = self.clone();
        out.start += value;
        out.transition_after = out.transition_after.map(|t| t.offset_by(value));
        out
    }
}

#[derive(Clone, Debug)]
pub struct ExportItem {
    pub instance_id: String,
    pub display_name: Option<String>,
    pub clip: Clip,
    pub start: f64,
    pub source_in: f64,
    pub source_out: f64,
    pub timeline_duration: f64,
    pub playback_rate: f64,
    pub plays_backward: bool,
    pub fcp7_time_remap_xml: Option<String>,
    pub fcp7_filter_xmls: Vec<String>,
    pub fcp7_retime_in: Option<i64>,
    pub fcp7_retime_out: Option<i64>,
    pub fcp7_retime_duration: Option<i64>,
    pub fcp7_labels_xml: Option<String>,
    pub audio_source_channel: Option<usize>,
    pub fcpxml_audio_role: Option<String>,
    pub preferred_source_key: Option<String>,
    pub preferred_audio_source_key: Option<String>,
    pub mapping_rate: f64,
    pub mapping_points: Vec<MappingPoint>,
    pub confidence: f64,
    pub corrected_audio_url: Option<PathBuf>,
    pub precision_audio_urls: Vec<PathBuf>,
    pub enabled: bool,
    pub track_enabled: bool,
    pub track_locked: bool,
    pub audio_enabled: Option<bool>,
    pub audio_track_enabled: Option<bool>,
    pub audio_track_locked: Option<bool>,
    pub transition_after: Option<ExportTransition>,
    pub audio_transition_after: Option<ExportTransition>,
    pub linked_audio_edit: Option<ExportLinkedAudio>,
}

impl ExportItem {
    /// One-based source channels used by timeline interchange writers.
    pub fn audio_source_channels(&self) -> std::ops::RangeInclusive<usize> {
        match self.selected_audio_source_channel() {
            Some(channel) => (channel + 1)..=(channel + 1),
            None => 1..=self.clip.audio.first().map_or(1, |a| a.channels.max(1)),
        }
    }

    pub fn selected_audio_source_channel(&self) -> Option<usize> {
        self.linked_audio_edit
            .as_ref()
            .map_or(self.audio_source_channel, |audio| {
                audio.audio_source_channel
            })
    }

    pub fn new(
        clip: Clip,
        start: f64,
        mapping_rate: f64,
        mapping_points: Vec<MappingPoint>,
        confidence: f64,
    ) -> Self {
        let source_out = clip.duration.as_seconds();
        Self {
            instance_id: clip.id.0.clone(),
            display_name: None,
            source_in: 0.0,
            source_out,
            timeline_duration: source_out.max(0.0),
            clip,
            start,
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
            preferred_source_key: None,
            preferred_audio_source_key: None,
            mapping_rate,
            mapping_points,
            confidence,
            corrected_audio_url: None,
            precision_audio_urls: Vec::new(),
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

    pub fn source_duration(&self) -> f64 {
        self.clip.duration.as_seconds()
    }
    pub fn mapped_duration(&self) -> f64 {
        self.source_duration() * self.mapping_rate
    }
    pub fn selected_source_duration(&self) -> f64 {
        (self.source_out - self.source_in).max(0.0)
    }
    pub fn is_retimed(&self, media: MediaKind) -> bool {
        self.plays_backward(media) || (self.playback_rate(media) - 1.0).abs() > RATE_TOLERANCE
    }
    pub fn minimum_timeline_start(&self) -> f64 {
        self.start.min(
            self.linked_audio_edit
                .as_ref()
                .map_or(self.start, |l| l.start),
        )
    }
    pub fn maximum_timeline_end(&self) -> f64 {
        let own = self.start + self.timeline_duration;
        match &self.linked_audio_edit {
            Some(l) => own.max(l.start + l.timeline_duration),
            None => own,
        }
    }
    fn mapped_value(&self, source: f64) -> f64 {
        PiecewiseTimeMapping::new(
            self.mapping_points
                .iter()
                .map(|p| MapPoint::new(p.source.as_seconds(), p.island.as_seconds()))
                .collect(),
        )
        .value_at(source)
    }
    pub fn corrected_source_in(&self) -> f64 {
        self.mapped_value(self.source_in) - self.mapped_value(0.0)
    }
    pub fn corrected_source_out(&self) -> f64 {
        self.mapped_value(self.source_out) - self.mapped_value(0.0)
    }
    pub fn corrected_selected_duration(&self) -> f64 {
        (self.corrected_source_out() - self.corrected_source_in()).max(0.0)
    }
    pub fn is_full_source_selection(&self) -> bool {
        let tolerance = 0.5 / self.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
        self.source_in.abs() <= tolerance
            && (self.source_out - self.source_duration()).abs() <= tolerance
    }
    pub fn precision_source_start(&self) -> f64 {
        if self.corrected_audio_url.is_none() {
            self.source_in
        } else {
            self.corrected_source_in()
        }
    }
    pub fn precision_source_duration(&self) -> f64 {
        if self.corrected_audio_url.is_none() {
            self.selected_source_duration()
        } else {
            self.corrected_selected_duration()
        }
    }
    pub fn mapping_digest(&self) -> String {
        let Some(first) = self.mapping_points.first() else {
            return "empty".to_string();
        };
        // Preserve sub-sample mapping changes; millisecond rounding reused
        // stale drift audio even when the new mapping moved dozens of samples.
        let mut hash = Sha256::new();
        for point in &self.mapping_points {
            hash.update(point.source.value.to_le_bytes());
            hash.update(point.source.timescale.to_le_bytes());
            hash.update(
                (point.island.as_seconds() - first.island.as_seconds())
                    .to_bits()
                    .to_le_bytes(),
            );
        }
        hex_prefix(&hash.finalize(), 16)
    }

    pub fn precision_digest(&self) -> String {
        let range = format!(
            "{};{};{}",
            self.mapping_digest(),
            self.source_in,
            self.source_out
        );
        hex_prefix(&Sha256::digest(range.as_bytes()), 6)
    }
    pub fn precision_asset_key(&self) -> String {
        format!("{}-{}", self.clip.id.0, self.precision_digest())
    }

    pub fn with_corrected_audio(&self, url: PathBuf) -> Self {
        let mut out = self.clone();
        out.corrected_audio_url = Some(url);
        out
    }
    pub fn with_precision_audio(&self, urls: Vec<PathBuf>) -> Self {
        let mut out = self.clone();
        out.precision_audio_urls = urls;
        out
    }

    /// The separately edited audio of a linked clip, when `media` is its
    /// audio side. Each side's edit settings come from here when present.
    fn linked_audio(&self, media: MediaKind) -> Option<&ExportLinkedAudio> {
        match media {
            MediaKind::Audio => self.linked_audio_edit.as_ref(),
            MediaKind::Video => None,
        }
    }
    /// The audio side of a video clip without its own linked edit.
    fn is_embedded_audio(&self, media: MediaKind) -> bool {
        media == MediaKind::Audio && self.clip.kind == MediaKind::Video
    }

    pub fn is_enabled(&self, media: MediaKind) -> bool {
        if let Some(linked) = self.linked_audio(media) {
            return linked.enabled;
        }
        if self.is_embedded_audio(media) {
            return self.audio_enabled.unwrap_or(self.enabled);
        }
        self.enabled
    }
    pub fn is_track_enabled(&self, media: MediaKind) -> bool {
        if let Some(linked) = self.linked_audio(media) {
            return linked.track_enabled;
        }
        if self.is_embedded_audio(media) {
            return self.audio_track_enabled.unwrap_or(self.track_enabled);
        }
        self.track_enabled
    }
    pub fn is_track_locked(&self, media: MediaKind) -> bool {
        if let Some(linked) = self.linked_audio(media) {
            return linked.track_locked;
        }
        if self.is_embedded_audio(media) {
            return self.audio_track_locked.unwrap_or(self.track_locked);
        }
        self.track_locked
    }
    pub fn timeline_start(&self, media: MediaKind) -> f64 {
        self.linked_audio(media).map_or(self.start, |l| l.start)
    }
    pub fn selected_source_in(&self, media: MediaKind) -> f64 {
        self.linked_audio(media)
            .map_or(self.source_in, |l| l.source_in)
    }
    pub fn selected_source_out(&self, media: MediaKind) -> f64 {
        self.linked_audio(media)
            .map_or(self.source_out, |l| l.source_out)
    }
    pub fn selected_timeline_duration(&self, media: MediaKind) -> f64 {
        self.linked_audio(media)
            .map_or(self.timeline_duration, |l| l.timeline_duration)
    }
    pub fn playback_rate(&self, media: MediaKind) -> f64 {
        self.linked_audio(media)
            .map_or(self.playback_rate, |l| l.playback_rate)
    }
    pub fn plays_backward(&self, media: MediaKind) -> bool {
        self.linked_audio(media)
            .map_or(self.plays_backward, |l| l.plays_backward)
    }
    pub fn fcp7_time_remap_xml(&self, media: MediaKind) -> Option<&str> {
        self.linked_audio(media)
            .and_then(|l| l.fcp7_time_remap_xml.as_deref())
            .or(self.fcp7_time_remap_xml.as_deref())
    }
    pub fn fcp7_retime_geometry(&self, media: MediaKind) -> Option<(i64, i64, i64)> {
        let linked = self.linked_audio(media);
        let input = linked
            .and_then(|l| l.fcp7_retime_in)
            .or(self.fcp7_retime_in);
        let output = linked
            .and_then(|l| l.fcp7_retime_out)
            .or(self.fcp7_retime_out);
        let duration = linked
            .and_then(|l| l.fcp7_retime_duration)
            .or(self.fcp7_retime_duration);
        Some((input?, output?, duration?))
    }
    pub fn fcp7_labels_xml(&self, media: MediaKind) -> Option<&str> {
        match self.linked_audio(media) {
            Some(linked) => linked.fcp7_labels_xml.as_deref(),
            None => self.fcp7_labels_xml.as_deref(),
        }
    }
    /// Verbatim non-timeremap filters for one clipitem side. Like labels
    /// (not like the remap fallback): a video-side filter must never leak
    /// onto the linked audio clipitem.
    pub fn fcp7_filter_xmls(&self, media: MediaKind) -> &[String] {
        match self.linked_audio(media) {
            Some(linked) => &linked.fcp7_filter_xmls,
            None => &self.fcp7_filter_xmls,
        }
    }
    pub fn fcpxml_audio_role(&self) -> Option<&str> {
        self.linked_audio_edit
            .as_ref()
            .and_then(|linked| linked.fcpxml_audio_role.as_deref())
            .or(self.fcpxml_audio_role.as_deref())
    }
    pub fn preferred_source_key(&self, media: MediaKind) -> Option<&str> {
        if let Some(linked) = self.linked_audio(media) {
            return Some(linked.preferred_source_key.as_str());
        }
        match media {
            MediaKind::Audio => self
                .preferred_audio_source_key
                .as_deref()
                .or(self.preferred_source_key.as_deref()),
            MediaKind::Video => self.preferred_source_key.as_deref(),
        }
    }
    pub fn transition_for(&self, media: MediaKind) -> Option<&ExportTransition> {
        match media {
            MediaKind::Audio => match &self.linked_audio_edit {
                Some(linked) => linked.transition_after.as_ref(),
                None => self.audio_transition_after.as_ref(),
            },
            MediaKind::Video => self.transition_after.as_ref(),
        }
    }

    pub fn offset_by(&self, value: f64) -> Self {
        let mut out = self.clone();
        out.start += value;
        out.mapping_points = self
            .mapping_points
            .iter()
            .map(|p| MappingPoint {
                source: p.source,
                island: MediaTime::microseconds(p.island.as_seconds() + value),
            })
            .collect();
        out.transition_after = out.transition_after.map(|t| t.offset_by(value));
        out.audio_transition_after = out.audio_transition_after.map(|t| t.offset_by(value));
        out.linked_audio_edit = out.linked_audio_edit.map(|l| l.offset_by(value));
        out
    }
}

fn hex_prefix(digest: &[u8], bytes: usize) -> String {
    digest[..bytes].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug)]
pub struct ExportIsland {
    pub id: usize,
    pub clips: Vec<ExportItem>,
    pub duration: f64,
}

#[derive(Clone, Debug)]
pub struct ExportTimeline {
    pub islands: Vec<ExportIsland>,
    pub frame_duration: MediaTime,
    pub name: String,
    /// Temporal evidence policy for island/unmatched chronology. Default
    /// (all Auto) preserves legacy ordering; set from the solve result.
    pub temporal_policy: TemporalPolicy,
    /// Prevent independent clock-positioned groups from overlapping.
    pub prevent_group_overlaps: bool,
    /// Keep imported sequence coordinates when one or more tracks anchor
    /// the edit. This survives the combine passes used by prepared export.
    pub preserve_origin: bool,
    /// Islands whose unmatched clips should be packed by stable order only.
    order_only_islands: HashSet<usize>,
    /// Review-state validation details associated with the effective view.
    pub review_issues: Vec<ReviewValidationIssue>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnmatchedPlacement {
    #[default]
    ByOrderAndTime,
    ByOrderOnly,
    Remove,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportAssemblyOptions {
    /// Manual timeline starts layered over the immutable automatic solve.
    pub review_session: ReviewSession,
    pub unmatched: UnmatchedPlacement,
    pub prevent_group_overlaps: bool,
    pub disable_unmatched: bool,
    pub label_synced: bool,
    pub label_unmatched: bool,
    pub cut_remove: CutRemoveOptions,
    /// Custom symbol for synchronized names (default `[SYNCED]`).
    /// A non-empty symbol enables labeling.
    pub synced_symbol: Option<String>,
    /// Attach the synchronized symbol as a suffix instead of a prefix.
    pub synced_symbol_suffix: bool,
    /// FCP 7 color label assigned to synchronized clips.
    pub synced_color: Option<String>,
    /// Final Cut audio role assigned to synchronized audio (FCPXML only).
    pub synced_role: Option<String>,
    /// Custom symbol for unmatched names (default `[UNSYNCED]`).
    /// A non-empty symbol enables labeling.
    pub unmatched_symbol: Option<String>,
    /// Attach the symbol as a suffix instead of a prefix.
    pub unmatched_symbol_suffix: bool,
    /// FCP 7 color label assigned to unmatched clips (Premiere/Resolve
    /// XML); explicit assignment wins over preserved source labels.
    /// FCPXML has no color-label representation; the OTIO carries the
    /// nearest Resolve clip color for the Resolve import script.
    pub unmatched_color: Option<String>,
    /// Final Cut audio role assigned to unmatched audio (FCPXML only).
    pub unmatched_role: Option<String>,
    /// Override the exported sequence/project name.
    pub sequence_name: Option<String>,
}

/// Effective timeline starts projected from an automatic result plus review
/// overrides. Automatic placements in `SyncResult` remain untouched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectivePlacementView {
    pub starts: HashMap<ClipId, f64>,
    /// Valid manual overrides represented in this view.
    pub manual_overrides: HashSet<ClipId>,
    pub issues: Vec<ReviewValidationIssue>,
}

impl EffectivePlacementView {
    pub fn from_result(result: &SyncResult, review: &ReviewSession) -> Self {
        let base_starts: HashMap<ClipId, f64> = result
            .islands
            .iter()
            .flat_map(|island| &island.placements)
            .filter_map(|placement| {
                placement
                    .mapping
                    .points
                    .first()
                    .map(|point| (placement.clip_id.clone(), point.island.as_seconds()))
            })
            .collect();
        let clips_by_id: HashMap<&ClipId, &Clip> = result
            .project
            .clips
            .iter()
            .map(|clip| (&clip.id, clip))
            .collect();
        let mut issues = review.validate(&result.project.clips);
        let issue_by_id: HashMap<ClipId, ReviewValidationReason> = issues
            .iter()
            .filter_map(|issue| Some((issue.clip_id.clone()?, issue.reason)))
            .collect();
        let mut deltas: HashMap<ClipId, f64> = HashMap::new();
        for (clip_id, override_) in &review.overrides {
            if issue_by_id.contains_key(clip_id) {
                continue;
            }
            if let Some(clip) = clips_by_id.get(clip_id) {
                let base = base_starts.get(clip_id).copied().unwrap_or(0.0);
                if review.effective_start(clip, base).is_ok() {
                    deltas.insert(clip_id.clone(), override_.timeline_start - base);
                }
            }
        }

        // Imported edits on one source track move together to preserve their
        // edit-relative gaps and overlaps. Different requested shifts on the
        // same track cannot all be honored, so flag those overrides and leave
        // the entire conflicting track on its automatic placement.
        let mut conflicting = HashSet::new();
        if let Some(imported) = &result.project.imported_timeline {
            let mut by_track: HashMap<String, Vec<(ClipId, f64)>> = HashMap::new();
            for edit in &imported.edits {
                if let Some(delta) = deltas.get(&edit.clip_id) {
                    by_track
                        .entry(imported_track_key(edit.media_type, edit.track_index))
                        .or_default()
                        .push((edit.clip_id.clone(), *delta));
                }
            }
            for overrides in by_track.values() {
                if let Some((_, first)) = overrides.first()
                    && overrides
                        .iter()
                        .any(|(_, delta)| (delta - first).abs() > 1e-6)
                {
                    conflicting.extend(overrides.iter().map(|(id, _)| id.clone()));
                }
            }
        }
        for clip_id in conflicting {
            deltas.remove(&clip_id);
            issues.push(ReviewValidationIssue {
                clip_id: Some(clip_id),
                reason: ReviewValidationReason::ConflictingPlacement,
            });
        }

        let mut starts = base_starts.clone();
        for (clip_id, delta) in &deltas {
            let base = base_starts.get(clip_id).copied().unwrap_or(0.0);
            starts.insert(clip_id.clone(), base + delta);
        }
        let manual_overrides = deltas.keys().cloned().collect();
        Self {
            starts,
            manual_overrides,
            issues,
        }
    }
}

impl ExportAssemblyOptions {
    fn synced_label(&self, fallback: &str) -> Option<String> {
        assignment_label(
            fallback,
            self.label_synced,
            self.synced_symbol.as_deref(),
            self.synced_symbol_suffix,
            "[SYNCED]",
        )
    }

    fn unmatched_label(&self, fallback: &str) -> Option<String> {
        assignment_label(
            fallback,
            self.label_unmatched,
            self.unmatched_symbol.as_deref(),
            self.unmatched_symbol_suffix,
            "[UNSYNCED]",
        )
    }

    fn apply_synced_assignment(&self, item: &mut ExportItem) {
        apply_clip_assignment(
            item,
            self.synced_color.as_deref(),
            self.synced_role.as_deref(),
        );
    }

    fn apply_unmatched_assignment(&self, item: &mut ExportItem) {
        apply_clip_assignment(
            item,
            self.unmatched_color.as_deref(),
            self.unmatched_role.as_deref(),
        );
    }
}

fn assignment_label(
    fallback: &str,
    enabled: bool,
    custom_symbol: Option<&str>,
    suffix: bool,
    default_symbol: &str,
) -> Option<String> {
    let symbol = custom_symbol.unwrap_or(default_symbol);
    if (!enabled && custom_symbol.is_none_or(str::is_empty)) || symbol.is_empty() {
        return None;
    }
    Some(if suffix {
        format!("{fallback} {symbol}")
    } else {
        format!("{symbol} {fallback}")
    })
}

fn apply_clip_assignment(item: &mut ExportItem, color: Option<&str>, role: Option<&str>) {
    if let Some(color) = color {
        let labels = format!(
            "<labels><label2>{}</label2></labels>",
            xml_text::escape(color)
        );
        item.fcp7_labels_xml = Some(labels.clone());
        if let Some(linked) = item.linked_audio_edit.as_mut() {
            linked.fcp7_labels_xml = Some(labels);
        }
    }
    if let Some(role) = role {
        item.fcpxml_audio_role = Some(role.to_string());
        if let Some(linked) = item.linked_audio_edit.as_mut() {
            linked.fcpxml_audio_role = Some(role.to_string());
        }
    }
}

/// Optional trimming and removal applied after timeline assembly.
/// All operations are disabled by default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CutRemoveOptions {
    /// Cut ranges empty on every item and close the timeline.
    pub common_gaps: bool,
    /// Drop audio-only items that overlap no video item.
    pub lone_recorder: bool,
    /// Drop items shorter than this many seconds (<= 0 disables).
    pub shorter_than: f64,
    /// Trim this many seconds off every item start (<= 0 disables).
    pub trim_starts: f64,
    /// Trim this many seconds off every item end (<= 0 disables).
    pub trim_ends: f64,
}

impl CutRemoveOptions {
    pub fn is_active(self) -> bool {
        self.common_gaps
            || self.lone_recorder
            || self.shorter_than > 0.0
            || self.trim_starts > 0.0
            || self.trim_ends > 0.0
    }
}

/// Apply Cut / Remove to assembled islands. Transitions never survive
/// timeline surgery: coordinates reference absolute positions, so cut
/// items rejoin as plain cuts.
pub fn apply_cut_remove(
    islands: Vec<ExportIsland>,
    options: CutRemoveOptions,
) -> Vec<ExportIsland> {
    if !options.is_active() {
        return islands;
    }
    islands
        .into_iter()
        .filter_map(|island| {
            let mut clips: Vec<ExportItem> = island
                .clips
                .into_iter()
                .filter_map(|item| trim_item(item, options.trim_starts, options.trim_ends))
                .collect();
            for item in &mut clips {
                item.transition_after = None;
                item.audio_transition_after = None;
                if let Some(linked) = item.linked_audio_edit.as_mut() {
                    linked.transition_after = None;
                }
            }
            if options.shorter_than > 0.0 {
                clips.retain(|item| item.timeline_duration >= options.shorter_than);
            }
            if options.lone_recorder {
                let video_spans: Vec<(f64, f64)> = clips
                    .iter()
                    .filter(|item| item.clip.video.is_some())
                    .map(|item| (item.minimum_timeline_start(), item.maximum_timeline_end()))
                    .collect();
                // No camera in this island: nothing can be judged lone.
                if !video_spans.is_empty() {
                    clips.retain(|item| {
                        if item.clip.video.is_some() {
                            return true;
                        }
                        let (start, end) =
                            (item.minimum_timeline_start(), item.maximum_timeline_end());
                        video_spans
                            .iter()
                            .any(|&(video_start, video_end)| start < video_end && video_start < end)
                    });
                }
            }
            if options.common_gaps {
                close_common_gaps(&mut clips);
            }
            if clips.is_empty() {
                return None;
            }
            let duration = clips
                .iter()
                .map(ExportItem::maximum_timeline_end)
                .fold(0.0f64, f64::max);
            Some(ExportIsland {
                id: island.id,
                clips,
                duration,
            })
        })
        .collect()
}

struct TrimSpan<'a> {
    start: &'a mut f64,
    source_in: &'a mut f64,
    source_out: &'a mut f64,
    duration: &'a mut f64,
    rate: f64,
    backward: bool,
}

fn trim_span(span: TrimSpan, head: f64, tail: f64) {
    let rate = if span.rate.abs() < 1e-9 {
        1.0
    } else {
        span.rate
    };
    if head > 0.0 {
        if span.backward {
            *span.source_out -= head / rate;
        } else {
            *span.source_in += head / rate;
        }
        *span.start += head;
        *span.duration -= head;
    }
    if tail > 0.0 {
        if span.backward {
            *span.source_in += tail / rate;
        } else {
            *span.source_out -= tail / rate;
        }
        *span.duration -= tail;
    }
}

fn trim_item(mut item: ExportItem, head: f64, tail: f64) -> Option<ExportItem> {
    if head <= 0.0 && tail <= 0.0 {
        return Some(item);
    }
    trim_span(
        TrimSpan {
            start: &mut item.start,
            source_in: &mut item.source_in,
            source_out: &mut item.source_out,
            duration: &mut item.timeline_duration,
            rate: item.playback_rate,
            backward: item.plays_backward,
        },
        head,
        tail,
    );
    if let Some(linked) = item.linked_audio_edit.as_mut() {
        trim_span(
            TrimSpan {
                start: &mut linked.start,
                source_in: &mut linked.source_in,
                source_out: &mut linked.source_out,
                duration: &mut linked.timeline_duration,
                rate: linked.playback_rate,
                backward: linked.plays_backward,
            },
            head,
            tail,
        );
    }
    if item.timeline_duration <= 0.0 {
        return None;
    }
    Some(item)
}

fn close_common_gaps(clips: &mut [ExportItem]) {
    let mut spans: Vec<(f64, f64)> = clips
        .iter()
        .map(|item| (item.minimum_timeline_start(), item.maximum_timeline_end()))
        .filter(|&(start, end)| end > start)
        .collect();
    if spans.is_empty() {
        return;
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut covered: Vec<(f64, f64)> = Vec::new();
    for (start, end) in spans {
        if let Some(last) = covered.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        covered.push((start, end));
    }
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let mut cursor = 0.0;
    for &(start, end) in &covered {
        if start > cursor {
            gaps.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if gaps.is_empty() {
        return;
    }
    let removed_before = |time: f64| -> f64 {
        gaps.iter()
            .map(|&(gap_start, gap_end)| (gap_end.min(time) - gap_start).max(0.0))
            .sum()
    };
    for item in clips.iter_mut() {
        item.start -= removed_before(item.start);
        if let Some(linked) = item.linked_audio_edit.as_mut() {
            linked.start -= removed_before(linked.start);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TimelineExportFormat {
    #[serde(rename = "aaf")]
    Aaf,
    #[serde(rename = "resolveOTIO")]
    ResolveOTIO,
    #[serde(rename = "resolveScript")]
    ResolveScript,
    #[serde(rename = "resolveXML")]
    ResolveXML,
    #[serde(rename = "premiereXML")]
    PremiereXML,
    #[serde(rename = "finalCutProXML")]
    FinalCutProXML,
}

impl TimelineExportFormat {
    pub fn default_formats() -> Vec<Self> {
        vec![
            Self::ResolveOTIO,
            Self::ResolveXML,
            Self::ResolveScript,
            Self::PremiereXML,
            Self::FinalCutProXML,
        ]
    }
    pub fn file_extension(&self) -> &'static str {
        match self {
            Self::Aaf => "aaf",
            Self::ResolveOTIO => "otio",
            Self::ResolveScript => "py",
            Self::ResolveXML => "xml",
            Self::PremiereXML => "xml",
            Self::FinalCutProXML => "fcpxml",
        }
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum TimelineExportError {
    #[error("There are no synchronized islands to export.")]
    NoSynchronizedIslands,
    #[error("The imported timeline contains no media edits that can be exported.")]
    NoImportedEdits,
    #[error("Audio channel {} is unavailable in {}.", .channel.saturating_add(1), .path.display())]
    InvalidAudioChannel { path: PathBuf, channel: usize },
}

/// One solved island's placements as items, in timeline order.
fn solved_island(
    island: &SyncIsland,
    clips_by_id: &HashMap<&ClipId, &Clip>,
    effective: &EffectivePlacementView,
) -> ExportIsland {
    let mut items: Vec<ExportItem> = island
        .placements
        .iter()
        .filter_map(|placement| {
            let clip = clips_by_id.get(&placement.clip_id).copied()?;
            let first = placement.mapping.points.first()?;
            let last = placement.mapping.points.last()?;
            let base_start = first.island.as_seconds();
            let start = effective
                .starts
                .get(&placement.clip_id)
                .copied()
                .unwrap_or(base_start);
            let offset = start - base_start;
            let mapping_points = if offset == 0.0 {
                placement.mapping.points.clone()
            } else {
                placement
                    .mapping
                    .points
                    .iter()
                    .map(|point| MappingPoint {
                        source: point.source,
                        island: MediaTime::microseconds(point.island.as_seconds() + offset),
                    })
                    .collect()
            };
            let source_span = last.source.as_seconds() - first.source.as_seconds();
            let island_span = last.island.as_seconds() - first.island.as_seconds();
            Some(ExportItem::new(
                clip.clone(),
                start,
                if source_span > 0.0 {
                    island_span / source_span
                } else {
                    1.0
                },
                mapping_points,
                placement.confidence,
            ))
        })
        .collect();
    items.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then_with(|| file_name(&a.clip.url).cmp(&file_name(&b.clip.url)))
    });
    let duration = items
        .iter()
        .map(|i| i.start + i.source_duration().max(i.mapped_duration()))
        .fold(0.0f64, f64::max);
    ExportIsland {
        id: island.id,
        clips: items,
        duration,
    }
}

/// A clip without a synchronization match, played whole at its own rate.
fn unmatched_item(
    clip: &Clip,
    options: &ExportAssemblyOptions,
    manual_start: Option<f64>,
) -> ExportItem {
    let zero = MediaTime::new(0, 1);
    let start = manual_start.unwrap_or(0.0);
    let identity = vec![
        MappingPoint {
            source: zero,
            island: if manual_start.is_some() {
                MediaTime::microseconds(start)
            } else {
                zero
            },
        },
        MappingPoint {
            source: clip.duration,
            island: if manual_start.is_some() {
                MediaTime::microseconds(clip.duration.as_seconds() + start)
            } else {
                clip.duration
            },
        },
    ];
    let mut item = ExportItem {
        display_name: options.unmatched_label(&file_name(&clip.url)),
        enabled: !options.disable_unmatched,
        ..ExportItem::new(clip.clone(), start, 1.0, identity, 0.0)
    };
    options.apply_unmatched_assignment(&mut item);
    item
}

/// Each track moves by the median difference between where the solve
/// places its edits' media and where the edit placed them. Preserved
/// tracks stay put and the others move relative to them.
fn imported_track_shifts(
    imported: &ImportedTimeline,
    base_by_id: &HashMap<&ClipId, &ExportItem>,
    result: &SyncResult,
    effective: &EffectivePlacementView,
    issues: &mut Vec<ReviewValidationIssue>,
) -> (HashMap<String, f64>, f64) {
    let mut track_diffs: HashMap<String, Vec<f64>> = HashMap::new();
    for edit in &imported.edits {
        let Some(base) = base_by_id.get(&edit.clip_id) else {
            continue;
        };
        let mapping = PiecewiseTimeMapping::new(
            base.mapping_points
                .iter()
                .map(|p| MapPoint::new(p.source.as_seconds(), p.island.as_seconds()))
                .collect(),
        );
        track_diffs
            .entry(imported_track_key(edit.media_type, edit.track_index))
            .or_default()
            .push(mapping.value_at(edit.source_in.as_seconds()) - edit.timeline_start.as_seconds());
        if let Some(audio) = &edit.linked_audio_edit {
            track_diffs
                .entry(imported_track_key(MediaKind::Audio, audio.track_index))
                .or_default()
                .push(
                    mapping.value_at(audio.source_in.as_seconds())
                        - audio.timeline_start.as_seconds(),
                );
        }
    }
    let track_medians: HashMap<String, f64> = track_diffs
        .into_iter()
        .map(|(key, mut diffs)| (key, median(&mut diffs)))
        .collect();
    let mut anchor_diffs: Vec<f64> = track_medians
        .iter()
        .filter(|(key, _)| result.preserve_editing_tracks.contains(*key))
        .map(|(_, value)| *value)
        .collect();
    let anchor_shift = (!anchor_diffs.is_empty()).then(|| median(&mut anchor_diffs));
    let mut shifts: HashMap<String, f64> = track_medians
        .into_iter()
        .map(|(key, synchronized_shift)| {
            let shift = if result.preserve_editing_tracks.contains(&key) {
                0.0
            } else {
                synchronized_shift - anchor_shift.unwrap_or(0.0)
            };
            (key, shift)
        })
        .collect();
    let automatic_origin = if result.preserve_editing_tracks.is_empty() {
        imported
            .edits
            .iter()
            .flat_map(|edit| {
                let video_start = edit.timeline_start.as_seconds()
                    + shifts
                        .get(&imported_track_key(edit.media_type, edit.track_index))
                        .copied()
                        .unwrap_or(0.0);
                std::iter::once(video_start).chain(edit.linked_audio_edit.iter().map(|audio| {
                    audio.timeline_start.as_seconds()
                        + shifts
                            .get(&imported_track_key(MediaKind::Audio, audio.track_index))
                            .copied()
                            .unwrap_or(0.0)
                }))
            })
            .fold(f64::INFINITY, f64::min)
    } else {
        0.0
    };
    // Manual starts are absolute in the assembled timeline. Measure each
    // requested move from the clip's current visible imported position, after
    // automatic track alignment and origin normalization.
    let mut manual_by_track: HashMap<String, HashMap<ClipId, (f64, f64)>> = HashMap::new();
    for edit in &imported.edits {
        let Some(target) = effective
            .manual_overrides
            .contains(&edit.clip_id)
            .then(|| effective.starts.get(&edit.clip_id).copied())
            .flatten()
        else {
            continue;
        };
        let key = imported_track_key(edit.media_type, edit.track_index);
        if !base_by_id.contains_key(&edit.clip_id) {
            continue;
        }
        let current_start = edit.timeline_start.as_seconds()
            + shifts.get(&key).copied().unwrap_or(0.0)
            - automatic_origin;
        let by_clip = manual_by_track.entry(key).or_default();
        let candidate = (edit.timeline_start.as_seconds(), target - current_start);
        by_clip
            .entry(edit.clip_id.clone())
            .and_modify(|current| {
                if candidate.0 < current.0 {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    for (key, moves) in &manual_by_track {
        let deltas: Vec<_> = moves.values().map(|(_, delta)| *delta).collect();
        if let Some(first) = deltas.first()
            && deltas.iter().any(|delta| (delta - first).abs() > 1e-6)
        {
            issues.extend(moves.keys().map(|id| ReviewValidationIssue {
                clip_id: Some(id.clone()),
                reason: ReviewValidationReason::ConflictingPlacement,
            }));
            continue;
        }
        if let Some((_, delta)) = moves.values().next() {
            *shifts.entry(key.clone()).or_default() += *delta;
        }
    }
    (shifts, automatic_origin)
}

/// Places imported editing-timeline edits by their synchronized media.
struct ImportedAssembly<'a> {
    result: &'a SyncResult,
    options: &'a ExportAssemblyOptions,
    /// Each clip's item in the combined synchronized sequence.
    base_by_id: HashMap<&'a ClipId, &'a ExportItem>,
    /// Timeline shift of each imported track.
    track_shifts: HashMap<String, f64>,
    /// The video edit owning each linked audio edit.
    parent_by_linked: HashMap<&'a str, &'a str>,
    unmatched_ids: HashSet<&'a ClipId>,
}

impl ImportedAssembly<'_> {
    fn preserves(&self, track_key: &str) -> bool {
        self.result.preserve_editing_tracks.contains(track_key)
    }

    /// The edit on the synchronized timeline, or `None` when its media or
    /// track was not placed.
    fn item(&self, edit: &TimelineEdit) -> Option<ExportItem> {
        let base = self.base_by_id.get(&edit.clip_id)?;
        let track_key = imported_track_key(edit.media_type, edit.track_index);
        let shift = self.track_shifts.get(&track_key).copied()?;
        let preserve_track = self.preserves(&track_key);
        let media_str = edit.media_type.as_str();
        let mk_transition = |t: &crate::model::TimelineTransition| ExportTransition {
            kind: ExportTransitionKind::from_model(t.kind),
            right_instance_id: format!("imported-{media_str}-{}", t.right_edit_id),
            start: t.start.as_seconds() + shift,
            end: t.end.as_seconds() + shift,
            alignment: t.alignment.clone(),
            fcp7_effect_xml: t.fcp7_effect_xml.clone(),
            fcp7_transition_xml: t.fcp7_transition_xml.clone(),
            is_otio_portable: t.is_otio_portable,
        };
        let linked = edit
            .linked_audio_edit
            .as_ref()
            .map(|audio| self.linked_audio(audio, base, shift));
        let is_unmatched = self.unmatched_ids.contains(&edit.clip_id);
        let fallback_name = || {
            edit.name
                .clone()
                .unwrap_or_else(|| file_name(&base.clip.url))
        };
        let mut item = ExportItem {
            instance_id: format!("imported-{media_str}-{}", edit.id),
            display_name: if is_unmatched {
                self.options.unmatched_label(&fallback_name())
            } else {
                self.options
                    .synced_label(&fallback_name())
                    .or_else(|| edit.name.clone())
            },
            clip: {
                let mut clip = base.clip.clone();
                if edit.media_type == MediaKind::Audio {
                    // The file's start timecode is its sound's too: editors
                    // count the sound's source times from it.
                    let timecode = clip.video.take().and_then(|video| video.source_timecode);
                    if let Some(audio) = clip.audio.first_mut()
                        && audio.source_timecode.is_none()
                    {
                        audio.source_timecode = timecode;
                    }
                    clip.kind = MediaKind::Audio;
                }
                clip
            },
            start: edit.timeline_start.as_seconds() + shift,
            source_in: edit.source_in.as_seconds().max(0.0),
            source_out: edit.source_out.as_seconds().min(base.source_duration()),
            timeline_duration: (edit.timeline_end.as_seconds() - edit.timeline_start.as_seconds())
                .max(0.0),
            playback_rate: edit.playback_rate,
            plays_backward: edit.plays_backward,
            fcp7_time_remap_xml: edit.fcp7_time_remap_xml.clone(),
            fcp7_filter_xmls: edit.fcp7_filter_xmls.clone(),
            fcp7_retime_in: edit.fcp7_retime_in,
            fcp7_retime_out: edit.fcp7_retime_out,
            fcp7_retime_duration: edit.fcp7_retime_duration,
            fcp7_labels_xml: edit.fcp7_labels_xml.clone(),
            audio_source_channel: edit.audio_source_channel,
            fcpxml_audio_role: edit.fcpxml_audio_role.clone(),
            preferred_source_key: Some(track_key),
            preferred_audio_source_key: edit
                .audio_track_index
                .map(|t| format!("imported-audio-{t:06}")),
            enabled: edit.enabled && base.enabled,
            track_enabled: edit.track_enabled,
            track_locked: edit.track_locked || preserve_track,
            audio_enabled: edit.audio_enabled,
            audio_track_enabled: edit.audio_track_enabled,
            audio_track_locked: edit.audio_track_index.map(|index| {
                edit.audio_track_locked.unwrap_or(false)
                    || self.preserves(&imported_track_key(MediaKind::Audio, index))
            }),
            transition_after: edit.transition_after.as_ref().map(mk_transition),
            audio_transition_after: edit.audio_transition_after.as_ref().map(|t| {
                let mut e = mk_transition(t);
                e.is_otio_portable = false;
                e
            }),
            mapping_rate: base.mapping_rate,
            mapping_points: base.mapping_points.clone(),
            confidence: base.confidence,
            corrected_audio_url: None,
            precision_audio_urls: Vec::new(),
            linked_audio_edit: linked,
        };
        if is_unmatched {
            self.options.apply_unmatched_assignment(&mut item);
        } else {
            self.options.apply_synced_assignment(&mut item);
        }
        Some(item)
    }

    /// A video edit's separately edited audio, moved with its own track.
    fn linked_audio(
        &self,
        audio: &TimelineLinkedAudioEdit,
        base: &ExportItem,
        video_shift: f64,
    ) -> ExportLinkedAudio {
        let audio_key = imported_track_key(MediaKind::Audio, audio.track_index);
        let audio_shift = self
            .track_shifts
            .get(&audio_key)
            .copied()
            .unwrap_or(video_shift);
        let preserve_audio = self.preserves(&audio_key);
        let transition_after = audio.transition_after.as_ref().and_then(|t| {
            self.parent_by_linked
                .get(t.right_edit_id.as_str())
                .map(|parent| ExportTransition {
                    kind: ExportTransitionKind::from_model(t.kind),
                    right_instance_id: format!("imported-video-{parent}"),
                    start: t.start.as_seconds() + audio_shift,
                    end: t.end.as_seconds() + audio_shift,
                    alignment: t.alignment.clone(),
                    fcp7_effect_xml: t.fcp7_effect_xml.clone(),
                    fcp7_transition_xml: t.fcp7_transition_xml.clone(),
                    is_otio_portable: false,
                })
        });
        ExportLinkedAudio {
            start: audio.timeline_start.as_seconds() + audio_shift,
            source_in: audio.source_in.as_seconds().max(0.0),
            source_out: audio.source_out.as_seconds().min(base.source_duration()),
            timeline_duration: (audio.timeline_end.as_seconds()
                - audio.timeline_start.as_seconds())
            .max(0.0),
            playback_rate: audio.playback_rate,
            plays_backward: audio.plays_backward,
            fcp7_time_remap_xml: audio.fcp7_time_remap_xml.clone(),
            fcp7_filter_xmls: audio.fcp7_filter_xmls.clone(),
            fcp7_retime_in: audio.fcp7_retime_in,
            fcp7_retime_out: audio.fcp7_retime_out,
            fcp7_retime_duration: audio.fcp7_retime_duration,
            fcp7_labels_xml: audio.fcp7_labels_xml.clone(),
            audio_source_channel: audio.audio_source_channel,
            fcpxml_audio_role: audio.fcpxml_audio_role.clone(),
            preferred_source_key: audio_key,
            enabled: audio.enabled && base.enabled,
            track_enabled: audio.track_enabled,
            track_locked: audio.track_locked || preserve_audio,
            transition_after,
        }
    }
}

fn validate_audio_channel(clip: &Clip, selected: Option<usize>) -> Result<(), TimelineExportError> {
    if let Some(channel) = selected
        && clip
            .audio
            .first()
            .is_none_or(|audio| channel >= audio.channels)
    {
        return Err(TimelineExportError::InvalidAudioChannel {
            path: clip.url.clone(),
            channel,
        });
    }
    Ok(())
}

impl ExportTimeline {
    pub fn validate_audio_source_channels(&self) -> Result<(), TimelineExportError> {
        for item in self.islands.iter().flat_map(|island| &island.clips) {
            validate_audio_channel(&item.clip, item.selected_audio_source_channel())?;
        }
        Ok(())
    }
    pub fn new(islands: Vec<ExportIsland>, frame_duration: MediaTime, name: &str) -> Self {
        Self {
            islands,
            frame_duration,
            name: name.to_string(),
            temporal_policy: TemporalPolicy::default(),
            prevent_group_overlaps: false,
            preserve_origin: false,
            order_only_islands: HashSet::new(),
            review_issues: Vec::new(),
        }
    }

    pub fn from_result(
        result: &SyncResult,
        include_unmatched: bool,
    ) -> Result<Self, TimelineExportError> {
        Self::from_result_with_options(
            result,
            ExportAssemblyOptions {
                unmatched: if include_unmatched {
                    UnmatchedPlacement::ByOrderAndTime
                } else {
                    UnmatchedPlacement::Remove
                },
                ..Default::default()
            },
        )
    }

    pub fn from_result_with_options(
        result: &SyncResult,
        options: ExportAssemblyOptions,
    ) -> Result<Self, TimelineExportError> {
        let clips_by_id: HashMap<&ClipId, &Clip> =
            result.project.clips.iter().map(|c| (&c.id, c)).collect();
        let effective = EffectivePlacementView::from_result(result, &options.review_session);
        let automatic = if result.project.imported_timeline.is_some() {
            EffectivePlacementView::from_result(result, &crate::ReviewSession::default())
        } else {
            effective.clone()
        };
        if let Some(imported) = &result.project.imported_timeline {
            for edit in &imported.edits {
                if let Some(clip) = clips_by_id.get(&edit.clip_id) {
                    validate_audio_channel(clip, edit.audio_source_channel)?;
                    if let Some(audio) = &edit.linked_audio_edit {
                        validate_audio_channel(clip, audio.audio_source_channel)?;
                    }
                }
            }
        }
        let mut groups: Vec<ExportIsland> = result
            .islands
            .iter()
            .map(|island| solved_island(island, &clips_by_id, &automatic))
            .collect();
        for item in groups.iter_mut().flat_map(|island| &mut island.clips) {
            let fallback = item
                .display_name
                .clone()
                .unwrap_or_else(|| file_name(&item.clip.url));
            if let Some(label) = options.synced_label(&fallback) {
                item.display_name = Some(label);
            }
            options.apply_synced_assignment(item);
        }

        let mut order_only_islands = HashSet::new();
        if options.unmatched != UnmatchedPlacement::Remove {
            let next_id = groups.iter().map(|g| g.id as i64).max().unwrap_or(-1) + 1;
            for (index, clip_id) in result.unmatched.iter().enumerate() {
                let Some(clip) = clips_by_id.get(clip_id) else {
                    continue;
                };
                let id = (next_id + index as i64) as usize;
                if options.unmatched == UnmatchedPlacement::ByOrderOnly {
                    order_only_islands.insert(id);
                }
                groups.push(ExportIsland {
                    id,
                    clips: vec![unmatched_item(
                        clip,
                        &options,
                        effective
                            .manual_overrides
                            .contains(clip_id)
                            .then(|| effective.starts.get(clip_id).copied())
                            .flatten(),
                    )],
                    duration: clip.duration.as_seconds(),
                });
            }
        }
        if groups.is_empty() {
            return Err(TimelineExportError::NoSynchronizedIslands);
        }
        let Some(imported) = &result.project.imported_timeline else {
            let islands = apply_cut_remove(groups, options.cut_remove);
            if islands.is_empty() {
                return Err(TimelineExportError::NoSynchronizedIslands);
            }
            let mut timeline = Self {
                islands,
                frame_duration: result
                    .project
                    .clips
                    .iter()
                    .filter_map(|c| c.video.as_ref()?.frame_duration)
                    .next()
                    .unwrap_or(MediaTime::new(1, 25)),
                name: options
                    .sequence_name
                    .clone()
                    .unwrap_or_else(|| "Align – Synchronized Timeline".to_string()),
                temporal_policy: result.temporal_policy.clone(),
                prevent_group_overlaps: options.prevent_group_overlaps,
                preserve_origin: false,
                order_only_islands,
                review_issues: effective.issues,
            };
            if !effective.manual_overrides.is_empty() {
                // Review starts are absolute positions in the visible,
                // assembled timeline. First lay out the automatic result,
                // then apply the user's exact positions and preserve them.
                let mut combined = timeline.combined_island(1.0);
                for item in &mut combined.clips {
                    if effective.manual_overrides.contains(&item.clip.id)
                        && let Some(start) = effective.starts.get(&item.clip.id)
                    {
                        *item = item.offset_by(*start - item.start);
                    }
                }
                timeline.islands = vec![combined];
                timeline.order_only_islands.clear();
                timeline.preserve_origin = true;
            }
            return Ok(timeline);
        };

        // Imported edits replace the solved islands: each edit keeps its own
        // placement, moved with its track to where the solve put its media.
        let frame_duration = imported.frame_duration;
        let name = options
            .sequence_name
            .clone()
            .unwrap_or_else(|| imported.name.clone());
        let mut combined_base = Self::new(groups, frame_duration, &name);
        combined_base.temporal_policy = result.temporal_policy.clone();
        combined_base.prevent_group_overlaps = options.prevent_group_overlaps;
        combined_base.order_only_islands = order_only_islands;
        combined_base.review_issues = effective.issues.clone();
        let combined = combined_base.combined_island(1.0);
        let base_by_id = combined.clips.iter().map(|i| (&i.clip.id, i)).collect();
        let (track_shifts, automatic_origin) = imported_track_shifts(
            imported,
            &base_by_id,
            result,
            &effective,
            &mut combined_base.review_issues,
        );
        let assembly = ImportedAssembly {
            result,
            options: &options,
            track_shifts,
            base_by_id,
            parent_by_linked: imported
                .edits
                .iter()
                .filter_map(|e| {
                    e.linked_audio_edit
                        .as_ref()
                        .map(|l| (l.id.as_str(), e.id.as_str()))
                })
                .collect(),
            unmatched_ids: result.unmatched.iter().collect(),
        };
        let mut imported_items: Vec<ExportItem> = imported
            .edits
            .iter()
            .filter_map(|edit| assembly.item(edit))
            .collect();
        if imported_items.is_empty() {
            return Err(TimelineExportError::NoImportedEdits);
        }
        if result.preserve_editing_tracks.is_empty()
            && automatic_origin != 0.0
            && automatic_origin.is_finite()
        {
            imported_items = imported_items
                .iter()
                .map(|i| i.offset_by(-automatic_origin))
                .collect();
        }
        let duration = imported_items
            .iter()
            .map(|i| i.maximum_timeline_end())
            .fold(0.0f64, f64::max);
        let islands = apply_cut_remove(
            vec![ExportIsland {
                id: 0,
                clips: imported_items,
                duration,
            }],
            options.cut_remove,
        );
        let Some(island) = islands.into_iter().next() else {
            return Err(TimelineExportError::NoImportedEdits);
        };
        Ok(Self {
            islands: vec![island],
            frame_duration,
            name,
            temporal_policy: result.temporal_policy.clone(),
            prevent_group_overlaps: options.prevent_group_overlaps,
            preserve_origin: !result.preserve_editing_tracks.is_empty(),
            order_only_islands: HashSet::new(),
            review_issues: combined_base.review_issues,
        })
    }

    /// Preserve assembly behavior when drift rendering rebuilds the same
    /// islands with corrected media references.
    pub fn copy_assembly_policy_from(&mut self, other: &Self) {
        self.prevent_group_overlaps = other.prevent_group_overlaps;
        self.preserve_origin = other.preserve_origin;
        self.order_only_islands = other.order_only_islands.clone();
    }

    /// The same timeline (name, rate, chronology and assembly policies)
    /// around other islands, such as clips with prepared audio attached.
    pub fn with_islands(&self, islands: Vec<ExportIsland>) -> Self {
        let mut timeline = Self::new(islands, self.frame_duration, &self.name);
        timeline.temporal_policy = self.temporal_policy.clone();
        timeline.copy_assembly_policy_from(self);
        timeline
    }

    /// This timeline as the single sequence every writer exports.
    pub fn combined(&self) -> Self {
        self.with_islands(vec![self.combined_island(1.0)])
    }

    /// One sequence for all islands and unmatched clips (gap = 1 s).
    pub fn combined_island(&self, gap: f64) -> ExportIsland {
        let mut combined = Vec::new();
        for (island, shift) in self.positioned_islands(gap) {
            combined.extend(island.clips.iter().map(|item| item.offset_by(shift)));
        }
        let duration = combined
            .iter()
            .map(|i| i.maximum_timeline_end())
            .fold(0.0f64, f64::max);
        ExportIsland {
            id: 0,
            clips: combined,
            duration,
        }
    }

    pub fn ruler_timecode_start(&self) -> Option<f64> {
        let mut timecoded: Vec<(&ExportIsland, f64)> = self
            .islands
            .iter()
            .filter_map(|island| {
                let (kind, origin) = self.chronology_key(island, &self.temporal_policy);
                (kind == 1).then_some((island, origin))
            })
            .collect();
        if self
            .islands
            .iter()
            .any(|island| self.chronology_key(island, &self.temporal_policy).0 == 0)
        {
            return None;
        }
        timecoded.sort_by(|(a, av), (b, bv)| av.total_cmp(bv).then_with(|| a.id.cmp(&b.id)));
        timecoded.first().map(|(island, origin)| {
            origin
                + island
                    .clips
                    .iter()
                    .map(ExportItem::minimum_timeline_start)
                    .fold(f64::INFINITY, f64::min)
        })
    }

    /// Position islands with the selected clock when it is available.
    /// Clock evidence determines their order and useful overlaps. Empty time
    /// is compacted, and successive clips from one source are kept sequential
    /// so they can reuse the same NLE track.
    fn positioned_islands(&self, gap: f64) -> Vec<(&ExportIsland, f64)> {
        if self.preserve_origin {
            return self.islands.iter().map(|island| (island, 0.0)).collect();
        }
        let policy = &self.temporal_policy;
        let mut order: Vec<(&ExportIsland, (i32, f64))> = self
            .islands
            .iter()
            .map(|island| (island, self.chronology_key(island, policy)))
            .collect();
        order.sort_by(|(a, (ak, av)), (b, (bk, bv))| {
            ak.cmp(bk)
                .then_with(|| av.total_cmp(bv))
                .then_with(|| a.id.cmp(&b.id))
        });

        let bounds = |island: &ExportIsland| {
            let minimum = island
                .clips
                .iter()
                .map(ExportItem::minimum_timeline_start)
                .fold(f64::INFINITY, f64::min);
            let maximum = island
                .clips
                .iter()
                .map(ExportItem::maximum_timeline_end)
                .fold(f64::NEG_INFINITY, f64::max);
            (minimum, maximum)
        };

        let mut positioned = Vec::with_capacity(order.len());
        let mut cursor = 0.0;
        let mut index = 0;
        while index < order.len() {
            let kind = order[index].1.0;
            if kind >= 2 {
                let (island, _) = order[index];
                let (minimum, maximum) = bounds(island);
                if minimum.is_finite() && maximum.is_finite() {
                    positioned.push((island, cursor - minimum));
                    cursor += (maximum - minimum).max(0.0) + gap;
                }
                index += 1;
                continue;
            }

            let group_origin = order[index].1.1;
            let end = order[index..]
                .iter()
                .position(|(_, key)| {
                    key.0 != kind || (kind == 0 && (key.1 - group_origin).abs() >= 57_600.0)
                })
                .map_or(order.len(), |offset| index + offset);
            let baseline = order[index..end]
                .iter()
                .map(|(island, (_, origin))| origin + bounds(island).0)
                .fold(f64::INFINITY, f64::min);
            let mut group_end = cursor;
            for (island, (_, origin)) in &order[index..end] {
                let (minimum, maximum) = bounds(island);
                if !minimum.is_finite() || !maximum.is_finite() || !baseline.is_finite() {
                    continue;
                }
                let desired = cursor + origin - baseline;
                let shift = if self.prevent_group_overlaps {
                    desired.max(group_end - minimum)
                } else {
                    desired
                };
                positioned.push((*island, shift));
                group_end = group_end.max(maximum + shift);
            }
            cursor = group_end + gap;
            index = end;
        }
        let mut compacted = Vec::with_capacity(positioned.len());
        let mut source_ends: HashMap<String, f64> = HashMap::new();
        let mut global_end = f64::NEG_INFINITY;
        for (island, mut shift) in positioned {
            let (minimum, maximum) = bounds(island);
            if !minimum.is_finite() || !maximum.is_finite() {
                continue;
            }

            let sources: HashSet<String> = island
                .clips
                .iter()
                .map(|item| {
                    item.preferred_source_key.clone().unwrap_or_else(|| {
                        source_key_for_clip(
                            &item.clip.url,
                            item.clip.source_identifier.as_deref(),
                            item.clip
                                .media_span
                                .as_ref()
                                .map(|span| span.identifier.as_str()),
                        )
                    })
                })
                .collect();
            let original_start = minimum + shift;
            let mut start = original_start;

            // A clock gap contains no synchronized material, so retain at
            // most the requested edit gap instead of stretching the ruler.
            if global_end.is_finite() {
                start = start.min(global_end + gap);
            }
            for source in &sources {
                if let Some(end) = source_ends.get(source) {
                    start = start.max(end + gap);
                }
            }
            if self.prevent_group_overlaps && global_end.is_finite() {
                start = start.max(global_end + gap);
            }

            shift += start - original_start;
            let end = maximum + shift;
            for source in sources {
                source_ends
                    .entry(source)
                    .and_modify(|previous| *previous = previous.max(end))
                    .or_insert(end);
            }
            global_end = global_end.max(end);
            compacted.push((island, shift));
        }
        compacted
    }

    fn chronology_key(&self, island: &ExportIsland, policy: &TemporalPolicy) -> (i32, f64) {
        if self.order_only_islands.contains(&island.id) {
            (2, island.id as f64)
        } else {
            chronology_key(island, policy)
        }
    }

    pub fn preview_items(&self, unmatched: &HashSet<ClipId>) -> Vec<TimelinePreviewItem> {
        self.combined_island(1.0)
            .clips
            .iter()
            .map(|item| TimelinePreviewItem {
                id: item.instance_id.clone(),
                clip_id: item.clip.id.clone(),
                url: item.clip.url.clone(),
                name: item
                    .display_name
                    .clone()
                    .unwrap_or_else(|| file_name(&item.clip.url)),
                kind: item.clip.kind,
                source_key: item.preferred_source_key.clone().unwrap_or_else(|| {
                    source_key_for_clip(
                        &item.clip.url,
                        item.clip.source_identifier.as_deref(),
                        item.clip
                            .media_span
                            .as_ref()
                            .map(|span| span.identifier.as_str()),
                    )
                }),
                start: item.start,
                duration: item.timeline_duration,
                confidence: item.confidence,
                matched: !unmatched.contains(&item.clip.id),
            })
            .collect()
    }
}

fn chronology_key(island: &ExportIsland, policy: &TemporalPolicy) -> (i32, f64) {
    let mut recorded = Vec::new();
    let mut tc = Vec::new();
    for i in &island.clips {
        let (r, t) = policy.anchors(&i.clip);
        recorded.extend(r.map(|v| v - i.start));
        tc.extend(t.map(|v| v - i.start));
    }
    if let Some(origin) = consistent_median(&recorded) {
        return (0, origin);
    }
    if let Some(origin) = consistent_median(&tc) {
        return (1, origin);
    }
    (2, island.id as f64)
}

pub fn consistent_median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in values {
        min = min.min(v);
        max = max.max(v);
    }
    if values.len() > 1 && max - min > 2.0 {
        return None;
    }
    Some(median(&mut values.to_vec()))
}

pub fn imported_track_key(kind: MediaKind, index: usize) -> String {
    format!("imported-{}-{index:06}", kind.as_str())
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

// ------------------------------------------------------------ xml text

/// XML escaping + identifier sanitizing.
pub mod xml_text {
    use crate::model::MediaTime;
    pub fn escape(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for c in value.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(c),
            }
        }
        out
    }

    pub fn identifier(value: &str) -> String {
        value
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect()
    }

    /// `%.9f`-style number without trailing zeros.
    pub fn number(value: f64) -> String {
        let rounded = value.round();
        if (value - rounded).abs() < 0.000_001 {
            return format!("{}", rounded as i64);
        }
        let mut s = format!("{value:.9}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }

    /// FCPXML rational time (`0s`, `100/25s`).
    pub fn fcpxml_time(value: MediaTime) -> String {
        if value.value == 0 {
            return "0s".to_string();
        }
        let mut a = value.value.abs();
        let mut b = value.timescale as i64;
        while b != 0 {
            let t = a % b;
            a = b;
            b = t;
        }
        let divisor = a.max(1);
        let (num, den) = (value.value / divisor, value.timescale as i64 / divisor);
        if den == 1 {
            format!("{num}s")
        } else {
            format!("{num}/{den}s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{
        AudioSummary, Clip, ClipId, MediaKind, RecordingTimestampSource, SourceTimecode,
        TemporalMode,
    };
    use std::path::PathBuf;

    #[test]
    fn mapping_cache_key_distinguishes_submillisecond_changes() {
        let mut item = placed_clip("recorder", false, 0.0, 1.0);
        item.mapping_points = vec![
            MappingPoint {
                source: MediaTime::seconds(0.0),
                island: MediaTime::seconds(0.0),
            },
            MappingPoint {
                source: MediaTime::seconds(1.0),
                island: MediaTime::seconds(1.0001),
            },
        ];
        let first = item.mapping_digest();
        item.mapping_points[1].island = MediaTime::seconds(1.0002);
        assert_ne!(first, item.mapping_digest());
        let second = item.mapping_digest();
        item.mapping_points[0].source = MediaTime::seconds(0.01);
        item.mapping_points[1].source = MediaTime::seconds(1.01);
        assert_ne!(second, item.mapping_digest());
    }

    #[test]
    fn sub_sample_dust_snaps_to_integer() {
        // 9.28 s is not binary-exact: 9.28 * 25 floors to 231 with a
        // 231.9999... remainder. Snapping encodes frame 232, not 231 + 80.
        let frames: f64 = 9.28 * 25.0;
        assert_eq!(frames.floor(), 231.0);
        let frac = frames - frames.floor();
        assert_eq!(snap_sub_sample_frac(frac, 48_000.0 / 25.0), (1, 0.0));
        // Genuine single-sample offsets never snap.
        let frames: f64 = (2.0 / 25.0 + 1.0 / 48_000.0) * 25.0;
        let frac = frames - frames.floor();
        assert_eq!(snap_sub_sample_frac(frac, 48_000.0 / 25.0), (0, frac));
        assert!(frac > 0.0);
    }

    fn island_clip(name: &str, recorded: i64, tc_secs: f64) -> ExportItem {
        island_clip_from("v", name, recorded, tc_secs)
    }

    fn island_clip_from(source: &str, name: &str, recorded: i64, tc_secs: f64) -> ExportItem {
        let clip = Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/{source}/{name}.wav")),
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
                    derived: false,
                }),
            }],
            video: None,
            recorded_at: Some(recorded),
            recorded_at_source: Some(RecordingTimestampSource::EmbeddedMetadata),
            source_identifier: None,
            media_span: None,
        };
        ExportItem::new(
            clip,
            0.0,
            1.0,
            vec![
                MappingPoint {
                    source: MediaTime::seconds(0.0),
                    island: MediaTime::seconds(0.0),
                },
                MappingPoint {
                    source: MediaTime::seconds(10.0),
                    island: MediaTime::seconds(10.0),
                },
            ],
            0.9,
        )
    }

    /// Recorded order B < A but timecode order A < B: the combined
    /// sequence follows the session policy.
    #[test]
    fn combined_island_follows_temporal_policy() {
        let islands = vec![
            ExportIsland {
                id: 0,
                clips: vec![island_clip("a", 2000, 100.0)],
                duration: 10.0,
            },
            ExportIsland {
                id: 1,
                clips: vec![island_clip("b", 1000, 200.0)],
                duration: 10.0,
            },
        ];
        let order = |timeline: &ExportTimeline| {
            timeline
                .combined_island(1.0)
                .clips
                .iter()
                .map(|i| i.clip.id.0.clone())
                .collect::<Vec<_>>()
        };
        let auto = ExportTimeline::new(islands.clone(), MediaTime::new(1, 25), "t");
        assert_eq!(order(&auto), vec!["b", "a"]);
        let mut timed = ExportTimeline::new(islands, MediaTime::new(1, 25), "t");
        timed.temporal_policy = TemporalPolicy {
            default: TemporalMode::Timecode,
            modes: HashMap::new(),
        };
        assert_eq!(order(&timed), vec!["a", "b"]);
        // Ruler follows timecode keys only.
        assert_eq!(timed.ruler_timecode_start(), Some(100.0));
        assert_eq!(auto.ruler_timecode_start(), None);
    }

    /// Combined-sequence starts of one-clip islands, each given as
    /// (source folder, clip, recording time, timecode seconds).
    fn combined_starts(
        clips: &[(&str, &str, i64, f64)],
        prevent_group_overlaps: bool,
    ) -> HashMap<String, f64> {
        let islands = clips
            .iter()
            .enumerate()
            .map(|(id, &(source, name, recorded, timecode))| ExportIsland {
                id,
                clips: vec![island_clip_from(source, name, recorded, timecode)],
                duration: 10.0,
            })
            .collect();
        let mut timeline = ExportTimeline::new(islands, MediaTime::new(1, 25), "t");
        timeline.prevent_group_overlaps = prevent_group_overlaps;
        timeline
            .combined_island(1.0)
            .clips
            .iter()
            .map(|item| (item.clip.id.0.clone(), item.start))
            .collect()
    }

    #[test]
    fn combined_island_compacts_clock_gaps_and_serializes_one_source() {
        let starts = combined_starts(
            &[
                ("v", "first", 1_000, 100.0),
                ("v", "in-gap", 1_015, 115.0),
                ("v", "overlap", 1_005, 105.0),
            ],
            false,
        );
        assert_eq!(starts["first"], 0.0);
        assert_eq!(starts["overlap"], 11.0);
        assert_eq!(starts["in-gap"], 22.0);
    }

    #[test]
    fn combined_island_packs_incompatible_recording_dates() {
        let starts = combined_starts(
            &[
                ("v", "recorder", 1_704_075_706, 100.0),
                ("v", "camera", 1_771_769_850, 200.0),
            ],
            false,
        );
        assert_eq!(starts["recorder"], 0.0);
        assert_eq!(starts["camera"], 11.0);
    }

    /// Distinct sources may overlap on the clock; preventing group
    /// overlaps serializes the same input instead.
    #[test]
    fn distinct_sources_keep_clock_overlap_unless_group_overlaps_are_prevented() {
        let clips = [
            ("one", "first", 1_000, 100.0),
            ("two", "overlap", 1_005, 105.0),
            ("three", "later", 1_030, 130.0),
        ];
        let overlapping = combined_starts(&clips, false);
        assert_eq!(overlapping["first"], 0.0);
        assert_eq!(overlapping["overlap"], 5.0);
        assert_eq!(overlapping["later"], 16.0);
        let serialized = combined_starts(&clips, true);
        assert_eq!(serialized["first"], 0.0);
        assert_eq!(serialized["overlap"], 11.0);
        assert_eq!(serialized["later"], 22.0);
    }

    #[test]
    fn order_only_islands_ignore_clock_for_unmatched_placement() {
        let islands = vec![
            ExportIsland {
                id: 0,
                clips: vec![island_clip("first-by-order", 2_000, 200.0)],
                duration: 10.0,
            },
            ExportIsland {
                id: 1,
                clips: vec![island_clip("first-by-time", 1_000, 100.0)],
                duration: 10.0,
            },
        ];
        let mut timeline = ExportTimeline::new(islands, MediaTime::new(1, 25), "t");
        assert_eq!(
            timeline.combined_island(1.0).clips[0].clip.id.0,
            "first-by-time"
        );
        timeline.order_only_islands = [0, 1].into_iter().collect();
        assert_eq!(
            timeline.combined_island(1.0).clips[0].clip.id.0,
            "first-by-order"
        );
    }

    #[test]
    fn unmatched_assembly_options_change_real_result_membership_and_order() {
        let first = island_clip("first-by-order", 2_000, 200.0).clip;
        let second = island_clip("first-by-time", 1_000, 100.0).clip;
        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: crate::model::SyncProject {
                clips: vec![first.clone(), second.clone()],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: Vec::new(),
            unmatched: vec![first.id.clone(), second.id.clone()],
            matches: Vec::new(),
            temporal_policy: TemporalPolicy::default(),
        };
        let timeline = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                unmatched: UnmatchedPlacement::ByOrderAndTime,
                ..Default::default()
            },
        )
        .expect("time placement");
        assert_eq!(
            timeline.combined_island(1.0).clips[0].clip.id.0,
            "first-by-time"
        );
        let timeline = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                unmatched: UnmatchedPlacement::ByOrderOnly,
                disable_unmatched: true,
                label_unmatched: true,
                ..Default::default()
            },
        )
        .expect("order placement");
        assert_eq!(
            timeline.combined_island(1.0).clips[0].clip.id.0,
            "first-by-order"
        );
        assert!(
            timeline
                .combined_island(1.0)
                .clips
                .iter()
                .all(|item| !item.is_enabled(item.clip.kind))
        );
        assert!(timeline.combined_island(1.0).clips.iter().all(|item| {
            item.display_name
                .as_deref()
                .is_some_and(|name| name.starts_with("[UNSYNCED] "))
        }));
        assert!(matches!(
            ExportTimeline::from_result_with_options(
                &result,
                ExportAssemblyOptions {
                    unmatched: UnmatchedPlacement::Remove,
                    ..Default::default()
                },
            ),
            Err(TimelineExportError::NoSynchronizedIslands)
        ));
    }

    #[test]
    fn preserved_track_keeps_basic_edits_and_anchors_other_tracks() {
        use crate::model::{
            ClipPlacement, ImportedTimeline, SyncIsland, SyncProject, TimeMap, TimelineEdit,
        };

        let camera = placed_clip("camera", false, 0.0, 10.0).clip;
        let recorder = placed_clip("recorder", false, 0.0, 10.0).clip;
        let edit = |id: &str,
                    clip_id: &ClipId,
                    source_in: f64,
                    source_out: f64,
                    start: f64,
                    end: f64,
                    track_index: usize| TimelineEdit {
            id: id.into(),
            name: Some(id.into()),
            clip_id: clip_id.clone(),
            media_type: MediaKind::Audio,
            source_in: MediaTime::seconds(source_in),
            source_out: MediaTime::seconds(source_out),
            timeline_start: MediaTime::seconds(start),
            timeline_end: MediaTime::seconds(end),
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
        };
        let placement = |clip_id: &ClipId, start: f64| ClipPlacement {
            clip_id: clip_id.clone(),
            mapping: TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(start),
                    },
                    MappingPoint {
                        source: MediaTime::seconds(10.0),
                        island: MediaTime::seconds(start + 10.0),
                    },
                ],
            },
            confidence: 0.9,
        };
        let mut result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: [imported_track_key(MediaKind::Audio, 1)]
                .into_iter()
                .collect(),
            project: SyncProject {
                clips: vec![camera.clone(), recorder.clone()],
                warnings: Vec::new(),
                imported_timeline: Some(ImportedTimeline {
                    name: "Edited".into(),
                    frame_duration: MediaTime::new(1, 25),
                    edits: vec![
                        edit("trim", &camera.id, 1.0, 4.0, 10.0, 13.0, 1),
                        edit("duplicate", &camera.id, 5.0, 7.0, 17.0, 19.0, 1),
                        edit("recorder", &recorder.id, 0.0, 2.0, 100.0, 102.0, 2),
                    ],
                }),
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement(&camera.id, 20.0), placement(&recorder.id, 22.0)],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: Default::default(),
        };

        let timeline = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("preserved edit timeline");
        let items = &timeline.islands[0].clips;
        let by_name: HashMap<_, _> = items
            .iter()
            .map(|item| (item.display_name.as_deref().unwrap(), item))
            .collect();
        assert_eq!(by_name["trim"].start, 10.0);
        assert_eq!(by_name["trim"].source_in, 1.0);
        assert_eq!(by_name["trim"].source_out, 4.0);
        assert_eq!(by_name["duplicate"].start, 17.0);
        assert_eq!(by_name["duplicate"].timeline_duration, 2.0);
        assert!(by_name["trim"].track_locked);
        assert!(by_name["duplicate"].track_locked);
        // The selected track's median solved difference is 9.5 seconds;
        // recorder source zero at solved time 22 therefore lands at 12.5.
        assert_eq!(by_name["recorder"].start, 12.5);
        assert!(!by_name["recorder"].track_locked);

        let mut review_session = crate::ReviewSession::default();
        review_session.overrides.insert(
            camera.id.clone(),
            crate::ManualPlacementOverride::for_clip(&camera, 40.0),
        );
        let reviewed = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session,
                ..Default::default()
            },
        )
        .expect("reviewed imported edit timeline");
        let reviewed_by_name: HashMap<_, _> = reviewed.islands[0]
            .clips
            .iter()
            .map(|item| (item.display_name.as_deref().unwrap(), item))
            .collect();
        assert_eq!(reviewed_by_name["trim"].start, 40.0);
        assert_eq!(reviewed_by_name["duplicate"].start, 47.0);
        assert_eq!(
            reviewed_by_name["duplicate"].start - reviewed_by_name["trim"].start,
            7.0,
            "manual placement must preserve same-track edit spacing"
        );

        let mut recorder_review = crate::ReviewSession::default();
        recorder_review.overrides.insert(
            recorder.id.clone(),
            crate::ManualPlacementOverride::for_clip(&recorder, 50.0),
        );
        let recorder_reviewed = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session: recorder_review,
                ..Default::default()
            },
        )
        .expect("reviewed non-preserved imported track");
        let recorder_item = recorder_reviewed.islands[0]
            .clips
            .iter()
            .find(|item| item.display_name.as_deref() == Some("recorder"))
            .expect("recorder edit");
        assert_eq!(recorder_item.start, 50.0);

        let mut unpreserved_result = result.clone();
        unpreserved_result.preserve_editing_tracks.clear();
        let mut unpreserved_review = crate::ReviewSession::default();
        unpreserved_review.overrides.insert(
            recorder.id.clone(),
            crate::ManualPlacementOverride::for_clip(&recorder, 50.0),
        );
        let unpreserved_reviewed = ExportTimeline::from_result_with_options(
            &unpreserved_result,
            ExportAssemblyOptions {
                review_session: unpreserved_review,
                ..Default::default()
            },
        )
        .expect("reviewed imported timeline with normalized origin");
        let unpreserved_recorder = unpreserved_reviewed.islands[0]
            .clips
            .iter()
            .find(|item| item.display_name.as_deref() == Some("recorder"))
            .expect("recorder edit");
        assert_eq!(unpreserved_recorder.start, 50.0);

        result
            .preserve_editing_tracks
            .insert(imported_track_key(MediaKind::Audio, 2));
        let two_anchors = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("two preserved edit tracks");
        let anchored: HashMap<_, _> = two_anchors.islands[0]
            .clips
            .iter()
            .map(|item| (item.display_name.as_deref().unwrap(), item))
            .collect();
        assert_eq!(anchored["trim"].start, 10.0);
        assert_eq!(anchored["duplicate"].start, 17.0);
        assert_eq!(anchored["recorder"].start, 100.0);
        assert!(anchored.values().all(|item| item.track_locked));

        result.preserve_editing_tracks.clear();
        let ordinary = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("ordinary edit timeline");
        let starts: Vec<_> = ordinary.islands[0]
            .clips
            .iter()
            .map(|item| item.start)
            .collect();
        assert_eq!(starts, vec![0.0, 7.0, 2.5]);
    }

    #[test]
    fn rebuilt_timeline_copies_assembly_policy() {
        let mut source = ExportTimeline::new(Vec::new(), MediaTime::new(1, 25), "source");
        source.prevent_group_overlaps = true;
        source.preserve_origin = true;
        source.order_only_islands.insert(7);
        let mut rebuilt = ExportTimeline::new(Vec::new(), MediaTime::new(1, 25), "rebuilt");
        rebuilt.copy_assembly_policy_from(&source);
        assert!(rebuilt.prevent_group_overlaps);
        assert!(rebuilt.preserve_origin);
        assert_eq!(rebuilt.order_only_islands, [7].into_iter().collect());
    }

    fn placed_clip(name: &str, video: bool, start: f64, duration: f64) -> ExportItem {
        let clip = Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/v/{name}")),
            kind: if video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
            duration: MediaTime::seconds(duration),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: video.then(|| crate::model::VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        ExportItem::new(
            clip,
            start,
            1.0,
            vec![
                MappingPoint {
                    source: MediaTime::seconds(0.0),
                    island: MediaTime::seconds(0.0),
                },
                MappingPoint {
                    source: MediaTime::seconds(duration),
                    island: MediaTime::seconds(duration),
                },
            ],
            0.9,
        )
    }

    fn cut_islands(items: Vec<ExportItem>) -> Vec<ExportIsland> {
        vec![ExportIsland {
            id: 0,
            clips: items,
            duration: 30.0,
        }]
    }

    #[test]
    fn cut_remove_trims_head_and_tail() {
        let islands = apply_cut_remove(
            cut_islands(vec![placed_clip("a", true, 0.0, 10.0)]),
            CutRemoveOptions {
                trim_starts: 1.0,
                trim_ends: 2.0,
                ..Default::default()
            },
        );
        let item = &islands[0].clips[0];
        assert_eq!(item.start, 1.0);
        assert_eq!(item.timeline_duration, 7.0);
        assert_eq!(item.source_in, 1.0);
        assert_eq!(item.source_out, 8.0);
        assert_eq!(islands[0].duration, 8.0);
    }

    #[test]
    fn cut_remove_trims_linked_audio_with_video() {
        let mut item = placed_clip("a", true, 0.0, 10.0);
        item.linked_audio_edit = Some(ExportLinkedAudio {
            start: 0.0,
            source_in: 0.0,
            source_out: 10.0,
            timeline_duration: 10.0,
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
            preferred_source_key: "audio".to_string(),
            enabled: true,
            track_enabled: true,
            track_locked: false,
            transition_after: None,
        });
        let islands = apply_cut_remove(
            cut_islands(vec![item]),
            CutRemoveOptions {
                trim_starts: 2.0,
                ..Default::default()
            },
        );
        let linked = islands[0].clips[0]
            .linked_audio_edit
            .as_ref()
            .expect("linked");
        assert_eq!(linked.start, 2.0);
        assert_eq!(linked.source_in, 2.0);
        assert_eq!(linked.timeline_duration, 8.0);
    }

    #[test]
    fn cut_remove_drops_shorter_than_threshold() {
        let islands = apply_cut_remove(
            cut_islands(vec![
                placed_clip("short", false, 0.0, 4.0),
                placed_clip("exact", false, 4.0, 5.0),
                placed_clip("long", false, 9.0, 6.0),
            ]),
            CutRemoveOptions {
                shorter_than: 5.0,
                ..Default::default()
            },
        );
        let names: Vec<_> = islands[0]
            .clips
            .iter()
            .map(|item| item.clip.id.0.as_str())
            .collect();
        assert_eq!(names, vec!["exact", "long"]);
    }

    #[test]
    fn cut_remove_drops_lone_recorder_only() {
        let islands = apply_cut_remove(
            cut_islands(vec![
                placed_clip("cam", true, 0.0, 10.0),
                placed_clip("near", false, 8.0, 4.0),
                placed_clip("lone", false, 20.0, 5.0),
            ]),
            CutRemoveOptions {
                lone_recorder: true,
                ..Default::default()
            },
        );
        let names: Vec<_> = islands[0]
            .clips
            .iter()
            .map(|item| item.clip.id.0.as_str())
            .collect();
        assert_eq!(names, vec!["cam", "near"]);
    }

    #[test]
    fn cut_remove_keeps_audio_when_no_video_to_judge_by() {
        let islands = apply_cut_remove(
            cut_islands(vec![placed_clip("rec", false, 0.0, 10.0)]),
            CutRemoveOptions {
                lone_recorder: true,
                ..Default::default()
            },
        );
        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].clips.len(), 1);
    }

    #[test]
    fn cut_remove_closes_common_gaps() {
        let islands = apply_cut_remove(
            cut_islands(vec![
                placed_clip("a", true, 4.0, 5.0),
                placed_clip("b", true, 12.0, 3.0),
            ]),
            CutRemoveOptions {
                common_gaps: true,
                ..Default::default()
            },
        );
        let starts: Vec<f64> = islands[0].clips.iter().map(|item| item.start).collect();
        assert_eq!(starts, vec![0.0, 5.0]);
        assert_eq!(islands[0].duration, 8.0);
    }

    #[test]
    fn cut_remove_clears_transitions() {
        let mut item = placed_clip("a", true, 0.0, 10.0);
        item.transition_after = Some(ExportTransition {
            kind: ExportTransitionKind::CrossDissolve,
            right_instance_id: "b".to_string(),
            start: 9.0,
            end: 11.0,
            alignment: "center".to_string(),
            fcp7_effect_xml: String::new(),
            fcp7_transition_xml: None,
            is_otio_portable: true,
        });
        let islands = apply_cut_remove(
            cut_islands(vec![item]),
            CutRemoveOptions {
                trim_starts: 1.0,
                ..Default::default()
            },
        );
        assert!(islands[0].clips[0].transition_after.is_none());
    }

    #[test]
    fn cut_remove_inactive_keeps_assembly() {
        let islands = apply_cut_remove(
            cut_islands(vec![
                placed_clip("a", true, 4.0, 5.0),
                placed_clip("b", false, 12.0, 3.0),
            ]),
            CutRemoveOptions::default(),
        );
        let starts: Vec<f64> = islands[0].clips.iter().map(|item| item.start).collect();
        assert_eq!(starts, vec![4.0, 12.0]);
        assert_eq!(islands[0].duration, 30.0);
    }

    fn unmatched_result() -> SyncResult {
        let first = placed_clip("first", false, 0.0, 10.0);
        let second = placed_clip("second", false, 0.0, 10.0);
        SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: crate::model::SyncProject {
                clips: vec![first.clip.clone(), second.clip.clone()],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: Vec::new(),
            unmatched: vec![first.clip.id.clone(), second.clip.id.clone()],
            matches: Vec::new(),
            temporal_policy: TemporalPolicy::default(),
        }
    }

    fn synced_result() -> SyncResult {
        let item = placed_clip("recorder.wav", false, 0.0, 10.0);
        SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: crate::model::SyncProject {
                clips: vec![item.clip.clone()],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: vec![crate::model::SyncIsland {
                id: 0,
                placements: vec![crate::model::ClipPlacement {
                    clip_id: item.clip.id.clone(),
                    mapping: crate::model::TimeMap {
                        points: item.mapping_points,
                    },
                    confidence: 0.9,
                }],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: TemporalPolicy::default(),
        }
    }

    #[test]
    fn manual_review_start_changes_export_without_mutating_the_sync_result() {
        let result = synced_result();
        let clip = result.project.clips[0].clone();
        let original_start = result.islands[0].placements[0].mapping.points[0]
            .island
            .as_seconds();
        let mut review_session = crate::ReviewSession::default();
        review_session.overrides.insert(
            clip.id.clone(),
            crate::ManualPlacementOverride::for_clip(&clip, 42.5),
        );

        let timeline = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session,
                ..Default::default()
            },
        )
        .expect("timeline");

        assert_eq!(timeline.islands[0].clips[0].start, 42.5);
        assert_eq!(
            result.islands[0].placements[0].mapping.points[0]
                .island
                .as_seconds(),
            original_start,
            "review placement must not rewrite the automatic match"
        );
    }

    #[test]
    fn manual_review_start_keeps_other_islands_in_their_assembled_positions() {
        let mut result = synced_result();
        let second = placed_clip("camera.mov", false, 100.0, 10.0);
        result.project.clips.push(second.clip.clone());
        result.islands.push(crate::model::SyncIsland {
            id: 1,
            placements: vec![crate::model::ClipPlacement {
                clip_id: second.clip.id.clone(),
                mapping: crate::model::TimeMap {
                    points: second.mapping_points,
                },
                confidence: 0.9,
            }],
        });
        let first = result.project.clips[0].clone();
        let mut review_session = crate::ReviewSession::default();
        review_session.overrides.insert(
            first.id.clone(),
            crate::ManualPlacementOverride::for_clip(&first, 42.5),
        );

        let timeline = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session,
                ..Default::default()
            },
        )
        .expect("timeline");
        let starts: HashMap<_, _> = timeline.islands[0]
            .clips
            .iter()
            .map(|item| (item.clip.id.clone(), item.start))
            .collect();

        assert_eq!(starts[&first.id], 42.5);
        assert_eq!(starts[&second.clip.id], 11.0);
    }

    #[test]
    fn empty_review_session_preserves_export_structure() {
        let result = synced_result();
        let legacy = ExportTimeline::from_result(&result, true).expect("legacy timeline");
        let reviewed = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session: crate::ReviewSession::default(),
                ..Default::default()
            },
        )
        .expect("timeline with empty review session");

        assert_eq!(
            format!("{:?}", legacy.islands),
            format!("{:?}", reviewed.islands)
        );
        assert!(reviewed.review_issues.is_empty());
    }

    #[test]
    fn no_review_override_preserves_original_mapping_precision() {
        let mut result = synced_result();
        let precise = MediaTime::new(1, 48_000);
        result.islands[0].placements[0].mapping.points[0].island = precise;

        let timeline = ExportTimeline::from_result(&result, true).expect("timeline");

        assert_eq!(
            timeline.islands[0].clips[0].mapping_points[0].island,
            precise
        );
    }

    #[test]
    fn manual_review_start_places_unmatched_clip_at_absolute_time_once() {
        let result = unmatched_result();
        let clip = result.project.clips[0].clone();
        let mut review_session = crate::ReviewSession::default();
        review_session.overrides.insert(
            clip.id.clone(),
            crate::ManualPlacementOverride::for_clip(&clip, 42.5),
        );

        let timeline = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                review_session,
                unmatched: UnmatchedPlacement::ByOrderAndTime,
                ..Default::default()
            },
        )
        .expect("timeline");
        let placements: Vec<_> = timeline
            .islands
            .iter()
            .flat_map(|island| &island.clips)
            .filter(|item| item.clip.id == clip.id)
            .collect();

        assert_eq!(
            placements.len(),
            1,
            "manual placement must not duplicate the unmatched clip"
        );
        assert_eq!(placements[0].start, 42.5);
    }

    #[test]
    fn synced_name_color_and_role_reach_writers() {
        let timeline = ExportTimeline::from_result_with_options(
            &synced_result(),
            ExportAssemblyOptions {
                synced_symbol: Some("✓".to_string()),
                synced_symbol_suffix: true,
                synced_color: Some("Iris".to_string()),
                synced_role: Some("dialogue.interview".to_string()),
                ..Default::default()
            },
        )
        .expect("timeline");
        let item = &timeline.islands[0].clips[0];
        assert_eq!(item.display_name.as_deref(), Some("recorder.wav ✓"));
        let premiere = super::super::premiere::write(
            &timeline,
            super::TimelineExportFormat::PremiereXML,
            Default::default(),
        );
        assert!(premiere.contains("<label2>Iris</label2>"));
        assert!(premiere.contains("<name>recorder.wav ✓</name>"));
        let fcpxml =
            super::super::fcpxml::write(&timeline, super::super::fcpxml::FcpxmlOptions::TIMELINE);
        assert!(fcpxml.contains("name=\"recorder.wav ✓\""));
        assert!(
            fcpxml.contains(" audioRole=\"dialogue.interview\""),
            "{fcpxml}"
        );
        // Resolve imports neither, so the OTIO carries the name and the
        // nearest Resolve color for the import script to apply.
        let otio: serde_json::Value =
            serde_json::from_slice(&super::super::otio::data(&timeline, false).unwrap()).unwrap();
        let clip = otio["tracks"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|track| track["children"].as_array().unwrap())
            .find(|child| child["OTIO_SCHEMA"] == "Clip.2")
            .expect("clip");
        assert_eq!(clip["name"], "recorder.wav ✓");
        assert_eq!(clip["metadata"]["Align"]["Resolve Clip Color"], "Blue");
    }

    #[test]
    fn unmatched_custom_symbol_suffix_implies_labeling() {
        let timeline = ExportTimeline::from_result_with_options(
            &unmatched_result(),
            ExportAssemblyOptions {
                unmatched_symbol: Some("!!".to_string()),
                unmatched_symbol_suffix: true,
                ..Default::default()
            },
        )
        .expect("timeline");
        assert!(timeline.combined_island(1.0).clips.iter().all(|item| {
            item.display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(" !!"))
        }));
    }

    #[test]
    fn unmatched_empty_symbol_disables_labeling() {
        let timeline = ExportTimeline::from_result_with_options(
            &unmatched_result(),
            ExportAssemblyOptions {
                label_unmatched: true,
                unmatched_symbol: Some(String::new()),
                ..Default::default()
            },
        )
        .expect("timeline");
        assert!(
            timeline
                .combined_island(1.0)
                .clips
                .iter()
                .all(|item| item.display_name.is_none())
        );
    }

    #[test]
    fn sequence_name_override_reaches_writers() {
        let timeline = ExportTimeline::from_result_with_options(
            &unmatched_result(),
            ExportAssemblyOptions {
                sequence_name: Some("Evening Cut".to_string()),
                ..Default::default()
            },
        )
        .expect("timeline");
        assert_eq!(timeline.name, "Evening Cut");
    }

    #[test]
    fn unmatched_color_overrides_labels_in_premiere_xml() {
        let timeline = ExportTimeline::from_result_with_options(
            &unmatched_result(),
            ExportAssemblyOptions {
                unmatched_color: Some("Iris".to_string()),
                ..Default::default()
            },
        )
        .expect("timeline");
        // Writers render one combined island, like the export pipeline.
        let mut single = timeline.clone();
        single.islands = vec![timeline.combined_island(1.0)];
        let xml = super::super::premiere::write(
            &single,
            super::TimelineExportFormat::PremiereXML,
            Default::default(),
        );
        assert_eq!(xml.matches("<label2>Iris</label2>").count(), 2);
    }

    #[test]
    fn unmatched_role_reaches_fcpxml_audio() {
        let timeline = ExportTimeline::from_result_with_options(
            &unmatched_result(),
            ExportAssemblyOptions {
                unmatched_role: Some("effects".to_string()),
                ..Default::default()
            },
        )
        .expect("timeline");
        let mut single = timeline.clone();
        single.islands = vec![timeline.combined_island(1.0)];
        let xml =
            super::super::fcpxml::write(&single, super::super::fcpxml::FcpxmlOptions::TIMELINE);
        assert_eq!(xml.matches(" audioRole=\"effects\"").count(), 2, "{xml}");
    }
}
