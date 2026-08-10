use std::{
    net::TcpStream,
    process::{Command, ExitStatus},
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

/// Shared whole-backend lifecycle state, owned as `Arc<Mutex<BackendState>>`
/// by main.rs and handed (cloned) to tray.rs. `status` drives the tray menu
/// labels/enabled state; `backend` holds the live process handle; `start_failed`
/// distinguishes "stopped" from "last start attempt failed" so the tray can
/// show "Backend: start failed" instead of silently reverting to Start.
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
