use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use zen_core::{DiagnosticsProvider, Tool, ToolContext};

use crate::lsp::LspClient;
use crate::lsp::{
    LspConnection, LspLanguage, LspManager, col_to_utf16, detect_language, find_binary,
    find_project_root, is_lsp_transport_error, parse_locations, parse_publish_diagnostics,
    path_to_uri, render_hover, server_key, server_spec, uri_to_path, utf16_to_col,
};
use crate::lsp::{MAX_FRAME_BYTES, MAX_HEADER_BYTES, read_frame, write_frame};
use crate::lsp_tools::{DiagnosticsTool, FindReferencesTool, GotoDefinitionTool, HoverTool};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

static PATH_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard restoring one env var's previous value on drop
/// (panic-safe). Same shape as the EnvGuard in zen-cli's settings
/// module, duplicated here because that guard is `pub(crate)` to zen-cli.
#[must_use = "a guard dropped immediately restores the var at once; bind it for the whole test"]
struct EnvGuard {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}

impl EnvGuard {
    /// Capture the current value of `key`, then set it to `value`.
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old = std::env::var_os(key);
        // SAFETY: callers hold PATH_LOCK while mutating env.
        unsafe { std::env::set_var(key, value) };
        Self { key, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same as `set` above.
        unsafe {
            match self.old.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn unique_tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zen-lsp-test-{}-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::SeqCst),
        tag
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

const MOCK_SERVER: &str = r##"#!/usr/bin/env python3
import sys, json

log_path, target_uri = sys.argv[1], sys.argv[2]

def log(method):
    with open(log_path, "a") as f:
        f.write(method + "\n")

def read_msg():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

def send(obj):
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None:
        break
    method, mid = msg.get("method", ""), msg.get("id")
    log(method)
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "capabilities": {"textDocumentSync": 2, "definitionProvider": True,
                             "hoverProvider": True, "referencesProvider": True}}})
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "uri": target_uri,
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}})
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "contents": {"kind": "markdown", "value": "**mock** hover"}}})
    elif method == "textDocument/references":
        send({"jsonrpc": "2.0", "id": mid, "result": []})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    # notifications (initialized / didOpen / didChange) and unknown methods: no reply
"##;

/// Mock server that answers every didOpen/didChange with a
/// `publishDiagnostics` batch derived from the file text: a line
/// containing `ERROR_ONE` yields an Error diagnostic, `ERROR_TWO` a
/// Warning, `INFO_THREE` an Information and `HINT_FOUR` a Hint.
/// Text without markers yields an empty batch (which must
/// clear previously published diagnostics for the URI).
const MOCK_SERVER_DIAG: &str = r##"#!/usr/bin/env python3
import sys, json

log_path, target_uri = sys.argv[1], sys.argv[2]

def log(method):
    with open(log_path, "a") as f:
        f.write(method + "\n")

def read_msg():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

def send(obj):
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

def diagnostics_for(text, uri):
    diags = []
    for i, line in enumerate(text.split("\n")):
        if "ERROR_ONE" in line:
            diags.append({"range": {"start": {"line": i, "character": 4},
                                    "end": {"line": i, "character": 9}},
                          "severity": 1, "message": "mock error one", "source": "mock"})
        if "ERROR_TWO" in line:
            diags.append({"range": {"start": {"line": i, "character": 0},
                                    "end": {"line": i, "character": 5}},
                          "severity": 2, "message": "mock warning two", "source": "mock"})
        if "INFO_THREE" in line:
            diags.append({"range": {"start": {"line": i, "character": 0},
                                    "end": {"line": i, "character": 5}},
                          "severity": 3, "message": "mock info three", "source": "mock"})
        if "HINT_FOUR" in line:
            diags.append({"range": {"start": {"line": i, "character": 0},
                                    "end": {"line": i, "character": 5}},
                          "severity": 4, "message": "mock hint four", "source": "mock"})
    return {"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": diags}}

while True:
    msg = read_msg()
    if msg is None:
        break
    method, mid = msg.get("method", ""), msg.get("id")
    log(method)
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "capabilities": {"textDocumentSync": 2}}})
    elif method == "textDocument/didOpen":
        td = msg["params"]["textDocument"]
        send(diagnostics_for(td["text"], td["uri"]))
    elif method == "textDocument/didChange":
        td = msg["params"]["textDocument"]
        text = msg["params"]["contentChanges"][0]["text"]
        send(diagnostics_for(text, td["uri"]))
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
"##;

/// Mock server that exits immediately: the client must treat the dead
/// server as "no diagnostics", never as a hang or a failed run.
const MOCK_SERVER_EXIT: &str = r##"#!/usr/bin/env python3
import sys
sys.exit(0)
"##;

/// Mock server that answers one `textDocument/definition` request and
/// then exits (#387): the manager must notice the corpse, evict it,
/// and serve the next call from a fresh server instead of failing
/// every later call until the process restarts.
/// Mock server that dies after answering its first textDocument
/// request (#387), but only on its first start: later starts see the
/// marker file (`<log>.died`) and stay alive. The first server's death
/// exercises the evict + respawn path; the respawned server must stay up
/// so the retry can succeed on it (a mock that died on every start
/// would make every retry fail too). The one answered request may be a
/// definition, a hover or a references call, so the same mock covers
/// the retry arm of all three methods.
const MOCK_SERVER_DIES_ONCE: &str = r##"#!/usr/bin/env python3
import sys, json, os

log_path, target_uri = sys.argv[1], sys.argv[2]
marker = log_path + ".died"

def log(method):
    with open(log_path, "a") as f:
        f.write(method + "\n")

def read_msg():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

def send(obj):
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

dies_once = not os.path.exists(marker)
answered = 0
while True:
    msg = read_msg()
    if msg is None:
        break
    method, mid = msg.get("method", ""), msg.get("id")
    log(method)
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "capabilities": {"textDocumentSync": 2, "definitionProvider": True,
                             "hoverProvider": True, "referencesProvider": True}}})
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "uri": target_uri,
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}})
        answered += 1
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "contents": {"kind": "markdown", "value": "**mock** hover"}}})
        answered += 1
    elif method == "textDocument/references":
        send({"jsonrpc": "2.0", "id": mid, "result": [{
            "uri": target_uri,
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}]})
        answered += 1
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    if dies_once and answered >= 1:
        with open(marker, "w") as f:
            f.write("died\n")
        sys.exit(0)
    # notifications (initialized / didOpen / didChange) and unknown methods: no reply
"##;

/// Mock server whose behaviour depends on how many times it has been
/// started (#387): starts listed in argv[3] exit at once, starts listed
/// in argv[4] answer one request then exit, all other starts stay alive.
/// The start counter lives in `<log>.starts`, so every fresh process in
/// the same test sees the running total. Used to build an exact failure
/// streak (deaths, then a success, then more deaths) for the backoff
/// tests.
const MOCK_SERVER_FLAKY: &str = r##"#!/usr/bin/env python3
import sys, json

log_path, target_uri = sys.argv[1], sys.argv[2]
die_at_start = set(sys.argv[3].split(",")) if len(sys.argv) > 3 and sys.argv[3] else set()
die_after_one = set(sys.argv[4].split(",")) if len(sys.argv) > 4 and sys.argv[4] else set()
counter_path = log_path + ".starts"

def log(method):
    with open(log_path, "a") as f:
        f.write(method + "\n")

try:
    with open(counter_path) as f:
        n = int(f.read().strip() or 0)
except FileNotFoundError:
    n = 0
n += 1
with open(counter_path, "w") as f:
    f.write(str(n))

if str(n) in die_at_start:
    log("started")
    sys.exit(0)

def read_msg():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

def send(obj):
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

dies_once = str(n) in die_after_one
answered = 0
while True:
    msg = read_msg()
    if msg is None:
        break
    method, mid = msg.get("method", ""), msg.get("id")
    log(method)
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "capabilities": {"textDocumentSync": 2, "definitionProvider": True,
                             "hoverProvider": True, "referencesProvider": True}}})
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "uri": target_uri,
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}})
        answered += 1
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "contents": {"kind": "markdown", "value": "**mock** hover"}}})
        answered += 1
    elif method == "textDocument/references":
        send({"jsonrpc": "2.0", "id": mid, "result": [{
            "uri": target_uri,
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}]})
        answered += 1
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    if dies_once and answered >= 1:
        sys.exit(0)
    # notifications (initialized / didOpen / didChange) and unknown methods: no reply
"##;

/// Mock server that logs one `started` line and exits at once (#387):
/// every start dies immediately, so repeated calls must hit the
/// respawn backoff instead of spawning forever.
const MOCK_SERVER_DIES_AT_START: &str = r##"#!/usr/bin/env python3
import sys
with open(sys.argv[1], "a") as f:
    f.write("started\n")
sys.exit(0)
"##;

struct MockSetup {
    manager: LspManager,
    _tmp: PathBuf,
    main_rs: PathBuf,
    target_rs: PathBuf,
    log: PathBuf,
}

