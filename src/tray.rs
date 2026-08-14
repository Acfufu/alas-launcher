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
        atomic::{AtomicBool, AtomicU8, Ordering},
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
use tauri_plugin_notification::NotificationExt;
use tracing::{info, warn};

use crate::{
    alas_tasks::{self, Task},
    backend::{
        scheduler_alive, toggle_decision, BackendLifecycle, BackendStateSnapshot,
        BackendStatus, SchedulerIntent, ToggleAction,
    },
    menu_model::{
        control_labels, poll_decision, poll_needs_rebuild,
        status_line_for, task_section, task_section_items,
        toggle_enabled, toggle_label, ControlLabels, TaskMenuItem, TaskSection,
    },
};

/// Poll cadence for the task section; also bounds each idle wait of the poll
/// thread (a refresh signal wakes it early, never later than this).
const TRAY_POLL_INTERVAL_SECS: Duration = Duration::from_secs(3);

/// Everything the tray menu needs beyond the AppHandle: the backend state
/// (shared with main.rs), the cached task list, the shared shell settings
/// (todo 4 — the effective UI language is re-resolved from it on every menu
/// build / poll so a live language switch re-renders the tray), the stop
/// flag (shared with main.rs so ExitRequested can halt the poll thread),
/// the refresh signal channel (manual Refresh menu item, and the todo-4
/// language-switch wake) and the WS in-flight flag (dedups concurrent
/// toggles — only one scheduler click session at a time). `port_fail_count`
/// is the poll thread's CONSECUTIVE failed-probe streak (MINOR-5): two
/// misses mark the backend crashed, a success resets it.
#[derive(Clone)]
struct TrayShared {
    backend: Arc<BackendLifecycle>,
    tasks: Arc<Mutex<Vec<Task>>>,
    settings: Arc<Mutex<crate::shell_settings::ShellSettings>>,
    stop: Arc<AtomicBool>,
    refresh: mpsc::Sender<()>,
    in_flight: Arc<AtomicBool>,
    port_fail_count: Arc<AtomicU8>,
}

/// Poll-thread-only notification baseline state (NOT shared across threads).
/// TrayShared is Arc/Mutex shared, so a notification baseline stored there
/// would be polluted by concurrent writers — this state is created inside the
/// poll-thread closure and passed to poll_once as `&mut` (task cache diff,
/// scheduler-instance-state baseline, and the death persistence streak).
#[derive(Default)]
struct PollNotifState {
    last_tasks: Option<Vec<Task>>,
    last_instance_state: Option<u8>,
    dead_streak: u8, // 存活翻转持久化计数（Round-2：2 次连续判死才触发）
    // 剧集锁存（Round-1 F-MEDIUM）：同一次死亡只通知一次——state 1→3 与
    // liveness streak=2 落在相邻两个 poll 周期时，第二个通道不再补发；
    // 复活（now_alive == Some(true)）或后端停止运行才复位。
    death_notified: bool,
}

