//! Minimal HTTP/1.1 client for the ALAS control API (patch-injected).
//! Deliberately dependency-free: one-shot TcpStream per call, JSON bodies,
//! localhost only (no TLS — password/SSL degrades client-side via
//! deploy_config::ws_control_available, and the server also 401s).
//!
//! Cross-platform (not macOS-gated): `update_restart.rs` also consumes the
//! protocol handshake on every platform.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

/// Protocol version the launcher expects from the patched-in control API.
/// MUST match `PROTOCOL_VERSION` in assets/patches/control_api.py — both ship
/// inside this binary, so in production they cannot drift; the handshake
/// exists to catch a payload-side file that survived from a different
/// launcher version (git reset timing, hand-edited trees, future direct
/// commits in the payload repo).
pub const EXPECTED_PROTOCOL_VERSION: u32 = 2;

/// Last confirmed protocol result. Handshakes run under the shared operation
/// permit so an old backend's response cannot overwrite a newer session.
/// Transport errors preserve the last confirmed result.
static PROTOCOL_MISMATCH: AtomicBool = AtomicBool::new(false);

pub fn protocol_mismatch() -> bool {
    PROTOCOL_MISMATCH.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstanceState {
    pub name: String,
    pub state: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProtocolState {
    pub version: u32,
}

/// Scheduler control direction (moved here from the deleted pywebio.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerAction {
    Start,
    Stop,
}

#[derive(Debug)]
pub enum ApiError {
    Locked,
    Http(u16),
    Io(std::io::Error),
    Protocol(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Locked => write!(f, "control API locked (password/SSL)"),
            ApiError::Http(code) => write!(f, "control API HTTP {code}"),
            ApiError::Io(e) => write!(f, "control API io: {e}"),
            ApiError::Protocol(m) => write!(f, "control API protocol: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

const TIMEOUT: Duration = Duration::from_secs(5);

fn request(port: u16, method: &str, path: &str) -> Result<Vec<u8>, ApiError> {
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(ApiError::Io)?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(ApiError::Io)?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(ApiError::Io)?;
    let req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(ApiError::Io)?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(ApiError::Io)?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ApiError::Protocol("no header/body separator".into()))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| ApiError::Protocol("malformed status line".into()))?;
    match status {
        200 => Ok(body.as_bytes().to_vec()),
        401 => Err(ApiError::Locked),
        other => Err(ApiError::Http(other)),
    }
}

fn parse_instance(body: &[u8]) -> Result<InstanceState, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::Protocol(e.to_string()))
}

pub fn api_instances(port: u16) -> Result<Vec<InstanceState>, ApiError> {
    let body = request(port, "GET", "/api/alas/instances")?;
    serde_json::from_slice(&body).map_err(|e| ApiError::Protocol(e.to_string()))
}

pub fn api_protocol(port: u16) -> Result<ProtocolState, ApiError> {
    let body = request(port, "GET", "/api/alas/protocol")?;
    serde_json::from_slice(&body).map_err(|e| ApiError::Protocol(e.to_string()))
}

/// Call synchronously on the worker while holding the operation permit.
/// A matching version recovers a previous mismatch; transport errors leave
/// the last confirmed result unchanged and the next backend start retries.
pub fn run_protocol_handshake(port: u16) {
    match api_protocol(port) {
        Ok(state) if state.version == EXPECTED_PROTOCOL_VERSION => {
            PROTOCOL_MISMATCH.store(false, Ordering::Relaxed);
            info!(target: "control_api", "protocol handshake ok (v{})", state.version);
        }
        Ok(state) => {
            warn!(
                "control API protocol mismatch: payload reports v{}, launcher expects v{EXPECTED_PROTOCOL_VERSION}; scheduler control degrades",
                state.version
            );
            PROTOCOL_MISMATCH.store(true, Ordering::Relaxed);
        }
        Err(e) => {
            warn!("control API protocol handshake failed; preserving last confirmed result: {e}");
        }
    }
}

pub fn api_scheduler_start(port: u16, name: &str) -> Result<InstanceState, ApiError> {
    let body = request(port, "POST", &format!("/api/alas/{name}/scheduler/start"))?;
    parse_instance(&body)
}

