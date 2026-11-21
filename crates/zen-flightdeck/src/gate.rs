//! Per-command approval through the flightdeck UI (Phase 2).
//!
//! When the flightdeck viewer is the session's UI, an approval
//! question (GATE bucket `Classifier`) is answered there instead of
//! on the terminal: the agent loop emits `PermissionClassified`
//! (the sink shows the pending block), then the prompter waits on
//! the [`GateBridge`] until the viewer loop delivers the user's key
//! (`y` once, `a` always, `n` no). There is no timeout (D5): if the
//! viewer closes while a command waits, the bridge answers Deny.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;
use zen_core::lock_recovery::lock_or_recover;
use zen_core::{AsyncPrompter, PromptAnswer};

/// The user's answer to a pending approval, as pressed in the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAnswer {
    /// Run this once (`y`).
    Once,
    /// Run and record an allow rule (`a`).
    Always,
    /// Do not run (`n`, or the viewer closed while waiting).
    Deny,
}

/// Shared channel between the viewer loop (which owns the keyboard)
/// and the prompter (which waits for the answer). At most one
/// approval can be pending at a time: the agent asks one question
/// at a time, and a second registration is refused.
#[derive(Default)]
pub struct GateBridge {
    pending: Mutex<Option<oneshot::Sender<GateAnswer>>>,
    viewer_open: AtomicBool,
}

impl GateBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the flightdeck viewer is currently on screen. The
    /// CLI routes approval questions to the terminal while the
    /// viewer is closed.
    pub fn viewer_open(&self) -> bool {
        self.viewer_open.load(Ordering::SeqCst)
    }

    pub fn set_viewer_open(&self, open: bool) {
        self.viewer_open.store(open, Ordering::SeqCst);
    }

    /// Register a waiting approval. Returns false when another
    /// approval is already pending.
    fn register(&self, tx: oneshot::Sender<GateAnswer>) -> bool {
        let mut slot = lock_or_recover(&self.pending, "gate pending");
        if slot.is_some() {
            return false;
        }
        *slot = Some(tx);
        true
    }

    /// Deliver the user's answer. Returns false when nobody waits.
    pub fn answer(&self, answer: GateAnswer) -> bool {
        let tx = lock_or_recover(&self.pending, "gate pending").take();
        match tx {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// D5: the UI went away while a command waited — deny it.
    pub fn deny_pending(&self) {
        self.answer(GateAnswer::Deny);
    }
}

/// Computes the allow rule recorded for an `Always` answer. Injected
/// by the CLI (backed by the permissions module's rule suggester) so
/// this crate stays free of rule syntax.
pub type RuleSuggester = Arc<dyn Fn(&str, &Value) -> Vec<String> + Send + Sync>;

/// The `AsyncPrompter` backed by the flightdeck viewer. Installed
/// by the CLI behind a composite that only routes here while the
/// viewer is open; the closed-viewer guard below is defence in depth
/// so a misroute denies instead of hanging forever.
pub struct FlightdeckPrompter {
    bridge: Arc<GateBridge>,
    suggester: RuleSuggester,
}

impl FlightdeckPrompter {
    pub fn new(bridge: Arc<GateBridge>, suggester: RuleSuggester) -> Self {
        Self { bridge, suggester }
    }
}

#[async_trait]
impl AsyncPrompter for FlightdeckPrompter {
    async fn ask_permission(
        &self,
        tool_name: &str,
        input: &Value,
        _tool_call_id: &str,
        _read_only: bool,
    ) -> PromptAnswer {
        if !self.bridge.viewer_open() {
            return PromptAnswer::No;
        }
        let (tx, rx) = oneshot::channel();
        if !self.bridge.register(tx) {
            return PromptAnswer::No;
        }
        match rx.await {
            Ok(GateAnswer::Once) => PromptAnswer::Yes,
            Ok(GateAnswer::Always) => {
                let rules = (self.suggester)(tool_name, input);
                if rules.is_empty() {
                    PromptAnswer::Always(tool_name.to_string())
                } else {
                    PromptAnswer::Always(rules.join("\n"))
                }
            }
            Ok(GateAnswer::Deny) | Err(_) => PromptAnswer::No,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompter(bridge: &Arc<GateBridge>) -> FlightdeckPrompter {
        bridge.set_viewer_open(true);
        FlightdeckPrompter::new(
            bridge.clone(),
            Arc::new(|tool: &str, _input: &Value| vec![format!("{tool}(suggested)")]),
        )
    }

    #[tokio::test]
    async fn closed_viewer_denies_immediately() {
        let bridge = Arc::new(GateBridge::new());
        let p = FlightdeckPrompter::new(bridge, Arc::new(|_, _| vec![]));
        let a = p
            .ask_permission("Bash", &serde_json::json!({}), "c1", false)
            .await;
        assert_eq!(a, PromptAnswer::No);
    }

    #[tokio::test]
    async fn answer_once_maps_to_yes() {
        let bridge = Arc::new(GateBridge::new());
        let p = prompter(&bridge);
        let b = bridge.clone();
        let handle = tokio::spawn(async move {
            p.ask_permission("Bash", &serde_json::json!({}), "c1", false)
                .await
        });
        // Let the ask register, then answer from the "viewer".
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(bridge.answer(GateAnswer::Once));
        assert_eq!(handle.await.unwrap(), PromptAnswer::Yes);
        let _ = b;
    }

    #[tokio::test]
    async fn answer_always_uses_the_suggester() {
        let bridge = Arc::new(GateBridge::new());
        let p = prompter(&bridge);
        let handle = tokio::spawn(async move {
            p.ask_permission("Bash", &serde_json::json!({}), "c1", false)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(bridge.answer(GateAnswer::Always));
        assert_eq!(
            handle.await.unwrap(),
            PromptAnswer::Always("Bash(suggested)".to_string())
        );
    }

    #[tokio::test]
    async fn viewer_close_denies_the_waiting_call() {
        let bridge = Arc::new(GateBridge::new());
        let p = prompter(&bridge);
        let handle = tokio::spawn(async move {
            p.ask_permission("Bash", &serde_json::json!({}), "c1", false)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bridge.deny_pending();
        assert_eq!(handle.await.unwrap(), PromptAnswer::No);
    }

    #[tokio::test]
    async fn second_concurrent_ask_is_refused() {
        let bridge = Arc::new(GateBridge::new());
        let p = prompter(&bridge);
        let first = tokio::spawn(async move {
            p.ask_permission("Bash", &serde_json::json!({}), "c1", false)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let p2 = prompter(&bridge);
        let a2 = p2
            .ask_permission("Bash", &serde_json::json!({}), "c2", false)
            .await;
        assert_eq!(a2, PromptAnswer::No);
        bridge.deny_pending();
        assert_eq!(first.await.unwrap(), PromptAnswer::No);
    }
}
