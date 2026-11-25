use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

/// Strip a Windows verbatim `\\?\` prefix (and `\\?\UNC\` server prefix)
/// from a canonicalized path, so downstream names use plain DOS paths.
/// UNC paths keep their leading `\\`: `\\?\UNC\server\share` -> `\\server\share`.
/// `std::fs::canonicalize` returns verbatim paths on Windows; leaving the
/// prefix in an encoded directory name puts a `?` in it, which Windows
/// rejects with error 123 when the directory is created.
pub fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// Encode a working directory as one project-directory name.
///
/// Injective (#458): every byte outside `[A-Za-z0-9._-]` is percent-encoded
/// (`%XX`, uppercase hex, over the UTF-8 bytes), and `%` itself encodes as
/// `%25`, so decoding is unambiguous and two distinct paths can never share
/// a name. The old dash mapping sent every separator and `.` to the same
/// `-`, so `/a/b.c`, `/a/b/c` and `/a/b-c` all encoded to `-a-b-c` and
/// their sessions interleaved in one project directory; resuming the
/// latest session for one project could return the other project's
/// session. `.` stays literal: it is legal in a file name on every OS,
/// and keeping it makes the names shorter and still distinct because `/`
/// is encoded (`/a/b.c` -> `%2Fa%2Fb.c`, `/a/b/c` -> `%2Fa%2Fb%2Fc`).
pub fn encode_cwd(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let abs = strip_verbatim_prefix(&abs);
    let s = abs.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Pre-#458 encoding: every separator and `.` maps to `-`, so distinct
/// paths collide (`/a/b.c`, `/a/b/c`, `/a/b-c` all -> `-a-b-c`). Kept
/// only to locate (and migrate) project directories created before the
/// fix. New code must use [`encode_cwd`].
fn legacy_encode_cwd(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let abs = strip_verbatim_prefix(&abs);
    let s = abs.to_string_lossy();
    // Replace every character Windows forbids in a file name, not just the
    // separators: `?` from a verbatim prefix, plus `* " < > |`.
    s.replace(['/', '\\', ':', '.', '?', '*', '"', '<', '>', '|'], "-")
}

/// Inverse of [`encode_cwd`]: `%XX` back to the byte, everything else
/// literal. `None` on a malformed `%` sequence or non-UTF-8 result, so a
/// legacy dash-encoded name (which may contain a stray `%` from the path
/// itself) never decodes into a wrong path.
fn decode_cwd(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = encoded.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Compare two cwd spellings ignoring only form differences: letter case
/// and `/` vs `\`. Punctuation such as `.` vs `-` stays significant, so
/// `/a/b.c` never matches `/a/b-c` (#458) the way the old encoded-name
/// normalization did.
fn normalize_path_form(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// Move the sessions out of a pre-#458 legacy project directory into the
/// project directories the injective encoding gives them, one file at a
/// time by the `cwd` its own first transcript record carries. Colliding
/// projects shared one legacy directory, so a blind rename of the whole
/// directory would hand every session to a single owner; splitting by
/// recorded `cwd` gives each project exactly its own sessions. Files
/// with no parseable recorded `cwd` stay put (they cannot be attributed
/// safely) and the legacy directory is removed only once it is empty.
/// No-op when there is no legacy directory. Called from the write paths
/// (`ensure_session_path`, `ensure_session_dir`) so an upgrade self-heals
/// on next use; read paths never mutate (`latest_session_for_cwd` and
/// `list_sessions_for_cwd` read the legacy directory filtered by recorded
/// `cwd` instead).
fn migrate_legacy_project_dir(home: &Path, cwd: &Path) {
    let new_dir = project_dir(home, &encode_cwd(cwd));
    let legacy_dir = project_dir(home, &legacy_encode_cwd(cwd));
    if legacy_dir == new_dir || !legacy_dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&legacy_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(owner) = recorded_cwd(&path) else {
            continue;
        };
        let owner_dir = project_dir(home, &encode_cwd(Path::new(&owner)));
        if owner_dir == legacy_dir {
            continue;
        }
        if std::fs::create_dir_all(&owner_dir).is_err() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let target = owner_dir.join(name);
        if target.exists() {
            continue;
        }
        let _ = std::fs::rename(&path, &target);
    }
    let _ = std::fs::remove_dir(&legacy_dir);
}

pub fn project_dir(home: &Path, encoded: &str) -> PathBuf {
    home.join(".zen").join("projects").join(encoded)
}

pub fn session_path(proj_dir: &Path, session_id: &str) -> PathBuf {
    proj_dir.join(format!("{}.jsonl", session_id))
}

/// Open a session transcript for appending, creating it owner-only (0600)
/// on Unix. `mode` applies only when the file is created, so an existing
/// transcript keeps its permissions.
fn open_transcript_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Create a project directory, restricting it to owner-only (0700) on Unix
/// when it is newly created. Existing directories keep their permissions.
fn create_project_dir(proj: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let existed = proj.exists();
    std::fs::create_dir_all(proj)?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(proj, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn session_scratch_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("")
}

pub fn find_session_path(home: &Path, id_or_path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(id_or_path);
    if p.is_absolute() && p.exists() {
        return Some(p);
    }
    if p.exists()
        && let Ok(abs) = p.canonicalize()
    {
        return Some(abs);
    }

    let projects_root = home.join(".zen").join("projects");
    if !projects_root.exists() {
        return None;
    }

    for e in walkdir::WalkDir::new(&projects_root).into_iter().flatten() {
        let path = e.path();
        if path
            .file_name()
            .map(|n| n.to_string_lossy() == format!("{}.jsonl", id_or_path))
            .unwrap_or(false)
        {
            return Some(path.to_path_buf());
        }
    }
    None
}

pub fn latest_session_for_cwd(home: &Path, encoded: &str) -> Option<PathBuf> {
    let dir = project_dir(home, encoded);
    if let Some(p) = latest_in_project_dir(&dir, None) {
        return Some(p);
    }
    // Pre-#458 sessions live under the legacy dash-encoded name. The
    // encoding is injective now, so decode it back to the cwd and look in
    // the legacy directory - but only at sessions whose own recorded `cwd`
    // is this cwd. A legacy directory may hold several projects' sessions
    // (that is the #458 collision), so an unfiltered latest pick could
    // still return another project's session. Files found here are moved
    // to their owners' directories by the next write path; this read path
    // never mutates.
    if let Some(cwd) = decode_cwd(encoded) {
        let legacy_dir = project_dir(home, &legacy_encode_cwd(Path::new(&cwd)));
        if legacy_dir != dir {
            let want = encode_cwd(Path::new(&cwd));
            if let Some(p) = latest_in_project_dir(&legacy_dir, Some(&want)) {
                return Some(p);
            }
        }
    }
    // Encoded-name drift fallback (round 5): on Windows the encoded
    // project name is computed from `canonicalize()` output in the
    // calling process, but the directory on disk may have been created
    // by another process whose form of the same cwd differs only in
    // case or separators (verbatim prefix, short/long name forms). The
    // exact-name lookup above then misses a project directory that does
    // hold this cwd's sessions, and `--continue` silently starts a new
    // session. If - and only if - the exact directory yields nothing,
    // retry against sibling project directories whose DECODED path
    // matches under `normalize_path_form`. Comparing decoded paths (not
    // the encoded names, as round 5 did) matters for #458: the old
    // normalization collapsed every run of punctuation to one `-`, so
    // `/a/b.c` and `/a/b-c` compared equal and the fallback itself
    // cross-matched the colliding projects. Decoded-path comparison
    // keeps `.` vs `-` distinct while still forgiving case and
    // separator form. Legacy dash-encoded siblings do not decode to a
    // path, so they never match here; they are covered above.
    let want = decode_cwd(encoded).map(|s| normalize_path_form(&s))?;
    let root = home.join(".zen").join("projects");
    for entry in std::fs::read_dir(&root).ok()? {
        let Ok(entry) = entry else { continue };
        let candidate = entry.path();
        if !candidate.is_dir() {
            continue;
        }
        let Some(name) = candidate
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(decoded) = decode_cwd(&name) else {
            continue;
        };
        if normalize_path_form(&decoded) != want {
            continue;
        }
        if let Some(p) = latest_in_project_dir(&candidate, None) {
            return Some(p);
        }
    }
    None
}

/// `want_cwd` is an `encode_cwd` value: when set, only session files
/// whose own recorded `cwd` encodes to it are considered. Used for the
/// pre-#458 legacy directory, which may hold several projects' sessions;
/// without the filter, "latest" there could be another project's
/// session. `None` keeps the old unfiltered behaviour for a directory
/// that belongs to exactly one (new-scheme) encoding.
fn latest_in_project_dir(dir: &Path, want_cwd: Option<&str>) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    // "Latest" is decided by the timestamp the session itself recorded
    // in its first transcript record - NOT by file mtime. The round-3 PC
    // probe (section D) showed two session files with distinct mtimes
    // listed in creation order, yet `--continue` still did not resolve
    // to the newer one, so filesystem timestamps are not a trustworthy
    // "latest" signal on that machine. The recorded timestamp is written
    // by the session writer when the session starts and travels with the
    // file content. Files without a parseable recorded timestamp fall
    // back to the file creation time, and the file name is the final
    // tie-break, so the result never depends on directory listing order.
    let mut latest: Option<((i64, u32), SystemTime, String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(want) = want_cwd
            && recorded_cwd(&path).is_none_or(|c| encode_cwd(Path::new(&c)) != want)
        {
            continue;
        }
        let recorded = recorded_timestamp_key(&path);
        let created = entry
            .metadata()
            .and_then(|m| m.created())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let replace = match &latest {
            None => true,
            Some((t, c, n, _)) => (recorded, created, name.as_str()) > (*t, *c, n.as_str()),
        };
        if replace {
            latest = Some((recorded, created, name, path));
        }
    }
    latest.map(|(_, _, _, p)| p)
}

/// The first transcript record of a session file, if it parses as JSON.
/// A torn first line (crash mid-append) yields `None`; callers that need
/// the value treat that as "unknown", never as a match.
fn first_record(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    let first_line = content.lines().next()?;
    serde_json::from_str::<Value>(first_line).ok()
}

/// The `cwd` the session writer recorded when the session started.
/// Attribution for the pre-#458 legacy directory: each file carries its
/// owner's cwd, which is how colliding projects sharing one legacy
/// directory are told apart again (migration and the legacy fallback in
/// `latest_session_for_cwd`).
fn recorded_cwd(path: &Path) -> Option<String> {
    first_record(path)?.get("cwd")?.as_str().map(String::from)
}

/// Ordering key from the `timestamp` of the first record in a session
/// file: (unix seconds, sub-second nanos) of the RFC 3339 value the
/// session writer recorded when the session started. `(i64::MIN, 0)`
/// when the file has no parseable first-record timestamp, so such files
/// sort before any timestamped session and the creation-time fallback
/// decides between them.
fn recorded_timestamp_key(path: &Path) -> (i64, u32) {
    let fallback = (i64::MIN, 0);
    let Some(v) = first_record(path) else {
        return fallback;
    };
    let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) else {
        return fallback;
    };
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => (dt.timestamp(), dt.timestamp_subsec_nanos()),
        Err(_) => fallback,
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct TranscriptRecord<'a> {
    r#type: &'a str,
    uuid: String,
    parent_uuid: Option<String>,
    session_id: String,
    timestamp: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Value>,
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

pub fn append_record(path: &Path, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_transcript_append(path)?;
    use std::io::Write;
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // floor-guard: params-struct refactor deferred
pub fn append_typed(
    path: &Path,
    r#type: &str,
    session_id: &str,
    cwd: &str,
    uuid_val: &str,
    parent_uuid: Option<&str>,
    message: Option<Value>,
    summary: Option<Value>,
) -> Result<()> {
    let rec = TranscriptRecord {
        r#type,
        uuid: uuid_val.to_string(),
        parent_uuid: parent_uuid.map(String::from),
        session_id: session_id.to_string(),
        timestamp: now_iso(),
        cwd: cwd.to_string(),
        message,
        summary,
    };
    append_record(path, &serde_json::to_value(rec)?)
}

pub fn load_messages(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(path)?;
    let mut records: Vec<Value> = vec![];
    for (n, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                // A torn line (crash mid-append) must not brick the whole
                // session: skip it with a warning, keep valid records.
                eprintln!(
                    "warning: {}:{}: skipping invalid transcript line: {}",
                    path.display(),
                    n + 1,
                    e
                );
                continue;
            }
        };
        records.push(v);
    }
    let last_summary_idx = records
        .iter()
        .rposition(|v| v.get("type").and_then(|t| t.as_str()) == Some("summary"));

    let mut out = vec![];
    let start = match last_summary_idx {
        Some(i) => {
            if let Some(text) = summary_text(&records[i]) {
                out.push(serde_json::json!({ "role": "user", "content": text }));
            }
            i + 1
        }
        None => 0,
    };
    for v in &records[start..] {
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if typ == "system" || typ == "summary" || typ == "plan_approved" {
            continue;
        }
        if let Some(msg) = v.get("message") {
            out.push(msg.clone());
        }
    }
    Ok(out)
}

pub(crate) fn summary_text(rec: &Value) -> Option<String> {
    let s = rec.get("summary")?;
    match s {
        Value::String(t) => Some(t.clone()),
        Value::Object(o) => o.get("summary").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

pub fn load_history(path: &Path) -> Result<Vec<zen_core::ChatMessage>> {
    Ok(load_messages(path)?
        .into_iter()
        .filter_map(|v| serde_json::from_value::<zen_core::ChatMessage>(v).ok())
        .collect())
}

pub struct TranscriptWriter {
    pub path: Option<PathBuf>,
    pub session_id: String,
    pub cwd: String,
    last_uuid: Option<String>,

    file: Option<std::io::BufWriter<std::fs::File>>,
}

impl TranscriptWriter {
    pub fn new(path: PathBuf, session_id: &str, cwd: &Path) -> Self {
        Self {
            path: Some(path),
            session_id: session_id.to_string(),
            cwd: cwd.display().to_string(),
            last_uuid: None,
            file: None,
        }
    }

    pub fn ephemeral(session_id: &str, cwd: &Path) -> Self {
        Self {
            path: None,
            session_id: session_id.to_string(),
            cwd: cwd.display().to_string(),
            last_uuid: None,
            file: None,
        }
    }

    fn append_line(&mut self, record: &Value) -> Result<()> {
        use std::io::Write;
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if self.file.is_none() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let f = open_transcript_append(path)?;
            self.file = Some(std::io::BufWriter::new(f));
        }
        let line = serde_json::to_string(record)?;
        let w = self.file.as_mut().expect("opened above");
        let res = writeln!(w, "{}", line).and_then(|_| w.flush());
        if let Err(e) = res {
            self.file = None;
            return Err(e.into());
        }
        Ok(())
    }

    /// Flush any buffered transcript bytes to disk. Records are
    /// already flushed by `append_line`, so this is a no-op when
    /// nothing is buffered; it exists so a degraded session can
    /// flush explicitly instead of relying on that.
    pub fn flush(&mut self) {
        use std::io::Write;
        if let Some(w) = self.file.as_mut() {
            let _ = w.flush();
        }
    }

    fn write(&mut self, kind: &str, message: Option<Value>, summary: Option<Value>) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let rec = TranscriptRecord {
            r#type: kind,
            uuid: id.clone(),
            parent_uuid: self.last_uuid.clone(),
            session_id: self.session_id.clone(),
            timestamp: now_iso(),
            cwd: self.cwd.clone(),
            message,
            summary,
        };
        self.append_line(&serde_json::to_value(rec)?)?;
        self.last_uuid = Some(id);
        Ok(())
    }

    pub fn persist_turn(
        &mut self,
        before: &[zen_core::ChatMessage],
        after: &[zen_core::ChatMessage],
    ) -> Result<()> {
        let extends = after.len() >= before.len() && after[..before.len()] == *before;
        let fresh: &[zen_core::ChatMessage] = if extends {
            &after[before.len()..]
        } else {
            let (first, rest) = match after.split_first() {
                Some(x) => x,
                None => return Ok(()),
            };
            let text = first.text();
            self.write("summary", None, Some(Value::String(text)))?;
            rest
        };
        for m in fresh {
            let kind = match m.role.as_str() {
                "assistant" => "assistant",
                "tool" => "tool",
                _ => "user",
            };
            self.write(kind, Some(serde_json::to_value(m)?), None)?;
        }
        Ok(())
    }

    pub fn record_plan_approved(&mut self, plan: &str) -> Result<()> {
        self.write(
            "plan_approved",
            Some(serde_json::json!({ "plan": plan })),
            None,
        )
    }
}

pub fn ensure_session_path(home: &Path, cwd: &Path, session_id: &str) -> Result<PathBuf> {
    let encoded = encode_cwd(cwd);
    let proj = project_dir(home, &encoded);
    migrate_legacy_project_dir(home, cwd);
    create_project_dir(&proj)?;
    Ok(session_path(&proj, session_id))
}

pub fn ensure_session_dir(home: &Path, cwd: &Path, session_id: &str) -> Result<PathBuf> {
    let encoded = encode_cwd(cwd);
    let proj = project_dir(home, &encoded);
    migrate_legacy_project_dir(home, cwd);
    create_project_dir(&proj)?;
    let _ = session_id;
    Ok(proj)
}

pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn list_sessions_for_cwd(home: &Path, cwd: &Path) -> Vec<(String, PathBuf)> {
    let encoded = encode_cwd(cwd);
    let dir = project_dir(home, &encoded);
    let mut sessions = sessions_in_dir(&dir, None);
    // Pre-#458 sessions not yet migrated out of the legacy directory:
    // include this cwd's own files (attributed by recorded `cwd`),
    // never another project's files sharing that directory. Read-only:
    // the files move to their owners' directories on the next write.
    let legacy_dir = project_dir(home, &legacy_encode_cwd(cwd));
    if legacy_dir != dir {
        sessions.extend(sessions_in_dir(&legacy_dir, Some(&encoded)));
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    sessions.into_iter().map(|(_, id, p)| (id, p)).collect()
}

/// `(mtime, session id, path)` of the `*.jsonl` files in one project
/// directory, newest first by mtime. With `want_cwd` set (an
/// `encode_cwd` value), only files whose own recorded `cwd` encodes to
/// it are returned - the legacy-directory filter of #458.
fn sessions_in_dir(dir: &Path, want_cwd: Option<&str>) -> Vec<(SystemTime, String, PathBuf)> {
    if !dir.exists() {
        return vec![];
    }
    let mut sessions: Vec<(SystemTime, String, PathBuf)> = vec![];
    for entry in std::fs::read_dir(dir).ok().into_iter().flatten() {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(want) = want_cwd
            && recorded_cwd(&path).is_none_or(|c| encode_cwd(Path::new(&c)) != want)
        {
            continue;
        }
        if let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            sessions.push((mtime, stem.to_string(), path));
        }
    }
    sessions
}

pub fn append_usage(path: &Path, session_id: &str, usage: &zen_core::UsageStats) -> Result<()> {
    append_record(
        path,
        &serde_json::json!({
            "type": "system",
            "subtype": "usage",
            "sessionId": session_id,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "usage": {"input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens, "cache_read_input_tokens": usage.cache_read_input_tokens},
        }),
    )
}

pub fn load_usage(path: &Path) -> zen_core::UsageStats {
    let mut total = zen_core::UsageStats::default();
    let Ok(content) = std::fs::read_to_string(path) else {
        return total;
    };
    for v in content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
    {
        if v.get("subtype").and_then(|t| t.as_str()) == Some("usage") {
            let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            total.input_tokens += n("input_tokens");
            total.output_tokens += n("output_tokens");
            total.cache_read_input_tokens += n("cache_read_input_tokens");
        }
    }
    total
}

pub fn append_title(path: &Path, session_id: &str, title: &str) -> Result<()> {
    append_record(
        path,
        &serde_json::json!({
            "type": "custom-title",
            "sessionId": session_id,
            "customTitle": title,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
    )
}

pub fn session_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("custom-title"))
        .filter_map(|v| {
            v.get("customTitle")
                .and_then(|t| t.as_str())
                .map(String::from)
        })
        .next_back()
}

pub fn append_summary(
    path: &Path,
    session_id: &str,
    cwd: &str,
    uuid_val: &str,
    parent_uuid: Option<&str>,
    summary_text: &str,
) -> Result<()> {
    let summary_val = serde_json::json!({"type":"summary","summary":summary_text});
    append_typed(
        path,
        "summary",
        session_id,
        cwd,
        uuid_val,
        parent_uuid,
        None,
        Some(summary_val),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // The pre-#458 dash encoding, replicated here (not via
    // `legacy_encode_cwd`, so the migration test also runs against the
    // unfixed code and fails there). It must replicate the old encoder
    // faithfully: `canonicalize()` returns a verbatim `\\?\...` path on
    // Windows, and the old encoder stripped that prefix before the dash
    // mapping. Replicating the mapping without the strip builds a legacy
    // directory name (`----C--...`) production code never creates and
    // never looks up, so on Windows the migration test's lookup misses
    // and panics unwrapping `None` (PR #537 windows-check, session.rs:939).
    fn legacy_encode(p: &Path) -> String {
        strip_verbatim_prefix(&p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
            .to_string_lossy()
            .replace(['/', '\\', ':', '.', '?', '*', '"', '<', '>', '|'], "-")
    }

    #[test]
    fn p0b_1_session_scratch_dir_sits_next_to_the_transcript() {
        let t = Path::new("/h/.zen/projects/-p/abc-123.jsonl");
        assert_eq!(
            session_scratch_dir(t),
            PathBuf::from("/h/.zen/projects/-p/abc-123")
        );
    }

    #[test]
    fn ephemeral_writer_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("zen_eph_w_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ghost = dir.join(".zen").join("projects").join("x").join("s.jsonl");
        let mut w = TranscriptWriter::ephemeral("s", &dir);
        assert!(w.path.is_none());

        w.persist_turn(
            &[],
            &[zen_core::ChatMessage {
                role: "user".into(),
                content: Some("hello".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
        )
        .unwrap();
        assert!(!ghost.exists());
        assert!(!dir.join(".zen").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_cwd_uses_only_file_name_safe_chars() {
        let tmp = std::env::temp_dir().join(format!("zenenc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let s = encode_cwd(&tmp);
        assert!(!s.contains('/'));
        assert!(!s.contains('\\'));
        assert!(!s.contains(':'));
        // Windows verbatim `\\?\` prefix must not leak a `?` into the name
        // (error 123 when the project dir is created), nor any other
        // Windows-illegal file-name character.
        assert!(!s.contains('?'));
        assert!(!s.contains('*'));
        assert!(!s.contains('"'));
        assert!(!s.contains('<'));
        assert!(!s.contains('>'));
        assert!(!s.contains('|'));
        // `.` is legal in a file name and stays literal under #458, so
        // the old "no dots" assertion no longer applies.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn encode_cwd_is_injective_for_dot_slash_and_dash() {
        // #458: the old dash mapping collapsed all three to `-a-b-c`.
        // They need not exist: canonicalize() falls back to the path
        // as given.
        let a = encode_cwd(Path::new("/a/b.c"));
        let b = encode_cwd(Path::new("/a/b/c"));
        let c = encode_cwd(Path::new("/a/b-c"));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn decode_cwd_roundtrips_encode_cwd() {
        for p in ["/a/b.c", "/a/b/c", "/a/b-c", "/a%b", "/a b/c", "/café/x"] {
            let enc = encode_cwd(Path::new(p));
            assert_eq!(decode_cwd(&enc).unwrap(), p);
        }
        // Malformed `%` never decodes to a wrong path.
        assert!(decode_cwd("%2").is_none());
        assert!(decode_cwd("%zz").is_none());
        assert!(decode_cwd("%2Ftmp%").is_none());
    }

    #[test]
    fn colliding_cwds_get_separate_project_dirs() {
        // #458: /a/b.c, /a/b/c, /a/b-c shared one project directory
        // under the old encoding, so --continue could resume another
        // project's session.
        let base = std::env::temp_dir().join(format!("zen_collide_{}", Uuid::new_v4()));
        let home = base.join("home");
        let cwd_dot = base.join("a").join("b.c");
        let cwd_slash = base.join("a").join("b").join("c");
        let cwd_dash = base.join("a").join("b-c");
        for d in [&cwd_dot, &cwd_slash, &cwd_dash] {
            fs::create_dir_all(d).unwrap();
        }
        let p_dot = ensure_session_path(&home, &cwd_dot, "sess-dot").unwrap();
        let p_slash = ensure_session_path(&home, &cwd_slash, "sess-slash").unwrap();
        let p_dash = ensure_session_path(&home, &cwd_dash, "sess-dash").unwrap();
        assert_ne!(p_dot.parent().unwrap(), p_slash.parent().unwrap());
        assert_ne!(p_dot.parent().unwrap(), p_dash.parent().unwrap());
        assert_ne!(p_slash.parent().unwrap(), p_dash.parent().unwrap());
        for (p, id, cwd) in [
            (&p_dot, "sess-dot", &cwd_dot),
            (&p_slash, "sess-slash", &cwd_slash),
            (&p_dash, "sess-dash", &cwd_dash),
        ] {
            append_typed(
                p,
                "user",
                id,
                cwd.to_str().unwrap(),
                "u",
                None,
                Some(serde_json::json!({"role":"user","content":"hi"})),
                None,
            )
            .unwrap();
        }
        // --continue for each cwd resolves to its own session only.
        for (cwd, want) in [
            (&cwd_dot, "sess-dot.jsonl"),
            (&cwd_slash, "sess-slash.jsonl"),
            (&cwd_dash, "sess-dash.jsonl"),
        ] {
            let latest = latest_session_for_cwd(&home, &encode_cwd(cwd)).unwrap();
            assert_eq!(latest.file_name().unwrap(), want);
            let listed = list_sessions_for_cwd(&home, cwd);
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].0, want.trim_end_matches(".jsonl"));
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_project_dir_is_migrated_and_split_by_cwd() {
        // Simulate a pre-#458 home: two projects' sessions share one
        // legacy dash-encoded directory.
        let base = std::env::temp_dir().join(format!("zen_legacy_{}", Uuid::new_v4()));
        let home = base.join("home");
        let cwd_dot = base.join("a").join("b.c");
        let cwd_slash = base.join("a").join("b").join("c");
        for d in [&cwd_dot, &cwd_slash] {
            fs::create_dir_all(d).unwrap();
        }
        // The old dash mapping (see `legacy_encode` above): the legacy
        // directory both colliding projects share.
        let legacy_dir = home
            .join(".zen")
            .join("projects")
            .join(legacy_encode(&cwd_dot));
        assert_eq!(
            legacy_dir,
            home.join(".zen")
                .join("projects")
                .join(legacy_encode(&cwd_slash))
        );
        fs::create_dir_all(&legacy_dir).unwrap();
        // One session per project, each recording its own cwd.
        for (id, cwd) in [("old-dot", &cwd_dot), ("old-slash", &cwd_slash)] {
            let p = legacy_dir.join(format!("{id}.jsonl"));
            append_typed(
                &p,
                "user",
                id,
                cwd.to_str().unwrap(),
                "u",
                None,
                Some(serde_json::json!({"role":"user","content":"hi"})),
                None,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        // Before any new session is created, --continue must already
        // return each project's own legacy session, not the other's.
        let found_dot = latest_session_for_cwd(&home, &encode_cwd(&cwd_dot)).unwrap();
        assert_eq!(found_dot.file_name().unwrap(), "old-dot.jsonl");
        let found_slash = latest_session_for_cwd(&home, &encode_cwd(&cwd_slash)).unwrap();
        assert_eq!(found_slash.file_name().unwrap(), "old-slash.jsonl");
        assert_eq!(list_sessions_for_cwd(&home, &cwd_dot).len(), 1);
        assert_eq!(list_sessions_for_cwd(&home, &cwd_slash).len(), 1);
        // Creating a new session migrates the legacy directory: each
        // file moves to its owner's new directory and the legacy one
        // goes away.
        let _ = ensure_session_path(&home, &cwd_dot, "new-dot").unwrap();
        assert!(!legacy_dir.exists());
        assert!(
            project_dir(&home, &encode_cwd(&cwd_dot))
                .join("old-dot.jsonl")
                .exists()
        );
        assert!(
            project_dir(&home, &encode_cwd(&cwd_slash))
                .join("old-slash.jsonl")
                .exists()
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_encode_matches_production_for_verbatim_windows_paths() {
        // Regression test for the PR #537 windows-check failure: on
        // Windows `canonicalize()` returns verbatim paths
        // (`\\?\C:\...`, `\\?\UNC\server\share\...`), and the pre-#458
        // encoder stripped that prefix before the dash mapping, so a
        // real legacy directory is named `C--...`, never `----C--...`.
        // These are pure string paths (nothing on disk), so the test
        // runs on every OS: `canonicalize()` falls back to the path
        // itself for them and the strip is a pure string transform.
        // With the old helper (no strip) this fails; with the fix it passes.
        for p in [
            r"\\?\C:\Users\runner\work\a\b.c",
            r"\\?\UNC\server\share\a\b",
            r"C:\a\b.c",
        ] {
            assert_eq!(legacy_encode(Path::new(p)), legacy_encode_cwd(Path::new(p)));
        }
    }

    #[test]
    fn latest_session_finds_case_variant_project_dir() {
        // Round-5 drift fallback, reworked for #458: a sibling project
        // directory whose DECODED path differs from the requested cwd
        // only in letter case still resolves, so --continue does not
        // silently start a new session on Windows form drift.
        let base = std::env::temp_dir().join(format!("zen_drift_{}", Uuid::new_v4()));
        let home = base.join("home");
        let cwd = base.join("drift").join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let encoded = encode_cwd(&cwd);
        // Same path, different letter case: on disk the directory was
        // created from another spelling of the cwd.
        let variant = encoded.to_uppercase();
        assert_ne!(variant, encoded);
        let dir = home.join(".zen").join("projects").join(&variant);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("drifted.jsonl");
        append_typed(
            &p,
            "user",
            "drifted",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();
        let latest = latest_session_for_cwd(&home, &encoded).unwrap();
        assert_eq!(latest.file_name().unwrap(), "drifted.jsonl");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn strip_verbatim_prefix_handles_drive_and_unc() {
        // Pure string transform, so this runs on every OS, including Linux CI.
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\foo\bar")),
            PathBuf::from(r"C:\foo\bar")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\dir")),
            PathBuf::from(r"\\server\share\dir")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\foo\bar")),
            PathBuf::from(r"C:\foo\bar")
        );
    }

    #[test]
    #[cfg(unix)]
    fn new_session_file_and_project_dir_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("zen_sess_0600_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_0600_cwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();

        let path = ensure_session_path(&home, &cwd, "sess-0600-1").unwrap();
        append_typed(
            &path,
            "user",
            "sess-0600-1",
            cwd.to_str().unwrap(),
            "u1",
            None,
            Some(serde_json::json!({"role":"user","content":"hello"})),
            None,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let proj = path.parent().unwrap();
        assert_eq!(
            fs::metadata(proj).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn append_then_load_roundtrip() {
        let home = std::env::temp_dir().join(format!("zen_sess_rt_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_cwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();

        let path = ensure_session_path(&home, &cwd, "sess-rt-1").unwrap();
        let m1 = serde_json::json!({"role":"user","content":"hello"});
        let m2 = serde_json::json!({"role":"assistant","content":"hi there"});
        append_typed(
            &path,
            "user",
            "sess-rt-1",
            cwd.to_str().unwrap(),
            "u1",
            None,
            Some(m1.clone()),
            None,
        )
        .unwrap();
        append_typed(
            &path,
            "assistant",
            "sess-rt-1",
            cwd.to_str().unwrap(),
            "a1",
            Some("u1"),
            Some(m2.clone()),
            None,
        )
        .unwrap();

        let pre = load_messages(&path).unwrap();
        assert_eq!(pre, vec![m1.clone(), m2.clone()]);

        append_typed(
            &path,
            "summary",
            "sess-rt-1",
            cwd.to_str().unwrap(),
            "sum1",
            Some("a1"),
            None,
            Some(serde_json::json!({"summary":"done"})),
        )
        .unwrap();
        let m3 = serde_json::json!({"role":"user","content":"after"});
        append_typed(
            &path,
            "user",
            "sess-rt-1",
            cwd.to_str().unwrap(),
            "u2",
            Some("sum1"),
            Some(m3.clone()),
            None,
        )
        .unwrap();

        let msgs = load_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], serde_json::json!({"role":"user","content":"done"}));
        assert_eq!(msgs[1], m3);

        let first: Value = serde_json::from_str(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert!(first.get("sessionId").is_some() && first.get("parentUuid").is_some());
    }

    #[test]
    fn load_messages_skips_torn_lines_and_keeps_valid_messages() {
        let home = std::env::temp_dir().join(format!("zen_sess_torn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_torn_cwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();

        let path = ensure_session_path(&home, &cwd, "sess-torn-1").unwrap();
        let m1 = serde_json::json!({"role":"user","content":"hello"});
        let m2 = serde_json::json!({"role":"assistant","content":"hi there"});
        append_typed(
            &path,
            "user",
            "sess-torn-1",
            cwd.to_str().unwrap(),
            "u1",
            None,
            Some(m1.clone()),
            None,
        )
        .unwrap();
        // Garbage line mid-file, as after a crash mid-append.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "not json").unwrap();
        }
        append_typed(
            &path,
            "assistant",
            "sess-torn-1",
            cwd.to_str().unwrap(),
            "a1",
            Some("u1"),
            Some(m2.clone()),
            None,
        )
        .unwrap();
        // Torn trailing line: unterminated JSON, as after SIGKILL mid-append.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            std::io::Write::write_all(&mut f, b"{\"type\":\"user\",\"mes").unwrap();
        }

        let msgs = load_messages(&path).unwrap();
        assert_eq!(msgs, vec![m1.clone(), m2.clone()]);
        let history = load_history(&path).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text_ref(), Some("hello"));
        assert_eq!(history[1].text_ref(), Some("hi there"));
    }

    #[test]
    fn transcript_writer_appends_delta_and_summary_on_compaction() {
        use zen_core::ChatMessage;
        let dir = std::env::temp_dir().join(format!("zen_tw_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut w = TranscriptWriter::new(path.clone(), "s", &dir);
        let t1 = vec![
            ChatMessage::user("q1"),
            ChatMessage::assistant(Some("a1".into()), None),
        ];
        w.persist_turn(&[], &t1).unwrap();
        let mut t2 = t1.clone();
        t2.push(ChatMessage::user("q2"));
        t2.push(ChatMessage::assistant(Some("a2".into()), None));
        w.persist_turn(&t1, &t2).unwrap();
        assert_eq!(load_history(&path).unwrap(), t2);

        let t3 = vec![
            ChatMessage::user("[Compaction summary of the conversation so far]\nS"),
            ChatMessage::assistant(Some("a2".into()), None),
            ChatMessage::user("q3"),
        ];
        w.persist_turn(&t2, &t3).unwrap();
        assert_eq!(load_history(&path).unwrap(), t3);

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for pair in lines.windows(2) {
            assert_eq!(pair[1]["parentUuid"], pair[0]["uuid"]);
        }
    }

    #[test]
    fn latest_session_picks_newest() {
        let home = std::env::temp_dir().join(format!("zen_sess_latest_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_lcwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();

        let p1 = ensure_session_path(&home, &cwd, "old").unwrap();
        append_typed(
            &p1,
            "user",
            "old",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let p2 = ensure_session_path(&home, &cwd, "new").unwrap();
        append_typed(
            &p2,
            "user",
            "new",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();

        let latest = latest_session_for_cwd(&home, &encode_cwd(&cwd)).unwrap();
        assert_eq!(latest.file_name().unwrap(), "new.jsonl");
    }

    #[test]
    fn find_session_by_id() {
        let home = std::env::temp_dir().join(format!("zen_sess_find_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_fcwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();
        let path = ensure_session_path(&home, &cwd, "findme").unwrap();
        append_typed(
            &path,
            "user",
            "findme",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();
        assert!(path.exists());
        let found = find_session_path(&home, "findme").unwrap();
        assert_eq!(found, path);
        assert!(find_session_path(&home, "nonexistent").is_none());
    }

    #[test]
    fn find_session_by_path() {
        let home = std::env::temp_dir().join(format!("zen_sess_findp_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_sess_fpcwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();
        let path = ensure_session_path(&home, &cwd, "sess-x").unwrap();
        append_typed(
            &path,
            "user",
            "sess-x",
            cwd.to_str().unwrap(),
            "u",
            None,
            None,
            None,
        )
        .unwrap();
        let found = find_session_path(&home, path.to_str().unwrap()).unwrap();
        assert_eq!(found, path);
    }

    #[test]
    fn list_sessions_for_cwd_returns_sorted() {
        let home = std::env::temp_dir().join(format!("zen_sess_list_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        // NOTE: keep this cwd dir unique to this test: the sibling latest_session test shares the process id, and sharing the cwd dir let one test delete it mid-run, so encode_cwd fell back to a non-canonical path and the lookup missed (flaky on macOS/Windows).
        let cwd = std::env::temp_dir().join(format!("zen_sess_list_cwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();

        let p1 = ensure_session_path(&home, &cwd, "sess-a").unwrap();
        append_typed(
            &p1,
            "user",
            "sess-a",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let p2 = ensure_session_path(&home, &cwd, "sess-b").unwrap();
        append_typed(
            &p2,
            "user",
            "sess-b",
            cwd.to_str().unwrap(),
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();

        let sessions = list_sessions_for_cwd(&home, &cwd);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, "sess-b");
        assert_eq!(sessions[1].0, "sess-a");
    }

    #[test]
    fn p0_2_transcript_writer_keeps_file_open_and_flushes_each_record() {
        use zen_core::ChatMessage;
        let dir = std::env::temp_dir().join(format!("zen_tw_open_{}", Uuid::new_v4()));
        let path = dir.join("nested").join("s.jsonl");
        let mut w = TranscriptWriter::new(path.clone(), "s", &dir);
        assert!(w.file.is_none(), "nothing opened before the first record");
        let mut hist = vec![];
        for i in 0..5 {
            let before = hist.clone();
            hist.push(ChatMessage::user(format!("q{}", i)));
            w.persist_turn(&before, &hist).unwrap();
            assert!(w.file.is_some(), "handle stays open between records");

            let users = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|l| l.contains("\"type\":\"user\""))
                .count();
            assert_eq!(users, i + 1);

            if i == 2 {
                append_usage(&path, "s", &zen_core::UsageStats::default()).unwrap();
            }
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 6);
        for l in content.lines() {
            serde_json::from_str::<Value>(l).expect("every line is whole JSON");
        }
        let msgs = load_messages(&path).unwrap();
        assert_eq!(msgs.len(), 5);

        let recs: Vec<Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let users: Vec<&Value> = recs.iter().filter(|r| r["type"] == "user").collect();
        for pair in users.windows(2) {
            assert_eq!(pair[1]["parentUuid"], pair[0]["uuid"]);
        }
    }

    #[test]
    fn normalize_path_form_lowercases_and_unifies_separators() {
        // Kills the cargo-mutants "replace body with String::new() /
        // "xyzzy" mutants on normalize_path_form (line 91): a constant
        // return makes every decoded path compare equal in the drift
        // fallback of latest_session_for_cwd, so --continue could resolve
        // to a sibling project's session. The existing case-variant test
        // has only one sibling directory, so it passes under the mutant.
        assert_eq!(normalize_path_form("/A/B.C"), "/a/b.c");
        assert_eq!(
            normalize_path_form(r"C:\Users\Runner\Work"),
            "c:/users/runner/work"
        );
        assert_eq!(normalize_path_form("/a\\b/c"), "/a/b/c");
        // Punctuation stays significant: `.` vs `-` must not collapse (#458).
        assert_ne!(normalize_path_form("/a/b.c"), normalize_path_form("/a/b-c"));
        assert_eq!(normalize_path_form(""), "");
    }

    #[test]
    fn migrate_legacy_project_dir_skips_when_legacy_and_new_coincide() {
        // Kills the `||` -> `&&` mutant on the early-return guard (line
        // 110). A relative cwd that does not exist on disk makes
        // canonicalize() fail, so both encoders fall back to the path as
        // given and agree on the name (only file-name-safe chars), and the
        // legacy directory IS the new directory. Production cwds are
        // absolute, so this coincidence is only reachable here; the test
        // pins the guard's intended logic directly.
        let base = std::env::temp_dir().join(format!("zen_mut2_{}", Uuid::new_v4()));
        let home = base.join("home");
        let rel = format!("zen_mut2_rel_{}", Uuid::new_v4());
        let rel_cwd = Path::new(&rel);
        let legacy_dir = project_dir(&home, &encode_cwd(rel_cwd));
        assert_eq!(legacy_dir, project_dir(&home, &legacy_encode_cwd(rel_cwd)));
        fs::create_dir_all(&legacy_dir).unwrap();
        // A session belonging to another project, stranded in the shared dir.
        let other_cwd = format!("/tmp/zen-mut2-other-{}", Uuid::new_v4());
        let other_path = legacy_dir.join("other.jsonl");
        append_typed(
            &other_path,
            "user",
            "other",
            &other_cwd,
            "u",
            None,
            Some(serde_json::json!({"role":"user","content":"hi"})),
            None,
        )
        .unwrap();
        // Correct code returns early: nothing moves, the directory stays.
        // Under the `&&` mutant the guard no longer fires, the foreign
        // file is migrated out to its owner's directory and the legacy
        // directory is removed.
        migrate_legacy_project_dir(&home, rel_cwd);
        assert!(
            legacy_dir.exists(),
            "legacy dir must survive when it coincides with the new dir"
        );
        assert!(
            other_path.exists(),
            "foreign session must stay put when migration is skipped"
        );
        assert!(
            !project_dir(&home, &encode_cwd(Path::new(&other_cwd)))
                .join("other.jsonl")
                .exists()
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn latest_session_prefers_old_timestamp_over_missing_timestamp() {
        // Kills the cargo-mutants mutants replacing the (i64::MIN, 0)
        // fallback in recorded_timestamp_key (line 355) with (-1, 1),
        // (0, 0) or (1, 1): a session with no parseable timestamp must sort
        // before ANY timestamped session, even one from 1960. The existing
        // latest_session_picks_newest test stamps both files with now, so
        // the fallback is never exercised there.
        let base = std::env::temp_dir().join(format!("zen_mut3_{}", Uuid::new_v4()));
        let home = base.join("home");
        let cwd = base.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let dir = project_dir(&home, &encode_cwd(&cwd));
        fs::create_dir_all(&dir).unwrap();
        let cwd_str = cwd.to_str().unwrap();
        // No parseable timestamp: the fallback (i64::MIN, 0) applies.
        let no_ts = dir.join("aaa-no-ts.jsonl");
        fs::write(
            &no_ts,
            serde_json::json!({
                "type": "user",
                "uuid": "u1",
                "session_id": "aaa-no-ts",
                "cwd": cwd_str,
                "message": {"role": "user", "content": "hi"},
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(recorded_timestamp_key(&no_ts), (i64::MIN, 0));
        // A real timestamp from 1960: negative unix seconds, still newer
        // than "no timestamp at all".
        let old = dir.join("zzz-old.jsonl");
        fs::write(
            &old,
            serde_json::json!({
                "type": "user",
                "uuid": "u2",
                "session_id": "zzz-old",
                "timestamp": "1960-01-01T00:00:00+00:00",
                "cwd": cwd_str,
                "message": {"role": "user", "content": "hi"},
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(recorded_timestamp_key(&old), (-315619200, 0));
        // Behavioural check: --continue resolves to the 1960 session, not
        // the timestamp-less one. Under any of the three fallback mutants
        // the timestamp-less file's key (-1/0/1, ...) beats 1960 and wins.
        let latest = latest_session_for_cwd(&home, &encode_cwd(&cwd)).unwrap();
        assert_eq!(latest.file_name().unwrap(), "zzz-old.jsonl");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_sessions_for_cwd_filters_legacy_dir_by_recorded_cwd() {
        // Kills the `!=` -> `==` mutant in sessions_in_dir (line 660):
        // the legacy-directory filter must KEEP a file whose recorded cwd
        // encodes to the wanted cwd and SKIP the others. Under the mutant
        // the filter inverts, so each project lists the OTHER project's
        // session. The existing migration test only asserts the list LENGTH
        // (1) for each cwd, which stays 1 under the mutant - only the
        // identity flips.
        let base = std::env::temp_dir().join(format!("zen_mut4_{}", Uuid::new_v4()));
        let home = base.join("home");
        let cwd_dot = base.join("a").join("b.c");
        let cwd_slash = base.join("a").join("b").join("c");
        for d in [&cwd_dot, &cwd_slash] {
            fs::create_dir_all(d).unwrap();
        }
        let legacy_dir = home
            .join(".zen")
            .join("projects")
            .join(legacy_encode(&cwd_dot));
        assert_eq!(
            legacy_dir,
            home.join(".zen")
                .join("projects")
                .join(legacy_encode(&cwd_slash))
        );
        fs::create_dir_all(&legacy_dir).unwrap();
        for (id, cwd) in [("old-dot", &cwd_dot), ("old-slash", &cwd_slash)] {
            let p = legacy_dir.join(format!("{id}.jsonl"));
            append_typed(
                &p,
                "user",
                id,
                cwd.to_str().unwrap(),
                "u",
                None,
                Some(serde_json::json!({"role":"user","content":"hi"})),
                None,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        // Each project lists exactly its own legacy session - identity,
        // not just count.
        let dot_list = list_sessions_for_cwd(&home, &cwd_dot);
        assert_eq!(dot_list.len(), 1);
        assert_eq!(dot_list[0].0, "old-dot");
        let slash_list = list_sessions_for_cwd(&home, &cwd_slash);
        assert_eq!(slash_list.len(), 1);
        assert_eq!(slash_list[0].0, "old-slash");
        let _ = fs::remove_dir_all(&base);
    }
}
