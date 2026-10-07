# The hub (Rust)

The `claudeship` binary in `hub/`: the session hub, the `claudeship`
command that runs Claude through it, and the hub's internal entry points.
It replaces the Swift hub (`Hub.swift`, `HubPTY.swift`, `HubCLI.swift`);
until the Mac cutover (plan phase 5) both exist, and the Swift one keeps the
real sessions. See `rust-core-plan.md` for the phases; this file is the
architecture and the regression checklist.

Phase 3 built the pty, the supervisor, the hub, the local socket, and the
CLI; phase 4 the web server, the pairing secret, and `hub link`'s working
URLs (see "The web face" below); phase 7 remote approval ("Approvals"
below, and `approvals.md`) and the login service.

## Commands

```
claudeship [claude args]            run claude through the hub
claudeship hub status [--json]      the hub and its sessions (--json joins the Claude registry)
claudeship hub link | unlink        pairing: print the links + QR / rotate the secret
claudeship hub start | stop [--force] | restart [--force]
claudeship hub attach <id | uuid>   open a running session in this terminal
claudeship hub kill <id>            end a session
claudeship hub run                  internal: be the hub
claudeship hub supervise <argv>     internal: session leader on a pty
claudeship hub install-hook | uninstall-hook          the PermissionRequest hook in Claude's settings
claudeship hub install-service | uninstall-service [--no-load]   the hub as a login service
claudeship permission-hook          the hook helper (Claude Code runs it; see approvals.md)
```

**The login service.** `install-service` writes, on macOS,
`~/Library/LaunchAgents/<label>.plist` (label `$BUNDLE_ID.hub`, from the
environment or else the `BUNDLE_ID` the binary was built with — `install.sh`
builds with `.env` loaded — else `com.example.claudeship.hub`; `ProgramArguments [<this binary>, hub, run]`,
`RunAtLoad`, `KeepAlive`, stdout and stderr to `hub.log`) and `launchctl
bootstrap`s it (booting out a loaded one first); on Linux
`~/.config/systemd/user/claudeship-hub.service` (`ExecStart=<this binary>
hub run`, `Restart=on-failure`, `WantedBy=default.target`), then
`systemctl --user daemon-reload` and `enable --now`, and prints the
`loginctl enable-linger` hint. Both set `CLAUDESHIP_SERVICE=1` (and
`CLAUDESHIP_HOME` when it is set), which makes `hub run` *wait* for a lock
another hub holds instead of exiting — so installing the service beside a
running hub doesn't have launchd respawning it every ten seconds; it takes
over when that hub stops. `--no-load` writes the file only (the tests use
it). Under launchd, `hub stop` is a restart with the installed build
(KeepAlive starts it again) and says so; `hub stop --force` boots the
service out first, so the hub stays stopped until the next login. Under
systemd a clean stop is not restarted (`Restart=on-failure`), and `hub stop`
says how to start it again. "Under the service" means the unit runs a hub
in this `CLAUDESHIP_HOME` (both unset, or the same path): `hub stop --force`
on a private hub never unloads the real service.

**SteamOS / distrobox.** Everything (claude, cargo, the hub) lives in one
box; the host has no systemd of the box's own, so `install-service` run
inside a box (`/run/.containerenv` or `/.dockerenv`) writes the unit for the
host's `systemctl --user` (same `~/.config/systemd/user`, `$HOME` is shared)
with `ExecStart=distrobox-enter -n <box> -- env CLAUDESHIP_SERVICE=1
[CLAUDESHIP_HOME=…] <binary> hub run` — `distrobox-enter` starts a stopped
box and doesn't reliably forward the environment, hence `env` — and runs
`daemon-reload` / `enable --now` / `disable --now` through
`distrobox-host-exec`. The box name is `$CONTAINER_ID`, else the `name="…"`
line of `/run/.containerenv`; without one it fails and says to set
`CONTAINER_ID`. "Under the service" matches the home anywhere in the wrapped
line. Don't `distrobox-export --bin` the binary (the wrapper would land on
the same `~/.local/bin` path); linger is enabled on the host
(`distrobox-host-exec loginctl enable-linger $USER`).

