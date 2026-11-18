use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use zen_core::lock_recovery::lock_or_recover;
use zen_core::*;

use crate::budget::{BudgetPolicy, BudgetVerdict};
use crate::compaction;
use crate::env_context;
use crate::finish;
use crate::prune::{PruneConfig, PruneStats};
use crate::tool_bundles::ToolBundle;

const MAX_EMPTY_REQUERIES: usize = 2;

const UNPARSED_REPLY_NUDGE: &str = "[zen: the server could not parse your last reply. Continue the task: make one tool call with plain JSON arguments, or give your final answer.]";

fn is_unparsed_sample(err: &str) -> bool {
    err.contains("model returned no choices") || err.contains("gateway error in 200 response")
}

pub const TRUNCATION_CONTINUE_NUDGE: &str = "[zen: your last reply was cut off by the output token limit. Continue exactly where you stopped, without repeating anything you already wrote.]";

pub const TRUNCATED_CALL_NUDGE: &str = "[zen: your last reply was cut off by the output token limit in the middle of a tool call, so no tool was run. Re-emit the tool call(s) in full. If the arguments are large, split the work into several smaller calls (e.g. write a big file in parts).]";

pub const DEFAULT_MAX_CONTINUATIONS: usize = 3;

/// Maximum LSP feedback appends per file in one run (Leader Q6): after
/// three rounds the agent has had its chance to fix the file; further
/// diagnostics would feed a fix loop, so the note stops and a single
/// line says so.
const MAX_LSP_FEEDBACK_PER_FILE: usize = 3;

pub fn is_truncated(finish_reason: Option<&str>) -> bool {
    finish_reason.is_some_and(|r| {
        let r = r.to_ascii_lowercase();
        r == "length" || r == "max_tokens" || r == "max_output_tokens"
    })
}

pub fn content_filter_message(partial: &str) -> String {
    let mut m = String::from(
        "Blocked by the provider's content filter (finish_reason: content_filter). The model's reply was stopped; Zen did not retry. Rephrase the request or use another model.",
    );
    if !partial.trim().is_empty() {
        m.push_str(" Partial reply: ");
        m.push_str(&partial.chars().take(300).collect::<String>());
    }
    m
}

const EMPTY_REPLY_NUDGE: &str =
    "[zen: your last reply was empty. Continue the task: call a tool, or give your final answer.]";
use crate::guard;
use crate::loop_guard;
use crate::plan;
use crate::repair;

pub const DEFAULT_STOP_HOOK_CONTINUES: usize = 8;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,

    pub fallback_models: Vec<String>,

    pub system_prompt: String,

    pub max_turns: usize,

    pub loop_stuck_k: usize,

    pub budget: Option<BudgetPolicy>,

    pub permission_mode: PermissionMode,

    pub context_window: u64,

    pub auto_compact_threshold: Option<u64>,

    pub keep_after_compact: usize,

    pub depth: usize,

    pub verify_reminder: bool,

    pub verify_command: Option<String>,

    pub verify_timeout_secs: u64,

    pub verify_after_edit: Option<String>,

    pub verify_after_edit_timeout_secs: u64,

    /// Post-edit LSP diagnostics provider (P0 #4). None by default: the
    /// feedback loop is off unless settings `lsp.diagnosticsAfterEdit`
    /// enables it (wired in zen-cli assembly).
    pub diagnostics_provider: Option<Arc<dyn DiagnosticsProvider>>,

    pub tool_timeouts: std::collections::HashMap<String, u64>,

    pub max_continuations: usize,

    pub prune: PruneConfig,

    pub max_output_tokens: Option<u32>,

    pub tool_bundle: ToolBundle,

    pub max_parallel_tool_calls: usize,

    pub done_scope: Vec<String>,
    /// Send per-request environment context as deltas (Viec 5): full
    /// snapshot on the first request, only changed fields afterwards,
    /// full again every 10 turns. Off by default; the CLI enables it.
    pub env_context_deltas: bool,
}

pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 300_000;

pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 4;

impl AgentConfig {
    pub fn tool_timeout_ms(&self, tool: &str) -> Option<u64> {
        self.tool_timeout_with_source(tool).0
    }

    pub fn tool_timeout_with_source(&self, tool: &str) -> (Option<u64>, TimeoutSource) {
        if tool == "Bash" {
            return (None, TimeoutSource::None);
        }
        match self.tool_timeouts.get(tool) {
            Some(t) => ((t > &0).then_some(*t), TimeoutSource::PerTool),

            None if matches!(tool, "Task" | "delegate" | "JobOutput") => {
                (None, TimeoutSource::None)
            }
            None => match self.tool_timeouts.get("default") {
                Some(t) => ((t > &0).then_some(*t), TimeoutSource::Default),
                None => (Some(DEFAULT_TOOL_TIMEOUT_MS), TimeoutSource::BuiltIn),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutSource {
    PerTool,

    Default,

    BuiltIn,

    None,
}

async fn run_tool(
    tool: Arc<dyn Tool>,
    args: Value,
    ctx: ToolContext,
    timeout: (Option<u64>, TimeoutSource),
) -> ToolOutput {
    let eligible = crate::retry::tool_retry_eligible_for(tool.name(), tool.is_read_only());
    let mut attempt: u32 = 0;
    loop {
        let out = run_tool_once(tool.clone(), args.clone(), &ctx, timeout).await;
        if !out.is_error {
            return out;
        }
        let transient = crate::retry::is_transient_error(&out.content);
        if !eligible || !transient || attempt >= crate::retry::MAX_TOOL_RETRIES {
            return out;
        }
        let backoff = crate::retry::retry_backoff_ms_for(&out.content, attempt);
        let snippet: String = out.content.chars().take(200).collect();
        eprintln!(
            "[retry] tool {} attempt {}/{} failed with transient error, retrying in {} ms: {}",
            tool.name(),
            attempt + 1,
            crate::retry::MAX_TOOL_RETRIES,
            backoff,
            snippet
        );
        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
        attempt += 1;
    }
}

async fn run_tool_once(
    tool: Arc<dyn Tool>,
    args: Value,
    ctx: &ToolContext,
    timeout: (Option<u64>, TimeoutSource),
) -> ToolOutput {
    let (timeout_ms, source) = timeout;
    let Some(ms) = timeout_ms else {
        return tool.call(args, ctx).await;
    };

    let src = match source {
        TimeoutSource::PerTool => format!("settings toolTimeouts.{}", tool.name()),
        TimeoutSource::Default => "settings toolTimeouts.default".to_string(),
        TimeoutSource::BuiltIn => "the built-in default timeout".to_string(),
        TimeoutSource::None => "a timeout that was then disabled".to_string(),
    };
    match tokio::time::timeout(std::time::Duration::from_millis(ms), tool.call(args, ctx)).await {
        Ok(out) => out,
        Err(_) => ToolOutput::err(format!(
            "Error: {} timed out after {} ms and was stopped ({})",
            tool.name(),
            ms,
            src
        )),
    }
}

const TOOL_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[allow(clippy::too_many_arguments)] // floor-guard: params-struct refactor deferred
pub(crate) async fn run_tool_with_heartbeat(
    tool: Arc<dyn Tool>,
    args: Value,
    ctx: ToolContext,
    timeout_ms: (Option<u64>, TimeoutSource),
    sink: Arc<dyn EventSink>,
    tool_use_id: String,
    tool_name: String,
    interval: std::time::Duration,
) -> ToolOutput {
    let hb_sink = sink.clone();
    let start = std::time::Instant::now();
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);

        ticker.tick().await;
        loop {
            ticker.tick().await;
            hb_sink.emit(AgentEvent::ToolProgress {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }
    });
    let out = run_tool(tool, args, ctx, timeout_ms).await;
    heartbeat.abort();
    out
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            // The decided CLI default (PR #71 review F8): a free
            // catalog entry, in the id its gateway accepts.
            model: "deepseek-v4.1-flash".into(),
            fallback_models: vec![],
            system_prompt: String::new(),
            max_turns: 50,
            loop_stuck_k: crate::loop_guard::DEFAULT_LOOP_STUCK_K,
            budget: None,
            permission_mode: PermissionMode::Default,
            context_window: 128_000,
            auto_compact_threshold: Some(100_000),
            keep_after_compact: 4,
            depth: 0,
            verify_reminder: false,
            verify_command: None,
            verify_timeout_secs: 300,
            verify_after_edit: None,
            verify_after_edit_timeout_secs: 60,
            diagnostics_provider: None,
            tool_timeouts: Default::default(),
            max_continuations: DEFAULT_MAX_CONTINUATIONS,
            prune: PruneConfig::default(),
            max_output_tokens: None,
            tool_bundle: ToolBundle::All,
            max_parallel_tool_calls: DEFAULT_MAX_PARALLEL_TOOL_CALLS,
            done_scope: vec![],
            env_context_deltas: false,
        }
    }
}

pub struct Agent {
    pub llm: Arc<dyn LlmBackend>,

    pub tools: ToolRegistry,

    pub permissions: Arc<dyn PermissionChecker>,

    pub prompter: Option<Arc<dyn Prompter>>,

    /// Async prompter (M4, Phase 2 #9): set by the ACP server so a
    /// permission question round-trips to the client instead of the
    /// terminal. Preferred over `prompter` when present.
    pub async_prompter: Option<Arc<dyn AsyncPrompter>>,

