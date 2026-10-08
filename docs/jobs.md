# Jobs

A job is a command a hub runs on pipes — not a pty, not a session to look
at — for another machine's Claude (or a script) to start, wait for, read,
and follow up later. The usual one is a headless `claude -p` whose
stream-json output is machine-readable and whose `session_id` the next job
`--resume`s, so the remote side keeps its context between jobs. Plan phase
11 (`rust-core-plan.md`); the engine is `hub/src/jobs.rs`, the routes
`hub/src/web/jobs.rs`, the command `hub/src/cli/jobs_cmd.rs`, the MCP
server `hub/src/mcp.rs`.

## Trust, stated plainly

**An `argv` job is remote command execution** on the hub that runs it,
authorised by the swarm secret (a peer relaying a request) or the hub's
pairing token (a paired browser, phone, `claudeship`, or the MCP server).
The swarm already allowed this — a bypass-mode launch on a peer is the same
power — jobs make it explicit. So jobs are **off by default** and switched
on per machine, live, **only from that machine itself**: `claudeship hub
jobs on|off` in a terminal there (the hub's socket, this user's alone), or
the web page's gear menu with the page opened there at `localhost` (the
hub takes `POST /api/settings {"jobs": true|false}` → `{ok, jobs, ended}`
only from a loopback connection); or `"jobs": true` in that hub's
`config.json` for the next start:

```json
{ "jobs": true, "jobsMaxSeconds": 28800 }
```

Not from the phone (it shows the state, read-only), not from a browser on
another computer (a non-loopback client is 403, pairing secret or not),
not through another hub (a `host` naming a peer is 403, and a peer's
`/peer/api/settings {jobs}` is 403). The point: a Claude session on one
machine must not be able to open remote execution on another, whatever it
holds or opens — the one thing that can switch it is something already
running on that machine, which already has that user's powers there. The
web switch asks for confirmation. **Off is total**: new jobs are refused
from that moment and every running job is ended at once (SIGKILL to its
group, as `hub stop` does); the answer says how many. Every flip is
logged (`jobs enabled from this machine (claudeship hub jobs)`). The hub
writes the new value to `config.json`, so it survives a restart;
`jobsMaxSeconds` is still read at start. `hub status` says `jobs:
enabled` or `jobs: disabled`; `hub status --json` and `/api/state`
(top-level and per host in `hosts[]`, and `ship_hosts`) carry `"jobs":
true|false`. With it off, every jobs route on that hub — the
client's `/api/jobs…` handled there and a peer's `/peer/api/jobs…` — is
403 `{"error": "jobs disabled on this host"}`. The switch is about running
jobs, not relaying them: a hub with jobs off still forwards a request
naming another host, which decides for itself.

## The model

`Job {id, cwd, argv, permissionMode, startedAt, finishedAt, exitCode, pid,
maxSeconds, timedOut, stdout, stderr, truncated, claudeSessionId, result}`,
in the hub's memory only (a hub restart forgets them).

- **id**: six hex digits, like a session's.
- **Spawn**: `argv` under the web launch's recipe — the user's login shell
  (`$SHELL -l -i -c 'exec "$0" "$@"' argv…`, so PATH and version managers
  are set up) with the same small whitelisted environment (`HOME`, `USER`,
  `LOGNAME`, `PATH`, `LANG`, `LC_*`, `TMPDIR`, `SSH_AUTH_SOCK`, `SHELL`,
  `TERM`, …) plus the request's `env`; stdin `/dev/null`; stdout and stderr
  on pipes; a process group of its own (`setpgid` before exec), every
  other descriptor close-on-exec and every signal default, as `pty.rs`
  does for sessions.
- **cwd**: canonicalised (symlinks resolved) and required to be the home
  directory, the hub's `root`, or any directory under the root
  (subfolders, worktrees). Absent: the home directory. Anything else is
  400. The request body is untrusted.
- **Output**: stdout keeps its newest 4 MB, stderr 256 KB; `truncated`
  (`stderrTruncated`) says bytes were dropped from the front. Offsets are
  absolute — byte *n* since the job started — so a reader's `since` stays
  meaningful after a drop.
- **Claude jobs** (the convenience form, or an `argv[0]` whose name ends in
  `claude`): stdout is read line by line as stream-json; a `system` or
  `result` event's `session_id` becomes `claudeSessionId`, the last
  `result` event's `result` becomes `result`.
