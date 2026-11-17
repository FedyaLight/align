//! Export preparation and format dispatch.
//!
//! An export first plans every audio file the timeline writers will reference
//! (drift corrections, Premiere placement pads, Resolve precision stems) and
//! attaches their paths to the timeline. The plan is then rendered job by job
//! with progress, and the timelines are written last. Prepared audio is
//! reused when its file name, which encodes its full identity, already exists.
//! Single and batch exports share the same preparation path.

use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(not(unix))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use align_core::drift::needs_correction_points;
use align_core::export::fcpxml::FcpxmlOptions;
use align_core::export::model::{
    ExportIsland, ExportItem, ExportTimeline, TimelineExportError, TimelineExportFormat,
    placement_pad_samples, sequence_fps,
};
use align_core::model::file_name;
use align_core::{MediaKind, MediaTime};

use crate::aaf::AafError;
use crate::backend::MediaBackend;
use crate::process::WaitError;
use crate::render::{
    ChannelStems, RenderError, render_channels_cancellable, render_drift_cancellable,
    render_pad_cancellable,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExportArtifact {
    pub format: ExportArtifactFormat,
    pub url: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExportArtifactFormat {
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
    #[serde(rename = "mediaFile")]
    MediaFile,
}

impl From<TimelineExportFormat> for ExportArtifactFormat {
    fn from(format: TimelineExportFormat) -> Self {
        match format {
            TimelineExportFormat::Aaf => Self::Aaf,
            TimelineExportFormat::ResolveOTIO => Self::ResolveOTIO,
            TimelineExportFormat::ResolveScript => Self::ResolveScript,
            TimelineExportFormat::ResolveXML => Self::ResolveXML,
            TimelineExportFormat::PremiereXML => Self::PremiereXML,
            TimelineExportFormat::FinalCutProXML => Self::FinalCutProXML,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportJobProgress {
    pub completed: usize,
    pub total: usize,
    pub current: Option<PathBuf>,
    pub kind: ExportJobKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportJobKind {
    Audio,
    Media,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Timeline(#[from] TimelineExportError),
    #[error("Could not prepare {operation} audio for {file}: {source}")]
    Audio {
        file: String,
        operation: &'static str,
        #[source]
        source: RenderError,
    },
    #[error("Could not export media for {file}: {message}")]
    Media { file: String, message: String },
    #[error("Cannot read {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: io::Error,
    },
    #[error("Cannot write export files: {0}")]
    Write(#[source] io::Error),
    #[error("Cannot build the timeline document: {0}")]
    Document(String),
    #[error(transparent)]
    Aaf(AafError),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("Cancelled.")]
    Cancelled,
}

impl ExportError {
    /// A render failure, named by the timeline media it was prepared from.
    fn audio(media: &Path, operation: &'static str, error: RenderError) -> Self {
        match error {
            RenderError::Cancelled => Self::Cancelled,
            source => Self::Audio {
                file: file_name(media),
                operation,
                source,
            },
        }
    }

    fn read(path: &Path) -> impl FnOnce(io::Error) -> Self {
        let file = file_name(path);
        move |source| Self::Read { file, source }
    }

    fn media(path: &Path, message: impl std::fmt::Display) -> Self {
        Self::Media {
            file: file_name(path),
            message: message.to_string(),
        }
    }

    /// Media preservation reports cancellation as an interrupted write.
    fn write_or_cancelled(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::Interrupted {
            Self::Cancelled
        } else {
            Self::Write(error)
        }
    }
}

impl From<AafError> for ExportError {
    fn from(error: AafError) -> Self {
        match error {
            AafError::Cancelled => Self::Cancelled,
            other => Self::Aaf(other),
        }
    }
}

impl From<WaitError> for ExportError {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::Cancelled => Self::Cancelled,
            WaitError::Io(error) => Self::Write(error),
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), ExportError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

/// What an export produces, independent of the timeline and media engine.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub formats: Vec<TimelineExportFormat>,
    /// Render drift-corrected audio for recorders whose clock drifts.
    pub correct_drift: bool,
    /// Premiere/Resolve XML: also write the sequence with camera audio replaced.
    pub include_replaced_sequence: bool,
    /// Copy each camera file with the synchronized recorder audio.
    pub include_media_files: bool,
    /// Override the AAF composition timecode rate used by Resolve. Picture
    /// source slots retain their native edit rates.
    pub aaf_frame_duration: Option<MediaTime>,
    pub fcpxml: FcpxmlOptions,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            formats: TimelineExportFormat::default_formats(),
            correct_drift: true,
            include_replaced_sequence: false,
            include_media_files: false,
            aaf_frame_duration: None,
            fcpxml: FcpxmlOptions::default(),
        }
    }
}

impl ExportOptions {
    fn wants(&self, format: TimelineExportFormat) -> bool {
        self.formats.contains(&format)
    }
}

/// One synchronized timeline to export.
pub struct ExportRequest<'a> {
    pub backend: &'a dyn MediaBackend,
    pub timeline: &'a ExportTimeline,
    pub directory: &'a Path,
    pub options: &'a ExportOptions,
    pub cancel: &'a AtomicBool,
}

/// One export action containing several independently synchronized sequences.
/// Premiere receives one multi-sequence project; formats whose containers hold
/// one timeline are written into numbered sequence folders.
pub struct ExportBatchRequest<'a> {
    pub backend: &'a dyn MediaBackend,
    pub timelines: &'a [ExportTimeline],
    pub directory: &'a Path,
    pub options: &'a ExportOptions,
    pub cancel: &'a AtomicBool,
}

pub fn export_prepared_many(
    request: ExportBatchRequest<'_>,
    mut progress: Option<&mut dyn FnMut(ExportJobProgress)>,
) -> Result<Vec<ExportArtifact>, ExportError> {
    let ExportBatchRequest {
        backend,
        timelines,
        directory,
        options,
        cancel,
    } = request;
    if let [timeline] = timelines {
        return export_prepared(
            ExportRequest {
                backend,
                timeline,
                directory,
                options,
                cancel,
            },
            progress,
        );
    }
    if timelines.is_empty() {
        return Err(TimelineExportError::NoSynchronizedIslands.into());
    }

    std::fs::create_dir_all(directory).map_err(ExportError::Write)?;
    let mut artifacts = Vec::new();
    let mut premiere_documents = Vec::new();
    let mut fcpxml_documents = Vec::new();
    let mut aaf_manifests = Vec::new();
    for (index, timeline) in timelines.iter().enumerate() {
        check_cancel(cancel)?;
        let sequence_dir = directory.join(sequence_folder(index, &timeline.name));
        let (sequence_artifacts, aaf_manifest) = export_prepared_internal(
            ExportRequest {
                backend,
                timeline,
                directory: &sequence_dir,
                options,
                cancel,
            },
            progress
                .as_mut()
                .map(|report| &mut **report as &mut dyn FnMut(ExportJobProgress)),
            true,
        )?;
        aaf_manifests.extend(aaf_manifest);
        for artifact in sequence_artifacts {
            let documents = match artifact.format {
                ExportArtifactFormat::PremiereXML => &mut premiere_documents,
                ExportArtifactFormat::FinalCutProXML => &mut fcpxml_documents,
                _ => {
                    artifacts.push(artifact);
                    continue;
                }
            };
            documents.push(
                std::fs::read_to_string(&artifact.url).map_err(ExportError::read(&artifact.url))?,
            );
            let _ = std::fs::remove_file(artifact.url);
        }
    }
    if !premiere_documents.is_empty() {
        let xml = align_core::export::premiere::combine_project_documents(&premiere_documents)
            .map_err(ExportError::Document)?;
        let url = directory.join("Align – Adobe Premiere Pro.xml");
        align_core::staged::write(&url, xml.as_bytes()).map_err(ExportError::Write)?;
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::PremiereXML,
            url,
        });
    }
    if !fcpxml_documents.is_empty() {
        let xml = align_core::export::fcpxml::combine_documents(&fcpxml_documents)
            .map_err(ExportError::Document)?;
        let url = directory.join("Align – Final Cut Pro.fcpxml");
        align_core::staged::write(&url, xml.as_bytes()).map_err(ExportError::Write)?;
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::FinalCutProXML,
            url,
        });
    }
    if !aaf_manifests.is_empty() {
        let manifest = serde_json::json!({"version": 3, "sequences": aaf_manifests});
        let url = directory.join("Align.aaf");
        write_aaf_manifest(&manifest, directory, &url, cancel)?;
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::Aaf,
            url,
        });
    }
    Ok(artifacts)
}

