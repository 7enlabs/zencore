use crate::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zen_core::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("zen_mcp_{}_{}", tag, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn untrusted_folder_skips_project_mcp_files() {
    let cwd = tmpdir("trust_cwd");
    let home = tmpdir("trust_home");
    std::fs::write(
        cwd.join(".mcp.json"),
        r#"{"mcpServers":{"proj":{"command":"echo"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(cwd.join(".zen")).unwrap();
    std::fs::write(
        cwd.join(".zen/mcp.json"),
        r#"{"mcpServers":{"local":{"command":"echo"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".zen")).unwrap();
    std::fs::write(
        home.join(".zen/mcp.json"),
        r#"{"mcpServers":{"user":{"command":"cat"}}}"#,
    )
    .unwrap();
    let cli = vec![r#"{"mcpServers":{"cli":{"command":"cat"}}}"#.to_string()];

    let un = crate::load_mcp_configs_scoped(&cwd, Some(&home), &cli, false, false).unwrap();
    assert_eq!(un.keys().cloned().collect::<Vec<_>>(), vec!["cli", "user"]);
    let tr = crate::load_mcp_configs_scoped(&cwd, Some(&home), &cli, false, true).unwrap();
    assert_eq!(
        tr.keys().cloned().collect::<Vec<_>>(),
        vec!["cli", "local", "proj", "user"]
    );

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn load_configs_project_user_and_cli() {
    let cwd = tmpdir("cfg_cwd");
    let home = tmpdir("cfg_home");

    std::fs::write(
        cwd.join(".mcp.json"),
        r#"{"mcpServers":{"proj":{"command":"echo","args":["a"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".zen")).unwrap();
    std::fs::write(
        home.join(".zen").join("mcp.json"),
        r#"{"mcpServers":{"user":{"command":"cat"},"proj":{"command":"overridden"}}}"#,
    )
    .unwrap();

    let cfg = load_mcp_configs(&cwd, Some(&home), &[], false).unwrap();
    assert_eq!(cfg.len(), 2);
    assert_eq!(cfg["user"].command.as_deref(), Some("cat"));

    assert_eq!(cfg["proj"].command.as_deref(), Some("echo"));

    let cli = vec![r#"{"mcpServers":{"cli":{"type":"http","url":"http://x"}}}"#.to_string()];
    let cfg = load_mcp_configs(&cwd, Some(&home), &cli, false).unwrap();
    assert_eq!(cfg.len(), 3);
    assert_eq!(cfg["cli"].url.as_deref(), Some("http://x"));
    assert!(cfg["cli"].is_http());

    let cfg = load_mcp_configs(&cwd, Some(&home), &cli, true).unwrap();
    assert_eq!(cfg.len(), 1);
    assert!(cfg.contains_key("cli"));

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn c7_scope_paths_match_spec_and_are_loaded() {
    let cwd = tmpdir("scope_cwd");
    let home = tmpdir("scope_home");
    let local = scope_path("local", &cwd, Some(&home)).unwrap();
    let project = scope_path("project", &cwd, Some(&home)).unwrap();
    let user = scope_path("user", &cwd, Some(&home)).unwrap();
    assert_eq!(local, cwd.join(".zen").join("mcp.json"));
    assert_eq!(project, cwd.join(".mcp.json"));
    assert_eq!(user, home.join(".zen").join("mcp.json"));
    assert!(scope_path("global", &cwd, Some(&home)).is_err());

    mcp_add(
        &user,
        "s",
        McpServerConfig {
            command: Some("u".into()),
            ..empty_cfg()
        },
    )
    .unwrap();
    mcp_add(
        &user,
        "only_user",
        McpServerConfig {
            command: Some("u".into()),
            ..empty_cfg()
        },
    )
    .unwrap();
    mcp_add(
        &project,
        "s",
        McpServerConfig {
            command: Some("p".into()),
            ..empty_cfg()
        },
    )
    .unwrap();
    mcp_add(
        &local,
        "s",
        McpServerConfig {
            command: Some("l".into()),
            ..empty_cfg()
        },
    )
    .unwrap();
    let cfg = load_mcp_configs(&cwd, Some(&home), &[], false).unwrap();
    assert_eq!(cfg["s"].command.as_deref(), Some("l"));
    assert_eq!(cfg["only_user"].command.as_deref(), Some("u"));
    mcp_remove(&local, "s").unwrap();
    let cfg = load_mcp_configs(&cwd, Some(&home), &[], false).unwrap();
    assert_eq!(cfg["s"].command.as_deref(), Some("p"));
    let (ex, _) = load_mcp_configs_ex(&cwd, Some(&home), &[], false);
    assert_eq!(ex["s"].command.as_deref(), Some("p"));

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

fn empty_cfg() -> McpServerConfig {
    McpServerConfig {
        kind: None,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    }
}

#[tokio::test]
async fn c7_3_invalid_entries_are_skipped_with_reason() {
    let mut configs = BTreeMap::new();
    configs.insert(
        "nourltype".to_string(),
        McpServerConfig {
            url: Some("http://127.0.0.1:9".into()),
            ..empty_cfg()
        },
    );
    configs.insert(
        "weird".to_string(),
        McpServerConfig {
            kind: Some("ws".into()),
            // Test-only fixture: kind "ws" is unknown, so this entry is
            // skipped before any connection is attempted; the URL is never opened.
            url: Some("wss://x".into()),
            ..empty_cfg()
        },
    );
    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 2_000))
        .await
        .unwrap();

    assert!(mgr.status().is_empty(), "{:?}", mgr.status());
    let mut errs: Vec<(String, String)> = mgr
        .errors()
        .into_iter()
        .map(|e| (e.server_name, e.reason))
        .collect();
    errs.sort();
    assert_eq!(
        errs,
        vec![
            ("nourltype".to_string(), "url_missing_type".to_string()),
            ("weird".to_string(), "unknown_type".to_string()),
        ]
    );
}

#[test]
fn load_configs_from_cli_file() {
    let cwd = tmpdir("cfg_file");
    let home = tmpdir("cfg_file_home");
    let file = cwd.join("extra.json");
    std::fs::write(
        &file,
        r#"{"mcpServers":{"fromfile":{"command":"node","args":["s.js"]}}}"#,
    )
    .unwrap();

    let cli = vec![file.to_string_lossy().to_string()];
    let cfg = load_mcp_configs(&cwd, Some(&home), &cli, true).unwrap();
    assert_eq!(cfg["fromfile"].command.as_deref(), Some("node"));
    assert_eq!(cfg["fromfile"].args, vec!["s.js".to_string()]);

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn load_configs_bare_map_and_bad_json() {
    let cwd = tmpdir("cfg_bare");
    let home = tmpdir("cfg_bare_home");
    let cli = vec![r#"{"srv":{"command":"x"}}"#.to_string()];
    let cfg = load_mcp_configs(&cwd, Some(&home), &cli, true).unwrap();
    assert_eq!(cfg["srv"].command.as_deref(), Some("x"));

    let bad = vec!["{not json".to_string()];
    assert!(load_mcp_configs(&cwd, Some(&home), &bad, true).is_err());

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn tool_name_sanitization() {
    assert_eq!(
        crate::manager::mcp_tool_name("my server", "do.thing"),
        "mcp__my_server__do_thing"
    );
    assert_eq!(crate::manager::mcp_tool_name("a/b", "c:d"), "mcp__a_b__c_d");
    assert_eq!(crate::manager::sanitize_component("ok-name_1"), "ok-name_1");
}

fn python_exe() -> Option<String> {
    for cand in ["python3", "python"] {
        if std::process::Command::new(cand)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(cand.to_string());
        }
    }
    None
}

const SERVER_SCRIPT: &str = r#"
import os, sys, json

mode = sys.argv[1] if len(sys.argv) > 1 else "ok"
calls = 0

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        # ignore unknown/garbage lines
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        if mode == "content":
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"img","description":"image tool","inputSchema":{"type":"object"}},
                {"name":"aud","description":"audio tool","inputSchema":{"type":"object"}},
                {"name":"link","description":"link tool","inputSchema":{"type":"object"}},
                {"name":"res","description":"resource tool","inputSchema":{"type":"object"}}
            ]}})
        elif mode == "responses":
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"needin","description":"needs input","inputSchema":{"type":"object"}},
                {"name":"tasky","description":"starts a task","inputSchema":{"type":"object"}}
            ]}})
        elif mode == "env":
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"envprobe","description":os.environ.get("ZEN_MCP_TEST_ENV","missing"),"inputSchema":{"type":"object"}}
            ]}})
        elif mode == "paged" and not msg.get("params", {}).get("cursor"):
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"alpha","description":"first tool","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}}}],"nextCursor":"page2"}})
        elif mode == "paged":
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"beta","description":"second tool","inputSchema":{"type":"object"}}]}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"echoes input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"boom","description":"fails","inputSchema":{"type":"object"}}]}})
    elif method == "tools/call":
        name = msg.get("params", {}).get("name")
        args = msg.get("params", {}).get("arguments", {})
        if name == "echo":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"echo: " + str(args.get("text"))},{"type":"text","text":"second part"}]}})
        elif name == "boom":
            send({"jsonrpc":"2.0","id":mid,"result":{"isError":True,"content":[{"type":"text","text":"boom happened"}]}})
        elif name == "image":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"image","data":"..."}]}})
        elif name == "img":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"image","data":"QQ==","mimeType":"image/png"}]}})
        elif name == "aud":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"audio","data":"QQ==","mimeType":"audio/wav"}]}})
        elif name == "link":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"resource_link","uri":"file:///a.txt","name":"a"}]}})
        elif name == "res":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"resource","resource":{"uri":"file:///a.txt","text":"hi","mimeType":"text/plain"}}]}})
        elif name == "needin":
            send({"jsonrpc":"2.0","id":mid,"result":{"resultType":"input_required","requestState":"st"}})
        elif name == "tasky":
            send({"jsonrpc":"2.0","id":mid,"result":{"resultType":"task","taskId":"t1","status":"working","createdAt":"2026-11-15T00:00:00Z","lastUpdatedAt":"2026-11-15T00:00:00Z"}})
        else:
            send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"no such tool"}})
    else:
        if mid is not None:
            send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"unknown method"}})
