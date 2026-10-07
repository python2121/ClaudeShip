"""Core tests: model parsing, grouping, glyph precedence, formats, terminal
choice, and the hub client against an in-process mock hub.

Run (from linux/): PYTHONPATH=. .venv/bin/python -m unittest discover tests
"""
import copy
import json
import os
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from shiptray.core import terminal
from shiptray.core.formats import (
    abbreviate_path,
    activity_text,
    compact_age,
    compact_duration,
    pending_caption,
    state_text,
)
from shiptray.core.glyph import tooltip, tray_glyph
from shiptray.core.hub import (
    PROTOCOL,
    HubClient,
    Refused,
    Unpaired,
    Unreachable,
    auth_url,
    hub_home,
    read_port,
    read_token,
    web_url,
)
from shiptray.core.model import (
    BACKGROUND,
    TERMINAL_ONLY,
    VIRTUAL,
    approval_ids,
    group_sessions,
    new_approvals,
    parse_state,
    protocol_mismatch,
)
from shiptray.core.polling import HIDDEN_POLL_MS, VISIBLE_POLL_MS, poll_interval_ms

TOKEN = "test-token-123"
NOW_MS = 1_000_000_000_000
UUID = "0b9d3c2e-1111-4222-8333-944455556666"

# What a protocol-3 hub (the Rust hub, phase 7) answers: one hub session with a
# pending approval and a standing rule, a background session, and a plain
# terminal session filed under `elsewhere`.
STATE_V3 = {
    "host": "linuxbox",
    "protocol": 3,
    "approvalsSupported": True,
    "root": "/home/me/code",
    "rootDisplay": "~/code",
    "home": "/home/me",
    "defaultPermissionMode": "auto",
    "permissionModes": ["auto", "acceptEdits", "plan", "manual", "bypassPermissions"],
    "now": NOW_MS,
    "projects": [
        {"name": "zeta", "path": "/home/me/code/zeta", "recent": [], "sessions": [
            {"key": "h:h2", "hubId": "h2", "pid": 300, "cwd": "/home/me/code/zeta",
             "status": "busy", "attachable": True, "background": False, "viewers": 1,
             "sessionId": "aaaa", "since": NOW_MS - 12_000, "startedAt": NOW_MS - 7_500_000,
             "approvals": [], "autoApprove": None},
        ]},
        {"name": "alpha", "path": "/home/me/code/alpha", "recent": [], "sessions": [
            {"key": "h:h1", "hubId": "h1", "pid": 200, "cwd": "/home/me/code/alpha",
             "status": "busy", "attachable": True, "background": False, "viewers": 0,
             "sessionId": UUID, "name": "alpha-b3", "title": "Fix the parser",
             "branch": "main", "since": NOW_MS - 150_000, "startedAt": NOW_MS - 9_240_000,
             "approvals": [{"id": "ap1", "tool": "Bash", "summary": "rm -rf build",
                            "detail": "rm -rf build\n# cleanup", "receivedAt": NOW_MS - 1000},
                           {"id": "ap2", "tool": "Bash", "summary": "make",
                            "detail": "make", "receivedAt": NOW_MS - 500}],
             "autoApprove": {"until": NOW_MS + 300_000}},
            {"key": "p:150", "pid": 150, "cwd": "/home/me/code/alpha",
             "status": "idle", "attachable": False, "background": True, "viewers": 0,
             "sessionId": "deadbeefcafe", "jobId": "job42", "approvals": [],
             "autoApprove": {"session": True}},
        ]},
    ],
    "elsewhere": [
        {"key": "p:99", "pid": 99, "cwd": "/tmp/scratch", "status": "waiting",
         "waitingFor": "permission", "attachable": False, "background": False, "viewers": 0,
         "approvals": [], "autoApprove": None},
    ],
}

# What today's Swift hub (protocol 2) answers: no approvals, no sessionId, a
# hub session Claude hasn't registered yet ("starting").
STATE_V2 = {
    "host": "mac", "protocol": 2, "root": "/Users/me/code", "rootDisplay": "~/code",
    "home": "/Users/me", "defaultPermissionMode": "auto", "permissionModes": ["auto"],
    "now": NOW_MS,
    "projects": [{"name": "p", "path": "/Users/me/code/p", "recent": [], "sessions": [
        {"key": "h:x", "hubId": "x", "pid": 5, "cwd": "/Users/me/code/p", "status": "starting",
         "attachable": True, "background": False, "viewers": 0, "startedAt": NOW_MS,
         "since": NOW_MS},
        {"key": "p:6", "pid": 6, "cwd": "/Users/me/code/p", "status": "shell",
         "attachable": False, "background": False, "viewers": 0, "branch": "dev"},
    ]}],
    "elsewhere": [],
}


