"""How often to poll the hub.

Two cadences, the same split the Mac app makes (`store.panelVisible`): every
2 s while the popup is open, and 5 s while only the tray glyph needs to stay
honest. The hub caches its state for 1 s, so 2 s costs it nothing.
"""
from __future__ import annotations

VISIBLE_POLL_MS = 2000
HIDDEN_POLL_MS = 5000


def poll_interval_ms(visible: bool) -> int:
    return VISIBLE_POLL_MS if visible else HIDDEN_POLL_MS
