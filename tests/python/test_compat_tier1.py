"""Tests for Tier 1 ibapi-compatible API gap closures.

Covers: req_market_data_type, req_mkt_depth, cancel_mkt_depth, req_open_orders,
req_all_open_orders, req_executions, req_historical_ticks, req_real_time_bars,
cancel_real_time_bars, cancel_head_time_stamp, req_sec_def_opt_params,
req_matching_symbols, req_current_time, and new EWrapper callbacks.
"""

import time
import pytest
from ibx import (
    Contract, BarData, ContractDetails, ContractDescription,
    EWrapper, EClient, TickAttrib, TickAttribLast, TickAttribBidAsk,
)


# ── EWrapper with new Tier 1 callbacks ──

class Tier1Wrapper(EWrapper):
    """Records all Tier 1-related callbacks."""

    def __init__(self):
        super().__init__()
        self.events = []

    # Connection
    def current_time(self, time):
        self.events.append(("current_time", time))

    def next_valid_id(self, order_id):
        self.events.append(("next_valid_id", order_id))

    def managed_accounts(self, accounts_list):
        self.events.append(("managed_accounts", accounts_list))

    def connect_ack(self):
        self.events.append(("connect_ack",))

    # Orders
    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self.events.append(("order_status", order_id, status, filled, remaining))

    def open_order(self, order_id, contract, order, order_state):
        self.events.append(("open_order", order_id))

    def open_order_end(self):
        self.events.append(("open_order_end",))

    def exec_details(self, req_id, contract, execution):
        self.events.append(("exec_details", req_id, execution))

    def exec_details_end(self, req_id):
        self.events.append(("exec_details_end", req_id))

    # Real-time bars
    def real_time_bar(self, req_id, date, open_, high, low, close, volume, wap, count):
        self.events.append(("real_time_bar", req_id, date, open_, high, low, close))

    # Historical ticks
    def historical_ticks(self, req_id, ticks, done):
        self.events.append(("historical_ticks", req_id, done))

    def historical_ticks_bid_ask(self, req_id, ticks, done):
        self.events.append(("historical_ticks_bid_ask", req_id, done))

    def historical_ticks_last(self, req_id, ticks, done):
        self.events.append(("historical_ticks_last", req_id, done))

    # Options chain
    def security_definition_option_parameter(self, req_id, exchange, underlying_con_id,
                                              trading_class, multiplier, expirations, strikes):
        self.events.append(("sec_def_opt_param", req_id, exchange, underlying_con_id))

    def security_definition_option_parameter_end(self, req_id):
        self.events.append(("sec_def_opt_param_end", req_id))

    # Symbol search
    def symbol_samples(self, req_id, contract_descriptions):
        self.events.append(("symbol_samples", req_id, contract_descriptions))

    # Market depth
    def update_mkt_depth(self, req_id, position, operation, side, price, size):
        self.events.append(("update_mkt_depth", req_id, position, operation, side, price, size))

    def update_mkt_depth_l2(self, req_id, position, market_maker, operation,
                             side, price, size, is_smart_depth):
        self.events.append(("update_mkt_depth_l2", req_id, position, market_maker))

    def mkt_depth_exchanges(self, depth_mkt_data_descriptions):
        self.events.append(("mkt_depth_exchanges", depth_mkt_data_descriptions))

    # Tick option
    def tick_option_computation(self, req_id, tick_type, tick_attrib,
                                 implied_vol, delta, opt_price, pv_dividend,
                                 gamma, vega, theta, und_price):
        self.events.append(("tick_option_computation", req_id, tick_type))


# ═══════════════════════════════════════
# ContractDescription class
# ═══════════════════════════════════════

def test_contract_description_defaults():
    cd = ContractDescription()
    assert cd.con_id == 0
    assert cd.symbol == ""
    assert cd.sec_type == ""
    assert cd.currency == ""
    assert cd.primary_exchange == ""
    assert cd.derivative_sec_types == []


def test_contract_description_kwargs():
    cd = ContractDescription(
        con_id=265598, symbol="AAPL", sec_type="STK",
        currency="USD", primary_exchange="NASDAQ",
        derivative_sec_types=["OPT", "WAR"],
    )
    assert cd.con_id == 265598
    assert cd.symbol == "AAPL"
    assert cd.sec_type == "STK"
    assert cd.currency == "USD"
    assert cd.primary_exchange == "NASDAQ"
    assert cd.derivative_sec_types == ["OPT", "WAR"]


def test_contract_description_mutable():
    cd = ContractDescription()
    cd.con_id = 272093
    cd.symbol = "MSFT"
    assert cd.con_id == 272093
    assert cd.symbol == "MSFT"