class MockHub:
    """An in-process stand-in for the hub's HTTP API: serves `state` at
    /api/state, records every POST, refuses a wrong cookie with 401."""

    def __init__(self, state=None):
        self.state = copy.deepcopy(STATE_V3 if state is None else state)
        self.posts: list[tuple[str, dict]] = []
        self.headers: list[dict] = []
        self.launched = 0
        hub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _send(self, code, obj):
                body = json.dumps(obj).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _authorized(self):
                hub.headers.append(dict(self.headers))
                cookie = self.headers.get("Cookie", "")
                if f"claude_ship={TOKEN}" not in [c.strip() for c in cookie.split(";")]:
                    self._send(401, {"error": "not paired"})
                    return False
                return True

            def do_GET(self):
                if not self._authorized():
                    return
                if self.path == "/api/state":
                    self._send(200, hub.state)
                else:
                    self._send(404, {"error": "not found"})

            def do_POST(self):
                if not self._authorized():
                    return
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"{}")
                if self.headers.get("Content-Type") != "application/json":
                    self._send(415, {"error": "JSON only"})
                    return
                hub.posts.append((self.path, body))
                if self.path == "/api/launch":
                    hub.launched += 1
                    self._send(200, {"id": f"new{hub.launched}"})
                elif self.path == "/api/approve":
                    ids = approval_ids(parse_state(hub.state).sessions)
                    if body.get("id") in ids:
                        self._send(200, {"ok": True})
                    else:
                        self._send(404, {"error": "no such approval"})
                elif self.path == "/api/kill":
                    if body.get("id") == "gone":
                        self._send(404, {"error": "no such session"})
                    else:
                        self._send(200, {"ok": True})
                elif self.path == "/api/auto-approve":
                    self._send(200, {"ok": True})
                else:
                    self._send(404, {"error": "not found"})

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def make_home(self, token=TOKEN, socket=True) -> Path:
        home = Path(tempfile.mkdtemp(prefix="shiptray-home-"))
        (home / "config.json").write_text(json.dumps({"port": self.port, "root": "/x"}))
        (home / "token").write_text(token + "\n")
        if socket:
            (home / "hub.sock").write_text("")  # presence is all the applet checks
        return home

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class ModelTest(unittest.TestCase):
    def test_parse_v3(self):
        state = parse_state(STATE_V3)
        self.assertEqual(state.host, "linuxbox")
        self.assertEqual(state.protocol, 3)
        self.assertTrue(state.approvals_supported)
        self.assertEqual(state.home, "/home/me")
        self.assertEqual(len(state.sessions), 4)
        by_key = {s.key: s for s in state.sessions}
        h1 = by_key["h:h1"]
        self.assertEqual(h1.hub_id, "h1")
        self.assertEqual(h1.session_id, UUID)
        self.assertEqual(h1.title, "Fix the parser")
        self.assertEqual(h1.branch, "main")
        self.assertEqual([a.id for a in h1.approvals], ["ap1", "ap2"])
        self.assertEqual(h1.approvals[0].detail, "rm -rf build\n# cleanup")
        self.assertEqual(h1.approvals[0].received_at, NOW_MS - 1000)
        self.assertEqual(h1.auto_approve.until, NOW_MS + 300_000)
        self.assertFalse(h1.auto_approve.session)
        self.assertTrue(by_key["p:150"].auto_approve.session)
        self.assertIsNone(by_key["h:h2"].auto_approve)
        self.assertEqual(by_key["p:99"].waiting_for, "permission")

    def test_parse_v2_has_no_approvals(self):
        state = parse_state(STATE_V2)
        self.assertEqual(state.protocol, 2)
        self.assertFalse(state.approvals_supported)
        self.assertTrue(all(s.approvals == () and s.auto_approve is None
                            for s in state.sessions))
        self.assertTrue(protocol_mismatch(state, PROTOCOL))
        self.assertFalse(protocol_mismatch(parse_state(STATE_V3), PROTOCOL))

    def test_stable_order_is_cwd_then_pid(self):
        keys = [s.key for s in parse_state(STATE_V3).sessions]
        self.assertEqual(keys, ["p:150", "h:h1", "h:h2", "p:99"])

    def test_lenient_parsing(self):
        state = parse_state({"protocol": "3", "projects": [{"sessions": [
            "junk", {"pid": "x", "cwd": "/a"}, {"pid": 1},
            {"pid": 2, "cwd": "/b", "status": 7, "approvals": [{"no": "id"}, 5],
             "autoApprove": "yes", "background": "true"}]}],
            "elsewhere": None})
        self.assertIsNone(state.protocol)
        self.assertEqual(len(state.sessions), 1)
        s = state.sessions[0]
        self.assertEqual((s.key, s.status, s.approvals, s.auto_approve, s.background),
                         ("p:2", "idle", (), None, False))
        self.assertEqual(parse_state(None).sessions, ())
        self.assertEqual(parse_state([]).sessions, ())

    def test_duplicate_keys_once(self):
        entry = {"key": "p:1", "pid": 1, "cwd": "/a", "status": "idle"}
        state = parse_state({"projects": [{"sessions": [entry]}], "elsewhere": [entry]})
        self.assertEqual(len(state.sessions), 1)

    def test_grouping(self):
        groups = group_sessions(parse_state(STATE_V3).sessions)
        self.assertEqual([(g, [s.key for s in rows]) for g, rows in groups],
                         [(VIRTUAL, ["h:h1", "h:h2"]), (BACKGROUND, ["p:150"]),
                          (TERMINAL_ONLY, ["p:99"])])
        self.assertEqual(group_sessions([]), [])
        only_terminal = group_sessions(parse_state(STATE_V2).sessions)
        self.assertEqual([g for g, _ in only_terminal], [VIRTUAL, TERMINAL_ONLY])

    def test_effective_status_and_derived(self):
        by_key = {s.key: s for s in parse_state(STATE_V3).sessions}
        self.assertEqual(by_key["h:h1"].status, "busy")
        self.assertEqual(by_key["h:h1"].effective_status, "waiting")
        self.assertEqual(by_key["h:h1"].project_name, "alpha")
        self.assertEqual(by_key["p:150"].attach_id, "job42")
        no_job = parse_state({"elsewhere": [{"pid": 1, "cwd": "/a", "background": True,
                                             "sessionId": "deadbeefcafe"}]}).sessions[0]
        self.assertEqual(no_job.attach_id, "deadbeef")

    def test_matches(self):
        h1 = {s.key: s for s in parse_state(STATE_V3).sessions}["h:h1"]
        for needle in ("", "ALPHA", "parser", "main", "alpha-b3", "rm -rf", "code/al"):
            self.assertTrue(h1.matches(needle), needle)
        self.assertFalse(h1.matches("zeta"))

    def test_new_approvals(self):
        sessions = parse_state(STATE_V3).sessions
        fresh = new_approvals(set(), sessions)
        self.assertEqual([a.id for _, a in fresh], ["ap1", "ap2"])
        self.assertEqual(fresh[0][0].key, "h:h1")
        self.assertEqual(new_approvals({"ap1", "ap2"}, sessions), [])
        self.assertEqual([a.id for _, a in new_approvals({"ap1"}, sessions)], ["ap2"])
        self.assertEqual(approval_ids(sessions), {"ap1", "ap2"})


