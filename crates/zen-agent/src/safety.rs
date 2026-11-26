use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn user_agent() -> String {
    format!("Zen/{} (+https://runzen.dev)", env!("CARGO_PKG_VERSION"))
}

pub const SAFETY_DISCLAIMER: &str = "No tool can guarantee your account won't be flagged - \
    providers use automated systems with false positives. Zen minimizes the risk factors \
    within its control and warns you about the rest.";

pub const MAX_RETRY_ATTEMPTS: u32 = 5;

pub const BACKOFF_BASE_MS: u64 = 1000;

pub const BACKOFF_CAP_MS: u64 = 60_000;

pub const MIN_INTER_REQUEST_MS: u64 = 100;

pub const DEFAULT_CONCURRENCY_CAP: usize = 2;

pub const PROACTIVE_THROTTLE_FRACTION: f64 = 0.8;

pub const PARALLEL_SESSION_WARN_THRESHOLD: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    RateLimit,

    QuotaExhausted,

    Overloaded,

    Fatal,

    Transient,
}

impl ErrorClass {
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorClass::RateLimit | ErrorClass::Overloaded | ErrorClass::Transient
        )
    }
}

/// Classify an HTTP error for retry decisions (issue #347).
///
/// The status decides first: a 429 is a rate limit and stays retryable
/// unless the body carries an explicit quota code (`insufficient_quota`
/// / `exceeded your current quota`). The old body-first scan matched
/// the broad substrings `billing` / `out of credit` on any status, so
/// a 429 saying "Rate limit exceeded. See billing to upgrade your plan"
/// was misclassified as non-retryable quota exhaustion and the run
/// failed instead of backing off. Those broad signals only count when
/// no status contradicts them (402, or no status at all).
pub fn classify_error(status: Option<u16>, body: &str) -> ErrorClass {
    let lower = body.to_ascii_lowercase();
    let explicit_quota = lower.contains("insufficient_quota")
        || lower.contains("insufficient quota")
        || lower.contains("exceeded your current quota");
    let billing_signal =
        explicit_quota || lower.contains("billing") || lower.contains("out of credit");

    match status {
        Some(529) => ErrorClass::Overloaded,
        Some(429) if explicit_quota => ErrorClass::QuotaExhausted,
        Some(429) => ErrorClass::RateLimit,
        Some(402) => ErrorClass::QuotaExhausted,
        Some(code) if (400..500).contains(&code) => ErrorClass::Fatal,
        Some(code) if (500..600).contains(&code) => ErrorClass::Transient,

        None => {
            if billing_signal {
                ErrorClass::QuotaExhausted
            } else {
                ErrorClass::Transient
            }
        }
        _ => ErrorClass::Fatal,
    }
}

pub fn backoff_with_jitter(attempt: u32, base_ms: u64, cap_ms: u64, jitter_seed: u64) -> u64 {
    let exp = attempt.saturating_sub(1).min(10);
    let backoff = base_ms.saturating_mul(1u64 << exp).min(cap_ms);

    let mut x = jitter_seed.wrapping_add(0x9E37_79B9_7F4A_7C15).max(1);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let jitter_pct = (x % 51) as f64 - 25.0;
    let jittered = backoff as f64 * (1.0 + jitter_pct / 100.0);
    (jittered.max(0.0) as u64).min(cap_ms).max(1)
}

pub fn jitter_now_nanos() -> u64 {
    if let Ok(seed) = std::env::var("ZEN_JITTER_SEED")
        && let Ok(n) = seed.parse::<u64>()
    {
        return n;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678)
}

pub fn default_rpm_ceiling(provider: &str) -> Option<u64> {
    match provider.to_ascii_lowercase().as_str() {
        p if p.contains("openai") => Some(400),
        p if p.contains("anthropic") => Some(40),
        p if p.contains("google") || p.contains("gemini") => Some(100),
        p if p.contains("openrouter") => Some(15),

        p if p.contains("ollama") || p.contains("localhost") || p.contains("127.0.0.1") => None,
        _ => Some(60),
    }
}

pub struct RateLimiter {
    rpm_ceiling: f64,

    tokens: f64,

    last_refill: Instant,

    last_request: Option<Instant>,

