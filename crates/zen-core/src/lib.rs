pub mod lock_recovery;
pub mod secret;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,

    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function")]
    pub call_type: String,
    pub function: FunctionCall,
}
fn default_function() -> String {
    "function".into()
}

/// Fixed token estimate for one image content part. The provider bills at
/// most 384 input tokens per image; the estimate never scales with the
/// base64 payload length, so a large image cannot silently eat the budget.
pub const IMAGE_PART_TOKEN_ESTIMATE: u64 = 384;

/// Placeholder rendered for an image part in transcripts and compaction
/// input. The base64 payload is never written there.
pub const IMAGE_PART_PLACEHOLDER: &str = "[image]";

/// The payload of one `image_url` content part (OpenAI-compatible shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ImageUrl {
    /// Build an image URL part payload. `detail` defaults to `"auto"`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            detail: Some("auto".into()),
        }
    }
}

/// The payload of one Anthropic `image` content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

impl ImageSource {
    /// Build a base64 image source for the Anthropic `image` block shape.
    pub fn base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            source_type: "base64".into(),
            media_type: media_type.into(),
            data: data.into(),
        }
    }
}

/// One entry of a multi-part message `content` array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    Image { source: ImageSource },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl::new(url),
        }
    }

    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::base64(media_type, data),
        }
    }

    pub fn is_image(&self) -> bool {
        !matches!(self, Self::Text { .. })
    }
}

/// A message `content` value: either a plain string (the historical shape,
/// serialised verbatim) or an array of content parts (text + images).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

impl From<Vec<ContentPart>> for MessageContent {
    fn from(parts: Vec<ContentPart>) -> Self {
        Self::Parts(parts)
    }
}

impl MessageContent {
    /// Plain-text view: the string itself, or the text parts of a parts
    /// message joined together. Image parts contribute nothing here.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Transcript-safe view: like [`MessageContent::text`], but each image
    /// part renders as the `[image]` placeholder. The base64 payload never
    /// appears in the returned string.
    pub fn transcript_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    match p {
                        ContentPart::Text { text } => out.push_str(text),
                        _ => out.push_str(IMAGE_PART_PLACEHOLDER),
                    }
                }
                out
            }
        }
    }

    /// Number of bytes of plain text (text parts only, never base64).
    pub fn text_len(&self) -> usize {
        match self {
            Self::Text(s) => s.len(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.len()),
                    _ => None,
                })
                .sum(),
        }
    }

    pub fn image_part_count(&self) -> usize {
        match self {
            Self::Text(_) => 0,
            Self::Parts(parts) => parts.iter().filter(|p| p.is_image()).count(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(c: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(MessageContent::Text(c.into())),
            ..Default::default()
        }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Text(c.into())),
            ..Default::default()
        }
    }
    /// A user message carrying text and/or image parts.
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Parts(parts)),
            ..Default::default()
        }
    }
    pub fn assistant(c: Option<String>, calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content: c.map(MessageContent::Text),
            tool_calls: calls,
            ..Default::default()
        }
    }
    /// An assistant message whose content is already a [`MessageContent`]
    /// (text or parts, e.g. decoded from a provider response).
    pub fn assistant_content(c: Option<MessageContent>, calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content: c,
            tool_calls: calls,
            ..Default::default()
        }
    }
    pub fn tool(id: impl Into<String>, c: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(MessageContent::Text(c.into())),
            tool_call_id: Some(id.into()),
            ..Default::default()
        }
    }

    /// Plain-text view of the content (`""` when absent). See
    /// [`MessageContent::text`].
    pub fn text(&self) -> String {
        self.content.as_ref().map(|c| c.text()).unwrap_or_default()
    }

    /// Transcript-safe view of the content. See
    /// [`MessageContent::transcript_text`].
    pub fn transcript_text(&self) -> String {
        self.content
            .as_ref()
            .map(|c| c.transcript_text())
            .unwrap_or_default()
    }

    /// The content as a plain `&str` when it is a text string, `None` for
    /// absent content and for parts content.
    pub fn text_ref(&self) -> Option<&str> {
        match &self.content {
            Some(MessageContent::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn image_part_count(&self) -> usize {
        self.content
            .as_ref()
            .map(|c| c.image_part_count())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,

    #[serde(default)]
    pub cache_read_input_tokens: u64,

    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

pub fn cache_hit_rate(cache_read_input_tokens: u64, prompt_tokens: u64) -> f64 {
    if prompt_tokens == 0 {
        return 0.0;
    }
    (cache_read_input_tokens as f64 / prompt_tokens as f64).min(1.0)
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub message: ChatMessage,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> anyhow::Result<ChatResponse>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    DontAsk,
    AskAll,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
            Self::AskAll => "askAll",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "default" => Self::Default,
            "acceptEdits" | "accept-edits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "bypassPermissions" | "bypass-permissions" => Self::BypassPermissions,
            "dontAsk" | "dont-ask" => Self::DontAsk,
            "askAll" | "ask-all" => Self::AskAll,
            _ => return None,
        })
    }

    /// Whether an ACP client may see and select this mode (M6, Phase 2
    /// #9): the Q3-advertised subset of #8's 6-mode model.
    /// `bypassPermissions` and `dontAsk` are never advertised and never
    /// run behind a client that cannot see them. The ACP layer asks this
    /// API instead of hardcoding the set.
    pub fn is_acp_advertised(&self) -> bool {
        matches!(
            self,
            Self::Default | Self::Plan | Self::AcceptEdits | Self::AskAll
        )
    }

    /// The ACP-advertised modes in wire order (M6): the order the
    /// `session/new` response lists them in.
    pub fn acp_advertised_modes() -> [PermissionMode; 4] {
        [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::AskAll,
        ]
    }
}

/// OS-level sandbox mode for child processes spawned by the Bash tool and
/// background jobs (Phase 2 #12). This is a second layer, orthogonal to the
/// permission layer: permissions decide whether a tool runs at all, the
/// sandbox decides what the child process may do at the kernel level.
/// Enforcement exists only on Linux (Landlock + seccomp); other platforms
/// warn and run without a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    Off,
    WorkspaceWrite,
    ReadOnly,
}

