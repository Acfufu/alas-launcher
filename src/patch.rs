//! Control API patch applier for the ALAS payload.
//!
//! Anchor: `module/webui/api/__init__.py` — the FastAPI app factory
//! (`create_api_app()`) introduced by the PR-5885 webui rewrite. The
//! pre-rewrite anchor `module/webui/fastapi.py` no longer exists on that
//! payload line. The patch injects two context-anchored fragments and drops
//! `module/webui/control_api.py`; idempotency is tracked by a marker
//! comment. Fail-closed: any mismatch surfaces as [`PatchOutcome::AnchorMismatch`]
//! or `Err`, never a partial write — **write order is control_api.py FIRST,
//! __init__.py LAST** (a failed __init__ write leaves a harmless unused
//! module; the reverse would leave __init__ importing a missing module and
//! break the webui at startup). Both writes are atomic-replace via tmp +
//! rename (Windows-safe: remove the destination before rename).
//!
//! Injection shape (mirrors `create_api_app()` in the payload):
//!   1. import fragment inserted directly ABOVE `def create_api_app() -> FastAPI:`
//!   2. `app.include_router(alas_control_router)` inserted directly BELOW
//!      the last `app.include_router(events.router)` line — registration
//!      order matters, the catch-all frontend mount at "/" comes later in
//!      the file and must not shadow API routes.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

const CONTROL_API_SRC: &str = include_str!("../assets/patches/control_api.py");
const INJECT_IMPORT: &str = include_str!("../assets/patches/fastapi.inject.py");
/// Exposed crate-wide: the real-payload integration test (control_api.rs)
/// asserts marker uniqueness on the installed payload.
pub(crate) const MARKER: &str = "# === alas-launcher:control-api ===";
const INJECT_INCLUDE: &str = "    app.include_router(alas_control_router)\n";
const ANCHOR_DEF: &str = "def create_api_app() -> FastAPI:";
const ANCHOR_INCLUDE: &str = "    app.include_router(events.router)";

static PATCH_FAILED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    Applied,
    AlreadyApplied,
    AnchorMismatch,
}

pub fn patch_failed() -> bool {
    PATCH_FAILED.load(Ordering::Relaxed)
}
// Round-3 NIT：`patch_failed()` 仅被 macOS 门控的 tray.rs 消费——win/linux target 上
// 是死代码（cargo clippy --target win/linux 会告警）。若未来跑跨 target clippy，
// 给 reader 函数加 `#[cfg(target_os = "macos")]`（writer 在 apply_patch 内部，跨平台保留）。

fn mark_patch_failed() {
    PATCH_FAILED.store(true, Ordering::Relaxed);
}

pub fn is_already_patched(init_content: &str) -> bool {
    init_content.contains(MARKER)
}

pub fn verify_anchor(init_content: &str) -> bool {
    init_content.contains(ANCHOR_DEF) && init_content.contains(ANCHOR_INCLUDE)
}

/// Inject the import + include_router into api/__init__.py content.
/// Context-anchored; fails when either anchor is missing or already patched.
fn inject_api_init(content: &str) -> Result<String> {
    if is_already_patched(content) {
        bail!("api/__init__.py already patched");
    }
    if !verify_anchor(content) {
        bail!("api/__init__.py anchor mismatch");
    }
    let with_import = content.replacen(ANCHOR_DEF, &format!("{INJECT_IMPORT}\n{ANCHOR_DEF}"), 1);
    let with_include = with_import.replacen(
        ANCHOR_INCLUDE,
        &format!("{ANCHOR_INCLUDE}\n{INJECT_INCLUDE}"),
        1,
    );
    if with_include == with_import {
        bail!("include injection produced no change");
    }
    Ok(with_include)
}

/// Atomic replace; Windows-safe (rename over an existing file fails on
/// Windows, so the destination is removed first — the tiny non-atomic window
/// is acceptable: a crash here just means the next launch re-applies).
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))
}

