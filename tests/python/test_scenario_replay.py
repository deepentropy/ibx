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

from ibx import (BarData, ComboLeg, CommissionAndFeesReport, Contract, EClient, EWrapper, Execution, Order,
                 OrderComboLeg, TagValue, TickAttrib)

if not hasattr(EClient, "_test_replay_scenario"):
    pytest.skip("built without the test-support feature", allow_module_level=True)

UNSET = sys.float_info.max


def contract_dict(c):
    return {"conId": c.conId, "symbol": c.symbol, "secType": c.secType, "exchange": c.exchange,
            "primaryExchange": c.primaryExchange}


ORDER_FIELDS = ["action", "totalQuantity", "orderType", "lmtPrice", "auxPrice", "tif", "ocaGroup", "orderRef",
                "parentId", "outsideRth", "goodAfterTime", "goodTillDate", "account", "trailingPercent", "trailStopPrice",
                "whatIf", "permId", "clientId", "ocaType", "clearingIntent", "shareholder", "deltaNeutralOrderType",
                "adjustedOrderType", "submitter"]


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

    def exec_details_end(self, req_id):
        self._add("execDetailsEnd", req_id)

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

    def tick_by_tick_all_last(self, req_id, tick_type, time, price, size, attrib, exchange, special):
        self._add("tickByTickAllLast", req_id, tick_type, time, price, size,
                  {"pastLimit": attrib.pastLimit, "unreported": attrib.unreported}, exchange, special)

    def tick_by_tick_bid_ask(self, req_id, time, bid, ask, bid_size, ask_size, attrib):
        self._add("tickByTickBidAsk", req_id, time, bid, ask, bid_size, ask_size,
                  {"bidPastLow": attrib.bidPastLow, "askPastHigh": attrib.askPastHigh})

    def tick_by_tick_mid_point(self, req_id, time, mid):
        self._add("tickByTickMidPoint", req_id, time, mid)

    def historical_ticks_last(self, req_id, ticks, done):
        self._add("historicalTicksLast", req_id, [{
            "time": t.time, "price": t.price, "size": t.size, "exchange": t.exchange,
            "specialConditions": t.specialConditions,
            "tickAttribLast": {"pastLimit": t.tickAttribLast.pastLimit, "unreported": t.tickAttribLast.unreported},
        } for t in ticks], done)

    def real_time_bar(self, req_id, time, open_, high, low, close, volume, wap, count):
        self._add("realtimeBar", req_id, time, open_, high, low, close, volume, wap, count)

    def head_timestamp(self, req_id, ts):
        self._add("headTimestamp", req_id, ts)

    def scanner_data(self, req_id, rank, details, distance, benchmark, projection, legs):
        self._add("scannerData", req_id, rank, {"contract": contract_dict(details.contract)}, distance, benchmark,
                  projection, legs)

    def scanner_data_end(self, req_id):
        self._add("scannerDataEnd", req_id)

    def completed_order(self, contract, order, state):
        o = {k: getattr(order, k) for k in ("action", "totalQuantity", "orderType", "lmtPrice", "auxPrice", "tif",
                                            "orderRef", "permId", "filledQuantity")}
        self._add("completedOrder", contract_dict(contract), o, {"status": state.status})

    def completed_orders_end(self):
        self._add("completedOrdersEnd")

    def update_account_value(self, key, value, currency, account):
        self._add("updateAccountValue", key, value, currency, account)

    def update_portfolio(self, contract, position, price, value, avg_cost, unrealized, realized, account):
        self._add("updatePortfolio", contract_dict(contract), position, price, value, avg_cost, unrealized, realized,
                  account)

    def update_account_time(self, ts):
        self._add("updateAccountTime", ts)

    def account_download_end(self, account):
        self._add("accountDownloadEnd", account)

    def pnl(self, req_id, daily, unrealized, realized):
        self._add("pnl", req_id, daily, unrealized, realized)

    def pnl_single(self, req_id, pos, daily, unrealized, realized, value):
        self._add("pnlSingle", req_id, pos, daily, unrealized, realized, value)

    def smart_components(self, req_id, components):
        self._add("smartComponents", req_id, {str(bit): list(v) for bit, v in dict(components).items()})


def make_contract(q):
    c = Contract()
    for k, v in q.items():
        if k == "comboLegs":
            c.comboLegs = [ComboLeg(l.get("conId", 0), l.get("ratio", 0), l.get("action", ""), l.get("exchange", "")) for l in v]
        elif k == "primaryExch":
            c.primaryExchange = v
        elif k == "multiplier" and not isinstance(v, str):
            c.multiplier = f"{v:g}"  # a number at session level 225, as the Rust driver (ibx#646)
        else:
            setattr(c, k, v)
    return c


NUMBERS = {"totalQuantity", "lmtPrice", "auxPrice", "trailingPercent", "trailStopPrice", "cashQty", "startingPrice",
           "stockRefPrice", "peggedChangeAmount", "referenceChangeAmount", "lmtPriceOffset"}


def order_combo_leg(price):
    leg = OrderComboLeg()
    leg.price = UNSET if price == "MAX" else float(price)
    return leg


