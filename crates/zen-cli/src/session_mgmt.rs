use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const ARCHIVE_DIR_NAME: &str = "archive";

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,

    pub path: PathBuf,

    pub archived: bool,

    pub modified: SystemTime,

    pub message_count: usize,

    pub input_tokens: u64,
    pub output_tokens: u64,

    pub title: Option<String>,
}

pub fn archive_dir(proj_dir: &Path) -> PathBuf {
    proj_dir.join(ARCHIVE_DIR_NAME)
}

pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

pub fn fmt_mtime(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

pub fn session_info(path: &Path, archived: bool) -> SessionInfo {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let (message_count, input_tokens, output_tokens) = scan_transcript(path);
    SessionInfo {
        id,
        path: path.to_path_buf(),
        archived,
        modified: std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH),
        message_count,
        input_tokens,
        output_tokens,
        title: crate::session::session_title(path),
    }
}

fn scan_transcript(path: &Path) -> (usize, u64, u64) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (0, 0, 0);
    };
    let mut count = 0usize;
    let mut input = 0u64;
    let mut output = 0u64;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        count += 1;
        if v.get("subtype").and_then(|t| t.as_str()) == Some("usage") {
            input += v["usage"]["input_tokens"].as_u64().unwrap_or(0);
            output += v["usage"]["output_tokens"].as_u64().unwrap_or(0);
        }
    }
    (count, input, output)
}

pub fn list_all_sessions(home: &Path, include_archived: bool) -> Vec<SessionInfo> {
    let projects_root = home.join(".zen").join("projects");
    if !projects_root.exists() {
        return vec![];
    }
    let mut out: Vec<SessionInfo> = vec![];
    let Ok(proj_dirs) = std::fs::read_dir(&projects_root) else {
        return vec![];
    };
    for proj in proj_dirs.flatten() {
        let proj_path = proj.path();
        if !proj_path.is_dir() {
            continue;
        }
        collect_dir(&proj_path, false, &mut out);
        if include_archived {
            let ad = archive_dir(&proj_path);
            if ad.is_dir() {
                collect_dir(&ad, true, &mut out);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

fn collect_dir(dir: &Path, archived: bool, out: &mut Vec<SessionInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(session_info(&p, archived));
        }
    }
}

/// True when `path` (which must exist) resolves inside the managed
/// session tree (`home/.zen/projects`, archive included). Both sides
/// are canonicalized first, so a `..` escape or a symlink pointing
/// outside cannot pass a lexical prefix check (issue #454). Fails
/// closed: anything that cannot be canonicalized is not managed.
fn is_managed_session_path(home: &Path, path: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(home.join(".zen").join("projects")) else {
        return false;
    };
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    canon.starts_with(&root)
}

pub fn find_managed_session(home: &Path, id_or_path: &str) -> Option<(PathBuf, bool)> {
    let p = PathBuf::from(id_or_path);
    if p.is_absolute() && p.exists() {
        // Issue #454: only a path inside the managed session tree is
        // a session. An existing path outside it is not found, never
        // deleted, archived or restored.
        let canon = std::fs::canonicalize(&p).ok()?;
        if is_managed_session_path(home, &canon) {
            return Some((canon.clone(), is_archived_path(home, &canon)));
        }
        return None;
    }
    if p.exists()
        && let Ok(abs) = p.canonicalize()
        && is_managed_session_path(home, &abs)
    {
        return Some((abs.clone(), is_archived_path(home, &abs)));
    }
    for info in list_all_sessions(home, true) {
        if info.id == id_or_path {
            return Some((info.path, info.archived));
        }
    }
    None
}

fn is_archived_path(home: &Path, path: &Path) -> bool {
    // Both sides canonical: the caller passes a canonical path (see
    // find_managed_session), and the root must be canonicalized the
    // same way or a symlinked home (macOS /tmp) breaks the check.
    let Ok(root) = std::fs::canonicalize(home.join(".zen").join("projects")) else {
        return false;
    };
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    canon
        .strip_prefix(&root)
        .ok()
        .and_then(|rel| rel.components().nth(1))
        .and_then(|c| c.as_os_str().to_str())
        == Some(ARCHIVE_DIR_NAME)
}

pub fn archive_session(home: &Path, id: &str) -> Result<PathBuf> {
    let (path, archived) = find_managed_session(home, id).ok_or_else(|| {
        crate::coded_error(
            crate::exit_code::NOT_FOUND,
            format!("no session found: {}", id),
        )
    })?;
    if archived {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!("session {} is already archived", short_id(id)),
        ));
    }
    let proj = path
        .parent()
        .ok_or_else(|| anyhow!("cannot determine project dir for {}", path.display()))?;
    let ad = archive_dir(proj);
    std::fs::create_dir_all(&ad)?;
    let dest = ad.join(
        path.file_name()
            .ok_or_else(|| anyhow!("bad session path"))?,
    );
    if dest.exists() {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!(
                "session {} already exists in the archive; restore or delete it first",
                short_id(id)
            ),
        ));
    }
    move_session_files(&path, &dest)?;
    Ok(dest)
}

