//! ALAS task data source for the tray menu's task-list section.
//!
//! Data source (mirrors the webui, verified live — see
//! `.omo/evidence/alas-discovery.md`): the merged scheduler file
//! `<alas_dir>/config/alas.json` plus the display-name i18n file
//! `<alas_dir>/module/config/i18n/<language>.json`.
//!
//! There is NO HTTP API: the ALAS webui is PyWebIO (server-rendered over a
//! WebSocket); every previously probed JSON endpoint (`/api/tasks`,
//! `/api/scheduler`, `/tasks`, `/api/task/list`) returns 404. This module is
//! therefore a pure file reader.
//!
//! Parsing mirrors `module/config/config.py` (`get_next_task`): top-level
//! keys carrying a `Scheduler` object are tasks; `Scheduler.Enable == true`
//! marks them enabled — disabled tasks (or entries with no `Scheduler` group,
//! e.g. `Alas`/`General`) are SKIPPED from the result, exactly like the webui
//! (`if not func.enable: continue`). `Scheduler.NextRun` is a zero-padded
//! `"YYYY-MM-DD HH:MM:SS"` string (lexicographic order == chronological
//! order); the display name comes from `Task.<Command>.name` in the i18n
//! file, falling back to the command key.
//!
//! Status classification mirrors the webui overview (`module/webui/app.py`
//! `alas_update_overview_task`): enabled tasks whose `NextRun <= now` are
//! past-due — the FIRST one renders as Running (运行中), the rest as Queued
//! (队列中) — and future ones as Waiting (等待中). The launcher has no
//! scheduler-liveness channel, so it always treats the first past-due task as
//! Running; that is a documented approximation (the webui only does so while
//! the ALAS scheduler process is alive).

use std::{
    cmp::Ordering,
    path::Path,
};

use anyhow::{anyhow, Result};
use tracing::warn;

/// A single ALAS scheduler task as shown (read-only) in the tray menu.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Task {
    /// Display name from i18n (`Task.<command>.name`), command key as fallback.
    pub name: String,
    /// `Scheduler.Command` — equals the top-level alas.json key. Stable
    /// identity for menu diffs (display names are i18n-dependent).
    pub command: String,
    /// `Scheduler.Enable` (only enabled tasks are listed).
    pub enabled: bool,
    /// Running / Queued / Waiting, per [`classify`] + fetch-time post-processing.
    pub status: TaskStatus,
    /// `Scheduler.NextRun` — `"YYYY-MM-DD HH:MM:SS"` (None when absent).
    pub next_time: Option<String>,
}

impl Default for Task {
    fn default() -> Self {
        Task {
            name: String::new(),
            command: String::new(),
            enabled: false,
            status: TaskStatus::Waiting,
            next_time: None,
        }
    }
}

/// Scheduler status of a task, mirroring the webui overview scopes
/// 运行中 / 队列中 / 等待中 (module/webui/app.py).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskStatus {
    /// First past-due task (the one the scheduler is executing, if alive).
    Running,
    /// Remaining past-due tasks behind the running one.
    Queued,
    /// `NextRun` in the future (or unknown).
    Waiting,
}

/// Pure status classification against a local clock string.
///
/// Both `next_time` and `now_str` use the zero-padded `"YYYY-MM-DD HH:MM:SS"`
/// format, so a plain string compare equals a chronological compare (verified
/// against the real payload: `datetime.fromisoformat` round-trips to exactly
/// this string — module/config/utils.py). Equal means "due now" → Running.
///
/// NOTE: the Running-vs-Queued split is a LIST-LEVEL concern (the first
/// past-due task is Running, later ones are Queued — module/webui/app.py
/// `pending_task[:1]` vs `pending_task[1:]`), handled in [`fetch_tasks`], not
/// here: [`classify`] returns Running for ANY past-due task.
pub fn classify(next_time: Option<&str>, now_str: &str) -> TaskStatus {
    match next_time {
        Some(t) if t <= now_str => TaskStatus::Running,
        _ => TaskStatus::Waiting,
    }
}

