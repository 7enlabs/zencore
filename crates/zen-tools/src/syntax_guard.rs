use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

// 30s: on a loaded Windows CI runner `node --check` can take over 10s.
// A timeout used to silently pass broken code (main run 44441607802); the
// fix then was a longer timeout, but the silent pass remained. Since #325
// the guard is fail-closed: a checker that cannot run refuses the write
// instead of reporting a clean pass, so the timeout only buys room.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of a syntax check: `Ok(None)` means the check ran and passed,
/// `Ok(Some(msg))` means it ran and found a syntax error, `Err(reason)`
/// means the check could not run at all (checker binary missing, crashed,
/// or timed out). The `Err` case must never be treated as a clean pass.
pub async fn syntax_error(path: &Path, text: &str) -> Result<Option<String>, String> {
    let Some(lang) = lang_of(path) else {
        return Ok(None);
    };
    match lang {
        Lang::Json => Ok(
            serde_json::from_str::<serde_json::Value>(text)
                .err()
                .map(|e| e.to_string()),
        ),
        Lang::Python => {
            run_checker(
                "python3",
                &["-c", "import ast,sys\nsrc=sys.stdin.read()\ntry:\n ast.parse(src)\nexcept SyntaxError as e:\n print('line %s: %s' % (e.lineno, e.msg)); sys.exit(1)"],
                text,
            )
            .await
        }
        Lang::Shell => {
            let sh = zen_core::resolve_shell("bash");
            if sh.kind != zen_core::ShellKind::Posix {
                return Ok(None);
            }
            run_checker(&sh.program, &["-n"], text)
                .await
                .map(|m| m.map(|m| m.replace("bash: ", "")))
        }
        Lang::JavaScript => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("js");
            let (tmp, mut f) = create_js_temp(ext)?;
            {
                use std::io::Write;
                if let Err(e) = f.write_all(text.as_bytes()) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!(
                        "failed to write syntax-check temp file {}: {e}",
                        tmp.display()
                    ));
                }
            }
            let tmp_str = tmp.to_str().ok_or_else(|| {
                format!(
                    "syntax-check temp path is not valid UTF-8: {}",
                    tmp.display()
                )
            })?;
            let out = run_checker("node", &["--check", tmp_str], "").await;
            let _ = tokio::fs::remove_file(&tmp).await;
            out.map(|m| m.map(|m| m.replace(tmp_str, "file")))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Lang {
    Python,
    JavaScript,
    Json,
    Shell,
}

fn lang_of(path: &Path) -> Option<Lang> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "py" => Some(Lang::Python),
        "js" | "cjs" | "mjs" => Some(Lang::JavaScript),
        "json" => Some(Lang::Json),
        "sh" | "bash" => Some(Lang::Shell),
        _ => None,
    }
}

/// Create the temp file `node --check` reads (#326): the file holds the new
/// content, so it must not be world-readable while the check runs.
/// Returns `Err` when the file cannot be created (#325: fail closed, the
/// caller must not treat a missing temp file as a clean pass).
fn create_js_temp(ext: &str) -> Result<(std::path::PathBuf, std::fs::File), String> {
    let tmp = std::env::temp_dir().join(format!("zen-syntax-{}.{}", random_suffix(), ext));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(&tmp).map_err(|e| {
        format!(
            "failed to create syntax-check temp file {}: {e}",
            tmp.display()
        )
    })?;
    Ok((tmp, f))
}

fn random_suffix() -> String {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 16];
            if f.read_exact(&mut buf).is_ok() {
                return buf.iter().map(|b| format!("{:02x}", b)).collect();
            }
        }
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    // Per-call counter: SystemTime ticks coarsely on Windows (~15.6 ms), so
    // two rapid calls could otherwise hash to the same suffix; the second
    // create_new would then fail and the check would silently be skipped.
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut h = DefaultHasher::new();
    CALLS.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    std::process::id().hash(&mut h);
    std::time::SystemTime::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    let stack_var = 0u8;
    std::ptr::addr_of!(stack_var).hash(&mut h);
    format!(
        "{:016x}{:016x}",
        h.finish(),
        h.finish().wrapping_mul(0x9e3779b97f4a7c15)
    )
}

