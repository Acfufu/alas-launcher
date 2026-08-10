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

/// Status line for the (disabled) status menu item.
pub fn label_for(status: BackendStatus) -> &'static str {
    match status {
        BackendStatus::Initializing => "Backend: initializing…",
        BackendStatus::Running => "Backend: running",
        BackendStatus::Stopped => "Backend: stopped",
    }
}

/// Text of the Start/Stop toggle item.
pub fn toggle_label(status: BackendStatus) -> &'static str {
    match status {
        BackendStatus::Running => "Stop Backend",
        BackendStatus::Stopped | BackendStatus::Initializing => "Start Backend",
    }
}

/// Whether the toggle item is clickable for the given status (disabled while
/// initializing so a 60s spawn window can never get a second toggle).
pub fn toggle_enabled(status: BackendStatus) -> bool {
    matches!(status, BackendStatus::Running | BackendStatus::Stopped)
}

/// Full status text for the status row: "start failed" outranks the plain
/// status label.
pub fn status_text(snapshot: &BackendStateSnapshot) -> String {
    if snapshot.start_failed {
        "Backend: start failed".to_string()
    } else {
        label_for(snapshot.status).to_string()
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
    fn status_text_prefers_start_failed() {
        let stopped_failed = BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: true,
        };
        assert_eq!(status_text(&stopped_failed), "Backend: start failed");
        let stopped_clean = BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: false,
        };
        assert_eq!(status_text(&stopped_clean), "Backend: stopped");
        let running_clean = BackendStateSnapshot {
            status: BackendStatus::Running,
            start_failed: false,
        };
        assert_eq!(status_text(&running_clean), "Backend: running");
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
}
