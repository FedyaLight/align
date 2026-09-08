//! Export orchestration: drift sidecars + precision stems + writers.
//! Port of `SyncEngine.export/exportCorrected/exportPrepared`.
//!
//! Job model mirrors Swift: drift-corrected WAVs (`Corrected Audio`) and
//! per-channel precision stems (`Resolve Precision Audio`) render once per
//! unique asset (existence short-circuits re-renders), progress counts
//! drift + precision jobs together, writers emit atomically, and the
//! precision script is chmodded executable.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use align_core::drift::needs_correction_points;
use align_core::export::model::{
    ExportIsland, ExportItem, ExportTimeline, TimelineExportError, TimelineExportFormat,
    placement_pad_samples, sequence_fps,
};

use crate::backend::MediaBackend;
use crate::render::{
    RenderError, render_channel_cancellable, render_drift_cancellable, render_pad_cancellable,
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

#[derive(Debug)]
pub enum ExportError {
    Timeline(TimelineExportError),
    Render(RenderError),
    Media(String),
    Cancelled,
    Io(String),
}

impl From<RenderError> for ExportError {
    fn from(error: RenderError) -> Self {
        match error {
            RenderError::Cancelled => Self::Cancelled,
            other => Self::Render(other),
        }
    }
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeline(e) => write!(f, "{e}"),
            Self::Render(e) => write!(f, "{e}"),
            Self::Media(message) => write!(f, "Media export: {message}"),
            Self::Cancelled => write!(f, "Cancelled."),
            Self::Io(msg) => write!(f, "Export IO: {msg}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// Plain export (no drift render, no stems) — mirrors `SyncEngine.export`.
pub fn export(
    timeline: &ExportTimeline,
    directory: &Path,
    formats: &[TimelineExportFormat],
    include_replaced_sequence: bool,
) -> Result<Vec<ExportArtifact>, ExportError> {
    timeline
        .validate_audio_source_channels()
        .map_err(ExportError::Timeline)?;
    std::fs::create_dir_all(directory).map_err(|e| ExportError::Io(e.to_string()))?;
    write_artifacts(
        timeline,
        directory,
        formats,
        include_replaced_sequence,
        true,
        true,
        false,
    )
}

/// Drift-corrected export with precision stems — mirrors `exportPrepared`.
/// Sequential job execution (Swift uses structured concurrency; same
/// outputs, deterministic order).
/// Export job description (bundles the growing parameter list).
pub struct ExportRequest<'a> {
    pub backend: &'a dyn MediaBackend,
    pub timeline: &'a ExportTimeline,
    pub directory: &'a Path,
    pub formats: &'a [TimelineExportFormat],
    pub correct_drift: bool,
    pub include_replaced_sequence: bool,
    pub include_media_files: bool,
    /// Override the AAF composition timecode rate used by Resolve. Picture
    /// source slots retain their native edit rates.
    pub aaf_frame_duration: Option<align_core::MediaTime>,
    pub include_fcpxml_timeline: bool,
    pub include_fcpxml_multicam: bool,
    pub group_fcpxml_storylines: bool,
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

/// One export action containing several independently synchronized sequences.
/// Premiere receives one multi-sequence project; formats whose containers hold
/// one timeline are written into numbered sequence folders.
pub struct ExportBatchRequest<'a> {
    pub backend: &'a dyn MediaBackend,
    pub timelines: &'a [ExportTimeline],
    pub directory: &'a Path,
    pub formats: &'a [TimelineExportFormat],
    pub correct_drift: bool,
    pub include_replaced_sequence: bool,
    pub include_media_files: bool,
    pub aaf_frame_duration: Option<align_core::MediaTime>,
    pub include_fcpxml_timeline: bool,
    pub include_fcpxml_multicam: bool,
    pub group_fcpxml_storylines: bool,
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

pub fn export_prepared_many(
    request: ExportBatchRequest<'_>,
    mut progress: Option<&mut dyn FnMut(ExportJobProgress)>,
) -> Result<Vec<ExportArtifact>, ExportError> {
    if request.timelines.is_empty() {
        return Err(ExportError::Timeline(
            TimelineExportError::NoSynchronizedIslands,
        ));
    }
    if request.timelines.len() == 1 {
        return export_prepared(
            ExportRequest {
                backend: request.backend,
                timeline: &request.timelines[0],
                directory: request.directory,
                formats: request.formats,
                correct_drift: request.correct_drift,
                include_replaced_sequence: request.include_replaced_sequence,
                include_media_files: request.include_media_files,
                aaf_frame_duration: request.aaf_frame_duration,
                include_fcpxml_timeline: request.include_fcpxml_timeline,
                include_fcpxml_multicam: request.include_fcpxml_multicam,
                group_fcpxml_storylines: request.group_fcpxml_storylines,
                cancel: request.cancel,
            },
            progress,
        );
    }

    std::fs::create_dir_all(request.directory).map_err(|e| ExportError::Io(e.to_string()))?;
    let mut artifacts = Vec::new();
    let mut premiere_documents = Vec::new();
    let mut fcpxml_documents = Vec::new();
    let mut aaf_manifests = Vec::new();
    for (index, timeline) in request.timelines.iter().enumerate() {
        if request.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let sequence_dir = request
            .directory
            .join(sequence_folder(index, &timeline.name));
        let (sequence_artifacts, aaf_manifest) = if let Some(callback) = progress.as_deref_mut() {
            export_prepared_internal(
                ExportRequest {
                    backend: request.backend,
                    timeline,
                    directory: &sequence_dir,
                    formats: request.formats,
                    correct_drift: request.correct_drift,
                    include_replaced_sequence: request.include_replaced_sequence,
                    include_media_files: request.include_media_files,
                    aaf_frame_duration: request.aaf_frame_duration,
                    include_fcpxml_timeline: request.include_fcpxml_timeline,
                    include_fcpxml_multicam: request.include_fcpxml_multicam,
                    group_fcpxml_storylines: request.group_fcpxml_storylines,
                    cancel: request.cancel,
                },
                Some(callback),
                true,
            )?
        } else {
            export_prepared_internal(
                ExportRequest {
                    backend: request.backend,
                    timeline,
                    directory: &sequence_dir,
                    formats: request.formats,
                    correct_drift: request.correct_drift,
                    include_replaced_sequence: request.include_replaced_sequence,
                    include_media_files: request.include_media_files,
                    aaf_frame_duration: request.aaf_frame_duration,
                    include_fcpxml_timeline: request.include_fcpxml_timeline,
                    include_fcpxml_multicam: request.include_fcpxml_multicam,
                    group_fcpxml_storylines: request.group_fcpxml_storylines,
                    cancel: request.cancel,
                },
                None,
                true,
            )?
        };
        aaf_manifests.extend(aaf_manifest);
        for artifact in sequence_artifacts {
            match artifact.format {
                ExportArtifactFormat::PremiereXML => premiere_documents.push(
                    std::fs::read_to_string(&artifact.url)
                        .map_err(|e| ExportError::Io(e.to_string()))?,
                ),
                ExportArtifactFormat::FinalCutProXML => fcpxml_documents.push(
                    std::fs::read_to_string(&artifact.url)
                        .map_err(|e| ExportError::Io(e.to_string()))?,
                ),
                _ => {
                    artifacts.push(artifact);
                    continue;
                }
            }
            let _ = std::fs::remove_file(artifact.url);
        }
    }
    if !premiere_documents.is_empty() {
        let xml = align_core::export::premiere::combine_project_documents(&premiere_documents)
            .map_err(ExportError::Io)?;
        let url = request.directory.join("Align – Adobe Premiere Pro.xml");
        let tmp = url.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, xml).map_err(|e| ExportError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &url).map_err(|e| ExportError::Io(e.to_string()))?;
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::PremiereXML,
            url,
        });
    }
    if !fcpxml_documents.is_empty() {
        let xml = align_core::export::fcpxml::combine_documents(&fcpxml_documents)
            .map_err(ExportError::Io)?;
        let url = request.directory.join("Align – Final Cut Pro.fcpxml");
        let tmp = url.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, xml).map_err(|e| ExportError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &url).map_err(|e| ExportError::Io(e.to_string()))?;
        artifacts.push(ExportArtifact {
            format: ExportArtifactFormat::FinalCutProXML,
            url,
        });
    }
    if !aaf_manifests.is_empty() {
        let manifest = serde_json::json!({"version": 3, "sequences": aaf_manifests});
        let url = request.directory.join("Align.aaf");
        write_aaf_manifest(&manifest, request.directory, &url, request.cancel)?;
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

fn export_prepared_internal(
    request: ExportRequest<'_>,
    mut progress: Option<&mut dyn FnMut(ExportJobProgress)>,
    defer_aaf: bool,
) -> Result<(Vec<ExportArtifact>, Option<serde_json::Value>), ExportError> {
    request
        .timeline
        .validate_audio_source_channels()
        .map_err(ExportError::Timeline)?;
    std::fs::create_dir_all(request.directory).map_err(|e| ExportError::Io(e.to_string()))?;
    let ExportRequest {
        backend,
        timeline,
        directory,
        formats,
        correct_drift,
        include_replaced_sequence,
        include_media_files,
        aaf_frame_duration,
        include_fcpxml_timeline,
        include_fcpxml_multicam,
        group_fcpxml_storylines,
        cancel,
    } = request;
    // Combine up front: the writer floors the combined island's starts, and
    // placement pads depend on those absolute starts (chronology shifts are
    // generally fractional). Planning on pre-combine starts would attach the
    // wrong prepend whenever the island minimum is not frame-exact.
    // Re-combining inside write_artifacts is a no-op shift afterwards.
    // Drift mappings are shift-invariant (relative source/island coords).
    let mut combined = ExportTimeline::new(
        vec![timeline.combined_island(1.0)],
        timeline.frame_duration,
        &timeline.name,
    );
    combined.temporal_policy = timeline.temporal_policy.clone();
    combined.copy_assembly_policy_from(timeline);
    // Resolve floors the timeline end even when the final audio item extends
    // into the next partial frame. Extend only a final recorder's derived
    // precision stem with silence so an Entire Timeline render retains it.
    if formats.contains(&TimelineExportFormat::ResolveScript) {
        let fps = sequence_fps(combined.frame_duration);
        let end_for = |item: &ExportItem| {
            let duration = if correct_drift
                && needs_correction_points(&item.mapping_points)
                && item.clip.kind == align_core::MediaKind::Audio
            {
                item.corrected_selected_duration()
            } else {
                item.selected_timeline_duration("audio")
            };
            item.timeline_start("audio") + duration
        };
        let maximum_end = combined.islands[0]
            .clips
            .iter()
            .map(&end_for)
            .fold(0.0, f64::max);
        let frame_end = maximum_end * fps;
        if (frame_end - frame_end.round()).abs() > 1e-7 {
            if let Some(last) = combined.islands[0].clips.iter_mut().find(|item| {
                item.clip.kind == align_core::MediaKind::Audio
                    && !item.is_retimed("audio")
                    && (end_for(item) - maximum_end).abs() < 1e-9
            }) {
                let sr = last.clip.audio.first().map_or(48000.0, |a| a.sample_rate);
                // One extra sample covers nearest-sample placement rounding.
                last.precision_tail_samples =
                    ((frame_end.ceil() / fps - maximum_end) * sr).ceil() as u64 + 1;
            }
        }
    }
    let timeline = &combined;
    let all_items: Vec<&ExportItem> = timeline
        .islands
        .iter()
        .flat_map(|i| i.clips.iter())
        .collect();

    let drift_ids: HashSet<&str> = all_items
        .iter()
        .filter(|item| {
            correct_drift
                && item.clip.kind == align_core::MediaKind::Audio
                && needs_correction_points(&item.mapping_points)
        })
        .map(|item| item.clip.id.0.as_str())
        .collect();

    let wants_aaf = formats.contains(&TimelineExportFormat::Aaf);
    if wants_aaf && include_replaced_sequence {
        return Err(ExportError::Io(
            "AAF replaced sequence export is not implemented".into(),
        ));
    }
    let wants_script = formats.contains(&TimelineExportFormat::ResolveScript);
    // Placement pads are a PremiereXML-only concern: no other writer reads
    // them (OTIO/FCPXML place exactly already; the Resolve script locates
    // stems through its own fractional API, so its stems stay unpadded).
    let wants_pad = formats.contains(&TimelineExportFormat::PremiereXML);
    let sequence_rate_fps = sequence_fps(timeline.frame_duration);
    let wants_fcpxml = formats.contains(&TimelineExportFormat::FinalCutProXML);
    let precision_items: Vec<&&ExportItem> = all_items
        .iter()
        .filter(|item| {
            (wants_script || wants_aaf || wants_fcpxml)
                && (item.clip.kind == align_core::MediaKind::Audio
                    || (wants_aaf && !item.clip.audio.is_empty() && item.is_enabled("audio")))
                && (wants_aaf
                    || item.clip.audio.first().map_or(0, |a| a.channels) > 1
                    || !item.is_full_source_selection()
                    || item.precision_tail_samples > 0)
        })
        .collect();
    let precision_keys: HashSet<String> = precision_items
        .iter()
        .map(|item| item.precision_asset_key())
        .collect();

    let media_dir = directory.join("Media Files");
    let media_jobs = if include_media_files {
        media_file_plans(timeline, &media_dir).len()
    } else {
        0
    };
    // Read the original once per export, including its metadata chunks.
    // File size/mtime alone cannot detect same-path, same-size replacements.
    let mut source_tags = HashMap::new();
    for item in &all_items {
        let needs_asset = drift_ids.contains(item.clip.id.0.as_str())
            || precision_keys.contains(&item.precision_asset_key())
            || (wants_pad
                && pad_plan(item, sequence_rate_fps, &drift_ids, &media_dir, "preview").is_some());
        if needs_asset && !source_tags.contains_key(&item.clip.url) {
            source_tags.insert(
                item.clip.url.clone(),
                source_content_tag(&item.clip.url, cancel)?,
            );
        }
    }
    // Unique pad sidecars are known upfront (per-item starts are fixed), so
    // progress totals stay exact like the drift/precision counts.
    let mut pad_dests: HashSet<PathBuf> = HashSet::new();
    if wants_pad {
        let preview_dir = directory.join("Corrected Audio");
        for item in &all_items {
            if let Some((_, dest)) = pad_plan(
                item,
                sequence_rate_fps,
                &drift_ids,
                &preview_dir,
                source_tags.get(&item.clip.url).map_or("", String::as_str),
            ) {
                pad_dests.insert(dest);
            }
        }
    }
    let total_jobs = drift_ids.len() + pad_dests.len() + precision_keys.len() + media_jobs;
    let audio_dir = directory.join("Corrected Audio");
    if !drift_ids.is_empty() || !pad_dests.is_empty() {
        std::fs::create_dir_all(&audio_dir).map_err(|e| ExportError::Io(e.to_string()))?;
    }
    let precision_dir = directory.join("Resolve Precision Audio");
    if !precision_keys.is_empty() {
        std::fs::create_dir_all(&precision_dir).map_err(|e| ExportError::Io(e.to_string()))?;
    }

    let mut completed = 0usize;
    let mut corrected_urls: HashMap<&str, PathBuf> = HashMap::new();
    let mut precision_urls: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut pad_rendered: HashSet<PathBuf> = HashSet::new();
    let mut corrected_islands = Vec::with_capacity(timeline.islands.len());

    for island in &timeline.islands {
        let mut items = Vec::with_capacity(island.clips.len());
        for item in &island.clips {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(ExportError::Cancelled);
            }
            let stem = item
                .clip
                .url
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("clip");
            let suffix: String = item.clip.id.0.chars().take(8).collect();
            let mut prepared = item.clone();

            if drift_ids.contains(item.clip.id.0.as_str()) {
                if !corrected_urls.contains_key(item.clip.id.0.as_str()) {
                    emit(
                        &mut progress,
                        completed,
                        total_jobs,
                        Some(item.clip.url.clone()),
                        ExportJobKind::Audio,
                    );
                    let points: Vec<(f64, f64)> = item
                        .mapping_points
                        .iter()
                        .map(|p| (p.source.as_seconds(), p.island.as_seconds()))
                        .collect();
                    let ppm = ((item.mapping_rate - 1.0) * 1_000_000.0).round() as i64;
                    let correction = if item.mapping_points.len() > 2 {
                        "piecewise warp".to_string()
                    } else {
                        format!("drift {}{}ppm", if ppm >= 0 { "+" } else { "" }, ppm)
                    };
                    let destination = audio_dir.join(format!(
                        "{stem} – {correction} – {suffix}-{}-{}-r11.wav",
                        item.mapping_digest(),
                        source_tags[&item.clip.url]
                    ));
                    if !destination.is_file() {
                        render_drift_cancellable(
                            backend,
                            &item.clip.url,
                            &destination,
                            &points,
                            cancel,
                        )
                        .map_err(ExportError::from)?;
                    }
                    corrected_urls.insert(item.clip.id.0.as_str(), destination);
                    completed += 1;
                    emit(
                        &mut progress,
                        completed,
                        total_jobs,
                        Some(item.clip.url.clone()),
                        ExportJobKind::Audio,
                    );
                }
                prepared =
                    prepared.with_corrected_audio(corrected_urls[item.clip.id.0.as_str()].clone());
            }

            if wants_pad {
                if let Some((pad_samples, destination)) = pad_plan(
                    item,
                    sequence_rate_fps,
                    &drift_ids,
                    &audio_dir,
                    source_tags.get(&item.clip.url).map_or("", String::as_str),
                ) {
                    if !pad_rendered.contains(&destination) {
                        emit(
                            &mut progress,
                            completed,
                            total_jobs,
                            Some(item.clip.url.clone()),
                            ExportJobKind::Audio,
                        );
                        if !destination.is_file() {
                            // Pad the drift sidecar when one exists, else the
                            // original: precision stems keep using the
                            // unpadded source, so the script workflow is
                            // unaffected by this file.
                            let pad_source = prepared
                                .corrected_audio_url
                                .clone()
                                .unwrap_or_else(|| item.clip.url.clone());
                            let sr = item.clip.audio.first().unwrap().sample_rate;
                            let (start, end) = if prepared.corrected_audio_url.is_some() {
                                (item.corrected_source_in(), item.corrected_source_out())
                            } else {
                                (
                                    item.selected_source_in("audio"),
                                    item.selected_source_out("audio"),
                                )
                            };
                            render_pad_cancellable(
                                backend,
                                &pad_source,
                                &destination,
                                pad_samples,
                                Some(((start * sr).round() as u64, (end * sr).round() as u64)),
                                cancel,
                            )
                            .map_err(ExportError::from)?;
                        }
                        pad_rendered.insert(destination.clone());
                        completed += 1;
                        emit(
                            &mut progress,
                            completed,
                            total_jobs,
                            Some(item.clip.url.clone()),
                            ExportJobKind::Audio,
                        );
                    }
                    let sample_rate = item.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
                    prepared =
                        prepared.with_placement_pad(destination, pad_samples as f64 / sample_rate);
                }
            }

            if precision_keys.contains(&item.precision_asset_key()) {
                let key = item.precision_asset_key();
                if let std::collections::hash_map::Entry::Vacant(vacant) =
                    precision_urls.entry(key.clone())
                {
                    emit(
                        &mut progress,
                        completed,
                        total_jobs,
                        Some(item.clip.url.clone()),
                        ExportJobKind::Audio,
                    );
                    let channels = item.clip.audio.first().map_or(0, |a| a.channels);
                    let source = prepared
                        .corrected_audio_url
                        .clone()
                        .unwrap_or_else(|| item.clip.url.clone());
                    let float_source = prepared.corrected_audio_url.is_some();
                    let urls = (0..channels)
                        .map(|channel| {
                            let destination = precision_dir.join(format!(
                                "{stem} – channel {} – {suffix}-{}-{}-{}-r11.wav",
                                channel + 1,
                                prepared.precision_digest(),
                                source_tags[&item.clip.url],
                                if float_source {
                                    "corrected"
                                } else {
                                    "original"
                                }
                            ));
                            if !destination.is_file() {
                                let (bit_depth, is_float) = if float_source {
                                    (Some(32), true)
                                } else {
                                    let a = item.clip.audio.first();
                                    (
                                        a.and_then(|a| a.bit_depth),
                                        a.and_then(|a| a.is_float).unwrap_or(false),
                                    )
                                };
                                render_channel_cancellable(
                                    backend,
                                    &source,
                                    &destination,
                                    channel,
                                    bit_depth,
                                    is_float,
                                    prepared.precision_source_start(),
                                    prepared.precision_source_duration(),
                                    prepared.precision_tail_samples,
                                    cancel,
                                )
                                .map_err(ExportError::from)?;
                            }
                            Ok(destination)
                        })
                        .collect::<Result<Vec<_>, ExportError>>()?;
                    vacant.insert(urls);
                    completed += 1;
                    emit(
                        &mut progress,
                        completed,
                        total_jobs,
                        Some(item.clip.url.clone()),
                        ExportJobKind::Audio,
                    );
                }
                prepared = prepared
                    .with_precision_audio(precision_urls[&item.precision_asset_key()].clone());
            }
            items.push(prepared);
        }
        corrected_islands.push(ExportIsland {
            id: island.id,
            clips: items,
            duration: island.duration,
        });
    }

    let mut corrected =
        ExportTimeline::new(corrected_islands, timeline.frame_duration, &timeline.name);
    corrected.temporal_policy = timeline.temporal_policy.clone();
    corrected.copy_assembly_policy_from(timeline);
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ExportError::Cancelled);
    }
    let standard_formats: Vec<_> = formats
        .iter()
        .copied()
        .filter(|f| *f != TimelineExportFormat::Aaf)
        .collect();
    let mut artifacts = write_artifacts(
        &corrected,
        directory,
        &standard_formats,
        include_replaced_sequence,
        include_fcpxml_timeline,
        include_fcpxml_multicam,
        group_fcpxml_storylines,
    )?;
    let mut deferred_aaf = None;
    if wants_aaf {
        let manifest =
            crate::aaf::timeline_manifest_with_frame_rate(&corrected, aaf_frame_duration, cancel)
                .map_err(|e| ExportError::Io(e.to_string()))?;
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
    if include_media_files {
        std::fs::create_dir_all(&media_dir).map_err(|e| ExportError::Io(e.to_string()))?;
        for plan in media_file_plans(&corrected, &media_dir) {
            emit(
                &mut progress,
                completed,
                total_jobs,
                Some(plan.video_url.clone()),
                ExportJobKind::Media,
            );
            let file = export_media_file(&plan, cancel)?;
            completed += 1;
            emit(
                &mut progress,
                completed,
                total_jobs,
                Some(plan.video_url.clone()),
                ExportJobKind::Media,
            );
            artifacts.push(ExportArtifact {
                format: ExportArtifactFormat::MediaFile,
                url: file,
            });
        }
    }
    Ok((artifacts, deferred_aaf))
}

