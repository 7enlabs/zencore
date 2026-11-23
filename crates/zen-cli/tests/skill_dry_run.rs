mod common;

use common::*;

// Issue #281: --dry-run previews the write and changes nothing.

#[test]
fn skill_install_builtin_dry_run_writes_nothing() {
    let home = TempDir::new("skill_dry_home");
    let cwd = TempDir::new("skill_dry_cwd");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("skill_dry", &home, &cwd)
            .args(args)
            .run_output()
    };

    let (out, err, code) = run(&["skill", "install", "--dry-run", "builtin:tdd"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("would install builtin skill tdd"),
        "stdout: {out}"
    );
    assert!(out.contains("no change made"), "stdout: {out}");
    assert!(!home.path().join(".zen/skills/tdd").exists());
    let (out, _, code) = run(&["skill", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("no skills installed"), "stdout: {out}");

    // An unknown builtin still errors in dry-run mode (validation runs).
    let (_, _, code) = run(&["skill", "install", "--dry-run", "builtin:nope"]);
    assert_ne!(code, 0);
}

#[test]
fn skill_remove_dry_run_keeps_installed_skill() {
    let home = TempDir::new("skill_dry_home2");
    let cwd = TempDir::new("skill_dry_cwd2");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("skill_dry", &home, &cwd)
            .args(args)
            .run_output()
    };

    let (_, err, code) = run(&["skill", "install", "--yes", "builtin:tdd"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(home.path().join(".zen/skills/tdd").exists());

    let (out, err, code) = run(&["skill", "remove", "--dry-run", "tdd"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("would remove skill tdd"), "stdout: {out}");
    assert!(out.contains("no change made"), "stdout: {out}");
    assert!(home.path().join(".zen/skills/tdd").exists());
    let (out, _, code) = run(&["skill", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("tdd"), "stdout: {out}");

    // The real remove still works.
    let (_, err, code) = run(&["skill", "remove", "tdd"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!home.path().join(".zen/skills/tdd").exists());

    // Removing a skill that is not installed errors, dry-run or not.
    let (_, _, code) = run(&["skill", "remove", "--dry-run", "tdd"]);
    assert_ne!(code, 0);
}

#[test]
fn skill_create_dry_run_writes_nothing() {
    let home = TempDir::new("skill_dry_home3");
    let cwd = TempDir::new("skill_dry_cwd3");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("skill_dry", &home, &cwd)
            .args(args)
            .run_output()
    };

    let (out, err, code) = run(&["skill", "create", "--dry-run", "my-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("would create skill my-skill"), "stdout: {out}");
    assert!(out.contains("no change made"), "stdout: {out}");
    assert!(!home.path().join(".zen/skills/my-skill").exists());

    // A bad name still errors in dry-run mode.
    let (_, _, code) = run(&["skill", "create", "--dry-run", "bad name!"]);
    assert_ne!(code, 0);

    // The real create still works, and dry-run then reports the conflict.
    let (_, err, code) = run(&["skill", "create", "my-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(home.path().join(".zen/skills/my-skill").exists());
    let (_, _, code) = run(&["skill", "create", "--dry-run", "my-skill"]);
    assert_ne!(code, 0);
}

#[test]
fn skill_install_local_dry_run_writes_nothing() {
    let home = TempDir::new("skill_dry_home4");
    let cwd = TempDir::new("skill_dry_cwd4");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|x| x.to_string()).collect();
        ZenRun::new("skill_dry", &home, &cwd)
            .args(args)
            .run_output()
    };

    let fixture = cwd.path().join("fixture-skill");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(
        fixture.join("SKILL.md"),
        "---\nname: fixture-skill\ndescription: Fixture skill for dry-run tests\nallowed-tools: read\n---\n\n# Fixture\n\nDo nothing.\n",
    )
    .unwrap();
    std::fs::write(
        fixture.join("zen-plugin.json"),
        "{\"name\": \"fixture-skill\", \"version\": \"0.2.0\", \"description\": \"Fixture skill for dry-run tests\", \"author\": \"\"}",
    )
    .unwrap();

    let (out, err, code) = run(&["skill", "install", "--dry-run", "./fixture-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("would install skill fixture-skill v0.2.0"),
        "stdout: {out}"
    );
    assert!(out.contains("no change made"), "stdout: {out}");
    assert!(!home.path().join(".zen/skills/fixture-skill").exists());

    // Real install, then the dry run reports what it would replace.
    let (_, err, code) = run(&["skill", "install", "--yes", "./fixture-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (out, err, code) = run(&["skill", "install", "--dry-run", "./fixture-skill"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("replaces: fixture-skill v0.2.0"),
        "stdout: {out}"
    );
    assert!(home.path().join(".zen/skills/fixture-skill").exists());
}
