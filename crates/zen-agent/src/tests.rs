use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};
use zen_core::*;

use crate::agent::combine_permission;
use crate::subagent::subagent_mode;
use crate::testutil::*;
use crate::{AgentConfig, SubAgentExecutor, budget::BudgetPolicy};

fn fake(name: &'static str, read_only: bool, out: &str) -> Arc<dyn Tool> {
    Arc::new(FakeTool {
        name,
        read_only,
        output: ToolOutput::ok(out),
    })
}

fn allow() -> Arc<FakePerms> {
    FakePerms::new(PermissionDecision::Allow)
}

fn assert_paired(msgs: &[ChatMessage]) {
    for (i, m) in msgs.iter().enumerate() {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                assert!(
                    msgs[i + 1..]
                        .iter()
                        .any(|t| t.role == "tool" && t.tool_call_id.as_deref() == Some(&c.id)),
                    "tool_call {} has no tool result in {:#?}",
                    c.id,
                    msgs
                );
            }
        }
    }
}

#[tokio::test]
async fn text_reply_is_the_result_and_system_prompt_is_sent_not_stored() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success");
    assert!(!r.is_error);
    assert_eq!(r.result, "hello");
    assert_eq!(r.num_turns, 1);
    assert_eq!(r.usage.input_tokens, 10);
    assert_eq!(r.usage.output_tokens, 5);
    let req = &llm.requests()[0];
    assert_eq!(req.messages[0].role, "system");
    assert_eq!(req.messages[0].text_ref(), Some("you are a test"));
    assert_eq!(
        agent.messages()[0].role,
        "user",
        "system prompt must not be stored in history"
    );
}

#[tokio::test]
async fn prune_old_tool_results_end_to_end() {
    let big = "x".repeat(12_000);
    let mut scripted = vec![];
    for i in 1..=7 {
        scripted.push(resp_tool(
            "Read",
            &format!(r#"{{"path": "src/f{}.rs"}}"#, i),
            &format!("c{}", i),
        ));
    }
    scripted.push(resp("done"));
    let llm = MockLlm::new(scripted);
    let reg = registry_with(vec![fake("Read", true, &big)]);
    let mut cfg = base_cfg();
    cfg.prune.prune_after = 2;
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), cfg);

    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");

    let stats = agent.prune_stats();
    assert!(stats.pruned >= 4, "expected pruning, got {:?}", stats);
    assert!(stats.pruned_tokens > 0);

    let requests = llm.requests();
    let last_req = requests.last().unwrap();
    let pruned_summaries: Vec<_> = last_req
        .messages
        .iter()
        .filter(|m| m.role == "tool" && m.text_ref().unwrap_or("").starts_with("[zen: pruned"))
        .collect();
    assert!(
        !pruned_summaries.is_empty(),
        "expected [zen: pruned markers in the final request"
    );

    let first = pruned_summaries[0].text_ref().unwrap();
    assert!(
        first.contains("src/f1.rs"),
        "identifier preserved: {}",
        first
    );
    assert!(pruned_summaries[0].tool_call_id.is_some());

    let events = sink.events.lock().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Pruned { pruned, tokens_saved }
                if *pruned > 0 && *tokens_saved > 0
        )),
        "expected AgentEvent::Pruned"
    );

    assert_paired(agent.messages());
}

