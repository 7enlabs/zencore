//! Two `zen task claim` processes racing on the same --expected-version
//! (#571, Z Tester repro).
//!
//! The registry is seeded with an unterminated tail, the precondition
//! that used to glue the first winner snapshot onto the torn line and
//! lose it on re-read: exactly one claim may win, the loser must be
//! refused as a version conflict (exit 7), and the winner must survive
//! a re-read.

mod common;

use common::{TempDir, ZenRun};
use zen_cli::tasks::load_registry;

#[test]
fn cas_two_cli_processes_with_same_expected_version_have_exactly_one_winner() {
    let home = TempDir::new("cas_race_home");
    let cwd = TempDir::new("cas_race_cwd");
    // Seed one card, deliberately without a trailing newline.
    home.write(
        ".zen/tasks.jsonl",
        r#"{"id":"B-4","title":"fix install","updated_at":"2026-11-07T00:00:00+00:00"}"#,
    );
    let registry = home.path().join(".zen").join("tasks.jsonl");
    let run_claim = |by: &str| {
        ZenRun::new("cas_race", &home, &cwd)
            .args([
                "task",
                "claim",
                "B-4",
                "--by",
                by,
                "--expected-version",
                "0",
                "--json",
                "--raw",
            ])
            .run_output()
    };
    let ((_, _, code_a), (_, _, code_b)) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run_claim("seat-A"));
        let b = scope.spawn(|| run_claim("seat-B"));
        (a.join().unwrap(), b.join().unwrap())
    });
    let winners: Vec<&str> = [("seat-A", code_a), ("seat-B", code_b)]
        .into_iter()
        .filter(|(_, code)| *code == 0)
        .map(|(seat, _)| seat)
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one claim may win: seat-A exit {code_a}, seat-B exit {code_b}"
    );
    let loser_code = if winners[0] == "seat-A" {
        code_b
    } else {
        code_a
    };
    assert_eq!(
        loser_code, 7,
        "the loser must be refused as a version conflict (exit 7), not lost as not-found"
    );
    let cards = load_registry(&registry).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].lease.as_ref().unwrap().holder,
        winners[0],
        "the re-read card must be the winner"
    );
    assert_eq!(cards[0].lease.as_ref().unwrap().version, 1);
}
