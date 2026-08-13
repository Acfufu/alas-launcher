//! Launcher shell settings persistence (pure module, no tauri types).
//!
//! Stores the macOS shell-settings state (language preference + auto-start
//! backend toggle) as pretty JSON at
//! `dirs::config_dir()/alas-launcher/settings.json` — on macOS that is
//! `~/Library/Application Support/alas-launcher/settings.json`. Deliberately
//! NOT under the payload's `./config` directory: that directory is wiped by
//! `atomic_failure_cleanup` (setup.rs) on every launch, so settings must live
//! in the user config dir to survive restarts.
//!
//! Loading is tolerant by design: a missing file is a first-run default
//! (no warning), a corrupt/empty file falls back to defaults with a `warn!`
//! (never a panic). `language: None` means "follow the ALAS deploy.yaml
//! language" — see [`ShellSettings::resolved_language`].
//!
//! All functions are pure Rust + serde + dirs; no tauri, no process spawning,
//! so the module is unit-testable without a runtime.

// `#![allow(dead_code)]` retained because `settings_path` is only consumed
// by test code in non-test builds (shell_menu.rs test guard); the rest of the
// surface (load/save/resolved_language) is live from main.rs and tray.rs.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use tracing::warn;

/// Persisted launcher shell settings.
///
/// `language: None` = follow the ALAS deploy.yaml `Gui.Language`;
/// `Some(code)` = explicit override (zh-CN / zh-TW / en-US / ja-JP).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellSettings {
    /// Override language; `None` follows the ALAS deploy language.
    #[serde(default)]
    pub language: Option<String>,
    /// Whether the launcher starts the backend (gui.py) automatically at
    /// launch. Defaults ON — identical to today's behavior.
    #[serde(default = "default_true")]
    pub auto_start_backend: bool,
    /// Master switch for all launcher notifications. Default ON.
    #[serde(default = "default_true")]
    pub notify_enabled: bool,
    /// Notify when the ALAS scheduler dies abnormally (state 3). Default ON.
    #[serde(default = "default_true")]
    pub notify_scheduler_death: bool,
    /// Notify when a task completes (NextRun bump). Default OFF (silent).
    #[serde(default = "default_false_notify")]
    pub notify_task_complete: bool,
}

/// Serde fallback for `auto_start_backend` when the field is missing from an
/// older settings file: absent means "leave the current behavior" = true.
fn default_true() -> bool {
    true
}

fn default_false_notify() -> bool {
    false
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            language: None,
            auto_start_backend: true,
            notify_enabled: true,
            notify_scheduler_death: true,
            notify_task_complete: false,
        }
    }
}

/// Absolute path of the settings file, per the current platform's config dir.
///
/// Returns an EMPTY [`PathBuf`] when the platform's config dir is
/// unavailable (chosen over `Option<PathBuf>` to keep the public signature
/// stable). Empty path is NOT a valid location: callers must treat it as
/// "no persistence" — and must NOT fall back to the current directory, which
/// would be the payload's `./config` tree, wiped by `atomic_failure_cleanup`
/// (setup.rs) on every launch.
pub fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("alas-launcher")
        .join("settings.json")
}

/// Load settings from [`settings_path`].
///
/// Missing file → defaults (first run, silent); empty/corrupt JSON → defaults
/// with a warning. Never panics. With no config dir, defaults are returned
/// (in-memory only) and the reason is logged.
pub fn load() -> ShellSettings {
    let path = settings_path();
    if path.as_os_str().is_empty() {
        warn!("no config dir; shell settings are in-memory only this session");
        return ShellSettings::default();
    }
    load_from(&path)
}

fn load_from(path: &Path) -> ShellSettings {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        // Missing (or unreadable) file = first run; defaults, no warning.
        Err(_) => return ShellSettings::default(),
    };
    if content.trim().is_empty() {
        // Empty file is not worth a warning either — same as missing.
        return ShellSettings::default();
    }
    match serde_json::from_str(&content) {
        Ok(settings) => settings,
        Err(e) => {
            // Corrupt settings must never take the menu down: warn + defaults
            // (mirrors the tolerant parse style of alas_tasks::parse_i18n).
            warn!("invalid settings json at {:?}: {e}; using defaults", path);
            ShellSettings::default()
        }
    }
}

