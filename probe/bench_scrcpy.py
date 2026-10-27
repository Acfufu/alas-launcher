#!/usr/bin/env python3
"""scrcpy trial v2 — patch prototype as a bench.

Patches (the proposed payload change, applied here via monkeypatch):
  1. ScrcpyOptions.command_v120: control='false'  -> server skips Device
     clipboard-listener init, which is what crashes on Android 16
     (NoSuchMethodException IClipboard.addPrimaryClipChangedListener).
  2. _scrcpy_server_start still opens BOTH connections: 1.20's
     DesktopConnection.open always accepts video then control even with
     control=false (device meta + video stream start only after both). The
     control connection is an idle placeholder — we never send a control
     byte, so no controller / clipboard listener runs server-side.

Everything else runs the real payload stack. Prints per-call frame latency.
"""
import sys
import time
import os
import json
import struct

_PROBE_CWD = os.getcwd()
APP = "/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript"
os.chdir(APP)
sys.path.insert(0, APP)

import numpy as np

from module.config.config import AzurLaneConfig
from module.device.method.scrcpy import Scrcpy
from module.device.method.scrcpy.options import ScrcpyOptions

_orig_command = ScrcpyOptions.command_v120.__func__


def command_v120_noclip(cls, jar_path="/data/local/tmp/scrcpy-server.jar"):
    parts = _orig_command(cls, jar_path)
    # args per command_v120: [..., tunnel_forward, crop, send_frame_meta,
    # control, display_id, ...] -> control is index 13 of the list
    parts[13] = "false"
    return parts


ScrcpyOptions.command_v120 = classmethod(command_v120_noclip)


class Bench(Scrcpy):
    def _scrcpy_server_start(self):
        # copy of payload start with the control-socket section removed
        from adbutils import Network
        from module.base.timer import Timer
        from module.device.method.scrcpy.core import ScrcpyError
        from module.device.method.utils import recv_all
        from module.logger import logger
        import socket as _socket

        logger.hr("Scrcpy server start")
        commands = ScrcpyOptions.command_v120(self.config.SCRCPY_FILEPATH_REMOTE)
        self._scrcpy_server_stream = self.adb.shell(commands, stream=True)
        self._scrcpy_server_stream.conn.settimeout(3)

        ret = self._scrcpy_server_stream.read(10)
        if b"Aborted" in ret:
            raise ScrcpyError("Aborted")
        if ret == b"[server] E":
            ret += recv_all(self._scrcpy_server_stream)
            logger.error(ret)
            raise ScrcpyError("Unknown scrcpy error")
        else:
            ret += self._scrcpy_receive_from_server_stream()
            logger.info(ret)

        logger.info("Create video socket")
        timeout = Timer(3).start()
        while 1:
            if timeout.reached():
                raise ScrcpyError("Connect scrcpy-server timeout")
            try:
                self._scrcpy_video_socket = self.adb.create_connection(Network.LOCAL_ABSTRACT, "scrcpy")
                self._scrcpy_video_socket.settimeout(3)
                break
            except Exception:  # noqa: BLE001
                time.sleep(0.1)

        dummy_byte = self._scrcpy_video_socket.recv(1)
        if not len(dummy_byte) or dummy_byte != b"\x00":
            raise ScrcpyError("Did not receive Dummy Byte from video stream")

        logger.info("Create control socket (idle placeholder — required for 1.20 handshake)")
        self._scrcpy_control_socket = self.adb.create_connection(Network.LOCAL_ABSTRACT, "scrcpy")
        self._scrcpy_control_socket.settimeout(3)

        try:
            device_name = self._scrcpy_video_socket.recv(64).decode("utf-8").rstrip("\x00")
            ret = self._scrcpy_video_socket.recv(4)
            self._scrcpy_resolution = struct.unpack(">HH", ret)
            logger.attr("Scrcpy Device", device_name)
        except _socket.timeout:
            # Not expected with the placeholder connection; keep a seed so
            # resolution_check survives a device that skips meta anyway.
            logger.info("No device meta — seed resolution (fallback)")
            self._scrcpy_resolution = (1280, 720)
        logger.attr("Scrcpy Resolution", self._scrcpy_resolution)

        self._scrcpy_video_socket.setblocking(False)
        self._scrcpy_alive = True

        from threading import Thread
        self._scrcpy_stream_loop_thread = Thread(target=self._scrcpy_stream_loop, daemon=True)
        self._scrcpy_stream_loop_thread.start()
        while not (self._scrcpy_stream_loop_thread and self._scrcpy_stream_loop_thread.is_alive()):
            self.sleep(0.001)
        logger.info("Scrcpy server is up")


def measure(cfg, json_out=None):
    b = Bench(cfg)

    t0 = time.time()
    b.scrcpy_init()
    print(f"scrcpy_init ok in {time.time()-t0:.1f}s", flush=True)

    # wait for first decoded frame (static screens can keep the encoder
    # silent for seconds; game-foreground scenes start streaming immediately)
    t0 = time.time()
    while time.time() - t0 < 30 and b._scrcpy_last_frame is None:
        time.sleep(0.05)
    f = b._scrcpy_last_frame
    if f is None:
        print("FAIL: no decoded frame in 30s (server alive but encoder silent?)")
        # drain whatever the server put on its shell stream before dying —
        # that's where AssertionError / encoder stack traces land
        tail = b""
        try:
            while len(tail) < 8192:
                chunk = b._scrcpy_server_stream.read(4096)
                if not chunk:
                    break
                tail += chunk
        except Exception:  # noqa: BLE001
            pass
        if tail:
            print("server stream tail: " + tail[-800:].decode("utf-8", "replace"))
        if json_out:
            with open(json_out, "w") as fp:
                json.dump({"bench": "scrcpy", "error": "no_decoded_frame"}, fp)
        return 2
    print(f"first frame shape={f.shape} mean={float(f.mean()):.1f}", flush=True)

    lat = []
    for _ in range(60):
        t1 = time.perf_counter()
        b.screenshot_scrcpy()
        lat.append((time.perf_counter() - t1) * 1000)
    p = np.percentile(lat, [50, 90, 99])
    print(f"per_call_latency p50={p[0]:.0f}ms p90={p[1]:.0f}ms p99={p[2]:.0f}ms mean={np.mean(lat):.0f}ms n={len(lat)}")
    out = {
        "bench": "scrcpy",
        "control": "false",
        "calls": len(lat),
        "ms_p50": round(float(p[0]), 1),
        "ms_p90": round(float(p[1]), 1),
        "ms_p99": round(float(p[2]), 1),
        "ms_avg": round(float(np.mean(lat)), 1),
    }
    print(json.dumps(out))
    if json_out:
        with open(json_out, "w") as fp:
            json.dump(out, fp)
    print("OK")
    return 0


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()
    json_out = args.json_out
    if json_out and not os.path.isabs(json_out):
        json_out = os.path.join(_PROBE_CWD, json_out)

    cfg = AzurLaneConfig("alas")
    # AzurLaneConfig persists on every setattr (the "memory only" comment was
    # wrong and left the deployed config on scrcpy). Snapshot and restore.
    old = cfg.Emulator_ScreenshotMethod
    cfg.Emulator_ScreenshotMethod = "scrcpy"
    try:
        return measure(cfg, json_out)
    finally:
        cfg.Emulator_ScreenshotMethod = old


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001
        import traceback

        traceback.print_exc()
        print(f"FAIL: {type(e).__name__}: {e}")
        sys.exit(2)
