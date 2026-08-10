use std::{
    net::TcpStream,
    process::{Command, ExitStatus},
    sync::Mutex,
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use command_group::{CommandGroup, GroupChild};
use tracing::warn;

use crate::window_util::CreateNoWindow as _;

/// Lifecycle status of the ALAS backend (the spawned gui.py process).
///
/// Three states: `Initializing` (the Ready thread or a menu Start is still
/// bringing the backend up — the toggle item is disabled), `Running` (backend
/// is up and reachable on the webui port), `Stopped` (no backend — or a
/// failed start; the user may retry).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendStatus {
    Initializing,
    Running,
    Stopped,
}

/// Whole-backend lifecycle state. Internal to [`BackendLifecycle`] (callers
/// never see or lock it); kept as-is from the pre-deep-module layout.
/// `status` drives the tray menu labels/enabled state; `backend` holds the
/// live process handle; `start_failed` distinguishes "stopped" from "last
/// start attempt failed" so the tray can show the start-failed label instead
/// of silently reverting to Start.
pub struct BackendState {
    pub status: BackendStatus,
    pub backend: Option<ManagedBackend>,
    pub start_failed: bool,
}

impl Default for BackendState {
    fn default() -> Self {
        Self {
            status: BackendStatus::Stopped,
            backend: None,
            start_failed: false,
        }
    }
}

/// Lock-free copy of the parts of [`BackendState`] the tray renders, taken
/// via [`BackendLifecycle::snapshot`]. No process handle, no lock held after
/// the call returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackendStateSnapshot {
    pub status: BackendStatus,
    pub start_failed: bool,
}

pub struct ManagedBackend {
    child: Option<GroupChild>,
}

impl ManagedBackend {
    pub fn new(port: u16) -> Result<Self> {
        std::env::set_var("ALAS_LAUNCHER_PID", format!("{}", std::process::id()));
        let child = Command::new("python")
            .args(["gui.py", "--host", "127.0.0.1", "--port", &port.to_string()])
            .group()
            .create_no_window()
            .spawn()?;
        let mut res = Self { child: Some(child) };

        let address = format!("127.0.0.1:{}", port).parse().unwrap();
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < Duration::from_secs(60) {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                // Metis MINOR-6: the port may be held by a stale/unrelated
                // process. Verify the spawned child actually survived before
                // declaring the backend ready.
                if let Some(child) = res.child.as_mut() {
                    if child
                        .try_wait()
                        .map_err(|e| anyhow!("Failed to check gui.py status: {e}"))?
                        .is_some()
                    {
                        return Err(anyhow!("gui.py exited before becoming ready"));
                    }
                }
                return Ok(res);
            }
            sleep(Duration::from_millis(100));
        }
        Err(anyhow!("Timeout waiting for port {} to be ready", port))
    }

    pub fn terminate(&mut self) -> Result<ExitStatus> {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                use command_group::{Signal, UnixChildExt};
                let _ = child.signal(Signal::SIGTERM);
                let start_time = std::time::Instant::now();
                while start_time.elapsed() < Duration::from_millis(500) {
                    if let Ok(Some(exit_status)) = child.try_wait() {
                        return Ok(exit_status);
                    }
                    sleep(Duration::from_millis(100));
                }
                warn!("gui.py didn't exit, killing it...");
            }
            child.kill()?;
            Ok(child.wait()?)
        } else {
            Ok(ExitStatus::default())
        }
    }

    /// Test seam: wrap an already-spawned process group so lifecycle tests can
    /// inject a fake backend without ever touching `new()` (and its 60s port
    /// readiness wait).
    #[cfg(test)]
    pub(crate) fn from_child(child: GroupChild) -> Self {
        Self { child: Some(child) }
    }

    /// Pid of the spawned gui.py wrapper (the process-group leader), None
    /// while no child is held. Process-tree scans (scheduler detection)
    /// start from this pid.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }
}

