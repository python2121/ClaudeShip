# iOS directory: a proposal

Status: **proposal** (2026-10-08). Prompted by a photo of the phone with the
Mac + steamdeck swarm in view (below). Nothing here is built; the quick
fixes at the end could ship on their own.

## What the photo shows

```
 Andrews-MacBook-Air
   ClaudeShip                              main
   ┌──────────────────────────────────────────┐
   │ ○ App build and relaunch   [Terminal only]│
   │   Idle · 20s                              │
   │ ──────────────────────────────────────── │
   │ [+ New session] [▾]                 Auto  │
   │ Earlier conversations                   › │
   └──────────────────────────────────────────┘
 steamdeck
   Running
   ┌──────────────────────────────────────────┐
   │ Nothing is running. Start a session below,│
   │ or run claudeship in a terminal on the Mac│
   └──────────────────────────────────────────┘
   Running elsewhere
   ┌──────────────────────────────────────────┐
   │ ○ Linux port setup verification [Terminal only]
   │   Idle · 2m · /home/deck/Docume…          │
   └──────────────────────────────────────────┘
 [🔍 Filter projects]            (picker peeking out underneath)
```

Reading it as someone who didn't write the code:

1. **A contradiction.** The steamdeck says "Nothing is running" and, one
   card later, shows something running. The empty state only counts
   sessions inside the projects folder; "elsewhere" doesn't count. (The
   web page gets this right: its empty line considers both.)
2. **Wrong words.** "…run claudeship in a terminal on the Mac" — on a
   steamdeck. "Start a session below" — below is now the Projects block,
   whose tab may be showing the other machine.
3. **The word "Running" appears only when nothing is running.** A machine
   with a running project shows the *project's* name as the section
   header; a machine with none shows "Running". Two machines side by side
   therefore don't look parallel, and the empty state is the only place
   the group gets a name.
