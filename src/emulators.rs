//! Android-emulator fleet control (direction-1 minimal loop): AVD inventory,
//! liveness, MaaEnd-aware mutex, manual start/stop and the conditional
//! self-heal poller.
//!
//! Ownership contract (2026-10-28 grilling, decision 1): the LAUNCHER is the
//! sole executor of emulator start/stop. ~/bin/azurelane-watchdog.sh stays a
//! passive crash recorder and never launches — see its own header.
//!
//! Detection model (mirrors the watchdog's proven approach, one `ps` pass):
//! - `ps axww -o command=` → running qemu per `-avd <name>` token (+ optional
//!   `-port N`), and MaaEnd presence via a `/Applications/MaaEnd.app/` PATH
//!   prefix match (main binary + helpers all live under that prefix; our own
//!   `ps` invocation is spawned without a shell, so it can never self-match).
//! - `adb devices` + `adb -s <serial> emu avd name` → authoritative
//!   serial↔AVD mapping (works for Studio auto-assigned ports too).
//!
//! The mutex rule (crash 九/十号, "MaaEnd 扫描开 vid probe 杀 AVD") lives in
//! [`start_block_reason`] — a PURE function, single source of truth for both
//! the manual path and the self-heal poller: while MaaEnd runs, every AVD is
//! blocked except the exempt list (Endfield survives the vid probe).
//!
//! The three control tiers (settings.json `emulator_control`, default
//! `manual`) escalate strictly: readonly ⊂ manual ⊂ selfheal. They are ONE
//! enum — not independent switches — so the dangerous combination
//! "mutex off + auto-revive on" cannot be selected by construction.
//!
//! Cross-platform hygiene: the module compiles everywhere; `ps`-based
//! detection is unix-only (`#[cfg(unix)]`), Windows falls back to adb-only
//! status and refuses start/stop with `unsupported`.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{info, warn};

use crate::shell_settings::{self, ControlTier};

/// Fresh tier read (the settings file is the source of truth — the menu
/// persists every tier click immediately).
fn current_tier() -> ControlTier {
    shell_settings::load().emulator_control
}

/// AVDs that may run while MaaEnd is present (crash 九/十号 verdict: Endfield
/// survives the AVDExtras vid probe, Azurelane/Arknight do not).
const MAAEND_EXEMPT: [&str; 1] = ["Endfield"];

/// Fleet port convention (console ports; adb follows at port+1): Azurelane
/// 5554, Arknight 5556, Endfield 5558 — matches the launch scripts' fixed
/// targets so launcher-started emulators land on the same serials as manual
/// ones. Unknown AVDs start with an emulator-assigned port (None here).
fn pinned_port(name: &str) -> Option<u16> {
    match name {
        "Azurelane" => Some(5554),
        "Arknight" => Some(5556),
        "Endfield" => Some(5558),
        _ => None,
    }
}

/// How long an emulator must have been alive before a self-heal revive is
/// allowed. A death inside this window marks the AVD crash-loop-halted (the
/// 四号 AGX 七连发 lesson: never auto-revive into a crash wave).
const SELF_HEAL_MIN_UPTIME: Duration = Duration::from_secs(300);
const SELF_HEAL_POLL: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Pure decision core (unit-tested)
// ---------------------------------------------------------------------------

/// Why a manual start of `name` is refused; `None` = allowed. Pure.
pub(crate) fn start_block_reason(
    tier: ControlTier,
    name: &str,
    maaend_present: bool,
) -> Option<&'static str> {
    match tier {
        ControlTier::ReadOnly => Some("read-only"),
        ControlTier::ManualGuarded | ControlTier::SelfHeal => {
            if maaend_present && !MAAEND_EXEMPT.contains(&name) {
                Some("maaend-mutex")
            } else {
                None
            }
        }
    }
}

/// Self-heal revive decision for a running→dead transition. Pure.
///
/// `uptime_before_death`: how long the emulator had been alive. Fast deaths
/// (below [`SELF_HEAL_MIN_UPTIME`]) mark the AVD halted — only a manual start
/// clears the halt (see [`manual_start`]); the poller logs and stays passive.
pub(crate) enum ReviveDecision {
    /// Allowed: tier + mutex + uptime all satisfied.
    Revive,
    /// Not our call this cycle (tier below selfheal / MaaEnd present). The
    /// reason is not logged — the poller would spam it every cycle.
    Skip,
    /// Second fast death — halt auto-revive for this AVD until manual start.
    Halt,
}

