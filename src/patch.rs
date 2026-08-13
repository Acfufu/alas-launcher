//! Control API patch applier for the ALAS payload.
//!
//! Anchor: `module/webui/fastapi.py` (unchanged since 2022-04-14 per the
//! payload git log). The patch injects two context-anchored fragments and
//! drops `module/webui/control_api.py`; idempotency is tracked by a marker
//! comment. Fail-closed: any mismatch surfaces as [`PatchOutcome::AnchorMismatch`]
//! or `Err`, never a partial write — **write order is control_api.py FIRST,
//! fastapi.py LAST** (a failed fastapi write leaves a harmless unused module;
//! the reverse would leave fastapi.py importing a missing module and break
//! the webui at startup). Both writes are atomic-replace via tmp + rename
//! (Windows-safe: remove the destination before rename).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

const CONTROL_API_SRC: &str = include_str!("../assets/patches/control_api.py");
const INJECT_IMPORT: &str = include_str!("../assets/patches/fastapi.inject.py");
const MARKER: &str = "# === alas-launcher:control-api ===";
const INJECT_EXTEND: &str = "    routes.extend(control_routes())\n";
const ANCHOR_IMPORT: &str = "from starlette.staticfiles import StaticFiles";
const ANCHOR_RETURN: &str = "    return Starlette(";

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

pub fn is_already_patched(fastapi_content: &str) -> bool {
    fastapi_content.contains(MARKER)
}

pub fn verify_anchor(fastapi_content: &str) -> bool {
    fastapi_content.contains(ANCHOR_IMPORT) && fastapi_content.contains(ANCHOR_RETURN)
}

/// Inject the import + routes.extend into fastapi.py content.
/// Context-anchored; fails when either anchor is missing or already patched.
fn inject_fastapi(content: &str) -> Result<String> {
    if is_already_patched(content) {
        bail!("fastapi.py already patched");
    }
    if !verify_anchor(content) {
        bail!("fastapi.py anchor mismatch");
    }
    let with_import = content.replacen(
        ANCHOR_IMPORT,
        &format!("{ANCHOR_IMPORT}\n{INJECT_IMPORT}"),
        1,
    );
    let with_extend = with_import.replacen(ANCHOR_RETURN, &format!("{INJECT_EXTEND}{ANCHOR_RETURN}"), 1);
    if with_extend == content {
        bail!("injection produced no change");
    }
    Ok(with_extend)
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
    let fastapi_path = webui.join("fastapi.py");
    let control_path = webui.join("control_api.py");
    let fastapi = match std::fs::read_to_string(&fastapi_path) {
        Ok(c) => c,
        Err(e) => {
            mark_patch_failed();
            return Err(e).context("read fastapi.py");
        }
    };
    if is_already_patched(&fastapi) {
        info!("control API patch already applied");
        return Ok(PatchOutcome::AlreadyApplied);
    }
    if !verify_anchor(&fastapi) {
        warn!("control API patch anchor mismatch in fastapi.py; scheduler toggle will degrade");
        mark_patch_failed();
        return Ok(PatchOutcome::AnchorMismatch);
    }
    match inject_fastapi(&fastapi) {
        Ok(injected) => {
            // Order matters: the module FIRST, the importer LAST. A failure
            // writing control_api.py must leave fastapi.py untouched. Every
            // error path MUST set the fail-closed flag (Round-2 MUST-FIX:
            // `?` short-circuiting bypassed mark_patch_failed, leaving the
            // tray armed while the API is actually dead).
            atomic_write(&control_path, CONTROL_API_SRC)
                .map_err(|e| { mark_patch_failed(); e })?;
            atomic_write(&fastapi_path, &injected)
                .map_err(|e| { mark_patch_failed(); e })?;
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
    use std::io::Write;

    const PRISTINE: &str = r#"from starlette.staticfiles import StaticFiles

def asgi_app(
    applications,
    ...
):
    routes = webio_routes(...)
    if static_dir:
        routes.append(...)
    routes.append(...)
    return Starlette(
        routes=routes, middleware=middleware, debug=debug, **starlette_settings
    )
"#;

    #[test]
    fn pristine_fastapi_passes_anchor() {
        assert!(verify_anchor(PRISTINE));
    }

    #[test]
    fn missing_anchor_fails_verify() {
        assert!(!verify_anchor("def something_else():\n    pass\n"));
    }

    #[test]
    fn patched_fastapi_is_detected_already_applied() {
        let patched = format!("{PRISTINE}\n# === alas-launcher:control-api ===\nfrom module.webui.control_api import control_routes\n");
        assert!(is_already_patched(&patched));
    }

    #[test]
    fn apply_patch_is_idempotent_and_writes_both_files() {
        let tmp = std::env::temp_dir().join(format!("patch-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let webui = tmp.join("module").join("webui");
        std::fs::create_dir_all(&webui).unwrap();
        std::fs::write(webui.join("fastapi.py"), PRISTINE).unwrap();

        let first = apply_patch(&tmp).expect("first apply");
        assert_eq!(first, PatchOutcome::Applied);
        assert!(webui.join("control_api.py").exists());
        assert!(std::fs::read_to_string(webui.join("fastapi.py")).unwrap().contains(MARKER));

        let second = apply_patch(&tmp).expect("second apply");
        assert_eq!(second, PatchOutcome::AlreadyApplied);
        // 注入不重复
        let fastapi = std::fs::read_to_string(webui.join("fastapi.py")).unwrap();
        assert_eq!(fastapi.matches(MARKER).count(), 1);
    }

    #[test]
    fn apply_patch_on_mismatched_anchor_degrades_not_blocks() {
        let tmp = std::env::temp_dir().join(format!("patch-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let webui = tmp.join("module").join("webui");
        std::fs::create_dir_all(&webui).unwrap();
        std::fs::write(webui.join("fastapi.py"), "def something_else():\n    pass\n").unwrap();
        assert_eq!(apply_patch(&tmp).unwrap(), PatchOutcome::AnchorMismatch);
        assert!(patch_failed());
        assert!(!webui.join("control_api.py").exists());
    }
}
