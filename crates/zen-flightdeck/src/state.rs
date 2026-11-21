//! Live session state for the flightdeck dashboard.
//!
//! The state is a plain data model over the existing `AgentEvent` stream:
//! every number shown has a real source in the repo (an event field, a
//! turn result, or the cost ledger). Nothing is invented: an event with
//! no real source is not shown, and a value with no source yet reads
//! "chưa có số liệu" / "giá chưa rõ" instead of a made-up number.
//!
//! Row identity: a subagent row is keyed by the parent tool call id
//! (`parent_tool_use_id` on the child's events, `tool_use_id` on the
//! parent's `delegate` progress). A `SubagentStart` system message
//! carries no id, so it can only name the most recently created unnamed
//! row, never create one on its own. (In the default hook setup no
//! `SubagentStart` event is emitted at all: it only fires when a skill
//! is missing or a hook returns a system message.)

use std::collections::HashMap;
use std::time::{Duration, Instant};
use zen_core::{AgentEvent, PermissionMode, UsageStats, estimate_tokens};

/// Cap on retained session-log lines: the dashboard is a live view, not
/// an archive; the oldest lines drop once the buffer is full.
const MAX_LOG_LINES: usize = 500;
/// First-line truncation for assistant text in the log.
const MAX_TEXT_CHARS: usize = 120;
/// Truncation for a delegate result in the agents table.
const MAX_RESULT_CHARS: usize = 60;
/// Default context window (the `AgentConfig` default) when the session
/// does not declare one.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// How many id characters to show for a subagent row whose real name
/// never arrived in an event.
const SHORT_ID_CHARS: usize = 12;

/// Status of one agent row in the live table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// A turn (main) or a delegated task (subagent) is in progress.
    Running,
    /// The delegated task finished and its result arrived.
    Done,
    /// The delegated task finished with an error.
    Failed,
    /// Idle between turns (main row only).
    Waiting,
}

impl AgentStatus {
    /// The label shown in the table (Vietnamese, as the dashboard is).
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Running => "đang chạy",
            AgentStatus::Done => "xong ✓",
            AgentStatus::Failed => "lỗi",
            AgentStatus::Waiting => "chờ",
        }
    }
}

/// One row of the live agents table. `agents[0]` in the state is always
/// the main agent; every other row is a subagent keyed by the parent
/// tool call id that spawned it.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// Row key: "main", or the parent tool call id for a subagent.
    pub key: String,
    /// Display name: the agent name when an event carried it, else
    /// `subagent <short id>`.
    pub name: String,
    pub status: AgentStatus,
    /// Tool currently executing for this agent, if the latest progress
    /// event could be attributed to it.
    pub current_tool: Option<String>,
    started_at: Instant,
    /// Frozen elapsed time once the row is done/failed; `None` while
    /// running (elapsed is then measured live from `started_at`).
    elapsed_frozen: Option<Duration>,
    /// First line of the delegate result, once the parent tool result
    /// for this row arrives.
    pub result: Option<String>,
}

impl AgentRow {
    fn new(key: &str, name: String) -> Self {
        Self {
            key: key.to_string(),
            name,
            status: AgentStatus::Running,
            current_tool: None,
            started_at: Instant::now(),
            elapsed_frozen: None,
            result: None,
        }
    }

    /// Elapsed run time: live while running, frozen at completion.
    pub fn elapsed(&self) -> Duration {
        self.elapsed_frozen
            .unwrap_or_else(|| self.started_at.elapsed())
    }

    fn finish(&mut self, failed: bool, result: Option<String>) {
        self.status = if failed {
            AgentStatus::Failed
        } else {
            AgentStatus::Done
        };
        self.current_tool = None;
        self.elapsed_frozen = Some(self.started_at.elapsed());
        self.result = result;
    }
}

/// One line of the session log, with the wall-clock time it arrived.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: String,
    pub text: String,
}

/// Which pane has keyboard focus (`Tab` switches it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Agents,
    Log,
}

