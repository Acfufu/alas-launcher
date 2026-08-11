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

use std::sync::{Arc, Mutex};

use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Wry,
};

use crate::{menu_model::ShellMenuLabels, shell_settings::ShellSettings};

/// Settings language ids in the same fixed order as `labels.lang_names`
/// (简体中文 / 繁體中文 / English / 日本語) — index `i` of each array maps to
/// menu id `settings-lang-{LANGS[i]}`.
const LANGS: [&str; 4] = ["zh-CN", "zh-TW", "en-US", "ja-JP"];

/// Build the app menu bar menu: the runtime-installed default menu (strategy
/// A base; `Menu::default` fallback if ever absent) plus the appended
/// 「外壳设置」 submenu reflecting the current settings.
pub fn build_settings_menu(
    app: &AppHandle,
    settings: &Arc<Mutex<ShellSettings>>,
    labels: &ShellMenuLabels,
) -> tauri::Result<Menu<Wry>> {
    // Probe: on macOS the runtime installs Menu::default unless disabled, so
    // this is Some; the fallback keeps strategy A intact for exotic setups.
    let menu = match app.menu() {
        Some(menu) => menu,
        None => Menu::default(app)?,
    };
    let submenu = build_settings_submenu(app, settings, labels)?;
    menu.append(&submenu)?;
    Ok(menu)
}

/// 「外壳设置」 submenu: 语言 submenu → separator → 检查更新 → separator →
/// 自动启动后端 check item.
fn build_settings_submenu(
    app: &AppHandle,
    settings: &Arc<Mutex<ShellSettings>>,
    labels: &ShellMenuLabels,
) -> tauri::Result<Submenu<Wry>> {
    let language_menu = build_language_menu(app, settings, labels)?;
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
        settings.lock().unwrap().auto_start_backend,
        None::<&str>,
    )?;

    let items: Vec<&dyn IsMenuItem<Wry>> =
        vec![&language_menu, &sep_after_language, &check_update, &sep_after_check, &auto_start];
    Submenu::with_id_and_items(app, "settings-menu", labels.settings.clone(), true, &items)
}

/// 语言 submenu: one CheckMenuItem per fixed language id + 跟随 ALAS. Exactly
/// one is checked by construction — the checked state is derived from
/// `settings.language` (Some(lang) checks that id, None checks follow).
fn build_language_menu(
    app: &AppHandle,
    settings: &Arc<Mutex<ShellSettings>>,
    labels: &ShellMenuLabels,
) -> tauri::Result<Submenu<Wry>> {
    let language = settings.lock().unwrap().language.clone();
    let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    for (i, lang) in LANGS.iter().enumerate() {
        items.push(Box::new(CheckMenuItem::with_id(
            app,
            format!("settings-lang-{lang}"),
            labels.lang_names[i].clone(),
            true,
            language.as_deref() == Some(*lang),
            None::<&str>,
        )?));
    }
    items.push(Box::new(CheckMenuItem::with_id(
        app,
        "settings-lang-follow",
        labels.follow_alas.clone(),
        true,
        language.is_none(),
        None::<&str>,
    )?));

    let refs: Vec<&dyn IsMenuItem<Wry>> =
        items.iter().map(|i| i.as_ref() as &dyn IsMenuItem<Wry>).collect();
    Submenu::with_id_and_items(app, "settings-language", labels.language.clone(), true, &refs)
}
