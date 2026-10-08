//! Typed reads of `config/deploy.yaml` — the single owner of the
//! `Deploy`/`Gui`/`Webui` traversals that were hand-written five times
//! (main.rs WebuiPort + deploy_language duplicate, tray.rs deploy_language /
//! enable_reload / ws_control_available). Cross-platform: setup.rs
//! `get_deploy_config` already reads the payload file cwd-relative without
//! any platform gating, so this module carries no cfg either.
//!
//! Return semantics are pinned one-to-one to the pre-module callers, with two
//! audited corrections (R1 dual review):
//! - `webui_port`: integer `Deploy.Webui.WebuiPort`, read via `as_u64`; a
//!   float or string falls back to the default. A value above `u16::MAX`
//!   warns once and falls back to `DEFAULT_PORT` (22267) — the legacy
//!   `as u16` wrap-around silently launched on the wrong port. Warns once
//!   when the key is missing/unparsable (the once-guards keep
//!   `load()`-backed helper calls from spamming).
//! - `language`: string `Deploy.Webui.Language` → `Some`; missing / null /
//!   non-string → `None`. The legacy upstream layout `Gui.Language` is kept
//!   as a fallback for older payload trees — the shipped deploy templates
//!   (and every observed payload) carry the key under `Deploy.Webui` only.
//!   An empty string stays `Some("")` — the old code never normalized it,
//!   and `ShellSettings::resolved_language` applies the zh-CN fallback
//!   downstream.
//! - `enable_reload`: bool `Deploy.Update.EnableReload`; anything else →
//!   `true` (the ALAS default, deploy.yaml:86).
//! - `ws_control_available`: true unless a non-empty value sits in
//!   `Deploy.Webui.Password` / `WebuiSSLKey` / `WebuiSSLCert` — non-empty
//!   strings AND non-string truthy values both count as configured; null,
//!   missing and empty-string count as "no credential" (R3 审计：与 Python
//!   网关 `_locked()` 的真值判定对齐，非字符串曾导致客户端放行而服务端
//!   401). A missing config file → available (no credentials can be
//!   configured without a file).

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tracing::warn;

/// Default webui port when `Deploy.Webui.WebuiPort` is absent/unparsable.
pub const DEFAULT_PORT: u16 = 22267;

/// Warn once (not per `load()` call) that the port fell back to the default
/// because the key was missing/unparsable.
static PORT_WARNED: AtomicBool = AtomicBool::new(false);

/// Warn once that the port fell back to the default because the configured
/// value exceeds `u16::MAX` (the legacy code silently truncated instead).
static PORT_RANGE_WARNED: AtomicBool = AtomicBool::new(false);

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
        let webui_port = match port_raw {
            Some(p) => u16::try_from(p).unwrap_or_else(|_| {
                // Legacy `p as u16` wrapped 70000 → 4464 (the launcher then
                // probed/connected a port the payload never listened on).
                // Fall back to the default instead — loudly.
                if !PORT_RANGE_WARNED.swap(true, Ordering::Relaxed) {
                    warn!("WebuiPort {p} exceeds u16 range, using default port {DEFAULT_PORT}");
                }
                DEFAULT_PORT
            }),
            None => DEFAULT_PORT,
        };
        if port_raw.is_none() && !PORT_WARNED.swap(true, Ordering::Relaxed) {
            warn!("WebuiPort not found in config, using default port 22267");
        }
        let language = config
            .and_then(|c| c.get("Deploy"))
            .and_then(|d| d.get("Webui"))
            .and_then(|w| w.get("Language"))
            .or_else(|| {
                // Legacy upstream layout: older payload trees carry the
                // language under a top-level Gui section.
                config
                    .and_then(|c| c.get("Gui"))
                    .and_then(|g| g.get("Language"))
            })
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
            cur.map(|v| match v {
                Value::Null => false,
                Value::String(s) => !s.is_empty(),
                // R4 审计：精确对齐 Python 真值语义（`_locked()` 的
                // bool(v) 判定即 PasswordGate 的锁门条件）——bool 0 是假、
                // 数字 0 是假、空序列/映射是假，其余非 null 一律按已配置。
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64() != Some(0.0),
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
            })
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

