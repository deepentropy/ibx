"""Python ↔ Rust bridge compatibility tests (issue #70).

These tests exercise the REAL ibx Rust module — not mocks.
They use _test_* helpers to inject data into SharedState and verify
callbacks fire with correctly converted types across the PyO3 boundary.
"""

import pytest
import threading
from ibx import (
    Contract, Order, TagValue, BarData, ContractDetails, OrderState,
    EWrapper, EClient,
    TickAttrib, TickAttribLast, TickAttribBidAsk, TickTypeEnum,
    PriceCondition, TimeCondition, MarginCondition,
    ExecutionCondition, VolumeCondition, PercentChangeCondition,
    ContractDescription,
)


# ── Helpers ──

class RecordingWrapper(EWrapper):
    """Records all callback invocations for assertion."""

    def __init__(self):
        super().__init__()
        self.events = []

    def tick_price(self, req_id, tick_type, price, attrib):
        self.events.append(("tick_price", req_id, tick_type, price, attrib))

    def tick_size(self, req_id, tick_type, size):
        self.events.append(("tick_size", req_id, tick_type, size))

    def tick_string(self, req_id, tick_type, value):
        self.events.append(("tick_string", req_id, tick_type, value))

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self.events.append(("order_status", order_id, status, filled, remaining, avg_fill_price))

    def exec_details(self, req_id, contract, execution):
        self.events.append(("exec_details", req_id, contract, execution))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))

    def next_valid_id(self, order_id):
        self.events.append(("next_valid_id", order_id))

    def managed_accounts(self, accounts_list):
        self.events.append(("managed_accounts", accounts_list))

    def connect_ack(self):
        self.events.append(("connect_ack",))

    def historical_data(self, req_id, bar):
        self.events.append(("historical_data", req_id, bar))

    def historical_data_end(self, req_id, start, end):
        self.events.append(("historical_data_end", req_id))

    def head_timestamp(self, req_id, head_timestamp):
        self.events.append(("head_timestamp", req_id, head_timestamp))

    def tick_by_tick_all_last(self, req_id, tick_type, time, price, size,
                              tick_attrib_last, exchange, special_conditions):
        self.events.append(("tick_by_tick_all_last", req_id, price, size, exchange))

    def tick_by_tick_bid_ask(self, req_id, time, bid_price, ask_price,
                             bid_size, ask_size, tick_attrib_bid_ask):
        self.events.append(("tick_by_tick_bid_ask", req_id, bid_price, ask_price, bid_size, ask_size))

    def update_account_value(self, key, value, currency, account_name):
        self.events.append(("update_account_value", key, value, currency, account_name))

    def pnl(self, req_id, daily_pnl, unrealized_pnl, realized_pnl):
        self.events.append(("pnl", req_id, daily_pnl, unrealized_pnl, realized_pnl))

    def account_summary(self, req_id, account, tag, value, currency):
        self.events.append(("account_summary", req_id, account, tag, value))

    def account_summary_end(self, req_id):
        self.events.append(("account_summary_end", req_id))

    def position(self, account, contract, pos, avg_cost):
        self.events.append(("position", account, contract, pos, avg_cost))

    def position_end(self):
        self.events.append(("position_end",))


def make_test_client(account_id="TEST123"):
    """Create a connected EClient backed by SharedState (no live gateway)."""
    w = RecordingWrapper()
    c = EClient(w)
    c._test_connect(account_id)
    return w, c


# ═══════════════════════════════════════
# Type Conversion Tests
# ═══════════════════════════════════════


