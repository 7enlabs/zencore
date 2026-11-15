//! C3 lock-poison graceful: tests that must live in their own test
//! binary, because they observe process-global state.
//!
//! The degraded flag is process-global and never cleared, so a test
//! asserting it starts clear cannot share a binary with any test
//! that poisons a lock. This binary never poisons anything itself:
//! the warn-once test runs the poison scenario in a child process
//! (this same binary, filtered to one test) and reads its stderr.

use std::sync::Arc;
use std::sync::Mutex;
use zen_core::lock_recovery::{is_degraded, lock_or_recover};

/// C3: the degraded flag starts clear. Kills the
/// `is_degraded -> true` mutant: with the flag forced true, a
/// session that never saw a poison would report itself degraded.
#[test]
fn c3_degraded_flag_starts_clear() {
    assert!(!is_degraded());
}

/// C3: the first poisoned lock warns once, later ones stay silent.
/// Kills the `delete !` mutant in `lock_or_recover_status`: with the
/// negation removed the warning moves from the first recovery to the
/// second, so its position relative to the markers flips.
#[test]
fn c3_warning_printed_once_on_first_poison() {
    let exe = std::env::current_exe().expect("test binary path");
    let out = std::process::Command::new(exe)
        .args(["c3_warn_once_child", "--exact", "--nocapture"])
        .env("C3_WARN_ONCE_CHILD", "1")
        .output()
        .expect("spawn child test binary");
    assert!(out.status.success(), "child exit: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let warning = "continuing in degraded mode";
    assert_eq!(
        stderr.matches(warning).count(),
        1,
        "expected exactly one degraded warning in child stderr:\n{stderr}"
    );
    let warn_pos = stderr.find(warning).unwrap();
    let first_done = stderr.find("first-done").expect("first-done marker");
    assert!(
        warn_pos < first_done,
        "warning must print on the first recovery, before first-done:\n{stderr}"
    );
}

/// Child side of `c3_warning_printed_once_on_first_poison`: only
/// runs when the parent spawns this binary with
/// `C3_WARN_ONCE_CHILD=1`. Poisons one mutex, recovers it twice,
/// and marks each step on stderr so the parent can tell which
/// recovery printed the warning.
#[test]
fn c3_warn_once_child() {
    if std::env::var_os("C3_WARN_ONCE_CHILD").is_none() {
        return;
    }
    let mutex = Arc::new(Mutex::new(0u32));
    let m = mutex.clone();
    let handle = std::thread::spawn(move || {
        let _guard = m.lock().unwrap();
        panic!("deliberate poison for the warn-once test");
    });
    let _ = handle.join();
    eprintln!("child: before first recovery");
    {
        let _guard = lock_or_recover(&mutex, "child");
    }
    eprintln!("child: first-done");
    {
        let _guard = lock_or_recover(&mutex, "child");
    }
    eprintln!("child: second-done");
    std::process::exit(0);
}
