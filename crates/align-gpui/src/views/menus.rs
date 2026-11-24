//! Diagnostics, sequence picker, and context menus.

use super::controls::{button, modal_header, prominent_button};
use super::settings::{
    audio_source_label, clip_order_label, match_threshold_label, track_content_label,
};
use super::{
    AlignApp, AppData, AudioAnalysisSource, ClickEvent, ClipOrder, Context, Div, HashMap,
    InteractiveElement, IntoElement, MatchThreshold, MenuTarget, MouseButton, ParentElement,
    SequencePicker, SharedString, Stateful, StatefulInteractiveElement, Styled, TemporalMode,
    Theme, TrackContent, Window, div, px, rgb,
};

// ---------------- diagnostics popover

// ---------------- sequence picker

pub(super) fn sequence_picker_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    picker: SequencePicker,
) -> impl IntoElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_5()
        .m_2()
        .max_w(px(420.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Choose a sequence",
            "seq-cancel",
            |this, _, _, cx| {
                this.data.sequence_pickers.pop_front();
                cx.notify();
            },
        ))
        .child(div().text_color(rgb(theme.dim)).child(format!(
                "{} contains multiple timelines.",
                picker
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("timeline")
            )));
    panel = panel.child(prominent_button(
        cx,
        theme,
        "seq-all",
        format!("Import all {} sequences", picker.options.len()),
        true,
        |this, _, _, cx| {
            this.data.choose_all_sequences();
            this.project_changed(cx);
            cx.notify();
        },
    ));
    for option in picker.options {
        let label = format!("{} — {} clips", option.name, option.clip_count);
        let index = option.index;
        panel = panel.child(button(
            cx,
            theme,
            format!("seq-{index}"),
            label,
            true,
            move |this, _, _, cx| {
                this.data.choose_sequence(index);
                this.project_changed(cx);
                cx.notify();
            },
        ));
    }
    panel
}

// ---------------- context menu (anchored popup, mirrors clip/track menus)

/// Transparent click-outside layer that dismisses the context menu.
/// The closing click is swallowed so it never activates the element
/// below the menu.
pub(super) fn menu_dismiss_layer(cx: &mut Context<AlignApp>) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.data.menu = None;
                cx.notify();
                cx.stop_propagation();
            }),
        )
}

/// Borderless menu row (compact, unlike dialog buttons).
pub(super) fn menu_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    danger: bool,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .flex()
        .px_3()
        .h(px(28.))
        .flex_shrink_0()
        .items_center()
        .rounded_md()
        .text_size(px(12.))
        .truncate()
        .whitespace_nowrap()
        .text_color(rgb(if danger { theme.danger } else { theme.text }))
        .cursor_pointer()
        .hover(|this| this.bg(rgb(theme.button_hover)))
        .on_click(cx.listener(move |this, e, window, cx| action(this, e, window, cx)))
        .child(label.into())
}

pub(super) fn menu_header(theme: &Theme, label: String) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_3()
        .pt_2()
        .text_xs()
        .text_color(rgb(theme.dim))
        .truncate()
        .whitespace_nowrap()
        .child(label)
}

pub(super) fn context_menu(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: MenuTarget,
) -> impl IntoElement {
    let mut panel = div()
        .id("ctx-menu")
        .flex()
        .flex_col()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .occlude()
        .min_w(px(280.))
        .max_w(px(420.))
        .max_h(px(380.))
        .overflow_y_scroll();
    // Lane right-click edits the whole track's analysis source; clip
    // right-click only offers pair corrections for that clip.
    if menu.stream_menu {
        return stream_menu_section(cx, theme, data, &menu, panel);
    }
    panel = camera_clock_section(cx, theme, data, &menu.clip_id, panel);
    let options = data.correction_options_for(&menu.clip_id);
    if options.is_empty() {
        panel = panel.child(menu_header(theme, "No other alignments found".to_string()));
    }
    for option in &options {
        panel = panel.child(menu_header(theme, option.other_name.clone()));
        let target = option.clone();
        panel = panel.child(menu_row(
            cx,
            theme,
            format!("fix-another-{}", target.id),
            "Find Another Alignment",
            false,
            move |this, _, _, cx| {
                let target = target.clone();
                this.find_another(&target, cx);
            },
        ));
        let target = option.clone();
        let label = format!("Reject Pair with {}", target.other_name);
        panel = panel.child(menu_row(
            cx,
            theme,
            format!("fix-reject-{}", target.id),
            label,
            true,
            move |this, _, _, cx| {
                let target = target.clone();
                this.reject_pair(&target, cx);
            },
        ));
    }
    panel
}