/// Parse the i18n JSON used for display names (e.g. zh-CN.json).
///
/// Tolerant: empty content → empty object (a missing/corrupt i18n file must
/// never take the task menu down — names fall back to command keys).
/// Garbage content that fails to parse is an `Err` for direct callers;
/// [`fetch_tasks`] swallows it.
pub fn parse_i18n(content: &str) -> Result<serde_json::Value> {
    if content.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(content).map_err(|e| anyhow!("invalid i18n json: {e}"))
}

/// Parse the merged scheduler file `config/alas.json` into the enabled task
/// list, mirroring `module/config/config.py:get_next_task`.
///
/// For every top-level key carrying a `Scheduler` object:
/// - enabled = `Scheduler.Enable == true`; anything else (false, missing,
///   non-bool) means disabled and the task is SKIPPED — the webui only lists
///   enabled tasks (`if not func.enable: continue`), and the discovery
///   evidence confirmed the real file has no per-task yaml fallback.
/// - next_time = `Scheduler.NextRun` as an optional string.
/// - name = i18n `Task.<command>.name`, falling back to the command key.
/// - status = [`classify`] against `now_str` (only meaningful for enabled
///   tasks; the Running/Queued split happens in [`fetch_tasks`]).
///
/// Malformed JSON → `Err`; valid JSON with no enabled tasks → `Ok(vec![])`.
pub fn parse_tasks_alas_json(
    content: &str,
    i18n: &serde_json::Value,
    now_str: &str,
) -> Result<Vec<Task>> {
    let root: serde_json::Value =
        serde_json::from_str(content).map_err(|e| anyhow!("invalid alas.json: {e}"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| anyhow!("alas.json root must be a JSON object"))?;

    let mut tasks = Vec::new();
    for (command, task_value) in obj {
        let Some(scheduler) = task_value.get("Scheduler") else {
            continue; // no Scheduler group → not a schedulable task (e.g. Alas/General)
        };
        if !matches!(scheduler.get("Enable").and_then(|v| v.as_bool()), Some(true)) {
            continue; // disabled (or malformed Enable) → skipped, like the webui
        }
        let next_time = scheduler
            .get("NextRun")
            .and_then(|v| v.as_str())
            .map(String::from);
        let name = i18n
            .get("Task")
            .and_then(|t| t.get(command))
            .and_then(|entry| entry.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| command.clone());
        tasks.push(Task {
            name,
            command: command.clone(),
            enabled: true,
            status: classify(next_time.as_deref(), now_str),
            next_time,
        });
    }
    Ok(tasks)
}

/// Fetch the task list straight from the ALAS payload files.
///
/// - `<alas_dir>/config/alas.json` — merged scheduler data (missing → empty
///   list, NOT an error: a payload without scheduler data is a valid "no
///   tasks" state; malformed content IS an error).
/// - `<alas_dir>/module/config/i18n/{language}.json` — display names; missing
///   or unparseable → empty i18n (names fall back to command keys), never
///   fatal.
///
/// Post-processing mirrors the webui overview ordering (module/webui/app.py
/// `alas_update_overview_task`): tasks sorted by `NextRun` ascending (None
/// last); after sorting, the FIRST task with status Running (earliest
/// past-due) stays Running, every later past-due task becomes Queued, future
/// tasks are Waiting.
pub fn fetch_tasks(alas_dir: &Path, now_str: &str, language: &str) -> Result<Vec<Task>> {
    let alas_path = alas_dir.join("config").join("alas.json");
    let content = match std::fs::read_to_string(&alas_path) {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()), // no scheduler file → no tasks, not an error
    };

    let i18n = load_i18n(alas_dir, language);
    let mut tasks = parse_tasks_alas_json(&content, &i18n, now_str)?;

    tasks.sort_by(|a, b| match (&a.next_time, &b.next_time) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });

    // After sorting, the earliest past-due task is the running one; any
    // further past-due tasks are queued behind it.
    if tasks.first().is_some_and(|t| t.status == TaskStatus::Running) {
        for t in tasks.iter_mut().skip(1) {
            if t.status == TaskStatus::Running {
                t.status = TaskStatus::Queued;
            }
        }
    }

    Ok(tasks)
}

