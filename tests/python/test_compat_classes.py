"""Tests for ibapi-compatible class construction, fields, and subclassing."""

import pytest
from ibx import (
    Contract, Order, BarData, ContractDetails, TagValue, OrderState,
    EWrapper, EClient,
    TickAttrib, TickAttribLast, TickAttribBidAsk, TickTypeEnum,
    PriceCondition, TimeCondition, MarginCondition,
    ExecutionCondition, VolumeCondition, PercentChangeCondition,
)


# ── Contract ──

def test_contract_defaults():
    """A fresh contract states nothing, as the reference client's does.

    A stock on SMART in dollars used to stand in these three, and a caller who
    named an id and nothing else had them sent as though they had stated them:
    a future described as a stock, a contract listed abroad asked for in
    dollars on a US venue.
    """
    c = Contract()
    assert c.con_id == 0
    assert c.symbol == ""
    assert c.sec_type == ""
    assert c.exchange == ""
    assert c.currency == ""
    assert c.strike == 0.0


def test_contract_kwargs():
    c = Contract(con_id=265598, symbol="AAPL", sec_type="STK", exchange="SMART", currency="USD")
    assert c.con_id == 265598
    assert c.symbol == "AAPL"


def test_contract_mutable():
    c = Contract()
    c.symbol = "MSFT"
    c.con_id = 272093
    assert c.symbol == "MSFT"
    assert c.con_id == 272093


def test_contract_repr():
    c = Contract(con_id=265598, symbol="AAPL")
    r = repr(c)
    assert "265598" in r
    assert "AAPL" in r


# ── Order ──

def test_order_defaults():
    o = Order()
    assert o.order_id == 0
    assert o.action == ""
    assert o.total_quantity == 0.0
    assert o.order_type == ""
    assert o.tif == ""
    assert o.transmit is True
    assert o.what_if is False


def test_order_kwargs():
    o = Order(action="BUY", total_quantity=100, order_type="LMT", lmt_price=150.50)
    assert o.action == "BUY"
    assert o.total_quantity == 100.0
    assert o.order_type == "LMT"
    assert o.lmt_price == 150.50


def test_order_aux_price_kwarg():
    o = Order(order_id=1, action="SELL", total_quantity=10, order_type="STP", aux_price=150.0)
    assert o.aux_price == 150.0
    assert "auxPrice=150" in repr(o)


def test_order_camel_case_aliases():
    o = Order()
    o.auxPrice = 150.0
    assert o.aux_price == 150.0
    assert o.auxPrice == 150.0
    o.lmtPrice = 200.0
    assert o.lmt_price == 200.0
    assert o.lmtPrice == 200.0
    o.orderId = 42
    assert o.order_id == 42
    o.totalQuantity = 100.0
    assert o.total_quantity == 100.0
    o.orderType = "STP"
    assert o.order_type == "STP"


def test_order_algo_params():
    o = Order()
    o.algo_strategy = "Vwap"
    o.algo_params = [TagValue("maxPctVol", "0.1"), TagValue("startTime", "09:30:00")]
    assert len(o.algo_params) == 2
    assert o.algo_params[0].tag == "maxPctVol"


# ── BarData ──

def test_bardata_defaults():
    b = BarData()
    assert b.date == ""
    assert b.open == 0.0
    assert b.volume == 0


def test_bardata_kwargs():
    b = BarData(date="20260311", open=150.0, high=155.0, low=149.0, close=153.0, volume=1_000_000)
    assert b.date == "20260311"
    assert b.high == 155.0
    assert b.volume == 1_000_000


# ── ContractDetails ──

def test_contract_details_defaults():
    cd = ContractDetails()
    assert cd.min_tick == 0.0
    assert cd.long_name == ""
    assert cd.contract.con_id == 0


