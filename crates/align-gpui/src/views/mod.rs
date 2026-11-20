//! Desktop views, controls, and background-job integration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use align_core::{
    AudioAnalysisSource, Clip, ClipId, ClipOrder, MatchPreview, MatchThreshold, MediaKind,
    SyncResult, TemporalMode, TrackContent, model::file_name,
};
use align_decode::export::ExportArtifact;
use align_decode::pipeline::{Phase, Pipeline};
use futures::StreamExt;
use gpui::{
    AnchoredPositionMode, Animation, AnimationExt, AnyView, App, BoxShadow, ClickEvent,
    ClipboardItem, Context, Corner, Div, DragMoveEvent, Entity, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, OwnedMenu,
    OwnedMenuItem, ParentElement, PathPromptOptions, Pixels, Point, Render, ScrollHandle,
    SharedString, Stateful, Styled, Window, anchored, deferred, div, img, point, prelude::*, px,
    rgb, rgba,
};

use crate::icons::{kind_badge, svg_icon};
use crate::lane::CorrectionOption;
use crate::state::{
    AppData, ClipState, ExportTarget, MenuTarget, Operation, SequenceDefaults, SequencePicker,
    SettingsScope, SyncDefaults,
};
use crate::text_input::TextInput;
use crate::theme::{Theme, ThemeMode};
use crate::updater::{self, UpdateState};
use crate::{MinimizeWindow, ToggleFullscreen, ZoomWindow};

mod animation;
mod controls;
mod export;
mod menus;
mod operation;
mod panels;
mod root;
mod settings;
mod sources;
mod timeline;
mod toolbar;

use animation::*;
use controls::*;
use export::*;
use menus::*;
use operation::*;
use panels::*;
use settings::*;
use sources::*;
use timeline::*;
use toolbar::*;

// ------------------------------------------------------------ messages

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
    waveform: Option<(ClipId, Vec<f32>)>,
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

#[derive(Clone, Copy)]
struct ExportReveals {
    final_cut: bool,
    storylines: bool,
    premiere: bool,
    aaf: bool,
    unmatched_kept: bool,
    unmatched_label_control: bool,
    synced_labels: bool,
    synced_xml: bool,
    synced_final_cut: bool,
    unmatched_labels: bool,
    unmatched_xml: bool,
    unmatched_final_cut: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportSelect {
    AafFrameRate,
    UnmatchedPlacement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSelect {
    Search,
    Audio,
    Temporal,
    Threshold,
    Order,
    Content,
}

impl SettingsSelect {
    fn id(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Audio => "audio",
            Self::Temporal => "temporal",
            Self::Threshold => "threshold",
            Self::Order => "order",
            Self::Content => "content",
        }
    }
}

const TIMELINE_MOVE_MS: u64 = 360;
const TIMELINE_ROW_H: f32 = 60.;
const PROGRESS_MOVE_MS: u64 = 320;

/// Progress bar fill that glides between reported values instead of jumping
/// between coarse phase updates. A retarget starts from the currently shown
/// width, so rapid updates stay continuous; a lower value (a new operation)
/// snaps back.
#[derive(Clone, Copy, Debug)]
struct ProgressMotion {
    from: f32,
    to: f32,
    started: Instant,
    generation: u64,
}

impl ProgressMotion {
    fn new() -> Self {
        Self {
            from: 0.,
            to: 0.,
            started: Instant::now(),
            generation: 0,
        }
    }

    fn presented(&self, now: Instant) -> f32 {
        let elapsed = now.duration_since(self.started).as_secs_f32();
        let progress =
            (elapsed / Duration::from_millis(PROGRESS_MOVE_MS).as_secs_f32()).clamp(0., 1.);
        self.from + (self.to - self.from) * motion_ease(progress)
    }