/// `Deploy.Webui.Language` (legacy fallback `Gui.Language`) as owned string;
/// `None` when absent/null/non-string.
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
            "Deploy": {"Webui": {"WebuiPort": null, "Language": null}, "Update": {"EnableReload": null}},
        }));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        assert_eq!(cfg.language(), None);
        assert!(cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    #[test]
    fn non_string_port_language_and_non_bool_reload_fall_back() {
        let cfg = typed(json!({
            "Deploy": {"Webui": {"WebuiPort": "22267", "Language": 42}, "Update": {"EnableReload": "true"}},
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

    /// R1 审计（B5）：超出 u16 的端口必须回落默认值并告警，而非按位截断
    /// （70000 as u16 == 4464，启动器会探活一个 payload 根本没监听的端口）。
    #[test]
    fn out_of_range_port_falls_back_to_default_not_truncated() {
        let cfg = typed(json!({"Deploy": {"Webui": {"WebuiPort": 70000}}}));
        assert_eq!(cfg.webui_port(), DEFAULT_PORT);
        // u16::MAX 边界值仍然有效。
        let max = typed(json!({"Deploy": {"Webui": {"WebuiPort": 65535}}}));
        assert_eq!(max.webui_port(), 65535);
    }

    #[test]
    fn empty_strings_keep_some_language_and_available_ws() {
        let cfg = typed(json!({
            "Deploy": {"Webui": {"Language": "", "Password": ""}},
        }));
        assert_eq!(cfg.language(), Some(String::new()));
        assert!(cfg.ws_control_available());
    }

    /// 语言键主路径：随附模板与全部实测 payload 的形状 `Deploy.Webui.Language`。
    #[test]
    fn full_valid_values_are_extracted() {
        let cfg = typed(json!({
            "Deploy": {
                "Webui": {"WebuiPort": 22268, "Language": "en-US"},
                "Update": {"EnableReload": false},
            },
        }));
        assert_eq!(cfg.webui_port(), 22268);
        assert_eq!(cfg.language(), Some("en-US".to_string()));
        assert!(!cfg.enable_reload());
        assert!(cfg.ws_control_available());
    }

    /// R1 审计（B2）：旧版上游布局 `Gui.Language` 仍是受支持的回退路径。
    #[test]
    fn legacy_gui_language_fallback_still_works() {
        let cfg = typed(json!({"Gui": {"Language": "zh-CN"}}));
        assert_eq!(cfg.language(), Some("zh-CN".to_string()));
    }

    /// 主路径优先：`Deploy.Webui.Language` 存在时不看 `Gui.Language`。
    #[test]
    fn deploy_webui_language_takes_precedence_over_legacy() {
        let cfg = typed(json!({
            "Deploy": {"Webui": {"Language": "en-US"}},
            "Gui": {"Language": "zh-CN"},
        }));
        assert_eq!(cfg.language(), Some("en-US".to_string()));
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

    /// R3/R4 审计：非字符串凭据精确对齐 Python 真值——123 降级、0/false
    /// 不降级、null/缺失/空串不降级。
    #[test]
    fn non_string_credential_values_follow_python_truthiness() {
        let truthy = typed(json!({"Deploy": {"Webui": {"Password": 123}}}));
        assert!(!truthy.ws_control_available());
        let zero = typed(json!({"Deploy": {"Webui": {"Password": 0}}}));
        assert!(zero.ws_control_available());
        let falsey = typed(json!({"Deploy": {"Webui": {"Password": false}}}));
        assert!(falsey.ws_control_available());
        let null_cfg = typed(json!({"Deploy": {"Webui": {"Password": null}}}));
        assert!(null_cfg.ws_control_available());
    }
}
