//! Export sheet and its options.

use super::animation::{
    export_reveal, export_select_motion, motion_ease, sidebar_reveal, slide_in,
};
use super::controls::{button, hover_tip, modal_header, prominent_button};
use super::settings::Dropdown;
use super::{
    AlignApp, AnchoredPositionMode, Animation, AnimationExt, BoxShadow, ClickEvent, Context, Div,
    Duration, Entity, ExportInputs, ExportReveals, ExportSelect, ExportTarget, FluentBuilder,
    HashMap, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    Operation, ParentElement, PathFixerInputs, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, Styled, TextInput, Theme, Window, anchored, deferred, div,
    middle_ellipsis, point, px, rgb, rgba, svg_icon, updater,
};

// ---------------- export sheet

pub(super) fn export_section_title(theme: &Theme, text: &str) -> impl IntoElement {
    div()
        .text_sm()
        .text_color(rgb(theme.dim))
        .child(text.to_string())
}

/// Native-style checkbox row used by the export sheet.
pub(super) fn check_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    checked: bool,
    title: String,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Stateful<Div> {
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
            .child(svg_icon("icons/check.svg", 10.0, tick)),
    )
    .child(
        div()
            .text_color(rgb(if enabled { theme.text } else { theme.dim }))
            .child(title),
    )
}

pub(super) fn export_select_option(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    selected: bool,
    title: &'static str,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Stateful<Div> {
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
        .on_click(cx.listener(move |this, event, window, cx| {
            action(this, event, window, cx);
            this.close_export_select(cx);
        }))
        .child(div().min_w(px(0.)).truncate().child(title))
        .child(
            div()
                .w(px(16.))
                .h(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |slot| {
                    slot.child(svg_icon("icons/check.svg", 12., theme.accent))
                }),
        )
}

pub(super) fn export_select_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    row: Dropdown<ExportSelect>,
    enabled: bool,
    menu_height: f32,
    menu: Stateful<Div>,
) -> Div {
    let Dropdown {
        id,
        label,
        value,
        select,
        open,
        closing,
    } = row;
    let mut button = div()
        .id(id)
        .relative()
        .w(px(160.))
        .h(px(28.))
        .px_2()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_lg()
        .bg(rgb(if open {
            theme.border
        } else {
            theme.button_hover
        }))
        .text_size(px(12.))
        .child(div().min_w(px(0.)).truncate().child(value))
        .child(div().w(px(12.)).h(px(12.)).flex_shrink_0().child(svg_icon(
            "icons/chevron-down.svg",
            12.,
            theme.icon,
        )));
    if enabled {
        button = button
            .text_color(rgb(theme.icon))
            .cursor_pointer()
            .hover(|this| this.bg(rgb(theme.border)))
            .active(|this| this.opacity(0.62))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_export_select(select, cx);
            }));
    } else {
        button = button.text_color(rgb(theme.dim)).opacity(0.42);
    }
    if open {
        button = button.child(deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Local)
                .offset(point(px(0.), px(4.)))
                .snap_to_window_with_margin(px(8.))
                .child(export_select_motion(menu, select, menu_height, closing)),
        ));
    }
    div()
        .w_full()
        .h(px(28.))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child(label),
        )
        .child(button)
}

pub(super) fn aaf_frame_rate_options()
-> [(&'static str, Option<align_core::MediaTime>, &'static str); 9] {
    [
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
    ]
}

pub(super) fn aaf_frame_rate_label(value: Option<align_core::MediaTime>) -> &'static str {
    aaf_frame_rate_options()
        .into_iter()
        .find_map(|(_, option, label)| (option == value).then_some(label))
        .unwrap_or("Automatic")
}

pub(super) fn unmatched_placement_label(
    placement: align_core::export_model::UnmatchedPlacement,
) -> &'static str {
    use align_core::export_model::UnmatchedPlacement as P;
    match placement {
        P::ByOrderAndTime => "By order & time",
        P::ByOrderOnly => "By order only",
        P::Remove => "Remove",
    }
}

