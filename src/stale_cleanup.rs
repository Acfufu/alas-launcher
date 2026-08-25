// Stale-process identity decision (pure, no I/O): given a snapshot of a live
// process, decide whether it is a stale ALAS backend left behind by a previous
// launcher run, and which evidence kind justifies killing it.
//
// Decision order (plan FR1.2): X vetoes first (own pid / own process group /
// registry-known pid), then the F anchor (executable must live under the ALAS
// repo's toolkit/), then evidence E1–E4. When the repo dir cannot be located
// (degraded mode) the F anchor is uncheckable, so only the strongest cmdline
// evidence (E2) may fire.
//
// Consumed by the discovery/kill loop (later tasks); until then the items are
// intentionally unused outside tests.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Snapshot of one candidate process (filled by the discovery pass).
#[derive(Clone)]
pub struct Candidate {
    pub pid: u32,
    pub pgid: Option<u32>,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub environ: Vec<OsString>,
    pub cmd: Vec<OsString>,
}

/// Which piece of evidence identified the process as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    /// Command line references the ALAS repo path.
    E1,
    /// Command line runs gui.py with our --port.
    E2,
    /// Working directory (or, on Windows, toolkit exe) is inside the repo.
    E3,
    /// Environment carries ALAS_LAUNCHER_PID= (launcher-spawned marker).
    E4,
}

/// Decide whether `c` is a stale ALAS backend process.
///
/// Returns the winning [`EvidenceKind`], or `None` when the process must be
/// spared (X veto, failed F anchor, or no evidence).
pub fn is_stale_candidate(
    c: &Candidate,
    repo: Option<&Path>,
    port: u16,
    launcher_pid: u32,
    launcher_pgid: Option<u32>,
    is_registered: &dyn Fn(u32) -> bool,
) -> Option<EvidenceKind> {
    // X1: never kill ourselves.
    if c.pid == launcher_pid {
        return None;
    }
    // X2: never kill anything in our own process group (tray helpers etc.).
    if let (Some(pgid), Some(lpgid)) = (c.pgid, launcher_pgid) {
        if pgid == lpgid {
            return None;
        }
    }
    // X3: pids we spawned and still track are alive by definition.
    if is_registered(c.pid) {
        return None;
    }
    // F anchor: executable must sit under <repo>/toolkit. Unmet ⇒ not ours.
    let exe_ok = c
        .exe
        .as_ref()
        .map(|p| repo.map(|r| p.starts_with(r.join("toolkit"))).unwrap_or(false))
        .unwrap_or(false);
    if repo.is_none() {
        // Degraded mode: repo dir unknown, F anchor unverifiable — keep only
        // the strongest cmdline evidence (E2), skip E1/E3/E4.
        let has_e2 = c.cmd.iter().any(|s| s.to_string_lossy().contains("gui.py"))
            && c.cmd.iter().any(|s| s.to_string_lossy() == format!("{port}"));
        return if has_e2 { Some(EvidenceKind::E2) } else { None };
    }
    if !exe_ok {
        return None;
    }
    let port_s = port.to_string();
    let cmd_s = c
        .cmd
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    // E1: command line mentions the repo path itself.
    let e1 = repo
        .map(|r| cmd_s.contains(r.to_string_lossy().as_ref()))
        .unwrap_or(false);
    // E2: argv[?] == "gui.py" exactly, followed by an ADJACENT "--port <port>"
    // pair (exact match — "--port=22267" or a detached value must not match).
    let e2 = c.cmd.iter().any(|s| s.to_string_lossy() == "gui.py")
        && c
            .cmd
            .windows(2)
            .any(|w| w[0].to_string_lossy() == "--port" && w[1].to_string_lossy() == port_s);
    // E3: cwd is the repo root (Windows additionally accepts a toolkit exe,
    // where cwd probing is less reliable).
    let e3 = {
        #[cfg(unix)]
        {
            c.cwd
                .as_ref()
                .map(|p| Some(p.as_path()) == repo)
                .unwrap_or(false)
        }
        #[cfg(windows)]
        {
            c.exe
                .as_ref()
                .map(|p| p.starts_with(repo.unwrap().join("toolkit")))
                .unwrap_or(false)
                || c.cwd.as_ref().map(|p| Some(p.as_path()) == repo).unwrap_or(false)
        }
    };
    // E4: environment carries the launcher-spawn marker.
    let e4 = c
        .environ
        .iter()
        .any(|e| e.to_string_lossy().starts_with("ALAS_LAUNCHER_PID="));
    if e1 {
        Some(EvidenceKind::E1)
    } else if e2 {
        Some(EvidenceKind::E2)
    } else if e3 {
        Some(EvidenceKind::E3)
    } else if e4 {
        Some(EvidenceKind::E4)
    } else {
        None
    }
}

