//! Motion curves and enter/exit animations shared by the views.

use super::settings::QUALITY_MENU_WIDTH;
use super::{
    Animation, AnimationExt, DROPDOWN_MS, Duration, ExportSelect, IntoElement, ParentElement,
    SELECTION_CONTROL_CLOSE_MS, SETTINGS_MOTION_MS, SIDEBAR_CLOSE_MS, SettingsSelect, SharedString,
    Styled, div, px,
};

/// Strong UI ease-out: cubic-bezier(0.23, 1, 0.32, 1).
/// Solve the time coordinate before evaluating opacity; input is not the
/// Bezier parameter. Opacity-only motion is also the reduced-motion variant.
pub(super) fn motion_ease(time: f32) -> f32 {
    let time = time.clamp(0.0, 1.0);
    if time == 0.0 || time == 1.0 {
        return time;
    }
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let x = 3.0 * (1.0 - t).powi(2) * t * 0.23 + 3.0 * (1.0 - t) * t * t * 0.32 + t * t * t;
        if x < time {
            low = t;
        } else {
            high = t;
        }
    }
    1.0 - (1.0 - (low + high) * 0.5).powi(3)
}

pub(super) fn entrance(duration_ms: u64) -> Animation {
    Animation::new(Duration::from_millis(duration_ms)).with_easing(motion_ease)
}

pub(super) fn slide_in<E: IntoElement + Styled + 'static>(
    child: E,
    id: impl Into<gpui::ElementId>,
    delay_ms: u64,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    crate::motion::Slide {
        child: Some(child),
        x: 0.,
        y: 0.,
    }
    .with_animation(
        id,
        Animation::new(Duration::from_millis(280 + delay_ms)),
        move |mut el, elapsed| {
            let progress =
                ((elapsed * (280 + delay_ms) as f32 - delay_ms as f32) / 280.).clamp(0., 1.);
            let eased = motion_ease(progress);
            el.y = if reduced { 0. } else { 24. * (1. - eased) };
            el.child = el.child.map(|child| child.opacity(eased));
            el
        },
    )
}

pub(super) enum SidebarEdge {
    Left,
    Right,
}

pub(super) fn sidebar_reveal<E: IntoElement + Styled + 'static>(
    child: E,
    id: impl Into<gpui::ElementId>,
    width: f32,
    edge: SidebarEdge,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    // Anchor the full panel to the inner edge of the clipped viewport.
    // With the left panel, shrinking the viewport moves the panel left.
    let panel = div()
        .absolute()
        .top(px(0.))
        .w(px(width))
        .h_full()
        .child(child);
    let panel = match edge {
        SidebarEdge::Left => panel.right(px(0.)),
        SidebarEdge::Right => panel.left(px(0.)),
    };
    div()
        .relative()
        .h_full()
        .flex_shrink_0()
        .overflow_hidden()
        // Put the shadow on the animated viewport. A shadow on the panel
        // itself is clipped by this viewport and never reaches the timeline.
        .shadow_md()
        .child(panel)
        .with_animation(
            id,
            entrance(if closing { SIDEBAR_CLOSE_MS } else { 280 }),
            move |el, progress| {
                let visible = if closing { 1. - progress } else { progress };
                let visible = if reduced {
                    if closing { 0. } else { 1. }
                } else {
                    visible
                };
                el.w(px(width * visible))
            },
        )
}

pub(super) fn quality_dropdown_motion<E: IntoElement + Styled + 'static>(
    child: E,
    closing: bool,
) -> impl IntoElement {
    // Padding and border 10, five 40 px rows, separator block 37.
    const MENU_HEIGHT: f32 = 248.;
    let reduced = crate::motion::reduced_motion();
    div()
        .w(px(QUALITY_MENU_WIDTH))
        .overflow_hidden()
        .rounded_lg()
        .shadow_md()
        .child(child)
        .with_animation(
            if closing {
                "search-quality-dropdown-out"
            } else {
                "search-quality-dropdown-in"
            },
            entrance(DROPDOWN_MS),
            move |el, progress| {
                let visible = if reduced {
                    if closing { 1. - progress } else { progress }
                } else if closing {
                    1. - progress
                } else {
                    progress
                };
                let height = if reduced { 1. } else { visible };
                el.h(px(MENU_HEIGHT * height)).opacity(visible)
            },
        )
}

