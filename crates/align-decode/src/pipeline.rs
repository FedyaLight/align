//! Media import, analysis, refinement, and synchronization orchestration.
//!
//! Stages: expand inputs → inspect (backend, warnings
//! for unreadable files) → fingerprints (backend decode + content-addressed
//! cache + automatic multi-stream selection) → coarse match → fine refine
//! (via [`BackendWindowProvider`]) → graph solve → [`align_core::SyncResult`].
//!
//! Important implementation choices:
//! - fingerprinting and refinement use four bounded workers with indexed
//!   deterministic results;
//! - ClipIDs hash `path\0duration-micros`, using the same units across backends;
//! - spanned-file, BWF/iXML/container/LTC timecode, FCP 7 XML and FCPXML
//!   evidence join the same waveform-first graph without overriding a
//!   confident audio match.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use align_core::{
    AudioAnalysisSource, AudioSummary, Clip, ClipId, ClipPlacement, ClipTimingHints,
    FingerprintCache, FingerprintExtractor, MappingPoint, MatchSummary, MediaKind, MediaTime,
    RecordingTimestampSource, SyncConstraint, SyncIsland, SyncProject, SyncWarning, TimeMap,
    match_fingerprints, refine_forest, solve_graph,
};
use sha2::{Digest, Sha256};

use crate::backend::{BackendKind, MediaBackend, default_backend};
use crate::provider::BackendWindowProvider;

// ------------------------------------------------------------ types

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Inspect,
    Fingerprint,
    Match,
    Refine,
    Solve,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PipelineProgress {
    pub completed_stage: Option<align_core::SyncStageKind>,
    pub phase: Phase,
    pub completed: usize,
    pub total: usize,
    pub current: Option<PathBuf>,
    /// Newly inspected clip (inspect phase, mirrors `discovered`).
    pub discovered: Option<Clip>,
    /// Match lifecycle event (match/refine/solve phases).
    pub preview: Option<align_core::MatchPreview>,
    /// Session-only timeline waveform derived from the fingerprints already
    /// in memory. It is never written as a separate preview cache.
    pub waveform: Option<(ClipId, Vec<f32>)>,
}

impl PipelineProgress {
    fn basic(phase: Phase, completed: usize, total: usize, current: Option<PathBuf>) -> Self {
        Self {
            completed_stage: None,
            phase,
            completed,
            total,
            current,
            discovered: None,
            preview: None,
            waveform: None,
        }
    }
}

const WAVEFORM_PREVIEW_BINS: usize = 512;

struct WaveformPreview {
    energy: Vec<f64>,
    counts: Vec<u64>,
    sample: u64,
    total_samples: f64,
}

impl WaveformPreview {
    fn new(duration_seconds: f64) -> Self {
        Self {
            energy: vec![0.0; WAVEFORM_PREVIEW_BINS],
            counts: vec![0; WAVEFORM_PREVIEW_BINS],
            sample: 0,
            total_samples: (duration_seconds.max(0.0)
                * align_core::fingerprint::SAMPLE_RATE as f64)
                .max(1.0),
        }
    }

    fn consume(&mut self, samples: &[f32]) {
        for value in samples {
            let index = ((self.sample as f64 / self.total_samples) * WAVEFORM_PREVIEW_BINS as f64)
                .floor()
                .clamp(0.0, (WAVEFORM_PREVIEW_BINS - 1) as f64) as usize;
            self.energy[index] += f64::from(*value) * f64::from(*value);
            self.counts[index] += 1;
            self.sample += 1;
        }
    }

    fn finish(self) -> Vec<f32> {
        let rms: Vec<f32> = self
            .energy
            .into_iter()
            .zip(self.counts)
            .map(|(energy, count)| {
                if count == 0 {
                    0.0
                } else {
                    (energy / count as f64).sqrt() as f32
                }
            })
            .collect();
        let peak = rms.iter().copied().fold(0.0_f32, f32::max);
        if peak == 0.0 {
            return rms;
        }
        rms.into_iter().map(|value| (value / peak).sqrt()).collect()
    }
}

struct FingerprintRunOptions<'a> {
    accuracy: align_core::SearchAccuracy,
    overrides: &'a HashMap<ClipId, align_core::SearchAccuracy>,
    generate_waveform_previews: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineOptions {
    /// Emit and cache real amplitude envelopes for the interactive timeline.
    pub generate_waveform_previews: bool,
    pub search_accuracy: align_core::SearchAccuracy,
    pub search_overrides: HashMap<ClipId, align_core::SearchAccuracy>,
    pub source_search_overrides: HashMap<String, align_core::SearchAccuracy>,
    /// Default wave source before per-track/per-clip overrides.
    pub audio_source: AudioAnalysisSource,
    pub audio_sources: HashMap<ClipId, AudioAnalysisSource>,
    pub match_policy: align_core::MatchPolicy,
    pub clip_order: align_core::ClipOrderPolicy,
    pub track_content: align_core::TrackContentPolicy,
    /// Imported tracks selected for exact basic-edit preservation.
    pub preserve_editing_tracks: HashSet<String>,
    /// Temporal evidence overrides, round-tripped
    /// into the result for timeline assembly and export.
    pub temporal: align_core::TemporalPolicy,
    /// Saved + CLI path redirections for missing-media relink.
    pub redirects: Vec<align_core::redirect::PathRedirection>,
    /// Exact manual `filename = path` relink picks (one-shot).
    pub manual_relinks: Vec<(String, PathBuf)>,
    /// Timeline-referenced extensions skipped silently (Omit extensions).
    pub omit_extensions: Vec<String>,
    pub prefer_proxies: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PipelineInput {
    Media(PathBuf),
    Timeline(PathBuf),
    TimelineSequence(PathBuf, usize),
}

#[derive(Debug)]
pub enum PipelineError {
    NoMedia,
    Inaccessible(PathBuf),
    MultipleTimelines,
    Timeline(String),
    Cancelled,
    Decode(crate::DecodeError),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMedia => write!(f, "No readable audio or video media was found."),
            Self::Inaccessible(p) => write!(f, "Cannot access {}.", p.display()),
            Self::MultipleTimelines => write!(f, "Add one timeline at a time."),
            Self::Timeline(msg) => write!(f, "Timeline import: {msg}."),
            Self::Cancelled => write!(f, "Cancelled."),
            Self::Decode(e) => write!(f, "Decode error: {e}."),
        }
    }
}

impl From<crate::timeline::TimelineError> for PipelineError {
    fn from(error: crate::timeline::TimelineError) -> Self {
        match error {
            crate::timeline::TimelineError::Aaf(crate::aaf::AafError::Cancelled) => Self::Cancelled,
            crate::timeline::TimelineError::Cancelled => Self::Cancelled,
            error => Self::Timeline(error.to_string()),
        }
    }
}

fn cancelled(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.load(std::sync::atomic::Ordering::Relaxed)
}

impl std::error::Error for PipelineError {}

pub struct Pipeline {
    backend: Box<dyn MediaBackend>,
    cache: FingerprintCache,
}

impl Pipeline {
    pub fn new(kind: BackendKind) -> Self {
        Self::new_in(kind, None)
    }

    /// `cache_dir`: isolated fingerprint cache (tests pass a temp dir for
    /// hermetic runs; production uses the OS cache directory).
    pub fn new_in(kind: BackendKind, cache_dir: Option<PathBuf>) -> Self {
        let backend = crate::backend::create(kind);
        let cache = FingerprintCache::with_backend(cache_dir, backend.kind().cache_tag());
        Self { backend, cache }
    }

