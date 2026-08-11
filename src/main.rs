// No default console window creation on Windows
#![windows_subsystem = "windows"]

// Consumed only by the macOS tray (tray.rs); gated so win/linux builds do not
// compile an unused module (clippy -D warnings).
#[cfg(target_os = "macos")]
mod alas_tasks;
mod backend;
// Pure tray menu model (no tauri). Gated to macOS only because its sole
// macOS-bound dependency, alas_tasks (above), is gated too — a plain
// declaration would break win/linux builds.
#[cfg(target_os = "macos")]
mod menu_model;
// PyWebIO WS protocol helpers for scheduler control; macOS-only because its
// sole consumer is the tray.
#[cfg(target_os = "macos")]
mod pywebio;
#[cfg(target_os = "macos")]
mod tray;
// Pure shell-settings persistence (no tauri); macOS-only until the settings
// menu (shell_menu.rs) lands — win/linux builds must not compile it.
#[cfg(target_os = "macos")]
mod shell_settings;
// macOS app menu bar shell-settings submenu (builds the 外壳设置 menu from
// ShellSettings + ShellMenuLabels); gated so win/linux builds stay untouched.
#[cfg(target_os = "macos")]
mod shell_menu;
mod setup;
mod window_util;

use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self},
};

use anyhow::{anyhow, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use tauri::{
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, Url, WebviewWindow,
};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tracing::{error, info, warn};

use crate::{
    backend::BackendLifecycle,
    setup::{get_deploy_config, setup_alas_repo, setup_environment},
};

fn main() -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
    }
    tracing_subscriber::fmt::init();
    setup_environment()?;

    let port = get_deploy_config()
        .as_ref()
        .and_then(|config| config.get("Deploy"))
        .and_then(|deploy| deploy.get("Webui"))
        .and_then(|webui| webui.get("WebuiPort"))
        .and_then(|port| port.as_u64());
    if port.is_none() {
        warn!("WebuiPort not found in config, using default port 22267");
    }
    let port = port.unwrap_or(22267) as u16;

    let backend = Arc::new(BackendLifecycle::default());
    let setup_backend = backend.clone();
    // Stop flag for the tray poll thread; set in ExitRequested so the thread
    // never calls set_menu on a disposed tray (Metis MAJOR-4).
    let tray_stop = Arc::new(AtomicBool::new(false));
    let setup_tray_stop = tray_stop.clone();

    // Shared shell settings (macOS app menu). Created in main() scope — the
    // setup closure (menu build + events) and the run closure (todo 6
    // auto-start Ready thread) are separate move closures, so each gets its
    // own clone; a local created inside setup would be invisible to the run
    // callback (plan MAJOR-1).
    #[cfg(target_os = "macos")]
    let shell_settings = Arc::new(std::sync::Mutex::new(crate::shell_settings::load()));
    #[cfg(target_os = "macos")]
    let setup_shell_settings = shell_settings.clone();
    #[cfg(target_os = "macos")]
    let run_shell_settings = shell_settings.clone();

    info!("Starting Webview...");
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![save_as])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = app
                .get_webview_window("main")
                .and_then(|w| w.set_focus().ok());
        }))
        .setup(move |app| {
            tauri::WebviewWindowBuilder::from_config(
                app,
                app.config()
                    .app
                    .windows
                    .iter()
                    .find(|w| w.label == "main")
                    .unwrap(),
            )?
            .on_page_load(page_load_injector)
            .build()?;
            #[cfg(target_os = "macos")]
            let tray_refresh: Option<std::sync::mpsc::Sender<()>> =
                match crate::tray::build_tray(
                    app,
                    setup_backend.clone(),
                    port,
                    setup_tray_stop.clone(),
                    setup_shell_settings.clone(),
                ) {
                    // build_tray returns the poll-thread refresh channel (todo
                    // 4 wake mechanism): the language handler below sends on
                    // it to force a tray rebuild in the new language.
                    Ok((_tray, refresh)) => Some(refresh),
                    Err(e) => {
                        warn!("tray failed: {e}");
                        None
                    }
                };
            #[cfg(target_os = "macos")]
            {
                // App menu bar: build the 外壳设置 menu from the shared
                // settings + localized labels, install it, and KEEP the item
                // handles — a live language switch relabels the installed
                // native items in place (locked tauri 2.5.1 exposes no
                // AppHandle::set_menu; see shell_menu.rs module doc for the
                // full design rationale). warn-and-continue — a menu failure
                // must never abort startup (mirrors the build_tray call site).
                let labels = crate::menu_model::shell_menu_labels(
                    &setup_shell_settings
                        .lock()
                        .unwrap()
                        .resolved_language(deploy_language().as_deref()),
                );
                let menu_handles: Option<crate::shell_menu::SettingsMenuHandles> =
                    match crate::shell_menu::build_settings_menu(
                        app.handle(),
                        &setup_shell_settings,
                        &labels,
                    ) {
                        Ok(handles) => {
                            if let Err(e) = app.set_menu(handles.menu()) {
                                warn!("settings menu failed: {e}");
                            }
                            Some(handles)
                        }
                        Err(e) => {
                            warn!("settings menu failed: {e}");
                            None
                        }
                    };
                // App-level menu events (settings-* ids). Independent from the
                // tray's own on_menu_event (tray-* ids, tray.rs:119).
                // settings-lang-* is the todo-4 LIVE language switch: main.rs
                // owns deploy_language(), the backend, the port and the tray
                // refresh sender, so it orchestrates here and shell_menu.rs
                // stays menu-build + label-computation only.
                let menu_shell_settings = setup_shell_settings.clone();
                let menu_backend = setup_backend.clone();
                app.on_menu_event(move |app, event| match event.id().as_ref() {
                    "settings-check-update" => warn!("check-update handler wired in todo 5"),
                    "settings-auto-start" => warn!("auto-start handler wired in todo 6"),
                    id if id.starts_with("settings-lang-") => {
                        // (a) Apply the click: mutate the shared language +
                        // persist (in-memory wins on save failure), returns
                        // the updated settings for label computation.
                        let updated =
                            crate::shell_menu::handle_language_click(&menu_shell_settings, id);
                        // (b) Relabel + re-check the INSTALLED app menu in
                        // place (muda handles update the macOS bar live).
                        let labels = crate::menu_model::shell_menu_labels(
                            &updated.resolved_language(deploy_language().as_deref()),
                        );
                        if let Some(handles) = &menu_handles {
                            if let Err(e) = handles.apply_labels(&labels, &updated) {
                                warn!("settings menu relabel failed: {e}");
                            }
                        }
                        // (c) Wake the tray poll thread — a woken poll ALWAYS
                        // rebuilds (force_rebuild path, tray.rs), so the tray
                        // menu re-renders in the new language even when the
                        // task section is unchanged.
                        if let Some(refresh) = &tray_refresh {
                            let _ = refresh.send(());
                        }
                        // (d) The stopped-page copy is baked into its data:
                        // URL at build time — re-navigate it when the backend
                        // is not Running (the webui URL is language-free).
                        crate::tray::refresh_stopped_page(
                            app,
                            &menu_backend,
                            &menu_shell_settings,
                            port,
                        );
                    }
                    _ => {}
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())?
        .run(move |app_handle, event| {
            match event {
                tauri::RunEvent::Ready => {
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        info!("Received Ctrl-C, shutting down...");
                        handle1.exit(0);
                    }).expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    // todo 6: auto-start backend gate — the Ready-thread
                    // branch reads this shared settings clone (MAJOR-1: the
                    // setup closure's clone is invisible here; the run
                    // callback is FnMut so we clone, not move).
                    #[cfg(target_os = "macos")]
                    let _shell_settings = run_shell_settings.clone();
                    thread::spawn(move || {
                        backend.begin_start();
                        let splash = app_handle.get_webview_window("splash").unwrap();
                        let status_updater = |text: &str| {
                            let content = format!("Loading ALAS, please wait..\n\n{}", text);
                            let url = Url::parse(&text_to_splash(&content)).unwrap();
                            splash.navigate(url).unwrap();
                        };
                        status_updater("Initialize ALAS");
                        if let Err(e) = setup_alas_repo(&status_updater) {
                            error!("{e}");
                            let content = format!("Failed loading ALAS, reason: {}\n\nPlease run alas-launcher from terminal for detailed logs", e);
                            let url = Url::parse(&text_to_splash(&content)).unwrap();
                            splash.navigate(url).unwrap();
                            // Init failure must not leave the state machine stuck in
                            // Initializing (toggle disabled forever): mark plain
                            // Stopped. This is a setup failure, not a
                            // backend-start failure — start_failed stays false.
                            backend.mark_stopped();
                            return;
                        }
                        info!("Starting gui.py on {}", crate::backend::webui_url(port));
                        status_updater("Starting GUI");
                        if let Err(e) = backend.start(port) {
                            // Same as today: the splash stays on the "Starting GUI"
                            // page, main window stays hidden. The module set
                            // Stopped + start_failed so the tray shows the
                            // localized start-failed label.
                            error!("Failed to start backend: {e}");
                        } else {
                            splash.destroy().unwrap();
                            info!("Webview is ready");
                            let window = app_handle.get_webview_window("main").unwrap();
                            window
                                .navigate(Url::parse(&crate::backend::webui_url(port)).unwrap())
                                .unwrap();
                            window.show().unwrap();
                        }
                    });
                }
                tauri::RunEvent::ExitRequested { .. } => {
                    info!("Webview closed, shutting down backend...");
                    // Stop the tray poll thread BEFORE terminating the backend:
                    // it must never set_menu on a disposed tray (Metis MAJOR-4).
                    tray_stop.store(true, Ordering::Relaxed);
                    backend.stop();
                }
                tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { .. }, .. } => {
                    info!("Window {} closed", label);
                    app_handle.exit(0);
                }
                _ => {}
            };
        });
    Ok(())
}
/// Webui language from `config/deploy.yaml` (`Gui.Language`), which selects
/// the label tables; zh-CN fallback via `ShellSettings::resolved_language`.
/// Replicated from the private `tray::deploy_language` (tray.rs:615) so this
/// todo's commit stages only shell_menu.rs + main.rs (decision recorded in
/// evidence task-3-shell-settings-menu.md).
#[cfg(target_os = "macos")]
fn deploy_language() -> Option<String> {
    get_deploy_config()?
        .get("Gui")?
        .get("Language")?
        .as_str()
        .map(String::from)
}

