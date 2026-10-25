use anyhow::{anyhow, Result};

use crate::window_util::CreateNoWindow as _;
use serde_json::Value as JsonValue;
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::child_process::{
    register_for_exit, spawn_with_group, unregister_for_exit, wait_with_timeout,
    GIT_UPDATE_TIMEOUT,
};

/// Fallible [`alas_repo_dir`]: `None` when no portable layout matches,
/// instead of panicking — lets stale-cleanup probe for an install first.
pub(crate) fn try_alas_repo_dir() -> Option<PathBuf> {
    // Always check if this is a typical same-folder portable distribution.
    // Fork-stable markers (neither exists upstream LmeSzinc): pyproject.toml
    // (the uv project file) plus module/cli (our CLI entry). The legacy
    // deploy/installer.py marker was deleted from the payload in e2f4ea925
    // (2026-08) and is kept only as a fallback for older payload snapshots.
    let exe_folder = std::env::current_exe()
        .ok()?
        .parent()?
        .to_path_buf();
    let mut pyproject = exe_folder.clone();
    pyproject.push("pyproject.toml");
    let mut module_cli = exe_folder.clone();
    module_cli.extend(["module", "cli"]);
    let mut installer_py = exe_folder.clone();
    installer_py.extend(["deploy", "installer.py"]);
    if (fs::exists(pyproject).unwrap() && fs::exists(module_cli).unwrap())
        || fs::exists(installer_py).unwrap()
    {
        return Some(exe_folder);
    }
    // If it's MacOS, it could be ALAS.app/Contents/AzurLaneAutoScript
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;
        if exe_folder.file_name() == Some(OsStr::new("MacOS")) {
            let mut repo_folder = exe_folder;
            repo_folder.pop();
            repo_folder.push("AzurLaneAutoScript");
            if fs::exists(&repo_folder).unwrap() {
                return Some(repo_folder);
            }
        }
    }
    None
}

pub(crate) fn alas_repo_dir() -> PathBuf {
    try_alas_repo_dir().unwrap_or_else(|| panic!("Cannot find ALAS repo folder"))
}

/// Fallible [`setup_environment`]: `Err` when no portable layout matches, so
/// main can surface a user-visible error (native dialog — the Tauri windows
/// do not exist yet) instead of panicking without any UI on a broken
/// install (moved/renamed folder next to the executable).
fn alas_repo_dir_or_err() -> Result<PathBuf> {
    try_alas_repo_dir().ok_or_else(|| {
        anyhow!(
            "Cannot find the ALAS repo folder.\nPortable layout: pyproject.toml + module/cli \
             next to the executable.\nmacOS: ALAS.app/Contents/AzurLaneAutoScript."
        )
    })
}

fn prepend_path_to_env(key: &str, path: PathBuf) {
    let mut paths = Vec::new();
    paths.push(path);
    if let Some(ref old_path) = &std::env::var_os(key) {
        paths.extend(std::env::split_paths(old_path));
    }
    std::env::set_var(key, std::env::join_paths(paths).unwrap());
}

/// PATH entries prepended for a payload, most-specific first. The classic
/// portable `toolkit/` layout ships its own python+git; the PR #5885 payload
/// has no toolkit and expects a uv-managed `.venv` inside the payload tree
/// (created by `uv sync` at install/switch time), with `git`/`adb` resolved
/// from the system PATH.
fn payload_path_prepend(dir: &std::path::Path) -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        vec![
            dir.join("toolkit").join("libexec").join("git-core"),
            dir.join("toolkit").join("bin"),
            dir.join(".venv").join("bin"),
        ]
    }
    #[cfg(windows)]
    {
        vec![
            dir.join("toolkit").join("git").join("cmd"),
            dir.join("toolkit").join("Scripts"),
            dir.join("toolkit"),
            dir.join(".venv").join("Scripts"),
        ]
    }
}

#[cfg(unix)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir_or_err()?;
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    for path in payload_path_prepend(&dir) {
        prepend_path_to_env("PATH", path);
    }
    prepend_path_to_env("LD_LIBRARY_PATH", dir.join("toolkit").join("lib"));
    Ok(())
}

