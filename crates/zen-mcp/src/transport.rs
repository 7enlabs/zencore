use anyhow::anyhow;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ContentBlock, PaginatedRequestParams, Tool,
};
use rmcp::service::{Peer, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::McpServerConfig;

/// One live rmcp client session. rmcp performs the
/// `initialize` / `notifications/initialized` handshake inside `serve_client`.
pub(crate) struct Session {
    /// Kept alive for the session's lifetime. Dropping it cancels the service,
    /// and rmcp's DropGuard kills the stdio child process.
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
    peer: Peer<RoleClient>,
    timeout_ms: u64,
    /// PID of the child process (for Unix process group kill)
    child_pid: Mutex<Option<u32>>,
}

impl Session {
    fn deadline(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }

    async fn list_all_tools(&self) -> anyhow::Result<Vec<Tool>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor
                .clone()
                .map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
            let result = tokio::time::timeout(self.deadline(), self.peer.list_tools(params))
                .await
                .map_err(|_| anyhow!("tools/list timed out after {} ms", self.timeout_ms))?
                .map_err(|e| anyhow!("tools/list failed: {e}"))?;
            all.extend(result.tools);
            match result.next_cursor {
                Some(next) => cursor = Some(next),
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
        // Kill the Unix process group first to ensure all child processes are terminated
        kill_process_group(self.child_pid.lock().ok().and_then(|mut g| g.take()));
        
        if let Ok(mut guard) = self.service.lock() {
            guard.take();
        }
    }
}

/// Kill the entire Unix process group to prevent orphan processes.
/// On Windows, kills the single PID.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", pid)])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
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

fn content_block_to_value(block: &ContentBlock) -> Value {
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
        ContentBlock::Resource(resource) => {
            // EmbeddedResource wraps ResourceContents in `resource` field.
            // We must extract the inner ResourceContents to match the shape
            // expected by render_result: { "type": "resource", "resource": { "uri": ..., ... } }
            json!({
                "type": "resource",
                "resource": resource.resource,
            })
        }
        // `ContentBlock` is #[non_exhaustive]: future variants render as a
        // placeholder so an rmcp upgrade cannot break the build silently.
        _ => json!({ "type": "unknown" }),
    }
}

async fn connect_stdio(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    timeout_ms: u64,
) -> anyhow::Result<Session> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    if !env.is_empty() {
        cmd.envs(env);
    }
    // Never inherit provider credentials from the parent process.
    for name in zen_core::api_key_env_names(None) {
        cmd.env_remove(name);
    }
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    // TokioChildProcess::new() returns the transport, and .id() exposes the
    // child PID. Capture the PID *before* the transport is moved into
    // serve_client, otherwise the process group is unreachable and a killed
    // session leaves the grandchildren running.
    let transport = TokioChildProcess::new(cmd)
        .map_err(|e| anyhow!("failed to spawn '{command}': {e}"))?;
    let child_pid = transport.id();
    let service = tokio::time::timeout(
        Duration::from_millis(timeout_ms.max(1)),
        rmcp::serve_client((), transport),
    )
    .await
    .map_err(|_| anyhow!("MCP stdio handshake timed out after {timeout_ms} ms"))?
    .map_err(|e| anyhow!("MCP stdio handshake failed: {e}"))?;
    let peer = service.peer().clone();
    Ok(Session {
        service: Mutex::new(Some(service)),
        peer,
        timeout_ms,
        child_pid: Mutex::new(child_pid),
    })
}

async fn connect_http(
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
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_ms: u64,
        session: tokio::sync::Mutex<Option<Arc<Session>>>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: u64,
        session: tokio::sync::Mutex<Option<Arc<Session>>>,
    },
}

impl Transport {
    pub(crate) fn from_config(cfg: &McpServerConfig, timeout_ms: u64) -> anyhow::Result<Self> {
        let timeout_ms = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        if cfg.is_http() {
            let url = cfg
                .url
                .clone()
                .ok_or_else(|| anyhow!("http server has no url"))?;
            Ok(Transport::Http {
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
                command,
                args: cfg.args.clone(),
                env: cfg.env.clone(),
                timeout_ms,
                session: tokio::sync::Mutex::new(None),
            })
        }
    }

    async fn session(&self) -> anyhow::Result<Arc<Session>> {
        match self {
            Transport::Stdio {
                command,
                args,
                env,
                timeout_ms,
                session,
            } => {
                let mut guard = session.lock().await;
                if let Some(existing) = guard.as_ref() {
                    return Ok(existing.clone());
                }
                let fresh = Arc::new(connect_stdio(command, args, env, *timeout_ms).await?);
                *guard = Some(fresh.clone());
                Ok(fresh)
            }
            Transport::Http {
                url,
                headers,
                timeout_ms,
                session,
            } => {
                let mut guard = session.lock().await;
                if let Some(existing) = guard.as_ref() {
                    return Ok(existing.clone());
                }
                let fresh = Arc::new(connect_http(url, headers, *timeout_ms).await?);
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

fn tool_to_value(tool: &Tool) -> Value {
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