pub(crate) fn revive_decision(
    tier: ControlTier,
    maaend_present: bool,
    uptime_before_death: Duration,
) -> ReviveDecision {
    if tier != ControlTier::SelfHeal {
        return ReviveDecision::Skip;
    }
    if maaend_present {
        return ReviveDecision::Skip;
    }
    if uptime_before_death < SELF_HEAL_MIN_UPTIME {
        return ReviveDecision::Halt;
    }
    ReviveDecision::Revive
}

/// Parse one `ps axww -o command=` line into (avd name, optional `-port`).
/// Only argv[0] (the first token — the emulator binary's absolute path) may
/// identify a qemu process, so `grep qemu-system -avd X` style probe lines
/// never false-positive. Returns `None` for non-qemu lines. Pure.
pub(crate) fn parse_qemu_line(line: &str) -> Option<(String, Option<u16>)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if !tokens.first().is_some_and(|first| first.contains("qemu-system")) {
        return None;
    }
    let mut name = None;
    let mut port = None;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "-avd" => {
                name = tokens.get(i + 1).map(|s| s.to_string());
                i += 2;
            }
            "-port" => {
                port = tokens.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }
    name.map(|name| (name, port))
}

/// MaaEnd presence predicate for a `ps` command line. Path-prefix match on
/// the app bundle: the main binary AND its Helper processes live under
/// `/Applications/MaaEnd.app/`, while any of OUR probe commands (spawned
/// shell-free) never start with that path. Pure.
pub(crate) fn is_maaend_line(line: &str) -> bool {
    line.starts_with("/Applications/MaaEnd.app/")
}

/// Parse `adb devices` output → emulator serials in `device` state. Pure.
pub(crate) fn parse_adb_devices(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|l| !l.contains("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?;
            let state = it.next()?;
            (state == "device" && serial.starts_with("emulator-"))
                .then(|| serial.to_string())
        })
        .collect()
}

/// Parse `adb -s <serial> emu avd name` output → the AVD name. Format (live
/// 2026-10-28): first line name, second line `OK`; auth failures come back as
/// `KO: ...`. Pure.
pub(crate) fn parse_emu_avd_name(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "OK" && !l.starts_with("KO:"))
        .map(|l| l.trim_start_matches("avd name:").trim().to_string())
        .filter(|l| !l.is_empty())
}

// ---------------------------------------------------------------------------
// Environment probing
// ---------------------------------------------------------------------------

/// Run a command with a hard timeout; `None` = spawn failure or timeout
/// (killed). Stdout only; stderr discarded — these are read-only probes.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    Some(out)
}

/// Android SDK root: ANDROID_HOME → ANDROID_SDK_ROOT → the macOS default.
/// GUI launches carry no Homebrew PATH (alas-gui-launch-adb-path-trap), so
/// the fallback must be absolute, never PATH-dependent.
fn sdk_root() -> std::path::PathBuf {
    std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(
                std::env::var_os("HOME").unwrap_or_default().to_string_lossy().as_ref(),
            )
            .join("Library/Android/sdk")
        })
}

fn adb_path() -> std::path::PathBuf {
    sdk_root().join("platform-tools/adb")
}

fn emulator_path() -> std::path::PathBuf {
    sdk_root().join("emulator/emulator")
}

/// AVD home: ANDROID_AVD_HOME override → ~/.android/avd.
fn avd_home() -> std::path::PathBuf {
    std::env::var_os("ANDROID_AVD_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(
                std::env::var_os("HOME").unwrap_or_default().to_string_lossy().as_ref(),
            )
            .join(".android/avd")
        })
}