#[cfg(windows)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir_or_err()?;
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    for path in payload_path_prepend(&dir) {
        prepend_path_to_env("PATH", path);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_git_ca_bundle() {
    let cert_file = openssl_probe::probe().cert_file;
    if let Some(file) = cert_file.as_ref().and_then(|f| f.to_str()) {
        let _ = Command::new("git")
            .args(["config", "--local", "http.sslCAInfo", file])
            .status();
    }
}

/// Setup 子进程 spawn 失败的可行动化包装（R5 审计 A2）：裸目录/拷贝不全的
/// payload 树上 PATH 里没有 unversioned `python`（由 payload 的 toolkit/ 或
/// uv 管理的 .venv 提供），原始错误只有 os error 2，对用户毫无指向性。
fn explain_spawn_error(e: anyhow::Error) -> anyhow::Error {
    let not_found = e
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
    if not_found {
        e.context(
            "`python` was not found on PATH — the ALAS payload tree must provide it \
             (the classic toolkit/ layout, or the uv-managed .venv created by `uv sync`)",
        )
    } else {
        e
    }
}

pub fn setup_alas_repo(mut status_updater: impl FnMut(&str)) -> Result<()> {
    info!("Starting setup for ALAS repository...");
    #[cfg(target_os = "linux")]
    setup_git_ca_bundle();
    // A webui-initiated update-restart request from a PREVIOUS session must
    // never fire on this launch (update_restart.rs consumes only requests
    // made while this instance lives).
    crate::update_restart::clear_stale_request();
    // Similar setup to deploy/installer.py
    // Fail-closed (R1 审计)：patch 步骤之前的任何早退都意味着 control API
    // 从未被确认就绪——PATCH_FAILED 若保持 false，托盘 WS 门控会放行一个
    // 可能不存在的端点（2300839 要消除的空转重试即复活）。git reset --hard
    // 会还原被注入的 __init__.py，"update 失败但补丁幸存"不可安全假设。
    status_updater("Cleaning up config files");
    if let Err(e) = atomic_failure_cleanup("./config") {
        crate::patch::mark_patch_failed();
        return Err(explain_spawn_error(e));
    }
    status_updater("Updating ALAS");
    if let Err(e) = git_update(&mut status_updater) {
        crate::patch::mark_patch_failed();
        return Err(explain_spawn_error(e));
    }
    apply_sparse_checkout(&mut status_updater);
    status_updater("Applying control API patch");
    match crate::patch::apply_patch(&alas_repo_dir()) {
        Ok(crate::patch::PatchOutcome::Applied) => {
            status_updater("Control API patch applied");
        }
        Ok(crate::patch::PatchOutcome::AlreadyApplied) => {
            status_updater("Control API patch already applied");
        }
        Ok(crate::patch::PatchOutcome::AnchorMismatch) => {
            status_updater("Control API patch skipped (anchor mismatch; degraded mode)");
        }
        Err(e) => {
            warn!("control API patch failed: {e:#}; continuing in degraded mode");
            status_updater("Control API patch failed (degraded mode)");
        }
    }
    Ok(())
}

/// Ensure the payload worktree is a full checkout.
///
/// Sparse-checkout state (`.git/info/sparse-checkout` +
/// `core.sparseCheckout`) survives both update paths in
/// `deploy.git.GitManager`: `git_repository_init` (reset --hard +
/// pull --ff-only) and the GitOverCdn pack download (raw git pack
/// objects, ends in the same reset --hard) — verified empirically.
///
/// Sparse-checkout patterns (non-cone, gitignore semantics — the last
/// matching pattern wins, so any future exclusion must follow `/*`).
///
/// `/*` includes every top-level entry so future dirs appear
/// automatically. The old denylist (`!webapp/`, `!webapp-tauri/src-tauri/`)
/// stripped what was then the dead upstream Electron tree — stale since the
/// payload renamed `webapp-tauri/` -> `webapp/` (2026-09-08): the fork's
/// `webapp/` is the ACTIVE Svelte SPA whose sources build the webui frontend
/// (`alas build frontend`; `webapp/dist/` is gitignored, so it survives
/// resets either way). Keeping this mechanism (instead of deleting it) is
/// deliberate: re-running `sparse-checkout set /*` on every startup clears
/// the stale denylist that older launcher builds wrote, re-materializing
/// `webapp/` sources on existing installs. Fail-soft: old git (<2.25, no
/// sparse-checkout) or a broken repo degrades to the current tree state
/// instead of blocking the launch.
fn sparse_checkout_patterns() -> Vec<&'static str> {
    vec!["/*"]
}