fn mock_setup() -> Option<MockSetup> {
    let py = find_binary("python3")?;
    let tmp = unique_tmpdir("mock");
    let main_rs = tmp.join("main.rs");
    write_file(
        &main_rs,
        "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
    );
    let target_rs = tmp.join("target.rs");
    let mut target_src = String::new();
    for i in 1..=9 {
        target_src.push_str(&format!("// line {i}\n"));
    }
    target_src.push_str("    pub fn target_fn() {}\n");
    write_file(&target_rs, &target_src);
    let script = tmp.join("mock_lsp.py");
    write_file(&script, MOCK_SERVER);
    let log = tmp.join("methods.log");
    write_file(&log, "");

    let manager = LspManager::new();
    manager.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            path_to_uri(&target_rs),
        ],
    );
    Some(MockSetup {
        manager,
        _tmp: tmp,
        main_rs,
        target_rs,
        log,
    })
}

/// Same files and command override as `mock_setup`, but the server
/// dies after answering one request, first start only (#387).
fn dying_mock_setup() -> Option<MockSetup> {
    let py = find_binary("python3")?;
    let tmp = unique_tmpdir("mock-dying");
    let main_rs = tmp.join("main.rs");
    write_file(&main_rs, "fn main() {\n    let x = 1;\n}\n");
    let target_rs = tmp.join("target.rs");
    write_file(&target_rs, "pub fn target_fn() {}\n");
    let script = tmp.join("mock_dying_lsp.py");
    write_file(&script, MOCK_SERVER_DIES_ONCE);
    let log = tmp.join("methods.log");
    write_file(&log, "");

    let manager = LspManager::new();
    manager.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            path_to_uri(&target_rs),
        ],
    );
    Some(MockSetup {
        manager,
        _tmp: tmp,
        main_rs,
        target_rs,
        log,
    })
}

/// Same files and command override as `mock_setup`, but the server is
/// the flaky mock (#387): `die_at_start` and `die_after_one` are
/// comma-separated start numbers (see `MOCK_SERVER_FLAKY`).
fn flaky_mock_setup(die_at_start: &str, die_after_one: &str) -> Option<MockSetup> {
    let py = find_binary("python3")?;
    let tmp = unique_tmpdir("mock-flaky");
    let main_rs = tmp.join("main.rs");
    write_file(&main_rs, "fn main() {\n    let x = 1;\n}\n");
    let target_rs = tmp.join("target.rs");
    write_file(&target_rs, "pub fn target_fn() {}\n");
    let script = tmp.join("mock_flaky_lsp.py");
    write_file(&script, MOCK_SERVER_FLAKY);
    let log = tmp.join("methods.log");
    write_file(&log, "");

    let manager = LspManager::new();
    manager.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            path_to_uri(&target_rs),
            die_at_start.to_owned(),
            die_after_one.to_owned(),
        ],
    );
    Some(MockSetup {
        manager,
        _tmp: tmp,
        main_rs,
        target_rs,
        log,
    })
}

/// Same shape as `mock_setup`, but the server logs one `started` line
/// and exits at once (#387). No target file is needed: the server
/// never answers a request.
fn dies_at_start_setup() -> Option<MockSetup> {
    let py = find_binary("python3")?;
    let tmp = unique_tmpdir("mock-dies-at-start");
    let main_rs = tmp.join("main.rs");
    write_file(&main_rs, "fn main() {\n    let x = 1;\n}\n");
    let target_rs = tmp.join("target.rs");
    write_file(&target_rs, "pub fn target_fn() {}\n");
    let script = tmp.join("mock_dies_at_start.py");
    write_file(&script, MOCK_SERVER_DIES_AT_START);
    let log = tmp.join("methods.log");
    write_file(&log, "");

    let manager = LspManager::new();
    manager.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
        ],
    );
    Some(MockSetup {
        manager,
        _tmp: tmp,
        main_rs,
        target_rs,
        log,
    })
}

fn mock_diag_setup() -> Option<MockSetup> {
    let py = find_binary("python3")?;
    let tmp = unique_tmpdir("mock-diag");
    let main_rs = tmp.join("main.rs");
    write_file(&main_rs, "fn main() {\n    let x = 1;\n}\n");
    let target_rs = tmp.join("target.rs");
    write_file(&target_rs, "pub fn target_fn() {}\n");
    let script = tmp.join("mock_lsp.py");
    write_file(&script, MOCK_SERVER_DIAG);
    let log = tmp.join("methods.log");
    write_file(&log, "");

    let manager = LspManager::new();
    manager.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            path_to_uri(&target_rs),
        ],
    );
    // Short timings so the wait tests run fast: cap 500ms, debounce 50ms.
    manager.set_diagnostics_timing(Duration::from_millis(500), Duration::from_millis(50));
    Some(MockSetup {
        manager,
        _tmp: tmp,
        main_rs,
        target_rs,
        log,
    })
}

