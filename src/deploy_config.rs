//! Typed reads of `config/deploy.yaml` — the single owner of the
//! `Deploy`/`Gui`/`Webui` traversals that were hand-written five times
//! (main.rs WebuiPort + deploy_language duplicate, tray.rs deploy_language /
//! enable_reload / ws_control_available). Cross-platform: setup.rs
//! `get_deploy_config` already reads the payload file cwd-relative without
//! any platform gating, so this module carries no cfg either.
//!
//! Return semantics are pinned one-to-one to the pre-module callers:
//! - `webui_port`: integer `Deploy.Webui.WebuiPort`, read via `as_u64` then
//!   `as u16` exactly like the old main.rs chain (a value above `u16::MAX`
//!   truncates, a float or string falls back to the default); anything else
//!   → `DEFAULT_PORT` (22267). Warns once when the key is missing/unparsable
//!   (main.rs used to warn at startup; the once-guard keeps `load()`-backed
//!   helper calls from spamming).
//! - `language`: string `Gui.Language` → `Some`; missing / null / non-string
//!   → `None`. An empty string stays `Some("")` — the old code never
//!   normalized it, and `ShellSettings::resolved_language` applies the
//!   zh-CN fallback downstream.
//! - `enable_reload`: bool `Deploy.Update.EnableReload`; anything else →
//!   `true` (the ALAS default, deploy.yaml:86).
//! - `ws_control_available`: true unless a NON-EMPTY string sits in
//!   `Deploy.Webui.Password` / `WebuiSSLKey` / `WebuiSSLCert`; null, missing,
//!   empty and non-string all count as "no credential" (ALAS skips login for
//!   an empty password). A missing config file → available (no credentials
//!   can be configured without a file).

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tracing::warn;

/// Default webui port when `Deploy.Webui.WebuiPort` is absent/unparsable.
pub const DEFAULT_PORT: u16 = 22267;

/// Warn once (not per `load()` call) that the port fell back to the default.
static PORT_WARNED: AtomicBool = AtomicBool::new(false);

/// Typed view of the deploy configuration; field fallbacks documented above.
pub struct DeployConfig {
    webui_port: u16,
    language: Option<String>,
    enable_reload: bool,
    ws_control_available: bool,
}

impl DeployConfig {
    /// Read `./config/deploy.yaml` (cwd-relative, same as the setup path)
    /// and build the typed view. A read/parse failure behaves exactly like a
    /// missing file: every field falls back to its default.
    pub fn load() -> DeployConfig {
        Self::from_value(crate::setup::get_deploy_config().as_ref())
    }

