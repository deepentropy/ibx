"""
Test for issue #117: Cross-session order visibility via req_all_open_orders().

Verifies that orders placed by one CCP session are visible to a subsequent
session via req_all_open_orders() — proving the CCP server delivers
account-level order data, not session-scoped.

Test flow:
1. Session A connects, places a GTC LMT order far from market
2. Session A disconnects (order remains live on the server)
3. Session B connects (new CCP session)
4. Session B calls req_all_open_orders()
5. Verify session A's order appears in open_order() callbacks
6. Session B cancels the order and disconnects

Requires IB_USERNAME and IB_PASSWORD.
Run with: pytest tests/python/test_issue_117.py -v
"""
import os, threading, time
import pytest
from ibx import EClient, EWrapper, Contract, Order

SPY_CON_ID = 756733


def make_spy():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class Wrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()
        self.connected_ev = threading.Event()
        self.got_next_id = threading.Event()
        self.got_open_order = threading.Event()
        self.got_open_order_end = threading.Event()
        self.got_order_status = threading.Event()
        self.got_order_cancelled = threading.Event()
        self.next_order_id = 0
        self.open_orders = []  # list of (order_id, symbol, action, order_type, lmt_price, status)
        self.order_statuses = {}  # oid -> [statuses]
        self.errors = []

    def connect_ack(self):
        self.connected_ev.set()

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        pass

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        with self.lock:
            self.errors.append((req_id, error_code, error_string))

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
            status = order_state.get("status", "") if isinstance(order_state, dict) else getattr(order_state, "status", "")
            self.open_orders.append((
                order_id,
                contract.symbol if hasattr(contract, "symbol") else "",
                order.action if hasattr(order, "action") else "",
                order.order_type if hasattr(order, "order_type") else "",
                order.lmt_price if hasattr(order, "lmt_price") else 0.0,
                status,
            ))
        self.got_open_order.set()

    def open_order_end(self):
        self.got_open_order_end.set()


def connect_session():
    """Create and connect a new IBX session. Returns (wrapper, client, run_thread)."""
    wrapper = Wrapper()
    client = EClient(wrapper)
    client.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )
    run_thread = threading.Thread(target=client.run, daemon=True)
    run_thread.start()
    assert wrapper.got_next_id.wait(timeout=15), "Connection failed"
    return wrapper, client, run_thread


@pytest.fixture(scope="module")
def skip_if_no_creds():
    if not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")):
        pytest.skip("IB_USERNAME and IB_PASSWORD not set")


class TestCrossSessionOrderVisibility:
    """Orders placed by session A must be visible to session B via req_all_open_orders()."""

    def test_cross_session_order_visible(self, skip_if_no_creds):
        # ── Session A: place a GTC order that will survive disconnect ──
        wrapper_a, client_a, thread_a = connect_session()

        oid = wrapper_a.next_order_id
        wrapper_a.next_order_id += 1

        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00  # far below market — won't fill
        order.tif = "GTC"
        order.outside_rth = True

        client_a.place_order(oid, make_spy(), order)
        time.sleep(5)  # wait for server ack

        # Verify session A got the order acknowledged
        statuses_a = wrapper_a.order_statuses.get(oid, [])
        assert len(statuses_a) > 0, f"Session A: order {oid} not acknowledged"
        print(f"Session A: order {oid} placed, statuses={statuses_a}")

        # Disconnect session A — order stays live on server
        client_a.disconnect()
        thread_a.join(timeout=5)
        time.sleep(2)

        # ── Session B: connect fresh, check if session A's order is visible ──
        wrapper_b, client_b, thread_b = connect_session()

        # Wait for init burst (35=8 exec reports from 6040=72 and 35=H)
        time.sleep(5)

        # Call req_all_open_orders — should include session A's order
        wrapper_b.got_open_order_end.clear()
        wrapper_b.got_open_order.clear()
        client_b.req_all_open_orders()

        wrapper_b.got_open_order_end.wait(timeout=10)

        print(f"Session B: open_orders = {wrapper_b.open_orders}")

        # Check if any open order matches session A's order
        # We match on: SPY, BUY, LMT, lmt_price=1.0
        matching = [
            o for o in wrapper_b.open_orders
            if o[1] == "SPY" and o[2] == "BUY" and o[3] == "LMT" and abs(o[4] - 1.0) < 0.01
        ]

        # Clean up: cancel the order from session B using global cancel
        # We need the order_id. If we found it in open_orders, use that.
        cancel_oids = set()
        for o in matching:
            cancel_oids.add(o[0])
        # Also try the original oid in case the server preserved it
        cancel_oids.add(oid)

        for cancel_oid in cancel_oids:
            try:
                wrapper_b.got_order_cancelled.clear()
                client_b.cancel_order(cancel_oid, "")
                wrapper_b.got_order_cancelled.wait(timeout=10)
            except Exception:
                pass

        time.sleep(2)
        client_b.disconnect()
        thread_b.join(timeout=5)

        # Assert cross-session visibility
        assert len(matching) > 0, (
            f"Cross-session order NOT visible. Session A placed SPY BUY LMT @1.00 (oid={oid}), "
            f"but session B's req_all_open_orders() returned: {wrapper_b.open_orders}"
        )
        print(f"PASS: Session B saw {len(matching)} matching cross-session order(s)")