    fn track(&mut self, target: f32, now: Instant) {
        let target = target.clamp(0., 1.);
        if (target - self.to).abs() < 0.0005 {
            return;
        }
        self.from = if target < self.to {
            target
        } else {
            self.presented(now)
        };
        self.to = target;
        self.started = now;
        self.generation += 1;
    }
}

#[derive(Clone, Copy, Debug)]
struct TimelinePose {
    x: f32,
    width: f32,
    lane: f32,
}

#[derive(Clone, Copy, Debug)]
struct TimelineTransition {
    from: TimelinePose,
    to: TimelinePose,
    generation: u64,
    started: Instant,
}

impl TimelineTransition {
    fn presented(self, now: Instant) -> TimelinePose {
        let progress = (now.duration_since(self.started).as_secs_f32()
            / Duration::from_millis(TIMELINE_MOVE_MS).as_secs_f32())
        .clamp(0., 1.);
        let progress = motion_ease(progress);
        TimelinePose {
            x: self.from.x + (self.to.x - self.from.x) * progress,
            width: self.from.width + (self.to.width - self.from.width) * progress,
            lane: self.from.lane + (self.to.lane - self.from.lane) * progress,
        }
    }
}

fn timeline_duration(data: &AppData) -> f64 {
    data.lanes
        .iter()
        .flat_map(|lane| lane.clips.iter())
        .map(|clip| clip.start + clip.duration)
        .fold(1.0f64, f64::max)
        .max(1.0)
}

fn timeline_poses(data: &AppData) -> HashMap<ClipId, TimelinePose> {
    let duration = timeline_duration(data);
    let mut poses = HashMap::new();
    for (lane_index, lane) in data.lanes.iter().enumerate() {
        for (clip_index, clip) in lane.clips.iter().enumerate() {
            let visible_duration = lane
                .clips
                .get(clip_index + 1)
                .map_or(clip.duration, |next| {
                    clip.duration.min((next.start - clip.start).max(0.))
                });
            poses.insert(
                clip.clip_id.clone(),
                TimelinePose {
                    x: (clip.start / duration) as f32,
                    width: (visible_duration / duration) as f32,
                    lane: lane_index as f32,
                },
            );
        }
    }
    poses
}

fn poses_differ(left: TimelinePose, right: TimelinePose) -> bool {
    (left.x - right.x).abs() > 0.0001
        || (left.width - right.width).abs() > 0.0001
        || (left.lane - right.lane).abs() > 0.0001
}

fn middle_ellipsis(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars || max_chars < 3 {
        return value.to_string();
    }
    let visible = max_chars - 1;
    let head = visible / 2;
    let tail = visible - head;
    chars[..head]
        .iter()
        .chain(std::iter::once(&'…'))
        .chain(chars[chars.len() - tail..].iter())
        .collect()
}

impl ExportReveals {
    fn new(data: &AppData) -> Self {
        let final_cut = data.export_selected.contains(&ExportTarget::FinalCutPro);
        let xml = data.export_selected.contains(&ExportTarget::Premiere)
            || data.export_selected.contains(&ExportTarget::ResolveXml);
        let unmatched_kept = data.unmatched_count() > 0
            && data.export_unmatched != align_core::export_model::UnmatchedPlacement::Remove;
        Self {
            final_cut,
            storylines: final_cut && data.export_fcpxml_timeline,
            premiere: data.export_selected.contains(&ExportTarget::Premiere),
            aaf: data.export_selected.contains(&ExportTarget::Aaf),
            unmatched_kept,
            unmatched_label_control: unmatched_kept,
            synced_labels: data.export_label_synced,
            synced_xml: data.export_label_synced && xml,
            synced_final_cut: data.export_label_synced && final_cut,
            unmatched_labels: unmatched_kept && data.export_label_unmatched,
            unmatched_xml: unmatched_kept && data.export_label_unmatched && xml,
            unmatched_final_cut: unmatched_kept && data.export_label_unmatched && final_cut,
        }
    }

