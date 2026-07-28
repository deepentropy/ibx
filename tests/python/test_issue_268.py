"""Regression tests for ibx#268: shared locks held across Python callbacks.

The defect class is a Rust mutex held while user code runs. Because the
callback runs with the GIL held, re-entering a path that takes the same
mutex freezes the whole interpreter rather than failing one call, so every
test here would hang rather than fail if its fix regressed. Each is bounded
by a watchdog that runs in C, because a Python-level timer would itself need
the GIL the frozen thread is holding.
"""
import faulthandler
import pytest
from ibx import EClient, EWrapper


class _watchdog:
    """Abort with a traceback instead of hanging the suite if a lock regresses.

    A `threading.Timer` cannot do this job: its callback needs the GIL, which
    is exactly what the frozen main thread is holding, so the timer never runs
    and the suite wedges until something outside kills it. `faulthandler` arms
    a timer in C that does not need the interpreter.
    """

    def __init__(self, seconds=10):
        self.seconds = seconds

    def start(self):
        faulthandler.dump_traceback_later(self.seconds, exit=True)

    def cancel(self):
        faulthandler.cancel_dump_traceback_later()


def test_disconnect_from_the_1100_handler_does_not_deadlock():
    """A handler answering connectivity loss with disconnect() re-enters the
    event lock. Holding it across the callback froze the interpreter."""
    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = None
            self.saw = 0

        def error(self, req_id, code, msg, advanced=""):
            if code == 1100:
                self.saw += 1
                self.client.disconnect()

    w = W()
    c = EClient(w)
    w.client = c
    c._test_connect("T")
    c._test_push_disconnect_event()

    t = _watchdog()
    t.start()
    c._test_dispatch_once()
    t.cancel()
    assert w.saw == 1


def test_reconnect_from_the_1100_handler_leaves_the_new_session_connected():
    """The handler disconnects and reconnects inside the callback. Storing the
    disconnected flag after the callback clobbered the new session."""
    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = None

        def error(self, req_id, code, msg, advanced=""):
            if code == 1100:
                self.client.disconnect()
                self.client._test_connect("T2")

    w = W()
    c = EClient(w)
    w.client = c
    c._test_connect("T")
    c._test_push_disconnect_event()

    t = _watchdog()
    t.start()
    c._test_dispatch_once()
    t.cancel()
    assert c.is_connected(), "the reconnected session must not be marked down"


def test_two_disconnects_in_one_batch_fire_one_1100():
    """One network cut can emit several Disconnected events, and a batch is one
    session. Firing per event took a stale second 1100 into the new session."""
    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.client = None
            self.saw = 0

        def error(self, req_id, code, msg, advanced=""):
            if code == 1100:
                self.saw += 1
                if self.saw == 1:
                    self.client.disconnect()
                    self.client._test_connect("T2")

    w = W()
    c = EClient(w)
    w.client = c
    c._test_connect("T")
    c._test_push_disconnect_event()
    c._test_push_disconnect_event()

    t = _watchdog()
    t.start()
    c._test_dispatch_once()
    t.cancel()
    assert w.saw == 1, f"one loss is one 1100, saw {w.saw}"
    assert c.is_connected(), "the reconnected session must survive the second event"
