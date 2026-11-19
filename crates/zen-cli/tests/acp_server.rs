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
use std::path::PathBuf;
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
    assert_eq!(resp["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        resp["result"]["agentCapabilities"]["sessionCapabilities"],
        json!({
            "list": {},
            "resume": {},
            "close": {},
            "delete": {},
        })
    );
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
        Self::from_run(ZenRun::new("acp", home, cwd).arg("--acp"))
    }

    /// Spawn with a stub LLM (MockServer): the stub answers every
    /// completion, so a prompt turn can run a tool call without a real
    /// model. Used by the permission round-trip tests (TCK Q7).
    fn spawn_mock(home: &TempDir, cwd: &TempDir, server: &MockServer) -> Self {
        Self::from_run(
            ZenRun::new("acp", home, cwd)
                .arg("--acp")
                .mock("mock-model", server),
        )
    }

    fn from_run(run: ZenRun) -> Self {
        let mut child = run.spawn_piped();
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
    assert_eq!(ids, ["default", "plan", "acceptEdits", "askAll"], "{resp}");
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

// -- M5: session persistence -----------------------------------------------

/// Seed a session journal under the temp home (M5): uses
/// `session::ensure_session_path` layout (`~/.zen/projects/<encoded
/// cwd>/<session-id>.jsonl`) so load/resume/list/delete can be driven
/// without an LLM. The `cwd` field of each record is the raw cwd
/// string, exactly as `TranscriptWriter` writes it.
fn seed_journal(home: &TempDir, cwd: &TempDir, session_id: &str) -> PathBuf {
    // Use the real layout helper: hand-rolling the encoding drifted
    // from encode_cwd (Windows verbatim prefix), so the seeded
    // journal landed in a different directory than the server looks
    // in, and load/resume/list/delete saw unknown session.
    let path = zen_cli::session::ensure_session_path(home.path(), cwd.path(), session_id).unwrap();
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let records = [
        json!({"type": "user", "uuid": "u1", "parent_uuid": null, "session_id": session_id, "timestamp": "2026-11-17T10:00:00Z", "cwd": cwd_str, "message": {"role": "user", "content": "hello from history"}}),
        json!({"type": "assistant", "uuid": "u2", "parent_uuid": "u1", "session_id": session_id, "timestamp": "2026-11-17T10:00:01Z", "cwd": cwd_str, "message": {"role": "assistant", "content": "hi back", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "Bash", "arguments": "{}"}}]}}),
        json!({"type": "tool", "uuid": "u3", "parent_uuid": "u2", "session_id": session_id, "timestamp": "2026-11-17T10:00:02Z", "cwd": cwd_str, "message": {"role": "tool", "tool_call_id": "call-1", "name": "Bash", "content": "done"}}),
    ];
    let mut out = String::new();
    for r in records {
        out.push_str(&r.to_string());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    path
}

#[test]
fn acp_session_list_empty() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let input = lines(&[initialize_req(1), req(2, "session/list", json!({}))]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["result"]["sessions"], json!([]));
}