impl SandboxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "workspace-write" | "workspace_write" => Self::WorkspaceWrite,
            "read-only" | "read_only" => Self::ReadOnly,
            _ => return None,
        })
    }

    /// Platform default (Zen decision 1, Phase 2 #12): Linux defaults to
    /// `workspace-write` when Landlock is available; every other platform
    /// defaults to `off` because there is no enforcement there.
    pub fn default_for_platform() -> Self {
        if cfg!(target_os = "linux") {
            Self::WorkspaceWrite
        } else {
            Self::Off
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}
impl ToolOutput {
    pub fn ok(c: impl Into<String>) -> Self {
        Self {
            content: c.into(),
            is_error: false,
        }
    }
    pub fn err(c: impl Into<String>) -> Self {
        Self {
            content: c.into(),
            is_error: true,
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: Arc<Mutex<PathBuf>>,

    pub extra_dirs: Vec<PathBuf>,

    /// OS-level sandbox mode applied to child processes spawned by the
    /// Bash tool and background jobs (Phase 2 #12). Defaults to `Off` for
    /// contexts built directly; the real agent path sets the resolved mode
    /// from CLI flag / settings / platform default.
    pub sandbox_mode: SandboxMode,

    pub env: HashMap<String, String>,

    pub api_key_env: Option<String>,

    pub allow_private_network: bool,

    pub read_files: Arc<Mutex<HashMap<PathBuf, u64>>>,

    pub todos: Arc<Mutex<Vec<TodoItem>>>,
    pub session_id: String,

    pub current_tool_use_id: Option<String>,

    pub session_dir: Option<PathBuf>,

    pub tool_timeout_ms: Option<u64>,

    pub checkpoints: Option<Arc<dyn FileCheckpoints>>,
}

pub trait FileCheckpoints: Send + Sync {
    fn begin_turn(&self, prompt: &str);

    fn before_write(&self, path: &Path) -> Option<String>;

    fn after_write(&self, path: &Path, content: &[u8]);
}

/// One diagnostic published by a language server
/// (`textDocument/publishDiagnostics`). Positions are 1-based
/// line/character columns, matching the rest of ZenCore.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
    /// LSP severity: 1 = Error, 2 = Warning, 3 = Information, 4 = Hint.
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

impl LspDiagnostic {
    pub fn severity_label(&self) -> &'static str {
        match self.severity {
            1 => "Error",
            2 => "Warning",
            3 => "Information",
            4 => "Hint",
            _ => "Diagnostic",
        }
    }

    /// Whether this diagnostic reaches the feedback loop: only Error (1)
    /// and Warning (2) do. Information (3), Hint (4) and unknown
    /// severities are filtered out at collection (Q5), so the model is
    /// only interrupted by diagnostics it can act on.
    pub fn is_actionable(&self) -> bool {
        matches!(self.severity, 1 | 2)
    }

    /// Reference-agent-style one-line render: `Error [line:col] message`.
    pub fn pretty(&self) -> String {
        format!(
            "{} [{}:{}] {}",
            self.severity_label(),
            self.line,
            self.col,
            self.message
        )
    }

    /// Identity for snapshot diffs: a diagnostic is "the same" when its
    /// range and message match, even if the server re-publishes it with a
    /// different severity, source or code.
    pub fn dedup_key(&self) -> (usize, usize, usize, usize, &str) {
        (
            self.line,
            self.col,
            self.end_line,
            self.end_col,
            self.message.as_str(),
        )
    }
}

/// Maximum diagnostic lines appended to a tool result after an edit.
pub const MAX_DIAGNOSTICS_LINES: usize = 20;

/// Render diagnostics as capped one-per-line text. Lines beyond the cap
/// are replaced by a single "... and N more" line.
pub fn render_diagnostics(diags: &[LspDiagnostic]) -> String {
    let mut lines: Vec<String> = diags
        .iter()
        .take(MAX_DIAGNOSTICS_LINES)
        .map(LspDiagnostic::pretty)
        .collect();
    if diags.len() > MAX_DIAGNOSTICS_LINES {
        lines.push(format!(
            "... and {} more",
            diags.len() - MAX_DIAGNOSTICS_LINES
        ));
    }
    lines.join("\n")
}

/// Provides post-edit LSP diagnostics to the agent loop. Implemented by
/// zen-mcp's `LspManager`; the trait lives in zen-core so zen-agent can
/// depend on it without depending on zen-mcp (same pattern as
/// `FileCheckpoints`).
#[async_trait]
pub trait DiagnosticsProvider: Send + Sync {
    /// Wait for the language server's diagnostics for `path` after an
    /// edit, returning only diagnostics that are NEW compared to the
    /// snapshot taken before the edit was sent. Only Error and Warning
    /// severities are returned; Information and Hint are filtered out
    /// at collection (Q5). Best effort: any failure
    /// (no server, unsupported file type, timeout) yields an empty vec,
    /// never an error, so diagnostics can never fail a run.
    async fn diagnostics_after_edit(&self, path: &Path) -> Vec<LspDiagnostic>;
}

// Debug for the trait object (as tracing-core does for `dyn Value`):
// `AgentConfig` derives Debug, so the provider field needs it, but
// implementations must not be forced to be Debug.
impl std::fmt::Debug for dyn DiagnosticsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagnosticsProvider")
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    pub fn new(cwd: PathBuf, session_id: impl Into<String>) -> Self {
        Self {
            cwd: Arc::new(Mutex::new(cwd)),
            extra_dirs: vec![],
            sandbox_mode: SandboxMode::Off,
            env: HashMap::new(),
            api_key_env: None,
            allow_private_network: false,
            read_files: Default::default(),
            todos: Default::default(),
            session_id: session_id.into(),
            current_tool_use_id: None,
            session_dir: None,
            tool_timeout_ms: None,
            checkpoints: None,
        }
    }
    pub fn cwd(&self) -> PathBuf {
        lock_recovery::lock_or_recover(&self.cwd, "cwd").clone()
    }

    pub fn scratch_dir(&self) -> PathBuf {
        match &self.session_dir {
            Some(d) => d.clone(),
            None => {
                let id: String = self
                    .session_id
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                std::env::temp_dir().join(format!(
                    "zen-{}",
                    if id.is_empty() { "session".into() } else { id }
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn parameters_schema(&self) -> Value;

    fn is_read_only(&self) -> bool;
    async fn call(&self, args: Value, ctx: &ToolContext) -> ToolOutput;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: self.parameters_schema(),
            },
        }
    }

    /// The sub-agent runner this tool spawns children through, if it is
    /// a sub-agent tool (`Task`/`delegate`). `None` for every other tool.
    /// Lets callers verify what spawned children inherit.
    fn subagent_runner(&self) -> Option<&dyn SubAgentRunner> {
        None
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, t: Arc<dyn Tool>) {
        self.tools.retain(|x| x.name() != t.name());
        self.tools.push(t);
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub fn filtered(&self, allowed: &[String]) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|t| allowed.iter().any(|a| a == t.name()))
                .cloned()
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow,
    Deny(String),

    Ask,
}

pub trait PermissionChecker: Send + Sync {
    fn check(
        &self,
        tool_name: &str,
        input: &Value,
        read_only: bool,
        ctx: &ToolContext,
    ) -> PermissionDecision;

    fn add_allow_rule(&self, _rule: &str) {}

    fn ask_rule_matches(&self, _tool_name: &str, _input: &Value, _ctx: &ToolContext) -> bool {
        false
    }

    /// Switch the mode permission decisions are evaluated under (M4,
    /// Phase 2 #9): `session/set_mode` calls this so the checker follows
    /// the session mode instead of the mode it was built with. Default
    /// no-op: a checker that bakes no mode in has nothing to update.
    fn set_mode(&self, mode: PermissionMode) {
        let _ = mode;
    }
}

pub trait Prompter: Send + Sync {
    fn ask_permission(&self, tool_name: &str, input: &Value) -> PromptAnswer;
}

/// Async counterpart of [`Prompter`] for hosts that answer permission
/// questions over an async channel instead of a terminal (M4, Phase 2
/// #9): the ACP server round-trips `session/request_permission` to the
/// client and awaits the outcome. `tool_call_id` is the id the client
/// already saw in the `tool_call` session update for this call, so the
/// client can match the request to it; `read_only` is the tool's own
/// read-only flag, used to pick the ACP tool kind.
#[async_trait]
pub trait AsyncPrompter: Send + Sync {
    async fn ask_permission(
        &self,
        tool_name: &str,
        input: &Value,
        tool_call_id: &str,
        read_only: bool,
    ) -> PromptAnswer;
}
#[derive(Debug, Clone, PartialEq)]
pub enum PromptAnswer {
    Yes,
    Always(String),
    No,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionDenial {
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SubagentStop,
    SessionStart,
    SessionEnd,
    PreCompact,
    Notification,

    PostToolUseFailure,

    SubagentStart,

    PostCompact,

    PermissionRequest,

    PermissionDenied,

    // Events added by the Wave 1 #3 hook system, mirroring the reference
    // agent's hooks reference (33 events). See docs/hooks.md for the
    // Zen trigger of each one.
    Setup,
    UserPromptExpansion,
    MessageDisplay,
    PostToolBatch,
    TaskCreated,
    TaskCompleted,
    StopFailure,
    TeammateIdle,
    InstructionsLoaded,
    ConfigChange,
    CwdChanged,
    DirectoryAdded,
    FileChanged,
    WorktreeCreate,
    WorktreeRemove,
    PreModelSwitch,
    PostModelSwitch,
    Elicitation,
    ElicitationResult,
}
impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreCompact => "PreCompact",
            Self::Notification => "Notification",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::SubagentStart => "SubagentStart",
            Self::PostCompact => "PostCompact",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::Setup => "Setup",
            Self::UserPromptExpansion => "UserPromptExpansion",
            Self::MessageDisplay => "MessageDisplay",
            Self::PostToolBatch => "PostToolBatch",
            Self::TaskCreated => "TaskCreated",
            Self::TaskCompleted => "TaskCompleted",
            Self::StopFailure => "StopFailure",
            Self::TeammateIdle => "TeammateIdle",
            Self::InstructionsLoaded => "InstructionsLoaded",
            Self::ConfigChange => "ConfigChange",
            Self::CwdChanged => "CwdChanged",
            Self::DirectoryAdded => "DirectoryAdded",
            Self::FileChanged => "FileChanged",
            Self::WorktreeCreate => "WorktreeCreate",
            Self::WorktreeRemove => "WorktreeRemove",
            Self::PreModelSwitch => "PreModelSwitch",
            Self::PostModelSwitch => "PostModelSwitch",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
        }
    }

    /// Parse an event name from settings/docs. Unknown names return `None`;
    /// the settings loader keeps unknown hook keys inert rather than failing.
    pub fn parse_name(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            "SessionStart" => Self::SessionStart,
            "SessionEnd" => Self::SessionEnd,
            "PreCompact" => Self::PreCompact,
            "Notification" => Self::Notification,
            "PostToolUseFailure" => Self::PostToolUseFailure,
            "SubagentStart" => Self::SubagentStart,
            "PostCompact" => Self::PostCompact,
            "PermissionRequest" => Self::PermissionRequest,
            "PermissionDenied" => Self::PermissionDenied,
            "Setup" => Self::Setup,
            "UserPromptExpansion" => Self::UserPromptExpansion,
            "MessageDisplay" => Self::MessageDisplay,
            "PostToolBatch" => Self::PostToolBatch,
            "TaskCreated" => Self::TaskCreated,
            "TaskCompleted" => Self::TaskCompleted,
            "StopFailure" => Self::StopFailure,
            "TeammateIdle" => Self::TeammateIdle,
            "InstructionsLoaded" => Self::InstructionsLoaded,
            "ConfigChange" => Self::ConfigChange,
            "CwdChanged" => Self::CwdChanged,
            "DirectoryAdded" => Self::DirectoryAdded,
            "FileChanged" => Self::FileChanged,
            "WorktreeCreate" => Self::WorktreeCreate,
            "WorktreeRemove" => Self::WorktreeRemove,
            "PreModelSwitch" => Self::PreModelSwitch,
            "PostModelSwitch" => Self::PostModelSwitch,
            "Elicitation" => Self::Elicitation,
            "ElicitationResult" => Self::ElicitationResult,
            _ => return None,
        })
    }

    /// All 33 hook events, in the order the reference lists them.
    pub fn all() -> [Self; 33] {
        [
            Self::SessionStart,
            Self::Setup,
            Self::UserPromptSubmit,
            Self::UserPromptExpansion,
            Self::PreToolUse,
            Self::PermissionRequest,
            Self::PermissionDenied,
            Self::PostToolUse,
            Self::PostToolUseFailure,
            Self::PostToolBatch,
            Self::Notification,
            Self::MessageDisplay,
            Self::SubagentStart,
            Self::SubagentStop,
            Self::TaskCreated,
            Self::TaskCompleted,
            Self::Stop,
            Self::StopFailure,
            Self::TeammateIdle,
            Self::InstructionsLoaded,
            Self::ConfigChange,
            Self::CwdChanged,
            Self::DirectoryAdded,
            Self::FileChanged,
            Self::WorktreeCreate,
            Self::WorktreeRemove,
            Self::PreCompact,
            Self::PostCompact,
            Self::PreModelSwitch,
            Self::PostModelSwitch,
            Self::Elicitation,
            Self::ElicitationResult,
            Self::SessionEnd,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookOutcome {
    pub blocked: bool,

    pub feedback: Option<String>,

    pub permission_override: Option<PermissionDecision>,

    pub stop_run: bool,

    pub stop_reason: Option<String>,

    pub updated_input: Option<Value>,

    pub updated_output: Option<String>,

    pub system_message: Option<String>,

    /// `PermissionDenied`: the hook said the model may retry the denied
    /// call (`hookSpecificOutput.retry: true`).
    pub allow_retry: bool,

    /// `MessageDisplay`: replacement text for what is shown on screen.
    /// The model context and transcript keep the original text.
    pub display_replacement: Option<String>,

    /// `WorktreeCreate`: absolute path of the worktree the hook created
    /// (last non-empty stdout line, or `hookSpecificOutput.worktreePath`).
    pub worktree_path: Option<String>,

    /// `CwdChanged` / `FileChanged`: dynamic watch list
    /// (`watchPaths` from the hook's JSON output). Replaces the current
    /// dynamic list; static matcher paths are always watched.
    pub watch_paths: Option<Vec<String>>,

    /// True when any hook failed to run (spawn failure, timeout, or a
    /// non-zero exit that is not a block). Non-blocking for most events;
    /// `WorktreeRemove` / `WorktreeCreate` use it to decide whether the
    /// worktree operation failed.
    pub failed: bool,
}

#[async_trait]
pub trait HookRunner: Send + Sync {
    async fn run(&self, event: HookEvent, matcher_target: &str, payload: Value) -> HookOutcome;
}

pub struct NoHooks;
#[async_trait]
impl HookRunner for NoHooks {
    async fn run(&self, _: HookEvent, _: &str, _: Value) -> HookOutcome {
        HookOutcome::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AgentEvent {
    Assistant {
        message: ChatMessage,
        parent_tool_use_id: Option<String>,
    },

    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        parent_tool_use_id: Option<String>,
        parallel: bool,
    },

    TextDelta {
        text: String,
    },

    ToolProgress {
        tool_use_id: String,
        tool_name: String,
        elapsed_ms: u64,
    },
    ApiRetry {
        attempt: u32,
        max_retries: u32,
        retry_delay_ms: u64,
        error_status: Option<u16>,
        error: String,
    },
    CompactBoundary {
        trigger: String,
        pre_tokens: u64,
        /// The compaction summary the agent wrote into history
        /// (AgentEvent module, Phase 2 #9): the model summary plus
        /// file references, without the "[Compaction summary ...]" prefix.
        summary: String,
    },

    Pruned {
        pruned: usize,
        tokens_saved: u64,
    },
    PermissionDenied(PermissionDenial),

    SystemMessage {
        event: String,
        text: String,
    },
}

pub trait EventSink: Send + Sync {
    fn emit(&self, ev: AgentEvent);
}
pub struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _: AgentEvent) {}
}

pub trait ToolJournal: Send + Sync {
    fn assistant(&self, message: &ChatMessage);

    fn tool_result(&self, tool_use_id: &str, content: &str, is_error: bool, final_result: bool);
}

pub const CACHE_VOLATILE_MARKER: &str = "\n\n<!-- zen-cache: volatile context below -->\n\n";

pub fn split_stable_volatile(prompt: &str) -> (&str, &str) {
    match prompt.split_once(CACHE_VOLATILE_MARKER) {
        Some((stable, volatile)) => (stable, volatile),
        None => (prompt, ""),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,

    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl UsageStats {
    pub fn cache_hit_rate(&self) -> f64 {
        cache_hit_rate(self.cache_read_input_tokens, self.input_tokens)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    #[serde(rename = "type")]
    pub record_type: String,
    pub subtype: String,
    pub is_error: bool,
    pub duration_ms: u64,
    pub duration_api_ms: u64,
    pub num_turns: usize,
    pub result: String,
    pub session_id: String,
    pub total_cost_usd: f64,
    pub usage: UsageStats,
    pub model: String,
    pub permission_denials: Vec<PermissionDenial>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub permissions: PermissionSettings,

    #[serde(default)]
    pub hooks: Value,
    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub auto_compact: Option<Value>,

    #[serde(default)]
    pub auto_commit: Option<bool>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    /// Loop-guard repeat threshold K (settings key `loopStuckK`);
    /// 0 disables the guard. Default 3.
    #[serde(default)]
    pub loop_stuck_k: Option<usize>,
    #[serde(default)]
    pub context_window: Option<u64>,

    #[serde(default)]
    pub allow_private_network: bool,

    #[serde(default)]
    pub rate_gate: RateGateFileConfig,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RateGateFileConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub daily_limit: Option<u64>,
    #[serde(default)]
    pub limits: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSettings {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub default_mode: Option<String>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
    /// Sandbox mode for child processes, JSON key `sandboxMode`
    /// (Phase 2 #12). `None` means "use the platform default".
    #[serde(default)]
    pub sandbox_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub body: String,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub system_prompt: String,

    pub disallowed_tools: Vec<String>,

    pub permission_mode: Option<String>,

    pub max_turns: Option<usize>,

    pub skills: Vec<String>,
}

#[async_trait]
pub trait SubAgentRunner: Send + Sync {
    async fn run_subagent(
        &self,
        def: &AgentDefinition,
        prompt: &str,
        parent_tool_use_id: &str,
    ) -> anyhow::Result<String>;

    /// The post-edit LSP diagnostics provider spawned children inherit
    /// (`None` when the parent session has the feedback loop off).
    fn diagnostics_provider(&self) -> Option<Arc<dyn DiagnosticsProvider>> {
        None
    }
}

pub fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let mut chars = 0usize;
    let mut images = 0u64;
    for m in messages {
        if let Some(content) = &m.content {
            chars += content.text_len();
            images += content.image_part_count() as u64;
        }
        chars += m
            .tool_calls
            .as_ref()
            .map(|v| {
                v.iter()
                    .map(|c| c.function.arguments.len() + c.function.name.len())
                    .sum()
            })
            .unwrap_or(0);
    }
    // Image parts cost a fixed estimate each (never the base64 length).
    chars as u64 / 4 + images * IMAGE_PART_TOKEN_ESTIMATE
}

pub fn extra_root_pem() -> Option<Vec<u8>> {
    let p = std::env::var_os("SSL_CERT_FILE")?;
    std::fs::read(p).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Posix,

    PowerShell,

    Cmd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: String,
    pub kind: ShellKind,
}

impl ShellSpec {
    pub fn args(&self, script: &str) -> Vec<String> {
        match self.kind {
            ShellKind::Posix => vec!["-c".into(), script.into()],
            ShellKind::PowerShell => {
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    script.into(),
                ]
            }
            ShellKind::Cmd => vec!["/C".into(), script.into()],
        }
    }
}

pub fn pick_shell(
    preferred: &str,
    windows: bool,
    zen_shell: Option<&str>,
    on_path: &dyn Fn(&str) -> bool,
) -> ShellSpec {
    let posix = |p: &str| ShellSpec {
        program: p.to_string(),
        kind: ShellKind::Posix,
    };
    if !windows {
        return posix(preferred);
    }
    let kind_of = |p: &str| {
        let file = p
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(p)
            .to_ascii_lowercase();
        let base = file.strip_suffix(".exe").unwrap_or(&file).to_string();
        match base.as_str() {
            "pwsh" | "powershell" => ShellKind::PowerShell,
            "cmd" => ShellKind::Cmd,
            _ => ShellKind::Posix,
        }
    };
    if let Some(z) = zen_shell.map(str::trim).filter(|z| !z.is_empty()) {
        return ShellSpec {
            program: z.to_string(),
            kind: kind_of(z),
        };
    }
    for cand in [preferred, "bash", "pwsh", "powershell"] {
        if on_path(cand) {
            return ShellSpec {
                program: cand.to_string(),
                kind: kind_of(cand),
            };
        }
    }
    ShellSpec {
        program: "cmd".into(),
        kind: ShellKind::Cmd,
    }
}

pub fn program_on_path(name: &str) -> bool {
    find_program(name).is_some()
}

pub fn find_program(name: &str) -> Option<PathBuf> {
    let system_root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .ok();
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let mut cands = vec![dir.join(name)];
            if cfg!(windows) {
                cands.insert(0, dir.join(format!("{}.exe", name)));
            }
            cands.into_iter().find(|p| {
                p.is_file() && !(cfg!(windows) && is_wsl_launcher(p, system_root.as_deref()))
            })
        })
    });
    if found.is_some() || !cfg!(windows) || !matches!(name, "bash" | "sh") {
        return found;
    }
    ["ProgramFiles", "ProgramW6432"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|pf| {
            PathBuf::from(pf)
                .join("Git")
                .join("bin")
                .join(format!("{}.exe", name))
        })
        .find(|p| p.is_file())
}

