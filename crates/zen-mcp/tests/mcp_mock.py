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
    cycle           - tools/list returns the same cursor forever (#377)
    endless         - tools/list returns a fresh cursor on every page (#377)
    pages100        - tools/list paginates across exactly 100 pages (#377)
    many_tools      - tools/list returns 10,001 tools in one page (#377)
    exact_tools     - tools/list returns exactly 10,000 tools in one page (#377)
    hang            - tools/call sleeps 120s (in-flight request tests)
    spawn_worker    - tools/call spawns a long-lived grandchild, writes its PID
    die_after_first_call - tools/list answers once, then the server exits
                      (regression aid for zencore #375: the client must
                      evict the dead cached session and reconnect)
    die_after_first_call also exits after one tools/call (same idea, for
                      the call path: the client must evict + retry with
                      the same arguments)
    hang_first_list - first tools/list sleeps 120s, later ones answer
                      (child stays alive, so the liveness probe cannot
                      heal: only evict-on-transport-error can)
    hang_first_call - first tools/call sleeps 120s, later ones answer
    die_first_spawn_after_first_list - first spawned server answers one
                      tools/list then exits; later spawns stay healthy
                      (concurrent-failure aid: retries share one fresh
                      server, so a stale evict must not kill it)
    die_after_first_call_then_list_error_once - spawn 1 dies after its
                      first list, spawn 2 always fails tools/list, spawn
                      3+ answer normally (probe aid: the single retry is
                      only enough if the probe runs before any attempt)
    die_after_first_call_then_list_error_twice - spawn 1 dies after its
                      first list, spawns 2 and 3 always fail tools/list,
                      spawn 4+ answer normally (retry-once aid: the
                      request must fail after exactly one retry)
    hang_first_list_then_refuse - first list hangs, and any later spawn
                      exits immediately at startup (reconnect-failure
                      aid: the client must report "reconnect failed")
    slow_init         - like ok, but waits 5 s before answering initialize
                      (regression aid for zencore #403: kill() during a
                      slow connect must not leak the fresh session)
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


def spawn_index():
    """1-based count of server processes started for this log path."""
    path = log_path + ".spawn" if log_path else None
    if not path:
        return 1
    try:
        with open(path) as f:
            n = int(f.read().strip() or 0)
    except (OSError, ValueError):
        n = 0
    n += 1
    try:
        with open(path, "w") as f:
            f.write(str(n))
    except OSError:
        pass
    return n


def once(path):
    """True the first time this path is seen, False afterwards."""
    if not path:
        return True
    try:
        with open(path) as f:
            f.read()
        return False
    except OSError:
        pass
    try:
        with open(path, "w") as f:
            f.write("1")
    except OSError:
        pass
    return True


# Modes that tell spawns apart need the spawn number up front. Spawns are
# serialized by the client's session lock, so the counter file never races.
_SPAWN_MODES = (
    "die_first_spawn_after_first_list",
    "die_after_first_call_then_list_error_once",
    "die_after_first_call_then_list_error_twice",
    "hang_first_list_then_refuse",
)
spawn_no = spawn_index() if mode in _SPAWN_MODES else 1
if mode == "hang_first_list_then_refuse" and spawn_no >= 2:
    # Regression aid for zencore #375: the reconnect target itself is
    # gone, so the client must report "reconnect failed" instead of
    # hanging or returning the raw first error.
    sys.exit(1)

TOOLS = [
    {
        "name": "echo",
        "description": "echoes input",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
    },
]

list_calls = [0]

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
        if mode == "slow_init":
            # Hold the handshake open so the test can kill() mid-connect.
            time.sleep(5)
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
        if mode == "cycle":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "tools": [{
                        "name": "looped",
                        "description": "loops forever",
                        "inputSchema": {"type": "object"},
                    }],
                    "nextCursor": "loop",
                },
            })
        elif mode == "endless":
            list_calls[0] += 1
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "tools": [{
                        "name": "t",
                        "description": "endless page",
                        "inputSchema": {"type": "object"},
                    }],
                    "nextCursor": "page-%d" % list_calls[0],
                },
            })
        elif mode == "pages100":
            list_calls[0] += 1
            if list_calls[0] < 100:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "tools": [{
                            "name": "tool-%d" % list_calls[0],
                            "description": "page tool",
                            "inputSchema": {"type": "object"},
                        }],
                        "nextCursor": "p%d" % list_calls[0],
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "tools": [{
                            "name": "tool-100",
                            "description": "last page tool",
                            "inputSchema": {"type": "object"},
                        }],
                    },
                })
        elif mode == "many_tools":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "tools": [
                        {
                            "name": "tool-%d" % i,
                            "description": "",
                            "inputSchema": {"type": "object"},
                        }
                        for i in range(10001)
                    ],
                },
            })
        elif mode == "exact_tools":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "tools": [
                        {
                            "name": "tool-%d" % i,
                            "description": "",
                            "inputSchema": {"type": "object"},
                        }
                        for i in range(10000)
                    ],
                },
            })
        elif mode == "paged":
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
        elif mode in ("hang_first_list", "hang_first_list_then_refuse") and once(
            log_path + ".once_list" if log_path else None
        ):
            # First list hangs: the client must time out (transport
            # error), evict, and reconnect. The child stays alive, so
            # the liveness probe cannot heal this; only eviction can.
            time.sleep(120)
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif mode == "die_first_spawn_after_first_list" and spawn_no == 1:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
            sys.stdout.flush()
            sys.exit(0)
        elif mode == "die_after_first_call_then_list_error_once" and spawn_no == 1:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
            sys.stdout.flush()
            sys.exit(0)
        elif mode == "die_after_first_call_then_list_error_once" and spawn_no == 2:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32603, "message": "list boom"},
            })
        elif mode == "die_after_first_call_then_list_error_twice" and spawn_no == 1:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
            sys.stdout.flush()
            sys.exit(0)
        elif mode == "die_after_first_call_then_list_error_twice" and spawn_no in (2, 3):
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32603, "message": "list boom"},
            })
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
            if mode == "die_after_first_call":
                # Regression aid for zencore #375: answer one call, then
                # die, so the client must evict the dead cached session
                # and reconnect instead of failing forever.
                sys.stdout.flush()
                sys.exit(0)
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
        elif mode == "hang_first_call" and once(
            log_path + ".once_call" if log_path else None
        ):
            # First call hangs: same evict isolation as hang_first_list,
            # on the call path. Later calls answer normally.
            time.sleep(120)
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
            if mode == "die_after_first_call":
                # Same one-shot death as for tools/list, for the call
                # path: answer one call, then die.
                sys.stdout.flush()
                sys.exit(0)
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
