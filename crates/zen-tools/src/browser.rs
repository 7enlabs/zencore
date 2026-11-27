//! Browser agent, Chặng 1 (spec `docs/browser-agent-spec.md` @ 57128f0):
//! drive headless Chromium over raw CDP (WebSocket) and expose it as six
//! agent tools. Only the CDP domains the spec needs (`Page`, `Runtime`,
//! `Input`) plus `Fetch` for session-wide network interception.
//!
//! Design notes (spec section 3):
//! - Raw CDP over `tokio-tungstenite` (option A): ZenCore already runs on
//!   tokio + serde_json, so only two small crates are added.
//! - Agent data (selector, typed text, URL) always travels as JSON
//!   parameters of a CDP call, never interpolated into a JS string
//!   (spec section 3.4). Selector lookup uses `Runtime.callFunctionOn`
//!   with a fixed `functionDeclaration` and the selector as a JSON `value`
//!   argument; typing uses `Input.insertText`.
//! - One session = one browser + one tab. The first tool call creates the
//!   session lazily; it lives in a process-wide registry keyed by the
//!   agent `session_id` and is closed when the agent session ends (or
//!   when the process exits - the Chromium child is `kill_on_drop`).
//! - Network interception (`Fetch.enable`, spec section 6.9) runs for the
//!   whole session: every paused request is re-checked with the
//!   `check_fetch_target` of WebFetch and aborted when it targets a
//!   non-public address. Paused requests are serviced inline while a tool
//!   call is pumping CDP messages; a request issued while the session is
//!   idle waits for the next tool call to pump.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use zen_core::{Tool, ToolContext, ToolOutput};

/// Tool names (spec section 2). Snake-case, as the spec writes them.
pub const BROWSER_NAVIGATE: &str = "browser_navigate";
pub const BROWSER_CLICK: &str = "browser_click";
pub const BROWSER_TYPE: &str = "browser_type";
pub const BROWSER_CONTENT: &str = "browser_content";
pub const BROWSER_SCREENSHOT: &str = "browser_screenshot";
pub const BROWSER_WAIT: &str = "browser_wait";

/// Rule-level class name (spec section 4.5). A rule may name either
/// spelling; matching normalises case and underscores.
pub const RULE_BROWSER_LOGIN: &str = "BrowserLogin";

pub fn is_browser_tool(name: &str) -> bool {
    matches!(
        name,
        BROWSER_NAVIGATE
            | BROWSER_CLICK
            | BROWSER_TYPE
            | BROWSER_CONTENT
            | BROWSER_SCREENSHOT
            | BROWSER_WAIT
    )
}

pub fn is_browser_mutating(name: &str) -> bool {
    matches!(name, BROWSER_CLICK | BROWSER_TYPE)
}

const UNDERSCORE: char = 95 as char;

/// Normalise a tool/rule name for comparison: lower-case, drop `_`.
pub fn norm_tool_name(name: &str) -> String {
    name.chars()
        .filter(|c| *c != UNDERSCORE)
        .flat_map(|c| c.to_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Fixed JS function declarations (spec section 3.4).
//
// Every one of these is a constant string baked into the code. Agent data
// NEVER enters them: selectors and text travel as JSON `arguments` values
// of `Runtime.callFunctionOn`, so a selector containing quotes cannot
// break out of the expression and become code.
// ---------------------------------------------------------------------------

/// Probe one element: geometry, tag/type, text (capped), visibility, and
/// the login/submit flags the permission gate needs. Returns
/// `{found, error?, ...}`; `error` means the selector is syntactically
/// invalid (`querySelector` threw), `found: false` means no match.
const FN_FIND: &str = r#"function(sel) {
  try {
    var el = document.querySelector(sel);
    if (!el) return {found: false};
    var r = el.getBoundingClientRect();
    var cs = getComputedStyle(el);
    var form = el.closest("form");
    return {found: true, tag: el.tagName, type: el.type || "",
      text: (el.innerText || el.value || "").slice(0, 500),
      x: r.x, y: r.y, width: r.width, height: r.height,
      visible: r.width > 0 && r.height > 0 && cs.visibility !== "hidden" && cs.display !== "none",
      editable: el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable,
      isPassword: el.tagName === "INPUT" && el.type === "password",
      isSubmit: (el.tagName === "BUTTON" && el.type !== "button" && el.type !== "reset")
        || (el.tagName === "INPUT" && (el.type === "submit" || el.type === "button" || el.type === "image")),
      inPasswordForm: !!form && !!form.querySelector("input[type=password]")};
  } catch (e) { return {error: "invalid selector: " + e.message}; }
}"#;

const FN_FOCUS: &str = r#"function(sel) {
  try {
    var el = document.querySelector(sel);
    if (!el) return {found: false};
    el.focus();
    return {found: true};
  } catch (e) { return {error: "invalid selector: " + e.message}; }
}"#;

const FN_SCROLL: &str = r#"function(sel) {
  try {
    var el = document.querySelector(sel);
    if (!el) return {found: false};
    el.scrollIntoView({block: "center"});
    var r = el.getBoundingClientRect();
    return {found: true, x: r.x, y: r.y, width: r.width, height: r.height};
  } catch (e) { return {error: "invalid selector: " + e.message}; }
}"#;

/// `sel === null` reads the whole body text. Returns `{found, text?}` or
/// `{error}` for an invalid selector.
const FN_TEXT: &str = r#"function(sel) {
  try {
    if (sel === null || sel === undefined) {
      return {found: true, text: document.body ? document.body.innerText : ""};
    }
    var el = document.querySelector(sel);
    if (!el) return {found: false};
    return {found: true, text: el.innerText || ""};
  } catch (e) { return {error: "invalid selector: " + e.message}; }
}"#;

const FN_HAS_PASSWORD: &str =
    r#"function() { return {hasPassword: !!document.querySelector("input[type=password]")}; }"#;

const FN_NAV_INFO: &str = r#"function() {
  var nav = performance.getEntriesByType("navigation")[0];
  return {url: location.href, title: document.title,
    textLen: document.body ? document.body.innerText.length : 0,
    status: nav ? (nav.responseStatus || 0) : 0,
    width: window.innerWidth, height: window.innerHeight};
}"#;

const FN_NET_IDLE: &str = r#"function() {
  return {n: performance.getEntriesByType("resource").length,
    state: document.readyState};
}"#;

const FN_TEXT_PRESENT: &str = r#"function(t) {
  var body = document.body ? document.body.innerText : "";
  return {found: body.indexOf(t) !== -1};
}"#;

// ---------------------------------------------------------------------------
// Selector validation (spec section 2.7) + action classification (spec
// sections 4 and 5).
// ---------------------------------------------------------------------------

/// Rust-side CSS selector syntax check, run before anything is sent to
/// CDP: empty, NUL, or unbalanced brackets/quotes/parens are rejected
/// immediately. The browser-side `try/catch` in `FN_FIND` is the second
/// layer: a selector that passes here but is still invalid CSS fails
/// there with "invalid selector" before any action runs.
pub fn validate_selector(selector: &str) -> std::result::Result<(), String> {
    let s = selector.trim();
    if s.is_empty() {
        return Err("selector is empty".to_string());
    }
    if s.as_bytes().contains(&0) {
        return Err("selector contains NUL".to_string());
    }
    // Byte codes, so no char literals are needed: 34 double quote,
    // 39 single quote, 40 open paren, 41 close paren, 91 open bracket,
    // 93 close bracket, 92 backslash.
    let mut paren = 0i32;
    let mut brack = 0i32;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for b in s.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            92 => escaped = true,
            34 | 39 if quote.is_none() => quote = Some(b),
            b if Some(b) == quote => quote = None,
            40 if quote.is_none() => paren += 1,
            41 if quote.is_none() => {
                paren -= 1;
                if paren < 0 {
                    return Err("selector has unbalanced )".to_string());
                }
            }
            91 if quote.is_none() => brack += 1,
            93 if quote.is_none() => {
                brack -= 1;
                if brack < 0 {
                    return Err("selector has unbalanced ]".to_string());
                }
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err("selector has an unterminated quote".to_string());
    }
    if paren != 0 {
        return Err("selector has unbalanced (".to_string());
    }
    if brack != 0 {
        return Err("selector has unbalanced [".to_string());
    }
    Ok(())
}

/// What the live page says about one element (from `FN_FIND`).
#[derive(Debug, Clone, Default)]
pub struct SelectorFlags {
    pub is_password_field: bool,
    pub is_submit: bool,
    pub is_destructive: bool,
    pub in_password_form: bool,
}

/// What the permission gate decided about one call (spec sections 4/5).
/// The gate writes it into the session cache; the tool re-verifies it at
/// runtime against the live element, so a stale cache can never smuggle a
/// login, submit, or destructive action past the gate.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateMark {
    pub login: bool,
    pub submit: bool,
    pub destructive: bool,
}

/// Selector-text heuristics: the gate can only see the selector string, so
/// it classifies from that plus the cached page state. Anything the
/// heuristic misses, the runtime probe catches (and refuses) instead.
pub fn selector_hints_password(selector: &str) -> bool {
    let l = selector.to_lowercase();
    l.contains("password") || l.contains("passwd") || l.contains("type=password")
}

pub fn selector_hints_submit(selector: &str) -> bool {
    let l = selector.to_lowercase();
    l.contains("submit") || l.contains("type=submit")
}

/// Destructive / irreversible keywords (spec section 4.9b): delete, cancel,
/// buy, pay, send out. Matched against the selector at the gate and
/// against the live element text at runtime.
pub fn text_hints_destructive(text: &str) -> bool {
    let l = text.to_lowercase();
    [
        "delete",
        "remove",
        "destroy",
        "cancel",
        "buy",
        "purchase",
        "checkout",
        "pay",
        "send",
        "transfer",
        "unsubscribe",
    ]
    .iter()
    .any(|k| l.contains(k))
}

pub fn selector_hints_destructive(selector: &str) -> bool {
    text_hints_destructive(selector)
}

// ---------------------------------------------------------------------------
// Raw CDP client (spec section 3.2, option A).
// ---------------------------------------------------------------------------

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Session-wide network guard (spec section 6.9).
struct Intercept {
    allow_private: bool,
    pinned: Arc<crate::builtin::PinnedResolver>,
}

struct CdpClient {
    ws: Ws,
    next_id: u64,
    events: VecDeque<Value>,
    intercept: Option<Arc<Intercept>>,
    /// Default JS execution context of the main frame, learned from
    /// `Runtime.executionContextCreated` events (spec section 3.4):
    /// real Chromium rejects `Runtime.callFunctionOn` without it.
    exec_ctx: Option<u64>,
    /// Main frame id, learned from `Page.frameNavigated` (a frame
    /// without a parent). Used to ignore iframe contexts.
    main_frame: Option<String>,
}

