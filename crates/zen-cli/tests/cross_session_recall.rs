mod common;

use common::*;
use std::path::Path;

/// Build one session transcript in `home` for `cwd`: a user message plus
/// an optional compacted summary record.
fn write_session(home: &Path, cwd: &Path, id: &str, user_text: &str, summary: Option<&str>) {
    let path = zen_cli::session::ensure_session_path(home, cwd, id).unwrap();
    zen_cli::session::append_typed(
        &path,
        "user",
        id,
        cwd.to_str().unwrap(),
        "u1",
        None,
        Some(serde_json::json!({"role":"user","content":user_text})),
        None,
    )
    .unwrap();
    if let Some(s) = summary {
        zen_cli::session::append_summary(&path, id, cwd.to_str().unwrap(), "s1", Some("u1"), s)
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_search_finds_keyword() {
    let home = TempDir::new("search_home");
    let cwd = TempDir::new("search_cwd");
    write_session(
        home.path(),
        cwd.path(),
        "aaaabbbb-1",
        "the quokka escaped",
        None,
    );
    write_session(home.path(), cwd.path(), "ccccdddd-2", "nothing here", None);

    let (stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("search1", &home, &cwd)
            .args(["sessions", "search", "quokka"])
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stdout: {stdout}");
    assert!(stdout.contains("aaaabbbb"), "stdout: {stdout}");
    assert!(!stdout.contains("ccccdddd"), "stdout: {stdout}");
    assert!(stdout.contains("quokka"), "stdout: {stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_search_json_output() {
    let home = TempDir::new("search_json_home");
    let cwd = TempDir::new("search_json_cwd");
    write_session(
        home.path(),
        cwd.path(),
        "aaaabbbb-1",
        "the quokka escaped",
        None,
    );
    write_session(home.path(), cwd.path(), "ccccdddd-2", "nothing here", None);

    let (stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("search3", &home, &cwd)
            .args(["--json", "sessions", "search", "quokka"])
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stdout: {stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is not valid JSON");
    let hits = v.as_array().expect("stdout is not a JSON array");
    assert_eq!(hits.len(), 1, "stdout: {stdout}");
    assert_eq!(hits[0]["id"], "aaaabbbb-1");
    assert!(
        hits[0]["snippet"].as_str().unwrap().contains("quokka"),
        "stdout: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_search_raw_output() {
    let home = TempDir::new("search_raw_home");
    let cwd = TempDir::new("search_raw_cwd");
    write_session(
        home.path(),
        cwd.path(),
        "aaaabbbb-1",
        "the quokka escaped",
        None,
    );

    let (stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("search4", &home, &cwd)
            .args(["--raw", "sessions", "search", "quokka"])
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stdout: {stdout}");
    assert_eq!(stdout.trim().lines().count(), 1, "stdout: {stdout}");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout is not valid JSON");
    assert_eq!(v.as_array().unwrap().len(), 1, "stdout: {stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_search_no_match() {
    let home = TempDir::new("search_home2");
    let cwd = TempDir::new("search_cwd2");
    write_session(
        home.path(),
        cwd.path(),
        "aaaabbbb-1",
        "plain text only",
        None,
    );

    let (stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("search2", &home, &cwd)
            .args(["sessions", "search", "quokka"])
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0, "stdout: {stdout}");
    assert!(stdout.contains("no sessions match"), "stdout: {stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_loads_summary_not_raw_transcript() {
    let home = TempDir::new("recall_home");
    let cwd = TempDir::new("recall_cwd");
    write_session(
        home.path(),
        cwd.path(),
        "old-session-1",
        "the vault code is ZEBRA-12345, do not share",
        Some("Discussed vault access procedures."),
    );

    let server = MockServer::single_text("noted").await;
    let (_stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("recall1", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--bare",
                "--setting-sources",
                "",
                "--recall",
            ])
            .mock("mock-model", &server)
            .arg("summarize")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let msgs_text = request_messages_text(&requests[0]);
    assert!(
        msgs_text.contains("Discussed vault access procedures."),
        "summary missing from request: {msgs_text}"
    );
    assert!(
        msgs_text.contains("Previous sessions"),
        "recall block header missing: {msgs_text}"
    );
    assert!(
        !msgs_text.contains("ZEBRA-12345"),
        "raw transcript leaked into request: {msgs_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_cap_enforced_end_to_end() {
    let home = TempDir::new("recall_cap_home");
    let cwd = TempDir::new("recall_cap_cwd");
    let big = "y".repeat(60 * 1024);
    write_session(home.path(), cwd.path(), "old-session-1", "hi", Some(&big));

    let server = MockServer::single_text("noted").await;
    let (_stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("recall2", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--bare",
                "--setting-sources",
                "",
                "--recall",
            ])
            .mock("mock-model", &server)
            .arg("summarize")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let recall_msg = request_messages(&requests[0])
        .into_iter()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("Previous sessions"))
        })
        .expect("no recall message in request");
    let content = recall_msg["content"].as_str().unwrap();
    // The recalled summary is truncated to the 50 KiB cap; only the
    // small block headings sit on top of it.
    assert!(
        content.len() <= zen_cli::recall::RECALL_CAP_BYTES + 1024,
        "recall block over cap: {} bytes",
        content.len()
    );
    assert!(
        content.len() > zen_cli::recall::RECALL_CAP_BYTES,
        "recall block unexpectedly small: {} bytes",
        content.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_redacts_secrets_end_to_end() {
    let home = TempDir::new("recall_red_home");
    let cwd = TempDir::new("recall_red_cwd");
    let secret = "ghp_0123456789abcdefghij";
    write_session(
        home.path(),
        cwd.path(),
        "old-session-1",
        "hi",
        Some(&format!("deployed with token {secret}")),
    );

    let server = MockServer::single_text("noted").await;
    let (_stdout, _, code) = tokio::time::timeout(TEST_TIMEOUT, async {
        ZenRun::new("recall3", &home, &cwd)
            .args([
                "-p",
                "--output-format",
                "json",
                "--bare",
                "--setting-sources",
                "",
                "--recall",
            ])
            .mock("mock-model", &server)
            .arg("summarize")
            .run_output()
    })
    .await
    .expect("timed out");

    assert_eq!(code, 0);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let msgs_text = request_messages_text(&requests[0]);
    assert!(
        !msgs_text.contains(secret),
        "secret leaked into request: {msgs_text}"
    );
    assert!(
        msgs_text.contains("[REDACTED]"),
        "no redaction marker in request: {msgs_text}"
    );
}
