"""ibx#489: the adapter that gives the ibx Python client the official client
library's names (tests/python/official_names.py).

The name checks compare the adapter with the installed official client
library and are skipped without it. The last test logs in to the paper
account and is skipped without IB_USERNAME / IB_PASSWORD.
"""

import inspect
import os
import threading
from decimal import Decimal

import pytest

import official_names as on

# Official EClient methods that are not requests of the API.
NOT_REQUESTS = {"reset", "logRequest", "sendMsg", "checkConnected", "validateInvalidSymbols", "validateOrderParameters",
                "validateAttachedOrdersParameters", "keyboardInterrupt", "keyboardInterruptHard", "msgLoopTmo",
                "msgLoopRec"}


def official():
    client = pytest.importorskip("ibapi.client")
    wrapper = pytest.importorskip("ibapi.wrapper")
    return client.EClient, wrapper.EWrapper


def names(cls):
    return {n for n in dir(cls) if not n.startswith("_") and callable(getattr(cls, n))}


def parameters(f):
    return [p for p in inspect.signature(f).parameters if p != "self"]


def test_every_official_callback_has_its_name_or_is_listed():
    _, wrapper = official()
    theirs = names(wrapper) - {"logAnswer"}
    ours = names(on.EWrapper)
    assert theirs - ours == on.NOT_IN_IBX & theirs
    assert ours - theirs == set()


def test_every_official_request_has_its_name_or_is_listed():
    client, _ = official()
    theirs = names(client) - NOT_REQUESTS
    ours = names(on.EClient)
    assert theirs - ours == on.NOT_IN_IBX & theirs
    assert ours - theirs == set()


def test_the_lists_hold_nothing_else():
    client, wrapper = official()
    assert on.NOT_IN_IBX <= names(client) | names(wrapper)
    assert not {on.official_name(n) for n in on.IBX_ONLY} & (names(client) | names(wrapper))


def test_official_arguments_are_the_ibx_ones_in_the_same_order():
    client, _ = official()
    special = {"connect", "cancelOrder", "reqGlobalCancel", "exerciseOptions"}
    for name in on.REQUESTS:
        theirs = on.official_name(name)
        if theirs in special:
            continue
        ours = on.ibx_parameters(name)
        assert [on.ibx_keyword(p, ours) for p in parameters(getattr(client, theirs))] == ours, theirs
    for theirs in special - {"connect"}:
        assert parameters(getattr(on.EClient, theirs)) == parameters(getattr(client, theirs)), theirs
    assert parameters(on.EClient.connect) == parameters(client.connect)


def test_callbacks_take_as_many_arguments_as_the_official_ones():
    _, wrapper = official()
    import ibx
    for name in on.CALLBACKS:
        if name == "error":
            continue
        assert len(parameters(getattr(ibx.EWrapper, name))) == len(parameters(getattr(wrapper, on.official_name(name)))), name
    assert parameters(wrapper.error) == ["reqId", "errorTime", "errorCode", "errorString", "advancedOrderRejectJson"]


def test_the_plain_objects_start_as_the_official_ones():
    pytest.importorskip("ibapi")
    from ibapi.execution import ExecutionFilter
    from ibapi.order_cancel import OrderCancel
    from ibapi.scanner import ScannerSubscription
    for ours, theirs in [(on.OrderCancel, OrderCancel), (on.ExecutionFilter, ExecutionFilter),
                         (on.ScannerSubscription, ScannerSubscription)]:
        assert vars(ours()) == vars(theirs()), ours.__name__


