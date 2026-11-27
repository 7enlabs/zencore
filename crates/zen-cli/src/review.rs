use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use zen_core::{
    EventSink, HookRunner, NoHooks, NullSink, PermissionChecker, PermissionDecision,
    PermissionMode, ToolContext, ToolRegistry,
};

use crate::cli::{Cli, OutputFormat, ReviewArgs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Nit,
}

impl Severity {
    pub fn rank(self) -> u8 {
        match self {
            Severity::Critical => 4,
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
            Severity::Nit => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Nit => "nit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" | "p0" => Severity::Critical,
            "high" | "p1" => Severity::High,
            "medium" | "med" | "p2" => Severity::Medium,
            "low" | "p3" => Severity::Low,
            "nit" | "p4" | "info" => Severity::Nit,
            _ => return None,
        })
    }

    pub fn ordered() -> [Severity; 5] {
        [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Nit,
        ]
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,

    #[serde(default = "default_category")]
    pub category: String,
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub suggestion: String,
    /// True when the fix needs a human decision (behavior, API shape,
    /// design, or a security tradeoff); false for a mechanical fix. The
    /// rendered report splits the two groups (S9 pattern 5).
    #[serde(default)]
    pub human_judgment: bool,
    /// B9 evidence label for the claim in this finding, as emitted by
    /// the reviewer model: "confirmed", "inferred" or "unverified"
    /// (decision 5526). Defaults to "unverified" when the model omits
    /// it, because an unlabelled claim must never read as confirmed.
    #[serde(default = "default_label")]
    pub label: String,
}

fn default_label() -> String {
    "unverified".to_string()
}

impl Finding {
    /// Short B9 tag rendered at the head of the finding line.
    pub fn label_tag(&self) -> &'static str {
        match self.label.trim().to_ascii_lowercase().as_str() {
            "confirmed" => "[CONFIRMED]",
            "inferred" => "[INFERRED]",
            _ => "[UNVERIFIED]",
        }
    }
}

fn default_category() -> String {
    "bugs".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub scope: String,
    pub model: String,
    pub findings: Vec<Finding>,
    pub summary: ReviewSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub total: usize,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub nit: usize,

    pub max_severity: Option<Severity>,
}

impl ReviewReport {
    pub fn new(scope: String, model: String, findings: Vec<Finding>) -> Self {
        let mut summary = ReviewSummary {
            total: findings.len(),
            critical: 0,
            high: 0,
            medium: 0,
            low: 0,
            nit: 0,
            max_severity: None,
        };
        for f in &findings {
            match f.severity {
                Severity::Critical => summary.critical += 1,
                Severity::High => summary.high += 1,
                Severity::Medium => summary.medium += 1,
                Severity::Low => summary.low += 1,
                Severity::Nit => summary.nit += 1,
            }
            if summary
                .max_severity
                .is_none_or(|m| f.severity.rank() > m.rank())
            {
                summary.max_severity = Some(f.severity);
            }
        }
        Self {
            scope,
            model,
            findings,
            summary,
        }
    }

    pub fn at_or_above(&self, threshold: Severity) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity.rank() >= threshold.rank())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum ReviewScope {
    Uncommitted,
    Base(String),
    Commit(String),
}

impl ReviewScope {
    pub fn from_args(args: &ReviewArgs) -> Result<Self> {
        if args.base.is_some() && args.commit.is_some() {
            anyhow::bail!("--base and --commit are mutually exclusive");
        }
        if let Some(b) = &args.base {
            if b.trim().is_empty() {
                anyhow::bail!("--base requires a non-empty ref");
            }
            Ok(ReviewScope::Base(b.trim().to_string()))
        } else if let Some(c) = &args.commit {
            if c.trim().is_empty() {
                anyhow::bail!("--commit requires a non-empty sha");
            }
            Ok(ReviewScope::Commit(c.trim().to_string()))
        } else {
            Ok(ReviewScope::Uncommitted)
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ReviewScope::Uncommitted => "uncommitted changes".to_string(),
            ReviewScope::Base(b) => format!("diff against base '{b}'"),
            ReviewScope::Commit(c) => format!("commit '{c}'"),
        }
    }
}

pub struct CollectedDiff {
    pub scope_description: String,
    pub files: Vec<String>,
    pub diff_text: String,
    pub truncated: bool,
}

const REVIEW_GIT_TIMEOUT: Duration = Duration::from_secs(15);

const UNTRACKED_FILE_CAP: usize = 20_000;

/// Join a reader thread only until `deadline`.
///
/// Issue #545: after git exits, `read_to_end` on a reader thread only
/// returns at EOF, and EOF only arrives once every process holding the
/// pipe's write end has closed it. A grandchild that outlives git (for
/// example a background process started by a git shell alias) therefore
/// held the old unconditional `join()` past the 15s deadline. Dropping
/// the parent's read end cannot force EOF while the grandchild still
/// holds the write end, so the only bound available here is to stop
/// joining at the deadline and let the reader thread finish on its own.
/// Returns `None` when the thread is still running at the deadline; the
/// handle is dropped, detaching the thread.
fn join_reader_until(
    handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    deadline: Instant,
) -> Option<Vec<u8>> {
    let Some(handle) = handle else {
        return Some(Vec::new());
    };
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().ok()
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git (is git installed?)")?;
    // Issue #456: drain stdout/stderr on reader threads while the
    // parent polls try_wait. A child whose output exceeds the pipe
    // buffer (~64 KiB) blocks on write until someone reads; polling
    // try_wait without draining deadlocked every large diff into the
    // 15s timeout.
    let stdout_thread = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut buf);
            buf
        })
    });
    let stderr_thread = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut buf);
            buf
        })
    });
    let deadline = Instant::now() + REVIEW_GIT_TIMEOUT;
    loop {
        match child.try_wait().context("failed to poll git")? {
            Some(status) => {
                // Issue #545: bound the whole call by the deadline,
                // including waiting for reader-thread EOF. A reader
                // still blocked at the deadline means a child is
                // holding the pipe past the deadline; report a timeout
                // instead of joining forever.
                let out = match join_reader_until(stdout_thread, deadline) {
                    Some(out) => out,
                    None => anyhow::bail!(
                        "git {} timed out after {:?} waiting for output",
                        args.join(" "),
                        REVIEW_GIT_TIMEOUT
                    ),
                };
                let err = match join_reader_until(stderr_thread, deadline) {
                    Some(err) => err,
                    None => anyhow::bail!(
                        "git {} timed out after {:?} waiting for output",
                        args.join(" "),
                        REVIEW_GIT_TIMEOUT
                    ),
                };
                if !status.success() {
                    let err = String::from_utf8_lossy(&err);
                    anyhow::bail!("git {} failed: {}", args.join(" "), err.trim());
                }
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "git {} timed out after {:?}",
                    args.join(" "),
                    REVIEW_GIT_TIMEOUT
                );
            }
        }
    }
}

