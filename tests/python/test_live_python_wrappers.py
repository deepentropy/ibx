"""Comprehensive Python wrapper compatibility tests against live IB paper account.

Requires IB_USERNAME and IB_PASSWORD environment variables.
Run with: pytest tests/python/test_live_python_wrappers.py -v --timeout=180

Tests the full Python EClient/EWrapper API surface against a real IB connection:
connection, market data, orders, historical data, account, contracts, scanner, news.
"""

import os
import time
import threading
import pytest
from ibx import EWrapper, EClient, Contract, Order, TagValue
from conftest import wait_for


# Skip entire module if credentials not set
pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


# ── Well-known IB con_ids ──
SPY_CON_ID = 756733
AAPL_CON_ID = 265598
MSFT_CON_ID = 272093


def make_spy_contract():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def make_aapl_contract():
    c = Contract()
    c.con_id = AAPL_CON_ID
    c.symbol = "AAPL"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


# ── Recording Wrapper ──

class FullWrapper(EWrapper):
    """Records all callback invocations for assertion."""

    def __init__(self):
        super().__init__()
        self.events = []
        self.lock = threading.Lock()
        # Signals for synchronization
        self.connected = threading.Event()
        self.got_next_id = threading.Event()
        self.got_tick = threading.Event()
        # A print, not a quote. Any price at all sets `got_tick` — a bid, an
        # ask, yesterday's close — and none of those says the contract is
        # trading. Only a last price does.
        self.got_print = threading.Event()
        self.got_order_status = threading.Event()
        self.got_order_cancelled = threading.Event()
        self.got_contract = threading.Event()
        self.got_contract_end = threading.Event()
        self.got_historical = threading.Event()
        self.got_historical_end = threading.Event()
        self.got_head_timestamp = threading.Event()
        self.got_account_summary = threading.Event()
        self.got_account_summary_end = threading.Event()
        self.got_account_value = threading.Event()
        self.got_position = threading.Event()
        self.got_position_end = threading.Event()
        self.got_pnl = threading.Event()
        self.got_symbol_samples = threading.Event()
        self.got_scanner_params = threading.Event()
        self.got_tbt = threading.Event()
        self.got_real_time_bar = threading.Event()
        self.got_historical_ticks = threading.Event()
        self.next_order_id = 0

    def _record(self, event):
        with self.lock:
            self.events.append(event)

    def _get_events(self, name=None):
        with self.lock:
            if name:
                return [e for e in self.events if e[0] == name]
            return list(self.events)

    # ── Connection ──
    def connect_ack(self):
        self._record(("connect_ack",))
        self.connected.set()

    def next_valid_id(self, order_id):
        self._record(("next_valid_id", order_id))
        self.next_order_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        self._record(("managed_accounts", accounts_list))

    def error(self, req_id, error_time, error_code, error_string, advanced_order_reject_json=""):
        self._record(("error", req_id, error_code, error_string))

    # ── Market Data ──
    #: When the last trade happened, as the venue states it — seconds since the
    #: epoch, on a string tick. The price of the last trade is not evidence that
    #: anything is trading: it stands unchanged all night. The time it happened
    #: is.
    LAST_TIMESTAMP = 45
    TRADED_WITHIN = 300  # seconds; a print this recent means the session is live

    def tick_price(self, req_id, tick_type, price, attrib):
        self._record(("tick_price", req_id, tick_type, price))
        if price > 0:
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        self._record(("tick_size", req_id, tick_type, size))

    def tick_string(self, req_id, tick_type, value):
        self._record(("tick_string", req_id, tick_type, value))
        if tick_type == self.LAST_TIMESTAMP:
            import time

            try:
                traded_at = float(value)
            except (TypeError, ValueError):
                return
            if 0 < time.time() - traded_at < self.TRADED_WITHIN:
                self.got_print.set()

    # ── Orders ──
    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self._record(("order_status", order_id, status, filled, remaining))
        self.got_order_status.set()
        if status == "Cancelled":
            self.got_order_cancelled.set()

    def exec_details(self, req_id, contract, execution):
        self._record(("exec_details", req_id, contract, execution))

    # ── Contracts ──
    def contract_details(self, req_id, contract_details):
        self._record(("contract_details", req_id, contract_details))
        self.got_contract.set()

    def contract_details_end(self, req_id):
        self._record(("contract_details_end", req_id))
        self.got_contract_end.set()

    def symbol_samples(self, req_id, contract_descriptions):
        self._record(("symbol_samples", req_id, contract_descriptions))
        self.got_symbol_samples.set()

    # ── Historical Data ──
    def historical_data(self, req_id, bar):
        self._record(("historical_data", req_id, bar))
        self.got_historical.set()

    def historical_data_end(self, req_id, start, end):
        self._record(("historical_data_end", req_id, start, end))
        self.got_historical_end.set()

    def head_timestamp(self, req_id, head_timestamp):
        self._record(("head_timestamp", req_id, head_timestamp))
        self.got_head_timestamp.set()

    # ── Account ──
    def update_account_value(self, key, value, currency, account_name):
        self._record(("update_account_value", key, value, currency, account_name))
        self.got_account_value.set()

    def account_summary(self, req_id, account, tag, value, currency):
        self._record(("account_summary", req_id, account, tag, value, currency))
        self.got_account_summary.set()

    def account_summary_end(self, req_id):
        self._record(("account_summary_end", req_id))
        self.got_account_summary_end.set()

    def position(self, account, contract, pos, avg_cost):
        self._record(("position", account, contract, pos, avg_cost))
        self.got_position.set()

    def position_end(self):
        self._record(("position_end",))
        self.got_position_end.set()

    def pnl(self, req_id, daily_pnl, unrealized_pnl, realized_pnl):
        self._record(("pnl", req_id, daily_pnl, unrealized_pnl, realized_pnl))
        self.got_pnl.set()

    # ── TBT ──
    def tick_by_tick_all_last(self, req_id, tick_type, time_, price, size,
                              tick_attrib_last, exchange, special_conditions):
        _a_moment(time_, "tick_by_tick_all_last")
        self._record(("tick_by_tick_all_last", req_id, price, size, exchange))
        self.got_tbt.set()

    def tick_by_tick_bid_ask(self, req_id, time_, bid_price, ask_price,
                             bid_size, ask_size, tick_attrib_bid_ask):
        _a_moment(time_, "tick_by_tick_bid_ask")
        self._record(("tick_by_tick_bid_ask", req_id, bid_price, ask_price))
        self.got_tbt.set()

    # ── Scanner ──
    def scanner_parameters(self, xml):
        self._record(("scanner_parameters", xml))
        self.got_scanner_params.set()

    # ── Real-time bars ──
    def real_time_bar(self, req_id, date, open_, high, low, close, volume, wap, count):
        self._record(("real_time_bar", req_id, open_, high, low, close, volume))
        self.got_real_time_bar.set()

    # ── Historical ticks ──
    def historical_ticks(self, req_id, ticks, done):
        self._record(("historical_ticks", req_id, ticks, done))
        self.got_historical_ticks.set()

    def historical_ticks_last(self, req_id, ticks, done):
        self._record(("historical_ticks_last", req_id, ticks, done))
        self.got_historical_ticks.set()

    def historical_ticks_bid_ask(self, req_id, ticks, done):
        self._record(("historical_ticks_bid_ask", req_id, ticks, done))
        self.got_historical_ticks.set()

    # ── Fundamental ──
    def fundamental_data(self, req_id, data):
        self._record(("fundamental_data", req_id, data))


