use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use zen_core::lock_recovery::lock_or_recover;
use zen_core::{AgentEvent, ChatMessage, EventSink, LlmBackend};

use crate::agent_run;
use crate::cli::Cli;
use crate::settings;

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct RpcNotification {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
}

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

const PROTOCOL_VERSION: u64 = 1;

// JSON-RPC 2.0 error codes (ACP v1 uses the standard JSON-RPC codes;
// -32000..-32099 is the implementation-defined range).
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;
const ERR_INTERNAL: i64 = -32603;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionUpdateParams {
    session_id: String,
    update: SessionUpdate,
}

#[derive(Debug, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    ToolCall(ToolCallUpdate),
    ToolCallUpdate(ToolCallResult),
    #[serde(rename_all = "camelCase")]
    UsageUpdate {
        used: u64,
        size: u64,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallUpdate {
    tool_call_id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResult {
    tool_call_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ContentBlock>>,
}

#[derive(Clone)]
struct AcpSink {
    out: Arc<Mutex<std::io::Stdout>>,
    session_id: String,

    tools: Arc<Mutex<HashMap<String, String>>>,
}

impl AcpSink {
    fn notify(&self, update: SessionUpdate) {
        let params = SessionUpdateParams {
            session_id: self.session_id.clone(),
            update,
        };
        let notif = RpcNotification {
            jsonrpc: "2.0",
            method: "session/update",
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        };
        if let Ok(line) = serde_json::to_string(&notif) {
            let mut out = lock_or_recover(&self.out, "acp stdout");
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

impl EventSink for AcpSink {
    fn emit(&self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta { text } => {
                self.notify(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text },
                });
            }
            AgentEvent::Assistant { message, .. } => {
                emit_assistant(self, &message);
            }
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                let status = if is_error { "failed" } else { "completed" };
                self.notify(SessionUpdate::ToolCallUpdate(ToolCallResult {
                    tool_call_id: tool_use_id,
                    status,
                    content: Some(vec![ContentBlock::Text { text: content }]),
                }));
            }
            AgentEvent::SystemMessage { text, .. } => {
                self.notify(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text },
                });
            }

            AgentEvent::ApiRetry { .. }
            | AgentEvent::PermissionDenied(_)
            | AgentEvent::CompactBoundary { .. } => {}
            _ => {}
        }
    }
}

fn emit_assistant(sink: &AcpSink, message: &ChatMessage) {
    if let Some(text) = message.text_ref().filter(|t| !t.is_empty()) {
        sink.notify(SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text {
                text: text.to_string(),
            },
        });
    }
    if let Some(calls) = message.tool_calls.as_deref() {
        let mut tools = lock_or_recover(&sink.tools, "acp tools");
        for call in calls {
            tools.insert(call.id.clone(), call.function.name.clone());
            sink.notify(SessionUpdate::ToolCall(ToolCallUpdate {
                tool_call_id: call.id.clone(),
                title: call.function.name.clone(),
                kind: None,
            }));
        }
    }
}

