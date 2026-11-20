//! Search quality and synchronization settings.

use super::animation::{entrance, settings_reset_motion, settings_select_motion};
use super::controls::{icon_button, prominent_button, segmented_control};
use super::menus::menu_row;
use super::{
    AlignApp, AnchoredPositionMode, AnimationExt, AudioAnalysisSource, ClipOrder, Context, Div,
    FluentBuilder, InteractiveElement, IntoElement, MatchThreshold, MouseButton, ParentElement,
    SETTINGS_MOTION_MS, SettingsScope, SettingsSelect, SharedString, Stateful,
    StatefulInteractiveElement, Styled, TemporalMode, Theme, TrackContent, anchored, deferred, div,
    point, px, rgb, svg_icon,
};

pub(super) fn audio_source_label(source: AudioAnalysisSource) -> &'static str {
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

pub(super) fn match_threshold_label(value: MatchThreshold) -> &'static str {
    match value {
        MatchThreshold::Permissive => "More matches",
        MatchThreshold::Balanced => "Balanced",
        MatchThreshold::Conservative => "Fewer false matches",
    }
}

pub(super) fn clip_order_label(value: ClipOrder) -> &'static str {
    match value {
        ClipOrder::Auto => "Auto",
        ClipOrder::AlternateAuto => "Alternate Auto",
        ClipOrder::AsImported => "As imported",
        ClipOrder::ByDateTime => "Date & time",
        ClipOrder::ByFileName => "File name",
        ClipOrder::Ignore => "Ignore",
    }
}

pub(super) fn track_content_label(value: TrackContent) -> &'static str {
    match value {
        TrackContent::Auto => "Automatic",
        TrackContent::Linear => "Linear",
        TrackContent::Takes => "Takes",
    }
}

pub(super) fn scoped_label<T: Copy>(
    value: Option<T>,
    common: T,
    label: impl Fn(T) -> &'static str,
) -> String {
    value.map_or_else(
        || format!("Inherit ({})", label(common)),
        |value| label(value).to_string(),
    )
}

pub(super) fn typed_settings_options<T: Copy + PartialEq>(
    current_scope: bool,
    current: Option<T>,
    common: T,
    values: &[T],
    label: impl Fn(T) -> &'static str,
) -> Vec<(String, bool)> {
    let mut options = Vec::with_capacity(values.len() + usize::from(current_scope));
    if current_scope {
        options.push((format!("Inherit ({})", label(common)), current.is_none()));
    }
    options.extend(
        values
            .iter()
            .copied()
            .map(|value| (label(value).to_string(), current == Some(value))),
    );
    options
}

pub(super) fn settings_options(
    data: &crate::state::AppData,
    select: SettingsSelect,
) -> Vec<(String, bool)> {
    let current_scope = data.settings_scope == SettingsScope::CurrentSequence;
    let sequence = data.current_sequence_settings();
    match select {
        SettingsSelect::Search => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.search_accuracy
            } else {
                Some(data.common_settings.search_accuracy)
            },
            data.common_settings.search_accuracy,
            &align_core::SearchAccuracy::ALL,
            |value| value.label(),
        ),
        SettingsSelect::Audio => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.audio_source
            } else {
                Some(data.common_settings.audio_source)
            },
            data.common_settings.audio_source,
            &[
                AudioAnalysisSource::Automatic,
                AudioAnalysisSource::AllMixed,
                AudioAnalysisSource::MixedStream(0),
                AudioAnalysisSource::Channel(0),
            ],
            audio_source_label,
        ),
        SettingsSelect::Temporal => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.temporal_mode
            } else {
                Some(data.common_settings.temporal_mode)
            },
            data.common_settings.temporal_mode,
            &[
                TemporalMode::Auto,
                TemporalMode::RecStart,
                TemporalMode::RecStop,
                TemporalMode::Timecode,
            ],
            |value| value.title(),
        ),
        SettingsSelect::Threshold => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.match_threshold
            } else {
                Some(data.common_settings.match_threshold)
            },
            data.common_settings.match_threshold,
            &[
                MatchThreshold::Permissive,
                MatchThreshold::Balanced,
                MatchThreshold::Conservative,
            ],
            match_threshold_label,
        ),
        SettingsSelect::Order => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.clip_order
            } else {
                Some(data.common_settings.clip_order)
            },
            data.common_settings.clip_order,
            &[
                ClipOrder::Auto,
                ClipOrder::AlternateAuto,
                ClipOrder::AsImported,
                ClipOrder::ByDateTime,
                ClipOrder::ByFileName,
                ClipOrder::Ignore,
            ],
            clip_order_label,
        ),
        SettingsSelect::Content => typed_settings_options(
            current_scope,
            if current_scope {
                sequence.track_content
            } else {
                Some(data.common_settings.track_content)
            },
            data.common_settings.track_content,
            &[
                TrackContent::Auto,
                TrackContent::Linear,
                TrackContent::Takes,
            ],
            track_content_label,
        ),
    }
}