impl CdpClient {
    async fn connect(ws_url: &str, intercept: Option<Arc<Intercept>>) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .context("connect CDP websocket")?;
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
            intercept,
            exec_ctx: None,
            main_frame: None,
        })
    }

    async fn next_msg(&mut self, dur: Duration) -> Result<Value> {
        let msg = timeout(dur, self.ws.next())
            .await
            .context("timed out waiting for CDP message")?
            .ok_or_else(|| anyhow!("CDP websocket closed"))??;
        let Message::Text(text) = msg else {
            bail!("unexpected non-text CDP message");
        };
        serde_json::from_str(&text).context("parse CDP message")
    }

    /// Send a method and wait for its reply (matching id). Events seen on
    /// the way are buffered for `wait_event`. `Fetch.requestPaused`
    /// events are answered inline (see `handle_paused`).
    async fn call(&mut self, method: &str, params: Value, dur: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .context("send CDP message")?;
        loop {
            let msg = self.next_msg(dur).await?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("CDP {method} failed: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            self.on_message(msg).await?;
        }
    }

    async fn on_message(&mut self, msg: Value) -> Result<()> {
        if msg.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
            self.handle_paused(&msg).await?;
            return Ok(());
        }
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            match method {
                "Runtime.executionContextCreated" => self.note_exec_ctx(&msg),
                "Runtime.executionContextDestroyed" => {
                    let id = msg
                        .get("params")
                        .and_then(|p| p.get("executionContextId"))
                        .and_then(Value::as_u64);
                    if id.is_some() && id == self.exec_ctx {
                        self.exec_ctx = None;
                    }
                }
                "Page.frameNavigated" => {
                    let frame = msg.get("params").and_then(|p| p.get("frame"));
                    let main_id = frame
                        .filter(|f| f.get("parentId").and_then(Value::as_str).is_none())
                        .and_then(|f| f.get("id"))
                        .and_then(Value::as_str);
                    if let Some(id) = main_id {
                        self.main_frame = Some(id.to_string());
                    }
                }
                _ => {}
            }
            self.events.push_back(msg);
        }
        Ok(())
    }

    /// Record the default execution context of the main frame from a
    /// `Runtime.executionContextCreated` event. Iframe contexts are
    /// ignored once the main frame id is known; before that, the page
    /// has a single frame, so any default context is the main one.
    fn note_exec_ctx(&mut self, msg: &Value) {
        let ctx = msg.get("params").and_then(|p| p.get("context"));
        let is_default = ctx
            .and_then(|c| c.get("auxData"))
            .and_then(|a| a.get("isDefault"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_default {
            return;
        }
        let frame_id = ctx
            .and_then(|c| c.get("auxData"))
            .and_then(|a| a.get("frameId"))
            .and_then(Value::as_str);
        let is_main_frame = self
            .main_frame
            .as_ref()
            .is_none_or(|main| frame_id == Some(main.as_str()));
        let id = ctx
            .and_then(|c| c.get("id"))
            .and_then(Value::as_u64)
            .filter(|_| is_default && is_main_frame);
        if let Some(id) = id {
            self.exec_ctx = Some(id);
        }
    }

    /// Answer one paused request (spec section 6.9): re-check the URL
    /// with the `check_fetch_target` of WebFetch. Non-HTTP schemes
    /// (`data:`, `blob:`, `about:`) are not network requests and pass
    /// through untouched. A blocked request fails closed: no retry by
    /// another path.
    async fn handle_paused(&mut self, msg: &Value) -> Result<()> {
        let Some(ic) = &self.intercept else {
            return Ok(());
        };
        let params = msg.get("params").unwrap_or(&Value::Null);
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let url = params
            .get("request")
            .and_then(|r| r.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_http = url.starts_with("http://") || url.starts_with("https://");
        let allowed = if !is_http {
            true
        } else {
            crate::builtin::check_fetch_target(url, ic.allow_private, &ic.pinned)
                .await
                .is_ok()
        };
        let (method, req_params) = if allowed {
            ("Fetch.continueRequest", json!({"requestId": request_id}))
        } else {
            eprintln!("[browser] blocked request to {url} (non-public target)");
            (
                "Fetch.failRequest",
                json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
            )
        };
        // Fire-and-forget: the reply arrives later with an id nobody
        // waits for, and the pump loop skips it. Awaiting it here would
        // deadlock the wait for the id the caller is expecting.
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "method": method, "params": req_params});
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .context("send CDP fetch verdict")?;
        Ok(())
    }

    async fn wait_event(&mut self, method: &str, dur: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            if let Some(pos) = self
                .events
                .iter()
                .position(|e| e.get("method").and_then(Value::as_str) == Some(method))
            {
                self.events.remove(pos);
                return Ok(());
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                bail!("timed out waiting for CDP event {method}");
            }
            let msg = self.next_msg(left).await?;
            if msg.get("method").and_then(Value::as_str) == Some(method) {
                return Ok(());
            }
            self.on_message(msg).await?;
        }
    }

    /// The default execution context of the main frame, for
    /// `Runtime.callFunctionOn` (spec section 3.4): real Chromium
    /// rejects the call without `executionContextId`. The id is
    /// learned from `Runtime.executionContextCreated` events; when it
    /// is not known yet, wait for the next one.
    async fn ensure_exec_ctx(&mut self, dur: Duration) -> Result<u64> {
        if let Some(id) = self.exec_ctx {
            return Ok(id);
        }
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                bail!("timed out waiting for a JS execution context");
            }
            let msg = self.next_msg(left).await?;
            self.on_message(msg).await?;
            if let Some(id) = self.exec_ctx {
                return Ok(id);
            }
        }
    }

    /// Call a fixed JS function with agent data as JSON arguments
    /// (spec section 3.4). Returns the `value` of the call result.
    async fn call_function_on(
        &mut self,
        decl: &str,
        args: Vec<Value>,
        dur: Duration,
    ) -> Result<Value> {
        let ctx = self.ensure_exec_ctx(dur).await?;
        let r = self
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": decl,
                    "executionContextId": ctx,
                    "arguments": args
                        .into_iter()
                        .map(|v| json!({ "value": v }))
                        .collect::<Vec<_>>(),
                    "returnByValue": true,
                }),
                dur,
            )
            .await?;
        if r.get("exceptionDetails").is_some() {
            bail!("JS exception: {r}");
        }
        Ok(r.get("result")
            .and_then(|x| x.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}

// ---------------------------------------------------------------------------
// Browser session: one Chromium child + one tab + cached page state.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct NavInfo {
    pub url: String,
    pub title: String,
    pub text_len: usize,
    pub status: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ElementInfo {
    pub found: bool,
    pub error: Option<String>,
    pub tag: String,
    pub el_type: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
    pub editable: bool,
    pub is_password: bool,
    pub is_submit: bool,
    pub in_password_form: bool,
}

fn v_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn v_bool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn v_f64(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn element_from_value(v: &Value) -> ElementInfo {
    ElementInfo {
        found: v_bool(v, "found"),
        error: v.get("error").and_then(Value::as_str).map(str::to_string),
        tag: v_str(v, "tag"),
        el_type: v_str(v, "type"),
        text: v_str(v, "text"),
        x: v_f64(v, "x"),
        y: v_f64(v, "y"),
        width: v_f64(v, "width"),
        height: v_f64(v, "height"),
        visible: v_bool(v, "visible"),
        editable: v_bool(v, "editable"),
        is_password: v_bool(v, "isPassword"),
        is_submit: v_bool(v, "isSubmit"),
        in_password_form: v_bool(v, "inPasswordForm"),
    }
}

pub struct BrowserSession {
    cdp: CdpClient,
    child: Option<Child>,
    profile_dir: Option<PathBuf>,
    work_dir: PathBuf,
    allow_private_network: bool,
    pinned: Arc<crate::builtin::PinnedResolver>,
    shot_count: u32,
    current_url: String,
    session_key: String,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        // The child is `kill_on_drop`, so no orphan Chromium survives.
        // The temp profile must go too; a sync remove is safe here.
        if let Some(dir) = self.profile_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

fn find_chromium() -> Result<String> {
    let bin = std::env::var("CHROMIUM_BIN").unwrap_or_default();
    if !bin.is_empty() {
        return Ok(bin);
    }
    for cand in ["chromium", "chromium-browser", "google-chrome"] {
        let Ok(out) = std::process::Command::new("which").arg(cand).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return Ok(p);
        }
    }
    for cand in [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
    ] {
        if Path::new(cand).exists() {
            return Ok(cand.to_string());
        }
    }
    bail!("no Chromium binary found (set CHROMIUM_BIN or install the `chromium` package)")
}

async fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    Ok(l.local_addr()?.port())
}

fn launch_chromium(bin: &str, port: u16, profile: &Path) -> Result<Child> {
    Command::new(bin)
        .args([
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--hide-scrollbars",
            "--window-size=1280,800",
            &format!("--remote-debugging-port={port}"),
            &format!("--user-data-dir={}", profile.display()),
            "about:blank",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to launch {bin}"))
}

async fn wait_for_version(client: &reqwest::Client, port: u16) -> Result<Value> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                return r.json().await.context("parse /json/version");
            }
            _ => {
                if tokio::time::Instant::now() >= deadline {
                    bail!("Chromium did not answer {url} within 30s");
                }
                sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn create_target(client: &reqwest::Client, port: u16) -> Result<String> {
    // Chrome >= 111 requires PUT for /json/new.
    let url = format!("http://127.0.0.1:{port}/json/new?about:blank");
    let resp = client
        .put(&url)
        .send()
        .await
        .context("PUT /json/new")?
        .error_for_status()
        .context("PUT /json/new status")?;
    let target: Value = resp.json().await.context("parse /json/new")?;
    target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in /json/new response: {target}"))
}

impl BrowserSession {
    /// Launch system Chromium headless and connect over CDP (spec 3.3).
    pub async fn launch(
        session_key: String,
        allow_private_network: bool,
        work_dir: PathBuf,
    ) -> Result<Self> {
        let bin = find_chromium()?;
        let port = free_port().await?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let profile =
            std::env::temp_dir().join(format!("zen-browser-{}-{nanos}", std::process::id()));
        let mut child = launch_chromium(&bin, port, &profile)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let pinned = Arc::new(crate::builtin::PinnedResolver::default());
        let res = async {
            let _version = wait_for_version(&client, port).await?;
            let ws_url = create_target(&client, port).await?;
            let intercept = Arc::new(Intercept {
                allow_private: allow_private_network,
                pinned: pinned.clone(),
            });
            let mut cdp = CdpClient::connect(&ws_url, Some(intercept)).await?;
            cdp.call("Page.enable", json!({}), Duration::from_secs(30))
                .await?;
            // Runtime.enable replays `Runtime.executionContextCreated`
            // for the existing contexts, so the default context id is
            // learned before the first `Runtime.callFunctionOn`.
            cdp.call("Runtime.enable", json!({}), Duration::from_secs(30))
                .await?;
            cdp.call(
                "Fetch.enable",
                json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
                Duration::from_secs(30),
            )
            .await?;
            Ok::<CdpClient, anyhow::Error>(cdp)
        }
        .await;
        match res {
            Ok(cdp) => Ok(Self {
                cdp,
                child: Some(child),
                profile_dir: Some(profile),
                work_dir,
                allow_private_network,
                pinned,
                shot_count: 0,
                current_url: String::new(),
                session_key,
            }),
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = std::fs::remove_dir_all(&profile);
                Err(e)
            }
        }
    }

    /// Connect to an already-running CDP endpoint (fake-server unit tests).
    #[cfg(test)]
    pub(crate) async fn connect_for_test(
        session_key: String,
        ws_url: &str,
        work_dir: PathBuf,
        with_intercept: bool,
    ) -> Result<Self> {
        let intercept = if with_intercept {
            Some(Arc::new(Intercept {
                allow_private: false,
                pinned: Arc::new(crate::builtin::PinnedResolver::default()),
            }))
        } else {
            None
        };
        let mut cdp = CdpClient::connect(ws_url, intercept).await?;
        // The fake server never sends `Runtime.executionContextCreated`
        // and ignores the id, so seed a dummy one: the unit tests
        // exercise the call path, not context discovery.
        cdp.exec_ctx = Some(1);
        Ok(Self {
            cdp,
            child: None,
            profile_dir: None,
            work_dir,
            allow_private_network: false,
            pinned: Arc::new(crate::builtin::PinnedResolver::default()),
            shot_count: 0,
            current_url: String::new(),
            session_key,
        })
    }

    async fn nav_info(&mut self, dur: Duration) -> Result<NavInfo> {
        let v = self.cdp.call_function_on(FN_NAV_INFO, vec![], dur).await?;
        Ok(NavInfo {
            url: v_str(&v, "url"),
            title: v_str(&v, "title"),
            text_len: v.get("textLen").and_then(Value::as_u64).unwrap_or(0) as usize,
            status: v.get("status").and_then(Value::as_u64).unwrap_or(0),
            width: v.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
            height: v.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
        })
    }

    async fn probe_element(&mut self, selector: &str, dur: Duration) -> Result<ElementInfo> {
        let v = self
            .cdp
            .call_function_on(FN_FIND, vec![json!(selector)], dur)
            .await?;
        let el = element_from_value(&v);
        if let Some(e) = &el.error {
            bail!("invalid selector {selector}: {e}");
        }
        Ok(el)
    }

    /// Refresh the sync page cache after an op that may change the page.
    /// Best effort: a cache failure must never fail the op itself.
    async fn refresh_page_state(&mut self, dur: Duration) {
        if let Ok(info) = self.nav_info(dur).await {
            self.current_url = info.url.clone();
            let has_password = self
                .cdp
                .call_function_on(FN_HAS_PASSWORD, vec![], dur)
                .await
                .map(|v| v_bool(&v, "hasPassword"))
                .unwrap_or(false);
            update_cached_page(&self.session_key, info.url, has_password);
        }
    }

    /// Navigate (spec 2.1). The initial URL is re-checked with
    /// `check_fetch_target`; every redirect hop and sub-resource is
    /// re-checked again by the `Fetch` interceptor (spec 6.9).
    pub async fn navigate(
        &mut self,
        url: &str,
        wait_until: &str,
        timeout_dur: Duration,
    ) -> Result<NavInfo> {
        crate::builtin::check_fetch_target(url, self.allow_private_network, &self.pinned)
            .await
            .map_err(|e| anyhow!(e))?;
        self.cdp
            .call("Page.navigate", json!({"url": url}), timeout_dur)
            .await?;
        self.cdp
            .wait_event("Page.loadEventFired", timeout_dur)
            .await?;
        if wait_until == "networkidle" {
            self.wait_network_idle(timeout_dur).await?;
        }
        let info = self.nav_info(timeout_dur).await?;
        if info.status >= 400 {
            bail!("page returned HTTP {}", info.status);
        }
        self.current_url = info.url.clone();
        self.refresh_page_state(timeout_dur).await;
        Ok(info)
    }

    async fn wait_network_idle(&mut self, dur: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + dur;
        let mut stable_since: Option<tokio::time::Instant> = None;
        let mut last_n: Option<u64> = None;
        loop {
            let v = self
                .cdp
                .call_function_on(FN_NET_IDLE, vec![], Duration::from_secs(10))
                .await?;
            let n = v.get("n").and_then(Value::as_u64).unwrap_or(0);
            let state = v_str(&v, "state");
            let now = tokio::time::Instant::now();
            if state == "complete" && Some(n) == last_n {
                match stable_since {
                    Some(since) if now.duration_since(since) >= Duration::from_millis(500) => {
                        return Ok(());
                    }
                    Some(_) => {}
                    None => stable_since = Some(now),
                }
            } else {
                stable_since = None;
                last_n = Some(n);
            }
            if now >= deadline {
                bail!("timed out waiting for network idle");
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Read rendered text (spec 2.4). Returns (text, truncated, total).
    pub async fn content(
        &mut self,
        selector: Option<&str>,
        max_chars: usize,
        dur: Duration,
    ) -> Result<(String, bool, usize)> {
        if let Some(sel) = selector {
            validate_selector(sel).map_err(|e| anyhow!(e))?;
        }
        let v = self
            .cdp
            .call_function_on(FN_TEXT, vec![json!(selector)], dur)
            .await?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            bail!("invalid selector: {e}");
        }
        if !v_bool(&v, "found") {
            bail!("element not found for selector {}", selector.unwrap_or(""));
        }
        let text = v_str(&v, "text");
        let total = text.chars().count();
        if total <= max_chars {
            return Ok((text, false, total));
        }
        let head: String = text.chars().take(max_chars).collect();
        Ok((head, true, total))
    }

    /// Screenshot to a PNG file under the session work dir (spec 2.5).
    /// Returns (path, file size, viewport width, viewport height).
    pub async fn screenshot(
        &mut self,
        selector: Option<&str>,
        full_page: bool,
        dur: Duration,
    ) -> Result<(PathBuf, u64, u32, u32)> {
        let mut params = json!({"format": "png", "captureBeyondViewport": full_page});
        if let Some(sel) = selector {
            validate_selector(sel).map_err(|e| anyhow!(e))?;
            let el = self.probe_element(sel, dur).await?;
            if !el.found {
                bail!("element not found for selector {sel}");
            }
            if !el.visible {
                bail!("element is not visible for selector {sel}");
            }
            params["clip"] = json!({
                "x": el.x, "y": el.y,
                "width": el.width, "height": el.height, "scale": 1.0
            });
        }
        let r = self.cdp.call("Page.captureScreenshot", params, dur).await?;
        let data = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("screenshot returned no data"))?;
        let bytes = B64.decode(data).context("decode screenshot PNG")?;
        let info = self.nav_info(dur).await.unwrap_or_default();
        std::fs::create_dir_all(&self.work_dir)?;
        self.shot_count += 1;
        let path = self
            .work_dir
            .join(format!("screenshot-{}.png", self.shot_count));
        std::fs::write(&path, &bytes)?;
        Ok((path, bytes.len() as u64, info.width, info.height))
    }

    /// Wait for a selector or a text to appear (spec 2.6).
    /// Returns (what matched, elapsed ms).
    pub async fn wait_for(
        &mut self,
        selector: Option<&str>,
        text: Option<&str>,
        dur: Duration,
    ) -> Result<(String, u64)> {
        let start = tokio::time::Instant::now();
        let deadline = start + dur;
        loop {
            if let Some(sel) = selector {
                let el = self.probe_element(sel, Duration::from_secs(10)).await?;
                if el.found {
                    return Ok(("selector".to_string(), start.elapsed().as_millis() as u64));
                }
            } else if let Some(t) = text {
                let v = self
                    .cdp
                    .call_function_on(FN_TEXT_PRESENT, vec![json!(t)], Duration::from_secs(10))
                    .await?;
                if v_bool(&v, "found") {
                    return Ok(("text".to_string(), start.elapsed().as_millis() as u64));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                if let Some(sel) = selector {
                    bail!("timed out waiting for selector {sel}");
                }
                bail!("timed out waiting for text");
            }
            sleep(Duration::from_millis(200)).await;
        }
    }

    /// Click an element (spec 2.2). Returns (element text, URL after).
    pub async fn click(&mut self, selector: &str, dur: Duration) -> Result<(String, String)> {
        validate_selector(selector).map_err(|e| anyhow!(e))?;
        let el = self.probe_element(selector, dur).await?;
        if !el.found {
            bail!("element not found for selector {selector}");
        }
        if !el.visible {
            bail!("element is not visible or not clickable for selector {selector}");
        }
        let runtime = SelectorFlags {
            is_password_field: false,
            is_submit: el.is_submit,
            is_destructive: text_hints_destructive(&el.text),
            in_password_form: el.in_password_form,
        };
        check_gate_mark(&self.session_key, selector, &runtime, false).map_err(|e| anyhow!(e))?;
        let scrolled = self
            .cdp
            .call_function_on(FN_SCROLL, vec![json!(selector)], dur)
            .await?;
        let el2 = element_from_value(&scrolled);
        let cx = el2.x + el2.width / 2.0;
        let cy = el2.y + el2.height / 2.0;
        for kind in ["mousePressed", "mouseReleased"] {
            self.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({"type": kind, "button": "left", "x": cx, "y": cy, "clickCount": 1}),
                    dur,
                )
                .await?;
        }
        sleep(Duration::from_millis(300)).await;
        self.refresh_page_state(dur).await;
        Ok((el.text, self.current_url.clone()))
    }

    /// Type into an editable element (spec 2.3). The text travels only
    /// as the `Input.insertText` JSON parameter; it is never logged and
    /// never echoed back. Returns (chars typed, was password field).
    pub async fn type_text(
        &mut self,
        selector: &str,
        text: &str,
        submit: bool,
        dur: Duration,
    ) -> Result<(usize, bool)> {
        validate_selector(selector).map_err(|e| anyhow!(e))?;
        let el = self.probe_element(selector, dur).await?;
        if !el.found {
            bail!("element not found for selector {selector}");
        }
        if !el.editable {
            bail!("element is not editable for selector {selector}");
        }
        let runtime = SelectorFlags {
            is_password_field: el.is_password,
            is_submit: false,
            is_destructive: false,
            in_password_form: el.in_password_form,
        };
        check_gate_mark(&self.session_key, selector, &runtime, submit).map_err(|e| anyhow!(e))?;
        let focused = self
            .cdp
            .call_function_on(FN_FOCUS, vec![json!(selector)], dur)
            .await?;
        if !v_bool(&focused, "found") {
            bail!("element not found for selector {selector}");
        }
        self.cdp
            .call("Input.insertText", json!({"text": text}), dur)
            .await?;
        if submit {
            for kind in ["keyDown", "keyUp"] {
                self.cdp
                    .call(
                        "Input.dispatchKeyEvent",
                        json!({"type": kind, "key": "Enter", "code": "Enter",
                               "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13}),
                        dur,
                    )
                    .await?;
            }
        }
        sleep(Duration::from_millis(200)).await;
        self.refresh_page_state(dur).await;
        Ok((text.chars().count(), el.is_password))
    }

    pub async fn close(mut self) {
        self.cdp.close().await;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(dir) = self.profile_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// Session registry + sync page cache.
//
// The permission gate (`permissions.rs`) is synchronous, so it cannot lock
// the async session mutex. The small facts it needs (current host,
// whether the page holds a password field, per-selector runtime flags,
// per-selector gate marks) live in this plain `std::sync::Mutex` cache,
// updated by the session after every op. Nothing secret ever lands here:
// no typed text, no cookies, no page content.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CachedPage {
    pub url: String,
    pub host: String,
    pub has_password_field: bool,
    pub selector_flags: HashMap<String, SelectorFlags>,
    pub gate_marks: HashMap<String, GateMark>,
}

static SESSIONS: std::sync::LazyLock<
    StdMutex<HashMap<String, Arc<tokio::sync::Mutex<BrowserSession>>>>,
> = std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
static PAGE_CACHE: std::sync::LazyLock<StdMutex<HashMap<String, CachedPage>>> =
    std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
static LAST_ACTIVE: StdMutex<Option<String>> = StdMutex::new(None);

pub(crate) fn cached_page(session_key: &str) -> Option<CachedPage> {
    PAGE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_key)
        .cloned()
}

pub(crate) fn update_cached_page(session_key: &str, url: String, has_password: bool) {
    let host = crate::builtin::url_host_port(&url).0;
    let mut cache = PAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache.entry(session_key.to_string()).or_default();
    entry.url = url;
    entry.host = host;
    entry.has_password_field = has_password;
    let mut last = LAST_ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    *last = Some(session_key.to_string());
}

pub(crate) fn gate_mark_for(session_key: &str, selector: &str) -> GateMark {
    cached_page(session_key)
        .and_then(|c| c.gate_marks.get(selector).copied())
        .unwrap_or_default()
}

pub(crate) fn record_gate_mark(session_key: &str, selector: &str, mark: GateMark) {
    let mut cache = PAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(session_key.to_string())
        .or_default()
        .gate_marks
        .insert(selector.to_string(), mark);
}

pub(crate) fn record_selector_flags(session_key: &str, selector: &str, flags: SelectorFlags) {
    let mut cache = PAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(session_key.to_string())
        .or_default()
        .selector_flags
        .insert(selector.to_string(), flags);
}

pub(crate) fn selector_flags_for(session_key: &str, selector: &str) -> Option<SelectorFlags> {
    cached_page(session_key).and_then(|c| c.selector_flags.get(selector).cloned())
}

/// Host of the most recently active browser session, for permission
/// prompts and host-scoped rules. Empty when no session exists yet.
pub fn current_browser_host() -> Option<String> {
    let last = LAST_ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let key = last.clone()?;
    cached_page(&key).map(|c| c.host)
}

pub fn current_browser_has_password() -> bool {
    let last = LAST_ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(key) = last.clone() else {
        return false;
    };
    cached_page(&key)
        .map(|c| c.has_password_field)
        .unwrap_or(false)
}

/// Runtime re-check (spec sections 4.9 and 5): the live element says what
/// the action really is. When it is a login, a submit, or a destructive
/// action and the gate did not mark this selector as such, refuse and
/// record the real flags, so the next gate evaluation classifies it
/// correctly and asks the user for that specific action.
pub(crate) fn check_gate_mark(
    session_key: &str,
    selector: &str,
    runtime: &SelectorFlags,
    submit: bool,
) -> std::result::Result<(), String> {
    let mark = gate_mark_for(session_key, selector);
    let login_needed = runtime.is_password_field || (runtime.is_submit && runtime.in_password_form);
    let submit_needed = runtime.is_submit || submit;
    let destructive_needed = runtime.is_destructive;
    if !login_needed && !submit_needed && !destructive_needed {
        return Ok(());
    }
    let mut missing: Vec<&str> = Vec::new();
    if login_needed && !mark.login {
        missing.push("BrowserLogin approval");
    }
    if submit_needed && !mark.submit {
        missing.push("a separate submit approval");
    }
    if destructive_needed && !mark.destructive {
        missing.push("a separate approval for a destructive action");
    }
    if missing.is_empty() {
        return Ok(());
    }
    record_selector_flags(session_key, selector, runtime.clone());
    Err(format!(
        "refused: this action needs {} first. The permission gate did not approve this specific action (its mark is missing or stale). Ask the user to approve it, then retry the same call.",
        missing.join(", ")
    ))
}

/// Gate-time classification of one click/type call (spec sections 4/5).
/// Uses the selector text, the `submit` flag, and the cached page state.
/// Writes the mark into the session cache for the runtime re-check and
/// returns (mark, is_login).
pub(crate) fn classify_browser_call(
    tool_name: &str,
    input: &Value,
    session_key: &str,
) -> (GateMark, bool) {
    let selector = input
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let submit_param = input
        .get("submit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cached = cached_page(session_key);
    let cached_flags = selector_flags_for(session_key, &selector);
    let page_has_password = cached
        .as_ref()
        .map(|c| c.has_password_field)
        .unwrap_or(false);
    let mut login = selector_hints_password(&selector);
    let mut submit = submit_param || selector_hints_submit(&selector);
    let mut destructive = selector_hints_destructive(&selector);
    if let Some(f) = &cached_flags {
        if f.is_password_field {
            login = true;
        }
        if f.is_submit {
            submit = true;
        }
        if f.is_destructive {
            destructive = true;
        }
        if f.is_submit && f.in_password_form {
            login = true;
        }
    }
    if tool_name == BROWSER_TYPE && page_has_password && !selector.is_empty() {
        // Conservative: on a page known to hold a login form, any typing
        // is treated as a possible login until the runtime probe says
        // otherwise. Over-asking here is safe; under-asking is not.
        login = true;
    }
    let mark = GateMark {
        login,
        submit,
        destructive,
    };
    record_gate_mark(session_key, &selector, mark);
    (mark, login)
}

/// Gate-time classification using the most recently active session
/// (spec section 4.5): rule matching has no `ToolContext`, so it uses
/// the last-active session key. The permission gate itself passes the
/// caller session key explicitly.
pub(crate) fn classify_current_browser_call(tool_name: &str, input: &Value) -> (GateMark, bool) {
    let last = LAST_ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let key = last.clone().unwrap_or_else(|| "default".to_string());
    classify_browser_call(tool_name, input, &key)
}

/// Lazily create (or reuse) the browser session for one agent session
/// (spec section 2.7). The registry key is the agent `session_id`.
pub async fn session_for(
    ctx: &ToolContext,
) -> std::result::Result<Arc<tokio::sync::Mutex<BrowserSession>>, String> {
    let key = if ctx.session_id.is_empty() {
        "default".to_string()
    } else {
        ctx.session_id.clone()
    };
    {
        let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = sessions.get(&key) {
            return Ok(s.clone());
        }
    }
    let work_dir = ctx.session_dir.clone().unwrap_or_else(std::env::temp_dir);
    let session = BrowserSession::launch(key.clone(), ctx.allow_private_network, work_dir)
        .await
        .map_err(|e| format!("could not start browser session: {e}"))?;
    let arc = Arc::new(tokio::sync::Mutex::new(session));
    {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = sessions.get(&key) {
            return Ok(existing.clone());
        }
        sessions.insert(key, arc.clone());
    }
    Ok(arc)
}

/// One-line permission-prompt hint (spec section 4.7): tool, selector,
/// host, action. Never includes typed text.
pub fn browser_permission_hint(tool_name: &str, input: &Value) -> Option<String> {
    if !is_browser_tool(tool_name) {
        return None;
    }
    let selector = input.get("selector").and_then(Value::as_str).unwrap_or("");
    let host = current_browser_host().unwrap_or_default();
    let action = if tool_name == BROWSER_TYPE {
        if input
            .get("submit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "submit form".to_string()
        } else if selector_hints_password(selector) {
            "type into a password field (login)".to_string()
        } else {
            "type text".to_string()
        }
    } else if tool_name == BROWSER_CLICK {
        if selector_hints_submit(selector) {
            "click a submit button".to_string()
        } else if selector_hints_destructive(selector) {
            "click (possibly destructive)".to_string()
        } else {
            "click".to_string()
        }
    } else {
        "read-only".to_string()
    };
    Some(format!(
        "browser: {tool_name} selector={selector:?} host={host} action={action}"
    ))
}

/// ACP permission-request title for browser tools (spec section 4.7).
pub fn browser_permission_title(tool_name: &str, input: &Value) -> String {
    match browser_permission_hint(tool_name, input) {
        Some(h) => h,
        None => tool_name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The six tools (spec section 2).
// ---------------------------------------------------------------------------

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn arg_str_opt(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn missing(key: &str) -> ToolOutput {
    ToolOutput::err(format!("Error: missing required parameter {key}"))
}

fn tool_err(tool: &str, msg: String) -> ToolOutput {
    ToolOutput::err(format!("Error: {tool}: {msg}"))
}

fn clamp_timeout(v: Option<i64>, default: u64, max: u64) -> Duration {
    match v {
        Some(n) if n > 0 => Duration::from_millis((n as u64).min(max)),
        _ => Duration::from_millis(default),
    }
}

fn clamp_usize(v: Option<i64>, default: usize, max: usize) -> usize {
    match v {
        Some(n) if n > 0 => (n as usize).min(max),
        _ => default,
    }
}

pub struct BrowserNavigateTool;

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        BROWSER_NAVIGATE
    }
    fn description(&self) -> &str {
        "Navigate the browser tab to a public http(s) URL; returns final URL, status, title, and text length."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "URL to navigate to."},
                "wait_until": {"type": "string", "description": "\"load\" (default) or \"networkidle\"."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms, default 30000."}
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(url) = arg_str_opt(&args, "url") else {
            return missing("url");
        };
        let url = crate::builtin::upgrade_scheme(&url);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return tool_err(BROWSER_NAVIGATE, format!("unsupported URL {url}"));
        }
        let wait_until = arg_str_opt(&args, "wait_until").unwrap_or_else(|| "load".to_string());
        if wait_until != "load" && wait_until != "networkidle" {
            return tool_err(
                BROWSER_NAVIGATE,
                format!("unsupported wait_until {wait_until}"),
            );
        }
        let timeout_dur = clamp_timeout(arg_i64(&args, "timeout_ms"), 30_000, 120_000);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_NAVIGATE, e),
        };
        let mut s = session.lock().await;
        match s.navigate(&url, &wait_until, timeout_dur).await {
            Ok(info) => ToolOutput::ok(format!(
                "navigated to {}\nstatus: {}\ntitle: {}\ntext length: {} chars",
                info.url, info.status, info.title, info.text_len
            )),
            Err(e) => tool_err(BROWSER_NAVIGATE, e.to_string()),
        }
    }
}

pub struct BrowserContentTool;

#[async_trait]
impl Tool for BrowserContentTool {
    fn name(&self) -> &str {
        BROWSER_CONTENT
    }
    fn description(&self) -> &str {
        "Read rendered page text (after JS ran), or of one element. Text only, never HTML."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of one element; omit for whole page."},
                "max_chars": {"type": "integer", "description": "Max chars, default 20000."}
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let selector = arg_str_opt(&args, "selector");
        let max_chars = clamp_usize(arg_i64(&args, "max_chars"), 20_000, 100_000);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_CONTENT, e),
        };
        let mut s = session.lock().await;
        match s
            .content(selector.as_deref(), max_chars, Duration::from_secs(30))
            .await
        {
            Ok((text, truncated, total)) => {
                if truncated {
                    ToolOutput::ok(format!(
                        "{text}\n... [truncated to {max_chars} of {total} chars]"
                    ))
                } else {
                    ToolOutput::ok(text)
                }
            }
            Err(e) => tool_err(BROWSER_CONTENT, e.to_string()),
        }
    }
}

pub struct BrowserScreenshotTool;

#[async_trait]
impl Tool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        BROWSER_SCREENSHOT
    }
    fn description(&self) -> &str {
        "Capture a PNG screenshot of the viewport or one element; saves it under the session work dir and returns the file path."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of one element; omit for viewport."},
                "full_page": {"type": "boolean", "description": "Whole page, default false."}
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let selector = arg_str_opt(&args, "selector");
        let full_page = arg_bool(&args, "full_page").unwrap_or(false);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_SCREENSHOT, e),
        };
        let mut s = session.lock().await;
        match s
            .screenshot(selector.as_deref(), full_page, Duration::from_secs(30))
            .await
        {
            Ok((path, size, width, height)) => ToolOutput::ok(format!(
                "screenshot saved to {}\nfile size: {} bytes\nviewport: {}x{}",
                path.display(),
                size,
                width,
                height
            )),
            Err(e) => tool_err(BROWSER_SCREENSHOT, e.to_string()),
        }
    }
}