/// Pre-filter the live process table down to raw stale candidates (FR1.1).
///
/// Over-collects on purpose: keeps processes matching the L-B cmdline pattern
/// (`gui.py` + adjacent `--port <port>` argv pair), the L-C heuristic hint
/// (`ALAS_LAUNCHER_PID=` environment marker), and — when the port is owned by
/// some pid not already in the set — that owner process too. The X∧F∧E gates
/// in [`is_stale_candidate`] do the precise killing decision later.
///
/// `enumerate` and `port_owner` are injected seams (T1): tests pass fakes,
/// production passes [`real_enumerate`] and `port_owner_pid`. `repo` is
/// reserved for later enrichment tasks and currently unused.
pub fn collect_raw_candidates(
    port: u16,
    repo: Option<&Path>,
    enumerate: &dyn Fn() -> Vec<Candidate>,
    port_owner: &dyn Fn(u16) -> Option<u32>,
) -> Vec<Candidate> {
    let _ = repo; // reserved for L-A enrichment in a later task
    let all = enumerate();
    let port_s = port.to_string();
    let mut raw: Vec<Candidate> = all
        .into_iter()
        .filter(|c| {
            // L-B: cmd contains gui.py AND an exact adjacent "--port <port>"
            // pair (no substring matching, no glued "--port=<port>").
            let has_port_pair = c
                .cmd
                .windows(2)
                .any(|w| w[0].to_string_lossy() == "--port" && w[1].to_string_lossy() == port_s);
            let is_lb = c.cmd.iter().any(|s| s.to_string_lossy().contains("gui.py"))
                && has_port_pair;
            // L-C heuristic: launcher-spawned marker in environ.
            let is_lc_hint = c
                .environ
                .iter()
                .any(|e| e.to_string_lossy().starts_with("ALAS_LAUNCHER_PID="));
            is_lb || is_lc_hint
        })
        .collect();
    if let Some(pid) = port_owner(port) {
        if !raw.iter().any(|c| c.pid == pid) {
            // Enrich the single owner pid via a fresh lookup; if it vanished
            // between calls it simply does not join the candidate set.
            if let Some(extra) = enumerate().into_iter().find(|c| c.pid == pid) {
                raw.push(extra);
            }
        }
    }
    raw
}

/// Truncate a rendered command line to 200 chars for log/display safety.
/// Char-safe (not byte-safe): multi-byte UTF-8 never splits mid-codepoint.
pub fn truncate_cmd(s: &str) -> String {
    if s.chars().count() <= 200 {
        s.to_string()
    } else {
        s.chars().take(200).collect()
    }
}

/// Production enumeration seam: snapshot every live process via sysinfo.
fn real_enumerate() -> Vec<Candidate> {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::everything(),
    );
    sys.processes()
        .values()
        .map(|p| {
            #[cfg(unix)]
            let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(
                p.pid().as_u32() as i32,
            )))
            .ok()
            .map(|pid| pid.as_raw() as u32);
            #[cfg(windows)]
            let pgid: Option<u32> = None;
            Candidate {
                pid: p.pid().as_u32(),
                pgid,
                exe: p.exe().map(|e| e.to_path_buf()),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                environ: p.environ().to_vec(),
                cmd: p.cmd().to_vec(),
            }
        })
        .collect()
}

