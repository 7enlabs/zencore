use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub const BUILTIN_COMMANDS: &[&str] = &[
    "/help",
    "/clear",
    "/compact",
    "/model",
    "/provider",
    "/resume",
    "/recall",
    "/cost",
    "/status",
    "/permissions",
    "/config",
    "/memory",
    "/init",
    "/todos",
    "/agents",
    "/mcp",
    "/hooks",
    "/skills",
    "/exit",
    "/context",
    "/add-dir",
    "/export",
    "/review",
    "/rename",
    "/quota",
    "/diff",
    "/plan",
    "/memories",
    "/loop",
    "/goal",
    "/personality",
    "/vim",
    "/theme",
    "/btw",
    "/terse",
    "/map",
    "/map-refresh",
    "/deep-research",
    "/pr_comments",
    "/ci-fix",
    "/done",
    "/scope",
    "/worktree",
    "/commit",
];

pub const REPL_ONLY_COMMANDS: &[&str] = &["/undo", "/expand"];

// One-line English description for every slash command, kept next to the
// command tables so a new command gains its description in the same change.
// The REPL prompt completion and hinting read this table; a test asserts
// every builtin and REPL-only command has an entry.
pub const COMMAND_DESCRIPTIONS: &[(&str, &str)] = &[
    ("/help", "List the built-in commands"),
    ("/clear", "Clear the conversation context"),
    ("/compact", "Compact the conversation to free context space"),
    ("/model", "Show or switch the current model"),
    (
        "/provider",
        "Switch provider; model resets to that provider's best free model",
    ),
    ("/resume", "Resume a saved session"),
    (
        "/recall",
        "Load summaries of previous sessions in this directory",
    ),
    (
        "/continue",
        "Resume the most recent session (alias of /resume)",
    ),
    ("/cost", "Show token usage and cost for this session"),
    ("/status", "Show session status"),
    ("/permissions", "Show or change the permission mode"),
    ("/config", "Show the resolved configuration"),
    ("/memory", "Show the memory files in effect"),
    ("/memories", "List saved memories"),
    ("/init", "Create or refresh the project memory file"),
    ("/todos", "Show the current todo list"),
    ("/agents", "List subagent definitions"),
    ("/mcp", "Show MCP server status"),
    ("/hooks", "Show configured hooks"),
    ("/skills", "List available skills"),
    ("/exit", "Exit the REPL"),
    ("/quit", "Exit the REPL (alias of /exit)"),
    ("/undo", "Undo the last file-changing turn"),
    ("/expand", "Expand or collapse a tool result card by number"),
    ("/context", "Show what is filling the context window"),
    ("/add-dir", "Add a working directory for this session"),
    ("/export", "Export the conversation"),
    ("/review", "Review the current diff with the agent"),
    ("/rename", "Rename the current session"),
    ("/quota", "Show provider quota status"),
    ("/diff", "Show uncommitted changes"),
    ("/plan", "Toggle plan mode (read-only planning)"),
    ("/loop", "Run a prompt repeatedly"),
    ("/goal", "Set a session goal"),
    ("/personality", "Switch the assistant personality"),
    ("/vim", "Toggle vi key bindings"),
    ("/theme", "Switch the color theme"),
    ("/btw", "Ask a side question outside the main thread"),
    ("/terse", "Toggle terse output mode"),
    ("/map", "Show a map of the repository"),
    ("/map-refresh", "Rebuild the repository map"),
    ("/deep-research", "Run a deep research pass on a topic"),
    ("/pr_comments", "Fetch PR review comments and address them"),
    ("/ci-fix", "Check CI status and offer to fix failures"),
    ("/done", "Mark the current work as done"),
    ("/scope", "Show or set the session scope guard"),
    ("/worktree", "Manage git worktrees for this session"),
    ("/commit", "Commit changes with the agent's help"),
];

/// Every slash command offered at the REPL prompt - builtins, REPL-only
/// commands and the two accepted aliases - each with its description, in
/// catalog order. Commands without a description are dropped here, which the
/// coverage test treats as a failure.
/// Display group for a slash command in `/help` (v2 design). Groups are
/// display-only; command names and descriptions do not change.
pub fn help_group(name: &str) -> &'static str {
    match name {
        "/help" | "/model" | "/provider" | "/cost" | "/status" | "/resume" | "/continue"
        | "/exit" | "/quit" | "/theme" | "/personality" | "/vim" | "/terse" | "/quota"
        | "/config" | "/permissions" => "Session & model",
        "/clear" | "/compact" | "/context" | "/memory" | "/memories" | "/init" | "/add-dir"
        | "/scope" | "/recall" => "Context",
        "/diff" | "/review" | "/commit" | "/undo" | "/plan" | "/ci-fix" | "/pr_comments"
        | "/worktree" | "/done" | "/goal" | "/loop" | "/export" | "/rename" | "/map"
        | "/map-refresh" | "/deep-research" => "Code & review",
        _ => "Tools",
    }
}

pub const HELP_GROUP_ORDER: &[&str] = &["Session & model", "Context", "Code & review", "Tools"];

pub fn slash_command_catalog() -> Vec<(&'static str, &'static str)> {
    let names = BUILTIN_COMMANDS
        .iter()
        .chain(REPL_ONLY_COMMANDS.iter())
        .chain(["/quit", "/continue"].iter())
        .copied();
    let mut out = Vec::new();
    for name in names {
        if let Some((_, desc)) = COMMAND_DESCRIPTIONS.iter().find(|(n, _)| *n == name) {
            out.push((name, *desc));
        }
    }
    out
}

pub const HEADLESS_BUILTINS: &[&str] = &[
    "compact", "model", "cost", "mcp", "context", "review", "init", "done",
];

pub fn parse_builtin(prompt: &str) -> Option<(String, String)> {
    let rest = prompt.trim_start().strip_prefix('/')?;
    let mut it = rest.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or("");
    let known =
        BUILTIN_COMMANDS.iter().any(|c| &c[1..] == name) || matches!(name, "quit" | "continue");
    if name.is_empty() || !known {
        return None;
    }
    Some((name.to_string(), it.next().unwrap_or("").trim().to_string()))
}

pub fn review_prompt(args: &str) -> String {
    let mut p = String::from(
        "Review the current changes in this repository. Start with `git status` and `git diff` \
         (and `git diff --staged`); if there are no local changes, review the latest commit. \
         Report concrete problems first (bugs, missing error handling, security issues, missing \
         tests), each with file and line, then smaller style notes. Do not modify files.",
    );
    if !args.trim().is_empty() {
        p.push_str(&format!("\n\nFocus: {}", args.trim()));
    }
    p
}

pub fn init_prompt() -> String {
    "Look through this repository (layout, build files, README, tests) and write a concise \
     ZEN.md at the project root for future coding sessions: how to build, test and lint, the \
     main modules and how they fit together, and project conventions. If ZEN.md already exists, \
     improve it instead of replacing useful content."
        .to_string()
}

pub fn init_prompt_with_imports(candidates: &[std::path::PathBuf]) -> String {
    let files = candidates
        .iter()
        .map(|p| format!("- {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\nBefore writing, read these existing agent memory files and incorporate any \
         still-relevant instructions into the new ZEN.md (dedupe against each other, keep zen's \
         concise style, drop tool-specific syntax that does not apply to zen):\n{}\nAfter \
         importing, the old files can be deleted: zen loads ZEN.md preferentially and ignores \
         AGENTS.md in any directory that has one.",
        init_prompt(),
        files
    )
}

/// Bytes one message adds to the estimate; the same terms as
/// `zen_core::estimate_tokens`, so the groups below add up to its total.
fn message_bytes(m: &zen_core::ChatMessage) -> usize {
    // Image parts cost the fixed estimate; expressed in bytes (4 bytes per
    // token) so the groups below still add up to `estimate_tokens`.
    m.text().len()
        + m.image_part_count() * (zen_core::IMAGE_PART_TOKEN_ESTIMATE as usize * 4)
        + m.tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| c.function.arguments.len() + c.function.name.len())
                    .sum()
            })
            .unwrap_or(0)
}