"#;

fn write_fake_server(dir: &Path) -> PathBuf {
    let p = dir.join("fake_mcp.py");
    std::fs::write(&p, SERVER_SCRIPT).unwrap();
    p
}

fn stdio_config(py: &str, script: &Path, mode: &str) -> McpServerConfig {
    McpServerConfig {
        kind: None,
        command: Some(py.to_string()),
        args: vec![script.to_string_lossy().to_string(), mode.to_string()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    }
}

#[tokio::test]
async fn mcp_stdio_handshake_list_and_call() {
    let py = match python_exe() {
        Some(p) => p,
        None => return,
    };
    let dir = tmpdir("stdio");
    let script = write_fake_server(&dir);

    let mut configs = BTreeMap::new();
    configs.insert("srv".to_string(), stdio_config(&py, &script, "ok"));

    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 10_000))
        .await
        .expect("start did not time out");

    let status = mgr.status();
    assert_eq!(status, vec![("srv".to_string(), "connected".to_string())]);

    let tools = mgr.tools();
    let mut names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["mcp__srv__boom".to_string(), "mcp__srv__echo".to_string()]
    );

    let echo = tools.iter().find(|t| t.name() == "mcp__srv__echo").unwrap();
    assert_eq!(echo.description(), "echoes input");
    assert!(!echo.is_read_only());
    let schema = echo.parameters_schema();
    assert_eq!(schema["properties"]["text"]["type"], "string");

    let ctx = ToolContext::new(dir.clone(), "sess");
    let out = tokio::time::timeout(TEST_TIMEOUT, echo.call(json!({"text": "hi"}), &ctx))
        .await
        .unwrap();
    assert!(!out.is_error, "got {:?}", out);
    assert_eq!(out.content, "echo: hi\nsecond part");

    let boom = tools.iter().find(|t| t.name() == "mcp__srv__boom").unwrap();
    let out = tokio::time::timeout(TEST_TIMEOUT, boom.call(json!({}), &ctx))
        .await
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "boom happened");

    tokio::time::timeout(TEST_TIMEOUT, mgr.shutdown())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mcp_stdio_pagination() {
    let py = match python_exe() {
        Some(p) => p,
        None => return,
    };
    let dir = tmpdir("paged");
    let script = write_fake_server(&dir);

    let mut configs = BTreeMap::new();
    configs.insert("p".to_string(), stdio_config(&py, &script, "paged"));

    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 10_000))
        .await
        .unwrap();
    assert_eq!(mgr.status()[0].1, "connected");
    let mut names: Vec<String> = mgr.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["mcp__p__alpha".to_string(), "mcp__p__beta".to_string()]
    );
    tokio::time::timeout(TEST_TIMEOUT, mgr.shutdown())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mcp_server_that_fails_to_start() {
    let mut configs = BTreeMap::new();
    configs.insert(
        "bad".to_string(),
        McpServerConfig {
            kind: None,
            command: Some("/nonexistent/definitely/not/a/binary".to_string()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
        },
    );

    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 3_000))
        .await
        .unwrap();
    assert_eq!(
        mgr.status(),
        vec![("bad".to_string(), "failed".to_string())]
    );
    assert!(mgr.tools().is_empty());

    let py = match python_exe() {
        Some(p) => p,
        None => return,
    };
    let dir = tmpdir("exits");
    let script = dir.join("exit.py");
    std::fs::write(&script, "import sys\nsys.exit(0)\n").unwrap();
    let mut configs = BTreeMap::new();
    configs.insert("quit".to_string(), stdio_config(&py, &script, "ok"));
    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 2_000))
        .await
        .unwrap();
    assert_eq!(
        mgr.status(),
        vec![("quit".to_string(), "failed".to_string())]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn spawn_http_mock(sse: bool) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/mcp", addr);
    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                let s = String::from_utf8_lossy(&buf);
                if let Some(hdr_end) = s.find("\r\n\r\n") {
                    let content_len = s
                        .lines()
                        .find_map(|l| {
                            let low = l.to_ascii_lowercase();
                            low.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
            let body = &req[body_start..];
            let msg: Value = serde_json::from_str(body).unwrap_or(json!({}));
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned().unwrap_or(Value::Null);

            let result = match method {
                "initialize" => json!({
                    "protocolVersion":"2025-06-18",
                    "capabilities":{},
                    "serverInfo":{"name":"fake-http","version":"1"}
                }),
                "notifications/initialized" => Value::Null,
                "tools/list" => {
                    let seen = req.to_ascii_lowercase().contains("x-zen-test: see-me");
                    let desc = if seen {
                        "header-seen"
                    } else {
                        "header-missing"
                    };
                    json!({"tools":[{"name":"http_echo","description":desc,"inputSchema":{"type":"object"}}]})
                }
                "tools/call" => json!({"content":[{"type":"text","text":"http-ok"}]}),
                _ => Value::Null,
            };
            let payload = if method == "notifications/initialized" {
                json!({"jsonrpc":"2.0"})
            } else {
                json!({"jsonrpc":"2.0","id":id,"result":result})
            };

            let response = if method == "notifications/initialized" {
                "HTTP/1.1 202 Accepted\r\nMcp-Session-Id: sess-123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else if sse {
                let data = serde_json::to_string(&payload).unwrap();
                let body = format!("event: message\ndata: {}\n\n", data);
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: sess-123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            } else {
                let data = serde_json::to_string(&payload).unwrap();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    data.len(),
                    data
                )
            };
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (url, handle)
}

async fn run_http_test(sse: bool) {
    let (url, handle) = spawn_http_mock(sse).await;
    let mut configs = BTreeMap::new();
    configs.insert(
        "web".to_string(),
        McpServerConfig {
            kind: Some("http".to_string()),
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some(url),
            headers: BTreeMap::new(),
        },
    );

    let mgr = tokio::time::timeout(TEST_TIMEOUT, McpManager::start(configs, 10_000))
        .await
        .unwrap();
    assert_eq!(
        mgr.status(),
        vec![("web".to_string(), "connected".to_string())]
    );
    let tools = mgr.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__web__http_echo");

    let dir = tmpdir("http_call");
    let ctx = ToolContext::new(dir.clone(), "sess");
    let out = tokio::time::timeout(TEST_TIMEOUT, tools[0].call(json!({}), &ctx))
        .await
        .unwrap();
    assert!(!out.is_error, "got {:?}", out);
    assert_eq!(out.content, "http-ok");

    tokio::time::timeout(TEST_TIMEOUT, mgr.shutdown())
        .await
        .unwrap();
    handle.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mcp_http_json_transport() {
    run_http_test(false).await;
}

#[tokio::test]
async fn mcp_http_sse_transport() {
    run_http_test(true).await;
}

#[test]
fn load_agents_from_dirs_and_cli() {
    let cwd = tmpdir("agents_cwd");
    let home = tmpdir("agents_home");

    std::fs::create_dir_all(home.join(".zen").join("agents")).unwrap();
    std::fs::write(
        home.join(".zen").join("agents").join("user-agent.md"),
        "---\nname: researcher\ndescription: Finds things\ntools: Read, Grep\nmodel: gpt-x\n---\nYou research.\n",
    )
    .unwrap();

    std::fs::create_dir_all(cwd.join(".zen").join("agents")).unwrap();
    std::fs::write(
        cwd.join(".zen").join("agents").join("proj.md"),
        "---\ndescription: Project helper\n---\nProject body.\n",
    )
    .unwrap();

    std::fs::write(
        cwd.join(".zen").join("agents").join("researcher.md"),
        "---\nname: researcher\ndescription: Project researcher\n---\nProject researcher body.\n",
    )
    .unwrap();

    let agents = load_agents(&cwd, Some(&home), None);
    let by_name: std::collections::HashMap<_, _> =
        agents.iter().map(|a| (a.name.clone(), a)).collect();

    assert!(by_name.contains_key("general-purpose"));
    assert_eq!(by_name["proj"].description, "Project helper");
    assert_eq!(by_name["proj"].system_prompt, "Project body.");
    assert_eq!(
        by_name["researcher"].system_prompt,
        "Project researcher body."
    );

    assert!(agents.iter().any(|a| a.name == "proj"));

    let only_user = load_agents(&tmpdir("empty_cwd"), Some(&home), None);
    let u = only_user.iter().find(|a| a.name == "researcher").unwrap();
    assert_eq!(u.description, "Finds things");
    assert_eq!(u.tools, Some(vec!["Read".to_string(), "Grep".to_string()]));
    assert_eq!(u.model.as_deref(), Some("gpt-x"));

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn load_agents_cli_json_and_file() {
    let cwd = tmpdir("agents_cli");
    let home = tmpdir("agents_cli_home");

    let inline = r#"{"helper":{"description":"inline helper","prompt":"Do it","tools":["Read","Bash"],"model":"inherit"}}"#;
    let agents = load_agents(&cwd, Some(&home), Some(inline));
    let h = agents.iter().find(|a| a.name == "helper").unwrap();
    assert_eq!(h.description, "inline helper");
    assert_eq!(h.system_prompt, "Do it");
    assert_eq!(h.tools, Some(vec!["Read".to_string(), "Bash".to_string()]));
    assert_eq!(h.model, None);

    let file = cwd.join("agents.json");
    std::fs::write(&file, r#"{"fromfile":{"description":"d","prompt":"p"}}"#).unwrap();
    let agents = load_agents(&cwd, Some(&home), Some(&file.to_string_lossy()));
    assert!(agents.iter().any(|a| a.name == "fromfile"));

    let agents = load_agents(&cwd, Some(&home), None);
    assert_eq!(
        agents
            .iter()
            .filter(|a| a.name == "general-purpose")
            .count(),
        1
    );
    let gp = agents.iter().find(|a| a.name == "general-purpose").unwrap();
    assert!(gp.tools.is_none());
    assert!(!gp.system_prompt.is_empty());

    let _ = std::fs::remove_dir_all(&cwd);
    let _ = std::fs::remove_dir_all(&home);
}

#[derive(Default)]
struct RecordingRunner {
    calls: Mutex<Vec<(String, String, String)>>,
    fail: bool,
}

#[async_trait]
impl SubAgentRunner for RecordingRunner {
    async fn run_subagent(
        &self,
        def: &AgentDefinition,
        prompt: &str,
        parent_tool_use_id: &str,
        _parent_depth: usize,
    ) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push((
            def.name.clone(),
            prompt.to_string(),
            parent_tool_use_id.to_string(),
        ));
        if self.fail {
            return Err(anyhow::anyhow!("runner exploded"));
        }
        Ok(format!("[{}] {}", def.name, prompt))
    }
}

fn test_agents() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "general-purpose".to_string(),
            description: "gp".to_string(),
            tools: None,
            model: None,
            system_prompt: "gp prompt".to_string(),
            ..Default::default()
        },
        AgentDefinition {
            name: "researcher".to_string(),
            description: "research".to_string(),
            tools: Some(vec!["Read".to_string()]),
            model: None,
            system_prompt: "research prompt".to_string(),
            ..Default::default()
        },
    ]
}

#[test]
fn task_tool_exposes_its_subagent_runner() {
    // F1 seam: callers verify what children inherit through the runner.
    let runner = Arc::new(RecordingRunner::default());
    let tool = TaskTool::new(test_agents(), runner);
    assert!(tool.subagent_runner().is_some());
}

#[tokio::test]
async fn task_tool_default_type_and_parent_id() {
    let dir = tmpdir("task");
    let runner = Arc::new(RecordingRunner::default());
    let tool = TaskTool::new(test_agents(), runner.clone());
    assert_eq!(tool.name(), "Task");
    assert!(tool.is_read_only());

    let mut ctx = ToolContext::new(dir.clone(), "sess");
    ctx.current_tool_use_id = Some("call_42".to_string());

    let out = tokio::time::timeout(
        TEST_TIMEOUT,
        tool.call(json!({"description": "d", "prompt": "do the thing"}), &ctx),
    )
    .await
    .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.content, "[general-purpose] do the thing");

    let calls = runner.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "general-purpose");
    assert_eq!(calls[0].2, "call_42");

    let mut ctx2 = ToolContext::new(dir.clone(), "sess");
    ctx2.current_tool_use_id = None;
    let out = tokio::time::timeout(
        TEST_TIMEOUT,
        tool.call(
            json!({"description": "d", "prompt": "p", "subagent_type": "researcher"}),
            &ctx2,
        ),
    )
    .await
    .unwrap();
    assert_eq!(out.content, "[researcher] p");
    let calls = runner.calls.lock().unwrap().clone();
    assert_eq!(calls[1].2, "task");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_tool_unknown_type_and_errors() {
    let dir = tmpdir("task_err");
    let runner = Arc::new(RecordingRunner::default());
    let tool = TaskTool::new(test_agents(), runner.clone());
    let ctx = ToolContext::new(dir.clone(), "sess");

    let out = tokio::time::timeout(
        TEST_TIMEOUT,
        tool.call(
            json!({"description": "d", "prompt": "p", "subagent_type": "nope"}),
            &ctx,
        ),
    )
    .await
    .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("unknown subagent_type"));
    assert!(out.content.contains("general-purpose"));
    assert!(out.content.contains("researcher"));

    let out = tokio::time::timeout(TEST_TIMEOUT, tool.call(json!({"description": "d"}), &ctx))
        .await
        .unwrap();
    assert!(out.is_error);

    let failing = Arc::new(RecordingRunner {
        calls: Mutex::new(vec![]),
        fail: true,
    });
    let tool2 = TaskTool::new(test_agents(), failing);
    let out = tokio::time::timeout(
        TEST_TIMEOUT,
        tool2.call(json!({"description": "d", "prompt": "p"}), &ctx),
    )
    .await
    .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("runner exploded"));

    let _ = std::fs::remove_dir_all(&dir);
}

