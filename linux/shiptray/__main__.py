"""Entry point.

    python -m shiptray                 start the tray applet (or open the running one's popup)
    python -m shiptray --smoke-test    run for three seconds and quit; never starts the hub
"""
from __future__ import annotations

import os
import sys


def main() -> int:
    # Wayland toplevels cannot position themselves, so the tray popup would
    # appear wherever the compositor drops it (KWin: screen center), and Tool
    # windows never receive activation so click-outside dismissal breaks.
    # Run via XWayland instead — X11 windows anchor to the tray like Plasma
    # applets. Overridable by setting QT_QPA_PLATFORM yourself.
    if (sys.platform.startswith("linux")
            and "QT_QPA_PLATFORM" not in os.environ
            and os.environ.get("WAYLAND_DISPLAY")):
        os.environ["QT_QPA_PLATFORM"] = "xcb;wayland"

    args = sys.argv[1:]
    smoke = "--smoke-test" in args

    from .ui.single_instance import try_forward
    if not smoke and try_forward([]):
        return 0

    from PySide6.QtCore import QTimer
    from PySide6.QtWidgets import QApplication

    from .ui.app import ShipApp

    qapp = QApplication(sys.argv)
    qapp.setApplicationName("ClaudeShip")
    qapp.setQuitOnLastWindowClosed(False)

    # A smoke run must not take the instance socket from a running applet.
    app = ShipApp(qapp, start_hub=not smoke, single_instance=not smoke)  # noqa: F841 — owns the tray for the app's life
    if smoke:
        QTimer.singleShot(3000, qapp.quit)
    return qapp.exec()


if __name__ == "__main__":
    raise SystemExit(main())
