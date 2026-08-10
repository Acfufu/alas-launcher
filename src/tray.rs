// macOS system tray: template icon + native menu with a three-state backend
// lifecycle toggle (BackendState shared with main.rs).
// Module is cfg-gated to macOS by `mod tray;` in main.rs, so this file never
// participates in win/linux builds.

use std::sync::{Arc, Mutex};

use base64::{prelude::BASE64_STANDARD, Engine};
use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, Url,
};
use tracing::warn;

use crate::backend::{BackendState, BackendStatus, ManagedBackend};

/// Build the macOS menu-bar tray icon with its native menu.
///
/// `backend` is the shared three-state lifecycle object (also owned by
/// main.rs); `port` is the ALAS webui port used for the main-page URL.
/// A returned `Err` is warn-and-continue at the call site — a tray failure
/// must never abort app startup.
pub fn build_tray(
    app: &tauri::App,
    backend: Arc<Mutex<BackendState>>,
    port: u16,
) -> tauri::Result<tauri::tray::TrayIcon> {
    let menu = build_menu(app, &BackendState::default())?;

    TrayIconBuilder::with_id("main-tray")
        .icon(tauri::include_image!("icons/tray-icon.png"))
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "tray-toggle" => handle_toggle(app, &backend, port),
            "tray-refresh" => {
                // todo 6: poll + diff-rebuild of the task section lives here.
                warn!("refresh stub (todo 6)");
            }
            "tray-show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "tray-quit" => {
                // Triggers the existing ExitRequested -> backend terminate path.
                app.exit(0);
            }
            _ => {}
        })
        .build(app)
}

/// (Re)build the whole menu from the current backend state. The status item
/// is always disabled; the toggle item's text/enabled follow the state
/// machine (Initializing disables it entirely).
fn build_menu(
    app: &impl tauri::Manager<tauri::Wry>,
    state: &BackendState,
) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "tray-status", status_text(state), false, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        "tray-toggle",
        toggle_label(state.status),
        toggle_enabled(state.status),
        None::<&str>,
    )?;
    let separator_after_toggle = PredefinedMenuItem::separator(app)?;
    let refresh = MenuItem::with_id(app, "tray-refresh", "Refresh", true, None::<&str>)?;
    let separator_after_refresh = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "tray-show", "Show Window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?;

    let items: [&dyn IsMenuItem<tauri::Wry>; 7] = [
        &status,
        &toggle,
        &separator_after_toggle,
        &refresh,
        &separator_after_refresh,
        &show,
        &quit,
    ];
    Menu::with_items(app, &items)
}

fn status_text(state: &BackendState) -> String {
    if state.start_failed {
        "Backend: start failed".to_string()
    } else {
        label_for(state.status).to_string()
    }
}

pub(crate) fn label_for(status: BackendStatus) -> &'static str {
    match status {
        BackendStatus::Initializing => "Backend: initializing…",
        BackendStatus::Running => "Backend: running",
        BackendStatus::Stopped => "Backend: stopped",
    }
}

pub(crate) fn toggle_label(status: BackendStatus) -> &'static str {
    match status {
        BackendStatus::Running => "Stop Backend",
        BackendStatus::Stopped | BackendStatus::Initializing => "Start Backend",
    }
}

pub(crate) fn toggle_enabled(status: BackendStatus) -> bool {
    matches!(status, BackendStatus::Running | BackendStatus::Stopped)
}

/// Where the main window should point for a given backend status.
pub(crate) fn main_page_url(status: BackendStatus, port: u16) -> Url {
    match status {
        BackendStatus::Running => Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap(),
        BackendStatus::Stopped | BackendStatus::Initializing => stopped_page_url(),
    }
}

/// Minimal inline page shown while the backend is not running.
fn stopped_page_url() -> Url {
    let html = "<!doctype html><html><head><meta charset=\"utf-8\"><style>html,body{height:100%;margin:0;display:flex;align-items:center;justify-content:center;background:#fff;color:#111;font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;}p{font-size:15px;}</style></head><body><p>Backend stopped. Click Start in the menu bar.</p></body></html>";
    let b64 = BASE64_STANDARD.encode(html.as_bytes());
    Url::parse(&format!("data:text/html;charset=utf-8;base64,{}", b64)).unwrap()
}

/// Decision the toggle handler makes from the current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleAction {
    NoOp,
    Stop,
    Start,
}

pub(crate) fn toggle_decision(state: &BackendState) -> ToggleAction {
    match state.status {
        BackendStatus::Initializing => ToggleAction::NoOp,
        BackendStatus::Running => ToggleAction::Stop,
        BackendStatus::Stopped => ToggleAction::Start,
    }
}

