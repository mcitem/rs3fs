#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]

mod services;
mod ui;

use services::{startup, sync_service};
#[cfg(windows)]
use ui::tray;
use ui::{appearance, browser, zoom};

use gpui_kit::*;
use std::{borrow::Cow, sync::Arc};

gpui_kit::assets::icon_assets!(
    FileBrowserIcons,
    [
        ArrowUpDown,
        Braces,
        CodeXml,
        Cog,
        Clock,
        Cloud,
        Container,
        Database,
        Download,
        FileArchive,
        FileBraces,
        FileCode,
        FileCog,
        FileDigit,
        FileImage,
        FileKey,
        FileLock,
        FileMusic,
        FilePlus,
        FileSpreadsheet,
        FileTerminal,
        FileType,
        FileVideoCamera,
        FolderInput,
        FolderPlus,
        FolderUp,
        HardDrive,
        GitBranch,
        List,
        LayoutGrid,
        LogOut,
        Maximize,
        Minimize,
        MousePointer,
        Package,
        Pencil,
        Presentation,
        ShieldCheck,
        SquareMinus,
        Trash,
        Type,
        Upload,
        X
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        match FileBrowserIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(FileBrowserIcons.list(path)?);
        Ok(paths)
    }
}

fn register_preview_grammars() {
    use gpui_kit::component::highlighter::{GrammarConfig, LanguageRegistry};
    let registry = LanguageRegistry::singleton();
    registry.register(
        "dockerfile",
        &GrammarConfig::new(
            "Dockerfile",
            tree_sitter_containerfile::LANGUAGE.into(),
            Vec::new(),
            tree_sitter_containerfile::HIGHLIGHTS_QUERY,
            tree_sitter_containerfile::INJECTIONS_QUERY,
            "",
        ),
    );
    registry.register(
        "powershell",
        &GrammarConfig::new(
            "PowerShell",
            tree_sitter_powershell::LANGUAGE.into(),
            Vec::new(),
            tree_sitter_powershell::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
    );
    registry.register(
        "xml",
        &GrammarConfig::new(
            "XML",
            tree_sitter_xml::LANGUAGE_XML.into(),
            Vec::new(),
            tree_sitter_xml::XML_HIGHLIGHT_QUERY,
            "",
            "",
        ),
    );
    // The bundled TSX query only covers TypeScript-specific nodes.
    if let Some(javascript) = registry.language("javascript") {
        let highlights = format!(
            "{}\n{}\n(jsx_opening_element name: (identifier) @tag)\n(jsx_closing_element name: (identifier) @tag)\n(jsx_self_closing_element name: (identifier) @tag)",
            javascript.highlights,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        );
        registry.register(
            "tsx",
            &GrammarConfig::new(
                "TSX",
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                Vec::new(),
                &highlights,
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            ),
        );
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(startup::REMOVE_STARTUP_ARG))
    {
        return startup::remove_own_registration();
    }

    #[cfg(windows)]
    let background = std::env::args_os()
        .skip(1)
        .any(|arg| arg == startup::BACKGROUND_ARG);
    #[cfg(windows)]
    let mut tray = match tray::Tray::start(background) {
        Ok(Some(tray)) => tray,
        Ok(None) => return Ok(()),
        Err(error) => {
            if !background {
                tray::show_error(&format!("{error:#}"));
            }
            return Err(error);
        }
    };

    let runtime = Arc::new(tokio::runtime::Runtime::new()?);
    let app = gpui_kit::application().with_assets(AppAssets);
    #[cfg(windows)]
    let app = app.with_quit_mode(QuitMode::Explicit);
    #[cfg(windows)]
    let app = if background {
        app.with_windowing(WindowingRequest::Headless)
    } else {
        app
    };
    app.run(move |cx| {
        let sync = cx.new(|cx| sync_service::SyncService::new(runtime.clone(), cx));
        #[cfg(not(windows))]
        {
            init_ui(cx);
            open_browser(runtime, sync, cx).expect("无法打开应用窗口");
            cx.activate(true);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        }
        #[cfg(windows)]
        {
            let mut initialized = false;
            let mut browser = None;
            if !background {
                init_ui(cx);
                initialized = true;
                match open_browser(runtime.clone(), sync.clone(), cx) {
                    Ok(view) => browser = Some(view.downgrade()),
                    Err(error) => tray::show_error(&format!("无法打开应用窗口：{error:#}")),
                }
            }
            cx.spawn(async move |cx| {
                while let Some(event) = tray.next_event().await {
                    match event {
                        tray::Event::Exit => {
                            let quitting = sync.clone();
                            let prompted = browser.as_ref().is_some_and(|view| {
                                view.update(cx, |browser, cx| {
                                    browser.confirm_preview_leave(cx, move |_, _, cx| {
                                        quitting.update(cx, |service, cx| service.request_quit(cx));
                                    })
                                })
                                .unwrap_or(false)
                            });
                            if !prompted {
                                sync.update(cx, |service, cx| service.request_quit(cx));
                            }
                        }
                        tray::Event::Open => {
                            let Some(request) = cx.update(|cx| {
                                if sync.read(cx).is_quitting() {
                                    return None;
                                }
                                if let Some(window) = cx.windows().first() {
                                    let _ = window.update(cx, |_, window, _| {
                                        tray::set_window_visible(window, true);
                                        window.activate_window();
                                    });
                                    return None;
                                }
                                Some((cx.graphical_environment().is_none()).then(|| {
                                    cx.request_windowing(WindowingRequest::Windowed(
                                        GraphicalEnvironment::detect(),
                                    ))
                                }))
                            }) else {
                                continue;
                            };
                            if let Some(request) = request
                                && let Err(error) = request.await
                            {
                                tray::show_error(&format!("无法启动图形界面：{error:#}"));
                                continue;
                            }
                            let result = cx.update(|cx| {
                                if !initialized {
                                    init_ui(cx);
                                    initialized = true;
                                }
                                open_browser(runtime.clone(), sync.clone(), cx)
                            });
                            match result {
                                Ok(view) => browser = Some(view.downgrade()),
                                Err(error) => {
                                    tray::show_error(&format!("无法打开应用窗口：{error:#}"))
                                }
                            }
                        }
                    }
                }
            })
            .detach();
        }
    });
    Ok(())
}

fn init_ui(cx: &mut App) {
    gpui_kit::init(cx);
    register_preview_grammars();
    zoom::init(cx);
    appearance::init(cx);
}

fn open_browser(
    runtime: Arc<tokio::runtime::Runtime>,
    sync: Entity<sync_service::SyncService>,
    cx: &mut App,
) -> anyhow::Result<Entity<browser::Browser>> {
    let (_, browser) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(720.)),
                cx,
            ))),
            window_min_size: Some(size(px(850.), px(460.))),
            titlebar: Some(TitlebarOptions {
                title: Some(env!("CARGO_PKG_NAME").into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |window, cx| {
            appearance::attach(window, cx);
            #[cfg(windows)]
            {
                tray::set_window_icon(window);
            }
            let browser = cx.new(|cx| browser::Browser::new(runtime, sync, window, cx));
            #[cfg(windows)]
            {
                let view = browser.downgrade();
                ui::touch::attach(window, cx, move |window, cx| {
                    let _ = view.update(cx, |browser, cx| {
                        browser.show_readonly_hint(window, cx);
                    });
                });
            }
            let view = browser.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                let close = |_: &mut browser::Browser,
                             window: &mut Window,
                             _: &mut Context<browser::Browser>| {
                    #[cfg(windows)]
                    tray::set_window_visible(window, false);
                    #[cfg(not(windows))]
                    window.remove_window();
                };
                if view
                    .update(cx, |browser, cx| browser.confirm_preview_leave(cx, close))
                    .unwrap_or(false)
                {
                    return false;
                }
                #[cfg(windows)]
                {
                    tray::set_window_visible(window, false);
                    false
                }
                #[cfg(not(windows))]
                true
            });
            browser
        },
    )?;
    Ok(browser)
}