    fn entries(self) -> [(&'static str, bool); 12] {
        [
            ("export-final-cut-options", self.final_cut),
            ("export-storylines", self.storylines),
            ("export-premiere-options", self.premiere),
            ("export-aaf-options", self.aaf),
            ("export-unmatched-kept", self.unmatched_kept),
            (
                "export-unmatched-label-control",
                self.unmatched_label_control,
            ),
            ("export-synced-labels", self.synced_labels),
            ("export-synced-xml", self.synced_xml),
            ("export-synced-final-cut", self.synced_final_cut),
            ("export-unmatched-labels", self.unmatched_labels),
            ("export-unmatched-xml", self.unmatched_xml),
            ("export-unmatched-final-cut", self.unmatched_final_cut),
        ]
    }
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

    fn labels(&self, cx: &App) -> crate::state::ExportLabels {
        let text = |input: &Entity<TextInput>| input.read(cx).text();
        crate::state::ExportLabels {
            synced_symbol: text(&self.synced_symbol),
            synced_color: text(&self.synced_color),
            synced_role: text(&self.synced_role),
            unmatched_symbol: text(&self.unmatched_symbol),
            unmatched_color: text(&self.unmatched_color),
            unmatched_role: text(&self.unmatched_role),
        }
    }

    fn set_labels(&self, labels: &crate::state::ExportLabels, cx: &mut App) {
        for (input, text) in [
            (&self.synced_symbol, &labels.synced_symbol),
            (&self.synced_color, &labels.synced_color),
            (&self.synced_role, &labels.synced_role),
            (&self.unmatched_symbol, &labels.unmatched_symbol),
            (&self.unmatched_color, &labels.unmatched_color),
            (&self.unmatched_role, &labels.unmatched_role),
        ] {
            input.update(cx, |input, cx| input.set_text(text.clone(), cx));
        }
    }
}

/// Exit and motion lengths, shared by an element's animation and the timer
/// that removes it once the animation has played.
const SIDEBAR_CLOSE_MS: u64 = 180;
const DROPDOWN_MS: u64 = 200;
const SETTINGS_MOTION_MS: u64 = 180;
const SELECTION_CONTROL_CLOSE_MS: u64 = 130;

/// Transitions of the settings header buttons: the close button switching
/// between its close and apply forms, and the reset button leaving. One
/// timer settles both.
#[derive(Default)]
struct SettingsButtons {
    close_changing: bool,
    reset_leaving: bool,
    generation: u64,
}

impl SettingsButtons {
    fn is_animating(&self) -> bool {
        self.close_changing || self.reset_leaving
    }

    /// Supersedes pending timers; the new timer must present the token.
    fn restart(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Ends both transitions when `token` is the latest.
    fn settle(&mut self, token: u64) {
        if self.generation == token {
            self.stop();
        }
    }

    fn stop(&mut self) {
        self.close_changing = false;
        self.reset_leaving = false;
    }
}

/// The exit phase of a transient element, which stays rendered while it
/// animates out. Restarting or cancelling the exit invalidates earlier timers.
#[derive(Default)]
struct Exit {
    active: bool,
    generation: u64,
}

impl Exit {
    fn is_active(&self) -> bool {
        self.active
    }

    /// Starts the exit; its timer must present the returned token.
    fn start(&mut self) -> u64 {
        self.generation += 1;
        self.active = true;
        self.generation
    }

    fn cancel(&mut self) {
        self.generation += 1;
        self.active = false;
    }

    /// Ends the exit when `token` is the latest; true when it ended.
    fn finish(&mut self, token: u64) -> bool {
        let current = self.active && self.generation == token;
        if current {
            self.active = false;
        }
        current
    }
}

pub struct AlignApp {
    pub(crate) data: AppData,
    pub(crate) focus_handle: FocusHandle,
    export_inputs: ExportInputs,
    path_fixer_inputs: PathFixerInputs,
    pan_origin: Option<Point<Pixels>>,
    app_menu_position: Option<Point<Pixels>>,
    export_scroll: ScrollHandle,
    export_scroll_drag: Option<(f32, f32)>,
    export_sidebar_exit: Exit,
    search_quality_exit: Exit,
    export_select: Option<ExportSelect>,
    export_select_exit: Exit,
    settings_select: Option<SettingsSelect>,
    settings_select_exit: Exit,
    settings_buttons: SettingsButtons,
    settings_initial_common: SyncDefaults,
    settings_initial_sequence: SequenceDefaults,
    selection_controls_exit: Exit,
    export_reveals_closing: HashMap<&'static str, u64>,
    export_reveal_generation: u64,
    timeline_transitions: HashMap<ClipId, TimelineTransition>,
    timeline_motion_generation: u64,
    progress_motion: ProgressMotion,
    update_state: UpdateState,
    show_problem_report: bool,
    feedback_notice_pending: bool,
    /// Error shown when the report opened, offered for copying (SharedString:
    /// the panel clones it every frame).
    feedback_error: Option<SharedString>,
    /// The update card was closed; the About panel still offers the update.
    update_notice_dismissed: bool,
}

impl Focusable for AlignApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AlignApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let app = Self {
            data: AppData::new(),
            focus_handle: cx.focus_handle(),
            export_inputs: ExportInputs::new(cx),
            path_fixer_inputs: PathFixerInputs::new(cx),
            pan_origin: None,
            app_menu_position: None,
            export_scroll: ScrollHandle::new(),
            export_scroll_drag: None,
            export_sidebar_exit: Exit::default(),
            search_quality_exit: Exit::default(),
            export_select: None,
            export_select_exit: Exit::default(),
            settings_select: None,
            settings_select_exit: Exit::default(),
            settings_buttons: SettingsButtons::default(),
            settings_initial_common: SyncDefaults::default(),
            settings_initial_sequence: SequenceDefaults::default(),
            selection_controls_exit: Exit::default(),
            export_reveals_closing: HashMap::new(),
            export_reveal_generation: 0,
            timeline_transitions: HashMap::new(),
            timeline_motion_generation: 0,
            progress_motion: ProgressMotion::new(),
            update_state: UpdateState::default(),
            show_problem_report: false,
            feedback_notice_pending: crate::feedback::notice_pending(),
            feedback_error: None,
            update_notice_dismissed: false,
        };
        app.export_inputs
            .set_labels(&app.data.export_baseline().labels, cx);
        app
    }

