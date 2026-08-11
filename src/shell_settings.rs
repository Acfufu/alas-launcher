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

// Staged module: consumers (settings menu + ready-thread wiring) land in
// later commits (todo 3+); until then the bin target must not fail
// clippy -D warnings on unused items.
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
}

/// Serde fallback for `auto_start_backend` when the field is missing from an
/// older settings file: absent means "leave the current behavior" = true.
fn default_true() -> bool {
    true
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            language: None,
            auto_start_backend: true,
        }
    }
}

/// Absolute path of the settings file, per the current platform's config dir.
pub fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("alas-launcher")
        .join("settings.json")
}

/// Load settings from [`settings_path`].
///
/// Missing file → defaults (first run, silent); empty/corrupt JSON → defaults
/// with a warning. Never panics.
pub fn load() -> ShellSettings {
    load_from(&settings_path())
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
    /// and keep the in-memory state).
    pub fn save(&self) -> Result<()> {
        self.save_to(&settings_path())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("create settings dir {:?}: {e}", parent))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize settings: {e}"))?;
        std::fs::write(path, json).map_err(|e| anyhow!("write settings {:?}: {e}", path))
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
        ShellSettings {
            language: None,
            auto_start_backend: true,
        }
    }

    /// Write raw file content into the isolated test dir (creates parents).
    fn write_raw(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
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
}
