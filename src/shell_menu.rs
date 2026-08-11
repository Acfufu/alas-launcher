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

use std::sync::{Arc, Mutex};

use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Wry,
};
use tracing::warn;

use crate::{
    menu_model::ShellMenuLabels,
    shell_settings::ShellSettings,
};

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
    let (language, auto_start_backend) = {
        let guard = settings.lock().unwrap();
        (guard.language.clone(), guard.auto_start_backend)
    };
    let checked_id = checked_language_id(&language);

    // 语言 submenu: one CheckMenuItem per fixed language id + 跟随 ALAS.
    // Exactly one is checked by construction (checked_id derives from
    // settings.language; None/unknown → follow).
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
    let lang_refs: Vec<&dyn IsMenuItem<Wry>> =
        lang_items.iter().map(|i| i as &dyn IsMenuItem<Wry>).collect();
    let language_submenu = Submenu::with_id_and_items(
        app,
        "settings-language",
        labels.language.clone(),
        true,
        &lang_refs,
    )?;

    // 外壳设置 submenu: 语言 submenu → separator → 检查更新 → separator →
    // 自动启动后端 check item.
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

    let items: Vec<&dyn IsMenuItem<Wry>> = vec![
        &language_submenu,
        &sep_after_language,
        &check_update,
        &sep_after_check,
        &auto_start,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