class GlyphTest(unittest.TestCase):
    def sessions(self, *statuses, approvals=()):
        entries = [{"pid": i, "cwd": f"/p{i}", "status": st} for i, st in enumerate(statuses)]
        if approvals:
            entries[0]["approvals"] = [{"id": "a"}]
        return parse_state({"elsewhere": entries}).sessions

    def test_precedence(self):
        self.assertEqual(tray_glyph(()), "idle")
        self.assertEqual(tray_glyph(self.sessions("idle", "idle")), "idle")
        self.assertEqual(tray_glyph(self.sessions("idle", "busy")), "busy")
        self.assertEqual(tray_glyph(self.sessions("busy", "waiting")), "waiting")
        self.assertEqual(tray_glyph(self.sessions("waiting", "busy", "idle")), "waiting")

    def test_pending_approval_beats_busy(self):
        self.assertEqual(tray_glyph(self.sessions("busy", approvals=True)), "waiting")

    def test_never_a_false_alarm(self):
        for status in ("shell", "starting", "mystery"):
            self.assertEqual(tray_glyph(self.sessions(status)), "idle", status)

    def test_tooltip(self):
        self.assertEqual(tooltip(()), "ClaudeShip — no sessions")
        self.assertEqual(tooltip(self.sessions("busy", "waiting", "idle")),
                         "ClaudeShip — 3 sessions, 1 waiting, 1 working")
        self.assertIn("not reachable", tooltip((), connected=False))