pub struct BrowserWaitTool;

#[async_trait]
impl Tool for BrowserWaitTool {
    fn name(&self) -> &str {
        BROWSER_WAIT
    }
    fn description(&self) -> &str {
        "Wait for an element or text to appear in the rendered page. Use exactly one of selector or text."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "Element to wait for."},
                "text": {"type": "string", "description": "Text to wait for."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms, default 10000."}
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let selector = arg_str_opt(&args, "selector");
        let text = arg_str_opt(&args, "text");
        if selector.is_some() && text.is_some() {
            return tool_err(BROWSER_WAIT, "use only one of selector or text".to_string());
        }
        let timeout_dur = clamp_timeout(arg_i64(&args, "timeout_ms"), 10_000, 60_000);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_WAIT, e),
        };
        if selector.is_none() && text.is_none() {
            sleep(timeout_dur).await;
            return ToolOutput::ok(format!(
                "waited {} ms (no condition given)",
                timeout_dur.as_millis()
            ));
        }
        let mut s = session.lock().await;
        match s
            .wait_for(selector.as_deref(), text.as_deref(), timeout_dur)
            .await
        {
            Ok((what, elapsed)) => {
                ToolOutput::ok(format!("condition met: {what} after {elapsed} ms"))
            }
            Err(e) => tool_err(BROWSER_WAIT, e.to_string()),
        }
    }
}