    pub hooks: Arc<dyn HookRunner>,

    pub sink: Arc<dyn EventSink>,

    pub ctx: ToolContext,

    pub config: AgentConfig,

    pub messages: Vec<ChatMessage>,

    pub usage: UsageStats,

    pub denials: Vec<PermissionDenial>,

    pub parent_tool_use_id: Option<String>,

    pub last_prompt_tokens: u64,

    pub stop_hook_max_continues: usize,

    pub budget_warned: bool,

    pub model_calls_since_todo: usize,

    pub journal: Option<Arc<dyn ToolJournal>>,

    pub prune_turn_starts: Vec<usize>,

    pub prune_stats: PruneStats,
    /// Per-session env-context delta state (Viec 5).
    pub env_deltas: env_context::DeltaStore,

    /// LSP feedback rounds already appended per file in this run
    /// (path string -> count), enforcing `MAX_LSP_FEEDBACK_PER_FILE`.
    pub lsp_feedback_counts: HashMap<String, usize>,

    /// Hook context waiting to be delivered with the next user prompt
    /// (PostModelSwitch `additionalContext`, DirectoryAdded
    /// `systemMessage`). Drained at the start of `run_turn`.
    pub pending_hook_context: Vec<String>,

    /// Dynamic FileChanged watch list from `watchPaths` returned by
    /// CwdChanged / FileChanged hooks. The REPL merges this with the
    /// static matcher paths when it polls for file changes.
    pub dynamic_watch_paths: Vec<std::path::PathBuf>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)] // floor-guard: params-struct refactor deferred
    pub fn new(
        llm: Arc<dyn LlmBackend>,
        tools: ToolRegistry,
        permissions: Arc<dyn PermissionChecker>,
        prompter: Option<Arc<dyn Prompter>>,
        hooks: Arc<dyn HookRunner>,
        sink: Arc<dyn EventSink>,
        ctx: ToolContext,
        config: AgentConfig,
    ) -> Self {
        Self {
            llm,
            tools,
            permissions,
            prompter,
            async_prompter: None,
            hooks,
            sink,
            ctx,
            config,
            messages: vec![],
            usage: UsageStats::default(),
            denials: vec![],
            parent_tool_use_id: None,
            last_prompt_tokens: 0,
            stop_hook_max_continues: DEFAULT_STOP_HOOK_CONTINUES,
            budget_warned: false,
            model_calls_since_todo: 0,
            journal: None,
            prune_turn_starts: vec![0],
            prune_stats: PruneStats::default(),
            env_deltas: env_context::DeltaStore::new(),
            lsp_feedback_counts: HashMap::new(),
            pending_hook_context: vec![],
            dynamic_watch_paths: vec![],
        }
    }

    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
        // A resumed/reset session restarts env-context deltas from a full
        // snapshot (Viec 5).
        self.env_deltas.reset();

        self.last_prompt_tokens = 0;

        self.prune_turn_starts = vec![0];
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn set_stop_hook_max_continues(&mut self, n: usize) {
        self.stop_hook_max_continues = n.max(1);
    }

    /// Install the async prompter (M4): the ACP server calls this right
    /// after construction so permission questions round-trip to the
    /// client. A later call replaces the prompter.
    pub fn set_async_prompter(&mut self, prompter: Arc<dyn AsyncPrompter>) {
        self.async_prompter = Some(prompter);
    }

    /// Apply a prompter's answer: `None` means the tool may run,
    /// `Some(reason)` means it is denied. Shared by the sync REPL
    /// prompter and the async ACP prompter (M4), so both map an
    /// `Always` answer to a persisted allow rule the same way.
    fn apply_prompt_answer(&self, answer: PromptAnswer, tool_name: &str) -> Option<String> {
        match answer {
            PromptAnswer::Yes => None,
            PromptAnswer::Always(rules) => {
                for rule in rules.lines() {
                    self.permissions.add_allow_rule(rule);
                }
                None
            }
            PromptAnswer::No => Some(format!("User denied permission for '{tool_name}'")),
        }
    }

    pub fn tools_for_request(&self) -> Vec<ToolDefinition> {
        let defs =
            crate::tool_bundles::definitions_for_bundle(&self.tools, self.config.tool_bundle);
        if self.config.permission_mode == PermissionMode::Plan {
            plan::plan_mode_tools(defs)
        } else {
            defs
        }
    }

    fn request_messages(&mut self) -> Vec<ChatMessage> {
        let mut msgs: Vec<ChatMessage> = Vec::with_capacity(self.messages.len() + 1);
        let mut prompt = self.config.system_prompt.clone();
        if self.config.env_context_deltas {
            // Viec 5: append this request's env payload (full snapshot on
            // the first request, changed fields only afterwards) after the
            // stable prompt text, i.e. inside the cache-volatile tail.
            let cwd = self.ctx.cwd.lock().unwrap().clone();
            let snap = env_context::EnvSnapshot::collect(&cwd);
            let delta = self.env_deltas.diff(&snap);
            prompt.push_str(&env_context::render_delta(&delta));
        }
        if !prompt.is_empty() {
            msgs.push(ChatMessage::system(prompt));
        }
        msgs.extend(self.messages.iter().cloned());
        crate::plan::ensure_tool_responses(msgs)
    }

    fn run_result(
        &self,
        subtype: &str,
        is_error: bool,
        result: String,
        duration_ms: u64,
        duration_api_ms: u64,
        num_turns: usize,
    ) -> RunResult {
        RunResult {
            record_type: "result".into(),
            subtype: subtype.into(),
            is_error,
            duration_ms,
            duration_api_ms,
            num_turns,
            result,
            session_id: self.ctx.session_id.clone(),

            total_cost_usd: crate::budget::estimate_cost_usd(
                &self.config.model,
                self.usage.input_tokens,
                self.usage.output_tokens,
            )
            .unwrap_or(0.0),
            usage: self.usage.clone(),
            model: self.config.model.clone(),
            permission_denials: self.denials.clone(),
        }
    }

    /// A permission denial: record it, emit the sink event, and fire the
    /// `PermissionDenied` hook (matcher: tool name). The hook cannot undo
    /// the denial; its `hookSpecificOutput.retry: true` is returned so the
    /// caller can tell the model it may retry the call.
    async fn record_denial(
        &mut self,
        tool_name: &str,
        tool_use_id: &str,
        input: &Value,
        reason: &str,
    ) -> HookOutcome {
        let denial = PermissionDenial {
            tool_name: tool_name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            tool_input: input.clone(),
        };
        self.sink.emit(AgentEvent::PermissionDenied(denial.clone()));
        self.denials.push(denial);
        let outcome = self
            .hooks
            .run(
                HookEvent::PermissionDenied,
                tool_name,
                json!({
                    "tool_name": tool_name,
                    "tool_input": input,
                    "tool_use_id": tool_use_id,
                    "reason": reason,
                }),
            )
            .await;
        self.emit_system_message(HookEvent::PermissionDenied, &outcome);
        outcome
    }

    async fn notify_permission_prompt(&self, tool_name: &str) {
        let note = self
            .hooks
            .run(
                HookEvent::Notification,
                "permission_prompt",
                json!({
                    "message": format!("zen needs your permission to use {}", tool_name),
                    "notification_type": "permission_prompt",
                }),
            )
            .await;
        self.emit_system_message(HookEvent::Notification, &note);
    }

    fn emit_system_message(&self, event: HookEvent, outcome: &HookOutcome) {
        if let Some(text) = &outcome.system_message
            && !text.is_empty()
        {
            self.sink.emit(AgentEvent::SystemMessage {
                event: event.as_str().to_string(),
                text: text.clone(),
            });
        }
    }

    /// Map a model-call error to a `StopFailure` matcher value
    /// (`rate_limit`, `overloaded`, `authentication_failed`, ...).
    fn classify_api_error(err: &str) -> &'static str {
        let e = err.to_ascii_lowercase();
        if e.contains("rate limit") || e.contains("429") || e.contains("too many requests") {
            "rate_limit"
        } else if e.contains("overloaded") || e.contains("529") || e.contains("503") {
            "overloaded"
        } else if e.contains("auth") || e.contains("401") || e.contains("403") {
            "authentication_failed"
        } else if e.contains("billing") || e.contains("payment") || e.contains("402") {
            "billing_error"
        } else if e.contains("model not found") || e.contains("404") {
            "model_not_found"
        } else if e.contains("max_output_tokens") || e.contains("max tokens") {
            "max_output_tokens"
        } else if e.contains("invalid request") || e.contains("400") {
            "invalid_request"
        } else if e.contains("server error") || e.contains("500") {
            "server_error"
        } else {
            "unknown"
        }
    }

    /// Emit an assistant message, running `MessageDisplay` first when the
    /// message has text. A hook's replacement text changes only what is
    /// shown: the message already pushed to `self.messages` keeps the
    /// original text, so the model never sees the replacement.
    async fn emit_assistant(&self, message: &ChatMessage, turn_id: &str) {
        let mut display = message.clone();
        let text = message.text();
        if !text.trim().is_empty() {
            let outcome = self
                .hooks
                .run(
                    HookEvent::MessageDisplay,
                    "",
                    json!({
                        "turn_id": turn_id,
                        "index": 0,
                        "final": true,
                        "delta": text,
                    }),
                )
                .await;
            self.emit_system_message(HookEvent::MessageDisplay, &outcome);
            if let Some(replacement) = outcome.display_replacement
                && matches!(message.content, Some(MessageContent::Text(_)))
            {
                display = ChatMessage::assistant(Some(replacement), message.tool_calls.clone());
            }
        }
        self.sink.emit(AgentEvent::Assistant {
            message: display,
            parent_tool_use_id: self.parent_tool_use_id.clone(),
        });
    }

    /// `PostToolBatch`: once per batch, after every call resolved. The
    /// entries carry the tool name, input, id, and the exact result text
    /// the model received. A block (exit 2 / decision block) or
    /// `continue: false` stops the loop before the next model call and
    /// returns the stop reason; `additionalContext` is injected as a user
    /// message for the next model call.
    async fn run_post_tool_batch(
        &mut self,
        tool_calls: &[ToolCall],
        msgs_before: usize,
    ) -> Option<String> {
        let mut entries: Vec<Value> = Vec::new();
        for tc in tool_calls {
            let response = self.messages[msgs_before..]
                .iter()
                .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(tc.id.as_str()))
                .map(|m| m.text())
                .unwrap_or_default();
            let input: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| Value::String(tc.function.arguments.clone()));
            entries.push(json!({
                "tool_name": tc.function.name,
                "tool_input": input,
                "tool_use_id": tc.id,
                "tool_response": response,
            }));
        }
        if entries.is_empty() {
            return None;
        }
        let outcome = self
            .hooks
            .run(
                HookEvent::PostToolBatch,
                "",
                json!({ "tool_calls": entries }),
            )
            .await;
        self.emit_system_message(HookEvent::PostToolBatch, &outcome);
        if outcome.blocked || outcome.stop_run {
            return Some(
                outcome
                    .stop_reason
                    .or(outcome.feedback)
                    .unwrap_or_else(|| "stopped by PostToolBatch hook".into()),
            );
        }
        if let Some(fb) = outcome.feedback.filter(|s| !s.trim().is_empty()) {
            self.messages.push(ChatMessage::user(fb));
        }
        None
    }

    /// `TaskCreated` / `TaskCompleted` for a TodoWrite that is about to
    /// run: diff the requested list against the current one and fire one
    /// event per new / newly-completed item. Returns the veto reason when
    /// a hook blocks a creation or a completion.
    async fn check_task_hooks(&self, args: &Value) -> Option<String> {
        let new_list = args.get("todos").and_then(|v| v.as_array())?;
        let old = {
            let Ok(guard) = self.ctx.todos.lock() else {
                return None;
            };
            guard.clone()
        };
        for (i, item) in new_list.iter().enumerate() {
            let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.trim().is_empty() {
                continue;
            }
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let task_id = format!("todo-{i}");
            let mut payload = json!({
                "task_id": task_id,
                "task_subject": content,
            });
            if let Some(desc) = item.get("activeForm").and_then(|v| v.as_str()) {
                payload["task_description"] = json!(desc);
            }
            let prev = old.iter().find(|t| t.content == content);
            let event = match prev {
                None => Some(HookEvent::TaskCreated),
                Some(p) if p.status != "completed" && status == "completed" => {
                    Some(HookEvent::TaskCompleted)
                }
                _ => None,
            };
            let Some(event) = event else { continue };
            let out = self.hooks.run(event, "", payload).await;
            self.emit_system_message(event, &out);
            if out.blocked {
                return Some(out.feedback.unwrap_or_else(|| {
                    format!(
                        "{} blocked by hook: {}",
                        event.as_str(),
                        content.chars().take(120).collect::<String>()
                    )
                }));
            }
        }
        None
    }

    fn emit_tool_result(&self, tool_use_id: &str, content: &str, is_error: bool, parallel: bool) {
        self.sink.emit(AgentEvent::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error,
            parent_tool_use_id: self.parent_tool_use_id.clone(),
            parallel,
        });
    }

    fn journal_result(&self, tool_use_id: &str, content: &str, is_error: bool, final_result: bool) {
        if let Some(j) = &self.journal {
            j.tool_result(tool_use_id, content, is_error, final_result);
        }
    }

    fn plan_text(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();

        let mut glue = false;
        for m in self.messages.iter().rev() {
            if m.role == "user" {
                if m.text_ref() == Some(TRUNCATION_CONTINUE_NUDGE) {
                    glue = true;
                    continue;
                }
                break;
            }
            if m.role == "assistant"
                && let Some(c) = m.text_ref()
            {
                if glue && !parts.is_empty() {
                    if let Some(last) = parts.last_mut() {
                        last.insert_str(0, c);
                    }
                } else if !c.trim().is_empty() {
                    parts.push(c.to_string());
                }
                glue = false;
            }
        }
        parts.reverse();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    pub async fn run_turn(&mut self, prompt: &str) -> RunResult {
        let started = Instant::now();
        let mut duration_api_ms: u64 = 0;

        let ups = self
            .hooks
            .run(HookEvent::UserPromptSubmit, "", json!({ "prompt": prompt }))
            .await;
        self.emit_system_message(HookEvent::UserPromptSubmit, &ups);
        if ups.stop_run {
            return self.run_result(
                "success",
                false,
                ups.stop_reason.or(ups.feedback).unwrap_or_default(),
                started.elapsed().as_millis() as u64,
                duration_api_ms,
                0,
            );
        }
        if ups.blocked {
            let feedback = ups
                .feedback
                .unwrap_or_else(|| "user prompt rejected by hook".into());
            return self.run_result(
                "error_during_execution",
                true,
                feedback,
                started.elapsed().as_millis() as u64,
                duration_api_ms,
                0,
            );
        }
        let mut user_content = prompt.to_string();
        if let Some(extra) = ups.feedback
            && !extra.is_empty()
        {
            user_content.push_str("\n\n");
            user_content.push_str(&extra);
        }
        // Context stashed by hooks since the last turn (PostModelSwitch
        // additionalContext, DirectoryAdded systemMessage).
        for pending in std::mem::take(&mut self.pending_hook_context) {
            if !pending.trim().is_empty() {
                user_content.push_str("\n\n");
                user_content.push_str(&pending);
            }
        }
        self.messages.push(ChatMessage::user(user_content));

        let mut num_turns: usize = 0;
        let mut final_text: String;

        let mut stop_continues: usize = 0;
        let plan_mode = self.config.permission_mode == PermissionMode::Plan;
        let mut turn_guard = guard::TurnGuard::default();
        turn_guard.set_cwd(lock_or_recover(&self.ctx.cwd, "ctx.cwd").clone());
        turn_guard.set_scope(self.config.done_scope.clone());
        let mut loop_guard = loop_guard::LoopGuard::new(self.config.loop_stuck_k);
        let mut finish_gate = finish::FinishGate::default();
        let mut empty_requeries = 0usize;

        let mut continuations = 0usize;
        let mut stitched = String::new();

        loop {
            if num_turns >= self.config.max_turns {
                let stop_text = format!(
                    "Reached maximum number of turns ({})",
                    self.config.max_turns
                );
                let (text, api_ms) = self.summarize_runaway(&stop_text).await;
                duration_api_ms += api_ms;
                return self.run_result(
                    "error_max_turns",
                    true,
                    text,
                    started.elapsed().as_millis() as u64,
                    duration_api_ms,
                    num_turns,
                );
            }

            if let Some(threshold) = self.config.auto_compact_threshold {
                let used = compaction::context_tokens(self);
                if used > threshold && !self.messages.is_empty() {
                    let _ = compaction::compact_impl(self, None, "auto").await;
                }
            }

            self.prune_turn_starts.push(self.messages.len());
            self.prune_old_results();

            num_turns += 1;
            if self.config.depth == 0 {
                self.maybe_reinject_todos();
            }
            let messages = self.request_messages();
            let tools = self.tools_for_request();

            if let Some(policy) = self.config.budget.clone() {
                let model = self.config.model.clone();
                if !crate::budget::is_free_model(&model) {
                    let spent = crate::budget::estimate_cost_usd(
                        &model,
                        self.usage.input_tokens,
                        self.usage.output_tokens,
                    );
                    let est_next = crate::budget::estimate_cost_usd(
                        &model,
                        crate::compaction::context_tokens(self),
                        crate::budget::DEFAULT_OUTPUT_ESTIMATE_TOKENS,
                    );
                    if let (Some(spent_usd), Some(est_cost_usd)) = (spent, est_next) {
                        match crate::budget::check_budget(
                            &policy,
                            spent_usd,
                            est_cost_usd,
                            self.budget_warned,
                        ) {
                            BudgetVerdict::Allow => {}
                            BudgetVerdict::Warn { spent_usd, cap_usd } => {
                                self.budget_warned = true;
                                self.sink.emit(AgentEvent::SystemMessage {
                                    event: "budget".to_string(),
                                    text: crate::budget::warn_message(spent_usd, cap_usd),
                                });
                            }
                            BudgetVerdict::Block {
                                spent_usd,
                                cap_usd,
                                est_cost_usd,
                            } => {
                                return self.run_result(
                                    "error_budget_exceeded",
                                    true,
                                    crate::budget::block_message(spent_usd, cap_usd, est_cost_usd),
                                    started.elapsed().as_millis() as u64,
                                    duration_api_ms,
                                    num_turns,
                                );
                            }
                        }
                    }
                }
            }

            let (resp, api_ms, _model) = match crate::client::chat_with_fallback(
                &self.llm,
                &self.config.model,
                &self.config.fallback_models,
                messages,
                tools,
                self.config.max_output_tokens,
            )
            .await
            {
                Ok(v) => v,
                Err(e)
                    if is_unparsed_sample(&e.to_string())
                        && empty_requeries < MAX_EMPTY_REQUERIES =>
                {
                    empty_requeries += 1;
                    self.messages.push(ChatMessage::user(UNPARSED_REPLY_NUDGE));
                    continue;
                }
                Err(e) => {
                    // `StopFailure`: the turn ends on an API error, not on
                    // a model decision. Hooks run for logging/alerting only;
                    // their output cannot change the outcome.
                    let err_text = e.to_string();
                    let error_kind = Self::classify_api_error(&err_text);
                    let failure = self
                        .hooks
                        .run(
                            HookEvent::StopFailure,
                            error_kind,
                            json!({
                                "error": error_kind,
                                "error_details": err_text,
                                "last_assistant_message": err_text,
                            }),
                        )
                        .await;
                    self.emit_system_message(HookEvent::StopFailure, &failure);
                    return self.run_result(
                        "error_during_execution",
                        true,
                        err_text,
                        started.elapsed().as_millis() as u64,
                        duration_api_ms,
                        num_turns,
                    );
                }
            };
            duration_api_ms += api_ms;

            self.usage.input_tokens += resp.usage.prompt_tokens;
            self.usage.output_tokens += resp.usage.completion_tokens;
            self.usage.cache_read_input_tokens += resp.usage.cache_read_input_tokens;
            self.last_prompt_tokens = resp.usage.prompt_tokens;

            let mut resp = resp;

            if crate::client::is_content_filtered(resp.finish_reason.as_deref())
                && resp
                    .message
                    .tool_calls
                    .as_ref()
                    .is_none_or(|c| c.is_empty())
            {
                let text = resp.message.text();
                if !text.trim().is_empty() {
                    self.messages.push(resp.message.clone());
                    self.emit_assistant(
                        &resp.message,
                        &format!("{}:{num_turns}", self.ctx.session_id),
                    )
                    .await;
                    if let Some(j) = &self.journal {
                        j.assistant(&resp.message);
                    }
                }
                // Distinct subtype (M3, Phase 2 #9): the ACP layer
                // maps this to stopReason "refusal".
                return self.run_result(
                    "error_refusal",
                    true,
                    content_filter_message(&text),
                    started.elapsed().as_millis() as u64,
                    duration_api_ms,
                    num_turns,
                );
            }
            if is_truncated(resp.finish_reason.as_deref()) {
                let calls = resp.message.tool_calls.clone().unwrap_or_default();
                let broken = calls.iter().any(|c| {
                    serde_json::from_str::<Value>(&c.function.arguments)
                        .map_or(true, |v| !v.is_object())
                });
                if calls.is_empty() || broken {
                    let mut msg = resp.message.clone();
                    msg.tool_calls = None;
                    let text = msg.text();

                    if !text.is_empty() {
                        self.messages.push(msg.clone());
                        self.emit_assistant(&msg, &format!("{}:{num_turns}", self.ctx.session_id))
                            .await;
                        if let Some(j) = &self.journal {
                            j.assistant(&msg);
                        }
                    }
                    stitched.push_str(&text);
                    if continuations >= self.config.max_continuations {
                        let elapsed = started.elapsed().as_millis() as u64;
                        if broken {
                            // Distinct subtype (M3, Phase 2 #9): the ACP
                            // layer maps this to stopReason "max_tokens".
                            return self.run_result(
                                "error_max_tokens",
                                true,
                                format!(
                                    "The model's tool call was cut off by the output token limit {} time(s) in a row; no tool was run. Ask for smaller steps or use a model with a larger output limit.",
                                    continuations + 1
                                ),
                                elapsed,
                                duration_api_ms,
                                num_turns,
                            );
                        }
                        self.sink.emit(AgentEvent::SystemMessage {
                            event: "Truncation".into(),
                            text: format!(
                                "The reply was cut off by the output token limit {} time(s) in a row; the answer may be incomplete.",
                                continuations + 1
                            ),
                        });
                        return self.run_result(
                            "success",
                            false,
                            stitched,
                            elapsed,
                            duration_api_ms,
                            num_turns,
                        );
                    }
                    continuations += 1;
                    let nudge = if broken {
                        TRUNCATED_CALL_NUDGE
                    } else {
                        TRUNCATION_CONTINUE_NUDGE
                    };
                    self.messages.push(ChatMessage::user(nudge));
                    continue;
                }
            }
            continuations = 0;

            let mut text_call_id: Option<String> = None;
            if resp
                .message
                .tool_calls
                .as_ref()
                .is_none_or(|c| c.is_empty())
            {
                let text = resp.message.text();
                if let Some((name, arguments)) =
                    repair::tool_call_in_text(&text, &self.tools.names())
                {
                    let id = format!("zen_text_call_{}", num_turns);
                    resp.message.content = None;
                    resp.message.tool_calls = Some(vec![ToolCall {
                        id: id.clone(),
                        call_type: "function".into(),
                        function: FunctionCall { name, arguments },
                    }]);
                    text_call_id = Some(id);
                }
            }

            self.messages.push(resp.message.clone());
            self.emit_assistant(
                &resp.message,
                &format!("{}:{num_turns}", self.ctx.session_id),
            )
            .await;

            let assistant = &resp.message;
            let tool_calls = assistant.tool_calls.clone().unwrap_or_default();
            final_text = assistant.text();

            if tool_calls.is_empty()
                && final_text.trim().is_empty()
                && empty_requeries < MAX_EMPTY_REQUERIES
            {
                empty_requeries += 1;
                self.messages.pop();
                self.messages.push(ChatMessage::user(EMPTY_REPLY_NUDGE));
                continue;
            }
            if let Some(j) = &self.journal {
                j.assistant(assistant);
            }

            if !stitched.is_empty() {
                let pieces = std::mem::take(&mut stitched);
                if tool_calls.is_empty() {
                    final_text.insert_str(0, &pieces);
                }
            }

            if tool_calls.is_empty() {
                let stop = if self.config.depth == 0 {
                    self.hooks
                        .run(
                            HookEvent::Stop,
                            "",
                            json!({
                                "stop_hook_active": stop_continues > 0,
                                "last_assistant_message": final_text,
                            }),
                        )
                        .await
                } else {
                    HookOutcome::default()
                };
                self.emit_system_message(HookEvent::Stop, &stop);
                if stop.stop_run {
                    return self.run_result(
                        "success",
                        false,
                        stop.stop_reason
                            .or(stop.feedback)
                            .unwrap_or_else(|| final_text.clone()),
                        started.elapsed().as_millis() as u64,
                        duration_api_ms,
                        num_turns,
                    );
                }
                if stop.blocked {
                    if stop_continues >= self.stop_hook_max_continues {
                        return self.run_result(
                            "success",
                            false,
                            final_text,
                            started.elapsed().as_millis() as u64,
                            duration_api_ms,
                            num_turns,
                        );
                    }
                    stop_continues += 1;
                    let fb = stop.feedback.unwrap_or_else(|| "continue".into());
                    self.messages.push(ChatMessage::user(fb));
                    continue;
                }

                if self.config.depth == 0 && !plan_mode {
                    let decision = finish_gate
                        .decide(
                            self.config.verify_reminder,
                            self.config.verify_command.as_deref(),
                            std::time::Duration::from_secs(self.config.verify_timeout_secs.max(1)),
                            &self.ctx.cwd(),
                            &self.ctx.env,
                        )
                        .await;
                    match decision {
                        finish::Decision::Continue(text)
                            if stop_continues < self.stop_hook_max_continues =>
                        {
                            stop_continues += 1;
                            self.messages.push(ChatMessage::user(text));
                            continue;
                        }
                        finish::Decision::FinishWithNote(text) => {
                            self.sink.emit(AgentEvent::SystemMessage {
                                event: "Verify".into(),
                                text,
                            });
                        }
                        _ => {}
                    }
                }
                let result_text = if plan_mode {
                    self.plan_text().unwrap_or_else(|| final_text.clone())
                } else {
                    final_text
                };
                return self.run_result(
                    "success",
                    false,
                    result_text,
                    started.elapsed().as_millis() as u64,
                    duration_api_ms,
                    num_turns,
                );
            }

            let mut stop_run: Option<String> = None;
            let mut batch: Vec<Slot> = Vec::new();
            let cwd_before = self.ctx.cwd();
            let msgs_before = self.messages.len();
            for tc in &tool_calls {
                let slot = self
                    .prepare_call(
                        tc,
                        text_call_id.as_deref(),
                        plan_mode,
                        &stop_run,
                        &mut loop_guard,
                    )
                    .await;
                if let Slot::Stop(id, content, reason) = slot {
                    batch.push(Slot::Done(id, content, true));
                    stop_run = Some(reason);
                    continue;
                }
                let alone = matches!(&slot, Slot::Run(p) if !p.parallel);
                if alone {
                    let pending = std::mem::take(&mut batch);
                    self.flush_calls(
                        pending,
                        &mut stop_run,
                        &mut finish_gate,
                        &mut turn_guard,
                        &mut loop_guard,
                        &final_text,
                    )
                    .await;
                    self.flush_calls(
                        vec![slot],
                        &mut stop_run,
                        &mut finish_gate,
                        &mut turn_guard,
                        &mut loop_guard,
                        &final_text,
                    )
                    .await;
                } else {
                    batch.push(slot);
                }
            }
            self.flush_calls(
                batch,
                &mut stop_run,
                &mut finish_gate,
                &mut turn_guard,
                &mut loop_guard,
                &final_text,
            )
            .await;
            // `CwdChanged`: a Bash `cd` in this batch moved the session
            // working directory (the Bash tool persists `$PWD` into the
            // context). `watchPaths` from the hook replaces the dynamic
            // FileChanged watch list for the REPL.
            let cwd_after = self.ctx.cwd();
            if cwd_after != cwd_before {
                let changed = self
                    .hooks
                    .run(
                        HookEvent::CwdChanged,
                        "",
                        json!({
                            "old_cwd": cwd_before.display().to_string(),
                            "new_cwd": cwd_after.display().to_string(),
                        }),
                    )
                    .await;
                self.emit_system_message(HookEvent::CwdChanged, &changed);
                if let Some(paths) = changed.watch_paths {
                    self.dynamic_watch_paths = paths.iter().map(std::path::PathBuf::from).collect();
                }
            }
            // `PostToolBatch`: once per batch, after every call resolved
            // and before the next model call.
            if let Some(text) = self.run_post_tool_batch(&tool_calls, msgs_before).await
                && stop_run.is_none()
            {
                stop_run = Some(text);
            }
            if let Some(stuck) = loop_guard.take_stuck().or_else(|| loop_guard.end_turn()) {
                let (text, api_ms) = self.summarize_runaway(&stuck.message()).await;
                duration_api_ms += api_ms;
                return self.run_result(
                    "loop_stuck",
                    true,
                    text,
                    started.elapsed().as_millis() as u64,
                    duration_api_ms,
                    num_turns,
                );
            }
            if let Some(text) = stop_run {
                let result_text = if plan_mode {
                    self.plan_text().unwrap_or_else(|| text.clone())
                } else {
                    text
                };
                return self.run_result(
                    "success",
                    false,
                    result_text,
                    started.elapsed().as_millis() as u64,
                    duration_api_ms,
                    num_turns,
                );
            }
        }
    }

    /// One final tool-less model call asking for a short summary of a
    /// runaway run (stopped by `loop_stuck` or `error_max_turns`).
    /// Returns the stop text with the summary appended, plus the API
    /// milliseconds the call took (0 when the call failed). Falls back
    /// to the plain stop text when the call fails or returns empty text,
    /// so a runaway run never fails twice. The call gets no tools and
    /// runs at most once, so it cannot loop itself.
    async fn summarize_runaway(&mut self, stop_text: &str) -> (String, u64) {
        let mut messages = self.request_messages();
        messages.push(ChatMessage::user(
            "The run was stopped before finishing. In a few short lines, \
             summarize what was already done, what is still unfinished, and \
             what remains to be done. Do not call any tools.",
        ));
        let (resp, api_ms, _model) = match crate::client::chat_with_fallback(
            &self.llm,
            &self.config.model,
            &self.config.fallback_models,
            messages,
            Vec::new(),
            Some(512),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => return (stop_text.to_string(), 0),
        };
        self.usage.input_tokens += resp.usage.prompt_tokens;
        self.usage.output_tokens += resp.usage.completion_tokens;
        self.usage.cache_read_input_tokens += resp.usage.cache_read_input_tokens;
        let summary = resp
            .message
            .content
            .as_ref()
            .map(|c| c.text())
            .unwrap_or_default();
        let summary = summary.trim();
        if summary.is_empty() {
            return (stop_text.to_string(), api_ms);
        }
        (format!("{stop_text}\n\nSummary:\n{summary}"), api_ms)
    }

    fn call_ctx(&self, tool_use_id: &str, tool_name: &str) -> ToolContext {
        let mut call_ctx = self.ctx.clone();
        call_ctx.current_tool_use_id = Some(tool_use_id.to_string());

        if tool_name == "Bash" {
            call_ctx.tool_timeout_ms = self
                .config
                .tool_timeouts
                .get("Bash")
                .copied()
                .filter(|t| *t > 0);
        }
        call_ctx
    }

    async fn prepare_call(
        &mut self,
        tc: &ToolCall,
        text_call_id: Option<&str>,
        plan_mode: bool,
        stop_run: &Option<String>,
        loop_guard: &mut loop_guard::LoopGuard,
    ) -> Slot {
        let tool_use_id = tc.id.clone();

        let mut repair_notes: Vec<String> = Vec::new();
        if text_call_id == Some(tool_use_id.as_str()) {
            repair_notes
                .push("the call was written as message text; it was run as a tool call".into());
        }
        let mut tool_name = tc.function.name.clone();
        if self.tools.get(&tool_name).is_none()
            && let Some(mapped) = repair::resolve_tool_name(&tool_name, &self.tools.names())
        {
            if repair::has_template_tokens(&tool_name) {
                repair_notes.push(format!(
                    "tool name had chat-template tokens; mapped to '{}'",
                    mapped
                ));
            } else {
                repair_notes.push(format!("tool name '{}' mapped to '{}'", tool_name, mapped));
            }
            tool_name = mapped;
        }
        let schema = self.tools.get(&tool_name).map(|t| t.parameters_schema());
        let args: Value = match repair::repair_args(&tc.function.arguments, schema.as_ref()) {
            Ok((v, notes)) => {
                repair_notes.extend(notes);
                v
            }
            Err(e) => {
                let content = format!("Invalid JSON arguments for tool '{}': {}", tool_name, e);
                // The arguments never parsed, so there is no Value to key
                // on: count the rejection by tool + reason only.
                loop_guard.observe_rejected(
                    &tool_name,
                    &Value::Null,
                    loop_guard::rejection_reason::INVALID_ARGS,
                );
                return Slot::Done(tool_use_id, content, true);
            }
        };
        if !repair_notes.is_empty() {
            self.rewrite_tool_call(&tool_use_id, &tool_name, &args);
        }

        if stop_run.is_some() {
            return Slot::Done(
                tool_use_id,
                "[tool call skipped: run stopping]".to_string(),
                false,
            );
        }

        let pre = self
            .hooks
            .run(
                HookEvent::PreToolUse,
                &tool_name,
                json!({
                    "tool_name": tool_name,
                    "tool_input": args,
                    "tool_use_id": tool_use_id,
                }),
            )
            .await;
        self.emit_system_message(HookEvent::PreToolUse, &pre);
        if pre.stop_run {
            let reason = pre
                .stop_reason
                .clone()
                .or(pre.feedback.clone())
                .unwrap_or_else(|| "stopped by PreToolUse hook".into());
            let content = format!("[tool call skipped: {}]", reason);
            return Slot::Stop(tool_use_id, content, reason);
        }
        if pre.blocked {
            let content = pre
                .feedback
                .unwrap_or_else(|| format!("Tool '{}' blocked by PreToolUse hook", tool_name));
            loop_guard.observe_rejected(
                &tool_name,
                &args,
                loop_guard::rejection_reason::HOOK_BLOCKED,
            );
            return Slot::Done(tool_use_id, content, true);
        }

        let args = match pre.updated_input.clone() {
            Some(Value::Object(o)) => Value::Object(o),
            _ => args,
        };

        let tool = match self.tools.get(&tool_name) {
            Some(t) => t,
            None => {
                let content = format!(
                    "Unknown tool '{}'. Available tools: {}",
                    tool_name,
                    self.tools.names().join(", ")
                );
                loop_guard.observe_rejected(
                    &tool_name,
                    &args,
                    loop_guard::rejection_reason::UNKNOWN_TOOL,
                );
                return Slot::Done(tool_use_id, content, true);
            }
        };

        if crate::tool_bundles::is_deferred(&self.tools, self.config.tool_bundle, &tool_name) {
            let content = format!(
                "Tool '{}' is deferred in tool bundle '{}': its schema was not sent to save context. \
                 Do not call it; use a tool from the active bundle instead.",
                tool_name,
                self.config.tool_bundle.as_str()
            );
            loop_guard.observe_rejected(&tool_name, &args, loop_guard::rejection_reason::DEFERRED);
            return Slot::Done(tool_use_id, content, true);
        }
        let read_only = tool.is_read_only();

        if plan_mode && !read_only {
            let content = format!("Tool '{}' is not available in plan mode", tool_name);
            loop_guard.observe_rejected(&tool_name, &args, loop_guard::rejection_reason::PLAN_MODE);
            return Slot::Done(tool_use_id, content, true);
        }

        let checked = self
            .permissions
            .check(&tool_name, &args, read_only, &self.ctx);
        let ask_rule = matches!(checked, PermissionDecision::Ask)
            && self
                .permissions
                .ask_rule_matches(&tool_name, &args, &self.ctx);
        let mut decision = combine_permission(
            checked,
            pre.permission_override.clone(),
            ask_rule,
            self.config.permission_mode,
        );
        // `PermissionRequest`: a permission prompt is about to be shown.
        // A hook may decide the request itself (allow/deny/ask) or block
        // it (exit 2 / decision block) instead of asking the user.
        if matches!(decision, PermissionDecision::Ask) {
            let req = self
                .hooks
                .run(
                    HookEvent::PermissionRequest,
                    &tool_name,
                    json!({
                        "tool_name": tool_name,
                        "tool_input": args,
                        "tool_use_id": tool_use_id,
                    }),
                )
                .await;
            self.emit_system_message(HookEvent::PermissionRequest, &req);
            if req.blocked {
                decision = PermissionDecision::Deny(req.feedback.unwrap_or_else(|| {
                    format!("Permission request for '{tool_name}' blocked by hook")
                }));
            } else if let Some(po) = req.permission_override {
                decision = po;
            }
        }
        let mut allowed = true;
        let mut deny_reason = String::new();
        match decision {
            PermissionDecision::Allow => {}
            PermissionDecision::Deny(reason) => {
                allowed = false;
                deny_reason = reason;
            }
            PermissionDecision::Ask => {
                // The async prompter wins when present (M4): the ACP
                // server round-trips the question to the client. The
                // sync terminal prompter is the REPL path; with neither,
                // a headless run cannot ask, so it denies.
                let answer = if let Some(ap) = self.async_prompter.clone() {
                    self.notify_permission_prompt(&tool_name).await;
                    Some(
                        ap.ask_permission(&tool_name, &args, &tool_use_id, read_only)
                            .await,
                    )
                } else {
                    match self.prompter.clone() {
                        Some(p) => {
                            self.notify_permission_prompt(&tool_name).await;
                            Some(p.ask_permission(&tool_name, &args))
                        }
                        None => None,
                    }
                };
                match answer {
                    Some(answer) => {
                        if let Some(reason) = self.apply_prompt_answer(answer, &tool_name) {
                            allowed = false;
                            deny_reason = reason;
                        }
                    }
                    None => {
                        allowed = false;
                        deny_reason = format!(
                            "Permission required for '{tool_name}' in non-interactive mode"
                        );
                    }
                }
            }
        }
        if !allowed {
            let denied = self
                .record_denial(&tool_name, &tool_use_id, &args, &deny_reason)
                .await;
            if denied.allow_retry {
                deny_reason.push_str(
                    "\n[hook: a PermissionDenied hook says you may retry this tool call]",
                );
            }
            // A repeated denial is a loop too: the model keeps asking for
            // a permission it will not get (including a user who already
            // answered No, and headless runs with no prompter).
            loop_guard.observe_rejected(&tool_name, &args, loop_guard::rejection_reason::DENIED);
            return Slot::Done(tool_use_id, deny_reason, true);
        }
        // `TaskCreated` / `TaskCompleted`: a TodoWrite that will run is
        // diffed against the current list first; a hook may veto a
        // creation or a completion (exit 2 / decision block) and the
        // whole TodoWrite is rejected with the reason.
        if tool_name == "TodoWrite"
            && let Some(veto) = self.check_task_hooks(&args).await
        {
            loop_guard.observe_rejected(
                &tool_name,
                &args,
                loop_guard::rejection_reason::HOOK_BLOCKED,
            );
            return Slot::Done(tool_use_id, veto, true);
        }
        let parallel = read_only && is_parallel_safe(&tool_name);
        Slot::Run(PendingCall {
            tool_use_id,
            tool_name,
            args,
            repair_notes,
            tool,
            parallel,
        })
    }

    async fn flush_calls(
        &mut self,
        slots: Vec<Slot>,
        stop_run: &mut Option<String>,
        finish_gate: &mut finish::FinishGate,
        turn_guard: &mut guard::TurnGuard,
        loop_guard: &mut loop_guard::LoopGuard,
        final_text: &str,
    ) {
        if slots.is_empty() {
            return;
        }
        let runnable = slots.iter().filter(|s| matches!(s, Slot::Run(_))).count();
        let mut outs: Vec<Option<ToolOutput>> = (0..slots.len()).map(|_| None).collect();

        let mut ran_parallel = vec![false; slots.len()];
        if runnable > 1 {
            let limit = self.config.max_parallel_tool_calls.max(1);
            let run_idx: Vec<usize> = slots
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s, Slot::Run(_)))
                .map(|(i, _)| i)
                .collect();
            for chunk in run_idx.chunks(limit) {
                let mut handles = Vec::with_capacity(chunk.len());
                for &i in chunk {
                    let Slot::Run(p) = &slots[i] else { continue };
                    let tool = p.tool.clone();
                    let args = p.args.clone();
                    let ctx = self.call_ctx(&p.tool_use_id, &p.tool_name);
                    let timeout = self.config.tool_timeout_with_source(&p.tool_name);
                    let journal = self.journal.clone();
                    let id = p.tool_use_id.clone();
                    let sink = self.sink.clone();
                    let tool_name = p.tool_name.clone();
                    handles.push((
                        i,
                        tokio::spawn(async move {
                            let out = run_tool_with_heartbeat(
                                tool,
                                args,
                                ctx,
                                timeout,
                                sink,
                                id.clone(),
                                tool_name,
                                TOOL_PROGRESS_INTERVAL,
                            )
                            .await;

                            if let Some(j) = journal {
                                j.tool_result(&id, &out.content, out.is_error, false);
                            }
                            out
                        }),
                    ));
                }
                if chunk.len() > 1 {
                    for &i in chunk {
                        ran_parallel[i] = true;
                    }
                }
                for (i, h) in handles {
                    outs[i] =
                        Some(h.await.unwrap_or_else(|e| {
                            ToolOutput::err(format!("tool task failed: {}", e))
                        }));
                }
            }
        }
        let mut skip = false;
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Slot::Done(id, content, is_error) => {
                    self.journal_result(&id, &content, is_error, true);
                    self.emit_tool_result(&id, &content, is_error, false);
                    self.messages.push(ChatMessage::tool(id, content));
                }
                Slot::Stop(id, content, _) => {
                    self.journal_result(&id, &content, true, true);
                    self.emit_tool_result(&id, &content, true, false);
                    self.messages.push(ChatMessage::tool(id, content));
                }
                Slot::Run(mut p) => {
                    if skip {
                        let content = "[tool call skipped: run stopping]".to_string();
                        self.journal_result(&p.tool_use_id, &content, false, true);
                        self.emit_tool_result(&p.tool_use_id, &content, false, false);
                        self.messages
                            .push(ChatMessage::tool(p.tool_use_id, content));
                        continue;
                    }

                    p.parallel = ran_parallel[i];
                    let out = match outs.get_mut(i).and_then(Option::take) {
                        Some(o) => o,
                        None => {
                            let ctx = self.call_ctx(&p.tool_use_id, &p.tool_name);
                            let timeout = self.config.tool_timeout_with_source(&p.tool_name);
                            let out = run_tool_with_heartbeat(
                                p.tool.clone(),
                                p.args.clone(),
                                ctx,
                                timeout,
                                self.sink.clone(),
                                p.tool_use_id.clone(),
                                p.tool_name.clone(),
                                TOOL_PROGRESS_INTERVAL,
                            )
                            .await;
                            self.journal_result(&p.tool_use_id, &out.content, out.is_error, false);
                            out
                        }
                    };
                    if let Some(reason) = self
                        .finish_call(p, out, finish_gate, turn_guard, loop_guard, final_text)
                        .await
                    {
                        *stop_run = Some(reason);
                        skip = true;
                    }
                }
            }
        }
    }

    async fn finish_call(
        &mut self,
        p: PendingCall,
        mut out: ToolOutput,
        finish_gate: &mut finish::FinishGate,
        turn_guard: &mut guard::TurnGuard,
        loop_guard: &mut loop_guard::LoopGuard,
        final_text: &str,
    ) -> Option<String> {
        let PendingCall {
            tool_use_id,
            tool_name,
            args,
            repair_notes,
            parallel,
            ..
        } = p;

        let raw = (self.journal.is_some()).then(|| out.content.clone());

        let post_event = if out.is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        let mut post_payload = json!({
            "tool_name": tool_name,
            "tool_input": args,
            "tool_use_id": tool_use_id,
        });
        if out.is_error {
            post_payload["error"] = json!(out.content);
        } else {
            post_payload["tool_response"] = json!(out.content);
        }
        let post = self.hooks.run(post_event, &tool_name, post_payload).await;
        self.emit_system_message(post_event, &post);
        if let Some(replaced) = post.updated_output.clone() {
            out.content = replaced;
        }
        if let Some(fb) = post.feedback.clone()
            && !fb.is_empty()
        {
            out.content.push('\n');
            out.content.push_str(&fb);
        }
        let verify = self.verify_after_edit(&tool_name, out.is_error).await;
        if let Some(Some(note)) = &verify {
            out.content.push_str("\n\n");
            out.content.push_str(note);
        }
        if let Some(note) = self
            .lsp_diagnostics_note(&tool_name, &args, out.is_error)
            .await
        {
            out.content.push_str("\n\n");
            out.content.push_str(&note);
        }
        finish_gate.observe(
            &tool_name,
            args.get("file_path").and_then(|v| v.as_str()),
            out.is_error,
        );
        if verify.is_some() {
            finish_gate.observe("Bash", None, false);
        }
        if let Some(note) = turn_guard.observe(&tool_name, &args, &out) {
            out.content.push_str("\n\n");
            out.content.push_str(&note);
        }
        loop_guard.observe_call(&tool_name, &args, &out);
        if tool_name == "TodoWrite" && !out.is_error {
            self.model_calls_since_todo = 0;
        }
        if !repair_notes.is_empty() {
            out.content.push_str(&format!(
                "\n[zen: your tool call was repaired before running: {}. Send well-formed calls next time.]",
                repair_notes.join("; ")
            ));
        }
        if raw.as_deref() != Some(out.content.as_str()) {
            self.journal_result(&tool_use_id, &out.content, out.is_error, true);
        }

        if self.config.prune.enabled {
            let max = self.config.prune.max_result_tokens;
            if crate::prune::est_tokens(&out.content) > max {
                let label = crate::prune::tool_call_label(
                    &tool_name,
                    &serde_json::to_string(&args).unwrap_or_default(),
                );
                let (capped, saved) = crate::prune::cap_content(&out.content, &label, max);
                out.content = capped;
                self.prune_stats.capped += 1;
                self.prune_stats.capped_tokens += saved;
            }
        }
        self.emit_tool_result(&tool_use_id, &out.content, out.is_error, parallel);
        self.messages
            .push(ChatMessage::tool(tool_use_id, out.content));
        if post.stop_run {
            Some(post.stop_reason.unwrap_or_else(|| final_text.to_string()))
        } else {
            None
        }
    }

    async fn verify_after_edit(&self, tool_name: &str, is_error: bool) -> Option<Option<String>> {
        let cmd = self.config.verify_after_edit.as_deref()?;
        if is_error || !matches!(tool_name, "Edit" | "MultiEdit" | "Write") {
            return None;
        }
        let timeout =
            std::time::Duration::from_secs(self.config.verify_after_edit_timeout_secs.max(1));
        let (passed, status, tail) =
            finish::run_command(cmd, &self.ctx.cwd(), &self.ctx.env, timeout).await;
        if passed {
            return Some(None);
        }
        Some(Some(format!(
            "[zen: verify `{}` failed ({}) after this change. Last lines of output:\n{}]",
            cmd, status, tail
        )))
    }

    /// Post-edit LSP diagnostics note (P0 #4): after a successful Edit,
    /// MultiEdit or Write, ask the diagnostics provider for NEW
    /// diagnostics (Error and Warning only, Q5) for the edited file and
    /// append them so the model can
    /// fix its own type errors. Returns None when the provider is off,
    /// the tool is not a file edit, the call failed, the file has no
    /// new diagnostics, or the per-file feedback cap for this run is
    /// reached. Diagnostics never fail a run: the provider returns an
    /// empty vec on any failure.
    async fn lsp_diagnostics_note(
        &mut self,
        tool_name: &str,
        args: &Value,
        is_error: bool,
    ) -> Option<String> {
        let provider = self.config.diagnostics_provider.clone()?;
        if is_error || !matches!(tool_name, "Edit" | "MultiEdit" | "Write") {
            return None;
        }
        let file = args.get("file_path")?.as_str()?;
        let path = self.ctx.cwd().join(file);
        let diags = provider.diagnostics_after_edit(&path).await;
        if diags.is_empty() {
            return None;
        }
        let key = path.to_string_lossy().into_owned();
        let count = self.lsp_feedback_counts.entry(key.clone()).or_insert(0);
        if *count >= MAX_LSP_FEEDBACK_PER_FILE {
            if *count == MAX_LSP_FEEDBACK_PER_FILE {
                *count += 1;
                return Some(format!(
                    "[zen: LSP feedback for {key} paused after {MAX_LSP_FEEDBACK_PER_FILE} rounds in this run.]"
                ));
            }
            return None;
        }
        *count += 1;
        Some(format!(
            "[zen: LSP diagnostics for {key} ({} new):\n{}]",
            diags.len(),
            zen_core::render_diagnostics(&diags)
        ))
    }

    fn maybe_reinject_todos(&mut self) {
        self.model_calls_since_todo += 1;
        if self.model_calls_since_todo < guard::TODO_REINJECT_EVERY {
            return;
        }
        let todos = lock_or_recover(&self.ctx.todos, "ctx.todos").clone();
        let Some(note) = guard::todo_reminder(&todos) else {
            return;
        };
        if let Some(last) = self.messages.last_mut()
            && (last.role == "tool" || last.role == "user")
        {
            let content = last
                .content
                .get_or_insert_with(|| MessageContent::Text(String::new()));
            match content {
                MessageContent::Text(text) => {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
                MessageContent::Parts(parts) => {
                    parts.push(ContentPart::text(format!("\n\n{note}")));
                }
            }
            self.model_calls_since_todo = 0;
        }
    }

    fn rewrite_tool_call(&mut self, id: &str, name: &str, args: &Value) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == "assistant")
        else {
            return;
        };
        if let Some(call) = msg.tool_calls.iter_mut().flatten().find(|c| c.id == id) {
            call.function.name = name.to_string();
            call.function.arguments = args.to_string();
        }
    }

    fn prune_old_results(&mut self) {
        if !self.config.prune.enabled {
            return;
        }
        let current_turn = self.prune_turn_starts.len().saturating_sub(1);
        let min_age = self.config.prune.min_age();

        let mut protected = self.plan_text().unwrap_or_default();
        protected.push('\n');
        for (i, m) in self.messages.iter().enumerate() {
            let age =
                current_turn.saturating_sub(crate::prune::turn_of(&self.prune_turn_starts, i));
            if age >= min_age {
                continue;
            }
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    if let Some(id) =
                        crate::prune::extract_identifier(&c.function.name, &c.function.arguments)
                    {
                        protected.push_str(&id);
                        protected.push('\n');
                    }
                }
            }
        }
        let stats = crate::prune::prune_messages(
            &mut self.messages,
            &self.prune_turn_starts,
            current_turn,
            &self.config.prune,
            &protected,
        );
        if stats.pruned > 0 {
            self.prune_stats.pruned += stats.pruned;
            self.prune_stats.pruned_tokens += stats.pruned_tokens;
            self.sink.emit(AgentEvent::Pruned {
                pruned: stats.pruned,
                tokens_saved: stats.pruned_tokens,
            });
        }
    }

    pub fn prune_stats(&self) -> &PruneStats {
        &self.prune_stats
    }

    pub async fn compact(&mut self, focus: Option<&str>, trigger: &str) -> anyhow::Result<()> {
        compaction::compact_impl(self, focus, trigger).await
    }
}