pub(super) fn step_button(
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

/// A labelled number of seconds with − and + buttons; zero reads "Off".
pub(super) struct Stepper {
    pub(super) id: &'static str,
    pub(super) title: &'static str,
    pub(super) value: f64,
    pub(super) decimals: usize,
    pub(super) enabled: bool,
}

pub(super) fn seconds_stepper(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    stepper: Stepper,
    decrement: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
    increment: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let Stepper {
        id,
        title,
        value,
        decimals,
        enabled,
    } = stepper;
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

pub(super) fn export_text_field(
    theme: &Theme,
    title: &'static str,
    input: Entity<TextInput>,
) -> Div {
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

pub(super) fn export_dependent_options(theme: &Theme, content: impl IntoElement) -> Div {
    div()
        .ml_4()
        .pl_2()
        .border_l_1()
        .border_color(rgb(theme.separator))
        .child(content)
}

pub(super) fn path_fixer_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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

pub(super) fn export_scrollbar(
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
                // A slim overlay-style thumb on an invisible track; the
                // full 10 px column stays the hit area.
                .left(px(3.))
                .right(px(3.))
                .h(px(thumb_height))
                .rounded_full()
                .bg(rgba((theme.dim << 8) | 0x70))
                .hover(|this| this.bg(rgba((theme.dim << 8) | 0xC0))),
        )
}

/// View state of the export sheet beyond the app data.
#[derive(Clone, Copy)]
pub(super) struct ExportPanel<'a> {
    pub(super) inputs: &'a ExportInputs,
    pub(super) scroll: &'a ScrollHandle,
    /// Option groups playing their hide animation, by reveal id.
    pub(super) reveals_closing: &'a HashMap<&'static str, u64>,
    pub(super) select: Option<ExportSelect>,
    pub(super) select_closing: bool,
}

pub(super) fn export_sidebar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    panel: ExportPanel,
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
                        .bg(rgb(theme.button_hover));
                    if can_close {
                        close = close
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(theme.border)))
                            .active(|this| this.opacity(0.62))
                            .tooltip(hover_tip("Close export panel".to_string(), theme))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_export_sidebar(cx);
                            }));
                    } else {
                        close = close.opacity(0.32);
                    }
                    close.child(svg_icon("icons/chevron-right.svg", 12., theme.icon))
                })
                .child(div().flex_1()),
        )
        .child(export_sheet(cx, theme, data, panel))
        .cursor_default()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
    sidebar_reveal(
        panel,
        if closing {
            "export-sidebar-slide-out"
        } else {
            "export-sidebar-slide-in"
        },
        440.,
        super::animation::SidebarEdge::Right,
        closing,
    )
}

pub(super) fn export_reveal_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Show in Finder",
        "windows" => "Show in Explorer",
        _ => "Open Folder",
    }
}

pub(super) fn export_sidebar_action(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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

pub(super) fn export_success_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: &'static str,
    label: &'static str,
    prominent: bool,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Stateful<Div> {
    let mut button = div()
        .id(id)
        .w_full()
        .h(px(34.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight(600.0))
        .child(label);
    if prominent {
        button = button
            .bg(rgb(theme.accent))
            .text_color(rgb(theme.on_accent))
            .hover(|this| this.bg(rgb(theme.accent_hover)));
    } else {
        button = button
            .bg(rgb(theme.button_hover))
            .text_color(rgb(theme.icon))
            .hover(|this| this.bg(rgb(theme.border)));
    }
    if enabled {
        button = button
            .cursor_pointer()
            .active(|this| this.opacity(0.72))
            .on_click(cx.listener(move |this, event, window, cx| {
                action(this, event, window, cx);
            }));
    } else {
        button = button.opacity(0.42);
    }
    button
}

pub(super) fn export_confetti(theme: &Theme) -> Div {
    let mut layer = div()
        .absolute()
        .top(px(4.))
        .left(px(0.))
        .w_full()
        .h(px(104.));
    if crate::motion::reduced_motion() {
        return layer;
    }

    // Fixed geometry keeps the burst tasteful and deterministic. Each piece
    // animates at paint time so the success layout never shifts underneath it.
    let particles = [
        (-78., -14., 4., 8., theme.accent),
        (-58., -43., 6., 5., theme.orange),
        (-35., -59., 4., 9., theme.cyan),
        (-12., -68., 5., 6., theme.green),
        (18., -66., 4., 8., theme.orange),
        (42., -54., 6., 5., theme.accent),
        (65., -34., 4., 9., theme.green),
        (80., -8., 5., 6., theme.cyan),
        (-66., 13., 5., 5., theme.green),
        (-42., 24., 4., 8., theme.accent),
        (45., 22., 5., 7., theme.orange),
        (68., 11., 4., 5., theme.accent),
    ];
    for (index, (x, y, width, height, color)) in particles.into_iter().enumerate() {
        let particle = div()
            .absolute()
            .left(gpui::relative(0.5))
            .top(px(39.))
            .w(px(width))
            .h(px(height))
            .rounded_sm()
            .bg(rgb(color));
        layer = layer.child(
            crate::motion::Slide {
                child: Some(particle),
                x: 0.,
                y: 0.,
            }
            .with_animation(
                SharedString::from(format!("export-success-confetti-{index}")),
                Animation::new(Duration::from_millis(720)),
                move |mut element, progress| {
                    let outward = motion_ease((progress / 0.72).clamp(0., 1.));
                    element.x = x * outward;
                    element.y = y * outward + 30. * progress * progress;
                    let opacity = if progress < 0.1 {
                        progress / 0.1
                    } else if progress > 0.62 {
                        (1. - progress) / 0.38
                    } else {
                        1.
                    }
                    .clamp(0., 1.);
                    element.child = element.child.map(|child| child.opacity(opacity));
                    element
                },
            ),
        );
    }
    layer
}

pub(super) fn export_support_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    let accent = theme.accent;
    let separator = theme.separator;
    let button = div()
        .id(id)
        .flex_1()
        .h(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(separator))
        .text_size(px(11.))
        .font_weight(gpui::FontWeight(550.0))
        .text_color(rgb(theme.icon))
        .child(svg_icon(icon, 13., theme.icon))
        .child(label)
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.button_hover)))
        .active(|this| this.opacity(0.62))
        .on_click(cx.listener(move |this, event, window, cx| {
            action(this, event, window, cx);
        }));

    button.with_animation(
        SharedString::from(format!("{id}-highlight")),
        Animation::new(Duration::from_millis(1_250)),
        move |button, progress| {
            if reduced || progress < 0.62 {
                return button.shadow_none().border_color(rgb(separator));
            }
            let phase = ((progress - 0.62) / 0.38).clamp(0., 1.);
            let pulse = (std::f32::consts::PI * phase).sin();
            if pulse <= 0.001 {
                return button.shadow_none().border_color(rgb(separator));
            }
            let glow_alpha = (42. * pulse).round() as u32;
            let border_alpha = (72. + 92. * pulse).round() as u32;
            button
                .border_color(rgba((accent << 8) | border_alpha))
                .shadow(vec![BoxShadow {
                    color: rgba((accent << 8) | glow_alpha).into(),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(10. + 5. * pulse),
                    spread_radius: px(1.),
                }])
        },
    )
}

