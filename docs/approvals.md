# Remote approval

A Claude Code permission prompt can be answered from the menubar overlay,
the web page, or the phone, as well as in the terminal. The hub owns the
flow; the clients only read `/api/state` and post two endpoints. Code:
`hub/src/hook.rs` (helper, installer), `hub/src/approvals.rs` (wire, rules,
reconciliation, socket), `hub/src/web/state.rs` (the state's part).

## The hook

`claudeship hub install-hook` adds one entry to `~/.claude/settings.json`
(`$CLAUDE_CONFIG_DIR/settings.json` when set):

```json
{"hooks": {"PermissionRequest": [{"hooks": [
  {"type": "command", "command": "/Users/me/.local/bin/claudeship permission-hook", "timeout": 86400}
]}]}}
```

The merge is surgical and idempotent: ours is the entry whose command is a
program named `claudeship…` (any path, quoted or not; the file name decides,
case-insensitively) followed by the word `permission-hook` and nothing else
— `/x/audit permission-hook` is someone else's. One pointing at another path
is repointed, a duplicate dropped, and the Swift helper's entries (a
`ClaudeShip` program followed by `--permission-hook`, by the same rule)
removed in the same pass. Every other key and hook is kept, but the file is
re-serialised (pretty-printed, keys sorted), so its formatting is not. It is
written beside the file (owner-only until it takes the original's mode) and
renamed over it; a symlinked `settings.json` stays a symlink, the file it
points at is what gets replaced. A file that isn't a JSON object is left alone (exit 1).
`uninstall-hook` removes ours and the Swift one, then any containers that
emptied. The timeout is a leak backstop, not UX.

`claudeship permission-hook` reads the hook's stdin, sends one request line
to `<hub home>/approvals.sock`, and blocks, with no deadline, for one verdict
line. It prints a decision **only** on an explicit verdict (and a panic
is one more failure: its hook exits 0, the message on stderr):

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied via ClaudeShip"}}}
```

No hub, bad input, a hang-up, a malformed reply: exit 0, nothing on stdout,
which Claude Code reads as "no decision" (the terminal prompt decides). It
never starts a hub, so approvals exist only while one runs — the login
service (`hub install-service`) keeps one running.

## The socket

`approvals.sock` in the hub's home, 0600 (bound under a 0177 umask). One
connection is one pending request. Lines are JSON, at most 1 MB:

- helper → hub: `{"toolName", "sessionId"?, "cwd"?, "toolInput"?}` (Claude
  Code's `tool_name`, `session_id`, `cwd`, `tool_input`)
- hub → helper: `{"behavior":"allow"|"deny"}`, then the hub closes.

The hub closes without a verdict (`cancel`) when the prompt was answered in
the terminal or the session is gone. When the helper goes away (Claude Code
killed it) the hub forgets the request at once.

The hub keeps each request as `{id (uuid), sessionId, cwd, tool, summary,
detail, receivedAt}`. `summary` is the tool and its most telling input field
(`command`, `file_path`, `url`, `pattern`, `prompt`, `description`, else
compact JSON), newlines collapsed, 200 characters; `detail` the same
uncollapsed, 4000.

**Reconciliation**, after every `/api/state` build (and every 2 s by itself
while anything is pending or any rule stands, so it happens with no screen
open): a request
whose session's registry status is no longer `waiting` with a
`statusUpdatedAt` newer than the request was answered in the terminal, and
is cancelled; one whose session isn't in the registry for 10 s is cancelled.
Every new, answered, or dropped request invalidates the one-second state
cache.

**Standing rules** ("approve all"), per Claude session id, in memory only
(a standing approval doesn't outlive the hub that granted it): `5m` (until
now + 300 s) or `session`. A request whose session has a live rule is
answered allow on arrival. Rules go when they expire or their session
leaves the registry — noticed by the ticker even with no screen open, or a
`claude --resume` of the conversation (same session id) would inherit a
`session` rule. `sessionId` must be a UUID.

## API (protocol 3)

`GET /api/state`: top level `approvalsSupported: true`; every session entry
has `approvals: [{id, tool, summary, detail, receivedAt (ms)}]` (oldest
first), `autoApprove: null | {until: ms} | {session: true}`, and
`sessionId` when Claude has registered it. A session with a request reads
`status: "waiting"`.

| | Body | Answer |
|---|---|---|
| `POST /api/approve` | `{id, allow: bool}` | `{ok: true}`; 404 `{error: "no such approval"}`; 400 without both |
| `POST /api/auto-approve` | `{sessionId, rule: "5m" \| "session" \| "off"}` | `{ok: true}`; 400 otherwise |

Setting a rule also answers (allow) everything that session has pending.
Both go through the same gate as the other POSTs: paired, same-origin,
JSON.
