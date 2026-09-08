//! GPUI views: faithful layout port of the SwiftUI app.
//!
//! Structure mirrors `ContentView`: top actions, main content (drop zone →
//! source list → timeline preview), warning banner, bottom status/zoom bar,
//! the export sidebar, and modal alerts and settings.
//! Interactive elements carry `.id()` and dispatch via `cx.listener`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use align_core::{
    AudioAnalysisSource, Clip, ClipOrder, MatchPreview, MatchThreshold, MediaKind, SyncResult,
    TemporalMode, TrackContent, model::file_name,
};
use align_decode::export::ExportArtifact;
use align_decode::pipeline::{Phase, Pipeline};
use futures::StreamExt;
use gpui::{
    AnchoredPositionMode, Animation, AnimationExt, AnyView, App, ClickEvent, Context, Corner, Div,
    DragMoveEvent, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, PathPromptOptions, Pixels, Point, Render,
    ScrollHandle, SharedString, Stateful, Styled, Window, anchored, deferred, div, point,
    prelude::*, px, rgb, rgba,
};

use super::icons::{icons, kind_badge, svg_icon};
use super::lane::CorrectionOption;
use super::state::{AppData, ClipState, MenuTarget, Operation, SequencePicker, SettingsScope};
use super::text_input::TextInput;
use super::theme::{Theme, ThemeMode};
use crate::{MinimizeWindow, ToggleFullscreen, ZoomWindow};

// ------------------------------------------------------------ messages

/// Strong UI ease-out: cubic-bezier(0.23, 1, 0.32, 1).
/// Solve the time coordinate before evaluating opacity; input is not the
/// Bezier parameter. Opacity-only motion is also the reduced-motion variant.
fn motion_ease(time: f32) -> f32 {
    let time = time.clamp(0.0, 1.0);
    if time == 0.0 || time == 1.0 {
        return time;
    }
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let x = 3.0 * (1.0 - t).powi(2) * t * 0.23 + 3.0 * (1.0 - t) * t * t * 0.32 + t * t * t;
        if x < time {
            low = t;
        } else {
            high = t;
        }
    }
    1.0 - (1.0 - (low + high) * 0.5).powi(3)
}

fn entrance(duration_ms: u64) -> Animation {
    Animation::new(Duration::from_millis(duration_ms)).with_easing(motion_ease)
}

fn slide_in<E: IntoElement + Styled + 'static>(
    child: E,
    id: impl Into<gpui::ElementId>,
    delay_ms: u64,
) -> impl IntoElement {
    let reduced = super::motion::reduced_motion();
    super::motion::Slide {
        child: Some(child),
        x: 0.,
        y: 0.,
    }
    .with_animation(
        id,
        Animation::new(Duration::from_millis(280 + delay_ms)),
        move |mut el, elapsed| {
            let progress =
                ((elapsed * (280 + delay_ms) as f32 - delay_ms as f32) / 280.).clamp(0., 1.);
            let eased = motion_ease(progress);
            el.y = if reduced { 0. } else { 24. * (1. - eased) };
            el.child = el.child.map(|child| child.opacity(eased));
            el
        },
    )
}

fn slide_in_from_right<E: IntoElement + Styled + 'static>(
    child: E,
    id: impl Into<gpui::ElementId>,
    distance: f32,
    closing: bool,
) -> impl IntoElement {
    let reduced = super::motion::reduced_motion();
    div()
        .h_full()
        .flex_shrink_0()
        .overflow_hidden()
        // Put the shadow on the animated viewport. A shadow on the panel
        // itself is clipped by this viewport and never reaches the timeline.
        .shadow_md()
        .child(child)
        .with_animation(
            id,
            entrance(if closing { 180 } else { 280 }),
            move |el, progress| {
                let visible = if closing { 1. - progress } else { progress };
                let visible = if reduced {
                    if closing { 0. } else { 1. }
                } else {
                    visible
                };
                el.w(px(distance * visible))
            },
        )
}

fn quality_dropdown_motion<E: IntoElement + Styled + 'static>(
    child: E,
    closing: bool,
) -> impl IntoElement {
    const MENU_HEIGHT: f32 = 188.;
    let reduced = super::motion::reduced_motion();
    div()
        .w(px(160.))
        .overflow_hidden()
        .rounded_lg()
        .shadow_md()
        .child(child)
        .with_animation(
            if closing {
                "search-quality-dropdown-out"
            } else {
                "search-quality-dropdown-in"
            },
            entrance(if closing { 150 } else { 180 }),
            move |el, progress| {
                let visible = if reduced {
                    if closing { 0. } else { 1. }
                } else if closing {
                    1. - progress
                } else {
                    progress
                };
                el.h(px(MENU_HEIGHT * visible)).opacity(visible)
            },
        )
}

fn selection_control_motion<E: IntoElement + Styled + 'static>(
    child: E,
    id: SharedString,
    closing: bool,
) -> impl IntoElement {
    let reduced = super::motion::reduced_motion();
    div()
        .h(px(16.))
        .flex_shrink_0()
        .overflow_hidden()
        .child(child)
        .with_animation(
            id,
            entrance(if closing { 130 } else { 160 }),
            move |el, progress| {
                let visible = if reduced {
                    if closing { 0. } else { 1. }
                } else if closing {
                    1. - progress
                } else {
                    progress
                };
                el.w(px(28. * visible)).opacity(visible)
            },
        )
}

#[cfg(test)]
mod motion_tests {
    use super::*;

    #[test]
    fn entrance_is_bounded_monotonic_and_settles() {
        assert_eq!(motion_ease(0.0), 0.0);
        assert_eq!(motion_ease(1.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let opacity = motion_ease(step as f32 / 100.0);
            assert!((previous..=1.0).contains(&opacity));
            previous = opacity;
        }
        assert!(motion_ease(0.5) > 0.9);
        assert!(entrance(200).oneshot);
    }
}

enum SyncMsg {
    SequenceStart {
        sequence_index: usize,
        sequence_count: usize,
    },
    Progress(Box<SyncProgress>),
    Done(Box<Result<Vec<SyncResult>, String>>),
}

struct SyncProgress {
    phase: Phase,
    completed: usize,
    total: usize,
    current: Option<PathBuf>,
    discovered: Option<Clip>,
    preview: Option<MatchPreview>,
    sequence_index: usize,
    sequence_count: usize,
}

enum ExportMsg {
    Progress { label: String, fraction: f32 },
    Done(Result<Vec<ExportArtifact>, String>),
}

enum PathRepairMsg {
    Done {
        destination: PathBuf,
        result: Result<usize, String>,
    },
}

// ------------------------------------------------------------ view

#[derive(Clone)]
struct ExportInputs {
    sequence_name: Entity<TextInput>,
    synced_symbol: Entity<TextInput>,
    synced_color: Entity<TextInput>,
    synced_role: Entity<TextInput>,
    unmatched_symbol: Entity<TextInput>,
    unmatched_color: Entity<TextInput>,
    unmatched_role: Entity<TextInput>,
}

#[derive(Clone)]
struct PathFixerInputs {
    old_folder: Entity<TextInput>,
    omit_extensions: Entity<TextInput>,
}

impl PathFixerInputs {
    fn new(cx: &mut Context<AlignApp>) -> Self {
        Self {
            old_folder: cx.new(|cx| TextInput::new(cx, "/old/media/folder")),
            omit_extensions: cx.new(|cx| TextInput::new(cx, "jpg, png")),
        }
    }

    fn any_focused(&self, window: &Window, cx: &App) -> bool {
        self.old_folder.read(cx).is_focused(window)
            || self.omit_extensions.read(cx).is_focused(window)
    }
}

impl ExportInputs {
    fn new(cx: &mut Context<AlignApp>) -> Self {
        Self {
            sequence_name: cx.new(|cx| TextInput::new(cx, "Keep source name")),
            synced_symbol: cx.new(|cx| TextInput::new(cx, "e.g. [SYNCED]")),
            synced_color: cx.new(|cx| TextInput::new(cx, "e.g. Green")),
            synced_role: cx.new(|cx| TextInput::new(cx, "e.g. Dialogue")),
            unmatched_symbol: cx.new(|cx| TextInput::new(cx, "e.g. [UNSYNCED]")),
            unmatched_color: cx.new(|cx| TextInput::new(cx, "e.g. Rose")),
            unmatched_role: cx.new(|cx| TextInput::new(cx, "e.g. Dialogue")),
        }
    }

    fn any_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.sequence_name,
            &self.synced_symbol,
            &self.synced_color,
            &self.synced_role,
            &self.unmatched_symbol,
            &self.unmatched_color,
            &self.unmatched_role,
        ]
        .into_iter()
        .any(|input| input.read(cx).is_focused(window))
    }

    fn value(input: &Entity<TextInput>, cx: &App) -> Option<String> {
        let value = input.read(cx).text();
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    }
}

pub struct AlignApp {
    pub(crate) data: AppData,
    pub(crate) focus_handle: FocusHandle,
    export_inputs: ExportInputs,
    path_fixer_inputs: PathFixerInputs,
    pan_origin: Option<Point<Pixels>>,
    export_scroll: ScrollHandle,
    export_scroll_drag: Option<(f32, f32)>,
    export_sidebar_closing: bool,
    search_quality_closing: bool,
    selection_controls_closing: bool,
}