fn write_aaf_manifest(
    manifest: &serde_json::Value,
    directory: &Path,
    url: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), ExportError> {
    static NEXT_AAF_MANIFEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT_AAF_MANIFEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let manifest_path = directory.join(format!(".align-aaf-{}-{serial}.json", std::process::id()));
    let bytes = serde_json::to_vec(manifest).map_err(|e| ExportError::Io(e.to_string()))?;
    std::fs::write(&manifest_path, bytes).map_err(|e| ExportError::Io(e.to_string()))?;
    let result = crate::aaf::write_audio(&manifest_path, url, cancel);
    let _ = std::fs::remove_file(&manifest_path);
    result.map_err(|e| match e {
        crate::aaf::AafError::Cancelled => ExportError::Cancelled,
        other => ExportError::Io(other.to_string()),
    })
}

/// Placement-pad sidecar plan for one timeline item: `Some((samples, path))`
/// when a fractional audio start needs a padded file. Pure (no I/O), so the
/// progress total is exact upfront and re-exports resolve identical paths
/// (existence short-circuits the render; existing files are never touched).
///
/// Skipped for retimed audio (time-remap graphs own subframe truth there),
/// items without audio, and non-positive sample rates. Only audio clipitems
/// reference the sidecar; video sides are untouched (frame-grid limitation).
fn pad_plan(
    item: &ExportItem,
    sequence_fps: f64,
    drift_ids: &HashSet<&str>,
    audio_dir: &Path,
    source_tag: &str,
) -> Option<(u64, PathBuf)> {
    let audio = item.clip.audio.first()?;
    if audio.sample_rate <= 0.0 || audio.channels == 0 {
        return None;
    }
    if item.is_retimed("audio") {
        return None;
    }
    let pad = placement_pad_samples(
        item.timeline_start("audio"),
        sequence_fps,
        audio.sample_rate,
    )?;
    let stem = item
        .clip
        .url
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("clip");
    let suffix: String = item.clip.id.0.chars().take(8).collect();
    // Drift-derived pads inherit the item mapping tag so a re-sync with
    // different drift cannot collide with an older pad in the same folder.
    let tag = if drift_ids.contains(item.clip.id.0.as_str()) {
        item.mapping_digest()
    } else {
        "direct".to_string()
    };
    Some((
        pad,
        audio_dir.join(format!(
            "{stem} – pad {pad} – {suffix}-{tag}-{}-{}-{source_tag}-r11.wav",
            item.selected_source_in("audio").to_bits(),
            item.selected_source_out("audio").to_bits()
        )),
    ))
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

fn source_content_tag(
    path: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String, ExportError> {
    let mut file = std::fs::File::open(path).map_err(|e| ExportError::Io(e.to_string()))?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let n = file
            .read(&mut buffer)
            .map_err(|e| ExportError::Io(e.to_string()))?;
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
        if video.is_retimed("video") || !planned_video.insert(video.clip.id.0.clone()) {
            continue;
        }
        let video_duration = video.source_duration();
        let video_start = video.start - video.source_in;
        let video_end = video_start + video_duration;
        let recorder = combined
            .clips
            .iter()
            .filter(|item| item.clip.kind == align_core::MediaKind::Audio)
            .filter(|item| !item.is_retimed("audio"))
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

pub(crate) fn wait_render_process(
    child: &mut std::process::Child,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<std::process::ExitStatus, RenderError> {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RenderError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RenderError::PlacementPad(error.to_string()));
            }
        }
    }
}