def test_contract_details_contract_is_mutable_in_place():
    # A getter handing back a clone makes this mutation a silent no-op, and
    # `cd.contract is cd.contract` False.
    cd = ContractDetails()
    cd.contract.con_id = 265598
    assert cd.contract.con_id == 265598
    assert cd.contract is cd.contract

    c = Contract()
    c.con_id = 999
    cd.contract = c
    assert cd.contract is c
    cd.contract.symbol = "AAPL"
    assert c.symbol == "AAPL"


# ── OrderState ──

def test_order_state_defaults():
    os = OrderState()
    assert os.status == ""
    # An order the venue has not priced is not one that costs nothing. The
    # commission figures carry the same marker the margin figures beside them
    # do, which is what every unstated number on this surface reads as.
    assert os.commission_and_fees == float("1.7976931348623157e+308")
    assert os.min_commission_and_fees == float("1.7976931348623157e+308")
    assert os.max_commission_and_fees == float("1.7976931348623157e+308")


# ── TagValue ──

def test_tagvalue():
    tv = TagValue("key", "value")
    assert tv.tag == "key"
    assert tv.value == "value"


# ── TickAttrib classes ──

def test_tick_attrib():
    ta = TickAttrib()
    assert ta.can_auto_execute is False
    assert ta.past_limit is False
    assert ta.pre_open is False


def test_tick_attrib_last():
    ta = TickAttribLast()
    assert ta.past_limit is False
    assert ta.unreported is False


def test_tick_attrib_bid_ask():
    ta = TickAttribBidAsk()
    assert ta.bid_past_low is False
    assert ta.ask_past_high is False


# ── TickTypeEnum ──

def test_tick_type_constants():
    assert TickTypeEnum.BID_SIZE == 0
    assert TickTypeEnum.BID == 1
    assert TickTypeEnum.ASK == 2
    assert TickTypeEnum.ASK_SIZE == 3
    assert TickTypeEnum.LAST == 4
    assert TickTypeEnum.LAST_SIZE == 5
    assert TickTypeEnum.HIGH == 6
    assert TickTypeEnum.LOW == 7
    assert TickTypeEnum.VOLUME == 8
    assert TickTypeEnum.CLOSE == 9
    assert TickTypeEnum.OPEN == 14


# ── Conditions ──

def test_price_condition():
    pc = PriceCondition(con_id=265598, price=200.0, is_more=True)
    assert pc.con_id == 265598
    assert pc.price == 200.0
    assert pc.is_more is True


def test_time_condition():
    tc = TimeCondition(time="20260311-09:30:00", is_more=True)
    assert tc.time == "20260311-09:30:00"


def test_margin_condition():
    mc = MarginCondition(percent=30, is_more=False)
    assert mc.percent == 30
    assert mc.is_more is False


def test_volume_condition():
    vc = VolumeCondition(con_id=265598, volume=1_000_000, is_more=True)
    assert vc.volume == 1_000_000


def test_percent_change_condition():
    pcc = PercentChangeCondition(con_id=265598, change_percent=5.0, is_more=True)
    assert pcc.change_percent == 5.0


def test_execution_condition():
    ec = ExecutionCondition(symbol="AAPL", exchange="SMART", sec_type="STK")
    assert ec.symbol == "AAPL"
    assert ec.exchange == "SMART"
    assert ec.sec_type == "STK"


def test_execution_condition_defaults():
    ec = ExecutionCondition()
    assert ec.symbol == ""
    assert ec.exchange == ""
    assert ec.sec_type == ""


# ── EWrapper subclassing ──

def test_ewrapper_subclass_with_args():
    """Subclassing EWrapper with constructor arguments."""
    class MyWrapper(EWrapper):
        def __init__(self, some_arg):
            super().__init__()
            self.thing = some_arg

    w = MyWrapper("hello")
    assert w.thing == "hello"

    # Also works with kwargs
    w2 = MyWrapper(some_arg="world")
    assert w2.thing == "world"