/// Estimated token cost of each part of the context, in the four groups
/// `/context` prints. The parts always add up to `total`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub instructions: u64,
    pub memory: u64,
    pub tools: u64,
    pub history_conversation: u64,
    pub history_tools: u64,
}

impl ContextBreakdown {
    pub fn history(&self) -> u64 {
        self.history_conversation + self.history_tools
    }

    pub fn total(&self) -> u64 {
        self.instructions + self.memory + self.tools + self.history()
    }
}

/// `memory` is the memory block (ZEN.md / AGENTS.md text) that was placed
/// inside `system_prompt`; it is counted as memory only when that exact text is
/// still found in the prompt, otherwise it stays in instructions.
pub fn context_breakdown(
    system_prompt: &str,
    memory: &str,
    tools: &[zen_core::ToolDefinition],
    messages: &[zen_core::ChatMessage],
) -> ContextBreakdown {
    let sys = system_prompt.len() as u64 / 4;
    let memory = memory.trim();
    let memory_tokens = if !memory.is_empty() && system_prompt.contains(memory) {
        (memory.len() as u64 / 4).min(sys)
    } else {
        0
    };
    let tools_tokens = serde_json::to_string(tools)
        .map(|t| t.len() as u64 / 4)
        .unwrap_or(0);
    let all_bytes: usize = messages.iter().map(message_bytes).sum();
    let tool_bytes: usize = messages
        .iter()
        .map(|m| {
            if m.role == "tool" {
                message_bytes(m)
            } else {
                m.tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| c.function.arguments.len() + c.function.name.len())
                            .sum()
                    })
                    .unwrap_or(0)
            }
        })
        .sum();
    let history_total = all_bytes as u64 / 4;
    let history_tools = (tool_bytes as u64 / 4).min(history_total);
    ContextBreakdown {
        instructions: sys - memory_tokens,
        memory: memory_tokens,
        tools: tools_tokens,
        history_conversation: history_total - history_tools,
        history_tools,
    }
}

pub fn context_report(
    system_prompt: &str,
    memory: &str,
    tools: &[zen_core::ToolDefinition],
    messages: &[zen_core::ChatMessage],
    context_window: u64,
    last_prompt_tokens: u64,
) -> String {
    let b = context_breakdown(system_prompt, memory, tools, messages);
    let total = b.total();
    let pct = |n: u64| {
        if context_window == 0 {
            0.0
        } else {
            n as f64 * 100.0 / context_window as f64
        }
    };
    let mut out = format!(
        "Context usage (estimated): {} / {} tokens ({:.1}%)\n\
         - instructions: {} tokens\n\
         - memory: {} tokens\n\
         - tools ({}): {} tokens\n\
         - messages ({}): {} tokens\n\
         \x20   - conversation: {} tokens\n\
         \x20   - tool calls and results: {} tokens\n\
         - free: {} tokens",
        total,
        context_window,
        pct(total),
        b.instructions,
        b.memory,
        tools.len(),
        b.tools,
        messages.len(),
        b.history(),
        b.history_conversation,
        b.history_tools,
        context_window.saturating_sub(total)
    );
    if last_prompt_tokens > 0 {
        out.push_str(&format!(
            "\nLast request (reported by the API): {} prompt tokens (estimate is {:.0}% of it; \
             the estimate counts 4 bytes per token)",
            last_prompt_tokens,
            total as f64 * 100.0 / last_prompt_tokens as f64
        ));
    }

    let costs = zen_agent::tool_bundles::per_tool_costs(tools);
    if !costs.is_empty() {
        out.push_str("\n\nTool schema costs (estimated tokens, most expensive first):");
        for (name, tok) in &costs {
            out.push_str(&format!("\n  {:<12} {}", name, tok));
        }
    }
    out
}

pub fn tool_help(name: &str) -> String {
    // The registry is the agent's own full tool set (see
    // `agent_run::documentable_tools`), so every tool the agent can run
    // is documented here, including the ones that need runtime state
    // (Skill, Task, delegate, LSP) and are never built by this path.
    let registry = crate::agent_run::documentable_tools();
    let found = registry.get(name).or_else(|| {
        let lower = name.to_ascii_lowercase();
        registry
            .names()
            .into_iter()
            .find(|n| n.to_ascii_lowercase() == lower)
            .and_then(|n| registry.get(&n))
    });
    match found {
        Some(tool) => {
            let def = tool.definition();
            let tokens = zen_agent::tool_bundles::estimate_schema_tokens(&def);
            let phase = zen_agent::tool_bundles::tool_phase(tool.name())
                .map(|p| p.as_str())
                .unwrap_or("unclassified");
            let docs = zen_tools::tool_long_docs(tool.name())
                .unwrap_or("No extended documentation available.");
            let mut out = format!(
                "{} (phase: {}, ~{} schema tokens)\n\n{}\n\n---\n\n{}",
                tool.name(),
                phase,
                tokens,
                tool.description(),
                docs
            );
            if let Some(note) = crate::agent_run::tool_runtime_note(tool.name()) {
                out.push_str(&format!("\n\nNote: {}", note));
            }
            out
        }
        None => format!(
            "Unknown tool '{}'. Available tools: {}",
            name,
            registry.names().join(", ")
        ),
    }
}

pub fn mcp_report(status: &[(String, String)]) -> String {
    if status.is_empty() {
        return "No MCP servers configured.".to_string();
    }
    status
        .iter()
        .map(|(n, st)| format!("{}: {}", n, st))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn cost_report(usage: &zen_core::UsageStats, model: &str, base_url: &str) -> String {
    let entry = crate::cost::CostEntry {
        model: model.to_string(),
        provider: crate::cost::provider_from_base_url(base_url),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        timestamp: String::new(),
    };
    let summary = crate::cost::summarize("current session", std::slice::from_ref(&entry));
    let mut s = crate::cost::render_text(&summary);

    if usage.input_tokens > 0 && usage.cache_read_input_tokens > 0 {
        s.push_str(&format!(
            "\nPrompt cache: {} tokens served from cache ({:.0}% of input) - ~90% cheaper on paid models",
            usage.cache_read_input_tokens,
            usage.cache_hit_rate() * 100.0
        ));
    }
    s
}

pub fn transcript_markdown(session_id: &str, messages: &[zen_core::ChatMessage]) -> String {
    let mut out = format!("# zen session {}\n", session_id);
    for m in messages {
        match m.role.as_str() {
            "user" => out.push_str(&format!("\n## User\n\n{}\n", m.text())),
            "assistant" => {
                out.push_str("\n## Assistant\n\n");
                let text = m.text();
                if !text.is_empty() {
                    out.push_str(&text);
                    out.push('\n');
                }
                for call in m.tool_calls.iter().flatten() {
                    out.push_str(&format!(
                        "\n**Tool call** `{}`: `{}`\n",
                        call.function.name, call.function.arguments
                    ));
                }
            }
            "tool" => {
                out.push_str(&format!(
                    "\n**Tool result** ({}):\n\n```\n{}\n```\n",
                    m.tool_call_id.as_deref().unwrap_or("?"),
                    m.text()
                ));
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug)]
pub struct CustomCommand {
    pub name: String,
    pub frontmatter: HashMap<String, Value>,
    pub body: String,
}

impl CustomCommand {
    pub fn parse(name: &str, raw: &str) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(raw);

        Ok(Self {
            name: name.to_string(),
            frontmatter,
            body: body.to_string(),
        })
    }

    pub fn expand(&self, args: &str) -> Result<String> {
        self.expand_in(args, Path::new("."))
    }

    pub fn expand_in(&self, args: &str, base: &Path) -> Result<String> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let mut body = self.body.clone();

        for i in 1..=9usize {
            let slot = if i <= parts.len() { parts[i - 1] } else { "" };
            body = body.replace(&format!("${}", i), slot);
        }

        body = body.replace("$ARGUMENTS", args);

        expand_file_refs(&body, base)
    }
}

fn split_frontmatter(raw: &str) -> (HashMap<String, Value>, &str) {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return (HashMap::new(), raw);
    }
    if let Some(end_idx) = find_closing_delim(trimmed) {
        let fm_text = &trimmed[4..end_idx];
        let body = trimmed[end_idx + 3..].trim_start();
        let fm = parse_yaml_frontmatter(fm_text);
        return (fm, body);
    }
    (HashMap::new(), raw)
}

