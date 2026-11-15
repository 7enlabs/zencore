mod common;

use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_tool_use_exit_2_blocks_tool() {
    let home = TempDir::new("hook_pre2_home");
    let cwd = TempDir::new("hook_pre2_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "echo 'BLOCKED by hook' >&2; exit 2", "timeout": 5}]
                }
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("hook blocked it"),
    ]).await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_pre2", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "hook blocked it");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("BLOCKED by hook"),
        "hook stderr should be in tool result: {}",
        second_req
    );

    assert_eq!(
        std::fs::read_to_string(cwd.path().join("test.txt")).unwrap(),
        "existing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_tool_use_exit_0_allows_tool() {
    let home = TempDir::new("hook_pre0_home");
    let cwd = TempDir::new("hook_pre0_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "echo 'allowing'", "timeout": 5}]
                }
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"out.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("tool allowed"),
    ]).await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_pre0", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
                "--permission-mode",
                "acceptEdits",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "tool allowed");
    let denials = v["permission_denials"]
        .as_array()
        .expect("permission_denials array");
    assert!(
        denials.is_empty(),
        "exit 0 hook should not cause denial: {:?}",
        denials
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("out.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_tool_use_other_exit_logs_but_allows() {
    let home = TempDir::new("hook_pre3_home");
    let cwd = TempDir::new("hook_pre3_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "echo 'oops' >&2; exit 3", "timeout": 5}]
                }
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("tool proceeded despite hook error"),
    ]).await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        // Core-perfect phase 2 (A6): a denied Write in headless Default
        // mode exits 1, so allow it here - this test is about the hook's
        // exit 3 being logged but not blocking the run.
        ZenRun::new("hook_pre3", &home, &cwd)
            .args([
                "-p",
                "--permission-mode",
                "bypassPermissions",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "tool proceeded despite hook error");

    assert!(
        stderr.contains("hook error") || stderr.contains("exit code 3"),
        "hook error should be logged: {}",
        stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_prompt_submit_hook_adds_context() {
    let home = TempDir::new("hook_ups_home");
    let cwd = TempDir::new("hook_ups_cwd");

    let hook_config = r#"{
        "hooks": {
            "UserPromptSubmit": [
                {"hooks": [{"type": "command", "command": "echo 'EXTRA CONTEXT FROM HOOK'", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("ok").await;

    let (_, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_ups", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("user prompt")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let msgs_text = request_messages_text(&requests[0]);
    assert!(
        msgs_text.contains("EXTRA CONTEXT FROM HOOK"),
        "hook stdout should be added to prompt: {}",
        msgs_text
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_prompt_submit_exit_2_blocks_prompt() {
    let home = TempDir::new("hook_ups2_home");
    let cwd = TempDir::new("hook_ups2_cwd");

    let hook_config = r#"{
        "hooks": {
            "UserPromptSubmit": [
                {"hooks": [{"type": "command", "command": "echo 'REJECTED' >&2; exit 2", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("should not reach").await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_ups2", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("user prompt")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 1, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["is_error"], true, "{}", v);
    assert!(
        v["result"].as_str().unwrap_or("").contains("REJECTED") || stderr.contains("REJECTED"),
        "hook stderr should be surfaced: {} / {}",
        v,
        stderr
    );

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        0,
        "model should not be called when prompt blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_feedback_continues_loop() {
    let home = TempDir::new("hook_stop_home");
    let cwd = TempDir::new("hook_stop_cwd");

    let hook_config = r#"{
        "hooks": {
            "Stop": [
                {"hooks": [{"type": "command", "command": "if [ -f .stopped ]; then exit 0; fi; touch .stopped; echo 'CONTINUE: do one more thing' >&2; exit 2", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::text("first answer"),
        Reply::text("done after stop hook"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_stop", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
                "--permission-mode",
                "bypassPermissions",
            ])
            .mock("mock-model", &server)
            .arg("start")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "done after stop hook");

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "expected 2 model requests with Stop hook feedback"
    );
    let second = request_messages_text(&requests[1]);
    assert!(
        second.contains("CONTINUE: do one more thing"),
        "stop feedback missing: {}",
        second
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_start_hook_runs() {
    let home = TempDir::new("hook_sess_home");
    let cwd = TempDir::new("hook_sess_cwd");

    let hook_config = r#"{
        "hooks": {
            "SessionStart": [
                {"matcher": "startup", "hooks": [{"type": "command", "command": "echo 'SESSION STARTED'", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("ok").await;

    let (_, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_sess", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);

    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let text = request_messages_text(&requests[0]);
    assert!(
        text.contains("SESSION STARTED"),
        "SessionStart context missing: {}",
        text
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_pre_tool_use_hooks_one_blocks() {
    let home = TempDir::new("hook_multi_home");
    let cwd = TempDir::new("hook_multi_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Write",
                    "hooks": [
                        {"type": "command", "command": "echo 'first allows'", "timeout": 5},
                        {"type": "command", "command": "echo 'SECOND BLOCKS' >&2; exit 2", "timeout": 5}
                    ]
                }
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("blocked by second hook"),
    ]).await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_multi", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "blocked by second hook");
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("SECOND BLOCKS"),
        "blocking hook stderr should be in result: {}",
        second_req
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_request_hook_blocks_tool() {
    let home = TempDir::new("hook_preq_home");
    let cwd = TempDir::new("hook_preq_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PermissionRequest": [
                {
                    "matcher": "Write",
                    "hooks": [{"type": "command", "command": "echo 'NOPE' >&2; exit 2", "timeout": 5}]
                }
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("denied by hook"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_preq", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    // A permission denial in headless mode exits 1 (A6).
    assert_eq!(code, 1, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "denied by hook");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("NOPE"),
        "hook stderr should be the deny reason: {}",
        second_req
    );

    assert_eq!(
        std::fs::read_to_string(cwd.path().join("test.txt")).unwrap(),
        "existing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_denied_hook_fires_on_denial() {
    let home = TempDir::new("hook_pden_home");
    let cwd = TempDir::new("hook_pden_cwd");
    cwd.write("test.txt", "existing");

    let hook_config = r#"{
        "hooks": {
            "PermissionDenied": [
                {"hooks": [{"type": "command", "command": "echo DENIED-HOOK >> denied.log", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Write", "arguments": "{\"file_path\":\"test.txt\",\"content\":\"hello\"}"}
            }])),
        ),
        Reply::text("denied"),
    ])
    .await;

    let (_stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_pden", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("write a file")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 1, "stderr: {}", stderr);
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("denied.log")).unwrap(),
        "DENIED-HOOK\n"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("test.txt")).unwrap(),
        "existing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_use_failure_hook_sees_error() {
    let home = TempDir::new("hook_ptuf_home");
    let cwd = TempDir::new("hook_ptuf_cwd");

    let hook_config = r#"{
        "hooks": {
            "PostToolUseFailure": [
                {"hooks": [{"type": "command", "command": "echo FAILED-HOOK >> failed.log", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"false\"}"}
            }])),
        ),
        Reply::text("after failure"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_ptuf", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
                "--permission-mode",
                "bypassPermissions",
            ])
            .mock("mock-model", &server)
            .arg("run a failing command")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "after failure");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("failed.log")).unwrap(),
        "FAILED-HOOK\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_hook_runs() {
    let home = TempDir::new("hook_send_home");
    let cwd = TempDir::new("hook_send_cwd");

    let hook_config = r#"{
        "hooks": {
            "SessionEnd": [
                {"hooks": [{"type": "command", "command": "echo ENDED >> ended.log", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("ok").await;

    let (_stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_send", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("ended.log")).unwrap(),
        "ENDED\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_batch_hook_adds_context() {
    let home = TempDir::new("hook_ptb_home");
    let cwd = TempDir::new("hook_ptb_cwd");
    cwd.write("a.txt", "aaa");
    cwd.write("b.txt", "bbb");

    let hook_config = r#"{
        "hooks": {
            "PostToolBatch": [
                {"hooks": [{"type": "command", "command": "printf '{\"hookSpecificOutput\": {\"additionalContext\": \"BATCH CONTEXT\"}}'", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{\"file_path\":\"a.txt\"}"}
                },
                {
                    "id": "call_2",
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{\"file_path\":\"b.txt\"}"}
                }
            ])),
        ),
        Reply::text("done reading"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_ptb", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("read both files")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "done reading");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("BATCH CONTEXT"),
        "batch hook context missing: {}",
        second_req
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_display_hook_replaces_text() {
    let home = TempDir::new("hook_md_home");
    let cwd = TempDir::new("hook_md_cwd");

    let hook_config = r#"{
        "hooks": {
            "MessageDisplay": [
                {"hooks": [{"type": "command", "command": "echo 'REPLACED TEXT'", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("ORIGINAL TEXT").await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        // Headless text/json output drops assistant events (NullSink);
        // stream-json is where the displayed message is observable.
        ZenRun::new("hook_md", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("say something")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let lines = ndjson(&stdout);
    let assistant = lines
        .iter()
        .find(|v| v["type"] == "assistant")
        .expect("assistant line missing");
    let shown = assistant["message"].to_string();
    assert!(
        shown.contains("REPLACED TEXT"),
        "displayed message should carry the replacement: {}",
        shown
    );
    assert!(
        !shown.contains("ORIGINAL TEXT"),
        "displayed message should not carry the original: {}",
        shown
    );
    // The stored result still carries the model's original text.
    let result = lines
        .iter()
        .find(|v| v["type"] == "result")
        .expect("result line missing");
    assert!(
        result.to_string().contains("ORIGINAL TEXT"),
        "result should keep the original text: {}",
        result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_created_hook_vetoes_todowrite() {
    let home = TempDir::new("hook_tc_home");
    let cwd = TempDir::new("hook_tc_cwd");

    let hook_config = r#"{
        "hooks": {
            "TaskCreated": [
                {"hooks": [{"type": "command", "command": "echo 'TASK VETOED' >&2; exit 2", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::start(vec![
        Reply::with_tool_calls(
            "",
            Some(serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "TodoWrite", "arguments": "{\"todos\":[{\"content\":\"do the thing\",\"status\":\"pending\",\"activeForm\":\"doing the thing\"}]}"}
            }])),
        ),
        Reply::text("vetoed"),
    ])
    .await;

    let (stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_tc", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("plan the work")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    let v = last_result(&stdout);
    assert_eq!(v["result"], "vetoed");

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second_req = request_messages_text(&requests[1]);
    assert!(
        second_req.contains("TASK VETOED"),
        "veto reason missing: {}",
        second_req
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instructions_loaded_hook_fires_for_memory_file() {
    let home = TempDir::new("hook_il_home");
    let cwd = TempDir::new("hook_il_cwd");
    cwd.write("ZEN.md", "# Project memory\n");

    let hook_config = r#"{
        "hooks": {
            "InstructionsLoaded": [
                {"hooks": [{"type": "command", "command": "echo LOADED >> loaded.log", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let server = MockServer::single_text("ok").await;

    let (_stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_il", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
            ])
            .mock("mock-model", &server)
            .arg("hi")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("loaded.log")).unwrap(),
        "LOADED\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_init_only_runs_setup_and_session_start() {
    let home = TempDir::new("hook_init_home");
    let cwd = TempDir::new("hook_init_cwd");

    let hook_config = r#"{
        "hooks": {
            "Setup": [
                {"matcher": "init", "hooks": [{"type": "command", "command": "echo SETUP-RAN >> setup.log", "timeout": 5}]}
            ],
            "SessionStart": [
                {"matcher": "startup", "hooks": [{"type": "command", "command": "echo STARTED >> setup.log", "timeout": 5}]}
            ]
        }
    }"#;
    cwd.write(".zen/settings.json", hook_config);

    let (_stdout, stderr, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("hook_init", &home, &cwd)
            .args(["--init-only", "--setting-sources", "project"])
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stderr: {}", stderr);
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("setup.log")).unwrap(),
        "SETUP-RAN\nSTARTED\n"
    );
}
