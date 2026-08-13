//! macOS app menu bar: the 「外壳设置」(shell settings) submenu.
//!
//! Strategy A (evidence task-3-shell-settings-menu.md): the Tauri runtime
//! already installs `Menu::default` on macOS at startup (tauri 2.5.1
//! app.rs:2068 — probe: `app.menu()` returns Some, and osascript listed
//! `Apple, alas-launcher, File, Edit, View, Window, Help`), so we keep that
//! default menu and append a single 「外壳设置」 submenu (id `settings-menu`)
//! instead of replacing the whole bar with a custom menu (strategy B would
//! drop File/Edit/View/Window/Help for no benefit).
//!
//! Menu events are dispatched by the app-level `on_menu_event` wired in
//! main.rs setup; the tray's own `TrayIconBuilder::on_menu_event` (tray-* ids)
//! is independent — `settings-*` ids never collide with it.
//!
//! # Live language switch (todo 4) — wiring design
//!
//! The plan sketched re-installing the app menu on every language click
//! (`AppHandle::set_menu`), but the LOCKED tauri is 2.5.1, where `set_menu` /
//! `menu()` exist only on `App` (never handed to menu-event handlers) and
//! `Window::set_menu` is a macOS no-op for the menu bar. Re-installing from a
//! handler is therefore impossible — and would also double-append the
//! settings submenu (`build_settings_menu` reads the previously installed
//! menu and appends again).
//!
//! Chosen design: the menu is built ONCE at startup and
//! [`build_settings_menu`] returns owned [`SettingsMenuHandles`] — the muda
//! item handles are Arc-backed native items, so mutating them in place
//! (`set_text` / `set_checked`) updates the macOS bar live. main.rs keeps the
//! handles and calls [`SettingsMenuHandles::apply_labels`] on a language
//! click; the tray wake + stopped-page re-navigation stay in main.rs (it
//! owns backend + port + the tray refresh sender).
//!
//! # Language scope (MINOR-6, design semantic)
//!
//! The switch re-labels ONLY launcher-owned UI (this app menu, the tray, the
//! launcher's stopped page). It deliberately does NOT touch the ALAS web
//! page: that language follows `Gui.Language` in deploy.yaml and is resolved
//! by the backend on startup. The two scopes are independent and MAY be out
//! of sync — a switched launcher UI next to an unchanged web page is
//! expected, not a bug.

use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Manager, Wry,
};
use tracing::{debug, info, warn};

use crate::{
    menu_model::ShellMenuLabels,
    shell_settings::ShellSettings,
};

/// Git command timeout for the check-update worker (mirrors the tray's 15s
/// control-API retry budget): a hung git (dead network, blocked DNS) must
/// degrade to 检查失败, never hang the worker forever.
const CHECK_UPDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Settings language ids in the same fixed order as `labels.lang_names`
/// (简体中文 / 繁體中文 / English / 日本語) — index `i` of each array maps to
/// menu id `settings-lang-{LANGS[i]}`.
const LANGS: [&str; 4] = ["zh-CN", "zh-TW", "en-US", "ja-JP"];

/// Pure: the next `ShellSettings.language` after a `settings-lang-*` menu
/// click. `settings-lang-follow` → None; any other `settings-lang-{lang}` →
/// Some(lang); an unrelated id leaves the current value unchanged.
pub fn language_after_click(current: Option<String>, clicked: &str) -> Option<String> {
    if clicked == "settings-lang-follow" {
        None
    } else {
        clicked
            .strip_prefix("settings-lang-")
            .map(|lang| lang.to_string())
            .or(current)
    }
}

/// Pure: whether the launcher should auto-start the backend at launch. The
/// Ready thread (main.rs) branches on this AFTER the repo setup; the
/// `settings-auto-start` menu toggle flips the underlying `auto_start_backend`
/// field. Kept as an explicit function (plan todo 6) so the launch gate is a
/// tested, named decision point instead of an inline field read.
pub fn should_auto_start(settings: &ShellSettings) -> bool {
    settings.auto_start_backend
}

