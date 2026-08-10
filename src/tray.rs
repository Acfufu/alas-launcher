// macOS system tray: template icon + native menu with a three-state backend
// lifecycle toggle (BackendState shared with main.rs).
//
// This file is the tauri ADAPTER: it is the ONLY place `tauri::menu::*`
// types appear. All menu CONTENT (grouping, labels, section classification,
// change detection) lives in the pure `menu_model` module; this file only
// turns its rows into native items and orchestrates (build_tray, handle_toggle,
// poll_once, rebuild_menu, navigation).
// Module is cfg-gated to macOS by `mod tray;` in main.rs, so this file never
// participates in win/linux builds.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use base64::{prelude::BASE64_STANDARD, Engine};
use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, Url,
};
use tracing::warn;

use crate::{
    alas_tasks::{self, Task},
    backend::{toggle_decision, BackendLifecycle, BackendStateSnapshot, BackendStatus, ToggleAction},
    menu_model::{
        control_labels, poll_decision, status_text, task_section, task_section_items,
        toggle_enabled, toggle_label, ControlLabels, TaskMenuItem, TaskSection,
    },
};

/// Poll cadence for the task section; also bounds each idle wait of the poll
/// thread (a refresh signal wakes it early, never later than this).
const TRAY_POLL_INTERVAL_SECS: Duration = Duration::from_secs(10);

/// Everything the tray menu needs beyond the AppHandle: the backend state
/// (shared with main.rs), the cached task list, the resolved webui language
/// (read ONCE from deploy.yaml at build time — it selects the i18n file for
/// task display names), the stop flag (shared with main.rs so ExitRequested
/// can halt the poll thread) and the refresh signal channel (manual Refresh
/// menu item).
#[derive(Clone)]
struct TrayShared {
    backend: Arc<BackendLifecycle>,
    tasks: Arc<Mutex<Vec<Task>>>,
    language: String,
    stop: Arc<AtomicBool>,
    refresh: mpsc::Sender<()>,
}