impl Focusable for AlignApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AlignApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            data: AppData::new(),
            focus_handle: cx.focus_handle(),
            export_inputs: ExportInputs::new(cx),
            path_fixer_inputs: PathFixerInputs::new(cx),
            pan_origin: None,
            export_scroll: ScrollHandle::new(),
            export_scroll_drag: None,
            export_sidebar_closing: false,
            search_quality_closing: false,
            selection_controls_closing: false,
        }
    }

    // ---------------- actions

    pub(crate) fn add_media(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                this.data.add_paths(paths);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn start_sync(&mut self, cx: &mut Context<Self>) {
        self.search_quality_closing = false;
        self.selection_controls_closing = false;
        self.export_sidebar_closing = false;
        if !self.data.begin_sync_run() {
            return;
        }
        cx.notify();

        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let input_sets = self.data.pipeline_input_sets();
        let run_settings: Vec<_> = (0..input_sets.len())
            .map(|position| {
                let sequence_key = self.data.sequence_key_for_position(position);
                (
                    self.data.constraints_for_sequence(sequence_key),
                    self.data.pipeline_options_for_run(position),
                    self.data.quality_retry_targets_for_position(position),
                    self.data.quality_retry_urls_for_position(position),
                )
            })
            .collect();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<SyncMsg>();
        std::thread::spawn(move || {
            let pipeline = Pipeline::default_backend();
            let sequence_count = input_sets.len();
            let mut results = Vec::with_capacity(sequence_count);
            for (sequence_index, (inputs, (constraints, options, targets, target_urls))) in
                input_sets.into_iter().zip(run_settings).enumerate()
            {
                let _ = tx.unbounded_send(SyncMsg::SequenceStart {
                    sequence_index,
                    sequence_count,
                });
                let progress = |mut p: align_decode::pipeline::PipelineProgress| {
                    if targets.is_some() {
                        p.discovered = None;
                        if matches!(p.phase, Phase::Inspect | Phase::Fingerprint)
                            && p.current
                                .as_ref()
                                .is_some_and(|url| !target_urls.contains(url))
                        {
                            p.current = None;
                        }
                        p.preview = None;
                    }
                    let _ = tx.unbounded_send(SyncMsg::Progress(Box::new(SyncProgress {
                        phase: p.phase,
                        completed: p.completed,
                        total: p.total,
                        current: p.current,
                        discovered: p.discovered,
                        preview: p.preview,
                        sequence_index,
                        sequence_count,
                    })));
                };
                match pipeline.synchronize(
                    &inputs,
                    &constraints,
                    &options,
                    Some(&progress),
                    &cancel,
                ) {
                    Ok(result) => results.push(result),
                    Err(error) => {
                        let _ = tx.unbounded_send(SyncMsg::Done(Box::new(Err(error.to_string()))));
                        return;
                    }
                }
            }
            let _ = tx.unbounded_send(SyncMsg::Done(Box::new(Ok(results))));
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            while let Some(msg) = rx.next().await {
                let finished = matches!(msg, SyncMsg::Done(_));
                let _ = view.update(cx, |this, cx| {
                    if this.data.generation == generation {
                        this.apply_sync_msg(msg);
                        cx.notify();
                    }
                });
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    fn choose_missing_media(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                if this.data.add_manual_relinks(paths) == 0 {
                    return;
                }
                this.data.show_warning_details = false;
                this.start_sync(cx);
            });
        })
        .detach();
    }

    fn apply_sync_msg(&mut self, msg: SyncMsg) {
        match msg {
            SyncMsg::SequenceStart {
                sequence_index,
                sequence_count,
            } => {
                self.data.begin_sequence_progress();
                self.data.status = format!(
                    "Sequence {}/{} · Reading metadata…",
                    sequence_index + 1,
                    sequence_count
                );
                self.data.progress = sequence_index as f32 / sequence_count as f32;
            }
            SyncMsg::Progress(event) => {
                let phase_name = match event.phase {
                    Phase::Inspect => "inspect",
                    Phase::Fingerprint => "fingerprint",
                    Phase::Match => "match",
                    Phase::Refine => "refine",
                    Phase::Solve => "solve",
                };
                let current_name = event
                    .current
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(str::to_string);
                self.data.apply_progress(
                    phase_name,
                    event.completed,
                    event.total,
                    current_name.as_deref(),
                    event.discovered.as_ref(),
                    event.preview.as_ref(),
                );
                if event.sequence_count > 1 {
                    self.data.status = format!(
                        "Sequence {}/{} · {}",
                        event.sequence_index + 1,
                        event.sequence_count,
                        self.data.status
                    );
                    self.data.progress = (event.sequence_index as f32 + self.data.progress)
                        / event.sequence_count as f32;
                }
            }
            SyncMsg::Done(boxed) => match *boxed {
                Ok(results) => self.data.apply_results(results),
                Err(error) => {
                    if error == "Cancelled." {
                        self.data.restore_after_sync_cancel();
                    } else {
                        self.data.operation = Operation::Idle;
                        self.data.error = Some(error);
                        self.data.status = "Synchronization failed.".to_string();
                    }
                }
            },
        }
    }

    pub(crate) fn start_export_sheet(&mut self, cx: &mut Context<Self>) {
        if !self.data.can_export() {
            return;
        }
        self.data.show_export = true;
        self.export_sidebar_closing = false;
        self.data.show_search_quality = false;
        self.search_quality_closing = false;
        self.data.export_started = false;
        self.data.error = None;
        self.export_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn close_search_quality(&mut self, cx: &mut Context<Self>) {
        if !self.data.show_search_quality {
            return;
        }
        self.data.show_search_quality = false;
        self.search_quality_closing = true;
        cx.notify();

        let timer = cx.background_executor().timer(Duration::from_millis(150));
        cx.spawn(async move |view, cx| {
            timer.await;
            let _ = view.update(cx, |this, cx| {
                if this.search_quality_closing {
                    this.search_quality_closing = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn close_export_sidebar(&mut self, cx: &mut Context<Self>) {
        if !self.data.show_export || self.export_sidebar_closing {
            return;
        }
        self.export_sidebar_closing = true;
        cx.notify();

        let timer = cx.background_executor().timer(Duration::from_millis(180));
        cx.spawn(async move |view, cx| {
            timer.await;
            let _ = view.update(cx, |this, cx| {
                if this.export_sidebar_closing {
                    this.data.show_export = false;
                    this.export_sidebar_closing = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn close_selection_controls(&mut self, cx: &mut Context<Self>) {
        self.selection_controls_closing = true;
        cx.notify();

        let timer = cx.background_executor().timer(Duration::from_millis(130));
        cx.spawn(async move |view, cx| {
            timer.await;
            let _ = view.update(cx, |this, cx| {
                if this.selection_controls_closing {
                    this.selection_controls_closing = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn finish_search_settings(&mut self, cx: &mut Context<Self>) {
        self.data.show_search_settings = false;
        let changed = std::mem::take(&mut self.data.settings_changed);
        if changed {
            self.data.mark_sync_dirty();
            self.start_sync(cx);
        } else {
            cx.notify();
        }
    }

    pub(crate) fn open_path_fixer(&mut self, cx: &mut Context<Self>) {
        self.data.discard_path_redirection_edits();
        self.data.path_fixer_prefer_proxies = self.data.prefer_proxies;
        let omitted = self.data.omit_extensions.join(", ");
        self.path_fixer_inputs
            .omit_extensions
            .update(cx, |input, cx| input.set_text(omitted, cx));
        self.data.show_export = false;
        self.export_sidebar_closing = false;
        self.data.show_path_fixer = true;
        self.data.error = None;
        self.data.menu = None;
        cx.notify();
    }

    fn choose_redirection_dir(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(mut paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.pop() else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                this.data.path_fixer_dir = Some(path);
                cx.notify();
            });
        })
        .detach();
    }

    fn add_path_redirection(&mut self, cx: &mut Context<Self>) {
        let old = self.path_fixer_inputs.old_folder.read(cx).text();
        let Some(new) = self.data.path_fixer_dir.clone() else {
            self.data.error = Some("Choose the replacement folder.".to_string());
            cx.notify();
            return;
        };
        match self.data.add_path_redirection(&old, new) {
            Ok(()) => self
                .path_fixer_inputs
                .old_folder
                .update(cx, |input, cx| input.set_text("", cx)),
            Err(error) => self.data.error = Some(error),
        }
        cx.notify();
    }

    fn apply_path_fixer_settings(&mut self, cx: &App) {
        let omitted = self.path_fixer_inputs.omit_extensions.read(cx).text();
        self.data.set_omit_extensions(&omitted);
        self.data.prefer_proxies = self.data.path_fixer_prefer_proxies;
        self.data.save_path_redirections();
    }

    fn save_fixed_project_copy(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.data.path_repair_source().map(PathBuf::from) else {
            self.data.error = Some("Add an XML, FCPXML, or AAF project first.".into());
            cx.notify();
            return;
        };
        self.apply_path_fixer_settings(cx);
        let stem = source
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("xml")
            .to_string();
        let suggested = format!("{stem}-fixed.{extension}");
        let directory = source.parent().unwrap_or_else(|| std::path::Path::new("."));
        let receiver = cx.prompt_for_new_path(directory, Some(&suggested));
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(mut destination))) = receiver.await else {
                return;
            };
            if destination.extension().is_none() {
                destination.set_extension(extension);
            }
            let _ = view.update(cx, |this, cx| {
                this.begin_fixed_project_copy(destination, cx);
            });
        })
        .detach();
    }

    fn begin_fixed_project_copy(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
        if !self.data.begin_path_repair() {
            return;
        }
        self.data.path_fixer_dir = None;
        self.data.show_path_fixer = false;
        let inputs = self.data.path_repair_inputs();
        let options = self.data.path_repair_options();
        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<PathRepairMsg>();
        std::thread::spawn(move || {
            let pipeline = Pipeline::default_backend();
            let result = pipeline
                .write_fixed_timeline_copy(&inputs, &options, &destination, &cancel)
                .map_err(|error| error.to_string());
            let _ = tx.unbounded_send(PathRepairMsg::Done {
                destination,
                result,
            });
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            if let Some(PathRepairMsg::Done {
                destination,
                result,
            }) = rx.next().await
            {
                let _ = view.update(cx, |this, cx| {
                    if this.data.generation != generation {
                        return;
                    }
                    this.data.operation = if this.data.has_result() {
                        Operation::Ready
                    } else {
                        Operation::Idle
                    };
                    match result {
                        Ok(count) => {
                            this.data.exported_files = vec![destination];
                            this.data.progress = 1.0;
                            this.data.status = format!(
                                "Fixed project copy saved with {count} repaired media path{}.",
                                if count == 1 { "" } else { "s" }
                            );
                        }
                        Err(error) if error == "Cancelled." => {
                            this.data.status = "Project repair cancelled.".into();
                        }
                        Err(error) => {
                            this.data.error = Some(error);
                            this.data.status = "Project repair failed.".into();
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_path_fixer(&mut self, cx: &mut Context<Self>) {
        self.apply_path_fixer_settings(cx);
        self.data.path_fixer_dir = None;
        self.data.show_path_fixer = false;
        if self
            .data
            .inputs
            .iter()
            .any(|path| align_decode::timeline::is_supported(path))
            && self.data.can_synchronize()
        {
            self.start_sync(cx);
        } else {
            cx.notify();
        }
    }

    fn choose_export_dir(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(mut paths))) = receiver.await else {
                return;
            };
            let Some(dir) = paths.pop() else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                this.data.export_dir = Some(dir);
                cx.notify();
            });
        })
        .detach();
    }

    fn begin_export(&mut self, cx: &mut Context<Self>) {
        let (Some(dir), formats) = (self.data.export_dir.clone(), self.data.export_targets())
        else {
            return;
        };
        if !self.data.can_begin_export() {
            return;
        }
        let Some(result) = self.data.result.clone() else {
            return;
        };
        use align_core::export_model::{CutRemoveOptions, ExportAssemblyOptions, ExportTimeline};
        let sequence_name = ExportInputs::value(&self.export_inputs.sequence_name, cx);
        let synced_symbol = ExportInputs::value(&self.export_inputs.synced_symbol, cx);
        let synced_color = ExportInputs::value(&self.export_inputs.synced_color, cx);
        let synced_role = ExportInputs::value(&self.export_inputs.synced_role, cx);
        let unmatched_symbol = ExportInputs::value(&self.export_inputs.unmatched_symbol, cx);
        let unmatched_color = ExportInputs::value(&self.export_inputs.unmatched_color, cx);
        let unmatched_role = ExportInputs::value(&self.export_inputs.unmatched_role, cx);
        let results = if self.data.sequence_results.is_empty() {
            vec![result]
        } else {
            self.data.sequence_results.clone()
        };
        let result_count = results.len();
        let mut timelines = Vec::with_capacity(result_count);
        for (index, result) in results.iter().enumerate() {
            let options = ExportAssemblyOptions {
                unmatched: self.data.export_unmatched,
                prevent_group_overlaps: self.data.export_prevent_overlaps,
                disable_unmatched: self.data.export_disable_unmatched,
                label_synced: self.data.export_label_synced,
                label_unmatched: self.data.export_label_unmatched,
                cut_remove: CutRemoveOptions {
                    common_gaps: self.data.export_cut_common_gaps,
                    lone_recorder: self.data.export_cut_lone_recorder,
                    shorter_than: self.data.export_cut_shorter_than,
                    trim_starts: self.data.export_trim_starts,
                    trim_ends: self.data.export_trim_ends,
                },
                synced_symbol: synced_symbol.clone(),
                synced_symbol_suffix: self.data.export_synced_symbol_suffix,
                synced_color: synced_color.clone(),
                synced_role: synced_role.clone(),
                unmatched_symbol: unmatched_symbol.clone(),
                unmatched_symbol_suffix: self.data.export_unmatched_symbol_suffix,
                unmatched_color: unmatched_color.clone(),
                unmatched_role: unmatched_role.clone(),
                sequence_name: sequence_name.as_ref().map(|name| {
                    if result_count > 1 {
                        format!("{name} {}", index + 1)
                    } else {
                        name.clone()
                    }
                }),
            };
            let Ok(timeline) = ExportTimeline::from_result_with_options(result, options) else {
                self.data.error = Some(format!("Sequence {} has nothing to export.", index + 1));
                cx.notify();
                return;
            };
            timelines.push(timeline);
        }
        self.data.operation = Operation::Exporting;
        self.data.progress = 0.0;
        self.data.export_started = true;
        self.data.error = None;
        self.data.status = if self.data.export_drift && self.data.has_drift() {
            "Preparing drift-corrected audio…".to_string()
        } else if self.data.export_media {
            "Exporting clean-audio video…".to_string()
        } else if timelines.len() > 1 {
            format!("Writing {} sequences…", timelines.len())
        } else {
            "Writing timelines…".to_string()
        };
        cx.notify();

        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let drift = self.data.export_drift;
        let replaced = self.data.export_replaced;
        let storylines = self.data.export_storylines;
        let fcpxml_timeline = self.data.export_fcpxml_timeline;
        let fcpxml_multicam = self.data.export_fcpxml_multicam;
        let aaf_frame_duration = self.data.export_aaf_frame_duration;
        let media = self.data.export_media;
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<ExportMsg>();
        std::thread::spawn(move || {
            let pipeline = Pipeline::default_backend();
            let mut progress = |p: align_decode::export::ExportJobProgress| {
                let label = match &p.current {
                    Some(path) if p.kind == align_decode::export::ExportJobKind::Media => {
                        format!("Exporting media: {}", file_name(path))
                    }
                    Some(path) => format!("Preparing audio: {}", file_name(path)),
                    None => "Preparing timelines…".to_string(),
                };
                let _ = tx.unbounded_send(ExportMsg::Progress {
                    label,
                    fraction: if p.total > 0 {
                        p.completed as f32 / p.total as f32
                    } else {
                        0.0
                    },
                });
            };
            let out = align_decode::export::export_prepared_many(
                align_decode::export::ExportBatchRequest {
                    backend: pipeline.backend(),
                    timelines: &timelines,
                    directory: &dir,
                    formats: &formats,
                    correct_drift: drift,
                    include_replaced_sequence: replaced,
                    include_media_files: media,
                    aaf_frame_duration,
                    include_fcpxml_timeline: fcpxml_timeline,
                    include_fcpxml_multicam: fcpxml_multicam,
                    group_fcpxml_storylines: storylines,
                    cancel: &cancel,
                },
                Some(&mut progress),
            );
            let _ = tx.unbounded_send(ExportMsg::Done(out.map_err(|e| e.to_string())));
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            while let Some(msg) = rx.next().await {
                let finished = matches!(msg, ExportMsg::Done(_));
                let _ = view.update(cx, |this, cx| {
                    if this.data.generation == generation {
                        this.apply_export_msg(msg);
                        cx.notify();
                    }
                });
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_export_msg(&mut self, msg: ExportMsg) {
        match msg {
            ExportMsg::Progress { label, fraction } => {
                self.data.status = label;
                self.data.progress = fraction;
            }
            ExportMsg::Done(Ok(artifacts)) => {
                self.data.exported_files = artifacts.iter().map(|a| a.url.clone()).collect();
                self.data.operation = Operation::Exported;
                self.data.progress = 1.0;
                self.data.status = "Export complete.".to_string();
            }
            ExportMsg::Done(Err(error)) => {
                if error == "Cancelled." {
                    self.data.operation = Operation::Ready;
                    self.data.status = "Export cancelled.".to_string();
                } else {
                    self.data.operation = Operation::Ready;
                    self.data.error = Some(error);
                    self.data.status = "Export failed.".to_string();
                }
            }
        }
    }

    fn cancel_current(&mut self, cx: &mut Context<Self>) {
        self.data
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        cx.notify();
    }

    // ---------------- corrections

    fn find_another(&mut self, target: &CorrectionOption, cx: &mut Context<Self>) {
        if self.data.reject_alignment(target) {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn reject_pair(&mut self, target: &CorrectionOption, cx: &mut Context<Self>) {
        if self.data.reject_pair(target) {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_stream_source(&mut self, source: Option<AudioAnalysisSource>, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|m| m.lane_id.clone());
        let changed = match lane_id {
            Some(id) => self.data.set_analysis_source(source, &id),
            None => false,
        };
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_temporal_mode(&mut self, mode: Option<TemporalMode>, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|m| m.lane_id.clone());
        let changed = match lane_id {
            Some(id) => self.data.set_temporal_mode(mode, &id),
            None => false,
        };
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_match_threshold(&mut self, threshold: Option<MatchThreshold>, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|m| m.lane_id.clone());
        let changed = match lane_id {
            Some(id) => self.data.set_match_threshold(threshold, &id),
            None => false,
        };
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_clip_order(&mut self, mode: Option<ClipOrder>, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|m| m.lane_id.clone());
        let changed = match lane_id {
            Some(id) => self.data.set_clip_order(mode, &id),
            None => false,
        };
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_track_content(&mut self, mode: Option<TrackContent>, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|m| m.lane_id.clone());
        let changed = match lane_id {
            Some(id) => self.data.set_track_content(mode, &id),
            None => false,
        };
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }

    fn set_preserve_editing(&mut self, preserve: bool, cx: &mut Context<Self>) {
        let lane_id = self.data.menu.clone().and_then(|menu| menu.lane_id);
        let changed = lane_id.is_some_and(|id| self.data.set_preserve_editing(preserve, &id));
        if changed {
            self.data.menu = None;
            self.start_sync(cx);
        }
    }
}

// ------------------------------------------------------------ rendering
//
// Colors come from `Theme::of(window.appearance())` (see `theme.rs`),
// so the whole UI follows the system light/dark appearance. No hardcoded
// dark-only fills live here.

/// Drag payload for the timeline zoom slider (value-less marker type).
#[derive(Clone, Copy)]
struct ZoomDrag;

/// Invisible drag ghost for the zoom slider interaction.
struct ZoomDragGhost;

impl Render for ZoomDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(1.)).h(px(1.)).opacity(0.)
    }
}

/// Hover tooltip: one-line floating label (track sources, icon buttons).
struct Tip {
    text: String,
    theme: Theme,
}

impl Render for Tip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme.border))
            .bg(rgb(theme.panel))
            .text_color(rgb(theme.text))
            .text_xs()
            .child(self.text.clone())
    }
}

fn hover_tip(text: String, theme: &Theme) -> impl Fn(&mut Window, &mut App) -> AnyView + use<> {
    let theme = *theme;
    move |_, cx| -> AnyView {
        cx.new(|_| Tip {
            text: text.clone(),
            theme,
        })
        .into()
    }
}

/// Centered modal overlay for sheets, alerts and popovers. Dimmed
/// full-window layer (intercepts clicks) with the panel in the middle;
/// the dim follows the system appearance via the theme mode.
fn overlay(
    theme: &Theme,
    id: impl Into<SharedString>,
    panel: impl IntoElement,
) -> impl IntoElement {
    let id: SharedString = id.into();
    let dim = match theme.mode {
        ThemeMode::Light => rgba(0x1D1D1F33),
        ThemeMode::Dark => rgba(0x00000080),
    };
    // The layer swallows all mouse input: without stopped propagation a
    // click on the modal would fall through to the content below
    // (e.g. opening the media picker behind the diagnostics panel).
    div()
        .id(id.clone())
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(dim)
        .cursor_default()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(slide_in(
            div().flex().flex_col().max_h_full().child(panel),
            "modal-rise",
            0,
        ))
        .with_animation(id, entrance(250), |el, progress| el.opacity(progress))
}

fn button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let mut el = div()
        .id(id.into())
        .px_3()
        .h(px(28.))
        .flex()
        .items_center()
        .rounded_lg()
        .bg(rgb(theme.button_hover))
        .text_size(px(12.))
        .child(label.into());
    if enabled {
        el = el
            .text_color(rgb(theme.icon))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.border)))
            .active(|this| this.opacity(0.62))
            .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)));
    } else {
        el = el.text_color(rgb(theme.dim)).opacity(0.42);
    }
    el
}

fn prominent_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let mut el = div()
        .id(id.into())
        .px_3()
        .h(px(28.))
        .flex()
        .items_center()
        .rounded_lg()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight(600.0))
        .child(label.into());
    if enabled {
        el = el
            .bg(rgb(theme.accent))
            .text_color(rgb(theme.on_accent))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.accent_hover)))
            .active(|this| this.opacity(0.76))
            .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)));
    } else {
        el = el.text_color(rgb(theme.dim)).opacity(0.42);
    }
    el
}