class TestContractConversion:
    """Verify Contract fields survive the PyO3 round-trip."""

    def test_all_fields(self):
        c = Contract(
            con_id=265598, symbol="AAPL", sec_type="STK",
            exchange="SMART", currency="USD",
            last_trade_date_or_contract_month="20260320",
            strike=200.0, right="C", multiplier="100",
            local_symbol="AAPL  260320C00200000",
            primary_exchange="NASDAQ",
            trading_class="AAPL",
        )
        assert c.con_id == 265598
        assert c.symbol == "AAPL"
        assert c.sec_type == "STK"
        assert c.exchange == "SMART"
        assert c.currency == "USD"
        assert c.last_trade_date_or_contract_month == "20260320"
        assert c.strike == 200.0
        assert c.right == "C"
        assert c.multiplier == "100"
        assert c.local_symbol == "AAPL  260320C00200000"
        assert c.primary_exchange == "NASDAQ"
        assert c.trading_class == "AAPL"

    def test_unicode_symbol(self):
        c = Contract(symbol="日経225")
        assert c.symbol == "日経225"

    def test_empty_string_fields(self):
        c = Contract()
        assert c.last_trade_date_or_contract_month == ""
        assert c.right == ""
        assert c.multiplier == ""
        assert c.local_symbol == ""

    def test_large_con_id(self):
        c = Contract(con_id=999_999_999)
        assert c.con_id == 999_999_999

    def test_negative_con_id(self):
        c = Contract(con_id=-1)
        assert c.con_id == -1

    def test_zero_strike(self):
        c = Contract(strike=0.0)
        assert c.strike == 0.0

    def test_fractional_strike(self):
        c = Contract(strike=123.456789)
        assert abs(c.strike - 123.456789) < 1e-6


class TestOrderConversion:
    """Verify Order fields survive the PyO3 round-trip."""

    def test_basic_limit_order(self):
        o = Order(action="BUY", total_quantity=100, order_type="LMT", lmt_price=150.50)
        assert o.action == "BUY"
        assert o.total_quantity == 100.0
        assert o.order_type == "LMT"
        assert o.lmt_price == 150.50

    def test_all_extended_attrs(self):
        o = Order()
        o.outside_rth = True
        o.hidden = True
        o.all_or_none = True
        o.sweep_to_fill = True
        o.display_size = 50
        o.min_qty = 10
        o.discretionary_amt = 0.05
        o.trigger_method = 1
        assert o.outside_rth is True
        assert o.hidden is True
        assert o.all_or_none is True
        assert o.sweep_to_fill is True
        assert o.display_size == 50
        assert o.min_qty == 10
        assert abs(o.discretionary_amt - 0.05) < 1e-9
        assert o.trigger_method == 1

    def test_algo_params_round_trip(self):
        o = Order()
        o.algo_strategy = "Vwap"
        o.algo_params = [
            TagValue("maxPctVol", "0.1"),
            TagValue("startTime", "09:30:00"),
            TagValue("endTime", "16:00:00"),
        ]
        assert o.algo_strategy == "Vwap"
        assert len(o.algo_params) == 3
        assert o.algo_params[0].tag == "maxPctVol"
        assert o.algo_params[0].value == "0.1"
        assert o.algo_params[2].tag == "endTime"

    def test_trailing_order(self):
        o = Order(action="SELL", total_quantity=50, order_type="TRAIL")
        o.trailing_percent = 1.5
        assert o.trailing_percent == 1.5

    def test_what_if_flag(self):
        o = Order(what_if=True)
        assert o.what_if is True

    def test_tif_variants(self):
        for tif in ["DAY", "GTC", "IOC", "GTD"]:
            o = Order(tif=tif)
            assert o.tif == tif

    def test_condition_types(self):
        """Verify all condition types are constructible via PyO3."""
        pc = PriceCondition(con_id=265598, price=200.0, is_more=True, trigger_method=1)
        assert pc.price == 200.0

        tc = TimeCondition(time="20260313-09:30:00", is_more=True)
        assert tc.time == "20260313-09:30:00"

        mc = MarginCondition(percent=30, is_more=False)
        assert mc.percent == 30

        vc = VolumeCondition(con_id=265598, volume=1_000_000, is_more=True)
        assert vc.volume == 1_000_000

        pcc = PercentChangeCondition(con_id=265598, change_percent=5.0, is_more=True)
        assert pcc.change_percent == 5.0

        ec = ExecutionCondition(symbol="AAPL", exchange="SMART", sec_type="STK")
        assert ec.symbol == "AAPL"

    def test_large_quantity(self):
        o = Order(total_quantity=1_000_000.0)
        assert o.total_quantity == 1_000_000.0

    def test_negative_price(self):
        """Negative prices (e.g. for spreads) should round-trip."""
        o = Order(lmt_price=-2.50)
        assert o.lmt_price == -2.50

    def test_zero_price(self):
        o = Order(lmt_price=0.0)
        assert o.lmt_price == 0.0


class TestBarDataConversion:

    def test_all_fields(self):
        b = BarData(date="20260313", open=150.0, high=155.0, low=149.0,
                    close=153.0, volume=1_000_000, wap=152.5, bar_count=500)
        assert b.date == "20260313"
        assert b.open == 150.0
        assert b.high == 155.0
        assert b.low == 149.0
        assert b.close == 153.0
        assert b.volume == 1_000_000
        assert b.wap == 152.5
        assert b.bar_count == 500


