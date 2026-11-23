//! Main content: drop zone and the source list.

use super::animation::{selection_control_motion, slide_in};
use super::controls::{fmt_duration, prominent_button};
use super::export::check_row;
use super::menu_sidebar::app_menu_button;
use super::timeline::timeline_preview;
use super::{
    AlignApp, ClipId, ClipState, Context, FluentBuilder, HashMap, InteractiveElement, IntoElement,
    MediaKind, ParentElement, SharedString, StatefulInteractiveElement, Styled, Theme,
    TimelineTransition, div, kind_badge, px, rgb, rgba, svg_icon,
};

// ---------------- main content (drop zone → file list → timeline)

pub(super) fn main_content(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    timeline_transitions: &HashMap<ClipId, TimelineTransition>,
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
        content = content.child(timeline_preview(
            cx,
            theme,
            data,
            timeline_transitions,
            fit_width,
            active,
        ));
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

pub(super) fn drop_zone(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    _data: &crate::state::AppData,
    active: bool,
) -> impl IntoElement {
    let accent = theme.accent;
    // The whole card is the drop target; it lights up while files hover.
    let panel = div()
        .id("dropzone")
        .w(px(520.))
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .px_8()
        .py_10()
        .rounded_2xl()
        .border_2()
        .border_dashed()
        .border_color(rgb(theme.border))
        .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
            style
                .border_color(rgb(accent))
                .bg(rgba((accent << 8) | 0x14))
        });
    div()
        .relative()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .when(
            cfg!(any(target_os = "windows", target_os = "linux")),
            |zone| {
                zone.child(
                    div()
                        .absolute()
                        .top(px(9.))
                        .left(px(12.))
                        .child(app_menu_button(cx, theme, active)),
                )
            },
        )
        .child(
            panel
                .child(
                    div()
                        .size(px(64.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgba((accent << 8) | 0x1A))
                        .child(svg_icon("icons/drop.svg", 28.0, accent)),
                )
                .child(
                    div()
                        .mt_5()
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
                .child(div().h(px(14.)))
                .child(prominent_button(
                    cx,
                    theme,
                    "btn-choose",
                    "Add media…",
                    active,
                    |this, _, _, cx| this.add_media(cx),
                ))
                .child(
                    div()
                        .mt_6()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_0p5()
                        .text_xs()
                        .text_color(rgb(theme.dim))
                        .child(MEDIA_FORMATS_HINT)
                        .child(TIMELINE_FORMATS_HINT),
                ),
        )
}

/// Shown under the empty-state call to action.
pub(super) const MEDIA_FORMATS_HINT: &str = "WAV · AIFF · MP3 · M4A · MOV · MP4 · MXF · MTS · R3D";
pub(super) const TIMELINE_FORMATS_HINT: &str = "Timelines: FCP XML · FCPXML · AAF";

// ---------------- source list (selectable rows with a stable header slot)

pub(super) fn file_list(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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
                    this.selection_controls_exit.cancel();
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
        let state_icon: Option<(&str, u32)> = match &clip.state {
            ClipState::Synchronized { evidence, .. } => {
                if *evidence == align_core::MatchEvidence::Timecode {
                    Some(("icons/check.svg", theme.blue))
                } else {
                    Some(("icons/check.svg", theme.green))
                }
            }
            ClipState::Matching { .. } => Some(("icons/check.svg", theme.green)),
            ClipState::Unmatched | ClipState::Warning => Some(("icons/warn.svg", theme.orange)),
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
                    this.selection_controls_exit.cancel();
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
                    el.child(svg_icon("icons/check.svg", 10., theme.on_accent))
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
                    .child(if data.folder_inputs.contains(&clip.url) {
                        svg_icon("icons/folder.svg", 13.0, badge_color).into_any_element()
                    } else {
                        kind_badge(video, badge_color, None).into_any_element()
                    }),
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
