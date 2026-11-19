# ACP server

The Agent Client Protocol (ACP) v1 server lives in
`crates/zen-cli/src/acp.rs` and speaks JSON-RPC 2.0 over stdio. Run it
with `zen --acp`. This document describes what the server implements,
what it advertises, and what it deliberately does not do, so a client
author knows exactly what to expect.

Protocol version is **1 only**. `initialize` answers `protocolVersion`
1 for any integer the client asks for (1, 2, 0); a v1 client that
cannot speak the answered version disconnects. There is no `-32022`
here: v1 negotiation never errors.

## initialize

Request: `initialize` with `protocolVersion` (integer, required;
missing or non-integer is `-32602`), `clientCapabilities`,
`clientInfo`.

Response `agentCapabilities` (only what M1-M7 actually implement is
advertised; an omitted or false capability means unsupported):

| Capability | Value | Notes |
| --- | --- | --- |
| `loadSession` | `true` | session/load + resume (M5) |
| `promptCapabilities.image` | `false` | image blocks are rejected with `-32602` |
| `promptCapabilities.audio` | `false` | audio blocks are rejected with `-32602` |
| `promptCapabilities.embeddedContext` | `false` | |
| `mcpCapabilities.http` | `true` | per-session MCP over HTTP (M7) |
| `mcpCapabilities.sse` | `false` | no SSE transport in the rmcp features enabled |
| `sessionCapabilities.list` | `{}` | |
| `sessionCapabilities.resume` | `{}` | |
| `sessionCapabilities.close` | `{}` | |
| `sessionCapabilities.delete` | `{}` | |
| `auth` | `{}` | no authentication is required; `authMethods` is `[]` and `authenticate` answers `{}` |

`agentInfo` is `{ "name": "zencore", "title": "Zen", "version": <crate version> }`.

Client capabilities the server reads (all optional; absent means
unsupported):

- `clientCapabilities.session.notices` — a present, non-null object
  advertises session notices (AgentEvent module). System messages are
  then sent as `notice` updates; otherwise they fall back to agent
  message chunks.
- `clientCapabilities.session.compaction` — a present, non-null
  object advertises session compaction. Compaction boundaries are
  then sent as `compaction_update`; otherwise the boundary is silent.
- `clientCapabilities.fs.readTextFile` / `fs.writeTextFile` —
  booleans (M7). When true, the agent's file read/write tools run on
  the client instead of locally, with a local fallback.
- `clientCapabilities.terminal` — boolean (M7). When true, the
  agent's shell tool runs commands on the client, with a local
  fallback.

Modes are **not** advertised in `initialize`: ACP v1 puts them in the
`session/new` response (see below).

## Methods

| Method | Notes |
| --- | --- |
| `initialize` | once; a second call is `-32600` (`already initialized`) |
| `authenticate` | answers `{}`; no credentials exist |
| `session/new` | see below |
| `session/prompt` | see below |
| `session/cancel` | cancels the running turn of a session |
| `session/set_mode` | `default` / `plan` / `acceptEdits` / `askAll`; `bypassPermissions` and `dontAsk` are rejected |
| `session/set_config_option` | `model` only |
| `session/list` | pagination: 50 entries per page, `cursor` is an offset; `cwd` filter |
| `session/load` | replays a persisted session; unknown session id is `-32002` |
| `session/resume` | resumes a persisted session |
| `session/close` | idempotent |
| `session/delete` | removes the persisted session |
| `$/cancel_request` | cancels an in-flight request by id |
| `shutdown`, `exit` | lifecycle |

Unknown methods are `-32601`. Malformed params are `-32602`.
Internal failures are `-32603`. Unknown sessions are `-32002`.

### session/new

`cwd` must be an absolute path string; an absent `cwd` falls back to
the default working directory, but a present non-string `cwd` is
`-32602`. The response carries the session id, the `modes` array
(exactly `default` / `plan` / `acceptEdits` / `askAll`, current mode
first) and the `configOptions` array (`model`).

`mcpServers` (M7) is accepted here: a map of server name to a
`stdio` or `http` server config (an `http` config needs a `url`).
Malformed entries are `-32602`. See "MCP servers" below.

### session/prompt

`prompt` is an array of content blocks. `text` blocks run; image and
audio blocks are `-32602` (see `promptCapabilities` above). The
prompt is echoed back as one `user_message_chunk` per block, all
sharing the turn's `messageId`.

