use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

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

/// Shared stdout writer for JSON-RPC lines. The dispatcher, prompt tasks
/// and the AcpSink all write through this one lock, so lines never
/// interleave. Boxed so tests can capture output with a Vec-backed writer
/// instead of the real stdout.
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone)]
struct AcpSink {
    out: SharedWriter,
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
    /// Guarded by a tokio Mutex: a prompt turn holds it for the whole
    /// turn, and the dispatcher never blocks on it (a second prompt for
    /// the same session is rejected via `prompt_running` first).
    agent: tokio::sync::Mutex<zen_agent::Agent>,
    sink: Arc<AcpSink>,
    cancelled: Arc<AtomicBool>,
    /// True while a prompt turn is running for this session (M2). The
    /// dispatcher sets it before spawning the turn and the turn clears it
    /// when done, so a second prompt for a busy session is rejected
    /// instead of racing the first turn for the agent.
    prompt_running: AtomicBool,
}

pub async fn run_acp_server(cli: &Cli) -> Result<i32> {
    let home = dirs::home_dir();
    let default_cwd = std::env::current_dir()?;
    let state = Arc::new(AcpServerState::new(Arc::new(Mutex::new(
        Box::new(std::io::stdout()) as Box<dyn Write + Send>,
    ))));

    // Stdin is read on a dedicated thread (M2): the reader only forwards
    // lines into a channel, so the dispatcher keeps receiving client
    // messages (session/cancel, responses to agent requests) while a
    // prompt turn runs on a spawned task. A blocking read inside the async
    // dispatcher would stall everything for the whole turn.
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let _reader = std::thread::Builder::new()
        .name("acp-stdin-reader".into())
        .spawn(move || read_stdin_lines(tx));

    run_dispatcher(cli.clone(), default_cwd, home, state, rx).await
}

/// Shared mutable state of the ACP server, owned by the dispatcher and
/// borrowed by prompt tasks (M2). Everything in here is Sync, so a prompt
/// can run on a spawned task while the dispatcher keeps reading stdin.
struct AcpServerState {
    /// Shared stdout writer (one JSON-RPC line per write, locked).
    out: SharedWriter,
    /// Sessions by id. A session stays in the map for the whole turn so
    /// `session/cancel` can find it while its prompt runs.
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    /// Requests the agent sent to the client and is still waiting on,
    /// keyed by the request id we generated.
    pending: Mutex<HashMap<String, oneshot::Sender<ClientResponse>>>,
    #[allow(dead_code)] // floor-guard: kept intentionally; first production caller is M4 (session/request_permission).
    /// Counter for agent-initiated request ids ("zen-1", "zen-2", ...).
    next_request_id: AtomicU64,
    /// Set once `initialize` succeeds (M1).
    initialized: AtomicBool,
}

impl AcpServerState {
    fn new(out: SharedWriter) -> Self {
        Self {
            out,
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            initialized: AtomicBool::new(false),
        }
    }

    /// Send a request to the client and wait for its response (M2). The
    /// request id is generated ("zen-1", ...) so it never collides with
    /// client-chosen ids. Returns the result value, or an error when the
    /// client answers with an error, the wait times out, or the client
    /// disconnects while waiting. Callers without a custom timeout pass
    /// 30 seconds; M4's `session/request_permission` uses that default
    /// and treats a timeout as a refusal (fail closed, Leader Q4).
    #[allow(dead_code)] // floor-guard: kept intentionally; first production caller is M4 (session/request_permission).
    async fn send_client_request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, SendRequestError> {
        let n = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        // Keep the id as a Value: the pending table is keyed by
        // id_key(&id), the same function the response router uses, so a
        // generated id can never miss its waiter over a key encoding.
        let id = Value::String(format!("zen-{n}"));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock_or_recover(&self.pending, "acp pending");
            pending.insert(id_key(&id), tx);
        }
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = write_json_line(&self.out, &request) {
            // The client is gone: drop the waiter so nobody waits forever.
            let mut pending = lock_or_recover(&self.pending, "acp pending");
            pending.remove(&id_key(&id));
            return Err(SendRequestError::WriteFailed(e.to_string()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(ClientResponse::Result(result))) => Ok(result),
            Ok(Ok(ClientResponse::Error { code, message })) => {
                Err(SendRequestError::ClientError { code, message })
            }
            Ok(Err(_)) => Err(SendRequestError::Disconnected),
            Err(_) => {
                let mut pending = lock_or_recover(&self.pending, "acp pending");
                pending.remove(&id_key(&id));
                Err(SendRequestError::Timeout)
            }
        }
    }
}

