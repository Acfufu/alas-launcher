use std::{
    io,
    net::TcpStream,
    process::{Command, ExitStatus},
    sync::Mutex,
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use tracing::warn;

// `from_child` (test seam) and the lifecycle tests build fake process groups;
// production code hands groups only through the child_process module.
#[cfg(test)]
use command_group::GroupChild;

use crate::child_process::port::{is_same_process_group, port_owner_pid};
use crate::child_process::{
    kill_group, register_for_exit, spawn_with_group, unregister_for_exit, ManagedChild,
};

/// Test-only serialization for tests that touch the process-global exit
/// registry (`EXIT_REGISTRY`): `kill_registered_groups` mem::takes the whole
/// registry, so every register/unregister test must run one at a time.
/// Module-level pub(crate) so child_process.rs tests can share it (Task 5).
#[cfg(test)]
pub(crate) static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Outstanding user intent about the ALAS SCHEDULER (not the backend), for
/// distinguishing "stopped on purpose" from "stopped because it errored".
///
/// The scheduler-death signal alone cannot tell them apart (both look like
/// Running backend + dead scheduler process), so the toggle records what the
/// user last asked for; the poll thread disarms `Start` once the scheduler is
/// confirmed alive again. `None` = no outstanding intent — a dead scheduler
/// then means it died on its own (abnormal stop).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SchedulerIntent {
    /// No outstanding user intent — scheduler death renders as abnormal stop.
    #[default]
    None,
    /// User's last scheduler action was Stop — death renders as normal stop.
    Stop,
    /// User's last action was Start (scheduler or whole backend) — the dead
    /// window while the scheduler boots renders as normal stop, not abnormal.
    Start,
}

/// Whole-backend lifecycle state. Internal to [`BackendLifecycle`] (callers
/// never see or lock it); kept as-is from the pre-deep-module layout.
/// `status` drives the tray menu labels/enabled state; `backend` holds the
/// live process handle; `start_failed` distinguishes "stopped" from "last
/// start attempt failed" so the tray can show the start-failed label instead
/// of silently reverting to Start. `crashed` distinguishes "stopped because
/// the backend died on its own" from a normal stop; `scheduler_intent`
/// records the user's last scheduler Start/Stop request (see
/// [`SchedulerIntent`]).
pub struct BackendState {
    pub status: BackendStatus,
    pub backend: Option<ManagedBackend>,
    pub start_failed: bool,
    pub crashed: bool,
    pub scheduler_intent: SchedulerIntent,
    /// When the current `Start` intent was armed (`begin_start` / `start`
    /// success) — the MINOR-2 TTL clock. `None` = no start in flight; cleared
    /// on disarm/stop. Read only by the macOS-gated
    /// `advance_intent_if_changed` (win/linux writes are harmless).
    start_intent_at: Option<Instant>,
    /// Generation counter for the exit race (see `BackendLifecycle::start`):
    /// `stop()` bumps it, so a start whose spawn is still in flight detects
    /// the interruption when it re-locks after the spawn and disposes its
    /// late child instead of overwriting the stopped state.
    epoch: u64,
    /// Epoch captured by the latest `begin_start()` — the start attempt the
    /// next `start()` call belongs to. `None` = no begin_start pending.
    pending_start_epoch: Option<u64>,
}

