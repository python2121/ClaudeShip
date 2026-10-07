"""`/api/state` → the rows the popup shows.

Pure stdlib. Parsing is lenient the way the phone's decoder is not: a field of
the wrong type reads as absent rather than failing the whole poll, because the
hub and this applet are updated separately and a newer hub may add or reshape
things. Unknown statuses are kept verbatim; the glyph and the colours treat
them as idle — never a false alarm.

Grouping mirrors the Mac overlay as far as a hub client can see: sessions the
hub owns (it can attach to them) under **Virtual**, Claude Code's daemon-run
sessions under **Background**, and the rest — plain `claude` in some terminal —
under **Terminal only**. Inside a group the order is (cwd, pid), so rows never
jump between polls; state is conveyed by colour, not position.
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field

VIRTUAL = "Virtual"
BACKGROUND = "Background"
TERMINAL_ONLY = "Terminal only"
GROUP_ORDER = (VIRTUAL, BACKGROUND, TERMINAL_ONLY)

BUSY = "busy"
SHELL = "shell"
IDLE = "idle"
WAITING = "waiting"
STARTING = "starting"


@dataclass(frozen=True)
class Approval:
    id: str
    tool: str
    summary: str
    detail: str
    received_at: int | None  # epoch ms


@dataclass(frozen=True)
class AutoApprove:
    until: int | None = None  # epoch ms; None with session=True
    session: bool = False


@dataclass(frozen=True)
class Session:
    key: str
    pid: int
    cwd: str
    status: str
    hub_id: str | None = None
    session_id: str | None = None
    job_id: str | None = None
    name: str | None = None
    title: str | None = None
    branch: str | None = None
    waiting_for: str | None = None
    since: int | None = None       # epoch ms the status has held since
    started_at: int | None = None  # epoch ms
    background: bool = False
    attachable: bool = False
    approvals: tuple[Approval, ...] = ()
    auto_approve: AutoApprove | None = None

    @property
    def group(self) -> str:
        if self.hub_id:
            return VIRTUAL
        if self.background:
            return BACKGROUND
        return TERMINAL_ONLY

    @property
    def project_name(self) -> str:
        """The row's bold line, as on the Mac: the cwd's last component."""
        return os.path.basename(self.cwd.rstrip("/")) or self.cwd or "?"

    @property
    def effective_status(self) -> str:
        """A pending approval means the session is waiting on someone,
        whatever the registry last said (the hub forces this too)."""
        return WAITING if self.approvals else self.status

    @property
    def attach_id(self) -> str | None:
        """What `claude attach` takes for a background session: the daemon's
        job id, else the first 8 characters of the session id (the Mac's
        `ClaudeSession.attachId` fallback)."""
        if self.job_id:
            return self.job_id
        if self.session_id:
            return self.session_id[:8]
        return None

    def matches(self, text: str) -> bool:
        """Search filter: case-insensitive substring over everything the row
        shows, plus the registry name."""
        needle = text.strip().lower()
        if not needle:
            return True
        hay = [self.project_name, self.cwd, self.name, self.title, self.branch,
               self.status, self.waiting_for]
        hay += [a.summary for a in self.approvals]
        return any(needle in h.lower() for h in hay if h)


@dataclass(frozen=True)
class HubState:
    host: str = ""
    protocol: int | None = None
    home: str | None = None
    approvals_supported: bool = False
    now: int | None = None
    sessions: tuple[Session, ...] = field(default_factory=tuple)


# -- parsing -------------------------------------------------------------------

def _str(obj: dict, key: str) -> str | None:
    value = obj.get(key)
    return value if isinstance(value, str) and value else None


def _int(obj: dict, key: str) -> int | None:
    value = obj.get(key)
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    return None


def _bool(obj: dict, key: str) -> bool:
    return obj.get(key) is True


def parse_approval(obj) -> Approval | None:
    if not isinstance(obj, dict) or not _str(obj, "id"):
        return None
    return Approval(id=obj["id"], tool=_str(obj, "tool") or "",
                    summary=_str(obj, "summary") or "", detail=_str(obj, "detail") or "",
                    received_at=_int(obj, "receivedAt"))


def parse_auto_approve(obj) -> AutoApprove | None:
    if not isinstance(obj, dict):
        return None
    if obj.get("session") is True:
        return AutoApprove(session=True)
    until = _int(obj, "until")
    if until is not None:
        return AutoApprove(until=until)
    return None


def parse_session(obj) -> Session | None:
    if not isinstance(obj, dict):
        return None
    pid = _int(obj, "pid")
    cwd = _str(obj, "cwd")
    if pid is None or cwd is None:
        return None
    hub_id = _str(obj, "hubId")
    approvals = obj.get("approvals")
    return Session(
        key=_str(obj, "key") or (f"h:{hub_id}" if hub_id else f"p:{pid}"),
        pid=pid,
        cwd=cwd,
        status=_str(obj, "status") or IDLE,
        hub_id=hub_id,
        session_id=_str(obj, "sessionId"),
        job_id=_str(obj, "jobId"),
        name=_str(obj, "name"),
        title=_str(obj, "title"),
        branch=_str(obj, "branch"),
        waiting_for=_str(obj, "waitingFor"),
        since=_int(obj, "since"),
        started_at=_int(obj, "startedAt"),
        background=_bool(obj, "background"),
        attachable=_bool(obj, "attachable"),
        approvals=tuple(a for a in map(parse_approval, approvals if isinstance(approvals, list)
                                       else []) if a is not None),
        auto_approve=parse_auto_approve(obj.get("autoApprove")),
    )


def parse_state(obj) -> HubState:
    """Every live session, wherever the hub filed it (under a project or in
    `elsewhere`), each once, in stable (cwd, pid) order."""
    if not isinstance(obj, dict):
        return HubState()
    raw: list = []
    projects = obj.get("projects")
    for project in projects if isinstance(projects, list) else []:
        if isinstance(project, dict) and isinstance(project.get("sessions"), list):
            raw.extend(project["sessions"])
    if isinstance(obj.get("elsewhere"), list):
        raw.extend(obj["elsewhere"])
    seen: set[str] = set()
    sessions = []
    for s in map(parse_session, raw):
        if s is None or s.key in seen:
            continue
        seen.add(s.key)
        sessions.append(s)
    sessions.sort(key=lambda s: (s.cwd, s.pid))
    return HubState(host=_str(obj, "host") or "", protocol=_int(obj, "protocol"),
                    home=_str(obj, "home"),
                    approvals_supported=_bool(obj, "approvalsSupported"),
                    now=_int(obj, "now"), sessions=tuple(sessions))


# -- derived -------------------------------------------------------------------

def group_sessions(sessions) -> list[tuple[str, list[Session]]]:
    """[(group label, rows)] in GROUP_ORDER, empty groups omitted, rows in
    (cwd, pid) order."""
    groups: dict[str, list[Session]] = {}
    for s in sessions:
        groups.setdefault(s.group, []).append(s)
    return [(g, sorted(groups[g], key=lambda s: (s.cwd, s.pid)))
            for g in GROUP_ORDER if g in groups]


def protocol_mismatch(state: HubState, expected: int) -> bool:
    return state.protocol != expected


def new_approvals(seen: set[str], sessions) -> list[tuple[Session, Approval]]:
    """Approvals not in `seen`, with their session, in row order."""
    return [(s, a) for s in sessions for a in s.approvals if a.id not in seen]


def approval_ids(sessions) -> set[str]:
    return {a.id for s in sessions for a in s.approvals}