def test_contract_description_repr():
    cd = ContractDescription(con_id=265598, symbol="AAPL", sec_type="STK", currency="USD")
    r = repr(cd)
    assert "265598" in r
    assert "AAPL" in r
    assert "STK" in r


# ═══════════════════════════════════════
# req_market_data_type
# ═══════════════════════════════════════

def test_req_market_data_type_live():
    """req_market_data_type(1) = live data."""
    client = EClient(EWrapper())
    client.req_market_data_type(1)  # should not raise


def test_req_market_data_type_frozen():
    """req_market_data_type(2) = frozen data."""
    client = EClient(EWrapper())
    client.req_market_data_type(2)


def test_req_market_data_type_delayed():
    """req_market_data_type(3) = delayed data."""
    client = EClient(EWrapper())
    client.req_market_data_type(3)


def test_req_market_data_type_delayed_frozen():
    """req_market_data_type(4) = delayed-frozen data."""
    client = EClient(EWrapper())
    client.req_market_data_type(4)


# ═══════════════════════════════════════
# req_mkt_depth / cancel_mkt_depth
# ═══════════════════════════════════════

def test_req_mkt_depth_signature():
    """req_mkt_depth accepts ibapi signature without raising."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    client.req_mkt_depth(1, contract, 5, False)


def test_req_mkt_depth_defaults():
    """req_mkt_depth works with minimal args."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    client.req_mkt_depth(1, contract)


def test_cancel_mkt_depth_signature():
    """cancel_mkt_depth accepts ibapi signature."""
    client = EClient(EWrapper())
    client.cancel_mkt_depth(1)


def test_cancel_mkt_depth_with_smart_depth():
    """cancel_mkt_depth accepts is_smart_depth kwarg."""
    client = EClient(EWrapper())
    client.cancel_mkt_depth(1, is_smart_depth=True)


# ═══════════════════════════════════════
# req_open_orders / req_all_open_orders
# ═══════════════════════════════════════

def test_req_open_orders_empty():
    """req_open_orders delivers open_order_end when no orders exist."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_open_orders()
    assert ("open_order_end",) in w.events


def test_req_open_orders_only_open_order_end():
    """With no prior orders, req_open_orders only fires open_order_end."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_open_orders()
    # Should only have open_order_end, no order_status
    assert len([e for e in w.events if e[0] == "order_status"]) == 0
    assert len([e for e in w.events if e[0] == "open_order_end"]) == 1


def test_req_all_open_orders_empty():
    """req_all_open_orders delivers open_order_end like req_open_orders."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_all_open_orders()
    assert ("open_order_end",) in w.events


# ═══════════════════════════════════════
# req_executions
# ═══════════════════════════════════════

def test_req_executions_empty():
    """req_executions delivers exec_details_end when no executions exist."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_executions(1)
    assert ("exec_details_end", 1) in w.events


def test_req_executions_only_end():
    """With no fills, only exec_details_end is fired."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_executions(1)
    assert len([e for e in w.events if e[0] == "exec_details"]) == 0
    assert len([e for e in w.events if e[0] == "exec_details_end"]) == 1


def test_req_executions_with_filter():
    """req_executions accepts a filter parameter."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_executions(1, None)  # filter=None is valid
    assert ("exec_details_end", 1) in w.events


# ═══════════════════════════════════════
# req_historical_ticks
# ═══════════════════════════════════════

def test_req_historical_ticks_not_connected():
    """req_historical_ticks raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_historical_ticks(1, contract, "20260311 09:30:00", "",
                                     1000, "TRADES", 1, False)


def test_req_historical_ticks_defaults_not_connected():
    """req_historical_ticks with defaults raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_historical_ticks(1, contract)


def test_req_historical_ticks_bid_ask_not_connected():
    """req_historical_ticks with BID_ASK raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_historical_ticks(1, contract, what_to_show="BID_ASK")


# ═══════════════════════════════════════
# req_real_time_bars / cancel_real_time_bars
# ═══════════════════════════════════════

def test_req_real_time_bars_not_connected():
    """req_real_time_bars raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_real_time_bars(1, contract, 5, "TRADES", 0)