/// Build the macOS menu-bar tray icon with its native menu.
///
/// `backend` is the shared lifecycle object (also owned by main.rs); `port`
/// is the ALAS webui port used for the main-page URL and the task fetch;
/// `stop` is the shared poll-thread stop flag (also owned by main.rs, set in
/// ExitRequested); `settings` is the shared shell settings (todo 4) whose
/// effective language selects every label — re-read per menu build, so a
/// language switch re-renders the tray. A returned `Err` is warn-and-continue
/// at the call site — a tray failure must never abort app startup.
///
/// Returns the tray icon PLUS the poll-thread refresh channel (todo 4 wake
/// mechanism): main.rs keeps the sender so the app-level language handler can
/// force a tray rebuild (`refresh.send(())` wakes the poll thread, which
/// always rebuilds on a wake — the force_rebuild path below).
pub fn build_tray(
    app: &tauri::App,
    backend: Arc<BackendLifecycle>,
    port: u16,
    stop: Arc<AtomicBool>,
    settings: Arc<Mutex<crate::shell_settings::ShellSettings>>,
    // MAJOR-4: the poll thread's JoinHandle lands here; cleanup_for_exit
    // (child_process.rs) takes and joins it before tauri tears down the tray,
    // so set_menu can never race cleanup_before_exit (issue #1, #12534).
    poll_handle: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
) -> tauri::Result<(tauri::tray::TrayIcon, mpsc::Sender<()>)> {
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();
    let shared = TrayShared {
        backend,
        tasks: Arc::new(Mutex::new(Vec::new())),
        settings,
        stop,
        refresh: refresh_tx.clone(),
        in_flight: Arc::new(AtomicBool::new(false)),
        port_fail_count: Arc::new(AtomicU8::new(0)),
    };

    let labels = load_control_labels(&shared.settings);
    let initial = BackendStateSnapshot {
        status: BackendStatus::Stopped,
        start_failed: false,
        crashed: false,
        scheduler_intent: SchedulerIntent::None,
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
        &status_line_for(
            &initial,
            None,
            &labels,
            crate::deploy_config::ws_control_available(),
        ),
        // No click in flight at startup.
        false,
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
                // Wake the poll thread; a woken poll ALWAYS rebuilds (manual
                // refresh = explicit user intent to see current state), unlike
                // a timed poll which only rebuilds on detected change.
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

    // Poll thread: 3s cadence (or on-demand via refresh), diff-based rebuild
    // of the task section, degraded fallback while the backend is down or the
    // fetch fails. TrayIcon is Send + Sync, so set_menu from here is safe;
    // the thread owns a clone so the tray stays alive for its whole lifetime
    // (rebuild_menu reaches it through the app handle).
    let app_handle = app.handle().clone();
    let _thread_tray = tray.clone();
    let handle = std::thread::spawn(move || {
        let mut last_section = TaskSection::Empty;
        // Last rendered scheduler-liveness (None = unknown / backend not
        // running); a flip forces a status-line rebuild even when the task
        // section is unchanged (manual webui stop/start must reflect).
    let mut last_scheduler_alive: Option<bool> = None;
    let mut notif = PollNotifState::default();
    while !thread_shared.stop.load(Ordering::Relaxed) {
        match refresh_rx.recv_timeout(TRAY_POLL_INTERVAL_SECS) {
            // Woken (manual Refresh, or the worker-complete wake after an
            // in-flight toggle): ALWAYS rebuild so the 处理中… toggle from
            // the tail rebuild is replaced by the real state — the
            // scheduler-liveness edge detector may have already consumed
            // its flip while processing was still true, and a dead scan
            // would otherwise skip the gate forever.
            Ok(()) => poll_once(
                &app_handle,
                &thread_shared,
                port,
                &mut last_section,
                &mut last_scheduler_alive,
                true,
                &mut notif,
            ),
            // Timed poll: keep the diff-based gate (rebuild only on
            // detected change) to avoid needless menu flicker.
            Err(mpsc::RecvTimeoutError::Timeout) => poll_once(
                &app_handle,
                &thread_shared,
                port,
                &mut last_section,
                &mut last_scheduler_alive,
                false,
                &mut notif,
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    });
    // MAJOR-4: hand the handle to cleanup_for_exit (via the main.rs slot) —
    // it joins this thread before tauri's cleanup_before_exit tears down the
    // tray, making the set_menu-vs-teardown race deterministic (issue #1).
    *poll_handle.lock().unwrap() = Some(handle);

    Ok((tray, refresh_tx))
}

/// (Re)build the whole menu from the current backend state, task cache and
/// task-section rendering. The status item is always disabled; the toggle
/// item's text/enabled follow the state machine (Initializing disables it
/// entirely; `processing` — a scheduler-control click in flight — shows the
/// localized 处理中… label and disables the item; the toggle text otherwise
/// follows the scheduler scan — `scheduler_alive` None on the initial build).
/// Task items are read-only (disabled): group headers id
/// `group-running|queued|waiting`, task rows id `task-{i}`.
#[allow(clippy::too_many_arguments)] // 8 orthogonal render inputs; a struct would obscure the call sites
fn build_menu(
    app: &AppHandle,
    snapshot: &BackendStateSnapshot,
    tasks: &[Task],
    section: TaskSection,
    labels: &ControlLabels,
    scheduler_alive: Option<bool>,
    status_line: &str,
    processing: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "tray-status", status_line, false, None::<&str>)?;
    let toggle_text = if processing {
        labels.processing.clone()
    } else {
        toggle_label(snapshot.status, scheduler_alive, labels)
    };
    let toggle = MenuItem::with_id(
        app,
        "tray-toggle",
        toggle_text,
        toggle_enabled(snapshot.status, processing),
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
/// Since todo 5 the toggle controls the ALAS SCHEDULER through the control
/// API (a short HTTP call on a worker thread, never a long-lived
/// connection):
/// - `StartScheduler` / `StopScheduler`: scheduler-only call, NO window
///   navigation — the webui stays alive.
/// - `StartBackend`: backend down — bring the whole backend up (existing
///   ordering contract inside [`BackendLifecycle::start`]) on a worker
///   thread so the UI thread never blocks (MAJOR-2), navigate to the
///   webui, then start the scheduler via the control API from the SAME
///   worker.
/// - `StopBackend` (degraded: control channel unavailable — webui
///   password/SSL or control patch failure): legacy process-level stop.
///
/// All control-API I/O happens on a worker thread, outside every lock; the
/// decision inputs (`scheduler_alive`, `ws_available`) are computed
/// lock-free here.
fn handle_toggle(app: &AppHandle, shared: &TrayShared, port: u16) {
    // In-flight dedup (plan todo 6f): one scheduler session at a time. The
    // guard is created IMMEDIATELY after the successful compare_exchange and
    // moved into the worker closure below, so the flag clears on every exit
    // path (worker finish, spawn failure, early return, panic).
    let Some(guard) = try_acquire_in_flight(&shared.in_flight) else {
        warn!("Toggle ignored: a previous toggle is still in flight");
        return;
    };

    // 密码/SSL 或控制补丁未应用 → 控制 API 不可用，退回进程级控制。
    let ws_available = crate::deploy_config::ws_control_available()
        && !crate::patch::patch_failed();
    if !ws_available {
        warn!("scheduler control unavailable (webui password/SSL configured or control API patch failed); falling back to process-level toggle");
    }

    // Click-time scheduler liveness — a lock-free re-scan (never the poll
    // cache), plus a 100ms port probe: a dead port while the snapshot still
    // says Running means the backend itself is gone (todo 3d), so fold to
    // Stopped and let the decision take the StartBackend path.
    let mut snapshot = shared.backend.snapshot();
    let scheduler_alive = if snapshot.status == BackendStatus::Running && backend_port_alive(port) {
        shared
            .backend
            .backend_pid()
            .map(|pid| scheduler_alive(uvicorn_alive_child_count(pid, crate::deploy_config::enable_reload())))
            .unwrap_or(false)
    } else if snapshot.status == BackendStatus::Running {
        // Backend died on its own (dead port despite a Running snapshot) —
        // mark the abnormal stop so the status line shows 异常停止, not a
        // plain stop.
        shared.backend.mark_stopped_crashed();
        snapshot = shared.backend.snapshot();
        false
    } else {
        false
    };

    let action = toggle_decision(&snapshot, scheduler_alive, ws_available);
    let labels = load_control_labels(&shared.settings);
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
            // spawn window, which may take up to 60s. The spawn itself runs
            // on a worker thread (MAJOR-2): the menu-event thread must never
            // block on the readiness wait.
            shared.backend.begin_start();
            spawn_start_worker(
                app.clone(),
                Arc::clone(&shared.backend),
                port,
                labels.clone(),
                guard,
                shared.refresh.clone(),
                ws_available,
            );
        }
        ToggleAction::StopScheduler | ToggleAction::StartScheduler => {
            // Scheduler-only control via the control API (HTTP): same ProcessManager
            // the webui button drives. Intent arms BEFORE the call so a scheduler-
            // dead window never flashes 异常停止 while the user's own stop/start is
            // in flight. A failed call only warns — the poll thread self-heals.
            let scheduler_action = match action {
                ToggleAction::StopScheduler => {
                    shared.backend.set_scheduler_intent(SchedulerIntent::Stop);
                    crate::control_api::SchedulerAction::Stop
                }
                _ => {
                    shared.backend.set_scheduler_intent(SchedulerIntent::Start);
                    crate::control_api::SchedulerAction::Start
                }
            };
            spawn_scheduler_call(
                port,
                scheduler_action,
                guard,
                shared.refresh.clone(),
            );
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

/// Worker-thread scheduler control via the control API. The in-flight guard
/// moves into the closure; the refresh wake forces a poll rebuild so the
/// 处理中… toggle is replaced by the real state (same contract as the old
/// WS click worker).
fn spawn_scheduler_call(port: u16, action: crate::control_api::SchedulerAction, guard: InFlightGuard, refresh: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let result = match action {
            crate::control_api::SchedulerAction::Start => crate::control_api::api_scheduler_start(port, "alas"),
            crate::control_api::SchedulerAction::Stop => crate::control_api::api_scheduler_stop(port, "alas"),
        };
        match result {
            Ok(state) => info!(target: "control_api", "scheduler {action:?} ok, state={}", state.state),
            Err(e) => warn!("control API scheduler {action:?} failed: {e}"),
        }
        drop(guard);
        let _ = refresh.send(());
    });
}

/// Spawn the backend start on a dedicated worker thread (MAJOR-2): the
/// up-to-60s spawn window must never block the menu-event (UI) thread.
///
/// The worker owns ONE in-flight guard for the whole start → navigate →
/// scheduler-start sequence. On exit (Ok or Err) the guard clears first,
/// then `refresh` wakes the poll thread: the tail rebuild runs before the
/// worker finishes and renders Initializing, and a failed start trips none
/// of the timed poll's diff gates (Degraded == Degraded, scheduler
/// None → None), so without the wake the tray would stay on 启动中… with
/// the toggle disabled forever. When control is available, a bounded retry
/// thread starts the scheduler through the control API after the backend is
/// ready; the guard is released right after spawning it (the process-tree
/// scan corrects the status within two polls, and the SchedulerIntent::Start
/// TTL covers the window).
fn spawn_start_worker(
    app: AppHandle,
    backend: Arc<BackendLifecycle>,
    port: u16,
    labels: ControlLabels,
    guard: InFlightGuard,
    refresh: mpsc::Sender<()>,
    ws_available: bool,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("backend-start".into())
        .spawn(move || {
            let guard = guard;
            match backend.start(port) {
                Ok(()) => {
                    navigate_main(&app, main_page_url(BackendStatus::Running, port, &labels));
                    // 后端就绪后，经控制 API 启动调度器（控制可用时）。有界重试（10×1.5s，
                    // 镜像旧 SCHEDULER_CLICK_TIMEOUT 15s 预算）；耗尽仅 warn——用户可再点托盘开关。
                    if ws_available {
                        let port = port;
                        std::thread::spawn(move || {
                            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                            loop {
                                match crate::control_api::api_scheduler_start(port, "alas") {
                                    Ok(state) => {
                                        info!(target: "control_api", "scheduler start-after-backend ok, state={}", state.state);
                                        break;
                                    }
                                    Err(e) if std::time::Instant::now() < deadline => {
                                        warn!("control API start-after-backend retry: {e}");
                                        std::thread::sleep(std::time::Duration::from_millis(1500));
                                    }
                                    Err(e) => {
                                        warn!("control API start-after-backend failed after retries: {e}");
                                        break;
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => {
                    warn!("Failed to start backend: {e}");
                    // Status is now Stopped (+ start_failed unless a stop
                    // intervened — set inside the module); the wake below
                    // re-renders the retryable toggle.
                }
            }
            // Clear the in-flight flag BEFORE the wake, so the rebuild the
            // poll thread runs on this signal already sees the finished
            // state (spawn_scheduler_call ordering).
            drop(guard);
            let _ = refresh.send(());
        })
    {
        warn!("Failed to spawn backend-start worker: {e}");
    }
}

/// Rebuild the menu from the current state + task cache and re-attach it to
/// the tray. The single set_menu site: the toggle handler and the poll thread
/// both route through here (never build-then-set inline).
///
/// This is ALSO the single intent-transition site: every rebuild runs the
/// scan result through [`BackendLifecycle::advance_intent_if_changed`] before
/// rendering, so NO status-reporting path (poll thread OR toggle rebuild) can
/// ever show 异常停止 with a stale Start/Stop intent. `scheduler_alive` is
/// None when the backend is not Running; the transition is a no-op then.
fn rebuild_menu(app: &AppHandle, shared: &TrayShared, section: TaskSection, scheduler_alive: Option<bool>) {
    // MINOR-1: the read-compute-write transition runs under ONE lock inside
    // BackendLifecycle — the old snapshot → scheduler_intent_after_scan →
    // set_scheduler_intent sequence took three separate locks, leaving a
    // window a concurrent toggle could slip into.
    shared.backend.advance_intent_if_changed(scheduler_alive);
    let snapshot = shared.backend.snapshot();
    let tasks = shared.tasks.lock().unwrap().clone();
    let labels = load_control_labels(&shared.settings);
    let status_line = status_line_for(
        &snapshot,
        scheduler_alive,
        &labels,
        crate::deploy_config::ws_control_available(),
    );
    // Processing = a scheduler-control click is in flight; rebuilds during
    // that window render the disabled 处理中… toggle instead of the real one.
    let processing = shared.in_flight.load(Ordering::Relaxed);
    let Ok(menu) = build_menu(
        app,
        &snapshot,
        &tasks,
        section,
        &labels,
        scheduler_alive,
        &status_line,
        processing,
    )
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
///
/// `force_rebuild` (a channel-woken poll) skips the diff gate: the worker
/// completes, refresh.send wakes this poll, and the 处理中… toggle rendered by
/// the tail rebuild must be replaced by the real state even when scheduler
/// liveness did not change since the previous poll (see
/// [`menu_model::poll_needs_rebuild`] for the full rationale).
///
/// `notif` is the poll-thread-exclusive notification baseline (see
/// [`PollNotifState`]): this cycle's task cache diff and scheduler-death
/// detection run here, and their baselines update here too.
fn poll_once(
    app: &AppHandle,
    shared: &TrayShared,
    port: u16,
    last_section: &mut TaskSection,
    last_scheduler: &mut Option<bool>,
    force_rebuild: bool,
    notif: &mut PollNotifState,
) {
    // (a) status snapshot — no I/O under the backend lock (the lock is
    // internal to BackendLifecycle; status() takes it briefly).
    let mut status = shared.backend.status();

    // (a2) Backend-liveness re-check: Running but the webui port is dead
    // (the backend crashed or was killed outside the launcher) -> abnormal
    // Stopped via mark_stopped_crashed(), so the toggle recovers to
    // StartBackend instead of showing a stuck Running state. The probe is
    // lock-free. MINOR-5: the marking needs TWO CONSECUTIVE failures — one
    // missed connect can be transient (bind races, packet loss); a success in
    // between resets the streak. The click-time toggle probe stays
    // single-shot: that is a real-time decision on the user's click.
    if status == BackendStatus::Running && !backend_port_alive(port) {
        let streak = shared.port_fail_count.fetch_add(1, Ordering::Relaxed) + 1;
        if should_mark_crashed(streak) {
            shared.backend.mark_stopped_crashed();
            status = BackendStatus::Stopped;
        }
    } else {
        // Successful probe — or no probe this cycle (not Running): the streak
        // only ever counts consecutive failures.
        shared.port_fail_count.store(0, Ordering::Relaxed);
    }

    // (b/c) Running -> fetch outside any lock; anything else -> degraded.
    // A failed clock (now_str Err) also degrades — never a silent
    // all-Waiting menu (the 9999-12-31 sentinel bug). Tasks are read from the
    // payload files (config/alas.json + i18n — the ALAS webui has no JSON
    // API, see alas_tasks module doc).
    let fetched: Result<Vec<Task>, ()> = if status == BackendStatus::Running {
        let alas_dir = std::env::current_dir().unwrap_or_default();
        // Effective UI language for task display names: resolved under a
        // brief settings lock (deploy.yaml read BEFORE the lock — no file
        // I/O under the lock), cloned, and the lock released before the
        // fetch below.
        let deploy_lang = crate::deploy_config::language();
        let task_language = shared
            .settings
            .lock()
            .unwrap()
            .resolved_language(deploy_lang.as_deref());
        match alas_tasks::now_str() {
            // Clock failure -> Err(()) like a fetch failure (now_str already
            // logged the real error); poll_decision degrades the section.
            Ok(now) => alas_tasks::fetch_tasks(&alas_dir, &now, &task_language).map_err(|_| ()),
            Err(_) => Err(()),
        }
    } else {
        Err(())
    };

    // (d) Scheduler liveness for the status line: process-tree scan rooted at
    // the backend pid (lock-free; sysinfo reads the OS process table). No
    // handle / no scan result -> None -> conservative stopped. The intent
    // transition (disarm Start on confirmed alive) runs inside rebuild_menu —
    // the single render choke point — so every status-reporting path shares it.
    let scheduler = if status == BackendStatus::Running {
        shared.backend.backend_pid().map(|pid| {
            scheduler_alive(uvicorn_alive_child_count(pid, crate::deploy_config::enable_reload()))
        })
    } else {
        None
    };

    // ---- 通知检测（macOS-only，随 poll 线程） ----
    let settings = shared.settings.lock().unwrap().clone();
    // 通知文案模板：来自 menu_model::shell_menu_labels 纯函数表，非硬编码。
    let nlabels = crate::menu_model::shell_menu_labels(
        &settings.resolved_language(crate::deploy_config::language().as_deref()),
    );
    // ① 任务完成：NextRun 前移（默认静默，开关控制）。
    let tasks = shared.tasks.lock().unwrap().clone();
    if let Some(prev) = &notif.last_tasks {
        for ev in crate::notify::diff_tasks(prev, &tasks) {
            if crate::notify::should_notify(&ev, &settings) {
                let body = match &ev {
                    crate::notify::NotifyEvent::TaskComplete { name, next_time, .. } =>
                        nlabels.notify_task_done.replace("{name}", name).replace("{next_time}", next_time),
                    crate::notify::NotifyEvent::SchedulerDeath { name } =>
                        nlabels.notify_death_body.replace("{name}", name),
                };
                let _ = app.notification().builder().title("ALAS").body(body).show();
            }
        }
    }
    notif.last_tasks = Some(tasks);

    // ② 调度器异常死亡：liveness 翻转为主信号，state 1→3 为附加信号。
    //    prev 读更新前的 *last_scheduler（本块位于既有扫描之后、写回之前）；
    //    dead_streak 连续 2 次判死才触发（滤掉更新/重启瞬断）。
    let prev_alive = *last_scheduler;
    let now_alive: Option<bool> = scheduler;
    // 闸门须为 intent==None——`!= Stop` 会放行用户自己的 Start 引导窗口
    // （死亡 streak=1 时点 Start → 下一拍 streak=2 → 误报）。异常死亡时
    // intent 必为 None（Start 在确认存活时已被 advance_intent_if_changed
    // 解除）；Stop 时调度器已死无意义。
    let gates_ok = shared.backend.status() == BackendStatus::Running
        && matches!(shared.backend.snapshot().scheduler_intent, SchedulerIntent::None);
    // 附加信号：API 可用时 state 1→3（renderables 无残留污染时也权威）。仅更新基线。
    // state 通道有意不加 gates——1→3 语义无歧义（用户 Stop → 追加 Manual stop → 2，
    // 后端下线 → api 调用失败 → false），加闸反而会漏掉 Start 引导期的真实崩溃。
    let state_died = if let Ok(list) = crate::control_api::api_instances(port) {
        let state = list.iter().find(|i| i.name == "alas").map(|i| i.state);
        let died = crate::notify::death_by_state(notif.last_instance_state, state);
        notif.last_instance_state = state;
        died
    } else {
        false
    };
    // 纯决策（notify::death_notify_decision）：双通道 + 闸门 + 剧集锁存
    // （Round-1 F-MEDIUM：state 1→3 与 liveness streak=2 相邻周期各自触发时
    // 只发一次；复活或后端停止运行才复位）。
    let died = crate::notify::death_notify_decision(
        prev_alive,
        now_alive,
        gates_ok,
        status == BackendStatus::Running,
        state_died,
        &mut notif.dead_streak,
        &mut notif.death_notified,
    );
    if died {
        let ev = crate::notify::NotifyEvent::SchedulerDeath { name: "alas".into() };
        if crate::notify::should_notify(&ev, &settings) {
            let _ = app.notification().builder()
                .title("ALAS")
                .body(nlabels.notify_death_body.replace("{name}", "alas"))
                .show();
        }
    }

    // Decision is pure (menu_model::poll_decision): the clock string and the
    // fetch result are injected, so this call site carries no decision logic.
    let outcome = poll_decision(status, fetched, *last_section, &shared.tasks.lock().unwrap());
    if let Some(tasks) = outcome.replace_cache {
        *shared.tasks.lock().unwrap() = tasks;
    }
    let status_line_changed = scheduler != *last_scheduler;
    *last_scheduler = scheduler;
    if poll_needs_rebuild(force_rebuild, outcome.changed, status_line_changed) {
        rebuild_menu(app, shared, outcome.section, scheduler);
        *last_section = outcome.section;
    }
}

/// Whether the webui port answers within 100ms — the backend-liveness probe.
/// Lock-free: a plain connect_timeout, no backend lock held. Used by poll now
/// (dead port -> mark_stopped) and by the toggle decision later (todo 6).
fn backend_port_alive(port: u16) -> bool {
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok()
}

/// Pure two-strike decision (MINOR-5): mark the backend crashed only after
/// TWO CONSECUTIVE failed port probes. `fail_count` is the caller's current
/// consecutive-failure streak (incremented BEFORE the call); a success in
/// between resets it to 0, so one transient miss never kills a Running
/// backend. The click-time toggle probe stays single-shot — that is a
/// real-time decision on the user's click.
fn should_mark_crashed(fail_count: u8) -> bool {
    fail_count >= 2
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

/// Tray control labels resolved from the shared shell settings' effective
/// language (todo 4: settings override → deploy.yaml → zh-CN) + the ALAS
/// i18n file (current dir); missing payload → built-in zh-CN table, never
/// panic. Called per menu build/poll, so a live language switch re-renders
/// the tray without a restart.
fn load_control_labels(settings: &Arc<Mutex<crate::shell_settings::ShellSettings>>) -> ControlLabels {
    // deploy.yaml is file I/O — read it BEFORE taking the settings lock.
    let deploy_lang = crate::deploy_config::language();
    let lang = settings
        .lock()
        .unwrap()
        .resolved_language(deploy_lang.as_deref());
    let alas_dir = std::env::current_dir().unwrap_or_default();
    let i18n = alas_tasks::load_i18n(&alas_dir, &lang);
    control_labels(&lang, &i18n)
}

/// Re-navigate the main window to a freshly localized stopped page after a
/// language switch (todo 4). The stopped-page copy is baked into its data:
/// URL at build time, so a language change must rebuild it; the webui URL
/// (backend Running) is language-independent and needs no refresh. Called by
/// the main.rs menu-event orchestration; a no-op while the backend runs.
pub(crate) fn refresh_stopped_page(
    app: &AppHandle,
    backend: &BackendLifecycle,
    settings: &Arc<Mutex<crate::shell_settings::ShellSettings>>,
    port: u16,
) {
    if backend.snapshot().status == BackendStatus::Running {
        return;
    }
    let labels = load_control_labels(settings);
    navigate_main(app, main_page_url(BackendStatus::Stopped, port, &labels));
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
        // Live call must not panic regardless of ambient cwd; the language
        // now resolves from the shared settings (None → deploy.yaml → zh-CN).
        let settings = Arc::new(Mutex::new(crate::shell_settings::ShellSettings::default()));
        let live = load_control_labels(&settings);
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

    // ---- MINOR-5 two-strike port probe ---------------------------------------

    #[test]
    fn should_mark_crashed_requires_two_consecutive_failures() {
        // A single probe failure must NOT kill a Running backend (a transient
        // miss — port bind race, packet loss); only >= 2 consecutive failures
        // earn the abnormal-stop marking.
        assert!(!should_mark_crashed(0), "no failures -> healthy");
        assert!(!should_mark_crashed(1), "one failure -> keep Running");
        assert!(should_mark_crashed(2), "two failures -> crashed");
        assert!(should_mark_crashed(3), "more failures stay crashed");
    }

    #[test]
    fn port_fail_counter_state_machine() {
        // The caller protocol replicated: failed probe -> fetch_add(1), any
        // successful probe (or a non-Running status) -> store(0). The counter
        // only ever counts CONSECUTIVE failures, so the decision sees the
        // current streak, not a lifetime total.
        let counter = Arc::new(AtomicU8::new(0));
        let probe_fail = |c: &AtomicU8| c.fetch_add(1, Ordering::Relaxed) + 1;
        let probe_ok = |c: &AtomicU8| c.store(0, Ordering::Relaxed);

        // fail once -> not crashed
        assert!(!should_mark_crashed(probe_fail(&counter)));
        // success in between -> reset
        probe_ok(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0, "success resets the streak");
        // fail twice consecutively -> crashed on the second
        assert!(!should_mark_crashed(probe_fail(&counter)));
        assert!(should_mark_crashed(probe_fail(&counter)));
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
