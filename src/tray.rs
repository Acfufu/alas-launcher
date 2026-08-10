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
    net::TcpStream,
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
        control_labels, poll_decision, scheduler_alive, status_line_for, task_section,
        task_section_items, toggle_enabled, toggle_label, ControlLabels, TaskMenuItem, TaskSection,
    },
    pywebio::{check_pywebio_version, click_scheduler, pywebio_version, SchedulerAction},
};

/// Poll cadence for the task section; also bounds each idle wait of the poll
/// thread (a refresh signal wakes it early, never later than this).
const TRAY_POLL_INTERVAL_SECS: Duration = Duration::from_secs(10);

/// Upper bound of one scheduler WS click session (the ~6s home quiesce
/// dominates the budget; plan todo 5 default).
const SCHEDULER_CLICK_TIMEOUT: Duration = Duration::from_secs(15);

/// Everything the tray menu needs beyond the AppHandle: the backend state
/// (shared with main.rs), the cached task list, the resolved webui language
/// (read ONCE from deploy.yaml at build time — it selects the i18n file for
/// task display names), the stop flag (shared with main.rs so ExitRequested
/// can halt the poll thread), the refresh signal channel (manual Refresh
/// menu item) and the WS in-flight flag (dedups concurrent toggles — only
/// one scheduler click session at a time).
#[derive(Clone)]
struct TrayShared {
    backend: Arc<BackendLifecycle>,
    tasks: Arc<Mutex<Vec<Task>>>,
    language: String,
    stop: Arc<AtomicBool>,
    refresh: mpsc::Sender<()>,
    in_flight: Arc<AtomicBool>,
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
        in_flight: Arc::new(AtomicBool::new(false)),
    };

    let labels = load_control_labels();
    let initial = BackendStateSnapshot {
        status: BackendStatus::Stopped,
        start_failed: false,
    };
    let menu = build_menu(
        app.handle(),
        &initial,
        &[],
        TaskSection::Empty,
        &labels,
        // No scheduler scan at startup: a fresh Stopped snapshot renders the
        // stopped label regardless (toggle_label/status_line_for ignore the
        // discriminator outside Running).
        None,
        &status_line_for(&initial, None, &labels),
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
        // Last rendered scheduler-liveness (None = unknown / backend not
        // running); a flip forces a status-line rebuild even when the task
        // section is unchanged (manual webui stop/start must reflect).
        let mut last_scheduler_alive: Option<bool> = None;
        while !thread_shared.stop.load(Ordering::Relaxed) {
            match refresh_rx.recv_timeout(TRAY_POLL_INTERVAL_SECS) {
                Ok(()) => poll_once(
                    &app_handle,
                    &thread_shared,
                    port,
                    &mut last_section,
                    &mut last_scheduler_alive,
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => poll_once(
                    &app_handle,
                    &thread_shared,
                    port,
                    &mut last_section,
                    &mut last_scheduler_alive,
                ),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    Ok(tray)
}

/// (Re)build the whole menu from the current backend state, task cache and
/// task-section rendering. The status item is always disabled; the toggle
/// item's text/enabled follow the state machine (Initializing disables it
/// entirely; the toggle text additionally follows the scheduler scan —
/// `scheduler_alive` None on the initial build). Task items are read-only
/// (disabled): group headers id `group-running|queued|waiting`, task rows id
/// `task-{i}`.
fn build_menu(
    app: &AppHandle,
    snapshot: &BackendStateSnapshot,
    tasks: &[Task],
    section: TaskSection,
    labels: &ControlLabels,
    scheduler_alive: Option<bool>,
    status_line: &str,
) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "tray-status", status_line, false, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        "tray-toggle",
        toggle_label(snapshot.status, scheduler_alive, labels),
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

/// Full Start/Stop toggle.
///
/// Since todo 6 the toggle controls the ALAS SCHEDULER through the webui
/// WebSocket (a short worker-thread session, never a long-lived connection):
/// - `StartScheduler` / `StopScheduler`: scheduler-only click, NO window
///   navigation — the webui stays alive.
/// - `StartBackend`: backend down — bring the whole backend up (existing
///   ordering contract inside [`BackendLifecycle::start`]), navigate to the
///   webui, then click Start over WS from a worker thread.
/// - `StopBackend` (degraded: webui password/SSL configured): legacy
///   process-level stop.
///
/// All WS I/O happens on a worker thread, outside every lock; the decision
/// inputs (`scheduler_alive`, `ws_available`) are computed lock-free here.
fn handle_toggle(app: &AppHandle, shared: &TrayShared, port: u16) {
    // In-flight dedup (plan todo 6f): one scheduler session at a time. The
    // guard is created IMMEDIATELY after the successful compare_exchange and
    // moved into the worker closure below, so the flag clears on every exit
    // path (worker finish, spawn failure, early return, panic).
    let Some(guard) = try_acquire_in_flight(&shared.in_flight) else {
        warn!("Toggle ignored: a previous toggle is still in flight");
        return;
    };

    // Password/SSL degradation guard (plan todo 6b): with a webui password or
    // TLS configured the plain ws:// client cannot drive the scheduler, so
    // the toggle falls back to process-level control.
    let ws_available = ws_available();
    if !ws_available {
        warn!("WebSocket scheduler control unavailable (webui password/SSL configured); falling back to process-level toggle");
    }
    warn_on_pywebio_mismatch();

    // Click-time scheduler liveness — a lock-free re-scan (never the poll
    // cache), plus a 100ms port probe: a dead port while the snapshot still
    // says Running means the backend itself is gone (todo 3d), so fold to
    // Stopped and let the decision take the StartBackend path.
    let mut snapshot = shared.backend.snapshot();
    let scheduler_alive = if snapshot.status == BackendStatus::Running && backend_port_alive(port) {
        shared
            .backend
            .backend_pid()
            .map(|pid| scheduler_alive(uvicorn_alive_child_count(pid, deploy_enable_reload())))
            .unwrap_or(false)
    } else if snapshot.status == BackendStatus::Running {
        shared.backend.mark_stopped();
        snapshot = shared.backend.snapshot();
        false
    } else {
        false
    };

    let action = toggle_decision(&snapshot, scheduler_alive, ws_available);
    let labels = load_control_labels();
    match action {
        // Initializing -> NoOp (the item is disabled anyway; this also makes
        // a second click during a 60s start window a no-op — BLOCKER-3:
        // never two backends).
        ToggleAction::NoOp => return,
        ToggleAction::StopBackend => {
            // Degraded fallback: legacy process-level stop.
            shared.backend.stop();
            navigate_main(app, main_page_url(BackendStatus::Stopped, port, &labels));
        }
        ToggleAction::StartBackend => {
            // Show "initializing…" (and make re-entry a no-op) for the whole
            // spawn window, which may take up to 60s.
            shared.backend.begin_start();
            match shared.backend.start(port) {
                Ok(()) => {
                    navigate_main(app, main_page_url(BackendStatus::Running, port, &labels));
                    if ws_available {
                        spawn_scheduler_click(port, SchedulerAction::Start, labels.clone(), guard);
                    }
                }
                Err(e) => {
                    warn!("Failed to start backend: {e}");
                    // Status is now Stopped + start_failed (set inside the
                    // module); the window stays on the stopped page and the
                    // next menu rebuild shows the localized failed label.
                }
            }
        }
        ToggleAction::StopScheduler | ToggleAction::StartScheduler => {
            // Scheduler-only control: the click is delivered from a worker
            // thread and the window is deliberately NOT navigated. A failed
            // click only warns — the poll thread self-heals the status line
            // and the toggle stays retryable.
            let scheduler_action = match action {
                ToggleAction::StopScheduler => SchedulerAction::Stop,
                _ => SchedulerAction::Start,
            };
            spawn_scheduler_click(port, scheduler_action, labels.clone(), guard);
        }
    }
    // Rebuild must include the task section so a toggle never wipes it. The
    // section derives from the POST-action state (fresh snapshot + cached
    // tasks): a Stop shows the degraded section, a Start shows the cache.
    // The toggle label uses the click-time scheduler scan computed above:
    // a Stop click (scheduler still alive at click time) shows 停止 until
    // the worker's click lands, a Start shows 启动 — the poll thread
    // corrects within one cycle.
    let tasks = shared.tasks.lock().unwrap().clone();
    rebuild_menu(
        app,
        shared,
        task_section(shared.backend.snapshot().status, Ok(tasks)),
        Some(scheduler_alive),
    );
}

/// RAII in-flight guard: clears the shared flag on drop, so the flag can
/// never stick even when the worker thread fails to spawn or panics (the
/// closure — and with it the guard — is dropped in both cases).
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Try to mark the in-flight flag; `Some(guard)` on success (the guard owns a
/// clone of the flag and clears it when dropped), `None` when a toggle is
/// already in flight. `compare_exchange(false, true, Acquire, Relaxed)` is
/// atomic — a fast double-click cannot race two workers in (TOCTOU-free).
fn try_acquire_in_flight(flag: &Arc<AtomicBool>) -> Option<InFlightGuard> {
    flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| InFlightGuard(flag.clone()))
}

/// Spawn the scheduler click on a dedicated worker thread. All WS I/O runs
/// there — off the UI (menu event) thread and outside every lock; the worker
/// never touches `TrayShared` or tauri, only the pure
/// [`click_scheduler`] channel. The in-flight guard moves into the closure.
/// A click failure only warns (the poll thread self-heals the status line).
fn spawn_scheduler_click(
    port: u16,
    action: SchedulerAction,
    labels: ControlLabels,
    guard: InFlightGuard,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("scheduler-click".into())
        .spawn(move || {
            let _guard = guard;
            if let Err(e) = click_scheduler(port, action, &labels, SCHEDULER_CLICK_TIMEOUT) {
                warn!("Scheduler control via WebSocket failed: {e:#}");
            }
        })
    {
        warn!("Failed to spawn scheduler-click worker: {e}");
    }
}

/// Rebuild the menu from the current state + task cache and re-attach it to
/// the tray. The single set_menu site: the toggle handler and the poll thread
/// both route through here (never build-then-set inline).
fn rebuild_menu(app: &AppHandle, shared: &TrayShared, section: TaskSection, scheduler_alive: Option<bool>) {
    let snapshot = shared.backend.snapshot();
    let tasks = shared.tasks.lock().unwrap().clone();
    let labels = load_control_labels();
    let status_line = status_line_for(&snapshot, scheduler_alive, &labels);
    let Ok(menu) = build_menu(app, &snapshot, &tasks, section, &labels, scheduler_alive, &status_line)
    else {
        return;
    };
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_menu(Some(menu));
    }
}

/// One poll cycle: snapshot the status (no network under the lock), fetch
/// outside any lock when Running, then let the pure decision in
/// [`poll_decision`] pick the section / rebuild / cache replacement.
/// `last_section` tracks what the menu currently renders; `last_scheduler`
/// tracks the last rendered scheduler liveness so a flip rebuilds the status
/// line even when the task section is unchanged.
fn poll_once(
    app: &AppHandle,
    shared: &TrayShared,
    port: u16,
    last_section: &mut TaskSection,
    last_scheduler: &mut Option<bool>,
) {
    // (a) status snapshot — no I/O under the backend lock (the lock is
    // internal to BackendLifecycle; status() takes it briefly).
    let mut status = shared.backend.status();

    // (a2) Backend-liveness re-check: Running but the webui port is dead
    // (the backend crashed or was killed outside the launcher) -> plain
    // Stopped via mark_stopped(), so the toggle recovers to StartBackend
    // instead of showing a stuck Running state. The probe is lock-free.
    if status == BackendStatus::Running && !backend_port_alive(port) {
        shared.backend.mark_stopped();
        status = BackendStatus::Stopped;
    }

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

    // (d) Scheduler liveness for the status line: process-tree scan rooted at
    // the backend pid (lock-free; sysinfo reads the OS process table). No
    // handle / no scan result -> None -> conservative stopped.
    let scheduler = if status == BackendStatus::Running {
        shared.backend.backend_pid().map(|pid| {
            scheduler_alive(uvicorn_alive_child_count(pid, deploy_enable_reload()))
        })
    } else {
        None
    };

    // Decision is pure (menu_model::poll_decision): the clock string and the
    // fetch result are injected, so this call site carries no decision logic.
    let outcome = poll_decision(status, fetched, *last_section, &shared.tasks.lock().unwrap());
    if let Some(tasks) = outcome.replace_cache {
        *shared.tasks.lock().unwrap() = tasks;
    }
    let status_line_changed = scheduler != *last_scheduler;
    *last_scheduler = scheduler;
    if outcome.changed || status_line_changed {
        rebuild_menu(app, shared, outcome.section, scheduler);
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

/// `Deploy.Update.EnableReload` from config/deploy.yaml (true when missing —
/// the ALAS default, deploy.yaml:86): decides WHICH process is the uvicorn
/// (reload wrapper vs the backend process itself) per the pinned rule.
fn deploy_enable_reload() -> bool {
    crate::setup::get_deploy_config()
        .and_then(|c| c["Deploy"]["Update"]["EnableReload"].as_bool())
        .unwrap_or(true)
}

/// Pure degradation check: WS scheduler control is unusable when the webui
/// has a password or TLS configured. Reads the REAL payload paths
/// (`Deploy.Webui.Password` / `WebuiSSLKey` / `WebuiSSLCert` — verified
/// against the live deploy.yaml and app.py:1008 / gui.py:59-60; the plan's
/// "Gui.Password" is stale). A non-empty string counts as configured; null,
/// missing and empty all mean "no credential" (ALAS skips login for an empty
/// password).
fn ws_control_available(config: &serde_json::Value) -> bool {
    let configured = |path: &[&str]| -> bool {
        let mut cur = config;
        for key in path {
            cur = match cur.get(key) {
                Some(v) => v,
                None => return false,
            };
        }
        cur.as_str().map(|s| !s.is_empty()).unwrap_or(false)
    };
    !(configured(&["Deploy", "Webui", "Password"])
        || configured(&["Deploy", "Webui", "WebuiSSLKey"])
        || configured(&["Deploy", "Webui", "WebuiSSLCert"]))
}

/// WS availability from the live deploy.yaml; true when it is missing (no
/// credentials can be configured without a file).
fn ws_available() -> bool {
    crate::setup::get_deploy_config()
        .as_ref()
        .map(ws_control_available)
        .unwrap_or(true)
}

/// Runtime pywebio version guard: warn once per toggle when the payload pins
/// a pywebio version other than the one this WS protocol was captured
/// against (1.6.2). File-level best-effort — an unreadable or absent
/// requirements.txt is silent.
fn warn_on_pywebio_mismatch() {
    let Ok(requirements) = std::fs::read_to_string("requirements.txt") else {
        return;
    };
    if !check_pywebio_version(pywebio_version(&requirements).as_deref()) {
        warn!("Payload pywebio version may drift from the WS protocol this launcher speaks (expected 1.6.2); scheduler control could fail");
    }
}

/// Whether the webui port answers within 100ms — the backend-liveness probe.
/// Lock-free: a plain connect_timeout, no backend lock held. Used by poll now
/// (dead port -> mark_stopped) and by the toggle decision later (todo 6).
fn backend_port_alive(port: u16) -> bool {
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok()
}

/// The multiprocessing resource_tracker child of the gui.py wrapper; excluded
/// from the scheduler count (it is present in BOTH tree shapes and would make
/// the reload=false baseline look like 2 children).
fn is_resource_tracker(p: &sysinfo::Process) -> bool {
    p.cmd()
        .iter()
        .any(|c| c.to_string_lossy().contains("multiprocessing.resource_tracker"))
}

/// Count of the uvicorn process's alive, non-zombie, non-resource-tracker
/// children — the pinned scheduler discriminator (evidence task-3). The
/// uvicorn is `backend_pid` itself when EnableReload=false (func runs
/// in-process), or its reload-wrapper child (the `spawn_main` process) when
/// EnableReload=true. Lock-free: reads only sysinfo; no backend lock held.
/// A missing/dead process yields 0 (conservative — scheduler shown stopped).
fn uvicorn_alive_child_count(backend_pid: u32, enable_reload: bool) -> usize {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let uvicorn_pid = if enable_reload {
        sys.processes()
            .values()
            .find(|p| {
                p.parent().map(|q| q.as_u32()) == Some(backend_pid) && !is_resource_tracker(p)
            })
            .map(|p| p.pid())
    } else {
        Some(sysinfo::Pid::from_u32(backend_pid))
    };
    let Some(uvicorn_pid) = uvicorn_pid else {
        return 0;
    };
    sys.processes()
        .values()
        .filter(|p| p.parent() == Some(uvicorn_pid))
        .filter(|p| p.status() != sysinfo::ProcessStatus::Zombie)
        .filter(|p| !is_resource_tracker(p))
        .count()
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
    use std::process::{Child, Command};

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

    // ---- in-flight dedup -----------------------------------------------------

    #[test]
    fn in_flight_acquire_rejects_second_toggle_until_released() {
        let flag = Arc::new(AtomicBool::new(false));
        let g1 = try_acquire_in_flight(&flag).expect("first toggle acquires");
        assert!(try_acquire_in_flight(&flag).is_none(), "second toggle ignored");
        assert!(flag.load(Ordering::Relaxed), "flag set while held");
        drop(g1);
        assert!(!flag.load(Ordering::Relaxed), "drop clears the flag");
        let g2 = try_acquire_in_flight(&flag).expect("re-acquire after release");
        drop(g2);
    }

    // ---- ws_control_available (password/SSL degradation) ---------------------

    #[test]
    fn ws_control_available_degrades_on_password() {
        let cfg = serde_json::json!({"Deploy": {"Webui": {"Password": "secret"}}});
        assert!(!ws_control_available(&cfg), "password configured -> degraded");
    }

    #[test]
    fn ws_control_available_degrades_on_ssl() {
        let key = serde_json::json!({"Deploy": {"Webui": {"WebuiSSLKey": "/tmp/k.pem"}}});
        assert!(!ws_control_available(&key), "ssl key -> degraded");
        let cert = serde_json::json!({"Deploy": {"Webui": {"WebuiSSLCert": "/tmp/c.pem"}}});
        assert!(!ws_control_available(&cert), "ssl cert -> degraded");
    }

    #[test]
    fn ws_control_available_null_missing_and_empty_are_available() {
        // Real payload shape: Password/SSL all null (live deploy.yaml).
        let nulls = serde_json::json!({"Deploy": {"Webui": {
            "Password": serde_json::Value::Null,
            "WebuiSSLKey": serde_json::Value::Null,
            "WebuiSSLCert": serde_json::Value::Null,
        }}});
        assert!(ws_control_available(&nulls), "nulls -> available");
        // Empty string = no password in ALAS semantics.
        let empty = serde_json::json!({"Deploy": {"Webui": {"Password": ""}}});
        assert!(ws_control_available(&empty), "empty password -> available");
        // Missing sections entirely.
        assert!(ws_control_available(&serde_json::json!({})));
        assert!(ws_control_available(&serde_json::json!({"Deploy": {}})));
    }

    // ---- backend_port_alive --------------------------------------------------

    #[test]
    fn backend_port_alive_refuses_a_closed_port() {
        assert!(!backend_port_alive(1), "port 1 is never listening");
    }

    #[test]
    fn backend_port_alive_accepts_a_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(backend_port_alive(port));
    }

    // ---- uvicorn_alive_child_count (real processes, pinned rule) -------------
    //
    // Fake-backend isolation: every scenario owns a dedicated root process
    // (plain `sleep` or a `sh -c` script whose backgrounded sleeps ARE its
    // children), so parallel tests never see each other's processes. sysinfo
    // may lag a spawn by a few ms, so expected counts are polled with a
    // retry loop. Leftover orphans die by themselves (sleep 30).

    fn spawn_sleep() -> Child {
        Command::new("sleep").arg("30").spawn().expect("spawn sleep")
    }

    fn spawn_sh(script: &str) -> Child {
        Command::new("sh").arg("-c").arg(script).spawn().expect("spawn sh")
    }

    fn kill_all(children: &mut [Child]) {
        for c in children.iter_mut() {
            let _ = c.kill();
        }
        for c in children.iter_mut() {
            let _ = c.wait();
        }
    }

    fn child_pids(pid: u32) -> Vec<u32> {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        sys.processes()
            .values()
            .filter(|p| p.parent().map(|q| q.as_u32()) == Some(pid))
            .map(|p| p.pid().as_u32())
            .collect()
    }

    fn wait_count(fake_backend: u32, enable_reload: bool, want: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let got = uvicorn_alive_child_count(fake_backend, enable_reload);
            if got == want || std::time::Instant::now() > deadline {
                assert_eq!(got, want, "count for backend {fake_backend}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn uvicorn_count_reload_off_baseline_and_scheduler() {
        let empty = spawn_sleep();
        wait_count(empty.id(), false, 0);
        kill_all(&mut [empty]);
        // Manager baseline: 1 child -> scheduler NOT alive (rule: >1).
        let baseline = spawn_sh("sleep 30 & wait");
        wait_count(baseline.id(), false, 1);
        kill_all(&mut [baseline]);
        // Scheduler joins: 2 children -> alive.
        let running = spawn_sh("sleep 30 & sleep 30 & wait");
        wait_count(running.id(), false, 2);
        kill_all(&mut [running]);
    }

    #[test]
    fn uvicorn_count_reload_off_excludes_tracker_and_unreaped_children() {
        // resource_tracker lookalike child: 2 children, one is the tracker
        // -> count 1 (would be 2 without the exclusion).
        let tracked = spawn_sh(
            "python3 -c 'from multiprocessing.resource_tracker import main; import time; time.sleep(60)' & sleep 30 & wait",
        );
        wait_count(tracked.id(), false, 1);
        kill_all(&mut [tracked]);
        // Killed child (SIGKILLed, never reaped: sh only waits the foreground
        // sleep) must not count: 3 children -> 2.
        let with_dead = spawn_sh("sleep 30 & sleep 30 & sleep 30");
        let backend = with_dead.id();
        wait_count(backend, false, 3);
        let victim = child_pids(backend).into_iter().min().unwrap();
        Command::new("kill")
            .arg("-9")
            .arg(victim.to_string())
            .status()
            .unwrap();
        wait_count(backend, false, 2);
        kill_all(&mut [with_dead]);
    }

    #[test]
    fn uvicorn_count_reload_on_uses_wrapper_child() {
        // No wrapper child -> no uvicorn -> 0.
        let empty = spawn_sleep();
        wait_count(empty.id(), true, 0);
        kill_all(&mut [empty]);
        // Wrapper (the reload P1) with its Manager baseline: 1 -> not alive.
        let baseline = spawn_sh("sh -c 'sleep 30 & wait' & wait");
        wait_count(baseline.id(), true, 1);
        kill_all(&mut [baseline]);
        // Wrapper with Manager + scheduler: 2 -> alive.
        let running = spawn_sh("sh -c 'sleep 30 & sleep 30 & wait' & wait");
        wait_count(running.id(), true, 2);
        kill_all(&mut [running]);
    }

    #[test]
    fn uvicorn_count_reload_on_ignores_tracker_sibling_of_wrapper() {
        // P0 children = [python3 tracker, inner-sh wrapper]; the wrapper must
        // be selected as uvicorn (count 1 — picking the tracker would give 0).
        let root = spawn_sh(
            "python3 -c 'from multiprocessing.resource_tracker import main; import time; time.sleep(60)' & sh -c 'sleep 30 & wait' & wait",
        );
        wait_count(root.id(), true, 1);
        kill_all(&mut [root]);
    }
}