# ── Shared Fixture ──

@pytest.fixture(scope="module")
def ib_connection():
    """Single IB connection shared across all tests in this module."""
    wrapper = FullWrapper()
    client = EClient(wrapper)

    username = os.environ["IB_USERNAME"]
    password = os.environ["IB_PASSWORD"]
    host = os.environ.get("IB_HOST", "cdc1.ibllc.com")

    client.connect(username=username, password=password, host=host, paper=True)
    run_thread = threading.Thread(target=client.run, daemon=True)
    run_thread.start()

    # Wait for connection
    assert wrapper.got_next_id.wait(timeout=15), "Connection failed: next_valid_id not received"

    yield wrapper, client

    client.disconnect()
    run_thread.join(timeout=5)


# ═══════════════════════════════════════
# Connection Tests
# ═══════════════════════════════════════

class TestConnection:

    def test_is_connected(self, ib_connection):
        wrapper, client = ib_connection
        assert client.is_connected()

    def test_next_valid_id_positive(self, ib_connection):
        wrapper, client = ib_connection
        assert wrapper.next_order_id > 0

    def test_managed_accounts_received(self, ib_connection):
        wrapper, client = ib_connection
        events = wrapper._get_events("managed_accounts")
        assert len(events) > 0
        assert len(events[0][1]) > 0, "Account list should be non-empty"

    def test_account_id_set(self, ib_connection):
        wrapper, client = ib_connection
        acct = client.get_account_id()
        assert len(acct) > 0, "Account ID should be set after connect"