/// The whole dashboard state: header numbers, the agents table, and the
/// session log. All mutations go through `apply` (one event at a time)
/// or `note_turn_result` (exact turn-end numbers from the agent).
#[derive(Debug, Clone)]
pub struct FlightdeckState {
    /// Short session id shown in the header.
    pub session_id: String,
    /// Model serving this session.
    pub model: String,
    /// Context window in tokens.
    pub context_window: u64,
    /// Tokens used, estimated live from assistant messages
    /// (`estimate_tokens`) until a turn result supplies the exact count.
    pub used_tokens: u64,
    /// Exact prompt tokens from the last turn result, when available.
    pub used_tokens_exact: Option<u64>,
    /// Session cost in USD, or `None` when the model has no price
    /// (shown as "giá chưa rõ", never as a fake $0).
    pub cost_usd: Option<f64>,
    /// LLM responses seen this session (one per `Assistant` event).
    pub requests: u64,
    /// Current permission mode, e.g. "default".
    pub permission_mode: String,
    /// Autocompact status, e.g. "auto" / "off" / "100000".
    pub autocompact: String,
    /// The agents table; `agents[0]` is always the main agent.
    pub agents: Vec<AgentRow>,
    /// Session log, oldest first, capped at `MAX_LOG_LINES`.
    pub log: Vec<LogLine>,
    /// Which pane has focus.
    pub focus: Focus,
    /// Log filter text (`/` to edit); empty means no filtering.
    pub filter: String,
    /// Last tool name seen per tool call id (from `ToolProgress`).
    tool_names: HashMap<String, String>,
    /// Last heartbeat elapsed per tool call id (from `ToolProgress`).
    tool_elapsed_ms: HashMap<String, u64>,
    /// Tool call ids already attributed to a subagent row, via a
    /// `ToolResult` that carried that row's id as `parent_tool_use_id`.
    subagent_tools: HashMap<String, String>,
}

impl FlightdeckState {
    /// A fresh state for a session. The main row starts idle ("chờ"):
    /// it flips to "đang chạy" on the first event of a turn.
    pub fn new(
        session_id: impl Into<String>,
        model: impl Into<String>,
        context_window: u64,
        permission_mode: PermissionMode,
        autocompact: impl Into<String>,
    ) -> Self {
        let mut main = AgentRow::new("main", "main".to_string());
        main.status = AgentStatus::Waiting;
        Self {
            session_id: session_id.into(),
            model: model.into(),
            context_window,
            used_tokens: 0,
            used_tokens_exact: None,
            cost_usd: None,
            requests: 0,
            permission_mode: permission_mode.as_str().to_string(),
            autocompact: autocompact.into(),
            agents: vec![main],
            log: Vec::new(),
            focus: Focus::Agents,
            filter: String::new(),
            tool_names: HashMap::new(),
            tool_elapsed_ms: HashMap::new(),
            subagent_tools: HashMap::new(),
        }
    }

    /// Tokens used, preferring the exact turn-end count when one exists.
    pub fn used_tokens_display(&self) -> u64 {
        self.used_tokens_exact.unwrap_or(self.used_tokens)
    }

    /// Context usage as a percentage of the window, or `None` when the
    /// window is zero.
    pub fn ctx_percent(&self) -> Option<f64> {
        if self.context_window == 0 {
            None
        } else {
            Some(self.used_tokens_display() as f64 / self.context_window as f64 * 100.0)
        }
    }

    /// Cost text for the header: a real amount, or "giá chưa rõ".
    pub fn cost_text(&self) -> String {
        match self.cost_usd {
            None => "giá chưa rõ".to_string(),
            Some(c) if c > 0.0 && c < 0.01 => format!("${c:.4}"),
            Some(c) => format!("${c:.2}"),
        }
    }

