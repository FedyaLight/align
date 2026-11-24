//! The editable part of a project, independent of the currently displayed result.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSession {
    pub inputs: Vec<PathBuf>,
    timeline_inputs: HashSet<PathBuf>,
    folders: HashSet<PathBuf>,
    timeline_choices: HashMap<PathBuf, Vec<usize>>,
    common: SyncDefaults,
    sequences: HashMap<usize, SequenceDefaults>,
    tracks: HashMap<usize, TrackOverrides>,
    constraints: HashMap<usize, Vec<SyncConstraint>>,
    /// Clips each sequence places by camera clock. Projects without it,
    /// like new ones, place none.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    camera_clock: HashMap<usize, BTreeSet<ClipId>>,
    manual_relinks: Vec<(String, PathBuf)>,
    omit_extensions: Vec<String>,
    prefer_proxies: bool,
    dirty: bool,
    quality_only_dirty: bool,
    active_sequence: usize,
    stages: Vec<Option<usize>>,
}

impl AppData {
    pub(crate) fn project_title(&self) -> String {
        project_title(&self.inputs, &self.timeline_inputs)
    }

    pub(super) fn relink_project_source(&mut self, filename: &str, replacement: &Path) {
        let mut missing = self.restore_paths.iter().filter(|path| {
            path.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(filename))
        });
        let Some(old) = missing.next().cloned() else {
            return;
        };
        if missing.next().is_some() || old == replacement {
            return;
        }
        let folder = self.folder_inputs.contains(&old) && !self.timeline_inputs.contains(&old);
        if (folder || align_decode::timeline::is_bundle(&old)) != replacement.is_dir() {
            return;
        }
        let Ok(replacement) = align_core::model::canonical_path(replacement) else {
            return;
        };
        if let Some(input) = self.inputs.iter_mut().find(|input| **input == old) {
            input.clone_from(&replacement);
        } else if !old.exists()
            && self
                .folder_inputs
                .iter()
                .any(|folder| old.starts_with(folder))
        {
            // A folder scan can no longer discover this child. Keep the
            // folder for its remaining media and add the relocated file.
            if !self.inputs.contains(&replacement) {
                self.inputs.push(replacement.clone());
            }
        }
        if self.timeline_inputs.remove(&old) {
            self.timeline_inputs.insert(replacement.clone());
        }
        if self.folder_inputs.remove(&old) {
            self.folder_inputs.insert(replacement.clone());
        }
        if let Some(choices) = self.timeline_choices.remove(&old) {
            self.timeline_choices.insert(replacement.clone(), choices);
        }
        for (position, result) in self.sequence_results.iter().enumerate() {
            let sequence = self.sequence_key_for_position(position);
            for clip in &result.project.clips {
                let destination = if clip.url == old {
                    replacement.clone()
                } else if folder {
                    let Ok(relative) = clip.url.strip_prefix(&old) else {
                        continue;
                    };
                    replacement.join(relative)
                } else {
                    continue;
                };
                // The pipeline will use this ID only if the replacement has
                // the same duration. A different take needs new corrections.
                let next_id = align_decode::pipeline::clip_id_for(
                    &destination.to_string_lossy(),
                    clip.duration.value,
                );
                if let Some(constraints) = self.sequence_constraints.get_mut(&sequence) {
                    for constraint in constraints {
                        constraint.relink_clip(&clip.id, &next_id);
                    }
                }
                if let Some(chosen) = self.camera_clock_choices.get_mut(&sequence)
                    && chosen.remove(&clip.id)
                {
                    chosen.insert(next_id.clone());
                }
                if let Some(tracks) = self.track_overrides.get_mut(&sequence) {
                    tracks.search.relink_clip(&clip.id, &next_id);
                    tracks.audio.relink_clip(&clip.id, &next_id);
                    tracks.temporal.relink_clip(&clip.id, &next_id);
                    tracks.thresholds.relink_clip(&clip.id, &next_id);
                    tracks.orders.relink_clip(&clip.id, &next_id);
                    tracks.contents.relink_clip(&clip.id, &next_id);
                }
            }
        }
    }

    pub(crate) fn project_session(&self) -> ProjectSession {
        ProjectSession {
            inputs: self.inputs.clone(),
            timeline_inputs: self.timeline_inputs.clone(),
            folders: self.folder_inputs.clone(),
            timeline_choices: self.timeline_choices.clone(),
            common: self.common_settings,
            sequences: self.sequence_settings.clone(),
            tracks: self.track_overrides.clone(),
            constraints: self.sequence_constraints.clone(),
            camera_clock: self.camera_clock_choices.clone(),
            manual_relinks: self.manual_relinks.clone(),
            omit_extensions: self.omit_extensions.clone(),
            prefer_proxies: self.prefer_proxies,
            dirty: self.sync_dirty || self.settings_changed || self.pending_count > 0,
            quality_only_dirty: self.quality_only_dirty
                && !self.settings_changed
                && self.pending_count == 0,
            active_sequence: self.active_sequence_result,
            stages: self
                .sequence_results
                .iter()
                .map(|r| r.selected_stage)
                .collect(),
        }
    }

    pub(crate) fn restore_project(
        &mut self,
        session: ProjectSession,
        mut results: Vec<SyncResult>,
        changed: Vec<PathBuf>,
        embedded_unavailable: bool,
    ) {
        self.clear();
        for (result, stage) in results.iter_mut().zip(session.stages) {
            if let Some(stage) = stage
                && result.selected_stage != Some(stage)
            {
                result.select_stage(stage);
            }
        }
        self.inputs = session.inputs;
        self.timeline_inputs = session.timeline_inputs;
        self.folder_inputs = session.folders;
        self.timeline_choices = session.timeline_choices;
        self.common_settings = session.common;
        self.sequence_settings = session.sequences;
        self.track_overrides = session.tracks;
        self.sequence_constraints = session.constraints;
        self.camera_clock_choices = session.camera_clock;
        self.manual_relinks = session.manual_relinks;
        self.omit_extensions = session.omit_extensions;
        self.prefer_proxies = session.prefer_proxies;
        if !results.is_empty() {
            self.active_sequence_result = session.active_sequence;
            self.apply_results(results);
        }
        self.sync_dirty = session.dirty || !changed.is_empty() || embedded_unavailable;
        self.quality_only_dirty =
            session.quality_only_dirty && changed.is_empty() && !embedded_unavailable;
        self.restore_paths = changed;
        self.restore_warnings = self
            .restore_paths
            .iter()
            .map(|path| {
                format!(
                    "Source moved or changed since synchronization: {}",
                    path.display()
                )
            })
            .collect();
        if embedded_unavailable {
            self.restore_warnings.push(
                "Embedded AAF audio could not be restored. Synchronize again to extract it.".into(),
            );
        }
        if self.result.is_none() || self.sync_dirty {
            let known: HashSet<_> = self
                .sequence_results
                .iter()
                .flat_map(|result| &result.project.clips)
                .map(|clip| &clip.url)
                .collect();
            for path in &self.inputs {
                let expanded_folder = self.folder_inputs.contains(path)
                    && known.iter().any(|clip| clip.starts_with(path));
                let imported_timeline = self.timeline_inputs.contains(path)
                    && self
                        .sequence_results
                        .iter()
                        .any(|result| result.project.imported_timeline.is_some());
                if !known.contains(path) && !expanded_folder && !imported_timeline {
                    self.clips.push(ClipRow {
                        clip_id: None,
                        url: path.clone(),
                        name: file_name(path),
                        kind: None,
                        duration: None,
                        timecode: None,
                        state: ClipState::Queued,
                    });
                }
            }
        }
        self.status = if !self.restore_warnings.is_empty() {
            "Project opened. Check the source media and synchronize again.".into()
        } else if self.result.is_none() {
            "Project opened. Ready to synchronize.".into()
        } else if self.sync_dirty {
            "Previous timeline restored. Synchronize again to apply project changes.".into()
        } else {
            "Synchronized timeline restored. Ready to export.".into()
        };
    }
}

impl ProjectSession {
    pub fn title(&self) -> String {
        project_title(&self.inputs, &self.timeline_inputs)
    }

    pub fn valid_paths(&self, valid: impl Fn(&Path) -> bool) -> bool {
        self.inputs.iter().all(|path| valid(path))
            && self
                .timeline_inputs
                .iter()
                .all(|path| self.inputs.contains(path))
            && self.manual_relinks.iter().all(|(_, path)| valid(path))
    }

    pub fn timeline_inputs(&self) -> impl Iterator<Item = &Path> {
        self.timeline_inputs.iter().map(PathBuf::as_path)
    }
}

fn project_title(inputs: &[PathBuf], timelines: &HashSet<PathBuf>) -> String {
    inputs
        .iter()
        .find(|path| timelines.contains(*path))
        .or_else(|| inputs.first())
        .and_then(|path| path.file_stem())
        .map_or_else(
            || "Untitled".into(),
            |name| name.to_string_lossy().into_owned(),
        )
}

impl<T> ScopedOverride<T> {
    fn relink_clip(&mut self, old: &ClipId, new: &ClipId) {
        if let Some(value) = self.by_clip.remove(old) {
            self.by_clip.insert(new.clone(), value);
        }
    }
}
