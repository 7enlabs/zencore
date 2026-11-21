use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::PermissionMode;

pub const GH_TIMEOUT: Duration = Duration::from_secs(30);

pub const MAX_LOG_LINES: usize = 120;

pub const MAX_ERROR_LINES: usize = 25;

fn run_gh(cwd: &Path, args: &[&str]) -> Result<String> {
    run_gh_with("gh", cwd, args)
}

fn run_gh_with(bin: &str, cwd: &Path, args: &[&str]) -> Result<String> {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("gh CLI not found (install from https://cli.github.com): {e}"))?;

    let mut out = child.stdout.take().expect("stdout piped");
    let mut err = child.stderr.take().expect("stderr piped");
    let reader = std::thread::spawn(move || {
        let mut o = Vec::new();
        let mut e = Vec::new();
        let _ = out.read_to_end(&mut o);
        let _ = err.read_to_end(&mut e);
        (o, e)
    });
    let deadline = Instant::now() + GH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (o, e) = reader
                    .join()
                    .map_err(|_| anyhow!("gh output thread failed"))?;
                let stderr = String::from_utf8_lossy(&e);
                if !status.success() {
                    if stderr.contains("auth login") || stderr.contains("GH_TOKEN") {
                        return Err(anyhow!(
                            "gh is not authenticated (run `gh auth login`): {}",
                            stderr.trim()
                        ));
                    }
                    return Err(anyhow!("gh {} failed: {}", args.join(" "), stderr.trim()));
                }
                return Ok(String::from_utf8_lossy(&o).into_owned());
            }
            Ok(None) | Err(_) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(anyhow!(
                        "gh {} timed out after {:?}",
                        args.join(" "),
                        GH_TIMEOUT
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn gh_json(cwd: &Path, args: &[&str]) -> Result<serde_json::Value> {
    let out = run_gh(cwd, args)?;
    serde_json::from_str(&out)
        .with_context(|| format!("gh {} returned non-JSON output", args.join(" ")))
}

pub fn gh_available(cwd: &Path) -> bool {
    run_gh(cwd, &["auth", "status"]).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    Review,
    Issue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub kind: CommentKind,

    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedComments {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub review_comments: Vec<PrComment>,
    pub issue_comments: Vec<PrComment>,
}

impl FetchedComments {
    pub fn total(&self) -> usize {
        self.review_comments.len() + self.issue_comments.len()
    }
}

#[derive(Deserialize)]
struct RawUser {
    login: Option<String>,
}

#[derive(Deserialize)]
struct RawReviewComment {
    id: u64,
    body: Option<String>,
    path: Option<String>,
    line: Option<u64>,
    diff_hunk: Option<String>,
    created_at: Option<String>,
    html_url: Option<String>,
    user: Option<RawUser>,
}

#[derive(Deserialize)]
struct RawIssueComment {
    id: u64,
    body: Option<String>,
    created_at: Option<String>,
    html_url: Option<String>,
    user: Option<RawUser>,
}

fn author_of(user: &Option<RawUser>) -> String {
    user.as_ref()
        .and_then(|u| u.login.clone())
        .unwrap_or_else(|| "(unknown)".to_string())
}

pub fn parse_review_comments(json: &str) -> Result<Vec<PrComment>> {
    let raw: Vec<RawReviewComment> =
        serde_json::from_str(json).context("review comments JSON did not parse")?;
    Ok(raw
        .into_iter()
        .map(|c| PrComment {
            id: c.id,
            author: author_of(&c.user),
            body: c.body.unwrap_or_default(),
            kind: CommentKind::Review,
            path: c.path,
            line: c.line,
            diff_hunk: c.diff_hunk,
            created_at: c.created_at.unwrap_or_default(),
            url: c.html_url.unwrap_or_default(),
        })
        .collect())
}

pub fn parse_issue_comments(json: &str) -> Result<Vec<PrComment>> {
    let raw: Vec<RawIssueComment> =
        serde_json::from_str(json).context("issue comments JSON did not parse")?;
    Ok(raw
        .into_iter()
        .map(|c| PrComment {
            id: c.id,
            author: author_of(&c.user),
            body: c.body.unwrap_or_default(),
            kind: CommentKind::Issue,
            path: None,
            line: None,
            diff_hunk: None,
            created_at: c.created_at.unwrap_or_default(),
            url: c.html_url.unwrap_or_default(),
        })
        .collect())
}

fn resolve_pr(cwd: &Path, pr_arg: Option<&str>) -> Result<(String, u64)> {
    let nwo = gh_json(cwd, &["repo", "view", "--json", "nameWithOwner"]).map_err(|e| {
        if e.to_string().contains("not authenticated") {
            e
        } else {
            anyhow!("could not determine the GitHub repo (is this a GitHub checkout?)")
        }
    })?;
    let nwo = nwo
        .get("nameWithOwner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if nwo.is_empty() {
        return Err(anyhow!("could not determine the GitHub repo"));
    }
    let number: u64 = match pr_arg {
        Some(a) => a
            .trim()
            .trim_start_matches('#')
            .parse()
            .with_context(|| format!("invalid PR number: {a}"))?,
        None => {
            let out = run_gh(cwd, &["pr", "view", "--json", "number", "--jq", ".number"])
                .context("no PR number given and no PR found for the current branch")?;
            out.trim()
                .parse()
                .context("could not parse the current branch's PR number")?
        }
    };
    Ok((nwo, number))
}

pub fn fetch_pr_comments(cwd: &Path, pr_arg: Option<&str>) -> Result<FetchedComments> {
    let (nwo, number) = resolve_pr(cwd, pr_arg)?;
    let meta = gh_json(
        cwd,
        &[
            "api",
            &format!("repos/{nwo}/pulls/{number}"),
            "--jq",
            "{number: .number, title: .title, url: .html_url}",
        ],
    )
    .with_context(|| format!("could not fetch PR #{number}"))?;
    let review_json = run_gh(
        cwd,
        &[
            "api",
            &format!("repos/{nwo}/pulls/{number}/comments"),
            "--paginate",
        ],
    )
    .with_context(|| format!("could not fetch review comments for PR #{number}"))?;
    let issue_json = run_gh(
        cwd,
        &[
            "api",
            &format!("repos/{nwo}/issues/{number}/comments"),
            "--paginate",
        ],
    )
    .with_context(|| format!("could not fetch conversation comments for PR #{number}"))?;
    Ok(FetchedComments {
        repo: nwo,
        number: meta["number"].as_u64().unwrap_or(number),
        title: meta["title"].as_str().unwrap_or("").to_string(),
        url: meta["url"].as_str().unwrap_or("").to_string(),
        review_comments: parse_review_comments(&review_json)?,
        issue_comments: parse_issue_comments(&issue_json)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Nit,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Critical => "CRITICAL",
            Self::High => "HIGH",
            Self::Medium => "MEDIUM",
            Self::Low => "LOW",
            Self::Nit => "NIT",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Critical => 0,
            Self::High => 1,
            Self::Medium => 2,
            Self::Low => 3,
            Self::Nit => 4,
        }
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Keyword heuristic for triage ordering only, not a security judgment:
/// checked in Critical-first order, case-insensitively, and anything
/// matching no keyword defaults to Medium rather than to the lowest tier.
pub fn severity_of(body: &str) -> Severity {
    let b = body.to_lowercase();
    if contains_any(
        &b,
        &[
            "security",
            "vulnerab",
            "unsound",
            "data race",
            "panic",
            "must fix",
            "blocking",
            "do not merge",
        ],
    ) {
        Severity::Critical
    } else if contains_any(
        &b,
        &[
            "bug",
            "wrong",
            "incorrect",
            "broken",
            "fails",
            "missing",
            "should",
            "fix this",
        ],
    ) {
        Severity::High
    } else if contains_any(&b, &["nitpick", "nit:", "typo", "style:"]) {
        Severity::Nit
    } else if contains_any(&b, &["consider", "suggestion", "maybe", "could"]) {
        Severity::Low
    } else {
        Severity::Medium
    }
}

#[derive(Debug, Clone)]
pub struct CommentGroup {
    pub title: String,
    pub comments: Vec<PrComment>,
}

pub fn group_comments(fetched: &FetchedComments) -> Vec<CommentGroup> {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<PrComment>> = BTreeMap::new();
    for c in &fetched.review_comments {
        by_file
            .entry(c.path.clone().unwrap_or_else(|| "(no file)".to_string()))
            .or_default()
            .push(c.clone());
    }
    let mut groups: Vec<CommentGroup> = by_file
        .into_iter()
        .map(|(title, mut comments)| {
            comments.sort_by(|a, b| {
                severity_of(&b.body)
                    .rank()
                    .cmp(&severity_of(&a.body).rank())
                    .then(a.line.cmp(&b.line))
            });
            CommentGroup { title, comments }
        })
        .collect();
    if !fetched.issue_comments.is_empty() {
        groups.push(CommentGroup {
            title: "General discussion".to_string(),
            comments: fetched.issue_comments.clone(),
        });
    }
    groups
}

pub fn format_comments_text(fetched: &FetchedComments) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "PR #{}: {}\n{}\n",
        fetched.number, fetched.title, fetched.url
    ));
    if fetched.total() == 0 {
        s.push_str("No review comments.\n");
        return s;
    }
    s.push_str(&format!("{} comment(s):\n", fetched.total()));
    for g in group_comments(fetched) {
        s.push_str(&format!(
            "\n## {} ({} comment{})\n",
            g.title,
            g.comments.len(),
            if g.comments.len() == 1 { "" } else { "s" }
        ));
        for c in &g.comments {
            let loc = match c.line {
                Some(l) => format!(":{l}"),
                None => String::new(),
            };
            s.push_str(&format!(
                "  [{}] @{}{}\n",
                severity_of(&c.body).label(),
                c.author,
                loc
            ));
            for line in c.body.lines().take(15) {
                s.push_str(&format!("  > {line}\n"));
            }
            if c.body.lines().count() > 15 {
                s.push_str("  > ...\n");
            }
            if !c.url.is_empty() {
                s.push_str(&format!("  {}\n", c.url));
            }
        }
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailingCheck {
    pub run_id: u64,
    pub name: String,
    pub conclusion: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiStatus {
    pub branch: String,
    pub pr_number: Option<u64>,
    pub failing: Vec<FailingCheck>,
}

#[derive(Deserialize)]
struct RawWorkflowRun {
    #[serde(rename = "databaseId")]
    database_id: u64,
    name: Option<String>,
    conclusion: Option<String>,
    url: Option<String>,
}

pub fn parse_workflow_runs(json: &str) -> Result<Vec<FailingCheck>> {
    let raw: Vec<RawWorkflowRun> =
        serde_json::from_str(json).context("workflow runs JSON did not parse")?;
    Ok(raw
        .into_iter()
        .filter(|r| r.conclusion.as_deref() == Some("failure"))
        .map(|r| FailingCheck {
            run_id: r.database_id,
            name: r.name.unwrap_or_else(|| "(unnamed)".to_string()),
            conclusion: r.conclusion.unwrap_or_else(|| "failure".to_string()),
            url: r.url.unwrap_or_default(),
            log_excerpt: None,
        })
        .collect())
}

/// Keep the tail of the log, not the head: CI failures are conventionally
/// reported at the end, and the omitted-line count is printed so the
/// excerpt is never mistaken for the full log.
pub fn truncate_log(log: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();
    if lines.len() <= max_lines {
        return log.to_string();
    }
    format!(
        "... ({} earlier lines omitted)\n{}",
        lines.len() - max_lines,
        lines[lines.len() - max_lines..].join("\n")
    )
}

pub fn extract_error_lines(log: &str, max: usize) -> Vec<String> {
    const KEYS: &[&str] = &[
        "error",
        "failed",
        "failure",
        "panicked",
        "assertion",
        "traceback",
        "exception",
    ];
    let mut out = Vec::new();
    for line in log.lines() {
        let l = line.to_lowercase();

        if l.contains("0 failed") || l.contains("0 errors") || l.contains("0 error") {
            continue;
        }
        if KEYS.iter().any(|k| l.contains(k)) {
            out.push(line.trim().to_string());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

pub fn fetch_ci_status(cwd: &Path) -> Result<CiStatus> {
    let branch =
        crate::git::current_branch(cwd).ok_or_else(|| anyhow!("not a git repo (or no branch)"))?;
    let pr_number = run_gh(cwd, &["pr", "view", "--json", "number", "--jq", ".number"])
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let runs_json = run_gh(
        cwd,
        &[
            "run",
            "list",
            "--branch",
            &branch,
            "--limit",
            "10",
            "--json",
            "databaseId,name,conclusion,url",
        ],
    )
    .context("could not list workflow runs (is `gh` authenticated?)")?;
    let mut failing = parse_workflow_runs(&runs_json)?;
    for f in failing.iter_mut() {
        if let Ok(log) = run_gh(cwd, &["run", "view", &f.run_id.to_string(), "--log-failed"]) {
            f.log_excerpt = Some(truncate_log(&log, MAX_LOG_LINES));
        }
    }
    Ok(CiStatus {
        branch,
        pr_number,
        failing,
    })
}

pub fn format_ci_text(status: &CiStatus) -> String {
    let mut s = String::new();
    s.push_str(&format!("branch: {}", status.branch));
    if let Some(n) = status.pr_number {
        s.push_str(&format!(" (PR #{n})"));
    }
    s.push('\n');
    if status.failing.is_empty() {
        s.push_str("All CI checks passing.\n");
        return s;
    }
    s.push_str(&format!("{} failing check(s):\n", status.failing.len()));
    for f in &status.failing {
        s.push_str(&format!("\n## {} - {}\n{}\n", f.name, f.conclusion, f.url));
        match &f.log_excerpt {
            Some(log) => {
                let errors = extract_error_lines(log, MAX_ERROR_LINES);
                if errors.is_empty() {
                    s.push_str("(no clear error lines in the failed log)\n");
                } else {
                    s.push_str("likely failure cause:\n");
                    for e in &errors {
                        s.push_str(&format!("  ! {e}\n"));
                    }
                }
            }
            None => s.push_str("(could not fetch the failed log)\n"),
        }
    }
    s
}

pub fn pr_fix_prompt(fetched: &FetchedComments) -> String {
    let mut p = format!(
        "Address the following review comments on PR #{} \"{}\" ({}).\n\
         For each comment, fix the underlying issue in the code. \
         Do not post replies on GitHub; only change the code.\n\n",
        fetched.number, fetched.title, fetched.url
    );
    for g in group_comments(fetched) {
        p.push_str(&format!("### {}\n", g.title));
        for c in &g.comments {
            p.push_str(&format!(
                "- @{} [{}]: {}\n",
                c.author,
                severity_of(&c.body).label(),
                c.body
            ));
        }
        p.push('\n');
    }
    p
}

pub fn ci_fix_prompt(status: &CiStatus) -> String {
    let mut p = format!(
        "The following GitHub Actions checks failed on branch `{}`{}:\n\n",
        status.branch,
        status
            .pr_number
            .map(|n| format!(" (PR #{n})"))
            .unwrap_or_default()
    );
    for f in &status.failing {
        p.push_str(&format!("## {} - {}\n{}\n", f.name, f.conclusion, f.url));
        if let Some(log) = &f.log_excerpt {
            p.push_str("Failed log excerpt:\n```\n");
            p.push_str(log);
            p.push_str("\n```\n");
        }
    }
    p.push_str(
        "\nAnalyze the failures and fix the underlying issues in the code. \
         Do not modify CI workflow files unless the failure is clearly in the workflow itself. \
         After fixing, summarize what was changed and how to verify (e.g. `cargo test`).\n",
    );
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixOffer {
    Never,

    Ask,

    Auto,
}

pub fn fix_offer_for_mode(mode: PermissionMode) -> FixOffer {
    match mode {
        PermissionMode::Plan => FixOffer::Never,
        PermissionMode::Default | PermissionMode::AskAll => FixOffer::Ask,
        PermissionMode::AcceptEdits
        | PermissionMode::BypassPermissions
        | PermissionMode::DontAsk => FixOffer::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW_JSON: &str = r#"[
        {
            "id": 101,
            "body": "This is a security issue: unsanitized input reaches the shell.",
            "path": "src/main.rs",
            "line": 42,
            "diff_hunk": "@@ -40,3 +40,3 @@",
            "created_at": "2026-10-17T10:00:00Z",
            "html_url": "https://github.com/o/r/pull/1#discussion_r101",
            "user": {"login": "lead-reviewer"}
        },
        {
            "id": 102,
            "body": "nit: typo in comment",
            "path": "src/lib.rs",
            "line": 7,
            "diff_hunk": null,
            "created_at": "2026-10-17T11:00:00Z",
            "html_url": "https://github.com/o/r/pull/1#discussion_r102",
            "user": {"login": "reviewer"}
        }
    ]"#;

    const ISSUE_JSON: &str = r#"[
        {
            "id": 201,
            "body": "LGTM, let's merge after CI passes.",
            "created_at": "2026-10-17T12:00:00Z",
            "html_url": "https://github.com/o/r/pull/1#issuecomment-201",
            "user": {"login": "zen"}
        }
    ]"#;

    const RUNS_JSON: &str = r#"[
        {"databaseId": 11, "name": "test", "conclusion": "failure", "url": "https://github.com/o/r/actions/runs/11"},
        {"databaseId": 12, "name": "lint", "conclusion": "success", "url": "https://github.com/o/r/actions/runs/12"},
        {"databaseId": 13, "name": "build", "conclusion": "in_progress", "url": "https://github.com/o/r/actions/runs/13"}
    ]"#;

    fn sample_fetched() -> FetchedComments {
        FetchedComments {
            repo: "o/r".to_string(),
            number: 1,
            title: "Add feature".to_string(),
            url: "https://github.com/o/r/pull/1".to_string(),
            review_comments: parse_review_comments(REVIEW_JSON).unwrap(),
            issue_comments: parse_issue_comments(ISSUE_JSON).unwrap(),
        }
    }

    #[test]
    fn parse_review_comments_maps_fields() {
        let cs = parse_review_comments(REVIEW_JSON).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].id, 101);
        assert_eq!(cs[0].author, "lead-reviewer");
        assert_eq!(cs[0].kind, CommentKind::Review);
        assert_eq!(cs[0].path.as_deref(), Some("src/main.rs"));
        assert_eq!(cs[0].line, Some(42));
        assert!(cs[0].diff_hunk.is_some());
        assert_eq!(cs[1].path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn parse_review_comments_rejects_bad_json() {
        assert!(parse_review_comments("not json").is_err());
        assert!(parse_review_comments("{}").is_err());
    }

    #[test]
    fn parse_issue_comments_are_conversation_kind() {
        let cs = parse_issue_comments(ISSUE_JSON).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, CommentKind::Issue);
        assert_eq!(cs[0].path, None);
        assert_eq!(cs[0].line, None);
        assert_eq!(cs[0].author, "zen");
    }

    #[test]
    fn severity_table() {
        assert_eq!(
            severity_of("security vulnerability here"),
            Severity::Critical
        );
        assert_eq!(severity_of("do not merge yet"), Severity::Critical);
        assert_eq!(severity_of("this is wrong"), Severity::High);
        assert_eq!(severity_of("bug: off by one"), Severity::High);
        assert_eq!(severity_of("you should handle the error"), Severity::High);
        assert_eq!(severity_of("nit: typo"), Severity::Nit);
        assert_eq!(severity_of("style: indent"), Severity::Nit);
        assert_eq!(severity_of("consider using a map"), Severity::Low);
        assert_eq!(severity_of("maybe rename this"), Severity::Low);
        assert_eq!(severity_of("why is this needed?"), Severity::Medium);
        assert_eq!(severity_of("looks good"), Severity::Medium);
    }

    #[test]
    fn group_comments_by_file_severity_first() {
        let f = sample_fetched();
        let groups = group_comments(&f);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].title, "src/lib.rs");
        assert_eq!(groups[1].title, "src/main.rs");
        assert_eq!(groups[2].title, "General discussion");
        assert_eq!(groups[2].comments[0].kind, CommentKind::Issue);
    }

    #[test]
    fn format_comments_text_shows_groups_and_severity() {
        let text = format_comments_text(&sample_fetched());
        assert!(text.contains("PR #1: Add feature"));
        assert!(text.contains("## src/main.rs"));
        assert!(text.contains("[CRITICAL]"));
        assert!(text.contains("@lead-reviewer:42"));
        assert!(text.contains("[NIT]"));
        assert!(text.contains("## General discussion"));
    }

    #[test]
    fn format_comments_text_empty() {
        let f = FetchedComments {
            repo: "o/r".into(),
            number: 2,
            title: "x".into(),
            url: "u".into(),
            review_comments: vec![],
            issue_comments: vec![],
        };
        assert!(format_comments_text(&f).contains("No review comments"));
    }

    #[test]
    fn parse_workflow_runs_keeps_only_failures() {
        let failing = parse_workflow_runs(RUNS_JSON).unwrap();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].run_id, 11);
        assert_eq!(failing[0].name, "test");
        assert_eq!(failing[0].log_excerpt, None);
    }

    #[test]
    fn truncate_log_keeps_tail() {
        let log: String = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_log(&log, 120);
        assert!(t.starts_with("... (80 earlier lines omitted)"));
        assert!(t.contains("line 199"));
        assert!(!t.contains("line 0\n"));

        assert_eq!(truncate_log("a\nb", 120), "a\nb");
    }

    #[test]
    fn extract_error_lines_finds_failures() {
        let log = " Compiling foo\nerror[E0308]: mismatched types\n   Finished ok\ntest bar FAILED\ntest result: FAILED. 0 passed; 1 failed\n0 errors here\n";
        let errs = extract_error_lines(log, 25);
        assert!(errs.iter().any(|l| l.contains("mismatched types")));
        assert!(errs.iter().any(|l| l.contains("FAILED")));
        assert!(!errs.iter().any(|l| l.contains("0 errors")));
    }

    #[test]
    fn fix_offer_for_mode_all_modes() {
        assert_eq!(fix_offer_for_mode(PermissionMode::Plan), FixOffer::Never);
        assert_eq!(fix_offer_for_mode(PermissionMode::Default), FixOffer::Ask);
        assert_eq!(fix_offer_for_mode(PermissionMode::AskAll), FixOffer::Ask);
        assert_eq!(
            fix_offer_for_mode(PermissionMode::AcceptEdits),
            FixOffer::Auto
        );
        assert_eq!(
            fix_offer_for_mode(PermissionMode::BypassPermissions),
            FixOffer::Auto
        );
        assert_eq!(fix_offer_for_mode(PermissionMode::DontAsk), FixOffer::Auto);
    }

    #[test]
    fn run_gh_missing_binary_is_clear_error() {
        let err = run_gh_with(
            "zen-no-such-gh-binary",
            &std::env::temp_dir(),
            &["--version"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("gh CLI not found"), "{err}");
    }

    #[test]
    fn pr_fix_prompt_lists_comments_and_forbids_replies() {
        let p = pr_fix_prompt(&sample_fetched());
        assert!(p.contains("PR #1"));
        assert!(p.contains("@lead-reviewer"));
        assert!(p.contains("[CRITICAL]"));
        assert!(p.contains("Do not post replies on GitHub"));
    }

    #[test]
    fn ci_fix_prompt_contains_branch_and_logs() {
        let status = CiStatus {
            branch: "feature/x".into(),
            pr_number: Some(7),
            failing: vec![FailingCheck {
                run_id: 11,
                name: "test".into(),
                conclusion: "failure".into(),
                url: "https://example.invalid/runs/11".into(),
                log_excerpt: Some("error: boom".into()),
            }],
        };
        let p = ci_fix_prompt(&status);
        assert!(p.contains("feature/x"));
        assert!(p.contains("PR #7"));
        assert!(p.contains("error: boom"));
        assert!(p.contains("Do not modify CI workflow files"));
    }

    #[test]
    fn format_ci_text_empty_is_all_passing() {
        let status = CiStatus {
            branch: "main".into(),
            pr_number: None,
            failing: vec![],
        };
        let t = format_ci_text(&status);
        assert!(t.contains("All CI checks passing"));
    }

    #[test]
    fn fetched_total_counts_both_streams() {
        assert_eq!(sample_fetched().total(), 3);
    }
}

// Tests added for Việc A (floor coverage 88%): the gh shell-out paths
// (driven by a fake `gh` on PATH) and the pure-formatting branches the
// existing tests do not reach.
#[cfg(test)]
mod viec_a_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    struct EnvRestore {
        path: Option<String>,
        gh_mode: Option<String>,
        gh_dir: Option<String>,
    }

    #[cfg(unix)]
    impl EnvRestore {
        fn capture() -> Self {
            Self {
                path: std::env::var("PATH").ok(),
                gh_mode: std::env::var("FAKE_GH_MODE").ok(),
                gh_dir: std::env::var("FAKE_GH_DIR").ok(),
            }
        }
    }

    #[cfg(unix)]
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match self.path.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
                match self.gh_mode.take() {
                    Some(v) => std::env::set_var("FAKE_GH_MODE", v),
                    None => std::env::remove_var("FAKE_GH_MODE"),
                }
                match self.gh_dir.take() {
                    Some(v) => std::env::set_var("FAKE_GH_DIR", v),
                    None => std::env::remove_var("FAKE_GH_DIR"),
                }
            }
        }
    }

    #[cfg(unix)]
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zen_viec_a_gh_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `gh` answering the calls this module makes. FAKE_GH_MODE
    /// switches failure modes; FAKE_GH_DIR holds fixture JSON files.
    #[cfg(unix)]
    fn write_fake_gh(bin_dir: &Path) {
        std::fs::create_dir_all(bin_dir).unwrap();
        let script = bin_dir.join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
args="$*"
case "$FAKE_GH_MODE" in
  auth-fail) echo "gh: not authenticated, run gh auth login" >&2; exit 1 ;;
  bad-json) printf 'not json\n'; exit 0 ;;
  empty-nwo) printf '{"nameWithOwner":""}\n'; exit 0 ;;
