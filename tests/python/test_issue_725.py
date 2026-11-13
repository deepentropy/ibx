"""ibx#725: the protobuf form of the requests and callbacks of the Python client.

- Requests: each plain request of the official client library is sent by that library
  in its protobuf form; what it sends is given to the ibx client (the bytes to
  ``send_msg_proto_buf``, the message to the request's ``..._proto_buf`` method) and
  must make the same plain request, with the same arguments, as the plain form.
- Callbacks: the protobuf message ibx makes for a plain callback is read by the
  official decoder, which must call the protobuf callback and then the plain callback
  with the same arguments.

Both need the official client library and are skipped without it; the last tests run
without it.
"""
import inspect
import re
from decimal import Decimal

import pytest

import ibx
import official_names as on

forms = ibx.proto_forms
T = forms.T


def official():
    pytest.importorskip("ibapi.protobuf")
    import ibapi.client
    import ibapi.decoder
    import ibapi.wrapper
    return ibapi


# ── Tables ──

def test_the_tables_are_those_of_the_installed_library():
    """scripts/proto_forms_table.py has to be run again when the official library changes."""
    ibapi = official()
    assert T.LIBRARY == ibapi.get_version_string()
    for message, fields in T.MESSAGES.items():
        cls = forms.official(message)
        assert cls is not None, message
        assert [f.name for f in cls.DESCRIPTOR.fields] == [name for name, _ in fields], message


def test_every_protobuf_request_and_callback_of_the_table_is_given_or_listed():
    requests = {name for name, *_ in T.REQUESTS.values()}
    missing = {n for n in requests if not hasattr(ibx.EClient, n)}
    assert missing == {n + "_proto_buf" for n in forms.NOT_GIVEN}
    callbacks = {spec[0] for spec in T.ANSWERS.values()}
    assert {n for n in callbacks if not hasattr(ibx.EWrapper, n)} == {
        "reroute_market_data_request_proto_buf", "reroute_market_depth_request_proto_buf"}
    # Each one answers to its official name.
    for name in (requests | callbacks) - missing:
        if hasattr(ibx.EClient, name) or hasattr(ibx.EWrapper, name):
            assert on.official_name(name) == T.OFFICIAL_NAMES[name]


# ── Requests ──

def same(a, b, path="value"):
    """`b` (what the protobuf form gave) holds what `a` (what the plain form gave) holds."""
    if isinstance(a, (list, tuple)):
        assert len(a) == len(b or ()), path
        for i, (x, y) in enumerate(zip(a, b)):
            same(x, y, f"{path}[{i}]")
    elif a is None or isinstance(a, (bool, int, float, str, Decimal)):
        if isinstance(a, bool) or isinstance(b, bool):
            assert bool(a) == bool(b), path
        else:
            assert a == b, f"{path}: {a!r} != {b!r}"
    else:
        fresh = vars(type(a)())
        for name, v in vars(a).items():
            if v != fresh.get(name):
                same(v, getattr(b, name), f"{path}.{name}")


def recorder():
    """An ibx client whose plain requests record their call."""
    calls = []

    def record(name):
        def call(self, *args, **kwargs):
            calls.append((name, args, kwargs))
        return call
    plain = {n: record(n) for n in on.REQUESTS if not n.endswith("_proto_buf") and n not in ("connect", "disconnect")}
    return type("Recorder", (ibx.EClient,), plain)(ibx.EWrapper()), calls