#[allow(dead_code)] // floor-guard: kept intentionally; first production caller is M4 (session/request_permission).
/// What the client answered to an agent-initiated request.
enum ClientResponse {
    Result(Value),
    Error { code: i64, message: String },
}

#[allow(dead_code)] // floor-guard: kept intentionally; first production caller is M4 (session/request_permission).
/// Why `send_client_request` did not return a result.
#[derive(Debug, PartialEq)]
enum SendRequestError {
    /// The client answered with a JSON-RPC error.
    ClientError { code: i64, message: String },
    /// No answer within the timeout.
    Timeout,
    /// The server is shutting down (the waiter was dropped).
    Disconnected,
    /// The request line could not be written (client gone).
    WriteFailed(String),
}

impl std::fmt::Display for SendRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientError { code, message } => {
                write!(f, "client request failed: {code} {message}")
            }
            Self::Timeout => write!(f, "client request timed out"),
            Self::Disconnected => write!(f, "client disconnected while waiting"),
            Self::WriteFailed(e) => write!(f, "failed to write client request: {e}"),
        }
    }
}

impl std::error::Error for SendRequestError {}

/// Route a client response (a message with `id` and `result`/`error` but
/// no `method`) to the waiter of the agent-initiated request with that
/// id. Returns true when a waiter was found. Unknown or late ids are
/// ignored: the waiter already timed out, and a late answer is not an
/// error.
fn route_client_response(state: &AcpServerState, msg: &Value) -> bool {
    let Some(id) = msg.get("id") else {
        return false;
    };
    let key = id_key(id);
    let response = if let Some(result) = msg.get("result") {
        ClientResponse::Result(result.clone())
    } else if let Some(error) = msg.get("error") {
        ClientResponse::Error {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(ERR_INTERNAL),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("client error")
                .to_string(),
        }
    } else {
        return false;
    };
    let sender = {
        let mut pending = lock_or_recover(&state.pending, "acp pending");
        pending.remove(&key)
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(response);
            true
        }
        None => {
            eprintln!("acp: response for unknown request id {key}, ignoring");
            false
        }
    }
}

/// Canonical map key for a JSON-RPC id (string or number). Generation and
/// lookup must use the same function, so `"zen-1"` and `1` never collide
/// with each other by accident.
fn id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

/// Stdin reader for the dispatcher (M2). Runs on its own thread and
/// forwards every line to the dispatcher; dropping the sender when stdin
/// closes tells the dispatcher to exit.
fn read_stdin_lines(tx: mpsc::UnboundedSender<String>) {
    read_lines(std::io::stdin().lock(), tx);
}

fn read_lines<R: BufRead>(reader: R, tx: mpsc::UnboundedSender<String>) {
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if tx.send(line).is_err() {
                    break; // Dispatcher is gone; nothing left to read for.
                }
            }
            Err(_) => break, // Read error: treat as EOF for the dispatcher.
        }
    }
}

/// What `dispatch_line` tells the dispatcher loop to do next.
#[derive(Debug, PartialEq)]
enum Dispatch {
    /// Keep reading the next line.
    Continue,
    /// The client asked to shut down; stop the loop.
    Stop,
}

/// The dispatcher loop (M2): reads lines from the channel fed by the
/// stdin reader thread and dispatches them one by one. It never blocks
/// on a prompt: `session/prompt` spawns a task and the loop moves on, so
/// `session/cancel` and responses to agent requests are processed while
/// a turn runs.
async fn run_dispatcher(
    cli: Cli,
    default_cwd: PathBuf,
    home: Option<PathBuf>,
    state: Arc<AcpServerState>,
    mut rx: mpsc::UnboundedReceiver<String>,
) -> Result<i32> {
    while let Some(line) = rx.recv().await {
        if dispatch_line(&cli, &default_cwd, home.as_deref(), &state, &line).await?
            == Dispatch::Stop
        {
            break;
        }
    }
    Ok(0)
}

