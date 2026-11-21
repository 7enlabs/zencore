//! `zen flightdeck` subcommand.
//!
//! The only standalone mode in Phase 1 is the scripted demo
//! (`--demo`): one task split into three simulated subagents, driving
//! the real sink and renderer. The live viewer over a real session
//! runs from the REPL (`/flightdeck`).

use anyhow::Result;

/// Run `zen flightdeck`. Without `--demo` there is nothing to show
/// standalone: point at the REPL command instead.
pub async fn run_flightdeck(demo: bool, demo_seconds: u64) -> Result<i32> {
    if !demo {
        eprintln!(
            "Flightdeck runs inside a session: open the REPL and use /flightdeck, \
             or run the scripted demo with: zen flightdeck --demo"
        );
        return Ok(2);
    }
    tokio::task::spawn_blocking(move || zen_flightdeck::run_demo(demo_seconds)).await??;
    Ok(0)
}