def test_req_real_time_bars_defaults_not_connected():
    """req_real_time_bars with defaults raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_real_time_bars(1, contract)


def test_cancel_real_time_bars_not_connected():
    """cancel_real_time_bars raises when not connected."""
    client = EClient(EWrapper())
    with pytest.raises(Exception, match="Not connected"):
        client.cancel_real_time_bars(1)


# ═══════════════════════════════════════
# cancel_head_time_stamp
# ═══════════════════════════════════════

def test_cancel_head_time_stamp_not_connected():
    """cancel_head_time_stamp raises when not connected."""
    client = EClient(EWrapper())
    with pytest.raises(Exception, match="Not connected"):
        client.cancel_head_time_stamp(1)


# ═══════════════════════════════════════
# req_sec_def_opt_params
# ═══════════════════════════════════════

def test_req_sec_def_opt_params_signature():
    """req_sec_def_opt_params accepts ibapi signature without raising."""
    client = EClient(EWrapper())
    client.req_sec_def_opt_params(1, "AAPL", "", "STK", 265598)


def test_req_sec_def_opt_params_defaults():
    """req_sec_def_opt_params works with minimal args."""
    client = EClient(EWrapper())
    client.req_sec_def_opt_params(1, "AAPL")


# ═══════════════════════════════════════
# req_matching_symbols
# ═══════════════════════════════════════

def test_req_matching_symbols_not_connected():
    """req_matching_symbols raises when not connected."""
    client = EClient(EWrapper())
    with pytest.raises(Exception, match="Not connected"):
        client.req_matching_symbols(1, "AAPL")


# ═══════════════════════════════════════
# req_current_time
# ═══════════════════════════════════════

def test_req_current_time_fires_callback():
    """req_current_time dispatches current_time callback with unix timestamp."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_current_time()
    assert len(w.events) == 1
    assert w.events[0][0] == "current_time"
    ts = w.events[0][1]
    assert isinstance(ts, int)
    # Should be a reasonable unix timestamp (after 2020)
    assert ts > 1_577_836_800  # 2020-01-01


def test_req_current_time_reasonable_value():
    """req_current_time returns a time close to now."""
    w = Tier1Wrapper()
    client = EClient(w)
    before = int(time.time())
    client.req_current_time()
    after = int(time.time())
    ts = w.events[0][1]
    assert before <= ts <= after


def test_req_current_time_multiple_calls():
    """Multiple req_current_time calls each fire the callback."""
    w = Tier1Wrapper()
    client = EClient(w)
    client.req_current_time()
    client.req_current_time()
    client.req_current_time()
    assert len(w.events) == 3
    assert all(e[0] == "current_time" for e in w.events)


# ═══════════════════════════════════════
# EWrapper base class no-ops for new callbacks
# ═══════════════════════════════════════

def test_ewrapper_base_real_time_bar_noop():
    """Base EWrapper real_time_bar is a no-op."""
    w = EWrapper()
    w.real_time_bar(1, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0)


def test_ewrapper_base_historical_ticks_noop():
    """Base EWrapper historical ticks methods are no-ops."""
    w = EWrapper()
    w.historical_ticks(1, None, True)
    w.historical_ticks_bid_ask(1, None, True)
    w.historical_ticks_last(1, None, True)


def test_ewrapper_base_sec_def_opt_params_noop():
    """Base EWrapper option chain methods are no-ops."""
    w = EWrapper()
    w.security_definition_option_parameter(1, "SMART", 265598, "AAPL", "100", None, None)
    w.security_definition_option_parameter_end(1)


def test_ewrapper_base_mkt_depth_exchanges_noop():
    """Base EWrapper mkt_depth_exchanges is a no-op."""
    w = EWrapper()
    w.mkt_depth_exchanges(None)


# ═══════════════════════════════════════
# EWrapper subclassing for new callbacks
# ═══════════════════════════════════════

def test_subclass_real_time_bar():
    """Subclass can override real_time_bar."""
    w = Tier1Wrapper()
    w.real_time_bar(1, 1741785600, 150.0, 155.0, 149.0, 153.0, 1000000.0, 152.0, 5000)
    assert w.events[0] == ("real_time_bar", 1, 1741785600, 150.0, 155.0, 149.0, 153.0)


def test_subclass_historical_ticks():
    """Subclass can override historical_ticks."""
    w = Tier1Wrapper()
    w.historical_ticks(1, [], True)
    assert w.events == [("historical_ticks", 1, True)]


def test_subclass_historical_ticks_bid_ask():
    w = Tier1Wrapper()
    w.historical_ticks_bid_ask(1, [], False)
    assert w.events == [("historical_ticks_bid_ask", 1, False)]


def test_subclass_historical_ticks_last():
    w = Tier1Wrapper()
    w.historical_ticks_last(1, [], True)
    assert w.events == [("historical_ticks_last", 1, True)]


def test_subclass_sec_def_opt_param():
    """Subclass can override security_definition_option_parameter."""
    w = Tier1Wrapper()
    w.security_definition_option_parameter(1, "SMART", 265598, "AAPL", "100", [], [])
    assert w.events[0] == ("sec_def_opt_param", 1, "SMART", 265598)


def test_subclass_sec_def_opt_param_end():
    w = Tier1Wrapper()
    w.security_definition_option_parameter_end(1)
    assert w.events == [("sec_def_opt_param_end", 1)]


def test_subclass_mkt_depth_exchanges():
    w = Tier1Wrapper()
    w.mkt_depth_exchanges([])
    assert w.events == [("mkt_depth_exchanges", [])]