impl Drop for ManagedBackend {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.kill() {
                Ok(_) => {}
                Err(e) => warn!("Failed to kill gui.py process: {:?}", e),
            }
        }
        // Kill potential leaked processes
        let sys = sysinfo::System::new_all();
        for (pid, process) in sys.processes() {
            for var in process.environ() {
                if pid.as_u32() != std::process::id()
                    && var.to_str().unwrap_or_default()
                        == format!("ALAS_LAUNCHER_PID={}", std::process::id())
                {
                    process.kill();
                }
            }
        }
    }
}

/// Deep lifecycle module for the ALAS backend: all the ordering behavior (the
/// terminate-before-spawn contract, port readiness, start_failed, Drop
/// residue cleanup, process-group termination) lives here behind a small
/// interface — `status` / `snapshot` / `begin_start` / `start` / `stop` /
/// `mark_stopped`. The `Mutex` is INTERNAL: callers never lock it, so lock
/// discipline (brief reads, no lock across the long spawn) cannot leak out.
pub struct BackendLifecycle {
    state: Mutex<BackendState>,
    // Internal seam so `start()` is testable without spawning python: tests
    // inject a fake spawner; production wraps ManagedBackend::new.
    spawner: Box<dyn Fn(u16) -> Result<ManagedBackend> + Send + Sync>,
}

impl Default for BackendLifecycle {
    fn default() -> Self {
        Self::new_with_spawner(ManagedBackend::new)
    }
}

impl BackendLifecycle {
    /// Build a lifecycle whose spawn step runs `spawner` instead of
    /// `ManagedBackend::new` — the test seam.
    ///
    /// Note: `Sync` is required (not just `Send`) so `Arc<BackendLifecycle>`
    /// can be shared across the Ready thread and the tray poll thread.
    pub fn new_with_spawner<F: Fn(u16) -> Result<ManagedBackend> + Send + Sync + 'static>(
        spawner: F,
    ) -> Self {
        Self {
            state: Mutex::new(BackendState::default()),
            spawner: Box::new(spawner),
        }
    }

    /// Current lifecycle status (brief lock, no handle out).
    pub fn status(&self) -> BackendStatus {
        self.state.lock().unwrap().status
    }

    /// Copy of the render-relevant state (status + start_failed).
    pub fn snapshot(&self) -> BackendStateSnapshot {
        let state = self.state.lock().unwrap();
        BackendStateSnapshot {
            status: state.status,
            start_failed: state.start_failed,
        }
    }

    /// Mark the backend as initializing (and clear any previous start-failed
    /// flag) BEFORE the possibly-long spawn. Callers invoke this first so the
    /// menu shows "initializing…" and a concurrent toggle is a no-op
    /// (BLOCKER-3: never two backends).
    pub fn begin_start(&self) {
        let mut state = self.state.lock().unwrap();
        state.status = BackendStatus::Initializing;
        state.start_failed = false;
    }

    /// Start the backend on `port`.
    ///
    /// ORDERING CONTRACT (Metis BLOCKER-2): the old backend MUST be fully
    /// terminated AND dropped BEFORE a new gui.py spawns — ManagedBackend's
    /// Drop scans every process for ALAS_LAUNCHER_PID and would kill a
    /// freshly spawned child. The take/terminate/drop sequence therefore runs
    /// entirely OUTSIDE the lock and before the spawn.
    ///
    /// No lock is held across the spawn either: the port-readiness wait inside
    /// `ManagedBackend::new` can take up to 60s, and status readers (the tray
    /// poll thread reads the status every 10s) must never block on it.
    pub fn start(&self, port: u16) -> Result<()> {
        let old = {
            let mut state = self.state.lock().unwrap();
            state.status = BackendStatus::Initializing;
            state.start_failed = false;
            state.backend.take()
        };
        // Outside the lock: fully terminate + drop the old backend before any
        // new process can exist.
        terminate_old(old);
        match (self.spawner)(port) {
            Ok(backend) => {
                let mut state = self.state.lock().unwrap();
                state.backend = Some(backend);
                state.status = BackendStatus::Running;
                Ok(())
            }
            Err(e) => {
                // Consistent start-failure marking for BOTH callers (Ready
                // thread and tray toggle) — previously the Ready thread left
                // start_failed unset.
                let mut state = self.state.lock().unwrap();
                state.status = BackendStatus::Stopped;
                state.start_failed = true;
                Err(e)
            }
        }
    }

    /// Stop the backend (if any): status -> Stopped, start_failed cleared,
    /// then the process is terminated and dropped OUTSIDE the lock. Always
    /// ends Stopped; never panics on a missing backend.
    pub fn stop(&self) -> BackendStatus {
        let old = {
            let mut state = self.state.lock().unwrap();
            state.status = BackendStatus::Stopped;
            state.start_failed = false;
            state.backend.take()
        };
        terminate_old(old);
        BackendStatus::Stopped
    }

    /// Plain-Stopped transition for the setup-failure path: a setup error is
    /// not a backend-start failure, so `start_failed` is left untouched.
    pub fn mark_stopped(&self) {
        let mut state = self.state.lock().unwrap();
        state.status = BackendStatus::Stopped;
    }

    /// Pid of the live backend process, if any — the root of the process-tree
    /// scheduler probe. Brief lock, no handle out.
    pub fn backend_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .unwrap()
            .backend
            .as_ref()
            .and_then(|b| b.pid())
    }
}