def test_ewrapper_subclass():
    class MyWrapper(EWrapper):
        def __init__(self):
            super().__init__()
            self.events = []

        def tick_price(self, req_id, tick_type, price, attrib):
            self.events.append(("tick_price", req_id, tick_type, price))

        def tick_size(self, req_id, tick_type, size):
            self.events.append(("tick_size", req_id, tick_type, size))

        def order_status(self, order_id, status, filled, remaining,
                         avg_fill_price, perm_id, parent_id,
                         last_fill_price, client_id, why_held, mkt_cap_price):
            self.events.append(("order_status", order_id, status))

        def next_valid_id(self, order_id):
            self.events.append(("next_valid_id", order_id))

        def managed_accounts(self, accounts_list):
            self.events.append(("managed_accounts", accounts_list))

    w = MyWrapper()
    w.tick_price(1, TickTypeEnum.BID, 150.0, TickAttrib())
    w.tick_size(1, TickTypeEnum.VOLUME, 1000.0)
    w.next_valid_id(42)
    w.managed_accounts("DU12345")

    assert len(w.events) == 4
    assert w.events[0] == ("tick_price", 1, 1, 150.0)
    assert w.events[3] == ("managed_accounts", "DU12345")


# ── EClient construction ──

def test_eclient_construction():
    wrapper = EWrapper()
    client = EClient(wrapper)
    assert client.is_connected() is False


def test_eclient_disconnect_without_connect():
    wrapper = EWrapper()
    client = EClient(wrapper)
    client.disconnect()  # should not raise


# ── ibapi pattern: separate wrapper + client ──

def test_ibapi_pattern():
    """Test the standard ibapi usage pattern."""
    class MyApp(EWrapper):
        def __init__(self):
            super().__init__()
            self.next_id = None

        def next_valid_id(self, order_id):
            self.next_id = order_id

    app = MyApp()
    client = EClient(app)
    assert client.is_connected() is False
    assert app.next_id is None


# ── P&L subscriptions ──

def test_pnl_subscribe_cancel():
    wrapper = EWrapper()
    client = EClient(wrapper)
    client._test_connect()
    client.req_pnl(1, "DU12345")
    client.cancel_pnl(1)


def test_pnl_single_subscribe_cancel():
    wrapper = EWrapper()
    client = EClient(wrapper)
    client.req_pnl_single(2, "DU12345", "", 265598)
    client.cancel_pnl_single(2)


# ── Account summary ──

def test_account_summary_subscribe_cancel():
    wrapper = EWrapper()
    client = EClient(wrapper)
    client.req_account_summary(3, "All", "NetLiquidation,BuyingPower")
    client.cancel_account_summary(3)


# ── Positions ──

def test_cancel_positions():
    wrapper = EWrapper()
    client = EClient(wrapper)
    client.cancel_positions()  # should not raise


# ── EWrapper account/P&L callbacks ──

def test_ewrapper_pnl_callbacks():
    class MyWrapper(EWrapper):
        def __init__(self):
            super().__init__()
            self.events = []

        def pnl(self, req_id, daily_pnl, unrealized_pnl, realized_pnl):
            self.events.append(("pnl", req_id, daily_pnl))

        def pnl_single(self, req_id, pos, daily_pnl, unrealized_pnl, realized_pnl, value):
            self.events.append(("pnl_single", req_id, pos))

        def account_summary(self, req_id, account, tag, value, currency):
            self.events.append(("account_summary", tag, value))

        def account_summary_end(self, req_id):
            self.events.append(("account_summary_end", req_id))

        def position(self, account, contract, pos, avg_cost):
            self.events.append(("position", pos, avg_cost))

        def position_end(self):
            self.events.append(("position_end",))

    w = MyWrapper()
    w.pnl(1, 100.0, 50.0, 50.0)
    w.pnl_single(2, 10.0, 20.0, 15.0, 5.0, 1500.0)
    w.account_summary(3, "DU12345", "NetLiquidation", "100000.00", "USD")
    w.account_summary_end(3)
    w.position_end()

    assert len(w.events) == 5
    assert w.events[0] == ("pnl", 1, 100.0)
    assert w.events[2] == ("account_summary", "NetLiquidation", "100000.00")
    assert w.events[4] == ("position_end",)