    // ---------------- actions

    /// Clear the session, or remove the selected sources. A cleared session
    /// starts the next job from the export defaults.
    pub(crate) fn delete_or_clear(&mut self, cx: &mut Context<Self>) {
        let clearing = self.data.selection.is_empty();
        self.data.delete_or_clear();
        if clearing {
            self.export_inputs
                .set_labels(&self.data.export_baseline().labels, cx);
        }
        cx.notify();
    }

    /// The export options in the sheet, text fields included.
    fn export_settings(&self, cx: &App) -> crate::state::ExportSettings {
        self.data.export_settings(self.export_inputs.labels(cx))
    }

    fn load_export_settings(
        &mut self,
        settings: crate::state::ExportSettings,
        cx: &mut Context<Self>,
    ) {
        self.update_export_options(cx, |data| data.apply_export_settings(&settings));
        self.export_inputs.set_labels(&settings.labels, cx);
        cx.notify();
    }

    fn save_export_defaults(&mut self, cx: &mut Context<Self>) {
        let settings = self.export_settings(cx);
        match settings.save() {
            Ok(()) => self.data.saved_export_settings = Some(settings),
            Err(_) => {
                self.data.error = Some(
                    "Could not save export defaults. Your current export options are unchanged."
                        .to_string(),
                );
            }
        }
        cx.notify();
    }

    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let paths = self.data.begin_import(paths);
        if paths.is_empty() {
            return;
        }
        let token = self.data.import_cancel.clone();
        let cancel = token.clone();
        let task = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .take_while(|_| !cancel.load(std::sync::atomic::Ordering::Relaxed))
                .map(|path| crate::state::InputPath::inspect(path, &cancel))
                .collect()
        });
        cx.spawn(async move |view, cx| {
            let inputs = task.await;
            let _ = view.update(cx, |this, cx| {
                this.data.finish_import(&token, inputs);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

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
                this.add_paths(paths, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the report over whatever modal is showing; that modal is hidden
    /// meanwhile and returns when the report closes.
    pub(crate) fn show_problem_report(&mut self, cx: &mut Context<Self>) {
        self.feedback_error = self.data.error.clone().map(SharedString::from);
        self.show_problem_report = true;
        self.dismiss_feedback_notice();
        cx.notify();
    }

    fn close_problem_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_problem_report = false;
        self.feedback_error = None;
        self.focus_handle.focus(window);
        cx.notify();
    }

    fn dismiss_feedback_notice(&mut self) {
        if std::mem::take(&mut self.feedback_notice_pending) {
            crate::feedback::dismiss_notice();
        }
    }

    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.run_update_check(false, cx);
    }

    /// Quiet check at launch: a new release shows the update card; being
    /// offline or up to date shows nothing. Development builds skip it.
    pub(crate) fn check_for_updates_on_launch(&mut self, cx: &mut Context<Self>) {
        if cfg!(debug_assertions) || std::env::var_os("ALIGN_NO_UPDATE_CHECK").is_some() {
            return;
        }
        self.run_update_check(true, cx);
    }

    fn run_update_check(&mut self, quiet: bool, cx: &mut Context<Self>) {
        if self.update_state.is_busy() {
            return;
        }
        self.update_state = UpdateState::Checking;
        cx.notify();

        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(updater::check());
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let state = rx.await.unwrap_or(UpdateState::Failed);
            let _ = view.update(cx, |this, cx| {
                this.update_state = match state {
                    UpdateState::Failed | UpdateState::Current if quiet => UpdateState::Idle,
                    state => state,
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn download_update(&mut self, cx: &mut Context<Self>) {
        let state = std::mem::take(&mut self.update_state);
        let UpdateState::Available(update) = state else {
            self.update_state = state;
            return;
        };
        if let updater::Update::Manual { page, .. } = &update {
            cx.open_url(page);
            self.update_state = UpdateState::Available(update);
            return;
        }
        let version = update.version().to_owned();
        self.update_state = UpdateState::Downloading { version };
        cx.notify();

        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(updater::download(update));
        });
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let state = rx.await.unwrap_or(UpdateState::Failed);
            let _ = view.update(cx, |this, cx| {
                this.update_state = state;
                cx.notify();
            });
        })
        .detach();
    }

    fn restart_and_update(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.data.operation,
            Operation::Synchronizing | Operation::Exporting | Operation::Repairing
        ) {
            return;
        }
        let state = std::mem::take(&mut self.update_state);
        let UpdateState::Ready(prepared) = state else {
            self.update_state = state;
            return;
        };
        match prepared {
            #[cfg(windows)]
            updater::Prepared::Velopack { manager, asset } => {
                align_decode::media_assets::cleanup();
                if let Err(error) = manager.apply_updates_and_restart(&asset) {
                    eprintln!("Could not install the Align update: {error}");
                    self.update_state = UpdateState::Failed;
                }
            }
            updater::Prepared::Package {
                install,
                version,
                file,
            } => {
                // The package manager may wait on an authorization prompt,
                // so it runs off the UI thread; Align restarts once it is done.
                self.update_state = UpdateState::Installing { version };
                cx.notify();
                let (tx, rx) = futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let result = updater::install(&install, &file)
                        .and_then(|()| updater::relaunch(&install));
                    let _ = tx.send(result);
                });
                let view = cx.entity();
                cx.spawn(async move |_, cx| {
                    let result = rx.await.unwrap_or_else(|_| Err("cancelled".into()));
                    let _ = view.update(cx, |this, cx| match result {
                        Ok(()) => {
                            align_decode::media_assets::cleanup();
                            cx.quit();
                        }
                        Err(error) => {
                            eprintln!("Could not install the Align update: {error}");
                            this.update_state = UpdateState::Failed;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }

    pub(crate) fn start_sync(&mut self, cx: &mut Context<Self>) {
        self.search_quality_exit.cancel();
        self.clear_export_select();
        self.clear_settings_select();
        self.selection_controls_exit.cancel();
        self.export_sidebar_exit.cancel();
        if !self.data.begin_sync_run() {
            return;
        }
        if self.data.lanes.is_empty() {
            self.timeline_transitions.clear();
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
                        waveform: p.waveform,
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
            while let Some(first) = rx.next().await {
                // What arrived meanwhile applies with it, as one update:
                // refinement reports hundreds of pairs.
                let mut batch = vec![first];
                while let Ok(msg) = rx.try_recv() {
                    batch.push(msg);
                }
                let finished = batch.iter().any(|msg| matches!(msg, SyncMsg::Done(_)));
                let _ = view.update(cx, |this, cx| {
                    if this.data.generation == generation {
                        this.apply_sync_msgs(batch);
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

    fn record_timeline_layout_change(&mut self, before: HashMap<ClipId, TimelinePose>) {
        let after = timeline_poses(&self.data);
        self.timeline_transitions
            .retain(|clip_id, _| after.contains_key(clip_id));
        let changes: Vec<_> = after
            .iter()
            .filter_map(|(clip_id, &to)| {
                let from = before.get(clip_id).copied()?;
                poses_differ(from, to).then(|| (clip_id.clone(), from, to))
            })
            .collect();
        if changes.is_empty() || crate::motion::reduced_motion() {
            return;
        }
        self.timeline_motion_generation += 1;
        let generation = self.timeline_motion_generation;
        let now = Instant::now();
        for (clip_id, previous, to) in changes {
            let from = self
                .timeline_transitions
                .get(&clip_id)
                .copied()
                .map_or(previous, |transition| transition.presented(now));
            self.timeline_transitions.insert(
                clip_id,
                TimelineTransition {
                    from,
                    to,
                    generation,
                    started: now,
                },
            );
        }
    }

    fn update_timeline_layout<T>(&mut self, update: impl FnOnce(&mut AppData) -> T) -> T {
        let before = timeline_poses(&self.data);
        let result = update(&mut self.data);
        self.record_timeline_layout_change(before);
        result
    }

    /// Apply messages in order. A sequence's start clears the timeline, so
    /// it applies on its own; the messages between apply as one change, and
    /// the timeline lays out, and animates, once for them.
    fn apply_sync_msgs(&mut self, msgs: Vec<SyncMsg>) {
        let mut group = Vec::new();
        for msg in msgs {
            if matches!(msg, SyncMsg::SequenceStart { .. }) {
                self.apply_sync_group(std::mem::take(&mut group));
                self.apply_sync_group(vec![msg]);
            } else {
                group.push(msg);
            }
        }
        self.apply_sync_group(group);
    }

    fn apply_sync_group(&mut self, msgs: Vec<SyncMsg>) {
        if msgs.is_empty() {
            return;
        }
        let changes_layout = msgs.iter().any(|msg| match msg {
            SyncMsg::Progress(event) => event.discovered.is_some() || event.preview.is_some(),
            _ => true,
        });
        let before = changes_layout.then(|| timeline_poses(&self.data));
        self.data.defer_layout(true);
        for msg in msgs {
            if let SyncMsg::Done(result) = &msg {
                // A result, or a cancellation, replaces the provisional
                // layout; after a failure it stays on screen.
                match result.as_ref() {
                    Err(error) if error != "Cancelled." => self.data.defer_layout(false),
                    _ => self.data.discard_deferred_layout(),
                }
            }
            self.apply_sync_msg(msg);
        }
        self.data.defer_layout(false);
        if let Some(before) = before {
            self.record_timeline_layout_change(before);
        }
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
            SyncMsg::Progress(mut event) => {
                if let Some((clip_id, waveform)) = event.waveform.take() {
                    self.data.waveform_previews.insert(clip_id, waveform);
                }
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
        self.export_sidebar_exit.cancel();
        self.export_reveals_closing.clear();
        self.clear_export_select();
        self.data.show_search_quality = false;
        self.search_quality_exit.cancel();
        self.data.export_started = false;
        self.data.error = None;
        self.export_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn close_search_quality(&mut self, cx: &mut Context<Self>) {
        if !self.data.show_search_quality || self.search_quality_exit.is_active() {
            return;
        }
        let exit = self.search_quality_exit.start();
        cx.notify();
        self.after(DROPDOWN_MS, cx, move |this| {
            if this.search_quality_exit.finish(exit) {
                this.data.show_search_quality = false;
            }
        });
    }

    fn toggle_export_select(&mut self, select: ExportSelect, cx: &mut Context<Self>) {
        if self.export_select == Some(select) && !self.export_select_exit.is_active() {
            self.close_export_select(cx);
            return;
        }
        self.export_select_exit.cancel();
        self.export_select = Some(select);
        cx.notify();
    }

    fn close_export_select(&mut self, cx: &mut Context<Self>) {
        if self.export_select.is_none() || self.export_select_exit.is_active() {
            return;
        }
        let exit = self.export_select_exit.start();
        cx.notify();
        self.after(DROPDOWN_MS, cx, move |this| {
            if this.export_select_exit.finish(exit) {
                this.export_select = None;
            }
        });
    }

    fn clear_export_select(&mut self) {
        self.export_select_exit.cancel();
        self.export_select = None;
    }

    fn toggle_settings_select(&mut self, select: SettingsSelect, cx: &mut Context<Self>) {
        if self.settings_select == Some(select) && !self.settings_select_exit.is_active() {
            self.close_settings_select(cx);
            return;
        }
        self.settings_select_exit.cancel();
        self.settings_select = Some(select);
        cx.notify();
    }

    fn close_settings_select(&mut self, cx: &mut Context<Self>) {
        if self.settings_select.is_none() || self.settings_select_exit.is_active() {
            return;
        }
        let exit = self.settings_select_exit.start();
        cx.notify();
        self.after(SETTINGS_MOTION_MS, cx, move |this| {
            if this.settings_select_exit.finish(exit) {
                this.settings_select = None;
            }
        });
    }

    fn clear_settings_select(&mut self) {
        self.settings_select_exit.cancel();
        self.settings_select = None;
    }

    fn animate_settings_change(
        &mut self,
        was_default: bool,
        changed: bool,
        cx: &mut Context<Self>,
    ) {
        if !changed {
            return;
        }
        let was_dirty = self.data.settings_changed;
        let is_dirty = self.data.common_settings != self.settings_initial_common
            || self.data.current_sequence_settings() != self.settings_initial_sequence;
        self.data.settings_changed = is_dirty;
        let is_default = self.data.scoped_settings_are_default();
        let buttons = &mut self.settings_buttons;
        let token = buttons.restart();
        if was_dirty != is_dirty {
            buttons.close_changing = true;
        }
        if was_default != is_default {
            buttons.reset_leaving = is_default;
        }
        cx.notify();
        if buttons.is_animating() {
            self.after(SETTINGS_MOTION_MS, cx, move |this| {
                this.settings_buttons.settle(token);
            });
        }
    }

    fn set_settings_scope(&mut self, scope: SettingsScope, cx: &mut Context<Self>) {
        let was_default = self.data.scoped_settings_are_default();
        self.data.settings_scope = scope;
        self.clear_settings_select();
        let is_default = self.data.scoped_settings_are_default();
        if was_default != is_default {
            let buttons = &mut self.settings_buttons;
            let token = buttons.restart();
            buttons.reset_leaving = is_default;
            // Settling also ends a close-button change whose timer the
            // restart superseded.
            if buttons.is_animating() {
                self.after(SETTINGS_MOTION_MS, cx, move |this| {
                    this.settings_buttons.settle(token);
                });
            }
        }
        cx.notify();
    }

    fn set_settings_option(
        &mut self,
        select: SettingsSelect,
        option: usize,
        cx: &mut Context<Self>,
    ) {
        let inherit = self.data.settings_scope == SettingsScope::CurrentSequence;
        let index = option.saturating_sub(usize::from(inherit));
        let was_default = self.data.scoped_settings_are_default();
        let changed = if inherit && option == 0 {
            match select {
                SettingsSelect::Search => self.data.set_scoped_search_accuracy(None),
                SettingsSelect::Audio => self.data.set_scoped_audio_source(None),
                SettingsSelect::Temporal => self.data.set_scoped_temporal_mode(None),
                SettingsSelect::Threshold => self.data.set_scoped_match_threshold(None),
                SettingsSelect::Order => self.data.set_scoped_clip_order(None),
                SettingsSelect::Content => self.data.set_scoped_track_content(None),
            }
        } else {
            match select {
                SettingsSelect::Search => align_core::SearchAccuracy::ALL
                    .get(index)
                    .copied()
                    .is_some_and(|value| self.data.set_scoped_search_accuracy(Some(value))),
                SettingsSelect::Audio => [
                    AudioAnalysisSource::Automatic,
                    AudioAnalysisSource::AllMixed,
                    AudioAnalysisSource::MixedStream(0),
                    AudioAnalysisSource::Channel(0),
                ]
                .get(index)
                .copied()
                .is_some_and(|value| self.data.set_scoped_audio_source(Some(value))),
                SettingsSelect::Temporal => [
                    TemporalMode::Auto,
                    TemporalMode::RecStart,
                    TemporalMode::RecStop,
                    TemporalMode::Timecode,
                ]
                .get(index)
                .copied()
                .is_some_and(|value| self.data.set_scoped_temporal_mode(Some(value))),
                SettingsSelect::Threshold => [
                    MatchThreshold::Permissive,
                    MatchThreshold::Balanced,
                    MatchThreshold::Conservative,
                ]
                .get(index)
                .copied()
                .is_some_and(|value| self.data.set_scoped_match_threshold(Some(value))),
                SettingsSelect::Order => [
                    ClipOrder::Auto,
                    ClipOrder::AlternateAuto,
                    ClipOrder::AsImported,
                    ClipOrder::ByDateTime,
                    ClipOrder::ByFileName,
                    ClipOrder::Ignore,
                ]
                .get(index)
                .copied()
                .is_some_and(|value| self.data.set_scoped_clip_order(Some(value))),
                SettingsSelect::Content => [
                    TrackContent::Auto,
                    TrackContent::Linear,
                    TrackContent::Takes,
                ]
                .get(index)
                .copied()
                .is_some_and(|value| self.data.set_scoped_track_content(Some(value))),
            }
        };
        self.clear_settings_select();
        self.animate_settings_change(was_default, changed, cx);
    }

    fn reset_settings(&mut self, cx: &mut Context<Self>) {
        let was_default = self.data.scoped_settings_are_default();
        let changed = self.data.reset_scoped_settings();
        self.clear_settings_select();
        self.animate_settings_change(was_default, changed, cx);
    }

    fn update_export_options(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut AppData)) {
        let before = ExportReveals::new(&self.data).entries();
        update(&mut self.data);
        let select_is_available = match self.export_select {
            Some(ExportSelect::AafFrameRate) => {
                self.data.export_selected.contains(&ExportTarget::Aaf)
            }
            Some(ExportSelect::UnmatchedPlacement) => self.data.unmatched_count() > 0,
            None => true,
        };
        if !select_is_available {
            self.clear_export_select();
        }
        let after = ExportReveals::new(&self.data).entries();
        let mut closing = Vec::new();
        for ((id, was_visible), (_, is_visible)) in before.into_iter().zip(after) {
            if was_visible && !is_visible {
                self.export_reveal_generation += 1;
                let generation = self.export_reveal_generation;
                self.export_reveals_closing.insert(id, generation);
                closing.push((id, generation));
            } else if !was_visible && is_visible {
                self.export_reveals_closing.remove(id);
            }
        }
        cx.notify();
        if closing.is_empty() {
            return;
        }
        self.after(SETTINGS_MOTION_MS, cx, move |this| {
            for (id, generation) in closing {
                if this.export_reveals_closing.get(id) == Some(&generation) {
                    this.export_reveals_closing.remove(id);
                }
            }
        });
    }

    fn close_export_sidebar(&mut self, cx: &mut Context<Self>) {
        if !self.data.show_export || self.export_sidebar_exit.is_active() {
            return;
        }
        self.clear_export_select();
        let exit = self.export_sidebar_exit.start();
        cx.notify();
        self.after(SIDEBAR_CLOSE_MS, cx, move |this| {
            if this.export_sidebar_exit.finish(exit) {
                this.data.show_export = false;
                this.clear_export_select();
            }
        });
    }

    fn close_selection_controls(&mut self, cx: &mut Context<Self>) {
        let exit = self.selection_controls_exit.start();
        cx.notify();
        self.after(SELECTION_CONTROL_CLOSE_MS, cx, move |this| {
            this.selection_controls_exit.finish(exit);
        });
    }

    /// Runs `finish` and redraws once `duration_ms` has passed, e.g. to
    /// remove an element after its exit animation.
    fn after(
        &self,
        duration_ms: u64,
        cx: &mut Context<Self>,
        finish: impl FnOnce(&mut Self) + 'static,
    ) {
        let timer = cx
            .background_executor()
            .timer(Duration::from_millis(duration_ms));
        cx.spawn(async move |view, cx| {
            timer.await;
            let _ = view.update(cx, |this, cx| {
                finish(this);
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_search_settings(&mut self, cx: &mut Context<Self>) {
        self.clear_settings_select();
        self.settings_buttons.stop();
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
        self.export_sidebar_exit.cancel();
        self.clear_export_select();
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
            .any(|path| self.data.timeline_inputs.contains(path))
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
        let media_bins = self.data.export_media_bins;
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
            let options = align_decode::export::ExportOptions {
                formats,
                correct_drift: drift,
                include_replaced_sequence: replaced,
                media_bins,
                include_media_files: media,
                aaf_frame_duration,
                fcpxml: align_core::export::fcpxml::FcpxmlOptions {
                    timeline: fcpxml_timeline,
                    multicam: fcpxml_multicam,
                    group_storylines: storylines,
                    ..Default::default()
                },
            };
            let out = align_decode::export::export_prepared_many(
                align_decode::export::ExportBatchRequest {
                    backend: pipeline.backend(),
                    timelines: &timelines,
                    directory: &dir,
                    options: &options,
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
        if self.data.importing() {
            self.data.cancel_import();
            cx.notify();
            return;
        }
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
