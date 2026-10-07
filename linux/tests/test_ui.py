"""Offscreen UI tests: popup grouping, filter, approval controls, and the
whole app against the mock hub, plus a `--smoke-test` run of the entry point.

Run (from linux/): PYTHONPATH=. .venv/bin/python -m unittest discover tests
"""
import copy
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from PySide6.QtCore import Qt
from PySide6.QtTest import QTest
from PySide6.QtWidgets import QApplication

from shiptray.core.model import parse_state
from shiptray.ui.popup import Popup
from tests.test_core import NOW_MS, STATE_V2, STATE_V3, UUID, MockHub

ROOT = Path(__file__).resolve().parent.parent


def wait_until(condition, timeout_ms=3000, step_ms=20):
    """Process events until condition() is true or timeout; returns success."""
    waited = 0
    while waited <= timeout_ms:
        if condition():
            return True
        QTest.qWait(step_ms)
        waited += step_ms
    return condition()


def fixed_now():
    return NOW_MS / 1000


class PopupTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.qapp = QApplication.instance() or QApplication([])

    def setUp(self):
        self.popup = Popup(now=fixed_now, home="/home/me")
        self.popup.set_sessions(parse_state(STATE_V3).sessions, host="linuxbox")

    def tearDown(self):
        self.popup.deleteLater()

    def visible_keys(self):
        return [s.key for _, rows in self.popup.grouped() for s in rows
                if not self.popup.row(s.key).isHidden()]

    def headers(self):
        return [(h.label.text(), not h.isHidden()) for h in self.popup._section_headers]

    def test_grouping(self):
        self.assertEqual([(g, [s.key for s in rows]) for g, rows in self.popup.grouped()],
                         [("Virtual", ["h:h1", "h:h2"]), ("Background", ["p:150"]),
                          ("Terminal only", ["p:99"])])
        self.assertEqual(self.headers(), [("Virtual · 2", True), ("Background · 1", True),
                                          ("Terminal only · 1", True)])
        # layout order matches: header, its rows, next header…
        layout = self.popup.list_layout
        widgets = [layout.itemAt(i).widget() for i in range(layout.count())]
        def describe(w):
            if hasattr(w, "key"):
                return w.key
            return w.label.text() if hasattr(w, "label") else None
        keys = [describe(w) for w in widgets if w is not None]
        self.assertEqual([k for k in keys if k], ["Virtual · 2", "h:h1", "h:h2",
                                                  "Background · 1", "p:150",
                                                  "Terminal only · 1", "p:99"])
        self.assertTrue(self.popup.placeholder.isHidden())
        self.assertEqual(self.popup.footer_stats.text(), "4 sessions · 2 waiting")

    def test_filter(self):
        self.popup.search.setText("zeta")
        self.assertEqual(self.visible_keys(), ["h:h2"])
        self.assertEqual([v for _, v in self.headers()], [True, False, False])
        self.popup.search.setText("scratch")
        self.assertEqual(self.visible_keys(), ["p:99"])
        self.popup.search.clear()
        self.assertEqual(len(self.visible_keys()), 4)

    def test_incremental_update_keeps_rows(self):
        row = self.popup.row("h:h2")
        state = copy.deepcopy(STATE_V3)
        state["projects"][0]["sessions"][0]["status"] = "idle"
        self.popup.set_sessions(parse_state(state).sessions)
        self.assertIs(self.popup.row("h:h2"), row)
        self.assertEqual(row.status_label.text(), "Idle")
        state["elsewhere"] = []
        self.popup.set_sessions(parse_state(state).sessions)
        self.assertIsNone(self.popup.row("p:99"))
        self.assertEqual([g for g, _ in self.popup.grouped()], ["Virtual", "Background"])

    def test_row_content(self):
        row = self.popup.row("h:h2")
        self.assertEqual(row.name_label.text(), "zeta")
        self.assertEqual(row.path_label.text(), "~/code/zeta")
        self.assertTrue(row.title_label.isHidden())
        self.assertTrue(row.branch_label.isHidden())
        self.assertTrue(row.bolt.isHidden())
        self.assertEqual(row.status_label.text(), "Working")
        self.assertEqual(row.activity_label.text(), "for 12s · up 2h 5m")
        bg = self.popup.row("p:150")
        self.assertFalse(bg.bolt.isHidden())  # {session: true}
        waiting = self.popup.row("p:99")
        self.assertEqual(waiting.status_label.text(), "Waiting: permission")
        self.assertEqual(waiting.path_label.text(), "/tmp/scratch")

    def test_approval_controls_replace_the_status(self):
        row = self.popup.row("h:h1")
        self.assertFalse(row.approval_box.isHidden())
        self.assertTrue(row.status_box.isHidden())
        self.assertEqual(row.pending_label.text(), "rm -rf build · +1 more")
        self.assertIn("rm -rf build\n# cleanup", row.pending_label.toolTip())
        self.assertEqual(row.title_label.text(), "Fix the parser")
        self.assertEqual(row.branch_label.text(), "⎇ main")
        self.assertFalse(row.bolt.isHidden())
        self.assertTrue(row.act_5m.isEnabled() and row.act_session.isEnabled())
        self.assertTrue(row.act_off.isEnabled())  # a rule is set
        self.assertTrue(self.popup.row("h:h2").approval_box.isHidden())

        got = []
        self.popup.approve_clicked.connect(lambda i, allow: got.append(("v", i, allow)))
        self.popup.auto_approve_requested.connect(lambda sid, r: got.append(("r", sid, r)))
        QTest.mouseClick(row.approve_btn, Qt.LeftButton)
        QTest.mouseClick(row.deny_btn, Qt.LeftButton)
        row.act_5m.trigger()
        row.act_session.trigger()
        row.act_off.trigger()
        self.assertEqual(got, [("v", "ap1", True), ("v", "ap1", False), ("r", UUID, "5m"),
                               ("r", UUID, "session"), ("r", UUID, "off")])

    def test_rules_need_a_session_id(self):
        state = {"elsewhere": [{"pid": 1, "cwd": "/a", "status": "waiting",
                                "approvals": [{"id": "x", "summary": "ls"}]}]}
        self.popup.set_sessions(parse_state(state).sessions)
        row = self.popup.row("p:1")
        self.assertFalse(row.act_5m.isEnabled())
        self.assertFalse(row.act_off.isEnabled())

    def test_context_menu(self):
        def entries(key):
            menu = self.popup.row(key).build_context_menu()
            return [(a.text(), a.isEnabled()) for a in menu.actions() if a.text()]
        self.assertEqual(entries("h:h2"), [("End session", True)])
        self.assertEqual(entries("p:99"), [("End session", False)])
        self.assertEqual(entries("h:h1"), [("End session", True), ("Stop auto-approving", True)])
        ended = []
        self.popup.end_requested.connect(ended.append)
        self.popup.row("h:h2").build_context_menu().actions()[0].trigger()
        self.assertEqual(ended, ["h:h2"])

    def test_row_click(self):
        clicked = []
        self.popup.session_clicked.connect(clicked.append)
        row = self.popup.row("h:h2")
        QTest.mouseClick(row, Qt.LeftButton, pos=row.rect().center())
        self.assertEqual(clicked, ["h:h2"])

    def test_untrusted_text_is_plain(self):
        state = {"elsewhere": [{"pid": 1, "cwd": "/a", "status": "idle",
                                "title": "<b>bold</b><img src=x>", "branch": "<i>b</i>"}]}
        self.popup.set_sessions(parse_state(state).sessions)
        row = self.popup.row("p:1")
        for label in (row.name_label, row.title_label, row.branch_label, row.status_label,
                      row.pending_label, row.path_label):
            self.assertEqual(label.textFormat(), Qt.PlainText)
        self.assertEqual(row.title_label.text(), "<b>bold</b><img src=x>")
        # Tooltips have no plain-text mode: untrusted text goes in escaped.
        state = {"elsewhere": [{"pid": 2, "cwd": "/a/<b>x</b>", "status": "busy",
                                "name": "<img src=n>", "approvals": [
                                    {"id": "q", "summary": "s", "detail": "echo <b>hi</b> & bye"}]}]}
        self.popup.set_sessions(parse_state(state).sessions)
        row = self.popup.row("p:2")
        for tip in (row.pending_label.toolTip(), row.path_label.toolTip(),
                    row.name_label.toolTip()):
            self.assertNotIn("<b>", tip)
            self.assertNotIn("<img", tip)
        self.assertIn("echo &lt;b&gt;hi&lt;/b&gt; &amp; bye", row.pending_label.toolTip())
        self.popup.set_error("<b>refused</b>")
        self.assertNotIn("<b>", self.popup.status_label.toolTip())

    def test_empty_and_error(self):
        self.popup.set_sessions(())
        self.assertFalse(self.popup.placeholder.isHidden())
        self.assertIn("No Claude sessions", self.popup.placeholder.text())
        self.popup.set_error("Can't reach the hub")
        self.assertEqual(self.popup.status_label.text(), "● Disconnected")
        self.assertEqual(self.popup.placeholder.text(), "Can't reach the hub")

    def test_escape_clears_search_then_closes(self):
        self.popup.show()
        self.popup.search.setText("x")
        QTest.keyClick(self.popup, Qt.Key_Escape)
        self.assertEqual(self.popup.search.text(), "")
        self.assertTrue(self.popup.isVisible())
        QTest.keyClick(self.popup, Qt.Key_Escape)
        self.assertFalse(self.popup.isVisible())