- **Wall clock**: `maxSeconds` (default 1800; a request may ask for 1 to
  14 400, or up to `config.jobsMaxSeconds`). On expiry the group gets
  SIGTERM — repeated each second, since one that lands while the login
  shell is still starting is ignored — and SIGKILL 10 s later; the job
  ends with exit code **124** and `timedOut: true`, its output so far kept.
  `POST …/kill` is the same escalation without `timedOut` (exit `128 +
  signal`, 143 for a SIGTERM).
  Once the program itself has gone, whatever is left of its group (a
  child that ignored SIGTERM) is SIGKILLed at once, so nothing outlives a
  kill or the cap.
- **Retention**: a finished job is kept 1 h, or until read with
  `consume=1`. At most 64 jobs per hub: a new one evicts the job that
  finished first; with 64 running, 429.
- **`hub stop`** SIGKILLs every running job's group.
- **`waitingFor`**: while a Claude job runs and its conversation is in the
  Claude registry (matched by `claudeSessionId`) with status `waiting`, its
  `waitingFor` (else `"permission"`). Under `manual`/`plan` the hub's
  approval bridge raises the prompt as for any session, so a person can
  answer it from the phone or the web page while the job waits.

## Endpoints

Same gates as the rest of `/api/*` (paired cookie; POSTs same-origin JSON);
`host` (an id from `/api/state`'s `hosts[]`; absent, empty, or this hub's
id means here) is proxied as in phase 10: 404 `no such host`, 409
`protocol mismatch`, 502 `unreachable`. A forwarded GET waits the `wait`
plus 5 s for the peer's answer, and the client's connection is allowed the
wait plus 10 s past the usual 15 s sweep. The peer side is
`/peer/api/jobs…` (bearer, `LocalOnly`; a `host` there is 400).

`POST /api/jobs`

```json
{"host": "<id>?", "cwd": "/abs/dir?", "argv": ["make", "test"],
 "permissionMode": "auto?", "maxSeconds": 1800, "env": {"CI": "1"}}
{"host": "<id>?", "cwd": "/abs/dir?", "prompt": "run the tests",
 "permissionMode": "auto?", "resume": "<uuid>?", "maxSeconds": 1800, "env": {}}
```

→ 200 `{"id": "3fa9c1", "host": "<this hub's id>"}`. Exactly one of
`argv` (a non-empty list of strings, run as given; `permissionMode` is
only recorded) and `prompt` (the convenience form: `<claude> -p <prompt>
--verbose --output-format stream-json --permission-mode <mode, default
auto> [--resume <uuid>]`, where `<claude>` is `CLAUDESHIP_CMD` or `claude`
as for sessions; stream-json under `-p` needs `--verbose`). `env` names
must be identifiers, and not `HOME`, `SHELL`, `USER`, `LOGNAME`, `ENV`,
`BASH_ENV`, `ZDOTDIR`, `DYLD_*`, `LD_*`. 400 `{error}` for a bad cwd,
argv, mode, resume, maxSeconds, or env; 429 with 64 running; 500 spawn
failure; 403 jobs off.

`GET /api/jobs?host=` → `{"host": "<id>", "jobs": [summary…]}`, oldest
first, where a summary is

```json
{"id": "3fa9c1", "cwd": "/abs", "argv": ["…"], "permissionMode": "auto",
 "running": false, "exitCode": 0, "pid": 4242, "startedAt": 1791400000000,
 "finishedAt": 1791400004000, "maxSeconds": 1800, "timedOut": false,
 "claudeSessionId": "<uuid>|null", "stdoutBytes": 1234, "truncated": false}
```

(times in epoch ms; `exitCode`/`finishedAt` null while running).

`GET /api/jobs/<id>?host=&since=<n>&wait=<s>&consume=1` → the summary plus

```json
{"stdout": "text from byte since", "since": 0, "next": 1234,
 "stderr": "all of it kept", "stderrTruncated": false,
 "result": "final text|null", "waitingFor": "…|null", "host": "<id>"}
```

