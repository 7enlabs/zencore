//! Three REAL subagents through the REAL `SubAgentExecutor`, with only
//! the LLM scripted: the flightdeck table must show all three live,
//! then done with checkmarks and elapsed times. No live model, no
//! network; the test runs in CI.
//!
//! The parent side of the flow is mirrored exactly: after each
//! `run_subagent` returns, the test feeds the parent's own `ToolResult`
//! for that delegate call into the sink, the same event the parent
//! agent emits when its `delegate` tool returns.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use zen_agent::{AgentConfig, SubAgentExecutor};
use zen_core::{
    AgentDefinition, AgentEvent, ChatMessage, ChatRequest, ChatResponse, EventSink, FunctionCall,
    LlmBackend, NoHooks, PermissionChecker, PermissionDecision, SubAgentRunner, Tool, ToolContext,
    ToolOutput, ToolRegistry, Usage,
};
use zen_flightdeck::{FlightdeckSink, FlightdeckState};

/// Scripted backend: pops responses in order, then answers "done" for
/// any further call so a turn always terminates.
struct ScriptedLlm {
    queue: Mutex<VecDeque<ChatResponse>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(responses.into()),
        })
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedLlm {
    async fn chat(&self, _req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let mut q = self.queue.lock().unwrap();
        Ok(q.pop_front().unwrap_or_else(|| resp("done")))
    }
}

fn resp(text: &str) -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(Some(text.into()), None),
        usage: Usage {
            prompt_tokens: 20,
            completion_tokens: 10,
            total_tokens: 30,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        finish_reason: Some("stop".into()),
    }
}

fn resp_tool(name: &str, args: &str, id: &str) -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(
            None,
            Some(vec![zen_core::ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                },
            }]),
        ),
        usage: Usage {
            prompt_tokens: 20,
            completion_tokens: 8,
            total_tokens: 28,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        finish_reason: Some("tool_calls".into()),
    }
}

/// A "Read" tool that takes `delay_ms` to answer, so the three
/// subagents finish at visibly different times.
struct SlowRead {
    delay_ms: u64,
}

#[async_trait::async_trait]
impl Tool for SlowRead {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file (test double with a delay)."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        ToolOutput::ok("file content for the test")
    }
}

struct AllowAll;

impl PermissionChecker for AllowAll {
    fn check(
        &self,
        _tool_name: &str,
        _input: &Value,
        _read_only: bool,
        _ctx: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

fn temp_ctx(tag: &str) -> (ToolContext, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("flightdeck-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test temp dir");
    (ToolContext::new(dir.clone(), "test-session"), dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_parallel_subagents_show_live_then_done() {
    let state = Arc::new(Mutex::new(FlightdeckState::new(
        "test-session",
        "mock-model",
        128_000,
        zen_core::PermissionMode::Default,
        "auto",
    )));
    let sink = FlightdeckSink::new(state.clone());
    let sink_dyn: Arc<dyn EventSink> = sink.clone();

    // One executor per subagent, each with its own scripted backend:
    // a Read tool call, then the final answer. Different tool delays
    // make them finish in order alpha, beta, gamma.
    let mut executors = Vec::new();
    let mut dirs = Vec::new();
    for (name, delay_ms) in [("alpha", 150), ("beta", 300), ("gamma", 450)] {
        let llm = ScriptedLlm::new(vec![
            resp_tool("Read", "{}", &format!("t-{name}")),
            resp(&format!("Xong phần {name}.")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SlowRead { delay_ms }));
        let (ctx, dir) = temp_ctx(name);
        dirs.push(dir);
        let cfg = AgentConfig {
            model: "mock-model".into(),
            system_prompt: "you are a test".into(),
            auto_compact_threshold: None,
            ..Default::default()
        };
        executors.push(SubAgentExecutor::new(
            llm,
            registry,
            Arc::new(AllowAll),
            Arc::new(NoHooks),
            sink_dyn.clone(),
            ctx,
            cfg,
        ));
    }

    let def = |name: &str| AgentDefinition {
        name: name.to_string(),
        description: format!("test subagent {name}"),
        tools: None,
        model: None,
        system_prompt: String::new(),
        disallowed_tools: Vec::new(),
        permission_mode: None,
        max_turns: None,
        skills: Vec::new(),
    };

    // The definitions must outlive the joined futures: `run_subagent`
    // borrows them across await points.
    let def_alpha = def("alpha");
    let def_beta = def("beta");
    let def_gamma = def("gamma");

    // Run all three in parallel, exactly like a parent delegating one
    // task split into three parts.
    let (r_alpha, r_beta, r_gamma) = tokio::join!(
        executors[0].run_subagent(&def_alpha, "do part alpha", "demo-alpha"),
        executors[1].run_subagent(&def_beta, "do part beta", "demo-beta"),
        executors[2].run_subagent(&def_gamma, "do part gamma", "demo-gamma"),
    );
    let results = [
        ("demo-alpha", r_alpha),
        ("demo-beta", r_beta),
        ("demo-gamma", r_gamma),
    ];
    for (id, r) in &results {
        let text = r
            .as_ref()
            .unwrap_or_else(|e| panic!("subagent {id} failed: {e:#}"));
        assert!(
            text.contains("Xong phần"),
            "subagent {id} returned unexpected text: {text}"
        );
        // The parent's own result for the delegate call: this is the
        // event that marks the row done.
        sink.emit(AgentEvent::ToolResult {
            tool_use_id: id.to_string(),
            content: text.clone(),
            is_error: false,
            parent_tool_use_id: None,
            parallel: true,
        });
    }

    let s = state.lock().unwrap();
    // Main row plus one row per subagent.
    assert_eq!(s.agents.len(), 4, "agents: {:?}", s.agent_rows());
    let rows = s.agent_rows();
    for (i, name) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let row = &rows[i + 1];
        assert_eq!(row[1], "xong ✓", "row for {name}: {row:?}");
        assert!(
            row[4].contains("Xong phần"),
            "row for {name} has no result: {row:?}"
        );
    }
    // Two assistant messages per subagent: the tool-call message and
    // the final answer.
    assert_eq!(s.requests, 6);
    assert!(s.used_tokens > 0);
    // No ledger in this test, so no cost is shown -- never a fake $0.
    assert!(
        s.header_line().contains("giá chưa rõ"),
        "{}",
        s.header_line()
    );
    // Every row ran for a measurable time (the tool delays).
    for row in s.agents.iter().skip(1) {
        assert!(
            row.elapsed() >= Duration::from_millis(100),
            "row {} elapsed too short: {:?}",
            row.name,
            row.elapsed()
        );
    }

    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}
