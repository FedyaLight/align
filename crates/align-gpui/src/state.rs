//! Application state: faithful port of Swift `AppModel`.
//! Session model only (no project files): stale-on-add, one-shot Clear,
//! live provisional preview during sync, corrections that re-run sync,
//! export-sheet state, diagnostics, reveal-in-finder.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use align_core::{
    AudioAnalysisSource, Clip, ClipId, ClipOrder, MatchEvidence, MatchPreview, MatchThreshold,
    MediaKind, SyncConstraint, SyncResult, TemporalMode, TimelineSequenceSummary, TrackContent,
    export_model::{TimelineExportFormat, UnmatchedPlacement},
    model::file_name,
};
use align_decode::pipeline::{PipelineInput, PipelineOptions};
use gpui::{Point, ScrollHandle, px};

use crate::lane::{self, BarVisual, CorrectionOption, LaneVisual, LiveClip, LiveMatch};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Operation {
    #[default]
    Idle,
    Synchronizing,
    Ready,
    Exporting,
    Exported,
    Repairing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClipState {
    Queued,
    Analyzing,
    Matching {
        confidence: f64,
    },
    Synchronized {
        island: usize,
        confidence: f64,
        drift_ppm: f64,
        evidence: MatchEvidence,
    },
    Unmatched,
    Warning,
}

impl ClipState {
    /// Second-line status text under the clip name (mirrors `stateText`).
    pub fn status_text(&self) -> String {
        match self {
            Self::Queued => "Queued".to_string(),
            Self::Analyzing => "Analyzing waveform".to_string(),
            Self::Matching { confidence } => format!("Match {}%", percent(confidence)),
            Self::Synchronized {
                confidence,
                drift_ppm,
                evidence,
                ..
            } => {
                if *evidence == MatchEvidence::Timecode {
                    "TC Sync".to_string()
                } else if drift_ppm.abs() >= 1.0 {
                    format!(
                        "{}% · {} ppm",
                        percent(confidence),
                        drift_ppm.round() as i64
                    )
                } else {
                    format!("{}%", percent(confidence))
                }
            }
            Self::Unmatched => "Not matched".to_string(),
            Self::Warning => "Unreadable".to_string(),
        }
    }
}

fn percent(confidence: &f64) -> String {
    format!("{:.0}", (confidence * 100.0).round().clamp(0.0, 100.0))
}

#[derive(Clone, Debug)]
pub struct ClipRow {
    pub clip_id: Option<ClipId>,
    pub url: PathBuf,
    pub name: String,
    pub kind: Option<MediaKind>,
    pub duration: Option<f64>,
    pub timecode: Option<String>,
    pub state: ClipState,
}

#[derive(Clone, Debug)]
pub struct MenuTarget {
    pub clip_id: ClipId,
    pub stream_menu: bool,
    /// Lane id for lane-level stream overrides (track-label menu).
    pub lane_id: Option<String>,
    /// Right-click point in window px: the popup anchors here.
    pub position: (f32, f32),
}

#[derive(Clone, Debug)]
pub struct SequencePicker {
    pub path: PathBuf,
    pub options: Vec<TimelineSequenceSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportTarget {
    Aaf,
    ResolveOtio,
    ResolveXml,
    Premiere,
    FinalCutPro,
}

impl ExportTarget {
    pub fn all() -> [Self; 5] {
        [
            Self::ResolveOtio,
            Self::ResolveXml,
            Self::Premiere,
            Self::FinalCutPro,
            Self::Aaf,
        ]
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::ResolveOtio => "DaVinci Resolve (.otio)",
            Self::ResolveXml => "DaVinci Resolve (.xml)",
            Self::Premiere => "Adobe Premiere Pro (.xml)",
            Self::FinalCutPro => "Final Cut Pro (.fcpxml)",
            Self::Aaf => "AAF (.aaf)",
        }
    }

    pub fn formats(&self) -> Vec<TimelineExportFormat> {
        match self {
            Self::ResolveOtio => vec![
                TimelineExportFormat::ResolveOTIO,
                TimelineExportFormat::ResolveScript,
            ],
            Self::ResolveXml => vec![TimelineExportFormat::ResolveXML],
            Self::Premiere => vec![TimelineExportFormat::PremiereXML],
            Self::FinalCutPro => vec![TimelineExportFormat::FinalCutProXML],
            Self::Aaf => vec![TimelineExportFormat::Aaf],
        }
    }
}

/// Concrete defaults used by every sequence unless that sequence replaces a
/// field. Track settings then replace the effective sequence value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncDefaults {
    pub search_accuracy: align_core::SearchAccuracy,
    pub audio_source: AudioAnalysisSource,
    pub temporal_mode: TemporalMode,
    pub match_threshold: MatchThreshold,
    pub clip_order: ClipOrder,
    pub track_content: TrackContent,
}

/// `None` means that the current sequence inherits the corresponding common
/// value. Keeping each field optional preserves inheritance when Common is
/// edited later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequenceDefaults {
    pub search_accuracy: Option<align_core::SearchAccuracy>,
    pub audio_source: Option<AudioAnalysisSource>,
    pub temporal_mode: Option<TemporalMode>,
    pub match_threshold: Option<MatchThreshold>,
    pub clip_order: Option<ClipOrder>,
    pub track_content: Option<TrackContent>,
}

