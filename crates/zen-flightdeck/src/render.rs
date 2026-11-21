//! Ratatui rendering for the flightdeck dashboard.
//!
//! Layout (spec section 3), top to bottom: header (two lines),
//! agents table, session log, footer with keys. Below 80 columns or 24 rows
//! the table drops out and only the header and log are drawn, so the
//! layout never breaks on a small terminal.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table},
};

use crate::state::{FlightdeckState, Focus};

/// Compact rendering kicks in below this width or height.
pub const MIN_FULL_WIDTH: u16 = 80;
pub const MIN_FULL_HEIGHT: u16 = 24;

/// Draw one frame of the dashboard from a snapshot of the state.
pub fn draw(frame: &mut Frame, state: &FlightdeckState) {
    let area = frame.area();
    if area.width < MIN_FULL_WIDTH || area.height < MIN_FULL_HEIGHT {
        draw_compact(frame, state, area);
        return;
    }
    let rows = state.agents.len();
    // Table height: top border + header + one line per agent + bottom
    // border. One short and the last agent row is clipped.
    let table_height = (rows + 3).clamp(5, 11) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(table_height),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);
    draw_header(frame, state, chunks[0]);
    draw_agents(frame, state, chunks[1]);
    draw_log(frame, state, chunks[2]);
    draw_footer(frame, state, chunks[3]);
}

fn focused_border(state: &FlightdeckState, pane: Focus) -> (BorderType, Style) {
    if state.focus == pane {
        (
            BorderType::Double,
            Style::default().add_modifier(Modifier::BOLD),
        )
    } else {
        (BorderType::Plain, Style::default())
    }
}