class TestContractDetailsConversion:

    def test_nested_contract(self):
        """ContractDetails.contract getter returns a copy (PyO3 pattern).
        Set fields via the Contract constructor, then assign to cd.contract."""
        c = Contract(con_id=265598, symbol="AAPL")
        cd = ContractDetails()
        cd.contract = c
        cd.long_name = "Apple Inc."
        assert cd.contract.con_id == 265598
        assert cd.contract.symbol == "AAPL"
        assert cd.long_name == "Apple Inc."


class TestOrderStateConversion:

    def test_all_fields(self):
        os_ = OrderState()
        os_.status = "Filled"
        os_.commission = 1.50
        os_.init_margin_before = "1000.00"
        assert os_.status == "Filled"
        assert os_.commission == 1.50


class TestTickAttribConversion:

    def test_tick_attrib_flags(self):
        ta = TickAttrib()
        ta.can_auto_execute = True
        ta.past_limit = True
        ta.pre_open = True
        assert ta.can_auto_execute is True
        assert ta.past_limit is True
        assert ta.pre_open is True

    def test_tick_attrib_last_flags(self):
        ta = TickAttribLast()
        ta.past_limit = True
        ta.unreported = True
        assert ta.past_limit is True
        assert ta.unreported is True

    def test_tick_attrib_bid_ask_flags(self):
        ta = TickAttribBidAsk()
        ta.bid_past_low = True
        ta.ask_past_high = True
        assert ta.bid_past_low is True
        assert ta.ask_past_high is True


# ═══════════════════════════════════════
# Callback Dispatch Tests
# ═══════════════════════════════════════