/// Build the macOS menu-bar tray icon with its native menu.
///
/// `backend` is the shared lifecycle object (also owned by main.rs); `port`
/// is the ALAS webui port used for the main-page URL and the task fetch;
/// `stop` is the shared poll-thread stop flag (also owned by main.rs, set in
/// ExitRequested). A returned `Err` is warn-and-continue at the call site — a
/// tray failure must never abort app startup.
pub fn build_tray(
    app: &tauri::App,
    backend: Arc<BackendLifecycle>,
    port: u16,
    stop: Arc<AtomicBool>,
) -> tauri::Result<tauri::tray::TrayIcon> {
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();
    // deploy.yaml is read once here, not per poll cycle: the language only
    // changes on app restart anyway (Gui.Language).
    let language = deploy_language().unwrap_or_else(|| "zh-CN".to_string());
    let shared = TrayShared {
        backend,
        tasks: Arc::new(Mutex::new(Vec::new())),
        language,
        stop,
        refresh: refresh_tx,
    };

    let labels = load_control_labels();
    let menu = build_menu(
        app.handle(),
        &BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: false,
        },
        &[],
        TaskSection::Empty,
        &labels,
    )?;

    let thread_shared = shared.clone();
    let tray = TrayIconBuilder::with_id("main-tray")
        .icon(tauri::include_image!("icons/tray-icon.png"))
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "tray-toggle" => handle_toggle(app, &shared, port),
            "tray-refresh" => {
                // Wake the poll thread; it diffs before rebuilding, so a
                // manual refresh can never cause a needless rebuild/flicker.
                let _ = shared.refresh.send(());
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
        .build(app)?;

    // Poll thread: 10s cadence (or on-demand via refresh), diff-based rebuild
    // of the task section, degraded fallback while the backend is down or the
    // fetch fails. TrayIcon is Send + Sync, so set_menu from here is safe;
    // the thread owns a clone so the tray stays alive for its whole lifetime
    // (rebuild_menu reaches it through the app handle).
    let app_handle = app.handle().clone();
    let _thread_tray = tray.clone();
    std::thread::spawn(move || {
        let mut last_section = TaskSection::Empty;
        while !thread_shared.stop.load(Ordering::Relaxed) {
            match refresh_rx.recv_timeout(TRAY_POLL_INTERVAL_SECS) {
                Ok(()) => poll_once(&app_handle, &thread_shared, &mut last_section),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    poll_once(&app_handle, &thread_shared, &mut last_section)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    Ok(tray)
}

/// (Re)build the whole menu from the current backend state, task cache and
/// task-section rendering. The status item is always disabled; the toggle
/// item's text/enabled follow the state machine (Initializing disables it
/// entirely). Task items are read-only (disabled): group headers id
/// `group-running|queued|waiting`, task rows id `task-{i}`.
fn build_menu(
    app: &AppHandle,
    snapshot: &BackendStateSnapshot,
    tasks: &[Task],
    section: TaskSection,
    labels: &ControlLabels,
) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "tray-status", status_text(snapshot, labels), false, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        "tray-toggle",
        toggle_label(snapshot.status, labels),
        toggle_enabled(snapshot.status),
        None::<&str>,
    )?;
    let separator_after_toggle = PredefinedMenuItem::separator(app)?;

    let mut section_items: Vec<Box<dyn IsMenuItem<tauri::Wry>>> = Vec::new();
    match section {
        TaskSection::Degraded => {
            push_row(app, &mut section_items, "tasks-degraded".into(), "Tasks: unavailable")?
        }
        TaskSection::Empty => push_row(app, &mut section_items, "tasks-empty".into(), "No tasks")?,
        TaskSection::Tasks => {
            for item in task_section_items(tasks) {
                match item {
                    TaskMenuItem::GroupHeader { id, text } => {
                        push_row(app, &mut section_items, id, text)?
                    }
                    TaskMenuItem::TaskItem { id, text } => {
                        push_row(app, &mut section_items, id, &text)?
                    }
                    TaskMenuItem::Separator => {
                        section_items.push(Box::new(PredefinedMenuItem::separator(app)?));
                    }
                }
            }
        }
    }

    let refresh = MenuItem::with_id(app, "tray-refresh", "Refresh", true, None::<&str>)?;
    let separator_after_refresh = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "tray-show", "Show Window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?;

    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&status, &toggle, &separator_after_toggle];
    items.extend(
        section_items
            .iter()
            .map(|i| i.as_ref() as &dyn IsMenuItem<tauri::Wry>),
    );
    items.push(&refresh);
    items.push(&separator_after_refresh);
    items.push(&show);
    items.push(&quit);

    Menu::with_items(app, &items)
}

/// Push one disabled text row. Group headers, task rows and the degraded /
/// empty rows all render identically (disabled `MenuItem::with_id`) — the
/// only difference is the text's type (`&'static str` vs `String`), so every
/// text row routes through this single construction site.
fn push_row(
    app: &AppHandle,
    items: &mut Vec<Box<dyn IsMenuItem<tauri::Wry>>>,
    id: String,
    text: &str,
) -> tauri::Result<()> {
    items.push(Box::new(MenuItem::with_id(app, id, text, false, None::<&str>)?));
    Ok(())
}

/// Where the main window should point for a given backend status.
pub(crate) fn main_page_url(status: BackendStatus, port: u16, labels: &ControlLabels) -> Url {
    match status {
        BackendStatus::Running => Url::parse(&crate::backend::webui_url(port)).unwrap(),
        BackendStatus::Stopped | BackendStatus::Initializing => stopped_page_url(labels),
    }
}

/// Minimal inline page shown while the backend is not running; the copy is
/// localized via the labels table (sep "：" → zh template, else en template).
fn stopped_page_url(labels: &ControlLabels) -> Url {
    let text = if labels.sep == "：" {
        format!(
            "{}{}{}。点击菜单栏「{}」。",
            labels.scheduler, labels.sep, labels.stopped, labels.start
        )
    } else {
        format!(
            "{}{}{}. Click \"{}\" in the menu bar.",
            labels.scheduler, labels.sep, labels.stopped, labels.start
        )
    };
    let html = format!("<!doctype html><html><head><meta charset=\"utf-8\"><style>html,body{{height:100%;margin:0;display:flex;align-items:center;justify-content:center;background:#fff;color:#111;font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;}}p{{font-size:15px;}}</style></head><body><p>{text}</p></body></html>");
    let b64 = BASE64_STANDARD.encode(html.as_bytes());
    Url::parse(&format!("data:text/html;charset=utf-8;base64,{}", b64)).unwrap()
}

/// Full Start/Stop toggle for the backend.
///
/// The ordering contract (Metis BLOCKER-2: the old backend MUST be fully
/// terminated AND dropped before a new gui.py spawns) lives inside
/// [`BackendLifecycle::start`]; the toggle only snapshots, decides (via
/// [`toggle_decision`]) and delegates. Navigation decisions go through
/// [`main_page_url`] exactly as before.
fn handle_toggle(app: &AppHandle, shared: &TrayShared, port: u16) {
    // Snapshot the state and decide. Initializing -> NoOp (the item is
    // disabled anyway; this also makes a second click during a 60s start
    // window a no-op — BLOCKER-3: never two backends).
    let action = toggle_decision(&shared.backend.snapshot());
    let labels = load_control_labels();
    match action {
        ToggleAction::NoOp => return,
        ToggleAction::Stop => {
            shared.backend.stop();
            navigate_main(app, main_page_url(BackendStatus::Stopped, port, &labels));
        }
        ToggleAction::Start => {
            // Show "initializing…" (and make re-entry a no-op) for the whole
            // spawn window, which may take up to 60s.
            shared.backend.begin_start();
            match shared.backend.start(port) {
                Ok(()) => navigate_main(app, main_page_url(BackendStatus::Running, port, &labels)),
                Err(e) => {
                    warn!("Failed to start backend: {e}");
                    // Status is now Stopped + start_failed (set inside the
                    // module); the window stays on the stopped page and the
                    // next menu rebuild shows the localized failed label.
                }
            }
        }
    }
    // Rebuild must include the task section so a toggle never wipes it. The
    // section derives from the POST-action state (fresh snapshot + cached
    // tasks): a Stop shows the degraded section, a Start shows the cache.
    let tasks = shared.tasks.lock().unwrap().clone();
    rebuild_menu(app, shared, task_section(shared.backend.snapshot().status, Ok(tasks)));
}

/// Rebuild the menu from the current state + task cache and re-attach it to
/// the tray. The single set_menu site: the toggle handler and the poll thread
/// both route through here (never build-then-set inline).
fn rebuild_menu(app: &AppHandle, shared: &TrayShared, section: TaskSection) {
    let snapshot = shared.backend.snapshot();
    let tasks = shared.tasks.lock().unwrap().clone();
    let labels = load_control_labels();
    let Ok(menu) = build_menu(app, &snapshot, &tasks, section, &labels) else {
        return;
    };
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_menu(Some(menu));
    }
}

