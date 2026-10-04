"""ibx#487: scenarios recorded from the reference gateway replayed through the
Python client. The scenario runner of the Rust tests (``test_support::scenario``)
plays the recorded servers on in-memory links; this driver makes each recorded
request on the Python ``EClient`` and hands back the callbacks of its wrapper in
the official client library's form. The callbacks are compared with the ones
the official client library got from the reference: same callbacks, fields,
count and order. The orders' frames are compared on the way, as in the Rust
tests.

Needs the bindings built with the test helpers:
``maturin develop --features python,test-support``. No network.
"""

import json
import sys

import pytest

from ibx import (BarData, ComboLeg, CommissionAndFeesReport, Contract, EClient, EWrapper, Execution, Order, TagValue,
                 TickAttrib)

if not hasattr(EClient, "_test_replay_scenario"):
    pytest.skip("built without the test-support feature", allow_module_level=True)

UNSET = sys.float_info.max


def contract_dict(c):
    return {"conId": c.conId, "symbol": c.symbol, "secType": c.secType}


ORDER_FIELDS = ["action", "totalQuantity", "orderType", "lmtPrice", "auxPrice", "tif", "ocaGroup", "orderRef",
                "parentId", "outsideRth", "goodAfterTime", "goodTillDate", "account", "trailingPercent", "whatIf",
                "permId", "clientId"]


def order_dict(o):
    return {k: getattr(o, k) for k in ORDER_FIELDS}


class Recorder(EWrapper):
    """Every callback as the official client library's wrapper call, read
    with the official attribute names."""

    def __init__(self):
        super().__init__()
        self.calls = []

    def _add(self, *call):
        self.calls.append(list(call))

    def error(self, req_id, code, text, advanced=""):
        self._add("error", req_id, None, code, text, advanced)

    def order_status(self, order_id, status, filled, remaining, avg, perm_id, parent_id, last, client_id, why_held, mkt_cap):
        self._add("orderStatus", order_id, status, filled, remaining, avg, perm_id, parent_id, last, client_id, why_held, mkt_cap)

    def open_order(self, order_id, contract, order, state):
        self._add("openOrder", order_id, contract_dict(contract), order_dict(order), {"status": state.status})

    def open_order_end(self):
        self._add("openOrderEnd")

    def position(self, account, contract, pos, avg_cost):
        self._add("position", account, contract_dict(contract), pos, avg_cost)

    def position_end(self):
        self._add("positionEnd")

    def exec_details(self, req_id, contract, e):
        self._add("execDetails", req_id, contract_dict(contract), {
            "exchange": e.exchange, "side": e.side, "shares": e.shares, "price": e.price,
            "cumQty": e.cumQty, "avgPrice": e.avgPrice, "orderId": e.orderId, "orderRef": e.orderRef,
            "lastLiquidity": e.lastLiquidity,
        })

    def commission_and_fees_report(self, r):
        self._add("commissionAndFeesReport", {
            "commissionAndFees": r.commissionAndFees, "currency": r.currency,
        })

    def tick_price(self, req_id, tick_type, price, a):
        self._add("tickPrice", req_id, tick_type, price, {
            "canAutoExecute": a.canAutoExecute, "pastLimit": a.pastLimit, "preOpen": a.preOpen,
        })

    def tick_size(self, req_id, tick_type, size):
        self._add("tickSize", req_id, tick_type, size)

    def tick_string(self, req_id, tick_type, value):
        self._add("tickString", req_id, tick_type, value)

    def tick_generic(self, req_id, tick_type, value):
        self._add("tickGeneric", req_id, tick_type, value)

    def tick_snapshot_end(self, req_id):
        self._add("tickSnapshotEnd", req_id)

    def market_data_type(self, req_id, t):
        self._add("marketDataType", req_id, t)

    def tick_req_params(self, req_id, min_tick, bbo, perms):
        self._add("tickReqParams", req_id, min_tick, bbo, perms)

    def account_summary(self, req_id, account, tag, value, currency):
        self._add("accountSummary", req_id, account, tag, value, currency)

    def account_summary_end(self, req_id):
        self._add("accountSummaryEnd", req_id)

    def _bar(self, b):
        return {"date": b.date, "open": b.open, "high": b.high, "low": b.low, "close": b.close,
                "volume": b.volume, "wap": b.wap, "barCount": b.barCount}

    def historical_data(self, req_id, bar):
        self._add("historicalData", req_id, self._bar(bar))

    def historical_data_update(self, req_id, bar):
        self._add("historicalDataUpdate", req_id, self._bar(bar))

    def historical_data_end(self, req_id, start, end):
        self._add("historicalDataEnd", req_id, start, end)


