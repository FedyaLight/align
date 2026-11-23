//! The window: layout of every panel, sheet, and overlay.

use super::animation::toolbar_entrance;
use super::controls::{overlay, sidebar_scrim};
use super::export::{ExportPanel, export_sidebar, export_sidebar_action, path_fixer_panel};
use super::menu_sidebar::app_menu_sidebar;
use super::menus::{context_menu, menu_dismiss_layer, sequence_picker_panel};
use super::operation::{
    operation_bar, sequence_results_panel, stage_settings_panel, warning_banner, warning_details,
};
use super::panels::{
    about_panel, agent_setup_panel, error_alert, feedback_notice, problem_report_panel,
    update_notice,
};
use super::settings::{
    export_select_dismiss_layer, search_quality_dismiss_layer, search_settings_panel,
};
use super::sources::main_content;
use super::toolbar::toolbar;
use super::{
    AlignApp, Context, Corner, Instant, InteractiveElement, IntoElement, KeyDownEvent,
    MinimizeWindow, Operation, ParentElement, Point, Render, Styled, Theme, ToggleFullscreen,
    Window, ZoomWindow, anchored, deferred, div, px, rgb,
};

impl Render for AlignApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance());
        // Fit width excludes the label column and content padding.
        let fit_width =
            (f32::from(window.viewport_size().width) - crate::lane::LABEL_WIDTH - 16.0).max(100.0);
        // While a modal or context menu is open the content below keeps
        // its layout but loses hover/cursor feedback (clicks are already
        // swallowed by the overlay layers).
        let content_active = !self.projects.busy
            && !self.show_problem_report
            && self.app_menu.is_none()
            && self.data.menu.is_none()
            && !self.data.show_path_fixer
            && self.data.error.is_none()
            && self.data.sequence_pickers.is_empty()
            && !self.data.show_about
            && !self.data.show_agent_setup
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
            .capture_key_down(cx.listener(|this, _, _, cx| {
                if this.projects.busy {
                    cx.stop_propagation();
                }
            }))
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
                if this.show_problem_report {
                    if key == "escape" {
                        this.close_problem_report(window, cx);
                    }
                    return;
                }
                if this.app_menu.is_some() {
                    if key == "escape" {
                        this.dismiss_app_menu(cx);
                    }
                    return;
                }
                if key == "escape" && this.data.menu.take().is_some() {
                    cx.notify();
                    return;
                }
                if key == "escape" && this.data.show_about {
                    this.data.show_about = false;
                    cx.notify();
                    return;
                }
                if this.data.show_about {
                    return;
                }
                if key == "escape" && this.export_select.is_some() {
                    this.close_export_select(cx);
                    return;
                }
                if key == "escape" && this.settings_select.is_some() {
                    this.close_settings_select(cx);
                    return;
                }
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
                if key == "escape" && this.data.show_agent_setup {
                    this.data.show_agent_setup = false;
                    cx.notify();
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
                if key == "space" {
                    if this.data.show_export
                        || this.data.show_path_fixer
                        || this.data.show_sequence_results
                        || this.data.show_search_quality
                        || this.data.show_search_settings
                        || this.data.show_stage_settings
                        || this.data.show_agent_setup
                    {
                        return;
                    }
                    this.toggle_timeline_playback(cx);
                    cx.stop_propagation();
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
            }));
        if content_active && !self.data.has_active_job() {
            root = root.on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.add_paths(paths.paths().to_vec(), cx);
            }));
        }
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
            content = content.child(toolbar_entrance(
                toolbar(
                    cx,
                    &theme,
                    &self.data,
                    content_active,
                    self.search_quality_exit.is_active(),
                ),
                "top-toolbar-entrance",
            ));
        }
        content = content.child(main_content(
            cx,
            &theme,
            &self.data,
            &self.timeline_transitions,
            fit_width,
            content_active,
            self.selection_controls_exit.is_active(),
        ));
        if !self.data.warnings.is_empty() || !self.data.restore_warnings.is_empty() {
            content = content.child(warning_banner(cx, &theme, &self.data));
            if self.data.show_warning_details {
                content = content.child(warning_details(&theme, &self.data));
            }
        }
        if let Some(notice) = &self.projects.notice {
            content = content.child(
                div()
                    .px_5()
                    .py_1()
                    .text_color(rgb(theme.dim))
                    .child(notice.clone()),
            );
        }
        // No bottom bar over the empty drop zone either.
        if !self.data.clips.is_empty() || self.data.importing() {
            self.progress_motion
                .track(self.data.progress, Instant::now());
            content = content.child(toolbar_entrance(
                operation_bar(cx, &theme, &self.data, self.progress_motion, content_active),
                "bottom-toolbar-entrance",
            ));
        }
        if self.data.show_export {
            content = content.child(sidebar_scrim(&theme, self.export_sidebar_exit.is_active()));
        }
        root = root.child(content);
        if self.projects.busy {
            return root.child(
                div()
                    .id("project-busy")
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .size_full()
                    .occlude()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation()),
            );
        }
        if self.data.show_export {
            root = root
                .child(export_sidebar(
                    cx,
                    &theme,
                    &self.data,
                    ExportPanel {
                        inputs: &self.export_inputs,
                        scroll: &self.export_scroll,
                        reveals_closing: &self.export_reveals_closing,
                        select: self.export_select,
                        select_closing: self.export_select_exit.is_active(),
                    },
                    self.export_sidebar_exit.is_active(),
                ))
                .child(export_sidebar_action(cx, &theme, &self.data));
        }
        if self.export_select.is_some() {
            root = root.child(export_select_dismiss_layer(cx));
        }
        if let Some(page) = &self.app_menu {
            root = root.child(app_menu_sidebar(cx, &theme, self, page));
        }
        // The problem report temporarily replaces the open modal, keeping a
        // single dim layer; that modal returns when the report closes.
        let modals_visible = !self.show_problem_report;
        if let (true, Some(picker)) = (modals_visible, self.data.sequence_pickers.front()) {
            root = root.child(overlay(
                &theme,
                "overlay-seq",
                sequence_picker_panel(cx, &theme, picker.clone()),
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
        if !self.update_notice_dismissed
            && !self.data.show_about
            && let Some(notice) = update_notice(cx, &theme, &self.update_state, self.data.operation)
        {
            root = root.child(notice);
        }
        // Same corner as the update card, so it waits until that is gone, and
        // it would cover the export sidebar's settings. Modals only dim it:
        // removing it would replay its entrance every time one closes.
        if self.feedback_notice_pending
            && !self.data.show_about
            && !self.data.show_export
            && (self.update_notice_dismissed || self.update_state.version().is_none())
        {
            root = root.child(feedback_notice(cx, &theme));
        }
        if modals_visible && self.data.show_stage_settings {
            root = root.child(overlay(
                &theme,
                "overlay-stages",
                stage_settings_panel(cx, &theme, &self.data),
            ));
        }
        if modals_visible && self.data.show_sequence_results {
            root = root.child(overlay(
                &theme,
                "overlay-sequence-results",
                sequence_results_panel(cx, &theme, &self.data),
            ));
        }
        if modals_visible && self.data.show_search_settings {
            root = root.child(overlay(
                &theme,
                "overlay-search",
                search_settings_panel(
                    cx,
                    &theme,
                    &self.data,
                    self.settings_select,
                    self.settings_select_exit.is_active(),
                    self.settings_buttons.close_changing,
                    self.settings_buttons.reset_leaving,
                ),
            ));
        }
        if modals_visible && self.data.show_path_fixer {
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
        if modals_visible && self.data.error.is_some() {
            root = root.child(overlay(
                &theme,
                "overlay-error",
                error_alert(cx, &theme, &self.data),
            ));
        }
        if modals_visible && self.data.show_about {
            root = root.child(overlay(
                &theme,
                "overlay-about",
                about_panel(cx, &theme, &self.update_state, self.data.operation),
            ));
        }
        if modals_visible && self.data.show_agent_setup {
            root = root.child(overlay(
                &theme,
                "overlay-agent-setup",
                agent_setup_panel(cx, &theme),
            ));
        }
        if self.show_problem_report {
            root = root.child(overlay(
                &theme,
                "overlay-problem-report",
                problem_report_panel(cx, &theme, self.feedback_error.clone()),
            ));
        }
        root
    }
}