def sample(name, annotation):
    ibapi = official()
    from ibapi.common import WshEventData
    from ibapi.contract import ComboLeg, Contract, DeltaNeutralContract
    from ibapi.execution import ExecutionFilter
    from ibapi.order import Order, OrderComboLeg
    from ibapi.order_cancel import OrderCancel
    from ibapi.order_condition import OrderCondition, PriceCondition, TimeCondition
    from ibapi.scanner import ScannerSubscription
    from ibapi.softdollartier import SoftDollarTier
    from ibapi.tag_value import TagValue
    a = annotation or ""
    if name == "contract":
        c = Contract()
        c.conId, c.symbol, c.secType, c.exchange, c.currency, c.primaryExchange = 265598, "AAPL", "STK", "SMART", "USD", "NASDAQ"
        c.lastTradeDateOrContractMonth, c.strike, c.right, c.multiplier, c.localSymbol = "20261120", 250.0, "C", "100", "AAPL L"
        leg = ComboLeg()
        leg.conId, leg.ratio, leg.action, leg.exchange = 9579970, 2, "SELL", "SMART"
        c.comboLegs = [leg]
        dn = DeltaNeutralContract()
        dn.conId, dn.delta, dn.price = 756733, 0.5, 432.1
        c.deltaNeutralContract = dn
        return c
    if name == "order":
        o = Order()
        o.action, o.totalQuantity, o.orderType, o.lmtPrice, o.auxPrice, o.tif = "BUY", Decimal("3"), "LMT", 100.5, 99.0, "GTC"
        o.account, o.outsideRth, o.transmit, o.parentId, o.ocaGroup, o.goodTillDate = "DU1234567", True, False, 12, "oca", "20261231 10:00:00"
        o.algoStrategy, o.algoParams = "Adaptive", [TagValue("adaptivePriority", "Normal")]
        o.orderComboLegs = [OrderComboLeg()]
        o.orderComboLegs[0].price = 1.25
        o.smartComboRoutingParams = [TagValue("NonGuaranteed", "1")]
        o.orderMiscOptions = [TagValue("k", "v")]
        price = PriceCondition(1, 265598, "SMART", True, 200.0)
        price.isConjunctionConnection = False
        o.conditions = [price, TimeCondition(False, "20261231 10:00:00")]
        o.softDollarTier = SoftDollarTier("tier", "val", "shown")
        o.usePriceMgmtAlgo = True
        o.slOrderId, o.slOrderType, o.ptOrderId, o.ptOrderType = 31, "STP", 32, "LMT"
        assert OrderCondition.Price == 1
        return o
    if "ExecutionFilter" in a:
        f = ExecutionFilter()
        f.clientId, f.acctCode, f.symbol, f.side, f.lastNDays, f.specificDates = 7, "DU1234567", "AAPL", "BUY", 3, [20261112, 20261113]
        return f
    if "ScannerSubscription" in a:
        s = ScannerSubscription()
        s.instrument, s.locationCode, s.scanCode, s.numberOfRows, s.abovePrice = "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 5, 10.0
        return s
    if "OrderCancel" in a:
        c = OrderCancel()
        c.manualOrderCancelTime = "20261113 10:00:00"
        return c
    if "WshEventData" in a:
        w = WshEventData()
        w.conId, w.fillWatchlist, w.totalLimit = 265598, True, 20
        return w
    if "TagValueList" in a or name.endswith("Options"):
        return [TagValue("k", "v")]
    if name in ("faData", "faDataType", "override"):
        return 1
    if name in ("manualOrderTime", "customerAccount"):
        return ""
    if name == "professionalCustomer":
        return False
    if a == "bool" or name in ("snapshot", "regulatorySnapshot", "keepUpToDate", "isSmartDepth", "ignoreSize", "subscribe",
                                "ledgerAndNLV", "allMsgs", "bAutoBind", "apiOnly", "useRTH", "useRth"):
        return name not in ("snapshot", "regulatorySnapshot")
    if a == "float":
        return 1.5
    if a == "str":
        return "x"
    if a in ("int", "TickerId", "OrderId") or name.lower().endswith("id") or name in ("numRows", "numIds", "override", "formatDate"):
        return 7
    return "x"


def official_requests():
    """(official plain request, its arguments' names and annotations, the protobuf request it sends)."""
    ibapi = official()
    src = inspect.getsource(ibapi.client.EClient)
    out = []
    for m in re.finditer(r"\n    def (\w+)\(\s*self(.*?)\):(.*?)(?=\n    def |\Z)", src, re.S):
        name, _, body = m.groups()
        sent = re.search(r"self\.(?!useProtoBuf)(\w+ProtoBuf)\(", body)
        if name.endswith("ProtoBuf") or name == "useProtoBuf" or not sent:
            continue
        params = [(p.name, "" if p.annotation is p.empty else getattr(p.annotation, "__name__", str(p.annotation)))
                  for p in list(inspect.signature(getattr(ibapi.client.EClient, name)).parameters.values())[1:]]
        out.append((name, params, sent.group(1)))
    return out