/// Square icon button matching `button()` chrome (toolbar, zoom bar).
/// The SVG tint inherits `text_color`, so it follows the theme too.
fn icon_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    icon: String,
    tip: &str,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let tip = tip.to_string();
    let mut el = div()
        .id(id.into())
        .w(px(30.))
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg();
    if enabled {
        el = el
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.button_hover)))
            .active(|this| this.opacity(0.62))
            .tooltip(hover_tip(tip, theme))
            .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)));
    } else {
        el = el.text_color(rgb(theme.dim)).opacity(0.32);
    }
    let tint = if enabled { theme.icon } else { theme.dim };
    el.child(svg_icon(icon, 13.0, tint))
}

fn modal_header(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    title: &'static str,
    close_id: &'static str,
    close: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Div {
    div()
        .w_full()
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .child(
            div()
                .text_size(px(16.))
                .font_weight(gpui::FontWeight(600.0))
                .child(title),
        )
        .child(div().flex_1())
        .child(icon_button(
            cx,
            theme,
            close_id,
            icons().close.clone(),
            "Close",
            true,
            close,
        ))
}

fn sidebar_scrim(theme: &Theme, closing: bool) -> impl IntoElement {
    let dim = match theme.mode {
        ThemeMode::Light => rgba(0x1D1D1F33),
        ThemeMode::Dark => rgba(0x00000080),
    };
    div()
        .id("export-sidebar-scrim")
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .bg(dim)
        .cursor_default()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .with_animation(
            if closing {
                "export-sidebar-scrim-out"
            } else {
                "export-sidebar-scrim-in"
            },
            entrance(if closing { 180 } else { 280 }),
            move |el, progress| el.opacity(if closing { 1. - progress } else { progress }),
        )
}

/// Text button with a leading icon (toolbar actions like Add Media).
fn fmt_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

fn fmt_timecode(start: f64, at: f64) -> String {
    let t = start + at;
    let h = (t / 3600.0).floor() as i64 % 24;
    let m = (t.rem_euclid(3600.0) / 60.0).floor() as i64;
    let s = t.rem_euclid(60.0).floor() as i64;
    let f = ((t - t.floor()) * 25.0).floor() as i64 % 25;
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

impl Render for AlignApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = match self.data.appearance {
            Some(ThemeMode::Light) => Theme::light(),
            Some(ThemeMode::Dark) => Theme::dark(),
            None => Theme::of(window.appearance()),
        };
        // Viewport-mapped Fit width (mirrors Swift's GeometryReader:
        // viewport minus label column and content padding).
        let fit_width =
            (f32::from(window.viewport_size().width) - super::lane::LABEL_WIDTH - 16.0).max(100.0);
        // While a modal or context menu is open the content below keeps
        // its layout but loses hover/cursor feedback (clicks are already
        // swallowed by the overlay layers).
        let content_active = self.data.menu.is_none()
            && !self.data.show_path_fixer
            && self.data.error.is_none()
            && self.data.sequence_picker.is_none()
            && !self.data.show_about
            && !self.data.show_search_quality
            && !self.data.show_search_settings
            && !self.data.show_stage_settings
            && !self.data.show_sequence_results
            && !self.data.show_export;
        let mut root = div()
            .id("app-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text))
            .text_sm()
            // Focusable root: without a focused view GPUI key dispatch
            // never reaches the app-level system shortcuts (Cmd+Q etc.).
            .track_focus(&self.focus_handle)
            // Window actions run here (not in app-level handlers): menu and
            // key dispatch already hold the window checked out, so a nested
            // update_window would fail with "window not found".
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| {
                window.minimize_window();
            }))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _| {
                window.zoom_window();
            }))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| {
                window.toggle_fullscreen();
            }))
            // Timeline navigation: arrows pan, Home/End jump unless an
            // export text field owns keyboard focus.
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                let key: &str = &e.keystroke.key;
                if key == "escape"
                    && this.data.show_export
                    && !matches!(this.data.operation, Operation::Exporting)
                {
                    this.close_export_sidebar(cx);
                    return;
                }
                if key == "escape" && this.data.show_search_quality {
                    this.close_search_quality(cx);
                    return;
                }
                if key == "escape" && this.data.show_path_fixer {
                    this.data.discard_path_redirection_edits();
                    this.data.show_path_fixer = false;
                    cx.notify();
                    return;
                }
                if key == "escape" && this.data.show_sequence_results {
                    this.data.show_sequence_results = false;
                    cx.notify();
                    return;
                }
                if this.data.show_export && this.export_inputs.any_focused(window, cx) {
                    return;
                }
                if this.data.show_path_fixer && this.path_fixer_inputs.any_focused(window, cx) {
                    return;
                }
                if this.data.lanes.is_empty() {
                    return;
                }
                let step = if e.keystroke.modifiers.shift {
                    480.0
                } else {
                    120.0
                };
                match key {
                    "left" => this.data.pan_by(step, 0.0),
                    "right" => this.data.pan_by(-step, 0.0),
                    "up" => this.data.pan_by(0.0, step),
                    "down" => this.data.pan_by(0.0, -step),
                    "home" => this.data.pan_to(Some(0.0), None),
                    "end" => this.data.pan_to(
                        Some(-f32::from(this.data.timeline_scroll.max_offset().width)),
                        None,
                    ),
                    _ => return,
                }
                cx.notify();
            }))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.data.add_paths(paths.paths().to_vec());
                cx.notify();
            }));
        let mut content = div()
            .id("primary-content")
            .relative()
            .h_full()
            .min_w(px(0.))
            .flex_1()
            .flex()
            .flex_col();
        // No toolbar over the empty drop zone: nothing to act on yet.
        if !(self.data.lanes.is_empty() && self.data.clips.is_empty()) {
            content = content.child(toolbar(
                cx,
                &theme,
                &self.data,
                content_active,
                self.search_quality_closing,
            ));
        }
        content = content.child(main_content(
            cx,
            &theme,
            &self.data,
            fit_width,
            content_active,
            self.selection_controls_closing,
        ));
        if !self.data.warnings.is_empty() {
            content = content.child(warning_banner(cx, &theme, &self.data));
            if self.data.show_warning_details {
                content = content.child(warning_details(&theme, &self.data));
            }
        }
        // No bottom bar over the empty drop zone either.
        if !self.data.clips.is_empty() {
            content = content.child(operation_bar(cx, &theme, &self.data, content_active));
        }
        if self.data.show_export {
            content = content.child(sidebar_scrim(&theme, self.export_sidebar_closing));
        }
        root = root.child(content);
        if self.data.show_export {
            root = root
                .child(export_sidebar(
                    cx,
                    &theme,
                    &self.data,
                    &self.export_inputs,
                    &self.export_scroll,
                    self.export_sidebar_closing,
                ))
                .child(export_sidebar_action(cx, &theme, &self.data));
        }
        if let Some(picker) = self.data.sequence_picker.clone() {
            root = root.child(overlay(
                &theme,
                "overlay-seq",
                sequence_picker_panel(cx, &theme, picker),
            ));
        }
        if let Some(menu) = self.data.menu.clone() {
            // Native-style context menu: click-outside layer plus an
            // anchored popup at the right-click point (GPUI-idiomatic
            // `deferred(anchored(...))`, snaps to the window edges).
            root = root.child(menu_dismiss_layer(cx));
            root = root.child(deferred(
                anchored()
                    .anchor(Corner::TopLeft)
                    .position(Point {
                        x: px(menu.position.0),
                        y: px(menu.position.1),
                    })
                    .snap_to_window()
                    .child(context_menu(cx, &theme, &self.data, menu)),
            ));
        }
        if self.data.show_search_quality {
            root = root.child(search_quality_dismiss_layer(cx));
        }
        if self.data.show_stage_settings {
            root = root.child(overlay(
                &theme,
                "overlay-stages",
                stage_settings_panel(cx, &theme, &self.data),
            ));
        }
        if self.data.show_sequence_results {
            root = root.child(overlay(
                &theme,
                "overlay-sequence-results",
                sequence_results_panel(cx, &theme, &self.data),
            ));
        }
        if self.data.show_search_settings {
            root = root.child(overlay(
                &theme,
                "overlay-search",
                search_settings_panel(cx, &theme, &self.data),
            ));
        }
        if self.data.show_path_fixer {
            root = root.child(overlay(
                &theme,
                "overlay-path-fixer",
                path_fixer_panel(
                    cx,
                    &theme,
                    &self.data,
                    &self.path_fixer_inputs,
                    (f32::from(window.viewport_size().height) - 64.).max(200.),
                ),
            ));
        }
        if self.data.error.is_some() {
            root = root.child(overlay(
                &theme,
                "overlay-error",
                error_alert(cx, &theme, &self.data),
            ));
        }
        if self.data.show_about {
            root = root.child(overlay(&theme, "overlay-about", about_panel(cx, &theme)));
        }
        root
    }
}

// ---------------- top toolbar (sources / sync / export)