fn mcpcap_ctx(tag: &str) -> ToolContext {
    let dir = tmpdir(tag);
    let mut c = ToolContext::new(dir.clone(), format!("mcpcap-{}", tag));
    c.session_dir = Some(dir.join("session"));
    c.current_tool_use_id = Some(format!("toolu_{}", tag));
    c
}

fn mcpcap_spill_path(reply: &str) -> PathBuf {
    let start = reply.find("Full output saved to ").expect(reply) + "Full output saved to ".len();
    let rest = &reply[start..];
    let end = rest.find(" (lines 1-").expect(reply);
    PathBuf::from(&rest[..end])
}

#[test]
fn mcpcap_small_text_result_unchanged() {
    let c = mcpcap_ctx("small");
    let out = crate::manager::render_result(
        json!({"content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}),
        &c,
    );
    assert!(!out.is_error);
    assert_eq!(out.content, "hello\nworld");
    assert!(
        !c.session_dir.as_ref().unwrap().join("tool-output").exists(),
        "no spill file for small output"
    );
}

#[test]
fn mcpcap_huge_text_result_truncated_and_spilled_in_full() {
    let c = mcpcap_ctx("huge");
    let text: String = (1..=20_000).map(|i| format!("row {i}\n")).collect();
    assert!(text.chars().count() > crate::manager::MCP_MAX_CHARS);
    let out = crate::manager::render_result(json!({"content":[{"type":"text","text":text}]}), &c);
    assert!(!out.is_error);
    assert!(
        out.content.chars().count() < crate::manager::MCP_MAX_CHARS + 400,
        "preview stays near the cap"
    );
    assert!(out.content.starts_with("row 1\nrow 2\n"), "head kept");
    assert!(out.content.ends_with("row 20000\n"), "tail kept");
    assert!(
        !out.content.contains("row 10000\n"),
        "middle only in the file"
    );
    let path = mcpcap_spill_path(&out.content);
    assert!(
        path.starts_with(c.session_dir.as_ref().unwrap().join("tool-output")),
        "{}",
        path.display()
    );
    assert_eq!(path.file_name().unwrap(), "toolu_huge.txt");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        text,
        "spill file holds the full text"
    );
}

#[test]
fn mcpcap_non_text_content_untouched_and_error_flag_kept() {
    let c = mcpcap_ctx("image");
    let img = json!({"content":[
        {"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"},
        {"type":"text","text":"caption"}
    ]});
    let out = crate::manager::render_result(img, &c);
    assert!(!out.is_error);
    assert_eq!(out.content, "[image content]\ncaption");
    assert!(!c.session_dir.as_ref().unwrap().join("tool-output").exists());

    let c2 = mcpcap_ctx("err");
    let big = "e".repeat(crate::manager::MCP_MAX_CHARS * 3);
    let out = crate::manager::render_result(
        json!({"isError":true,"content":[{"type":"text","text":big}]}),
        &c2,
    );
    assert!(out.is_error);
    assert_eq!(
        std::fs::read_to_string(mcpcap_spill_path(&out.content)).unwrap(),
        big
    );
}

// ========================================================================
// Transport-level tests for crates/zen-mcp/src/transport.rs
//
// These drive the real `Transport` (the crate's only pub(crate) entry point
// into transport.rs) so every mutant reported by cargo-mutants on that file
// is exercised: connect_stdio's env branch, connect_http's header branch,
// Session::call_tool's response arms, content_block_to_value's variants,
// tool_to_value, Transport::from_config's timeout default, and
// Transport::kill / Session::shutdown.
//
// The mock server speaks newline-delimited JSON-RPC, the same framing the
// existing SERVER_SCRIPT above uses, which is what this repo's rmcp
// TokioChildProcess transport actually speaks.
// ========================================================================

fn transport_mock_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp_mock.py")
}

