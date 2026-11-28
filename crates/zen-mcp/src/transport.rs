use anyhow::anyhow;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ContentBlock, PaginatedRequestParams, Tool,
};
use rmcp::service::{Peer, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::McpServerConfig;

/// Bounds for `tools/list` pagination in `Session::list_all_tools` (#377).
/// A server returning cursors forever (or a never-ending stream of them)
/// used to hang session startup and grow memory without bound. Each page
/// still has its own per-request timeout, and the manager wraps the whole
/// handshake in `timeout_ms`, so total time at startup was already bounded
/// there; these caps close the remaining holes (infinite pages, unbounded
/// memory) inside the listing itself.
pub(crate) const MAX_LIST_PAGES: usize = 100;
pub(crate) const MAX_LIST_TOOLS: usize = 10_000;

/// One live rmcp client session. rmcp performs the
/// `initialize` / `notifications/initialized` handshake inside `serve_client`.
pub(crate) struct Session {
    /// Server name, for error messages (#377: a pagination-cap error must
    /// name the server so it can be told apart in the manager's failed list).
    name: String,
    /// Kept alive for the session's lifetime. Dropping it cancels the service,
    /// and rmcp's DropGuard kills the stdio child process.
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
    peer: Peer<RoleClient>,
    timeout_ms: u64,
    /// PID of the stdio child process (for process group kill on Unix)
    child_pid: Mutex<Option<u32>>,
}

/// Whether a failed session call means the session itself is dead.
/// Only a transport failure evicts the cached session: a call the live
/// server rejected (bad arguments, a tool that needs client-side input,
/// ...) must not kill a healthy session. Timeouts count as transport
/// failures: a peer that stops answering within its deadline is
/// indistinguishable from a dead one, and reconnecting is the only
/// self-healing path.
#[derive(Debug)]
enum SessionError {
    Transport(anyhow::Error),
    Call(anyhow::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Transport(e) | SessionError::Call(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl Session {
    fn deadline(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }

    /// Cheap liveness probe: the rmcp service must still be held, and a
    /// stdio child must still be alive. A missing service means the
    /// session was shut down; an exited stdio child means the server is
    /// gone. (On Unix the child is checked with waitpid(WNOHANG), which
    /// reaps a zombie: kill(pid, 0) cannot see zombies, so the old probe
    /// called a dead-but-unreaped child alive and the single retry was
    /// wasted on the corpse. A child already reaped elsewhere reports
    /// dead too (waitpid returns -1), so the session is reconnected
    /// instead of retried on the corpse; the transport-error eviction in
    /// `Transport::list_all_tools_resilient` /
    /// `Transport::call_tool_resilient` covers any case the probe still
    /// misses. HTTP sessions have no child process, so for them only the
    /// service check applies and healing comes from eviction on
    /// transport error.)
    fn is_alive(&self) -> bool {
        match self.service.lock() {
            Ok(guard) => {
                if guard.is_none() {
                    return false;
                }
            }
            Err(_) => return false,
        }
        let pid = match self.child_pid.lock() {
            Ok(guard) => *guard,
            Err(_) => return false,
        };
        let Some(pid) = pid else { return true };
        #[cfg(unix)]
        {
            // waitpid(WNOHANG) reaps a zombie child, which kill(pid, 0)
            // cannot see: a dead stdio server that has exited but not
            // been waited on yet still answers kill(pid, 0), so the old
            // probe called it alive and the single retry was wasted on
            // the corpse. The child is ours (spawned in
            // `connect_stdio`), and reaping it here is safe: tokio's
            // orphan queue drops an already-reaped child cleanly on
            // ECHILD. A return of 0 means the child is still running;
            // any other outcome (the pid: it exited; -1: already reaped
            // elsewhere, or an error) means the session is dead and must
            // be reconnected. waitpid subsumes the old kill(pid, 0)
            // check: for our own child a return of 0 already proves it
            // is running.
            // SAFETY: waitpid(2) with WNOHANG on our own child; plain
            // integers only, no memory-safety preconditions.
            let mut status = 0;
            let ret = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
            ret == 0
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            true
        }
    }

    async fn list_all_tools(&self) -> Result<Vec<Tool>, SessionError> {
        let mut all = Vec::new();
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            pages += 1;
            if pages > MAX_LIST_PAGES {
                return Err(SessionError::Call(anyhow!(
                    "MCP server '{}': tools/list exceeded {MAX_LIST_PAGES} pages; refusing to page further",
                    self.name
                )));
            }
            let params = cursor
                .clone()
                .map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
            let result = tokio::time::timeout(self.deadline(), self.peer.list_tools(params))
                .await
                .map_err(|_| {
                    SessionError::Transport(anyhow!(
                        "tools/list timed out after {} ms",
                        self.timeout_ms
                    ))
                })?
                .map_err(|e| SessionError::Transport(anyhow!("tools/list failed: {e}")))?;
            all.extend(result.tools);
            if all.len() > MAX_LIST_TOOLS {
                return Err(SessionError::Call(anyhow!(
                    "MCP server '{}': tools/list exceeded {MAX_LIST_TOOLS} tools; refusing to collect further",
                    self.name
                )));
            }
            match result.next_cursor {
                Some(next) => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(SessionError::Call(anyhow!(
                            "MCP server '{}': tools/list cursor '{next}' repeated; refusing to page further",
                            self.name
                        )));
                    }
                    cursor = Some(next);
                }
                None => break,
            }
        }
        Ok(all)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, SessionError> {
        let arguments = match args {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => {
                return Err(SessionError::Call(anyhow!(
                    "arguments must be a JSON object, got {other}"
                )));
            }
        };
        let mut call = CallToolRequestParams::new(name.to_string());
        if let Some(map) = arguments {
            call = call.with_arguments(map);
        }
        let result = tokio::time::timeout(self.deadline(), self.peer.call_tool_once(call))
            .await
            .map_err(|_| {
                SessionError::Transport(anyhow!(
                    "tools/call timed out after {} ms",
                    self.timeout_ms
                ))
            })?
            .map_err(|e| SessionError::Transport(anyhow!("tools/call failed: {e}")))?;
        match result {
            CallToolResponse::Complete(done) => {
                let parts: Vec<Value> = done.content.iter().map(content_block_to_value).collect();
                let mut out = json!({ "content": parts });
                if let Some(is_error) = done.is_error {
                    out["isError"] = json!(is_error);
                }
                if let Some(structured) = done.structured_content {
                    out["structuredContent"] = structured;
                }
                Ok(out)
            }
            CallToolResponse::InputRequired(_) => Err(SessionError::Call(anyhow!(
                "tool '{name}' requires client-side input, which this client does not provide"
            ))),
            CallToolResponse::Task(_) => Err(SessionError::Call(anyhow!(
                "tool '{name}' returned a task result"
            ))),
            // `CallToolResponse` is #[non_exhaustive].
            _ => Err(SessionError::Call(anyhow!(
                "tool '{name}' returned an unsupported response"
            ))),
        }
    }

    fn shutdown(&self) {
        // Kill process tree first (Unix: process group, Windows: tree)
        if let Ok(pid_guard) = self.child_pid.lock()
            && let Some(pid) = *pid_guard
        {
            #[cfg(unix)]
            {
                // Signal the process group directly instead of shelling out to
                // `kill`: minimal images (rust:slim, distroless) ship no
                // /bin/kill binary, so Command::new("kill") fails silently and
                // the group survives. Prove it with
                // transport_shutdown_kills_the_whole_process_group.
                // SAFETY: kill(2) with a negative pid signals the group led by
                // `pid`. Plain integers, no memory-safety preconditions.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        // Then drop the rmcp service (kills the direct child via DropGuard)
        if let Ok(mut guard) = self.service.lock() {
            guard.take();
        }
    }
}

pub(crate) fn content_block_to_value(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text(text) => json!({ "type": "text", "text": text.text }),
        ContentBlock::Image(image) => {
            json!({ "type": "image", "data": image.data, "mimeType": image.mime_type })
        }
        ContentBlock::Audio(audio) => {
            json!({ "type": "audio", "data": audio.data, "mimeType": audio.mime_type })
        }
        ContentBlock::ResourceLink(link) => {
            json!({ "type": "resource_link", "uri": link.uri, "name": link.name })
        }
        ContentBlock::Resource(resource) => json!({
            "type": "resource",
            "resource": serde_json::to_value(&resource.resource).unwrap_or(Value::Null),
        }),
        // `ContentBlock` is #[non_exhaustive]: future variants render as a
        // placeholder so an rmcp upgrade cannot break the build silently.
        _ => json!({ "type": "unknown" }),
    }
}

async fn connect_stdio(
    name: &str,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    api_key_env: Option<&str>,
    timeout_ms: u64,
) -> anyhow::Result<Session> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    if !env.is_empty() {
        cmd.envs(env);
    }
    // Never inherit provider credentials from the parent process: strip
    // the two defaults, the configured key env, and every known provider
    // key env, so a third-party MCP server cannot see any of them (#378).
    for name in zen_core::all_api_key_env_names(api_key_env) {
        cmd.env_remove(name);
    }
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    // Use rmcp's TokioChildProcess builder to capture PID
    let transport = TokioChildProcess::builder(cmd)
        .spawn()
        .map_err(|e| anyhow!("failed to spawn '{command}': {e}"))?;
    let child_pid = transport.0.id();

    let service = tokio::time::timeout(
        Duration::from_millis(timeout_ms.max(1)),
        rmcp::serve_client((), transport.0),
    )
    .await
    .map_err(|_| anyhow!("MCP stdio handshake timed out after {timeout_ms} ms"))?
    .map_err(|e| anyhow!("MCP stdio handshake failed: {e}"))?;
    let peer = service.peer().clone();
    Ok(Session {
        name: name.to_string(),
        service: Mutex::new(Some(service)),
        peer,
        timeout_ms,
        child_pid: Mutex::new(child_pid),
    })
}

async fn connect_http(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    timeout_ms: u64,
) -> anyhow::Result<Session> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if !headers.is_empty() {
        let mut map = HashMap::new();
        for (key, value) in headers {
            let name = key
                .parse::<http::HeaderName>()
                .map_err(|e| anyhow!("invalid MCP header name '{key}': {e}"))?;
            let value = value
                .parse::<http::HeaderValue>()
                .map_err(|e| anyhow!("invalid value for MCP header '{key}': {e}"))?;
            map.insert(name, value);
        }
        config = config.custom_headers(map);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = tokio::time::timeout(
        Duration::from_millis(timeout_ms.max(1)),
        rmcp::serve_client((), transport),
    )
    .await
    .map_err(|_| anyhow!("MCP HTTP handshake timed out after {timeout_ms} ms"))?
    .map_err(|e| anyhow!("MCP HTTP handshake failed: {e}"))?;
    let peer = service.peer().clone();
    Ok(Session {
        name: name.to_string(),
        service: Mutex::new(Some(service)),
        peer,
        timeout_ms,
        child_pid: Mutex::new(None),
    })
}

/// A configured MCP server plus its lazily-created session. Connection work
/// happens in `session()`, so `from_config` stays synchronous and the manager
/// can wrap the handshake in its own timeout.
pub(crate) enum Transport {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        /// Configured provider key env name (model's `apiKeyEnv`), stripped
        /// from the child environment in `connect_stdio` (#378).
        api_key_env: Option<String>,
        timeout_ms: u64,
        session: tokio::sync::Mutex<Option<Arc<Session>>>,
        /// Set by `kill()` (#403). Once set, `session()` refuses to
        /// connect and shuts down any session that finishes
        /// connecting afterwards. Permanent: no production path
        /// reuses a transport after `kill()` (the manager drops it),
        /// so the flag never needs clearing.
        killed: AtomicBool,
    },
    Http {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: u64,
        session: tokio::sync::Mutex<Option<Arc<Session>>>,
        /// Set by `kill()` (#403); see the Stdio variant.
        killed: AtomicBool,
    },
}

impl Transport {
    pub(crate) fn from_config(
        name: &str,
        cfg: &McpServerConfig,
        timeout_ms: u64,
        api_key_env: Option<&str>,
    ) -> anyhow::Result<Self> {
        let timeout_ms = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        if cfg.is_http() {
            let url = cfg
                .url
                .clone()
                .ok_or_else(|| anyhow!("http server has no url"))?;
            Ok(Transport::Http {
                name: name.to_string(),
                url,
                headers: cfg.headers.clone(),
                timeout_ms,
                session: tokio::sync::Mutex::new(None),
                killed: AtomicBool::new(false),
            })
        } else {
            let command = cfg
                .command
                .clone()
                .ok_or_else(|| anyhow!("stdio server has no command"))?;
            Ok(Transport::Stdio {
                name: name.to_string(),
                command,
                args: cfg.args.clone(),
                env: cfg.env.clone(),
                api_key_env: api_key_env.map(str::to_string),
                timeout_ms,
                session: tokio::sync::Mutex::new(None),
                killed: AtomicBool::new(false),
            })
        }
    }

    async fn session(&self) -> anyhow::Result<Arc<Session>> {
        // One flow for both variants (#403): the session lock is held
        // across the connect, so `kill()` cannot take it mid-connect.
        // The `killed` flag is checked twice - before connecting (a kill
        // that landed earlier refuses outright) and after the connect
        // finishes (a kill that landed mid-connect shuts the fresh
        // session down instead of caching it).
        let mut guard = match self {
            Transport::Stdio { session, .. } | Transport::Http { session, .. } => {
                session.lock().await
            }
        };
        if self.is_killed() {
            return Err(anyhow!(
                "MCP server '{}' was killed; refusing to connect",
                self.name()
            ));
        }
        if let Some(existing) = guard.as_ref() {
            if existing.is_alive() {
                return Ok(existing.clone());
            }
            // Dead session: drop it (killing the child process group,
            // if any) so the fresh connect below starts a new server
            // instead of reusing the corpse.
            existing.shutdown();
            *guard = None;
        }
        let fresh = Arc::new(match self {
            Transport::Stdio {
                name,
                command,
                args,
                env,
                api_key_env,
                timeout_ms,
                ..
            } => {
                connect_stdio(
                    name,
                    command,
                    args,
                    env,
                    api_key_env.as_deref(),
                    *timeout_ms,
                )
                .await?
            }
            Transport::Http {
                name,
                url,
                headers,
                timeout_ms,
                ..
            } => connect_http(name, url, headers, *timeout_ms).await?,
        });
        if self.is_killed() {
            // Killed mid-connect: shut the fresh session down (kills
            // its child process) instead of caching a session nobody
            // owns any more.
            fresh.shutdown();
            return Err(anyhow!(
                "MCP server '{}' was killed during connect",
                self.name()
            ));
        }
        *guard = Some(fresh.clone());
        Ok(fresh)
    }

    /// Drop the cached session, but only if it is still the session the
    /// caller just failed on: another task may already have reconnected,
    /// and its fresh session must not be evicted by a stale failure.
    async fn evict(&self, dead: &Arc<Session>) {
        let mut guard = match self {
            Transport::Stdio { session, .. } | Transport::Http { session, .. } => {
                session.lock().await
            }
        };
        if let Some(existing) = guard.as_ref()
            && Arc::ptr_eq(existing, dead)
        {
            existing.shutdown();
            *guard = None;
        }
    }

    /// `tools/list` with self-healing: a transport error evicts the dead
    /// cached session and the call is retried once on a fresh connection.
    /// A call error is returned as-is, and a reconnect (or retry) that
    /// also fails is spelled out so the caller can tell a dead server
    /// from a failed call.
    async fn list_all_tools_resilient(&self) -> anyhow::Result<Vec<Tool>> {
        let session = self.session().await?;
        match session.list_all_tools().await {
            Ok(tools) => Ok(tools),
            Err(SessionError::Call(e)) => Err(e),
            Err(SessionError::Transport(first)) => {
                let first = first.to_string();
                self.evict(&session).await;
                let fresh = self
                    .session()
                    .await
                    .map_err(|e| anyhow!("MCP session died ({first}); reconnect failed: {e}"))?;
                fresh.list_all_tools().await.map_err(|retry| match retry {
                    SessionError::Call(e) => e,
                    SessionError::Transport(e) => {
                        anyhow!("MCP session died ({first}); reconnected session also failed: {e}")
                    }
                })
            }
        }
    }

    /// `tools/call` with the same self-healing as
    /// `list_all_tools_resilient`: evict + reconnect once on a transport
    /// error, return a call error untouched.
    async fn call_tool_resilient(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let session = self.session().await?;
        let retry_args = args.clone();
        match session.call_tool(name, args).await {
            Ok(out) => Ok(out),
            Err(SessionError::Call(e)) => Err(e),
            Err(SessionError::Transport(first)) => {
                let first = first.to_string();
                self.evict(&session).await;
                let fresh = self
                    .session()
                    .await
                    .map_err(|e| anyhow!("MCP session died ({first}); reconnect failed: {e}"))?;
                fresh
                    .call_tool(name, retry_args)
                    .await
                    .map_err(|retry| match retry {
                        SessionError::Call(e) => e,
                        SessionError::Transport(e) => anyhow!(
                            "MCP session died ({first}); reconnected session also failed: {e}"
                        ),
                    })
            }
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match method {
            "tools/list" => {
                let tools = self.list_all_tools_resilient().await?;
                let encoded: Vec<Value> = tools.iter().map(tool_to_value).collect();
                Ok(json!({ "tools": encoded }))
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("tools/call missing 'name'"))?
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                self.call_tool_resilient(&name, args).await
            }
            other => Err(anyhow!("unsupported MCP method '{other}'")),
        }
    }

    /// The timeout this transport resolved at construction (0 means the
    /// documented 30 s default). Test-only: production code never reads it
    /// back, it only passes the value to the session.
    #[cfg(test)]
    pub(crate) fn resolved_timeout_ms(&self) -> u64 {
        match self {
            Transport::Stdio { timeout_ms, .. } | Transport::Http { timeout_ms, .. } => *timeout_ms,
        }
    }

    pub(crate) fn kill(&self) {
        // Flag first (#403): `session()` may be holding the session
        // lock mid-connect, so the `try_lock` below can fail and
        // this kill would otherwise be a silent no-op. Setting the
        // flag before touching the lock makes the in-flight connect
        // shut its fresh session down when it finishes, and makes
        // any later `session()` refuse to spawn a replacement. The
        // flag is permanent: no production path reuses a transport
        // after `kill()` (the manager drops it), so it never needs
        // clearing.
        match self {
            Transport::Stdio { killed, .. } | Transport::Http { killed, .. } => {
                killed.store(true, Ordering::SeqCst);
            }
        }
        // Called from the synchronous manager shutdown path, so `try_lock`
        // keeps it non-blocking: a session mid-request is dropped with the
        // runtime instead.
        let session_guard = match self {
            Transport::Stdio { session, .. } | Transport::Http { session, .. } => {
                session.try_lock()
            }
        };
        if let Ok(mut guard) = session_guard {
            if let Some(existing) = guard.as_ref() {
                existing.shutdown();
            }
            *guard = None;
        }
    }

    /// Server name, for error messages.
    fn name(&self) -> &str {
        match self {
            Transport::Stdio { name, .. } | Transport::Http { name, .. } => name,
        }
    }

    /// Whether `kill()` has run (#403). Read by `session()` to refuse
    /// connects on a killed transport.
    fn is_killed(&self) -> bool {
        match self {
            Transport::Stdio { killed, .. } | Transport::Http { killed, .. } => {
                killed.load(Ordering::SeqCst)
            }
        }
    }
}

pub(crate) fn tool_to_value(tool: &Tool) -> Value {
    json!({
        "name": tool.name.as_ref(),
        "description": tool.description.as_deref().unwrap_or(""),
        "inputSchema": serde_json::to_value(&*tool.input_schema).unwrap_or(Value::Null),
    })
}

/// Connect (rmcp handles initialize + notifications/initialized) and list tools.
pub(crate) async fn handshake(transport: &Transport) -> anyhow::Result<Vec<RemoteTool>> {
    list_tools(transport).await
}

pub(crate) async fn list_tools(transport: &Transport) -> anyhow::Result<Vec<RemoteTool>> {
    // Same self-healing path as Transport::request: a dead cached
    // session is evicted and the list retried on a fresh connection.
    let tools = transport.list_all_tools_resilient().await?;
    Ok(tools
        .iter()
        .filter_map(|tool| {
            let name = tool.name.to_string();
            if name.is_empty() {
                return None;
            }
            Some(RemoteTool {
                name,
                description: tool.description.as_deref().unwrap_or("").to_string(),
                schema: serde_json::to_value(&*tool.input_schema)
                    .unwrap_or_else(|_| json!({ "type": "object" })),
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_error_display_forwards_inner_message() {
        // Mutant killer for the Display impl: the resilient helpers
        // return the inner anyhow error, so Display is only reached
        // when a SessionError itself is formatted; pin that it
        // forwards the inner message for both variants.
        assert_eq!(SessionError::Transport(anyhow!("boom")).to_string(), "boom");
        assert_eq!(
            SessionError::Call(anyhow!("bad args")).to_string(),
            "bad args"
        );
    }

    #[cfg(target_os = "linux")]
    fn test_python_exe() -> Option<String> {
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

    #[cfg(target_os = "linux")]
    fn test_tmpdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("zen_mcp_403_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // Linux-only: polls /proc/<pid> to observe the child, and macOS has
    // no /proc.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kill_during_slow_connect_shuts_down_fresh_session() {
        // #403: `kill()` runs while `session()` is mid-connect (the mock
        // waits 5 s before answering `initialize`), so `kill()` cannot
        // take the session lock. The fresh session must not survive:
        // `session()` must fail, the child must die, and nothing may be
        // cached for a later call.
        let Some(py) = test_python_exe() else {
            return;
        };
        let dir = test_tmpdir("t_403_race");
        let log = dir.join("methods.log");
        let pid_file = dir.join("methods.log.pid");
        let cfg = McpServerConfig {
            kind: None,
            command: Some(py),
            args: vec![
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/mcp_mock.py")
                    .to_string_lossy()
                    .into_owned(),
                "slow_init".to_string(),
                log.to_string_lossy().into_owned(),
            ],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
        };
        let tr = Arc::new(Transport::from_config("fake", &cfg, 30_000, None).unwrap());

        // Killer task: wait for the child to spawn (its pid file proves
        // the connect is in flight), then kill(). It runs on its own task
        // so the session future stays on this task (no Send requirement
        // on the session itself).
        let killer_tr = tr.clone();
        let killer_pid_file = pid_file.clone();
        let killer = tokio::spawn(async move {
            let mut pid = String::new();
            for _ in 0..50 {
                if let Ok(text) = std::fs::read_to_string(&killer_pid_file)
                    && !text.trim().is_empty()
                {
                    pid = text.trim().to_string();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(!pid.is_empty(), "mock never spawned");
            let proc_path = format!("/proc/{pid}");
            assert!(
                std::path::Path::new(&proc_path).exists(),
                "child {pid} should be alive mid-connect"
            );
            killer_tr.kill();
            pid
        });

        let res = tokio::time::timeout(Duration::from_secs(30), tr.session())
            .await
            .expect("session() hung");
        let pid = killer.await.expect("killer task panicked");
        let proc_path = format!("/proc/{pid}");
        match res {
            Ok(_) => panic!("session() returned a live session after kill()"),
            Err(e) => assert!(
                e.to_string().contains("killed"),
                "session() after kill() should report the kill, got: {e}"
            ),
        }

        let mut dead = false;
        for _ in 0..100 {
            if !std::path::Path::new(&proc_path).exists() {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(dead, "child {pid} still alive after kill-during-connect");

        // Nothing cached: a later session() must fail fast without
        // spawning a second child (the pid file still names the first).
        assert!(
            tr.session().await.is_err(),
            "session() after kill() must refuse to connect"
        );
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap().trim(),
            pid,
            "a second child must not have spawned after kill()"
        );

        tr.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_stdio_config(command: &str) -> McpServerConfig {
        McpServerConfig {
            kind: None,
            command: Some(command.to_string()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn kill_while_session_lock_held_sets_flag_and_blocks_connect() {
        // #403: `kill()` cannot take the session lock while another task
        // holds it, but the flag must still land - and a later `session()`
        // must refuse instead of spawning.
        let cfg = test_stdio_config("true");
        let tr = Transport::from_config("fake", &cfg, 30_000, None).unwrap();
        assert!(!tr.is_killed());
        let guard = match &tr {
            Transport::Stdio { session, .. } | Transport::Http { session, .. } => {
                session.lock().await
            }
        };
        tr.kill();
        assert!(tr.is_killed());
        drop(guard);
        let err = match tr.session().await {
            Ok(_) => panic!("session() after kill() must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("killed"),
            "session() after kill() should report the kill, got: {err}"
        );
        assert!(
            err.to_string().contains("fake"),
            "killed error should name the server, got: {err}"
        );
    }

    #[tokio::test]
    async fn kill_on_idle_transport_blocks_later_connect() {
        // #403: killing an idle transport must also refuse any later
        // connect - the flag, not the lock, is what blocks it.
        let cfg = test_stdio_config("true");
        let tr = Transport::from_config("fake", &cfg, 30_000, None).unwrap();
        assert!(!tr.is_killed());
        tr.kill();
        assert!(tr.is_killed());
        let err = match tr.session().await {
            Ok(_) => panic!("session() after kill() must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("killed"),
            "session() after kill() should report the kill, got: {err}"
        );
        assert!(
            err.to_string().contains("fake"),
            "killed error should name the server, got: {err}"
        );
    }
}