fn draw_header(frame: &mut Frame, state: &FlightdeckState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ZEN flightdeck ");
    let paragraph = Paragraph::new(state.header_line()).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_agents(frame: &mut Frame, state: &FlightdeckState, area: Rect) {
    let (border_type, border_style) = focused_border(state, Focus::Agents);
    let header = Row::new(["tên", "trạng thái", "tool hiện tại", "đã chạy", "kết quả"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = state
        .agent_rows()
        .into_iter()
        .map(|cells| Row::new(cells.into_iter().map(Cell::from)));
    let widths = [
        Constraint::Length(18),
        Constraint::Length(11),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Min(0),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border_type)
                .border_style(border_style)
                .title(" AGENTS LIVE "),
        )
        .column_spacing(1);
    frame.render_widget(table, area);
}

fn draw_log(frame: &mut Frame, state: &FlightdeckState, area: Rect) {
    let (border_type, border_style) = focused_border(state, Focus::Log);
    let lines = state.visible_log();
    let inner_height = area.height.saturating_sub(2) as usize;
    let skip = lines.len().saturating_sub(inner_height);
    let shown: Vec<Line> = lines
        .iter()
        .skip(skip)
        .map(|l| Line::from(l.as_str()))
        .collect();
    let paragraph = Paragraph::new(shown).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            .title(" SESSION LOG "),
    );
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, state: &FlightdeckState, area: Rect) {
    let focus = match state.focus {
        Focus::Agents => "bảng agents",
        Focus::Log => "log",
    };
    let filter = if state.filter.is_empty() {
        String::new()
    } else {
        format!(" | lọc: {}", state.filter)
    };
    let text = format!("q thoát | Tab chuyển vùng (đang: {focus}) | / lọc log{filter}");
    let paragraph = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

/// Small terminals: header line plus log only, no table, no borders on
/// the header, so nothing wraps or breaks.
fn draw_compact(frame: &mut Frame, state: &FlightdeckState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(state.header_line()), chunks[0]);
    let lines = state.visible_log();
    let inner_height = chunks[1].height as usize;
    let skip = lines.len().saturating_sub(inner_height);
    let shown: Vec<Line> = lines
        .iter()
        .skip(skip)
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(shown), chunks[1]);
    frame.render_widget(
        Paragraph::new("q thoát | Tab chuyển vùng | / lọc log"),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use zen_core::{AgentEvent, ChatMessage, PermissionMode};

    fn populated_state() -> FlightdeckState {
        let mut s = FlightdeckState::new(
            "sess-0123456789abcdef",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Default,
            "auto",
        );
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(Some("hello from the main agent".into()), None),
            parent_tool_use_id: None,
        });
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-1".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 120,
        });
        s.apply(&AgentEvent::ToolResult {
            tool_use_id: "dlg-1".to_string(),
            content: "Kết quả phần một".to_string(),
            is_error: false,
            parent_tool_use_id: None,
            parallel: false,
        });
        s
    }

    fn render_to_string(state: &FlightdeckState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, state))
            .expect("test draw");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn full_layout_shows_header_agents_and_log() {
        let s = populated_state();
        let out = render_to_string(&s, 100, 30);
        assert!(out.contains("ctx "), "{out}");
        assert!(out.contains("req 1"), "{out}");
        assert!(out.contains("AGENTS LIVE"), "{out}");
        assert!(out.contains("main"), "{out}");
        // Status and result live in different columns, so they are
        // never one substring: check each half separately.
        assert!(out.contains("xong"), "{out}");
        assert!(out.contains("✓"), "{out}");
        assert!(out.contains("Kết quả phần một"), "{out}");
        assert!(out.contains("SESSION LOG"), "{out}");
        assert!(
            out.contains("assistant: hello from the main agent"),
            "{out}"
        );
        assert!(out.contains("q thoát"), "{out}");
    }

    #[test]
    fn compact_layout_never_breaks_on_small_terminals() {
        let s = populated_state();
        let out = render_to_string(&s, 60, 20);
        // Header and log survive; the table is dropped, not broken.
        assert!(out.contains("ctx "), "{out}");
        assert!(
            out.contains("assistant: hello from the main agent"),
            "{out}"
        );
        assert!(!out.contains("AGENTS LIVE"), "{out}");
    }

    #[test]
    fn empty_state_renders_without_panic() {
        let s = FlightdeckState::new(
            "sess-empty",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Plan,
            "off",
        );
        let out = render_to_string(&s, 100, 30);
        assert!(out.contains("mode: plan (autocompact: off)"), "{out}");
        assert!(out.contains("req 0"), "{out}");
    }

    #[test]
    fn compact_threshold_is_exact_at_the_boundary() {
        let s = populated_state();
        // Exactly 80x24 renders the full layout.
        let out = render_to_string(&s, 80, 24);
        assert!(out.contains("AGENTS LIVE"), "{out}");
        // One column short, or one row short: compact, no table.
        let out = render_to_string(&s, 79, 30);
        assert!(!out.contains("AGENTS LIVE"), "{out}");
        let out = render_to_string(&s, 100, 23);
        assert!(!out.contains("AGENTS LIVE"), "{out}");
    }

    #[test]
    fn log_block_starts_right_after_the_table() {
        // Layout geometry with 2 agent rows at 100x24: header takes
        // lines 0-3, the table 5 lines (4-8), so the log block's
        // title sits on line 9. The table-height mutant (`rows + 3`
        // -> `rows * 3`, 6 lines) pushes it to line 10.
        let s = populated_state();
        let out = render_to_string(&s, 100, 24);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 24, "{out}");
        assert!(lines[9].contains("SESSION LOG"), "{out}");
    }

    #[test]
    fn single_agent_row_is_not_clipped() {
        let s = FlightdeckState::new(
            "sess-empty",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Plan,
            "off",
        );
        let out = render_to_string(&s, 100, 30);
        assert!(out.contains("main"), "{out}");
    }

    #[test]
    fn focused_pane_has_the_double_border() {
        let s = populated_state();
        let out = render_to_string(&s, 100, 30);
        // Default focus is the agents table: its title line carries
        // the double border corner, the log block stays plain.
        assert!(out.contains("╔ AGENTS LIVE"), "{out}");
        assert!(!out.contains("╔ SESSION LOG"), "{out}");
    }
}