def test_each_protobuf_request_makes_the_plain_request_of_the_plain_form():
    ibapi = official()

    class Capture(ibapi.client.EClient):
        """The official client, with what it sends kept instead of written to a socket."""

        def __init__(self):
            super().__init__(ibapi.wrapper.EWrapper())
            self.sent, self.extraAuth = [], True

        def isConnected(self):
            return True

        def serverVersion(self):
            return 225

        def sendMsgProtoBuf(self, msgId, msg):
            self.sent.append((msgId, msg))

    done = set()
    for name, params, proto_name in official_requests():
        if name in ("startApi", "connect") or name in on.NOT_IN_IBX:
            continue
        args = [sample(p, a) for p, a in params]
        capture = Capture()
        getattr(capture, name)(*args)
        assert len(capture.sent) == 1, f"{name}: the official library sent {len(capture.sent)} messages"
        msg_id, data = capture.sent[0]

        # The plain form, through the adapter of the official names.
        plain_client, plain_calls = recorder()
        app = on.EClient.__new__(on.EClient)
        app.ibx = plain_client
        getattr(app, name)(*args)
        assert len(plain_calls) == 1, name

        # The protobuf form: the bytes, then the message itself.
        entry = T.REQUESTS[msg_id - 200]
        assert T.OFFICIAL_NAMES[entry[0]] == proto_name
        for give in ("bytes", "message"):
            client, calls = recorder()
            if give == "bytes":
                client.send_msg_proto_buf(msg_id, data)
            else:
                proto = forms.official(entry[1])()
                proto.ParseFromString(data)
                getattr(client, entry[0])(proto)
            assert len(calls) == 1, f"{proto_name} ({give})"
            assert calls[0][0] == plain_calls[0][0], f"{proto_name} ({give})"
            assert not plain_calls[0][2] and not calls[0][2]
            same(list(plain_calls[0][1]), list(calls[0][1]), proto_name)
        done.add(entry[0])
    given = {n for n, *_ in T.REQUESTS.values() if hasattr(ibx.EClient, n)}
    # The two requests of the configuration have no plain form (ibx#610, ibx#612).
    assert given - done == {"req_config_proto_buf", "update_config_proto_buf"}


def test_a_request_reads_any_object_with_the_fields_of_the_message():
    class Fields:
        def __init__(self, **fields):
            self.__dict__.update(fields)

    client, calls = recorder()
    client.req_pnl_single_proto_buf(Fields(reqId=5, account="DU1234567", modelCode=None, conId=265598))
    assert calls == [("req_pnl_single", (5, "DU1234567", "", 265598), {})]
    calls.clear()
    client.req_contract_data_proto_buf(Fields(reqId=6, contract=Fields(conId=265598, symbol="AAPL", secType="STK", exchange="SMART")))
    name, (req_id, contract), _ = calls[0]
    assert (name, req_id, contract.conId, contract.symbol, contract.secType, contract.exchange) == (
        "req_contract_details", 6, 265598, "AAPL", "STK", "SMART")
    # The official argument name is accepted; a missing message is not a request (as in the official library).
    calls.clear()
    client.cancel_pnl_proto_buf(cancel_pn_l_proto=Fields(reqId=9))
    client.cancel_pnl_proto_buf(None)
    assert calls == [("cancel_pnl", (9,), {})]


def test_use_proto_buf_follows_the_session_level():
    c = ibx.EClient(ibx.EWrapper())
    assert c.use_proto_buf(3) is False          # not connected
    c._test_connect("DU1234567")
    try:
        assert c.server_version() == 225
        assert c.use_proto_buf(3) is True and c.use_proto_buf(1) is True
        assert c.use_proto_buf(999) is False
    finally:
        c.disconnect()


# ── Callbacks ──

class Calls:
    """A wrapper that records every callback."""

    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)

        def call(*args):
            self.calls.append((name, args))
        return call


def scalar(kind, conv, i):
    if kind == "i":
        return 7 + i
    if kind == "f":
        return 1.5 + i
    if kind == "b":
        return True
    if conv == "Decimal":
        return Decimal(3 + i)
    if conv == "float":
        return 0.01
    return f"text{i}"


def filled(cls, decoder, message_variable, variable, message, skip=()):
    """An ibx object with every attribute the official decoder reads set, and those values."""
    o, kinds, expected = cls(), forms.KINDS[message], {}
    for i, (mv, field, var, attr, conv) in enumerate(T.OBJECTS[decoder]):
        if mv != message_variable or var != variable or conv == "enum" or attr in skip:
            continue
        kind = kinds[field]
        if conv == "decimalMaxString":
            v = "1.25"
        elif conv == "floatMaxString":
            v = "100"
        elif conv == "bool01":
            v = 1
        else:
            v = scalar(kind, conv, i)
        setattr(o, attr, v)
        expected[attr] = v
    return o, expected


