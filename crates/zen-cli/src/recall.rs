//! Cross-session memory (Phase 2 #13, direction A): search and recall
//! prior sessions in the same working directory.
//!
//! Recall loads ONLY the compacted `summary` records of prior sessions,
//! never the raw transcript. Recalled text is redacted like
//! `export --redact` and the total is capped at [`RECALL_CAP_BYTES`].
//! Retention is unchanged: transcripts stay until the user archives or
//! deletes them (there is no 30-day rule).

use std::path::Path;
use std::time::SystemTime;

/// Cap on the total recalled summary text loaded into a new session, in
/// bytes. Same cap as the delegate handoff envelope (50 KiB).
pub const RECALL_CAP_BYTES: usize = 50 * 1024;

/// Default number of prior sessions `--recall` / `/recall` load when no
/// count is given.
pub const DEFAULT_RECALL_COUNT: usize = 3;

/// Context characters kept on each side of a search match in a snippet.
const SNIPPET_CONTEXT_CHARS: usize = 60;

/// A compacted summary of one prior session, ready to load into context.
/// `text` holds the redacted summary only, never raw transcript lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub modified: SystemTime,
    pub title: Option<String>,
    pub text: String,
}

/// One keyword hit in a prior session transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub id: String,
    pub modified: SystemTime,
    pub title: Option<String>,
    /// Redacted snippet around the first match.
    pub snippet: String,
}

fn mtime_of(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Truncate `s` to at most `max_bytes` bytes, at a char boundary.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Bounded walk down to the nearest char boundary: no arithmetic loop,
    // so no mutant can turn this into an unbounded loop. (The old
    // `while end -= 1` let the `-=` -> `/=` mutant hang forever and fail
    // CI's mutants job on timeout, PR #210 run 43808976403.)
    let end = (0..=max_bytes)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[..end].to_string()
}

/// The latest compacted summary of a session transcript, if it has one.
/// Returns the LAST `summary` record's text: later compactions supersede
/// earlier ones.
pub fn latest_summary(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut latest: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("summary") {
            continue;
        }
        if let Some(text) = crate::session::summary_text(&v) {
            latest = Some(text);
        }
    }
    latest
}

/// Summaries of the `count` most recent prior sessions in `cwd` that have
/// a compacted summary, newest first. The session named by `exclude_id`
/// (the current or resumed session) is skipped. Sessions without a
/// summary are skipped, so recall never loads raw transcript. Text is
/// redacted like `export --redact`, and the total is capped at
/// [`RECALL_CAP_BYTES`]: a summary that would overflow the cap is
/// truncated at a char boundary and nothing further is added.
pub fn recall_summaries(
    home: &Path,
    cwd: &Path,
    count: usize,
    exclude_id: Option<&str>,
) -> Vec<SessionSummary> {
    if count == 0 {
        return vec![];
    }
    let mut out: Vec<SessionSummary> = vec![];
    let mut total: usize = 0;
    for (id, path) in crate::session::list_sessions_for_cwd(home, cwd) {
        if out.len() >= count {
            break;
        }
        if Some(id.as_str()) == exclude_id {
            continue;
        }
        let Some(raw) = latest_summary(&path) else {
            continue;
        };
        let text = crate::export::redact_str(&raw);
        if text.trim().is_empty() {
            continue;
        }
        let remaining = RECALL_CAP_BYTES.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let text = truncate_to_bytes(&text, remaining);
        total += text.len();
        out.push(SessionSummary {
            id,
            modified: mtime_of(&path),
            title: crate::session::session_title(&path).map(|t| crate::export::redact_str(&t)),
            text,
        });
    }
    out
}

/// Render recalled summaries as one context block for a new session.
/// Empty input renders an empty string (the caller then loads nothing).
/// Rendered context block for recalled summaries, or None when there are
/// no summaries to recall.
pub fn recall_block(summaries: &[SessionSummary]) -> Option<String> {
    if summaries.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Previous sessions in this directory (recalled summaries)\n\
         The following are compacted summaries of earlier sessions in this \
         working directory, newest first. They are background context only; \
         verify anything that matters before acting on it.\n",
    );
    for s in summaries {
        out.push_str("\n## Session ");
        out.push_str(crate::session_mgmt::short_id(&s.id));
        out.push_str(" (");
        out.push_str(&crate::session_mgmt::fmt_mtime(s.modified));
        out.push(')');
        if let Some(title) = &s.title {
            out.push_str(" - ");
            out.push_str(title);
        }
        out.push('\n');
        out.push_str(&s.text);
        out.push('\n');
    }
    Some(out)
}