struct AcpSession {
    agent: zen_agent::Agent,
    sink: Arc<AcpSink>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

pub async fn run_acp_server(cli: &Cli) -> Result<i32> {
    let home = dirs::home_dir();
    let default_cwd = std::env::current_dir()?;
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut sessions: HashMap<String, AcpSession> = HashMap::new();
    let mut initialized = false;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_response(
                    &out,
                    &RpcResponse::err(Value::Null, ERR_PARSE, format!("parse error: {e}")),
                )?;
                continue;
            }
        };
        if req.jsonrpc != "2.0" {
            if let Some(id) = req.id.clone() {
                write_response(
                    &out,
                    &RpcResponse::err(id, ERR_INVALID_REQUEST, "invalid JSON-RPC version"),
                )?;
            }
            continue;
        }
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        // Requests other than initialize (and the local shutdown/exit
        // lifecycle) before a successful initialize are rejected with
        // -32600; pre-init notifications get no response at all.
        match pre_init_decision(initialized, &req.method, is_notification, &id) {
            PreInit::Dispatch => {}
            PreInit::Reject(resp) => {
                write_response(&out, &resp)?;
                continue;
            }
            PreInit::Ignore => continue,
        }

        let response = match req.method.as_str() {
            "initialize" => Some(apply_initialize(&mut initialized, id, &req.params)),
            "authenticate" => Some(RpcResponse::ok(id, json!({}))),
            "session/new" => {
                match handle_session_new(cli, &default_cwd, home.as_deref(), &req.params, &out)
                    .await
                {
                    Ok((session_id, session)) => {
                        sessions.insert(session_id.clone(), session);
                        Some(RpcResponse::ok(id, json!({ "sessionId": session_id })))
                    }
                    Err(e) => Some(RpcResponse::err(
                        id,
                        ERR_INTERNAL,
                        format!("session/new failed: {e:#}"),
                    )),
                }
            }
            "session/prompt" => match handle_prompt(&mut sessions, &req.params).await {
                Ok(stop_reason) => Some(RpcResponse::ok(id, json!({ "stopReason": stop_reason }))),
                Err(e) => Some(RpcResponse::err(
                    id,
                    ERR_INTERNAL,
                    format!("session/prompt failed: {e:#}"),
                )),
            },
            "session/cancel" => {
                handle_cancel(&sessions, &req.params);
                if is_notification {
                    None
                } else {
                    Some(RpcResponse::ok(id, json!({})))
                }
            }
            "shutdown" | "exit" => {
                if !is_notification {
                    write_response(&out, &RpcResponse::ok(id, json!({})))?;
                }
                break;
            }
            _ => {
                if is_notification {
                    None
                } else {
                    Some(RpcResponse::err(
                        id,
                        ERR_METHOD_NOT_FOUND,
                        format!("method not found: {}", req.method),
                    ))
                }
            }
        };
        if let Some(resp) = response {
            write_response(&out, &resp)?;
        }
    }
    Ok(0)
}

