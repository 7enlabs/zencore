use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};

use zen_core::lock_recovery::lock_or_recover;
use zen_core::{AgentEvent, ChatMessage, EventSink, LlmBackend, PermissionMode, PromptAnswer};

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
/// Unknown session (ACP v1 implementation-defined range; Session Setup
/// doc, session/close section). M1-M3 used -32603 for an unknown session
/// in session/prompt; the new M4 methods use this code instead.
const ERR_UNKNOWN_SESSION: i64 = -32002;

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
    /// Echo of the client's prompt blocks (M3, ACP v1): one chunk per
    /// block, all sharing the turn's `messageId`, so the client can
    /// render the user message it sent. `messageId` is optional in the
    /// v1 schema, but always set here so chunks of one turn group
    /// together. There is NO `state_update` in ACP v1: `running` /
    /// `idle` / `requires_action` is a v2-only concept (v2 RFD), so a
    /// v1 server must not emit it.
    UserMessageChunk {
        content: ContentBlock,
        #[serde(rename = "messageId")]
        message_id: String,
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

    /// Tool calls announced but not yet finished (M3): a cancelled turn
    /// reports these as `cancelled` before answering the prompt.
    pending_tools: Arc<Mutex<HashSet<String>>>,
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

    /// Report every announced-but-unfinished tool call as `cancelled`
    /// (M3, ACP v1): the turn was aborted, so the client must not
    /// leave these calls hanging in progress. Called before the prompt
    /// response, after the turn's resources are closed.
    fn cancel_pending_tools(&self) {
        let ids: Vec<String> = {
            let mut pending = lock_or_recover(&self.pending_tools, "acp pending tools");
            pending.drain().collect()
        };
        for id in ids {
            self.notify(SessionUpdate::ToolCallUpdate(ToolCallResult {
                tool_call_id: id,
                status: "cancelled",
                content: None,
            }));
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
                {
                    let mut pending = lock_or_recover(&self.pending_tools, "acp pending tools");
                    pending.remove(&tool_use_id);
                }
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
        let mut pending = lock_or_recover(&sink.pending_tools, "acp pending tools");
        for call in calls {
            tools.insert(call.id.clone(), call.function.name.clone());
            pending.insert(call.id.clone());
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
    /// Wakes the prompt task when a cancel arrives mid-turn (M3): the
    /// task selects on this while `run_turn` runs, so the LLM call is
    /// aborted instead of running to completion.
    cancel_notify: Arc<tokio::sync::Notify>,
    /// JSON-RPC id of the prompt request currently running for this
    /// session, if any (M3): `$/cancel_request` matches on it.
    prompt_request_id: Mutex<Option<Value>>,
    /// The mode the session runs in (M4): `session/set_mode` writes it,
    /// `run_prompt_turn` applies it to the agent at turn start.
    mode: Mutex<PermissionMode>,
    /// The system prompt before any mode banner is applied (M4):
    /// `planmode::set_permission_mode` rebuilds from this base, so mode
    /// switches never stack banners.
    base_system_prompt: String,
    /// The model the session runs (gateway id, M4): `set_config_option`
    /// writes it, `run_prompt_turn` applies it at turn start.
    model: Mutex<String>,
    /// Working directory this session belongs to (M5). Sessions are
    /// listed and restored per workspace, and the journal is stored
    /// under the encoded cwd (PR #210).
    cwd: PathBuf,
    /// Last activity time: creation or the last completed prompt turn
    /// (M5). `session/list` sorts by it, newest first.
    last_activity: Mutex<SystemTime>,
    /// Writer for the session journal (M5): `None` when the session
    /// runs without a home dir (in-memory only) or the journal could
    /// not be created. A failed persist never fails a turn.
    transcript: Mutex<Option<crate::session::TranscriptWriter>>,
}

impl AcpSession {
    fn new(
        agent: zen_agent::Agent,
        sink: Arc<AcpSink>,
        mode: PermissionMode,
        base_system_prompt: String,
        model: String,
        cwd: PathBuf,
        transcript: Option<crate::session::TranscriptWriter>,
    ) -> Self {
        Self {
            agent: tokio::sync::Mutex::new(agent),
            sink,
            cancelled: Arc::new(AtomicBool::new(false)),
            prompt_running: AtomicBool::new(false),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            prompt_request_id: Mutex::new(None),
            mode: Mutex::new(mode),
            base_system_prompt,
            model: Mutex::new(model),
            cwd,
            last_activity: Mutex::new(SystemTime::now()),
            transcript: Mutex::new(transcript),
        }
    }

    /// Persist a completed turn to the session journal (M5): the
    /// messages the turn appended, as a transcript diff. A cancelled
    /// turn is never persisted (its history was rolled back), and a
    /// persist failure is logged, not reported: the turn already
    /// succeeded. Takes both snapshots because the agent lives behind
    /// a tokio Mutex, which this sync method cannot lock.
    fn persist_turn(&self, before: &[ChatMessage], after: &[ChatMessage]) {
        let mut guard = lock_or_recover(&self.transcript, "acp session transcript");
        let Some(writer) = guard.as_mut() else {
            return;
        };
        if let Err(e) = writer.persist_turn(before, after) {
            eprintln!("acp: failed to persist turn to session journal: {e:#}");
        }
        *lock_or_recover(&self.last_activity, "acp session last activity") = SystemTime::now();
    }

    /// The mode the session currently runs in (M4).
    fn current_mode(&self) -> PermissionMode {
        *lock_or_recover(&self.mode, "acp session mode")
    }

    /// The model the session currently runs (gateway id, M4).
    fn current_model(&self) -> String {
        lock_or_recover(&self.model, "acp session model").clone()
    }

    /// Record a mode change from `session/set_mode` (M4): the session
    /// state updates at once; the agent picks it up immediately when
    /// idle (try_lock succeeds) or at the next turn start when a turn
    /// is running, so a set_mode during a turn never blocks the
    /// dispatcher and never races the running turn for the agent.
    fn set_mode(&self, mode: PermissionMode) {
        *lock_or_recover(&self.mode, "acp session mode") = mode;
        if let Ok(mut agent) = self.agent.try_lock() {
            self.sync_session_state_to_agent(&mut agent);
        }
    }

    /// Record a model change from `session/set_config_option` (M4):
    /// same apply timing as `set_mode`.
    fn set_model(&self, model: String) {
        *lock_or_recover(&self.model, "acp session model") = model;
        if let Ok(mut agent) = self.agent.try_lock() {
            self.sync_session_state_to_agent(&mut agent);
        }
    }

    /// Apply the session's mode and model to the agent (M4): the
    /// permission mode (system prompt rebuilt from the base prompt),
    /// the permission checker's mode, and the model. Called at every
    /// turn start in `run_prompt_turn` and opportunistically from
    /// `set_mode`/`set_model` when the agent is idle.
    fn sync_session_state_to_agent(&self, agent: &mut zen_agent::Agent) {
        let mode = self.current_mode();
        crate::planmode::set_permission_mode(&mut agent.config, mode, &self.base_system_prompt);
        agent.permissions.set_mode(mode);
        agent.config.model = self.current_model();
    }

    /// The `modes` object for the `session/new` response (M4): exactly
    /// the four modes Q3 advertises.
    fn modes_state(&self) -> Value {
        json!({
            "currentModeId": self.current_mode().as_str(),
            "availableModes": advertised_mode_list(),
        })
    }

    /// The `configOptions` array for the `session/new` response and the
    /// `session/set_config_option` response (M4): the `model` select.
    fn config_options(&self) -> Value {
        json!([model_config_option(&self.current_model())])
    }

    /// Signal a running turn to stop (M3): set the flag the turn
    /// checks after `run_turn` returns AND wake the task waiting
    /// mid-turn. The flag is set first: a task that has not started
    /// waiting yet sees the flag, so a notify sent with no waiter is
    /// never lost.
    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel_notify.notify_waiters();
    }
}

pub async fn run_acp_server(cli: &Cli, home: Option<&Path>) -> Result<i32> {
    // Use the home already resolved by the CLI entry point: it honors
    // HOME/USERPROFILE, while dirs::home_dir() on Windows ignores HOME,
    // so the server silently used the real profile instead of the
    // configured home and never found managed sessions there.
    let home = home.map(Path::to_path_buf);
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

/// What the client answered to an agent-initiated request.
enum ClientResponse {
    Result(Value),
    Error { code: i64, message: String },
}

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
        "session/new" => match reject_system_prompt(&req.params) {
            Err((code, msg)) => Some(RpcResponse::err(id, code, msg)),
            Ok(()) => match handle_session_new(cli, default_cwd, home, &req.params, state).await {
                Ok((session_id, session)) => {
                    let session = Arc::new(session);
                    let modes = session.modes_state();
                    let config_options = session.config_options();
                    {
                        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
                        sessions.insert(session_id.clone(), Arc::clone(&session));
                    }
                    Some(RpcResponse::ok(
                        id,
                        json!({
                            "sessionId": session_id,
                            "modes": modes,
                            "configOptions": config_options,
                        }),
                    ))
                }
                Err(e) => Some(RpcResponse::err(
                    id,
                    ERR_INTERNAL,
                    format!("session/new failed: {e:#}"),
                )),
            },
        },
        "session/prompt" => {
            // A prompt runs on a spawned task (M2): the dispatcher
            // keeps reading stdin while the turn runs, so session/cancel
            // and responses to agent requests are processed mid-turn.
            // The session stays in the map for the whole turn, so a
            // cancel sent mid-turn can find it.
            //
            // Validate before the session lookup (M3): an invalid
            // prompt is rejected without starting a turn, so nothing
            // is inserted.
            match validate_prompt(&req.params) {
                Err(e) => Some(RpcResponse::err(id, ERR_INVALID_PARAMS, e)),
                Ok(prompt) => match req.params.get("sessionId").and_then(Value::as_str) {
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
                                    {
                                        let mut running_id = lock_or_recover(
                                            &session.prompt_request_id,
                                            "acp prompt request id",
                                        );
                                        *running_id = Some(id.clone());
                                    }
                                    let state2 = Arc::clone(state);
                                    tokio::spawn(async move {
                                        let _guard = PromptRunningGuard { session: &session };
                                        let resp = match run_prompt_turn(&session, &prompt).await {
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
                },
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
        "session/set_mode" => {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            match handle_set_mode(&sessions, &req.params) {
                Ok(()) => Some(RpcResponse::ok(id, json!({}))),
                Err((code, message)) => Some(RpcResponse::err(id, code, message)),
            }
        }
        "session/set_config_option" => {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            match handle_set_config_option(&sessions, &req.params) {
                Ok(config_options) => Some(RpcResponse::ok(
                    id,
                    json!({ "configOptions": config_options }),
                )),
                Err((code, message)) => Some(RpcResponse::err(id, code, message)),
            }
        }
        "session/list" => match handle_session_list(state, home, &req.params) {
            // Journal reads are blocking IO, but the journal is
            // small (one metadata line per entry) and the dispatcher
            // never holds the sessions lock across this call.
            Ok(value) => Some(RpcResponse::ok(id, value)),
            Err((code, message)) => Some(RpcResponse::err(id, code, message)),
        },
        "session/load" => match handle_session_load(cli, home, &req.params, state).await {
            Ok(value) => Some(RpcResponse::ok(id, value)),
            Err((code, message)) => Some(RpcResponse::err(id, code, message)),
        },
        "session/resume" => match handle_session_resume(cli, home, &req.params, state).await {
            Ok(value) => Some(RpcResponse::ok(id, value)),
            Err((code, message)) => Some(RpcResponse::err(id, code, message)),
        },
        "session/close" => match handle_session_close(state, &req.params) {
            Ok(()) => Some(RpcResponse::ok(id, json!({}))),
            Err((code, message)) => Some(RpcResponse::err(id, code, message)),
        },
        "session/delete" => match handle_session_delete(state, home, &req.params) {
            Ok(()) => Some(RpcResponse::ok(id, json!({}))),
            Err((code, message)) => Some(RpcResponse::err(id, code, message)),
        },
        "$/cancel_request" => {
            // A notification in practice: no response is ever sent for
            // it (M3). If a client sends it as a request, answer with
            // an empty result after handling it, like session/cancel.
            handle_dollar_cancel(state, &req.params);
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
                // Only what M1-M5 actually implement is advertised; the
                // rest (MCP servers, auth) arrives with M7. An omitted or
                // false capability means unsupported.
                // Modes are NOT advertised here: ACP v1 puts them in the
                // session/new response (M4), which lists exactly
                // default/plan/acceptEdits/askAll and never bypassPermissions
                // or dontAsk (Leader Q3, tin 15113).
                // M5 advertises the persistence capability set of ACP v1.
                "loadSession": true,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
                "mcpCapabilities": { "http": false, "sse": false },
                "sessionCapabilities": {
                    "list": {},
                    "resume": {},
                    "close": {},
                    "delete": {},
                },
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
    state: &Arc<AcpServerState>,
) -> Result<(String, AcpSession)> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cwd.to_path_buf());
    if !cwd.is_absolute() {
        anyhow::bail!("ACP requires absolute paths; got: {}", cwd.display());
    }

    let session_id = uuid::Uuid::new_v4().hyphenated().to_string();
    let session = build_acp_session(cli, &cwd, home, state, &session_id, &[]).await?;
    Ok((session_id, session))
}

/// Build a live ACP session for a given id, optionally seeded with a
/// restored history (M5). Shared by `session/new` (fresh id, empty
/// seed) and `session/load` / `session/resume` (the stored id, the
/// journal history). The mode and model start at the configured
/// defaults: neither is persisted in the journal format (v1), so a
/// restored session restarts with the defaults, not the values it had
/// when it was saved.
async fn build_acp_session(
    cli: &Cli,
    cwd: &Path,
    home: Option<&Path>,
    state: &Arc<AcpServerState>,
    session_id: &str,
    seed: &[ChatMessage],
) -> Result<AcpSession> {
    let cfg = settings::resolve_config_with_home(cli, cwd, home)?;

    let sink = Arc::new(AcpSink {
        out: state.out.clone(),
        session_id: session_id.to_string(),
        tools: Arc::new(Mutex::new(HashMap::new())),
        pending_tools: Arc::new(Mutex::new(HashSet::new())),
    });

    let assembly = agent_run::assemble(cli, cwd, home, &cfg, session_id, None).await?;
    assembly.subagent_sink.set(sink.clone());

    let system_prompt = crate::build_system_prompt(cli, cwd, home)?;
    let llm: Arc<dyn LlmBackend> = cfg.llm_client(sink.clone());
    let mut agent_config = agent_run::base_agent_config(&cfg, cli, &system_prompt);
    agent_config.diagnostics_provider = assembly.diagnostics_provider.clone();
    let ctx = agent_run::tool_context_public(cwd, &cfg, cli, session_id);
    // M4: the session only ever runs an advertised mode. A configured
    // bypassPermissions/dontAsk mode (CLI flag or settings) is clamped
    // to Default for ACP sessions: those modes must never be advertised
    // (Q3) and must never run behind a client that cannot see them.
    let initial_mode = match agent_config.permission_mode {
        PermissionMode::Default
        | PermissionMode::AcceptEdits
        | PermissionMode::Plan
        | PermissionMode::AskAll => agent_config.permission_mode,
        PermissionMode::BypassPermissions | PermissionMode::DontAsk => {
            eprintln!(
                "acp: clamping unadvertised mode {} to default for session {session_id}",
                agent_config.permission_mode.as_str()
            );
            PermissionMode::Default
        }
    };
    let mut agent = zen_agent::Agent::new(
        llm,
        assembly.registry.clone(),
        assembly.permissions.clone(),
        None,
        assembly.hooks.clone(),
        sink.clone(),
        ctx,
        agent_config,
    );
    // Apply the (possibly clamped) mode to the agent config, the system
    // prompt and the permission checker, then install the ACP prompter
    // so permission questions round-trip to the client (M4).
    crate::planmode::set_permission_mode(&mut agent.config, initial_mode, &system_prompt);
    agent.permissions.set_mode(initial_mode);
    agent.set_async_prompter(Arc::new(AcpPrompter::new(
        Arc::clone(state),
        session_id.to_string(),
    )));
    if !seed.is_empty() {
        agent.set_messages(seed.to_vec());
    }

    let _ = assembly;

    let model = agent.config.model.clone();
    // The journal writer lives with the session: every completed
    // turn is appended to the PR #210 journal for this cwd + id.
    let transcript = match home {
        Some(home) => match crate::session::ensure_session_path(home, cwd, session_id) {
            Ok(path) => Some(crate::session::TranscriptWriter::new(path, session_id, cwd)),
            Err(e) => {
                eprintln!("acp: session journal unavailable: {e:#}");
                None
            }
        },
        None => None,
    };
    Ok(AcpSession::new(
        agent,
        sink,
        initial_mode,
        system_prompt,
        model,
        cwd.to_path_buf(),
        transcript,
    ))
}

/// One validated prompt turn (M3): the text the agent runs on, plus
/// the per-block renderings echoed back as `user_message_chunk`.
#[derive(Debug)]
struct ValidatedPrompt {
    text: String,
    /// Rendered text of each prompt block, in order.
    echo_blocks: Vec<String>,
}

/// Validate a `session/prompt` params value (M3, ACP v1).
///
/// The baseline the agent advertises (`promptCapabilities`: image /
/// audio / embeddedContext all false) MUST support `text` and
/// `resource_link`. Any other content type is rejected with an
/// invalid-params error naming the type: the client MUST restrict
/// content to the advertised capabilities, and silently dropping a
/// block the user sent would lose their message. An empty prompt
/// (missing, empty, or no usable content) is rejected the same way:
/// the agent has nothing to run on.
fn validate_prompt(params: &Value) -> Result<ValidatedPrompt, String> {
    let Some(blocks) = params.get("prompt").and_then(Value::as_array) else {
        return Err("prompt must be a non-empty array of content blocks".into());
    };
    if blocks.is_empty() {
        return Err("prompt must be a non-empty array of content blocks".into());
    }
    let mut echo_blocks = Vec::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        let Some(t) = b.get("type").and_then(Value::as_str) else {
            return Err(format!("prompt block {i} has no content type"));
        };
        match t {
            "text" => {
                let Some(text) = b.get("text").and_then(Value::as_str) else {
                    return Err(format!("text block {i} has no text"));
                };
                echo_blocks.push(text.to_string());
            }
            "resource_link" => {
                let Some(uri) = b.get("uri").and_then(Value::as_str) else {
                    return Err(format!("resource block {i} has no uri"));
                };
                echo_blocks.push(format!("[resource: {uri}]"));
            }
            // Unsupported content type: rejected, never silently
            // dropped (see the doc comment above).
            other => return Err(format!("unsupported content type: {other}")),
        }
    }
    let text = echo_blocks.join("\n");
    if text.trim().is_empty() {
        return Err("prompt must not be empty".into());
    }
    Ok(ValidatedPrompt { text, echo_blocks })
}

/// Run one prompt turn for a session (M2). Called on a spawned task by
/// the dispatcher, so the turn runs while the dispatcher keeps reading
/// stdin. The caller guarantees the session exists and no other turn is
/// running for it (see `prompt_running`), and resets `cancelled` at
/// dispatch time.
async fn run_prompt_turn(
    session: &Arc<AcpSession>,
    prompt: &ValidatedPrompt,
) -> Result<&'static str> {
    // The dispatcher resets `cancelled` at dispatch time (before the
    // spawn), not here: a cancel sent between the spawn and the first
    // poll of this task must survive.
    //
    // A cancel sent between dispatch and the first poll of this task:
    // arm the wake-up first, then check the flag. The flag is set
    // before the notify in `request_cancel`, so a notify sent with no
    // waiter yet is never lost: the flag check catches it.
    let notified = session.cancel_notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if session.cancelled.load(Ordering::SeqCst) {
        return Ok("cancelled");
    }

    // Echo the user message first (M3, ACP v1): one
    // `user_message_chunk` per prompt block, all sharing one
    // `messageId` for the turn, so the client can render the message
    // it sent.
    let message_id = uuid::Uuid::new_v4().hyphenated().to_string();
    for block in &prompt.echo_blocks {
        session.sink.notify(SessionUpdate::UserMessageChunk {
            content: ContentBlock::Text {
                text: block.clone(),
            },
            message_id: message_id.clone(),
        });
    }

    let mut agent = session.agent.lock().await;
    // M4: apply the session's mode and model (a set_mode or
    // set_config_option that arrived during the previous turn, or while
    // this turn was starting) before the turn reads them.
    session.sync_session_state_to_agent(&mut agent);
    // Snapshots for the cancel path (M3): history, usage and live
    // descendants before the turn, so a cancel restores all three,
    // like the REPL interrupt (`run_turn_interruptible`).
    let before = agent.messages().to_vec();
    let usage_before = agent.usage.clone();
    let pre = agent_run::descendants_of(std::process::id());
    // The select drops the losing future: when the cancel wins, the
    // in-flight LLM request (and any in-flight tool call) is aborted
    // right there, not run to completion.
    let result = tokio::select! {
        r = agent.run_turn(&prompt.text) => Some(r),
        () = notified.as_mut() => None,
    };
    let size = agent.config.context_window;
    match result {
        Some(result) => {
            // M5: the turn completed, so its messages join the
            // session journal before the agent is dropped. The
            // journal mirrors the agent's actual history, including
            // a turn that finished while a cancel was racing it; a
            // turn aborted mid-flight (the None arm) restores
            // `before`, so it is never persisted.
            let after = agent.messages().to_vec();
            session.persist_turn(&before, &after);
            drop(agent);
            session.sink.notify(SessionUpdate::UsageUpdate {
                used: result.usage.input_tokens + result.usage.output_tokens,
                size,
            });
            if session.cancelled.load(Ordering::SeqCst) {
                Ok("cancelled")
            } else {
                Ok(match result.subtype.as_str() {
                    "success" => "end_turn",
                    "error_max_turns" => "max_turn_requests",
                    "error_max_tokens" => "max_tokens",
                    "error_refusal" => "refusal",
                    // V1 has no error stopReason: an execution error
                    // (e.g. the LLM call failed) still ends the turn,
                    // with the failure in the streamed content.
                    _ => "end_turn",
                })
            }
        }
        None => {
            // Cancelled mid-turn: the turn future is already dropped
            // (the select did it), so the LLM call and the tool call
            // are aborted. Close what the turn started - new child
            // processes - then restore history, and report the
            // in-flight tool calls cancelled, all before the prompt
            // response, as ACP v1 requires.
            agent_run::kill_new_descendants(&pre);
            agent.set_messages(before);
            let used = (agent.usage.input_tokens + agent.usage.output_tokens)
                - (usage_before.input_tokens + usage_before.output_tokens);
            drop(agent);
            session.sink.cancel_pending_tools();
            session
                .sink
                .notify(SessionUpdate::UsageUpdate { used, size });
            Ok("cancelled")
        }
    }
}

/// Clears a session's `prompt_running` flag when the prompt task ends,
/// including on panic, so a failed turn does not wedge the session.
struct PromptRunningGuard<'a> {
    session: &'a AcpSession,
}

impl Drop for PromptRunningGuard<'_> {
    fn drop(&mut self) {
        self.session.prompt_running.store(false, Ordering::SeqCst);
        let mut id = lock_or_recover(&self.session.prompt_request_id, "acp prompt request id");
        *id = None;
    }
}

// ---------------------------------------------------------------------------
// Session persistence (M5, ACP v1): session/list, session/load,
// session/resume, session/close, session/delete.
//
// The journal is the source of truth across processes:
// `~/.zen/projects/<encoded-cwd>/<session-id>.jsonl` (PR #210 format).
// A turn is persisted when it completes (`run_prompt_turn`); a
// cancelled turn restores history, so nothing new is written. Live
// sessions also stay in the in-memory map, so `session/list` merges
// both: a session with no completed turn yet has no journal, but it
// is still a known session.
//
// Two prep-doc claims turned out to be v2-only and do NOT apply to
// v1 (checked against the v1 schema, 17/11/2026): `session/resume`
// has no `replayFrom` parameter and there is no `replay_complete`
// update in v1 — resume never replays, load replays the whole
// history with `session/update` and then answers. `state_update`
// is v2-only too (M3).
// ---------------------------------------------------------------------------

/// Page size for `session/list` (ACP v1 leaves the size to the agent;
/// the cursor is the offset of the next page, as a decimal string).
const SESSION_LIST_PAGE_SIZE: usize = 50;

/// One row of a `session/list` result, before serialisation.
struct ListedSession {
    id: String,
    cwd: String,
    title: Option<String>,
    updated_at: Option<SystemTime>,
}

impl ListedSession {
    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("sessionId".into(), json!(self.id));
        obj.insert("cwd".into(), json!(self.cwd));
        if let Some(title) = &self.title {
            obj.insert("title".into(), json!(title));
        }
        if let Some(updated) = self.updated_at {
            let dt: DateTime<Utc> = updated.into();
            obj.insert("updatedAt".into(), json!(dt.to_rfc3339()));
        }
        Value::Object(obj)
    }
}

/// Read the working directory recorded in a session journal: the
/// `cwd` field of the first record that carries one. Every record
/// written by `TranscriptWriter` carries it; usage-only records do
/// not, so they are skipped.
fn journal_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            return Some(cwd.to_string());
        }
    }
    None
}