fn find_closing_delim(s: &str) -> Option<usize> {
    let mut idx = 4;
    while idx < s.len() {
        let nl = s[idx..].find('\n')?;
        idx += nl + 1;
        let line_end = s[idx..].find('\n').map(|p| idx + p).unwrap_or(s.len());
        let line = s[idx..line_end].trim();
        if line == "---" {
            return Some(idx);
        }
        idx = if line_end == s.len() { s.len() } else { idx };
    }
    None
}

fn parse_yaml_frontmatter(text: &str) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            map.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
        } else if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
        }
    }
    map
}

fn expand_file_refs(body: &str, base: &Path) -> Result<String> {
    let mut out = String::new();
    for (i, line) in body.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rebuilt: Vec<String> = Vec::new();
        let leading_ws = line.len() - line.trim_start().len();
        let (indent, rest) = line.split_at(leading_ws);
        if rest.starts_with('@') && !rest.trim()[1..].contains(' ') {
            let path = rest.trim()[1..].trim();
            if let Some(content) = read_ref(path, base) {
                out.push_str(indent);
                out.push_str(&content);
                continue;
            }
        }
        for token in line.split_whitespace() {
            if let Some(path) = token.strip_prefix('@')
                && !path.is_empty()
                && let Some(content) = read_ref(path, base)
            {
                rebuilt.push(content);
                continue;
            }
            rebuilt.push(token.to_string());
        }
        out.push_str(&rebuilt.join(" "));
    }
    Ok(out)
}

fn read_ref(path: &str, base: &Path) -> Option<String> {
    // Issue #444: confine the reference to the project directory and
    // apply the gitignore rules. The old code fell back to the path as
    // given, so an absolute path or a `..` escape inlined any readable
    // file into the expanded command. Canonicalizing both sides also
    // stops symlinks from escaping the project; an absolute path that
    // stays inside the project still resolves.
    let candidate = base.join(path);
    let root = std::fs::canonicalize(base).ok()?;
    let canon = std::fs::canonicalize(&candidate).ok()?;
    if !canon.starts_with(&root) || !canon.is_file() {
        return None;
    }
    let rules = zen_core::gitignore::load_ignore_rules(&root);
    if zen_core::gitignore::is_ignored(&rules, &canon, false) {
        return None;
    }
    // TOCTOU (issue #444): the checks above read the path, but the file
    // could be replaced by a symlink to outside before the open below.
    // `open_file_inside_project` binds the containment check to the
    // descriptor that is actually read, so a swap in between is caught.
    let file = zen_core::fs::open_file_inside_project(&canon, &root)?;
    read_ref_file(file)
}

/// Read a file for `@ref` inlining, capped at the 512 KiB mention cap
/// (issue #444). Reads at most cap + 1 bytes, so a huge file never lands
/// fully in memory. The file arrives already opened and verified inside
/// the project by `read_ref`; this function only reads the descriptor.
fn read_ref_file(file: std::fs::File) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(crate::mentions::MAX_MENTION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if bytes.len() as u64 > crate::mentions::MAX_MENTION_FILE_BYTES {
        let keep = crate::mentions::MAX_MENTION_FILE_BYTES as usize;
        let mut end = keep.min(text.len());
        // A UTF-8 char is at most 4 bytes, so the previous char boundary
        // is at most 3 bytes back. Bounding the walk keeps the loop from
        // hanging forever if `end -= 1` is ever replaced by a no-op
        // (e.g. the `/=` mutant): the loop exits after 3 steps and the
        // test fails on the wrong output instead of timing out.
        let mut steps = 0;
        while end > 0 && !text.is_char_boundary(end) && steps < 3 {
            end -= 1;
            steps += 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated: file exceeds the 512 KiB mention cap]");
    }
    Some(text)
}

fn custom_commands_dirs(cwd: &Path, home: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![];
    let proj = cwd.join(".zen").join("commands");
    dirs.push(proj);
    if let Some(h) = home {
        dirs.push(h.join(".zen").join("commands"));
    }
    dirs
}

pub fn load_custom_commands(cwd: &Path, home: Option<&Path>) -> Vec<CustomCommand> {
    let dirs = custom_commands_dirs(cwd, home);
    let mut cmds = vec![];
    for dir in &dirs {
        if !dir.exists() {
            continue;
        }
        for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
            let path = e.path();
            if path.is_file()
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                let name = stem.replace('_', ":");
                if let Ok(content) = std::fs::read_to_string(path)
                    && let Ok(cmd) = CustomCommand::parse(&name, &content)
                {
                    cmds.push(cmd);
                }
            }
        }
    }

    dedup_keep_project_first(&mut cmds);
    cmds
}

fn dedup_keep_project_first(cmds: &mut Vec<CustomCommand>) {
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < cmds.len() {
        if seen.contains(&cmds[i].name) {
            cmds.remove(i);
        } else {
            seen.insert(cmds[i].name.clone());
            i += 1;
        }
    }
}

#[derive(Debug)]
pub enum SlashResult {
    Handled,

    Custom(CustomCommand),

    Unknown(String),
}

pub fn dispatch_builtin(cmd: &str, _arg: &str) -> SlashResult {
    let known: std::collections::HashSet<&str> = BUILTIN_COMMANDS.iter().copied().collect();
    if known.contains(&cmd) {
        SlashResult::Handled
    } else {
        SlashResult::Unknown(cmd.to_string())
    }
}

pub fn mcp_scope_path(scope: &str, cwd: &Path, home: Option<&Path>) -> Result<std::path::PathBuf> {
    let scope = if scope.is_empty() { "local" } else { scope };
    zen_mcp::scope_path(scope, cwd, home)
}

pub fn split_kv(entry: &str) -> Option<(String, String)> {
    let idx = entry.find([':', '='])?;
    let (k, v) = (entry[..idx].trim(), entry[idx + 1..].trim());
    (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
}

fn read_mcp_file(path: &Path) -> Value {
    let mut root = match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => {
            serde_json::from_str::<Value>(&t).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => Value::Object(Default::default()),
    };
    if !root.is_object() {
        root = Value::Object(Default::default());
    }
    if !root
        .get("mcpServers")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        // `root` was coerced to an object above, so this always succeeds.
        if let Some(obj) = root.as_object_mut() {
            obj.insert("mcpServers".to_string(), Value::Object(Default::default()));
        }
    }
    root
}