pub fn apply_patch(alas_dir: &Path) -> Result<PatchOutcome> {
    let webui = alas_dir.join("module").join("webui");
    let init_path = webui.join("api").join("__init__.py");
    let control_path = webui.join("control_api.py");
    let api_init = match std::fs::read_to_string(&init_path) {
        Ok(c) => c,
        Err(e) => {
            mark_patch_failed();
            return Err(e).context("read api/__init__.py");
        }
    };
    if is_already_patched(&api_init) {
        info!("control API patch already applied");
        return Ok(PatchOutcome::AlreadyApplied);
    }
    if !verify_anchor(&api_init) {
        warn!("control API patch anchor mismatch in api/__init__.py; scheduler toggle will degrade");
        mark_patch_failed();
        return Ok(PatchOutcome::AnchorMismatch);
    }
    match inject_api_init(&api_init) {
        Ok(injected) => {
            // Order matters: the module FIRST, the importer LAST. A failure
            // writing control_api.py must leave api/__init__.py untouched.
            // Every error path MUST set the fail-closed flag (Round-2
            // MUST-FIX: `?` short-circuiting bypassed mark_patch_failed,
            // leaving the tray armed while the API is actually dead).
            atomic_write(&control_path, CONTROL_API_SRC)
                .inspect_err(|_| mark_patch_failed())?;
            atomic_write(&init_path, &injected)
                .inspect_err(|_| mark_patch_failed())?;
            info!("control API patch applied");
            Ok(PatchOutcome::Applied)
        }
        Err(e) => {
            warn!("control API patch failed: {e:#}; scheduler toggle will degrade");
            mark_patch_failed();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRISTINE: &str = r#""""FastAPI-based REST + SSE API for the Svelte SPA frontend."""

import os
import threading

from fastapi import FastAPI

from module.webui.api.routers import config, control, events, i18n, remote, scheduler, schema, status, theme, updater
from module.webui.setting import State


def create_api_app() -> FastAPI:
    app = FastAPI(title="Alas API")

    app.include_router(status.router)
    app.include_router(events.router)

    return app
"#;

    #[test]
    fn pristine_api_init_passes_anchor() {
        assert!(verify_anchor(PRISTINE));
    }

    #[test]
    fn missing_anchor_fails_verify() {
        assert!(!verify_anchor("def something_else():\n    pass\n"));
    }

    #[test]
    fn missing_include_anchor_fails_injection() {
        // 删掉 events.router 注册行后：def 锚点还在、include 锚点缺失，
        // 注入必须在第二步失败（防半截注入）。
        let partial = PRISTINE.replace("    app.include_router(events.router)\n", "");
        assert!(!partial.contains(ANCHOR_INCLUDE));
        assert!(inject_api_init(&partial).is_err());
    }

    #[test]
    fn patched_api_init_is_detected_already_applied() {
        let patched = format!("{PRISTINE}\n{MARKER}\nfrom module.webui.control_api import router as alas_control_router\n");
        assert!(is_already_patched(&patched));
    }

    #[test]
    fn injected_content_registers_router_before_return() {
        let out = inject_api_init(PRISTINE).unwrap();
        assert!(out.contains("from module.webui.control_api import router as alas_control_router"));
        assert!(out.contains(INJECT_INCLUDE));
        // include 必须仍位于 return app 之前（路由先注册，前端 catch-all mount 后挂载）
        let include_pos = out.find("app.include_router(alas_control_router)").unwrap();
        let return_pos = out.find("    return app").unwrap();
        assert!(include_pos < return_pos);
        // import 注入必须位于 def create_api_app 之前（模块级 import）
        let import_pos = out.find("from module.webui.control_api import router").unwrap();
        let def_pos = out.find(ANCHOR_DEF).unwrap();
        assert!(import_pos < def_pos);
    }

    #[test]
    fn apply_patch_is_idempotent_and_writes_both_files() {
        let tmp = std::env::temp_dir().join(format!("patch-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let api_dir = tmp.join("module").join("webui").join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(api_dir.join("__init__.py"), PRISTINE).unwrap();

        let first = apply_patch(&tmp).expect("first apply");
        assert_eq!(first, PatchOutcome::Applied);
        assert!(tmp.join("module").join("webui").join("control_api.py").exists());
        assert!(std::fs::read_to_string(api_dir.join("__init__.py")).unwrap().contains(MARKER));

        let second = apply_patch(&tmp).expect("second apply");
        assert_eq!(second, PatchOutcome::AlreadyApplied);
        // 注入不重复
        let init = std::fs::read_to_string(api_dir.join("__init__.py")).unwrap();
        assert_eq!(init.matches(MARKER).count(), 1);
    }

    #[test]
    fn apply_patch_on_mismatched_anchor_degrades_not_blocks() {
        let tmp = std::env::temp_dir().join(format!("patch-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let api_dir = tmp.join("module").join("webui").join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(api_dir.join("__init__.py"), "def something_else():\n    pass\n").unwrap();
        assert_eq!(apply_patch(&tmp).unwrap(), PatchOutcome::AnchorMismatch);
        assert!(patch_failed());
        assert!(!tmp.join("module").join("webui").join("control_api.py").exists());
    }

    #[test]
    fn apply_patch_write_failure_is_fail_closed() {
        let tmp = std::env::temp_dir().join(format!("patch-writefail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let api_dir = tmp.join("module").join("webui").join("api");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::write(api_dir.join("__init__.py"), PRISTINE).unwrap();
        // control_api.py exists as a DIRECTORY: rename(tmp, path) fails on Unix.
        std::fs::create_dir(tmp.join("module").join("webui").join("control_api.py")).unwrap();

        let result = apply_patch(&tmp);
        assert!(result.is_err());
        assert!(patch_failed());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_patch_read_failure_is_fail_closed() {
        let tmp = std::env::temp_dir().join(format!("patch-readfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("module").join("webui").join("api")).unwrap();

        let result = apply_patch(&tmp);
        assert!(result.is_err());
        assert!(patch_failed());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
