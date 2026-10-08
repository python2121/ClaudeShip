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
claudeship hub link | unlink        pairing: print the links + QR / rotate the secret (and leave the swarm)
claudeship hub pair <link>          join the swarm of the hub whose link this is
claudeship hub peers                the swarm: this hub and its peers
claudeship hub unpair <name | id>   drop a hub from the swarm (a tombstone gossip carries)
claudeship hub start | stop [--force] | restart [--force]
claudeship hub attach <id | uuid>   open a running session in this terminal
claudeship hub kill <id>            end a session
claudeship hub run                  internal: be the hub
claudeship hub supervise <argv>     internal: session leader on a pty
claudeship hub install-hook | uninstall-hook          the PermissionRequest hook in Claude's settings
claudeship hub install-service | uninstall-service [--no-load]   the hub as a login service
claudeship permission-hook          the hook helper (Claude Code runs it; see approvals.md)
claudeship run | ask | jobs …       jobs here or on a peer (see jobs.md)
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
`config.json` (`jobs`, default false, and `jobsMaxSeconds` switch on
and cap jobs — `jobs.md`; `name` is what this machine is called on every
surface — the state's `host`, the swarm's record of it, so peers, the
phone, and the web page all show it — else the hostname; one line, at
most 64 characters, read at start), `token` (the pairing secret: 64 hex characters from
`getrandom`, written to `token.new` and renamed, 0600), `approvals.sock`
(0600, bound under the same umask as `hub.sock`), `swarm.secret` and
`peers.json` (the swarm, below; both 0600, written the same atomic way). `CLAUDESHIP_CMD`
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
| `swarm.rs` | the swarm's book: `swarm.secret`, `peers.json`, records, the merge rules, tombstones, each peer's last state, `hosts` — no network |
| `swarm/client.rs` | the peer HTTP client (hand-rolled over tokio's `TcpStream`), counted |
| `swarm/gossip.rs` | `Swarm` (book + client): the poller, join, rotate |
| `web/peer.rs` | the `/peer/*` routes, on `LocalOnly` state (no peer client) |
| `web/peer_api.rs` | `/peer/api/*` and `/peer/ws/term`: a peer's relayed action or terminal, on `LocalOnly` |
| `web/proxy.rs` | a client's action or terminal naming another host: route, protocol check, POST forwarding, the WebSocket relay |
| `cli/pair.rs` | `hub pair`, `hub peers`, `hub unpair` |
| `jobs.rs` | jobs (`jobs.md`): spawn on pipes under the login-shell recipe in a group of its own, bounded stdout/stderr, the stream-json session id, the wall clock, retention and eviction, `hub stop`'s kill |
| `web/jobs.rs` | the jobs routes, shared by `/api/jobs…` and `/peer/api/jobs…`; `config.jobs` off → 403 |
| `cli/jobs_cmd.rs` | `claudeship run`, `ask`, `jobs …` over loopback HTTP |
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
| `POST /api/settings` `{defaultPermissionMode}` or `{jobs: bool}` | same; `{jobs}` also needs a loopback connection | `{ok}`, saved to `config.json`; `{jobs}` is the live jobs switch (`jobs.md`): `{ok, jobs, ended}`, from this machine only — a non-loopback client, a `host` naming a peer, and `/peer/api/settings {jobs}` are all 403 |
| `POST /api/approve` `{id, allow}` | same | `{ok}`; 404 no such approval; 400 |
| `POST /api/auto-approve` `{sessionId, rule}` | same | `{ok}` (`5m`, `session`, `off`); 400 |
| `POST /api/swarm` `{}` | same | `{secret, peers}` (see "The swarm") |
| `POST /api/swarm/join` `{secret, peers}` | same | `{ok, id, hello: [{id, name, ok, error}], rotated: [{id, name, ok}]}`; 400 |
| `POST /api/swarm/invite` `{link}` | same | `{ok, name, id}`; `{error}` with 400 (bad link), 403 (its key refused), 409 (other protocol, or no swarm), 502 (not reached) |
| `POST /api/swarm/unpair` `{id}` | same | `{ok, record}`; 400 (not an id, or this hub), 404 |
| `GET /api/swarm/peers` | host, paired | `{self, peers: [{record, reachable, refused}]}` — what `hub peers` prints |
| `/peer/hello`, `/peer/rotate`, `/peer/state` | host, swarm bearer (401 `{"error":"not a peer"}`), POSTs JSON with no foreign Origin (403) | see "The swarm" |
| `POST /peer/api/launch\|kill\|settings\|approve\|auto-approve`, `GET /peer/ws/term?id&rows&cols&claim` | same (the upgrade needs no Origin) | as `/api/…` and `/ws/term`; 400 if the request names a `host` |
| `POST /api/jobs`, `GET /api/jobs`, `GET /api/jobs/<id>?since&wait&consume`, `POST /api/jobs/<id>/kill` (each with `host`) | host, paired; POSTs same-origin JSON | jobs, see `jobs.md`: remote command execution, off unless `config.jobs` (403 `{"error":"jobs disabled on this host"}`); a `wait` long-poll (≤ 60 s) is exempt from the 15 s sweep |
| `GET\|POST /peer/api/jobs…` | as `/peer/api/*` | as `/api/jobs…`, run here; 400 if it names a `host` |
| `GET /*` | host | the page's files: `.`-prefixed or empty components refused, html/js/css/svg/png/json/webmanifest only |

Every answer but the 101 carries `Cache-Control: no-store`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`Connection: close` (the router's header is what closes the connection, not hyper's `keep_alive(false)`: that appends `close` to the 101 as well, and Apple's URLSession — the phone's WebSocket — rejects a handshake that says `Connection: upgrade, close`); the page's files add the CSP (`connect-src` spells out
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

## The swarm

Hubs that know each other (plan phase 10): any hub can show a client every
machine in its swarm. One secret per swarm, manual pairing only.

**Files.** `swarm.secret`: 64 hex characters, minted at first start (a
swarm of one). `peers.json`: `{self, peers}`, each a record `{id (UUID v4),
name (hostname), addresses ["ip:port"], protocol, build (crate version),
lastSeen (epoch ms), tombstone? (epoch ms)}`. Our own record is rebuilt
whenever it is sent: the tailnet addresses held by the tunnel interfaces
(`tunnelInterfaces`, not any 100.64/10 a Wi-Fi hands out) with the web
port, plus `127.0.0.1:<port>` when `CLAUDESHIP_ADVERTISE_LOOPBACK=1` (the
test knob: two hubs on one machine then peer over loopback, which the gate
already allows loopback-to-loopback). Written on every membership change,
and for `lastSeen` alone at most once a minute.

**Merge rules** (`swarm::merge`, unit-tested): union by id; of two live
records the newer `lastSeen` wins; a tombstone beats a record whatever its
`lastSeen` (of two tombstones the later is kept); tombstones are dropped
after 7 days. Our own id is never a peer — but a tombstone of it means a
peer unpaired us: the hub *leaves* (new id, new secret, no peers), so it
stays gone and a later `hub pair` makes it a new member. Answers to a
request made with a secret that has changed since are ignored (a poll in
flight across a leave can't bring the old swarm back; a 401 to one across
a rotation isn't counted as a refusal).

**Records from the wire** (`sanitized`, `admissible`): ids must be UUIDs;
addresses must be `ip:port` literals in the tailnet ranges (loopback only
under the test knob) — anything else is dropped, or a join, hello, or
gossiped record could make this hub send the secret to any host — and the
client also refuses to dial a tailnet address while no tunnel interface
holds one (Tailscale down: 100.64/10 would route to the local network);
names lose control characters. `lastSeen` is clamped to our now (a clock
ahead of ours can't pin a record or make a dead peer look just seen); a
tombstone already past 7 days by our clock is ignored (a lagging peer
can't bring back one we forgot); an unknown record unseen for 7 days is
ignored (it may be one whose tombstone we forgot). A peer answering a poll
or hello directly is believed about itself — name, addresses, protocol,
build — whatever its `lastSeen`, since our record of it carries our clock
and its own may lag.

**Auth.** Every `/peer/*` request needs `Authorization: Bearer <swarm
secret>`, compared in constant time, and passes the same network gate and
Host check as everything else (a peer is a tailnet address, or loopback
under the knob). A POST must be JSON, and an Origin, if any, ours.

**No re-forwarding.** `/peer/*` handlers answer from local state only:
their axum state is `web::peer::LocalOnly` (hub command channel, state
cache, the `Book`), which has no way to reach a peer — the client lives in
`swarm::Swarm`, held only by `Shared` (client-facing handlers) and the
poller. `hub status --json` reports `swarmId` (this hub's id) and `peerRequests` (every outgoing peer
request and connection, counted in `PeerClient`); the tests hold it at 0
across peer requests to a hub with `CLAUDESHIP_GOSSIP=0` (no poller).

**Endpoints.**
- `POST /api/swarm` (paired client): `{secret, peers: [ours, then every
  peer's, tombstones included]}` — what a joiner needs.
- `POST /api/swarm/join {secret, peers}` (paired client, on the joining
  hub): if we had peers of our own not in that swarm, `POST /peer/rotate`
  to them with our *old* secret (so our old swarm merges into the new one);
  adopt the secret; merge the records; `POST /peer/hello` to each member.
  If that swarm carries a tombstone of our id, we take a new id first.
- `POST /api/swarm/invite {link}` (paired client, on a member): enrol
  another computer from here — the web page's Computers panel. A browser
  paired with us can't post to the other hub (its Origin check refuses a
  foreign page, on purpose), so we follow its pairing link ourselves.
  The link must be exactly `http://<ip literal>:<port>/auth?k=<key>`
  (`swarm::parse_invite_link`, unit-tested): no DNS name (we are about to
  hand over the swarm secret, and whoever runs DNS could point a name
  anywhere), no other scheme or path, the address a peer address
  (tailnet, or loopback under the test knob) — anything else is 400 before
  any connection. Then, over `PeerClient::visit` (counted, dialable
  addresses only, the cookie in place of the bearer): `GET /auth?k=` →
  the 303's cookie (403 if refused); `POST /api/swarm` there for its
  record (409 if its protocol differs or it has none); `POST
  /api/swarm/join {secret: ours, peers: our records}` there, so it joins
  our swarm and greets every member, us included (502 if any step isn't
  reached). Its record is merged here as well, in case its hello didn't
  reach us. The key is never logged or echoed.
- `POST /api/swarm/unpair {id}`: `hub unpair` by full id, for the page.
- `GET /api/swarm/peers`: the socket's `peers` view, for the page.
- `POST /peer/hello {record}` → `{record: ours, peers}`: the newcomer is
  recorded (its `lastSeen` = our now); 410 if it is tombstoned here.
- `POST /peer/rotate {newSecret, peers}` (bearer: the old secret) →
  `{ok}`: adopt the secret, merge the records.
- `GET /peer/state` → the local `/api/state` JSON (the same 1 s cache, no
  `hosts`) plus `record` (ours) and `peers` (our records).

**Gossip** (`gossip.rs`). Every 2 s, every peer not tombstoned is polled
at once: `GET /peer/state` at each of its addresses in turn (the one that
answered last first), 3 s for all of them. An answer's `record` and
`peers` are merged; if the record is that peer's, it is marked reachable,
its `lastSeen` set to our now, and the rest kept as its last state. A 401
marks it refused (logged once: it must pair again); no answer, unreachable.
The state is in memory only.

**Aggregated state.** `/api/state` keeps every top-level field as the local
host's (old clients) and adds `hosts: [{id, name, local, reachable,
lastSeen, protocol, now, root, rootDisplay, home, defaultPermissionMode,
projects, elsewhere, approvalsSupported}]` — this hub first, then peers by
name (then id). A peer's fields are from its last state (its own `now`,
carried forward by the time since that poll, for a per-host clock offset); an unreachable one keeps them with `reachable:
false`; one never reached has empty `projects`/`elsewhere`. `protocol`
stays 3 (the fields are additive). Built per request from the cached local
state and the book: nothing waits on a peer.

**Commands.** `hub pair <link>` (the `http://…/auth?k=…` link a member's
`hub link` prints): `GET /auth` on the member for the cookie, `POST
/api/swarm` there, then `POST /api/swarm/join` to this machine's hub over
loopback (`127.0.0.1:<port>`, this hub's own token from the socket's `link`
op as the cookie) — the same route the phone uses. `hub peers`: a table
(name, addresses, reachable, last seen, protocol, tombstoned), from the
socket's `peers` op. `hub unpair <name | id | id prefix>` (socket op
`unpair`): tombstones it here; gossip carries it. `hub unlink` also leaves
the swarm — new swarm secret, no peers, id kept — told to nobody, so every
old peer is refused (401) until one of them runs `hub pair` with this hub's
new link (its own old peers come along by `/peer/rotate`). To evict a
machine for good: `hub unpair` it, then `hub unlink`, then re-pair the rest.

