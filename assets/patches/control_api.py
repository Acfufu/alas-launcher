# module/webui/control_api.py
# Added by alas-launcher (GPLv3). Control API for scheduler start/stop.
# Mirrors the webui button path: ProcessManager.get_manager(name).start/stop.
from starlette.responses import JSONResponse

from module.config.utils import alas_instance
from module.webui.process_manager import ProcessManager
from module.webui.setting import State
from module.webui.updater import updater


def _locked() -> bool:
    """Webui password/SSL configured -> refuse (launcher degrades client-side too)."""
    cfg = State.deploy_config
    return bool(getattr(cfg, "Password", None) or getattr(cfg, "WebuiSSLKey", None))


def _local_host(request) -> bool:
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


async def _instances(_request):
    # Read-only endpoint: intentionally NOT locked — the launcher's death
    # detection stays usable even with a password/SSL configured. Only the
    # mutating endpoints (start/stop) require the lock.
    out = []
    for name in alas_instance():
        pm = ProcessManager.get_manager(name)
        out.append({"name": name, "state": pm.state})
    return JSONResponse(out)


async def _start(request):
    if _locked() or not _local_host(request):
        return JSONResponse({"error": "locked"}, status_code=401)
    name = request.path_params["name"]
    if name not in alas_instance():  # Round-2 SHOULD-FIX：拒绝任意名，防创建幻影 manager
        return JSONResponse({"error": "unknown instance"}, status_code=404)
    pm = ProcessManager.get_manager(name)
    pm.start(None, updater.event)
    return JSONResponse({"name": name, "state": pm.state})


async def _stop(request):
    if _locked() or not _local_host(request):
        return JSONResponse({"error": "locked"}, status_code=401)
    name = request.path_params["name"]
    if name not in alas_instance():
        return JSONResponse({"error": "unknown instance"}, status_code=404)
    pm = ProcessManager.get_manager(name)
    pm.stop()
    return JSONResponse({"name": name, "state": pm.state})


def control_routes():
    from starlette.routing import Route

    return [
        Route("/api/alas/instances", _instances, methods=["GET"]),
        Route("/api/alas/{name}/scheduler/start", _start, methods=["POST"]),
        Route("/api/alas/{name}/scheduler/stop", _stop, methods=["POST"]),
    ]