pub(super) fn export_success_sheet(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
) -> Stateful<Div> {
    let destination = data
        .export_dir
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Export destination".to_string());
    let destination_name = data
        .export_dir
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Export destination".to_string());
    let file_count = data.exported_files.len();
    let file_count_label = format!(
        "{file_count} file{}",
        if file_count == 1 { "" } else { "s" }
    );

    let hero = div()
        .relative()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .child(export_confetti(theme))
        .child(
            div()
                .w(px(52.))
                .h(px(52.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgba((theme.green << 8) | 0x26))
                .child(svg_icon("icons/check.svg", 23., theme.green)),
        )
        .child(
            div()
                .mt_3()
                .text_size(px(20.))
                .font_weight(gpui::FontWeight(650.0))
                .text_color(rgb(theme.text))
                .child("Export Complete"),
        );

    let destination_card = div()
        .w_full()
        .h(px(62.))
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .rounded_xl()
        .border_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.bg))
        .shadow_xs()
        .child(
            div()
                .w(px(36.))
                .h(px(36.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .bg(rgba((theme.accent << 8) | 0x1E))
                .child(svg_icon("icons/folder.svg", 18., theme.accent)),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight(600.0))
                        .text_color(rgb(theme.text))
                        .child(destination_name),
                )
                .child(
                    div()
                        .id("export-success-destination-path")
                        .mt(px(2.))
                        .truncate()
                        .text_size(px(10.))
                        .text_color(rgb(theme.dim))
                        .tooltip(hover_tip(destination.clone(), theme))
                        .child(middle_ellipsis(&destination, 38)),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(rgb(theme.dim))
                .child(file_count_label),
        );

    let actions = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(export_success_button(
            cx,
            theme,
            "export-success-reveal",
            export_reveal_label(),
            true,
            true,
            |this, _, _, cx| {
                this.data.reveal_export();
                cx.notify();
            },
        ))
        .child(export_success_button(
            cx,
            theme,
            "export-success-back",
            "Back to Export Settings",
            false,
            true,
            |this, _, _, cx| {
                this.data.return_to_export_settings();
                cx.notify();
            },
        ));

    let support = div()
        .w_full()
        .pt_4()
        .border_t_1()
        .border_color(rgb(theme.separator))
        .child(
            div()
                .mb_2()
                .w_full()
                .text_center()
                .text_size(px(10.))
                .font_weight(gpui::FontWeight(600.0))
                .text_color(rgb(theme.dim))
                .child("Align saved you some time?"),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .gap_2()
                .child(export_support_button(
                    cx,
                    theme,
                    "export-success-star",
                    "icons/github.svg",
                    "Star on GitHub",
                    |_, _, _, cx| cx.open_url(updater::REPOSITORY_URL),
                ))
                .child(export_support_button(
                    cx,
                    theme,
                    "export-success-support",
                    "icons/heart.svg",
                    "Support Align",
                    |_, _, _, cx| cx.open_url(updater::DONATION_URL),
                )),
        );

    div()
        .id("export-success-sheet")
        .relative()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .p_5()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(slide_in(hero, "export-success-hero", 0))
        .child(
            div()
                .mt_5()
                .child(slide_in(destination_card, "export-success-destination", 50)),
        )
        .child(
            div()
                .mt_3()
                .child(slide_in(actions, "export-success-actions", 100)),
        )
        .child(div().flex_1())
        .child(slide_in(support, "export-success-support-footer", 150))
}