/// Unquote a git C-style quoted path (`core.quotePath`, on by default):
/// `"caf\303\251.rs"` is `café.rs`. Returns `None` when the token is not
/// a well-formed quoted string.
fn unquote_git_path(token: &str) -> Option<String> {
    let inner = token.strip_prefix('"')?.strip_suffix('"')?;
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let esc = *bytes.get(i)?;
        i += 1;
        match esc {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'0'..=b'7' => {
                let mut value: u16 = (esc - b'0') as u16;
                let mut count = 1;
                while count < 3 {
                    let Some(&digit) = bytes.get(i) else { break };
                    if !(b'0'..=b'7').contains(&digit) {
                        break;
                    }
                    value = value * 8 + (digit - b'0') as u16;
                    i += 1;
                    count += 1;
                }
                if value > u8::MAX as u16 {
                    return None;
                }
                out.push(value as u8);
            }
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Strip the `a/` or `b/` prefix from one side of a `diff --git` header,
/// unquoting first when the side is C-style quoted. Returns `None` when
/// the side is malformed.
fn diff_git_side_path(side: &str, prefix: &str) -> Option<String> {
    if side.starts_with('"') {
        unquote_git_path(side)?
            .strip_prefix(prefix)
            .map(str::to_string)
    } else {
        side.strip_prefix(prefix).map(str::to_string)
    }
}

/// Split the remainder of a `diff --git` header line into its a-side and
/// b-side paths.
///
/// Issue #464: splitting the header on spaces silently dropped filenames
/// with spaces and kept git-quoted non-ASCII filenames verbatim (quotes
/// and octal escapes included). The separator between the two sides is
/// ` b/` for plain paths and ` "b/` when the b-side is quoted. For the
/// common non-rename case both sides are the same path, so the split
/// where the two sides agree is exact even when a side contains spaces
/// or a literal ` b/`; otherwise the first plausible separator is the
/// rename split. (A rename whose a-side itself contains ` b/` stays
/// ambiguous in the header alone; `changed_files_from_diff` resolves
/// it from the `rename to` line instead.)
fn split_diff_git_header(rest: &str) -> Option<(String, String)> {
    let mut separators: Vec<usize> = rest.match_indices(" b/").map(|(i, _)| i).collect();
    separators.extend(rest.match_indices(" \"b/").map(|(i, _)| i));
    separators.sort_unstable();
    separators.dedup();
    let mut fallback = None;
    for i in separators {
        let a = diff_git_side_path(&rest[..i], "a/");
        let b = diff_git_side_path(&rest[i + 1..], "b/");
        let Some((a, b)) = a.zip(b) else { continue };
        if a == b {
            return Some((a, b));
        }
        if fallback.is_none() {
            fallback = Some((a, b));
        }
    }
    fallback
}

/// Read the destination of a rename from the `rename to` line git emits
/// under a `diff --git` header. The header alone is ambiguous when the
/// old path itself contains ` b/` (for example renaming
/// `old b/part.rs` to `name.rs`), but the `rename to` line carries the
/// destination path verbatim, so it is the unambiguous source. The
/// value is unquoted when git C-style quoted it. Returns `None` when
/// the section has no rename.
fn rename_to_path(lines: &[&str], header: usize) -> Option<String> {
    let mut i = header + 1;
    while i < lines.len() && !lines[i].starts_with("diff --git ") {
        if let Some(path) = lines[i].strip_prefix("rename to ") {
            return Some(unquote_git_path(path).unwrap_or_else(|| path.to_string()));
        }
        i += 1;
    }
    None
}

fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let file =
            rename_to_path(&lines, i).or_else(|| split_diff_git_header(rest).map(|(_, b)| b));
        if let Some(file) = file
            && !files.contains(&file)
        {
            files.push(file);
        }
    }
    files
}

pub fn collect_diff(cwd: &Path, scope: &ReviewScope, max_bytes: usize) -> Result<CollectedDiff> {
    run_git(cwd, &["rev-parse", "--git-dir"]).context("not a git repository")?;

    let mut files: Vec<String> = Vec::new();
    let mut text = String::new();

    match scope {
        ReviewScope::Uncommitted => {
            let diff = run_git(cwd, &["diff", "HEAD", "--"])?;
            files.extend(changed_files_from_diff(&diff));
            text.push_str(&diff);

            let status = run_git(cwd, &["status", "--porcelain", "--"])?;
            for line in status.lines() {
                if let Some(path) = line.strip_prefix("?? ") {
                    let p = path.trim();
                    if p.is_empty() {
                        continue;
                    }
                    if !files.contains(&p.to_string()) {
                        files.push(p.to_string());
                    }
                    let full = cwd.join(p);
                    if full.is_file() {
                        text.push_str(&format!("\n--- new file: {p} ---\n"));
                        match std::fs::read(&full) {
                            Ok(bytes) => {
                                let mut s = String::from_utf8_lossy(&bytes).into_owned();
                                // Untracked files are inlined only up to
                                // UNTRACKED_FILE_CAP, with an explicit
                                // truncation marker: a review must never
                                // silently see a partial file as complete.
                                if s.len() > UNTRACKED_FILE_CAP {
                                    s.truncate(UNTRACKED_FILE_CAP);
                                    s.push_str("\n[... truncated: file larger than 20k ...]\n");
                                }
                                text.push_str(&s);
                                if !s.ends_with('\n') {
                                    text.push('\n');
                                }
                            }
                            Err(e) => {
                                text.push_str(&format!("[could not read: {e}]\n"));
                            }
                        }
                    }
                }
            }
        }
        ReviewScope::Base(base) => {
            let diff = run_git(cwd, &["diff", base, "--"])?;
            files.extend(changed_files_from_diff(&diff));
            text = diff;
        }
        ReviewScope::Commit(sha) => {
            let show = run_git(cwd, &["show", "--format=", sha, "--"])?;
            files.extend(changed_files_from_diff(&show));
            text = show;
        }
    }

    let truncated = text.len() > max_bytes;
    if truncated {
        text.truncate(max_bytes);
        text.push_str("\n[... diff truncated: exceeded --max-diff-bytes ...]\n");
    }

    Ok(CollectedDiff {
        scope_description: scope.describe(),
        files,
        diff_text: text,
        truncated,
    })
}