fn write_mcp_file(path: &Path, root: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(root)?))?;
    Ok(())
}

/// Flags that belong to `zen mcp add` itself (issue #275). With
/// `trailing_var_arg` on `McpSubcommand::Add::args`, clap puts every token
/// after `<NAME>` into the server command, so a zen flag written after the
/// name would silently land in the server's `args` array and the server
/// would be written to the wrong scope. Reject such tokens instead of
/// writing the wrong file. A literal `--` token ends the scan: everything
/// after it was explicitly separated from zen's flags, e.g.
/// `zen mcp add srv echo -- --scope project`.
fn reject_swallowed_mcp_flags(rest: &[String]) -> Result<()> {
    const ZEN_FLAGS: &[&str] = &["--scope", "--transport", "--env", "--header", "-e", "-s"];
    for tok in rest {
        if tok == "--" {
            break;
        }
        let looks_like_zen_flag = ZEN_FLAGS.contains(&tok.as_str())
            || tok.starts_with("--scope=")
            || tok.starts_with("--transport=")
            || tok.starts_with("--env=")
            || tok.starts_with("--header=");
        if looks_like_zen_flag {
            return Err(anyhow!(
                "flag-like token {:?} in server command; `mcp add` flags (--scope, --transport, -e, --env, --header, -s) must come before the server name, e.g. `zen mcp add --scope project srv echo hi`. If the token belongs to the server command, separate it with `--` after the command, e.g. `zen mcp add srv echo -- --scope project`",
                tok
            ));
        }
    }
    Ok(())
}

/// One-line summary of an existing server entry, for conflict errors.
fn describe_mcp_server(cfg: &Value) -> String {
    match cfg.get("type").and_then(|t| t.as_str()) {
        Some("http") => match cfg.get("url").and_then(|u| u.as_str()) {
            Some(url) => format!("url {url}"),
            None => "http server".to_string(),
        },
        Some("stdio") => {
            let cmd = cfg.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let args = cfg
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("command {cmd} {args}").trim_end().to_string()
        }
        _ => "server".to_string(),
    }
}

pub async fn mcp_command(
    cli: &crate::cli::Cli,
    cwd: &Path,
    home: Option<&Path>,
    args: &crate::cli::McpArgs,
) -> Result<()> {
    use crate::cli::McpSubcommand;
    match &args.command {
        McpSubcommand::List => {
            let mut scopes: Vec<(String, Vec<String>, String)> = Vec::new();
            for scope in ["user", "project", "local"] {
                let path = mcp_scope_path(scope, cwd, home)?;
                let names: Vec<String> = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| v.get("mcpServers").and_then(|s| s.as_object()).cloned())
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default();
                scopes.push((scope.to_string(), names, path.display().to_string()));
            }
            if cli.quiet {
                return Ok(());
            }
            if cli.json_output || cli.raw {
                let v = serde_json::json!(
                    scopes
                        .iter()
                        .map(|(scope, names, path)| {
                            serde_json::json!({"scope": scope, "servers": names, "path": path})
                        })
                        .collect::<Vec<_>>()
                );
                if cli.raw {
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                return Ok(());
            }
            for (scope, names, path) in &scopes {
                if names.is_empty() {
                    println!("{}: (none)  {}", scope, path);
                } else {
                    println!("{}: {}  {}", scope, names.join(", "), path);
                }
            }
            Ok(())
        }
        McpSubcommand::Add {
            transport,
            scope,
            name,
            args: rest,
            env,
            header,
            dry_run,
            force,
        } => {
            reject_swallowed_mcp_flags(rest)?;
            // Issue #281: --dry-run is a clap flag of this variant, so it
            // is only read before the server name. A --dry-run after the
            // name would land in the server's args (trailing_var_arg);
            // refuse it with the same guidance as #275 instead.
            for tok in rest {
                if tok == "--" {
                    break;
                }
                if tok == "--dry-run" || tok.starts_with("--dry-run=") {
                    return Err(anyhow!(
                        "flag-like token {:?} in server command; `mcp add` flags (--scope, --transport, -e, --env, --header, --dry-run) must come before the server name, e.g. `zen mcp add --dry-run --scope project srv echo hi`. If the token belongs to the server command, separate it with `--` after the command, e.g. `zen mcp add srv echo -- --dry-run`",
                        tok
                    ));
                }
            }
            if transport != "stdio" && transport != "http" {
                return Err(anyhow!(
                    "unsupported transport {:?} (expected stdio or http)",
                    transport
                ));
            }
            let path = mcp_scope_path(scope, cwd, home)?;
            let mut root = read_mcp_file(&path);
            let mut cfg: Value = if transport == "http" {
                let url = rest
                    .first()
                    .ok_or_else(|| anyhow!("http transport requires a URL"))?;
                serde_json::json!({"type": "http", "url": url})
            } else {
                if rest.is_empty() {
                    return Err(anyhow!("stdio transport requires a command"));
                }
                serde_json::json!({"type": "stdio", "command": rest[0], "args": rest[1..]})
            };
            if !env.is_empty() {
                let mut env_map = serde_json::Map::new();
                for kv in env {
                    match kv.split_once('=') {
                        Some((k, v)) if !k.is_empty() => {
                            env_map.insert(k.to_string(), Value::String(v.to_string()));
                        }
                        _ => return Err(anyhow!("invalid -e {:?} (expected KEY=VALUE)", kv)),
                    }
                }
                cfg["env"] = Value::Object(env_map);
            }
            if !header.is_empty() {
                let mut header_map = serde_json::Map::new();
                for kv in header {
                    let (k, v) = split_kv(kv).ok_or_else(|| {
                        anyhow!("invalid --header {:?} (expected \"Name: value\")", kv)
                    })?;
                    header_map.insert(k, Value::String(v));
                }
                cfg["headers"] = Value::Object(header_map);
            }
            if *dry_run {
                if !cli.quiet {
                    println!("would write {}", path.display());
                    println!("  + mcpServers.{name} = {}", serde_json::to_string(&cfg)?);
                    if let Some(old) = root["mcpServers"].get(name) {
                        println!("  replaces: {}", serde_json::to_string(old)?);
                    }
                    println!("(no change made; drop --dry-run to apply)");
                }
                return Ok(());
            }
            // Issue #285: a taken name must not be replaced silently.
            // Refuse with a conflict (exit 7, like `secret register`)
            // unless --force was given.
            let existing = root["mcpServers"].get(name).cloned();
            if let Some(old) = &existing
                && !force
            {
                return Err(crate::coded_error(
                    crate::exit_code::CONFLICT,
                    format!(
                        "MCP server {:?} already exists in {} ({}); pass --force to replace it",
                        name,
                        path.display(),
                        describe_mcp_server(old),
                    ),
                ));
            }
            root["mcpServers"][name] = cfg;
            write_mcp_file(&path, &root)?;
            if !cli.quiet {
                if existing.is_some() {
                    println!("replaced MCP server {:?} in {}", name, path.display());
                } else {
                    println!("added MCP server {:?} to {}", name, path.display());
                }
            }
            Ok(())
        }
        McpSubcommand::Get { scope, name } => {
            let scopes: Vec<&str> = if scope.is_empty() {
                vec!["local", "project", "user"]
            } else {
                vec![scope]
            };
            for sc in scopes {
                let path = mcp_scope_path(sc, cwd, home)?;
                let root = read_mcp_file(&path);
                if let Some(server) = root["mcpServers"].get(name) {
                    if !cli.quiet {
                        if cli.raw {
                            println!("{}", serde_json::to_string(server)?);
                        } else {
                            println!("{}", serde_json::to_string_pretty(server)?);
                        }
                    }
                    return Ok(());
                }
            }
            Err(anyhow!("no MCP server named {:?}", name))
        }
        McpSubcommand::AddJson {
            scope,
            name,
            json,
            force,
        } => {
            let path = mcp_scope_path(scope, cwd, home)?;
            let mut root = read_mcp_file(&path);
            let cfg: Value = serde_json::from_str(json)?;
            if !cfg.is_object() {
                return Err(anyhow!("server JSON must be an object"));
            }
            serde_json::from_value::<zen_mcp::McpServerConfig>(cfg.clone())
                .map_err(|e| anyhow!("invalid MCP server JSON: {}", e))?;
            // Issue #315: a taken name must not be replaced silently
            // (same as #285 for `mcp add`). Refuse with a conflict
            // (exit 7, like `secret register`) unless --force was given.
            let existing = root["mcpServers"].get(name).cloned();
            if let Some(old) = &existing
                && !force
            {
                return Err(crate::coded_error(
                    crate::exit_code::CONFLICT,
                    format!(
                        "MCP server {:?} already exists in {} ({}); pass --force to replace it",
                        name,
                        path.display(),
                        describe_mcp_server(old),
                    ),
                ));
            }
            root["mcpServers"][name] = cfg;
            write_mcp_file(&path, &root)?;
            if !cli.quiet {
                if existing.is_some() {
                    println!("replaced MCP server {:?} in {}", name, path.display());
                } else {
                    println!("added MCP server {:?} to {}", name, path.display());
                }
            }
            Ok(())
        }
        McpSubcommand::Remove {
            scope,
            name,
            dry_run,
        } => {
            let path = mcp_scope_path(scope, cwd, home)?;
            let mut root = read_mcp_file(&path);
            if *dry_run {
                match root["mcpServers"].get(name) {
                    Some(old) => {
                        if !cli.quiet {
                            println!("would remove MCP server {name:?} from {}", path.display());
                            println!("  was: {}", serde_json::to_string(old)?);
                            println!("(no change made; drop --dry-run to apply)");
                        }
                    }
                    None => {
                        eprintln!("no MCP server {name:?} in {}", path.display());
                    }
                }
                return Ok(());
            }
            let removed = root["mcpServers"]
                .as_object_mut()
                .map(|o| o.remove(name).is_some())
                .unwrap_or(false);
            if removed {
                write_mcp_file(&path, &root)?;
                if !cli.quiet {
                    println!("removed MCP server {:?} from {}", name, path.display());
                }
            } else {
                eprintln!("no MCP server {:?} in {}", name, path.display());
            }
            Ok(())
        }
    }
}