/// Apply a `settings-auto-start` click: flip the shared `auto_start_backend`,
/// persist it, and return the updated settings. `save()` runs AFTER the lock
/// is dropped (no file I/O under the lock); a failed save only warns — the
/// in-memory value still applies for this session (todo-4 precedent in
/// [`handle_language_click`]). The caller (main.rs) re-checks the installed
/// CheckMenuItem in place via [`SettingsMenuHandles::apply_auto_start`]. The
/// toggle ONLY affects the NEXT launch — no backend action happens here.
pub fn handle_auto_start_click(settings: &Arc<Mutex<ShellSettings>>) -> ShellSettings {
    let updated = {
        let mut guard = settings.lock().unwrap();
        guard.auto_start_backend = !guard.auto_start_backend;
        guard.clone()
    };
    if let Err(e) = updated.save() {
        warn!("failed to persist shell settings: {e}; auto-start applies for this session only");
    }
    updated
}

/// Apply a `settings-notify-master` click: flip the shared `notify_enabled`
/// master switch, persist it, and return the updated settings. Mirrors
/// [`handle_auto_start_click`] (save AFTER the lock drops; a failed save only
/// warns — the in-memory value applies this session). The caller (main.rs)
/// re-checks the installed CheckMenuItems in place via
/// [`SettingsMenuHandles::apply_labels`].
pub fn handle_notify_master_click(settings: &Arc<Mutex<ShellSettings>>) -> ShellSettings {
    let updated = {
        let mut guard = settings.lock().unwrap();
        guard.notify_enabled = !guard.notify_enabled;
        guard.clone()
    };
    if let Err(e) = updated.save() {
        warn!("failed to persist shell settings: {e}; notifications apply for this session only");
    }
    updated
}

/// Apply a `settings-notify-death` click: flip the shared
/// `notify_scheduler_death`, persist, return the updated settings. Mirrors
/// [`handle_notify_master_click`].
pub fn handle_notify_death_click(settings: &Arc<Mutex<ShellSettings>>) -> ShellSettings {
    let updated = {
        let mut guard = settings.lock().unwrap();
        guard.notify_scheduler_death = !guard.notify_scheduler_death;
        guard.clone()
    };
    if let Err(e) = updated.save() {
        warn!("failed to persist shell settings: {e}; notifications apply for this session only");
    }
    updated
}

/// Apply a `settings-notify-task` click: flip the shared
/// `notify_task_complete`, persist, return the updated settings. Mirrors
/// [`handle_notify_master_click`].
pub fn handle_notify_task_click(settings: &Arc<Mutex<ShellSettings>>) -> ShellSettings {
    let updated = {
        let mut guard = settings.lock().unwrap();
        guard.notify_task_complete = !guard.notify_task_complete;
        guard.clone()
    };
    if let Err(e) = updated.save() {
        warn!("failed to persist shell settings: {e}; notifications apply for this session only");
    }
    updated
}

/// Pure: the menu id of the language item that must render CHECKED for the
/// given setting — None → `settings-lang-follow`, each known lang → its own
/// id, an unknown lang → follow (defensive: an unlisted language cannot be
/// checked against the fixed item set, so the follow item stays checked).
pub fn checked_language_id(language: &Option<String>) -> &'static str {
    match language.as_deref() {
        Some("zh-CN") => "settings-lang-zh-CN",
        Some("zh-TW") => "settings-lang-zh-TW",
        Some("en-US") => "settings-lang-en-US",
        Some("ja-JP") => "settings-lang-ja-JP",
        _ => "settings-lang-follow",
    }
}

/// Apply a `settings-lang-*` click: mutate the shared language, persist it,
/// and return the updated settings. `save()` runs AFTER the lock is dropped
/// (no file I/O under the lock); a failed save only warns — the in-memory
/// language still applies for this session (plan failure path). The caller
/// (main.rs) computes the new labels from the returned settings and re-renders
/// the menus (app menu via [`SettingsMenuHandles::apply_labels`], tray via
/// its refresh wake, stopped page via re-navigation).
pub fn handle_language_click(
    settings: &Arc<Mutex<ShellSettings>>,
    clicked: &str,
) -> ShellSettings {
    let updated = {
        let mut guard = settings.lock().unwrap();
        guard.language = language_after_click(guard.language.clone(), clicked);
        guard.clone()
    };
    if let Err(e) = updated.save() {
        warn!("failed to persist shell settings: {e}; language applies for this session only");
    }
    updated
}