def make_order(q):
    o = Order()
    for k, v in q.items():
        if k in ("softDollarTier", "orderId"):
            continue
        if k in ("algoParams", "smartComboRoutingParams"):
            v = [TagValue(t["tag"], t["value"]) for t in v]
        elif k == "orderComboLegs":
            v = [order_combo_leg(l.get("price", "MAX")) for l in v]
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
        elif name == "REQ_ALL_OPEN_ORDERS":
            c.req_all_open_orders()
        elif name == "REQ_GLOBAL_CANCEL":
            c.req_global_cancel()
        elif name == "CANCEL_POSITIONS":
            c.cancel_positions()
        elif name == "REQ_CONTRACT_DATA":
            c.req_contract_details(rid, make_contract(q["contract"]))
        elif name == "REQ_TICK_BY_TICK_DATA":
            c.req_tick_by_tick_data(rid, make_contract(q["contract"]), q.get("tickType", ""), q.get("numberOfTicks", 0),
                                    q.get("ignoreSize", False))
        elif name == "REQ_HISTORICAL_TICKS":
            c.req_historical_ticks(rid, make_contract(q["contract"]), q.get("startDateTime", ""), q.get("endDateTime", ""),
                                   q.get("numberOfTicks", 0), q.get("whatToShow", ""), int(q.get("useRTH", False)),
                                   q.get("ignoreSize", False), [])
        elif name == "CANCEL_TICK_BY_TICK_DATA":
            c.cancel_tick_by_tick_data(rid)
        elif name == "REQ_REAL_TIME_BARS":
            c.req_real_time_bars(rid, make_contract(q["contract"]), q.get("barSize", 5), q.get("whatToShow", ""),
                                 int(q.get("useRTH", False)), [])
        elif name == "CANCEL_REAL_TIME_BARS":
            c.cancel_real_time_bars(rid)
        elif name == "REQ_OPEN_ORDERS":
            c.req_open_orders()
        elif name == "REQ_COMPLETED_ORDERS":
            c.req_completed_orders(q.get("apiOnly", False))
        elif name == "REQ_SMART_COMPONENTS":
            c.req_smart_components(rid, q.get("bboExchange", ""))
        elif name == "REQ_ACCT_DATA":
            c.req_account_updates(q.get("subscribe", False), q.get("acctCode", ""))
        elif name == "REQ_PNL":
            c.req_pnl(rid, q.get("account", ""), q.get("modelCode", ""))
        elif name == "CANCEL_PNL":
            c.cancel_pnl(rid)
        elif name == "REQ_PNL_SINGLE":
            c.req_pnl_single(rid, q.get("account", ""), q.get("modelCode", ""), q.get("conId", 0))
        elif name == "CANCEL_PNL_SINGLE":
            c.cancel_pnl_single(rid)
        elif name == "REQ_SCANNER_SUBSCRIPTION":
            s = q.get("scannerSubscription", {})
            sub = type("ScannerSubscription", (), {})()
            sub.numberOfRows = s.get("numberOfRows", -1)
            for k in ("instrument", "locationCode", "scanCode"):
                setattr(sub, k, s.get(k, ""))
            c.req_scanner_subscription(rid, sub, [], [])
        elif name == "CANCEL_SCANNER_SUBSCRIPTION":
            c.cancel_scanner_subscription(rid)
        elif name == "REQ_HEAD_TIMESTAMP":
            c.req_head_time_stamp(rid, make_contract(q["contract"]), q.get("whatToShow", ""), int(q.get("useRTH", False)),
                                  q.get("formatDate", 1))
        elif name == "CANCEL_HEAD_TIMESTAMP":
            c.cancel_head_time_stamp(rid)
        elif name == "REQ_MKT_DEPTH":
            c.req_mkt_depth(rid, make_contract(q["contract"]), q.get("numRows", 0), q.get("isSmartDepth", False), [])
        elif name == "CANCEL_MKT_DEPTH":
            c.cancel_mkt_depth(rid, q.get("isSmartDepth", False))
        elif name == "REQ_EXECUTIONS":
            f = q.get("executionFilter", {})
            filt = type("ExecutionFilter", (), {})()
            filt.clientId = f.get("clientId", 0)
            for k in ("acctCode", "time", "symbol", "secType", "exchange", "side"):
                setattr(filt, k, f.get(k, ""))
            c.req_executions(rid, filt)
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
    `known`): the first STP limit price."""
    f = line.split("|")
    if f[0] == "openOrder" and len(f) > 5 and "orderType=STP," in f[5]:
        f[5] = ",".join("lmtPrice=-" if kv.startswith("lmtPrice=") else kv for kv in f[5].split(","))
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


# The closed-market scenarios recorded again on 10.51 with the official client
# at its own level, 225 (18/10/2026, ibx#604), next to their 10.40 recordings.
CLOSED = ["gw1051/20261018", "20260926"]


# A LMT order before the open, then its cancel (26/09/2026, ibx#472, ibx#465).
@pytest.mark.parametrize("day", CLOSED)
def test_lmt_order_then_cancel(day):
    out = replay(f"{day}/lmt_cancel", compare=["order"])
    assert out["frames_compared"] == 2
    assert len(assert_same(out)) == 9


# Two orders in one OCA group, both cancelled (26/09/2026, ibx#311, ibx#329).
@pytest.mark.parametrize("day", CLOSED)
def test_oca_group_and_the_refused_second_cancel(day):
    out = replay(f"{day}/oca_group", compare=["order"])
    assert out["frames_compared"] == 3
    theirs = assert_same(out)
    assert any("|10148|" in l for l in theirs)


# The cancel of an unknown order (26/09/2026, ibx#464).
@pytest.mark.parametrize("day", CLOSED)
def test_cancel_of_an_unknown_order(day):
    out = replay(f"{day}/cancel_unknown", compare=["order"])
    assert assert_same(out)[0].startswith("error|")


# A far LMT, its cancel, then a modify of the cancelled order (10.51 at level
# 225, market closed, 18/10/2026, ibx#604): PendingCancel twice, Cancelled,
# 202, then 104 and nothing sent.
def test_cancel_then_modify_of_the_cancelled_order():
    out = replay("gw1051/20261018/modify_cancelled", compare=["order"])
    assert out["frames_compared"] == 2
    theirs = assert_same(out)
    assert [l.split("|")[2] for l in theirs if l.startswith("orderStatus|")] ==         ["PreSubmitted", "PendingCancel", "PendingCancel", "Cancelled"]
    assert theirs[-1].startswith("error|") and "|104|" in theirs[-1]


# A bracket in the client library's form, as the Rust test.
@pytest.mark.parametrize("name", ["gw1051/20261018/bracket", "gw1051/20261018b/bracket"])
def test_bracket(name):
    out = replay(name, compare=["order"])
    assert out["frames_compared"] == 4
    assert_same(out)


# Combo orders on 10.51 (18/10/2026, ibx#604): the first of the gateway's
# process with its set-up, then, the definitions known, another one and one
# with per-leg prices, whose modify sends the new leg prices.
def test_combo_orders_on_10_51():
    first = "gw1051/20261018/i105_combo_stock_smart"
    assert any("|399|" in l for l in assert_same(replay(first)))
    for name in ["gw1051/20261018b/i105_combo_stock_smart", "gw1051/20261018b/i105_combo_leg_prices"]:
        out = replay(name, replies_from=first)
        assert out["frames_compared"] >= 2, name
        assert_same(out)


# Orders of client 0 of earlier sessions known from the logon replay
# (01/10/2026, paper), then client 193: reqAllOpenOrders lists them in the
# book's order (order id 0, client 0), reqGlobalCancel sends the 8 cancels
# tagged ALL in that order and gives client 193 nothing of their reports, as
# the Rust test (tests/scenario_replay.rs).
@pytest.mark.parametrize("name", ["gw1051/20261019rth/global_cancel_replayed", "20261001/global_cancel_replayed"])
def test_global_cancel_of_orders_of_earlier_sessions(name):
    out = replay(name, compare=["order"])
    assert out["frames_compared"] == 8
    assert len(assert_same(out)) == 18


# A SMART combo bought and sold (30/09/2026, ibx#474, ibx#471): the fills of
# the combo and its legs, commissions. As the Rust test: QQQ's top of book
# and the positions are left out (the reference's session state).
@pytest.mark.parametrize("name", ["gw1051/20261019rth/i105_combo_fill", "20260930/i105_combo_fill"])
def test_combo_fill_executions_and_commissions(name):
    out = replay(name, compare=["order"])
    theirs = assert_same(out, keep=lambda l: not l.startswith("position") and l.split("|")[1:2] != ["9501"])
    assert any(l.startswith("execDetails|") for l in theirs)
    assert any(l.startswith("commissionAndFeesReport|") for l in theirs)


# Partial fills in RTH (14/10/2026, the reference 10.51 with the client at
# level 214, ibx#515): three rounds of a marketable limit buying 300 F and
# its sale, the second sale filled in two executions, reqExecutions after each
# buy. As the Rust test: the replay stops before the closing global cancel.
def test_partial_fills_then_executions():
    out = replay("gw1051/20261014/partial_fills", compare=["order"], until=2137)
    theirs = assert_same(out)
    assert sum(1 for l in theirs if l.startswith("execDetails|")) == 17
    assert sum(1 for l in theirs if l.startswith("execDetails|-1|") and "|SLD|200|" in l) == 1


# The modify of a cancelled order, with its answer (14/10/2026 in RTH, the
# reference 10.51 at level 214, ibx#515): Cancelled and 202, then 104 for the
# modify and nothing sent. As the Rust test: the server reported the order
# pending then working in one run, working twice in the other.
@pytest.mark.parametrize("name,first", [("modify_cancelled", "PreSubmitted"),
                                        ("modify_cancelled_no_pending", "Submitted")])
def test_modify_of_a_cancelled_order(name, first):
    out = replay(f"gw1051/20261014/{name}", compare=["order"])
    assert out["frames_compared"] == 2
    theirs = assert_same(out)
    assert [l.split("|")[2] for l in theirs if l.startswith("orderStatus|")] == [first, "Submitted", "Cancelled"]
    assert theirs[-1].startswith("error|") and "|104|" in theirs[-1]


# A market-to-limit BUY 1 SPY filled before its cancel, then a market SELL 1
# back, with reqPositions running (15/10/2026 in RTH, the reference 10.51,
# ibx#515, ibx#707). As the Rust test: the first answer's rows compared as a
# set, the average costs with all their digits (ibx#708).
def test_fill_then_positions():
    out = replay("gw1051/20261015/fill_then_positions", compare=["order"], positions_of_the_recording=True)
    assert out["frames_compared"] == 2

    def split(lines):
        end = lines.index("positionEnd")
        return sorted(lines[:end]), lines[end:]

    assert out["frame_error"] is None, out["frame_error"]
    ours_first, ours_rest = split([known(l) for l in out["ours"]])
    theirs_first, theirs_rest = split([known(l) for l in out["theirs"]])
    assert ours_first == theirs_first
    for k, (a, b) in enumerate(zip(ours_rest, theirs_rest)):
        assert a == b, f"callback {k}:\n  ours      {a}\n  reference {b}"
    assert len(ours_rest) == len(theirs_rest)
    assert sum(1 for l in ours_rest if l.startswith("position|")) == 5


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
    # 10.51 at level 225 (18/10/2026, ibx#604): the cancel sends nothing; the
    # server's cancel at the disconnect is not replayed.
    out = replay("gw1051/20261018/account_summary", compare=["account_subscription"], until=2717)
    assert out["frame_error"] is None and out["frames_compared"] == 1
    assert out["ours"][:4] == out["theirs"][:4]


# The whole answer (ibx#486): the tag rows, the $LEDGER:ALL rows of the
# account "All" per currency, the end; each frame twice, as the reference
# in a process that had served account updates earlier (ibx#752).
def test_account_summary_whole_answer():
    out = replay("20260926/account_summary", after_account_updates=True)
    theirs = assert_same(out)
    assert sum(l.startswith("accountSummaryEnd|") for l in theirs) == 2
    assert any(l.startswith("accountSummary|9002|All|CashBalance|933115.05|USD") for l in theirs)


# The whole answer on 10.51 at level 225, in a gateway process where no
# session had asked the account updates (18/10/2026, ibx#604): each frame once.
def test_account_summary_whole_answer_on_10_51():
    theirs = assert_same(replay("gw1051/20261018c/account_summary", until=1413))
    assert sum(l.startswith("accountSummaryEnd|") for l in theirs) == 1


# The same after an earlier session of the process had asked the account
# updates: each frame twice (ibx#752).
def test_account_summary_whole_answer_after_account_updates():
    theirs = assert_same(replay("gw1051/20261018/account_summary", until=2717, after_account_updates=True))
    assert sum(l.startswith("accountSummaryEnd|") for l in theirs) == 2


# A combo whose BAG symbol is not in the legs' order (10.51 at level 225,
# 18/10/2026, ibx#604): 478.
def test_combo_symbol_conflict_on_10_51():
    theirs = assert_same(replay("gw1051/20261018c/i105_combo_symbol_conflict"))
    assert sum("|478|" in l for l in theirs) == 1


# A combo directed to ARCA with no definition (26/09/2026): 200, then its
# cancel gives orderStatus ApiCancelled (the order id stays pending)
# (ibx#487).
def test_directed_combo_without_definition_then_cancel():
    out = replay("20260926b/i105_combo_directed")
    theirs = assert_same(out)
    assert any(l.startswith("orderStatus|31|ApiCancelled|") for l in theirs)
    # 10.51 at level 225, market closed (18/10/2026, ibx#604).
    theirs = assert_same(replay("gw1051/20261018b/i105_combo_directed"))
    assert any("|200|" in l for l in theirs) and any("|ApiCancelled|" in l for l in theirs)


# keepUpToDate bars then their cancel (26/09/2026, ibx#429, ibx#431).
@pytest.mark.parametrize("name", ["gw1051/20261018/hist_keep_up_to_date", "gw1051/20261018b/hist_keep_up_to_date",
                                  "20260926b/hist_keep_up_to_date"])
def test_historical_keep_up_to_date_then_cancel(name):
    out = replay(name)
    assert len(assert_same(out)) == 32


# The pre-market order types on 10.51 at level 225 (19/10/2026, ibx#604): GTX
# and NMIN refused at once with 10052, the other orders and their reports as
# the reference. The market data requests between the orders are left out.
def test_premarket_order_types_on_10_51():
    import pathlib
    name = "gw1051/20261019pre/premarket_order_types"
    path = pathlib.Path(__file__).resolve().parents[1] / "fixtures/gw1051/scenarios/20261019pre/premarket_order_types"
    md = [r["seq"] for r in map(json.loads, open(f"{path}.api.jsonl", encoding="utf-8"))
          if r.get("msg_name") in ("REQ_MKT_DATA", "CANCEL_MKT_DATA", "REQ_MARKET_DATA_TYPE")]
    keep = lambda l: not l.startswith("marketDataType") and not l.startswith("tick") and not l.startswith("error|9003|")
    theirs = assert_same(replay(name, compare=["order"], skip_seqs=md), keep=keep)
    assert any("|10052|Invalid time in force:GTX" in l for l in theirs)
    assert any("|10052|Invalid time in force:NMIN" in l for l in theirs)


# Historical ticks of AAPL (02/10/2026 on 10.40, 19/10/2026 on 10.51 at
# level 225, pre-market, ibx#432, ibx#604): on 10.51 the trades the server
# flags fractional are given in their fractional step (0.01734). The EUR.USD
# requests go to the cash farm, which the replay does not run: left out.
@pytest.mark.parametrize("name", ["gw1051/20261019b1/b1_432_hist_ticks", "20261002/b1_432_hist_ticks"])
def test_historical_ticks(name):
    import pathlib
    root = pathlib.Path(__file__).resolve().parents[1] / "fixtures"
    path = root / ("gw1051/scenarios/" + name[len("gw1051/"):] if name.startswith("gw1051/") else "gw1040/scenarios/" + name)
    eur = [r["seq"] for r in map(json.loads, open(f"{path}.api.jsonl", encoding="utf-8"))
           if r.get("msg_name") == "REQ_HISTORICAL_TICKS" and r["request"]["contract"]["symbol"] == "EUR"]
    theirs = assert_same(replay(name, skip_seqs=eur))
    assert any(l.startswith("historicalTicksLast|") for l in theirs)
    if name.startswith("gw1051/"):
        assert any("'size': '0.01734'" in l or '"size": "0.01734"' in l or "0.01734" in l for l in theirs)


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


# Generic ticks (05/10/2026, ibx#450): AAPL with sixteen generic ticks, SPY
# with mdoff, 233 and 236, an invalid list (321), EUR.USD with 233 on the cash
# farm, MNQ with 588 on the futures farm: the entries on the wire and every
# API tick of the blocks, as the Rust test.
def test_generic_ticks():
    out = replay("20261005/b2_generic", farms=["cashfarm", "usfuture"])
    theirs = assert_same(out)
    assert out["frames_compared"] > 10
    for prefix in ("tickGeneric|9620|46|", "tickSize|9620|87|", "tickString|9620|59|", "tickString|9621|48|",
                   "tickSize|9625|86|"):
        assert any(l.startswith(prefix) for l in theirs), prefix


# Market data errors (05/10/2026, ibx#444): two ids on AAPL, then 7203 refused
# on the Tokyo farm (354 with its contract, 10167 and delayed data, the kept
# request parameters), as the Rust test; the exchange letters of AAPL (the
# reference had its map from earlier in its session) are left out.
def test_market_data_errors():
    out = replay("20261005/b2_mkt_errors", farms=["jfarm"], skip_seqs=[18512], compare=[])
    theirs = assert_same(out, keep=lambda l: not (l.startswith("tickString|") and l.split("|")[2] in ("32", "33")))
    assert any("|354|" in l and l.endswith("7203 TSEJ (7203.T) /TOP/ALL") for l in theirs)
    assert any(l.startswith("error|9654|10167|") for l in theirs)


# Tick-by-tick types, past ticks, ignoreSize, EUR.USD (the reference's query
# on the cash farm), an unknown type (05/10/2026, ibx#404, ibx#455), as the
# Rust test.
def test_tick_by_tick_types():
    out = replay("20261005/b2_tbt", hmds_farms=["cashfarm"])
    theirs = assert_same(out)
    assert any(l.startswith("historicalTicksLast|9605|") for l in theirs)
    assert any(l.startswith("tickByTickMidPoint|9608|") for l in theirs)


# Real-time bars shared by four requests, empty bars, the cancel of one
# (05/10/2026, ibx#454).
def test_real_time_bars_shared():
    out = replay("20261005/b2_rtbars")
    theirs = assert_same(out)
    assert sum(l.startswith("realtimeBar|9642|") for l in theirs) > 3


# Plain TRAIL orders: openOrder shows the stop price each report gives
# (05/10/2026, ibx#491). The reqOpenOrders are left out, as in the Rust test
# (the book's order goes by permId).
def test_plain_trail_follows_the_server():
    out = replay("20261005/b2_trail", compare=["order"], skip_seqs=[s + k for s in (15585, 15768, 15964, 16182, 16349, 16522) for k in range(6)])
    theirs = assert_same(out)
    assert any("trailStopPrice=775.06" in l for l in theirs)


# The regular-hours family b2 recorded again on 10.51 at level 225
# (19/10/2026, ibx#604), as the Rust tests.
def api_seqs(name, keep):
    """The seqs of the decoded API records of a 10.51 recording that `keep` takes."""
    import pathlib
    path = pathlib.Path(__file__).resolve().parents[1] / "fixtures/gw1051/scenarios" / (name[len("gw1051/"):] + ".api.jsonl")
    return [r["seq"] for r in map(json.loads, open(path, encoding="utf-8")) if "seq" in r and keep(r)]


def test_b2_on_10_51():
    b2 = "gw1051/20261019b2"
    out = replay(f"{b2}/b2_generic", farms=["cashfarm", "usfuture"])
    assert_same(out)
    assert out["frames_compared"] > 10
    assert any(l.startswith("historicalTicksLast|") for l in assert_same(replay(f"{b2}/b2_tbt", hmds_farms=["cashfarm"])))
    assert any(l.startswith("realtimeBar|9642|") for l in assert_same(replay(f"{b2}/b2_rtbars")))
    listings = [s + k for s in api_seqs(f"{b2}/b2_trail", lambda r: r.get("msg_name") == "REQ_OPEN_ORDERS") for k in range(6)]
    assert len(listings) == 36
    assert_same(replay(f"{b2}/b2_trail", compare=["order"], skip_seqs=listings))
    # The 7203 requests: the reference had kept AAPL's quote and trade from
    # the earlier scenarios of its process (the Rust test).
    aapl = api_seqs(f"{b2}/b2_mkt_errors", lambda r: r.get("msg_name") in ("REQ_MKT_DATA", "CANCEL_MKT_DATA")
                    and r["request"].get("reqId") in (9650, 9651))
    theirs = assert_same(replay(f"{b2}/b2_mkt_errors", farms=["jfarm"], skip_seqs=aapl, compare=[]),
                         keep=lambda l: l.split("|")[1] not in ("9650", "9651"))
    assert any("|354|" in l for l in theirs)


# ── The replays of tests/scenario_replay.rs the Python client had not (ibx#515) ──

def scenario_path(name):
    import pathlib
    root = pathlib.Path(__file__).resolve().parents[1] / "fixtures"
    if name.startswith("gw1051/"):
        return root / "gw1051/scenarios" / name[len("gw1051/"):]
    return root / "gw1040/scenarios" / name


def seqs(name, keep):
    """The seqs of the decoded API records of a recording that `keep` takes."""
    return [r["seq"] for r in map(json.loads, open(f"{scenario_path(name)}.api.jsonl", encoding="utf-8"))
            if "seq" in r and keep(r)]


def md_seqs(name):
    return seqs(name, lambda r: r.get("msg_name") in ("REQ_MKT_DATA", "CANCEL_MKT_DATA", "REQ_MARKET_DATA_TYPE"))


def same_lines(ours, theirs):
    for k, (a, b) in enumerate(zip(ours, theirs)):
        assert a == b, f"callback {k}:\n  ours      {a}\n  reference {b}"
    assert len(ours) == len(theirs), (ours[len(theirs):], theirs[len(ours):])


# A LMT order, its cancel, a modify after the cancel (26/09/2026): the
# answer to the modify is not in the recording, its request left out.
def test_cancel_gives_pending_cancel_then_cancelled():
    theirs = assert_same(replay("20260926/modify_cancelled", compare=["order"], skip_seqs=[2441]))
    assert any(l.startswith("orderStatus|2|PendingCancel") for l in theirs)


# The pre-market order types on 10.40 (28/09/2026): the fill and the 104 for
# the modify of the filled order.
def test_premarket_order_types_on_10_40():
    name = "20260928/premarket_order_types"
    keep = lambda l: not l.startswith("marketDataType") and not l.startswith("tick") and not l.startswith("error|9003|")
    out = replay(name, compare=["order"], skip_seqs=md_seqs(name))
    theirs = assert_same(out, keep=keep)
    assert out["frames_compared"] >= 23
    assert any("|104|Cannot modify a filled order" in l for l in theirs)
    assert any(l.startswith("execDetails|") for l in theirs)


# The fill and the modify of the same scenario on 10.51 (19/10/2026).
def test_premarket_fill_then_modify_on_10_51():
    name = "gw1051/20261019preb/premarket_fill_modify"
    keep = lambda l: not l.startswith("marketDataType") and not l.startswith("tick")
    theirs = assert_same(replay(name, compare=["order"], skip_seqs=md_seqs(name)), keep=keep)
    assert any("|104|Cannot modify a filled order" in l for l in theirs)
    assert sum(1 for l in theirs if l.startswith("execDetails|")) == 2


# AAPL top of book after the orders on AAPL (the market data part of the
# pre-market scenario).
@pytest.mark.parametrize("name,skip,until", [
    ("gw1051/20261019pre/premarket_order_types", [2167, 2168, 2706, 2707], 2773),
    ("20260928/premarket_order_types", [21961, 21962, 22575, 22576], 22599)])
def test_top_of_book_after_orders_on_the_contract(name, skip, until):
    assert_same(replay(name, skip_seqs=skip, until=until), keep=lambda l: l.split("|")[1:2] not in (["9002"], ["9003"]))


# OVERNIGHT time in force (28/09/2026; 19/10/2026 on 10.51).
def test_overnight_time_in_force():
    theirs = assert_same(replay("20260928/overnight_tif", compare=["order"]))
    assert any(l.startswith("error|2|10329|") for l in theirs)
    for name in ["i196_overnight_smart", "i196_overnight_exch", "i196_overnight_day"]:
        theirs = assert_same(replay(f"gw1051/20261019ov/{name}", compare=["order"]))
        assert any("|10052|" in l for l in theirs), name
    assert_same(replay("gw1051/20261019ov/i196_day_include_overnight", compare=["order"]))


# The session's order types in regular hours (28/09/2026; 19/10/2026 on
# 10.51), as the Rust test.
@pytest.mark.parametrize("name,first,until", [("gw1051/20261019rth/rth_order_types", 15, 2217),
                                              ("20260928/rth_order_types", 1, 6581)])
def test_rth_order_types(name, first, until):
    opts = {"skip_frame": "exchange_map"} if name.startswith("gw1051/") else {}
    out = replay(name, skip_orders=list(range(first, first + 7)), until=until, **opts)
    assert_same(out, keep=lambda l: not l.startswith("commissionAndFeesReport"))
    assert out["frames_compared"] >= 10


# A SMART stock combo whose symbol does not match its legs (26/09/2026): 478
# twice.
def test_combo_symbol_conflict():
    theirs = assert_same(replay("20260926/i105_combo_stock_smart"))
    assert sum("|478|" in l for l in theirs) == 2


# The combo orders of the second session (26/09/2026), with the first
# session's definitions.
@pytest.mark.parametrize("name", ["20260926b/i105_combo_stock_smart", "20260926b/i105_combo_leg_prices"])
def test_combo_orders_of_the_second_session(name):
    assert_same(replay(name, replies_from="20260926/i105_combo_stock_smart"))


# The bracket of 10.40 (26/09/2026), as on 10.51.
@pytest.mark.parametrize("name", ["20260926/bracket", "20260926b/bracket"])
def test_bracket_on_10_40(name):
    assert_same(replay(name, compare=["order"]))


# Four keepUpToDate requests: their bars, ends and first updates.
@pytest.mark.parametrize("name,until", [("gw1051/20261019b1/b1_429_keep_up_to_date", 3770),
                                        ("20261002/b1_429_keep_up_to_date", 3650)])
def test_keep_up_to_date_bars_and_first_updates(name, until):
    theirs = assert_same(replay(name, until=until))
    assert any(l.startswith("historicalDataUpdate|9533|") for l in theirs)


# The later rounds of updates in the order the requests joined the stream,
# the lookups answered at their recorded points (ibx#512).
@pytest.mark.parametrize("name", ["gw1051/20261019b1/b1_429_keep_up_to_date", "20261002/b1_429_keep_up_to_date"])
def test_keep_up_to_date_update_order(name):
    theirs = assert_same(replay(name, lookups_at_their_point=True), keep=lambda l: not l.startswith("error|9536|366|"))
    assert any(l.startswith("error|9535|366|") for l in theirs)


# Bars with formatDate 1 and 2, head timestamps, the second from the store.
@pytest.mark.parametrize("name", ["gw1051/20261019b1/b1_431_hist_format", "20261002/b1_431_hist_format"])
def test_historical_formats_and_head_timestamps(name):
    theirs = assert_same(replay(name))
    assert any(l.startswith("headTimestamp|9545|") for l in theirs)


# Two scanner subscriptions, then their cancels (26/09/2026; 18/10/2026 on
# 10.51, the parameters asked first).
def test_two_scanner_subscriptions():
    keep = lambda l: not l.startswith("error|9006|162|")
    out = replay("20260926b/scanner_two", compare=["scanner", "lookup"])
    assert_same(out, keep=keep)
    assert out["frames_compared"] == 24
    out = replay("gw1051/20261018/scanner_two", compare=["scanner", "lookup"])
    assert_same(out, keep=keep)
    assert out["frames_compared"] > 20


# News ticks on a contract asked twice, then on a second contract with one
# provider (02/10/2026; 19/10/2026 on 10.51); the cancels in the paced replay
# of the 10.51 recording.
def test_news_ticks_twice():
    keep = lambda l: not l.startswith("marketDataType|") and not l.startswith("tickReqParams|")
    opts = {"compare": ["market_data", "historical"], "skip_frame": "exchange_map"}
    for name, until in [("gw1051/20261019b1/b1_458_news_dup", 16439), ("20261002/b1_458_news_dup", 15737)]:
        out = replay(name, until=until, **opts)
        assert_same(out, keep=keep)
        assert out["frames_compared"] == 4, name
    out = replay("gw1051/20261019b1/b1_458_news_dup", paced=True, **opts)
    assert_same(out, keep=keep)
    assert out["frames_compared"] == 8


def generic_line(l):
    f = l.split("|")
    if f[0] == "error":
        return f[2] != "2172"
    return f[0] in ("tickPrice", "tickSize", "tickString", "tickGeneric")


# One request per generic tick on AAPL before the open (07/10/2026; 19/10/2026
# on 10.51): every error and tick.
@pytest.mark.parametrize("name", ["gw1051/20261019b4/b4_generic_rest", "20261007/b4_generic_rest"])
def test_generic_ticks_one_by_one(name):
    out = replay(name)
    assert out["frame_error"] is None, out["frame_error"]
    ours = [l for l in out["ours"] if generic_line(l)]
    same_lines(ours, [l for l in out["theirs"] if generic_line(l)])
    if name.startswith("2026"):
        assert (out["frames_compared"], len(ours)) == (36, 190)


# The same ticks in one list on AAPL, up to the SPY request; on 10.51 the list
# refused with 10358.
@pytest.mark.parametrize("name,until", [("gw1051/20261019b4/b4_generic_rest2", 8213), ("20261007/b4_generic_rest2", 9363)])
def test_generic_ticks_in_one_list(name, until):
    def kept(l):
        f = l.split("|")
        return (f[0] == "error" and f[2] != "2172") or (f[0] == "tickPrice" and f[2] == "37") \
            or (f[0] == "tickSize" and f[2] in ("63", "64", "65"))
    out = replay(name, until=until, frame_mask="without_fundamentals_and_map")
    assert out["frame_error"] is None, out["frame_error"]
    ours = [l for l in out["ours"] if kept(l)]
    same_lines(ours, [l for l in out["theirs"] if kept(l)])
    if name.startswith("gw1051/"):
        assert any(l.startswith("error|9740|10358|") for l in ours)
    else:
        assert len(ours) >= 5


# The same list without 258 on 10.51: every tick and error.
def test_generic_ticks_in_one_list_on_10_51():
    keep = lambda l: l.split("|")[0] in ("error", "tickPrice", "tickSize", "tickString", "tickGeneric")
    theirs = assert_same(replay("gw1051/20261019b4l/b4_generic_rest3"), keep=keep)
    assert any(l.startswith("tickPrice|9790|37|") for l in theirs)


def sorted_between(lines, first, end):
    lines = list(lines)
    a = next((k for k, l in enumerate(lines) if l.startswith(first)), None)
    b = next((k for k, l in enumerate(lines) if l == end), None)
    if a is not None and b is not None:
        lines[a:b] = sorted(lines[a:b])
    return lines


# Session start (07/10/2026; 19/10/2026 on 10.51): the logon burst, then the
# first client's requests, as the Rust test. Left out: 2172, the P&L
# callbacks and the P&L partition's account request (ibx#757); the completed orders'
# rows without `completed`, as a set with it (ibx#695).
@pytest.mark.parametrize("completed", [False, True])
@pytest.mark.parametrize("name", ["gw1051/20261019b4s/session_start", "20261007/session_start"])
def test_session_start(name, completed):
    def keep(l):
        if l.startswith(("error|-1|2172|", "pnl|", "pnlSingle|", "error|9831|2150|")):
            return False
        return completed or not l.startswith("completedOrder|")
    out = replay(name, compare=["order", "account_subscription"], skip_frame="core_model")
    assert out["frame_error"] is None, out["frame_error"]
    prep = lambda v: sorted_between(sorted_between([l for l in v if keep(l)], "position|", "positionEnd"),
                                    "completedOrder|", "completedOrdersEnd")
    same_lines(prep(out["ours"]), prep(out["theirs"]))


# A market-to-limit order cancelled during the lookups of the contract's
# first order (15/10/2026, ibx#706): ApiCancelled, nothing sent; the sale
# after. The lookups answered at their recorded points.
@pytest.mark.parametrize("paced", [False, True])
def test_cancel_during_the_first_orders_lookups(paced):
    out = replay("gw1051/20261015/cancel_during_lookup", compare=["order"], positions_of_the_recording=True,
                 lookups_at_their_point=True, paced=paced)
    assert out["frame_error"] is None, out["frame_error"]
    assert out["frames_compared"] == 1
    ours, theirs = [known(l) for l in out["ours"]], [known(l) for l in out["theirs"]]
    a, b = ours.index("positionEnd"), theirs.index("positionEnd")
    assert sorted(ours[:a]) == sorted(theirs[:b])
    same_lines(ours[a:], theirs[b:])
    assert any(l.startswith("orderStatus|") and "|ApiCancelled|" in l for l in ours)


# The first P&L requests of a session (15/10/2026, ibx#511): the quotes, two
# exchange maps and their cancels; the first value to the last digit.
def test_pnl_first_request_of_the_session():
    out = replay("gw1051/20261015/pnl_first_request", compare=["market_data"], skip_frame="dividends_cancel")
    assert out["frame_error"] is None, out["frame_error"]
    assert out["frames_compared"] == 5
    assert out["ours"][0].split("|")[:5] == out["theirs"][0].split("|")[:5]

# The same replayed on the recording's time, the client computing on it too
# (ibx#515), its first computation as long after the request as the
# reference's (ibx#757): the seventeen callbacks of the two requests equal to
# the last digit.
def test_pnl_on_the_recordings_time():
    out = replay("gw1051/20261015/pnl_first_request", compare=["market_data"], skip_frame="dividends_cancel", paced=True)
    assert out["frame_error"] is None, out["frame_error"]
    assert len(out["theirs"]) == 17
    assert_same(out)