/// One poll cycle: snapshot the status (no network under the lock), fetch
/// outside any lock when Running, then let the pure decision in
/// [`poll_decision`] pick the section / rebuild / cache replacement.
/// `last_section` tracks what the menu currently renders.
fn poll_once(app: &AppHandle, shared: &TrayShared, last_section: &mut TaskSection) {
    // (a) status snapshot — no I/O under the backend lock (the lock is
    // internal to BackendLifecycle; status() takes it briefly).
    let status = shared.backend.status();

    // (b/c) Running -> fetch outside any lock; anything else -> degraded.
    // A failed clock (now_str Err) also degrades — never a silent
    // all-Waiting menu (the 9999-12-31 sentinel bug). Tasks are read from the
    // payload files (config/alas.json + i18n — the ALAS webui has no JSON
    // API, see alas_tasks module doc).
    let fetched: Result<Vec<Task>, ()> = if status == BackendStatus::Running {
        let alas_dir = std::env::current_dir().unwrap_or_default();
        match alas_tasks::now_str() {
            // Clock failure -> Err(()) like a fetch failure (now_str already
            // logged the real error); poll_decision degrades the section.
            Ok(now) => alas_tasks::fetch_tasks(&alas_dir, &now, &shared.language).map_err(|_| ()),
            Err(_) => Err(()),
        }
    } else {
        Err(())
    };

    // Decision is pure (menu_model::poll_decision): the clock string and the
    // fetch result are injected, so this call site carries no decision logic.
    let outcome = poll_decision(status, fetched, *last_section, &shared.tasks.lock().unwrap());
    if let Some(tasks) = outcome.replace_cache {
        *shared.tasks.lock().unwrap() = tasks;
    }
    if outcome.changed {
        rebuild_menu(app, shared, outcome.section);
        *last_section = outcome.section;
    }
}

