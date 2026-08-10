// Pure menu-row model for the tray: the complete menu CONTENT logic —
// grouping, the 3-item cap, labels, row structure, section classification,
// change detection — with NO tauri types and NO cfg attributes. It is the
// deep module behind the tray menu: testable anywhere (no tauri runtime
// needed), compiled only where its macOS-only dependency (`alas_tasks`,
// gated in main.rs) exists.
//
// Adapter seam: src/tray.rs is the ONLY place `tauri::menu::*` types appear.
// It consumes these rows and turns them into native menu items; this module
// never knows a menu bar exists.

use std::collections::HashSet;

use crate::{
    alas_tasks::{self, Task},
    backend::{BackendStateSnapshot, BackendStatus},
};

/// Which rendering the task section of the menu should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSection {
    /// Backend not running, or the task fetch failed — "Tasks: unavailable".
    Degraded,
    /// Backend running, zero tasks known — "No tasks".
    Empty,
    /// Backend running with a real task list.
    Tasks,
}

/// Menu compactness: each status group shows at most the 3 soonest tasks.
/// The task slice is sorted by `next_time` ascending (see fetch_tasks), so the
/// first items of a group are the soonest — the top of the queue.
pub const TASK_GROUP_MAX: usize = 3;

/// One renderable row of the tray's task section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskMenuItem {
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
pub fn task_section_items(tasks: &[Task]) -> Vec<TaskMenuItem> {
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

/// The task-section rendering implied by backend status + fetch result.
pub fn task_section(status: BackendStatus, fetch: Result<Vec<Task>, ()>) -> TaskSection {
    if status != BackendStatus::Running {
        return TaskSection::Degraded;
    }
    match fetch {
        Err(()) => TaskSection::Degraded,
        Ok(tasks) if tasks.is_empty() => TaskSection::Empty,
        Ok(_) => TaskSection::Tasks,
    }
}

/// Whether the command-name set differs between the old and new task lists,
/// used to decide whether a rebuild is needed. True iff any command in OLD is
/// missing from NEW, or any command in NEW is missing from OLD.
///
/// Identity is the `command` key, NOT the display `name`: names come from the
/// i18n file (`Task.<command>.name`) and can change with the webui language,
/// while the command equals the stable alas.json top-level key.
pub fn menu_diff_changed(old: &[Task], new: &[Task]) -> bool {
    let new_commands: HashSet<&str> = new.iter().map(|t| t.command.as_str()).collect();
    let old_commands: HashSet<&str> = old.iter().map(|t| t.command.as_str()).collect();
    old.iter().any(|t| !new_commands.contains(t.command.as_str()))
        || new.iter().any(|t| !old_commands.contains(t.command.as_str()))
}

/// One poll cycle's decision: the section to render, whether the menu must be
/// rebuilt, and the new cache contents when they changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    /// Section the task area should render next.
    pub section: TaskSection,
    /// Whether the menu must be rebuilt (section kind changed, or the task
    /// command set changed).
    pub changed: bool,
    /// New task cache contents when `changed` (None otherwise — the caller
    /// keeps the old cache).
    pub replace_cache: Option<Vec<Task>>,
}

/// Pure poll-cycle decision: given the backend status, the fetch result, the
/// currently-rendered section and the cached task list, what to render next.
///
/// This is the EXACT decision previously inlined in `tray::poll_once` —
/// extracted so the rebuild logic is testable without threads, files or
/// processes. It never touches I/O: the clock string and the fetch result are
/// injected (the clock seam is `alas_tasks::fetch_tasks` already taking the
/// `now_str` parameter), so the tests below lock the rebuild logic itself.
pub fn poll_decision(
    status: BackendStatus,
    fetched: Result<Vec<Task>, ()>,
    last_section: TaskSection,
    cached: &[Task],
) -> PollOutcome {
    if status != BackendStatus::Running {
        return PollOutcome {
            section: TaskSection::Degraded,
            changed: last_section != TaskSection::Degraded,
            replace_cache: None,
        };
    }
    match fetched {
        Err(()) => PollOutcome {
            section: TaskSection::Degraded,
            changed: last_section != TaskSection::Degraded,
            replace_cache: None,
        },
        Ok(tasks) => {
            let section = if tasks.is_empty() {
                TaskSection::Empty
            } else {
                TaskSection::Tasks
            };
            let changed = last_section != section || menu_diff_changed(cached, &tasks);
            PollOutcome {
                section,
                changed,
                replace_cache: changed.then_some(tasks),
            }
        }
    }
}