    window_count: u64,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(rpm_ceiling: u64) -> Self {
        let now = Instant::now();
        Self {
            rpm_ceiling: rpm_ceiling as f64,
            tokens: rpm_ceiling as f64,
            last_refill: now,
            last_request: None,
            window_count: 0,
            window_start: now,
        }
    }

    pub fn for_provider(provider: &str) -> Option<Self> {
        default_rpm_ceiling(provider).map(Self::new)
    }

    pub fn acquire(&mut self) -> Duration {
        self.refill();
        let mut wait = Duration::ZERO;

        if let Some(last) = self.last_request {
            let floor = Duration::from_millis(MIN_INTER_REQUEST_MS);
            let elapsed = last.elapsed();
            if elapsed < floor {
                wait = floor - elapsed;
            }
        }

        if self.tokens < 1.0 {
            let per_token = Duration::from_secs_f64(60.0 / self.rpm_ceiling.max(1.0));
            wait = wait.max(per_token);
        } else {
            self.tokens -= 1.0;
        }

        if self.window_count as f64 >= self.rpm_ceiling * PROACTIVE_THROTTLE_FRACTION {
            wait = wait.max(Duration::from_millis(500));
        }

        self.last_request = Some(Instant::now() + wait);
        self.window_count += 1;
        wait
    }

    pub fn update_ceiling(&mut self, rpm: u64) {
        self.rpm_ceiling = rpm as f64;
        self.tokens = self.tokens.min(self.rpm_ceiling);
    }

    fn refill(&mut self) {
        let now = Instant::now();

        if now.duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.window_count = 0;
        }
        let elapsed_secs = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed_secs * self.rpm_ceiling / 60.0).min(self.rpm_ceiling);
        self.last_refill = now;
    }
}

/// How long a registration waits for the registry lock before giving
/// up and skipping the save. Registrations are short, so ten seconds
/// is ample; waiting forever would hang session startup behind a stuck
/// peer. Same bound as the secret-store lock (zen-core `lock_store`).
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The lock file next to the registry file: `session-registry.lock`.
/// It only carries the flock; its content is never read or written, and
/// it is never renamed or removed, so the lock stays valid across
/// registry rewrites and a crashed holder releases it with the process.
fn registry_lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// One held registry lock. Dropping it closes the file, which releases
/// the flock.
struct RegistryLock {
    _file: std::fs::File,
}

/// Whether a lock error means another process holds the lock (retry
/// until the deadline) or something else is wrong (give up at once).
/// Kept as a tiny pure function so tests can feed it synthetic errors:
/// Unix reports contention as WouldBlock, Windows as ERROR_LOCK_VIOLATION
/// (raw OS error 33), which std maps to ErrorKind::Other (issue #319).
enum LockWait {
    Retry,
    GiveUp,
}

fn classify_lock_error(e: &std::io::Error) -> LockWait {
    if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(33) {
        LockWait::Retry
    } else {
        LockWait::GiveUp
    }
}