/// Dispatch one incoming line. Returns `Dispatch::Stop` when the client
/// asked to shut down.
async fn dispatch_line(
    cli: &Cli,
    default_cwd: &Path,
    home: Option<&Path>,
    state: &Arc<AcpServerState>,
    line: &str,
) -> Result<Dispatch> {
    if line.trim().is_empty() {
        return Ok(Dispatch::Continue);
    }
    // Parse as a generic value first: a client response to an
    // agent-initiated request has no `method`, so it does not parse as an
    // RpcRequest at all. (Before M2, such a response came back as a -32700
    // parse error, which made any agent-to-client request unusable.)
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            write_response(
                &state.out,
                &RpcResponse::err(Value::Null, ERR_PARSE, format!("parse error: {e}")),
            )?;
            return Ok(Dispatch::Continue);
        }
    };
    if msg.get("method").is_none() {
        if msg.get("id").is_some() && (msg.get("result").is_some() || msg.get("error").is_some()) {
            route_client_response(state, &msg);
            return Ok(Dispatch::Continue);
        }
        // A message with neither method nor result/error is invalid.
        if let Some(id) = msg.get("id").cloned() {
            write_response(
                &state.out,
                &RpcResponse::err(
                    id,
                    ERR_INVALID_REQUEST,
                    "invalid request: expected method or result/error",
                ),
            )?;
        }
        return Ok(Dispatch::Continue);
    }
    let req: RpcRequest = match serde_json::from_value(msg) {
        Ok(r) => r,
        Err(e) => {
            write_response(
                &state.out,
                &RpcResponse::err(Value::Null, ERR_PARSE, format!("parse error: {e}")),
            )?;
            return Ok(Dispatch::Continue);
        }
    };
    if req.jsonrpc != "2.0" {
        if let Some(id) = req.id.clone() {
            write_response(
                &state.out,
                &RpcResponse::err(id, ERR_INVALID_REQUEST, "invalid JSON-RPC version"),
            )?;
        }
        return Ok(Dispatch::Continue);
    }
    let is_notification = req.id.is_none();
    let id = req.id.clone().unwrap_or(Value::Null);

    // Requests other than initialize (and the local shutdown/exit
    // lifecycle) before a successful initialize are rejected with
    // -32600; pre-init notifications get no response at all.
    match pre_init_decision(
        state.initialized.load(Ordering::SeqCst),
        &req.method,
        is_notification,
        &id,
    ) {
        PreInit::Dispatch => {}
        PreInit::Reject(resp) => {
            write_response(&state.out, &resp)?;
            return Ok(Dispatch::Continue);
        }
        PreInit::Ignore => return Ok(Dispatch::Continue),
    }

    let response = match req.method.as_str() {
        "initialize" => Some(apply_initialize(&state.initialized, id, &req.params)),
        "authenticate" => Some(RpcResponse::ok(id, json!({}))),
        "session/new" => {
            match handle_session_new(cli, default_cwd, home, &req.params, &state.out).await {
                Ok((session_id, session)) => {
                    let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
                    sessions.insert(session_id.clone(), Arc::new(session));
                    Some(RpcResponse::ok(id, json!({ "sessionId": session_id })))
                }
                Err(e) => Some(RpcResponse::err(
                    id,
                    ERR_INTERNAL,
                    format!("session/new failed: {e:#}"),
                )),
            }
        }
        "session/prompt" => {
            // A prompt runs on a spawned task (M2): the dispatcher
            // keeps reading stdin while the turn runs, so session/cancel
            // and responses to agent requests are processed mid-turn.
            // The session stays in the map for the whole turn, so a
            // cancel sent mid-turn can find it.
            match req.params.get("sessionId").and_then(Value::as_str) {
                None => Some(RpcResponse::err(
                    id,
                    ERR_INTERNAL,
                    "session/prompt failed: missing sessionId",
                )),
                Some(session_id) => {
                    let session = {
                        let sessions = lock_or_recover(&state.sessions, "acp sessions");
                        sessions.get(session_id).cloned()
                    };
                    match session {
                        None => Some(RpcResponse::err(
                            id,
                            ERR_INTERNAL,
                            "session/prompt failed: unknown sessionId",
                        )),
                        Some(session) => {
                            if session.prompt_running.swap(true, Ordering::SeqCst) {
                                Some(RpcResponse::err(
                                    id,
                                    ERR_INVALID_REQUEST,
                                    "session already has a prompt running",
                                ))
                            } else {
                                // Reset here, at dispatch time: the
                                // turn may not start polling until
                                // later, and a cancel sent in between
                                // must not be clobbered by the reset.
                                session.cancelled.store(false, Ordering::SeqCst);
                                let state2 = Arc::clone(state);
                                let params = req.params.clone();
                                tokio::spawn(async move {
                                    let _guard = PromptRunningGuard { session: &session };
                                    let resp = match run_prompt_turn(&session, &params).await {
                                        Ok(stop_reason) => RpcResponse::ok(
                                            id,
                                            json!({ "stopReason": stop_reason }),
                                        ),
                                        Err(e) => RpcResponse::err(
                                            id,
                                            ERR_INTERNAL,
                                            format!("session/prompt failed: {e:#}"),
                                        ),
                                    };
                                    let _ = write_response(&state2.out, &resp);
                                });
                                None
                            }
                        }
                    }
                }
            }
        }
        "session/cancel" => {
            {
                let sessions = lock_or_recover(&state.sessions, "acp sessions");
                handle_cancel(&sessions, &req.params);
            }
            if is_notification {
                None
            } else {
                Some(RpcResponse::ok(id, json!({})))
            }
        }
        "shutdown" | "exit" => {
            if !is_notification {
                write_response(&state.out, &RpcResponse::ok(id, json!({})))?;
            }
            return Ok(Dispatch::Stop);
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
        write_response(&state.out, &resp)?;
    }
    Ok(Dispatch::Continue)
}

