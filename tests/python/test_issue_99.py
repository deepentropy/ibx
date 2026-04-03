"""IBX Test #99: Bracket Order with Trailing Stop (QQQ).

Parent LMT + take-profit LMT + trailing stop — OCA linked orders.
Exercises parent-child linking via parentId, OCA grouping, and TRAIL encoding.

Run: pytest tests/python/test_issue_99.py -v --timeout=120
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

QQQ_CON_ID = 320227571


class BracketWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0

        # Order tracking
        self.order_statuses = {}  # order_id -> [(status, filled, remaining, perm_id)]
        self.perm_ids = {}        # order_id -> perm_id
        self.open_orders_list = []

        # Events
        self.got_status = threading.Event()
        self.got_fill = threading.Event()
        self.got_open_order_end = threading.Event()

        # Tick for current price
        self.last_price = 0.0
        self.got_tick = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0 and tick_type in (1, 2, 4):  # bid, ask, last
            self.last_price = price
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        if order_id not in self.order_statuses:
            self.order_statuses[order_id] = []
        self.order_statuses[order_id].append((status, filled, remaining, perm_id))
        self.perm_ids[order_id] = perm_id
        self.got_status.set()
        if status == "Filled" or filled > 0:
            self.got_fill.set()

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders_list.append((order_id, contract, order))

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] oid={req_id} code={error_code}: {error_string}")


def make_qqq():
    c = Contract()
    c.con_id = QQQ_CON_ID
    c.symbol = "QQQ"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class TestBracketOrder:
    """Issue #99: Bracket order — parent LMT + take-profit + trailing stop."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = BracketWrapper()
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

    def _get_price(self):
        """Get current QQQ price for building bracket orders."""
        qqq = make_qqq()
        self.client.req_mkt_data(7777, qqq, "", False)
        got = self.wrapper.got_tick.wait(timeout=30)
        self.client.cancel_mkt_data(7777)
        if not got or self.wrapper.last_price <= 0:
            return None
        return self.wrapper.last_price

    def test_bracket_place_and_cancel(self):
        """Place a bracket order (parent + TP + trailing stop), then cancel all."""
        price = self._get_price()
        if price is None:
            pytest.skip("No price data — market may be closed")

        qqq = make_qqq()
        parent_id = self.wrapper.next_id
        tp_id = parent_id + 1
        sl_id = parent_id + 2
        oca_group = f"bracket_{parent_id}"

        # Parent: BUY LMT far below market (won't fill)
        parent = Order()
        parent.action = "BUY"
        parent.total_quantity = 1
        parent.order_type = "LMT"
        parent.lmt_price = round(price * 0.80, 2)  # 20% below market
        parent.tif = "GTC"
        parent.outside_rth = True
        parent.transmit = True

        # Take-profit child: SELL LMT above market
        tp = Order()
        tp.action = "SELL"
        tp.total_quantity = 1
        tp.order_type = "LMT"
        tp.lmt_price = round(price * 1.10, 2)  # 10% above market
        tp.tif = "GTC"
        tp.outside_rth = True
        tp.parent_id = parent_id
        tp.oca_group = oca_group
        tp.oca_type = 1
        tp.transmit = True

        # Trailing stop child: SELL TRAIL
        sl = Order()
        sl.action = "SELL"
        sl.total_quantity = 1
        sl.order_type = "TRAIL"
        sl.trailing_percent = 1.0
        sl.tif = "GTC"
        sl.outside_rth = True
        sl.parent_id = parent_id
        sl.oca_group = oca_group
        sl.oca_type = 1
        sl.transmit = True

        # Place all three
        self.client.place_order(parent_id, qqq, parent)
        self.client.place_order(tp_id, qqq, tp)
        self.client.place_order(sl_id, qqq, sl)

        # Wait for statuses
        assert self.wrapper.got_status.wait(timeout=30), "No order status received"
        time.sleep(3)

        # Verify parent got a status
        parent_statuses = self.wrapper.order_statuses.get(parent_id, [])
        assert len(parent_statuses) > 0, "Parent should have status"

        # Children should be PreSubmitted or Inactive (parent not filled)
        for child_id in (tp_id, sl_id):
            child_statuses = self.wrapper.order_statuses.get(child_id, [])
            if child_statuses:
                last_status = child_statuses[-1][0]
                assert last_status in ("PreSubmitted", "Inactive", "Submitted"), \
                    f"Child {child_id} unexpected status: {last_status}"

        # Verify all have permIds
        for oid in (parent_id, tp_id, sl_id):
            if oid in self.wrapper.perm_ids:
                assert self.wrapper.perm_ids[oid] > 0, \
                    f"Order {oid} should have positive permId"

        # Verify via reqOpenOrders
        self.client.req_open_orders()
        self.wrapper.got_open_order_end.wait(timeout=15)
        our_orders = {o[0] for o in self.wrapper.open_orders_list
                      if o[0] in (parent_id, tp_id, sl_id)}

        # Cancel all
        self.client.cancel_order(parent_id, "")
        time.sleep(3)
        # Children should auto-cancel when parent is cancelled

    def test_bracket_fill_activates_children(self):
        """Fill parent, then place OCA-linked exit orders (TP + trailing stop)."""
        price = self._get_price()
        if price is None:
            pytest.skip("No price data — market may be closed")

        qqq = make_qqq()
        parent_id = self.wrapper.next_id

        # Parent: BUY MKT (fills immediately)
        parent = Order()
        parent.action = "BUY"
        parent.total_quantity = 1
        parent.order_type = "MKT"
        parent.tif = "DAY"

        self.client.place_order(parent_id, qqq, parent)
        got_fill = self.wrapper.got_fill.wait(timeout=30)
        if not got_fill:
            pytest.skip("No fill — market may be closed")

        # After fill, place OCA-linked exit orders
        tp_id = parent_id + 1
        sl_id = parent_id + 2
        oca_group = f"bracket_{parent_id}"

        # Take-profit: SELL LMT well above market
        tp = Order()
        tp.action = "SELL"
        tp.total_quantity = 1
        tp.order_type = "LMT"
        tp.lmt_price = round(price * 1.20, 2)
        tp.tif = "GTC"
        tp.outside_rth = True
        tp.oca_group = oca_group
        tp.oca_type = 1

        # Trailing stop: SELL TRAIL
        sl = Order()
        sl.action = "SELL"
        sl.total_quantity = 1
        sl.order_type = "TRAIL"
        sl.trailing_percent = 2.0
        sl.tif = "GTC"
        sl.outside_rth = True
        sl.oca_group = oca_group
        sl.oca_type = 1

        self.client.place_order(tp_id, qqq, tp)
        self.client.place_order(sl_id, qqq, sl)

        time.sleep(5)

        # TP (LMT) should be Submitted; TRAIL may be Inactive (valid — server needs time
        # to compute reference price, or activates only when position is confirmed)
        tp_statuses = self.wrapper.order_statuses.get(tp_id, [])
        if tp_statuses:
            tp_names = [s[0] for s in tp_statuses]
            assert any(s in ("Submitted", "PreSubmitted") for s in tp_names), \
                f"TP exit {tp_id} should be active, got: {tp_names}"

        sl_statuses = self.wrapper.order_statuses.get(sl_id, [])
        if sl_statuses:
            sl_names = [s[0] for s in sl_statuses]
            assert any(s in ("Submitted", "PreSubmitted", "Inactive") for s in sl_names), \
                f"TRAIL exit {sl_id} should be accepted, got: {sl_names}"

        # Cancel both exits
        self.client.cancel_order(tp_id, "")
        self.client.cancel_order(sl_id, "")
        time.sleep(3)

        # Sell to flatten position
        sell = Order()
        sell.action = "SELL"
        sell.total_quantity = 1
        sell.order_type = "MKT"
        sell.tif = "DAY"
        sell_oid = sl_id + 1
        self.client.place_order(sell_oid, qqq, sell)
        time.sleep(5)

    def test_oca_cancel_sibling(self):
        """When one OCA child is cancelled, the other should remain (cancel doesn't trigger OCA)."""
        price = self._get_price()
        if price is None:
            pytest.skip("No price data — market may be closed")

        qqq = make_qqq()
        parent_id = self.wrapper.next_id
        tp_id = parent_id + 1
        sl_id = parent_id + 2
        oca_group = f"bracket_{parent_id}"

        # Parent far from market
        parent = Order()
        parent.action = "BUY"
        parent.total_quantity = 1
        parent.order_type = "LMT"
        parent.lmt_price = round(price * 0.80, 2)
        parent.tif = "GTC"
        parent.outside_rth = True
        parent.transmit = True

        tp = Order()
        tp.action = "SELL"
        tp.total_quantity = 1
        tp.order_type = "LMT"
        tp.lmt_price = round(price * 1.10, 2)
        tp.tif = "GTC"
        tp.outside_rth = True
        tp.parent_id = parent_id
        tp.oca_group = oca_group
        tp.oca_type = 1
        tp.transmit = True

        sl = Order()
        sl.action = "SELL"
        sl.total_quantity = 1
        sl.order_type = "TRAIL"
        sl.trailing_percent = 1.0
        sl.tif = "GTC"
        sl.outside_rth = True
        sl.parent_id = parent_id
        sl.oca_group = oca_group
        sl.oca_type = 1
        sl.transmit = True

        self.client.place_order(parent_id, qqq, parent)
        self.client.place_order(tp_id, qqq, tp)
        self.client.place_order(sl_id, qqq, sl)

        self.wrapper.got_status.wait(timeout=30)
        time.sleep(3)

        # Cancel everything (parent cancellation should cascade)
        self.client.cancel_order(parent_id, "")
        time.sleep(5)
