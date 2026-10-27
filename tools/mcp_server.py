#!/usr/bin/env python3
"""Minimal MCP server for the ALAS launcher control API (direction-9,
2026-10-28 decision: local-only, layered on the control API).

Transport: stdio, one JSON-RPC 2.0 message per line (MCP stdio framing).
Scope: localhost control API only (127.0.0.1:<port>, default 22267). The
server itself binds nothing — it is a client of the webui's API, which
already refuses non-local hosts; running this on a remote machine simply
fails to connect. No token layer until remote access exists (decision 9).

Tools:
  alas_instances                     GET  /api/alas/instances
  alas_scheduler_start {name}        POST /api/alas/{name}/scheduler/start
  alas_scheduler_stop  {name}        POST /api/alas/{name}/scheduler/stop
  alas_notify {title, content}       POST /api/alas/notify      (v3+)

Usage (e.g. in an MCP client config):
  python3 /path/to/tools/mcp_server.py [--port 22267]
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request

PROTOCOL_VERSION = "2024-11-05"
SERVER_INFO = {"name": "alas-launcher-control", "version": "0.1.0"}


def http_json(port: int, method: str, path: str, body: dict | None = None) -> tuple[int, object]:
    url = f"http://127.0.0.1:{port}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"Content-Type": "application/json"} if data else {},
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, json.loads(resp.read().decode() or "null")
    except urllib.error.HTTPError as e:
        try:
            payload = json.loads(e.read().decode() or "null")
        except Exception:  # noqa: BLE001
            payload = None
        return e.code, payload
    except urllib.error.URLError as e:
        return 0, f"connection failed (is the ALAS backend running?): {e.reason}"


TOOLS = [
    {
        "name": "alas_instances",
        "description": "List ALAS instances and scheduler states (state 1=running, 2=stopped, 3=abnormal).",
        "inputSchema": {"type": "object", "properties": {}, "required": []},
    },
    {
        "name": "alas_scheduler_start",
        "description": "Start the scheduler of an ALAS instance by name (e.g. 'alas').",
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
    },
    {
        "name": "alas_scheduler_stop",
        "description": "Stop the scheduler of an ALAS instance by name.",
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
    },
    {
        "name": "alas_notify",
        "description": "Push a notification through the payload's onepush channel (requires control API v3+ and a configured Error.OnePushConfig).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["content"],
        },
    },
]


def call_tool(port: int, name: str, args: dict) -> tuple[bool, str]:
    if name == "alas_instances":
        status, payload = http_json(port, "GET", "/api/alas/instances")
        return status == 200, json.dumps(payload, ensure_ascii=False)
    if name in ("alas_scheduler_start", "alas_scheduler_stop"):
        instance = str(args.get("name", "")).strip()
        if not instance:
            return False, "missing required argument: name"
        action = "start" if name.endswith("start") else "stop"
        status, payload = http_json(port, "POST", f"/api/alas/{instance}/scheduler/{action}")
        return status == 200, json.dumps(payload, ensure_ascii=False)
    if name == "alas_notify":
        content = str(args.get("content", "")).strip()
        if not content:
            return False, "missing required argument: content"
        status, payload = http_json(
            port,
            "POST",
            "/api/alas/notify",
            {"title": str(args.get("title", "MCP")), "content": content},
        )
        if status == 404:
            return False, "control API predates /api/alas/notify (needs protocol v3+)"
        return status == 200, json.dumps(payload, ensure_ascii=False)
    return False, f"unknown tool: {name}"


def handle(port: int, msg: dict) -> dict | None:
    method = msg.get("method", "")
    msg_id = msg.get("id")
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": SERVER_INFO,
            },
        }
    if method == "notifications/initialized":
        return None  # notification — no response
    if method == "ping":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {}}
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}
    if method == "tools/call":
        params = msg.get("params", {})
        ok, text = call_tool(port, str(params.get("name", "")), dict(params.get("arguments", {})))
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "content": [{"type": "text", "text": text}],
                "isError": not ok,
            },
        }
    if msg_id is not None:
        return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": f"method not found: {method}"}}
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=22267, help="control API port (default 22267)")
    args = parser.parse_args()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        response = handle(args.port, msg)
        if response is not None:
            sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