def check(theirs, expected, path):
    for attr, v in expected.items():
        got = getattr(theirs, attr)
        if isinstance(v, dict):
            check(got, v, f"{path}.{attr}")
        elif isinstance(v, list):
            assert got is not None and len(got) == len(v), f"{path}.{attr}: {got!r}"
            for i, (g, e) in enumerate(zip(got, v)):
                check(g, e, f"{path}.{attr}[{i}]") if isinstance(e, dict) else same(e, g, f"{path}.{attr}[{i}]")
        else:
            same(v, got, f"{path}.{attr}")


def contract():
    c, expected = filled(ibx.Contract, "decodeContract", "contractProto", "contract", "Contract")
    leg, leg_expected = filled(ibx.ComboLeg, "decodeComboLegs", "comboLegProto", "comboLeg", "ComboLeg")
    c.comboLegs = [leg]
    expected["comboLegs"] = [leg_expected]
    return c, expected


def order():
    o, expected = filled(ibx.Order, "decodeOrder", "orderProto", "order", "Order")
    o.orderId = expected["orderId"] = 41
    o.smartComboRoutingParams = [ibx.TagValue("NonGuaranteed", "1")]
    o.algoStrategy, o.algoParams = "Adaptive", [ibx.TagValue("adaptivePriority", "Normal")]
    o.conditions = [ibx.PriceCondition(1, 265598, "SMART", True, 200.0), ibx.TimeCondition(False, "20261231 10:00:00")]
    o.softDollarTier = ibx.SoftDollarTier("tier", "val", "shown")
    price = ibx.OrderComboLeg()
    price.price = 1.25
    o.orderComboLegs = [price]
    expected["smartComboRoutingParams"] = [{"tag": "NonGuaranteed", "value": "1"}]
    expected["algoStrategy"], expected["algoParams"] = "Adaptive", [{"tag": "adaptivePriority", "value": "Normal"}]
    expected["conditions"] = [{"conId": 265598, "exchange": "SMART", "isMore": True, "price": 200.0, "triggerMethod": 1},
                              {"isMore": False, "time": "20261231 10:00:00"}]
    expected["softDollarTier"] = {"name": "tier", "val": "val", "displayName": "shown"}
    expected["orderComboLegs"] = [{"price": 1.25}]
    return o, expected


def order_state():
    s, expected = filled(ibx.OrderState, "decodeOrderState", "orderStateProto", "orderState", "OrderState")
    a, a_expected = filled(ibx.OrderAllocation, "decodeOrderAllocations", "orderAllocationProto", "orderAllocation", "OrderAllocation")
    s.orderAllocations = [a]
    expected["orderAllocations"] = [a_expected]
    return s, expected


def details():
    cd, expected = filled(ibx.ContractDetails, "decodeContractDetails", "contractDetailsProto", "contractDetails", "ContractDetails")
    c, c_expected = contract()
    c.lastTradeDateOrContractMonth = c_expected["lastTradeDateOrContractMonth"] = "20261120"
    cd.contract, cd.lastTradeTime = c, "16:00:00"
    cd.secIdList = [ibx.TagValue("ISIN", "US0378331005")]
    reason = ibx.IneligibilityReason("id1", "why")
    cd.ineligibilityReasonList = [reason]
    expected.update(contract=c_expected, lastTradeTime="16:00:00", secIdList=[{"tag": "ISIN", "value": "US0378331005"}],
                    ineligibilityReasonList=[{"id_": "id1", "description": "why"}])
    return cd, expected


def bar():
    return filled(ibx.BarData, "decodeHistoricalDataBar", "historicalDataBarProto", "bar", "HistoricalDataBar")


def tick_last():
    t, expected = filled(ibx.HistoricalTickLast, "decodeHistoricalTickLast", "historicalTickLastProto", "historicalTickLast",
                         "HistoricalTickLast")
    t.tickAttribLast = ibx.TickAttribLast(True, False)
    expected["tickAttribLast"] = {"pastLimit": True, "unreported": False}
    return t, expected