fn write_response(out: &SharedWriter, resp: &RpcResponse) -> Result<()> {
    // Serialize the struct directly: round-tripping through Value would
    // sort the keys alphabetically and move `jsonrpc` after `id`.
    write_line(out, &serde_json::to_string(resp)?)
}

/// Write one JSON value as a single line on the shared writer (M2).
/// Every JSON-RPC line the server sends goes through here (responses,
/// notifications and agent-initiated requests), so a line is never split
/// by a concurrent writer.
#[allow(dead_code)] // floor-guard: kept intentionally; first production caller is M4 (session/request_permission).
fn write_json_line(out: &SharedWriter, value: &Value) -> Result<()> {
    write_line(out, &serde_json::to_string(value)?)
}

fn write_line(out: &SharedWriter, line: &str) -> Result<()> {
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
fn apply_initialize(initialized: &AtomicBool, id: Value, params: &Value) -> RpcResponse {
    if initialized.load(Ordering::SeqCst) {
        return RpcResponse::err(id, ERR_INVALID_REQUEST, "already initialized");
    }
    let resp = handle_initialize(id, params);
    if resp.error.is_none() {
        initialized.store(true, Ordering::SeqCst);
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
    out: &SharedWriter,
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
            agent: tokio::sync::Mutex::new(agent),
            sink: sink.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            prompt_running: AtomicBool::new(false),
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

/// Run one prompt turn for a session (M2). Called on a spawned task by
/// the dispatcher, so the turn runs while the dispatcher keeps reading
/// stdin. The caller guarantees the session exists and no other turn is
/// running for it (see `prompt_running`), and resets `cancelled` at
/// dispatch time.
async fn run_prompt_turn(session: &Arc<AcpSession>, params: &Value) -> Result<&'static str> {
    // The dispatcher resets `cancelled` at dispatch time (before the
    // spawn), not here: a cancel sent between the spawn and the first
    // poll of this task must survive.
    let text = prompt_text(params);
    let mut agent = session.agent.lock().await;
    let result = agent.run_turn(&text).await;
    let size = agent.config.context_window;
    drop(agent);

    session.sink.notify(SessionUpdate::UsageUpdate {
        used: result.usage.input_tokens + result.usage.output_tokens,
        size,
    });

    let stop_reason = if session.cancelled.load(Ordering::SeqCst) {
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

/// Clears a session's `prompt_running` flag when the prompt task ends,
/// including on panic, so a failed turn does not wedge the session.
struct PromptRunningGuard<'a> {
    session: &'a AcpSession,
}

impl Drop for PromptRunningGuard<'_> {
    fn drop(&mut self) {
        self.session.prompt_running.store(false, Ordering::SeqCst);
    }
}

fn handle_cancel(sessions: &HashMap<String, Arc<AcpSession>>, params: &Value) {
    if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
        && let Some(s) = sessions.get(sid)
    {
        s.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
        let initialized = AtomicBool::new(false);
        let resp = apply_initialize(&initialized, json!(5), &json!({"protocolVersion": 1}));
        assert!(serde_json::to_value(&resp).unwrap().get("error").is_none());
        assert!(initialized.load(Ordering::SeqCst));

        // A repeat initialize is rejected and leaves the flag set.
        let resp = apply_initialize(&initialized, json!(6), &json!({"protocolVersion": 1}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32600);
        assert!(initialized.load(Ordering::SeqCst));
    }

    #[test]
    fn apply_initialize_failed_attempt_leaves_flag_clear() {
        let initialized = AtomicBool::new(false);
        let resp = apply_initialize(&initialized, json!(7), &json!({}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert!(!initialized.load(Ordering::SeqCst));

        // Retrying with a good request then succeeds.
        let resp = apply_initialize(&initialized, json!(8), &json!({"protocolVersion": 1}));
        assert!(serde_json::to_value(&resp).unwrap().get("error").is_none());
        assert!(initialized.load(Ordering::SeqCst));
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
            out: Arc::new(Mutex::new(
                Box::new(std::io::stdout()) as Box<dyn Write + Send>
            )),
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
            out: Arc::new(Mutex::new(
                Box::new(std::io::stdout()) as Box<dyn Write + Send>
            )),
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
        write_response(
            &Arc::new(Mutex::new(
                Box::new(std::io::stdout()) as Box<dyn Write + Send>
            )),
            &resp,
        )
        .unwrap();
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

    // ---- M2 helpers and tests ----

    /// A Vec-backed writer for capturing dispatcher output in tests.
    struct VecWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for VecWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Test server state with a Vec writer; returns the state and the
    /// captured-output buffer.
    fn test_state() -> (Arc<AcpServerState>, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let out: SharedWriter = Arc::new(Mutex::new(
            Box::new(VecWriter { buf: buf.clone() }) as Box<dyn Write + Send>
        ));
        (Arc::new(AcpServerState::new(out)), buf)
    }

    /// Parse the captured output buffer into one JSON value per complete
    /// line. A line that does not parse yet (a write in flight) is
    /// skipped; the next poll sees it whole.
    fn read_output_lines(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
        let bytes = buf.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&bytes);
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Wait until the captured output satisfies `pred`, or panic with
    /// what was captured so far.
    async fn wait_for_output(
        buf: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
        pred: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let lines = read_output_lines(buf);
            if pred(&lines) {
                return lines;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for dispatcher output; got: {lines:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The response with the given numeric id, if one was written.
    fn find_response(lines: &[Value], id: u64) -> Option<&Value> {
        lines
            .iter()
            .find(|l| l.get("id").and_then(Value::as_u64) == Some(id))
    }

    fn init_line(id: u64) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": 1}})
            .to_string()
    }

    /// Run the dispatcher over `lines` (sent back-to-back, then the
    /// channel is closed) and return a handle to await for its exit.
    /// Prompt tasks outlive the loop; use `wait_for_output` afterwards
    /// for their responses.
    fn drive_dispatcher(
        state: Arc<AcpServerState>,
        lines: Vec<String>,
    ) -> tokio::task::JoinHandle<Result<i32>> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let cwd = std::env::temp_dir();
        let run = tokio::spawn(run_dispatcher(cli, cwd, None, state, rx));
        for line in lines {
            tx.send(line).unwrap();
        }
        drop(tx);
        run
    }

    /// A fake LLM backend that sleeps `delay_ms` before every reply (M2):
    /// the delay is what lets a test send a cancel mid-turn and prove the
    /// dispatcher processes it while the turn runs.
    struct DelayedLlm {
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl LlmBackend for DelayedLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            Ok(zen_core::ChatResponse {
                message: ChatMessage::assistant(Some("done".into()), None),
                usage: zen_core::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                finish_reason: Some("stop".into()),
            })
        }
    }

    /// A permission checker that allows everything (test double).
    struct AllowAll;

    impl zen_core::PermissionChecker for AllowAll {
        fn check(
            &self,
            _tool_name: &str,
            _input: &Value,
            _read_only: bool,
            _ctx: &zen_core::ToolContext,
        ) -> zen_core::PermissionDecision {
            zen_core::PermissionDecision::Allow
        }
    }

    /// Build a session backed by the delayed fake LLM and insert it into
    /// the test state, so dispatcher tests can run a real turn without a
    /// live LLM.
    fn fake_session(state: &Arc<AcpServerState>, session_id: &str, delay_ms: u64) {
        let sink = Arc::new(AcpSink {
            out: state.out.clone(),
            session_id: session_id.into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
        });
        let agent = zen_agent::Agent::new(
            Arc::new(DelayedLlm { delay_ms }),
            zen_core::ToolRegistry::new(),
            Arc::new(AllowAll),
            None,
            Arc::new(zen_core::NoHooks),
            sink.clone(),
            zen_core::ToolContext::new(std::env::temp_dir(), session_id),
            zen_agent::AgentConfig::default(),
        );
        let session = Arc::new(AcpSession {
            agent: tokio::sync::Mutex::new(agent),
            sink,
            cancelled: Arc::new(AtomicBool::new(false)),
            prompt_running: AtomicBool::new(false),
        });
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.insert(session_id.to_string(), session);
    }

    /// M2 core property: while a prompt turn runs, the dispatcher keeps
    /// processing stdin. The turn uses a delayed fake LLM (800 ms); a
    /// cancel and an unknown-method request sent right after the prompt
    /// are answered before the prompt finishes, and the turn ends
    /// cancelled.
    #[tokio::test]
    async fn dispatcher_processes_messages_during_prompt() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 800);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "hi"}]}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "bogus/method", "params": {}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        assert_eq!(lines[0]["result"]["protocolVersion"], 1, "{lines:?}");
        // The -32601 for the unknown method arrives before the prompt
        // response: the dispatcher did not block on the turn.
        let pos = |id: u64| {
            lines
                .iter()
                .position(|l| l.get("id").and_then(Value::as_u64) == Some(id))
                .unwrap()
        };
        assert!(pos(3) < pos(2), "{lines:?}");
        let unknown = find_response(&lines, 3).expect("bogus method must be answered");
        assert_eq!(unknown["error"]["code"], -32601, "{lines:?}");
        let prompt = find_response(&lines, 2).expect("prompt must be answered");
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{lines:?}");
    }

    /// A second prompt for a session with a turn running is rejected
    /// with -32600 instead of racing the first turn for the agent.
    #[tokio::test]
    async fn second_prompt_for_busy_session_is_rejected() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 800);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "one"}]}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "two"}]}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some() && find_response(ls, 3).is_some()
        })
        .await;
        let busy = find_response(&lines, 3).unwrap();
        assert_eq!(busy["error"]["code"], -32600, "{lines:?}");
        let first = find_response(&lines, 2).unwrap();
        assert_eq!(first["result"]["stopReason"], "end_turn", "{lines:?}");
    }

    /// The agent can send a request to the client and get its response
    /// back through the pending table (M2 infrastructure for M4).
    #[tokio::test]
    async fn pending_request_routes_response() {
        let (state, buf) = test_state();
        let asked = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                state
                    .send_client_request(
                        "session/request_permission",
                        json!({}),
                        Duration::from_secs(5),
                    )
                    .await
            }
        });
        // The fake client: read the request the server wrote, answer it.
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(|l| l.get("method").is_some())
        })
        .await;
        let request = lines
            .iter()
            .find(|l| l.get("method").is_some())
            .expect("request must be written");
        assert_eq!(request["method"], "session/request_permission");
        let id = request["id"].clone();
        assert!(route_client_response(
            &state,
            &json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": "ok"}}),
        ));
        assert_eq!(asked.await.unwrap().unwrap(), json!({"outcome": "ok"}));
    }

    #[tokio::test]
    async fn pending_request_reports_client_error() {
        let (state, buf) = test_state();
        let asked = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                state
                    .send_client_request(
                        "session/request_permission",
                        json!({}),
                        Duration::from_secs(5),
                    )
                    .await
            }
        });
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(|l| l.get("method").is_some())
        })
        .await;
        let id = lines.iter().find(|l| l.get("method").is_some()).unwrap()["id"].clone();
        assert!(route_client_response(
            &state,
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}}),
        ));
        assert_eq!(
            asked.await.unwrap().unwrap_err(),
            SendRequestError::ClientError {
                code: -32601,
                message: "no such method".into(),
            }
        );
    }

    #[tokio::test]
    async fn pending_request_times_out_and_late_answer_is_ignored() {
        let (state, _buf) = test_state();
        let err = state
            .send_client_request(
                "session/request_permission",
                json!({}),
                Duration::from_millis(10),
            )
            .await
            .expect_err("no client answer must time out");
        assert_eq!(err, SendRequestError::Timeout);
        // The stale waiter is gone: a late answer is ignored, not routed.
        assert!(!route_client_response(
            &state,
            &json!({"jsonrpc": "2.0", "id": "zen-1", "result": {}}),
        ));
    }

    #[test]
    fn stdin_reader_forwards_lines() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let data = b"one\ntwo\n\nthree\n";
        read_lines(std::io::Cursor::new(&data[..]), tx);
        assert_eq!(rx.try_recv().unwrap(), "one");
        assert_eq!(rx.try_recv().unwrap(), "two");
        assert_eq!(rx.try_recv().unwrap(), "");
        assert_eq!(rx.try_recv().unwrap(), "three");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatcher_rejects_prompt_for_missing_or_unknown_session() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "nope"}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = read_output_lines(&buf);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[1]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing sessionId"),
            "{lines:?}"
        );
        assert!(
            lines[2]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown sessionId"),
            "{lines:?}"
        );
    }
    /// Display for SendRequestError must name the failure: the messages
    /// surface in M4 permission errors, so an empty string would hide
    /// why the client request failed.
    #[test]
    fn send_request_error_display_strings() {
        assert_eq!(
            SendRequestError::ClientError {
                code: -32601,
                message: "no such method".into(),
            }
            .to_string(),
            "client request failed: -32601 no such method"
        );
        assert_eq!(
            SendRequestError::Timeout.to_string(),
            "client request timed out"
        );
        assert_eq!(
            SendRequestError::Disconnected.to_string(),
            "client disconnected while waiting"
        );
        assert_eq!(
            SendRequestError::WriteFailed("broken pipe".into()).to_string(),
            "failed to write client request: broken pipe"
        );
    }

    /// `id_key` must keep string and number JSON-RPC ids apart: the
    /// pending table keys both generations and lookups through it, so
    /// `"1"` and `1` colliding would route a client response to the
    /// wrong waiter.
    #[test]
    fn id_key_distinguishes_string_and_number_ids() {
        assert_eq!(id_key(&json!("zen-1")), "\"zen-1\"");
        assert_eq!(id_key(&json!(1)), "1");
        assert_ne!(id_key(&json!("1")), id_key(&json!(1)));
        assert_ne!(id_key(&json!("zen-1")), id_key(&json!("zen-2")));
    }

    /// `authenticate` is a no-op success in v1 (M4 will add real auth):
    /// the arm must answer, not fall through to method-not-found.
    #[tokio::test]
    async fn authenticate_returns_empty_ok() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "authenticate", "params": {}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = read_output_lines(&buf);
        let auth = find_response(&lines, 2).expect("authenticate must be answered");
        assert_eq!(auth["result"], json!({}), "{lines:?}");
    }

    /// A line with an id but neither method nor result/error is an
    /// invalid request: it must be answered with -32600, not silently
    /// swallowed as a stray client response.
    #[tokio::test]
    async fn bare_id_without_method_is_invalid_request() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![init_line(1), json!({"jsonrpc": "2.0", "id": 9}).to_string()],
        );
        run.await.unwrap().unwrap();
        let lines = read_output_lines(&buf);
        let resp = find_response(&lines, 9).expect("bare id must be answered with an error");
        assert_eq!(resp["error"]["code"], -32600, "{lines:?}");
    }

    /// A prompt turn emits a UsageUpdate whose `used` is the SUM of the
    /// turn's input and output tokens: the delayed fake LLM reports 10
    /// prompt + 5 completion tokens, so the client must see 15.
    #[tokio::test]
    async fn prompt_turn_reports_usage_update_with_summed_tokens() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "hi"}]}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{lines:?}");
        let usage = lines
            .iter()
            .find(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/update")
                    && l["params"]["update"]["sessionUpdate"].as_str() == Some("usage_update")
            })
            .expect("usage update must be emitted");
        assert_eq!(usage["params"]["update"]["used"], 15, "{lines:?}");
    }

    /// A second prompt on the same session AFTER the first turn finished
    /// must be accepted: PromptRunningGuard's Drop clears
    /// `prompt_running`, so the session is free again. (A prompt sent
    /// while a turn is still running is still rejected; see
    /// second_prompt_for_busy_session_is_rejected.)
    #[tokio::test]
    async fn prompt_after_completed_turn_is_accepted() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let run = tokio::spawn(run_dispatcher(
            cli,
            std::env::temp_dir(),
            None,
            Arc::clone(&state),
            rx,
        ));
        tx.send(init_line(1)).unwrap();
        tx.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "one"}]}})
                .to_string(),
        )
        .unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        assert_eq!(
            find_response(&lines, 2).unwrap()["result"]["stopReason"],
            "end_turn",
            "{lines:?}"
        );
        // Wait for the first turn's task to drop its guard and clear
        // prompt_running before sending the second prompt.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let running = {
                let sessions = lock_or_recover(&state.sessions, "acp sessions");
                sessions
                    .get("s1")
                    .map(|s| s.prompt_running.load(Ordering::SeqCst))
                    .unwrap_or(false)
            };
            if !running {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "prompt_running never cleared after the first turn"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1", "content": [{"type": "text", "text": "two"}]}})
                .to_string(),
        )
        .unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 3).is_some()
        })
        .await;
        let second = find_response(&lines, 3).unwrap();
        assert_eq!(second["result"]["stopReason"], "end_turn", "{lines:?}");
        drop(tx);
        run.await.unwrap().unwrap();
    }

    /// A client response carrying only `result` (no `error`) must be
    /// routed to its waiter through the dispatcher: the pending
    /// request tests call `route_client_response` directly, so only a
    /// test that answers through `dispatch_line` pins the routing
    /// condition down.
    #[tokio::test]
    async fn dispatcher_routes_result_only_client_response() {
        let (state, buf) = test_state();
        let asked = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                state
                    .send_client_request(
                        "session/request_permission",
                        json!({}),
                        Duration::from_secs(5),
                    )
                    .await
            }
        });
        // The fake client: read the request the server wrote, answer it.
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(|l| l.get("method").is_some())
        })
        .await;
        let request = lines
            .iter()
            .find(|l| l.get("method").is_some())
            .expect("request must be written");
        let id = request["id"].clone();
        // Answer through the dispatcher, not by calling
        // route_client_response directly.
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": "ok"}}).to_string()],
        );
        run.await.unwrap().unwrap();
        assert_eq!(asked.await.unwrap().unwrap(), json!({"outcome": "ok"}));
        // The answer must not have been treated as an invalid request.
        let lines = read_output_lines(&buf);
        assert!(
            !lines.iter().any(|l| l
                .get("error")
                .and_then(|e| e.get("code"))
                .is_some_and(|c| c == &json!(-32600))),
            "{lines:?}"
        );
    }
}