def make_contract(q):
    c = Contract()
    for k, v in q.items():
        if k == "comboLegs":
            c.comboLegs = [ComboLeg(l.get("conId", 0), l.get("ratio", 0), l.get("action", ""), l.get("exchange", "")) for l in v]
        elif k == "primaryExch":
            c.primaryExchange = v
        else:
            setattr(c, k, v)
    return c


NUMBERS = {"totalQuantity", "lmtPrice", "auxPrice", "trailingPercent", "trailStopPrice", "cashQty", "startingPrice",
           "stockRefPrice", "peggedChangeAmount", "referenceChangeAmount", "lmtPriceOffset"}


def make_order(q):
    o = Order()
    for k, v in q.items():
        if k in ("softDollarTier", "orderId"):
            continue
        if k in ("algoParams", "smartComboRoutingParams"):
            v = [TagValue(t["tag"], t["value"]) for t in v]
        elif k in NUMBERS:
            v = UNSET if v == "MAX" else float(v)
        setattr(o, k, v)
    return o


class Driver:
    """Makes the recorded requests on the Python client."""

    def __init__(self, client, wrapper):
        self.client, self.wrapper = client, wrapper

    def request(self, name, request):
        q = json.loads(request)
        c = self.client
        rid = q.get("reqId", 0) if isinstance(q, dict) else 0
        if name == "PLACE_ORDER":
            c.place_order(q["orderId"], make_contract(q["contract"]), make_order(q["order"]))
        elif name == "CANCEL_ORDER":
            c.cancel_order(q["orderId"], "")
        elif name == "REQ_MKT_DATA":
            c.req_mkt_data(rid, make_contract(q["contract"]), q.get("genericTickList", ""), q.get("snapshot", False),
                           q.get("regulatorySnapshot", False), [])
        elif name == "CANCEL_MKT_DATA":
            c.cancel_mkt_data(rid)
        elif name == "REQ_MARKET_DATA_TYPE":
            c.req_market_data_type(q.get("marketDataType", 1))
        elif name == "REQ_ACCOUNT_SUMMARY":
            c.req_account_summary(rid, q.get("group", ""), q.get("tags", ""))
        elif name == "CANCEL_ACCOUNT_SUMMARY":
            c.cancel_account_summary(rid)
        elif name == "REQ_HISTORICAL_DATA":
            c.req_historical_data(rid, make_contract(q["contract"]), q.get("endDateTime", ""), q.get("duration", ""),
                                  q.get("barSizeSetting", ""), q.get("whatToShow", ""), int(q.get("useRTH", False)),
                                  q.get("formatDate", 1), q.get("keepUpToDate", False), [])
        elif name == "CANCEL_HISTORICAL_DATA":
            c.cancel_historical_data(rid)
        elif name == "REQ_POSITIONS":
            c.req_positions()
        elif name == "CANCEL_POSITIONS":
            c.cancel_positions()
        else:
            return False
        return True

    def dispatch(self):
        self.client._test_dispatch_once()
        calls, self.wrapper.calls = self.wrapper.calls, []
        # Decimals as their text, as the recorded callbacks have them.
        return json.dumps(calls, default=str)


def known(line):
    """The known differences of the Rust order scenarios (tests/scenario_replay.rs
    `known`): the 399 exchange, the first STP limit price."""
    f = line.split("|")
    if f[0] == "error" and len(f) > 3 and f[2] == "399":
        rows = f[3].split("\n")
        if len(rows) == 3:
            words = rows[1].split(" ")
            words[-1] = "{exchange}"
            rows[1] = " ".join(words)
        f[3] = "\n".join(rows)
    return "|".join(f)


def replay(name, **opts):
    w = Recorder()
    c = EClient(w)
    out = c._test_replay_scenario(name, Driver(c, w), **opts)
    return out