#[test]
fn acp_session_list_shows_seeded_journal() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    seed_journal(&home, &cwd, "seed-1");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let input = lines(&[
        initialize_req(1),
        req(2, "session/list", json!({})),
        req(3, "session/list", json!({"cwd": cwd_str})),
        req(
            4,
            "session/list",
            json!({"cwd": std::env::temp_dir().join("nonexistent-dir-xyz").to_string_lossy()}),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 4, "expected four response lines: {out:?}");
    let sessions = out[1]["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{out:?}");
    assert_eq!(sessions[0]["sessionId"], "seed-1");
    assert_eq!(sessions[0]["cwd"], cwd_str);
    assert!(sessions[0].get("updatedAt").is_some(), "{out:?}");
    assert_eq!(out[2]["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(out[3]["result"]["sessions"], json!([]));
}

#[test]
fn acp_session_load_replays_history() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    seed_journal(&home, &cwd, "seed-1");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(
        2,
        "session/load",
        json!({"sessionId": "seed-1", "cwd": cwd_str, "mcpServers": []}),
    ));
    // Replay first, in journal order: user chunk, agent chunk, tool
    // call, tool result. Then the load response.
    let n1 = acp.next_line();
    assert_eq!(n1["method"], "session/update");
    assert_eq!(
        n1["params"]["update"]["sessionUpdate"],
        "user_message_chunk"
    );
    assert_eq!(
        n1["params"]["update"]["content"]["text"],
        "hello from history"
    );
    let n2 = acp.next_line();
    assert_eq!(
        n2["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
    assert_eq!(n2["params"]["update"]["content"]["text"], "hi back");
    let n3 = acp.next_line();
    assert_eq!(n3["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(n3["params"]["update"]["toolCallId"], "call-1");
    let n4 = acp.next_line();
    assert_eq!(n4["params"]["update"]["sessionUpdate"], "tool_call_update");
    assert_eq!(n4["params"]["update"]["status"], "completed");
    let resp = acp.next_line();
    assert_eq!(resp["id"], 2);
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(resp["result"]["modes"]["currentModeId"], "default");
    assert_eq!(acp.finish(), 0);
}

#[test]
fn acp_session_resume_returns_without_replay() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    seed_journal(&home, &cwd, "seed-1");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(
        2,
        "session/resume",
        json!({"sessionId": "seed-1", "cwd": cwd_str, "mcpServers": []}),
    ));
    // No notifications: the very next line is the resume response.
    let resp = acp.next_line();
    assert_eq!(resp["id"], 2);
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(resp["result"]["modes"]["currentModeId"], "default");
    assert_eq!(acp.finish(), 0);
}

#[test]
fn acp_session_close_is_idempotent() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(2, "session/new", json!({"cwd": cwd_str})));
    let new_resp = acp.next_line();
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    acp.send(&req(3, "session/close", json!({"sessionId": session_id})));
    let close1 = acp.next_line();
    assert_eq!(close1["id"], 3);
    assert_eq!(close1["result"], json!({}));
    acp.send(&req(4, "session/close", json!({"sessionId": session_id})));
    let close2 = acp.next_line();
    assert_eq!(close2["id"], 4);
    assert_eq!(close2["result"], json!({}));
    acp.send(&req(5, "session/close", json!({"sessionId": "ghost"})));
    let close3 = acp.next_line();
    assert_eq!(close3["id"], 5);
    assert_eq!(close3["result"], json!({}));
    assert_eq!(acp.finish(), 0);
}

#[test]
fn acp_session_delete_removes_journal() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let journal = seed_journal(&home, &cwd, "seed-1");
    let mut acp = AcpInteractive::spawn(&home, &cwd);
    acp.send(&initialize_req(1));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(2, "session/delete", json!({"sessionId": "seed-1"})));
    let resp = acp.next_line();
    assert_eq!(resp["id"], 2);
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    assert_eq!(resp["result"], json!({}));
    acp.send(&req(3, "session/list", json!({})));
    let list = acp.next_line();
    assert_eq!(list["result"]["sessions"], json!([]));
    assert_eq!(acp.finish(), 0);
    assert!(!journal.exists(), "journal should be deleted");
    // Deleting again succeeds silently.
    let input = lines(&[
        initialize_req(1),
        req(2, "session/delete", json!({"sessionId": "seed-1"})),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out[1]["result"], json!({}));
}