pub fn restore_session(home: &Path, id: &str) -> Result<PathBuf> {
    let (path, archived) = find_managed_session(home, id).ok_or_else(|| {
        crate::coded_error(
            crate::exit_code::NOT_FOUND,
            format!("no session found: {}", id),
        )
    })?;
    if !archived {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!(
                "session {} is not archived (nothing to restore)",
                short_id(id)
            ),
        ));
    }
    let archive = path
        .parent()
        .ok_or_else(|| anyhow!("cannot determine archive dir for {}", path.display()))?;
    let proj = archive
        .parent()
        .ok_or_else(|| anyhow!("cannot determine project dir for {}", path.display()))?;
    let dest = proj.join(
        path.file_name()
            .ok_or_else(|| anyhow!("bad session path"))?,
    );
    if dest.exists() {
        return Err(crate::coded_error(
            crate::exit_code::CONFLICT,
            format!(
                "an active session {} already exists; delete it first",
                short_id(id)
            ),
        ));
    }
    move_session_files(&path, &dest)?;
    Ok(dest)
}

pub fn delete_session(home: &Path, id: &str) -> Result<()> {
    let (path, _) = find_managed_session(home, id).ok_or_else(|| {
        crate::coded_error(
            crate::exit_code::NOT_FOUND,
            format!("no session found: {}", id),
        )
    })?;

    let scratch = path.with_extension("");
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    // Issue #454: the stem-sibling scratch is only removed when it
    // resolves inside the managed tree - never follow a scratch
    // symlink (or a `..` escape) outside it.
    if scratch.is_dir() && is_managed_session_path(home, &scratch) {
        std::fs::remove_dir_all(&scratch)?;
    }
    Ok(())
}

fn move_session_files(src: &Path, dst: &Path) -> Result<()> {
    std::fs::rename(src, dst).map_err(|e| {
        anyhow!(
            "failed to move {} to {}: {}",
            src.display(),
            dst.display(),
            e
        )
    })?;
    let src_scratch = src.with_extension("");
    if src_scratch.is_dir() {
        let dst_scratch = dst.with_extension("");

        if let Err(e) = std::fs::rename(&src_scratch, &dst_scratch) {
            eprintln!(
                "warning: archived the transcript but could not move scratch dir {}: {}",
                src_scratch.display(),
                e
            );
        }
    }
    Ok(())
}