class TestQuoteDispatch:
    """Verify quote data flows from Rust SharedState → Python wrapper callbacks."""

    def test_bid_ask_last_dispatch(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=150.25, ask=150.50, last=150.30)
        c._test_dispatch_once()

        prices = [(e[2], e[3]) for e in w.events if e[0] == "tick_price"]
        assert (TickTypeEnum.BID, 150.25) in prices
        assert (TickTypeEnum.ASK, 150.50) in prices
        assert (TickTypeEnum.LAST, 150.30) in prices

    def test_size_dispatch(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=100.0, ask=101.0, bid_size=500, ask_size=300, last_size=100)
        c._test_dispatch_once()

        sizes = [(e[2], e[3]) for e in w.events if e[0] == "tick_size"]
        assert (TickTypeEnum.BID_SIZE, 500.0) in sizes
        assert (TickTypeEnum.ASK_SIZE, 300.0) in sizes
        assert (TickTypeEnum.LAST_SIZE, 100.0) in sizes

    def test_ohlcv_dispatch(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, open=148.0, high=155.0, low=147.5, close=152.0, volume=2_000_000)
        c._test_dispatch_once()

        prices = {e[2]: e[3] for e in w.events if e[0] == "tick_price"}
        sizes = {e[2]: e[3] for e in w.events if e[0] == "tick_size"}
        assert prices[TickTypeEnum.HIGH] == 155.0
        assert prices[TickTypeEnum.LOW] == 147.5
        assert prices[TickTypeEnum.CLOSE] == 152.0
        assert prices[TickTypeEnum.OPEN] == 148.0
        assert sizes[TickTypeEnum.VOLUME] == 2_000_000.0

    def test_attrib_is_tick_attrib(self):
        """Verify the attrib object passed to tick_price is a TickAttrib."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=100.0)
        c._test_dispatch_once()

        tick_events = [e for e in w.events if e[0] == "tick_price"]
        assert len(tick_events) > 0
        attrib = tick_events[0][4]
        assert isinstance(attrib, TickAttrib)

    def test_change_detection(self):
        """Second dispatch with same quote should NOT fire duplicate callbacks."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=100.0, ask=101.0)
        c._test_dispatch_once()
        count1 = len(w.events)

        # Same quote again
        c._test_dispatch_once()
        count2 = len(w.events)

        # No new events on NLV=0 account
        assert count2 == count1

    def test_price_update_fires_new_events(self):
        """Quote update with different price should fire new tick_price."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        c._test_push_quote(0, bid=100.0)
        c._test_dispatch_once()
        count1 = len([e for e in w.events if e[0] == "tick_price"])

        c._test_push_quote(0, bid=101.0)
        c._test_dispatch_once()
        count2 = len([e for e in w.events if e[0] == "tick_price"])

        assert count2 > count1

    def test_multi_instrument_independent(self):
        """Two instruments should fire independent tick callbacks."""
        w, c = make_test_client()
        c._test_set_instrument_count(2)
        c._test_map_instrument(1, 0)  # req_id=1 → instrument 0
        c._test_map_instrument(2, 1)  # req_id=2 → instrument 1
        c._test_push_quote(0, bid=100.0)
        c._test_push_quote(1, bid=200.0)
        c._test_dispatch_once()

        bid_events = [(e[1], e[3]) for e in w.events
                      if e[0] == "tick_price" and e[2] == TickTypeEnum.BID]
        assert (1, 100.0) in bid_events
        assert (2, 200.0) in bid_events


class TestFillDispatch:
    """Verify fills flow from Rust → Python order_status + exec_details."""

    def test_full_fill(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_fill(0, order_id=42, side="BUY", price=150.50, qty=100, remaining=0)
        c._test_dispatch_once()

        status_events = [e for e in w.events if e[0] == "order_status"]
        assert len(status_events) >= 1
        e = status_events[0]
        assert e[1] == 42        # order_id
        assert e[2] == "Filled"  # status
        assert e[3] == 100.0     # filled
        assert e[4] == 0.0       # remaining
        assert abs(e[5] - 150.50) < 0.01  # avg_fill_price

    def test_partial_fill(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_fill(0, order_id=42, side="BUY", price=150.0, qty=50, remaining=50)
        c._test_dispatch_once()

        status_events = [e for e in w.events if e[0] == "order_status"]
        assert status_events[0][2] == "PartiallyFilled"
        assert status_events[0][3] == 50.0   # filled
        assert status_events[0][4] == 50.0   # remaining

    def test_sell_side(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_fill(0, order_id=10, side="SELL", price=200.0, qty=25, remaining=0)
        c._test_dispatch_once()

        exec_events = [e for e in w.events if e[0] == "exec_details"]
        assert len(exec_events) == 1
        execution = exec_events[0][3]
        assert execution["side"] == "SELL"
        assert abs(execution["price"] - 200.0) < 0.01

    def test_short_sell(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_fill(0, order_id=10, side="SSHORT", price=100.0, qty=10, remaining=0)
        c._test_dispatch_once()

        exec_events = [e for e in w.events if e[0] == "exec_details"]
        assert exec_events[0][3]["side"] == "SSHORT"

    def test_exec_details_has_required_keys(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_fill(0, order_id=1, side="BUY", price=100.0, qty=10, remaining=0)
        c._test_dispatch_once()

        exec_events = [e for e in w.events if e[0] == "exec_details"]
        execution = exec_events[0][3]
        assert "execId" in execution
        assert "side" in execution
        assert "price" in execution
        assert "shares" in execution
        assert "time" in execution


class TestOrderUpdateDispatch:

    def test_submitted(self):
        w, c = make_test_client()
        c._test_push_order_update(42, 0, "Submitted", 0, 100)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "order_status"]
        assert events[0][1] == 42
        assert events[0][2] == "Submitted"

    def test_cancelled(self):
        w, c = make_test_client()
        c._test_push_order_update(42, 0, "Cancelled", 50, 50)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "order_status"]
        assert events[0][2] == "Cancelled"

    def test_rejected(self):
        w, c = make_test_client()
        c._test_push_order_update(42, 0, "Rejected", 0, 100)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "order_status"]
        assert events[0][2] == "Inactive"  # Rejected maps to Inactive


class TestCancelRejectDispatch:

    def test_cancel_reject_fires_error(self):
        w, c = make_test_client()
        c._test_push_cancel_reject(42, 0, 1)  # reason 1 = unknown order
        c._test_dispatch_once()

        errors = [e for e in w.events if e[0] == "error"]
        assert len(errors) == 1
        assert errors[0][1] == 42  # order_id
        assert errors[0][2] == 202  # error_code for cancel reject


class TestTbtDispatch:

    def test_tbt_trade(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_tbt_trade(0, price=152.75, size=50, exchange="ARCA")
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "tick_by_tick_all_last"]
        assert len(events) == 1
        assert abs(events[0][2] - 152.75) < 0.01  # price
        assert events[0][3] == 50.0                # size
        assert events[0][4] == "ARCA"              # exchange

    def test_tbt_quote(self):
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_tbt_quote(0, bid=150.0, ask=150.50, bid_size=100, ask_size=200)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "tick_by_tick_bid_ask"]
        assert len(events) == 1
        assert abs(events[0][2] - 150.0) < 0.01   # bid
        assert abs(events[0][3] - 150.50) < 0.01  # ask
        assert events[0][4] == 100.0               # bid_size
        assert events[0][5] == 200.0               # ask_size


class TestHistoricalDispatch:

    def test_bars_and_end(self):
        w, c = make_test_client()
        bars = [
            ("20260310", 150.0, 155.0, 149.0, 153.0, 1000000),
            ("20260311", 153.0, 158.0, 152.0, 157.0, 1200000),
        ]
        c._test_push_historical_data(1, bars, True)
        c._test_dispatch_once()

        bar_events = [e for e in w.events if e[0] == "historical_data"]
        end_events = [e for e in w.events if e[0] == "historical_data_end"]
        assert len(bar_events) == 2
        assert len(end_events) == 1

        # Verify BarData object fields
        bar = bar_events[0][2]
        assert isinstance(bar, BarData)
        assert bar.date == "20260310"
        assert bar.open == 150.0
        assert bar.high == 155.0
        assert bar.close == 153.0

    def test_incomplete_data_no_end(self):
        w, c = make_test_client()
        bars = [("20260310", 150.0, 155.0, 149.0, 153.0, 1000000)]
        c._test_push_historical_data(1, bars, False)
        c._test_dispatch_once()

        end_events = [e for e in w.events if e[0] == "historical_data_end"]
        assert len(end_events) == 0


class TestHeadTimestampDispatch:

    def test_head_timestamp(self):
        w, c = make_test_client()
        c._test_push_head_timestamp(1, "20200101-00:00:00")
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "head_timestamp"]
        assert len(events) == 1
        assert events[0][1] == 1
        assert events[0][2] == "20200101-00:00:00"


class TestAccountDispatch:

    def test_account_update_value(self):
        w, c = make_test_client("DU12345")
        c._test_set_account(net_liquidation=100000.0)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "update_account_value"]
        assert len(events) >= 1
        nlv_event = [e for e in events if e[1] == "NetLiquidation"][0]
        assert nlv_event[2] == "100000.00"
        assert nlv_event[3] == "USD"
        assert nlv_event[4] == "DU12345"

    def test_pnl_dispatch(self):
        w, c = make_test_client()
        c.req_pnl(1, "TEST123")
        c._test_set_account(daily_pnl=500.0, unrealized_pnl=300.0, realized_pnl=200.0)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "pnl"]
        assert len(events) == 1
        assert events[0][1] == 1      # req_id
        assert abs(events[0][2] - 500.0) < 0.01   # daily
        assert abs(events[0][3] - 300.0) < 0.01   # unrealized
        assert abs(events[0][4] - 200.0) < 0.01   # realized

    def test_pnl_change_detection(self):
        """Same P&L should not fire duplicate callback."""
        w, c = make_test_client()
        c.req_pnl(1, "TEST123")
        c._test_set_account(daily_pnl=100.0)
        c._test_dispatch_once()
        count1 = len([e for e in w.events if e[0] == "pnl"])

        c._test_dispatch_once()
        count2 = len([e for e in w.events if e[0] == "pnl"])
        assert count2 == count1  # no change = no new callback

    def test_account_summary(self):
        w, c = make_test_client()
        c.req_account_summary(1, "All", "NetLiquidation,BuyingPower")
        c._test_set_account(net_liquidation=100000.0, buying_power=200000.0)
        c._test_dispatch_once()

        events = [e for e in w.events if e[0] == "account_summary"]
        tags = {e[3]: e[4] for e in events}
        assert "NetLiquidation" in tags
        assert "BuyingPower" in tags
        assert tags["NetLiquidation"] == "100000.00"

        end_events = [e for e in w.events if e[0] == "account_summary_end"]
        assert len(end_events) == 1

    def test_positions(self):
        w, c = make_test_client("DU12345")
        c._test_set_position(265598, 100, 150.50)
        c.req_positions()

        pos_events = [e for e in w.events if e[0] == "position"]
        end_events = [e for e in w.events if e[0] == "position_end"]
        assert len(pos_events) == 1
        assert pos_events[0][1] == "DU12345"
        assert pos_events[0][3] == 100.0
        assert abs(pos_events[0][4] - 150.50) < 0.01
        assert len(end_events) == 1


# ═══════════════════════════════════════
# Error & Edge Cases
# ═══════════════════════════════════════


class TestNotConnected:

    def test_req_mkt_data_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c.req_mkt_data(1, Contract(con_id=265598, symbol="AAPL"), "")

    def test_place_order_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c.place_order(1, Contract(), Order(action="BUY", total_quantity=100, order_type="MKT"))

    def test_cancel_order_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c.cancel_order(1)

    def test_historical_data_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c.req_historical_data(1, Contract(), "", "1 D", "1 hour", "TRADES", 1)

    def test_dispatch_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c._test_dispatch_once()

    def test_run_not_connected(self):
        w = EWrapper()
        c = EClient(w)
        with pytest.raises(RuntimeError, match="Not connected"):
            c.run()


class TestCallbackException:
    """Verify that a Python exception in a callback doesn't crash Rust."""

    def test_exception_in_tick_price(self):
        class BadWrapper(EWrapper):
            def tick_price(self, req_id, tick_type, price, attrib):
                raise ValueError("boom!")

        w = BadWrapper()
        c = EClient(w)
        c._test_connect()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=100.0)

        with pytest.raises(Exception):
            c._test_dispatch_once()

        # Client should still be alive
        assert c.is_connected() is True

    def test_exception_in_order_status(self):
        class BadWrapper(EWrapper):
            def order_status(self, *args):
                raise RuntimeError("explode!")

        w = BadWrapper()
        c = EClient(w)
        c._test_connect()
        c._test_push_fill(0, order_id=1, side="BUY", price=100.0, qty=10, remaining=0)

        with pytest.raises(Exception):
            c._test_dispatch_once()

        assert c.is_connected() is True


