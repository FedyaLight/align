//! One export policy for freshly synchronized and saved single/batch results.
use super::{CliError, ExportOptions};
use align_core::camera_clock::CameraClockEstimate;
use align_core::export_model::UnmatchedPlacement;
use align_core::model::file_name;
use align_core::{ClipId, SyncResult};
use align_decode::MediaBackend;
use align_decode::export::ExportArtifact;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

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
    let mut results = match decoded {
        Results::Batch(results) => results,
        Results::Single(result) => vec![*result],
    };
    // Results saved from relative paths name media relative to the working
    // directory, where export also reads them.
    for clip in results.iter_mut().flat_map(|r| &mut r.project.clips) {
        if clip.url.is_relative() {
            clip.url = super::absolute(std::mem::take(&mut clip.url));
        }
    }
    if results.is_empty() {
        return Err(CliError::Failure(
            "Saved result contains no sequences".into(),
        ));
    }
    Ok(results)
}

/// Clips named by `--camera-clock-except`: a clip ID, a path, or a file
/// name that only one source path has.
fn excepted_clips(results: &[SyncResult], names: &[String]) -> Result<HashSet<ClipId>, CliError> {
    let clips: Vec<&align_core::Clip> = results.iter().flat_map(|r| &r.project.clips).collect();
    let mut excepted = HashSet::new();
    for name in names {
        let path = super::absolute(PathBuf::from(name));
        let by_identity: Vec<_> = clips
            .iter()
            .filter(|clip| clip.id.0 == *name || clip.url == path)
            .collect();
        let matches = if by_identity.is_empty() {
            clips
                .iter()
                .filter(|clip| file_name(&clip.url).eq_ignore_ascii_case(name))
                .collect()
        } else {
            by_identity
        };
        let paths: HashSet<&Path> = matches.iter().map(|clip| clip.url.as_path()).collect();
        match paths.len() {
            0 => {
                return Err(CliError::Failure(format!(
                    "--camera-clock-except {name} matches no clip."
                )));
            }
            1 => excepted.extend(matches.iter().map(|clip| clip.id.clone())),
            _ => {
                return Err(CliError::Failure(format!(
                    "--camera-clock-except {name} names {} files; give the path of one.",
                    paths.len()
                )));
            }
        }
    }
    Ok(excepted)
}

