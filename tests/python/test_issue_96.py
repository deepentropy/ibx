"""IBX Test #96: Order Lifecycle — Place, Modify, Cancel, Fill (SPY).

Full order state machine: place → modify → cancel, plus fill handling.
Exercises permId routing, order status transitions, and execution queries.

Run: pytest tests/python/test_issue_96.py -v --timeout=120
"""

import os
import time
import pytest
import threading
from ibx import EWrapper, EClient, Contract, Order


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

SPY_CON_ID = 756733


class OrderLifecycleWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0

        # Order tracking
        self.order_statuses = []  # (order_id, status, filled, remaining, perm_id)
        self.open_orders = []     # (order_id, contract, order)
        self.got_open_order_end = threading.Event()
        self.executions = []      # (req_id, contract, execution)
        self.got_exec_end = threading.Event()
        self.perm_ids = {}        # order_id -> perm_id

        # Events for synchronization
        self.got_status = threading.Event()
        self.got_fill = threading.Event()
        self.got_cancelled = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self.order_statuses.append((order_id, status, filled, remaining, perm_id))
        self.perm_ids[order_id] = perm_id
        self.got_status.set()
        if status == "Filled" or filled > 0:
            self.got_fill.set()
        if status == "Cancelled":
            self.got_cancelled.set()

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders.append((order_id, contract, order))

    def open_order_end(self):
        self.got_open_order_end.set()

    def exec_details(self, req_id, contract, execution):
        self.executions.append((req_id, contract, execution))

    def exec_details_end(self, req_id):
        self.got_exec_end.set()

    def commission_report(self, commission_report):
        pass

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        # 2104/2106/2158 = data farm connection messages (noise)
        # 202 = order cancelled (expected)
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] {error_code}: {error_string}")


def make_spy():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class TestOrderLifecycle:
    """Issue #96: Full order lifecycle on SPY."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = OrderLifecycleWrapper()
        self.client = EClient(self.wrapper)
        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )
        self.thread = threading.Thread(target=self.client.run, daemon=True)
        self.thread.start()
        assert self.wrapper.connected.wait(timeout=15), "Connection failed"
        yield
        self.client.disconnect()
        self.thread.join(timeout=5)

    def test_place_and_cancel(self):
        """Place a LMT order far from market, verify status, then cancel."""
        spy = make_spy()
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00  # far below market — won't fill
        order.tif = "GTC"
        order.outside_rth = True

        oid = self.wrapper.next_id
        self.client.place_order(oid, spy, order)

        # Wait for order acknowledgement
        assert self.wrapper.got_status.wait(timeout=30), "No order status received"
        statuses = [s for s in self.wrapper.order_statuses if s[0] == oid]
        assert len(statuses) > 0, "Should have status for our order"
        # permId should be assigned
        assert self.wrapper.perm_ids.get(oid, 0) > 0, "permId should be positive"
        perm_id_on_place = self.wrapper.perm_ids[oid]

        # Verify via reqOpenOrders
        self.client.req_open_orders()
        assert self.wrapper.got_open_order_end.wait(timeout=15), "open_order_end not received"
        our_open = [o for o in self.wrapper.open_orders if o[0] == oid]
        assert len(our_open) > 0, "Our order should appear in open orders"

        # Cancel
        self.client.cancel_order(oid, "")
        assert self.wrapper.got_cancelled.wait(timeout=15), "Cancel not confirmed"
        cancel_statuses = [s for s in self.wrapper.order_statuses
                           if s[0] == oid and s[1] == "Cancelled"]
        assert len(cancel_statuses) > 0

    def test_place_modify_permid_stable(self):
        """Place a LMT order, modify price — permId must remain the same."""
        spy = make_spy()
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00
        order.tif = "GTC"
        order.outside_rth = True

        oid = self.wrapper.next_id
        self.client.place_order(oid, spy, order)
        assert self.wrapper.got_status.wait(timeout=30), "No status on place"
        perm_id_before = self.wrapper.perm_ids[oid]
        assert perm_id_before > 0

        # Modify: same orderId, new price
        self.wrapper.got_status.clear()
        order.lmt_price = 2.00
        self.client.place_order(oid, spy, order)
        assert self.wrapper.got_status.wait(timeout=15), "No status on modify"
        perm_id_after = self.wrapper.perm_ids[oid]

        assert perm_id_before == perm_id_after, \
            f"permId changed on modify: {perm_id_before} → {perm_id_after}"

        # Cleanup
        self.client.cancel_order(oid, "")
        time.sleep(2)

    def test_place_fill_and_executions(self):
        """Place a MKT order to trigger a fill, then query executions."""
        spy = make_spy()
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "MKT"
        order.tif = "DAY"

        oid = self.wrapper.next_id
        self.client.place_order(oid, spy, order)

        got_fill = self.wrapper.got_fill.wait(timeout=30)
        if not got_fill:
            pytest.skip("No fill received — market may be closed")

        fill_statuses = [s for s in self.wrapper.order_statuses
                         if s[0] == oid and (s[1] == "Filled" or s[2] > 0)]
        assert len(fill_statuses) > 0, "Should have fill status"

        # Query executions
        self.client.req_executions(8001)
        assert self.wrapper.got_exec_end.wait(timeout=15), "exec_details_end not received"
        our_execs = [e for e in self.wrapper.executions if e[1].symbol == "SPY"]
        assert len(our_execs) > 0, "Should have exec for SPY"

        # Sell back to flatten
        sell = Order()
        sell.action = "SELL"
        sell.total_quantity = 1
        sell.order_type = "MKT"
        sell.tif = "DAY"
        sell_oid = oid + 1
        self.client.place_order(sell_oid, spy, sell)
        time.sleep(5)