fn toolbar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
    quality_closing: bool,
) -> impl IntoElement {
    let live = matches!(data.operation, super::state::Operation::Synchronizing);
    let busy = matches!(
        data.operation,
        Operation::Synchronizing | Operation::Exporting | Operation::Repairing
    );
    let has_timeline = !data.lanes.is_empty();
    let mut bar = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px_3()
        .h(px(48.))
        .flex_shrink_0()
        .border_b_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.panel));
    bar = bar.child(icon_button(
        cx,
        theme,
        "btn-add",
        icons().plus.clone(),
        "Add Media",
        true,
        |this, _, _, cx| this.add_media(cx),
    ));
    let trash_tip = if data.selection.is_empty() {
        "Clear session"
    } else {
        "Remove selected"
    };
    bar = bar.child(icon_button(
        cx,
        theme,
        "btn-clear",
        icons().trash.clone(),
        trash_tip,
        !data.clips.is_empty(),
        |this, _, _, cx| {
            this.data.delete_or_clear();
            cx.notify();
        },
    ));
    bar = bar.child(div().w(px(12.)));
    if has_timeline {
        bar = bar.child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight(600.0))
                .child(if live {
                    "Building Timeline"
                } else {
                    "Timeline"
                }),
        );
        if live {
            bar = bar.child(div().text_color(rgb(theme.dim)).child("…"));
        }
        bar = bar.child(div().flex_1());
    } else {
        bar = bar.child(div().font_weight(gpui::FontWeight(600.0)).child("Sources"));
        bar = bar.child(div().flex_1());
    }
    if !data.show_export {
        bar = bar.child(div().w(px(8.)).flex_shrink_0());
        let mut quality = div()
            .relative()
            .w(px(160.))
            .h(px(28.))
            .flex_shrink_0()
            .child(search_quality_button(cx, theme, data, active && !busy));
        if data.show_search_quality || quality_closing {
            quality = quality.child(deferred(
                anchored()
                    .position_mode(AnchoredPositionMode::Local)
                    .offset(point(px(0.), px(4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(quality_dropdown_motion(
                        search_quality_menu(cx, theme, data),
                        quality_closing,
                    )),
            ));
        }
        bar = bar.child(quality);
        bar = bar.child(
            div()
                .absolute()
                .left(gpui::relative(0.5))
                .ml(px(-56.))
                .w(px(112.))
                .flex()
                .justify_center()
                .child(prominent_button(
                    cx,
                    theme,
                    "btn-sync-bar",
                    "Synchronize",
                    active && data.can_synchronize(),
                    |this, _, _, cx| this.start_sync(cx),
                )),
        );
    }
    if !data.show_export {
        bar = bar.child(prominent_button(
            cx,
            theme,
            "btn-export-bar",
            "Export",
            active && data.can_export(),
            |this, _, _, cx| this.start_export_sheet(cx),
        ));
    }
    bar
}

fn timeline_zoom_controls(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(icon_button(
            cx,
            theme,
            "zoom-fit",
            icons().fit.clone(),
            "Zoom to fit",
            active && data.zoom_level > 0.001,
            |this, _, _, cx| {
                this.data.zoom_at(0.0, 0.0);
                this.data.pan_to(Some(0.0), None);
                cx.notify();
            },
        ))
        .child(icon_button(
            cx,
            theme,
            "zoom-out",
            icons().zoom_out.clone(),
            "Zoom out",
            active && data.zoom_level > 0.0,
            |this, _, _, cx| {
                let bounds = this.data.timeline_scroll.bounds();
                let anchor = f32::from(bounds.size.width) * 0.5;
                this.data
                    .zoom_at((this.data.zoom_level - 0.2).max(0.0), anchor);
                cx.notify();
            },
        ))
        .child(zoom_slider(cx, theme, data, active))
        .child(icon_button(
            cx,
            theme,
            "zoom-in",
            icons().zoom_in.clone(),
            "Zoom in",
            active && data.zoom_level < 1.0,
            |this, _, _, cx| {
                let bounds = this.data.timeline_scroll.bounds();
                let anchor = f32::from(bounds.size.width) * 0.5;
                this.data
                    .zoom_at((this.data.zoom_level + 0.2).min(1.0), anchor);
                cx.notify();
            },
        ))
        .child(
            div()
                .w(px(48.))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .justify_end()
                .text_color(rgb(theme.dim))
                .child(data.zoom_label()),
        )
}

// ---------------- main content (drop zone → file list → timeline)

fn main_content(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    fit_width: f32,
    active: bool,
    selection_controls_closing: bool,
) -> impl IntoElement {
    let mut content = div()
        .id("main")
        .flex_1()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .bg(rgb(theme.bg));
    if active && data.lanes.is_empty() {
        content = content.on_click(cx.listener(|this, _, _, cx| {
            if !this.data.selection.is_empty() {
                this.data.selection.clear();
                this.close_selection_controls(cx);
            }
        }));
    }
    if data.lanes.is_empty() {
        if data.clips.is_empty() {
            content = content.child(drop_zone(cx, theme, data, active));
        } else {
            content = content.child(file_list(
                cx,
                theme,
                data,
                active,
                selection_controls_closing,
            ));
        }
    } else {
        content = content.child(timeline_preview(cx, theme, data, fit_width, active));
    }
    let screen = if !data.lanes.is_empty() {
        "screen-timeline"
    } else if !data.clips.is_empty() {
        "screen-sources"
    } else {
        "screen-welcome"
    };
    // A stable ID animates only screen changes, never progress notifications,
    // scrolling, or zoom. GPUI stops requesting frames after the one-shot.
    slide_in(content, screen, 0)
}

fn drop_zone(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    _data: &super::state::AppData,
    active: bool,
) -> impl IntoElement {
    let panel = div()
        .id("dropzone")
        .w(px(480.))
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .p_6();
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .child(
            panel
                .child(svg_icon(icons().drop.clone(), 28.0, theme.icon))
                .child(
                    div()
                        .mt_4()
                        .text_size(px(20.))
                        .font_weight(gpui::FontWeight(600.0))
                        .child("Add media to sync"),
                )
                .child(
                    div()
                        .w(px(380.))
                        .mt_1()
                        .text_center()
                        .text_sm()
                        .text_color(rgb(theme.dim))
                        .child("Drop audio, video, folders, or a timeline here."),
                )
                .child(div().h(px(12.)))
                .child(prominent_button(
                    cx,
                    theme,
                    "btn-choose",
                    "Add media…",
                    active,
                    |this, _, _, cx| this.add_media(cx),
                )),
        )
}

// ---------------- source list (selectable rows with a stable header slot)

fn file_list(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
    selection_controls_closing: bool,
) -> impl IntoElement {
    let mut list = div().flex().flex_col().m_4().gap_2();
    let selecting = !data.selection.is_empty();
    let mut header = div()
        .id("source-list-header")
        .h(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center();
    if selecting {
        let all = data
            .clips
            .iter()
            .all(|clip| data.selection.contains(&clip.url));
        header = header.child(check_row(
            cx,
            theme,
            "select-all-sources",
            all,
            if all { "Deselect all" } else { "Select all" }.to_string(),
            active,
            move |this, _, _, cx| {
                cx.stop_propagation();
                if all {
                    this.data.selection.clear();
                    this.close_selection_controls(cx);
                } else {
                    this.data.selection = this
                        .data
                        .clips
                        .iter()
                        .map(|clip| clip.url.clone())
                        .collect();
                    this.selection_controls_closing = false;
                    cx.notify();
                }
            },
        ));
    } else {
        header = header.child(
            div()
                .px_2()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child(format!(
                    "{} item{}",
                    data.clips.len(),
                    if data.clips.len() == 1 { "" } else { "s" }
                )),
        );
    }
    list = list.child(header);
    for (index, clip) in data.clips.iter().enumerate() {
        let selected = data.selection.contains(&clip.url);
        let url = clip.url.clone();
        let video = matches!(clip.kind, Some(MediaKind::Video));
        let badge_color = if video { theme.cyan } else { theme.dim };
        let second_line = {
            let mut parts = Vec::new();
            if let Some(d) = clip.duration {
                parts.push(fmt_duration(d));
            }
            if let Some(tc) = &clip.timecode {
                parts.push(format!("TC {tc}"));
            }
            parts.push(clip.state.status_text());
            parts.join(" · ")
        };
        let state_icon: Option<(String, u32)> = match &clip.state {
            ClipState::Synchronized { evidence, .. } => {
                if *evidence == align_core::MatchEvidence::Timecode {
                    Some((icons().check.clone(), theme.blue))
                } else {
                    Some((icons().check.clone(), theme.green))
                }
            }
            ClipState::Matching { .. } => Some((icons().check.clone(), theme.green)),
            ClipState::Unmatched | ClipState::Warning => Some((icons().warn.clone(), theme.orange)),
            ClipState::Queued | ClipState::Analyzing => None,
        };
        let mut row = div()
            .id(SharedString::from(format!(
                "clip-{}",
                clip.url.to_string_lossy()
            )))
            .flex()
            .flex_row()
            .items_center()
            .px_4()
            .py_3()
            .rounded_xl()
            .border_1()
            .border_color(rgb(theme.separator))
            .bg(rgb(theme.panel))
            .overflow_hidden()
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if !active {
                    return;
                }
                if this.data.selection.contains(&url) {
                    this.data.selection.remove(&url);
                    if this.data.selection.is_empty() {
                        this.close_selection_controls(cx);
                    } else {
                        cx.notify();
                    }
                } else {
                    this.data.selection.insert(url.clone());
                    this.selection_controls_closing = false;
                    cx.notify();
                }
            }));
        if active {
            row = row.cursor_pointer();
        }
        if selected {
            row = row
                .bg(rgba((theme.accent << 8) | 0x20))
                .border_color(rgb(theme.accent));
        } else if active {
            row = row.hover(|this| this.bg(rgb(theme.button_hover)));
        }
        if selecting || selection_controls_closing {
            let indicator = div()
                .size(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(rgb(if selected { theme.accent } else { theme.border }))
                .bg(rgb(if selected { theme.accent } else { theme.panel }))
                .when(selected, |el| {
                    el.child(svg_icon(icons().check.clone(), 10., theme.on_accent))
                });
            row = row.child(selection_control_motion(
                indicator,
                SharedString::from(format!(
                    "selection-control-{}-{}",
                    if selection_controls_closing {
                        "out"
                    } else {
                        "in"
                    },
                    clip.url.display()
                )),
                selection_controls_closing,
            ));
        }
        row = row
            .child(
                div()
                    .w(px(18.))
                    .mr_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kind_badge(video, badge_color, None)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight(500.0))
                            .truncate()
                            .whitespace_nowrap()
                            .child(clip.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.dim))
                            .truncate()
                            .whitespace_nowrap()
                            .child(second_line),
                    ),
            )
            .child(div().flex_1().flex_shrink_0());
        if let Some((path, color)) = state_icon {
            row = row.child(svg_icon(path, 13.0, color));
        }
        list = list.child(slide_in(
            row,
            SharedString::from(format!("source-enter-{}", clip.url.display())),
            (index.min(3) as u64) * 40,
        ));
    }
    list
}

// ---------------- timeline preview (toolbar + ruler + lanes)

fn timeline_preview(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    let mut root = div().flex().flex_col().bg(rgb(theme.bg)).overflow_hidden();
    // Lanes (the header row with zoom controls lives in the unified
    // toolbar now).
    root = root.child(timeline_lanes(cx, theme, data, fit_width, active));
    if data.is_stale() {
        // Stale dimming is applied per-lane via opacity on the container.
        root = root.opacity(0.5);
    }
    root
}

/// Draggable zoom slider (mirrors the native `Slider(value:in: 0...1)`):
/// press and drag along the track to set the Fit…8x level, with a thumb
/// knob marking the current position. Step buttons and Option/Command +
/// wheel cover the rest of the native gestures (trackpad pinch has no
/// GPUI 0.2 event).
fn zoom_slider(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
) -> impl IntoElement {
    const TRACK_W: f32 = 120.0;
    const THUMB: f32 = 14.0;
    let frac = data.zoom_level.clamp(0.0, 1.0) as f32;
    let head = (frac * (TRACK_W - THUMB)).clamp(0.0, TRACK_W - THUMB);
    let tail = (TRACK_W - THUMB - head).clamp(0.0, TRACK_W - THUMB);
    let mut track = div()
        .id("zoom-slider")
        .w(px(TRACK_W))
        .h(px(20.))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center();
    if active {
        track = track.cursor_pointer();
    }
    track
        .child(
            div()
                .w(px(head))
                .h(px(6.))
                .rounded_full()
                .bg(rgb(theme.dim)),
        )
        .child(
            div()
                .w(px(THUMB))
                .h(px(THUMB))
                .flex_shrink_0()
                .rounded_full()
                .bg(rgb(theme.panel))
                .border_1()
                .border_color(rgb(theme.dim)),
        )
        .child(
            div()
                .w(px(tail))
                .h(px(6.))
                .rounded_full()
                .bg(rgb(theme.border)),
        )
        .on_drag(ZoomDrag, |_, _, _, cx| cx.new(|_| ZoomDragGhost))
        .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ZoomDrag>, _, cx| {
            let w = e.bounds.size.width;
            if w > px(0.) {
                let level = ((e.event.position.x - e.bounds.origin.x) / w).clamp(0., 1.) as f64;
                let viewport = this.data.timeline_scroll.bounds();
                this.data
                    .zoom_at(level, f32::from(viewport.size.width) * 0.5);
                cx.notify();
            }
        }))
}

fn timeline_lanes(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    use super::lane::{LABEL_WIDTH, bar_row_geometry, timeline_scale};
    const RULER_H: f32 = 32.0;
    const ROW_H: f32 = 60.0;
    let duration = data
        .lanes
        .iter()
        .flat_map(|l| l.clips.iter())
        .map(|c| c.start + c.duration)
        .fold(1.0f64, f64::max)
        .max(1.0);
    let zoom = data.zoom();
    let (px_per_sec, timeline_w) = timeline_scale(duration, zoom, fit_width);
    let tick_count = ((timeline_w / 160.0).floor() as usize).max(5);

    // Labels stay fixed while only the ruler and clips scroll horizontally.
    // This also makes a normal two-finger/vertical wheel pan the timeline:
    // GPUI maps it onto X for an x-only scroll view.
    let mut labels = div()
        .w(px(LABEL_WIDTH))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .child(div().h(px(RULER_H)).flex_shrink_0());
    let mut tracks = div().w(px(timeline_w)).flex_shrink_0().flex().flex_col();

    let mut ticks = div()
        .relative()
        .h(px(RULER_H))
        .flex_shrink_0()
        .w(px(timeline_w));
    for i in 0..=tick_count {
        let at = duration * i as f64 / tick_count as f64;
        let label = match data.ruler_timecode {
            Some(start) => fmt_timecode(start, at),
            None => fmt_duration(at),
        };
        let tick = div()
            .absolute()
            .top(px(8.))
            .text_xs()
            .whitespace_nowrap()
            .text_color(rgb(theme.dim))
            .child(label);
        ticks = ticks.child(if i == tick_count {
            tick.right(px(0.))
        } else {
            tick.left(px(timeline_w * i as f32 / tick_count as f32))
        });
    }
    tracks = tracks.child(ticks);

    // Rows.
    for lane in &data.lanes {
        let kind_glyph = match lane.kind {
            MediaKind::Video => "V",
            MediaKind::Audio => "A",
        };
        let kind_color = theme.icon;
        let lane_id = lane.id.clone();
        let first_clip = lane.clips.first().map(|c| c.clip_id.clone());
        // Hover tooltip keeps the source behind the number discoverable
        // (mirrors the native `.help` on the label cell), including an
        // active stream/channel override.
        let override_suffix = match data.effective_lane_analysis_source(&lane_id) {
            AudioAnalysisSource::Automatic => String::new(),
            AudioAnalysisSource::AllMixed => " · All Mix".to_string(),
            AudioAnalysisSource::Channel(index) => format!(" · Ch {}", index + 1),
            AudioAnalysisSource::MixedStream(index) => format!(" · S{} Mix", index + 1),
            AudioAnalysisSource::Stream { index, channel } => match channel {
                Some(ch) => format!(" · S{} Ch {}", index + 1, ch + 1),
                None => format!(" · S{}", index + 1),
            },
        };
        let tip = if lane.source_name.is_empty() {
            format!("Track {kind_glyph}{}{override_suffix}", lane.number)
        } else {
            format!(
                "Track {kind_glyph}{} · {}{override_suffix}",
                lane.number, lane.source_name
            )
        };
        // Label cell shows nothing but V1/A1…; stream override lives on
        // right-click (mirrors the native track header menu).
        let mut label = div()
            .id(SharedString::from(format!("lane-{lane_id}")))
            .w(px(LABEL_WIDTH))
            .flex_shrink_0()
            .overflow_hidden()
            .h(px(ROW_H))
            .flex()
            .flex_col()
            .items_center()
            .justify_center();
        if active {
            label = label.cursor_pointer();
        }
        labels = labels.child(
            label
                .tooltip(hover_tip(tip, theme))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        if let Some(clip_id) = first_clip.clone() {
                            this.data.menu = Some(super::state::MenuTarget {
                                clip_id,
                                stream_menu: true,
                                lane_id: Some(lane_id.clone()),
                                position: (f32::from(e.position.x), f32::from(e.position.y)),
                            });
                            cx.notify();
                        }
                    }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(kind_color))
                        .child(format!("{kind_glyph}{}", lane.number)),
                ),
        );
        // Track area: fixed-size cell; bars are absolutely positioned
        // (mirrors Swift's ZStack + offset), so placement is exact and
        // min-width bars can never shift their neighbours.
        {
            let mut track = div()
                .relative()
                .w(px(timeline_w))
                .h(px(ROW_H))
                .flex_shrink_0();
            // Vertical gridlines at ruler ticks (mirrors the Canvas).
            for i in 0..=tick_count {
                let x = timeline_w * i as f32 / tick_count as f32;
                track = track.child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(0.))
                        .w(px(1.))
                        .h_full()
                        .bg(rgb(theme.separator)),
                );
            }
            let geometry = bar_row_geometry(&lane.clips, px_per_sec);
            for (bar_index, (bar, (x, width))) in lane.clips.iter().zip(geometry.iter()).enumerate()
            {
                let (x, width) = (*x, *width);
                let color = match bar.match_state {
                    super::lane::BarMatchState::Pending => 0x007AFF,
                    super::lane::BarMatchState::Matched => 0x34C759,
                    super::lane::BarMatchState::Unmatched => 0xFF9500,
                };
                let ink = 0xFFFFFF;
                let clip_id = bar.clip_id.clone();
                // Corner radius min(4, w/2) like Swift; square slivers.
                let mut el = div()
                    .id(SharedString::from(format!("bar-{}-{bar_index}", lane.id)))
                    .absolute()
                    .left(px(x))
                    .top(px(6.0))
                    .w(px(width))
                    .h(px(ROW_H - 12.0));
                if width >= 8.0 {
                    el = el.rounded_md();
                }
                el = el
                    .bg(rgb(color))
                    .border_1()
                    .border_color(rgba(0x00000018))
                    .text_color(rgb(ink))
                    .px_1()
                    .overflow_hidden()
                    .cursor_default();
                if active {
                    el = el.tooltip(hover_tip(
                        format!("{} · {}", bar.url.display(), fmt_duration(bar.duration)),
                        theme,
                    ));
                }
                // Icon + name when wide enough (mirrors w>=24 / w>=64).
                // Bar label: icon pinned top-left, text pinned to the
                // remaining width. Absolute blocks (not flex items) hold
                // exact bounds, so long names shape a real "…" instead of
                // hard-clipping past the bar edge. `el` itself is
                // absolutely positioned, hence a valid anchor.
                if width >= 24.0 {
                    let video = matches!(bar.kind, MediaKind::Video);
                    let icon_path = if video {
                        icons().film.clone()
                    } else {
                        icons().waveform.clone()
                    };
                    el = el.child(
                        div()
                            .absolute()
                            .left(px(7.0))
                            .top(px(17.0))
                            .child(svg_icon(icon_path, 13.0, ink)),
                    );
                    if width >= 64.0 {
                        el = el.child(
                            div()
                                .absolute()
                                .left(px(27.0))
                                .right(px(4.0))
                                .top(px(0.))
                                .bottom(px(0.))
                                .flex()
                                .flex_col()
                                .justify_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .whitespace_nowrap()
                                        .text_color(rgb(ink))
                                        .child(bar.name.clone()),
                                ),
                        );
                    }
                }
                el = el.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        this.data.menu = Some(super::state::MenuTarget {
                            clip_id: clip_id.clone(),
                            stream_menu: false,
                            lane_id: None,
                            position: (f32::from(e.position.x), f32::from(e.position.y)),
                        });
                        cx.notify();
                    }),
                );
                track = track.child(el);
            }
            tracks = tracks.child(track);
        }
        // Row separator (mirrors the Canvas horizontal gridlines).
        labels = labels.child(div().h(px(1.)).flex_shrink_0().bg(rgb(theme.separator)));
        tracks = tracks.child(div().h(px(1.)).flex_shrink_0().bg(rgb(theme.separator)));
    }

    let viewport = div()
        .id("timeline-viewport")
        .flex_1()
        .min_w(px(0.))
        .overflow_x_scroll()
        .track_scroll(&data.timeline_scroll)
        .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
            if !(event.modifiers.alt || event.modifiers.platform) {
                return;
            }
            let dy: f32 = match event.delta {
                gpui::ScrollDelta::Pixels(point) => f32::from(point.y) * 0.005,
                gpui::ScrollDelta::Lines(point) => point.y * 0.05,
            };
            let next = (this.data.zoom_level as f32 + dy).clamp(0.0, 1.0) as f64;
            if (next - this.data.zoom_level).abs() > 0.0001 {
                let bounds = this.data.timeline_scroll.bounds();
                let anchor = f32::from(event.position.x - bounds.origin.x)
                    .clamp(0.0, f32::from(bounds.size.width));
                this.data.zoom_at(next, anchor);
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, e: &MouseDownEvent, _, _| {
                this.pan_origin = Some(e.position);
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|this, e: &MouseDownEvent, _, _| {
                this.pan_origin = Some(e.position);
            }),
        )
        .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
            let dragging = matches!(
                e.pressed_button,
                Some(MouseButton::Left) | Some(MouseButton::Middle)
            );
            if !dragging {
                this.pan_origin = None;
                return;
            }
            if let Some(origin) = this.pan_origin {
                this.data.pan_by(f32::from(e.position.x - origin.x), 0.0);
                this.pan_origin = Some(e.position);
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, _| this.pan_origin = None),
        )
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(|this, _, _, _| this.pan_origin = None),
        )
        .child(tracks);

    div()
        .w_full()
        .flex()
        .flex_row()
        .p_2()
        .child(labels)
        .child(viewport)
}

