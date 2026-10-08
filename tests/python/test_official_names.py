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
    return {n for n in dir(cls) if not n.startswith("_") and callable(getattr(cls, n)) and not n.endswith("ProtoBuf")}


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
