"""Issue #165: connect_ack must fire from connect(), not run().

Verifies the official Python ibapi behaviour where connect_ack signals
"socket ready" synchronously from connect(), so callers can gate run()
on connect_ack without deadlocking.
"""

import os
import threading
import time

import pytest
from ibx import EClient, EWrapper

pytestmark = pytest.mark.skipif(
    not os.environ.get("IBX_LIVE_TEST"),
    reason="Set IBX_LIVE_TEST=1 to run live gateway tests",
)


class OrderingWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.events: list[tuple] = []
        self.lock = threading.Lock()

    def connect_ack(self):
        with self.lock:
            self.events.append(("connect_ack", time.monotonic()))

    def managed_accounts(self, accounts_list):
        with self.lock:
            self.events.append(("managed_accounts", accounts_list, time.monotonic()))

    def next_valid_id(self, order_id):
        with self.lock:
            self.events.append(("next_valid_id", order_id, time.monotonic()))


def _credentials():
    return os.environ["IB_USERNAME"], os.environ["IB_PASSWORD"], os.environ.get(
        "IB_HOST", "cdc1.ibllc.com"
    )


def test_connect_ack_fires_before_connect_returns():
    user, pw, host = _credentials()
    w = OrderingWrapper()
    c = EClient(w)
    try:
        c.connect(username=user, password=pw, host=host, paper=True)
        # connect() must have fired all three preamble callbacks before returning,
        # without anyone ever calling run().
        names = [e[0] for e in w.events]
        assert names == ["connect_ack", "managed_accounts", "next_valid_id"], (
            f"unexpected order/contents (run() never called): {w.events}"
        )
        # next_valid_id payload sanity
        nvi = next(e for e in w.events if e[0] == "next_valid_id")
        assert nvi[1] > 0, f"next_valid_id should be positive, got {nvi[1]}"
        # managed_accounts payload sanity
        ma = next(e for e in w.events if e[0] == "managed_accounts")
        assert ma[1], "managed_accounts payload should be non-empty"
    finally:
        c.disconnect()


def test_caller_can_block_on_connect_ack_before_calling_run():
    """The point of the fix: a caller that gates run() on connect_ack must
    not deadlock. We simulate that by using a threading.Event set inside the
    wrapper callback, then asserting it is already set by the time connect()
    returns (i.e. before any run() is started)."""
    user, pw, host = _credentials()

    class GatedWrapper(EWrapper):
        def __init__(self):
            super().__init__()
            self.acked = threading.Event()

        def connect_ack(self):
            self.acked.set()

    w = GatedWrapper()
    c = EClient(w)
    try:
        c.connect(username=user, password=pw, host=host, paper=True)
        assert w.acked.is_set(), "connect_ack must fire before connect() returns"
        # Now starting run() should still work normally.
        t = threading.Thread(target=c.run, daemon=True)
        t.start()
        time.sleep(0.5)
        assert c.is_connected()
    finally:
        c.disconnect()