`since` in the answer is where the text actually starts (later than asked
when those bytes were dropped); pass `next` as the next `since`. Text never
splits a UTF-8 character (a partial one waits for the next read; other
invalid bytes read as U+FFFD). `wait` (seconds, at most 60) holds the
request until the job finishes or has stdout past `since`, or the time is
up — it returns at once if either is already true. `consume=1` removes a
finished job once this answer is made. 404 `{"error": "no such job"}`.

`POST /api/jobs/<id>/kill {"host": "<id>?"}` → `{"ok": true}` (also for a
job that already finished); 404.

## The command

```
claudeship run [--host <name|id>] [--cwd <dir>] [--mode <m>] [--max-seconds <n>] -- <argv…>
claudeship ask [--host …] [--cwd …] [--mode …] [--resume <uuid>] [--max-seconds <n>] [-v] "<prompt>"
claudeship jobs [--host …]
claudeship jobs wait|output|kill <id> [--host …]
```

It talks to this machine's hub over loopback with its pairing secret (the
socket's `link` op), as a paired client; `--host` takes a host's id, name,
or id prefix from `hosts[]`. `--cwd` defaults to the current directory
here, the host's home there (quote a `~/…` meant for the other machine:
the hub there expands it).

- `run` prints the job id.
- `ask` starts the convenience form, follows it (long-polls of 60 s), and
  prints the final `result` text to stdout, `session: <uuid>` to stderr
  (pass it to `--resume` for a follow-up), the stream to stderr with `-v`,
  the job's stderr when it failed, and exits with the job's code — 124,
  with a note, when the wall clock ended it. While the remote Claude waits
  on a permission prompt it says so on stderr.
- `jobs` lists; `jobs wait` streams the output and exits with the job's
  code; `jobs output` prints what is kept; `jobs kill` ends it.

`run`, `ask`, and `jobs` as the first word are these commands, not a
prompt for an interactive session.

## Example orchestration

Build here, test there, merge: from a terminal on the Mac with `steam` in
the swarm (jobs on there),

```sh
cargo build && git push origin feature
claudeship ask --host steam --cwd '~/code/ship' --max-seconds 3600 \
  "git fetch && git checkout feature && cargo test --workspace; fix what fails and commit" \
  2> /tmp/steam.err
session=$(sed -n 's/^session: //p' /tmp/steam.err)
claudeship ask --host steam --cwd '~/code/ship' --resume "$session" \
  "push your commits to origin/feature"
git pull origin feature && git checkout main && git merge feature
```

The same from a Claude session goes through the MCP tools below.

## MCP

`claudeship mcp` is an MCP server on stdio that gives Claude Code the swarm
and the jobs engine as tools (`hub/src/mcp.rs`). Register it once per
machine you orchestrate from:

```
claude mcp add claudeship -- claudeship mcp
```

Only the bare `claudeship mcp` is the server; `claudeship mcp add …` and the
other `mcp` subcommands still go to claude's own `mcp` command.

**How it reaches the hub.** Like the Mac app's `HubClient`: plain HTTP to
`http://127.0.0.1:<port>` (`port` from the hub home's `config.json`, default
7433) with `Cookie: claude_ship=<token>` from the home's `token` — read on
every call, so a hub installed or re-paired after the server started works
without restarting it. `CLAUDESHIP_HOME` relocates the home as everywhere
else. No cookie jar, no proxy; 1 s for `/api/state`, the wait plus 5 s for a
long-poll, 10 s for a start or kill (they may be relayed to a peer). Every
other machine is reached *through* the local hub: a tool's `host` (a name,
an id, or an id prefix of 4+ characters from `ship_hosts`; omitted, `local`,
or `here` means this machine) is resolved against `/api/state`'s `hosts[]`
and sent as the jobs endpoints' `host` id. Stdout is the protocol; the
server logs to stderr only.

**Protocol.** JSON-RPC 2.0, one message per line, MCP revision
`2024-11-05`: `initialize` (capabilities `tools`), `ping`, `tools/list`,
`tools/call`; notifications (`notifications/initialized`, cancellations)
are taken silently; batches are answered as batches; anything else is
`-32601`, a line that isn't JSON `-32700`, a message that isn't JSON-RPC
2.0 `-32600`, an unknown tool `-32602`. Calls are served one at a time.

**Tools.** Every answer is `content: [{type: "text", text: <JSON>}]`; a
failure is a result with `isError: true` and a message naming the host
(403 → jobs are off on that host until its `config.json` has `"jobs":
true`; 404 → unknown job or host; 409 → protocol mismatch, theirs and ours;
502 → unreachable; no local hub → "cannot reach the local ClaudeShip hub").