/// Rewrite a file until its mtime moves, so the next LSP sync sends
/// `didChange` instead of reusing the cached document.
fn rewrite_moved(path: &Path, content: &str) {
    let old_mtime = std::fs::metadata(path).unwrap().modified().unwrap();
    for _ in 0..50 {
        write_file(path, content);
        let new_mtime = std::fs::metadata(path).unwrap().modified().unwrap();
        if new_mtime != old_mtime {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("mtime did not move after 50 rewrites of {}", path.display());
}

fn logged_methods(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count(methods: &[String], name: &str) -> usize {
    methods.iter().filter(|m| m.as_str() == name).count()
}

#[tokio::test]
async fn frame_roundtrip_over_duplex() {
    let (mut a, mut b) = tokio::io::duplex(65536);
    let first = json!({"jsonrpc": "2.0", "id": 7, "method": "ping"});
    let second = json!({"jsonrpc": "2.0", "id": 8, "method": "pong", "params": {"é": "😀"}});
    write_frame(&mut a, &first).await.unwrap();
    write_frame(&mut a, &second).await.unwrap();

    let mut reader = BufReader::new(&mut b);
    let got1 = read_frame(&mut reader).await.unwrap();
    let got2 = read_frame(&mut reader).await.unwrap();
    assert_eq!(got1, first);
    assert_eq!(got2, second);
}

#[tokio::test]
async fn read_frame_ignores_extra_headers_and_case() {
    let (mut a, mut b) = tokio::io::duplex(65536);
    let body = br#"{"ok":true}"#;
    let raw = format!(
        "content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
        body.len()
    );
    a.write_all(raw.as_bytes()).await.unwrap();
    a.write_all(body).await.unwrap();
    let got = read_frame(&mut BufReader::new(&mut b)).await.unwrap();
    assert_eq!(got, json!({"ok": true}));
}

#[tokio::test]
async fn read_frame_errors_without_content_length() {
    let (mut a, mut b) = tokio::io::duplex(65536);
    a.write_all(b"Content-Type: application/json\r\n\r\n{}")
        .await
        .unwrap();
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(err.to_string().contains("Content-Length"), "got: {err}");
}

#[tokio::test]
async fn read_frame_errors_on_huge_content_length() {
    // Issue #277: a 100 GB Content-Length used to make `read_frame` try
    // to allocate the whole body up front and abort the process
    // (SIGABRT) instead of returning an error.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        a.write_all(b"Content-Length: 107374182400\r\n\r\n{")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("exceeds the {MAX_FRAME_BYTES} byte limit")),
        "got: {err}"
    );
}

#[tokio::test]
async fn read_frame_errors_one_byte_over_cap() {
    // Boundary: cap + 1 is rejected without allocating the body.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        a.write_all(format!("Content-Length: {}\r\n\r\n{{", MAX_FRAME_BYTES + 1).as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("exceeds the {MAX_FRAME_BYTES} byte limit")),
        "got: {err}"
    );
}

#[tokio::test]
async fn read_frame_accepts_frame_at_cap() {
    // A frame of exactly MAX_FRAME_BYTES must still read fine; only
    // frames *over* the cap are rejected (issue #277). The writer runs
    // in a task because the 10 MiB body does not fit the 64 KiB duplex
    // buffer and would deadlock a sequential write.
    let (mut a, mut b) = tokio::io::duplex(65536);
    let body = format!("{{\"data\":\"{}\"}}", "x".repeat(MAX_FRAME_BYTES - 11));
    assert_eq!(body.len(), MAX_FRAME_BYTES);
    let len = body.len();
    tokio::spawn(async move {
        a.write_all(format!("Content-Length: {len}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        a.write_all(body.as_bytes()).await.unwrap();
    });
    let got = read_frame(&mut BufReader::new(&mut b)).await.unwrap();
    assert_eq!(got["data"].as_str().unwrap().len(), MAX_FRAME_BYTES - 11);
}

#[tokio::test]
async fn read_frame_cap_is_ten_mib_written_literally() {
    // The cap is 10 MiB = 10485760 bytes, written literally here. The
    // earlier cap tests derive their sizes from MAX_FRAME_BYTES, so a
    // mutant changing the constant changed the test inputs too and
    // survived (issue #277 follow-up).
    let (mut a, mut b) = tokio::io::duplex(65536);
    let body = format!("{{\"data\":\"{}\"}}", "x".repeat(10_485_760 - 11));
    assert_eq!(body.len(), 10_485_760);
    let len = body.len();
    tokio::spawn(async move {
        a.write_all(format!("Content-Length: {len}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        a.write_all(body.as_bytes()).await.unwrap();
    });
    let got = read_frame(&mut BufReader::new(&mut b)).await.unwrap();
    assert_eq!(got["data"].as_str().unwrap().len(), 10_485_760 - 11);
}

#[tokio::test]
async fn read_frame_rejects_one_byte_over_ten_mib_written_literally() {
    // Boundary with a literal size, so the error message names the real
    // 10485760-byte limit even if the constant is mutated.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        a.write_all(b"Content-Length: 10485761\r\n\r\n{")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(
        err.to_string().contains("exceeds the 10485760 byte limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn read_frame_rejects_endless_header_lines() {
    // Issue #385: the header loop read lines with no limit, so a
    // server sending endless short header lines kept the reader
    // looping (and allocating) forever instead of returning an error.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        for _ in 0..100 {
            a.write_all(b"X-Pad: x\r\n").await.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(err.to_string().contains("32 header lines"), "got: {err}");
}

#[tokio::test]
async fn read_frame_rejects_oversize_header_line() {
    // Issue #385: a single header line had no length limit, so a
    // server could force an unbounded allocation with one huge line
    // before the body cap ever applied. The line here (64 KiB) is
    // far over the 8192-byte header cap.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        let line = format!("X-Pad: {}\r\n", "x".repeat(65536));
        a.write_all(line.as_bytes()).await.unwrap();
        a.write_all(b"\r\n{}").await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("headers exceed the 8192 byte limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn read_frame_accepts_large_but_capped_headers() {
    // Boundary: headers well under the cap must still read fine, so
    // the cap does not reject legitimate frames with long extra
    // headers (e.g. a long Content-Type value).
    let (mut a, mut b) = tokio::io::duplex(65536);
    let body = br#"{"ok":true}"#;
    let raw = format!(
        "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8; {}\r\n\r\n",
        body.len(),
        "x".repeat(MAX_HEADER_BYTES / 2)
    );
    a.write_all(raw.as_bytes()).await.unwrap();
    a.write_all(body).await.unwrap();
    let got = read_frame(&mut BufReader::new(&mut b)).await.unwrap();
    assert_eq!(got, json!({"ok": true}));
}

#[tokio::test]
async fn read_frame_accepts_max_header_lines() {
    // Issue #385, mutant kill: the line-count check is `header_lines
    // > MAX_HEADER_LINES` (32). A frame with 31 header lines plus the
    // empty terminator line uses exactly 32 loop iterations, so it
    // must still read fine; `>=` or `==` would reject the terminator
    // iteration instead.
    let (mut a, mut b) = tokio::io::duplex(65536);
    let body = br#"{"ok":true}"#;
    let mut raw = format!("Content-Length: {}\r\n", body.len());
    for _ in 0..30 {
        raw.push_str("X-Pad: x\r\n");
    }
    raw.push_str("\r\n");
    a.write_all(raw.as_bytes()).await.unwrap();
    a.write_all(body).await.unwrap();
    let got = read_frame(&mut BufReader::new(&mut b)).await.unwrap();
    assert_eq!(got, json!({"ok": true}));
}

#[tokio::test]
async fn read_frame_rejects_headers_over_cap_across_lines() {
    // Issue #385, mutant kill: the per-read cap is
    // `MAX_HEADER_BYTES - header_bytes`, so the cap applies to the
    // TOTAL of all header lines, not to each line alone. Two ~5 KiB
    // pad lines total ~10 KiB, over the 8192-byte cap, even though
    // neither line alone exceeds it; `+` instead of `-` would let
    // the second line through and accept the frame.
    let (mut a, mut b) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        let body = br#"{"ok":true}"#;
        let mut raw = format!("Content-Length: {}\r\n", body.len());
        for _ in 0..2 {
            raw.push_str(&format!("X-Pad: {}\r\n", "x".repeat(4992)));
        }
        raw.push_str("\r\n");
        a.write_all(raw.as_bytes()).await.unwrap();
        a.write_all(body).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("headers exceed the 8192 byte limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn read_frame_errors_when_server_closes_mid_headers() {
    // Issue #385, mutant kill: when the server closes the connection
    // mid-headers (EOF with the bytes seen so far under the cap), the
    // error must say the server closed, not that the headers exceeded
    // the cap; `<` instead of `>=` in the EOF branch swaps the two.
    let (mut a, mut b) = tokio::io::duplex(65536);
    a.write_all(b"Content-Length: 11\r\n").await.unwrap();
    drop(a);
    let err = read_frame(&mut BufReader::new(&mut b)).await.unwrap_err();
    assert!(err.to_string().contains("server closed"), "got: {err}");
}

#[tokio::test]
async fn mock_server_full_roundtrip() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "expected one definition, got {locs:?}");
    let loc = &locs[0];
    assert_eq!(loc.path, setup.target_rs.to_string_lossy());
    assert_eq!(loc.line, 10);
    assert_eq!(loc.col, 5);
    assert_eq!(loc.end_col, 13);

    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        1,
        "handshake must run once: {methods:?}"
    );
    assert!(
        methods.contains(&"initialized".to_string()),
        "initialized notification missing: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/didOpen"), 1);
    assert_eq!(count(&methods, "textDocument/definition"), 1);

    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 2, 9)
        .await
        .unwrap();
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        1,
        "server must stay alive: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/didOpen"), 1);
    assert_eq!(count(&methods, "textDocument/definition"), 2);
}

#[tokio::test]
async fn mock_server_sends_did_change_when_mtime_moves() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let old_mtime = std::fs::metadata(&setup.main_rs)
        .unwrap()
        .modified()
        .unwrap();

    for _ in 0..50 {
        write_file(&setup.main_rs, "fn main() {\n    let y = 2;\n}\n");
        let new_mtime = std::fs::metadata(&setup.main_rs)
            .unwrap()
            .modified()
            .unwrap();
        if new_mtime != old_mtime {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "textDocument/didChange"),
        1,
        "expected one didChange after mtime moved: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/didOpen"), 1);
}

#[tokio::test]
async fn mock_server_sends_did_change_when_content_changes_but_mtime_does_not() {
    // Issue #383: `ensure_open` keyed the sync decision on mtime only,
    // so a rewrite that left the mtime unchanged (coarse filesystem
    // granularity, or a restore of the old mtime) silently kept the
    // stale cached text and never sent `didChange`. Restoring the old
    // mtime after the rewrite forces the mtime-equal case.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let old_mtime = std::fs::metadata(&setup.main_rs)
        .unwrap()
        .modified()
        .unwrap();

    write_file(&setup.main_rs, "fn main() {\n    let y = 2;\n}\n");
    std::fs::File::options()
        .write(true)
        .open(&setup.main_rs)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old_mtime))
        .unwrap();
    let restored_mtime = std::fs::metadata(&setup.main_rs)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        restored_mtime, old_mtime,
        "test setup failed: mtime moved despite the restore"
    );

    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "textDocument/didChange"),
        1,
        "expected one didChange after content changed at the same mtime: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/didOpen"), 1);
}

#[tokio::test]
async fn mock_server_reuses_cached_doc_when_nothing_changed() {
    // Issue #383, mutant kill: `ensure_open` reuses the cached document
    // only when BOTH the mtime and the freshly read text match
    // (`doc.mtime == mtime && doc.text == text`). A second request with
    // the file untouched must send neither `didOpen` nor `didChange`;
    // replacing the guard with `false`, or flipping either `==` to
    // `!=`, would send a spurious `didChange` instead.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "textDocument/didOpen"),
        1,
        "second request must reuse the open document: {methods:?}"
    );
    assert_eq!(
        count(&methods, "textDocument/didChange"),
        0,
        "nothing changed, so no didChange may be sent: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/definition"), 2);
}

#[tokio::test]
async fn mock_server_non_utf8_file_still_answers() {
    // Issue #384: `ensure_open` used `read_to_string`, so a source
    // file containing invalid UTF-8 failed every LSP operation with
    // "stream did not contain valid UTF-8" before the server was ever
    // contacted. LSP text is UTF-8 by spec, so the unreadable bytes
    // are lossy-decoded instead of failing the whole operation.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    std::fs::write(
        &setup.main_rs,
        b"fn main() {\n    let s = \"caf\xff\xfe\";\n}\n",
    )
    .unwrap();
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "expected one definition, got {locs:?}");
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "textDocument/didOpen"),
        1,
        "expected didOpen for the non-UTF-8 file: {methods:?}"
    );
}

#[tokio::test]
async fn mock_server_hover_and_references() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let hover = setup.manager.hover(&setup.main_rs, 1, 5).await.unwrap();
    assert!(hover.contains("**mock** hover"), "got: {hover}");
    let refs = setup
        .manager
        .find_references(&setup.main_rs, 1, 5, true)
        .await
        .unwrap();
    assert!(refs.is_empty(), "mock returns no references, got {refs:?}");
}

