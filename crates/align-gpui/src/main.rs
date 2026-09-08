//! align: native GPUI desktop app (Metal / DirectX / Vulkan via Blade).

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod icons;
mod lane;
mod motion;
mod state;
mod text_input;
mod theme;
mod views;

use gpui::{
    App, AppContext, Application, Bounds, Entity, Global, KeyBinding, Menu, MenuItem,
    SystemMenuType, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size,
};

actions!(
    align,
    [
        AboutAlign,
        UseWithAiAgents,
        LightAppearance,
        DarkAppearance,
        SystemAppearance,
        AddMedia,
        ReloadAndSynchronize,
        ExportTimeline,
        OpenPathFixer,
        ClearCurrentAnalysisCache,
        ClearAnalysisCache,
        KeepAnalysisCache7Days,
        KeepAnalysisCache30Days,
        KeepAnalysisCache90Days,
        KeepAnalysisCacheForever,
        QuitApp,
        HideApp,
        HideOthersApp,
        ShowAllApps,
        MinimizeWindow,
        ZoomWindow,
        ToggleFullscreen
    ]
);

/// Root view handle, reachable from app-level menu actions (which have
/// no focused view to dispatch through). Window operations themselves
/// live as view-level `on_action` handlers: dispatch already holds the
/// window checked out, so they use the borrowed `&mut Window` directly.
#[derive(Clone)]
struct AppView(Entity<views::AlignApp>);

impl Global for AppView {}

fn show_about(cx: &mut App) {
    let Some(view) = cx.try_global::<AppView>().map(|app| app.0.clone()) else {
        return;
    };
    view.update(cx, |this, cx| {
        this.data.show_about = true;
        cx.notify();
    });
}

fn show_agent_setup(cx: &mut App) {
    update_view(cx, |this, cx| {
        this.data.show_agent_setup = true;
        cx.notify();
    });
}

fn clear_analysis_cache(cx: &mut App) {
    let Some(view) = cx.try_global::<AppView>().map(|app| app.0.clone()) else {
        return;
    };
    view.update(cx, |this, cx| {
        if matches!(
            this.data.operation,
            state::Operation::Synchronizing
                | state::Operation::Exporting
                | state::Operation::Repairing
        ) {
            this.data.status = "Wait for the current operation before clearing the cache.".into();
            cx.notify();
            return;
        }
        let cache = align_core::FingerprintCache::new(None);
        let statistics = cache.statistics();
        cache.clear();
        this.data.status = if statistics.file_count == 0 {
            "Analysis cache is already empty.".into()
        } else {
            format!(
                "Cleared {} cached analysis file{} ({:.1} MB).",
                statistics.file_count,
                if statistics.file_count == 1 { "" } else { "s" },
                statistics.total_bytes as f64 / 1_048_576.0
            )
        };
        cx.notify();
    });
}

fn clear_current_analysis_cache(cx: &mut App) {
    update_view(cx, |this, cx| {
        if matches!(
            this.data.operation,
            state::Operation::Synchronizing
                | state::Operation::Exporting
                | state::Operation::Repairing
        ) {
            this.data.status = "Wait for the current operation before clearing the cache.".into();
            cx.notify();
            return;
        }
        let media = this.data.current_cache_media();
        let removed = align_core::FingerprintCache::new(None).clear_media(&media);
        this.data.status = if media.is_empty() {
            "Analyze this project before clearing its cache.".into()
        } else if removed.file_count == 0 {
            "Current project analysis cache is already empty.".into()
        } else {
            format!(
                "Cleared {} cached analysis file{} for this project ({:.1} MB).",
                removed.file_count,
                if removed.file_count == 1 { "" } else { "s" },
                removed.total_bytes as f64 / 1_048_576.0
            )
        };
        cx.notify();
    });
}

fn set_cache_retention(days: Option<u64>, cx: &mut App) {
    align_core::CacheSettings {
        retention_days: days,
    }
    .save();
    update_view(cx, |this, cx| {
        this.data.status = match days {
            Some(days) => format!("Cached analysis will be removed after {days} days."),
            None => "Cached analysis will be kept until you clear it.".into(),
        };
        cx.notify();
    });
}

