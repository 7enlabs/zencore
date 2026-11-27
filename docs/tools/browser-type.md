# browser_type

Type text into an input, textarea, or contenteditable element.

## Parameters

- `selector` (string, required): CSS selector of an input, textarea, or contenteditable element.
- `text` (string, required): Text to type.
- `submit` (boolean, optional): Press Enter after typing, default false. Submitting a form needs a separate approval.
- `timeout_ms` (integer, optional): Timeout in ms, default 10000.

## Behavior

- Types the text into the element; never returns the typed text (only its length), and never logs it.
- Mutating tool (same group as `Write`/`Bash`): asks for approval in every permission mode.
- Typing into a password field is a login action and needs a separate BrowserLogin approval. See the browser agent spec, sections 4-5.

## Phase bundle

`write` (implementation).
