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
    c = Contract()
    assert c.con_id == 0
    assert c.symbol == ""
    assert c.sec_type == "STK"
    assert c.exchange == "SMART"
    assert c.currency == "USD"
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
    assert o.tif == "DAY"
    assert o.transmit is True
    assert o.what_if is False


def test_order_kwargs():
    o = Order(action="BUY", total_quantity=100, order_type="LMT", lmt_price=150.50)
    assert o.action == "BUY"
    assert o.total_quantity == 100.0
    assert o.order_type == "LMT"
    assert o.lmt_price == 150.50


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


# ── OrderState ──

def test_order_state_defaults():
    os = OrderState()
    assert os.status == ""
    assert os.commission == 0.0


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


# ── EWrapper subclassing ──

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
