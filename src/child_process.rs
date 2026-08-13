////// Managed child processes: process-group spawn, timeout-wait, exit
////// registry and port-ownership probes (Momus ADVISORY-1/2, plan todo 7).

use anyhow::{anyhow, Result};
use command_group::{CommandGroup, GroupChild};
use std::io;
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::window_util::CreateNoWindow as _;

/// Hard ceiling for repository-update subprocesses (`git_update` /
/// `atomic_failure_cleanup`).
///
/// 600s, not 300s: the update script runs `gm.git_install()`, which is a
/// fetch of the ALAS history PLUS a pip install of the toolkit wheels. On a
/// cold cache or slow link that comfortably exceeds 300s. The timeout exists
/// only to turn a wedged child into a loud failure instead of a splash hang,
/// so a generous ceiling costs nothing (Momus ADVISORY-2).
pub const GIT_UPDATE_TIMEOUT: Duration = Duration::from_secs(600);

/// A child spawned in its own process group (a job object on Windows).
///
/// Owns the `command_group` wrapper so callers never see two group layers:
/// spawn through [`spawn_with_group`] and the group semantics (killing the
/// whole tree, not just the leader) are guaranteed for every child this
/// module hands out.
pub struct ManagedChild {
    child: GroupChild,
}

impl ManagedChild {
    /// Wrap an already-spawned `GroupChild` (test seam, e.g.
    /// `ManagedBackend::from_child`).
    #[cfg(test)]
    pub(crate) fn from_group_child(child: GroupChild) -> Self {
        Self { child }
    }

    /// Pid of the process-group leader.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Non-blocking exit poll; `Ok(Some(status))` once the leader exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Block until the leader exits (reaps it).
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Move the piped stdout into a reader thread; None when not piped.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.inner().stdout.take()
    }

    /// Move the piped stderr into a reader thread; None when not piped.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.inner().stderr.take()
    }

    /// Signal the whole process group (Unix only).
    #[cfg(unix)]
    pub fn signal_group(&mut self, sig: command_group::Signal) -> io::Result<()> {
        use command_group::UnixChildExt;
        self.child.signal(sig)
    }
}

/// Spawn `cmd` as the leader of a new process group (job object on Windows).
///
/// Wraps `command_group`'s `group()` — on macOS/Linux the child calls setpgid
/// itself before exec, on Windows a kill-on-close job object is created — so
/// [`kill_group`] and the timeout path kill the whole tree (python plus its
/// git/pip grandchildren), never leaving orphans.
pub fn spawn_with_group(cmd: &mut Command) -> Result<ManagedChild> {
    let child = cmd.group().create_no_window().spawn()?;
    Ok(ManagedChild { child })
}

/// Kill the child's whole process group (SIGKILL to the group on Unix, job
/// terminate on Windows). Does not reap; the caller still calls `wait()`.
pub fn kill_group(child: &mut ManagedChild) -> io::Result<()> {
    child.child.kill()
}

