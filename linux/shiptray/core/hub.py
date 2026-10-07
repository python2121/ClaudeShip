"""The hub client: where the hub lives, how to reach it, and its five calls.

Pure stdlib (urllib), so it is tested against an in-process mock hub with no
Qt. The applet is a pure hub client: it never reads `~/.claude`; everything it
shows comes from `GET /api/state`, and everything it does is a POST.

Credentials come from the hub's home, the directory the hub itself keeps its
socket, config and pairing secret in:

    $CLAUDESHIP_HOME                     if set (tests, private hubs)
    $XDG_STATE_HOME/claudeship           on Linux
    ~/.local/state/claudeship            when XDG_STATE_HOME is unset
    ~/Library/Application Support/ClaudeShip/hub   on macOS (dev runs only)

The token is presented as the `claude_ship` cookie, exactly as a paired
browser does; non-browser clients send no Origin header, which the hub
accepts.
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

COOKIE_NAME = "claude_ship"
DEFAULT_PORT = 7433
# The hub protocol this applet speaks (PROTOCOL in hub/src/frame.rs and web/app.js).
PROTOCOL = 3

STATE_TIMEOUT_S = 1.0
# A launch waits for the hub to spawn the session; the other POSTs are quick,
# but nobody is staring at a spinner for them either.
POST_TIMEOUT_S = 3.0

AUTO_APPROVE_RULES = ("5m", "session", "off")


class HubError(Exception):
    """Anything that kept a call from succeeding; the message is for people."""


class Unpaired(HubError):
    """The hub refused the token (401) or there is no token to send."""


class Unreachable(HubError):
    """Nothing answered at the hub's address."""


class Refused(HubError):
    """The hub answered, but not with success."""

    def __init__(self, message: str, status: int | None = None):
        super().__init__(message)
        self.status = status


def hub_home(env: dict | None = None, platform: str | None = None) -> Path:
    env = os.environ if env is None else env
    platform = sys.platform if platform is None else platform
    if env.get("CLAUDESHIP_HOME"):
        return Path(env["CLAUDESHIP_HOME"]).expanduser()
    home = Path(env.get("HOME") or Path.home())
    if platform == "darwin":
        return home / "Library" / "Application Support" / "ClaudeShip" / "hub"
    state = env.get("XDG_STATE_HOME")
    base = Path(state) if state else home / ".local" / "state"
    return base / "claudeship"


def socket_path(home: Path) -> Path:
    return home / "hub.sock"


def read_port(home: Path) -> int:
    """The web port from `config.json`; the default when it's missing or odd."""
    try:
        config = json.loads((home / "config.json").read_text())
    except (OSError, ValueError):
        return DEFAULT_PORT
    port = config.get("port") if isinstance(config, dict) else None
    if isinstance(port, int) and not isinstance(port, bool) and 0 < port < 65536:
        return port
    return DEFAULT_PORT


def read_token(home: Path) -> str:
    try:
        return (home / "token").read_text().strip()
    except OSError:
        return ""


def web_url(home: Path) -> str:
    return f"http://localhost:{read_port(home)}"


def auth_url(home: Path) -> str:
    """The web app's pairing link: the browser that opens it trades the
    secret for the cookie and lands on the page, already paired. Without a
    token (hub never ran) it is the bare web URL."""
    token = read_token(home)
    if not token:
        return web_url(home)
    return f"{web_url(home)}/auth?k={urllib.parse.quote(token, safe='')}"


# Never through a proxy: urllib honours $http_proxy for localhost unless
# $no_proxy lists it, which would hand the pairing cookie to the proxy.
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class HubClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.token = token

    @classmethod
    def from_home(cls, home: Path) -> HubClient:
        """Re-read on every poll by the app: both files are tiny, and a
        `claudeship hub unlink` (new token) or a port change then needs no
        restart of the applet."""
        return cls(web_url(home), read_token(home))

    # -- transport ------------------------------------------------------------

    def _call(self, path: str, body: dict | None = None,
              timeout: float = STATE_TIMEOUT_S) -> dict:
        if not self.token:
            raise Unpaired("No pairing secret in the hub's home yet — has the hub ever run?")
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base_url + path, data=data,
                                         method="GET" if body is None else "POST")
        request.add_header("Cookie", f"{COOKIE_NAME}={self.token}")
        if body is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with _OPENER.open(request, timeout=timeout) as response:
                raw = response.read()
        except urllib.error.HTTPError as e:
            if e.code == 401:
                raise Unpaired("The hub refused this applet's pairing secret.") from e
            raise Refused(_reason(e.read(), e.code), e.code) from e
        except (urllib.error.URLError, OSError) as e:
            reason = getattr(e, "reason", e)
            raise Unreachable(f"Can't reach the hub at {self.base_url}: {reason}") from e
        if not raw:
            return {}
        try:
            parsed = json.loads(raw)
        except ValueError as e:
            raise Refused("The hub answered in a form this applet doesn't understand.") from e
        return parsed if isinstance(parsed, dict) else {}

    # -- the API --------------------------------------------------------------

    def state(self) -> dict:
        return self._call("/api/state")

    def kill(self, hub_id: str) -> None:
        self._call("/api/kill", {"id": hub_id}, POST_TIMEOUT_S)

    def launch(self, path: str, permission_mode: str = "auto") -> str:
        """Start a session; returns its hub id."""
        answer = self._call("/api/launch", {"path": path, "permissionMode": permission_mode},
                            POST_TIMEOUT_S)
        hub_id = answer.get("id")
        if not isinstance(hub_id, str) or not hub_id:
            raise Refused("The hub started nothing (no session id in its answer).")
        return hub_id

    def approve(self, approval_id: str, allow: bool) -> None:
        """Answer a pending approval. Gone already (answered in the terminal
        or by another screen: 404) is not an error."""
        try:
            self._call("/api/approve", {"id": approval_id, "allow": bool(allow)}, POST_TIMEOUT_S)
        except Refused as e:
            if e.status != 404:
                raise

    def auto_approve(self, session_id: str, rule: str) -> None:
        if rule not in AUTO_APPROVE_RULES:
            raise ValueError(f"unknown auto-approve rule {rule!r}")
        self._call("/api/auto-approve", {"sessionId": session_id, "rule": rule}, POST_TIMEOUT_S)


def _reason(raw: bytes, status: int) -> str:
    try:
        obj = json.loads(raw)
    except ValueError:
        obj = None
    if isinstance(obj, dict) and isinstance(obj.get("error"), str):
        return obj["error"]
    return f"The hub refused ({status})."