/// Remux one complete camera file without re-encoding its video stream. The
/// external track is trimmed sample-accurately, padded to the camera duration,
/// and encoded as uncompressed 32-bit float at the NLE-native 48 kHz rate.
fn export_media_file(
    plan: &MediaFilePlan,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<PathBuf, ExportError> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ExportError::Cancelled);
    }
    let ffmpeg = crate::ff::ffmpeg_bin()
        .ok_or_else(|| ExportError::Media("ffmpeg not found in PATH.".into()))?;
    let delay_samples = (plan.leading_silence.max(0.0) * 48_000.0).round() as u64;
    let filter = format!(
        "[1:a:0]atrim=duration={:.9},asetpts=PTS-STARTPTS,aresample=48000,adelay={delay_samples}S:all=1,apad=whole_dur={:.9},atrim=duration={:.9}[clean]",
        plan.recorder_duration, plan.video_duration, plan.video_duration
    );
    let temp = plan
        .url
        .with_extension(format!("tmp-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut child = std::process::Command::new(ffmpeg)
        .args(["-y", "-v", "error", "-i"])
        .arg(&plan.video_url)
        .args(["-ss", &format!("{:.9}", plan.recorder_source_in), "-i"])
        .arg(&plan.recorder_url)
        .args(["-filter_complex", &filter])
        .args(["-map", "0:v:0", "-map", "[clean]"])
        .args(["-map_metadata", "0", "-c:v", "copy", "-c:a", "pcm_f32le"])
        .args(["-t", &format!("{:.9}", plan.video_duration)])
        .arg(&temp)
        .spawn()
        .map_err(|e| ExportError::Io(e.to_string()))?;
    let status = match wait_render_process(&mut child, cancel) {
        Ok(status) => status,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(ExportError::from(error));
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        return Err(ExportError::Media(format!(
            "ffmpeg failed for {}",
            plan.video_url.display()
        )));
    }
    #[cfg(windows)]
    if plan.url.is_file() {
        std::fs::remove_file(&plan.url).map_err(|e| ExportError::Io(e.to_string()))?;
    }
    std::fs::rename(&temp, &plan.url).map_err(|e| ExportError::Io(e.to_string()))?;
    Ok(plan.url.clone())
}

fn emit(
    progress: &mut Option<&mut dyn FnMut(ExportJobProgress)>,
    completed: usize,
    total: usize,
    current: Option<PathBuf>,
    kind: ExportJobKind,
) {
    if let Some(p) = progress {
        p(ExportJobProgress {
            completed,
            total,
            current,
            kind,
        });
    }
}

fn write_artifacts(
    timeline: &ExportTimeline,
    directory: &Path,
    formats: &[TimelineExportFormat],
    include_replaced_sequence: bool,
    include_fcpxml_timeline: bool,
    include_fcpxml_multicam: bool,
    group_fcpxml_storylines: bool,
) -> Result<Vec<ExportArtifact>, ExportError> {
    let mut combined = ExportTimeline::new(
        vec![timeline.combined_island(1.0)],
        timeline.frame_duration,
        &timeline.name,
    );
    combined.temporal_policy = timeline.temporal_policy.clone();
    combined.copy_assembly_policy_from(timeline);
    let mut artifacts = Vec::with_capacity(formats.len());
    for format in formats {
        if *format == TimelineExportFormat::Aaf {
            return Err(ExportError::Io("AAF requires prepared audio export".into()));
        }
        let application = match format {
            TimelineExportFormat::Aaf => unreachable!(),
            TimelineExportFormat::PremiereXML => "Adobe Premiere Pro",
            TimelineExportFormat::FinalCutProXML => "Final Cut Pro",
            TimelineExportFormat::ResolveOTIO
            | TimelineExportFormat::ResolveScript
            | TimelineExportFormat::ResolveXML => "DaVinci Resolve",
        };
        // En dash in app bundle names mirrors Swift exactly.
        let url = directory.join(format!("Align – {application}.{}", format.file_extension()));
        let bytes: Vec<u8> = match format {
            TimelineExportFormat::Aaf => unreachable!(),
            TimelineExportFormat::ResolveOTIO => {
                align_core::export::otio::data(&combined).map_err(ExportError::Io)?
            }
            TimelineExportFormat::ResolveScript => {
                align_core::export::script::write().as_bytes().to_vec()
            }
            TimelineExportFormat::ResolveXML | TimelineExportFormat::PremiereXML => {
                align_core::export::premiere::write(&combined, *format, include_replaced_sequence)
                    .into_bytes()
            }
            TimelineExportFormat::FinalCutProXML => align_core::export::fcpxml::write_with_options(
                &combined,
                include_fcpxml_timeline,
                include_fcpxml_multicam,
                group_fcpxml_storylines,
            )
            .into_bytes(),
        };
        // Atomic write: tmp + rename.
        let tmp = url.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, &bytes).map_err(|e| ExportError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &url).map_err(|e| ExportError::Io(e.to_string()))?;
        #[cfg(unix)]
        if *format == TimelineExportFormat::ResolveScript {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&url, std::fs::Permissions::from_mode(0o755));
        }
        artifacts.push(ExportArtifact {
            format: (*format).into(),
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
        if crate::ff::ffmpeg_bin().is_none() {
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
                    formats: &[TimelineExportFormat::PremiereXML],
                    correct_drift: false,
                    include_replaced_sequence: false,
                    include_media_files: false,
                    aaf_frame_duration: None,
                    include_fcpxml_timeline: true,
                    include_fcpxml_multicam: true,
                    group_fcpxml_storylines: false,
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
                formats: &[
                    TimelineExportFormat::PremiereXML,
                    TimelineExportFormat::FinalCutProXML,
                    TimelineExportFormat::ResolveOTIO,
                ],
                correct_drift: false,
                include_replaced_sequence: false,
                include_media_files: false,
                aaf_frame_duration: None,
                include_fcpxml_timeline: true,
                include_fcpxml_multicam: true,
                group_fcpxml_storylines: false,
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
    fn resolve_final_partial_frame_keeps_selected_tail_and_pads_only_silence() {
        let dir = std::env::temp_dir().join(format!("align-resolve-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.wav");
        let mut w = hound::WavWriter::create(
            &src,
            hound::WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for i in 0..4800 {
            w.write_sample(if i == 3479 || i == 3500 { 16000i16 } else { 0 })
                .unwrap();
        }
        w.finalize().unwrap();
        let original = std::fs::read(&src).unwrap();
        let mut clip = item("tail", MediaKind::Audio, 0.0, 0.1);
        clip.clip.url = src.clone();
        clip.clip.audio[0].channels = 1;
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
        export_prepared(
            ExportRequest {
                backend: &crate::portable::PortableBackend,
                timeline: &timeline,
                directory: &out,
                formats: &[
                    TimelineExportFormat::ResolveScript,
                    TimelineExportFormat::ResolveOTIO,
                ],
                correct_drift: false,
                include_replaced_sequence: false,
                include_media_files: false,
                aaf_frame_duration: None,
                include_fcpxml_timeline: true,
                include_fcpxml_multicam: true,
                group_fcpxml_storylines: false,
                cancel: &std::sync::atomic::AtomicBool::new(false),
            },
            None,
        )
        .unwrap();
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
        let Some(ffmpeg) = crate::ff::ffmpeg_bin() else {
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

        export_media_file(&plan, &std::sync::atomic::AtomicBool::new(false)).unwrap();
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
            .chunks_exact(4)
            .position(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).abs() > 0.05)
            .unwrap();
        assert!((24_000..24_100).contains(&first_signal));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
