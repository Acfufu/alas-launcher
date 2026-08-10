// macOS system tray: template icon + native menu scaffold.
// Module is cfg-gated to macOS by `mod tray;` in main.rs, so this file never
// participates in win/linux builds.

use std::sync::{Arc, Mutex};

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};
use tracing::warn;

use crate::backend::ManagedBackend;

/// Build the macOS menu-bar tray icon with its native menu.
///
/// `backend`/`port` are placeholders for now: todo 4 (three-state backend
/// toggle) drives the status item text and the Start/Stop item from them.
/// A returned `Err` is warn-and-continue at the call site — a tray failure
/// must never abort app startup.
pub fn build_tray(
    app: &tauri::App,
    _backend: Arc<Mutex<Option<ManagedBackend>>>,
    _port: u16,
) -> tauri::Result<tauri::tray::TrayIcon> {
    // Static menu scaffold (all ids unique). Todo 4 re-labels/re-enables
    // tray-status / tray-toggle from the backend state.
    let status = MenuItem::with_id(app, "tray-status", "Backend: initializing", false, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "tray-toggle", "Start Backend", false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let refresh = MenuItem::with_id(app, "tray-refresh", "Refresh", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "tray-show", "Show Window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?;

    let items: [&dyn IsMenuItem<tauri::Wry>; 7] =
        [&status, &toggle, &separator, &refresh, &separator, &show, &quit];
    let menu = Menu::with_items(app, &items)?;

    TrayIconBuilder::new()
        .icon(tauri::include_image!("icons/tray-icon.png"))
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-toggle" => {
                // todo 4: three-state backend start/stop lives here.
                warn!("toggle stub (todo 4)");
            }
            "tray-refresh" => {
                warn!("refresh stub");
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