fn sequence_folder(index: usize, name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' => '_',
            other => other,
        })
        .take(80)
        .collect();
    let safe = safe.trim().trim_end_matches('.').trim();
    format!(
        "Sequence {:02} – {}",
        index + 1,
        if safe.is_empty() { "Untitled" } else { safe }
    )
}

pub fn export_prepared(
    request: ExportRequest<'_>,
    progress: Option<&mut dyn FnMut(ExportJobProgress)>,
) -> Result<Vec<ExportArtifact>, ExportError> {
    export_prepared_internal(request, progress, false).map(|(artifacts, _)| artifacts)
}

/// Prepares audio and writes every requested artifact. With `defer_aaf`, the
/// AAF manifest is returned for a batch to combine instead of being written.
fn export_prepared_internal(
    request: ExportRequest<'_>,
    progress: Option<&mut dyn FnMut(ExportJobProgress)>,
    defer_aaf: bool,
) -> Result<(Vec<ExportArtifact>, Option<serde_json::Value>), ExportError> {
    let ExportRequest {
        backend,
        timeline,
        directory,
        options,
        cancel,
    } = request;
    timeline.validate_audio_source_channels()?;
    if options.wants(TimelineExportFormat::Aaf) && options.include_replaced_sequence {
        return Err(ExportError::Unsupported(
            "AAF replaced sequence export is not implemented.",
        ));
    }
    std::fs::create_dir_all(directory).map_err(ExportError::Write)?;
    // Combine up front: the writer floors the combined island's starts, and
    // placement pads depend on those absolute starts (chronology shifts are
    // generally fractional). Planning on pre-combine starts would attach the
    // wrong prepend whenever the island minimum is not frame-exact.
    // Re-combining inside write_artifacts is a no-op shift afterwards.
    // Drift mappings are shift-invariant (relative source/island coords).
    let mut combined = timeline.combined();
    crate::media_assets::preserve_export(&mut combined, directory, cancel)
        .map_err(ExportError::write_or_cancelled)?;
    if options.wants(TimelineExportFormat::ResolveScript) {
        extend_final_precision_tail(&mut combined, options.correct_drift);
    }

    let plan = AudioPlan::new(&combined, options, directory, cancel)?;
    let media_dir = directory.join("Media Files");
    let media_plans = if options.include_media_files {
        media_file_plans(&plan.timeline, &media_dir)
    } else {
        Vec::new()
    };
    let mut progress = Progress {
        report: progress,
        completed: 0,
        total: plan.jobs.len() + media_plans.len(),
    };
    for job in &plan.jobs {
        check_cancel(cancel)?;
        progress.report(&job.media, ExportJobKind::Audio);
        job.render(backend, cancel)?;
        progress.complete(&job.media, ExportJobKind::Audio);
    }
    check_cancel(cancel)?;

    let prepared = &plan.timeline;
    let mut artifacts = write_artifacts(
        prepared,
        directory,
        &options.formats,
        options.include_replaced_sequence,
        options.fcpxml,
    )?;
    let mut deferred_aaf = None;
    if options.wants(TimelineExportFormat::Aaf) {
        let manifest = crate::aaf::timeline_manifest_with_frame_rate(
            prepared,
            options.aaf_frame_duration,
            cancel,
        )?;
        if defer_aaf {
            deferred_aaf = Some(manifest);
        } else {
            let url = directory.join("Align.aaf");
            write_aaf_manifest(&manifest, directory, &url, cancel)?;
            artifacts.push(ExportArtifact {
                format: ExportArtifactFormat::Aaf,
                url,
            });
        }
    }
    if options.include_media_files {
        std::fs::create_dir_all(&media_dir).map_err(ExportError::Write)?;
    }
    for plan in &media_plans {
        progress.report(&plan.video_url, ExportJobKind::Media);
        let file = export_media_file(backend, plan, cancel)?;
        progress.complete(&plan.video_url, ExportJobKind::Media);
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::MediaFile,
            url: file,
        });
    }
    Ok((artifacts, deferred_aaf))
}

/// Counts completed export jobs for an optional progress callback.
struct Progress<'a> {
    report: Option<&'a mut dyn FnMut(ExportJobProgress)>,
    completed: usize,
    total: usize,
}

impl Progress<'_> {
    fn report(&mut self, current: &Path, kind: ExportJobKind) {
        if let Some(report) = self.report.as_deref_mut() {
            report(ExportJobProgress {
                completed: self.completed,
                total: self.total,
                current: Some(current.to_path_buf()),
                kind,
            });
        }
    }

    fn complete(&mut self, current: &Path, kind: ExportJobKind) {
        self.completed += 1;
        self.report(current, kind);
    }
}

/// Matches durations computed from an item's own source and timeline times.
const SAME_END_TOLERANCE: f64 = 1e-9;
/// A timeline end closer than this to a frame boundary is on that boundary.
const FRAME_BOUNDARY_TOLERANCE: f64 = 1e-7;

/// Resolve floors the timeline end even when the final audio item extends
/// into the next partial frame. Extend only a final recorder's derived
/// precision stem with silence so an Entire Timeline render retains it.
fn extend_final_precision_tail(combined: &mut ExportTimeline, correct_drift: bool) {
    let fps = sequence_fps(combined.frame_duration);
    let end_for = |item: &ExportItem| {
        let duration = if correct_drift
            && needs_correction_points(&item.mapping_points)
            && item.clip.kind == MediaKind::Audio
        {
            item.corrected_selected_duration()
        } else {
            item.selected_timeline_duration(MediaKind::Audio)
        };
        item.timeline_start(MediaKind::Audio) + duration
    };
    let Some(island) = combined.islands.first_mut() else {
        return;
    };
    let maximum_end = island.clips.iter().map(end_for).fold(0.0, f64::max);
    let frame_end = maximum_end * fps;
    if (frame_end - frame_end.round()).abs() <= FRAME_BOUNDARY_TOLERANCE {
        return;
    }
    if let Some(last) = island.clips.iter_mut().find(|item| {
        item.clip.kind == MediaKind::Audio
            && !item.is_retimed(MediaKind::Audio)
            && (end_for(item) - maximum_end).abs() < SAME_END_TOLERANCE
    }) {
        let sr = last.clip.audio.first().map_or(48000.0, |a| a.sample_rate);
        // One extra sample covers nearest-sample placement rounding.
        last.precision_tail_samples =
            ((frame_end.ceil() / fps - maximum_end) * sr).ceil() as u64 + 1;
    }
}