fn update_view(
    cx: &mut App,
    update: impl FnOnce(&mut views::AlignApp, &mut gpui::Context<views::AlignApp>),
) {
    let Some(view) = cx.try_global::<AppView>().map(|app| app.0.clone()) else {
        return;
    };
    view.update(cx, update);
}

fn set_appearance(appearance: theme::AppearancePreference, cx: &mut App) {
    appearance.save();
    cx.set_menus(app_menus(appearance));
    update_view(cx, |this, cx| {
        this.data.appearance = appearance;
        cx.notify();
    });
}

fn appearance_label(
    label: &'static str,
    selected: theme::AppearancePreference,
    item: theme::AppearancePreference,
) -> &'static str {
    if selected == item {
        match item {
            theme::AppearancePreference::Auto => "✓ Auto",
            theme::AppearancePreference::Light => "✓ Light",
            theme::AppearancePreference::Dark => "✓ Dark",
        }
    } else {
        label
    }
}

fn app_menus(appearance: theme::AppearancePreference) -> Vec<Menu> {
    vec![
        Menu {
            name: "Align".into(),
            items: vec![
                MenuItem::action("About Align", AboutAlign),
                MenuItem::separator(),
                MenuItem::action("Use with AI Agents…", UseWithAiAgents),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Align", HideApp),
                MenuItem::action("Hide Others", HideOthersApp),
                MenuItem::action("Show All", ShowAllApps),
                MenuItem::separator(),
                MenuItem::action("Quit Align", QuitApp),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Add Media…", AddMedia),
                MenuItem::action("Reload and Synchronize", ReloadAndSynchronize),
                MenuItem::separator(),
                MenuItem::action("Export…", ExportTimeline),
                MenuItem::separator(),
                MenuItem::action("Path Fixer…", OpenPathFixer),
                MenuItem::submenu(Menu {
                    name: "Analysis Cache".into(),
                    items: vec![
                        MenuItem::action("Clear Current Project", ClearCurrentAnalysisCache),
                        MenuItem::action("Clear All", ClearAnalysisCache),
                        MenuItem::separator(),
                        MenuItem::action("Keep for 7 Days", KeepAnalysisCache7Days),
                        MenuItem::action("Keep for 30 Days", KeepAnalysisCache30Days),
                        MenuItem::action("Keep for 90 Days", KeepAnalysisCache90Days),
                        MenuItem::action("Keep Until Cleared", KeepAnalysisCacheForever),
                    ],
                }),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![MenuItem::submenu(Menu {
                name: "Appearance".into(),
                items: vec![
                    MenuItem::action(
                        appearance_label("Auto", appearance, theme::AppearancePreference::Auto),
                        SystemAppearance,
                    ),
                    MenuItem::action(
                        appearance_label("Light", appearance, theme::AppearancePreference::Light),
                        LightAppearance,
                    ),
                    MenuItem::action(
                        appearance_label("Dark", appearance, theme::AppearancePreference::Dark),
                        DarkAppearance,
                    ),
                ],
            })],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", MinimizeWindow),
                MenuItem::action("Zoom", ZoomWindow),
                MenuItem::separator(),
                MenuItem::action("Toggle Full Screen", ToggleFullscreen),
            ],
        },
    ]
}

