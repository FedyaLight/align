//! Updates, feedback, About, AI agent setup, and error alerts.

use super::animation::slide_in;
use super::controls::{button, hover_tip, modal_header, prominent_button};
use super::{
    AlignApp, ClipboardItem, Context, Div, FluentBuilder, InteractiveElement, IntoElement,
    Operation, ParentElement, PathBuf, SharedString, StatefulInteractiveElement, Styled, Theme,
    UpdateState, div, img, px, rgb, svg_icon, updater,
};

// ---------------- updates

pub(super) fn update_action_label(update: &updater::Update) -> String {
    match update {
        updater::Update::Manual { version, .. } => format!("Download {version}"),
        update => format!("Update to {}", update.version()),
    }
}

/// Floating card announcing a new release, with the next step inline. It
/// works on every platform, alongside the About panel's update controls.
pub(super) fn update_notice(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    update_state: &UpdateState,
    operation: Operation,
) -> Option<impl IntoElement> {
    let version = update_state.version()?.to_owned();
    let restart_blocked = matches!(
        operation,
        Operation::Synchronizing | Operation::Exporting | Operation::Repairing
    );
    let (detail, action) = match update_state {
        UpdateState::Available(update) => (
            "A new version is ready to download.",
            prominent_button(
                cx,
                theme,
                "update-notice-action",
                update_action_label(update),
                true,
                |this, _, _, cx| this.download_update(cx),
            ),
        ),
        UpdateState::Downloading { .. } => (
            "Downloading in the background.",
            prominent_button(
                cx,
                theme,
                "update-notice-action",
                "Downloading…",
                false,
                |_, _, _, _| {},
            ),
        ),
        UpdateState::Ready(_) => (
            if restart_blocked {
                "Installs when the current operation finishes."
            } else {
                "Downloaded. Align restarts to finish."
            },
            prominent_button(
                cx,
                theme,
                "update-notice-action",
                "Restart and Update",
                !restart_blocked,
                |this, _, _, cx| this.restart_and_update(cx),
            ),
        ),
        UpdateState::Installing { .. } => (
            "Approve the system prompt if one appears.",
            prominent_button(
                cx,
                theme,
                "update-notice-action",
                "Installing…",
                false,
                |_, _, _, _| {},
            ),
        ),
        _ => return None,
    };
    let dismiss: Option<NoticeDismiss> =
        (!update_state.is_busy()).then_some(("Later", |this| this.update_notice_dismissed = true));
    Some(floating_notice(
        cx,
        theme,
        "update-notice",
        format!("Align {version}"),
        detail,
        action,
        dismiss,
    ))
}

/// Close-button tooltip and what closing a floating notice records.
pub(super) type NoticeDismiss = (&'static str, fn(&mut AlignApp));

/// Bottom-right card shared by the update and feedback notices: app icon,
/// title, detail, one action, and an optional close button.
pub(super) fn floating_notice(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: &'static str,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    action: impl IntoElement,
    dismiss: Option<NoticeDismiss>,
) -> impl IntoElement {
    let card = div()
        .id(id)
        .w(px(300.))
        .p_3()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_lg()
        .child(img(crate::icons::APP_ICON).size(px(36.)).rounded_md())
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight(600.0))
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme.dim))
                        .child(detail.into()),
                )
                .child(div().mt_2().flex().child(action)),
        )
        .when_some(dismiss, |card, (tip, dismiss)| {
            card.child(
                div()
                    .id((id, 0usize))
                    .cursor_pointer()
                    .opacity(0.6)
                    .hover(|this| this.opacity(1.))
                    .child(svg_icon("icons/close.svg", 12., theme.icon))
                    .tooltip(hover_tip(tip.to_string(), theme))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        dismiss(this);
                        cx.notify();
                    })),
            )
        });
    div()
        .absolute()
        .right(px(16.))
        .bottom(px(16.))
        .child(slide_in(card, (id, 1usize), 0))
}

// ---------------- user-initiated feedback

pub(super) fn feedback_notice(cx: &mut Context<AlignApp>, theme: &Theme) -> impl IntoElement {
    let action = button(
        cx,
        theme,
        "feedback-notice-report",
        "Report a Problem…",
        true,
        |this, _, _, cx| this.show_problem_report(cx),
    )
    // Ends the opaque type's borrow of `cx`.
    .into_any_element();
    floating_notice(
        cx,
        theme,
        "feedback-notice",
        "Something not working?",
        "Tell us and we will look into it. Report a Problem is always in the Help menu.",
        action,
        Some(("Dismiss", |this| this.dismiss_feedback_notice())),
    )
}