/// Whether a poll cycle must rebuild the menu.
///
/// A channel-woken poll (`force_rebuild` — manual Refresh, or the
/// worker-complete wake after an in-flight scheduler-control click) ALWAYS
/// rebuilds: the 处理中… toggle rendered by the tail rebuild must be replaced
/// by the real state even when the scheduler-liveness edge detector consumed
/// its flip while `processing` was still true (see the stuck-processing bug:
/// the worker finishes, the wake lands, but scan liveness is unchanged, so
/// without force the gate skips and the menu stays 处理中… forever).
pub fn poll_needs_rebuild(force_rebuild: bool, changed: bool, status_line_changed: bool) -> bool {
    force_rebuild || changed || status_line_changed
}

/// Text of the Start/Stop toggle item.
///
/// "停止" (labels.stop) only while the backend is Running AND the scheduler
/// scan confirms liveness (`Some(true)`); every other combination shows
/// "启动" (labels.start) — Running with a scan result of `Some(false)` or
/// an unknown scan (`None`) never claims the scheduler is running
/// (conservative, same rule as [`status_line_for`]).
pub fn toggle_label(
    status: BackendStatus,
    scheduler_alive: Option<bool>,
    labels: &ControlLabels,
) -> String {
    match status {
        BackendStatus::Running => {
            if scheduler_alive == Some(true) {
                labels.stop.clone()
            } else {
                labels.start.clone()
            }
        }
        BackendStatus::Stopped | BackendStatus::Initializing => labels.start.clone(),
    }
}

/// Whether the toggle item is clickable for the given status: disabled while
/// initializing (a 60s spawn window can never get a second toggle) or while a
/// scheduler-control click is in flight (the 处理中… state).
pub fn toggle_enabled(status: BackendStatus, processing: bool) -> bool {
    !processing && matches!(status, BackendStatus::Running | BackendStatus::Stopped)
}

/// Status line for the (disabled) status menu item, composed as
/// `{scheduler}{sep}{word}` ("调度器：运行中"); "start failed" outranks the
/// plain status word. While the backend is Running the word reflects the
/// SCHEDULER: `scheduler_alive` is the process-tree discriminator result
/// (Some), or None when no process handle / scan result exists — conservative
/// stopped (evidence task-3: unknown must never claim running).
pub fn status_line_for(
    snapshot: &BackendStateSnapshot,
    scheduler_alive: Option<bool>,
    labels: &ControlLabels,
) -> String {
    let word = if snapshot.start_failed {
        &labels.failed
    } else {
        match snapshot.status {
            BackendStatus::Initializing => &labels.initializing,
            BackendStatus::Stopped => &labels.stopped,
            BackendStatus::Running => {
                if scheduler_alive == Some(true) {
                    &labels.running
                } else {
                    &labels.stopped
                }
            }
        }
    };
    format!("{}{}{}", labels.scheduler, labels.sep, word)
}

/// Whether the ALAS scheduler is running, per the pinned process-tree
/// discriminator (evidence task-3): the uvicorn process's alive, non-zombie,
/// non-resource-tracker child count. The multiprocessing.Manager is the
/// permanent baseline child (+1); the scheduler adds a second one.
pub fn scheduler_alive(alive_child_count: usize) -> bool {
    alive_child_count > 1
}

/// Localized labels for the tray's scheduler-control rows (status line +
/// Start/Stop toggle), following the webui language. `scheduler`, `running`,
/// `start` and `stop` come from the ALAS i18n file when the keys exist and
/// are strings; `stopped`, `initializing`, `failed` and `sep` ALWAYS come
/// from the built-in table. `sep` also decides the stopped-page template
/// style: full-width "：" → zh template, half-width ": " → en template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlLabels {
    pub scheduler: String,
    pub running: String,
    pub stopped: String,
    pub initializing: String,
    pub failed: String,
    pub start: String,
    pub stop: String,
    pub processing: String,
    pub sep: String,
}

