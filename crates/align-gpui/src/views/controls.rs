//! Shared controls (buttons, overlays, tips) and display formatting.

use super::*;

// ------------------------------------------------------------ rendering
//
// Colors come from `Theme::of(window.appearance())` (see `theme.rs`),
// so the whole UI follows the system light/dark appearance. No hardcoded
// dark-only fills live here.

/// Drag payload for the timeline zoom slider (value-less marker type).
#[derive(Clone, Copy)]
pub(super) struct ZoomDrag;

/// Invisible drag ghost for the zoom slider interaction.
pub(super) struct ZoomDragGhost;

impl Render for ZoomDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(1.)).h(px(1.)).opacity(0.)
    }
}

/// Hover tooltip: one-line floating label (track sources, icon buttons).
pub(super) struct Tip {
    pub(super) text: String,
    pub(super) theme: Theme,
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

pub(super) fn hover_tip(
    text: String,
    theme: &Theme,
) -> impl Fn(&mut Window, &mut App) -> AnyView + use<> {
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
pub(super) fn overlay(
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

pub(super) fn button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl IntoElement,
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
        .child(label);
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

/// Two-state segmented control: a sunken track with the selected segment
/// raised, as in native settings panes.
pub(super) fn segmented_control(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    segments: [(&'static str, &'static str, bool, SettingsScope); 2],
) -> impl IntoElement + use<> {
    let theme = *theme;
    let mut track = div()
        .flex()
        .flex_row()
        .p(px(2.))
        .gap(px(2.))
        .rounded_lg()
        .bg(rgb(theme.button_hover))
        .border_1()
        .border_color(rgb(theme.separator));
    for (id, label, selected, scope) in segments {
        let segment = div()
            .id(id)
            .px_3()
            .h(px(24.))
            .flex()
            .items_center()
            .rounded_md()
            .text_size(px(12.))
            .cursor_pointer();
        let segment = if selected {
            segment
                .bg(rgb(theme.panel))
                .shadow_sm()
                .text_color(rgb(theme.text))
                .font_weight(gpui::FontWeight(500.0))
        } else {
            segment
                .text_color(rgb(theme.dim))
                .hover(move |this| this.text_color(rgb(theme.text)))
        };
        track = track.child(
            segment
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_settings_scope(scope, cx))),
        );
    }
    div().flex().child(track)
}

pub(super) fn prominent_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Stateful<Div> {
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
pub(super) fn icon_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    icon: &'static str,
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

pub(super) fn modal_header(
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
            "icons/close.svg",
            "Close",
            true,
            close,
        ))
}

pub(super) fn sidebar_scrim(theme: &Theme, closing: bool) -> impl IntoElement {
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
            entrance(if closing { SIDEBAR_CLOSE_MS } else { 280 }),
            move |el, progress| el.opacity(if closing { 1. - progress } else { progress }),
        )
}

/// Text button with a leading icon (toolbar actions like Add Media).
pub(super) fn fmt_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

/// Approximate width reserved for one ruler label.
pub(super) const RULER_LABEL_W: f32 = 76.;

/// Ruler positions (seconds from the timeline start) on round values of
/// the displayed clock: the smallest 1–2–5 style step (seconds, minutes,
/// hours) that keeps labels about 110 px apart, aligned to multiples of
/// the step on the timecode when one is shown.
pub(super) fn ruler_ticks(duration: f64, px_per_sec: f64, origin: f64) -> Vec<f64> {
    const STEPS: [f64; 16] = [
        1., 2., 5., 10., 15., 30., 60., 120., 300., 600., 900., 1800., 3600., 7200., 14400., 28800.,
    ];
    if !(duration > 0. && px_per_sec > 0.) {
        return Vec::new();
    }
    let wanted = 110. / px_per_sec;
    let step = STEPS
        .iter()
        .copied()
        .find(|step| *step >= wanted)
        .unwrap_or(STEPS[STEPS.len() - 1]);
    let first = (origin / step).ceil() * step - origin;
    let count = ((duration - first) / step).floor().max(-1.) as i64 + 1;
    (0..count).map(|i| first + i as f64 * step).collect()
}

/// Clip fill tracks synchronization: pending while analysis runs, then
/// matched or unmatched.
pub(super) fn clip_color(state: crate::lane::BarMatchState) -> u32 {
    use crate::lane::BarMatchState;
    match state {
        BarMatchState::Pending => 0x007AFF,
        BarMatchState::Matched => 0x34C759,
        BarMatchState::Unmatched => 0xFF9500,
    }
}

pub(super) fn fmt_timecode(start: f64, at: f64) -> String {
    let t = start + at;
    let h = (t / 3600.0).floor() as i64 % 24;
    let m = (t.rem_euclid(3600.0) / 60.0).floor() as i64;
    let s = t.rem_euclid(60.0).floor() as i64;
    let f = ((t - t.floor()) * 25.0).floor() as i64 % 25;
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

pub(super) fn waveform_columns(waveform: &[f32], width: f32) -> (Vec<f32>, f32) {
    let visible_bins = ((width / 2.0).floor() as usize).clamp(1, waveform.len());
    let columns = (0..visible_bins)
        .map(|index| {
            let start = index * waveform.len() / visible_bins;
            let end = (index + 1) * waveform.len() / visible_bins;
            let values = &waveform[start..end];
            values.iter().sum::<f32>() / values.len() as f32
        })
        .collect();
    (columns, (width / visible_bins as f32).max(1.0))
}
