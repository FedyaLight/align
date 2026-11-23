//! In-window application menu for platforms without a native menu bar.

use align_core::CacheSettings;
use gpui::{Action, OwnedMenu, OwnedMenuItem};

use crate::theme::AppearancePreference;

use super::animation::{entrance, sidebar_reveal};
use super::controls::{button, modal_header};
use super::export::check_row;
use super::{
    AlignApp, AnchoredPositionMode, AnimationExt, ClickEvent, Context, Div, InteractiveElement,
    IntoElement, MouseButton, ParentElement, SIDEBAR_CLOSE_MS, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Theme, ThemeMode, Window, anchored, deferred, div, point,
    px, rgb, rgba, svg_icon,
};

const WIDTH: f32 = 324.;
const PADDING: f32 = 12.;

pub(super) enum AppMenuPage {
    Main,
    Submenu(OwnedMenu),
    AnalysisCache(CachePage),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AppMenuSelect {
    Appearance,
    Retention,
}

#[derive(Default)]
pub(super) struct CachePage {
    settings: CacheSettings,
    status: Option<String>,
}

impl AlignApp {
    pub(super) fn dismiss_app_menu(&mut self, cx: &mut Context<Self>) {
        if self.app_menu_select.take().is_some() {
            cx.notify();
            return;
        }
        self.close_app_menu(cx);
    }

    fn close_app_menu(&mut self, cx: &mut Context<Self>) {
        if self.app_menu.is_none() || self.app_menu_exit.is_active() {
            return;
        }
        self.app_menu_select = None;
        let exit = self.app_menu_exit.start();
        cx.notify();
        self.after(SIDEBAR_CLOSE_MS, cx, move |this| {
            if this.app_menu_exit.finish(exit) {
                this.app_menu = None;
            }
        });
    }

    pub(crate) fn set_analysis_cache_status(&mut self, status: String, cx: &mut Context<Self>) {
        if let Some(AppMenuPage::AnalysisCache(page)) = &mut self.app_menu {
            page.settings = CacheSettings::load();
            page.status = Some(status.clone());
        }
        self.data.status = status;
        cx.notify();
    }
}

pub(super) fn app_menu_button(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    active: bool,
) -> impl IntoElement {
    button(
        cx,
        theme,
        "btn-app-menu",
        "Menu",
        active,
        |this, _, window, cx| {
            this.data.menu = None;
            this.app_menu_exit.cancel();
            this.app_menu = Some(AppMenuPage::Main);
            this.app_menu_select = None;
            this.focus_handle.focus(window);
            cx.notify();
        },
    )
}

pub(super) fn app_menu_sidebar(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    app: &AlignApp,
    page: &AppMenuPage,
) -> impl IntoElement {
    let closing = app.app_menu_exit.is_active();
    let dim = match theme.mode {
        ThemeMode::Light => rgba(0x1D1D1F33),
        ThemeMode::Dark => rgba(0x00000080),
    };
    let scrim = div()
        .id("app-menu-scrim")
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .bg(dim)
        .occlude()
        .on_click(cx.listener(|this, _, _, cx| this.close_app_menu(cx)))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .with_animation(
            if closing {
                "app-menu-scrim-out"
            } else {
                "app-menu-scrim-in"
            },
            entrance(if closing { SIDEBAR_CLOSE_MS } else { 280 }),
            move |el, progress| el.opacity(if closing { 1. - progress } else { progress }),
        );
    let title = match page {
        AppMenuPage::Main => "Menu".into(),
        AppMenuPage::Submenu(menu) => menu.name.clone(),
        AppMenuPage::AnalysisCache(_) => "Analysis Cache".into(),
    };
    let header = div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(48.))
        .px_3()
        .flex_shrink_0()
        .border_b_1()
        .border_color(rgb(theme.separator))
        .child(modal_header(
            cx,
            theme,
            title,
            "close-app-menu",
            |this, _, _, cx| {
                this.close_app_menu(cx);
            },
        ));
    let mut body = div()
        .id("app-menu-body")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(PADDING))
        .overflow_y_scroll();
    if !matches!(page, AppMenuPage::Main) {
        body = body.child(button(
            cx,
            theme,
            "app-menu-back",
            "‹  Menu",
            true,
            |this, _, _, cx| {
                this.app_menu_select = None;
                this.app_menu = Some(AppMenuPage::Main);
                cx.notify();
            },
        ));
    }
    let body = match page {
        AppMenuPage::Main => {
            let mut menus = crate::app_menu::in_window_menus(cx.get_menus().unwrap_or_default());
            menus.sort_by_key(|menu| menu.name.as_ref() != "File");
            body.children(menus.into_iter().map(|menu| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(section_title(theme, menu.name.clone()))
                    .child(menu_items(cx, theme, app, menu))
            }))
        }
        AppMenuPage::Submenu(menu) => body.child(menu_items(cx, theme, app, menu.clone())),
        AppMenuPage::AnalysisCache(page) => body.child(cache_settings(cx, theme, app, page)),
    };
    let panel = div()
        .w(px(WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(rgb(theme.separator))
        .bg(rgb(theme.panel))
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(header)
        .child(body);
    let mut overlay = div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .flex()
        .child(scrim)
        .child(sidebar_reveal(
            panel,
            if closing {
                "app-menu-slide-out"
            } else {
                "app-menu-slide-in"
            },
            WIDTH,
            closing,
        ));
    if app.app_menu_select.is_some() {
        // The deferred dropdown paints above this layer. An outside click only
        // dismisses the list, even when a clear-cache action is under it.
        overlay = overlay.child(
            div()
                .id("dismiss-app-menu-select")
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.dismiss_app_menu(cx);
                        cx.stop_propagation();
                    }),
                ),
        );
    }
    overlay
}