pub(super) fn export_sheet(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    panel: ExportPanel,
) -> Stateful<Div> {
    use crate::state::Operation;
    if matches!(data.operation, Operation::Exported) && data.export_started {
        return export_success_sheet(cx, theme, data);
    }
    let scroll = panel.scroll;
    let busy = matches!(data.operation, Operation::Exporting);
    let sheet_context = SheetContext {
        theme,
        data,
        panel,
        reveals: ExportReveals::new(data),
        busy,
    };
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
    sheet = sheet.child(export_destination_row(cx, sheet_context));
    let mut columns = div()
        .id("export-sidebar-scroll")
        .flex()
        .flex_col()
        .gap_4()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .pb_2();
    columns = columns.child(export_formats_section(cx, sheet_context));
    let mut settings = div().flex().flex_col().gap_3();
    // Rendering is offered only when the result contains measured drift.
    if data.has_drift() {
        settings = settings.child(export_audio_section(cx, sheet_context));
    }
    settings = settings
        .child(export_timeline_section(cx, sheet_context))
        .child(export_cleanup_section(cx, sheet_context));
    if data.unmatched_count() > 0 {
        settings = settings.child(export_unmatched_section(cx, sheet_context));
    }
    // Naming stays in the same scrollable form as the processing settings.
    settings = settings
        .child(export_names_section(cx, sheet_context))
        .child(export_defaults_section(cx, sheet_context));
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
        if data.export.correct_drift || data.export.media_files {
            sheet = sheet.child(
                div()
                    .w_full()
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
    }
    // In-flight cancellation stays in the persistent operation bar so it is
    // not duplicated here.
    sheet
}

/// The option to organize imported source clips. Resolve XML and AAF carry
/// no browser organization, so it applies only with another editor format.
fn export_source_bins(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Stateful<Div> {
    let SheetContext {
        theme, data, busy, ..
    } = sheet;
    let applies = [
        ExportTarget::Premiere,
        ExportTarget::FinalCutPro,
        ExportTarget::ResolveOtio,
    ]
    .iter()
    .any(|target| data.export.targets.contains(target));
    check_row(
        cx,
        theme,
        "exp-media-bins",
        data.export.media_bins,
        "Organize imported source clips".to_string(),
        !busy && applies,
        |this, _, _, cx| {
            this.data.export.media_bins = !this.data.export.media_bins;
            cx.notify();
        },
    )
}

/// Saving the sheet as the starting point for new jobs, and loading the
/// saved or Align's defaults back.
fn export_defaults_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme,
        data,
        busy,
        panel: ExportPanel { inputs, .. },
        ..
    } = sheet;
    let current = data.export_settings(inputs.labels(cx));
    let align_defaults = crate::state::ExportSettings::default();
    let saved = data.saved_export_settings.clone();
    let status = match &saved {
        Some(saved) if *saved == current => "Using your saved defaults.",
        None if current == align_defaults => "Using Align's defaults.",
        _ => "Changed for this export.",
    };
    let mut buttons = div().flex().flex_row().flex_wrap().gap_2().px_2();
    buttons = buttons.child(button(
        cx,
        theme,
        "btn-exp-save-defaults",
        "Save as Default",
        !busy && saved.as_ref() != Some(&current),
        |this, _, _, cx| this.save_export_defaults(cx),
    ));
    if let Some(saved) = saved.filter(|saved| *saved != current) {
        buttons = buttons.child(button(
            cx,
            theme,
            "btn-exp-revert-defaults",
            "Revert to Saved",
            !busy,
            move |this, _, _, cx| this.load_export_settings(saved.clone(), cx),
        ));
    }
    if current != align_defaults {
        buttons = buttons.child(button(
            cx,
            theme,
            "btn-exp-align-defaults",
            "Use Align Defaults",
            !busy,
            |this, _, _, cx| this.load_export_settings(crate::state::ExportSettings::default(), cx),
        ));
    }
    let note = |text: &'static str| {
        div()
            .px_2()
            .text_size(px(11.))
            .text_color(rgb(theme.dim))
            .child(text)
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(export_section_title(theme, "Defaults"))
        .child(note(status))
        .child(buttons)
        .child(note(
            "New jobs and Align's next launch start from the saved defaults. The \
             destination and sequence name are not saved.",
        ))
}

