"""ibx#488 (tests layer D): a wrapper that calls back into the client from
each callback (req_executions, req_open_orders, req_mkt_data, place_order,
cancel_order, and at last disconnect) must not find a lock held by its
caller (ibx#265, ibx#268, ibx#271).

A lock held across a callback freezes the whole interpreter, so each
session runs in its own Python process with a time limit: a frozen
session fails the test instead of hanging it.
"""
import subprocess
import sys
import threading

import pytest

from ibx import Contract, EClient, EWrapper, Order

LIMIT_S = 60

CALLBACKS = ["tick_price", "tick_size", "order_status", "open_order", "exec_details",
             "exec_details_end", "open_order_end", "error"]


def stock():
    c = Contract()
    c.con_id = 265598
    c.symbol = "AAPL"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def limit_order():
    o = Order()
    o.action = "BUY"
    o.total_quantity = 1
    o.order_type = "LMT"
    o.lmt_price = 100.0
    return o


class Reentrant(EWrapper):
    """Calls back into the client from the first call of each callback,
    one level down from there too, and disconnects from `disconnect_on`."""

    def __init__(self, disconnect_on):
        super().__init__()
        self.client = None
        self.disconnect_on = disconnect_on
        self.depth = 0
        self.next_id = 1000
        self.seen = []

    def back_in(self, name):
        first = name not in self.seen
        if first:
            self.seen.append(name)
        if first and self.depth < 2:
            self.depth += 1
            c = self.client
            self.next_id += 1
            n = self.next_id
            c._test_serve_commands_after(0)
            c.req_executions(n, None)
            c.req_open_orders()
            c.req_mkt_data(n, stock(), "", False, False)
            c.place_order(n, stock(), limit_order())
            c.cancel_order(n, "")
            c.cancel_mkt_data(n)
            self.depth -= 1
        if name == self.disconnect_on and self.client.is_connected():
            self.client.disconnect()

    def tick_price(self, req_id, tick_type, price, attrib):
        self.back_in("tick_price")

    def tick_size(self, req_id, tick_type, size):
        self.back_in("tick_size")

    def order_status(self, *args):
        self.back_in("order_status")

    def open_order(self, *args):
        self.back_in("open_order")

    def open_order_end(self):
        self.back_in("open_order_end")

    def exec_details(self, *args):
        self.back_in("exec_details")

    def exec_details_end(self, req_id):
        self.back_in("exec_details_end")

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.back_in("error")


def session(disconnect_on):
    """One session on the test engine: a quote, an order with its status
    and a fill, executions asked for, a lost link notice. The callbacks
    seen, one per line."""
    w = Reentrant(disconnect_on)
    c = EClient(w)
    w.client = c
    c._test_connect("DUXXXXXXX")
    c._test_map_instrument(1, 0)
    c._test_set_instrument_count(1)
    c._test_track_order(100, 0, "AAPL", "BUY", 1.0, 100.0)
    c._test_push_quote(0, bid=99.0, ask=101.0, bid_size=5)
    c._test_push_order_update(100, 0, "Submitted", 0, 1)
    c._test_push_fill(0, 100, "BUY", 100.0, 1, 0, 0.5)
    # Run the dispatch on another thread as an application does, with the
    # main thread taking part.
    done = threading.Event()

    def dispatch():
        if c.is_connected():
            c._test_dispatch_once()
        if c.is_connected():
            c.req_executions(2, None)
            c._test_push_connection_notice(1100, "Connectivity between client and server has been lost.")
            c._test_dispatch_once()
        done.set()

    t = threading.Thread(target=dispatch)
    t.start()
    t.join()
    assert done.is_set()
    if c.is_connected():
        c.disconnect()
    print("\n".join(w.seen))


def run_session(disconnect_on):
    p = subprocess.run([sys.executable, __file__, disconnect_on], capture_output=True, text=True, timeout=LIMIT_S)
    assert p.returncode == 0, f"disconnect from {disconnect_on}: exit {p.returncode}\n{p.stderr[-2000:]}"
    return p.stdout.split()


def test_callbacks_call_back_into_the_client():
    try:
        seen = run_session("none")
    except subprocess.TimeoutExpired:
        pytest.fail(f"no end within {LIMIT_S} s: a lock is held across a callback")
    for name in CALLBACKS:
        assert name in seen, f"{name} not called: {seen}"


@pytest.mark.parametrize("callback", CALLBACKS)
def test_disconnect_from_the_callback(callback):
    try:
        run_session(callback)
    except subprocess.TimeoutExpired:
        pytest.fail(f"disconnect from {callback}: no end within {LIMIT_S} s, a lock is held across a callback")


if __name__ == "__main__":
    session(sys.argv[1])
