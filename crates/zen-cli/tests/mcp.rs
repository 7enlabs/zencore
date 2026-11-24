mod common;

use common::*;

fn fake_mcp_server_script() -> String {
    r#"import sys, json, asyncio

async def main():
    # Read JSON-RPC requests from stdin, one per line
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = req.get("method")
        id = req.get("id")

        if method == "initialize":
            # Respond to initialize
            resp = {
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1.0.0"}
                }
            }
        elif method == "initialized":
            # Notification, no response needed
            continue
        elif method == "tools/list":
            resp = {
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo the input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "message": {"type": "string"}
                            },
                            "required": ["message"]
                        }
                    }]
                }
            }
        elif method == "tools/call":
            params = req.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {})
            if name == "echo":
                message = args.get("message", "")
                resp = {
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": f"ECHO: {message}"}],
                        "isError": False
                    }
                }
            else:
                resp = {
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": f"Unknown tool: {name}"}
                }
        else:
            resp = {
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": f"Unknown method: {method}"}
            }

        print(json.dumps(resp), flush=True)

if __name__ == "__main__":
    asyncio.run(main())
"#
    .to_string()
}

fn write_mcp_server_script(dir: &TempDir) -> std::path::PathBuf {
    dir.write("mcp_fake_server.py", &fake_mcp_server_script())
}

// `python3` does not exist on a stock Windows install (only `python`).
fn python_cmd() -> &'static str {
    let ok = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    if ok { "python3" } else { "python" }
}