/// What every section of the export sheet reads.
#[derive(Clone, Copy)]
pub(super) struct SheetContext<'a> {
    theme: &'a Theme,
    data: &'a crate::state::AppData,
    panel: ExportPanel<'a>,
    reveals: ExportReveals,
    busy: bool,
}

/// The chosen export folder and the button that changes it.
fn export_destination_row(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme, data, busy, ..
    } = sheet;
    let mut row = div().flex().flex_row().items_center().gap_3().h(px(32.));
    row = row.child(export_section_title(theme, "Destination"));
    let destination = data
        .export_dir
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Not selected".to_string());
    let destination_display = middle_ellipsis(&destination, 31);
    let mut destination_label = div()
        .id("export-destination-path-text")
        .w_full()
        .max_w_full()
        .flex_1()
        .min_w(px(0.))
        .truncate()
        .text_color(rgb(if data.export_dir.is_some() {
            theme.text
        } else {
            theme.dim
        }))
        .child(destination_display);
    if data.export_dir.is_some() {
        destination_label = destination_label.tooltip(hover_tip(destination, theme));
    }
    row = row.child(
        div()
            .id("export-destination-path")
            .w(px(232.))
            .flex_shrink_0()
            .overflow_hidden()
            .child(destination_label),
    );
    row = row.child(button(
        cx,
        theme,
        "btn-exp-dir",
        "Choose…",
        !busy,
        |this, _, _, cx| this.choose_export_dir(cx),
    ));
    row
}

/// Names and labels of synchronized and unmatched clips.
fn export_names_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme,
        data,
        busy,
        reveals,
        panel:
            ExportPanel {
                inputs,
                reveals_closing,
                ..
            },
        ..
    } = sheet;
    let mut group = div().flex().flex_col().gap_1();
    group = group.child(export_section_title(theme, "Names & labels"));
    group = group.child(export_text_field(
        theme,
        "Sequence name",
        inputs.sequence_name.clone(),
    ));
    group = group.child(check_row(
        cx,
        theme,
        "exp-label-synced",
        data.export.label_synced,
        "Label synchronized clips".to_string(),
        !busy,
        |this, _, _, cx| {
            this.update_export_options(cx, |data| {
                data.export.label_synced = !data.export.label_synced;
            });
        },
    ));
    if reveals.synced_labels || reveals_closing.contains_key("export-synced-labels") {
        let mut details = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(export_text_field(
                theme,
                "Name marker",
                inputs.synced_symbol.clone(),
            ))
            .child(check_row(
                cx,
                theme,
                "exp-synced-symbol-suffix",
                data.export.synced_symbol_suffix,
                "Place marker after clip name".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export.synced_symbol_suffix = !this.data.export.synced_symbol_suffix;
                    cx.notify();
                },
            ));
        if reveals.synced_xml || reveals_closing.contains_key("export-synced-xml") {
            details = details.child(export_reveal(
                export_text_field(theme, "XML label color", inputs.synced_color.clone()),
                "export-synced-xml",
                36.,
                !reveals.synced_xml,
            ));
        }
        if reveals.synced_final_cut || reveals_closing.contains_key("export-synced-final-cut") {
            details = details.child(export_reveal(
                export_text_field(theme, "Final Cut audio role", inputs.synced_role.clone()),
                "export-synced-final-cut",
                36.,
                !reveals.synced_final_cut,
            ));
        }
        group = group.child(export_reveal(
            export_dependent_options(theme, details),
            "export-synced-labels",
            152.,
            !reveals.synced_labels,
        ));
    }
    if data.unmatched_count() > 0
        && (reveals.unmatched_label_control
            || reveals_closing.contains_key("export-unmatched-label-control"))
    {
        let mut unmatched = div().flex().flex_col().gap_1().child(check_row(
            cx,
            theme,
            "exp-label-unmatched",
            data.export.label_unmatched,
            "Label unmatched clips".to_string(),
            !busy,
            |this, _, _, cx| {
                this.update_export_options(cx, |data| {
                    data.export.label_unmatched = !data.export.label_unmatched;
                });
            },
        ));
        if reveals.unmatched_labels || reveals_closing.contains_key("export-unmatched-labels") {
            let mut details = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(export_text_field(
                    theme,
                    "Name marker",
                    inputs.unmatched_symbol.clone(),
                ))
                .child(check_row(
                    cx,
                    theme,
                    "exp-symbol-suffix",
                    data.export.unmatched_symbol_suffix,
                    "Place marker after clip name".to_string(),
                    !busy,
                    |this, _, _, cx| {
                        this.data.export.unmatched_symbol_suffix =
                            !this.data.export.unmatched_symbol_suffix;
                        cx.notify();
                    },
                ));
            if reveals.unmatched_xml || reveals_closing.contains_key("export-unmatched-xml") {
                details = details.child(export_reveal(
                    export_text_field(theme, "XML label color", inputs.unmatched_color.clone()),
                    "export-unmatched-xml",
                    36.,
                    !reveals.unmatched_xml,
                ));
            }
            if reveals.unmatched_final_cut
                || reveals_closing.contains_key("export-unmatched-final-cut")
            {
                details = details.child(export_reveal(
                    export_text_field(theme, "Final Cut audio role", inputs.unmatched_role.clone()),
                    "export-unmatched-final-cut",
                    36.,
                    !reveals.unmatched_final_cut,
                ));
            }
            unmatched = unmatched.child(export_reveal(
                export_dependent_options(theme, details),
                "export-unmatched-labels",
                152.,
                !reveals.unmatched_labels,
            ));
        }
        group = group.child(export_reveal(
            unmatched,
            "export-unmatched-label-control",
            192.,
            !reveals.unmatched_label_control,
        ));
    }
    group
}