/// On-disk AVD inventory from `<avd_home>/*.ini` (file `Azurelane.ini` ↔ AVD
/// `Azurelane`), sorted for stable panel order.
fn avd_inventory() -> Vec<String> {
    let dir = avd_home();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            name.strip_suffix(".ini").map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// One `ps axww -o command=` pass → (all command lines). Unix-only; the
/// Windows build relies on adb-only detection.
#[cfg(unix)]
fn ps_lines() -> Vec<String> {
    run_with_timeout("ps", &["axww", "-o", "command="], Duration::from_secs(5))
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Running qemu processes keyed by AVD name → optional console port.
fn running_qemus() -> BTreeMap<String, Option<u16>> {
    let mut map = BTreeMap::new();
    #[cfg(unix)]
    for line in ps_lines() {
        if let Some((name, port)) = parse_qemu_line(&line) {
            map.insert(name, port);
        }
    }
    map
}

/// MaaEnd presence. Unix-only ps scan; unknown (false) on other platforms.
fn maaend_present() -> bool {
    #[cfg(unix)]
    {
        ps_lines().iter().any(|l| is_maaend_line(l))
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// serial↔AVD mapping via the emulator consoles (`emu avd name`, ~30ms each).
/// AVD name → serial. Empty when adb is unavailable.
fn serials_by_avd() -> BTreeMap<String, String> {
    let adb = adb_path();
    let Some(output) = run_with_timeout(&adb.to_string_lossy(), &["devices"], Duration::from_secs(5))
    else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    for serial in parse_adb_devices(&output) {
        let out = run_with_timeout(
            &adb.to_string_lossy(),
            &["-s", &serial, "emu", "avd", "name"],
            Duration::from_secs(5),
        );
        if let Some(name) = out.as_deref().and_then(parse_emu_avd_name) {
            map.insert(name, serial);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Fleet snapshot (the emu_list command surface)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvdStatus {
    pub name: String,
    pub running: bool,
    /// adb serial (e.g. `emulator-5554`) when resolvable.
    pub serial: Option<String>,
    /// Fleet console-port convention for this AVD, if any.
    pub pinned_port: Option<u16>,
    /// Block code for a manual start: `read-only` | `maaend-mutex`; the
    /// webapp maps codes to localized text (Rust stays i18n-free).
    pub blocked: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetStatus {
    /// Current tier id: `readonly` | `manual` | `selfheal`.
    pub tier: &'static str,
    pub maaend_present: bool,
    pub avds: Vec<AvdStatus>,
}

/// Probe everything and assemble the panel payload. Tier comes from the
/// settings FILE (fresh read — the menu persists every change immediately).
pub fn fleet_snapshot() -> FleetStatus {
    let tier = current_tier();
    let maaend = maaend_present();
    let running = running_qemus();
    let serials = serials_by_avd();
    let avds = avd_inventory()
        .into_iter()
        .map(|name| {
            let qemu = running.get(&name);
            let running_flag = qemu.is_some() || serials.contains_key(&name);
            AvdStatus {
                name: name.clone(),
                running: running_flag,
                serial: serials.get(&name).cloned().or_else(|| {
                    // Fall back to the pinned/console port when the qemu
                    // cmdline carried it (-port explicit).
                    qemu.copied().flatten().map(|p| format!("emulator-{p}"))
                }),
                pinned_port: pinned_port(&name),
                blocked: start_block_reason(tier, &name, maaend),
            }
        })
        .collect();
    FleetStatus {
        tier: tier.as_str(),
        maaend_present: maaend,
        avds,
    }
}

// ---------------------------------------------------------------------------
// Start / stop (launcher = sole executor; every path re-checks the mutex)
// ---------------------------------------------------------------------------

/// Serialize concurrent start/stop dispatches (double-click guard).
static OP_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The launch recipe mirrors the observed Studio/manual launch of this fleet
/// (windowed, no snapshot storage, full network). `-no-window` would drop
/// GuestAngle without `-feature GuestAngle` (android-avd-gl-shim-crashes) —
/// deliberately NOT headless.
fn spawn_emulator(name: &str) -> Result<(), String> {
    if !avd_inventory().iter().any(|n| n == name) {
        return Err("no-such-avd".into());
    }
    let emulator = emulator_path();
    if !emulator.exists() {
        return Err("emulator-binary-missing".into());
    }
    let mut args: Vec<String> = vec![
        "-avd".into(),
        name.into(),
        "-netdelay".into(),
        "none".into(),
        "-netspeed".into(),
        "full".into(),
        "-no-snapstorage".into(),
    ];
    if let Some(port) = pinned_port(name) {
        args.push("-port".into());
        args.push(port.to_string());
    }
    let child = Command::new(&emulator)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            warn!("emulator spawn failed for {name}: {e}");
            "spawn-failed"
        })?;
    // Detached by intent: the emulator outlives the launcher (same contract
    // as manual launches); drop the handle without waiting.
    info!("emulator spawned for {name}: pid {}", child.id());
    Ok(())
}

/// Manual start from the webapp panel. Enforces the tier + mutex rule at
/// click time (the decision, not a cached snapshot). A successful manual
/// start also clears any self-heal crash-loop halt for that AVD.
pub fn manual_start(name: &str) -> Result<(), String> {
    if !OP_IN_FLIGHT
        .compare_exchange(false, true, std::sync::atomic::Ordering::Acquire, std::sync::atomic::Ordering::Relaxed)
        .is_ok()
    {
        return Err("busy".into());
    }
    let _guard = PermitRelease;
    let tier = current_tier();
    if let Some(reason) = start_block_reason(tier, name, maaend_present()) {
        return Err(reason.into());
    }
    if running_qemus().contains_key(name) || serials_by_avd().contains_key(name) {
        return Err("already-running".into());
    }
    spawn_emulator(name)?;
    clear_halt(name);
    Ok(())
}

/// Manual stop: graceful console shutdown (`emu kill`), never a signal.
pub fn manual_stop(name: &str) -> Result<(), String> {
    if !OP_IN_FLIGHT
        .compare_exchange(false, true, std::sync::atomic::Ordering::Acquire, std::sync::atomic::Ordering::Relaxed)
        .is_ok()
    {
        return Err("busy".into());
    }
    let _guard = PermitRelease;
    let Some(serial) = serials_by_avd().get(name).cloned() else {
        return Err("not-running".into());
    };
    let adb = adb_path();
    run_with_timeout(
        &adb.to_string_lossy(),
        &["-s", &serial, "emu", "kill"],
        Duration::from_secs(10),
    )
    .ok_or("adb-failed")?;
    // A purposeful stop must not be revived by the self-heal poller: the
    // transition will look like a fast death if it was young, so record it
    // as a halt until the next manual start clears it.
    set_halt(name);
    Ok(())
}

/// RAII release for [`OP_IN_FLIGHT`] (mirrors operation.rs's InFlightGuard
/// semantics: release on every early return).
struct PermitRelease;
impl Drop for PermitRelease {
    fn drop(&mut self) {
        OP_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Conditional self-heal poller (tier selfheal; default off)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HealTrack {
    /// First poll that saw this AVD running (uptime anchor).
    running_since: Option<Instant>,
    /// Crash-loop halt; cleared only by a manual start.
    halted: bool,
}

fn heal_state() -> &'static std::sync::Mutex<BTreeMap<String, HealTrack>> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, HealTrack>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

fn clear_halt(name: &str) {
    if let Some(track) = heal_state().lock().unwrap().get_mut(name) {
        track.halted = false;
    }
}

fn set_halt(name: &str) {
    heal_state()
        .lock()
        .unwrap()
        .entry(name.to_string())
        .or_default()
        .halted = true;
}

/// Background thread: poll the fleet, revive dead AVDs on the selfheal tier
/// with the crash-loop guard. Owns no permits (emulator ops are independent
/// of backend lifecycle ops) and reads the tier fresh each cycle, so a menu
/// switch takes effect within one poll.
pub fn spawn_self_heal_poller() {
    std::thread::Builder::new()
        .name("emu-self-heal".into())
        .spawn(|| {
            info!(
                "emulator self-heal poller started ({}s cycle)",
                SELF_HEAL_POLL.as_secs()
            );
            loop {
                std::thread::sleep(SELF_HEAL_POLL);
                let tier = current_tier();
                let maaend = maaend_present();
                let running = running_qemus();
                let serials = serials_by_avd();
                enum Action {
                    Revive,
                    LogHalt(Duration),
                    LogHalted,
                }
                // Decide under the lock, act after releasing it — spawn_emulator
                // takes its time and manual_start's clear_halt must not queue
                // behind a spawn held under the state lock.
                let mut actions: Vec<(String, Action)> = Vec::new();
                {
                    let mut state = heal_state().lock().unwrap();
                    for name in avd_inventory() {
                        let is_running = running.contains_key(&name) || serials.contains_key(&name);
                        let track = state.entry(name.clone()).or_default();
                        if is_running {
                            if track.running_since.is_none() {
                                // Fresh sighting: a human (or a surviving
                                // revive) started this AVD — clear any stale
                                // crash-loop halt so the guard tracks the NEW
                                // run, not an old wave.
                                track.running_since = Some(Instant::now());
                                track.halted = false;
                            }
                            continue;
                        }
                        let Some(since) = track.running_since.take() else {
                            continue; // was already down; nothing to revive
                        };
                        let uptime = since.elapsed();
                        if track.halted {
                            actions.push((name, Action::LogHalted));
                            continue;
                        }
                        match revive_decision(tier, maaend, uptime) {
                            ReviveDecision::Revive => actions.push((name, Action::Revive)),
                            // Parked on purpose (tier below selfheal / MaaEnd
                            // present): the uptime anchor is already consumed,
                            // so re-entering the tier cannot revive a long-dead
                            // AVD by accident.
                            ReviveDecision::Skip => {}
                            ReviveDecision::Halt => {
                                track.halted = true;
                                actions.push((name, Action::LogHalt(uptime)));
                            }
                        }
                    }
                }
                for (name, action) in actions {
                    match action {
                        Action::Revive => {
                            info!("self-heal: {name} died — reviving");
                            if let Err(e) = spawn_emulator(&name) {
                                warn!("self-heal: revive failed for {name}: {e}");
                            }
                            heal_state()
                                .lock()
                                .unwrap()
                                .entry(name)
                                .or_default()
                                .running_since = Some(Instant::now());
                        }
                        Action::LogHalt(uptime) => {
                            warn!(
                                "self-heal: {name} died after only {uptime:?} (< {:?}) — \
                                 crash-loop halt, manual start required",
                                SELF_HEAL_MIN_UPTIME
                            );
                        }
                        Action::LogHalted => {
                            info!("self-heal: {name} down (crash-loop halt active) — staying passive");
                        }
                    }
                }
            }
        })
        .expect("spawn emu-self-heal thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier_json(tier: ControlTier) -> String {
        serde_json::to_string(&tier).unwrap()
    }

    #[test]
    fn block_reason_matrix() {
        // Manual tier: only the MaaEnd mutex blocks, exempt AVD passes.
        assert_eq!(start_block_reason(ControlTier::ManualGuarded, "Azurelane", false), None);
        assert_eq!(start_block_reason(ControlTier::ManualGuarded, "Azurelane", true), Some("maaend-mutex"));
        assert_eq!(start_block_reason(ControlTier::ManualGuarded, "Arknight", true), Some("maaend-mutex"));
        assert_eq!(start_block_reason(ControlTier::ManualGuarded, "Endfield", true), None);
        // Self-heal tier shares the same manual-start rule.
        assert_eq!(start_block_reason(ControlTier::SelfHeal, "Azurelane", true), Some("maaend-mutex"));
        // Read-only blocks everything, even the exempt AVD.
        assert_eq!(start_block_reason(ControlTier::ReadOnly, "Endfield", false), Some("read-only"));
    }

    #[test]
    fn revive_decision_gates() {
        let long = SELF_HEAL_MIN_UPTIME + Duration::from_secs(1);
        let short = SELF_HEAL_MIN_UPTIME - Duration::from_secs(1);
        assert!(matches!(
            revive_decision(ControlTier::SelfHeal, false, long),
            ReviveDecision::Revive
        ));
        assert!(matches!(
            revive_decision(ControlTier::ManualGuarded, false, long),
            ReviveDecision::Skip
        ));
        assert!(matches!(
            revive_decision(ControlTier::SelfHeal, true, long),
            ReviveDecision::Skip
        ));
        assert!(matches!(
            revive_decision(ControlTier::SelfHeal, false, short),
            ReviveDecision::Halt
        ));
    }

    #[test]
    fn parse_qemu_line_matrix() {
        let line = "/sdk/qemu/darwin-aarch64/qemu-system-aarch64 -netdelay none -netspeed full -no-snapstorage -avd Azurelane";
        assert_eq!(parse_qemu_line(line), Some(("Azurelane".into(), None)));
        let with_port = "/x/qemu-system-aarch64 -avd Arknight -port 5556 -no-snapstorage";
        assert_eq!(parse_qemu_line(with_port), Some(("Arknight".into(), Some(5556))));
        assert_eq!(parse_qemu_line("/usr/sbin/sshd -D"), None);
        assert_eq!(parse_qemu_line("grep qemu-system -avd X"), None);
    }

    #[test]
    fn maaend_line_requires_bundle_path_prefix() {
        assert!(is_maaend_line("/Applications/MaaEnd.app/Contents/MacOS/MaaEnd"));
        assert!(is_maaend_line(
            "/Applications/MaaEnd.app/Contents/Frameworks/MaaEnd Helper (GPU).app/Contents/MacOS/x"
        ));
        // Our own probe commands (shell-free) never start with the bundle path.
        assert!(!is_maaend_line("ps axww -o command="));
        assert!(!is_maaend_line("/bin/sh -c ps axww MaaEnd.app"));
        // A similarly-named app must not match.
        assert!(!is_maaend_line("/Applications/MaaX.app/Contents/MacOS/MaaX"));
    }

    #[test]
    fn adb_devices_parse() {
        let out = "List of devices attached\nemulator-5554\tdevice\nemulator-5556\toffline\n127.0.0.1:5555\tdevice\n\n";
        assert_eq!(parse_adb_devices(out), vec!["emulator-5554".to_string()]);
        assert_eq!(parse_adb_devices(""), Vec::<String>::new());
    }

    #[test]
    fn emu_avd_name_parse() {
        assert_eq!(parse_emu_avd_name("Azurelane\nOK\n"), Some("Azurelane".into()));
        assert_eq!(parse_emu_avd_name("OK\n"), None);
        assert_eq!(parse_emu_avd_name("KO: authentication failed\n"), None);
    }

    #[test]
    fn pinned_ports_follow_fleet_convention() {
        assert_eq!(pinned_port("Azurelane"), Some(5554));
        assert_eq!(pinned_port("Arknight"), Some(5556));
        assert_eq!(pinned_port("Endfield"), Some(5558));
        assert_eq!(pinned_port("SomeOtherAvd"), None);
    }

    #[test]
    fn control_tier_serde_ids() {
        assert_eq!(tier_json(ControlTier::ReadOnly), "\"readonly\"");
        assert_eq!(tier_json(ControlTier::ManualGuarded), "\"manual\"");
        assert_eq!(tier_json(ControlTier::SelfHeal), "\"selfheal\"");
        assert_eq!(ControlTier::default(), ControlTier::ManualGuarded);
        assert_eq!(ControlTier::ReadOnly.as_str(), "readonly");
    }

    /// LIVE smoke (run explicitly: `cargo test -- --ignored`): exercises the
    /// real detection stack — ps + adb devices + `emu avd name` — read-only,
    /// no side effects. Expect: the on-disk fleet (Azurelane/Arknight/Endfield)
    /// all listed; a running emulator carries an emulator-XXXX serial.
    #[test]
    #[ignore]
    fn live_fleet_snapshot_smoke() {
        let status = fleet_snapshot();
        println!(
            "tier={} maaend={} avds={:?}",
            status.tier, status.maaend_present, status.avds
        );
        assert!(
            status.avds.iter().any(|a| a.name == "Azurelane"),
            "Azurelane must be in the on-disk inventory: {:?}",
            status.avds
        );
        if let Some(azurelane) = status.avds.iter().find(|a| a.name == "Azurelane") {
            if azurelane.running {
                assert!(
                    azurelane.serial.as_deref().is_some_and(|s| s.starts_with("emulator-")),
                    "a running Azurelane must resolve to an emulator-* serial: {:?}",
                    azurelane
                );
            }
        }
    }
}
