//! Top toolbar: sources, synchronization, and export.

use super::*;

// ---------------- top toolbar (sources / sync / export)

pub(super) fn app_menu_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    active: bool,
) -> impl IntoElement {
    button(
        cx,
        theme,
        "btn-app-menu",
        div()
            .flex()
            .items_center()
            .gap_1()
            .child("Menu")
            .child(svg_icon("icons/chevron-down.svg", 12., theme.icon)),
        active,
        |this, event, window, cx| {
            this.data.menu = None;
            this.app_menu_position = Some(event.position() + point(px(0.), px(12.)));
            this.focus_handle.focus(window);
            cx.notify();
            cx.stop_propagation();
        },
    )
}

pub(super) fn app_menu_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    window: &Window,
) -> Stateful<Div> {
    let menus = crate::app_menu::in_window_menus(cx.get_menus().unwrap_or_default());
    let mut panel = div()
        .id("app-menu")
        .flex()
        .flex_col()
        .p_1()
        .min_w(px(260.))
        .max_h(px(
            (f32::from(window.viewport_size().height) - 32.).clamp(100., 560.)
        ))
        .overflow_y_scroll()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
    for menu in menus {
        panel = append_app_menu(cx, theme, panel, menu, String::new());
    }
    panel
}

pub(super) fn append_app_menu(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    mut panel: Stateful<Div>,
    menu: OwnedMenu,
    parent: String,
) -> Stateful<Div> {
    let path = if parent.is_empty() {
        menu.name.to_string()
    } else {
        format!("{parent} › {}", menu.name)
    };
    panel = panel.child(menu_header(theme, path.clone()));
    for (index, item) in menu.items.into_iter().enumerate() {
        match item {
            OwnedMenuItem::Action { name, action, .. } => {
                panel = panel.child(menu_row(
                    cx,
                    theme,
                    format!("app-menu-{path}-{index}"),
                    name,
                    false,
                    move |this, _, window, cx| {
                        this.app_menu_position = None;
                        cx.notify();
                        window.dispatch_action(action.boxed_clone(), cx);
                        cx.stop_propagation();
                    },
                ));
            }
            OwnedMenuItem::Submenu(submenu) => {
                panel = append_app_menu(cx, theme, panel, submenu, path.clone());
            }
            OwnedMenuItem::Separator => {
                panel = panel.child(
                    div()
                        .flex_shrink_0()
                        .my_1()
                        .h(px(1.))
                        .bg(rgb(theme.separator)),
                );
            }
            OwnedMenuItem::SystemMenu(_) => {}
        }
    }
    panel
}

pub(super) fn toolbar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    active: bool,
    quality_closing: bool,
) -> Div {
    let live = matches!(data.operation, crate::state::Operation::Synchronizing);
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
    if cfg!(any(target_os = "windows", target_os = "linux")) {
        bar = bar.child(app_menu_button(cx, theme, active));
    }
    bar = bar.child(icon_button(
        cx,
        theme,
        "btn-add",
        "icons/plus.svg",
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
        "icons/trash.svg",
        trash_tip,
        !data.clips.is_empty(),
        |this, _, _, cx| this.delete_or_clear(cx),
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
        if has_timeline {
            bar = bar.child(quality_shift_for_export(quality));
        } else {
            bar = bar.child(quality);
        }
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
    if !data.show_export && has_timeline {
        bar = bar.child(export_button_entrance(prominent_button(
            cx,
            theme,
            "btn-export-bar",
            "Export",
            active && data.can_export(),
            |this, _, _, cx| this.start_export_sheet(cx),
        )));
    }
    bar
}

pub(super) fn timeline_zoom_controls(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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
            "icons/fit.svg",
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
            "icons/zoom-out.svg",
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
            "icons/zoom-in.svg",
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
