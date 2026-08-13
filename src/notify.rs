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

/// Episode latch for the scheduler-death notification (round-1 F-MEDIUM fix):
/// once either channel has fired for one death episode, both channels stay
/// suppressed until the scheduler is seen alive again (`now_alive ==
/// Some(true)`) or the backend stops running (crash-mark or a user stop) — a
/// later real episode then fires anew. Returns whether a death notification
/// is allowed THIS cycle; the cycle that observes revival/restart resets the
/// latch but still returns false (the observing cycle never fires).
fn death_notify_allowed(now_alive: Option<bool>, backend_running: bool, fired: &mut bool) -> bool {
    if *fired {
        if now_alive == Some(true) || !backend_running {
            *fired = false;
        }
        return false;
    }
    true
}

/// Pure composition of the two scheduler-death channels with the gates and
/// the episode latch (round-1 F-LOW: the poll_once wiring is embedded, so the
/// decision lives here as a tested pure function).
///
/// `gates_ok` = backend Running + scheduler intent == None (the user Start
/// window must never count as death); `backend_running` gates ONLY the latch
/// reset — an intent change must not re-arm a latched episode (a Stop click
/// on an already-dead scheduler would otherwise re-fire one cycle later).
/// `state_died` is injected: the `api_instances` read is I/O, done at the
/// call site, which also owns the state-baseline update.
pub fn death_notify_decision(
    prev_alive: Option<bool>,
    now_alive: Option<bool>,
    gates_ok: bool,
    backend_running: bool,
    state_died: bool,
    dead_streak: &mut u8,
    death_notified: &mut bool,
) -> bool {
    let allowed = death_notify_allowed(now_alive, backend_running, death_notified);
    let liveness_died = allowed && gates_ok && liveness_death(prev_alive, now_alive, dead_streak);
    let died = liveness_died || (allowed && state_died);
    if died {
        *death_notified = true;
    }
    died
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

    // ---- death_notify_decision (round-1 F-MEDIUM latch + F-LOW composition) --

    #[test]
    fn death_notify_decision_scheduler_child_death_fires_via_liveness() {
        // Headline scenario (F-HIGH trace): only the scheduler CHILD dies —
        // the uvicorn web server (the port owner) stays alive, so the backend
        // stays Running, gates stay open, and the liveness streak completes.
        let mut streak = 0u8;
        let mut fired = false;
        // cycle 1: alive → dead, streak=1, no fire yet
        assert!(!death_notify_decision(
            Some(true), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert_eq!(streak, 1);
        assert!(!fired);
        // cycle 2: still dead → streak=2 → fires exactly once
        assert!(death_notify_decision(
            Some(false), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(fired);
        // cycle 3: still dead → latched, no duplicate
        assert!(!death_notify_decision(
            Some(false), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(fired, "latch stays set while the episode continues");
    }

    #[test]
    fn death_notify_decision_state_channel_fire_suppresses_liveness_duplicate() {
        // F-MEDIUM repro: state 1→3 fires on cycle 1 (API reachable on the
        // still-live uvicorn port), liveness streak completes on cycle 2 —
        // one notification for the episode, not two.
        let mut streak = 0u8;
        let mut fired = false;
        assert!(death_notify_decision(
            Some(true), Some(false), true, true, true, &mut streak, &mut fired
        ));
        assert!(fired);
        assert!(!death_notify_decision(
            Some(false), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(fired);
    }

    #[test]
    fn death_notify_decision_whole_backend_death_never_fires() {
        // Whole-backend death: port dies → the 2-strike crash-mark flips
        // status to Stopped on the cycle the liveness streak would complete →
        // gates closed → no fire (that path is the tray's 异常停止 status
        // line, not a notification).
        let mut streak = 0u8;
        let mut fired = false;
        // cycle 1: alive → dead, streak=1 (crash-mark strike 1 only)
        assert!(!death_notify_decision(
            Some(true), Some(false), true, true, false, &mut streak, &mut fired
        ));
        // cycle 2: crash-marked → backend not running, scan unknown
        assert!(!death_notify_decision(
            Some(false), None, false, false, false, &mut streak, &mut fired
        ));
        assert!(!fired);
    }

    #[test]
    fn death_notify_decision_revival_resets_latch_and_reenables() {
        let mut streak = 0u8;
        let mut fired = false;
        assert!(death_notify_decision(
            Some(true), Some(false), true, true, true, &mut streak, &mut fired
        ));
        // revival: no fire this cycle, latch reset (liveness_death is skipped
        // while latched, so the streak reset lands on the NEXT cycle)
        assert!(!death_notify_decision(
            Some(false), Some(true), true, true, false, &mut streak, &mut fired
        ));
        assert_eq!(streak, 1, "streak reset deferred while the latch suppresses");
        assert!(!death_notify_decision(
            Some(true), Some(true), true, true, false, &mut streak, &mut fired
        ));
        assert_eq!(streak, 0, "alive scan resets the streak");
        // a new death episode fires again
        assert!(!death_notify_decision(
            Some(true), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(death_notify_decision(
            Some(false), Some(false), true, true, false, &mut streak, &mut fired
        ));
    }

    #[test]
    fn death_notify_decision_backend_restart_resets_latch() {
        let mut streak = 0u8;
        let mut fired = false;
        assert!(death_notify_decision(
            Some(true), Some(false), true, true, true, &mut streak, &mut fired
        ));
        // backend stops (crash-mark / user stop) → latch reset
        assert!(!death_notify_decision(
            Some(false), None, false, false, false, &mut streak, &mut fired
        ));
        // fresh backend, scheduler never scanned alive → no fire (no baseline)
        assert!(!death_notify_decision(
            None, Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(!fired);
    }

    #[test]
    fn death_notify_decision_intent_change_does_not_reatm_latched_episode() {
        // A user Stop click (intent != None → gates_ok false) while the
        // episode is latched must NOT re-arm the notification one cycle later.
        let mut streak = 0u8;
        let mut fired = false;
        assert!(death_notify_decision(
            Some(true), Some(false), true, true, true, &mut streak, &mut fired
        ));
        // Stop click: gates close, backend still running → still latched
        assert!(!death_notify_decision(
            Some(false), Some(false), false, true, false, &mut streak, &mut fired
        ));
        assert!(fired, "latch survives the intent change");
        // gates back open (intent cleared) → still suppressed
        assert!(!death_notify_decision(
            Some(false), Some(false), true, true, false, &mut streak, &mut fired
        ));
        assert!(fired);
    }
}