pub fn api_scheduler_stop(port: u16, name: &str) -> Result<InstanceState, ApiError> {
    let body = request(port, "POST", &format!("/api/alas/{name}/scheduler/stop"))?;
    parse_instance(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Serve ONE canned HTTP response, capture the request line, then close.
    fn mock_server(response: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).unwrap();
            tx.send(String::from_utf8_lossy(&buf[..n]).to_string()).unwrap();
            sock.write_all(response.as_bytes()).unwrap();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        (port, rx)
    }

    const OK_INSTANCES: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n[{\"name\":\"alas\",\"state\":1}]";
    const LOCKED: &str = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 18\r\n\r\n{\"error\":\"locked\"}";
    const OK_PROTOCOL: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{\"version\":2}";
    const BAD_PROTOCOL: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{\"version\":1}";

    #[test]
    fn instances_parses_json_array() {
        let (port, _rx) = mock_server(OK_INSTANCES);
        let list = api_instances(port).unwrap();
        assert_eq!(list, vec![InstanceState { name: "alas".into(), state: 1 }]);
    }

    #[test]
    fn locked_returns_api_error_locked() {
        let (port, _rx) = mock_server(LOCKED);
        assert!(matches!(api_scheduler_start(port, "alas"), Err(ApiError::Locked)));
    }

    #[test]
    fn protocol_parses_version() {
        let (port, _rx) = mock_server(OK_PROTOCOL);
        assert_eq!(api_protocol(port).unwrap(), ProtocolState { version: 2 });
    }

    /// PATCH test serialization: the mismatch flag is a process global.
    static HANDSHAKE_FLAG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn handshake_recovers_on_matching_version_and_preserves_result_on_io_error() {
        let _g = HANDSHAKE_FLAG_TEST_LOCK.lock().unwrap();
        PROTOCOL_MISMATCH.store(false, Ordering::Relaxed);
        let (port, _rx) = mock_server(OK_PROTOCOL);
        run_protocol_handshake(port);
        assert!(!protocol_mismatch(), "matching version must stay fail-open");
        let (port, _rx) = mock_server(BAD_PROTOCOL);
        run_protocol_handshake(port);
        assert!(protocol_mismatch(), "wrong version must latch the flag");
        run_protocol_handshake(1);
        assert!(protocol_mismatch(), "transport failure must not clear a confirmed mismatch");
        let (port, _rx) = mock_server(OK_PROTOCOL);
        run_protocol_handshake(port);
        assert!(!protocol_mismatch(), "matching version must recover the control channel");
        // Transport error: fail-open — unknown ≠ wrong.
        run_protocol_handshake(1);
        assert!(!protocol_mismatch(), "io error must NOT latch the flag");
    }
}

/// Real-payload integration test, ported from the deleted src/pywebio.rs
/// `real_payload_tests` module (commit 029e95b removed the pywebio DOM
/// client). The helpers now require a disposable isolated copy, refuse port
/// reuse and clean up only the test-owned process group. The pywebio version guard and
/// the WebSocket `click_scheduler` are replaced by:
///   - `crate::patch::apply_patch` (idempotent; anchor mismatch fails), and
///   - the control API `api_instances` / `api_scheduler_start` /
///     `api_scheduler_stop`.
///
/// State assertions poll with `wait_for` (start is async — asserting
/// `state == 1` immediately would race).
///
/// Run with ALAS_TEST_PAYLOAD set to a marked disposable temp copy:
/// `cargo test api_roundtrip_real_payload -- --ignored --nocapture --test-threads=1`
#[cfg(all(test, target_os = "macos"))] // nix + std::os::unix + isolated real payload
mod real_payload_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::Instant;

    /// Only disposable copies under the system temp directory are accepted.
    /// Running an ignored integration test must never mutate /Applications.
    fn isolated_payload() -> std::path::PathBuf {
        let configured = std::env::var_os("ALAS_TEST_PAYLOAD")
            .expect("set ALAS_TEST_PAYLOAD to a disposable payload copy under the system temp directory");
        validate_isolated_payload(Path::new(&configured))
    }

    fn validate_isolated_payload(configured: &Path) -> std::path::PathBuf {
        let payload = configured.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert!(payload.starts_with(temp), "real-payload tests require an isolated temp-directory copy");
        assert_eq!(
            std::fs::read_to_string(payload.join(".alas-launcher-test-root")).expect(
                ".alas-launcher-test-root marker file missing — prepare the copy per README 隔离验证（macOS）"
            ),
            "disposable-alas-payload\n",
            "missing disposable-payload marker"
        );
        payload
    }

    #[test]
    fn real_payload_guard_rejects_installed_paths_and_requires_disposable_marker() {
        assert!(std::panic::catch_unwind(|| validate_isolated_payload(Path::new("/Applications"))).is_err());
        let tmp = std::env::temp_dir().join(format!("alas-payload-guard-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(std::panic::catch_unwind(|| validate_isolated_payload(&tmp)).is_err());
        std::fs::write(tmp.join(".alas-launcher-test-root"), "disposable-alas-payload\n").unwrap();
        assert_eq!(validate_isolated_payload(&tmp), tmp.canonicalize().unwrap());
        std::fs::remove_dir_all(tmp).unwrap();
    }
    /// Isolated test port — NEVER 22267 (a user's live backend may be there).
    const TEST_PORT: u16 = 22367;
    /// The live backend port, probed read-only to prove we never touched it.
    const LIVE_PORT: u16 = 22267;
    /// Budget for the webui to boot after spawn.
    const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
    /// Budget for the scheduler child to appear after Start (todo 7: <=20s).
    const START_TIMEOUT: Duration = Duration::from_secs(20);
    /// Budget for the scheduler child to disappear after Stop (todo 7: <=10s).
    const STOP_TIMEOUT: Duration = Duration::from_secs(10);

    /// Mirror of src/setup.rs `prepend_path_to_env`.
    fn prepend_path(key: &str, path: &Path) {
        let mut paths = vec![path.to_path_buf()];
        if let Some(old) = std::env::var_os(key) {
            paths.extend(std::env::split_paths(&old));
        }
        std::env::set_var(key, std::env::join_paths(paths).unwrap());
    }

    /// Mirror src/setup.rs setup_environment (PATH prepend order) (unix `setup_environment`): cwd becomes
    /// the payload dir and PATH/LD_LIBRARY_PATH gain the toolkit entries.
    fn setup_payload_env(payload: &Path) {
        std::env::set_current_dir(payload).expect("set cwd to payload");
        prepend_path(
            "PATH",
            &payload.join("toolkit").join("libexec").join("git-core"),
        );
        prepend_path("PATH", &payload.join("toolkit").join("bin"));
        prepend_path("PATH", &payload.join(".venv").join("bin"));
        prepend_path("LD_LIBRARY_PATH", &payload.join("toolkit").join("lib"));
    }

    /// `Deploy.Update.EnableReload` from config/deploy.yaml (true when
    /// missing — the ALAS default): decides which process is the uvicorn.
    /// Kept as a payload-path mirror (QA runs against an arbitrary installed
    /// payload, not the launcher cwd) but the traversal itself is delegated
    /// to the shared `deploy_config` module so the mirror cannot drift.
    fn deploy_enable_reload(payload: &Path) -> bool {
        std::fs::read_to_string(payload.join("config").join("deploy.yaml"))
            .ok()
            .and_then(|s| serde_yaml::from_str::<serde_json::Value>(&s).ok())
            .map(|c| crate::deploy_config::DeployConfig::from_value(Some(&c)).enable_reload())
            .unwrap_or(true)
    }

    /// The multiprocessing resource_tracker child, excluded from the count
    /// (present in BOTH tree shapes; tray.rs `is_resource_tracker`).
    fn is_resource_tracker(p: &sysinfo::Process) -> bool {
        p.cmd()
            .iter()
            .any(|c| c.to_string_lossy().contains("multiprocessing.resource_tracker"))
    }

    /// The todo-3 discriminator, mirrored from tray.rs
    /// `uvicorn_alive_child_count`: alive non-zombie non-resource-tracker
    /// children of the uvicorn process. 0 when the backend is gone
    /// (conservative — scheduler shown stopped).
    fn scheduler_child_count(backend_pid: u32, enable_reload: bool) -> usize {
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

    fn port_open(port: u16) -> bool {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
    }

    /// HTTP status code of `GET /`; -1 when the port is not serving HTTP.
    fn webui_status(port: u16) -> i32 {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
            return -1;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        let mut buf = [0u8; 64];
        let Ok(n) = stream.read(&mut buf) else {
            return -1;
        };
        String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<i32>().ok())
            .unwrap_or(-1)
    }

    /// Poll `cond` every 500ms until it holds or `timeout` elapses.
    fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Poll the discriminator, printing each observation, until `pred`
    /// holds or `timeout` elapses. Returns the last observed count.
    fn wait_scheduler_count(
        what: &str,
        backend_pid: u32,
        reload: bool,
        timeout: Duration,
        pred: impl Fn(usize) -> bool,
    ) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let count = scheduler_child_count(backend_pid, reload);
            eprintln!("  [{what}] scheduler child count = {count}");
            if pred(count) || Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Poll `api_instances` for the named instance until its state satisfies
    /// `pred` or `timeout` elapses. Returns the last observed state (-1 when
    /// the API never returned the instance).
    fn wait_instance_state(
        what: &str,
        port: u16,
        name: &str,
        timeout: Duration,
        pred: impl Fn(u8) -> bool,
    ) -> i32 {
        let deadline = Instant::now() + timeout;
        let mut last: i32 = -1;
        loop {
            let list = api_instances(port);
            match list {
                Ok(list) => {
                    if let Some(inst) = list.iter().find(|i| i.name == name) {
                        last = i32::from(inst.state);
                        eprintln!("  [{what}] instance state = {last}");
                        if pred(inst.state) {
                            return last;
                        }
                    } else {
                        eprintln!("  [{what}] instance '{name}' not in API list");
                    }
                }
                Err(e) => eprintln!("  [{what}] api_instances error: {e}"),
            }
            if Instant::now() >= deadline {
                return last;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Owns exactly one test process group; never reuses an occupied port
    /// and never uses a process-name/port-wide pkill fallback.
    struct BackendGuard {
        child: Child,
        pgid: i32,
    }

    impl Drop for BackendGuard {
        fn drop(&mut self) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.pgid),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // Also terminate descendants if the leader exited first.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = self.child.wait();
        }
    }

    fn spawn_backend(payload: &Path) -> BackendGuard {
        assert!(!port_open(TEST_PORT), "test port {TEST_PORT} is occupied; refusing reuse");
        let python = [payload.join(".venv/bin/python"), payload.join("toolkit/bin/python")]
            .into_iter().find(|path| path.exists()).expect("payload Python missing");
        let mut cmd = Command::new(python);
        if payload.join("gui.py").exists() {
            cmd.arg("gui.py");
        } else {
            cmd.args(["-m", "module.cli", "run", "web"]);
            cmd.arg("--no-open");
        }
        cmd.args(["--host", "127.0.0.1", "--port", &TEST_PORT.to_string()]);
        cmd.current_dir(payload);
        cmd.process_group(0);
        let log = std::fs::File::create(payload.join("integration-backend.log")).unwrap();
        cmd.stdout(log.try_clone().unwrap()).stderr(log);
        let child = cmd.spawn().expect("failed to spawn the ALAS backend");
        let pid = child.id();
        eprintln!("spawned backend pid {pid} on port {TEST_PORT} (own process group)");
        BackendGuard { child, pgid: pid as i32 }
    }

    /// End-to-end scheduler control against the real payload on an isolated
    /// port: apply the control API patch -> spawn backend -> control API
    /// instances/start/stop -> scheduler child count + state assertions.
    /// Env mirrors the app (cwd = isolated payload, venv/toolkit PATH).
    #[test]
    #[ignore]
    fn api_roundtrip_real_payload() {
        let isolated = isolated_payload();
        let payload = isolated.as_path();
        assert!(!port_open(TEST_PORT), "test port occupied; refusing to patch or reuse a running payload");
        eprintln!("=== api_roundtrip_real_payload: isolated payload at {}", payload.display());
        eprintln!(
            "live backend ({LIVE_PORT}) listening before test = {}",
            port_open(LIVE_PORT)
        );

        // 1. Apply the control API patch (idempotent, anchor-verified). This
        //    is the production behavior — the launcher patches the real
        //    payload; the test exercises the same path. AnchorMismatch ->
        //    degraded mode -> the API cannot exist -> skip (documented).
        let init_path = payload
            .join("module")
            .join("webui")
            .join("api")
            .join("__init__.py");
        let control_path = payload.join("module").join("webui").join("control_api.py");
        let before = std::fs::read_to_string(&init_path).unwrap_or_default();
        eprintln!(
            "api/__init__.py patched marker before apply = {}",
            crate::patch::is_already_patched(&before)
        );
        match crate::patch::apply_patch(payload) {
            Ok(crate::patch::PatchOutcome::Applied) => {
                eprintln!("patch applied (fresh)");
            }
            Ok(crate::patch::PatchOutcome::AlreadyApplied) => {
                eprintln!("patch already applied (idempotent)");
            }
            Ok(crate::patch::PatchOutcome::AnchorMismatch) => {
                panic!("patch anchor mismatch in isolated real payload");
            }
            Err(e) => {
                panic!("apply_patch failed against real payload: {e:#}");
            }
        }
        // Idempotency + marker uniqueness on the real payload (F5).
        let second = crate::patch::apply_patch(payload).expect("second apply");
        assert_eq!(
            second,
            crate::patch::PatchOutcome::AlreadyApplied,
            "second apply must be AlreadyApplied"
        );
        let init_after = std::fs::read_to_string(&init_path).expect("read patched api/__init__.py");
        assert!(
            crate::patch::is_already_patched(&init_after),
            "marker missing after apply"
        );
        assert_eq!(
            init_after.matches(crate::patch::MARKER).count(),
            1,
            "marker must be unique (no double injection)"
        );
        assert!(control_path.exists(), "control_api.py missing after apply");
        eprintln!("patch idempotent: second apply AlreadyApplied, marker unique, control_api.py present");

        // Exercise upgrade and a failed write/recovery on real payload files,
        // before importing them into the backend. Nothing is mocked here.
        let importer = std::fs::read_to_string(&init_path).unwrap();
        std::fs::write(&control_path, "PROTOCOL_VERSION = 1\n# stale launcher module\n").unwrap();
        let blocked = control_path.with_extension("tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(crate::patch::apply_patch(payload).is_err());
        assert!(crate::patch::patch_failed());
        assert!(std::fs::read_to_string(&control_path).unwrap().contains("PROTOCOL_VERSION = 1"));
        std::fs::remove_dir(&blocked).unwrap();
        assert_eq!(crate::patch::apply_patch(payload).unwrap(), crate::patch::PatchOutcome::Applied);
        assert!(!crate::patch::patch_failed());
        assert_eq!(std::fs::read_to_string(&init_path).unwrap(), importer);
        eprintln!("real payload: old module upgrade + write failure recovery verified");

        setup_payload_env(payload);
        let reload = deploy_enable_reload(payload);
        eprintln!("Deploy.Update.EnableReload = {reload}");

        let guard = spawn_backend(payload);
        let backend_pid = guard.child.id();
        eprintln!("backend pid = {backend_pid}");

        assert!(
            wait_for(BOOT_TIMEOUT, || port_open(TEST_PORT)),
            "webui did not open port {TEST_PORT} within {BOOT_TIMEOUT:?}"
        );
        assert_eq!(webui_status(TEST_PORT), 200, "webui not serving HTTP 200");
        eprintln!("webui up on {TEST_PORT}: HTTP 200");
        assert_eq!(api_protocol(TEST_PORT).unwrap().version, EXPECTED_PROTOCOL_VERSION);
        run_protocol_handshake(TEST_PORT);
        assert!(!protocol_mismatch());
        eprintln!("upgraded control API protocol handshake: v{EXPECTED_PROTOCOL_VERSION}");

        // 3. instances: non-empty, contains "alas", state is a known value.
        assert!(
            wait_for(BOOT_TIMEOUT, || !api_instances(TEST_PORT)
                .map(|l| l.is_empty())
                .unwrap_or(true)),
            "api_instances never returned a non-empty list"
        );
        let instances = api_instances(TEST_PORT).expect("api_instances failed");
        assert!(!instances.is_empty(), "instances list is empty");
        let alas = instances
            .iter()
            .find(|i| i.name == "alas")
            .unwrap_or_else(|| panic!("'alas' missing from instances: {instances:?}"));
        assert!(
            (1..=4).contains(&alas.state),
            "instance state {} outside {{1,2,3,4}}",
            alas.state
        );
        eprintln!("instances: {instances:?} (state {})", alas.state);

        // Settle on the baseline (Manager-only) count before starting, so a
        // transient startup child can never satisfy the Start check.
        assert!(
            wait_for(Duration::from_secs(30), || {
                scheduler_child_count(backend_pid, reload) <= 1
            }),
            "scheduler child count never settled to baseline"
        );
        let baseline = scheduler_child_count(backend_pid, reload);
        eprintln!("baseline scheduler child count = {baseline}");

        // ---- START ----
        // start() is async: the response may still show the pre-start state,
        // so poll for state == 1 (Round-1 SHOULD-FIX) AND count == baseline+1.
        eprintln!("api_scheduler_start(\"alas\")");
        let started_state = api_scheduler_start(TEST_PORT, "alas")
            .expect("api_scheduler_start failed")
            .state;
        eprintln!("start response state = {started_state} (async; polling…)");
        let polled = wait_instance_state(
            "after Start",
            TEST_PORT,
            "alas",
            START_TIMEOUT,
            |state| state == 1,
        );
        assert_eq!(
            polled, 1,
            "scheduler must reach state 1 after Start (polled; last = {polled})"
        );
        let started = wait_scheduler_count(
            "after Start",
            backend_pid,
            reload,
            START_TIMEOUT,
            |count| count == baseline + 1,
        );
        assert_eq!(
            started,
            baseline + 1,
            "exactly one scheduler child must appear after Start; count {started} (a double spawn would show {})",
            baseline + 2,
        );
        assert_eq!(
            webui_status(TEST_PORT),
            200,
            "webui died while scheduler running"
        );
        eprintln!("scheduler STARTED: state 1, child count {started} (baseline {baseline} + 1), webui still 200");

        // ---- STOP ----
        eprintln!("api_scheduler_stop(\"alas\")");
        let stopped_state = api_scheduler_stop(TEST_PORT, "alas")
            .expect("api_scheduler_stop failed")
            .state;
        eprintln!("stop response state = {stopped_state} (async; polling…)");
        let stopped = wait_scheduler_count(
            "after Stop",
            backend_pid,
            reload,
            STOP_TIMEOUT,
            |count| count == baseline,
        );
        assert_eq!(
            stopped,
            baseline,
            "scheduler child count must return to the pre-Start baseline; count {stopped}"
        );
        let polled_stop = wait_instance_state(
            "after Stop",
            TEST_PORT,
            "alas",
            STOP_TIMEOUT,
            |state| state == 2,
        );
        assert_eq!(
            polled_stop, 2,
            "scheduler must reach state 2 (stopped) after Stop; last = {polled_stop}"
        );
        assert_eq!(webui_status(TEST_PORT), 200, "webui died after scheduler stop");
        eprintln!("scheduler STOPPED: child count {stopped}, state {polled_stop}, webui still 200");
        eprintln!(
            "live backend ({LIVE_PORT}) still listening = {}",
            port_open(LIVE_PORT)
        );

        drop(guard);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !port_open(TEST_PORT),
            "port {TEST_PORT} still open after guard cleanup"
        );
        eprintln!(
            "cleanup verified: port {TEST_PORT} closed; live {LIVE_PORT} intact = {}",
            port_open(LIVE_PORT)
        );
    }
}
