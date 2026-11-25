# Zen Artifacts Spec (Shared)

**Version:** 0 (draft)
**Status:** draft — pending written approval from ZC Leader (acceptance: spec covers all 6 sections below).
**Scope:** spec only, no code. The `zen-artifact` crate and Desktop UI land in stage 1, after the C1/C2 sweep merges.
**Consumers:** Zen Desktop and Sen Desktop (both render artifacts from the same `zencore` core).

Artifacts are versioned, renderable documents produced by the agent inside a
chat turn: code, Markdown, SVG, Mermaid diagrams, and (from wave 2) HTML.
The model emits them inline in its reply; the core parses, stores, and
versions them; the Desktop shows them in a side panel next to the chat.

## 1. Marker format

The model emits exactly one marker element per artifact, inline in the turn text:

```xml
<zenArtifact identifier="kebab-case-id" type="<mime>" language="…" title="…">…</zenArtifact>
```

- `identifier`: kebab-case, unique per artifact within a session. It is the
  stable identity of the artifact across turns.
- `type`: the MIME type from §2. It selects the renderer and the file
  extension on disk (§5).
- `language`: required for `application/vnd.zen.code` (e.g. `rust`,
  `typescript`); ignored for other types.
- `title`: human-readable title shown in the panel; may be empty.
- The element body is the full artifact content. **Truncation is forbidden:**
  the model must emit the complete content; a marker whose body was cut short
  is rejected by the parser (see §4 `parse_turn`).

**Versioning rule:** emitting a marker with an `identifier` that already
exists in the session, with different content, creates a **new version** of
that artifact — it never overwrites in place. Re-emitting identical content
is a no-op (no new version). Versions are numbered `v1`, `v2`, … in emission
order.

## 2. MIME type table (v1)

| `type` | Renders as | `language` | Notes |
|---|---|---|---|
| `text/markdown` | Rendered Markdown | — | Default when the model omits `type`. |
| `application/vnd.zen.code` | Syntax-highlighted code | required | `language` picks the grammar; unknown language falls back to plain text. |
| `image/svg+xml` | SVG image | — | Static SVG only; no `<script>`, no external refs (§3). |
| `application/vnd.zen.mermaid` | Mermaid diagram | — | Rendered client-side from the diagram source. |
| `text/html` | Sandboxed HTML page | — | **Wave 2 only.** Not implemented in v1. |

**Out of scope for v1 (do not build):** React compilation, Docs / Slides /
Design templates, AI features inside an artifact, public share links.

## 3. Sandbox and CSP policy

Artifacts render in an `<iframe sandbox="allow-scripts" srcdoc="…">`. The
sandbox token list is exactly `allow-scripts` — **`allow-same-origin` is
forbidden**. The frame therefore has an opaque origin: it cannot reach the
parent page, local storage, or any Tauri IPC channel.

Content-Security-Policy applied to every rendered artifact:

```http
Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:
```

- `default-src 'none'`: no network by default; the artifact cannot fetch,
  navigate, or open sockets.
- `script-src 'unsafe-inline'` / `style-src 'unsafe-inline'`: inline
  scripts and styles are allowed (needed for Mermaid and `text/html`); no
  external scripts or stylesheets.
- `img-src data:` / `font-src data:`: images and fonts only as embedded
  `data:` URIs.
- No network access of any kind. The frame has no access to Tauri IPC, so a
  malicious or buggy artifact cannot invoke app commands or read the
  filesystem.

SVG artifacts are additionally sanitized before render: `<script>` elements,
event-handler attributes, and external references (`href` / `xlink:href` not
starting with `#` or `data:`) are stripped.

## 4. `zen-artifact` crate API

A new crate `crates/zen-artifact` owns parsing, storage, and versioning. The
Desktop and CLI link it; the model-facing contract is the marker in §1.

| Function | Signature (sketch) | Behaviour |
|---|---|---|
| `parse_turn` | `parse_turn(text: &str) -> Vec<ParsedArtifact>` | Extracts every well-formed `<zenArtifact>` marker from one turn. Rejects truncated bodies and unknown `type` values; each rejection is reported with the marker's `identifier` (or byte offset when the identifier is missing). |
| `store` | `store(session_id, parsed) -> Version` | Persists a parsed artifact under §5. Same identifier + changed content → next version number. Identical content → returns the existing version, no write. |
| `versions` | `versions(session_id, identifier) -> Vec<VersionMeta>` | Lists all versions of one artifact, newest last, with `created_at` and byte size. |
| `get` | `get(session_id, identifier, version) -> Artifact` | Reads one version's content and metadata. |
| `csp_meta` | `csp_meta(type) -> CspMeta` | Returns the CSP header and the sandbox attribute string for a MIME type (§3). The renderer must use exactly what this returns. |
| `delete_session` | `delete_session(session_id)` | Removes the whole `artifacts/<session-id>/` tree and its index (§5). Called when a session is deleted. |

Error handling: storage failures and malformed markers are returned as
errors, never panicked on; the chat turn itself still renders (without the
failed artifact) and the failure is logged.

## 5. Storage layout and index

On disk, under the app data directory:

```text
artifacts/
  <session-id>/
    <identifier>/
      v1.<ext>
      v2.<ext>
      …
    index.json
```

- `<ext>` is derived from `type`: `md`, `code.<language>` (e.g. `rs`,
  `ts` — the raw language name when no canonical extension exists),
  `svg`, `mmd`, `html`.
- `index.json` sits next to the artifact directories of one session and
  records, per artifact: `identifier`, `type`, `language`, `title`,
  `latest_version`, and per version `created_at` + `bytes`. It is the only
  read path the UI uses to list artifacts; version files are read by `get`
  (§4).
- **Session deletion deletes artifacts:** deleting a session removes the
  whole `artifacts/<session-id>/` directory, including `index.json`. There
  is no separate artifact retention.
- Paths are built from the session id and the validated kebab-case
  identifier only — no user-supplied path segment is ever trusted as a
  filesystem path.

## 6. Minimum UI behaviour

The Desktop shows artifacts in a **panel beside the chat** (shared by both
Desktops; exact placement is a Desktop concern, the behaviours below are
not):

- **Preview / Code toggle:** Preview renders the artifact (§2, §3); Code
  shows the raw source with the language from the marker.
- **Versions + restore:** the panel lists all versions (§4 `versions`) and
  lets the user view any of them. **Restore creates a new version** — it
  never rewrites history; restoring `v2` while `v3` is latest writes `v4`
  with `v2`'s content.
- **Copy + download:** copy the current version's content to the clipboard,
  and download it as a file named `<identifier>-v<n>.<ext>`.
- **Edit by follow-up:** the user edits an artifact by sending a follow-up
  message in the same session ("make the heading blue", "fix the typo");
  the model re-emits the marker with the same `identifier`, which creates
  the next version (§1). There is no in-panel editor in v1.

## Acceptance

This spec is accepted when ZC Leader approves it in writing. Acceptance
requires all 6 sections above to be present and consistent with the design
decisions they record; code in stage 1 must implement this spec as written,
and any deviation needs a spec amendment first.