fn section_title(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .px_2()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight(600.))
        .text_color(rgb(theme.dim))
        .child(label.into())
}

fn menu_items(cx: &mut Context<AlignApp>, theme: &Theme, app: &AlignApp, menu: OwnedMenu) -> Div {
    let mut group = div().flex().flex_col();
    let busy = app.data.has_active_job();
    for (index, item) in menu.items.into_iter().enumerate() {
        let id = SharedString::from(format!("app-menu-{}-{index}", menu.name));
        match item {
            OwnedMenuItem::Action { name, action, .. } => {
                let action_type = action.as_any();
                if action_type.is::<crate::ExportTimeline>()
                    || (action_type.is::<crate::AddMedia>() && busy)
                    || (action_type.is::<crate::ReloadAndSynchronize>() && !app.data.can_run_sync())
                    || (action_type.is::<crate::OpenPathFixer>()
                        && (busy || app.data.path_repair_source().is_none()))
                {
                    continue;
                }
                let name = if action_type.is::<crate::ReloadAndSynchronize>()
                    && !app.data.has_result()
                {
                    "Synchronize".into()
                } else if action_type.is::<crate::CheckForUpdates>() && app.update_state.is_busy() {
                    "Update Status…".into()
                } else {
                    name
                };
                group = group.child(sidebar_row(
                    cx,
                    theme,
                    id,
                    name,
                    None,
                    move |this, _, window, cx| {
                        this.close_app_menu(cx);
                        window.dispatch_action(action.boxed_clone(), cx);
                    },
                ));
            }
            OwnedMenuItem::Submenu(submenu) if submenu.name.as_ref() == "Appearance" => {
                group = group.child(appearance_select(cx, theme, app));
            }
            OwnedMenuItem::Submenu(submenu) => {
                group = group.child(sidebar_row(
                    cx,
                    theme,
                    id,
                    submenu.name.clone(),
                    Some("icons/chevron-right.svg"),
                    move |this, _, _, cx| {
                        this.app_menu = Some(if submenu.name.as_ref() == "Analysis Cache" {
                            AppMenuPage::AnalysisCache(CachePage {
                                settings: CacheSettings::load(),
                                ..Default::default()
                            })
                        } else {
                            AppMenuPage::Submenu(submenu.clone())
                        });
                        cx.notify();
                    },
                ));
            }
            // Section headings and spacing group the in-window commands.
            OwnedMenuItem::Separator | OwnedMenuItem::SystemMenu(_) => {}
        }
    }
    group
}