pub async fn sessions_command(
    cli: &crate::cli::Cli,
    cwd: &Path,
    home: Option<&Path>,
    args: &crate::cli::SessionsArgs,
) -> Result<()> {
    use crate::cli::SessionSubcommand;
    use crate::session_mgmt as sm;
    let Some(home) = home else {
        return Err(anyhow!("sessions need a home directory"));
    };
    let cfg = crate::settings::resolve_config_with_home(cli, cwd, Some(home))?;
    if cfg.auto_archive_days > 0 {
        for id in sm::auto_archive_old(home, cfg.auto_archive_days)? {
            if !cli.quiet && !cli.json_output && !cli.raw {
                println!(
                    "auto-archived session {} (older than {}d)",
                    sm::short_id(&id),
                    cfg.auto_archive_days
                );
            }
        }
    }
    match &args.command {
        SessionSubcommand::List { all } => {
            let sessions = sm::list_all_sessions(home, *all);
            if cli.json_output || cli.raw {
                if !cli.quiet {
                    let v = serde_json::json!(
                        sessions
                            .iter()
                            .map(|x| {
                                serde_json::json!({
                                    "id": x.id,
                                    "path": x.path.display().to_string(),
                                    "archived": x.archived,
                                    "modified": sm::fmt_mtime(x.modified),
                                    "messages": x.message_count,
                                    "input_tokens": x.input_tokens,
                                    "output_tokens": x.output_tokens,
                                    "title": x.title,
                                })
                            })
                            .collect::<Vec<_>>()
                    );
                    if cli.raw {
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    }
                }
                return Ok(());
            }
            if sessions.is_empty() {
                if !cli.quiet {
                    println!("no sessions yet");
                }
                return Ok(());
            }
            if cli.quiet {
                return Ok(());
            }
            println!(
                "{:<36} {:<16} {:>5} {:>15}  {:<9} TITLE",
                "ID", "MODIFIED (UTC)", "MSGS", "TOKENS in/out", "STATUS"
            );
            for s in &sessions {
                println!(
                    "{:<36} {:<16} {:>5} {:>15}  {:<9} {}",
                    s.id,
                    sm::fmt_mtime(s.modified),
                    s.message_count,
                    format!("{}/{}", s.input_tokens, s.output_tokens),
                    if s.archived { "archived" } else { "active" },
                    s.title.as_deref().unwrap_or(""),
                );
            }
            Ok(())
        }
        SessionSubcommand::Search { keyword } => {
            let hits = crate::recall::search_sessions(home, cwd, keyword);
            if cli.json_output || cli.raw {
                if !cli.quiet {
                    let v = serde_json::json!(
                        hits.iter()
                            .map(|h| {
                                serde_json::json!({
                                    "id": h.id,
                                    "modified": sm::fmt_mtime(h.modified),
                                    "title": h.title,
                                    "snippet": h.snippet,
                                })
                            })
                            .collect::<Vec<_>>()
                    );
                    if cli.raw {
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    }
                }
                return Ok(());
            }
            if hits.is_empty() {
                if !cli.quiet {
                    println!("no sessions match {:?}", keyword);
                }
                return Ok(());
            }
            if cli.quiet {
                return Ok(());
            }
            for h in &hits {
                println!(
                    "{}  {}  {}",
                    sm::short_id(&h.id),
                    sm::fmt_mtime(h.modified),
                    h.title.as_deref().unwrap_or("")
                );
                println!("    {}", h.snippet);
            }
            Ok(())
        }
        SessionSubcommand::Archive { id } => {
            let dest = sm::archive_session(home, id)?;
            if !cli.quiet {
                println!(
                    "archived session {} -> {}",
                    sm::short_id(id),
                    dest.display()
                );
            }
            Ok(())
        }
        SessionSubcommand::Delete { id, yes } => {
            if !yes && !sm::confirm_delete_prompt(id)? {
                if !cli.quiet {
                    println!("delete cancelled");
                }
                return Ok(());
            }
            sm::delete_session(home, id)?;
            if !cli.quiet {
                println!("deleted session {}", sm::short_id(id));
            }
            Ok(())
        }
        SessionSubcommand::Restore { id } => {
            let dest = sm::restore_session(home, id)?;
            if !cli.quiet {
                println!(
                    "restored session {} -> {}",
                    sm::short_id(id),
                    dest.display()
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn every_command_has_a_description() {
        let catalog = slash_command_catalog();
        let expected = BUILTIN_COMMANDS.len() + REPL_ONLY_COMMANDS.len() + 2;
        assert_eq!(catalog.len(), expected, "catalog: {catalog:?}");
        for c in BUILTIN_COMMANDS.iter().chain(REPL_ONLY_COMMANDS) {
            assert!(
                COMMAND_DESCRIPTIONS.iter().any(|(n, _)| n == c),
                "missing description for {c}"
            );
        }
    }

    #[test]
    fn tool_help_covers_every_documentable_tool() {
        // Issue #251: `zen tool-help` only knew the 12 tools it built
        // itself and rejected Skill, Task, delegate and the 4 LSP tools
        // with "Unknown tool". It now documents from the agent's own
        // full tool set, so every name there must resolve.
        let names = crate::agent_run::documentable_tools().names();
        assert_eq!(names.len(), 19, "documentable tools: {names:?}");
        for name in &names {
            let help = tool_help(name);
            assert!(
                !help.starts_with("Unknown tool"),
                "tool_help({name}) should resolve, got: {help}"
            );
            assert!(
                help.starts_with(&format!("{} (phase:", name)),
                "tool_help({name}) should start with its header, got: {help}"
            );
        }
        // The runtime-state tools carry the note; the plain ones do not.
        assert!(
            tool_help("Task").contains("Requires configured sub-agents"),
            "Task help must carry its runtime note"
        );
        assert!(
            tool_help("delegate").contains("Requires configured sub-agents"),
            "delegate help must carry its runtime note"
        );
        assert!(
            tool_help("goto_definition").contains("active LSP provider"),
            "LSP help must carry its runtime note"
        );
        assert!(
            !tool_help("Read").contains("Note:"),
            "Read needs no runtime note"
        );
    }

    fn sample_messages() -> Vec<zen_core::ChatMessage> {
        use zen_core::{ChatMessage, FunctionCall, ToolCall};
        vec![
            ChatMessage::user("please read the config file and summarise it"),
            ChatMessage::assistant(
                Some("reading it".into()),
                Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "Read".into(),
                        arguments: r#"{"file_path":"config.toml"}"#.into(),
                    },
                }]),
            ),
            ChatMessage::tool("c1", "x".repeat(400)),
            ChatMessage::assistant(Some("it sets three options".into()), None),
        ]
    }

    #[test]
    fn context_breakdown_groups_add_up_to_the_estimate() {
        let memory = "# Memory: ZEN.md\n\nalways run the tests before finishing";
        let prompt = format!("You are zen.\n\n{memory}\n\nDefinition of done: ...");
        let msgs = sample_messages();
        let b = context_breakdown(&prompt, memory, &[], &msgs);
        assert!(b.memory > 0 && b.instructions > 0);
        assert_eq!(
            b.instructions + b.memory,
            prompt.len() as u64 / 4,
            "instructions and memory split the system prompt exactly"
        );
        assert_eq!(b.history(), zen_core::estimate_tokens(&msgs));
        assert!(b.history_tools >= 100, "tool result is ~400 bytes: {b:?}");
        assert!(b.history_conversation > 0);
        assert_eq!(
            b.total(),
            prompt.len() as u64 / 4 + zen_core::estimate_tokens(&msgs)
        );
    }

    #[test]
    fn context_breakdown_ignores_memory_missing_from_the_prompt() {
        let b = context_breakdown("You are zen.", "# Memory: gone", &[], &[]);
        assert_eq!(b.memory, 0);
        assert_eq!(b.instructions, "You are zen.".len() as u64 / 4);
        let none = context_breakdown("You are zen.", "", &[], &[]);
        assert_eq!(none.memory, 0);
    }

    #[test]
    fn context_report_group_lines_sum_to_the_printed_total() {
        let memory = "# Memory: ZEN.md\n\nkeep diffs small and tested";
        let prompt = format!("You are zen.\n\n{memory}");
        let msgs = sample_messages();
        let report = context_report(&prompt, memory, &[], &msgs, 128_000, 0);
        let num = |label: &str| -> u64 {
            report
                .lines()
                .find(|l| l.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no '{label}' line in:\n{report}"))
                .split(':')
                .next_back()
                .unwrap()
                .trim()
                .trim_end_matches(" tokens")
                .parse()
                .unwrap()
        };
        let total: u64 = report
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("Context usage (estimated): ")
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let sum = num("- instructions")
            + num("- memory")
            + num("- tools")
            + num("- conversation")
            + num("- tool calls and results");
        assert_eq!(sum, total, "{report}");
        assert_eq!(
            num("- messages"),
            num("- conversation") + num("- tool calls and results")
        );
        assert!(report.contains("- memory: "), "{report}");
    }

    #[test]
    fn context_report_compares_with_the_api_reported_total() {
        let report = context_report("You are zen.", "", &[], &sample_messages(), 128_000, 500);
        assert!(
            report.contains("Last request (reported by the API): 500 prompt tokens (estimate is "),
            "{report}"
        );
    }

    #[test]
    fn context_breakdown_counts_images_and_tool_calls_exactly() {
        // Exact groups pin message_bytes: swapping `+` for `-` or `*`
        // for `+` in the image term changes the totals (or underflows).
        let msgs = vec![
            zen_core::ChatMessage::user_parts(vec![
                zen_core::ContentPart::text("abcdefgh"),
                zen_core::ContentPart::image_url("data:image/png;base64,AAAA"),
                zen_core::ContentPart::image("image/png", "AAAA"),
            ]),
            zen_core::ChatMessage::assistant(
                None,
                Some(vec![zen_core::ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: zen_core::FunctionCall {
                        name: "Read".into(),
                        arguments: "{}".into(),
                    },
                }]),
            ),
        ];
        // System prompt "abcd" is 1 token; no memory, no tools.
        let b = context_breakdown("abcd", "", &[], &msgs);
        assert_eq!(b.instructions, 1);
        assert_eq!(b.memory, 0);
        assert_eq!(b.tools, 0);
        // History bytes: (8 + 2 * 384 * 4) + (4 + 2) = 3086, / 4 = 771.
        // Tool bytes: 4 + 2 = 6, / 4 = 1.
        assert_eq!(b.history_tools, 1);
        assert_eq!(b.history_conversation, 770);
        assert_eq!(b.history(), 771);
        assert_eq!(b.total(), 772);
        // The groups add up to zen_core::estimate_tokens, as documented.
        assert_eq!(b.history(), zen_core::estimate_tokens(&msgs));
    }

    #[test]
    fn transcript_markdown_shows_unknown_tool_call_id() {
        // Pins the `unwrap_or("?")`: replacing "?" with "" must fail.
        let missing = zen_core::ChatMessage {
            role: "tool".into(),
            content: Some(zen_core::MessageContent::Text("result".into())),
            ..Default::default()
        };
        let md = transcript_markdown("s1", &[missing]);
        assert!(md.contains("**Tool result** (?):"), "transcript:\n{md}");
        let known = zen_core::ChatMessage::tool("call_9", "result");
        let md = transcript_markdown("s1", &[known]);
        assert!(
            md.contains("**Tool result** (call_9):"),
            "transcript:\n{md}"
        );
    }

    #[test]
    fn builtin_commands_list() {
        assert!(BUILTIN_COMMANDS.contains(&"/help"));
        assert!(BUILTIN_COMMANDS.contains(&"/exit"));
        assert!(BUILTIN_COMMANDS.contains(&"/compact"));
        assert!(BUILTIN_COMMANDS.contains(&"/diff"));
    }

    #[test]
    fn parse_builtin_diff_with_flags() {
        assert_eq!(
            parse_builtin("/diff --staged"),
            Some(("diff".to_string(), "--staged".to_string()))
        );
        assert_eq!(
            parse_builtin("/diff"),
            Some(("diff".to_string(), "".to_string()))
        );
        assert_eq!(
            parse_builtin("/diff --stat"),
            Some(("diff".to_string(), "--stat".to_string()))
        );
    }

    #[test]
    fn builtin_commands_list_includes_memories() {
        assert!(BUILTIN_COMMANDS.contains(&"/memories"));

        assert!(!HEADLESS_BUILTINS.contains(&"memories"));
    }

    #[test]
    fn builtin_commands_list_includes_recall() {
        assert!(BUILTIN_COMMANDS.contains(&"/recall"));

        assert!(!HEADLESS_BUILTINS.contains(&"recall"));
    }

    #[test]
    fn init_prompt_with_imports_lists_files() {
        let cands = vec![
            std::path::PathBuf::from("/proj/CLAUDE.md"),
            std::path::PathBuf::from("/proj/sub/AGENTS.md"),
        ];
        let p = init_prompt_with_imports(&cands);
        assert!(p.contains("/proj/CLAUDE.md"), "prompt: {}", p);
        assert!(p.contains("/proj/sub/AGENTS.md"), "prompt: {}", p);
        assert!(p.contains("incorporate"), "prompt: {}", p);

        assert!(p.contains("write a concise"), "prompt: {}", p);

        for c in ["/loop", "/goal", "/personality", "/vim", "/theme"] {
            assert!(BUILTIN_COMMANDS.contains(&c), "missing {c}");
            assert!(parse_builtin(c).is_some(), "not parsed: {c}");
        }
        assert_eq!(
            parse_builtin("/loop 3 do x"),
            Some(("loop".to_string(), "3 do x".to_string()))
        );
        assert!(BUILTIN_COMMANDS.contains(&"/deep-research"));
    }

    #[test]
    fn dispatch_builtin_known() {
        match dispatch_builtin("/help", "") {
            SlashResult::Handled => {}
            other => panic!("expected handled, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_builtin_unknown() {
        match dispatch_builtin("/customfoo", "") {
            SlashResult::Unknown(name) => assert_eq!(name, "/customfoo"),
            other => panic!("expected unknown, got {:?}", other),
        }
    }

    #[test]
    fn custom_command_positional_expansion() {
        let cmd = CustomCommand::parse(
            "review",
            "Review the file $1 and fix issues in $2.\n$ARGUMENTS done.",
        )
        .unwrap();
        let out = cmd.expand("src/main.rs").unwrap();
        assert!(out.contains("Review the file src/main.rs"));
        assert!(out.contains("$2.") || out.contains("."));
        assert!(out.contains("src/main.rs done"));
    }

    #[test]
    fn custom_command_arguments_expansion() {
        let cmd = CustomCommand::parse("echo", "you said: $ARGUMENTS").unwrap();
        let out = cmd.expand("hello world").unwrap();
        assert_eq!(out, "you said: hello world");
    }

    #[test]
    fn custom_command_atfile_inlining() {
        let tmp = std::env::temp_dir().join(format!("zen_cmd_atfile_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("data.txt"), "inline-content").unwrap();

        let cmd = CustomCommand::parse("show", "data: @data.txt").unwrap();
        let out = cmd.expand_in("", &tmp).unwrap();
        assert!(out.contains("data: inline-content"));
    }

    fn read_ref_tmp(tag: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "zen_cmd_readref_{}_{}_{}",
            tag,
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn read_ref_rejects_absolute_path_outside_project() {
        // Issue #444: an absolute path outside the project must not inline.
        let tmp = read_ref_tmp("abs");
        let secret = tmp.join("secret.txt");
        fs::write(&secret, "top-secret").unwrap();
        let proj = tmp.join("proj");
        fs::create_dir_all(&proj).unwrap();
        assert_eq!(read_ref(secret.to_str().unwrap(), &proj), None);
    }

    #[test]
    fn read_ref_rejects_dotdot_escape() {
        // Issue #444: `..` must not escape the project directory.
        let tmp = read_ref_tmp("dotdot");
        fs::write(tmp.join("secret.txt"), "top-secret").unwrap();
        let proj = tmp.join("proj");
        fs::create_dir_all(&proj).unwrap();
        assert_eq!(read_ref("../secret.txt", &proj), None);
    }

    #[test]
    fn read_ref_respects_gitignore() {
        // Issue #444: gitignored files must not inline.
        let tmp = read_ref_tmp("ignore");
        fs::write(tmp.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(tmp.join("ignored.txt"), "nope").unwrap();
        fs::write(tmp.join("kept.txt"), "yes").unwrap();
        assert_eq!(read_ref("ignored.txt", &tmp), None);
        assert_eq!(read_ref("kept.txt", &tmp).as_deref(), Some("yes"));
    }

    #[test]
    fn read_ref_caps_large_file() {
        // Issue #444: the inlined size is capped like mentions.
        let tmp = read_ref_tmp("cap");
        let big = "x".repeat(crate::mentions::MAX_MENTION_FILE_BYTES as usize + 1000);
        fs::write(tmp.join("big.txt"), &big).unwrap();
        let out = read_ref("big.txt", &tmp).expect("in-project file must inline");
        assert!(out.ends_with("[truncated: file exceeds the 512 KiB mention cap]"));
        assert!(out.len() < crate::mentions::MAX_MENTION_FILE_BYTES as usize + 100);
    }

    #[test]
    fn read_ref_file_exact_cap_is_not_truncated() {
        // Issue #444 (mutant-killer): `read_ref_file` truncates only when
        // the file is strictly larger than the cap. A file of exactly
        // MAX_MENTION_FILE_BYTES must inline whole, with no marker.
        // Kills commands.rs:609 `>` -> `>=`.
        let tmp = read_ref_tmp("exactcap");
        let cap = crate::mentions::MAX_MENTION_FILE_BYTES as usize;
        let content = "a".repeat(cap);
        fs::write(tmp.join("exact.txt"), &content).unwrap();
        let out = read_ref_file(std::fs::File::open(tmp.join("exact.txt")).unwrap())
            .expect("file must read");
        assert_eq!(out, content);
        assert!(!out.contains("[truncated"));
    }

    #[test]
    fn read_ref_file_truncates_at_char_boundary() {
        // Issue #444 (mutant-killer): when a multi-byte character straddles
        // the cap, truncation must back up to the previous char boundary,
        // never split the character and never panic.
        // Kills commands.rs:612 `>` -> `==`, `<`, delete-`!`;
        // commands.rs:613 `-=` -> `+=`, `/=` (the last one by timeout:
        // `/=` never moves `end`, so the loop never ends).
        let tmp = read_ref_tmp("boundary");
        let cap = crate::mentions::MAX_MENTION_FILE_BYTES as usize;
        let mut content = "a".repeat(cap - 1);
        content.push('\u{e9}'); // 2-byte char straddling the cap
        content.push_str(&"b".repeat(10));
        fs::write(tmp.join("straddle.txt"), &content).unwrap();
        let out = read_ref_file(std::fs::File::open(tmp.join("straddle.txt")).unwrap())
            .expect("file must read");
        let expected = format!(
            "{}\n[truncated: file exceeds the 512 KiB mention cap]",
            "a".repeat(cap - 1)
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn read_ref_file_over_cap_ascii_truncates_exactly() {
        // Issue #444 (mutant-killer): an over-cap ASCII file truncates to
        // exactly the cap plus the marker, nothing less.
        // Kills commands.rs:612 `&&` -> `||` and delete-`!` (both walk
        // `end` all the way to 0, returning an empty body).
        let tmp = read_ref_tmp("asciicap");
        let cap = crate::mentions::MAX_MENTION_FILE_BYTES as usize;
        let content = "a".repeat(cap + 1);
        fs::write(tmp.join("over.txt"), &content).unwrap();
        let out = read_ref_file(std::fs::File::open(tmp.join("over.txt")).unwrap())
            .expect("file must read");
        let expected = format!(
            "{}\n[truncated: file exceeds the 512 KiB mention cap]",
            "a".repeat(cap)
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn read_ref_allows_absolute_path_inside_project() {
        // Containment, not a blanket ban: an absolute path that stays
        // inside the project still inlines.
        let tmp = read_ref_tmp("inside");
        let p = tmp.join("data.txt");
        fs::write(&p, "inside-content").unwrap();
        assert_eq!(
            read_ref(p.to_str().unwrap(), &tmp).as_deref(),
            Some("inside-content")
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_ref_rejects_symlink_to_outside_file() {
        // Issue #444 (TOCTOU): a symlink inside the project pointing
        // to an outside file must not inline, and the decision must be
        // made on the descriptor actually read, not on the path spelling.
        let tmp = read_ref_tmp("symlink");
        let secret = tmp.join("secret.txt");
        fs::write(&secret, "top-secret").unwrap();
        let proj = tmp.join("proj");
        fs::create_dir_all(&proj).unwrap();
        std::os::unix::fs::symlink(&secret, proj.join("link.txt")).unwrap();
        assert_eq!(read_ref("link.txt", &proj), None);
    }

    #[cfg(unix)]
    #[test]
    fn read_ref_swapped_symlink_mid_read_never_leaks_outside() {
        // Issue #444 (TOCTOU): `read_ref` used to check the path and
        // open it in two steps, so a file swapped for a symlink to
        // outside in between leaked the outside file. A swapper thread
        // flips the victim between a regular in-project file and a
        // symlink to an outside secret while the reader loops; the
        // outside content must never come back. Fails on the pre-fix
        // code (the swap lands in the check-then-open window) and
        // passes with the fd-bound open.
        let tmp = read_ref_tmp("toctou");
        let secret = tmp.join("secret.txt");
        fs::write(&secret, "TOP-SECRET-444").unwrap();
        let proj = tmp.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let victim = proj.join("victim.txt");
        fs::write(&victim, "inside").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let stop = std::sync::Arc::clone(&stop);
            let victim = victim.clone();
            let secret = secret.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = fs::remove_file(&victim);
                    let _ = std::os::unix::fs::symlink(&secret, &victim);
                    let _ = fs::remove_file(&victim);
                    let _ = fs::write(&victim, "inside");
                }
            })
        };
        for _ in 0..300 {
            if let Some(content) = read_ref("victim.txt", &proj) {
                assert!(
                    !content.contains("TOP-SECRET-444"),
                    "TOCTOU leak: swapped symlink read the outside file"
                );
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = swapper.join();
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn custom_command_frontmatter_parse() {
        let raw = "---\ndescription: Run tests\nmodel: gpt-4\n---\nTest the repo.";
        let cmd = CustomCommand::parse("test", raw).unwrap();
        assert_eq!(cmd.body, "Test the repo.");
        assert_eq!(
            cmd.frontmatter.get("description"),
            Some(&Value::String("Run tests".to_string()))
        );
        assert_eq!(
            cmd.frontmatter.get("model"),
            Some(&Value::String("gpt-4".to_string()))
        );
    }

    #[test]
    fn issue_440_closing_delim_at_eof_without_trailing_newline() {
        let raw = "---\ndescription: Run tests\n---";
        let cmd = CustomCommand::parse("test", raw).unwrap();
        assert_eq!(cmd.body, "");
        assert_eq!(
            cmd.frontmatter.get("description"),
            Some(&Value::String("Run tests".to_string()))
        );
    }

    #[test]
    fn issue_440_closing_delim_at_eof_with_trailing_newline_only() {
        let raw = "---\ndescription: Run tests\n---\n";
        let cmd = CustomCommand::parse("test", raw).unwrap();
        assert_eq!(cmd.body, "");
        assert_eq!(
            cmd.frontmatter.get("description"),
            Some(&Value::String("Run tests".to_string()))
        );
    }

    #[test]
    fn issue_440_closing_delim_at_eof_crlf_without_trailing_newline() {
        let raw = "---\r\ndescription: Run tests\r\n---";
        let cmd = CustomCommand::parse("test", raw).unwrap();
        assert_eq!(cmd.body, "");
        assert_eq!(
            cmd.frontmatter.get("description"),
            Some(&Value::String("Run tests".to_string()))
        );
    }

    #[test]
    fn custom_command_no_frontmatter() {
        let cmd = CustomCommand::parse("simple", "Just do it.").unwrap();
        assert_eq!(cmd.body, "Just do it.");
        assert!(cmd.frontmatter.is_empty());
    }

    #[test]
    fn load_custom_commands_from_dir() {
        let home = std::env::temp_dir().join(format!("zen_cmd_load_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let cwd = std::env::temp_dir().join(format!("zen_cmd_cwd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(cwd.join(".zen").join("commands")).unwrap();
        fs::write(
            cwd.join(".zen").join("commands").join("test.md"),
            "---\ndescription: test cmd\n---\nRun: $1",
        )
        .unwrap();

        let cmds = load_custom_commands(&cwd, Some(&home));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "test");
        assert_eq!(cmds[0].body, "Run: $1");
    }

    #[test]
    fn cost_report_shows_cache_hit_rate_when_cached() {
        let u = zen_core::UsageStats {
            input_tokens: 1000,
            output_tokens: 50,
            cache_read_input_tokens: 800,
        };
        let r = cost_report(&u, "mock-model", "http://127.0.0.1:1");
        assert!(
            r.contains("Prompt cache: 800 tokens served from cache (80% of input)"),
            "{}",
            r
        );
    }

    #[test]
    fn cost_report_omits_cache_line_when_unsupported() {
        let u = zen_core::UsageStats {
            input_tokens: 100,
            output_tokens: 5,
            cache_read_input_tokens: 0,
        };
        let r = cost_report(&u, "mock-model", "http://127.0.0.1:1");
        assert!(!r.contains("Prompt cache"), "{}", r);
    }

    #[test]
    fn help_group_puts_recall_in_context() {
        assert_eq!(help_group("/recall"), "Context");
        assert_eq!(help_group("/clear"), "Context");
        assert_eq!(help_group("/help"), "Session & model");
        assert_eq!(help_group("/commit"), "Code & review");
        assert_eq!(help_group("/not-a-command"), "Tools");
    }

    #[test]
    fn mcp_add_rejects_swallowed_zen_flags() {
        // Issue #275: a zen flag after the server name must be rejected,
        // never silently swallowed into the server's args.
        let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--scope", "project"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--scope=project"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--transport=stdio"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--env=K=V"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--header=A: b"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "-s", "project"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--transport", "http"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "-e", "K=V"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--env", "K=V"])).is_err());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "hi", "--header", "A: b"])).is_err());
        // A server command's own flags stay untouched.
        assert!(reject_swallowed_mcp_flags(&t(&["npx", "-y", "pkg", "/tmp"])).is_ok());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "--foo", "bar"])).is_ok());
        // Everything after a literal `--` is the server's own command.
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "--", "--scope", "project"])).is_ok());
        assert!(reject_swallowed_mcp_flags(&t(&["echo", "--", "-e", "hi"])).is_ok());
        assert!(reject_swallowed_mcp_flags(&[]).is_ok());
    }
}