class FormatsTest(unittest.TestCase):
    # The Swift self-test's vectors (SelfTest.swift, "MARK: formatting").
    NOW = 1_000_000

    def test_compact_age_matches_swift(self):
        self.assertEqual(compact_age(self.NOW - 12, self.NOW), "12s")
        self.assertEqual(compact_age(self.NOW - 150, self.NOW), "2m")
        self.assertEqual(compact_age(self.NOW - 7500, self.NOW), "2h 5m")

    def test_compact_duration_matches_swift(self):
        self.assertEqual(compact_duration(0, 59), "0m")
        self.assertEqual(compact_duration(0, 9240), "2h 34m")

    def test_edges(self):
        self.assertEqual(compact_age(self.NOW + 5, self.NOW), "0s")  # clock skew
        self.assertEqual(compact_age(self.NOW - 59.9, self.NOW), "59s")
        self.assertEqual(compact_age(self.NOW - 60, self.NOW), "1m")

    def test_activity_text(self):
        now_s = NOW_MS / 1000
        self.assertEqual(activity_text(NOW_MS - 12_000, NOW_MS - 7_500_000, now_s),
                         "for 12s · up 2h 5m")
        self.assertEqual(activity_text(None, None, now_s), "—")
        self.assertEqual(activity_text(None, NOW_MS - 120_000, now_s), "up 2m")

    def test_abbreviate_path(self):
        self.assertEqual(abbreviate_path("/home/me/code/x", "/home/me"), "~/code/x")
        self.assertEqual(abbreviate_path("/home/me", "/home/me"), "~")
        self.assertEqual(abbreviate_path("/home/meg/x", "/home/me"), "/home/meg/x")
        self.assertEqual(abbreviate_path("/tmp/x", "/home/me/"), "/tmp/x")

    def test_state_text(self):
        self.assertEqual(state_text("busy"), "Working")
        self.assertEqual(state_text("shell"), "Shell command")
        self.assertEqual(state_text("idle"), "Idle")
        self.assertEqual(state_text("waiting"), "Waiting for input")
        self.assertEqual(state_text("waiting", "permission"), "Waiting: permission")
        self.assertEqual(state_text("starting"), "Starting")
        self.assertEqual(state_text("whatever"), "Idle")

    def test_pending_caption(self):
        self.assertEqual(pending_caption("ls", 1), "ls")
        self.assertEqual(pending_caption("ls", 3), "ls · +2 more")


class PollingTest(unittest.TestCase):
    def test_intervals(self):
        self.assertEqual(poll_interval_ms(True), VISIBLE_POLL_MS)
        self.assertEqual(poll_interval_ms(False), HIDDEN_POLL_MS)
        self.assertEqual((VISIBLE_POLL_MS, HIDDEN_POLL_MS), (2000, 5000))