def tick_bid_ask():
    t, expected = filled(ibx.HistoricalTickBidAsk, "decodeHistoricalTickBidAsk", "historicalTickBidAskProto", "historicalTickBidAsk",
                         "HistoricalTickBidAsk")
    t.tickAttribBidAsk = ibx.TickAttribBidAsk(False, True)
    expected["tickAttribBidAsk"] = {"bidPastLow": False, "askPastHigh": True}
    return t, expected


def tick():
    return filled(ibx.HistoricalTick, "decodeHistoricalTick", "historicalTickProto", "historicalTick", "HistoricalTick")


def with_objects():
    """ibx callback -> (its arguments, what the official plain callback must get: a value, or the attributes of an object)."""
    c, c_e = contract()
    o, o_e = order()
    s, s_e = order_state()
    cd, cd_e = details()
    b, b_e = bar()
    e, e_e = filled(ibx.Execution, "decodeExecution", "executionProto", "execution", "Execution")
    report = ibx.CommissionAndFeesReport()
    report.execId, report.commissionAndFees, report.currency, report.realizedPNL, report.yield_, report.yieldRedemptionDate = (
        "0001", 1.25, "USD", 3.5, 0.5, 20270101)
    depth = ibx.DepthMktDataDescription("NASDAQ", "STK", "NSDQ", "Deep2", 7)
    provider = ibx.NewsProvider()
    provider.code, provider.name = "BRFG", "Briefing"
    description = ibx.ContractDescription(265598, "AAPL", "STK", "USD", "NASDAQ", ["OPT", "WAR"], "Apple", "issuer")
    description_e = dict(conId=265598, symbol="AAPL", secType="STK", currency="USD", primaryExchange="NASDAQ", description="Apple",
                         issuerId="issuer")
    tl, tl_e = tick_last()
    tb, tb_e = tick_bid_ask()
    t, t_e = tick()
    return {
        "open_order": ((41, c, o, s), [41, c_e, o_e, s_e]),
        "completed_order": ((c, o, s), [c_e, o_e, s_e]),
        "update_portfolio": ((c, Decimal(5), 1.5, 2.5, 3.5, 4.5, 5.5, "DU1234567"), [c_e, Decimal(5), 1.5, 2.5, 3.5, 4.5, 5.5, "DU1234567"]),
        "position": (("DU1234567", c, Decimal(5), 1.5), ["DU1234567", c_e, Decimal(5), 1.5]),
        "position_multi": ((3, "DU1234567", "model", c, Decimal(5), 1.5), [3, "DU1234567", "model", c_e, Decimal(5), 1.5]),
        "contract_details": ((3, cd), [3, cd_e]),
        "exec_details": ((3, c, e), [3, c_e, e_e]),
        "commission_and_fees_report": ((report,), [dict(execId="0001", commissionAndFees=1.25, currency="USD", realizedPNL=3.5,
                                                         yield_=0.5, yieldRedemptionDate=20270101)]),
        "historical_data_update": ((3, b), [3, b_e]),
        "security_definition_option_parameter": ((3, "SMART", 265598, "AAPL", "100", ["20261120", "20261218"], [250.0, 255.0]),
                                                 [3, "SMART", 265598, "AAPL", "100", {"20261120", "20261218"}, {250.0, 255.0}]),
        "soft_dollar_tiers": ((3, [ibx.SoftDollarTier("tier", "val", "shown")]), [3, [dict(name="tier", val="val", displayName="shown")]]),
        "family_codes": (([ibx.FamilyCode("DU1234567", "F1")],), [[dict(accountID="DU1234567", familyCodeStr="F1")]]),
        "symbol_samples": ((3, [description]), [3, [dict(contract=description_e, derivativeSecTypes=["OPT", "WAR"])]]),
        "smart_components": ((3, {5: ("NASDAQ", "Q")}), [3, {5: ("NASDAQ", "Q")}]),
        "mkt_depth_exchanges": (([depth],), [[dict(exchange="NASDAQ", secType="STK", listingExch="NSDQ", serviceDataType="Deep2",
                                                   aggGroup=7)]]),
        "news_providers": (([provider],), [[dict(code="BRFG", name="Briefing")]]),
        "histogram_data": ((3, [ibx.HistogramData(1.5, Decimal(9))]), [3, [dict(price=1.5, size=Decimal(9))]]),
        "market_rule": ((26, [ibx.PriceIncrement(0.0, 0.01), ibx.PriceIncrement(1.0, 0.05)]),
                        [26, [dict(lowEdge=0.0, increment=0.01), dict(lowEdge=1.0, increment=0.05)]]),
        "historical_ticks": ((3, [t], True), [3, [t_e], True]),
        "historical_ticks_bid_ask": ((3, [tb], True), [3, [tb_e], True]),
        "historical_ticks_last": ((3, [tl], False), [3, [tl_e], False]),
        "tick_by_tick_all_last": ((3, 1, 1700000000, 150.25, Decimal(100), ibx.TickAttribLast(True, False), "NASDAQ", "T"),
                                  [3, 1, 1700000000, 150.25, Decimal(100), dict(pastLimit=True, unreported=False), "NASDAQ", "T"]),
        "tick_by_tick_bid_ask": ((3, 1700000000, 150.25, 150.5, Decimal(100), Decimal(200), ibx.TickAttribBidAsk(False, True)),
                                 [3, 1700000000, 150.25, 150.5, Decimal(100), Decimal(200), dict(bidPastLow=False, askPastHigh=True)]),
        "tick_by_tick_mid_point": ((3, 1700000000, 150.375), [3, 1700000000, 150.375]),
        "update_mkt_depth": ((3, 0, 1, 1, 150.25, Decimal(100)), [3, 0, 1, 1, 150.25, Decimal(100)]),
        "update_mkt_depth_l2": ((3, 0, "NSDQ", 1, 1, 150.25, Decimal(100), True), [3, 0, "NSDQ", 1, 1, 150.25, Decimal(100), True]),
        "historical_schedule": ((3, "20261112-09:30:00", "20261113-16:00:00", "US/Eastern",
                                 [ibx.HistoricalSession("20261112", "20261112-09:30:00", "20261112-16:00:00")]),
                                [3, "20261112-09:30:00", "20261113-16:00:00", "US/Eastern",
                                 [dict(refDate="20261112", startDateTime="20261112-09:30:00", endDateTime="20261112-16:00:00")]]),
    }