/// The estimates `--camera-clock` applies to one result, reported on stderr
/// with the clips it leaves unmatched and why.
fn camera_clock_estimates(
    result: &SyncResult,
    excepted: &HashSet<ClipId>,
    unmatched: UnmatchedPlacement,
) -> Vec<CameraClockEstimate> {
    let report = align_core::camera_clock::camera_clock(result, &[]);
    let names = align_core::camera_clock::clip_names(result);
    let name = |id: &ClipId| names.get(id).cloned().unwrap_or_else(|| id.0.clone());
    for (id, refusal) in &report.refusals {
        eprintln!(
            "camera clock: {} stays unmatched. {}",
            name(id),
            refusal.describe(&name)
        );
    }
    let (applied, skipped): (Vec<_>, Vec<_>) = report
        .estimates
        .into_iter()
        .partition(|estimate| !excepted.contains(&estimate.clip_id));
    for estimate in &skipped {
        eprintln!(
            "camera clock: {} stays unmatched (--camera-clock-except).",
            name(&estimate.clip_id)
        );
    }
    if result.project.imported_timeline.is_some() {
        for hint in align_core::camera_clock::imported_edit_hints(result, &applied) {
            let deviation = hint.deviation();
            let relation = if hint.agrees() {
                "agrees with".to_string()
            } else {
                format!(
                    "starts {:.2} s {} than",
                    deviation.abs(),
                    if deviation > 0.0 { "later" } else { "earlier" }
                )
            };
            eprintln!(
                "camera clock: edit {} of {} on {} {relation} its camera clock (±{:.1} s). Imported edits keep their placement.",
                hint.edit_id,
                name(&hint.clip_id),
                hint.track_label(),
                hint.tolerance
            );
        }
        return Vec::new();
    }
    if unmatched == UnmatchedPlacement::Remove {
        if !applied.is_empty() {
            eprintln!(
                "camera clock: --unmatched remove leaves out {} clip{} the camera clock would place.",
                applied.len(),
                if applied.len() == 1 { "" } else { "s" }
            );
        }
        return Vec::new();
    }
    applied
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
            media_bins,
            fcpxml_storylines,
            no_fcpxml_timeline,
            no_fcpxml_multicam,
            export_media,
            unmatched,
            camera_clock,
            camera_clock_except,
            prevent_group_overlaps,
            disable_unmatched,
            label_synced,
            label_unmatched,
            cut_remove,
            assign,
        } = self;
        let result_count = results.len();
        let excepted = excepted_clips(results, &camera_clock_except)?;
        let mut timelines = Vec::with_capacity(result_count);
        for (index, result) in results.iter().enumerate() {
            let estimates = if camera_clock {
                camera_clock_estimates(result, &excepted, unmatched.core())
            } else {
                Vec::new()
            };
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
                    camera_clock: estimates.clone(),
                },
            )
            .map_err(|e| CliError::Failure(e.to_string()))?;
            if !estimates.is_empty() {
                let names = align_core::camera_clock::clip_names(result);
                let name = |id: &ClipId| names.get(id).cloned().unwrap_or_else(|| id.0.clone());
                let placed: HashSet<_> = timeline
                    .islands
                    .iter()
                    .flat_map(|i| &i.clips)
                    .filter(|item| item.camera_clock.is_some())
                    .map(|item| &item.clip.id)
                    .collect();
                for estimate in &estimates {
                    if placed.contains(&estimate.clip_id) {
                        eprintln!(
                            "camera clock: {} placed. {}",
                            name(&estimate.clip_id),
                            align_core::camera_clock::describe_estimate(estimate, &name)
                        );
                    } else {
                        eprintln!(
                            "camera clock: cleanup leaves out {}.",
                            name(&estimate.clip_id)
                        );
                    }
                }
            }
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
        let options = align_decode::export::ExportOptions {
            formats,
            correct_drift: !no_drift,
            include_replaced_sequence: replaced_audio,
            media_bins,
            include_media_files: export_media,
            aaf_frame_duration: aaf_fps.frame_duration(),
            fcpxml: align_core::export::fcpxml::FcpxmlOptions {
                timeline: !no_fcpxml_timeline,
                multicam: !no_fcpxml_multicam,
                group_storylines: fcpxml_storylines,
                ..Default::default()
            },
        };
        let artifacts = align_decode::export::export_prepared_many(
            align_decode::export::ExportBatchRequest {
                backend,
                timelines: &timelines,
                directory: output,
                options: &options,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn result(paths: &[&str]) -> SyncResult {
        let clips: Vec<align_core::Clip> = paths
            .iter()
            .map(|path| align_core::Clip {
                id: ClipId::new(format!("id-{path}")),
                url: PathBuf::from(path),
                kind: align_core::MediaKind::Video,
                duration: align_core::MediaTime::seconds(1.0),
                audio: Vec::new(),
                video: None,
                recorded_at: None,
                recorded_at_source: None,
                source_identifier: None,
                media_span: None,
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "project": {"clips": clips, "warnings": [], "importedTimeline": null},
            "islands": [],
            "unmatched": [],
            "matches": [],
        }))
        .unwrap()
    }

    #[test]
    fn camera_clock_exceptions_name_one_source_each() {
        let root = std::env::temp_dir();
        let a = root.join("cam-a").join("C0003.MP4");
        let b = root.join("cam-b").join("C0003.MP4");
        let c = root.join("cam-a").join("C0004.MP4");
        let paths: Vec<String> = [&a, &b, &c]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let saved = [result(&[&paths[0], &paths[2]]), result(&[&paths[1]])];
        let ids = |names: &[&str]| {
            excepted_clips(
                &saved,
                &names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            )
            .map(|set| {
                let mut ids: Vec<String> = set.into_iter().map(|id| id.0).collect();
                ids.sort();
                ids
            })
            .map_err(|error| match error {
                CliError::Failure(message) => message,
                CliError::Usage => "usage".into(),
            })
        };
        assert_eq!(ids(&["c0004.mp4"]), Ok(vec![format!("id-{}", paths[2])]));
        assert_eq!(ids(&[&paths[1]]), Ok(vec![format!("id-{}", paths[1])]));
        assert_eq!(
            ids(&[&format!("id-{}", paths[0])]),
            Ok(vec![format!("id-{}", paths[0])])
        );
        assert!(ids(&["C0003.MP4"]).unwrap_err().contains("names 2 files"));
        assert!(ids(&["C0009.MP4"]).unwrap_err().contains("matches no clip"));
    }
}
