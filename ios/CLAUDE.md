# CLAUDE.md — iPhone companion

Guidance for Claude Code when working in `ios/`. The hub, its web app, and
the menubar app are described in [`../CLAUDE.md`](../CLAUDE.md); this is
only the phone's shell over the same hub API.

## What this is

`ClaudeShip` is a SwiftUI iPhone app — a hand-written Xcode project
(`ClaudeShip.xcodeproj`, synchronized root folder, so adding a file needs no
project edit) that does what the web app does, natively: the project
directory with live sessions, launch/resume in a chosen permission mode,
settings, and a terminal attached to a session over the hub's WebSocket.
The terminal is [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)
(pinned to 1.20.0; its build plugin needs `-skipPackagePluginValidation`,
its Metal shaders need Xcode's Metal toolchain: `xcodebuild
-downloadComponent MetalToolchain` once).

Personal build. iOS 17+, iPhone only, signed with the user's own team and
installed from Xcode. Team and bundle id live in `Signing.local.xcconfig`
(gitignored), included by `Signing.xcconfig` (the target's base config,
neutral defaults); never put them back in the project file. The keychain
service and URL name follow the bundle id. Plain `@State` is fine here (full Xcode builds it);
the Mac app's `@ViewState` rule does not apply.

## Common commands

```bash
./build.sh                 # simulator build (CODE_SIGNING_ALLOWED=NO)
./build.sh run             # …then install + launch on the booted simulator
open ClaudeShip.xcodeproj   # device install: Run with the team under Signing
swift ios/make-icon.swift  # from the repo root: regenerate the app icon from web/icon.svg
```

**Device installs go through Xcode, not `xcodebuild`** (the Apple ID lives
in Xcode's session). **Needs full Xcode** for the iOS SDK.

## How it talks to the hub

- **Several hubs.** `App/HubRegistry.swift` keeps an ordered list of
  `Hub { id, name, baseURL }` in UserDefaults (`hubs`) and one Keychain
  item per hub (`token.<uuid>`, service = bundle id; an unsigned simulator
  build has no keychain — `errSecMissingEntitlement` — and falls back to
  UserDefaults, see `Keychain.swift`). `name` is the state's `host`,
  refreshed on each successful poll. The single-hub keys of older builds
  (`hubURL`, Keychain `token`, `lastHost`) are migrated once. Each hub gets
  its own `HubConnection` (base URL + token: every request carries
  `Cookie: claude_ship=<token>`, so the hub treats the phone exactly like a
  paired browser; no cookie jar, `httpShouldSetCookies = false`) and its own
  `HubStore` (polling, offline clock, notices, launching — all per hub).
- Pairing = the link `claudeship hub link` prints: scanned (AVFoundation
  QR, `QRScannerSheet`), pasted, `claudeship://pair?link=<url-encoded>`,
  or `-pair <link>` (repeatable). `HubConnection.parse(link:)` takes the
  host/port and `k=`. Pairing **adds** a hub, or gives the hub with the same
  base URL the new token (same id, place, name). `PairView` is the whole
  app when nothing is paired or the only hub refuses the token (exactly the
  old behaviour); otherwise it is a sheet — "Pair another hub" in Settings,
  "Pair again" in a refused hub's section. Settings: one hub → its settings
  inline as before; several → a list (swipe or open one to unpair; the
  default mode is per hub).
- The directory is Running on top, then Projects (the web page's
  layout, `docs/ios-directory-tabs.md`): one run of running sections per
  hub (`HubSections`), headed by a `HubTitle` row only when two or more
  are paired, then one Projects block listing one machine's idle
  projects at a time — a tab per computer (`ProjectsTab`: segmented up to
  three, a menu beyond; label = name · idle count after the filter,
  dimmed when unreachable) when several are in view, remembered in
  `@AppStorage("projectsTab")` (store UUID + host id; a vanished tab
  falls back to the first). With one machine there is no picker and the
  directory looks as it always did. Offline (`OfflineNote`), refused, and
  the protocol banner are per hub. Session screens are `SessionRoute { hub, host, id }`; RootView
  puts that hub's store in the environment. The top bar's quick "+" asks
  which machine (confirmation dialog) when two or more are in view.
- **Swarm (phase 10).** A hub's `/api/state` may carry `hosts: [HubHost]`
  (every machine of its swarm, local first; each field optional, absent
  on an older hub, which then reads as one host: the top-level fields,
  `HubState.localHost`). The Running part renders per paired hub, then
  per host (`HostSections(part: .running)`): a `HostTitle` header (the
  hub's own machine marked "hub") only when the hub shows more than its
  own machine; an unreachable host says "Unreachable since <lastSeen>"
  (and "Nothing was running when it was last seen"), its Projects tab is
  dimmed and its list (`HostSections(part: .projects)`) disabled; a
  protocol banner per host (vs this app, and vs the home hub, which
  won't relay across builds).
  **Dedupe** (`HubRegistry.hostOwners`): every paired member reports the
  whole swarm, so each host id appears once — under the paired hub where
  it is `local`, else the first (pairing order) reporting it `reachable`,
  else the first listing it. Only hubs answering count, so when the
  phone can't reach one hub its machine shows under another; its
  "Can't connect" note stays. Clocks are per host (`HubStore.hostNow`,
  keyed by host; learnt only while reachable). Rows read the host from
  the `hostScope` environment; launch/resume/kill/approve/auto-approve
  and the quick "+" (a picker over every reachable host when more than
  one) send `host` **only for a peer** (`HubHost.target` is nil for the
  answering hub's own machine, so an older hub never sees the field);
  `SessionRoute` and `/ws/term?host=` carry it too. `-session` and
  `claudeship://session?id=` search every host of every hub
  (`HubRegistry.route(for:)`). A 409 (`protocol mismatch`) or 502
  (`unreachable`) from a relayed action becomes `HubError.proxy` → a
  root-level alert naming the machine (`HubStore.alert`); a 409 on the
  WebSocket upgrade stops retrying with the same words.
- **Swarm enrolment** (`SwarmView`, `HubRegistry.addToSwarm`): after a
  *new* hub is paired from the sheet and another paired, swarm-capable
  hub isn't already in a swarm with it, the sheet's second step offers
  "Add <new> to the swarm with <existing>" (a picker with several; "Not
  now" leaves it standalone); also Settings → a hub → "Add to swarm
  with…". It POSTs `/api/swarm` `{}` to the member, then
  `/api/swarm/join {secret, peers}` to the joining hub, peers passed
  through untouched. The secret lives only in that call — never logged,
  shown, or stored. Errors name the hub that failed and stay in the sheet.
- `App/HubAPI.swift` mirrors `/api/state`, `/api/launch`, `/api/kill`,
  `/api/settings`, `/api/approve`, `/api/auto-approve` (models decode the
  JSON the hub's `hub/src/web/state.rs` produces; keep
  `HubState.protocolVersion` equal to the hub's `PROTOCOL` in
  `hub/src/frame.rs` — 3). Every protocol-3 field is optional, so an older
  hub still decodes. `HubStore` polls every 2 s while the app is active,
  same as the page.
- **Approvals** (protocol 3): a session entry carries `sessionId`,
  `approvals: [{id, tool, summary, detail, receivedAt}]`, and
  `autoApprove: {until | session}`. A row with pendings stops being a
  NavigationLink (its own buttons must take their taps): the title area
  and a trailing bordered "Open ›" button beside the answers both open the
  session, so attaching stays one obvious tap. Approve / Deny / ⋯ sit where the status line was,
  the first prompt's `summary` (which the hub already prefixes with the
  tool's name, "Bash: npm test") is the caption, tapping it opens
  `ApprovalDetail` (the full detail, Approve/Deny at the bottom). ⋯ holds
  "Approve all for 5 minutes", "Approve all for this session", and "Stop
  approving" when a rule is set; the hub answers what's already pending
  for the session when a rule is set, so the app sends nothing more. A yellow bolt marks an active
  rule. No confirmation on Deny — the terminal prompt stays live, and a
  404 from `/api/approve` (answered elsewhere) is not an error.
- `Terminal/TerminalSession.swift` is the attachment: `URLSessionWebSocketTask`
  to `/ws/term?id&rows&cols&claim`, binary frames fed to SwiftTerm, JSON
  control frames the other way. Size rules are the web page's (CLAUDE.md
  "One pty, one size"): any connect while the app is in front claims (background reconnects don't) and coming back to the foreground claims outright, `resize`
  when this screen owns the size and `fit` when it doesn't, a `size`
  message with `owner:false` and a foreign grid **mirrors** it: the view is
  framed to precisely cols × rows cells inside a host view
  (`TerminalContainer`/`TerminalSession.layout`) at the smallest font
  (half-point steps, 5–12 pt) that overflows the room, then scaled down
  with a transform to fill the limiting dimension exactly — cell sizes are
  pixel-snapped, so no font size lands on the edge and the scale is what
  closes the gap. Zoomed out, never reflowed, so bytes laid out for the
  other screen land where it drew them. `cellSize(fontSize:)` replicates SwiftTerm's internal cell
  computation (line height, "W" advance, pixel-snapped); keep them in
  step if SwiftTerm changes. `measure()` derives this screen's own grid
  from the host, not the terminal, which may be on the foreign grid.
  "Fit here", typing, or coming to the front claims the size and the
  normal font returns. Heartbeat `ping`/`pong` every 15 s;
  `revive()` on return to the foreground. `HubTerminalView` (a `TerminalView`
  subclass) owns scrolling while the program has mouse reporting on —
  Claude Code's full-screen TUI — where SwiftTerm would otherwise send a
  finger drag as mouse-drag reports: it disables SwiftTerm's drag
  recognizer and the scroll view's own pan, and turns vertical drags into
  wheel events (`Terminal.sendEvent`, buttons 64/65, encoded in whatever
  mouse protocol the program negotiated) with scroll-view-style momentum.
  `linesPerWheelTick` is the feel knob (1: measured, Claude Code scrolls one line per wheel event, answering in 3–26 ms on the Mac — what's left of the lag is the tailnet round trip, Claude's per-event redraw, and SwiftTerm's 60 fps coalescing). For truly local, native scrolling, a session can run Claude's classic non-fullscreen TUI (`/tui default` in the session, or `--settings '{"tui":"default"}'` at launch): the transcript then lives in the terminal's own scrollback. With reporting off, SwiftTerm's
  native scrollback scrolling is back. `KeyBar` is the row above the
  keyboard (esc, tab, ⇧tab, ^C, arrows, pgup/pgdn, ⏎); arrows honour
  application-cursor mode. A keyboard-down key is pinned at its left,
  outside the scrolling row; it, a downward swipe on the bar, and
  `keyboardDismissMode = .interactive` on the terminal (effective only
  on the normal screen, where the scroll view's pan is live) all lower
  the keyboard, and a tap on the terminal brings it back (SwiftTerm takes
  focus on tap).
- When the Mac stops answering, `HubStore.offline` turns on 5 s after
  the first failed poll (`offlineAfter`; a success in between cancels it,
  so one dropped poll on flaky Wi-Fi changes nothing) and the directory
  swaps that hub's rows for `OfflineNote` ("Can't connect to <host>"). The data
  is kept, so the rows return the moment a poll succeeds. The clock is
  cancelled when polling stops (background), so the app never wakes to
  a stale "offline".
- Directory rows show name, branch, and when; a project's conversations
  appear only in its expanded resume list (`recentRows`). A running
  project's card reaches that list through `EarlierRow`.
- Launch arguments for a scripted simulator (there is no way to tap the
  custom-URL "Open?" prompt from outside): `-pair <link>` (repeat it for
  several hubs; a tiny Python server answering `/api/state` on two ports
  is enough to lay out the multi-hub directory), `-session <hub
  id>`, `-expand <project name>`, `-swarm` (open the add-to-swarm step for
  the last hub paired) / `-swarm-confirm` (…and press Add); `SIMCTL_CHILD_CH_OFF=glyph,launch,header,tally,running` turns directory
  pieces off for bisecting layout trouble (`running` hides the Running
  part, which puts the Projects tabs on the first screen); `-projectsTab
  <store uuid>/<host id>` preselects a tab. Screenshots:
  `xcrun simctl io booted screenshot x.png`.