def test_auto_binding_is_refused_only_for_a_client_that_is_not_zero():
    """Nothing is sent to the wire for this request.

    It is refused for any client id but the one those orders bind to, and
    otherwise sets state that does not apply here: this session hears about
    every order on the account either way, so the refusal is the observable
    part.
    """
    from ibx import EClient, EWrapper

    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.errors = []

        def error(self, req_id, error_time, code, msg, advanced=""):
            self.errors.append((code, msg))

    w = W()
    c = EClient(w)
    c._test_connect("T")

    # Nothing names a client, so nothing is refused.
    c.req_auto_open_orders(True)
    c._test_dispatch_once()
    assert not [e for e in w.errors if e[0] == 327]

    # A client that is not the one those orders bind to is told so.
    c._test_set_client_id(7)
    c.req_auto_open_orders(True)
    c._test_dispatch_once()
    assert [e for e in w.errors if e[0] == 327], w.errors


def test_solving_an_option_answers_rather_than_refusing():
    """Both calculations are solved locally and answered on
    `tick_option_computation`; the wire carries no request for either.

    This surface refused them while the Rust one answered, so the same call
    against the same session gave a number in one language and an error in
    the other.
    """
    from ibx import EClient, EWrapper, Contract

    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.computed = []
            self.errors = []

        def tick_option_computation(self, req_id, tick_type, attrib, implied_vol,
                                    delta, opt_price, pv_dividend, gamma, vega,
                                    theta, und_price):
            self.computed.append((req_id, implied_vol, opt_price))

        def error(self, req_id, error_time, code, msg, advanced=""):
            self.errors.append((code, msg))

    w = W()
    c = EClient(w)
    c._test_connect("T")

    option = Contract()
    option.conId = 999001
    option.secType = "OPT"
    option.symbol = "SPY"
    option.strike = 500.0
    option.right = "C"
    option.lastTradeDateOrContractMonth = "20270115"
    c._test_map_con_id(999001, 0)

    # What the venue published for this contract, which the answer is solved
    # against — without it there is no model and the call says so.
    c._test_push_option_model(0, 0.20, 30.0, 505.0)
    c._test_dispatch_once()
    w.computed.clear()

    c.calculate_option_price(77, option, 0.25, 505.0)
    c._test_dispatch_once()

    assert not w.errors, w.errors
    assert w.computed, "the price was answered, not refused"
    assert w.computed[0][0] == 77, "under the request that asked for it"


def test_a_question_kept_for_a_model_does_not_outlive_its_session():
    """It belongs to the session that asked it.

    Left behind, the next session answers it under a request id nobody there
    ever used, or waits on a model for a contract it is not watching.
    """
    from ibx import EClient, EWrapper, Contract

    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.computed = []

        def tick_option_computation(self, req_id, tick_type, attrib, implied_vol,
                                    delta, opt_price, pv_dividend, gamma, vega,
                                    theta, und_price):
            self.computed.append(req_id)

        def error(self, req_id, error_time, code, msg, advanced=""):
            pass

    w = W()
    c = EClient(w)
    c._test_connect("T")
    option = Contract()
    option.conId = 999003
    option.secType = "OPT"
    option.symbol = "SPY"
    option.strike = 500.0
    option.right = "C"
    option.lastTradeDateOrContractMonth = "20270115"
    c._test_map_con_id(999003, 2)
    c._test_map_instrument(91, 2)
    c.calculate_implied_volatility(91, option, 35.0, 505.0)
    c.disconnect()

    # A second session, and the venue publishes a model for that slot.
    c._test_connect("T")
    c._test_map_con_id(999003, 2)
    c._test_push_option_model(2, 0.20, 30.0, 505.0)
    c._test_dispatch_once()

    assert 91 not in w.computed, (
        "the question outlived the session that asked it and was answered "
        f"under an id this one never used: {w.computed}"
    )