fn write_response(out: &Arc<Mutex<std::io::Stdout>>, resp: &RpcResponse) -> Result<()> {
    let line = serde_json::to_string(resp)?;
    let mut out = lock_or_recover(out, "acp stdout");
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

/// Outcome of the pre-initialize gate in `run_acp_server`.
enum PreInit {
    /// Dispatch the request normally.
    Dispatch,
    /// Send this error response instead of dispatching.
    Reject(RpcResponse),
    /// Drop the message silently (a pre-init notification has no response).
    Ignore,
}

/// Gate for requests arriving before a successful `initialize` (ACP v1:
/// the client must initialize first; any other request is invalid,
/// -32600). `shutdown`/`exit` stay usable so a client can always quit.
fn pre_init_decision(
    initialized: bool,
    method: &str,
    is_notification: bool,
    id: &Value,
) -> PreInit {
    if initialized || matches!(method, "initialize" | "shutdown" | "exit") {
        return PreInit::Dispatch;
    }
    if is_notification {
        return PreInit::Ignore;
    }
    PreInit::Reject(RpcResponse::err(
        id.clone(),
        ERR_INVALID_REQUEST,
        "not initialized: send initialize first",
    ))
}

/// Applies an `initialize` request to the connection state. A repeat
/// `initialize` after a successful one is rejected with -32600 (exactly
/// one initialize per connection); `initialized` is set only when
/// negotiation succeeds, so a failed attempt can be retried.
fn apply_initialize(initialized: &mut bool, id: Value, params: &Value) -> RpcResponse {
    if *initialized {
        return RpcResponse::err(id, ERR_INVALID_REQUEST, "already initialized");
    }
    let resp = handle_initialize(id, params);
    if resp.error.is_none() {
        *initialized = true;
    }
    resp
}

fn handle_initialize(id: Value, params: &Value) -> RpcResponse {
    // protocolVersion is required (ACP v1 InitializeRequest) and must be
    // an integer major version; anything else is malformed params.
    if params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .is_none()
    {
        return RpcResponse::err(
            id,
            ERR_INVALID_PARAMS,
            "invalid params: protocolVersion must be an integer",
        );
    }

    // Version negotiation (ACP v1): answer the client's version when we
    // support it, otherwise our latest supported version; the client
    // disconnects when it cannot speak the answered version. We speak
    // v1 only, so the answer is always 1, even for a newer request (2)
    // or an unknown one (0). v1 negotiation never errors, so there is
    // no -32022 here; revisit if v2 is ever added (see Q2).
    let version = PROTOCOL_VERSION;

    RpcResponse::ok(
        id,
        json!({
            "protocolVersion": version,
            "agentCapabilities": {
                // Only what M1 actually implements is advertised; the
                // rest (session/load, MCP servers, session list/resume/
                // close/delete, auth) arrives with M5/M7. An omitted or
                // false capability means unsupported.
                // Modes are NOT advertised here: ACP v1 puts them in the
                // session/new response (M4). When M4 advertises them it
                // must list exactly default/plan/acceptEdits and never
                // bypassPermissions or dontAsk (Leader Q3, tin 15113).
                "loadSession": false,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
                "mcpCapabilities": { "http": false, "sse": false },
                "sessionCapabilities": {},
                "auth": {},
            },
            "agentInfo": {
                "name": "zencore",
                "title": "Zen",
                "version": env!("CARGO_PKG_VERSION"),
            },
            // No authentication is required.
            "authMethods": [],
        }),
    )
}

async fn handle_session_new(
    cli: &Cli,
    default_cwd: &Path,
    home: Option<&Path>,
    params: &Value,
    out: &Arc<Mutex<std::io::Stdout>>,
) -> Result<(String, AcpSession)> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cwd.to_path_buf());
    if !cwd.is_absolute() {
        anyhow::bail!("ACP requires absolute paths; got: {}", cwd.display());
    }

    let cfg = settings::resolve_config_with_home(cli, &cwd, home)?;
    let session_id = uuid::Uuid::new_v4().hyphenated().to_string();

    let sink = Arc::new(AcpSink {
        out: out.clone(),
        session_id: session_id.clone(),
        tools: Arc::new(Mutex::new(HashMap::new())),
    });

    let assembly = agent_run::assemble(cli, &cwd, home, &cfg, &session_id, None).await?;
    assembly.subagent_sink.set(sink.clone());

    let system_prompt = crate::build_system_prompt(cli, &cwd, home)?;
    let llm: Arc<dyn LlmBackend> = cfg.llm_client(sink.clone());
    let mut agent_config = agent_run::base_agent_config(&cfg, cli, &system_prompt);
    agent_config.diagnostics_provider = assembly.diagnostics_provider.clone();
    let ctx = agent_run::tool_context_public(&cwd, &cfg, cli, &session_id);
    let agent = zen_agent::Agent::new(
        llm,
        assembly.registry.clone(),
        assembly.permissions.clone(),
        None,
        assembly.hooks.clone(),
        sink.clone(),
        ctx,
        agent_config,
    );

    let _ = assembly;

    Ok((
        session_id,
        AcpSession {
            agent,
            sink: sink.clone(),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    ))
}

fn prompt_text(params: &Value) -> String {
    params
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    let t = b.get("type")?.as_str()?;
                    match t {
                        "text" => b.get("text")?.as_str().map(str::to_string),
                        "resource_link" | "resource" => b
                            .get("uri")
                            .and_then(Value::as_str)
                            .map(|u| format!("[resource: {u}]")),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

async fn handle_prompt(
    sessions: &mut HashMap<String, AcpSession>,
    params: &Value,
) -> Result<&'static str> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .context("missing sessionId")?;
    let session = sessions.get_mut(session_id).context("unknown sessionId")?;
    session
        .cancelled
        .store(false, std::sync::atomic::Ordering::SeqCst);

    let text = prompt_text(params);
    let result = session.agent.run_turn(&text).await;

    session.sink.notify(SessionUpdate::UsageUpdate {
        used: result.usage.input_tokens + result.usage.output_tokens,
        size: session.agent.config.context_window,
    });

    let stop_reason = if session.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        "cancelled"
    } else {
        match result.subtype.as_str() {
            "success" => "end_turn",
            "error_max_turns" => "max_turn_requests",
            _ => "end_turn",
        }
    };
    Ok(stop_reason)
}

