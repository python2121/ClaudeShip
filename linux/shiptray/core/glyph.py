"""Which glyph the tray shows.

Pure stdlib — the same three states and precedence as the Mac menubar
(`SessionStore.trayState`): orange when anything waits on the user (a pending
approval counts), else green when anything is busy, else the idle ring. A
`shell` (`!` command) or a hub session still `starting` is not busy, and
unknown statuses read as idle: never a false alarm colour.
"""
from __future__ import annotations

from .model import BUSY, WAITING

GLYPH_BUSY = "busy"
GLYPH_WAITING = "waiting"
GLYPH_IDLE = "idle"
GLYPHS = (GLYPH_BUSY, GLYPH_WAITING, GLYPH_IDLE)


def asset_name(glyph: str) -> str:
    """Basename of the SVG in shiptray/assets/."""
    return f"tray-{glyph}"


def tray_glyph(sessions) -> str:
    statuses = [s.effective_status for s in sessions]
    if WAITING in statuses:
        return GLYPH_WAITING
    if BUSY in statuses:
        return GLYPH_BUSY
    return GLYPH_IDLE


def tooltip(sessions, connected: bool = True) -> str:
    if not connected:
        return "ClaudeShip — hub not reachable"
    n = len(sessions)
    if n == 0:
        return "ClaudeShip — no sessions"
    waiting = sum(1 for s in sessions if s.effective_status == WAITING)
    busy = sum(1 for s in sessions if s.effective_status == BUSY)
    parts = [f"{n} session{'s' if n != 1 else ''}"]
    if waiting:
        parts.append(f"{waiting} waiting")
    if busy:
        parts.append(f"{busy} working")
    return "ClaudeShip — " + ", ".join(parts)