/// Placement of clips without a synchronization match.
fn export_unmatched_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme,
        data,
        busy,
        reveals,
        panel:
            ExportPanel {
                reveals_closing,
                select: export_select,
                select_closing: export_select_closing,
                ..
            },
        ..
    } = sheet;
    use align_core::export_model::UnmatchedPlacement as P;
    let mut group = div().flex().flex_col();
    group = group.child(export_section_title(theme, "Unmatched clips"));
    let mut menu = div()
        .id("export-unmatched-placement-menu")
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
    for (placement, title, id) in [
        (P::ByOrderAndTime, "By order & time", "exp-unmatched-time"),
        (P::ByOrderOnly, "By order only", "exp-unmatched-order"),
        (P::Remove, "Remove", "exp-unmatched-remove"),
    ] {
        menu = menu.child(export_select_option(
            cx,
            theme,
            id,
            data.export.unmatched == placement,
            title,
            move |this, _, _, cx| {
                this.update_export_options(cx, |data| {
                    data.export.unmatched = placement;
                    data.relayout_result();
                });
            },
        ));
    }
    group = group.child(export_select_row(
        cx,
        theme,
        Dropdown::new(
            "exp-unmatched-placement",
            "Placement",
            unmatched_placement_label(data.export.unmatched).to_string(),
            ExportSelect::UnmatchedPlacement,
            export_select,
            export_select_closing,
        ),
        !busy,
        92.,
        menu,
    ));
    let placed = data.camera_clock_export_count();
    if placed > 0 {
        let clips = if placed == 1 {
            "1 clip".to_string()
        } else {
            format!("{placed} clips")
        };
        group = group.child(
            div()
                .px_1()
                .pb_1()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child(format!(
                    "Camera-clock placement applies to {clips}. Exported names begin with {}.",
                    align_core::camera_clock::CAMERA_CLOCK_LABEL
                )),
        );
    }
    if reveals.unmatched_kept || reveals_closing.contains_key("export-unmatched-kept") {
        group = group.child(export_reveal(
            export_dependent_options(
                theme,
                check_row(
                    cx,
                    theme,
                    "exp-disable-unmatched",
                    data.export.disable_unmatched,
                    "Disable in timeline".to_string(),
                    !busy,
                    |this, _, _, cx| {
                        this.data.export.disable_unmatched = !this.data.export.disable_unmatched;
                        cx.notify();
                    },
                ),
            ),
            "export-unmatched-kept",
            40.,
            !reveals.unmatched_kept,
        ));
    }
    group
}

