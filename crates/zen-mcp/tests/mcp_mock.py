#!/usr/bin/env python3
"""Mock MCP server for zen-mcp transport tests (stdio).

Speaks newline-delimited JSON-RPC on stdin/stdout, the same framing the
existing write_fake_server() helper in src/tests.rs uses (proven to work
with the rmcp TokioChildProcess transport in this repo).

Usage:
    python mcp_mock.py <mode> <log_path> <env_dump_path>

Modes:
    ok              - normal initialize + tools/list + tools/call echo
    env_dump        - like ok, plus writes os.environ to env_dump_path
    input_required  - tools/call returns resultType "input_required"
    task            - tools/call returns resultType "task"
    image           - tools/call returns an image content block
    audio           - tools/call returns an audio content block
    resource_link   - tools/call returns a resource_link content block
    resource        - tools/call returns an embedded resource block
    paged           - tools/list paginates across two pages
    hang            - tools/call sleeps 120s (in-flight request tests)
    spawn_worker    - tools/call spawns a long-lived grandchild, writes its PID
"""

import sys
import os
import json
import time


def log(path, method):
    try:
        with open(path, "a") as f:
            f.write(method + "\n")
    except OSError:
        pass


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


mode = sys.argv[1] if len(sys.argv) > 1 else "ok"
log_path = sys.argv[2] if len(sys.argv) > 2 else None
env_dump = sys.argv[3] if len(sys.argv) > 3 else None

# Dump the environment immediately: the child may be killed before EOF, so
# writing at exit is not reliable.
if log_path:
    try:
        with open(log_path + ".pid", "w") as f:
            f.write(str(os.getpid()))
    except OSError:
        pass

if env_dump:
    try:
        with open(env_dump, "w") as f:
            for k in sorted(os.environ.keys()):
                f.write("{}={}\n".format(k, os.environ[k]))
    except OSError:
        pass

TOOLS = [
    {
        "name": "echo",
        "description": "echoes input",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
    },
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue

    method = msg.get("method", "")
    mid = msg.get("id")
    if log_path:
        log(log_path, method)

    if method == "initialize":
        params = msg.get("params") or {}
        # Echo back whatever protocol version the client asked for so the two
        # sides always agree (the MRTR/tasks results are gated on the
        # negotiated version).
        send({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "1"},
            },
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        cursor = (msg.get("params") or {}).get("cursor")
        if mode == "paged":
            if not cursor:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "tools": [{
                            "name": "alpha",
                            "description": "first tool",
                            "inputSchema": {"type": "object"},
                        }],
                        "nextCursor": "page2",
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "tools": [{
                            "name": "beta",
                            "description": "second tool",
                            "inputSchema": {"type": "object"},
                        }],
                    },
                })
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}

        if mode == "spawn_worker":
            # Spawn a long-lived grandchild and record its PID so a test can
            # assert Transport::kill() tears down the whole process group.
            import subprocess, time as _t
            worker = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(300)"],
                start_new_session=False,
            )
            try:
                with open(log_path + ".worker.pid", "w") as f:
                    f.write(str(worker.pid))
            except OSError:
                pass
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {"content": [{"type": "text", "text": "spawned"}]},
            })
        elif mode == "hang":
            # Hold the response so a test can keep its own Arc<Session> alive
            # while calling Transport::kill().
            time.sleep(120)
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {"content": [{"type": "text", "text": "late"}]},
            })
        elif mode == "input_required":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "resultType": "input_required",
                    "inputRequests": {},
                    "requestState": "opaque",
                },
            })
        elif mode == "task":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "resultType": "task",
                    "taskId": "t1",
                    "status": "completed",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "lastUpdatedAt": "2026-01-01T00:00:00Z",
                    "ttlMs": None,
                },
            })
        elif mode == "image" or name == "image":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{
                        "type": "image",
                        "data": "abc",
                        "mimeType": "image/png",
                    }],
                },
            })
        elif mode == "audio" or name == "audio":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{
                        "type": "audio",
                        "data": "abc",
                        "mimeType": "audio/wav",
                    }],
                },
            })
        elif mode == "resource_link" or name == "resource_link":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{
                        "type": "resource_link",
                        "uri": "file:///x",
                        "name": "x",
                    }],
                },
            })
        elif mode == "resource" or name == "resource":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [{
                        "type": "resource",
                        "resource": {
                            "uri": "file:///x",
                            "mimeType": "text/plain",
                            "text": "hi",
                        },
                    }],
                },
            })
        else:
            text = args.get("text", "")
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "content": [
                        {"type": "text", "text": "echo: " + str(text)},
                        {"type": "text", "text": "second part"},
                    ],
                },
            })
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method in ("shutdown", "exit"):
        break
    else:
        if mid is not None:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32601, "message": "unknown method"},
            })