// Build the CLI MCP config with real JSON serialization: interpolating a
// raw Windows path (C:\...) into a format! string injects invalid JSON
// escapes (\U, ...) and the whole config is rejected, leaving 0 servers.
fn mcp_config_json(server_path: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            "fake": { "command": python_cmd(), "args": [server_path] }
        }
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_connected_and_tool_works() {
    let home = TempDir::new("mcp_ok_home");
    let cwd = TempDir::new("mcp_ok_cwd");

    let server_script = write_mcp_server_script(&cwd);
    let server_path = server_script.to_string_lossy().to_string();
    let mcp_config = mcp_config_json(&server_path);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "mcp__fake__echo", "arguments": "{\"message\":\"hello mcp\"}"}
            }])),
        ),
        Reply::text("mcp tool worked"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_ok", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--bare",
                "--permission-mode",
                "bypassPermissions",
                "--mcp-config",
                &mcp_config,
            ])
            .mock("mock-model", &server)
            .arg("call mcp echo")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let lines = ndjson(&stdout);
    let init = &lines[0];
    assert_eq!(init["type"], "system");
    assert_eq!(init["subtype"], "init");

    let mcp_servers = init["mcp_servers"].as_array().expect("mcp_servers array");
    assert!(
        !mcp_servers.is_empty(),
        "mcp_servers should list our fake server"
    );
    let fake_server = mcp_servers
        .iter()
        .find(|s| s["name"] == "fake")
        .expect("fake server in list");
    assert_eq!(
        fake_server["status"], "connected",
        "fake server should be connected: {}",
        fake_server
    );

    let tools: Vec<String> = init["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tools.contains(&"mcp__fake__echo".to_string()),
        "MCP tool should be in tools list: {:?}",
        tools
    );

    let v = last_result(&stdout);
    assert_eq!(v["result"], "mcp tool worked");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("ECHO: hello mcp"),
        "MCP tool result should be in model request: {}",
        second_req
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_failed_shows_failed_status() {
    let home = TempDir::new("mcp_fail_home");
    let cwd = TempDir::new("mcp_fail_cwd");

    let mcp_config = r#"{"mcpServers":{"broken":{"command":"nonexistent-command-xyz","args":[]}}}"#;

    let server = MockServer::single_text("ok").await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_fail", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--bare",
                "--mcp-config",
                mcp_config,
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let lines = ndjson(&stdout);
    let init = &lines[0];
    let mcp_servers = init["mcp_servers"].as_array().expect("mcp_servers array");
    let broken_server = mcp_servers
        .iter()
        .find(|s| s["name"] == "broken")
        .expect("broken server in list");
    assert_eq!(
        broken_server["status"], "failed",
        "broken server should show failed: {}",
        broken_server
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_mcp_servers_mixed_status() {
    let home = TempDir::new("mcp_multi_home");
    let cwd = TempDir::new("mcp_multi_cwd");

    let server_script = write_mcp_server_script(&cwd);
    let server_path = server_script.to_string_lossy().to_string();
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "good": { "command": python_cmd(), "args": [server_path] },
            "bad": { "command": "nonexistent", "args": [] }
        }
    })
    .to_string();

    let server = MockServer::single_text("ok").await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_multi", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--bare",
                "--mcp-config",
                &mcp_config,
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let lines = ndjson(&stdout);
    let init = &lines[0];
    let mcp_servers = init["mcp_servers"].as_array().expect("mcp_servers array");

    let good = mcp_servers
        .iter()
        .find(|s| s["name"] == "good")
        .expect("good server");
    assert_eq!(good["status"], "connected");

    let bad = mcp_servers
        .iter()
        .find(|s| s["name"] == "bad")
        .expect("bad server");
    assert_eq!(bad["status"], "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_mcp_config_ignores_file_configs() {
    let home = TempDir::new("mcp_strict_home");
    let cwd = TempDir::new("mcp_strict_cwd");

    cwd.write(
        ".mcp.json",
        r#"{"mcpServers":{"project":{"command":"python3","args":["/nonexistent.py"]}}}"#,
    );

    let server_script = write_mcp_server_script(&cwd);
    let server_path = server_script.to_string_lossy().to_string();
    let cli_mcp_config = mcp_config_json(&server_path);

    let server = MockServer::single_text("ok").await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_strict", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--bare",
                "--strict-mcp-config",
                "--mcp-config",
                &cli_mcp_config,
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let lines = ndjson(&stdout);
    let init = &lines[0];
    let mcp_servers = init["mcp_servers"].as_array().expect("mcp_servers array");

    assert_eq!(
        mcp_servers.len(),
        1,
        "strict mode should only have CLI server: {:?}",
        mcp_servers
    );
    assert_eq!(mcp_servers[0]["name"], "fake");
    assert_eq!(mcp_servers[0]["status"], "connected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c7_3_invalid_entries_reported_in_init() {
    let home = TempDir::new("mcp_inv_home");
    let cwd = TempDir::new("mcp_inv_cwd");
    let script = write_mcp_server_script(&cwd);
    let cfg = serde_json::json!({
        "mcpServers": {
            "good": { "command": python_cmd(), "args": [script.display().to_string()] },
            "nourl": { "url": "http://127.0.0.1:9/mcp" },
            "odd": { "type": "carrier-pigeon", "url": "x" }
        }
    })
    .to_string();
    let server = MockServer::single_text("ok").await;
    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_inv", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--bare",
                "--mcp-config",
                &cfg,
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");
    assert_eq!(code, 0, "stderr: {}", stderr);
    let init = &ndjson(&stdout)[0];
    let servers = init["mcp_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "{}", init);
    assert_eq!(servers[0]["name"], "good");
    assert_eq!(servers[0]["status"], "connected");
    let mut errs: Vec<(String, String)> = init["mcp_server_errors"]
        .as_array()
        .expect("mcp_server_errors")
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().into(),
                e["reason"].as_str().unwrap().into(),
            )
        })
        .collect();
    errs.sort();
    assert_eq!(
        errs,
        vec![
            ("nourl".into(), "url_missing_type".into()),
            ("odd".into(), "unknown_type".into())
        ]
    );

    assert!(!stderr.contains("skipped"), "stderr: {}", stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c7_5_mcp_subcommands_round_trip() {
    let home = TempDir::new("mcp_cmd_home");
    let cwd = TempDir::new("mcp_cmd_cwd");
    let script = write_mcp_server_script(&cwd);
    let run = |args: Vec<String>| ZenRun::new("mcp_cmd", &home, &cwd).args(args).run_output();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    let py = python_cmd();
    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "-e",
        "K=V",
        "fake",
        "--",
        py,
        "${ZEN_TEST_NOPE:-SCRIPT}",
    ]));
    assert_eq!(code, 0, "{}", err);
    let local: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.path().join(".zen/mcp.json")).unwrap())
            .unwrap();
    assert_eq!(local["mcpServers"]["fake"]["env"]["K"], "V");

    let mcp_path = cwd.path().join(".zen/mcp.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
    let args = doc["mcpServers"]["fake"]["args"]
        .as_array_mut()
        .expect("fake args");
    for a in args.iter_mut() {
        if let Some(v) = a.as_str()
            && v.contains("SCRIPT")
        {
            *a = serde_json::Value::String(v.replace("SCRIPT", &script.display().to_string()));
        }
    }
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "-s",
        "project",
        "--header",
        "Authorization: Bearer t",
        "web",
        "http://127.0.0.1:9/mcp",
    ]));
    assert_eq!(code, 0, "{}", err);
    let (out, err, code) = run(s(&["mcp", "get", "web"]));
    assert_eq!(code, 0, "{}", err);
    let web: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(web["type"], "http");
    assert_eq!(web["headers"]["Authorization"], "Bearer t");
    let (_, _, code) = run(s(&["mcp", "get", "nope"]));
    assert_ne!(code, 0);
    let (_, _, code) = run(s(&["mcp", "add-json", "bad", "[1,2]"]));
    assert_ne!(code, 0, "non-object JSON must be rejected");
    let (_, _, code) = run(s(&["mcp", "add", "--transport", "smoke", "x", "y"]));
    assert_ne!(code, 0, "unknown transport must be rejected");

    let server = MockServer::single_text("ok").await;
    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("mcp_cmd", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--setting-sources",
                "",
            ])
            .env("MCP_TIMEOUT", Some("5000"))
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");
    assert_eq!(code, 0, "stderr: {}", stderr);
    let init = &ndjson(&stdout)[0];
    let st: Vec<(String, String)> = init["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().into(),
                e["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        st.contains(&("fake".into(), "connected".into())),
        "{:?}",
        st
    );
    assert!(st.contains(&("web".into(), "failed".into())), "{:?}", st);
    assert!(
        init["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "mcp__fake__echo")
    );

    let (_, err, code) = run(s(&["mcp", "remove", "fake"]));
    assert_eq!(code, 0, "{}", err);
    let (_, _, code) = run(s(&["mcp", "get", "-s", "local", "fake"]));
    assert_ne!(code, 0);
}

// Issue #275: `zen mcp add` swallowed `--scope` written after the server
// name into the server's args and wrote to the wrong (local) scope.
#[test]
fn mcp_add_scope_flag_after_name_is_rejected() {
    let home = TempDir::new("mcp_275_home");
    let cwd = TempDir::new("mcp_275_cwd");
    let run = |args: Vec<String>| ZenRun::new("mcp_275", &home, &cwd).args(args).run_output();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // Flag before the name: honoured, server lands in the project file.
    let (_, err, code) = run(s(&[
        "mcp", "add", "--scope", "project", "projA", "echo", "hij",
    ]));
    assert_eq!(code, 0, "{err}");
    let project: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.path().join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        project["mcpServers"]["projA"]["args"],
        serde_json::json!(["hij"])
    );
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // Flag after the name: rejected, nothing written anywhere.
    let (_, err, code) = run(s(&[
        "mcp", "add", "projB", "echo", "hij", "--scope", "project",
    ]));
    assert_ne!(code, 0, "swallowed --scope must fail, stderr: {err}");
    assert!(
        err.contains("--scope"),
        "stderr should name the flag: {err}"
    );
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // Invalid scope after the name: rejected like before the name.
    let (_, _, code) = run(s(&["mcp", "add", "--scope", "racxyz", "a1", "echo", "hi"]));
    assert_ne!(code, 0);
    let (_, err, code) = run(s(&["mcp", "add", "a2", "echo", "hi", "--scope", "racxyz"]));
    assert_ne!(code, 0, "swallowed invalid scope must fail, stderr: {err}");
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // Short -s form after the name: rejected too.
    let (_, _, code) = run(s(&["mcp", "add", "projC", "echo", "hi", "-s", "project"]));
    assert_ne!(code, 0);
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // Attached forms after the name: rejected too (issue #275 follow-up).
    let (_, err, code) = run(s(&["mcp", "add", "projD", "echo", "hi", "--scope=project"]));
    assert_ne!(
        code, 0,
        "swallowed --scope=project must fail, stderr: {err}"
    );
    assert!(
        err.contains("--scope=project"),
        "stderr should name the flag: {err}"
    );
    assert!(!cwd.path().join(".zen/mcp.json").exists());
    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "projE",
        "echo",
        "hi",
        "--transport=stdio",
    ]));
    assert_ne!(
        code, 0,
        "swallowed --transport=stdio must fail, stderr: {err}"
    );
    assert!(
        err.contains("--transport=stdio"),
        "stderr should name the flag: {err}"
    );
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // The server's own flags still work.
    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "srv",
        "npx",
        "-y",
        "@modelcontextprotocol/server-filesystem",
        "/tmp",
    ]));
    assert_eq!(code, 0, "{err}");
    let local: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.path().join(".zen/mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        local["mcpServers"]["srv"]["args"],
        serde_json::json!(["-y", "@modelcontextprotocol/server-filesystem", "/tmp"])
    );

    // A literal `--` after the command escapes a server arg that looks
    // like a zen flag.
    let (_, err, code) = run(s(&[
        "mcp", "add", "srv2", "echo", "--", "--scope", "project",
    ]));
    assert_eq!(code, 0, "{err}");
    let local: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.path().join(".zen/mcp.json")).unwrap())
            .unwrap();
    assert_eq!(
        local["mcpServers"]["srv2"]["args"],
        serde_json::json!(["--", "--scope", "project"])
    );
}

