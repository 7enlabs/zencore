//! Flightdeck: live session dashboard (TUI) for Zen.
//!
//! Phase 1: a new `EventSink` that folds the existing `AgentEvent`
//! stream into dashboard state, plus a ratatui renderer (header with
//! ctx % / cost $ / request count / model, live agents table, session
//! log) and a scripted demo (`zen flightdeck --demo`).
//!
//! Clean-room rule: this crate names no outside agent product; prose
//! refers only to the "phòng sạch" (clean room) approach.

pub mod app;
pub mod demo;
pub mod render;
pub mod sink;
pub mod state;

pub use app::run_viewer;
pub use demo::run_demo;
pub use sink::{FanoutSink, FlightdeckSink};
pub use state::{AgentRow, AgentStatus, FlightdeckState, Focus};