/// Runs the external checker. `Ok(None)` means it ran and passed,
/// `Ok(Some(msg))` means it ran and found a syntax error, `Err(reason)`
/// means it could not run at all. The `Err` case is a failure of the
/// guard itself and must never be read as "no error found" (#325).
async fn run_checker(cmd: &str, args: &[&str], text: &str) -> Result<Option<String>, String> {
    run_checker_with_timeout(cmd, args, text, CHECK_TIMEOUT).await
}

async fn run_checker_with_timeout(
    cmd: &str,
    args: &[&str],
    text: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let mut child = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start checker '{cmd}': {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("checker '{cmd}' failed while waiting: {e}")),
        Err(_) => {
            return Err(format!(
                "checker '{cmd}' timed out after {}s",
                timeout.as_secs()
            ));
        }
    };
    if out.status.success() {
        return Ok(None);
    }
    let mut msg = String::from_utf8_lossy(&out.stdout).to_string();
    msg.push_str(&String::from_utf8_lossy(&out.stderr));
    let msg = msg.trim().to_string();
    Ok(Some(if msg.is_empty() {
        "syntax check failed".into()
    } else {
        msg.chars().take(1500).collect()
    }))
}

fn error_line(msg: &str) -> Option<usize> {
    let lower = msg.to_ascii_lowercase();
    if let Some(i) = lower.find("line ") {
        let digits: String = lower[i + 5..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse() {
            return Some(n);
        }
    }
    msg.split(':').find_map(|p| p.trim().parse::<usize>().ok())
}

fn refusal(path: &Path, new_text: &str, msg: &str) -> String {
    let mut out = format!(
        "Error: this change was NOT applied because it would leave {} with a syntax error:\n{}\n",
        path.display(),
        msg
    );
    if let Some(line) = error_line(msg) {
        let lines: Vec<&str> = new_text.lines().collect();
        let from = line.saturating_sub(4).max(1);
        let to = (line + 3).min(lines.len());
        if from <= to {
            out.push_str("Your version around that line:\n");
            for n in from..=to {
                out.push_str(&format!(
                    "{:>6}{}\t{}\n",
                    n,
                    if n == line { ">" } else { " " },
                    lines[n - 1]
                ));
            }
        }
    }
    out.push_str("The file on disk is unchanged. Fix the syntax (indentation, brackets, quotes) and edit again.");
    out
}

fn refusal_unavailable(path: &Path, reason: &str) -> String {
    format!(
        "Error: this change was NOT applied because the syntax guard could not run for {}: {}\n\
         The file on disk is unchanged. Install or repair the checker, or set ZEN_NO_SYNTAX_GUARD=1 to bypass the guard, then edit again.",
        path.display(),
        reason
    )
}

pub async fn guard_write(
    path: &Path,
    old_text: Option<&str>,
    new_text: &str,
) -> Result<(), String> {
    if std::env::var_os("ZEN_NO_SYNTAX_GUARD").is_some() {
        return Ok(());
    }
    // Fail closed (#325): a checker that could not run refuses the write
    // instead of silently passing, so the agent knows the guard did not run.
    let msg = match syntax_error(path, new_text).await {
        Ok(Some(msg)) => msg,
        Ok(None) => return Ok(()),
        Err(reason) => return Err(refusal_unavailable(path, &reason)),
    };
    if let Some(old) = old_text {
        // Old text already broken: allow the edit. Old text clean, or its
        // check could not run (cannot prove it was already broken): stay refused.
        if let Ok(Some(_)) = syntax_error(path, old).await {
            return Ok(());
        }
    }
    Err(refusal(path, new_text, &msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(cmd: &str) -> bool {
        std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn python_edit_that_breaks_syntax_is_refused_with_context() {
        if !has("python3") {
            return;
        }
        let p = Path::new("m.py");
        let old = "def f():\n    return 1\n";
        assert!(
            guard_write(p, Some(old), "def f():\n    return 2\n")
                .await
                .is_ok()
        );
        let err = guard_write(p, Some(old), "def f():\n    if x\n        return 2\n")
            .await
            .unwrap_err();
        assert!(
            err.contains("NOT applied") && err.contains("line 2"),
            "{}",
            err
        );
        assert!(err.contains("     2>\t    if x"), "{}", err);

        assert!(guard_write(p, None, "def (:\n").await.is_err());
        assert!(
            guard_write(p, Some("def (:\n"), "def (:\n  x\n")
                .await
                .is_ok()
        );
    }

    #[test]
    fn syntax_guard_temp_suffixes_are_unique_across_rapid_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(random_suffix()));
        }
    }

    #[test]
    #[cfg(unix)]
    fn js_temp_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _f) = create_js_temp("js").expect("temp file");
        let mode = std::fs::metadata(&tmp)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(mode, 0o600, "temp file mode is {:o}, expected 600", mode);
    }

    #[tokio::test]
    async fn json_shell_and_js_are_checked_and_other_types_are_not() {
        assert!(
            guard_write(Path::new("a.json"), Some("{}"), "{\"a\": 1,}")
                .await
                .is_err()
        );
        assert!(
            guard_write(Path::new("a.json"), Some("{}"), "{\"a\": 1}")
                .await
                .is_ok()
        );
        assert!(
            guard_write(Path::new("a.txt"), Some("x"), "{{{")
                .await
                .is_ok()
        );
        assert!(
            guard_write(Path::new("a.rs"), Some("fn a(){}"), "fn a({")
                .await
                .is_ok()
        );
        if has("bash") {
            assert!(
                guard_write(Path::new("s.sh"), Some("echo hi\n"), "if then fi\n")
                    .await
                    .is_err()
            );
            assert!(
                guard_write(Path::new("s.sh"), Some("echo hi\n"), "echo ok\n")
                    .await
                    .is_ok()
            );
        }
        if has("node") {
            assert!(
                guard_write(Path::new("f.js"), Some("let a = 1;\n"), "let a = ;\n")
                    .await
                    .is_err()
            );
            assert!(
                guard_write(Path::new("f.js"), Some("let a = 1;\n"), "let a = 2;\n")
                    .await
                    .is_ok()
            );
        }
    }

    // #325: a checker that cannot run must be an error, never a clean pass.
    #[tokio::test]
    async fn missing_checker_binary_is_an_error_not_a_clean_pass() {
        let r = run_checker("zen-no-such-checker-binary-xyz", &["--version"], "x = 1\n").await;
        assert!(
            r.is_err(),
            "missing checker must not report a clean pass: {r:?}"
        );
    }

    #[tokio::test]
    async fn checker_timeout_is_an_error_not_a_clean_pass() {
        if !has("python3") {
            return;
        }
        let r = run_checker_with_timeout(
            "python3",
            &["-c", "import time; time.sleep(5)"],
            "",
            Duration::from_millis(100),
        )
        .await;
        assert!(
            r.is_err(),
            "timed-out checker must not report a clean pass: {r:?}"
        );
    }

    #[tokio::test]
    async fn run_checker_reports_exit_status() {
        if !has("true") || !has("false") {
            return;
        }
        assert_eq!(run_checker("true", &[], "x = 1\n").await, Ok(None));
        assert!(matches!(
            run_checker("false", &[], "x = 1\n").await,
            Ok(Some(_))
        ));
    }

    // #325: on a machine without python3 the check must report
    // unavailable instead of silently passing. (No-op where python3
    // exists; the missing-binary case is covered by
    // missing_checker_binary_is_an_error_not_a_clean_pass, and the
    // guard_write level by the harness binary, see SUMMARY.md.)
    #[tokio::test]
    async fn syntax_error_reports_unavailable_checker() {
        if !has("python3") {
            // No python3 here: the check itself must report unavailable,
            // not silently pass.
            let r = syntax_error(Path::new("m.py"), "x = 1\n").await;
            assert!(
                r.is_err(),
                "missing python3 must not report a clean pass: {r:?}"
            );
        }
    }

    // The refusal message is the only thing the agent sees when the guard
    // cannot run: it must name the file, carry the reason, and say the
    // change was not applied. (Kills the mutants that replace the whole
    // message with an empty or arbitrary string.)
    #[test]
    fn refusal_unavailable_names_file_reason_and_refusal() {
        let msg = refusal_unavailable(Path::new("m.py"), "checker 'python3' timed out");
        assert!(msg.contains("m.py"), "message must name the file: {msg}");
        assert!(
            msg.contains("timed out"),
            "message must carry the reason: {msg}"
        );
        assert!(
            msg.contains("NOT applied"),
            "message must say the change was not applied: {msg}"
        );
        assert!(
            msg.contains("ZEN_NO_SYNTAX_GUARD=1"),
            "message must point at the bypass: {msg}"
        );
    }
}