/// Extra output contract for the review prompt (S9 pattern 5): every
/// finding must say whether its fix needs a human decision or is
/// mechanical, so the rendered report can split the two groups instead of
/// mixing them together.
const HUMAN_JUDGMENT_GUIDANCE: &str = "\n\nEach finding MUST also include \"human_judgment\": true when fixing it needs a human decision (behavior, API shape, design, or a security tradeoff), or false when the fix is mechanical (formatting, naming, an obvious missing check, a one-right-answer correction).";

const EVIDENCE_LABEL_GUIDANCE: &str = "\n\nEach finding MUST also include \"label\": \"confirmed\" when the evidence cited inside the finding proves the claim, \"inferred\" when the claim follows from the code shown but is not directly proven, or \"unverified\" when it rests on anything outside the diff. Never label a claim confirmed without evidence inside the finding.";

pub fn review_system_prompt() -> String {
    let mut prompt =
        "You are a senior code reviewer. Review the provided diff carefully and report \
     concrete, actionable findings. You have read-only tools (Read, Glob, Grep) to \
     inspect surrounding code when a diff hunk lacks context. You CANNOT modify \
     anything: no write tools exist in this session.\n\n\
     OUTPUT CONTRACT (must follow exactly): output ONLY a JSON array of findings \
     inside a ```json fenced block, no prose outside it. Each finding:\n\
     {\"severity\": \"critical|high|medium|low|nit\", \
      \"category\": \"bugs|security|perf|style|tests-missing\", \
      \"file\": \"path/relative\", \"line\": 123 or null, \
      \"title\": \"short rule name\", \
      \"detail\": \"what is wrong and why it matters\", \
      \"suggestion\": \"concrete fix\", \"label\": \"confirmed|inferred|unverified\"}\n\n\
     Severity guide:\n\
     - critical: data loss, exploitable security issue, crash on a normal path\n\
     - high: likely bug, broken error handling, race condition, wrong API use\n\
     - medium: real code smell, missing tests for new logic, clear perf problem\n\
     - low: minor clarity or maintainability note\n\
     - nit: formatting, naming, trivial style\n\n\
     Be strict but fair: do not invent problems. If the diff is clean, output []."
            .to_string();
    prompt.push_str(HUMAN_JUDGMENT_GUIDANCE);
    prompt.push_str(EVIDENCE_LABEL_GUIDANCE);
    prompt
}

pub fn build_review_prompt(diff: &CollectedDiff) -> String {
    let mut p = String::new();
    p.push_str("Review the following changes (");
    p.push_str(&diff.scope_description);
    p.push_str(").\n\nChanged files:\n");
    if diff.files.is_empty() {
        p.push_str("(none detected)\n");
    } else {
        for f in &diff.files {
            p.push_str("- ");
            p.push_str(f);
            p.push('\n');
        }
    }
    if diff.truncated {
        p.push_str("\nNOTE: the diff was truncated for size; focus on what is shown.\n");
    }
    p.push_str("\n```diff\n");
    p.push_str(&diff.diff_text);
    p.push_str(
        "\n```\n\nNow output your findings as the JSON array described in your instructions.",
    );
    p
}

pub fn extract_findings(text: &str) -> Result<Vec<Finding>> {
    if let Some(start) = text.find("```json") {
        let inner_start = start + "```json".len();
        if let Some(end) = text[inner_start..].find("```") {
            let inner = text[inner_start..inner_start + end].trim();
            if let Ok(v) = serde_json::from_str::<Vec<Finding>>(inner) {
                return Ok(v);
            }
        }
    }

    let mut search = text;
    while let Some(start) = search.find("```") {
        let inner_start = start + 3;

        let content_start = search[inner_start..]
            .find('\n')
            .map(|i| inner_start + i + 1)
            .unwrap_or(inner_start);
        if let Some(end) = search[content_start..].find("```") {
            let inner = search[content_start..content_start + end].trim();
            if inner.starts_with('[')
                && let Ok(v) = serde_json::from_str::<Vec<Finding>>(inner)
            {
                return Ok(v);
            }
            search = &search[content_start + end..];
        } else {
            break;
        }
    }

    if let (Some(s), Some(e)) = (text.find('['), text.rfind(']'))
        && s < e
    {
        let inner = &text[s..=e];
        if let Ok(v) = serde_json::from_str::<Vec<Finding>>(inner) {
            return Ok(v);
        }
    }
    anyhow::bail!(
        "model output did not contain a parseable JSON findings array (output was {} chars)",
        text.len()
    )
}

pub const REVIEW_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Build a review-only tool registry, failing closed: if a review tool is
/// not marked read-only, or an expected read-only tool is missing, the
/// review refuses to run instead of reviewing with write-capable tools.
pub fn read_only_registry() -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for t in zen_tools::builtin_tools() {
        if REVIEW_TOOLS.contains(&t.name()) {
            if !t.is_read_only() {
                anyhow::bail!(
                    "tool '{}' is not marked read-only; refusing to run review",
                    t.name()
                );
            }
            registry.register(t);
        }
    }
    for name in REVIEW_TOOLS {
        if registry.get(name).is_none() {
            anyhow::bail!("read-only tool '{name}' missing from registry; refusing to run review");
        }
    }
    Ok(registry)
}

struct ReviewPermissions;

impl PermissionChecker for ReviewPermissions {
    fn check(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        read_only: bool,
        _ctx: &ToolContext,
    ) -> PermissionDecision {
        if read_only {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny(format!(
                "zen review is read-only: tool '{tool_name}' is not permitted"
            ))
        }
    }
}

/// Make every finding path absolute against the reviewed tree (S9 pattern
/// 4): reports must carry the full path as evidence for each finding, in
/// text, JSON and stream-json alike, not the relative path the model
/// happened to emit.
fn absolutize_paths(findings: &mut [Finding], root: &Path) {
    for f in findings {
        let p = Path::new(&f.file);
        if !p.is_absolute() {
            f.file = root.join(p).display().to_string();
        }
        // Finding paths are report output (text, JSON and stream-json) and
        // are compared as strings, so normalize Windows separators to '/':
        // Path::display would otherwise emit a backslash there on Windows.
        f.file = f.file.replace('\\', "/");
    }
}