#[tokio::test]
async fn no_prune_disables_pruning_and_capping() {
    let big = "x".repeat(12_000);
    let llm = MockLlm::new(vec![
        resp_tool("Read", r#"{"path": "src/f1.rs"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Read", true, &big)]);
    let mut cfg = base_cfg();
    cfg.prune.enabled = false;
    let (mut agent, _sink, _h) = build_agent(llm.clone(), reg, allow(), cfg);

    agent.run_turn("go").await;
    assert_eq!(agent.prune_stats().pruned, 0);
    assert_eq!(agent.prune_stats().capped, 0);

    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool_msg.text_ref().unwrap().len(), 12_000);
}

#[tokio::test]
async fn oversized_tool_result_is_capped_with_reread_hint() {
    let huge = "y".repeat(40_000);
    let llm = MockLlm::new(vec![
        resp_tool("Read", r#"{"path": "src/big.rs"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Read", true, &huge)]);
    let (mut agent, _sink, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());

    agent.run_turn("go").await;
    let stats = agent.prune_stats();
    assert_eq!(stats.capped, 1);
    assert!(stats.capped_tokens > 0);

    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    let content = tool_msg.text_ref().unwrap();
    assert!(
        content.starts_with("[zen: truncated"),
        "capped result carries the marker"
    );
    assert!(content.contains("re-read with a narrower query"));
    assert!(
        content.contains("src/big.rs"),
        "label preserved: {}",
        &content[..120]
    );
}

#[tokio::test]
async fn max_output_tokens_is_threaded_into_the_request() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.max_output_tokens = Some(512);
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(llm.requests()[0].max_tokens, Some(512));
}

#[tokio::test]
async fn max_output_tokens_defaults_to_none() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(llm.requests()[0].max_tokens, None);
}

#[tokio::test]
async fn tool_call_roundtrip_feeds_result_back_and_accumulates_usage() {
    let llm = MockLlm::new(vec![resp_tool("Echo", r#"{"x":1}"#, "c1"), resp("done")]);
    let reg = registry_with(vec![fake("Echo", true, "ECHOED")]);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(r.num_turns, 2);
    assert_eq!(r.usage.input_tokens, 30);
    assert_eq!(r.usage.output_tokens, 13);
    let second = &llm.requests()[1];
    let tool_msg = second.messages.iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(tool_msg.text_ref(), Some("ECHOED"));
    let events = sink.events.lock().unwrap();
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ToolResult { tool_use_id, content, is_error: false, .. }
            if tool_use_id == "c1" && content == "ECHOED")));
}

#[tokio::test]
async fn unknown_tool_and_bad_json_become_tool_errors_not_crashes() {
    let llm = MockLlm::new(vec![
        resp_tool("Nope", "{}", "u1"),
        resp_tool("Echo", "{not json", "b1"),
        resp("recovered"),
    ]);
    let reg = registry_with(vec![fake("Echo", true, "x")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "recovered");
    let msgs = agent.messages();
    let contents: Vec<&str> = msgs
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref())
        .collect();
    assert!(contents[0].contains("Unknown tool 'Nope'"));
    assert!(contents[1].contains("Invalid JSON arguments"));
    assert_paired(msgs);
}

#[tokio::test]
async fn deny_is_recorded_in_result_and_event() {
    let llm = MockLlm::new(vec![resp_tool("Write", r#"{"p":"a"}"#, "w1"), resp("ok")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let perms = FakePerms::new(PermissionDecision::Deny("rule says no".into()));
    let (mut agent, sink, _h) = build_agent(llm, reg, perms, base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.permission_denials.len(), 1);
    assert_eq!(r.permission_denials[0].tool_name, "Write");
    assert_eq!(r.permission_denials[0].tool_use_id, "w1");
    assert_eq!(r.permission_denials[0].tool_input, json!({"p":"a"}));
    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool_msg.text_ref(), Some("rule says no"));
    assert!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionDenied(_)))
    );
}

#[tokio::test]
async fn ask_without_prompter_denies_in_headless_mode() {
    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w1"), resp("ok")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let (mut agent, _s, _h) = build_agent(
        llm,
        reg,
        FakePerms::new(PermissionDecision::Ask),
        base_cfg(),
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.permission_denials.len(), 1);
    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    assert!(tool_msg.text_ref().unwrap().contains("non-interactive"));
}

#[tokio::test]
async fn plan_mode_hides_mutating_tools_and_returns_plan_text() {
    let llm = MockLlm::new(vec![resp("1. do X\n2. do Y")]);
    let reg = registry_with(vec![fake("Read", true, "r"), fake("Write", false, "w")]);
    let cfg = AgentConfig {
        permission_mode: PermissionMode::Plan,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("plan it").await;
    assert_eq!(r.result, "1. do X\n2. do Y");
    let names: Vec<String> = llm.requests()[0]
        .tools
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(names.contains(&"Read".to_string()));
    assert!(!names.contains(&"Write".to_string()));
}

#[tokio::test]
async fn max_turns_stops_with_error_subtype() {
    let llm = MockLlm::with_delay(
        vec![
            resp_tool("Echo", "{}", "a"),
            resp_tool("Echo", "{}", "b"),
            resp("echoed twice, ran out of turns"),
        ],
        200,
    );
    let reg = registry_with(vec![fake("Echo", true, "x")]);
    let cfg = AgentConfig {
        max_turns: 2,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "error_max_turns");
    assert!(r.is_error);
    assert_eq!(r.num_turns, 2);
    // G3: the third model call is the final summary, not a turn.
    assert!(r.result.contains("echoed twice, ran out of turns"));
    assert!(r.result.contains("Summary:"));
    assert_eq!(llm.requests().len(), 3);
    assert!(llm.requests()[2].tools.is_empty());
    // The summary call's usage and latency are accounted in the result:
    // 2 tool replies (20 prompt / 8 completion each) + 1 summary reply
    // (10 / 5). Each mock call sleeps 200ms, so the total API time is
    // at least 3 x 200ms; the wide upper bound only guards against the
    // accumulation itself being wrong, not against jitter.
    assert_eq!(r.usage.input_tokens, 50);
    assert_eq!(r.usage.output_tokens, 21);
    assert!(r.duration_api_ms >= 600, "api ms: {}", r.duration_api_ms);
    assert!(r.duration_api_ms < 5000, "api ms: {}", r.duration_api_ms);
}

#[tokio::test]
async fn loop_stuck_stops_repeated_bash_at_third_turn() {
    let args = r#"{"command": "ls"}"#;
    let llm = MockLlm::with_delay(
        vec![
            resp_tool("Bash", args, "1"),
            resp_tool("Bash", args, "2"),
            resp_tool("Bash", args, "3"),
            with_cache_read(resp("listed the directory three times, still stuck"), 7),
        ],
        200,
    );
    let reg = registry_with(vec![fake("Bash", true, "x")]);
    let cfg = AgentConfig {
        max_turns: 50,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "loop_stuck");
    assert!(r.is_error);
    assert_eq!(r.num_turns, 3);
    assert!(r.result.contains("loop_stuck"));
    assert!(r.result.contains("prompt"));
    // G3: one extra tool-less summary call after the stop.
    assert!(r.result.contains("listed the directory three times"));
    assert!(r.result.contains("Summary:"));
    assert_eq!(llm.requests().len(), 4);
    assert!(llm.requests()[3].tools.is_empty());
    assert_eq!(llm.requests()[3].max_tokens, Some(512));
    // The summary call's usage and latency are accounted in the result:
    // 3 tool replies (20 prompt / 8 completion each) + 1 summary reply
    // (10 / 5, 7 cached). Each mock call sleeps 200ms, so the total API
    // time is at least 4 x 200ms; the wide upper bound only guards
    // against the accumulation itself being wrong, not against jitter.
    assert_eq!(r.usage.input_tokens, 70);
    assert_eq!(r.usage.output_tokens, 29);
    assert_eq!(r.usage.cache_read_input_tokens, 7);
    assert!(r.duration_api_ms >= 800, "api ms: {}", r.duration_api_ms);
    assert!(r.duration_api_ms < 5000, "api ms: {}", r.duration_api_ms);
}

#[tokio::test]
async fn loop_stuck_allows_two_repeats() {
    let args = r#"{"command": "ls"}"#;
    let llm = MockLlm::new(vec![
        resp_tool("Bash", args, "1"),
        resp_tool("Bash", args, "2"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Bash", true, "x")]);
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

#[tokio::test]
async fn loop_stuck_stops_unchanged_multi_call_turns() {
    let scripted = vec![
        resp_multi_tool(vec![
            ("Read", r#"{"path": "a.rs"}"#, "a1"),
            ("Read", r#"{"path": "b.rs"}"#, "b1"),
        ]),
        resp_multi_tool(vec![
            ("Read", r#"{"path": "a.rs"}"#, "a2"),
            ("Read", r#"{"path": "b.rs"}"#, "b2"),
        ]),
        resp_multi_tool(vec![
            ("Read", r#"{"path": "a.rs"}"#, "a3"),
            ("Read", r#"{"path": "b.rs"}"#, "b3"),
        ]),
    ];
    let llm = MockLlm::new(scripted);
    let reg = registry_with(vec![fake("Read", true, "r")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "loop_stuck");
    assert_eq!(r.num_turns, 3);
    // G3 fallback: the summary call was attempted (4th request) but the
    // script was exhausted, so the result is the plain stop message.
    assert_eq!(llm.requests().len(), 4);
    assert!(!r.result.contains("Summary:"));
}

#[tokio::test]
async fn loop_stuck_k_is_configurable() {
    let args = r#"{"command": "ls"}"#;
    let llm = MockLlm::new(vec![
        resp_tool("Bash", args, "1"),
        resp_tool("Bash", args, "2"),
        resp_tool("Bash", args, "3"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Bash", true, "x")]);
    let cfg = AgentConfig {
        loop_stuck_k: 5,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

#[tokio::test]
async fn same_error_different_commands_trips_loop_stuck() {
    // G2 (TDD): the same error through different commands must trip the
    // guard even though no two tool calls share identical arguments.
    // Written BEFORE the error-fingerprint trigger exists: it fails on
    // current main (the run reaches the fourth scripted reply instead).
    let err = "error: test `foo` failed\nassertion failed at src/lib.rs:42";
    let llm = MockLlm::new(vec![
        resp_tool("Bash", r#"{"command": "cargo test --workspace"}"#, "1"),
        resp_tool("Bash", r#"{"command": "cargo test -p zen-cli"}"#, "2"),
        resp_tool("Bash", r#"{"command": "cargo test -- --nocapture"}"#, "3"),
        resp("summary of the failed tests"),
    ]);
    let reg = registry_with(vec![Arc::new(FakeTool {
        name: "Bash",
        read_only: true,
        output: ToolOutput::err(err),
    })]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "loop_stuck");
    assert!(r.is_error);
    assert_eq!(r.num_turns, 3);
    assert!(r.result.contains("same error"));
    assert_eq!(llm.requests().len(), 4);
}

#[tokio::test]
async fn denied_call_repeats_trip_loop_stuck() {
    // G1: a call denied 3 times in a row trips the guard instead of
    // burning turns until max_turns.
    let args = r#"{"file_path": "a.rs"}"#;
    let llm = MockLlm::new(vec![
        resp_tool("Edit", args, "1"),
        resp_tool("Edit", args, "2"),
        resp_tool("Edit", args, "3"),
        resp("could not edit, permission denied throughout"),
    ]);
    let reg = registry_with(vec![fake("Edit", false, "edited")]);
    let perms = FakePerms::new(PermissionDecision::Deny("rule says no".into()));
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, perms, base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "loop_stuck");
    assert!(r.is_error);
    assert_eq!(r.num_turns, 3);
    assert!(r.result.contains("denied"));
    assert_eq!(r.permission_denials.len(), 3);
    assert_eq!(llm.requests().len(), 4);
}

#[tokio::test]
async fn denied_different_args_do_not_trip() {
    // G1: rejections only accumulate per (tool, args, reason); denying
    // different calls is not a loop.
    let llm = MockLlm::new(vec![
        resp_tool("Edit", r#"{"file_path": "a.rs"}"#, "1"),
        resp_tool("Edit", r#"{"file_path": "b.rs"}"#, "2"),
        resp_tool("Edit", r#"{"file_path": "a.rs"}"#, "3"),
        resp_tool("Edit", r#"{"file_path": "a.rs"}"#, "4"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Edit", false, "edited")]);
    let perms = FakePerms::new(PermissionDecision::Deny("rule says no".into()));
    let (mut agent, _s, _h) = build_agent(llm, reg, perms, base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

#[tokio::test]
async fn transient_error_different_commands_do_not_trip() {
    // G2: transient errors never feed the error trigger, and different
    // commands never feed the repeated-call trigger.
    let llm = MockLlm::new(vec![
        resp_tool("Bash", r#"{"command": "cmd-a"}"#, "1"),
        resp_tool("Bash", r#"{"command": "cmd-b"}"#, "2"),
        resp_tool("Bash", r#"{"command": "cmd-c"}"#, "3"),
        resp_tool("Bash", r#"{"command": "cmd-d"}"#, "4"),
        resp_tool("Bash", r#"{"command": "cmd-e"}"#, "5"),
        resp("done"),
    ]);
    let reg = registry_with(vec![Arc::new(FakeTool {
        name: "Bash",
        read_only: true,
        output: ToolOutput::err("rate limit exceeded, try again later"),
    })]);
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

/// Test tool returning a different error on each call.
struct CountingErrorTool {
    name: &'static str,
    errors: Vec<&'static str>,
    next: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Tool for CountingErrorTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "test tool returning a different error per call"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolOutput::err(
            self.errors
                .get(i)
                .copied()
                .unwrap_or("error: out of scripted errors"),
        )
    }
}

#[tokio::test]
async fn changing_error_does_not_trip() {
    // G2: an error that changes every time is progress, not a loop.
    let tool = Arc::new(CountingErrorTool {
        name: "Bash",
        errors: vec![
            "error: borrow of moved value",
            "error: mismatched types",
            "error: cannot find value in this scope",
            "error: expected semicolon",
            "error: type annotations needed",
        ],
        next: std::sync::atomic::AtomicUsize::new(0),
    });
    let llm = MockLlm::new(vec![
        resp_tool("Bash", r#"{"command": "cmd-a"}"#, "1"),
        resp_tool("Bash", r#"{"command": "cmd-b"}"#, "2"),
        resp_tool("Bash", r#"{"command": "cmd-c"}"#, "3"),
        resp_tool("Bash", r#"{"command": "cmd-d"}"#, "4"),
        resp_tool("Bash", r#"{"command": "cmd-e"}"#, "5"),
        resp("done"),
    ]);
    let reg = registry_with(vec![tool]);
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

#[tokio::test]
async fn loop_stuck_k_zero_disables_guard() {
    // G4: K = 0 turns the guard off entirely; identical repeats run on.
    let args = r#"{"command": "ls"}"#;
    let llm = MockLlm::new(vec![
        resp_tool("Bash", args, "1"),
        resp_tool("Bash", args, "2"),
        resp_tool("Bash", args, "3"),
        resp_tool("Bash", args, "4"),
        resp_tool("Bash", args, "5"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Bash", true, "x")]);
    let cfg = AgentConfig {
        loop_stuck_k: 0,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(r.result, "done");
}

#[tokio::test]
async fn model_failure_surfaces_as_error_result() {
    let llm = MockLlm::with_errors(vec![Err(anyhow::anyhow!("boom 500"))]);
    let (mut agent, _s, _h) = build_agent(llm, ToolRegistry::new(), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "error_during_execution");
    assert!(r.is_error);
    assert!(r.result.contains("boom 500"));
}

#[tokio::test]
async fn fallback_model_is_used_after_primary_fails() {
    let llm = MockLlm::with_errors(vec![
        Err(anyhow::anyhow!("primary down")),
        Ok(resp("via fb")),
    ]);
    let cfg = AgentConfig {
        fallback_models: vec!["fb-model".into()],
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "via fb");
    let models: Vec<String> = llm.requests().iter().map(|r| r.model.clone()).collect();
    assert_eq!(
        models,
        vec!["mock-model".to_string(), "fb-model".to_string()]
    );
}

#[tokio::test]
async fn c3_10_user_prompt_submit_block_skips_model_and_context_is_appended() {
    let llm = MockLlm::new(vec![]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    hooks.push(
        HookEvent::UserPromptSubmit,
        HookOutcome {
            blocked: true,
            feedback: Some("no secrets".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("my password is x").await;
    assert!(r.is_error);
    assert_eq!(r.result, "no secrets");
    assert!(
        llm.requests().is_empty(),
        "blocked prompt must not reach the model"
    );

    let llm = MockLlm::new(vec![resp("ok")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    hooks.push(
        HookEvent::UserPromptSubmit,
        HookOutcome {
            feedback: Some("CTX: branch main".into()),
            ..Default::default()
        },
    );
    agent.run_turn("hello").await;
    let user = llm.requests()[0].messages[1].text();
    assert!(user.starts_with("hello") && user.contains("CTX: branch main"));
    let calls = hooks.calls.lock().unwrap();
    assert_eq!(calls[0].2["prompt"], "hello");
}

#[tokio::test]
async fn c3_7_pre_tool_use_block_and_updated_input() {
    let llm = MockLlm::new(vec![
        resp_tool("Echo", r#"{"v":"orig"}"#, "c1"),
        resp_tool("Echo", r#"{"v":"orig"}"#, "c2"),
        resp("end"),
    ]);
    let reg = registry_with(vec![fake("Echo", true, "ran")]);
    let (mut agent, _s, hooks) = build_agent(llm, reg, allow(), base_cfg());
    hooks.push(
        HookEvent::PreToolUse,
        HookOutcome {
            blocked: true,
            feedback: Some("blocked by policy".into()),
            ..Default::default()
        },
    );
    hooks.push(
        HookEvent::PreToolUse,
        HookOutcome {
            updated_input: Some(json!({"v":"rewritten"})),
            ..Default::default()
        },
    );
    agent.run_turn("go").await;
    let tools: Vec<String> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.text())
        .collect();
    assert_eq!(
        tools,
        vec!["blocked by policy".to_string(), "ran".to_string()]
    );
    let calls = hooks.calls.lock().unwrap();
    let pre: Vec<&(HookEvent, String, Value)> = calls
        .iter()
        .filter(|c| c.0 == HookEvent::PreToolUse)
        .collect();
    assert_eq!(pre[0].1, "Echo", "matcher target is the tool name");
    assert_eq!(pre[0].2["tool_use_id"], "c1");

    let post = calls
        .iter()
        .find(|c| c.0 == HookEvent::PostToolUse)
        .unwrap();
    assert_eq!(post.2["tool_input"], json!({"v":"rewritten"}));
    assert_eq!(post.2["tool_response"], "ran");
}

#[tokio::test]
async fn c2_13_hook_allow_cannot_override_rule_deny() {
    assert_eq!(
        combine_permission(
            PermissionDecision::Deny("r".into()),
            Some(PermissionDecision::Allow),
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Deny("r".into())
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Ask,
            Some(PermissionDecision::Allow),
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Allow
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Allow,
            Some(PermissionDecision::Ask),
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Ask
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Allow,
            Some(PermissionDecision::Deny("h".into())),
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Deny("h".into())
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Allow,
            None,
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Allow
    );

    assert_eq!(
        combine_permission(
            PermissionDecision::Ask,
            Some(PermissionDecision::Allow),
            true,
            PermissionMode::Default
        ),
        PermissionDecision::Ask
    );

    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w"), resp("ok")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let perms = FakePerms::new(PermissionDecision::Deny("deny rule".into()));
    let (mut agent, _s, hooks) = build_agent(llm, reg, perms, base_cfg());
    hooks.push(
        HookEvent::PreToolUse,
        HookOutcome {
            permission_override: Some(PermissionDecision::Allow),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.permission_denials.len(), 1);
}

#[test]
fn combine_permission_hook_allow_cannot_loosen_ask_all() {
    // In askAll mode a hook Allow must not lift a mode Ask to Allow:
    // hooks may only tighten, never loosen. In default mode the same
    // combination still lifts, so the mode is what decides.
    assert_eq!(
        combine_permission(
            PermissionDecision::Ask,
            Some(PermissionDecision::Allow),
            false,
            PermissionMode::AskAll
        ),
        PermissionDecision::Ask
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Ask,
            Some(PermissionDecision::Allow),
            false,
            PermissionMode::Default
        ),
        PermissionDecision::Allow
    );
    // A hook Ask still asks, and a hook Deny still denies, in askAll.
    assert_eq!(
        combine_permission(
            PermissionDecision::Allow,
            Some(PermissionDecision::Ask),
            false,
            PermissionMode::AskAll
        ),
        PermissionDecision::Ask
    );
    assert_eq!(
        combine_permission(
            PermissionDecision::Allow,
            Some(PermissionDecision::Deny("h".into())),
            false,
            PermissionMode::AskAll
        ),
        PermissionDecision::Deny("h".into())
    );
    // A mode Deny still beats a hook Allow in askAll.
    assert_eq!(
        combine_permission(
            PermissionDecision::Deny("r".into()),
            Some(PermissionDecision::Allow),
            false,
            PermissionMode::AskAll
        ),
        PermissionDecision::Deny("r".into())
    );
}

struct AskPerms {
    ask_rule: bool,
}

impl PermissionChecker for AskPerms {
    fn check(&self, _n: &str, _i: &Value, _ro: bool, _c: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask
    }
    fn ask_rule_matches(&self, _n: &str, _i: &Value, _c: &ToolContext) -> bool {
        self.ask_rule
    }
}

#[tokio::test]
async fn c2_13_hook_allow_cannot_override_ask_rule_end_to_end() {
    for (ask_rule, expect_denied) in [(true, 1usize), (false, 0usize)] {
        let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w"), resp("ok")]);
        let reg = registry_with(vec![fake("Write", false, "wrote")]);
        let (mut agent, _s, hooks) =
            build_agent(llm, reg, Arc::new(AskPerms { ask_rule }), base_cfg());
        hooks.push(
            HookEvent::PreToolUse,
            HookOutcome {
                permission_override: Some(PermissionDecision::Allow),
                ..Default::default()
            },
        );
        let r = agent.run_turn("go").await;
        assert_eq!(
            r.permission_denials.len(),
            expect_denied,
            "ask_rule={}",
            ask_rule
        );
    }
}

#[tokio::test]
async fn c3_6_pre_tool_use_continue_false_stops_and_keeps_history_valid() {
    let llm = MockLlm::new(vec![ChatResponse {
        message: ChatMessage::assistant(
            None,
            Some(vec![
                ToolCall {
                    id: "a".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "Echo".into(),
                        arguments: "{}".into(),
                    },
                },
                ToolCall {
                    id: "b".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "Echo".into(),
                        arguments: "{}".into(),
                    },
                },
            ]),
        ),
        usage: Usage::default(),
        finish_reason: None,
    }]);
    let reg = registry_with(vec![fake("Echo", true, "x")]);
    let (mut agent, _s, hooks) = build_agent(llm, reg, allow(), base_cfg());
    hooks.push(
        HookEvent::PreToolUse,
        HookOutcome {
            stop_run: true,
            stop_reason: Some("Build is red, stopping".into()),
            system_message: Some("warn".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "Build is red, stopping");
    assert_paired(agent.messages());
}

#[tokio::test]
async fn c3_8_post_tool_use_updated_output_and_feedback() {
    let llm = MockLlm::new(vec![resp_tool("Echo", "{}", "c1"), resp("fin")]);
    let reg = registry_with(vec![fake("Echo", true, "raw output")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), reg, allow(), base_cfg());
    hooks.push(
        HookEvent::PostToolUse,
        HookOutcome {
            updated_output: Some("redacted".into()),
            feedback: Some("lint: 2 warnings".into()),
            ..Default::default()
        },
    );
    agent.run_turn("go").await;
    let tool = llm.requests()[1]
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .cloned()
        .unwrap();
    assert_eq!(tool.text_ref(), Some("redacted\nlint: 2 warnings"));
}

#[tokio::test]
async fn c3_2_failed_tool_fires_post_tool_use_failure() {
    let llm = MockLlm::new(vec![resp_tool("Bad", "{}", "c1"), resp("fin")]);
    let bad: Arc<dyn Tool> = Arc::new(FakeTool {
        name: "Bad",
        read_only: true,
        output: ToolOutput::err("exploded"),
    });
    let (mut agent, _s, hooks) = build_agent(llm, registry_with(vec![bad]), allow(), base_cfg());
    agent.run_turn("go").await;
    let calls = hooks.calls.lock().unwrap();
    let f = calls
        .iter()
        .find(|c| c.0 == HookEvent::PostToolUseFailure)
        .expect("failure hook");
    assert_eq!(f.2["error"], "exploded");
    assert!(!calls.iter().any(|c| c.0 == HookEvent::PostToolUse));
}

#[tokio::test]
async fn c3_9_stop_hook_block_continues_with_reason_and_sets_stop_hook_active() {
    let llm = MockLlm::new(vec![resp("first"), resp("second")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    hooks.push(
        HookEvent::Stop,
        HookOutcome {
            blocked: true,
            feedback: Some("run the tests".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "second");
    assert_eq!(r.num_turns, 2);
    let second = &llm.requests()[1];
    assert_eq!(
        second.messages.last().unwrap().text_ref(),
        Some("run the tests")
    );
    let calls = hooks.calls.lock().unwrap();
    let stops: Vec<&Value> = calls
        .iter()
        .filter(|c| c.0 == HookEvent::Stop)
        .map(|c| &c.2)
        .collect();
    assert_eq!(stops[0]["stop_hook_active"], false);
    assert_eq!(stops[0]["last_assistant_message"], "first");
    assert_eq!(stops[1]["stop_hook_active"], true);
}

#[tokio::test]
async fn c3_9_stop_hook_loop_guard_caps_continuations() {
    let n = 3;
    let llm = MockLlm::new((0..=n).map(|i| resp(&format!("r{}", i))).collect());
    let (mut agent, _s, hooks) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    agent.set_stop_hook_max_continues(n);
    for _ in 0..10 {
        hooks.push(
            HookEvent::Stop,
            HookOutcome {
                blocked: true,
                ..Default::default()
            },
        );
    }
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert_eq!(
        llm.requests().len(),
        n + 1,
        "initial answer + n continuations"
    );
    assert_eq!(crate::DEFAULT_STOP_HOOK_CONTINUES, 8);
}

#[tokio::test]
async fn system_message_from_hook_is_emitted_to_sink_only() {
    let llm = MockLlm::new(vec![resp("ok")]);
    let (mut agent, sink, hooks) =
        build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    hooks.push(
        HookEvent::UserPromptSubmit,
        HookOutcome {
            system_message: Some("heads up".into()),
            ..Default::default()
        },
    );
    agent.run_turn("hi").await;
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::SystemMessage { event, text } if event == "UserPromptSubmit" && text == "heads up")));
    let sent = serde_json::to_string(&llm.requests()[0].messages).unwrap();
    assert!(
        !sent.contains("heads up"),
        "systemMessage must not reach the model"
    );
}

#[tokio::test]
async fn c10_manual_compact_replaces_history_keeps_pairs_and_fires_hooks() {
    let llm = MockLlm::new(vec![
        resp_tool("Echo", "{}", "c1"),
        resp("answer one"),
        resp("SUMMARY TEXT"),
    ]);
    let reg = registry_with(vec![fake("Echo", true, "x")]);
    let cfg = AgentConfig {
        keep_after_compact: 2,
        ..base_cfg()
    };
    let (mut agent, sink, hooks) = build_agent(llm.clone(), reg, allow(), cfg);
    agent.run_turn("first").await;
    agent
        .compact(Some("focus on parser"), "manual")
        .await
        .unwrap();
    let msgs = agent.messages();
    assert!(msgs[0].text_ref().unwrap().contains("SUMMARY TEXT"));
    assert!(
        msgs[0]
            .text_ref()
            .unwrap()
            .starts_with("[Compaction summary")
    );
    assert_paired(msgs);
    assert!(
        msgs.iter()
            .all(|m| m.role != "tool" || msgs.iter().any(|a| a.tool_calls.is_some()))
    );

    let sreq = llm.requests().last().cloned().unwrap();
    assert!(sreq.tools.is_empty());
    assert!(
        sreq.messages
            .last()
            .unwrap()
            .text_ref()
            .unwrap()
            .contains("focus on parser")
    );

    let calls = hooks.calls.lock().unwrap();
    let pre = calls.iter().find(|c| c.0 == HookEvent::PreCompact).unwrap();
    assert_eq!(pre.1, "manual");
    assert_eq!(pre.2["custom_instructions"], "focus on parser");
    assert!(calls.iter().any(|c| c.0 == HookEvent::PostCompact));
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::CompactBoundary { trigger, .. } if trigger == "manual")));
}

#[tokio::test]
async fn compaction_request_is_one_transcript_message_and_summary_tracks_files() {
    let llm = MockLlm::new(vec![
        resp_tool("Read", r#"{"file_path":"src/a.rs"}"#, "c1"),
        resp("answer one"),
        resp("SUMMARY TEXT"),
    ]);
    let reg = registry_with(vec![fake("Read", true, "file body")]);
    let cfg = AgentConfig {
        keep_after_compact: 1,
        system_prompt: "SYSTEM-PROMPT-XYZ".into(),
        ..base_cfg()
    };
    let (mut agent, _sink, _hooks) = build_agent(llm.clone(), reg, allow(), cfg);
    agent.run_turn("first").await;
    agent.compact(None, "manual").await.unwrap();
    let sreq = llm.requests().last().cloned().unwrap();
    assert_eq!(
        sreq.messages.len(),
        1,
        "one user message: {:?}",
        sreq.messages
    );
    assert_eq!(sreq.messages[0].role, "user");
    let body = sreq.messages[0].text_ref().unwrap();
    assert!(body.contains("[User]: first"));
    assert!(body.contains(r#"[Assistant tool calls]: Read({"file_path":"src/a.rs"})"#));
    assert!(body.contains("[Tool result]: file body"));
    assert!(body.contains("Summarize this coding session"));
    assert!(!body.contains("SYSTEM-PROMPT-XYZ"));
    let summary = agent.messages()[0].text_ref().unwrap();
    assert!(
        summary.contains("SUMMARY TEXT\n\n<read-files>\nsrc/a.rs\n</read-files>"),
        "{summary}"
    );
}

#[tokio::test]
async fn c10_pre_compact_block_leaves_history_untouched() {
    let llm = MockLlm::new(vec![resp("a")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    agent.run_turn("x").await;
    let before = agent.messages().to_vec();
    hooks.push(
        HookEvent::PreCompact,
        HookOutcome {
            blocked: true,
            ..Default::default()
        },
    );
    agent.compact(None, "manual").await.unwrap();
    assert_eq!(agent.messages(), &before[..]);
    assert_eq!(llm.requests().len(), 1, "no summarization call when vetoed");
}

#[tokio::test]
async fn c10_auto_compact_triggers_over_threshold() {
    let llm = MockLlm::new(vec![resp("one"), resp("SUM"), resp("two")]);
    let cfg = AgentConfig {
        auto_compact_threshold: Some(5),
        keep_after_compact: 1,
        ..base_cfg()
    };
    let (mut agent, sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    agent.run_turn("first").await;
    let r = agent.run_turn("second").await;
    assert_eq!(r.result, "two");
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::CompactBoundary { trigger, .. } if trigger == "auto")));
    let last = llm.requests().last().cloned().unwrap();
    let joined = serde_json::to_string(&last.messages).unwrap();
    assert!(joined.contains("SUM"));
    assert!(
        !joined.contains("\"one\""),
        "compacted history must not be resent verbatim"
    );
}

#[tokio::test]
async fn c10_auto_compact_failure_is_reported_not_swallowed() {
    // Issue #370: a failed auto-compact was discarded with `let _ =`, so
    // nothing surfaced it. The model answers the summarization call with
    // an empty summary, which makes `compact_impl` bail; the turn itself
    // must still complete, but the failure must reach the event sink.
    let llm = MockLlm::new(vec![resp("one"), resp(""), resp("two")]);
    let cfg = AgentConfig {
        auto_compact_threshold: Some(5),
        keep_after_compact: 1,
        ..base_cfg()
    };
    let (mut agent, sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    agent.run_turn("first").await;
    let r = agent.run_turn("second").await;
    assert_eq!(
        r.result, "two",
        "a failed auto-compact must not fail the turn"
    );
    assert!(
        !sink.events.lock().unwrap().iter().any(|e| matches!(e,
            AgentEvent::CompactBoundary { trigger, .. } if trigger == "auto")),
        "no compaction happened, so no CompactBoundary may be emitted"
    );
    let events = sink.events.lock().unwrap();
    assert!(
        events.iter().any(|e| matches!(e,
            AgentEvent::SystemMessage { event, text }
                if event == "AutoCompact" && text.contains("empty summary"))),
        "expected an AutoCompact SystemMessage reporting the failure, got: {events:?}"
    );
}

#[tokio::test]
async fn set_messages_replaces_history_for_resume() {
    let llm = MockLlm::new(vec![resp("cont")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), base_cfg());
    agent.set_messages(vec![
        ChatMessage::user("old q"),
        ChatMessage::assistant(Some("old a".into()), None),
    ]);
    agent.run_turn("new q").await;
    let sent: Vec<String> = llm.requests()[0]
        .messages
        .iter()
        .filter_map(|m| m.text_ref().map(str::to_string))
        .collect();
    assert_eq!(sent, vec!["you are a test", "old q", "old a", "new q"]);
}

fn executor(
    llm: Arc<MockLlm>,
    tools: ToolRegistry,
    hooks: Arc<FakeHooks>,
) -> (SubAgentExecutor, Arc<CollectSink>) {
    let sink = Arc::new(CollectSink::default());
    let ex = SubAgentExecutor::new(
        llm,
        tools,
        allow(),
        hooks,
        sink.clone(),
        temp_ctx(),
        base_cfg(),
    );
    (ex, sink)
}

#[tokio::test]
async fn c6_subagent_fresh_context_tool_filter_and_parent_id() {
    let llm = MockLlm::new(vec![resp_tool("Read", "{}", "s1"), resp("sub result")]);
    let reg = registry_with(vec![
        fake("Read", true, "r"),
        fake("Write", false, "w"),
        fake("Task", true, "t"),
        fake("Grep", true, "g"),
    ]);
    let hooks = FakeHooks::new();
    let (ex, sink) = executor(llm.clone(), reg, hooks.clone());
    let def = AgentDefinition {
        name: "reader".into(),
        description: "reads".into(),
        tools: Some(vec!["Read".into(), "Task".into(), "Grep".into()]),
        disallowed_tools: vec!["Grep".into()],
        max_turns: Some(5),
        ..Default::default()
    };
    let out = ex.run_subagent(&def, "find it", "task-1", 0).await.unwrap();
    assert_eq!(out, "sub result");
    let req = &llm.requests()[0];
    let names: Vec<String> = req.tools.iter().map(|t| t.function.name.clone()).collect();
    assert_eq!(
        names,
        vec!["Read".to_string()],
        "allowlist minus disallowed minus Task at max depth"
    );
    assert_eq!(req.messages.len(), 2, "fresh context: system + prompt only");
    assert!(sink.events.lock().unwrap().iter().all(|e| match e {
        AgentEvent::Assistant {
            parent_tool_use_id, ..
        }
        | AgentEvent::ToolResult {
            parent_tool_use_id, ..
        } => parent_tool_use_id.as_deref() == Some("task-1"),
        _ => true,
    }));
    let calls = hooks.calls.lock().unwrap();
    let start = calls
        .iter()
        .position(|c| c.0 == HookEvent::SubagentStart)
        .unwrap();
    let stop = calls
        .iter()
        .position(|c| c.0 == HookEvent::SubagentStop)
        .unwrap();
    assert!(start < stop);
    assert_eq!(calls[stop].1, "reader");
    assert_eq!(calls[stop].2["last_assistant_message"], "sub result");
    assert!(
        !calls.iter().any(|c| c.0 == HookEvent::Stop),
        "sub-agents fire SubagentStop, not Stop"
    );
}

#[tokio::test]
async fn c6_subagent_stop_block_continues_subagent() {
    let llm = MockLlm::new(vec![resp("draft"), resp("final")]);
    let hooks = FakeHooks::new();
    hooks.push(
        HookEvent::SubagentStop,
        HookOutcome {
            blocked: true,
            feedback: Some("add tests".into()),
            ..Default::default()
        },
    );
    let (ex, _sink) = executor(llm.clone(), ToolRegistry::new(), hooks);
    let def = AgentDefinition {
        name: "w".into(),
        description: "d".into(),
        ..Default::default()
    };
    let out = ex.run_subagent(&def, "do", "t", 0).await.unwrap();
    assert_eq!(out, "final");
    assert_eq!(
        llm.requests()[1].messages.last().unwrap().text_ref(),
        Some("add tests")
    );
}

#[tokio::test]
async fn c6_depth_limit_refuses_nested_spawn() {
    let llm = MockLlm::new(vec![]);
    let hooks = FakeHooks::new();
    let sink = Arc::new(CollectSink::default());
    // The depth decision uses the depth passed to run_subagent (the
    // spawning agent's depth), not this snapshot: keep the base config
    // at depth 0 and pass the spawner depth explicitly.
    let cfg = base_cfg();
    let ex = SubAgentExecutor::new(
        llm,
        ToolRegistry::new(),
        allow(),
        hooks,
        sink,
        temp_ctx(),
        cfg,
    );
    let def = AgentDefinition {
        name: "deep".into(),
        description: "d".into(),
        ..Default::default()
    };
    let err = ex
        .run_subagent(&def, "x", "t", 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("depth limit"));
}

#[tokio::test]
async fn c6_depth_limit_enforced_across_reused_executor() {
    // Issue #327: spawning through the SAME executor must enforce the
    // depth limit from the spawning agent's actual depth, not from the
    // executor's depth-0 snapshot (which used to recompute depth 1 for
    // every nested spawn, nesting without limit).
    let llm = MockLlm::new(vec![resp("done"), resp("done")]);
    let reg = registry_with(vec![
        fake("Read", true, "r"),
        fake("Task", true, "t"),
        fake("delegate", true, "d"),
    ]);
    let (mut ex, _sink) = executor(llm.clone(), reg, FakeHooks::new());
    ex.set_max_depth(2);
    let def = AgentDefinition {
        name: "w".into(),
        description: "d".into(),
        ..Default::default()
    };
    let tool_names = |req: usize| {
        llm.requests()[req]
            .tools
            .iter()
            .map(|t| t.function.name.clone())
            .collect::<Vec<String>>()
    };
    // Depth-0 spawner: child is depth 1, keeps Task/delegate.
    let out = ex.run_subagent(&def, "go", "t", 0).await.unwrap();
    assert_eq!(out, "done");
    let names = tool_names(0);
    assert!(names.contains(&"Task".to_string()), "{names:?}");
    assert!(names.contains(&"delegate".to_string()), "{names:?}");
    // Depth-1 spawner: child is depth 2, Task/delegate are stripped.
    let out = ex.run_subagent(&def, "go", "t", 1).await.unwrap();
    assert_eq!(out, "done");
    let names = tool_names(1);
    assert!(!names.contains(&"Task".to_string()), "{names:?}");
    assert!(!names.contains(&"delegate".to_string()), "{names:?}");
    // Depth-2 spawner: a third level is refused.
    let err = ex
        .run_subagent(&def, "go", "t", 2)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("depth limit"), "{err}");
}

#[tokio::test]
async fn c6_1_preloads_skills_into_subagent_system_prompt() {
    let llm = MockLlm::new(vec![resp("done")]);
    let (mut ex, sink) = executor(llm.clone(), ToolRegistry::new(), FakeHooks::new());
    let cwd = ex.ctx.cwd.lock().unwrap().clone();
    let skill_dir = cwd.join(".zen/skills/style");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: style\ndescription: S\n---\nUse tabs. See ${ZEN_SKILL_DIR}/x\n",
    )
    .unwrap();
    ex.skills = vec![SkillInfo {
        name: "known".into(),
        description: "k".into(),
        path: std::path::PathBuf::from("/k/SKILL.md"),
        body: "KNOWN BODY".into(),
        allowed_tools: vec![],
    }];
    let def = AgentDefinition {
        name: "w".into(),
        description: "d".into(),
        system_prompt: "Base.".into(),
        skills: vec!["style".into(), "known".into(), "ghost".into()],
        ..Default::default()
    };
    assert_eq!(ex.run_subagent(&def, "go", "t", 0).await.unwrap(), "done");
    let sys = llm.requests()[0].messages[0].text();
    assert!(sys.starts_with("Base."));

    let norm = |p: &str| p.replace('\\', "/");
    let want = format!("## Skill: style\nUse tabs. See {}/x", skill_dir.display());
    assert!(norm(&sys).contains(&norm(&want)), "{}", sys);
    assert!(!sys.contains("description: S"), "frontmatter stripped");
    assert!(sys.contains("## Skill: known\nKNOWN BODY"));
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::SystemMessage { text, .. } if text.contains("'ghost' not found"))));
}

#[tokio::test]
async fn c6_3_builtin_read_only_agent_and_no_nested_task_by_default() {
    let llm = MockLlm::new(vec![resp_tool("Grep", "{}", "g1"), resp("only this")]);
    let reg = registry_with(vec![
        fake("Read", true, "r"),
        fake("Grep", true, "grep-out"),
        fake("Write", false, "w"),
        fake("Bash", false, "b"),
        fake("Task", true, "t"),
    ]);
    let (ex, _sink) = executor(llm.clone(), reg, FakeHooks::new());
    let def = AgentDefinition {
        name: "Plan".into(),
        description: "d".into(),
        tools: Some(vec!["Read".into(), "Grep".into(), "Task".into()]),
        permission_mode: Some("plan".into()),
        ..Default::default()
    };
    let out = ex
        .run_subagent(&def, "investigate", "task-9", 0)
        .await
        .unwrap();
    assert_eq!(
        out, "only this",
        "intermediate tool output is not returned to the parent"
    );
    let names: Vec<String> = llm.requests()[0]
        .tools
        .iter()
        .map(|t| t.function.name.clone())
        .collect();

    assert_eq!(names, vec!["Grep".to_string(), "Read".to_string()]);

    let llm2 = MockLlm::new(vec![resp("ok")]);
    let reg2 = registry_with(vec![fake("Read", true, "r"), fake("Task", true, "t")]);
    let (mut ex2, _s) = executor(llm2.clone(), reg2, FakeHooks::new());
    ex2.set_max_depth(2);
    let gp = AgentDefinition {
        name: "gp".into(),
        description: "d".into(),
        ..Default::default()
    };
    ex2.run_subagent(&gp, "x", "t", 0).await.unwrap();
    let names2: Vec<String> = llm2.requests()[0]
        .tools
        .iter()
        .map(|t| t.function.name.clone())
        .collect();
    assert!(names2.contains(&"Task".to_string()));
}

#[test]
fn c6_subagent_mode_cannot_escalate() {
    use PermissionMode::*;
    assert_eq!(subagent_mode(Default, Some("bypassPermissions")), Default);
    assert_eq!(subagent_mode(Default, Some("acceptEdits")), Default);
    assert_eq!(subagent_mode(Default, Some("plan")), Plan);
    assert_eq!(subagent_mode(AcceptEdits, Some("acceptEdits")), AcceptEdits);
    assert_eq!(
        subagent_mode(BypassPermissions, Some("bypassPermissions")),
        BypassPermissions
    );
    assert_eq!(subagent_mode(Plan, Some("garbage")), Plan);
    assert_eq!(subagent_mode(Default, None), Default);
    // 'auto' and 'manual' are not modes: they must not silently
    // downgrade the parent mode to Default.
    assert_eq!(subagent_mode(Plan, Some("auto")), Plan);
    assert_eq!(subagent_mode(Plan, Some("manual")), Plan);
    // Kebab-case spellings parse the same as camelCase.
    assert_eq!(
        subagent_mode(AcceptEdits, Some("accept-edits")),
        AcceptEdits
    );
}

struct YesPrompter;
impl Prompter for YesPrompter {
    fn ask_permission(&self, _: &str, _: &Value) -> PromptAnswer {
        PromptAnswer::Yes
    }
}

#[tokio::test]
async fn c3_2_notification_hook_fires_before_permission_prompt() {
    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w1"), resp("ok")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let (mut agent, _s, hooks) = build_agent(
        llm,
        reg,
        FakePerms::new(PermissionDecision::Ask),
        base_cfg(),
    );
    agent.prompter = Some(Arc::new(YesPrompter));
    let r = agent.run_turn("go").await;
    assert!(r.permission_denials.is_empty());
    let calls = hooks.calls.lock().unwrap();
    let n = calls
        .iter()
        .find(|c| c.0 == HookEvent::Notification)
        .expect("Notification fired");
    assert_eq!(n.1, "permission_prompt");
    assert_eq!(n.2["notification_type"], "permission_prompt");
    assert!(n.2["message"].as_str().unwrap().contains("Write"));
}

struct AlwaysTwo;

impl Prompter for AlwaysTwo {
    fn ask_permission(&self, _tool: &str, _input: &Value) -> PromptAnswer {
        PromptAnswer::Always("Bash(git add *)\nBash(git commit *)".into())
    }
}

#[tokio::test]
async fn c2_12_always_allow_persists_one_rule_per_line() {
    let llm = MockLlm::new(vec![resp_tool("Bash", "{}", "b"), resp("ok")]);
    let reg = registry_with(vec![fake("Bash", false, "ran")]);
    let perms = FakePerms::new(PermissionDecision::Ask);
    let mut agent = crate::Agent::new(
        llm,
        reg,
        perms.clone(),
        Some(Arc::new(AlwaysTwo)),
        FakeHooks::new(),
        Arc::new(CollectSink::default()),
        temp_ctx(),
        base_cfg(),
    );
    let r = agent.run_turn("go").await;
    assert!(r.permission_denials.is_empty());
    assert_eq!(
        *perms.always.lock().unwrap(),
        vec![
            "Bash(git add *)".to_string(),
            "Bash(git commit *)".to_string()
        ]
    );
}

struct RecordingTool {
    calls: std::sync::Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "records args"
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn call(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        self.calls.lock().unwrap().push(args);
        ToolOutput::ok("edited")
    }
}

#[tokio::test]
async fn r7_fenced_args_and_lowercase_name_are_repaired_and_run() {
    let raw = "```json{\"file_path\":\"a.rs\",\"old_string\":\"x\",\"new_string\":\"y\",\"replace_all\":\"true\",}```";
    let llm = MockLlm::new(vec![resp_tool("str_replace", raw, "e1"), resp("done")]);
    let tool = Arc::new(RecordingTool {
        calls: Default::default(),
    });
    let reg = registry_with(vec![tool.clone() as Arc<dyn Tool>]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(
        tool.calls.lock().unwrap().clone(),
        vec![json!({"file_path":"a.rs","old_string":"x","new_string":"y","replace_all":true})]
    );

    let second = &llm.requests()[1];
    let tool_msg = second.messages.iter().find(|m| m.role == "tool").unwrap();
    let content = tool_msg.text_ref().unwrap();
    assert!(content.starts_with("edited"), "{}", content);
    assert!(
        content.contains("mapped to 'Edit'") && content.contains("code fence"),
        "{}",
        content
    );
    let call = &second
        .messages
        .iter()
        .find_map(|m| m.tool_calls.clone())
        .unwrap()[0];
    assert_eq!(call.function.name, "Edit");
    assert_eq!(
        serde_json::from_str::<Value>(&call.function.arguments).unwrap()["replace_all"],
        json!(true)
    );

    assert!(
        hooks
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(ev, target, _)| *ev == HookEvent::PreToolUse && target == "Edit")
    );
}

#[tokio::test]
async fn c4_leaked_template_tokens_in_tool_name_are_cut_and_not_echoed() {
    let llm = MockLlm::new(vec![
        resp_tool(
            "Edit<\u{200B}|channel|>commentary",
            r#"{"file_path":"a","old_string":"x","new_string":"y"}"#,
            "e1",
        ),
        resp("done"),
    ]);
    let tool = Arc::new(RecordingTool {
        calls: Default::default(),
    });
    let reg = registry_with(vec![tool.clone() as Arc<dyn Tool>]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(tool.calls.lock().unwrap().len(), 1, "the call runs as Edit");
    let second = &llm.requests()[1];
    let call = &second
        .messages
        .iter()
        .find_map(|m| m.tool_calls.clone())
        .unwrap()[0];
    assert_eq!(call.function.name, "Edit", "history carries the clean name");
    let body = serde_json::to_string(&second.messages).unwrap();
    assert!(
        !body.contains("|channel|"),
        "no leaked tokens anywhere in the next request: {body}"
    );
}

#[tokio::test]
async fn c5_empty_reply_is_requeried_then_accepted_after_two() {
    let llm = MockLlm::new(vec![resp(""), resp("done")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), registry_with(vec![]), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    let second = &llm.requests()[1];
    let last = second.messages.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(last.text_ref().unwrap().contains("reply was empty"));
    assert!(
        !second.messages.iter().any(|m| m.role == "assistant"
            && m.text_ref().unwrap_or("").is_empty()
            && m.tool_calls.is_none()),
        "the empty reply is not kept in history"
    );

    let llm = MockLlm::new(vec![resp(""), resp(" "), resp(""), resp("never")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), registry_with(vec![]), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result.trim(), "");
    assert_eq!(llm.requests().len(), 3);
}

#[tokio::test]
async fn c7_unparsed_sample_error_is_requeried_but_other_errors_end_the_run() {
    let llm = MockLlm::with_errors(vec![
        Err(anyhow::anyhow!(
            "model returned no choices (body: {{\"error\":{{\"message\":\"unexpected tokens remaining in message header\"}}}})"
        )),
        Ok(resp("done")),
    ]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), registry_with(vec![]), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    let last = llm.requests()[1].messages.last().unwrap().clone();
    assert!(last.text().contains("could not parse your last reply"));

    let llm = MockLlm::with_errors(vec![
        Err(anyhow::anyhow!("HTTP 401: bad key")),
        Ok(resp("never")),
    ]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), registry_with(vec![]), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert!(r.is_error && r.result.contains("401"), "{}", r.result);
    assert_eq!(llm.requests().len(), 1);
}

#[tokio::test]
async fn r7_well_formed_call_gets_no_repair_note_and_bad_call_lists_fields() {
    let llm = MockLlm::new(vec![
        resp_tool(
            "Edit",
            r#"{"file_path":"a","old_string":"x","new_string":"y"}"#,
            "ok1",
        ),
        resp_tool("Edit", "{file_path: a", "bad1"),
        resp("done"),
    ]);
    let tool = Arc::new(RecordingTool {
        calls: Default::default(),
    });
    let reg = registry_with(vec![tool.clone() as Arc<dyn Tool>]);
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), base_cfg());
    agent.run_turn("go").await;
    let tools: Vec<String> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref().map(str::to_string))
        .collect();
    assert_eq!(tools[0], "edited");
    assert!(
        tools[1].contains("Invalid JSON arguments")
            && tools[1].contains("file_path (string, required)"),
        "{}",
        tools[1]
    );
    assert_eq!(
        tool.calls.lock().unwrap().len(),
        1,
        "the unrepairable call must not run"
    );
}

fn tool_contents(msgs: &[ChatMessage]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn r7_fifth_edit_turn_gets_exactly_one_guard_note() {
    // NOTE: args vary per call so the loop guard does not trip; the
    // edit nudge under test keys on file_path only. Five edits in ONE
    // model turn nudge exactly once, at the fourth edit; the same file
    // edited in the next model turn starts a fresh count (#345), so no
    // second nudge fires there.
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            (
                "Edit",
                r#"{"file_path":"a.rs","old_string":"x0","new_string":"y"}"#,
                "e0",
            ),
            (
                "Edit",
                r#"{"file_path":"a.rs","old_string":"x1","new_string":"y"}"#,
                "e1",
            ),
            (
                "Edit",
                r#"{"file_path":"a.rs","old_string":"x2","new_string":"y"}"#,
                "e2",
            ),
            (
                "Edit",
                r#"{"file_path":"a.rs","old_string":"x3","new_string":"y"}"#,
                "e3",
            ),
            (
                "Edit",
                r#"{"file_path":"a.rs","old_string":"x4","new_string":"y"}"#,
                "e4",
            ),
        ]),
        resp_tool(
            "Edit",
            r#"{"file_path":"a.rs","old_string":"x5","new_string":"y"}"#,
            "e5",
        ),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Edit", false, "edited")]);
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), base_cfg());
    agent.run_turn("go").await;
    let tools = tool_contents(agent.messages());
    let noted: Vec<usize> = tools
        .iter()
        .enumerate()
        .filter(|(_, c)| c.contains("changed a.rs 4 times"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(noted, vec![3], "{:#?}", tools);

    let mut script: Vec<ChatResponse> = (0..3)
        .map(|i| {
            resp_tool(
                "Edit",
                &format!(r#"{{"file_path":"a.rs","old_string":"y{}"}}"#, i),
                &format!("f{}", i),
            )
        })
        .collect();
    script.push(resp("done"));
    agent.llm = MockLlm::new(script);
    agent.run_turn("again").await;
    assert_eq!(
        tool_contents(agent.messages())
            .iter()
            .filter(|c| c.contains("[zen:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn r7_two_identical_failures_nudge() {
    // Two identical failures in ONE model turn nudge; the same failures
    // spread over separate turns start a fresh count each turn (#345).
    let failing: Arc<dyn Tool> = Arc::new(FakeTool {
        name: "Bash",
        read_only: false,
        output: ToolOutput::err("exit 1"),
    });
    let (mut agent, _s, _h) = build_agent(
        MockLlm::new(vec![
            resp_multi_tool(vec![
                ("Bash", r#"{"command":"make"}"#, "b0"),
                ("Bash", r#"{"command":"make"}"#, "b1"),
                ("Bash", r#"{"command":"make"}"#, "b2"),
            ]),
            resp("done"),
        ]),
        registry_with(vec![failing]),
        allow(),
        base_cfg(),
    );
    agent.run_turn("go").await;
    let tools = tool_contents(agent.messages());
    assert!(
        !tools[0].contains("[zen:") && tools[1].contains("failed 2 times"),
        "{:#?}",
        tools
    );
}

#[tokio::test]
async fn r7_todos_reinjected_every_sixth_call_and_after_compaction() {
    // NOTE: args vary per call so the loop guard does not trip; the
    // todo reinjection under test keys on call count only.
    let mut script: Vec<ChatResponse> = (0..12)
        .map(|i| resp_tool("Echo", &format!(r#"{{"n":{}}}"#, i), &format!("c{}", i)))
        .collect();
    script.push(resp("done"));
    script.push(resp("SUMMARY TEXT"));
    let llm = MockLlm::new(script);
    let reg = registry_with(vec![fake("Echo", true, "ok")]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    *agent.ctx.todos.lock().unwrap() = vec![TodoItem {
        content: "fix the parser".into(),
        status: "in_progress".into(),
        active_form: None,
    }];
    agent.run_turn("go").await;

    let reqs = llm.requests();
    let with_todo: Vec<usize> = reqs
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.messages
                .last()
                .and_then(|m| m.text_ref())
                .is_some_and(|c| c.contains("[~] fix the parser"))
        })
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(with_todo, vec![6, 12]);

    agent.compact(None, "manual").await.unwrap();
    let summary = agent.messages()[0].text();
    assert!(
        summary.contains("SUMMARY TEXT") && summary.contains("[~] fix the parser"),
        "{}",
        summary
    );
}

#[tokio::test]
async fn r7_short_turn_and_empty_todo_list_get_no_notes() {
    let mut script: Vec<ChatResponse> = (0..7)
        .map(|i| {
            resp_tool(
                "Edit",
                &format!(r#"{{"file_path":"f{}.rs"}}"#, i),
                &format!("c{}", i),
            )
        })
        .collect();
    script.push(resp("done"));
    let reg = registry_with(vec![fake("Edit", false, "edited")]);
    let (mut agent, _s, _h) = build_agent(MockLlm::new(script), reg, allow(), base_cfg());
    agent.run_turn("go").await;
    assert!(
        agent
            .messages()
            .iter()
            .all(|m| !m.text_ref().unwrap_or("").contains("[zen:"))
    );
}

#[tokio::test]
async fn r7_edit_then_answer_gets_one_reminder_then_bash_then_success() {
    let llm = MockLlm::new(vec![
        resp_tool("Edit", r#"{"file_path":"src/a.rs"}"#, "e1"),
        resp("looks fine"),
        resp_tool("Bash", r#"{"command":"cargo test"}"#, "b1"),
        resp("tests pass"),
    ]);
    let reg = registry_with(vec![
        fake("Edit", false, "edited"),
        fake("Bash", false, "ok"),
    ]);
    let cfg = AgentConfig {
        verify_reminder: true,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("fix it").await;
    assert_eq!(r.result, "tests pass");
    assert_eq!(r.num_turns, 4);
    let reminders: Vec<&ChatMessage> = agent
        .messages()
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.text_ref()
                    .unwrap_or("")
                    .contains("have not run any command")
        })
        .collect();
    assert_eq!(reminders.len(), 1);
    assert!(reminders[0].text_ref().unwrap().contains("src/a.rs"));
}

#[tokio::test]
async fn r7_no_reminder_without_edits_or_when_disabled() {
    for (cfg, script) in [
        (
            AgentConfig {
                verify_reminder: true,
                ..base_cfg()
            },
            vec![resp_tool("Read", "{}", "r1"), resp("answer")],
        ),
        (
            base_cfg(),
            vec![
                resp_tool("Edit", r#"{"file_path":"a"}"#, "e1"),
                resp("answer"),
            ],
        ),
    ] {
        let reg = registry_with(vec![
            fake("Read", true, "text"),
            fake("Edit", false, "edited"),
        ]);
        let (mut agent, _s, _h) = build_agent(MockLlm::new(script), reg, allow(), cfg);
        let r = agent.run_turn("q").await;
        assert_eq!((r.result.as_str(), r.num_turns), ("answer", 2));
    }
}

#[tokio::test]
async fn r7_failing_verify_command_stops_after_two_rounds() {
    let mut script = Vec::new();
    for i in 0..3 {
        // NOTE: args vary per round so the loop guard does not trip;
        // the verify loop under test does not depend on Edit args.
        script.push(resp_tool(
            "Edit",
            &format!(r#"{{"file_path":"a.rs","old_string":"x{}"}}"#, i),
            &format!("e{}", i),
        ));
        script.push(resp(&format!("attempt {}", i)));
    }
    let llm = MockLlm::new(script);
    let reg = registry_with(vec![fake("Edit", false, "edited")]);
    let cfg = AgentConfig {
        verify_reminder: true,
        verify_command: Some("echo FAILING-CHECK; exit 1".into()),
        ..base_cfg()
    };
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("fix").await;
    assert!(!r.is_error);
    assert_eq!(
        r.result, "attempt 2",
        "the turn ends after two automatic rounds"
    );
    assert_eq!(llm.requests().len(), 6);
    let feedback = agent
        .messages()
        .iter()
        .filter(|m| m.role == "user" && m.text_ref().unwrap_or("").contains("FAILING-CHECK"))
        .count();
    assert_eq!(feedback, 2);
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::SystemMessage { event, .. } if event == "Verify")));
}

#[tokio::test]
async fn r7_tool_call_written_as_text_runs_as_a_call() {
    let llm = MockLlm::new(vec![
        resp(
            r#"{"name": "Edit", "parameters": {"file_path": "a.rs", "old_string": "x", "new_string": "y"}}"#,
        ),
        resp("done"),
    ]);
    let tool = Arc::new(RecordingTool {
        calls: Default::default(),
    });
    let reg = registry_with(vec![tool.clone() as Arc<dyn Tool>]);
    let (mut agent, _s, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(tool.calls.lock().unwrap().len(), 1);
    let second = &llm.requests()[1];
    let tool_msg = second.messages.iter().find(|m| m.role == "tool").unwrap();
    assert!(
        tool_msg
            .text_ref()
            .unwrap()
            .contains("written as message text")
    );
    assert_paired(agent.messages());
}

struct SlowTool {
    delay_ms: u64,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "Slow"
    }
    fn description(&self) -> &str {
        "slow fake tool"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        ToolOutput::ok("slow done")
    }
}

#[tokio::test]
async fn tool_progress_heartbeat_emitted_for_slow_tool() {
    use crate::agent::run_tool_with_heartbeat;
    let sink = Arc::new(CollectSink::default());
    let tool: Arc<dyn Tool> = Arc::new(SlowTool { delay_ms: 250 });
    let ctx = temp_ctx();
    let out = run_tool_with_heartbeat(
        tool,
        json!({}),
        ctx,
        (None, crate::agent::TimeoutSource::None),
        sink.clone(),
        "call_slow_1".into(),
        "Slow".into(),
        std::time::Duration::from_millis(50),
    )
    .await;
    assert_eq!(out.content, "slow done");
    let events = sink.events.lock().unwrap();
    let progress: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_ms,
            } => Some((tool_use_id.clone(), tool_name.clone(), *elapsed_ms)),
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty(), "expected at least one heartbeat");
    for (id, name, _) in &progress {
        assert_eq!(id, "call_slow_1");
        assert_eq!(name, "Slow");
    }

    let elapsed: Vec<u64> = progress.iter().map(|(_, _, ms)| *ms).collect();
    assert!(
        elapsed.windows(2).all(|w| w[1] > w[0]),
        "elapsed not increasing: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn tool_progress_no_heartbeat_for_fast_tool() {
    use crate::agent::run_tool_with_heartbeat;
    let sink = Arc::new(CollectSink::default());
    let tool: Arc<dyn Tool> = Arc::new(SlowTool { delay_ms: 10 });
    let ctx = temp_ctx();
    let out = run_tool_with_heartbeat(
        tool,
        json!({}),
        ctx,
        (None, crate::agent::TimeoutSource::None),
        sink.clone(),
        "call_fast_1".into(),
        "Slow".into(),
        std::time::Duration::from_secs(60),
    )
    .await;
    assert_eq!(out.content, "slow done");
    let events = sink.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolProgress { .. })),
        "fast tool should emit no heartbeat"
    );
}

struct PanickingTool;

#[async_trait::async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &str {
        "Panicking"
    }
    fn description(&self) -> &str {
        "fake tool that panics"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        panic!("deliberate panic for the #367 regression test");
    }
}

fn count_progress(sink: &CollectSink) -> usize {
    sink.events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolProgress { .. }))
        .count()
}

#[tokio::test]
async fn heartbeat_aborted_when_tool_panics() {
    use crate::agent::run_tool_with_heartbeat;
    let sink = Arc::new(CollectSink::default());
    let tool: Arc<dyn Tool> = Arc::new(PanickingTool);
    let sink_clone = sink.clone();
    let handle = tokio::spawn(async move {
        run_tool_with_heartbeat(
            tool,
            json!({}),
            temp_ctx(),
            (None, crate::agent::TimeoutSource::None),
            sink_clone,
            "call_panic_1".into(),
            "Panicking".into(),
            std::time::Duration::from_millis(50),
        )
        .await
    });
    let joined = handle.await;
    assert!(
        joined.is_err() && joined.unwrap_err().is_panic(),
        "expected run_tool_with_heartbeat to propagate the tool panic"
    );
    // The heartbeat must stop once the tool panics: the ToolProgress count
    // must be stable across two windows after the panic.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let first = count_progress(&sink);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let second = count_progress(&sink);
    assert_eq!(
        first, second,
        "heartbeat kept emitting ToolProgress after the tool panicked (leaked task): {first} then {second}"
    );
}

#[tokio::test]
async fn budget_cap_blocks_before_first_call_when_tiny() {
    use crate::budget::BudgetPolicy;
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "gpt-4o".into();
    cfg.budget = Some(BudgetPolicy {
        max_budget_usd: 0.0001,
        warn_at: 0.8,
    });
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "error_budget_exceeded");
    assert!(r.is_error);
    assert!(r.result.contains("Budget exceeded"), "got: {}", r.result);
    assert!(r.result.contains("$0.00"), "got: {}", r.result);
    assert!(
        llm.requests().is_empty(),
        "no model call should have been made"
    );
}

#[tokio::test]
async fn budget_warns_once_at_threshold_then_stays_quiet() {
    use crate::budget::BudgetPolicy;
    let llm = MockLlm::new(vec![resp("hello"), resp("world")]);
    let mut cfg = base_cfg();
    cfg.model = "gpt-4o".into();
    cfg.budget = Some(BudgetPolicy {
        max_budget_usd: 1.0,
        warn_at: 0.5,
    });
    let (mut agent, sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);

    agent.usage = UsageStats {
        input_tokens: 200_000,
        output_tokens: 0,
        ..Default::default()
    };
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success");
    assert!(agent.budget_warned, "warn-once flag must be set");
    let warn_count = sink
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AgentEvent::SystemMessage { event, .. } if event == "budget"))
        .count();
    assert_eq!(warn_count, 1, "exactly one budget warning");

    let r2 = agent.run_turn("again").await;
    assert_eq!(r2.subtype, "success");
    let warn_count2 = sink
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AgentEvent::SystemMessage { event, .. } if event == "budget"))
        .count();
    assert_eq!(warn_count2, 1, "warning must not repeat");
}

#[tokio::test]
async fn budget_is_noop_for_free_models() {
    use crate::budget::BudgetPolicy;
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "qwen/qwen3-coder:free".into();
    cfg.budget = Some(BudgetPolicy {
        max_budget_usd: 0.000_001,
        warn_at: 0.8,
    });
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success", "free models never trip the cap");
    assert_eq!(llm.requests().len(), 1);
    assert!(!agent.budget_warned);
}

#[tokio::test]
async fn budget_allows_unknown_pricing() {
    use crate::budget::BudgetPolicy;
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "mystery-model-9999".into();
    cfg.budget = Some(BudgetPolicy {
        max_budget_usd: 0.000_001,
        warn_at: 0.8,
    });
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;

    assert_eq!(r.subtype, "success");
    assert_eq!(llm.requests().len(), 1);
}

#[tokio::test]
async fn no_budget_config_means_no_checks() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "gpt-4o".into();
    cfg.budget = None;
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;
    assert_eq!(r.subtype, "success");
    assert!(!agent.budget_warned);
}

#[tokio::test]
async fn plan_mode_rejects_hallucinated_mutating_call_by_the_harness() {
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"path":"x"}"#, "w1"),
        resp("plan done"),
    ]);
    let reg = registry_with(vec![
        fake("Read", true, "r"),
        fake("Write", false, "MUTATED"),
    ]);
    let cfg = AgentConfig {
        permission_mode: PermissionMode::Plan,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    let tool_contents: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref())
        .collect();
    assert_eq!(tool_contents.len(), 1, "one tool result expected");
    assert!(
        tool_contents[0].contains("not available in plan mode"),
        "harness must reject with a clear error, got: {}",
        tool_contents[0]
    );
    assert!(
        !agent
            .messages()
            .iter()
            .any(|m| m.text_ref().unwrap_or("").contains("MUTATED")),
        "the mutating tool must never execute in plan mode"
    );
    assert!(
        r.permission_denials.is_empty(),
        "plan-mode rejection is not a permission denial"
    );
    assert_eq!(r.result, "plan done");
    assert_paired(agent.messages());
}

/// #355: read-only-but-denied tools (Task, delegate, JobOutput) report
/// `is_read_only() == true`, so the old `plan_mode && !read_only` gate in
/// `prepare_call` let a hallucinated call to them run in plan mode even
/// though `plan_mode_hides` hides them from the model.
#[tokio::test]
async fn plan_mode_rejects_read_only_but_denied_tools() {
    let llm = MockLlm::new(vec![
        resp_tool("Task", r#"{"prompt":"x"}"#, "t1"),
        resp_tool("delegate", r#"{"task":"x"}"#, "t2"),
        resp_tool("JobOutput", r#"{"job_id":"j"}"#, "t3"),
        resp("plan done"),
    ]);
    let reg = registry_with(vec![
        fake("Read", true, "r"),
        fake("Task", true, "TASK_RAN"),
        fake("delegate", true, "DELEGATE_RAN"),
        fake("JobOutput", true, "JOBOUTPUT_RAN"),
    ]);
    let cfg = AgentConfig {
        permission_mode: PermissionMode::Plan,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    let tool_contents: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref())
        .collect();
    assert_eq!(tool_contents.len(), 3, "one tool result per call expected");
    for content in &tool_contents {
        assert!(
            content.contains("not available in plan mode"),
            "plan mode must reject the denied tool, got: {content}"
        );
    }
    for marker in ["TASK_RAN", "DELEGATE_RAN", "JOBOUTPUT_RAN"] {
        assert!(
            !agent
                .messages()
                .iter()
                .any(|m| m.text_ref().unwrap_or("").contains(marker)),
            "denied tool {marker} must never execute in plan mode"
        );
    }
    assert_eq!(r.result, "plan done");
    assert_paired(agent.messages());
}

/// #355 (mutant kill): `prepare_call` gates plan mode on
/// `!read_only || plan_mode_hides(...)`, so a plain read-only tool that
/// is not denied must still run in plan mode — plan mode is for
/// researching with read-only tools. Dropping the `!` would reject
/// every read-only tool, including `Read`.
#[tokio::test]
async fn plan_mode_still_runs_plain_read_only_tools() {
    let llm = MockLlm::new(vec![
        resp_tool("Read", r#"{"path":"src/lib.rs"}"#, "c1"),
        resp("plan done"),
    ]);
    let reg = registry_with(vec![fake("Read", true, "READ_RAN")]);
    let cfg = AgentConfig {
        permission_mode: PermissionMode::Plan,
        ..base_cfg()
    };
    let (mut agent, _s, _h) = build_agent(llm, reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    let tool_contents: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.text_ref())
        .collect();
    assert_eq!(tool_contents.len(), 1, "one tool result expected");
    assert!(
        tool_contents[0].contains("READ_RAN"),
        "plain read-only Read must execute in plan mode, got: {}",
        tool_contents[0]
    );
    assert_eq!(r.result, "plan done");
    assert_paired(agent.messages());
}

use std::sync::atomic::Ordering;

#[tokio::test]
async fn parallel_reads_run_concurrently_and_keep_order() {
    let (tool, max) = ConcurrencyTool::read(100);
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            ("Read", r#"{"p":"a"}"#, "r1"),
            ("Read", r#"{"p":"b"}"#, "r2"),
            ("Read", r#"{"p":"c"}"#, "r3"),
            ("Read", r#"{"p":"d"}"#, "r4"),
        ]),
        resp("done"),
    ]);
    let reg = registry_with(vec![tool as Arc<dyn Tool>]);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(r.num_turns, 2);

    assert_eq!(
        max.load(Ordering::SeqCst),
        4,
        "expected all 4 reads to run concurrently"
    );

    let ids: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["r1", "r2", "r3", "r4"]);

    let second = &llm.requests()[1];
    let tool_msgs: Vec<_> = second
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 4);

    let events = sink.events.lock().unwrap();
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                tool_use_id,
                parallel,
                ..
            } => Some((tool_use_id.clone(), *parallel)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4);
    assert!(
        results.iter().all(|(_, p)| *p),
        "all four ran in a parallel batch: {results:?}"
    );
    assert_paired(agent.messages());
}

#[tokio::test]
async fn max_parallel_tool_calls_caps_concurrency() {
    let (tool, max) = ConcurrencyTool::read(80);
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            ("Read", r#"{"p":"a"}"#, "r1"),
            ("Read", r#"{"p":"b"}"#, "r2"),
            ("Read", r#"{"p":"c"}"#, "r3"),
            ("Read", r#"{"p":"d"}"#, "r4"),
        ]),
        resp("done"),
    ]);
    let reg = registry_with(vec![tool as Arc<dyn Tool>]);
    let cfg = AgentConfig {
        max_parallel_tool_calls: 2,
        ..base_cfg()
    };
    let (mut agent, _sink, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    let observed = max.load(Ordering::SeqCst);
    assert!(observed <= 2, "cap of 2 exceeded: observed {observed}");
    assert!(
        observed >= 2,
        "expected some overlap with cap 2, observed {observed}"
    );
    let ids: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["r1", "r2", "r3", "r4"]);
    assert_paired(agent.messages());
}

#[tokio::test]
async fn max_parallel_tool_calls_one_is_sequential() {
    let (tool, max) = ConcurrencyTool::read(50);
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            ("Read", r#"{"p":"a"}"#, "r1"),
            ("Read", r#"{"p":"b"}"#, "r2"),
            ("Read", r#"{"p":"c"}"#, "r3"),
        ]),
        resp("done"),
    ]);
    let reg = registry_with(vec![tool as Arc<dyn Tool>]);
    let cfg = AgentConfig {
        max_parallel_tool_calls: 1,
        ..base_cfg()
    };
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), cfg);
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert_eq!(
        max.load(Ordering::SeqCst),
        1,
        "calls must not overlap with cap 1"
    );
    let events = sink.events.lock().unwrap();
    assert!(
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { parallel, .. } => Some(*parallel),
                _ => None,
            })
            .all(|p| !p)
    );
    assert_paired(agent.messages());
}

#[tokio::test]
async fn error_in_one_parallel_call_does_not_block_others() {
    let (reads, _max) = ConcurrencyTool::read(50);
    let failing = Arc::new(FakeTool {
        name: "Glob",
        read_only: true,
        output: ToolOutput::err("boom"),
    });
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            ("Read", r#"{"p":"a"}"#, "r1"),
            ("Glob", r#"{"p":"*"}"#, "g1"),
            ("Read", r#"{"p":"b"}"#, "r2"),
        ]),
        resp("done"),
    ]);
    let reg = registry_with(vec![reads as Arc<dyn Tool>, failing as Arc<dyn Tool>]);
    let (mut agent, _sink, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    let tool_msgs: Vec<(&str, &str)> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| (m.tool_call_id.as_deref().unwrap(), m.text_ref().unwrap()))
        .collect();
    assert_eq!(tool_msgs.len(), 3);
    assert_eq!(tool_msgs[0].0, "r1");
    assert_eq!(tool_msgs[1].0, "g1");
    assert!(tool_msgs[1].1.contains("boom"));
    assert_eq!(tool_msgs[2].0, "r2");
    assert_paired(agent.messages());
}

#[tokio::test]
async fn write_flushes_parallel_batch_and_runs_alone() {
    let (reads, max) = ConcurrencyTool::read(50);
    let write = Arc::new(FakeTool {
        name: "Write",
        read_only: false,
        output: ToolOutput::ok("wrote"),
    });
    let llm = MockLlm::new(vec![
        resp_multi_tool(vec![
            ("Read", r#"{"p":"a"}"#, "r1"),
            ("Read", r#"{"p":"b"}"#, "r2"),
            ("Write", r#"{"p":"c"}"#, "w1"),
            ("Read", r#"{"p":"d"}"#, "r3"),
        ]),
        resp("done"),
    ]);
    let reg = registry_with(vec![reads as Arc<dyn Tool>, write as Arc<dyn Tool>]);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");

    assert!(max.load(Ordering::SeqCst) <= 2);
    let ids: Vec<&str> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["r1", "r2", "w1", "r3"]);
    let events = sink.events.lock().unwrap();
    let by_id: std::collections::HashMap<String, bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                tool_use_id,
                parallel,
                ..
            } => Some((tool_use_id.clone(), *parallel)),
            _ => None,
        })
        .collect();
    assert_eq!(
        by_id.get("w1"),
        Some(&false),
        "the write must not be marked parallel"
    );
    assert_eq!(by_id.get("r3"), Some(&false), "a lone call is not parallel");
    assert_paired(agent.messages());
}

#[tokio::test]
async fn result_total_cost_is_priced_from_usage_for_paid_model() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "gpt-4o".into();
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;

    let expected = crate::budget::estimate_cost_usd("gpt-4o", 10, 5).expect("gpt-4o is priced");
    assert!(expected > 0.0);
    assert!(
        (r.total_cost_usd - expected).abs() < 1e-12,
        "result total_cost_usd {} != priced cost {}",
        r.total_cost_usd,
        expected
    );
}

#[tokio::test]
async fn result_total_cost_is_a_genuine_zero_for_free_models() {
    let llm = MockLlm::new(vec![resp("hello")]);
    let mut cfg = base_cfg();
    cfg.model = "qwen/qwen3-coder:free".into();
    let (mut agent, _sink, _h) = build_agent(llm.clone(), ToolRegistry::new(), allow(), cfg);
    let r = agent.run_turn("hi").await;

    assert!(r.usage.input_tokens > 0 && r.usage.output_tokens > 0);
    assert_eq!(r.total_cost_usd, 0.0);
}

struct CountingFlakyTool {
    name: String,
    fail_text: String,
    always_fail: bool,
    read_only: bool,
    calls: std::sync::Mutex<u32>,
}

#[async_trait::async_trait]
impl Tool for CountingFlakyTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "counting flaky fake tool"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        let mut guard = self.calls.lock().unwrap();
        *guard += 1;
        let n = *guard;
        drop(guard);
        if self.always_fail || n == 1 {
            ToolOutput::err(self.fail_text.clone())
        } else {
            ToolOutput::ok("flaky done")
        }
    }
}

fn flaky_tool(
    name: &str,
    fail_text: &str,
    always_fail: bool,
    read_only: bool,
) -> (Arc<CountingFlakyTool>, Arc<dyn Tool>) {
    let concrete = Arc::new(CountingFlakyTool {
        name: name.to_string(),
        fail_text: fail_text.to_string(),
        always_fail,
        read_only,
        calls: std::sync::Mutex::new(0),
    });
    let tool: Arc<dyn Tool> = concrete.clone();
    (concrete, tool)
}

async fn run_flaky_once(tool: Arc<dyn Tool>, name: &str) -> ToolOutput {
    use crate::agent::run_tool_with_heartbeat;
    run_tool_with_heartbeat(
        tool,
        json!({}),
        temp_ctx(),
        (None, crate::agent::TimeoutSource::None),
        Arc::new(CollectSink::default()),
        "call_b61".into(),
        name.into(),
        std::time::Duration::from_secs(60),
    )
    .await
}

#[tokio::test]
async fn b61_retry_transient_then_success() {
    let (counter, tool) = flaky_tool("Read", "Error: connection reset by peer", false, true);
    let out = run_flaky_once(tool, "Read").await;
    assert!(
        !out.is_error,
        "expected success after retry, got: {}",
        out.content
    );
    assert_eq!(out.content, "flaky done");
    assert_eq!(*counter.calls.lock().unwrap(), 2);
}

#[tokio::test]
async fn b61_no_retry_deterministic_error() {
    let (counter, tool) = flaky_tool("Read", "Error: file not found", true, true);
    let out = run_flaky_once(tool, "Read").await;
    assert!(out.is_error);
    assert_eq!(*counter.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn b61_no_retry_out_of_scope_tool() {
    let (counter, tool) = flaky_tool("Write", "Error: connection reset by peer", false, true);
    let out = run_flaky_once(tool, "Write").await;
    assert!(out.is_error, "out-of-scope tool must not retry");
    assert_eq!(*counter.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn b61_retry_mcp_tool() {
    let (counter, tool) = flaky_tool(
        "mcp__github__search",
        "Error: service unavailable",
        false,
        false,
    );
    let out = run_flaky_once(tool, "mcp__github__search").await;
    assert!(
        !out.is_error,
        "expected success after retry, got: {}",
        out.content
    );
    assert_eq!(out.content, "flaky done");
    assert_eq!(*counter.calls.lock().unwrap(), 2);
}

#[tokio::test]
async fn b61_no_retry_mutating_bash() {
    let (counter, tool) = flaky_tool("Bash", "Error: connection reset by peer", false, false);
    let out = run_flaky_once(tool, "Bash").await;
    assert!(out.is_error, "mutating Bash must not retry");
    assert_eq!(*counter.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn b61_no_retry_internal_timeout() {
    let (counter, tool) = flaky_tool(
        "Read",
        "Error: Read timed out after 5000 ms and was stopped (settings toolTimeouts.default)",
        true,
        true,
    );
    let out = run_flaky_once(tool, "Read").await;
    assert!(out.is_error, "internal timeout must not retry");
    assert_eq!(*counter.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn b61_no_retry_transient_word_in_later_line() {
    let (counter, tool) = flaky_tool(
        "Read",
        "Error: tests failed
output mentions rate limit",
        true,
        true,
    );
    let out = run_flaky_once(tool, "Read").await;
    assert!(out.is_error, "transient word in later line must not retry");
    assert_eq!(*counter.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn b61_transient_always_fail_calls_three_times() {
    let (counter, tool) = flaky_tool("Read", "Error: connection reset by peer", true, true);
    let out = run_flaky_once(tool, "Read").await;
    assert!(out.is_error);
    assert_eq!(*counter.calls.lock().unwrap(), 3);
}

// --- Wave 1 #3: hook system, 33 events ---

fn todo_args(items: &[(&str, &str)]) -> String {
    let list: Vec<Value> = items
        .iter()
        .map(|(c, s)| json!({"content": c, "status": s}))
        .collect();
    serde_json::to_string(&json!({ "todos": list })).unwrap()
}

#[tokio::test]
async fn wave1_permission_request_allow_skips_prompt() {
    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w1"), resp("done")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let (mut agent, _s, hooks) = build_agent(
        llm,
        reg,
        FakePerms::new(PermissionDecision::Ask),
        base_cfg(),
    );
    hooks.push(
        HookEvent::PermissionRequest,
        HookOutcome {
            permission_override: Some(PermissionDecision::Allow),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    assert!(r.permission_denials.is_empty());
    let calls = hooks.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|(ev, target, p)| *ev == HookEvent::PermissionRequest
                && target == "Write"
                && p["tool_name"] == "Write"),
        "PermissionRequest should fire with the tool name: {:?}",
        calls
    );
}

#[tokio::test]
async fn wave1_permission_request_block_denies_tool() {
    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "w1"), resp("done")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let (mut agent, _s, hooks) = build_agent(
        llm,
        reg,
        FakePerms::new(PermissionDecision::Ask),
        base_cfg(),
    );
    hooks.push(
        HookEvent::PermissionRequest,
        HookOutcome {
            blocked: true,
            feedback: Some("hook says no".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.permission_denials.len(), 1);
    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool_msg.text_ref(), Some("hook says no"));
    let calls = hooks.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|(ev, _, _)| *ev == HookEvent::PermissionRequest)
    );
    assert!(
        calls
            .iter()
            .any(|(ev, _, _)| *ev == HookEvent::PermissionDenied),
        "a blocked request still records a denial: {:?}",
        calls
    );
}

#[tokio::test]
async fn wave1_permission_denied_hook_runs_and_retry_note() {
    let llm = MockLlm::new(vec![resp_tool("Write", r#"{"p":"a"}"#, "w1"), resp("ok")]);
    let reg = registry_with(vec![fake("Write", false, "wrote")]);
    let perms = FakePerms::new(PermissionDecision::Deny("rule says no".into()));
    let (mut agent, _s, hooks) = build_agent(llm, reg, perms, base_cfg());
    hooks.push(
        HookEvent::PermissionDenied,
        HookOutcome {
            allow_retry: true,
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.permission_denials.len(), 1);
    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    let text = tool_msg.text_ref().unwrap();
    assert!(text.contains("rule says no"), "{text}");
    assert!(text.contains("may retry"), "{text}");
    let calls = hooks.calls.lock().unwrap();
    let (_, _, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::PermissionDenied)
        .expect("PermissionDenied should fire");
    assert_eq!(payload["tool_name"], "Write");
    assert_eq!(payload["reason"], "rule says no");
}

#[tokio::test]
async fn wave1_task_created_block_vetoes_todo_write() {
    let llm = MockLlm::new(vec![
        resp_tool("TodoWrite", &todo_args(&[("write tests", "pending")]), "t1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("TodoWrite", true, "RAN")]);
    let (mut agent, _s, hooks) = build_agent(llm, reg, allow(), base_cfg());
    hooks.push(
        HookEvent::TaskCreated,
        HookOutcome {
            blocked: true,
            feedback: Some("tasks need a ticket".into()),
            ..Default::default()
        },
    );
    agent.run_turn("go").await;
    let tool_msg = agent.messages().iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool_msg.text_ref(), Some("tasks need a ticket"));
    assert!(
        !agent
            .messages()
            .iter()
            .any(|m| m.role == "tool" && m.text_ref() == Some("RAN")),
        "a vetoed TodoWrite must not run"
    );
    let calls = hooks.calls.lock().unwrap();
    let (_, _, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::TaskCreated)
        .expect("TaskCreated should fire");
    assert_eq!(payload["task_subject"], "write tests");
    assert_eq!(payload["task_id"], "todo-0");
}

#[tokio::test]
async fn wave1_task_completed_fires_for_newly_completed_todo() {
    let llm = MockLlm::new(vec![
        resp_tool(
            "TodoWrite",
            &todo_args(&[("write tests", "completed")]),
            "t1",
        ),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("TodoWrite", true, "RAN")]);
    let (mut agent, _s, hooks) = build_agent(llm, reg, allow(), base_cfg());
    agent.ctx.todos.lock().unwrap().push(TodoItem {
        content: "write tests".into(),
        status: "pending".into(),
        active_form: None,
    });
    agent.run_turn("go").await;
    let calls = hooks.calls.lock().unwrap();
    let (_, _, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::TaskCompleted)
        .expect("TaskCompleted should fire");
    assert_eq!(payload["task_subject"], "write tests");
    assert!(
        !calls.iter().any(|(ev, _, _)| *ev == HookEvent::TaskCreated),
        "an existing item completing is not a creation: {:?}",
        calls
    );
}

#[tokio::test]
async fn wave1_post_tool_batch_context_injected_before_next_call() {
    let llm = MockLlm::new(vec![resp_tool("Echo", "{}", "e1"), resp("done")]);
    let reg = registry_with(vec![fake("Echo", true, "echoed")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), reg, allow(), base_cfg());
    hooks.push(
        HookEvent::PostToolBatch,
        HookOutcome {
            feedback: Some("BATCH-NOTE: check the echo".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "done");
    let calls = hooks.calls.lock().unwrap();
    let (_, _, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::PostToolBatch)
        .expect("PostToolBatch should fire");
    assert_eq!(payload["tool_calls"][0]["tool_name"], "Echo");
    assert_eq!(payload["tool_calls"][0]["tool_response"], "echoed");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == "user" && m.text().contains("BATCH-NOTE")),
        "batch context must reach the next model call"
    );
}

#[tokio::test]
async fn wave1_post_tool_batch_block_stops_run() {
    let llm = MockLlm::new(vec![resp_tool("Echo", "{}", "e1"), resp("never seen")]);
    let reg = registry_with(vec![fake("Echo", true, "echoed")]);
    let (mut agent, _s, hooks) = build_agent(llm.clone(), reg, allow(), base_cfg());
    hooks.push(
        HookEvent::PostToolBatch,
        HookOutcome {
            blocked: true,
            feedback: Some("batch rejected".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "batch rejected");
    assert_eq!(llm.requests().len(), 1, "no model call after the block");
}

#[tokio::test]
async fn wave1_message_display_replacement_changes_display_only() {
    let llm = MockLlm::new(vec![resp("hello secret")]);
    let (mut agent, sink, hooks) = build_agent(llm, ToolRegistry::new(), allow(), base_cfg());
    hooks.push(
        HookEvent::MessageDisplay,
        HookOutcome {
            display_replacement: Some("hello [redacted]".into()),
            ..Default::default()
        },
    );
    let r = agent.run_turn("go").await;
    assert_eq!(r.result, "hello secret");
    let events = sink.events.lock().unwrap();
    let shown = events.iter().find_map(|e| match e {
        AgentEvent::Assistant { message, .. } => Some(message.text()),
        _ => None,
    });
    assert_eq!(shown.as_deref(), Some("hello [redacted]"));
    let stored = agent
        .messages()
        .iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(
        stored.text(),
        "hello secret",
        "model context keeps the original"
    );
}

#[tokio::test]
async fn wave1_stop_failure_hook_runs_on_api_error() {
    let llm = MockLlm::with_errors(vec![Err(anyhow::anyhow!(
        "API Error: 429 rate limit reached"
    ))]);
    let (mut agent, _s, hooks) = build_agent(llm, ToolRegistry::new(), allow(), base_cfg());
    let r = agent.run_turn("go").await;
    assert!(r.is_error);
    let calls = hooks.calls.lock().unwrap();
    let (_, target, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::StopFailure)
        .expect("StopFailure should fire");
    assert_eq!(target, "rate_limit");
    assert_eq!(payload["error"], "rate_limit");
    assert!(payload["error_details"].as_str().unwrap().contains("429"));
}

struct CdTool;
#[async_trait::async_trait]
impl Tool for CdTool {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "fake bash that cds"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn call(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        let sub = ctx.cwd().join("deeper");
        std::fs::create_dir_all(&sub).unwrap();
        *ctx.cwd.lock().unwrap() = sub;
        ToolOutput::ok("moved")
    }
}

#[tokio::test]
async fn wave1_cwd_changed_fires_when_tool_moves_cwd() {
    let llm = MockLlm::new(vec![
        resp_tool("Bash", r#"{"command":"cd deeper"}"#, "b1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![Arc::new(CdTool)]);
    let (mut agent, _s, hooks) = build_agent(llm, reg, allow(), base_cfg());
    let before = agent.ctx.cwd();
    hooks.push(
        HookEvent::CwdChanged,
        HookOutcome {
            watch_paths: Some(vec!["/tmp/watch.env".to_string()]),
            ..Default::default()
        },
    );
    agent.run_turn("go").await;
    let calls = hooks.calls.lock().unwrap();
    let (_, _, payload) = calls
        .iter()
        .find(|(ev, _, _)| *ev == HookEvent::CwdChanged)
        .expect("CwdChanged should fire");
    assert_eq!(payload["old_cwd"], before.display().to_string());
    assert!(payload["new_cwd"].as_str().unwrap().ends_with("deeper"));
    assert_eq!(
        agent.dynamic_watch_paths,
        vec![std::path::PathBuf::from("/tmp/watch.env")]
    );
}

/// A test async prompter that always answers Yes and records that it
/// was asked (M4): stands in for the ACP round-trip to the client.
struct YesAsyncPrompter {
    asked: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl AsyncPrompter for YesAsyncPrompter {
    async fn ask_permission(
        &self,
        _tool_name: &str,
        _input: &Value,
        _tool_call_id: &str,
        _read_only: bool,
    ) -> PromptAnswer {
        self.asked.store(true, Ordering::SeqCst);
        PromptAnswer::Yes
    }
}

#[tokio::test]
async fn async_prompter_is_consulted_for_asked_tool_call() {
    // M4: the agent asks its installed async prompter for a tool call
    // the checker marks Ask, and runs the tool when it answers Yes.
    // (Without set_async_prompter the Ask denies in non-interactive
    // mode, the tool never runs, and the turn has no "written" result.)
    let llm = MockLlm::new(vec![resp_tool("Write", "{}", "c1"), resp("done")]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let (mut agent, _sink, _h) = build_agent(
        llm.clone(),
        reg,
        FakePerms::new(PermissionDecision::Ask),
        base_cfg(),
    );
    let asked = Arc::new(AtomicBool::new(false));
    agent.set_async_prompter(Arc::new(YesAsyncPrompter {
        asked: Arc::clone(&asked),
    }));
    let r = agent.run_turn("go").await;
    assert_eq!(r.subtype, "success");
    assert!(
        asked.load(Ordering::SeqCst),
        "the async prompter must have been asked"
    );
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.text_ref() == Some("written")),
        "the tool must have run, got {:#?}",
        requests[1].messages
    );
}

// ---------------------------------------------------------------------
// Flightdeck Phase 2: gate events (PermissionClassified / Resolved)
// ---------------------------------------------------------------------

struct FixedPrompter(PromptAnswer);

impl Prompter for FixedPrompter {
    fn ask_permission(&self, _tool_name: &str, _input: &Value) -> PromptAnswer {
        self.0.clone()
    }
}

fn gate_events(sink: &CollectSink) -> Vec<String> {
    sink.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PermissionClassified {
                bucket,
                reason_code,
                tool_use_id,
                ..
            } => Some(format!("classified:{tool_use_id}:{bucket:?}:{reason_code}")),
            AgentEvent::PermissionResolved { outcome, .. } => Some(format!("resolved:{outcome:?}")),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn gate_events_for_approved_ask() {
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"file_path": "a.txt"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let perms = FakePerms::new(PermissionDecision::Ask);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, perms, base_cfg());
    agent.prompter = Some(Arc::new(FixedPrompter(PromptAnswer::Yes)));
    agent.run_turn("go").await;
    assert_eq!(
        gate_events(&sink),
        vec![
            "classified:c1:Classifier:ask".to_string(),
            "resolved:ApprovedOnce".to_string(),
        ]
    );
}

#[tokio::test]
async fn gate_events_for_denied_by_user_and_always() {
    // No from the user: classified Classifier, resolved DeniedByUser,
    // and the tool never runs (no ToolResult content "written").
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"file_path": "a.txt"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let perms = FakePerms::new(PermissionDecision::Ask);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, perms.clone(), base_cfg());
    agent.prompter = Some(Arc::new(FixedPrompter(PromptAnswer::No)));
    agent.run_turn("go").await;
    assert_eq!(
        gate_events(&sink),
        vec![
            "classified:c1:Classifier:ask".to_string(),
            "resolved:DeniedByUser".to_string(),
        ]
    );

    // Always: the rule is recorded on the checker.
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"file_path": "a.txt"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, perms.clone(), base_cfg());
    agent.prompter = Some(Arc::new(FixedPrompter(PromptAnswer::Always(
        "Write(a.txt)".to_string(),
    ))));
    agent.run_turn("go").await;
    assert_eq!(
        gate_events(&sink),
        vec![
            "classified:c1:Classifier:ask".to_string(),
            "resolved:ApprovedAlways(\"Write(a.txt)\")".to_string(),
        ]
    );
    assert_eq!(perms.always.lock().unwrap().clone(), vec!["Write(a.txt)"]);
}

#[tokio::test]
async fn gate_events_for_hard_deny_and_headless_ask() {
    // Checker deny: one Classified(Denied), no Resolved.
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"file_path": "a.txt"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let perms = FakePerms::new(PermissionDecision::Deny("no".into()));
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, perms, base_cfg());
    agent.run_turn("go").await;
    assert_eq!(
        gate_events(&sink),
        vec!["classified:c1:Denied:deny".to_string()]
    );

    // Ask with no prompter at all: Resolved(DeniedNoPrompter).
    let llm = MockLlm::new(vec![
        resp_tool("Write", r#"{"file_path": "a.txt"}"#, "c1"),
        resp("done"),
    ]);
    let reg = registry_with(vec![fake("Write", false, "written")]);
    let perms = FakePerms::new(PermissionDecision::Ask);
    let (mut agent, sink, _h) = build_agent(llm.clone(), reg, perms, base_cfg());
    agent.run_turn("go").await;
    assert_eq!(
        gate_events(&sink),
        vec![
            "classified:c1:Classifier:ask".to_string(),
            "resolved:DeniedNoPrompter".to_string(),
        ]
    );
}

/// Fake `Task` tool that spawns through a real `SubAgentExecutor`
/// (issue #349): the parent can only see child spend if the runner
/// reports it and the parent drains it.
struct SpawnTaskTool {
    runner: Arc<dyn SubAgentRunner>,
}

#[async_trait::async_trait]
impl Tool for SpawnTaskTool {
    fn name(&self) -> &str {
        "Task"
    }

    fn subagent_runner(&self) -> Option<&dyn SubAgentRunner> {
        Some(&*self.runner)
    }

    fn description(&self) -> &str {
        "test-only Task tool"
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let def = AgentDefinition {
            name: "general-purpose".into(),
            description: "test child".into(),
            ..Default::default()
        };
        match self.runner.run_subagent(&def, &prompt, "test-1", 0).await {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

fn spawn_runner(llm: Arc<MockLlm>, model: &str) -> Arc<dyn SubAgentRunner> {
    let mut cfg = base_cfg();
    cfg.model = model.into();
    Arc::new(SubAgentExecutor::new(
        llm,
        ToolRegistry::new(),
        allow(),
        FakeHooks::new(),
        Arc::new(CollectSink::default()),
        temp_ctx(),
        cfg,
    ))
}

#[tokio::test]
async fn c349_parent_usage_includes_subagent_spend() {
    // Shared queue, in call order: parent tool call, child call,
    // parent final call. gpt-4o ($2.50/$10 per 1M) so every token
    // has a price on both sides.
    let llm = MockLlm::new(vec![
        resp_tool("Task", r#"{"prompt":"do it"}"#, "t1"),
        resp("sub result"),
        resp("done"),
    ]);
    let reg = registry_with(vec![Arc::new(SpawnTaskTool {
        runner: spawn_runner(llm.clone(), "gpt-4o"),
    })]);
    let mut parent_cfg = base_cfg();
    parent_cfg.model = "gpt-4o".into();
    let (mut agent, _sink, _hooks) = build_agent(llm.clone(), reg, allow(), parent_cfg);
    let result = agent.run_turn("go").await;
    assert_eq!(result.subtype, "success");
    assert_eq!(result.result, "done");
    // Parent own: 20 in / 8 out (tool call) + 10 in / 5 out (final).
    // Child: 10 in / 5 out. Without the fix the total is 30 / 13
    // and the child's $0.000075 never appears.
    assert_eq!(result.usage.input_tokens, 40);
    assert_eq!(result.usage.output_tokens, 18);
    assert_eq!(agent.subagent_usage.input_tokens, 10);
    assert_eq!(agent.subagent_usage.output_tokens, 5);
    // Own $0.000205 + child $0.000075 = $0.00028.
    assert!(
        (result.total_cost_usd - 0.00028).abs() < 1e-9,
        "total_cost_usd: {}",
        result.total_cost_usd
    );
    assert!(
        (agent.subagent_cost_usd - 0.000075).abs() < 1e-9,
        "subagent_cost_usd: {}",
        agent.subagent_cost_usd
    );
    assert_eq!(llm.requests().len(), 3, "parent, child, parent");
}

#[tokio::test]
async fn c349_spawn_refused_when_parent_already_over_budget() {
    // The spawn gate is the only enforcement that can fire here, so
    // the parent model must be unpriced ("mock-model": the loop-top
    // budget check is skipped when the estimate is `None`, while the
    // gate deliberately has no free-model skip). A priced parent can
    // never reach the gate with `MockLlm` fixed usage: the loop-top
    // check blocks the first call whenever `spent + est_next` (4096
    // output tokens) exceeds the cap, and one call can never cost more
    // than that estimate - the existing
    // `budget_cap_blocks_before_first_call_when_tiny` test pins the
    // loop-top behaviour, so a cap of $0.0001 on gpt-4o would make
    // zero model calls and never exercise the gate.
    let llm = MockLlm::new(vec![
        resp_tool("Task", r#"{"prompt":"do it"}"#, "t1"),
        resp("sub result"),
        resp("done"),
        resp_tool("Task", r#"{"prompt":"again"}"#, "t2"),
        resp("done"),
    ]);
    let reg = registry_with(vec![Arc::new(SpawnTaskTool {
        runner: spawn_runner(llm.clone(), "gpt-4o"),
    })]);
    // Turn 1, no budget: the child runs and its $0.000075 is drained
    // into the parent (parent model unpriced, so its own cost is $0).
    let (mut agent, _sink, _hooks) = build_agent(llm.clone(), reg, allow(), base_cfg());
    let r1 = agent.run_turn("go").await;
    assert_eq!(r1.subtype, "success");
    assert_eq!(agent.subagent_usage.input_tokens, 10);
    assert_eq!(agent.subagent_usage.output_tokens, 5);
    assert!(
        (agent.subagent_cost_usd - 0.000075).abs() < 1e-9,
        "subagent_cost_usd: {}",
        agent.subagent_cost_usd
    );
    // Now install a cap below what the child already spent. The next
    // spawn must be refused at `prepare_call`, before any child runs.
    agent.config.budget = Some(BudgetPolicy {
        max_budget_usd: 0.00005,
        warn_at: 0.8,
    });
    let r2 = agent.run_turn("again").await;
    assert_eq!(r2.subtype, "success");
    assert!(
        agent
            .messages()
            .iter()
            .any(|m| m.role == "tool" && m.text().contains("refusing to spawn")),
        "spawn refusal must reach the model as a tool result: {:?}",
        agent.messages()
    );
    // Parent, child, parent, parent, parent - the second child never ran.
    assert_eq!(llm.requests().len(), 5);
    assert_eq!(agent.subagent_usage.input_tokens, 10);
    assert_eq!(agent.subagent_usage.output_tokens, 5);
    assert!(
        (agent.subagent_cost_usd - 0.000075).abs() < 1e-9,
        "subagent_cost_usd: {}",
        agent.subagent_cost_usd
    );
}

#[tokio::test]
async fn c349_executor_reports_child_spend_once() {
    let llm = MockLlm::new(vec![resp("sub result")]);
    let (ex, _sink) = executor(llm.clone(), ToolRegistry::new(), FakeHooks::new());
    let def = AgentDefinition {
        name: "w".into(),
        description: "d".into(),
        ..Default::default()
    };
    let out = ex.run_subagent(&def, "do", "t", 0).await.unwrap();
    assert_eq!(out, "sub result");
    let spend = ex.take_child_spend();
    assert_eq!(spend.usage.input_tokens, 10);
    assert_eq!(spend.usage.output_tokens, 5);
    // mock-model is unpriced, so the child costs $0 by the estimator.
    assert!((spend.total_cost_usd - 0.0).abs() < 1e-9);
    assert!(
        ex.take_child_spend().is_empty(),
        "drain takes the balance exactly once"
    );
}
