mod common;

use common::*;

// Issue #281 follow-up: kill 5 `delete !` mutants in the dry-run previews.
// Every mutant below removes one `!`: three flip `if !quiet` (the preview
// prints only when quiet) and two flip a name filter (the previewed skill
// name collapses). Each test pins the behaviour the `!` guards.

#[test]
fn secret_register_dry_run_prints_unless_quiet() {
    let home = TempDir::new("dry_quiet_home1");
    let cwd = TempDir::new("dry_quiet_cwd1");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("dry_quiet", &home, &cwd)
            .args(args)
            .run_output()
    };
    let base = [
        "secret",
        "register",
        "--name",
        "DRY_Q_KEY",
        "--class",
        "seat",
        "--service",
        "model-service",
        "--owner",
        "zc-developer",
        "--by",
        "zc-leader",
        "--default-until-days",
        "30",
        "--rotation-days",
        "30",
        "--reason",
        "msg-1",
        "--dry-run",
    ];

    let (out, err, code) = run(&base);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("would append registration for DRY_Q_KEY"),
        "stdout: {out}"
    );

    let mut quiet = base.to_vec();
    quiet.push("--quiet");
    let (out, err, code) = run(&quiet);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !out.contains("would append registration for DRY_Q_KEY"),
        "quiet dry-run must print nothing, stdout: {out}"
    );
}

#[test]
fn secret_approve_dry_run_prints_unless_quiet() {
    let home = TempDir::new("dry_quiet_home2");
    let cwd = TempDir::new("dry_quiet_cwd2");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("dry_quiet", &home, &cwd)
            .args(args)
            .run_output()
    };

    let (_, err, code) = run(&[
        "secret",
        "register",
        "--name",
        "DRY_Q_KEY",
        "--class",
        "seat",
        "--service",
        "model-service",
        "--owner",
        "zc-developer",
        "--by",
        "zc-leader",
        "--default-until-days",
        "30",
        "--rotation-days",
        "30",
        "--reason",
        "msg-1",
    ]);
    assert_eq!(code, 0, "stderr: {err}");

    let (out, err, code) = run(&[
        "secret",
        "request",
        "--name",
        "DRY_Q_KEY",
        "--purpose",
        "test",
        "--scope",
        "task:DS-7",
        "--until",
        "2026-12-01T00:00:00Z",
        "--seat",
        "zc-developer",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    let id = out
        .split_whitespace()
        .find(|t| t.starts_with("sr-"))
        .expect("request prints its id")
        .to_string();

    let (out, err, code) = run(&[
        "secret",
        "approve",
        "--request",
        &id,
        "--by",
        "zc-leader",
        "--evidence",
        "msg-2",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains(&format!("would approve {id}")),
        "stdout: {out}"
    );

    let (out, err, code) = run(&[
        "secret",
        "approve",
        "--request",
        &id,
        "--by",
        "zc-leader",
        "--evidence",
        "msg-2",
        "--dry-run",
        "--quiet",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !out.contains(&format!("would approve {id}")),
        "quiet dry-run must print nothing, stdout: {out}"
    );
}

#[test]
fn skill_install_local_dry_run_uses_directory_name() {
    // Kills skills.rs:1476: with no manifest and no frontmatter name the
    // previewed name comes only from the directory name, so dropping the
    // `!` in `.filter(|n| !n.is_empty())` makes the preview fail.
    let home = TempDir::new("dry_quiet_home3");
    let cwd = TempDir::new("dry_quiet_cwd3");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("dry_quiet", &home, &cwd)
            .args(args)
            .run_output()
    };

    let fixture = cwd.path().join("plain-skill");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(
        fixture.join("SKILL.md"),
        "# Plain\n\nNo frontmatter here.\n",
    )
    .unwrap();

    let (out, err, code) = run(&["skill", "install", "--dry-run", "./plain-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("would install skill plain-skill v0.0.0"),
        "stdout: {out}"
    );
    assert!(!home.path().join(".zen/skills/plain-skill").exists());
}

#[test]
fn skill_install_local_dry_run_uses_frontmatter_name() {
    // Kills skills.rs:1495: with no manifest the SKILL.md frontmatter name
    // wins over the directory name, so dropping the `!` in
    // `.filter(|n| !n.trim().is_empty())` prints the directory name instead.
    let home = TempDir::new("dry_quiet_home4");
    let cwd = TempDir::new("dry_quiet_cwd4");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("dry_quiet", &home, &cwd)
            .args(args)
            .run_output()
    };

    let fixture = cwd.path().join("fm-skill");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(
        fixture.join("SKILL.md"),
        "---\nname: fm-named\ndescription: Frontmatter named skill\nallowed-tools: read\n---\n\n# FM\n",
    )
    .unwrap();

    let (out, err, code) = run(&["skill", "install", "--dry-run", "./fm-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("would install skill fm-named v0.0.0"),
        "stdout: {out}"
    );
    assert!(
        !out.contains("would install skill fm-skill"),
        "frontmatter name must win over the directory name, stdout: {out}"
    );
    assert!(!home.path().join(".zen/skills/fm-named").exists());
}

#[test]
fn import_dry_run_prints_unless_quiet() {
    // Kills export.rs:701: the dry-run preview prints only when not quiet.
    let home = TempDir::new("dry_quiet_home5");
    let cwd = TempDir::new("dry_quiet_cwd5");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("dry_quiet", &home, &cwd)
            .args(args)
            .run_output()
    };

    let file = cwd.path().join("export.json");
    std::fs::write(
        &file,
        "{\"format_version\": 1, \"exported_by\": \"test\", \"exported_at\": \"2026-11-23T00:00:00Z\", \"session_id\": \"sess-dry-1\", \"usage\": {\"input_tokens\": 10, \"output_tokens\": 5}, \"redacted\": false, \"messages\": [{\"type\": \"user\", \"payload\": {\"text\": \"hi\"}}, {\"type\": \"assistant\", \"payload\": {\"text\": \"hello\"}}]}",
    )
    .unwrap();

    let (out, err, code) = run(&["import", file.to_str().unwrap(), "--dry-run"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("would import 2 message(s)"), "stdout: {out}");

    let (out, err, code) = run(&["import", file.to_str().unwrap(), "--dry-run", "--quiet"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        !out.contains("would import 2 message(s)"),
        "quiet dry-run must print nothing, stdout: {out}"
    );
}
