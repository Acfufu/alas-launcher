// macOS system tray: template icon + native menu with a three-state backend
// lifecycle toggle (BackendState shared with main.rs).
// Module is cfg-gated to macOS by `mod tray;` in main.rs, so this file never
// participates in win/linux builds.

use std::{
    collections::HashSet,
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
    backend::{BackendState, BackendStatus, ManagedBackend},
};

/// Poll cadence for the task section; also bounds each idle wait of the poll
/// thread (a refresh signal wakes it early, never later than this).
const TRAY_POLL_INTERVAL_SECS: Duration = Duration::from_secs(10);

/// Menu compactness: each status group shows at most the 3 soonest tasks.
/// The task slice is sorted by `next_time` ascending (see fetch_tasks), so the
/// first items of a group are the soonest — the top of the queue.
const TASK_GROUP_MAX: usize = 3;

/// Which rendering the task section of the menu should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskSection {
    /// Backend not running, or the task fetch failed — "Tasks: unavailable".
    Degraded,
    /// Backend running, zero tasks known — "No tasks".
    Empty,
    /// Backend running with a real task list.
    Tasks,
}

/// Everything the tray menu needs beyond the AppHandle: the backend state
/// (shared with main.rs), the cached task list, the stop flag (shared with
/// main.rs so ExitRequested can halt the poll thread) and the refresh signal
/// channel (manual Refresh menu item).
#[derive(Clone)]
struct TrayShared {
    backend: Arc<Mutex<BackendState>>,
    tasks: Arc<Mutex<Vec<Task>>>,
    stop: Arc<AtomicBool>,
    refresh: mpsc::Sender<()>,
}