class TerminalTest(unittest.TestCase):
    @staticmethod
    def which_of(*present):
        return lambda name, path=None: f"/usr/bin/{name}" if name in present else None

    BOX = "/run/.containerenv"

    def cmd(self, env=None, present=(), markers=(), containerenv="", argv=("a", "b")):
        return terminal.terminal_command(
            list(argv), "/w", env=env or {}, which=self.which_of(*present),
            exists=lambda p: p in markers, read_text=lambda p: containerenv)

    def test_terminal_env_first(self):
        env = {"TERMINAL": "ghostty"}
        self.assertEqual(self.cmd(env, ["ghostty", "konsole"]),
                         ["ghostty", "--working-directory=/w", "-e", "a", "b"])
        self.assertEqual(self.cmd({"TERMINAL": "konsole"}, ["ghostty", "konsole"]),
                         ["konsole", "--workdir", "/w", "-e", "a", "b"])
        self.assertEqual(self.cmd({"TERMINAL": "kitty --x"}, ["kitty"]),
                         ["kitty", "--x", "--directory", "/w", "-e", "a", "b"])

    def test_terminal_env_unknown_emulator_cds_inside(self):
        self.assertEqual(self.cmd({"TERMINAL": "st"}, ["st", "ghostty"]),
                         ["st", "-e", "sh", "-c", 'cd "$1" && shift && exec "$@"',
                          "sh", "/w", "a", "b"])

    def test_terminal_env_missing_from_path_is_skipped(self):
        self.assertEqual(self.cmd({"TERMINAL": "nope"}, ["ghostty"]),
                         ["ghostty", "--working-directory=/w", "-e", "a", "b"])
        self.assertEqual(self.cmd({"TERMINAL": "  "}, ["konsole"]),
                         ["konsole", "--workdir", "/w", "-e", "a", "b"])

    def test_ghostty_before_konsole(self):
        self.assertEqual(self.cmd({}, ["konsole", "ghostty", "xdg-terminal-exec"],
                                  argv=("claudeship", "hub", "attach", "h1")),
                         ["ghostty", "--working-directory=/w", "-e",
                          "claudeship", "hub", "attach", "h1"])

    def test_konsole_on_path(self):
        self.assertEqual(self.cmd({}, ["konsole", "distrobox-host-exec"], markers=[self.BOX]),
                         ["konsole", "--workdir", "/w", "-e", "a", "b"])

    def test_host_konsole_through_distrobox(self):
        want = ["distrobox-host-exec", "konsole", "--workdir", "/w", "-e",
                "distrobox-enter", "-n", "box1", "--", "a", "b"]
        self.assertEqual(self.cmd({"CONTAINER_ID": "box1"}, ["distrobox-host-exec"],
                                  markers=[self.BOX]), want)
        self.assertEqual(self.cmd({}, ["distrobox-host-exec"], markers=["/.dockerenv"],
                                  containerenv='engine="podman"\nname="box1"\nid="x"\n'), want)

    def test_host_konsole_needs_container_box_name_and_helper(self):
        # not in a container
        self.assertIsNone(self.cmd({"CONTAINER_ID": "b"}, ["distrobox-host-exec"]))
        # no name to enter
        self.assertIsNone(self.cmd({}, ["distrobox-host-exec"], markers=[self.BOX]))
        # no distrobox-host-exec
        self.assertIsNone(self.cmd({"CONTAINER_ID": "b"}, [], markers=[self.BOX]))

    def test_then_xdg_terminal_exec(self):
        self.assertEqual(self.cmd({}, ["xdg-terminal-exec"]), ["xdg-terminal-exec", "a", "b"])
        self.assertEqual(self.cmd({"CONTAINER_ID": "b"}, ["xdg-terminal-exec"],
                                  markers=[self.BOX]), ["xdg-terminal-exec", "a", "b"])

    def test_none(self):
        self.assertIsNone(self.cmd({}, []))

    def test_container_name(self):
        self.assertEqual(terminal.container_name({"CONTAINER_ID": " x "}, lambda p: ""), "x")
        self.assertEqual(terminal.container_name({}, lambda p: "name=y\n"), "y")
        self.assertEqual(terminal.container_name({}, lambda p: ""), "")

    def test_resolve_binary_falls_back_to_local_bin(self):
        env = {"HOME": "/home/me", "PATH": "/usr/bin"}
        self.assertEqual(terminal.resolve_binary("claudeship", env, which=self.which_of(),
                                                 exists=lambda p: p.startswith("/home/me/.local")),
                         "/home/me/.local/bin/claudeship")
        self.assertEqual(terminal.resolve_binary("claude", env, which=self.which_of("claude"),
                                                 exists=lambda p: False), "/usr/bin/claude")
        self.assertEqual(terminal.resolve_binary("claude", env, which=self.which_of(),
                                                 exists=lambda p: False), "claude")

    def test_argv(self):
        self.assertEqual(terminal.attach_argv("cs", "h1"), ["cs", "hub", "attach", "h1"])
        self.assertEqual(terminal.background_attach_argv("c", "j"), ["c", "attach", "j"])
        self.assertEqual(terminal.hub_start_argv("cs"), ["cs", "hub", "start"])


