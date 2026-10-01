"""Issue #251: open-order requests made while the auth link is lost get no
answer until the order replay of the new logon has ended, as in the
reference. Then each request kind is answered once, after the replayed
statuses. No server needed.
"""
from ibx import EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def open_order(self, order_id, contract, order, order_state):
        self.events.append(("open_order", order_id, order_state.status))

    def order_status(self, order_id, status, filled, remaining, avg_fill_price,
                     perm_id, parent_id, last_fill_price, client_id, why_held,
                     mkt_cap_price):
        self.events.append(("order_status", order_id, status))

    def open_order_end(self):
        self.events.append(("open_order_end",))


def client():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_track_order(order_id=42, instrument=0, symbol="SPY",
                        action="BUY", total_quantity=1.0, lmt_price=400.0)
    return w, c


def test_requests_wait_for_the_order_replay():
    w, c = client()
    c._test_set_open_orders_held(True)
    c.req_open_orders()
    c.req_open_orders()
    c.req_all_open_orders()
    c._test_dispatch_once()
    assert w.events == []

    c._test_push_order_update(42, 0, "Submitted", 0, 1)
    c._test_set_open_orders_held(False)
    c._test_dispatch_once()
    assert w.events == [
        ("open_order", 42, "Submitted"), ("order_status", 42, "Submitted"),
        ("open_order", 42, "Submitted"), ("order_status", 42, "Submitted"), ("open_order_end",),
        ("open_order", 42, "Submitted"), ("order_status", 42, "Submitted"), ("open_order_end",),
    ]

    w.events.clear()
    c._test_dispatch_once()
    assert w.events == [], "answered once"


def test_requests_are_answered_at_once_with_the_link_up():
    w, c = client()
    c.req_open_orders()
    assert w.events[-1] == ("open_order_end",)