/// Collect the sessions `session/list` reports (M5): journal sessions
/// on disk plus live in-memory sessions, newest activity first (ties
/// break by session id, so pagination is stable). A `cwd` filter
/// restricts to that working directory; without a filter every known
/// session is listed. Archived journals are not listed: they are no
/// longer live sessions.
fn collect_listed_sessions(
    state: &AcpServerState,
    home: Option<&Path>,
    cwd_filter: Option<&Path>,
) -> Vec<ListedSession> {
    let mut by_id: HashMap<String, ListedSession> = HashMap::new();
    if let Some(home) = home {
        if let Some(filter) = cwd_filter {
            for (id, path) in crate::session::list_sessions_for_cwd(home, filter) {
                let info = crate::session_mgmt::session_info(&path, false);
                by_id.insert(
                    id.clone(),
                    ListedSession {
                        id,
                        cwd: filter.display().to_string(),
                        title: info.title,
                        updated_at: Some(info.modified),
                    },
                );
            }
        } else {
            for info in crate::session_mgmt::list_all_sessions(home, false) {
                let Some(cwd) = journal_cwd(&info.path) else {
                    // A journal without a recorded cwd cannot be
                    // reported (SessionInfo requires cwd). It is a
                    // corrupt record: log it and skip it instead of
                    // failing the whole list.
                    eprintln!(
                        "acp: skipping journal without cwd in session/list: {}",
                        info.path.display()
                    );
                    continue;
                };
                by_id.insert(
                    info.id.clone(),
                    ListedSession {
                        id: info.id,
                        cwd,
                        title: info.title,
                        updated_at: Some(info.modified),
                    },
                );
            }
        }
    }
    // Live sessions with no journal yet (no completed turn) are still
    // known sessions; a live session with a journal refreshes the
    // activity time when its last turn ended after the last persist.
    let live: Vec<(String, PathBuf, SystemTime)> = {
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions
            .iter()
            .map(|(id, s)| {
                (
                    id.clone(),
                    s.cwd.clone(),
                    *lock_or_recover(&s.last_activity, "acp session last activity"),
                )
            })
            .collect()
    };
    for (id, cwd, activity) in live {
        if cwd_filter.is_some_and(|f| cwd.as_path() != f) {
            continue;
        }
        match by_id.get_mut(&id) {
            Some(existing) => {
                let newer = match existing.updated_at {
                    Some(t) => activity > t,
                    None => true,
                };
                if newer {
                    existing.updated_at = Some(activity);
                }
            }
            None => {
                by_id.insert(
                    id.clone(),
                    ListedSession {
                        id,
                        cwd: cwd.display().to_string(),
                        title: None,
                        updated_at: Some(activity),
                    },
                );
            }
        }
    }
    let mut out: Vec<ListedSession> = by_id.into_values().collect();
    out.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .cmp(&a.updated_at.unwrap_or(SystemTime::UNIX_EPOCH))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Handle `session/list` (M5, ACP v1): optional `cursor` (opaque,
/// the offset of the next page) and `cwd` (absolute path filter).
/// Returns `{"sessions": [...]}` plus `nextCursor` when another page
/// follows; `nextCursor` is omitted on the last page.
fn handle_session_list(
    state: &AcpServerState,
    home: Option<&Path>,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let cwd_filter: Option<PathBuf> = match params.get("cwd").and_then(Value::as_str) {
        Some(cwd) => {
            let path = PathBuf::from(cwd);
            if !path.is_absolute() {
                return Err((
                    ERR_INVALID_PARAMS,
                    "session/list requires an absolute cwd".to_string(),
                ));
            }
            Some(path)
        }
        None => None,
    };
    let offset: usize = match params.get("cursor").and_then(Value::as_str) {
        Some(cursor) => cursor.parse().map_err(|_| {
            (
                ERR_INVALID_PARAMS,
                format!("session/list got an invalid cursor: {cursor}"),
            )
        })?,
        None => 0,
    };
    let listed = collect_listed_sessions(state, home, cwd_filter.as_deref());
    let total = listed.len();
    let page: Vec<Value> = listed
        .iter()
        .skip(offset)
        .take(SESSION_LIST_PAGE_SIZE)
        .map(ListedSession::to_json)
        .collect();
    let mut result = json!({ "sessions": page });
    if offset + SESSION_LIST_PAGE_SIZE < total {
        result["nextCursor"] = json!((offset + SESSION_LIST_PAGE_SIZE).to_string());
    }
    Ok(result)
}

/// Reject a client-provided `systemPrompt` on a lifecycle request
/// (M5): the agent does not advertise `sessionCapabilities.systemPrompt`,
/// so a compliant client never sends one. ACP v1 requires an agent
/// that cannot apply it to return an error instead of creating,
/// loading or resuming the session; -32602 is the recommended code.
fn reject_system_prompt(params: &Value) -> Result<(), (i64, String)> {
    if params.get("systemPrompt").is_some_and(|v| !v.is_null()) {
        return Err((
            ERR_INVALID_PARAMS,
            "systemPrompt is not supported by this agent".to_string(),
        ));
    }
    Ok(())
}

/// Resolve the working directory for a `session/load` / `session/resume`
/// request (M5). The schema marks `cwd` required; when it is absent
/// the stored session cwd is used instead (it is authoritative). A
/// relative `cwd`, or one that does not match the stored session cwd,
/// is rejected with -32602: a session belongs to the workspace it was
/// created in.
fn resolve_lifecycle_cwd(params: &Value, stored_cwd: &Path) -> Result<PathBuf, (i64, String)> {
    match params.get("cwd").and_then(Value::as_str) {
        Some(cwd) => {
            let path = PathBuf::from(cwd);
            if !path.is_absolute() {
                return Err((
                    ERR_INVALID_PARAMS,
                    "session/load and session/resume require an absolute cwd".to_string(),
                ));
            }
            if path != stored_cwd {
                return Err((
                    ERR_INVALID_PARAMS,
                    format!(
                        "cwd {} does not match the session working directory {}",
                        path.display(),
                        stored_cwd.display()
                    ),
                ));
            }
            Ok(path)
        }
        None => Ok(stored_cwd.to_path_buf()),
    }
}

/// Restore a persisted session (M5): find its journal, seed a live
/// session from the stored history, and register it. Returns the live
/// session and the history it was seeded with. A session that is
/// already active in this connection is returned as-is: resuming a
/// running session syncs the current state instead of replaying, and
/// loading replays the journal (a stable snapshot of the completed
/// turns) rather than the live agent, whose history may change
/// mid-replay while a turn runs.
async fn restore_session(
    cli: &Cli,
    home: Option<&Path>,
    params: &Value,
    state: &Arc<AcpServerState>,
) -> Result<(Arc<AcpSession>, Vec<ChatMessage>), (i64, String)> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                ERR_INVALID_PARAMS,
                "session/load and session/resume require a sessionId".to_string(),
            )
        })?
        .to_string();
    reject_system_prompt(params)?;
    // Already active in this connection: reuse it.
    if let Some(live) = {
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.get(&session_id).cloned()
    } {
        resolve_lifecycle_cwd(params, &live.cwd)?;
        // History for a load replay: prefer the journal (a stable
        // snapshot of the completed turns). Without a usable journal,
        // take the live history when the agent is idle; a running
        // turn holds the agent lock, so loading mid-turn is rejected
        // instead of replaying a history that changes under it.
        let history =
            match home.and_then(|h| crate::session_mgmt::find_managed_session(h, &session_id)) {
                Some((journal, false)) => crate::session::load_history(&journal).map_err(|e| {
                    (
                        ERR_INTERNAL,
                        format!("failed to read session history: {e:#}"),
                    )
                })?,
                _ => match live.agent.try_lock() {
                    Ok(agent) => agent.messages().to_vec(),
                    Err(_) => {
                        return Err((
                            ERR_INVALID_REQUEST,
                            "session has a prompt running".to_string(),
                        ));
                    }
                },
            };
        return Ok((live, history));
    }
    // Not active: the journal is the only source. Without a home dir
    // there is nowhere a journal could live.
    let Some(home_path) = home else {
        return Err((
            ERR_UNKNOWN_SESSION,
            format!("unknown session: {session_id}"),
        ));
    };
    let Some((journal, archived)) =
        crate::session_mgmt::find_managed_session(home_path, &session_id)
    else {
        return Err((
            ERR_UNKNOWN_SESSION,
            format!("unknown session: {session_id}"),
        ));
    };
    if archived {
        // An archived session is no longer a live session: it does
        // not come back through load/resume (restore it via the CLI
        // first, like any other archived session).
        return Err((
            ERR_UNKNOWN_SESSION,
            format!("unknown session: {session_id}"),
        ));
    }
    let stored_cwd = journal_cwd(&journal).ok_or_else(|| {
        (
            ERR_INTERNAL,
            format!("session journal has no recorded cwd: {}", journal.display()),
        )
    })?;
    let cwd = resolve_lifecycle_cwd(params, Path::new(&stored_cwd))?;
    let history = crate::session::load_history(&journal).map_err(|e| {
        (
            ERR_INTERNAL,
            format!("failed to read session history: {e:#}"),
        )
    })?;
    let session = build_acp_session(cli, &cwd, Some(home_path), state, &session_id, &history)
        .await
        .map_err(|e| (ERR_INTERNAL, format!("failed to restore session: {e:#}")))?;
    let session = Arc::new(session);
    {
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.insert(session_id, Arc::clone(&session));
    }
    Ok((session, history))
}

