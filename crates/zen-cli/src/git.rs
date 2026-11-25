use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

pub const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(30);

struct Probe {
    child: Child,
    reader: std::thread::JoinHandle<Vec<u8>>,
}

fn spawn_probe(program: &str, cwd: &Path, args: &[&str]) -> Option<Probe> {
    let mut child = Command::new(program)
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    Some(Probe { child, reader })
}

fn finish_probe(probe: Option<Probe>, deadline: Instant) -> Option<String> {
    let Probe { mut child, reader } = probe?;

    let kill_and_reap = |mut child: Child, reader: std::thread::JoinHandle<Vec<u8>>| {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status
                    .success()
                    .then(|| String::from_utf8_lossy(&out).into_owned());
            }
            Ok(None) => {
                if Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                } else {
                    kill_and_reap(child, reader);
                    return None;
                }
            }

            Err(_) => {
                if Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                } else {
                    kill_and_reap(child, reader);
                    return None;
                }
            }
        }
    }
}

fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let probe = spawn_probe("git", cwd, args);
    finish_probe(probe, Instant::now() + timeout)
}

fn parse_branch(out: &str) -> Option<String> {
    let branch = out.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

fn parse_status(out: &str) -> String {
    let n = out.lines().filter(|l| !l.trim().is_empty()).count();
    if n == 0 {
        "clean".to_string()
    } else {
        format!("{} changed file(s)", n)
    }
}

pub fn git_info_with(
    program: &str,
    cwd: &Path,
    timeout: Duration,
) -> (Option<String>, Option<String>) {
    let deadline = Instant::now() + timeout;
    let branch = spawn_probe(program, cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let status = spawn_probe(program, cwd, &["status", "--porcelain"]);
    let branch = finish_probe(branch, deadline).and_then(|o| parse_branch(&o));
    let status = finish_probe(status, deadline).map(|o| parse_status(&o));
    (branch, status)
}

pub fn git_info(cwd: &Path) -> (Option<String>, Option<String>) {
    git_info_with("git", cwd, GIT_TIMEOUT)
}

pub fn current_branch(cwd: &Path) -> Option<String> {
    let deadline = Instant::now() + GIT_TIMEOUT;
    let p = spawn_probe("git", cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    finish_probe(p, deadline).and_then(|o| parse_branch(&o))
}

pub fn status_summary(cwd: &Path) -> Option<String> {
    let deadline = Instant::now() + GIT_TIMEOUT;
    let p = spawn_probe("git", cwd, &["status", "--porcelain"]);
    finish_probe(p, deadline).map(|o| parse_status(&o))
}

pub fn is_git_repo(cwd: &Path) -> bool {
    matches!(
        run_git(cwd, &["rev-parse", "--is-inside-work-tree"], GIT_TIMEOUT),
        Some(o) if o.trim() == "true"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    WorkingTree,

    Staged,

    Stat,
}

pub fn git_diff(cwd: &Path, mode: DiffMode, color: bool) -> Option<String> {
    let color_arg = if color {
        "--color=always"
    } else {
        "--color=never"
    };
    let args: Vec<&str> = match mode {
        DiffMode::WorkingTree => vec!["diff", color_arg, "HEAD", "--"],
        DiffMode::Staged => vec!["diff", color_arg, "--cached", "--"],
        DiffMode::Stat => vec!["diff", color_arg, "HEAD", "--stat", "--"],
    };
    run_git(cwd, &args, GIT_DIFF_TIMEOUT)
}

/// Unquote a git-quoted path (C-style quoting used by `status --porcelain`
/// when `core.quotepath` is on): handles `\n`, `\t`, `\"`, `\\` and octal
/// byte escapes (`\303\251` for the UTF-8 bytes of `é`). Octal escapes are
/// accumulated as bytes and decoded as UTF-8 (lossy if invalid), so a
/// multi-byte character split across escapes decodes as one character.
/// Unquoted input is returned unchanged.
pub(crate) fn unquote_path(p: &str) -> String {
    let p = p.trim();
    if p.len() < 2 || !p.starts_with('"') || !p.ends_with('"') {
        return p.to_string();
    }
    let inner = &p.as_bytes()[1..p.len() - 1];
    let mut bytes = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == b'\\' && i + 1 < inner.len() {
            match inner[i + 1] {
                b'n' => {
                    bytes.push(b'\n');
                    i += 2;
                }
                b't' => {
                    bytes.push(b'\t');
                    i += 2;
                }
                b'"' => {
                    bytes.push(b'"');
                    i += 2;
                }
                b'\\' => {
                    bytes.push(b'\\');
                    i += 2;
                }
                b'0'..=b'7' => {
                    let mut value: u32 = 0;
                    let mut count = 0;
                    while count < 3 && i + 1 + count < inner.len() {
                        let d = inner[i + 1 + count];
                        if !matches!(d, b'0'..=b'7') {
                            break;
                        }
                        value = value * 8 + (d - b'0') as u32;
                        count += 1;
                    }
                    if value > 0o377 {
                        // Not a byte value: keep the backslash literally and
                        // let the digits through on later iterations.
                        bytes.push(b'\\');
                        i += 1;
                    } else {
                        bytes.push(value as u8);
                        i += 1 + count;
                    }
                }
                _ => {
                    bytes.push(b'\\');
                    i += 1;
                }
            }
        } else {
            bytes.push(inner[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_untracked(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("?? "))
        .map(unquote_path)
        .collect()
}

pub fn untracked_files(cwd: &Path) -> Option<Vec<String>> {
    run_git(cwd, &["status", "--porcelain=v1"], GIT_DIFF_TIMEOUT).map(|o| parse_untracked(&o))
}

pub fn os_info() -> String {
    format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
}

pub fn create_worktree(cwd: &Path, path: &Path, branch: &str) -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "add", "-b", branch, "--"])
        .arg(path)
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;
    if out.status.success() {
        Ok(path.to_path_buf())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn list_worktrees(cwd: &Path) -> Vec<(String, String, String)> {
    let p = spawn_probe("git", cwd, &["worktree", "list", "--porcelain"]);
    let out = match finish_probe(p, Instant::now() + GIT_TIMEOUT) {
        Some(o) => o,
        None => return vec![],
    };
    parse_worktree_porcelain(&out)
}

fn parse_worktree_porcelain(out: &str) -> Vec<(String, String, String)> {
    let mut result = Vec::new();
    let (mut path, mut branch, mut head) = (String::new(), String::new(), String::new());
    for line in out.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !path.is_empty() {
                let b = if branch.is_empty() {
                    "detached".to_string()
                } else {
                    branch.clone()
                };
                result.push((path.clone(), b, head.clone()));
            }
            path.clear();
            branch.clear();
            head.clear();
        } else if let Some(p) = line.strip_prefix("worktree ") {
            path = p.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.trim_start_matches("refs/heads/").to_string();
        } else if let Some(c) = line.strip_prefix("HEAD ") {
            head = c.to_string();
        }
    }
    result
}

/// Worktree names are restricted to ASCII alphanumerics plus `-`, `_`,
/// `.`, with no leading `-` or `.` and no `..`: a leading `-` would be
/// parsed as a git flag, and `..` would let the name escape the worktrees
/// directory.
pub fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn remove_worktree(cwd: &Path, path: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "remove"])
        .arg(path)
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Isolation record for #11 (worktree isolation): one task owns exactly
/// one worktree, and no other task may claim or remove it. Stored as
/// `zen-isolation.json` inside the git directory of the worktree
/// (`git rev-parse --absolute-git-dir`), so it is invisible to the
/// checkout, moves with `git worktree move`, and disappears with
/// `git worktree remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIsolation {
    pub task: String,
    pub branch: String,
    pub created_unix: u64,
    pub created_by_session: String,
}

const ISOLATION_RECORD_FILE: &str = "zen-isolation.json";

fn git_dir_of(path: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

fn isolation_record_path(worktree_path: &Path) -> Option<PathBuf> {
    git_dir_of(worktree_path).map(|d| d.join(ISOLATION_RECORD_FILE))
}

/// Reads the isolation record of a worktree, if it has one. Returns
/// `None` for worktrees created before #11 (or by hand with plain git)
/// and for malformed records: both are treated as legacy, unowned
/// worktrees, never as owned by anyone.
pub fn read_isolation_record(worktree_path: &Path) -> Option<WorktreeIsolation> {
    let path = isolation_record_path(worktree_path)?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(WorktreeIsolation {
        task: v.get("task")?.as_str()?.to_string(),
        branch: v.get("branch")?.as_str()?.to_string(),
        created_unix: v.get("created_unix")?.as_u64()?,
        created_by_session: v
            .get("created_by_session")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn write_isolation_record(worktree_path: &Path, record: &WorktreeIsolation) -> Result<(), String> {
    let path = isolation_record_path(worktree_path)
        .ok_or_else(|| "cannot resolve git dir of worktree".to_string())?;
    let text = serde_json::json!({
        "task": record.task,
        "branch": record.branch,
        "created_unix": record.created_unix,
        "created_by_session": record.created_by_session,
    })
    .to_string();
    std::fs::write(path, text).map_err(|e| format!("failed to write isolation record: {e}"))
}

/// Writes (or rewrites) the isolation record for an existing worktree,
/// e.g. one a WorktreeCreate hook just created. The caller must already
/// have verified the path is a registered worktree of this repo.
pub fn create_isolation_record(
    worktree_path: &Path,
    task: &str,
    branch: &str,
    session_id: &str,
) -> Result<WorktreeIsolation, String> {
    if !valid_worktree_name(task) || !valid_worktree_name(branch) {
        return Err(format!(
            "invalid worktree name: {task} (letters, digits, - _ .)"
        ));
    }
    let record = WorktreeIsolation {
        task: task.to_string(),
        branch: branch.to_string(),
        created_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        created_by_session: session_id.to_string(),
    };
    write_isolation_record(worktree_path, &record)?;
    Ok(record)
}

fn canonical_or_raw(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// True when `path` is a worktree registered with the repo at `cwd`.
pub fn is_registered_worktree(cwd: &Path, path: &Path) -> bool {
    let want = canonical_or_raw(path);
    list_worktrees(cwd)
        .iter()
        .any(|(p, _, _)| canonical_or_raw(Path::new(p)) == want)
}

/// Creates a worktree owned by `task` on `branch`, with an isolation
/// record. Re-attaching the same task to its own worktree is idempotent;
/// any other task claiming the same path is refused, as is claiming a
/// legacy worktree that carries no record.
pub fn create_isolated_worktree(
    cwd: &Path,
    path: &Path,
    branch: &str,
    task: &str,
    session_id: &str,
) -> Result<PathBuf, String> {
    if !valid_worktree_name(branch) || !valid_worktree_name(task) {
        return Err(format!(
            "invalid worktree name: {task} (letters, digits, - _ .)"
        ));
    }
    if is_registered_worktree(cwd, path) {
        return match read_isolation_record(path) {
            Some(record) if record.task == task && record.branch == branch => {
                Ok(path.to_path_buf())
            }
            Some(record) => Err(format!(
                "worktree {} is isolated for task \"{}\" (branch \"{}\"), not \"{}\"",
                path.display(),
                record.task,
                record.branch,
                task
            )),
            None => Err(format!(
                "worktree {} already exists without an isolation record; remove it first (`/worktree rm {}`) then create it again",
                path.display(),
                task
            )),
        };
    }
    let created = create_worktree(cwd, path, branch)?;
    if let Err(e) = create_isolation_record(path, task, branch, session_id) {
        let _ = remove_worktree(cwd, path);
        return Err(e);
    }
    Ok(created)
}

/// Removes a worktree owned by `task`. Refuses when the worktree
/// isolation record names a different task; legacy worktrees without a
/// record may still be removed.
pub fn remove_isolated_worktree(cwd: &Path, path: &Path, task: &str) -> Result<(), String> {
    match read_isolation_record(path) {
        Some(record) if record.task != task => {
            return Err(format!(
                "refusing to remove worktree {}: isolated for task \"{}\", not \"{}\"",
                path.display(),
                record.task,
                task
            ));
        }
        _ => {}
    }
    remove_worktree(cwd, path)
}

pub fn changed_files(cwd: &Path) -> Vec<(String, String)> {
    let p = spawn_probe("git", cwd, &["status", "--porcelain"]);
    // A timed-out probe reports no changes rather than erroring: callers
    // treat this as "nothing changed", so a slow repository can silently
    // skip scope and commit-message checks that depend on this list.
    let out = match finish_probe(p, Instant::now() + GIT_TIMEOUT) {
        Some(o) => o,
        None => return vec![],
    };
    out.lines()
        .filter_map(|l| {
            if l.len() < 4 {
                return None;
            }
            let status = l[..2].to_string();
            let path = l[3..].trim().to_string();

            let path = path.split(" -> ").last().unwrap_or(&path).to_string();
            Some((status, path))
        })
        .collect()
}

/// Stages everything in the working tree (`git add -A`), not a file list:
/// callers must ensure the cwd contains only what should be committed,
/// since untracked and unrelated changes are committed together.
pub fn stage_and_commit(cwd: &Path, message: &str) -> Result<String, String> {
    let add = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["add", "-A"])
        .output()
        .map_err(|e| format!("failed to run git add: {}", e))?;
    if !add.status.success() {
        return Err(String::from_utf8_lossy(&add.stderr).trim().to_string());
    }
    let commit = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["commit", "-m", message])
        .output()
        .map_err(|e| format!("failed to run git commit: {}", e))?;
    if commit.status.success() {
        Ok(String::from_utf8_lossy(&commit.stdout)
            .lines()
            .next()
            .unwrap_or("committed")
            .to_string())
    } else {
        Err(String::from_utf8_lossy(&commit.stderr).trim().to_string())
    }
}

pub fn suggest_commit_message(files: &[(String, String)]) -> String {
    if files.is_empty() {
        return "chore: update files".to_string();
    }
    let paths: Vec<&str> = files.iter().map(|(_, p)| p.as_str()).collect();

    let prefix = if paths
        .iter()
        .all(|p| p.ends_with(".md") || p.contains("CHANGELOG"))
    {
        "docs"
    } else if paths
        .iter()
        .all(|p| p.contains("test") || p.contains("spec"))
    {
        "test"
    } else if paths
        .iter()
        .any(|p| p.contains("Cargo.toml") || p.contains("package.json"))
    {
        "chore"
    } else if paths
        .iter()
        .any(|p| p.ends_with(".rs") || p.ends_with(".ts") || p.ends_with(".py"))
    {
        let added = files
            .iter()
            .any(|(s, _)| s.trim_start().starts_with('A') || s == "??");
        if added { "feat" } else { "fix" }
    } else {
        "chore"
    };

    let scope = if paths.len() == 1 {
        paths[0].to_string()
    } else {
        let dirs: Vec<&str> = paths.iter().filter_map(|p| p.split('/').next()).collect();
        let first = dirs.first().copied().unwrap_or("");
        if !first.is_empty() && dirs.iter().all(|d| *d == first) {
            format!("{} ({} files)", first, paths.len())
        } else {
            format!("{} files", paths.len())
        }
    };

    format!("{}: update {}", prefix, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_porcelain_keeps_branch_with_its_path() {
        let out = "worktree /r\nHEAD 1111111aaaa\nbranch refs/heads/main\n\nworktree /r-x\nHEAD 2222222bbbb\ndetached\n";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], ("/r".into(), "main".into(), "1111111aaaa".into()));
        assert_eq!(
            v[1],
            ("/r-x".into(), "detached".into(), "2222222bbbb".into())
        );
    }

    #[test]
    fn worktree_names_reject_options_and_traversal() {
        assert!(valid_worktree_name("feature-1"));
        assert!(!valid_worktree_name("-b"));
        assert!(!valid_worktree_name("../x"));
        assert!(!valid_worktree_name("a/b"));
        assert!(!valid_worktree_name(""));
    }
    use std::path::PathBuf;

    fn init_repo() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("zen_git_diff_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).ok()?;
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q", "-b", "main"]) {
            return None;
        }
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        std::fs::write(dir.join("a.txt"), "one\n").ok()?;
        git(&["add", "a.txt"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "add a",
        ]);
        Some(dir)
    }

    #[test]
    fn os_info_nonempty() {
        assert!(!os_info().is_empty());
    }

    #[test]
    fn branch_in_non_repo_is_none_or_some() {
        let _ = current_branch(&std::env::temp_dir());
    }

    #[test]
    fn status_summary_no_panic() {
        let _ = status_summary(&std::env::temp_dir());
    }

    #[test]
    fn p0_3_missing_git_gives_no_info() {
        let (b, s) = git_info_with("zen-no-such-git-binary", &std::env::temp_dir(), GIT_TIMEOUT);
        assert_eq!((b, s), (None, None));
    }

    #[test]
    fn suggest_commit_message_empty() {
        assert_eq!(suggest_commit_message(&[]), "chore: update files");
    }

    #[test]
    fn suggest_commit_message_single_rs() {
        let files = vec![(" M".to_string(), "crates/zen-cli/src/git.rs".to_string())];
        assert_eq!(
            suggest_commit_message(&files),
            "fix: update crates/zen-cli/src/git.rs"
        );
    }

    #[test]
    fn suggest_commit_message_new_rs_is_feat() {
        let files = vec![("??".to_string(), "crates/zen-cli/src/new.rs".to_string())];
        assert_eq!(
            suggest_commit_message(&files),
            "feat: update crates/zen-cli/src/new.rs"
        );
    }

    #[test]
    fn suggest_commit_message_docs() {
        let files = vec![
            (" M".to_string(), "README.md".to_string()),
            (" M".to_string(), "docs/guide.md".to_string()),
        ];
        assert_eq!(suggest_commit_message(&files), "docs: update 2 files");
    }

    #[test]
    fn suggest_commit_message_same_dir() {
        let files = vec![
            (" M".to_string(), "crates/zen-cli/src/a.rs".to_string()),
            (" M".to_string(), "crates/zen-cli/src/b.rs".to_string()),
        ];
        assert_eq!(
            suggest_commit_message(&files),
            "fix: update crates (2 files)"
        );
    }

    #[test]
    fn changed_files_no_panic_outside_repo() {
        let _ = changed_files(&std::env::temp_dir());
    }

    #[cfg(unix)]
    #[test]
    fn p0_3_hung_git_is_killed_at_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("zen_git_slow_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("git");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t0 = Instant::now();
        let (b, s) = git_info_with(fake.to_str().unwrap(), &dir, Duration::from_millis(300));
        let took = t0.elapsed();
        assert_eq!((b, s), (None, None));

        assert!(took < Duration::from_millis(1500), "took {:?}", took);
    }

    #[test]
    fn p0_3_real_repo_reports_branch_and_status() {
        let dir = std::env::temp_dir().join(format!("zen_git_repo_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = Command::new("git")
            .args(["init", "-q", "-b", "p0main"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        let (_, status) = git_info(&dir);
        assert_eq!(status.as_deref(), Some("clean"));
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let (_, status) = git_info(&dir);
        assert_eq!(status.as_deref(), Some("1 changed file(s)"));
    }

    #[test]
    fn is_git_repo_false_outside_repo() {
        assert!(!is_git_repo(&std::env::temp_dir()));
    }

    #[test]
    fn diff_clean_repo_is_empty() {
        let Some(dir) = init_repo() else { return };
        assert!(is_git_repo(&dir));
        let d = git_diff(&dir, DiffMode::WorkingTree, false).unwrap();
        assert!(d.trim().is_empty(), "clean repo should have empty diff");
    }

    #[test]
    fn diff_working_tree_shows_unstaged_change() {
        let Some(dir) = init_repo() else { return };
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let d = git_diff(&dir, DiffMode::WorkingTree, false).expect("diff should run");
        assert!(
            d.contains("diff --git a/a.txt b/a.txt"),
            "diff header missing:\n{d}"
        );
        assert!(d.contains("+two"), "added line missing:\n{d}");

        let s = git_diff(&dir, DiffMode::Staged, false).expect("staged diff should run");
        assert!(s.trim().is_empty(), "staged diff should be empty:\n{s}");
    }

    #[test]
    fn diff_staged_shows_staged_change() {
        let Some(dir) = init_repo() else { return };
        std::fs::write(dir.join("a.txt"), "one\nstaged-line\n").unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
        };
        git(&["add", "a.txt"]).ok();
        let s = git_diff(&dir, DiffMode::Staged, false).expect("staged diff should run");
        assert!(s.contains("+staged-line"), "staged line missing:\n{s}");
    }

    #[test]
    fn diff_stat_summarizes() {
        let Some(dir) = init_repo() else { return };
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let st = git_diff(&dir, DiffMode::Stat, false).expect("stat should run");
        assert!(st.contains("a.txt"), "stat should name the file:\n{st}");
        assert!(st.contains("1 file changed"), "stat summary missing:\n{st}");

        assert!(!st.contains("@@"), "stat should not contain hunks:\n{st}");
    }

    #[test]
    fn untracked_files_lists_new_files() {
        let Some(dir) = init_repo() else { return };
        assert!(untracked_files(&dir).expect("status should run").is_empty());
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();
        let u = untracked_files(&dir).expect("status should run");
        assert_eq!(u, vec!["new.txt".to_string()]);
    }

    #[test]
    fn untracked_files_none_outside_repo() {
        assert!(
            untracked_files(&std::env::temp_dir()).is_none()
                || untracked_files(&std::env::temp_dir()).unwrap().is_empty()
        );
    }

    #[test]
    fn parse_untracked_handles_quoted_paths() {
        let out = "?? plain.txt\n?? \"with space.txt\"\nM  staged.txt\n";
        assert_eq!(
            parse_untracked(out),
            vec!["plain.txt".to_string(), "with space.txt".to_string()]
        );
    }

    #[test]
    fn parse_untracked_decodes_git_octal_escapes() {
        let out = "?? plain.txt\n?? \"caf\\303\\251.txt\"\n";
        assert_eq!(
            parse_untracked(out),
            vec!["plain.txt".to_string(), "café.txt".to_string()]
        );
    }

    #[test]
    fn diff_color_flag_adds_ansi() {
        let Some(dir) = init_repo() else { return };
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let plain = git_diff(&dir, DiffMode::WorkingTree, false).unwrap();
        assert!(
            !plain.contains("\x1b["),
            "plain diff must not have ANSI codes"
        );
        let colored = git_diff(&dir, DiffMode::WorkingTree, true).unwrap();
        assert!(
            colored.contains("\x1b["),
            "colored diff should have ANSI codes"
        );
    }

    fn wt_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("zen_git_wt_{}_{}", uuid::Uuid::new_v4(), name))
    }

    #[test]
    fn isolated_create_writes_record_and_reattaches() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat1");
        let created =
            create_isolated_worktree(&dir, &wt, "feat1", "feat1", "sess-1").expect("create");
        assert_eq!(created, wt);
        let record = read_isolation_record(&wt).expect("record written");
        assert_eq!(record.task, "feat1");
        assert_eq!(record.branch, "feat1");
        assert_eq!(record.created_by_session, "sess-1");
        assert!(is_registered_worktree(&dir, &wt));
        // Same task re-attaching is idempotent and keeps the first record.
        let again =
            create_isolated_worktree(&dir, &wt, "feat1", "feat1", "sess-2").expect("re-attach");
        assert_eq!(again, wt);
        assert_eq!(
            read_isolation_record(&wt)
                .expect("record kept")
                .created_by_session,
            "sess-1"
        );
        // Windows: `git worktree list --porcelain` prints the path in
        // git's own form (forward slashes, resolved case), which does
        // not compare equal to the temp_dir-built path as raw `Path`s
        // (Windows job log run 44928292749: count 0 vs 1, while
        // `is_registered_worktree` on the same list passed). Compare
        // canonicalized paths, like `is_registered_worktree` does.
        let want = canonical_or_raw(&wt);
        assert_eq!(
            list_worktrees(&dir)
                .iter()
                .filter(|(p, _, _)| canonical_or_raw(Path::new(p)) == want)
                .count(),
            1
        );
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolated_create_refuses_other_task() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat1");
        create_isolated_worktree(&dir, &wt, "feat1", "feat1", "sess-1").expect("create");
        let err = create_isolated_worktree(&dir, &wt, "feat1", "other-task", "sess-1").unwrap_err();
        assert!(
            err.contains("feat1"),
            "refusal names the owning task: {err}"
        );
        // Same task but a different branch name is also refused.
        let err =
            create_isolated_worktree(&dir, &wt, "other-branch", "feat1", "sess-1").unwrap_err();
        assert!(
            err.contains("feat1"),
            "refusal names the owning task: {err}"
        );
        assert!(is_registered_worktree(&dir, &wt));
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #11: a single invalid name must be rejected even when the other
    // name is valid. Replacing `||` with `&&` in the validation would
    // only reject when both names are invalid. The `feat/sub` and
    // `bad/task` cases alone do NOT kill that mutant: the downstream
    // create_isolation_record repeats the same validation with the
    // identical message, and rolls the worktree back, so the error
    // looks the same. Two observable differences do kill it:
    // (1) an empty branch is invalid for git too, so the mutant leaks
    // raw git stderr instead of the "invalid worktree name" message;
    // (2) a rejected create must leave no branch behind, but the
    // mutant creates the branch first and `git worktree remove` does
    // not delete it, so the branch survives the rollback.
    #[test]
    fn isolated_create_rejects_single_invalid_name() {
        let Some(dir) = init_repo() else { return };
        let branch_exists = |dir: &Path, branch: &str| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // Invalid branch, valid task: git accepts `feat/sub` as a branch
        // name, so the mutant would create it and leave it behind.
        let wt = wt_path("feat-sub");
        let err = create_isolated_worktree(&dir, &wt, "feat/sub", "feat1", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "invalid branch must be rejected: {err}"
        );
        assert!(
            !branch_exists(&dir, "feat/sub"),
            "rejected create must not leave branch feat/sub behind"
        );
        // Valid branch, invalid task: the mutant would create branch
        // `feat1` before the record write refuses the task.
        let wt = wt_path("bad-task");
        let err = create_isolated_worktree(&dir, &wt, "feat1", "bad/task", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "invalid task must be rejected: {err}"
        );
        assert!(
            !branch_exists(&dir, "feat1"),
            "rejected create must not leave branch feat1 behind"
        );
        // Empty branch: invalid for git as well, so the mutant leaks raw
        // git stderr instead of the validation message.
        let wt = wt_path("empty-branch");
        let err = create_isolated_worktree(&dir, &wt, "", "feat1", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "empty branch must be rejected with the validation error: {err}"
        );
        // Both invalid is rejected too (control case).
        let wt = wt_path("both-bad");
        let err = create_isolated_worktree(&dir, &wt, "a/b", "c/d", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "both invalid must be rejected: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #11: create_isolation_record validates both names itself. The
    // `||` in its guard must reject when either name alone is invalid;
    // replacing it with `&&` would only reject when both are invalid
    // and would write a record carrying the bad name. These tests call
    // it directly: create_isolated_worktree validates first, so its own
    // guard would mask this one.
    #[test]
    fn isolation_record_rejects_invalid_branch() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat-bad-branch");
        create_worktree(&dir, &wt, "feat1").expect("plain create");
        let err = create_isolation_record(&wt, "feat1", "bad branch", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "invalid branch must be rejected: {err}"
        );
        assert!(
            read_isolation_record(&wt).is_none(),
            "rejected record write must not leave a record behind"
        );
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolation_record_rejects_invalid_task() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("bad-task");
        create_worktree(&dir, &wt, "feat1").expect("plain create");
        let err = create_isolation_record(&wt, "bad task", "feat1", "sess-1").unwrap_err();
        assert!(
            err.contains("invalid worktree name"),
            "invalid task must be rejected: {err}"
        );
        assert!(
            read_isolation_record(&wt).is_none(),
            "rejected record write must not leave a record behind"
        );
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolated_create_refuses_legacy_worktree() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("legacy1");
        create_worktree(&dir, &wt, "legacy1").expect("plain create");
        assert!(read_isolation_record(&wt).is_none());
        let err = create_isolated_worktree(&dir, &wt, "legacy1", "legacy1", "sess-1").unwrap_err();
        assert!(
            err.contains("without an isolation record"),
            "legacy refusal explains: {err}"
        );
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolated_remove_refuses_other_task_and_allows_owner() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat2");
        create_isolated_worktree(&dir, &wt, "feat2", "feat2", "sess-1").expect("create");
        let err = remove_isolated_worktree(&dir, &wt, "someone-else").unwrap_err();
        assert!(err.contains("feat2"), "refusal names the owner: {err}");
        assert!(is_registered_worktree(&dir, &wt));
        remove_isolated_worktree(&dir, &wt, "feat2").expect("owner removes");
        assert!(!is_registered_worktree(&dir, &wt));
        assert!(read_isolation_record(&wt).is_none());
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolated_create_rejects_bad_names() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("bad");
        assert!(create_isolated_worktree(&dir, &wt, "-b", "-b", "s").is_err());
        assert!(create_isolated_worktree(&dir, &wt, "ok", "../escape", "s").is_err());
        assert!(!wt.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolation_record_malformed_is_legacy() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat3");
        create_isolated_worktree(&dir, &wt, "feat3", "feat3", "sess-1").expect("create");
        let record_path = isolation_record_path(&wt).expect("gitdir resolves");
        std::fs::write(&record_path, "not json{").unwrap();
        assert!(read_isolation_record(&wt).is_none());
        std::fs::write(&record_path, r#"{"task": "feat3"}"#).unwrap();
        assert!(read_isolation_record(&wt).is_none());
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #11: the isolation record must live in the worktree's gitdir,
    // not in the process working directory. A `git_dir_of` mutant
    // returning a default path would read/write a stray
    // `zen-isolation.json` in the CWD instead.
    #[test]
    fn isolation_record_lives_in_worktree_gitdir() {
        let Some(dir) = init_repo() else { return };
        let wt = wt_path("feat4");
        create_isolated_worktree(&dir, &wt, "feat4", "feat4", "sess-1").expect("create");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse succeeds");
        let gitdir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let record_path = isolation_record_path(&wt).expect("record path resolves");
        assert!(
            record_path.is_absolute(),
            "record path is absolute: {}",
            record_path.display()
        );
        assert!(
            record_path.starts_with(&gitdir),
            "record {} is inside gitdir {gitdir}",
            record_path.display()
        );
        assert!(record_path.is_file(), "record file exists");
        let _ = remove_worktree(&dir, &wt);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unregistered_path_is_not_a_worktree() {
        let Some(dir) = init_repo() else { return };
        assert!(!is_registered_worktree(&dir, &dir.join("no-such-wt")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
