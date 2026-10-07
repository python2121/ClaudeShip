"""The tray popup, styled after Plasma 6 applets (plasma-nm anatomy):
header strip (title row + toolbar with search and the quick "+"), the
grouped session list, footer with a summary and the web app button. All
colours from QPalette; every text through Qt.PlainText labels.
"""
from __future__ import annotations

import time

from PySide6.QtCore import QEvent, QRect, Qt, QTimer, Signal
from PySide6.QtGui import QCursor, QFontMetrics, QGuiApplication, QKeySequence, QShortcut
from PySide6.QtWidgets import (
    QApplication,
    QFrame,
    QHBoxLayout,
    QLineEdit,
    QScrollArea,
    QSizePolicy,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from ..core.model import WAITING, Session, group_sessions
from .session_row import SessionRow, plain_label, plain_tooltip
from .style import build_stylesheet


class SectionHeader(QWidget):
    """Small label + horizontal line — Plasma's ListSectionHeader."""

    def __init__(self, text: str, parent=None):
        super().__init__(parent)
        self.label = plain_label(text, small=True, subtitle=True)
        line = QFrame(objectName="hline")
        line.setFixedHeight(1)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(4, 6, 4, 0)
        layout.setSpacing(8)
        layout.addWidget(self.label)
        layout.addWidget(line, 1)


class Popup(QWidget):
    session_clicked = Signal(str)          # session key
    end_requested = Signal(str)            # session key
    approve_clicked = Signal(str, bool)    # approval id, allow
    auto_approve_requested = Signal(str, str)
    new_session_requested = Signal()
    open_web_requested = Signal()

    def __init__(self, now=time.time, home: str | None = None, parent=None):
        super().__init__(parent)
        # NOT Qt.Popup: popup windows are override-redirect, so KWin neither
        # manages nor revokes their focus — under XWayland that means no
        # deactivate event ever fires and outside clicks can't dismiss. A Tool
        # window is KWin-managed: it activates on show and deactivates (→ hide)
        # when anything else is clicked, X11 or Wayland alike.
        self.setWindowFlags(Qt.Tool | Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint)
        self.setAttribute(Qt.WA_TranslucentBackground)
        self._now = now
        self._home = home
        self._rows: dict[str, SessionRow] = {}
        self._section_headers: list[SectionHeader] = []
        self._row_sections: dict[str, SectionHeader] = {}
        self._layout_key: list = []

        root_frame = QFrame(objectName="popupRoot")
        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        outer.addWidget(root_frame)
        root = QVBoxLayout(root_frame)
        root.setContentsMargins(1, 1, 1, 1)
        root.setSpacing(0)

        # --- header: title row --------------------------------------------
        heading = QWidget(objectName="popupHeading")
        heading_layout = QVBoxLayout(heading)
        heading_layout.setContentsMargins(8, 6, 8, 6)
        heading_layout.setSpacing(4)
        self.title_label = plain_label("ClaudeShip")
        self.title_label.setObjectName("popupTitle")
        self.status_label = plain_label(small=True, subtitle=True)
        title_row = QHBoxLayout()
        title_row.setSpacing(6)
        title_row.addWidget(self.title_label)
        title_row.addStretch(1)
        title_row.addWidget(self.status_label)
        heading_layout.addLayout(title_row)

        # --- header: toolbar row ------------------------------------------
        self.search = QLineEdit(placeholderText="Search…", clearButtonEnabled=True)
        self.search.textChanged.connect(self._apply_filter)
        self.search.setSizePolicy(QSizePolicy.Expanding, QSizePolicy.Fixed)
        self.new_btn = QToolButton(autoRaise=True, text="＋")
        self.new_btn.setToolTip("New Claude session in your home folder (auto mode), "
                                "attached in a terminal")
        self.new_btn.clicked.connect(self.new_session_requested)
        toolbar = QHBoxLayout()
        toolbar.setSpacing(4)
        toolbar.addWidget(self.search)
        toolbar.addWidget(self.new_btn)
        heading_layout.addLayout(toolbar)
        root.addWidget(heading)

        top_line = QFrame(objectName="hline")
        top_line.setFixedHeight(1)
        root.addWidget(top_line)

        # --- banner: protocol mismatch / errors ---------------------------
        self.banner = QFrame(objectName="banner")
        banner_layout = QHBoxLayout(self.banner)
        banner_layout.setContentsMargins(8, 4, 8, 4)
        self.banner_label = plain_label(small=True)
        self.banner_label.setWordWrap(True)
        banner_layout.addWidget(self.banner_label, 1)
        banner_holder = QVBoxLayout()
        banner_holder.setContentsMargins(8, 4, 8, 0)
        banner_holder.addWidget(self.banner)
        root.addLayout(banner_holder)
        self.banner.hide()

        # --- session list --------------------------------------------------
        self.scroll = QScrollArea(objectName="listScroll")
        self.scroll.setWidgetResizable(True)
        self.scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarAlwaysOff)
        self.list_container = QWidget()
        self.list_layout = QVBoxLayout(self.list_container)
        self.list_layout.setContentsMargins(8, 4, 8, 8)
        self.list_layout.setSpacing(2)
        self.placeholder = plain_label("Connecting…", subtitle=True)
        self.placeholder.setAlignment(Qt.AlignCenter)
        self.placeholder.setWordWrap(True)
        self.list_layout.addWidget(self.placeholder)
        self.list_layout.addStretch(1)
        self.scroll.setWidget(self.list_container)
        root.addWidget(self.scroll, 1)

        # --- footer --------------------------------------------------------
        bottom_line = QFrame(objectName="hline")
        bottom_line.setFixedHeight(1)
        root.addWidget(bottom_line)
        footer = QWidget(objectName="popupFooter")
        footer_layout = QHBoxLayout(footer)
        footer_layout.setContentsMargins(8, 4, 8, 4)
        self.footer_stats = plain_label(small=True)
        self.footer_stats.setObjectName("footerStats")
        self.footer_stats.setSizePolicy(QSizePolicy.Ignored, QSizePolicy.Preferred)
        self.web_btn = QToolButton(autoRaise=True, text="🌐")
        self.web_btn.setToolTip("Open the ClaudeShip web app")
        self.web_btn.clicked.connect(self.open_web_requested)
        footer_layout.addWidget(self.footer_stats, 1)
        footer_layout.addWidget(self.web_btn)
        root.addWidget(footer)

        # Size in font units like Plasma (gridUnit = font height, popup =
        # gridUnit * 24 wide) so large fonts / scaling don't overflow the frame.
        self._grid = max(QFontMetrics(QApplication.font()).height(), 14)
        self.setFixedSize(self._grid * 24, self._grid * 31)
        self._apply_theme()
        QShortcut(QKeySequence.Find, self, activated=self.search.setFocus)

        # The "for 12s" captions tick once a second, but only while open.
        self._clock = QTimer(self, interval=1000, timeout=self._tick)

        # Focus loss is the primary outside-click signal…
        QGuiApplication.instance().focusWindowChanged.connect(self._on_focus_window_changed)
        # …and a watchdog covers the case where the popup never received
        # activation in the first place (focus-stealing prevention): if the
        # window is inactive, no menu of ours is open, and the pointer is not
        # over the popup for ~1.5s, dismiss it.
        self._dismiss_timer = QTimer(self, interval=300, timeout=self._check_dismiss)
        self._dismiss_strikes = 0

    def _on_focus_window_changed(self, window):
        if not self.isVisible():
            return
        handle = self.windowHandle()
        if window is not None and (window is handle or window.transientParent() is handle):
            return
        self.hide()

    def _check_dismiss(self):
        if not self.isVisible():
            self._dismiss_timer.stop()
            return
        keep = (self.isActiveWindow()
                or QApplication.activeModalWidget() is not None
                or QApplication.activePopupWidget() is not None
                or self.rect().contains(self.mapFromGlobal(QCursor.pos())))
        if keep:
            self._dismiss_strikes = 0
            return
        self._dismiss_strikes += 1
        if self._dismiss_strikes >= 5:
            self.hide()

    def showEvent(self, event):
        self._dismiss_strikes = 0
        self._dismiss_timer.start()
        self._clock.start()
        self._tick()
        super().showEvent(event)

    def hideEvent(self, event):
        self._dismiss_timer.stop()
        self._clock.stop()
        super().hideEvent(event)

    def _tick(self):
        for row in self._rows.values():
            row.refresh_clock()

    # --- theming -----------------------------------------------------------

    def _apply_theme(self):
        self.setStyleSheet(build_stylesheet(self.palette()))

    def changeEvent(self, e):
        if e.type() in (QEvent.PaletteChange, QEvent.ApplicationPaletteChange):
            self._apply_theme()
        super().changeEvent(e)

    # --- data --------------------------------------------------------------

    def set_banner(self, text: str) -> None:
        self.banner_label.setText(text)
        self.banner.setVisible(bool(text))

    def set_error(self, message: str) -> None:
        self.status_label.setText("● Disconnected")
        self.status_label.setToolTip(plain_tooltip(message))
        if not self._rows:
            self.placeholder.setText(message)
            self.placeholder.show()
        self.footer_stats.setText(message)
        self.footer_stats.setToolTip(plain_tooltip(message))

    def set_sessions(self, sessions, host: str = "") -> None:
        self.status_label.setText(f"● {host}" if host else "● Connected")
        self.status_label.setToolTip("Connected to the hub")
        self.footer_stats.setToolTip("")

        current = {s.key for s in sessions}
        for key in [k for k in self._rows if k not in current]:
            row = self._rows.pop(key)
            self.list_layout.removeWidget(row)
            row.deleteLater()
        for s in sessions:
            if s.key in self._rows:
                self._rows[s.key].update_session(s)
            else:
                row = SessionRow(s, now=self._now, home=self._home)
                row.clicked.connect(self.session_clicked)
                row.end_requested.connect(self.end_requested)
                row.approve_clicked.connect(self.approve_clicked)
                row.auto_approve_requested.connect(self.auto_approve_requested)
                self._rows[s.key] = row
        self._relayout()

        n = len(sessions)
        waiting = sum(1 for s in sessions if s.effective_status == WAITING)
        text = f"{n} session{'s' if n != 1 else ''}"
        if waiting:
            text += f" · {waiting} waiting"
        self.footer_stats.setText(text)
        if sessions:
            self.placeholder.hide()
        else:
            self.placeholder.setText("No Claude sessions running.\n"
                                     "＋ starts one in your home folder.")
            self.placeholder.show()

    def grouped(self) -> list[tuple[str, list[Session]]]:
        return group_sessions(row.session() for row in self._rows.values())

    def row(self, key: str) -> SessionRow | None:
        return self._rows.get(key)

    def _relayout(self):
        grouped = self.grouped()
        key = [(g, [s.key for s in rows]) for g, rows in grouped]
        if key == self._layout_key:
            self._update_counts(grouped)
            self._apply_filter()
            return
        self._layout_key = key
        while self.list_layout.count():
            item = self.list_layout.takeAt(0)
            widget = item.widget()
            if isinstance(widget, SectionHeader):
                widget.deleteLater()
            elif widget is not None:
                widget.setParent(None)
        self._section_headers = []
        self._row_sections = {}
        self.list_layout.addWidget(self.placeholder)
        for group, rows in grouped:
            header = SectionHeader(f"{group} · {len(rows)}")
            self._section_headers.append(header)
            self.list_layout.addWidget(header)
            for s in rows:
                row = self._rows[s.key]
                self.list_layout.addWidget(row)
                self._row_sections[s.key] = header
        self.list_layout.addStretch(1)
        self._apply_filter()

    def _update_counts(self, grouped):
        for header, (group, rows) in zip(self._section_headers, grouped):
            header.label.setText(f"{group} · {len(rows)}")

    def _apply_filter(self, *_):
        text = self.search.text()
        visible_by_header: dict = {}
        for key, row in self._rows.items():
            show = row.matches(text)
            row.setVisible(show)
            header = self._row_sections.get(key)
            if header is not None:
                visible_by_header[header] = visible_by_header.get(header, False) or show
        for header in self._section_headers:
            header.setVisible(visible_by_header.get(header, False))

    # --- behaviour ---------------------------------------------------------

    def toggle_near(self, anchor: QRect | None) -> None:
        """Show anchored to the tray icon (when its geometry is known) or to
        the panel corner — the edge the panel occupies is derived from the gap
        between the screen's full and available geometry."""
        if self.isVisible():
            self.hide()
            return
        has_anchor = anchor is not None and anchor.isValid() and not anchor.isEmpty()
        screen = (QGuiApplication.screenAt(anchor.center()) if has_anchor else None) \
            or QGuiApplication.primaryScreen()
        full = screen.geometry()
        avail = screen.availableGeometry()

        # Gap between popup and panel. Plasma 6 floating panels float 8px off
        # the screen edge and plasmashell floats its popups a further ~8px off
        # the panel; a window closer than that also triggers the panel's
        # "window touching → dock" behavior. 8+8 keeps both properties.
        panel_gap = 16
        edge_margin = 8

        if self.height() > avail.height() - 2 * panel_gap:
            self.setFixedHeight(avail.height() - 2 * panel_gap)
        w, h = self.width(), self.height()

        if has_anchor:
            x = anchor.center().x() - w // 2
            above = anchor.center().y() > full.center().y()
            y = anchor.top() - h - edge_margin if above else anchor.bottom() + edge_margin
        elif avail.bottom() < full.bottom():      # panel at bottom (KDE default)
            x, y = avail.right() - w - edge_margin, avail.bottom() - h - panel_gap
        elif avail.top() > full.top():            # panel at top
            x, y = avail.right() - w - edge_margin, avail.top() + panel_gap
        elif avail.right() < full.right():        # panel at right
            x, y = avail.right() - w - panel_gap, avail.bottom() - h - edge_margin
        elif avail.left() > full.left():          # panel at left
            x, y = avail.left() + panel_gap, avail.bottom() - h - edge_margin
        else:
            x, y = avail.right() - w - edge_margin, avail.bottom() - h - panel_gap

        x = min(max(x, avail.left()), avail.right() - w)
        y = min(max(y, avail.top()), avail.bottom() - h)
        self.move(x, y)
        self.show()
        self.raise_()
        self.activateWindow()
        if self.windowHandle() is not None:
            self.windowHandle().requestActivate()  # focus-loss = our outside-click signal
        self.search.setFocus()

    def keyPressEvent(self, event):
        if event.key() == Qt.Key_Escape:
            # Escape clears the filter first, then closes.
            if self.search.text():
                self.search.clear()
            else:
                self.hide()
            return
        super().keyPressEvent(event)

    def event(self, e):
        if e.type() == QEvent.WindowDeactivate:
            self.hide()
        return super().event(e)
