use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rmcp::{
    Client, ServerInfo, Service,
    model::{CallToolResult, ClientCapabilities, ClientInfo, InitializeRequestParam, ListToolsRequestParam},
    transport::{stdio, sse_client::SseClientTransport, TokioChildProcess},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::config::McpServerConfig;

const PROTOCOL_VERSION: &str = "2025-06-18";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

#[derive(Clone)]
pub(crate) struct McpClient {
    client: Client<rmcp::transport::TokioIo<tokio::process::ChildStdin, tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    pending: Pending,
}

impl McpClient {
    pub(crate) async fn spawn(cfg: &McpServerConfig, timeout_ms: u64) -> Result<Arc<Self>> {
        let command = cfg
            .command
            .clone()
            .ok_or_else(|| anyhow::anyhow!("stdio server has no command"))?;

        let mut cmd = Command::new(&command);
        cmd.args(&cfg.args);
        if !cfg.env.is_empty() {
            cmd.envs(&cfg.env);
        }

        for name in zen_core::api_key_env_names(None) {
            cmd.env_remove(name);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {}", command, e))?;

        let transport = stdio::StdioTransport::new(child);
        let client = Client::new(transport).await
            .map_err(|e| anyhow::anyhow!("failed to create MCP client: {}", e))?;

        let init_params = InitializeRequestParam {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "ZenCode".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        client.initialize(init_params).await
            .map_err(|e| anyhow::anyhow!("initialize failed: {}", e))?;

        Ok(Arc::new(Self {
            client,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "tools/list" => {
                let result = self.client.list_tools(ListToolsRequestParam::default()).await
                    .map_err(|e| anyhow::anyhow!("tools/list failed: {}", e))?;
                let tools: Vec<Value> = result.tools.into_iter().map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema
                    })
                }).collect();
                Ok(json!({ "tools": tools }))
            }
            "initialize" => {
                Ok(json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "serverInfo": { "name": "mcp", "version": "1.0.0" } }))
            }
            _ => {
                Ok(Value::Null)
            }
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    timeout_ms: u64,
}

fn with_extra_roots(mut b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if let Some(pem) = zen_core::extra_root_pem()
        && let Ok(certs) = reqwest::Certificate::from_pem_bundle(&pem)
    {
        for c in certs {
            b = b.add_root_certificate(c);
        }
    }
    b
}

impl HttpTransport {
    pub(crate) fn new(cfg: &McpServerConfig, timeout_ms: u64) -> Result<Arc<Self>> {
        let url = cfg
            .url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("http server has no url"))?;
        let mut builder = with_extra_roots(
            reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms.max(1))),
        );

        if url_is_loopback(&url) {
            builder = builder.no_proxy();
        }
        let client = builder.build()?;
        Ok(Arc::new(Self {
            client,
            url,
            headers: cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            timeout_ms,
        }))
    }

    fn build(&self, msg: &Value) -> reqwest::RequestBuilder {
        let mut rb = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(msg);
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = self.session_id.lock().expect("session_id lock must not be poisoned").clone() {
            rb = rb.header("Mcp-Session-Id", sid);
        }
        rb
    }

    fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(sid) = resp.headers().get("Mcp-Session-Id")
            && let Ok(s) = sid.to_str()
        {
            *self.session_id.lock().expect("session_id lock must not be poisoned") = Some(s.to_string());
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let resp = self.build(&msg).send().await?;
        self.capture_session(&resp);
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("MCP HTTP {} for '{}'", status, method));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await?;

        let val = if content_type.contains("text/event-stream")
            || body.trim_start().starts_with("data:")
        {
            parse_sse_for_id(&body, id).ok_or_else(|| {
                anyhow::anyhow!("MCP SSE response carried no payload for id {}", id)
            })?
        } else {
            serde_json::from_str::<Value>(&body)
                .map_err(|e| anyhow::anyhow!("invalid MCP JSON response: {}", e))?
        };
        if let Some(err) = val.get("error") {
            return Err(anyhow::anyhow!("MCP error: {}", err));
        }
        Ok(val.get("result").cloned().unwrap_or(Value::Null))
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        let resp = self.build(&msg).send().await?;
        self.capture_session(&resp);
        Ok(())
    }
}

fn parse_sse_for_id(body: &str, id: u64) -> Option<Value> {
    let mut last: Option<Value> = None;
    for line in body.lines() {
        let line = line.trim_start();
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(data) {
            let matches = val.get("id").and_then(id_to_u64) == Some(id);
            if matches || val.get("id").is_none() {
                last = Some(val);
            }
        }
    }
    last
}

fn id_to_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn url_is_loopback(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.rsplit_once('@') {
        Some((_, h)) => h,
        None => authority,
    };
    let host = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.starts_with("127.")
        || host == "0.0.0.0"
}

pub(crate) enum Transport {
    Stdio(Arc<McpClient>),
    Http(Arc<HttpTransport>),
}

impl Transport {
    pub(crate) fn from_config(cfg: &McpServerConfig, timeout_ms: u64) -> Result<Self> {
        if cfg.is_http() {
            Ok(Transport::Http(HttpTransport::new(cfg, timeout_ms)?))
        } else {
            Ok(Transport::Stdio(Arc::new(McpClient::spawn(cfg, timeout_ms).await?)))
        }
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(t) => t.request(method, params).await,
            Transport::Http(t) => t.request(method, params).await,
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        match self {
            Transport::Stdio(t) => t.notify(method, params).await,
            Transport::Http(t) => t.notify(method, params).await,
        }
    }
}

pub(crate) async fn handshake(transport: &Transport) -> Result<Vec<RemoteTool>> {
    let init_params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "ZenCode", "version": env!("CARGO_PKG_VERSION")}
    });
    let _ = transport
        .request("initialize", init_params)
        .await
        .map_err(|e| anyhow::anyhow!("initialize failed: {}", e))?;
    transport
        .notify("notifications/initialized", json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("initialized notification failed: {}", e))?;
    list_tools(transport).await
}

pub(crate) async fn list_tools(transport: &Transport) -> Result<Vec<RemoteTool>> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let result = transport
            .request("tools/list", params)
            .await
            .map_err(|e| anyhow::anyhow!("tools/list failed: {}", e))?;
        if let Some(arr) = result.get("tools").and_then(|v| v.as_array()) {
            for t in arr {
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                tools.push(RemoteTool {
                    name,
                    description,
                    schema,
                });
            }
        }
        cursor = result
            .get("nextCursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}