/// Camera-clock placement of one unmatched clip: apply or undo its
/// estimate, or the reason it has none.
fn camera_clock_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    clip: &align_core::ClipId,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let name = |id: &align_core::ClipId| data.clip_name(id);
    if let Some(estimate) = data.camera_clock.estimate(clip) {
        panel = panel.child(menu_header(
            theme,
            format!(
                "Camera clock · between {} and {} · ±{:.1} s",
                name(&estimate.before),
                name(&estimate.after),
                estimate.tolerance
            ),
        ));
        let imported = data
            .result
            .as_ref()
            .is_some_and(|result| result.project.imported_timeline.is_some());
        if imported {
            panel = panel.child(menu_note(theme, "Imported edits keep their placement."));
        } else {
            let applied = data.camera_clock_applied(clip);
            let target = clip.clone();
            panel = panel.child(menu_row(
                cx,
                theme,
                "camera-clock-toggle",
                if applied {
                    "Return to Unmatched"
                } else {
                    "Place Approximately by Camera Clock"
                },
                false,
                move |this, _, _, cx| {
                    this.data.menu = None;
                    this.set_camera_clock(vec![target.clone()], !applied, cx);
                },
            ));
        }
    } else if let Some(refusal) = data.camera_clock.refusal(clip) {
        panel = panel.child(menu_header(theme, "Camera clock".to_string()));
        panel = panel.child(menu_note(theme, refusal.describe(&name)));
    }
    panel
}

/// Dim wrapped text under a menu header.
fn menu_note(theme: &Theme, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_3()
        .pb_1()
        .text_xs()
        .text_color(rgb(theme.dim))
        .child(text.into())
}