pub(super) fn problem_report_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    error: Option<SharedString>,
) -> impl IntoElement {
    let mut panel = div()
        .id("problem-report-panel")
        .w(px(460.))
        .max_h_full()
        .m_4()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Report a Problem",
            "problem-report-close",
            |this, _, window, cx| this.close_problem_report(window, cx),
        ))
        .child("Tell us what happened and how to reproduce it. Your report helps us investigate and fix Align.")
        .child(setup_text_block(theme, crate::feedback::environment()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child("Continue to a prefilled issue on GitHub, where you can add details and screenshots. A GitHub account is required. Nothing is posted until you submit it there."),
        );
    if let Some(error) = error {
        panel = panel
            .child(button(
                cx,
                theme,
                "problem-report-copy-error",
                "Copy error details",
                true,
                move |_, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(error.to_string()));
                },
            ))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(theme.dim))
                    .child("Error details may contain filenames and paths. Remove anything private before pasting them into your public report."),
            );
    }
    panel.child(
        div()
            .flex()
            .gap_2()
            .child(button(
                cx,
                theme,
                "problem-report-existing",
                "View existing issues",
                true,
                |_, _, _, cx| cx.open_url(crate::feedback::ISSUES_URL),
            ))
            .child(prominent_button(
                cx,
                theme,
                "problem-report-continue",
                "Continue on GitHub",
                true,
                |this, _, window, cx| {
                    cx.open_url(&crate::feedback::issue_url());
                    this.close_problem_report(window, cx);
                },
            )),
    )
}

// ---------------- about panel (Align menu → About Align)

pub(super) fn platform_label() -> String {
    format!(
        "{} · {}",
        crate::feedback::os_name(),
        std::env::consts::ARCH
    )
}

pub(super) fn about_panel(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    update_state: &UpdateState,
    operation: Operation,
) -> impl IntoElement {
    let update_version = update_state.version().unwrap_or_default();
    let restart_blocked = matches!(
        operation,
        Operation::Synchronizing | Operation::Exporting | Operation::Repairing
    );

    let mut update_actions = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2();
    match update_state {
        UpdateState::Idle | UpdateState::Current | UpdateState::Failed => {
            update_actions = update_actions.child(button(
                cx,
                theme,
                "about-check-updates",
                match update_state {
                    UpdateState::Idle => "Check for Updates",
                    UpdateState::Current => "Up to Date",
                    UpdateState::Failed => "Couldn’t Check · Try Again",
                    _ => unreachable!(),
                },
                true,
                |this, _, _, cx| this.check_for_updates(cx),
            ));
        }
        UpdateState::Checking => {
            update_actions = update_actions.child(button(
                cx,
                theme,
                "about-checking-updates",
                "Checking for Updates…",
                false,
                |_, _, _, _| {},
            ));
        }
        UpdateState::Available(update) => {
            update_actions = update_actions.child(prominent_button(
                cx,
                theme,
                "about-download-update",
                update_action_label(update),
                true,
                |this, _, _, cx| this.download_update(cx),
            ));
        }
        UpdateState::Installing { .. } => {
            update_actions = update_actions.child(prominent_button(
                cx,
                theme,
                "about-installing-update",
                format!("Installing {update_version}…"),
                false,
                |_, _, _, _| {},
            ));
        }
        UpdateState::Downloading { .. } => {
            update_actions = update_actions.child(prominent_button(
                cx,
                theme,
                "about-downloading-update",
                format!("Downloading {update_version}…"),
                false,
                |_, _, _, _| {},
            ));
        }
        UpdateState::Ready(_) => {
            update_actions = update_actions.child(prominent_button(
                cx,
                theme,
                "about-install-update",
                if restart_blocked {
                    "Finish Current Operation First"
                } else {
                    "Restart and Update"
                },
                !restart_blocked,
                |this, _, _, cx| this.restart_and_update(cx),
            ));
        }
    }

    div()
        .id("about-panel")
        .flex()
        .flex_col()
        .items_center()
        .gap_3()
        .p_5()
        .m_4()
        .w(px(380.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "About",
            "btn-about-close",
            |this, _, _, cx| {
                this.data.show_about = false;
                cx.notify();
            },
        ))
        .child(
            img(crate::icons::APP_ICON)
                .mt_1()
                .size(px(72.))
                .rounded_xl(),
        )
        .child(
            div()
                .text_size(px(24.))
                .font_weight(gpui::FontWeight(650.0))
                .child("Align"),
        )
        .child(
            div()
                .text_color(rgb(theme.dim))
                .text_size(px(12.))
                .child(format!(
                    "Version {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    platform_label()
                )),
        )
        .child(update_actions)
        .child(button(
            cx,
            theme,
            "about-github",
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(svg_icon("icons/github.svg", 13.0, theme.icon))
                .child("Star on GitHub"),
            true,
            |_, _, _, cx| cx.open_url(updater::REPOSITORY_URL),
        ))
        .child(button(
            cx,
            theme,
            "about-report-problem",
            "Report a Problem…",
            true,
            |this, _, _, cx| this.show_problem_report(cx),
        ))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.dim))
                .child("© 2026 Align contributors"),
        )
}

