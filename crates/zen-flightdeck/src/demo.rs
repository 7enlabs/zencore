//! Scripted demo: one task split into three simulated subagents.
//!
//! The demo drives the REAL `FlightdeckSink` and the REAL ratatui
//! renderer with a fixed event script (no live model, no network): it
//! proves the state machine and the live rendering end to end, and it
//! is the manual check for acceptance points 1-3. The scripted agents
//! are labelled `demo-*` so a demo frame is never mistaken for a real
//! session.
//!
//! Numbers in the demo are derived from the scripted event stream
//! itself: token use comes from the production `estimate_tokens`
//! applied to the scripted messages, and the cost is $0.00 because the
//! demo model is the free default (`deepseek-v4.1-flash`, priced $0 in
//! the repo's own table). Cost correctness for priced models is covered
//! by the state unit tests and by the real-subagent integration test,
//! not by this script.

use std::io;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use zen_core::{AgentEvent, ChatMessage, PermissionMode, UsageStats};

use crate::app::run_viewer;
use crate::sink::FlightdeckSink;
use crate::state::{DEFAULT_CONTEXT_WINDOW, FlightdeckState};

/// Run the scripted demo. The viewer stays open until `q` is pressed,
/// unless `auto_exit_secs > 0`, in which case it exits that many
/// seconds after the script finishes (for unattended runs).
pub fn run_demo(auto_exit_secs: u64) -> io::Result<()> {
    let state = Arc::new(Mutex::new(FlightdeckState::new(
        "demo-session",
        "deepseek-v4.1-flash",
        DEFAULT_CONTEXT_WINDOW,
        PermissionMode::Default,
        "auto",
    )));
    let sink = FlightdeckSink::new(state.clone());
    let done = Arc::new(AtomicBool::new(false));
    let feeder_done = done.clone();
    let feeder_state = state.clone();
    thread::spawn(move || {
        run_scenario(&sink, &feeder_state);
        if auto_exit_secs > 0 {
            thread::sleep(Duration::from_secs(auto_exit_secs));
            feeder_done.store(true, Ordering::SeqCst);
        }
    });
    run_viewer(state, Some(done))
}

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

/// Promote the live estimate to the header's exact numbers, as the
/// REPL does after a real turn. The cost is $0.00: the demo model is
/// the free default.
fn note_demo_turn(state: &Arc<Mutex<FlightdeckState>>) {
    let used = state.lock().map(|s| s.used_tokens).unwrap_or(0);
    let usage = UsageStats {
        input_tokens: used,
        output_tokens: 0,
        cache_read_input_tokens: 0,
    };
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .note_turn_result(&usage, Some(0.0));
}

fn assistant(text: &str, parent: Option<&str>) -> AgentEvent {
    AgentEvent::Assistant {
        message: ChatMessage::assistant(Some(text.to_string()), None),
        parent_tool_use_id: parent.map(str::to_string),
    }
}

fn tool_progress(tool_use_id: &str, tool_name: &str, elapsed_ms: u64) -> AgentEvent {
    AgentEvent::ToolProgress {
        tool_use_id: tool_use_id.to_string(),
        tool_name: tool_name.to_string(),
        elapsed_ms,
    }
}

fn tool_result(tool_use_id: &str, content: &str, parent: Option<&str>) -> AgentEvent {
    AgentEvent::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: content.to_string(),
        is_error: false,
        parent_tool_use_id: parent.map(str::to_string),
        parallel: false,
    }
}

/// The script: one task, three subagents, finishing at different
/// times so the live transitions are visible.
fn run_scenario(sink: &Arc<FlightdeckSink>, state: &Arc<Mutex<FlightdeckState>>) {
    use zen_core::EventSink;
    let emit = |ev: AgentEvent| sink.emit(ev);

    // The main agent splits the task.
    emit(assistant("Chia task thành 3 phần, chạy song song.", None));
    note_demo_turn(state);
    sleep_ms(500);

    // Three delegate calls start: three rows appear at once.
    for (id, ms) in [("demo-alpha", 120), ("demo-beta", 180), ("demo-gamma", 240)] {
        emit(tool_progress(id, "delegate", ms));
    }
    sleep_ms(400);

    // All three start reading.
    for (tool, ms) in [
        ("demo-tool-a", 300),
        ("demo-tool-b", 300),
        ("demo-tool-c", 300),
    ] {
        emit(tool_progress(tool, "Read", ms));
    }
    sleep_ms(600);

    // Alpha finishes first.
    emit(tool_result(
        "demo-tool-a",
        "nội dung phần một",
        Some("demo-alpha"),
    ));
    emit(assistant(
        "Xong phần một: tóm tắt ngắn.",
        Some("demo-alpha"),
    ));
    note_demo_turn(state);
    emit(tool_result(
        "demo-alpha",
        "Kết quả phần một: đã rà xong",
        None,
    ));
    sleep_ms(500);

    // Beta finishes second.
    emit(tool_progress("demo-tool-b2", "Grep", 700));
    emit(tool_result(
        "demo-tool-b",
        "nội dung phần hai",
        Some("demo-beta"),
    ));
    emit(assistant("Xong phần hai: tóm tắt ngắn.", Some("demo-beta")));
    note_demo_turn(state);
    emit(tool_result(
        "demo-beta",
        "Kết quả phần hai: đã rà xong",
        None,
    ));
    sleep_ms(500);

    // Gamma finishes last.
    emit(tool_progress("demo-tool-c2", "Read", 900));
    emit(tool_result(
        "demo-tool-c",
        "nội dung phần ba",
        Some("demo-gamma"),
    ));
    emit(assistant("Xong phần ba: tóm tắt ngắn.", Some("demo-gamma")));
    note_demo_turn(state);
    emit(tool_result(
        "demo-gamma",
        "Kết quả phần ba: đã rà xong",
        None,
    ));
    sleep_ms(400);

    // The main agent merges the three results.
    emit(assistant("Đã gộp 3 kết quả thành bản cuối.", None));
    note_demo_turn(state);
}