// ------------------------------------------------------------ audio plan

/// Version of the prepared-audio formats, part of every sidecar's identity:
/// a change to how sidecars are rendered never reuses an older file.
const AUDIO_SIDECAR_REVISION: u32 = 14;

/// `"<media> – <label> – <identity hash>.wav"`. The readable part names the
/// file in an editor's bin; the hash keeps every rendering input, so a file
/// with this name can be reused as is.
fn sidecar_name(media: &Path, label: &str, identity: &[&str]) -> String {
    let stem = media
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("clip");
    let mut hash = blake3::Hasher::new();
    hash.update(&AUDIO_SIDECAR_REVISION.to_le_bytes());
    for part in identity {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{stem} – {label} – {}.wav", &hash.finalize().to_hex()[..16])
}

/// Every audio file one export renders, decided before rendering starts.
struct AudioPlan {
    /// The export timeline with each planned file attached to its items.
    timeline: ExportTimeline,
    /// Unique renders in timeline order.
    jobs: Vec<AudioJob>,
}

struct AudioJob {
    /// The timeline media this file is prepared from.
    media: PathBuf,
    kind: AudioJobKind,
}

enum AudioJobKind {
    Drift {
        destination: PathBuf,
        points: Vec<(f64, f64)>,
    },
    Pad {
        source: PathBuf,
        destination: PathBuf,
        samples: u64,
        selection: (u64, u64),
    },
    Stems {
        source: PathBuf,
        outputs: Vec<PathBuf>,
        bit_depth: Option<u32>,
        is_float: bool,
        source_start: f64,
        duration: f64,
        tail_samples: u64,
    },
}

impl AudioPlan {
    fn new(
        combined: &ExportTimeline,
        options: &ExportOptions,
        directory: &Path,
        cancel: &AtomicBool,
    ) -> Result<Self, ExportError> {
        let wants_aaf = options.wants(TimelineExportFormat::Aaf);
        let wants_precision = wants_aaf
            || options.wants(TimelineExportFormat::ResolveScript)
            || options.wants(TimelineExportFormat::FinalCutProXML);
        // Placement pads are a PremiereXML-only concern: no other writer reads
        // them (OTIO/FCPXML place exactly already; the Resolve script locates
        // stems through its own fractional API, so its stems stay unpadded).
        let wants_pad = options.wants(TimelineExportFormat::PremiereXML);
        let fps = sequence_fps(combined.frame_duration);

        let needs_drift = |item: &ExportItem| {
            options.correct_drift
                && item.clip.kind == MediaKind::Audio
                && needs_correction_points(&item.mapping_points)
        };
        let pad = |item: &ExportItem| {
            if wants_pad {
                placement_pad(item, fps)
            } else {
                None
            }
        };
        let needs_precision = |item: &ExportItem| {
            let channels = item.clip.audio.first().map_or(0, |a| a.channels);
            wants_precision
                && (item.clip.kind == MediaKind::Audio
                    || (wants_aaf
                        && !item.clip.audio.is_empty()
                        && item.is_enabled(MediaKind::Audio)))
                && (wants_aaf
                    || channels > 1
                    || !item.is_full_source_selection()
                    || item.precision_tail_samples > 0)
        };

        // Every item of a corrected clip plays its drift sidecar, and every
        // item of a precision asset its stems.
        let items = || combined.islands.iter().flat_map(|island| &island.clips);
        let corrected_clips: HashSet<&str> = items()
            .filter(|item| needs_drift(item))
            .map(|item| item.clip.id.0.as_str())
            .collect();
        let precision_assets: HashSet<String> = items()
            .filter(|item| needs_precision(item))
            .map(ExportItem::precision_asset_key)
            .collect();

        // Stamp each source once. Unix inode + ctime detect replacements and
        // same-size rewrites even with a restored mtime, without reading video.
        let mut source_tags = HashMap::new();
        for item in items() {
            let needs_file = corrected_clips.contains(item.clip.id.0.as_str())
                || pad(item).is_some()
                || precision_assets.contains(&item.precision_asset_key());
            if needs_file && !source_tags.contains_key(&item.clip.url) {
                source_tags.insert(
                    item.clip.url.clone(),
                    source_revision_tag(&item.clip.url, cancel)?,
                );
            }
        }

        let audio_dir = directory.join("Corrected Audio");
        let precision_dir = directory.join("Resolve Precision Audio");
        let mut jobs = Vec::new();
        let mut planned_pads = HashSet::new();
        let mut drift_files: HashMap<&str, PathBuf> = HashMap::new();
        let mut precision_files: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut islands = Vec::with_capacity(combined.islands.len());
        for island in &combined.islands {
            let mut clips = Vec::with_capacity(island.clips.len());
            for item in &island.clips {
                let media = &item.clip.url;
                let id = item.clip.id.0.as_str();
                let mut prepared = item.clone();

                if corrected_clips.contains(id) {
                    let destination = drift_files.entry(id).or_insert_with(|| {
                        let ppm = ((item.mapping_rate - 1.0) * 1_000_000.0).round() as i64;
                        let correction = if item.mapping_points.len() > 2 {
                            "piecewise warp".to_string()
                        } else {
                            format!("drift {ppm:+}ppm")
                        };
                        let destination = audio_dir.join(sidecar_name(
                            media,
                            &correction,
                            &["drift", id, &item.mapping_digest(), &source_tags[media]],
                        ));
                        jobs.push(AudioJob {
                            media: media.clone(),
                            kind: AudioJobKind::Drift {
                                destination: destination.clone(),
                                points: item
                                    .mapping_points
                                    .iter()
                                    .map(|p| (p.source.as_seconds(), p.island.as_seconds()))
                                    .collect(),
                            },
                        });
                        destination
                    });
                    prepared = prepared.with_corrected_audio(destination.clone());
                }

                if let Some((samples, sample_rate)) = pad(item) {
                    // Pad the drift sidecar when one exists, else the
                    // original: precision stems keep using the unpadded
                    // source, so the script workflow is unaffected.
                    let corrected = prepared.corrected_audio_url.clone();
                    let mapping = if corrected.is_some() {
                        item.mapping_digest()
                    } else {
                        "direct".to_string()
                    };
                    let (start, end) = if corrected.is_some() {
                        (item.corrected_source_in(), item.corrected_source_out())
                    } else {
                        (
                            item.selected_source_in(MediaKind::Audio),
                            item.selected_source_out(MediaKind::Audio),
                        )
                    };
                    let destination = audio_dir.join(sidecar_name(
                        media,
                        &format!("pad {samples}"),
                        &[
                            "pad",
                            id,
                            &mapping,
                            &item
                                .selected_source_in(MediaKind::Audio)
                                .to_bits()
                                .to_string(),
                            &item
                                .selected_source_out(MediaKind::Audio)
                                .to_bits()
                                .to_string(),
                            &source_tags[media],
                            &samples.to_string(),
                        ],
                    ));
                    if planned_pads.insert(destination.clone()) {
                        jobs.push(AudioJob {
                            media: media.clone(),
                            kind: AudioJobKind::Pad {
                                source: corrected.unwrap_or_else(|| media.clone()),
                                destination: destination.clone(),
                                samples,
                                selection: (
                                    (start * sample_rate).round() as u64,
                                    (end * sample_rate).round() as u64,
                                ),
                            },
                        });
                    }
                    prepared =
                        prepared.with_placement_pad(destination, samples as f64 / sample_rate);
                }

                let key = item.precision_asset_key();
                if precision_assets.contains(&key) {
                    let outputs = match precision_files.get(&key) {
                        Some(outputs) => outputs.clone(),
                        None => {
                            let corrected = prepared.corrected_audio_url.clone();
                            let audio = item.clip.audio.first();
                            // Drift sidecars are always 32-bit float.
                            let (bit_depth, is_float) = match corrected {
                                Some(_) => (Some(32), true),
                                None => (
                                    audio.and_then(|a| a.bit_depth),
                                    audio.and_then(|a| a.is_float).unwrap_or(false),
                                ),
                            };
                            let origin = if corrected.is_some() {
                                "corrected"
                            } else {
                                "original"
                            };
                            let digest = prepared.precision_digest();
                            let outputs: Vec<_> = (0..audio.map_or(0, |a| a.channels))
                                .map(|channel| {
                                    precision_dir.join(sidecar_name(
                                        media,
                                        &format!("channel {}", channel + 1),
                                        &[
                                            "channel",
                                            &channel.to_string(),
                                            id,
                                            &digest,
                                            &source_tags[media],
                                            origin,
                                        ],
                                    ))
                                })
                                .collect();
                            jobs.push(AudioJob {
                                media: media.clone(),
                                kind: AudioJobKind::Stems {
                                    source: corrected.unwrap_or_else(|| media.clone()),
                                    outputs: outputs.clone(),
                                    bit_depth,
                                    is_float,
                                    source_start: prepared.precision_source_start(),
                                    duration: prepared.precision_source_duration(),
                                    tail_samples: prepared.precision_tail_samples,
                                },
                            });
                            precision_files.insert(key, outputs.clone());
                            outputs
                        }
                    };
                    prepared = prepared.with_precision_audio(outputs);
                }
                clips.push(prepared);
            }
            islands.push(ExportIsland {
                id: island.id,
                clips,
                duration: island.duration,
            });
        }
        Ok(Self {
            timeline: combined.with_islands(islands),
            jobs,
        })
    }
}

impl AudioJob {
    /// Renders the files this job still lacks; existing files are reused.
    fn render(&self, backend: &dyn MediaBackend, cancel: &AtomicBool) -> Result<(), ExportError> {
        let create_parent = |path: &Path| match path.parent() {
            Some(parent) => std::fs::create_dir_all(parent).map_err(ExportError::Write),
            None => Ok(()),
        };
        match &self.kind {
            AudioJobKind::Drift {
                destination,
                points,
            } => {
                if destination.is_file() {
                    return Ok(());
                }
                create_parent(destination)?;
                render_drift_cancellable(backend, &self.media, destination, points, cancel)
                    .map_err(|e| ExportError::audio(&self.media, "drift-corrected", e))
            }
            AudioJobKind::Pad {
                source,
                destination,
                samples,
                selection,
            } => {
                if destination.is_file() {
                    return Ok(());
                }
                create_parent(destination)?;
                render_pad_cancellable(
                    backend,
                    source,
                    destination,
                    *samples,
                    Some(*selection),
                    cancel,
                )
                .map_err(|e| ExportError::audio(&self.media, "sample-accurate Premiere", e))
            }
            AudioJobKind::Stems {
                source,
                outputs,
                bit_depth,
                is_float,
                source_start,
                duration,
                tail_samples,
            } => {
                let missing: Vec<_> = outputs
                    .iter()
                    .enumerate()
                    .filter(|(_, path)| !path.is_file())
                    .map(|(channel, path)| (channel, path.as_path()))
                    .collect();
                if let Some((_, first)) = missing.first() {
                    create_parent(first)?;
                }
                render_channels_cancellable(
                    backend,
                    source,
                    &ChannelStems {
                        outputs: &missing,
                        bit_depth: *bit_depth,
                        is_float: *is_float,
                        source_start: *source_start,
                        duration: *duration,
                        tail_samples: *tail_samples,
                    },
                    cancel,
                )
                .map_err(|e| ExportError::audio(&self.media, "channel-stem", e))
            }
        }
    }
}

/// Samples of silence (and their rate) that land a fractional audio start
/// sample-accurately in Premiere, when one is needed.
///
/// Skipped for retimed audio (time-remap graphs own subframe truth there),
/// items without audio, and non-positive sample rates. Only audio clipitems
/// reference the padded file; video sides are untouched (frame-grid
/// limitation).
fn placement_pad(item: &ExportItem, sequence_fps: f64) -> Option<(u64, f64)> {
    let audio = item.clip.audio.first()?;
    if audio.sample_rate <= 0.0 || audio.channels == 0 || item.is_retimed(MediaKind::Audio) {
        return None;
    }
    let samples = placement_pad_samples(
        item.timeline_start(MediaKind::Audio),
        sequence_fps,
        audio.sample_rate,
    )?;
    Some((samples, audio.sample_rate))
}

fn write_aaf_manifest(
    manifest: &serde_json::Value,
    directory: &Path,
    url: &Path,
    cancel: &AtomicBool,
) -> Result<(), ExportError> {
    let bytes = serde_json::to_vec(manifest).map_err(|e| ExportError::Document(e.to_string()))?;
    // Removed when dropped, after the AAF module has read it.
    let manifest_path = tempfile::Builder::new()
        .prefix(".align-aaf-")
        .suffix(".json")
        .tempfile_in(directory)
        .map_err(ExportError::Write)?
        .into_temp_path();
    std::fs::write(&manifest_path, bytes).map_err(ExportError::Write)?;
    crate::aaf::write_audio(&manifest_path, url, cancel)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct MediaFilePlan {
    video_url: PathBuf,
    recorder_url: PathBuf,
    recorder_source_in: f64,
    leading_silence: f64,
    recorder_duration: f64,
    video_duration: f64,
    url: PathBuf,
}

fn source_revision_tag(path: &Path, cancel: &AtomicBool) -> Result<String, ExportError> {
    check_cancel(cancel)?;
    #[cfg(unix)]
    {
        let revision = align_core::cache::media_revision(path).map_err(ExportError::read(path))?;
        Ok(blake3::hash(revision.as_bytes()).to_hex()[..32].to_string())
    }
    // Without an inode/change-time stamp, retain strict content invalidation.
    #[cfg(not(unix))]
    source_content_tag(path, cancel)
}

#[cfg(not(unix))]
fn source_content_tag(path: &Path, cancel: &AtomicBool) -> Result<String, ExportError> {
    let mut file = std::fs::File::open(path).map_err(ExportError::read(path))?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = file.read(&mut buffer).map_err(ExportError::read(path))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().to_hex()[..32].to_string())
}

/// Keep each complete camera file and replace its scratch audio with the
/// first (uppermost) overlapping recorder clip. Shorter recorder clips are
/// padded with silence instead of shortening the camera file.
fn media_file_plans(timeline: &ExportTimeline, directory: &Path) -> Vec<MediaFilePlan> {
    let combined = timeline.combined_island(1.0);
    let mut planned_video = HashSet::new();
    let mut plans = Vec::new();
    for video in combined
        .clips
        .iter()
        .filter(|item| item.clip.video.is_some())
    {
        if video.is_retimed(align_core::MediaKind::Video)
            || !planned_video.insert(video.clip.id.0.clone())
        {
            continue;
        }
        let video_duration = video.source_duration();
        let video_start = video.start - video.source_in;
        let video_end = video_start + video_duration;
        let recorder = combined
            .clips
            .iter()
            .filter(|item| item.clip.kind == align_core::MediaKind::Audio)
            .filter(|item| !item.is_retimed(align_core::MediaKind::Audio))
            .find(|recorder| {
                let start = recorder.start - recorder.source_in;
                let end = start + recorder.source_duration();
                video_start < end && start < video_end
            });
        let Some(recorder) = recorder else {
            continue;
        };
        let (recorder_url, recorder_start, recorder_end) =
            match recorder.corrected_audio_url.clone() {
                Some(corrected) => {
                    let start = recorder.start - recorder.corrected_source_in();
                    (corrected, start, start + recorder.mapped_duration())
                }
                None => {
                    let start = recorder.start - recorder.source_in;
                    (
                        recorder.clip.url.clone(),
                        start,
                        start + recorder.source_duration(),
                    )
                }
            };
        let overlap_start = video_start.max(recorder_start);
        let overlap_end = video_end.min(recorder_end);
        if overlap_end <= overlap_start {
            continue;
        }
        let stem = video
            .clip
            .url
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("clip");
        let suffix: String = video.clip.id.0.chars().take(8).collect();
        plans.push(MediaFilePlan {
            video_url: video.clip.url.clone(),
            recorder_url,
            recorder_source_in: overlap_start - recorder_start,
            leading_silence: overlap_start - video_start,
            recorder_duration: overlap_end - overlap_start,
            video_duration,
            url: directory.join(format!("{stem} – clean audio – {suffix}.mov")),
        });
    }
    plans
}

/// Remux one complete camera file without re-encoding its video stream. The
/// external track is trimmed sample-accurately, padded to the camera duration,
/// and encoded as uncompressed 32-bit float at the NLE-native 48 kHz rate.
/// QuickTime/MP4 cameras are copied natively; other containers use FFmpeg.
fn export_media_file(
    backend: &dyn MediaBackend,
    plan: &MediaFilePlan,
    cancel: &AtomicBool,
) -> Result<PathBuf, ExportError> {
    check_cancel(cancel)?;
    let staged = align_core::staged::StagedFile::new(&plan.url).map_err(ExportError::Write)?;
    let audio = crate::remux::ReplacementAudio {
        backend,
        source: &plan.recorder_url,
        source_in: plan.recorder_source_in,
        duration: plan.recorder_duration,
        leading_silence: plan.leading_silence,
        total: plan.video_duration,
    };
    match crate::remux::replace_audio(&plan.video_url, staged.path(), &audio, cancel) {
        Ok(()) => {
            staged.commit().map_err(ExportError::Write)?;
            return Ok(plan.url.clone());
        }
        Err(crate::remux::RemuxError::Unsupported(_)) => {}
        Err(crate::remux::RemuxError::Render(error)) => {
            return Err(ExportError::audio(
                &plan.recorder_url,
                "replacement camera",
                error,
            ));
        }
        Err(other) => return Err(ExportError::media(&plan.video_url, other)),
    }
    let ffmpeg = crate::ff::ffmpeg_bin()
        .ok_or_else(|| ExportError::media(&plan.video_url, "FFmpeg was not found."))?;
    // FFmpeg decodes the recorder here, so trim the decoder delay its output
    // keeps (AC-3 in MPEG-TS), as the backends do.
    let decoder_delay = crate::ff::inspect(&plan.recorder_url)
        .map_err(|error| ExportError::media(&plan.recorder_url, error))?
        .audio_streams
        .first()
        .map_or(0.0, |stream| stream.decoder_delay);
    let delay_samples = (plan.leading_silence.max(0.0) * 48_000.0).round() as u64;
    let filter = format!(
        "[1:a:0]atrim=duration={:.9},asetpts=PTS-STARTPTS,aresample=48000,adelay={delay_samples}S:all=1,apad=whole_dur={:.9},atrim=duration={:.9}[clean]",
        plan.recorder_duration, plan.video_duration, plan.video_duration
    );
    let mut child = crate::process::command(ffmpeg)
        .args(["-y", "-v", "error", "-i"])
        .arg(&plan.video_url)
        .args([
            "-ss",
            &format!("{:.9}", plan.recorder_source_in + decoder_delay),
            "-i",
        ])
        .arg(&plan.recorder_url)
        .args(["-filter_complex", &filter])
        .args(["-map", "0:v:0", "-map", "[clean]"])
        .args(["-map_metadata", "0", "-c:v", "copy", "-c:a", "pcm_f32le"])
        .args(["-t", &format!("{:.9}", plan.video_duration)])
        .arg(staged.path())
        .spawn()
        .map_err(|error| ExportError::media(&plan.video_url, error))?;
    let status = crate::process::wait_cancellable(&mut child, cancel)?;
    if !status.success() {
        return Err(ExportError::media(
            &plan.video_url,
            format!("FFmpeg failed ({status})."),
        ));
    }
    staged.commit().map_err(ExportError::Write)?;
    Ok(plan.url.clone())
}

/// Writes the timeline documents. AAF is written separately, from the
/// prepared audio manifest.
fn write_artifacts(
    timeline: &ExportTimeline,
    directory: &Path,
    formats: &[TimelineExportFormat],
    include_replaced_sequence: bool,
    fcpxml: FcpxmlOptions,
) -> Result<Vec<ExportArtifact>, ExportError> {
    let combined = timeline.combined();
    let mut artifacts = Vec::with_capacity(formats.len());
    for &format in formats {
        let application = match format {
            TimelineExportFormat::Aaf => continue,
            TimelineExportFormat::PremiereXML => "Adobe Premiere Pro",
            TimelineExportFormat::FinalCutProXML => "Final Cut Pro",
            TimelineExportFormat::ResolveOTIO
            | TimelineExportFormat::ResolveScript
            | TimelineExportFormat::ResolveXML => "DaVinci Resolve",
        };
        let url = directory.join(format!("Align – {application}.{}", format.file_extension()));
        let bytes: Vec<u8> = match format {
            TimelineExportFormat::Aaf => continue,
            TimelineExportFormat::ResolveOTIO => {
                align_core::export::otio::data(&combined).map_err(ExportError::Document)?
            }
            TimelineExportFormat::ResolveScript => {
                align_core::export::script::write().as_bytes().to_vec()
            }
            TimelineExportFormat::ResolveXML | TimelineExportFormat::PremiereXML => {
                align_core::export::premiere::write(&combined, format, include_replaced_sequence)
                    .into_bytes()
            }
            TimelineExportFormat::FinalCutProXML => {
                align_core::export::fcpxml::write(&combined, fcpxml).into_bytes()
            }
        };
        align_core::staged::write(&url, &bytes).map_err(ExportError::Write)?;
        #[cfg(unix)]
        if format == TimelineExportFormat::ResolveScript {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&url, std::fs::Permissions::from_mode(0o755));
        }
        artifacts.push(ExportArtifact {
            format: format.into(),
            url,
        });
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use align_core::{
        AudioSummary, Clip, ClipId, MediaKind, MediaTime, VideoFrameRateMode, VideoSummary,
    };

    #[cfg(unix)]
    #[test]
    fn source_revision_invalidates_restored_mtime_and_honors_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.wav");
        std::fs::write(&path, b"original").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let before = source_revision_tag(&path, &cancel).unwrap();
        assert_eq!(source_revision_tag(&path, &cancel).unwrap(), before);
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"replaced").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(source_revision_tag(&path, &cancel).unwrap(), before);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            source_revision_tag(&path, &cancel),
            Err(ExportError::Cancelled)
        ));
    }

    fn item(id: &str, kind: MediaKind, start: f64, duration: f64) -> ExportItem {
        let audio = AudioSummary {
            sample_rate: 48_000.0,
            channels: 2,
            bit_depth: Some(24),
            is_float: Some(false),
            source_timecode: None,
        };
        let clip = Clip {
            id: ClipId::new(id),
            url: PathBuf::from(format!(
                "/{id}.{}",
                if kind == MediaKind::Video {
                    "mov"
                } else {
                    "wav"
                }
            )),
            kind,
            duration: MediaTime::seconds(duration),
            audio: vec![audio],
            video: (kind == MediaKind::Video).then_some(VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: Some(VideoFrameRateMode::Constant),
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
                align_core::MappingPoint {
                    source: MediaTime::seconds(0.0),
                    island: MediaTime::seconds(start),
                },
                align_core::MappingPoint {
                    source: MediaTime::seconds(duration),
                    island: MediaTime::seconds(start + duration),
                },
            ],
            1.0,
        )
    }

    #[test]
    fn premiere_export_renders_placement_pads_for_fractional_starts() {
        if crate::ff::test_ffmpeg().is_none() {
            eprintln!("SKIP: ffmpeg is unavailable");
            return;
        }
        let dir = std::env::temp_dir().join(format!("align-padprep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 4 s stereo source, real file the pad renderer can read.
        let src = dir.join("take.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for i in 0..48_000 * 4 {
            let v = if i == 4800 { 16000 } else { 0 };
            w.write_sample(v as i16).unwrap();
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();

        let mut frac = item("frac", MediaKind::Audio, 5.084, 4.0);
        frac.clip.url = src.clone();
        frac.clip.duration = MediaTime::seconds(4.0);
        // Island minimum normalizes to timeline zero at combine time, so the
        // exact item anchors the island and the fractional offset survives
        // the shift the writer also sees.
        let mut exact = item("exact", MediaKind::Audio, 5.0, 4.0);
        exact.clip.url = src.clone();
        exact.clip.duration = MediaTime::seconds(4.0);
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips: vec![frac, exact],
                duration: 10.0,
            }],
            MediaTime::new(1, 25),
            "padtest",
        );
        let out = dir.join("out");
        let run = || {
            export_prepared(
                ExportRequest {
                    backend: &crate::portable::PortableBackend,
                    timeline: &timeline,
                    directory: &out,
                    options: &ExportOptions {
                        formats: vec![TimelineExportFormat::PremiereXML],
                        correct_drift: false,
                        ..ExportOptions::default()
                    },
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                None,
            )
            .expect("export")
        };
        let first = run();
        let xml_url = first
            .iter()
            .find(|a| a.format == ExportArtifactFormat::PremiereXML)
            .expect("premiere artifact")
            .url
            .clone();
        let pads: Vec<PathBuf> = std::fs::read_dir(out.join("Corrected Audio"))
            .expect("pad dir")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(pads.len(), 1, "only the fractional start needs a file");
        assert!(
            pads[0]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("pad 192"),
            "unexpected pad name: {}",
            pads[0].display()
        );
        let xml = std::fs::read_to_string(&xml_url).expect("read xml");
        assert!(xml.contains("pad 192"), "xml must reference the sidecar");
        assert!(
            !xml.contains("subframeoffset"),
            "padded export carries no subframeoffset"
        );
        // Re-export resolves the identical sidecar without re-rendering.
        let mtime = std::fs::metadata(&pads[0]).unwrap().modified().unwrap();
        let second = run();
        assert_eq!(first.len(), second.len());
        assert_eq!(
            std::fs::metadata(&pads[0]).unwrap().modified().unwrap(),
            mtime,
            "existing sidecar must not be rewritten"
        );
        // Same-size replacement at the same URL must never reuse old essence.
        let mut bytes = std::fs::read(&src).unwrap();
        let marker = bytes.windows(4).position(|w| w == b"data").unwrap() + 8 + 4800 * 4;
        bytes[marker..marker + 2].copy_from_slice(&8000i16.to_le_bytes());
        std::fs::write(&src, bytes).unwrap();
        run();
        let fresh_xml = std::fs::read_to_string(&xml_url).unwrap();
        assert_ne!(fresh_xml, xml);
        let fresh_pad = std::fs::read_dir(out.join("Corrected Audio"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p != &pads[0])
            .unwrap();
        let samples: Vec<f32> = hound::WavReader::open(fresh_pad)
            .unwrap()
            .into_samples::<f32>()
            .map(Result::unwrap)
            .collect();
        assert!((samples[(192 + 4800) * 2] - 8000.0 / 32768.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn premiere_export_keeps_a_camera_with_a_shorter_audio_track() {
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            return;
        };
        // 1.9 s of audio in a 2.04 s camera file, with clicks at 0.1 s and
        // on the last audio sample.
        const AUDIO_FRAMES: usize = 91_200;
        const FIRST_CLICK: usize = 4_800;
        const LAST_CLICK: usize = AUDIO_FRAMES - 1;
        // The clip starts 0.05 s into the file, at 5.084 s on a 25 fps
        // timeline: frame 127 (5.08 s) plus a 4 ms (192-sample) prepend.
        const SOURCE_IN: usize = 2_400;
        const PAD: usize = 192;
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("source.wav");
        let camera = dir.path().join("camera.mov");
        let mut input = hound::WavWriter::create(
            &audio,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..AUDIO_FRAMES {
            let value = if frame == FIRST_CLICK || frame == LAST_CLICK {
                16000i16
            } else {
                0
            };
            input.write_sample(value).unwrap();
            input.write_sample(-value).unwrap();
        }
        input.finalize().unwrap();
        assert!(
            std::process::Command::new(ffmpeg)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg("color=c=black:s=160x90:r=25:d=2.04")
                .arg("-i")
                .arg(&audio)
                .args([
                    "-c:v",
                    "mpeg4",
                    "-c:a",
                    "pcm_s16le",
                    "-movflags",
                    "+faststart"
                ])
                .arg(&camera)
                .status()
                .unwrap()
                .success()
        );
        let original = std::fs::read(&camera).unwrap();
        for kind in [
            crate::backend::BackendKind::Portable,
            #[cfg(target_os = "macos")]
            crate::backend::BackendKind::AppleNative,
        ] {
            let backend = crate::backend::create(kind);
            for end in [2.04, 2.0] {
                let mut fractional = item("fractional", MediaKind::Video, 5.084, 2.04);
                fractional.clip.url = camera.clone();
                fractional.clip.audio[0].bit_depth = Some(16);
                fractional.source_in = 0.05;
                fractional.source_out = end;
                fractional.timeline_duration = end - 0.05;
                let mut anchor = item("anchor", MediaKind::Video, 5.0, 2.04);
                anchor.clip.url = camera.clone();
                let timeline = ExportTimeline::new(
                    vec![ExportIsland {
                        id: 0,
                        clips: vec![anchor, fractional],
                        duration: 2.2,
                    }],
                    MediaTime::new(1, 25),
                    "camera tail",
                );
                let out = dir.path().join(format!("{kind:?}-{end}"));
                let run = || {
                    export_prepared(
                        ExportRequest {
                            backend: backend.as_ref(),
                            timeline: &timeline,
                            directory: &out,
                            options: &ExportOptions {
                                formats: vec![TimelineExportFormat::PremiereXML],
                                correct_drift: false,
                                ..ExportOptions::default()
                            },
                            cancel: &std::sync::atomic::AtomicBool::new(false),
                        },
                        None,
                    )
                };
                let artifacts = run().expect("a valid camera audio extent must export");
                let pads: Vec<_> = std::fs::read_dir(out.join("Corrected Audio"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                assert_eq!(pads.len(), 1);
                let reader = hound::WavReader::open(&pads[0]).unwrap();
                assert_eq!(
                    reader.duration(),
                    (PAD + ((end - 0.05) * 48000.0).round() as usize) as u32
                );
                let samples: Vec<f32> = reader.into_samples().map(Result::unwrap).collect();
                let first = PAD + FIRST_CLICK - SOURCE_IN;
                let last = PAD + LAST_CLICK - SOURCE_IN;
                assert_eq!(
                    &samples[first * 2..first * 2 + 2],
                    &[16000.0 / 32768.0, -16000.0 / 32768.0]
                );
                assert_eq!(
                    &samples[last * 2..last * 2 + 2],
                    &[16000.0 / 32768.0, -16000.0 / 32768.0]
                );
                assert!(samples[..PAD * 2].iter().all(|sample| *sample == 0.0));
                assert!(
                    samples[(last + 1) * 2..]
                        .iter()
                        .all(|sample| *sample == 0.0)
                );
                assert_eq!(std::fs::read(&camera).unwrap(), original);
                let xml = &artifacts[0].url;
                let previous_xml = std::fs::read(xml).unwrap();
                let previous_pad = std::fs::read(&pads[0]).unwrap();
                // The header precedes the media data (faststart): dropping the
                // file's last 91,200 bytes removes audio the track declares.
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&camera)
                    .unwrap()
                    .set_len((original.len() - 91_200) as u64)
                    .unwrap();
                assert!(
                    backend.inspect(&camera).is_ok(),
                    "metadata remains readable"
                );
                let error =
                    run().expect_err("missing media inside the declared audio track must fail");
                assert!(error.to_string().contains("camera.mov"), "{error}");
                assert!(
                    error.to_string().contains("sample-accurate Premiere"),
                    "{error}"
                );
                assert_eq!(std::fs::read(xml).unwrap(), previous_xml);
                assert_eq!(std::fs::read(&pads[0]).unwrap(), previous_pad);
                assert_eq!(
                    std::fs::read_dir(out.join("Corrected Audio"))
                        .unwrap()
                        .count(),
                    1
                );
                std::fs::write(&camera, &original).unwrap();
            }
        }
    }

    #[test]
    fn batch_export_combines_premiere_and_keeps_other_files_per_sequence() {
        let dir = std::env::temp_dir().join(format!("align-multi-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let timeline = |name: &str, id: &str| {
            ExportTimeline::new(
                vec![ExportIsland {
                    id: 0,
                    clips: vec![item(id, MediaKind::Video, 0.0, 1.0)],
                    duration: 1.0,
                }],
                MediaTime::new(1, 25),
                name,
            )
        };
        let timelines = [
            timeline("First/cut", "first"),
            timeline("Second cut", "second"),
        ];
        let artifacts = export_prepared_many(
            ExportBatchRequest {
                backend: &crate::portable::PortableBackend,
                timelines: &timelines,
                directory: &dir,
                options: &ExportOptions {
                    formats: vec![
                        TimelineExportFormat::PremiereXML,
                        TimelineExportFormat::FinalCutProXML,
                        TimelineExportFormat::ResolveOTIO,
                    ],
                    correct_drift: false,
                    ..ExportOptions::default()
                },
                cancel: &std::sync::atomic::AtomicBool::new(false),
            },
            None,
        )
        .expect("batch export");

        let premiere = artifacts
            .iter()
            .find(|artifact| artifact.format == ExportArtifactFormat::PremiereXML)
            .expect("combined Premiere project");
        let xml = std::fs::read_to_string(&premiere.url).unwrap();
        assert_eq!(xml.matches("<sequence id=").count(), 2);
        assert!(xml.contains("<name>First/cut</name>"));
        assert!(xml.contains("<name>Second cut</name>"));
        let fcpxml = artifacts
            .iter()
            .find(|artifact| artifact.format == ExportArtifactFormat::FinalCutProXML)
            .expect("combined Final Cut project");
        let summaries = align_core::timeline_sequence_summaries(&fcpxml.url).unwrap();
        assert_eq!(summaries.len(), 4, "synced + multicam per source sequence");
        assert_eq!(
            artifacts
                .iter()
                .filter(|artifact| artifact.format == ExportArtifactFormat::ResolveOTIO)
                .count(),
            2
        );
        assert!(dir.join("Sequence 01 – First_cut").is_dir());
        assert!(dir.join("Sequence 02 – Second cut").is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_precision_stems_keep_selected_tail_and_reuse_completed_channels() {
        let dir = std::env::temp_dir().join(format!("align-resolve-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.wav");
        let mut w = hound::WavWriter::create(
            &src,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for i in 0..4800 {
            let value = if i == 3479 || i == 3500 { 16000i16 } else { 0 };
            w.write_sample(value).unwrap();
            w.write_sample(-value).unwrap();
        }
        w.finalize().unwrap();
        let original = std::fs::read(&src).unwrap();
        let mut clip = item("tail", MediaKind::Audio, 0.0, 0.1);
        clip.clip.url = src.clone();
        clip.clip.audio[0].channels = 2;
        clip.clip.audio[0].bit_depth = Some(16);
        clip.source_in = 0.01;
        clip.source_out = 0.0725;
        clip.timeline_duration = 0.0625;
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips: vec![clip],
                duration: 0.0625,
            }],
            MediaTime::new(1, 25),
            "tail",
        );
        let out = dir.join("out");
        let run_export = || {
            export_prepared(
                ExportRequest {
                    backend: &crate::portable::PortableBackend,
                    timeline: &timeline,
                    directory: &out,
                    options: &ExportOptions {
                        formats: vec![
                            TimelineExportFormat::ResolveScript,
                            TimelineExportFormat::ResolveOTIO,
                        ],
                        correct_drift: false,
                        ..ExportOptions::default()
                    },
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                None,
            )
            .unwrap()
        };
        run_export();
        let doc: serde_json::Value = serde_json::from_slice(
            &std::fs::read(out.join("Align – DaVinci Resolve.otio")).unwrap(),
        )
        .unwrap();
        let audio = doc["tracks"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "Audio")
            .unwrap();
        let c = &audio["children"][0];
        let reference = &c["media_references"]["DEFAULT_MEDIA"];
        let reader = hound::WavReader::open(reference["target_url"].as_str().unwrap()).unwrap();
        let samples: Vec<f32> = reader.into_samples::<f32>().map(Result::unwrap).collect();
        assert!((3841..=3842).contains(&samples.len()));
        assert!(samples[2999] > 0.4);
        assert!(samples[3000..].iter().all(|v| *v == 0.0));
        assert_eq!(samples.iter().filter(|v| **v != 0.0).count(), 1);
        let duration = c["source_range"]["duration"]["value"].as_f64().unwrap() / 25.0;
        assert!((0.08..=0.08 + 2.0 / 48000.0).contains(&duration));
        let mut stems: Vec<_> = std::fs::read_dir(out.join("Resolve Precision Audio"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        stems.sort();
        assert_eq!(stems.len(), 2);
        let expected: Vec<_> = stems
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stems[0])
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(946684800))
            .unwrap();
        let modified = std::fs::metadata(&stems[0]).unwrap().modified().unwrap();
        std::fs::remove_file(&stems[1]).unwrap();
        run_export();
        assert_eq!(
            std::fs::metadata(&stems[0]).unwrap().modified().unwrap(),
            modified
        );
        for (path, bytes) in stems.iter().zip(&expected) {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(std::fs::read(src).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn media_plan_keeps_full_video_and_uses_uppermost_audio() {
        let clips = vec![
            item("camera", MediaKind::Video, 10.0, 10.0),
            item("upper", MediaKind::Audio, 12.0, 6.0),
            item("lower", MediaKind::Audio, 10.0, 10.0),
        ];
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips,
                duration: 20.0,
            }],
            MediaTime::new(1, 25),
            "test",
        );

        let plans = media_file_plans(&timeline, Path::new("/out"));

        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.video_url, PathBuf::from("/camera.mov"));
        assert_eq!(plan.recorder_url, PathBuf::from("/upper.wav"));
        assert!((plan.video_duration - 10.0).abs() < 1e-9);
        assert!((plan.leading_silence - 2.0).abs() < 1e-9);
        assert!((plan.recorder_duration - 6.0).abs() < 1e-9);
    }

    #[test]
    fn media_plan_emits_one_file_per_camera_source() {
        let mut second_edit = item("camera", MediaKind::Video, 20.0, 10.0);
        second_edit.instance_id = "camera-second-edit".into();
        let clips = vec![
            item("camera", MediaKind::Video, 0.0, 10.0),
            item("audio", MediaKind::Audio, 0.0, 30.0),
            second_edit,
        ];
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips,
                duration: 30.0,
            }],
            MediaTime::new(1, 25),
            "test",
        );

        assert_eq!(media_file_plans(&timeline, Path::new("/out")).len(), 1);
    }

    #[test]
    fn media_export_remuxes_video_and_writes_float_audio() {
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            eprintln!("SKIP: ffmpeg is unavailable");
            return;
        };
        let dir = std::env::temp_dir().join(format!("align-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("camera.mov");
        let recorder = dir.join("recorder.wav");
        let video_status = std::process::Command::new(&ffmpeg)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=160x90:r=25:d=2",
                "-c:v",
                "mpeg4",
            ])
            .arg(&video)
            .status();
        let audio_status = std::process::Command::new(&ffmpeg)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:duration=1:sample_rate=48000",
                "-c:a",
                "pcm_s24le",
            ])
            .arg(&recorder)
            .status();
        if !video_status.is_ok_and(|status| status.success())
            || !audio_status.is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: fixture generation is unsupported by this ffmpeg");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let output = dir.join("out.mov");
        std::fs::write(&output, b"stale output").unwrap();
        let plan = MediaFilePlan {
            video_url: video,
            recorder_url: recorder,
            recorder_source_in: 0.0,
            leading_silence: 0.5,
            recorder_duration: 1.0,
            video_duration: 2.0,
            url: output.clone(),
        };

        export_media_file(
            &crate::portable::PortableBackend,
            &plan,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        let report = crate::ff::inspect(&output).unwrap();

        assert!(report.has_video);
        assert_eq!(report.audio_streams.len(), 1);
        assert_eq!(report.audio_streams[0].bit_depth, Some(32));
        assert_eq!(report.audio_streams[0].is_float, Some(true));
        assert!((report.duration_seconds - 2.0).abs() < 0.05);

        let video_digest = |path: &Path| {
            std::process::Command::new(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-map", "0:v:0", "-c", "copy", "-f", "md5", "-"])
                .output()
                .unwrap()
                .stdout
        };
        assert_eq!(video_digest(&plan.video_url), video_digest(&output));

        let decoded = std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        let first_signal = decoded
            .stdout
            .as_chunks::<4>()
            .0
            .iter()
            .position(|bytes| f32::from_le_bytes(*bytes).abs() > 0.05)
            .unwrap();
        assert!((24_000..24_100).contains(&first_signal));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FFmpeg remuxes cameras outside QuickTime and MP4 and then decodes the
    /// recorder itself. It must trim the recorder's AC-3 decoder delay as the
    /// backends do, or the clean audio lands 256 samples late.
    #[test]
    fn ffmpeg_remux_trims_the_recorder_decoder_delay() {
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            eprintln!("SKIP: ffmpeg is unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("camera.mts");
        let recorder = dir.path().join("recorder.mts");
        let video_ok = crate::process::command(&ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=160x90:r=25:d=2")
            .args(["-c:v", "mpeg2video", "-f", "mpegts"])
            .arg(&video)
            .status()
            .is_ok_and(|status| status.success());
        if !video_ok || !crate::ff::write_ac3_transport_stream(&recorder, 0.25, 1.0) {
            eprintln!("SKIP: fixture generation is unsupported by this ffmpeg");
            return;
        }
        let plan = MediaFilePlan {
            video_url: video,
            recorder_url: recorder,
            recorder_source_in: 0.1,
            leading_silence: 0.5,
            recorder_duration: 0.9,
            video_duration: 2.0,
            url: dir.path().join("out.mov"),
        };

        export_media_file(
            &crate::portable::PortableBackend,
            &plan,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();

        let mut left = Vec::new();
        crate::portable::PortableBackend
            .decode_native(&plan.url, 0, None, &mut |block| {
                left.extend_from_slice(&block.frames[0]);
                Ok(())
            })
            .unwrap();
        // 0.5 s of leading silence, then the click 0.15 s into the recorder.
        let at = crate::ff::click_at(&left);
        assert!(at.abs_diff(31_200) <= 2, "click at {at}");
    }
}