def decoders():
    """Protobuf callback (official name) -> the method of the official decoder that reads its message."""
    ibapi = official()
    out = {}
    for name, f in vars(ibapi.decoder.Decoder).items():
        if name.startswith("process") and name.endswith("ProtoBuf") and callable(f):
            for called in re.findall(r"self\.wrapper\.(\w+ProtoBuf)\(", inspect.getsource(f)):
                out[called] = name
    return out


def decoded(proto_callback, message):
    """The callbacks the official decoder makes for a protobuf message of ibx."""
    ibapi = official()
    theirs = Calls()
    getattr(ibapi.decoder.Decoder(theirs, 225), decoders()[T.OFFICIAL_NAMES[proto_callback]])(message.SerializeToString())
    assert theirs.calls[0][0] == T.OFFICIAL_NAMES[proto_callback]
    return theirs.calls[1:]


def layer():
    ours = Calls()
    return forms.Callbacks(ours, set(forms._PROTO_CALLBACKS)), ours


def compare(expected, got, path):
    assert len(expected) == len(got), path
    for i, (e, g) in enumerate(zip(expected, got)):
        if isinstance(e, dict) and all(isinstance(k, str) for k in e):
            check(g, e, f"{path}[{i}]")
        elif isinstance(e, list) and e and isinstance(e[0], dict):
            assert len(e) == len(g), f"{path}[{i}]"
            for j, (ee, gg) in enumerate(zip(e, g)):
                check(gg, ee, f"{path}[{i}][{j}]")
        else:
            assert e == g, f"{path}[{i}]: {e!r} != {g!r}"


def test_each_protobuf_callback_is_read_back_by_the_official_decoder_as_the_plain_callback():
    official()
    objects = with_objects()
    done = set()
    for name, (proto_callback, message, spec) in T.ANSWERS.items():
        # The others have their own tests below.
        if (not hasattr(ibx.EWrapper, name) or forms._BUILDERS.get(name, 1) is None
                or name in ("error", "tick_price", "tick_size", "bond_contract_details")):
            continue
        if name in objects:
            args, expected = objects[name]
        else:
            kinds = forms.KINDS[message]
            assert all(s is not None for s in spec), f"{name}: no sample"
            args = tuple(scalar(kinds[field], conv, i) for i, (field, conv, _) in enumerate(spec))
            expected = list(args)
        proxy, ours = layer()
        getattr(proxy, name)(*args)
        assert [c[0] for c in ours.calls] == [proto_callback, name], name
        assert ours.calls[1][1] == args
        theirs = decoded(proto_callback, ours.calls[0][1][0])
        assert [c[0] for c in theirs] == [on.official_name(name)], name
        compare(expected, theirs[0][1], name)
        done.add(name)
    # The bond form of the contract details: the maturity, the time and the time zone come back.
    cd, _ = details()
    cd.maturity, cd.lastTradeTime, cd.timeZoneId = "20301115", "16:00:00", "US/Eastern"
    proxy, ours = layer()
    proxy.bond_contract_details(4, cd)
    (_, (req_id, theirs)), = decoded("bond_contract_data_proto_buf", ours.calls[0][1][0])
    assert (req_id, theirs.maturity, theirs.lastTradeTime, theirs.timeZoneId) == (4, "20301115", "16:00:00", "US/Eastern")
    assert len(done) >= 70