pub(super) fn export_select_motion<E: IntoElement + Styled + 'static>(
    child: E,
    select: ExportSelect,
    menu_height: f32,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    let animation_id = match (select, closing) {
        (ExportSelect::AafFrameRate, false) => "export-aaf-select-in",
        (ExportSelect::AafFrameRate, true) => "export-aaf-select-out",
        (ExportSelect::UnmatchedPlacement, false) => "export-unmatched-select-in",
        (ExportSelect::UnmatchedPlacement, true) => "export-unmatched-select-out",
    };
    div()
        .w(px(160.))
        .overflow_hidden()
        .rounded_lg()
        .shadow_md()
        .child(child)
        .with_animation(animation_id, entrance(DROPDOWN_MS), move |el, progress| {
            let visible = if closing { 1. - progress } else { progress };
            let height = if reduced { 1. } else { visible };
            el.h(px(menu_height * height)).opacity(visible)
        })
}

pub(super) fn settings_select_motion<E: IntoElement + Styled + 'static>(
    child: E,
    select: SettingsSelect,
    menu_height: f32,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    div()
        .w(px(210.))
        .overflow_hidden()
        .rounded_lg()
        .shadow_md()
        .child(child)
        .with_animation(
            SharedString::from(format!(
                "settings-{}-{}",
                select.id(),
                if closing { "out" } else { "in" }
            )),
            entrance(SETTINGS_MOTION_MS),
            move |el, progress| {
                let visible = if closing { 1. - progress } else { progress };
                let height = if reduced { 1. } else { visible };
                el.h(px(menu_height * height)).opacity(visible)
            },
        )
}

pub(super) fn settings_reset_motion<E: IntoElement + Styled + 'static>(
    child: E,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    div()
        .h(px(30.))
        .overflow_hidden()
        .child(child)
        .with_animation(
            if closing {
                "settings-reset-out"
            } else {
                "settings-reset-in"
            },
            entrance(SETTINGS_MOTION_MS),
            move |el, progress| {
                let visible = if closing { 1. - progress } else { progress };
                let width = if reduced { 1. } else { visible };
                el.w(px(34. * width)).opacity(visible)
            },
        )
}

pub(super) fn quality_shift_for_export<E: IntoElement + Styled + 'static>(
    child: E,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    crate::motion::Slide {
        child: Some(child),
        x: 68.,
        y: 0.,
    }
    .with_animation(
        "timeline-quality-shift",
        entrance(200),
        move |mut el, progress| {
            el.x = if reduced { 0. } else { 68. * (1. - progress) };
            el
        },
    )
}

pub(super) fn export_button_entrance<E: IntoElement + 'static>(child: E) -> impl IntoElement {
    div().child(child).with_animation(
        "timeline-export-button-in",
        entrance(200),
        |el, progress| el.opacity(progress),
    )
}

pub(super) fn toolbar_entrance<E: IntoElement + 'static>(
    child: E,
    id: &'static str,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    div()
        .flex_shrink_0()
        .overflow_hidden()
        .child(child)
        .with_animation(id, entrance(240), move |el, progress| {
            let height = if reduced { 1. } else { progress };
            el.h(px(48. * height)).opacity(progress)
        })
}

pub(super) fn export_reveal<E: IntoElement + 'static>(
    child: E,
    id: &'static str,
    max_height: f32,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    div()
        .w_full()
        .overflow_hidden()
        .child(child)
        .with_animation(
            (id, usize::from(closing)),
            entrance(SETTINGS_MOTION_MS),
            move |el, progress| {
                let visible = if closing { 1. - progress } else { progress };
                let height = if reduced { 1. } else { visible };
                el.max_h(px(max_height * height)).opacity(visible)
            },
        )
}

pub(super) fn selection_control_motion<E: IntoElement + Styled + 'static>(
    child: E,
    id: SharedString,
    closing: bool,
) -> impl IntoElement {
    let reduced = crate::motion::reduced_motion();
    div()
        .h(px(16.))
        .flex_shrink_0()
        .overflow_hidden()
        .child(child)
        .with_animation(
            id,
            entrance(if closing {
                SELECTION_CONTROL_CLOSE_MS
            } else {
                160
            }),
            move |el, progress| {
                let visible = if reduced {
                    if closing { 0. } else { 1. }
                } else if closing {
                    1. - progress
                } else {
                    progress
                };
                el.w(px(28. * visible)).opacity(visible)
            },
        )
}