fn handle_cancel(sessions: &HashMap<String, AcpSession>, params: &Value) {
    if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
        && let Some(s) = sessions.get(sid)
    {
        s.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_initialize_roundtrip() {
        let line =
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}"#;
        let req: RpcRequest = serde_json::from_str(line).unwrap();
        assert_eq!(req.method, "initialize");
        assert_eq!(req.id, Some(json!(0)));
        let resp = handle_initialize(req.id.unwrap(), &req.params);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert_eq!(v["result"]["agentInfo"]["name"], "zencore");
    }

    #[test]
    fn initialize_advertises_v1_capabilities() {
        let resp = handle_initialize(
            json!(1),
            &json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], 1);
        let caps = &v["result"]["agentCapabilities"];
        assert_eq!(caps["loadSession"], false);
        assert_eq!(caps["promptCapabilities"]["image"], false);
        assert_eq!(caps["promptCapabilities"]["audio"], false);
        assert_eq!(caps["promptCapabilities"]["embeddedContext"], false);
        assert_eq!(caps["mcpCapabilities"]["http"], false);
        assert_eq!(caps["mcpCapabilities"]["sse"], false);
        assert_eq!(caps["sessionCapabilities"], json!({}));
        assert_eq!(caps["auth"], json!({}));
        assert_eq!(v["result"]["authMethods"], json!([]));
    }

    #[test]
    fn initialize_newer_client_version_negotiates_down_to_v1() {
        // ACP v1: a client asking for 2 gets our latest supported version.
        let resp = handle_initialize(json!(2), &json!({"protocolVersion": 2}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn initialize_version_zero_still_answers_v1() {
        // ACP v1 has no -32022 in negotiation: even a request for 0 is
        // answered with our latest version; the client disconnects when
        // it cannot speak it.
        let resp = handle_initialize(json!(3), &json!({"protocolVersion": 0}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn initialize_missing_or_non_integer_version_is_invalid_params() {
        for params in [
            json!({}),
            json!({"protocolVersion": "1"}),
            json!({"protocolVersion": 1.5}),
            json!({"protocolVersion": null}),
        ] {
            let resp = handle_initialize(json!(4), &params);
            let v = serde_json::to_value(&resp).unwrap();
            assert_eq!(v["error"]["code"], -32602, "params: {params}");
            assert!(v.get("result").is_none());
        }
    }

    #[test]
    fn apply_initialize_sets_flag_only_on_success() {
        let mut initialized = false;
        let resp = apply_initialize(&mut initialized, json!(5), &json!({"protocolVersion": 1}));
        assert!(serde_json::to_value(&resp).unwrap().get("error").is_none());
        assert!(initialized);

        // A repeat initialize is rejected and leaves the flag set.
        let resp = apply_initialize(&mut initialized, json!(6), &json!({"protocolVersion": 1}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32600);
        assert!(initialized);
    }

    #[test]
    fn apply_initialize_failed_attempt_leaves_flag_clear() {
        let mut initialized = false;
        let resp = apply_initialize(&mut initialized, json!(7), &json!({}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(!initialized);

        // Retrying with a good request then succeeds.
        let resp = apply_initialize(&mut initialized, json!(8), &json!({"protocolVersion": 1}));
        assert!(serde_json::to_value(&resp).unwrap().get("error").is_none());
        assert!(initialized);
    }

    #[test]
    fn pre_init_decision_gates_requests_but_not_initialize() {
        // Uninitialized: session requests are rejected with -32600.
        let d = pre_init_decision(false, "session/new", false, &json!(9));
        let PreInit::Reject(resp) = d else {
            panic!("expected Reject, got Dispatch/Ignore");
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32600);
        assert_eq!(v["id"], 9);

        // Uninitialized: pre-init notifications are dropped silently.
        assert!(matches!(
            pre_init_decision(false, "session/cancel", true, &json!(10)),
            PreInit::Ignore
        ));

        // Uninitialized: initialize, shutdown and exit still dispatch.
        assert!(matches!(
            pre_init_decision(false, "initialize", false, &json!(11)),
            PreInit::Dispatch
        ));
        assert!(matches!(
            pre_init_decision(false, "shutdown", false, &json!(12)),
            PreInit::Dispatch
        ));
        assert!(matches!(
            pre_init_decision(false, "exit", true, &json!(13)),
            PreInit::Dispatch
        ));

        // Initialized: everything dispatches.
        assert!(matches!(
            pre_init_decision(true, "session/new", false, &json!(14)),
            PreInit::Dispatch
        ));
        assert!(matches!(
            pre_init_decision(true, "session/prompt", false, &json!(15)),
            PreInit::Dispatch
        ));
    }

    #[test]
    fn rpc_unknown_method_is_notification_safe() {
        let line = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"x"}}"#;
        let req: RpcRequest = serde_json::from_str(line).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn prompt_text_extracts_text_blocks() {
        let params = json!({
            "sessionId": "s1",
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" },
                { "type": "image", "data": "ignored-without-capability" },
            ]
        });
        assert_eq!(prompt_text(&params), "hello\nworld");
    }

    #[test]
    fn prompt_text_empty_without_content() {
        assert_eq!(prompt_text(&json!({})), "");
    }

    #[test]
    fn session_update_serialization_shape() {
        let update = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text { text: "hi".into() },
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn tool_call_update_serialization_shape() {
        let update = SessionUpdate::ToolCallUpdate(ToolCallResult {
            tool_call_id: "t1".into(),
            status: "completed",
            content: Some(vec![ContentBlock::Text { text: "ok".into() }]),
        });
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["toolCallId"], "t1");
        assert_eq!(v["status"], "completed");
    }

    #[test]
    fn emit_assistant_registers_tool_calls() {
        let sink = AcpSink {
            out: Arc::new(Mutex::new(std::io::stdout())),
            session_id: "probe".into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
        };
        let message = ChatMessage::assistant(
            None,
            Some(vec![zen_core::ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: zen_core::FunctionCall {
                    name: "Read".into(),
                    arguments: "{}".into(),
                },
            }]),
        );
        emit_assistant(&sink, &message);
        let tools = sink.tools.lock().unwrap();
        assert_eq!(tools.get("c1").map(String::as_str), Some("Read"));
    }

    /// Probe entry point for the emit_assistant stdout tests below.
    /// The parent test runs this test as a subprocess (current_exe with
    /// this test name); it reads the message JSON from ACP_PROBE_MSG and
    /// emits it through emit_assistant, so the parent can assert on the
    /// JSON-RPC lines it writes to stdout. Without the env var it is a
    /// no-op, so a plain `cargo test` run is unaffected.
    #[test]
    fn __acp_probe_emit_assistant() {
        let Ok(json) = std::env::var("ACP_PROBE_MSG") else {
            return;
        };
        let Ok(message) = serde_json::from_str::<ChatMessage>(&json) else {
            return;
        };
        let sink = AcpSink {
            out: Arc::new(Mutex::new(std::io::stdout())),
            session_id: "probe".into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
        };
        emit_assistant(&sink, &message);
    }

    fn probe_emit_assistant(message_json: &str) -> Vec<serde_json::Value> {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "acp::tests::__acp_probe_emit_assistant",
                "--exact",
                "--nocapture",
            ])
            .env("ACP_PROBE_MSG", message_json)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "probe run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .lines()
            .filter(|l| l.starts_with("{\"jsonrpc\""))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Probe entry point for the write_response stdout test below.
    /// Same current_exe + env-var pattern as __acp_probe_emit_assistant:
    /// reads a {"id": ..., "result": ...} response from ACP_PROBE_RESPONSE
    /// and writes it through write_response, so the parent can assert the
    /// JSON-RPC line actually reaches stdout. Without the env var it is a
    /// no-op. If write_response were replaced with Ok(()), the probe
    /// would write nothing and the parent test would fail.
    #[test]
    fn __acp_probe_write_response() {
        let Ok(json) = std::env::var("ACP_PROBE_RESPONSE") else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&json) else {
            return;
        };
        let resp = RpcResponse::ok(
            v.get("id").cloned().unwrap_or(Value::Null),
            v.get("result").cloned().unwrap_or(Value::Null),
        );
        write_response(&Arc::new(Mutex::new(std::io::stdout())), &resp).unwrap();
    }

    fn probe_write_response(response_json: &str) -> Vec<serde_json::Value> {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "acp::tests::__acp_probe_write_response",
                "--exact",
                "--nocapture",
            ])
            .env("ACP_PROBE_RESPONSE", response_json)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "probe run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .lines()
            .filter(|l| l.starts_with("{\"jsonrpc\""))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn write_response_writes_jsonrpc_line() {
        // Pins write_response: replacing its body with Ok(()) would
        // emit no line and fail the len assertion below.
        let lines = probe_write_response(r#"{"id":7,"result":{"ok":true}}"#);
        assert_eq!(lines.len(), 1, "lines: {lines:?}");
        assert_eq!(lines[0]["jsonrpc"], "2.0");
        assert_eq!(lines[0]["id"], 7);
        assert_eq!(lines[0]["result"]["ok"], true);
    }

    #[test]
    fn emit_assistant_sends_nonempty_text_chunk() {
        // Pins the `.filter(|t| !t.is_empty())`: negating the check
        // would send no chunk for real text.
        let lines = probe_emit_assistant(r#"{"role":"assistant","content":"hello"}"#);
        assert_eq!(lines.len(), 1, "lines: {lines:?}");
        assert_eq!(
            lines[0]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(lines[0]["params"]["update"]["content"]["text"], "hello");
    }

    #[test]
    fn emit_assistant_skips_empty_text() {
        // Pins the filter the other way: dropping it would send an
        // empty chunk for empty text.
        let lines = probe_emit_assistant(r#"{"role":"assistant","content":""}"#);
        assert!(lines.is_empty(), "lines: {lines:?}");
        let lines = probe_emit_assistant(r#"{"role":"assistant"}"#);
        assert!(lines.is_empty(), "lines: {lines:?}");
    }

    #[test]
    fn error_response_shape() {
        let resp = RpcResponse::err(json!(7), -32601, "method not found: foo");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], -32601);
        assert!(v.get("result").is_none());
    }
}

// Tests added for Việc A (floor coverage 88%): the AcpSink::emit arms
// (driven through a subprocess probe like __acp_probe_emit_assistant),
// write_response, the prompt_text resource arm, and the cheap error paths
// of handle_prompt / handle_cancel / handle_session_new. The stdio loop
// in run_acp_server is not drivable in-process; it is covered end to end
// by crates/zen-cli/tests/acp_server.rs instead.
#[cfg(test)]
mod viec_a_tests {
    use super::*;
    use clap::Parser;

    /// Probe entry point for the emit test below, mirroring
    /// __acp_probe_emit_assistant: the parent test runs this test as a
    /// subprocess and asserts on the JSON-RPC lines it writes to stdout.
    /// AgentEvent has no Deserialize impl, so the events are built here
    /// in code instead of coming from an env var.
    #[test]
    fn __acp_probe_emit() {
        let sink = AcpSink {
            out: Arc::new(Mutex::new(std::io::stdout())),
            session_id: "probe".into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
        };
        sink.emit(AgentEvent::TextDelta { text: "hi".into() });
        sink.emit(AgentEvent::ToolResult {
            tool_use_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
            parent_tool_use_id: None,
            parallel: false,
        });
        sink.emit(AgentEvent::ToolResult {
            tool_use_id: "t2".into(),
            content: "boom".into(),
            is_error: true,
            parent_tool_use_id: None,
            parallel: false,
        });
        sink.emit(AgentEvent::SystemMessage {
            event: "notice".into(),
            text: "sys".into(),
        });
        // An assistant message with no text and no tool calls emits
        // nothing, but still exercises the Assistant arm.
        sink.emit(AgentEvent::Assistant {
            message: ChatMessage::assistant(None, None),
            parent_tool_use_id: None,
        });
        // Retries and compaction boundaries are swallowed silently.
        sink.emit(AgentEvent::ApiRetry {
            attempt: 1,
            max_retries: 3,
            retry_delay_ms: 100,
            error_status: None,
            error: "x".into(),
        });
        sink.emit(AgentEvent::CompactBoundary {
            trigger: "t".into(),
            pre_tokens: 10,
        });
    }

    fn probe_emit() -> Vec<serde_json::Value> {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "acp::viec_a_tests::__acp_probe_emit",
                "--exact",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "probe run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .lines()
            .filter(|l| l.starts_with("{\"jsonrpc\""))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn emit_covers_text_tool_result_and_system_arms() {
        let lines = probe_emit();
        assert_eq!(lines.len(), 4, "lines: {lines:?}");
        assert_eq!(
            lines[0]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(lines[0]["params"]["update"]["content"]["text"], "hi");
        assert_eq!(
            lines[1]["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(lines[1]["params"]["update"]["toolCallId"], "t1");
        assert_eq!(lines[1]["params"]["update"]["status"], "completed");
        assert_eq!(lines[2]["params"]["update"]["status"], "failed");
        assert_eq!(lines[2]["params"]["update"]["toolCallId"], "t2");
        assert_eq!(
            lines[3]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(lines[3]["params"]["update"]["content"]["text"], "sys");
    }

    #[test]
    fn write_response_writes_json_line() {
        let out = Arc::new(Mutex::new(std::io::stdout()));
        write_response(&out, &RpcResponse::ok(json!(1), json!({"a": 1}))).unwrap();
        write_response(
            &out,
            &RpcResponse::err(json!(2), -32601, "method not found: x"),
        )
        .unwrap();
    }

    #[test]
    fn prompt_text_resource_blocks() {
        let params = json!({
            "content": [
                { "type": "resource_link", "uri": "file:///x.md" },
                { "type": "resource", "uri": "file:///y.md" },
                { "type": "text", "text": "hi" },
            ]
        });
        assert_eq!(
            prompt_text(&params),
            "[resource: file:///x.md]\n[resource: file:///y.md]\nhi"
        );
        // A block without a type (or a resource without a uri) is skipped.
        assert_eq!(prompt_text(&json!({"content": [{"text": "no type"}]})), "");
        assert_eq!(
            prompt_text(&json!({"content": [{"type": "resource_link"}]})),
            ""
        );
    }

    #[tokio::test]
    async fn handle_prompt_missing_and_unknown_session() {
        let mut sessions: HashMap<String, AcpSession> = HashMap::new();
        let err = handle_prompt(&mut sessions, &json!({}))
            .await
            .expect_err("missing sessionId must error");
        assert!(err.to_string().contains("missing sessionId"), "{err}");
        let err = handle_prompt(&mut sessions, &json!({"sessionId": "nope"}))
            .await
            .expect_err("unknown sessionId must error");
        assert!(err.to_string().contains("unknown sessionId"), "{err}");
    }

    #[test]
    fn handle_cancel_unknown_session_is_noop() {
        let sessions: HashMap<String, AcpSession> = HashMap::new();
        handle_cancel(&sessions, &json!({"sessionId": "nope"}));
        handle_cancel(&sessions, &json!({}));
    }

    #[tokio::test]
    async fn handle_session_new_rejects_relative_cwd() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let out = Arc::new(Mutex::new(std::io::stdout()));
        let cwd = std::env::temp_dir();
        let err = match handle_session_new(&cli, &cwd, None, &json!({"cwd": "relative/path"}), &out)
            .await
        {
            Ok(_) => panic!("relative cwd must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("ACP requires absolute paths"),
            "{err}"
        );
    }
}