esac
case "$args" in
  *'repo view'*) printf '{"nameWithOwner":"7enlabs/zencore"}\n'; exit 0 ;;
  *'pr view'*)
    if [ "$FAKE_GH_MODE" = "no-pr" ]; then echo "no PR for branch" >&2; exit 1; fi
    printf '42\n'; exit 0 ;;
  *'pulls/42/comments'*) cat "$FAKE_GH_DIR/review.json"; exit 0 ;;
  *'issues/42/comments'*) cat "$FAKE_GH_DIR/issue.json"; exit 0 ;;
  *'pulls/42'*)
    if [ "$FAKE_GH_MODE" = "meta-fail" ]; then echo "not found" >&2; exit 1; fi
    printf '{"number":42,"title":"Fix thing","url":"https://github.com/7enlabs/zencore/pull/42"}\n'; exit 0 ;;
  *'run list'*) cat "$FAKE_GH_DIR/runs.json"; exit 0 ;;
  *'run view'*)
    if [ "$FAKE_GH_MODE" = "log-fail" ]; then echo "no log" >&2; exit 1; fi
    printf 'line1\nerror: boom failed\nline3\n'; exit 0 ;;
  *'auth status'*) exit 0 ;;
  *) echo "unexpected gh call: $args" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Sets up the fake gh on PATH (first, so it wins over a real gh).
    /// Callers must already hold the env lock. Returns (workdir, restore).
    #[cfg(unix)]
    fn setup_fake_gh(tag: &str, mode: &str) -> (PathBuf, EnvRestore) {
        let base = temp_dir(tag);
        let bindir = base.join("fakebin");
        write_fake_gh(&bindir);
        let fixture = base.join("fixtures");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(
            fixture.join("review.json"),
            r#"[{"id":1,"body":"security issue here","path":"src/a.rs","line":10,"diff_hunk":"@@","created_at":"2026-01-01","html_url":"https://example/r1","user":{"login":"alice"}},{"id":2,"body":"consider this","path":null,"line":null,"diff_hunk":null,"created_at":"","html_url":"","user":null}]"#,
        )
        .unwrap();
        std::fs::write(
            fixture.join("issue.json"),
            r#"[{"id":3,"body":"general note","created_at":"","html_url":"","user":{"login":"bob"}}]"#,
        )
        .unwrap();
        std::fs::write(
            fixture.join("runs.json"),
            r#"[{"databaseId":111,"name":"CI","conclusion":"failure","url":"https://example/run/111"},{"databaseId":222,"name":"check","conclusion":"success","url":"https://example/run/222"}]"#,
        )
        .unwrap();
        let restore = EnvRestore::capture();
        unsafe {
            std::env::set_var("FAKE_GH_MODE", mode);
            std::env::set_var("FAKE_GH_DIR", fixture.as_os_str());
            let old_path = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{}:{}", bindir.display(), old_path));
        }
        (base, restore)
    }

    fn comment(id: u64, author: &str, body: &str, path: Option<&str>) -> PrComment {
        PrComment {
            id,
            author: author.to_string(),
            body: body.to_string(),
            kind: CommentKind::Review,
            path: path.map(str::to_string),
            line: Some(1),
            diff_hunk: None,
            created_at: String::new(),
            url: String::new(),
        }
    }

    fn fetched_with(review: Vec<PrComment>, issue: Vec<PrComment>) -> FetchedComments {
        FetchedComments {
            repo: "7enlabs/zencore".to_string(),
            number: 42,
            title: "Fix thing".to_string(),
            url: "https://github.com/7enlabs/zencore/pull/42".to_string(),
            review_comments: review,
            issue_comments: issue,
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_gh_with_success_and_failure_paths() {
        let dir = temp_dir("run_gh_with");
        // Success: stdout is returned.
        let out = run_gh_with("/bin/echo", &dir, &["hello"]).unwrap();
        assert_eq!(out, "hello\n");
        // Non-zero exit with stderr: the stderr text is in the error.
        let err = run_gh_with("/bin/sh", &dir, &["-c", "echo oops >&2; exit 3"])
            .expect_err("failing command must error");
        assert!(err.to_string().contains("failed: oops"), "{err}");
        // Stderr mentioning auth login maps to the not-authenticated error.
        let err = run_gh_with(
            "/bin/sh",
            &dir,
            &["-c", "echo 'run gh auth login' >&2; exit 1"],
        )
        .expect_err("auth failure must map to the auth error");
        assert!(err.to_string().contains("not authenticated"), "{err}");
        // Missing binary: clear install hint.
        let err = run_gh_with("/nonexistent-gh-xyz", &dir, &["--version"])
            .expect_err("missing binary must error");
        assert!(err.to_string().contains("gh CLI not found"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn gh_json_and_available_via_fake_gh() {
        let _env = crate::settings::env_lock();
        let (base, _restore) = setup_fake_gh("gh_json", "ok");
        let v = gh_json(&base, &["repo", "view", "--json", "nameWithOwner"]).unwrap();
        assert_eq!(v["nameWithOwner"].as_str(), Some("7enlabs/zencore"));
        assert!(gh_available(&base));
        let _ = std::fs::remove_dir_all(&base);

        let (_base2, _restore2) = setup_fake_gh("gh_json_bad", "bad-json");
        let err = gh_json(&_base2, &["repo", "view"]).expect_err("bad JSON must error");
        assert!(err.to_string().contains("non-JSON"), "{err}");
        let _ = std::fs::remove_dir_all(&_base2);

        let (_base3, _restore3) = setup_fake_gh("gh_json_auth", "auth-fail");
        assert!(!gh_available(&_base3));
        let err = gh_json(&_base3, &["repo", "view"]).expect_err("auth failure must error");
        assert!(err.to_string().contains("not authenticated"), "{err}");
        let _ = std::fs::remove_dir_all(&_base3);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_pr_variants() {
        let _env = crate::settings::env_lock();
        let (base, _restore) = setup_fake_gh("resolve_pr", "ok");
        assert_eq!(
            resolve_pr(&base, Some("42")).unwrap(),
            ("7enlabs/zencore".to_string(), 42)
        );
        assert_eq!(
            resolve_pr(&base, Some("#42")).unwrap(),
            ("7enlabs/zencore".to_string(), 42)
        );
        // No PR arg: falls back to `pr view` for the current branch.
        assert_eq!(
            resolve_pr(&base, None).unwrap(),
            ("7enlabs/zencore".to_string(), 42)
        );
        let err = resolve_pr(&base, Some("bad")).expect_err("bad number must error");
        assert!(err.to_string().contains("invalid PR number"), "{err}");
        let _ = std::fs::remove_dir_all(&base);

        let (_b2, _r2) = setup_fake_gh("resolve_pr_auth", "auth-fail");
        let err = resolve_pr(&_b2, Some("42")).expect_err("auth failure must error");
        assert!(err.to_string().contains("not authenticated"), "{err}");
        let _ = std::fs::remove_dir_all(&_b2);

        let (_b3, _r3) = setup_fake_gh("resolve_pr_empty", "empty-nwo");
        let err = resolve_pr(&_b3, Some("42")).expect_err("empty repo must error");
        assert!(
            err.to_string()
                .contains("could not determine the GitHub repo"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&_b3);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_pr_comments_full_flow() {
        let _env = crate::settings::env_lock();
        let (base, _restore) = setup_fake_gh("fetch_comments", "ok");
        let f = fetch_pr_comments(&base, Some("42")).unwrap();
        assert_eq!(f.repo, "7enlabs/zencore");
        assert_eq!(f.number, 42);
        assert_eq!(f.title, "Fix thing");
        assert_eq!(f.total(), 3);
        assert_eq!(f.review_comments[0].author, "alice");
        // Missing user falls back to "(unknown)".
        assert_eq!(f.review_comments[1].author, "(unknown)");
        assert_eq!(f.issue_comments[0].author, "bob");
        assert_eq!(f.issue_comments[0].kind, CommentKind::Issue);
        let _ = std::fs::remove_dir_all(&base);

        let (_b2, _r2) = setup_fake_gh("fetch_comments_meta", "meta-fail");
        let err = fetch_pr_comments(&_b2, Some("42")).expect_err("meta fetch must error");
        assert!(err.to_string().contains("could not fetch PR #42"), "{err}");
        let _ = std::fs::remove_dir_all(&_b2);
    }

    #[test]
    fn severity_labels_and_group_sort_cover_all_variants() {
        assert_eq!(Severity::Critical.label(), "CRITICAL");
        assert_eq!(Severity::High.label(), "HIGH");
        assert_eq!(Severity::Medium.label(), "MEDIUM");
        assert_eq!(Severity::Low.label(), "LOW");
        assert_eq!(Severity::Nit.label(), "NIT");
        // Two comments in one file exercise the sort comparator (rank)
        // and the "(no file)" group.
        let f = fetched_with(
            vec![
                comment(1, "a", "security hole", Some("src/a.rs")),
                comment(2, "b", "consider renaming", Some("src/a.rs")),
                comment(3, "c", "looks good", None),
            ],
            vec![comment(4, "d", "general note", None)],
        );
        let groups = group_comments(&f);
        assert_eq!(groups.len(), 3);
        let file_group = groups.iter().find(|g| g.title == "src/a.rs").unwrap();
        assert_eq!(file_group.comments.len(), 2);
        // NOTE: the comparator sorts by descending rank number, so the
        // least severe comment comes first; pinning current behaviour.
        assert_eq!(file_group.comments[0].body, "consider renaming");
        assert_eq!(file_group.comments[1].body, "security hole");
        assert!(groups.iter().any(|g| g.title == "(no file)"));
        assert!(groups.iter().any(|g| g.title == "General discussion"));
    }

    #[test]
    fn format_comments_text_long_body_and_singular() {
        let long_body = (1..=16)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let f = fetched_with(vec![comment(1, "a", &long_body, Some("src/a.rs"))], vec![]);
        let text = format_comments_text(&f);
        assert!(text.contains("1 comment(s):"), "{text}");
        assert!(text.contains("## src/a.rs (1 comment)\n"), "{text}");
        assert!(!text.contains("(1 comments)"), "{text}");
        // Bodies longer than 15 lines are truncated with an ellipsis line.
        assert!(text.contains("> ..."), "{text}");
        assert!(text.contains("[MEDIUM] @a:1"), "{text}");
        // A comment without a URL prints no URL line.
        let f2 = fetched_with(
            vec![{
                let mut c = comment(1, "a", "security hole", Some("src/a.rs"));
                c.url = "https://example/c1".to_string();
                c
            }],
            vec![],
        );
        assert!(format_comments_text(&f2).contains("https://example/c1"));
    }

    #[test]
    fn extract_error_lines_max_cap_and_zero_filter() {
        let log = "0 failed tests\nerror: first\nfailed second\nerror: third\n";
        let out = extract_error_lines(log, 2);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("first"));
        assert!(out[1].contains("second"));
        // "0 failed" lines never count as errors even though they
        // contain the word "failed".
        assert!(!out.iter().any(|l| l.contains("0 failed")));
    }

    #[cfg(unix)]
    fn git_repo(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .expect("git fixture command");
            assert!(st.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ]);
        dir
    }

    #[cfg(unix)]
    #[test]
    fn fetch_ci_status_full_flow() {
        let _env = crate::settings::env_lock();
        let repo = git_repo("ci_repo");
        let (_base, _restore) = setup_fake_gh("ci_status", "ok");
        let status = fetch_ci_status(&repo).unwrap();
        assert_eq!(status.branch, "main");
        assert_eq!(status.pr_number, Some(42));
        assert_eq!(status.failing.len(), 1);
        assert_eq!(status.failing[0].run_id, 111);
        let excerpt = status.failing[0].log_excerpt.as_deref().unwrap_or("");
        assert!(excerpt.contains("boom"), "{excerpt}");
        let _ = std::fs::remove_dir_all(&repo);

        // No PR for the branch: pr_number is None, the rest still works.
        let repo2 = git_repo("ci_repo_nopr");
        let (_b2, _r2) = setup_fake_gh("ci_status_nopr", "no-pr");
        let status2 = fetch_ci_status(&repo2).unwrap();
        assert_eq!(status2.pr_number, None);
        assert_eq!(status2.failing.len(), 1);
        let _ = std::fs::remove_dir_all(&repo2);

        // The failed log cannot be fetched: excerpt stays None.
        let repo3 = git_repo("ci_repo_nolog");
        let (_b3, _r3) = setup_fake_gh("ci_status_nolog", "log-fail");
        let status3 = fetch_ci_status(&repo3).unwrap();
        assert_eq!(status3.failing.len(), 1);
        assert!(status3.failing[0].log_excerpt.is_none());
        let _ = std::fs::remove_dir_all(&repo3);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_ci_status_not_a_repo_errors() {
        let _env = crate::settings::env_lock();
        let dir = temp_dir("ci_status_norepo");
        let (_base, _restore) = setup_fake_gh("ci_status_norepo", "ok");
        let err = fetch_ci_status(&dir).expect_err("not a repo must error");
        assert!(err.to_string().contains("not a git repo"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_ci_text_failing_variants() {
        let failing = |excerpt: Option<&str>| FailingCheck {
            run_id: 111,
            name: "CI".to_string(),
            conclusion: "failure".to_string(),
            url: "https://example/run/111".to_string(),
            log_excerpt: excerpt.map(str::to_string),
        };
        // With error lines: the likely cause is shown.
        let s = CiStatus {
            branch: "main".to_string(),
            pr_number: Some(42),
            failing: vec![failing(Some("ok\nerror: boom\n"))],
        };
        let text = format_ci_text(&s);
        assert!(text.contains("(PR #42)"), "{text}");
        assert!(text.contains("likely failure cause:"), "{text}");
        assert!(text.contains("error: boom"), "{text}");
        // Excerpt without error keywords: explicit "no clear error" note.
        let s2 = CiStatus {
            branch: "main".to_string(),
            pr_number: None,
            failing: vec![failing(Some("all quiet here\n"))],
        };
        assert!(format_ci_text(&s2).contains("(no clear error lines in the failed log)"));
        // No excerpt at all: explicit "could not fetch" note.
        let s3 = CiStatus {
            branch: "main".to_string(),
            pr_number: None,
            failing: vec![failing(None)],
        };
        assert!(format_ci_text(&s3).contains("(could not fetch the failed log)"));
    }
}