#[tokio::test]
async fn request_timeout_is_surfaced_not_hung() {
    let py = match find_binary("python3") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: python3 not found");
            return;
        }
    };
    let tmp = unique_tmpdir("timeout");
    let script = tmp.join("mock_lsp.py");
    write_file(&script, MOCK_SERVER);
    let log = tmp.join("methods.log");
    write_file(&log, "");
    let client = LspClient::spawn(
        py.to_str().unwrap(),
        &[
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            "file:///none".to_string(),
        ],
    )
    .unwrap();

    let err = client
        .request("test/neverResponds", json!({}), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "got: {err}");
}

/// Mock server for issue #386: it sends server-to-client requests
/// while a client request is pending, and logs every frame it receives
/// (full JSON, one per line) so the test can check the client answered
/// those requests instead of misrouting them. The `read_msg_timeout`
/// read keeps the mock from hanging when the client never answers: a
/// missing answer is logged as `{"timeout": true}` and the mock moves
/// on, so the test fails on its assertions instead of hanging.
const MOCK_SERVER_386: &str = r##"#!/usr/bin/env python3
import sys, json, threading, queue

log_path = sys.argv[1]

def log(obj):
    with open(log_path, "a") as f:
        f.write(json.dumps(obj) + "\n")

def _read_msg_raw():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return json.loads(sys.stdin.buffer.read(n).decode("utf-8"))

# One reader thread feeds every read: `select` on stdin does not work on
# Windows (sockets only), so the mock crashed there as soon as it waited
# for the client's answer and the server looked closed during
# initialize. A single thread also means a timed-out wait can never
# steal bytes from a later read.
_msg_queue = queue.Queue()

def _reader():
    while True:
        msg = _read_msg_raw()
        _msg_queue.put(msg)
        if msg is None:
            return

threading.Thread(target=_reader, daemon=True).start()

def read_msg():
    return _msg_queue.get()

def read_msg_timeout(seconds):
    try:
        return _msg_queue.get(timeout=seconds)
    except queue.Empty:
        return {"timeout": True}

def send(obj):
    data = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None:
        break
    method, mid = msg.get("method", ""), msg.get("id")
    log(msg)
    if method == "initialize":
        # Server-to-client request reusing the pending initialize id:
        # the old reader routed on `id` first, so this landed in the
        # initialize waiter as if it were the initialize response.
        send({"jsonrpc": "2.0", "id": mid, "method": "window/workDoneProgress/create",
              "params": {"token": "mock-progress"}})
        # The next frame from the client must be the -32601 answer to
        # that request; read it before sending the real response.
        log(read_msg_timeout(5))
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {}}})
    elif method == "textDocument/definition":
        # Server-to-client request with an id no waiter holds: the old
        # reader dropped it silently instead of answering it.
        send({"jsonrpc": "2.0", "id": 9999, "method": "client/registerCapability",
              "params": {"registrations": []}})
        log(read_msg_timeout(5))
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "uri": "file:///target.rs",
            "range": {"start": {"line": 9, "character": 4},
                      "end": {"line": 9, "character": 12}}}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    # notifications (initialized) and unknown methods: no reply
"##;

fn logged_messages(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn server_request_is_answered_not_misrouted_as_response() {
    // Issue #386: the reader loop dispatched on `id` before `method`,
    // so a server-to-client request was delivered to a pending waiter
    // as if it were that request's response (corrupting the caller)
    // or dropped silently when no waiter matched its id.
    let Some(py) = find_binary("python3") else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let tmp = unique_tmpdir("386");
    let script = tmp.join("mock_386.py");
    write_file(&script, MOCK_SERVER_386);
    let log = tmp.join("messages.log");
    write_file(&log, "");
    let client = LspClient::spawn(
        py.to_str().unwrap(),
        &[
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
        ],
    )
    .unwrap();

    // The mock answers `initialize` only after the client answers its
    // colliding server request, so a misrouted request would surface
    // here as a wrong initialize result or a stalled handshake.
    client.initialize(tmp.as_path()).await.unwrap();
    let result = client
        .request(
            "textDocument/definition",
            json!({"textDocument": {"uri": "file:///main.rs"},
                   "position": {"line": 0, "character": 4}}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    // The definition result must be the real response, not the server
    // request that arrived while it was pending.
    assert_eq!(
        result.get("uri").and_then(Value::as_str),
        Some("file:///target.rs"),
        "got: {result}"
    );

    client.kill();

    let messages = logged_messages(&log);
    // The colliding server request (same id as the pending initialize)
    // must have been answered with Method-not-found, not fed to the
    // initialize waiter.
    assert!(
        messages
            .iter()
            .any(|m| m.get("id").and_then(Value::as_u64) == Some(1)
                && m.get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_i64)
                    == Some(-32601)),
        "expected a -32601 answer to the colliding server request (id 1): {messages:?}"
    );
    // The unmatched server request (id 9999, no waiter) must have been
    // answered too, not dropped silently.
    assert!(
        messages
            .iter()
            .any(|m| m.get("id").and_then(Value::as_u64) == Some(9999)
                && m.get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_i64)
                    == Some(-32601)),
        "expected a -32601 answer to the unmatched server request (id 9999): {messages:?}"
    );
}

/// A fake server that answers `initialize` with a bare JSON number
/// instead of an object: not a valid JSON-RPC frame.
const MOCK_NON_OBJECT_SERVER: &str = r##"#!/usr/bin/env python3
import sys, time

def read_msg():
    headers = {}
    while True:
        raw = sys.stdin.buffer.readline()
        if not raw:
            return None
        line = raw.decode("utf-8", "replace")
        if line in ("\r\n", "\n", ""):
            break
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    if n <= 0:
        return None
    return sys.stdin.buffer.read(n)

msg = read_msg()
if msg is not None:
    sys.stdout.buffer.write(b"Content-Length: 1\r\n\r\n5")
    sys.stdout.buffer.flush()
time.sleep(30)
"##;

#[tokio::test]
async fn non_object_frame_ends_connection_promptly() {
    // Issue #277 follow-up: a server answering with a non-object frame
    // used to leave the reader loop spinning on it, pinning the CPU and
    // hanging every test that waits on the connection. The loop now
    // treats a non-object as a dead connection, so start fails at once
    // instead of waiting out the 30 s initialize timeout.
    let Some(py) = find_binary("python3") else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let tmp = unique_tmpdir("non-object");
    let script = tmp.join("mock_non_object.py");
    write_file(&script, MOCK_NON_OBJECT_SERVER);
    let log = tmp.join("methods.log");
    write_file(&log, "");
    let start = Instant::now();
    let err = match LspConnection::start(
        LspLanguage::Rust,
        py.to_str().unwrap(),
        &[
            script.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
            "file:///none".to_string(),
        ],
        tmp.as_path(),
    )
    .await
    {
        Ok(_) => panic!("start must fail on a non-object frame"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("LSP server closed"), "got: {err}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "connection must fail promptly, took {:?}",
        start.elapsed()
    );
}

fn loc_result(uri: &str, sl: u64, sc: u64, el: u64, ec: u64) -> Value {
    json!({"uri": uri,
           "range": {"start": {"line": sl, "character": sc},
                     "end": {"line": el, "character": ec}}})
}

#[test]
fn parse_definition_single_location() {
    let locs = parse_locations(&loc_result("file:///nonexistent/a.rs", 4, 2, 4, 8));
    assert_eq!(locs.len(), 1);
    assert_eq!(
        PathBuf::from(locs[0].path.as_str()),
        PathBuf::from("/nonexistent/a.rs")
    );
    assert_eq!((locs[0].line, locs[0].col), (5, 3));
    assert_eq!((locs[0].end_line, locs[0].end_col), (5, 9));
}

#[test]
fn parse_definition_location_array() {
    let result = json!([
        loc_result("file:///nonexistent/a.rs", 0, 0, 0, 5),
        loc_result("file:///nonexistent/b.py", 10, 1, 10, 6),
    ]);
    let locs = parse_locations(&result);
    assert_eq!(locs.len(), 2);
    assert_eq!(
        PathBuf::from(locs[1].path.as_str()),
        PathBuf::from("/nonexistent/b.py")
    );
    assert_eq!((locs[1].line, locs[1].col), (11, 2));
}

#[test]
fn parse_definition_location_links() {
    let result = json!([{
        "originSelectionRange": {"start": {"line": 1, "character": 1},
                                "end": {"line": 1, "character": 4}},
        "targetUri": "file:///nonexistent/lib.rs",
        "targetRange": {"start": {"line": 41, "character": 4},
                       "end": {"line": 41, "character": 10}},
        "targetSelectionRange": {"start": {"line": 41, "character": 4},
                                "end": {"line": 41, "character": 10}},
    }]);
    let locs = parse_locations(&result);
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0].uri, "file:///nonexistent/lib.rs");
    assert_eq!(
        PathBuf::from(locs[0].path.as_str()),
        PathBuf::from("/nonexistent/lib.rs")
    );
    assert_eq!((locs[0].line, locs[0].col), (42, 5));
}

#[test]
fn parse_definition_null_and_empty() {
    assert!(parse_locations(&Value::Null).is_empty());
    assert!(parse_locations(&json!([])).is_empty());

    assert!(parse_locations(&json!([{"nope": 1}])).is_empty());
}

