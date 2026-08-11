use anyhow::{anyhow, Result};
use serde_json::Value as JsonValue;
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;
use tracing::{info, warn};

use crate::child_process::{
    kill_group, register_for_exit, spawn_with_group, unregister_for_exit, wait_with_timeout,
    GIT_UPDATE_TIMEOUT,
};

fn alas_repo_dir() -> PathBuf {
    // Always check if this is a typical same-folder portable distribution
    let exe_folder = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut installer_py = exe_folder.clone();
    installer_py.extend(["deploy", "installer.py"]);
    if fs::exists(installer_py).unwrap() {
        return exe_folder;
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
                return repo_folder;
            }
        }
    }
    panic!("Cannot find ALAS repo folder");
}

fn prepend_path_to_env(key: &str, path: PathBuf) {
    let mut paths = Vec::new();
    paths.push(path);
    if let Some(ref old_path) = &std::env::var_os(key) {
        paths.extend(std::env::split_paths(old_path));
    }
    std::env::set_var(key, std::env::join_paths(paths).unwrap());
}

#[cfg(unix)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    prepend_path_to_env("PATH", dir.join("toolkit").join("libexec").join("git-core"));
    prepend_path_to_env("PATH", dir.join("toolkit").join("bin"));
    prepend_path_to_env("LD_LIBRARY_PATH", dir.join("toolkit").join("lib"));
    Ok(())
}

#[cfg(windows)]
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("ALAS dir is {:?}", &dir);
    set_current_dir(&dir)?;
    prepend_path_to_env("PATH", dir.join("toolkit").join("git").join("cmd"));
    prepend_path_to_env("PATH", dir.join("toolkit").join("Scripts"));
    prepend_path_to_env("PATH", dir.join("toolkit"));
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

pub fn setup_alas_repo(mut status_updater: impl FnMut(&str)) -> Result<()> {
    info!("Starting setup for ALAS repository...");
    #[cfg(target_os = "linux")]
    setup_git_ca_bundle();
    // Similar setup to deploy/installer.py
    status_updater("Cleaning up config files");
    atomic_failure_cleanup("./config")?;
    status_updater("Updating ALAS");
    git_update(status_updater)?;
    Ok(())
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

fn git_update(mut status_updater: impl FnMut(&str)) -> Result<()> {
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
    let mut child = spawn_with_group(&mut cmd)?;
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
    let deadline = Instant::now() + GIT_UPDATE_TIMEOUT;

    // Receive lines and tee them to stdout/stderr and the status_updater callback.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
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
            Err(RecvTimeoutError::Timeout) => {
                // Silent past the deadline. The child may have just exited
                // (readers still draining buffered output): only kill the
                // group when it is genuinely still running.
                if child.try_wait()?.is_some() {
                    break;
                }
                let _ = kill_group(&mut child);
                let _ = child.wait();
                return Err(anyhow!(
                    "Repository update timed out after {GIT_UPDATE_TIMEOUT:?}"
                ));
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Wait for child to exit and check status
    let status = child.wait()?;
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
    // must fail the setup loudly, not hang the splash forever. The exit
    // status stays unchecked — as before, only spawn/timeout errors fail.
    let mut child = spawn_with_group(&mut cmd)?;
    let _ = wait_with_timeout(&mut child, GIT_UPDATE_TIMEOUT)?;
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
    #[test]
    fn test_find_percentage() {
        assert_eq!(Some(8), find_percentage("8%"));
        assert_eq!(Some(25), find_percentage("loading 25%..."));
        assert_eq!(Some(100), find_percentage("100%..."));
        assert_eq!(None, find_percentage("%1"));
    }
}