#[test]
fn acp_session_load_unknown_is_32002() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let input = lines(&[
        initialize_req(1),
        req(
            2,
            "session/load",
            json!({"sessionId": "ghost", "cwd": cwd_str, "mcpServers": []}),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["error"]["code"], -32002);
}

#[test]
fn acp_session_load_cwd_mismatch_is_invalid_params() {
    let home = TempDir::new("acp_home");
    let cwd = TempDir::new("acp_cwd");
    let other = TempDir::new("acp_other_cwd");
    seed_journal(&home, &cwd, "seed-1");
    let other_str = other.path().to_string_lossy().to_string();
    let input = lines(&[
        initialize_req(1),
        req(
            2,
            "session/load",
            json!({"sessionId": "seed-1", "cwd": other_str, "mcpServers": []}),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["error"]["code"], -32602);
}

/// TCK Q7 (Leader tin 16271): the `session/request_permission`
/// round-trip through the real server. The stub LLM answers the first
/// completion with a `Write` tool call, so the agent asks the client
/// for permission; the test client answers, and the turn outcome must
/// follow the answer. Local stub only (127.0.0.1), no real model.
fn write_tool_call_reply() -> Reply {
    Reply::with_tool_calls(
        "",
        Some(json!([{
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "Write",
                "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"
            }
        }])),
    )
}

/// Drive one prompt turn that hits a permission question: send the
/// prompt, answer the first `session/request_permission` request with
/// `option_id`, and return that request plus the prompt response.
/// Any other line is a `session/update` notification.
fn drive_permission_turn(
    acp: &mut AcpInteractive,
    prompt_id: u64,
    session_id: &str,
    option_id: &str,
) -> (Value, Value) {
    acp.send(&req(
        prompt_id,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "write a file"}],
        }),
    ));
    let mut permission_request = None;
    loop {
        let line = acp.next_line();
        if line.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            assert!(
                permission_request.is_none(),
                "expected exactly one permission request, got a second: {line}"
            );
            let answer = json!({
                "jsonrpc": "2.0",
                "id": line["id"].clone(),
                "result": {"outcome": {"type": "selected", "optionId": option_id}},
            });
            permission_request = Some(line);
            acp.send(&answer);
        } else if line.get("id") == Some(&json!(prompt_id)) {
            let request =
                permission_request.expect("prompt answered without any session/request_permission");
            return (request, line);
        }
        // Anything else is a session/update notification; keep reading.
    }
}

/// The permission request the server sends for a `Write` tool call:
/// this session, this tool call, kind `edit`, and the four options.
fn assert_permission_request_shape(request: &Value, session_id: &str) {
    assert_eq!(request["jsonrpc"], "2.0", "{request}");
    assert_eq!(request["params"]["sessionId"], session_id, "{request}");
    let tool_call = &request["params"]["toolCall"];
    assert_eq!(tool_call["toolCallId"], "call_1", "{request}");
    assert_eq!(tool_call["title"], "Write", "{request}");
    assert_eq!(tool_call["kind"], "edit", "{request}");
    let option_ids: Vec<&str> = request["params"]["options"]
        .as_array()
        .expect("options array")
        .iter()
        .map(|o| o["id"].as_str().expect("option id"))
        .collect();
    assert_eq!(
        option_ids,
        ["allow_once", "allow_always", "reject_once", "reject_always"],
        "{request}"
    );
}

