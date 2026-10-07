# Plan: a Rust core for ClaudeShip

The hub (pty owner, web server, `claudeship` command) becomes one Rust binary
that runs on the Mac and on the Linux box. The macOS menubar app and the iPhone
app stay Swift and become pure clients of that binary. A Linux tray applet
(Python 3 + PySide6, the torrent-flinger shape) is the Linux counterpart of the
menubar app. A browser or the phone can be connected to both hubs at once.

Decisions already taken (2026-10-07):

- **Two hubs in the clients:** the simple form. One hub per machine, one
  pairing per hub; the phone keeps a list of hubs and shows both. No hub
  proxies to another. The federated option is deferred.
- **Hostnames:** unchanged. `localhost`, IP literals, and `allowedHosts` only.
- **Linux desktop:** the same thing torrent-flinger does for KDE — a PySide6
  `QSystemTrayIcon` with a popup styled as a Plasma applet, venv dev mode,
  `.desktop` + autostart, single instance over a local socket.
- **Approvals move into the hub** on both platforms, so the web page, the
  phone, and the Linux applet can answer permission prompts too.
- **Shared scanner crate:** the registry and transcript reader is one Rust
  crate used by the hub and by system-stats.

Phases are ordered so the Mac keeps working at every step. Each phase ends
with the acceptance checks listed under it; don't start the next until they
pass. Checkboxes are the work items.

---

## 0. Shape of the repo afterwards

```
claude-status/
  Cargo.toml                 workspace: hub, claude-sessions
  claude-sessions/           crate: registry + transcript reader (hub, system-stats)
  hub/                       crate: the `claudeship` binary (hub, CLI, web, approvals)
    src/
    tests/                   integration tests against a private hub
    build.rs                 embeds ../web into the binary
  web/                       unchanged; embedded into the hub at build time
  Sources/ClaudeShip/        macOS menubar app (shrinks by ~4,000 lines)
  ios/                       iPhone app (gains a hub list)
  linux/                     PySide6 tray applet, mirrors torrent-flinger/linux
  vscode-extension/          unchanged
  docs/                      this plan, then per-piece docs as they settle
  build-app.sh, install.sh   Mac: gain a cargo build, lose the binary copy
```

The Mac app is not moved into a `macos/` folder in this plan. It would be
tidy, but it is churn with no functional gain; do it later if wanted.

**Binary names.** The Rust binary is `claudeship`. The Mac app's executable is
`ClaudeShip`. They live in different directories (`~/.local/bin` and the app
bundle), so the case-insensitive-filesystem collision that forced the
`claudeship-cli` copy no longer exists. `install.sh` never touches the hub
binary while the hub runs; the hub keeps running the old build until
`claudeship hub restart`.

**What the Rust binary answers to:**

```
claudeship [claude args]            run claude through the hub (unchanged UX)
claudeship hub status [--json]      (unchanged)
claudeship hub link | unlink        (unchanged)
claudeship hub start | stop [--force] | restart
claudeship hub attach <id | uuid>   (unchanged)
claudeship hub kill <id>            (unchanged)
claudeship hub install-service | uninstall-service   launchd / systemd user unit
claudeship hub install-hook | uninstall-hook         PermissionRequest hook in ~/.claude/settings.json
claudeship hub run                  internal: be the hub
claudeship hub supervise <argv>     internal: session leader on a pty
claudeship permission-hook          internal: the PermissionRequest hook helper
```

---

## 1. The `claude-sessions` crate

Pure file reading, no OS seams. Used by the hub's directory builder, by
`hub status --json`, and by system-stats (replacing the hand-rolled JSON
extraction in its `claude.rs`).

**API (first cut):**

```rust
pub struct RegistryEntry { pid, cwd, session_id, name, status, waiting_for,
                           started_at, status_updated_at, kind, job_id }
pub enum State { Busy, Shell, Idle, Waiting }        // unknown → Idle
pub struct LiveSession { entry: RegistryEntry, state: State, is_background: bool,
                         git_branch: Option<String>, title: Option<String>,
                         last_activity: Option<SystemTime> }
pub struct TranscriptTail { session_id, git_branch, ai_title }

pub fn claude_root() -> PathBuf                     // ~/.claude
pub fn project_dir_name(cwd: &str) -> String        // every non-alnum/- char → '-'
pub fn parse_registry_entry(json: &[u8]) -> Option<RegistryEntry>
pub fn state_from_status(s: Option<&str>) -> State
pub fn pid_alive(pid: u32) -> bool                  // kill(pid, 0); EPERM counts as alive
pub fn read_tail(path: &Path, max_bytes: usize) -> Option<TranscriptTail>   // 64 KB, tolerant first line
pub fn parse_tail(text: &str) -> Option<TranscriptTail>
pub fn transcripts_for_project(cwd: &str) -> Vec<(PathBuf, String /*uuid*/, SystemTime)>  // newest first
pub struct Scanner { tail_cache: HashMap<PathBuf, (SystemTime, Option<TranscriptTail>)> }
impl Scanner { pub fn scan(&mut self) -> Vec<LiveSession> }   // sorted (cwd, pid); dead pids skipped
```

Rules carried over exactly (each is a Swift self-test today; port the vectors):
status mapping, dir-name flattening, tail parsing (sessionId, gitBranch, and
ai-title hunted independently from the newest line that carries each), pid
liveness, tail cache keyed on mtime, cache bounded at 256 entries.

Work items:

- [x] Workspace `Cargo.toml` at the repo root; `claude-sessions` with
      `serde`, `serde_json`, `libc` only.
- [x] Port the five "registry/transcript" sections of `SelfTest.run()`
      as `#[test]`s with the same vectors.
- [x] system-stats: path dependency on the crate; replace `read_live_sessions`,
      `munge_path`, `pid_alive`, and the tail reading in `claude.rs`. Its
      richer per-transcript parse (first prompt, cost) stays in system-stats
      for now; move it into the crate later if the hub wants it.

