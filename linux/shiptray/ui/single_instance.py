"""Single-instance plumbing over a local socket.

Launching the applet again (the menu entry, autostart racing a manual start)
must not put a second glyph in the tray: if an instance is already running,
the new process sends it a bare "show" ping — it opens its popup — and exits.
The wire format is torrent-flinger's (`{"links": [...]}` per line), kept so
the two stay one copy; this applet only ever sends an empty list.
"""
from __future__ import annotations

import getpass
import json

from PySide6.QtCore import QObject, Signal
from PySide6.QtNetwork import QLocalServer, QLocalSocket

SOCKET_NAME = f"claudeship-tray-{getpass.getuser()}"


def try_forward(links: list[str]) -> bool:
    """Send links (or a bare 'show' ping) to a running instance. True on success."""
    sock = QLocalSocket()
    sock.connectToServer(SOCKET_NAME)
    if not sock.waitForConnected(500):
        return False
    msg = json.dumps({"links": links}) + "\n"
    sock.write(msg.encode())
    sock.waitForBytesWritten(1000)
    sock.disconnectFromServer()
    return True


class InstanceServer(QObject):
    link_received = Signal(str)
    show_requested = Signal()

    def __init__(self, parent=None):
        super().__init__(parent)
        QLocalServer.removeServer(SOCKET_NAME)  # clear stale socket from a crash
        self.server = QLocalServer(self)
        self.server.newConnection.connect(self._on_connection)
        self.server.listen(SOCKET_NAME)

    def _on_connection(self):
        sock = self.server.nextPendingConnection()
        sock.readyRead.connect(lambda: self._on_ready(sock))

    def _on_ready(self, sock):
        for line in bytes(sock.readAll()).decode().splitlines():
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            links = msg.get("links", [])
            if not links:
                self.show_requested.emit()
            for link in links:
                self.link_received.emit(link)
