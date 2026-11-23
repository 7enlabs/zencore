//! Issue #276 follow-up: cover the `skill remove` paths mutants survived in:
//! the `--quiet` guards around the remove messages (skills.rs skill_command)
//! and the interactive decline path in `cmd_remove`.

mod common;

use common::{TempDir, ZenRun};
#[cfg(unix)]
use common::{scrub_zen_env, zen_bin};
#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Stdio};

fn install_tdd(home: &TempDir, cwd: &TempDir) {
    let (_, err, code) = ZenRun::new("skill_remove", home, cwd)
        .args(["skill", "install", "builtin:tdd", "--yes"])
        .run_output();
    assert_eq!(code, 0, "{err}");
}

fn skill_dir(home: &TempDir) -> PathBuf {
    home.path().join(".zen").join("skills").join("tdd")
}

// `skill remove --yes` prints the confirmation unless `--quiet` is given.
#[test]
fn skill_remove_yes_prints_removed_unless_quiet() {
    let home = TempDir::new("skill_remove_quiet");
    let cwd = TempDir::new("skill_remove_quiet_cwd");

    install_tdd(&home, &cwd);
    let (out, err, code) = ZenRun::new("skill_remove", &home, &cwd)
        .args(["skill", "remove", "--yes", "tdd"])
        .run_output();
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("removed skill"),
        "stdout should confirm removal: {out}"
    );
    assert!(!skill_dir(&home).exists());

    install_tdd(&home, &cwd);
    let (out, err, code) = ZenRun::new("skill_remove", &home, &cwd)
        .args(["--quiet", "skill", "remove", "--yes", "tdd"])
        .run_output();
    assert_eq!(code, 0, "{err}");
    assert!(
        !out.contains("removed skill"),
        "--quiet must suppress the message: {out}"
    );
    assert!(!skill_dir(&home).exists());
}

// Declining the interactive prompt cancels the removal, and the
// "remove cancelled" message follows the same `--quiet` rule. The prompt
// only appears on a real terminal, so the binary runs under a pty via
// `script` (same approach as the flightdeck demo tests). Unix-only:
// Windows has no `script`.
#[cfg(unix)]
fn run_remove_under_pty(home: &TempDir, cwd: &TempDir, quiet: bool) -> String {
    let bin = zen_bin();
    let inner = if quiet {
        format!("{} --quiet skill remove tdd", bin.display())
    } else {
        format!("{} skill remove tdd", bin.display())
    };
    let cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("script");
        c.args(["-q", "/dev/null", "sh", "-c", &inner]);
        c
    } else {
        let mut c = Command::new("script");
        c.args(["-qec", &inner, "/dev/null"]);
        c
    };
    let mut cmd = scrub_zen_env(cmd);
    cmd.current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn script");
    {
        let mut stdin = child.stdin.take().expect("pty stdin");
        stdin.write_all(b"n\n").expect("write answer");
    }
    let out = child.wait_with_output().expect("wait script");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
#[cfg(unix)]
fn skill_remove_decline_on_terminal_cancels() {
    let home = TempDir::new("skill_remove_decline");
    let cwd = TempDir::new("skill_remove_decline_cwd");
    install_tdd(&home, &cwd);

    let out = run_remove_under_pty(&home, &cwd, false);
    assert!(
        skill_dir(&home).exists(),
        "declining must keep the skill: {out}"
    );
    assert!(
        out.contains("remove cancelled"),
        "decline must print the message: {out}"
    );

    let out = run_remove_under_pty(&home, &cwd, true);
    assert!(
        skill_dir(&home).exists(),
        "declining must keep the skill (quiet): {out}"
    );
    assert!(
        !out.contains("remove cancelled"),
        "--quiet must suppress the message: {out}"
    );
}
