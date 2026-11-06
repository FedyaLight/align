use crate::State;
use align_core::{AudioAnalysisSource, MatchThreshold, SearchAccuracy, TemporalMode};
use align_ui::{lane::BarMatchState, state::ExportTarget};
use rune::tok::{self, type_};
use rune::{
    Color, Cx, El, FileDialogOptions, GrowthSpec, ScrollSpec, Shape, Tx, col, el, row, text,
};

fn label(value: impl Into<String>, color: Color) -> El {
    text(value.into()).font(type_::LABEL).color(color)
}

fn button(
    key: impl Into<String>,
    title: impl Into<String>,
    enabled: bool,
    primary: bool,
    dark: bool,
    action: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let t = tok::theme(dark);
    let title = title.into();
    row()
        .key(key.into())
        .role("button")
        .label(title.clone())
        .focusable(true)
        .disabled(!enabled)
        .h(32.0)
        .px(12.0)
        .center()
        .radius(7.0)
        .fill(if primary {
            tok::accent(0, dark)
        } else {
            t.card
        })
        .border(1.0, t.card_border)
        .opacity(if enabled { 1.0 } else { 0.4 })
        .hover(t.hover)
        .on_press_tx(move |s, tx| {
            if enabled {
                action(s, tx);
            }
        })
        .child(label(title, if primary { Color::WHITE } else { t.text }))
}

pub fn add_file(s: &mut State, tx: &mut Tx) {
    if s.busy() {
        return;
    }
    tx.open_file(
        FileDialogOptions {
            title: "Add media or timeline".into(),
            ..Default::default()
        },
        |s: &mut State, answer, _| match answer {
            Ok(Some(path)) if !s.busy() => s.data.add_paths(vec![path]),
            Err(e) => s.data.error = Some(e.to_string()),
            _ => {}
        },
    );
}