/// Read + parse the i18n file for `language`; any failure → empty object
/// (display names are cosmetic, never fatal).
fn load_i18n(alas_dir: &Path, language: &str) -> serde_json::Value {
    let path = alas_dir
        .join("module")
        .join("config")
        .join("i18n")
        .join(format!("{language}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    parse_i18n(&content).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
}

/// Local clock in the scheduler's string format, `"YYYY-MM-DD HH:MM:SS"`.
///
/// Produced by spawning `date +"%F %T"` (macOS provides it; this module is
/// cfg-gated to macOS in main.rs, so win/linux are unaffected). No chrono
/// dependency is worth adding for one timestamp. On spawn failure, falls back
/// to the lexicographic maximum "9999-12-31 23:59:59" — every task classifies
/// as Waiting — so the menu still renders rather than erroring.
pub fn now_str() -> String {
    match std::process::Command::new("date").args(["+%F %T"]).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => {
            warn!("`date +\"%F %T\"` failed; classifying all tasks as Waiting");
            "9999-12-31 23:59:59".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fixtures (real values, quoted from the live payload 2026-08-10;
    //      full Scheduler blocks verbatim from config/alas.json) --------------

    /// Real i18n entries (module/config/i18n/zh-CN.json, `Task.<cmd>.name`).
    const I18N_FIXTURE: &str = r#"{
        "Task": {
            "Main":     { "name": "主线图" },
            "Exercise": { "name": "演习" },
            "Tactical": { "name": "战术学院" },
            "Guild":    { "name": "大舰队" }
        }
    }"#;

    /// Real scheduler blocks: Main + GemsFarming disabled, General without a
    /// Scheduler group, Exercise (future), Tactical + Guild (past-due).
    const ALAS_JSON_FIXTURE: &str = r#"{
        "General":    { "Retirement": {}, "Storage": {} },
        "Main":       { "Scheduler": { "Enable": false, "NextRun": "2026-06-01 23:18:00", "Command": "Main", "SuccessInterval": 0, "FailureInterval": 120 }, "Campaign": {} },
        "Exercise":   { "Scheduler": { "Enable": true, "NextRun": "2026-08-11 00:00:00", "Command": "Exercise", "SuccessInterval": 30, "FailureInterval": 30 } },
        "Tactical":   { "Scheduler": { "Enable": true, "NextRun": "2026-08-10 20:14:24", "Command": "Tactical", "SuccessInterval": "30-60", "FailureInterval": "120-240" } },
        "Guild":      { "Scheduler": { "Enable": true, "NextRun": "2026-08-10 21:00:00", "Command": "Guild", "SuccessInterval": 30, "FailureInterval": 30 } },
        "GemsFarming": { "Scheduler": { "Enable": false, "NextRun": "2026-08-09 08:33:32", "Command": "GemsFarming", "SuccessInterval": 0, "FailureInterval": 120 } }
    }"#;

    /// Past the Tactical/Guild times, before Exercise.
    const NOW_FIXTURE: &str = "2026-08-10 22:00:00";

    // ---- test helpers --------------------------------------------------------

    /// Temporary directory guard; removed on drop (also on test panic).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "alas-tasks-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- parse_i18n ------------------------------------------------------------

    #[test]
    fn parse_i18n_valid_object() {
        let v = parse_i18n(I18N_FIXTURE).unwrap();
        assert_eq!(v["Task"]["Main"]["name"], "主线图");
    }

    #[test]
    fn parse_i18n_empty_is_empty_object() {
        assert_eq!(parse_i18n("").unwrap(), serde_json::Value::Object(Default::default()));
        assert_eq!(parse_i18n("  \n ").unwrap(), serde_json::Value::Object(Default::default()));
    }

    #[test]
    fn parse_i18n_garbage_is_err() {
        assert!(parse_i18n("{not json").is_err());
    }

    // ---- parse_tasks_alas_json --------------------------------------------------

    #[test]
    fn parse_tasks_alas_json_happy_path() {
        let i18n = parse_i18n(I18N_FIXTURE).unwrap();
        let tasks = parse_tasks_alas_json(ALAS_JSON_FIXTURE, &i18n, NOW_FIXTURE).unwrap();
        // General (no Scheduler), Main + GemsFarming (disabled) are skipped.
        assert_eq!(tasks.len(), 3);

        let exercise = tasks.iter().find(|t| t.command == "Exercise").unwrap();
        assert_eq!(
            exercise,
            &Task {
                name: "演习".into(),
                command: "Exercise".into(),
                enabled: true,
                status: TaskStatus::Waiting, // 2026-08-11 00:00:00 > now
                next_time: Some("2026-08-11 00:00:00".into()),
            }
        );

        let tactical = tasks.iter().find(|t| t.command == "Tactical").unwrap();
        assert_eq!(tactical.name, "战术学院"); // real i18n name
        assert_eq!(tactical.next_time.as_deref(), Some("2026-08-10 20:14:24"));
        // classify-level: past-due => Running (Queued split happens in fetch_tasks).
        assert_eq!(tactical.status, TaskStatus::Running);

        let guild = tasks.iter().find(|t| t.command == "Guild").unwrap();
        assert_eq!(guild.name, "大舰队");
        assert_eq!(guild.next_time.as_deref(), Some("2026-08-10 21:00:00"));
        assert_eq!(guild.status, TaskStatus::Running);

        // Disabled / non-scheduler entries never appear.
        assert!(tasks.iter().all(|t| t.enabled));
        assert!(!tasks.iter().any(|t| t.command == "Main" || t.command == "GemsFarming"));
    }

    #[test]
    fn parse_tasks_alas_json_fallback_name_without_i18n() {
        let empty_i18n = serde_json::Value::Object(Default::default());
        let tasks = parse_tasks_alas_json(ALAS_JSON_FIXTURE, &empty_i18n, NOW_FIXTURE).unwrap();
        assert_eq!(tasks.len(), 3);
        // No i18n → display name falls back to the command key.
        assert!(tasks.iter().all(|t| t.name == t.command));
        // Missing single entry also falls back while others resolve.
        let partial = serde_json::json!({ "Task": { "Exercise": { "name": "演习" } } });
        let tasks = parse_tasks_alas_json(ALAS_JSON_FIXTURE, &partial, NOW_FIXTURE).unwrap();
        assert_eq!(tasks.iter().find(|t| t.command == "Exercise").unwrap().name, "演习");
        assert_eq!(tasks.iter().find(|t| t.command == "Tactical").unwrap().name, "Tactical");
    }

    #[test]
    fn parse_tasks_alas_json_malformed_is_err() {
        let i18n = parse_i18n(I18N_FIXTURE).unwrap();
        assert!(parse_tasks_alas_json("{not json", &i18n, NOW_FIXTURE).is_err());
        assert!(parse_tasks_alas_json("", &i18n, NOW_FIXTURE).is_err());
        assert!(parse_tasks_alas_json("[1,2,3]", &i18n, NOW_FIXTURE).is_err());
    }

    #[test]
    fn parse_tasks_alas_json_empty_is_empty_list() {
        let i18n = parse_i18n(I18N_FIXTURE).unwrap();
        assert_eq!(parse_tasks_alas_json("{}", &i18n, NOW_FIXTURE).unwrap(), vec![]);
        // Object with entries but no enabled Scheduler tasks → empty list.
        let all_disabled = r#"{ "Main": { "Scheduler": { "Enable": false, "NextRun": "2026-06-01 23:18:00", "Command": "Main" } } }"#;
        assert_eq!(parse_tasks_alas_json(all_disabled, &i18n, NOW_FIXTURE).unwrap(), vec![]);
    }

    // ---- classify ----------------------------------------------------------------

    #[test]
    fn classify_matrix() {
        // past vs now → Running
        assert_eq!(classify(Some("2026-08-10 20:14:24"), "2026-08-10 22:00:00"), TaskStatus::Running);
        // future → Waiting
        assert_eq!(classify(Some("2026-08-11 00:00:00"), "2026-08-10 22:00:00"), TaskStatus::Waiting);
        // equal boundary → Running (due now)
        assert_eq!(classify(Some("2026-08-10 22:00:00"), "2026-08-10 22:00:00"), TaskStatus::Running);
        // None → Waiting
        assert_eq!(classify(None, "2026-08-10 22:00:00"), TaskStatus::Waiting);
        // day boundary across midnight
        assert_eq!(classify(Some("2026-08-09 23:59:59"), "2026-08-10 00:00:00"), TaskStatus::Running);
    }

    // ---- fetch_tasks -----------------------------------------------------------------

    #[test]
    fn fetch_tasks_sorted_with_running_queued_waiting() {
        let tmp = TempDir::new("sorted");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("alas.json"), ALAS_JSON_FIXTURE).unwrap();
        let i18n_dir = tmp.path().join("module/config/i18n");
        std::fs::create_dir_all(&i18n_dir).unwrap();
        std::fs::write(i18n_dir.join("zh-CN.json"), I18N_FIXTURE).unwrap();

        let tasks = fetch_tasks(tmp.path(), NOW_FIXTURE, "zh-CN").unwrap();
        // Sorted by next_time ascending: Tactical (20:14) < Guild (21:00) < Exercise (next day).
        assert_eq!(
            tasks.iter().map(|t| t.command.as_str()).collect::<Vec<_>>(),
            vec!["Tactical", "Guild", "Exercise"]
        );
        // First past-due = Running, later past-due = Queued, future = Waiting.
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(tasks[0].name, "战术学院");
        assert_eq!(tasks[1].status, TaskStatus::Queued);
        assert_eq!(tasks[2].status, TaskStatus::Waiting);
        assert_eq!(tasks[2].name, "演习");
    }

    #[test]
    fn fetch_tasks_missing_alas_json_is_empty() {
        let tmp = TempDir::new("missing-alas");
        let tasks = fetch_tasks(tmp.path(), NOW_FIXTURE, "zh-CN").unwrap();
        assert_eq!(tasks, vec![]);
    }

    #[test]
    fn fetch_tasks_missing_i18n_falls_back_to_command_keys() {
        let tmp = TempDir::new("missing-i18n");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("alas.json"), ALAS_JSON_FIXTURE).unwrap();
        // No module/config/i18n dir at all.
        let tasks = fetch_tasks(tmp.path(), NOW_FIXTURE, "zh-CN").unwrap();
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| t.name == t.command));
        assert_eq!(tasks[0].status, TaskStatus::Running); // post-processing intact
        assert_eq!(tasks[1].status, TaskStatus::Queued);
        assert_eq!(tasks[2].status, TaskStatus::Waiting);
    }

    #[test]
    fn fetch_tasks_malformed_alas_json_is_err() {
        let tmp = TempDir::new("malformed-alas");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("alas.json"), "{not json").unwrap();
        assert!(fetch_tasks(tmp.path(), NOW_FIXTURE, "zh-CN").is_err());
    }

    #[test]
    fn fetch_tasks_nothing_past_due_all_waiting() {
        let tmp = TempDir::new("all-waiting");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("alas.json"), ALAS_JSON_FIXTURE).unwrap();
        // Before every task's NextRun → nothing Running, nothing Queued.
        let tasks = fetch_tasks(tmp.path(), "2026-08-09 00:00:00", "zh-CN").unwrap();
        assert!(tasks.iter().all(|t| t.status == TaskStatus::Waiting));
    }

    // ---- real payload (manual-QA channel) --------------------------------------------

    /// Reads the REAL installed ALAS payload when present (read-only) and
    /// prints the parsed task list — the real-surface proof that the parser
    /// handles the live file. Skipped (with a note) when the payload is not
    /// installed on this machine.
    #[test]
    fn fetch_tasks_real_payload_if_present() {
        let payload = Path::new("/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript");
        if !payload.join("config").join("alas.json").exists() {
            eprintln!("real ALAS payload not present; skipping real-surface check");
            return;
        }
        let tasks = fetch_tasks(payload, &now_str(), "zh-CN").unwrap();
        assert!(!tasks.is_empty(), "real payload must yield enabled tasks");
        assert!(tasks.iter().any(|t| !t.name.is_empty()));
        println!("real payload: {} enabled tasks\n{tasks:#?}", tasks.len());
    }
}
