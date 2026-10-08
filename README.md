# ClaudeShip

Run your [Claude Code](https://claude.com/claude-code) sessions from anywhere: a macOS menu bar app that shows what every session is doing, a session hub that lets one session be driven from your terminal, a browser, and your iPhone at the same time, and the clients to do it.

Everything stays on your Mac. The only thing on the network is the hub, and it answers only to this machine and to your Tailscale network.

## The pieces

| | What it is |
|---|---|
| **Menu bar app** (`ClaudeShip.app`) | One glyph summarising all live sessions; click it for the list, with Approve/Deny for permission prompts and click-to-focus to raise a session's terminal. |
| **`claudeship`** | Run Claude through the hub: `claudeship` in a project folder does exactly what `claude` does, but the session lives in the hub, outlives the window, and can be attached to from anywhere. |
| **Web app** | A directory of your projects with live sessions and conversation summaries; launch, resume, and attach to a terminal in the browser — full screen, or several at once in floating windows over the directory — on the Mac or on a phone over Tailscale. |
| **iPhone app** (`ios/`) | The same, native: SwiftUI directory, SwiftTerm terminal with a key bar, native scrolling, and a zoomed-out mirror of whatever screen currently owns a session. |
| **Linux tray applet** (`linux/`) | The menu bar app's counterpart for KDE Plasma (PySide6): the same glyph and session list, grouped Virtual / Background / Terminal only, with Approve/Deny, End session, and click-to-attach in Konsole. A pure hub client. See [docs/linux.md](docs/linux.md). |

## Menu bar

The glyph:

- **● green** — one or more sessions are busy
- **● orange** — one or more sessions are **waiting on your input**
- **○** — everything is idle (or nothing is running)

Click it for the overlay: each session's project, branch, path, state and how long it has held, uptime, and its conversation summary. Sessions are grouped by the app they run in (Ghostty, VS Code windows, …); sessions on the hub appear under **Virtual**, and clicking one opens a terminal window attached to it. Right-click any row to **End session**.

When a session asks for permission, the row shows **Approve** / **Deny** where its status was, plus a `⋯` menu with "Approve all for 5 minutes" and "Approve all for this session". The terminal prompt stays live too; answer in whichever is closer. `./install.sh` registers the required hook (`PermissionRequest` in `~/.claude/settings.json`; `SKIP_HOOK=1` to opt out).

## The hub and `claudeship`

```bash
claudeship                    # start Claude here, through the hub (any claude arguments pass through)
claudeship --resume <id>      # resume a conversation — or attach, if it's already running in the hub
claudeship hub status         # the hub, its web address, and its sessions
claudeship hub link           # pairing link + QR code for a browser or the phone
claudeship hub attach <id>    # open a running session in this terminal
claudeship hub kill <id>      # end a session
claudeship hub unlink         # new pairing and swarm secrets: every browser, phone, and peer hub must pair again
claudeship hub pair <link>    # join the swarm of the hub whose `hub link` this is (see "Several machines")
claudeship hub peers          # the hubs in this one's swarm
claudeship hub unpair <name>  # drop a hub from the swarm, everywhere
claudeship hub stop           # stop the hub (ends its sessions); it restarts on next use
```

Closing a `claudeship` window only detaches: the session keeps running and shows in the web app, the phone, and the menu bar. Any number of screens can look at one session; the one you last used sets its size, the others follow.

The hub runs as a login service (`claudeship hub install-service`, which `install.sh` runs) and otherwise starts on first use. It is never restarted by `install.sh`, so it keeps its sessions across installs; restart it with `claudeship hub stop` when its sessions can end (under the login service, launchd starts the new build).

## Web app

`http://localhost:7433` on the Mac, or `http://<tailscale-ip>:7433` from another device on your tailnet (`claudeship hub status` prints both). Pair each browser once: `claudeship hub link` prints a link to open, and a QR code for a phone.

Projects are the folders in `~/Documents/code`. Running ones come first, with each session's live status and conversation summary; the rest show their name, branch, and when they were last used. Expand a project (or **Earlier** on a running one) for its recent conversations. **New session** (a **+** on a running project's card) starts Claude in **auto** mode; **Resume** picks up an earlier conversation, also in auto. The permission mode is changed inside the session: **Mode** beside **End** in the terminal bar cycles it, as shift+tab does. The **+** in the top bar starts a session in your home folder, in auto mode, for work that isn't about any one project. Sessions started with plain `claude` in a terminal are listed too, but can only be used there.

A session opens full screen. Its bar has minimize, window, pop out, and close: **window** turns it into a floating panel over the directory — drag it by its bar, resize it by the corner, open another session and window that too — **minimize** sends it to a dock along the bottom, and **pop out** gives it a browser window of its own, with a button to pop it back in. Each window is its own connection, so the one you last typed in or resized sets the session's size and the others follow. Floating windows come back after a reload. On a phone (or any narrow window) sessions are full screen or minimized, and the window controls stay out of the way. Close detaches; **End** ends the session.