/// Webui language from `config/deploy.yaml` (`Gui.Language`), which selects
/// the i18n file for task display names. zh-CN fallback — the payload default
/// (verified live: deploy.yaml `Language: zh-CN`).
fn deploy_language() -> Option<String> {
    crate::setup::get_deploy_config()?
        .get("Gui")?
        .get("Language")?
        .as_str()
        .map(String::from)
}

/// Tray control labels resolved from deploy.yaml language + the ALAS i18n
/// file (current dir); missing payload → built-in zh-CN table, never panic.
fn load_control_labels() -> ControlLabels {
    let lang = deploy_language().unwrap_or_else(|| "zh-CN".to_string());
    let alas_dir = std::env::current_dir().unwrap_or_default();
    let i18n = alas_tasks::load_i18n(&alas_dir, &lang);
    control_labels(&lang, &i18n)
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

    /// zh-CN builtin labels (no payload present) for the URL tests.
    fn zh_labels() -> ControlLabels {
        control_labels("zh-CN", &serde_json::json!({}))
    }

    #[test]
    fn main_page_url_running_is_webui() {
        let labels = zh_labels();
        assert_eq!(
            main_page_url(BackendStatus::Running, 22267, &labels),
            Url::parse("http://127.0.0.1:22267/").unwrap()
        );
    }

    #[test]
    fn main_page_url_stopped_and_initializing_are_data_pages() {
        let labels = zh_labels();
        let stopped = main_page_url(BackendStatus::Stopped, 22267, &labels);
        assert!(stopped.as_str().starts_with("data:text/html"));
        let initializing = main_page_url(BackendStatus::Initializing, 22267, &labels);
        assert!(initializing.as_str().starts_with("data:text/html"));
    }

    #[test]
    fn stopped_page_url_localizes_template_by_sep() {
        let decode = |url: &Url| {
            let b64 = url.as_str().split(',').nth(1).unwrap();
            String::from_utf8(BASE64_STANDARD.decode(b64).unwrap()).unwrap()
        };
        // Full-width "：" → zh template with 「」quotes.
        let zh = decode(&stopped_page_url(&control_labels("zh-CN", &serde_json::json!({}))));
        assert!(zh.contains("调度器：已停止。点击菜单栏「启动」。"));
        // Half-width ": " → en template (en-US and ja-JP share it).
        let en = decode(&stopped_page_url(&control_labels("en-US", &serde_json::json!({}))));
        assert!(en.contains("Scheduler: stopped. Click \"Start\" in the menu bar."));
        let ja = decode(&stopped_page_url(&control_labels("ja-JP", &serde_json::json!({}))));
        assert!(ja.contains("スケジューラー: 停止済み. Click \"実行\" in the menu bar."));
    }

    #[test]
    fn load_control_labels_missing_payload_falls_back_to_zh_cn() {
        // Empty dir: load_i18n → empty object and no deploy.yaml → zh-CN;
        // the full chain yields builtin zh-CN labels, never panicking.
        let empty_dir =
            std::env::temp_dir().join(format!("tray-behavior-align-{}", std::process::id()));
        let i18n = alas_tasks::load_i18n(&empty_dir, "zh-CN");
        assert_eq!(i18n, serde_json::json!({}));
        let labels = control_labels("zh-CN", &i18n);
        assert_eq!(labels.scheduler, "调度器");
        assert_eq!(labels.failed, "启动失败");
        assert_eq!(labels.sep, "：");
        // Live call must not panic regardless of ambient cwd.
        let live = load_control_labels();
        assert!(!live.scheduler.is_empty());
    }
}