#[test]
fn parse_definition_converts_utf16_columns_via_target_file() {
    let tmp = unique_tmpdir("utf16-target");
    let target = tmp.join("u.rs");
    write_file(
        &target,
        "fn x() {}\nlet caf\u{e9}_\u{2615}_\u{1f600} = 1;\n",
    );
    let uri = path_to_uri(&target);
    let locs = parse_locations(&loc_result(&uri, 1, 11, 1, 13));
    assert_eq!(locs.len(), 1);
    assert_eq!(
        locs[0].col, 12,
        "emoji counts as one character, got {:?}",
        locs[0]
    );

    assert_eq!(locs[0].end_col, 13);
}

#[test]
fn utf16_conversions() {
    assert_eq!(col_to_utf16("abc", 1), 0);
    assert_eq!(col_to_utf16("abc", 2), 1);
    assert_eq!(col_to_utf16("abc", 4), 3);
    assert_eq!(col_to_utf16("abc", 99), 3);
    assert_eq!(col_to_utf16("", 1), 0);

    assert_eq!(col_to_utf16("a\u{e9}\u{1f600}b", 3), 2);
    assert_eq!(col_to_utf16("a\u{e9}\u{1f600}b", 4), 4);

    assert_eq!(utf16_to_col("abc", 0), 1);
    assert_eq!(utf16_to_col("abc", 3), 4);
    assert_eq!(utf16_to_col("", 0), 1);
    assert_eq!(utf16_to_col("a\u{e9}\u{1f600}b", 2), 3);
    assert_eq!(utf16_to_col("a\u{e9}\u{1f600}b", 4), 4);

    assert_eq!(utf16_to_col("a\u{e9}\u{1f600}b", 3), 4);
}

#[test]
fn hover_renders_all_content_shapes() {
    let md = |v: &str| format!("{{\"contents\":{v}}}");

    assert_eq!(
        render_hover(&serde_json::from_str(&md("\"hello\"")).unwrap()),
        "hello"
    );

    let r = render_hover(
        &serde_json::from_str(&md("{\"kind\":\"markdown\",\"value\":\"**sig**\"}")).unwrap(),
    );
    assert_eq!(r, "**sig**");

    let r = render_hover(
        &serde_json::from_str(&md("{\"language\":\"rust\",\"value\":\"fn f()\"}")).unwrap(),
    );
    assert_eq!(r, "```rust\nfn f()\n```");

    let r = render_hover(
        &serde_json::from_str(&md(
            "[\"intro\",{\"language\":\"python\",\"value\":\"def f():\"},{\"kind\":\"plaintext\",\"value\":\"docs\"}]",
        ))
        .unwrap(),
    );
    assert_eq!(r, "intro\n\n```python\ndef f():\n```\n\ndocs");

    assert_eq!(render_hover(&json!(null)), "");
    assert_eq!(render_hover(&json!({})), "");
}

#[test]
fn uri_roundtrip_with_special_chars() {
    let p = PathBuf::from("/tmp/some dir/caf\u{e9}/x.rs");
    let uri = path_to_uri(&p);
    assert!(uri.starts_with("file:///tmp/some%20dir/"), "got: {uri}");
    assert_eq!(uri_to_path(&uri).unwrap(), p);
    assert_eq!(uri_to_path("untitled:Untitled-1"), None);
}

#[test]
fn language_detection_matrix() {
    let cases: &[(&str, Option<LspLanguage>)] = &[
        ("main.rs", Some(LspLanguage::Rust)),
        ("MAIN.RS", Some(LspLanguage::Rust)),
        ("mod.py", Some(LspLanguage::Python)),
        ("index.ts", Some(LspLanguage::TypeScript)),
        ("app.tsx", Some(LspLanguage::TypeScript)),
        ("bundle.js", Some(LspLanguage::TypeScript)),
        ("comp.jsx", Some(LspLanguage::TypeScript)),
        ("esm.mjs", Some(LspLanguage::TypeScript)),
        ("cjs.cjs", Some(LspLanguage::TypeScript)),
        ("notes.txt", None),
        ("Makefile", None),
        ("noext", None),
    ];
    for (name, expected) in cases {
        assert_eq!(
            detect_language(Path::new(name)),
            *expected,
            "extension of {name}"
        );
    }
}

#[test]
fn project_root_uses_markers() {
    let tmp = unique_tmpdir("markers");

    let rs = tmp.join("proj-rs").join("src").join("deep").join("main.rs");
    write_file(&tmp.join("proj-rs").join("Cargo.toml"), "[package]\n");
    write_file(&rs, "fn main() {}\n");
    assert_eq!(
        find_project_root(&rs, LspLanguage::Rust),
        tmp.join("proj-rs")
    );

    let py = tmp.join("proj-py").join("pkg").join("mod.py");
    write_file(&tmp.join("proj-py").join("pyproject.toml"), "[project]\n");
    write_file(&py, "x = 1\n");
    assert_eq!(
        find_project_root(&py, LspLanguage::Python),
        tmp.join("proj-py")
    );

    let ts = tmp.join("proj-ts").join("src").join("index.ts");
    write_file(&tmp.join("proj-ts").join("package.json"), "{}\n");
    write_file(&ts, "export {};\n");
    assert_eq!(
        find_project_root(&ts, LspLanguage::TypeScript),
        tmp.join("proj-ts")
    );

    let mixed = tmp.join("mixed").join("x.rs");
    write_file(&tmp.join("mixed").join("setup.py"), "");
    write_file(&mixed, "fn f() {}\n");
    assert_eq!(
        find_project_root(&mixed, LspLanguage::Rust),
        tmp.join("mixed")
    );

    let plain = tmp.join("plain").join("a").join("b").join("c.rs");
    write_file(&plain, "fn f() {}\n");
    assert_eq!(
        find_project_root(&plain, LspLanguage::Rust),
        tmp.join("plain").join("a").join("b")
    );
}

#[test]
fn server_specs_match_requirements() {
    let rust = server_spec(LspLanguage::Rust);
    assert_eq!(rust.command, "rust-analyzer");
    assert!(rust.args.is_empty());
    assert!(
        rust.install_hint
            .contains("rustup component add rust-analyzer")
    );

    let py = server_spec(LspLanguage::Python);
    assert_eq!(py.command, "pyright-langserver");
    assert_eq!(py.args, vec!["--stdio".to_string()]);
    assert!(py.install_hint.contains("npm install -g pyright"));

    let ts = server_spec(LspLanguage::TypeScript);
    assert_eq!(ts.command, "typescript-language-server");
    assert_eq!(ts.args, vec!["--stdio".to_string()]);
    assert!(
        ts.install_hint
            .contains("npm install -g typescript-language-server")
    );
}

#[tokio::test]
async fn unknown_extension_is_a_clear_error() {
    let tmp = unique_tmpdir("unknown-ext");
    let f = tmp.join("data.xyz");
    write_file(&f, "hello\n");
    let mgr = LspManager::new();
    let err = mgr.goto_definition(&f, 1, 1).await.unwrap_err().to_string();
    assert!(err.contains("unsupported file type"), "got: {err}");
    assert!(err.contains(".rs"), "got: {err}");
}

#[tokio::test]
async fn missing_binary_names_the_install_command() {
    // Hold PATH_LOCK while setting the fixture PATH, then release it
    // across the awaits (clippy::await_holding_lock) and re-acquire it
    // before restoring PATH. The EnvGuard stays alive across the awaits,
    // so PATH keeps the fixture value; this is the only test in this
    // binary that mutates PATH, so no other test can change it under us.
    let mut path_lock = Some(PATH_LOCK.lock().unwrap());
    let tmp = unique_tmpdir("empty-path");
    let _env_guard = EnvGuard::set("PATH", &tmp);
    drop(path_lock.take());

    let mgr = LspManager::new();
    let cases = [
        ("a.rs", "rustup component add rust-analyzer"),
        ("b.py", "npm install -g pyright"),
        ("c.ts", "npm install -g typescript-language-server"),
    ];
    let mut failures = Vec::new();
    for (name, hint) in cases {
        let f = tmp.join(name);
        write_file(&f, "x\n");
        let err = mgr.goto_definition(&f, 1, 1).await.unwrap_err().to_string();
        if !err.contains(hint) {
            failures.push(format!("{name}: expected hint '{hint}', got '{err}'"));
        }
        if !err.contains("not found on PATH") {
            failures.push(format!("{name}: expected 'not found on PATH', got '{err}'"));
        }
    }

    // Re-acquire the lock before restoring PATH, as before: another
    // test must never observe the fixture PATH.
    let _path_lock = PATH_LOCK.lock().unwrap();
    drop(_env_guard);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn test_ctx(dir: &Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), "lsp-test-session".to_string())
}

#[test]
fn tool_metadata() {
    let mgr = std::sync::Arc::new(LspManager::new());
    let def = GotoDefinitionTool::new(mgr.clone());
    let hov = HoverTool::new(mgr.clone());
    let refs = FindReferencesTool::new(mgr);
    assert_eq!(def.name(), "goto_definition");
    assert_eq!(hov.name(), "hover");
    assert_eq!(refs.name(), "find_references");
    assert!(def.is_read_only() && hov.is_read_only() && refs.is_read_only());
    for t in [&def as &dyn Tool, &hov as &dyn Tool, &refs as &dyn Tool] {
        let schema = t.parameters_schema();
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(required.contains(&"file"), "{}", t.name());
        assert!(required.contains(&"line"), "{}", t.name());
        assert!(required.contains(&"col"), "{}", t.name());
    }
    let refs_schema = refs.parameters_schema();
    assert!(
        refs_schema
            .get("properties")
            .and_then(|p| p.get("include_declaration"))
            .is_some(),
        "find_references must document include_declaration"
    );
}