/// Acquire the registry lock, waiting a bounded time. Returns `None` on
/// timeout or when the lock file cannot be opened, so the caller can
/// fail closed instead of hanging session startup.
fn acquire_registry_lock(path: &Path) -> Option<RegistryLock> {
    use fs2::FileExt;
    let lock_path = registry_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // The file only carries the flock; its content is never read
        // or written, so it must not be truncated on open.
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    let deadline = Instant::now() + REGISTRY_LOCK_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Some(RegistryLock { _file: file }),
            // Contention means retry until the deadline; any other
            // error gives up at once (see `classify_lock_error`).
            Err(e) => match classify_lock_error(&e) {
                LockWait::Retry => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                LockWait::GiveUp => return None,
            },
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionRegistry {
    #[serde(default)]
    entries: HashMap<String, HashMap<String, Vec<(String, u64)>>>,
}

impl SessionRegistry {
    pub fn load() -> Self {
        let path = registry_path();
        let Ok(data) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&data).unwrap_or_default()
    }

    pub fn register(&mut self, provider: &str, key_fingerprint: &str, session_id: &str) -> usize {
        // Hold the lock across the whole read-modify-write: reload under
        // it first, because another process may have registered since
        // this instance was loaded (issue #356). Without the reload the
        // second save overwrites the first registration.
        // If the lock cannot be acquired, do NOT save: persisting the
        // stale in-memory snapshot would wipe a concurrent registration
        // (the exact lost update #356 fixed). The session simply goes
        // untracked this once; no other process's data is harmed.
        let Some(_lock) = acquire_registry_lock(&registry_path()) else {
            return self.count_others(provider, key_fingerprint, session_id);
        };
        *self = Self::load();
        self.prune_stale();
        let now = now_unix();
        let others = {
            let key_map = self.entries.entry(provider.to_string()).or_default();
            let sessions = key_map.entry(key_fingerprint.to_string()).or_default();
            if !sessions.iter().any(|(id, _)| id == session_id) {
                sessions.push((session_id.to_string(), now));
            }
            sessions.len().saturating_sub(1)
        };
        self.save();
        others
    }

    pub fn unregister(&mut self, provider: &str, key_fingerprint: &str, session_id: &str) {
        // Same lock + reload as `register`: an unregister racing a
        // register in another process must not drop that registration.
        // If the lock cannot be acquired, do nothing: removing from a
        // stale snapshot and saving it would wipe a concurrent
        // registration.
        let Some(_lock) = acquire_registry_lock(&registry_path()) else {
            return;
        };
        *self = Self::load();
        if let Some(key_map) = self.entries.get_mut(provider)
            && let Some(sessions) = key_map.get_mut(key_fingerprint)
        {
            sessions.retain(|(id, _)| id != session_id);
        }
        self.save();
    }

    fn prune_stale(&mut self) {
        let cutoff = now_unix().saturating_sub(24 * 3600);
        for key_map in self.entries.values_mut() {
            for sessions in key_map.values_mut() {
                sessions.retain(|(_, ts)| *ts >= cutoff);
            }
        }
    }

    /// How many sessions other than `session_id` are registered under
    /// this provider + key in the in-memory snapshot. Used for the
    /// fail-closed return when the lock cannot be acquired.
    fn count_others(&self, provider: &str, key_fingerprint: &str, session_id: &str) -> usize {
        let Some(sessions) = self
            .entries
            .get(provider)
            .and_then(|key_map| key_map.get(key_fingerprint))
        else {
            return 0;
        };
        let total = sessions.len();
        if sessions.iter().any(|(id, _)| id == session_id) {
            total.saturating_sub(1)
        } else {
            total
        }
    }

    fn save(&self) {
        // Atomic write like `RateGate::write_store`: write a temp file in
        // the same directory, then rename over the registry, so a reader
        // never sees a torn file and a crash cannot leave a half-written
        // registry behind. The temp file is unique per save (pid +
        // counter), never the fixed `session-registry.json.tmp` shared
        // by all processes: two writers without the lock used to
        // interleave writes into the same temp file and publish a torn
        // registry through the rename.
        let path = registry_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(data) = serde_json::to_vec_pretty(self) else {
            return;
        };
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// A temp path private to this save: the process id plus a per-process
/// counter, so two processes (or two threads) never share one temp file.
fn unique_tmp_path(path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("json.tmp.{}-{n}", std::process::id()))
}

pub fn key_fingerprint(api_key: &str) -> String {
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let prefix: String = api_key.chars().take(3).collect();
    format!("{}-...{}", prefix, tail)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Test hook: point the registry at a temp file so two-process tests do
/// not touch the real `~/.zen/session-registry.json`. Production never
/// sets this variable.
const REGISTRY_PATH_ENV: &str = "ZEN_SESSION_REGISTRY_PATH";

fn registry_path() -> PathBuf {
    if let Ok(path) = std::env::var(REGISTRY_PATH_ENV)
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".zen").join("session-registry.json")
}

pub fn log_risk_acknowledgement(session_dir: &std::path::Path, reason: &str) {
    let entry = serde_json::json!({
        "event": "risk_acknowledged",
        "flag": "--i-understand-the-risks",
        "reason": reason,
        "timestamp_unix": now_unix(),
    });
    let path = session_dir.join("risk-acknowledgements.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", entry);
    }
}

pub fn auth_login_warning(provider: &str) -> String {
    format!(
        "API key saved for {provider}.\n\
        \n\
        Account safety: Zen paces requests and backs off automatically, but your account is your responsibility. To stay safe:\n\
        - Use one key per machine - don't share keys across computers or people.\n\
        - Avoid running many parallel Zen sessions on the same key.\n\
        - If you see repeated rate-limit warnings, slow down rather than pushing through.\n\
        \n\
        Providers may suspend accounts for payment disputes, shared keys, or bot-like traffic patterns. Run `zen doctor` for a safety checklist."
    )
}