pub struct BrowserClickTool;

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        BROWSER_CLICK
    }
    fn description(&self) -> &str {
        "Click one element in the browser tab. Always asks for approval; login, submit, and destructive clicks need a separate approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of exactly one element."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms, default 10000."}
            },
            "required": ["selector"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(selector) = arg_str_opt(&args, "selector") else {
            return missing("selector");
        };
        let timeout_dur = clamp_timeout(arg_i64(&args, "timeout_ms"), 10_000, 60_000);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_CLICK, e),
        };
        let mut s = session.lock().await;
        match s.click(&selector, timeout_dur).await {
            Ok((text, url)) => {
                ToolOutput::ok(format!("clicked {selector}\ntext: {text}\nurl: {url}"))
            }
            Err(e) => tool_err(BROWSER_CLICK, e.to_string()),
        }
    }
}

pub struct BrowserTypeTool;

#[async_trait]
impl Tool for BrowserTypeTool {
    fn name(&self) -> &str {
        BROWSER_TYPE
    }
    fn description(&self) -> &str {
        "Type text into an input or textarea. Always asks for approval; never returns or logs the text. Password fields need a BrowserLogin approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "CSS selector of an input, textarea, or contenteditable element."},
                "text": {"type": "string", "description": "Text to type."},
                "submit": {"type": "boolean", "description": "Press Enter after typing, default false."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms, default 10000."}
            },
            "required": ["selector", "text"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(selector) = arg_str_opt(&args, "selector") else {
            return missing("selector");
        };
        let Some(text) = arg_str_opt(&args, "text") else {
            return missing("text");
        };
        let submit = arg_bool(&args, "submit").unwrap_or(false);
        let timeout_dur = clamp_timeout(arg_i64(&args, "timeout_ms"), 10_000, 60_000);
        let session = match session_for(ctx).await {
            Ok(s) => s,
            Err(e) => return tool_err(BROWSER_TYPE, e),
        };
        let mut s = session.lock().await;
        match s.type_text(&selector, &text, submit, timeout_dur).await {
            Ok((len, is_password)) => {
                if is_password {
                    ToolOutput::ok(format!(
                        "typed {len} chars into {selector} (password field: content not echoed)"
                    ))
                } else {
                    ToolOutput::ok(format!("typed {len} chars into {selector}"))
                }
            }
            Err(e) => tool_err(BROWSER_TYPE, e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[test]
    fn validate_selector_accepts_valid() {
        for sel in [
            "#main",
            ".quote",
            "input[type=text]",
            "div[data-x=\"a\\\"b\"]",
            "#x:not(.a)",
            "ul li:nth-child(2)",
        ] {
            assert!(validate_selector(sel).is_ok(), "{sel}");
        }
    }

    #[test]
    fn validate_selector_rejects_invalid() {
        for sel in ["", "   ", "#x[", "#x(", "#x\"", "a\0b", "#x]"] {
            assert!(validate_selector(sel).is_err(), "{sel:?}");
        }
    }

    #[test]
    fn norm_tool_name_matches_both_spellings() {
        assert_eq!(norm_tool_name("BrowserClick"), "browserclick");
        assert_eq!(norm_tool_name("browser_click"), "browserclick");
        assert_eq!(norm_tool_name("BrowserLogin"), "browserlogin");
    }

    #[test]
    fn destructive_hints_cover_keywords() {
        assert!(text_hints_destructive("Delete account"));
        assert!(text_hints_destructive("BUY NOW"));
        assert!(!text_hints_destructive("Read more"));
        assert!(selector_hints_password("#password"));
        assert!(selector_hints_password("input[type=password]"));
        assert!(!selector_hints_password("#search"));
        assert!(selector_hints_submit("button[type=submit]"));
    }

    #[test]
    fn permission_hint_names_tool_selector_host_action() {
        update_cached_page("hint-key", "https://example.com/page".to_string(), false);
        let hint =
            browser_permission_hint(BROWSER_CLICK, &json!({"selector": "#ok"})).expect("hint");
        assert!(hint.contains("browser_click"), "{hint}");
        assert!(hint.contains("#ok"), "{hint}");
        assert!(hint.contains("host="), "{hint}");
        assert!(hint.contains("action=click"), "{hint}");
        assert!(browser_permission_hint("Read", &json!({})).is_none());
    }

    /// Canned CDP replies for one method. The selector in
    /// `arguments[0].value` picks the element the fake page pretends
    /// to hold; anything else is a visible DIV.
    fn fake_result(method: &str, params: &Value) -> Value {
        match method {
            "Runtime.callFunctionOn" => {
                let decl = params
                    .get("functionDeclaration")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arg0 = params
                    .get("arguments")
                    .and_then(|a| a.get(0))
                    .and_then(|x| x.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let sel = arg0.as_str().unwrap_or("");
                let value = if decl.contains("scrollIntoView") {
                    json!({"found": true, "x": 10, "y": 10, "width": 80, "height": 30})
                } else if decl.contains("el.focus") {
                    json!({"found": true})
                } else if decl.contains("hasPassword") {
                    json!({"hasPassword": true})
                } else if decl.contains("navigation") {
                    json!({"url": "https://example.com/page", "title": "Example",
                           "textLen": 42, "status": 200, "width": 1280, "height": 800})
                } else if decl.contains("resource") {
                    json!({"n": 3, "state": "complete"})
                } else if decl.contains("indexOf") {
                    json!({"found": true})
                } else if decl.contains("getBoundingClientRect") {
                    if sel == "#missing" {
                        json!({"found": false})
                    } else if sel == "#pass" {
                        json!({"found": true, "tag": "INPUT", "type": "password", "text": "",
                               "x": 0, "y": 0, "width": 100, "height": 20,
                               "visible": true, "editable": true,
                               "isPassword": true, "isSubmit": false, "inPasswordForm": true})
                    } else if sel == "#submit" {
                        json!({"found": true, "tag": "BUTTON", "type": "submit", "text": "Log in",
                               "x": 10, "y": 10, "width": 80, "height": 30,
                               "visible": true, "editable": false,
                               "isPassword": false, "isSubmit": true, "inPasswordForm": true})
                    } else if sel == "#plain-submit" {
                        json!({"found": true, "tag": "BUTTON", "type": "submit", "text": "Submit",
                               "x": 10, "y": 10, "width": 80, "height": 30,
                               "visible": true, "editable": false,
                               "isPassword": false, "isSubmit": true, "inPasswordForm": false})
                    } else if sel == "#delete" {
                        json!({"found": true, "tag": "BUTTON", "type": "button", "text": "Delete account",
                               "x": 10, "y": 10, "width": 80, "height": 30,
                               "visible": true, "editable": false,
                               "isPassword": false, "isSubmit": false, "inPasswordForm": false})
                    } else {
                        json!({"found": true, "tag": "DIV", "type": "", "text": "hello",
                               "x": 0, "y": 0, "width": 200, "height": 100,
                               "visible": true, "editable": false,
                               "isPassword": false, "isSubmit": false, "inPasswordForm": false})
                    }
                } else if arg0.is_null() {
                    json!({"found": true, "text": "body text here"})
                } else {
                    json!({"found": true, "text": "element text"})
                };
                json!({"result": {"type": "object", "value": value}})
            }
            "Page.captureScreenshot" => {
                json!({"data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8Xw8AAwMCAO+ip1sAAAAASUVORK5CYII="})
            }
            "Page.navigate" => json!({"frameId": "1"}),
            _ => json!({}),
        }
    }

    /// Fake CDP endpoint on loopback: answers every method with
    /// `fake_result`, records every request, and after a navigate sends
    /// two paused requests (one private, one public) plus loadEventFired.
    /// Like fake_result, but the network-idle probe returns a growing request
    /// count when `changing` is set, so wait_network_idle never sees stability.
    fn fake_result_mode(
        method: &str,
        params: &Value,
        net_idle_n: &StdMutex<u64>,
        changing: bool,
    ) -> Value {
        if method == "Runtime.callFunctionOn" {
            let decl = params
                .get("functionDeclaration")
                .and_then(Value::as_str)
                .unwrap_or("");
            if decl.contains("resource") {
                let n = if changing {
                    let mut g = net_idle_n.lock().unwrap();
                    let n = *g;
                    *g += 1;
                    n
                } else {
                    3
                };
                return json!({"result": {"type": "object", "value": {"n": n, "state": "complete"}}});
            }
        }
        fake_result(method, params)
    }

    async fn run_fake_server(
        addr_tx: oneshot::Sender<String>,
        seen: Arc<StdMutex<Vec<Value>>>,
        net_idle_changing: bool,
    ) {
        let net_idle_n = StdMutex::new(3u64);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _ = addr_tx.send(format!("ws://127.0.0.1:{port}/"));
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else { break };
            let Message::Text(text) = msg else { continue };
            let Ok(req) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            seen.lock().unwrap().push(req.clone());
            let method = req
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
            let result = fake_result_mode(
                &method,
                req.get("params").unwrap_or(&Value::Null),
                &net_idle_n,
                net_idle_changing,
            );
            let resp = json!({"id": id, "result": result});
            let _ = sink.send(Message::Text(resp.to_string().into())).await;
            if method == "Page.navigate" {
                sleep(Duration::from_millis(30)).await;
                let paused_private = json!({"method": "Fetch.requestPaused",
                    "params": {"requestId": "r-private",
                               "request": {"url": "http://127.0.0.1:9/x.png"}}});
                let _ = sink
                    .send(Message::Text(paused_private.to_string().into()))
                    .await;
                sleep(Duration::from_millis(30)).await;
                let paused_public = json!({"method": "Fetch.requestPaused",
                    "params": {"requestId": "r-public",
                               "request": {"url": "https://8.8.8.8/img.png"}}});
                let _ = sink
                    .send(Message::Text(paused_public.to_string().into()))
                    .await;
                sleep(Duration::from_millis(30)).await;
                let load = json!({"method": "Page.loadEventFired", "params": {}});
                let _ = sink.send(Message::Text(load.to_string().into())).await;
            }
            if method == "Page.enable" {
                sleep(Duration::from_millis(30)).await;
                let frame =
                    json!({"method": "Page.frameNavigated", "params": {"frame": {"id": "F1"}}});
                let _ = sink.send(Message::Text(frame.to_string().into())).await;
                sleep(Duration::from_millis(30)).await;
                let load = json!({"method": "Page.loadEventFired", "params": {}});
                let _ = sink.send(Message::Text(load.to_string().into())).await;
            }
        }
    }

    async fn fake_session(
        key: &str,
    ) -> (
        BrowserSession,
        Arc<StdMutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        fake_session_mode(key, false).await
    }

    async fn fake_session_mode(
        key: &str,
        net_idle_changing: bool,
    ) -> (
        BrowserSession,
        Arc<StdMutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let (tx, rx) = oneshot::channel();
        let seen2 = seen.clone();
        let handle = tokio::spawn(run_fake_server(tx, seen2, net_idle_changing));
        let ws_url = rx.await.expect("fake server address");
        let dir = std::env::temp_dir().join(format!("zen-browser-fake-{key}"));
        let _ = std::fs::remove_dir_all(&dir);
        let session = BrowserSession::connect_for_test(key.to_string(), &ws_url, dir, true)
            .await
            .expect("connect to fake CDP server");
        (session, seen, handle)
    }

    /// Raw fake CDP endpoint without a session, for CdpClient-level tests.
    async fn spawn_fake_server() -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let (tx, rx) = oneshot::channel();
        let seen2 = seen.clone();
        let handle = tokio::spawn(run_fake_server(tx, seen2, false));
        let ws_url = rx.await.expect("fake server address");
        (ws_url, seen, handle)
    }

    fn has_verdict(seen: &Arc<StdMutex<Vec<Value>>>, method: &str, request_id: &str) -> bool {
        seen.lock().unwrap().iter().any(|m| {
            m.get("method").and_then(Value::as_str) == Some(method)
                && m.get("params")
                    .and_then(|p| p.get("requestId"))
                    .and_then(Value::as_str)
                    == Some(request_id)
        })
    }

    fn call_ids(seen: &Arc<StdMutex<Vec<Value>>>) -> Vec<u64> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_u64))
            .collect()
    }