// ---------------- warning banner + details

fn warning_banner(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let n = data.warnings.len();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_5()
        .py_1()
        .bg(rgba((theme.warning << 8) | 0x14))
        .child(svg_icon(icons().warn.clone(), 14.0, theme.orange))
        .child(format!(
            "{} media warning{}",
            n,
            if n == 1 { "" } else { "s" }
        ))
        .child(button(
            cx,
            theme,
            "btn-warn-details",
            "Details",
            true,
            |this, _, _, cx| {
                this.data.show_warning_details = !this.data.show_warning_details;
                cx.notify();
            },
        ))
        .when(data.can_locate_timeline_media(), |banner| {
            banner.child(button(
                cx,
                theme,
                "btn-relink-media",
                "Relink…",
                true,
                |this, _, _, cx| this.choose_missing_media(cx),
            ))
        })
}

fn warning_details(theme: &Theme, data: &super::state::AppData) -> impl IntoElement {
    let mut panel = div()
        .id("warn-details")
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .m_2()
        .max_h(px(320.))
        .overflow_y_scroll()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.orange))
        .bg(rgb(theme.panel));
    for message in data.warnings.iter().take(50) {
        panel = panel.child(div().text_color(rgb(theme.text)).child(message.clone()));
    }
    panel
}

// ---------------- bottom operation bar (mirrors OperationBar)

fn operation_bar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
) -> impl IntoElement {
    use super::state::Operation;
    let busy = matches!(
        data.operation,
        Operation::Synchronizing | Operation::Exporting | Operation::Repairing
    );
    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .h(px(48.))
        .flex_shrink_0()
        .bg(rgb(theme.panel))
        .border_t_1()
        .border_color(rgb(theme.separator));
    // Success check.
    if matches!(data.operation, Operation::Ready) && data.has_result() && !data.is_stale() {
        bar = bar.child(svg_icon(icons().check.clone(), 14.0, theme.green));
    }
    // Status + progress.
    {
        let mut status = div()
            .flex()
            .flex_col()
            .min_w(px(0.))
            .text_size(px(12.))
            .gap_1();
        status = status.child(data.status.clone());
        if busy {
            status = status.child(
                div()
                    .w(px(280.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(theme.border))
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(rgb(theme.accent))
                            .w(gpui::relative(data.progress.clamp(0.0, 1.0))),
                    ),
            );
        }
        bar = bar.child(status);
    }
    if data.is_stale() && !busy {
        if data.pending_count > 0 {
            bar = bar.child(
                div()
                    .text_color(rgb(theme.orange))
                    .child(format!("＋ {} new", data.pending_count)),
            );
            bar = bar.child(
                div()
                    .text_color(rgb(theme.dim))
                    .child("Synchronize to include them."),
            );
        } else {
            bar = bar.child(
                div()
                    .text_color(rgb(theme.orange))
                    .child("Settings changed"),
            );
            bar = bar.child(
                div()
                    .text_color(rgb(theme.dim))
                    .child("Synchronize to apply them."),
            );
        }
    }
    bar = bar.child(div().flex_1());
    if busy {
        bar = bar.child(button(
            cx,
            theme,
            "btn-cancel",
            "Cancel",
            true,
            |this, _, _, cx| this.cancel_current(cx),
        ));
    } else {
        if let Some(result) = &data.result {
            if result.stages.len() > 1 {
                bar = bar.child(button(
                    cx,
                    theme,
                    "btn-stage-settings",
                    format!(
                        "Stage {}/{}",
                        result.selected_stage.unwrap_or(0) + 1,
                        result.stages.len()
                    ),
                    data.can_export(),
                    |this, _, _, cx| {
                        this.data.show_stage_settings = true;
                        cx.notify();
                    },
                ));
            }
        }
        if data.sequence_results.len() > 1 {
            let name = data
                .result
                .as_ref()
                .and_then(|result| result.project.imported_timeline.as_ref())
                .map_or("Untitled", |timeline| timeline.name.as_str());
            bar = bar.child(button(
                cx,
                theme,
                "btn-sequence-results",
                format!(
                    "Sequence {}/{} · {name}",
                    data.active_sequence_result + 1,
                    data.sequence_results.len()
                ),
                true,
                |this, _, _, cx| {
                    this.data.show_sequence_results = true;
                    cx.notify();
                },
            ));
        }
        if !data.exported_files.is_empty() && !data.show_export {
            bar = bar.child(button(
                cx,
                theme,
                "btn-reveal",
                "Show in Finder",
                true,
                |this, _, _, cx| {
                    this.data.reveal_export();
                    cx.notify();
                },
            ));
        }
        let fixes = data.correction_count();
        if fixes > 0 {
            bar = bar.child(button(
                cx,
                theme,
                "btn-reset-fixes",
                format!("Reset {fixes} Fixes"),
                true,
                |this, _, _, cx| {
                    if this.data.reset_corrections() {
                        this.start_sync(cx);
                    } else {
                        cx.notify();
                    }
                },
            ));
        }
    }
    if !data.show_export && !data.lanes.is_empty() {
        bar = bar.child(timeline_zoom_controls(cx, theme, data, active && !busy));
    }
    bar
}

fn stage_settings_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let mut panel = div().id("stage-settings").w(px(400.)).p_4().flex().flex_col()
        .gap_2().rounded_lg().bg(rgb(theme.panel)).border_1().border_color(rgb(theme.border))
        .child(modal_header(cx, theme, "Synchronization stages", "stage-close", |this, _, _, cx| {
            this.data.show_stage_settings = false;
            cx.notify();
        }))
        .child(div().text_size(px(12.)).text_color(rgb(theme.dim))
            .child("Compare completed results. The selected stage is shown on the timeline and used for export."));
    if let Some(result) = &data.result {
        for (index, stage) in result.stages.iter().enumerate() {
            let check = if result.selected_stage == Some(index) {
                "✓ "
            } else {
                ""
            };
            panel = panel.child(button(
                cx,
                theme,
                format!("select-stage-{index}"),
                format!(
                    "{check}{} · {} synced",
                    stage.kind.label(),
                    stage.synchronized_count()
                ),
                true,
                move |this, _, _, cx| {
                    this.data.select_sync_stage(index);
                    cx.notify();
                },
            ));
        }
    }
    panel
}

fn sequence_results_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let mut panel = div()
        .id("sequence-results")
        .w(px(420.))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .child(modal_header(
            cx,
            theme,
            "Synchronized sequences",
            "sequence-results-close",
            |this, _, _, cx| {
                this.data.show_sequence_results = false;
                cx.notify();
            },
        ))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child("Choose the result shown on the timeline. Export includes every sequence."),
        );
    for (index, result) in data.sequence_results.iter().enumerate() {
        let name = result
            .project
            .imported_timeline
            .as_ref()
            .map_or("Untitled", |timeline| timeline.name.as_str());
        let check = if index == data.active_sequence_result {
            "✓ "
        } else {
            ""
        };
        panel = panel.child(button(
            cx,
            theme,
            format!("select-sequence-result-{index}"),
            format!("{check}{} · {name}", index + 1),
            true,
            move |this, _, _, cx| {
                this.data.select_sequence_result(index);
                cx.notify();
            },
        ));
    }
    panel
}

fn audio_source_label(source: AudioAnalysisSource) -> &'static str {
    match source {
        AudioAnalysisSource::Automatic => "Automatic",
        AudioAnalysisSource::AllMixed => "All streams mixed",
        AudioAnalysisSource::Channel(0) => "First channel",
        AudioAnalysisSource::MixedStream(0) => "First stream mixed",
        AudioAnalysisSource::Channel(_) => "Channel",
        AudioAnalysisSource::MixedStream(_) => "Stream mixed",
        AudioAnalysisSource::Stream { .. } => "Stream channel",
    }
}

fn match_threshold_label(value: MatchThreshold) -> &'static str {
    match value {
        MatchThreshold::Permissive => "More matches",
        MatchThreshold::Balanced => "Balanced",
        MatchThreshold::Conservative => "Fewer false matches",
    }
}

fn clip_order_label(value: ClipOrder) -> &'static str {
    match value {
        ClipOrder::Auto => "Auto",
        ClipOrder::AlternateAuto => "Alternate Auto",
        ClipOrder::AsImported => "As imported",
        ClipOrder::ByDateTime => "Date & time",
        ClipOrder::ByFileName => "File name",
        ClipOrder::Ignore => "Ignore",
    }
}

fn track_content_label(value: TrackContent) -> &'static str {
    match value {
        TrackContent::Auto => "Automatic",
        TrackContent::Linear => "Linear",
        TrackContent::Takes => "Takes",
    }
}

fn next_setting<T: Copy + PartialEq>(current: Option<T>, values: &[T], inherit: bool) -> Option<T> {
    match current {
        None => values.first().copied(),
        Some(value) => values
            .iter()
            .position(|candidate| *candidate == value)
            .and_then(|index| values.get(index + 1).copied())
            .or_else(|| (!inherit).then(|| values[0])),
    }
}

fn settings_value_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: &'static str,
    title: &'static str,
    value: String,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_size(px(12.)).child(title))
        .child(button(cx, theme, id, value, true, action))
}

fn scoped_label<T: Copy>(value: Option<T>, common: T, label: impl Fn(T) -> &'static str) -> String {
    value.map_or_else(
        || format!("Inherit ({})", label(common)),
        |value| label(value).to_string(),
    )
}

fn search_quality_dismiss_layer(cx: &mut Context<AlignApp>) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.close_search_quality(cx);
                cx.stop_propagation();
            }),
        )
}

fn search_quality_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    enabled: bool,
) -> impl IntoElement {
    let label = if data.show_search_quality {
        "Sync quality".to_string()
    } else {
        format!(
            "Quality: {}",
            data.current_effective_settings().search_accuracy.label()
        )
    };
    let mut control = div()
        .id("btn-search-quality")
        .w_full()
        .h_full()
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_lg()
        .bg(rgb(if data.show_search_quality {
            theme.border
        } else {
            theme.button_hover
        }))
        .text_size(px(12.))
        .child(div().min_w(px(0.)).truncate().child(label))
        .child(div().w(px(12.)).h(px(12.)).flex_shrink_0().child(svg_icon(
            icons().chevron_down.clone(),
            12.,
            theme.icon,
        )));
    if enabled {
        control = control
            .text_color(rgb(theme.icon))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.border)))
            .active(|this| this.opacity(0.62))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.data.show_search_quality {
                    this.close_search_quality(cx);
                } else {
                    this.search_quality_closing = false;
                    this.data.show_search_quality = true;
                    cx.notify();
                }
            }));
    } else {
        control = control.text_color(rgb(theme.dim)).opacity(0.42);
    }
    control
}

fn search_quality_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: &'static str,
    selected: bool,
    accuracy: align_core::SearchAccuracy,
) -> impl IntoElement {
    div()
        .id(id.into())
        .h(px(28.))
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.button_hover)))
        .on_click(cx.listener(move |this, _, _, cx| {
            if this.data.set_scoped_search_accuracy(Some(accuracy)) {
                this.data.mark_quality_dirty();
            }
            this.close_search_quality(cx);
        }))
        .child(div().min_w(px(0.)).truncate().child(label))
        .child(
            div()
                .w(px(16.))
                .h(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |slot| {
                    slot.child(svg_icon(icons().check.clone(), 12., theme.accent))
                }),
        )
}

