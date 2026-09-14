"""An unmodified ib_async program, running on this engine.

ib_async is the client most Python programs for this venue are written
against. What this proves is that such a program runs here with one line
changed — the attach — and no gateway process anywhere.

Skipped where ib_async is not installed: it is not a dependency of this
package, and is not vendored. The live parts need credentials.
"""

import os

import pytest

ib_async = pytest.importorskip("ib_async")

import ibx.ib_async  # noqa: E402

LIVE = bool(os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD"))
needs_venue = pytest.mark.skipif(not LIVE, reason="IB_USERNAME/IB_PASSWORD not set")


def test_attach_replaces_only_the_transport():
    ib = ibx.ib_async.attach(ib_async.IB())
    # Their object, their wrapper, their events — this client underneath.
    assert isinstance(ib, ib_async.IB)
    assert isinstance(ib.wrapper, ib_async.wrapper.Wrapper)
    assert ib.client.__class__ is ibx.ib_async.IbxClient
    assert not ib.isConnected()


def test_a_session_the_engine_lost_is_not_reported_as_connected():
    """The shim's own flag records what it was asked to do, not what holds.

    It moves on connect and on disconnect and nothing else touches it, so a
    session the venue took away or a reconnect gave up on left it saying
    connected. A watchdog written the ordinary way never fired, and every
    request made afterwards waited on an answer that was not coming.
    """
    ib = ibx.ib_async.attach(ib_async.IB())
    client = ib.client
    client.connState = client.CONNECTED
    assert not client.isConnected(), (
        "the engine underneath holds no session, so neither does this"
    )


def test_a_request_their_client_carries_and_this_one_does_not_says_so():
    ib = ibx.ib_async.attach(ib_async.IB())
    with pytest.raises(NotImplementedError, match="not carried"):
        ib.client.reqSomethingNobodyCarries(1)


@needs_venue
def test_an_unmodified_program_runs_on_this_engine():
    ib = ibx.ib_async.attach(ib_async.IB())
    ib.connect("no gateway", 0, clientId=1)
    try:
        assert ib.isConnected()
        assert ib.managedAccounts(), "the session names its account"

        spy = ib_async.Stock("SPY", "SMART", "USD")
        (qualified,) = ib.qualifyContracts(spy)
        assert qualified.conId > 0, "their qualify, answered here"

        bars = ib.reqHistoricalData(spy, "", "2 D", "1 hour", "TRADES", useRTH=True)
        assert len(bars) > 1, "their bars, in their own type"
        assert bars[-1].close > 0

        # Their event system, which is what their programs are built on.
        heard = []
        ib.pendingTickersEvent += lambda tickers: heard.append(len(tickers))
        ib.reqMktData(qualified)
        ib.sleep(4)
        ib.cancelMktData(qualified)
        assert sum(heard) > 0, "their tickers reached their event"
    finally:
        ib.disconnect()


@needs_venue
def test_an_order_lives_its_whole_life_through_their_api():
    """Placed, changed and withdrawn, in their objects.

    A limit far under the market, so it rests and nothing trades.
    """
    ib = ibx.ib_async.attach(ib_async.IB())
    ib.connect("no gateway", 0, clientId=1)
    try:
        ib.RequestTimeout = 15
        spy = ib_async.Stock("SPY", "SMART", "USD")
        (spy,) = ib.qualifyContracts(spy)

        state = ib.whatIfOrder(spy, ib_async.LimitOrder("BUY", 1, 1.00))
        assert state.status, "the venue prices it without placing it"
        # The venue prices some orders to a figure and others to a range, and
        # states only what it has: a preview it can only bound carries the two
        # bounds and leaves the figure at their unset. Read as a figure alone,
        # a priced preview looked unpriced.
        priced = [v for v in (state.commission, state.minCommission, state.maxCommission)
                  if v < 1e300]
        assert priced, "their unset is not a commission"

        order = ib_async.LimitOrder("BUY", 10, 100.00)
        trade = ib.placeOrder(spy, order)
        ib.sleep(3)
        assert trade.orderStatus.status in ("PreSubmitted", "Submitted")

        order.lmtPrice = 101.00
        ib.placeOrder(spy, order)
        ib.sleep(3)
        assert trade.order.lmtPrice == 101.00

        ib.cancelOrder(order)
        ib.sleep(3)
        assert trade.orderStatus.status == "Cancelled"
        assert [entry.status for entry in trade.log][-1] == "Cancelled"
    finally:
        ib.disconnect()


def test_a_bar_is_dated_the_way_their_parser_reads_one():
    """Their own first documented example is `util.df(bars)`.

    ib_async parses the date itself and decides the shape from the string: a
    day from eight digits, a moment in seconds from all digits, and an aware
    moment from a date, a time and a zone separated by single spaces.

    The venue stamps a bar in UTC and names the exchange's zone beside it. Put
    together by writing one after the other — as the bridge did — their parser
    reads the UTC time as a time on the exchange's clock, and every bar is out
    by whatever that zone is from UTC.
    """
    from ib_async.util import parseIBDatetime

    import ibx

    seen = []

    class W(ibx.EWrapper):
        def historical_data(self, req_id, bar):
            seen.append(bar.date)

        def error(self, *a):
            pass

    c = ibx.EClient(W())
    c._test_connect("T")
    c._test_push_historical_data(
        1, [("20260812-13:30:00", 1.0, 2.0, 0.5, 1.5, 100)], True, "US/Eastern"
    )
    c._test_dispatch_once()

    read = parseIBDatetime(seen[0])
    assert read.tzinfo is not None, "a naive moment cannot be converted to a zone"
    assert read.isoformat() == "2026-08-12T09:30:00-04:00", seen[0]
    # The same instant the venue stamped, which is the whole point.
    assert int(read.timestamp()) == 1786541400

def test_ending_a_session_is_not_a_session_that_went_away():
    """Their wrapper's `connectionClosed` fails every waiting request and
    raises on their global error event. That is a socket that dropped, not a
    caller who asked to stop, and their own client does not call it here."""
    import inspect

    from ibx.ib_async import IbxClient

    ends = inspect.getsource(IbxClient.disconnect)
    assert "connectionClosed" not in ends.split('"""')[-1]


def test_every_call_their_library_makes_is_carried():
    """Their `IB` calls its transport by name. Each one has to land here.

    Measured against their own source rather than a list kept by hand: a list
    is right on the day it is written. This found six that were reached by a
    name this engine does not use — `reqPnL` split on every capital is
    `req_pn_l` — and two that were not carried at all, so a program asking to
    stop a calendar query had nothing to call.
    """
    import inspect
    import re

    import ibx

    src = inspect.getsource(ib_async.ib)
    called = sorted(set(re.findall(r"self\.client\.(\w+)\(", src)))
    assert len(called) > 50, "their transport surface should be substantial"

    spelled_out = {n for n in dir(ibx.ib_async.IbxClient) if not n.startswith("_")}
    unrouted = [
        name for name in called
        if name not in spelled_out
        and not hasattr(ibx.EClient, ibx.ib_async._our_name_for(name, ibx.EClient))
    ]
    assert not unrouted, f"their library calls what this engine does not carry: {unrouted}"


def test_a_contract_named_by_id_states_nothing_else():
    """An id names one contract exactly, and nothing is stated beside it.

    This engine's own contract stands ready as a US stock on SMART in dollars,
    which is what a caller naming a symbol means. Carried onto a contract named
    only by its id, it stated that a future was a stock as well — a description
    the venue reads alongside an id it can contradict.
    """
    by_id = ibx.ib_async._as_ours(ib_async.Contract(conId=756733))
    assert by_id.conId == 756733
    assert (by_id.secType, by_id.exchange, by_id.currency) == ("", "", ""), (
        "an id was given, so nothing was guessed beside it"
    )

    # And a description is carried as it was written. A bare symbol used to
    # arrive as a US stock on SMART in dollars, which is three terms the caller
    # never stated and the reference client never sends.
    described = ibx.ib_async._as_ours(ib_async.Contract(symbol="AAPL"))
    assert (described.secType, described.exchange, described.currency) == ("", "", "")

    # And what the caller did state is carried whichever way it was given.
    stock = ibx.ib_async._as_ours(ib_async.Stock("AAPL", "SMART", "USD"))
    assert (stock.symbol, stock.secType, stock.exchange) == ("AAPL", "STK", "SMART")


def test_a_combination_keeps_its_legs_across_the_bridge():
    """Everything the contract carries, not the fields a shorter list names.

    Converting a contract through a narrower helper lost what that helper did
    not name — the legs of a combination among them, which makes the order one
    for something else entirely.
    """
    theirs = ib_async.Contract(symbol="SPY", secType="BAG", exchange="SMART", currency="USD")
    theirs.comboLegs = [ib_async.ComboLeg(conId=756733, ratio=1, action="BUY", exchange="SMART")]
    theirs.secIdType, theirs.secId = "ISIN", "US78462F1030"
    theirs.includeExpired = True

    ours = ibx.ib_async._as_ours(theirs)
    assert len(ours.comboLegs) == 1, "a combination is its legs"
    assert (ours.secIdType, ours.secId) == ("ISIN", "US78462F1030")
    assert ours.includeExpired is True
    assert ours.secType == "BAG"


def test_what_tunes_an_algo_reaches_the_order():
    """A field set by the name the reference client uses has to arrive.

    The camelCase names were readable and not writable, and the failure to
    write them was swallowed: an Adaptive order carried its strategy and lost
    the priority that tunes it, a combination lost its routing, and an order
    directing its commission to a tier stated one that was never carried. The
    order went out on terms nobody had asked for, and nothing said so.
    """
    their = ib_async.Order(
        orderId=1, action="BUY", totalQuantity=1, orderType="LMT", lmtPrice=1.0,
    )
    their.algoStrategy = "Adaptive"
    their.algoParams = [ib_async.TagValue("adaptivePriority", "Normal")]
    their.smartComboRoutingParams = [ib_async.TagValue("NonGuaranteed", "1")]
    their.softDollarTier = ib_async.SoftDollarTier("T", "v", "D")

    ours = ibx.ib_async._as_ours(their)
    assert [(p.tag, p.value) for p in ours.algoParams] == [("adaptivePriority", "Normal")]
    assert [(p.tag, p.value) for p in ours.smartComboRoutingParams] == [("NonGuaranteed", "1")]
    tier = ours.softDollarTier
    assert (tier.name, tier.val, tier.displayName) == ("T", "v", "D")


def test_a_field_that_cannot_be_carried_is_refused():
    """Swallowed, an unset field is an order placed on terms nobody stated."""
    their = ib_async.Order(orderId=1, action="BUY", totalQuantity=1)
    their.orderType = object()  # not a string, so nothing can carry it
    with pytest.raises(ValueError, match="cannot carry"):
        ibx.ib_async._as_ours(their)



def test_readonly_reaches_the_session_through_the_adapter():
    """A read-only connection through this adapter is read-only.

    The flag was accepted by the facade and dropped by the adapter, so a
    program that asked ib_async for a read-only session got one that could
    place orders. It is carried now, and `attach` takes one for a caller that
    sets it before connecting.
    """
    import inspect

    from ibx.ib_async import IbxClient, attach

    sig = inspect.signature(IbxClient.connectAsync)
    assert "readonly" in sig.parameters, "the adapter takes it"
    assert "readonly" in inspect.signature(attach).parameters, "and so does attach"
    assert "readonly" in inspect.signature(IbxClient.__init__).parameters

    # What attach set stands where the connect states none.
    client = IbxClient(wrapper=None, readonly=True)
    assert client._readonly is True