#[tokio::test]
async fn tool_rejects_bad_arguments() {
    let mgr = std::sync::Arc::new(LspManager::new());
    let ctx = test_ctx(&unique_tmpdir("bad-args"));
    let def = GotoDefinitionTool::new(mgr);

    let out = def.call(json!({}), &ctx).await;
    assert!(
        out.is_error && out.content.contains("'file'"),
        "got: {}",
        out.content
    );

    let out = def
        .call(json!({"file": "x.rs", "line": 0, "col": 1}), &ctx)
        .await;
    assert!(
        out.is_error && out.content.contains(">= 1"),
        "got: {}",
        out.content
    );
}

#[tokio::test]
async fn tool_reports_unsupported_extension() {
    let tmp = unique_tmpdir("tool-unsupported");
    write_file(&tmp.join("data.xyz"), "hi\n");
    let mgr = std::sync::Arc::new(LspManager::new());
    let ctx = test_ctx(&tmp);
    let out = GotoDefinitionTool::new(mgr)
        .call(json!({"file": "data.xyz", "line": 1, "col": 1}), &ctx)
        .await;
    assert!(out.is_error, "expected error, got: {}", out.content);
    assert!(
        out.content.contains("unsupported file type"),
        "got: {}",
        out.content
    );
}

#[tokio::test]
async fn tool_output_has_human_lines_and_json_block() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };

    let mgr = std::sync::Arc::new(setup.manager);
    let ctx = test_ctx(&setup._tmp);
    let out = GotoDefinitionTool::new(mgr.clone())
        .call(json!({"file": "main.rs", "line": 1, "col": 5}), &ctx)
        .await;
    assert!(!out.is_error, "got error: {}", out.content);
    assert!(
        out.content.contains("goto_definition"),
        "got: {}",
        out.content
    );
    assert!(
        out.content.contains("target.rs:10:5"),
        "got: {}",
        out.content
    );
    assert!(out.content.contains("```json"), "got: {}", out.content);
    assert!(out.content.contains("\"line\": 10"), "got: {}", out.content);

    let hover_out = HoverTool::new(mgr)
        .call(json!({"file": "main.rs", "line": 1, "col": 5}), &ctx)
        .await;
    assert!(!hover_out.is_error, "got error: {}", hover_out.content);
    assert!(
        hover_out.content.contains("**mock** hover"),
        "got: {}",
        hover_out.content
    );
    assert!(
        hover_out.content.contains("```json"),
        "got: {}",
        hover_out.content
    );
}

#[tokio::test]
async fn diagnostics_publish_are_captured() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    let d = &diags[0];
    assert_eq!(d.message, "mock error one");
    assert_eq!(d.severity, 1);
    assert_eq!((d.line, d.col), (2, 5));
    assert_eq!((d.end_line, d.end_col), (2, 10));
    assert_eq!(d.source.as_deref(), Some("mock"));

    // The batch stays in the map: the read-only tool sees it without waiting.
    let cached = setup
        .manager
        .cached_diagnostics(&setup.main_rs)
        .expect("connection must exist after diagnostics_after_edit");
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].message, "mock error one");
}

#[tokio::test]
async fn diagnostics_empty_publish_clears_previous() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 1, "got: {diags:?}");

    rewrite_moved(&setup.main_rs, "fn main() {\n    let x = 1;\n}\n");
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert!(diags.is_empty(), "got: {diags:?}");
    let cached = setup.manager.cached_diagnostics(&setup.main_rs).unwrap();
    assert!(
        cached.is_empty(),
        "an empty publish must clear the map: {cached:?}"
    );
}

#[tokio::test]
async fn diagnostics_wait_resolves_on_publish_not_on_timeout() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    let start = Instant::now();
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    let elapsed = start.elapsed();
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    // The cap is 500ms; a wait that resolved on the publish (+50ms
    // debounce) must finish well before it. This is the measured proof
    // that the loop does NOT wait a fixed 3s.
    assert!(
        elapsed < Duration::from_millis(500),
        "wait ran to the cap: {elapsed:?}"
    );
}

#[tokio::test]
async fn diagnostics_wait_runs_to_cap_when_server_silent() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    // The plain mock never publishes: the wait must run the full cap
    // (200ms here) and return empty, not error.
    setup
        .manager
        .set_diagnostics_timing(Duration::from_millis(200), Duration::from_millis(20));
    let start = Instant::now();
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    let elapsed = start.elapsed();
    assert!(diags.is_empty(), "got: {diags:?}");
    assert!(
        elapsed >= Duration::from_millis(150),
        "wait returned before the cap: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "wait hung past the cap: {elapsed:?}"
    );
}

#[tokio::test]
async fn diagnostics_connection_wait_ends_at_cap_not_past_it() {
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    // The plain mock never publishes. Cap 1500ms with a 600ms debounce
    // is deliberately NOT a multiple: the correct wait ends at the cap
    // (600 + 600 + 200), while `timeout + elapsed` instead of
    // `timeout - elapsed` would wait a full extra debounce (1800ms).
    // This calls the connection directly: the manager wraps the same
    // wait in an outer timeout that would mask the overshoot.
    let conn = setup
        .manager
        .connection_for(&setup.main_rs)
        .await
        .expect("mock server must spawn");
    let start = Instant::now();
    let diags = conn
        .diagnostics_after_edit(
            &setup.main_rs,
            Duration::from_millis(1500),
            Duration::from_millis(600),
        )
        .await
        .expect("connection wait must not error");
    let elapsed = start.elapsed();
    assert!(diags.is_empty(), "got: {diags:?}");
    assert!(
        elapsed >= Duration::from_millis(1300),
        "wait returned before the cap: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1650),
        "wait ran past the cap: {elapsed:?}"
    );
}

#[tokio::test]
async fn diagnostics_trait_method_forwards_to_manager() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    // Call through the trait object, as the agent loop does: the trait
    // impl must forward to the manager, not return an empty vec.
    let provider: &dyn DiagnosticsProvider = &setup.manager;
    let diags = provider.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    assert_eq!(diags[0].message, "mock error one");
}

#[tokio::test]
async fn diagnostics_diff_only_new() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    let first = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(first.len(), 1, "got: {first:?}");

    rewrite_moved(
        &setup.main_rs,
        "fn main() {\n    ERROR_ONE\n    ERROR_TWO\n}\n",
    );
    let second = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(
        second.len(),
        1,
        "only the new diagnostic may be reported: {second:?}"
    );
    assert_eq!(second[0].message, "mock warning two");
    assert_eq!(second[0].severity, 2);
}

#[tokio::test]
async fn diagnostics_after_edit_keeps_only_error_and_warning() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    // Mixed severity: the mock publishes one diagnostic per severity.
    // Q5: only Error and Warning reach the feedback loop; Information
    // and Hint are filtered out at collection.
    write_file(
        &setup.main_rs,
        "fn main() {\n    ERROR_ONE\n    ERROR_TWO\n    INFO_THREE\n    HINT_FOUR\n}\n",
    );
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 2, "got: {diags:?}");
    assert_eq!(diags[0].message, "mock error one");
    assert_eq!(diags[0].severity, 1);
    assert_eq!(diags[1].message, "mock warning two");
    assert_eq!(diags[1].severity, 2);
}

#[tokio::test]
async fn diagnostics_after_edit_unsupported_extension_returns_empty_without_spawn() {
    let tmp = unique_tmpdir("diag-unsupported");
    let f = tmp.join("notes.txt");
    write_file(&f, "hello\n");
    let mgr = LspManager::new();
    let diags = mgr.diagnostics_after_edit(&f).await;
    assert!(diags.is_empty());
    assert!(
        mgr.cached_diagnostics(&f).is_none(),
        "no server must be spawned for an unsupported extension"
    );
}

#[tokio::test]
async fn diagnostics_after_edit_server_exit_returns_empty() {
    let py = match find_binary("python3") {
        Some(p) => p,
        None => {
            eprintln!("SKIP: python3 not found");
            return;
        }
    };
    let tmp = unique_tmpdir("diag-exit");
    let main_rs = tmp.join("main.rs");
    write_file(&main_rs, "fn main() {}\n");
    let script = tmp.join("mock_exit.py");
    write_file(&script, MOCK_SERVER_EXIT);
    let mgr = LspManager::new();
    mgr.set_server_command(
        LspLanguage::Rust,
        py.to_string_lossy().into_owned(),
        vec![script.to_string_lossy().into_owned()],
    );
    mgr.set_diagnostics_timing(Duration::from_millis(500), Duration::from_millis(50));
    let start = Instant::now();
    let diags = mgr.diagnostics_after_edit(&main_rs).await;
    assert!(diags.is_empty(), "got: {diags:?}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "a dead server must not hang the wait"
    );
}