impl SequenceDefaults {
    fn resolve(self, common: SyncDefaults) -> SyncDefaults {
        SyncDefaults {
            search_accuracy: self.search_accuracy.unwrap_or(common.search_accuracy),
            audio_source: self.audio_source.unwrap_or(common.audio_source),
            temporal_mode: self.temporal_mode.unwrap_or(common.temporal_mode),
            match_threshold: self.match_threshold.unwrap_or(common.match_threshold),
            clip_order: self.clip_order.unwrap_or(common.clip_order),
            track_content: self.track_content.unwrap_or(common.track_content),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsScope {
    #[default]
    Common,
    CurrentSequence,
}

#[derive(Clone, Debug, Default)]
struct TrackOverrides {
    search: HashMap<ClipId, align_core::SearchAccuracy>,
    search_keys: HashMap<String, align_core::SearchAccuracy>,
    audio: HashMap<ClipId, AudioAnalysisSource>,
    audio_keys: HashMap<String, AudioAnalysisSource>,
    temporal: HashMap<ClipId, TemporalMode>,
    temporal_keys: HashMap<String, TemporalMode>,
    thresholds: HashMap<ClipId, MatchThreshold>,
    threshold_keys: HashMap<String, MatchThreshold>,
    orders: HashMap<ClipId, ClipOrder>,
    order_keys: HashMap<String, ClipOrder>,
    contents: HashMap<ClipId, TrackContent>,
    content_keys: HashMap<String, TrackContent>,
    preserve_editing: HashSet<String>,
}

impl TrackOverrides {
    fn count(&self) -> usize {
        self.search.len()
            + self.audio.len()
            + self.temporal.len()
            + self.thresholds.len()
            + self.orders.len()
            + self.contents.len()
            + self.preserve_editing.len()
    }

    fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

pub struct AppData {
    pub common_settings: SyncDefaults,
    pub sequence_settings: HashMap<usize, SequenceDefaults>,
    pub settings_scope: SettingsScope,
    pub settings_changed: bool,
    sync_dirty: bool,
    quality_only_dirty: bool,
    incremental_quality_run: bool,
    track_overrides: HashMap<usize, TrackOverrides>,
    pub show_search_quality: bool,
    pub show_search_settings: bool,
    pub show_stage_settings: bool,
    pub show_sequence_results: bool,
    pub appearance: crate::theme::AppearancePreference,
    pub inputs: Vec<PathBuf>,
    pub clips: Vec<ClipRow>,
    pub selection: HashSet<PathBuf>,
    pub island_count: usize,
    pub lanes: Vec<LaneVisual>,
    pub ruler_timecode: Option<f64>,
    pub live_matches: HashMap<String, LiveMatch>,
    pub visible_matches: Vec<LiveMatch>,
    pub warnings: Vec<String>,
    pub operation: Operation,
    pub progress: f32,
    pub status: String,
    pub error: Option<String>,
    pub exported_files: Vec<PathBuf>,
    pub result: Option<SyncResult>,
    /// Independently solved imported sequences. `result` is the currently
    /// visible member so existing timeline/correction code stays single-view.
    pub sequence_results: Vec<SyncResult>,
    pub active_sequence_result: usize,
    sequence_constraints: HashMap<usize, Vec<SyncConstraint>>,
    pub audio_stream_channels: HashMap<ClipId, Vec<usize>>,
    /// Timeline waveforms live only for this process and are regenerated
    /// from cached fingerprints when a sync runs.
    pub waveform_previews: HashMap<ClipId, Vec<f32>>,
    pub pending_count: usize,
    pub menu: Option<MenuTarget>,
    pub sequence_picker: Option<SequencePicker>,
    pub timeline_choices: HashMap<PathBuf, Vec<usize>>,
    /// Path Fixer state. Saved prefix redirections load once when the app
    /// starts; exact file choices are session-local and survive re-syncs.
    pub redirects: Vec<align_core::redirect::PathRedirection>,
    pub manual_relinks: Vec<(String, PathBuf)>,
    pub omit_extensions: Vec<String>,
    pub prefer_proxies: bool,
    pub path_fixer_prefer_proxies: bool,
    pub show_path_fixer: bool,
    pub path_fixer_dir: Option<PathBuf>,
    // Export sheet state (mirrors ExportSheet @State).
    pub show_export: bool,
    pub export_dir: Option<PathBuf>,
    pub export_selected: HashSet<ExportTarget>,
    pub export_drift: bool,
    pub export_replaced: bool,
    pub export_storylines: bool,
    pub export_fcpxml_timeline: bool,
    pub export_fcpxml_multicam: bool,
    pub export_aaf_frame_duration: Option<align_core::MediaTime>,
    pub export_media: bool,
    pub export_unmatched: UnmatchedPlacement,
    pub export_prevent_overlaps: bool,
    pub export_disable_unmatched: bool,
    pub export_label_synced: bool,
    pub export_label_unmatched: bool,
    pub export_cut_common_gaps: bool,
    pub export_cut_lone_recorder: bool,
    pub export_cut_shorter_than: f64,
    pub export_trim_starts: f64,
    pub export_trim_ends: f64,
    pub export_synced_symbol_suffix: bool,
    pub export_unmatched_symbol_suffix: bool,
    pub export_started: bool,
    // Overlays.
    pub show_warning_details: bool,
    pub show_about: bool,
    pub show_agent_setup: bool,
    pub zoom_level: f64,
    /// Scroll position of the main content (timeline panning).
    pub timeline_scroll: ScrollHandle,
    /// In-flight run coordination: `cancel` stops the worker
    /// cooperatively, `generation` filters its late messages.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub generation: u64,
}

impl Default for AppData {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            appearance: crate::theme::AppearancePreference::load(),
            common_settings: Default::default(),
            sequence_settings: HashMap::new(),
            settings_scope: Default::default(),
            settings_changed: false,
            sync_dirty: false,
            quality_only_dirty: false,
            incremental_quality_run: false,
            track_overrides: HashMap::new(),
            show_search_quality: false,
            show_search_settings: false,
            show_stage_settings: false,
            show_sequence_results: false,
            clips: Vec::new(),
            selection: HashSet::new(),
            island_count: 0,
            lanes: Vec::new(),
            ruler_timecode: None,
            live_matches: HashMap::new(),
            visible_matches: Vec::new(),
            warnings: Vec::new(),
            operation: Operation::Idle,
            progress: 0.0,
            status: "Drop media or choose files to begin.".to_string(),
            error: None,
            exported_files: Vec::new(),
            result: None,
            sequence_results: Vec::new(),
            active_sequence_result: 0,
            sequence_constraints: HashMap::new(),
            audio_stream_channels: HashMap::new(),
            waveform_previews: HashMap::new(),
            pending_count: 0,
            menu: None,
            sequence_picker: None,
            timeline_choices: HashMap::new(),
            redirects: Vec::new(),
            manual_relinks: Vec::new(),
            omit_extensions: Vec::new(),
            prefer_proxies: false,
            path_fixer_prefer_proxies: false,
            show_path_fixer: false,
            path_fixer_dir: None,
            show_export: false,
            export_dir: None,
            export_selected: [ExportTarget::ResolveOtio].into_iter().collect(),
            export_drift: true,
            export_replaced: false,
            export_storylines: false,
            export_fcpxml_timeline: true,
            export_fcpxml_multicam: true,
            export_aaf_frame_duration: None,
            export_media: false,
            export_unmatched: UnmatchedPlacement::ByOrderAndTime,
            export_prevent_overlaps: false,
            export_disable_unmatched: false,
            export_label_synced: false,
            export_label_unmatched: false,
            export_cut_common_gaps: false,
            export_cut_lone_recorder: false,
            export_cut_shorter_than: 0.0,
            export_trim_starts: 0.0,
            export_trim_ends: 0.0,
            export_synced_symbol_suffix: false,
            export_unmatched_symbol_suffix: false,
            export_started: false,
            show_warning_details: false,
            show_about: false,
            show_agent_setup: false,
            zoom_level: 0.0,
            timeline_scroll: ScrollHandle::new(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            generation: 0,
        }
    }
}

impl AppData {
    pub fn new() -> Self {
        Self {
            redirects: align_core::redirect::load_from(&align_core::redirect::config_file()),
            ..Default::default()
        }
    }

    fn selected_sequence_indices(&self) -> Vec<usize> {
        self.inputs
            .iter()
            .find_map(|path| self.timeline_choices.get(path).cloned())
            .unwrap_or_default()
    }

    pub fn sequence_key_for_position(&self, position: usize) -> usize {
        self.selected_sequence_indices()
            .get(position)
            .copied()
            .unwrap_or(0)
    }

    pub fn current_sequence_key(&self) -> usize {
        self.sequence_key_for_position(self.active_sequence_result)
    }

    pub fn effective_settings_for(&self, sequence_key: usize) -> SyncDefaults {
        self.sequence_settings
            .get(&sequence_key)
            .copied()
            .unwrap_or_default()
            .resolve(self.common_settings)
    }

    pub fn current_effective_settings(&self) -> SyncDefaults {
        self.effective_settings_for(self.current_sequence_key())
    }

    pub fn current_sequence_settings(&self) -> SequenceDefaults {
        self.sequence_settings
            .get(&self.current_sequence_key())
            .copied()
            .unwrap_or_default()
    }

    pub fn scoped_settings_are_default(&self) -> bool {
        match self.settings_scope {
            SettingsScope::Common => self.common_settings == SyncDefaults::default(),
            SettingsScope::CurrentSequence => {
                self.current_sequence_settings() == SequenceDefaults::default()
            }
        }
    }

    pub fn reset_scoped_settings(&mut self) -> bool {
        match self.settings_scope {
            SettingsScope::Common => {
                let previous = std::mem::take(&mut self.common_settings);
                previous != self.common_settings
            }
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                self.sequence_settings
                    .remove(&key)
                    .is_some_and(|value| value != SequenceDefaults::default())
            }
        }
    }

    fn current_track_overrides(&self) -> Option<&TrackOverrides> {
        self.track_overrides.get(&self.current_sequence_key())
    }

    pub fn set_scoped_search_accuracy(
        &mut self,
        value: Option<align_core::SearchAccuracy>,
    ) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.search_accuracy, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.search_accuracy, value) != value
            }
        }
    }

    pub fn set_scoped_audio_source(&mut self, value: Option<AudioAnalysisSource>) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.audio_source, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.audio_source, value) != value
            }
        }
    }

    pub fn set_scoped_temporal_mode(&mut self, value: Option<TemporalMode>) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.temporal_mode, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.temporal_mode, value) != value
            }
        }
    }

    pub fn set_scoped_match_threshold(&mut self, value: Option<MatchThreshold>) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.match_threshold, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.match_threshold, value) != value
            }
        }
    }

    pub fn set_scoped_clip_order(&mut self, value: Option<ClipOrder>) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.clip_order, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.clip_order, value) != value
            }
        }
    }

    pub fn set_scoped_track_content(&mut self, value: Option<TrackContent>) -> bool {
        match self.settings_scope {
            SettingsScope::Common => value.is_some_and(|value| {
                std::mem::replace(&mut self.common_settings.track_content, value) != value
            }),
            SettingsScope::CurrentSequence => {
                let key = self.current_sequence_key();
                let settings = self.sequence_settings.entry(key).or_default();
                std::mem::replace(&mut settings.track_content, value) != value
            }
        }
    }

    pub fn has_result(&self) -> bool {
        self.result.is_some()
    }

    pub fn is_stale(&self) -> bool {
        self.result.is_some() && (self.pending_count > 0 || self.sync_dirty)
    }

    pub fn can_synchronize(&self) -> bool {
        self.can_run_sync() && (self.result.is_none() || self.pending_count > 0 || self.sync_dirty)
    }

    fn can_run_sync(&self) -> bool {
        !self.clips.is_empty()
            && !matches!(
                self.operation,
                Operation::Synchronizing | Operation::Exporting | Operation::Repairing
            )
    }

    pub fn mark_sync_dirty(&mut self) {
        self.sync_dirty = true;
        self.quality_only_dirty = false;
    }

    pub fn mark_quality_dirty(&mut self) {
        let has_unmatched = self
            .sequence_results
            .iter()
            .any(|result| !result.unmatched.is_empty())
            || self
                .result
                .as_ref()
                .is_some_and(|result| !result.unmatched.is_empty());
        if !has_unmatched {
            return;
        }
        if !self.sync_dirty {
            self.quality_only_dirty = true;
        }
        self.sync_dirty = true;
    }

    pub fn can_export(&self) -> bool {
        self.has_result()
            && !self.is_stale()
            && !matches!(
                self.operation,
                Operation::Synchronizing | Operation::Exporting | Operation::Repairing
            )
    }

    pub fn export_output_selected(&self) -> bool {
        !self.export_targets().is_empty() || self.export_media
    }

    pub fn can_begin_export(&self) -> bool {
        self.can_export() && self.export_dir.is_some() && self.export_output_selected()
    }

    pub fn correction_count(&self) -> usize {
        self.current_constraints().len()
            + self
                .track_overrides
                .get(&self.current_sequence_key())
                .map_or(0, TrackOverrides::count)
    }

    pub fn unmatched_count(&self) -> usize {
        self.clips
            .iter()
            .filter(|c| c.state == ClipState::Unmatched)
            .count()
    }

    /// Whether export would actually render at least one corrected audio file.
    /// Video clock differences and sub-threshold slips do not expose a no-op
    /// option in the export sheet.
    pub fn has_drift(&self) -> bool {
        let Some(result) = &self.result else {
            return false;
        };
        let audio: HashSet<&ClipId> = result
            .project
            .clips
            .iter()
            .filter(|clip| clip.kind == MediaKind::Audio)
            .map(|clip| &clip.id)
            .collect();
        result.islands.iter().any(|island| {
            island.placements.iter().any(|placement| {
                audio.contains(&placement.clip_id)
                    && align_core::drift::needs_correction_points(&placement.mapping.points)
            })
        })
    }

    /// Zoom factor: 0.0 = Fit, 1.0 = 8x (mirrors `pow(8, zoomLevel)`).
    pub fn zoom(&self) -> f64 {
        8.0f64.powf(self.zoom_level.clamp(0.0, 1.0))
    }

    /// Toolbar zoom label: percent of Fit magnification (100% at rest,
    /// so it never duplicates the Fit button).
    pub fn zoom_label(&self) -> String {
        format!("{}%", (self.zoom() * 100.0).round() as i64)
    }

    /// Change zoom while keeping the content beneath `anchor_x` in the
    /// same screen position. `anchor_x` is measured inside the scrollable
    /// timeline viewport, so wheel zoom feels attached to the pointer.
    pub fn zoom_at(&mut self, level: f64, anchor_x: f32) {
        let old_zoom = self.zoom() as f32;
        let old_offset = self.timeline_scroll.offset();
        let content_x = (anchor_x - f32::from(old_offset.x)) / old_zoom;
        self.zoom_level = level.clamp(0.0, 1.0);
        let new_offset_x = (anchor_x - content_x * self.zoom() as f32).min(0.0);
        self.timeline_scroll.set_offset(Point {
            x: px(new_offset_x),
            y: old_offset.y,
        });
    }

    /// Timeline panning. Offsets are negative-of-scroll with (0,0) at the
    /// top-left origin, clamped to the measured content size.
    pub fn pan_to(&self, x: Option<f32>, y: Option<f32>) {
        let handle = &self.timeline_scroll;
        let max = handle.max_offset();
        let cur = handle.offset();
        let clamp = |v: f32, extent: f32| v.clamp(-extent, 0.0);
        handle.set_offset(Point {
            x: px(x.map_or_else(|| f32::from(cur.x), |v| clamp(v, f32::from(max.width)))),
            y: px(y.map_or_else(|| f32::from(cur.y), |v| clamp(v, f32::from(max.height)))),
        });
    }

    pub fn pan_by(&self, dx: f32, dy: f32) {
        let cur = self.timeline_scroll.offset();
        self.pan_to(Some(f32::from(cur.x) + dx), Some(f32::from(cur.y) + dy));
    }

    // ---------------- session edits (mirror add/remove/clear)

    /// Add media/tagged timeline paths; marks the session stale when a
    /// result exists, otherwise resets the preview. Multi-sequence
    /// timelines open the picker instead of joining the inputs.
    pub fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let existing: HashSet<PathBuf> = self.clips.iter().map(|c| c.url.clone()).collect();
        let mut additions = Vec::new();
        for path in paths.iter().cloned() {
            if existing.contains(&path) || additions.iter().any(|r: &ClipRow| r.url == path) {
                continue;
            }
            if is_timeline(&path)
                && !self.timeline_choices.contains_key(&path)
                && let Ok(summaries) = align_decode::timeline::sequences(
                    &path,
                    &std::sync::atomic::AtomicBool::new(false),
                )
                && summaries.len() > 1
            {
                self.sequence_picker = Some(SequencePicker {
                    path,
                    options: summaries,
                });
                continue;
            }
            additions.push(ClipRow {
                clip_id: None,
                url: path,
                name: String::new(),
                kind: None,
                duration: None,
                timecode: None,
                state: ClipState::Queued,
            });
        }
        for row in &mut additions {
            row.name = file_name(&row.url);
        }
        if additions.is_empty() {
            return;
        }
        self.inputs.extend(additions.iter().map(|r| r.url.clone()));
        self.clips.extend(additions.iter().cloned());
        self.clips
            .sort_by(|a, b| a.url.to_string_lossy().cmp(&b.url.to_string_lossy()));
        let added = additions.len();
        if self.result.is_some() {
            self.pending_count += added;
            self.exported_files.clear();
            self.status = format!("{} file{} added.", added, if added == 1 { "" } else { "s" });
            return;
        }
        self.reset_preview();
        self.exported_files.clear();
        self.operation = Operation::Idle;
        self.status = format!("Ready to analyze {} source items.", self.clips.len());
        self.error = None;
    }

    pub fn remove_selection(&mut self) {
        if matches!(
            self.operation,
            Operation::Synchronizing | Operation::Exporting | Operation::Repairing
        ) {
            return;
        }
        let selected = std::mem::take(&mut self.selection);
        if selected.is_empty() {
            return;
        }
        let removed_ids: HashSet<ClipId> = self
            .clips
            .iter()
            .filter(|clip| selected.contains(&clip.url))
            .filter_map(|clip| clip.clip_id.clone())
            .collect();
        self.clips.retain(|c| !selected.contains(&c.url));
        self.waveform_previews
            .retain(|clip_id, _| !removed_ids.contains(clip_id));
        self.inputs.retain(|u| !selected.contains(u));
        for url in &selected {
            self.timeline_choices.remove(url);
        }
        if self.result.is_some() {
            self.pending_count += selected.len();
            self.exported_files.clear();
            self.status = "Selection changed.".to_string();
            return;
        }
        self.reset_preview();
        self.exported_files.clear();
        self.operation = Operation::Idle;
        self.status = if self.clips.is_empty() {
            "Drop media or choose files to begin.".to_string()
        } else {
            format!("Ready to analyze {} source items.", self.clips.len())
        };
    }

    pub fn delete_or_clear(&mut self) {
        if self.selection.is_empty() {
            self.clear();
        } else {
            self.remove_selection();
        }
    }

    pub fn clear(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let generation = self.generation.wrapping_add(1);
        let export_drift = self.export_drift;
        let appearance = self.appearance;
        let redirects = self.redirects.clone();
        *self = Self {
            generation,
            appearance,
            export_drift,
            redirects,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ..Default::default()
        };
    }

    fn reset_preview(&mut self) {
        self.island_count = 0;
        self.lanes.clear();
        self.ruler_timecode = None;
        self.live_matches.clear();
        self.visible_matches.clear();
        self.warnings.clear();
    }

    /// Record a sequence choice and queue the timeline as an input.
    pub fn choose_sequence(&mut self, index: usize) {
        if let Some(picker) = self.sequence_picker.take() {
            if picker.options.iter().any(|o| o.index == index) {
                self.queue_timeline_sequences(picker.path, vec![index]);
            }
        }
    }

    pub fn choose_all_sequences(&mut self) {
        if let Some(picker) = self.sequence_picker.take() {
            let indices = picker.options.iter().map(|option| option.index).collect();
            self.queue_timeline_sequences(picker.path, indices);
        }
    }

    fn queue_timeline_sequences(&mut self, path: PathBuf, indices: Vec<usize>) {
        if indices.is_empty() {
            return;
        }
        self.timeline_choices.insert(path.clone(), indices);
        if !self.inputs.contains(&path) {
            let url = path;
            let name = file_name(&url);
            self.clips.push(ClipRow {
                clip_id: None,
                url: url.clone(),
                name,
                kind: None,
                duration: None,
                timecode: None,
                state: ClipState::Queued,
            });
            self.clips
                .sort_by(|a, b| a.url.to_string_lossy().cmp(&b.url.to_string_lossy()));
            self.inputs.push(url);
            self.pending_count += 1;
        }
    }

    // ---------------- sync lifecycle (mirror synchronize/apply/cancel)

    /// Reset per-run state before a sync pass (mirrors `synchronize()`).
    pub fn begin_sync_run(&mut self) -> bool {
        if !self.can_run_sync() {
            return false;
        }
        self.incremental_quality_run = self.quality_only_dirty
            && self.pending_count == 0
            && (self
                .sequence_results
                .iter()
                .any(|result| !result.unmatched.is_empty())
                || self
                    .result
                    .as_ref()
                    .is_some_and(|result| !result.unmatched.is_empty()));
        if !self.sync_dirty {
            self.quality_only_dirty = false;
        }
        self.sync_dirty = true;
        self.cancel_run();
        self.operation = Operation::Synchronizing;
        self.progress = 0.0;
        self.error = None;
        self.exported_files.clear();
        self.island_count = 0;
        self.warnings.clear();
        if self.incremental_quality_run {
            let targets: HashSet<ClipId> = self
                .sequence_results
                .iter()
                .flat_map(|result| result.unmatched.iter().cloned())
                .chain(
                    self.result
                        .iter()
                        .flat_map(|result| result.unmatched.iter().cloned()),
                )
                .collect();
            for clip in &mut self.clips {
                if clip.clip_id.as_ref().is_some_and(|id| targets.contains(id)) {
                    clip.state = ClipState::Analyzing;
                }
            }
            for bar in self.lanes.iter_mut().flat_map(|lane| &mut lane.clips) {
                if targets.contains(&bar.clip_id) {
                    bar.match_state = lane::BarMatchState::Pending;
                }
            }
        } else {
            self.waveform_previews.clear();
            self.begin_sequence_progress();
        }
        self.selection.clear();
        self.menu = None;
        self.show_export = false;
        self.show_stage_settings = false;
        self.show_search_quality = false;
        self.show_search_settings = false;
        self.settings_changed = false;
        self.show_sequence_results = false;
        self.status = "Inspecting media…".to_string();
        true
    }

    pub fn begin_sequence_progress(&mut self) {
        if self.incremental_quality_run {
            return;
        }
        self.clips = self
            .inputs
            .iter()
            .map(|url| ClipRow {
                clip_id: None,
                url: url.clone(),
                name: file_name(url),
                kind: None,
                duration: None,
                timecode: None,
                state: ClipState::Queued,
            })
            .collect();
        self.lanes.clear();
        self.ruler_timecode = None;
        self.visible_matches.clear();
        self.live_matches.clear();
    }

    pub fn restore_after_sync_cancel(&mut self) {
        self.incremental_quality_run = false;
        if let Some(previous) = self.result.clone() {
            let pending_count = self.pending_count;
            let known: HashSet<PathBuf> = previous
                .project
                .clips
                .iter()
                .map(|clip| clip.url.clone())
                .collect();
            let mut queued: Vec<ClipRow> = self
                .clips
                .iter()
                .filter(|row| !known.contains(&row.url))
                .cloned()
                .collect();
            for row in &mut queued {
                row.state = ClipState::Queued;
            }
            self.apply_visible_result(previous);
            self.clips.extend(queued);
            self.clips.sort_by(|a, b| a.url.cmp(&b.url));
            self.pending_count = pending_count;
            self.status = "Synchronization cancelled. Previous result restored.".to_string();
        } else {
            for row in &mut self.clips {
                row.state = ClipState::Queued;
            }
            self.live_matches.clear();
            self.visible_matches.clear();
            self.lanes.clear();
            self.operation = Operation::Idle;
            self.status = "Synchronization cancelled.".to_string();
        }
    }

    pub fn cancel_run(&mut self) {
        let cancelled = std::mem::replace(
            &mut self.cancel,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn begin_path_repair(&mut self) -> bool {
        if matches!(
            self.operation,
            Operation::Synchronizing | Operation::Exporting | Operation::Repairing
        ) {
            return false;
        }
        self.cancel_run();
        self.operation = Operation::Repairing;
        self.progress = 0.0;
        self.error = None;
        self.exported_files.clear();
        self.status = "Writing fixed project copy…".into();
        true
    }

    /// Phase-weighted progress + status (mirrors `apply(SyncProgress)`).
    pub fn apply_progress(
        &mut self,
        phase: &str,
        completed: usize,
        total: usize,
        current: Option<&str>,
        discovered: Option<&Clip>,
        preview: Option<&MatchPreview>,
    ) {
        if !matches!(self.operation, Operation::Synchronizing) {
            return;
        }
        if let Some(clip) = discovered {
            self.receive_discovered(clip);
        }
        if let Some(event) = preview {
            self.receive_preview(event);
        }
        let (start, share) = match phase {
            "inspect" => (0.0, 0.05),
            "fingerprint" => (0.05, 0.60),
            "match" => (0.65, 0.10),
            "refine" => (0.75, 0.20),
            "solve" => (0.95, 0.05),
            _ => (0.0, 1.0),
        };
        let fraction = if total == 0 {
            0.0
        } else {
            completed as f32 / total as f32
        };
        self.progress = (start + share * fraction as f64).min(1.0) as f32;
        let action = match phase {
            "inspect" => "Reading metadata",
            "fingerprint" => "Analyzing",
            "match" => "Finding matches",
            "refine" => "Refining",
            "solve" => "Building timeline",
            _ => "Working",
        };
        self.status = match current {
            Some(name) => format!("{action}: {name}"),
            None => format!("{action}…"),
        };
    }

    fn receive_discovered(&mut self, clip: &Clip) {
        let first = self.clips.iter().all(|c| c.clip_id.is_none())
            && !self
                .clips
                .iter()
                .any(|c| c.clip_id == Some(clip.id.clone()));
        if first {
            self.clips.clear();
        }
        self.clips.retain(|c| c.clip_id.as_ref() != Some(&clip.id));
        self.audio_stream_channels.insert(
            clip.id.clone(),
            clip.audio.iter().map(|a| a.channels).collect(),
        );
        self.clips.push(ClipRow {
            clip_id: Some(clip.id.clone()),
            url: clip.url.clone(),
            name: file_name(&clip.url),
            kind: Some(clip.kind),
            duration: Some(clip.duration.as_seconds()),
            timecode: clip.source_timecode().map(|t| t.text.clone()),
            state: ClipState::Analyzing,
        });
        self.clips
            .sort_by(|a, b| a.url.to_string_lossy().cmp(&b.url.to_string_lossy()));
        self.rebuild_provisional();
    }

    fn receive_preview(&mut self, event: &MatchPreview) {
        lane::apply_preview(&mut self.live_matches, event);
        // Mark both sides as matching (mirrors `receive(SyncMatchPreview)`).
        for row in &mut self.clips {
            let Some(id) = &row.clip_id else { continue };
            if *id == event.left || *id == event.right {
                // Rejected previews remove the edge; only mark when the edge
                // survives.
                let key = LiveMatch::id(&event.left, &event.right);
                if self.live_matches.contains_key(&key) {
                    row.state = ClipState::Matching {
                        confidence: event.confidence,
                    };
                }
            }
        }
        self.visible_matches = sorted_live_matches(&self.live_matches);
        self.rebuild_provisional();
    }

    fn rebuild_provisional(&mut self) {
        let clips: HashMap<ClipId, LiveClip> = self
            .clips
            .iter()
            .filter_map(|c| {
                let id = c.clip_id.clone()?;
                Some((
                    id.clone(),
                    LiveClip {
                        url: c.url.clone(),
                        kind: c.kind.unwrap_or(MediaKind::Audio),
                        duration: c.duration.unwrap_or(0.0),
                    },
                ))
            })
            .collect();
        if clips.is_empty() {
            self.lanes.clear();
            return;
        }
        let confidences: HashMap<ClipId, f64> = self
            .live_matches
            .values()
            .flat_map(|m| {
                [
                    (m.left.clone(), m.confidence),
                    (m.right.clone(), m.confidence),
                ]
            })
            .fold(HashMap::new(), |mut acc, (id, c)| {
                acc.entry(id).and_modify(|v| *v = v.max(c)).or_insert(c);
                acc
            });
        let bars = lane::provisional_bars(&clips, &self.live_matches, &confidences);
        self.lanes = lane::layout_bars(bars, &self.audio_stream_channels);
    }

    pub fn select_sync_stage(&mut self, index: usize) -> bool {
        if !self.can_export() {
            return false;
        }
        let Some(mut result) = self.result.clone() else {
            return false;
        };
        if !result.select_stage(index) {
            return false;
        }
        self.show_stage_settings = false;
        self.exported_files.clear();
        if let Some(stored) = self.sequence_results.get_mut(self.active_sequence_result) {
            *stored = result.clone();
        }
        self.apply_visible_result(result);
        true
    }

    /// Final result (mirrors `apply(SyncResult)`).
    pub fn apply_result(&mut self, result: SyncResult) {
        self.sequence_results = vec![result.clone()];
        self.active_sequence_result = 0;
        self.apply_visible_result(result);
        self.sync_dirty = false;
        self.quality_only_dirty = false;
        self.incremental_quality_run = false;
    }

    pub fn apply_results(&mut self, mut results: Vec<SyncResult>) {
        if results.len() == 1 {
            self.apply_result(results.pop().expect("one result"));
            return;
        }
        if results.is_empty() {
            self.operation = Operation::Idle;
            self.error = Some("No sequence results were produced.".into());
            self.incremental_quality_run = false;
            return;
        }
        let count = results.len();
        let active = self.active_sequence_result.min(count - 1);
        let visible = results[active].clone();
        self.sequence_results = results;
        self.active_sequence_result = active;
        self.apply_visible_result(visible);
        self.sync_dirty = false;
        self.quality_only_dirty = false;
        self.incremental_quality_run = false;
        if count > 1 {
            self.status = format!(
                "Synchronized {count} sequences. Showing sequence {} of {count}.",
                active + 1
            );
        }
    }

    pub fn select_sequence_result(&mut self, index: usize) -> bool {
        if matches!(
            self.operation,
            Operation::Synchronizing | Operation::Exporting | Operation::Repairing
        ) {
            return false;
        }
        let Some(result) = self.sequence_results.get(index).cloned() else {
            return false;
        };
        self.active_sequence_result = index;
        self.show_sequence_results = false;
        self.exported_files.clear();
        self.apply_visible_result(result);
        let count = self.sequence_results.len();
        let name = self
            .result
            .as_ref()
            .and_then(|result| result.project.imported_timeline.as_ref())
            .map_or("Untitled", |timeline| timeline.name.as_str());
        self.status = format!("Sequence {} of {count}: {name}.", index + 1);
        true
    }

    fn apply_visible_result(&mut self, result: SyncResult) {
        for clip in &result.project.clips {
            self.audio_stream_channels.insert(
                clip.id.clone(),
                clip.audio.iter().map(|a| a.channels).collect(),
            );
        }
        self.warnings = result
            .project
            .warnings
            .iter()
            .map(|w| {
                let name = w.url.file_name().and_then(|n| n.to_str()).unwrap_or("");
                format!("{name}: {}", w.message)
            })
            .collect();

        // Final bars (sync-graph branch) or export-model preview when an
        // imported timeline reshaped the sequence.
        let (bars, states, ruler) = if result.project.imported_timeline.is_some() {
            imported_preview(&result)
        } else {
            let final_timeline = lane::final_bars(&result);
            (
                final_timeline.bars,
                final_timeline.states,
                final_timeline.ruler_timecode,
            )
        };
        self.ruler_timecode = ruler;
        let states: HashMap<&ClipId, (f64, MatchEvidence)> =
            states.iter().map(|(id, c, e)| (id, (*c, *e))).collect();
        let unmatched: HashSet<&ClipId> = result.unmatched.iter().collect();
        let warned_urls: HashSet<&PathBuf> =
            result.project.warnings.iter().map(|w| &w.url).collect();
        let mut rows: Vec<ClipRow> = result
            .project
            .clips
            .iter()
            .map(|clip| {
                let state = if let Some((confidence, evidence)) = states.get(&clip.id) {
                    // Island index for display: position of the first island
                    // containing this clip.
                    let island = result
                        .islands
                        .iter()
                        .position(|i| i.placements.iter().any(|p| p.clip_id == clip.id))
                        .unwrap_or(0);
                    ClipState::Synchronized {
                        island,
                        confidence: *confidence,
                        drift_ppm: drift_of_placement(&result, &clip.id),
                        evidence: *evidence,
                    }
                } else if unmatched.contains(&clip.id) {
                    ClipState::Unmatched
                } else if warned_urls.contains(&clip.url) {
                    ClipState::Warning
                } else {
                    ClipState::Unmatched
                };
                ClipRow {
                    clip_id: Some(clip.id.clone()),
                    url: clip.url.clone(),
                    name: file_name(&clip.url),
                    kind: Some(clip.kind),
                    duration: Some(clip.duration.as_seconds()),
                    timecode: clip.source_timecode().map(|t| t.text.clone()),
                    state,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.url.to_string_lossy().cmp(&b.url.to_string_lossy()));
        self.clips = rows;
        self.island_count = result.islands.len();
        // Keep live map in sync so corrections work off the same ids.
        self.live_matches = result
            .matches
            .iter()
            .map(|m| {
                (
                    LiveMatch::id(&m.left, &m.right),
                    LiveMatch {
                        left: m.left.clone(),
                        right: m.right.clone(),
                        offset: m.offset.as_seconds(),
                        confidence: m.confidence,
                        refined: true,
                    },
                )
            })
            .collect();
        self.visible_matches = sorted_live_matches(&self.live_matches);
        self.lanes = lane::layout_bars(bars, &self.audio_stream_channels);
        self.pending_count = 0;
        let stopped = result.stopped;
        self.result = Some(result);
        self.operation = Operation::Ready;
        self.progress = 1.0;
        let total = self.clips.len();
        let unmatched = self.unmatched_count();
        self.status = if stopped {
            format!(
                "Stopped at completed stage: {} of {total} clips synchronized.",
                total - unmatched
            )
        } else if unmatched == 0 {
            format!("Synchronized all {total} clips.")
        } else {
            format!("Synchronized {} of {total} clips.", total - unmatched)
        };
    }

    // ---------------- corrections (mirror rejectPair/rejectAlignment/...)

    pub fn correction_options_for(&self, clip_id: &ClipId) -> Vec<lane::CorrectionOption> {
        let names: HashMap<ClipId, String> = self
            .clips
            .iter()
            .filter_map(|c| c.clip_id.clone().map(|id| (id, c.name.clone())))
            .collect();
        let live: Vec<LiveMatch> = self.live_matches.values().cloned().collect();
        lane::correction_options(clip_id, &live, &names)
    }

    pub fn reject_pair(&mut self, target: &CorrectionOption) -> bool {
        let constraint = SyncConstraint::rejecting_pair(target.left.clone(), target.right.clone());
        let sequence_key = self.current_sequence_key();
        let constraints = self.sequence_constraints.entry(sequence_key).or_default();
        if constraints.contains(&constraint) {
            return false;
        }
        constraints.push(constraint);
        true
    }

    pub fn reject_alignment(&mut self, target: &CorrectionOption) -> bool {
        let constraint = SyncConstraint::rejecting_alignment(
            target.left.clone(),
            target.right.clone(),
            target.offset,
        );
        let sequence_key = self.current_sequence_key();
        let constraints = self.sequence_constraints.entry(sequence_key).or_default();
        if constraints.contains(&constraint) {
            return false;
        }
        constraints.push(constraint);
        true
    }

    pub fn constraints_for_sequence(&self, sequence_key: usize) -> Vec<SyncConstraint> {
        self.sequence_constraints
            .get(&sequence_key)
            .cloned()
            .unwrap_or_default()
    }

    fn current_constraints(&self) -> &[SyncConstraint] {
        self.sequence_constraints
            .get(&self.current_sequence_key())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Clips sharing the lane's source track, even when the compact preview
    /// has packed a non-overlapping source into the same visual row.
    fn lane_group(&self, lane_id: &str) -> Option<(String, HashSet<ClipId>)> {
        let lane = self.lanes.iter().find(|l| l.id == lane_id)?;
        let kind_name = match lane.kind {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        };
        let key = format!("{kind_name}:{}", lane.source_key);
        let ids: HashSet<ClipId> = self
            .lanes
            .iter()
            .filter(|l| l.kind == lane.kind)
            .flat_map(|l| l.clips.iter())
            .filter(|clip| clip.source_key == lane.source_key)
            .map(|clip| clip.clip_id.clone())
            .collect();
        Some((key, ids))
    }

    /// Lane-level audio source override. `None` explicitly inherits the
    /// current sequence; `Some(Automatic)` remains a real override.
    pub fn set_analysis_source(
        &mut self,
        source: Option<AudioAnalysisSource>,
        lane_id: &str,
    ) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let sequence_key = self.current_sequence_key();
        let overrides = self.track_overrides.entry(sequence_key).or_default();
        if let Some(source) = source {
            overrides.audio_keys.insert(key, source);
            for id in ids {
                overrides.audio.insert(id, source);
            }
        } else {
            overrides.audio_keys.remove(&key);
            for id in ids {
                overrides.audio.remove(&id);
            }
        }
        true
    }

    pub fn lane_analysis_source(&self, lane_id: &str) -> Option<AudioAnalysisSource> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .audio_keys
                .get(&key)
                .copied()
        })
    }

    pub fn effective_lane_analysis_source(&self, lane_id: &str) -> AudioAnalysisSource {
        self.lane_analysis_source(lane_id)
            .unwrap_or(self.current_effective_settings().audio_source)
    }

    pub fn set_temporal_mode(&mut self, mode: Option<TemporalMode>, lane_id: &str) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let sequence_key = self.current_sequence_key();
        let overrides = self.track_overrides.entry(sequence_key).or_default();
        if let Some(mode) = mode {
            overrides.temporal_keys.insert(key, mode);
            for id in ids {
                overrides.temporal.insert(id, mode);
            }
        } else {
            overrides.temporal_keys.remove(&key);
            for id in ids {
                overrides.temporal.remove(&id);
            }
        }
        true
    }

    pub fn lane_temporal_mode(&self, lane_id: &str) -> Option<TemporalMode> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .temporal_keys
                .get(&key)
                .copied()
        })
    }

    /// Current lane mode + whether any grouped clip offers its evidence.
    /// The menu shows an explicit "no data — stable order" note when
    /// false; sync itself never blocks on it.
    pub fn temporal_availability(&self, lane_id: &str) -> (TemporalMode, bool) {
        let mode = self
            .lane_temporal_mode(lane_id)
            .unwrap_or(self.current_effective_settings().temporal_mode);
        let Some(result) = &self.result else {
            return (mode, true);
        };
        let Some((_, ids)) = self.lane_group(lane_id) else {
            return (mode, true);
        };
        let any = result
            .project
            .clips
            .iter()
            .filter(|c| ids.contains(&c.id))
            .any(|c| mode.has_evidence(c));
        (mode, any)
    }

    pub fn set_lane_search_accuracy(
        &mut self,
        accuracy: Option<align_core::SearchAccuracy>,
        lane_id: &str,
    ) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        if let Some(accuracy) = accuracy {
            let sequence_key = self.current_sequence_key();
            let overrides = self.track_overrides.entry(sequence_key).or_default();
            overrides.search_keys.insert(key, accuracy);
            for id in ids {
                overrides.search.insert(id, accuracy);
            }
        } else {
            let sequence_key = self.current_sequence_key();
            let overrides = self.track_overrides.entry(sequence_key).or_default();
            overrides.search_keys.remove(&key);
            for id in ids {
                overrides.search.remove(&id);
            }
        }
        true
    }

    pub fn lane_search_accuracy(&self, lane_id: &str) -> Option<align_core::SearchAccuracy> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .search_keys
                .get(&key)
                .copied()
        })
    }

    pub fn set_match_threshold(
        &mut self,
        threshold: Option<MatchThreshold>,
        lane_id: &str,
    ) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let sequence_key = self.current_sequence_key();
        let overrides = self.track_overrides.entry(sequence_key).or_default();
        if let Some(threshold) = threshold {
            overrides.threshold_keys.insert(key, threshold);
            for id in ids {
                overrides.thresholds.insert(id, threshold);
            }
        } else {
            overrides.threshold_keys.remove(&key);
            for id in ids {
                overrides.thresholds.remove(&id);
            }
        }
        true
    }

    pub fn lane_match_threshold(&self, lane_id: &str) -> Option<MatchThreshold> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .threshold_keys
                .get(&key)
                .copied()
        })
    }

    pub fn set_clip_order(&mut self, mode: Option<ClipOrder>, lane_id: &str) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let sequence_key = self.current_sequence_key();
        let overrides = self.track_overrides.entry(sequence_key).or_default();
        if let Some(mode) = mode {
            overrides.order_keys.insert(key, mode);
            for id in ids {
                overrides.orders.insert(id, mode);
            }
        } else {
            overrides.order_keys.remove(&key);
            for id in ids {
                overrides.orders.remove(&id);
            }
        }
        true
    }

    pub fn lane_clip_order(&self, lane_id: &str) -> Option<ClipOrder> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .order_keys
                .get(&key)
                .copied()
        })
    }

    pub fn set_track_content(&mut self, mode: Option<TrackContent>, lane_id: &str) -> bool {
        let Some((key, ids)) = self.lane_group(lane_id) else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let sequence_key = self.current_sequence_key();
        let overrides = self.track_overrides.entry(sequence_key).or_default();
        if let Some(mode) = mode {
            overrides.content_keys.insert(key, mode);
            for id in ids {
                overrides.contents.insert(id, mode);
            }
        } else {
            overrides.content_keys.remove(&key);
            for id in ids {
                overrides.contents.remove(&id);
            }
        }
        true
    }

    pub fn lane_track_content(&self, lane_id: &str) -> Option<TrackContent> {
        self.lane_group(lane_id).and_then(|(key, _)| {
            self.current_track_overrides()?
                .content_keys
                .get(&key)
                .copied()
        })
    }

    pub fn lane_can_preserve_editing(&self, lane_id: &str) -> bool {
        self.lanes.iter().any(|lane| {
            lane.id == lane_id
                && (lane.source_key.starts_with("imported-video-")
                    || lane.source_key.starts_with("imported-audio-"))
        })
    }

    pub fn lane_preserves_editing(&self, lane_id: &str) -> bool {
        let Some(lane) = self.lanes.iter().find(|lane| lane.id == lane_id) else {
            return false;
        };
        self.current_track_overrides()
            .is_some_and(|overrides| overrides.preserve_editing.contains(&lane.source_key))
    }

    pub fn set_preserve_editing(&mut self, preserve: bool, lane_id: &str) -> bool {
        let Some(key) = self
            .lanes
            .iter()
            .find(|lane| lane.id == lane_id)
            .map(|lane| lane.source_key.clone())
            .filter(|key| key.starts_with("imported-video-") || key.starts_with("imported-audio-"))
        else {
            return false;
        };
        let overrides = self
            .track_overrides
            .entry(self.current_sequence_key())
            .or_default();
        if preserve {
            overrides.preserve_editing.insert(key)
        } else {
            overrides.preserve_editing.remove(&key)
        }
    }

    pub fn reset_corrections(&mut self) -> bool {
        let sequence_key = self.current_sequence_key();
        if self.current_constraints().is_empty()
            && self
                .track_overrides
                .get(&sequence_key)
                .is_none_or(TrackOverrides::is_empty)
        {
            return false;
        }
        self.sequence_constraints.remove(&sequence_key);
        self.track_overrides.remove(&sequence_key);
        true
    }

    // ---------------- export sheet + diagnostics

    pub fn export_targets(&self) -> Vec<TimelineExportFormat> {
        let mut formats = Vec::new();
        for target in ExportTarget::all() {
            if self.export_selected.contains(&target) {
                if target == ExportTarget::FinalCutPro
                    && !self.export_fcpxml_timeline
                    && !self.export_fcpxml_multicam
                {
                    continue;
                }
                formats.extend(target.formats());
            }
        }
        formats
    }

    pub fn reveal_export(&self) {
        if self.exported_files.is_empty() {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let mut cmd = std::process::Command::new("open");
            cmd.arg("-R");
            for file in &self.exported_files {
                cmd.arg(file);
            }
            let _ = cmd.spawn();
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(first) = self.exported_files.first() {
                let _ = std::process::Command::new("explorer")
                    .arg("/select,")
                    .arg(first)
                    .spawn();
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(first) = self.exported_files.first() {
                let dir = first.parent().unwrap_or(first);
                let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
            }
        }
    }

    pub fn dismiss_error(&mut self) {
        self.error = None;
    }

    pub fn has_missing_media_warning(&self) -> bool {
        self.warnings.iter().any(|warning| {
            warning.contains("Timeline media could not be opened:")
                || warning.contains("Could not relink ")
        })
    }

    pub fn can_locate_timeline_media(&self) -> bool {
        self.inputs.iter().any(|path| is_timeline(path))
            && (self.has_missing_media_warning()
                || self.error.as_deref() == Some("No readable audio or video media was found."))
    }

    /// Add exact file choices from the native picker. A later choice for
    /// the same filename replaces the earlier one, matching the CLI's
    /// one-choice-per-missing-name contract.
    pub fn add_manual_relinks(&mut self, paths: Vec<PathBuf>) -> usize {
        let mut added = 0;
        for path in paths {
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            self.manual_relinks
                .retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
            self.manual_relinks.push((name, path));
            added += 1;
        }
        added
    }

    pub fn set_omit_extensions(&mut self, text: &str) {
        let mut values: Vec<String> = text
            .split([',', ';', ' '])
            .map(|value| value.trim().trim_start_matches('.').to_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        values.sort();
        values.dedup();
        self.omit_extensions = values;
    }

    pub fn add_path_redirection(&mut self, from: &str, to: PathBuf) -> Result<(), String> {
        let from = from.trim().trim_end_matches(['/', '\\']);
        if from.is_empty() {
            return Err("Enter the old folder path.".to_string());
        }
        if !to.is_dir() {
            return Err(format!("Folder does not exist: {}", to.display()));
        }
        self.redirects.retain(|entry| entry.from_prefix != from);
        self.redirects
            .push(align_core::redirect::PathRedirection::new(from, to));
        self.redirects
            .sort_by(|left, right| left.from_prefix.cmp(&right.from_prefix));
        self.path_fixer_dir = None;
        Ok(())
    }

    pub fn remove_path_redirection(&mut self, index: usize) {
        if index >= self.redirects.len() {
            return;
        }
        self.redirects.remove(index);
    }

    pub fn save_path_redirections(&self) {
        align_core::redirect::save_to(&align_core::redirect::config_file(), &self.redirects);
    }

    /// Media represented by the current session/result, for project-only
    /// analysis-cache cleanup. Timeline containers themselves have no
    /// fingerprint entry.
    pub fn current_cache_media(&self) -> Vec<PathBuf> {
        let mut media: Vec<PathBuf> = self
            .sequence_results
            .iter()
            .flat_map(|result| result.project.clips.iter().map(|clip| clip.url.clone()))
            .chain(
                self.result
                    .iter()
                    .flat_map(|result| result.project.clips.iter().map(|clip| clip.url.clone())),
            )
            .chain(
                self.clips
                    .iter()
                    .filter(|clip| !is_timeline(&clip.url))
                    .map(|clip| clip.url.clone()),
            )
            .collect();
        media.sort();
        media.dedup();
        media
    }

    pub fn discard_path_redirection_edits(&mut self) {
        self.redirects = align_core::redirect::load_from(&align_core::redirect::config_file());
        self.path_fixer_dir = None;
    }

    pub fn path_repair_source(&self) -> Option<&std::path::Path> {
        self.inputs
            .iter()
            .find(|path| align_decode::timeline::is_supported(path))
            .map(PathBuf::as_path)
    }

    pub fn path_repair_inputs(&self) -> Vec<PipelineInput> {
        self.inputs
            .iter()
            .cloned()
            .map(PipelineInput::Media)
            .collect()
    }

    pub fn path_repair_options(&self) -> PipelineOptions {
        PipelineOptions {
            redirects: self.redirects.clone(),
            manual_relinks: self.manual_relinks.clone(),
            prefer_proxies: self.prefer_proxies,
            ..Default::default()
        }
    }

    pub fn pipeline_input_sets(&self) -> Vec<Vec<PipelineInput>> {
        let selected = self.selected_sequence_indices();
        if selected.len() > 1 {
            return selected
                .into_iter()
                .map(|index| self.pipeline_inputs_for_sequence(Some(index)))
                .collect();
        }
        vec![self.pipeline_inputs_for_sequence(selected.first().copied())]
    }

    fn previous_result_for_position(&self, position: usize) -> Option<&SyncResult> {
        self.sequence_results
            .get(position)
            .or_else(|| (position == 0).then_some(self.result.as_ref()).flatten())
    }

    pub fn quality_retry_targets_for_position(&self, position: usize) -> Option<HashSet<ClipId>> {
        if !self.incremental_quality_run {
            return None;
        }
        let targets: HashSet<ClipId> = self
            .previous_result_for_position(position)?
            .unmatched
            .iter()
            .cloned()
            .collect();
        (!targets.is_empty()).then_some(targets)
    }

    pub fn quality_retry_urls_for_position(&self, position: usize) -> HashSet<PathBuf> {
        let Some(targets) = self.quality_retry_targets_for_position(position) else {
            return HashSet::new();
        };
        self.previous_result_for_position(position)
            .into_iter()
            .flat_map(|result| result.project.clips.iter())
            .filter(|clip| targets.contains(&clip.id))
            .map(|clip| clip.url.clone())
            .collect()
    }

    fn pipeline_inputs_for_sequence(&self, selected: Option<usize>) -> Vec<PipelineInput> {
        self.inputs
            .iter()
            .map(|path| {
                if !is_timeline(path) {
                    return PipelineInput::Media(path.clone());
                }
                match selected.or_else(|| {
                    self.timeline_choices
                        .get(path)
                        .and_then(|indices| indices.first().copied())
                }) {
                    Some(index) => PipelineInput::TimelineSequence(path.clone(), index),
                    None => PipelineInput::Timeline(path.clone()),
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub fn pipeline_options(&self) -> PipelineOptions {
        self.pipeline_options_for_sequence(self.current_sequence_key())
    }

    pub fn pipeline_options_for_sequence(&self, sequence_key: usize) -> PipelineOptions {
        let defaults = self.effective_settings_for(sequence_key);
        let overrides = self
            .track_overrides
            .get(&sequence_key)
            .cloned()
            .unwrap_or_default();
        PipelineOptions {
            generate_waveform_previews: true,
            search_accuracy: defaults.search_accuracy,
            search_overrides: overrides.search,
            source_search_overrides: overrides.search_keys,
            audio_source: defaults.audio_source,
            audio_sources: overrides.audio,
            match_policy: align_core::MatchPolicy {
                default: defaults.match_threshold,
                thresholds: overrides.thresholds,
            },
            clip_order: align_core::ClipOrderPolicy {
                default: defaults.clip_order,
                modes: overrides.orders,
            },
            track_content: align_core::TrackContentPolicy {
                default: defaults.track_content,
                modes: overrides.contents,
            },
            preserve_editing_tracks: overrides.preserve_editing,
            temporal: align_core::TemporalPolicy {
                default: defaults.temporal_mode,
                modes: overrides.temporal,
            },
            redirects: self.redirects.clone(),
            manual_relinks: self.manual_relinks.clone(),
            omit_extensions: self.omit_extensions.clone(),
            prefer_proxies: self.prefer_proxies,
        }
    }

    pub fn pipeline_options_for_run(&self, position: usize) -> PipelineOptions {
        let sequence_key = self.sequence_key_for_position(position);
        let mut options = self.pipeline_options_for_sequence(sequence_key);
        let Some(targets) = self.quality_retry_targets_for_position(position) else {
            return options;
        };
        let Some(previous) = self.previous_result_for_position(position) else {
            return options;
        };

        let requested = options.search_accuracy;
        let current_overrides = std::mem::take(&mut options.search_overrides);
        options.search_accuracy = previous.search_accuracy;
        options.search_overrides = previous.search_overrides.clone();
        for target in targets {
            options.search_overrides.insert(
                target.clone(),
                current_overrides.get(&target).copied().unwrap_or(requested),
            );
        }
        // The previous result already resolved source-level values into
        // per-clip overrides. Reapplying source defaults would upgrade
        // stable clips along with the unmatched retry targets.
        options.source_search_overrides.clear();
        options
    }
}

// ---------------- helpers (mirror private AppModel helpers)

fn drift_of_placement(result: &SyncResult, clip_id: &ClipId) -> f64 {
    let placement = result
        .islands
        .iter()
        .flat_map(|i| i.placements.iter())
        .find(|p| &p.clip_id == clip_id);
    let Some(placement) = placement else {
        return 0.0;
    };
    let (Some(first), Some(last)) = (
        placement.mapping.points.first(),
        placement.mapping.points.last(),
    ) else {
        return 0.0;
    };
    let source = last.source.as_seconds() - first.source.as_seconds();
    if source <= 0.0 {
        return 0.0;
    }
    let mapped = last.island.as_seconds() - first.island.as_seconds();
    (mapped / source - 1.0) * 1_000_000.0
}

fn is_timeline(path: &std::path::Path) -> bool {
    align_decode::timeline::is_supported(path)
}

fn sorted_live_matches(map: &HashMap<String, LiveMatch>) -> Vec<LiveMatch> {
    let mut rows: Vec<LiveMatch> = map.values().cloned().collect();
    rows.sort_by(|a, b| LiveMatch::id(&a.left, &a.right).cmp(&LiveMatch::id(&b.left, &b.right)));
    rows
}

/// Preview items for imported timelines (mirror `timelinePreview`).
type ImportedPreview = (
    Vec<BarVisual>,
    Vec<(ClipId, f64, MatchEvidence)>,
    Option<f64>,
);

fn imported_preview(result: &SyncResult) -> ImportedPreview {
    use align_core::export_model::ExportTimeline;
    let Ok(timeline) = ExportTimeline::from_result(result, true) else {
        let fallback = lane::final_bars(result);
        return (fallback.bars, fallback.states, fallback.ruler_timecode);
    };
    let unmatched: HashSet<ClipId> = result.unmatched.iter().cloned().collect();
    let bars: Vec<BarVisual> = timeline
        .preview_items(&unmatched)
        .into_iter()
        .map(|item| {
            let state = if unmatched.contains(&item.clip_id) {
                lane::BarMatchState::Unmatched
            } else {
                lane::BarMatchState::Matched
            };
            BarVisual {
                id: item.id.clone(),
                clip_id: item.clip_id.clone(),
                url: item.url.clone(),
                source_key: item.source_key.clone(),
                name: item.name.clone(),
                kind: item.kind,
                start: item.start,
                duration: item.duration,
                confidence: item.confidence,
                match_state: state,
            }
        })
        .collect();
    let states = bars
        .iter()
        .map(|b| (b.clip_id.clone(), b.confidence, MatchEvidence::Waveform))
        .collect();
    (bars, states, timeline.ruler_timecode_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_defaults_respects_the_selected_scope() {
        let mut data = AppData::default();
        data.common_settings.search_accuracy = align_core::SearchAccuracy::Exhaustive;
        assert!(!data.scoped_settings_are_default());
        assert!(data.reset_scoped_settings());
        assert_eq!(data.common_settings, SyncDefaults::default());

        data.settings_scope = SettingsScope::CurrentSequence;
        assert!(data.set_scoped_clip_order(Some(ClipOrder::ByFileName)));
        assert!(!data.scoped_settings_are_default());
        assert!(data.reset_scoped_settings());
        assert_eq!(
            data.current_sequence_settings(),
            SequenceDefaults::default()
        );
        assert!(!data.reset_scoped_settings());
    }

    fn empty_sequence_result(name: &str) -> SyncResult {
        SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: align_core::SyncProject {
                clips: Vec::new(),
                warnings: Vec::new(),
                imported_timeline: Some(align_core::ImportedTimeline {
                    name: name.into(),
                    frame_duration: align_core::MediaTime::new(1, 25),
                    edits: Vec::new(),
                }),
            },
            islands: Vec::new(),
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: Default::default(),
        }
    }

    #[test]
    fn import_all_sequences_builds_one_pipeline_input_set_per_sequence() {
        let path = PathBuf::from("/tmp/project.xml");
        let media = PathBuf::from("/tmp/extra.wav");
        let mut data = AppData::default();
        data.inputs.push(media.clone());
        data.sequence_picker = Some(SequencePicker {
            path: path.clone(),
            options: vec![
                TimelineSequenceSummary {
                    index: 0,
                    name: "Morning".into(),
                    clip_count: 2,
                },
                TimelineSequenceSummary {
                    index: 1,
                    name: "Evening".into(),
                    clip_count: 3,
                },
            ],
        });

        data.choose_all_sequences();

        assert_eq!(data.timeline_choices[&path], vec![0, 1]);
        let sets = data.pipeline_input_sets();
        assert_eq!(sets.len(), 2);
        assert!(sets[0].contains(&PipelineInput::Media(media.clone())));
        assert!(sets[1].contains(&PipelineInput::Media(media)));
        assert!(sets[0].contains(&PipelineInput::TimelineSequence(path.clone(), 0)));
        assert!(sets[1].contains(&PipelineInput::TimelineSequence(path, 1)));
    }

    #[test]
    fn retained_sequence_results_can_be_switched_independently() {
        let mut data = AppData::default();
        data.apply_results(vec![
            empty_sequence_result("Morning"),
            empty_sequence_result("Evening"),
        ]);

        assert_eq!(data.sequence_results.len(), 2);
        assert_eq!(
            data.result
                .as_ref()
                .unwrap()
                .project
                .imported_timeline
                .as_ref()
                .unwrap()
                .name,
            "Morning"
        );
        assert!(data.select_sequence_result(1));
        assert_eq!(data.active_sequence_result, 1);
        assert_eq!(
            data.result
                .as_ref()
                .unwrap()
                .project
                .imported_timeline
                .as_ref()
                .unwrap()
                .name,
            "Evening"
        );
        assert_eq!(
            data.sequence_results[0]
                .project
                .imported_timeline
                .as_ref()
                .unwrap()
                .name,
            "Morning"
        );
        assert_eq!(
            data.sequence_results[1]
                .project
                .imported_timeline
                .as_ref()
                .unwrap()
                .name,
            "Evening"
        );

        data.sequence_constraints.insert(
            1,
            vec![SyncConstraint::rejecting_pair(
                ClipId::new("a"),
                ClipId::new("b"),
            )],
        );
        assert!(data.constraints_for_sequence(0).is_empty());
        assert_eq!(data.constraints_for_sequence(1).len(), 1);

        data.apply_results(vec![
            empty_sequence_result("Morning rerun"),
            empty_sequence_result("Evening rerun"),
        ]);
        assert_eq!(data.active_sequence_result, 1);
        assert_eq!(
            data.result
                .as_ref()
                .unwrap()
                .project
                .imported_timeline
                .as_ref()
                .unwrap()
                .name,
            "Evening rerun"
        );
    }

    /// Synthetic result fixture (no corpus, no pipeline): one island of
    /// two clips plus one unmatched singleton. Guards the "all 25"
    /// regression where unmatched singletons leaked into `states` and rows
    /// read Synchronized instead of Unmatched.
    #[test]
    fn apply_result_marks_unmatched_and_packs_lanes() {
        use align_core::{
            ClipPlacement, MappingPoint, MatchSummary, MediaTime, SyncIsland, SyncProject, TimeMap,
        };

        let clip = |name: &str, kind: MediaKind, duration: f64| Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/v/{name}")),
            kind,
            duration: MediaTime::seconds(duration),
            audio: vec![align_core::AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: None,
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let placement = |name: &str, island_start: f64, duration: f64| ClipPlacement {
            clip_id: ClipId::new(name),
            mapping: TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(island_start),
                    },
                    MappingPoint {
                        source: MediaTime::seconds(duration),
                        island: MediaTime::seconds(island_start + duration),
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
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: vec![
                    clip("a.mov", MediaKind::Video, 10.0),
                    clip("b.wav", MediaKind::Audio, 10.0),
                    clip("c.wav", MediaKind::Audio, 5.0),
                ],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement("a.mov", 0.0, 10.0), placement("b.wav", 0.5, 10.0)],
            }],
            unmatched: vec![ClipId::new("c.wav")],
            matches: vec![MatchSummary {
                left: ClipId::new("a.mov"),
                right: ClipId::new("b.wav"),
                drift_ppm: 0.0,
                offset: MediaTime::seconds(0.5),
                confidence: 0.9,
                anchors: 10,
                covered: MediaTime::seconds(9.5),
                residual: MediaTime::seconds(0.001),
                evidence: Some(MatchEvidence::Waveform),
            }],
            temporal_policy: align_core::TemporalPolicy::default(),
        };
        result.stages = vec![
            align_core::SyncStage {
                kind: align_core::SyncStageKind::Waveform,
                islands: Vec::new(),
                matches: Vec::new(),
                unmatched: result
                    .project
                    .clips
                    .iter()
                    .map(|clip| clip.id.clone())
                    .collect(),
            },
            align_core::SyncStage {
                kind: align_core::SyncStageKind::Timecode,
                islands: result.islands.clone(),
                matches: result.matches.clone(),
                unmatched: result.unmatched.clone(),
            },
        ];
        result.selected_stage = Some(1);
        let mut data = AppData::default();
        data.apply_result(result);
        assert_eq!(
            data.status, "Synchronized 2 of 3 clips.",
            "status={}",
            data.status
        );
        assert_eq!(data.unmatched_count(), 1);
        // The singleton reads Unmatched, not Synchronized.
        let states: HashMap<_, _> = data
            .clips
            .iter()
            .map(|r| (r.name.clone(), format!("{:?}", r.state)))
            .collect();
        assert!(states["a.mov"].starts_with("Synchronized"));
        assert!(states["b.wav"].starts_with("Synchronized"));
        assert_eq!(states["c.wav"], "Unmatched");
        // Lanes pack without overlap.
        for lane in &data.lanes {
            let mut bars = lane.clips.clone();
            bars.sort_by(|a, b| a.start.total_cmp(&b.start));
            for pair in bars.windows(2) {
                assert!(
                    pair[0].start + pair[0].duration <= pair[1].start + 0.005,
                    "overlap in lane {}: {} vs {}",
                    lane.id,
                    pair[0].name,
                    pair[1].name
                );
            }
        }
        // Orange bars are exactly the unmatched set.
        let orange = data
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .filter(|b| b.match_state == crate::lane::BarMatchState::Unmatched)
            .count();
        assert_eq!(orange, 1);
        // Every bar spans (nearly) its whole clip.
        for row in &data.clips {
            if let (Some(id), Some(duration)) = (&row.clip_id, row.duration) {
                let bar = data
                    .lanes
                    .iter()
                    .flat_map(|l| l.clips.iter())
                    .find(|b| &b.clip_id == id)
                    .expect("bar for clip");
                assert!(
                    (bar.duration - duration).abs() <= duration.max(1.0) * 0.05 + 0.05,
                    "{}: bar {} vs clip {}",
                    row.name,
                    bar.duration,
                    duration
                );
            }
        }
        data.exported_files
            .push(PathBuf::from("/tmp/previous-stage.xml"));
        data.sequence_constraints.insert(
            0,
            vec![SyncConstraint::rejecting_pair(
                ClipId::new("x"),
                ClipId::new("y"),
            )],
        );
        let constraints = data.current_constraints().to_vec();
        assert!(data.select_sync_stage(0));
        assert_eq!(data.unmatched_count(), 3);
        assert!(data.live_matches.is_empty());
        assert!(data.exported_files.is_empty());
        assert_eq!(data.current_constraints(), constraints);
        assert!(data.select_sync_stage(1));
        assert_eq!(data.unmatched_count(), 1);
        assert_eq!(data.live_matches.len(), 1);
        data.pending_count = 1;
        assert!(
            !data.select_sync_stage(0),
            "stale inputs cannot select old stages"
        );
        data.pending_count = 0;
        data.operation = Operation::Synchronizing;
        assert!(
            !data.select_sync_stage(0),
            "active sync cannot change stages"
        );
        let queued_path = PathBuf::from("/v/new.wav");
        data.clips.push(ClipRow {
            clip_id: None,
            url: queued_path.clone(),
            name: "new.wav".into(),
            kind: None,
            duration: None,
            timecode: None,
            state: ClipState::Analyzing,
        });
        data.pending_count = 1;
        data.restore_after_sync_cancel();
        assert_eq!(data.clips.len(), 4);
        assert_eq!(
            data.clips
                .iter()
                .find(|row| row.url == queued_path)
                .unwrap()
                .state,
            ClipState::Queued
        );
        assert!(data.is_stale());
        assert!(!data.can_export());
        assert!(data.can_synchronize());
    }

    #[test]
    fn common_sequence_and_track_settings_resolve_in_order() {
        use crate::lane::{BarMatchState, BarVisual, LaneVisual};

        let bar = |name: &str| BarVisual {
            id: name.to_string(),
            clip_id: ClipId::new(name),
            url: PathBuf::from(format!("/v/{name}")),
            source_key: "/v".to_string(),
            name: name.to_string(),
            kind: MediaKind::Audio,
            start: 0.0,
            duration: 10.0,
            confidence: 0.0,
            match_state: BarMatchState::Unmatched,
        };
        let mut packed_other_source = bar("c.wav");
        packed_other_source.source_key = "/other".to_string();
        let mut data = AppData::default();
        data.inputs.push(PathBuf::from("/tmp/project.xml"));
        data.timeline_choices
            .insert(PathBuf::from("/tmp/project.xml"), vec![3, 8]);
        data.lanes = vec![LaneVisual {
            id: "lane-1".to_string(),
            kind: MediaKind::Audio,
            number: 0,
            source_key: "/v".to_string(),
            source_name: "src".to_string(),
            stream_channels: Vec::new(),
            clips: vec![bar("a.wav"), bar("b.wav"), packed_other_source],
        }];

        data.common_settings = SyncDefaults {
            search_accuracy: align_core::SearchAccuracy::Fast,
            audio_source: AudioAnalysisSource::AllMixed,
            temporal_mode: TemporalMode::RecStart,
            match_threshold: MatchThreshold::Conservative,
            clip_order: ClipOrder::ByDateTime,
            track_content: TrackContent::Linear,
        };
        assert_eq!(data.current_sequence_key(), 3);
        assert_eq!(data.current_effective_settings(), data.common_settings);

        data.settings_scope = SettingsScope::CurrentSequence;
        assert!(data.set_scoped_search_accuracy(Some(align_core::SearchAccuracy::Exhaustive)));
        assert!(data.set_scoped_temporal_mode(Some(TemporalMode::RecStop)));
        assert!(data.set_scoped_track_content(Some(TrackContent::Takes)));
        let sequence = data.pipeline_options_for_sequence(3);
        assert_eq!(
            sequence.search_accuracy,
            align_core::SearchAccuracy::Exhaustive
        );
        assert_eq!(sequence.audio_source, AudioAnalysisSource::AllMixed);
        assert_eq!(sequence.temporal.default, TemporalMode::RecStop);
        assert_eq!(sequence.match_policy.default, MatchThreshold::Conservative);
        assert_eq!(sequence.clip_order.default, ClipOrder::ByDateTime);
        assert_eq!(sequence.track_content.default, TrackContent::Takes);

        // Every enum default remains a real track override. Only `None`
        // means inherit from the current sequence.
        assert!(data.set_analysis_source(Some(AudioAnalysisSource::Automatic), "lane-1"));
        assert!(data.set_temporal_mode(Some(TemporalMode::Auto), "lane-1"));
        assert!(data.set_match_threshold(Some(MatchThreshold::Balanced), "lane-1"));
        assert!(data.set_clip_order(Some(ClipOrder::Auto), "lane-1"));
        assert!(data.set_track_content(Some(TrackContent::Auto), "lane-1"));
        assert!(
            data.set_lane_search_accuracy(Some(align_core::SearchAccuracy::Thorough), "lane-1")
        );
        assert_eq!(
            data.lane_analysis_source("lane-1"),
            Some(AudioAnalysisSource::Automatic)
        );
        assert_eq!(data.lane_temporal_mode("lane-1"), Some(TemporalMode::Auto));
        assert_eq!(
            data.lane_match_threshold("lane-1"),
            Some(MatchThreshold::Balanced)
        );
        assert_eq!(data.lane_clip_order("lane-1"), Some(ClipOrder::Auto));
        assert_eq!(data.lane_track_content("lane-1"), Some(TrackContent::Auto));
        let track = data.pipeline_options_for_sequence(3);
        let a = ClipId::new("a.wav");
        assert_eq!(track.audio_sources[&a], AudioAnalysisSource::Automatic);
        assert_eq!(track.temporal.resolve(&a), TemporalMode::Auto);
        assert_eq!(track.match_policy.resolve(&a), MatchThreshold::Balanced);
        assert_eq!(track.clip_order.resolve(&a), ClipOrder::Auto);
        assert_eq!(track.track_content.resolve(&a), TrackContent::Auto);
        assert_eq!(
            track.search_overrides[&a],
            align_core::SearchAccuracy::Thorough
        );
        assert!(!track.audio_sources.contains_key(&ClipId::new("c.wav")));

        // A second sequence keeps independent defaults and track overrides.
        data.active_sequence_result = 1;
        assert_eq!(data.current_sequence_key(), 8);
        let second = data.pipeline_options_for_sequence(8);
        assert_eq!(second.search_accuracy, align_core::SearchAccuracy::Fast);
        assert_eq!(second.audio_source, AudioAnalysisSource::AllMixed);
        assert!(second.audio_sources.is_empty());
        assert_eq!(data.lane_analysis_source("lane-1"), None);

        data.active_sequence_result = 0;
        assert!(data.set_analysis_source(None, "lane-1"));
        assert!(data.set_temporal_mode(None, "lane-1"));
        assert!(data.set_match_threshold(None, "lane-1"));
        assert!(data.set_clip_order(None, "lane-1"));
        assert!(data.set_track_content(None, "lane-1"));
        assert!(data.set_lane_search_accuracy(None, "lane-1"));
        let inherited = data.pipeline_options_for_sequence(3);
        assert!(inherited.audio_sources.is_empty());
        assert_eq!(inherited.temporal.resolve(&a), TemporalMode::RecStop);
        assert_eq!(
            inherited.match_policy.resolve(&a),
            MatchThreshold::Conservative
        );
        assert_eq!(inherited.clip_order.resolve(&a), ClipOrder::ByDateTime);
        assert_eq!(inherited.track_content.resolve(&a), TrackContent::Takes);
        assert!(inherited.search_overrides.is_empty());
    }

    #[test]
    fn preserve_basic_editing_is_an_imported_track_override() {
        use crate::lane::{BarMatchState, BarVisual, LaneVisual};

        let mut data = AppData {
            inputs: vec![PathBuf::from("/tmp/project.xml")],
            timeline_choices: [(PathBuf::from("/tmp/project.xml"), vec![3, 8])]
                .into_iter()
                .collect(),
            lanes: vec![LaneVisual {
                id: "audio-0".into(),
                kind: MediaKind::Audio,
                number: 1,
                source_key: "imported-audio-000002".into(),
                source_name: "Imported A2".into(),
                stream_channels: Vec::new(),
                clips: vec![BarVisual {
                    id: "edit".into(),
                    clip_id: ClipId::new("clip"),
                    url: PathBuf::from("/v/clip.wav"),
                    source_key: "imported-audio-000002".into(),
                    name: "clip.wav".into(),
                    kind: MediaKind::Audio,
                    start: 4.0,
                    duration: 2.0,
                    confidence: 0.9,
                    match_state: BarMatchState::Matched,
                }],
            }],
            ..Default::default()
        };
        assert!(data.lane_can_preserve_editing("audio-0"));
        assert!(!data.lane_preserves_editing("audio-0"));
        assert!(data.set_preserve_editing(true, "audio-0"));
        assert!(data.lane_preserves_editing("audio-0"));
        assert_eq!(
            data.pipeline_options_for_sequence(3)
                .preserve_editing_tracks
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["imported-audio-000002"]
        );
        data.active_sequence_result = 1;
        assert!(!data.lane_preserves_editing("audio-0"));
        assert!(
            data.pipeline_options_for_sequence(8)
                .preserve_editing_tracks
                .is_empty()
        );
        data.active_sequence_result = 0;
        assert!(data.reset_corrections());
        assert!(!data.lane_preserves_editing("audio-0"));
        assert!(data.set_preserve_editing(true, "audio-0"));
        assert!(data.set_preserve_editing(false, "audio-0"));
        assert!(!data.lane_preserves_editing("audio-0"));
        data.lanes[0].source_key = "/raw/folder".into();
        assert!(!data.lane_can_preserve_editing("audio-0"));
        assert!(!data.set_preserve_editing(true, "audio-0"));
    }

    #[test]
    fn status_text_mirrors_swift() {
        assert_eq!(ClipState::Queued.status_text(), "Queued");
        assert_eq!(
            ClipState::Matching { confidence: 0.637 }.status_text(),
            "Match 64%"
        );
        assert_eq!(
            ClipState::Synchronized {
                island: 0,
                confidence: 0.9,
                drift_ppm: 0.3,
                evidence: MatchEvidence::Waveform,
            }
            .status_text(),
            "90%"
        );
        assert_eq!(
            ClipState::Synchronized {
                island: 0,
                confidence: 0.9,
                drift_ppm: 12.0,
                evidence: MatchEvidence::Waveform,
            }
            .status_text(),
            "90% · 12 ppm"
        );
        assert_eq!(
            ClipState::Synchronized {
                island: 0,
                confidence: 0.9,
                drift_ppm: 0.0,
                evidence: MatchEvidence::Timecode,
            }
            .status_text(),
            "TC Sync"
        );
    }

    #[test]
    fn export_target_mapping() {
        assert_eq!(ExportTarget::ResolveOtio.formats().len(), 2);
        assert_eq!(ExportTarget::Premiere.formats().len(), 1);
        assert_eq!(ExportTarget::all().len(), 5);
        assert_eq!(ExportTarget::Aaf.formats(), vec![TimelineExportFormat::Aaf]);
    }

    #[test]
    fn media_only_export_is_a_valid_output_selection() {
        use align_core::SyncProject;
        let mut data = AppData::default();
        data.export_selected.clear();
        assert!(!data.export_output_selected());
        data.export_media = true;
        assert!(data.export_output_selected());
        assert!(
            !data.can_begin_export(),
            "a result and destination are still required"
        );
        data.result = Some(SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: Vec::new(),
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: Vec::new(),
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: align_core::TemporalPolicy::default(),
        });
        data.export_dir = Some("/tmp/align-export-test".into());
        assert!(
            data.can_begin_export(),
            "media-only export must enable the button"
        );
        data.operation = Operation::Exporting;
        assert!(!data.can_begin_export());
        data.operation = Operation::Ready;
        data.pending_count = 1;
        assert!(!data.can_begin_export(), "stale results must not export");
        data.pending_count = 0;
        data.export_media = false;
        data.export_selected.insert(ExportTarget::Premiere);
        assert!(data.export_output_selected());
    }

    #[test]
    fn add_remove_selection_session() {
        let mut data = AppData::default();
        data.add_paths(vec![
            PathBuf::from("/v/b.wav"),
            PathBuf::from("/v/a.wav"),
            PathBuf::from("/v/a.wav"),
        ]);
        // Deduped + sorted by path.
        assert_eq!(data.clips.len(), 2);
        assert_eq!(data.clips[0].url, PathBuf::from("/v/a.wav"));
        assert_eq!(data.inputs.len(), 2);
        assert!(data.can_synchronize());

        data.selection.insert(PathBuf::from("/v/a.wav"));
        data.remove_selection();
        assert_eq!(data.clips.len(), 1);
        assert!(data.selection.is_empty());
        assert_eq!(data.inputs, vec![PathBuf::from("/v/b.wav")]);
    }

    #[test]
    fn synchronize_requires_new_inputs_or_changed_settings_after_a_result() {
        let mut data = AppData::default();
        data.add_paths(vec![PathBuf::from("/v/a.wav")]);
        data.result = Some(empty_sequence_result("Finished"));
        data.operation = Operation::Ready;

        assert!(!data.can_synchronize());
        assert!(data.can_export());
        data.mark_sync_dirty();
        assert!(data.can_synchronize());
        assert!(!data.can_export());
        assert!(data.begin_sync_run());
        assert!(data.sync_dirty);

        data.operation = Operation::Ready;
        data.apply_result(empty_sequence_result("Rerun"));
        assert!(!data.sync_dirty);
        assert!(!data.can_synchronize());

        data.add_paths(vec![PathBuf::from("/v/b.wav")]);
        assert!(data.can_synchronize());
    }

    #[test]
    fn quality_retry_upgrades_only_unmatched_fingerprints() {
        let mut data = AppData::default();
        data.add_paths(vec![PathBuf::from("/v/a.wav")]);
        let unmatched = ClipId::new("unmatched");
        let matched = ClipId::new("matched");
        let mut previous = empty_sequence_result("Previous");
        previous.search_accuracy = align_core::SearchAccuracy::Balanced;
        previous.unmatched = vec![unmatched.clone()];
        previous
            .search_overrides
            .insert(matched.clone(), align_core::SearchAccuracy::Fast);
        data.result = Some(previous.clone());
        data.sequence_results = vec![previous];
        data.operation = Operation::Ready;
        data.common_settings.search_accuracy = align_core::SearchAccuracy::Deep;
        data.mark_quality_dirty();

        assert!(data.can_synchronize());
        assert!(data.begin_sync_run());
        assert_eq!(
            data.quality_retry_targets_for_position(0),
            Some([unmatched.clone()].into_iter().collect())
        );
        let options = data.pipeline_options_for_run(0);
        assert_eq!(
            options.search_accuracy,
            align_core::SearchAccuracy::Balanced
        );
        assert_eq!(
            options.search_overrides.get(&matched),
            Some(&align_core::SearchAccuracy::Fast)
        );
        assert_eq!(
            options.search_overrides.get(&unmatched),
            Some(&align_core::SearchAccuracy::Deep)
        );
    }

    #[test]
    fn quality_change_does_not_stale_a_fully_matched_result() {
        let mut data = AppData::default();
        data.add_paths(vec![PathBuf::from("/v/a.wav")]);
        let result = empty_sequence_result("Complete");
        data.result = Some(result.clone());
        data.sequence_results = vec![result];
        data.operation = Operation::Ready;

        data.mark_quality_dirty();
        assert!(!data.can_synchronize());
        assert!(data.can_export());
    }

    #[test]
    fn trash_removes_selection_then_clears_session() {
        let mut data = AppData::default();
        data.add_paths(vec![PathBuf::from("/v/a.wav"), PathBuf::from("/v/b.wav")]);
        data.selection.insert(PathBuf::from("/v/a.wav"));

        data.delete_or_clear();
        assert_eq!(data.inputs, vec![PathBuf::from("/v/b.wav")]);

        data.delete_or_clear();
        assert!(data.inputs.is_empty());
        assert!(data.clips.is_empty());
    }

    #[test]
    fn path_fixer_choices_flow_to_pipeline_and_survive_clear() {
        let mut data = AppData::default();
        data.redirects
            .push(align_core::redirect::PathRedirection::new(
                "/old/card",
                PathBuf::from("/new/card"),
            ));
        assert_eq!(
            data.add_manual_relinks(vec![
                PathBuf::from("/pick/first/A.WAV"),
                PathBuf::from("/pick/final/a.wav"),
            ]),
            2
        );
        let options = data.pipeline_options();
        assert_eq!(options.redirects, data.redirects);
        assert_eq!(
            options.manual_relinks,
            vec![("a.wav".to_string(), PathBuf::from("/pick/final/a.wav"))]
        );

        data.set_omit_extensions(".JPG, png; jpg  WAV");
        assert_eq!(data.omit_extensions, vec!["jpg", "png", "wav"]);
        assert_eq!(
            data.pipeline_options().omit_extensions,
            vec!["jpg", "png", "wav"]
        );
        data.inputs.insert(0, PathBuf::from("/v/edit.fcpxml"));
        assert_eq!(
            data.path_repair_source(),
            Some(std::path::Path::new("/v/edit.fcpxml"))
        );
        assert_eq!(data.path_repair_options().redirects, data.redirects);
        assert_eq!(data.path_repair_inputs().len(), data.inputs.len());
        data.add_paths(vec![PathBuf::from("/v/recorder.wav")]);
        assert!(data.can_synchronize());
        assert!(data.begin_path_repair());
        assert_eq!(data.operation, Operation::Repairing);
        assert!(!data.can_synchronize());

        data.clear();
        assert_eq!(data.redirects.len(), 1);
        assert!(data.manual_relinks.is_empty());
    }

    #[test]
    fn current_cache_media_excludes_timeline_containers() {
        let mut data = AppData::default();
        data.add_paths(vec![
            PathBuf::from("/v/edit.fcpxml"),
            PathBuf::from("/v/recorder.wav"),
        ]);
        assert_eq!(
            data.current_cache_media(),
            vec![PathBuf::from("/v/recorder.wav")]
        );
    }

    #[test]
    fn only_actionable_missing_media_warnings_offer_relink() {
        let mut data = AppData::default();
        data.inputs.push(PathBuf::from("/v/cut.xml"));
        data.warnings
            .push("camera.mov: Variable frame rate detected.".into());
        assert!(!data.has_missing_media_warning());
        assert!(!data.can_locate_timeline_media());
        data.warnings
            .push("cut.xml: Timeline media could not be opened: /old/card/A.WAV".into());
        assert!(data.has_missing_media_warning());
        assert!(data.can_locate_timeline_media());

        data.warnings.clear();
        data.error = Some("No readable audio or video media was found.".into());
        assert!(data.can_locate_timeline_media());
    }

    #[test]
    fn export_targets_follow_selection() {
        let mut data = AppData::default();
        assert_eq!(data.export_targets().len(), 2);
        data.export_selected.insert(ExportTarget::Premiere);
        let formats = data.export_targets();
        assert!(formats.contains(&TimelineExportFormat::PremiereXML));
        // Canonical order: OTIO, script, then Premiere.
        let names: Vec<_> = formats.iter().map(|f| f.file_extension()).collect();
        assert_eq!(names, vec!["otio", "py", "xml"]);

        data.export_selected.clear();
        data.export_selected.insert(ExportTarget::FinalCutPro);
        assert_eq!(
            data.export_targets(),
            vec![TimelineExportFormat::FinalCutProXML]
        );
        data.export_fcpxml_timeline = false;
        data.export_fcpxml_multicam = false;
        assert!(data.export_targets().is_empty());
        assert!(!data.export_output_selected());
    }

    #[test]
    fn zoom_factor_matches_native() {
        let mut data = AppData::default();
        assert!((data.zoom() - 1.0).abs() < 1e-9);
        data.zoom_level = 1.0;
        assert!((data.zoom() - 8.0).abs() < 1e-9);
        data.zoom_level = 0.5;
        assert!((data.zoom() - 8.0f64.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn has_drift_matches_actual_audio_export_jobs() {
        use align_core::{
            ClipPlacement, MappingPoint, MediaTime, SyncIsland, SyncProject, TimeMap,
        };

        let clip = |name: &str, kind: MediaKind| Clip {
            id: ClipId::new(name),
            url: PathBuf::from(format!("/v/{name}")),
            kind,
            duration: MediaTime::seconds(1_000.0),
            audio: Vec::new(),
            video: None,
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
                        source: MediaTime::seconds(1_000.0),
                        island: MediaTime::seconds(1_000.020),
                    },
                ],
            },
            confidence: 0.9,
        };
        let mut data = AppData::default();
        assert!(!data.has_drift());
        data.result = Some(SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: vec![clip("camera.mov", MediaKind::Video)],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement("camera.mov")],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: align_core::TemporalPolicy::default(),
        });
        assert!(!data.has_drift());
        let result = data.result.as_mut().expect("result");
        result
            .project
            .clips
            .push(clip("recorder.wav", MediaKind::Audio));
        result.islands[0].placements.push(placement("recorder.wav"));
        assert!(data.has_drift());
    }

    #[test]
    fn pan_clamps_to_measured_content() {
        let data = AppData::default();
        // No layout yet: max offset is zero, everything clamps to origin.
        data.pan_by(50.0, 30.0);
        let offset = data.timeline_scroll.offset();
        assert_eq!((f32::from(offset.x), f32::from(offset.y)), (0.0, 0.0));
        data.pan_to(Some(-500.0), Some(-500.0));
        let offset = data.timeline_scroll.offset();
        assert_eq!((f32::from(offset.x), f32::from(offset.y)), (0.0, 0.0));
    }

    #[test]
    fn zoom_label_and_clamping() {
        let mut data = AppData::default();
        assert_eq!(data.zoom_label(), "100%");
        data.zoom_level = 1.0;
        assert_eq!(data.zoom_label(), "800%");
        data.zoom_level = 0.5;
        assert_eq!(data.zoom_label(), "283%");
        data.zoom_at(1.5, 0.0);
        assert!((data.zoom_level - 1.0).abs() < 1e-9);
        data.zoom_at(-0.5, 0.0);
        assert!((data.zoom_level - 0.0).abs() < 1e-9);
        data.zoom_at(0.25, 0.0);
        assert!((data.zoom_level - 0.25).abs() < 1e-9);
    }

    #[test]
    fn zoom_at_keeps_the_time_under_the_pointer_fixed() {
        let mut data = AppData::default();
        data.timeline_scroll.set_offset(Point {
            x: px(-120.0),
            y: px(0.0),
        });
        let pointer_x = 300.0;
        let before = (pointer_x - f32::from(data.timeline_scroll.offset().x)) / data.zoom() as f32;

        data.zoom_at(0.5, pointer_x);

        let after = (pointer_x - f32::from(data.timeline_scroll.offset().x)) / data.zoom() as f32;
        assert!((before - after).abs() < 0.001, "{before} != {after}");
    }
}
