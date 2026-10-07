"""Text for the rows. Mirrors `StatusFormat` in
`Sources/ClaudeShip/SessionsView.swift` — the vectors in tests/test_core.py are
the Swift self-test's (`SelfTest.swift`, "MARK: formatting"); change both.
"""
from __future__ import annotations

import os

from .model import BUSY, SHELL, STARTING, WAITING


def compact_duration(start_s: float, end_s: float) -> str:
    """"2h 34m" / "57m" / "0m"."""
    secs = max(0, int(end_s - start_s))
    h, m = secs // 3600, (secs % 3600) // 60
    return f"{h}h {m}m" if h > 0 else f"{m}m"


def compact_age(since_s: float, now_s: float) -> str:
    """"12s" / "3m" / "2h 5m" — seconds-precision only while young."""
    secs = max(0, int(now_s - since_s))
    if secs < 60:
        return f"{secs}s"
    return compact_duration(since_s, now_s)


def abbreviate_path(path: str, home: str | None = None) -> str:
    """`/home/me/code/x` → `~/code/x`. Only at a path boundary: `/home/meg`
    is not under `/home/me`."""
    home = (home if home is not None else os.path.expanduser("~")).rstrip("/")
    if home and (path == home or path.startswith(home + "/")):
        return "~" + path[len(home):]
    return path


def state_text(status: str, waiting_for: str | None = None) -> str:
    if status == BUSY:
        return "Working"
    if status == SHELL:
        return "Shell command"
    if status == WAITING:
        return f"Waiting: {waiting_for}" if waiting_for else "Waiting for input"
    if status == STARTING:
        return "Starting"
    return "Idle"


def activity_text(since_ms: int | None, started_ms: int | None, now_s: float) -> str:
    """"for 12s · up 2h 14m" — how long the state has held, and uptime."""
    parts = []
    if since_ms is not None:
        parts.append(f"for {compact_age(since_ms / 1000, now_s)}")
    if started_ms is not None:
        parts.append(f"up {compact_duration(started_ms / 1000, now_s)}")
    return " · ".join(parts) if parts else "—"


def pending_caption(summary: str, count: int) -> str:
    """The first approval's summary, plus how many more are queued behind it."""
    more = count - 1
    return f"{summary} · +{more} more" if more > 0 else summary
