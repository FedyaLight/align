//! Shared media, synchronization, and timeline data types.
//!
//! Serialized field names are part of the saved-result and CLI JSON contracts.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- Clip IDs

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipId(pub String);

impl ClipId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }
}

impl std::fmt::Display for ClipId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------- Media time

/// Rational media timestamp: value divided by timescale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaTime {
    pub value: i64,
    pub timescale: i32,
}

impl MediaTime {
    pub fn new(value: i64, timescale: i32) -> Self {
        assert!(timescale > 0);
        Self { value, timescale }
    }

    pub fn seconds(value: f64) -> Self {
        Self {
            value: (value * 1_000_000.0).round() as i64,
            timescale: 1_000_000,
        }
    }

    /// Construct from seconds, rounded to microsecond precision.
    pub fn microseconds(value: f64) -> Self {
        Self {
            value: (value * 1_000_000.0).round() as i64,
            timescale: 1_000_000,
        }
    }

    pub fn as_seconds(&self) -> f64 {
        self.value as f64 / self.timescale as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingTimestampSource {
    EmbeddedMetadata,
    FileSystem,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSummary {
    pub sample_rate: f64,
    pub channels: usize,
    pub bit_depth: Option<u32>,
    pub is_float: Option<bool>,
    pub source_timecode: Option<SourceTimecode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoFrameRateMode {
    Constant,
    Variable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSummary {
    pub width: u32,
    pub height: u32,
    pub frame_duration: Option<MediaTime>,
    pub source_timecode: Option<SourceTimecode>,
    pub frame_rate_mode: Option<VideoFrameRateMode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSpan {
    pub identifier: String,
    pub part_number: usize,
    pub part_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTimecode {
    /// Display label (`HH:MM:SS:FF`, `;` iff drop-frame). Wraps at 24 h;
    /// never used for alignment, only for display.
    pub text: String,
    /// Elapsed-time numerator; the pair `(frame_number, frame_duration)` is
    /// the exact source instant — NOT a display-frame grid. For label
    /// sources it is the elapsed frame count at the true rate
    /// (`1001/30000` for 29.97, drop-compensated for DF); for BWF it is the
    /// raw `TimeReference` sample count at the audio rate. Elapsed time
    /// never wraps; only `text` does.
    pub frame_number: i64,
    /// Elapsed-time denominator (true rate, e.g. `1001/30000`, or the audio
    /// sample rate for BWF). Not a display rate: consumers must only use
    /// [`SourceTimecode::as_seconds`] and the [`SourceTimecode::text`]
    /// field.
    pub frame_duration: MediaTime,
    pub drop_frame: bool,
}

impl SourceTimecode {
    /// Exact elapsed seconds. The single alignment-relevant accessor —
    /// matcher, overlap and export all go through here, so DF/BWF/iXML
    /// details never leak past the constructors below.
    pub fn as_seconds(&self) -> f64 {
        self.frame_number as f64 * self.frame_duration.as_seconds()
    }

    /// Nominal whole frames per second of a true video rate
    /// (`1001/30000` → 30). `None` for degenerate durations.
    pub fn nominal_fps(frame_duration: MediaTime) -> Option<i64> {
        if frame_duration.value <= 0 || frame_duration.timescale <= 0 {
            return None;
        }
        let nominal = (frame_duration.timescale as f64 / frame_duration.value as f64).round();
        if nominal.is_finite() && (1.0..=120.0).contains(&nominal) {
            Some(nominal as i64)
        } else {
            None
        }
    }

    /// Dropped labels per minute for a nominal rate (SMPTE ST 12-1: DF is
    /// defined only for nominal 30 and 60). `None` = drop-frame forbidden.
    fn drop_per_minute(nominal: i64) -> Option<i64> {
        match nominal {
            30 => Some(2),
            60 => Some(4),
            _ => None,
        }
    }

    /// Claimed `HH:MM:SS:FF` components → validated timecode. `drop_frame`
    /// must come from the `;` separator (derived labels use
    /// [`SourceTimecode::from_frame_number`], which sanitizes instead).
    /// Skipped DF labels (`mm:00:00/01`, `mm:00:00–03` at 59.94) and DF at
    /// non-DF rates are `None` — never guessed.
    pub fn from_components(
        h: i64,
        m: i64,
        s: i64,
        f: i64,
        frame_duration: MediaTime,
        drop_frame: bool,
    ) -> Option<Self> {
        if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..60).contains(&s) {
            return None;
        }
        let nominal = Self::nominal_fps(frame_duration)?;
        if f < 0 || f >= nominal {
            return None;
        }
        let nominal_count = (h * 3600 + m * 60 + s) * nominal + f;
        let frame_number = if drop_frame {
            let drop = Self::drop_per_minute(nominal)?;
            if s == 0 && m % 10 != 0 && f < drop {
                return None;
            }
            let total_minutes = h * 60 + m;
            nominal_count - drop * (total_minutes - total_minutes / 10)
        } else {
            nominal_count
        };
        let sep = if drop_frame { ";" } else { ":" };
        Some(Self {
            text: format!("{h:02}:{m:02}:{s:02}{sep}{f:02}"),
            frame_number,
            frame_duration,
            drop_frame,
        })
    }

    /// Parse `HH:MM:SS:FF` (`;` = drop-frame) at a true video rate.
    /// Strict grammar: exactly 11 chars, 2 ASCII digits per component,
    /// `:` separators except the last (`:` NDF / `;` DF). Anything else —
    /// empty components, mixed separators, prefixes, `.` — is `None`.
    /// Ranges and DF rules per [`SourceTimecode::from_components`].
    pub fn from_label(text: &str, frame_duration: MediaTime) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 11
            || b[2] != b':'
            || b[5] != b':'
            || (b[8] != b':' && b[8] != b';')
            || !b[0].is_ascii_digit()
            || !b[1].is_ascii_digit()
            || !b[3].is_ascii_digit()
            || !b[4].is_ascii_digit()
            || !b[6].is_ascii_digit()
            || !b[7].is_ascii_digit()
            || !b[9].is_ascii_digit()
            || !b[10].is_ascii_digit()
        {
            return None;
        }
        let digits = |i: usize| i64::from(b[i] - b'0') * 10 + i64::from(b[i + 1] - b'0');
        Self::from_components(
            digits(0),
            digits(3),
            digits(6),
            digits(9),
            frame_duration,
            b[8] == b';',
        )
    }

    /// Elapsed frame count → timecode. Display wraps at 24 h, elapsed does
    /// not. A DF flag at a non-DF rate is sanitized to NDF (the count stays
    /// authoritative; only the derived label was contradictory) — claimed
    /// labels stay strict, derived labels stay total.
    pub fn from_frame_number(
        frame_number: i64,
        frame_duration: MediaTime,
        drop_frame: bool,
    ) -> Option<Self> {
        if frame_number < 0 {
            return None;
        }
        let nominal = Self::nominal_fps(frame_duration)?;
        let drop_frame = drop_frame && Self::drop_per_minute(nominal).is_some();
        let (h, m, s, f) = Self::label_from_elapsed(frame_number, nominal, drop_frame)?;
        let sep = if drop_frame { ";" } else { ":" };
        Some(Self {
            text: format!("{h:02}:{m:02}:{s:02}{sep}{f:02}"),
            frame_number,
            frame_duration,
            drop_frame,
        })
    }

    /// Inverse of the forward DF math: elapsed count → display components.
    fn label_from_elapsed(
        elapsed: i64,
        nominal: i64,
        drop_frame: bool,
    ) -> Option<(i64, i64, i64, i64)> {
        let nominal_count = if drop_frame {
            let drop = Self::drop_per_minute(nominal)?;
            // Elapsed frames per 10-minute block and per 24 h day.
            let per_10_nominal = 600 * nominal;
            let per_10_elapsed = per_10_nominal - 9 * drop;
            if per_10_elapsed <= 0 {
                return None;
            }
            let e = elapsed.rem_euclid(144 * per_10_elapsed);
            let base = e / per_10_elapsed * per_10_nominal;
            let r = e % per_10_elapsed;
            let first_minute = 60 * nominal;
            if r < first_minute {
                // 10th minute carries no drops.
                base + r
            } else {
                // Minute (idx+1) of the block: nominal restarts past its own
                // skipped labels, so only this minute's `drop` is added —
                // earlier minutes are already placed by `idx`.
                let stride = first_minute - drop;
                let idx = (r - first_minute) / stride;
                let rest = (r - first_minute) % stride;
                base + (idx + 1) * first_minute + rest + drop
            }
        } else {
            elapsed
        };
        // Display wraps at 24 h (FFmpeg `tc24hmax` behaviour); elapsed above does not.
        let n = nominal_count.rem_euclid(24 * 3600 * nominal);
        let f = n % nominal;
        let total_seconds = n / nominal;
        Some((
            (total_seconds / 3600) % 24,
            (total_seconds / 60) % 60,
            total_seconds % 60,
            f,
        ))
    }

    /// BWF/iXML sample count → sample-accurate timecode. Elapsed is the raw
    /// `(samples, sample_rate)` pair (EBU Tech 3285: TimeReference is
    /// samples since midnight — no fps involved); the display label is
    /// derived at `display_duration` with exact integer math, so frame
    /// boundaries never wobble on float error.
    pub fn from_samples(
        samples: u64,
        sample_rate: f64,
        display_duration: MediaTime,
        drop_frame: bool,
    ) -> Option<Self> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return None;
        }
        let rate = sample_rate.round() as i128;
        if rate <= 0 || rate > i128::from(i32::MAX) {
            return None;
        }
        let value = i128::from(display_duration.value);
        let timescale = i128::from(display_duration.timescale);
        if value <= 0 || timescale <= 0 {
            return None;
        }
        let nominal = Self::nominal_fps(display_duration)?;
        let drop_frame = drop_frame && Self::drop_per_minute(nominal).is_some();
        // Exact floored elapsed display-frames: samples × video-rate ÷ audio-rate.
        let elapsed_frames =
            (i128::from(samples) * timescale / (rate * value)).min(i128::from(i64::MAX)) as i64;
        let (h, m, s, f) = Self::label_from_elapsed(elapsed_frames, nominal, drop_frame)?;
        let sep = if drop_frame { ";" } else { ":" };
        Some(Self {
            text: format!("{h:02}:{m:02}:{s:02}{sep}{f:02}"),
            frame_number: samples.try_into().ok()?,
            frame_duration: MediaTime::new(1, rate as i32),
            drop_frame,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: ClipId,
    /// Filesystem path serialized as a JSON string.
    pub url: PathBuf,
    pub kind: MediaKind,
    pub duration: MediaTime,
    pub audio: Vec<AudioSummary>,
    pub video: Option<VideoSummary>,
    /// Unix seconds; `None` means unknown.
    pub recorded_at: Option<i64>,
    pub recorded_at_source: Option<RecordingTimestampSource>,
    pub source_identifier: Option<String>,
    pub media_span: Option<MediaSpan>,
}

impl Clip {
    pub fn source_timecode(&self) -> Option<&SourceTimecode> {
        self.video
            .as_ref()
            .and_then(|v| v.source_timecode.as_ref())
            .or_else(|| self.audio.iter().find_map(|a| a.source_timecode.as_ref()))
    }
}

// ------------------------------------------------------- Timeline / results

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProject {
    pub clips: Vec<Clip>,
    pub warnings: Vec<SyncWarning>,
    pub imported_timeline: Option<ImportedTimeline>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedTimeline {
    pub name: String,
    pub frame_duration: MediaTime,
    pub edits: Vec<TimelineEdit>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineTransition {
    pub kind: TimelineTransitionKind,
    pub right_edit_id: String,
    pub start: MediaTime,
    pub end: MediaTime,
    pub alignment: String,
    /// Original FCP7 payloads round-tripped verbatim for Premiere.
    pub fcp7_effect_xml: String,
    pub fcp7_transition_xml: Option<String>,
    pub is_otio_portable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimelineTransitionKind {
    CrossDissolve,
    AudioTransition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEdit {
    pub id: String,
    pub name: Option<String>,
    /// Serialized as `clipID` for saved-result compatibility.
    #[serde(rename = "clipID")]
    pub clip_id: ClipId,
    pub media_type: MediaKind,
    pub source_in: MediaTime,
    pub source_out: MediaTime,
    pub timeline_start: MediaTime,
    pub timeline_end: MediaTime,
    #[serde(default = "default_rate")]
    pub playback_rate: f64,
    #[serde(default)]
    pub plays_backward: bool,
    pub fcp7_time_remap_xml: Option<String>,
    #[serde(default)]
    pub fcp7_filter_xmls: Vec<String>,
    pub fcp7_retime_in: Option<i64>,
    pub fcp7_retime_out: Option<i64>,
    pub fcp7_retime_duration: Option<i64>,
    #[serde(default)]
    pub fcp7_labels_xml: Option<String>,
    /// Zero-based physical source channel; absent means all channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source_channel: Option<usize>,
    #[serde(default)]
    pub fcpxml_audio_role: Option<String>,
    pub track_index: usize,
    pub audio_track_index: Option<usize>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub track_enabled: bool,
    #[serde(default)]
    pub track_locked: bool,
    pub audio_enabled: Option<bool>,
    pub audio_track_enabled: Option<bool>,
    pub audio_track_locked: Option<bool>,
    pub transition_after: Option<TimelineTransition>,
    pub audio_transition_after: Option<TimelineTransition>,
    pub linked_audio_edit: Option<TimelineLinkedAudioEdit>,
}

fn default_rate() -> f64 {
    1.0
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineLinkedAudioEdit {
    pub id: String,
    pub source_in: MediaTime,
    pub source_out: MediaTime,
    pub timeline_start: MediaTime,
    pub timeline_end: MediaTime,
    #[serde(default = "default_rate")]
    pub playback_rate: f64,
    #[serde(default)]
    pub plays_backward: bool,
    pub fcp7_time_remap_xml: Option<String>,
    #[serde(default)]
    pub fcp7_filter_xmls: Vec<String>,
    pub fcp7_retime_in: Option<i64>,
    pub fcp7_retime_out: Option<i64>,
    pub fcp7_retime_duration: Option<i64>,
    #[serde(default)]
    pub fcp7_labels_xml: Option<String>,
    /// Zero-based physical source channel; absent means all channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source_channel: Option<usize>,
    #[serde(default)]
    pub fcpxml_audio_role: Option<String>,
    pub track_index: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub track_enabled: bool,
    #[serde(default)]
    pub track_locked: bool,
    pub transition_after: Option<TimelineTransition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelinePreviewItem {
    pub id: String,
    pub clip_id: ClipId,
    pub url: PathBuf,
    pub name: String,
    pub kind: MediaKind,
    pub source_key: String,
    pub start: f64,
    pub duration: f64,
    pub confidence: f64,
    pub matched: bool,
}

/// Live match preview for progress callbacks.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchPreview {
    pub left: ClipId,
    pub right: ClipId,
    pub rate: f64,
    pub offset: f64,
    pub confidence: f64,
    pub stage: MatchPreviewStage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchPreviewStage {
    Candidate,
    Refined,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub search_overrides: std::collections::HashMap<ClipId, crate::SearchAccuracy>,
    /// Stopped after a fully solved stage; no in-flight matches are included.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// Completed graph variants, sharing this result's media/project context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<SyncStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_stage: Option<usize>,
    /// Search budget that produced this result. Legacy results used Balanced.
    #[serde(default, skip_serializing_if = "crate::SearchAccuracy::is_balanced")]
    pub search_accuracy: crate::SearchAccuracy,
    /// Imported source tracks whose edits remain fixed while other tracks
    /// are placed relative to their synchronized media clock. Keys use the
    /// writer-neutral `imported-video-000001` / `imported-audio-000001` form.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub preserve_editing_tracks: std::collections::BTreeSet<String>,
    pub project: SyncProject,
    pub islands: Vec<SyncIsland>,
    pub unmatched: Vec<ClipId>,
    pub matches: Vec<MatchSummary>,
    /// Temporal evidence policy used for this solve. Old serializations
    /// without the field load with the current Automatic policy.
    #[serde(default)]
    pub temporal_policy: TemporalPolicy,
}

/// A fully solved, exportable intermediate result. Coarse candidates are
/// never stored here: waveform edges have passed ordinary fine validation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncStage {
    pub kind: SyncStageKind,
    pub islands: Vec<SyncIsland>,
    pub unmatched: Vec<ClipId>,
    pub matches: Vec<MatchSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncStageKind {
    Waveform,
    FileSpans,
    Timecode,
}

impl SyncStageKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waveform => "Waveform",
            Self::FileSpans => "Waveform + file spans",
            Self::Timecode => "Waveform + metadata",
        }
    }
}

impl SyncStage {
    pub fn synchronized_count(&self) -> usize {
        self.islands
            .iter()
            .map(|island| island.placements.len())
            .sum()
    }
}

impl SyncResult {
    /// Changes both the visible graph and the graph consumed by every
    /// exporter. Invalid indices leave the result untouched.
    pub fn select_stage(&mut self, index: usize) -> bool {
        if self.stages.is_empty() {
            return index == 0;
        }
        let Some(stage) = self.stages.get(index) else {
            return false;
        };
        self.islands = stage.islands.clone();
        self.unmatched = stage.unmatched.clone();
        self.matches = stage.matches.clone();
        self.selected_stage = Some(index);
        true
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncIsland {
    pub id: usize,
    pub placements: Vec<ClipPlacement>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipPlacement {
    #[serde(rename = "clipID")]
    pub clip_id: ClipId,
    pub mapping: TimeMap,
    pub confidence: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimeMap {
    pub points: Vec<MappingPoint>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MappingPoint {
    pub source: MediaTime,
    pub island: MediaTime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchSummary {
    pub left: ClipId,
    pub right: ClipId,
    #[serde(rename = "driftPPM")]
    pub drift_ppm: f64,
    pub offset: MediaTime,
    pub confidence: f64,
    pub anchors: usize,
    pub covered: MediaTime,
    pub residual: MediaTime,
    pub evidence: Option<MatchEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MatchEvidence {
    Waveform,
    SpannedMetadata,
    Timecode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncWarning {
    pub url: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineSequenceSummary {
    pub index: usize,
    pub name: String,
    pub clip_count: usize,
}

// ------------------------------------------------------------- Sync control

/// Which audio to analyse. Automatic and discrete-channel modes mirror the
/// original engine; the mixed modes add the explicit all-channel and
/// all-stream rescues used by modern synchronization tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioAnalysisSource {
    #[default]
    Automatic,
    AllMixed,
    Channel(usize),
    MixedStream(usize),
    Stream {
        index: usize,
        channel: Option<usize>,
    },
}

impl AudioAnalysisSource {
    pub fn cache_key(&self) -> String {
        match *self {
            AudioAnalysisSource::Automatic => "automatic".to_string(),
            AudioAnalysisSource::AllMixed => "all-streams-mixed".to_string(),
            AudioAnalysisSource::Channel(i) => format!("channel-{i}"),
            AudioAnalysisSource::MixedStream(index) => format!("stream-{index}-mixed"),
            AudioAnalysisSource::Stream { index, channel } => match channel {
                Some(c) => format!("stream-{index}-channel-{c}"),
                None => format!("stream-{index}-automatic"),
            },
        }
    }

    /// Explicit discrete channel choice. `None` means either adaptive loudest
    /// channel or explicit mixing; [`Self::mixes_channels`] distinguishes them.
    pub fn selected_channel(&self) -> Option<usize> {
        match *self {
            AudioAnalysisSource::Automatic | AudioAnalysisSource::AllMixed => None,
            AudioAnalysisSource::Channel(i) => Some(i),
            AudioAnalysisSource::MixedStream(_) => None,
            AudioAnalysisSource::Stream { channel, .. } => channel,
        }
    }

    pub fn mixes_channels(&self) -> bool {
        matches!(
            self,
            AudioAnalysisSource::AllMixed | AudioAnalysisSource::MixedStream(_)
        )
    }

    pub fn stream_index(&self) -> usize {
        match *self {
            AudioAnalysisSource::Automatic
            | AudioAnalysisSource::AllMixed
            | AudioAnalysisSource::Channel(_) => 0,
            AudioAnalysisSource::MixedStream(index) => index,
            AudioAnalysisSource::Stream { index, .. } => index,
        }
    }
}

/// Timestamp evidence permitted for clips from a source.
/// Orthogonal to [`AudioAnalysisSource`] (wave source): this never selects
/// audio, only which instants may hint competing waveform peaks and order
/// islands/unmatched clips. Valid timecode can also support temporal
/// matches; it does not override a confident waveform edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TemporalMode {
    /// Automatically use the available file timestamp as recording start
    /// and timecode.
    #[default]
    Auto,
    /// The clip's timestamp (metadata or file date) is the
    /// recording start.
    RecStart,
    /// The clip's timestamp is the recording end: start = stamp − duration.
    RecStop,
    /// Only a valid [`SourceTimecode`]; anything else is missing evidence.
    Timecode,
}

impl TemporalMode {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::RecStart => "REC START",
            Self::RecStop => "REC STOP",
            Self::Timecode => "Timecode",
        }
    }

    /// (recorded-start anchor, timecode anchor) of one clip under this
    /// mode. `None` = missing selected metadata: callers fall back to
    /// stable order, never guess. Auto resolves exactly like the legacy
    /// hint path (embedded timestamp, else timecode-agnostic pairing).
    pub fn anchors(&self, clip: &Clip) -> (Option<f64>, Option<f64>) {
        let timecode = clip.source_timecode().map(SourceTimecode::as_seconds);
        match self {
            Self::Auto => (clip.recorded_at.map(|t| t as f64), timecode),
            Self::RecStart => (clip.recorded_at.map(|t| t as f64), None),
            Self::RecStop => (
                clip.recorded_at
                    .map(|t| t as f64 - clip.duration.as_seconds()),
                None,
            ),
            Self::Timecode => (None, timecode),
        }
    }

    /// Whether this clip offers the selected evidence (for explicit
    /// UI/status fallback; `false` never blocks sync, it only documents
    /// that the mode is inactive for this clip).
    pub fn has_evidence(&self, clip: &Clip) -> bool {
        let (recorded, timecode) = self.anchors(clip);
        recorded.is_some() || timecode.is_some()
    }
}

/// Per-source temporal overrides with a session default.
/// Missing overrides use the default. Saved in [`SyncResult`] so export
/// and lane layout use the same timestamp policy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemporalPolicy {
    #[serde(default)]
    pub default: TemporalMode,
    #[serde(default)]
    pub modes: HashMap<ClipId, TemporalMode>,
}

impl TemporalPolicy {
    pub fn resolve(&self, id: &ClipId) -> TemporalMode {
        self.modes.get(id).copied().unwrap_or(self.default)
    }

    /// Mode-resolved anchors for hint and chronology consumers.
    pub fn anchors(&self, clip: &Clip) -> (Option<f64>, Option<f64>) {
        self.resolve(&clip.id).anchors(clip)
    }
}

/// Adaptive mono selection:
/// picks the loudest channel per block, keeps the previous channel while it
/// stays within 90% of the loudest (hysteresis). This is what keeps
/// opposite-phase stereo from cancelling out.
pub fn select_mono_channel(energies: &[f64], previous: Option<usize>) -> usize {
    assert!(!energies.is_empty());
    let mut loudest = 0;
    for (i, e) in energies.iter().enumerate() {
        if *e > energies[loudest] {
            loudest = i;
        }
    }
    match previous {
        Some(p) if p < energies.len() && energies[p] >= energies[loudest] * 0.9 => p,
        _ => loudest,
    }
}

// ------------------------------------------------------------- constraints

/// Session-only matching constraints from the timeline UI. Not serialized.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConstraintKind {
    RejectedPair,
    RejectedAlignment,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SyncConstraint {
    kind: ConstraintKind,
    left: ClipId,
    right: ClipId,
    offset: Option<MediaTime>,
}

impl SyncConstraint {
    pub fn rejecting_pair(left: ClipId, right: ClipId) -> Self {
        Self {
            kind: ConstraintKind::RejectedPair,
            left,
            right,
            offset: None,
        }
    }

    pub fn rejecting_alignment(left: ClipId, right: ClipId, offset_seconds: f64) -> Self {
        let s = if offset_seconds.is_finite() {
            offset_seconds
        } else {
            0.0
        };
        Self {
            kind: ConstraintKind::RejectedAlignment,
            left,
            right,
            offset: Some(MediaTime::seconds(s)),
        }
    }

    /// Mirrors `[SyncConstraint].rejectPair`: symmetric in left/right.
    pub fn rejects_pair(constraints: &[Self], left: &ClipId, right: &ClipId) -> bool {
        constraints.iter().any(|c| {
            c.kind == ConstraintKind::RejectedPair
                && ((&c.left == left && &c.right == right)
                    || (&c.left == right && &c.right == left))
        })
    }

    /// Mirrors `[SyncConstraint].rejectAlignment`: a bucket is rejected when
    /// within 1.5 s of the rejected offset (sign flips when pair is swapped).
    pub fn rejects_alignment(
        constraints: &[Self],
        left: &ClipId,
        right: &ClipId,
        offset: f64,
    ) -> bool {
        constraints.iter().any(|c| {
            if c.kind != ConstraintKind::RejectedAlignment {
                return false;
            }
            let Some(rejected) = c.offset.as_ref().map(MediaTime::as_seconds) else {
                return false;
            };
            if &c.left == left && &c.right == right {
                (offset - rejected).abs() <= 1.5
            } else if &c.left == right && &c.right == left {
                (offset + rejected).abs() <= 1.5
            } else {
                false
            }
        })
    }
}

/// Last path component, lossy UTF-8 (shared display/error helper).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_timecode_constructors_validate() {
        // Nominal fps snaps through true rates (29.97 → 30).
        assert_eq!(
            SourceTimecode::nominal_fps(MediaTime::new(1001, 30_000)),
            Some(30)
        );
        assert_eq!(SourceTimecode::nominal_fps(MediaTime::new(1, 25)), Some(25));
        assert_eq!(SourceTimecode::nominal_fps(MediaTime::new(0, 25)), None);
        // NDF round-trip at 25 fps.
        let tc = SourceTimecode::from_label("12:34:56:07", MediaTime::new(1, 25)).expect("tc");
        assert_eq!(tc.frame_number, ((12 * 60 + 34) * 60 + 56) * 25 + 7);
        assert!((tc.as_seconds() - 45_296.28).abs() < 1e-9);
        let back = SourceTimecode::from_frame_number(tc.frame_number, MediaTime::new(1, 25), false)
            .expect("back");
        assert_eq!(back.text, "12:34:56:07");
        // Strict grammar: no empty/extra components, mixed separators,
        // prefixes, suffixes, or non-2-digit fields.
        for bad in [
            ":01:02:03:04",
            "01::02:03:04",
            "x:01:02:03:04",
            "01;02:03:04",
            "01:02:03.04",
            "01:02:03:04 ",
            " 01:02:03:04",
            "1:02:03:04",
            "01:02:03:4",
            "01:02:03:044",
            "01:02-03:04",
            "01:02:03:04:05",
            "",
        ] {
            assert!(
                SourceTimecode::from_label(bad, MediaTime::new(1, 25)).is_none(),
                "{bad:?}"
            );
        }
        // Ranges still enforced after the grammar.
        assert!(SourceTimecode::from_label("24:00:00:00", MediaTime::new(1, 25)).is_none());
        assert!(SourceTimecode::from_label("00:00:00:25", MediaTime::new(1, 25)).is_none());
        assert!(SourceTimecode::from_frame_number(-1, MediaTime::new(1, 25), false).is_none());
        assert!(SourceTimecode::from_samples(100, 0.0, MediaTime::new(1, 25), false).is_none());
        assert!(
            SourceTimecode::from_samples(100, 48_000.0, MediaTime::new(0, 25), false).is_none()
        );
    }

    #[test]
    fn temporal_mode_anchors_and_round_trip() {
        use std::path::PathBuf;
        let clip = |recorded: Option<i64>, fs: bool, tc: bool| Clip {
            id: ClipId::new("c"),
            url: PathBuf::from("/v/c.wav"),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(60.0),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: None,
                is_float: None,
                source_timecode: tc.then(|| SourceTimecode {
                    text: "00:01:00:00".into(),
                    frame_number: 1500,
                    frame_duration: MediaTime::new(1, 25),
                    drop_frame: false,
                }),
            }],
            video: None,
            recorded_at: recorded,
            recorded_at_source: recorded.map(|_| {
                if fs {
                    RecordingTimestampSource::FileSystem
                } else {
                    RecordingTimestampSource::EmbeddedMetadata
                }
            }),
            source_identifier: None,
            media_span: None,
        };
        let full = clip(Some(1000), false, true);
        // Auto: file timestamp + timecode, regardless of whether the
        // timestamp came from embedded metadata or the filesystem.
        assert_eq!(
            TemporalMode::Auto.anchors(&full),
            (Some(1000.0), Some(60.0))
        );
        assert!(TemporalMode::Auto.has_evidence(&full));
        let fs_only = clip(Some(1000), true, false);
        assert_eq!(TemporalMode::Auto.anchors(&fs_only), (Some(1_000.0), None));
        assert!(TemporalMode::Auto.has_evidence(&fs_only));
        // REC START takes any stamp as start; REC STOP subtracts duration.
        assert_eq!(
            TemporalMode::RecStart.anchors(&fs_only),
            (Some(1000.0), None)
        );
        assert_eq!(TemporalMode::RecStop.anchors(&fs_only), (Some(940.0), None));
        assert_eq!(TemporalMode::RecStop.anchors(&full), (Some(940.0), None));
        // Timecode ignores stamps entirely; missing timecode = no evidence.
        assert_eq!(TemporalMode::Timecode.anchors(&full), (None, Some(60.0)));
        assert_eq!(TemporalMode::Timecode.anchors(&fs_only), (None, None));
        assert!(!TemporalMode::Timecode.has_evidence(&fs_only));
        // Serde round-trip incl. policy defaults.
        for mode in [
            TemporalMode::Auto,
            TemporalMode::RecStart,
            TemporalMode::RecStop,
            TemporalMode::Timecode,
        ] {
            let json = serde_json::to_string(&mode).expect("json");
            assert_eq!(
                serde_json::from_str::<TemporalMode>(&json).expect("parse"),
                mode
            );
        }
        assert_eq!(TemporalMode::default(), TemporalMode::Auto);
        let policy = TemporalPolicy {
            default: TemporalMode::RecStop,
            modes: [(ClipId::new("c"), TemporalMode::Timecode)]
                .into_iter()
                .collect(),
        };
        assert_eq!(policy.resolve(&ClipId::new("c")), TemporalMode::Timecode);
        assert_eq!(policy.resolve(&ClipId::new("other")), TemporalMode::RecStop);
        let json = serde_json::to_string(&policy).expect("json");
        assert_eq!(
            serde_json::from_str::<TemporalPolicy>(&json).expect("parse"),
            policy
        );
        // Old payloads without the policy parse as all-Auto.
        assert_eq!(
            serde_json::from_str::<TemporalPolicy>("{}").expect("default"),
            TemporalPolicy::default()
        );
    }

    #[test]
    fn mono_hysteresis_keeps_previous_within_90_percent() {
        assert_eq!(select_mono_channel(&[100.0, 95.0], Some(1)), 1);
        assert_eq!(select_mono_channel(&[100.0, 80.0], Some(1)), 0);
        assert_eq!(select_mono_channel(&[10.0, 50.0], None), 1);
    }

    #[test]
    fn audio_source_cache_keys_are_stable() {
        assert_eq!(AudioAnalysisSource::Automatic.cache_key(), "automatic");
        assert_eq!(
            AudioAnalysisSource::AllMixed.cache_key(),
            "all-streams-mixed"
        );
        assert!(AudioAnalysisSource::AllMixed.mixes_channels());
        assert_eq!(AudioAnalysisSource::Channel(2).cache_key(), "channel-2");
        assert_eq!(
            AudioAnalysisSource::MixedStream(1).cache_key(),
            "stream-1-mixed"
        );
        assert!(AudioAnalysisSource::MixedStream(1).mixes_channels());
        assert_eq!(AudioAnalysisSource::MixedStream(1).stream_index(), 1);
        assert_eq!(
            AudioAnalysisSource::Stream {
                index: 1,
                channel: None
            }
            .cache_key(),
            "stream-1-automatic"
        );
        assert_eq!(
            AudioAnalysisSource::Stream {
                index: 1,
                channel: Some(0)
            }
            .cache_key(),
            "stream-1-channel-0"
        );
    }

    #[test]
    fn constraints_apply_to_unordered_pairs() {
        let a = ClipId::new("a");
        let b = ClipId::new("b");
        let c = ClipId::new("c");
        let pair = vec![SyncConstraint::rejecting_pair(a.clone(), b.clone())];
        assert!(SyncConstraint::rejects_pair(&pair, &a, &b));
        // Symmetric.
        assert!(SyncConstraint::rejects_pair(&pair, &b, &a));
        assert!(!SyncConstraint::rejects_pair(&pair, &a, &c));

        let align = vec![SyncConstraint::rejecting_alignment(
            a.clone(),
            b.clone(),
            6.0,
        )];
        assert!(SyncConstraint::rejects_alignment(&align, &a, &b, 6.0));
        assert!(SyncConstraint::rejects_alignment(&align, &a, &b, 7.4));
        assert!(!SyncConstraint::rejects_alignment(&align, &a, &b, 7.6));
        // Swapped pair flips the sign.
        assert!(SyncConstraint::rejects_alignment(&align, &b, &a, -6.0));
        assert!(!SyncConstraint::rejects_alignment(&align, &b, &a, 6.0));
        // Non-finite offset degrades to 0 rather than poisoning matching.
        let nan = vec![SyncConstraint::rejecting_alignment(
            a.clone(),
            b.clone(),
            f64::NAN,
        )];
        assert!(SyncConstraint::rejects_alignment(&nan, &a, &b, 0.0));
    }
}
