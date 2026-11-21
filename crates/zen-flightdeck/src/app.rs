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

use crate::gate::{GateAnswer, GateBridge};
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
    /// Approval bridge (Phase 2): set when the viewer can answer a
    /// pending per-command approval with `y` / `a` / `n`.
    gate: Option<Arc<GateBridge>>,
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
        // Phase 2: while a command waits for approval, `y` / `a` /
        // `n` answer it and mean nothing else.
        if let Some(bridge) = &self.gate
            && lock_state(state).pending_gate.is_some()
        {
            match key.code {
                KeyCode::Char('y') => {
                    bridge.answer(GateAnswer::Once);
                    return false;
                }
                KeyCode::Char('a') => {
                    bridge.answer(GateAnswer::Always);
                    return false;
                }
                KeyCode::Char('n') => {
                    bridge.answer(GateAnswer::Deny);
                    return false;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
            KeyCode::Tab => {
                let mut s = lock_state(state);
                s.focus = match s.focus {
                    Focus::Agents => Focus::Gate,
                    Focus::Gate => Focus::Log,
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

/// Whether this key event is a press: releases and repeats never
/// act, so holding `q` cannot exit the viewer twice over. Split out
/// so the press/release boundary is unit-testable without a tty.
fn is_press(key: event::KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
}

/// Run the viewer over shared state until `q` (or Ctrl-C) is pressed.
/// When `exit_when` is set, the viewer also exits once that flag is
/// set (used by the scripted demo for unattended runs). While the
/// viewer is open it owns per-command approvals through `gate`
/// (Phase 2); closing the viewer denies any command still waiting
/// (D5).
pub fn run_viewer(
    state: Arc<Mutex<FlightdeckState>>,
    gate: Option<Arc<GateBridge>>,
    exit_when: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    if let Some(bridge) = &gate {
        bridge.set_viewer_open(true);
    }
    let mut keys = KeyHandler {
        gate: gate.clone(),
        ..Default::default()
    };
    let result = viewer_loop(&mut terminal, &state, &mut keys, &exit_when);
    if let Some(bridge) = &gate {
        bridge.set_viewer_open(false);
        bridge.deny_pending();
    }
    result
}

fn viewer_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &Arc<Mutex<FlightdeckState>>,
    keys: &mut KeyHandler,
    exit_when: &Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    loop {
        if let Some(done) = &exit_when
            && done.load(Ordering::SeqCst)
        {
            break;
        }
        {
            let snapshot = lock_state(state).clone();
            terminal.draw(|frame| draw(frame, &snapshot))?;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !is_press(key) {
            continue;
        }
        if keys.handle(state, key) {
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
            "nvidia/nemotron-3-ultra-550b-a55b:free",
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
        assert_eq!(lock_state(&s).focus, Focus::Gate);
        assert!(!h.handle(&s, key(KeyCode::Tab)));
        assert_eq!(lock_state(&s).focus, Focus::Log);
        assert!(!h.handle(&s, key(KeyCode::Tab)));
        assert_eq!(lock_state(&s).focus, Focus::Agents);
    }

    #[test]
    fn gate_keys_answer_a_pending_approval() {
        use crate::state::PendingGate;
        let s = state();
        lock_state(&s).pending_gate = Some(PendingGate {
            tool_use_id: "c1".to_string(),
            tool_name: "Bash".to_string(),
            summary: "git reset --hard".to_string(),
            reason_code: "warn.git-destructive".to_string(),
        });
        let bridge = Arc::new(GateBridge::new());
        let mut h = KeyHandler {
            gate: Some(bridge.clone()),
            ..Default::default()
        };
        // 'n' answers Deny through the bridge (nobody waits here, so
        // the bridge reports false, but the key is consumed as an
        // answer, not as anything else).
        assert!(!h.handle(&s, key(KeyCode::Char('n'))));
        // With no pending approval, 'y' falls through and does nothing.
        lock_state(&s).pending_gate = None;
        assert!(!h.handle(&s, key(KeyCode::Char('y'))));
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

    fn key_with_kind(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent { kind, ..key(code) }
    }

    #[test]
    fn only_key_presses_act() {
        assert!(is_press(key_with_kind(
            KeyCode::Char('q'),
            KeyEventKind::Press
        )));
        assert!(!is_press(key_with_kind(
            KeyCode::Char('q'),
            KeyEventKind::Release
        )));
        assert!(!is_press(key_with_kind(
            KeyCode::Char('q'),
            KeyEventKind::Repeat
        )));
    }

    /// Helper run only under a pty by `viewer_draws_and_restores_terminal`:
    /// it opens the real viewer and waits for `q`. The parent test sets
    /// FLIGHTDECK_PTY_HELPER=1 and runs this under `script`, so stdin is
    /// a terminal there. In a normal suite run the marker is absent (and
    /// stdin is piped), so this returns at once instead of blocking on a
    /// key that will never come.
    #[test]
    fn viewer_pty_helper() {
        if std::env::var_os("FLIGHTDECK_PTY_HELPER").is_none() {
            return;
        }
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return;
        }
        let s = state();
        let done = Arc::new(AtomicBool::new(false));
        run_viewer(s, None, Some(done)).expect("viewer failed under pty");
    }

    /// The viewer draws a frame and restores the terminal on exit, run
    /// under a pty (`script`) so raw mode works. The drawn frame kills
    /// the run_viewer -> Ok(()) mutant; the LeaveAlternateScreen bytes
    /// kill the TerminalGuard Drop-deletion mutant. A reader thread
    /// answers the cursor-position (DSR) queries the viewer's
    /// Terminal::clear() sends: `script` is not a terminal emulator and
    /// never replies, so without the answer clear() times out after 2s.
    #[test]
    #[cfg(target_os = "linux")]
    fn viewer_draws_and_restores_terminal() {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        let exe = std::env::current_exe().expect("current test exe");
        let mut child = Command::new("script")
            .arg("-qec")
            .arg(format!(
                "stty rows 24 cols 100; exec {} viewer_pty_helper --nocapture",
                exe.display()
            ))
            .arg("/dev/null")
            .env("FLIGHTDECK_PTY_HELPER", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn `script` (util-linux) for the pty viewer test");
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("pty stdin")));
        let mut stdout = child.stdout.take().expect("pty stdout");
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let reader_out = Arc::clone(&output);
        let reader_stdin = Arc::clone(&stdin);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut answered = 0usize;
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let queries = {
                            let mut out = reader_out.lock().expect("output lock");
                            out.extend_from_slice(&buf[..n]);
                            out.windows(4).filter(|w| *w == b"\x1b[6n").count()
                        };
                        while answered < queries {
                            answered += 1;
                            if let Ok(mut s) = reader_stdin.lock() {
                                let _ = s.write_all(b"\x1b[1;1R");
                                let _ = s.flush();
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        // Press q again and again until the child exits: a key that
        // arrives while the viewer polls must be read, not skipped.
        // This kills the delete-! mutant on event::poll: with the `!`
        // gone, a ready key makes the loop spin on continue instead
        // of reading it, so the viewer never exits. Keep stdin open
        // until the child exits: closing it early could let `script`
        // exit before the viewer has drawn.
        let stop_sending = Arc::new(AtomicBool::new(false));
        let sender_stop = Arc::clone(&stop_sending);
        let sender_stdin = Arc::clone(&stdin);
        let sender = std::thread::spawn(move || {
            while !sender_stop.load(Ordering::SeqCst) {
                if let Ok(mut s) = sender_stdin.lock() {
                    let _ = s.write_all(b"q");
                    let _ = s.flush();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        // Stop the q-sender when the test ends, even on panic: without
        // this a failing test would leave the sender thread writing
        // forever and the test binary would never exit.
        struct StopSender {
            stop: Arc<AtomicBool>,
            handle: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for StopSender {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }
        let _sender_guard = StopSender {
            stop: stop_sending,
            handle: Some(sender),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("viewer did not exit on q within 15s");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("try_wait: {e}"),
            }
        };
        let _ = reader.join();
        drop(stdin);
        assert!(status.success(), "pty viewer failed, status {status}");
        let guard = output.lock().expect("output lock");
        let raw = String::from_utf8_lossy(&guard);
        // The frame writes text in pieces with cursor moves between
        // them ("SESSION" ... "LOG"), so assert on the text with the
        // escape sequences stripped out.
        let text = strip_ansi(&guard);
        assert!(text.contains("AGENTS LIVE"), "no frame drawn: {text}");
        assert!(text.contains("SESSION"), "no log block drawn: {text}");
        assert!(text.contains("LOG"), "no log block drawn: {text}");
        assert!(
            raw.contains("\u{1b}[?1049l"),
            "terminal not restored (no LeaveAlternateScreen): {raw}"
        );
    }
    /// Strip ANSI escape sequences (CSI, ending at a letter, and two-byte
    /// escapes) so frame text can be asserted on without the cursor moves
    /// ratatui writes between the pieces. Linux-only: the sole caller is
    /// the Linux-gated pty test below, so on Windows this would be dead
    /// code (windows-check clippy error on commit a4fe2ce).
    #[cfg(target_os = "linux")]
    fn strip_ansi(bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        }
        out
    }
}