fn main() {
    struct SessionCleanup;
    impl Drop for SessionCleanup {
        fn drop(&mut self) {
            icons::cleanup();
            align_decode::aaf::cleanup_session_media();
        }
    }
    let _session_cleanup = SessionCleanup;
    let initial_paths: Vec<std::path::PathBuf> = std::env::args_os()
        .skip(1)
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().to_string_lossy().starts_with('-'))
        .collect();
    Application::new()
        .with_assets(icons::FileAssets)
        .run(move |cx: &mut App| {
            text_input::init(cx);
            // Covers every graceful termination path, including an OS-level
            // quit that does not dispatch our custom QuitApp action.
            cx.on_app_quit(|_| async {
                icons::cleanup();
                align_decode::aaf::cleanup_session_media();
            })
            .detach();
            // System commands: without registered actions + bindings + menus
            // macOS swallows keys like Cmd+Q and the menu bar stays empty.
            cx.on_action(|_: &AboutAlign, cx| show_about(cx));
            cx.on_action(|_: &UseWithAiAgents, cx| show_agent_setup(cx));
            cx.on_action(|_: &LightAppearance, cx| {
                set_appearance(theme::AppearancePreference::Light, cx)
            });
            cx.on_action(|_: &DarkAppearance, cx| {
                set_appearance(theme::AppearancePreference::Dark, cx)
            });
            cx.on_action(|_: &SystemAppearance, cx| {
                set_appearance(theme::AppearancePreference::Auto, cx)
            });
            cx.on_action(|_: &ClearAnalysisCache, cx| clear_analysis_cache(cx));
            cx.on_action(|_: &ClearCurrentAnalysisCache, cx| clear_current_analysis_cache(cx));
            cx.on_action(|_: &KeepAnalysisCache7Days, cx| set_cache_retention(Some(7), cx));
            cx.on_action(|_: &KeepAnalysisCache30Days, cx| set_cache_retention(Some(30), cx));
            cx.on_action(|_: &KeepAnalysisCache90Days, cx| set_cache_retention(Some(90), cx));
            cx.on_action(|_: &KeepAnalysisCacheForever, cx| set_cache_retention(None, cx));
            cx.on_action(|_: &AddMedia, cx| update_view(cx, |this, cx| this.add_media(cx)));
            cx.on_action(|_: &ReloadAndSynchronize, cx| {
                update_view(cx, |this, cx| this.start_sync(cx));
            });
            cx.on_action(|_: &ExportTimeline, cx| {
                update_view(cx, |this, cx| this.start_export_sheet(cx));
            });
            cx.on_action(|_: &OpenPathFixer, cx| {
                update_view(cx, |this, cx| this.open_path_fixer(cx));
            });
            // Generated icon assets are disposable. Fingerprints are an
            // OS-managed performance cache and must outlive the session.
            cx.on_action(|_: &QuitApp, cx| {
                icons::cleanup();
                cx.quit();
            });
            cx.on_action(|_: &HideApp, cx| cx.hide());
            cx.on_action(|_: &HideOthersApp, cx| cx.hide_other_apps());
            cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
            cx.bind_keys([
                KeyBinding::new("cmd-q", QuitApp, None),
                KeyBinding::new("cmd-h", HideApp, None),
                KeyBinding::new("alt-cmd-h", HideOthersApp, None),
                KeyBinding::new("cmd-m", MinimizeWindow, None),
                KeyBinding::new("cmd-o", AddMedia, None),
                KeyBinding::new("cmd-r", ReloadAndSynchronize, None),
                KeyBinding::new("cmd-e", ExportTimeline, None),
                KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
            ]);
            cx.set_menus(app_menus(theme::AppearancePreference::load()));

            let bounds = Bounds::centered(None, size(px(880.), px(540.)), cx);
            let view = cx.new(move |cx| {
                let mut app = views::AlignApp::new(cx);
                app.data.add_paths(initial_paths);
                app
            });
            cx.set_global(AppView(view.clone()));
            let window = match cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(760.), px(440.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Align".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                {
                    let view = view.clone();
                    move |_, _| view
                },
            ) {
                Ok(window) => window,
                Err(_) => {
                    eprintln!("error: could not open application window");
                    std::process::exit(1);
                }
            };
            cx.set_global(AppView(view));
            // Initial keyboard focus: without it system shortcuts stay dead
            // until the user clicks something focusable (of which we have none).
            window
                .update(cx, |view, window, _| {
                    window.focus(&view.focus_handle);
                })
                .ok();
            cx.activate(true);
        });
}