/// ORDERING CONTRACT, shared by `start()` and `stop()`: the old backend is
/// fully terminated (SIGTERM -> kill) and then dropped — the Drop impl runs
/// the ALAS_LAUNCHER_PID kill-all scan — while no new process exists. Called
/// with the lifecycle lock already released.
fn terminate_old(old: Option<ManagedBackend>) {
    if let Some(mut old) = old {
        let _ = old.terminate();
        // Drop completes the residue scan here.
        drop(old);
    }
}

/// The webui root URL for `port` (owns the port concept; plain String, no
/// tauri dependency).
pub fn webui_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/", port)
}

/// Decision a Start/Stop toggle makes from the current snapshot.
///
/// Lives here (not in the menu model) because it is a backend-lifecycle
/// decision, not menu rendering: Initializing is a no-op (the tray item is
/// disabled anyway; this also makes a second click during a 60s start window
/// a no-op — BLOCKER-3: never two backends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleAction {
    NoOp,
    Stop,
    Start,
}

pub(crate) fn toggle_decision(snapshot: &BackendStateSnapshot) -> ToggleAction {
    match snapshot.status {
        BackendStatus::Initializing => ToggleAction::NoOp,
        BackendStatus::Running => ToggleAction::Stop,
        BackendStatus::Stopped => ToggleAction::Start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A harmless real process group the fake spawner hands back. `sleep 60`
    /// dies instantly on SIGTERM; the lifecycle's Drop kills it if a test
    /// leaves it running.
    fn sleep_child() -> GroupChild {
        Command::new("sleep").arg("60").group_spawn().unwrap()
    }

    fn ok_spawner() -> impl Fn(u16) -> Result<ManagedBackend> {
        |_| Ok(ManagedBackend::from_child(sleep_child()))
    }

    /// Deterministic liveness check (ps exits non-zero when the pid is gone;
    /// on ps failure assume alive so the test never false-passes).
    fn process_is_alive(pid: u32) -> bool {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(true)
    }

    /// A spawner that records every spawned child's pid (Arc by value so the
    /// returned closure needs no borrow and is 'static).
    fn recording_spawner(pids: Arc<Mutex<Vec<u32>>>) -> impl Fn(u16) -> Result<ManagedBackend> {
        move |_| {
            let child = sleep_child();
            pids.lock().unwrap().push(child.id());
            Ok(ManagedBackend::from_child(child))
        }
    }

    #[test]
    fn start_success_sets_running() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.start(22267).unwrap();
        assert_eq!(lc.status(), BackendStatus::Running);
        assert!(!lc.snapshot().start_failed);
    }

    #[test]
    fn start_failure_sets_stopped_start_failed() {
        let lc = BackendLifecycle::new_with_spawner(|_| Err(anyhow!("boom")));
        let err = lc.start(22267).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(lc.status(), BackendStatus::Stopped);
        assert!(lc.snapshot().start_failed);
    }

    #[test]
    fn begin_start_sets_initializing_and_clears_failed() {
        let lc = BackendLifecycle::new_with_spawner(|_| Err(anyhow!("boom")));
        let _ = lc.start(22267); // leaves Stopped + start_failed
        lc.begin_start();
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Initializing);
        assert!(!s.start_failed);
    }

    #[test]
    fn stop_with_no_backend_is_stopped_without_panic() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        assert_eq!(lc.stop(), BackendStatus::Stopped);
        assert!(!lc.snapshot().start_failed);
    }

    #[test]
    fn stop_terminates_the_backend() {
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let lc = BackendLifecycle::new_with_spawner(recording_spawner(pids.clone()));
        lc.start(22267).unwrap();
        let pid = pids.lock().unwrap()[0];
        assert!(process_is_alive(pid), "child alive right after start");
        lc.stop();
        assert_eq!(lc.status(), BackendStatus::Stopped);
        assert!(!process_is_alive(pid), "stop must terminate the child");
    }

    #[test]
    fn mark_stopped_clears_initializing_without_start_failed() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.begin_start();
        lc.mark_stopped();
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Stopped);
        assert!(!s.start_failed);
    }

    #[test]
    fn backend_pid_tracks_the_live_child() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        assert_eq!(lc.backend_pid(), None, "no backend before start");
        lc.start(22267).unwrap();
        let pid = lc.backend_pid().expect("backend pid after start");
        assert!(process_is_alive(pid), "recorded pid is the live child");
        lc.stop();
        assert_eq!(lc.backend_pid(), None, "no handle after stop");
    }

    #[test]
    fn backend_pid_uses_the_recording_spawner_child() {
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let lc = BackendLifecycle::new_with_spawner(recording_spawner(pids.clone()));
        lc.start(22267).unwrap();
        let spawned = pids.lock().unwrap()[0];
        assert_eq!(lc.backend_pid(), Some(spawned));
    }

    /// ORDERING CONTRACT (Metis BLOCKER-2): a second start() must fully
    /// terminate the first child BEFORE the new spawn — the old backend's
    /// kill-all Drop scan must never see the fresh process.
    #[test]
    fn ordering_contract_second_start_kills_first_child_before_spawn() {
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let lc = BackendLifecycle::new_with_spawner(recording_spawner(pids.clone()));
        lc.start(22267).unwrap();
        lc.start(22267).unwrap();
        let pids = pids.lock().unwrap().clone();
        assert_eq!(pids.len(), 2, "two spawns happened");
        assert_ne!(pids[0], pids[1], "second spawn is a distinct process");
        assert!(
            !process_is_alive(pids[0]),
            "first child must be terminated before the second spawn"
        );
        assert!(process_is_alive(pids[1]), "second child stays up");
    }

    #[test]
    fn webui_url_format() {
        assert_eq!(webui_url(22267), "http://127.0.0.1:22267/");
    }

    #[test]
    fn toggle_decision_matrix() {
        let stopped = BackendStateSnapshot {
            status: BackendStatus::Stopped,
            start_failed: false,
        };
        assert_eq!(toggle_decision(&stopped), ToggleAction::Start);
        let running = BackendStateSnapshot {
            status: BackendStatus::Running,
            start_failed: false,
        };
        assert_eq!(toggle_decision(&running), ToggleAction::Stop);
        let initializing = BackendStateSnapshot {
            status: BackendStatus::Initializing,
            start_failed: false,
        };
        assert_eq!(toggle_decision(&initializing), ToggleAction::NoOp);
    }
}
