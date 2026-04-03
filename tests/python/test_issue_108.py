"""IBX Test #108: Multi-client session — cross-session order visibility.

Since IBX does not support clientId routing (single session per connect),
this test verifies cross-session order visibility using sequential sessions
(Session A places order, Session B sees it via req_all_open_orders).

Run: pytest tests/python/test_issue_108.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract, Order

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

SPY_CON_ID = 756733


def make_spy():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class SessionWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()
        self.connected = threading.Event()
        self.next_order_id = 0
        self.order_statuses = {}
        self.got_order_status = threading.Event()
        self.got_order_cancelled = threading.Event()
        self.open_orders = []
        self.got_open_order_end = threading.Event()
        self.exec_details_list = []
        self.got_exec_end = threading.Event()

    def connect_ack(self):
        pass

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        with self.lock:
            self.order_statuses.setdefault(order_id, []).append(status)
        self.got_order_status.set()
        if status == "Cancelled":
            self.got_order_cancelled.set()

    def open_order(self, order_id, contract, order, order_state):
        with self.lock:
            self.open_orders.append((
                order_id,
                contract.symbol if hasattr(contract, "symbol") else "",
                order.action if hasattr(order, "action") else "",
                order.order_type if hasattr(order, "order_type") else "",
                order.lmt_price if hasattr(order, "lmt_price") else 0.0,
            ))

    def open_order_end(self):
        self.got_open_order_end.set()

    def exec_details(self, req_id, contract, execution):
        with self.lock:
            self.exec_details_list.append((req_id, contract, execution))

    def exec_details_end(self, req_id):
        self.got_exec_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202, 10147):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")

    def reset(self):
        self.connected.clear()
        with self.lock:
            self.open_orders.clear()
        self.got_open_order_end.clear()
        self.got_order_status.clear()
        self.got_order_cancelled.clear()
        self.got_exec_end.clear()
        with self.lock:
            self.exec_details_list.clear()


def connect_session(wrapper):
    client = EClient(wrapper)
    client.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )
    t = threading.Thread(target=client.run, daemon=True)
    t.start()
    assert wrapper.connected.wait(timeout=15), "Connection failed"
    return client, t


class TestMultiClientVisibility:
    """Issue #108: Cross-session order visibility and execution sharing."""

    def test_cross_session_order_and_exec_visibility(self):
        """Session A places order, Session B sees it and checks executions."""
        wrapper = SessionWrapper()

        # ── Session A: place a GTC order ──
        client_a, thread_a = connect_session(wrapper)
        oid = wrapper.next_order_id

        order = Order()
        order.action = "BUY"
        order.total_quantity = 50
        order.order_type = "LMT"
        order.lmt_price = 1.00  # Far below market
        order.tif = "GTC"
        order.outside_rth = True

        client_a.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=15), "Order not acked"
        print(f"  Session A: placed order oid={oid}")

        # reqOpenOrders should show own order
        wrapper.got_open_order_end.clear()
        client_a.req_open_orders()
        wrapper.got_open_order_end.wait(timeout=10)
        own_orders = [o for o in wrapper.open_orders
                      if o[1] == "SPY" and o[2] == "BUY" and abs(o[4] - 1.0) < 0.01]
        print(f"  Session A reqOpenOrders: {len(own_orders)} matching orders")

        # Disconnect session A
        client_a.disconnect()
        thread_a.join(timeout=5)
        time.sleep(2)

        # ── Session B: verify order visible ──
        wrapper.reset()
        client_b, thread_b = connect_session(wrapper)
        time.sleep(3)

        # reqAllOpenOrders should show Session A's order
        client_b.req_all_open_orders()
        wrapper.got_open_order_end.wait(timeout=10)

        matching = [o for o in wrapper.open_orders
                    if o[1] == "SPY" and o[2] == "BUY" and abs(o[4] - 1.0) < 0.01]
        print(f"  Session B reqAllOpenOrders: {len(matching)} matching, "
              f"total: {len(wrapper.open_orders)}")
        assert len(matching) > 0, \
            f"Session A's order not visible in Session B. Orders: {wrapper.open_orders}"

        # reqExecutions — should see shared executions
        client_b.req_executions(6001, None)
        wrapper.got_exec_end.wait(timeout=10)
        print(f"  Session B executions: {len(wrapper.exec_details_list)}")

        # Cleanup: cancel the order
        cancel_oids = {oid}
        for o in matching:
            cancel_oids.add(o[0])
        for cid in cancel_oids:
            try:
                wrapper.got_order_cancelled.clear()
                client_b.cancel_order(cid, "")
                wrapper.got_order_cancelled.wait(timeout=10)
            except Exception:
                pass

        time.sleep(2)
        client_b.disconnect()
        thread_b.join(timeout=5)