def test_a_price_carries_its_size_and_the_size_has_no_message_of_its_own():
    official()
    proxy, ours = layer()
    proxy.price_has_size(Decimal(300))
    proxy.tick_price(1, 1, 150.25, ibx.TickAttrib(True, False, True))
    proxy.tick_size(1, 0, Decimal(300))
    proxy.tick_size(1, 0, Decimal(400))            # a size of its own
    assert [c[0] for c in ours.calls] == ["tick_price_proto_buf", "tick_price", "tick_size", "tick_size_proto_buf", "tick_size"]
    price = ours.calls[0][1][0]
    assert (price.reqId, price.tickType, price.price, price.size, price.attrMask) == (1, 1, 150.25, "300", 5)
    theirs = decoded("tick_price_proto_buf", price)
    assert [c[0] for c in theirs] == ["tickPrice", "tickSize"]
    assert theirs[0][1][:3] == (1, 1, 150.25) and theirs[1][1] == (1, 0, Decimal(300))
    attrib = theirs[0][1][3]
    assert (attrib.canAutoExecute, attrib.pastLimit, attrib.preOpen) == (True, False, True)
    assert decoded("tick_size_proto_buf", ours.calls[3][1][0]) == [("tickSize", (1, 0, Decimal(400)))]


def test_the_bars_and_the_scan_rows_come_in_one_message_before_their_callbacks():
    official()
    proxy, ours = layer()
    b, b_e = bar()
    proxy.historical_data_of(3, [b, b])
    proxy.historical_data(3, b)
    assert [c[0] for c in ours.calls] == ["historical_data_proto_buf", "historical_data"]
    theirs = decoded("historical_data_proto_buf", ours.calls[0][1][0])
    assert [c[0] for c in theirs] == ["historicalData", "historicalData"]
    check(theirs[0][1][1], b_e, "bar")

    proxy, ours = layer()
    cd = ibx.ContractDetails()
    cd.contract.conId, cd.contract.symbol, cd.marketName = 265598, "AAPL", "NMS"
    proxy.scanner_data_of(5, [(0, cd, "d", "b", "p", "legs")])
    proxy.scanner_data(5, 0, cd, "d", "b", "p", "legs")
    proxy.scanner_data_end(5)
    assert [c[0] for c in ours.calls] == ["scanner_data_proto_buf", "scanner_data", "scanner_data_end"]
    theirs = decoded("scanner_data_proto_buf", ours.calls[0][1][0])
    assert [c[0] for c in theirs] == ["scannerData", "scannerDataEnd"]
    req_id, rank, details_, distance, benchmark, projection, legs = theirs[0][1]
    assert (req_id, rank, details_.contract.conId, details_.contract.symbol, details_.marketName, distance, benchmark, projection,
            legs) == (5, 0, 265598, "AAPL", "NMS", "d", "b", "p", "legs")


def test_an_error_of_the_server_has_its_message_and_one_the_library_makes_itself_has_none():
    official()
    proxy, ours = layer()
    proxy.error(7, 200, "No security definition has been found for the request", "")
    proxy.error(-1, 504, "Not connected", "")
    assert [c[0] for c in ours.calls] == ["error_proto_buf", "error", "error"]
    (name, (req_id, when, code, text, advanced)), = decoded("error_proto_buf", ours.calls[0][1][0])
    assert (name, req_id, code, text, advanced) == ("error", 7, 200, "No security definition has been found for the request", "")
    assert when > 1_600_000_000_000


# ── Values of callbacks met on the way ──