fn search_quality_menu(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> Stateful<Div> {
    let selected = data.current_effective_settings().search_accuracy;
    let mut menu = div()
        .id("search-quality-menu")
        .w(px(160.))
        .flex()
        .flex_col()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    for (index, accuracy) in align_core::SearchAccuracy::ALL.into_iter().enumerate() {
        menu = menu.child(search_quality_row(
            cx,
            theme,
            format!("search-quality-{index}"),
            accuracy.label(),
            accuracy == selected,
            accuracy,
        ));
    }
    menu.child(
        div()
            .mt_1()
            .pt_1()
            .border_t_1()
            .border_color(rgb(theme.separator))
            .child(menu_row(
                cx,
                theme,
                "search-settings-more",
                "More settings…",
                false,
                |this, _, _, cx| {
                    this.close_search_quality(cx);
                    this.data.show_search_settings = true;
                    this.data.settings_changed = false;
                    cx.notify();
                },
            )),
    )
}

fn search_settings_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let sequence = data.current_sequence_settings();
    let common = data.common_settings;
    let current_scope = data.settings_scope == SettingsScope::CurrentSequence;
    let search = if current_scope {
        sequence.search_accuracy
    } else {
        Some(common.search_accuracy)
    };
    let audio = if current_scope {
        sequence.audio_source
    } else {
        Some(common.audio_source)
    };
    let temporal = if current_scope {
        sequence.temporal_mode
    } else {
        Some(common.temporal_mode)
    };
    let threshold = if current_scope {
        sequence.match_threshold
    } else {
        Some(common.match_threshold)
    };
    let order = if current_scope {
        sequence.clip_order
    } else {
        Some(common.clip_order)
    };
    let content = if current_scope {
        sequence.track_content
    } else {
        Some(common.track_content)
    };
    let scope_row = div()
        .flex()
        .flex_row()
        .gap_2()
        .child(button(
            cx,
            theme,
            "settings-scope-common",
            if current_scope {
                "Common"
            } else {
                "✓ Common"
            },
            true,
            |this, _, _, cx| {
                this.data.settings_scope = SettingsScope::Common;
                cx.notify();
            },
        ))
        .child(button(
            cx,
            theme,
            "settings-scope-sequence",
            if current_scope {
                "✓ Current sequence"
            } else {
                "Current sequence"
            },
            true,
            |this, _, _, cx| {
                this.data.settings_scope = SettingsScope::CurrentSequence;
                cx.notify();
            },
        ));
    div()
        .id("search-settings")
        .w(px(520.))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .child(modal_header(
            cx,
            theme,
            "Synchronization settings",
            "settings-close",
            |this, _, _, cx| this.finish_search_settings(cx),
        ))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child("Choose defaults for every sequence or replace them for the current sequence. Track menus inherit these values."),
        )
        .child(scope_row)
        .child(settings_value_row(
            cx,
            theme,
            "settings-search",
            "Search accuracy",
            scoped_label(search, common.search_accuracy, |value| value.label()),
            |this, _, _, cx| {
                let sequence = this.data.current_sequence_settings();
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    sequence.search_accuracy
                } else {
                    Some(this.data.common_settings.search_accuracy)
                };
                let value = next_setting(current, &align_core::SearchAccuracy::ALL, inherit);
                this.data.settings_changed |= this.data.set_scoped_search_accuracy(value);
                cx.notify();
            },
        ))
        .child(settings_value_row(
            cx,
            theme,
            "settings-audio",
            "Wave source",
            scoped_label(audio, common.audio_source, audio_source_label),
            |this, _, _, cx| {
                const VALUES: [AudioAnalysisSource; 4] = [
                    AudioAnalysisSource::Automatic,
                    AudioAnalysisSource::AllMixed,
                    AudioAnalysisSource::MixedStream(0),
                    AudioAnalysisSource::Channel(0),
                ];
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    this.data.current_sequence_settings().audio_source
                } else {
                    Some(this.data.common_settings.audio_source)
                };
                let value = next_setting(current, &VALUES, inherit);
                this.data.settings_changed |= this.data.set_scoped_audio_source(value);
                cx.notify();
            },
        ))
        .child(settings_value_row(
            cx,
            theme,
            "settings-temporal",
            "Time source",
            scoped_label(temporal, common.temporal_mode, |value| value.title()),
            |this, _, _, cx| {
                const VALUES: [TemporalMode; 4] = [
                    TemporalMode::Auto,
                    TemporalMode::RecStart,
                    TemporalMode::RecStop,
                    TemporalMode::Timecode,
                ];
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    this.data.current_sequence_settings().temporal_mode
                } else {
                    Some(this.data.common_settings.temporal_mode)
                };
                let value = next_setting(current, &VALUES, inherit);
                this.data.settings_changed |= this.data.set_scoped_temporal_mode(value);
                cx.notify();
            },
        ))
        .child(settings_value_row(
            cx,
            theme,
            "settings-threshold",
            "Match threshold",
            scoped_label(threshold, common.match_threshold, match_threshold_label),
            |this, _, _, cx| {
                const VALUES: [MatchThreshold; 3] = [
                    MatchThreshold::Permissive,
                    MatchThreshold::Balanced,
                    MatchThreshold::Conservative,
                ];
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    this.data.current_sequence_settings().match_threshold
                } else {
                    Some(this.data.common_settings.match_threshold)
                };
                let value = next_setting(current, &VALUES, inherit);
                this.data.settings_changed |= this.data.set_scoped_match_threshold(value);
                cx.notify();
            },
        ))
        .child(settings_value_row(
            cx,
            theme,
            "settings-order",
            "Clip order",
            scoped_label(order, common.clip_order, clip_order_label),
            |this, _, _, cx| {
                const VALUES: [ClipOrder; 6] = [
                    ClipOrder::Auto,
                    ClipOrder::AlternateAuto,
                    ClipOrder::AsImported,
                    ClipOrder::ByDateTime,
                    ClipOrder::ByFileName,
                    ClipOrder::Ignore,
                ];
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    this.data.current_sequence_settings().clip_order
                } else {
                    Some(this.data.common_settings.clip_order)
                };
                let value = next_setting(current, &VALUES, inherit);
                this.data.settings_changed |= this.data.set_scoped_clip_order(value);
                cx.notify();
            },
        ))
        .child(settings_value_row(
            cx,
            theme,
            "settings-content",
            "Track content",
            scoped_label(content, common.track_content, track_content_label),
            |this, _, _, cx| {
                const VALUES: [TrackContent; 3] = [
                    TrackContent::Auto,
                    TrackContent::Linear,
                    TrackContent::Takes,
                ];
                let inherit = this.data.settings_scope == SettingsScope::CurrentSequence;
                let current = if inherit {
                    this.data.current_sequence_settings().track_content
                } else {
                    Some(this.data.common_settings.track_content)
                };
                let value = next_setting(current, &VALUES, inherit);
                this.data.settings_changed |= this.data.set_scoped_track_content(value);
                cx.notify();
            },
        ))
        .when(data.settings_changed, |panel| {
            panel.child(prominent_button(
                cx,
                theme,
                "settings-apply",
                "Apply settings",
                true,
                |this, _, _, cx| this.finish_search_settings(cx),
            ))
        })
}

// ---------------- diagnostics popover (mirrors DiagnosticsButton)

// ---------------- sequence picker (mirrors chooseSequence alert)

fn sequence_picker_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    picker: SequencePicker,
) -> impl IntoElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_5()
        .m_2()
        .max_w(px(420.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Choose a sequence",
            "seq-cancel",
            |this, _, _, cx| {
                this.data.sequence_picker = None;
                cx.notify();
            },
        ))
        .child(div().text_color(rgb(theme.dim)).child(format!(
                "{} contains multiple timelines.",
                picker
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("timeline")
            )));
    panel = panel.child(prominent_button(
        cx,
        theme,
        "seq-all",
        format!("Import all {} sequences", picker.options.len()),
        true,
        |this, _, _, cx| {
            this.data.choose_all_sequences();
            cx.notify();
        },
    ));
    for option in picker.options {
        let label = format!("{} — {} clips", option.name, option.clip_count);
        let index = option.index;
        panel = panel.child(button(
            cx,
            theme,
            format!("seq-{index}"),
            label,
            true,
            move |this, _, _, cx| {
                this.data.choose_sequence(index);
                cx.notify();
            },
        ));
    }
    panel
}

// ---------------- context menu (anchored popup, mirrors clip/track menus)

/// Transparent click-outside layer that dismisses the context menu.
/// The closing click is swallowed so it never activates the element
/// below the menu.
fn menu_dismiss_layer(cx: &mut Context<AlignApp>) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.data.menu = None;
                cx.notify();
                cx.stop_propagation();
            }),
        )
}

/// Borderless menu row (compact, unlike dialog buttons).
fn menu_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    danger: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .flex()
        .px_3()
        .h(px(28.))
        .flex_shrink_0()
        .items_center()
        .rounded_md()
        .text_size(px(12.))
        .truncate()
        .whitespace_nowrap()
        .text_color(rgb(if danger { theme.danger } else { theme.text }))
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.button_hover)))
        .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)))
        .child(label.into())
}

fn menu_header(theme: &Theme, label: String) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_3()
        .pt_2()
        .text_xs()
        .text_color(rgb(theme.dim))
        .truncate()
        .whitespace_nowrap()
        .child(label)
}

fn context_menu(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: MenuTarget,
) -> impl IntoElement {
    let mut panel = div()
        .id("ctx-menu")
        .flex()
        .flex_col()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .occlude()
        .min_w(px(280.))
        .max_w(px(420.))
        .max_h(px(380.))
        .overflow_y_scroll();
    // Lane right-click edits the whole track's analysis source; clip
    // right-click only offers pair corrections for that clip.
    if menu.stream_menu {
        return stream_menu_section(cx, theme, data, &menu, panel);
    }
    let options = data.correction_options_for(&menu.clip_id);
    if options.is_empty() {
        panel = panel.child(menu_header(theme, "No other alignments found".to_string()));
    }
    for option in &options {
        panel = panel.child(menu_header(theme, option.other_name.clone()));
        let target = option.clone();
        panel = panel.child(menu_row(
            cx,
            theme,
            format!("fix-another-{}", target.id),
            "Find Another Alignment",
            false,
            move |this, _, _, cx| {
                let target = target.clone();
                this.find_another(&target, cx);
            },
        ));
        let target = option.clone();
        let label = format!("Reject Pair with {}", target.other_name);
        panel = panel.child(menu_row(
            cx,
            theme,
            format!("fix-reject-{}", target.id),
            label,
            true,
            move |this, _, _, cx| {
                let target = target.clone();
                this.reject_pair(&target, cx);
            },
        ));
    }
    panel
}

fn stream_menu_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    // Stream/channel counts of the lane's clips (mirror the track menu).
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let current = menu
        .lane_id
        .as_deref()
        .and_then(|lane_id| data.lane_analysis_source(lane_id));
    let label = move |source, title: String| {
        if current == Some(source) {
            format!("✓ {title}")
        } else {
            title
        }
    };
    if let Some(lane_id) = &menu.lane_id {
        if let Some(lane) = data.lanes.iter().find(|l| &l.id == lane_id) {
            for (index, channels) in lane.stream_channels.iter().copied().enumerate() {
                counts.insert(index, channels);
            }
        }
    }
    if counts.is_empty() {
        counts = data
            .audio_stream_channels
            .get(&menu.clip_id)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, n)| (i, *n))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
    }
    let mut indices: Vec<usize> = counts.keys().copied().collect();
    indices.sort_unstable();
    panel = panel.child(menu_header(theme, "Analysis source".to_string()));
    let inherited = data.current_effective_settings().audio_source;
    panel = panel.child(menu_row(
        cx,
        theme,
        "stream-inherit",
        if current.is_none() {
            format!("✓ Inherit ({})", audio_source_label(inherited))
        } else {
            format!("Inherit ({})", audio_source_label(inherited))
        },
        false,
        |this, _, _, cx| this.set_stream_source(None, cx),
    ));
    panel = panel.child(menu_row(
        cx,
        theme,
        "stream-auto",
        label(AudioAnalysisSource::Automatic, "Automatic".to_string()),
        false,
        |this, _, _, cx| this.set_stream_source(Some(AudioAnalysisSource::Automatic), cx),
    ));
    if counts.len() > 1 {
        panel = panel.child(menu_row(
            cx,
            theme,
            "all-streams-mix",
            label(
                AudioAnalysisSource::AllMixed,
                "Mix all audio streams".to_string(),
            ),
            false,
            |this, _, _, cx| this.set_stream_source(Some(AudioAnalysisSource::AllMixed), cx),
        ));
    }
    for index in indices {
        let channels = counts.get(&index).copied().unwrap_or(0);
        if counts.len() == 1 && channels <= 1 {
            continue;
        }
        if counts.len() == 1 {
            panel = panel.child(menu_row(
                cx,
                theme,
                "stream-mix",
                label(
                    AudioAnalysisSource::MixedStream(index),
                    "Mix all channels".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_stream_source(Some(AudioAnalysisSource::MixedStream(index)), cx)
                },
            ));
            for ch in 0..channels {
                panel = panel.child(menu_row(
                    cx,
                    theme,
                    format!("stream-ch-{ch}"),
                    label(
                        AudioAnalysisSource::Channel(ch),
                        format!("Use channel {}", ch + 1),
                    ),
                    false,
                    move |this, _, _, cx| {
                        this.set_stream_source(Some(AudioAnalysisSource::Channel(ch)), cx)
                    },
                ));
            }
        } else if channels <= 1 {
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}"),
                label(
                    AudioAnalysisSource::Stream {
                        index,
                        channel: None,
                    },
                    format!("Audio stream {}", index + 1),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_stream_source(
                        Some(AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        }),
                        cx,
                    )
                },
            ));
        } else {
            panel = panel.child(menu_header(theme, format!("Audio stream {}", index + 1)));
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}-mix"),
                label(
                    AudioAnalysisSource::MixedStream(index),
                    "Mix channels".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_stream_source(Some(AudioAnalysisSource::MixedStream(index)), cx)
                },
            ));
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}-auto"),
                label(
                    AudioAnalysisSource::Stream {
                        index,
                        channel: None,
                    },
                    "Automatic channel".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_stream_source(
                        Some(AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        }),
                        cx,
                    )
                },
            ));
            for ch in 0..channels {
                panel = panel.child(menu_row(
                    cx,
                    theme,
                    format!("stream-{index}-ch-{ch}"),
                    label(
                        AudioAnalysisSource::Stream {
                            index,
                            channel: Some(ch),
                        },
                        format!("Use channel {}", ch + 1),
                    ),
                    false,
                    move |this, _, _, cx| {
                        this.set_stream_source(
                            Some(AudioAnalysisSource::Stream {
                                index,
                                channel: Some(ch),
                            }),
                            cx,
                        )
                    },
                ));
            }
        }
    }
    panel = track_content_section(cx, theme, data, menu, panel);
    panel = preserve_editing_section(cx, theme, data, menu, panel);
    if let Some(lane_id) = &menu.lane_id {
        let current = data.lane_search_accuracy(lane_id);
        panel = panel.child(menu_header(theme, "Search accuracy".to_string()));
        for (index, accuracy) in std::iter::once(None)
            .chain(align_core::SearchAccuracy::ALL.into_iter().map(Some))
            .enumerate()
        {
            let title = accuracy
                .map(|value| value.label().to_string())
                .unwrap_or_else(|| {
                    format!(
                        "Inherit ({})",
                        data.current_effective_settings().search_accuracy.label()
                    )
                });
            let title = if current == accuracy {
                format!("✓ {title}")
            } else {
                title
            };
            let lane_id = lane_id.clone();
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("lane-search-{index}"),
                title,
                false,
                move |this, _, _, cx| {
                    if this.data.set_lane_search_accuracy(accuracy, &lane_id) {
                        this.data.menu = None;
                        this.start_sync(cx);
                    }
                },
            ));
        }
    }
    panel = clip_order_section(cx, theme, data, menu, panel);
    panel = time_source_section(cx, theme, data, menu, panel);
    panel = match_threshold_section(cx, theme, data, menu, panel);
    panel
}

fn preserve_editing_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let Some(lane_id) = menu.lane_id.as_deref() else {
        return panel;
    };
    if !data.lane_can_preserve_editing(lane_id) {
        return panel;
    }
    panel = panel.child(menu_header(theme, "Extra options".to_string()));
    let selected = data.lane_preserves_editing(lane_id);
    panel.child(menu_row(
        cx,
        theme,
        "preserve-basic-editing",
        if selected {
            "✓ Preserve basic editing".to_string()
        } else {
            "Preserve basic editing".to_string()
        },
        false,
        move |this, _, _, cx| this.set_preserve_editing(!selected, cx),
    ))
}