/// Pure: the sha of the `git ls-remote` HEAD line (`<sha>\tHEAD`), e.g. from
/// `git ls-remote origin HEAD`. `None` when no HEAD line or empty output.
#[allow(dead_code)] // plan-mandated pure surface; unit-tested, worker parses branch refs
pub fn parse_ls_remote(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim_end)
        .find_map(|line| line.strip_suffix("\tHEAD").or_else(|| line.strip_suffix(" HEAD")))
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// Pure: the sha of the `git ls-remote origin <branch>` line for the LOCAL
/// branch being compared (MINOR-3 — never compare against the remote default
/// branch). Matches `refs/heads/<branch>` (full ref form) or a bare
/// `<branch>` ref; `None` when absent (no such remote branch / no remote).
pub fn parse_ls_remote_branch(output: &str, branch: &str) -> Option<String> {
    let head = format!("\trefs/heads/{branch}");
    let bare = format!("\t{branch}");
    output
        .lines()
        .map(str::trim_end)
        .find_map(|line| {
            if let Some(sha) = line.strip_suffix(&head) {
                Some(sha)
            } else {
                line.strip_suffix(&bare)
            }
        })
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// Pure: true when the remote branch head differs from the local HEAD —
/// i.e. an update is available. `None` remote (check failed) is never
/// "available".
pub fn update_available(local: &str, remote: Option<&str>) -> bool {
    match remote {
        Some(remote) => remote != local,
        None => false,
    }
}

/// Command builder: `git rev-parse HEAD` in the ALAS directory — the local
/// commit the remote is compared against.
pub fn build_rev_parse_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "HEAD"]).current_dir(cwd);
    cmd
}

/// Command builder: `git rev-parse --abbrev-ref HEAD` in the ALAS directory —
/// the LOCAL branch name, so the remote comparison targets the same branch
/// (MINOR-3: never `ls-remote origin HEAD`, which would compare against the
/// remote DEFAULT branch and misreport on non-default checkouts).
pub fn build_branch_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(cwd);
    cmd
}

/// Command builder: `git ls-remote origin <branch>` in the ALAS directory —
/// the remote tracking branch head. Read-only (no fetch/pull/update).
pub fn build_ls_remote_cmd(cwd: &Path, branch: &str) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", "origin", branch]).current_dir(cwd);
    cmd
}

/// Run one git command with piped stdout read on a reader thread (mirrors the
/// setup.rs git_update reader pattern), enforcing `timeout` via recv_timeout —
/// on timeout the child is killed and the step degrades. Returns stdout only
/// when the process exited successfully; any spawn/exit/timeout failure → None.
fn run_git_capture(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd.stdout(Stdio::piped()).spawn().ok()?;
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = kill_child(&mut child);
            return None;
        }
    };
    let mut stdout = stdout;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        let _ = tx.send(buf);
    });
    let output = match rx.recv_timeout(timeout) {
        Ok(out) => out,
        Err(_) => {
            let _ = kill_child(&mut child);
            return None;
        }
    };
    if !child.wait().is_ok_and(|s| s.success()) {
        return None;
    }
    let out = output.trim();
    if out.is_empty() {
        None
    } else {
        Some(output)
    }
}

/// Kill + reap a timed-out child; returns its exit status.
fn kill_child(child: &mut Child) -> Option<std::process::ExitStatus> {
    let _ = child.kill();
    child.wait().ok()
}

