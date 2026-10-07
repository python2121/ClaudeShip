"""One session in the popup, the Mac overlay's row in Plasma clothes.

Left: an 8 px state dot. Middle: project name (bold, with a bolt when a
standing auto-approve rule exists), path, the conversation title in italics,
branch. Right: state + how long it has held over "for 12s · up 2h 5m" — or,
while a permission request is pending, Approve / Deny / ⋯ with the request's
summary as the caption and its full detail as the tooltip.

Every label is Qt.PlainText: titles, branches and command summaries are
untrusted text and must never be read as rich text.
"""
from __future__ import annotations

import html
import time

from PySide6.QtCore import QEasingCurve, QEvent, Qt, QVariantAnimation, Signal
from PySide6.QtGui import QAction, QPainter, QPalette
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QMenu,
    QPushButton,
    QSizePolicy,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from ..core.formats import abbreviate_path, activity_text, pending_caption, state_text
from ..core.model import Session
from .style import argb, small_font, state_color

HOVER_MS = 50


def plain_label(text: str = "", small: bool = False, subtitle: bool = False) -> QLabel:
    label = QLabel()
    label.setTextFormat(Qt.PlainText)
    label.setText(text)
    if small:
        label.setFont(small_font())
    if subtitle:
        label.setProperty("class", "subtitle")
    return label


def plain_tooltip(text: str | None) -> str:
    """A tooltip that shows `text` verbatim. Qt renders a tooltip as rich
    text whenever it *looks* like HTML (`Qt::mightBeRichText`), and there is
    no plain-text flag, so untrusted text goes in escaped, as explicit rich
    text that keeps its line breaks."""
    if not text:
        return ""
    return f"<p style='white-space:pre-wrap'>{html.escape(text, quote=False)}</p>"


class Dot(QWidget):
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFixedSize(8, 8)
        self._color = None

    def set_color(self, color):
        self._color = color
        self.update()

    def paintEvent(self, _event):
        if self._color is None:
            return
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        p.setPen(Qt.NoPen)
        p.setBrush(self._color)
        p.drawEllipse(self.rect())
        p.end()