type SelectOption = (&'static str, bool, Box<dyn Action>);

fn appearance_select(cx: &mut Context<AlignApp>, theme: &Theme, app: &AlignApp) -> Div {
    let appearance = app.data.appearance;
    let value = match appearance {
        AppearancePreference::Auto => "Auto",
        AppearancePreference::Light => "Light",
        AppearancePreference::Dark => "Dark",
    };
    let options: [SelectOption; 3] = [
        (
            "Auto",
            appearance == AppearancePreference::Auto,
            Box::new(crate::SystemAppearance),
        ),
        (
            "Light",
            appearance == AppearancePreference::Light,
            Box::new(crate::LightAppearance),
        ),
        (
            "Dark",
            appearance == AppearancePreference::Dark,
            Box::new(crate::DarkAppearance),
        ),
    ];
    sidebar_select(
        cx,
        theme,
        AppMenuSelect::Appearance,
        app.app_menu_select,
        value,
        options,
    )
}

fn cache_settings(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    app: &AlignApp,
    page: &CachePage,
) -> Div {
    let settings = page.settings;
    let days = settings.retention_days;
    let retention = days.map_or_else(|| "Until cleared".into(), |days| format!("{days} days"));
    let options: [SelectOption; 4] = [
        (
            "7 days",
            days == Some(7),
            Box::new(crate::KeepAnalysisCache7Days),
        ),
        (
            "30 days",
            days == Some(30),
            Box::new(crate::KeepAnalysisCache30Days),
        ),
        (
            "90 days",
            days == Some(90),
            Box::new(crate::KeepAnalysisCache90Days),
        ),
        (
            "Until cleared",
            days.is_none(),
            Box::new(crate::KeepAnalysisCacheForever),
        ),
    ];
    let mut controls = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(check_row(
            cx,
            theme,
            "app-cache-enabled",
            settings.enabled,
            "Use on-disk analysis cache".into(),
            true,
            |_, _, window, cx| {
                window.dispatch_action(Box::new(crate::ToggleAnalysisCache), cx);
            },
        ))
        .child(
            div()
                .px_2()
                .text_size(px(12.))
                .text_color(rgb(theme.dim))
                .child("Applies to new analyses. Existing files stay until cleared or expired."),
        )
        .child(div().mt_3().child(sidebar_select(
            cx,
            theme,
            AppMenuSelect::Retention,
            app.app_menu_select,
            retention,
            options,
        )));
    if !app.data.has_active_job() {
        controls = controls.child(div().mt_3().child(section_title(theme, "Clear cache")));
        if app.data.current_cache_paths().next().is_some() {
            controls = controls.child(sidebar_row(
                cx,
                theme,
                "app-cache-clear-project",
                "Clear Current Project",
                None,
                |_, _, window, cx| {
                    window.dispatch_action(Box::new(crate::ClearCurrentAnalysisCache), cx);
                },
            ));
        }
        controls = controls.child(sidebar_row(
            cx,
            theme,
            "app-cache-clear-all",
            "Clear All",
            None,
            |_, _, window, cx| window.dispatch_action(Box::new(crate::ClearAnalysisCache), cx),
        ));
    }
    controls.children(page.status.as_ref().map(|status| {
        div()
            .px_2()
            .pt_2()
            .text_size(px(12.))
            .text_color(rgb(theme.dim))
            .child(status.clone())
    }))
}

fn sidebar_select(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    select: AppMenuSelect,
    current: Option<AppMenuSelect>,
    value: impl Into<SharedString>,
    options: impl IntoIterator<Item = SelectOption>,
) -> Div {
    let (id, label) = match select {
        AppMenuSelect::Appearance => ("app-menu-appearance", "Appearance"),
        AppMenuSelect::Retention => ("app-menu-retention", "Keep for"),
    };
    let open = current == Some(select);
    let control = div()
        .id(id)
        .h(px(28.))
        .px_3()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_lg()
        .text_size(px(12.))
        .text_color(rgb(theme.icon))
        .bg(rgb(if open {
            theme.border
        } else {
            theme.button_hover
        }))
        .cursor_pointer()
        .hover(|row| row.bg(rgb(theme.border)))
        .active(|row| row.opacity(0.62))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.app_menu_select = Some(select);
            cx.notify();
            cx.stop_propagation();
        }))
        .child(
            div()
                .min_w(px(0.))
                .truncate()
                .child(format!("{label}: {}", value.into())),
        )
        .child(
            div()
                .flex_shrink_0()
                .child(svg_icon("icons/chevron-down.svg", 12., theme.icon)),
        );
    let mut field = div().relative().child(control);
    if open {
        let popup =
            div()
                .id(SharedString::from(format!("{id}-options")))
                .w(px(WIDTH - PADDING * 2. - 1.))
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(theme.border))
                .bg(rgb(theme.panel))
                .shadow_md()
                .occlude()
                .children(options.into_iter().enumerate().map(
                    |(index, (label, selected, action))| {
                        sidebar_row(
                            cx,
                            theme,
                            format!("{id}-{index}"),
                            label,
                            selected.then_some("icons/check.svg"),
                            move |this, _, window, cx| {
                                this.dismiss_app_menu(cx);
                                window.dispatch_action(action.boxed_clone(), cx);
                            },
                        )
                    },
                ));
        field = field.child(deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Local)
                .offset(point(px(0.), px(4.)))
                .snap_to_window_with_margin(px(8.))
                .child(popup),
        ));
    }
    field
}

fn sidebar_row(
    cx: &mut Context<AlignApp>,
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    icon: Option<&'static str>,
    action: impl Fn(&mut AlignApp, &ClickEvent, &mut Window, &mut Context<AlignApp>) + 'static,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .h(px(32.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .rounded_lg()
        .text_size(px(13.))
        .cursor_pointer()
        .hover(|row| row.bg(rgb(theme.button_hover)))
        .on_click(cx.listener(move |this, event, window, cx| {
            action(this, event, window, cx);
            cx.stop_propagation();
        }))
        .child(label.into())
        .children(icon.map(|icon| svg_icon(icon, 12., theme.icon)))
}