pub fn parallel_sessions_warning(count: usize, provider: &str) -> String {
    format!(
        "Warning: {count} concurrent Zen sessions are using the same {provider} API key.\n\
        Agentic loops fire many requests per minute. Multiple sessions on one key can hit rate limits fast and may look like automated abuse to the provider.\n\
        Consider: separate API keys per session, or run fewer sessions. (Silence with --i-understand-the-risks.)"
    )
}

pub fn concurrency_cap_warning(requested: usize, cap: usize, provider: &str) -> String {
    format!(
        "Warning: requested concurrency {requested} exceeds the safe concurrency cap ({cap}) for {provider}.\n\
        High concurrency on a single key risks rate-limit storms and account flags.\n\
        Re-run with --i-understand-the-risks to proceed anyway, or lower the concurrency."
    )
}

pub fn repeated_429_warning(hits: u32, provider: &str) -> String {
    format!(
        "Warning: rate limits hit {hits} times in the last 5 minutes ({provider}).\n\
        Zen is backing off automatically. If this persists: reduce parallel sessions, switch to a higher tier, or use a different provider to spread load. Pushing through rate limits is the fastest way to get flagged."
    )
}

pub fn quota_exhausted_error(provider: &str, detail: &str) -> String {
    format!(
        "Out of quota / billing issue ({provider}: {detail}).\n\
        Zen will NOT retry - retrying a billing error only creates failed-request noise on your account.\n\
        Check your billing dashboard, then re-run. Your work is saved; nothing was lost."
    )
}

