//! Top toolbar: sources, synchronization, and export.

use super::animation::{export_button_entrance, quality_dropdown_motion, quality_shift_for_export};
use super::controls::{dropdown_surface, icon_button, prominent_button};
use super::menu_sidebar::app_menu_button;
use super::settings::{QUALITY_MENU_WIDTH, search_quality_button, search_quality_menu};
use super::timeline::zoom_slider;
use super::{
    AlignApp, AnchoredPositionMode, Context, Div, Operation, ParentElement, Styled, Theme,
    anchored, deferred, div, point, px, rgb,
};

// ---------------- top toolbar (sources / sync / export)

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
        // Media added during a job would be dropped; the job's own bar
        // offers Cancel.
        !busy,
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
        !data.clips.is_empty() && !busy,
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
                        dropdown_surface(
                            theme,
                            QUALITY_MENU_WIDTH,
                            search_quality_menu(cx, theme, data),
                        ),
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