/// Start an ACP session backed by the stub LLM, ready for a prompt turn.
fn start_permission_session(
    home: &TempDir,
    cwd: &TempDir,
    server: &MockServer,
) -> (AcpInteractive, String) {
    let mut acp = AcpInteractive::spawn_mock(home, cwd, server);
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
    (acp, session_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_allow_once_runs_write_tool() {
    let home = TempDir::new("acp_perm_allow_home");
    let cwd = TempDir::new("acp_perm_allow_cwd");
    let server = MockServer::start(vec![write_tool_call_reply(), Reply::text("write done")]).await;
    let (mut acp, session_id) = start_permission_session(&home, &cwd, &server);
    let (request, response) = drive_permission_turn(&mut acp, 3, &session_id, "allow_once");
    assert_permission_request_shape(&request, &session_id);
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
    // The tool really ran: the file the stub LLM asked to write exists.
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("test.txt")).expect("test.txt written"),
        "hello"
    );
    let requests = server.finish().await;
    assert!(
        requests.len() >= 2,
        "expected the tool result to reach the LLM again, got {} requests",
        requests.len()
    );
    assert!(
        requests[1].contains("call_1"),
        "second LLM request must carry the tool result: {}",
        requests[1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_permission_reject_once_denies_write_tool() {
    let home = TempDir::new("acp_perm_deny_home");
    let cwd = TempDir::new("acp_perm_deny_cwd");
    let server =
        MockServer::start(vec![write_tool_call_reply(), Reply::text("write denied")]).await;
    let (mut acp, session_id) = start_permission_session(&home, &cwd, &server);
    let (request, response) = drive_permission_turn(&mut acp, 3, &session_id, "reject_once");
    assert_permission_request_shape(&request, &session_id);
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
    // The tool never ran: a denied Write must not create the file.
    assert!(
        !cwd.path().join("test.txt").exists(),
        "denied Write created test.txt"
    );
    let requests = server.finish().await;
    assert!(
        requests.len() >= 2,
        "expected the denial to reach the LLM again, got {} requests",
        requests.len()
    );
}

// ---------------------------------------------------------------------------
// M7 (Phase 2 #9): client fs/terminal, per-session MCP servers. These tests
// drive the real `zen --acp` binary against a fake client: the test answers
// fs/terminal requests, so a file that exists only on the client can be
// read, and a write the client performs never touches the local disk.
// ---------------------------------------------------------------------------

/// `initialize` that advertises the given client capabilities (M7).
fn initialize_m7_req(id: u64, caps: Value) -> Value {
    req(
        id,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": caps,
            "clientInfo": {"name": "acp-test", "version": "0"},
        }),
    )
}

/// The stub LLM emits a `Read` call for a file that does not exist locally.
fn read_tool_call_reply() -> Reply {
    Reply::with_tool_calls(
        "",
        Some(json!([{
            "id": "call_1",
            "type": "function",
            "function": {"name": "Read", "arguments": "{\"file_path\":\"ghost.txt\"}"}
        }])),
    )
}

/// The stub LLM emits a `Bash` call.
fn bash_tool_call_reply() -> Reply {
    Reply::with_tool_calls(
        "",
        Some(json!([{
            "id": "call_1",
            "type": "function",
            "function": {"name": "Bash", "arguments": "{\"command\":\"echo client-side\"}"}
        }])),
    )
}

/// The stub LLM emits a call to the fake MCP server `echo` tool (M7).
fn mcp_echo_tool_call_reply() -> Reply {
    Reply::with_tool_calls(
        "",
        Some(json!([{
            "id": "call_1",
            "type": "function",
            "function": {"name": "mcp__fake__echo", "arguments": "{\"message\":\"hello mcp\"}"}
        }])),
    )
}

/// Start an ACP session backed by the stub LLM, advertising `caps` as the
/// client capabilities. Returns the driver and the session id.
fn start_m7_session(
    home: &TempDir,
    cwd: &TempDir,
    server: &MockServer,
    caps: Value,
) -> (AcpInteractive, String) {
    let mut acp = AcpInteractive::spawn_mock(home, cwd, server);
    acp.send(&initialize_m7_req(1, caps));
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
    (acp, session_id)
}

/// Drive one prompt turn, answering the agent client requests as they
/// arrive. `answer` maps a request method to its result JSON; returning
/// `None` fails the test (unexpected request). `session/request_permission`
/// is always answered `allow_once`. Returns the agent requests seen, the
/// `session/update` notifications, and the prompt response.
fn drive_client_turn<F: FnMut(&str) -> Option<Value>>(
    acp: &mut AcpInteractive,
    prompt_id: u64,
    session_id: &str,
    prompt_text: &str,
    mut answer: F,
) -> (Vec<Value>, Vec<Value>, Value) {
    acp.send(&req(
        prompt_id,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": [{"type": "text", "text": prompt_text}]}),
    ));
    let mut requests = Vec::new();
    let mut updates = Vec::new();
    loop {
        let line = acp.next_line();
        if let Some(method) = line.get("method").and_then(Value::as_str) {
            if line.get("id").is_none() {
                // Notification (session/update); keep reading.
                updates.push(line);
                continue;
            }
            if method == "session/request_permission" {
                let ans = json!({"jsonrpc": "2.0", "id": line["id"].clone(), "result": {"outcome": {"type": "selected", "optionId": "allow_once"}}});
                requests.push(line);
                acp.send(&ans);
            } else if let Some(result) = answer(method) {
                let ans = json!({"jsonrpc": "2.0", "id": line["id"].clone(), "result": result});
                requests.push(line);
                acp.send(&ans);
            } else {
                panic!("unexpected agent request: {line}");
            }
        } else if line.get("id") == Some(&json!(prompt_id)) {
            return (requests, updates, line);
        } else {
            updates.push(line);
        }
    }
}

/// Join the `session/update` notifications into one searchable text.
fn updates_text(updates: &[Value]) -> String {
    updates
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_client_fs_read_serves_file_not_present_locally() {
    let home = TempDir::new("acp_m7_read_home");
    let cwd = TempDir::new("acp_m7_read_cwd");
    let server = MockServer::start(vec![read_tool_call_reply(), Reply::text("read done")]).await;
    let caps = json!({"fs": {"readTextFile": true, "writeTextFile": false}});
    let (mut acp, session_id) = start_m7_session(&home, &cwd, &server, caps);
    let (requests, updates, response) = drive_client_turn(
        &mut acp,
        3,
        &session_id,
        "read ghost.txt",
        |method| match method {
            "fs/read_text_file" => Some(json!({"content": "ghost content\nsecond line\n"})),
            _ => None,
        },
    );
    // The agent really asked the client for the file, with an absolute path.
    let read = requests
        .iter()
        .find(|r| r["method"] == "fs/read_text_file")
        .expect("expected an fs/read_text_file request");
    assert_eq!(read["params"]["sessionId"], session_id);
    let expected = std::fs::canonicalize(cwd.path())
        .unwrap()
        .join("ghost.txt")
        .to_string_lossy()
        .to_string();
    assert_eq!(read["params"]["path"], expected);
    // The client content reached the turn: the tool result is echoed in
    // the tool_call_update notification.
    let text = updates_text(&updates);
    assert!(text.contains("ghost content"), "updates: {text}");
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
    // Nothing was created locally: the file exists only on the client.
    assert!(!cwd.path().join("ghost.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_client_fs_write_does_not_touch_local_disk() {
    let home = TempDir::new("acp_m7_write_home");
    let cwd = TempDir::new("acp_m7_write_cwd");
    let server = MockServer::start(vec![write_tool_call_reply(), Reply::text("write done")]).await;
    let caps = json!({"fs": {"readTextFile": false, "writeTextFile": true}});
    let (mut acp, session_id) = start_m7_session(&home, &cwd, &server, caps);
    let (requests, _updates, response) = drive_client_turn(
        &mut acp,
        3,
        &session_id,
        "write a file",
        |method| match method {
            "fs/write_text_file" => Some(json!({})),
            _ => None,
        },
    );
    // The agent really asked the client to write, with the exact content.
    let write = requests
        .iter()
        .find(|r| r["method"] == "fs/write_text_file")
        .expect("expected an fs/write_text_file request");
    assert_eq!(write["params"]["sessionId"], session_id);
    let expected = std::fs::canonicalize(cwd.path())
        .unwrap()
        .join("test.txt")
        .to_string_lossy()
        .to_string();
    assert_eq!(write["params"]["path"], expected);
    assert_eq!(write["params"]["content"], "hello");
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
    // The client wrote the file: nothing exists on the local disk.
    assert!(!cwd.path().join("test.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_client_terminal_runs_bash_via_client() {
    let home = TempDir::new("acp_m7_bash_home");
    let cwd = TempDir::new("acp_m7_bash_cwd");
    let server = MockServer::start(vec![bash_tool_call_reply(), Reply::text("bash done")]).await;
    let caps = json!({"terminal": true});
    let (mut acp, session_id) = start_m7_session(&home, &cwd, &server, caps);
    let (requests, updates, response) = drive_client_turn(
        &mut acp,
        3,
        &session_id,
        "run echo",
        |method| match method {
            "terminal/create" => Some(json!({"terminalId": "t-1"})),
            "terminal/wait_for_exit" => Some(json!({"exitCode": 0})),
            "terminal/output" => Some(
                json!({"output": "client-side\n", "truncated": false, "exitStatus": {"exitCode": 0}}),
            ),
            "terminal/release" => Some(json!({})),
            _ => None,
        },
    );
    // The agent created a client terminal for the command, in the session cwd.
    let create = requests
        .iter()
        .find(|r| r["method"] == "terminal/create")
        .expect("expected a terminal/create request");
    assert_eq!(create["params"]["sessionId"], session_id);
    assert_eq!(create["params"]["command"], "echo client-side");
    let cwd_sent = create["params"]["cwd"].as_str().expect("cwd string");
    let canon = std::fs::canonicalize(cwd.path()).unwrap();
    assert!(
        cwd_sent == cwd.path().to_string_lossy() || cwd_sent == canon.to_string_lossy(),
        "unexpected terminal cwd: {cwd_sent}"
    );
    // The full terminal lifecycle ran: wait, output, and release.
    for method in [
        "terminal/wait_for_exit",
        "terminal/output",
        "terminal/release",
    ] {
        assert!(
            requests.iter().any(|r| r["method"] == method),
            "expected a {method} request: {requests:?}"
        );
    }
    // The client output reached the turn.
    let text = updates_text(&updates);
    assert!(text.contains("client-side"), "updates: {text}");
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
}

#[test]
fn acp_session_new_mcp_servers_invalid_returns_invalid_params() {
    let home = TempDir::new("acp_m7_mcp_bad_home");
    let cwd = TempDir::new("acp_m7_mcp_bad_cwd");
    let input = lines(&[
        initialize_req(1),
        req(
            2,
            "session/new",
            json!({
                "cwd": cwd.path().to_string_lossy(),
                "mcpServers": [{"name": "bad", "type": "sse", "url": "https://example.invalid/sse"}],
            }),
        ),
    ]);
    let (out, stderr, code) = run_acp(&home, &cwd, &input);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out.len(), 2, "expected two response lines: {out:?}");
    assert_eq!(out[1]["id"], 2);
    assert_eq!(out[1]["error"]["code"], -32602);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_session_new_unreachable_http_mcp_server_still_creates_session() {
    let home = TempDir::new("acp_m7_mcp_dead_home");
    let cwd = TempDir::new("acp_m7_mcp_dead_cwd");
    let server = MockServer::start(vec![]).await;
    let mut acp = AcpInteractive::spawn_mock(&home, &cwd, &server);
    acp.send(&initialize_m7_req(1, json!({})));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(
        2,
        "session/new",
        json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": [{"name": "dead", "type": "http", "url": "http://127.0.0.1:1/mcp"}],
        }),
    ));
    let resp = acp.next_line();
    assert!(
        resp.get("error").is_none(),
        "unreachable MCP server must not fail session/new: {resp}"
    );
    assert!(resp["result"]["sessionId"].as_str().is_some(), "{resp}");
    assert_eq!(acp.finish(), 0);
}

/// Minimal fake MCP server (stdio, JSON-RPC lines): answers `initialize`,
/// `tools/list` (one `echo` tool) and `tools/call`. Mirrors the fixture in
/// tests/mcp.rs, trimmed to what the rmcp client handshake needs (M7).
fn fake_mcp_server_script() -> String {
    r#"import sys, json

def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = req.get("method")
        rid = req.get("id")
        if rid is None:
            continue
        if method == "initialize":
            resp = {"jsonrpc": "2.0", "id": rid, "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "1.0.0"}}}
        elif method == "tools/list":
            resp = {"jsonrpc": "2.0", "id": rid, "result": {"tools": [{
                "name": "echo",
                "description": "Echo the input",
                "inputSchema": {"type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]}}]}}
        elif method == "tools/call":
            params = req.get("params", {})
            args = params.get("arguments", {})
            message = args.get("message", "")
            resp = {"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "ECHO: " + message}],
                "isError": False}}
        else:
            resp = {"jsonrpc": "2.0", "id": rid, "error": {
                "code": -32601, "message": "Unknown method: " + str(method)}}
        print(json.dumps(resp), flush=True)

main()
"#
    .to_string()
}

/// `python3` does not exist on a stock Windows install (only `python`).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_session_mcp_server_stdio_tool_works() {
    let home = TempDir::new("acp_m7_mcp_ok_home");
    let cwd = TempDir::new("acp_m7_mcp_ok_cwd");
    let script = cwd.write("mcp_fake_server.py", &fake_mcp_server_script());
    let server = MockServer::start(vec![mcp_echo_tool_call_reply(), Reply::text("mcp done")]).await;
    let mut acp = AcpInteractive::spawn_mock(&home, &cwd, &server);
    acp.send(&initialize_m7_req(1, json!({})));
    assert_eq!(acp.next_line()["result"]["protocolVersion"], 1);
    acp.send(&req(
        2,
        "session/new",
        json!({
            "cwd": cwd.path().to_string_lossy(),
            "mcpServers": [{"name": "fake", "type": "stdio", "command": python_cmd(), "args": [script.to_string_lossy()]}],
        }),
    ));
    let resp = acp.next_line();
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let (_requests, updates, response) =
        drive_client_turn(&mut acp, 3, &session_id, "call mcp echo", |_method| None);
    // The MCP tool result reached the turn.
    let text = updates_text(&updates);
    assert!(text.contains("ECHO: hello mcp"), "updates: {text}");
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(acp.finish(), 0);
}
