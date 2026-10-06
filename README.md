# Claude Ship

Run your [Claude Code](https://claude.com/claude-code) sessions from anywhere: a macOS menu bar app that shows what every session is doing, a session hub that lets one session be driven from your terminal, a browser, and your iPhone at the same time, and the clients to do it.

Everything stays on your Mac. The only thing on the network is the hub, and it answers only to this machine and to your Tailscale network.

## The pieces

| | What it is |
|---|---|
| **Menu bar app** (`ClaudeShip.app`) | One glyph summarising all live sessions; click it for the list, with Approve/Deny for permission prompts and click-to-focus to raise a session's terminal. |
| **`claudeship`** | Run Claude through the hub: `claudeship` in a project folder does exactly what `claude` does, but the session lives in the hub, outlives the window, and can be attached to from anywhere. |
| **Web app** | A directory of your projects with live sessions and conversation summaries; launch, resume, and attach to a terminal in the browser — on the Mac or on a phone over Tailscale. |
| **iPhone app** (`ios/`) | The same, native: SwiftUI directory, SwiftTerm terminal with a key bar, native scrolling. |

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
claudeship hub unlink         # new pairing secret: every browser and phone must pair again
claudeship hub stop           # stop the hub (ends its sessions); it restarts on next use
```

Closing a `claudeship` window only detaches: the session keeps running and shows in the web app, the phone, and the menu bar. Any number of screens can look at one session; the one you last used sets its size, the others follow.

The hub starts on first use and keeps running until you log out or stop it. It is never restarted by `install.sh`, so it keeps its sessions across installs; restart it with `claudeship hub stop` when its sessions can end.

## Web app

`http://localhost:7433` on the Mac, or `http://<tailscale-ip>:7433` from another device on your tailnet (`claudeship hub status` prints both). Pair each browser once: `claudeship hub link` prints a link to open, and a QR code for a phone.

Projects are the folders in `~/Documents/code`. Running ones come first, with live status and the conversation's summary; the rest list their last conversation. **New session** starts Claude in **auto** mode by default (change it in the gear menu, or per launch from the arrow); **Resume** picks up an earlier conversation. Sessions started with plain `claude` in a terminal are listed too, but can only be used there.

Security: the hub accepts connections only from this Mac or from Tailscale addresses on its tunnel interface, only by `localhost` or IP literal (no DNS names, so no rebinding), and every request must carry the pairing secret. Treat the pairing link like a password.

## iPhone app

`ios/ClaudeShip.xcodeproj`. Open it in Xcode, choose your phone, Run (your team is set in the project). Then **Scan the QR code** from `claudeship hub link`, or paste the link. Needs full Xcode and its Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`); `ios/build.sh` does a simulator build.

The phone takes the session's size whenever you look at it; drag to scroll Claude's transcript, with momentum; the key bar above the keyboard has esc, tab, ⇧tab, ^C, arrows, page up/down, and return.

## Install

```bash
./install.sh
```

Builds a release bundle, installs `/Applications/ClaudeShip.app`, links `~/.local/bin/claudeship`, starts the hub, registers the permission hook, and installs the VS Code bridge extension (`SKIP_HOOK=1`, `SKIP_VSCODE_EXT=1`, `SKIP_HUB=1` opt out of each). Upgrading from the app's former name (ClaudeStatus) is handled automatically: its settings and the hub's pairing move with it.

Code signing is best-effort: set `SIGN_IDENTITY` in a `.env` file for a stable identity (keeps macOS from re-asking for Automation permission after every rebuild); otherwise ad-hoc.

## Development

```bash
./build-app.sh                     # release build → ./ClaudeShip.app
swift run                          # dev loop (menubar app, unsigned)
swift run ClaudeShip --self-test   # the test suite
swift run ClaudeShip --scan        # headless: print detected sessions
swift run ClaudeShip --cli hub status   # the claudeship command, from a dev build
```

Tests are a hand-rolled assertion harness baked into the binary (`SelfTest.swift`) — no XCTest or swift-testing dependency, so the Mac app builds with a Command Line Tools-only toolchain. `CLAUDE.md` has the design notes: how the hub owns ptys, how late attachers get a correct screen, the size rules between screens, and the security gate.

The build scripts and visual style are shared with [claude-usage](https://github.com/jakeonrails/claude-usage) — the two apps are meant to sit side by side in the menu bar.
