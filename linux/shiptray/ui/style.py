"""Native-Plasma theming.

Everything derives from the active QPalette so Breeze light/dark come along
automatically. QSS `palette(role)` can't carry alpha, so translucent colors
are injected as hex-ARGB and the stylesheet is rebuilt on palette change.
Numbers follow Kirigami units: smallSpacing 4, largeSpacing 8, corner radius
5, animations 50/100 ms.
"""
from __future__ import annotations

from pathlib import Path

from PySide6.QtCore import Qt
from PySide6.QtGui import QColor, QFont, QFontDatabase, QPainter, QPalette, QPixmap

POSITIVE = QColor("#27ae60")   # Breeze positive: busy, Approve
NEUTRAL = QColor("#f67400")    # Breeze neutral: waiting on the user
NEGATIVE = QColor("#da4453")   # Breeze negative: Deny, errors
BOLT = QColor("#f6c400")       # standing auto-approve rule

ASSETS = Path(__file__).resolve().parent.parent / "assets"


def argb(color: QColor, alpha: float | None = None) -> str:
    c = QColor(color)
    if alpha is not None:
        c.setAlphaF(alpha)
    return c.name(QColor.NameFormat.HexArgb)


def subtitle_color(palette: QPalette) -> QColor:
    c = QColor(palette.color(QPalette.WindowText))
    c.setAlphaF(0.75)
    return c


def state_color(palette: QPalette, status: str) -> QColor:
    """Mac overlay colours: busy green, shell blue, waiting orange, idle grey."""
    if status == "busy":
        return QColor(POSITIVE)
    if status == "waiting":
        return QColor(NEUTRAL)
    if status == "shell":
        return QColor(palette.color(QPalette.Highlight))
    c = QColor(palette.color(QPalette.WindowText))
    c.setAlphaF(0.55)
    return c


def glyph_color(palette: QPalette, glyph: str) -> QColor:
    if glyph == "busy":
        return QColor(POSITIVE)
    if glyph == "waiting":
        return QColor(NEUTRAL)
    return QColor(palette.color(QPalette.WindowText))


_tray_cache: dict[tuple, QPixmap] = {}


def tray_pixmap(glyph: str, color: QColor, size: int = 22, dpr: float = 1.0) -> QPixmap:
    """Tray glyph, tinted to `color`.

    The `tray-*.svg` files are monochrome alpha masks (the Mac draws the same
    dot and ring with NSBezierPath). Qt has no template images, so render the
    SVG and composite the colour through its alpha (SourceIn); without that a
    black ring would vanish on a dark Plasma panel.

    Returns an empty pixmap if QtSvg is unavailable, so the caller keeps the
    raster icon.
    """
    key = (glyph, color.rgba(), size, round(dpr * 4))
    if key in _tray_cache:
        return _tray_cache[key]
    px = QPixmap(int(size * dpr), int(size * dpr))
    px.setDevicePixelRatio(dpr)
    px.fill(Qt.transparent)
    try:
        from PySide6.QtSvg import QSvgRenderer
    except ImportError:  # QtSvg not in this PySide6 build
        return QPixmap()
    renderer = QSvgRenderer(str(ASSETS / f"tray-{glyph}.svg"))
    if not renderer.isValid():
        return QPixmap()
    p = QPainter(px)
    p.setRenderHint(QPainter.Antialiasing)
    renderer.render(p)
    p.setCompositionMode(QPainter.CompositionMode_SourceIn)
    p.fillRect(px.rect(), color)
    p.end()
    _tray_cache[key] = px
    return px


def small_font() -> QFont:
    return QFontDatabase.systemFont(QFontDatabase.SmallestReadableFont)


def build_stylesheet(palette: QPalette) -> str:
    mid = argb(palette.color(QPalette.Mid))
    dark = argb(palette.color(QPalette.Dark))
    window = argb(palette.color(QPalette.Window))
    highlight = argb(palette.color(QPalette.Highlight))
    sub = argb(subtitle_color(palette))
    return f"""
#popupRoot {{
    background: {window};
    border: 1px solid {dark};
    border-radius: 5px;
}}
#popupHeading, #popupFooter {{ background: transparent; }}
QLabel#popupTitle {{ font-size: {int(palette_font_px() * 1.35)}px; }}
QLabel.subtitle, QLabel#footerStats {{ color: {sub}; }}
QFrame#hline {{ border: none; border-top: 1px solid {mid}; }}
#listScroll, #listScroll > QWidget > QWidget {{ background: transparent; border: none; }}
QScrollBar:vertical {{ background: transparent; width: 8px; margin: 0; }}
QScrollBar::handle:vertical {{ background: {mid}; border-radius: 4px; min-height: 24px; }}
QScrollBar::handle:vertical:hover {{ background: {highlight}; }}
QScrollBar::add-line:vertical, QScrollBar::sub-line:vertical {{ height: 0; }}
QScrollBar::add-page:vertical, QScrollBar::sub-page:vertical {{ background: transparent; }}
QLabel#bolt {{ color: {argb(BOLT)}; }}
QToolButton#more::menu-indicator {{ image: none; width: 0; }}
QPushButton#approve, QPushButton#deny {{
    border-radius: 9px;
    padding: 2px 9px;
    font-weight: bold;
}}
QPushButton#approve {{
    color: {argb(POSITIVE)};
    background: {argb(POSITIVE, 0.15)};
    border: 1px solid {argb(POSITIVE, 0.35)};
}}
QPushButton#approve:pressed {{ background: {argb(POSITIVE, 0.35)}; }}
QPushButton#deny {{
    color: {argb(NEGATIVE)};
    background: {argb(NEGATIVE, 0.15)};
    border: 1px solid {argb(NEGATIVE, 0.35)};
}}
QPushButton#deny:pressed {{ background: {argb(NEGATIVE, 0.35)}; }}
#banner {{
    background: {argb(NEUTRAL, 0.12)};
    border: 1px solid {argb(NEUTRAL, 0.45)};
    border-radius: 4px;
}}
"""


def palette_font_px() -> int:
    from PySide6.QtWidgets import QApplication
    f = QApplication.font()
    if f.pixelSize() > 0:
        return f.pixelSize()
    from PySide6.QtGui import QFontMetrics
    return QFontMetrics(f).height() - 3  # approx pt→px body size