/// Handle `session/load` (M5, ACP v1): restore the session and replay
/// its whole history as `session/update` notifications, then answer
/// with the session's modes and config options. The response is sent
/// only after every history entry has been streamed, so the client
/// can continue the conversation as if it was never interrupted.
/// Live-only notices are never replayed (there are none to replay
/// yet: notices arrive in M6).
async fn handle_session_load(
    cli: &Cli,
    home: Option<&Path>,
    params: &Value,
    state: &Arc<AcpServerState>,
) -> Result<Value, (i64, String)> {
    let (session, history) = restore_session(cli, home, params, state).await?;
    replay_history(&session, &history);
    Ok(json!({
        "modes": session.modes_state(),
        "configOptions": session.config_options(),
    }))
}

/// Handle `session/resume` (M5, ACP v1): restore the session without
/// replaying any history. The response carries the session's current
/// modes and config options, which is the state sync for a session
/// that was already active in this connection.
async fn handle_session_resume(
    cli: &Cli,
    home: Option<&Path>,
    params: &Value,
    state: &Arc<AcpServerState>,
) -> Result<Value, (i64, String)> {
    let (session, _) = restore_session(cli, home, params, state).await?;
    Ok(json!({
        "modes": session.modes_state(),
        "configOptions": session.config_options(),
    }))
}

/// Replay a restored history to the client as `session/update`
/// notifications (M5, ACP v1 `session/load`): one `user_message_chunk`
/// per user message and one `agent_message_chunk` per assistant text,
/// each user chunk with a fresh `messageId` (the journal stores no
/// message ids), plus `tool_call` / `tool_call_update` for the
/// recorded tool calls and their results, matched by tool call id. A
/// tool call announced but never answered in the journal is closed
/// as `completed` at the end, so the client never leaves it hanging
/// in progress. Messages with no text are skipped; `system` messages
/// are not in the journal history (`load_history` drops them).
fn replay_history(session: &Arc<AcpSession>, history: &[ChatMessage]) {
    let mut announced: Vec<String> = vec![];
    let mut resolved: HashSet<String> = HashSet::new();
    for message in history {
        match message.role.as_str() {
            "user" => {
                let text = message.text();
                if text.trim().is_empty() {
                    continue;
                }
                session.sink.notify(SessionUpdate::UserMessageChunk {
                    content: ContentBlock::Text { text },
                    message_id: uuid::Uuid::new_v4().hyphenated().to_string(),
                });
            }
            "assistant" => {
                let text = message.text();
                if !text.trim().is_empty() {
                    session.sink.notify(SessionUpdate::AgentMessageChunk {
                        content: ContentBlock::Text { text },
                    });
                }
                if let Some(calls) = message.tool_calls.as_deref() {
                    for call in calls {
                        announced.push(call.id.clone());
                        session.sink.notify(SessionUpdate::ToolCall(ToolCallUpdate {
                            tool_call_id: call.id.clone(),
                            title: call.function.name.clone(),
                            kind: None,
                        }));
                    }
                }
            }
            "tool" => {
                let Some(id) = &message.tool_call_id else {
                    continue;
                };
                if !announced.contains(id) {
                    announced.push(id.clone());
                    session.sink.notify(SessionUpdate::ToolCall(ToolCallUpdate {
                        tool_call_id: id.clone(),
                        title: message.name.clone().unwrap_or_else(|| "tool".to_string()),
                        kind: None,
                    }));
                }
                resolved.insert(id.clone());
                let text = message.text();
                session
                    .sink
                    .notify(SessionUpdate::ToolCallUpdate(ToolCallResult {
                        tool_call_id: id.clone(),
                        status: "completed",
                        content: if text.trim().is_empty() {
                            None
                        } else {
                            Some(vec![ContentBlock::Text { text }])
                        },
                    }));
            }
            _ => {}
        }
    }
    for id in announced {
        if !resolved.contains(&id) {
            session
                .sink
                .notify(SessionUpdate::ToolCallUpdate(ToolCallResult {
                    tool_call_id: id,
                    status: "completed",
                    content: None,
                }));
        }
    }
}

/// Handle `session/close` (M5, ACP v1): cancel any ongoing work for
/// the session as if `session/cancel` had been called, then drop it
/// from the live map. The journal stays on disk, so the session can
/// be loaded or resumed later. Closing is idempotent: an unknown or
/// already-closed session answers with the same empty result (the
/// spec explicitly says so; clients also treat -32002 as success).
fn handle_session_close(state: &AcpServerState, params: &Value) -> Result<(), (i64, String)> {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return Err((
            ERR_INVALID_PARAMS,
            "session/close requires a sessionId".to_string(),
        ));
    };
    let session = {
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.remove(session_id)
    };
    if let Some(session) = session {
        // The prompt task keeps its own Arc and answers the in-flight
        // prompt as `cancelled`; close does not wait for it, so the
        // dispatcher stays responsive while the turn aborts.
        session.request_cancel();
    }
    Ok(())
}

/// Handle `session/delete` (M5, ACP v1): remove the session from
/// future `session/list` results by deleting its journal (and scratch
/// dir). An active session is closed first, so no turn keeps writing
/// to a deleted journal. Deleting a missing or already-deleted
/// session succeeds silently, as the spec requires.
fn handle_session_delete(
    state: &AcpServerState,
    home: Option<&Path>,
    params: &Value,
) -> Result<(), (i64, String)> {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return Err((
            ERR_INVALID_PARAMS,
            "session/delete requires a sessionId".to_string(),
        ));
    };
    {
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        if let Some(session) = sessions.remove(session_id) {
            session.request_cancel();
        }
    }
    if let Some(home) = home
        && crate::session_mgmt::find_managed_session(home, session_id).is_some()
    {
        crate::session_mgmt::delete_session(home, session_id)
            .map_err(|e| (ERR_INTERNAL, format!("session/delete failed: {e:#}")))?;
    }
    Ok(())
}

fn handle_cancel(sessions: &HashMap<String, Arc<AcpSession>>, params: &Value) {
    if let Some(sid) = params.get("sessionId").and_then(Value::as_str)
        && let Some(s) = sessions.get(sid)
    {
        s.request_cancel();
    }
}

/// The advertised mode list for the `session/new` response (M4): exactly
/// the four modes Q3 allows, never bypassPermissions/dontAsk. askAll
/// joins the advertised set (Phase 2 #8): it is the strictest
/// interactive mode, so a client that sees it can use it safely.
fn advertised_mode_list() -> Value {
    json!([
        {
            "id": "default",
            "name": "Default",
            "description": "Ask for permission before sensitive actions.",
        },
        {
            "id": "plan",
            "name": "Plan",
            "description": "Read-only planning: explore and propose a plan without changing files.",
        },
        {
            "id": "acceptEdits",
            "name": "Accept edits",
            "description": "Apply file edits without asking; other actions still ask for permission.",
        },
        {
            "id": "askAll",
            "name": "Ask all",
            "description": "Ask for permission before every tool call, including read-only tools.",
        },
    ])
}

