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

#[test]
fn acp_session_prompt_empty_returns_invalid_params() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let input = lines(&[
        initialize_req(1),
        req(
            2,
            "session/prompt",
            json!({"sessionId": "does-not-exist", "prompt": []}),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32602);
}

#[test]
fn acp_session_prompt_unsupported_content_returns_invalid_params() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let input = lines(&[
        initialize_req(1),
        req(
            2,
            "session/prompt",
            json!({"sessionId": "does-not-exist", "prompt": [{"type": "image", "data": "x"}]}),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32602);
}

/// Interactive `zen --acp` driver (M4): spawn the binary with piped
/// stdin/stdout, send request lines one at a time, and read response
/// lines with a deadline. A reader thread forwards stdout lines into a
/// channel, so a missing response fails the test instead of blocking.
struct AcpInteractive {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<String>,
}

impl AcpInteractive {
    fn spawn(home: &TempDir, cwd: &TempDir) -> Self {
        let mut child = ZenRun::new("acp", home, cwd).arg("--acp").spawn_piped();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { child, stdin, rx }
    }

    fn send(&mut self, value: &Value) {
        let line = value.to_string() + "\n";
        self.stdin.write_all(line.as_bytes()).expect("write stdin");
        self.stdin.flush().expect("flush stdin");
    }

    fn next_line(&self) -> Value {
        let line = self
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for a response line");
        serde_json::from_str(&line).expect("response line must be JSON")
    }

    fn finish(mut self) -> i32 {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for child");
        status.code().unwrap_or(-1)
    }
}

#[test]
fn acp_session_new_advertises_modes_and_model() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(
        2,
        "session/new",
        json!({"cwd": cwd.path().to_string_lossy()}),
    ));
    let resp = acp.next_line();
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert!(!session_id.is_empty());
    let modes = &resp["result"]["modes"];
    assert_eq!(modes["currentModeId"], "default", "{resp}");
    let ids: Vec<&str> = modes["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["default", "plan", "acceptEdits"], "{resp}");
    assert!(
        !resp.to_string().contains("bypassPermissions") && !resp.to_string().contains("dontAsk"),
        "{resp}"
    );
    let options = resp["result"]["configOptions"]
        .as_array()
        .expect("configOptions");
    assert_eq!(options.len(), 1, "{resp}");
    assert_eq!(options[0]["id"], "model");
    assert_eq!(options[0]["type"], "select");
    let current = options[0]["currentValue"].as_str().unwrap();
    assert!(
        options[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"].as_str() == Some(current)),
        "{resp}"
    );
    assert_eq!(acp.finish(), 0);
}

#[test]
fn acp_set_mode_and_set_config_option_roundtrip() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    let _ = acp.next_line();
    acp.send(&req(
        2,
        "session/new",
        json!({"cwd": cwd.path().to_string_lossy()}),
    ));
    let resp = acp.next_line();
    let session_id = resp["result"]["sessionId"].as_str().unwrap().to_string();
    // set_mode to plan succeeds with an empty result.
    acp.send(&req(
        3,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "plan"}),
    ));
    let resp = acp.next_line();
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(resp["result"], json!({}));
    // set_mode to a mode the agent never advertised is rejected.
    acp.send(&req(
        4,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "bypassPermissions"}),
    ));
    let resp = acp.next_line();
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    // set_config_option switches the model and answers the full list.
    acp.send(&req(
        5,
        "session/set_config_option",
        json!({"sessionId": session_id, "configOptionId": "model", "valueId": "qwen/qwen3-coder:free"}),
    ));
    let resp = acp.next_line();
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(
        resp["result"]["configOptions"][0]["currentValue"], "qwen/qwen3-coder:free",
        "{resp}"
    );
    // An unknown model is rejected.
    acp.send(&req(
        6,
        "session/set_config_option",
        json!({"sessionId": session_id, "configOptionId": "model", "valueId": "no-such-model"}),
    ));
    let resp = acp.next_line();
    assert_eq!(resp["error"]["code"], -32602, "{resp}");
    // An unknown session is -32002.
    acp.send(&req(
        7,
        "session/set_mode",
        json!({"sessionId": "does-not-exist", "modeId": "plan"}),
    ));
    let resp = acp.next_line();
    assert_eq!(resp["error"]["code"], -32002, "{resp}");
    assert_eq!(acp.finish(), 0);
}