# ibx#697 (decision of 15/10/2026): a callback object lists the official names
# only; the second spelling stays usable. One known difference (ibx#712): the
# bar keeps the time zone of its date, which the official object drops.
CALLBACK_OBJECTS = {
    "Contract": "ibapi.contract", "ContractDetails": "ibapi.contract", "ComboLeg": "ibapi.contract",
    "ContractDescription": "ibapi.contract", "Order": "ibapi.order", "OrderComboLeg": "ibapi.order",
    "OrderState": "ibapi.order_state", "Execution": "ibapi.execution", "TagValue": "ibapi.tag_value",
    "BarData": "ibapi.common", "TickAttrib": "ibapi.common", "TickAttribLast": "ibapi.common",
    "TickAttribBidAsk": "ibapi.common", "HistoricalTick": "ibapi.common", "HistoricalTickLast": "ibapi.common",
    "HistoricalTickBidAsk": "ibapi.common", "HistogramData": "ibapi.common",
    "CommissionAndFeesReport": "ibapi.commission_and_fees_report", "SoftDollarTier": "ibapi.softdollartier",
    "NewsProvider": "ibapi.common", "SmartComponent": "ibapi.common", "DepthMktDataDescription": "ibapi.common",
    "IneligibilityReason": "ibapi.ineligibility_reason", "WshEventData": "ibapi.common",
    "OrderAllocation": "ibapi.order_state", "PriceCondition": "ibapi.order_condition",
    "TimeCondition": "ibapi.order_condition", "MarginCondition": "ibapi.order_condition",
    "ExecutionCondition": "ibapi.order_condition", "VolumeCondition": "ibapi.order_condition",
    "PercentChangeCondition": "ibapi.order_condition",
    "FamilyCode": "ibapi.common", "PriceIncrement": "ibapi.common", "HistoricalSession": "ibapi.common",
}
CONDITION_TYPES = {"Price", "Time", "Margin", "Execution", "Volume", "PercentChange"}
# None left: BarData's timezone went with ibx#654.
KNOWN_ATTRIBUTE_DIFFERENCES = {}


def test_callback_objects_list_the_official_names():
    pytest.importorskip("ibapi")
    import importlib

    import ibx

    def listed(obj):
        return {n for n in dir(obj) if not n.startswith("_") and not callable(getattr(obj, n))}

    for name, module in CALLBACK_OBJECTS.items():
        theirs = listed(getattr(importlib.import_module(module), name)())
        obj = getattr(ibx, name)()
        ours = listed(obj)
        missing, extra = KNOWN_ATTRIBUTE_DIFFERENCES.get(name, (set(), set()))
        assert (theirs - ours, ours - theirs) == (missing, extra), name
    # The second spelling is still there.
    attrib = ibx.TickAttrib()
    attrib.can_auto_execute = True
    assert attrib.canAutoExecute and "can_auto_execute" not in dir(attrib)


class App(on.EWrapper, on.EClient):
    """A client in the form of the scripts written for the official library."""

    def __init__(self):
        on.EClient.__init__(self, self)
        self.calls = []
        self.seen = threading.Condition()
        for name in dir(on.EWrapper):
            if not name.startswith("_"):
                setattr(self, name, self._recorder(name))

    def _recorder(self, name):
        def record(*args):
            with self.seen:
                self.calls.append((name, args))
                self.seen.notify_all()
        return record

    def wait(self, name, timeout):
        with self.seen:
            assert self.seen.wait_for(lambda: any(n == name for n, _ in self.calls), timeout), f"no {name}"
            return next(a for n, a in self.calls if n == name)


def test_requests_and_callbacks_go_through_under_the_official_names():
    app = App()
    app.ibx._test_connect("DU1234567")
    assert app.isConnected()
    app.ibx._test_map_instrument(1, 0)
    app.ibx._test_push_quote(0, bid=150.25, ask=150.50)
    app.ibx._test_dispatch_once()
    prices = [a[:3] for n, a in app.calls if n == "tickPrice"]
    assert (1, 1, 150.25) in prices and (1, 2, 150.50) in prices

    # A refused request: the refusal arrives as the official five-argument error.
    app.reqPnL(5, "", "")
    app.ibx._test_dispatch_once()
    req_id, error_time, code, text, advanced = next(a for n, a in app.calls if n == "error")
    assert (req_id, code) == (5, 321) and error_time > 1_600_000_000_000 and advanced == ""
    app.cancelOrder(4242, on.OrderCancel())
    app.reqGlobalCancel(on.OrderCancel())
    app.disconnect()


def test_official_keywords_are_accepted():
    app = App()
    app.ibx._test_connect("DU1234567")
    app.reqPnLSingle(reqId=7, account="DU1234567", modelCode="", conid=265598)
    app.cancelPnLSingle(reqId=7)
    app.disconnect()


def test_an_order_takes_the_official_quantity_type():
    order = on.Order()
    order.action, order.orderType, order.totalQuantity, order.lmtPrice = "BUY", "LMT", Decimal("1"), 1.0
    assert order.totalQuantity == 1


def test_a_live_account_is_refused():
    with pytest.raises(SystemExit):
        on.assert_paper_account("U1234567")
    with pytest.raises(SystemExit):
        on.assert_paper_account("")
    on.assert_paper_account("DU1234567")


