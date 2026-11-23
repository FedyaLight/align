//! Desktop project commands and the save lifecycle.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::{FutureExt, future::Shared};

use super::{AlignApp, Context, Operation, PathPromptOptions};
use crate::project::{self, Destination, Placements, Preferences, Writer};

type Checkpoint = Shared<futures::channel::oneshot::Receiver<Result<(), String>>>;

pub(super) struct Projects {
    pub preferences: Preferences,
    writer: Writer,
    destination: Option<Destination>,
    pub placements: Option<Arc<Placements>>,
    pub busy: bool,
    pub notice: Option<String>,
    revision: u64,
    saved_revision: u64,
    pending: Option<(u64, Checkpoint)>,
}

impl Projects {
    pub fn new(preferences: Preferences, writer: Writer) -> Self {
        Self {
            preferences,
            writer,
            destination: None,
            placements: None,
            busy: false,
            notice: None,
            revision: 0,
            saved_revision: 0,
            pending: None,
        }
    }

    pub fn clear(&mut self) {
        self.destination = None;
        self.placements = None;
        self.notice = None;
        self.revision = self.revision.wrapping_add(1);
        self.saved_revision = self.revision;
        self.pending = None;
    }
}

impl AlignApp {
    pub(super) fn project_changed(&mut self, cx: &mut Context<Self>) {
        self.projects.revision = self.projects.revision.wrapping_add(1);
        if !self.projects.preferences.disabled
            && let Some(destination) = self.projects.destination.clone()
        {
            drop(self.checkpoint(destination, false, cx));
        }
    }

    pub(super) fn checkpoint_before_sync(&mut self, cx: &mut Context<Self>) -> Option<Checkpoint> {
        self.projects.revision = self.projects.revision.wrapping_add(1);
        if self.projects.preferences.disabled {
            return None;
        }
        let destination = if let Some(destination) = &self.projects.destination {
            destination.clone()
        } else if let Some(folder) = self.projects.preferences.autosave_folder() {
            let destination = Destination::autosave(&folder);
            self.projects.destination = Some(destination.clone());
            destination
        } else {
            self.projects.notice =
                Some("Choose a Project Autosave folder to enable recovery.".into());
            return None;
        };
        Some(self.checkpoint(destination, true, cx))
    }