    fn count_calls(seen: &Arc<StdMutex<Vec<Value>>>, method: &str, decl_substr: &str) -> usize {
        count_calls_from(seen, 0, method, decl_substr)
    }

    fn count_calls_from(
        seen: &Arc<StdMutex<Vec<Value>>>,
        from: usize,
        method: &str,
        decl_substr: &str,
    ) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .skip(from)
            .filter(|m| {
                if m.get("method").and_then(Value::as_str) != Some(method) {
                    return false;
                }
                if decl_substr.is_empty() {
                    return true;
                }
                m.get("params")
                    .and_then(|p| p.get("functionDeclaration"))
                    .and_then(Value::as_str)
                    .map(|d| d.contains(decl_substr))
                    .unwrap_or(false)
            })
            .count()
    }

    fn last_call_ctx(seen: &Arc<StdMutex<Vec<Value>>>) -> Option<u64> {
        seen.lock().unwrap().iter().rev().find_map(|m| {
            if m.get("method").and_then(Value::as_str) != Some("Runtime.callFunctionOn") {
                return None;
            }
            m.get("params")
                .and_then(|p| p.get("executionContextId"))
                .and_then(Value::as_u64)
        })
    }

    fn dispatch_coords(seen: &Arc<StdMutex<Vec<Value>>>) -> Vec<(f64, f64)> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|m| {
                if m.get("method").and_then(Value::as_str) != Some("Input.dispatchMouseEvent") {
                    return None;
                }
                let p = m.get("params")?;
                Some((
                    p.get("x").and_then(Value::as_f64)?,
                    p.get("y").and_then(Value::as_f64)?,
                ))
            })
            .collect()
    }

    /// Restores an env var on drop. Serialises tests that mutate the process env.
    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
            EnvGuard { key, old }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// Seed a fake session into the global registry so tool `call` fns use it
    /// instead of launching a real Chromium.
    async fn seeded_tool_ctx(key: &str) -> (ToolContext, tokio::task::JoinHandle<()>) {
        let (session, _seen, handle) = fake_session(key).await;
        let arc = Arc::new(tokio::sync::Mutex::new(session));
        {
            let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            sessions.insert(key.to_string(), arc);
        }
        let dir = std::env::temp_dir().join(format!("zen-browser-tool-{key}"));
        let _ = std::fs::create_dir_all(&dir);
        (ToolContext::new(dir, key), handle)
    }

    /// Tiny loopback HTTP server for wait_for_version / create_target tests.
    /// The first request gets `first`, later requests get `after`.
    async fn run_version_server(first: (u16, &'static str), after: (u16, &'static str)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut seen = 0u32;
            while let Ok((mut stream, _)) = listener.accept().await {
                loop {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = stream.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let (status, body) = if seen == 0 { first } else { after };
                    seen += 1;
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn fake_navigate_content_wait_screenshot() {
        let (mut s, seen, handle) = fake_session("fake-nav-1").await;
        let info = s
            .navigate("https://8.8.8.8/page", "load", Duration::from_secs(10))
            .await
            .expect("navigate");
        assert_eq!(info.url, "https://example.com/page");
        assert_eq!(info.status, 200);
        assert_eq!(info.title, "Example");
        sleep(Duration::from_millis(300)).await;
        assert!(
            has_verdict(&seen, "Fetch.failRequest", "r-private"),
            "private request must be aborted"
        );
        assert!(
            has_verdict(&seen, "Fetch.continueRequest", "r-public"),
            "public request must continue"
        );
        let (text, truncated, total) = s
            .content(None, 20_000, Duration::from_secs(10))
            .await
            .expect("content");
        assert_eq!(text, "body text here");
        assert!(!truncated);
        assert_eq!(total, 14);
        let (what, _) = s
            .wait_for(Some("#x"), None, Duration::from_secs(5))
            .await
            .expect("wait_for");
        assert_eq!(what, "selector");
        let (path, size, _, _) = s
            .screenshot(None, false, Duration::from_secs(10))
            .await
            .expect("screenshot");
        assert!(size > 0);
        assert!(path.exists(), "screenshot file must exist");
        handle.abort();
    }

    #[tokio::test]
    async fn fake_click_and_type_with_gate_marks() {
        let (mut s, _seen, handle) = fake_session("fake-gate-1").await;
        let (text, _url) = s
            .click("#x", Duration::from_secs(10))
            .await
            .expect("plain click");
        assert_eq!(text, "hello");
        let err = s
            .click("#submit", Duration::from_secs(10))
            .await
            .expect_err("submit click without a gate mark must refuse");
        assert!(err.to_string().contains("BrowserLogin approval"), "{err}");
        let err = s
            .click("#delete", Duration::from_secs(10))
            .await
            .expect_err("destructive click without a gate mark must refuse");
        assert!(err.to_string().contains("destructive"), "{err}");
        let err = s
            .type_text("#pass", "secret", false, Duration::from_secs(10))
            .await
            .expect_err("password typing without a gate mark must refuse");
        assert!(err.to_string().contains("BrowserLogin approval"), "{err}");
        record_gate_mark(
            "fake-gate-1",
            "#pass",
            GateMark {
                login: true,
                submit: false,
                destructive: false,
            },
        );
        let (len, is_password) = s
            .type_text("#pass", "secret", false, Duration::from_secs(10))
            .await
            .expect("password typing with a login mark");
        assert_eq!(len, 6);
        assert!(is_password);
        handle.abort();
    }

    #[tokio::test]
    async fn fake_selector_with_quotes_stays_json_data() {
        let (mut s, seen, handle) = fake_session("fake-quote-1").await;
        let sel = "div[data-x=\"a\\\"b\"]";
        let (text, _, _) = s
            .content(Some(sel), 20_000, Duration::from_secs(10))
            .await
            .expect("content with quoted selector");
        assert_eq!(text, "element text");
        let found = seen.lock().unwrap().iter().any(|m| {
            if m.get("method").and_then(Value::as_str) != Some("Runtime.callFunctionOn") {
                return false;
            }
            let params = m.get("params").unwrap_or(&Value::Null);
            let decl = params
                .get("functionDeclaration")
                .and_then(Value::as_str)
                .unwrap_or("");
            let arg = params
                .get("arguments")
                .and_then(|a| a.get(0))
                .and_then(|x| x.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            arg == sel && !decl.contains("a\"b")
        });
        assert!(
            found,
            "selector must travel as a JSON argument value, never inside the JS source"
        );
        handle.abort();
    }

    #[tokio::test]
    #[ignore] // floor-guard: live test needs system Chromium + network; run explicitly with --ignored
    async fn live_navigate_content_screenshot() {
        // Needs system Chromium + network. Run:
        // cargo test -p zen-tools --lib browser::tests::live_ -- --ignored
        let dir = std::env::temp_dir().join("zen-browser-live-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = BrowserSession::launch("live-test".to_string(), false, dir.clone())
            .await
            .expect("launch chromium");
        let info = s
            .navigate(
                "https://quotes.toscrape.com/js/",
                "load",
                Duration::from_secs(60),
            )
            .await
            .expect("navigate");
        assert_eq!(info.status, 200);
        assert!(
            info.text_len > 100,
            "page text too short: {}",
            info.text_len
        );
        let (_what, _elapsed) = s
            .wait_for(Some(".quote"), None, Duration::from_secs(30))
            .await
            .expect("JS-rendered quotes must appear");
        let (text, _, _) = s
            .content(None, 20_000, Duration::from_secs(30))
            .await
            .expect("content");
        assert!(
            text.contains("Quotes to Scrape"),
            "rendered text missing page header"
        );
        let (path, size, _, _) = s
            .screenshot(None, false, Duration::from_secs(30))
            .await
            .expect("screenshot");
        assert!(size > 1000, "screenshot too small: {size}");
        assert!(path.exists());
        s.close().await;
    }

    // ==================== PR #580 mutant kills ====================

    #[test]
    fn validate_selector_quotes_do_not_count_parens_or_brackets() {
        // A paren/bracket inside a quoted attribute value is data, not syntax.
        for sel in [
            "div[title=\"(x\"]",
            "div[title=\")x\"]",
            "div[title=\"[x\"]",
            "div[title=\"]x\"]",
            "div[data-x='a\"b']",
        ] {
            assert!(validate_selector(sel).is_ok(), "{sel}");
        }
        // Unbalanced ( / [ must report the right bracket in the message.
        let err = validate_selector("#x(()").expect_err("unbalanced (");
        assert!(err.contains("unbalanced ("), "{err}");
        let err = validate_selector("#x[[]").expect_err("unbalanced [");
        assert!(err.contains("unbalanced ["), "{err}");
    }

    #[test]
    fn selector_hints_need_only_one_keyword() {
        // "passwd" alone must count as a password hint (kills || -> &&).
        assert!(selector_hints_password("#passwd"));
        assert!(selector_hints_password("input[type=password]"));
        assert!(!selector_hints_password("#search"));
        // "#submit" alone must count as a submit hint.
        assert!(selector_hints_submit("#submit"));
        assert!(selector_hints_submit("button[type=submit]"));
        assert!(!selector_hints_submit("#cancel"));
    }

    #[test]
    fn element_from_value_reads_floats() {
        let el = element_from_value(&json!({
            "found": true, "tag": "DIV", "type": "", "text": "hello",
            "x": 10.5, "y": 20.25, "width": 80.0, "height": 30.5,
            "visible": true, "editable": false,
            "isPassword": false, "isSubmit": false, "inPasswordForm": false
        }));
        assert!(el.found);
        assert_eq!(el.tag, "DIV");
        assert_eq!(el.text, "hello");
        assert_eq!(el.x, 10.5);
        assert_eq!(el.y, 20.25);
        assert_eq!(el.width, 80.0);
        assert_eq!(el.height, 30.5);
    }

    #[test]
    fn arg_accessors_read_typed_values() {
        let args =
            json!({"timeout_ms": 5000, "submit": true, "url": "https://example.com", "n": -5});
        assert_eq!(arg_i64(&args, "timeout_ms"), Some(5000));
        assert_eq!(arg_i64(&args, "n"), Some(-5));
        assert_eq!(arg_i64(&args, "missing"), None);
        assert_eq!(arg_bool(&args, "submit"), Some(true));
        assert_eq!(arg_bool(&json!({"submit": false}), "submit"), Some(false));
        assert_eq!(arg_bool(&args, "missing"), None);
        assert_eq!(
            arg_str_opt(&args, "url"),
            Some("https://example.com".to_string())
        );
        assert_eq!(arg_str_opt(&args, "missing"), None);
    }

    #[test]
    fn clamp_timeout_bounds_and_defaults() {
        assert_eq!(
            clamp_timeout(Some(5000), 30_000, 120_000),
            Duration::from_millis(5000)
        );
        assert_eq!(
            clamp_timeout(Some(0), 30_000, 120_000),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            clamp_timeout(Some(-5), 30_000, 120_000),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            clamp_timeout(None, 30_000, 120_000),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            clamp_timeout(Some(999_999), 30_000, 120_000),
            Duration::from_millis(120_000)
        );
    }

    #[test]
    fn clamp_usize_bounds_and_defaults() {
        assert_eq!(clamp_usize(Some(5), 10, 100), 5);
        assert_eq!(clamp_usize(Some(0), 10, 100), 10);
        assert_eq!(clamp_usize(Some(-3), 10, 100), 10);
        assert_eq!(clamp_usize(None, 10, 100), 10);
        assert_eq!(clamp_usize(Some(999), 10, 100), 100);
    }

    #[test]
    fn permission_title_is_the_hint_or_the_tool_name() {
        update_cached_page("title-key", "https://example.com/page".to_string(), false);
        let title = browser_permission_title(BROWSER_CLICK, &json!({"selector": "#ok"}));
        assert!(title.contains("browser_click"), "{title}");
        assert!(title.contains("#ok"), "{title}");
        assert_eq!(browser_permission_title("Read", &json!({})), "Read");
    }

    #[test]
    fn page_cache_round_trips() {
        update_cached_page("cache-key", "https://myhost.test/page".to_string(), true);
        let page = cached_page("cache-key").expect("cached page");
        assert_eq!(page.url, "https://myhost.test/page");
        assert_eq!(page.host, "myhost.test");
        assert!(page.has_password_field);
        // Host / password helpers read the most recently active session.
        assert_eq!(current_browser_host().as_deref(), Some("myhost.test"));
        assert!(current_browser_has_password());
        update_cached_page("cache-key-2", "https://other.test/".to_string(), false);
        assert_eq!(current_browser_host().as_deref(), Some("other.test"));
        assert!(!current_browser_has_password());
    }

    #[test]
    fn selector_flags_round_trip() {
        let flags = SelectorFlags {
            is_password_field: true,
            is_submit: true,
            is_destructive: false,
            in_password_form: true,
        };
        record_selector_flags("flags-key", "#login", flags);
        let back = selector_flags_for("flags-key", "#login").expect("flags stored");
        assert!(back.is_password_field);
        assert!(back.is_submit);
        assert!(!back.is_destructive);
        assert!(back.in_password_form);
        assert!(selector_flags_for("flags-key", "#other").is_none());
    }

    #[test]
    fn classify_browser_call_gate_branches() {
        // Cached submit flags outside a password form do not imply login.
        record_selector_flags(
            "class-key",
            "#plain-submit",
            SelectorFlags {
                is_password_field: false,
                is_submit: true,
                is_destructive: false,
                in_password_form: false,
            },
        );
        let (mark, login) = classify_browser_call(
            BROWSER_CLICK,
            &json!({"selector": "#plain-submit"}),
            "class-key",
        );
        assert!(!login, "submit outside a password form is not a login");
        assert!(mark.submit);
        assert!(!mark.login);

        // Typing on a page known to hold a password field is a login.
        update_cached_page("class-key-2", "https://example.com/login".to_string(), true);
        let (_mark, login) =
            classify_browser_call(BROWSER_TYPE, &json!({"selector": "#search"}), "class-key-2");
        assert!(login, "typing on a login page is a login");

        // ... but only for browser_type, not for a plain click ...
        let (_mark, login) = classify_browser_call(
            BROWSER_CLICK,
            &json!({"selector": "#search"}),
            "class-key-2",
        );
        assert!(!login, "clicking on a login page is not itself a login");

        // ... and only with a non-empty selector.
        let (_mark, login) =
            classify_browser_call(BROWSER_TYPE, &json!({"selector": ""}), "class-key-2");
        assert!(!login, "typing with no selector is not a login");

        // The current-session classifier reports the login mark.
        let (mark, login) =
            classify_current_browser_call(BROWSER_CLICK, &json!({"selector": "#password"}));
        assert!(login);
        assert!(mark.login);
    }

    #[test]
    fn tool_descriptions_are_exact() {
        assert_eq!(
            BrowserNavigateTool.description(),
            "Navigate the browser tab to a public http(s) URL; returns final URL, status, title, and text length."
        );
        assert_eq!(
            BrowserContentTool.description(),
            "Read rendered page text (after JS ran), or of one element. Text only, never HTML."
        );
        assert_eq!(
            BrowserScreenshotTool.description(),
            "Capture a PNG screenshot of the viewport or one element; saves it under the session work dir and returns the file path."
        );
        assert_eq!(
            BrowserWaitTool.description(),
            "Wait for an element or text to appear in the rendered page. Use exactly one of selector or text."
        );
        assert_eq!(
            BrowserClickTool.description(),
            "Click one element in the browser tab. Always asks for approval; login, submit, and destructive clicks need a separate approval."
        );
        assert_eq!(
            BrowserTypeTool.description(),
            "Type text into an input or textarea. Always asks for approval; never returns or logs the text. Password fields need a BrowserLogin approval."
        );
    }

    #[test]
    fn find_chromium_env_bin_and_path_lookup() {
        // (1) CHROMIUM_BIN wins outright.
        {
            let _guard = EnvGuard::set("CHROMIUM_BIN", "/usr/bin/chromium-test");
            assert_eq!(find_chromium().expect("env bin"), "/usr/bin/chromium-test");
        }
        // (2) Without it, the first `chromium` on PATH wins.
        let dir = std::env::temp_dir().join(format!(
            "zen-browser-which-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("chromium");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.display(), old_path);
        unsafe { std::env::set_var("PATH", &new_path) };
        let _env_bin = EnvGuard::set("CHROMIUM_BIN", "");
        let found = find_chromium().expect("fake chromium on PATH");
        unsafe { std::env::set_var("PATH", &old_path) };
        assert_eq!(found, fake.to_string_lossy().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn navigate_tool_validates_url_and_wait_until() {
        let (ctx, handle) = seeded_tool_ctx("tool-nav-1").await;
        let out = BrowserNavigateTool
            .call(
                json!({"url": "https://8.8.8.8/page", "wait_until": "load"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("navigated to https://example.com/page"),
            "{}",
            out.content
        );
        assert!(out.content.contains("status: 200"), "{}", out.content);

        let out = BrowserNavigateTool
            .call(
                json!({"url": "https://8.8.8.8/page", "wait_until": "networkidle"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);

        let out = BrowserNavigateTool
            .call(
                json!({"url": "https://8.8.8.8/page", "wait_until": "bogus"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("unsupported wait_until"),
            "{}",
            out.content
        );

        let out = BrowserNavigateTool
            .call(json!({"url": "ftp://example.com/x"}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("unsupported URL"), "{}", out.content);
        handle.abort();
    }

    #[tokio::test]
    async fn wait_tool_needs_exactly_one_condition() {
        let (ctx, handle) = seeded_tool_ctx("tool-wait-1").await;
        let out = BrowserWaitTool
            .call(json!({"selector": "#x", "timeout_ms": 1000}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("condition met: selector"),
            "{}",
            out.content
        );

        let out = BrowserWaitTool
            .call(json!({"selector": "#x", "text": "hello"}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("use only one of selector or text"),
            "{}",
            out.content
        );

        let out = BrowserWaitTool.call(json!({"timeout_ms": 100}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("no condition given"),
            "{}",
            out.content
        );
        handle.abort();
    }

    #[tokio::test]
    async fn navigate_request_ids_increase_and_load_skips_idle_wait() {
        let (mut s, seen, handle) = fake_session("fake-ids-1").await;
        let info = s
            .navigate("https://8.8.8.8/page", "load", Duration::from_secs(10))
            .await
            .expect("navigate");
        assert_eq!(info.status, 200);
        let ids = call_ids(&seen);
        assert!(ids.len() >= 5, "expected several CDP calls, got {ids:?}");
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "request ids must increase: {ids:?}"
        );
        // A plain "load" navigation never waits for network idle.
        assert_eq!(count_calls(&seen, "Runtime.callFunctionOn", "resource"), 0);
        // navigate refreshes the page cache.
        let page = cached_page("fake-ids-1").expect("page cache updated by navigate");
        assert_eq!(page.url, "https://example.com/page");
        assert!(page.has_password_field);
        handle.abort();
    }

    #[tokio::test]
    async fn wait_event_times_out_when_nothing_arrives() {
        let (mut s, _seen, handle) = fake_session("fake-wait-1").await;
        let err = s
            .cdp
            .wait_event("Page.loadEventFired", Duration::from_millis(300))
            .await
            .expect_err("no event arrives, must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        handle.abort();
    }

    #[tokio::test]
    async fn wait_event_reads_live_event_after_buffering_other_events() {
        let (ws_url, _seen, handle) = spawn_fake_server().await;
        let mut client = CdpClient::connect(&ws_url, None).await.expect("connect");
        client
            .call("Page.enable", json!({}), Duration::from_secs(5))
            .await
            .expect("Page.enable");
        // The fake server sends Page.frameNavigated, then Page.loadEventFired.
        client
            .wait_event("Page.loadEventFired", Duration::from_secs(5))
            .await
            .expect("load event");
        // The other event must have been buffered, not swallowed.
        let buffered: Vec<String> = client
            .events
            .iter()
            .filter_map(|e| e.get("method").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(buffered, vec!["Page.frameNavigated".to_string()]);
        handle.abort();
    }

    #[tokio::test]
    async fn exec_ctx_learned_only_from_main_frame_default_context() {
        let (mut s, seen, handle) = fake_session("fake-ctx-1").await;
        let ctx_of_last_probe = |seen: &Arc<StdMutex<Vec<Value>>>| last_call_ctx(seen);

        // Baseline: the seeded dummy context is used.
        s.probe_element("#x", Duration::from_secs(5))
            .await
            .expect("probe baseline");
        assert_eq!(ctx_of_last_probe(&seen), Some(1));

        // Main frame F1, then a default context from an iframe is ignored.
        s.cdp
            .on_message(json!({"method": "Page.frameNavigated", "params": {"frame": {"id": "F1"}}}))
            .await
            .expect("frame");
        s.cdp.on_message(json!({"method": "Runtime.executionContextCreated", "params": {"context": {"id": 42, "auxData": {"isDefault": true, "frameId": "F2"}}}})).await.expect("iframe ctx");
        s.probe_element("#x", Duration::from_secs(5))
            .await
            .expect("probe after iframe ctx");
        assert_eq!(ctx_of_last_probe(&seen), Some(1));

        // A non-default context of the main frame is ignored too.
        s.cdp.on_message(json!({"method": "Runtime.executionContextCreated", "params": {"context": {"id": 43, "auxData": {"isDefault": false, "frameId": "F1"}}}})).await.expect("non-default ctx");
        s.probe_element("#x", Duration::from_secs(5))
            .await
            .expect("probe after non-default ctx");
        assert_eq!(ctx_of_last_probe(&seen), Some(1));

        // The default context of the main frame is learned.
        s.cdp.on_message(json!({"method": "Runtime.executionContextCreated", "params": {"context": {"id": 44, "auxData": {"isDefault": true, "frameId": "F1"}}}})).await.expect("main ctx");
        s.probe_element("#x", Duration::from_secs(5))
            .await
            .expect("probe after main ctx");
        assert_eq!(ctx_of_last_probe(&seen), Some(44));

        // Destroying a different context leaves the learned one alone.
        s.cdp.on_message(json!({"method": "Runtime.executionContextDestroyed", "params": {"executionContextId": 7}})).await.expect("destroy other");
        s.probe_element("#x", Duration::from_secs(5))
            .await
            .expect("probe after destroying other");
        assert_eq!(ctx_of_last_probe(&seen), Some(44));

        // Destroying the learned context drops back to waiting (short timeout).
        s.cdp.on_message(json!({"method": "Runtime.executionContextDestroyed", "params": {"executionContextId": 44}})).await.expect("destroy learned");
        let err = s
            .probe_element("#x", Duration::from_millis(300))
            .await
            .expect_err("no context left, must time out");
        assert!(err.to_string().contains("timed out"), "{err}");

        // With no context, ensure_exec_ctx waits the full duration.
        let start = tokio::time::Instant::now();
        let err = s
            .cdp
            .ensure_exec_ctx(Duration::from_millis(500))
            .await
            .expect_err("still no context");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "ensure must wait, took {:?}",
            start.elapsed()
        );
        handle.abort();
    }

    #[tokio::test]
    async fn wait_network_idle_waits_for_stability() {
        let (mut s, seen, handle) = fake_session("fake-idle-1").await;
        let before = seen.lock().unwrap().len();
        s.wait_network_idle(Duration::from_secs(2))
            .await
            .expect("stable page goes idle");
        let idle_calls = count_calls_from(&seen, before, "Runtime.callFunctionOn", "resource");
        assert!(
            idle_calls >= 3,
            "expected several idle polls, got {idle_calls}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn wait_network_idle_never_stable_times_out() {
        let (mut s, _seen, handle) = fake_session_mode("fake-idle-2", true).await;
        let err = s
            .wait_network_idle(Duration::from_secs(1))
            .await
            .expect_err("request count never settles, must time out");
        assert!(
            err.to_string()
                .contains("timed out waiting for network idle"),
            "{err}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn wait_for_missing_selector_polls_until_deadline() {
        let (mut s, seen, handle) = fake_session("fake-wait-2").await;
        let before = seen.lock().unwrap().len();
        let err = s
            .wait_for(Some("#missing"), None, Duration::from_millis(600))
            .await
            .expect_err("missing selector must time out");
        assert!(
            err.to_string().contains("timed out waiting for selector"),
            "{err}"
        );
        let probes = count_calls_from(
            &seen,
            before,
            "Runtime.callFunctionOn",
            "getBoundingClientRect",
        );
        assert!(probes >= 2, "expected repeated probes, got {probes}");
        handle.abort();
    }

    #[tokio::test]
    async fn gate_mark_plain_submit_needs_only_submit_approval() {
        let (mut s, _seen, handle) = fake_session("fake-gate-plain").await;
        // #plain-submit is a submit button outside any password form:
        // no login is needed, only a separate submit approval.
        let err = s
            .click("#plain-submit", Duration::from_secs(10))
            .await
            .expect_err("plain submit click without a mark must refuse");
        let msg = err.to_string();
        assert!(msg.contains("a separate submit approval"), "{msg}");
        assert!(!msg.contains("BrowserLogin approval"), "{msg}");
        handle.abort();
    }

    #[tokio::test]
    async fn gate_mark_submit_with_both_marks_clicks() {
        let (mut s, _seen, handle) = fake_session("fake-gate-marks").await;
        record_gate_mark(
            "fake-gate-marks",
            "#submit",
            GateMark {
                login: true,
                submit: true,
                destructive: false,
            },
        );
        let (text, _url) = s
            .click("#submit", Duration::from_secs(10))
            .await
            .expect("submit click with both marks");
        assert_eq!(text, "Log in");
        handle.abort();
    }

    #[tokio::test]
    async fn click_dispatches_at_element_center() {
        let (mut s, seen, handle) = fake_session("fake-click-xy").await;
        let (text, _url) = s
            .click("#x", Duration::from_secs(10))
            .await
            .expect("plain click");
        assert_eq!(text, "hello");
        let coords = dispatch_coords(&seen);
        assert_eq!(coords.len(), 2, "pressed + released, got {coords:?}");
        for (x, y) in coords {
            assert_eq!(x, 50.0, "cx = 10 + 80/2");
            assert_eq!(y, 25.0, "cy = 10 + 30/2");
        }
        handle.abort();
    }

    #[tokio::test]
    async fn screenshot_element_and_numbering() {
        let (mut s, _seen, handle) = fake_session("fake-shot-1").await;
        let (path1, size1, _, _) = s
            .screenshot(Some("#x"), false, Duration::from_secs(10))
            .await
            .expect("element screenshot");
        assert!(size1 > 0);
        assert!(path1.exists());
        assert!(
            path1
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("screenshot-1.png")
        );
        let (path2, _, _, _) = s
            .screenshot(None, false, Duration::from_secs(10))
            .await
            .expect("viewport screenshot");
        assert!(
            path2
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("screenshot-2.png")
        );
        assert_ne!(path1, path2);
        handle.abort();
    }

    #[tokio::test]
    async fn session_close_closes_the_socket() {
        let (s, _seen, handle) = fake_session("fake-close-1").await;
        s.close().await;
        sleep(Duration::from_millis(500)).await;
        assert!(
            handle.is_finished(),
            "fake server must see the socket close"
        );
    }

    #[tokio::test]
    async fn session_drop_removes_the_profile_dir() {
        let dir = std::env::temp_dir().join(format!(
            "zen-browser-drop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (ws_url, _seen, handle) = spawn_fake_server().await;
        let cdp = CdpClient::connect(&ws_url, None).await.expect("connect");
        let session = BrowserSession {
            cdp,
            child: None,
            profile_dir: Some(dir.clone()),
            work_dir: std::env::temp_dir(),
            allow_private_network: false,
            pinned: Arc::new(crate::builtin::PinnedResolver::default()),
            shot_count: 0,
            current_url: String::new(),
            session_key: "drop-test".to_string(),
        };
        drop(session);
        assert!(!dir.exists(), "profile dir must be removed on drop");
        handle.abort();
    }

    #[tokio::test]
    async fn free_port_returns_a_bindable_port() {
        let port = free_port().await.expect("free port");
        assert!(port > 1024, "ephemeral port, got {port}");
        let _listener = TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .expect("port must be bindable");
    }

    #[tokio::test]
    async fn wait_for_version_retries_until_success() {
        let port = run_version_server(
            (500, r#"{"error": "not ready yet"}"#),
            (200, r#"{"Browser": "HeadlessChrome/155.0", "Protocol-Version": "1.3", "webSocketDebuggerUrl": "ws://127.0.0.1:9/devtools/browser/ABC"}"#),
        ).await;
        let client = reqwest::Client::new();
        let v = wait_for_version(&client, port)
            .await
            .expect("version after one retry");
        assert_eq!(
            v.get("Browser").and_then(Value::as_str),
            Some("HeadlessChrome/155.0")
        );
        assert!(
            v.get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(|u| u.starts_with("ws://"))
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn create_target_returns_the_debugger_url() {
        let port = run_version_server(
            (
                200,
                r#"{"webSocketDebuggerUrl": "ws://127.0.0.1:9/devtools/page/ABC123"}"#,
            ),
            (
                200,
                r#"{"webSocketDebuggerUrl": "ws://127.0.0.1:9/devtools/page/ABC123"}"#,
            ),
        )
        .await;
        let client = reqwest::Client::new();
        let url = create_target(&client, port).await.expect("target url");
        assert_eq!(url, "ws://127.0.0.1:9/devtools/page/ABC123");
    }
}