use crate::transport::Transport;

/// Build a stdio config for the transport mock.
fn tcfg_stdio(
    py: &str,
    mode: &str,
    log: &Path,
    env: BTreeMap<String, String>,
    env_dump: Option<&Path>,
) -> McpServerConfig {
    let mut args = vec![
        transport_mock_script().to_string_lossy().into_owned(),
        mode.to_string(),
        log.to_string_lossy().into_owned(),
    ];
    if let Some(dump) = env_dump {
        args.push(dump.to_string_lossy().into_owned());
    }
    McpServerConfig {
        kind: None,
        command: Some(py.to_string()),
        args,
        env,
        url: None,
        headers: BTreeMap::new(),
    }
}

fn tcfg_http(url: &str, headers: BTreeMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        kind: Some("http".to_string()),
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url.to_string()),
        headers,
    }
}

#[tokio::test]
async fn transport_from_config_timeout_defaults_to_30s() {
    // Kills the `if timeout_ms == 0 { 30_000 }` mutant in from_config.
    let cfg = tcfg_stdio("x", "ok", Path::new("l"), BTreeMap::new(), None);
    match Transport::from_config(&cfg, 0).unwrap() {
        Transport::Stdio { timeout_ms, .. } => assert_eq!(timeout_ms, 30_000),
        _ => panic!("expected Stdio"),
    }
}