/// Built-in label table, keyed by webui language; any unknown or empty
/// language falls back to zh-CN.
fn builtin_labels(lang: &str) -> ControlLabels {
    let (scheduler, running, stopped, initializing, failed, start, stop, processing, sep) = match lang {
        "zh-TW" => (
            "調度器",
            "執行中",
            "已停止",
            "啟動中…",
            "啟動失敗",
            "啟動",
            "停止",
            "處理中…",
            "：",
        ),
        "en-US" => (
            "Scheduler",
            "Running",
            "stopped",
            "initializing…",
            "start failed",
            "Start",
            "Stop",
            "Processing…",
            ": ",
        ),
        "ja-JP" => (
            "スケジューラー",
            "実行中",
            "停止済み",
            "起動中…",
            "起動失敗",
            "実行",
            "中止",
            "処理中…",
            // Half-width ": " (en template), NOT the full-width "：".
            ": ",
        ),
        // zh-CN doubles as the fallback for any unknown or empty language.
        _ => (
            "调度器",
            "运行中",
            "已停止",
            "启动中…",
            "启动失败",
            "启动",
            "停止",
            "处理中…",
            "：",
        ),
    };
    ControlLabels {
        scheduler: scheduler.into(),
        running: running.into(),
        stopped: stopped.into(),
        initializing: initializing.into(),
        failed: failed.into(),
        start: start.into(),
        stop: stop.into(),
        processing: processing.into(),
        sep: sep.into(),
    }
}