/// Parse the optional count argument of `/recall`: empty means the
/// default count, a number means that many sessions.
pub fn parse_recall_count(arg: &str) -> Result<usize, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(DEFAULT_RECALL_COUNT);
    }
    arg.parse::<usize>()
        .map_err(|_| format!("usage: /recall [n] - n must be a number, got {arg:?}"))
}

/// Search the transcripts of this directory's sessions for `keyword`
/// (case-insensitive substring over message and summary text), newest
/// session first. Snippets are redacted like `export --redact`.
/// Archived sessions are not searched.
pub fn search_sessions(home: &Path, cwd: &Path, keyword: &str) -> Vec<SearchHit> {
    let needle = keyword.trim().to_lowercase();
    if needle.is_empty() {
        return vec![];
    }
    let mut hits = vec![];
    for (id, path) in crate::session::list_sessions_for_cwd(home, cwd) {
        let Some(snippet) = first_match_snippet(&path, &needle) else {
            continue;
        };
        hits.push(SearchHit {
            id,
            modified: mtime_of(&path),
            title: crate::session::session_title(&path).map(|t| crate::export::redact_str(&t)),
            snippet: crate::export::redact_str(&snippet),
        });
    }
    hits
}

/// Snippet around the first keyword match in a transcript, or None.
fn first_match_snippet(path: &Path, needle: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        for text in record_texts(&v) {
            if let Some(snippet) = match_snippet(&text, needle) {
                return Some(snippet);
            }
        }
    }
    None
}

/// Human-readable text of one transcript record: message content for
/// user/assistant/tool records, the summary text for summary records,
/// the title for custom-title records. JSON keys, ids and timestamps
/// are not searchable.
fn record_texts(v: &serde_json::Value) -> Vec<String> {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "user" | "assistant" | "tool" => match v.get("message").and_then(|m| m.get("content")) {
            Some(c) => vec![content_text(c)],
            None => vec![],
        },
        "summary" => match crate::session::summary_text(v) {
            Some(t) => vec![t],
            None => vec![],
        },
        "custom-title" => match v.get("customTitle").and_then(|t| t.as_str()) {
            Some(t) => vec![t.to_string()],
            None => vec![],
        },
        _ => vec![],
    }
}