pub fn auto_archive_old(home: &Path, days: u64) -> Result<Vec<String>> {
    if days == 0 {
        return Ok(vec![]);
    }
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(days.saturating_mul(86400)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut archived = vec![];
    for info in list_all_sessions(home, false) {
        if info.modified < cutoff {
            match archive_session(home, &info.id) {
                Ok(_) => archived.push(info.id),
                Err(e) => eprintln!(
                    "warning: auto-archive of {} failed: {}",
                    short_id(&info.id),
                    e
                ),
            }
        }
    }
    Ok(archived)
}

pub fn parse_confirmation_answer(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn confirm_delete_prompt(id: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(crate::coded_error(
            crate::exit_code::FORBIDDEN,
            "refusing to delete session without --yes on non-interactive stdin",
        ));
    }
    eprint!(
        "Permanently delete session {}? This cannot be undone. [y/N] ",
        short_id(id)
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(parse_confirmation_answer(&answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home(tag: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("zen_sm_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&h);
        h
    }

    fn make_session(home: &Path, cwd: &Path, id: &str) -> PathBuf {
        let path = crate::session::ensure_session_path(home, cwd, id).unwrap();
        for i in 0..3 {
            crate::session::append_typed(
                &path,
                "user",
                id,
                cwd.to_str().unwrap(),
                &format!("u{}", i),
                None,
                Some(serde_json::json!({"role":"user","content":"hi"})),
                None,
            )
            .unwrap();
        }

        crate::session::append_usage(
            &path,
            id,
            &zen_core::UsageStats {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        )
        .unwrap();
        path
    }

    #[test]
    fn archive_restore_roundtrip() {
        let home = tmp_home("rt");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let path = make_session(&home, &cwd, "sess-rt");

        let dest = archive_session(&home, "sess-rt").unwrap();
        assert!(!path.exists());
        assert!(dest.exists());
        assert!(dest.to_string_lossy().contains("archive"));

        assert!(list_all_sessions(&home, false).is_empty());
        let all = list_all_sessions(&home, true);
        assert_eq!(all.len(), 1);
        assert!(all[0].archived);
        assert_eq!(all[0].message_count, 4);
        assert_eq!(all[0].input_tokens, 100);

        let err = archive_session(&home, "sess-rt").unwrap_err();
        assert_eq!(
            crate::exit_code_for_error(&err),
            crate::exit_code::CONFLICT,
            "archiving an archived session must exit CONFLICT (B2 code 7): {err}"
        );

        let back = restore_session(&home, "sess-rt").unwrap();
        assert_eq!(back, path);
        assert!(path.exists());
        let listed = list_all_sessions(&home, false);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].archived);

        assert!(restore_session(&home, "sess-rt").is_err());
    }

    #[test]
    fn delete_removes_transcript_and_scratch() {
        let home = tmp_home("del");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let path = make_session(&home, &cwd, "sess-del");

        let scratch = path.with_extension("");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("spill.txt"), "x").unwrap();

        delete_session(&home, "sess-del").unwrap();
        assert!(!path.exists());
        assert!(!scratch.exists());
        assert!(list_all_sessions(&home, true).is_empty());

        assert!(delete_session(&home, "nope").is_err());
    }

    #[test]
    fn delete_archived_session() {
        let home = tmp_home("delarch");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        make_session(&home, &cwd, "sess-da");
        archive_session(&home, "sess-da").unwrap();
        delete_session(&home, "sess-da").unwrap();
        assert!(list_all_sessions(&home, true).is_empty());
    }

    #[test]
    fn auto_archive_old_only_archives_stale() {
        let home = tmp_home("auto");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let old_path = make_session(&home, &cwd, "sess-old");
        make_session(&home, &cwd, "sess-new");

        let ten_days = std::time::Duration::from_secs(10 * 86400);
        let old_mtime = SystemTime::now().checked_sub(ten_days).unwrap();
        let f = fs::File::options().write(true).open(&old_path).unwrap();
        f.set_modified(old_mtime).unwrap();

        let archived = auto_archive_old(&home, 7).unwrap();
        assert_eq!(archived, vec!["sess-old".to_string()]);

        let active = list_all_sessions(&home, false);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "sess-new");

        let none = auto_archive_old(&home, 0).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn find_managed_session_reports_archived_flag() {
        let home = tmp_home("find");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        make_session(&home, &cwd, "sess-f");
        let (p, arch) = find_managed_session(&home, "sess-f").unwrap();
        assert!(!arch);
        assert!(p.exists());
        archive_session(&home, "sess-f").unwrap();
        let (_, arch2) = find_managed_session(&home, "sess-f").unwrap();
        assert!(arch2);
        assert!(find_managed_session(&home, "missing").is_none());
    }

    #[test]
    fn confirmation_answer_parsing() {
        assert!(parse_confirmation_answer("y"));
        assert!(parse_confirmation_answer("Y"));
        assert!(parse_confirmation_answer("yes"));
        assert!(parse_confirmation_answer("  YES  "));
        assert!(!parse_confirmation_answer("n"));
        assert!(!parse_confirmation_answer(""));
        assert!(!parse_confirmation_answer("yeah"));
    }

    #[test]
    fn short_id_and_fmt_mtime() {
        assert_eq!(short_id("abcdef123456"), "abcdef12");
        assert_eq!(short_id("abc"), "abc");
        let s = fmt_mtime(SystemTime::UNIX_EPOCH);
        assert_eq!(s, "1970-01-01 00:00");
    }

    #[test]
    fn archive_moves_scratch_dir_too() {
        let home = tmp_home("scratch");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let path = make_session(&home, &cwd, "sess-s");
        let scratch = path.with_extension("");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("log.txt"), "x").unwrap();

        let dest = archive_session(&home, "sess-s").unwrap();
        assert!(!scratch.exists());
        assert!(dest.with_extension("").join("log.txt").exists());

        restore_session(&home, "sess-s").unwrap();
        assert!(scratch.join("log.txt").exists());
    }

    #[test]
    fn issue_454_delete_outside_absolute_path_refused() {
        // Issue #454: `zen session delete /home/user/notes.jsonl`
        // must not delete an arbitrary file plus its stem-sibling
        // directory - only paths under the managed session tree are
        // sessions. Archive must refuse the same path too.
        let home = tmp_home("454out");
        fs::create_dir_all(&home).unwrap();
        let victim = home.join("notes.jsonl");
        let sibling = home.join("notes");
        fs::write(&victim, "keep me").unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("marker.txt"), "keep me too").unwrap();

        assert!(find_managed_session(&home, victim.to_str().unwrap()).is_none());
        assert!(delete_session(&home, victim.to_str().unwrap()).is_err());
        assert!(archive_session(&home, victim.to_str().unwrap()).is_err());
        assert!(
            victim.exists(),
            "outside file must survive a refused delete"
        );
        assert!(
            sibling.join("marker.txt").exists(),
            "stem-sibling dir must survive a refused delete"
        );
    }

    #[test]
    fn issue_454_dotdot_path_escape_refused() {
        // A path that lexically starts with the managed tree but
        // resolves outside it (`projects/../../victim.jsonl`) must be
        // refused: containment is checked after canonicalization.
        let home = tmp_home("454dot");
        fs::create_dir_all(&home).unwrap();
        let victim = home.join("victim.jsonl");
        fs::write(&victim, "keep me").unwrap();
        // The `..` escape only resolves through existing directories.
        fs::create_dir_all(home.join(".zen").join("projects")).unwrap();
        let escape = home
            .join(".zen")
            .join("projects")
            .join("..")
            .join("..")
            .join("victim.jsonl");
        assert!(escape.exists(), "escape path must resolve to the victim");
        assert!(find_managed_session(&home, escape.to_str().unwrap()).is_none());
        assert!(delete_session(&home, escape.to_str().unwrap()).is_err());
        assert!(
            victim.exists(),
            "outside file must survive a refused delete"
        );
    }

    #[test]
    #[cfg(unix)]
    fn issue_454_symlink_inside_tree_pointing_outside_refused() {
        // A symlink planted inside the tree pointing outside must not
        // make the outside file a "managed session".
        let home = tmp_home("454sym");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let proj = crate::session::ensure_session_dir(&home, &cwd, "sess-sym").unwrap();
        let victim = home.join("victim.jsonl");
        fs::write(&victim, "keep me").unwrap();
        let link = proj.join("evil.jsonl");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(find_managed_session(&home, link.to_str().unwrap()).is_none());
        assert!(delete_session(&home, link.to_str().unwrap()).is_err());
        assert!(
            victim.exists(),
            "outside file must survive a refused delete"
        );
    }

    #[test]
    #[cfg(unix)]
    fn issue_454_scratch_symlink_not_followed() {
        // The stem-sibling scratch of a real session must not be
        // followed when it is a symlink: deleting the session removes
        // the transcript, never the outside directory it points at.
        let home = tmp_home("454scr");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let path = make_session(&home, &cwd, "sess-scr");
        let scratch = path.with_extension("");
        let outside = home.join("outside-scratch");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("marker.txt"), "keep me").unwrap();
        std::os::unix::fs::symlink(&outside, &scratch).unwrap();

        delete_session(&home, "sess-scr").unwrap();
        assert!(!path.exists(), "transcript must be deleted");
        assert!(
            outside.join("marker.txt").exists(),
            "outside dir behind a scratch symlink must survive"
        );
    }

    #[test]
    fn issue_454_scratch_file_not_removed_on_delete() {
        // A stem-sibling scratch that is a regular file (not a
        // directory) inside the managed tree must not make the delete
        // fail: the guard only removes scratch *directories* inside
        // the tree. Replacing `&&` with `||` in the guard calls
        // remove_dir_all on the file and delete_session errors out.
        let home = tmp_home("454scrf");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let path = make_session(&home, &cwd, "sess-sf");
        let scratch = path.with_extension("");
        fs::write(&scratch, "not a dir").unwrap();

        delete_session(&home, "sess-sf").unwrap();
        assert!(!path.exists(), "transcript must be deleted");
        assert!(
            scratch.exists(),
            "scratch file must survive: only scratch dirs are removed"
        );
        assert_eq!(fs::read_to_string(&scratch).unwrap(), "not a dir");
    }

    #[test]
    fn issue_454_relative_path_must_not_skip_session_id_lookup() {
        // A relative path that exists in the test process working
        // directory (the crate root, where Cargo.toml lives) must not
        // short-circuit the session-id lookup: find_managed_session
        // only treats absolute paths as paths, and a relative string
        // still falls through to the id loop. Replacing `&&` with
        // `||` in the absolute-path guard enters that branch for the
        // relative path and returns None before the id loop runs.
        assert!(
            std::path::Path::new("Cargo.toml").exists(),
            "test assumes cargo runs unit tests with the crate root as CWD"
        );
        let home = tmp_home("454rel");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        make_session(&home, &cwd, "Cargo.toml");

        let found = find_managed_session(&home, "Cargo.toml");
        assert!(found.is_some(), "session id must still resolve");
    }

    #[test]
    fn issue_454_archived_flag_correct_for_path_lookup() {
        // is_archived_path is only consulted on the path branches of
        // find_managed_session: the id loop reports the flag from
        // list_all_sessions instead. So a path lookup must return
        // archived=false for an active session and archived=true for
        // an archived one. Replacing the function body with `true`
        // or `false` must fail this test.
        let home = tmp_home("454arc");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let active = make_session(&home, &cwd, "sess-act");
        make_session(&home, &cwd, "sess-arch");
        let archived_path = archive_session(&home, "sess-arch").unwrap();

        let (_, active_archived) = find_managed_session(&home, active.to_str().unwrap()).unwrap();
        assert!(
            !active_archived,
            "active session found by path must not be archived"
        );
        let (_, archived_archived) =
            find_managed_session(&home, archived_path.to_str().unwrap()).unwrap();
        assert!(
            archived_archived,
            "archived session found by path must be archived"
        );
    }
}