fn track_content_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_track_content(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Track content".to_string()));
    let inherited = data.current_effective_settings().track_content;
    for (mode, title, row_id) in [
        (
            None,
            format!("Inherit ({})", track_content_label(inherited)),
            "content-inherit",
        ),
        (Some(TrackContent::Auto), "Automatic".into(), "content-auto"),
        (
            Some(TrackContent::Linear),
            "Linear".into(),
            "content-linear",
        ),
        (Some(TrackContent::Takes), "Takes".into(), "content-takes"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_track_content(mode, cx),
        ));
    }
    panel
}

fn clip_order_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_clip_order(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Clip order".to_string()));
    let inherited = data.current_effective_settings().clip_order;
    for (mode, title, row_id) in [
        (
            None,
            format!("Inherit ({})", clip_order_label(inherited)),
            "order-inherit",
        ),
        (Some(ClipOrder::Auto), "Auto".into(), "order-auto"),
        (
            Some(ClipOrder::AlternateAuto),
            "Alternate Auto".into(),
            "order-alternate-auto",
        ),
        (
            Some(ClipOrder::AsImported),
            "As imported".into(),
            "order-imported",
        ),
        (
            Some(ClipOrder::ByDateTime),
            "Date & time".into(),
            "order-date",
        ),
        (
            Some(ClipOrder::ByFileName),
            "File name".into(),
            "order-name",
        ),
        (Some(ClipOrder::Ignore), "Ignore".into(), "order-ignore"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_clip_order(mode, cx),
        ));
    }
    panel
}

/// Track-level time source (Syncaila Time source): which timestamp
/// evidence this track's clips may use. Compact radio rows; when the
/// selected evidence is missing on every clip the header says so and the
/// engine falls back to stable order without guessing.
fn time_source_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    use TemporalMode as M;
    let (effective, available) = menu
        .lane_id
        .as_deref()
        .map(|id| data.temporal_availability(id))
        .unwrap_or((M::Auto, true));
    let header = if available {
        "Time source".to_string()
    } else {
        let missing = match effective {
            M::Timecode => "timecode",
            M::RecStart | M::RecStop => "timestamps",
            M::Auto => "evidence",
        };
        format!("Time source — no {missing}, stable order")
    };
    panel = panel.child(menu_header(theme, header));
    let current = menu
        .lane_id
        .as_deref()
        .and_then(|id| data.lane_temporal_mode(id));
    for mode in [
        None,
        Some(M::Auto),
        Some(M::RecStart),
        Some(M::RecStop),
        Some(M::Timecode),
    ] {
        let title = mode.map_or_else(
            || {
                format!(
                    "Inherit ({})",
                    data.current_effective_settings().temporal_mode.title()
                )
            },
            |mode| mode.title().to_string(),
        );
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        let row_id = match mode {
            None => "time-inherit",
            Some(M::Auto) => "time-auto",
            Some(M::RecStart) => "time-rec-start",
            Some(M::RecStop) => "time-rec-stop",
            Some(M::Timecode) => "time-timecode",
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| {
                this.set_temporal_mode(mode, cx);
            },
        ));
    }
    panel
}

fn match_threshold_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_match_threshold(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Match threshold".to_string()));
    let inherited = data.current_effective_settings().match_threshold;
    for (threshold, title, row_id) in [
        (
            None,
            format!("Inherit ({})", match_threshold_label(inherited)),
            "match-inherit",
        ),
        (
            Some(MatchThreshold::Permissive),
            "More matches".into(),
            "match-more",
        ),
        (
            Some(MatchThreshold::Balanced),
            "Balanced".into(),
            "match-balanced",
        ),
        (
            Some(MatchThreshold::Conservative),
            "Fewer false matches".into(),
            "match-fewer",
        ),
    ] {
        let label = if threshold == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_match_threshold(threshold, cx),
        ));
    }
    panel
}

// ---------------- export sheet (mirrors ExportSheet)

fn export_section_title(theme: &Theme, text: &str) -> impl IntoElement {
    div()
        .text_sm()
        .text_color(rgb(theme.dim))
        .child(text.to_string())
}

/// Native-style checkbox row used by the export sheet.
fn check_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    checked: bool,
    title: String,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    // The tick hides in the unchecked box by matching its fill.
    let tick = if checked { theme.on_accent } else { theme.bg };
    let mut row = div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .rounded_md();
    if enabled {
        row = row
            .cursor_pointer()
            .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)));
    } else {
        row = row.opacity(0.6);
    }
    row.child(
        div()
            .w(px(16.))
            .h(px(16.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(if checked { theme.accent } else { theme.border }))
            .bg(rgb(if checked { theme.accent } else { theme.bg }))
            .child(svg_icon(icons().check.clone(), 10.0, tick)),
    )
    .child(
        div()
            .text_color(rgb(if enabled { theme.text } else { theme.dim }))
            .child(title),
    )
}

/// Compact single-choice row: a checkmark without checkbox chrome.
fn choice_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    checked: bool,
    title: String,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let mut row = div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md();
    if enabled {
        row = row
            .cursor_pointer()
            .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)));
    } else {
        row = row.opacity(0.6);
    }
    row.child(
        div()
            .w(px(16.))
            .h(px(16.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .when(checked, |slot| {
                slot.child(svg_icon(icons().check.clone(), 12.0, theme.accent))
            }),
    )
    .child(
        div()
            .text_color(rgb(if enabled { theme.text } else { theme.dim }))
            .child(title),
    )
}

fn step_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: &'static str,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let mut control = div()
        .id(id.into())
        .w(px(24.))
        .h(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_sm()
        .child(label);
    if enabled {
        control = control
            .text_color(rgb(theme.icon))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.button_hover)))
            .active(|this| this.opacity(0.62))
            .on_click(cx.listener(move |this, event, window, cx| action(this, event, window, cx)));
    } else {
        control = control.text_color(rgb(theme.dim)).opacity(0.42);
    }
    control
}

#[allow(clippy::too_many_arguments)]
fn seconds_stepper(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: &'static str,
    title: &'static str,
    value: f64,
    decimals: usize,
    enabled: bool,
    decrement: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
    increment: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let can_decrement = enabled && value > 0.0;
    let value_label = if value <= 0.0 {
        "Off".to_string()
    } else {
        format!("{value:.decimals$} s")
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(28.))
        .px_2()
        .child(div().flex_1().text_color(rgb(theme.text)).child(title))
        .child(step_button(
            cx,
            theme,
            format!("{id}-minus"),
            "−",
            can_decrement,
            decrement,
        ))
        .child(
            div()
                .w(px(54.))
                .text_center()
                .text_sm()
                .text_color(rgb(theme.dim))
                .child(value_label),
        )
        .child(step_button(
            cx,
            theme,
            format!("{id}-plus"),
            "+",
            enabled,
            increment,
        ))
}

fn export_text_field(
    theme: &Theme,
    title: &'static str,
    input: Entity<TextInput>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(32.))
        .px_2()
        .child(
            div()
                .w(px(144.))
                .flex_shrink_0()
                .text_color(rgb(theme.text))
                .child(title),
        )
        .child(div().flex_1().min_w(px(0.)).child(input))
}

fn path_fixer_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    inputs: &PathFixerInputs,
    max_height: f32,
) -> impl IntoElement {
    let mut sheet = div()
        .id("path-fixer-sheet")
        .flex()
        .flex_col()
        .gap_4()
        .p_5()
        .m_4()
        .w(px(520.))
        .max_h(px(max_height))
        .flex_shrink_0()
        .overflow_y_scroll()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Path Fixer",
            "path-cancel",
            |this, _, _, cx| {
                this.data.discard_path_redirection_edits();
                this.data.show_path_fixer = false;
                cx.notify();
            },
        ));

    let mut saved = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(export_section_title(theme, "Saved redirects"));
    if data.redirects.is_empty() {
        saved = saved.child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px_2()
                .text_color(rgb(theme.dim))
                .child("None"),
        );
    } else {
        for (index, redirect) in data.redirects.iter().enumerate() {
            saved = saved.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(32.))
                    .px_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .whitespace_nowrap()
                                    .text_color(rgb(theme.text))
                                    .child(redirect.from_prefix.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(rgb(theme.dim))
                                    .child(format!("→ {}", redirect.to_dir.display())),
                            ),
                    )
                    .child(button(
                        cx,
                        theme,
                        format!("path-remove-{index}"),
                        "Remove",
                        true,
                        move |this, _, _, cx| {
                            this.data.remove_path_redirection(index);
                            cx.notify();
                        },
                    )),
            );
        }
    }
    sheet = sheet.child(saved);

    let mut add = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(export_section_title(theme, "New redirect"))
        .child(export_text_field(
            theme,
            "Old folder",
            inputs.old_folder.clone(),
        ));
    let target = data
        .path_fixer_dir
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Not selected".to_string());
    add = add.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(32.))
            .px_2()
            .child(
                div()
                    .w(px(144.))
                    .flex_shrink_0()
                    .text_color(rgb(theme.text))
                    .child("New folder"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .whitespace_nowrap()
                    .text_color(rgb(if data.path_fixer_dir.is_some() {
                        theme.text
                    } else {
                        theme.dim
                    }))
                    .child(target),
            )
            .child(button(
                cx,
                theme,
                "path-choose-dir",
                "Choose…",
                true,
                |this, _, _, cx| this.choose_redirection_dir(cx),
            )),
    );
    add = add.child(div().flex().flex_row().justify_end().px_2().child(button(
        cx,
        theme,
        "path-add-redirect",
        "Add Redirect",
        data.path_fixer_dir.is_some(),
        |this, _, _, cx| this.add_path_redirection(cx),
    )));
    sheet = sheet.child(add);

    sheet = sheet.child(check_row(
        cx,
        theme,
        "prefer-proxies",
        data.path_fixer_prefer_proxies,
        "Prefer FCPXML proxies when available".to_string(),
        true,
        |this, _, _, cx| {
            this.data.path_fixer_prefer_proxies = !this.data.path_fixer_prefer_proxies;
            cx.notify();
        },
    ));

    sheet = sheet.child(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(export_section_title(theme, "Ignore timeline media"))
            .child(export_text_field(
                theme,
                "Extensions",
                inputs.omit_extensions.clone(),
            )),
    );

    sheet.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(div().flex_1())
            .child(button(
                cx,
                theme,
                "path-apply",
                "Apply",
                true,
                |this, _, _, cx| this.finish_path_fixer(cx),
            ))
            .child(prominent_button(
                cx,
                theme,
                "path-save-fixed-copy",
                "Save Fixed Copy…",
                data.path_repair_source().is_some(),
                |this, _, _, cx| this.save_fixed_project_copy(cx),
            )),
    )
}

fn export_scrollbar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    scroll: &ScrollHandle,
) -> impl IntoElement {
    let viewport_height = f32::from(scroll.bounds().size.height).max(1.);
    let max_scroll = f32::from(scroll.max_offset().height).max(0.);
    let content_height = viewport_height + max_scroll;
    let thumb_height = if max_scroll > 0. {
        (viewport_height * viewport_height / content_height).clamp(36., viewport_height)
    } else {
        48_f32.min(viewport_height)
    };
    let travel = (viewport_height - thumb_height).max(1.);
    let progress = if max_scroll > 0. {
        (-f32::from(scroll.offset().y) / max_scroll).clamp(0., 1.)
    } else {
        0.
    };
    let thumb_top = progress * travel;

    div()
        .id("export-scrollbar")
        .relative()
        .w(px(10.))
        .h_full()
        .ml_2()
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(theme.separator))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                let bounds = this.export_scroll.bounds();
                let viewport_height = f32::from(bounds.size.height).max(1.);
                let max_scroll = f32::from(this.export_scroll.max_offset().height).max(0.);
                let content_height = viewport_height + max_scroll;
                let thumb_height = if max_scroll > 0. {
                    (viewport_height * viewport_height / content_height).clamp(36., viewport_height)
                } else {
                    viewport_height
                };
                let travel = (viewport_height - thumb_height).max(1.);
                let current_offset = f32::from(this.export_scroll.offset().y);
                let current_top = if max_scroll > 0. {
                    (-current_offset / max_scroll).clamp(0., 1.) * travel
                } else {
                    0.
                };
                let local_y = f32::from(event.position.y - bounds.origin.y);
                let start_offset = if local_y < current_top || local_y > current_top + thumb_height
                {
                    let progress = ((local_y - thumb_height * 0.5) / travel).clamp(0., 1.);
                    let offset = -max_scroll * progress;
                    this.export_scroll.set_offset(point(px(0.), px(offset)));
                    offset
                } else {
                    current_offset
                };
                this.export_scroll_drag = Some((f32::from(event.position.y), start_offset));
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
            if !event.dragging() {
                this.export_scroll_drag = None;
                return;
            }
            let Some((pointer_start, offset_start)) = this.export_scroll_drag else {
                return;
            };
            let viewport_height = f32::from(this.export_scroll.bounds().size.height).max(1.);
            let max_scroll = f32::from(this.export_scroll.max_offset().height).max(0.);
            let content_height = viewport_height + max_scroll;
            let thumb_height = if max_scroll > 0. {
                (viewport_height * viewport_height / content_height).clamp(36., viewport_height)
            } else {
                viewport_height
            };
            let travel = (viewport_height - thumb_height).max(1.);
            let delta = f32::from(event.position.y) - pointer_start;
            let offset = (offset_start - delta * max_scroll / travel).clamp(-max_scroll, 0.);
            this.export_scroll.set_offset(point(px(0.), px(offset)));
            cx.stop_propagation();
            cx.notify();
        }))
        .capture_any_mouse_up(cx.listener(|this, _, _, _| {
            this.export_scroll_drag = None;
        }))
        .child(
            div()
                .absolute()
                .top(px(thumb_top))
                .left(px(1.))
                .right(px(1.))
                .h(px(thumb_height))
                .rounded_full()
                .bg(rgb(theme.dim))
                .hover(|this| this.bg(rgb(theme.icon))),
        )
}

fn export_sidebar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    inputs: &ExportInputs,
    scroll: &ScrollHandle,
    closing: bool,
) -> impl IntoElement {
    let can_close = !matches!(data.operation, Operation::Exporting);
    let panel = div()
        .w(px(440.))
        .h_full()
        .flex()
        .flex_col()
        .border_l_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.panel))
        .child(
            div()
                .h(px(48.))
                .px_3()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .border_b_1()
                .border_color(rgb(theme.separator))
                .child({
                    let mut close = div()
                        .id("btn-close-export-sidebar")
                        .w(px(30.))
                        .h(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(theme.separator));
                    if can_close {
                        close = close
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(theme.button_hover)))
                            .active(|this| this.opacity(0.62))
                            .tooltip(hover_tip("Close export panel".to_string(), theme))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_export_sidebar(cx);
                            }));
                    } else {
                        close = close.opacity(0.32);
                    }
                    close.child(svg_icon(icons().chevron_right.clone(), 12., theme.icon))
                })
                .child(div().flex_1()),
        )
        .child(export_sheet(cx, theme, data, inputs, scroll))
        .cursor_default()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
    slide_in_from_right(
        panel,
        if closing {
            "export-sidebar-slide-out"
        } else {
            "export-sidebar-slide-in"
        },
        440.,
        closing,
    )
}