Acceptance: `cargo test -p claude-sessions` green; system-stats builds and its
Claude tab shows the same rows as before.

---

## 2. The hub crate: pure core first

Everything in this phase has no OS dependency and is where the subtle
behaviour lives. Port it with the Swift tests before touching a pty.

**Modules and their Swift sources:**

| Rust module | From | Lines today | Notes |
|---|---|---|---|
| `term/modes.rs` | `TerminalModes` | 50 | tracked DEC modes, kitty stack, modifyOtherKeys; `restore_sequence`, `reset_sequence` |
| `term/stream.rs` | `TerminalStream` | 280 | the tokenizer: queries, OSC 52/9/99/777, bells stripped from the replay copy; `ESC[2J`+`ESC[3J` → clear; `pending_bytes` |
| `term/replay.rs` | `ReplayBuffer` | 45 | 4 MB cap, 32 KB chunks, `base` modes at the front |
| `term/input.rs` | `TerminalInput.isUserActivity` | 70 | what terminals send on their own: query answers, focus, bare motion |
| `frame.rs` | `HubFrame`, `HubFrameDecoder` | 60 | type byte + u32 BE length; `maxPayload` 16 MB; version constant |
| `config.rs` | `HubConfig` | 50 | tolerant parse; `permissionModes`; adds `tunnelInterfaces` |
| `web/security.rs` | `HubWebSecurity` | 180 | canonical v4-mapped, loopback, tailnet ranges, `is_allowed_pair`, `is_allowed_host`, `is_same_origin`, cookie parse, constant-time compare |
| `web/state.rs` (pure half) | `HubState.order`, `projectIndex`, `branch(fromHEAD)`, `launchTarget`, `isSessionId` | 60 | |
| `cli/args.rs` | `bypassesHub`, `resumedSessionId` | 30 | |

Work items:

- [x] `hub/` crate skeleton, `main.rs` with argv dispatch stubs.
- [x] Port every module above.
- [x] Port the hub sections of `SelfTest.run()` one for one: wire framing,
      terminal stream, whose keystroke was that, replay buffer, config,
      directory model, web: who may connect, web: HTTP parse where still
      relevant (host header, cookie, origin). Same vectors, same names.
- [x] `HubFrame.version` → `PROTOCOL: u32 = 2` for now (bumped to 3 in
      phase 6).

Acceptance: `cargo test -p claudeship` green with every ported vector.

---

## 3. The hub crate: pty, supervisor, hub, local socket, CLI

