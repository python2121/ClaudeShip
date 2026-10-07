"""Run blocking RPC calls off the UI thread."""
from __future__ import annotations

from PySide6.QtCore import QObject, QRunnable, QThreadPool, Signal


class _Signals(QObject):
    done = Signal(object)
    error = Signal(str)


class _Worker(QRunnable):
    def __init__(self, fn):
        super().__init__()
        self.setAutoDelete(False)  # keep alive until queued signals are delivered
        self.fn = fn
        self.signals = _Signals()

    def run(self):
        try:
            result = self.fn()
        except Exception as e:  # noqa: BLE001 — surfaced to the UI as a message
            self._emit(self.signals.error, str(e))
        else:
            self._emit(self.signals.done, result)

    @staticmethod
    def _emit(signal, arg):
        try:
            signal.emit(arg)
        except RuntimeError:
            pass  # Qt objects already destroyed — app quit mid-request


_live: set[_Worker] = set()


def run_async(fn, on_done=None, on_error=None):
    """Run fn() in the thread pool; deliver result/error on the UI thread."""
    w = _Worker(fn)
    _live.add(w)

    def _finish(handler):
        def inner(arg):
            _live.discard(w)
            if handler:
                handler(arg)
        return inner

    w.signals.done.connect(_finish(on_done))
    w.signals.error.connect(_finish(on_error))
    QThreadPool.globalInstance().start(w)