/// Clip removal and trimming applied to the exported timeline.
fn export_cleanup_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme, data, busy, ..
    } = sheet;
    let mut group = div().flex().flex_col().gap_1();
    group = group.child(export_section_title(theme, "Cleanup"));
    group = group.child(export_source_bins(cx, sheet));
    group = group.child(check_row(
        cx,
        theme,
        "exp-cut-gaps",
        data.export.cut_common_gaps,
        "Cut gaps empty on all tracks".to_string(),
        !busy,
        |this, _, _, cx| {
            this.update_timeline_layout(|data| {
                data.export.cut_common_gaps = !data.export.cut_common_gaps;
                data.relayout_result();
            });
            cx.notify();
        },
    ));
    group = group.child(check_row(
        cx,
        theme,
        "exp-cut-lone",
        data.export.cut_lone_recorder,
        "Cut recorder audio with no camera".to_string(),
        !busy,
        |this, _, _, cx| {
            this.update_timeline_layout(|data| {
                data.export.cut_lone_recorder = !data.export.cut_lone_recorder;
                data.relayout_result();
            });
            cx.notify();
        },
    ));
    {
        group = group.child(seconds_stepper(
            cx,
            theme,
            Stepper {
                id: "exp-cut-short",
                title: "Remove clips shorter than",
                value: data.export.cut_shorter_than,
                decimals: 0,
                enabled: !busy,
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.cut_shorter_than = (data.export.cut_shorter_than - 1.0).max(0.0);
                    data.relayout_result();
                });
                cx.notify();
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.cut_shorter_than =
                        (data.export.cut_shorter_than + 1.0).min(3_600.0);
                    data.relayout_result();
                });
                cx.notify();
            },
        ));
        group = group.child(seconds_stepper(
            cx,
            theme,
            Stepper {
                id: "exp-trim-start",
                title: "Trim each clip start",
                value: data.export.trim_starts,
                decimals: 1,
                enabled: !busy,
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.trim_starts =
                        ((data.export.trim_starts - 0.1).max(0.0) * 10.0).round() / 10.0;
                    data.relayout_result();
                });
                cx.notify();
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.trim_starts =
                        ((data.export.trim_starts + 0.1).min(3_600.0) * 10.0).round() / 10.0;
                    data.relayout_result();
                });
                cx.notify();
            },
        ));
        group = group.child(seconds_stepper(
            cx,
            theme,
            Stepper {
                id: "exp-trim-end",
                title: "Trim each clip end",
                value: data.export.trim_ends,
                decimals: 1,
                enabled: !busy,
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.trim_ends =
                        ((data.export.trim_ends - 0.1).max(0.0) * 10.0).round() / 10.0;
                    data.relayout_result();
                });
                cx.notify();
            },
            |this, _, _, cx| {
                this.update_timeline_layout(|data| {
                    data.export.trim_ends =
                        ((data.export.trim_ends + 0.1).min(3_600.0) * 10.0).round() / 10.0;
                    data.relayout_result();
                });
                cx.notify();
            },
        ));
    }
    group
}

/// Timeline assembly options.
fn export_timeline_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme, data, busy, ..
    } = sheet;
    let mut group = div().flex().flex_col().gap_1();
    group = group.child(export_section_title(theme, "Timeline"));
    let on = data.export.prevent_group_overlaps;
    group = group.child(check_row(
        cx,
        theme,
        "exp-prevent-overlaps",
        on,
        "Prevent overlaps".to_string(),
        !busy,
        |this, _, _, cx| {
            this.update_timeline_layout(|data| {
                data.export.prevent_group_overlaps = !data.export.prevent_group_overlaps;
                data.relayout_result();
            });
            cx.notify();
        },
    ));
    group
}

/// Drift-corrected audio rendering, offered when drift was measured.
fn export_audio_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme, data, busy, ..
    } = sheet;
    let mut group = div().flex().flex_col();
    group = group.child(export_section_title(theme, "Audio"));
    // Shown only when the result has drift.
    let on = data.export.correct_drift;
    group = group.child(check_row(
        cx,
        theme,
        "exp-drift",
        on,
        "Render drift-corrected audio".to_string(),
        !busy,
        |this, _, _, cx| {
            this.data.export.correct_drift = !this.data.export.correct_drift;
            cx.notify();
        },
    ));
    group
}