// ---------------- CLI setup (Align menu → Use with AI Agents…)

fn agent_prompt() -> String {
    format!(
        "Use the Align CLI at {}. Inspect my media by passing its paths directly. Run sync --output-json result.json --summary, then report matched and unmatched clips, drift and warnings. When I request an export, use export-json with the saved result.",
        cli_path().display()
    )
}

fn cli_path() -> PathBuf {
    let executable = if cfg!(windows) {
        "align-cli.exe"
    } else {
        "align-cli"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(executable)))
        .unwrap_or_else(|| PathBuf::from(executable))
}

pub(super) fn setup_text_block(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.bg))
        .text_size(px(12.))
        .text_color(rgb(theme.icon))
        .whitespace_normal()
        .child(text.into())
}

pub(super) fn agent_setup_panel(cx: &mut Context<AlignApp>, theme: &Theme) -> impl IntoElement {
    let path = cli_path().display().to_string();
    div()
        .id("agent-setup")
        .w(px(620.))
        .max_h_full()
        .m_4()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Use Align with AI agents",
            "agent-setup-close",
            |this, _, _, cx| {
                this.data.show_agent_setup = false;
                cx.notify();
            },
        ))
        .child(
            div()
                .text_color(rgb(theme.dim))
                .child("Copy the prompt into an agent that can run local commands."),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .font_weight(gpui::FontWeight(600.))
                        .child("CLI executable"),
                )
                .child(div().flex_1())
                .child(button(
                    cx,
                    theme,
                    "copy-cli-path",
                    "Copy path",
                    true,
                    |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            cli_path().display().to_string(),
                        ));
                    },
                )),
        )
        .child(setup_text_block(theme, path))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .font_weight(gpui::FontWeight(600.))
                        .child("Agent prompt"),
                )
                .child(div().flex_1())
                .child(prominent_button(
                    cx,
                    theme,
                    "copy-agent-prompt",
                    "Copy prompt",
                    true,
                    |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(agent_prompt()));
                    },
                )),
        )
        .child(setup_text_block(theme, agent_prompt()))
}

// ---------------- error alert

pub(super) fn error_alert(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    data: &crate::state::AppData,
) -> impl IntoElement {
    let panel = div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .m_4()
        .max_w(px(420.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .shadow_md()
        .child(modal_header(
            cx,
            theme,
            "Align could not finish",
            "btn-alert-close",
            |this, _, _, cx| {
                this.data.dismiss_error();
                cx.notify();
            },
        ))
        .child(
            data.error
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        );
    // Recovery comes first; reporting is the fallback when it does not help.
    panel
        .when(data.can_relink_media(), |panel| {
            panel.child(button(
                cx,
                theme,
                "btn-error-relink-media",
                "Locate media…",
                true,
                |this, _, _, cx| this.choose_missing_media(cx),
            ))
        })
        .child(
            div()
                .mt_1()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child("If this looks like a bug, report it and we will look into it."),
        )
        .child(button(
            cx,
            theme,
            "error-report-problem",
            "Report a Problem…",
            true,
            |this, _, _, cx| this.show_problem_report(cx),
        ))
}