#[test]
fn mcp_add_dry_run_writes_nothing() {
    let home = TempDir::new("mcp_dry_home");
    let cwd = TempDir::new("mcp_dry_cwd");
    let run = |args: Vec<String>| ZenRun::new("mcp_dry", &home, &cwd).args(args).run_output();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // Dry run: exit 0, preview printed, no config file created.
    let (out, err, code) = run(s(&["mcp", "add", "--dry-run", "srv", "echo", "hi"]));
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("would write"), "stdout: {out}");
    assert!(out.contains("mcpServers.srv"), "stdout: {out}");
    assert!(out.contains("no change made"), "stdout: {out}");
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // A real add still works, then dry-run remove previews without deleting.
    let (_, err, code) = run(s(&["mcp", "add", "srv", "echo", "hi"]));
    assert_eq!(code, 0, "stderr: {err}");
    let (out, err, code) = run(s(&["mcp", "remove", "--dry-run", "srv"]));
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("would remove MCP server"), "stdout: {out}");
    assert!(out.contains("was:"), "stdout: {out}");
    assert!(cwd.path().join(".zen/mcp.json").exists());

    // The real remove still deletes it.
    let (_, err, code) = run(s(&["mcp", "remove", "srv"]));
    assert_eq!(code, 0, "stderr: {err}");
    let (out, err, code) = run(s(&["mcp", "get", "srv"]));
    assert_ne!(code, 0, "server must be gone: {out} {err}");

    // Dry-run remove of a missing server keeps the current notice.
    let (_, err, code) = run(s(&["mcp", "remove", "--dry-run", "srv"]));
    assert_eq!(code, 0, "stderr: {err}");
    assert!(err.contains("no MCP server"), "stderr: {err}");
}

