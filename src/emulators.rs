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

// ---------------------------------------------------------------------------
// Incident events (direction-2: crash/stuck → notification pipeline)
// ---------------------------------------------------------------------------

/// Emulator incident kinds surfaced to the notification pipeline. Detection
/// is passive and tier-independent; only crash HANDLING varies by tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmuIncident {
    /// AVD died and the current disposition will not auto-revive (tier below
    /// selfheal, or parked because MaaEnd is present) — manual pull needed.
    Died,
    /// AVD died and the self-heal poller relaunched it.
    CrashRevived,
    /// Death inside the crash-loop window — auto-revive halted until a
    /// manual start.
    CrashHalt,
    /// Serial present but `adb shell` unresponsive twice in a row (guest
    /// hang; the 2026-10-28 04:43 family — Watchdog soft-reboot follows).
    Stuck,
}

impl EmuIncident {
    /// Wording for the notification body `{incident}` placeholder.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Died => "crashed, no auto-revive (restart manually if unintended)",
            Self::CrashRevived => "crashed, auto-revived",
            Self::CrashHalt => "crash-loop halt, manual start required",
            Self::Stuck => "guest stuck (adb unresponsive)",
        }
    }
}

/// One incident for one AVD, handed to the sink the poller was spawned with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmuEvent {
    pub avd: String,
    pub incident: EmuIncident,
}

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
    mark_expected_stop(name);
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

/// Purposeful-stop markers: a death observed while a marker is fresh is a
/// user stop (manual_stop / window close handled via manual_stop only), not
/// a crash incident — consumed by the poller at the death sighting.
fn expected_stops() -> &'static std::sync::Mutex<BTreeMap<String, Instant>> {
    static STOPS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, Instant>>> =
        std::sync::OnceLock::new();
    STOPS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

fn mark_expected_stop(name: &str) {
    expected_stops()
        .lock()
        .unwrap()
        .insert(name.to_string(), Instant::now());
}

