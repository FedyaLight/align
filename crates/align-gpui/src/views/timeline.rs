//! Timeline preview: toolbar, ruler, and lanes.

use super::*;

// ---------------- timeline preview (toolbar + ruler + lanes)

pub(super) fn timeline_preview(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    timeline_transitions: &HashMap<ClipId, TimelineTransition>,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    let mut root = div().flex().flex_col().bg(rgb(theme.bg)).overflow_hidden();
    // Lanes (the header row with zoom controls lives in the unified
    // toolbar now).
    root = root.child(timeline_lanes(
        cx,
        theme,
        data,
        timeline_transitions,
        fit_width,
        active,
    ));
    if data.is_stale() {
        // Stale dimming is applied per-lane via opacity on the container.
        root = root.opacity(0.5);
    }
    root
}

/// Draggable zoom slider:
/// press and drag along the track to set the Fit…8x level, with a thumb
/// knob marking the current position. Step buttons and Option/Command +
/// wheel cover the rest of the native gestures (trackpad pinch has no
/// GPUI 0.2 event).
pub(super) fn zoom_slider(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
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

pub(super) fn timeline_lanes(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    timeline_transitions: &HashMap<ClipId, TimelineTransition>,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    use crate::lane::{LABEL_WIDTH, bar_row_geometry, timeline_scale};
    const RULER_H: f32 = 32.0;
    let duration = timeline_duration(data);
    let zoom = data.zoom();
    let (px_per_sec, timeline_w) = timeline_scale(duration, zoom, fit_width);
    let tick_times = ruler_ticks(duration, px_per_sec, data.ruler_timecode.unwrap_or(0.));
    let target_poses = timeline_poses(data);

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
    for &at in &tick_times {
        let x = (at * px_per_sec) as f32;
        let label = match data.ruler_timecode {
            Some(start) => fmt_timecode(start, at),
            None => fmt_duration(at),
        };
        ticks = ticks
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .bottom(px(0.))
                    .w(px(1.))
                    .h(px(6.))
                    .bg(rgb(theme.border)),
            )
            // Labels that would run past the end are left out rather than
            // clipped mid-glyph.
            .when(x + RULER_LABEL_W <= timeline_w, |ticks| {
                ticks.child(
                    div()
                        .absolute()
                        .top(px(8.))
                        .left(px(x + 4.))
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(rgb(theme.dim))
                        .child(label),
                )
            });
    }
    tracks = tracks.child(ticks);

    // Rows.
    for (lane_index, lane) in data.lanes.iter().enumerate() {
        let kind_glyph = match lane.kind {
            MediaKind::Video => "V",
            MediaKind::Audio => "A",
        };
        let kind_color = theme.icon;
        let lane_id = lane.id.clone();
        let first_clip = lane.clips.first().map(|c| c.clip_id.clone());
        // The tooltip shows the source and any active stream/channel override.
        let override_suffix = match data.effective_lane_analysis_source(&lane_id) {
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
        // right-click.
        let mut label = div()
            .id(SharedString::from(format!("lane-{lane_id}")))
            .w(px(LABEL_WIDTH))
            .flex_shrink_0()
            .overflow_hidden()
            .h(px(TIMELINE_ROW_H))
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
                            this.data.menu = Some(crate::state::MenuTarget {
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
        // Absolute placement prevents minimum-width bars from shifting
        // their neighbors.
        {
            let mut track = div()
                .relative()
                .w(px(timeline_w))
                .h(px(TIMELINE_ROW_H))
                .flex_shrink_0();
            // Vertical gridlines at ruler ticks.
            for &at in &tick_times {
                let x = (at * px_per_sec) as f32;
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
            for (bar, (x, width)) in lane.clips.iter().zip(geometry.iter()) {
                let (x, width) = (*x, *width);
                let color = clip_color(bar.match_state);
                let ink = 0xFFFFFF;
                let clip_id = bar.clip_id.clone();
                // Keep narrow slivers square; cap other corner radii at 4 px.
                let mut el = div()
                    .id(SharedString::from(format!("bar-{}", bar.clip_id.0)))
                    .absolute()
                    .left(px(x))
                    .top(px(6.0))
                    .w(px(width))
                    .h(px(TIMELINE_ROW_H - 12.0));
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
                // Icon + name when wide enough.
                // Bar label: icon pinned top-left, text pinned to the
                // remaining width. Absolute blocks (not flex items) hold
                // exact bounds, so long names shape a real "…" instead of
                // hard-clipping past the bar edge. `el` itself is
                // absolutely positioned, hence a valid anchor.
                if width >= 24.0 {
                    if let Some(waveform) = data.waveform_previews.get(&bar.clip_id) {
                        let (columns, bar_width) = waveform_columns(waveform, width);
                        let mut preview = div()
                            .absolute()
                            .left(px(0.))
                            .right(px(0.))
                            .top(px(0.))
                            .bottom(px(0.))
                            .overflow_hidden();
                        for (index, amplitude) in columns.into_iter().enumerate() {
                            let height = 3.0 + amplitude * (TIMELINE_ROW_H - 21.0);
                            preview = preview.child(
                                div()
                                    .absolute()
                                    .left(px(index as f32 * bar_width))
                                    .top(px((TIMELINE_ROW_H - 12.0 - height) * 0.5))
                                    .w(px((bar_width - 1.0).max(1.0)))
                                    .h(px(height))
                                    .rounded_sm()
                                    .bg(rgba(0xFFFFFF42)),
                            );
                        }
                        el = el.child(preview);
                    }
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
                        this.data.menu = Some(crate::state::MenuTarget {
                            clip_id: clip_id.clone(),
                            stream_menu: false,
                            lane_id: None,
                            position: (f32::from(e.position.x), f32::from(e.position.y)),
                        });
                        cx.notify();
                    }),
                );
                let transition =
                    timeline_transitions
                        .get(&bar.clip_id)
                        .copied()
                        .filter(|transition| {
                            target_poses
                                .get(&bar.clip_id)
                                .is_some_and(|pose| !poses_differ(transition.to, *pose))
                        });
                if let Some(transition) = transition {
                    track = track.child(timeline_bar_motion(
                        el,
                        &bar.clip_id,
                        transition,
                        x,
                        width,
                        timeline_w,
                        lane_index as f32,
                    ));
                } else {
                    track = track.child(el.with_animation(
                        SharedString::from(format!("timeline-clip-enter-{}", bar.clip_id.0)),
                        entrance(220),
                        |el, progress| el.opacity(progress),
                    ));
                }
            }
            tracks = tracks.child(track);
        }
        // Row separator.
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
            // Zoom: Option/Alt or Cmd (Ctrl off macOS) + wheel.
            if !(event.modifiers.alt || event.modifiers.secondary()) {
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

    let lanes = div().w_full().flex().flex_row();
    let lanes = if data.show_export {
        lanes.pl_2().py_2()
    } else {
        lanes.p_2()
    };
    lanes.child(labels).child(viewport)
}