    /// Pure parse core — no file I/O, so the default matrix is testable with
    /// crafted values. `None` = missing/unreadable config.
    pub fn from_value(config: Option<&Value>) -> DeployConfig {
        let port_raw = config
            .and_then(|c| c.get("Deploy"))
            .and_then(|d| d.get("Webui"))
            .and_then(|w| w.get("WebuiPort"))
            .and_then(|p| p.as_u64());
        let webui_port = port_raw.map(|p| p as u16).unwrap_or(DEFAULT_PORT);
        if port_raw.is_none() && !PORT_WARNED.swap(true, Ordering::Relaxed) {
            warn!("WebuiPort not found in config, using default port 22267");
        }
        let language = config
            .and_then(|c| c.get("Gui"))
            .and_then(|g| g.get("Language"))
            .and_then(|l| l.as_str())
            .map(String::from);
        let enable_reload = config
            .and_then(|c| c.get("Deploy"))
            .and_then(|d| d.get("Update"))
            .and_then(|u| u.get("EnableReload"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let configured = |path: &[&str]| -> bool {
            let mut cur = config;
            for key in path {
                cur = cur.and_then(|c| c.get(key));
                if cur.is_none() {
                    return false;
                }
            }
            cur.and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        };
        let ws_control_available = !(configured(&["Deploy", "Webui", "Password"])
            || configured(&["Deploy", "Webui", "WebuiSSLKey"])
            || configured(&["Deploy", "Webui", "WebuiSSLCert"]));
        DeployConfig {
            webui_port,
            language,
            enable_reload,
            ws_control_available,
        }
    }

    pub fn webui_port(&self) -> u16 {
        self.webui_port
    }

    pub fn language(&self) -> Option<String> {
        self.language.clone()
    }

    pub fn enable_reload(&self) -> bool {
        self.enable_reload
    }

    pub fn ws_control_available(&self) -> bool {
        self.ws_control_available
    }
}

// The pre-module call sites each re-read the payload file per call, so these
// helpers keep the same I/O profile: one `load()` per typed read.

/// `Deploy.Webui.WebuiPort` as `u16`, `DEFAULT_PORT` when absent/unparsable.
pub fn webui_port() -> u16 {
    DeployConfig::load().webui_port()
}

/// `Gui.Language` as owned string; `None` when absent/null/non-string.
pub fn language() -> Option<String> {
    DeployConfig::load().language()
}

/// `Deploy.Update.EnableReload`, `true` when absent (the ALAS default).
pub fn enable_reload() -> bool {
    DeployConfig::load().enable_reload()
}

/// True when the webui has no password/TLS configured (ws control usable).
pub fn ws_control_available() -> bool {
    DeployConfig::load().ws_control_available()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn typed(v: Value) -> DeployConfig {
        DeployConfig::from_value(Some(&v))
    }

    /// `load()` is a thin file wrapper; smoke-test that a missing/unreadable
    /// payload file never panics (cwd here is the repo root, which has no
    /// config/deploy.yaml — so this exercises the all-defaults path).
    #[test]
    fn load_without_payload_file_never_panics() {
        let cfg = DeployConfig::load();
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn missing_config_falls_back_to_all_defaults() {
        let cfg = DeployConfig::from_value(None);
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn missing_keys_fall_back_to_all_defaults() {
        let cfg = typed(json!({}));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn null_values_fall_back_to_all_defaults() {
        let cfg = typed(json!({
            "Deploy": {"Webui": {"WebuiPort": null}, "Update": {"EnableReload": null}},
            "Gui": {"Language": null},
        }));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn non_string_port_language_and_non_bool_reload_fall_back() {
        let cfg = typed(json!({
            "Deploy": {"Webui": {"WebuiPort": "22267"}, "Update": {"EnableReload": "true"}},
            "Gui": {"Language": 42},
        }));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
    }

    #[test]
    fn float_port_falls_back_to_default() {
        let cfg = typed(json!({"Deploy": {"Webui": {"WebuiPort": 22267.5}}}));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
    }

    #[test]
    fn empty_strings_keep_some_language_and_available_ws() {
        let cfg = typed(json!({
            "Gui": {"Language": ""},
            "Deploy": {"Webui": {"Password": ""}},
        }));
        assert_eq!(cfg.language(), Some(String::new()));
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn full_valid_values_are_extracted() {
        let cfg = typed(json!({
            "Deploy": {
                "Webui": {"WebuiPort": 22268},
                "Update": {"EnableReload": false},
            },
            "Gui": {"Language": "zh-CN"},
        }));
        assert_eq!(cfg.webui_port(), 22268);
        assert_eq!(cfg.language(), Some("zh-CN".to_string()));
        assert!(!cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn non_empty_password_degrades_ws_control() {
        let cfg = typed(json!({"Deploy": {"Webui": {"Password": "secret"}}}));
        assert!(!cfg.ws_control_available());
    }

    #[test]
    fn non_empty_ssl_key_or_cert_degrades_ws_control() {
        let key = typed(json!({"Deploy": {"Webui": {"WebuiSSLKey": "/tmp/k.pem"}}}));
        assert!(!key.ws_control_available());
        let cert = typed(json!({"Deploy": {"Webui": {"WebuiSSLCert": "/tmp/c.pem"}}}));
        assert!(!cert.ws_control_available());
    }

    #[test]
    fn non_string_credential_values_do_not_degrades_ws_control() {
        let cfg = typed(json!({"Deploy": {"Webui": {"Password": 123}}}));
        assert!(cfg.ws_control_available());
    }
}