/// The tray's scheduler-control labels for `lang` and the parsed i18n file:
/// payload values win for scheduler/running/start/stop when present and
/// string-typed, everything else falls back to the built-in table. Pure —
/// no file I/O, no tauri types.
pub fn control_labels(lang: &str, i18n: &serde_json::Value) -> ControlLabels {
    let mut labels = builtin_labels(lang);
    if let Some(s) = i18n["Gui"]["Overview"]["Scheduler"].as_str() {
        labels.scheduler = s.to_string();
    }
    if let Some(s) = i18n["Gui"]["Overview"]["Running"].as_str() {
        labels.running = s.to_string();
    }
    if let Some(s) = i18n["Gui"]["Button"]["Start"].as_str() {
        labels.start = s.to_string();
    }
    if let Some(s) = i18n["Gui"]["Button"]["Stop"].as_str() {
        labels.stop = s.to_string();
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_label_matrix() {
        // The toggle shows the stop word only when the backend is Running AND
        // the scheduler scan confirms liveness; every other combination shows
        // start (Running + unknown scan is conservative: never claim running).
        for lang in ["zh-CN", "zh-TW", "en-US", "ja-JP"] {
            let labels = expected_labels(lang);
            assert_eq!(
                toggle_label(BackendStatus::Running, Some(true), &labels),
                labels.stop,
                "lang {lang}"
            );
            assert_eq!(
                toggle_label(BackendStatus::Running, Some(false), &labels),
                labels.start,
                "lang {lang}"
            );
            assert_eq!(
                toggle_label(BackendStatus::Running, None, &labels),
                labels.start,
                "lang {lang}"
            );
            assert_eq!(
                toggle_label(BackendStatus::Stopped, Some(true), &labels),
                labels.start,
                "lang {lang}"
            );
            assert_eq!(
                toggle_label(BackendStatus::Stopped, None, &labels),
                labels.start,
                "lang {lang}"
            );
            assert_eq!(
                toggle_label(BackendStatus::Initializing, Some(false), &labels),
                labels.start,
                "lang {lang}"
            );
        }
    }

    #[test]
    fn toggle_enabled_matrix() {
        // Disabled while initializing (60s spawn window) or while a scheduler
        // click is in flight (处理中…); every other combination is clickable.
        for processing in [false, true] {
            assert_eq!(toggle_enabled(BackendStatus::Running, processing), !processing);
            assert_eq!(toggle_enabled(BackendStatus::Stopped, processing), !processing);
            assert!(!toggle_enabled(BackendStatus::Initializing, processing));
        }
    }

    #[test]
    fn status_line_for_prefers_start_failed() {
        let labels = expected_labels("zh-CN");
        let stopped_failed = BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: true,
        };
        assert_eq!(status_line_for(&stopped_failed, None, &labels), "调度器：启动失败");
        let stopped_clean = BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: false,
        };
        assert_eq!(status_line_for(&stopped_clean, Some(true), &labels), "调度器：已停止");
        let initializing_clean = BackendStateSnapshot {
            status: BackendStatus::Initializing,
            start_failed: false,
        };
        assert_eq!(
            status_line_for(&initializing_clean, None, &labels),
            "调度器：启动中…"
        );
    }

    #[test]
    fn status_line_for_running_follows_scheduler_discriminator() {
        let labels = expected_labels("zh-CN");
        let running = BackendStateSnapshot {
            status: BackendStatus::Running,
            start_failed: false,
        };
        // Scheduler confirmed alive -> running; confirmed dead OR scan
        // unknown (None) -> conservative stopped.
        assert_eq!(status_line_for(&running, Some(true), &labels), "调度器：运行中");
        assert_eq!(status_line_for(&running, Some(false), &labels), "调度器：已停止");
        assert_eq!(status_line_for(&running, None, &labels), "调度器：已停止");
        // Non-Running statuses ignore the discriminator entirely.
        assert_eq!(status_line_for(&stopped_snapshot(), Some(true), &labels), "调度器：已停止");
        let initializing = BackendStateSnapshot {
            status: BackendStatus::Initializing,
            start_failed: false,
        };
        assert_eq!(
            status_line_for(&initializing, Some(false), &labels),
            "调度器：启动中…"
        );
    }

    #[test]
    fn scheduler_alive_count_boundaries() {
        // Pinned rule (evidence task-3): Manager baseline = 1 -> not alive;
        // a second alive child (the scheduler) crosses the threshold.
        assert!(!scheduler_alive(0), "no children");
        assert!(!scheduler_alive(1), "Manager baseline only");
        assert!(scheduler_alive(2), "Manager + scheduler");
        assert!(scheduler_alive(3), "any extra child counts alive");
    }

    fn stopped_snapshot() -> BackendStateSnapshot {
        BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: false,
        }
    }

    // ---- control_labels -------------------------------------------------------

    /// i18n payload fixture with the four control keys set.
    fn i18n_value(scheduler: &str, running: &str, start: &str, stop: &str) -> serde_json::Value {
        serde_json::json!({
            "Gui": {
                "Overview": { "Scheduler": scheduler, "Running": running },
                "Button": { "Start": start, "Stop": stop },
            }
        })
    }

    /// Expected full label set per language, from the REAL ALAS payload
    /// cross-check (evidence task-1: zero delta vs the built-in table).
    fn expected_labels(lang: &str) -> ControlLabels {
        let (scheduler, running, stopped, initializing, failed, start, stop, processing, sep) =
            match lang {
                "zh-TW" => ("調度器", "執行中", "已停止", "啟動中…", "啟動失敗", "啟動", "停止", "處理中…", "："),
                "en-US" => ("Scheduler", "Running", "stopped", "initializing…", "start failed", "Start", "Stop", "Processing…", ": "),
                "ja-JP" => ("スケジューラー", "実行中", "停止済み", "起動中…", "起動失敗", "実行", "中止", "処理中…", ": "),
                _ => ("调度器", "运行中", "已停止", "启动中…", "启动失败", "启动", "停止", "处理中…", "："),
            };
        ControlLabels {
            scheduler: scheduler.into(),
            running: running.into(),
            stopped: stopped.into(),
            initializing: initializing.into(),
            failed: failed.into(),
            start: start.into(),
            stop: stop.into(),
            processing: processing.into(),
            sep: sep.into(),
        }
    }

    #[test]
    fn control_labels_matrix_all_fields_four_languages() {
        // Payload values (verified in evidence) drive the i18n-sourced
        // fields; every field of every language asserted exactly.
        let cases = [
            ("zh-CN", "调度器", "运行中", "启动", "停止", "处理中…"),
            ("zh-TW", "調度器", "執行中", "啟動", "停止", "處理中…"),
            ("en-US", "Scheduler", "Running", "Start", "Stop", "Processing…"),
            ("ja-JP", "スケジューラー", "実行中", "実行", "中止", "処理中…"),
        ];
        for (lang, scheduler, running, start, stop, processing) in cases {
            let labels = control_labels(lang, &i18n_value(scheduler, running, start, stop));
            assert_eq!(labels, expected_labels(lang), "lang {lang}");
            assert_eq!(labels.processing, processing, "lang {lang} processing");
        }
    }

    #[test]
    fn control_labels_empty_i18n_uses_builtin_table() {
        let empty = serde_json::json!({});
        for lang in ["zh-CN", "zh-TW", "en-US", "ja-JP"] {
            let labels = control_labels(lang, &empty);
            assert_eq!(labels, expected_labels(lang), "lang {lang}");
        }
    }

    #[test]
    fn control_labels_unknown_language_falls_back_zh_cn() {
        let empty = serde_json::json!({});
        for lang in ["xx-XX", "", "zh-cn", "EN"] {
            let labels = control_labels(lang, &empty);
            assert_eq!(labels, expected_labels("zh-CN"), "lang {lang:?}");
        }
    }

    #[test]
    fn control_labels_non_string_values_fall_back_to_builtin() {
        // Keys present but wrong-typed: number, null, object, array — every
        // field must come from the built-in table instead.
        let bogus = serde_json::json!({
            "Gui": {
                "Overview": { "Scheduler": 42, "Running": null },
                "Button": { "Start": { "x": 1 }, "Stop": ["no"] },
            }
        });
        for lang in ["zh-CN", "zh-TW", "en-US", "ja-JP"] {
            let labels = control_labels(lang, &bogus);
            assert_eq!(labels, expected_labels(lang), "lang {lang}");
        }
    }

    #[test]
    fn control_labels_partial_payload_overrides_only_present_keys() {
        let partial = serde_json::json!({
            "Gui": { "Overview": { "Scheduler": "OnlyThis" } }
        });
        let labels = control_labels("zh-CN", &partial);
        assert_eq!(labels.scheduler, "OnlyThis");
        assert_eq!(labels.running, expected_labels("zh-CN").running);
        assert_eq!(labels.start, expected_labels("zh-CN").start);
        assert_eq!(labels.stop, expected_labels("zh-CN").stop);
        assert_eq!(labels.sep, expected_labels("zh-CN").sep);
    }

    #[test]
    fn task_section_items_all_three_groups() {
        let tasks = vec![
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
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
                status: alas_tasks::TaskStatus::Waiting,
                next_time: Some("2026-08-11 00:00:00".into()),
            },
            Task {
                name: "主线图-2".into(),
                command: "Main2".into(),
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
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
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
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
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
                status: alas_tasks::TaskStatus::Running,
                next_time: Some("2026-08-10 20:14:24".into()),
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                status: alas_tasks::TaskStatus::Queued,
                next_time: Some("2026-08-10 21:00:00".into()),
            },
            Task {
                name: "演习".into(),
                command: "Exercise".into(),
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
            alas_tasks::fetch_tasks(payload, &alas_tasks::now_str().unwrap(), "zh-CN").unwrap();
        for item in task_section_items(&tasks) {
            match item {
                TaskMenuItem::GroupHeader { id, text } => println!("[{id}] {text}"),
                TaskMenuItem::TaskItem { id, text } => println!("[{id}] {text}"),
                TaskMenuItem::Separator => println!("[separator]"),
            }
        }
    }

    #[test]
    fn menu_diff_changed_unchanged_is_false() {
        let old = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
            ..Default::default()
        }];
        // Same command set (status changed) -> no structural change.
        let new = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
            status: alas_tasks::TaskStatus::Waiting,
            next_time: Some("2026-08-11 00:00:00".into()),
        }];
        assert!(!menu_diff_changed(&old, &new));
        assert!(!menu_diff_changed(&[], &[]));
    }

    #[test]
    fn menu_diff_changed_add_scenario() {
        let old: Vec<Task> = vec![];
        let new = vec![
            Task {
                name: "每日任务".into(),
                command: "Daily".into(),
                ..Default::default()
            },
            Task {
                name: "困难图".into(),
                command: "Hard".into(),
                ..Default::default()
            },
        ];
        assert!(menu_diff_changed(&old, &new));
    }

    #[test]
    fn menu_diff_changed_remove_scenario() {
        let old = vec![
            Task {
                name: "每日任务".into(),
                command: "Daily".into(),
                ..Default::default()
            },
            Task {
                name: "战术学院".into(),
                command: "Tactical".into(),
                ..Default::default()
            },
            Task {
                name: "大舰队".into(),
                command: "Guild".into(),
                ..Default::default()
            },
        ];
        let new = vec![Task {
            name: "战术学院".into(),
            command: "Tactical".into(),
            ..Default::default()
        }];
        assert!(menu_diff_changed(&old, &new));
    }

    #[test]
    fn menu_diff_changed_same_commands_different_names_no_change() {
        // i18n display names may change (e.g. webui language switch) while
        // commands stay stable — identity must be the command.
        let old = vec![Task {
            name: "主线图".into(),
            command: "Main".into(),
            ..Default::default()
        }];
        let new = vec![Task {
            name: "Main".into(), // i18n gone -> name falls back to command
            command: "Main".into(),
            ..Default::default()
        }];
        assert!(!menu_diff_changed(&old, &new));
    }

    #[test]
    fn task_section_matrix() {
        let one = vec![Task {
            name: "每日任务".into(),
            command: "Daily".into(),
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

    // ---- poll_decision --------------------------------------------------------

    /// Minimal task fixture: command doubles as name, all else defaulted.
    fn task(cmd: &str) -> Task {
        Task {
            name: cmd.to_string(),
            command: cmd.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn poll_needs_rebuild_matrix() {
        // force_rebuild | changed | status_line_changed | expected
        let cases: [(bool, bool, bool, bool); 8] = [
            (false, false, false, false),
            (false, false, true, true),
            (false, true, false, true),
            (false, true, true, true),
            (true, false, false, true),
            (true, false, true, true),
            (true, true, false, true),
            (true, true, true, true),
        ];
        for (force, changed, status_line_changed, expected) in cases {
            assert_eq!(
                poll_needs_rebuild(force, changed, status_line_changed),
                expected,
                "force_rebuild={force}, changed={changed}, status_line_changed={status_line_changed}",
            );
        }
        // force=true always wins, regardless of the edge detectors.
        for changed in [false, true] {
            for status_line_changed in [false, true] {
                assert!(poll_needs_rebuild(true, changed, status_line_changed));
            }
        }
    }

    #[test]
    fn poll_decision_running_ok_changed_replaces_cache() {
        let cached = vec![task("Daily")];
        let new = vec![task("Daily"), task("Hard")];
        let outcome = poll_decision(
            BackendStatus::Running,
            Ok(new.clone()),
            TaskSection::Empty,
            &cached,
        );
        assert_eq!(outcome.section, TaskSection::Tasks);
        assert!(outcome.changed);
        assert_eq!(outcome.replace_cache, Some(new));
    }

    #[test]
    fn poll_decision_running_ok_unchanged_keeps_cache() {
        let cached = vec![task("Daily")];
        let outcome = poll_decision(
            BackendStatus::Running,
            Ok(cached.clone()),
            TaskSection::Tasks,
            &cached,
        );
        assert_eq!(outcome.section, TaskSection::Tasks);
        assert!(!outcome.changed);
        assert_eq!(outcome.replace_cache, None);
    }

    #[test]
    fn poll_decision_running_err_degrades() {
        let from_tasks =
            poll_decision(BackendStatus::Running, Err(()), TaskSection::Tasks, &[task("Daily")]);
        assert_eq!(from_tasks.section, TaskSection::Degraded);
        assert!(from_tasks.changed); // Tasks -> Degraded is a render change
        assert_eq!(from_tasks.replace_cache, None);
        // Already degraded -> no rebuild churn.
        let from_degraded = poll_decision(BackendStatus::Running, Err(()), TaskSection::Degraded, &[]);
        assert!(!from_degraded.changed);
    }

    #[test]
    fn poll_decision_not_running_always_degrades() {
        for status in [BackendStatus::Stopped, BackendStatus::Initializing] {
            let from_tasks = poll_decision(status, Ok(vec![task("Daily")]), TaskSection::Tasks, &[]);
            assert_eq!(from_tasks.section, TaskSection::Degraded);
            assert!(from_tasks.changed);
            assert_eq!(from_tasks.replace_cache, None);
            let from_empty = poll_decision(status, Ok(vec![]), TaskSection::Empty, &[]);
            assert!(from_empty.changed);
            let from_degraded = poll_decision(status, Err(()), TaskSection::Degraded, &[]);
            assert!(!from_degraded.changed);
        }
    }

    #[test]
    fn poll_decision_initial_empty_stays_empty() {
        let outcome = poll_decision(BackendStatus::Running, Ok(vec![]), TaskSection::Empty, &[]);
        assert_eq!(outcome.section, TaskSection::Empty);
        assert!(!outcome.changed);
        assert_eq!(outcome.replace_cache, None);
    }

    #[test]
    fn poll_decision_cache_replacement_carries_new_tasks() {
        let new = vec![task("Guild")];
        let outcome = poll_decision(BackendStatus::Running, Ok(new.clone()), TaskSection::Empty, &[]);
        assert_eq!(outcome.replace_cache, Some(new));
    }
}