/// Whether a death of `name` right now was pre-announced by [`manual_stop`]
/// (consuming the marker). Stale markers (>60s) expire — the kill may have
/// failed and a later genuine crash must still notify.
fn consume_expected_stop(name: &str) -> bool {
    let mut stops = expected_stops().lock().unwrap();
    match stops.get(name) {
        Some(at) if at.elapsed() < Duration::from_secs(60) => {
            stops.remove(name);
            true
        }
        Some(_) => {
            stops.remove(name);
            false
        }
        None => false,
    }
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
    /// Consecutive failed adb-shell probes (stuck detection).
    stuck_fails: u8,
    /// Stuck incident already fired for this episode (re-armed by success).
    stuck_fired: bool,
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

/// Stuck detection: `adb shell echo` must answer within this budget; a
/// hung guest blocks past it (the 04:43 family: qemu alive, guest frozen).
const STUCK_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Uptime an AVD must reach before stuck probing starts (boot grace — adbd
/// answers shell within seconds of device-online, but give cold boots air).
const STUCK_BOOT_GRACE: Duration = Duration::from_secs(120);

/// Stuck state machine (pure): two consecutive failed shell probes fire
/// exactly once; any success resets and re-arms. `None` = not probed this
/// cycle (AVD down / inside the boot grace) — state kept as-is.
pub(crate) fn stuck_decision(
    probe_ok: Option<bool>,
    fail_streak: &mut u8,
    fired: &mut bool,
) -> bool {
    const PERSIST: u8 = 2;
    match probe_ok {
        Some(true) => {
            *fail_streak = 0;
            *fired = false;
        }
        Some(false) => *fail_streak = fail_streak.saturating_add(1),
        None => {}
    }
    if !*fired && *fail_streak >= PERSIST {
        *fired = true;
        return true;
    }
    false
}

/// Background thread: poll the fleet, revive dead AVDs on the selfheal tier
/// with the crash-loop guard. Owns no permits (emulator ops are independent
/// of backend lifecycle ops) and reads the tier fresh each cycle, so a menu
/// switch takes effect within one poll. `notify` receives every incident
/// (crash died / auto-revived / crash-loop halt / guest stuck) — the
/// direction-2 notification pipeline, passive across ALL tiers.
pub fn spawn_self_heal_poller(notify: Box<dyn Fn(EmuEvent) + Send + 'static>) {
    std::thread::Builder::new()
        .name("emu-self-heal".into())
        .spawn(move || {
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
                    Notify(EmuIncident),
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
                                track.stuck_fails = 0;
                                track.stuck_fired = false;
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
                        if consume_expected_stop(&name) {
                            // manual_stop announced this death — not an incident.
                            continue;
                        }
                        match revive_decision(tier, maaend, uptime) {
                            ReviveDecision::Revive => {
                                actions.push((name.clone(), Action::Revive));
                                actions.push((name, Action::Notify(EmuIncident::CrashRevived)));
                            }
                            // Parked on purpose (tier below selfheal / MaaEnd
                            // present): the uptime anchor is already consumed,
                            // so re-entering the tier cannot revive a long-dead
                            // AVD by accident. Still an incident — nothing
                            // will revive it, so the user must hear about it.
                            ReviveDecision::Skip => {
                                actions.push((name, Action::Notify(EmuIncident::Died)));
                            }
                            ReviveDecision::Halt => {
                                track.halted = true;
                                actions.push((name.clone(), Action::LogHalt(uptime)));
                                actions.push((name, Action::Notify(EmuIncident::CrashHalt)));
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
                            let mut state = heal_state().lock().unwrap();
                            let track = state.entry(name).or_default();
                            track.running_since = Some(Instant::now());
                            track.stuck_fails = 0;
                            track.stuck_fired = false;
                        }
                        Action::Notify(incident) => {
                            info!("emu incident: {name} — {}", incident.as_str());
                            notify(EmuEvent { avd: name, incident });
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
                // Stuck probe (passive, ALL tiers): one bounded `adb shell
                // echo` per running AVD past the boot grace. Observation, not
                // control — tier semantics unaffected.
                for (name, serial) in &serials {
                    let uptime_ready = {
                        let state = heal_state().lock().unwrap();
                        state
                            .get(name)
                            .and_then(|t| t.running_since)
                            .is_some_and(|since| since.elapsed() >= STUCK_BOOT_GRACE)
                    };
                    if !uptime_ready {
                        continue;
                    }
                    let adb = adb_path();
                    let probe = run_with_timeout(
                        &adb.to_string_lossy(),
                        &["-s", serial, "shell", "echo", "ok"],
                        STUCK_PROBE_TIMEOUT,
                    );
                    let probe_ok = probe.as_deref().map(|out| out.contains("ok"));
                    let fire = {
                        let mut state = heal_state().lock().unwrap();
                        let track = state.entry(name.clone()).or_default();
                        if track.running_since.is_none() {
                            continue; // died between the scan and the probe
                        }
                        stuck_decision(probe_ok, &mut track.stuck_fails, &mut track.stuck_fired)
                    };
                    if fire {
                        info!("emu incident: {name} — guest stuck (adb unresponsive)");
                        notify(EmuEvent {
                            avd: name.clone(),
                            incident: EmuIncident::Stuck,
                        });
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

    // ---- direction-2: stuck state machine --------------------------------

    #[test]
    fn stuck_fires_once_on_two_consecutive_failures() {
        let mut streak = 0u8;
        let mut fired = false;
        // healthy baseline
        assert!(!stuck_decision(Some(true), &mut streak, &mut fired));
        // first failure: no fire
        assert!(!stuck_decision(Some(false), &mut streak, &mut fired));
        assert_eq!(streak, 1);
        // second failure: fires exactly once
        assert!(stuck_decision(Some(false), &mut streak, &mut fired));
        // still failing: latched, no duplicate
        assert!(!stuck_decision(Some(false), &mut streak, &mut fired));
        // recovery resets and re-arms
        assert!(!stuck_decision(Some(true), &mut streak, &mut fired));
        assert_eq!(streak, 0);
        assert!(!fired);
        // a new hang episode fires again
        assert!(!stuck_decision(Some(false), &mut streak, &mut fired));
        assert!(stuck_decision(Some(false), &mut streak, &mut fired));
    }

    #[test]
    fn stuck_not_probed_cycles_keep_state() {
        // AVD down / boot-grace cycles (None) must neither advance nor reset
        // the streak — e.g. probe fail, one grace cycle, probe fail → still
        // only 2 strikes when the second FAIL lands.
        let mut streak = 0u8;
        let mut fired = false;
        assert!(!stuck_decision(Some(false), &mut streak, &mut fired));
        assert!(!stuck_decision(None, &mut streak, &mut fired));
        assert_eq!(streak, 1);
        assert!(stuck_decision(Some(false), &mut streak, &mut fired));
    }

    #[test]
    fn expected_stop_marker_roundtrip_and_single_consumption() {
        mark_expected_stop("MarkerAvd");
        // consumed once…
        assert!(consume_expected_stop("MarkerAvd"));
        // …never twice — a later death of the same AVD is a real incident.
        assert!(!consume_expected_stop("MarkerAvd"));
        // other AVDs unaffected
        assert!(!consume_expected_stop("OtherAvd"));
    }

    #[test]
    fn incident_wording_is_distinct_and_nonempty() {
        let wordings = [
            EmuIncident::Died.as_str(),
            EmuIncident::CrashRevived.as_str(),
            EmuIncident::CrashHalt.as_str(),
            EmuIncident::Stuck.as_str(),
        ];
        assert!(wordings.iter().all(|w| !w.is_empty()));
        for (i, a) in wordings.iter().enumerate() {
            assert!(wordings.iter().skip(i + 1).all(|b| a != b), "wordings must be distinct: {wordings:?}");
        }
    }
}