    pub fn default_backend() -> Self {
        let backend = default_backend();
        let cache = FingerprintCache::with_backend(None, backend.kind().cache_tag());
        if let Some(days) = align_core::CacheSettings::load().retention_days {
            cache.prune_older_than(std::time::Duration::from_secs(
                days.saturating_mul(24 * 60 * 60),
            ));
        }
        Self { backend, cache }
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    pub fn backend(&self) -> &dyn MediaBackend {
        &*self.backend
    }

    /// Apply this run's relink choices to every sequence in the imported
    /// project, then save those locations into a new XML, FCPXML, or AAF.
    pub fn write_fixed_timeline_copy(
        &self,
        inputs: &[PipelineInput],
        options: &PipelineOptions,
        destination: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<usize, PipelineError> {
        let expanded = expand(inputs)?;
        let timeline = expanded
            .timeline
            .ok_or_else(|| PipelineError::Timeline("No imported project was provided".into()))?;
        let summaries = crate::timeline::sequences(&timeline.path, cancel)?;
        let mut replacements = Vec::new();
        for summary in summaries {
            if cancelled(cancel) {
                return Err(PipelineError::Cancelled);
            }
            let draft = crate::timeline::read_with_proxies(
                &timeline.path,
                Some(summary.index),
                cancel,
                options.prefer_proxies,
            )?;
            let (_, paths) = draft.relinking_missing_media_with_replacements(
                &expanded.media,
                &options.redirects,
                &options.manual_relinks,
            );
            replacements.extend(paths);
        }
        replacements.sort();
        replacements.dedup();
        crate::timeline::write_relinked_copy(&timeline.path, destination, &replacements, cancel)?;
        Ok(replacements.len())
    }

    /// Inspect-only open: expand, inspect,
    /// spanned warnings, timeline relink/resolve — no matching.
    pub fn open(&self, inputs: &[PipelineInput]) -> Result<SyncProject, PipelineError> {
        self.open_with(inputs, &PipelineOptions::default())
    }

    /// [`Self::open`] with relink/omit overrides (saved redirections,
    /// manual picks, omitted timeline extensions).
    pub fn open_with(
        &self,
        inputs: &[PipelineInput],
        options: &PipelineOptions,
    ) -> Result<SyncProject, PipelineError> {
        let expanded = expand(inputs)?;
        let draft = expanded
            .timeline
            .map(|t| {
                crate::timeline::read_with_proxies(
                    &t.path,
                    t.sequence,
                    &std::sync::atomic::AtomicBool::new(false),
                    options.prefer_proxies,
                )
                .map(|d| {
                    d.relinking_missing_media(
                        &expanded.media,
                        &options.redirects,
                        &options.manual_relinks,
                    )
                })
                .map_err(PipelineError::from)
            })
            .transpose()?;
        let mut urls = expanded.media;
        if let Some(d) = &draft {
            for url in d.media_urls() {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
            urls.sort();
        }
        let mut clips = Vec::new();
        let mut warnings = Vec::new();
        for url in &urls {
            match self.inspect_one(url) {
                Ok(clip) => clips.push(clip),
                Err(e) => warnings.push(SyncWarning {
                    url: url.clone(),
                    message: e.to_string(),
                }),
            }
        }
        if clips.is_empty() {
            return Err(PipelineError::NoMedia);
        }
        let spanned = align_core::analyze_spans(&clips);
        warnings.extend(spanned.warnings);
        if let Some(d) = &draft {
            // Import and relink findings belong to the session, not just
            // the draft: surface them next to decode warnings.
            d.validate_source_channels(&clips)
                .map_err(PipelineError::Timeline)?;
            warnings.extend(d.warnings.clone());
            warnings.extend(d.unresolved_warnings(&clips, &options.omit_extensions));
        }
        Ok(SyncProject {
            imported_timeline: draft.as_ref().map(|d| d.resolve(&clips)),
            clips,
            warnings,
        })
    }

    /// Full pipeline: media files/dirs (+ optional one timeline) →
    /// [`align_core::SyncResult`].
    ///
    /// Fingerprint decode and fine refine run on up to 4 workers with
    /// deterministic indexed results; `progress`
    /// is shared across those threads, `cancel` is polled per clip,
    /// candidate and job.
    pub fn synchronize(
        &self,
        inputs: &[PipelineInput],
        constraints: &[SyncConstraint],
        options: &PipelineOptions,
        progress: Option<&(dyn Fn(PipelineProgress) + Send + Sync)>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<align_core::SyncResult, PipelineError> {
        let expanded = expand(inputs)?;
        // Timeline relink runs BEFORE inspection so relinked files are
        // inspected like directly added media.
        let draft = expanded
            .timeline
            .map(|t| {
                crate::timeline::read_with_proxies(
                    &t.path,
                    t.sequence,
                    cancel,
                    options.prefer_proxies,
                )
                .map(|d| {
                    d.relinking_missing_media(
                        &expanded.media,
                        &options.redirects,
                        &options.manual_relinks,
                    )
                })
                .map_err(PipelineError::from)
            })
            .transpose()?;
        let mut urls = expanded.media;
        if let Some(d) = &draft {
            for url in d.media_urls() {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
            urls.sort();
        }
        if urls.is_empty() {
            return Err(PipelineError::NoMedia);
        }

        // ---- inspect
        let mut clips = Vec::new();
        let mut warnings = Vec::new();
        for (i, url) in urls.iter().enumerate() {
            if cancelled(cancel) {
                return Err(PipelineError::Cancelled);
            }
            if let Some(p) = &progress {
                p(PipelineProgress::basic(
                    Phase::Inspect,
                    i,
                    urls.len(),
                    Some(url.clone()),
                ));
            }
            match self.inspect_one(url) {
                Ok(clip) => {
                    if let Some(p) = &progress {
                        let mut event = PipelineProgress::basic(
                            Phase::Inspect,
                            clips.len(),
                            urls.len(),
                            Some(url.clone()),
                        );
                        event.discovered = Some(clip.clone());
                        p(event);
                    }
                    // Variable video timing does not affect audio sync,
                    // but constant-rate interchange needs a CFR transcode.
                    if let Some(video) = &clip.video {
                        use align_core::VideoFrameRateMode;
                        match video.frame_rate_mode {
                            Some(VideoFrameRateMode::Variable) => warnings.push(SyncWarning {
                                url: url.clone(),
                                message: "Variable frame rate detected. Audio synchronization remains sample-accurate, but FCP 7 XML can describe only a constant edit rate; transcode this source to CFR before frame-exact interchange.".into(),
                            }),
                            Some(VideoFrameRateMode::Unknown) => warnings.push(SyncWarning {
                                url: url.clone(),
                                message: "This media container does not expose frame sample timing, so constant versus variable frame rate could not be verified. Audio synchronization is unaffected; transcode to CFR before frame-exact interchange.".into(),
                            }),
                            _ => {}
                        }
                    }
                    clips.push(clip);
                }
                Err(e) => warnings.push(SyncWarning {
                    url: url.clone(),
                    message: e.to_string(),
                }),
            }
        }
        if let Some(p) = &progress {
            p(PipelineProgress::basic(
                Phase::Inspect,
                urls.len(),
                urls.len(),
                None,
            ));
        }
        if clips.is_empty() {
            return Err(PipelineError::NoMedia);
        }
        if let Some(d) = &draft {
            d.validate_source_channels(&clips)
                .map_err(PipelineError::Timeline)?;
            warnings.extend(d.warnings.clone());
            warnings.extend(d.unresolved_warnings(&clips, &options.omit_extensions));
        }
        let imported_timeline = draft.as_ref().map(|d| d.resolve(&clips));
        let order_context =
            align_core::ClipOrderContext::from_clips(&clips, imported_timeline.as_ref());

        let mut search_overrides = options.search_overrides.clone();
        for clip in &clips {
            let kind = match clip.kind {
                MediaKind::Video => "video",
                MediaKind::Audio => "audio",
            };
            let source = align_core::source_key_for_clip(
                &clip.url,
                clip.source_identifier.as_deref(),
                clip.media_span
                    .as_ref()
                    .map(|span| span.identifier.as_str()),
            );
            let mut keys = vec![format!("{kind}:{source}")];
            if let Some(timeline) = &imported_timeline {
                keys = timeline
                    .edits
                    .iter()
                    .filter(|edit| edit.clip_id == clip.id)
                    .map(|edit| {
                        format!(
                            "{kind}:imported-{}-{:06}",
                            edit.media_type.as_str(),
                            edit.track_index
                        )
                    })
                    .collect();
            }
            // A media file used on several tracks is analysed once. Resolve
            // competing track budgets to the deepest requested search.
            let requested = keys
                .iter()
                .filter_map(|key| options.source_search_overrides.get(key))
                .max_by_key(|level| {
                    align_core::SearchAccuracy::ALL
                        .iter()
                        .position(|item| item == *level)
                });
            if let Some(level) = requested {
                search_overrides.entry(clip.id.clone()).or_insert(*level);
            }
        }

        // ---- fingerprints (multi-stream variants + shared-hash selection)
        let usable: Vec<&Clip> = clips.iter().filter(|c| !c.audio.is_empty()).collect();
        let mut audio_sources = options.audio_sources.clone();
        for clip in &clips {
            audio_sources
                .entry(clip.id.clone())
                .or_insert(options.audio_source);
        }
        let (features, selected_sources) = self.fingerprints(
            &usable,
            &audio_sources,
            FingerprintRunOptions {
                accuracy: options.search_accuracy,
                overrides: &search_overrides,
                generate_waveform_previews: options.generate_waveform_previews,
            },
            progress,
            cancel,
        )?;
        let mut source_map = audio_sources;
        source_map.extend(selected_sources);

        // ---- coarse match
        if let Some(p) = &progress {
            p(PipelineProgress::basic(Phase::Match, 0, 1, None));
        }
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
        let hints = ClipTimingHints::from_clips(&clips, &options.temporal);
        let candidates = align_core::enforce_track_content(
            &match_fingerprints(features, Some(&hints), constraints),
            &options.track_content,
            &order_context,
        );
        if let Some(p) = &progress {
            for (i, m) in candidates.iter().enumerate() {
                let mut event =
                    PipelineProgress::basic(Phase::Match, i + 1, candidates.len(), None);
                event.preview = Some(align_core::MatchPreview {
                    left: m.left.clone(),
                    right: m.right.clone(),
                    rate: m.rate,
                    offset: m.offset,
                    confidence: m.confidence,
                    stage: align_core::MatchPreviewStage::Candidate,
                });
                p(event);
            }
            p(PipelineProgress::basic(Phase::Match, 1, 1, None));
        }

        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
        // ---- refine
        let durations: HashMap<ClipId, f64> = clips
            .iter()
            .map(|c| (c.id.clone(), c.duration.as_seconds()))
            .collect();
        let clip_paths = clips
            .iter()
            .map(|c| (c.id.clone(), c.url.clone()))
            .collect();
        let provider = BackendWindowProvider {
            backend: &*self.backend,
            clips: clip_paths,
        };
        if let Some(p) = &progress {
            p(PipelineProgress::basic(
                Phase::Refine,
                0,
                candidates.len(),
                None,
            ));
        }
        // Preview the refined result if accepted, otherwise the original
        // candidate. Boxing keeps the shared reference valid for the call.
        let refine_progress: Option<Box<dyn Fn(align_core::RefineEvent) + Send + Sync>> =
            progress.as_ref().map(|p| {
                let candidates = &candidates;
                Box::new(move |event: align_core::RefineEvent| {
                    let lookup = candidates
                        .iter()
                        .find(|m| m.left == event.left && m.right == event.right);
                    let (rate, offset, confidence) = lookup
                        .map(|m| (m.rate, m.offset, m.confidence))
                        .unwrap_or((1.0, 0.0, 0.0));
                    let mut out =
                        PipelineProgress::basic(Phase::Refine, event.completed, event.total, None);
                    out.preview = Some(align_core::MatchPreview {
                        left: event.left.clone(),
                        right: event.right.clone(),
                        rate,
                        offset,
                        confidence,
                        stage: match event.stage {
                            align_core::RefineStage::Refined => {
                                align_core::MatchPreviewStage::Refined
                            }
                            align_core::RefineStage::Rejected => {
                                align_core::MatchPreviewStage::Rejected
                            }
                        },
                    });
                    p(out);
                }) as Box<dyn Fn(align_core::RefineEvent) + Send + Sync>
            });
        let refined = refine_forest(
            &durations,
            &candidates,
            &options.match_policy,
            &source_map,
            &provider,
            cancel,
            refine_progress.as_deref(),
        );

        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }

        // Metadata edges: seamless file-set parts
        // join without waveform, then timecode overlaps the leftovers.
        // Spanned edges ignore rejected *alignments* (they carry none) but
        // honour rejected pairs.
        let spanned = align_core::analyze_spans(&clips);
        warnings.extend(spanned.warnings);
        let spanned_matches: Vec<_> = spanned
            .matches
            .into_iter()
            .filter(|m| !SyncConstraint::rejects_pair(constraints, &m.left, &m.right))
            .collect();
        let eligible = |matches: &[align_core::PairwiseMatch]| {
            let matches =
                align_core::enforce_track_content(matches, &options.track_content, &order_context);
            align_core::enforce_clip_order(&matches, &options.clip_order, &order_context)
        };
        let waveform_matches = eligible(&refined);
        let mut combined = refined;
        combined.extend(spanned_matches);
        let timecoded =
            align_core::analyze_timecodes(&clips, &combined, constraints, &options.temporal);
        let span_matches = eligible(&combined);
        combined.extend(timecoded);
        let refined = eligible(&combined);

        // ---- solve
        if let Some(p) = &progress {
            p(PipelineProgress::basic(Phase::Solve, 0, 1, None));
            // Metadata edges also appear in refined previews.
            for m in refined
                .iter()
                .filter(|m| !matches!(m.evidence, align_core::MatchEvidence::Waveform))
            {
                let mut event = PipelineProgress::basic(Phase::Solve, 0, 1, None);
                event.preview = Some(align_core::MatchPreview {
                    left: m.left.clone(),
                    right: m.right.clone(),
                    rate: m.rate,
                    offset: m.offset,
                    confidence: m.confidence,
                    stage: align_core::MatchPreviewStage::Refined,
                });
                p(event);
            }
        }
        let mut stages: Vec<align_core::SyncStage> = Vec::new();
        for (kind, matches) in [
            (align_core::SyncStageKind::Waveform, &waveform_matches),
            (align_core::SyncStageKind::FileSpans, &span_matches),
            (align_core::SyncStageKind::Timecode, &refined),
        ] {
            if cancelled(cancel) {
                if stages.is_empty() {
                    return Err(PipelineError::Cancelled);
                }
                break;
            }
            let stage = solve_stage(kind, &clips, matches, &options.match_policy);
            if stages
                .last()
                .is_none_or(|previous| previous.matches != stage.matches)
            {
                stages.push(stage);
                if let Some(p) = &progress {
                    let mut event = PipelineProgress::basic(Phase::Solve, stages.len(), 3, None);
                    event.completed_stage = Some(kind);
                    p(event);
                }
            }
        }
        let stopped = cancelled(cancel);
        // Prefer the most placed clips; equal counts retain the later stage.
        let selected = if stopped {
            stages.len() - 1
        } else {
            stages
                .iter()
                .enumerate()
                .max_by_key(|(index, stage)| (stage.synchronized_count(), *index))
                .map(|(index, _)| index)
                .expect("waveform stage is always present")
        };
        let chosen = stages[selected].clone();
        // One-stage results retain legacy JSON, with no redundant history.
        let selected_stage = (stages.len() > 1).then_some(selected);
        if selected_stage.is_none() {
            stages.clear();
        }
        if let Some(p) = &progress {
            p(PipelineProgress::basic(Phase::Solve, 1, 1, None));
        }

        Ok(align_core::SyncResult {
            search_overrides,
            stopped,
            stages,
            selected_stage,
            search_accuracy: options.search_accuracy,
            preserve_editing_tracks: options.preserve_editing_tracks.iter().cloned().collect(),
            project: SyncProject {
                clips,
                warnings,
                imported_timeline,
            },
            islands: chosen.islands,
            unmatched: chosen.unmatched,
            matches: chosen.matches,
            temporal_policy: options.temporal.clone(),
        })
    }

    fn inspect_one(&self, url: &Path) -> Result<Clip, PipelineError> {
        let probe = self.backend.inspect(url).map_err(PipelineError::Decode)?;
        let micros = (probe.duration_seconds * 1_000_000.0).round() as i64;
        if micros <= 0 {
            return Err(PipelineError::Decode(crate::DecodeError::InvalidPcm));
        }
        let id = clip_id_for(&url.to_string_lossy(), micros);
        let mut audio: Vec<AudioSummary> = probe
            .audio_streams
            .iter()
            .map(|s| AudioSummary {
                sample_rate: s.sample_rate,
                channels: s.channels,
                bit_depth: s.bit_depth,
                is_float: s.is_float,
                source_timecode: None,
            })
            .collect();

        // Embedded metadata first (both engines share these pure file-IO
        // parsers): Sony tail → BWF bext → filesystem fallback. Only
        // *differences* of embedded timestamps steer matching, and the BWF
        // date resolves in UTC (see meta.rs) — consistent per machine.
        let bwf = align_core::meta::read_bwf(url);
        let sony = align_core::meta::read_sony_tail(url);
        let (recorded_at, recorded_at_source) = sony
            .as_deref()
            .and_then(align_core::meta::sony_recording_date)
            .map(|t| (Some(t), Some(RecordingTimestampSource::EmbeddedMetadata)))
            .or_else(|| {
                bwf.as_ref().and_then(|b| {
                    b.recording_date
                        .map(|t| (Some(t), Some(RecordingTimestampSource::EmbeddedMetadata)))
                })
            })
            .unwrap_or_else(|| {
                std::fs::metadata(url)
                    .ok()
                    .and_then(|m| m.created().or_else(|_| m.modified()).ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| {
                        (
                            Some(d.as_secs() as i64),
                            Some(RecordingTimestampSource::FileSystem),
                        )
                    })
                    .unwrap_or((None, None))
            });
        if !audio.is_empty() {
            // One shared chunk walk (bext/fmt/link/iXML); the explicit
            // priority lives in `audio_timecode`, so both backends agree
            // by construction.
            if let Some(tc) = bwf.as_ref().and_then(|b| b.audio_timecode()) {
                audio[0].source_timecode = Some(tc);
            }
        }

        let video = probe.video.as_ref().map(|v| {
            // Prefer container timecode, then Sony sidecar timecode with the sequence rate.
            let fallback = v.frame_duration.unwrap_or(MediaTime::new(1, 25));
            let source_timecode = v.source_timecode.clone().or_else(|| {
                sony.as_deref()
                    .and_then(|xml| align_core::meta::sony_timecode(xml, fallback))
            });
            align_core::VideoSummary {
                width: v.width,
                height: v.height,
                frame_duration: v.frame_duration,
                source_timecode,
                frame_rate_mode: Some(v.mode),
            }
        });

        // Last metadata tier: LTC recorded as an audio signal. Inspect a
        // short window per discrete channel only when container/BWF/Sony
        // supplied no timecode. Eight consecutive labels guard ordinary
        // programme audio from becoming a clock.
        let has_timecode = video
            .as_ref()
            .and_then(|v| v.source_timecode.as_ref())
            .is_some()
            || audio.iter().any(|a| a.source_timecode.is_some());
        if !has_timecode {
            'streams: for (stream_index, stream) in probe.audio_streams.iter().enumerate() {
                let mut sample_rate = stream.sample_rate;
                let mut channels = vec![Vec::new(); stream.channels];
                let decoded = self.backend.decode_native(
                    url,
                    stream_index,
                    Some((0.0, Some(3.0))),
                    &mut |block| {
                        sample_rate = block.sample_rate;
                        for (output, input) in channels.iter_mut().zip(&block.frames) {
                            output.extend_from_slice(input);
                        }
                        Ok(())
                    },
                );
                if decoded.is_err() {
                    continue;
                }
                for samples in &channels {
                    if let Some(timecode) = crate::ltc::detect(samples, sample_rate) {
                        audio[stream_index].source_timecode = Some(timecode);
                        break 'streams;
                    }
                }
            }
        }

        Ok(Clip {
            id,
            url: url.to_path_buf(),
            kind: if probe.has_video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
            duration: MediaTime::new(micros, 1_000_000),
            audio,
            video,
            recorded_at,
            recorded_at_source,
            source_identifier: sony.as_deref().and_then(align_core::meta::sony_device_id),
            media_span: bwf.and_then(|b| b.media_span),
        })
    }

    fn fingerprints(
        &self,
        clips: &[&Clip],
        requested: &HashMap<ClipId, AudioAnalysisSource>,
        options: FingerprintRunOptions<'_>,
        progress: Option<&(dyn Fn(PipelineProgress) + Send + Sync)>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<
        (
            Vec<align_core::ClipFingerprints>,
            HashMap<ClipId, AudioAnalysisSource>,
        ),
        PipelineError,
    > {
        use align_core::Fingerprint;
        use std::sync::atomic::Ordering;
        struct Variant {
            source: AudioAnalysisSource,
            fingerprints: Vec<Fingerprint>,
            waveform: Option<Vec<f32>>,
        }
        let total = clips.len();
        let done = std::sync::atomic::AtomicUsize::new(0);
        let accuracy = options.accuracy;
        let overrides = options.overrides;
        let generate_waveform_previews = options.generate_waveform_previews;
        // Four streaming readers avoid saturating external media while
        // variants of one clip stay sequential and results keep input order.
        let all_variants: Vec<Result<Vec<Variant>, PipelineError>> =
            align_core::par_map(clips, 4, |clip| {
                if cancelled(cancel) {
                    return Err(PipelineError::Cancelled);
                }
                let accuracy = overrides.get(&clip.id).copied().unwrap_or(accuracy);
                let req = requested
                    .get(&clip.id)
                    .copied()
                    .unwrap_or(AudioAnalysisSource::Automatic);
                let sources: Vec<AudioAnalysisSource> =
                    if req == AudioAnalysisSource::Automatic && clip.audio.len() > 1 {
                        (0..clip.audio.len())
                            .map(|idx| AudioAnalysisSource::Stream {
                                index: idx,
                                channel: None,
                            })
                            .collect()
                    } else {
                        vec![req]
                    };
                let mut variants = Vec::with_capacity(sources.len());
                for source in sources {
                    let cache_key = accuracy.cache_key(&source.cache_key());
                    if let Some(cached) = self.cache.load(&clip.url, &cache_key) {
                        let waveform = if generate_waveform_previews {
                            if let Some(waveform) = self.cache.load_waveform(&clip.url, &cache_key)
                            {
                                Some(waveform)
                            } else {
                                let mut preview = WaveformPreview::new(clip.duration.as_seconds());
                                self.backend
                                    .decode_mono_8k(&clip.url, source, &mut |block| {
                                        if cancelled(cancel) {
                                            return Err(crate::DecodeError::Cancelled);
                                        }
                                        preview.consume(block);
                                        Ok(())
                                    })
                                    .map_err(|error| match error {
                                        crate::DecodeError::Cancelled => PipelineError::Cancelled,
                                        other => PipelineError::Decode(other),
                                    })?;
                                let waveform = preview.finish();
                                self.cache.save_waveform(&waveform, &clip.url, &cache_key);
                                Some(waveform)
                            }
                        } else {
                            None
                        };
                        variants.push(Variant {
                            source,
                            fingerprints: cached,
                            waveform,
                        });
                        continue;
                    }
                    let mut extractor = FingerprintExtractor::with_accuracy(accuracy);
                    let mut preview = generate_waveform_previews
                        .then(|| WaveformPreview::new(clip.duration.as_seconds()));
                    self.backend
                        .decode_mono_8k(&clip.url, source, &mut |block| {
                            if cancelled(cancel) {
                                return Err(crate::DecodeError::Cancelled);
                            }
                            extractor.consume(block);
                            if let Some(preview) = &mut preview {
                                preview.consume(block);
                            }
                            Ok(())
                        })
                        .map_err(|error| match error {
                            crate::DecodeError::Cancelled => PipelineError::Cancelled,
                            other => PipelineError::Decode(other),
                        })?;
                    if cancelled(cancel) {
                        return Err(PipelineError::Cancelled);
                    }
                    let fingerprints = extractor.finish();
                    self.cache.save(&fingerprints, &clip.url, &cache_key);
                    let waveform = preview.map(WaveformPreview::finish);
                    if let Some(waveform) = &waveform {
                        self.cache.save_waveform(waveform, &clip.url, &cache_key);
                    }
                    variants.push(Variant {
                        source,
                        fingerprints,
                        waveform,
                    });
                }
                if let Some(p) = &progress {
                    let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut event = PipelineProgress::basic(
                        Phase::Fingerprint,
                        completed,
                        total,
                        Some(clip.url.clone()),
                    );
                    if generate_waveform_previews {
                        let mut waveform = vec![0.0_f32; WAVEFORM_PREVIEW_BINS];
                        for preview in variants
                            .iter()
                            .filter_map(|variant| variant.waveform.as_ref())
                        {
                            for (combined, value) in waveform.iter_mut().zip(preview) {
                                *combined = combined.max(*value);
                            }
                        }
                        event.waveform = Some((clip.id.clone(), waveform));
                    }
                    p(event);
                }
                Ok(variants)
            });
        let mut collected = Vec::with_capacity(clips.len());
        for variants in all_variants {
            collected.push(variants?);
        }
        let all_variants = collected;
        if let Some(p) = &progress {
            p(PipelineProgress::basic(
                Phase::Fingerprint,
                total,
                total,
                None,
            ));
        }

        if all_variants.iter().all(|variants| variants.len() == 1) {
            return Ok((
                clips
                    .iter()
                    .zip(all_variants)
                    .map(|(clip, mut variants)| align_core::ClipFingerprints {
                        clip_id: clip.id.clone(),
                        fingerprints: variants.pop().unwrap().fingerprints,
                    })
                    .collect(),
                HashMap::new(),
            ));
        }

        // Automatic multi-stream selection: most shared hashes wins.
        // Ties select the lowest channel index.
        let mut first_owner: HashMap<u64, &ClipId> = HashMap::new();
        let mut shared: HashSet<u64> = HashSet::new();
        for (clip, variants) in clips.iter().zip(all_variants.iter()) {
            let hashes: HashSet<u64> = variants
                .iter()
                .flat_map(|v| v.fingerprints.iter().map(|f| f.hash))
                .collect();
            for h in hashes {
                match first_owner.get(&h) {
                    Some(owner) if **owner != clip.id => {
                        shared.insert(h);
                    }
                    Some(_) => {}
                    None => {
                        first_owner.insert(h, &clip.id);
                    }
                }
            }
        }
        let mut features = Vec::with_capacity(clips.len());
        let mut selected = HashMap::new();
        for (clip, variants) in clips.iter().zip(all_variants) {
            let mut best = None::<(usize, usize)>;
            for (idx, v) in variants.iter().enumerate() {
                let score = v
                    .fingerprints
                    .iter()
                    .filter(|f| shared.contains(&f.hash))
                    .count();
                if best.is_none_or(|(_, s)| score > s) {
                    best = Some((idx, score));
                }
            }
            if let Some((idx, _)) = best {
                let mut variants = variants;
                let v = variants.swap_remove(idx);
                if !variants.is_empty() {
                    selected.insert(clip.id.clone(), v.source);
                }
                features.push(align_core::ClipFingerprints {
                    clip_id: clip.id.clone(),
                    fingerprints: v.fingerprints,
                });
            }
        }
        Ok((features, selected))
    }
}

fn solve_stage(
    kind: align_core::SyncStageKind,
    clips: &[Clip],
    matches: &[align_core::PairwiseMatch],
    match_policy: &align_core::MatchPolicy,
) -> align_core::SyncStage {
    let video_clips: HashSet<ClipId> = clips
        .iter()
        .filter(|c| c.kind == MediaKind::Video)
        .map(|c| c.id.clone())
        .collect();
    let solved = solve_graph(
        &clips.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        matches,
        &video_clips,
        match_policy,
    );
    let clips_by_id: HashMap<&ClipId, &Clip> = clips.iter().map(|c| (&c.id, c)).collect();
    let mut islands = Vec::new();
    let mut unmatched = Vec::new();
    for island in solved {
        if island.placements.len() <= 1 {
            unmatched.extend(island.placements.iter().map(|p| p.clip_id.clone()));
            continue;
        }
        let placements = island
            .placements
            .into_iter()
            .filter_map(|placement| {
                let clip = clips_by_id.get(&placement.clip_id)?;
                Some(ClipPlacement {
                    clip_id: placement.clip_id,
                    mapping: TimeMap {
                        points: mapping_points(
                            placement.rate,
                            placement.offset,
                            &placement.mapping_points,
                            clip.duration.as_seconds(),
                        ),
                    },
                    confidence: placement.confidence,
                })
            })
            .collect();
        islands.push(SyncIsland {
            id: islands.len(),
            placements,
        });
    }
    unmatched.sort_by(|a, b| a.0.cmp(&b.0));
    align_core::SyncStage {
        kind,
        islands,
        unmatched,
        matches: matches
            .iter()
            .map(|m| MatchSummary {
                left: m.left.clone(),
                right: m.right.clone(),
                drift_ppm: ((m.rate - 1.0) * 1_000_000_000.0).round() / 1_000.0,
                offset: MediaTime::seconds(m.offset),
                confidence: m.confidence,
                anchors: m.anchors,
                covered: MediaTime::seconds(m.covered_seconds),
                residual: MediaTime::seconds(m.residual_seconds),
                evidence: Some(m.evidence),
            })
            .collect(),
    }
}

/// Mapping knots filtered to the clip range and re-anchored at both ends.
/// Affine fallbacks sampled at source 0…1 must span the full clip duration.
fn mapping_points(
    rate: f64,
    offset: f64,
    points: &[align_core::MapPoint],
    duration: f64,
) -> Vec<MappingPoint> {
    if points.len() < 2 {
        return vec![
            MappingPoint {
                source: MediaTime::seconds(0.0),
                island: MediaTime::seconds(offset),
            },
            MappingPoint {
                source: MediaTime::seconds(duration),
                island: MediaTime::seconds(rate * duration + offset),
            },
        ];
    }
    let map = align_core::PiecewiseTimeMapping::new(points.to_vec());
    let mut sources = vec![0.0];
    sources.extend(
        points
            .iter()
            .map(|p| p.source)
            .filter(|s| (0.0..=duration).contains(s)),
    );
    sources.push(duration);
    sources.sort_by(|a, b| a.total_cmp(b));
    let mut knots: Vec<(f64, f64)> = Vec::new();
    for source in sources {
        if knots.last().is_some_and(|(s, _)| (s - source).abs() < 1e-6) {
            continue;
        }
        knots.push((source, map.value_at(source)));
    }
    align_core::PiecewiseTimeMapping::new(
        knots
            .into_iter()
            .map(|(source, island)| align_core::MapPoint::new(source, island))
            .collect(),
    )
    .points
    .into_iter()
    .map(|p| MappingPoint {
        source: MediaTime::seconds(p.source),
        island: MediaTime::seconds(p.island),
    })
    .collect()
}

/// Canonical ClipID: SHA256 hex (12 bytes) of `path\0duration-micros`.
/// Uses the same duration units across media backends.
pub fn clip_id_for(path: &str, duration_micros: i64) -> ClipId {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(duration_micros.to_string().as_bytes());
    let digest = hasher.finalize();
    ClipId::new(
        digest[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
}

struct TimelineRequest {
    path: PathBuf,
    sequence: Option<usize>,
}

struct ExpandedInputs {
    media: Vec<PathBuf>,
    timeline: Option<TimelineRequest>,
}

fn expand(inputs: &[PipelineInput]) -> Result<ExpandedInputs, PipelineError> {
    let mut media = Vec::new();
    let mut timeline: Option<TimelineRequest> = None;
    for input in inputs {
        match input {
            PipelineInput::Timeline(path) | PipelineInput::TimelineSequence(path, _) => {
                if !path.is_file() {
                    return Err(PipelineError::Inaccessible(path.clone()));
                }
                if timeline.is_some() {
                    return Err(PipelineError::MultipleTimelines);
                }
                let sequence = match input {
                    PipelineInput::TimelineSequence(_, index) => Some(*index),
                    _ => None,
                };
                timeline = Some(TimelineRequest {
                    path: path.clone(),
                    sequence,
                });
            }
            PipelineInput::Media(path) => {
                let meta = std::fs::metadata(path)
                    .map_err(|_| PipelineError::Inaccessible(path.clone()))?;
                if meta.is_dir() {
                    visit_dir(path, &mut media)?;
                    continue;
                }
                match path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                {
                    Some(e) if e == "xml" || e == "fcpxml" || e == "aaf" => {
                        if timeline.is_some() {
                            return Err(PipelineError::MultipleTimelines);
                        }
                        timeline = Some(TimelineRequest {
                            path: path.clone(),
                            sequence: None,
                        });
                    }
                    _ => {
                        if crate::is_supported(path) {
                            media.push(normalize(path));
                        }
                    }
                }
            }
        }
    }
    media.sort();
    media.dedup();
    Ok(ExpandedInputs { media, timeline })
}

fn visit_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PipelineError> {
    let entries =
        std::fs::read_dir(dir).map_err(|_| PipelineError::Inaccessible(dir.to_path_buf()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            visit_dir(&path, out)?;
        } else if meta.is_file() && crate::is_supported(&path) {
            out.push(normalize(&path));
        }
    }
    Ok(())
}

fn normalize(p: &Path) -> PathBuf {
    // Lexical normalization (no symlink resolution: identity must survive
    // relink scenarios where targets move between runs).
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_preview_uses_real_amplitude_and_preserves_silence() {
        let mut accumulator = WaveformPreview::new(4.0);
        let mut samples = vec![0.0; align_core::fingerprint::SAMPLE_RATE as usize];
        samples.extend(vec![0.25; align_core::fingerprint::SAMPLE_RATE as usize]);
        samples.extend(vec![1.0; align_core::fingerprint::SAMPLE_RATE as usize]);
        samples.extend(vec![0.5; align_core::fingerprint::SAMPLE_RATE as usize]);
        accumulator.consume(&samples);
        let preview = accumulator.finish();
        assert_eq!(preview.len(), WAVEFORM_PREVIEW_BINS);
        assert!(preview.iter().all(|value| (0.0..=1.0).contains(value)));
        assert_eq!(preview.iter().copied().fold(0.0_f32, f32::max), 1.0);
        assert!(
            preview[..WAVEFORM_PREVIEW_BINS / 4]
                .iter()
                .all(|value| *value == 0.0)
        );
        assert!(preview[WAVEFORM_PREVIEW_BINS / 4] > 0.0);
        assert!(preview[WAVEFORM_PREVIEW_BINS / 2] > preview[WAVEFORM_PREVIEW_BINS / 4]);
        assert!(preview[WAVEFORM_PREVIEW_BINS * 3 / 4] < 1.0);
    }

    /// Evaluate an island map (linear interp/extrapolation on knots).
    fn island_at(knots: &[MappingPoint], source: f64) -> f64 {
        assert!(knots.len() >= 2);
        let (a, b) = if source <= knots[0].source.as_seconds() {
            (&knots[0], &knots[1])
        } else if source >= knots[knots.len() - 1].source.as_seconds() {
            (&knots[knots.len() - 2], &knots[knots.len() - 1])
        } else {
            let i = knots
                .iter()
                .position(|k| k.source.as_seconds() >= source)
                .unwrap_or(1)
                .max(1);
            (&knots[i - 1], &knots[i])
        };
        let span = b.source.as_seconds() - a.source.as_seconds();
        if span <= 0.0 {
            return a.island.as_seconds();
        }
        a.island.as_seconds()
            + (source - a.source.as_seconds()) / span
                * (b.island.as_seconds() - a.island.as_seconds())
    }

    /// Write a stereo 44.1 kHz float WAV: `delay_sec` of silence, then noise.
    fn write_delayed_wav(dir: &Path, name: &str, noise: &[f32], delay_sec: f64) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..(delay_sec * 44100.0) as usize {
            w.write_sample(0.0f32).unwrap();
            w.write_sample(0.0f32).unwrap();
        }
        for &s in noise {
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    fn write_mono_wav(dir: &Path, name: &str, samples: &[f32]) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// Raw RIFF writer with optional bext (date/time/timeref), link XML and
    /// iXML SPEED XML. `timeref_secs`: TimeReference in seconds at 48 kHz
    /// (also drives the BWF timecode display at 25 fps, like production files).
    fn write_bext_wav(
        dir: &Path,
        name: &str,
        body: &[f32],
        timeref_secs: Option<f64>,
        link_xml: Option<&str>,
    ) -> PathBuf {
        write_bext_wav_ixml(dir, name, body, timeref_secs, link_xml, None)
    }

    fn write_bext_wav_ixml(
        dir: &Path,
        name: &str,
        body: &[f32],
        timeref_secs: Option<f64>,
        link_xml: Option<&str>,
        ixml_xml: Option<&str>,
    ) -> PathBuf {
        let path = dir.join(name);
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes()); // FLOAT
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&(48000u32 * 2 * 4).to_le_bytes());
        wav.extend_from_slice(&8u16.to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        if let Some(t) = timeref_secs {
            let mut bext = vec![0u8; 346];
            bext[320..330].copy_from_slice(b"2024-05-06");
            bext[330..338].copy_from_slice(b"12:34:56");
            let tref = (t * 48000.0) as u64;
            bext[338..346].copy_from_slice(&tref.to_le_bytes());
            wav.extend_from_slice(b"bext");
            wav.extend_from_slice(&346u32.to_le_bytes());
            wav.extend_from_slice(&bext);
        }
        if let Some(xml) = link_xml {
            wav.extend_from_slice(b"link");
            wav.extend_from_slice(&(xml.len() as u32).to_le_bytes());
            wav.extend_from_slice(xml.as_bytes());
            if xml.len() % 2 == 1 {
                wav.push(0);
            }
        }
        if let Some(xml) = ixml_xml {
            wav.extend_from_slice(b"iXML");
            wav.extend_from_slice(&(xml.len() as u32).to_le_bytes());
            wav.extend_from_slice(xml.as_bytes());
            if xml.len() % 2 == 1 {
                wav.push(0);
            }
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&((body.len() * 2 * 4) as u32).to_le_bytes());
        for &s in body {
            wav.extend_from_slice(&s.to_le_bytes());
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&path, &wav).unwrap();
        path
    }

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("align-pipe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Generate a clip with `ffmpeg`, skipping when the binary is absent.
    fn ffmpeg_clip(dir: &std::path::Path, name: &str, args: &[&str]) -> Option<PathBuf> {
        let out = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-v")
            .arg("error")
            .args(args)
            .arg(&out)
            .status();
        match status {
            Ok(s) if s.success() && out.is_file() => Some(out),
            _ => {
                eprintln!("SKIP: ffmpeg generation failed for {name}");
                None
            }
        }
    }

    #[test]
    fn fixed_project_copy_repairs_every_sequence_and_preserves_source() {
        let dir = fixture_dir("fixed-project-copy");
        let media = dir.join("found");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("A.wav"), b"a").unwrap();
        std::fs::write(media.join("B.wav"), b"b").unwrap();
        let source = dir.join("edit.xml");
        let sequence = |name: &str, id: &str, path: &str| {
            format!(
                r#"<sequence><name>{name}</name><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><media><audio><track><enabled>TRUE</enabled><locked>FALSE</locked><clipitem id="{id}"><name>{name}</name><in>0</in><out>25</out><start>0</start><end>25</end><file id="f{id}"><pathurl>file://{path}</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>25</duration></file></clipitem></track></audio></media></sequence>"#
            )
        };
        let original = format!(
            "<?xml version=\"1.0\"?><xmeml>{}{}</xmeml>",
            sequence("One", "a", "/gone/A.wav"),
            sequence("Two", "b", "/gone/B.wav")
        );
        std::fs::write(&source, &original).unwrap();
        let destination = dir.join("edit-fixed.xml");
        let pipeline = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let count = pipeline
            .write_fixed_timeline_copy(
                &[
                    PipelineInput::Timeline(source.clone()),
                    PipelineInput::Media(media.clone()),
                ],
                &PipelineOptions::default(),
                &destination,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        let fixed = std::fs::read_to_string(destination).unwrap();
        // Rewritten file URLs use URL separators and the RFC form
        // `file:///C:/...` on Windows, rather than Path::display's `C:\\...`.
        assert!(fixed.contains("/found/A.wav</pathurl>"));
        assert!(fixed.contains("/found/B.wav</pathurl>"));
        assert!(!fixed.contains("/gone/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_and_relink_warnings_reach_project() {
        // draft.warnings (import findings + relink notes) used to die in
        // the draft: the session must surface them next to decode warnings.
        let dir = fixture_dir("warnmerge");
        let tone = write_mono_wav(&dir, "tone.wav", &noise(48_000, 7));
        let xml = dir.join("edit.xml");
        let uri = format!("file://{}", tone.display());
        std::fs::write(
            &xml,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4">
<sequence>
<name>W</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media>
<audio>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="au1">
<name>Tone</name>
<enabled>TRUE</enabled>
<in>0</in><out>25</out><start>0</start><end>25</end>
<filter><effect><name>Gain</name><effectid>gain</effectid></effect></filter>
<file id="fa"><name>Tone</name><pathurl>{uri}</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>50</duration></file>
</clipitem>
</track>
</audio>
</media>
</sequence>
</xmeml>"#
            ),
        )
        .unwrap();
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let project = pipe
            .open_with(&[PipelineInput::Timeline(xml)], &PipelineOptions::default())
            .expect("open");
        assert!(
            project
                .warnings
                .iter()
                .any(|w| w.message.contains("passed through to Premiere XML")),
            "warnings={:?}",
            project
                .warnings
                .iter()
                .map(|w| &w.message)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_cfr_b_frames_do_not_warn_variable_timing() {
        let dir = fixture_dir("cfr-b-frames");
        let Some(clip) = ffmpeg_clip(
            &dir,
            "b-frames.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=25:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-bf",
                "2",
                "-c:a",
                "aac",
                "-shortest",
            ],
        ) else {
            return;
        };
        let timing = crate::ff::video_timing(&clip).unwrap();
        assert_eq!(timing.mode, align_core::VideoFrameRateMode::Constant);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(clip)],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert!(
            result
                .project
                .warnings
                .iter()
                .all(|w| !w.message.contains("Variable frame rate")),
            "{:?}",
            result.project.warnings
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn portable_vfr_walk_classifies_variable_and_warns() {
        // 30 fps + 15 fps segments concatenated = genuinely variable
        // packet durations.
        let dir = fixture_dir("vfr");
        let seg = |name: &str, rate: u32| {
            ffmpeg_clip(
                &dir,
                name,
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc=size=320x240:rate={rate}:duration=2"),
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=4",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:v",
                    "mpeg4",
                    "-c:a",
                    "aac",
                    "-shortest",
                ],
            )
        };
        let (Some(seg1), Some(seg2)) = (seg("seg1.mp4", 30), seg("seg2.mp4", 15)) else {
            return;
        };
        std::fs::write(
            dir.join("concat.txt"),
            format!("file '{}'\nfile '{}'\n", seg1.display(), seg2.display()),
        )
        .unwrap();
        let vfr = dir.join("vfr.mp4");
        let joined = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                &dir.join("concat.txt").to_string_lossy(),
                "-c",
                "copy",
            ])
            .arg(&vfr)
            .status();
        if !joined.is_ok_and(|s| s.success()) || !vfr.is_file() {
            eprintln!("SKIP: ffmpeg concat failed");
            return;
        }
        // 1. The packet walk itself classifies variable durations.
        let timing = crate::ff::video_timing(&vfr).expect("walk");
        assert_eq!(timing.mode, align_core::VideoFrameRateMode::Variable);
        // 2. A full sync surfaces the CFR caveat on the clip.
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(vfr)],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        assert!(
            result
                .project
                .warnings
                .iter()
                .any(|w| w.message.contains("Variable frame rate detected")),
            "warnings={:?}",
            result
                .project
                .warnings
                .iter()
                .map(|w| &w.message)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_single_frame_container_warns_unverifiable_timing() {
        // One video packet: fewer than 2 timing samples, so the walk
        // reports Unknown and the pipeline warns instead of guessing CFR.
        let dir = fixture_dir("vfr1");
        let Some(one) = ffmpeg_clip(
            &dir,
            "one.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-shortest",
            ],
        ) else {
            return;
        };
        let timing = crate::ff::video_timing(&one).expect("walk");
        assert_eq!(timing.mode, align_core::VideoFrameRateMode::Unknown);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(one)],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        assert!(
            result
                .project
                .warnings
                .iter()
                .any(|w| w.message.contains("does not expose frame sample timing")),
            "warnings={:?}",
            result
                .project
                .warnings
                .iter()
                .map(|w| &w.message)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn independent_groups_and_singleton_survive_export() {
        use align_core::export::{ExportTimeline, TimelineExportFormat};
        let dir = fixture_dir("independent-groups");
        for (prefix, seed) in [("one", 0x123), ("two", 0x987)] {
            let body = noise(24 * 44100, seed);
            write_delayed_wav(&dir, &format!("{prefix}-a.wav"), &body, 0.0);
            write_delayed_wav(&dir, &format!("{prefix}-b.wav"), &body, 1.25);
        }
        write_delayed_wav(&dir, "unrelated.wav", &noise(24 * 44100, 0x555), 0.0);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(result.project.clips.len(), 5);
        assert_eq!(result.islands.len(), 2);
        assert_eq!(result.unmatched.len(), 1);
        assert_eq!(result.matches.len(), 2);
        let name = |id: &ClipId| {
            result
                .project
                .clips
                .iter()
                .find(|c| &c.id == id)
                .unwrap()
                .url
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        };
        assert_eq!(name(&result.unmatched[0]), "unrelated.wav");
        for island in &result.islands {
            assert_eq!(island.placements.len(), 2);
            let a = island
                .placements
                .iter()
                .find(|p| name(&p.clip_id).ends_with("-a.wav"))
                .unwrap();
            let b = island
                .placements
                .iter()
                .find(|p| name(&p.clip_id).ends_with("-b.wav"))
                .unwrap();
            assert_eq!(
                name(&a.clip_id).split('-').next(),
                name(&b.clip_id).split('-').next()
            );
            for t in [2.0, 12.0, 22.0] {
                assert!(
                    (island_at(&a.mapping.points, t) - island_at(&b.mapping.points, t + 1.25))
                        .abs()
                        < 0.001
                );
            }
            for p in &island.placements {
                assert!(!align_core::drift::needs_correction_points(
                    &p.mapping.points
                ));
            }
        }
        for keep_unmatched in [true, false] {
            let timeline = ExportTimeline::from_result(&result, keep_unmatched).unwrap();
            let artifacts = crate::export::export(
                &timeline,
                &dir.join(format!("export-{keep_unmatched}")),
                &[TimelineExportFormat::ResolveOTIO],
                false,
            )
            .unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&artifacts[0].url).unwrap()).unwrap();
            let mut exported = HashMap::<String, usize>::new();
            for track in json["tracks"]["children"].as_array().unwrap() {
                for clip in track["children"].as_array().unwrap() {
                    if clip["OTIO_SCHEMA"] == "Clip.2" {
                        *exported
                            .entry(clip["name"].as_str().unwrap().to_string())
                            .or_default() += 1;
                    }
                }
            }
            assert_eq!(exported.len(), if keep_unmatched { 5 } else { 4 });
            assert_eq!(exported.contains_key("unrelated.wav"), keep_unmatched);
            assert!(exported.values().all(|&channels| channels == 2));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn known_clock_drift_survives_pipeline_and_export() {
        use crate::export::{ExportRequest, export_prepared};
        use align_core::export::{ExportTimeline, TimelineExportFormat};
        let body = noise(120 * 44100, 0xD21F7);
        for (tag, clock_rate) in [
            ("clock-flat", 1.0),
            ("clock-drift", 1.0008),
            ("clock-negative", 0.9992),
        ] {
            let dir = fixture_dir(tag);
            let a = write_delayed_wav(&dir, "a.wav", &body, 0.0);
            // Independent fixture resampling: B[t] contains A[t * clock_rate].
            let warped: Vec<f32> = (0..((body.len() - 1) as f64 / clock_rate) as usize)
                .map(|i| {
                    let pos = i as f64 * clock_rate;
                    let j = pos.floor() as usize;
                    body[j] + (body[j + 1] - body[j]) * (pos - j as f64) as f32
                })
                .collect();
            let b = write_delayed_wav(&dir, "b.wav", &warped, 2.0);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
            let previews = std::sync::Mutex::new(Vec::new());
            let result = pipe
                .synchronize(
                    &[
                        PipelineInput::Media(a.clone()),
                        PipelineInput::Media(b.clone()),
                    ],
                    &[],
                    &PipelineOptions::default(),
                    Some(&|p: PipelineProgress| {
                        if let Some(preview) = p.preview {
                            previews.lock().unwrap().push(preview);
                        }
                    }),
                    &cancel,
                )
                .unwrap();
            assert!(
                result.unmatched.is_empty(),
                "{tag}: unmatched; previews={:?}",
                previews.lock().unwrap()
            );
            assert_eq!(result.islands.len(), 1, "{tag}");
            let map = |path: &Path| {
                let clip = result.project.clips.iter().find(|c| c.url == path).unwrap();
                &result.islands[0]
                    .placements
                    .iter()
                    .find(|p| p.clip_id == clip.id)
                    .unwrap()
                    .mapping
                    .points
            };
            for path in [&a, &b] {
                assert!(
                    map(path).windows(2).all(|w| {
                        w[1].source.as_seconds() > w[0].source.as_seconds()
                            && w[1].island.as_seconds() > w[0].island.as_seconds()
                    }),
                    "{tag}: non-monotonic mapping"
                );
            }
            for t in (1..120).map(f64::from) {
                let error = island_at(map(&a), t) - island_at(map(&b), 2.0 + t / clock_rate);
                assert!(error.abs() < 0.005, "{tag}: t={t}, error={error}");
            }
            let timeline = ExportTimeline::from_result(&result, true).unwrap();
            let output = dir.join("export");
            export_prepared(
                ExportRequest {
                    backend: &crate::portable::PortableBackend,
                    timeline: &timeline,
                    directory: &output,
                    formats: &[TimelineExportFormat::ResolveOTIO],
                    correct_drift: true,
                    include_replaced_sequence: false,
                    include_media_files: false,
                    aaf_frame_duration: None,
                    include_fcpxml_timeline: true,
                    include_fcpxml_multicam: true,
                    group_fcpxml_storylines: false,
                    cancel: &cancel,
                },
                None,
            )
            .unwrap();
            let corrected = output.join("Corrected Audio");
            let count = if corrected.exists() {
                let files: Vec<_> = std::fs::read_dir(corrected)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                for path in &files {
                    let reader = hound::WavReader::open(path).unwrap();
                    assert_eq!(reader.spec().sample_rate, 44100, "{tag}");
                    assert_eq!(reader.spec().channels, 2, "{tag}");
                    let item = timeline
                        .islands
                        .iter()
                        .flat_map(|i| &i.clips)
                        .find(|item| {
                            path.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(&format!(
                                    "{} –",
                                    item.clip.url.file_stem().unwrap().to_string_lossy()
                                ))
                        })
                        .unwrap();
                    let expected: u32 = item
                        .mapping_points
                        .windows(2)
                        .map(|w| {
                            ((w[1].island.as_seconds() - w[0].island.as_seconds()) * 44100.0)
                                .round() as u32
                        })
                        .sum();
                    assert_eq!(reader.duration(), expected, "{tag}: render length");
                    assert!(
                        item.mapping_points[0].source.as_seconds().abs() < 1.0 / 44100.0,
                        "{tag}: mapping omits source head: {:?}",
                        item.mapping_points
                    );
                    assert!(
                        (item.mapping_points.last().unwrap().source.as_seconds()
                            - item.source_duration())
                        .abs()
                            < 1.0 / 44100.0,
                        "{tag}: mapping omits source tail: {:?}",
                        item.mapping_points
                    );
                    let samples: Vec<f32> =
                        reader.into_samples::<f32>().map(Result::unwrap).collect();
                    assert!(
                        samples.iter().all(|v| v.is_finite()),
                        "{tag}: invalid samples"
                    );
                    assert!(
                        samples.chunks_exact(2).all(|v| (v[0] - v[1]).abs() < 1e-6),
                        "{tag}: stereo divergence"
                    );
                }
                files.len()
            } else {
                0
            };
            assert_eq!(count, usize::from(clock_rate != 1.0), "{tag}: sidecars");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn per_clip_search_override_reuses_only_compatible_cache() {
        let dir = fixture_dir("search-override");
        let body = noise(12 * 44100, 0xBCAD);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 1.024);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let inputs = [PipelineInput::Media(dir.clone())];
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let baseline = pipe
            .synchronize(&inputs, &[], &PipelineOptions::default(), None, &cancel)
            .unwrap();
        assert_eq!(pipe.cache.statistics().file_count, 2);
        let id = baseline.project.clips[0].id.clone();
        let options = PipelineOptions {
            search_overrides: [(id, align_core::SearchAccuracy::Exhaustive)]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let result = pipe
            .synchronize(&inputs, &[], &options, None, &cancel)
            .unwrap();
        assert_eq!(
            pipe.cache.statistics().file_count,
            3,
            "only overridden clip needs another cache entry"
        );
        assert_eq!(result.search_overrides, options.search_overrides);
        assert_eq!(result.matches.len(), 1);
        assert!(result.unmatched.is_empty());
        assert!((result.matches[0].offset.as_seconds().abs() - 1.024).abs() < 0.001);
        assert_eq!(
            serde_json::from_slice::<align_core::SyncResult>(&serde_json::to_vec(&result).unwrap())
                .unwrap(),
            result
        );
        let restored = pipe
            .synchronize(&inputs, &[], &PipelineOptions::default(), None, &cancel)
            .unwrap();
        assert_eq!(restored, baseline);
        let source_options = PipelineOptions {
            source_search_overrides: [(
                format!("audio:{}", dir.display()),
                align_core::SearchAccuracy::Deep,
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        write_delayed_wav(&dir, "new.wav", &body, 2.048);
        let inherited = pipe
            .synchronize(&inputs, &[], &source_options, None, &cancel)
            .unwrap();
        assert_eq!(inherited.project.clips.len(), 3);
        assert_eq!(
            inherited.search_overrides.len(),
            3,
            "new source file must inherit the lane budget"
        );
        assert!(
            inherited
                .search_overrides
                .values()
                .all(|level| *level == align_core::SearchAccuracy::Deep)
        );
        assert!(inherited.unmatched.is_empty());
        let other = dir.join("other-source");
        std::fs::create_dir(&other).unwrap();
        write_delayed_wav(&other, "other.wav", &body, 3.072);
        let independent = pipe
            .synchronize(&inputs, &[], &source_options, None, &cancel)
            .unwrap();
        let other_id = &independent
            .project
            .clips
            .iter()
            .find(|clip| clip.url.parent() == Some(other.as_path()))
            .unwrap()
            .id;
        assert!(
            !independent.search_overrides.contains_key(other_id),
            "other folders inherit global search"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn search_levels_keep_independent_caches_and_reproducible_results() {
        use align_core::SearchAccuracy;
        let dir = fixture_dir("search-levels");
        let body = noise(12 * 44100, 0xFACE);
        let a = write_delayed_wav(&dir, "a.wav", &body, 0.0);
        let b = write_delayed_wav(&dir, "b.wav", &body, 1.024);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let inputs = [PipelineInput::Media(a), PipelineInput::Media(b)];
        let cancel = std::sync::atomic::AtomicBool::new(false);
        for (index, accuracy) in SearchAccuracy::ALL.into_iter().enumerate() {
            let options = PipelineOptions {
                search_accuracy: accuracy,
                ..Default::default()
            };
            let cold = pipe
                .synchronize(&inputs, &[], &options, None, &cancel)
                .unwrap();
            assert_eq!(cold.search_accuracy, accuracy);
            assert_eq!(cold.islands.len(), 1, "{accuracy:?}");
            assert!(cold.unmatched.is_empty(), "{accuracy:?}");
            assert_eq!(cold.matches.len(), 1);
            assert!(
                (cold.matches[0].offset.as_seconds().abs() - 1.024).abs() < 0.001,
                "{accuracy:?}: {:?}",
                cold.matches
            );
            assert_eq!(
                pipe.cache.statistics().file_count,
                (index + 1) * 2,
                "search levels must not reuse incompatible fingerprints"
            );
            let warm = pipe
                .synchronize(&inputs, &[], &options, None, &cancel)
                .unwrap();
            assert_eq!(warm, cold, "{accuracy:?}: warm cache changed result");
            let json = serde_json::to_value(&cold).unwrap();
            assert_eq!(
                serde_json::from_value::<align_core::SyncResult>(json.clone()).unwrap(),
                cold
            );
            assert_eq!(
                json.get("searchAccuracy").is_none(),
                accuracy == SearchAccuracy::Balanced,
                "legacy Balanced result encoding must remain unchanged"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn end_to_end_two_clips_share_one_island() {
        // WAV fixtures carry n_frames, so Symphonia probes duration without
        // ffprobe; the test exercises decode → fingerprint → match →
        // refine → solve with zero external binaries.
        let dir = fixture_dir("e2e");
        let body = noise(20 * 44100, 0xE2E);
        let a = write_delayed_wav(&dir, "a.wav", &body, 0.0);
        let _ = &a;
        let _ = write_delayed_wav(&dir, "b.wav", &body, 2.0);

        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let phases = std::sync::Mutex::new(Vec::new());
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                Some(&|p: PipelineProgress| {
                    phases.lock().expect("phases").push(p.phase.clone());
                }),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        assert_eq!(
            result.islands.len(),
            1,
            "islands={:?}",
            result.islands.len()
        );
        assert!(result.unmatched.is_empty());
        assert_eq!(result.project.clips.len(), 2);
        let island = &result.islands[0];
        assert_eq!(island.placements.len(), 2);
        // Content alignment invariant: the same acoustic events must share
        // island time. A[0] == B[2.0] (B has a 2 s silence lead) and
        // A[10] == B[12], within 10 ms. NOTE: comparing knot islands
        // directly is wrong — knots of different clips live at different
        // source times; evaluate each map at the shared content point.
        let map_of: HashMap<&str, &Vec<MappingPoint>> = island
            .placements
            .iter()
            .map(|p| {
                let name = if p.clip_id == result.project.clips[0].id {
                    "first"
                } else {
                    "second"
                };
                (name, &p.mapping.points)
            })
            .collect();
        for (sa, sb) in [(0.0, 2.0), (10.0, 12.0)] {
            let ia = island_at(map_of["first"], sa);
            let ib = island_at(map_of["second"], sb);
            assert!((ia - ib).abs() < 0.01, "a[{sa}]={ia} b[{sb}]={ib}");
        }
        assert_eq!(result.matches.len(), 1);
        assert!(result.matches[0].confidence >= 0.55);
        let phases = phases.lock().expect("phases");
        assert!(phases.contains(&Phase::Inspect));
        assert!(phases.contains(&Phase::Solve));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_alignment_is_sample_exact() {
        use align_core::gcc_phat_align;
        let dir = fixture_dir("dbg");
        let body = noise(20 * 44100, 0xE2E);
        let a = write_delayed_wav(&dir, "a.wav", &body, 0.0);
        let b = write_delayed_wav(&dir, "b.wav", &body, 2.0);
        let backend = crate::portable::PortableBackend;
        use crate::backend::MediaBackend;
        let mut sa = Vec::new();
        backend
            .decode_mono_8k(&a, align_core::AudioAnalysisSource::Automatic, &mut |s| {
                sa.extend_from_slice(s);
                Ok(())
            })
            .unwrap();
        let mut sb = Vec::new();
        backend
            .decode_mono_8k(&b, align_core::AudioAnalysisSource::Automatic, &mut |s| {
                sb.extend_from_slice(s);
                Ok(())
            })
            .unwrap();
        // Post-delay-compensation lengths: content ± one FFT quantum of
        // silent ring-down (length exactness is not promised; bounded
        // emission + determinism are — see mono.rs).
        assert!(
            (sa.len() as isize - 160_000).abs() <= 800,
            "len={}",
            sa.len()
        );
        assert!(
            (sb.len() as isize - 176_000).abs() <= 800,
            "len={}",
            sb.len()
        );
        assert!(sa.len() >= 160_000 - 64 && sb.len() >= 176_000 - 64);
        // b starts with 2 s of digital silence.
        assert!(sb[..8000].iter().all(|v| *v == 0.0));
        assert!(sb[16000..24000].iter().any(|v| *v != 0.0));
        // Full-stream GCC: exactly 16000 samples = 2.0 s.
        let r = gcc_phat_align(&sa, &sb, 40000).expect("align");
        assert!(
            (r.lag_samples.abs() - 16000.0).abs() < 2.0,
            "lag={}",
            r.lag_samples
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pipeline_reads_ltc_from_audio_signal() {
        let dir = fixture_dir("ltc");
        let wav = write_mono_wav(&dir, "ltc.wav", &crate::ltc::synthetic_ltc(0, 40, 25));
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let project = pipe
            .open(&[PipelineInput::Media(wav)])
            .expect("inspect LTC WAV");
        let timecode = project.clips[0].audio[0]
            .source_timecode
            .as_ref()
            .expect("LTC source timecode");
        assert_eq!(timecode.text, "01:02:03:00");
        assert_eq!(timecode.frame_duration, MediaTime::new(1, 25));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn apple_engine_end_to_end_matches_portable() {
        // Same fixtures through AVFoundation: one island, same content
        // alignment within 10 ms. ClipIDs may differ from portable
        // (duration rationals), so compare placements, not ids.
        let dir = fixture_dir("e2e-apple");
        let body = noise(20 * 44100, 0xE2E);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 2.0);

        let pipe = Pipeline::new_in(BackendKind::AppleNative, Some(dir.join(".cache")));
        assert_eq!(pipe.backend_kind(), BackendKind::AppleNative);
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("apple pipeline");
        assert_eq!(result.islands.len(), 1);
        assert!(result.unmatched.is_empty());
        let island = &result.islands[0];
        assert_eq!(island.placements.len(), 2);
        let by_id: HashMap<&ClipId, &Vec<MappingPoint>> = island
            .placements
            .iter()
            .map(|p| (&p.clip_id, &p.mapping.points))
            .collect();
        let ids: Vec<&&ClipId> = by_id.keys().collect();
        for (sa, sb) in [(0.0, 2.0), (10.0, 12.0)] {
            // Each clip's own map evaluated at the shared content points;
            // which placement is A vs B is resolved by best pairing.
            let pairs = [(ids[0], ids[1]), (ids[1], ids[0])];
            let best = pairs
                .iter()
                .map(|(a, b)| (island_at(by_id[*a], sa) - island_at(by_id[*b], sb)).abs())
                .fold(f64::INFINITY, f64::min);
            assert!(best < 0.01, "content misaligned: {best}");
        }
        assert_eq!(result.matches.len(), 1);
        assert!((result.matches[0].offset.as_seconds() - 2.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clip_ids_are_canonical() {
        let a = clip_id_for("/v/a.wav", 20_000_000);
        let b = clip_id_for("/v/a.wav", 20_000_001);
        assert_ne!(a, b);
        assert_eq!(a.0.len(), 24);
        assert_eq!(clip_id_for("/v/a.wav", 20_000_000), a);
    }

    #[test]
    fn bwf_enriches_clip() {
        use align_core::RecordingTimestampSource;
        let dir = fixture_dir("bwf");
        // TimeReference 45296 s @48 kHz → 2024-05-06T12:34:56Z + TC.
        let body = noise(10 * 48000, 0xBE);
        let wav = write_bext_wav(&dir, "take.wav", &body, Some(45_296.0), None);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        let clip = result
            .project
            .clips
            .iter()
            .find(|c| c.url == wav)
            .expect("clip");
        assert_eq!(clip.recorded_at, Some(1_714_998_896));
        assert_eq!(
            clip.recorded_at_source,
            Some(RecordingTimestampSource::EmbeddedMetadata)
        );
        let tc = clip.audio[0].source_timecode.as_ref().expect("tc");
        assert_eq!(tc.text, "12:34:56:00");
        assert!(clip.media_span.is_none());
        assert!(clip.video.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spanned_parts_join_without_waveform() {
        use align_core::MatchEvidence;
        let dir = fixture_dir("span");
        // Acoustically unrelated parts of one link set must still join.
        let a_body = noise(10 * 48000, 0xAA);
        let b_body = noise(10 * 48000, 0xBB);
        // NOTE: link XML built per file below (actual name differs).
        let xml_a = "<LINK><ID>SET1</ID><FILE type=\"actual\"><FILENUMBER>1</FILENUMBER><FILENAME>p1.wav</FILENAME></FILE><FILE><FILENUMBER>2</FILENUMBER><FILENAME>p2.wav</FILENAME></FILE></LINK>";
        let xml_b = "<LINK><ID>SET1</ID><FILE><FILENUMBER>1</FILENUMBER><FILENAME>p1.wav</FILENAME></FILE><FILE type=\"actual\"><FILENUMBER>2</FILENUMBER><FILENAME>p2.wav</FILENAME></FILE></LINK>";
        write_bext_wav(&dir, "p1.wav", &a_body, None, Some(xml_a));
        write_bext_wav(&dir, "p2.wav", &b_body, None, Some(xml_b));
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        assert_eq!(result.islands.len(), 1, "must join by span");
        assert!(result.unmatched.is_empty());
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.evidence, Some(MatchEvidence::SpannedMetadata));
        assert!((m.covered.as_seconds() - 10.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timecode_overlap_joins_without_waveform() {
        use align_core::MatchEvidence;
        let dir = fixture_dir("tc");
        // A[100..160] and B[120..180] by BWF timecode, unrelated audio.
        let a_body = noise(60 * 48000, 0xC1);
        let b_body = noise(60 * 48000, 0xC2);
        write_bext_wav(&dir, "a.wav", &a_body, Some(100.0), None);
        write_bext_wav(&dir, "b.wav", &b_body, Some(120.0), None);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        // Trusted BWF overlap must produce a usable synchronized group,
        // even when the microphones carry unrelated waveforms.
        assert_eq!(result.islands.len(), 1);
        assert!(result.unmatched.is_empty());
        let placements = &result.islands[0].placements;
        assert_eq!(placements.len(), 2);
        let mapping = |name: &str| {
            let clip = result
                .project
                .clips
                .iter()
                .find(|c| c.url.file_name().unwrap() == name)
                .unwrap();
            &placements
                .iter()
                .find(|p| p.clip_id == clip.id)
                .unwrap()
                .mapping
                .points
        };
        for t in [0.0, 20.0, 39.0] {
            assert!(
                (island_at(mapping("a.wav"), t + 20.0) - island_at(mapping("b.wav"), t)).abs()
                    < 0.001
            );
        }
        assert_eq!(result.matches.len(), 1);
        let m = &result.matches[0];
        assert_eq!(m.evidence, Some(MatchEvidence::Timecode));
        // Left/right follow ClipID hash order; check offset magnitude and overlap.
        assert!(
            (m.offset.as_seconds().abs() - 20.0).abs() < 0.01,
            "offset={}",
            m.offset.as_seconds()
        );
        assert!((m.covered.as_seconds() - 40.0).abs() < 0.01);
        assert_eq!(result.stages.len(), 2);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let stopped = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                Some(&|event: PipelineProgress| {
                    if event.completed_stage == Some(align_core::SyncStageKind::Waveform) {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }),
                &stop,
            )
            .unwrap();
        assert!(stopped.stopped);
        assert!(
            stopped.matches.is_empty(),
            "later timecode rescue must not leak into stopped result"
        );
        assert_eq!(stopped.unmatched.len(), 2);
        assert_eq!(
            serde_json::from_slice::<align_core::SyncResult>(
                &serde_json::to_vec(&stopped).unwrap()
            )
            .unwrap(),
            stopped
        );
        assert_eq!(result.selected_stage, Some(1));
        assert_eq!(result.stages[0].kind, align_core::SyncStageKind::Waveform);
        assert_eq!(result.stages[1].kind, align_core::SyncStageKind::Timecode);
        let mut selected = result.clone();
        assert!(selected.select_stage(0));
        assert!(selected.matches.is_empty());
        assert_eq!(selected.unmatched.len(), 2);
        let assembly = || align_core::export_model::ExportAssemblyOptions {
            unmatched: align_core::export_model::UnmatchedPlacement::Remove,
            ..Default::default()
        };
        assert!(
            align_core::export_model::ExportTimeline::from_result_with_options(
                &selected,
                assembly()
            )
            .is_err()
        );
        let encoded = serde_json::to_vec(&selected).unwrap();
        let mut decoded: align_core::SyncResult = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, selected);
        let before = decoded.clone();
        assert!(!decoded.select_stage(99));
        assert_eq!(decoded, before);
        assert!(decoded.select_stage(1));
        let timeline = align_core::export_model::ExportTimeline::from_result_with_options(
            &decoded,
            assembly(),
        )
        .unwrap();
        assert_eq!(timeline.combined_island(1.0).clips.len(), 2);
        assert_eq!(decoded, result);
        let rejected = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[align_core::SyncConstraint::rejecting_pair(
                    m.left.clone(),
                    m.right.clone(),
                )],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert!(rejected.matches.is_empty());
        assert!(rejected.stages.iter().all(|stage| stage.matches.is_empty()));
        assert_eq!(rejected.unmatched.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ixml_sets_display_bext_keeps_elapsed() {
        // EBU precedence: bext TimeReference drives elapsed even when the
        // redundant iXML timestamp copy disagrees; iXML RATE/FLAG drive
        // only the display label.
        let dir = fixture_dir("ixml");
        let body = noise(10 * 48000, 0x11);
        let ixml_a = r#"<?xml version="1.0" encoding="UTF-8"?><BWFXML><SPEED><TIMECODE_RATE>30000/1001</TIMECODE_RATE><TIMECODE_FLAG>DF</TIMECODE_FLAG><TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>0</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI><TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>47952000</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO><TIMESTAMP_SAMPLE_RATE>48000</TIMESTAMP_SAMPLE_RATE></SPEED></BWFXML>"#;
        let a = write_bext_wav_ixml(&dir, "a.wav", &body, Some(100.0), None, Some(ixml_a));
        // No bext at all: the iXML timestamp copy (3600 s) is the fallback.
        let ixml_b = r#"<?xml version="1.0" encoding="UTF-8"?><BWFXML><SPEED><TIMECODE_RATE>30000/1001</TIMECODE_RATE><TIMECODE_FLAG>DF</TIMECODE_FLAG><TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>0</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI><TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>172800000</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO><TIMESTAMP_SAMPLE_RATE>48000</TIMESTAMP_SAMPLE_RATE></SPEED></BWFXML>"#;
        let b = write_bext_wav_ixml(&dir, "b.wav", &body, None, None, Some(ixml_b));

        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let project = pipe
            .open(&[PipelineInput::Media(dir.clone())])
            .expect("open");
        let tc_of = |url: &PathBuf| {
            project
                .clips
                .iter()
                .find(|c| &c.url == url)
                .expect("clip")
                .audio[0]
                .source_timecode
                .clone()
                .expect("tc")
        };
        // bext wins: exactly 100 s elapsed, DF display at 29.97.
        let ta = tc_of(&a);
        assert!(
            (ta.as_seconds() - 100.0).abs() < 1e-9,
            "elapsed={}",
            ta.as_seconds()
        );
        assert!(ta.drop_frame);
        // Elapsed base stays the audio rate; the iXML rate lives only in
        // the derived display label.
        assert_eq!(ta.frame_number, 100 * 48_000);
        assert_eq!(ta.frame_duration, align_core::MediaTime::new(1, 48_000));
        assert_eq!(ta.text, "00:01:39;29");
        // iXML fallback: 3600 s elapsed → 01:00:00;00 DF anchor.
        let tb = tc_of(&b);
        assert!(
            (tb.as_seconds() - 3600.0).abs() < 1e-9,
            "elapsed={}",
            tb.as_seconds()
        );
        assert_eq!(tb.text, "01:00:00;00");
        assert_eq!(tb.frame_number, 172_800_000);

        #[cfg(target_os = "macos")]
        {
            // Cross-backend: AVFoundation inspect flows through the same
            // shared metadata seam, so labels and elapsed agree exactly.
            let apple = Pipeline::new_in(BackendKind::AppleNative, Some(dir.join(".cache-a")));
            let aproj = apple
                .open(&[PipelineInput::Media(dir.clone())])
                .expect("apple open");
            for url in [&a, &b] {
                let atc = aproj
                    .clips
                    .iter()
                    .find(|c| &c.url == url)
                    .expect("apple clip")
                    .audio[0]
                    .source_timecode
                    .clone()
                    .expect("apple tc");
                let ptc = tc_of(url);
                assert_eq!(atc, ptc, "backend mismatch for {}", url.display());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waveform_match_survives_conflicting_timecode() {
        use align_core::MatchEvidence;
        // Same audio (2 s silence lead on b ⇒ waveform offset −2 s) with
        // overlapping BWF timecodes implying a −10 s offset: the confident
        // waveform edge must stand and no timecode edge may second-guess it.
        let dir = fixture_dir("wvtc");
        let content = noise(20 * 48000, 0x9E);
        let mut lead = vec![0.0f32; 2 * 48000];
        lead.extend_from_slice(&content);
        write_bext_wav(&dir, "a.wav", &content, Some(100.0), None);
        write_bext_wav(&dir, "b.wav", &lead, Some(110.0), None);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        assert_eq!(result.islands.len(), 1);
        assert!(result.unmatched.is_empty());
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].evidence, Some(MatchEvidence::Waveform));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serialized_timecode_clips_still_parse() {
        // Pre-core JSON shape (25 fps display grid) must keep parsing with
        // identical elapsed seconds — the struct shape never changed.
        let json = r#"{
            "id": "abc", "url": "/v/a.wav", "kind": "audio",
            "duration": {"value": 20000000, "timescale": 1000000},
            "audio": [{"sampleRate": 48000.0, "channels": 2,
                "bitDepth": null, "isFloat": null,
                "sourceTimecode": {"text": "12:34:56:00", "frameNumber": 1132400,
                    "frameDuration": {"value": 1, "timescale": 25},
                    "dropFrame": false}}],
            "video": null, "recordedAt": null, "recordedAtSource": null,
            "sourceIdentifier": null, "mediaSpan": null
        }"#;
        let clip: align_core::Clip = serde_json::from_str(json).expect("clip");
        let tc = clip.source_timecode().expect("tc");
        assert_eq!(tc.text, "12:34:56:00");
        assert!((tc.as_seconds() - 45_296.0).abs() < 1e-9);
    }

    #[test]
    fn temporal_policy_flows_into_result_untouched_matching() {
        use align_core::{MatchEvidence, TemporalMode};
        let dir = fixture_dir("tpol");
        let body = noise(12 * 44100, 0x70);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 2.0);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let mut options = PipelineOptions::default();
        options.temporal.default = TemporalMode::RecStart;
        let result = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &options,
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        // Policy round-trips into the result; the waveform edge stands.
        assert_eq!(result.temporal_policy.default, TemporalMode::RecStart);
        assert_eq!(result.islands.len(), 1);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].evidence, Some(MatchEvidence::Waveform));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_sync_results_parse_as_all_auto() {
        let json = r#"{"project":{"clips":[],"warnings":[],"importedTimeline":null},"islands":[],"unmatched":[],"matches":[]}"#;
        let result: align_core::SyncResult = serde_json::from_str(json).expect("parse");
        assert_eq!(
            result.temporal_policy,
            align_core::TemporalPolicy::default()
        );
        let mut result = result;
        result.temporal_policy.default = align_core::TemporalMode::RecStop;
        let back: align_core::SyncResult =
            serde_json::from_str(&serde_json::to_string(&result).expect("json")).expect("parse");
        assert_eq!(back.temporal_policy, result.temporal_policy);
    }

    #[test]
    fn mp4_video_inspect_finds_cfr_track() {
        use crate::backend::MediaBackend;
        let ffmpeg = match crate::ff::ffmpeg_bin() {
            Some(b) => b,
            None => {
                eprintln!("SKIP: no ffmpeg binary");
                return;
            }
        };
        let dir = fixture_dir("mp4");
        let mp4 = dir.join("clip.mp4");
        // 3 s CFR 25 fps testsrc + sine: deterministic container.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1280x720:rate=25:duration=3",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&mp4)
            .status()
            .expect("spawn ffmpeg");
        if !status.success() {
            eprintln!("SKIP: ffmpeg mp4 generation failed");
            return;
        }
        let portable = crate::portable::PortableBackend
            .inspect(&mp4)
            .expect("inspect");
        assert!(portable.has_video);
        let v = portable.video.as_ref().expect("video probe");
        assert_eq!((v.width, v.height), (1280, 720));
        assert_eq!(v.mode, align_core::VideoFrameRateMode::Constant);
        assert_eq!(v.frame_duration, Some(align_core::MediaTime::new(1, 25)));
        assert!((portable.duration_seconds - 3.0).abs() < 0.1);
        assert_eq!(portable.audio_streams.len(), 1);

        #[cfg(target_os = "macos")]
        {
            let apple = crate::apple::AppleNativeBackend
                .inspect(&mp4)
                .expect("apple inspect");
            assert!(apple.has_video);
            let av = apple.video.as_ref().expect("apple video probe");
            assert_eq!((av.width, av.height), (1280, 720));
            // Same classifier, same verdict on a CFR file.
            assert_eq!(av.mode, align_core::VideoFrameRateMode::Constant);
            assert_eq!(av.frame_duration, v.frame_duration);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn xml_inputs_are_explicit_not_silent() {
        let dir = fixture_dir("xml");
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, "<xmeml></xmeml>").unwrap();
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        // A sequence-less file errors explicitly (no silent empty result).
        let err = pipe
            .synchronize(
                &[PipelineInput::Timeline(xml)],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect_err("xml must not silently pass");
        assert!(matches!(err, PipelineError::Timeline(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeline_xml_resolves_against_inspected_clips() {
        let dir = fixture_dir("tl");
        let body = noise(10 * 44100, 0x71);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        let media_url = format!("file://{}", dir.join("a.wav").display());
        let xml_text = format!(
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>Cut</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><audio><track>
<clipitem id="e1"><name>A</name><in>0</in><out>250</out><start>0</start><end>250</end>
<file id="f1"><pathurl>{media_url}</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file>
</clipitem>
</track></audio></media>
</sequence></xmeml>"#
        );
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, xml_text).unwrap();
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let result = pipe
            .synchronize(
                &[
                    PipelineInput::Media(dir.clone()),
                    PipelineInput::Timeline(xml),
                ],
                &[],
                &PipelineOptions {
                    source_search_overrides: [(
                        "audio:imported-audio-000001".to_string(),
                        align_core::SearchAccuracy::Deep,
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline");
        let timeline = result.project.imported_timeline.as_ref().expect("timeline");
        assert_eq!(
            result.search_overrides.get(&timeline.edits[0].clip_id),
            Some(&align_core::SearchAccuracy::Deep)
        );
        assert_eq!(timeline.name, "Cut");
        assert_eq!(timeline.edits.len(), 1);
        assert_eq!(timeline.edits[0].id, "e1");
        assert!((timeline.edits[0].timeline_end.as_seconds() - 10.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_dir_is_no_media() {
        let dir = fixture_dir("empty");
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let err = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect_err("empty");
        assert!(matches!(err, PipelineError::NoMedia));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mapping_points_span_full_duration() {
        use align_core::MapPoint;
        // Affine fallback (e.g. graph fallback sampled at source 0…1)
        // must still cover the whole clip, not collapse to a 1 s bar.
        let knots = mapping_points(1.0, 12.5, &[], 730.8);
        assert_eq!(knots.len(), 2);
        assert!((knots[0].source.as_seconds() - 0.0).abs() < 1e-9);
        assert!((knots[1].source.as_seconds() - 730.8).abs() < 1e-9);
        assert!((knots[0].island.as_seconds() - 12.5).abs() < 1e-9);
        assert!((knots[1].island.as_seconds() - 743.3).abs() < 1e-9);

        let degenerate = vec![MapPoint::new(0.0, 0.0), MapPoint::new(1.0, 1.0)];
        let knots = mapping_points(1.0, 0.0, &degenerate, 730.8);
        assert!((knots.first().expect("knots").source.as_seconds() - 0.0).abs() < 1e-9);
        assert!(
            (knots.last().expect("knots").source.as_seconds() - 730.8).abs() < 1e-9,
            "fallback knots must re-anchor at duration, got {:?}",
            knots.last().expect("knots").source
        );

        // Real propagated knots keep their interior points.
        let real = vec![
            MapPoint::new(4.37, 12.9),
            MapPoint::new(300.0, 310.0),
            MapPoint::new(715.4, 723.9),
        ];
        let knots = mapping_points(1.0, 8.5, &real, 720.3);
        assert!(knots.len() >= 3);
        assert!((knots.first().expect("knots").source.as_seconds() - 0.0).abs() < 1e-9);
        assert!((knots.last().expect("knots").source.as_seconds() - 720.3).abs() < 1e-9);
    }

    #[test]
    fn parallel_runs_are_bit_identical() {
        // Indexed ordered results: threads must never perturb output.
        let dir = fixture_dir("det");
        let body = noise(20 * 44100, 0xDE7);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 2.0);
        // Fresh caches per run so decode actually races.
        let run = || {
            let cache = dir.join(format!(
                "cache-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos())
            ));
            let pipe = Pipeline::new_in(BackendKind::Portable, Some(cache));
            pipe.synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("pipeline")
        };
        let (a, b) = (run(), run());
        let ja = serde_json::to_string(&a).expect("json");
        let jb = serde_json::to_string(&b).expect("json");
        assert_eq!(ja, jb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_during_matching_or_refinement_never_reports_success() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = fixture_dir("cancel-late");
        let body = noise(10 * 44100, 0xCA);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 1.0);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        for phase in [Phase::Fingerprint, Phase::Match, Phase::Refine] {
            let cancel = AtomicBool::new(false);
            let observed = AtomicBool::new(false);
            let result = pipe.synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                Some(&|event: PipelineProgress| {
                    if event.phase == phase {
                        observed.store(true, Ordering::Relaxed);
                        cancel.store(true, Ordering::Relaxed);
                    }
                }),
                &cancel,
            );
            assert!(
                observed.load(Ordering::Relaxed),
                "phase {phase:?} not exercised"
            );
            assert!(
                matches!(result, Err(PipelineError::Cancelled)),
                "{phase:?}: {result:?}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_pipeline_reports_cancelled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = fixture_dir("cancel");
        let body = noise(10 * 44100, 0xCA);
        write_delayed_wav(&dir, "a.wav", &body, 0.0);
        write_delayed_wav(&dir, "b.wav", &body, 1.0);
        let pipe = Pipeline::new_in(BackendKind::Portable, Some(dir.join(".cache")));
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let err = pipe
            .synchronize(
                &[PipelineInput::Media(dir.clone())],
                &[],
                &PipelineOptions::default(),
                None,
                &cancel,
            )
            .expect_err("cancelled");
        assert!(matches!(err, PipelineError::Cancelled));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