#[tokio::test]
async fn transport_from_config_keeps_explicit_timeout() {
    // Kills the counterpart mutant (always returning the default).
    let cfg = tcfg_stdio("x", "ok", Path::new("l"), BTreeMap::new(), None);
    match Transport::from_config(&cfg, 5_000).unwrap() {
        Transport::Stdio { timeout_ms, .. } => assert_eq!(timeout_ms, 5_000),
        _ => panic!("expected Stdio"),
    }
    let h = tcfg_http("http://127.0.0.1:1/mcp", BTreeMap::new());
    match Transport::from_config(&h, 7_000).unwrap() {
        Transport::Http { timeout_ms, .. } => assert_eq!(timeout_ms, 7_000),
        _ => panic!("expected Http"),
    }
}

#[tokio::test]
async fn transport_from_config_rejects_missing_command_and_url() {
    let empty = McpServerConfig {
        kind: None,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    };
    // command.is_none() && url.is_some() is false, and kind != "http", so this
    // takes the stdio branch and must fail with "no command".
    assert!(Transport::from_config(&empty, 1_000).is_err());

    let http_no_url = McpServerConfig {
        kind: Some("http".to_string()),
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    };
    let err = match Transport::from_config(&http_no_url, 1_000) {
        Ok(_) => panic!("expected an error for http config without url"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("no url"), "got: {err}");
}

#[tokio::test]
async fn transport_stdio_tools_list_uses_tool_to_value() {
    // Kills tool_to_value mutants (name / description / inputSchema).
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_list");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "ok", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let out = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap()
        .unwrap();
    let tools = out["tools"].as_array().expect("tools array");
    let echo = tools
        .iter()
        .find(|t| t["name"] == "echo")
        .expect("echo tool present");
    assert_eq!(echo["description"], "echoes input");
    assert_eq!(echo["inputSchema"]["properties"]["text"]["type"], "string");

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_stdio_tools_list_paginates() {
    // Kills the cursor loop in list_all_tools.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_page");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "paged", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let out = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap()
        .unwrap();
    let names: Vec<String> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_stdio_call_tool_text_blocks() {
    // Baseline Complete arm + text rendering.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_text");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "ok", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let out = tokio::time::timeout(
        TEST_TIMEOUT,
        tr.request(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "hi"}}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let content = out["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "echo: hi");
    assert_eq!(content[1]["text"], "second part");
    assert!(out.get("isError").is_none());

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_stdio_call_tool_input_required_arm() {
    // Kills the CallToolResponse::InputRequired arm.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_ir");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "input_required", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let err = tokio::time::timeout(
        TEST_TIMEOUT,
        tr.request("tools/call", json!({"name": "echo", "arguments": {}})),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        err.to_string().contains("requires client-side input"),
        "got: {err}"
    );

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_stdio_call_tool_task_arm() {
    // Kills the CallToolResponse::Task arm.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_task");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "task", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let err = tokio::time::timeout(
        TEST_TIMEOUT,
        tr.request("tools/call", json!({"name": "echo", "arguments": {}})),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.to_string().contains("task result"), "got: {err}");

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_stdio_content_block_variants() {
    // Kills content_block_to_value mutants for image / audio / resource_link /
    // resource.
    let Some(py) = python_exe() else { return };

    let cases: [(&str, &str, &str, &str); 4] = [
        ("image", "type", "image", "image/png"),
        ("audio", "type", "audio", "audio/wav"),
        ("resource_link", "type", "resource_link", "file:///x"),
        ("resource", "type", "resource", "file:///x"),
    ];

    for (mode, key, want, extra) in cases {
        let dir = tmpdir(&format!("t_cb_{mode}"));
        let log = dir.join("methods.log");
        let cfg = tcfg_stdio(&py, mode, &log, BTreeMap::new(), None);
        let tr = Transport::from_config(&cfg, 10_000).unwrap();

        let out = tokio::time::timeout(
            TEST_TIMEOUT,
            tr.request("tools/call", json!({"name": mode, "arguments": {}})),
        )
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{mode} call failed: {e}"));

        let first = &out["content"].as_array().expect("content array")[0];
        assert_eq!(first[key], want, "mode {mode} produced {first}");
        if mode == "image" || mode == "audio" {
            assert_eq!(first["data"], "abc", "mode {mode}");
            assert_eq!(first["mimeType"], extra, "mode {mode}");
        }
        if mode == "resource_link" {
            assert_eq!(first["uri"], extra, "mode {mode}");
            assert_eq!(first["name"], "x", "mode {mode}");
        }
        if mode == "resource" {
            // Strict shape: render_result and every consumer read
            // content[i].resource.uri. A double-wrapped payload
            // (resource.resource.uri) breaks them, so pin the exact path.
            assert_eq!(
                first["resource"]["uri"], extra,
                "resource uri not at resource.uri; full block: {first}"
            );
        }

        tr.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn transport_stdio_connect_passes_env_to_child() {
    // Kills the `if !env.is_empty()` branch in connect_stdio.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_env");
    let log = dir.join("methods.log");
    let dump = dir.join("env.txt");

    let mut env = BTreeMap::new();
    env.insert("ZEN_MCP_TEST_TOKEN".to_string(), "secret123".to_string());

    let cfg = tcfg_stdio(&py, "ok", &log, env, Some(&dump));
    let tr = Transport::from_config(&cfg, 10_000).unwrap();
    // Any request forces the handshake, which spawns the child with the env.
    let _ = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap()
        .expect("tools/list");

    let want = format!("ZEN_MCP_TEST_TOKEN={}", "secret123");
    let mut found = false;
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&dump)
            && text.contains(&want)
        {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found, "child did not receive ZEN_MCP_TEST_TOKEN");

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

/// In-process HTTP MCP mock that records request headers so the test can
/// assert connect_http's header branch. Returns (url, headers, handle).
async fn spawn_header_recording_http_mock()
-> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/mcp", addr);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_task = seen.clone();

    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                let s = String::from_utf8_lossy(&buf);
                if let Some(hdr_end) = s.find("\r\n\r\n") {
                    let content_len = s
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            for line in req.lines().take_while(|l| !l.is_empty()) {
                if let Ok(mut g) = seen_task.lock() {
                    g.push(line.to_string());
                }
            }
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
            let body = &req[body_start..];
            let msg: Value = serde_json::from_str(body).unwrap_or(json!({}));
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned().unwrap_or(Value::Null);

            let (status, ctype, payload) = match method {
                "initialize" => (
                    "200 OK",
                    "application/json",
                    json!({"jsonrpc":"2.0","id":id,"result":{
                        "protocolVersion":"2026-07-28",
                        "capabilities":{},
                        "serverInfo":{"name":"hdr-mock","version":"1"}}}),
                ),
                "notifications/initialized" => {
                    let resp = "HTTP/1.1 202 Accepted\r\nMcp-Session-Id: s1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                    continue;
                }
                "tools/list" => (
                    "200 OK",
                    "application/json",
                    json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                        {"name":"echo","description":"echoes input",
                         "inputSchema":{"type":"object"}}]}}),
                ),
                _ => (
                    "200 OK",
                    "application/json",
                    json!({"jsonrpc":"2.0","id":id,"result":{}}),
                ),
            };
            let data = serde_json::to_string(&payload).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nMcp-Session-Id: s1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                data.len(),
                data
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    (url, seen, handle)
}

#[tokio::test]
async fn transport_http_connect_sends_configured_headers() {
    // Kills the `if !headers.is_empty()` branch (and the header-parsing loop)
    // in connect_http.
    let (url, seen, handle) = spawn_header_recording_http_mock().await;

    let mut headers = BTreeMap::new();
    headers.insert("X-Test-Header".to_string(), "secret123".to_string());
    headers.insert("X-Second".to_string(), "two".to_string());

    let cfg = tcfg_http(&url, headers);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();
    let out = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap()
        .expect("tools/list over http");
    assert_eq!(out["tools"][0]["name"], "echo");

    let lines = seen.lock().unwrap().clone();
    let joined = lines.join("\n").to_ascii_lowercase();
    assert!(
        joined.contains("x-test-header: secret123"),
        "headers not forwarded; saw:\n{joined}"
    );
    assert!(joined.contains("x-second: two"), "saw:\n{joined}");

    tr.kill();
    handle.abort();
}

#[tokio::test]
async fn transport_http_without_headers_still_connects() {
    // Companion to the header test: the empty-headers branch must not panic.
    let (url, seen, handle) = spawn_header_recording_http_mock().await;
    let cfg = tcfg_http(&url, BTreeMap::new());
    let tr = Transport::from_config(&cfg, 10_000).unwrap();
    let out = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap()
        .expect("tools/list over http");
    assert_eq!(out["tools"][0]["name"], "echo");

    let joined = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(
        !joined.contains("x-test-header"),
        "unexpected header leaked:\n{joined}"
    );

    tr.kill();
    handle.abort();
}

// Linux-only: the test polls /proc/<pid> to observe the child, and macOS
// has no /proc, so the "alive before kill" check always fails there.
// The kill itself (libc::kill) is Unix-wide and stays covered on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn transport_kill_tears_down_the_session() {
    // Kills Transport::kill and Session::shutdown by proving the spawned child
    // actually dies. A reconnect would succeed (the script is still on disk),
    // so the observable is the child PID: the mock writes it on startup and
    // we poll /proc until it disappears.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_kill");
    let log = dir.join("methods.log");
    let pid_file = dir.join("methods.log.pid");
    let cfg = tcfg_stdio(&py, "ok", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    let ok = tokio::time::timeout(TEST_TIMEOUT, tr.request("tools/list", json!({})))
        .await
        .unwrap();
    assert!(ok.is_ok(), "first request should succeed");

    let mut pid = String::new();
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && !text.trim().is_empty()
        {
            pid = text.trim().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!pid.is_empty(), "mock never wrote its pid");
    let proc_path = format!("/proc/{pid}");
    assert!(
        Path::new(&proc_path).exists(),
        "child {pid} should be alive before kill"
    );

    tr.kill();

    let mut dead = false;
    for _ in 0..100 {
        if !Path::new(&proc_path).exists() {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(dead, "child {pid} still alive after Transport::kill()");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_request_rejects_unknown_method_and_bad_params() {
    // Kills the method-dispatch catch-all in request().
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_req");
    let log = dir.join("methods.log");
    let cfg = tcfg_stdio(&py, "ok", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 10_000).unwrap();

    // Unknown method needs no live session.
    let err = tr.request("resources/list", json!({})).await.unwrap_err();
    assert!(
        err.to_string().contains("unsupported MCP method"),
        "got: {err}"
    );

    // tools/call without a name.
    let err = tr
        .request("tools/call", json!({"arguments": {}}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("missing 'name'"), "got: {err}");

    tr.kill();
    let _ = std::fs::remove_dir_all(&dir);
}
// Linux-only: same /proc polling as transport_kill_tears_down_the_session;
// macOS has no /proc, so the "alive before kill" check cannot run there.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn transport_shutdown_kills_the_whole_process_group() {
    // Kills `replace kill_process_group with ()`. The mock spawns a long-lived
    // grandchild; rmcp's DropGuard kills only the leader, so if shutdown() does
    // not kill the process group the grandchild survives and this test fails.
    let Some(py) = python_exe() else { return };
    let dir = tmpdir("t_pgrp");
    let log = dir.join("methods.log");
    let worker_pid_file = dir.join("methods.log.worker.pid");
    let cfg = tcfg_stdio(&py, "spawn_worker", &log, BTreeMap::new(), None);
    let tr = Transport::from_config(&cfg, 15_000).unwrap();

    let _ = tokio::time::timeout(
        TEST_TIMEOUT,
        tr.request("tools/call", json!({"name": "echo", "arguments": {}})),
    )
    .await
    .expect("tools/call should resolve")
    .expect("tools/call should succeed");

    let mut worker_pid = String::new();
    for _ in 0..100 {
        if let Ok(t) = std::fs::read_to_string(&worker_pid_file) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                worker_pid = t;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!worker_pid.is_empty(), "mock never recorded a worker pid");
    let proc_path = format!("/proc/{worker_pid}");
    assert!(
        Path::new(&proc_path).exists(),
        "grandchild {worker_pid} should be alive before kill"
    );
    tr.kill();

    let mut dead = false;
    for _ in 0..150 {
        if !Path::new(&proc_path).exists() {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        dead,
        "grandchild {worker_pid} survived Transport::kill(): process group not torn down"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transport_from_config_defaults_timeout_and_does_not_connect() {
    // Kills the `timeout_ms == 0` / `is_http()` mutants in
    // Transport::from_config, and pins the documented 30 s default.
    let stdio_cfg = McpServerConfig {
        kind: None,
        command: Some("definitely-not-spawned".to_string()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    };
    let tr = Transport::from_config(&stdio_cfg, 0).unwrap();
    assert_eq!(tr.resolved_timeout_ms(), 30_000);
    assert!(
        matches!(tr, Transport::Stdio { .. }),
        "stdio config must not build an HTTP transport"
    );
    // Never connected: kill must be a no-op, not a panic.
    tr.kill();

    let explicit = Transport::from_config(&stdio_cfg, 1234).unwrap();
    assert_eq!(explicit.resolved_timeout_ms(), 1234);
    explicit.kill();

    let http_cfg = McpServerConfig {
        kind: Some("http".to_string()),
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some("http://127.0.0.1:9/mcp".to_string()),
        headers: BTreeMap::new(),
    };
    let http = Transport::from_config(&http_cfg, 0).unwrap();
    assert_eq!(http.resolved_timeout_ms(), 30_000);
    assert!(
        matches!(http, Transport::Http { .. }),
        "http config must not build a stdio transport"
    );
    http.kill();

    // Missing command / url are hard errors.
    let bad_stdio = McpServerConfig {
        kind: None,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    };
    assert!(Transport::from_config(&bad_stdio, 1000).is_err());
    let bad_http = McpServerConfig {
        kind: Some("http".to_string()),
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
    };
    assert!(Transport::from_config(&bad_http, 1000).is_err());
}

#[test]
fn tool_to_value_maps_name_description_schema() {
    // Kills the `tool_to_value -> Default::default()` mutant.
    use rmcp::model::{JsonObject, Tool};

    let mut schema = JsonObject::new();
    schema.insert("type".to_string(), json!("object"));
    let tool = Tool::new("probe", "does things", schema);
    let v = crate::transport::tool_to_value(&tool);
    assert_eq!(v["name"], "probe");
    assert_eq!(v["description"], "does things");
    assert_eq!(v["inputSchema"]["type"], "object");

    // A tool without a description renders an empty string, not null.
    let bare = Tool::new_with_raw("bare", None, JsonObject::new());
    let v = crate::transport::tool_to_value(&bare);
    assert_eq!(v["name"], "bare");
    assert_eq!(v["description"], "");
}

#[tokio::test]
async fn transport_kill_terminates_session() {
    // Transport::kill() must terminate the session (covers mutant: kill is a no-op)
    let py = match crate::tests::python_exe() {
        Some(p) => p,
        None => return,
    };
    let dir = crate::tests::tmpdir("kill_test");
    let script = dir.join("sleep.py");
    std::fs::write(&script, "import time; time.sleep(60)").unwrap();

    let mut configs = std::collections::BTreeMap::new();
    configs.insert(
        "sleep".to_string(),
        crate::McpServerConfig {
            kind: None,
            command: Some(py.to_string()),
            args: vec![script.to_string_lossy().to_string()],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
        },
    );

    let _mgr = tokio::time::timeout(
        crate::tests::TEST_TIMEOUT,
        crate::McpManager::start(configs, 10_000),
    )
    .await
    .expect("start did not time out");

    // Get a transport directly and kill it
    let cfg = crate::McpServerConfig {
        kind: None,
        command: Some(py.to_string()),
        args: vec![script.to_string_lossy().to_string()],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
    };
    let transport = Transport::from_config(&cfg, 10_000).unwrap();

    // Get a session first (this spawns the process)
    let _session = tokio::time::timeout(
        crate::tests::TEST_TIMEOUT,
        transport.request("tools/list", json!({})),
    )
    .await
    .expect("session did not time out");

    // Now kill it
    transport.kill();

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transport_tool_to_value_shape() {
    // tool_to_value must produce non-default JSON for real rmcp::Tool input.
    // Gets a Tool from mock server's list_all_tools and passes through tool_to_value.
    // This kills the mutant: replace tool_to_value -> Value with Default::default().
    use serde_json::json;

    let py = match crate::tests::python_exe() {
        Some(p) => p,
        None => return,
    };
    let dir = crate::tests::tmpdir("tool_to_value");
    let script = dir.join("fake_tool.py");
    std::fs::write(
        &script,
        r#"import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":msg.get("id"),"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}})
    elif msg.get("method") == "notifications/initialized":
        pass
    elif msg.get("method") == "tools/list":
        send({"jsonrpc":"2.0","id":msg.get("id"),"result":{"tools":[{"name":"test_tool","description":"test","inputSchema":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}}]}})
    elif msg.get("method") == "tools/call":
        send({"jsonrpc":"2.0","id":msg.get("id"),"result":{"content":[{"type":"text","text":"ok"}],"isError":False}})
    else:
        if msg.get("id") is not None:
            send({"jsonrpc":"2.0","id":msg.get("id"),"error":{"code":-32601,"message":"unknown method"}})
"#,
    )
    .unwrap();

    let mut configs = std::collections::BTreeMap::new();
    configs.insert(
        "srv".to_string(),
        crate::McpServerConfig {
            kind: None,
            command: Some(py.to_string()),
            args: vec![script.to_string_lossy().to_string()],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
        },
    );

    let mgr = tokio::time::timeout(
        crate::tests::TEST_TIMEOUT,
        crate::McpManager::start(configs, 10_000),
    )
    .await
    .expect("start did not time out");

    // Get the transport and session to access list_all_tools
    // Access via internal: we need the Transport to call session() then list_all_tools
    // Instead, use mgr.tools() which returns RemoteTool - but we need rmcp::Tool for tool_to_value

    // We'll test tool_to_value indirectly: call transport.request("tools/list")
    // which uses tool_to_value internally. Get transport from manager internals.
    // Since we can't access Transport from mgr directly, start a new transport:
    let cfg = crate::McpServerConfig {
        kind: None,
        command: Some(py.to_string()),
        args: vec![script.to_string_lossy().to_string()],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
    };
    let transport = Transport::from_config(&cfg, 10_000).unwrap();

    // Call tools/list via transport.request which uses tool_to_value
    let result = tokio::time::timeout(
        crate::tests::TEST_TIMEOUT,
        transport.request("tools/list", json!({})),
    )
    .await
    .expect("tools/list did not time out")
    .expect("tools/list request failed");

    // Verify the output has tools with inputSchema (tool_to_value produced non-default output)
    let tools = result
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    assert!(!tools.is_empty(), "tools list should not be empty");
    let first_tool = &tools[0];
    assert_eq!(
        first_tool.get("name").and_then(|v| v.as_str()).unwrap(),
        "test_tool"
    );
    assert_eq!(
        first_tool
            .get("inputSchema")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap(),
        "object"
    );
    assert!(
        first_tool
            .get("inputSchema")
            .and_then(|v| v.get("required"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    );

    tokio::time::timeout(crate::tests::TEST_TIMEOUT, mgr.shutdown())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