    /// The header, two lines so it never truncates the mode on a
    /// normal terminal: line 1 session/ctx/cost/requests/model, line 2
    /// the permission mode and autocompact status. The compact layout
    /// shows only line 1.
    pub fn header_line(&self) -> String {
        let pct = match self.ctx_percent() {
            Some(p) => format!("{p:.0}%"),
            None => "-".to_string(),
        };
        format!(
            "ZEN - phiên {} | ctx {} ({}/{}) | {} | req {} | model {}\nmode: {} (autocompact: {})",
            short_id(&self.session_id),
            pct,
            fmt_u64(self.used_tokens_display()),
            fmt_u64(self.context_window),
            self.cost_text(),
            self.requests,
            self.model,
            self.permission_mode,
            self.autocompact,
        )
    }

    /// Table rows as `[name, status, current tool, elapsed, result]`.
    pub fn agent_rows(&self) -> Vec<[String; 5]> {
        self.agents
            .iter()
            .map(|r| {
                [
                    r.name.clone(),
                    r.status.label().to_string(),
                    r.current_tool.clone().unwrap_or_else(|| "-".to_string()),
                    fmt_duration(r.elapsed()),
                    r.result.clone().unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect()
    }

    /// Visible log lines, newest last, with the active filter applied
    /// (case-insensitive substring on the line text).
    pub fn visible_log(&self) -> Vec<String> {
        let needle = self.filter.to_lowercase();
        self.log
            .iter()
            .filter(|l| needle.is_empty() || l.text.to_lowercase().contains(&needle))
            .map(|l| format!("{} {}", l.time, l.text))
            .collect()
    }

    /// Exact turn-end numbers from the agent: cumulative prompt tokens
    /// and the session cost so far. Also marks the main agent idle
    /// until the next turn starts.
    pub fn note_turn_result(&mut self, usage: &UsageStats, cost_usd: Option<f64>) {
        self.used_tokens_exact = Some(usage.input_tokens);
        self.cost_usd = cost_usd;
        if self.agents[0].status == AgentStatus::Running {
            self.agents[0].status = AgentStatus::Waiting;
            self.agents[0].current_tool = None;
        }
    }

    /// Fold one event into the state. `TextDelta` is deliberately
    /// ignored: the Leader decided Phase 1 shows the full text when a
    /// turn completes, with no streaming producer.
    pub fn apply(&mut self, ev: &AgentEvent) {
        // Any event means the main agent is mid-turn.
        if self.agents[0].status == AgentStatus::Waiting {
            self.agents[0].status = AgentStatus::Running;
        }
        match ev {
            AgentEvent::Assistant {
                message,
                parent_tool_use_id,
            } => {
                self.requests += 1;
                self.used_tokens += estimate_tokens(std::slice::from_ref(message));
                let text = match message.text_ref() {
                    Some(t) => format!("assistant: {}", first_line(t, MAX_TEXT_CHARS)),
                    None => match &message.tool_calls {
                        Some(calls) if !calls.is_empty() => {
                            let names: Vec<&str> =
                                calls.iter().map(|c| c.function.name.as_str()).collect();
                            format!("assistant: gọi tool {}", names.join(", "))
                        }
                        _ => "assistant: (không có nội dung text)".to_string(),
                    },
                };
                if let Some(pid) = parent_tool_use_id {
                    let idx = self.ensure_row(pid);
                    self.agents[idx].current_tool = None;
                    // A tool-call assistant message announces the tool
                    // call ids the child is about to run: attribute
                    // their progress (and result lines) to this row.
                    // Without this, a `ToolProgress` event carries no
                    // parent linkage at all.
                    if let Some(calls) = &message.tool_calls {
                        for call in calls {
                            self.subagent_tools.insert(call.id.clone(), pid.clone());
                            self.tool_names
                                .insert(call.id.clone(), call.function.name.clone());
                        }
                    }
                    let line = format!("[{}] {text}", self.agents[idx].name);
                    self.push_log(line);
                } else {
                    self.push_log(text);
                }
            }
            AgentEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                parent_tool_use_id,
                parallel,
            } => {
                let name = self
                    .tool_names
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                let elapsed = self.tool_elapsed_ms.get(tool_use_id).copied().unwrap_or(0);
                let mut line = format!(
                    "tool {name} {} ({})",
                    if *is_error { "lỗi" } else { "ok" },
                    fmt_ms(elapsed),
                );
                if *parallel {
                    line.push_str(" (song song)");
                }
                if let Some(pid) = parent_tool_use_id {
                    self.subagent_tools.insert(tool_use_id.clone(), pid.clone());
                    let idx = self.ensure_row(pid);
                    if self.agents[idx].current_tool.as_deref() == Some(name.as_str()) {
                        self.agents[idx].current_tool = None;
                    }
                    let row_name = self.agents[idx].name.clone();
                    self.push_log(format!("[{row_name}] {line}"));
                } else if let Some(idx) = self.row_index(tool_use_id) {
                    // The parent's own result for a delegate call: the
                    // subagent is done.
                    let result = first_line(content, MAX_RESULT_CHARS);
                    self.agents[idx].finish(*is_error, Some(result));
                    let row_name = self.agents[idx].name.clone();
                    self.push_log(format!("[{row_name}] {line}"));
                } else {
                    self.agents[0].current_tool = None;
                    self.push_log(line);
                }
            }
            AgentEvent::ToolProgress {
                tool_use_id,
                tool_name,
                elapsed_ms,
            } => {
                // Progress heartbeats never write a log line: they would
                // flood it. They feed the table's current-tool column
                // and the per-tool elapsed used by the result line.
                self.tool_names
                    .insert(tool_use_id.clone(), tool_name.clone());
                self.tool_elapsed_ms
                    .insert(tool_use_id.clone(), *elapsed_ms);
                if tool_name == "delegate" || tool_name == "Task" {
                    // The parent is waiting on a subagent: the row is
                    // keyed by this delegate call, and the main row is
                    // busy running the delegate tool.
                    self.ensure_row(tool_use_id);
                    self.agents[0].current_tool = Some(tool_name.clone());
                } else if let Some(pid) = self.subagent_tools.get(tool_use_id).cloned() {
                    let idx = self.ensure_row(&pid);
                    self.agents[idx].current_tool = Some(tool_name.clone());
                } else if self.running_subagent_rows() == 1 {
                    // Exactly one child running: its tool must be this
                    // one. (With several children the tool column stays
                    // on the main row until a result attributes the tool.)
                    let idx = self
                        .agents
                        .iter()
                        .position(|r| r.key != "main" && r.status == AgentStatus::Running)
                        .expect("one running subagent row");
                    self.agents[idx].current_tool = Some(tool_name.clone());
                } else {
                    self.agents[0].current_tool = Some(tool_name.clone());
                }
            }
            AgentEvent::ApiRetry {
                attempt,
                max_retries,
                retry_delay_ms,
                error_status,
                error,
            } => {
                self.push_log(format!(
                    "retry lần {attempt}/{max_retries} sau {retry_delay_ms}ms (mã {error_status:?}, {error})"
                ));
            }
            AgentEvent::CompactBoundary {
                trigger,
                pre_tokens,
                summary,
            } => {
                // The estimate stays as-is after compaction: the sink
                // has no message list to re-estimate from. The next
                // turn result corrects it with the exact count.
                self.push_log(format!(
                    "đã nén ngữ cảnh ({trigger}, {pre_tokens} token trước khi nén): {}",
                    first_line(summary, MAX_TEXT_CHARS)
                ));
            }
            AgentEvent::Pruned {
                pruned,
                tokens_saved,
            } => {
                // `tokens_saved` is exact, so the estimate can drop by
                // that much right away.
                self.used_tokens = self.used_tokens.saturating_sub(*tokens_saved);
                self.push_log(format!(
                    "đã cắt {pruned} tin, tiết kiệm {tokens_saved} token"
                ));
            }
            AgentEvent::PermissionDenied(denial) => {
                self.push_log(format!(
                    "quyền bị từ chối: {} ({})",
                    denial.tool_name, denial.tool_use_id
                ));
            }
            AgentEvent::SystemMessage { event, text } => {
                if event == "SubagentStart"
                    && let Some(name) = parse_subagent_name(text)
                {
                    self.name_latest_unnamed_row(&name);
                }
                if text.is_empty() {
                    self.push_log(event.clone());
                } else {
                    self.push_log(format!("{event}: {text}"));
                }
            }
            AgentEvent::TextDelta { .. } => {
                // Phase 1 shows the full text on `Assistant`; streaming
                // deltas are ignored by Leader decision.
            }
            // `AgentEvent` is non-exhaustive: future variants are
            // ignored until the dashboard learns to show them.
            _ => {}
        }
    }

    /// Index of the row for a parent tool call id, creating it when this
    /// is the first event that mentions it.
    fn ensure_row(&mut self, key: &str) -> usize {
        if let Some(idx) = self.row_index(key) {
            return idx;
        }
        let name = format!("subagent {}", short_id(key));
        self.agents.push(AgentRow::new(key, name));
        self.agents.len() - 1
    }

    fn row_index(&self, key: &str) -> Option<usize> {
        self.agents.iter().position(|r| r.key == key)
    }

    /// How many subagent rows are currently running.
    fn running_subagent_rows(&self) -> usize {
        self.agents
            .iter()
            .filter(|r| r.key != "main" && r.status == AgentStatus::Running)
            .count()
    }

    /// A `SubagentStart` message names its agent but carries no id, so
    /// it can only name the most recently created row that still has a
    /// generated name.
    fn name_latest_unnamed_row(&mut self, name: &str) {
        if let Some(row) = self
            .agents
            .iter_mut()
            .rev()
            .find(|r| r.key != "main" && r.name.starts_with("subagent "))
        {
            row.name = name.to_string();
        }
    }

    fn push_log(&mut self, text: String) {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push(LogLine { time, text });
        if self.log.len() > MAX_LOG_LINES {
            let excess = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..excess);
        }
    }
}

/// First line of `s`, truncated to `max` characters (char-boundary safe).
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

/// Short id for display: the first `SHORT_ID_CHARS` characters.
fn short_id(key: &str) -> String {
    key.chars().take(SHORT_ID_CHARS).collect()
}

/// `53760` -> `"53.760"` (Vietnamese thousands separator).
pub fn fmt_u64(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// A tool heartbeat elapsed: `350` -> `"350ms"`, `1200` -> `"1.2s"`.
fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// An agent run time: `12s`, `48s`, `1m 05s`.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// Parse `sub-agent 'NAME': ...` from a `SubagentStart` message.
fn parse_subagent_name(text: &str) -> Option<String> {
    let rest = text.strip_prefix("sub-agent '")?;
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zen_core::{ChatMessage, FunctionCall, ToolCall};

    fn assistant_event(text: &str) -> AgentEvent {
        AgentEvent::Assistant {
            message: ChatMessage::assistant(Some(text.to_string()), None),
            parent_tool_use_id: None,
        }
    }

    fn state() -> FlightdeckState {
        FlightdeckState::new(
            "sess-0123456789abcdef",
            "deepseek-v4.1-flash",
            128_000,
            PermissionMode::Default,
            "auto",
        )
    }

    #[test]
    fn header_shows_ctx_cost_requests_and_model() {
        let mut s = state();
        s.apply(&assistant_event("hello world, this is a test message"));
        assert_eq!(s.requests, 1);
        assert!(s.used_tokens > 0);
        let header = s.header_line();
        assert!(header.contains("ctx "), "{header}");
        assert!(header.contains("req 1"), "{header}");
        assert!(header.contains("deepseek-v4.1-flash"), "{header}");
        assert!(
            header.contains("mode: default (autocompact: auto)"),
            "{header}"
        );
        // No price known yet: never a fake $0.
        assert!(header.contains("giá chưa rõ"), "{header}");
    }

    #[test]
    fn turn_result_sets_exact_tokens_and_cost() {
        let mut s = state();
        s.apply(&assistant_event("some text here to estimate"));
        s.note_turn_result(
            &UsageStats {
                input_tokens: 53_760,
                output_tokens: 1_200,
                cache_read_input_tokens: 0,
            },
            Some(0.42),
        );
        assert_eq!(s.used_tokens_display(), 53_760);
        assert_eq!(s.ctx_percent(), Some(42.0));
        assert!(
            s.header_line().contains("ctx 42% (53.760/128.000)"),
            "{}",
            s.header_line()
        );
        assert!(s.header_line().contains("$0.42"), "{}", s.header_line());
        assert_eq!(s.agents[0].status, AgentStatus::Waiting);
    }

    #[test]
    fn subagent_rows_complete_on_parent_tool_result() {
        let mut s = state();
        // Parent starts a delegate call.
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-1".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 120,
        });
        assert_eq!(s.agents.len(), 2);
        assert_eq!(s.agents[1].status, AgentStatus::Running);
        // Child does work: its tool result carries the parent id.
        s.apply(&AgentEvent::ToolResult {
            tool_use_id: "t-1".to_string(),
            content: "read ok".to_string(),
            is_error: false,
            parent_tool_use_id: Some("dlg-1".to_string()),
            parallel: false,
        });
        // Child answers.
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(Some("done part one".into()), None),
            parent_tool_use_id: Some("dlg-1".to_string()),
        });
        assert_eq!(s.requests, 1);
        // Parent's own result for the delegate call finishes the row.
        s.apply(&AgentEvent::ToolResult {
            tool_use_id: "dlg-1".to_string(),
            content: "Kết quả phần một\nsecond line".to_string(),
            is_error: false,
            parent_tool_use_id: None,
            parallel: false,
        });
        assert_eq!(s.agents[1].status, AgentStatus::Done);
        assert_eq!(s.agents[1].result.as_deref(), Some("Kết quả phần một"));
        let rows = s.agent_rows();
        assert_eq!(rows[1][1], "xong ✓");
        // Elapsed is frozen at completion (here it is "0s": the row lived
        // only microseconds in this test); the format is what matters.
        assert_eq!(rows[1][3], "0s");
    }

    #[test]
    fn subagent_tool_progress_attributes_to_single_running_child() {
        let mut s = state();
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-1".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 100,
        });
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "t-9".to_string(),
            tool_name: "Read".to_string(),
            elapsed_ms: 450,
        });
        assert_eq!(s.agents[1].current_tool.as_deref(), Some("Read"));
        assert_eq!(s.agents[0].current_tool.as_deref(), Some("delegate"));
    }

    #[test]
    fn subagent_start_names_latest_unnamed_row() {
        let mut s = state();
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-7".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 50,
        });
        assert!(s.agents[1].name.starts_with("subagent "));
        s.apply(&AgentEvent::SystemMessage {
            event: "SubagentStart".to_string(),
            text: "sub-agent 'scout': skill 'x' not found, not preloaded".to_string(),
        });
        assert_eq!(s.agents[1].name, "scout");
    }

    #[test]
    fn text_delta_is_ignored_by_decision() {
        let mut s = state();
        s.apply(&AgentEvent::TextDelta {
            text: "streaming chunk".to_string(),
        });
        assert_eq!(s.requests, 0);
        assert!(s.log.is_empty());
    }

    #[test]
    fn log_filter_is_case_insensitive() {
        let mut s = state();
        s.apply(&assistant_event("first message"));
        s.apply(&AgentEvent::ApiRetry {
            attempt: 1,
            max_retries: 3,
            retry_delay_ms: 500,
            error_status: Some(429),
            error: "busy".to_string(),
        });
        assert_eq!(s.visible_log().len(), 2);
        s.filter = "RETRY".to_string();
        assert_eq!(s.visible_log().len(), 1);
        assert!(s.visible_log()[0].contains("retry lần 1/3"));
    }

    #[test]
    fn pruned_tokens_drop_the_estimate() {
        let mut s = state();
        s.used_tokens = 10_000;
        s.apply(&AgentEvent::Pruned {
            pruned: 5,
            tokens_saved: 2_500,
        });
        assert_eq!(s.used_tokens, 7_500);
    }

    #[test]
    fn tool_call_assistant_message_is_logged_without_text() {
        let mut s = state();
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(
                None,
                Some(vec![ToolCall {
                    id: "c1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "Read".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            ),
            parent_tool_use_id: None,
        });
        assert_eq!(s.requests, 1);
        assert!(
            s.visible_log()[0].contains("gọi tool Read"),
            "{}",
            s.visible_log()[0]
        );
    }

    #[test]
    fn tool_call_announcement_attributes_later_progress_to_child() {
        let mut s = state();
        // Two children running in parallel.
        for pid in ["dlg-1", "dlg-2"] {
            s.apply(&AgentEvent::ToolProgress {
                tool_use_id: pid.to_string(),
                tool_name: "delegate".to_string(),
                elapsed_ms: 100,
            });
        }
        // Child 1 announces its tool call; child 2 does not.
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(
                None,
                Some(vec![ToolCall {
                    id: "t-1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "Read".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            ),
            parent_tool_use_id: Some("dlg-1".to_string()),
        });
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "t-1".to_string(),
            tool_name: "Read".to_string(),
            elapsed_ms: 200,
        });
        assert_eq!(s.agents[1].current_tool.as_deref(), Some("Read"));
        assert_eq!(s.agents[2].current_tool, None);
    }

    #[test]
    fn fmt_u64_groups_by_thousands() {
        assert_eq!(fmt_u64(0), "0");
        assert_eq!(fmt_u64(999), "999");
        assert_eq!(fmt_u64(53_760), "53.760");
        assert_eq!(fmt_u64(1_234_567), "1.234.567");
    }

    #[test]
    fn cost_text_formats_small_and_large_amounts() {
        let mut s = state();
        s.cost_usd = Some(0.004);
        assert_eq!(s.cost_text(), "$0.0040");
        s.cost_usd = Some(0.01);
        assert_eq!(s.cost_text(), "$0.01");
        s.cost_usd = Some(0.0);
        assert_eq!(s.cost_text(), "$0.00");
        s.cost_usd = Some(2.5);
        assert_eq!(s.cost_text(), "$2.50");
        s.cost_usd = None;
        assert_eq!(s.cost_text(), "giá chưa rõ");
    }

    #[test]
    fn any_event_marks_main_running() {
        let mut s = state();
        assert_eq!(s.agents[0].status, AgentStatus::Waiting);
        s.apply(&assistant_event("hello"));
        assert_eq!(s.agents[0].status, AgentStatus::Running);
        // A second event keeps it running (the == is not inverted).
        s.apply(&assistant_event("again"));
        assert_eq!(s.agents[0].status, AgentStatus::Running);
    }

    #[test]
    fn assistant_with_empty_tool_calls_logs_no_text() {
        let mut s = state();
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(None, Some(vec![])),
            parent_tool_use_id: None,
        });
        assert!(
            s.visible_log()[0].contains("(không có nội dung text)"),
            "{}",
            s.visible_log()[0]
        );
    }

    #[test]
    fn child_tool_result_clears_matching_current_tool() {
        let mut s = state();
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-1".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 100,
        });
        s.apply(&AgentEvent::Assistant {
            message: ChatMessage::assistant(
                None,
                Some(vec![ToolCall {
                    id: "t-1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "Read".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
            ),
            parent_tool_use_id: Some("dlg-1".to_string()),
        });
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "t-1".to_string(),
            tool_name: "Read".to_string(),
            elapsed_ms: 200,
        });
        assert_eq!(s.agents[1].current_tool.as_deref(), Some("Read"));
        s.apply(&AgentEvent::ToolResult {
            tool_use_id: "t-1".to_string(),
            content: "ok".to_string(),
            is_error: false,
            parent_tool_use_id: Some("dlg-1".to_string()),
            parallel: false,
        });
        assert_eq!(s.agents[1].current_tool, None);
    }

    #[test]
    fn permission_denied_is_logged() {
        let mut s = state();
        s.apply(&AgentEvent::PermissionDenied(zen_core::PermissionDenial {
            tool_name: "Read".to_string(),
            tool_use_id: "t-9".to_string(),
            tool_input: serde_json::Value::Null,
        }));
        assert!(
            s.visible_log()
                .iter()
                .any(|l| l.contains("quyền bị từ chối: Read (t-9)")),
            "{:?}",
            s.visible_log()
        );
    }

    #[test]
    fn second_subagent_start_does_not_rename_a_named_row() {
        let mut s = state();
        s.apply(&AgentEvent::ToolProgress {
            tool_use_id: "dlg-7".to_string(),
            tool_name: "delegate".to_string(),
            elapsed_ms: 50,
        });
        s.apply(&AgentEvent::SystemMessage {
            event: "SubagentStart".to_string(),
            text: "sub-agent 'scout': skill 'x' not found".to_string(),
        });
        assert_eq!(s.agents[1].name, "scout");
        // No unnamed row left: the second start must not rename the
        // already-named row (the && in the finder is not ||).
        s.apply(&AgentEvent::SystemMessage {
            event: "SubagentStart".to_string(),
            text: "sub-agent 'other': skill 'y' not found".to_string(),
        });
        assert_eq!(s.agents[1].name, "scout");
    }

    #[test]
    fn log_is_capped_at_500_lines() {
        let mut s = state();
        for i in 0..505 {
            s.apply(&assistant_event(&format!("message {i}")));
        }
        assert_eq!(s.log.len(), 500);
        // The oldest lines are the ones dropped.
        assert!(s.log[0].text.contains("message 5"));
        for i in 0..505 {
            s.apply(&assistant_event(&format!("more {i}")));
        }
        assert_eq!(s.log.len(), 500);
    }

    #[test]
    fn first_line_truncates_only_past_the_limit() {
        let mut s = state();
        let exact = "x".repeat(MAX_TEXT_CHARS);
        s.apply(&assistant_event(&exact));
        assert!(!s.visible_log()[0].contains('…'), "{}", s.visible_log()[0]);
        let over = "y".repeat(MAX_TEXT_CHARS + 1);
        s.apply(&assistant_event(&over));
        assert!(s.visible_log()[1].contains('…'), "{}", s.visible_log()[1]);
    }

    #[test]
    fn header_shows_short_session_id() {
        let s = state(); // session "sess-0123456789abcdef"
        assert!(
            s.header_line().contains("sess-0123456"),
            "{}",
            s.header_line()
        );
        assert!(
            !s.header_line().contains("sess-0123456789abcdef"),
            "{}",
            s.header_line()
        );
    }

    #[test]
    fn tool_result_log_shows_heartbeat_elapsed() {
        let mut s = state();
        for (i, (ms, want)) in [(999, "999ms"), (1000, "1.0s"), (1200, "1.2s")]
            .into_iter()
            .enumerate()
        {
            s.apply(&AgentEvent::ToolProgress {
                tool_use_id: "t-x".to_string(),
                tool_name: "Read".to_string(),
                elapsed_ms: ms,
            });
            s.apply(&AgentEvent::ToolResult {
                tool_use_id: "t-x".to_string(),
                content: "ok".to_string(),
                is_error: false,
                parent_tool_use_id: None,
                parallel: false,
            });
            let log = s.visible_log();
            assert!(log[i].contains(&format!("({want})")), "{}", log[i]);
        }
    }
}