pub(super) fn settings_menu_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    select: SettingsSelect,
    index: usize,
    label: String,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!(
            "settings-{}-{index}",
            select.id()
        )))
        .h(px(28.))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.button_hover)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_settings_option(select, index, cx);
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
                    slot.child(svg_icon("icons/check.svg", 12., theme.accent))
                }),
        )
}

/// A labelled dropdown row: the menu it opens and how that menu is shown.
pub(super) struct Dropdown<S> {
    pub(super) id: SharedString,
    pub(super) label: &'static str,
    pub(super) value: String,
    pub(super) select: S,
    pub(super) open: bool,
    /// The open menu is playing its exit animation.
    pub(super) closing: bool,
}

impl<S: Copy + PartialEq> Dropdown<S> {
    /// `current` is the open menu, if any; `closing` its exit state.
    pub(super) fn new(
        id: impl Into<SharedString>,
        label: &'static str,
        value: String,
        select: S,
        current: Option<S>,
        closing: bool,
    ) -> Self {
        let open = current == Some(select);
        Self {
            id: id.into(),
            label,
            value,
            select,
            open,
            closing: open && closing,
        }
    }
}

pub(super) fn settings_select_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    row: Dropdown<SettingsSelect>,
    options: Vec<(String, bool)>,
) -> Div {
    let Dropdown {
        id,
        label: title,
        value,
        select,
        open,
        closing,
    } = row;
    let menu_height = options.len() as f32 * 28. + 8.;
    let mut menu = div()
        .id(SharedString::from(format!("settings-{}-menu", select.id())))
        .w(px(210.))
        .flex()
        .flex_col()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    for (index, (label, selected)) in options.into_iter().enumerate() {
        menu = menu.child(settings_menu_row(cx, theme, select, index, label, selected));
    }
    let mut control = div()
        .id(id)
        .relative()
        .w(px(210.))
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
        .text_color(rgb(theme.icon))
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.border)))
        .active(|this| this.opacity(0.62))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_settings_select(select, cx)))
        .child(div().min_w(px(0.)).truncate().child(value))
        .child(div().w(px(12.)).h(px(12.)).flex_shrink_0().child(svg_icon(
            "icons/chevron-down.svg",
            12.,
            theme.icon,
        )));
    if open {
        control = control.child(deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Local)
                .offset(point(px(0.), px(4.)))
                .snap_to_window_with_margin(px(8.))
                .child(settings_select_motion(menu, select, menu_height, closing)),
        ));
    }
    div()
        .w_full()
        .h(px(28.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child(title),
        )
        .child(control)
}

pub(super) fn settings_header(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    close_closing: bool,
    reset_closing: bool,
) -> Div {
    let dirty_scope = !data.scoped_settings_are_default();
    let mut actions = div().h(px(30.)).flex().flex_row().items_center().gap_1();
    if dirty_scope || reset_closing {
        actions = actions.child(settings_reset_motion(
            div().child(icon_button(
                cx,
                theme,
                "settings-reset",
                "icons/reset.svg",
                "Reset to defaults",
                true,
                |this, _, _, cx| this.reset_settings(cx),
            )),
            reset_closing,
        ));
    }
    if data.settings_changed {
        let mut confirmation = div().relative().w(px(68.)).h(px(30.));
        confirmation = confirmation.child(
            div().absolute().right(px(0.)).child(
                prominent_button(
                    cx,
                    theme,
                    "settings-apply",
                    "Apply",
                    true,
                    |this, _, _, cx| this.finish_search_settings(cx),
                )
                .with_animation(
                    "settings-apply-in",
                    entrance(SETTINGS_MOTION_MS),
                    |el, progress| el.opacity(progress),
                ),
            ),
        );
        if close_closing {
            confirmation = confirmation.child(
                div().absolute().right(px(0.)).child(
                    div()
                        .child(icon_button(
                            cx,
                            theme,
                            "settings-close-out",
                            "icons/close.svg",
                            "Close",
                            true,
                            |_, _, _, _| {},
                        ))
                        .with_animation(
                            "settings-close-out",
                            entrance(SETTINGS_MOTION_MS),
                            |el, progress| el.opacity(1. - progress),
                        ),
                ),
            );
        }
        actions = actions.child(confirmation);
    } else {
        let mut confirmation = div().relative().w(px(68.)).h(px(30.));
        confirmation = confirmation.child(
            div().absolute().right(px(0.)).child(
                div()
                    .child(icon_button(
                        cx,
                        theme,
                        "settings-close",
                        "icons/close.svg",
                        "Close",
                        true,
                        |this, _, _, cx| this.finish_search_settings(cx),
                    ))
                    .with_animation(
                        "settings-close-in",
                        entrance(SETTINGS_MOTION_MS),
                        |el, progress| el.opacity(progress),
                    ),
            ),
        );
        if close_closing {
            confirmation = confirmation.child(
                div().absolute().right(px(0.)).child(
                    prominent_button(
                        cx,
                        theme,
                        "settings-apply-out",
                        "Apply",
                        true,
                        |_, _, _, _| {},
                    )
                    .with_animation(
                        "settings-apply-out",
                        entrance(SETTINGS_MOTION_MS),
                        |el, progress| el.opacity(1. - progress),
                    ),
                ),
            );
        }
        actions = actions.child(confirmation);
    }
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
                .child("Synchronization settings"),
        )
        .child(div().flex_1())
        .child(actions)
}

