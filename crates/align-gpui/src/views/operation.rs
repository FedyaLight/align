//! Warning banner and the bottom operation bar.

use super::animation::entrance;
use super::controls::{button, icon_button, modal_header};
use super::export::export_reveal_label;
use super::toolbar::timeline_zoom_controls;
use super::{
    AlignApp, AnimationExt, Context, Div, FluentBuilder, InteractiveElement, IntoElement,
    PROGRESS_MOVE_MS, ParentElement, ProgressMotion, SharedString, StatefulInteractiveElement,
    Styled, Theme, div, px, rgb, rgba, svg_icon,
};

// ---------------- warning banner + details

pub(super) fn warning_banner(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
) -> impl IntoElement {
    let n = data.warnings.len() + data.restore_warnings.len();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_5()
        .py_1()
        .bg(rgba((theme.warning << 8) | 0x14))
        .child(svg_icon("icons/warn.svg", 14.0, theme.orange))
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
        .when(data.can_relink_media(), |banner| {
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

pub(super) fn warning_details(theme: &Theme, data: &crate::state::AppData) -> impl IntoElement {
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
    for message in data.restore_warnings.iter().chain(&data.warnings).take(50) {
        panel = panel.child(div().text_color(rgb(theme.text)).child(message.clone()));
    }
    panel
}

// ---------------- bottom operation bar

pub(super) fn operation_bar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    progress: ProgressMotion,
    active: bool,
) -> Div {
    use crate::state::Operation;
    let busy = data.importing()
        || matches!(
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
        // A check only for complete success; partial results get a warning.
        bar = bar.child(if data.unmatched_count() == 0 {
            svg_icon("icons/check.svg", 14.0, theme.green)
        } else {
            svg_icon("icons/warn.svg", 14.0, theme.orange)
        });
    }
    // Status + progress.
    {
        let mut status = div()
            .flex()
            .flex_col()
            .min_w(px(0.))
            .text_size(px(12.))
            .gap_1();
        if !(data.show_export && matches!(data.operation, Operation::Exported)) {
            status = status.child(if data.importing() {
                "Reading inputs…".to_owned()
            } else {
                data.status.clone()
            });
        }
        if busy {
            let fill = div().h_full().rounded_full().bg(rgb(theme.accent));
            let fill = if crate::motion::reduced_motion() {
                fill.w(gpui::relative(progress.to)).into_any_element()
            } else {
                let ProgressMotion { from, to, .. } = progress;
                fill.w(gpui::relative(from))
                    .with_animation(
                        SharedString::from(format!("progress-{}", progress.generation)),
                        entrance(PROGRESS_MOVE_MS),
                        move |fill, eased| fill.w(gpui::relative(from + (to - from) * eased)),
                    )
                    .into_any_element()
            };
            status = status.child(
                div()
                    .w(px(280.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(theme.border))
                    .child(fill),
            );
        }
        bar = bar.child(status);
    }
    if busy {
        bar = bar.child(icon_button(
            cx,
            theme,
            "btn-cancel",
            "icons/close.svg",
            "Cancel",
            true,
            |this, _, _, cx| this.cancel_current(cx),
        ));
    }
    bar = bar.child(div().flex_1());
    if !busy {
        if let Some(result) = &data.result
            && result.stages.len() > 1
        {
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
                export_reveal_label(),
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
                        this.data.mark_sync_dirty();
                        this.project_changed(cx);
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

pub(super) fn stage_settings_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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
                    if this.update_timeline_layout(|data| data.select_sync_stage(index)) {
                        this.project_changed(cx);
                    }
                    cx.notify();
                },
            ));
        }
    }
    panel
}

pub(super) fn sequence_results_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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
                if this.update_timeline_layout(|data| data.select_sequence_result(index)) {
                    this.project_changed(cx);
                }
                cx.notify();
            },
        ));
    }
    panel
}
