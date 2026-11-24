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

impl Session {
    fn deadline(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }

    async fn list_all_tools(&self) -> anyhow::Result<Vec<Tool>> {
        let mut all = Vec::new();
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            pages += 1;
            if pages > MAX_LIST_PAGES {
                return Err(anyhow!(
                    "MCP server '{}': tools/list exceeded {MAX_LIST_PAGES} pages; refusing to page further",
                    self.name
                ));
            }
            let params = cursor
                .clone()
                .map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
            let result = tokio::time::timeout(self.deadline(), self.peer.list_tools(params))
                .await
                .map_err(|_| anyhow!("tools/list timed out after {} ms", self.timeout_ms))?
                .map_err(|e| anyhow!("tools/list failed: {e}"))?;
            all.extend(result.tools);
            if all.len() > MAX_LIST_TOOLS {
                return Err(anyhow!(
                    "MCP server '{}': tools/list exceeded {MAX_LIST_TOOLS} tools; refusing to collect further",
                    self.name
                ));
            }
            match result.next_cursor {
                Some(next) => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(anyhow!(
                            "MCP server '{}': tools/list cursor '{next}' repeated; refusing to page further",
                            self.name
                        ));
                    }
                    cursor = Some(next);
                }
                None => break,
            }
        }
        Ok(all)
    }

    async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let arguments = match args {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => return Err(anyhow!("arguments must be a JSON object, got {other}")),
        };
        let mut call = CallToolRequestParams::new(name.to_string());
        if let Some(map) = arguments {
            call = call.with_arguments(map);
        }
        let result = tokio::time::timeout(self.deadline(), self.peer.call_tool_once(call))
            .await
            .map_err(|_| anyhow!("tools/call timed out after {} ms", self.timeout_ms))?
            .map_err(|e| anyhow!("tools/call failed: {e}"))?;
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
            CallToolResponse::InputRequired(_) => Err(anyhow!(
                "tool '{name}' requires client-side input, which this client does not provide"
            )),
            CallToolResponse::Task(_) => Err(anyhow!("tool '{name}' returned a task result")),
            // `CallToolResponse` is #[non_exhaustive].
            _ => Err(anyhow!("tool '{name}' returned an unsupported response")),
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
    },
    Http {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: u64,
        session: tokio::sync::Mutex<Option<Arc<Session>>>,
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
            })
        }
    }

    async fn session(&self) -> anyhow::Result<Arc<Session>> {
        match self {
            Transport::Stdio {
                name,
                command,
                args,
                env,
                api_key_env,
                timeout_ms,
                session,
            } => {
                let mut guard = session.lock().await;
                if let Some(existing) = guard.as_ref() {
                    return Ok(existing.clone());
                }
                let fresh = Arc::new(
                    connect_stdio(
                        name,
                        command,
                        args,
                        env,
                        api_key_env.as_deref(),
                        *timeout_ms,
                    )
                    .await?,
                );
                *guard = Some(fresh.clone());
                Ok(fresh)
            }
            Transport::Http {
                name,
                url,
                headers,
                timeout_ms,
                session,
            } => {
                let mut guard = session.lock().await;
                if let Some(existing) = guard.as_ref() {
                    return Ok(existing.clone());
                }
                let fresh = Arc::new(connect_http(name, url, headers, *timeout_ms).await?);
                *guard = Some(fresh.clone());
                Ok(fresh)
            }
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let session = self.session().await?;
        match method {
            "tools/list" => {
                let tools = session.list_all_tools().await?;
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
                session.call_tool(&name, args).await
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
    let session = transport.session().await?;
    let tools = session.list_all_tools().await?;
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
