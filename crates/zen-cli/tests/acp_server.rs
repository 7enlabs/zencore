//! End-to-end tests for the ACP server stdio loop (`run_acp_server`).
//!
//! These drive the real `zen --acp` binary over stdin/stdout and pin the
//! dispatch loop: initialize / session/new / session/prompt / exit, plus the
//! exact JSON-RPC error codes (-32700 parse, -32601 method-not-found,
//! -32603 internal). The stdio loop is not drivable in-process, so the unit
//! tests in `acp.rs` cover the helpers while these tests cover the loop
//! itself (M1, Phase 2 #9; Leader tin 15260: add tests, do not exclude the
//! dispatch loop).

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::Write;
use std::time::{Duration, Instant};

fn req(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notif(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn initialize_req(id: u64) -> Value {
    req(
        id,
        "initialize",
        json!({
            "protocolVersion": 1,
            "capabilities": {},
            "clientInfo": {"name": "acp-test", "version": "0"},
        }),
    )
}

fn lines(reqs: &[Value]) -> String {
    reqs.iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Run `zen --acp` with `input` on stdin, close stdin (EOF), and collect
/// the JSON-RPC response lines plus the process exit code.
fn run_acp(home: &TempDir, cwd: &TempDir, input: &str) -> (Vec<Value>, String, i32) {
    let (stdout, stderr, code) = ZenRun::new("acp", home, cwd)
        .arg("--acp")
        .stdin(input)
        .timeout(Duration::from_secs(30))
        .run_output();
    (ndjson(&stdout), stderr, code)
}

#[test]
fn acp_initialize_roundtrip() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let (out, stderr, code) = run_acp(&home, &cwd, &lines(&[initialize_req(1)]));
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 1, "expected one response line: {out:?}");
    let resp = &out[0];
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(resp["result"]["protocolVersion"], 1);
    assert_eq!(resp["result"]["agentCapabilities"]["loadSession"], false);
}

#[test]
fn acp_malformed_json_returns_parse_error() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let (out, stderr, code) = run_acp(&home, &cwd, "this is not json\n");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 1, "expected one response line: {out:?}");
    let resp = &out[0];
    assert_eq!(resp["id"], Value::Null);
    assert_eq!(resp["error"]["code"], -32700);
}

#[test]
fn acp_unknown_method_returns_method_not_found() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let input = lines(&[initialize_req(1), req(2, "bogus/method", json!({}))]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32601);
    assert!(
        out[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bogus/method"),
        "message: {}",
        out[1]["error"]["message"]
    );
}

#[test]
fn acp_session_new_returns_session_id() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let input = lines(&[
        initialize_req(1),
        req(2, "session/new", json!({"cwd": cwd_str})),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    let session_id = out[1]["result"]["sessionId"]
        .as_str()
        .expect("sessionId missing");
    assert!(!session_id.is_empty(), "empty sessionId: {}", out[1]);
}

#[test]
fn acp_session_prompt_unknown_session_returns_internal_error() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let prompt = req(
        2,
        "session/prompt",
        json!({"sessionId": "does-not-exist", "prompt": [{"type": "text", "text": "hi"}]}),
    );
    let input = lines(&[initialize_req(1), prompt]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32603);
    assert!(
        out[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown sessionId"),
        "message: {}",
        out[1]["error"]["message"]
    );
}

#[test]
fn acp_session_new_relative_cwd_returns_internal_error() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let input = lines(&[
        initialize_req(1),
        req(2, "session/new", json!({"cwd": "relative/path"})),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32603);
}

#[test]
fn acp_exit_notification_exits_zero() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let mut child = ZenRun::new("acp", &home, &cwd).arg("--acp").spawn_piped();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = lines(&[initialize_req(1), notif("exit", json!({}))]);
    stdin.write_all(input.as_bytes()).expect("write stdin");
    stdin.flush().expect("flush stdin");
    // Do NOT close stdin: the server must exit on the `exit`
    // notification alone, not on EOF.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("zen --acp did not exit on `exit` notification within 10s");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    assert_eq!(status.code(), Some(0), "exit status: {status}");
    drop(stdin);
    let out = child.wait_with_output().expect("wait_with_output");
    let lines = ndjson(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(
        lines.len(),
        1,
        "expected only the initialize response: {lines:?}"
    );
    assert_eq!(lines[0]["result"]["protocolVersion"], 1);
}
