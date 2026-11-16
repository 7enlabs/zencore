//! Recovery from poisoned mutexes (C3 lock-poison graceful).
//!
//! A `std::sync::Mutex` is poisoned when a thread panics while holding
//! it. The old code called `.lock().expect("... poisoned ...")` at 66
//! production sites, so one poisoned lock crashed the whole REPL
//! session (exit code 101). C3 replaces those panics with graceful
//! degradation: recover the guarded value, mark the session degraded,
//! warn once, and keep going.
//!
//! The first poisoned lock observed in the process sets the global
//! degraded flag and prints one warning telling the user to save their
//! work and restart. Later poisoned locks recover silently. Callers
//! that own a transcript flush it when [`is_degraded`] is true (see
//! `TranscriptWriter::flush` and the `persist` step in the REPL loop).

use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set the first time any mutex in the process is observed poisoned.
/// Never cleared: a degraded session stays degraded until restart.
pub static DEGRADED_MODE: AtomicBool = AtomicBool::new(false);

/// Whether any mutex has been observed poisoned in this process.
pub fn is_degraded() -> bool {
    DEGRADED_MODE.load(Ordering::SeqCst)
}

/// Lock `mutex`, recovering its inner value instead of panicking when
/// the mutex is poisoned.
///
/// On the first poisoned lock in the process this sets the degraded
/// flag and prints one warning to stderr; later poisoned locks recover
/// silently. The recovered value is the last state the previous holder
/// left behind, so callers that need a consistency guarantee (for
/// example checkpoint turns) must validate or rebuild that state
/// themselves instead of trusting it blindly.
pub fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    lock_or_recover_status(mutex, name).0
}

/// Like [`lock_or_recover`], but also reports whether the lock was
/// poisoned, so callers with state to validate can sanitize the
/// recovered value instead of trusting it blindly.
pub fn lock_or_recover_status<'a, T>(mutex: &'a Mutex<T>, name: &str) -> (MutexGuard<'a, T>, bool) {
    match mutex.lock() {
        Ok(guard) => (guard, false),
        Err(poisoned) => {
            if !DEGRADED_MODE.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "warning: lock {name} poisoned, continuing in degraded mode; save your work and restart"
                );
            }
            (poisoned.into_inner(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Poison `mutex` by locking it in a thread that panics while
    /// holding the guard, then joining that thread.
    fn poison<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
        let m = mutex.clone();
        let handle = std::thread::spawn(move || {
            let _guard = m.lock().unwrap();
            panic!("deliberate poison for the lock-recovery test");
        });
        let _ = handle.join();
        assert!(mutex.is_poisoned());
    }

    #[test]
    fn recovers_poisoned_value_and_marks_degraded() {
        let mutex = Arc::new(Mutex::new(41u32));
        poison(&mutex);
        // The degraded flag is set by the first recovery, not by the
        // poison itself.
        let mut guard = lock_or_recover(&mutex, "test counter");
        assert!(is_degraded());
        assert_eq!(*guard, 41);
        *guard = 42;
        drop(guard);
        assert_eq!(*mutex.lock().unwrap_or_else(|e| e.into_inner()), 42);
    }

    #[test]
    fn second_poison_still_recovers_while_degraded() {
        let mutex = Arc::new(Mutex::new(String::from("kept")));
        poison(&mutex);
        let first = lock_or_recover(&mutex, "test string");
        assert_eq!(*first, "kept");
        drop(first);
        poison(&mutex);
        let second = lock_or_recover(&mutex, "test string");
        assert_eq!(*second, "kept");
        assert!(is_degraded());
    }
}