**Runtime:** tokio (multi-thread). The hub's state stays single-owner, the
same shape as today's serial queue: one task owns `Hub { sessions, ended,
config, locals }` and takes `HubCommand`s over an mpsc channel. Attachments
are handles, not trait objects on a queue: each has a bounded output channel
drained by its own writer task; overflow (32 MB today) closes it.

**pty (`pty.rs`).** `posix_openpt`/`grantpt`/`unlockpt`, slave held open
across the spawn carrying `TIOCSWINSZ` and `IUTF8`. Spawn with
`std::process::Command` and a `pre_exec` hook that does only async-signal-safe
work: `setsid()`, open the slave as fds 0–2, `ioctl(0, TIOCSCTTY)`. The
`/bin/sh` trampoline goes away (`Command::current_dir`). The master is
nonblocking and read through `tokio::io::unix::AsyncFd`.

**Supervisor (`supervisor.rs`).** Same program, same reason, same behaviour:
session leader on the pty, job spawned in its own process group and put in
the foreground, `waitpid(WUNTRACED)`; a stop → `tcsetpgrp` + `SIGCONT`;
`SIGHUP`/`SIGINT`/`SIGQUIT` forwarded to the job's group, `SIGTERM` → `SIGKILL`
the job; exits with the job's code (shell convention, `128 + signal`). The
signals are held until the handlers exist. Orphaned-process-group semantics
are POSIX, so this is right on Linux too.

**Hub (`hub.rs`, `session.rs`).** Port verbatim: `launch`, bounded `drain`
(8 rounds per wakeup, 256 on exit), `output` (feed stream → replay; raw bytes
to every attachment), `reap` (drain until empty, then exit notice; keep
`ended` 30 s), `terminate` (SIGHUP to the group → 5 s → SIGTERM → 1 s →
SIGKILL the foreground group via `TIOCGPGRP` then the leader's group),
`attach` (size first, then `replay.snapshot() + stream.pending_bytes`),
`detach` (next attachment claims), `input` (claims size only on user
activity; 8 MB pending cap), `resize`, `note_fit`, `claim_size`. Pty writes
keep the retry timer (3 ms doubling to 100 ms), not write-readiness. Child
exit via `tokio::process::Child` is not possible because the child is not a
tokio child — use a `SIGCHLD` stream (`tokio::signal::unix`) that triggers
`waitpid(WNOHANG)` on every session, plus the read-EOF path that already
calls reap.

**Launch recipes.** `launch_from_terminal` (caller's argv, env, cwd) and
`launch_from_web` (login shell `-l -i -c 'exec "$0" "$@"'`, fish variant,
whitelisted env, `TERM=xterm-256color`, 36×120). Login shell from `getpwuid`,
restricted to the known list, else `/bin/zsh` on macOS and `/bin/bash` on
Linux.

**Local socket (`local.rs`).** Unix socket at `<home>/hub.sock` mode 0600,
same frame protocol, same ops: `launch`, `attach`, `status`, `link`,
`unlink`, `kill`, `stop`. Version check on `launch`/`attach` only. Lock file
`hub.lock` with `flock`. `RLIMIT_NOFILE` raised to 4096. `chdir("/")`.

**CLI (`cli/`).** `claudeship [args]`: bypass set, tty check, size, resume
→ attach-if-running, else launch; raw mode before the hello; the attach loop
with `poll` over socket/stdin/signal pipe; a `TerminalStream` mirror so the
local terminal's modes are reset on early exit; `clearFirst` on attach.
`hub status` text and `--json` (joined to the registry via the ancestry
walk). `hub link` with the QR code (`qrcode` crate, half-block renderer,
black on white). `hub start` spawns `claudeship hub run` detached (new
session, stdout+stderr to `hub.log`, log rotated at 2 MB). Add `hub restart`
(= `stop --force` + `start`, refusing unless `--force` when sessions run).

**Paths (`paths.rs`).** `CLAUDESHIP_HOME` overrides everything. Defaults:
macOS `~/Library/Application Support/ClaudeShip/hub` (unchanged, so the Mac
app and any paired browser keep working); Linux
`$XDG_STATE_HOME/claudeship` (default `~/.local/state/claudeship`). Mind the
104/108-byte `sun_path` limit in tests.

**OS seams, kept in two files from day one:**

| Concern | `procs.rs` / `net.rs` macOS | Linux |
|---|---|---|
| parent pid | `sysctl KERN_PROC` | `/proc/<pid>/stat` field 4 |
| ancestor chain (limit 16) | walk | walk |
| tunnel addresses | interfaces named `utun*` | interfaces named `tailscale*` |
| interface name list | `config.tunnelInterfaces` default `["utun"]` | default `["tailscale"]` |
| hostname | `gethostname`, strip `.local` | `gethostname` |

Dependencies allowed: `tokio`, `libc`, `nix` (optional, for `ioctl`/termios
wrappers), `socket2`, `serde`, `serde_json`, `qrcode`, `getrandom`. No clap:
every argument belongs to claude.

Work items:

- [x] `pty.rs`, `supervisor.rs` with a Ctrl+Z test (stand-in program that
      sends itself SIGTSTP; assert it is continued and the session survives).
- [x] `hub.rs`, `session.rs`, `local.rs`, `cli/*`, `paths.rs`.
- [x] `hub/tests/private_hub.rs`: spawn the built binary with
      `CLAUDESHIP_HOME` (short scratch dir) and `CLAUDESHIP_CMD` pointing at
      a stand-in (a tiny `hub/tests/bin/stand-in.rs`: echoes input, can
      switch to the alt screen, can emit a DA1 query, can blast N MB). Cover:
      launch and output; late attach gets the replay with modes restored;
      a replayed query is not answered twice; two clients, size claim on
      typing but not on a focus report; 8 KB paste arrives whole; kill
      escalation ends a program that ignores SIGHUP; `ended` keeps a failed
      launch's output for a late attacher; exit code is the program's.
- [ ] Side-by-side on the Mac: run the Rust hub under a private home while
      the Swift hub keeps the real sessions. Drive real `claude` through it
      for a working day.

Acceptance: integration tests green on macOS; a real Claude Code session
through the Rust hub survives closing its terminal, is re-attachable, handles
Ctrl+Z, and a second terminal attached mid-TUI shows the right screen.

---

## 4. The hub crate: web

**Server.** hyper 1 + axum router over a manual accept loop on a dual-stack
`socket2` listener (IPv6, `v6only=false`, `SO_REUSEADDR`, keepalive 30/10/4).
The accept loop is where the gate lives, because axum alone can't see the
local address: `peer_addr()`/`local_addr()` → `is_allowed_pair(local, remote,
tunnel_addresses())`, refused connections logged at most once a minute, cap
128 connections, `TCP_NODELAY`, then hand the stream to hyper's http1
connection builder with the axum service. No TLS (unchanged; `tailscale
serve` is the way to get it).

**Router (unchanged paths and semantics):**

| Route | Gate | Behaviour |
|---|---|---|
| `GET /auth?k=` | host | constant-time token compare → 303 `/` + HttpOnly SameSite=Strict cookie `claude_ship` (1 year) |
| `GET /ws/term?id&rows&cols&claim` | host, same-origin, paired | WebSocket (axum `ws`): binary = pty bytes, text = `{type: resize|fit|ping}`; sends `size`, `exit`, `gone`, `pong`; output chunked at 256 KB; 32 MB queue cap |
| `GET /api/state` | host, paired | cached 1 s, built off the hub task |
| `POST /api/launch` | host, same-origin, paired, JSON | `launchTarget` (child of root, or home); mode validated; optional `resume` uuid |
| `POST /api/kill` | same | |
| `POST /api/settings` | same | `defaultPermissionMode` |
| `GET /*` | host | static assets, CSP as today (ws origin spelled out), `.`-prefixed components refused |

Every response: `Cache-Control: no-store`, `nosniff`, `Referrer-Policy:
no-referrer`, `Connection: close` for plain requests.

**Assets.** Embedded at build time from `../web` (`include_dir` or a small
`build.rs`); `CLAUDESHIP_WEB` overrides with a directory read per request,
for development. This ends page-versus-hub drift: the page always matches
the hub that serves it.

**State builder (`web/state.rs`).** Port `HubState.build`: directories under
`root`; `Scanner::scan()`; hub sessions matched to registry sessions by the
supervisor pid nearest in the ancestry (depth then pid); `starting` entries
for hub sessions Claude hasn't registered; `sub` for worktrees; per-project
`recent` (3 titled transcripts among the newest 8, excluding live ones,
title cache by mtime); `branch` from `.git/HEAD`; `elsewhere`; ordering by
`order()`. Same JSON keys and value types as today. Runs on
`spawn_blocking`, memoised 1 s, waiters coalesced.

**Token (`token.rs`).** `<home>/token`, 64 hex chars from `getrandom`,
written to `token.new` then renamed, 0600. `unlink` rotates and drops every
web connection.

Work items:

- [x] `web/server.rs`, `web/router.rs`, `web/ws.rs`, `web/assets.rs`,
      `web/state.rs`, `token.rs`, `build.rs`.
- [x] Golden test: with the Swift hub and the Rust hub pointed at the same
      registry and root (both idle, no hub sessions), `GET /api/state` from
      each differs only in `now` and `protocol`-bearing banner fields.
      Script it: `hub/tests/golden_state.sh` (manual, run during the overlap
      only; deleted in phase 5).
- [x] Integration tests: pairing flow; `/api/*` without cookie → 401; wrong
      Host → 403; cross-origin POST → 403; non-JSON POST → 403; WebSocket
      attach gets replay then live bytes; `claim=0` doesn't resize; a
      `resize` makes the sender the owner and the other client gets
      `owner:false`; stalled plain request swept at 15 s.
- [ ] Point the unchanged `web/` page at the Rust hub (private home,
      different port): launch, resume, attach, float, dock, pop out, End.
- [ ] Point the unchanged iPhone app at it over the tailnet: pair by QR,
      directory, attach, mirror-when-not-owner, key bar, end.

Acceptance: the page and the phone work against the Rust hub with no client
change; the golden diff is clean; `tailscale status --json` shows traffic
from the phone to the Rust hub's port.

---

## 5. Mac cutover

The Mac starts running the Rust hub for real, the Swift hub code is deleted,
and the Mac app becomes an HTTP client of the hub for the two things it
needs from it.

**`install.sh`/`build-app.sh`:**

- [x] `build-app.sh`: drop the `claudeship-cli` copy and its codesign line.
- [x] `install.sh`: `cargo build --release -p claudeship`; copy
      `target/release/claudeship` to `~/.local/bin/claudeship` (a real file,
      replacing the symlink into the bundle). Copy to a temp name and
      `mv` over, so a running hub keeps its mapped binary. Keep "the hub is
      never restarted by an install"; print the restart hint when
      `hub status` reports a different build (compare `protocol` and add a
      `build` string — git short hash — to the status reply).
- [x] `claudeship hub install-service` writes
      `~/Library/LaunchAgents/<bundle-id>.hub.plist` (`RunAtLoad`,
      `KeepAlive`, `ProgramArguments: [claudeship, hub, run]`, log to
      `hub.log`) and bootstraps it. `install.sh` calls it. `hub stop` under
      launchd means launchd restarts it — which is what "restart with the new
      build" wants; `hub stop --force` for a true stop should `bootout` first.
      Document that.

**Mac app (`Sources/ClaudeShip/`):**

- [x] New `HubClient.swift` (~120 lines): reads `hub/config.json` for the
      port and `hub/token` for the cookie; `GET /api/state` (1 s timeout) →
      `[pid: hubId]`; `POST /api/kill`. Replaces `HubCLI.liveSessions()` and
      `HubCLI.endHubSession()` in `SessionScanner.scan` and
      `AppDelegate.endSession`. `TerminalFocus` keeps typing
      `claudeship hub attach <id>` / `claudeship --permission-mode auto`
      into a new terminal window; `claudeship` is on PATH.
- [x] Delete `Hub.swift`, `HubPTY.swift`, `HubProtocol.swift`, `HubWeb.swift`,
      `HubState.swift`, `HubCLI.swift` (3,508 lines) and the hub sections of
      `SelfTest.swift` (~360 lines). Remove the `--cli` dispatch and the
      argv[0] dispatch from `App.main`.
- [x] `CLAUDE.md`: rewrite "Session hub" to point at `hub/` and
      `docs/hub.md`; the Rust rules (cargo test is fine; no `@State` rule
      doesn't apply there); keep the Mac-only sections.
- [x] README: install section (needs cargo), the Linux section placeholder.

Acceptance: `./install.sh` on a clean checkout produces a working menubar
app and a running Rust hub under launchd; the overlay's Virtual group and
End session work; `swift run ClaudeShip --self-test` green with the hub tests
gone; `claudeship hub status` shows the launchd-run hub; the Swift hub is
stopped and its binary gone from the bundle.

---

## 6. Linux hub

Mostly configuration of the seams built in phase 3.

- [ ] Build on the Linux box (`rustup`, `cargo build --release`). No
      cross-compilation: build on each machine.
- [ ] `procs.rs` Linux arm: `/proc/<pid>/stat` parse (the comm field can
      contain spaces and parens — parse from the last `)`).
- [ ] `net.rs` Linux arm: `getifaddrs` with the `tailscale*` prefix. Note
      Tailscale's userspace-networking mode has no interface; then
      `tunnelInterfaces` can't help and the user must use `tailscale serve`
      or loopback. Say so in the log line when a tailnet-range connection is
      refused and no tunnel interface exists.
- [x] `claudeship hub install-service` Linux arm: `~/.config/systemd/user/
      claudeship-hub.service` (`ExecStart=%h/.local/bin/claudeship hub run`,
      `Restart=on-failure`, `WantedBy=default.target`), then
      `systemctl --user daemon-reload && enable --now`. `loginctl
      enable-linger` is the user's call; print the hint.
- [ ] Web launches: confirm bash `-l -i` is quiet (no job-control warnings
      on a non-interactive-looking pty) and that `claude` resolves from the
      login shell's PATH on that box (npm global or `~/.local/bin`).
- [ ] Verify `ulimit -n` is raised; `/dev/ptmx` permissions; `IUTF8` on the
      slave.
- [x] `linux/scripts/install-hub.sh`: builds, installs to `~/.local/bin`,
      runs `install-service`, prints `hub link`.
- [ ] From the Mac: pair Safari with the Linux hub by its tailnet IP and
      run a session; from the phone (phase 9 gives it two hubs; until then,
      pair it to the Linux hub temporarily).

Acceptance: a session started from Safari on the Mac in a project on the
Linux box survives closing the tab, is visible from the phone, and
`claudeship` in a terminal on the Linux box behaves like it does on the Mac.

---

## 7. Approvals in the hub

Today the PermissionRequest hook helper talks to the menubar app over
`approvals.sock`. It moves to the hub so every client can answer.

**Hook helper (`claudeship permission-hook`).** Port `PermissionHook.swift`:
read stdin JSON, build the request line (`toolName`, `sessionId`, `cwd`,
`toolInput`), connect to `<home>/approvals.sock`, block with no deadline for
one verdict line; print the decision JSON only on an explicit verdict; every
failure → exit 0 with no output (terminal prompt wins). Never starts a hub.

**Hook installer (`claudeship hub install-hook`).** Port `HookInstaller`:
surgical, idempotent merge into `~/.claude/settings.json` of one
`PermissionRequest` entry whose command contains the `permission-hook`
marker, timeout 86400. Replaces any entry containing `--permission-hook`
(the Swift helper) in the same pass, so the Mac transition is one install.

**Hub side (`approvals.rs`).** Port `ApprovalServer` + the store's logic:
one connection per pending approval; `PendingApproval { id, session_id, cwd,
tool, summary, detail, received_at }` with the same `summary()` field
priority (`command`, `file_path`, `url`, `pattern`, `prompt`,
`description`, else compact JSON; 200 chars collapsed / 4000 chars raw);
`respond(id, allow)`; `cancel(id)` (EOF, no verdict); `on_closed` drops the
pending at once. Each state build runs the reconciliation: a pending whose
session left `waiting` with `statusUpdatedAt` newer than `received_at` was
answered in the terminal → `cancel`. Pendings with no matching live session
pruned after 10 s. Auto-approve rules per session, in memory only: 5
minutes or for-the-session, pruned on expiry or session end; a matching
pending is answered `allow` on arrival.

**API, protocol 3:**

- `/api/state`: each session entry gains `approvals: [{id, tool, summary,
  detail, receivedAt}]` and `autoApprove: null | {until: ms} | {session: true}`;
  the top level gains `approvalsSupported: true`. A session with pendings has
  `status: "waiting"` forced, as the Mac app does today.
- `POST /api/approve {id, allow}` → `{ok}` (404 if gone).
- `POST /api/auto-approve {sessionId, rule: "5m" | "session" | "off"}`.
- `PROTOCOL = 3` in the hub, `web/app.js`, `ios` `HubState.protocolVersion`,
  and the Mac app's client. The CLI and the page ship with the hub, so the
  only cross-version pairs are phone↔hub and Mac-app↔hub; both show their
  banner.

**Clients:**

- [x] Mac app: `SessionStore` drops `ApprovalServer` and reads pendings and
      rules from the hub state it already polls (2 s); Approve/Deny/⋯ call
      the two endpoints. Delete `ApprovalCenter.swift`, `PermissionHook.swift`,
      `HookInstaller.swift` and their tests; keep the pure `summary` test
      vectors in the Rust port. `install.sh` calls `claudeship hub
      install-hook` instead of `--install-hook`, and runs the old
      `--uninstall-hook` once if the old marker is present (or let the
      installer's replace-in-place handle it — pick one, test both states).
- [x] Web page: Approve / Deny / ⋯ in the session row where the status text
      sits, command summary as the caption, tooltip with the detail; the
      bolt mark for a standing rule. `textContent` only.
- [x] iPhone: the same on the session row; a confirmation on Deny is not
      needed (the terminal prompt stays live).
- [x] Hook timing caveat carried over: if no hub is running the hook finds
      no socket and exits silently, so approvals only exist while the hub
      runs — which the login service guarantees.

Acceptance: a `Bash` permission prompt in a session on either machine shows
Approve/Deny in the menubar overlay, the web page, and the phone within one
poll; answering in the terminal clears it everywhere within one poll;
"Approve all for 5 minutes" auto-answers the next prompt; `claudeship hub
install-hook` is idempotent and leaves other hooks untouched.

---

## 8. Linux tray applet (`linux/`)

A copy of torrent-flinger's Linux shape, with ClaudeShip's content. It is a
pure hub client: it reads nothing from `~/.claude` itself and needs the hub
running (it runs `claudeship hub start` at launch if the socket is absent).

```
linux/
  pyproject.toml            PySide6>=6.6; ruff config as torrent-flinger
  bin/claudeship-tray       launcher: venv, distrobox re-entry (copy of torrent-flinger's)
  claudeship-tray.desktop.in
  scripts/setup.sh          venv + PySide6
  scripts/install-linux.sh  .desktop into ~/.local/share/applications; --autostart
  shiptray/
    __main__.py             QT_QPA_PLATFORM=xcb;wayland under Wayland; try_forward; --smoke-test
    assets/                 tray-busy.svg, tray-waiting.svg, tray-idle.svg (ring), icon128.png from web/icon.svg
    core/                   pure stdlib, tested without Qt
      hub.py                HubClient: base URL + token from the hub's home (XDG_STATE_HOME or CLAUDESHIP_HOME);
                            GET /api/state, POST kill/launch/approve/auto-approve; urllib, 1 s timeouts
      model.py              parse /api/state → Session rows; grouping: Virtual (hub), Background, Terminal only;
                            stable order (cwd, pid)
      glyph.py              tray_glyph(sessions) → busy | waiting | idle   (orange beats green)
      formats.py            compact_age (mirror of Swift StatusFormat.compactAge), path abbreviation
      polling.py            VISIBLE_POLL_MS 2000, HIDDEN_POLL_MS 5000
    ui/
      app.py                ShipApp: tray icon + context menu (Open web app, New session in ~, Quit),
                            poll timer, event filter on the popup's Show/Hide, notifications on new approvals
      popup.py              Plasma anatomy: header (title, search, "+"), grouped list, footer
      session_row.py        name · path · title (italic) · branch; state + held-for; uptime; Approve/Deny/⋯ in
                            place of the status when approvals pending; right-click → End session
      style.py              palette-derived QSS, Kirigami units (copy)
      worker.py             run_async over QThreadPool (copy)
      single_instance.py    QLocalServer "claudeship-tray-<user>" (copy)
  tests/
    test_core.py            model, glyph, formats against canned /api/state JSON; a MockHub HTTPServer
    test_ui.py              offscreen popup grouping, approval buttons, full-app smoke against MockHub
```

Row click: a hub session opens a terminal running `claudeship hub attach
<id>` — Konsole via `konsole --workdir <cwd> -e claudeship hub attach <id>`,
else `$TERMINAL`, else `xdg-terminal-exec`. Background sessions: `claude
attach <jobId>` the same way. Terminal-only sessions: no focus (window
activation by pid on KWin/Wayland is its own project); the row says
"Terminal only" like the web page does. The footer's "+" POSTs a launch in
the home directory in auto mode and then attaches in a terminal.

Work items:

- [x] Scaffold by copying torrent-flinger's `linux/` plumbing files
      (launcher, setup, install, style, worker, single instance,
      `__main__`), renamed.
- [x] `core/` with tests against canned state JSON from both hubs.
- [x] `ui/` with offscreen tests.
- [x] Tray SVGs: three monochrome 16 px glyphs matching the Mac's drawn
      dots and ring; tinted from the palette like torrent-flinger's.
- [x] Autostart `.desktop` via `install-linux.sh --autostart`; the applet
      starts the hub if it isn't up (the systemd unit normally already has).
      (Written and tested offscreen on the Mac; the acceptance run on the
      Linux box is still to do.)
- [ ] Flatpak: later, if wanted. The hub must stay on the host (ptys,
      `~/.claude`), so the applet's sandbox would need only network and tray.

Acceptance: on the Linux box, the tray glyph follows the hub's sessions;
the popup lists them grouped; Approve/Deny works from the popup; a row click
attaches in Konsole; "+" starts a home session; it survives a Plasma
light/dark switch; `python -m unittest discover tests` and `ruff check` green.

---

## 9. iPhone: two hubs at once

- [x] `HubConnection` → `HubRegistry`: an ordered list of `Hub { id, name
      (the state's `host`), baseURL, token }`. Keychain item per hub id;
      UserDefaults holds the list. Pairing (QR, paste, URL scheme) adds or
      replaces by baseURL instead of overwriting the single hub.
- [x] One `HubStore` per hub; the directory shows a section per hub headed
      by its host name (or a picker in the top bar when there are several —
      try the sections first, they read better on a phone). Offline is per
      hub.
- [x] Session screens and launch/resume carry the hub id; the quick "+" asks
      which hub when there are two.
- [x] Settings: list of hubs with unpair.
- [x] `protocolVersion = 3`; approvals UI from phase 7 if not already done.

Acceptance: the phone paired with both hubs shows both directories, attaches
to a session on either, and shows "Can't connect to <host>" for one hub
while the other keeps working.

---

## Cross-cutting

**Protocol and build drift.** The Rust binary carries the CLI, the hub, the
supervisor, the hook helper, and the page, so drift reduces to: phone↔hub,
Mac-app↔hub, Linux-applet↔hub. All three read `protocol` from `/api/state`
and show a banner on mismatch. The supervisor is spawned from the installed
binary path, so after an install new sessions get the new supervisor while
the old hub keeps running — same as today; keep the note in the docs.

**Testing.** `cargo test --workspace` (unit + integration; integration tests
use a short scratch `CLAUDESHIP_HOME` and the stand-in program). The Swift
self-test keeps the Mac-only sections. The Python suite mirrors
torrent-flinger's. A `./test.sh` at the root runs all three where their
toolchains exist. Hand checks per phase are listed above; keep the hub ones
in `docs/hub.md` as the regression checklist (paste, Ctrl+Z, late attach
into the alt screen, two-screen size claim, kill escalation, phone over
tailnet, zero-bytes-from-phone diagnosis).

**Security invariants to re-verify after the port**, since they are the
point of the gate: loopback only to loopback; tailnet address must be one a
tunnel interface holds and the peer in the tailnet ranges; Host header never
a DNS name; cookie compared in constant time; Origin checked on the upgrade
and on POSTs; POSTs JSON-only; token file 0600 written atomically; socket and
lock 0600; `unlink` drops every connection. Each has a test today; each gets
one in Rust.

**Docs to write as the pieces land:** `docs/hub.md` (architecture, protocol,
the pty and replay reasoning moved out of `CLAUDE.md`), `docs/linux.md`
(hub install, applet, systemd), `docs/approvals.md` (hook, socket, API),
`docs/ios.md` (hubs list). `CLAUDE.md` shrinks to pointers plus the Mac-app
rules.

**Rough sizes.** Rust: ~3,500 lines of code and ~1,200 of tests across
phases 1–4 and 7; Linux applet ~1,800 lines of Python; Mac app −4,000 and
+150; iOS +400. Phases 1–4 are the bulk; 5 and 6 are small once the seams
exist; 7 is the only phase that touches every client at once, which is why
it sits after the Mac cutover and the Linux hub rather than before.

---

## 10. The swarm: hubs that know each other

Decided 2026-10-07. Any hub can show a client every machine in the swarm. One
secret per swarm; manual pairing only; the phone or the CLI is the introducer;
a client talks to one hub and that hub proxies to the others.

**Identity and membership.** Every hub mints a random `swarm.secret` at first
start (a swarm of one) and a `peers.json` holding its own record plus its
peers: `{id (uuid), name (hostname), addresses: [tailnet ip:port], protocol,
build, lastSeen, tombstone?}`. Peer requests carry the secret as a bearer
header and are checked in constant time; they also pass the existing
network gate (tunnel interface + tailnet ranges), which is unchanged.

**Joining.**
- `POST /api/swarm` (paired client) returns `{secret, peers}`; the phone or
  `hub pair` fetches this from a member.
- `POST /api/swarm/join {secret, peers}` on the joining hub: it adopts the
  secret, stores the peers, and sends `POST /peer/hello {record}` to each,
  authenticated with the secret; a member that accepts a hello records the
  newcomer. If the joining hub already had peers of its own, it sends them
  `POST /peer/rotate {newSecret, peers}` authenticated with the old secret —
  the same operation `hub unlink` uses to rotate a swarm's secret.
- CLI: `claudeship hub pair <link>` (the link from `hub link` on a member) does
  fetch + join. `claudeship hub peers` lists them; `hub unpair <name>` writes a
  tombstone that gossip carries; `hub unlink` rotates the swarm secret and the
  pairing token together (every browser, phone, and peer pairs again).
- Phone: scanning a second hub's QR pairs with it as today, then offers
  "Add to the swarm with <first hub>"; the app does the two calls above.
  The phone keeps its list of hubs — each is a possible home — and the
  directory switches to the swarm view from any of them.

**Gossip.** Each hub polls each peer's `GET /peer/state` every 2 s (the same
1 s cache serves it) and exchanges peer lists on every poll: union by id,
newest `lastSeen` wins, tombstones win over records and are kept 7 days.
Unreachable peers stay listed with `reachable: false` and their last state.

**No re-forwarding.** `GET /peer/state` and every `/peer/*` request answer
from local state only and never consult peers. Only client requests fan out.

**Aggregated state.** `/api/state` grows `hosts: [{id, name, local, reachable,
lastSeen, protocol, now, root, home, defaultPermissionMode, projects,
elsewhere, approvalsSupported}]`; the existing top-level fields stay as the
local host's for old clients. Clients keep one clock offset per host.

**Proxying.** `/ws/term?host=<id>&id=…` and `POST /api/launch|kill|settings|
approve|auto-approve` with a `host` field are relayed by the home hub to the
peer over the tailnet (bytes both ways for the WebSocket; the peer's reply
for the POSTs). The home hub refuses to proxy to a peer whose `protocol`
differs from its own and says which machine to restart; the client shows
that host's banner on its section. Approvals stay local to the hub whose
hook raised them; the home hub forwards only the button press.

**Clients.** Web page: a host section per reachable hub (name as header,
dimmed when unreachable), launch/attach/approve carrying the host id. Phone:
the same sections, from any paired hub; the pairing sheet gains the
"add to swarm" step. Linux applet and Mac overlay: local host only (they are
about the machine they run on).

Work items:

- [x] `swarm.rs`: secret, `peers.json`, records, tombstones, rotation.
- [x] `/api/swarm`, `/api/swarm/join`, `/peer/hello`, `/peer/rotate`,
      `/peer/state`, the bearer check, the local-only rule.
- [x] Gossip poller and the aggregated `hosts[]` build (per-host `now`).
- [x] Proxying: WebSocket relay and POST forwarding, protocol check
      (`web/proxy.rs`, `web/peer_api.rs`; tests in `hub/tests/proxy.rs`).
- [x] CLI: `hub pair`, `hub peers`, `hub unpair`, `hub unlink` rotation.
- [x] Web page host sections (done: `web/app.js`, per-host clock offsets, host id on
      every action, terminals, and saved windows; quick "+" is local-only); [x] phone (done: `ios/`, see
      `ios/CLAUDE.md`): "add to swarm" step after pairing a new hub and in
      Settings, host sections deduped by host id across paired hubs (local,
      else first reachable), per-host clocks and banners, `host` on actions
      and `/ws/term`, 409/502 alerts naming the machine. Verified against two
      mock hubs in the simulator, not yet against the real hub; the models
      decode a live two-hub `/api/state` (final review).
- [x] Tests: two private hubs on one Mac (loopback peers allowed under a
      test knob, since the gate wants tunnel addresses) — join, gossip,
      tombstone, rotation, proxy attach, proxy approve, no re-forwarding
      (a peer request must never produce a peer request), skew refusal.
      (Done in `hub/tests/swarm.rs`: join, gossip, tombstone, rotation, no
      re-forwarding, bearer, gate; proxy attach/approve and skew refusal
      in `hub/tests/proxy.rs`.)

Acceptance: phone paired with the Mac sees the Steam machine's and the NAS's
sessions, attaches to one on the NAS, approves a prompt there; the Mac asleep,
the phone switches to the Steam hub and still sees the NAS; `hub unpair` on
one machine removes it everywhere within a few polls.

---

## 11. Jobs: a Claude session that drives a Claude session elsewhere

Decided 2026-10-07: arbitrary `argv` jobs allowed; MCP tool names prefixed `ship_`; a wall-clock cap per job (below). A session on one machine can start a
headless Claude run on another swarm member, wait for it, read its output,
and continue the same remote conversation later. Claude Code on the
orchestrating machine reaches this as MCP tools; the `claudeship` command
offers the same for scripts and for Bash-tool use.

**Why jobs and not terminals.** A `-p` run on pipes gives clean, machine-
readable output (`--output-format stream-json`) and an exit code; a pty
gives a TUI meant for eyes. Multi-turn comes from Claude Code itself: the
first run's `session_id` is resumed by the next (`--resume`), so the
remote side keeps its context between jobs.

**Job model (`hub/src/jobs.rs`).**
`Job { id (6 hex like sessions), host-local, cwd, argv, permissionMode,
startedAt, finishedAt?, exitCode?, pid, stdout: bounded buffer (4 MB,
oldest dropped, `truncated` flag), stderr: 256 KB, claudeSessionId? (parsed
from stream-json `result`/`system` events when argv[0] is claude) }`.
Spawned with pipes (no pty; `std::process::Command`, `kill_on_drop`), the
same whitelisted login-shell environment as a web launch, cwd restricted
by `launch_target` **or** any directory under `root` (jobs run in
subfolders and worktrees; the request body is still untrusted — canonicalise
and require the `root` prefix or the home directory). Finished jobs are
kept 1 h or until read with `?consume=1`, at most 64 per hub (oldest
finished evicted). `hub stop` kills running jobs' process groups.
**Wall clock:** every job has `maxSeconds` (default 1800, request may set
up to 14400; `config.jobsMaxSeconds` raises the ceiling); on expiry the
job's process group gets SIGTERM, then SIGKILL 10 s later, `exitCode` 124
and `timedOut: true`, and the output so far is kept. `ask` uses the same
cap; the MCP `ship_ask`/`ship_wait` calls return early with the job handle
well before it.

**Endpoints** (same gates as every POST; `host` proxied like Phase 10):
- `POST /api/jobs {host?, cwd, argv: [..] | prompt + claude options,
  permissionMode?, resume?: uuid, env?: {k: v} whitelist}` → `{id, host}`.
  The convenience form builds `claude -p <prompt> --output-format stream-json
  --permission-mode <mode> [--resume <id>]`; `argv` runs anything (trust
  statement below).
- `GET /api/jobs?host=` → list; `GET /api/jobs/<id>?host=&wait=<s>&since=<n>`
  → `{id, running, exitCode, claudeSessionId, stdout (from byte n), stderr,
  truncated}`; `wait` long-polls up to 60 s for a change (finish or new
  output); `POST /api/jobs/<id>/kill`.
- Peer arms: `/peer/api/jobs…` on `LocalOnly`.

**CLI.**
```
claudeship run [--host <name|id>] [--cwd <dir>] [--mode auto] -- <argv…>     # prints the job id
claudeship ask [--host …] [--cwd …] [--mode …] [--resume <uuid>] "<prompt>"   # start, wait, print result text, exit with its code
claudeship jobs [--host …]                                                    # list
claudeship jobs wait|output|kill <id> [--host …]
```
`ask` prints the remote Claude's final text (the `result` event) to stdout
and the stream to stderr with `-v`, and prints the session id on stderr so a
follow-up can `--resume` it.

**MCP server (`claudeship mcp`, stdio JSON-RPC).** Registered once:
`claude mcp add claudeship -- claudeship mcp`. Tools:
- `ship_hosts()` → the swarm's `hosts[]` (name, id, reachable, protocol, root).
- `ship_run(host, cwd, argv, mode?, maxSeconds?)` → `{job}`;
  `ship_wait(host, job, timeoutSeconds?)` → status + new output;
  `ship_output(host, job, since?)`; `ship_kill(host, job)`.
- `ship_ask(host, cwd, prompt, mode?, resume?, maxSeconds?)` → `{result,
  sessionId, exitCode, job}`: starts the job and waits up to the MCP call's deadline, returning
  `{job, running: true}` if it is not done, so the caller `wait`s again.
- `ship_sessions(host?)` → the live sessions from `hosts[]`, for "is anything
  already running there".
Every tool answer is JSON text; errors are MCP errors naming the host.
The server talks to the local hub over loopback with the token file,
exactly like the Mac app.

**Permissions.** A `-p` run cannot answer prompts. The orchestrator chooses
the mode per job; default `auto`. Under `manual`/`plan` the remote hub's
approval bridge still raises the prompt, so a person can approve from the
phone or web page while the job waits — the job's `wait` reports
`waitingFor` from the registry when the run has registered a session.
The Claude Code hook (`claudeship permission-hook`) is per machine and
already installed by `install-hub.sh`/`install.sh`.

**Trust, stated plainly.** `argv` jobs are remote command execution on a
member, authorised by the swarm secret or the member's pairing token. The
swarm already permits this (a bypass-mode launch on a peer); jobs make it
explicit. `docs/hub.md` says so, and `hub status` prints "jobs enabled"
only when `config.jobs` is true (default **false** until the owner turns it
on per machine — the one new switch).

**Watching (later).** A `watch: true` flag would mirror a job's output into
a hub session so it can be attached to from the page or phone. Not in this
phase.

Work items:

- [x] `jobs.rs`: spawn on pipes, bounded buffers, stream-json session-id
      parse, eviction, kill, `hub stop` cleanup; `config.jobs` switch.
- [x] Endpoints + peer arms + proxy (long-poll `wait` through the relay
      path — a plain forwarded GET with the 60 s budget). (`web/jobs.rs`;
      a `wait` is exempt from the server's 15 s sweep for its length.)
- [x] CLI `run`, `ask`, `jobs …` (`cli/jobs_cmd.rs`).
- [x] `claudeship mcp`: stdio server, the seven `ship_*` tools, `claude mcp add`
      line in docs; a smoke test that drives it with a scripted JSON-RPC
      client.
- [x] Tests (private hubs, stand-in in place of claude producing stream-json
      lines): run/wait/output/kill, truncation flag, eviction, `consume`,
      a job through a peer, `wait` long-poll returns on finish and on new
      output, cwd outside root refused, `config.jobs=false` → 403, `hub
      stop` kills jobs, the wall-clock cap (SIGTERM then SIGKILL, 124,
      `timedOut`), MCP tool round-trips. (Jobs: `hub/tests/jobs.rs`; MCP:
      part B's smoke test.)
- [x] Docs: `docs/jobs.md` (model, endpoints, CLI, MCP, the trust
      statement, an example orchestration: build here, test there, merge).

Acceptance: from a Claude session on the Mac with the MCP server
registered, "build this branch here, push it, then have the Steam machine
pull it, run the suite, and report" runs end to end, with the remote
session resumed for a follow-up question; a `manual`-mode remote job's
permission prompt is answered from the phone.

---

## Remaining — needs the owner

- Phase 3 side-by-side: drive real `claude` through a private Rust hub for a working day.
- Phase 3: the unchanged web page against the Rust hub in a real browser (launch, resume, attach, float, dock, pop out, End).
- Phase 3: the unchanged iPhone app against it over the tailnet (QR pair, attach, mirror, key bar, end).
- Phase 5 cutover: stop the old Swift hub, run `./install.sh`, verify (see `docs/cutover.md`).
- Phase 6: build and run the hub on the Linux box (`procs.rs`/`net.rs` Linux arms, bash `-l -i` quiet, `claude` on PATH, `ulimit -n`, `/dev/ptmx`, `IUTF8`).
- Phase 6: pair Safari and the phone with the Linux hub by its tailnet IP.
- Phase 8: the Flatpak question, if ever wanted.
- Phase 10 acceptance: three real machines (Mac, Steam box, NAS) — the phone sees, attaches, and approves on the NAS via the Mac, then via the Steam hub with the Mac asleep; `hub unpair` removes one everywhere within a few polls.