class TestEdgeCases:

    def test_dispatch_empty_queues(self):
        """Dispatch with no data should not crash or fire callbacks."""
        w, c = make_test_client()
        c._test_dispatch_once()
        # Only account-related events if NLV > 0 (it's 0 here)
        tick_events = [e for e in w.events if e[0] in ("tick_price", "tick_size", "order_status")]
        assert len(tick_events) == 0

    def test_double_test_connect(self):
        w = EWrapper()
        c = EClient(w)
        c._test_connect()
        with pytest.raises(RuntimeError, match="Already connected"):
            c._test_connect()

    def test_req_ids_without_connection(self):
        """req_ids fires next_valid_id even without connect."""
        w = RecordingWrapper()
        c = EClient(w)
        c.req_ids()
        events = [e for e in w.events if e[0] == "next_valid_id"]
        assert len(events) == 1

    def test_disconnect_idempotent(self):
        w, c = make_test_client()
        c.disconnect()
        c.disconnect()
        assert c.is_connected() is False


# ═══════════════════════════════════════
# Multi-step Scenario Tests
# ═══════════════════════════════════════


class TestScenarios:

    def test_subscribe_ticks_unsubscribe(self):
        """Full lifecycle: connect → map instrument → inject ticks → verify → disconnect."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        # Inject tick
        c._test_push_quote(0, bid=150.0, ask=151.0, last=150.50)
        c._test_dispatch_once()

        # Verify
        bids = [e for e in w.events if e[0] == "tick_price" and e[2] == TickTypeEnum.BID]
        assert len(bids) == 1
        assert bids[0][3] == 150.0

        c.disconnect()
        assert c.is_connected() is False

    def test_order_place_fill_verify(self):
        """Place order → receive fill → verify exec_details + order_status."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        # Simulate order submitted, then filled
        c._test_push_order_update(100, 0, "Submitted", 0, 50)
        c._test_dispatch_once()

        c._test_push_fill(0, order_id=100, side="BUY", price=150.0, qty=50, remaining=0)
        c._test_dispatch_once()

        # Verify sequence
        statuses = [e for e in w.events if e[0] == "order_status"]
        assert statuses[0][2] == "Submitted"
        assert statuses[1][2] == "Filled"

        execs = [e for e in w.events if e[0] == "exec_details"]
        assert len(execs) == 1
        assert execs[0][3]["side"] == "BUY"

    def test_partial_fill_then_cancel(self):
        """Partial fill → cancel → verify statuses."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        c._test_push_fill(0, order_id=200, side="BUY", price=100.0, qty=30, remaining=70)
        c._test_dispatch_once()

        c._test_push_order_update(200, 0, "Cancelled", 30, 70)
        c._test_dispatch_once()

        statuses = [e for e in w.events if e[0] == "order_status"]
        assert statuses[0][2] == "PartiallyFilled"
        assert statuses[1][2] == "Cancelled"

    def test_ticks_during_fills(self):
        """Ticks and fills can be dispatched in the same cycle."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        c._test_push_quote(0, bid=100.0, ask=101.0)
        c._test_push_fill(0, order_id=1, side="BUY", price=100.5, qty=10, remaining=0)
        c._test_dispatch_once()

        ticks = [e for e in w.events if e[0] == "tick_price"]
        orders = [e for e in w.events if e[0] == "order_status"]
        assert len(ticks) >= 2   # bid + ask at minimum
        assert len(orders) >= 1  # fill

    def test_historical_then_ticks(self):
        """Request historical data, then switch to live ticks."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        bars = [("20260310", 150.0, 155.0, 149.0, 153.0, 1000000)]
        c._test_push_historical_data(1, bars, True)
        c._test_dispatch_once()

        # Now live ticks
        c._test_push_quote(0, bid=154.0, ask=155.0)
        c._test_dispatch_once()

        hist = [e for e in w.events if e[0] == "historical_data"]
        ticks = [e for e in w.events if e[0] == "tick_price"]
        assert len(hist) == 1
        assert len(ticks) >= 2

    def test_cancel_reject_during_fills(self):
        """Cancel reject and fill in same dispatch cycle."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        c._test_push_fill(0, order_id=1, side="BUY", price=100.0, qty=10, remaining=0)
        c._test_push_cancel_reject(2, 0, 0)  # cancel rejected for different order
        c._test_dispatch_once()

        fills = [e for e in w.events if e[0] == "order_status"]
        errors = [e for e in w.events if e[0] == "error"]
        assert len(fills) >= 1
        assert len(errors) == 1

    def test_multi_instrument_fills(self):
        """Fills on different instruments dispatch independently."""
        w, c = make_test_client()
        c._test_set_instrument_count(2)
        c._test_map_instrument(1, 0)
        c._test_map_instrument(2, 1)

        c._test_push_fill(0, order_id=10, side="BUY", price=100.0, qty=50, remaining=0)
        c._test_push_fill(1, order_id=20, side="SELL", price=200.0, qty=25, remaining=0)
        c._test_dispatch_once()

        fills = [e for e in w.events if e[0] == "order_status"]
        assert len(fills) == 2
        order_ids = {e[1] for e in fills}
        assert order_ids == {10, 20}