The response carries `stopReason`: `end_turn` (normal finish),
`cancelled` (client cancelled), `refusal` (the model refused),
`max_tokens` (the model stopped at its output limit). There is no
v1 error stopReason: an execution error fails the request instead.

### session/request_permission (agent to client)

The agent asks the client before a write the current mode does not
allow (M4/M6 permission model). The client answers with one of four
options (`allow_once`, `allow_always`, `reject_once`,
`reject_always`); an answer the agent does not understand is a bad
response, never an acceptance. The agent waits at most 30 s for an
answer; a client that never answers is treated as a failure, and the
operation is denied. The permission model is fail-closed throughout:
no answer, no capability, no permission.

## Session updates

Notifications are `session/update` with a `sessionUpdate` tag:

| Update | Notes |
| --- | --- |
| `agent_message_chunk` | assistant text, one chunk per message |
| `user_message_chunk` | echo of the client's prompt blocks |
| `tool_call` | a tool started; `title`, `kind`, `status` |
| `tool_call_update` | a tool finished or its status changed (`completed`, `failed`, `cancelled`) |
| `usage_update` | `used` / `size` token counts plus `cost` `{ amount, currency: "USD" }`; `cost` is omitted when the model has no known price, so a missing cost means "unknown", never a false $0 |
| `notice` | live-only information (never session history); only when the client advertised `session.notices` |
| `compaction_update` | one `completed` notification per compaction, with a summary when one exists; only when the client advertised `session.compaction` |

There is no `state_update` in ACP v1: `running` / `idle` /
`requires_action` is a v2-only concept, so a v1 server must not
emit it.

## Client tools (M7)

When the client advertises `fs.readTextFile`, the agent's file-read
tool (`Read`) calls the client's `fs/read_text_file` instead of
reading the local disk; when it advertises `fs.writeTextFile`, the
write tool (`Write`) calls the client's `fs/write_text_file`. When
it advertises `terminal`, the shell tool (`Bash`) calls the client's
`terminal/create`, `terminal/output`, `terminal/wait_for_exit`,
`terminal/kill`, `terminal/release`. Paths must be absolute on every
platform (Linux, macOS, Windows); a relative path is rejected.

Every client call is fail-closed: if the client errors, times out
(30 s), or the capability was not advertised, the tool falls back to
the local equivalent. The e2e tests prove the client path is really
used: a file that exists only on the client is read without touching
the local disk, and a write lands on the client only.

## MCP servers (M7)

`mcpServers` in `session/new` starts one MCP client per server,
over stdio or HTTP, with a 30 s start timeout. A server that fails
to start (for example an unreachable HTTP URL) is skipped with a
warning; the session still starts. A malformed server entry
(unknown transport, missing `url` for HTTP) is `-32602` and the
session is not created. The tools of started servers are registered
into the agent's tool registry before the first prompt, so they are
usable from the first turn. Servers live for the session; closing
the session stops them.

## Persistence

Sessions are persisted as journals (one file per session, format
from PR #210) and can be listed, loaded, resumed, closed and
deleted. A resumed session keeps its id. `session/list` returns 50
entries per page; pass the returned `cursor` to page further.

## Limits (deliberate, not bugs)

- No `agent_thought_chunk`: the model stream carries no reasoning
  content to forward, so nothing is invented.
- No `plan_update`: the agent has no plan to report.
- No `terminal_update`: not in the ACP v1 schema.
- No `available_commands_update`: the command set is static.
- No `config_option_update`: the agent never changes its own
  config, so there is nothing to report.
- No `session_info_update`: the agent has no session info to push.
- No `elicitation/create` caller: the wire layer was written in M7
  and then removed because it had no caller — there is no AskUser
  tool in the repo and zen-mcp has no elicitation handler. A future
  AskUser tool or an MCP elicitation handler redirected to the user
  is the follow-up that would reconnect it; until then the agent
  never asks the client to elicit anything.
- Compaction is reported only when it completes; a boundary that
  fires after a successful compaction has no `in_progress` state.

## Conformance

The ACP Test Compatibility Kit (acpx v0.22.0, profile
`acp-core-v1`) is the acceptance gate for this server: 100% of the
advertised methods must pass. The kit is run from
`/data/home/acpx` (tag v0.22.0) against a `zen --acp` binary built
from the branch under test. The e2e suite
`crates/zen-cli/tests/acp_server.rs` covers the round-trips
(initialize, session lifecycle, permissions, client fs/terminal,
MCP stdio/HTTP, usage, notices, compaction) on the CI matrix
(Linux, macOS, Windows).