class SessionRow(QWidget):
    clicked = Signal(str)                  # session key
    end_requested = Signal(str)            # session key
    approve_clicked = Signal(str, bool)    # approval id, allow
    auto_approve_requested = Signal(str, str)  # Claude session id, rule

    def __init__(self, session: Session, now=time.time, home: str | None = None, parent=None):
        super().__init__(parent)
        self.key = session.key
        self._s = session
        self._now = now
        self._home = home
        self._hover = 0.0
        self.setAttribute(Qt.WA_Hover, True)
        self._hover_anim = QVariantAnimation(self, duration=HOVER_MS,
                                             easingCurve=QEasingCurve.OutQuad)
        self._hover_anim.valueChanged.connect(self._set_hover)

        self.dot = Dot()
        dot_col = QVBoxLayout()
        dot_col.setContentsMargins(0, 6, 0, 0)
        dot_col.addWidget(self.dot)
        dot_col.addStretch(1)

        self.name_label = plain_label()
        bold = self.name_label.font()
        bold.setBold(True)
        self.name_label.setFont(bold)
        self.bolt = plain_label("⚡", small=True)
        self.bolt.setObjectName("bolt")
        self.bolt.setToolTip("Auto-approving permission requests for this session")
        name_row = QHBoxLayout()
        name_row.setSpacing(4)
        name_row.addWidget(self.name_label, 1)
        name_row.addWidget(self.bolt)

        self.path_label = plain_label(small=True, subtitle=True)
        self.title_label = plain_label(small=True, subtitle=True)
        italic = self.title_label.font()
        italic.setItalic(True)
        self.title_label.setFont(italic)
        self.branch_label = plain_label(small=True, subtitle=True)
        for label in (self.name_label, self.path_label, self.title_label, self.branch_label):
            # never let long text widen the popup; the layout decides the width
            label.setSizePolicy(QSizePolicy.Ignored, QSizePolicy.Preferred)
            label.setMinimumWidth(40)

        text_col = QVBoxLayout()
        text_col.setContentsMargins(0, 0, 0, 0)
        text_col.setSpacing(1)
        text_col.addLayout(name_row)
        text_col.addWidget(self.path_label)
        text_col.addWidget(self.title_label)
        text_col.addWidget(self.branch_label)

        # --- right column: status, or the approval controls ----------------
        self.status_label = plain_label()
        self.status_label.setAlignment(Qt.AlignRight)
        self.activity_label = plain_label(small=True, subtitle=True)
        self.activity_label.setAlignment(Qt.AlignRight)
        self.status_box = QWidget()
        sb = QVBoxLayout(self.status_box)
        sb.setContentsMargins(0, 0, 0, 0)
        sb.setSpacing(1)
        sb.addWidget(self.status_label)
        sb.addWidget(self.activity_label)

        self.approve_btn = QPushButton("Approve", objectName="approve")
        self.deny_btn = QPushButton("Deny", objectName="deny")
        for b in (self.approve_btn, self.deny_btn):
            b.setCursor(Qt.PointingHandCursor)
            b.setFont(small_font())
        self.approve_btn.clicked.connect(lambda: self._verdict(True))
        self.deny_btn.clicked.connect(lambda: self._verdict(False))
        self.more_btn = QToolButton(autoRaise=True, text="⋯", objectName="more")
        self.more_btn.setToolTip("More approval options")
        self.more_btn.setPopupMode(QToolButton.InstantPopup)
        self.more_menu = QMenu(self.more_btn)
        self.act_5m = QAction("Approve all for 5 minutes", self.more_menu,
                              triggered=lambda: self._rule("5m"))
        self.act_session = QAction("Approve all for this session", self.more_menu,
                                   triggered=lambda: self._rule("session"))
        self.act_off = QAction("Stop approving", self.more_menu,
                               triggered=lambda: self._rule("off"))
        for a in (self.act_5m, self.act_session, self.act_off):
            self.more_menu.addAction(a)
        self.more_btn.setMenu(self.more_menu)
        self.pending_label = plain_label(small=True, subtitle=True)
        self.pending_label.setAlignment(Qt.AlignRight)
        self.pending_label.setMaximumWidth(200)
        self.approval_box = QWidget()
        ab = QVBoxLayout(self.approval_box)
        ab.setContentsMargins(0, 0, 0, 0)
        ab.setSpacing(3)
        buttons = QHBoxLayout()
        buttons.setSpacing(4)
        buttons.addStretch(1)
        buttons.addWidget(self.approve_btn)
        buttons.addWidget(self.deny_btn)
        buttons.addWidget(self.more_btn)
        ab.addLayout(buttons)
        ab.addWidget(self.pending_label)

        right = QVBoxLayout()
        right.setContentsMargins(0, 0, 0, 0)
        right.addWidget(self.status_box)
        right.addWidget(self.approval_box)
        right.addStretch(1)

        layout = QHBoxLayout(self)
        layout.setContentsMargins(6, 5, 6, 5)
        layout.setSpacing(8)
        layout.addLayout(dot_col)
        layout.addLayout(text_col, 1)
        layout.addLayout(right)

        self.update_session(session)

    # -- data ---------------------------------------------------------------

    def session(self) -> Session:
        return self._s

    def update_session(self, s: Session) -> None:
        self._s = s
        status = s.effective_status
        self.dot.set_color(state_color(self.palette(), status))
        self.name_label.setText(s.project_name)
        self.name_label.setToolTip(plain_tooltip(s.name))
        self.bolt.setVisible(s.auto_approve is not None)
        self.path_label.setText(abbreviate_path(s.cwd, self._home))
        self.path_label.setToolTip(plain_tooltip(s.cwd))
        self.title_label.setText(s.title or "")
        self.title_label.setVisible(bool(s.title))
        self.branch_label.setText(f"⎇ {s.branch}" if s.branch else "")
        self.branch_label.setVisible(bool(s.branch))

        pending = s.approvals[0] if s.approvals else None
        self.approval_box.setVisible(pending is not None)
        self.status_box.setVisible(pending is None)
        if pending is not None:
            self.pending_label.setText(pending_caption(pending.summary or pending.tool,
                                                       len(s.approvals)))
            self.pending_label.setToolTip(plain_tooltip(pending.detail or pending.summary))
            can_rule = s.session_id is not None
            self.act_5m.setEnabled(can_rule)
            self.act_session.setEnabled(can_rule)
            self.act_off.setEnabled(can_rule and s.auto_approve is not None)
        else:
            self.status_label.setText(state_text(s.status, s.waiting_for))
            self.status_label.setStyleSheet(
                f"color: {argb(state_color(self.palette(), s.status))};")
        self.refresh_clock()
        self.setToolTip(self._hint())

    def refresh_clock(self) -> None:
        self.activity_label.setText(activity_text(self._s.since, self._s.started_at, self._now()))

    def _hint(self) -> str:
        if self._s.hub_id:
            return "Open a terminal attached to this session"
        if self._s.background:
            return "Open a terminal attached to this background session"
        return "Terminal only: this session can only be used in the terminal it runs in"

    def matches(self, text: str) -> bool:
        return self._s.matches(text)

    # -- actions --------------------------------------------------------------

    def _verdict(self, allow: bool) -> None:
        if self._s.approvals:
            self.approve_clicked.emit(self._s.approvals[0].id, allow)

    def _rule(self, rule: str) -> None:
        if self._s.session_id:
            self.auto_approve_requested.emit(self._s.session_id, rule)

    def build_context_menu(self) -> QMenu:
        menu = QMenu(self)
        end = QAction("End session", menu, triggered=lambda: self.end_requested.emit(self.key))
        end.setEnabled(self._s.hub_id is not None)
        if self._s.hub_id is None:
            end.setToolTip("Only sessions on the hub can be ended from here")
        menu.addAction(end)
        if self._s.auto_approve is not None and self._s.session_id:
            menu.addSeparator()
            menu.addAction(QAction("Stop auto-approving", menu,
                                   triggered=lambda: self._rule("off")))
        return menu

    def mouseReleaseEvent(self, event):
        if event.button() == Qt.LeftButton and self.rect().contains(event.position().toPoint()):
            self.clicked.emit(self.key)
        super().mouseReleaseEvent(event)

    def contextMenuEvent(self, event):
        self.build_context_menu().exec(event.globalPos())

    # -- hover --------------------------------------------------------------

    def _set_hover(self, value):
        self._hover = float(value)
        self.update()

    def event(self, e):
        if e.type() in (QEvent.HoverEnter, QEvent.HoverLeave):
            self._hover_anim.stop()
            self._hover_anim.setStartValue(self._hover)
            self._hover_anim.setEndValue(1.0 if e.type() == QEvent.HoverEnter else 0.0)
            self._hover_anim.start()
            if self._s.hub_id or self._s.background:
                self.setCursor(Qt.PointingHandCursor)
        elif e.type() in (QEvent.PaletteChange, QEvent.ApplicationPaletteChange):
            self.update_session(self._s)
        return super().event(e)

    def paintEvent(self, event):
        if self._hover > 0 and (self._s.hub_id or self._s.background):
            p = QPainter(self)
            p.setRenderHint(QPainter.Antialiasing)
            c = self.palette().color(QPalette.Highlight)
            c.setAlphaF(0.30 * self._hover)
            p.setPen(Qt.NoPen)
            p.setBrush(c)
            p.drawRoundedRect(self.rect(), 5, 5)
            p.end()
        super().paintEvent(event)