#[tauri::command]
fn save_as(app_handle: tauri::AppHandle, filename: &str, data: &str) {
    match BASE64_STANDARD.decode(data) {
        Ok(decoded_data) => app_handle
            .dialog()
            .file()
            .set_file_name(filename)
            .save_file(move |path| {
                let result: Result<()> = (move || {
                    let file_path = path
                        .as_ref()
                        .and_then(FilePath::as_path)
                        .ok_or_else(|| anyhow!("Invalid file path {:?}", &path))?;
                    fs::write(file_path, &decoded_data)?;
                    info!("Saved file to {:?}", file_path);
                    Ok(())
                })();
                if let Err(e) = result {
                    error!("Failed to save file: {:?}", e);
                }
            }),
        Err(e) => {
            error!("Failed to decode file content: {:?}", e);
        }
    }
}

fn page_load_injector(webview: WebviewWindow, payload: PageLoadPayload<'_>) {
    if payload.event() == PageLoadEvent::Finished {
        info!(
            "Injecting saveFile function to loaded page: {}",
            payload.url()
        );
        let injected_js = r#"
if (!window.alas_launcher_injected) {
    window.alas_launcher_injected = true;
    (function () {
        // Prevent going back
        history.pushState(null, document.title, location.href);
        window.addEventListener('popstate', event => {
            history.pushState(null, document.title, location.href);
        });
        // Overwrite original saveAs function
        window.saveAs = function (blob, filename) {
            const reader = new FileReader();
            reader.onload = async () => {
                const data = reader.result.split(',')[1];
                window.__TAURI__.core.invoke('save_as', { filename, data });
            };
            reader.readAsDataURL(blob);
        };
    })();
}
"#;
        if let Err(e) = webview.eval(injected_js) {
            error!("Failed to inject JS to webview: {:?}", e);
        }
    }
}

fn text_to_splash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => {} // drop CR, keep LF
            other => out.push(other),
        }
    }
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
  /* fill viewport and hide any scrollbars */
  html,body{{height:100%;margin:0;padding:0;overflow:hidden;background:#fff;color:#111;font-family:system-ui,-apple-system,Segoe UI,Roboto,"Helvetica Neue",Arial;}}
  /* make PRE fill the whole page, add inner padding, clip overflow (no scrollbars) */
  pre{{position:fixed;inset:0;margin:0;padding:20px;box-sizing:border-box;background:#f6f8fa;overflow:hidden;white-space:pre-wrap;word-break:break-word;font-family:Menlo,monospace;font-size:13px;line-height:1.45;}}
  /* remove default focus outlines or user agent scrollbars if present */
  ::-webkit-scrollbar{{display:none;}}
</style>
</head>
<body><pre>{}</pre></body>
</html>"#,
        out
    );

    let b64 = BASE64_STANDARD.encode(html.as_bytes());
    format!("data:text/html;charset=utf-8;base64,{}", b64)
}
