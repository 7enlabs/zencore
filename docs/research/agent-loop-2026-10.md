# Agent Loop Research - Preparing Zen Phase 1

**Date:** 2026-10-20
**Author:** ZC Developer (team CLI)
**Scope:** How three reference agents implement the agent loop
(tool calling, permissions, compaction) - from public documentation only.
**Clean-room note:** All findings below come from official docs, engineering blogs,
and public changelogs. No leaked source code, decompiled binaries, or unofficial
reproductions were used.

---

## 1. Comparison of the three tools

### 1.1 Architecture overview

| | Reference agent A | Reference agent B | Reference agent C |
|---|---|---|---|
| Language / license | Closed-source CLI | Rust, Apache-2.0 (public repo) | Open source (TUI + agent) |
| Loop shape | Single-threaded master loop (ReAct) | Turn-based loop over the reference agent's core | Layered: TUI → agent loop → provider abstraction |
| Core abstraction | Turn = prompt → tools → results → repeat until no tool calls | Items / Turns / Threads (durable, forkable, resumable) | Session → agent loop bounded by per-agent `steps` cap |
| Headless mode | `--print` / SDK | the reference agent's exec command (progress on stderr, answer on stdout) | the reference agent's run command |

### 1.2 Tool calling

**Reference agent A.** The canonical ReAct loop: (1) model receives system prompt, tool
definitions, history; (2) model emits text and/or tool calls; (3) harness executes
tools, feeds `tool_result` blocks back; (4) repeat - one full cycle is one turn;
(5) stop when a response contains no tool calls. Parallel tool calls are natively
supported - independent calls are batched in one turn. Sub-agent delegation is
itself a tool call (`Agent` tool, fork-exec): the child runs an isolated loop
with its own context, system prompt, tool subset, and permissions; the parent
receives only a summary. Built-ins: Explore (read-only), Plan, general-purpose.
Subagents are Markdown + YAML frontmatter (the reference agent's agents directory), with fields like
`description`, `tools`, `model`, `permissionMode`, `maxTurns`, `isolation`
(`worktree` gives an isolated repo copy). No documented hard turn cap on the main
loop; subagents expose `maxTurns`.

**Reference agent B.** Turn-based over the Responses API: each turn sends accumulated
input, executes returned tool calls (shell, file read/edit, web search, MCP via
the reference agent's MCP command), feeds results back. Sub-agent workflows are on by default with
three built-ins (`default`, `worker`, `explorer`); custom agents in
`config.toml` (`[agents.<name>]`). Defaults cap at 6 concurrent agent threads
with 1 nesting level; git worktrees (the reference agent's `--worktree` flag) isolate
parallel agents at the filesystem level.

**Reference agent C.** Standard agentic loop (prompt + history → stream → execute tools →
feed back) bounded by a per-agent `steps` cap: on exhaustion the agent gets a
system prompt forcing it to summarize work and list remaining tasks. A
`doom_loop` permission key triggers recovery when the agent repeats the same
tool call (three identical calls). Two primary agents (Build: full tools; Plan:
asks before edits/shell) plus subagents (General, Explore, Scout) invoked
automatically or via @-mention; each runs in a child session. Parallelism:
native parallel tool calls within a turn, plus the General subagent for parallel
work units. After every file-modifying turn, a git snapshot is taken as a
revert safety net.

### 1.3 Permissions

**Reference agent A** - layered system:
- **Modes:** `default` (prompt on first use), `acceptEdits` (auto-accept edits),
  `plan` (read-only), `auto` (classifier reviews shell/network), `dontAsk`
  (auto-deny; CI), `bypassPermissions` (skip prompts; still guards `.git`,
  the reference agent's config dir, etc.). Switchable mid-session (`/permissions`, Shift+Tab).
- **Rules:** `allow`/`ask`/`deny` lists with `Tool(specifier)` + wildcards
  (e.g. `Bash(npm run *)`). Evaluation order **deny → ask → allow, first match
  wins**; a bare deny removes the tool from the model's context entirely.
- **Settings hierarchy:** managed settings → user →
  project settings (committed) → project-local (gitignored).
- **Hooks (deterministic layer):** 31+ events; `PreToolUse` with exit code 2
  blocks a call, `PostToolUse` exit 2 surfaces stderr to the model for
  self-correction. Community mantra: "the instruction file is advisory, hooks are
  mandatory."

**Reference agent B** - two independent knobs:
- **`sandbox_mode`** (what the agent can reach): `read-only` | `workspace-write`
  (network off by default) | `danger-full-access` (disposable envs only).
  OS-level enforcement: macOS Seatbelt, Linux Landlock + seccomp-BPF.
- **`approval_policy`** (when it must ask): `on-request` (default) | `never` |
  `untrusted`. Flags `--sandbox`/`-s`, `--ask-for-approval`/`-a`;
  `--full-auto` and `--yolo` convenience combos.
- Granularity: `writable_roots`, `[[rules]]` command-prefix allowlists,
  protected paths (`.git/` and the reference agent's config dir stay read-only).

**Reference agent C** - most distinctive: every tool resolves to `allow` / `ask` / `deny`
via ordered pattern rules in its config file (`permission` section), set
globally (`*`), per tool, or per agent (agent overrides global). **Last matching
rule wins** (put catch-all first). Input-level granularity: bash `ask` by
default, `git *` allowed, `rm *` denied; edit denied except a docs glob.
`--auto` flag auto-approves anything not denied (for unattended runs).
Permission keys gate tool families: `read`, `edit`, `bash`, `task` (subagents),
`external_directory`, `webfetch`, `skill`, `doom_loop`, etc. Caution: schema is
migrating from object to ordered `permissions` array (`bash` → `shell`,
`task` → `subagent`) - design against an abstraction, not either spelling.

### 1.4 Compaction / context management

**Reference agent A.** Auto-compaction near a configurable window (default per model;
third-party: ~95%). Precedence: the reference agent's auto-compact window env var →
`--autocompact` flag → `/autocompact` command → `autoCompactWindow` in settings.
Mechanism: older history replaced by an LLM-generated summary
(`compact_boundary` in stream); durable layers re-injected afterwards -
project instruction file (reloaded *every* request), auto-memory, recently touched
files re-read. `/compact` accepts inline focus (`/compact focus on the auth
bug fix`); `PreCompact` hook can archive or block. Related: `/clear`, `/rewind`,
`/context` (live usage breakdown). Subagents compact on independent windows.

**Reference agent B.** `/compact` manual; auto when usage exceeds
`model_auto_compact_token_limit` (third-party: ~95%) or on
`ContextWindowExceeded`. A "CONTEXT CHECKPOINT COMPACTION" prompt requests a
handoff summary (progress, decisions, constraints, next steps); the summary -
prefixed "Another language model started to solve this problem…" - replaces
history alongside initial context + recent messages (up to ~20k tokens).
`/status` shows usage headroom; <10% warning nudges `/compact`.

**Reference agent C.** `compaction` key: `auto` (default true), `prune` (strip old tool
outputs without summarizing; default false), `reserved` (token buffer against
overflow). Manual `/compact` (alias `/summarize`). A hidden `compact` agent
(not UI-selectable) generates the summary; durable checkpoints with retry -
older history serialized, summary generated, event persisted, request retried
from the new checkpoint; raw rows stay in SQLite for audit but leave the
model-visible window. Plugin hook `experimental.session.compacting` lets
authors inject domain context into the summary prompt or replace it.

---

## 2. Design proposal for Zen

Zen already has per-turn checkpoints (`/undo`, `/diff --turn`) and a
`zen-agent` crate. Phase 1 should add a production-grade agent loop reusing
proven mechanics, adapted to Zen's constraints (free-first, any
OpenAI-compatible endpoint, clean-room implementation).

### 2.1 Loop core (borrow from reference agents A and C)

- **Single-threaded ReAct loop** in `zen-agent`: one turn = build prompt →
  model call → execute tool calls → feed results back; terminate on a
  tool-call-free response. Keep it provider-agnostic (OpenAI-compatible
  `/chat/completions`).
- **Per-agent step cap** (reference agent C's `steps`): config `agent.max_steps`
  (default e.g. 50); on exhaustion, inject a summarization prompt forcing the
  model to report progress + remaining tasks instead of looping forever.
- **Stuck detection** (reference agent C's `doom_loop`): track (tool, input-hash) pairs;
  on N identical consecutive calls, inject a recovery nudge ("you seem stuck;
  try a different approach or stop").
- **Native parallel tool calls** within a turn for independent calls (read-only
  batching), matching all three reference agents.

### 2.2 Permissions (borrow from reference agent A's layers + reference agent C's granularity)

- **Two knobs like reference agent B** (simple to explain): `sandbox` (what the agent can
  touch: `read-only` | `workspace` | `off`) × `approval` (when to ask:
  `always` | `on-edit` | `never`). Keep the matrix small - free-first users
  won't read a 31-event hook reference on day one.
- **Ordered allow/ask/deny rules** (reference agent C) with **deny-first evaluation**
  (reference agent A): patterns like `Bash(cargo test *)`, evaluated deny → ask →
  allow, first match wins. Store in `~/.zen/settings.json` with
  project (`.zen/settings.json`, committed) → project-local (gitignored)
  precedence.
- **Deterministic guard layer** (reference agent A's hooks, minimal v1): a
  `pre_tool_use` hook table evaluated before execution - block on match
  (e.g. never `rm -rf /`, never exfiltrate `~/.ssh`). Advisory prompt text is
  not enforcement.
- **Sub-agent permissions**: child loops inherit a *narrower* policy than the
  parent by default (read-only Explore-style), escalating only via explicit
  config - keeps exploration noise out of the main window *and* the blast
  radius small.

### 2.3 Compaction (borrow from all three)

- **Auto-compaction** at a configurable fraction of the model's context window
  (default 90%; env `ZEN_AUTO_COMPACT_FRACTION` overrides). Reuse the existing
  per-turn checkpoint store as the durable layer: compacted-away turns stay
  restorable via `/undo`-style history.
- **Summary agent pattern** (reference agents C and B): a dedicated summarization prompt
  producing (progress, decisions, constraints, next steps); the summary
  replaces old history while the tail (last ~N turns) is preserved verbatim.
- **Durable re-injection** (reference agent A): `ZEN.md` memory + project config are
  re-loaded every turn, so they never depend on surviving compaction.
  `/compact [focus]` manual command with optional focus text.
- **Prune mode** (reference agent C): optional config to strip verbose tool outputs
  (e.g. long `cargo test` logs) from old turns without a full summarization
  pass - cheaper than compaction for noisy-but-unimportant history.

### 2.4 What NOT to copy

- Don't copy any tool's exact config schema (reference agent C's permission schema is
  churning; reference agent A's 31 hooks are overkill for v1).
- Don't implement OS-level sandboxing (Seatbelt/Landlock) in Phase 1 - do
  process-level guards (cwd jail, command denylist, no-network flag) first;
  OS sandboxes are Phase 2+.
- Keep the free-first constraint front and center: every loop feature must
  work against any OpenAI-compatible endpoint, not just one provider's API.

---

## 3. Phase 1 task list (proposed)

1. **Loop core** - implement turn loop with step cap + stuck detection in
   `zen-agent` (`agent.max_steps`, doom-loop nudge). Acceptance: a runaway
   prompt terminates with a summary instead of looping; unit test with a mock
   provider.
2. **Parallel tool calls** - batch independent read-only calls in one turn.
   Acceptance: 3 parallel reads complete in one model round-trip in tests.
3. **Permission model v1** - `sandbox` × `approval` knobs + ordered
   deny→ask→allow rules in settings (`~/.zen`, `.zen/`, `.zen.local`).
   Acceptance: `Bash(rm -rf *)` denied even when a broader allow exists.
4. **Deterministic guards** - `pre_tool_use` blocklist (destructive commands,
   secret paths). Acceptance: blocked call never reaches the tool; user sees
   why.
5. **Sub-agents v1** - spawn child loop with narrowed permissions + own
   context; parent receives summary only. Acceptance: Explore-style read-only
   subagent completes a codebase question without touching the main history.
6. **Auto-compaction** - threshold-triggered summarization with tail
   preservation + `ZEN.md` re-injection; `/compact [focus]` command.
   Acceptance: a long session compacts without losing the active task; config
   survives compaction.
7. **Prune mode** - strip old verbose tool outputs (opt-in). Acceptance:
   token usage drops on noisy sessions with no summary pass.
8. **Contract tests** - extend the stream-json contract suite with loop
   lifecycle events (turn start/end, compaction boundary) so the Desktop team
   can observe loop state. Acceptance: `cargo test -p zen-cli` green.

*Ordering note: 1→4 are the safety-critical spine (loop can't run unattended
without them); 5→7 are capability depth; 8 keeps the CLI↔Desktop contract in
sync throughout.*

---

## Sources

- Reference agent documentation (public, studied clean-room): agent loop
  ("How the agent loop works"), permissions, sub-agents, hooks reference,
  model config, commands.
- Second reference agent documentation (public, studied clean-room);
  second reference agent engineering blog, "Unlocking the harness"
  (Feb 2026); the reference agent's public repo (its compaction module,
  templates).
- Third reference agent documentation (public, studied clean-room): agents,
  permissions, config, plugins, sessions; third-party technical analyses of
  the open-source implementation.
- Vercel AI Gateway changelog (MiniMax M3.1 Flash free tier - context only).

*All mechanics described from public documentation. Where a claim rests on
third-party analysis rather than official prose, it is marked inline above.*