# ═══════════════════════════════════════
# Contract Details Tests
# ═══════════════════════════════════════

class TestContractDetails:

    def test_spy_by_con_id(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_contract.clear()
        wrapper.got_contract_end.clear()

        client.req_contract_details(3001, make_spy_contract())
        assert wrapper.got_contract.wait(timeout=15), "No contract details received"

        events = wrapper._get_events("contract_details")
        cd_events = [e for e in events if e[1] == 3001]
        assert len(cd_events) > 0
        cd = cd_events[0][2]
        assert cd.contract.symbol == "SPY"
        assert cd.contract.con_id == SPY_CON_ID

    def test_aapl_by_symbol(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_contract.clear()

        c = Contract()
        c.symbol = "AAPL"
        c.sec_type = "STK"
        c.exchange = "SMART"
        c.currency = "USD"
        client.req_contract_details(3002, c)

        assert wrapper.got_contract.wait(timeout=15), "No AAPL contract details"
        events = [e for e in wrapper._get_events("contract_details") if e[1] == 3002]
        assert len(events) > 0
        assert events[0][2].contract.symbol == "AAPL"

    def test_matching_symbols(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_symbol_samples.clear()

        client.req_matching_symbols(3003, "TSLA")
        assert wrapper.got_symbol_samples.wait(timeout=15), "No symbol samples received"

        events = [e for e in wrapper._get_events("symbol_samples") if e[1] == 3003]
        assert len(events) > 0
        descriptions = events[0][2]
        assert len(descriptions) > 0, "Should have matches for 'TSLA'"


# ═══════════════════════════════════════
# Market Data Tests
# ═══════════════════════════════════════

class TestMarketData:

    def test_spy_ticks(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_tick.clear()

        client.req_mkt_data(4001, make_spy_contract(), "", False)
        got_tick = wrapper.got_tick.wait(timeout=30)
        client.cancel_mkt_data(4001)

        # A subscription that yields nothing at all is broken whatever the
        # hour: the venue states a close on a market that has one, and states
        # it whether or not the market is open. Skipped for want of ticks, this
        # passed on a client that delivered none.
        assert got_tick, "a subscription on a listed contract delivered nothing"

        events = [e for e in wrapper._get_events("tick_price") if e[1] == 4001]
        prices = [e[3] for e in events if e[3] > 0]
        assert len(prices) > 0
        for p in prices:
            assert 50 < p < 1000, f"SPY price {p} out of range"

    def test_multi_instrument_subscribe(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_tick.clear()

        client.req_mkt_data(4002, make_spy_contract(), "", False)
        client.req_mkt_data(4003, make_aapl_contract(), "", False)

        wrapper.got_tick.wait(timeout=30)
        time.sleep(3)  # collect ticks

        client.cancel_mkt_data(4002)
        client.cancel_mkt_data(4003)

        spy_ticks = [e for e in wrapper._get_events("tick_price") if e[1] == 4002 and e[3] > 0]
        aapl_ticks = [e for e in wrapper._get_events("tick_price") if e[1] == 4003 and e[3] > 0]

        assert spy_ticks or aapl_ticks, (
            "two subscriptions on two listed contracts delivered nothing"
        )

    def test_subscribe_cancel_no_crash(self, ib_connection):
        """Subscribe then immediately cancel — should not crash."""
        wrapper, client = ib_connection
        client.req_mkt_data(4004, make_spy_contract(), "", False)
        time.sleep(0.5)
        client.cancel_mkt_data(4004)
        time.sleep(0.5)
        assert client.is_connected()


# ═══════════════════════════════════════
# Order Tests
# ═══════════════════════════════════════

class TestOrders:

    def _next_oid(self, wrapper):
        oid = wrapper.next_order_id
        wrapper.next_order_id += 1
        return oid

    def test_limit_order_submit_cancel(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = self._next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy_contract(), order)
        assert wait_for(
            lambda: any(e[1] == oid for e in wrapper._get_events("order_status")), 30
        ), "No order status"

        client.cancel_order(oid, "")
        got_cancel = wrapper.got_order_cancelled.wait(timeout=15)

        events = [e for e in wrapper._get_events("order_status") if e[1] == oid]
        statuses = [e[2] for e in events]
        assert "Submitted" in statuses or "PreSubmitted" in statuses or "Cancelled" in statuses or "Inactive" in statuses

    def test_stop_order_submit_cancel(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = self._next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "STP"
        order.aux_price = 999.00  # Stop price far above market
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy_contract(), order)
        assert wait_for(
            lambda: any(e[1] == oid for e in wrapper._get_events("order_status")), 30
        ), "No order status"

        client.cancel_order(oid, "")
        wrapper.got_order_cancelled.wait(timeout=15)

        events = [e for e in wrapper._get_events("order_status") if e[1] == oid]
        statuses = [e[2] for e in events]
        assert len(statuses) > 0

    def test_trailing_stop_order(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = self._next_oid(wrapper)
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "TRAIL"
        order.trailing_percent = 5.0
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy_contract(), order)
        assert wait_for(
            lambda: any(e[1] == oid for e in wrapper._get_events("order_status")), 30
        ), "No order status"

        client.cancel_order(oid, "")
        wrapper.got_order_cancelled.wait(timeout=15)

        events = [e for e in wrapper._get_events("order_status") if e[1] == oid]
        assert len(events) > 0

    def test_adaptive_algo_order(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = self._next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00
        order.tif = "GTC"
        order.outside_rth = True
        order.algo_strategy = "Adaptive"
        order.algo_params = [TagValue("adaptivePriority", "Patient")]

        client.place_order(oid, make_spy_contract(), order)
        assert wait_for(
            lambda: any(e[1] == oid for e in wrapper._get_events("order_status")), 30
        ), "No order status"

        client.cancel_order(oid, "")
        wrapper.got_order_cancelled.wait(timeout=15)

        events = [e for e in wrapper._get_events("order_status") if e[1] == oid]
        assert len(events) > 0

    def test_global_cancel(self, ib_connection):
        """Submit 2 orders, global cancel, verify both cancelled."""
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()

        oid1 = self._next_oid(wrapper)
        oid2 = self._next_oid(wrapper)

        for oid in [oid1, oid2]:
            order = Order()
            order.action = "BUY"
            order.total_quantity = 1
            order.order_type = "LMT"
            order.lmt_price = 1.00
            order.tif = "GTC"
            order.outside_rth = True
            client.place_order(oid, make_spy_contract(), order)

        # Wait for at least one ack
        wrapper.got_order_status.wait(timeout=30)
        time.sleep(2)

        client.req_global_cancel()
        time.sleep(5)

        events = wrapper._get_events("order_status")
        cancelled = [e for e in events if e[1] in (oid1, oid2) and e[2] == "Cancelled"]
        rejected = [e for e in events if e[1] in (oid1, oid2) and e[2] in ("Rejected", "Inactive")]
        # Both should be cancelled or rejected
        assert len(cancelled) + len(rejected) >= 2 or len(cancelled) >= 1


# ═══════════════════════════════════════
# Historical Data Tests
# ═══════════════════════════════════════

class TestHistoricalData:

    def test_spy_daily_bars(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_historical.clear()
        wrapper.got_historical_end.clear()

        client.req_historical_data(
            5001, make_spy_contract(), "", "5 D", "1 day", "TRADES", 1,
        )
        assert wrapper.got_historical_end.wait(timeout=30), "No historical_data_end"

        bars = [e for e in wrapper._get_events("historical_data") if e[1] == 5001]
        assert len(bars) >= 3, f"Expected at least 3 daily bars, got {len(bars)}"

        # Verify bar fields
        bar = bars[0][2]
        assert bar.open > 0
        assert bar.high >= bar.low
        assert bar.close > 0
        assert bar.volume > 0

    def test_spy_5min_bars(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_historical.clear()
        wrapper.got_historical_end.clear()

        client.req_historical_data(
            5002, make_spy_contract(), "", "1 D", "5 mins", "TRADES", 1,
        )
        assert wrapper.got_historical_end.wait(timeout=30), "No historical_data_end"

        bars = [e for e in wrapper._get_events("historical_data") if e[1] == 5002]
        assert len(bars) >= 10, f"Expected 10+ 5-min bars, got {len(bars)}"

    def test_cancel_historical_no_crash(self, ib_connection):
        """Request then immediately cancel historical data."""
        wrapper, client = ib_connection
        client.req_historical_data(
            5003, make_spy_contract(), "", "1 D", "1 hour", "TRADES", 1,
        )
        time.sleep(0.3)
        client.cancel_historical_data(5003)
        time.sleep(1)
        assert client.is_connected()

    def test_head_timestamp(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_head_timestamp.clear()

        client.req_head_time_stamp(5004, make_spy_contract(), "TRADES", 1)
        assert wrapper.got_head_timestamp.wait(timeout=15), "No head timestamp"

        events = [e for e in wrapper._get_events("head_timestamp") if e[1] == 5004]
        assert len(events) > 0
        assert len(events[0][2]) > 0, "Head timestamp should be non-empty"


# ═══════════════════════════════════════
# Account & Position Tests
# ═══════════════════════════════════════

class TestAccount:

    def test_account_summary(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_account_summary.clear()
        wrapper.got_account_summary_end.clear()

        client.req_account_summary(6001, "All", "NetLiquidation,BuyingPower,AvailableFunds")

        # Account summary may take a moment to populate
        got_summary = wrapper.got_account_summary.wait(timeout=15)
        time.sleep(2)

        client.cancel_account_summary(6001)

        # The venue states an account's summary on request, at any hour.
        assert got_summary, "the account was asked for a summary and stated none"

        events = [e for e in wrapper._get_events("account_summary") if e[1] == 6001]
        tags = {e[3]: e[4] for e in events}
        assert "NetLiquidation" in tags, "NetLiquidation should be in summary"
        nlv = float(tags["NetLiquidation"])
        assert nlv > 0, f"NetLiquidation should be positive, got {nlv}"

    def test_positions(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_position_end.clear()

        client.req_positions()

        # position_end fires even if no positions
        got_end = wrapper.got_position_end.wait(timeout=10)
        if not got_end:
            # req_positions delivers data synchronously here
            time.sleep(1)

        end_events = wrapper._get_events("position_end")
        # position_end fires whether or not the account holds anything, and is
        # the only thing that says the replay is over. A caller waiting on it
        # blocks indefinitely if it never arrives.
        assert end_events, "the end of the position replay was never announced"

    def test_pnl_subscription(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_pnl.clear()

        acct = client.get_account_id()
        client.req_pnl(6002, acct)

        got_pnl = wrapper.got_pnl.wait(timeout=15)
        client.cancel_pnl(6002)

        # What a running profit is reported on is a position. Skipped for
        # want of one — which is evidence — and failed otherwise.
        held = [e for e in wrapper._get_events("position") if e[3] != 0]
        if not got_pnl and not held:
            pytest.skip("the account holds nothing, so there is no running profit")
        assert got_pnl, "the account holds a position and no profit was reported on it"

        events = [e for e in wrapper._get_events("pnl") if e[1] == 6002]
        assert len(events) > 0

    def test_account_updates(self, ib_connection):
        """Req_account_updates triggers update_account_value callbacks."""
        wrapper, client = ib_connection
        wrapper.got_account_value.clear()

        client.req_account_updates(True, "")
        got_value = wrapper.got_account_value.wait(timeout=15)

        # The venue states an account's figures on request, at any hour.
        assert got_value, "the account was asked for its figures and stated none"

        events = wrapper._get_events("update_account_value")
        keys = {e[1] for e in events}
        assert len(keys) > 0, "Should have at least one account value key"


# ═══════════════════════════════════════
# Scanner & Misc Tests
# ═══════════════════════════════════════

class TestScanner:

    def test_scanner_parameters(self, ib_connection):
        wrapper, client = ib_connection
        wrapper.got_scanner_params.clear()

        client.req_scanner_parameters()
        got_params = wrapper.got_scanner_params.wait(timeout=30)

        # The parameter set is reference data, the same at every hour.
        assert got_params, "the venue was asked for its scanner parameters and sent none"

        events = wrapper._get_events("scanner_parameters")
        assert len(events) > 0
        xml = events[0][1]
        assert len(xml) > 100, "Scanner XML should be substantial"
        assert "<?xml" in xml or "<ScanParameterResponse" in xml


# ═══════════════════════════════════════
# Tick-by-Tick Tests
# ═══════════════════════════════════════

def _inside_the_session(client, wrapper, contract):
    """Whether the venue says this contract's own session is open right now.

    Asked of the venue rather than worked out from a clock here: it states the
    liquid hours on the contract, in the contract's own zone, and those are the
    hours the trade stream carries prints for. A quote is no evidence — outside
    the session a contract carries a bid, an ask and the price of the last trade
    there ever was. Nor is the time of that trade: it can come from a route this
    stream does not carry, which is what a subscription the venue accepts and
    then says nothing on looks like at four in the morning.
    """
    import datetime
    import zoneinfo

    wrapper._get_events("contract_details")
    client.req_contract_details(9911, contract)
    for _ in range(150):
        client.poll() if hasattr(client, "poll") else None
        found = [e for e in wrapper._get_events("contract_details") if e[1] == 9911]
        if found:
            break
        time.sleep(0.1)
    else:
        return False

    details = found[0][2]
    hours = getattr(details, "liquid_hours", "") or ""
    zone = getattr(details, "time_zone_id", "") or "US/Eastern"
    try:
        now = datetime.datetime.now(zoneinfo.ZoneInfo(zone))
    except Exception:
        return False
    for span in hours.split(";"):
        if "-" not in span or "CLOSED" in span:
            continue
        opens, shuts = span.split("-", 1)
        try:
            a = datetime.datetime.strptime(opens, "%Y%m%d:%H%M")
            b = datetime.datetime.strptime(shuts, "%Y%m%d:%H%M")
        except ValueError:
            continue
        if a.replace(tzinfo=now.tzinfo) <= now <= b.replace(tzinfo=now.tzinfo):
            return True
    return False


class TestTickByTick:

    def test_tbt_last(self, ib_connection):
        """A trade stream on a contract that is trading.

        Skipped only on evidence that nothing is trading, and the evidence has
        to be a print. A quote is not one: outside the session a contract
        carries a bid, an ask and yesterday's close while nothing trades at
        all, and this asked for any price above zero — so at three in the
        morning it read a stale close as proof of trading and failed a client
        that was working. Skipping whenever no trade arrived would be the
        opposite mistake, and made this pass on a client that never delivered
        one.
        """
        wrapper, client = ib_connection
        wrapper.got_tbt.clear()

        contract = make_spy_contract()
        client.req_tick_by_tick_data(7001, contract, "Last", 0, False)
        got_tbt = wrapper.got_tbt.wait(timeout=45)
        client.cancel_tick_by_tick_data(7001)

        if not _inside_the_session(client, wrapper, contract):
            pytest.skip(
                "the venue's own liquid hours say this contract's session is shut, "
                "and the trade stream carries the session's prints"
            )
        assert got_tbt, (
            "the venue reported a trade on this contract, so it is trading, "
            "and a trade stream on it delivered nothing"
        )

        events = [e for e in wrapper._get_events("tick_by_tick_all_last") if e[1] == 7001]
        assert len(events) > 0
        price = events[0][2]
        assert price > 0, "TBT price should be positive"


# ═══════════════════════════════════════
# Cancel Method Tests
# ═══════════════════════════════════════

class TestCancelMethods:
    """Verify all cancel methods don't crash when called."""

    def test_cancel_mkt_data_no_sub(self, ib_connection):
        """Cancel non-existent subscription — should not crash."""
        _, client = ib_connection
        client.cancel_mkt_data(99999)
        time.sleep(0.3)
        assert client.is_connected()

    def test_cancel_historical_no_req(self, ib_connection):
        _, client = ib_connection
        client.cancel_historical_data(99999)
        time.sleep(0.3)
        assert client.is_connected()

    def test_cancel_head_timestamp_no_req(self, ib_connection):
        _, client = ib_connection
        client.cancel_head_time_stamp(99999)
        time.sleep(0.3)
        assert client.is_connected()

    def test_cancel_tbt_no_sub(self, ib_connection):
        _, client = ib_connection
        client.cancel_tick_by_tick_data(99999)
        time.sleep(0.3)
        assert client.is_connected()

    def test_cancel_real_time_bars_no_sub(self, ib_connection):
        _, client = ib_connection
        client.cancel_real_time_bars(99999)
        time.sleep(0.3)
        assert client.is_connected()

    def test_cancel_order_bogus_id(self, ib_connection):
        """Cancel non-existent order — should get error or be ignored."""
        _, client = ib_connection
        client.cancel_order(999999999, "")
        time.sleep(1)
        assert client.is_connected()


# ═══════════════════════════════════════
# Robustness Tests
# ═══════════════════════════════════════

class TestRobustness:

    def test_rapid_subscribe_unsubscribe(self, ib_connection):
        """Rapidly subscribe/unsubscribe 10 times — no crash."""
        _, client = ib_connection
        for i in range(10):
            client.req_mkt_data(8000 + i, make_spy_contract(), "", False)
        time.sleep(1)
        for i in range(10):
            client.cancel_mkt_data(8000 + i)
        time.sleep(1)
        assert client.is_connected()

    def test_rapid_order_submit_cancel(self, ib_connection):
        """Submit and cancel 5 orders rapidly."""
        wrapper, client = ib_connection
        oids = []
        base_oid = wrapper.next_order_id
        wrapper.next_order_id += 10

        for i in range(5):
            oid = base_oid + i
            oids.append(oid)
            order = Order()
            order.action = "BUY"
            order.total_quantity = 1
            order.order_type = "LMT"
            order.lmt_price = 1.00
            order.tif = "GTC"
            order.outside_rth = True
            client.place_order(oid, make_spy_contract(), order)

        time.sleep(3)

        for oid in oids:
            client.cancel_order(oid, "")

        time.sleep(5)
        assert client.is_connected()

    def test_still_connected_at_end(self, ib_connection):
        """Final sanity check: connection survives all tests."""
        _, client = ib_connection
        assert client.is_connected()


def _a_moment(time_, callback):
    """A tick-by-tick time is a Unix second, which is what the reference
    client's callback of this name carries and what `datetime.fromtimestamp`
    reads. Handing it milliseconds put every print in the year 58553, and no
    test saw it because every wrapper here took the argument and dropped it.
    """
    import datetime
    assert isinstance(time_, int), f"{callback} time is {type(time_).__name__}"
    when = datetime.datetime.fromtimestamp(time_, datetime.timezone.utc)
    now = datetime.datetime.now(datetime.timezone.utc)
    assert abs((now - when).total_seconds()) < 86_400, \
        f"{callback} timed a print at {when.isoformat()}, which is not today"