fn apply_sparse_checkout(status_updater: &mut impl FnMut(&str)) {
    status_updater("Ensuring full checkout (clears legacy webapp/ exclusion)");
    let dir = alas_repo_dir();
    let mut args = vec!["sparse-checkout", "set", "--no-cone"];
    args.extend(sparse_checkout_patterns());
    let status = Command::new("git")
        .current_dir(&dir)
        .args(&args)
        // Windows GUI 进程派生控制台程序会弹可见黑窗（R4 审计 P1）；unix no-op。
        .create_no_window()
        .status();
    match status {
        Ok(s) if s.success() => {
            info!("sparse checkout applied: full tree in {:?}", dir)
        }
        Ok(s) => warn!("sparse checkout failed with {s}; keeping current tree state"),
        Err(e) => warn!("sparse checkout failed: {e:#}; keeping current tree state"),
    }
}

pub fn get_deploy_config() -> Option<JsonValue> {
    let config_content = fs::read_to_string("./config/deploy.yaml").ok()?;
    let config: JsonValue = serde_yaml::from_str(&config_content).ok()?;
    Some(config)
}

fn pipe_lines(read: impl Read + Send + 'static, tx: Sender<(bool, String)>, is_err: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(read);
        let mut buffer = "".to_owned();
        loop {
            let mut line = [0u8; 64];
            match reader.read(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    for c in &line[0..size] {
                        if *c < 32 || *c > 127 {
                            if !buffer.is_empty() {
                                let _ = tx.send((is_err, buffer));
                                buffer = "".to_owned();
                            }
                        } else if *c as char == ':' {
                            let mut cut = 0usize;
                            if let Some((l, r)) = buffer.split_once(':') {
                                if r.ends_with(l) {
                                    cut = r.len() + 1;
                                }
                            }
                            if cut > 0 {
                                let (l, r) = buffer.split_at(cut);
                                let _ = tx.send((is_err, l.to_owned()));
                                buffer = r.to_owned();
                            }
                            buffer.push(*c as char);
                        } else {
                            buffer.push(*c as char);
                        }
                    }
                }
            }
        }
        if !buffer.is_empty() {
            let _ = tx.send((is_err, buffer));
        }
    });
}