def test_connect_needs_the_credentials(monkeypatch):
    monkeypatch.delenv("IB_USERNAME", raising=False)
    with pytest.raises(RuntimeError):
        App().connect("127.0.0.1", 4002, 7)


@pytest.mark.skipif(not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
                    reason="live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)")
def test_live_script_in_the_official_form_runs_on_the_paper_account():
    app = App()
    app.connect("127.0.0.1", 4002, 489)
    loop = threading.Thread(target=app.run, daemon=True)
    loop.start()
    try:
        (order_id,) = app.wait("nextValidId", 20)
        assert order_id > 0
        (accounts,) = app.wait("managedAccounts", 20)
        assert all(a.startswith("DU") for a in accounts.split(",") if a)

        contract = on.Contract()
        contract.symbol, contract.secType, contract.exchange, contract.currency = "AAPL", "STK", "SMART", "USD"
        app.reqContractDetails(1, contract)
        req_id, details = app.wait("contractDetails", 20)
        assert req_id == 1 and details.contract.conId == 265598
        app.wait("contractDetailsEnd", 20)

        app.reqCurrentTime()
        (now,) = app.wait("currentTime", 20)
        assert now > 1_600_000_000
    finally:
        app.disconnect()
        loop.join(timeout=5)


# ibx#712: the execution's exercise or lapse type, the condition type numbers
# and the trigger methods, as the official objects give them.
def test_exercise_type_condition_numbers_and_trigger_methods():
    pytest.importorskip("ibapi")
    import ibx
    from ibapi.execution import Execution, OptionExerciseType
    from ibapi.order_condition import PriceCondition, VolumeCondition

    ours, theirs = ibx.Execution(), Execution()
    assert ours.optExerciseOrLapseType.value == theirs.optExerciseOrLapseType.value == (-1, "None")
    ours.optExerciseOrLapseType = OptionExerciseType.Lapse
    assert ours.optExerciseOrLapseType.name == "Lapse" and "OptExerciseOrLapseType: Lapse" in str(ours)
    for name in ["Price", "Time", "Margin", "Execution", "Volume", "PercentChange"]:
        assert getattr(ibx.VolumeCondition, name) == getattr(VolumeCondition, name), name
    ours, theirs = ibx.PriceCondition.TriggerMethodEnum, PriceCondition.TriggerMethodEnum
    assert ours.idx2name == theirs.idx2name and ours.MidPoint == theirs.MidPoint == 8
    assert ours.toStr(7) == "LastBidAsk" and ours.toStr(99) == "NOTFOUND"


# ibx#602: at session level 225 the official contract row carries the fund
# asset type and distribution policy as members of its enums (NoneItem when
# the row has none), and 0 as an integer for an absent multiplier or coupon.
def test_contract_details_fund_enums_and_absent_numbers():
    pytest.importorskip("ibapi")
    import ibx
    from ibapi.contract import ContractDetails, FundAssetType, FundDistributionPolicyIndicator

    ours, theirs = ibx.ContractDetails(), ContractDetails()
    for name in ("fundAssetType", "fundDistributionPolicyIndicator"):
        a, b = getattr(ours, name), getattr(theirs, name)
        assert (type(a).__name__, a.name, a.value) == (type(b).__name__, b.name, b.value), name
    for name in ("evMultiplier", "coupon"):
        assert type(getattr(ours, name)) is type(getattr(theirs, name)) is int and getattr(ours, name) == 0, name
    assert [(m.name, m.value) for m in type(ours.fundAssetType)] == [(m.name, m.value) for m in FundAssetType]
    assert [(m.name, m.value) for m in type(ours.fundDistributionPolicyIndicator)] ==         [(m.name, m.value) for m in FundDistributionPolicyIndicator]
    ours.fund_asset_type, ours.fund_distribution_policy_indicator = "004", "Y"
    assert (ours.fundAssetType.name, ours.fundDistributionPolicyIndicator.name) == ("Equity", "IncomeFund")
    ours.fund_asset_type = "000"          # the official lookup finds no member for it
    assert ours.fundAssetType.name == "NoneItem"
    ours.fundAssetType = FundAssetType.Sector
    ours.fundDistributionPolicyIndicator = FundDistributionPolicyIndicator.NoneItem
    assert (ours.fund_asset_type, ours.fund_distribution_policy_indicator) == ("005", "")
    ours.evMultiplier, ours.coupon = 2.5, 1.25
    assert (ours.evMultiplier, ours.ev_multiplier, ours.coupon) == (2.5, 2.5, 1.25)