/// Worker: run the whole check-update git sequence on a dedicated thread —
/// branch name → local HEAD sha → remote same-branch sha (all read-only), then
/// surface the result as a native dialog via the plugin's ASYNC `show()`
/// callback API. The async path is the sanctioned non-blocking one: the plugin
/// hops to the main thread internally (desktop.rs) and returns immediately,
/// so the worker thread never blocks the UI.
///
/// Do NOT switch this to `blocking_show()` / `run_on_main_thread`: that
/// combination deadlocks the main thread. `blocking_show()` (tauri-plugin-dialog
/// 2.2.1 lib.rs) wraps `show()` in `blocking_fn!`, which calls `show(cb)` and
/// then blocks on `rx.recv()` — but `show()` itself queues the dialog display
/// via `handle.run_on_main_thread(...)`. If the main thread is already inside
/// our own `run_on_main_thread` closure (or any context that cannot service
/// that queued task), the callback never fires, `rx.recv()` never returns, and
/// the main thread hangs forever → the app freezes (observed: click 检查更新
/// wedges the whole UI). Plugin docs forbid blocking_show on the main thread.
///
/// Any step failing (no git / not a repo / no remote / network hang past the
/// 15s timeout) degrades to 检查失败 — the plan's failure path.
///
/// Mirrors `spawn_scheduler_click` (tray.rs:476): dedicated thread, UI thread
/// never blocks, no settings lock held across the git I/O.
pub fn spawn_check_update(
    app: &AppHandle,
    settings: &Arc<Mutex<ShellSettings>>,
    labels: ShellMenuLabels,
) {
    let app = app.clone();
    let settings = settings.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("check-update".into())
        .spawn(move || {
            let _ = settings; // worker never writes settings (todo 7 constraint)
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let msg = check_update_message(&cwd, &labels);
            info!(%msg, "check-update worker finished");
            let dialog_app = app.clone();
            let title = labels.check_update.clone();
            // Non-blocking: the plugin's `show()` hops to the main thread
            // internally (desktop.rs) and returns immediately. The previous
            // `run_on_main_thread { blocking_show() }` wedged the main thread —
            // blocking_show's rx.recv() waits for the plugin's queued main-
            // thread task, which can never run while the main thread is
            // blocked inside our own run_on_main_thread closure.
            use tauri_plugin_dialog::DialogExt;
            let mut builder = dialog_app
                .dialog()
                .message(msg)
                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                .title(title);
            // Parent the alert to the main window: with a parent rfd uses
            // the NSAlert sheet path (the reliable display path in QA).
            if let Some(win) = dialog_app.get_webview_window("main") {
                builder = builder.parent(&win);
            }
            builder.show(|_| {});
        })
    {
        warn!("failed to spawn check-update worker: {e}");
    }
}

/// The worker body split out for testability: run the git sequence and return
/// the localized message. `check_failed` on any step failure (degraded).
fn check_update_message(cwd: &Path, labels: &ShellMenuLabels) -> String {
    let Some(branch_out) = run_git_capture(&mut build_branch_cmd(cwd), CHECK_UPDATE_TIMEOUT) else {
        debug!(?cwd, "check-update: branch resolve failed");
        return labels.check_failed.clone();
    };
    let Some(branch) = branch_out.lines().next().map(str::trim).filter(|b| !b.is_empty()) else {
        debug!(?cwd, "check-update: empty branch name");
        return labels.check_failed.clone();
    };
    let Some(local) = run_git_capture(&mut build_rev_parse_cmd(cwd), CHECK_UPDATE_TIMEOUT) else {
        debug!(?cwd, "check-update: local HEAD resolve failed");
        return labels.check_failed.clone();
    };
    let local = local.trim().to_string();
    let remote = run_git_capture(&mut build_ls_remote_cmd(cwd, branch), CHECK_UPDATE_TIMEOUT)
        .and_then(|out| parse_ls_remote_branch(&out, branch));
    match remote {
        Some(remote) if update_available(&local, Some(&remote)) => labels.update_available.clone(),
        Some(_) => labels.up_to_date.clone(),
        None => {
            debug!(?cwd, branch, "check-update: no remote branch head");
            labels.check_failed.clone()
        }
    }
}

