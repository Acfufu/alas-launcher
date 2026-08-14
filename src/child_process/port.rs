////// Port-ownership probes (Momus ADVISORY-1): which process listens on
////// a port, per-platform, fail-open.

use std::process::Command;
use tracing::warn;

/// Pid of the process listening on `port`, if determinable.
///
/// Platform probe (Momus ADVISORY-1): macOS `lsof -tiTCP:<port>
/// -sTCP:LISTEN`, Linux `ss -ltnp`, Windows `netstat -ano`.
///
/// FAIL-OPEN: a missing tool or unparseable output returns None — the start
/// is NOT blocked. Acceptability: None only skips the port-ownership check
/// for that one start; `ManagedBackend`'s ALAS_LAUNCHER_PID residue scan
/// (Drop) still reaps same-launcher stale processes, so only a server
/// left behind by ANOTHER launcher instance slips through on that rare path —
/// and the lsof probe is near-universal on macOS. Fail-closed would break
/// startup entirely on machines without the tool, which is worse than the
/// hole it plugs.
pub fn port_owner_pid(port: u16) -> Option<u32> {
    let mut probe = probe_command(port);
    let output = probe.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    match parse_owner_pid(&text, port) {
        Some(pid) => Some(pid),
        None => {
            warn!(
                "port_owner_pid({port}): owner not found (tool unavailable or permission-restricted); skipping port-ownership check (fail-open)"
            );
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn probe_command(port: u16) -> Command {
    let mut cmd = Command::new("lsof");
    cmd.args([format!("-tiTCP:{port}"), "-sTCP:LISTEN".to_owned()]);
    cmd
}

#[cfg(target_os = "linux")]
fn probe_command(port: u16) -> Command {
    let mut cmd = Command::new("ss");
    cmd.args(["-ltnp", &format!("sport = :{port}")]);
    cmd
}

#[cfg(target_os = "windows")]
fn probe_command(port: u16) -> Command {
    let mut cmd = Command::new("netstat");
    cmd.args(["-ano"]);
    cmd
}

#[cfg(target_os = "macos")]
fn parse_owner_pid(out: &str, _port: u16) -> Option<u32> {
    parse_lsof_owner(out)
}

#[cfg(target_os = "linux")]
fn parse_owner_pid(out: &str, port: u16) -> Option<u32> {
    parse_ss_owner(out, port)
}

#[cfg(target_os = "windows")]
fn parse_owner_pid(out: &str, port: u16) -> Option<u32> {
    parse_netstat_owner(out, port)
}

/// lsof `-tiTCP:<port> -sTCP:LISTEN` output: one pid per line, nothing else.
/// First line wins.
#[cfg(any(test, target_os = "macos"))]
pub fn parse_lsof_owner(out: &str) -> Option<u32> {
    out.lines().next()?.trim().parse().ok()
}

/// ss `-ltnp` row: `LISTEN 0 128 127.0.0.1:22267 0.0.0.0:* users:(("python",pid=1234,fd=5))`.
/// Selects the row holding `:port` and reads `pid=N`.
#[cfg(any(test, target_os = "linux"))]
pub fn parse_ss_owner(out: &str, port: u16) -> Option<u32> {
    let needle = format!(":{port} ");
    out.lines()
        .find(|l| l.contains(&needle))
        .and_then(|l| l.split("pid=").nth(1))
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|pid| pid.parse().ok())
}

/// netstat `-ano` row: `TCP 127.0.0.1:22267 0.0.0.0:0 LISTENING 1234`.
/// Selects the LISTENING row holding `:port`; pid is the last column.
#[cfg(any(test, target_os = "windows"))]
pub fn parse_netstat_owner(out: &str, port: u16) -> Option<u32> {
    let needle = format!(":{port} ");
    out.lines()
        .find(|l| l.contains(&needle) && l.contains("LISTENING"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|pid| pid.parse().ok())
}

/// True when `pid` runs in the process group led by `leader` (the spawned
/// child) — i.e. the port listener is OUR backend, not a stale server from
/// another launcher instance.
pub fn is_same_process_group(pid: u32, leader: u32) -> bool {
    pid == leader || same_pgid(pid, leader)
}

#[cfg(unix)]
fn same_pgid(pid: u32, leader: u32) -> bool {
    use nix::unistd::{getpgid, Pid};
    match (
        getpgid(Some(Pid::from_raw(pid as i32))),
        getpgid(Some(Pid::from_raw(leader as i32))),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(windows)]
fn same_pgid(pid: u32, leader: u32) -> bool {
    // No queryable group id on Windows; approximate via the ancestor chain —
    // job membership is inherited through forks, so the listener is ours iff
    // it descends from the spawned leader.
    let sys = sysinfo::System::new_all();
    let mut current = pid;
    for _ in 0..64 {
        let Some(parent) = sys
            .process(sysinfo::Pid::from_u32(current))
            .and_then(|p| p.parent())
        else {
            return false;
        };
        if parent.as_u32() == leader {
            return true;
        }
        current = parent.as_u32();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use nix::unistd::{getpgid, Pid};

    #[test]
    fn parse_lsof_owner_lines() {
        assert_eq!(parse_lsof_owner("12345\n"), Some(12345));
        assert_eq!(parse_lsof_owner("12345\n67890\n"), Some(12345));
        assert_eq!(parse_lsof_owner(""), None);
        assert_eq!(parse_lsof_owner("garbage\n"), None);
    }

    #[test]
    fn parse_ss_owner_ports() {
        let line = "LISTEN 0 128 127.0.0.1:22267 0.0.0.0:* users:((\"python\",pid=12345,fd=5))";
        assert_eq!(parse_ss_owner(line, 22267), Some(12345));
        assert_eq!(parse_ss_owner(line, 22268), None);
        assert_eq!(parse_ss_owner("LISTEN 0 128 127.0.0.1:22267 0.0.0.0:*", 22267), None);
        assert_eq!(parse_ss_owner("", 22267), None);
    }

    #[test]
    fn parse_netstat_owner_rows() {
        let listening = "  TCP    127.0.0.1:22267    0.0.0.0:0    LISTENING    12345";
        assert_eq!(parse_netstat_owner(listening, 22267), Some(12345));
        let time_wait = "  TCP    127.0.0.1:22267    127.0.0.1:54321    TIME_WAIT    0";
        assert_eq!(parse_netstat_owner(time_wait, 22267), None);
        assert_eq!(parse_netstat_owner(listening, 22266), None);
        assert_eq!(parse_netstat_owner("", 22267), None);
    }

    #[test]
    fn is_same_process_group_membership() {
        let me = std::process::id();
        assert!(is_same_process_group(me, me));
        #[cfg(unix)]
        {
            // The test process is a member of its own group.
            let pgid = getpgid(Some(Pid::from_raw(me as i32))).unwrap();
            assert!(is_same_process_group(me, pgid.as_raw() as u32));
            // A pid that cannot exist belongs to nobody's group.
            assert!(!is_same_process_group(99_999_999, me));
        }
    }
}
