//! Timeline preview: toolbar, ruler, and lanes.

use super::animation::entrance;
use super::controls::{
    RULER_LABEL_W, ZoomDrag, ZoomDragGhost, clip_status_icon, fmt_duration, fmt_timecode,
    hover_tip, ruler_ticks, waveform_columns,
};
use super::{
    AlignApp, AnimationExt, AudioAnalysisSource, Context, DragMoveEvent, FluentBuilder, HashMap,
    InteractiveElement, IntoElement, MediaKind, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, TIMELINE_ROW_H, Theme,
    TimelineTransition, div, point, poses_differ, px, rgb, rgba, svg_icon, timeline_duration,
    timeline_poses,
};
use gpui::AppContext as _;

// ---------------- timeline preview (toolbar + ruler + lanes)

/// Size of a clip's status mark in its top-right corner.
const STATUS_ICON: f32 = 10.0;
/// Gap between the status mark and the clip's top and right edges. The
/// mark ends above the name's line, so it never covers the name.
const STATUS_INSET: f32 = 4.0;

pub(super) fn timeline_preview(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    timeline_transitions: &HashMap<String, TimelineTransition>,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    let mut root = div().flex().flex_col().overflow_hidden();
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

/// Static stripes over a clip the analysis has not settled yet. Nothing
/// moves, so they read the same with reduced motion.
fn unsettled_hatch(rounded: bool, color: u32) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            // The pattern is measured in device pixels.
            let scale = window.scale_factor();
            let stripes = gpui::pattern_slash(rgba(color).into(), 2.0 * scale, 4.0 * scale);
            let mut quad = gpui::fill(bounds, stripes);
            if rounded {
                // The bar's corner inside its 1 px border.
                quad = quad.corner_radii(px(5.0));
            }
            window.paint_quad(quad);
        },
    )
    .absolute()
    .inset_0()
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
    timeline_transitions: &HashMap<String, TimelineTransition>,
    fit_width: f32,
    active: bool,
) -> impl IntoElement {
    use crate::lane::{LABEL_WIDTH, bar_row_geometry, timeline_scale};
    const RULER_H: f32 = 32.0;
    let duration = timeline_duration(data);
    let zoom = data.zoom();
    let (px_per_sec, timeline_w) = timeline_scale(duration, zoom, fit_width);
    let tick_times = ruler_ticks(
        duration,
        px_per_sec,
        data.ruler_timecode.map_or(0., |clock| clock.start),
    );
    let target_poses = (!timeline_transitions.is_empty()).then(|| timeline_poses(data));
    let now = std::time::Instant::now();
    let row_stride = TIMELINE_ROW_H + 1.;
    // Bars more than a screen away from the visible part keep their element,
    // so nothing fades in again, but not their waveform, icon, name or
    // tooltip, which are built again once they come near.
    let view_width = f32::from(data.timeline_scroll.bounds().size.width);
    let view_left = -f32::from(data.timeline_scroll.offset().x);
    let near_view = |left: f32, width: f32| {
        view_width <= 0.
            || (left + width >= view_left - view_width && left <= view_left + 2. * view_width)
    };

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
            Some(clock) => fmt_timecode(clock, at),
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
    // One canvas draws the gridlines of every row at the ruler ticks, under
    // the bars, in the color of the row separators it crosses.
    let grid_color = rgb(theme.separator);
    let grid_x: Vec<f32> = tick_times
        .iter()
        .map(|&at| (at * px_per_sec) as f32)
        .collect();
    tracks = tracks.relative().child(
        gpui::canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                for &x in &grid_x {
                    window.paint_quad(gpui::fill(
                        gpui::Bounds::new(
                            bounds.origin + point(px(x), px(0.)),
                            gpui::size(px(1.), bounds.size.height),
                        ),
                        grid_color,
                    ));
                }
            },
        )
        .absolute()
        .left(px(0.))
        .top(px(RULER_H))
        .w(px(timeline_w))
        .h(px(data.lanes.len() as f32 * row_stride)),
    );

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
            let geometry = bar_row_geometry(&lane.clips, px_per_sec);
            for (bar, (x, width)) in lane.clips.iter().zip(geometry.iter()) {
                let (x, width) = (*x, *width);
                let palette = theme.clip;
                let color = palette.fill(bar.match_state);
                let ink = palette.ink;
                // Where the palette asks for it, the fill is laid again
                // behind the name and status mark, muting the waveform and
                // stripes there so the ink stays readable.
                let backdrop = palette.backdrop(bar.match_state).map(rgba);
                let clip_id = bar.clip_id.clone();
                // A bar that moved to this layout glides from where it was.
                // Its last frame is this layout, so it is drawn the same way
                // once the move is over.
                let (left, shown_width, top) = timeline_transitions
                    .get(&bar.id)
                    .filter(|transition| {
                        target_poses
                            .as_ref()
                            .and_then(|poses| poses.get(&bar.id))
                            .is_some_and(|pose| !poses_differ(transition.to, *pose))
                    })
                    .map_or((x, width, 0.), |transition| {
                        let progress = transition.progress(now);
                        let from_x = transition.from.x * timeline_w;
                        let from_width = (transition.from.width * timeline_w).max(2.);
                        (
                            from_x + (x - from_x) * progress,
                            from_width + (width - from_width) * progress,
                            (transition.from.lane - lane_index as f32)
                                * row_stride
                                * (1. - progress),
                        )
                    });
                // Keep narrow slivers square; cap other corner radii at 4 px.
                let mut el = div()
                    .id(SharedString::from(format!("bar-{}", bar.id)))
                    .absolute()
                    .left(px(left))
                    .top(px(6.0 + top))
                    .w(px(shown_width))
                    .h(px(TIMELINE_ROW_H - 12.0));
                let rounded = width >= 8.0;
                if rounded {
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
                let near = near_view(left, shown_width);
                if active && near {
                    let mut tip = format!("{} · {}", bar.url.display(), fmt_duration(bar.duration));
                    if let Some(mark) = &bar.camera_clock {
                        tip += &format!(
                            "\nApproximately placed by camera clock between {} and {}, within about ±{:.1} s. Not synchronized.",
                            data.clip_name(&mark.before),
                            data.clip_name(&mark.after),
                            mark.tolerance
                        );
                    }
                    el = el.tooltip(hover_tip(tip, theme));
                }
                // A clip the analysis has not settled is hatched at any
                // width, under its waveform.
                if near && bar.match_state.unsettled() {
                    el = el.child(unsettled_hatch(rounded, palette.hatch));
                }
                // Icon + name when wide enough.
                // Bar label: icon pinned top-left, text pinned to the
                // remaining width. Absolute blocks (not flex items) hold
                // exact bounds, so long names shape a real "…" instead of
                // hard-clipping past the bar edge. `el` itself is
                // absolutely positioned, hence a valid anchor.
                if width >= 24.0 && near {
                    if let Some(waveform) = data.waveform_previews.get(&bar.clip_id) {
                        let (columns, bar_width) = waveform_columns(waveform, width);
                        let wave = rgba(palette.wave);
                        let preview = gpui::canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                let radius = window.rem_size() * 0.125;
                                for (index, amplitude) in columns.into_iter().enumerate() {
                                    let h = 3.0 + amplitude * (TIMELINE_ROW_H - 21.0);
                                    let origin = bounds.origin
                                        + point(
                                            px(index as f32 * bar_width),
                                            px((TIMELINE_ROW_H - 12.0 - h) * 0.5),
                                        );
                                    let rect = gpui::Bounds::new(
                                        origin,
                                        gpui::size(px((bar_width - 1.0).max(1.0)), px(h)),
                                    );
                                    window.paint_quad(gpui::fill(rect, wave).corner_radii(radius));
                                }
                            },
                        )
                        .absolute()
                        .inset_0();
                        el = el.child(preview);
                    }
                    if width >= 64.0 {
                        // A backdrop starts under the media icon and ends
                        // just past the name; the name keeps its place and
                        // the width it may take before it is cut short.
                        el = el.child(
                            div()
                                .absolute()
                                .left(px(5.0))
                                .right(px(2.0))
                                .top(px(0.))
                                .bottom(px(0.))
                                .flex()
                                .flex_col()
                                .justify_center()
                                .items_start()
                                .child(
                                    div()
                                        .max_w_full()
                                        .pl(px(22.0))
                                        .pr(px(2.0))
                                        .when_some(backdrop, |name, backdrop| {
                                            name.rounded(px(4.0)).bg(backdrop)
                                        })
                                        .text_xs()
                                        .truncate()
                                        .whitespace_nowrap()
                                        .text_color(rgb(ink))
                                        .child(bar.name.clone()),
                                ),
                        );
                    }
                    let video = matches!(bar.kind, MediaKind::Video);
                    let icon_path = if video {
                        "icons/film.svg"
                    } else {
                        "icons/waveform.svg"
                    };
                    el = el.child(
                        div()
                            .absolute()
                            .left(px(7.0))
                            .top(px(17.0))
                            .child(svg_icon(icon_path, 13.0, ink)),
                    );
                }
                // The status mark sits apart from the media icon, in the
                // corner, or centered across a bar too narrow for the
                // corner's gap (inside its 1 px borders).
                let status_right = ((width - 2.0 - STATUS_ICON) / 2.0).min(STATUS_INSET);
                if near
                    && status_right >= 1.0
                    && let Some(icon) = clip_status_icon(bar.match_state)
                {
                    el = el.child(
                        div()
                            .absolute()
                            .top(px(STATUS_INSET))
                            .right(px(status_right))
                            .size(px(STATUS_ICON))
                            .when_some(backdrop, |mark, backdrop| {
                                mark.rounded(px(2.0)).bg(backdrop)
                            })
                            .child(svg_icon(icon, STATUS_ICON, ink)),
                    );
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
                // The same element every frame, moving or not, so a bar fades
                // in only when it first appears.
                track = track.child(el.with_animation(
                    SharedString::from(format!("timeline-clip-enter-{}", bar.id)),
                    entrance(220),
                    |el, progress| el.opacity(progress),
                ));
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
