//! C3 lock-poison graceful: a poisoned session lock must not crash
//! the session.
//!
//! The test poisons `ctx.todos` - a lock on the real session path -
//! runs a turn that writes the todo list through the recovered lock,
//! and asserts the turn succeeds and the transcript holds the turn's
//! records. A poisoned lock cannot be injected into the interactive
//! REPL binary from outside the process, so the session-level check
//! runs the real agent, recorder and transcript writer in-process.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use zen_agent::{Agent, AgentConfig};
use zen_cli::eventlog::{Recorder, wrap};
use zen_cli::session::{TranscriptWriter, load_history};
use zen_core::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "zen_c3_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Scripted {
    replies: Mutex<Vec<ChatResponse>>,
}

#[async_trait]
impl LlmBackend for Scripted {
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
        let mut r = self.replies.lock().unwrap();
        if r.is_empty() {
            return Err(anyhow::anyhow!("script exhausted"));
        }
        Ok(r.remove(0))
    }
}

fn reply_with_call() -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(
            None,
            Some(vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "TodoWrite".into(),
                    arguments: serde_json::to_string(&json!({"todos": [
                        {"content": "Write the report", "status": "in_progress", "activeForm": "Writing the report"}
                    ]}))
                    .unwrap(),
                },
            }]),
        ),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        finish_reason: Some("stop".into()),
    }
}

fn reply_text(text: &str) -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(Some(text.to_string()), None),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        finish_reason: Some("stop".into()),
    }
}

struct AllowAll;

impl PermissionChecker for AllowAll {
    fn check(&self, _: &str, _: &Value, _: bool, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// Poison `mutex` by locking it in a thread that panics while holding
/// the guard, then joining that thread.
fn poison<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
    let m = mutex.clone();
    let handle = std::thread::spawn(move || {
        let _guard = m.lock().unwrap();
        panic!("deliberate poison for the C3 test");
    });
    let _ = handle.join();
    assert!(mutex.is_poisoned());
}

#[tokio::test]
async fn c3_poisoned_todos_lock_session_survives_and_transcript_flushes() {
    let dir = tmp("session");
    let transcript = dir.join("sess.jsonl");
    let ctx = ToolContext::new(dir.clone(), "sess");

    // Poison a lock on the real session path before the turn runs.
    // The degraded flag is set by the first recovery, not by the
    // poison itself, so it is asserted after the turn.
    poison(&ctx.todos);

    let recorder = Recorder::new(transcript.clone());
    let scripted = Arc::new(Scripted {
        replies: Mutex::new(vec![reply_with_call(), reply_text("Tracked.")]),
    });
    let llm = wrap(scripted, Some(&recorder));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(zen_tools::TodoWriteTool));
    let config = AgentConfig {
        system_prompt: "SYSTEM".into(),
        auto_compact_threshold: None,
        ..Default::default()
    };
    let mut agent = Agent::new(
        llm,
        tools,
        Arc::new(AllowAll),
        None,
        Arc::new(NoHooks),
        Arc::new(NullSink),
        ctx,
        config,
    );
    agent.journal = Some(recorder.clone());

    let before = agent.messages().to_vec();
    let res = agent.run_turn("track this").await;
    assert!(!res.is_error, "{}", res.result);
    assert!(zen_core::lock_recovery::is_degraded());
    // The TodoWrite tool result is in the message history, not in the
    // turn's final text reply.
    let history = serde_json::to_string(agent.messages()).unwrap();
    assert!(history.contains("Write the report"), "{history}");

    // The recovered lock holds the written todos.
    let todos = agent
        .ctx
        .todos
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].content, "Write the report");

    // The turn persists to the transcript and reloads identically.
    let mut writer = TranscriptWriter::new(transcript.clone(), "sess", &dir);
    writer.persist_turn(&before, agent.messages()).unwrap();
    assert_eq!(load_history(&transcript).unwrap(), agent.messages());
    let text = std::fs::read_to_string(&transcript).unwrap();
    assert!(
        text.lines().count() >= 2,
        "transcript must hold the turn's records: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn c3_lock_or_recover_marks_degraded_session() {
    let mutex = Arc::new(Mutex::new(7u32));
    poison(&mutex);
    let guard = zen_core::lock_recovery::lock_or_recover(&mutex, "test value");
    assert_eq!(*guard, 7);
    assert!(zen_core::lock_recovery::is_degraded());
}