def assert_same(out, keep=lambda l: True, mask=known):
    assert out["frame_error"] is None, out["frame_error"]
    assert out["not_made"] == [], out["not_made"]
    ours = [mask(l) for l in out["ours"] if keep(l)]
    theirs = [mask(l) for l in out["theirs"] if keep(l)]
    for k, (a, b) in enumerate(zip(ours, theirs)):
        assert a == b, f"callback {k}:\n  ours      {a}\n  reference {b}"
    assert len(ours) == len(theirs), (ours[len(theirs):], theirs[len(ours):])
    return theirs


# A LMT order before the open, then its cancel (26/09/2026, ibx#472, ibx#465).
def test_lmt_order_then_cancel():
    out = replay("20260926/lmt_cancel", compare=["order"])
    assert out["frames_compared"] == 2
    assert len(assert_same(out)) == 9


# Two orders in one OCA group, both cancelled (26/09/2026, ibx#311, ibx#329).
def test_oca_group_and_the_refused_second_cancel():
    out = replay("20260926/oca_group", compare=["order"])
    assert out["frames_compared"] == 3
    theirs = assert_same(out)
    assert any("|10148|" in l for l in theirs)


# The cancel of an unknown order (26/09/2026, ibx#464).
def test_cancel_of_an_unknown_order():
    out = replay("20260926/cancel_unknown", compare=["order"])
    assert assert_same(out)[0].startswith("error|")


# A SMART combo bought and sold (30/09/2026, ibx#474, ibx#471): the fills of
# the combo and its legs, commissions. As the Rust test: QQQ's top of book
# and the positions are left out (the reference's session state).
def test_combo_fill_executions_and_commissions():
    out = replay("20260930/i105_combo_fill", compare=["order"])
    theirs = assert_same(out, keep=lambda l: not l.startswith("position") and l.split("|")[1:2] != ["9501"])
    assert any(l.startswith("execDetails|") for l in theirs)
    assert any(l.startswith("commissionAndFeesReport|") for l in theirs)


# SPY then QQQ top of book in the session (28/09/2026), up to the second SPY
# request (src/golden/l1.rs `spy_and_qqq_in_the_session`).
def test_top_of_book_in_the_session():
    out = replay("l1_spy_qqq_rth", codec=True, until=6581, compare=["market_data"])
    theirs = assert_same(out)
    assert sum(l.startswith("tick") for l in theirs) > 40


# reqAccountSummary of four tags and $LEDGER:ALL (26/09/2026, ibx#479): the
# subscription and its cancel, the four tag rows (the rest: ibx#486).
def test_account_summary_tag_rows():
    out = replay("20260926/account_summary", compare=["account_subscription"])
    assert out["frame_error"] is None and out["frames_compared"] == 2
    assert out["ours"][:4] == out["theirs"][:4]


# keepUpToDate bars then their cancel (26/09/2026, ibx#429, ibx#431).
def test_historical_keep_up_to_date_then_cancel():
    out = replay("20260926b/hist_keep_up_to_date")
    assert len(assert_same(out)) == 32


# The callback objects answer to the official client library's attribute
# names (ibx#487: Execution, TickAttrib, BarData and CommissionAndFeesReport
# had only ibx's names).
def test_callback_objects_have_the_official_attribute_names():
    e = Execution()
    e.cumQty, e.avgPrice, e.lastLiquidity, e.orderRef = 2, 340.5, 2, "ref"
    assert (e.cum_qty, e.avg_price, e.last_liquidity, e.order_ref) == (2, 340.5, 2, "ref")
    assert (e.execId, e.acctNumber, e.permId, e.clientId, e.orderId, e.evRule, e.evMultiplier, e.modelCode,
            e.pendingPriceRevision) == ("", "", 0, 0, 0, "", 0.0, "", False)
    a = TickAttrib(True, False, True)
    assert (a.canAutoExecute, a.pastLimit, a.preOpen) == (True, False, True)
    assert BarData(bar_count=3).barCount == 3
    r = CommissionAndFeesReport()
    r.realizedPNL, r.yield_ = 1.5, 0.25
    # yieldRedemptionDate is an int (YYYYMMDD), 0 when none, as the official API's.
    assert (r.realized_pnl, r.yield_amount, r.execId, r.commissionAndFees, r.yieldRedemptionDate) == (1.5, 0.25, "", 0.0, 0)
