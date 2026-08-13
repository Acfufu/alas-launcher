//! Notification event detection (pure, no tauri types).
//! Scheduler death (Round-2 redesign): the PRIMARY signal is the process-tree
//! liveness flip — `ProcessManager.state` is unreliable for kills because
//! renderables accumulate across runs (a previous normal "Reason: Finish"
//! tail makes a killed scheduler read state 2, not 3). The flip is gated by
//! persistence (2 consecutive dead scans ride out update/restart blips).
//! `death_by_state` (1→3) remains as an ADDITIONAL authoritative signal.
//! Task completion = `Scheduler.NextRun` moved forward between polls.

use crate::alas_tasks::Task;
use crate::shell_settings::ShellSettings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyEvent {
    SchedulerDeath { name: String },
    TaskComplete { name: String, command: String, next_time: String },
}

pub fn diff_tasks(prev: &[Task], now: &[Task]) -> Vec<NotifyEvent> {
    let mut out = Vec::new();
    for p in prev {
        let Some(old_next) = p.next_time.as_deref() else { continue };
        let Some(cur) = now.iter().find(|t| t.command == p.command) else { continue };
        if let Some(new_next) = cur.next_time.as_deref() {
            if new_next > old_next {
                out.push(NotifyEvent::TaskComplete {
                    name: cur.name.clone(),
                    command: cur.command.clone(),
                    next_time: new_next.to_string(),
                });
            }
        }
    }
    out
}

/// Authoritative abnormal-death signal: running (1) → abnormal (3).
pub fn death_by_state(prev_state: Option<u8>, new_state: Option<u8>) -> bool {
    matches!((prev_state, new_state), (Some(1), Some(3)))
}

/// Liveness-flip death with 2-scan persistence, fire-once-per-episode
/// (Round-3 MUST-FIX：`>=` 无复位导致持续复火；`(_, Some(false))` 无基线也计）。
///
/// `dead_streak` is the poll-loop-owned counter. Counting starts ONLY from an
/// observed alive baseline (`prev == Some(true)`): the scheduler-off-at-launch
/// and degraded-mode (scheduler never started) states must never fire.
/// Fires exactly once, on the scan where the streak reaches `PERSIST_SCANS`;
/// `saturating_add` prevents u8 wrap on long-dead episodes. Any alive or
/// unknown scan resets.
pub fn liveness_death(prev: Option<bool>, now: Option<bool>, dead_streak: &mut u8) -> bool {
    const PERSIST_SCANS: u8 = 2;
    match (prev, now) {
        (Some(true), Some(false)) => *dead_streak = 1,
        (Some(false), Some(false)) if *dead_streak > 0 => {
            *dead_streak = dead_streak.saturating_add(1)
        }
        _ => *dead_streak = 0,
    }
    *dead_streak == PERSIST_SCANS
}

pub fn should_notify(event: &NotifyEvent, settings: &ShellSettings) -> bool {
    if !settings.notify_enabled {
        return false;
    }
    match event {
        NotifyEvent::SchedulerDeath { .. } => settings.notify_scheduler_death,
        NotifyEvent::TaskComplete { .. } => settings.notify_task_complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alas_tasks::{Task, TaskStatus};
    use crate::shell_settings::ShellSettings;

    fn task(name: &str, next: Option<&str>) -> Task {
        Task {
            name: name.into(),
            command: name.into(),
            status: TaskStatus::Waiting,
            next_time: next.map(String::from),
        }
    }

    #[test]
    fn next_run_forward_detects_task_complete() {
        let prev = vec![task("Daily", Some("2026-08-13 05:00:00"))];
        let now = vec![task("Daily", Some("2026-08-13 05:05:00"))];
        let events = diff_tasks(&prev, &now);
        assert_eq!(events.len(), 1);
        // Brief used `events[0].command`; enum-variant field access is not
        // legal Rust (E0609) — assert via pattern match instead.
        match &events[0] {
            NotifyEvent::TaskComplete { command, next_time, .. } => {
                assert_eq!(command, "Daily");
                assert_eq!(next_time, "2026-08-13 05:05:00");
            }
            other => panic!("expected TaskComplete, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_or_backwards_next_run_is_not_complete() {
        let prev = vec![task("Daily", Some("2026-08-13 05:00:00"))];
        assert!(diff_tasks(&prev, &prev).is_empty());
        let backwards = vec![task("Daily", Some("2026-08-13 04:00:00"))];
        assert!(diff_tasks(&prev, &backwards).is_empty());
    }

    #[test]
    fn state_3_is_death_state_1_2_4_are_not() {
        assert!(death_by_state(Some(1), Some(3)));
        assert!(!death_by_state(Some(1), Some(1)));
        assert!(!death_by_state(Some(1), Some(2))); // normal stop / renderables 残留
        assert!(!death_by_state(Some(1), Some(4))); // update
        assert!(!death_by_state(Some(3), Some(3))); // no transition
    }

    #[test]
    fn liveness_death_fires_once_on_persisted_flip() {
        let mut streak = 0u8;
        // 翻转第一拍：起计（streak=1），未达 2 → 不触发
        assert!(!liveness_death(Some(true), Some(false), &mut streak));
        assert_eq!(streak, 1);
        // 第二拍仍死 → 恰好触发一次
        assert!(liveness_death(Some(false), Some(false), &mut streak));
        // 第三拍仍死 → 不重复触发（Round-3：>= 复火 bug 回归测试）
        assert!(!liveness_death(Some(false), Some(false), &mut streak));
        assert!(!liveness_death(Some(false), Some(false), &mut streak));
        // 复活 → 归零
        assert!(!liveness_death(Some(false), Some(true), &mut streak));
        assert_eq!(streak, 0);
        // 复活后再死亡 → 重新完整一轮
        assert!(!liveness_death(Some(true), Some(false), &mut streak));
        assert!(liveness_death(Some(false), Some(false), &mut streak));
    }

    #[test]
    fn liveness_death_never_fires_without_alive_baseline() {
        // Round-3：调度器从未启动（auto-start off / 降级模式）→ 永不误报
        let mut streak = 0u8;
        assert!(!liveness_death(None, Some(false), &mut streak));
        assert!(!liveness_death(Some(false), Some(false), &mut streak));
        assert!(!liveness_death(Some(false), Some(false), &mut streak));
        assert_eq!(streak, 0);
        // 未知（后端重启窗口）同样不误报
        let mut s2 = 0u8;
        assert!(!liveness_death(Some(false), None, &mut s2));
        assert!(!liveness_death(None, None, &mut s2));
    }

    #[test]
    fn notification_default_matrix() {
        let s = ShellSettings::default();
        assert!(should_notify(&NotifyEvent::SchedulerDeath { name: "alas".into() }, &s));
        assert!(!should_notify(&NotifyEvent::TaskComplete { name: "日常".into(), command: "Daily".into(), next_time: "x".into() }, &s));
        let off = ShellSettings { notify_enabled: false, ..Default::default() };
        assert!(!should_notify(&NotifyEvent::SchedulerDeath { name: "alas".into() }, &off));
    }
}