pub(super) fn search_quality_dismiss_layer(cx: &mut Context<AlignApp>) -> impl IntoElement {
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

pub(super) fn export_select_dismiss_layer(cx: &mut Context<AlignApp>) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.close_export_select(cx);
                cx.stop_propagation();
            }),
        )
}

pub(super) fn search_quality_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    enabled: bool,
) -> impl IntoElement {
    let label = format!(
        "Quality: {}",
        data.current_effective_settings().search_accuracy.label()
    );
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
            "icons/chevron-down.svg",
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
                    this.search_quality_exit.cancel();
                    this.data.show_search_quality = true;
                    cx.notify();
                }
            }));
    } else {
        control = control.text_color(rgb(theme.dim)).opacity(0.42);
    }
    control
}

pub(super) const QUALITY_MENU_WIDTH: f32 = 232.;

/// One-line guidance under each search level in the quality menu.
pub(super) fn search_quality_hint(accuracy: align_core::SearchAccuracy) -> &'static str {
    use align_core::SearchAccuracy::*;
    match accuracy {
        Fast => "Quickest; clear shared sound",
        Balanced => "Best for most shoots",
        Thorough => "Noisy rooms, distant mics",
        Deep => "Hard material, slower",
        Exhaustive => "Last resort, slowest",
    }
}

pub(super) fn search_quality_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: &'static str,
    selected: bool,
    accuracy: align_core::SearchAccuracy,
) -> impl IntoElement {
    div()
        .id(id.into())
        .h(px(40.))
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
        .child(
            div()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(div().truncate().child(label))
                .child(
                    div()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(rgb(theme.dim))
                        .child(search_quality_hint(accuracy)),
                ),
        )
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

pub(super) fn search_quality_menu(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
) -> Stateful<Div> {
    let selected = data.current_effective_settings().search_accuracy;
    let mut menu = div()
        .id("search-quality-menu")
        .w(px(QUALITY_MENU_WIDTH))
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
                    this.clear_settings_select();
                    this.settings_buttons.stop();
                    this.settings_initial_common = this.data.common_settings;
                    this.settings_initial_sequence = this.data.current_sequence_settings();
                    this.data.show_search_settings = true;
                    this.data.settings_changed = false;
                    cx.notify();
                },
            )),
    )
}

pub(super) fn search_settings_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    selected: Option<SettingsSelect>,
    select_closing: bool,
    close_closing: bool,
    reset_closing: bool,
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
    let scope_row = segmented_control(
        cx,
        theme,
        [
            (
                "settings-scope-common",
                "Common",
                !current_scope,
                SettingsScope::Common,
            ),
            (
                "settings-scope-sequence",
                "Current sequence",
                current_scope,
                SettingsScope::CurrentSequence,
            ),
        ],
    );
    div()
        .id("search-settings")
        .w(px(560.))
        .p_4()
        .flex().flex_col().gap_2()
        .rounded_lg().bg(rgb(theme.panel)).border_1().border_color(rgb(theme.border))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close_settings_select(cx)))
        .child(settings_header(cx, theme, data, close_closing, reset_closing))
        .child(div().text_size(px(12.)).text_color(rgb(theme.dim)).child(
            "Choose defaults for every sequence or replace them for the current sequence. Track menus inherit these values.",
        ))
        .child(scope_row)
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-search", "Search accuracy", scoped_label(search, common.search_accuracy, |value| value.label()), SettingsSelect::Search, selected, select_closing),
            settings_options(data, SettingsSelect::Search)))
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-audio", "Wave source", scoped_label(audio, common.audio_source, audio_source_label), SettingsSelect::Audio, selected, select_closing),
            settings_options(data, SettingsSelect::Audio)))
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-temporal", "Time source", scoped_label(temporal, common.temporal_mode, |value| value.title()), SettingsSelect::Temporal, selected, select_closing),
            settings_options(data, SettingsSelect::Temporal)))
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-threshold", "Match threshold", scoped_label(threshold, common.match_threshold, match_threshold_label), SettingsSelect::Threshold, selected, select_closing),
            settings_options(data, SettingsSelect::Threshold)))
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-order", "Clip order", scoped_label(order, common.clip_order, clip_order_label), SettingsSelect::Order, selected, select_closing),
            settings_options(data, SettingsSelect::Order)))
        .child(settings_select_row(cx, theme,
            Dropdown::new("settings-content", "Track content", scoped_label(content, common.track_content, track_content_label), SettingsSelect::Content, selected, select_closing),
            settings_options(data, SettingsSelect::Content)))
}