/// The `model` config option advertised in `session/new` (M4): a select
/// over the CLI's free model catalog plus `auto` routing, with the
/// current model as the value. The value the client sends back is the
/// catalog id; the agent runs the catalog entry's `gateway_id`.
fn model_config_option(current_model: &str) -> Value {
    let mut options: Vec<Value> = crate::models::FREE_MODEL_CATALOG
        .iter()
        .map(|m| json!({ "id": m.id, "name": m.id }))
        .collect();
    options.push(json!({ "id": "auto", "name": "auto" }));
    // The current value must be one of the options: a model outside the
    // catalog (e.g. a custom model from settings) is listed verbatim so
    // the client can show it and switch back to it.
    let current_value = current_model_value_id(current_model);
    if !options
        .iter()
        .any(|o| o.get("id").and_then(Value::as_str) == Some(current_value.as_str()))
    {
        options.push(json!({ "id": current_value, "name": current_value }));
    }
    json!({
        "id": "model",
        "name": "Model",
        "description": "Language model for this session.",
        "type": "select",
        "currentValue": current_value,
        "options": options,
    })
}

/// The option id for the model the agent currently runs: the catalog id
/// whose gateway id matches, `auto` for auto-routing, the raw model
/// string for a custom model, or the default catalog entry when the
/// agent has no model yet (fake/test sessions).
fn current_model_value_id(model: &str) -> String {
    if model.is_empty() {
        return crate::models::FREE_MODEL_CATALOG
            .first()
            .map(|m| m.id.to_string())
            .unwrap_or_default();
    }
    if model == "auto" {
        return "auto".to_string();
    }
    if let Some(entry) = crate::models::FREE_MODEL_CATALOG
        .iter()
        .find(|m| m.gateway_id == model)
    {
        return entry.id.to_string();
    }
    model.to_string()
}

/// The model string the agent runs for a config-option value id: the
/// catalog entry's gateway id, or the value itself for `auto` and for a
/// model outside the catalog.
fn gateway_model_for_value_id(value_id: &str) -> String {
    match crate::models::catalog_entry(value_id) {
        Some(entry) => entry.gateway_id.to_string(),
        None => value_id.to_string(),
    }
}

/// Apply `session/set_mode` (M4, ACP v1): the mode must be one of the
/// advertised modes. `bypassPermissions` and `dontAsk` are rejected even
/// though Zen knows them: Q3 forbids advertising them, and a mode the
/// client never saw must never start running. An unknown session is
/// -32002 (ACP v1 unknown-session code); a bad mode is -32602. The
/// change applies at once when the agent is idle, or at the next turn
/// start when a turn is running.
fn handle_set_mode(
    sessions: &HashMap<String, Arc<AcpSession>>,
    params: &Value,
) -> Result<(), (i64, String)> {
    let session_id = params.get("sessionId").and_then(Value::as_str).ok_or((
        ERR_INVALID_PARAMS,
        "session/set_mode requires a sessionId".to_string(),
    ))?;
    let session = sessions.get(session_id).ok_or((
        ERR_UNKNOWN_SESSION,
        format!("unknown sessionId: {session_id}"),
    ))?;
    let mode_id = params.get("modeId").and_then(Value::as_str).ok_or((
        ERR_INVALID_PARAMS,
        "session/set_mode requires a modeId".to_string(),
    ))?;
    let mode = match PermissionMode::parse(mode_id) {
        Some(PermissionMode::BypassPermissions) | Some(PermissionMode::DontAsk) => {
            return Err((
                ERR_INVALID_PARAMS,
                format!("mode not advertised: {mode_id}"),
            ));
        }
        Some(mode) => mode,
        None => {
            return Err((ERR_INVALID_PARAMS, format!("unknown mode: {mode_id}")));
        }
    };
    session.set_mode(mode);
    Ok(())
}

/// Apply `session/set_config_option` (M4, ACP v1) and return the full
/// current option list, which the response MUST carry (not just the
/// changed option). Only the `model` select exists today; a boolean
/// shape for it, an unknown option id, or an unknown model value are
/// all -32602. An unknown session is -32002.
fn handle_set_config_option(
    sessions: &HashMap<String, Arc<AcpSession>>,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let session_id = params.get("sessionId").and_then(Value::as_str).ok_or((
        ERR_INVALID_PARAMS,
        "session/set_config_option requires a sessionId".to_string(),
    ))?;
    let session = sessions.get(session_id).ok_or((
        ERR_UNKNOWN_SESSION,
        format!("unknown sessionId: {session_id}"),
    ))?;
    let option_id = params
        .get("configOptionId")
        .and_then(Value::as_str)
        .ok_or((
            ERR_INVALID_PARAMS,
            "session/set_config_option requires a configOptionId".to_string(),
        ))?;
    if option_id != "model" {
        return Err((
            ERR_INVALID_PARAMS,
            format!("unknown config option: {option_id}"),
        ));
    }
    if params.get("type").and_then(Value::as_str) == Some("boolean") {
        return Err((
            ERR_INVALID_PARAMS,
            "option model is a select option, not a boolean".to_string(),
        ));
    }
    let value_id = params.get("valueId").and_then(Value::as_str).ok_or((
        ERR_INVALID_PARAMS,
        "session/set_config_option for model requires a valueId".to_string(),
    ))?;
    // Valid values: the catalog ids, `auto` routing, and the model the
    // session already runs (a custom model from settings stays
    // selectable). Anything else is rejected, never silently mapped.
    let current = session.current_model();
    let valid = !value_id.is_empty()
        && (value_id == "auto"
            || value_id == current
            || crate::models::catalog_entry(value_id).is_some());
    if !valid {
        return Err((ERR_INVALID_PARAMS, format!("unknown model: {value_id}")));
    }
    session.set_model(gateway_model_for_value_id(value_id));
    Ok(session.config_options())
}

/// Timeout for a `session/request_permission` round-trip (M4): the
/// default M2 timeout (30 s). A client that does not answer in time is
/// treated as a refusal (fail closed, Q4), never as approval.
const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The ACP prompter (M4): answers an agent permission question by
/// round-tripping `session/request_permission` to the client and mapping
/// the outcome onto Zen's `PromptAnswer`. The agent owns remembered
/// decisions (ACP v1 Tool Calls doc): `allow_always` is persisted by the
/// agent as an allow rule, `reject_always` in this prompter's
/// per-session set, so neither asks the client again.
///
/// STUB (#8): Phase 2 #8 (Z Red) defines Zen's 6-mode permission model
/// and the permission API #9 calls. That API does not exist in the repo
/// yet (checked 17/11: no phase2-#8 branch, no permission-model API in
/// zen-core/zen-agent), so M4 round-trips to the client directly
/// against `PermissionMode`. When #8 lands, its API plugs in here.
struct AcpPrompter {
    state: Arc<AcpServerState>,
    session_id: String,
    /// Tools the user rejected with `reject_always` this session.
    deny_always: Arc<Mutex<HashSet<String>>>,
}

impl AcpPrompter {
    fn new(state: Arc<AcpServerState>, session_id: String) -> Self {
        Self {
            state,
            session_id,
            deny_always: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The ACP tool kind for a permission request (M4): the agent's
    /// read-only flag is authoritative for `read`; edit tools map to
    /// `edit`, the shell to `execute`, everything else to `other` (the
    /// kind is only a client UI hint).
    fn tool_kind(tool_name: &str, read_only: bool) -> &'static str {
        if read_only {
            "read"
        } else if zen_tools::permissions::is_edit_tool(tool_name) {
            "edit"
        } else if tool_name == "Bash" {
            "execute"
        } else {
            "other"
        }
    }

    /// Map a `session/request_permission` result onto a `PromptAnswer`
    /// (M4): `cancelled` and both reject kinds deny; `allow_once`
    /// allows this once; `allow_always` allows and the caller persists
    /// it as an allow rule; `reject_always` denies and is remembered
    /// here. Anything unrecognised denies: an outcome the agent does
    /// not understand is never approval (ACP v1 Tool Calls doc,
    /// security guidance).
    fn map_permission_outcome(&self, result: &Value, tool_name: &str) -> PromptAnswer {
        let outcome = result.get("outcome");
        let kind = outcome
            .and_then(|o| o.get("type").and_then(Value::as_str))
            .unwrap_or("");
        match kind {
            "selected" => {
                let option_id = outcome
                    .and_then(|o| o.get("optionId").and_then(Value::as_str))
                    .unwrap_or("");
                match option_id {
                    "allow_once" => PromptAnswer::Yes,
                    "allow_always" => PromptAnswer::Always(tool_name.to_string()),
                    "reject_once" => PromptAnswer::No,
                    "reject_always" => {
                        lock_or_recover(&self.deny_always, "acp deny_always")
                            .insert(tool_name.to_string());
                        PromptAnswer::No
                    }
                    other => {
                        eprintln!("acp: unknown permission optionId {other:?}, denying");
                        PromptAnswer::No
                    }
                }
            }
            "cancelled" => PromptAnswer::No,
            other => {
                eprintln!("acp: unknown permission outcome {other:?}, denying");
                PromptAnswer::No
            }
        }
    }
}

#[async_trait::async_trait]
impl zen_core::AsyncPrompter for AcpPrompter {
    async fn ask_permission(
        &self,
        tool_name: &str,
        _input: &Value,
        tool_call_id: &str,
        read_only: bool,
    ) -> PromptAnswer {
        // A remembered reject_always answers without a round-trip: the
        // agent owns the remembered decision (ACP v1 Tool Calls doc).
        if lock_or_recover(&self.deny_always, "acp deny_always").contains(tool_name) {
            return PromptAnswer::No;
        }
        // The tool input is not sent: the client already saw this tool
        // call in the `tool_call` session update, and the request
        // carries the same id so the client can match them.
        let tool_call = ToolCallUpdate {
            tool_call_id: tool_call_id.to_string(),
            title: tool_name.to_string(),
            kind: Some(Self::tool_kind(tool_name, read_only).to_string()),
        };
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": tool_call,
            "options": [
                {"id": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"id": "allow_always", "name": format!("Allow always for {tool_name}"), "kind": "allow_always"},
                {"id": "reject_once", "name": "Reject once", "kind": "reject_once"},
                {"id": "reject_always", "name": format!("Reject always for {tool_name}"), "kind": "reject_always"},
            ],
        });
        match self
            .state
            .send_client_request(
                "session/request_permission",
                params,
                PERMISSION_REQUEST_TIMEOUT,
            )
            .await
        {
            Ok(result) => self.map_permission_outcome(&result, tool_name),
            Err(e) => {
                // Fail closed (Q4): a timeout, a client error, or a
                // disconnect is a refusal, never an approval.
                eprintln!("acp: permission request for {tool_name} failed ({e}), denying");
                PromptAnswer::No
            }
        }
    }
}