class HubHomeTest(unittest.TestCase):
    def test_resolution(self):
        self.assertEqual(hub_home({"CLAUDESHIP_HOME": "/x/h", "XDG_STATE_HOME": "/s"}, "linux"),
                         Path("/x/h"))
        self.assertEqual(hub_home({"XDG_STATE_HOME": "/s", "HOME": "/home/me"}, "linux"),
                         Path("/s/claudeship"))
        self.assertEqual(hub_home({"HOME": "/home/me"}, "linux"),
                         Path("/home/me/.local/state/claudeship"))
        self.assertEqual(hub_home({"HOME": "/Users/me"}, "darwin"),
                         Path("/Users/me/Library/Application Support/ClaudeShip/hub"))

    def test_port_and_token(self):
        home = Path(tempfile.mkdtemp())
        self.assertEqual(read_port(home), 7433)
        self.assertEqual(read_token(home), "")
        (home / "config.json").write_text('{"port": 8123}')
        (home / "token").write_text("  abc\n")
        self.assertEqual(read_port(home), 8123)
        self.assertEqual(read_token(home), "abc")
        self.assertEqual(web_url(home), "http://localhost:8123")
        self.assertEqual(auth_url(home), "http://localhost:8123/auth?k=abc")
        (home / "token").write_text("a+b/c=\n")
        self.assertEqual(auth_url(home), "http://localhost:8123/auth?k=a%2Bb%2Fc%3D")
        (home / "token").unlink()
        self.assertEqual(auth_url(home), "http://localhost:8123")
        for bad in ('{"port": "80"}', '{"port": true}', '{"port": 70000}', "[1]", "nope"):
            (home / "config.json").write_text(bad)
            self.assertEqual(read_port(home), 7433, bad)


class HubClientTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.hub = MockHub()
        cls.home = cls.hub.make_home()

    @classmethod
    def tearDownClass(cls):
        cls.hub.close()

    def setUp(self):
        self.hub.posts.clear()
        self.hub.headers.clear()
        self.client = HubClient.from_home(self.home)

    def test_base_url_and_cookie(self):
        self.assertEqual(self.client.base_url, f"http://localhost:{self.hub.port}")
        state = parse_state(self.client.state())
        self.assertEqual(state.host, "linuxbox")
        sent = self.hub.headers[-1]
        self.assertEqual(sent.get("Cookie"), f"claude_ship={TOKEN}")
        self.assertNotIn("Origin", sent)

    def test_posts(self):
        self.client.kill("h1")
        self.assertEqual(self.client.launch("/home/me", "auto"), "new1")
        self.client.approve("ap1", True)
        self.client.approve("ap2", False)
        self.client.auto_approve(UUID, "5m")
        self.client.auto_approve(UUID, "off")
        self.assertEqual(self.hub.posts, [
            ("/api/kill", {"id": "h1"}),
            ("/api/launch", {"path": "/home/me", "permissionMode": "auto"}),
            ("/api/approve", {"id": "ap1", "allow": True}),
            ("/api/approve", {"id": "ap2", "allow": False}),
            ("/api/auto-approve", {"sessionId": UUID, "rule": "5m"}),
            ("/api/auto-approve", {"sessionId": UUID, "rule": "off"}),
        ])
        with self.assertRaises(ValueError):
            self.client.auto_approve(UUID, "forever")

    def test_refusal_carries_the_hubs_reason(self):
        with self.assertRaises(Refused) as cm:
            self.client.kill("gone")
        self.assertEqual(str(cm.exception), "no such session")
        self.assertEqual(cm.exception.status, 404)

    def test_approval_already_answered_is_not_an_error(self):
        # 404 from /api/approve: the terminal or another screen answered first.
        self.client.approve("gone", True)
        self.assertEqual(self.hub.posts[-1], ("/api/approve", {"id": "gone", "allow": True}))

    def test_wrong_token_is_unpaired(self):
        with self.assertRaises(Unpaired):
            HubClient.from_home(self.hub.make_home(token="wrong")).state()
        with self.assertRaises(Unpaired):
            HubClient(self.client.base_url, "").state()

    def test_never_through_a_proxy(self):
        saved = {k: os.environ.get(k) for k in ("http_proxy", "HTTP_PROXY", "no_proxy", "NO_PROXY")}
        try:
            for k in saved:
                os.environ.pop(k, None)
            os.environ["http_proxy"] = os.environ["HTTP_PROXY"] = "http://127.0.0.1:1"
            self.assertEqual(parse_state(self.client.state()).host, "linuxbox")
        finally:
            for k, v in saved.items():
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v

    def test_unreachable(self):
        home = Path(tempfile.mkdtemp())
        (home / "token").write_text("t")
        (home / "config.json").write_text('{"port": 1}')
        with self.assertRaises(Unreachable):
            HubClient.from_home(home).state()


if __name__ == "__main__":
    os.environ.setdefault("PYTHONPATH", ".")
    unittest.main()