#[cfg(test)]
mod motion_tests {
    use super::*;
    use crate::views::controls::{ruler_ticks, waveform_columns};
    use crate::views::{
        AppData, ExportReveals, ExportTarget, Instant, PROGRESS_MOVE_MS, ProgressMotion,
        TIMELINE_MOVE_MS, TimelinePose, TimelineTransition, middle_ellipsis, poses_differ,
    };

    #[test]
    fn entrance_is_bounded_monotonic_and_settles() {
        assert_eq!(motion_ease(0.0), 0.0);
        assert_eq!(motion_ease(1.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let opacity = motion_ease(step as f32 / 100.0);
            assert!((previous..=1.0).contains(&opacity));
            previous = opacity;
        }
        assert!(motion_ease(0.5) > 0.9);
        assert!(entrance(200).oneshot);
    }

    #[test]
    fn export_reveals_follow_selected_formats_and_parent_options() {
        let mut data = AppData::default();
        let initial = ExportReveals::new(&data);
        assert!(!initial.final_cut);
        assert!(!initial.premiere);
        assert!(!initial.aaf);
        assert!(!initial.synced_labels);

        data.export.targets.extend([
            ExportTarget::FinalCutPro,
            ExportTarget::Premiere,
            ExportTarget::Aaf,
        ]);
        data.export.label_synced = true;
        let selected = ExportReveals::new(&data);
        assert!(selected.final_cut && selected.storylines);
        assert!(selected.premiere && selected.aaf);
        assert!(selected.synced_xml && selected.synced_final_cut);

        data.export.fcpxml_timeline = false;
        data.export.label_synced = false;
        let disabled = ExportReveals::new(&data);
        assert!(!disabled.storylines);
        assert!(!disabled.synced_xml && !disabled.synced_final_cut);
    }

    #[test]
    fn ruler_timecode_counts_the_timeline_frames() {
        use crate::lane::RulerClock;
        use crate::views::controls::fmt_timecode;
        let pal = RulerClock {
            start: 0.,
            frame_duration: 1. / 25.,
        };
        // A tick on a whole second just short of it in binary arithmetic.
        assert_eq!(fmt_timecode(pal, 360. - 1e-9), "00:06:00:00");
        let thirty = RulerClock {
            start: 10.,
            frame_duration: 1. / 30.,
        };
        assert_eq!(fmt_timecode(thirty, 0.5), "00:00:10:15");
    }

    #[test]
    fn every_edit_of_one_source_has_its_own_timeline_pose() {
        use crate::lane::{BarMatchState, BarVisual, LaneVisual};
        let edit = |id: &str, start: f64| BarVisual {
            id: id.into(),
            clip_id: align_core::ClipId::new("same-file"),
            url: "/v/take.mov".into(),
            source_key: "/v".into(),
            name: "take.mov".into(),
            kind: align_core::MediaKind::Video,
            start,
            duration: 2.,
            confidence: 1.,
            match_state: BarMatchState::Matched,
            evidence: None,
            source_in: 0.0,
            source_out: 0.0,
            camera_clock: None,
        };
        let mut data = AppData::default();
        data.lanes = vec![LaneVisual {
            id: "V1".into(),
            kind: align_core::MediaKind::Video,
            number: 1,
            source_key: "/v".into(),
            source_name: String::new(),
            stream_channels: Vec::new(),
            clips: vec![edit("imported-video-1", 0.), edit("imported-video-2", 4.)],
        }];
        let poses = crate::views::timeline_poses(&data);
        assert_eq!(poses.len(), 2);
        assert!(poses_differ(
            poses["imported-video-1"],
            poses["imported-video-2"]
        ));
    }

    #[test]
    fn timeline_transition_reaches_the_new_position_and_size() {
        let started = Instant::now();
        let from = TimelinePose {
            x: 0.8,
            width: 0.15,
            lane: 2.,
        };
        let to = TimelinePose {
            x: 0.2,
            width: 0.3,
            lane: 0.,
        };
        let transition = TimelineTransition { from, to, started };
        assert!(!poses_differ(transition.presented(started), from));
        let end = started + Duration::from_millis(TIMELINE_MOVE_MS);
        assert!(!poses_differ(transition.presented(end), to));
        // A finished move is dropped; its bar is then drawn at `to`.
        assert!(!transition.finished(started) && transition.finished(end));
    }

    #[test]
    fn progress_glides_forward_continuously_and_snaps_back() {
        let start = Instant::now();
        let mut motion = ProgressMotion::new();
        motion.track(0.4, start);
        assert_eq!(motion.presented(start), 0.);
        let mid = start + Duration::from_millis(PROGRESS_MOVE_MS / 2);
        let shown = motion.presented(mid);
        assert!(shown > 0. && shown < 0.4);
        // Retargeting mid-flight continues from the shown width.
        motion.track(0.9, mid);
        assert!((motion.presented(mid) - shown).abs() < 1e-6);
        let end = mid + Duration::from_millis(PROGRESS_MOVE_MS);
        assert!((motion.presented(end) - 0.9).abs() < 1e-6);
        // A new operation restarts at its own value without animating back.
        let generation = motion.generation;
        motion.track(0.05, end);
        assert_eq!(motion.presented(end), 0.05);
        assert_eq!(motion.generation, generation + 1);
        // Repeated identical reports do not restart the animation.
        motion.track(0.05, end);
        assert_eq!(motion.generation, generation + 1);
    }

    #[test]
    fn ruler_ticks_land_on_round_clock_values() {
        // 240 s across 880 px: 30 s steps.
        let ticks = ruler_ticks(240., 880. / 240., 0.);
        assert_eq!(ticks.first(), Some(&0.));
        assert!(ticks.windows(2).all(|pair| pair[1] - pair[0] == 30.));
        assert_eq!(ticks.last(), Some(&240.));
        // Timecode 01:00:07 origin: ticks sit on whole 10 s timecodes.
        let ticks = ruler_ticks(60., 12., 3607.);
        assert_eq!(ticks[0], 3.);
        assert!(ticks.iter().all(|t| ((t + 3607.) % 10.).abs() < 1e-9));
        // Zoomed far in never goes below one-second labels.
        assert!(
            ruler_ticks(10., 10_000., 0.)
                .windows(2)
                .all(|p| p[1] - p[0] == 1.)
        );
        assert!(ruler_ticks(0., 10., 0.).is_empty());
    }

    #[test]
    fn long_destination_paths_keep_both_ends() {
        assert_eq!(
            middle_ellipsis("/projects/client/episode/final-deliverables", 21),
            "/projects/…liverables"
        );
        assert_eq!(middle_ellipsis("brief", 21), "brief");
    }

    #[test]
    fn settled_clip_states_carry_distinct_embedded_marks() {
        // Each settled state reads without color; an unsettled one is
        // hatched instead, so it never shows the synchronized link.
        use crate::lane::BarMatchState;
        use crate::views::controls::clip_status_icon;
        use gpui::AssetSource as _;
        let settled = [
            BarMatchState::Matched,
            BarMatchState::Unmatched,
            BarMatchState::CameraClock,
        ];
        let marks: std::collections::HashSet<&str> = settled
            .iter()
            .map(|state| clip_status_icon(*state).expect("settled clips carry a mark"))
            .collect();
        assert_eq!(marks.len(), settled.len());
        for mark in &marks {
            assert!(
                crate::icons::EmbeddedAssets
                    .load(mark)
                    .is_ok_and(|bytes| bytes.is_some()),
                "{mark} is not embedded"
            );
            // The media-kind icon sits beside it and must not repeat.
            assert!(!["icons/film.svg", "icons/waveform.svg"].contains(mark));
        }
        for state in [BarMatchState::Pending, BarMatchState::Refined] {
            assert!(state.unsettled() && clip_status_icon(state).is_none());
        }
    }

    #[test]
    fn waveform_columns_fill_a_non_divisible_clip_width() {
        let waveform: Vec<f32> = (0..512).map(|value| value as f32 / 511.0).collect();
        let width = 748.0;
        let (columns, column_width) = waveform_columns(&waveform, width);
        assert!((columns.len() as f32 * column_width - width).abs() < 0.01);
        assert!(columns.last().is_some_and(|value| *value > 0.99));
    }
}