/// One surviving stale process, rendered for the user-facing error payload.
#[derive(Debug, Clone)]
pub struct StaleProcessInfo {
    pub pid: u32,
    pub cmdline: String,
    pub evidence: String,
}

/// Why stale cleanup did not (fully) succeed.
#[derive(Debug)]
pub enum StaleCleanupError {
    /// Processes still alive after `u8` convergence rounds.
    Survivors(Vec<StaleProcessInfo>, u8),
    /// The port is held by a live process that is NOT a stale ALAS backend —
    /// killing it would be wrong, so the start is refused instead.
    ForeignPortOwner { port: u16, pid: u32, cmdline: String },
}

impl std::fmt::Display for StaleCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Survivors(v, r) => write!(
                f,
                "Failed to clean up stale ALAS processes after {} attempts: {}",
                r,
                v.iter()
                    .map(|i| format!("pid {}: {}", i.pid, i.cmdline))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::ForeignPortOwner { port, pid, cmdline } => write!(
                f,
                "Port {} is occupied by a non-ALAS process (pid {}: {})",
                port, pid, cmdline
            ),
        }
    }
}

impl std::error::Error for StaleCleanupError {}

// Stub for Task 3 compilation — real_kill lands in Task 6.
fn real_kill(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

/// Production entry point: converge stale ALAS processes on `port` to zero
/// within 10 rounds (200 ms apart), reporting progress via `progress`.
pub fn kill_stale_alas(port: u16, progress: &dyn Fn(&str)) -> Result<(), StaleCleanupError> {
    let repo = crate::setup::try_alas_repo_dir();
    kill_stale_alas_with_injection(
        port,
        progress,
        repo.as_deref(),
        &real_enumerate,
        &crate::child_process::port::port_owner_pid,
        &crate::child_process::is_registered,
        &mut real_kill,
        10,
        Duration::from_millis(200),
    )
}

fn kill_stale_alas_with_injection(
    port: u16,
    progress: &dyn Fn(&str),
    repo: Option<&Path>,
    enumerate: &dyn Fn() -> Vec<Candidate>,
    port_owner: &dyn Fn(u16) -> Option<u32>,
    is_registered: &dyn Fn(u32) -> bool,
    kill: &mut dyn FnMut(u32) -> std::io::Result<()>,
    max_rounds: u8,
    sleep: Duration,
) -> Result<(), StaleCleanupError> {
    let launcher_pid = std::process::id();
    #[cfg(unix)]
    let launcher_pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(
        launcher_pid as i32,
    )))
    .ok()
    .map(|p| p.as_raw() as u32);
    #[cfg(windows)]
    let launcher_pgid: Option<u32> = None;
    // AppTranslocation detect
    if let Ok(exe) = std::env::current_exe() {
        if exe.components().any(|c| c.as_os_str() == "AppTranslocation") {
            tracing::warn!("running under AppTranslocation — stale cleanup may be degraded");
        }
    }
    let mut round_survivors: Vec<StaleProcessInfo> = Vec::new();
    for round in 1..=max_rounds {
        let raw = collect_raw_candidates(port, repo, enumerate, port_owner);
        if raw.is_empty() {
            return Ok(());
        }
        // Port owner interception — X vs no-evidence routing (FR2.3, FR1.4)
        if let Some(owner_pid) = port_owner(port) {
            if let Some(c) = raw.iter().find(|c| c.pid == owner_pid) {
                let passes_x = c.pid != launcher_pid
                    && !is_registered(c.pid)
                    && match (c.pgid, launcher_pgid) {
                        (Some(a), Some(b)) => a != b,
                        _ => true,
                    };
                if passes_x {
                    if is_stale_candidate(c, repo, port, launcher_pid, launcher_pgid, is_registered)
                        .is_none()
                    {
                        let cmd_s = c
                            .cmd
                            .iter()
                            .map(|s| s.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(" ");
                        let cmdline = truncate_cmd(&cmd_s);
                        return Err(StaleCleanupError::ForeignPortOwner {
                            port,
                            pid: owner_pid,
                            cmdline,
                        });
                    }
                    // passes_x && F+E — will be killed via verified below
                } else {
                    // X veto (same pgid or registered) — count as survivor with
                    // detailed evidence per FR2.3/T3
                    let cmd_s = c
                        .cmd
                        .iter()
                        .map(|s| s.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let which_x = if c.pid == launcher_pid {
                        "X1 self"
                    } else if is_registered(c.pid) {
                        "X3 registered"
                    } else {
                        "X2 same pgid"
                    };
                    round_survivors.push(StaleProcessInfo {
                        pid: owner_pid,
                        cmdline: truncate_cmd(&cmd_s),
                        evidence: format!("{} F/E {:?}", which_x, c.cmd),
                    });
                }
            }
        }
        if !round_survivors.is_empty() {
            // X-intercepted survivors present — do not early-Ok, collect
            // verified as well and kill
        }
        let verified: Vec<&Candidate> = raw
            .iter()
            .filter(|c| {
                is_stale_candidate(c, repo, port, launcher_pid, launcher_pgid, is_registered)
                    .is_some()
            })
            .collect();
        if verified.is_empty() && round_survivors.is_empty() {
            tracing::warn!(
                "stale candidates filtered out (F/E): pids {:?} — possible F-anchor breakage",
                raw.iter().map(|c| c.pid).collect::<Vec<_>>()
            );
            return Ok(());
        }
        // Kill verified only (X-survivors not killed per veto) — per-kill
        // tracing per FR1.5
        for c in &verified {
            let evidence =
                is_stale_candidate(c, repo, port, launcher_pid, launcher_pgid, is_registered);
            match kill(c.pid) {
                Ok(_) => tracing::info!(
                    "killed pid {} pgid {:?} evidence {:?} cmd {:?}",
                    c.pid,
                    c.pgid,
                    evidence,
                    c.cmd
                ),
                Err(e) => tracing::error!("failed to kill pid {}: {}", c.pid, e),
            }
        }
        progress(&format!(
            "Cleaning up stale ALAS processes (round {round}/{max_rounds})..."
        ));
        std::thread::sleep(sleep);
        // On final round, build survivors payload from this round's verified +
        // X-intercepted (ensures non-empty per FR1.4)
        if round == max_rounds {
            let mut final_survivors = round_survivors.clone();
            for c in &verified {
                let cmd_s = c
                    .cmd
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ");
                final_survivors.push(StaleProcessInfo {
                    pid: c.pid,
                    cmdline: truncate_cmd(&cmd_s),
                    evidence: "verified survivor".into(),
                });
            }
            if !final_survivors.is_empty() {
                return Err(StaleCleanupError::Survivors(final_survivors, max_rounds));
            }
            return Ok(());
        }
        round_survivors.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "/tmp/fake/AzurLaneAutoScript";
    const PORT: u16 = 22267;

    fn repo() -> PathBuf {
        PathBuf::from(REPO)
    }

    fn cand(pid: u32, pgid: Option<u32>) -> Candidate {
        Candidate {
            pid,
            pgid,
            exe: None,
            cwd: None,
            environ: vec![],
            cmd: vec![],
        }
    }

    // ---- T1 cases from the brief ----

    #[test]
    fn f_anchor_blocks_shell_in_repo() {
        let r = repo();
        let c = Candidate {
            exe: Some("/bin/zsh".into()),
            cwd: Some(r.clone()),
            cmd: vec!["zsh".into()],
            ..cand(999, Some(1))
        };
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false).is_none());
    }

    #[test]
    fn e2_plus_f_passes() {
        let r = repo();
        let toolkit_exe = r.join("toolkit/bin/python");
        let c = Candidate {
            exe: Some(toolkit_exe),
            cwd: Some(r.clone()),
            environ: vec!["ALAS_LAUNCHER_PID=123".into()],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
            ..cand(100, Some(10))
        };
        assert_eq!(
            is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false),
            Some(EvidenceKind::E2)
        );
    }

    #[test]
    fn x3_veto_overrides_f_e() {
        let r = repo();
        let toolkit_exe = r.join("toolkit/bin/python");
        let c = Candidate {
            exe: Some(toolkit_exe),
            cwd: Some(r.clone()),
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
            ..cand(50, Some(10))
        };
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, None, &|p| p == 50).is_none());
    }

    // ---- Extended coverage: E1 / E3 / E4 / X1 / X2 / truncation ----

    #[test]
    fn e1_cmdline_mentions_repo_wins_first() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(r.clone()),
            environ: vec!["ALAS_LAUNCHER_PID=7".into()],
            cmd: vec!["python".into(), REPO.into(), "gui.py".into()],
            ..cand(101, Some(11))
        };
        // E1 outranks E3/E4 present on the same process.
        assert_eq!(
            is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false),
            Some(EvidenceKind::E1)
        );
    }

    #[test]
    fn e3_cwd_equals_repo_when_no_cmdline_evidence() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(r.clone()),
            cmd: vec!["python".into(), "-c".into(), "print(1)".into()],
            ..cand(102, Some(12))
        };
        assert_eq!(
            is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false),
            Some(EvidenceKind::E3)
        );
    }

    #[test]
    fn e4_environ_marker_alone_suffices() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(PathBuf::from("/elsewhere")),
            environ: vec!["PATH=/usr/bin".into(), "ALAS_LAUNCHER_PID=42".into()],
            cmd: vec!["python".into(), "-m".into(), "http.server".into()],
            ..cand(103, Some(13))
        };
        assert_eq!(
            is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false),
            Some(EvidenceKind::E4)
        );
    }

    #[test]
    fn x1_launcher_pid_never_killed() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(r.clone()),
            environ: vec!["ALAS_LAUNCHER_PID=1".into()],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
            ..cand(77, Some(7))
        };
        // pid == launcher_pid vetoes despite full F+E evidence.
        assert!(is_stale_candidate(&c, Some(&r), PORT, 77, Some(7), &|_| false).is_none());
    }

    #[test]
    fn x2_same_pgid_never_killed() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(r.clone()),
            environ: vec!["ALAS_LAUNCHER_PID=1".into()],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
            ..cand(78, Some(5))
        };
        // pgid == launcher_pgid vetoes despite full F+E evidence.
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, Some(5), &|_| false).is_none());
    }

    #[test]
    fn registered_pid_vetoed_even_with_evidence() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(r.clone()),
            environ: vec!["ALAS_LAUNCHER_PID=1".into()],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
            ..cand(300, Some(30))
        };
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, None, &|p| p == 300).is_none());
    }

    #[test]
    fn truncated_or_detached_port_args_do_not_match_e2() {
        let r = repo();
        // "--port=22267" glued form and detached-but-wrong value must NOT fire
        // E2; with no other evidence the process is spared.
        let glued = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(PathBuf::from("/elsewhere")),
            cmd: vec!["python".into(), "gui.py".into(), "--port=22267".into()],
            ..cand(104, Some(14))
        };
        assert!(is_stale_candidate(&glued, Some(&r), PORT, 1, None, &|_| false).is_none());

        let wrong_value = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(PathBuf::from("/elsewhere")),
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "9999".into(),
            ],
            ..cand(105, Some(15))
        };
        assert!(is_stale_candidate(&wrong_value, Some(&r), PORT, 1, None, &|_| false).is_none());

        // Truncated argv: "--port" present but its value cut off by the
        // snapshotter — adjacent-pair check finds nothing.
        let truncated = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(PathBuf::from("/elsewhere")),
            cmd: vec!["python".into(), "gui.py".into(), "--port".into()],
            ..cand(106, Some(16))
        };
        assert!(is_stale_candidate(&truncated, Some(&r), PORT, 1, None, &|_| false).is_none());
    }

    #[test]
    fn degraded_repo_mode_keeps_only_strong_e2() {
        // No repo: toolkit-exe F anchor unverifiable, but bare gui.py + port
        // anywhere in argv still fires E2.
        let c = Candidate {
            exe: Some(PathBuf::from("/opt/other/python")),
            cmd: vec!["python".into(), "-x", "gui.py", "22267"].iter().map(OsString::from).collect(),
            ..cand(107, Some(17))
        };
        assert_eq!(
            is_stale_candidate(&c, None, PORT, 1, None, &|_| false),
            Some(EvidenceKind::E2)
        );

        // Same relaxed shape must NOT fire when a repo IS known (F anchor
        // fails: exe outside toolkit) — degraded relaxation never leaks into
        // anchored mode.
        let r = repo();
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false).is_none());

        // Degraded mode still respects X vetoes.
        let own = Candidate {
            exe: Some(PathBuf::from("/opt/other/python")),
            cmd: vec!["python".into(), "gui.py".into(), "22267".into()],
            ..cand(1, None)
        };
        assert!(is_stale_candidate(&own, None, PORT, 1, None, &|_| false).is_none());
    }

    #[test]
    fn no_evidence_toolkit_exe_without_markers_is_spared() {
        let r = repo();
        let c = Candidate {
            exe: Some(r.join("toolkit/bin/python")),
            cwd: Some(PathBuf::from("/home/user")),
            cmd: vec!["python".into(), "repl".into()],
            ..cand(108, Some(18))
        };
        assert!(is_stale_candidate(&c, Some(&r), PORT, 1, None, &|_| false).is_none());
    }

    // ---- T2: discovery seam (collect_raw_candidates) ----

    #[test]
    fn collect_with_port_owner_none_uses_lb_lc_only() {
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let cand = Candidate {
            pid: 100,
            pgid: Some(10),
            exe: Some(repo.join("toolkit/bin/python")),
            cwd: Some(repo.clone()),
            environ: vec![],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
        };
        let raw = collect_raw_candidates(22267, Some(&repo), &|| vec![cand.clone()], &|_| None);
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].pid, 100);
    }

    // ---- T4b: spoofed cmdline cannot fake identity without the F anchor ----

    #[test]
    fn t4b_spoofed_cmdline_identity_via_injection() {
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let toolkit_exe = repo.join("toolkit/bin/python");
        // exe inside toolkit + spoofed gui.py --port passes F+E
        let cand_ok = Candidate {
            pid: 1,
            pgid: Some(1),
            exe: Some(toolkit_exe.clone()),
            cwd: Some(repo.clone()),
            environ: vec![],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
        };
        assert!(is_stale_candidate(&cand_ok, Some(&repo), 22267, 99, None, &|_| false).is_some());
        // exe outside toolkit with same cmd fails F
        let cand_bad = Candidate {
            pid: 2,
            pgid: Some(2),
            exe: Some("/usr/bin/python3".into()),
            cwd: Some(repo.clone()),
            environ: vec![],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
        };
        assert!(is_stale_candidate(&cand_bad, Some(&repo), 22267, 99, None, &|_| false).is_none());
    }

    // ---- T3: convergence loop (kill_stale_alas_with_injection) ----

    fn lb_candidate(pid: u32, pgid: u32, exe: PathBuf) -> Candidate {
        Candidate {
            pid,
            pgid: Some(pgid),
            exe: Some(exe),
            cwd: None,
            environ: vec![],
            cmd: vec![
                "python".into(),
                "gui.py".into(),
                "--port".into(),
                "22267".into(),
            ],
        }
    }

    #[test]
    fn raw_nonempty_all_filtered_no_port_owner_immediate_ok() {
        // enumerate returns one candidate that fails F (exe outside toolkit),
        // port_owner returns None → zero kill calls, Ok, no progress rounds.
        let repo = PathBuf::from("/tmp/fake");
        let mut kill_calls = 0;
        let res = kill_stale_alas_with_injection(
            22267,
            &|_s| {},
            Some(repo.as_path()),
            &|| {
                vec![Candidate {
                    pid: 10,
                    pgid: Some(10),
                    exe: Some("/bin/zsh".into()),
                    cwd: Some(repo.clone()),
                    environ: vec![],
                    cmd: vec!["zsh".into()],
                }]
            },
            &|_| None,
            &|_| false,
            &mut |_| {
                kill_calls += 1;
                Ok(())
            },
            10,
            Duration::from_millis(0),
        );
        assert!(res.is_ok());
        assert_eq!(kill_calls, 0);
    }

    #[test]
    fn first_round_empty_returns_ok_without_kills() {
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let enum_calls = std::cell::Cell::new(0);
        let mut kill_calls = 0;
        let progress_calls = std::cell::Cell::new(0);
        let res = kill_stale_alas_with_injection(
            22267,
            &|_| progress_calls.set(progress_calls.get() + 1),
            Some(repo.as_path()),
            &|| {
                enum_calls.set(enum_calls.get() + 1);
                vec![]
            },
            &|_| None,
            &|_| false,
            &mut |_| {
                kill_calls += 1;
                Ok(())
            },
            10,
            Duration::from_millis(0),
        );
        assert!(res.is_ok());
        assert_eq!(enum_calls.get(), 1); // early-Ok before port-owner probe re-enumeration
        assert_eq!(kill_calls, 0);
        assert_eq!(progress_calls.get(), 0);
    }

    #[test]
    fn ten_round_survivors_x_veto_reports_survivors_error() {
        // Registered pid owning the port: X3 veto each round, never killed,
        // never early-Ok → Survivors(payload, 10) after max_rounds.
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let mut kill_calls = 0;
        let res = kill_stale_alas_with_injection(
            22267,
            &|_| {},
            Some(repo.as_path()),
            &|| {
                vec![Candidate {
                    pid: 300,
                    pgid: Some(30),
                    exe: Some(repo.join("toolkit/bin/python")),
                    cwd: Some(repo.clone()),
                    environ: vec!["ALAS_LAUNCHER_PID=1".into()],
                    cmd: vec![
                        "python".into(),
                        "gui.py".into(),
                        "--port".into(),
                        "22267".into(),
                    ],
                }]
            },
            &|_| Some(300),
            &|p| p == 300,
            &mut |_| {
                kill_calls += 1;
                Ok(())
            },
            10,
            Duration::from_millis(0),
        );
        match res {
            Err(StaleCleanupError::Survivors(v, rounds)) => {
                assert_eq!(rounds, 10);
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].pid, 300);
                assert!(v[0].evidence.contains("X3 registered"));
            }
            other => panic!("expected Survivors, got {:?}", other.err().map(|e| e.to_string())),
        }
        assert_eq!(kill_calls, 0); // X-vetoed pids are never killed
    }

    #[test]
    fn foreign_port_owner_zero_kill() {
        // Port owner passes X but fails F+E (exe outside toolkit) → refuse
        // with ForeignPortOwner, never kill anything.
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let mut kill_calls = 0;
        let res = kill_stale_alas_with_injection(
            22267,
            &|_| {},
            Some(repo.as_path()),
            &|| {
                vec![lb_candidate(500, 50, "/usr/bin/python3".into())]
            },
            &|_| Some(500),
            &|_| false,
            &mut |_| {
                kill_calls += 1;
                Ok(())
            },
            10,
            Duration::from_millis(0),
        );
        match res {
            Err(StaleCleanupError::ForeignPortOwner { port, pid, cmdline }) => {
                assert_eq!(port, 22267);
                assert_eq!(pid, 500);
                assert!(cmdline.contains("gui.py"));
                assert!(cmdline.chars().count() <= 200);
            }
            other => panic!("expected ForeignPortOwner, got {:?}", other.err().map(|e| e.to_string())),
        }
        assert_eq!(kill_calls, 0);
    }

    #[test]
    fn kill_err_retries_until_max_rounds_then_survivors() {
        // Verified stale candidate whose kill always fails: retried every
        // round, Survivors error after 10 rounds with 10 kill attempts.
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let mut kill_calls = 0;
        let res = kill_stale_alas_with_injection(
            22267,
            &|_| {},
            Some(repo.as_path()),
            &|| {
                vec![lb_candidate(600, 60, repo.join("toolkit/bin/python"))]
            },
            &|_| None,
            &|_| false,
            &mut |_| {
                kill_calls += 1;
                Err(std::io::Error::other("denied"))
            },
            10,
            Duration::from_millis(0),
        );
        match res {
            Err(StaleCleanupError::Survivors(v, rounds)) => {
                assert_eq!(rounds, 10);
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].pid, 600);
                assert_eq!(v[0].evidence, "verified survivor");
            }
            other => panic!("expected Survivors, got {:?}", other.err().map(|e| e.to_string())),
        }
        assert_eq!(kill_calls, 10);
    }

    #[test]
    fn second_round_vanish_returns_ok() {
        // Round 1 kills a verified candidate; round 2 sees an empty table →
        // early-Ok without a Survivors payload.
        let repo = PathBuf::from("/tmp/fake/AzurLaneAutoScript");
        let round = std::cell::Cell::new(0);
        let mut kill_calls = 0;
        let res = kill_stale_alas_with_injection(
            22267,
            &|_| {},
            Some(repo.as_path()),
            &|| {
                round.set(round.get() + 1);
                if round.get() == 1 {
                    vec![lb_candidate(700, 70, repo.join("toolkit/bin/python"))]
                } else {
                    vec![]
                }
            },
            &|_| None,
            &|_| false,
            &mut |_| {
                kill_calls += 1;
                Ok(())
            },
            10,
            Duration::from_millis(0),
        );
        assert!(res.is_ok());
        assert_eq!(kill_calls, 1);
    }

    // ---- T4: truncation + error Display/anyhow contract (FR1.5) ----

    #[test]
    fn cmdline_truncated_to_200_chars() {
        let long = "x".repeat(500);
        let info = StaleProcessInfo {
            pid: 1,
            cmdline: truncate_cmd(&long),
            evidence: "E2".into(),
        };
        assert!(info.cmdline.chars().count() <= 200);
        assert_eq!(info.cmdline.chars().count(), 200);

        let short = "gui.py --port 22267";
        assert_eq!(truncate_cmd(short), short);

        let multibyte = "舰".repeat(300);
        assert_eq!(truncate_cmd(&multibyte).chars().count(), 200);
    }

    #[test]
    fn stale_cleanup_error_display_and_anyhow_conversion() {
        let survivors = StaleCleanupError::Survivors(
            vec![StaleProcessInfo {
                pid: 9,
                cmdline: "python gui.py".into(),
                evidence: "E4".into(),
            }],
            10,
        );
        assert!(survivors
            .to_string()
            .contains("Failed to clean up stale ALAS processes after 10 attempts"));
        assert!(survivors.to_string().contains("pid 9"));

        let foreign = StaleCleanupError::ForeignPortOwner {
            port: 22267,
            pid: 5,
            cmdline: "nginx".into(),
        };
        assert!(foreign.to_string().contains("Port 22267 is occupied"));
        assert!(foreign.to_string().contains("pid 5"));

        fn takes_anyhow(e: impl Into<anyhow::Error>) -> String {
            e.into().to_string()
        }
        assert!(takes_anyhow(foreign).contains("nginx"));
    }
}