/// Owned handles to every localizable part of the installed settings menu.
/// Kept by main.rs after [`build_settings_menu`] installs the menu, so a
/// language switch can mutate the native items IN PLACE — no re-install
/// (impossible from an event handler on locked tauri 2.5.1 anyway).
pub struct SettingsMenuHandles {
    menu: Menu<Wry>,
    settings: Submenu<Wry>,
    language: Submenu<Wry>,
    lang_items: Vec<CheckMenuItem<Wry>>,
    follow: CheckMenuItem<Wry>,
    check_update: MenuItem<Wry>,
    auto_start: CheckMenuItem<Wry>,
    notify_master: CheckMenuItem<Wry>,
    notify_death: CheckMenuItem<Wry>,
    notify_task: CheckMenuItem<Wry>,
}

impl SettingsMenuHandles {
    /// The installed app menu (for the one-time `App::set_menu` at startup).
    pub fn menu(&self) -> Menu<Wry> {
        self.menu.clone()
    }

    /// Relabel + re-check every settings-menu item from the given labels and
    /// settings, in place — the macOS bar reflects the change immediately
    /// (muda handles are Arc-backed native items).
    pub fn apply_labels(
        &self,
        labels: &ShellMenuLabels,
        settings: &ShellSettings,
    ) -> tauri::Result<()> {
        self.settings.set_text(labels.settings.clone())?;
        self.language.set_text(labels.language.clone())?;
        let checked_id = checked_language_id(&settings.language);
        for (i, item) in self.lang_items.iter().enumerate() {
            item.set_text(labels.lang_names[i].clone())?;
            item.set_checked(checked_id == format!("settings-lang-{}", LANGS[i]))?;
        }
        self.follow.set_text(labels.follow_alas.clone())?;
        self.follow.set_checked(checked_id == "settings-lang-follow")?;
        self.check_update.set_text(labels.check_update.clone())?;
        self.auto_start.set_text(labels.auto_start.clone())?;
        self.auto_start.set_checked(settings.auto_start_backend)?;
        self.notify_master.set_text(labels.notify_master.clone())?;
        self.notify_master.set_checked(settings.notify_enabled)?;
        self.notify_death.set_text(labels.notify_death.clone())?;
        self.notify_death
            .set_checked(settings.notify_scheduler_death)?;
        self.notify_task.set_text(labels.notify_task.clone())?;
        self.notify_task.set_checked(settings.notify_task_complete)?;
        Ok(())
    }

    /// Re-check the installed auto-start item in place after a toggle click
    /// (muda Arc-backed native item — the macOS bar updates live; no
    /// re-install, the same locked-tauri-2.5.1 constraint as [`apply_labels`]).
    /// No menu rebuild, no relabel — only the check state changes.
    pub fn apply_auto_start(&self, settings: &ShellSettings) -> tauri::Result<()> {
        self.auto_start.set_checked(settings.auto_start_backend)?;
        Ok(())
    }
}