/// Plain text of a message `content` field: a string as-is, or the `text`
/// blocks of a content array joined with spaces.
fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Snippet of `text` around the first case-insensitive occurrence of
/// `needle` (already lowercased), with up to SNIPPET_CONTEXT_CHARS of
/// context on each side and "..." where text was cut. None when absent.
fn match_snippet(text: &str, needle: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let byte_idx = lower.find(needle)?;
    let match_chars = lower[..byte_idx].chars().count();
    let needle_chars = needle.chars().count();
    let chars: Vec<char> = text.chars().collect();
    let start = match_chars.saturating_sub(SNIPPET_CONTEXT_CHARS);
    let end = (match_chars + needle_chars + SNIPPET_CONTEXT_CHARS).min(chars.len());
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("...");
    }
    snippet.extend(&chars[start..end]);
    if end < chars.len() {
        snippet.push_str("...");
    }
    Some(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("zen_recall_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&h);
        h
    }

    fn tmp_cwd(home: &Path, tag: &str) -> PathBuf {
        let c = home.join(format!("cwd-{tag}"));
        fs::create_dir_all(&c).unwrap();
        c
    }

    fn write_session(
        home: &Path,
        cwd: &Path,
        id: &str,
        user_text: &str,
        summary: Option<&str>,
    ) -> PathBuf {
        let path = crate::session::ensure_session_path(home, cwd, id).unwrap();
        crate::session::append_typed(
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
            crate::session::append_summary(&path, id, cwd.to_str().unwrap(), "s1", Some("u1"), s)
                .unwrap();
        }
        path
    }

    /// Sessions are ordered by mtime; sleep between creations so the
    /// order is deterministic (same practice as the session tests).
    fn settle() {
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[test]
    fn latest_summary_returns_last_summary() {
        let home = tmp_home("last");
        let cwd = tmp_cwd(&home, "last");
        let path = write_session(&home, &cwd, "sess-last", "hello", Some("first summary"));
        crate::session::append_summary(
            &path,
            "sess-last",
            cwd.to_str().unwrap(),
            "s2",
            None,
            "second summary",
        )
        .unwrap();
        assert_eq!(latest_summary(&path).as_deref(), Some("second summary"));
    }

    #[test]
    fn latest_summary_accepts_plain_string_summary() {
        let home = tmp_home("plain");
        let cwd = tmp_cwd(&home, "plain");
        let path = crate::session::ensure_session_path(&home, &cwd, "sess-plain").unwrap();
        crate::session::append_typed(
            &path,
            "summary",
            "sess-plain",
            cwd.to_str().unwrap(),
            "s1",
            None,
            None,
            Some(serde_json::Value::String("plain form".to_string())),
        )
        .unwrap();
        assert_eq!(latest_summary(&path).as_deref(), Some("plain form"));
    }

    #[test]
    fn latest_summary_none_without_summary_or_file() {
        let home = tmp_home("none");
        let cwd = tmp_cwd(&home, "none");
        let path = write_session(&home, &cwd, "sess-none", "hello", None);
        assert_eq!(latest_summary(&path), None);
        assert_eq!(latest_summary(&home.join("missing.jsonl")), None);
    }

    #[test]
    fn recall_loads_summaries_newest_first_only() {
        let home = tmp_home("order");
        let cwd = tmp_cwd(&home, "order");
        write_session(&home, &cwd, "sess-old", "old raw text", Some("old summary"));
        settle();
        write_session(&home, &cwd, "sess-nosum", "no summary here", None);
        settle();
        write_session(&home, &cwd, "sess-new", "new raw text", Some("new summary"));

        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "sess-new");
        assert_eq!(out[0].text, "new summary");
        assert_eq!(out[1].id, "sess-old");
        assert_eq!(out[1].text, "old summary");
        // Raw transcript text must never leak into recalled content.
        assert!(!out.iter().any(|s| s.text.contains("raw text")));
    }

    #[test]
    fn recall_respects_count_and_zero() {
        let home = tmp_home("count");
        let cwd = tmp_cwd(&home, "count");
        for i in 0..4 {
            write_session(
                &home,
                &cwd,
                &format!("sess-c{i}"),
                "hi",
                Some(&format!("summary {i}")),
            );
            settle();
        }
        assert_eq!(recall_summaries(&home, &cwd, 2, None).len(), 2);
        assert!(recall_summaries(&home, &cwd, 0, None).is_empty());
    }

    #[test]
    fn recall_excludes_current_session() {
        let home = tmp_home("excl");
        let cwd = tmp_cwd(&home, "excl");
        write_session(&home, &cwd, "sess-a", "hi", Some("summary a"));
        settle();
        write_session(&home, &cwd, "sess-b", "hi", Some("summary b"));
        let out = recall_summaries(&home, &cwd, 10, Some("sess-b"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "sess-a");
    }

    #[test]
    fn recall_skips_empty_summary() {
        let home = tmp_home("empty");
        let cwd = tmp_cwd(&home, "empty");
        write_session(&home, &cwd, "sess-blank", "hi", Some("   "));
        write_session(&home, &cwd, "sess-real", "hi", Some("real summary"));
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "sess-real");
    }

    #[test]
    fn recall_cap_truncates_oversized_summary() {
        let home = tmp_home("cap1");
        let cwd = tmp_cwd(&home, "cap1");
        let big = "x".repeat(RECALL_CAP_BYTES + 10_000);
        write_session(&home, &cwd, "sess-big", "hi", Some(&big));
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text.len(), RECALL_CAP_BYTES);
    }

    #[test]
    fn recall_cap_stops_at_total() {
        let home = tmp_home("cap2");
        let cwd = tmp_cwd(&home, "cap2");
        // Newest first: 30 KiB kept whole, next truncated to the 20 KiB
        // left, the oldest dropped entirely.
        write_session(&home, &cwd, "sess-old", "hi", Some(&"o".repeat(30 * 1024)));
        settle();
        write_session(&home, &cwd, "sess-mid", "hi", Some(&"m".repeat(30 * 1024)));
        settle();
        write_session(&home, &cwd, "sess-new", "hi", Some(&"n".repeat(30 * 1024)));
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "sess-new");
        assert_eq!(out[0].text.len(), 30 * 1024);
        assert_eq!(out[1].id, "sess-mid");
        assert_eq!(out[1].text.len(), 20 * 1024);
        let total: usize = out.iter().map(|s| s.text.len()).sum();
        assert_eq!(total, RECALL_CAP_BYTES);
    }

    #[test]
    fn recall_cap_exact_fit_keeps_both_whole() {
        let home = tmp_home("cap3");
        let cwd = tmp_cwd(&home, "cap3");
        write_session(&home, &cwd, "sess-a", "hi", Some(&"a".repeat(25 * 1024)));
        settle();
        write_session(&home, &cwd, "sess-b", "hi", Some(&"b".repeat(25 * 1024)));
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text.len(), 25 * 1024);
        assert_eq!(out[1].text.len(), 25 * 1024);
    }

    #[test]
    fn recall_cap_truncation_is_char_boundary_safe() {
        let home = tmp_home("cap4");
        let cwd = tmp_cwd(&home, "cap4");
        // Multi-byte chars: truncation must not split one (would panic
        // on slicing) and must stay within the cap.
        let big = "é".repeat(RECALL_CAP_BYTES + 100);
        write_session(&home, &cwd, "sess-mb", "hi", Some(&big));
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.len() <= RECALL_CAP_BYTES);
        assert!(out[0].text.len() > RECALL_CAP_BYTES - 2);
        assert!(out[0].text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn recall_redacts_secrets() {
        let home = tmp_home("redact");
        let cwd = tmp_cwd(&home, "redact");
        let secret = "ghp_0123456789abcdefghij";
        write_session(
            &home,
            &cwd,
            "sess-sec",
            "hi",
            Some(&format!("used token {secret} to deploy")),
        );
        let out = recall_summaries(&home, &cwd, 10, None);
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].text.contains(secret),
            "secret leaked: {}",
            out[0].text
        );
        assert!(out[0].text.contains("[REDACTED]"), "text: {}", out[0].text);
    }

    #[test]
    fn recall_block_renders_ids_titles_and_text() {
        let summaries = vec![SessionSummary {
            id: "abcdef123456".to_string(),
            modified: SystemTime::UNIX_EPOCH,
            title: Some("My title".to_string()),
            text: "the summary".to_string(),
        }];
        let block = recall_block(&summaries).expect("non-empty summaries render a block");
        assert!(block.contains("abcdef12"), "block: {block}");
        assert!(block.contains("My title"), "block: {block}");
        assert!(block.contains("the summary"), "block: {block}");
        assert!(block.contains("1970-01-01 00:00"), "block: {block}");
        assert_eq!(recall_block(&[]), None);
    }

    #[test]
    fn parse_recall_count_defaults_and_errors() {
        assert_eq!(parse_recall_count(""), Ok(DEFAULT_RECALL_COUNT));
        assert_eq!(parse_recall_count("   "), Ok(DEFAULT_RECALL_COUNT));
        assert_eq!(parse_recall_count("5"), Ok(5));
        assert_eq!(parse_recall_count("0"), Ok(0));
        assert!(parse_recall_count("abc").is_err());
        assert!(parse_recall_count("-1").is_err());
        assert!(parse_recall_count("3.5").is_err());
    }

    #[test]
    fn search_finds_keyword_in_user_message() {
        let home = tmp_home("s1");
        let cwd = tmp_cwd(&home, "s1");
        write_session(&home, &cwd, "sess-hit", "the quokka escaped", None);
        settle();
        write_session(&home, &cwd, "sess-miss", "nothing here", None);
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sess-hit");
        assert!(
            hits[0].snippet.contains("quokka"),
            "snippet: {}",
            hits[0].snippet
        );
    }

    #[test]
    fn search_is_case_insensitive() {
        let home = tmp_home("s2");
        let cwd = tmp_cwd(&home, "s2");
        write_session(&home, &cwd, "sess-ci", "the Quokka escaped", None);
        assert_eq!(search_sessions(&home, &cwd, "QUOKKA").len(), 1);
        assert_eq!(search_sessions(&home, &cwd, "quokka").len(), 1);
    }

    #[test]
    fn search_finds_keyword_in_summary_and_title() {
        let home = tmp_home("s3");
        let cwd = tmp_cwd(&home, "s3");
        let path = write_session(
            &home,
            &cwd,
            "sess-sum",
            "unrelated",
            Some("fixed the quokka bug"),
        );
        crate::session::append_title(&path, "sess-sum", "quokka release").unwrap();
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sess-sum");
    }

    #[test]
    fn search_ignores_json_keys_and_ids() {
        let home = tmp_home("s4");
        let cwd = tmp_cwd(&home, "s4");
        write_session(&home, &cwd, "sess-keys", "plain text only", None);
        // These strings appear in every transcript line as JSON keys or
        // field names, but they are not message content.
        assert!(search_sessions(&home, &cwd, "sessionId").is_empty());
        assert!(search_sessions(&home, &cwd, "parentUuid").is_empty());
        assert!(search_sessions(&home, &cwd, "timestamp").is_empty());
    }

    #[test]
    fn search_no_match_and_empty_keyword() {
        let home = tmp_home("s5");
        let cwd = tmp_cwd(&home, "s5");
        write_session(&home, &cwd, "sess-nm", "plain text only", None);
        assert!(search_sessions(&home, &cwd, "quokka").is_empty());
        assert!(search_sessions(&home, &cwd, "").is_empty());
        assert!(search_sessions(&home, &cwd, "   ").is_empty());
    }

    #[test]
    fn search_snippet_redacts_secrets() {
        let home = tmp_home("s6");
        let cwd = tmp_cwd(&home, "s6");
        let secret = "ghp_0123456789abcdefghij";
        write_session(
            &home,
            &cwd,
            "sess-sec",
            &format!("quokka deployed with {secret}"),
            None,
        );
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert!(
            !hits[0].snippet.contains(secret),
            "snippet: {}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.contains("[REDACTED]"),
            "snippet: {}",
            hits[0].snippet
        );
    }

    #[test]
    fn search_snippet_trims_long_context() {
        let home = tmp_home("s7");
        let cwd = tmp_cwd(&home, "s7");
        let text = format!("{}quokka{}", "a".repeat(200), "b".repeat(200));
        write_session(&home, &cwd, "sess-long", &text, None);
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.starts_with("..."),
            "snippet: {}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.ends_with("..."),
            "snippet: {}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.contains("quokka"));
        assert!(hits[0].snippet.len() < text.len());

        // Match at the very start: no leading ellipsis.
        let home2 = tmp_home("s7b");
        let cwd2 = tmp_cwd(&home2, "s7b");
        write_session(&home2, &cwd2, "sess-start", "quokka at the start", None);
        let hits2 = search_sessions(&home2, &cwd2, "quokka");
        assert_eq!(hits2.len(), 1);
        assert!(
            !hits2[0].snippet.starts_with("..."),
            "snippet: {}",
            hits2[0].snippet
        );
        assert!(
            !hits2[0].snippet.ends_with("..."),
            "snippet: {}",
            hits2[0].snippet
        );
    }

    #[test]
    fn search_reads_content_array_blocks() {
        let home = tmp_home("s8");
        let cwd = tmp_cwd(&home, "s8");
        let path = crate::session::ensure_session_path(&home, &cwd, "sess-arr").unwrap();
        crate::session::append_typed(
            &path,
            "assistant",
            "sess-arr",
            cwd.to_str().unwrap(),
            "a1",
            None,
            Some(serde_json::json!({"role":"assistant","content":[{"type":"text","text":"the quokka block"},{"type":"tool_use","name":"Read"}]})),
            None,
        )
        .unwrap();
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("quokka"));
    }

    #[test]
    fn search_finds_keyword_only_in_summary_record() {
        let home = tmp_home("s9");
        let cwd = tmp_cwd(&home, "s9");
        write_session(
            &home,
            &cwd,
            "sess-sum",
            "nothing to see here",
            Some("the quokka summary"),
        );
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("quokka"));
    }

    #[test]
    fn search_finds_keyword_only_in_custom_title() {
        let home = tmp_home("s10");
        let cwd = tmp_cwd(&home, "s10");
        let path = crate::session::ensure_session_path(&home, &cwd, "sess-title").unwrap();
        crate::session::append_typed(
            &path,
            "user",
            "sess-title",
            cwd.to_str().unwrap(),
            "u1",
            None,
            Some(serde_json::json!({"role":"user","content":"nothing to see here"})),
            None,
        )
        .unwrap();
        crate::session::append_title(&path, "sess-title", "the quokka plan").unwrap();
        let hits = search_sessions(&home, &cwd, "quokka");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("quokka"));
    }

    #[test]
    fn truncate_to_bytes_keeps_short_strings_whole() {
        assert_eq!(truncate_to_bytes("abc", 3), "abc");
        assert_eq!(truncate_to_bytes("abc", 10), "abc");
        assert_eq!(truncate_to_bytes("abcdef", 3), "abc");
        assert_eq!(truncate_to_bytes("", 0), "");
    }

    #[test]
    fn truncate_to_bytes_backs_off_mid_char() {
        // "é" is 2 bytes: byte 3 of "éééé" is mid-character, so the
        // cut backs off to 2 bytes, not forward to 4.
        assert_eq!(truncate_to_bytes("éééé", 3), "é");
        assert_eq!(truncate_to_bytes("éééé", 5), "éé");
        // 'a' is 1 byte, 'é' occupies bytes 1-2: byte 2 is mid-'é'.
        assert_eq!(truncate_to_bytes("aébc", 2), "a");
    }
}
