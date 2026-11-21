//! Terminal event loop for the flightdeck viewer.
//!
//! The viewer owns the terminal (raw mode + alternate screen) while it
//! is open and restores it on every exit path, so the REPL underneath
//! keeps working. Keys (Leader decision): `q` exits, `Tab` switches
//! focus between the agents table and the log, `/` filters the log.
//! The loop polls for keys with a short timeout so live state updates
//! repaint even when no key is pressed.

use std::io;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::render::draw;
use crate::state::{FlightdeckState, Focus};

/// Restores the terminal (alternate screen off, cooked mode back on)
/// when dropped, so a panic or early return never leaves the terminal
/// broken.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Lock the shared state, recovering a poisoned lock instead of
/// failing: a poisoned lock means an earlier `emit` panicked, and the
/// viewer must keep working.
fn lock_state(state: &Mutex<FlightdeckState>) -> std::sync::MutexGuard<'_, FlightdeckState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run the viewer over shared state until `q` (or Ctrl-C) is pressed.
/// When `exit_when` is set, the viewer also exits once that flag is
/// set (used by the scripted demo for unattended runs).
pub fn run_viewer(
    state: Arc<Mutex<FlightdeckState>>,
    exit_when: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut filter_editing = false;
    let mut filter_draft = String::new();
    // Filter in force before the current edit, so `Esc` can restore it
    // (the draft previews live in the state while editing).
    let mut saved_filter = String::new();
    loop {
        if let Some(done) = &exit_when
            && done.load(Ordering::SeqCst)
        {
            break;
        }
        {
            let snapshot = lock_state(&state).clone();
            terminal.draw(|frame| draw(frame, &snapshot))?;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if filter_editing {
            match key.code {
                KeyCode::Esc => {
                    // Cancel: discard the draft, restore the old filter.
                    filter_editing = false;
                    filter_draft.clear();
                    lock_state(&state).filter = std::mem::take(&mut saved_filter);
                }
                KeyCode::Enter => {
                    let draft = std::mem::take(&mut filter_draft);
                    lock_state(&state).filter = draft;
                    filter_editing = false;
                }
                KeyCode::Backspace => {
                    filter_draft.pop();
                }
                KeyCode::Char(c) => {
                    filter_draft.push(c);
                }
                _ => {}
            }
            // Live preview of the draft in the footer while editing.
            if filter_editing {
                lock_state(&state).filter = filter_draft.clone();
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Tab => {
                let mut s = lock_state(&state);
                s.focus = match s.focus {
                    Focus::Agents => Focus::Log,
                    Focus::Log => Focus::Agents,
                };
            }
            KeyCode::Char('/') => {
                filter_editing = true;
                saved_filter = lock_state(&state).filter.clone();
                filter_draft = saved_filter.clone();
            }
            _ => {}
        }
    }
    Ok(())
}
