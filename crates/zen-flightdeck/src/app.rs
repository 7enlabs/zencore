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

/// Keyboard state for the viewer loop, kept separate from the
/// terminal so the key handling is unit-testable without a tty:
/// every key the viewer understands is decided here, and the loop
/// only owns the terminal and the poll timeout.
#[derive(Default)]
struct KeyHandler {
    filter_editing: bool,
    filter_draft: String,
    /// Filter in force before the current edit, so `Esc` can restore
    /// it (the draft previews live in the state while editing).
    saved_filter: String,
}

impl KeyHandler {
    /// Handle one key press. Returns true when the viewer should exit.
    fn handle(&mut self, state: &Arc<Mutex<FlightdeckState>>, key: event::KeyEvent) -> bool {
        if self.filter_editing {
            match key.code {
                KeyCode::Esc => {
                    // Cancel: discard the draft, restore the old filter.
                    self.filter_editing = false;
                    self.filter_draft.clear();
                    lock_state(state).filter = std::mem::take(&mut self.saved_filter);
                }
                KeyCode::Enter => {
                    let draft = std::mem::take(&mut self.filter_draft);
                    lock_state(state).filter = draft;
                    self.filter_editing = false;
                }
                KeyCode::Backspace => {
                    self.filter_draft.pop();
                }
                KeyCode::Char(c) => {
                    self.filter_draft.push(c);
                }
                _ => {}
            }
            // Live preview of the draft in the footer while editing.
            if self.filter_editing {
                lock_state(state).filter = self.filter_draft.clone();
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::Tab => {
                let mut s = lock_state(state);
                s.focus = match s.focus {
                    Focus::Agents => Focus::Log,
                    Focus::Log => Focus::Agents,
                };
                false
            }
            KeyCode::Char('/') => {
                self.filter_editing = true;
                self.saved_filter = lock_state(state).filter.clone();
                self.filter_draft = self.saved_filter.clone();
                false
            }
            _ => false,
        }
    }
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
    let mut keys = KeyHandler::default();
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
        if keys.handle(&state, key) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use zen_core::PermissionMode;

    fn state() -> Arc<Mutex<FlightdeckState>> {
        Arc::new(Mutex::new(FlightdeckState::new(
            "sess-1",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Default,
            "auto",
        )))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn q_and_ctrl_c_exit() {
        let s = state();
        assert!(KeyHandler::default().handle(&s, key(KeyCode::Char('q'))));
        assert!(
            KeyHandler::default().handle(&s, key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn plain_c_does_not_exit() {
        let s = state();
        assert!(!KeyHandler::default().handle(&s, key(KeyCode::Char('c'))));
    }

    #[test]
    fn tab_switches_focus_back_and_forth() {
        let s = state();
        let mut h = KeyHandler::default();
        assert_eq!(lock_state(&s).focus, Focus::Agents);
        assert!(!h.handle(&s, key(KeyCode::Tab)));
        assert_eq!(lock_state(&s).focus, Focus::Log);
        assert!(!h.handle(&s, key(KeyCode::Tab)));
        assert_eq!(lock_state(&s).focus, Focus::Agents);
    }

    #[test]
    fn filter_editing_flow() {
        let s = state();
        let mut h = KeyHandler::default();
        // Open the filter editor and type: the draft previews live.
        assert!(!h.handle(&s, key(KeyCode::Char('/'))));
        assert!(!h.handle(&s, key(KeyCode::Char('x'))));
        assert_eq!(lock_state(&s).filter, "x");
        assert!(!h.handle(&s, key(KeyCode::Backspace)));
        assert_eq!(lock_state(&s).filter, "");
        assert!(!h.handle(&s, key(KeyCode::Char('y'))));
        // Enter commits the draft.
        assert!(!h.handle(&s, key(KeyCode::Enter)));
        assert_eq!(lock_state(&s).filter, "y");
        // Esc while editing cancels and restores the previous filter.
        assert!(!h.handle(&s, key(KeyCode::Char('/'))));
        assert!(!h.handle(&s, key(KeyCode::Char('z'))));
        assert_eq!(lock_state(&s).filter, "yz");
        assert!(!h.handle(&s, key(KeyCode::Esc)));
        assert_eq!(lock_state(&s).filter, "y");
        // q while editing the filter types 'q'; it does not exit.
        assert!(!h.handle(&s, key(KeyCode::Char('/'))));
        assert!(!h.handle(&s, key(KeyCode::Char('q'))));
        assert_eq!(lock_state(&s).filter, "yq");
    }
}
