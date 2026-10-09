"""Tests for ibapi-compatible class construction, fields, and subclassing."""

from decimal import Decimal

import pytest
from ibx import (
    Contract, Order, BarData, ContractDetails, TagValue, OrderState,
    EWrapper, EClient,
    TickAttrib, TickAttribLast, TickAttribBidAsk, TickTypeEnum,
    PriceCondition, TimeCondition, MarginCondition,
    ExecutionCondition, VolumeCondition, PercentChangeCondition,
)


# ── Contract ──

# The official API's values of an unset field (ibapi 10.46 `ibapi.const`).
UNSET_DOUBLE = 1.7976931348623157e308
UNSET_INTEGER = 2147483647
UNSET_DECIMAL = Decimal("170141183460469231731687303715884105727")


def test_contract_defaults():
    # The official API's Contract(): no security type, exchange or currency.
    c = Contract()
    assert c.con_id == 0
    assert c.symbol == ""
    assert (c.sec_type, c.secType) == ("", "")
    assert c.exchange == ""
    assert c.currency == ""
    assert c.strike == UNSET_DOUBLE
    assert c.comboLegs == [] and c.deltaNeutralContract is None


def test_contract_combo_legs_list_is_the_attribute():
    # As the official API's: appending to comboLegs changes the contract.
    from ibx import ComboLeg
    c = Contract()
    c.comboLegs.append(ComboLeg())
    assert len(c.comboLegs) == 1
    assert c.comboLegs is c.combo_legs


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
    # The official API's Order(): unset values, empty texts, None lists.
    o = Order()
    assert o.order_id == 0
    assert o.action == ""
    assert o.totalQuantity == UNSET_DECIMAL and isinstance(o.totalQuantity, Decimal)
    assert o.filledQuantity == UNSET_DECIMAL
    assert o.order_type == ""
    assert o.tif == ""
    assert o.transmit is True
    assert o.what_if is False
    for name in ("lmtPrice", "auxPrice", "trailingPercent", "cashQty", "triggerPrice",
                 "adjustedStopPrice", "adjustedStopLimitPrice", "percentOffset", "trailStopPrice"):
        assert getattr(o, name) == UNSET_DOUBLE, name
    for name in ("minQty", "volatilityType", "referencePriceType", "hedgeMaxSize", "basisPointsType"):
        assert getattr(o, name) == UNSET_INTEGER, name
    assert o.dontUseAutoPriceForHedge is False
    for name in ("algoParams", "smartComboRoutingParams", "orderComboLegs", "orderMiscOptions",
                 "routeMarketableToBbo", "seekPriceImprovement", "usePriceMgmtAlgo"):
        assert getattr(o, name) is None, name
    assert o.conditions == []


def _conditional(conditions):
    from ibx import Contract, EClient, EWrapper, Order
    client = EClient(EWrapper())
    client._test_connect("TEST123")
    client._test_seed_instrument(756733, 0)
    contract = Contract()
    contract.con_id, contract.symbol, contract.sec_type, contract.exchange, contract.currency = 756733, "SPY", "STK", "SMART", "USD"
    order = Order()
    order.action, order.total_quantity, order.order_type, order.lmt_price, order.tif = "BUY", 1, "LMT", 10.0, "GTC"
    order.conditions = conditions
    return client, contract, order


def test_an_order_with_a_condition_that_is_not_understood_is_not_sent():
    """ibx#541: such a condition was skipped with a log line and the order sent without it."""
    import pytest
    for bad in (object(), "20991231 23:59:59", {"condType": 3}):
        client, contract, order = _conditional([TimeCondition(True, "20991231-23:59:59"), bad])
        with pytest.raises(ValueError, match="order condition 2 .* is not understood"):
            client.place_order(1, contract, order)
        assert client._test_take_order_conditions() is None, "nothing went to the engine"


def test_the_condition_classes_of_the_official_library_are_read():
    """ibx#541: a program ported from the official client library keeps its conditions."""
    conditions = pytest.importorskip("ibapi.order_condition")
    price = conditions.PriceCondition()
    price.isMore, price.price, price.conId, price.exchange, price.triggerMethod = False, 12.5, 265598, "SMART", 2
    time_ = conditions.TimeCondition()
    time_.isMore, time_.time = True, "20991231 23:59:59 US/Eastern"
    margin = conditions.MarginCondition()
    margin.isMore, margin.percent = True, 30
    execution = conditions.ExecutionCondition()
    execution.symbol, execution.exchange, execution.secType = "AAPL", "SMART", "STK"
    volume = conditions.VolumeCondition()
    volume.isMore, volume.volume, volume.conId, volume.exchange = True, 1000, 265598, "SMART"
    change = conditions.PercentChangeCondition()
    change.isMore, change.changePercent, change.conId, change.exchange = False, 1.5, 265598, "SMART"
    client, contract, order = _conditional([price, time_, margin, execution, volume, change])
    client.place_order(1, contract, order)
    sent = client._test_take_order_conditions()
    assert [s.split(" ")[0] for s in sent] == ["Price", "Time", "Margin", "Execution", "Volume", "PercentChange"], sent
    assert "con_id: 265598" in sent[0] and "is_more: false" in sent[0] and "trigger_method: 2" in sent[0], sent[0]
    # The time goes out in the form the server takes (ibx#416).
    assert 'time: "21000101-04:59:59"' in sent[1] and "is_more: true" in sent[1], sent[1]
    assert "percent: 30" in sent[2] and 'symbol: "AAPL"' in sent[3] and "volume: 1000" in sent[4], sent
    assert "percent: 1.5" in sent[5], sent[5]