/// Wait up to `timeout` for the child to exit; on timeout the process group
/// is killed and reaped and Err is returned — a loud failure instead of a
/// hang for the caller.
pub fn wait_with_timeout(child: &mut ManagedChild, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = kill_group(child);
            let _ = child.wait();
            return Err(anyhow!("Process group timed out after {timeout:?}"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Exit registry (Momus ADVISORY-2): pids of live process groups spawned by
/// the Ready thread. The main thread's ExitRequested kills them, so closing
/// the window mid-update never leaves an orphaned git/pip tree.
static EXIT_REGISTRY: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

fn exit_registry() -> &'static Mutex<Vec<u32>> {
    EXIT_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register `child` for group-kill when the app exits.
pub fn register_for_exit(child: &ManagedChild) {
    exit_registry().lock().unwrap().push(child.id());
}

/// Drop a pid from the exit registry once its group is no longer running —
/// a recycled pid must never be killed on a later exit.
pub fn unregister_for_exit(pid: u32) {
    exit_registry().lock().unwrap().retain(|p| *p != pid);
}

/// Kill every registered process group (best effort). Called from the main
/// thread's ExitRequested. Unix: SIGKILL to each group. Windows: no queryable
/// job handle exists from a pid, so the registered leader is killed via
/// sysinfo — the launcher is exiting anyway, and `ManagedBackend`'s Drop runs
/// the ALAS_LAUNCHER_PID residue scan for backend children.
pub fn kill_registered_groups() {
    let pids = std::mem::take(&mut *exit_registry().lock().unwrap());
    for pid in pids {
        kill_process_group(pid);
    }
}

/// Kill the process group led by `pid` (best effort, retried briefly).
///
/// A SIGKILL sent during the spawn handshake can be delayed until the
/// child's exec completes (measured on macOS: kills accepted at t0, process
/// dead by t+50ms) or miss entirely before the child's pre-exec setpgid
/// runs — a single kill(-pgid) may silently fail, so exit cleanup must not
/// rely on timing luck.
fn kill_process_group(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if process_is_gone(pid) {
            return;
        }
        if signal_process_group(pid) {
            // Accepted — give the kernel time to land the signal.
            thread::sleep(Duration::from_millis(100));
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Send SIGKILL to the group and the leader; true when both were accepted.
#[cfg(unix)]
fn signal_process_group(pid: u32) -> bool {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    // Negative pid = the whole group; the direct leader kill closes the
    // pre-setpgid window (before exec the leader has no descendants anyway).
    kill(Pid::from_raw(-(pid as i32)), Some(Signal::SIGKILL)).is_ok()
        && kill(Pid::from_raw(pid as i32), Some(Signal::SIGKILL)).is_ok()
}

#[cfg(windows)]
fn signal_process_group(pid: u32) -> bool {
    let sys = sysinfo::System::new_all();
    match sys.process(sysinfo::Pid::from_u32(pid)) {
        Some(proc) => proc.kill(),
        None => true, // already gone
    }
}

/// True when no process exists at `pid` (a zombie still counts as existing).
#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(kill(Pid::from_raw(pid as i32), None), Err(nix::errno::Errno::ESRCH))
}

#[cfg(windows)]
fn process_is_gone(pid: u32) -> bool {
    let sys = sysinfo::System::new_all();
    sys.process(sysinfo::Pid::from_u32(pid)).is_none()
}

pub mod port;
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[cfg(unix)]
    use nix::sys::signal::kill as sys_kill;
    #[cfg(unix)]
    use nix::unistd::{getpgid, Pid};

    /// A process that lives ~30s; the test always kills it before returning.
    #[cfg(unix)]
    fn spawn_long_running() -> ManagedChild {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        spawn_with_group(&mut cmd).unwrap()
    }

    #[cfg(windows)]
    fn spawn_long_running() -> ManagedChild {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "ping", "-n", "30", "127.0.0.1"]);
        spawn_with_group(&mut cmd).unwrap()
    }

    /// A process that exits immediately and successfully.
    #[cfg(unix)]
    fn spawn_quick() -> ManagedChild {
        let mut cmd = Command::new("sleep");
        cmd.arg("0");
        spawn_with_group(&mut cmd).unwrap()
    }

    #[cfg(windows)]
    fn spawn_quick() -> ManagedChild {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "ver"]);
        spawn_with_group(&mut cmd).unwrap()
    }

    /// True when no process remains in the group led by `leader` (Unix only;
    /// kill(-pgid, 0) → ESRCH).
    #[cfg(unix)]
    fn group_is_gone(leader: u32) -> bool {
        sys_kill(Pid::from_raw(-(leader as i32)), None)
            .map(|_| false)
            .unwrap_or_else(|e| e == nix::errno::Errno::ESRCH)
    }

    #[test]
    fn spawn_with_group_creates_own_process_group() {
        let mut child = spawn_long_running();
        let pid = child.id();
        assert!(pid > 0);
        #[cfg(unix)]
        {
            // The child must lead its OWN group (setpgid before exec), so a
            // later kill_group cannot touch the launcher or its siblings.
            let pgid = getpgid(Some(Pid::from_raw(pid as i32))).unwrap();
            assert_eq!(pgid.as_raw() as u32, pid);
        }
        let _ = kill_group(&mut child);
        let _ = child.wait();
        #[cfg(unix)]
        assert!(group_is_gone(pid));
    }

    #[test]
    fn wait_with_timeout_returns_status_when_child_exits() {
        let mut child = spawn_quick();
        let status = wait_with_timeout(&mut child, Duration::from_secs(5)).unwrap();
        assert!(status.success());
    }

    #[test]
    fn wait_with_timeout_kills_group_and_errors() {
        let mut child = spawn_long_running();
        let pid = child.id();
        let err = wait_with_timeout(&mut child, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
        #[cfg(unix)]
        assert!(group_is_gone(pid), "process group must be killed on timeout");
    }

    #[cfg(unix)]
    #[test]
    fn kill_group_reaps_whole_group() {
        // `sh -c "sleep 30"` — sh leads the group, sleep is a member. Killing
        // the group must take BOTH down, not just the leader.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let mut child = spawn_with_group(&mut cmd).unwrap();
        let pid = child.id();
        let _ = kill_group(&mut child);
        let _ = child.wait();
        assert!(group_is_gone(pid));
    }

    #[test]
    fn exit_registry_register_and_unregister() {
        // Registered → kill_registered_groups() takes the group down.
        let mut child = spawn_long_running();
        register_for_exit(&child);
        kill_registered_groups();
        assert!(
            child.try_wait().unwrap().is_some(),
            "registered group must be killed by kill_registered_groups()"
        );
        // Unregistered → the group survives kill_registered_groups().
        let mut child2 = spawn_long_running();
        register_for_exit(&child2);
        unregister_for_exit(child2.id());
        kill_registered_groups();
        assert!(
            child2.try_wait().unwrap().is_none(),
            "unregistered group must survive kill_registered_groups()"
        );
        let _ = kill_group(&mut child2);
        let _ = child2.wait();
    }
}
