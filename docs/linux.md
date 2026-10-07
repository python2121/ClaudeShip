# ClaudeShip on Linux

Two pieces: the **hub** (`claudeship`, the Rust build, which owns the
sessions) and the **tray applet** (`linux/`, Python + PySide6, which shows
them). The applet only talks to the hub, so the hub has to be installed first.

## The hub

`linux/scripts/install-hub.sh` (needs cargo from https://rustup.rs): it
builds, installs `claudeship` to `~/.local/bin`, registers and starts the
`claudeship-hub` systemd user unit (`claudeship hub install-service`),
registers Claude Code's PermissionRequest hook (`claudeship hub
install-hook`; `SKIP_HOOK=1` skips it), and prints the `claudeship hub link`
pairing URLs. For the hub to outlive your login session (and start at boot):
`loginctl enable-linger $USER`. See [hub.md](hub.md) for the hub itself.

**SteamOS / distrobox.** Put everything in one box (claude, cargo, the hub,
the applet) and run `install-hub.sh` inside it: `install-service` sees the
box and writes a host `systemctl --user` unit that re-enters it
(`distrobox-enter -n <box> -- env … claudeship hub run`). Set
`TERMINAL=ghostty` (or whichever terminal the box has) so row clicks open
one. Don't `distrobox-export --bin` the binary — `$HOME` is shared, and the
wrapper would overwrite `~/.local/bin/claudeship`. Linger is a host setting:
`distrobox-host-exec loginctl enable-linger $USER`.

The hub keeps its socket, config and pairing secret in its **home**:
`$CLAUDESHIP_HOME` if set, else `$XDG_STATE_HOME/claudeship`, else
`~/.local/state/claudeship`. The applet reads the same directory.

**Several machines.** To put this hub in a swarm with the Mac's (or any
other member's), run `claudeship hub pair <link>` here with the link the
member's `claudeship hub link` prints, or pair both hubs on the phone and
use its "Add to the swarm" step; `claudeship hub peers` lists the members.
The web page and the phone then show every machine's sessions from either
hub (see "The swarm" in [hub.md](hub.md)).

## The tray applet (`linux/`)

A `QSystemTrayIcon` with a popup styled to pass for a KDE Plasma applet, the
counterpart of the Mac menu bar app. Its plumbing is a copy of
torrent-flinger's `linux/` (launcher, setup, install script, theming, worker
pool, single instance); the content is ClaudeShip's.

Everything below is relative to `linux/`, which is self-contained: the venv,
the package and its tests all live there.

```bash
cd linux
./scripts/setup.sh                       # .venv + PySide6 + ruff, nothing system-wide
./bin/claudeship-tray                    # run it
./scripts/install-linux.sh               # application-menu entry (dev paths)
./scripts/install-linux.sh --autostart   # …and start it at login
PYTHONPATH=. .venv/bin/python -m unittest discover tests
.venv/bin/ruff check shiptray tests
```

Inside a distrobox, `setup.sh` records the box's name and the launcher
re-enters it when started from the host, so the menu entry and autostart work
from the host session.

### What it does

- **Tray glyph**: an orange dot when any session waits on you (a pending
  permission request counts), else a green dot when any is busy, else a ring
  in the panel's text colour. Same precedence as the Mac; `shell` and
  `starting` are not alarms, and unknown statuses read as idle. The three
  `assets/tray-*.svg` are monochrome masks tinted at runtime, re-tinted when
  the palette changes (a Breeze light/dark switch shows within one poll).
- **Popup** (left-click the tray icon): header with the hub's host name, a
  search filter and **＋** (a new session in your home folder, auto mode,
  started through the hub and then attached in a terminal); the sessions,
  grouped **Virtual** (the hub's own), **Background** (Claude Code daemon
  sessions) and **Terminal only** (plain `claude` in some terminal), in stable
  (cwd, pid) order; a footer with counts and a button for the web app.
- **Rows**: project name, path, conversation title (italic), branch; state and
  how long it has held, uptime. A pending permission request replaces the
  status with **Approve** / **Deny** / **⋯** ("Approve all for 5 minutes",
  "Approve all for this session", "Stop approving"), with the request's
  summary under them and its full text as the tooltip. A ⚡ marks a standing
  auto-approve rule.
- **Clicking a row**: a hub session opens a terminal running `claudeship hub
  attach <id>` in its folder (see **Which terminal** below); a background session runs `claude attach
  <jobId>` the same way; a terminal-only session does nothing (raising another
  app's window by pid on KWin/Wayland is its own project).
- **Right-click a row**: End session (hub sessions only), and Stop
  auto-approving when a rule is set.
- **Tray menu**: Open ClaudeShip, New session in ~, Quit. Open ClaudeShip
  (here and in the popup's footer) opens the pairing link
  `http://localhost:<port>/auth?k=<token>`, so a browser that has never seen
  the hub pairs itself on the way in.
- **Notifications**: one desktop notification when a new permission request
  appears; clicking it opens the popup.
- **Polling**: every 2 s while the popup is open, 5 s while it isn't, off the
  UI thread.
- **Hub start**: at launch, if `hub.sock` is missing from the hub's home, the
  applet runs `claudeship hub start` once. Normally the systemd unit already
  has.

A hub speaking a different protocol (the applet speaks 3) shows a banner in
the popup; nothing else changes.

### How it talks to the hub

A pure hub client: it never reads `~/.claude`. From the hub's home it reads
`config.json` (for `port`, default 7433) and `token`, re-read on every poll so
a `claudeship hub unlink` or a port change needs no restart. Requests go to
`http://localhost:<port>` with `Cookie: claude_ship=<token>` — the cookie a
paired browser holds — and no Origin header, which the hub accepts from
non-browsers. They never go through a proxy: urllib would otherwise honour
`$http_proxy` for localhost and hand the cookie to it. It reads only the
state's top-level fields — this machine's sessions — and ignores the
swarm's `hosts[]`, so its requests never name a `host`: the applet is about
the box it runs on.

| Call | When |
|---|---|
| `GET /api/state` | every poll (1 s timeout) |
| `POST /api/approve {id, allow}` | Approve / Deny |
| `POST /api/auto-approve {sessionId, rule: "5m"\|"session"\|"off"}` | the ⋯ menu |
| `POST /api/kill {id}` | End session |
| `POST /api/launch {path: <state.home>, permissionMode: "auto"}` | ＋ |

Every string from the hub (titles, branches, command summaries) is shown
through `Qt.PlainText` labels, never as rich text. Tooltips have no
plain-text mode (Qt renders anything that looks like HTML), so untrusted
tooltip text goes in HTML-escaped (`plain_tooltip`), as does a notification's
body, which notification servers read as markup.

### Layout

| Path | Contents |
|---|---|
| `shiptray/__main__.py` | Entry point. Forces XWayland (`QT_QPA_PLATFORM=xcb;wayland`) under Wayland unless set, so the popup can anchor to the tray and lose focus properly; forwards to a running instance (`try_forward`); `--smoke-test` runs three seconds and quits without starting the hub. |
| `shiptray/core/` | Pure stdlib, tested without Qt. `hub.py` (home resolution, `HubClient`), `model.py` (`/api/state` → `Session` rows, grouping, approvals), `glyph.py` (tray glyph precedence), `formats.py` (`compact_age` etc., mirroring Swift `StatusFormat`), `polling.py`, `terminal.py` (which terminal command, binary lookup with a `~/.local/bin` fallback for desktop sessions without it on PATH). |
| `shiptray/ui/` | PySide6. `app.py` (`ShipApp`: tray, menu, poll timer driven by an event filter on the popup's Show/Hide, actions, notifications), `popup.py`, `session_row.py`, and the copied `style.py`, `worker.py`, `single_instance.py`. |
| `shiptray/assets/` | `tray-busy.svg`, `tray-waiting.svg`, `tray-idle.svg`; `icon128.png`, rendered from `web/icon.svg` with QtSvg. |
| `tests/` | `test_core.py` (parsing canned protocol-3 and protocol-2 state, grouping, glyph, `compact_age` vectors from `SelfTest.swift`, terminal choice, `HubClient` against an in-process mock hub that records POSTs); `test_ui.py` (offscreen popup, approval buttons reaching the mock hub, the whole app with spawning injected, and a `--smoke-test` run). |

The tests never start a hub or a terminal: `ShipApp` takes `spawn` and
`open_url` as parameters, and the mock hub's home is a temp directory.

### Which terminal

A row click picks the first that applies (`shiptray/core/terminal.py`):

1. `$TERMINAL`, if set and on PATH, run as `$TERMINAL <workdir flag> -e <command…>`.
   Known flags: ghostty, foot, alacritty (`--working-directory`), konsole
   (`--workdir`), kitty (`--directory`); any other emulator gets the directory
   by `sh -c 'cd "$1" && shift && exec "$@"'` inside the command. A value not
   on PATH is skipped. `TERMINAL=ghostty` makes the choice explicit.
2. `ghostty` on PATH.
3. `konsole` on PATH. Inside a container (`/run/.containerenv` or
   `/.dockerenv` exists) with no `konsole` on the box's PATH, the host's
   Konsole runs as `distrobox-host-exec konsole --workdir <cwd> -e
   distrobox-enter -n <box> -- <command…>`; the box is `$CONTAINER_ID`, else
   the `name=` line of `/run/.containerenv`.
4. `xdg-terminal-exec`.

Arguments stay a list throughout; no shell string is built.