pub(super) fn stream_menu_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    // Stream/channel counts of the lane's clips (mirror the track menu).
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let current = menu
        .lane_id
        .as_deref()
        .and_then(|lane_id| data.lane_analysis_source(lane_id));
    let label = move |source, title: String| {
        if current == Some(source) {
            format!("✓ {title}")
        } else {
            title
        }
    };
    if let Some(lane_id) = &menu.lane_id
        && let Some(lane) = data.lanes.iter().find(|l| &l.id == lane_id)
    {
        for (index, channels) in lane.stream_channels.iter().copied().enumerate() {
            counts.insert(index, channels);
        }
    }
    if counts.is_empty() {
        counts = data
            .audio_stream_channels
            .get(&menu.clip_id)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, n)| (i, *n))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
    }
    let mut indices: Vec<usize> = counts.keys().copied().collect();
    indices.sort_unstable();
    panel = panel.child(menu_header(theme, "Analysis source".to_string()));
    let inherited = data.current_effective_settings().audio_source;
    panel = panel.child(menu_row(
        cx,
        theme,
        "stream-inherit",
        if current.is_none() {
            format!("✓ Inherit ({})", audio_source_label(inherited))
        } else {
            format!("Inherit ({})", audio_source_label(inherited))
        },
        false,
        |this, _, _, cx| this.set_lane_option(AppData::set_analysis_source, None, cx),
    ));
    panel = panel.child(menu_row(
        cx,
        theme,
        "stream-auto",
        label(AudioAnalysisSource::Automatic, "Automatic".to_string()),
        false,
        |this, _, _, cx| {
            this.set_lane_option(
                AppData::set_analysis_source,
                Some(AudioAnalysisSource::Automatic),
                cx,
            )
        },
    ));
    if counts.len() > 1 {
        panel = panel.child(menu_row(
            cx,
            theme,
            "all-streams-mix",
            label(
                AudioAnalysisSource::AllMixed,
                "Mix all audio streams".to_string(),
            ),
            false,
            |this, _, _, cx| {
                this.set_lane_option(
                    AppData::set_analysis_source,
                    Some(AudioAnalysisSource::AllMixed),
                    cx,
                )
            },
        ));
    }
    for index in indices {
        let channels = counts.get(&index).copied().unwrap_or(0);
        if counts.len() == 1 && channels <= 1 {
            continue;
        }
        if counts.len() == 1 {
            panel = panel.child(menu_row(
                cx,
                theme,
                "stream-mix",
                label(
                    AudioAnalysisSource::MixedStream(index),
                    "Mix all channels".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_lane_option(
                        AppData::set_analysis_source,
                        Some(AudioAnalysisSource::MixedStream(index)),
                        cx,
                    )
                },
            ));
            for ch in 0..channels {
                panel = panel.child(menu_row(
                    cx,
                    theme,
                    format!("stream-ch-{ch}"),
                    label(
                        AudioAnalysisSource::Channel(ch),
                        format!("Use channel {}", ch + 1),
                    ),
                    false,
                    move |this, _, _, cx| {
                        this.set_lane_option(
                            AppData::set_analysis_source,
                            Some(AudioAnalysisSource::Channel(ch)),
                            cx,
                        )
                    },
                ));
            }
        } else if channels <= 1 {
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}"),
                label(
                    AudioAnalysisSource::Stream {
                        index,
                        channel: None,
                    },
                    format!("Audio stream {}", index + 1),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_lane_option(
                        AppData::set_analysis_source,
                        Some(AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        }),
                        cx,
                    )
                },
            ));
        } else {
            panel = panel.child(menu_header(theme, format!("Audio stream {}", index + 1)));
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}-mix"),
                label(
                    AudioAnalysisSource::MixedStream(index),
                    "Mix channels".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_lane_option(
                        AppData::set_analysis_source,
                        Some(AudioAnalysisSource::MixedStream(index)),
                        cx,
                    )
                },
            ));
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("stream-{index}-auto"),
                label(
                    AudioAnalysisSource::Stream {
                        index,
                        channel: None,
                    },
                    "Automatic channel".to_string(),
                ),
                false,
                move |this, _, _, cx| {
                    this.set_lane_option(
                        AppData::set_analysis_source,
                        Some(AudioAnalysisSource::Stream {
                            index,
                            channel: None,
                        }),
                        cx,
                    )
                },
            ));
            for ch in 0..channels {
                panel = panel.child(menu_row(
                    cx,
                    theme,
                    format!("stream-{index}-ch-{ch}"),
                    label(
                        AudioAnalysisSource::Stream {
                            index,
                            channel: Some(ch),
                        },
                        format!("Use channel {}", ch + 1),
                    ),
                    false,
                    move |this, _, _, cx| {
                        this.set_lane_option(
                            AppData::set_analysis_source,
                            Some(AudioAnalysisSource::Stream {
                                index,
                                channel: Some(ch),
                            }),
                            cx,
                        )
                    },
                ));
            }
        }
    }
    panel = track_content_section(cx, theme, data, menu, panel);
    panel = preserve_editing_section(cx, theme, data, menu, panel);
    if let Some(lane_id) = &menu.lane_id {
        let current = data.lane_search_accuracy(lane_id);
        panel = panel.child(menu_header(theme, "Search accuracy".to_string()));
        for (index, accuracy) in std::iter::once(None)
            .chain(align_core::SearchAccuracy::ALL.into_iter().map(Some))
            .enumerate()
        {
            let title = accuracy
                .map(|value| value.label().to_string())
                .unwrap_or_else(|| {
                    format!(
                        "Inherit ({})",
                        data.current_effective_settings().search_accuracy.label()
                    )
                });
            let title = if current == accuracy {
                format!("✓ {title}")
            } else {
                title
            };
            let lane_id = lane_id.clone();
            panel = panel.child(menu_row(
                cx,
                theme,
                format!("lane-search-{index}"),
                title,
                false,
                move |this, _, _, cx| {
                    if this.data.set_lane_search_accuracy(accuracy, &lane_id) {
                        this.data.menu = None;
                        this.data.mark_sync_dirty();
                        this.project_changed(cx);
                        this.start_sync(cx);
                    }
                },
            ));
        }
    }
    panel = clip_order_section(cx, theme, data, menu, panel);
    panel = time_source_section(cx, theme, data, menu, panel);
    panel = match_threshold_section(cx, theme, data, menu, panel);
    panel
}

pub(super) fn preserve_editing_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let Some(lane_id) = menu.lane_id.as_deref() else {
        return panel;
    };
    if !data.lane_can_preserve_editing(lane_id) {
        return panel;
    }
    panel = panel.child(menu_header(theme, "Extra options".to_string()));
    let selected = data.lane_preserves_editing(lane_id);
    panel.child(menu_row(
        cx,
        theme,
        "preserve-basic-editing",
        if selected {
            "✓ Preserve basic editing".to_string()
        } else {
            "Preserve basic editing".to_string()
        },
        false,
        move |this, _, _, cx| this.set_lane_option(AppData::set_preserve_editing, !selected, cx),
    ))
}