pub fn is_wsl_launcher(path: &Path, system_root: Option<&str>) -> bool {
    // A `bash.exe`/`sh.exe` under a `WindowsApps` directory is the
    // Microsoft Store execution alias for WSL, wherever it lives
    // (per-user `%LOCALAPPDATA%\Microsoft\WindowsApps` or the packaged
    // `C:\Program Files\WindowsApps\...`). It is not a native POSIX
    // shell: it starts a Linux bash that does not understand MSYS
    // `/c/...` paths, so hook commands written for Git Bash fail and
    // `$PWD` reporting breaks (PC hook diagnostics, round 5). It never
    // sits under the system root, so the prefix check below misses it;
    // reject it by path component instead, independent of
    // `system_root`.
    {
        let p = path
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let file = p.rsplit('\\').next().unwrap_or(&p);
        let base = file.strip_suffix(".exe").unwrap_or(file);
        if matches!(base, "bash" | "sh") && p.split('\\').any(|c| c == "windowsapps") {
            return true;
        }
    }
    let Some(root) = system_root.map(str::trim).filter(|r| !r.is_empty()) else {
        return false;
    };
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    let p = norm(&path.to_string_lossy());
    let file = p.rsplit('\\').next().unwrap_or(&p);
    let base = file.strip_suffix(".exe").unwrap_or(file);
    matches!(base, "bash" | "sh") && p.starts_with(&format!("{}\\", norm(root)))
}

