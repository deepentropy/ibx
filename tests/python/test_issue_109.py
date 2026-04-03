"""IBX Test #109: Connection stress — rapid reconnect cycles with order survival.

Places a GTC order, disconnects, performs 3 rapid reconnect/disconnect cycles
(~1s each), then verifies order survived and market data works from the
recovered session.

Run: pytest tests/python/test_issue_109.py -v --timeout=120
"""

import os, threading, time
import pytest
from ibx import EClient, EWrapper, Contract, Order

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


class StressWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()
        self.connected_ev = threading.Event()
        self.open_orders = []
        self.got_open_order_end = threading.Event()
        self.order_statuses = {}
        self.got_order_status = threading.Event()
        self.got_order_cancelled = threading.Event()
        self.next_order_id = 0

        # Market data
        self.bid = 0.0
        self.ask = 0.0
        self.got_tick = threading.Event()

        # Account summary
        self.summary = {}
        self.got_summary_end = threading.Event()

    def connect_ack(self):
        pass

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.connected_ev.set()

    def managed_accounts(self, accounts_list):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0:
            if tick_type == 1:
                self.bid = price
            elif tick_type == 2:
                self.ask = price
            if tick_type in (1, 2):
                self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        with self.lock:
            self.order_statuses.setdefault(order_id, []).append(
                (status, filled, remaining, perm_id)
            )
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

    def account_summary(self, req_id, account, tag, value, currency):
        self.summary[tag] = (value, currency)

    def account_summary_end(self, req_id):
        self.got_summary_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] oid={req_id} code={error_code}: {error_string}")

    def reset(self):
        """Reset mutable state for a new session."""
        self.connected_ev.clear()
        with self.lock:
            self.open_orders.clear()
        self.got_open_order_end.clear()
        self.got_order_status.clear()
        self.got_order_cancelled.clear()
        self.bid = 0.0
        self.ask = 0.0
        self.got_tick.clear()
        self.summary.clear()
        self.got_summary_end.clear()


def connect(wrapper):
    """Create a new EClient, connect, start run thread. Returns (client, thread)."""
    client = EClient(wrapper)
    client.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )
    t = threading.Thread(target=client.run, daemon=True)
    t.start()
    assert wrapper.connected_ev.wait(timeout=15), "Connection failed"
    return client, t


def disconnect(client, thread):
    client.disconnect()
    thread.join(timeout=5)


class TestConnectionStress:
    """Issue #109: Rapid reconnect cycles, order survival, session recovery."""

    def test_rapid_reconnect_order_survival(self):
        wrapper = StressWrapper()

        # ── Phase 1: Connect and place a GTC order ──
        client, thread = connect(wrapper)
        oid = wrapper.next_order_id

        order = Order()
        order.action = "BUY"
        order.total_quantity = 10
        order.order_type = "LMT"
        order.lmt_price = 1.00  # far below market — won't fill
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=15), "Order not acknowledged"
        statuses = wrapper.order_statuses.get(oid, [])
        assert len(statuses) > 0, f"No status for order {oid}"
        perm_id = statuses[-1][3]
        print(f"Phase 1: order placed oid={oid} permId={perm_id}")

        # ── Phase 2: Disconnect, then 3 rapid reconnect/disconnect cycles ──
        disconnect(client, thread)
        time.sleep(1)

        for i in range(3):
            wrapper.reset()
            client, thread = connect(wrapper)
            time.sleep(0.5)   # ~500ms connected
            disconnect(client, thread)
            time.sleep(0.5)   # ~500ms disconnected
            print(f"  Cycle {i+1}/3 complete")

        # ── Phase 3: Reconnect and verify order survived ──
        wrapper.reset()
        client, thread = connect(wrapper)

        # Wait for init burst
        time.sleep(3)

        # Check open orders
        client.req_all_open_orders()
        assert wrapper.got_open_order_end.wait(timeout=15), "open_order_end not received"

        matching = [
            o for o in wrapper.open_orders
            if o[1] == "SPY" and o[2] == "BUY" and o[3] == "LMT" and abs(o[4] - 1.0) < 0.01
        ]
        found = len(matching) > 0
        print(f"Phase 3: reqOpenOrders found={found}, matches={len(matching)}")
        assert found, (
            f"GTC order (oid={oid}, permId={perm_id}) did NOT survive reconnect cycles. "
            f"Open orders: {wrapper.open_orders}"
        )

        # ── Phase 4: Verify market data works (wait for both bid and ask) ──
        client.req_mkt_data(5000, make_spy(), "", False)
        got_tick = wrapper.got_tick.wait(timeout=30)

        if got_tick:
            # Wait up to 10s for ask to arrive (may come in a separate message)
            for _ in range(100):
                if wrapper.bid > 0 and wrapper.ask > 0:
                    break
                time.sleep(0.1)
            client.cancel_mkt_data(5000)
            print(f"Phase 4: market data ok — bid={wrapper.bid}, ask={wrapper.ask}")
            assert wrapper.bid > 0, "No bid price"
            assert wrapper.ask > 0, "No ask price — ask tick lost"
        else:
            client.cancel_mkt_data(5000)
            print("Phase 4: no tick data (market may be closed) — skipping mkt data check")

        # ── Phase 5: Account summary ──
        tags = "NetLiquidation,TotalCashValue,BuyingPower"
        client.req_account_summary(9100, "All", tags)
        got_summary = wrapper.got_summary_end.wait(timeout=15)
        client.cancel_account_summary(9100)
        print(f"Phase 5: account summary received={got_summary}, tags={list(wrapper.summary.keys())}")

        # ── Cleanup: Cancel the order ──
        cancel_oids = {oid}
        for o in matching:
            cancel_oids.add(o[0])

        for cancel_oid in cancel_oids:
            try:
                wrapper.got_order_cancelled.clear()
                client.cancel_order(cancel_oid, "")
                wrapper.got_order_cancelled.wait(timeout=10)
            except Exception:
                pass

        time.sleep(2)
        disconnect(client, thread)
        print("Cleanup: order cancelled, disconnected")
