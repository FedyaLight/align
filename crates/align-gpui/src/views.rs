//! GPUI views: faithful layout port of the SwiftUI app.
//!
//! Structure mirrors `ContentView`: toolbar (Add / Delete / diagnostics),
//! diagnostics), main content (drop zone → source list → timeline
//! preview), warning banner, bottom operation bar, plus overlays for the
//! export sheet, alert, diagnostics, warning details and sequence picker.
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
    Animation, AnimationExt, AnyView, App, ClickEvent, Context, Corner, Div, DragMoveEvent, Entity,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, PathPromptOptions, Pixels, Point, Render, SharedString, Stateful, Styled,
    Window, anchored, deferred, div, prelude::*, px, rgb, rgba,
};

use super::icons::{icons, kind_badge, svg_icon};
use super::lane::CorrectionOption;
use super::state::{AppData, ClipState, MenuTarget, Operation, SequencePicker};
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
    Progress(Box<SyncProgress>),
    Done(Box<Result<SyncResult, String>>),
}

struct SyncProgress {
    phase: Phase,
    completed: usize,
    total: usize,
    current: Option<PathBuf>,
    discovered: Option<Clip>,
    preview: Option<MatchPreview>,
}

enum ExportMsg {
    Progress { label: String, fraction: f32 },
    Done(Result<Vec<ExportArtifact>, String>),
}

// ------------------------------------------------------------ view