**Trust.** Members are trusted alike: any member (or paired client) can add
records, and every hub then sends the swarm secret to those addresses.
`unpair` is membership, not revocation (the unpaired hub still knows the
secret until `unlink`).

**Proxying.** `POST /api/launch|kill|settings|approve|auto-approve` take
an optional `host` (an id from `hosts[]`), and `/ws/term` a `host` query
parameter; absent, empty, or this hub's id means here. Otherwise
`web/proxy.rs`: the book's route (404 `{error: "no such host"}` for an
unknown or tombstoned id, or a `host` that isn't a string); the peer's
protocol — from its last state, else its record — against ours (409
`{error: "protocol mismatch", host, theirs, ours}`, decided before any
connection); then its addresses, last good first, 3 s each (502 `{error:
"unreachable", host}` when none answers; a peer that answers 401 counts as
unreachable, since to a client a 401 means *it* is unpaired, and is marked
refused in the book). A POST goes to the peer's `/peer/api/<same path>`
with the bearer and the body minus `host`, and its status and body come
back verbatim. Only an address that refuses the *connection* moves on to
the next: once one is connected it settles the request, answer or not — a
request sent and then unanswered (3 s) may have run there (a launch whose
answer was lost), so it is a 502 and never sent elsewhere
(`PeerClient::connect` then `exchange`). After a forwarded POST the home
hub fetches that peer's `/peer/state` once (1 s cap, inside the request;
the peer invalidated its own cache) and folds it into the book as a poll
would (`answered_by`, with the peer's own record), so the client's next
`/api/state` shows the launch or kill without waiting for gossip. A terminal: the WebSocket handshake to
the peer's `/peer/ws/term?id&rows&cols&claim` is done by hand over
`swarm.client.connect` (bearer in the request, `Sec-WebSocket-Accept`
checked) *before* the client's upgrade, so a refusal is still a plain
HTTP answer; then messages are relayed one for one, binary and text
alike, never parsed (`tokio-tungstenite` speaks the client side to the
peer). Each direction is its own pump holding one message at a time (one
loop doing both would deadlock on a big paste while the peer is writing
output: each side blocked writing to the other): a client that stops
reading backs up into the peer's socket, where the peer's 32 MB queue cap
and stall sweep apply, and each pump drops a side that takes nothing for
60 s, as `ws.rs` does. A close on either side is passed on with its code
(the session's exit → the peer's 1000 → the client's), and a new pairing
secret on the home hub drops relays like any terminal. The peer side
(`web/peer_api.rs`) runs on `LocalOnly`: the same handlers as `/api/…`
(`router::local_action`) and the same attachment as `/ws/term`
(`ws::run_on`, bearer instead of cookie, no Origin check), and a request
naming a `host` is refused (400), so nothing a peer asks is forwarded
again. A peer's terminal also hangs up when this hub's swarm secret
changes (it joined another swarm, left, or rotated: `Book::secret_epoch`),
so a hub the swarm has moved on from keeps no session open here.

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

`swarm.rs` (in `hub/tests/`) runs two or three private hubs with
`CLAUDESHIP_ADVERTISE_LOOPBACK=1`: a phone-style join (`/api/swarm` from
A posted to B's `/api/swarm/join`) and both listing each other, a third
hub joining through B by `hub pair` and appearing on A, an
`/api/swarm/invite` enrolling B from A (a DNS name, `https`, another path,
a non-tailnet address → 400 with `peerRequests` unchanged; a wrong key →
403; a stopped target → 502) and `/api/swarm/unpair` removing it, `hosts[]` with B's
session and B's `now` from A, an unreachable peer keeping its last state,
`hub unpair` of a stopped hub reaching the third and the unpaired one
leaving when it comes back, `unlink`'s rotation (the old secret refused,
re-paired by `hub pair`), the bearer (missing, wrong by one character,
other schemes, a cookie → 401; non-JSON or foreign-Origin POSTs → 403),
`/peer/*` dropped unanswered over the LAN address, and no re-forwarding
(`peerRequests` stays 0 across peer requests to a hub with no poller and a
peer at a closed port).

`proxy.rs` (in `hub/tests/`) runs two such hubs and acts on B through A:
a launch with `host` (B's answer, B's session in A's `hosts[]` at once,
B's own 400 verbatim), a relayed terminal (size, replay, typing, ping),
kill through A closing that terminal with 1000, a relayed resize claiming
the size on B over a direct screen, the client's close releasing B's
attachment and the session's exit closing the relay, approve (the real
helper on B, the decision JSON), auto-approve and settings through A, and
the refusals: 404 unknown host (POST and upgrade), 400 a `/peer/api/*` or
`/peer/ws/term` naming a host (with no peer request made by B), 401 with
no bearer, 502 with B stopped, 409 with B's protocol edited in A's
`peers.json` (and no connection attempted).

`jobs.rs` (in `hub/tests/`) runs private hubs with `"jobs": true` and the
stand-in's job modes (`-p` stream-json, `sleep`, `ticks`, `ignore-term`):
run/wait/output/kill over HTTP and the CLI, the session id parsed and a
`--resume` follow-up carrying it, `waitingFor` from a registry entry,
`ask` end to end (result, `session:`, `-v`, exit code, the cap's 124),
truncation at 4 MB, `consume`, the keep time (`CLAUDESHIP_JOBS_KEEP_SECONDS`,
a test knob) and eviction at 64, `wait` waking on output and on the finish
past the 15 s sweep, the cwd rules and `env`, `config.jobs` off → 403
everywhere, `hub stop` killing jobs, the wall clock (SIGTERM; SIGKILL 10 s
later for one that ignores it), and a job relayed through a peer with a
long-poll past the proxy's 3 s and `ask --host`.

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
10. **Swarm.** Two private hubs (own homes and ports,
   `CLAUDESHIP_ADVERTISE_LOOPBACK=1`): `hub pair <A's /auth?k= link>` on B,
   `hub peers` on both shows the other reachable; A's `/api/state` lists B
   in `hosts[]`; launch on B through A (`host`), attach through A
   (`/ws/term?host=`), approve a `permission-hook` request raised on B
   through A, kill through A; `hub unpair <B's id>` on A drops B from A's
   `hosts[]` at once and B's `hub peers` shows no peers a poll later.