#[test]
fn mcp_add_dry_run_after_name_is_rejected() {
    let home = TempDir::new("mcp_dry_home2");
    let cwd = TempDir::new("mcp_dry_cwd2");
    let run = |args: Vec<String>| ZenRun::new("mcp_dry", &home, &cwd).args(args).run_output();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // --dry-run after the name is a server arg, not the flag: refuse it
    // instead of silently adding a server with a bogus arg.
    let (_, err, code) = run(s(&["mcp", "add", "srv", "echo", "hi", "--dry-run"]));
    assert_ne!(code, 0, "swallowed --dry-run must be rejected");
    assert!(err.contains("--dry-run"), "stderr: {err}");
    assert!(!cwd.path().join(".zen/mcp.json").exists());

    // ... but --dry-run after -- still belongs to the server command.
    let (_, err, code) = run(s(&["mcp", "add", "srv", "echo", "--", "--dry-run"]));
    assert_eq!(code, 0, "stderr: {err}");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.path().join(".zen/mcp.json")).unwrap())
            .unwrap();
    let args = doc["mcpServers"]["srv"]["args"].as_array().unwrap();
    assert!(args.iter().any(|a| a == "--dry-run"), "args: {args:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_add_duplicate_refuses_without_force() {
    // Issue #285: adding a server whose name already exists must fail
    // with exit 7 instead of silently replacing it; --force replaces.
    let home = TempDir::new("mcp_dup_home");
    let cwd = TempDir::new("mcp_dup_cwd");
    let run = |args: Vec<String>| ZenRun::new("mcp_dup", &home, &cwd).args(args).run_output();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mcp_path = cwd.path().join(".zen/mcp.json");
    let read_url = || {
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        doc["mcpServers"]["probe"]["url"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "probe",
        "http://127.0.0.1:8931/",
    ]));
    assert_eq!(code, 0, "{}", err);
    assert_eq!(read_url(), "http://127.0.0.1:8931/");

    let (_, err, code) = run(s(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "probe",
        "http://127.0.0.1:9999/",
    ]));
    assert_eq!(
        code, 7,
        "duplicate without --force must be a conflict: {}",
        err
    );
    assert!(err.contains("already exists"), "stderr: {}", err);
    // The conflict names the existing server, so the old URL must show.
    assert!(
        err.contains("http://127.0.0.1:8931/"),
        "stderr should name the existing server: {}",
        err
    );
    assert_eq!(read_url(), "http://127.0.0.1:8931/");

    let (out, err, code) = run(s(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "--force",
        "probe",
        "http://127.0.0.1:9999/",
    ]));
    assert_eq!(code, 0, "{}", err);
    assert!(out.contains("replaced"), "stdout: {}", out);
    assert_eq!(read_url(), "http://127.0.0.1:9999/");

    // Same for a stdio server: the conflict must name its command.
    let (_, err, code) = run(s(&["mcp", "add", "std1", "echo", "hi"]));
    assert_eq!(code, 0, "{}", err);
    let (_, err, code) = run(s(&["mcp", "add", "std1", "echo", "bye"]));
    assert_eq!(
        code, 7,
        "stdio duplicate without --force must be a conflict: {}",
        err
    );
    assert!(
        err.contains("command echo hi"),
        "stderr should name the existing stdio server: {}",
        err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_add_json_duplicate_refuses_without_force() {
    // Issue #315: `mcp add-json` on an existing server name must fail
    // with exit 7 instead of silently replacing it; --force replaces
    // (same as #285 for `mcp add`).
    let home = TempDir::new("mcp_add_json_dup_home");
    let cwd = TempDir::new("mcp_add_json_dup_cwd");
    let run = |args: Vec<String>| {
        ZenRun::new("mcp_add_json_dup", &home, &cwd)
            .args(args)
            .run_output()
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mcp_path = cwd.path().join(".zen/mcp.json");
    let read_command = || {
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        doc["mcpServers"]["probe"]["command"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let (out, err, code) = run(s(&[
        "mcp",
        "add-json",
        "probe",
        r#"{"type":"stdio","command":"echo"}"#,
    ]));
    assert_eq!(code, 0, "{}", err);
    assert!(out.contains("added"), "stdout: {}", out);
    assert_eq!(read_command(), "echo");

    let (_, err, code) = run(s(&[
        "mcp",
        "add-json",
        "probe",
        r#"{"type":"stdio","command":"cat"}"#,
    ]));
    assert_eq!(
        code, 7,
        "duplicate without --force must be a conflict: {}",
        err
    );
    assert!(err.contains("already exists"), "stderr: {}", err);
    // The conflict names the existing server, so the old command must show.
    assert!(
        err.contains("command echo"),
        "stderr should name the existing server: {}",
        err
    );
    assert_eq!(read_command(), "echo");

    let (out, err, code) = run(s(&[
        "mcp",
        "add-json",
        "--force",
        "probe",
        r#"{"type":"stdio","command":"cat"}"#,
    ]));
    assert_eq!(code, 0, "{}", err);
    assert!(out.contains("replaced"), "stdout: {}", out);
    assert_eq!(read_command(), "cat");
}