pub fn api_key_env_names(api_key_env: Option<&str>) -> Vec<String> {
    let mut names = vec!["ZEN_API_KEY".to_string(), "OPENAI_API_KEY".to_string()];
    if let Some(n) = api_key_env {
        let n = n.trim();
        if !n.is_empty() && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    names
}

pub fn resolve_shell(preferred: &str) -> ShellSpec {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, ShellSpec>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(s) = lock_recovery::lock_or_recover(cache, "shell cache").get(preferred) {
        return s.clone();
    }
    let zen_shell = std::env::var("ZEN_SHELL").ok();
    let mut spec = pick_shell(
        preferred,
        cfg!(windows),
        zen_shell.as_deref(),
        &program_on_path,
    );

    if cfg!(windows)
        && zen_shell
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        && let Some(p) = find_program(&spec.program)
    {
        spec.program = p.to_string_lossy().into_owned();
    }
    lock_recovery::lock_or_recover(cache, "shell cache")
        .insert(preferred.to_string(), spec.clone());
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_mode_as_str_and_parse_roundtrip() {
        // Kills the as_str mutants ("" / "xyzzy") and the parse
        // mutants (whole-body -> None, and each deleted match arm).
        assert_eq!(SandboxMode::Off.as_str(), "off");
        assert_eq!(SandboxMode::WorkspaceWrite.as_str(), "workspace-write");
        assert_eq!(SandboxMode::ReadOnly.as_str(), "read-only");
        assert_eq!(SandboxMode::parse("off"), Some(SandboxMode::Off));
        assert_eq!(
            SandboxMode::parse("workspace-write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            SandboxMode::parse("workspace_write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(SandboxMode::parse("read-only"), Some(SandboxMode::ReadOnly));
        assert_eq!(SandboxMode::parse("read_only"), Some(SandboxMode::ReadOnly));
        assert_eq!(SandboxMode::parse("bogus"), None);
        for mode in [
            SandboxMode::Off,
            SandboxMode::WorkspaceWrite,
            SandboxMode::ReadOnly,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn hook_events_all_33_roundtrip() {
        let all = HookEvent::all();
        assert_eq!(all.len(), 33);
        for ev in all {
            assert_eq!(
                HookEvent::parse_name(ev.as_str()),
                Some(ev),
                "{}",
                ev.as_str()
            );
        }
        assert_eq!(HookEvent::parse_name("NoSuchEvent"), None);
        assert_eq!(HookEvent::parse_name("pretooluse"), None);
    }

    #[test]
    fn acp_advertised_modes_are_the_q3_set_in_wire_order() {
        assert_eq!(
            PermissionMode::acp_advertised_modes(),
            [
                PermissionMode::Default,
                PermissionMode::Plan,
                PermissionMode::AcceptEdits,
                PermissionMode::AskAll,
            ]
        );
        for mode in PermissionMode::acp_advertised_modes() {
            assert!(mode.is_acp_advertised(), "{mode:?}");
        }
    }

    #[test]
    fn unadvertised_modes_are_bypass_and_dont_ask() {
        assert!(!PermissionMode::BypassPermissions.is_acp_advertised());
        assert!(!PermissionMode::DontAsk.is_acp_advertised());
        // The advertised set plus the unadvertised pair is the whole
        // model: no mode is left out of the classification.
        let advertised = PermissionMode::acp_advertised_modes();
        assert_eq!(advertised.len(), 4);
        assert!(!advertised.contains(&PermissionMode::BypassPermissions));
        assert!(!advertised.contains(&PermissionMode::DontAsk));
    }
    #[test]
    fn message_serializes_without_nulls() {
        let s = serde_json::to_string(&ChatMessage::user("hi")).unwrap();
        assert_eq!(s, r#"{"role":"user","content":"hi"}"#);
    }
    #[test]
    fn message_parts_serialize_openai_compatible() {
        // Golden JSON: a user message with a text part and an image_url part
        // serialises to the exact OpenAI-compatible shape.
        let msg = ChatMessage::user_parts(vec![
            ContentPart::text("Describe this image"),
            ContentPart::image_url("data:image/png;base64,iVBORw0KGgo="),
        ]);
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            s,
            r#"{"role":"user","content":[{"type":"text","text":"Describe this image"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"auto"}}]}"#
        );
    }
    #[test]
    fn message_content_roundtrips_string_and_parts() {
        let from_string: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert_eq!(from_string, ChatMessage::user("hi"));
        let from_parts: ChatMessage = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA","detail":"auto"}}]}"#,
        )
        .unwrap();
        assert_eq!(from_parts.image_part_count(), 1);
        assert_eq!(from_parts.text(), "hi");
        // The base64 payload never leaks into the transcript view.
        assert!(!from_parts.transcript_text().contains("AAAA"));
        assert!(from_parts.transcript_text().contains("[image]"));
    }
    #[test]
    fn estimate_tokens_charges_image_parts_fixed_estimate() {
        let text_only = vec![ChatMessage::user("hi")];
        assert_eq!(estimate_tokens(&text_only), 0);
        let with_image = vec![ChatMessage::user_parts(vec![
            ContentPart::text("hi"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
        ])];
        assert_eq!(
            estimate_tokens(&with_image),
            IMAGE_PART_TOKEN_ESTIMATE,
            "one image part costs the fixed estimate, not base64 length / 4"
        );
        // A long base64 payload must not raise the estimate.
        let long_payload = "A".repeat(100_000);
        let with_long_image = vec![ChatMessage::user_parts(vec![
            ContentPart::text("hi"),
            ContentPart::image_url(format!("data:image/png;base64,{long_payload}")),
        ])];
        assert_eq!(estimate_tokens(&with_long_image), IMAGE_PART_TOKEN_ESTIMATE);
        let two_images = vec![ChatMessage::user_parts(vec![
            ContentPart::image_url("data:image/png;base64,AAAA"),
            ContentPart::image_url("data:image/png;base64,BBBB"),
        ])];
        assert_eq!(estimate_tokens(&two_images), 2 * IMAGE_PART_TOKEN_ESTIMATE);
    }
    #[test]
    fn tool_message_carries_role_content_and_call_id() {
        // Exact field values: deleting the `role` or `content` field from
        // the struct literal (filled in by `..Default::default()`) must fail.
        let msg = ChatMessage::tool("call_123", "result text");
        assert_eq!(msg.role, "tool");
        assert_eq!(
            msg.content,
            Some(MessageContent::Text("result text".into()))
        );
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(msg.text_ref(), Some("result text"));
        assert_eq!(msg.text(), "result text");
        // Serialised shape pins role, content and call id together.
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"role":"tool","content":"result text","tool_call_id":"call_123"}"#
        );
    }
    #[test]
    fn text_ref_is_some_only_for_plain_text_content() {
        // Exact value: kills replacing the body with None, Some("") and
        // Some("xyzzy"), and deleting the Some(MessageContent::Text(s)) arm.
        assert_eq!(ChatMessage::user("hello").text_ref(), Some("hello"));
        // Parts content is never plain text, even with text parts in it.
        let parts = ChatMessage::user_parts(vec![
            ContentPart::text("hello"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
        ]);
        assert_eq!(parts.text_ref(), None);
        // Absent content is not plain text.
        assert_eq!(ChatMessage::assistant(None, None).text_ref(), None);
        // Empty text is still Some(""): empty is not absent.
        assert_eq!(ChatMessage::user("").text_ref(), Some(""));
    }
    #[test]
    fn image_part_count_counts_only_image_parts() {
        // Zero kills the "replace with 1" mutant; two pins the count.
        assert_eq!(ChatMessage::user("hi").image_part_count(), 0);
        assert_eq!(ChatMessage::assistant(None, None).image_part_count(), 0);
        let one = ChatMessage::user_parts(vec![
            ContentPart::text("hi"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
        ]);
        assert_eq!(one.image_part_count(), 1);
        let two = ChatMessage::user_parts(vec![
            ContentPart::image_url("data:image/png;base64,AAAA"),
            ContentPart::image("image/png", "AAAA"),
        ]);
        assert_eq!(two.image_part_count(), 2);
        let text_only =
            ChatMessage::user_parts(vec![ContentPart::text("a"), ContentPart::text("b")]);
        assert_eq!(text_only.image_part_count(), 0);
        // Same assertions on MessageContent directly.
        assert_eq!(MessageContent::Text("hi".into()).image_part_count(), 0);
        assert_eq!(
            MessageContent::Parts(vec![
                ContentPart::text("a"),
                ContentPart::image_url("data:image/png;base64,AAAA"),
                ContentPart::image("image/png", "AAAA"),
            ])
            .image_part_count(),
            2
        );
    }
    #[test]
    fn estimate_tokens_accumulates_text_and_images_exactly() {
        // Exact totals: every operator mutation changes the result.
        assert_eq!(estimate_tokens(&[]), 0);
        // Single text message: 8 chars / 4 = 2. Kills `chars +=` -> `*=`.
        assert_eq!(estimate_tokens(&[ChatMessage::user("abcdefgh")]), 2);
        // Two messages accumulate: (8 + 8) / 4 = 4.
        assert_eq!(
            estimate_tokens(&[ChatMessage::user("abcdefgh"), ChatMessage::user("ijklmnop"),]),
            4
        );
        // Text + one image: 8 / 4 + 384 = 386.
        assert_eq!(
            estimate_tokens(&[ChatMessage::user_parts(vec![
                ContentPart::text("abcdefgh"),
                ContentPart::image_url("data:image/png;base64,AAAA"),
            ])]),
            2 + IMAGE_PART_TOKEN_ESTIMATE
        );
        // Text + two images: 8 / 4 + 2 * 384 = 770.
        // Kills `*` -> `+` / `/` and `+` -> `-` in the final expression.
        assert_eq!(
            estimate_tokens(&[ChatMessage::user_parts(vec![
                ContentPart::text("abcdefgh"),
                ContentPart::image_url("data:image/png;base64,AAAA"),
                ContentPart::image("image/png", "AAAA"),
            ])]),
            2 + 2 * IMAGE_PART_TOKEN_ESTIMATE
        );
        // Long text + one image: 2000 / 4 + 384 = 884. The `+` -> `-`
        // mutant gives 500 - 384 = 116 here (no underflow), so the exact
        // value is what kills it.
        let long = "x".repeat(2000);
        assert_eq!(
            estimate_tokens(&[ChatMessage::user_parts(vec![
                ContentPart::text(long),
                ContentPart::image_url("data:image/png;base64,AAAA"),
            ])]),
            500 + IMAGE_PART_TOKEN_ESTIMATE
        );
    }
    #[test]
    fn estimate_tokens_counts_tool_call_name_and_arguments() {
        // Tool call name "get_time" (8) + arguments (16) = 24 chars,
        // 24 / 4 = 6. Kills `+=` -> `*=` / `-=` on the tool-call
        // accumulation and `+` -> `-` inside the length sum.
        let call = ChatMessage::assistant(
            None,
            Some(vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "get_time".into(),
                    arguments: r#"{"city":"Hanoi"}"#.into(),
                },
            }]),
        );
        assert_eq!(estimate_tokens(&[call]), 6);
        // Text + tool call accumulate: (8 + 24) / 4 = 8.
        let with_text = ChatMessage::assistant(
            Some("abcdefgh".into()),
            Some(vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "get_time".into(),
                    arguments: r#"{"city":"Hanoi"}"#.into(),
                },
            }]),
        );
        assert_eq!(estimate_tokens(&[with_text]), 8);
    }
    #[test]
    fn user_parts_and_assistant_content_carry_role_and_content() {
        let parts = ChatMessage::user_parts(vec![ContentPart::text("hi")]);
        assert_eq!(parts.role, "user");
        assert_eq!(parts.text(), "hi");
        assert_eq!(parts.image_part_count(), 0);
        let content = ChatMessage::assistant_content(
            Some(MessageContent::Parts(vec![ContentPart::text("hi")])),
            None,
        );
        assert_eq!(content.role, "assistant");
        assert_eq!(content.text(), "hi");
        // Parts content is not plain text.
        assert_eq!(content.text_ref(), None);
    }
    #[test]
    fn message_content_text_transcript_and_len_pin_multimodal_views() {
        assert_eq!(MessageContent::Text("hi".into()).text(), "hi");
        let parts = MessageContent::Parts(vec![
            ContentPart::text("a"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
            ContentPart::text("b"),
        ]);
        // Only text parts join; image parts contribute nothing.
        assert_eq!(parts.text(), "ab");
        assert_eq!(parts.transcript_text(), "a[image]b");
        assert_eq!(parts.text_len(), 2);
        assert_eq!(MessageContent::Text("hello".into()).text_len(), 5);
        assert_eq!(MessageContent::Text("hi".into()).transcript_text(), "hi");
        // Image-only parts have no text at all.
        let images_only =
            MessageContent::Parts(vec![ContentPart::image_url("data:image/png;base64,AAAA")]);
        assert_eq!(images_only.text(), "");
        assert_eq!(images_only.text_len(), 0);
    }
    #[test]
    fn content_part_is_image_distinguishes_text_from_images() {
        assert!(!ContentPart::text("hi").is_image());
        assert!(ContentPart::image_url("data:image/png;base64,AAAA").is_image());
        assert!(ContentPart::image("image/png", "AAAA").is_image());
    }
    #[test]
    fn chat_message_text_and_transcript_views() {
        assert_eq!(ChatMessage::user("hi").text(), "hi");
        assert_eq!(ChatMessage::assistant(None, None).text(), "");
        let parts = ChatMessage::user_parts(vec![
            ContentPart::text("a"),
            ContentPart::image_url("data:image/png;base64,AAAA"),
        ]);
        assert_eq!(parts.text(), "a");
        assert_eq!(parts.transcript_text(), "a[image]");
        assert_eq!(ChatMessage::user("hi").transcript_text(), "hi");
        assert_eq!(ChatMessage::assistant(None, None).transcript_text(), "");
    }
    #[test]
    fn system_message_carries_role_and_content() {
        // Exact field values: replacing "system" with "" or the content
        // with None must fail, as must emptying the text.
        let msg = ChatMessage::system("hello");
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, Some(MessageContent::Text("hello".into())));
        assert_eq!(msg.tool_calls, None);
        assert_eq!(msg.tool_call_id, None);
        assert_eq!(msg.name, None);
        assert_eq!(msg.text_ref(), Some("hello"));
        assert_eq!(msg.text(), "hello");
        // Serialised shape pins role and content together.
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "system");
        assert_eq!(v["content"], "hello");
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("tool_call_id").is_none());
        // Empty text is still content, not absent.
        assert_eq!(ChatMessage::system("").text_ref(), Some(""));
    }
    #[test]
    fn assistant_message_carries_role_content_and_calls() {
        // Exact field values: replacing "assistant" with "", the content
        // with None, or the tool calls with None must fail.
        let msg = ChatMessage::assistant(
            Some("hi".into()),
            Some(vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "Read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }]),
        );
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, Some(MessageContent::Text("hi".into())));
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].function.name, "Read");
        assert_eq!(msg.tool_call_id, None);
        assert_eq!(msg.name, None);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "hi");
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "Read");
        // No content and no calls stay absent, not empty.
        let bare = ChatMessage::assistant(None, None);
        assert_eq!(bare.role, "assistant");
        assert_eq!(bare.content, None);
        assert_eq!(bare.tool_calls, None);
        assert_eq!(bare.text_ref(), None);
        // Calls without text keep the calls and no content.
        let calls_only = ChatMessage::assistant(
            None,
            Some(vec![ToolCall {
                id: "call_2".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "Bash".into(),
                    arguments: "{}".into(),
                },
            }]),
        );
        assert_eq!(calls_only.content, None);
        assert_eq!(calls_only.tool_calls.as_ref().unwrap().len(), 1);
    }
    #[test]
    fn settings_parse_camel_case_and_extra() {
        let s: Settings = serde_json::from_str(r#"{"model":"m","baseUrl":"u","permissions":{"allow":["Read"],"defaultMode":"plan"},"foo":1}"#).unwrap();
        assert_eq!(s.base_url.as_deref(), Some("u"));
        assert_eq!(s.permissions.allow, vec!["Read"]);
        assert_eq!(s.extra.get("foo"), Some(&serde_json::json!(1)));
    }
    #[test]
    fn mode_roundtrip() {
        for m in [
            "default",
            "acceptEdits",
            "plan",
            "bypassPermissions",
            "dontAsk",
            "askAll",
        ] {
            assert_eq!(PermissionMode::parse(m).unwrap().as_str(), m);
        }
        // Kebab-case spellings map to the same modes.
        assert_eq!(
            PermissionMode::parse("accept-edits").unwrap().as_str(),
            "acceptEdits"
        );
        assert_eq!(
            PermissionMode::parse("bypass-permissions")
                .unwrap()
                .as_str(),
            "bypassPermissions"
        );
        assert_eq!(
            PermissionMode::parse("dont-ask").unwrap().as_str(),
            "dontAsk"
        );
        assert_eq!(PermissionMode::parse("ask-all").unwrap().as_str(), "askAll");
        // 'auto' and 'manual' are not modes and never mean default.
        assert_eq!(PermissionMode::parse("auto"), None);
        assert_eq!(PermissionMode::parse("manual"), None);
    }

    #[test]
    fn p0_7_shell_unix_is_unchanged() {
        let none = |_: &str| false;
        assert_eq!(
            pick_shell("bash", false, Some("pwsh"), &none),
            ShellSpec {
                program: "bash".into(),
                kind: ShellKind::Posix
            }
        );
        assert_eq!(pick_shell("sh", false, None, &none).program, "sh");
    }
    #[test]
    fn p0_7_shell_windows_fallback_order() {
        let only = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        assert_eq!(
            pick_shell("bash", true, None, &only(&["bash", "pwsh"])).kind,
            ShellKind::Posix
        );
        let s = pick_shell("bash", true, None, &only(&["pwsh", "powershell"]));
        assert_eq!(
            (s.program.as_str(), s.kind),
            ("pwsh", ShellKind::PowerShell)
        );
        let s = pick_shell("bash", true, None, &only(&["powershell"]));
        assert_eq!(
            (s.program.as_str(), s.kind),
            ("powershell", ShellKind::PowerShell)
        );
        let s = pick_shell("sh", true, None, &only(&[]));
        assert_eq!((s.program.as_str(), s.kind), ("cmd", ShellKind::Cmd));
        let s = pick_shell(
            "bash",
            true,
            Some(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            &only(&["bash"]),
        );
        assert_eq!(s.kind, ShellKind::PowerShell);
        assert_eq!(
            ShellSpec {
                program: "cmd".into(),
                kind: ShellKind::Cmd
            }
            .args("dir"),
            vec!["/C", "dir"]
        );
        assert_eq!(
            ShellSpec {
                program: "bash".into(),
                kind: ShellKind::Posix
            }
            .args("ls"),
            vec!["-c", "ls"]
        );
    }
    #[test]
    fn windows_bash_skips_wsl_launcher() {
        let root = Some(r"C:\Windows");
        assert!(is_wsl_launcher(
            Path::new(r"C:\Windows\System32\bash.exe"),
            root
        ));
        assert!(is_wsl_launcher(
            Path::new(r"c:/windows/system32/BASH.EXE"),
            root
        ));
        assert!(is_wsl_launcher(
            Path::new(r"C:\WINDOWS\system32\sh.exe"),
            Some(r"C:\Windows\")
        ));
        assert!(!is_wsl_launcher(
            Path::new(r"C:\Program Files\Git\bin\bash.exe"),
            root
        ));
        assert!(!is_wsl_launcher(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            root
        ));
        assert!(is_wsl_launcher(Path::new(r"C:\WindowsApps\bash.exe"), root));
        assert!(is_wsl_launcher(
            Path::new(r"C:\Users\Z\AppData\Local\Microsoft\WindowsApps\bash.exe"),
            root
        ));
        assert!(is_wsl_launcher(
            Path::new(r"C:\Program Files\WindowsApps\bash.exe"),
            None
        ));
        assert!(!is_wsl_launcher(
            Path::new(r"C:\Windows\System32\bash.exe"),
            None
        ));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn split_stable_volatile_with_marker() {
        let p = format!("stable{}volatile", CACHE_VOLATILE_MARKER);
        let (s, v) = split_stable_volatile(&p);
        assert_eq!(s, "stable");
        assert_eq!(v, "volatile");
    }

    #[test]
    fn split_stable_volatile_without_marker_is_all_stable() {
        let (s, v) = split_stable_volatile("just a prompt");
        assert_eq!(s, "just a prompt");
        assert_eq!(v, "");
    }

    #[test]
    fn cache_hit_rate_math() {
        assert_eq!(cache_hit_rate(0, 0), 0.0);
        assert_eq!(cache_hit_rate(0, 100), 0.0);
        assert!((cache_hit_rate(75, 100) - 0.75).abs() < 1e-9);

        assert_eq!(cache_hit_rate(150, 100), 1.0);
    }

    #[test]
    fn usage_stats_cache_hit_rate() {
        let u = UsageStats {
            input_tokens: 1000,
            output_tokens: 10,
            cache_read_input_tokens: 900,
        };
        assert!((u.cache_hit_rate() - 0.9).abs() < 1e-9);
        assert_eq!(UsageStats::default().cache_hit_rate(), 0.0);
    }

    #[test]
    fn usage_deserializes_old_json_without_cache_fields() {
        let u: Usage =
            serde_json::from_str(r#"{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}"#)
                .unwrap();
        assert_eq!(u.cache_read_input_tokens, 0);
        assert_eq!(u.cache_creation_input_tokens, 0);
        let s: UsageStats =
            serde_json::from_str(r#"{"input_tokens":5,"output_tokens":2}"#).unwrap();
        assert_eq!(s.cache_read_input_tokens, 0);
    }
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0] == "**" {
        return (0..=path.len()).any(|i| match_segments(&pat[1..], &path[i..]));
    }
    if path.is_empty() || !match_segment(pat[0], path[0]) {
        return false;
    }
    match_segments(&pat[1..], &path[1..])
}

fn match_segment(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let c: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut ss) = (None, 0usize);
    while si < c.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == c[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            ss = si;
        } else if let Some(sp) = star {
            pi = sp + 1;
            ss += 1;
            si = ss;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/**", "src/x.rs"));
        assert!(!glob_match("src/**", "other/x.rs"));
        assert!(glob_match("exact/path.rs", "exact/path.rs"));
        assert!(!glob_match("exact/path.rs", "exact/other.rs"));
    }

    #[test]
    fn glob_star_stays_in_segment() {
        assert!(glob_match("*.md", "README.md"));
        assert!(!glob_match("*.md", "docs/a.md"));
        assert!(glob_match("docs/*.md", "docs/a.md"));
        assert!(!glob_match("docs/*.md", "docs/sub/a.md"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
    }

    #[test]
    fn glob_double_star_spans_segments() {
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("**/*.rs", "c.rs"));
        assert!(glob_match("a/**/z.rs", "a/z.rs"));
        assert!(glob_match("a/**/z.rs", "a/b/c/z.rs"));
        assert!(!glob_match("a/**/z.rs", "a/b/c/y.rs"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("src/?ain.rs", "src/main.rs"));
        assert!(!glob_match("src/?ain.rs", "src/xxain.rs"));
        assert!(!glob_match("src/?ain.rs", "src/main.tsx"));
    }

    fn sample_diag(severity: u8, message: &str) -> LspDiagnostic {
        LspDiagnostic {
            line: 3,
            col: 5,
            end_line: 3,
            end_col: 9,
            severity,
            message: message.to_string(),
            source: Some("mock".to_string()),
            code: None,
        }
    }

    #[test]
    fn lsp_diagnostic_pretty_and_severity_labels() {
        assert_eq!(
            sample_diag(1, "type error").pretty(),
            "Error [3:5] type error"
        );
        assert_eq!(sample_diag(2, "w").severity_label(), "Warning");
        assert_eq!(sample_diag(3, "i").severity_label(), "Information");
        assert_eq!(sample_diag(4, "h").severity_label(), "Hint");
        assert_eq!(sample_diag(9, "?").severity_label(), "Diagnostic");
    }

    #[test]
    fn lsp_diagnostic_is_actionable_only_error_and_warning() {
        assert!(sample_diag(1, "e").is_actionable());
        assert!(sample_diag(2, "w").is_actionable());
        assert!(!sample_diag(3, "i").is_actionable());
        assert!(!sample_diag(4, "h").is_actionable());
        assert!(!sample_diag(0, "unknown").is_actionable());
        assert!(!sample_diag(9, "unknown").is_actionable());
    }

    #[test]
    fn lsp_diagnostic_dedup_key_ignores_severity_source_code() {
        let a = sample_diag(1, "same");
        let mut b = sample_diag(2, "same");
        b.source = Some("other".to_string());
        b.code = Some("E1".to_string());
        assert_eq!(a.dedup_key(), b.dedup_key());
        assert_ne!(a.dedup_key(), sample_diag(1, "different").dedup_key());
        let mut moved = sample_diag(1, "same");
        moved.col = 6;
        assert_ne!(a.dedup_key(), moved.dedup_key());
    }

    #[test]
    fn render_diagnostics_caps_at_20_lines() {
        let diags: Vec<LspDiagnostic> = (0..25)
            .map(|i| sample_diag(1, &format!("error {i}")))
            .collect();
        let out = render_diagnostics(&diags);
        assert_eq!(out.lines().count(), 21);
        assert!(out.contains("error 0"), "{out}");
        assert!(out.contains("error 19"), "{out}");
        assert!(!out.contains("error 20"), "{out}");
        assert!(out.contains("... and 5 more"), "{out}");

        let few = vec![sample_diag(1, "only")];
        assert_eq!(render_diagnostics(&few), "Error [3:5] only");
        assert_eq!(render_diagnostics(&[]), "");

        // Exactly at the cap: no suffix line (boundary for `>` vs `>=`).
        let exact: Vec<LspDiagnostic> = (0..20)
            .map(|i| sample_diag(1, &format!("error {i}")))
            .collect();
        let out = render_diagnostics(&exact);
        assert_eq!(out.lines().count(), 20);
        assert!(!out.contains("more"), "{out}");
        // One over the cap: the suffix appears with the right count.
        let over: Vec<LspDiagnostic> = (0..21)
            .map(|i| sample_diag(1, &format!("error {i}")))
            .collect();
        assert!(
            render_diagnostics(&over).contains("... and 1 more"),
            "{}",
            render_diagnostics(&over)
        );
    }

    struct DummyProvider;

    #[async_trait]
    impl DiagnosticsProvider for DummyProvider {
        async fn diagnostics_after_edit(&self, _path: &Path) -> Vec<LspDiagnostic> {
            Vec::new()
        }
    }

    #[test]
    fn diagnostics_provider_debug_names_the_trait() {
        let provider: &dyn DiagnosticsProvider = &DummyProvider;
        let out = format!("{provider:?}");
        assert!(out.contains("DiagnosticsProvider"), "{out}");
    }

    #[test]
    fn c3_tool_context_cwd_returns_constructed_cwd() {
        let ctx = ToolContext::new(PathBuf::from("/work/dir"), "sess-1");
        assert_eq!(ctx.cwd(), PathBuf::from("/work/dir"));
    }
}
