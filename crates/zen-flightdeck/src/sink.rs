//! Event sinks for the flightdeck dashboard.
//!
//! `FlightdeckSink` is one more `EventSink` beside the existing
//! `AcpSink`/`TextSink`/`ReplProgressSink`: it folds every `AgentEvent`
//! into the shared dashboard state. It never drops an event: a poisoned
//! state lock is recovered, not swallowed.
//!
//! `FanoutSink` forwards each event to several sinks, so the dashboard
//! can run alongside the existing sinks without replacing any of them.

use std::sync::{Arc, Mutex, MutexGuard};
use zen_core::{AgentEvent, EventSink};

use crate::state::FlightdeckState;

/// Recover a poisoned mutex instead of dropping the event that found
/// it poisoned. A poisoned lock means an earlier `emit` panicked; the
/// state is still usable, and losing the event would break the
/// no-drop guarantee the acceptance gate checks.
fn lock_or_recover<'a>(m: &'a Mutex<FlightdeckState>) -> MutexGuard<'a, FlightdeckState> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The dashboard's sink: every event updates the shared state.
pub struct FlightdeckSink {
    state: Arc<Mutex<FlightdeckState>>,
}

impl FlightdeckSink {
    /// Build a sink over shared state (the REPL and the viewer hold the
    /// same `Arc`, so the viewer sees live updates).
    pub fn new(state: Arc<Mutex<FlightdeckState>>) -> Arc<Self> {
        Arc::new(Self { state })
    }

    /// The shared state this sink writes to.
    pub fn state(&self) -> Arc<Mutex<FlightdeckState>> {
        self.state.clone()
    }
}

impl EventSink for FlightdeckSink {
    fn emit(&self, ev: AgentEvent) {
        lock_or_recover(&self.state).apply(&ev);
    }
}

/// Forward each event to every sink in order. The event is cloned per
/// sink (it is cheap next to the work the sinks do), so one sink
/// failing to keep up never starves the others.
pub struct FanoutSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl FanoutSink {
    pub fn new(sinks: Vec<Arc<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for FanoutSink {
    fn emit(&self, ev: AgentEvent) {
        for sink in &self.sinks {
            sink.emit(ev.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zen_core::{AgentEvent, EventSink};
    use zen_core::{ChatMessage, PermissionMode};

    /// A recording sink: the no-drop test compares what it saw with
    /// what the dashboard state holds.
    struct CollectSink {
        seen: Mutex<Vec<String>>,
    }

    impl CollectSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    impl EventSink for CollectSink {
        fn emit(&self, ev: AgentEvent) {
            let label = match &ev {
                AgentEvent::Assistant { .. } => "assistant",
                AgentEvent::ToolResult { .. } => "tool-result",
                AgentEvent::ToolProgress { .. } => "tool-progress",
                AgentEvent::SystemMessage { .. } => "system",
                AgentEvent::TextDelta { .. } => "delta",
                AgentEvent::ApiRetry { .. } => "retry",
                AgentEvent::CompactBoundary { .. } => "compact",
                AgentEvent::Pruned { .. } => "pruned",
                AgentEvent::PermissionDenied(_) => "denied",
                // `AgentEvent` is non-exhaustive: future variants are
                // recorded as "other" until the test learns them.
                _ => "other",
            };
            self.seen.lock().unwrap().push(label.to_string());
        }
    }

    fn assistant(text: &str) -> AgentEvent {
        AgentEvent::Assistant {
            message: ChatMessage::assistant(Some(text.to_string()), None),
            parent_tool_use_id: None,
        }
    }

    #[test]
    fn fanout_delivers_every_event_to_every_sink() {
        let state = Arc::new(Mutex::new(FlightdeckState::new(
            "sess-1",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Default,
            "auto",
        )));
        let flightdeck = FlightdeckSink::new(state.clone());
        let collector = CollectSink::new();
        let fanout = FanoutSink::new(vec![
            flightdeck as Arc<dyn EventSink>,
            collector.clone() as Arc<dyn EventSink>,
        ]);

        // A realistic burst: progress heartbeats, a tool result, an
        // assistant answer, a retry, a compaction marker, a delta.
        let events = vec![
            AgentEvent::ToolProgress {
                tool_use_id: "t1".to_string(),
                tool_name: "Read".to_string(),
                elapsed_ms: 120,
            },
            AgentEvent::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "file content".to_string(),
                is_error: false,
                parent_tool_use_id: None,
                parallel: false,
            },
            assistant("turn complete"),
            AgentEvent::ApiRetry {
                attempt: 1,
                max_retries: 3,
                retry_delay_ms: 200,
                error_status: Some(429),
                error: "busy".to_string(),
            },
            AgentEvent::CompactBoundary {
                trigger: "auto".to_string(),
                pre_tokens: 96_000,
                summary: "summary".to_string(),
            },
            AgentEvent::TextDelta {
                text: "chunk".to_string(),
            },
        ];
        for ev in &events {
            fanout.emit(ev.clone());
        }

        // The collector saw all six (it records every variant).
        assert_eq!(collector.seen.lock().unwrap().len(), 6);
        // The dashboard state reflects all five it keeps (TextDelta is
        // ignored by Leader decision, so no request and no log line
        // come from it).
        let s = state.lock().unwrap();
        assert_eq!(s.requests, 1);
        assert_eq!(s.visible_log().len(), 4);
        assert_eq!(s.agents[0].current_tool, None);
    }

    #[test]
    fn flightdeck_sink_survives_a_poisoned_state_lock() {
        let state = Arc::new(Mutex::new(FlightdeckState::new(
            "sess-1",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Default,
            "auto",
        )));
        // Poison the lock on purpose.
        let _ = std::panic::catch_unwind({
            let state = state.clone();
            move || {
                let _guard = state.lock().unwrap();
                panic!("simulated panic while holding the state lock");
            }
        });
        assert!(state.is_poisoned());
        let sink = FlightdeckSink::new(state.clone());
        sink.emit(assistant("after the poison"));
        // The event still landed: no drop. The poison flag stays set
        // on the mutex, so the lock must be recovered, not unwrapped.
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .requests,
            1
        );
    }
}