fn render_text(report: &ReviewReport, threshold: Severity) -> String {
    let mut out = String::new();
    out.push_str(&format!("zen review - {}\n", report.scope));
    let human = report.findings.iter().filter(|f| f.human_judgment).count();
    let mechanical = report.summary.total - human;
    out.push_str(&format!(
        "model: {} - findings: {} ({} need human judgment, {} mechanical)\n\n",
        report.model, report.summary.total, human, mechanical
    ));
    if report.findings.is_empty() {
        out.push_str("No findings. Diff looks clean.\n");
        return out;
    }
    // S9 patterns 4 and 5: one numbered list split into the findings a
    // human must judge and the mechanical fixes, severity first inside
    // each group, every finding carrying its full path as evidence.
    let mut n = 0usize;
    for (title, want_human) in [("Needs human judgment", true), ("Mechanical fixes", false)] {
        let mut group: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.human_judgment == want_human)
            .collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by_key(|f| std::cmp::Reverse(f.severity.rank()));
        out.push_str(&format!("== {} ({}) ==\n", title, group.len()));
        for f in group {
            n += 1;
            let loc = match f.line {
                Some(l) => format!("{}:{}", f.file, l),
                None => f.file.clone(),
            };
            let marker = if f.severity.rank() >= threshold.rank() {
                " [fails --fail-on]"
            } else {
                ""
            };
            out.push_str(&format!(
                "{n}. {} [{} / {}] {}{}\n",
                f.label_tag(),
                f.severity,
                f.category,
                f.title,
                marker
            ));
            out.push_str(&format!("    evidence: {loc}\n"));
            if !f.detail.is_empty() {
                out.push_str(&format!("    {}\n", f.detail));
            }
            if !f.suggestion.is_empty() {
                out.push_str(&format!("    fix: {}\n", f.suggestion));
            }
        }
        out.push('\n');
    }
    out
}

fn render_json_value(report: &ReviewReport) -> serde_json::Value {
    serde_json::to_value(report).unwrap_or(serde_json::Value::Null)
}

/// Build the stream-json records for a review run. Every record carries
/// the contract version field `v`, matching the agent stream in
/// output.rs (STREAM_JSON_VERSION); consumers key on it to pick a parser.
fn stream_json_records(
    report: &ReviewReport,
    threshold: Severity,
    exit_code: i32,
) -> Vec<serde_json::Value> {
    let failed = !report.at_or_above(threshold).is_empty();
    let mut records = vec![serde_json::json!({
        "v": crate::output::STREAM_JSON_VERSION,
        "type": "review",
        "event": "start",
        "scope": report.scope,
        "model": report.model,
    })];
    for f in &report.findings {
        records.push(serde_json::json!({
            "v": crate::output::STREAM_JSON_VERSION,
            "type": "review",
            "event": "finding",
            "finding": f,
        }));
    }
    records.push(serde_json::json!({
        "v": crate::output::STREAM_JSON_VERSION,
        "type": "review",
        "event": "complete",
        "summary": report.summary,
        "failed": failed,
        "exit_code": exit_code,
    }));
    records
}

/// Exit code for a `collect_diff` failure (migration queue Q3,
/// docs/exit-codes.md): not a git repository is a validation error, a
/// missing ref is not found, anything else stays general.
fn diff_error_exit_code(err: &anyhow::Error) -> i32 {
    let text = format!("{err:#}").to_lowercase();
    if text.contains("not a git repository") {
        crate::exit_code::VALIDATION
    } else if text.contains("unknown revision")
        || text.contains("unknown ref")
        || text.contains("not found")
    {
        crate::exit_code::NOT_FOUND
    } else {
        crate::exit_code::GENERAL
    }
}

/// Exit code for a failed review run against the provider (queue Q3):
/// rate limit only when the provider says so, otherwise general.
fn provider_error_exit_code(text: &str) -> i32 {
    let text = text.to_lowercase();
    if text.contains("rate limit") || text.contains("rate_limit") || text.contains("429") {
        crate::exit_code::RATE_LIMIT
    } else {
        crate::exit_code::GENERAL
    }
}

fn print_stream_json(report: &ReviewReport, threshold: Severity, exit_code: i32) {
    for record in stream_json_records(report, threshold, exit_code) {
        println!("{record}");
    }
}