impl Default for BackendState {
    fn default() -> Self {
        Self {
            status: BackendStatus::Stopped,
            backend: None,
            start_failed: false,
            crashed: false,
            scheduler_intent: SchedulerIntent::None,
            start_intent_at: None,
            epoch: 0,
            pending_start_epoch: None,
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
    pub crashed: bool,
    pub scheduler_intent: SchedulerIntent,
}

pub struct ManagedBackend {
    child: Option<ManagedChild>,
    #[cfg(test)]
    #[expect(dead_code)] // read by Task 2's wait_for_ready (ready_skip short-circuit)
    ready_skip: bool,
}

impl ManagedBackend {
    /// Spawn gui.py as the leader of a new process group and register it with
    /// the exit registry — the spawn-window safety net (spec §3.1): even before
    /// the child is installed in state, an exit sweep can group-kill it.
    /// No readiness wait: that lives in `BackendLifecycle::wait_for_ready`.
    pub fn spawn(port: u16) -> Result<Self> {
        std::env::set_var("ALAS_LAUNCHER_PID", format!("{}", std::process::id()));
        let mut cmd = Command::new("python");
        cmd.args(["gui.py", "--host", "127.0.0.1", "--port", &port.to_string()]);
        let child = spawn_with_group(&mut cmd)?;
        register_for_exit(&child);
        Ok(Self {
            child: Some(child),
            #[cfg(test)]
            ready_skip: false,
        })
    }

    /// Spawn + 60s port-wait, fused (legacy shape kept until Task 2 moves the
    /// wait into the lifecycle).
    pub fn new(port: u16) -> Result<Self> {
        let mut res = Self::spawn(port)?;

        let address = format!("127.0.0.1:{}", port).parse().unwrap();
        let start_time = std::time::Instant::now();
        while start_time.elapsed() < Duration::from_secs(60) {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                if let Some(child) = res.child.as_mut() {
                    // Metis MINOR-6: the port may be held by a stale/unrelated
                    // process. Verify the spawned child actually survived before
                    // declaring the backend ready.
                    if child
                        .try_wait()
                        .map_err(|e| anyhow!("Failed to check gui.py status: {e}"))?
                        .is_some()
                    {
                        return Err(anyhow!("gui.py exited before becoming ready"));
                    }
                    // MAJOR-3: the port may answer from a STALE server left by
                    // an earlier launcher instance while our gui.py is still
                    // coming up. Verify the listening process is our own child
                    // (or in its process group) before declaring Running — a
                    // mismatch means the port was already taken.
                    let spawned = child.id();
                    match port_owner_pid(port) {
                        Some(owner) if !is_same_process_group(owner, spawned) => {
                            return Err(anyhow!(
                                "Port {port} is occupied by pid {owner} (our backend is {spawned}); refusing to start over a stale server"
                            ));
                        }
                        // None → the port-owner tool is unavailable: fail-open
                        // (see child_process::port_owner_pid) and rely on the
                        // ALAS_LAUNCHER_PID residue scan in Drop.
                        _ => {}
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
            let pid = child.id();
            #[cfg(unix)]
            {
                use command_group::Signal;
                let _ = child.signal_group(Signal::SIGTERM);
                let start_time = std::time::Instant::now();
                while start_time.elapsed() < Duration::from_millis(500) {
                    if let Ok(Some(exit_status)) = child.try_wait() {
                        // Round-2 (Oracle 2)：leader 退出 ≠ 组已死——uvicorn 子进程 /
                        // 非 daemon multiprocessing 子进程可能存活。必须先 kill_group
                        // 清组再注销，否则注销后 EXIT_REGISTRY 兜底失效（孤儿组）。
                        // Round-4 (Oracle 1，同时解决 Momus 1 的测试确定性)：注销条件为
                        // Ok **或 ESRCH**。ESRCH ⟺ 组已死（zombie 也算成员、kill 返回
                        // Ok）→ 注销严格更优：清陈旧条目（防 pid 复用误杀），且注册表
                        // 测试用 sleep 子进程（SIGTERM 即死 → kill_group ESRCH）可确定性
                        // 通过。EPERM（组存活但不可杀）→ 不注销，registry best-effort 兜底。
                        match kill_group(&mut child) {
                            Ok(_) => unregister_for_exit(pid),
                            // Round-5 终审（Momus+Oracle 一致）：libc 非直接依赖（Cargo.toml
                            // 仅 nix 0.30 cfg(unix)），裸 libc::ESRCH 编译 E0433。改用
                            // nix::errno::Errno::ESRCH as i32——Errno 全平台 #[repr(i32)]，
                            // errno 模块无 feature 门控，仓库既有模式 child_process.rs:192。
                            Err(e) if e.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => {
                                unregister_for_exit(pid)
                            }
                            Err(_) => {}
                        }
                        return Ok(exit_status);
                    }
                    sleep(Duration::from_millis(100));
                }
                warn!("gui.py didn't exit, killing it...");
            }
            // kill 失败 → 不注销：组可能还活着，registry 是退出兜底。
            kill_group(&mut child)?;
            unregister_for_exit(pid);
            Ok(child.wait()?)
        } else {
            Ok(ExitStatus::default())
        }
    }

    /// Non-blocking exit poll for the readiness wait (Task 2's
    /// `wait_for_ready` calls this on the installed child).
    #[expect(dead_code)] // first call site lands in Task 2 (wait_for_ready)
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }

    /// Test seam: wrap an already-spawned process group so lifecycle tests
    /// can inject a fake backend without touching `spawn()`. `ready_skip`
    /// short-circuits the readiness wait (Task 2).
    #[cfg(test)]
    pub(crate) fn from_child(child: GroupChild) -> Self {
        Self::from_managed_child(ManagedChild::from_group_child(child))
    }

    /// Test seam: same, from a `ManagedChild` (lets registry-pairing tests
    /// register a real spawned group without invoking `spawn()`'s python).
    #[cfg(test)]
    pub(crate) fn from_managed_child(child: ManagedChild) -> Self {
        Self {
            child: Some(child),
            ready_skip: true,
        }
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
        // ALAS_LAUNCHER_PID 残留清扫（Round-1 修正 + Round-2 修正，双审 blocker）：
        // 只在 Drop 入口 child 仍被持有（未被 terminate 取走 = 真泄漏）时执行。
        // Round-2 关键修正：had_child 必须在 take **之前**捕获——`take()` 无条件
        // 置 None，若在 if-let 之后检查 is_some() 恒为 false，清扫变死代码
        // （panic 路径兜底 + 逃逸进程清扫全部丢失）。清扫会误杀并发 start 刚
        // spawn 的同 ALAS_LAUNCHER_PID 子进程——但那只发生在 terminate 已 take
        // child 的路径（had_child=false → 跳过 ✓）；had_child=true 时我们拥有
        // child，清扫仅杀逃逸进程（kill 失败存活组由 EXIT_REGISTRY 兜底）。
        let had_child = self.child.is_some();
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            match kill_group(&mut child) {
                Ok(_) => unregister_for_exit(pid),
                Err(e) => warn!("Failed to kill gui.py process: {e}"),
            }
        }
        if had_child {
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
}

/// Deep lifecycle module for the ALAS backend: all the ordering behavior (the
/// terminate-before-spawn contract, port readiness, start_failed, Drop
/// residue cleanup, process-group termination) lives here behind a small
/// interface — `status` / `snapshot` / `begin_start` / `start` / `stop` /
/// `mark_stopped` / `set_scheduler_intent` / `advance_intent_if_changed`
/// (macOS-only). The `Mutex` is INTERNAL: callers never lock it, so lock
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

    /// Copy of the render-relevant state (status + failure flags).
    pub fn snapshot(&self) -> BackendStateSnapshot {
        let state = self.state.lock().unwrap();
        BackendStateSnapshot {
            status: state.status,
            start_failed: state.start_failed,
            crashed: state.crashed,
            scheduler_intent: state.scheduler_intent,
        }
    }

    /// Mark the backend as initializing (and clear any previous start-failed
    /// flag) BEFORE the possibly-long spawn. Callers invoke this first so the
    /// menu shows "initializing…" and a concurrent toggle is a no-op
    /// (BLOCKER-3: never two backends). The start is user-initiated, so the
    /// scheduler intent arms to `Start` — the boot window renders as a normal
    /// stop, not an abnormal one.
    pub fn begin_start(&self) {
        let mut state = self.state.lock().unwrap();
        state.status = BackendStatus::Initializing;
        state.start_failed = false;
        state.crashed = false;
        state.scheduler_intent = SchedulerIntent::Start;
        // Start the MINOR-2 TTL clock: an unconfirmed Start intent older than
        // START_INTENT_TTL disarms on the next scan (abnormal-stop semantics).
        state.start_intent_at = Some(Instant::now());
        // Bind this start attempt to the current epoch; `start()` compares it
        // against the live epoch to detect a `stop()` landing in the
        // begin_start → spawn window (the exit race, MAJOR-2).
        state.pending_start_epoch = Some(state.epoch);
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
    /// poll thread reads the status every 3s) must never block on it.
    ///
    /// EXIT RACE (MAJOR-2): once the spawn runs on a worker thread, an
    /// ExitRequested can land inside the spawn window. `stop()` bumps the
    /// epoch, so this method detects the interruption — at entry (a stop
    /// between `begin_start` and here aborts before any spawn) or after the
    /// spawn (a stop mid-spawn disposes the late child and re-affirms
    /// Stopped). Either way the exit path stays authoritative and the start
    /// reports `STOP_INTERVENED` — never a bogus Running.
    pub fn start(&self, port: u16) -> Result<()> {
        let (old, epoch) = {
            let mut state = self.state.lock().unwrap();
            if let Some(pending) = state.pending_start_epoch {
                state.pending_start_epoch = None;
                if pending != state.epoch {
                    return Err(anyhow!(STOP_INTERVENED));
                }
            }
            state.status = BackendStatus::Initializing;
            state.start_failed = false;
            (state.backend.take(), state.epoch)
        };
        // Outside the lock: fully terminate + drop the old backend before any
        // new process can exist.
        terminate_old(old);
        match (self.spawner)(port) {
            Ok(backend) => {
                let mut state = self.state.lock().unwrap();
                if state.epoch != epoch {
                    // A stop() intervened while the spawn was in flight: it
                    // already took backend=None and set Stopped. Dispose the
                    // late child (terminate the process group) so it cannot
                    // orphan, re-affirm Stopped, and report the interruption.
                    state.status = BackendStatus::Stopped;
                    state.start_failed = false;
                    state.crashed = false;
                    state.scheduler_intent = SchedulerIntent::None;
                    state.start_intent_at = None;
                    drop(state);
                    let mut backend = backend;
                    let _ = backend.terminate();
                    drop(backend);
                    return Err(anyhow!(STOP_INTERVENED));
                }
                state.backend = Some(backend);
                state.status = BackendStatus::Running;
                state.crashed = false;
                state.scheduler_intent = SchedulerIntent::Start;
                // Re-arm the TTL clock at the moment the backend is actually
                // up: the 90s boot window starts here.
                state.start_intent_at = Some(Instant::now());
                Ok(())
            }
            Err(e) => {
                // Consistent start-failure marking for BOTH callers (Ready
                // thread and tray toggle) — previously the Ready thread left
                // start_failed unset.
                let mut state = self.state.lock().unwrap();
                state.status = BackendStatus::Stopped;
                state.start_failed = true;
                state.crashed = false;
                state.scheduler_intent = SchedulerIntent::None;
                state.start_intent_at = None;
                Err(e)
            }
        }
    }

    /// Stop the backend (if any): status -> Stopped, all failure flags and
    /// the scheduler intent cleared, then the process is terminated and
    /// dropped OUTSIDE the lock. Always ends Stopped; never panics on a
    /// missing backend. Bumps the epoch so an in-flight async start detects
    /// the stop and disposes its late child (the exit race, MAJOR-2).
    pub fn stop(&self) -> BackendStatus {
        let old = {
            let mut state = self.state.lock().unwrap();
            state.status = BackendStatus::Stopped;
            state.start_failed = false;
            state.crashed = false;
            state.scheduler_intent = SchedulerIntent::None;
            state.start_intent_at = None;
            state.epoch += 1;
            // pending_start_epoch is deliberately NOT cleared: it must survive
            // so a start() arriving after this stop can still detect it.
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
        state.crashed = false;
        state.scheduler_intent = SchedulerIntent::None;
        state.start_intent_at = None;
    }

    /// Stopped-with-error transition: the backend died on its own (port probe
    /// dead while Running) or was killed outside the launcher. Renders as the
    /// abnormal-stop label instead of a plain stop.
    pub fn mark_stopped_crashed(&self) {
        let mut state = self.state.lock().unwrap();
        state.status = BackendStatus::Stopped;
        state.start_failed = false;
        state.crashed = true;
        state.scheduler_intent = SchedulerIntent::None;
        state.start_intent_at = None;
    }

    /// Record the user's last scheduler Start/Stop request (see
    /// [`SchedulerIntent`]); `None` disarms. A non-Start intent also clears
    /// the MINOR-2 TTL clock: the start (if any) is no longer in flight.
    /// `Start` armed here (scheduler-only WS clicks) leaves the clock
    /// untouched — `begin_start`/`start` are the recorded TTL origins.
    pub fn set_scheduler_intent(&self, intent: SchedulerIntent) {
        let mut state = self.state.lock().unwrap();
        state.scheduler_intent = intent;
        if intent != SchedulerIntent::Start {
            state.start_intent_at = None;
        }
    }

    /// Run ONE poll-scan intent transition atomically (MINOR-1): read the
    /// current intent, apply [`scheduler_intent_after_scan`], write back —
    /// all under a single lock acquisition. The old rebuild_menu sequence
    /// (snapshot → compute → set_scheduler_intent) took three separate locks,
    /// leaving a read-modify-write window a concurrent toggle could slip
    /// into. Returns the surviving intent so the caller can render the
    /// post-transition state.
    ///
    /// `alive` is the scan's scheduler liveness — `None` when the backend is
    /// not Running (the transition is a no-op then). No I/O happens under the
    /// lock: the transition is a pure match on the snapshot fields.
    ///
    /// The decision function lives here with `toggle_decision` — both are
    /// backend-lifecycle decisions, not menu rendering (todo 8 relocation).
    /// MINOR-2: the elapsed time for the Start-intent TTL is derived from the
    /// internal `start_intent_at` record (armed by `begin_start`/`start`,
    /// cleared on disarm/stop) — an unconfirmed Start intent that survives
    /// [`START_INTENT_TTL`] disarms, so a scheduler that never boots
    /// eventually renders as abnormal stop. Stop intents are never TTL'd.
    #[cfg(target_os = "macos")]
    pub fn advance_intent_if_changed(&self, alive: Option<bool>) -> SchedulerIntent {
        let mut state = self.state.lock().unwrap();
        let elapsed = state
            .start_intent_at
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);
        let next = scheduler_intent_after_scan(state.scheduler_intent, alive, elapsed);
        state.scheduler_intent = next;
        if next != SchedulerIntent::Start {
            state.start_intent_at = None;
        }
        next
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

/// Error text for a start interrupted by [`BackendLifecycle::stop`] (the exit
/// race) — the caller must not treat it as a start failure.
const STOP_INTERVENED: &str = "stop intervened during backend start";

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
///
/// Since todo 6 the toggle drives the ALAS SCHEDULER through the webui
/// WebSocket when it can, and falls back to process-level control when it
/// cannot:
/// - `StopScheduler` / `StartScheduler`: scheduler-only control (the webui
///   stays alive; no window navigation).
/// - `StartBackend`: the backend is down (or Stopped) — bring the whole
///   backend up, then optionally start the scheduler over WS (the caller
///   decides that tail based on `ws_available`).
/// - `StopBackend`: DEGRADED mode only (webui password/SSL configured, so WS
///   control is impossible) — the legacy semantics, Running → stop the
///   backend process. Kept as an explicit variant (plan option A) instead of
///   mapping `StopScheduler`→`StopBackend` in the caller: the variant makes
///   the degraded behavior a first-class, testable decision cell rather than
///   a hidden rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleAction {
    NoOp,
    StartBackend,
    /// Degraded mode (no WS available): process-level stop, legacy semantics.
    StopBackend,
    StopScheduler,
    StartScheduler,
}

/// Pure toggle decision (todo 6 matrix):
///
/// | status      | scheduler_alive | ws_available | action         |
/// |-------------|-----------------|--------------|----------------|
/// | Initializing| any             | any          | NoOp           |
/// | Stopped     | any             | any          | StartBackend   |
/// | Running     | any             | false        | StopBackend    |
/// | Running     | true            | true         | StopScheduler  |
/// | Running     | false           | true         | StartScheduler |
///
/// `scheduler_alive` is the click-time process-tree liveness (re-scanned by
/// the caller outside any lock, never the poll cache); `ws_available` is the
/// password/SSL degradation flag.
pub(crate) fn toggle_decision(
    snapshot: &BackendStateSnapshot,
    scheduler_alive: bool,
    ws_available: bool,
) -> ToggleAction {
    match snapshot.status {
        BackendStatus::Initializing => ToggleAction::NoOp,
        BackendStatus::Stopped => ToggleAction::StartBackend,
        BackendStatus::Running => {
            if !ws_available {
                ToggleAction::StopBackend
            } else if scheduler_alive {
                ToggleAction::StopScheduler
            } else {
                ToggleAction::StartScheduler
            }
        }
    }
}

/// Start-intent TTL (MINOR-2): an unconfirmed `Start` intent older than this
/// disarms to `None` on the next scan, restoring the abnormal-stop semantics
/// for a scheduler that never came up after a user start.
#[cfg(target_os = "macos")]
pub const START_INTENT_TTL: Duration = Duration::from_secs(90);

/// Whether the ALAS scheduler is running, per the pinned process-tree
/// discriminator (evidence task-3): the uvicorn process's alive, non-zombie,
/// non-resource-tracker child count. The multiprocessing.Manager is the
/// permanent baseline child (+1); the scheduler adds a second one.
#[cfg(target_os = "macos")]
pub fn scheduler_alive(alive_child_count: usize) -> bool {
    alive_child_count > 1
}

/// What scheduler intent survives one poll scan (pure, no I/O).
///
/// The poll thread calls this after every liveness scan:
/// - a confirmed-alive scan disarms a `Start` intent (the boot window is
///   over — a later death has no user start behind it and must render as
///   abnormal stop);
/// - an UNCONFIRMED `Start` intent disarms once `elapsed` (the time since
///   the start was recorded) reaches [`START_INTENT_TTL`] — a scheduler that
///   never boots stops rendering as a normal stop (MINOR-2);
/// - every other combination leaves the intent untouched. `Stop` survives
///   alive scans so a user stop can never re-arm into the abnormal path.
#[cfg(target_os = "macos")]
pub fn scheduler_intent_after_scan(
    intent: SchedulerIntent,
    scheduler_alive: Option<bool>,
    elapsed: Duration,
) -> SchedulerIntent {
    match (intent, scheduler_alive) {
        (SchedulerIntent::Start, Some(true)) => SchedulerIntent::None,
        (SchedulerIntent::Start, _) if elapsed >= START_INTENT_TTL => SchedulerIntent::None,
        _ => intent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_group::CommandGroup;
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
    fn mark_stopped_crashed_marks_abnormal_stop() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.begin_start();
        lc.mark_stopped_crashed();
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Stopped);
        assert!(!s.start_failed);
        assert!(s.crashed);
        assert_eq!(s.scheduler_intent, SchedulerIntent::None);
    }

    #[test]
    fn scheduler_intent_roundtrip_and_clearing() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.set_scheduler_intent(SchedulerIntent::Stop);
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::Stop);
        // A user start re-arms to Start; begin_start also arms Start.
        lc.set_scheduler_intent(SchedulerIntent::Start);
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::Start);
        lc.begin_start();
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::Start);
        // Stop (and mark_stopped_crashed) disarm to None.
        lc.stop();
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::None);
        lc.set_scheduler_intent(SchedulerIntent::Stop);
        lc.mark_stopped_crashed();
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::None);
    }

    #[test]
    fn start_arms_start_intent_and_success_keeps_it() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.start(22267).unwrap();
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Running);
        assert_eq!(s.scheduler_intent, SchedulerIntent::Start);
        assert!(!s.crashed);
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

    // ---- the exit race (MAJOR-2): stop() landing inside the spawn window ----

    /// The worker-thread start runs outside the menu-event thread, so an
    /// ExitRequested can land while the (up-to-60s) spawn is still in flight.
    /// `stop()` must not be overwritten by the late-finishing start: the fresh
    /// child is disposed, the state stays Stopped, and start() reports the
    /// interruption.
    #[test]
    fn stop_during_start_disposes_child_and_restores_stopped() {
        // A 2-party barrier used twice keeps the ordering deterministic
        // (no sleep, no channel race): round 1 signals "the spawn step is
        // entered", round 2 releases the spawn only AFTER the test's stop()
        // has landed — so the epoch bump strictly precedes the re-lock.
        let gate = Arc::new(std::sync::Barrier::new(2));
        let gate2 = Arc::clone(&gate);
        let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let pids2 = Arc::clone(&pids);
        let lc = Arc::new(BackendLifecycle::new_with_spawner(move |_| {
            gate2.wait(); // signal mid-spawn
            gate2.wait(); // hold until the test's stop() has landed
            let child = sleep_child();
            pids2.lock().unwrap().push(child.id());
            Ok(ManagedBackend::from_child(child))
        }));

        lc.begin_start();
        let worker = {
            let lc = Arc::clone(&lc);
            std::thread::spawn(move || lc.start(22267))
        };
        gate.wait(); // the worker is now inside the spawn step

        lc.stop(); // the exit race lands inside the spawn window

        gate.wait(); // release the spawn; the epoch bump already landed
        let err = worker.join().unwrap().unwrap_err();
        assert!(
            err.to_string().contains("stop intervened"),
            "start must report the interruption, got: {err}"
        );
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Stopped, "state restored to Stopped");
        assert!(!s.start_failed, "an interruption is not a start failure");
        assert_eq!(lc.backend_pid(), None, "interrupted child is not installed");
        let pid = pids.lock().unwrap()[0];
        assert!(
            !process_is_alive(pid),
            "interrupted child must be disposed, not orphaned"
        );
    }

    /// A stop landing between begin_start and the worker's start() entry must
    /// abort the start BEFORE any spawn — the exit path stays authoritative.
    #[test]
    fn stop_between_begin_start_and_start_aborts_without_spawning() {
        let spawn_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let spawn_count2 = Arc::clone(&spawn_count);
        let lc = BackendLifecycle::new_with_spawner(move |_| {
            *spawn_count2.lock().unwrap() += 1;
            Ok(ManagedBackend::from_child(sleep_child()))
        });
        lc.begin_start();
        lc.stop(); // the exit lands between begin_start and start()
        let err = lc.start(22267).unwrap_err();
        assert!(
            err.to_string().contains("stop intervened"),
            "start must report the interruption, got: {err}"
        );
        assert_eq!(
            *spawn_count.lock().unwrap(),
            0,
            "no spawn may happen after a stop"
        );
        assert_eq!(lc.status(), BackendStatus::Stopped);
    }

    // ---- MINOR-1: atomic intent transition -----------------------------------

    /// The whole poll-scan intent transition (read current intent, apply
    /// scheduler_intent_after_scan, write back) now runs under ONE lock via
    /// advance_intent_if_changed — the old rebuild_menu sequence (snapshot →
    /// compute → set_scheduler_intent) took three separate lock acquisitions,
    /// so a toggle landing between them could be silently overwritten.
    #[cfg(target_os = "macos")]
    #[test]
    fn advance_intent_disarms_start_on_confirmed_alive() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.set_scheduler_intent(SchedulerIntent::Start);
        let next = lc.advance_intent_if_changed(Some(true));
        assert_eq!(next, SchedulerIntent::None, "confirmed alive disarms Start");
        assert_eq!(
            lc.snapshot().scheduler_intent,
            SchedulerIntent::None,
            "the write lands inside the same lock"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn advance_intent_keeps_intent_on_dead_or_unknown_scans() {
        // The boot window: a dead/unknown scan keeps Start armed (the death
        // renders as a normal stop until the scheduler is confirmed alive).
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.set_scheduler_intent(SchedulerIntent::Start);
        assert_eq!(
            lc.advance_intent_if_changed(Some(false)),
            SchedulerIntent::Start
        );
        assert_eq!(lc.advance_intent_if_changed(None), SchedulerIntent::Start);
        // No intent + any scan stays None.
        assert_eq!(lc.advance_intent_if_changed(Some(true)), SchedulerIntent::None);
        // A user Stop survives every scan (never re-arms into abnormal).
        lc.set_scheduler_intent(SchedulerIntent::Stop);
        assert_eq!(lc.advance_intent_if_changed(Some(true)), SchedulerIntent::Stop);
        assert_eq!(lc.advance_intent_if_changed(Some(false)), SchedulerIntent::Stop);
        assert_eq!(lc.advance_intent_if_changed(None), SchedulerIntent::Stop);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn start_intent_ttl_matrix() {
        // MINOR-2 matrix: Start + unconfirmed + <90s stays; >=90s disarms to
        // None (abnormal-stop semantics back); Start + confirmed always
        // disarms; Stop is never subject to the TTL.
        let below_ttl = Duration::from_secs(1);
        let at_ttl = START_INTENT_TTL;
        let past_ttl = START_INTENT_TTL + Duration::from_secs(3600);
        // Start + unconfirmed + <90s -> stays (boot window, never alarms).
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, None, below_ttl),
            SchedulerIntent::Start
        );
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, Some(false), below_ttl),
            SchedulerIntent::Start
        );
        // Start + unconfirmed + >=90s -> disarm.
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, None, at_ttl),
            SchedulerIntent::None
        );
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, Some(false), past_ttl),
            SchedulerIntent::None
        );
        // Start + confirmed -> None regardless of elapsed.
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, Some(true), below_ttl),
            SchedulerIntent::None
        );
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, Some(true), past_ttl),
            SchedulerIntent::None
        );
        // Stop -> always stays, any alive x any elapsed.
        for alive in [None, Some(false), Some(true)] {
            for elapsed in [below_ttl, at_ttl, past_ttl] {
                assert_eq!(
                    scheduler_intent_after_scan(SchedulerIntent::Stop, alive, elapsed),
                    SchedulerIntent::Stop,
                    "Stop must never be TTL'd (alive {alive:?}, elapsed {elapsed:?})"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn advance_intent_ttl_disarms_stale_start_via_lifecycle() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.begin_start();
        lc.start(22267).unwrap();
        // Fresh start: a dead scan keeps the intent armed (boot window).
        assert_eq!(
            lc.advance_intent_if_changed(Some(false)),
            SchedulerIntent::Start
        );
        // Backdate the recorded start moment past the TTL: the next dead scan
        // must disarm — the scheduler never came up.
        lc.state.lock().unwrap().start_intent_at =
            Some(Instant::now() - START_INTENT_TTL - Duration::from_secs(1));
        assert_eq!(
            lc.advance_intent_if_changed(Some(false)),
            SchedulerIntent::None,
            "unconfirmed Start past the TTL disarms"
        );
        assert_eq!(lc.snapshot().scheduler_intent, SchedulerIntent::None);
        assert!(
            lc.state.lock().unwrap().start_intent_at.is_none(),
            "disarm clears the TTL clock"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scheduler_intent_after_scan_only_disarms_start_on_confirmed_alive() {
        // With a fresh (zero) elapsed, only (Start, Some(true)) disarms;
        // everything else is a no-op — the TTL matrix above covers elapsed
        // >= START_INTENT_TTL.
        for (intent, alive) in [
            (SchedulerIntent::None, None),
            (SchedulerIntent::None, Some(false)),
            (SchedulerIntent::None, Some(true)),
            (SchedulerIntent::Stop, None),
            (SchedulerIntent::Stop, Some(false)),
            (SchedulerIntent::Stop, Some(true)),
            (SchedulerIntent::Start, None),
            (SchedulerIntent::Start, Some(false)),
        ] {
            assert_eq!(
                scheduler_intent_after_scan(intent, alive, Duration::ZERO),
                intent,
                "intent {intent:?} alive {alive:?}"
            );
        }
        assert_eq!(
            scheduler_intent_after_scan(SchedulerIntent::Start, Some(true), Duration::ZERO),
            SchedulerIntent::None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scheduler_alive_count_boundaries() {
        // Pinned rule (evidence task-3): Manager baseline = 1 -> not alive;
        // a second alive child (the scheduler) crosses the threshold.
        assert!(!scheduler_alive(0), "no children");
        assert!(!scheduler_alive(1), "Manager baseline only");
        assert!(scheduler_alive(2), "Manager + scheduler");
        assert!(scheduler_alive(3), "any extra child counts alive");
    }

    /// BLOCKER-3 double-spawn insurance: while a start is in flight the
    /// lifecycle is Initializing, which makes the toggle decision NoOp for
    /// every (scheduler_alive, ws_available) combination — a second click
    /// never reaches a second start(). The tray's in-flight guard is the
    /// second layer on top (tray.rs in_flight_acquire_rejects_second_toggle).
    #[test]
    fn begin_start_disables_toggle_so_a_second_click_cannot_spawn() {
        let lc = BackendLifecycle::new_with_spawner(ok_spawner());
        lc.begin_start();
        let s = lc.snapshot();
        assert_eq!(s.status, BackendStatus::Initializing);
        for alive in [false, true] {
            for ws in [false, true] {
                assert_eq!(toggle_decision(&s, alive, ws), ToggleAction::NoOp);
            }
        }
    }

    #[test]
    fn webui_url_format() {
        assert_eq!(webui_url(22267), "http://127.0.0.1:22267/");
    }

    #[test]
    fn toggle_decision_matrix() {
        let snapshot = |status| BackendStateSnapshot {
            status,
            start_failed: false,
            crashed: false,
            scheduler_intent: SchedulerIntent::None,
        };
        let stopped = snapshot(BackendStatus::Stopped);
        let running = snapshot(BackendStatus::Running);
        let initializing = snapshot(BackendStatus::Initializing);

        // Initializing: NoOp regardless of scheduler liveness or ws state.
        for alive in [false, true] {
            for ws in [false, true] {
                assert_eq!(
                    toggle_decision(&initializing, alive, ws),
                    ToggleAction::NoOp
                );
            }
        }
        // Stopped: StartBackend regardless of ws availability (the ws tail,
        // if any, is the caller's concern).
        assert_eq!(toggle_decision(&stopped, false, true), ToggleAction::StartBackend);
        assert_eq!(toggle_decision(&stopped, true, true), ToggleAction::StartBackend);
        assert_eq!(toggle_decision(&stopped, false, false), ToggleAction::StartBackend);
        // Running + ws available: the scheduler state decides.
        assert_eq!(toggle_decision(&running, true, true), ToggleAction::StopScheduler);
        assert_eq!(toggle_decision(&running, false, true), ToggleAction::StartScheduler);
        // Running + degraded (password/SSL): legacy process-level stop.
        assert_eq!(toggle_decision(&running, true, false), ToggleAction::StopBackend);
        assert_eq!(toggle_decision(&running, false, false), ToggleAction::StopBackend);
    }

    /// The spawn()/terminate()/Drop registry pairing: registered at spawn,
    /// dropped on terminate (after a successful group kill) and on Drop, so a
    /// recycled pid can never be swept by a later exit.
    #[test]
    fn registry_wiring_spawn_terminate() {
        let _g = REGISTRY_TEST_LOCK.lock().unwrap();
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let managed = crate::child_process::spawn_with_group(&mut cmd).unwrap();
        let pid = managed.id();
        crate::child_process::register_for_exit(&managed);
        assert!(
            crate::child_process::registered_pids().contains(&pid),
            "registered child must appear in the exit registry"
        );
        let mut mb = ManagedBackend::from_managed_child(managed);
        mb.terminate().unwrap();
        assert!(
            !crate::child_process::registered_pids().contains(&pid),
            "terminate must unregister the child after a successful group kill"
        );
    }

    #[test]
    fn drop_unregisters_from_exit_registry() {
        let _g = REGISTRY_TEST_LOCK.lock().unwrap();
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let managed = crate::child_process::spawn_with_group(&mut cmd).unwrap();
        let pid = managed.id();
        crate::child_process::register_for_exit(&managed);
        drop(ManagedBackend::from_managed_child(managed));
        assert!(
            !crate::child_process::registered_pids().contains(&pid),
            "Drop must unregister the child after a successful group kill"
        );
    }
}