fn git_update(status_updater: impl FnMut(&str)) -> Result<()> {
    // Decorate execute() to get fetch progress
    let script = r#"
import deploy.git
def decorate_execute(fn):
    def new_fn(*args, **kwargs):
        if len(args) >= 1 and ' fetch ' in args[0] and '--progress' not in args[0]:
            args = (args[0].replace(' fetch ', ' fetch --progress '),) + args[1:]
        return fn(*args, **kwargs)
    return new_fn
gm = deploy.git.GitManager()
gm.execute = decorate_execute(gm.execute)
gm.git_install()
"#;
    // Spawn the child with piped stdout/stderr so we can tee them. The
    // child_process module wraps it in its own process group (job object on
    // Windows), so a timeout or app exit can kill the whole tree — python
    // plus its git/pip grandchildren — instead of leaving orphans.
    let mut cmd = Command::new("python");
    cmd.args(["-c", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    run_update_command(&mut cmd, GIT_UPDATE_TIMEOUT, status_updater)
}

/// The same deadline covers streaming output AND the final process wait.
/// EOF on both pipes does not prove that the child has exited.
fn run_update_command(
    cmd: &mut Command,
    timeout: Duration,
    mut status_updater: impl FnMut(&str),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut child = spawn_with_group(cmd)?;
    // Register with the exit registry: if the user closes the window mid-
    // update, ExitRequested group-kills this child.
    register_for_exit(&child);

    // Unregister once the update is done — a recycled pid must never be
    // killed on a later exit.
    struct ExitGuard(u32);
    impl Drop for ExitGuard {
        fn drop(&mut self) {
            unregister_for_exit(self.0);
        }
    }
    let _guard = ExitGuard(child.id());

    // Channels to receive lines from reader threads. (is_err, line)
    let (tx, rx) = mpsc::channel::<(bool, String)>();

    // Spawn a reader thread for stdout
    if let Some(stdout) = child.take_stdout() {
        pipe_lines(stdout, tx.clone(), false);
    }

    // Spawn a reader thread for stderr
    if let Some(stderr) = child.take_stderr() {
        pipe_lines(stderr, tx.clone(), true);
    }

    // Drop the original sender so rx will close when both reader threads finish.
    drop(tx);

    let mut last_err = "".to_owned();

    // Deadline-based recv_timeout (the run_git_capture pattern): the timeout
    // is TOTAL, not per line — a chatty but wedged fetch still fires.
    // Receive lines and tee them to stdout/stderr and the status_updater callback.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Even continuously queued output must not extend the deadline.
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((is_err, line)) => {
                if line.contains("=====") {
                    let sanitized = line.replace("=", " ").trim().to_owned();
                    status_updater(&format!("Updating ALAS: {sanitized}"));
                } else if line.contains("objects:")
                    || line.contains("deltas:")
                    || line.contains("files:")
                {
                    let sanitized = line.trim().to_owned();
                    let mut n = 0usize;
                    if let Some(precentage) = find_percentage(&sanitized) {
                        n = (precentage / 2) as usize;
                    }
                    let bar = "=".repeat(n) + &" ".repeat(50 - n);
                    status_updater(&format!("Updating ALAS: {sanitized}\n[{bar}]"));
                }
                if is_err {
                    warn!("{line}");
                    last_err = line;
                } else {
                    info!("{line}");
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Wait for child to exit and check status. The loop above only breaks on
    // EOF or deadline exhaustion, so the final wait usually has zero budget;
    // wait_with_timeout still reaps/kills, but report the timeout once instead
    // of nesting a "timed out after 0s" inside it.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let status = wait_with_timeout(&mut child, remaining)
        .map_err(|e| {
            if remaining.is_zero() {
                // Keep the "timed out" keyword: callers (and tests) match on it.
                anyhow!("Repository update did not finish within {timeout:?}: timed out")
            } else {
                anyhow!("Repository update did not finish within {timeout:?}: {e}")
            }
        })?;
    if !status.success() {
        if last_err.is_empty() {
            last_err = "Failed to update repository".to_owned();
        }
        return Err(anyhow!(last_err));
    }
    Ok(())
}

fn atomic_failure_cleanup(path: &str) -> Result<()> {
    let mut cmd = Command::new("python");
    cmd.args([
        "-c",
        "import sys; from deploy.atomic import atomic_failure_cleanup; atomic_failure_cleanup(sys.argv[1])",
        path,
    ]);
    // Same group + timeout treatment as git_update: a wedged cleanup script
    // must fail the setup loudly, not hang the splash forever. Registered for
    // exit too (R4 审计：此前未注册——脚本卡住时用户退出，registry 没有它、
    // ALAS_LAUNCHER_PID 兜底也扫不到（setup 阶段尚未 spawn 过 backend），
    // 修复进程会脱离超时约束继续跑）。
    let mut child = spawn_with_group(&mut cmd)?;
    register_for_exit(&child);
    struct ExitGuard(u32);
    impl Drop for ExitGuard {
        fn drop(&mut self) {
            unregister_for_exit(self.0);
        }
    }
    let _guard = ExitGuard(child.id());
    let status = wait_with_timeout(&mut child, GIT_UPDATE_TIMEOUT)?;
    // A non-success exit most often means the payload no longer ships
    // `deploy/atomic` (the fork reorg already removed gui.py) — the config
    // repair silently did nothing. Loud in the log, but NOT a setup
    // failure: a skipped repair step must not block the launch.
    if !status.success() {
        warn!("atomic_failure_cleanup exited with {status}; config repair skipped");
    }
    Ok(())
}

fn find_percentage(s: &str) -> Option<u8> {
    s.split('%')
        .next()
        .and_then(|before| {
            before
                .rsplit(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
        })
        .and_then(|num| {
            num.parse::<f32>()
                .ok()
                .map(|v| v.round().clamp(0.0, u8::MAX as f32) as u8)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn update_command(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }

    #[cfg(unix)]
    #[test]
    fn update_closed_output_still_times_out_and_kills_descendants() {
        let _registry = crate::backend::REGISTRY_TEST_LOCK.lock().unwrap();
        let pid_file = std::env::temp_dir().join(format!("update-eof-{}.pid", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let mut cmd = update_command(
            "echo $$ > \"$1\"; exec 1>&- 2>&-; sleep 30 & wait",
        );
        cmd.arg("update-eof-test").arg(&pid_file);
        let started = Instant::now();
        let result = run_update_command(&mut cmd, Duration::from_millis(250), |_| {});
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3), "EOF must not trigger an unbounded wait");
        let pid: i32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(!crate::child_process::is_registered(pid as u32));
        let deadline = Instant::now() + Duration::from_secs(2);
        while nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(Instant::now() < deadline, "update process group survived timeout");
            thread::sleep(Duration::from_millis(20));
        }
        fs::remove_file(pid_file).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn update_continuous_output_cannot_extend_deadline() {
        let _registry = crate::backend::REGISTRY_TEST_LOCK.lock().unwrap();
        let mut cmd = update_command("while :; do echo fetching; sleep 0.01; done");
        let started = Instant::now();
        let result = run_update_command(&mut cmd, Duration::from_millis(200), |_| {});
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn update_success_and_nonzero_exit_keep_their_results() {
        let _registry = crate::backend::REGISTRY_TEST_LOCK.lock().unwrap();
        let mut success = update_command("echo '===== fetch complete ====='");
        let mut progress = Vec::new();
        run_update_command(&mut success, Duration::from_secs(3), |text| progress.push(text.to_string())).unwrap();
        assert!(progress.iter().any(|text| text.contains("fetch complete")));
        let mut failure = update_command("echo 'fetch failed' >&2; exit 7");
        let error = run_update_command(&mut failure, Duration::from_secs(3), |_| {}).unwrap_err();
        assert!(error.to_string().contains("fetch failed"));
    }

    #[test]
    fn test_find_percentage() {
        assert_eq!(Some(8), find_percentage("8%"));
        assert_eq!(Some(25), find_percentage("loading 25%..."));
        assert_eq!(Some(100), find_percentage("100%..."));
        assert_eq!(None, find_percentage("%1"));
    }

    #[test]
    fn test_payload_path_prepend_includes_venv() {
        let dir = std::path::Path::new("/repo");
        let entries = payload_path_prepend(dir);
        let rendered = format!("{:?}", entries);
        // uv-managed venv resolves python for the PR #5885 payload...
        if cfg!(unix) {
            assert!(rendered.contains("\"/repo/.venv/bin\""), "{rendered}");
        } else {
            assert!(rendered.contains("\"/repo/.venv/Scripts\""), "{rendered}");
        }
        // ...while the classic toolkit layout keeps priority (prepended first).
        let venv_pos = rendered.find(".venv").expect("venv entry present");
        let toolkit_pos = rendered.find("toolkit").expect("toolkit entries present");
        assert!(toolkit_pos < venv_pos, "toolkit must precede .venv: {rendered}");
    }

    #[test]
    fn test_sparse_checkout_patterns() {
        // `/*` first: non-cone patterns need the include-all baseline up
        // front, and gitignore semantics make later negations win over it.
        // No exclusions: `webapp/` is the ACTIVE Svelte SPA since the
        // payload's webapp-tauri -> webapp rename (2026-09-08), and
        // `webapp-tauri/` no longer exists — see sparse_checkout_patterns.
        let patterns = sparse_checkout_patterns();
        assert_eq!(patterns.first(), Some(&"/*"));
        assert!(
            !patterns.iter().any(|p| p.starts_with('!')),
            "webapp/ is the active SPA; nothing may be excluded: {patterns:?}"
        );
    }
}