def test_subclass_update_mkt_depth():
    """Subclass can override update_mkt_depth."""
    w = Tier1Wrapper()
    w.update_mkt_depth(1, 0, 0, 1, 150.25, 300.0)
    assert w.events == [("update_mkt_depth", 1, 0, 0, 1, 150.25, 300.0)]


def test_subclass_update_mkt_depth_l2():
    """Subclass can override update_mkt_depth_l2."""
    w = Tier1Wrapper()
    w.update_mkt_depth_l2(1, 0, "ARCA", 0, 1, 150.25, 300.0, True)
    assert w.events == [("update_mkt_depth_l2", 1, 0, "ARCA")]


# ═══════════════════════════════════════
# Full sequences
# ═══════════════════════════════════════

def test_full_real_time_bar_sequence():
    """Simulate a stream of real-time 5-second bars."""
    w = Tier1Wrapper()
    for i in range(5):
        w.real_time_bar(1, 1741785600 + i * 5,
                        150.0 + i, 151.0 + i, 149.0 + i, 150.5 + i,
                        1000.0 * (i + 1), 150.25 + i, 100 * (i + 1))
    assert len(w.events) == 5
    assert all(e[0] == "real_time_bar" for e in w.events)
    # Verify timestamps are 5 seconds apart
    assert w.events[1][2] - w.events[0][2] == 5


def test_full_historical_ticks_sequence():
    """Simulate historical ticks request/response."""
    w = Tier1Wrapper()
    w.historical_ticks(1, [{"time": 1741785600, "price": 150.25, "size": 100}], False)
    w.historical_ticks(1, [{"time": 1741785601, "price": 150.30, "size": 200}], True)
    assert len(w.events) == 2
    assert w.events[0][2] is False  # not done
    assert w.events[1][2] is True   # done


def test_full_option_chain_sequence():
    """Simulate option chain parameters response."""
    w = Tier1Wrapper()
    expirations = ["20260320", "20260417", "20260515"]
    strikes = [140.0, 145.0, 150.0, 155.0, 160.0]
    w.security_definition_option_parameter(1, "SMART", 265598, "AAPL", "100",
                                            expirations, strikes)
    w.security_definition_option_parameter_end(1)
    assert len(w.events) == 2
    assert w.events[0][0] == "sec_def_opt_param"
    assert w.events[1] == ("sec_def_opt_param_end", 1)


def test_full_depth_sequence():
    """Simulate a market depth update stream."""
    w = Tier1Wrapper()
    # Insert 3 bids and 3 asks
    for i in range(3):
        w.update_mkt_depth(1, i, 0, 1, 150.00 - i * 0.01, (300 - i * 50))
        w.update_mkt_depth(1, i, 0, 0, 150.01 + i * 0.01, (200 - i * 30))
    assert len(w.events) == 6
    # All are update_mkt_depth
    assert all(e[0] == "update_mkt_depth" for e in w.events)


def test_full_ibapi_app_pattern_with_tier1():
    """Test the standard ibapi App pattern with all Tier 1 methods."""

    class App(EWrapper):
        def __init__(self):
            super().__init__()
            self.events = []
            self.client = EClient(self)

        def current_time(self, t):
            self.events.append(("current_time", t))

        def open_order_end(self):
            self.events.append(("open_order_end",))

        def exec_details_end(self, req_id):
            self.events.append(("exec_details_end", req_id))

        def symbol_samples(self, req_id, descriptions):
            self.events.append(("symbol_samples", req_id))

    app = App()
    assert app.client.is_connected() is False

    # These should all work without connection
    app.client.req_market_data_type(3)
    app.client.req_current_time()
    app.client.req_open_orders()
    app.client.req_executions(1)

    assert len(app.events) == 3
    assert app.events[0][0] == "current_time"
    assert app.events[1] == ("open_order_end",)
    assert app.events[2] == ("exec_details_end", 1)


# ═══════════════════════════════════════
# Signature compatibility with ibapi patterns
# ═══════════════════════════════════════

def test_mkt_depth_options_list():
    """req_mkt_depth accepts mkt_depth_options list like ibapi."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    client.req_mkt_depth(1, contract, 10, True, [])


def test_real_time_bars_options_list():
    """req_real_time_bars with options list raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_real_time_bars(1, contract, 5, "TRADES", 1, [])


def test_historical_ticks_misc_options():
    """req_historical_ticks with misc_options raises when not connected."""
    client = EClient(EWrapper())
    contract = Contract(con_id=265598, symbol="AAPL")
    with pytest.raises(Exception, match="Not connected"):
        client.req_historical_ticks(1, contract, "20260311 09:30:00", "",
                                     1000, "TRADES", 1, False, [])