def test_a_calculation_asked_before_the_model_waits_for_it():
    """A question asked before the venue has stated a model is kept, not
    refused, and answered when the model arrives.

    The venue states a model only for a contract something watches, so asking
    first is the ordinary order to ask in. The Rust surface kept the question
    and this one refused it, which is the same divergence as above one step
    earlier.
    """
    from ibx import EClient, EWrapper, Contract

    class W(EWrapper):
        def __init__(self):
            super().__init__()
            self.computed = []
            self.errors = []

        def tick_option_computation(self, req_id, tick_type, attrib, implied_vol,
                                    delta, opt_price, pv_dividend, gamma, vega,
                                    theta, und_price):
            self.computed.append((req_id, implied_vol, opt_price))

        def error(self, req_id, error_time, code, msg, advanced=""):
            self.errors.append((code, msg))

    w = W()
    c = EClient(w)
    c._test_connect("T")

    option = Contract()
    option.conId = 999002
    option.secType = "OPT"
    option.symbol = "SPY"
    option.strike = 500.0
    option.right = "C"
    option.lastTradeDateOrContractMonth = "20270115"
    # Watched, so the venue would state a model for it — but has not yet.
    c._test_map_con_id(999002, 1)
    c._test_map_instrument(88, 1)

    # A price the model below does not carry, so the answer to this question
    # cannot be mistaken for the model that answers it.
    c.calculate_implied_volatility(88, option, 35.0, 505.0)
    c._test_dispatch_once()
    assert not w.errors, f"the question was refused rather than kept: {w.errors}"
    assert not w.computed, "answered with no model stated to answer from"

    c._test_push_option_model(1, 0.20, 30.0, 505.0)
    c._test_dispatch_once()

    assert not w.errors, w.errors
    kept = [x for x in w.computed if x[0] == 88 and x[2] == 35.0]
    assert kept, (
        "the model arrived and the question that waited on it went unanswered: "
        f"{w.computed}"
    )


def test_an_order_the_transport_refused_is_not_left_working():
    """A refused order does not stay in `openTrades()`.

    A transport that will not carry the order says so on the error callback
    and returns, which is how the reference client answers a request it
    refuses. The record was written before the call and never taken back, so
    an order the venue never received read as one working at it.
    """
    import ibx

    class Refusing:
        """Answers a place the way a transport that cannot send does."""

        def __init__(self, wrapper):
            self.wrapper = wrapper

        def next_order_id(self):
            return 11

        def place_order(self, order_id, contract, order):
            self.wrapper.error(order_id, 504, "not connected", "")

    ib = ibx.IB()
    ib.client = Refusing(ib.wrapper)

    contract = ibx.Contract(symbol="SPY", secType="STK", exchange="SMART", currency="USD")
    order = ibx.Order(action="BUY", totalQuantity=1, orderType="MKT")
    trade = ib.placeOrder(contract, order)

    assert trade is not None, "the caller still gets the object to read"
    assert ib.wrapper.trade_for(11) is None, "and it is not a working order"
    assert all(t.order.orderId != 11 for t in ib.openTrades())


def test_a_model_the_venue_left_blank_reaches_a_wrapper_that_did_not_write_the_method():
    """A caller who watches an option and never writes
    `tick_option_computation` keeps its session.

    The venue leaves figures out of the model it publishes, and this surface
    hands those on as `None`, which is what the reference client hands on.
    Declared as numbers, the base class refused the call, and the refusal left
    the caller's reading loop: one model without a dividend in it ended the
    session, on every caller that had not written the method itself. Every test
    here had written one, so nothing saw it.
    """
    from ibx import EWrapper

    EWrapper().tick_option_computation(
        1, 13, 0, None, None, None, None, None, None, None, None,
    )
    # And the same call with every figure stated.
    EWrapper().tick_option_computation(
        1, 13, 0, 0.21, 0.5, 2.5, 0.0, 0.01, 0.1, -0.02, 330.0,
    )