#[test]
fn diagnostics_timing_defaults_and_clamping() {
    let mgr = LspManager::new();
    let (timeout, debounce) = mgr.diagnostics_timing();
    assert_eq!(timeout, Duration::from_secs(3));
    assert_eq!(debounce, Duration::from_millis(150));
    mgr.set_diagnostics_timing(Duration::from_millis(0), Duration::from_millis(0));
    let (timeout, debounce) = mgr.diagnostics_timing();
    assert_eq!(timeout, Duration::from_millis(1));
    assert_eq!(debounce, Duration::from_millis(1));
    mgr.set_diagnostics_timing(Duration::from_secs(10), Duration::from_millis(500));
    let (timeout, debounce) = mgr.diagnostics_timing();
    assert_eq!(timeout, Duration::from_secs(10));
    assert_eq!(debounce, Duration::from_millis(500));
}

#[tokio::test]
async fn diagnostics_tool_reads_cached_diagnostics_without_waiting() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(&setup.main_rs, "fn main() {\n    ERROR_ONE\n}\n");
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 1, "got: {diags:?}");

    let mgr = std::sync::Arc::new(setup.manager);
    let ctx = test_ctx(&setup._tmp);
    let out = DiagnosticsTool::new(mgr)
        .call(json!({"file": "main.rs"}), &ctx)
        .await;
    assert!(!out.is_error, "got error: {}", out.content);
    assert!(
        out.content.contains("mock error one"),
        "got: {}",
        out.content
    );
    assert!(out.content.contains("Error [2:5]"), "got: {}", out.content);
    assert!(out.content.contains("```json"), "got: {}", out.content);
}

#[tokio::test]
async fn diagnostics_tool_keeps_only_error_and_warning() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    write_file(
        &setup.main_rs,
        "fn main() {\n    ERROR_ONE\n    ERROR_TWO\n    INFO_THREE\n    HINT_FOUR\n}\n",
    );
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert_eq!(diags.len(), 2, "got: {diags:?}");

    let mgr = std::sync::Arc::new(setup.manager);
    let ctx = test_ctx(&setup._tmp);
    let out = DiagnosticsTool::new(mgr)
        .call(json!({"file": "main.rs"}), &ctx)
        .await;
    assert!(!out.is_error, "got error: {}", out.content);
    assert!(
        out.content.contains("mock error one"),
        "got: {}",
        out.content
    );
    assert!(
        out.content.contains("mock warning two"),
        "got: {}",
        out.content
    );
    assert!(
        !out.content.contains("mock info three"),
        "Information must be filtered out: {}",
        out.content
    );
    assert!(
        !out.content.contains("mock hint four"),
        "Hint must be filtered out: {}",
        out.content
    );
    assert!(
        out.content.contains("(2):"),
        "count header must say 2: {}",
        out.content
    );
}

#[tokio::test]
async fn diagnostics_tool_reports_no_server_without_spawning() {
    let tmp = unique_tmpdir("diag-tool-noserver");
    write_file(&tmp.join("main.rs"), "fn main() {}\n");
    let mgr = std::sync::Arc::new(LspManager::new());
    let ctx = test_ctx(&tmp);
    let out = DiagnosticsTool::new(mgr.clone())
        .call(json!({"file": "main.rs"}), &ctx)
        .await;
    assert!(!out.is_error, "got error: {}", out.content);
    assert!(
        out.content.contains("no language server running"),
        "got: {}",
        out.content
    );
    assert!(
        mgr.cached_diagnostics(&tmp.join("main.rs")).is_none(),
        "the diagnostics tool must not spawn a server"
    );
}

#[tokio::test]
async fn diagnostics_tool_rejects_missing_file() {
    let mgr = std::sync::Arc::new(LspManager::new());
    let ctx = test_ctx(&unique_tmpdir("diag-tool-badargs"));
    let out = DiagnosticsTool::new(mgr).call(json!({}), &ctx).await;
    assert!(
        out.is_error && out.content.contains("'file'"),
        "got: {}",
        out.content
    );
}

#[tokio::test]
async fn diagnostics_tool_reports_no_diagnostics_when_cache_empty() {
    let Some(setup) = mock_diag_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    // Content with no markers: the mock publishes an empty batch, so a
    // connection exists but the cache is empty.
    write_file(&setup.main_rs, "fn main() {\n    let x = 1;\n}\n");
    let diags = setup.manager.diagnostics_after_edit(&setup.main_rs).await;
    assert!(diags.is_empty(), "got: {diags:?}");

    let mgr = std::sync::Arc::new(setup.manager);
    let ctx = test_ctx(&setup._tmp);
    let out = DiagnosticsTool::new(mgr)
        .call(json!({"file": "main.rs"}), &ctx)
        .await;
    assert!(!out.is_error, "got error: {}", out.content);
    assert!(
        out.content.contains("(no diagnostics)"),
        "got: {}",
        out.content
    );
}

#[test]
fn diagnostics_tool_metadata() {
    let mgr = std::sync::Arc::new(LspManager::new());
    let tool = DiagnosticsTool::new(mgr);
    assert_eq!(tool.name(), "diagnostics");
    assert!(tool.is_read_only());
    let desc = tool.description();
    assert!(desc.contains("diagnostics"), "{desc}");
    assert!(desc.contains("Read-only"), "{desc}");
    let schema = tool.parameters_schema();
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    assert_eq!(required, vec!["file"]);
}

#[test]
fn parse_publish_diagnostics_ok_and_defaults() {
    let val = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": "file:///nonexistent/a.rs",
            "diagnostics": [
                {"range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 8}},
                 "severity": 1, "message": "type error", "source": "rust-analyzer", "code": "E0308"},
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                 "message": "no severity here", "code": 42},
                {"range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 2}},
                 "severity": 2, "message": "warn"},
                {"nope": true},
                {"range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 2}}}
            ]
        }
    });
    let (uri, diags) = parse_publish_diagnostics(&val).expect("must parse");
    assert_eq!(uri, "file:///nonexistent/a.rs");
    assert_eq!(diags.len(), 3, "malformed items must be skipped: {diags:?}");
    assert_eq!(diags[0].message, "type error");
    assert_eq!((diags[0].line, diags[0].col), (5, 3));
    assert_eq!((diags[0].end_line, diags[0].end_col), (5, 9));
    assert_eq!(diags[0].severity, 1);
    assert_eq!(diags[0].source.as_deref(), Some("rust-analyzer"));
    assert_eq!(diags[0].code.as_deref(), Some("E0308"));
    assert_eq!(diags[1].severity, 1, "missing severity defaults to Error");
    assert_eq!(
        diags[1].code.as_deref(),
        Some("42"),
        "a numeric code stringifies"
    );
    assert_eq!(diags[1].source, None);
    assert_eq!(diags[2].severity, 2);
}

#[test]
fn parse_publish_diagnostics_non_file_uri_uses_utf16_plus_one() {
    let val = json!({
        "params": {
            "uri": "untitled:Untitled-1",
            "diagnostics": [
                {"range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 9}},
                 "severity": 1, "message": "unsaved buffer error"}
            ]
        }
    });
    let (_uri, diags) = parse_publish_diagnostics(&val).expect("must parse");
    assert_eq!(diags.len(), 1);
    // No file to read: columns fall back to UTF-16 + 1.
    assert_eq!((diags[0].line, diags[0].col), (2, 5));
    assert_eq!((diags[0].end_line, diags[0].end_col), (2, 10));
}

#[test]
fn parse_publish_diagnostics_rejects_bad_shapes() {
    assert!(parse_publish_diagnostics(&json!({})).is_none());
    assert!(parse_publish_diagnostics(&json!({"params": {}})).is_none());
    assert!(parse_publish_diagnostics(&json!({"params": {"uri": "file:///a.rs"}})).is_none());
    let (uri, diags) =
        parse_publish_diagnostics(&json!({"params": {"uri": "file:///a.rs", "diagnostics": []}}))
            .expect("an empty diagnostics array is valid");
    assert_eq!(uri, "file:///a.rs");
    assert!(diags.is_empty());
}

#[tokio::test]
async fn dead_server_is_evicted_and_next_call_respawns() {
    // Issue #387: a dead LSP server used to stay cached forever, so
    // every later call failed until the process restarted. The manager
    // must evict the corpse (liveness probe, or the transport error
    // from the call itself) and serve the next call from a fresh
    // server.
    let Some(setup) = dying_mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "first call must succeed: {locs:?}");

    // The server exited after answering. The next call must NOT fail
    // with the corpse's transport error: it respawns and answers.
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(
        locs.len(),
        1,
        "second call must succeed on a fresh server: {locs:?}"
    );

    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        2,
        "a fresh server must have been started after the death: {methods:?}"
    );
    assert_eq!(count(&methods, "textDocument/definition"), 2);
}

