# iOS: Running on top, Projects tabbed by computer

Status: **to do** (written on Linux, which can't build the iOS app). The web
page already has this layout; make the phone's directory match it. Mostly a
reorganization of `ios/ClaudeShip/Views/DirectoryView.swift`; no API or model
changes.

## What the web page does now (`web/app.js`, `render` → `runningAll`, `projectTabs`)

With one computer in view, nothing changed. With several:

1. **Running** comes first: one segment per computer, this hub's own first
   (the hub already sends it first; the page also sorts `local` to the
   front). Each segment is the computer's name line (name, "This hub" tag,
   "Unreachable since …"), its version banner if any, then its running
   project cards plus an "Elsewhere" card for sessions outside the projects
   folder. A computer with nothing running shows one quiet line ("Nothing
   is running." / "Nothing was running when it was last seen.").
2. **Projects** comes below: a tab per computer, each tab labelled with the
   computer's name and its count of idle projects (after the filter). Only
   the chosen tab's idle projects are listed, with its root folder in the
   section header and the "Projects are the folders in …" footer. The
   chosen tab is remembered per browser (`localStorage`
   `claudeship.projectsTab`); a vanished computer falls back to the first
   tab. An unreachable computer's tab is dimmed and its list disabled.
3. The filter applies to both: a computer with no running match drops out
   of Running; tab counts show where the matching idle projects are.

## Suggested phone change

Today `DirectoryView` → `HubSections` (per hub) → `HostSections` (per
host) renders Running, Running elsewhere, and Projects together for each
host, so the list grows by a whole project list per machine.

- Split `HostSections` by a `part` (`.running` / `.projects`):
  - `.running`: the existing `HostTitle` row (when `headed`), unreachable
    label, protocol notices, the running project sections, and "Running
    elsewhere". Keep `.disabled(!host.isReachable)`.
  - `.projects`: only the idle-projects section (`rest`), header "Projects"
    plus `rootDisplay`.
- `HubSections` keeps the hub-level bits (HubTitle, notice, unpaired,
  offline, error, progress) and renders its hosts with `part: .running`.
- `DirectoryView`'s `List`: first the `ForEach(registry.stores)` of
  `HubSections` (running), then one Projects block:
  - Tabs = every `(store, host)` from `registry.hosts(of:)` across reachable,
    paired stores, in store order, host order (local first within a hub).
  - With more than one tab, a `Picker` in its own row: `.segmented` for up to
    about three computers, `.menu` beyond that (segmented gets cramped).
    Label: `host.displayName` plus the idle count.
  - Then the chosen tab's `HostSections(part: .projects)` with the same
    `.environment(store)` and `.environment(\.hostScope, …)` that
    `HubSections` applies today.
  - Remember the choice in `@AppStorage("projectsTab")`, keyed like
    `HostSections.key` without the path (store UUID + host id); fall back to
    the first tab when it's gone.
- With exactly one computer in view, no picker, and the directory should
  look exactly as it does today.
- `expanded` keys already include store and host, so expanding a project
  in one tab doesn't leak into another.

## Check

- One hub, one machine: unchanged.
- Swarm of two or three: running sessions of every machine at the top,
  this hub's machine first; Projects tabs switch the list; the choice
  survives relaunch.
- Unreachable peer: dimmed tab, disabled rows, its Running segment says when
  it was last seen.
- Filter: counts in the picker labels update; Running drops machines with
  no match.
- `./build.sh` simulator build, then try it on the phone against the real
  swarm.
