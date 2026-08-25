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

/// Snapshot of one candidate process (filled by the discovery pass).
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
}