Invocations that aren't a session to share run claude directly, in place
of the command (`exec`): `-p`, `--version`, `--bg`, claude's management
subcommands (`cli/args.rs`), stdin or stdout not a terminal, no terminal
size, and — new in the Rust port — any argument that isn't UTF-8 (the
socket protocol carries JSON strings). Arguments reach claude byte for byte.
Environment variables that aren't UTF-8 are not forwarded to hub sessions.

## Files

`CLAUDESHIP_HOME` overrides the directory (tests use a short one: the
socket path must fit `sun_path`, 104 bytes on macOS). Defaults: macOS
`~/Library/Application Support/ClaudeShip/hub` (the Swift hub's, so paired
browsers keep working); Linux `$XDG_STATE_HOME/claudeship`, else
`~/.local/state/claudeship`. Inside: `hub.sock` (0600, created under a 0177 umask so there is no window before the chmod), `hub.lock` (flock,
0600), `hub.log` (rotated to `hub.log.1` past 2 MB at a start),
`config.json`, `token` (the pairing secret: 64 hex characters from
`getrandom`, written to `token.new` and renamed, 0600), `approvals.sock`
(0600, bound under the same umask as `hub.sock`). `CLAUDESHIP_CMD`
replaces `claude`; `CLAUDESHIP_WEB` serves the page from that directory,
read per request, instead of the copy built into the binary.

## Modules

