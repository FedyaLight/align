//! align: native GPUI desktop app (Metal / DirectX / Vulkan via Blade).
// Release builds on Windows are GUI programs: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app_menu;
mod feedback;
mod icons;
mod lane;
mod motion;
mod project;
mod settings;
mod state;
mod text_input;
mod theme;
mod updater;
mod views;
#[cfg(windows)]
mod windows_association;

use align_core::CacheSettings;
use gpui::{
    App, AppContext, Application, Bounds, Entity, Global, KeyBinding, Menu, MenuItem,
    SystemMenuType, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size,
};

actions!(
    align,
    [
        AboutAlign,
        ReportProblem,
        CheckForUpdates,
        UseWithAiAgents,
        LightAppearance,
        DarkAppearance,
        SystemAppearance,
        AddMedia,
        OpenProject,
        SaveProject,
        SaveProjectAs,
        EnableProjectAutosave,
        DisableProjectAutosave,
        ChooseProjectAutosaveFolder,
        ReloadAndSynchronize,
        ExportTimeline,
        OpenPathFixer,
        ClearCurrentAnalysisCache,
        ClearAnalysisCache,
        ToggleAnalysisCache,
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

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = align, no_json)]
struct OpenRecentProject(std::path::PathBuf);

/// Root view handle, reachable from app-level menu actions (which have
/// no focused view to dispatch through). Window operations themselves
/// live as view-level `on_action` handlers: dispatch already holds the
/// window checked out, so they use the borrowed `&mut Window` directly.
#[derive(Clone)]
struct AppView(Entity<views::AlignApp>);

impl Global for AppView {}

fn show_about(cx: &mut App) {
    update_view(cx, |this, cx| {
        this.data.show_about = true;
        cx.notify();
    });
}

fn check_for_updates(cx: &mut App) {
    update_view(cx, |this, cx| {
        this.data.show_about = true;
        this.check_for_updates(cx);
    });
}

fn show_agent_setup(cx: &mut App) {
    update_view(cx, |this, cx| {
        this.data.show_agent_setup = true;
        cx.notify();
    });
}

fn clear_analysis_cache(cx: &mut App) {
    update_view(cx, |this, cx| {
        if this.data.has_active_job() {
            this.set_analysis_cache_status(
                "Wait for the current operation before clearing the cache.".into(),
                cx,
            );
            return;
        }
        // Reading and removing every entry can take a while on a large cache.
        let cleared = cx.background_executor().spawn(async {
            let cache = align_core::FingerprintCache::new(None);
            let statistics = cache.statistics();
            cache.clear();
            statistics
        });
        cx.spawn(async move |view, cx| {
            let statistics = cleared.await;
            let status = if statistics.file_count == 0 {
                "Analysis cache is already empty.".into()
            } else {
                format!(
                    "Cleared {} cached analysis file{} ({:.1} MB).",
                    statistics.file_count,
                    if statistics.file_count == 1 { "" } else { "s" },
                    statistics.total_bytes as f64 / 1_048_576.0
                )
            };
            let _ = view.update(cx, |this, cx| this.set_analysis_cache_status(status, cx));
        })
        .detach();
    });
}

fn clear_current_analysis_cache(cx: &mut App) {
    update_view(cx, |this, cx| {
        if this.data.has_active_job() {
            this.set_analysis_cache_status(
                "Wait for the current operation before clearing the cache.".into(),
                cx,
            );
            return;
        }
        let media = this.data.current_cache_media();
        if media.is_empty() {
            this.set_analysis_cache_status(
                "Analyze this project before clearing its cache.".into(),
                cx,
            );
            return;
        }
        let cleared = cx
            .background_executor()
            .spawn(async move { align_core::FingerprintCache::new(None).clear_media(&media) });
        cx.spawn(async move |view, cx| {
            let removed = cleared.await;
            let status = if removed.file_count == 0 {
                "Current project analysis cache is already empty.".into()
            } else {
                format!(
                    "Cleared {} cached analysis file{} for this project ({:.1} MB).",
                    removed.file_count,
                    if removed.file_count == 1 { "" } else { "s" },
                    removed.total_bytes as f64 / 1_048_576.0
                )
            };
            let _ = view.update(cx, |this, cx| this.set_analysis_cache_status(status, cx));
        })
        .detach();
    });
}

fn set_cache_retention(days: Option<u64>, cx: &mut App) {
    update_view(cx, |this, cx| {
        let mut settings = CacheSettings::load();
        settings.retention_days = days;
        let status = match settings.save() {
            Ok(()) => match days {
                Some(days) => format!("Analysis cache retention set to {days} days."),
                None => "Analysis cache retention set to Keep Until Cleared.".into(),
            },
            Err(error) => format!("Could not save analysis cache settings: {error}"),
        };
        this.refresh_project_menus(cx);
        this.set_analysis_cache_status(status, cx);
    });
}