/// Cancel a request by its JSON-RPC id (M3, ACP v1 `$/cancel_request`):
/// a running prompt turn with this id is cancelled (same as
/// `session/cancel` for its session); a pending agent-initiated
/// request with this id has its waiter dropped, so the waiter fails
/// closed instead of waiting forever. Any other id is ignored: the
/// request is already done, or was never ours.
fn handle_dollar_cancel(state: &AcpServerState, params: &Value) {
    let Some(id) = params.get("id").cloned() else {
        return;
    };
    {
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        for session in sessions.values() {
            let running = lock_or_recover(&session.prompt_request_id, "acp prompt request id")
                .as_ref()
                == Some(&id);
            if running {
                session.request_cancel();
                return;
            }
        }
    }
    let mut pending = lock_or_recover(&state.pending, "acp pending");
    pending.remove(&id_key(&id));
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
        assert_eq!(caps["loadSession"], true);
        assert_eq!(caps["promptCapabilities"]["image"], false);
        assert_eq!(caps["promptCapabilities"]["audio"], false);
        assert_eq!(caps["promptCapabilities"]["embeddedContext"], false);
        assert_eq!(caps["mcpCapabilities"]["http"], false);
        assert_eq!(caps["mcpCapabilities"]["sse"], false);
        assert_eq!(
            caps["sessionCapabilities"],
            json!({
                "list": {},
                "resume": {},
                "close": {},
                "delete": {},
            })
        );
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
    fn validate_prompt_extracts_text_blocks() {
        let params = json!({
            "sessionId": "s1",
            "prompt": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" },
                { "type": "resource_link", "uri": "file:///x.md" },
            ]
        });
        let prompt = validate_prompt(&params).expect("baseline blocks must validate");
        assert_eq!(prompt.text, "hello\nworld\n[resource: file:///x.md]");
        assert_eq!(
            prompt.echo_blocks,
            ["hello", "world", "[resource: file:///x.md]"]
        );
    }

    #[test]
    fn validate_prompt_rejects_empty_prompt() {
        // Missing prompt, empty array, and content that renders to
        // nothing are all rejected (M3, ACP v1).
        assert!(validate_prompt(&json!({})).is_err());
        assert!(validate_prompt(&json!({ "prompt": [] })).is_err());
        assert!(
            validate_prompt(&json!({ "prompt": [{ "type": "text", "text": "   " }] })).is_err()
        );
        assert!(validate_prompt(&json!({ "prompt": [{ "type": "text", "text": "" }] })).is_err());
    }

    #[test]
    fn validate_prompt_rejects_unsupported_content() {
        // image/audio are not advertised (promptCapabilities false),
        // `resource` needs embeddedContext (also false): all are
        // rejected, never silently dropped (M3, ACP v1).
        for block in [
            json!({ "type": "image", "data": "x" }),
            json!({ "type": "audio", "data": "x" }),
            json!({ "type": "resource", "resource": { "uri": "file:///y.md" } }),
            json!({ "type": "video", "data": "x" }),
        ] {
            let params = json!({ "prompt": [block] });
            let err = validate_prompt(&params).expect_err("unsupported type must fail");
            assert!(err.contains("unsupported content type"), "{err}");
        }
        // A block without a type, and blocks without their payload.
        assert!(validate_prompt(&json!({ "prompt": [{ "text": "no type" }] })).is_err());
        assert!(validate_prompt(&json!({ "prompt": [{ "type": "text" }] })).is_err());
        assert!(validate_prompt(&json!({ "prompt": [{ "type": "resource_link" }] })).is_err());
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
    fn user_message_chunk_serialization_shape() {
        let update = SessionUpdate::UserMessageChunk {
            content: ContentBlock::Text { text: "hi".into() },
            message_id: "m1".into(),
        };
        let v = serde_json::to_value(&update).unwrap();
        assert_eq!(v["sessionUpdate"], "user_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");
        assert_eq!(v["messageId"], "m1");
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
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
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
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
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

    /// A fake LLM backend that always answers with a content-filter
    /// finish reason (M3): the agent maps it to `error_refusal`, and
    /// ACP maps that to stopReason `refusal`.
    struct ContentFilterLlm;

    #[async_trait::async_trait]
    impl LlmBackend for ContentFilterLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            Ok(zen_core::ChatResponse {
                message: ChatMessage::assistant(Some("blocked".into()), None),
                usage: zen_core::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                finish_reason: Some("content_filter".into()),
            })
        }
    }

    /// A fake LLM backend that always answers with a truncated tool
    /// call (M3): the agent maps it to `error_max_tokens`, and ACP
    /// maps that to stopReason `max_tokens`.
    struct TruncatedToolCallLlm;

    #[async_trait::async_trait]
    impl LlmBackend for TruncatedToolCallLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            Ok(zen_core::ChatResponse {
                message: ChatMessage::assistant(
                    Some("partial".into()),
                    Some(vec![zen_core::ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: zen_core::FunctionCall {
                            name: "Write".into(),
                            arguments: "{\"file_path\":\"a\",\"content\":\"xy".into(),
                        },
                    }]),
                ),
                usage: zen_core::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                finish_reason: Some("length".into()),
            })
        }
    }

    /// A fake LLM backend that always fails (M3): the agent maps the
    /// failure to `error_during_execution`, which has no dedicated
    /// stopReason and falls back to `end_turn`.
    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmBackend for FailingLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            Err(anyhow::anyhow!("boom 500"))
        }
    }

    /// Guard that records whether the future holding it was dropped
    /// before it finished (M3): the test LLM holds one across its
    /// sleep, so the flag proves a cancel really aborted the in-flight
    /// LLM call instead of letting it run to completion.
    struct AbortGuard {
        aborted: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    impl Drop for AbortGuard {
        fn drop(&mut self) {
            if !self.finished.load(Ordering::SeqCst) {
                self.aborted.store(true, Ordering::SeqCst);
            }
        }
    }

    /// A fake LLM backend that sleeps before replying and records
    /// whether its `chat` future was dropped mid-sleep (M3).
    struct DroppableLlm {
        aborted: Arc<AtomicBool>,
        started: Arc<AtomicBool>,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl LlmBackend for DroppableLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            let finished = Arc::new(AtomicBool::new(false));
            let _guard = AbortGuard {
                aborted: self.aborted.clone(),
                finished: finished.clone(),
            };
            self.started.store(true, Ordering::SeqCst);
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            finished.store(true, Ordering::SeqCst);
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

    /// A fake LLM backend that always answers with a tool call for
    /// the `Slow` tool (M3): the tool sleeps, so a cancel sent while
    /// it runs proves the in-flight tool call is reported cancelled.
    struct ToolCallLlm;

    #[async_trait::async_trait]
    impl LlmBackend for ToolCallLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            Ok(zen_core::ChatResponse {
                message: ChatMessage::assistant(
                    Some("calling slow tool".into()),
                    Some(vec![zen_core::ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: zen_core::FunctionCall {
                            name: "Slow".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                ),
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

    /// A fake LLM backend that answers text on the first call and a
    /// Slow tool call on every later call (M3): lets one session run
    /// a completed turn first, then a turn that can be cancelled
    /// mid-tool-call.
    struct TwoPhaseLlm {
        calls: AtomicU64,
    }

    impl Default for TwoPhaseLlm {
        fn default() -> Self {
            Self {
                calls: AtomicU64::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for TwoPhaseLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let usage = zen_core::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            };
            if n == 0 {
                Ok(zen_core::ChatResponse {
                    message: ChatMessage::assistant(Some("hello".into()), None),
                    usage,
                    finish_reason: Some("stop".into()),
                })
            } else {
                Ok(zen_core::ChatResponse {
                    message: ChatMessage::assistant(
                        Some("calling slow tool".into()),
                        Some(vec![zen_core::ToolCall {
                            id: "c1".into(),
                            call_type: "function".into(),
                            function: zen_core::FunctionCall {
                                name: "Slow".into(),
                                arguments: "{}".into(),
                            },
                        }]),
                    ),
                    usage,
                    finish_reason: Some("stop".into()),
                })
            }
        }
    }

    struct SlowTool {
        ms: u64,
    }

    #[async_trait::async_trait]
    impl zen_core::Tool for SlowTool {
        fn name(&self) -> &str {
            "Slow"
        }
        fn description(&self) -> &str {
            "slow test tool"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(&self, _args: Value, _ctx: &zen_core::ToolContext) -> zen_core::ToolOutput {
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            zen_core::ToolOutput::ok("finished")
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

    /// A permission checker that asks for every tool (test double).
    struct AskAll;

    impl zen_core::PermissionChecker for AskAll {
        fn check(
            &self,
            _tool_name: &str,
            _input: &Value,
            _read_only: bool,
            _ctx: &zen_core::ToolContext,
        ) -> zen_core::PermissionDecision {
            zen_core::PermissionDecision::Ask
        }
    }

    /// An async prompter that always answers Yes and records that it was
    /// asked (test double for the ACP round-trip).
    struct YesPrompter {
        asked: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl zen_core::AsyncPrompter for YesPrompter {
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

    /// A fake LLM backend that answers with one Slow tool call and then
    /// plain text (M4): exactly one permission question per turn.
    struct OneToolCallLlm {
        calls: AtomicU64,
    }

    impl Default for OneToolCallLlm {
        fn default() -> Self {
            Self {
                calls: AtomicU64::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for OneToolCallLlm {
        async fn chat(
            &self,
            _req: zen_core::ChatRequest,
        ) -> anyhow::Result<zen_core::ChatResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let usage = zen_core::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            };
            if n == 0 {
                Ok(zen_core::ChatResponse {
                    message: ChatMessage::assistant(
                        Some("calling slow tool".into()),
                        Some(vec![zen_core::ToolCall {
                            id: "c1".into(),
                            call_type: "function".into(),
                            function: zen_core::FunctionCall {
                                name: "Slow".into(),
                                arguments: "{}".into(),
                            },
                        }]),
                    ),
                    usage,
                    finish_reason: Some("stop".into()),
                })
            } else {
                Ok(zen_core::ChatResponse {
                    message: ChatMessage::assistant(Some("done".into()), None),
                    usage,
                    finish_reason: Some("stop".into()),
                })
            }
        }
    }

    /// Build a session backed by the delayed fake LLM and insert it into
    /// the test state, so dispatcher tests can run a real turn without a
    /// live LLM.
    fn fake_session(state: &Arc<AcpServerState>, session_id: &str, delay_ms: u64) {
        fake_session_with(
            state,
            session_id,
            Arc::new(DelayedLlm { delay_ms }),
            zen_core::ToolRegistry::new(),
        );
    }

    /// Build a session backed by the given fake LLM and tool registry
    /// and insert it into the test state, so dispatcher tests can run
    /// a real turn without a live LLM.
    fn fake_session_with(
        state: &Arc<AcpServerState>,
        session_id: &str,
        llm: Arc<dyn LlmBackend>,
        registry: zen_core::ToolRegistry,
    ) {
        fake_session_with_config(
            state,
            session_id,
            llm,
            registry,
            zen_agent::AgentConfig::default(),
        );
    }

    fn fake_session_with_config(
        state: &Arc<AcpServerState>,
        session_id: &str,
        llm: Arc<dyn LlmBackend>,
        registry: zen_core::ToolRegistry,
        config: zen_agent::AgentConfig,
    ) {
        let sink = Arc::new(AcpSink {
            out: state.out.clone(),
            session_id: session_id.into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
        });
        let mode = config.permission_mode;
        let model = config.model.clone();
        let agent = zen_agent::Agent::new(
            llm,
            registry,
            Arc::new(AllowAll),
            None,
            Arc::new(zen_core::NoHooks),
            sink.clone(),
            zen_core::ToolContext::new(std::env::temp_dir(), session_id),
            config,
        );
        let session = Arc::new(AcpSession::new(
            agent,
            sink,
            mode,
            String::new(),
            model,
            std::env::temp_dir(),
            None,
        ));
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.insert(session_id.to_string(), session);
    }

    /// Build a session backed by the given fake LLM, tool registry and
    /// permission checker, and return it (M4): the caller installs the
    /// async prompter on the session's agent before driving prompts.
    fn fake_session_with_checker(
        state: &Arc<AcpServerState>,
        session_id: &str,
        llm: Arc<dyn LlmBackend>,
        registry: zen_core::ToolRegistry,
        permissions: Arc<dyn zen_core::PermissionChecker>,
    ) -> Arc<AcpSession> {
        let sink = Arc::new(AcpSink {
            out: state.out.clone(),
            session_id: session_id.into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
        });
        let config = zen_agent::AgentConfig::default();
        let mode = config.permission_mode;
        let model = config.model.clone();
        let agent = zen_agent::Agent::new(
            llm,
            registry,
            permissions,
            None,
            Arc::new(zen_core::NoHooks),
            sink.clone(),
            zen_core::ToolContext::new(std::env::temp_dir(), session_id),
            config,
        );
        let session = Arc::new(AcpSession::new(
            agent,
            sink,
            mode,
            String::new(),
            model,
            std::env::temp_dir(),
            None,
        ));
        let mut sessions = lock_or_recover(&state.sessions, "acp sessions");
        sessions.insert(session_id.to_string(), Arc::clone(&session));
        session
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
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}]}})
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
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "one"}]}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "two"}]}})
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
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "hi"}]}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "nope", "prompt": [{"type": "text", "text": "hi"}]}})
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
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}]}})
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

    /// M3: a cancelled turn reports the usage DELTA, not the sum.
    /// Turn 1 completes (15 tokens); turn 2 is cancelled while its
    /// tool call runs, with the agent holding 30 tokens at the
    /// cancel, so the delta is 15 and a sum would report 45.
    #[tokio::test]
    async fn cancelled_turn_reports_usage_delta_not_sum() {
        let (state, buf) = test_state();
        let mut registry = zen_core::ToolRegistry::new();
        registry.register(Arc::new(SlowTool { ms: 2000 }));
        fake_session_with(&state, "s1", Arc::new(TwoPhaseLlm::default()), registry);
        let (tx, run) = drive_dispatcher_live(Arc::clone(&state));
        tx.send(init_line(1)).unwrap();
        tx.send(prompt_line(2, "hi")).unwrap();
        let _ = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        tx.send(prompt_line(3, "hi again")).unwrap();
        // Wait for the tool call to be announced, then cancel while
        // the tool is still running.
        let _ = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(is_update("tool_call"))
        })
        .await;
        tx.send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})
                .to_string(),
        )
        .unwrap();
        drop(tx);
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 3).is_some()
        })
        .await;
        let prompt = find_response(&lines, 3).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{lines:?}");
        let usages: Vec<&Value> = lines
            .iter()
            .filter(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/update")
                    && l["params"]["update"]["sessionUpdate"].as_str() == Some("usage_update")
            })
            .collect();
        assert_eq!(usages.len(), 2, "{lines:?}");
        assert_eq!(usages[1]["params"]["update"]["used"], 15, "{lines:?}");
        run.await.unwrap().unwrap();
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
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "one"}]}})
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
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "two"}]}})
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
    /// Drive the dispatcher with a live channel, so the test can send
    /// lines after observing output (M3): `drive_dispatcher` sends
    /// everything up front, which cannot time a cancel mid-turn.
    fn drive_dispatcher_live(
        state: Arc<AcpServerState>,
    ) -> (
        mpsc::UnboundedSender<String>,
        tokio::task::JoinHandle<Result<i32>>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let cwd = std::env::temp_dir();
        let run = tokio::spawn(run_dispatcher(cli, cwd, None, state, rx));
        (tx, run)
    }

    fn prompt_line(id: u64, text: &str) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": text}]}})
            .to_string()
    }

    fn is_update(variant: &str) -> impl Fn(&Value) -> bool + '_ {
        move |l: &Value| {
            l.get("method").and_then(Value::as_str) == Some("session/update")
                && l["params"]["update"]["sessionUpdate"].as_str() == Some(variant)
        }
    }

    /// M3: the prompt is echoed as one `user_message_chunk` per block,
    /// all sharing one `messageId`, before the agent's reply.
    #[tokio::test]
    async fn prompt_echoes_user_message_with_shared_message_id() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}, {"type": "resource_link", "uri": "file:///x.md"}]}})
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
        let is_echo = is_update("user_message_chunk");
        let echoes: Vec<&Value> = lines.iter().filter(|l| is_echo(l)).collect();
        assert_eq!(echoes.len(), 2, "{lines:?}");
        let message_id = echoes[0]["params"]["update"]["messageId"]
            .as_str()
            .expect("echo must carry a messageId");
        assert!(!message_id.is_empty());
        assert_eq!(
            echoes[1]["params"]["update"]["messageId"].as_str(),
            Some(message_id),
            "chunks of one turn share one messageId: {lines:?}"
        );
        assert_eq!(echoes[0]["params"]["update"]["content"]["text"], "hi");
        assert_eq!(
            echoes[1]["params"]["update"]["content"]["text"],
            "[resource: file:///x.md]"
        );
        // The echo precedes the agent's reply.
        let echo_pos = lines
            .iter()
            .position(is_update("user_message_chunk"))
            .unwrap();
        let agent_pos = lines
            .iter()
            .position(is_update("agent_message_chunk"))
            .expect("agent reply must be emitted");
        assert!(echo_pos < agent_pos, "{lines:?}");
    }

    /// M3: an empty prompt is rejected with -32602 and no turn starts.
    #[tokio::test]
    async fn prompt_empty_returns_invalid_params() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": []}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1"}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some() && find_response(ls, 3).is_some()
        })
        .await;
        for id in [2, 3] {
            let resp = find_response(&lines, id).unwrap();
            assert_eq!(resp["error"]["code"], -32602, "{lines:?}");
        }
        // No turn started: no user echo was emitted.
        assert!(
            !lines.iter().any(is_update("user_message_chunk")),
            "{lines:?}"
        );
    }

    /// M3: image / `resource` / unknown content is rejected with
    /// -32602, never silently dropped.
    #[tokio::test]
    async fn prompt_unsupported_content_returns_invalid_params() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "image", "data": "x"}]}})
                    .to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "resource", "resource": {"uri": "file:///y.md"}}]}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some() && find_response(ls, 3).is_some()
        })
        .await;
        for id in [2, 3] {
            let resp = find_response(&lines, id).unwrap();
            assert_eq!(resp["error"]["code"], -32602, "{lines:?}");
            assert!(
                resp["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("unsupported content type"),
                "{lines:?}"
            );
        }
    }

    /// M3: a content-filtered turn ends with stopReason `refusal`.
    #[tokio::test]
    async fn prompt_refusal_maps_to_stop_reason() {
        let (state, buf) = test_state();
        fake_session_with(
            &state,
            "s1",
            Arc::new(ContentFilterLlm),
            zen_core::ToolRegistry::new(),
        );
        let run = drive_dispatcher(Arc::clone(&state), vec![init_line(1), prompt_line(2, "hi")]);
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "refusal", "{lines:?}");
    }

    /// M3: a turn cut off by the output token limit ends with
    /// stopReason `max_tokens`.
    #[tokio::test]
    async fn prompt_max_tokens_maps_to_stop_reason() {
        let (state, buf) = test_state();
        fake_session_with(
            &state,
            "s1",
            Arc::new(TruncatedToolCallLlm),
            zen_core::ToolRegistry::new(),
        );
        let run = drive_dispatcher(Arc::clone(&state), vec![init_line(1), prompt_line(2, "hi")]);
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "max_tokens", "{lines:?}");
    }

    /// M3: a turn that hits the agent's turn cap maps to stopReason
    /// `max_turn_requests` (v1). The backend keeps answering with a
    /// tool call, so the loop would run forever; with max_turns 1 the
    /// agent stops after the first tool call.
    #[tokio::test]
    async fn prompt_max_turns_maps_to_stop_reason() {
        let (state, buf) = test_state();
        let mut registry = zen_core::ToolRegistry::new();
        registry.register(Arc::new(SlowTool { ms: 0 }));
        fake_session_with_config(
            &state,
            "s1",
            Arc::new(ToolCallLlm),
            registry,
            zen_agent::AgentConfig {
                max_turns: 1,
                ..Default::default()
            },
        );
        let run = drive_dispatcher(Arc::clone(&state), vec![init_line(1), prompt_line(2, "hi")]);
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(
            prompt["result"]["stopReason"], "max_turn_requests",
            "{lines:?}"
        );
    }

    /// M3: a turn whose LLM call fails ends the turn (v1 has no error
    /// stopReason); the failure is in the streamed content.
    #[tokio::test]
    async fn prompt_llm_error_falls_back_to_end_turn() {
        let (state, buf) = test_state();
        fake_session_with(
            &state,
            "s1",
            Arc::new(FailingLlm),
            zen_core::ToolRegistry::new(),
        );
        let run = drive_dispatcher(Arc::clone(&state), vec![init_line(1), prompt_line(2, "hi")]);
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{lines:?}");
    }

    /// M3: a cancel sent while the LLM call is in flight aborts the
    /// call (the `chat` future is dropped, not run to completion) and
    /// leaves no trace of the turn in history.
    #[tokio::test]
    async fn cancel_aborts_llm_call_and_rolls_back_history() {
        let (state, buf) = test_state();
        let aborted = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        fake_session_with(
            &state,
            "s1",
            Arc::new(DroppableLlm {
                aborted: aborted.clone(),
                started: started.clone(),
                delay_ms: 2000,
            }),
            zen_core::ToolRegistry::new(),
        );
        let (tx, run) = drive_dispatcher_live(Arc::clone(&state));
        tx.send(init_line(1)).unwrap();
        tx.send(prompt_line(2, "hi")).unwrap();
        // Wait until the LLM call is actually in flight, then cancel:
        // a cancel sent earlier would be caught by the pre-start check
        // instead of aborting the call.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if started.load(Ordering::SeqCst) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("LLM call never started");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let t0 = std::time::Instant::now();
        tx.send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})
                .to_string(),
        )
        .unwrap();
        drop(tx);
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        // The turn must end well before the 2 s LLM delay: the call
        // was aborted, not awaited.
        assert!(
            t0.elapsed() < Duration::from_millis(1500),
            "cancel must abort the turn early; took {:?}",
            t0.elapsed()
        );
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{lines:?}");
        assert!(
            aborted.load(Ordering::SeqCst),
            "the in-flight LLM call must be aborted: {lines:?}"
        );
        // History rolled back: the cancelled turn left no messages.
        let session = {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            sessions.get("s1").unwrap().clone()
        };
        let agent = session.agent.lock().await;
        assert!(
            agent.messages().is_empty(),
            "cancelled turn must not leave history"
        );
        run.await.unwrap().unwrap();
    }

    /// M3: a cancel sent while a tool call runs reports the call
    /// `cancelled` before the prompt response, and never `completed`
    /// afterwards.
    #[tokio::test]
    async fn cancel_marks_in_flight_tool_call_cancelled() {
        let (state, buf) = test_state();
        let mut registry = zen_core::ToolRegistry::new();
        registry.register(Arc::new(SlowTool { ms: 2000 }));
        fake_session_with(&state, "s1", Arc::new(ToolCallLlm), registry);
        let (tx, run) = drive_dispatcher_live(Arc::clone(&state));
        tx.send(init_line(1)).unwrap();
        tx.send(prompt_line(2, "hi")).unwrap();
        // Wait for the tool call to be announced, then cancel while
        // the tool is still running.
        let _ = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(is_update("tool_call"))
        })
        .await;
        tx.send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})
                .to_string(),
        )
        .unwrap();
        drop(tx);
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{lines:?}");
        let cancelled = lines.iter().find(|l| {
            l.get("method").and_then(Value::as_str) == Some("session/update")
                && l["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call_update")
                && l["params"]["update"]["toolCallId"].as_str() == Some("c1")
                && l["params"]["update"]["status"].as_str() == Some("cancelled")
        });
        assert!(
            cancelled.is_some(),
            "in-flight tool call must be cancelled: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/update")
                    && l["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call_update")
                    && l["params"]["update"]["toolCallId"].as_str() == Some("c1")
                    && l["params"]["update"]["status"].as_str() == Some("completed")
            }),
            "cancelled tool call must never complete afterwards: {lines:?}"
        );
        run.await.unwrap().unwrap();
    }

    /// M3: `$/cancel_request` with the running prompt's id cancels
    /// the turn, like `session/cancel`.
    #[tokio::test]
    async fn dollar_cancel_request_cancels_prompt_by_id() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 2000);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                prompt_line(2, "hi"),
                json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"id": 2}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let prompt = find_response(&lines, 2).unwrap();
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{lines:?}");
    }

    /// M3: `$/cancel_request` with an unknown id is ignored: the turn
    /// runs to completion.
    #[tokio::test]
    async fn dollar_cancel_request_ignores_unknown_id() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                prompt_line(2, "hi"),
                json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"id": 99}})
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
    }

    /// M3: a session is free again after a cancelled turn: the next
    /// prompt is accepted, not rejected as busy.
    #[tokio::test]
    async fn prompt_after_cancel_is_accepted() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 2000);
        let (tx, run) = drive_dispatcher_live(Arc::clone(&state));
        tx.send(init_line(1)).unwrap();
        tx.send(prompt_line(2, "one")).unwrap();
        tx.send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})
                .to_string(),
        )
        .unwrap();
        let _ = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        tx.send(prompt_line(3, "two")).unwrap();
        drop(tx);
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 3).is_some()
        })
        .await;
        let second = find_response(&lines, 3).unwrap();
        assert_eq!(second["result"]["stopReason"], "end_turn", "{lines:?}");
        run.await.unwrap().unwrap();
    }

    /// M3: `cancel_pending_tools` reports announced-but-unfinished
    /// calls as `cancelled`, and skips calls that already finished.
    #[test]
    fn cancel_pending_tools_reports_cancelled() {
        let (state, buf) = test_state();
        let sink = Arc::new(AcpSink {
            out: state.out.clone(),
            session_id: "s1".into(),
            tools: Arc::new(Mutex::new(HashMap::new())),
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
        });
        let tool_call = |id: &str| {
            ChatMessage::assistant(
                None,
                Some(vec![zen_core::ToolCall {
                    id: id.into(),
                    call_type: "function".into(),
                    function: zen_core::FunctionCall {
                        name: "Read".into(),
                        arguments: "{}".into(),
                    },
                }]),
            )
        };
        sink.emit(AgentEvent::Assistant {
            message: tool_call("c1"),
            parent_tool_use_id: None,
        });
        sink.emit(AgentEvent::Assistant {
            message: tool_call("c2"),
            parent_tool_use_id: None,
        });
        // c1 finishes: it must not be reported cancelled.
        sink.emit(AgentEvent::ToolResult {
            tool_use_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            parent_tool_use_id: None,
            parallel: false,
        });
        sink.cancel_pending_tools();
        let lines = read_output_lines(&buf);
        let cancelled: Vec<&Value> = lines
            .iter()
            .filter(|l| {
                l["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call_update")
                    && l["params"]["update"]["status"].as_str() == Some("cancelled")
            })
            .collect();
        assert_eq!(cancelled.len(), 1, "{lines:?}");
        assert_eq!(cancelled[0]["params"]["update"]["toolCallId"], "c2");
    }

    /// M4: `session/set_mode` switches the session mode, applies it to
    /// the idle agent at once, and rejects unadvertised modes, unknown
    /// modes and unknown sessions.
    #[tokio::test]
    async fn set_mode_roundtrip_via_dispatcher() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_mode", "params": {"sessionId": "s1", "modeId": "plan"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/set_mode", "params": {"sessionId": "s1", "modeId": "bypassPermissions"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 4, "method": "session/set_mode", "params": {"sessionId": "s1", "modeId": "bogus"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 5, "method": "session/set_mode", "params": {"sessionId": "nope", "modeId": "plan"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 6, "method": "session/set_mode", "params": {"sessionId": "s1", "modeId": "dontAsk"}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 6).is_some()
        })
        .await;
        let ok = find_response(&lines, 2).unwrap();
        assert!(ok.get("error").is_none(), "{lines:?}");
        assert_eq!(ok["result"], json!({}));
        // The session runs in plan mode now, and the idle agent picked
        // it up at once (config + checker mode).
        {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            let s = sessions.get("s1").unwrap();
            assert_eq!(s.current_mode(), PermissionMode::Plan);
            let agent = s.agent.try_lock().expect("agent must be idle");
            assert_eq!(agent.config.permission_mode, PermissionMode::Plan);
        }
        let bypass = find_response(&lines, 3).unwrap();
        assert_eq!(bypass["error"]["code"], -32602, "{lines:?}");
        assert!(
            bypass["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not advertised"),
            "{lines:?}"
        );
        let bogus = find_response(&lines, 4).unwrap();
        assert_eq!(bogus["error"]["code"], -32602, "{lines:?}");
        assert!(
            bogus["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown mode"),
            "{lines:?}"
        );
        let unknown = find_response(&lines, 5).unwrap();
        assert_eq!(unknown["error"]["code"], -32002, "{lines:?}");
        let dont_ask = find_response(&lines, 6).unwrap();
        assert_eq!(dont_ask["error"]["code"], -32602, "{lines:?}");
        assert!(
            dont_ask["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not advertised"),
            "{lines:?}"
        );
    }

    /// M4: `session/set_config_option` switches the model and answers
    /// the full option list; bad values are -32602.
    #[tokio::test]
    async fn set_config_option_model_roundtrip_via_dispatcher() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_config_option", "params": {"sessionId": "s1", "configOptionId": "model", "valueId": "qwen/qwen3-coder:free"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/set_config_option", "params": {"sessionId": "s1", "configOptionId": "model", "valueId": "no-such-model"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 4, "method": "session/set_config_option", "params": {"sessionId": "s1", "configOptionId": "model", "type": "boolean", "value": true}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 5, "method": "session/set_config_option", "params": {"sessionId": "s1", "configOptionId": "bogus", "valueId": "x"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 6, "method": "session/set_config_option", "params": {"sessionId": "nope", "configOptionId": "model", "valueId": "auto"}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 6).is_some()
        })
        .await;
        let ok = find_response(&lines, 2).unwrap();
        assert!(ok.get("error").is_none(), "{lines:?}");
        assert_eq!(
            ok["result"]["configOptions"][0]["currentValue"], "qwen/qwen3-coder:free",
            "{lines:?}"
        );
        {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            let s = sessions.get("s1").unwrap();
            assert_eq!(s.current_model(), "qwen/qwen3-coder:free");
            let agent = s.agent.try_lock().expect("agent must be idle");
            assert_eq!(agent.config.model, "qwen/qwen3-coder:free");
        }
        for (id, msg) in [
            (3, "unknown model"),
            (4, "not a boolean"),
            (5, "unknown config option"),
        ] {
            let r = find_response(&lines, id).unwrap();
            assert_eq!(r["error"]["code"], -32602, "{lines:?}");
            assert!(
                r["error"]["message"].as_str().unwrap().contains(msg),
                "{lines:?}"
            );
        }
        let unknown = find_response(&lines, 6).unwrap();
        assert_eq!(unknown["error"]["code"], -32002, "{lines:?}");
    }

    /// M4: `session/set_config_option` accepts `auto` (auto-routing) and
    /// the session's current model (a custom model from settings stays
    /// selectable), not only catalog ids.
    #[tokio::test]
    async fn set_config_option_accepts_auto_and_current_model() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        fake_session_with_config(
            &state,
            "s2",
            Arc::new(DelayedLlm { delay_ms: 0 }),
            zen_core::ToolRegistry::new(),
            zen_agent::AgentConfig {
                model: "my-custom-model".to_string(),
                ..Default::default()
            },
        );
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_config_option", "params": {"sessionId": "s1", "configOptionId": "model", "valueId": "auto"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/set_config_option", "params": {"sessionId": "s2", "configOptionId": "model", "valueId": "my-custom-model"}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 3).is_some()
        })
        .await;
        let auto = find_response(&lines, 2).unwrap();
        assert!(auto.get("error").is_none(), "{lines:?}");
        assert_eq!(
            auto["result"]["configOptions"][0]["currentValue"], "auto",
            "{lines:?}"
        );
        let custom = find_response(&lines, 3).unwrap();
        assert!(custom.get("error").is_none(), "{lines:?}");
        assert_eq!(
            custom["result"]["configOptions"][0]["currentValue"], "my-custom-model",
            "{lines:?}"
        );
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        assert_eq!(sessions.get("s1").unwrap().current_model(), "auto");
        assert_eq!(
            sessions.get("s2").unwrap().current_model(),
            "my-custom-model"
        );
    }

    /// M4: a fake session advertises exactly the three Q3 modes and a
    /// model select whose current value is always one of its options.
    #[test]
    fn session_advertises_modes_and_model_option() {
        let (state, _buf) = test_state();
        fake_session(&state, "s1", 0);
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        let s = sessions.get("s1").unwrap();
        let modes = s.modes_state();
        assert_eq!(modes["currentModeId"], "default", "{modes:?}");
        let ids: Vec<&str> = modes["availableModes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["default", "plan", "acceptEdits", "askAll"],
            "{modes:?}"
        );
        assert!(
            !modes.to_string().contains("bypassPermissions")
                && !modes.to_string().contains("dontAsk"),
            "{modes:?}"
        );
        let options = s.config_options();
        let model = &options[0];
        assert_eq!(model["id"], "model");
        assert_eq!(model["type"], "select");
        let current = model["currentValue"].as_str().unwrap();
        assert!(
            model["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["id"].as_str() == Some(current)),
            "{options:?}"
        );
        // The fake session has no model, so the value falls back to the
        // default catalog entry.
        assert_eq!(current, "deepseek-v4.1-flash", "{options:?}");
    }

    /// M4: the outcome mapping for `session/request_permission` is
    /// pure: every shape the client may answer maps to exactly one
    /// `PromptAnswer`, and anything unrecognised denies.
    #[test]
    fn permission_outcome_mapping() {
        let (state, _buf) = test_state();
        let prompter = AcpPrompter::new(Arc::clone(&state), "s1".to_string());
        let selected =
            |option_id: &str| json!({"outcome": {"type": "selected", "optionId": option_id}});
        assert_eq!(
            prompter.map_permission_outcome(&selected("allow_once"), "Read"),
            PromptAnswer::Yes
        );
        assert_eq!(
            prompter.map_permission_outcome(&selected("allow_always"), "Read"),
            PromptAnswer::Always("Read".to_string())
        );
        assert_eq!(
            prompter.map_permission_outcome(&selected("reject_once"), "Read"),
            PromptAnswer::No
        );
        assert_eq!(
            prompter.map_permission_outcome(&selected("reject_always"), "Read"),
            PromptAnswer::No
        );
        assert_eq!(
            prompter.map_permission_outcome(&json!({"outcome": {"type": "cancelled"}}), "Read"),
            PromptAnswer::No
        );
        assert_eq!(
            prompter.map_permission_outcome(&selected("bogus"), "Read"),
            PromptAnswer::No
        );
        assert_eq!(
            prompter.map_permission_outcome(&json!({}), "Read"),
            PromptAnswer::No
        );
        assert_eq!(
            prompter.map_permission_outcome(&json!({"outcome": {"type": "weird"}}), "Read"),
            PromptAnswer::No
        );
    }

    /// M4: the permission round-trip over a fake client: the request
    /// carries the session id, the tool call id and the four options,
    /// and each outcome maps as specified.
    #[tokio::test]
    async fn acp_prompter_roundtrip_with_fake_client() {
        let (state, buf) = test_state();
        let prompter = Arc::new(AcpPrompter::new(Arc::clone(&state), "s1".to_string()));
        let mut answered = 0usize;
        for (result, expected) in [
            (
                json!({"outcome": {"type": "selected", "optionId": "allow_once"}}),
                PromptAnswer::Yes,
            ),
            (
                json!({"outcome": {"type": "selected", "optionId": "allow_always"}}),
                PromptAnswer::Always("Bash".to_string()),
            ),
            (
                json!({"outcome": {"type": "selected", "optionId": "reject_once"}}),
                PromptAnswer::No,
            ),
            (json!({"outcome": {"type": "cancelled"}}), PromptAnswer::No),
        ] {
            let asked = tokio::spawn({
                let prompter = Arc::clone(&prompter);
                async move {
                    use zen_core::AsyncPrompter;
                    prompter
                        .ask_permission("Bash", &json!({"command": "ls"}), "c1", false)
                        .await
                }
            });
            let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
                ls.iter()
                    .filter(|l| {
                        l.get("method").and_then(Value::as_str)
                            == Some("session/request_permission")
                    })
                    .count()
                    > answered
            })
            .await;
            let requests: Vec<&Value> = lines
                .iter()
                .filter(|l| {
                    l.get("method").and_then(Value::as_str) == Some("session/request_permission")
                })
                .collect();
            answered = requests.len();
            let request = requests.last().expect("permission request must be written");
            assert_eq!(request["params"]["sessionId"], "s1", "{request:?}");
            assert_eq!(
                request["params"]["toolCall"]["toolCallId"], "c1",
                "{request:?}"
            );
            assert_eq!(
                request["params"]["toolCall"]["title"], "Bash",
                "{request:?}"
            );
            assert_eq!(
                request["params"]["toolCall"]["kind"], "execute",
                "{request:?}"
            );
            assert_eq!(
                request["params"]["options"].as_array().unwrap().len(),
                4,
                "{request:?}"
            );
            let id = request["id"].clone();
            assert!(route_client_response(
                &state,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            ));
            assert_eq!(asked.await.unwrap(), expected);
        }
    }

    /// M4: `reject_always` is remembered for the session: the second
    /// identical question is denied without a second round-trip.
    #[tokio::test]
    async fn acp_prompter_reject_always_is_remembered() {
        let (state, buf) = test_state();
        let prompter = Arc::new(AcpPrompter::new(Arc::clone(&state), "s1".to_string()));
        let ask_once = |prompter: Arc<AcpPrompter>| {
            tokio::spawn(async move {
                use zen_core::AsyncPrompter;
                prompter
                    .ask_permission("Bash", &json!({}), "c1", false)
                    .await
            })
        };
        let first = ask_once(Arc::clone(&prompter));
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/request_permission")
            })
        })
        .await;
        let id = lines
            .iter()
            .find(|l| l.get("method").and_then(Value::as_str) == Some("session/request_permission"))
            .unwrap()["id"]
            .clone();
        assert!(route_client_response(
            &state,
            &json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"type": "selected", "optionId": "reject_always"}}}),
        ));
        assert_eq!(first.await.unwrap(), PromptAnswer::No);
        // Second ask: no request is written at all.
        let second = ask_once(Arc::clone(&prompter));
        assert_eq!(second.await.unwrap(), PromptAnswer::No);
        let lines = read_output_lines(&buf);
        let requests = lines
            .iter()
            .filter(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/request_permission")
            })
            .count();
        assert_eq!(requests, 1, "{lines:?}");
    }

    /// M4: a client error answering the permission request denies
    /// (fail closed, Q4) instead of waiting for an answer that never
    /// comes.
    #[tokio::test]
    async fn acp_prompter_fail_closed_on_client_error() {
        let (state, buf) = test_state();
        let prompter = AcpPrompter::new(Arc::clone(&state), "s1".to_string());
        let asked = tokio::spawn(async move {
            use zen_core::AsyncPrompter;
            prompter
                .ask_permission("Bash", &json!({}), "c1", false)
                .await
        });
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            ls.iter().any(|l| {
                l.get("method").and_then(Value::as_str) == Some("session/request_permission")
            })
        })
        .await;
        let id = lines
            .iter()
            .find(|l| l.get("method").and_then(Value::as_str) == Some("session/request_permission"))
            .unwrap()["id"]
            .clone();
        assert!(route_client_response(
            &state,
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}}),
        ));
        assert_eq!(asked.await.unwrap(), PromptAnswer::No);
    }

    /// M4: a set_mode that arrives while a turn runs is accepted at
    /// once and applies at the next turn start (the running turn keeps
    /// the mode it started with).
    #[tokio::test]
    async fn set_mode_during_turn_applies_at_next_turn() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 800);
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let _run = tokio::spawn(run_dispatcher(
            cli,
            std::env::temp_dir(),
            None,
            Arc::clone(&state),
            rx,
        ));
        tx.send(init_line(1)).unwrap();
        tx.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "one"}]}})
                .to_string(),
        )
        .unwrap();
        // The mode switch arrives while the 800 ms turn runs.
        tx.send(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/set_mode", "params": {"sessionId": "s1", "modeId": "plan"}})
                .to_string(),
        )
        .unwrap();
        // The set_mode answer must not wait for the turn.
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 3).is_some()
        })
        .await;
        let mode_resp = find_response(&lines, 3).unwrap();
        assert!(mode_resp.get("error").is_none(), "{lines:?}");
        assert_eq!(mode_resp["result"], json!({}));
        // The mode is recorded while the turn still runs in Default.
        {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            let s = sessions.get("s1").unwrap();
            assert_eq!(s.current_mode(), PermissionMode::Plan);
        }
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        assert_eq!(
            find_response(&lines, 2).unwrap()["result"]["stopReason"],
            "end_turn",
            "{lines:?}"
        );
        // Wait for the turn task to drop its guard before the next prompt.
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
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "two"}]}})
                .to_string(),
        )
        .unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 4).is_some()
        })
        .await;
        assert_eq!(
            find_response(&lines, 4).unwrap()["result"]["stopReason"],
            "end_turn",
            "{lines:?}"
        );
        // The second turn applied the recorded mode at its start.
        let sessions = lock_or_recover(&state.sessions, "acp sessions");
        let s = sessions.get("s1").unwrap();
        let agent = s.agent.try_lock().expect("agent must be idle");
        assert_eq!(agent.config.permission_mode, PermissionMode::Plan);
    }

    /// M4: the agent installed as a session's async prompter is the one
    /// a turn asks: with an Ask checker and a Yes prompter, the Slow tool
    /// runs and the turn ends end_turn. (Without the setter the agent has
    /// no prompter, the Ask denies, and the tool never runs.)
    #[tokio::test]
    async fn async_prompter_is_consulted_during_turn() {
        let (state, buf) = test_state();
        let asked = Arc::new(AtomicBool::new(false));
        let mut registry = zen_core::ToolRegistry::new();
        registry.register(Arc::new(SlowTool { ms: 0 }));
        let session = fake_session_with_checker(
            &state,
            "s1",
            Arc::new(OneToolCallLlm::default()),
            registry,
            Arc::new(AskAll),
        );
        {
            let mut agent = session.agent.lock().await;
            agent.set_async_prompter(Arc::new(YesPrompter {
                asked: Arc::clone(&asked),
            }));
        }
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": "s1", "prompt": [{"type": "text", "text": "hi"}]}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        assert_eq!(
            find_response(&lines, 2).unwrap()["result"]["stopReason"],
            "end_turn",
            "{lines:?}"
        );
        assert!(
            asked.load(Ordering::SeqCst),
            "the async prompter must have been asked"
        );
        // The tool ran: its result is in the tool_call_update stream.
        assert!(
            lines
                .iter()
                .any(|l| l["params"]["update"]["status"] == "completed"),
            "{lines:?}"
        );
    }

    /// M4: a model outside the catalog is listed verbatim in the model
    /// config option so the client can show it and switch back to it,
    /// with no duplicate ids.
    #[test]
    fn model_config_option_lists_custom_model_verbatim() {
        let opt = model_config_option("my-custom-model");
        assert_eq!(opt["currentValue"], "my-custom-model");
        let options = opt["options"].as_array().unwrap();
        let ids: Vec<&str> = options
            .iter()
            .filter_map(|o| o.get("id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"my-custom-model"), "{ids:?}");
        assert!(ids.contains(&"auto"), "{ids:?}");
        let mut seen = ids.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ids.len(), "no duplicate option ids: {ids:?}");
    }

    /// M4: a catalog model is listed exactly once in the model config
    /// option (the verbatim-push guard must not fire for it).
    #[test]
    fn model_config_option_lists_catalog_model_once() {
        let first = crate::models::FREE_MODEL_CATALOG.first().unwrap();
        let opt = model_config_option(first.gateway_id);
        assert_eq!(opt["currentValue"], first.id);
        let options = opt["options"].as_array().unwrap();
        let count = options
            .iter()
            .filter(|o| o.get("id").and_then(Value::as_str) == Some(first.id))
            .count();
        assert_eq!(count, 1, "{options:?}");
    }

    // -- M5: session persistence -------------------------------------

    #[tokio::test]
    async fn session_list_empty_when_no_sessions() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let resp = find_response(&lines, 2).unwrap();
        assert!(resp.get("error").is_none(), "{lines:?}");
        assert_eq!(resp["result"]["sessions"], json!([]), "{lines:?}");
        assert!(resp["result"].get("nextCursor").is_none(), "{lines:?}");
    }

    #[tokio::test]
    async fn session_list_reports_live_session() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}})
                    .to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let resp = find_response(&lines, 2).unwrap();
        assert!(resp.get("error").is_none(), "{lines:?}");
        let sessions = resp["result"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{lines:?}");
        assert_eq!(sessions[0]["sessionId"], "s1");
        assert_eq!(
            sessions[0]["cwd"].as_str().unwrap(),
            std::env::temp_dir().to_str().unwrap()
        );
        // One session is a partial page: no nextCursor (a '*' here
        // would point at a page past the end).
        assert!(resp["result"].get("nextCursor").is_none(), "{lines:?}");
    }

    #[test]
    fn session_list_paginates_with_cursor() {
        let (state, _buf) = test_state();
        for i in 0..55 {
            fake_session(&state, &format!("s{i:02}"), 0);
        }
        let page1 = handle_session_list(&state, None, &json!({})).unwrap();
        assert_eq!(page1["sessions"].as_array().unwrap().len(), 50);
        assert_eq!(page1["nextCursor"], "50");
        let page2 = handle_session_list(&state, None, &json!({"cursor": "50"})).unwrap();
        assert_eq!(page2["sessions"].as_array().unwrap().len(), 5);
        assert!(page2.get("nextCursor").is_none());
        let mut ids: Vec<&str> = page1["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .chain(page2["sessions"].as_array().unwrap().iter())
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 55);
    }

    #[test]
    fn session_list_rejects_bad_cursor_and_relative_cwd() {
        let (state, _buf) = test_state();
        let err =
            handle_session_list(&state, None, &json!({"cursor": "not-a-number"})).unwrap_err();
        assert_eq!(err.0, -32602);
        let err = handle_session_list(&state, None, &json!({"cwd": "relative/path"})).unwrap_err();
        assert_eq!(err.0, -32602);
        // A cursor past the end is an empty last page, not an error.
        let page = handle_session_list(&state, None, &json!({"cursor": "999"})).unwrap();
        assert_eq!(page["sessions"], json!([]));
        assert!(page.get("nextCursor").is_none());
    }

    #[tokio::test]
    async fn session_close_is_idempotent() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/close", "params": {"sessionId": "s1"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/close", "params": {"sessionId": "s1"}}).to_string(),
                json!({"jsonrpc": "2.0", "id": 4, "method": "session/close", "params": {"sessionId": "ghost"}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 4).is_some()
        })
        .await;
        for id in [2, 3, 4] {
            let resp = find_response(&lines, id).unwrap();
            assert!(resp.get("error").is_none(), "{lines:?}");
            assert_eq!(resp["result"], json!({}), "{lines:?}");
        }
        assert!(lock_or_recover(&state.sessions, "acp sessions").is_empty());
    }

    #[tokio::test]
    async fn session_delete_unknown_succeeds() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/delete", "params": {"sessionId": "ghost"}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let resp = find_response(&lines, 2).unwrap();
        assert!(resp.get("error").is_none(), "{lines:?}");
        assert_eq!(resp["result"], json!({}));
    }

    #[tokio::test]
    async fn session_load_unknown_session_is_32002() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, _buf) = test_state();
        let err = handle_session_load(
            &cli,
            None,
            &json!({"sessionId": "ghost", "cwd": "/tmp"}),
            &state,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32002);
    }

    #[tokio::test]
    async fn session_resume_unknown_session_is_32002() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, _buf) = test_state();
        let err = handle_session_resume(
            &cli,
            None,
            &json!({"sessionId": "ghost", "cwd": "/tmp"}),
            &state,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32002);
    }

    #[tokio::test]
    async fn session_load_missing_session_id_is_invalid_params() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, _buf) = test_state();
        let err = handle_session_load(&cli, None, &json!({"cwd": "/tmp"}), &state)
            .await
            .unwrap_err();
        assert_eq!(err.0, -32602);
    }

    #[tokio::test]
    async fn session_load_rejects_system_prompt() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, _buf) = test_state();
        let err = handle_session_load(
            &cli,
            None,
            &json!({
                "sessionId": "ghost",
                "systemPrompt": {"mode": "append", "content": [{"type": "text", "text": "x"}]},
            }),
            &state,
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, -32602);
    }

    #[tokio::test]
    async fn session_new_rejects_system_prompt() {
        let (state, buf) = test_state();
        let run = drive_dispatcher(
            Arc::clone(&state),
            vec![
                init_line(1),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp", "systemPrompt": {"mode": "append", "content": []}}}).to_string(),
            ],
        );
        run.await.unwrap().unwrap();
        let lines = wait_for_output(&buf, Duration::from_secs(5), |ls| {
            find_response(ls, 2).is_some()
        })
        .await;
        let resp = find_response(&lines, 2).unwrap();
        assert_eq!(resp["error"]["code"], -32602, "{lines:?}");
    }

    #[test]
    fn replay_history_streams_messages_and_tool_calls() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let session = {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            sessions.get("s1").unwrap().clone()
        };
        let history = vec![
            ChatMessage::user("first question"),
            ChatMessage::assistant(
                Some("working on it".to_string()),
                Some(vec![zen_core::ToolCall {
                    id: "call-1".to_string(),
                    call_type: "function".to_string(),
                    function: zen_core::FunctionCall {
                        name: "Bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            ),
            ChatMessage::tool("call-1", "done"),
        ];
        replay_history(&session, &history);
        let lines = read_output_lines(&buf);
        assert_eq!(lines.len(), 4, "{lines:?}");
        let n1 = &lines[0];
        assert_eq!(n1["method"], "session/update");
        assert_eq!(n1["params"]["sessionId"], "s1");
        assert_eq!(
            n1["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(n1["params"]["update"]["content"]["text"], "first question");
        assert!(n1["params"]["update"]["messageId"].is_string(), "{lines:?}");
        let n2 = &lines[1];
        assert_eq!(
            n2["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(n2["params"]["update"]["content"]["text"], "working on it");
        let n3 = &lines[2];
        assert_eq!(n3["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(n3["params"]["update"]["toolCallId"], "call-1");
        assert_eq!(n3["params"]["update"]["title"], "Bash");
        let n4 = &lines[3];
        assert_eq!(n4["params"]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(n4["params"]["update"]["toolCallId"], "call-1");
        assert_eq!(n4["params"]["update"]["status"], "completed");
        assert_eq!(n4["params"]["update"]["content"][0]["text"], "done");
    }

    #[test]
    fn replay_history_closes_unanswered_tool_call() {
        let (state, buf) = test_state();
        fake_session(&state, "s1", 0);
        let session = {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            sessions.get("s1").unwrap().clone()
        };
        let history = vec![ChatMessage::assistant(
            Some(String::new()),
            Some(vec![zen_core::ToolCall {
                id: "call-9".to_string(),
                call_type: "function".to_string(),
                function: zen_core::FunctionCall {
                    name: "Read".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
        )];
        replay_history(&session, &history);
        let lines = read_output_lines(&buf);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(
            lines[1]["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(lines[1]["params"]["update"]["toolCallId"], "call-9");
        assert_eq!(lines[1]["params"]["update"]["status"], "completed");
        assert!(
            lines[1]["params"]["update"].get("content").is_none(),
            "{lines:?}"
        );
    }

    #[test]
    fn persist_turn_writes_journal_diff() {
        let (state, _buf) = test_state();
        fake_session(&state, "s1", 0);
        let session = {
            let sessions = lock_or_recover(&state.sessions, "acp sessions");
            sessions.get("s1").unwrap().clone()
        };
        let home = std::env::temp_dir().join("acp-m5-persist-test");
        let _ = std::fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir();
        let path = crate::session::ensure_session_path(&home, &cwd, "s1").unwrap();
        let writer = crate::session::TranscriptWriter::new(path.clone(), "s1", &cwd);
        *lock_or_recover(&session.transcript, "acp session transcript") = Some(writer);
        let before = vec![ChatMessage::user("q1")];
        let after = vec![
            ChatMessage::user("q1"),
            ChatMessage::assistant(Some("a1".to_string()), None),
        ];
        session.persist_turn(&before, &after);
        let history = crate::session::load_history(&path).unwrap();
        assert_eq!(history.len(), 1, "{history:?}");
        assert_eq!(history[0].role, "assistant");
        assert_eq!(history[0].text(), "a1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn restore_session_seeds_agent_history() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, _buf) = test_state();
        let home = std::env::temp_dir().join("acp-m5-restore-test");
        let _ = std::fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join("acp-m5-restore-cwd");
        let _ = std::fs::create_dir_all(&cwd);
        let session_id = "restore-1";
        let path = crate::session::ensure_session_path(&home, &cwd, session_id).unwrap();
        let mut writer = crate::session::TranscriptWriter::new(path, session_id, &cwd);
        writer
            .persist_turn(&[], &[ChatMessage::user("hello from history")])
            .unwrap();
        drop(writer);
        let params = json!({"sessionId": session_id, "cwd": cwd.to_string_lossy()});
        let (session, history) = restore_session(&cli, Some(&home), &params, &state)
            .await
            .expect("restore succeeds");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text(), "hello from history");
        let agent = session.agent.try_lock().expect("agent idle");
        assert_eq!(agent.messages().len(), 1);
        assert_eq!(agent.messages()[0].text(), "hello from history");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn session_list_cwd_filter_keeps_matching_live_session() {
        let (state, _buf) = test_state();
        fake_session(&state, "s1", 0);
        let cwd = std::env::temp_dir();
        let listed =
            handle_session_list(&state, None, &json!({"cwd": cwd.to_string_lossy()})).unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{listed:?}");
        assert_eq!(sessions[0]["sessionId"], "s1");
        let absent = std::env::temp_dir().join("nonexistent-dir-xyz");
        let listed =
            handle_session_list(&state, None, &json!({"cwd": absent.to_string_lossy()})).unwrap();
        assert_eq!(listed["sessions"], json!([]), "{listed:?}");
    }

    #[test]
    fn session_list_live_activity_refreshes_journal_time() {
        let (state, _buf) = test_state();
        let home = std::env::temp_dir().join("acp-m5-list-time-test");
        let _ = std::fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir();
        let session_id = "s1";
        let path = crate::session::ensure_session_path(&home, &cwd, session_id).unwrap();
        let mut writer = crate::session::TranscriptWriter::new(path.clone(), session_id, &cwd);
        writer
            .persist_turn(&[], &[ChatMessage::user("hello")])
            .unwrap();
        drop(writer);
        // Age the journal one hour so the live activity is clearly newer.
        let old = SystemTime::now() - Duration::from_secs(3600);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(file);
        fake_session(&state, session_id, 0);
        let listed = handle_session_list(&state, Some(&home), &json!({})).unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{listed:?}");
        let updated = sessions[0]["updatedAt"].as_str().unwrap();
        let dt = DateTime::parse_from_rfc3339(updated).unwrap();
        assert!(
            dt.with_timezone(&Utc) > Utc::now() - chrono::Duration::minutes(5),
            "updatedAt should be the live activity, not the aged journal: {updated}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn session_list_no_next_cursor_on_exact_page() {
        let (state, _buf) = test_state();
        for i in 0..50 {
            fake_session(&state, &format!("s{i:02}"), 0);
        }
        let page = handle_session_list(&state, None, &json!({})).unwrap();
        assert_eq!(page["sessions"].as_array().unwrap().len(), 50);
        // Exactly one full page: there is no next page, so nextCursor
        // must be absent (a "<=" here would point at an empty page).
        assert!(page.get("nextCursor").is_none(), "{page:?}");
    }

    #[tokio::test]
    async fn session_load_active_session_replays_journal() {
        let cli = Cli::try_parse_from(["zen"]).unwrap();
        let (state, buf) = test_state();
        let home = std::env::temp_dir().join("acp-m5-load-active-test");
        let _ = std::fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir();
        let session_id = "s1";
        let path = crate::session::ensure_session_path(&home, &cwd, session_id).unwrap();
        let mut writer = crate::session::TranscriptWriter::new(path, session_id, &cwd);
        writer
            .persist_turn(&[], &[ChatMessage::user("journal hello")])
            .unwrap();
        drop(writer);
        fake_session(&state, session_id, 0);
        let params = json!({"sessionId": session_id, "cwd": cwd.to_string_lossy()});
        let result = handle_session_load(&cli, Some(&home), &params, &state).await;
        assert!(result.is_ok(), "{result:?}");
        let lines = read_output_lines(&buf);
        // The replay comes from the journal, not the live agent (whose
        // history is empty): one user chunk with the journal text.
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            lines[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            lines[0]["params"]["update"]["content"]["text"],
            "journal hello"
        );
        let _ = std::fs::remove_dir_all(&home);
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
            pending_tools: Arc::new(Mutex::new(HashSet::new())),
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
    fn validate_prompt_renders_resource_links() {
        let params = json!({
            "prompt": [
                { "type": "resource_link", "uri": "file:///x.md" },
                { "type": "text", "text": "hi" },
            ]
        });
        let prompt = validate_prompt(&params).expect("resource_link is baseline");
        assert_eq!(prompt.text, "[resource: file:///x.md]\nhi");
        // The v1 field is `prompt`: a params value carrying the old
        // M1 `content` key has no prompt at all (M3 fix).
        assert!(
            validate_prompt(&json!({ "content": [{ "type": "text", "text": "hi" }] })).is_err()
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
        let state = Arc::new(AcpServerState::new(out));
        let cwd = std::env::temp_dir();
        let err =
            match handle_session_new(&cli, &cwd, None, &json!({"cwd": "relative/path"}), &state)
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
