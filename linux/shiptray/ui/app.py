"""Application hub: tray icon, polling, actions, notifications.

The counterpart of the Mac app's `SessionStore` + `AppDelegate`, minus every
local inspection: all state comes from the hub's `/api/state`.
"""
from __future__ import annotations

import html
import os
import subprocess
import time
import webbrowser
from pathlib import Path

from PySide6.QtCore import QObject, QTimer
from PySide6.QtGui import QAction, QIcon
from PySide6.QtWidgets import QApplication, QMenu, QSystemTrayIcon

from ..core import terminal
from ..core.glyph import tooltip, tray_glyph
from ..core.hub import PROTOCOL, HubClient, auth_url, hub_home, socket_path
from ..core.model import HubState, approval_ids, new_approvals, parse_state, protocol_mismatch
from ..core.polling import HIDDEN_POLL_MS, poll_interval_ms
from .popup import Popup
from .single_instance import InstanceServer
from .style import ASSETS, glyph_color, tray_pixmap
from .worker import run_async


def _spawn_detached(argv: list[str], cwd: str | None = None) -> None:
    subprocess.Popen(argv, cwd=cwd, start_new_session=True,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL)


class ShipApp(QObject):
    def __init__(self, qapp: QApplication, home: Path | None = None, start_hub: bool = True,
                 spawn=_spawn_detached, open_url=webbrowser.open, now=time.time,
                 single_instance: bool = True):
        super().__init__()
        self.qapp = qapp
        self.home = home if home is not None else hub_home()
        self._spawn = spawn
        self._open_url = open_url
        self._polling = False
        self._popup_visible = False
        self._glyph_key: tuple | None = None
        self._seen_approvals: set[str] = set()
        self.state = HubState()
        self.connected = False

        icon = QIcon(str(ASSETS / "icon128.png"))
        qapp.setWindowIcon(icon)

        self.popup = Popup(now=now, home=os.path.expanduser("~"))
        self.popup.session_clicked.connect(self.open_session)
        self.popup.end_requested.connect(self.end_session)
        self.popup.approve_clicked.connect(self.approve)
        self.popup.auto_approve_requested.connect(self.auto_approve)
        self.popup.new_session_requested.connect(self.new_home_session)
        self.popup.open_web_requested.connect(self.open_web)

        self.tray = QSystemTrayIcon(icon)
        self.tray.setToolTip("ClaudeShip")
        self.tray.activated.connect(self._on_tray_activated)
        self.tray.messageClicked.connect(self._show_popup)
        menu = QMenu()
        menu.addAction(QAction("Open ClaudeShip", menu, triggered=self.open_web))
        menu.addAction(QAction("New session in ~", menu, triggered=self.new_home_session))
        menu.addSeparator()
        menu.addAction(QAction("Quit", menu, triggered=qapp.quit))
        self.tray.setContextMenu(menu)
        self.tray.show()
        self._update_tray_icon()

        # Tests pass single_instance=False: listening would first remove the
        # socket of an applet already running in the same session.
        self.instance_server = None
        if single_instance:
            self.instance_server = InstanceServer(self)
            self.instance_server.show_requested.connect(self._show_popup)

        if start_hub:
            self.ensure_hub()

        self.timer = QTimer(self)
        self.timer.timeout.connect(self.poll)
        self.timer.start(HIDDEN_POLL_MS)
        self.popup.installEventFilter(self)
        self.poll()

    # -- the hub --------------------------------------------------------------

    def ensure_hub(self) -> bool:
        """Start the hub if its socket isn't there. Normally the systemd user
        unit already has; `claudeship hub start` is a no-op when it's up.
        Returns whether a start was attempted."""
        if socket_path(self.home).exists():
            return False
        try:
            self._spawn(terminal.hub_start_argv(terminal.resolve_binary("claudeship")), None)
        except OSError as e:
            self.popup.set_error(f"Couldn't start the hub: {e}")
            return False
        return True

    def client(self) -> HubClient:
        return HubClient.from_home(self.home)

    # -- polling ------------------------------------------------------------

    def eventFilter(self, obj, event):
        if obj is self.popup and event.type() in (event.Type.Show, event.Type.Hide):
            self._popup_visible = event.type() == event.Type.Show
            ms = poll_interval_ms(self._popup_visible)
            if ms != self.timer.interval():
                self.timer.start(ms)
            if self._popup_visible:
                self.poll()
        return super().eventFilter(obj, event)

    def poll(self):
        if self._polling:
            return
        self._polling = True
        client = self.client()
        run_async(client.state, on_done=self._on_poll_done, on_error=self._on_poll_error)

    def _on_poll_done(self, raw):
        self._polling = False
        self.connected = True
        self.state = parse_state(raw)
        sessions = self.state.sessions
        if protocol_mismatch(self.state, PROTOCOL):
            theirs = self.state.protocol if self.state.protocol is not None else "an older one"
            self.popup.set_banner(
                f"The hub speaks protocol {theirs}; this applet speaks {PROTOCOL}. "
                "Update whichever is older (restart the hub with `claudeship hub stop` "
                "once its sessions can end).")
        else:
            self.popup.set_banner("")
        self.popup.set_sessions(sessions, host=self.state.host)
        self._update_tray_icon()
        self.tray.setToolTip(tooltip(sessions))
        self._notify_new_approvals()

    def _on_poll_error(self, message):
        self._polling = False
        self.connected = False
        self.state = HubState()
        self.popup.set_error(message)
        self._update_tray_icon()
        self.tray.setToolTip(f"{tooltip((), connected=False)}\n{message}")

    def _notify_new_approvals(self):
        fresh = new_approvals(self._seen_approvals, self.state.sessions)
        self._seen_approvals = approval_ids(self.state.sessions)
        if not fresh:
            return
        session, approval = fresh[0]
        title = f"Permission requested — {session.project_name}"
        body = approval.summary or approval.tool
        if len(fresh) > 1:
            body += f"\n+{len(fresh) - 1} more"
        # The body is markup to the notification server (Plasma, GNOME: the
        # spec's body-markup); the summary is untrusted, so escape it.
        self.tray.showMessage(title, html.escape(body, quote=False), QSystemTrayIcon.Information, 8000)

    # -- tray icon ------------------------------------------------------------

    def _update_tray_icon(self) -> None:
        """Pick the glyph and tint it for the current panel colours. Keyed on
        the colour as well as the glyph, so a Breeze light/dark switch is
        picked up within one poll; a no-op in the steady case."""
        glyph = tray_glyph(self.state.sessions)
        color = glyph_color(self.qapp.palette(), glyph)
        key = (glyph, color.rgba())
        if key == self._glyph_key:
            return
        pixmap = tray_pixmap(glyph, color, size=22, dpr=self.qapp.devicePixelRatio())
        if pixmap.isNull():
            return  # QtSvg unavailable — keep the raster icon we started with
        self._glyph_key = key
        self.tray.setIcon(QIcon(pixmap))

    def tray_glyph(self) -> str | None:
        return self._glyph_key[0] if self._glyph_key else None

    # -- actions ------------------------------------------------------------

    def _action(self, fn, on_done=None):
        def done(result):
            if on_done:
                on_done(result)
            self.poll()
        run_async(fn, on_done=done,
                  on_error=lambda msg: self.tray.showMessage(
                      "ClaudeShip", msg, QSystemTrayIcon.Warning, 5000))

    def approve(self, approval_id: str, allow: bool):
        client = self.client()
        self._action(lambda: client.approve(approval_id, allow))

    def auto_approve(self, session_id: str, rule: str):
        client = self.client()
        self._action(lambda: client.auto_approve(session_id, rule))

    def end_session(self, key: str):
        session = self._session(key)
        if session is None or not session.hub_id:
            return
        client = self.client()
        hub_id = session.hub_id
        self._action(lambda: client.kill(hub_id))

    def new_home_session(self):
        """The quick "+": a session in the home folder, in auto mode, through
        the hub (so it shows everywhere), then attached in a terminal."""
        path = self.state.home or os.path.expanduser("~")
        client = self.client()
        self.popup.hide()
        self._action(lambda: client.launch(path, "auto"),
                     on_done=lambda hub_id: self._attach_in_terminal(hub_id, path))

    def open_session(self, key: str):
        session = self._session(key)
        if session is None:
            return
        if session.hub_id:
            self.popup.hide()
            self._attach_in_terminal(session.hub_id, session.cwd)
        elif session.background and session.attach_id:
            self.popup.hide()
            argv = terminal.background_attach_argv(terminal.resolve_binary("claude"),
                                                   session.attach_id)
            self._open_terminal(argv, session.cwd)
        # Terminal only: nothing to do — window activation by pid on
        # KWin/Wayland is its own project. The row's tooltip says so.

    def open_web(self):
        """The pairing link, so a browser that has never seen the hub pairs
        itself on the way in."""
        self._open_url(auth_url(self.home))

    def _attach_in_terminal(self, hub_id: str, cwd: str):
        argv = terminal.attach_argv(terminal.resolve_binary("claudeship"), hub_id)
        self._open_terminal(argv, cwd)

    def _open_terminal(self, argv: list[str], cwd: str):
        if not os.path.isdir(cwd):
            cwd = os.path.expanduser("~")
        command = terminal.terminal_command(argv, cwd)
        if command is None:
            self.tray.showMessage("No terminal found",
                                  "Install Ghostty or Konsole, or set $TERMINAL.",
                                  QSystemTrayIcon.Warning, 5000)
            return
        try:
            self._spawn(command, cwd)
        except OSError as e:
            self.tray.showMessage("Couldn't open a terminal", str(e),
                                  QSystemTrayIcon.Warning, 5000)

    def _session(self, key: str):
        return next((s for s in self.state.sessions if s.key == key), None)

    # -- windows ------------------------------------------------------------

    def _on_tray_activated(self, reason):
        if reason in (QSystemTrayIcon.Trigger, QSystemTrayIcon.MiddleClick):
            self._show_popup()

    def _show_popup(self):
        geo = self.tray.geometry()  # valid on X11/XWayland; empty under pure Wayland
        self.popup.toggle_near(geo if geo.isValid() and not geo.isEmpty() else None)