fn toggle_analysis_cache(cx: &mut App) {
    update_view(cx, |this, cx| {
        let mut settings = CacheSettings::load();
        settings.enabled = !settings.enabled;
        let status = match settings.save() {
            Ok(()) if settings.enabled => "On-disk analysis cache enabled for new runs.".into(),
            Ok(()) => "On-disk analysis cache disabled for new runs.".into(),
            Err(error) => format!("Could not save analysis cache settings: {error}"),
        };
        this.refresh_project_menus(cx);
        this.set_analysis_cache_status(status, cx);
    });
}

fn update_view(
    cx: &mut App,
    update: impl FnOnce(&mut views::AlignApp, &mut gpui::Context<views::AlignApp>),
) {
    let Some(view) = cx.try_global::<AppView>().map(|app| app.0.clone()) else {
        return;
    };
    view.update(cx, |view, cx| {
        if !view.project_busy() {
            update(view, cx);
        }
    });
}

fn set_appearance(appearance: theme::AppearancePreference, cx: &mut App) {
    update_view(cx, |this, cx| {
        appearance.save();
        this.data.appearance = appearance;
        this.refresh_project_menus(cx);
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

fn app_menus(
    appearance: theme::AppearancePreference,
    projects: &project::Preferences,
) -> Vec<Menu> {
    let recent: Vec<_> = projects
        .recent
        .iter()
        .map(|entry| MenuItem::action(entry.label.clone(), OpenRecentProject(entry.path.clone())))
        .collect();
    let cache = CacheSettings::load();
    let toggle_cache = if cache.enabled {
        "Disable On-Disk Analysis Cache"
    } else {
        "Enable On-Disk Analysis Cache"
    };
    vec![
        Menu {
            name: "Align".into(),
            items: vec![
                MenuItem::action("About Align", AboutAlign),
                MenuItem::action("Check for Updates…", CheckForUpdates),
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
                MenuItem::action("Open Project…", OpenProject),
                MenuItem::submenu(Menu {
                    name: "Recent Projects".into(),
                    items: recent,
                }),
                MenuItem::action("Save Project", SaveProject),
                MenuItem::action("Save As…", SaveProjectAs),
                MenuItem::separator(),
                MenuItem::action("Add Media…", AddMedia),
                MenuItem::action("Reload and Synchronize", ReloadAndSynchronize),
                MenuItem::separator(),
                MenuItem::action("Export…", ExportTimeline),
                MenuItem::separator(),
                MenuItem::action("Path Fixer…", OpenPathFixer),
                MenuItem::submenu(Menu {
                    name: "Project Autosave".into(),
                    items: vec![
                        MenuItem::action(
                            if projects.disabled { "On" } else { "✓ On" },
                            EnableProjectAutosave,
                        ),
                        MenuItem::action(
                            if projects.disabled { "✓ Off" } else { "Off" },
                            DisableProjectAutosave,
                        ),
                        MenuItem::separator(),
                        MenuItem::action("Choose Folder…", ChooseProjectAutosaveFolder),
                    ],
                }),
                MenuItem::submenu(Menu {
                    name: "Analysis Cache".into(),
                    items: vec![
                        MenuItem::action(toggle_cache, ToggleAnalysisCache),
                        MenuItem::separator(),
                        MenuItem::action("Clear Current Project", ClearCurrentAnalysisCache),
                        MenuItem::action("Clear All", ClearAnalysisCache),
                        MenuItem::separator(),
                        MenuItem::action(
                            if cache.retention_days == Some(7) {
                                "✓ Keep for 7 Days"
                            } else {
                                "Keep for 7 Days"
                            },
                            KeepAnalysisCache7Days,
                        ),
                        MenuItem::action(
                            if cache.retention_days == Some(30) {
                                "✓ Keep for 30 Days"
                            } else {
                                "Keep for 30 Days"
                            },
                            KeepAnalysisCache30Days,
                        ),
                        MenuItem::action(
                            if cache.retention_days == Some(90) {
                                "✓ Keep for 90 Days"
                            } else {
                                "Keep for 90 Days"
                            },
                            KeepAnalysisCache90Days,
                        ),
                        MenuItem::action(
                            if cache.retention_days.is_none() {
                                "✓ Keep Until Cleared"
                            } else {
                                "Keep Until Cleared"
                            },
                            KeepAnalysisCacheForever,
                        ),
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
            name: "Help".into(),
            items: vec![MenuItem::action("Report a Problem…", ReportProblem)],
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
    #[cfg(unix)]
    if std::env::args_os().next().is_some_and(|arg| {
        std::path::Path::new(&arg)
            .file_name()
            .is_some_and(|name| name == "align-cli")
    }) {
        align_cli::main_entry();
    }
    #[cfg(windows)]
    windows_association::run_installer_hooks();

    struct SessionCleanup;
    impl Drop for SessionCleanup {
        fn drop(&mut self) {
            align_decode::media_assets::cleanup();
        }
    }
    let _session_cleanup = SessionCleanup;
    let projects = project::Preferences::load(&settings::file());
    let writer = project::Writer::default();
    let project_writer = writer.clone();
    let mut initial_paths: Vec<std::path::PathBuf> = std::env::args_os()
        .skip(1)
        .map(std::path::PathBuf::from)
        .filter(|path| {
            // Launchers may pass empty or option-like arguments; neither is media.
            let text = path.as_os_str().to_string_lossy();
            !text.is_empty() && !text.starts_with('-')
        })
        .collect();
    // Finder can deliver file URLs either before the first window exists
    // or to an already running instance.
    let open_events = std::rc::Rc::new(std::cell::RefCell::new((
        None::<gpui::AsyncApp>,
        Vec::new(),
    )));
    let launch_events = open_events.clone();
    let application = Application::new().with_assets(icons::EmbeddedAssets);
    application.on_open_urls(move |urls| {
        let paths: Vec<_> = urls
            .iter()
            .filter_map(|url| url::Url::parse(url).ok()?.to_file_path().ok())
            .filter(|path| project::is_project(path))
            .collect();
        let mut events = open_events.borrow_mut();
        if let Some(cx) = events.0.as_ref() {
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| update_view(cx, |this, cx| this.add_paths(paths, cx)));
            })
            .detach();
        } else {
            events.1.extend(paths);
        }
    });
    application.run(move |cx: &mut App| {
        text_input::init(cx);
        // Covers every graceful termination path, including an OS-level
        // quit that does not dispatch our custom QuitApp action.
        cx.on_app_quit(move |_| {
            // Drain the bounded save queue before GPUI starts its short
            // shutdown timeout. The actual file I/O belongs to the writer.
            project_writer.flush();
            async { align_decode::media_assets::cleanup() }
        })
        .detach();
        // System commands: without registered actions + bindings + menus
        // macOS swallows keys like Cmd+Q and the menu bar stays empty.
        cx.on_action(|_: &AboutAlign, cx| show_about(cx));
        cx.on_action(|_: &ReportProblem, cx| {
            update_view(cx, |this, cx| this.show_problem_report(cx));
        });
        cx.on_action(|_: &CheckForUpdates, cx| check_for_updates(cx));
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
        cx.on_action(|_: &ToggleAnalysisCache, cx| toggle_analysis_cache(cx));
        cx.on_action(|_: &AddMedia, cx| update_view(cx, |this, cx| this.add_media(cx)));
        cx.on_action(|_: &OpenProject, cx| update_view(cx, |this, cx| this.pick_project(cx)));
        cx.on_action(|action: &OpenRecentProject, cx| {
            update_view(cx, |this, cx| this.open_project(action.0.clone(), cx));
        });
        cx.on_action(|_: &SaveProject, cx| {
            update_view(cx, |this, cx| this.save_project(false, cx))
        });
        cx.on_action(|_: &SaveProjectAs, cx| {
            update_view(cx, |this, cx| this.save_project(true, cx))
        });
        cx.on_action(|_: &EnableProjectAutosave, cx| {
            update_view(cx, |this, cx| this.set_autosave(true, cx))
        });
        cx.on_action(|_: &DisableProjectAutosave, cx| {
            update_view(cx, |this, cx| this.set_autosave(false, cx))
        });
        cx.on_action(|_: &ChooseProjectAutosaveFolder, cx| {
            update_view(cx, |this, cx| this.choose_autosave_folder(cx));
        });
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
            cx.quit();
        });
        cx.on_action(|_: &HideApp, cx| cx.hide());
        cx.on_action(|_: &HideOthersApp, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
        // "secondary" is Cmd on macOS and Ctrl elsewhere; "cmd" alone
        // would be the Windows/Super key on other platforms.
        cx.bind_keys([
            KeyBinding::new("secondary-q", QuitApp, None),
            KeyBinding::new("secondary-o", AddMedia, None),
            KeyBinding::new("secondary-shift-o", OpenProject, None),
            KeyBinding::new("secondary-s", SaveProject, None),
            KeyBinding::new("secondary-shift-s", SaveProjectAs, None),
            KeyBinding::new("secondary-r", ReloadAndSynchronize, None),
            KeyBinding::new("secondary-e", ExportTimeline, None),
        ]);
        if cfg!(target_os = "macos") {
            cx.bind_keys([
                KeyBinding::new("cmd-h", HideApp, None),
                KeyBinding::new("alt-cmd-h", HideOthersApp, None),
                KeyBinding::new("cmd-m", MinimizeWindow, None),
                KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
            ]);
        } else {
            cx.bind_keys([KeyBinding::new("f11", ToggleFullscreen, None)]);
        }
        cx.set_menus(app_menus(theme::AppearancePreference::load(), &projects));
        {
            let mut events = launch_events.borrow_mut();
            initial_paths.append(&mut events.1);
            events.0 = Some(cx.to_async());
        }

        let bounds = Bounds::centered(None, size(px(1120.), px(700.)), cx);
        let view = cx.new(move |cx| {
            let mut app = views::AlignApp::new(projects, writer, cx);
            app.add_paths(initial_paths, cx);
            app.check_for_updates_on_launch(cx);
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
            move |_, _| view,
        ) {
            Ok(window) => window,
            Err(_) => {
                eprintln!("error: could not open application window");
                std::process::exit(1);
            }
        };
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