Security: the hub accepts connections only from this Mac or from Tailscale addresses on its tunnel interface, only by `localhost` or IP literal (no DNS names, so no rebinding), and every request must carry the pairing secret. Treat the pairing link like a password.

## Several machines

Hubs on several machines (a Mac, a Linux box, …) can form a **swarm**, so any one of them shows every machine's sessions. Pair them by hand, any of three ways: on the joining machine run `claudeship hub pair <link>` with the link a member's `claudeship hub link` prints; pair both hubs on the phone and take its "Add to the swarm" step; or, in the web page of any member, open the gear menu's **Computers…**, paste the other machine's `claudeship hub link` link, and press Add. That panel also lists every machine (addresses, reachable or since when not, a mark for one on another build) and removes one. A browser or phone paired with any member then sees a section per machine — unreachable ones dimmed with their last-known sessions — and launches, attaches, approves, and ends sessions on any of them through the hub it is paired with, which relays over the tailnet. The menu bar app and the Linux applet stay about the machine they run on. All members must run the same build; a hub won't relay to one on another and says which to restart.

Trust: there is one secret per swarm, and members trust each other alike — anyone paired with any member can enrol more machines. `claudeship hub unpair <name>` removes a machine from every member's list within a few polls; to lock it out for good, follow it with `claudeship hub unlink` and pair the rest again. Details in [docs/hub.md](docs/hub.md) ("The swarm").

## iPhone app

`ios/ClaudeShip.xcodeproj`. Open it in Xcode, choose your phone, Run. First put your team and a bundle id of your own in `ios/Signing.local.xcconfig` (gitignored; see `ios/Signing.xcconfig`). Then **Scan the QR code** from `claudeship hub link`, or paste the link. Needs full Xcode and its Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`); `ios/build.sh` does a simulator build.

The phone takes the session's size whenever you look at it, and when another screen owns it, shows that screen's exact grid zoomed out rather than reflowing it. Drag to scroll Claude's transcript, with momentum. The key bar above the keyboard has mode (cycles Claude's permission mode, as shift+tab does), esc, tab, ⇧tab, ^C, arrows, page up/down, and return, plus a key to put the keyboard away (a downward swipe does it too). Sessions started from the phone or the web page begin in auto mode; the mode is changed inside the session (the phone's mode key, the web terminal's Mode button beside End). When the Mac stops answering, the directory says so calmly and fills back in when it returns.

## Install

```bash
./install.sh
```

Needs Swift (the Command Line Tools are enough) and Rust (`cargo`, from [rustup.rs](https://rustup.rs)). Builds the menubar app and installs `/Applications/ClaudeShip.app`; builds the hub (`cargo build --release -p claudeship`) and installs it as `~/.local/bin/claudeship`; installs the hub's login service (`claudeship hub install-service`), registers the permission hook (`claudeship hub install-hook`), and installs the VS Code bridge extension (`SKIP_HUB=1`, `SKIP_HOOK=1`, `SKIP_VSCODE_EXT=1` opt out of each). A running hub is never restarted by an install.

**Coming from the Swift hub** (before the Rust one): stop the old hub by hand before the first install, when its sessions can end — `claudeship hub stop` while `~/.local/bin/claudeship` still points into the old bundle.

Code signing is best-effort: set `SIGN_IDENTITY` in a `.env` file for a stable identity (keeps macOS from re-asking for Automation permission after every rebuild); otherwise ad-hoc. Set `BUNDLE_ID` there too (default `com.example.claudeship`); changing it on an installed app resets its settings.

## Linux

The hub runs on Linux too (a systemd user service), and a KDE Plasma tray applet mirrors the menu bar app.

```bash
./linux/scripts/install-hub.sh      # cargo build, ~/.local/bin/claudeship, systemd user unit, prints the pairing link
cd linux && ./scripts/setup.sh && ./scripts/install-linux.sh --autostart   # the tray applet
```

Pair a browser or the phone with `claudeship hub link` (use the box's tailnet address). The applet is described in [docs/linux.md](docs/linux.md); a headless container: [docs/containers.md](docs/containers.md).

## Development

```bash
./build-app.sh                     # release build → ./ClaudeShip.app
swift run                          # dev loop (menubar app, unsigned)
swift run ClaudeShip --self-test   # the test suite
swift run ClaudeShip --scan        # headless: print detected sessions
cargo run -p claudeship -- hub status  # the claudeship command (hub/), from a dev build
```

Tests are a hand-rolled assertion harness baked into the binary (`SelfTest.swift`) — no XCTest or swift-testing dependency, so the Mac app builds with a Command Line Tools-only toolchain. The hub (Rust) has its own `cargo test`. `CLAUDE.md` has the menubar app's design notes; `docs/hub.md` the hub's: how it owns ptys, how late attachers get a correct screen, the size rules between screens, and the security gate.

The icon — a starship in Claude's colour with the Claude mark's burst as its exhaust — is one drawing, `web/icon.svg`, rendered for every surface by `ios/make-icon.swift` and `make-mac-icon.sh`.

The build scripts and visual style are shared with [claude-usage](https://github.com/python2121/claude-usage) — the two apps are meant to sit side by side in the menu bar.
