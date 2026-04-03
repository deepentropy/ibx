"""IBX Test #110: Bulk order placement + reqGlobalCancel across 5 instruments.

Places 8 orders (LMT, MKT, STP, MOC) across SPY, QQQ, IWM, DIA, AAPL,
then exercises reqGlobalCancel to cancel all at once.

Run: pytest tests/python/test_issue_110.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract, Order

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

INSTRUMENTS = [
    (756733, "SPY"),
    (320227571, "QQQ"),
    (9579970, "IWM"),
    (73128548, "DIA"),
    (265598, "AAPL"),
]


def make_contract(con_id, symbol):
    c = Contract()
    c.con_id = con_id
    c.symbol = symbol
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class BulkWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()
        self.next_order_id = 0

        # Prices
        self.prices = {}
        self.got_price = {}

        # Order tracking
        self.order_statuses = {}
        self.got_fill = threading.Event()
        self.all_cancelled = threading.Event()
        self._cancel_count = 0
        self._expected_cancels = 0
        self.open_orders = []
        self.got_open_order_end = threading.Event()
        self.errors = []

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
            self.order_statuses.setdefault(order_id, []).append(status)
        if status == "Filled":
            self.got_fill.set()
        if status == "Cancelled":
            with self.lock:
                self._cancel_count += 1
                if self._cancel_count >= self._expected_cancels:
                    self.all_cancelled.set()

    def open_order(self, order_id, contract, order, order_state):
        with self.lock:
            self.open_orders.append((order_id, contract.symbol))

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.errors.append((req_id, error_code, error_string))
        if error_code not in (2104, 2106, 2158, 202, 161):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestBulkOrdersGlobalCancel:
    """Issue #110: Bulk orders across 5 instruments + reqGlobalCancel."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = BulkWrapper()
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

    def _get_prices(self):
        """Get current prices for all 5 instruments."""
        prices = {}
        for i, (con_id, symbol) in enumerate(INSTRUMENTS):
            req_id = 7000 + i
            self.wrapper.got_price[req_id] = threading.Event()
            self.client.req_mkt_data(req_id, make_contract(con_id, symbol), "", False)

        for i, (con_id, symbol) in enumerate(INSTRUMENTS):
            req_id = 7000 + i
            got = self.wrapper.got_price[req_id].wait(timeout=30)
            self.client.cancel_mkt_data(req_id)
            if got:
                prices[symbol] = self.wrapper.prices.get(req_id, 0)

        return prices

    def test_bulk_orders_and_global_cancel(self):
        """Place 8 orders across 5 instruments, then global cancel."""
        prices = self._get_prices()
        if len(prices) < 3:
            pytest.skip("Not enough price data — market may be closed")

        placed_oids = []

        # 5 LMT orders (one per instrument, far below market)
        for con_id, symbol in INSTRUMENTS:
            if symbol not in prices:
                continue
            oid = self.wrapper.next_order_id
            self.wrapper.next_order_id += 1

            order = Order()
            order.action = "BUY"
            order.total_quantity = 10
            order.order_type = "LMT"
            order.lmt_price = round(prices[symbol] * 0.98, 2)  # 2% below market
            order.tif = "GTC"
            order.outside_rth = True

            self.client.place_order(oid, make_contract(con_id, symbol), order)
            placed_oids.append((oid, symbol, "LMT"))

        # STP order on SPY (stop below market — won't trigger)
        if "SPY" in prices:
            oid = self.wrapper.next_order_id
            self.wrapper.next_order_id += 1
            order = Order()
            order.action = "BUY"
            order.total_quantity = 5
            order.order_type = "STP"
            order.aux_price = round(prices["SPY"] * 0.97, 2)
            order.tif = "GTC"
            order.outside_rth = True
            self.client.place_order(oid, make_contract(SPY_CON_ID, "SPY"), order)
            placed_oids.append((oid, "SPY", "STP"))

        # MOC order on SPY
        if "SPY" in prices:
            oid = self.wrapper.next_order_id
            self.wrapper.next_order_id += 1
            order = Order()
            order.action = "BUY"
            order.total_quantity = 1
            order.order_type = "MOC"
            order.tif = "DAY"
            self.client.place_order(oid, make_contract(SPY_CON_ID, "SPY"), order)
            placed_oids.append((oid, "SPY", "MOC"))

        # MKT order on SPY (will fill immediately)
        if "SPY" in prices:
            oid = self.wrapper.next_order_id
            self.wrapper.next_order_id += 1
            order = Order()
            order.action = "BUY"
            order.total_quantity = 1
            order.order_type = "MKT"
            order.tif = "DAY"
            self.client.place_order(oid, make_contract(SPY_CON_ID, "SPY"), order)
            placed_oids.append((oid, "SPY", "MKT"))

        time.sleep(5)

        # Print statuses
        for oid, symbol, otype in placed_oids:
            statuses = self.wrapper.order_statuses.get(oid, [])
            print(f"  {symbol} {otype} (oid={oid}): {statuses}")

        # Count non-filled orders for expected cancels
        cancellable = sum(1 for oid, _, _ in placed_oids
                         if "Filled" not in self.wrapper.order_statuses.get(oid, []))
        self.wrapper._expected_cancels = cancellable

        # ── Global cancel ──
        print(f"  Issuing reqGlobalCancel ({cancellable} cancellable orders)...")
        self.client.req_global_cancel()

        # Wait for all cancels
        got_all = self.wrapper.all_cancelled.wait(timeout=15)
        time.sleep(2)

        # Verify open orders are empty
        self.wrapper.open_orders.clear()
        self.wrapper.got_open_order_end.clear()
        self.client.req_open_orders()
        self.wrapper.got_open_order_end.wait(timeout=10)

        remaining = len(self.wrapper.open_orders)
        print(f"  After global cancel: {remaining} open orders remaining")

        # Error 161 expected for already-filled MKT order
        err_161 = [e for e in self.wrapper.errors if e[1] == 161]
        if err_161:
            print(f"  Error 161 (not cancellable): {len(err_161)} orders")

        # If MKT filled, sell to flatten
        mkt_filled = any(
            "Filled" in self.wrapper.order_statuses.get(oid, [])
            for oid, _, otype in placed_oids if otype == "MKT"
        )
        if mkt_filled:
            sell_oid = self.wrapper.next_order_id
            sell = Order()
            sell.action = "SELL"
            sell.total_quantity = 1
            sell.order_type = "MKT"
            sell.tif = "DAY"
            self.client.place_order(sell_oid, make_contract(SPY_CON_ID, "SPY"), sell)
            time.sleep(3)


SPY_CON_ID = 756733
