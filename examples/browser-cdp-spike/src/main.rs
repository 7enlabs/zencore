// browser-cdp-spike: drive headless Chromium from Rust over raw CDP.
//
// Proves, with no other browser library, that Rust can:
//   1. launch system Chromium headless with a remote-debugging port,
//   2. navigate to a page whose content only appears after JS runs,
//   3. read the post-JS text via Runtime.evaluate,
//   4. capture a real PNG screenshot via Page.captureScreenshot.
//
// Public page only, no credentials, no login. Companion to
// docs/browser-agent-spec.md (Chặng 1, spec + spike, no feature implementation).
//
// Run: cargo run --release [-- <url>] [--out <png path>]
// Defaults: url https://quotes.toscrape.com/js/, out ./screenshot.png

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_URL: &str = "https://quotes.toscrape.com/js/";
const DEFAULT_OUT: &str = "screenshot.png";

fn find_chromium() -> Result<String> {
    if let Ok(bin) = std::env::var("CHROMIUM_BIN") {
        if !bin.is_empty() {
            return Ok(bin);
        }
    }
    for cand in ["chromium", "chromium-browser", "google-chrome"] {
        if let Ok(out) = std::process::Command::new("which").arg(cand).output() {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() {
                    return Ok(p);
                }
            }
        }
    }
    bail!("no Chromium binary found (set CHROMIUM_BIN or install the `chromium` package)")
}

async fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    Ok(l.local_addr()?.port())
}

fn launch_chromium(bin: &str, port: u16, profile: &std::path::Path) -> Result<Child> {
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
                return r.json().await.context("parse /json/version")
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
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in /json/new response: {target}"))
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Cdp {
    ws: Ws,
    next_id: u64,
    events: VecDeque<Value>,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .context("connect CDP websocket")?;
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
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

    /// Send a method and wait for its reply (matching id). Events seen on the
    /// way are buffered for wait_event().
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(req.to_string().into()))
            .await
            .context("send CDP message")?;
        loop {
            let msg = self.next_msg(Duration::from_secs(60)).await?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("CDP {method} failed: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            if msg.get("method").is_some() {
                self.events.push_back(msg);
            }
        }
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
            if msg.get("method").is_some() {
                self.events.push_back(msg);
            }
        }
    }

    /// Evaluate JS in the page, return the value (returnByValue).
    async fn eval(&mut self, expr: &str) -> Result<Value> {
        let r = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expr, "returnByValue": true}),
            )
            .await?;
        // call() already returns msg["result"], so the value lives at
        // result.result.value (Runtime.evaluate result object -> remote object).
        let v = r
            .get("result")
            .and_then(|x| x.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        if r.get("exceptionDetails").is_some() {
            bail!("JS exception evaluating {expr}: {r}");
        }
        Ok(v)
    }
}

async fn run(url: &str, out: &std::path::Path) -> Result<()> {
    let chromium = find_chromium()?;
    let version_out = std::process::Command::new(&chromium)
        .arg("--version")
        .output()
        .context("chromium --version")?;
    let chromium_version = String::from_utf8_lossy(&version_out.stdout)
        .trim()
        .to_string();
    println!("chromium: {chromium} ({chromium_version})");

    let port = free_port().await?;
    let profile = std::env::temp_dir().join(format!("browser-cdp-spike-{}", std::process::id()));
    let mut child = launch_chromium(&chromium, port, &profile)?;
    println!("launched headless Chromium, remote-debugging-port={port}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let res = async {
        let version = wait_for_version(&client, port).await?;
        println!(
            "CDP: browser={} protocol={} v8={}",
            version
                .get("Browser")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            version
                .get("Protocol-Version")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            version
                .get("V8-Version")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
        let ws_url = create_target(&client, port).await?;
        let mut cdp = Cdp::connect(&ws_url).await?;
        cdp.call("Page.enable", json!({})).await?;
        cdp.call("Page.navigate", json!({"url": url})).await?;
        println!("navigated to {url}");
        cdp.wait_event("Page.loadEventFired", Duration::from_secs(30))
            .await?;
        println!("page load event fired");

        // The quotes on this page are rendered by JS only. Poll until they exist.
        let mut quotes = 0u64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            quotes = cdp
                .eval("document.querySelectorAll('.quote').length")
                .await?
                .as_u64()
                .unwrap_or(0);
            if quotes > 0 {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
        // Diagnostics if the page did not render: they show whether JS ran at all.
        let ready = cdp.eval("document.readyState").await?;
        let has_jquery = cdp.eval("typeof jQuery").await?;
        let body_len = cdp
            .eval("document.body ? document.body.innerText.length : -1")
            .await?;
        println!("diagnostics: readyState={ready} jQuery={has_jquery} innerText_len={body_len}");
        println!("JS-rendered quotes found: {quotes}");
        if quotes == 0 {
            bail!("no .quote elements after JS ran - page did not render as expected");
        }

        let text = cdp
            .eval("document.body.innerText.slice(0, 2000)")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string();
        println!("--- innerText (first 500 chars) ---");
        println!("{}", text.chars().take(500).collect::<String>());
        println!(
            "--- end innerText, total {} chars ---",
            text.chars().count()
        );
        if text.trim().is_empty() {
            bail!("innerText is empty after JS ran");
        }

        let shot = cdp
            .call("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        let data = shot
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no data in captureScreenshot result"))?;
        let png = B64.decode(data).context("decode screenshot base64")?;
        std::fs::write(out, &png).with_context(|| format!("write {}", out.display()))?;
        println!("screenshot: {} ({} bytes)", out.display(), png.len());
        if png.len() < 10_000 {
            bail!("screenshot suspiciously small ({} bytes)", png.len());
        }
        // PNG magic, so the file is a real PNG and not an error blob.
        if png[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
            bail!("screenshot is not a PNG");
        }
        println!("OK: navigate + JS text + screenshot all worked");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Cleanup: kill the browser and remove the throwaway profile either way.
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&profile);
    res
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut url = DEFAULT_URL.to_string();
    let mut out = PathBuf::from(DEFAULT_OUT);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = PathBuf::from(args.get(i).context("--out needs a path")?);
            }
            other => url = other.to_string(),
        }
        i += 1;
    }
    run(&url, &out).await
}