/// Build the macOS menu-bar tray icon with its native menu.
///
/// `backend` is the shared three-state lifecycle object (also owned by
/// main.rs); `port` is the ALAS webui port used for the main-page URL and the
/// task fetch; `stop` is the shared poll-thread stop flag (also owned by
/// main.rs, set in ExitRequested). A returned `Err` is warn-and-continue at
/// the call site — a tray failure must never abort app startup.
pub fn build_tray(
    app: &tauri::App,
    backend: Arc<Mutex<BackendState>>,
    port: u16,
    stop: Arc<AtomicBool>,
) -> tauri::Result<tauri::tray::TrayIcon> {
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();
    let shared = TrayShared {
        backend,
        tasks: Arc::new(Mutex::new(Vec::new())),
        stop,
        refresh: refresh_tx,
    };

    let menu = build_menu(app, &BackendState::default(), &[], TaskSection::Empty)?;

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
    // the thread owns a clone so the tray stays alive for its whole lifetime.
    let app_handle = app.handle().clone();
    let thread_tray = tray.clone();
    std::thread::spawn(move || {
        let mut last_section = TaskSection::Empty;
        while !thread_shared.stop.load(Ordering::Relaxed) {
            match refresh_rx.recv_timeout(TRAY_POLL_INTERVAL_SECS) {
                Ok(()) => poll_once(&app_handle, &thread_tray, &thread_shared, &mut last_section),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    poll_once(&app_handle, &thread_tray, &thread_shared, &mut last_section)
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
    app: &impl tauri::Manager<tauri::Wry>,
    state: &BackendState,
    tasks: &[Task],
    section: TaskSection,
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

    let mut section_items: Vec<Box<dyn IsMenuItem<tauri::Wry>>> = Vec::new();
    match section {
        TaskSection::Degraded => section_items.push(Box::new(MenuItem::with_id(
            app,
            "tasks-degraded",
            "Tasks: unavailable",
            false,
            None::<&str>,
        )?)),
        TaskSection::Empty => section_items.push(Box::new(MenuItem::with_id(
            app,
            "tasks-empty",
            "No tasks",
            false,
            None::<&str>,
        )?)),
        TaskSection::Tasks => {
            for item in task_section_items(tasks) {
                match item {
                    TaskMenuItem::GroupHeader { id, text } => {
                        section_items.push(Box::new(MenuItem::with_id(
                            app, id, text, false, None::<&str>,
                        )?));
                    }
                    TaskMenuItem::TaskItem { id, text } => {
                        section_items.push(Box::new(MenuItem::with_id(
                            app, id, text, false, None::<&str>,
                        )?));
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

/// One renderable row of the tray's task section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskMenuItem {
    /// Non-empty status-group header (disabled; 运行中/队列中/等待中).
    GroupHeader { id: String, text: &'static str },
    /// One task row (disabled; `task-{i}`, text `"{name} — {time}"`).
    TaskItem { id: String, text: String },
    /// Native separator line between two non-empty groups.
    Separator,
}

/// Render the enabled-task list as grouped rows, mirroring the webui
/// overview scopes (module/webui/app.py `alas_update_overview_task`):
/// 运行中 → 队列中 → 等待中, each non-empty group led by a header and
/// separated from the next non-empty group by a `Separator` (never before the
/// first group, never after the last). Each group caps at [`TASK_GROUP_MAX`]
/// tasks — the input slice is sorted by `next_time` ascending, so the first
/// items of a group are the soonest. Empty groups render NO header. Empty
/// input → empty output (the caller renders the "No tasks" empty state
/// instead).
pub(crate) fn task_section_items(tasks: &[Task]) -> Vec<TaskMenuItem> {
    let mut items = Vec::new();
    let mut index = 0usize;
    let mut emitted_group = false;
    for (status, id, text) in [
        (alas_tasks::TaskStatus::Running, "group-running", "运行中"),
        (alas_tasks::TaskStatus::Queued, "group-queued", "队列中"),
        (alas_tasks::TaskStatus::Waiting, "group-waiting", "等待中"),
    ] {
        let group: Vec<&Task> = tasks.iter().filter(|t| t.status == status).collect();
        if group.is_empty() {
            continue;
        }
        if emitted_group {
            items.push(TaskMenuItem::Separator);
        }
        emitted_group = true;
        items.push(TaskMenuItem::GroupHeader {
            id: id.to_string(),
            text,
        });
        for task in group.into_iter().take(TASK_GROUP_MAX) {
            // Full NextRun string: the webui renders `str(func.next_run)`
            // verbatim, so keeping the whole "YYYY-MM-DD HH:MM:SS" matches the
            // reference exactly (no truncation). Empty when the payload lacks
            // a NextRun (real payloads always carry one).
            items.push(TaskMenuItem::TaskItem {
                id: format!("task-{index}"),
                text: format!("{} — {}", task.name, task.next_time.as_deref().unwrap_or_default()),
            });
            index += 1;
        }
    }
    items
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
fn handle_toggle(app: &AppHandle, shared: &TrayShared, port: u16) {
    // Snapshot the state and decide. Initializing -> NoOp (the item is
    // disabled anyway; this also makes a second click during a 60s start
    // window a no-op — BLOCKER-3: never two backends).
    let action = toggle_decision(&shared.backend.lock().unwrap());
    match action {
        ToggleAction::NoOp => return,
        ToggleAction::Stop => {
            // ORDERING CONTRACT, stop path: take() -> status Stopped -> drop
            // the lock guard -> terminate() -> drop(old). terminate() kills
            // gui.py BEFORE the Option is dropped, so the Drop kill-all scan
            // runs with gui.py already dead.
            let mut old = {
                let mut state = shared.backend.lock().unwrap();
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
                let mut state = shared.backend.lock().unwrap();
                state.backend.take()
            };
            drop(old);
            {
                let mut state = shared.backend.lock().unwrap();
                state.status = BackendStatus::Initializing;
                state.start_failed = false;
            }
            match ManagedBackend::new(port) {
                Ok(b) => {
                    {
                        let mut state = shared.backend.lock().unwrap();
                        state.backend = Some(b);
                        state.status = BackendStatus::Running;
                        state.start_failed = false;
                    }
                    navigate_main(app, main_page_url(BackendStatus::Running, port));
                }
                Err(e) => {
                    warn!("Failed to start backend: {e}");
                    {
                        let mut state = shared.backend.lock().unwrap();
                        state.status = BackendStatus::Stopped;
                        state.start_failed = true;
                    }
                }
            }
        }
    }
    // Rebuild must include the task section so a toggle never wipes it.
    rebuild_menu(app, shared, section_from_state(shared));
}

/// Snapshot of the backend state needed for menu rendering (no lock held
/// afterwards; `backend` handle is not needed to render the menu).
fn snapshot_state(shared: &TrayShared) -> BackendState {
    let state = shared.backend.lock().unwrap();
    BackendState {
        status: state.status,
        backend: None,
        start_failed: state.start_failed,
    }
}

/// Build the current menu (state + task cache + section) if possible.
fn current_menu(
    app: &impl tauri::Manager<tauri::Wry>,
    shared: &TrayShared,
    section: TaskSection,
) -> Option<Menu<tauri::Wry>> {
    let state = snapshot_state(shared);
    let tasks = shared.tasks.lock().unwrap().clone();
    build_menu(app, &state, &tasks, section).ok()
}

/// The task-section rendering implied by the current status + task cache
/// (used by the toggle handler; the poll thread computes its own via
/// [`task_section`] because it knows the fetch result).
fn section_from_state(shared: &TrayShared) -> TaskSection {
    let status = snapshot_state(shared).status;
    let tasks = shared.tasks.lock().unwrap().clone();
    task_section(status, Ok(tasks))
}

/// Rebuild the menu from the current state and re-attach it to the tray.
fn rebuild_menu(app: &AppHandle, shared: &TrayShared, section: TaskSection) {
    let Some(menu) = current_menu(app, shared, section) else {
        return;
    };
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_menu(Some(menu));
    }
}

/// The task-section rendering implied by backend status + fetch result.
pub(crate) fn task_section(status: BackendStatus, fetch: Result<Vec<Task>, ()>) -> TaskSection {
    if status != BackendStatus::Running {
        return TaskSection::Degraded;
    }
    match fetch {
        Err(()) => TaskSection::Degraded,
        Ok(tasks) if tasks.is_empty() => TaskSection::Empty,
        Ok(_) => TaskSection::Tasks,
    }
}

/// Name-set difference between the old and new task lists, used to decide
/// whether a rebuild is needed. Returned ids are positional in the OLD list
/// (`task-{i}`); returned tasks come from NEW (command not present in OLD).
/// Both empty => no structural change => caller skips the rebuild.
///
/// Identity is the `command` key, NOT the display `name`: names come from the
/// i18n file (`Task.<command>.name`) and can change with the webui language,
/// while the command equals the stable alas.json top-level key.
pub(crate) fn menu_diff(old: &[Task], new: &[Task]) -> (Vec<String>, Vec<Task>) {
    let new_commands: HashSet<&str> = new.iter().map(|t| t.command.as_str()).collect();
    let to_remove = old
        .iter()
        .enumerate()
        .filter(|(_, t)| !new_commands.contains(t.command.as_str()))
        .map(|(i, _)| format!("task-{i}"))
        .collect();
    let old_commands: HashSet<&str> = old.iter().map(|t| t.command.as_str()).collect();
    let to_add: Vec<Task> = new
        .iter()
        .filter(|t| !old_commands.contains(t.command.as_str()))
        .cloned()
        .collect();
    (to_remove, to_add)
}

/// One poll cycle: snapshot the status (no network under the lock), fetch
/// outside any lock when Running, then rebuild only when the rendered section
/// kind or the task command set changed (anti-flicker). `last_section` tracks
/// what the menu currently renders.
fn poll_once(
    app: &AppHandle,
    tray: &tauri::tray::TrayIcon,
    shared: &TrayShared,
    last_section: &mut TaskSection,
) {
    // (a) status snapshot — no I/O under the backend lock.
    let status = shared.backend.lock().unwrap().status;

    // (b/c) Running -> fetch outside any lock; anything else -> degraded.
    // Tasks are read from the payload files (config/alas.json + i18n — the
    // ALAS webui has no JSON API, see alas_tasks module doc).
    let fetched: Result<Vec<Task>, ()> = if status == BackendStatus::Running {
        let alas_dir = std::env::current_dir().unwrap_or_default();
        let language = deploy_language().unwrap_or_else(|| "zh-CN".to_string());
        alas_tasks::fetch_tasks(&alas_dir, &alas_tasks::now_str(), &language).map_err(|_| ())
    } else {
        Err(())
    };
    let section = task_section(status, fetched.clone());

    let tasks_changed = match &fetched {
        Ok(tasks) => {
            let (to_remove, to_add) = {
                let cached = shared.tasks.lock().unwrap();
                menu_diff(&cached, tasks)
            };
            !(to_remove.is_empty() && to_add.is_empty())
        }
        Err(()) => false,
    };

    if *last_section != section || tasks_changed {
        if let Ok(tasks) = fetched {
            *shared.tasks.lock().unwrap() = tasks;
        }
        if let Some(menu) = current_menu(app, shared, section) {
            let _ = tray.set_menu(Some(menu));
        }
        *last_section = section;
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

    #[test]
    fn task_section_items_all_three_groups() {
        let tasks = vec![
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
        ];
        let items = task_section_items(&tasks);
        assert_eq!(
            items,
            vec![
                TaskMenuItem::GroupHeader { id: "group-running".into(), text: "运行中" },
                TaskMenuItem::TaskItem {
                    id: "task-0".into(),
                    text: "战术学院 — 2026-08-10 20:14:24".into(),
                },
                TaskMenuItem::Separator,
                TaskMenuItem::GroupHeader { id: "group-queued".into(), text: "队列中" },
                TaskMenuItem::TaskItem {
                    id: "task-1".into(),
                    text: "大舰队 — 2026-08-10 21:00:00".into(),
                },
                TaskMenuItem::Separator,
                TaskMenuItem::GroupHeader { id: "group-waiting".into(), text: "等待中" },
                TaskMenuItem::TaskItem {
                    id: "task-2".into(),
                    text: "演习 — 2026-08-11 00:00:00".into(),
                },
            ]
        );
    }

    #[test]
    fn task_section_items_only_waiting_renders_one_header() {
        let tasks = vec![
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
            Task {
                name: "主线图-2".into(),
                command: "Main2".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 06:00:00".into()),
            },
        ];
        let items = task_section_items(&tasks);
        assert_eq!(
            items,
            vec![
                TaskMenuItem::GroupHeader { id: "group-waiting".into(), text: "等待中" },
                TaskMenuItem::TaskItem {
                    id: "task-0".into(),
                    text: "演习 — 2026-08-11 00:00:00".into(),
                },
                TaskMenuItem::TaskItem {
                    id: "task-1".into(),
                    text: "主线图-2 — 2026-08-11 06:00:00".into(),
                },
            ]
        );
    }

    #[test]
    fn task_section_items_empty_is_empty_vec() {
        assert_eq!(task_section_items(&[]), vec![]);
    }

    #[test]
    fn task_section_items_ids_sequential_across_groups() {
        let tasks = vec![
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
        ];
        let items = task_section_items(&tasks);
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                TaskMenuItem::TaskItem { id, .. } => Some(id.as_str()),
                TaskMenuItem::GroupHeader { .. } | TaskMenuItem::Separator => None,
            })
            .collect();
        assert_eq!(ids, vec!["task-0", "task-1", "task-2"]);
    }

    #[test]
    fn task_section_items_caps_group_at_three() {
        // Five waiting tasks (soonest first, as fetch_tasks sorts) -> only the
        // top 3 are rendered, ids task-0..task-2.
        let tasks: Vec<Task> = (0..5)
            .map(|i| Task {
                name: format!("任务-{i}"),
                command: format!("Task{i}"),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some(format!("2026-08-11 0{i}:00:00")),
            })
            .collect();
        let items = task_section_items(&tasks);
        assert_eq!(items.len(), 1 + TASK_GROUP_MAX); // one header + 3 tasks
        assert_eq!(
            items[0],
            TaskMenuItem::GroupHeader { id: "group-waiting".into(), text: "等待中" }
        );
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                TaskMenuItem::TaskItem { id, .. } => Some(id.as_str()),
                TaskMenuItem::GroupHeader { .. } | TaskMenuItem::Separator => None,
            })
            .collect();
        assert_eq!(ids, vec!["task-0", "task-1", "task-2"]);
    }

    #[test]
    fn task_section_items_separator_only_between_groups() {
        // Running(1) + Waiting(1): exactly one separator, no leading/trailing.
        let tasks = vec![
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
        ];
        let items = task_section_items(&tasks);
        assert_eq!(
            items,
            vec![
                TaskMenuItem::GroupHeader { id: "group-running".into(), text: "运行中" },
                TaskMenuItem::TaskItem {
                    id: "task-0".into(),
                    text: "战术学院 — 2026-08-10 20:14:24".into(),
                },
                TaskMenuItem::Separator,
                TaskMenuItem::GroupHeader { id: "group-waiting".into(), text: "等待中" },
                TaskMenuItem::TaskItem {
                    id: "task-1".into(),
                    text: "演习 — 2026-08-11 00:00:00".into(),
                },
            ]
        );

        // Three groups, one task each -> exactly two separators, none at the
        // ends.
        let tasks = vec![
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
        ];
        let items = task_section_items(&tasks);
        let separator_count = items
            .iter()
            .filter(|i| **i == TaskMenuItem::Separator)
            .count();
        assert_eq!(separator_count, 2);
        assert_ne!(items.first(), Some(&TaskMenuItem::Separator));
        assert_ne!(items.last(), Some(&TaskMenuItem::Separator));
    }

    /// Manual-QA channel against the REAL installed ALAS payload (read-only).
    /// Prints the grouped task rows exactly as the tray renders them.
    /// Ignored by default; run with
    /// `cargo test real_payload_group_preview -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_payload_group_preview() {
        let payload = std::path::Path::new(
            "/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript",
        );
        if !payload.join("config").join("alas.json").exists() {
            eprintln!("real ALAS payload not present; skipping");
            return;
        }
        let tasks =
            alas_tasks::fetch_tasks(payload, &alas_tasks::now_str(), "zh-CN").unwrap();
        for item in task_section_items(&tasks) {
            match item {
                TaskMenuItem::GroupHeader { id, text } => println!("[{id}] {text}"),
                TaskMenuItem::TaskItem { id, text } => println!("[{id}] {text}"),
                TaskMenuItem::Separator => println!("[separator]"),
            }
        }
    }

    #[test]
    fn menu_diff_unchanged_is_empty() {
        let old = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
            enabled: true,
            ..Default::default()
        }];
        // Same command set (status changed) -> no structural change.
        let new = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
            enabled: true,
            status: alas_tasks::TaskStatus::Waiting,
            next_time: Some("2026-08-11 00:00:00".into()),
        }];
        assert_eq!(menu_diff(&old, &new), (vec![], vec![]));
        assert_eq!(menu_diff(&[], &[]), (vec![], vec![]));
    }

    #[test]
    fn menu_diff_add_scenario() {
        let old: Vec<Task> = vec![];
        let new = vec![
            Task {
                name: "每日任务".into(),
                command: "Daily".into(),
                enabled: true,
                ..Default::default()
            },
            Task {
                name: "困难图".into(),
                command: "Hard".into(),
                enabled: false,
                ..Default::default()
            },
        ];
        let (to_remove, to_add) = menu_diff(&old, &new);
        assert!(to_remove.is_empty());
        assert_eq!(to_add.len(), 2);
        assert_eq!(to_add[0].command, "Daily");
        assert_eq!(to_add[1].command, "Hard");
    }

    #[test]
    fn menu_diff_remove_scenario() {
        let old = vec![
            Task {
                name: "每日任务".into(),
                command: "Daily".into(),
                enabled: true,
                ..Default::default()
            },
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                enabled: true,
                ..Default::default()
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                enabled: true,
                ..Default::default()
            },
        ];
        let new = vec![Task {
            name: "战术学院".into(),
            command: "Tactical".into(),
            enabled: true,
            ..Default::default()
        }];
        let (to_remove, to_add) = menu_diff(&old, &new);
        // Positional ids in the OLD list: Daily=0, Guild=2.
        assert_eq!(
            to_remove,
            vec!["task-0".to_string(), "task-2".to_string()]
        );
        assert!(to_add.is_empty());
    }

    #[test]
    fn menu_diff_same_commands_different_names_no_change() {
        // i18n display names may change (e.g. webui language switch) while
        // commands stay stable — identity must be the command.
        let old = vec![Task {
            name: "主线图".into(),
            command: "Main".into(),
            enabled: true,
            ..Default::default()
        }];
        let new = vec![Task {
            name: "Main".into(), // i18n gone -> name falls back to command
            command: "Main".into(),
            enabled: true,
            ..Default::default()
        }];
        assert_eq!(menu_diff(&old, &new), (vec![], vec![]));
    }

    #[test]
    fn task_section_matrix() {
        let one = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
            enabled: true,
            ..Default::default()
        }];
        assert_eq!(
            task_section(BackendStatus::Running, Ok(one.clone())),
            TaskSection::Tasks
        );
        assert_eq!(
            task_section(BackendStatus::Running, Ok(vec![])),
            TaskSection::Empty
        );
        assert_eq!(
            task_section(BackendStatus::Running, Err(())),
            TaskSection::Degraded
        );
        assert_eq!(
            task_section(BackendStatus::Stopped, Ok(one.clone())),
            TaskSection::Degraded
        );
        assert_eq!(
            task_section(BackendStatus::Initializing, Err(())),
            TaskSection::Degraded
        );
    }
}