/// Export formats with their per-format options.
fn export_formats_section(cx: &mut Context<AlignApp>, sheet: SheetContext) -> Div {
    let SheetContext {
        theme,
        data,
        busy,
        reveals,
        panel:
            ExportPanel {
                reveals_closing,
                select: export_select,
                select_closing: export_select_closing,
                ..
            },
        ..
    } = sheet;
    let mut group = div().flex().flex_col().gap_1().flex_shrink_0();
    group = group.child(export_section_title(theme, "Formats"));
    for target in ExportTarget::all() {
        let active = data.export.targets.contains(&target);
        let title = target.title().to_string();
        group = group.child(check_row(
            cx,
            theme,
            format!("exp-fmt-{}", target.title()),
            active,
            title,
            !busy,
            move |this, _, _, cx| {
                this.update_export_options(cx, |data| {
                    if data.export.targets.contains(&target) {
                        data.export.targets.retain(|t| *t != target);
                    } else {
                        data.export.targets.push(target);
                    }
                });
            },
        ));
        if target == ExportTarget::FinalCutPro
            && (reveals.final_cut || reveals_closing.contains_key("export-final-cut-options"))
        {
            let mut options = div().flex().flex_col().gap_1();
            options = options.child(check_row(
                cx,
                theme,
                "exp-fcpxml-timeline",
                data.export.fcpxml_timeline,
                "Synchronized timeline".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.update_export_options(cx, |data| {
                        data.export.fcpxml_timeline = !data.export.fcpxml_timeline;
                    });
                },
            ));
            options = options.child(check_row(
                cx,
                theme,
                "exp-fcpxml-multicam",
                data.export.fcpxml_multicam,
                "Multicam clip".to_string(),
                !busy,
                |this, _, _, cx| {
                    this.data.export.fcpxml_multicam = !this.data.export.fcpxml_multicam;
                    cx.notify();
                },
            ));
            if reveals.storylines || reveals_closing.contains_key("export-storylines") {
                options = options.child(export_reveal(
                    check_row(
                        cx,
                        theme,
                        "exp-storylines",
                        data.export.fcpxml_storylines,
                        "Group tracks as storylines".to_string(),
                        !busy,
                        |this, _, _, cx| {
                            this.data.export.fcpxml_storylines =
                                !this.data.export.fcpxml_storylines;
                            cx.notify();
                        },
                    ),
                    "export-storylines",
                    32.,
                    !reveals.storylines,
                ));
            }
            group = group.child(export_reveal(
                export_dependent_options(theme, options),
                "export-final-cut-options",
                104.,
                !reveals.final_cut,
            ));
        }
        if target == ExportTarget::Premiere
            && (reveals.premiere || reveals_closing.contains_key("export-premiere-options"))
        {
            group = group.child(export_reveal(
                export_dependent_options(
                    theme,
                    check_row(
                        cx,
                        theme,
                        "exp-replaced",
                        data.export.replaced_sequence,
                        "Add a sequence with replaced camera audio".to_string(),
                        !busy,
                        |this, _, _, cx| {
                            this.data.export.replaced_sequence =
                                !this.data.export.replaced_sequence;
                            cx.notify();
                        },
                    ),
                ),
                "export-premiere-options",
                40.,
                !reveals.premiere,
            ));
        }
        if target == ExportTarget::Aaf
            && (reveals.aaf || reveals_closing.contains_key("export-aaf-options"))
        {
            let mut menu = div()
                .id("export-aaf-frame-rate-menu")
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
            for (id, frame_duration, title) in aaf_frame_rate_options() {
                menu = menu.child(export_select_option(
                    cx,
                    theme,
                    format!("exp-aaf-fps-{id}"),
                    data.export.aaf_frame_duration == frame_duration,
                    title,
                    move |this, _, _, cx| {
                        this.data.export.aaf_frame_duration = frame_duration;
                        cx.notify();
                    },
                ));
            }
            let options = export_select_row(
                cx,
                theme,
                Dropdown::new(
                    "exp-aaf-frame-rate",
                    "Frame rate",
                    aaf_frame_rate_label(data.export.aaf_frame_duration).to_string(),
                    ExportSelect::AafFrameRate,
                    export_select,
                    export_select_closing,
                ),
                !busy,
                260.,
                menu,
            );
            group = group.child(export_reveal(
                export_dependent_options(theme, options),
                "export-aaf-options",
                36.,
                !reveals.aaf,
            ));
        }
    }
    group = group.child(check_row(
        cx,
        theme,
        "exp-media",
        data.export.media_files,
        "Video files with external audio (.mov)".to_string(),
        !busy,
        |this, _, _, cx| {
            this.data.export.media_files = !this.data.export.media_files;
            cx.notify();
        },
    ));
    group
}