def test_family_codes_price_increments_and_sessions_have_the_official_attributes_and_stay_tuples():
    ibapi = official()
    from ibapi.common import FamilyCode, HistoricalSession, PriceIncrement
    for ours, theirs in ((ibx.FamilyCode, FamilyCode), (ibx.PriceIncrement, PriceIncrement), (ibx.HistoricalSession, HistoricalSession)):
        assert {n: getattr(ours(), n) for n in vars(theirs())} == vars(theirs()), ours.__name__
        assert str(ours()) == str(theirs()), ours.__name__

    class W(ibx.EWrapper):
        def __init__(self):
            super().__init__()
            self.got = []

        def family_codes(self, family_codes):
            self.got.append(family_codes)

    w = W()
    c = ibx.EClient(w)
    c._test_connect("DU1234567")
    try:
        c.req_family_codes()
        c._test_dispatch_once()
    finally:
        c.disconnect()
    (codes,) = w.got
    for code in codes:
        account, family = code                      # as before: a pair
        assert (code.accountID, code.familyCodeStr) == (account, family)


# ── The client ──

class Plain(ibx.EWrapper):
    def __init__(self):
        super().__init__()
        self.calls = []

    def tick_price(self, req_id, tick_type, price, attrib):
        self.calls.append(("tick_price", req_id, tick_type, price))

    def tick_size(self, req_id, tick_type, size):
        self.calls.append(("tick_size", req_id, tick_type, size))


class WithProtoBuf(Plain):
    def tick_price_proto_buf(self, tick_price_proto):
        self.calls.append(("tick_price_proto_buf", tick_price_proto.reqId, tick_price_proto.tickType, tick_price_proto.price,
                           tick_price_proto.size if tick_price_proto.HasField("size") else None))

    def tick_size_proto_buf(self, tick_size_proto):
        self.calls.append(("tick_size_proto_buf", tick_size_proto.reqId, tick_size_proto.tickType, tick_size_proto.size))


def quotes(wrapper):
    c = ibx.EClient(wrapper)
    c._test_connect("DU1234567")
    try:
        c._test_map_instrument(1, 0)
        c._test_push_quote(0, bid=150.25, ask=150.50)
        c._test_dispatch_once()
    finally:
        c.disconnect()
    return c, [x for x in wrapper.calls if x[1] == 1]


def test_a_wrapper_with_protobuf_callbacks_gets_each_before_the_plain_callback():
    _, plain = quotes(Plain())
    c, both = quotes(WithProtoBuf())
    assert [x for x in both if not x[0].endswith("_proto_buf")] == plain
    for i, x in enumerate(both):
        if x[0] == "tick_price":
            assert both[i - 1][:4] == ("tick_price_proto_buf",) + x[1:], both
            # The size callback that follows a price is of the price's message.
            if i + 1 < len(both) and both[i + 1][0] == "tick_size":
                assert both[i - 1][4] == str(both[i + 1][3]), both
        if x[0] == "tick_size" and both[i - 1][0] != "tick_price":
            assert both[i - 1] == ("tick_size_proto_buf", x[1], x[2], str(x[3])), both
    assert any(x[0] == "tick_price_proto_buf" for x in both)


def test_a_wrapper_without_protobuf_callbacks_is_called_directly():
    w = Plain()
    assert forms.callbacks(w) is w
    assert forms.defined(w) == set()
    assert forms.defined(WithProtoBuf()) == {"tick_price_proto_buf", "tick_size_proto_buf"}


def test_without_the_official_library_the_message_is_a_plain_object(monkeypatch):
    monkeypatch.setattr(forms, "_classes", {m: None for m in T.MESSAGES})
    ours = Calls()
    proxy = forms.Callbacks(ours, set(forms._PROTO_CALLBACKS))
    c = ibx.Contract()
    c.conId, c.symbol, c.secType = 265598, "AAPL", "STK"
    proxy.position("DU1234567", c, Decimal(5), 1.5)
    message = ours.calls[0][1][0]
    assert isinstance(message, forms.ProtoForm)
    assert (message.account, message.position, message.avgCost) == ("DU1234567", "5", 1.5)
    assert (message.contract.conId, message.contract.symbol, message.contract.secType) == (265598, "AAPL", "STK")
    assert message.contract.lastTradeDate is None and message.contract.comboLegs == []
    assert message.HasField("account") and not message.contract.HasField("right")