// Tests added for Việc A (floor coverage 88%): the AcpSink::emit arms
// (driven through a subprocess probe like __acp_probe_emit_assistant),
// write_response, the prompt_text resource arm, and the cheap error paths
// of handle_prompt / handle_cancel / handle_session_new. The stdio loop
// in run_acp_server is not drivable in-process; it is covered end to end
// by crates/zen-cli/tests/acp_server.rs instead. The dispatcher (M2) is
// drivable in-process through run_dispatcher with a channel; see the M2
// tests at the end of mod tests.
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
            out: Arc::new(Mutex::new(
                Box::new(std::io::stdout()) as Box<dyn Write + Send>
            )),
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
        let out = Arc::new(Mutex::new(
            Box::new(std::io::stdout()) as Box<dyn Write + Send>
        ));
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

    #[test]
    fn handle_cancel_unknown_session_is_noop() {
        let sessions: HashMap<String, Arc<AcpSession>> = HashMap::new();
        handle_cancel(&sessions, &json!({"sessionId": "nope"}));
        handle_cancel(&sessions, &json!({}));
    }

    #[tokio::test]
    async fn handle_session_new_rejects_relative_cwd() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let out = Arc::new(Mutex::new(
            Box::new(std::io::stdout()) as Box<dyn Write + Send>
        ));
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
