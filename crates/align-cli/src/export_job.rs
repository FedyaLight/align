//! One export policy for freshly synchronized and saved single/batch results.
use super::{CliError, ExportOptions};
use align_core::SyncResult;
use align_decode::MediaBackend;
use align_decode::export::ExportArtifact;
use std::{path::Path, sync::atomic::AtomicBool};

pub(super) fn read_results(path: &Path) -> Result<Vec<SyncResult>, CliError> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Results {
        Batch(Vec<SyncResult>),
        Single(Box<SyncResult>),
    }
    let bytes = std::fs::read(path)
        .map_err(|error| CliError::Failure(format!("read {}: {error}", path.display())))?;
    let decoded: Results = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::Failure(format!("parse {}: {error}", path.display())))?;
    let results = match decoded {
        Results::Batch(results) => results,
        Results::Single(result) => vec![*result],
    };
    if results.is_empty() {
        return Err(CliError::Failure(
            "Saved result contains no sequences".into(),
        ));
    }
    Ok(results)
}

impl ExportOptions {
    pub(super) fn execute(
        self,
        backend: &dyn MediaBackend,
        results: &[SyncResult],
        output: &Path,
        cancel: &AtomicBool,
    ) -> Result<Vec<ExportArtifact>, CliError> {
        let Self {
            no_drift,
            aaf,
            aaf_fps,
            replaced_audio,
            fcpxml_storylines,
            no_fcpxml_timeline,
            no_fcpxml_multicam,
            export_media,
            unmatched,
            prevent_group_overlaps,
            disable_unmatched,
            label_synced,
            label_unmatched,
            cut_remove,
            assign,
        } = self;
        let result_count = results.len();
        let mut timelines = Vec::with_capacity(result_count);
        for (index, result) in results.iter().enumerate() {
            let timeline = align_core::export_model::ExportTimeline::from_result_with_options(
                result,
                align_core::export_model::ExportAssemblyOptions {
                    unmatched: unmatched.core(),
                    prevent_group_overlaps,
                    disable_unmatched,
                    label_synced,
                    label_unmatched,
                    cut_remove: cut_remove.core()?,
                    synced_symbol: assign.synced_symbol.clone(),
                    synced_symbol_suffix: assign.synced_symbol_suffix,
                    synced_color: assign.synced_color.clone(),
                    synced_role: assign.synced_role.clone(),
                    unmatched_symbol: assign.unmatched_symbol.clone(),
                    unmatched_symbol_suffix: assign.unmatched_symbol_suffix,
                    unmatched_color: assign.unmatched_color.clone(),
                    unmatched_role: assign.unmatched_role.clone(),
                    sequence_name: assign.sequence_name.as_ref().map(|name| {
                        if result_count > 1 {
                            format!("{name} {}", index + 1)
                        } else {
                            name.clone()
                        }
                    }),
                },
            )
            .map_err(|e| CliError::Failure(e.to_string()))?;
            timelines.push(timeline);
        }
        let mut formats = if aaf {
            vec![align_core::export_model::TimelineExportFormat::Aaf]
        } else {
            align_core::export_model::TimelineExportFormat::default_formats()
        };
        if no_fcpxml_timeline && no_fcpxml_multicam {
            formats.retain(|format| {
                *format != align_core::export_model::TimelineExportFormat::FinalCutProXML
            });
        }
        let artifacts = align_decode::export::export_prepared_many(
            align_decode::export::ExportBatchRequest {
                backend,
                timelines: &timelines,
                directory: output,
                formats: &formats,
                correct_drift: !no_drift,
                include_replaced_sequence: replaced_audio,
                include_media_files: export_media,
                aaf_frame_duration: aaf_fps.frame_duration(),
                include_fcpxml_timeline: !no_fcpxml_timeline,
                include_fcpxml_multicam: !no_fcpxml_multicam,
                group_fcpxml_storylines: fcpxml_storylines,
                cancel,
            },
            Some(&mut |progress: align_decode::export::ExportJobProgress| {
                eprintln!(
                    "export {}/{} {}",
                    progress.completed,
                    progress.total,
                    progress
                        .current
                        .as_deref()
                        .map(align_core::model::file_name)
                        .unwrap_or_default()
                );
            }),
        )?;
        Ok(artifacts)
    }
}