fn export_sidebar_action(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    div()
        .absolute()
        .top(px(10.))
        .right(px(12.))
        .child(prominent_button(
            cx,
            theme,
            "btn-export-sidebar",
            "Export",
            data.can_begin_export(),
            |this, _, _, cx| this.begin_export(cx),
        ))
}

fn export_sheet(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    inputs: &ExportInputs,
    scroll: &ScrollHandle,
) -> Stateful<Div> {
    use super::state::{ExportTarget, Operation};
    let busy = matches!(data.operation, Operation::Exporting);
    let done = matches!(data.operation, Operation::Exported) && data.export_started;
    let mut sheet = div()
        .id("export-sheet")
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex_shrink_0()
        .overflow_hidden();
    // Destination.
    {
        let mut row = div().flex().flex_row().items_center().gap_3().h(px(32.));
        row = row.child(export_section_title(theme, "Destination"));
        row = row.child(
            div()
                .flex_1()
                .truncate()
                .whitespace_nowrap()
                .text_color(rgb(if data.export_dir.is_some() {
                    theme.text
                } else {
                    theme.dim
                }))
                .child(
                    data.export_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Not selected".to_string()),
                ),
        );
        row = row.child(button(
            cx,
            theme,
            "btn-exp-dir",
            "Choose…",
            !busy,
            |this, _, _, cx| this.choose_export_dir(cx),
        ));
        sheet = sheet.child(row);
    }
    let mut columns = div()
        .id("export-sidebar-scroll")
        .flex()
        .flex_col()
        .gap_4()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
        .pb_2();
    let mut settings = div().flex().flex_col().gap_3();
    // Formats.
    {
        let mut group = div().flex().flex_col().gap_1().flex_shrink_0();
        group = group.child(export_section_title(theme, "Formats"));
        for target in ExportTarget::all() {
            let active = data.export_selected.contains(&target);
            let title = target.title().to_string();
            group = group.child(check_row(
                cx,
                theme,
                format!("exp-fmt-{}", target.title()),
                active,
                title,
                !busy,
                move |this, _, _, cx| {
                    if this.data.export_selected.contains(&target) {
                        this.data.export_selected.remove(&target);
                    } else {
                        this.data.export_selected.insert(target);
                    }
                    cx.notify();
                },
            ));
        }
        group = group.child(check_row(
            cx,
            theme,
            "exp-media",
            data.export_media,
            "Video files with external audio (.mov)".to_string(),
            !busy,
            |this, _, _, cx| {
                this.data.export_media = !this.data.export_media;
                cx.notify();
            },
        ));
        if data.export_selected.contains(&ExportTarget::FinalCutPro) {
            group = group.child(check_row(
                cx,
                theme,
                "exp-fcpxml-timeline",
                data.export_fcpxml_timeline,
                "Include synchronized timeline".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_fcpxml_timeline = !this.data.export_fcpxml_timeline;
                    cx.notify();
                },
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-fcpxml-multicam",
                data.export_fcpxml_multicam,
                "Include multicam clip".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_fcpxml_multicam = !this.data.export_fcpxml_multicam;
                    cx.notify();
                },
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-storylines",
                data.export_storylines,
                "Group FCPXML tracks as storylines".to_string(),
                !busy && data.export_fcpxml_timeline,
                |this, _, _, cx| {
                    this.data.export_storylines = !this.data.export_storylines;
                    cx.notify();
                },
            ));
        }
        if data.export_selected.contains(&ExportTarget::Premiere) {
            let on = data.export_replaced;
            group = group.child(check_row(
                cx,
                theme,
                "exp-replaced",
                on,
                "Also add a sequence with external audio replacing camera audio".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_replaced = !this.data.export_replaced;
                    cx.notify();
                },
            ));
        }
        columns = columns.child(group);
    }
    {
        // Only offer rendering when the result contains measured drift.
        if data.has_drift() {
            let mut group = div().flex().flex_col();
            group = group.child(export_section_title(theme, "Audio"));
            let on = data.export_drift && data.has_drift();
            group = group.child(check_row(
                cx,
                theme,
                "exp-drift",
                on,
                "Render drift-corrected audio".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_drift = !this.data.export_drift;
                    cx.notify();
                },
            ));
            settings = settings.child(group);
        }
        if data.export_selected.contains(&ExportTarget::Aaf) {
            let mut group = div().flex().flex_col().gap_1();
            group = group.child(export_section_title(theme, "Resolve AAF frame rate"));
            for (id, frame_duration, title) in [
                ("auto", None, "Automatic"),
                (
                    "23976",
                    Some(align_core::MediaTime::new(1001, 24_000)),
                    "23.976",
                ),
                ("24", Some(align_core::MediaTime::new(1, 24)), "24"),
                ("25", Some(align_core::MediaTime::new(1, 25)), "25"),
                (
                    "2997",
                    Some(align_core::MediaTime::new(1001, 30_000)),
                    "29.97",
                ),
                ("30", Some(align_core::MediaTime::new(1, 30)), "30"),
                ("50", Some(align_core::MediaTime::new(1, 50)), "50"),
                (
                    "5994",
                    Some(align_core::MediaTime::new(1001, 60_000)),
                    "59.94",
                ),
                ("60", Some(align_core::MediaTime::new(1, 60)), "60"),
            ] {
                group = group.child(choice_row(
                    cx,
                    theme,
                    format!("exp-aaf-fps-{id}"),
                    data.export_aaf_frame_duration == frame_duration,
                    title.to_string(),
                    !busy,
                    move |this, _, _, cx| {
                        this.data.export_aaf_frame_duration = frame_duration;
                        cx.notify();
                    },
                ));
            }
            settings = settings.child(group);
        }
        // Timeline assembly.
        {
            let mut group = div().flex().flex_col().gap_1();
            group = group.child(export_section_title(theme, "Timeline"));
            let on = data.export_prevent_overlaps;
            group = group.child(check_row(
                cx,
                theme,
                "exp-prevent-overlaps",
                on,
                "Prevent overlaps".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_prevent_overlaps = !this.data.export_prevent_overlaps;
                    cx.notify();
                },
            ));
            settings = settings.child(group);
        }
        // Cut / Remove.
        {
            let mut group = div().flex().flex_col().gap_1();
            group = group.child(export_section_title(theme, "Cleanup"));
            group = group.child(check_row(
                cx,
                theme,
                "exp-cut-gaps",
                data.export_cut_common_gaps,
                "Cut gaps empty on all tracks".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_cut_common_gaps = !this.data.export_cut_common_gaps;
                    cx.notify();
                },
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-cut-lone",
                data.export_cut_lone_recorder,
                "Cut recorder audio with no camera".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_cut_lone_recorder = !this.data.export_cut_lone_recorder;
                    cx.notify();
                },
            ));
            {
                group = group.child(seconds_stepper(
                    cx,
                    theme,
                    "exp-cut-short",
                    "Remove clips shorter than",
                    data.export_cut_shorter_than,
                    0,
                    !busy,
                    |this, _, _, cx| {
                        this.data.export_cut_shorter_than =
                            (this.data.export_cut_shorter_than - 1.0).max(0.0);
                        cx.notify();
                    },
                    |this, _, _, cx| {
                        this.data.export_cut_shorter_than =
                            (this.data.export_cut_shorter_than + 1.0).min(3_600.0);
                        cx.notify();
                    },
                ));
                group = group.child(seconds_stepper(
                    cx,
                    theme,
                    "exp-trim-start",
                    "Trim each clip start",
                    data.export_trim_starts,
                    1,
                    !busy,
                    |this, _, _, cx| {
                        this.data.export_trim_starts =
                            ((this.data.export_trim_starts - 0.1).max(0.0) * 10.0).round() / 10.0;
                        cx.notify();
                    },
                    |this, _, _, cx| {
                        this.data.export_trim_starts =
                            ((this.data.export_trim_starts + 0.1).min(3_600.0) * 10.0).round()
                                / 10.0;
                        cx.notify();
                    },
                ));
                group = group.child(seconds_stepper(
                    cx,
                    theme,
                    "exp-trim-end",
                    "Trim each clip end",
                    data.export_trim_ends,
                    1,
                    !busy,
                    |this, _, _, cx| {
                        this.data.export_trim_ends =
                            ((this.data.export_trim_ends - 0.1).max(0.0) * 10.0).round() / 10.0;
                        cx.notify();
                    },
                    |this, _, _, cx| {
                        this.data.export_trim_ends =
                            ((this.data.export_trim_ends + 0.1).min(3_600.0) * 10.0).round() / 10.0;
                        cx.notify();
                    },
                ));
            }
            settings = settings.child(group);
        }
        if data.unmatched_count() > 0 {
            use align_core::export_model::UnmatchedPlacement as P;
            let mut group = div().flex().flex_col();
            group = group.child(export_section_title(theme, "Unmatched clips"));
            for (placement, title, id) in [
                (P::ByOrderAndTime, "By order & time", "exp-unmatched-time"),
                (P::ByOrderOnly, "By order only", "exp-unmatched-order"),
                (P::Remove, "Remove", "exp-unmatched-remove"),
            ] {
                group = group.child(choice_row(
                    cx,
                    theme,
                    id,
                    data.export_unmatched == placement,
                    title.to_string(),
                    !busy,
                    move |this, _, _, cx| {
                        this.data.export_unmatched = placement;
                        cx.notify();
                    },
                ));
            }
            group = group.child(check_row(
                cx,
                theme,
                "exp-disable-unmatched",
                data.export_disable_unmatched,
                "Disable in timeline".to_string(),
                !busy && data.export_unmatched != P::Remove,
                |this, _, _, cx| {
                    this.data.export_disable_unmatched = !this.data.export_disable_unmatched;
                    cx.notify();
                },
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-label-unmatched",
                data.export_label_unmatched,
                "Mark names as [UNSYNCED]".to_string(),
                !busy && data.export_unmatched != P::Remove,
                |this, _, _, cx| {
                    this.data.export_label_unmatched = !this.data.export_label_unmatched;
                    cx.notify();
                },
            ));
            settings = settings.child(group);
        }
    }
    // Naming stays in the same scrollable form as the processing settings.
    {
        let mut group = div().flex().flex_col().gap_1();
        group = group.child(export_section_title(theme, "Names & labels"));
        {
            group = group.child(export_text_field(
                theme,
                "Sequence name",
                inputs.sequence_name.clone(),
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-label-synced",
                data.export_label_synced,
                "Mark names as [SYNCED]".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_label_synced = !this.data.export_label_synced;
                    cx.notify();
                },
            ));
            group = group.child(export_text_field(
                theme,
                "Synchronized symbol",
                inputs.synced_symbol.clone(),
            ));
            group = group.child(check_row(
                cx,
                theme,
                "exp-synced-symbol-suffix",
                data.export_synced_symbol_suffix,
                "Put synchronized symbol after clip name".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export_synced_symbol_suffix = !this.data.export_synced_symbol_suffix;
                    cx.notify();
                },
            ));
            if data.export_selected.contains(&ExportTarget::Premiere)
                || data.export_selected.contains(&ExportTarget::ResolveXml)
            {
                group = group.child(export_text_field(
                    theme,
                    "Synchronized XML color",
                    inputs.synced_color.clone(),
                ));
            }
            if data.export_selected.contains(&ExportTarget::FinalCutPro) {
                group = group.child(export_text_field(
                    theme,
                    "Synchronized Final Cut role",
                    inputs.synced_role.clone(),
                ));
            }
            if data.unmatched_count() > 0 {
                group = group.child(export_text_field(
                    theme,
                    "Unmatched symbol",
                    inputs.unmatched_symbol.clone(),
                ));
                group = group.child(check_row(
                    cx,
                    theme,
                    "exp-symbol-suffix",
                    data.export_unmatched_symbol_suffix,
                    "Put symbol after clip name".to_string(),
                    !busy,
                    |this, _, _, cx| {
                        this.data.export_unmatched_symbol_suffix =
                            !this.data.export_unmatched_symbol_suffix;
                        cx.notify();
                    },
                ));
                if data.export_selected.contains(&ExportTarget::Premiere)
                    || data.export_selected.contains(&ExportTarget::ResolveXml)
                {
                    group = group.child(export_text_field(
                        theme,
                        "Unmatched XML color",
                        inputs.unmatched_color.clone(),
                    ));
                }
                if data.export_selected.contains(&ExportTarget::FinalCutPro) {
                    group = group.child(export_text_field(
                        theme,
                        "Unmatched Final Cut role",
                        inputs.unmatched_role.clone(),
                    ));
                }
            }
        }
        settings = settings.child(group);
    }
    columns = columns.child(div().id("export-settings").min_w(px(0.)).child(settings));
    sheet = sheet.child(
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .child(columns)
            .child(export_scrollbar(cx, theme, scroll)),
    );
    // Progress / completion.
    if data.export_started && busy {
        sheet = sheet.child(data.status.clone());
        if data.export_drift || data.export_media {
            sheet = sheet.child(
                div()
                    .w(px(420.))
                    .h(px(8.))
                    .rounded_full()
                    .bg(rgb(theme.border))
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(rgb(theme.accent))
                            .w(gpui::relative(data.progress.clamp(0.0, 1.0))),
                    ),
            );
        }
    } else if done {
        sheet = sheet.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_color(rgb(theme.green))
                .child(svg_icon(icons().check.clone(), 14.0, theme.green))
                .child("Export complete"),
        );
    }
    // Sidebar actions. In-flight cancellation stays in the persistent
    // operation bar so it is not duplicated here.
    if done && !busy {
        let mut row = div()
            .flex()
            .flex_row()
            .gap_2()
            .flex_shrink_0()
            .pt_3()
            .border_t_1()
            .border_color(rgb(theme.separator));
        row = row.child(div().flex_1());
        row = row.child(button(
            cx,
            theme,
            "exp-reveal",
            "Show in Finder",
            true,
            |this, _, _, cx| {
                this.data.reveal_export();
                cx.notify();
            },
        ));
        row = row.child(prominent_button(
            cx,
            theme,
            "exp-done",
            "Done",
            true,
            |this, _, _, cx| {
                this.close_export_sidebar(cx);
            },
        ));
        sheet = sheet.child(row);
    }
    sheet
}

// ---------------- about panel (Align menu → About Align)

fn about_panel(cx: &mut Context<AlignApp>, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .p_6()
        .m_4()
        .w(px(360.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .child(modal_header(
            cx,
            theme,
            "Align",
            "btn-about-close",
            |this, _, _, cx| {
                this.data.show_about = false;
                cx.notify();
            },
        ))
        .child(
            div()
                .text_color(rgb(theme.dim))
                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
        )
        .child(div().child("Cross-platform media synchronizer"))
}

// ---------------- error alert (mirrors .alert)

fn error_alert(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let panel = div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .m_4()
        .max_w(px(420.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Align could not finish",
            "btn-alert-close",
            |this, _, _, cx| {
                this.data.dismiss_error();
                cx.notify();
            },
        ))
        .child(
            data.error
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        );
    if data.can_locate_timeline_media() {
        panel.child(button(
            cx,
            theme,
            "btn-error-relink-media",
            "Locate media…",
            true,
            |this, _, _, cx| this.choose_missing_media(cx),
        ))
    } else {
        panel
    }
}