4. **"Running elsewhere" is a category the user never chose.** It means
   "the folder isn't under the projects root", which matters to the hub's
   bookkeeping (it won't appear in the resume list later) but not to
   someone glancing at the phone. It also demotes a real session below an
   empty placeholder.
5. **"Terminal only" is jargon.** It means "not started through the hub, so
   this phone can't attach". Nothing says why, or what one can do instead
   (nothing, from here — not even End).
6. **"Idle · 20s" is ambiguous.** Uptime? Time since the last message?
   (It's how long the state has held.) The separator-dot style makes every
   caption read as a list of unrelated facts.
7. **Depth.** Machine → project → card → session is four levels for one
   row of information. With two machines the Running part is a screen and
   a half before Projects begins; the photo's Projects picker is hidden
   under the search field.
8. **Each machine is a silo.** The thing the phone exists for — "is anything
   waiting on me?" — needs a scan across every machine. An approval on the
   steamdeck sits below the Mac's idle session.

## What the phone is for

The phone isn't the web page at 400 px. It's a glance and a tap:

- **Is anything waiting on me?** Approve, deny, or open it. This is the
  reason to pick the phone up.
- **What's working, and for how long?** Check in on progress; open one to
  watch or steer.
- **Hop into something.** Resume a parked session, or start one in a
  project (or the home folder) on some machine.
- **Is a machine missing or out of date?** Only when it is.

Everything else — the projects root, the "elsewhere" distinction, the
hub-vs-terminal provenance — is secondary and should read as a detail on
a row, not as structure.

## Proposed layout

One list, two blocks: **Now** (every session on every machine, grouped by
what it needs from you) and **Projects** (places to start or resume
something). One **machine scope** control at the top filters both.

```
┌──────────────────────────────────────────┐
│ ●1 ◐2            ClaudeShip         +  ⚙ │
├──────────────────────────────────────────┤
│  All  │ MacBook Air │ steamdeck ◦        │  ← only with 2+ machines
│                                          │
│ NEEDS YOU                                │
│ ● Fix the login redirect     MacBook Air │
│   Bash: npm test                         │
│   [Approve] [Deny] [⋯]           Open ›  │
│                                          │
│ WORKING                                  │
│ ◐ App build and relaunch     MacBook Air │
│   working 4 m · ClaudeShip · main      › │
│ ◐ Linux port setup verification steamdeck│
│   working 12 m · ~/Documents/rust      › │
│                                          │
│ IDLE                                     │
│ ○ New conversation             steamdeck │
│   idle 2 m · rmd-shell         ⌨ terminal│
│                                          │
│ PROJECTS                      by activity│
│ ClaudeShip         main  ◐1   MacBook Air│
│ system-stats       main       steamdeck ›│
│ rmd-shell          main       MacBook Air│
│ …                                        │
│ [🔍 Filter]                              │
└──────────────────────────────────────────┘
```

### Machine scope

A segmented control (a menu past three machines): **All**, then one
segment per machine, the hub's own first. It scopes both blocks and is
remembered (`@AppStorage`). With one machine in view there is no control
and no machine chips anywhere — the single-hub, single-machine directory
stays as plain as it is today.

- An unreachable machine's segment carries a small moon (◦ above) and is
  dimmed. Choosing it shows its last-reported sessions and projects,
  disabled, under one line: "Unreachable since 3 m · showing what it last
  reported." Under **All**, its sessions are left out and one quiet line
  stands in: "steamdeck unreachable since 3 m."
- A version mismatch (vs this app, or vs the home hub that would relay)
  is a one-line notice at the top of that machine's scope and a warning
  triangle on its segment. Under All, the line appears once, naming the
  machine.
- The quick **+** starts in the home folder of the chosen machine; under
  All it asks, as today.

This replaces both today's machine headers and the Projects tab picker
with one control — the machine is chosen once, not per block.

### Now

Three groups, each present only when non-empty: **Needs you** (sessions
with a pending approval or `waiting` status), **Working** (`busy`,
`shell`), **Idle** (`idle`, and `starting` at the end). Within **Needs
you**, the longest wait first (see Open questions); within the other
groups, machine order then the hub's project order, so rows don't jump
between polls.

Row anatomy:

- **Glyph** by state (orange filled, green half, hollow) — unchanged.
- **Title**: the conversation title, "New conversation" until there is
  one, "Starting…" while starting.
- **Caption**: `<state> for <duration> · <where> · <branch if it differs>`.
  "working for 4 m", "idle for 2 m", "waiting for 1 m". *Where* is the
  project name when the folder is a project, else the folder abbreviated
  (`~/Documents/rust`): that is the whole of "elsewhere", shown as the
  path rather than as a group.
- **Machine chip**, trailing, only under **All** with two or more machines.
  Short names: the hub's `host` is "Andrews-MacBook-Air"; the phone can
  show a per-machine alias from Settings (default: the hub name with the
  user's possessive and hyphens cleaned up, "MacBook Air").
- **Trailing control**: a chevron when the hub owns the session (tap opens
  the terminal); otherwise a `⌨ terminal` tag instead of "Terminal only",
  with the row inert and the accessibility label "Running in a terminal on
  MacBook Air; open it there." (If the hub grows a kill-by-pid for
  registry sessions, End could return to these rows; today it can't.)
- **Approval rows** keep today's treatment: Approve / Deny / ⋯ in place of
  the caption, the request summary under them, Open › trailing.

Empty Now: one line, "Nothing is running." — no instructions. The + and
the Projects block below are the instructions. (Under a machine scope:
"Nothing is running on steamdeck.")

### Projects

Every project folder of the scoped machine(s) — including the ones with
sessions running, which today are pulled out into cards above. Sorted by
last activity across machines (the hub's order within a machine is by
activity already), so under All the list interleaves machines with a chip
on each row; the same project name on two machines appears twice, which
is correct and useful.

Row: name, branch, a small `◐1` / `●1` count when sessions run there, the
machine chip (All only), when it was last active, chevron. Expanding a
row shows what it shows today: New session with the mode menu, then the
resume list. The running sessions themselves are not repeated here — they
live in Now.

Header: "Projects" with the root folder on the right under a single
machine; under All, "Projects" alone (the roots differ). The filter field
matches names, branches, titles, and session titles as today, and applies
to both blocks; the tally in the top bar is unaffected by the filter and
the scope (it is "anything, anywhere").

### Single machine

With one hub and one machine the result is today's directory minus its
rough edges:

```
│ WORKING                                  │
│ ◐ App build and relaunch                 │
│   working 4 m · ClaudeShip · main      › │
│ IDLE                                     │
│ ○ New conversation                       │
│   idle 20 s · ~                ⌨ terminal│
│ PROJECTS                  ~/Documents/code│
│ ClaudeShip         main  ◐1        7 h › │
│ energy-project     main            7 h › │
```

## Where the data comes from

Everything above is in `/api/state` already:

| Shown | Source |
|---|---|
| Groups | `status` (`waiting`/`busy`/`shell`/`idle`/`starting`) and `approvals[]` |
| "for 4 m" | `since` against the host's clock (`HubStore.now(for:)`) |
| where | the project's `name`, else `cwd` abbreviated with the host's `home` |
| machine chip | `hosts[].name` / `id`; alias phone-side |
| chevron vs terminal | `attachable` + `hubId` |
| unreachable / version | `reachable`, `lastSeen`, `protocol` per host |
| Projects count badge | `projects[].sessions.count` |

No hub changes are required. Two would help: a `shortName` per host
(or just letting the phone alias), and kill-by-pid for terminal sessions
so End works on every row (the Mac overlay already does SIGHUP by pid
locally; the hub could do the same for its own machine).

## Alternatives considered

- **Keep per-machine segments, as the web page does now.** Consistent
  with the web, and it is what the last change made. Rejected for the
  phone because the first question ("is anything waiting on me?") spans
  machines, and because each machine costs a header plus an empty state
  even when it has nothing to say. The web page has the width to show
  machines side by side; the phone stacks them, and stacking is what
  produced the photo.
- **Tabs per machine for everything (no All).** Simpler rows (no chips),
  but an approval on the unselected machine is invisible until the tally
  is noticed. All as the default keeps the glance honest; the scope is
  there for when one machine is the job.
- **Keep project cards for running projects.** They bundle "running here"
  with "start another here", which is handy on the web. On the phone the
  card is mostly chrome; the Projects row with a count badge does the
  same with one line.

## Open questions

- Sort within **Needs you**: oldest wait first (most overdue) or newest
  first (what just happened)? Oldest first is the better default for an
  approval queue; newest first for a feed. Proposal: oldest first, since
  the hub prunes answered ones immediately.
- Should idle sessions collapse after N (e.g. "and 4 more idle") to keep
  Projects within reach? Probably yes past five.
- Machine aliases: Settings only, or also editable by tapping the chip?
- Does Now need the three group labels when only one group is present?
  Likely still yes: "WORKING" over one row is cheaper than wondering.

## Quick fixes that don't need the redesign

Each is a few lines in `DirectoryView.swift` and could ship today:

1. The empty state counts `elsewhere` too (`HostSections.running` →
   consider `host.allSessions`), matching the web page.
2. Copy: name the machine ("…on steamdeck"), drop "Start a session below".
3. Show "Running" consistently: either as a group header over each
   machine's running cards or not at all; today it only appears when
   empty.
4. "Terminal only" → "⌨ terminal" tag with the explanatory accessibility
   label; "Background" keeps its name.
5. Captions: "idle for 20 s" instead of "Idle · 20s"; the folder path for
   elsewhere sessions abbreviated with the host's `home`.
