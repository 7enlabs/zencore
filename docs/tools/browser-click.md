# browser_click

Click one element in the browser tab.

## Parameters

- `selector` (string, required): CSS selector of exactly one element.
- `timeout_ms` (integer, optional): Timeout in ms, default 10000.

## Behavior

- Clicks the element matching the selector; errors when the selector matches zero or more than one element.
- Mutating tool (same group as `Write`/`Bash`): asks for approval in every permission mode.
- Login, submit, and destructive clicks need a separate approval for that specific action. See the browser agent spec, sections 4-5.

## Phase bundle

`write` (implementation).