/// Full Start/Stop toggle for the backend.
///
/// ORDERING CONTRACT (Metis BLOCKER-2): the old backend MUST be fully
/// terminated AND dropped BEFORE a new gui.py is spawned. ManagedBackend's
/// Drop scans every process for ALAS_LAUNCHER_PID and kills matches; if the
/// old backend were dropped after a new spawn, that scan would kill the
/// freshly spawned gui.py. Both paths below therefore order: take() out of
/// the shared state -> drop the Option (running the kill-all scan) -> only
/// then spawn.
fn handle_toggle(app: &AppHandle, backend: &Arc<Mutex<BackendState>>, port: u16) {
    // Snapshot the state and decide. Initializing -> NoOp (the item is
    // disabled anyway; this also makes a second click during a 60s start
    // window a no-op — BLOCKER-3: never two backends).
    let action = toggle_decision(&backend.lock().unwrap());
    match action {
        ToggleAction::NoOp => return,
        ToggleAction::Stop => {
            // ORDERING CONTRACT, stop path: take() -> status Stopped -> drop
            // the lock guard -> terminate() -> drop(old). terminate() kills
            // gui.py BEFORE the Option is dropped, so the Drop kill-all scan
            // runs with gui.py already dead.
            let mut old = {
                let mut state = backend.lock().unwrap();
                state.status = BackendStatus::Stopped;
                state.start_failed = false;
                state.backend.take()
            };
            if let Some(mut b) = old.take() {
                let _ = b.terminate();
            }
            drop(old);
            navigate_main(app, main_page_url(BackendStatus::Stopped, port));
        }
        ToggleAction::Start => {
            // ORDERING CONTRACT, start path: take + drop any lingering old
            // backend BEFORE ManagedBackend::new spawns the new gui.py.
            let old = {
                let mut state = backend.lock().unwrap();
                state.backend.take()
            };
            drop(old);
            {
                let mut state = backend.lock().unwrap();
                state.status = BackendStatus::Initializing;
                state.start_failed = false;
            }
            match ManagedBackend::new(port) {
                Ok(b) => {
                    {
                        let mut state = backend.lock().unwrap();
                        state.backend = Some(b);
                        state.status = BackendStatus::Running;
                        state.start_failed = false;
                    }
                    navigate_main(app, main_page_url(BackendStatus::Running, port));
                }
                Err(e) => {
                    warn!("Failed to start backend: {e}");
                    {
                        let mut state = backend.lock().unwrap();
                        state.status = BackendStatus::Stopped;
                        state.start_failed = true;
                    }
                }
            }
        }
    }
    rebuild_menu(app, backend);
}

/// Rebuild the menu from the current state and re-attach it to the tray.
fn rebuild_menu(app: &AppHandle, backend: &Arc<Mutex<BackendState>>) {
    let state = backend.lock().unwrap();
    if let Ok(menu) = build_menu(app, &state) {
        if let Some(tray) = app.tray_by_id("main-tray") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

/// Point the main window at `url` (errors are non-fatal).
fn navigate_main(app: &AppHandle, url: Url) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.navigate(url) {
            warn!("Failed to navigate main window: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_for_matrix() {
        assert_eq!(label_for(BackendStatus::Initializing), "Backend: initializing…");
        assert_eq!(label_for(BackendStatus::Running), "Backend: running");
        assert_eq!(label_for(BackendStatus::Stopped), "Backend: stopped");
    }

    #[test]
    fn toggle_label_matrix() {
        assert_eq!(toggle_label(BackendStatus::Running), "Stop Backend");
        assert_eq!(toggle_label(BackendStatus::Stopped), "Start Backend");
        assert_eq!(toggle_label(BackendStatus::Initializing), "Start Backend");
    }

    #[test]
    fn toggle_enabled_matrix() {
        assert!(toggle_enabled(BackendStatus::Running));
        assert!(toggle_enabled(BackendStatus::Stopped));
        assert!(!toggle_enabled(BackendStatus::Initializing));
    }

    #[test]
    fn main_page_url_running_is_webui() {
        assert_eq!(
            main_page_url(BackendStatus::Running, 22267),
            Url::parse("http://127.0.0.1:22267/").unwrap()
        );
    }

    #[test]
    fn main_page_url_stopped_and_initializing_are_data_pages() {
        let stopped = main_page_url(BackendStatus::Stopped, 22267);
        assert!(stopped.as_str().starts_with("data:text/html"));
        let initializing = main_page_url(BackendStatus::Initializing, 22267);
        assert!(initializing.as_str().starts_with("data:text/html"));
    }

    #[test]
    fn status_text_prefers_start_failed() {
        let stopped_failed = BackendState {
            status: BackendStatus::Stopped,
            backend: None,
            start_failed: true,
        };
        assert_eq!(status_text(&stopped_failed), "Backend: start failed");
        let stopped_clean = BackendState {
            status: BackendStatus::Stopped,
            backend: None,
            start_failed: false,
        };
        assert_eq!(status_text(&stopped_clean), "Backend: stopped");
        let running_clean = BackendState {
            status: BackendStatus::Running,
            backend: None,
            start_failed: false,
        };
        assert_eq!(status_text(&running_clean), "Backend: running");
    }

    #[test]
    fn toggle_decision_matrix() {
        let stopped = BackendState::default();
        assert_eq!(toggle_decision(&stopped), ToggleAction::Start);
        let running = BackendState {
            status: BackendStatus::Running,
            ..BackendState::default()
        };
        assert_eq!(toggle_decision(&running), ToggleAction::Stop);
        let initializing = BackendState {
            status: BackendStatus::Initializing,
            ..BackendState::default()
        };
        assert_eq!(toggle_decision(&initializing), ToggleAction::NoOp);
    }
}
