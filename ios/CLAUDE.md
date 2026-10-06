# CLAUDE.md — iPhone companion

Guidance for Claude Code when working in `ios/`. The hub, its web app, and
the menubar app are described in [`../CLAUDE.md`](../CLAUDE.md); this is
only the phone's shell over the same hub API.

## What this is

`ClaudeHub` is a SwiftUI iPhone app — a hand-written Xcode project
(`ClaudeHub.xcodeproj`, synchronized root folder, so adding a file needs no
project edit) that does what the web app does, natively: the project
directory with live sessions, launch/resume in a chosen permission mode,
settings, and a terminal attached to a session over the hub's WebSocket.
The terminal is [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)
(pinned to 1.20.0; its build plugin needs `-skipPackagePluginValidation`,
its Metal shaders need Xcode's Metal toolchain: `xcodebuild
-downloadComponent MetalToolchain` once).

Personal build. iOS 17+, iPhone only, signed with the user's own team and
installed from Xcode. Plain `@State` is fine here (full Xcode builds it);
the Mac app's `@ViewState` rule does not apply.

## Common commands

```bash
./build.sh                 # simulator build (CODE_SIGNING_ALLOWED=NO)
./build.sh run             # …then install + launch on the booted simulator
open ClaudeHub.xcodeproj   # device install: Run with the team under Signing
swift ios/make-icon.swift  # from the repo root: regenerate the app icon
```

**Device installs go through Xcode, not `xcodebuild`** (the Apple ID lives
in Xcode's session). **Needs full Xcode** for the iOS SDK.

## How it talks to the hub

- `App/HubConnection.swift` holds the hub's base URL (UserDefaults) and the
  pairing token (Keychain; an unsigned simulator build has no keychain —
  `errSecMissingEntitlement` — and falls back to UserDefaults, see
  `Keychain.swift`). Every request carries `Cookie: claude_hub=<token>`,
  so the hub treats the phone exactly like a paired browser. No cookie
  jar: `httpShouldSetCookies = false`.
- Pairing = the link `claudeandrew hub link` prints: scanned (AVFoundation
  QR, `QRScannerSheet`), pasted, or `claudehub://pair?link=<url-encoded>`.
  `HubConnection.parse(link:)` takes the host/port and `k=`.
- `App/HubAPI.swift` mirrors `/api/state`, `/api/launch`, `/api/kill`,
  `/api/settings` (models decode the JSON `HubState.build` produces; keep
  `HubState.protocolVersion` equal to `HubFrame.version`). `HubStore`
  polls every 2 s while the app is active, same as the page.
- `Terminal/TerminalSession.swift` is the attachment: `URLSessionWebSocketTask`
  to `/ws/term?id&rows&cols&claim`, binary frames fed to SwiftTerm, JSON
  control frames the other way. Size rules are the web page's (CLAUDE.md
  "One pty, one size"): first connect claims, reconnects don't, `resize`
  when this screen owns the size and `fit` when it doesn't, a `size`
  message with `owner:false` and a foreign grid shrinks the font so that
  grid fits and shows "Fit here". Heartbeat `ping`/`pong` every 15 s;
  `revive()` on return to the foreground. `HubTerminalView` (a `TerminalView`
  subclass) owns scrolling while the program has mouse reporting on —
  Claude Code's full-screen TUI — where SwiftTerm would otherwise send a
  finger drag as mouse-drag reports: it disables SwiftTerm's drag
  recognizer and the scroll view's own pan, and turns vertical drags into
  wheel events (`Terminal.sendEvent`, buttons 64/65, encoded in whatever
  mouse protocol the program negotiated) with scroll-view-style momentum.
  `linesPerWheelTick` is the feel knob. With reporting off, SwiftTerm's
  native scrollback scrolling is back. `KeyBar` is the row above the
  keyboard (esc, tab, ⇧tab, ^C, arrows, pgup/pgdn, ⏎); arrows honour
  application-cursor mode.
- Launch arguments for a scripted simulator (there is no way to tap the
  custom-URL "Open?" prompt from outside): `-pair <link>`, `-session <hub
  id>`; `SIMCTL_CHILD_CH_OFF=glyph,launch,header,tally` turns directory
  pieces off for bisecting layout trouble. Screenshots:
  `xcrun simctl io booted screenshot x.png`.