#[derive(Clone)]
struct ExportInputs {
    sequence_name: Entity<TextInput>,
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
            unmatched_symbol: cx.new(|cx| TextInput::new(cx, "e.g. [UNSYNCED]")),
            unmatched_color: cx.new(|cx| TextInput::new(cx, "e.g. Rose")),
            unmatched_role: cx.new(|cx| TextInput::new(cx, "e.g. Dialogue")),
        }
    }

    fn any_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.sequence_name,
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
        if !self.data.begin_sync_run() {
            return;
        }
        cx.notify();

        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let inputs = self.data.pipeline_inputs();
        let constraints = self.data.constraints.clone();
        let options = self.data.pipeline_options();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<SyncMsg>();
        std::thread::spawn(move || {
            let pipeline = Pipeline::default_backend();
            let progress = |p: align_decode::pipeline::PipelineProgress| {
                let _ = tx.unbounded_send(SyncMsg::Progress(Box::new(SyncProgress {
                    phase: p.phase,
                    completed: p.completed,
                    total: p.total,
                    current: p.current,
                    discovered: p.discovered,
                    preview: p.preview,
                })));
            };
            let result =
                pipeline.synchronize(&inputs, &constraints, &options, Some(&progress), &cancel);
            let _ = tx.unbounded_send(SyncMsg::Done(Box::new(result.map_err(|e| e.to_string()))));
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
            }
            SyncMsg::Done(boxed) => match *boxed {
                Ok(result) => self.data.apply_result(result),
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
        self.data.export_started = false;
        self.data.error = None;
        cx.notify();
    }

    pub(crate) fn open_path_fixer(&mut self, cx: &mut Context<Self>) {
        self.data.discard_path_redirection_edits();
        self.data.path_fixer_prefer_proxies = self.data.prefer_proxies;
        let omitted = self.data.omit_extensions.join(", ");
        self.path_fixer_inputs
            .omit_extensions
            .update(cx, |input, cx| input.set_text(omitted, cx));
        self.data.show_export = false;
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

    fn finish_path_fixer(&mut self, cx: &mut Context<Self>) {
        let omitted = self.path_fixer_inputs.omit_extensions.read(cx).text();
        self.data.set_omit_extensions(&omitted);
        self.data.prefer_proxies = self.data.path_fixer_prefer_proxies;
        self.data.save_path_redirections();
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
        let unmatched_symbol = ExportInputs::value(&self.export_inputs.unmatched_symbol, cx);
        let unmatched_color = ExportInputs::value(&self.export_inputs.unmatched_color, cx);
        let unmatched_role = ExportInputs::value(&self.export_inputs.unmatched_role, cx);
        let Ok(timeline) = ExportTimeline::from_result_with_options(
            &result,
            ExportAssemblyOptions {
                unmatched: self.data.export_unmatched,
                prevent_group_overlaps: self.data.export_prevent_overlaps,
                disable_unmatched: self.data.export_disable_unmatched,
                label_unmatched: self.data.export_label_unmatched,
                cut_remove: CutRemoveOptions {
                    common_gaps: self.data.export_cut_common_gaps,
                    lone_recorder: self.data.export_cut_lone_recorder,
                    shorter_than: self.data.export_cut_shorter_than,
                    trim_starts: self.data.export_trim_starts,
                    trim_ends: self.data.export_trim_ends,
                },
                unmatched_symbol,
                unmatched_symbol_suffix: self.data.export_unmatched_symbol_suffix,
                unmatched_color,
                unmatched_role,
                sequence_name,
            },
        ) else {
            self.data.error = Some("Nothing to export.".to_string());
            cx.notify();
            return;
        };
        self.data.operation = Operation::Exporting;
        self.data.progress = 0.0;
        self.data.export_started = true;
        self.data.error = None;
        self.data.status = if self.data.export_drift && self.data.has_drift() {
            "Preparing drift-corrected audio…".to_string()
        } else if self.data.export_media {
            "Exporting clean-audio video…".to_string()
        } else {
            "Writing timelines…".to_string()
        };
        cx.notify();

        let generation = self.data.generation;
        let cancel = self.data.cancel.clone();
        let drift = self.data.export_drift;
        let replaced = self.data.export_replaced;
        let storylines = self.data.export_storylines;
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
            let out = align_decode::export::export_prepared(
                align_decode::export::ExportRequest {
                    backend: pipeline.backend(),
                    timeline: &timeline,
                    directory: &dir,
                    formats: &formats,
                    correct_drift: drift,
                    include_replaced_sequence: replaced,
                    include_media_files: media,
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

    fn set_stream_source(&mut self, source: AudioAnalysisSource, cx: &mut Context<Self>) {
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

    fn set_temporal_mode(&mut self, mode: TemporalMode, cx: &mut Context<Self>) {
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

    fn set_match_threshold(&mut self, threshold: MatchThreshold, cx: &mut Context<Self>) {
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

    fn set_clip_order(&mut self, mode: ClipOrder, cx: &mut Context<Self>) {
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

    fn set_track_content(&mut self, mode: TrackContent, cx: &mut Context<Self>) {
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
            && !self.data.show_export
            && !self.data.show_path_fixer
            && self.data.error.is_none()
            && self.data.sequence_picker.is_none()
            && !self.data.show_about
            && !self.data.show_search_settings
            && !self.data.show_stage_settings;
        let mut root = div()
            .id("app-root")
            .flex()
            .flex_col()
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
                    this.data.show_export = false;
                    cx.notify();
                    return;
                }
                if key == "escape" && this.data.show_path_fixer {
                    this.data.discard_path_redirection_edits();
                    this.data.show_path_fixer = false;
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
        // No toolbar over the empty drop zone: nothing to act on yet.
        if !(self.data.lanes.is_empty() && self.data.clips.is_empty()) {
            root = root.child(toolbar(cx, &theme, &self.data, content_active));
        }
        root = root.child(main_content(
            cx,
            &theme,
            &self.data,
            fit_width,
            content_active,
        ));
        if !self.data.warnings.is_empty() {
            root = root.child(warning_banner(cx, &theme, &self.data));
            if self.data.show_warning_details {
                root = root.child(warning_details(&theme, &self.data));
            }
        }
        // No bottom bar over the empty drop zone either.
        if !self.data.clips.is_empty() {
            root = root.child(operation_bar(cx, &theme, &self.data));
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
        if self.data.show_stage_settings {
            root = root.child(overlay(
                &theme,
                "overlay-stages",
                stage_settings_panel(cx, &theme, &self.data),
            ));
        }
        if self.data.show_search_settings {
            root = root.child(overlay(
                &theme,
                "overlay-search",
                search_settings_panel(cx, &theme, &self.data),
            ));
        }
        if self.data.show_export {
            root = root.child(overlay(
                &theme,
                "overlay-export",
                export_sheet(
                    cx,
                    &theme,
                    &self.data,
                    &self.export_inputs,
                    (f32::from(window.viewport_size().height) - 64.).max(200.),
                ),
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

// ---------------- toolbar (Add / Delete / diagnostics)

fn toolbar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
) -> impl IntoElement {
    let live = matches!(data.operation, super::state::Operation::Synchronizing);
    let has_timeline = !data.lanes.is_empty();
    let mut bar = div()
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
        bar = bar.child(div().text_color(rgb(theme.dim)).child(format!(
            "{} item{}",
            data.clips.len(),
            if data.clips.len() == 1 { "" } else { "s" }
        )));
    }
    // Timeline section (only once there is something to preview).
    if has_timeline {
        bar = bar.child(icon_button(
            cx,
            theme,
            "zoom-fit",
            icons().fit.clone(),
            "Zoom to fit",
            data.zoom_level > 0.001,
            |this, _, _, cx| {
                this.data.zoom_at(0.0, 0.0);
                this.data.pan_to(Some(0.0), None);
                cx.notify();
            },
        ));
        bar = bar.child(icon_button(
            cx,
            theme,
            "zoom-out",
            icons().zoom_out.clone(),
            "Zoom out",
            data.zoom_level > 0.0,
            |this, _, _, cx| {
                let bounds = this.data.timeline_scroll.bounds();
                let anchor = f32::from(bounds.size.width) * 0.5;
                this.data
                    .zoom_at((this.data.zoom_level - 0.2).max(0.0), anchor);
                cx.notify();
            },
        ));
        bar = bar.child(zoom_slider(cx, theme, data, active));
        bar = bar.child(icon_button(
            cx,
            theme,
            "zoom-in",
            icons().zoom_in.clone(),
            "Zoom in",
            data.zoom_level < 1.0,
            |this, _, _, cx| {
                let bounds = this.data.timeline_scroll.bounds();
                let anchor = f32::from(bounds.size.width) * 0.5;
                this.data
                    .zoom_at((this.data.zoom_level + 0.2).min(1.0), anchor);
                cx.notify();
            },
        ));
        // Fixed width: the percent must not reflow the toolbar while zooming.
        bar = bar.child(
            div()
                .w(px(48.))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .justify_end()
                .text_color(rgb(theme.dim))
                .child(data.zoom_label()),
        );
    }
    bar
}

// ---------------- main content (drop zone → file list → timeline)

fn main_content(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    fit_width: f32,
    active: bool,
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
            this.data.selection.clear();
            cx.notify();
        }));
    }
    if data.lanes.is_empty() {
        if data.clips.is_empty() {
            content = content.child(drop_zone(cx, theme, data, active));
        } else {
            content = content.child(file_list(cx, theme, data, active));
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

// ---------------- source list (selectable rows; header lives in the toolbar)

fn file_list(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    active: bool,
) -> impl IntoElement {
    let mut list = div().flex().flex_col().m_4().gap_2();
    let selecting = !data.selection.is_empty();
    if selecting {
        let all = data
            .clips
            .iter()
            .all(|clip| data.selection.contains(&clip.url));
        list = list.child(check_row(
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
                } else {
                    this.data.selection = this
                        .data
                        .clips
                        .iter()
                        .map(|clip| clip.url.clone())
                        .collect();
                }
                cx.notify();
            },
        ));
    }
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
            .gap_3()
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
                } else {
                    this.data.selection.insert(url.clone());
                }
                cx.notify();
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
        if selecting {
            row = row.child(
                div()
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
                    }),
            );
        }
        row = row
            .child(
                div()
                    .w(px(18.))
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
        let override_suffix = match lane.analysis_source {
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
) -> impl IntoElement {
    use super::state::Operation;
    let busy = matches!(
        data.operation,
        Operation::Synchronizing | Operation::Exporting
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
    if data.is_stale() {
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
        bar = bar.child(button(
            cx,
            theme,
            "btn-search-settings",
            format!("Search: {}", data.search_accuracy.label()),
            true,
            |this, _, _, cx| {
                this.data.show_search_settings = true;
                cx.notify();
            },
        ));
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
        if !data.exported_files.is_empty() {
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
        if data.can_export() {
            bar = bar.child(button(
                cx,
                theme,
                "btn-sync-bar",
                "Synchronize",
                data.can_synchronize(),
                |this, _, _, cx| this.start_sync(cx),
            ));
            bar = bar.child(prominent_button(
                cx,
                theme,
                "btn-export-bar",
                "Export",
                true,
                |this, _, _, cx| this.start_export_sheet(cx),
            ));
        } else {
            bar = bar.child(prominent_button(
                cx,
                theme,
                "btn-sync-bar",
                "Synchronize",
                data.can_synchronize(),
                |this, _, _, cx| this.start_sync(cx),
            ));
        }
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
        .child(div().text_size(px(16.)).child("Synchronization stages"))
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
    panel.child(button(
        cx,
        theme,
        "stage-close",
        "Close",
        true,
        |this, _, _, cx| {
            this.data.show_stage_settings = false;
            cx.notify();
        },
    ))
}

fn search_settings_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
) -> impl IntoElement {
    let mut panel = div().id("search-settings").w(px(360.)).p_4().flex().flex_col()
        .gap_2().rounded_lg().bg(rgb(theme.panel)).border_1().border_color(rgb(theme.border))
        .child(div().text_size(px(16.)).child("Search accuracy"))
        .child(div().text_size(px(12.)).text_color(rgb(theme.dim))
            .child("Deeper levels search more sound detail and use more time and memory. Changing the level runs synchronization again."));
    for (index, accuracy) in align_core::SearchAccuracy::ALL.into_iter().enumerate() {
        let title = if accuracy == data.search_accuracy {
            format!("✓ {}", accuracy.label())
        } else {
            accuracy.label().to_string()
        };
        panel = panel.child(button(
            cx,
            theme,
            format!("search-level-{index}"),
            title,
            true,
            move |this, _, _, cx| {
                this.data.show_search_settings = false;
                if this.data.search_accuracy != accuracy {
                    this.data.search_accuracy = accuracy;
                    this.start_sync(cx);
                } else {
                    cx.notify();
                }
            },
        ));
    }
    panel.child(button(
        cx,
        theme,
        "search-close",
        "Close",
        true,
        |this, _, _, cx| {
            this.data.show_search_settings = false;
            cx.notify();
        },
    ))
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
        .child("Choose a sequence")
        .child(div().text_color(rgb(theme.dim)).child(format!(
                "{} contains multiple timelines.",
                picker
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("timeline")
            )));
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
    panel = panel.child(button(
        cx,
        theme,
        "seq-cancel",
        "Cancel",
        true,
        |this, _, _, cx| {
            this.data.sequence_picker = None;
            cx.notify();
        },
    ));
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
        .and_then(|lane_id| data.lanes.iter().find(|lane| lane.id == lane_id))
        .map(|lane| lane.analysis_source)
        .unwrap_or(AudioAnalysisSource::Automatic);
    let label = move |source, title: String| {
        if current == source {
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
    panel = panel.child(menu_row(
        cx,
        theme,
        "stream-auto",
        label(AudioAnalysisSource::Automatic, "Automatic".to_string()),
        false,
        |this, _, _, cx| this.set_stream_source(AudioAnalysisSource::Automatic, cx),
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
            |this, _, _, cx| this.set_stream_source(AudioAnalysisSource::AllMixed, cx),
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
                    this.set_stream_source(AudioAnalysisSource::MixedStream(index), cx)
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
                        this.set_stream_source(AudioAnalysisSource::Channel(ch), cx)
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
                        AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        },
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
                    this.set_stream_source(AudioAnalysisSource::MixedStream(index), cx)
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
                        AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        },
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
                            AudioAnalysisSource::Stream {
                                index,
                                channel: Some(ch),
                            },
                            cx,
                        )
                    },
                ));
            }
        }
    }
    panel = track_content_section(cx, theme, data, menu, panel);
    if let Some(lane_id) = &menu.lane_id {
        let current = data.lane_search_accuracy(lane_id);
        panel = panel.child(menu_header(theme, "Search accuracy".to_string()));
        for (index, accuracy) in std::iter::once(None)
            .chain(align_core::SearchAccuracy::ALL.into_iter().map(Some))
            .enumerate()
        {
            let title = accuracy
                .map(|value| value.label().to_string())
                .unwrap_or_else(|| format!("Inherit ({})", data.search_accuracy.label()));
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
        .unwrap_or_default();
    panel = panel.child(menu_header(theme, "Track content".to_string()));
    for (mode, title, row_id) in [
        (TrackContent::Auto, "Automatic", "content-auto"),
        (TrackContent::Linear, "Linear", "content-linear"),
        (TrackContent::Takes, "Takes", "content-takes"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title.to_string()
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
        .unwrap_or_default();
    panel = panel.child(menu_header(theme, "Clip order".to_string()));
    for (mode, title, row_id) in [
        (ClipOrder::Auto, "Auto", "order-auto"),
        (
            ClipOrder::AlternateAuto,
            "Alternate Auto",
            "order-alternate-auto",
        ),
        (ClipOrder::AsImported, "As imported", "order-imported"),
        (ClipOrder::ByDateTime, "Date & time", "order-date"),
        (ClipOrder::ByFileName, "File name", "order-name"),
        (ClipOrder::Ignore, "Ignore", "order-ignore"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title.to_string()
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
    let (current, available) = menu
        .lane_id
        .as_deref()
        .map(|id| data.temporal_availability(id))
        .unwrap_or((M::Auto, true));
    let header = if available {
        "Time source".to_string()
    } else {
        let missing = match current {
            M::Timecode => "timecode",
            M::RecStart | M::RecStop => "timestamps",
            M::Auto => "evidence",
        };
        format!("Time source — no {missing}, stable order")
    };
    panel = panel.child(menu_header(theme, header));
    for mode in [M::Auto, M::RecStart, M::RecStop, M::Timecode] {
        let label = if mode == current {
            format!("✓ {}", mode.title())
        } else {
            mode.title().to_string()
        };
        let row_id = match mode {
            M::Auto => "time-auto",
            M::RecStart => "time-rec-start",
            M::RecStop => "time-rec-stop",
            M::Timecode => "time-timecode",
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
        .unwrap_or_default();
    panel = panel.child(menu_header(theme, "Match threshold".to_string()));
    for (threshold, title, row_id) in [
        (MatchThreshold::Permissive, "More matches", "match-more"),
        (MatchThreshold::Balanced, "Balanced", "match-balanced"),
        (
            MatchThreshold::Conservative,
            "Fewer false matches",
            "match-fewer",
        ),
    ] {
        let label = if threshold == current {
            format!("✓ {title}")
        } else {
            title.to_string()
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
        .child(
            div()
                .text_lg()
                .font_weight(gpui::FontWeight(600.0))
                .child("Path Fixer"),
        );

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
            .child(button(
                cx,
                theme,
                "path-cancel",
                "Cancel",
                true,
                |this, _, _, cx| {
                    this.data.discard_path_redirection_edits();
                    this.data.show_path_fixer = false;
                    cx.notify();
                },
            ))
            .child(div().flex_1())
            .child(prominent_button(
                cx,
                theme,
                "path-apply",
                "Apply",
                true,
                |this, _, _, cx| this.finish_path_fixer(cx),
            )),
    )
}

fn export_sheet(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &super::state::AppData,
    inputs: &ExportInputs,
    max_height: f32,
) -> impl IntoElement {
    use super::state::{ExportTarget, Operation};
    let busy = matches!(data.operation, Operation::Exporting);
    let done = matches!(data.operation, Operation::Exported) && data.export_started;
    let mut sheet = div()
        .id("export-sheet")
        .flex()
        .flex_col()
        .gap_4()
        .p_5()
        .m_4()
        .w(px(740.))
        .max_h(px(max_height))
        .flex_shrink_0()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.panel))
        .shadow_md();
    sheet = sheet.child(
        div()
            .text_size(px(18.))
            .font_weight(gpui::FontWeight(600.0))
            .child("Export"),
    );
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
        .flex()
        .gap_5()
        .h(px((max_height - 180.).max(100.)))
        .min_h(px(0.));
    let mut settings = div().flex().flex_col().gap_3();
    // Formats.
    {
        let mut group = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(270.))
            .flex_shrink_0()
            .pr_4()
            .border_r_1()
            .border_color(rgb(theme.separator));
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
                "exp-storylines",
                data.export_storylines,
                "Group FCPXML tracks as storylines".to_string(),
                !busy,
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
                if data.export_selected.contains(&ExportTarget::Premiere) {
                    group = group.child(export_text_field(
                        theme,
                        "Premiere color",
                        inputs.unmatched_color.clone(),
                    ));
                }
                if data.export_selected.contains(&ExportTarget::FinalCutPro) {
                    group = group.child(export_text_field(
                        theme,
                        "Final Cut audio role",
                        inputs.unmatched_role.clone(),
                    ));
                }
            }
        }
        settings = settings.child(group);
    }
    columns = columns.child(
        div()
            .id("export-settings")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(settings),
    );
    sheet = sheet.child(columns);
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
    // Buttons.
    {
        let mut row = div()
            .flex()
            .flex_row()
            .gap_2()
            .flex_shrink_0()
            .pt_3()
            .border_t_1()
            .border_color(rgb(theme.separator));
        if busy {
            row = row.child(button(
                cx,
                theme,
                "exp-cancel",
                "Cancel",
                true,
                |this, _, _, cx| this.cancel_current(cx),
            ));
        } else if !done {
            row = row.child(button(
                cx,
                theme,
                "exp-dismiss",
                "Cancel",
                true,
                |this, _, _, cx| {
                    this.data.show_export = false;
                    cx.notify();
                },
            ));
        }
        row = row.child(div().flex_1());
        if done {
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
                    this.data.show_export = false;
                    cx.notify();
                },
            ));
        } else {
            let can_go = data.can_begin_export();
            row = row.child(prominent_button(
                cx,
                theme,
                "exp-go",
                "Export",
                can_go,
                |this, _, _, cx| this.begin_export(cx),
            ));
        }
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
        .child(div().text_lg().child("Align"))
        .child(
            div()
                .text_color(rgb(theme.dim))
                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
        )
        .child(div().child("Cross-platform media synchronizer"))
        .child(button(
            cx,
            theme,
            "btn-about-ok",
            "OK",
            true,
            |this, _, _, cx| {
                this.data.show_about = false;
                cx.notify();
            },
        ))
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
        .child(
            div()
                .text_size(px(18.))
                .font_weight(gpui::FontWeight(600.0))
                .child("Align could not finish"),
        )
        .child(
            data.error
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        );
    let panel = if data.can_locate_timeline_media() {
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
    };
    panel.child(button(
        cx,
        theme,
        "btn-alert-ok",
        "OK",
        true,
        |this, _, _, cx| {
            this.data.dismiss_error();
            cx.notify();
        },
    ))
}