pub(super) fn track_content_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_track_content(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Track content".to_string()));
    let inherited = data.current_effective_settings().track_content;
    for (mode, title, row_id) in [
        (
            None,
            format!("Inherit ({})", track_content_label(inherited)),
            "content-inherit",
        ),
        (Some(TrackContent::Auto), "Automatic".into(), "content-auto"),
        (
            Some(TrackContent::Linear),
            "Linear".into(),
            "content-linear",
        ),
        (Some(TrackContent::Takes), "Takes".into(), "content-takes"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_lane_option(AppData::set_track_content, mode, cx),
        ));
    }
    panel
}

pub(super) fn clip_order_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_clip_order(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Clip order".to_string()));
    let inherited = data.current_effective_settings().clip_order;
    for (mode, title, row_id) in [
        (
            None,
            format!("Inherit ({})", clip_order_label(inherited)),
            "order-inherit",
        ),
        (Some(ClipOrder::Auto), "Auto".into(), "order-auto"),
        (
            Some(ClipOrder::AlternateAuto),
            "Alternate Auto".into(),
            "order-alternate-auto",
        ),
        (
            Some(ClipOrder::AsImported),
            "As imported".into(),
            "order-imported",
        ),
        (
            Some(ClipOrder::ByDateTime),
            "Date & time".into(),
            "order-date",
        ),
        (
            Some(ClipOrder::ByFileName),
            "File name".into(),
            "order-name",
        ),
        (Some(ClipOrder::Ignore), "Ignore".into(), "order-ignore"),
    ] {
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_lane_option(AppData::set_clip_order, mode, cx),
        ));
    }
    panel
}

/// Track-level time source: which timestamp
/// evidence this track's clips may use. Compact radio rows; when the
/// selected evidence is missing on every clip the header says so and the
/// engine falls back to stable order without guessing.
pub(super) fn time_source_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    use TemporalMode as M;
    let (effective, available) = menu
        .lane_id
        .as_deref()
        .map(|id| data.temporal_availability(id))
        .unwrap_or((M::Auto, true));
    let header = if available {
        "Time source".to_string()
    } else {
        let missing = match effective {
            M::Timecode => "timecode",
            M::RecStart | M::RecStop => "timestamps",
            M::Auto => "evidence",
        };
        format!("Time source — no {missing}, stable order")
    };
    panel = panel.child(menu_header(theme, header));
    let current = menu
        .lane_id
        .as_deref()
        .and_then(|id| data.lane_temporal_mode(id));
    for mode in [
        None,
        Some(M::Auto),
        Some(M::RecStart),
        Some(M::RecStop),
        Some(M::Timecode),
    ] {
        let title = mode.map_or_else(
            || {
                format!(
                    "Inherit ({})",
                    data.current_effective_settings().temporal_mode.title()
                )
            },
            |mode| mode.title().to_string(),
        );
        let label = if mode == current {
            format!("✓ {title}")
        } else {
            title
        };
        let row_id = match mode {
            None => "time-inherit",
            Some(M::Auto) => "time-auto",
            Some(M::RecStart) => "time-rec-start",
            Some(M::RecStop) => "time-rec-stop",
            Some(M::Timecode) => "time-timecode",
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| {
                this.set_lane_option(AppData::set_temporal_mode, mode, cx);
            },
        ));
    }
    panel
}

pub(super) fn match_threshold_section(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
    menu: &MenuTarget,
    mut panel: Stateful<Div>,
) -> Stateful<Div> {
    let current = menu
        .lane_id
        .as_deref()
        .map(|id| data.lane_match_threshold(id))
        .unwrap_or(None);
    panel = panel.child(menu_header(theme, "Match threshold".to_string()));
    let inherited = data.current_effective_settings().match_threshold;
    for (threshold, title, row_id) in [
        (
            None,
            format!("Inherit ({})", match_threshold_label(inherited)),
            "match-inherit",
        ),
        (
            Some(MatchThreshold::Permissive),
            "More matches".into(),
            "match-more",
        ),
        (
            Some(MatchThreshold::Balanced),
            "Balanced".into(),
            "match-balanced",
        ),
        (
            Some(MatchThreshold::Conservative),
            "Fewer false matches".into(),
            "match-fewer",
        ),
    ] {
        let label = if threshold == current {
            format!("✓ {title}")
        } else {
            title
        };
        panel = panel.child(menu_row(
            cx,
            theme,
            row_id,
            label,
            false,
            move |this, _, _, cx| this.set_lane_option(AppData::set_match_threshold, threshold, cx),
        ));
    }
    panel
}