/// Build the app menu bar menu: the runtime-installed default menu (strategy
/// A base; `Menu::default` fallback if ever absent) plus the appended
/// 「外壳设置」 submenu reflecting the current settings, and return the
/// handles needed to relabel it in place on a language switch.
pub fn build_settings_menu(
    app: &AppHandle,
    settings: &Arc<Mutex<ShellSettings>>,
    labels: &ShellMenuLabels,
) -> tauri::Result<SettingsMenuHandles> {
    // Probe: on macOS the runtime installs Menu::default unless disabled, so
    // this is Some; the fallback keeps strategy A intact for exotic setups.
    let menu = match app.menu() {
        Some(menu) => menu,
        None => Menu::default(app)?,
    };

    // Snapshot the settings ONCE (brief lock; no I/O held).
    let (language, auto_start_backend, notify_enabled, notify_scheduler_death, notify_task_complete) =
        {
            let guard = settings.lock().unwrap();
            (
                guard.language.clone(),
                guard.auto_start_backend,
                guard.notify_enabled,
                guard.notify_scheduler_death,
                guard.notify_task_complete,
            )
        };
    let checked_id = checked_language_id(&language);

    // 语言 submenu: one CheckMenuItem per fixed language id + 跟随 ALAS.
    // Exactly one is checked by construction (checked_id derives from
    // settings.language; None/unknown → follow). Scope note (MINOR-6): the
    // switch only re-labels LAUNCHER UI — the ALAS web page keeps its own
    // deploy.yaml Gui.Language, so the two can be out of sync by design.
    let mut lang_items = Vec::with_capacity(LANGS.len());
    for (i, lang) in LANGS.iter().enumerate() {
        lang_items.push(CheckMenuItem::with_id(
            app,
            format!("settings-lang-{lang}"),
            labels.lang_names[i].clone(),
            true,
            checked_id == format!("settings-lang-{lang}"),
            None::<&str>,
        )?);
    }
    let follow = CheckMenuItem::with_id(
        app,
        "settings-lang-follow",
        labels.follow_alas.clone(),
        true,
        checked_id == "settings-lang-follow",
        None::<&str>,
    )?;
    let mut lang_refs: Vec<&dyn IsMenuItem<Wry>> =
        lang_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
    // follow must be IN the submenu (todo-8 regression: created but never
    // appended since the todo-4 rewrite; user could not switch back to follow).
    lang_refs.push(&follow);
    let language_submenu = Submenu::with_id_and_items(
        app,
        "settings-language",
        labels.language.clone(),
        true,
        &lang_refs,
    )?;

    // 外壳设置 submenu: 语言 submenu → separator → 检查更新 → separator →
    // 自动启动后端 check item → separator → 通知开关（总开关 / 调度器异常退出 /
    // 任务完成）check items.
    let sep_after_language = PredefinedMenuItem::separator(app)?;
    let check_update = MenuItem::with_id(
        app,
        "settings-check-update",
        labels.check_update.clone(),
        true,
        None::<&str>,
    )?;
    let sep_after_check = PredefinedMenuItem::separator(app)?;
    let auto_start = CheckMenuItem::with_id(
        app,
        "settings-auto-start",
        labels.auto_start.clone(),
        true,
        auto_start_backend,
        None::<&str>,
    )?;
    let sep_after_auto_start = PredefinedMenuItem::separator(app)?;
    let notify_master = CheckMenuItem::with_id(
        app,
        "settings-notify-master",
        labels.notify_master.clone(),
        true,
        notify_enabled,
        None::<&str>,
    )?;
    let notify_death = CheckMenuItem::with_id(
        app,
        "settings-notify-death",
        labels.notify_death.clone(),
        true,
        notify_scheduler_death,
        None::<&str>,
    )?;
    let notify_task = CheckMenuItem::with_id(
        app,
        "settings-notify-task",
        labels.notify_task.clone(),
        true,
        notify_task_complete,
        None::<&str>,
    )?;

    let items: Vec<&dyn IsMenuItem<Wry>> = vec![
        &language_submenu,
        &sep_after_language,
        &check_update,
        &sep_after_check,
        &auto_start,
        &sep_after_auto_start,
        &notify_master,
        &notify_death,
        &notify_task,
    ];
    let settings_submenu = Submenu::with_id_and_items(
        app,
        "settings-menu",
        labels.settings.clone(),
        true,
        &items,
    )?;
    menu.append(&settings_submenu)?;

    Ok(SettingsMenuHandles {
        menu,
        settings: settings_submenu,
        language: language_submenu,
        lang_items,
        follow,
        check_update,
        auto_start,
        notify_master,
        notify_death,
        notify_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the real-settings-file round-trip tests: they all write the
    /// SAME user settings file, so running in parallel would race each
    /// other's flip→assert→restore sequences against the shared file.
    static SETTINGS_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn language_after_click_follow_resets_to_none() {
        assert_eq!(language_after_click(Some("zh-CN".into()), "settings-lang-follow"), None);
        assert_eq!(language_after_click(None, "settings-lang-follow"), None);
    }

    #[test]
    fn language_after_click_known_langs() {
        for lang in LANGS {
            assert_eq!(
                language_after_click(None, &format!("settings-lang-{lang}")),
                Some(lang.to_string())
            );
        }
    }

    #[test]
    fn language_after_click_unknown_id_keeps_current() {
        assert_eq!(
            language_after_click(Some("en-US".into()), "tray-refresh"),
            Some("en-US".into())
        );
        assert_eq!(language_after_click(None, "settings-check-update"), None);
    }

    #[test]
    fn checked_language_id_maps_settings_to_item() {
        assert_eq!(checked_language_id(&None), "settings-lang-follow");
        assert_eq!(checked_language_id(&Some("zh-CN".into())), "settings-lang-zh-CN");
        assert_eq!(checked_language_id(&Some("zh-TW".into())), "settings-lang-zh-TW");
        assert_eq!(checked_language_id(&Some("en-US".into())), "settings-lang-en-US");
        assert_eq!(checked_language_id(&Some("ja-JP".into())), "settings-lang-ja-JP");
    }

    #[test]
    fn checked_language_id_unknown_lang_falls_back_to_follow() {
        assert_eq!(checked_language_id(&Some("fr-FR".into())), "settings-lang-follow");
    }

    #[test]
    fn should_auto_start_reflects_setting() {
        let on = ShellSettings {
            language: None,
            auto_start_backend: true,
            ..Default::default()
        };
        let off = ShellSettings {
            language: None,
            auto_start_backend: false,
            ..Default::default()
        };
        assert!(should_auto_start(&on));
        assert!(!should_auto_start(&off));
    }

    #[test]
    fn handle_auto_start_click_flips_and_persists() {
        let _serial = SETTINGS_FILE_LOCK.lock().unwrap();
        // The handler persists via ShellSettings::save() → the REAL settings
        // path (no injectable path exists in the public surface, and
        // shell_settings.rs stays untouched per plan constraints), so this
        // test snapshots the real file and restores it unconditionally via a
        // Drop guard — a panic mid-test still leaves the machine's settings
        // file exactly as it was.
        struct Restore {
            path: std::path::PathBuf,
            original: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.original {
                    Some(content) => {
                        let _ = std::fs::write(&self.path, content);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
        let restore = Restore {
            path: crate::shell_settings::settings_path(),
            original: std::fs::read_to_string(crate::shell_settings::settings_path()).ok(),
        };

        let settings = Arc::new(Mutex::new(ShellSettings {
            language: None,
            auto_start_backend: true,
            ..Default::default()
        }));
        let updated = handle_auto_start_click(&settings);
        // In-memory: both the returned snapshot and the shared settings flipped.
        assert!(!updated.auto_start_backend);
        assert!(!settings.lock().unwrap().auto_start_backend);
        // Persisted: the file on disk now reflects the flip (load is the
        // tolerant read path — corrupt/missing would default TRUE and fail
        // this assertion, so it genuinely verifies the write).
        assert!(!crate::shell_settings::load().auto_start_backend);

        drop(restore); // restore the pre-test file state
    }

    #[test]
    fn handle_notify_master_click_flips_and_persists() {
        let _serial = SETTINGS_FILE_LOCK.lock().unwrap();
        // Mirrors handle_auto_start_click_flips_and_persists: the handler
        // persists via the REAL settings path, so the test snapshots the real
        // file and restores it unconditionally via a Drop guard (a panic
        // mid-test still leaves the machine's settings file untouched).
        struct Restore {
            path: std::path::PathBuf,
            original: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.original {
                    Some(content) => {
                        let _ = std::fs::write(&self.path, content);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
        let restore = Restore {
            path: crate::shell_settings::settings_path(),
            original: std::fs::read_to_string(crate::shell_settings::settings_path()).ok(),
        };

        let settings = Arc::new(Mutex::new(ShellSettings {
            language: None,
            notify_enabled: true,
            ..Default::default()
        }));
        let updated = handle_notify_master_click(&settings);
        assert!(!updated.notify_enabled);
        assert!(!settings.lock().unwrap().notify_enabled);
        assert!(!crate::shell_settings::load().notify_enabled);

        drop(restore);
    }

    #[test]
    fn handle_notify_death_click_flips_and_persists() {
        let _serial = SETTINGS_FILE_LOCK.lock().unwrap();
        struct Restore {
            path: std::path::PathBuf,
            original: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.original {
                    Some(content) => {
                        let _ = std::fs::write(&self.path, content);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
        let restore = Restore {
            path: crate::shell_settings::settings_path(),
            original: std::fs::read_to_string(crate::shell_settings::settings_path()).ok(),
        };

        let settings = Arc::new(Mutex::new(ShellSettings {
            language: None,
            notify_scheduler_death: true,
            ..Default::default()
        }));
        let updated = handle_notify_death_click(&settings);
        assert!(!updated.notify_scheduler_death);
        assert!(!settings.lock().unwrap().notify_scheduler_death);
        assert!(!crate::shell_settings::load().notify_scheduler_death);

        drop(restore);
    }

    #[test]
    fn handle_notify_task_click_flips_and_persists() {
        let _serial = SETTINGS_FILE_LOCK.lock().unwrap();
        struct Restore {
            path: std::path::PathBuf,
            original: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.original {
                    Some(content) => {
                        let _ = std::fs::write(&self.path, content);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
        let restore = Restore {
            path: crate::shell_settings::settings_path(),
            original: std::fs::read_to_string(crate::shell_settings::settings_path()).ok(),
        };

        // notify_task_complete defaults OFF — the flip must persist false→true.
        let settings = Arc::new(Mutex::new(ShellSettings {
            language: None,
            notify_task_complete: false,
            ..Default::default()
        }));
        let updated = handle_notify_task_click(&settings);
        assert!(updated.notify_task_complete);
        assert!(settings.lock().unwrap().notify_task_complete);
        assert!(crate::shell_settings::load().notify_task_complete);

        drop(restore);
    }

    #[test]
    fn parse_ls_remote_extracts_head_sha() {
        assert_eq!(
            parse_ls_remote("abc123def456\n789012\tHEAD\n"),
            Some("789012".to_string())
        );
        assert_eq!(
            parse_ls_remote("abc123def456\trefs/heads/master\nabc123def456\tHEAD\n"),
            Some("abc123def456".to_string())
        );
    }

    #[test]
    fn parse_ls_remote_no_head_line_is_none() {
        assert_eq!(parse_ls_remote("abc123def456\trefs/heads/master\n"), None);
    }

    #[test]
    fn parse_ls_remote_empty_output_is_none() {
        assert_eq!(parse_ls_remote(""), None);
        assert_eq!(parse_ls_remote("\n\n"), None);
    }

    #[test]
    fn parse_ls_remote_branch_matches_refs_heads_and_bare() {
        let out = "abc\trefs/heads/master\nabc\tHEAD\n";
        assert_eq!(parse_ls_remote_branch(out, "master"), Some("abc".to_string()));
        assert_eq!(parse_ls_remote_branch("abc\tmaster\n", "master"), Some("abc".to_string()));
    }

    #[test]
    fn parse_ls_remote_branch_missing_branch_is_none() {
        assert_eq!(parse_ls_remote_branch("abc\trefs/heads/dev\n", "master"), None);
        assert_eq!(parse_ls_remote_branch("", "master"), None);
    }

    #[test]
    fn update_available_compares_remote_to_local() {
        assert!(!update_available("abc", Some("abc")));
        assert!(update_available("abc", Some("def")));
        assert!(!update_available("abc", None));
        assert!(!update_available("", None));
    }

    #[test]
    fn build_rev_parse_cmd_targets_local_head_in_cwd() {
        let cmd = build_rev_parse_cmd(std::path::Path::new("/tmp/alas"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["rev-parse", "HEAD"]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/tmp/alas")));
    }

    #[test]
    fn build_ls_remote_cmd_targets_origin_branch_in_cwd() {
        let cmd = build_ls_remote_cmd(std::path::Path::new("/tmp/alas"), "master");
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["ls-remote", "origin", "master"]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/tmp/alas")));
    }

    #[test]
    fn build_branch_cmd_abbrev_ref_in_cwd() {
        let cmd = build_branch_cmd(std::path::Path::new("/tmp/alas"));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/tmp/alas")));
    }
}