pub fn first_429_notice(provider: &str) -> String {
    format!(
        "Rate limited by {provider} - backing off automatically. This is normal under load; your request will be retried."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_has_version_and_domain() {
        let ua = user_agent();
        assert!(ua.starts_with("Zen/"), "ua: {ua}");
        assert!(ua.contains("runzen.dev"), "ua: {ua}");
    }

    #[test]
    fn classify_insufficient_quota_never_retries() {
        let bodies = [
            r#"{"error":{"type":"insufficient_quota","message":"You exceeded your current quota"}}"#,
            r#"{"error":{"code":"insufficient_quota"}}"#,
        ];
        for b in bodies {
            let c = classify_error(Some(429), b);
            assert_eq!(c, ErrorClass::QuotaExhausted, "body: {b}");
            assert!(!c.is_retryable());
        }

        assert_eq!(classify_error(Some(402), "{}"), ErrorClass::QuotaExhausted);
    }

    #[test]
    fn classify_429_mentioning_billing_is_retryable_rate_limit() {
        // Issue #347: a 429 whose body mentions billing is a rate
        // limit, not quota exhaustion - the old body-first scan
        // classified it QuotaExhausted and the run failed instead of
        // backing off and retrying.
        let c = classify_error(
            Some(429),
            "Rate limit exceeded. See billing to upgrade your plan",
        );
        assert_eq!(c, ErrorClass::RateLimit);
        assert!(c.is_retryable());

        let c = classify_error(
            Some(429),
            r#"{"error": {"message": "Billing issue: out of credit"}}"#,
        );
        assert_eq!(
            c,
            ErrorClass::RateLimit,
            "broad billing words must not beat a 429"
        );
        assert!(c.is_retryable());
    }

    #[test]
    fn classify_429_with_only_exceeded_quota_phrase_is_quota_exhausted() {
        // Issue #347: each quota phrase must count on its own - a 429
        // whose body carries only "exceeded your current quota" (no
        // "insufficient_quota" / "insufficient quota") is still quota
        // exhaustion. Kills the mutant that turns the second `||` of
        // explicit_quota into `&&`.
        let c = classify_error(
            Some(429),
            r#"{"error": {"message": "You have exceeded your current quota."}}"#,
        );
        assert_eq!(c, ErrorClass::QuotaExhausted);
        assert!(!c.is_retryable());
    }

    #[test]
    fn classify_no_status_with_only_billing_word_is_quota_exhausted() {
        // Issue #347: with no status to contradict the body, the word
        // "billing" alone is a quota signal. Kills the mutant that
        // turns the first `||` of billing_signal into `&&`.
        let c = classify_error(None, "Billing issue, please check your account");
        assert_eq!(c, ErrorClass::QuotaExhausted);
        assert!(!c.is_retryable());
    }

    #[test]
    fn classify_no_status_with_only_out_of_credit_is_quota_exhausted() {
        // Issue #347: with no status to contradict the body, the phrase
        // "out of credit" alone is a quota signal. Kills the mutant
        // that turns the second `||` of billing_signal into `&&`.
        let c = classify_error(None, "You are out of credit for this month");
        assert_eq!(c, ErrorClass::QuotaExhausted);
        assert!(!c.is_retryable());
    }

    #[test]
    fn classify_billing_body_is_quota_only_without_contradicting_status() {
        // The broad billing / out-of-credit signals still count where
        // no status contradicts them: 402 is a billing status, and
        // with no status there is nothing to contradict the body.
        assert_eq!(
            classify_error(
                Some(402),
                r#"{"error": {"message": "Billing issue: out of credit"}}"#
            ),
            ErrorClass::QuotaExhausted
        );
        assert_eq!(
            classify_error(None, "Billing issue: out of credit"),
            ErrorClass::QuotaExhausted
        );
        assert_eq!(
            classify_error(Some(500), "Billing issue: out of credit"),
            ErrorClass::Transient,
            "a 5xx keeps its status-based class"
        );
    }

    #[test]
    fn classify_429_is_retryable_rate_limit() {
        let c = classify_error(Some(429), r#"{"error":{"type":"rate_limit_exceeded"}}"#);
        assert_eq!(c, ErrorClass::RateLimit);
        assert!(c.is_retryable());
    }

    #[test]
    fn classify_529_is_overloaded() {
        let c = classify_error(Some(529), r#"{"error":{"type":"overloaded_error"}}"#);
        assert_eq!(c, ErrorClass::Overloaded);
        assert!(c.is_retryable());
    }

    #[test]
    fn classify_4xx_fatal_no_retry() {
        for code in [400u16, 401, 403, 404] {
            let c = classify_error(Some(code), "{}");
            assert_eq!(c, ErrorClass::Fatal, "code {code}");
            assert!(!c.is_retryable());
        }
    }

    #[test]
    fn classify_5xx_transient_retryable() {
        for code in [500u16, 502, 503, 504] {
            let c = classify_error(Some(code), "{}");
            assert_eq!(c, ErrorClass::Transient, "code {code}");
            assert!(c.is_retryable());
        }
    }

    #[test]
    fn classify_network_error_transient() {
        assert_eq!(classify_error(None, ""), ErrorClass::Transient);
    }

    #[test]
    fn backoff_grows_exponentially_with_cap() {
        let b1 = backoff_with_jitter(1, 1000, 60_000, 42);
        let b2 = backoff_with_jitter(2, 1000, 60_000, 42);
        let b3 = backoff_with_jitter(3, 1000, 60_000, 42);

        assert!((750..=1250).contains(&b1), "b1={b1}");
        assert!((1500..=2500).contains(&b2), "b2={b2}");
        assert!((3000..=5000).contains(&b3), "b3={b3}");

        let b20 = backoff_with_jitter(20, 1000, 60_000, 42);
        assert!(b20 <= 60_000, "b20={b20}");
        assert!(b20 >= 1);
    }

    #[test]
    fn backoff_deterministic_for_fixed_seed() {
        assert_eq!(
            backoff_with_jitter(3, 1000, 60_000, 7),
            backoff_with_jitter(3, 1000, 60_000, 7)
        );
    }

    #[test]
    fn default_ceilings_are_conservative() {
        assert_eq!(default_rpm_ceiling("openai"), Some(400));
        assert_eq!(default_rpm_ceiling("anthropic"), Some(40));
        assert_eq!(default_rpm_ceiling("openrouter"), Some(15));

        assert_eq!(default_rpm_ceiling("ollama"), None);
        assert_eq!(default_rpm_ceiling("http://localhost:11434/v1"), None);
    }

    #[test]
    fn rate_limiter_enforces_floor_and_bucket() {
        let mut rl = RateLimiter::new(600);
        let w1 = rl.acquire();
        assert_eq!(w1, Duration::ZERO, "first request goes immediately");
        let w2 = rl.acquire();

        assert!(
            w2 >= Duration::from_millis(90),
            "floor not enforced: {w2:?}"
        );
    }

    #[test]
    fn rate_limiter_update_ceiling_from_headers() {
        let mut rl = RateLimiter::new(40);
        rl.update_ceiling(1000);

        let _ = rl.acquire();
    }

    #[test]
    fn key_fingerprint_masks() {
        let fp = key_fingerprint("sk-ant-abc123xyz789");
        assert!(fp.contains("..."), "fp: {fp}");
        assert!(!fp.contains("abc123xyz789"), "full key leaked in fp: {fp}");
        assert_eq!(fp, "sk--...z789");
    }

    #[test]
    fn warning_texts_mention_key_points() {
        assert!(auth_login_warning("TestProvider").contains("one key per machine"));
        assert!(parallel_sessions_warning(4, "TestProvider").contains("--i-understand-the-risks"));
        assert!(concurrency_cap_warning(8, 2, "TestProvider").contains("--i-understand-the-risks"));
        assert!(repeated_429_warning(4, "TestProvider").contains("backing off"));
        let q = quota_exhausted_error("TestProvider", "insufficient_quota");
        assert!(q.contains("will NOT retry"));
        assert!(first_429_notice("TestProvider").contains("normal"));
    }

    #[test]
    fn registry_register_counts_others() {
        let mut reg = SessionRegistry::default();
        let others1 = reg.entries.len();
        let _ = others1;

        let o1 = {
            let key_map = reg.entries.entry("p".into()).or_default();
            let s = key_map.entry("k".into()).or_default();
            s.push(("s1".into(), now_unix()));
            s.len().saturating_sub(1)
        };
        assert_eq!(o1, 0);

        let o2 = {
            let sessions = reg.entries.get_mut("p").unwrap().get_mut("k").unwrap();
            sessions.push(("s2".into(), now_unix()));
            sessions.len().saturating_sub(1)
        };
        assert_eq!(o2, 1);
    }

    /// Serialises the registry tests that point `REGISTRY_PATH_ENV` at a
    /// temp file: the env var is process-global, so two such tests on
    /// different threads would otherwise steal each other's path.
    static REGISTRY_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fresh temp dir for one registry test, removed by the caller.
    fn temp_registry_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zen-reg-356-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// RAII guard restoring one env var's previous value on drop
    /// (panic-safe). Same shape as the TestEnvGuard in zen-mcp tests.
    struct TestEnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl TestEnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: the var is unique to this test; no other test reads
            // it, and the old value is restored on drop.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as `set` above.
            unsafe {
                match self.old.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Child entry point for
    /// `registry_concurrent_registrations_from_two_processes_keep_both`:
    /// when `ZEN_REGISTRY_CHILD_SESSION` is set, load the registry, wait
    /// so the sibling process loads the same snapshot, then register.
    /// Not a real test on its own - without the env var it does nothing.
    #[test]
    fn registry_child_register() {
        let Ok(session) = std::env::var("ZEN_REGISTRY_CHILD_SESSION") else {
            return;
        };
        let mut reg = SessionRegistry::load();
        // Let the sibling load the same snapshot before either saves, so
        // the lost-registration race is deterministic, not timing luck.
        std::thread::sleep(std::time::Duration::from_millis(200));
        reg.register("test-provider", "test-key", &session);
    }

    #[test]
    fn registry_concurrent_registrations_from_two_processes_keep_both() {
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Issue #356: two processes registering at the same time must not
        // lose each other's session. On the old code both children load
        // the same (empty) snapshot and the last save wins, so one
        // registration is lost.
        let dir = std::env::temp_dir().join(format!(
            "zen-reg-356-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);

        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for session in ["s1", "s2"] {
            let child = std::process::Command::new(&exe)
                .arg("--exact")
                .arg("safety::tests::registry_child_register")
                .env(REGISTRY_PATH_ENV, &path)
                .env("ZEN_REGISTRY_CHILD_SESSION", session)
                .spawn()
                .unwrap();
            children.push(child);
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "child process failed");
        }

        let reg = SessionRegistry::load();
        let sessions = reg
            .entries
            .get("test-provider")
            .and_then(|m| m.get("test-key"))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids.len(),
            2,
            "both concurrent registrations must survive, got {ids:?}"
        );
        assert!(ids.contains(&"s1"), "s1 lost: {ids:?}");
        assert!(ids.contains(&"s2"), "s2 lost: {ids:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_register_returns_other_session_count() {
        // Kills the whole-body -> 0 / -> 1 mutants on `register`: the
        // return is the number of OTHER sessions under the same
        // provider + key, not a constant.
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_registry_dir("count");
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);
        let mut reg = SessionRegistry::load();
        assert_eq!(reg.register("p", "k", "s1"), 0);
        assert_eq!(reg.register("p", "k", "s2"), 1);
        assert_eq!(reg.register("p", "k", "s1"), 1);
        assert_eq!(reg.register("p", "k2", "s3"), 0);
        let reg2 = SessionRegistry::load();
        let sessions = reg2
            .entries
            .get("p")
            .and_then(|m| m.get("k"))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"s1") && ids.contains(&"s2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_unregister_removes_only_that_session() {
        // Kills the whole-body -> () mutant on `unregister`: without
        // it, the session stays registered.
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_registry_dir("unreg");
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);
        let mut reg = SessionRegistry::load();
        reg.register("p", "k", "s1");
        reg.register("p", "k", "s2");
        reg.unregister("p", "k", "s1");
        let reg2 = SessionRegistry::load();
        let sessions = reg2
            .entries
            .get("p")
            .and_then(|m| m.get("k"))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["s2"], "only s1 must go, got {ids:?}");
        // Unregistering a missing session changes nothing.
        let mut reg3 = SessionRegistry::load();
        reg3.unregister("p", "k", "nope");
        let reg4 = SessionRegistry::load();
        let sessions4 = reg4
            .entries
            .get("p")
            .and_then(|m| m.get("k"))
            .cloned()
            .unwrap_or_default();
        let ids4: Vec<&str> = sessions4.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids4, vec!["s2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_count_others_counts_without_persisting() {
        // Direct unit test for the fail-closed return of `register`:
        // counts others in the in-memory snapshot, without touching disk.
        let mut reg = SessionRegistry::default();
        assert_eq!(reg.count_others("p", "k", "s1"), 0);
        reg.entries
            .entry("p".to_string())
            .or_default()
            .entry("k".to_string())
            .or_default()
            .push(("s1".to_string(), now_unix()));
        assert_eq!(reg.count_others("p", "k", "s1"), 0);
        assert_eq!(reg.count_others("p", "k", "s2"), 1);
        reg.entries
            .get_mut("p")
            .unwrap()
            .get_mut("k")
            .unwrap()
            .push(("s2".to_string(), now_unix()));
        assert_eq!(reg.count_others("p", "k", "s1"), 1);
        assert_eq!(reg.count_others("p", "k", "s3"), 2);
        assert_eq!(reg.count_others("p", "missing", "s1"), 0);
    }

    #[test]
    fn registry_register_writes_to_configured_path() {
        // Issue #356 follow-up: `registry_path` must honour
        // ZEN_SESSION_REGISTRY_PATH. Kills the `delete !` mutant,
        // which makes a set-but-non-empty path fall through to the
        // default location.
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_registry_dir("path");
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);
        let mut reg = SessionRegistry::load();
        reg.register("p", "k", "s1");
        let data = std::fs::read_to_string(&path)
            .expect("registry file must exist at the configured path");
        assert!(
            data.contains("s1"),
            "configured registry file must contain the session, got: {data}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_contention_classifies_errors() {
        // Direct unit test for the lock-error guard: Unix contention is
        // WouldBlock, Windows lock violation is raw OS error 33. Kills
        // the || -> && and == -> != mutants on both comparisons.
        use std::io::{Error, ErrorKind};
        // Raw 11 is EAGAIN (WouldBlock) only on Unix. On Windows it is
        // ERROR_BAD_FORMAT, which classifies as GiveUp, so this assert
        // is Unix-only; the WouldBlock-kind assert below covers Retry
        // on every platform.
        #[cfg(unix)]
        assert!(matches!(
            classify_lock_error(&Error::from_raw_os_error(11)),
            LockWait::Retry
        ));
        assert!(matches!(
            classify_lock_error(&Error::from_raw_os_error(33)),
            LockWait::Retry
        ));
        assert!(matches!(
            classify_lock_error(&Error::new(ErrorKind::WouldBlock, "busy")),
            LockWait::Retry
        ));
        assert!(matches!(
            classify_lock_error(&Error::from_raw_os_error(13)),
            LockWait::GiveUp
        ));
        assert!(matches!(
            classify_lock_error(&Error::new(ErrorKind::PermissionDenied, "denied")),
            LockWait::GiveUp
        ));
    }

    #[test]
    fn registry_save_uses_unique_temp_paths() {
        // The temp file of one save must never be another save's temp
        // file: kills the whole-body -> Default mutant on
        // `unique_tmp_path` (both calls would return the same empty path).
        let dir = temp_registry_dir("tmp");
        let path = dir.join("session-registry.json");
        let first = unique_tmp_path(&path);
        let second = unique_tmp_path(&path);
        assert_ne!(first, second, "two saves must not share a temp file");
        assert_ne!(
            first.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some("session-registry.json.tmp".to_string()),
            "temp file must not be the old fixed shared name"
        );
        assert!(first.starts_with(&dir) && second.starts_with(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #356 follow-up: when the registry lock is held by another
    /// process, register must fail closed after the bounded wait instead
    /// of saving a stale snapshot (which would wipe the other process's
    /// registration). Mirrors `locked_store_fails_closed_after_bounded_wait`
    /// in zen-core (issue #319).
    #[test]
    fn registry_locked_register_fails_closed_after_bounded_wait() {
        use fs2::FileExt;
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_registry_dir("locked");
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);
        // Seed a peer registration while the lock is free.
        let mut peer = SessionRegistry::load();
        peer.register("test-provider", "test-key", "peer");
        // A stale snapshot taken before the peer registered: this is the
        // snapshot a racing process would save on the old fallback.
        let mut stale = SessionRegistry::default();
        // Hold the lock from outside the registry API, as another
        // process would.
        let lock_path = registry_lock_path(&path);
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held.lock_exclusive().unwrap();
        let start = std::time::Instant::now();
        let others = stale.register("test-provider", "test-key", "ours");
        // The wait is bounded (10s) and actually waited, not instant.
        assert!(start.elapsed() >= std::time::Duration::from_secs(9));
        drop(held);
        // Fail closed: our session was NOT persisted, and the peer survived.
        assert_eq!(others, 0, "stale snapshot has no others to count");
        let reg = SessionRegistry::load();
        let sessions = reg
            .entries
            .get("test-provider")
            .and_then(|m| m.get("test-key"))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.contains(&"peer"),
            "peer registration must survive a lock-timeout register, got {ids:?}"
        );
        assert!(
            !ids.contains(&"ours"),
            "timed-out register must not persist, got {ids:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #356 follow-up: two processes registering while the lock
    /// is held must not corrupt the registry: both time out, neither
    /// persists, and the peer registration plus a parseable file survive.
    #[test]
    fn registry_concurrent_fallback_registrations_keep_peer_and_valid_file() {
        use fs2::FileExt;
        let _test_lock = REGISTRY_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_registry_dir("fallback");
        let path = dir.join("session-registry.json");
        let _guard = TestEnvGuard::set(REGISTRY_PATH_ENV, &path);
        let mut peer = SessionRegistry::load();
        peer.register("test-provider", "test-key", "peer");
        // Hold the lock so both children time out in register.
        let lock_path = registry_lock_path(&path);
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held.lock_exclusive().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for session in ["s1", "s2"] {
            let child = std::process::Command::new(&exe)
                .arg("--exact")
                .arg("safety::tests::registry_child_register")
                .env(REGISTRY_PATH_ENV, &path)
                .env("ZEN_REGISTRY_CHILD_SESSION", session)
                .spawn()
                .unwrap();
            children.push(child);
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "child process failed");
        }
        drop(held);
        // The file must still parse and still hold the peer; neither
        // timed-out child persisted.
        let data = std::fs::read_to_string(&path).expect("registry file must still exist");
        let reg: SessionRegistry =
            serde_json::from_str(&data).expect("registry file must still be valid JSON");
        let sessions = reg
            .entries
            .get("test-provider")
            .and_then(|m| m.get("test-key"))
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.contains(&"peer"),
            "peer must survive concurrent fallback registers, got {ids:?}"
        );
        assert!(
            !ids.contains(&"s1") && !ids.contains(&"s2"),
            "timed-out registers must not persist, got {ids:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
