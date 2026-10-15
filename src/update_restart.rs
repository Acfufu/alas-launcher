//! Webui-requested "pull & restart" — the 更新器 tab's restart-to-update path.
//!
//! Channel: the patched-in control API (assets/patches/control_api.py)
//! writes `config/.update-restart-request`; this module's watcher polls for
//! it. The launcher runs no HTTP server, so a file is the lowest-common
//! signal. Lifecycle contract: requests are honored only while THIS launcher
//! instance lives — `setup_alas_repo` clears stale files at startup (a
//! request written while the user was quitting must never fire on the next
//! launch), and the watcher consumes the file before acting.
//!
//! The worker mirrors tray.rs `spawn_start_worker`'s stop→begin_start→start
//! shape, with `setup_alas_repo` (git pull + sparse checkout + control
//! patch) re-run between stop and start: the restart IS the update.
//! Cross-platform (no tray assumptions); macOS additionally wakes the tray
//! poll via the `refresh` channel so the menu re-renders promptly.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc,
};
use std::time::Duration;

use tauri::{AppHandle, Manager, Url};
use tracing::{error, info, warn};

use crate::backend::{BackendLifecycle, BackendStatus};

/// One update-restart at a time, process-wide. Held by the worker; the
/// watcher only consumes the request file when it can acquire it, so a
/// request arriving mid-restart stays on disk for the next tick.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Poll cadence: fast enough to feel immediate after the webui click, slow
/// enough that the loop never shows up in anything.
const WATCH_INTERVAL: Duration = Duration::from_secs(3);

fn request_path() -> PathBuf {
    request_path_under(&crate::setup::alas_repo_dir())
}

/// Pure shape of [`request_path`], testable without a portable layout.
fn request_path_under(root: &std::path::Path) -> PathBuf {
    root.join("config").join(".update-restart-request")
}

/// Drop a request written by a previous launcher session. Called from
/// `setup_alas_repo` (before any backend exists), so a file can never be
/// consumed by the wrong run.
pub(crate) fn clear_stale_request() {
    clear_stale_request_at(&request_path());
}

fn clear_stale_request_at(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => info!("update-restart: dropped stale request {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("update-restart: could not remove stale request: {e}"),
    }
}

/// Status/error page for the main window while the webui is down (same
/// splash data-URL shape as main.rs `text_to_splash`): a pull that takes a
/// minute shows words instead of a dead webview.
fn status_url(title: &str, detail: &str) -> Url {
    let content = format!("{title}\n\n{detail}");
    Url::parse(&crate::text_to_splash(&content)).expect("status url parses")
}

fn navigate_main(app: &AppHandle, url: Url) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.navigate(url) {
            warn!("update-restart: navigate failed: {e}");
        }
    }
}

pub(crate) fn spawn_watcher(
    app: AppHandle,
    backend: Arc<BackendLifecycle>,
    port: u16,
    refresh: Option<Sender<()>>,
) {
    let builder = std::thread::Builder::new().name("update-restart-watch".into());
    if let Err(e) = builder.spawn(move || loop {
        std::thread::sleep(WATCH_INTERVAL);
        if backend.snapshot().status != BackendStatus::Running {
            continue;
        }
        if !request_path().exists() {
            continue;
        }
        // Acquire BEFORE consuming: while a restart runs, fresh requests
        // stay on disk; when the worker finishes it wakes this loop and the
        // next tick handles (or the next startup clears) the leftover.
        if IN_FLIGHT.swap(true, Ordering::SeqCst) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(request_path()) {
            warn!("update-restart: consume failed: {e}");
            IN_FLIGHT.store(false, Ordering::SeqCst);
            continue;
        }
        info!("update-restart: request consumed, restarting with update");
        spawn_worker(app.clone(), backend.clone(), port, refresh.clone());
    }) {
        warn!("update-restart watcher failed to spawn: {e}");
    }
}

/// Runs with IN_FLIGHT held; resets it on every exit path.
fn spawn_worker(
    app: AppHandle,
    backend: Arc<BackendLifecycle>,
    port: u16,
    refresh: Option<Sender<()>>,
) {
    let builder = std::thread::Builder::new().name("update-restart".into());
    if let Err(e) = builder.spawn(move || {
        navigate_main(
            &app,
            status_url(
                "Updating ALAS, please wait..",
                "Pulling the latest payload and restarting the backend.",
            ),
        );
        // ORDERING CONTRACT (backend.rs): stop() bumps the epoch BEFORE
        // begin_start(), so a second start can never run against the old
        // child.
        backend.stop();
        backend.begin_start();
        // The same setup the startup path runs: git pull, sparse checkout,
        // control patch. Progress goes to the log; the window shows the
        // status page above.
        if let Err(e) = crate::setup::setup_alas_repo(&|text: &str| info!("update-restart: {text}")) {
            error!("update-restart: setup failed: {e:#}");
            backend.mark_stopped();
            navigate_main(
                &app,
                status_url(
                    "Update failed",
                    &format!("Please run alas-launcher from terminal for detailed logs\n\n{e:#}"),
                ),
            );
            IN_FLIGHT.store(false, Ordering::SeqCst);
            if let Some(sender) = refresh {
                let _ = sender.send(());
            }
            return;
        }
        match backend.start(port, &|_| {}) {
            Ok(()) => {
                // Re-handshake the (freshly re-patched) control API.
                std::thread::spawn(move || crate::control_api::run_protocol_handshake(port));
                navigate_main(
                    &app,
                    Url::parse(&crate::backend::webui_url(port)).expect("webui url parses"),
                );
                if let Some(splash) = app.get_webview_window("splash") {
                    let _ = splash.destroy();
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                info!("update-restart: backend restarted with the latest payload");
            }
            Err(e) => {
                error!("update-restart: backend start failed: {e:#}");
                navigate_main(
                    &app,
                    status_url(
                        "Update failed",
                        &format!("The backend did not come back up\n\n{e:#}"),
                    ),
                );
            }
        }
        IN_FLIGHT.store(false, Ordering::SeqCst);
        if let Some(sender) = refresh {
            let _ = sender.send(());
        }
    }) {
        warn!("update-restart worker failed to spawn: {e}");
        IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request path must sit under config/ next to the payload root the
    /// py side writes to (module/webui/control_api.py: REPO_ROOT/config/...).
    #[test]
    fn request_path_lives_under_payload_config() {
        let path = request_path_under(std::path::Path::new("/payload"));
        assert_eq!(path.file_name().unwrap(), ".update-restart-request");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "config");
    }

    #[test]
    fn clear_stale_request_is_noop_without_file() {
        let tmp = std::env::temp_dir().join(format!("update-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        clear_stale_request_at(&tmp.join("config").join(".update-restart-request"));
        // And it actually removes an existing file.
        std::fs::create_dir_all(tmp.join("config")).unwrap();
        let path = tmp.join("config").join(".update-restart-request");
        std::fs::write(&path, "{}").unwrap();
        clear_stale_request_at(&path);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