pub(crate) fn is_parallel_safe(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "Glob" | "Grep" | "LS")
}

struct PendingCall {
    tool_use_id: String,
    tool_name: String,
    args: Value,
    repair_notes: Vec<String>,
    tool: Arc<dyn Tool>,
    parallel: bool,
}

enum Slot {
    Done(String, String, bool),

    Stop(String, String, String),

    Run(PendingCall),
}

pub(crate) fn combine_permission(
    checked: PermissionDecision,
    hook: Option<PermissionDecision>,
    ask_rule: bool,
    mode: PermissionMode,
) -> PermissionDecision {
    match (checked, hook) {
        (PermissionDecision::Deny(r), _) => PermissionDecision::Deny(r),
        (_, Some(PermissionDecision::Deny(r))) => PermissionDecision::Deny(r),
        (_, Some(PermissionDecision::Ask)) => PermissionDecision::Ask,

        // A hook Allow lifts a mode Ask to Allow only when no ask rule
        // matched - and never in askAll mode, where a hook may only
        // tighten, never loosen.
        (PermissionDecision::Ask, Some(PermissionDecision::Allow))
            if !ask_rule && mode != PermissionMode::AskAll =>
        {
            PermissionDecision::Allow
        }
        (c, _) => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        FakeHooks, FakePerms, MockLlm, base_cfg, build_agent, registry_with, resp,
    };

    fn agent_with(messages: Vec<ChatMessage>) -> Agent {
        let (mut agent, _sink, _hooks) = build_agent(
            MockLlm::new(vec![]),
            registry_with(vec![]),
            FakePerms::new(PermissionDecision::Allow),
            base_cfg(),
        );
        agent.set_messages(messages);
        agent
    }

    #[test]
    fn plan_text_keeps_chronological_order() {
        // Pins parts.reverse() and the "\n\n" join: dropping the
        // reverse (or the separator) changes the result.
        let agent = agent_with(vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant(Some("first".into()), None),
            ChatMessage::assistant(Some("second".into()), None),
        ]);
        assert_eq!(agent.plan_text().as_deref(), Some("first\n\nsecond"));
    }

    #[test]
    fn plan_text_glues_across_truncation_nudge() {
        // The text after a truncation nudge continues the text before
        // it, with no separator. Pins `glue && !parts.is_empty()`: if
        // the `!` were dropped, the parts would stay separate.
        let agent = agent_with(vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant(Some("part one".into()), None),
            ChatMessage::user(TRUNCATION_CONTINUE_NUDGE),
            ChatMessage::assistant(Some("part two".into()), None),
        ]);
        assert_eq!(agent.plan_text().as_deref(), Some("part onepart two"));
    }

    #[test]
    fn plan_text_glue_with_empty_parts_keeps_text() {
        // Pins `glue && !parts.is_empty()` the other way: swapping
        // `&&` for `||` (or dropping the `!`) would swallow the only
        // part when glue is set but nothing was collected yet.
        let agent = agent_with(vec![
            ChatMessage::assistant(Some("only".into()), None),
            ChatMessage::user(TRUNCATION_CONTINUE_NUDGE),
        ]);
        assert_eq!(agent.plan_text().as_deref(), Some("only"));
    }

    #[test]
    fn plan_text_does_not_carry_glue_past_one_message() {
        // After one assistant message the glue is spent: an earlier
        // message starts a new part. Pins `glue = false`.
        let agent = agent_with(vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant(Some("a".into()), None),
            ChatMessage::assistant(Some("b".into()), None),
            ChatMessage::user(TRUNCATION_CONTINUE_NUDGE),
        ]);
        assert_eq!(agent.plan_text().as_deref(), Some("a\n\nb"));
    }

    #[test]
    fn plan_text_skips_whitespace_only_assistant_text() {
        // Pins `!c.trim().is_empty()`: negating it would keep the
        // blank text and drop the real one.
        let agent = agent_with(vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant(Some("   ".into()), None),
            ChatMessage::assistant(Some("real".into()), None),
        ]);
        assert_eq!(agent.plan_text().as_deref(), Some("real"));
        let blank_only = agent_with(vec![
            ChatMessage::user("do it"),
            ChatMessage::assistant(Some("   ".into()), None),
        ]);
        assert_eq!(blank_only.plan_text(), None);
    }

    #[test]
    fn plan_text_returns_none_without_assistant_text() {
        let agent = agent_with(vec![ChatMessage::user("do it")]);
        assert_eq!(agent.plan_text(), None);
    }

    #[test]
    fn classify_api_error_matches_each_single_signal() {
        // Pins every `||` in classify_api_error (agent.rs:501-515):
        // each case matches exactly ONE disjunct of its branch, so
        // swapping any `||` for `&&` drops the branch and the test
        // fails. A case matching two disjuncts at once would NOT kill
        // the mutant, so keep each input to a single signal.
        let cases: &[(&str, &str)] = &[
            ("rate limit exceeded", "rate_limit"),
            ("error 429", "rate_limit"),
            ("too many requests", "rate_limit"),
            ("RATE LIMIT", "rate_limit"),
            ("model overloaded", "overloaded"),
            ("error 529", "overloaded"),
            ("error 503", "overloaded"),
            ("auth error", "authentication_failed"),
            ("error 401", "authentication_failed"),
            ("error 403", "authentication_failed"),
            ("billing issue", "billing_error"),
            ("payment required", "billing_error"),
            ("error 402", "billing_error"),
            ("model not found", "model_not_found"),
            ("error 404", "model_not_found"),
            ("max_output_tokens reached", "max_output_tokens"),
            ("max tokens exceeded", "max_output_tokens"),
            ("invalid request", "invalid_request"),
            ("error 400", "invalid_request"),
            ("server error", "server_error"),
            ("error 500", "server_error"),
            ("something went wrong", "unknown"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                Agent::classify_api_error(input),
                *expected,
                "classify_api_error({input:?})"
            );
        }
    }

    fn agent_with_hooks() -> (Agent, Arc<FakeHooks>) {
        let (agent, _sink, hooks) = build_agent(
            MockLlm::new(vec![]),
            registry_with(vec![]),
            FakePerms::new(PermissionDecision::Allow),
            base_cfg(),
        );
        (agent, hooks)
    }

    fn post_tool_batch_response(hooks: &FakeHooks) -> String {
        let calls = hooks.calls.lock().unwrap();
        let payload = calls
            .iter()
            .find(|(ev, _, _)| *ev == HookEvent::PostToolBatch)
            .map(|(_, _, payload)| payload.clone())
            .expect("PostToolBatch hook should run");
        payload["tool_calls"][0]["tool_response"]
            .as_str()
            .expect("entry should carry a tool_response string")
            .to_string()
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "Read".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn post_tool_batch_matches_tool_response_by_call_id() {
        // Pins the `&&` in run_post_tool_batch's `.find(|m| m.role ==
        // "tool" && m.tool_call_id == Some(tc.id))`: each scenario
        // satisfies exactly ONE conjunct in the wrong place, so
        // swapping `&&` for `||` returns the wrong response text.
        // (a) An earlier tool message with a different id: it
        // satisfies the role conjunct only and must not match.
        let (mut agent, hooks) = agent_with_hooks();
        agent.set_messages(vec![
            ChatMessage::tool("call-first", "WRONG-first-response"),
            ChatMessage::tool("call-second", "RIGHT-second-response"),
        ]);
        let stop = agent
            .run_post_tool_batch(&[tool_call("call-second")], 0)
            .await;
        assert_eq!(stop, None);
        assert_eq!(post_tool_batch_response(&hooks), "RIGHT-second-response");

        // (b) A non-tool message carrying the wanted id: it satisfies
        // the id conjunct only and must not match.
        let (mut agent, hooks) = agent_with_hooks();
        agent.set_messages(vec![
            ChatMessage {
                role: "assistant".into(),
                content: Some(MessageContent::Text("WRONG-assistant-text".into())),
                tool_call_id: Some("call-second".into()),
                ..Default::default()
            },
            ChatMessage::tool("call-second", "RIGHT-second-response"),
        ]);
        let stop = agent
            .run_post_tool_batch(&[tool_call("call-second")], 0)
            .await;
        assert_eq!(stop, None);
        assert_eq!(post_tool_batch_response(&hooks), "RIGHT-second-response");
    }

    #[tokio::test]
    async fn check_task_hooks_fires_task_completed_only_on_transition_to_completed() {
        // Pins the match guard `p.status != "completed" &&
        // status == "completed"` in check_task_hooks: each row
        // exercises exactly ONE side of the guard, so replacing the
        // guard with `true` or `&&` with `||` fires TaskCompleted
        // where the original fires nothing (and vice versa the
        // positive rows prove the event fires at all).
        let cases: &[(Option<&str>, &str, Option<HookEvent>)] = &[
            // Already completed, still completed: no transition.
            (Some("completed"), "completed", None),
            // Not completed before, still not completed: no transition.
            (Some("in_progress"), "pending", None),
            // Not completed before, now completed: the transition.
            (
                Some("in_progress"),
                "completed",
                Some(HookEvent::TaskCompleted),
            ),
            // Brand-new item: creation, not completion.
            (None, "pending", Some(HookEvent::TaskCreated)),
        ];
        for (prev_status, new_status, expected) in cases {
            let (agent, hooks) = agent_with_hooks();
            if let Some(prev) = prev_status {
                agent.ctx.todos.lock().unwrap().push(TodoItem {
                    content: "Write tests".into(),
                    status: (*prev).into(),
                    active_form: None,
                });
            }
            let veto = agent
                .check_task_hooks(
                    &json!({ "todos": [{ "content": "Write tests", "status": new_status }] }),
                )
                .await;
            assert_eq!(veto, None, "no hook blocks in case {cases:?}");
            let fired: Vec<HookEvent> = hooks
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|(ev, _, _)| *ev)
                .collect();
            match expected {
                Some(event) => assert!(
                    fired.contains(event),
                    "expected {event:?} to fire for prev={prev_status:?} new={new_status}, got {fired:?}"
                ),
                None => assert!(
                    !fired.contains(&HookEvent::TaskCompleted)
                        && !fired.contains(&HookEvent::TaskCreated),
                    "expected no task hook to fire for prev={prev_status:?} new={new_status}, got {fired:?}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn run_turn_appends_only_non_empty_pending_hook_context() {
        // Pins the `!` in run_turn's `if !pending.trim().is_empty()`:
        // a non-empty pending note must reach the user message, while
        // a blank one must not. Dropping the `!` flips both, so each
        // row on its own kills the mutant.
        let cases: &[(&[&str], &str)] = &[
            (&["hook context note"], "do the thing\n\nhook context note"),
            (
                &["first note", "second note"],
                "do the thing\n\nfirst note\n\nsecond note",
            ),
            (&["   "], "do the thing"),
            (&[""], "do the thing"),
        ];
        for (pending, expected) in cases {
            let llm = MockLlm::new(vec![resp("done")]);
            let (mut agent, _sink, _hooks) = build_agent(
                llm.clone(),
                registry_with(vec![]),
                FakePerms::new(PermissionDecision::Allow),
                base_cfg(),
            );
            agent.pending_hook_context = pending.iter().map(|s| (*s).to_string()).collect();
            let result = agent.run_turn("do the thing").await;
            assert_eq!(result.subtype, "success");
            assert_eq!(agent.messages()[0].text(), *expected, "pending={pending:?}");
            let requests = llm.requests();
            let sent = requests[0]
                .messages
                .iter()
                .find(|m| m.role == "user")
                .expect("a user message should be sent to the model");
            assert_eq!(sent.text(), *expected, "pending={pending:?}");
        }
    }

    #[test]
    fn default_model_is_the_decided_free_gateway_id() {
        // PR #71 review F8: the old default named a paid model in
        // its OpenRouter form; the decided default is the free
        // deepseek-v4.1-flash, already in gateway form.
        assert_eq!(AgentConfig::default().model, "deepseek-v4.1-flash");
    }
}