fn add_folder(s: &mut State, tx: &mut Tx) {
    if s.busy() {
        return;
    }
    tx.open_folder(
        FileDialogOptions {
            title: "Add media folder".into(),
            ..Default::default()
        },
        |s: &mut State, answer, _| match answer {
            Ok(Some(path)) if !s.busy() => s.data.add_paths(vec![path]),
            Err(e) => s.data.error = Some(e.to_string()),
            _ => {}
        },
    );
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let dark = s.dark;
    let t = tok::theme(dark);
    let busy = s.busy();
    let toolbar = row()
        .h(66.0)
        .px(20.0)
        .gap(8.0)
        .center_y()
        .fill(t.card)
        .child(text("Align").font(type_::TITLE).color(t.text))
        .child(label("RUNE EXPERIMENT", t.text_secondary))
        .child(el().grow(1.0))
        .child(button(
            "add-file",
            "Add file…",
            !busy,
            false,
            dark,
            add_file,
        ))
        .child(button(
            "add-folder",
            "Add folder…",
            !busy,
            false,
            dark,
            add_folder,
        ))
        .child(button(
            "settings",
            "Settings",
            !busy,
            false,
            dark,
            |s, _| s.settings = !s.settings,
        ))
        .child(button(
            "theme",
            if dark { "Light" } else { "Dark" },
            true,
            false,
            dark,
            |s, _| s.dark = !s.dark,
        ))
        .child(button(
            "clear",
            "Clear",
            !busy && !s.data.clips.is_empty(),
            false,
            dark,
            |s, _| s.data.clear(),
        ))
        .child(button(
            "sync",
            if busy { "Cancel" } else { "Synchronize" },
            busy || s.data.can_synchronize(),
            true,
            dark,
            |s, _| {
                if s.busy() {
                    s.cancel();
                } else {
                    s.synchronize();
                }
            },
        ))
        .child(button(
            "export",
            "Export…",
            s.data.can_export(),
            false,
            dark,
            |s, _| s.data.show_export = !s.data.show_export,
        ));
    let sidebar = col()
        .w(300.0)
        .h_full()
        .min_h(0.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(row().h(40.0).px(16.0).center_y().child(label(
            format!("MEDIA · {}", s.data.clips.len()),
            t.text_secondary,
        )))
        .child(
            col()
                .key("media-scroll")
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(
                    col()
                        .key("media-rows")
                        .growth(GrowthSpec::default())
                        .virtual_rows(
                            s.data.clips.len(),
                            64.0,
                            |s: &State, i| s.data.clips[i].url.to_string_lossy(),
                            |s: &State, i| {
                                let t = tok::theme(s.dark);
                                let clip = &s.data.clips[i];
                                let path = clip.url.clone();
                                let selected = s.data.selection.contains(&path);
                                col()
                                    .h(64.0)
                                    .w_full()
                                    .px(16.0)
                                    .py(10.0)
                                    .gap(4.0)
                                    .radius(4.0)
                                    .fill(if selected { t.control_off } else { t.card })
                                    .hover(t.hover)
                                    .on_press(move |s: &mut State| {
                                        if !s.busy() && !s.data.selection.remove(&path) {
                                            s.data.selection.insert(path.clone());
                                        }
                                    })
                                    .child(label(&clip.name, t.text).truncate())
                                    .child(label(
                                        format!(
                                            "{}  {}",
                                            clip.state.status_text(),
                                            clip.duration
                                                .map(|d| format!("{d:.1}s"))
                                                .unwrap_or_default()
                                        ),
                                        t.text_secondary,
                                    ))
                            },
                        ),
                ),
        )
        .child(button(
            "remove",
            "Remove selected",
            !busy && !s.data.selection.is_empty(),
            false,
            dark,
            |s, _| s.data.remove_selection(),
        ));
    let width = cx
        .layout_size("timeline")
        .map_or(820.0, |s| s[0])
        .max(300.0);
    let mut main = col()
        .grow(1.0)
        .min_h(0.0)
        .min_w(0.0)
        .gap(14.0)
        .padding(20.0)
        .child(
            row()
                .h(34.0)
                .center_y()
                .gap(8.0)
                .child(label(
                    format!(
                        "{} groups · {} unmatched · {} tracks",
                        s.data.island_count,
                        s.data.unmatched_count(),
                        s.data.lanes.len()
                    ),
                    t.text_secondary,
                ))
                .child(el().grow(1.0))
                .child(button("zoom-out", "−", true, false, dark, |s, _| {
                    s.data.zoom_level = (s.data.zoom_level - 0.2).max(0.0)
                }))
                .child(label(s.data.zoom_label(), t.text_secondary))
                .child(button("zoom-in", "+", true, false, dark, |s, _| {
                    s.data.zoom_level = (s.data.zoom_level + 0.2).min(1.0)
                }))
                .child(button("fit", "Fit", true, false, dark, |s, tx| {
                    s.data.zoom_level = 0.0;
                    tx.scroll_to_instant("timeline", 0.0, 0.0);
                })),
        );
    if s.settings {
        main = main.child(settings(s));
    }
    if s.data.show_export {
        main = main.child(export_panel(s));
    }
    if let Some(picker) = &s.data.sequence_picker {
        main = main.child(
            col()
                .gap(6.0)
                .child(label("Choose a sequence", t.text))
                .children(picker.options.iter().enumerate().map(|(index, option)| {
                    button(
                        format!("sequence-{index}"),
                        &option.name,
                        true,
                        false,
                        dark,
                        move |s, _| s.data.choose_sequence(index),
                    )
                }))
                .child(button(
                    "all-sequences",
                    "All sequences",
                    true,
                    false,
                    dark,
                    |s, _| s.data.choose_all_sequences(),
                )),
        );
    }
    if s.data.sequence_results.len() > 1 {
        main = main.child(
            row()
                .gap(6.0)
                .children(s.data.sequence_results.iter().enumerate().map(|(i, _)| {
                    button(
                        format!("result-{i}"),
                        format!("Sequence {}", i + 1),
                        true,
                        false,
                        dark,
                        move |s, _| {
                            s.data.select_sequence_result(i);
                        },
                    )
                })),
        );
    }
    main = main.child(timeline(s, width));
    if let Some(error) = &s.data.error {
        main = main
            .child(label(error, rune::oklch(0.7, 0.17, 25.0)))
            .child(button(
                "dismiss-error",
                "Dismiss",
                true,
                false,
                dark,
                |s, _| s.data.dismiss_error(),
            ));
    }
    if !s.data.warnings.is_empty() {
        main = main.child(
            label(
                format!(
                    "{} warnings · {}",
                    s.data.warnings.len(),
                    s.data.warnings[0]
                ),
                t.text_secondary,
            )
            .truncate(),
        );
    }
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .fill(t.bg)
        .child(toolbar)
        .child(row().grow(1.0).min_h(0.0).child(sidebar).child(main))
        .child(
            row()
                .h(36.0)
                .px(20.0)
                .center_y()
                .fill(t.card)
                .child(label(&s.data.status, t.text_secondary).truncate().grow(1.0))
                .child(label(
                    if busy {
                        format!("{:.0}%", s.data.progress * 100.0)
                    } else {
                        "Local processing".into()
                    },
                    t.text_secondary,
                )),
        )
        .on_drop_files(|s: &mut State, paths, _| {
            if !s.busy() {
                s.data.add_paths(paths.to_vec());
            }
        })
}

fn timeline(s: &State, viewport: f32) -> El {
    let t = tok::theme(s.dark);
    if s.data.lanes.is_empty() {
        return col()
            .key("timeline")
            .grow(1.0)
            .min_h(160.0)
            .w_full()
            .center()
            .gap(12.0)
            .radius(10.0)
            .border(1.0, t.card_border)
            .child(
                text("Sync your recordings. Keep your edit.")
                    .font(type_::TITLE)
                    .color(t.text),
            )
            .child(label(
                "Drop files, folders or an XML / FCPXML / AAF timeline here",
                t.text_secondary,
            ));
    }
    let duration = s
        .data
        .lanes
        .iter()
        .flat_map(|l| &l.clips)
        .map(|b| b.start + b.duration)
        .fold(1.0f64, f64::max);
    let width = (viewport - 150.0).max(200.0) * s.data.zoom() as f32;
    let scale = width as f64 / duration;
    let ruler = row()
        .h(28.0)
        .child(el().w(140.0))
        .children((0..=8).map(|i| {
            label(
                format!("{:.0}s", duration * i as f64 / 8.0),
                t.text_secondary,
            )
            .w(width / 8.0)
        }));
    let lanes = s.data.lanes.iter().map(|lane| {
        let mut track = el().w(width).h(74.0).fill(t.card);
        for bar in &lane.clips {
            let x = (bar.start * scale) as f32;
            let w = (bar.duration * scale) as f32;
            let color = match bar.match_state {
                BarMatchState::Matched => rune::oklch(0.64, 0.14, 150.0),
                BarMatchState::Unmatched => rune::oklch(0.73, 0.14, 65.0),
                BarMatchState::Pending => tok::accent(0, s.dark),
            };
            let path = bar.url.clone();
            let mut clip = col()
                .key(format!("bar-{}", bar.id))
                .absolute(x, 6.0)
                .size(w.max(3.0), 62.0)
                .radius(5.0)
                .fill(color.alpha(0.24))
                .border(1.0, color)
                .on_press(move |s: &mut State| {
                    s.data.selection.clear();
                    s.data.selection.insert(path.clone());
                })
                .child(label(&bar.name, t.text).truncate().px(6.0).h(22.0));
            if let Some(wave) = s.data.waveform_previews.get(&bar.clip_id) {
                let mut shape = Shape::new();
                let stride = (wave.len() / 180).max(1);
                for (i, value) in wave.iter().step_by(stride).enumerate() {
                    let x = i as f32 / wave.len().div_ceil(stride).max(1) as f32 * w;
                    let a = value.abs().clamp(0.0, 1.0) * 14.0;
                    shape = shape.stroke(vec![[x, 16.0 - a], [x, 16.0 + a]], 1.0, color);
                }
                clip = clip.child(rune::canvas(shape).w_full().h(32.0));
            } else {
                clip = clip.child(
                    label(
                        format!("{:.1}s · {:.0}%", bar.duration, bar.confidence * 100.0),
                        t.text_secondary,
                    )
                    .truncate()
                    .px(6.0),
                );
            }
            track = track.child(clip);
        }
        row()
            .h(74.0)
            .child(
                col()
                    .w(140.0)
                    .h_full()
                    .center_y()
                    .px(10.0)
                    .child(label(format!("{:?} {}", lane.kind, lane.number), t.text))
                    .child(label(&lane.source_name, t.text_secondary).truncate()),
            )
            .child(track)
    });
    col()
        .key("timeline")
        .grow(1.0)
        .min_h(100.0)
        .min_w(0.0)
        .w_full()
        .scroll_xy()
        .radius(8.0)
        .border(1.0, t.card_border)
        .child(col().w(width + 140.0).child(ruler).children(lanes))
}

fn settings(s: &State) -> El {
    let t = tok::theme(s.dark);
    let current = s.data.current_effective_settings();
    col()
        .gap(8.0)
        .padding(12.0)
        .fill(t.card)
        .radius(8.0)
        .child(label("Synchronization · common settings", t.text))
        .child(
            row()
                .gap(5.0)
                .child(label("Search", t.text_secondary).w(85.0))
                .children(SearchAccuracy::ALL.into_iter().map(|value| {
                    button(
                        format!("accuracy-{value:?}"),
                        value.label(),
                        true,
                        current.search_accuracy == value,
                        s.dark,
                        move |s, _| {
                            s.data.set_scoped_search_accuracy(Some(value));
                        },
                    )
                })),
        )
        .child(
            row()
                .gap(5.0)
                .child(label("Time", t.text_secondary).w(85.0))
                .children(
                    [
                        TemporalMode::Auto,
                        TemporalMode::RecStart,
                        TemporalMode::RecStop,
                        TemporalMode::Timecode,
                    ]
                    .into_iter()
                    .map(|value| {
                        button(
                            format!("time-{value:?}"),
                            value.title(),
                            true,
                            current.temporal_mode == value,
                            s.dark,
                            move |s, _| {
                                s.data.set_scoped_temporal_mode(Some(value));
                            },
                        )
                    }),
                ),
        )
        .child(
            row()
                .gap(5.0)
                .child(label("Threshold", t.text_secondary).w(85.0))
                .children(
                    [
                        MatchThreshold::Permissive,
                        MatchThreshold::Balanced,
                        MatchThreshold::Conservative,
                    ]
                    .into_iter()
                    .map(|value| {
                        button(
                            format!("threshold-{value:?}"),
                            format!("{value:?}"),
                            true,
                            current.match_threshold == value,
                            s.dark,
                            move |s, _| {
                                s.data.set_scoped_match_threshold(Some(value));
                            },
                        )
                    }),
                ),
        )
        .child(
            row()
                .gap(5.0)
                .child(label("Audio", t.text_secondary).w(85.0))
                .children(
                    [
                        AudioAnalysisSource::Automatic,
                        AudioAnalysisSource::AllMixed,
                        AudioAnalysisSource::Channel(0),
                        AudioAnalysisSource::Channel(1),
                    ]
                    .into_iter()
                    .map(|value| {
                        button(
                            format!("audio-{value:?}"),
                            format!("{value:?}"),
                            true,
                            current.audio_source == value,
                            s.dark,
                            move |s, _| {
                                s.data.set_scoped_audio_source(Some(value));
                            },
                        )
                    }),
                ),
        )
}

fn export_panel(s: &State) -> El {
    let t = tok::theme(s.dark);
    col()
        .gap(8.0)
        .padding(12.0)
        .fill(t.card)
        .radius(8.0)
        .child(label("Export aligned timeline", t.text))
        .child(
            col()
                .gap(5.0)
                .children(ExportTarget::all().into_iter().map(|value| {
                    button(
                        format!("format-{value:?}"),
                        value.title(),
                        true,
                        s.data.export_selected.contains(&value),
                        s.dark,
                        move |s, _| {
                            if !s.data.export_selected.remove(&value) {
                                s.data.export_selected.insert(value);
                            }
                        },
                    )
                })),
        )
        .child(
            row()
                .gap(8.0)
                .child(button(
                    "export-dir",
                    "Choose output folder…",
                    !s.busy(),
                    false,
                    s.dark,
                    |_, tx| {
                        tx.open_folder(
                            FileDialogOptions {
                                title: "Export folder".into(),
                                ..Default::default()
                            },
                            |s: &mut State, answer, _| match answer {
                                Ok(Some(path)) => s.data.export_dir = Some(path),
                                Err(e) => s.data.error = Some(e.to_string()),
                                _ => {}
                            },
                        )
                    },
                ))
                .child(button(
                    "drift",
                    if s.data.export_drift {
                        "Drift correction: on"
                    } else {
                        "Drift correction: off"
                    },
                    !s.busy(),
                    false,
                    s.dark,
                    |s, _| s.data.export_drift = !s.data.export_drift,
                ))
                .child(button(
                    "write-export",
                    "Write timelines",
                    s.data.can_begin_export(),
                    true,
                    s.dark,
                    |s, _| s.export(),
                )),
        )
        .child(
            label(
                s.data
                    .export_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "Choose a destination".into()),
                t.text_secondary,
            )
            .truncate(),
        )
}