    fn checkpoint(
        &mut self,
        destination: Destination,
        immediate: bool,
        cx: &mut Context<Self>,
    ) -> Checkpoint {
        let revision = self.projects.revision;
        let session = self.data.project_session();
        let title = session.title();
        let receipt = self
            .projects
            .writer
            .submit(
                destination.clone(),
                session,
                self.projects.placements.clone(),
                immediate,
            )
            .shared();
        self.projects.pending = Some((revision, receipt.clone()));
        let completion = receipt.clone();
        cx.spawn(async move |view, cx| {
            let Ok(result) = completion.await else { return };
            let _ = view.update(cx, |this, cx| {
                let active = this.projects.destination.as_ref() == Some(&destination);
                match result {
                    Ok(()) => {
                        if active && this.projects.revision == revision {
                            this.projects.saved_revision = revision;
                            this.projects.notice = None;
                        }
                        this.remember_project(destination, title, cx);
                    }
                    Err(error) if active => {
                        this.projects.notice = Some(format!("Project autosave failed. {error}"));
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        receipt
    }

    pub(crate) fn refresh_project_menus(&self, cx: &mut Context<Self>) {
        cx.set_menus(crate::app_menus(
            self.data.appearance,
            &self.projects.preferences,
        ));
    }

    fn remember_project(
        &mut self,
        destination: Destination,
        title: String,
        cx: &mut Context<Self>,
    ) {
        let recent = project::Recent::new(destination, title);
        if self.projects.preferences.recent.first() != Some(&recent) {
            self.projects.preferences.remember(recent);
            self.save_project_preferences(cx);
        }
    }

    fn save_project_preferences(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.projects.preferences.save(&crate::settings::file()) {
            self.projects.notice = Some(format!("Could not save project preferences. {error}"));
        }
        self.refresh_project_menus(cx);
        cx.notify();
    }

    pub(crate) fn set_autosave(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let changed = self.projects.preferences.disabled == enabled;
        self.projects.preferences.disabled = !enabled;
        if changed
            && enabled
            && self.projects.revision != self.projects.saved_revision
            && let Some(destination) = self.projects.destination.clone()
        {
            drop(self.checkpoint(destination, false, cx));
        }
        self.save_project_preferences(cx);
    }

    pub(crate) fn choose_autosave_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Project Autosave Folder".into()),
        });
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            let task = cx
                .background_executor()
                .spawn(async move { align_core::model::canonical_path(&folder) });
            let folder = task.await;
            let _ = view.update(cx, |this, cx| match folder {
                Ok(folder) => {
                    this.projects.preferences.set_folder(folder.clone());
                    this.projects.notice = Some(format!(
                        "New projects will autosave to {}.",
                        folder.display()
                    ));
                    this.save_project_preferences(cx);
                }
                Err(error) => {
                    this.data.error =
                        Some(format!("Could not choose the autosave folder. {error}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn pick_project(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open Align Project".into()),
        });
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                let _ = view.update(cx, |this, cx| this.open_project(path, cx));
            }
        })
        .detach();
    }

    fn project_command_available(&self) -> bool {
        !self.projects.busy
            && !self.data.importing()
            && !matches!(
                self.data.operation,
                Operation::Synchronizing | Operation::Exporting | Operation::Repairing
            )
    }

    pub(crate) fn open_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.project_command_available() {
            self.projects.notice =
                Some("Wait for the current operation before opening a project.".into());
            cx.notify();
            return;
        }
        if !project::is_project(&path) {
            self.data.error = Some("Choose an Align project ending in .align.".into());
            cx.notify();
            return;
        }
        let dirty = self.projects.revision != self.projects.saved_revision;
        let pending = self
            .projects
            .pending
            .as_ref()
            .filter(|(revision, _)| *revision == self.projects.revision)
            .map(|(_, receipt)| receipt.clone());
        let writer = self.projects.writer.clone();
        self.projects.busy = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .spawn(async move { writer.flush() })
                .await;
            let was_saved = if let Some(receipt) = pending {
                matches!(receipt.await, Ok(Ok(())))
            } else {
                false
            };
            if dirty && !was_saved {
                let response = cx
                    .update(|cx| {
                        let window = cx
                            .active_window()
                            .or_else(|| cx.windows().into_iter().next())?;
                        window
                            .update(cx, |_, window, cx| {
                                window.prompt(
                                    gpui::PromptLevel::Warning,
                                    "Open another project?",
                                    Some("There are unsaved changes to the current project."),
                                    &["Cancel", "Discard Changes"],
                                    cx,
                                )
                            })
                            .ok()
                    })
                    .ok()
                    .flatten();
                let discard = if let Some(response) = response {
                    matches!(response.await, Ok(1))
                } else {
                    false
                };
                if !discard {
                    let _ = view.update(cx, |this, cx| {
                        this.projects.busy = false;
                        cx.notify();
                    });
                    return;
                }
            }
            let _ = view.update(cx, |this, cx| {
                this.projects.notice = Some("Opening project…".into());
                cx.notify();
            });
            let outcome = cx
                .background_executor()
                .spawn(async move { project::open(&path) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.projects.busy = false;
                this.projects.notice = None;
                match outcome {
                    Ok(opened) => {
                        this.projects.clear();
                        this.timeline_transitions.clear();
                        this.clear_export_select();
                        this.clear_settings_select();
                        this.data.restore_project(
                            opened.session,
                            opened.results,
                            opened.changed,
                            opened.embedded_unavailable,
                        );
                        this.export_inputs
                            .set_labels(&this.data.export_baseline().labels, cx);
                        this.projects.placements = opened.placements;
                        this.projects.destination = Some(opened.destination.clone());
                        this.remember_project(opened.destination, opened.title, cx);
                    }
                    Err(error) => {
                        this.data.error = Some(format!("Could not open project. {error}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn save_project(&mut self, save_as: bool, cx: &mut Context<Self>) {
        if !self.project_command_available()
            || self.data.inputs.is_empty()
            || !self.data.sequence_pickers.is_empty()
        {
            return;
        }
        if !save_as
            && let Some(destination) = self.projects.destination.as_ref()
            && !destination.managed
        {
            self.write_manual_project(destination.path.clone(), cx);
            return;
        }
        let title = format!("{}.align", self.data.project_title());
        let directory = self
            .projects
            .destination
            .as_ref()
            .and_then(|destination| destination.path.parent().map(Path::to_path_buf))
            .filter(|path| path.is_dir())
            .or_else(dirs::document_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        self.projects.busy = true;
        cx.notify();
        let receiver = cx.prompt_for_new_path(&directory, Some(&title));
        cx.spawn(async move |view, cx| {
            let selected = receiver.await;
            let _ = view.update(cx, |this, cx| {
                this.projects.busy = false;
                if let Ok(Ok(Some(path))) = selected {
                    if project::is_project(&path) {
                        this.write_manual_project(path, cx);
                    } else {
                        this.data.error = Some("Project filenames must end in .align.".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn write_manual_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.projects.busy = true;
        cx.notify();
        let writer = self.projects.writer.clone();
        let session = self.data.project_session();
        let placements = self.projects.placements.clone();
        let title = session.title();
        cx.spawn(async move |view, cx| {
            let task = cx.background_executor().spawn(async move {
                let path = project::manual_path(&path).map_err(|error| error.to_string())?;
                let destination = Destination {
                    path,
                    managed: false,
                };
                writer
                    .submit(destination.clone(), session, placements, true)
                    .await
                    .map_err(|error| error.to_string())??;
                Ok::<_, String>(destination)
            });
            let outcome = task.await;
            let _ = view.update(cx, |this, cx| {
                this.projects.busy = false;
                match outcome {
                    Ok(destination) => {
                        this.projects.saved_revision = this.projects.revision;
                        this.projects.pending = None;
                        this.projects.notice = None;
                        this.projects.destination = Some(destination.clone());
                        this.data.status =
                            format!("Saved project to {}.", destination.path.display());
                        this.remember_project(destination, title, cx);
                    }
                    Err(error) => {
                        this.data.error = Some(format!("Could not save project. {error}"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