# ═══════════════════════════════════════
# Thread Safety Tests
# ═══════════════════════════════════════


class TestThreadSafety:

    def test_concurrent_dispatch_and_push(self):
        """Push data from one thread while dispatching from another."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        errors = []

        def pusher():
            try:
                for i in range(100):
                    c._test_push_quote(0, bid=100.0 + i, ask=101.0 + i)
            except Exception as e:
                errors.append(e)

        t = threading.Thread(target=pusher)
        t.start()
        for _ in range(50):
            try:
                c._test_dispatch_once()
            except Exception:
                pass  # Python callbacks may race, that's OK
        t.join()
        assert len(errors) == 0

    def test_disconnect_during_dispatch(self):
        """Disconnect while dispatch is running should not crash."""
        w, c = make_test_client()
        c._test_set_instrument_count(1)
        c._test_map_instrument(1, 0)

        c._test_push_quote(0, bid=100.0, ask=101.0)

        def disconnecter():
            import time
            time.sleep(0.001)
            c.disconnect()

        t = threading.Thread(target=disconnecter)
        t.start()
        # Dispatch — may or may not see the quote depending on timing
        try:
            c._test_dispatch_once()
        except RuntimeError:
            pass  # Expected if disconnected mid-dispatch
        t.join()
        assert c.is_connected() is False

    def test_concurrent_req_ids(self):
        """Multiple threads calling req_ids should not crash."""
        w, c = make_test_client()
        errors = []

        def caller():
            try:
                for _ in range(100):
                    c.req_ids()
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=caller) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        assert len(errors) == 0