| Module | What |
|---|---|
| `main.rs` | argv dispatch: `hub …`, `permission-hook`, else the CLI |
| `approvals.rs` | remote approval: the wire format and `summary`, the rules, reconciliation, `Approvals` (the hub's pendings and rules), the `approvals.sock` server |
| `hook.rs` | `permission-hook` (the helper) and `install-hook` / `uninstall-hook` |
| `service.rs` | `install-service` / `uninstall-service`: the LaunchAgent plist, the systemd unit |
| `paths.rs` | the files above, `program()`, `self_command()` |
| `pty.rs` | `posix_openpt`, slave held open across the spawn with `TIOCSWINSZ` + `IUTF8`; `Command` + `pre_exec` (setsid, slave as fds 0–2, `TIOCSCTTY`, every other descriptor close-on-exec — the Swift hub's `POSIX_SPAWN_CLOEXEC_DEFAULT` — default signals); master non-blocking, CLOEXEC. `hub start` marks the caller's descriptors close-on-exec the same way |
| `supervisor.rs` | the session leader; see below |
| `session.rs` | `Session`, `Client`, and `Sink` (a client's output channel) |
| `hub.rs` | the hub: one task owns all state and takes `Command`s |
| `local.rs` | `hub run`: lock, `RLIMIT_NOFILE` 4096, `chdir("/")`, SIGPIPE ignored, the socket, a reader + writer task per connection |
| `procs.rs` | parent pid (macOS `sysctl KERN_PROC`, Linux `/proc/<pid>/stat`), ancestor walk, hostname |
| `net.rs` | tunnel-interface addresses (for the web gate), tailnet IPv4s (for links) |
| `token.rs` | the pairing secret: load or mint, rotate |
| `web/server.rs` | the listener, the connection gate, serving a connection, the unanswered-request sweep |
| `web/router.rs` | the request gate (Host, Origin, cookie), the routes, the response headers |
| `web/ws.rs` | a browser terminal: one WebSocket as a hub client |
| `web/assets.rs` | the page, embedded from `../web` (`include_dir`; `build.rs` makes cargo watch the folder) |
| `web/state.rs` | `/api/state`: the pure rules, the builder, the one-second cache |
| `web/security.rs` | the rules behind the gate (addresses, Host, Origin, cookie, constant-time compare) |
| `cli/` | the command: `mod.rs` (launch/attach decision), `attach.rs` (the terminal loop), `client.rs` (socket, requests, `hub start`), `hub_cmd.rs`, `qr.rs` |

## How the hub runs

Tokio, multi-thread. One task owns `Hub { sessions, ended, clients, … }`;
everything else talks to it over an unbounded command channel — the shape
of the Swift hub's serial queue.

- **pty output.** A pump task per session awaits readiness of the master
  (`AsyncFd`) and sends `Drain`; the hub reads at most 8 × 64 KB and
  answers whether it read the pty dry. Readiness is cleared only then, so a
  drain the round limit cut short is followed by another at once. Output
  is fed through the `TerminalStream` into the `ReplayBuffer`, and the raw
  bytes go to every attached client.
- **Exit.** A `SIGCHLD` stream makes the hub `waitpid(pid, WNOHANG)` each
  session's supervisor (never `-1`); read-EOF/EIO does the same. Reaping
  drains up to 256 more reads first (the program's exit cleanup must reach
  the screens before the exit notice), then moves the session to `ended`
  for 30 s, so a late attacher still gets the output and exit code.
- **Input.** Written non-blocking; the rest retried on a timer, 3 ms
  doubling to 100 ms, never write-readiness (it does not fire reliably for
  a pty master on macOS: an 8 KB paste stalled after 1 KB). 8 MB cap.
- **Clients.** Each connection has a reader task (frames → commands) and a
  writer task draining its `Sink`. A sink counts queued bytes; past 32 MB
  the client is cut off (abort ends both tasks, the reader's `Closed`
  detaches it). Error, reply, and exit frames close the connection after
  they are written; `stop`'s reply fires the shutdown once it is out.
- **Size.** One pty, one size: the client that attached, typed, or resized
  most recently owns it (`claim_size`). Typing means `is_user_activity`: a
  focus report or a query answer doesn't take the size. Sizes are clamped
  to 2..=1000 rows, 10..=2000 columns.
- **Ending a session** (`kill`): SIGHUP to the supervisor's group; after
  5 s SIGTERM (the supervisor SIGKILLs its job); after 1 s more SIGKILL to
  the pty's foreground group (`TIOCGPGRP`) and the supervisor's.
- **Shutdown** (`stop`, SIGTERM, SIGINT): SIGHUP to every session's group,
  remove the socket, exit. With no hub left to escalate, each supervisor
  does it itself: 5 s after a hang-up it SIGKILLs a job still running.
- **Versions.** `frame::PROTOCOL` is checked on `launch` and `attach` only;
  management ops work across builds. The hub is never restarted by an
  install; `hub restart --force` does it.

## The supervisor

Each session's pid is `claudeship hub supervise <program> <args>`, the
session leader on the pty, with the program as its foreground job in a
group of its own. A leader with no parent in its session is an orphaned
process group, whose SIGTSTP the kernel discards: Claude Code's Ctrl+Z
(cooked mode, "run `fg`", stop itself) would strand the session half
suspended. The supervisor turns a stop into `tcsetpgrp` + SIGCONT — a
redraw. The job takes the terminal itself between fork and exec (from
the parent alone it would race the exec, and a program that sets raw
mode at once would be stopped for doing it from the background). It
ignores SIGTTOU/SIGTTIN/SIGTSTP itself (the job gets defaults),
holds SIGHUP/TERM/INT/QUIT blocked until its handlers exist, forwards
SIGHUP/INT/QUIT to the job's group, answers SIGTERM — and an alarm 5 s
after a SIGHUP, so a job that ignores the hang-up can't outlive a stopped
hub — by SIGKILLing the job, and exits only when the job does, with its status (`128 + signal` if
killed). The hub spawns it from the binary's resolved path, so after an
install new sessions get the new supervisor while the old hub runs on.

## The terminal client

`claudeship [args]` connects (starting the hub if needed: `hub run` in a
new session, stdin `/dev/null`, output appended to `hub.log`), puts the
terminal in raw mode **before** sending the hello (a Ctrl+C typed then is
input for the session, not a signal that strands it), and relays bytes
until the session exits (its exit code becomes ours) or the terminal goes
away (that only detaches). A `TerminalStream` mirror of the output lets it
switch off the modes the program set if it leaves first. SIGWINCH becomes
a resize frame; SIGTERM/HUP/INT/QUIT restore the terminal and exit
`128 + signal`. `--resume <uuid>` of a conversation already running in the
hub attaches to it instead (registry entry → ancestor walk → supervisor).

## The web face

The page (`web/`), its JSON API, and one WebSocket per browser terminal, on
`config.port` (7433). It is a remote shell, so it is gated three ways, all
required (the reasons are in `CLAUDE.md`, "Session hub"):

1. **Network**, in the accept loop (axum never sees the local address): a
   loopback connection only from loopback; otherwise our address must be one
   a tunnel interface (`config.tunnelInterfaces`: `utun` on macOS,
   `tailscale` on Linux) holds right now, and both ends in Tailscale's ranges.
   Refusals are logged at most once a minute, with both addresses.
2. **Host header**: `localhost` or a loopback/tailnet IP literal, never a DNS
   name (rebinding); `config.allowedHosts` is the exact-match escape hatch.
3. **Pairing secret**: `/auth?k=<token>` (from `hub link`) sets the
   `claude_ship` cookie (HttpOnly, SameSite=Strict, a year); every `/api/*`
   request and the WebSocket need it, compared in constant time. On the
   upgrade and on POSTs any `Origin` must be ours (one that isn't text, or
   more than one, is not); POSTs must be JSON (body at most 1 MB).
   `hub unlink` mints a new secret and drops every open web connection,
   including an upgrade that passed the check a moment before.

**Server.** One dual-stack socket (`socket2`: IPv6 with `v6only` off,
`SO_REUSEADDR`, listen 32, non-blocking; IPv4 alone where there's no IPv6),
retried every 10 s while the port is taken (logged once). Each admitted
connection gets `TCP_NODELAY` and keepalive (30 s idle, 10 s apart, 4
probes — a phone that leaves never says goodbye) and is served by hyper's
HTTP/1 connection with upgrades, the axum router as the service
(`TowerToHyperService`). One request per connection (`Connection: close`), its head at most 64 KB
(hyper's `max_buf_size`, enforced per read, so roughly).
At most 128 connections (a WebSocket counts until it closes). A request not
answered within 15 s is dropped.

**Routes** (paths, codes, and bodies as the Swift hub's):

| Route | Gate | |
|---|---|---|
| `GET /auth?k=` | host | 303 `/` + cookie; 403 "That pairing link is not valid for this hub." |
| `GET /ws/term?id&rows&cols&claim` | host, same-origin, paired | the terminal; any other upgrade → 403 "websocket refused" |
| `GET /api/state` | host, paired (401 `{"error":"not paired"}`) | the directory |
| `POST /api/launch` `{path, permissionMode?, resume?}` | host, same-origin, paired, JSON (403 `{"error":"refused"}`) | `{id}`; 400 not a project directory / unknown permission mode / bad conversation id; 500 on spawn failure |
| `POST /api/kill` `{id}` | same | `{ok}`; 404 no such session |
| `POST /api/settings` `{defaultPermissionMode}` | same | `{ok}`, saved to `config.json` |
| `POST /api/approve` `{id, allow}` | same | `{ok}`; 404 no such approval; 400 |
| `POST /api/auto-approve` `{sessionId, rule}` | same | `{ok}` (`5m`, `session`, `off`); 400 |
| `GET /*` | host | the page's files: `.`-prefixed or empty components refused, html/js/css/svg/png/json/webmanifest only |

Every answer but the 101 carries `Cache-Control: no-store`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`Connection: close`; the page's files add the CSP (`connect-src` spells out
`ws://<host> wss://<host>`: Safari doesn't count ws: under `'self'`). A
Host that fails the check is 403 "This hub answers only to localhost or its
Tailscale IP address."

**A browser terminal** is a hub client like a terminal on the Unix socket:
`Connected` with a `Sink`, then `WebAttach`. Binary messages are pty bytes
both ways; the page sends `{type: resize|fit|ping}` (`resize` claims the
size, `fit` only records it unless this screen owns the size, `ping` is
answered `pong`), and is sent `size {rows, cols, owner}`, `exit {code}`
(then a 1000 close), and `gone` (unknown id or size out of range; then a
1000 close). `claim=0` joins as a spectator. Output goes in messages of at
most 256 KB; past 32 MB queued the browser is cut off; a peer that hasn't
taken a message in 60 s is dropped (it can reconnect and take the replay).
WebSocket pings are answered by the library.

**State** (`GET /api/state`): the Swift `HubState.build`, key for key —
projects under `root`, live registry sessions matched to hub sessions by the
supervisor nearest in each one's ancestry, `starting` entries for hub
sessions Claude hasn't registered, `sub` for worktrees, three titled recent
transcripts per project among its newest eight, `branch` from `.git/HEAD`,
`elsewhere`, the directory order — plus `jobId` on a session whose registry
entry has one, and (protocol 3) `sessionId`, `approvals`, `autoApprove` on
every session and `approvalsSupported` at the top (see "Approvals"). Built from a snapshot the hub task hands over, on a blocking
thread, at most once a second with concurrent requests waiting on the same
build; invalidated by a launch, kill, settings change, or a permission request
arriving, being answered, or being dropped — a build begun
before that is neither cached nor handed to a request made after it. Milliseconds are
computed along the same floating-point path Foundation took, so the two
hubs agree to the millisecond. `hub/tests/golden_state.sh` diffs the
running Swift hub's answer against a private Rust hub's (read-only towards
the Swift one; masks `now`, `protocol`, `jobId`, and the hub-owned
`hubId`/`attachable`/`viewers`/`mode`/`key`).

## Approvals

The PermissionRequest hook's helper (`claudeship permission-hook`) connects
to `approvals.sock`; each connection is one pending request, held by the
hub task in `Approvals` with the oneshot its connection task waits on (a
value is the verdict; dropping it closes the connection without one; the
helper's EOF sends `ApprovalClosed`). `POST /api/approve` answers one;
`POST /api/auto-approve` sets a per-session rule (in memory) and answers
that session's pendings; a request whose session has a live rule is
answered on arrival. Each `/api/state` build gets the pendings and rules in
its snapshot, leaves out the stale ones (answered in the terminal: status
left `waiting` with a newer `statusUpdatedAt`; or no registry session for
10 s), forces `waiting` on a session with requests, and sends `Reconcile`
back to the hub task, which cancels those and prunes rules (expired, or
session gone). While anything is pending or any rule stands, a ticker
builds every 2 s so this happens with no screen open. Every change invalidates the state cache. The
hook, the socket, and the API are in `approvals.md`.

## Tests

```
cargo test -p claudeship                  # unit tests + hub/tests/private_hub.rs
cargo clippy -p claudeship --all-targets
cargo check -p claudeship --all-targets --target x86_64-unknown-linux-gnu
```

`private_hub.rs` starts a private hub per test (each with a free port of
its own in its `config.json`, never the real hub's) (`/tmp/cs-<pid>-<n>`, the
`claudeship-stand-in` binary from `hub/tests/bin/stand-in.rs` as the
program) and speaks the socket protocol: launch and echo, exit codes, late
attach into the alt screen with the modes from before a wipe restored and
the DA1 query left out, size claimed by typing but not a focus report, an
8 KB paste, kill escalation past a SIGHUP-ignoring program (and `stop
--force` leaving no such program behind), a failed
launch's output for a late attacher, Ctrl+Z continued, a 20 MB blast
arriving whole, a client that stops reading cut off without stalling the
hub, version refusals, request errors, the lock, `stop` with and without
`--force`, the bypass (`exec`, non-UTF-8 argument), and the CLI's text.
Every test stops its hub and checks it is gone.

`approvals.rs` (in `hub/tests/`) runs the real helper against a private
hub, with a fake Claude registry in a scratch `CLAUDE_CONFIG_DIR`: approve
and deny (the exact decision JSON on stdout), a killed helper's request
gone, no hub → no output, the socket 0600, standing rules answering pending
and new requests and pruned when the session leaves, a prompt answered in
the terminal hung up on with no screen polling, the 10 s prune, the cache
invalidated by an arrival and a hang-up, `install-hook` against a settings
file with other hooks and the Swift helper's entry, and `install-service
--no-load` (with `HOME` in the scratch directory and a test-only
`BUNDLE_ID`, so nothing real is written or unloaded).

`web.rs` does the same for the web face, with `HOME` and the project root
inside the scratch directory too, and hand-written HTTP and WebSocket
clients: pairing (cookie, redirect, secret file 0600), 401 without the
cookie, 403 for a DNS Host, cross-origin and non-JSON POSTs, launch targets
and modes (the home directory accepted, under the login shell), kill and
settings, attach (size, replay, live bytes, ping/pong, WebSocket ping), the
spectator and the size owner, `exit` then a 1000 close, `gone`, refused
upgrades, `unlink` hanging up open terminals, the page's files and the CSP,
and both sweeps (an unfinished request at 15 s; a terminal that stops
reading at 60 s — this one takes a minute). Its hubs and `approvals.rs`'s
wait for `webListening` in `hub status --json` (that hub listening, not
whoever answers on the port), and start again on another port, up to five
times, when the one picked was taken before the hub could bind it — the
hub itself would only retry in 10 s.
## Manual checklist (before cutover, and after changing the pty path)

Run the Rust hub beside the Swift one with a private home, e.g.
`export CLAUDESHIP_HOME=/tmp/cs-manual` in each terminal used, and
`target/debug/claudeship` (or a release build) as the command.

1. **Paste.** In a session, paste a ~10 KB block into Claude's prompt. All
   of it arrives (check the end of the pasted text).
2. **Ctrl+Z.** Press Ctrl+Z in Claude. It redraws and stays usable; no
   "run `fg`" left on screen, `hub status` still lists it.
3. **Late attach into the alt screen.** Start a session, open a TUI view
   that uses the alt screen (e.g. `/config` or a long transcript), then in
   a second terminal `claudeship hub attach <id>`: the second terminal
   shows the same screen, scrolling and mouse work, and leaving it with
   the window close (not Ctrl+C) restores that terminal's normal screen.
4. **Two-screen size claim.** With the session attached in a small and a
   large terminal: typing in one reflows Claude to that window; just
   clicking into (focusing) the other does not; resizing either takes it.
5. **Kill escalation.** `claudeship hub kill <id>` on a session ends it in
   under a second; on one running `trap '' HUP; sleep 1000` via `!` it
   still ends within ~6 s.
6. **Survives the terminal.** Close the terminal window of a running
   session; `claudeship hub attach <id>` (or `claudeship --resume <uuid>`)
   in a new one brings it back mid-conversation.
7. **Ending.** `claudeship hub stop --force`, then confirm no
   `claudeship hub run` or `hub supervise` process with that home remains
   (`ps -axo pid,command | grep 'claudeship hub'`).

8. **The page.** Pair a browser (`claudeship hub link`, with the private
   home and a free `port` in its `config.json`), then: launch, resume a
   recent conversation, attach, float, dock, pop out, End.
9. **Phone over the tailnet.** Pair the iPhone app by the QR code; directory,
   attach, mirror-when-not-owner, key bar, end. If it "can't connect", check
   `tailscale status --json` for a handshake and bytes from the phone first:
   zero bytes means the phone never sent (its Tailscale off, or iOS Local
   Network permission denied). A refusal by the gate is in `hub.log`
   ("web: refused connection from … to …").