pub async fn run_review(
    cli: &Cli,
    cwd: &Path,
    home: Option<&Path>,
    args: &ReviewArgs,
) -> Result<i32> {
    let threshold = Severity::parse(&args.fail_on).unwrap_or_else(|| {
        eprintln!(
            "warning: invalid --fail-on '{}', using 'high'",
            args.fail_on
        );
        Severity::High
    });

    let scope = match ReviewScope::from_args(args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(crate::exit_code::VALIDATION);
        }
    };

    let cfg = crate::settings::resolve_config_with_home(cli, cwd, home)?;
    // The global --json/--raw flags select the JSON report form here
    // too; --quiet suppresses the report on stdout (exit code keeps
    // the verdict).
    let output_format = if cli.json_output || cli.raw {
        OutputFormat::Json
    } else {
        args.output_format
    };

    let diff = match collect_diff(cwd, &scope, args.max_diff_bytes) {
        Ok(d) => d,
        Err(e) => {
            let code = diff_error_exit_code(&e);
            eprintln!("error: {e:#}");
            return Ok(code);
        }
    };
    if diff.diff_text.trim().is_empty() {
        if !cli.quiet {
            match output_format {
                OutputFormat::Text => {
                    println!(
                        "zen review - {}: no changes to review.",
                        diff.scope_description
                    );
                }
                OutputFormat::Json => {
                    let report =
                        ReviewReport::new(diff.scope_description, cfg.model.clone(), vec![]);
                    println!(
                        "{}",
                        if cli.raw {
                            serde_json::to_string(&render_json_value(&report))?
                        } else {
                            serde_json::to_string_pretty(&render_json_value(&report))?
                        }
                    );
                }
                OutputFormat::StreamJson => {
                    let report =
                        ReviewReport::new(diff.scope_description, cfg.model.clone(), vec![]);
                    print_stream_json(&report, threshold, 0);
                }
            }
        }
        return Ok(0);
    }

    if cfg.api_key().is_none() {
        eprintln!(
            "error: no API key configured (set it via `zen auth login` or env); cannot run review"
        );
        return Ok(crate::exit_code::AUTH);
    }

    let registry = match read_only_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return Ok(crate::exit_code::GENERAL);
        }
    };
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    let llm = cfg.llm_client(sink.clone());
    let session_id = format!("review-{}", uuid::Uuid::new_v4().hyphenated());
    let ctx = ToolContext::new(cwd.to_path_buf(), session_id.clone());
    let config = zen_agent::AgentConfig {
        model: cfg.model.clone(),
        system_prompt: review_system_prompt(),
        max_turns: 8,
        permission_mode: PermissionMode::Default,
        auto_compact_threshold: None,
        ..Default::default()
    };
    let mut agent = zen_agent::Agent::new(
        llm,
        registry,
        Arc::new(ReviewPermissions),
        None,
        Arc::new(NoHooks) as Arc<dyn HookRunner>,
        sink,
        ctx,
        config,
    );

    let result = agent.run_turn(&build_review_prompt(&diff)).await;
    if result.is_error {
        let code = provider_error_exit_code(&format!("{} {}", result.subtype, result.result));
        eprintln!(
            "error: review run failed ({}): {}",
            result.subtype, result.result
        );
        return Ok(code);
    }

    let mut findings = match extract_findings(&result.result) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e:#}");
            eprintln!("--- model output (unparseable) ---");
            eprintln!("{}", result.result);
            return Ok(crate::exit_code::VALIDATION);
        }
    };
    absolutize_paths(&mut findings, cwd);
    let report = ReviewReport::new(diff.scope_description, cfg.model.clone(), findings);
    let failed = !report.at_or_above(threshold).is_empty();
    let exit_code = if failed { 1 } else { 0 };

    if !cli.quiet {
        match output_format {
            OutputFormat::Text => {
                print!("{}", render_text(&report, threshold));
                if failed {
                    println!(
                        "{} finding(s) at or above --fail-on={} → exit 1",
                        report.at_or_above(threshold).len(),
                        threshold
                    );
                }
            }
            OutputFormat::Json => {
                println!(
                    "{}",
                    if cli.raw {
                        serde_json::to_string(&render_json_value(&report))?
                    } else {
                        serde_json::to_string_pretty(&render_json_value(&report))?
                    }
                );
            }
            OutputFormat::StreamJson => {
                print_stream_json(&report, threshold, exit_code);
            }
        }
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    fn review_args(extra: &[&str]) -> ReviewArgs {
        let mut argv: Vec<String> = vec!["zen".to_string(), "review".to_string()];
        argv.extend(extra.iter().map(|s| s.to_string()));
        let cli = Cli::parse_from(argv);
        match cli.subcommand {
            Some(crate::cli::Commands::Review(a)) => a,
            other => panic!("expected review subcommand, got {other:?}"),
        }
    }

    fn git_env(cmd: &mut Command) {
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
    }

    fn git_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zen_review_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut init = Command::new("git");
        git_env(&mut init);
        let out = init
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .output()
            .expect("git failed");
        assert!(out.status.success());
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let mut add = Command::new("git");
        git_env(&mut add);
        assert!(
            add.args(["add", "."])
                .current_dir(&dir)
                .output()
                .unwrap()
                .status
                .success()
        );
        let mut commit = Command::new("git");
        git_env(&mut commit);
        assert!(
            commit
                .args(["commit", "-qm", "init"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .status
                .success()
        );
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        git_env(&mut cmd);
        let out = cmd
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git failed");
        assert!(out.status.success(), "git {args:?} failed");
    }

    #[test]
    fn severity_parse_all_forms() {
        assert_eq!(Severity::parse("critical"), Some(Severity::Critical));
        assert_eq!(Severity::parse("HIGH"), Some(Severity::High));
        assert_eq!(Severity::parse("p2"), Some(Severity::Medium));
        assert_eq!(Severity::parse("low"), Some(Severity::Low));
        assert_eq!(Severity::parse("nit"), Some(Severity::Nit));
        assert_eq!(Severity::parse("bogus"), None);
    }

    #[test]
    fn severity_rank_ordering_for_thresholds() {
        assert!(Severity::Critical.rank() > Severity::High.rank());
        assert!(Severity::High.rank() > Severity::Medium.rank());
        assert!(Severity::Medium.rank() > Severity::Low.rank());
        assert!(Severity::Low.rank() > Severity::Nit.rank());
    }

    #[test]
    fn at_or_above_filters_correctly() {
        let mk = |s| Finding {
            severity: s,
            category: "bugs".into(),
            file: "f.rs".into(),
            line: Some(1),
            title: "t".into(),
            detail: "".into(),
            suggestion: "".into(),
            human_judgment: false,
            label: "unverified".to_string(),
        };
        let report = ReviewReport::new(
            "s".into(),
            "m".into(),
            vec![
                mk(Severity::Critical),
                mk(Severity::Medium),
                mk(Severity::Nit),
            ],
        );
        assert_eq!(report.at_or_above(Severity::High).len(), 1);
        assert_eq!(report.at_or_above(Severity::Medium).len(), 2);
        assert_eq!(report.at_or_above(Severity::Nit).len(), 3);
        assert_eq!(report.summary.total, 3);
        assert_eq!(report.summary.critical, 1);
        assert_eq!(report.summary.max_severity, Some(Severity::Critical));
    }

    #[test]
    fn parse_review_defaults() {
        let a = review_args(&[]);
        assert!(a.base.is_none() && a.commit.is_none() && !a.uncommitted);
        assert_eq!(a.fail_on, "high");
        assert_eq!(a.output_format, OutputFormat::Text);
    }

    #[test]
    fn parse_review_scope_flags() {
        let a = review_args(&["--base", "main"]);
        assert_eq!(a.base.as_deref(), Some("main"));
        let a = review_args(&["--commit", "abc123"]);
        assert_eq!(a.commit.as_deref(), Some("abc123"));
        let a = review_args(&[
            "--uncommitted",
            "--fail-on",
            "medium",
            "--output-format",
            "json",
        ]);
        assert!(a.uncommitted);
        assert_eq!(a.fail_on, "medium");
        assert_eq!(a.output_format, OutputFormat::Json);
    }

    #[test]
    fn stream_json_records_carry_contract_version() {
        let report = ReviewReport::new("uncommitted".into(), "test-model".into(), vec![]);
        let records = stream_json_records(&report, Severity::High, 0);
        assert_eq!(records.len(), 2);
        for record in &records {
            assert_eq!(
                record["v"],
                serde_json::json!(crate::output::STREAM_JSON_VERSION),
                "review stream-json record missing v: {record}"
            );
            assert_eq!(record["type"], "review");
        }
        assert_eq!(records[0]["event"], "start");
        assert_eq!(records[1]["event"], "complete");
    }

    #[test]
    fn diff_error_exit_code_classifies_by_cause() {
        let not_repo = anyhow::anyhow!("fatal: not a git repository (or any parent)");
        assert_eq!(
            diff_error_exit_code(&not_repo),
            crate::exit_code::VALIDATION
        );
        let bad_ref = anyhow::anyhow!("fatal: ambiguous argument 'xyz': unknown revision");
        assert_eq!(diff_error_exit_code(&bad_ref), crate::exit_code::NOT_FOUND);
        let other = anyhow::anyhow!("io error");
        assert_eq!(diff_error_exit_code(&other), crate::exit_code::GENERAL);
    }

    #[test]
    fn provider_error_exit_code_only_rate_limits_on_provider_word() {
        assert_eq!(
            provider_error_exit_code("HTTP 429: rate limit exceeded"),
            crate::exit_code::RATE_LIMIT
        );
        assert_eq!(
            provider_error_exit_code("connection reset by peer"),
            crate::exit_code::GENERAL
        );
    }

    #[test]
    fn parse_review_stream_json_format() {
        let a = review_args(&["--output-format", "stream-json"]);
        assert_eq!(a.output_format, OutputFormat::StreamJson);
    }

    #[test]
    fn scope_rejects_base_and_commit_together() {
        let a = review_args(&["--base", "main", "--commit", "abc"]);
        assert!(ReviewScope::from_args(&a).is_err());
    }

    #[test]
    fn scope_defaults_to_uncommitted() {
        let a = review_args(&[]);
        assert!(matches!(
            ReviewScope::from_args(&a).unwrap(),
            ReviewScope::Uncommitted
        ));
    }

    #[test]
    fn collect_uncommitted_diff() {
        let dir = git_repo();
        std::fs::write(dir.join("a.txt"), "hello\nworld\n").unwrap();
        let diff = collect_diff(&dir, &ReviewScope::Uncommitted, 1_000_000).unwrap();
        assert!(diff.diff_text.contains("world"));
        assert!(diff.files.iter().any(|f| f == "a.txt"));
        assert!(!diff.truncated);
    }

    #[test]
    fn collect_uncommitted_includes_untracked() {
        let dir = git_repo();
        std::fs::write(dir.join("new.rs"), "fn main() {}\n").unwrap();
        let diff = collect_diff(&dir, &ReviewScope::Uncommitted, 1_000_000).unwrap();
        assert!(diff.diff_text.contains("new file: new.rs"));
        assert!(diff.diff_text.contains("fn main()"));
        assert!(diff.files.iter().any(|f| f == "new.rs"));
    }

    #[test]
    fn collect_base_diff() {
        let dir = git_repo();
        git_in(&dir, &["checkout", "-qb", "feature"]);
        std::fs::write(dir.join("b.txt"), "feature work\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "feat"]);
        let diff = collect_diff(&dir, &ReviewScope::Base("main".into()), 1_000_000).unwrap();
        assert!(diff.diff_text.contains("feature work"));
    }

    #[test]
    fn collect_commit_diff() {
        let dir = git_repo();
        std::fs::write(dir.join("a.txt"), "hello\nchanged\n").unwrap();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-qm", "change"]);
        let sha = {
            let mut cmd = Command::new("git");
            git_env(&mut cmd);
            String::from_utf8(
                cmd.args(["rev-parse", "HEAD"])
                    .current_dir(&dir)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
        };
        let diff = collect_diff(
            &dir,
            &ReviewScope::Commit(sha.trim().to_string()),
            1_000_000,
        )
        .unwrap();
        assert!(diff.diff_text.contains("changed"));
    }

    #[test]
    fn collect_diff_truncates_large_diffs() {
        let dir = git_repo();
        let big = "x".repeat(5000);
        std::fs::write(dir.join("a.txt"), format!("hello\n{big}\n")).unwrap();
        let diff = collect_diff(&dir, &ReviewScope::Uncommitted, 100).unwrap();
        assert!(diff.truncated);
        assert!(diff.diff_text.contains("truncated"));
    }

    #[test]
    fn issue_456_large_diff_does_not_deadlock() {
        // Issue #456: `run_git` polled `try_wait` while nobody
        // drained the pipes, so a diff over the ~64 KiB pipe buffer
        // blocked git on write and `zen review` died with a spurious
        // 15s timeout. A ~210 KiB diff must return, not time out.
        let dir = git_repo();
        let big = "line of changed text\n".repeat(10_000);
        std::fs::write(dir.join("a.txt"), format!("hello\n{big}")).unwrap();
        let diff = collect_diff(&dir, &ReviewScope::Uncommitted, 10_000_000)
            .expect("large diff must return instead of timing out");
        assert!(diff.diff_text.len() > 64 * 1024);
        assert!(diff.diff_text.contains("line of changed text"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn issue_545_reader_eof_wait_is_bounded_by_deadline() {
        // Issue #545: git itself exits, but a grandchild started by a
        // git shell alias keeps the stdout pipe open for 18s. The old
        // unconditional join of the reader threads waited for EOF and
        // returned after ~18s, past the 15s deadline. The call must
        // return within the deadline instead.
        let dir = git_repo();
        git_in(
            &dir,
            &[
                "config",
                "alias.hold-pipe",
                "!sh -c 'echo held; sleep 18 &'",
            ],
        );
        let start = Instant::now();
        let result = run_git(&dir, &["hold-pipe"]);
        let elapsed = start.elapsed();
        let err = result.expect_err("expected a timeout error, not success");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got {err}"
        );
        // The timeout must come from respecting the deadline, not from
        // returning at once: the `>=` -> `<` mutant in join_reader_until
        // bails immediately and still passes the error check above, so
        // pin the lower bound too (Z Tester verdict on #546).
        assert!(
            elapsed >= Duration::from_secs(14),
            "call must wait until the deadline, took only {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(17),
            "call must return within the deadline, took {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn issue_545_join_reader_until_returns_value_when_thread_finishes_before_deadline() {
        // Mutants shard 5 on PR #546: replacing `>=` with `<` in the
        // deadline check makes join_reader_until return None at once
        // whenever the thread is still running, even though the
        // deadline is far away. A thread that finishes before the
        // deadline must have its value returned, not a timeout.
        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            vec![1, 2, 3]
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        assert_eq!(
            join_reader_until(Some(handle), deadline),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn collect_diff_outside_repo_errors() {
        let dir = std::env::temp_dir().join(format!("zen_review_norepo_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(collect_diff(&dir, &ReviewScope::Uncommitted, 1_000_000).is_err());
    }

    #[test]
    fn collect_diff_bad_base_errors() {
        let dir = git_repo();
        assert!(
            collect_diff(
                &dir,
                &ReviewScope::Base("no-such-branch-xyz".into()),
                1_000_000
            )
            .is_err()
        );
    }

    #[test]
    fn registry_contains_only_read_tools() {
        let r = read_only_registry().unwrap();
        let names = r.names();
        assert_eq!(names.len(), 3);
        for n in ["Read", "Glob", "Grep"] {
            assert!(names.contains(&n.to_string()), "missing {n}");
        }

        for n in [
            "Write",
            "Edit",
            "MultiEdit",
            "Bash",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
        ] {
            assert!(r.get(n).is_none(), "write tool {n} must not be registered");
        }

        for t in zen_tools::builtin_tools() {
            if REVIEW_TOOLS.contains(&t.name()) {
                assert!(t.is_read_only(), "{} must be read-only", t.name());
            }
        }
    }

    #[test]
    fn review_permissions_deny_non_readonly() {
        use zen_core::PermissionChecker;
        let p = ReviewPermissions;
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "s".to_string());
        let v = serde_json::json!({});
        assert!(matches!(
            p.check("Read", &v, true, &ctx),
            PermissionDecision::Allow
        ));
        assert!(matches!(
            p.check("Write", &v, false, &ctx),
            PermissionDecision::Deny(_)
        ));
        assert!(matches!(
            p.check("Bash", &v, false, &ctx),
            PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn extract_from_json_fence() {
        let text = "Here you go:\n```json\n[{\"severity\":\"high\",\"category\":\"bugs\",\"file\":\"a.rs\",\"line\":3,\"title\":\"oops\",\"detail\":\"d\",\"suggestion\":\"s\"}]\n```\n";
        let f = extract_findings(text).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].file, "a.rs");
        assert_eq!(f[0].line, Some(3));
    }

    #[test]
    fn extract_bare_array() {
        let text = r#"[{"severity":"nit","file":"b.rs","title":"t"}]"#;
        let f = extract_findings(text).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Nit);
        assert_eq!(f[0].line, None);
        assert_eq!(f[0].category, "bugs");
    }

    #[test]
    fn extract_empty_array_means_clean() {
        let f = extract_findings("```json\n[]\n```").unwrap();
        assert!(f.is_empty());
    }

    #[test]
    fn extract_garbage_errors() {
        assert!(extract_findings("looks fine to me, ship it").is_err());
        assert!(extract_findings("```json\nnot json\n```").is_err());
    }

    #[test]
    fn json_shape_has_summary_and_findings() {
        let f = Finding {
            severity: Severity::Medium,
            category: "perf".into(),
            file: "x.rs".into(),
            line: None,
            title: "slow".into(),
            detail: "d".into(),
            suggestion: "s".into(),
            human_judgment: false,
            label: "unverified".to_string(),
        };
        let report = ReviewReport::new("s".into(), "m".into(), vec![f]);
        let v = render_json_value(&report);
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["summary"]["medium"], 1);
        assert_eq!(v["findings"][0]["severity"], "medium");
        assert_eq!(v["findings"][0]["file"], "x.rs");
        assert!(v["findings"][0]["line"].is_null());
    }

    #[test]
    fn text_splits_judgment_groups_numbered_with_evidence() {
        let mk = |s: Severity, t: &str, h: bool| Finding {
            severity: s,
            category: "bugs".into(),
            file: "f.rs".into(),
            line: Some(1),
            title: t.into(),
            detail: "".into(),
            suggestion: "".into(),
            human_judgment: h,
            label: "unverified".to_string(),
        };
        let report = ReviewReport::new(
            "s".into(),
            "m".into(),
            vec![
                mk(Severity::Low, "mech-low", false),
                mk(Severity::Critical, "judge-crit", true),
                mk(Severity::High, "mech-high", false),
            ],
        );
        let text = render_text(&report, Severity::High);
        let hpos = text.find("Needs human judgment (1)").unwrap();
        let mpos = text.find("Mechanical fixes (2)").unwrap();
        assert!(hpos < mpos, "human judgment section first: {text}");
        assert!(
            text.contains("(1 need human judgment, 2 mechanical)"),
            "{text}"
        );
        assert!(
            text.contains("1. [UNVERIFIED] [critical / bugs] judge-crit [fails --fail-on]"),
            "{text}"
        );
        assert!(
            text.contains("2. [UNVERIFIED] [high / bugs] mech-high [fails --fail-on]"),
            "{text}"
        );
        assert!(
            text.contains("3. [UNVERIFIED] [low / bugs] mech-low"),
            "{text}"
        );
        assert_eq!(text.matches("evidence: f.rs:1").count(), 3, "{text}");
    }

    #[test]
    fn findings_default_and_parse_human_judgment() {
        let text = "\u{60}\u{60}\u{60}json\n[{\"severity\":\"high\",\"category\":\"bugs\",\"file\":\"a.rs\",\"title\":\"t\"},{\"severity\":\"low\",\"category\":\"style\",\"file\":\"b.rs\",\"title\":\"u\",\"human_judgment\":true}]\n\u{60}\u{60}\u{60}";
        let f = extract_findings(text).unwrap();
        assert!(!f[0].human_judgment);
        assert!(f[1].human_judgment);
    }

    #[test]
    fn absolutize_paths_makes_relative_paths_full() {
        let mut findings = vec![
            Finding {
                severity: Severity::Low,
                category: "bugs".into(),
                file: "src/a.rs".into(),
                line: None,
                title: "t".into(),
                detail: "".into(),
                suggestion: "".into(),
                human_judgment: false,
                label: "unverified".to_string(),
            },
            Finding {
                severity: Severity::Low,
                category: "bugs".into(),
                file: "/already/abs.rs".into(),
                line: None,
                title: "t".into(),
                detail: "".into(),
                suggestion: "".into(),
                human_judgment: true,
                label: "unverified".to_string(),
            },
        ];
        absolutize_paths(&mut findings, Path::new("/repo"));
        assert_eq!(findings[0].file, "/repo/src/a.rs");
        assert_eq!(findings[1].file, "/already/abs.rs");
    }

    #[test]
    fn system_prompt_asks_for_human_judgment_split() {
        assert!(review_system_prompt().contains("human_judgment"));
    }

    #[test]
    fn system_prompt_asks_for_evidence_label() {
        let p = review_system_prompt();
        assert!(p.contains("label"));
        assert!(p.contains("confirmed|inferred|unverified"));
        assert!(p.contains("Never label a claim confirmed"));
    }

    #[test]
    fn findings_parse_label_and_default_to_unverified() {
        let text = "\u{60}\u{60}\u{60}json\n[{\"severity\":\"high\",\"category\":\"bugs\",\"file\":\"a.rs\",\"title\":\"t\",\"label\":\"confirmed\"},{\"severity\":\"low\",\"category\":\"style\",\"file\":\"b.rs\",\"title\":\"u\"}]\n\u{60}\u{60}\u{60}";
        let f = extract_findings(text).unwrap();
        assert_eq!(f[0].label, "confirmed");
        assert_eq!(f[0].label_tag(), "[CONFIRMED]");
        // B9: a finding the model did not label defaults to unverified,
        // never to confirmed.
        assert_eq!(f[1].label, "unverified");
        assert_eq!(f[1].label_tag(), "[UNVERIFIED]");
    }

    #[test]
    fn text_render_puts_label_tag_at_head_of_finding() {
        let f = Finding {
            severity: Severity::High,
            category: "bugs".into(),
            file: "f.rs".into(),
            line: Some(1),
            title: "boom".into(),
            detail: "".into(),
            suggestion: "".into(),
            human_judgment: false,
            label: "inferred".to_string(),
        };
        let report = ReviewReport::new("s".into(), "m".into(), vec![f]);
        let text = render_text(&report, Severity::High);
        assert!(text.contains("1. [INFERRED] [high / bugs] boom"), "{text}");
    }

    #[test]
    fn system_prompt_demands_json_only() {
        let p = review_system_prompt();
        assert!(p.contains("```json"));
        assert!(p.contains("CANNOT modify"));
        assert!(p.contains("critical"));
    }

    #[test]
    fn user_prompt_embeds_diff_and_files() {
        let diff = CollectedDiff {
            scope_description: "uncommitted changes".into(),
            files: vec!["a.rs".into()],
            diff_text: "diff --git a/a.rs b/a.rs".into(),
            truncated: false,
        };
        let p = build_review_prompt(&diff);
        assert!(p.contains("a.rs"));
        assert!(p.contains("```diff"));
    }

    // Issue #464: filenames with spaces and git-quoted non-ASCII
    // filenames must not be silently dropped from the file list.
    #[test]
    fn changed_files_from_diff_keeps_spaced_and_quoted_paths() {
        let diff = "diff --git a/my file.rs b/my file.rs\n--- a/my file.rs\n+++ b/my file.rs\n\
             diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n\
             diff --git \"a/my caf\\303\\251.rs\" \"b/my caf\\303\\251.rs\"\n\
             diff --git a/plain.rs b/plain.rs\n";
        assert_eq!(
            changed_files_from_diff(diff),
            vec!["my file.rs", "café.rs", "my café.rs", "plain.rs"]
        );
    }

    #[test]
    fn changed_files_from_diff_uses_b_side_on_rename_with_spaces() {
        let diff = "diff --git a/old name.rs b/new name.rs\n";
        assert_eq!(changed_files_from_diff(diff), vec!["new name.rs"]);
    }

    // Mutant-kill tests for unquote_git_path (#464, PR #575). Each
    // escape arm below must decode to its control character; deleting
    // any single arm makes its test fail, because the escape then
    // falls through to `_ => return None`.
    #[test]
    fn unquote_git_path_bell_escape() {
        assert_eq!(unquote_git_path("\"\\a\""), Some("\u{7}".to_string()));
    }

    #[test]
    fn unquote_git_path_backspace_escape() {
        assert_eq!(unquote_git_path("\"\\b\""), Some("\u{8}".to_string()));
    }

    #[test]
    fn unquote_git_path_form_feed_escape() {
        assert_eq!(unquote_git_path("\"\\f\""), Some("\u{c}".to_string()));
    }

    #[test]
    fn unquote_git_path_newline_escape() {
        assert_eq!(unquote_git_path("\"\\n\""), Some("\n".to_string()));
    }

    #[test]
    fn unquote_git_path_carriage_return_escape() {
        assert_eq!(unquote_git_path("\"\\r\""), Some("\r".to_string()));
    }

    #[test]
    fn unquote_git_path_tab_escape() {
        assert_eq!(unquote_git_path("\"\\t\""), Some("\t".to_string()));
    }

    #[test]
    fn unquote_git_path_vertical_tab_escape() {
        assert_eq!(unquote_git_path("\"\\v\""), Some("\u{b}".to_string()));
    }

    #[test]
    fn unquote_git_path_backslash_escape() {
        assert_eq!(unquote_git_path("\"\\\\\""), Some("\\".to_string()));
    }

    #[test]
    fn unquote_git_path_quote_escape() {
        assert_eq!(unquote_git_path("\"\\\"\""), Some("\"".to_string()));
    }

    // Octal escapes: the exact accumulated value matters. `\101` is
    // 'A' only when `value * 8 + digit` is computed exactly, so this
    // kills both `+` -> `-` and `+` -> `*` on line 369.
    #[test]
    fn unquote_git_path_octal_101_is_capital_a() {
        assert_eq!(unquote_git_path("\"\\101\""), Some("A".to_string()));
    }

    #[test]
    fn unquote_git_path_octal_cafe_escapes_are_exact() {
        assert_eq!(unquote_git_path("\"\\303\\251\""), Some("é".to_string()));
    }

    // Only three octal digits are consumed: `\123` is 'S' and the
    // trailing `4` stays a literal character. With `count <= 3` the
    // fourth digit is consumed instead, giving 668 > 255, so the
    // mutant returns `None`.
    #[test]
    fn unquote_git_path_octal_stops_after_three_digits() {
        assert_eq!(unquote_git_path("\"\\1234\""), Some("S4".to_string()));
    }

    // 0o377 == 255 is the largest valid octal byte. With `>=` the
    // mutant rejects it and returns `None`; the lone 0xFF byte
    // decodes lossy to U+FFFD.
    #[test]
    fn unquote_git_path_octal_377_is_allowed() {
        assert_eq!(unquote_git_path("\"\\377\""), Some("\u{fffd}".to_string()));
    }

    // 0o400 == 256 and 0o777 == 511 overflow a byte and must return
    // `None`. With `==` instead of `>` the mutant only rejects
    // exactly 255, so both of these leak through as `Some`.
    #[test]
    fn unquote_git_path_octal_overflow_returns_none() {
        assert_eq!(unquote_git_path("\"\\400\""), None);
        assert_eq!(unquote_git_path("\"\\777\""), None);
    }

    // split_diff_git_header: a non-rename path containing a literal
    // ` b/` must take the early `a == b` return with the exact pair.
    // With `!=` the mutant returns the first (wrong) fallback pair
    // instead, because the equal pair no longer returns early.
    #[test]
    fn split_diff_git_header_non_rename_path_with_b_slash_returns_exact_pair() {
        assert_eq!(
            split_diff_git_header("a/my b/file.rs b/my b/file.rs"),
            Some(("my b/file.rs".to_string(), "my b/file.rs".to_string()))
        );
    }

    // Z Tester review of PR #575 (P2): renaming `old b/part.rs` to
    // `name.rs` returned "part.rs b/name.rs" because the first " b/"
    // inside the old path was taken as the header separator. The
    // `rename to` line carries the destination verbatim, so it is the
    // unambiguous source for the renamed file.
    #[test]
    fn changed_files_from_diff_uses_rename_to_when_old_path_contains_b_slash() {
        let diff = "diff --git a/old b/part.rs b/name.rs\n\
             similarity index 100%\n\
             rename from old b/part.rs\n\
             rename to name.rs\n";
        assert_eq!(changed_files_from_diff(diff), vec!["name.rs"]);
    }

    #[test]
    fn changed_files_from_diff_unquotes_rename_to_path() {
        let diff = "diff --git \"a/old b/caf\\303\\251.rs\" \"b/new caf\\303\\251.rs\"\n\
             similarity index 100%\n\
             rename from \"old b/caf\\303\\251.rs\"\n\
             rename to \"new caf\\303\\251.rs\"\n";
        assert_eq!(changed_files_from_diff(diff), vec!["new café.rs"]);
    }
}
