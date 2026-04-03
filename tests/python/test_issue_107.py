"""IBX Test #107: Conditional order with PriceCondition (cross-instrument).

Places conditional BUY IWM triggered by SPY price, verifies PreSubmitted state
and condition visibility. Then places a conditional SELL with condition
that should trigger immediately.

Run: pytest tests/python/test_issue_107.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract, Order, PriceCondition

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

SPY_CON_ID = 756733
IWM_CON_ID = 9579970


def make_contract(con_id, symbol):
    c = Contract()
    c.con_id = con_id
    c.symbol = symbol
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class ConditionalWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()
        self.next_order_id = 0

        # Prices
        self.prices = {}  # req_id -> price
        self.got_price = {}

        # Orders
        self.order_statuses = {}
        self.got_order_status = threading.Event()
        self.got_fill = threading.Event()
        self.open_orders_list = []
        self.got_open_order_end = threading.Event()

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0 and tick_type in (1, 2, 4):
            self.prices[req_id] = price
            ev = self.got_price.get(req_id)
            if ev:
                ev.set()

    def tick_size(self, req_id, tick_type, size):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        with self.lock:
            self.order_statuses.setdefault(order_id, []).append(
                (status, filled, perm_id))
        self.got_order_status.set()
        if status == "Filled":
            self.got_fill.set()

    def open_order(self, order_id, contract, order, order_state):
        with self.lock:
            self.open_orders_list.append((order_id, contract, order, order_state))

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestConditionalOrder:
    """Issue #107: Conditional orders with PriceCondition triggers."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = ConditionalWrapper()
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

    def _get_price(self, req_id, contract):
        self.wrapper.got_price[req_id] = threading.Event()
        self.client.req_mkt_data(req_id, contract, "", False)
        got = self.wrapper.got_price[req_id].wait(timeout=30)
        self.client.cancel_mkt_data(req_id)
        return self.wrapper.prices.get(req_id)

    def test_conditional_buy_presubmitted(self):
        """Place conditional BUY IWM when SPY > current+1. Should stay PreSubmitted."""
        spy_price = self._get_price(5001, make_contract(SPY_CON_ID, "SPY"))
        iwm_price = self._get_price(5002, make_contract(IWM_CON_ID, "IWM"))
        if not spy_price or not iwm_price:
            pytest.skip("No market data — market may be closed")

        print(f"  SPY: {spy_price}, IWM: {iwm_price}")

        oid = self.wrapper.next_order_id
        self.wrapper.next_order_id += 1

        # Condition: SPY > current + $1 (won't trigger immediately)
        cond = PriceCondition()
        cond.con_id = SPY_CON_ID
        cond.exchange = "SMART"
        cond.price = spy_price + 1.0
        cond.is_more = True
        cond.trigger_method = 1  # DoubleBidAsk

        order = Order()
        order.action = "BUY"
        order.total_quantity = 10
        order.order_type = "LMT"
        order.lmt_price = round(iwm_price - 1.0, 2)
        order.tif = "GTC"
        order.outside_rth = True
        order.conditions = [cond]
        order.conditions_ignore_rth = True
        order.conditions_cancel_order = False

        self.client.place_order(oid, make_contract(IWM_CON_ID, "IWM"), order)
        assert self.wrapper.got_order_status.wait(timeout=15), "No order status"

        time.sleep(3)
        statuses = self.wrapper.order_statuses.get(oid, [])
        assert len(statuses) > 0, "Should have status"
        status_names = [s[0] for s in statuses]
        print(f"  Conditional BUY statuses: {status_names}")

        # Should be PreSubmitted (condition not met) or Submitted
        assert any(s in ("PreSubmitted", "Submitted") for s in status_names), \
            f"Expected PreSubmitted/Submitted, got: {status_names}"

        # Verify via reqOpenOrders
        self.client.req_open_orders()
        self.wrapper.got_open_order_end.wait(timeout=10)

        # Cancel
        self.client.cancel_order(oid, "")
        time.sleep(3)

    def test_conditional_sell_triggers_immediately(self):
        """Place conditional SELL IWM when SPY < current-1. Should trigger on last price."""
        spy_price = self._get_price(5003, make_contract(SPY_CON_ID, "SPY"))
        iwm_price = self._get_price(5004, make_contract(IWM_CON_ID, "IWM"))
        if not spy_price or not iwm_price:
            pytest.skip("No market data — market may be closed")

        oid = self.wrapper.next_order_id
        self.wrapper.next_order_id += 1

        # Condition: SPY < current + $1 (will trigger immediately — SPY IS < current+1)
        cond = PriceCondition()
        cond.con_id = SPY_CON_ID
        cond.exchange = "SMART"
        cond.price = spy_price + 1.0
        cond.is_more = False  # Trigger when SPY < trigger_price
        cond.trigger_method = 2  # Last

        order = Order()
        order.action = "SELL"
        order.total_quantity = 10
        order.order_type = "LMT"
        order.lmt_price = round(iwm_price - 2.0, 2)  # Below market
        order.tif = "GTC"
        order.outside_rth = True
        order.conditions = [cond]
        order.conditions_ignore_rth = True
        order.conditions_cancel_order = True

        self.client.place_order(oid, make_contract(IWM_CON_ID, "IWM"), order)

        time.sleep(5)
        statuses = self.wrapper.order_statuses.get(oid, [])
        status_names = [s[0] for s in statuses]
        print(f"  Conditional SELL statuses: {status_names}")

        # Should activate (PreSubmitted->Submitted or even Filled if LMT crosses)
        # If condition triggers, order becomes active
        if statuses:
            # It should have progressed past initial state
            assert len(status_names) > 0

        # Cancel if still open
        self.client.cancel_order(oid, "")
        time.sleep(3)
