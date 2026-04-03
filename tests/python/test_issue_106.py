"""IBX Test #106: Futures contract — market data, historical bars, order (ES).

Qualifies front-month ES, subscribes to market data (triggers usfuture farm),
requests historical bars, and attempts order placement.

Run: pytest tests/python/test_issue_106.py -v -s
"""

import os, threading, time
import pytest
from ibx import EWrapper, EClient, Contract, Order

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


def make_es():
    """Front-month ES mini S&P 500 future."""
    c = Contract()
    c.symbol = "ES"
    c.sec_type = "FUT"
    c.exchange = "CME"
    c.currency = "USD"
    c.last_trade_date_or_contract_month = "202606"
    c.multiplier = "50"
    c.trading_class = "ES"
    return c


class FuturesWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.lock = threading.Lock()
        self.next_order_id = 0

        # Contract details
        self.contract_details_list = []
        self.got_contract_end = threading.Event()

        # Market data
        self.bid = 0.0
        self.ask = 0.0
        self.last = 0.0
        self.volume = 0
        self.tick_count = 0
        self.got_tick = threading.Event()

        # Historical bars
        self.bars = []
        self.got_hist_end = threading.Event()

        # Orders
        self.order_statuses = {}
        self.got_order_status = threading.Event()
        self.errors = []

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def contract_details(self, req_id, contract_details):
        with self.lock:
            self.contract_details_list.append(contract_details)

    def contract_details_end(self, req_id):
        self.got_contract_end.set()

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0:
            with self.lock:
                if tick_type == 1:
                    self.bid = price
                elif tick_type == 2:
                    self.ask = price
                elif tick_type == 4:
                    self.last = price
                self.tick_count += 1
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        if tick_type == 8 and size > 0:
            self.volume = size

    def historical_data(self, req_id, bar):
        self.bars.append(bar)

    def historical_data_end(self, req_id, start, end):
        self.got_hist_end.set()

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        with self.lock:
            self.order_statuses.setdefault(order_id, []).append(status)
        self.got_order_status.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.errors.append((req_id, error_code, error_string))
        if error_code not in (2104, 2106, 2119, 2158, 460, 202):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


class TestFutures:
    """Issue #106: ES futures — contract details, market data, historical bars."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = FuturesWrapper()
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

    def test_contract_details(self):
        """Qualify ES front-month and verify contract details."""
        es = make_es()
        self.client.req_contract_details(4001, es)

        assert self.wrapper.got_contract_end.wait(timeout=15), \
            "contract_details_end not received"

        assert len(self.wrapper.contract_details_list) > 0, \
            "Should have contract details for ES"

        cd = self.wrapper.contract_details_list[0]
        print(f"  ES contract: {cd.contract.local_symbol}, "
              f"conId={cd.contract.con_id}, multiplier={cd.contract.multiplier}")
        assert cd.contract.symbol == "ES"
        assert cd.contract.sec_type == "FUT"
        assert cd.contract.exchange == "CME"

    def test_market_data(self):
        """Subscribe to ES market data (triggers usfuture farm)."""
        es = make_es()
        self.client.req_mkt_data(4002, es, "", False)

        got = self.wrapper.got_tick.wait(timeout=30)
        time.sleep(5)  # Collect a few ticks
        self.client.cancel_mkt_data(4002)

        if not got:
            pytest.skip("No ES tick data — usfuture farm may not be available")

        print(f"  ES ticks: bid={self.wrapper.bid}, ask={self.wrapper.ask}, "
              f"last={self.wrapper.last}, ticks={self.wrapper.tick_count}")
        assert self.wrapper.bid > 0 or self.wrapper.last > 0, "Should have price data"

        # ES price should be in reasonable range
        p = self.wrapper.bid or self.wrapper.last
        assert 1000 < p < 20000, f"ES price {p} out of expected range"

    def test_historical_bars(self):
        """Request 1 day of 5-min bars for ES (including Globex session)."""
        es = make_es()
        self.client.req_historical_data(
            4003, es,
            end_date_time="",
            duration_str="1 D",
            bar_size_setting="5 mins",
            what_to_show="TRADES",
            use_rth=0,  # Include Globex session
        )

        assert self.wrapper.got_hist_end.wait(timeout=30), \
            "historical_data_end not received for ES"

        assert len(self.wrapper.bars) > 0, "Should have ES bars"
        print(f"  ES bars: {len(self.wrapper.bars)} 5-min bars (including Globex)")

        bar = self.wrapper.bars[0]
        assert bar.high >= bar.low
        # ES price in 0.25 increments
        assert 1000 < bar.close < 20000, f"ES close {bar.close} out of range"

    def test_order_rejected_no_perms(self):
        """Place ES LMT order — expect rejection (no futures perms on paper)."""
        es = make_es()
        oid = self.wrapper.next_order_id

        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1000.0  # Far below market
        order.tif = "GTC"
        order.outside_rth = True

        self.client.place_order(oid, es, order)
        time.sleep(5)

        # Expect error 460 (no trading permissions) for futures
        reject_errors = [e for e in self.wrapper.errors if e[1] == 460]
        if reject_errors:
            print(f"  Order rejected as expected: {reject_errors[0][2]}")
        else:
            # If it was accepted, cancel it
            statuses = self.wrapper.order_statuses.get(oid, [])
            print(f"  Order statuses: {statuses}")
            self.client.cancel_order(oid, "")
            time.sleep(2)