| Tool | Arguments | Answer |
| --- | --- | --- |
| `ship_hosts` | — | `{hosts: [{name, id, local, reachable, protocol, root, home, defaultPermissionMode, lastSeen, sessions}]}` |
| `ship_sessions` | `host?` | `{sessions: [{host, hostId, project, status, waitingFor, cwd, name, title, branch, sessionId, hubId, pid, background, mode, pendingApprovals?}], unreachableHosts?}` |
| `ship_ask` | `host, cwd, prompt, mode?, resume?, maxSeconds?` | finished: `{result, sessionId, exitCode, timedOut, job, host}`; else after 50 s `{job, host, running: true, stdoutSoFar, waitingFor?}` |
| `ship_run` | `host, cwd, argv, mode?, maxSeconds?` | `{job, host, hostId}` at once |
| `ship_wait` | `host, job, timeoutSeconds? (0–50, default 30)` | status (`running, exitCode, timedOut, waitingFor, truncated, stdoutOffset, sessionId, stderr`) + `stdout` new since the last `ship_wait`/`ship_ask` of that job; `result` once a Claude job has finished |
| `ship_output` | `host, job, since? (byte offset, default 0)` | status + `stdout` from `since`, at most 48 KB, `more`, `nextSince`; does not move `ship_wait`'s position |
| `ship_kill` | `host, job` | `{ok, job, host}` |

`cwd` must be absolute (inside that host's root or home — the hub checks).
`mode` is one of the hub's permission modes (`acceptEdits`, `auto`,
`bypassPermissions`, `manual`, `plan`, `dontAsk`); `ship_ask` sends `auto`
when none is given, `ship_run` sends none. `ship_ask` posts the convenience
form (`prompt`, which the hub turns into `claude -p … --output-format
stream-json`) and long-polls `GET /api/jobs/<id>?wait=` in a loop until the
job ends or 50 s have passed, well inside an MCP client's patience and the
job's own `maxSeconds`. `result` is the `result` field of the last
stream-json `result` event (found even when it arrives split across two
reads), else the last 4 KB of stdout (a non-Claude command, or a run that
died early). `sessionId` is the hub's `claudeSessionId`, else the event's
`session_id`; pass it as `resume` to continue that remote conversation. The
server keeps one thing in memory per job: its next byte offset (the hub's
`next`, not the text's length — dropped or non-UTF-8 bytes make those
differ) and the last `result` event it read, which a `ship_wait` that sees
the finish with no new text still reports (a Claude run prints `result`
and exits a moment later), so
`ship_wait` returns only new output (at most 48 KB of it; a larger burst
says which `since` to read the rest from with `ship_output`).

**A worked example.** With the server registered on the Mac and a Linux
box named `steam` in the swarm, a prompt to Claude on the Mac:

> Build this branch and push it. Then on steam, in /home/me/code/ship,
> pull it and run `cargo test --workspace`, and report what failed.
> Ask the Claude there to fix any failure it can and tell you what it
> changed, then ask it as a follow-up whether the fix needs a doc change.

goes, tool by tool:

1. `ship_hosts {}` → `steam` is reachable, root `/home/me/code`.
2. Local Bash: `cargo build`, `git push`.
3. `ship_ask {host: "steam", cwd: "/home/me/code/ship", prompt: "git pull,
   then run cargo test --workspace. Fix any failing test you can and say
   what you changed.", maxSeconds: 3600}` → after 50 s `{job: "3fa9c1",
   running: true, stdoutSoFar: …}`.
4. `ship_wait {host: "steam", job: "3fa9c1"}` (repeated while `running`) →
   `{running: false, exitCode: 0, result: "2 tests failed in …; fixed …",
   sessionId: "6c1f…"}`.
5. `ship_ask {host: "steam", cwd: "/home/me/code/ship", resume: "6c1f…",
   prompt: "Does that fix need a change in docs/?"}` → the same remote
   conversation, with its context, answers.

Meanwhile `ship_sessions {host: "steam"}` shows whether someone is already
working in that checkout. In `manual` mode a remote permission prompt
shows up on the phone and the web page; `ship_wait` reports `waitingFor`
until someone answers it there.
