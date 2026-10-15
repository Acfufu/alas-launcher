# module/webui/control_api.py
# Added by alas-launcher (GPLv3). Control API for scheduler start/stop,
# protocol handshake and update-restart requests.
# FastAPI router form for the PR-5885 webui: injected via
# `app.include_router` into module/webui/api/__init__.py (the old
# starlette-routes form anchored on the now-deleted fastapi.py).
# Paths mirror the Rust client (src/control_api.rs) verbatim.
import json
import os
import time
from pathlib import Path

from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse

from module.config.utils import alas_instance
from module.webui.process_manager import ProcessManager
from module.webui.setting import State

router = APIRouter(tags=["alas-launcher-control"])

# Bump on ANY endpoint/semantic change. The launcher ships this file and
# verifies the version once the backend is ready (run_protocol_handshake in
# src/control_api.rs); a mismatch degrades the tray toggle fail-open-side —
# only a CONFIRMED mismatch disables control calls.
PROTOCOL_VERSION = 2

# Payload root (module/webui/control_api.py -> module/webui -> module -> root).
# cwd-independent on purpose: the webui process cwd is the payload root today,
# but the request file must land next to config/ regardless.
REPO_ROOT = Path(__file__).resolve().parents[2]
# Signal channel to the launcher (src/update_restart.rs watcher): the
# launcher runs no HTTP server, so the webui drops a file and the watcher
# consumes it. Cleared by setup_alas_repo at startup, so only requests made
# while THIS launcher instance lives are honored.
UPDATE_RESTART_REQUEST = REPO_ROOT / "config" / ".update-restart-request"


def _locked() -> bool:
    """Webui password/SSL configured -> refuse (launcher degrades client-side too).

    MUST stay in lockstep with the Rust-side ws_control_available predicate
    (src/deploy_config.rs): any non-null, non-empty value in Password /
    WebuiSSLKey / WebuiSSLCert counts as locked — the truthy check here
    (bool(123) == True for an unquoted YAML number) is the contract; the Rust
    side treats non-string values the same way. R2 audit: WebuiSSLCert was
    missing here; R3 audit: the non-string semantics were one-sided."""
    cfg = State.deploy_config
    return bool(
        getattr(cfg, "Password", None)
        or getattr(cfg, "WebuiSSLKey", None)
        or getattr(cfg, "WebuiSSLCert", None)
    )


def _local_host(request: Request) -> bool:
    """Round-3/4：拒绝非本机 Host（封远端/LAN 访问 API）+ Origin 校验（封本地跨源驱动）。
    Host 头永远是请求目标主机（浏览器 fetch 127.0.0.1 → Host: 127.0.0.1），挡不住跨源；
    真正拦跨源的是 Origin：跨源 POST 时浏览器必带 Origin（简单请求无 preflight），
    非本机 Origin 拒绝；本机 webui 同源 fetch 与启动器客户端（无 Origin）均放行。"""
    host = request.headers.get("host", "").split(":")[0].strip("[]")  # [::1] → ::1
    if host not in ("127.0.0.1", "localhost", "::1"):
        return False
    origin = request.headers.get("origin", "")
    if origin:
        from urllib.parse import urlparse
        return urlparse(origin).hostname in ("127.0.0.1", "localhost", "::1")
    return True


@router.get("/api/alas/instances")
async def instances(request: Request):
    # Read-only endpoint: intentionally NOT locked — the launcher's death
    # detection stays usable even with a password/SSL configured. Only the
    # mutating endpoints (start/stop) require the lock. NOTE: the PR's
    # PasswordGate middleware (api/__init__.py) wraps every route when a
    # password is set, so this endpoint 401s in that case too — the launcher
    # already degrades client-side when a password is configured.
    out = []
    for name in alas_instance():
        pm = ProcessManager.get_manager(name)
        out.append({"name": name, "state": pm.state})
    return JSONResponse(out)


@router.get("/api/alas/protocol")
async def protocol(request: Request):
    # Read-only handshake: the launcher compares PROTOCOL_VERSION against the
    # version it shipped. Not locked (mirrors /instances).
    return JSONResponse({"version": PROTOCOL_VERSION})


@router.post("/api/alas/app/update-restart")
async def update_restart(request: Request):
    """Ask the launcher to git-pull the payload and restart the backend.

    Used by the webui updater tab (更新器): the launcher re-runs its startup
    setup (pull + sparse checkout + control patch) between stop and start,
    so a restart IS the update. File write failure is a 500 — the tab shows
    it; nothing else is affected."""
    if _locked() or not _local_host(request):
        return JSONResponse({"error": "locked"}, status_code=401)
    try:
        UPDATE_RESTART_REQUEST.parent.mkdir(parents=True, exist_ok=True)
        UPDATE_RESTART_REQUEST.write_text(
            json.dumps({"pid": os.getpid(), "time": time.time()}), encoding="utf-8"
        )
    except OSError as e:
        return JSONResponse({"error": f"request file write failed: {e}"}, status_code=500)
    return JSONResponse({"ok": True, "version": PROTOCOL_VERSION})


@router.post("/api/alas/{name}/scheduler/start")
async def start(request: Request, name: str):
    if _locked() or not _local_host(request):
        return JSONResponse({"error": "locked"}, status_code=401)
    if name not in alas_instance():  # Round-2 SHOULD-FIX：拒绝任意名，防创建幻影 manager
        return JSONResponse({"error": "unknown instance"}, status_code=404)
    pm = ProcessManager.get_manager(name)
    pm.start(None)  # PR-5885 的 start(func, ev=None)：router 惯例不传 ev
    return JSONResponse({"name": name, "state": pm.state})


@router.post("/api/alas/{name}/scheduler/stop")
async def stop(request: Request, name: str):
    if _locked() or not _local_host(request):
        return JSONResponse({"error": "locked"}, status_code=401)
    if name not in alas_instance():
        return JSONResponse({"error": "unknown instance"}, status_code=404)
    pm = ProcessManager.get_manager(name)
    pm.stop()
    return JSONResponse({"name": name, "state": pm.state})