impl ShellSettings {
    /// Effective UI language: explicit setting wins, then the ALAS deploy
    /// language, then the hard default `zh-CN`.
    pub fn resolved_language(&self, deploy_language: Option<&str>) -> String {
        match &self.language {
            Some(lang) => lang.clone(),
            None => deploy_language.unwrap_or("zh-CN").to_string(),
        }
    }

    /// Persist these settings to [`settings_path`], creating parent dirs as
    /// needed. Fails with `anyhow` on IO/serialization errors (callers warn
    /// and keep the in-memory state). With no config dir the save is skipped
    /// (in-memory only) and the reason is logged.
    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        if path.as_os_str().is_empty() {
            warn!("no config dir; shell settings not persisted (in-memory only)");
            return Ok(());
        }
        self.save_to(&path)
    }

    /// Atomic write: stage `settings.json.tmp` in the SAME directory, fsync
    /// it (crash consistency — the tmp content is durable before the rename),
    /// then `fs::rename` over the target (atomic on the same filesystem).
    /// A crash mid-save therefore leaves either the old file or the new one,
    /// never a truncated settings.json. The parent directory fsync is
    /// deliberately skipped: on macOS it costs an extra open+sync_all, and a
    /// lost *rename* on crash only defaults settings back one save — the file
    /// itself is never corrupt. If the atomic path fails (tmp write or
    /// rename), fall back to a direct write and warn: persistence is
    /// best-effort, the caller keeps the in-memory state regardless.
    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("create settings dir {:?}: {e}", parent))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize settings: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        let stage = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut file, json.as_bytes())?;
            file.sync_all()
        })();
        if let Err(e) = stage {
            let _ = std::fs::remove_file(&tmp); // never leave .tmp residue
            warn!("atomic settings write failed ({e}); falling back to direct write");
            return std::fs::write(path, json).map_err(|e| anyhow!("write settings {:?}: {e}", path));
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp); // never leave .tmp residue
                warn!("atomic settings rename failed ({e}); falling back to direct write");
                std::fs::write(path, json).map_err(|e| anyhow!("write settings {:?}: {e}", path))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolated settings path for a test (no tempfile dep; keyed by pid +
    /// test name so parallel tests never collide).
    fn temp_settings_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "alas-launcher-settings-test-{}-{}",
                std::process::id(),
                name
            ))
            .join("settings.json")
    }

    fn cleanup(name: &str) {
        if let Some(parent) = temp_settings_path(name).parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn defaults() -> ShellSettings {
        ShellSettings::default()
    }

    /// Write raw file content into the isolated test dir (creates parents).
    fn write_raw(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// The temp file the atomic writer stages next to the target.
    fn tmp_path(path: &Path) -> PathBuf {
        path.with_extension("json.tmp")
    }

    #[test]
    fn settings_path_points_at_config_dir() {
        // Validates the dirs::config_dir base + join chain without doing I/O
        // on the real file.
        let path = settings_path();
        assert_eq!(path.file_name().unwrap(), "settings.json");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "alas-launcher");
    }

    #[test]
    fn save_load_roundtrip_equal() {
        let path = temp_settings_path("roundtrip");
        let settings = ShellSettings {
            language: Some("en-US".to_string()),
            auto_start_backend: false,
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(load_from(&path), settings);
        cleanup("roundtrip");
    }

    #[test]
    fn roundtrip_defaults_equal() {
        let path = temp_settings_path("roundtrip-defaults");
        defaults().save_to(&path).unwrap();
        assert_eq!(load_from(&path), defaults());
        cleanup("roundtrip-defaults");
    }

    #[test]
    fn missing_file_returns_defaults() {
        let path = temp_settings_path("missing");
        // Parent may exist from a previous run; the file itself must not.
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), defaults());
        cleanup("missing");
    }

    #[test]
    fn empty_file_returns_defaults() {
        let path = temp_settings_path("empty");
        write_raw(&path, "");
        assert_eq!(load_from(&path), defaults());
        cleanup("empty");
    }

    #[test]
    fn corrupt_json_returns_defaults() {
        let path = temp_settings_path("corrupt");
        write_raw(&path, "{bad");
        assert_eq!(load_from(&path), defaults());
        cleanup("corrupt");
    }

    #[test]
    fn resolved_language_setting_wins() {
        let settings = ShellSettings {
            language: Some("ja-JP".to_string()),
            auto_start_backend: true,
            ..Default::default()
        };
        assert_eq!(settings.resolved_language(Some("en-US")), "ja-JP");
    }

    #[test]
    fn resolved_language_falls_back_to_deploy() {
        let settings = defaults();
        assert_eq!(settings.resolved_language(Some("zh-TW")), "zh-TW");
    }

    #[test]
    fn resolved_language_both_none_falls_back_zh_cn() {
        let settings = defaults();
        assert_eq!(settings.resolved_language(None), "zh-CN");
    }

    #[test]
    fn serde_defaults_missing_fields() {
        // Older/partial settings file: auto_start_backend absent → true.
        let settings: ShellSettings = serde_json::from_str(r#"{"language":"zh-TW"}"#).unwrap();
        assert!(settings.auto_start_backend);
        assert_eq!(settings.language.as_deref(), Some("zh-TW"));

        // Empty object: language absent → None, auto_start_backend → true.
        let settings: ShellSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, defaults());
    }

    #[test]
    fn old_settings_file_without_notify_fields_defaults() {
        let s: ShellSettings = serde_json::from_str(r#"{"language": "zh-CN", "auto_start_backend": false}"#).unwrap();
        assert!(s.notify_enabled);
        assert!(s.notify_scheduler_death);
        assert!(!s.notify_task_complete);
    }

    #[test]
    fn explicit_notify_values_roundtrip() {
        let s = ShellSettings {
            language: None,
            auto_start_backend: true,
            notify_enabled: false,
            notify_scheduler_death: false,
            notify_task_complete: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ShellSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn atomic_save_leaves_no_tmp_residue() {
        let path = temp_settings_path("atomic");
        let settings = ShellSettings {
            language: Some("en-US".to_string()),
            auto_start_backend: false,
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        // Content is readable via the tolerant load path.
        assert_eq!(load_from(&path), settings);
        // Atomic contract: the staged temp file must not linger next to the
        // target (a naive "write tmp, forget rename" leaves a stale copy).
        assert!(!tmp_path(&path).exists());
        cleanup("atomic");
    }

    #[test]
    fn save_to_rename_failure_falls_back_and_errors() {
        // Simulate a rename-only failure: the target path is an existing
        // DIRECTORY, which rename() rejects (file→dir is EISDIR on Unix). The
        // fallback direct write hits the same wall (O_WRONLY on a dir path is
        // EISDIR), so save_to must surface Err — and must still not leave the
        // staged .tmp behind.
        let dir = temp_settings_path("rename-fail");
        let parent = dir.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let result = defaults().save_to(&dir);
        assert!(result.is_err());
        assert!(!tmp_path(&dir).exists());
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_replaces_target_symlink_instead_of_writing_through() {
        use std::os::unix::fs::symlink;
        let parent = temp_settings_path("symlink")
            .parent()
            .unwrap()
            .to_path_buf();
        let target = parent.join("settings.json");
        let real = parent.join("real-file.json");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&real, "original").unwrap();
        symlink(&real, &target).unwrap();
        defaults().save_to(&target).unwrap();
        // Atomic rename replaces the symlink itself — the pre-existing real
        // file stays untouched (a direct write would have gone THROUGH the
        // link), and the target is now a plain file with our JSON.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "original");
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(!meta.file_type().is_symlink());
        assert_eq!(load_from(&target), defaults());
        std::fs::remove_dir_all(&parent).unwrap();
    }
}