#[tokio::test]
async fn cached_diagnostics_reports_no_server_after_death() {
    // Issue #387: after the server dies, the read-only diagnostics
    // query must report "no server" instead of serving an empty cache
    // from the corpse. The child exits right after answering the
    // first call, so poll until the probe sees it (a fixed wait would
    // be racy on a loaded machine; on unfixed code this never turns
    // None and the test fails).
    let Some(setup) = dying_mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let _ = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    let start = Instant::now();
    loop {
        if setup.manager.cached_diagnostics(&setup.main_rs).is_none() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cached_diagnostics must report no server once the child has exited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn dead_server_backoff_stops_respawn_loop() {
    // Issue #387: a server that dies on every start must not be
    // respawned in a tight loop. After MAX_CONSECUTIVE_DEAD_SERVERS
    // failures the manager refuses to spawn again until the cooldown
    // passes, and the refusal names the failure count.
    let Some(setup) = dies_at_start_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    for attempt in 1..=3 {
        let err = setup
            .manager
            .goto_definition(&setup.main_rs, 1, 5)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to start"),
            "attempt {attempt} must fail at start, got: {err}"
        );
    }
    let err = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not respawning"),
        "fourth call must hit the backoff, got: {err}"
    );
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "started"),
        3,
        "no server must have been started on the backed-off call: {methods:?}"
    );
}

#[test]
fn transport_error_classifier_only_matches_dead_server_errors() {
    // Issue #387, mutant killer: the eviction decision must split
    // transport errors (dead server) from call errors (a live server
    // rejecting the request) and timeouts (a live but slow server).
    for msg in [
        "write to LSP server failed: broken pipe",
        "LSP server dropped the response",
        "LSP error: {\"code\":-32000,\"message\":\"LSP server closed\"}",
        "LSP notify 'textDocument/didOpen' failed: broken pipe",
        "textDocument/didOpen failed: LSP notify 'textDocument/didOpen' failed: broken pipe",
        "LSP initialize failed: LSP server closed",
    ] {
        assert!(
            is_lsp_transport_error(&anyhow::anyhow!(msg)),
            "must classify as transport: {msg}"
        );
    }
    for msg in [
        "LSP error: {\"code\":-32601,\"message\":\"Method not found\"}",
        "LSP request 'textDocument/hover' timed out after 15s",
        "cannot stat 'main.rs': No such file or directory",
        "line 99 out of range (file has 10 lines)",
        "unsupported file type '.txt'",
    ] {
        assert!(
            !is_lsp_transport_error(&anyhow::anyhow!(msg)),
            "must NOT classify as transport: {msg}"
        );
    }
}

#[tokio::test]
async fn live_server_is_reused_across_calls() {
    // Issue #387, mutant killer for both `is_alive -> false` mutants
    // (`LspClient::is_alive` and `LspConnection::is_alive`): the
    // liveness probe must report a live server as alive, or every call
    // evicts the live server and spawns a fresh one. Two calls on the
    // live mock must share exactly one `initialize`.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    for _ in 0..2 {
        let locs = setup
            .manager
            .goto_definition(&setup.main_rs, 1, 5)
            .await
            .unwrap();
        assert_eq!(locs.len(), 1, "call must succeed: {locs:?}");
    }
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        1,
        "a live server must be reused, not respawned per call: {methods:?}"
    );
}

#[tokio::test]
async fn hover_dead_server_is_evicted_and_next_call_respawns() {
    // Issue #387, mutant killer for the hover retry arm: a hover on a
    // dead server must evict it and retry once on a fresh server, the
    // same shape as goto_definition. The dying mock answers one hover
    // then exits, so the second call exercises the retry.
    let Some(setup) = dying_mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let hover = setup.manager.hover(&setup.main_rs, 1, 5).await.unwrap();
    assert!(
        hover.contains("**mock** hover"),
        "first hover must succeed: {hover}"
    );
    let hover = setup.manager.hover(&setup.main_rs, 1, 5).await.unwrap();
    assert!(
        hover.contains("**mock** hover"),
        "second hover must succeed on a fresh server: {hover}"
    );
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        2,
        "a fresh server must have been started after the death: {methods:?}"
    );
}

#[tokio::test]
async fn references_dead_server_is_evicted_and_next_call_respawns() {
    // Issue #387, mutant killer for the find_references retry arm (and
    // for the whole-method replacement with `Ok(vec![])`): the dying
    // mock answers one references request with a single location, then
    // exits, so the second call must respawn and return that location
    // again instead of an empty list.
    let Some(setup) = dying_mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let locs = setup
        .manager
        .find_references(&setup.main_rs, 1, 5, false)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "first references call: {locs:?}");
    let locs = setup
        .manager
        .find_references(&setup.main_rs, 1, 5, false)
        .await
        .unwrap();
    assert_eq!(
        locs.len(),
        1,
        "second references call must succeed on a fresh server: {locs:?}"
    );
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        2,
        "a fresh server must have been started after the death: {methods:?}"
    );
}

#[tokio::test]
async fn call_error_is_not_retried_and_server_stays_cached() {
    // Issue #387, mutant killer for the `is_lsp_transport_error` guard
    // replaced with true in all three methods: a call error from a LIVE
    // server (here a local out-of-range position) must NOT evict the
    // server or retry. The error text is identical either way, so the
    // only observable difference is the server count: with the guard
    // replaced, each failed call spawns a fresh server.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let err = setup
        .manager
        .goto_definition(&setup.main_rs, 99, 1)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "got: {err}");
    let err = setup
        .manager
        .hover(&setup.main_rs, 99, 1)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "got: {err}");
    let err = setup
        .manager
        .find_references(&setup.main_rs, 99, 1, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "got: {err}");
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        1,
        "a call error must not evict or respawn the live server: {methods:?}"
    );
}

#[tokio::test]
async fn evict_ignores_stale_failure_after_respawn() {
    // Issue #387, mutant killer for the `Arc::ptr_eq` guard replaced
    // with true: evicting the OLD connection after another call already
    // respawned must leave the fresh server alone. The dying mock kills
    // the first server after one request, so the second call respawns a
    // live one; evicting the stale first connection afterwards must be
    // a no-op, and the next call must reuse the live server.
    let Some(setup) = dying_mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let first = setup.manager.connection_for(&setup.main_rs).await.unwrap();
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "first call must succeed: {locs:?}");
    // The first server exited after answering; the next call evicts it
    // and respawns a live server.
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "second call must succeed: {locs:?}");
    // A stale failure for the first connection arrives now: it must not
    // touch the live second server.
    let key = server_key(&setup.main_rs).unwrap();
    setup.manager.evict(&key, &first);
    let second = setup.manager.connection_for(&setup.main_rs).await.unwrap();
    assert!(
        !std::sync::Arc::ptr_eq(&first, &second),
        "the second call must have respawned a different connection"
    );
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "third call must succeed: {locs:?}");
    let methods = logged_methods(&setup.log);
    assert_eq!(
        count(&methods, "initialize"),
        2,
        "the stale evict must not have killed the live server: {methods:?}"
    );
}

#[tokio::test]
async fn evict_kills_the_evicted_child() {
    // Issue #387, mutant killer for the evict guard replaced with false
    // (and for `kill` replaced with ()): evicting a connection must
    // terminate its child process, so no orphan survives the cache
    // removal. With the guard replaced, evict is a no-op and the child
    // keeps running.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let conn = setup.manager.connection_for(&setup.main_rs).await.unwrap();
    assert!(conn.is_alive(), "the server must be alive before evict");
    let key = server_key(&setup.main_rs).unwrap();
    setup.manager.evict(&key, &conn);
    let start = Instant::now();
    loop {
        if !conn.is_alive() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "evict must terminate the child process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn kill_terminates_the_child_process() {
    // Issue #387, mutant killer for `LspConnection::kill` replaced with
    // (): killing a live connection must terminate its child. The
    // liveness probe returns false once the child handle is taken, so
    // the loop exits at once on correct code and times out under the
    // mutant.
    let Some(setup) = mock_setup() else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    let conn = setup.manager.connection_for(&setup.main_rs).await.unwrap();
    assert!(conn.is_alive(), "the server must be alive before kill");
    conn.kill();
    let start = Instant::now();
    loop {
        if !conn.is_alive() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill must terminate the child process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn success_clears_the_failure_streak() {
    // Issue #387, mutant killer for `record_success` replaced with ():
    // a successful call must forget earlier dead-server failures, so
    // the backoff counts only consecutive deaths. Two deaths, one
    // success, then two more deaths must still spawn (the streak is 2,
    // below the threshold of 3); if the success never cleared the
    // streak, the second death after it would already hit the backoff
    // and report "not respawning" instead of "failed to start".
    let Some(setup) = flaky_mock_setup("1,2,4,5", "3") else {
        eprintln!("SKIP: python3 not found");
        return;
    };
    for attempt in 1..=2 {
        let err = setup
            .manager
            .goto_definition(&setup.main_rs, 1, 5)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to start"),
            "attempt {attempt} must fail at start, got: {err}"
        );
    }
    let locs = setup
        .manager
        .goto_definition(&setup.main_rs, 1, 5)
        .await
        .unwrap();
    assert_eq!(locs.len(), 1, "the third call must succeed: {locs:?}");
    for attempt in 4..=5 {
        let err = setup
            .manager
            .goto_definition(&setup.main_rs, 1, 5)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to start"),
            "attempt {attempt} must fail at start, not hit the backoff, got: {err}"
        );
    }
}