def test_official_conditions_joined_by_or_are_refused():
    """The conditions of an order are joined by "and": one joined to the next by "or" would change its meaning."""
    conditions = pytest.importorskip("ibapi.order_condition")
    first, second = conditions.TimeCondition(), conditions.TimeCondition()
    first.isMore, first.time, first.isConjunctionConnection = True, "20991231 23:59:59 US/Eastern", False
    second.isMore, second.time, second.isConjunctionConnection = True, "20991231 23:59:59 US/Eastern", False
    client, contract, order = _conditional([first, second])
    with pytest.raises(ValueError, match='joined by "or"'):
        client.place_order(1, contract, order)
    # The flag of the last condition joins nothing.
    first.isConjunctionConnection = True
    client.place_order(1, contract, order)
    assert len(client._test_take_order_conditions()) == 2


def test_order_list_attributes_are_the_lists():
    # As the official API's: appending to the list changes the order.
    o = Order()
    o.conditions.append(TimeCondition(True, "20991231-23:59:59"))
    assert len(o.conditions) == 1
    o.algoParams = []
    o.algoParams.append(TagValue("maxPctVol", "0.1"))
    assert o.algoParams[0].tag == "maxPctVol"


def test_order_quantities_are_decimals():
    # totalQuantity takes a Decimal, an int, a float or a text, and is a Decimal.
    o = Order()
    o.totalQuantity = 100
    assert o.totalQuantity == Decimal("100")
    o.totalQuantity = Decimal("1.5")
    assert o.total_quantity == Decimal("1.5")
    o.totalQuantity = "2"
    assert o.totalQuantity == 2
    o.totalQuantity = UNSET_DECIMAL
    assert o.totalQuantity == UNSET_DECIMAL


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
    assert b.volume == UNSET_DECIMAL
    assert b.wap == UNSET_DECIMAL


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
    # The official API's names and unset values.
    assert (cd.minTick, cd.longName, cd.marketName) == (0.0, "", "")
    assert cd.minSize == UNSET_DECIMAL and cd.sizeIncrement == UNSET_DECIMAL
    assert cd.secIdList is None and cd.ineligibilityReasonList is None


def test_contract_details_contract_is_mutable_in_place():
    # ibx#230: the getter used to hand back a CLONE, so this mutation was a
    # silent no-op and `cd.contract is cd.contract` was False.
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
    assert os.commission_and_fees == UNSET_DOUBLE
    # The official API's names and unset values.
    assert (os.commissionAndFees, os.minCommissionAndFees, os.initMarginBeforeOutsideRTH) == (UNSET_DOUBLE,) * 3
    assert os.suggestedSize == UNSET_DECIMAL
    assert os.orderAllocations is None
    os.initMarginBefore = "5"
    assert os.init_margin_before == "5"


# ── TagValue ──

def test_tagvalue():
    tv = TagValue("key", "value")
    assert tv.tag == "key"
    assert tv.value == "value"
    # As the official API's: the text of what it is given.
    assert (TagValue().tag, TagValue().value) == ("None", "None")


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

# The official API's constructors (ibapi 10.46 `order_condition`): the same
# arguments, the camelCase names, None until set.

def test_price_condition():
    pc = PriceCondition(conId=265598, price=200.0, isMore=True)
    assert pc.con_id == 265598 and pc.conId == 265598
    assert pc.price == 200.0
    assert pc.is_more is True and pc.isMore is True
    assert pc.condType == 1
    assert PriceCondition(1, 265598, "SMART", True, 200.0).triggerMethod == 1


def test_time_condition():
    tc = TimeCondition(time="20260311-09:30:00", isMore=True)
    assert tc.time == "20260311-09:30:00"
    assert tc.condType == 3


def test_margin_condition():
    mc = MarginCondition(percent=30, isMore=False)
    assert mc.percent == 30
    assert mc.is_more is False
    assert mc.condType == 4


def test_volume_condition():
    vc = VolumeCondition(conId=265598, volume=1_000_000, isMore=True)
    assert vc.volume == 1_000_000
    assert vc.condType == 6


def test_percent_change_condition():
    pcc = PercentChangeCondition(conId=265598, changePercent=5.0, isMore=True)
    assert pcc.change_percent == 5.0 and pcc.changePercent == 5.0
    assert pcc.condType == 7
    assert PercentChangeCondition().changePercent == UNSET_DOUBLE


def test_execution_condition():
    ec = ExecutionCondition(symbol="AAPL", exch="SMART", secType="STK")
    assert ec.symbol == "AAPL"
    assert ec.exchange == "SMART"
    assert ec.sec_type == "STK" and ec.secType == "STK"
    assert ec.condType == 5


def test_condition_defaults():
    ec = ExecutionCondition()
    assert (ec.symbol, ec.exchange, ec.secType) == (None, None, None)
    pc = PriceCondition()
    assert (pc.conId, pc.exchange, pc.price, pc.isMore, pc.triggerMethod) == (None,) * 5
    assert (TimeCondition().time, MarginCondition().percent, VolumeCondition().volume) == (None, None, None)


# ── EWrapper subclassing ──

def test_ewrapper_subclass_with_args():
    """Issue #105: subclassing EWrapper with constructor arguments."""
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