class AppTest(unittest.TestCase):
    """The whole app against the mock hub. Spawning and URL opening are
    injected, so nothing here starts a hub or a terminal."""

    @classmethod
    def setUpClass(cls):
        cls.qapp = QApplication.instance() or QApplication([])

    def setUp(self):
        self.hub = MockHub()
        self.spawned = []
        self.opened = []
        self._env = dict(os.environ)
        os.environ["TERMINAL"] = "fake-term"
        bindir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, bindir, True)
        fake = Path(bindir) / "fake-term"
        fake.write_text("#!/bin/sh\n")
        fake.chmod(0o755)
        os.environ["PATH"] = bindir  # only $TERMINAL is findable

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._env)
        self.hub.close()

    def make_app(self, home=None):
        from shiptray.ui.app import ShipApp
        app = ShipApp(self.qapp, home=home or self.hub.make_home(),
                      spawn=lambda argv, cwd: self.spawned.append((argv, cwd)),
                      open_url=self.opened.append, now=fixed_now,
                      single_instance=False)
        self.messages = []
        app.tray.showMessage = lambda title, body, *a: self.messages.append((title, body))
        self.addCleanup(app.popup.deleteLater)
        self.addCleanup(app.timer.stop)
        return app

    def test_starts_the_hub_only_when_the_socket_is_absent(self):
        self.make_app(home=self.hub.make_home(socket=True))
        self.assertEqual(self.spawned, [])
        self.make_app(home=self.hub.make_home(socket=False))
        self.assertEqual(len(self.spawned), 1)
        argv, _cwd = self.spawned[0]
        self.assertEqual(argv[-2:], ["hub", "start"])
        self.assertTrue(argv[0].endswith("claudeship"))

    def test_poll_glyph_rows_and_notification(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        self.assertEqual(app.tray_glyph(), "waiting")
        self.assertEqual(len(app.popup._rows), 4)
        self.assertTrue(app.popup.banner.isHidden())
        self.assertEqual(len(self.messages), 1)
        self.assertEqual(self.messages[0], ("Permission requested — alpha",
                                            "rm -rf build\n+1 more"))
        # the same pendings on the next poll don't notify again
        app.poll()
        self.assertTrue(wait_until(lambda: not app._polling))
        self.assertEqual(len(self.messages), 1)

    def test_notification_body_is_escaped(self):
        # notification servers read the body as markup
        self.hub.state["projects"][1]["sessions"][0]["approvals"][0]["summary"] = "cat <a & b"
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        self.assertEqual(self.messages[0][1], "cat &lt;a &amp; b\n+1 more")

    def test_glyph_follows_the_state(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        for s in self.hub.state["projects"][1]["sessions"]:
            s["approvals"] = []
        self.hub.state["elsewhere"] = []
        app.poll()
        self.assertTrue(wait_until(lambda: app.tray_glyph() == "busy"))
        for p in self.hub.state["projects"]:
            for s in p["sessions"]:
                s["status"] = "idle"
        app.poll()
        self.assertTrue(wait_until(lambda: app.tray_glyph() == "idle"))

    def test_approval_buttons_reach_the_hub(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.popup.row("h:h1") is not None))
        row = app.popup.row("h:h1")
        QTest.mouseClick(row.approve_btn, Qt.LeftButton)
        self.assertTrue(wait_until(lambda: self.hub.posts))
        QTest.mouseClick(row.deny_btn, Qt.LeftButton)
        self.assertTrue(wait_until(lambda: len(self.hub.posts) == 2))
        for i, action in enumerate((row.act_5m, row.act_session, row.act_off)):
            action.setEnabled(True)  # "Stop approving" is only offered with a rule set
            action.trigger()
            self.assertTrue(wait_until(lambda n=i: len(self.hub.posts) == 3 + n))
        self.assertEqual(self.hub.posts, [("/api/approve", {"id": "ap1", "allow": True}),
                                          ("/api/approve", {"id": "ap1", "allow": False}),
                                          ("/api/auto-approve",
                                           {"sessionId": UUID, "rule": "5m"}),
                                          ("/api/auto-approve",
                                           {"sessionId": UUID, "rule": "session"}),
                                          ("/api/auto-approve",
                                           {"sessionId": UUID, "rule": "off"})])

    def test_end_session(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        app.end_session("p:99")  # terminal only: nothing to send
        app.end_session("h:h2")
        self.assertTrue(wait_until(lambda: self.hub.posts))
        self.assertEqual(self.hub.posts, [("/api/kill", {"id": "h2"})])

    def test_row_clicks_open_terminals(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        app.open_session("p:99")  # terminal only: no focus
        self.assertEqual(self.spawned, [])
        app.open_session("h:h2")
        argv, _cwd = self.spawned[-1]
        self.assertEqual(argv[:2], ["fake-term", "-e"])
        self.assertEqual(argv[-3:], ["hub", "attach", "h2"])
        app.open_session("p:150")
        argv, _cwd = self.spawned[-1]
        self.assertEqual(argv[-2:], ["attach", "job42"])

    def test_quick_plus_launches_home_then_attaches(self):
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        app.new_home_session()
        self.assertTrue(wait_until(lambda: self.spawned))
        self.assertEqual(self.hub.posts[0],
                         ("/api/launch", {"path": "/home/me", "permissionMode": "auto"}))
        self.assertEqual(self.spawned[0][0][-3:], ["hub", "attach", "new1"])

    def test_open_web(self):
        app = self.make_app()
        app.open_web()
        # the pairing link, so the browser pairs itself on the way in
        self.assertEqual(self.opened, [f"http://localhost:{self.hub.port}/auth?k=test-token-123"])

    def test_protocol_banner(self):
        self.hub.state = copy.deepcopy(STATE_V2)
        app = self.make_app()
        self.assertTrue(wait_until(lambda: app.connected))
        self.assertFalse(app.popup.banner.isHidden())
        self.assertIn("protocol 2", app.popup.banner_label.text())
        self.assertEqual(app.tray_glyph(), "idle")  # starting + shell: no alarm

    def test_unreachable(self):
        home = self.hub.make_home()
        (home / "config.json").write_text('{"port": 1}')
        app = self.make_app(home=home)
        self.assertTrue(wait_until(lambda: not app._polling))
        self.assertFalse(app.connected)
        self.assertEqual(app.popup.status_label.text(), "● Disconnected")
        self.assertEqual(app.tray_glyph(), "idle")

    def test_poll_interval_follows_the_popup(self):
        app = self.make_app()
        self.assertEqual(app.timer.interval(), 5000)
        app.popup.show()
        self.assertEqual(app.timer.interval(), 2000)
        app.popup.hide()
        self.assertEqual(app.timer.interval(), 5000)


class SmokeTest(unittest.TestCase):
    def test_entry_point_runs_and_quits(self):
        hub = MockHub()
        self.addCleanup(hub.close)
        env = dict(os.environ, QT_QPA_PLATFORM="offscreen",
                   CLAUDESHIP_HOME=str(hub.make_home()),
                   PYTHONPATH=str(ROOT))
        result = subprocess.run([sys.executable, "-m", "shiptray", "--smoke-test"],
                                cwd=ROOT, env=env, capture_output=True, text=True,
                                timeout=30, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
