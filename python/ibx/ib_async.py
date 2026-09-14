"""Run an unmodified `ib_async` program without a gateway.

`ib_async` is layered: `IB`, `Wrapper`, `Ticker`, `Trade` and everything above
them are transport-agnostic, and only its `Client`/`Connection` know there is a
socket to a gateway on localhost. This replaces that one layer with this
engine. Everything above it — the events, the async variants, the notebooks —
runs unchanged, from the copy of `ib_async` already installed.

No part of `ib_async` is copied or modified: install it as usual, and attach.

    from ib_async import IB, Stock
    import ibx.ib_async

    ib = ibx.ib_async.attach(IB(), username="…", password="…")
    ib.connect()                      # names no host: there is no gateway

    spy = Stock("SPY", "SMART", "USD")
    ib.qualifyContracts(spy)
    print(ib.reqHistoricalData(spy, "", "2 D", "1 hour", "TRADES", useRTH=True))

`IB.connect` takes a host, a port and a client id because it was written for a
gateway. They are accepted and ignored. The credentials are given to `attach`,
or left to `IB_USERNAME` and `IB_PASSWORD`.
"""

import asyncio
import os
import pathlib
import threading
import time

from eventkit import Event

import ibx as _ibx
from ._ib import _refuse_options


#: The widest number this protocol carries on a request.
#:
#: A request id is four billion wide and the top quarter of that range is this
#: client's own, for the calls it numbers on a caller's behalf. An order id is
#: not held to it: the venue numbers orders as wide as it likes, and an account
#: whose orders have outgrown a request id is ordinary rather than broken.
WIDEST_REQUEST_ID = 0xC000_0000 - 1


class IbxClient:
    """What `ib_async.IB` talks to, answered by this engine.

    Holds the same attributes and events `ib_async.Client` does, because `IB`
    reads them directly.
    """

    DISCONNECTED, CONNECTING, CONNECTED = range(3)
    MinClientVersion = 157
    MaxClientVersion = 178

    def __init__(self, wrapper, username="", password="", paper=True,
                 session_file=None, client_id=None, readonly=False):
        self.wrapper = wrapper
        self._username = username or os.environ.get("IB_USERNAME", "")
        self._password = password or os.environ.get("IB_PASSWORD", "")
        self._paper = paper
        # Where a caller said so before connecting. `connectAsync` takes one of
        # its own; unstated there, this is what stands.
        self._readonly = bool(readonly)
        self.apiStart = Event("apiStart")
        self.apiEnd = Event("apiEnd")
        self.apiError = Event("apiError")
        self.throttleStart = Event("throttleStart")
        self.throttleEnd = Event("throttleEnd")

        self.host = ""
        self.port = -1
        self.clientId = -1
        self.optCapab = ""
        self.connectOptions = b""
        self.connState = IbxClient.DISCONNECTED
        self._reqIdSeq = 1
        self._accounts: list[str] = []
        self._loop = None
        self._pump = None
        self._stop = threading.Event()
        self._since = time.time()

        # The engine, with ib_async's own wrapper as the callback target: this
        # client already resolves a callback under the reference client's
        # spelling, which is the spelling ib_async's wrapper uses.
        self._callbacks = _LoopBound(wrapper)
        self._client = _ibx.EClient(self._callbacks)

        # Where this session is kept between runs. The venue answers a request
        # that names a session it still holds with a challenge rather than a
        # whole handshake, so a program that starts often is not a new login
        # every time — and does not ask a person to approve each one. Owner
        # only, sealed with the password, and refused if it names another
        # account. Pass session_file=False to attach() to keep nothing.
        self._session_file = session_file
        # Which counter this session counts on. Their own connect names one
        # too; stated here it is what the counter is keyed by before that call
        # is ever made.
        if client_id is not None:
            self.clientId = int(client_id)

    # ── connection ──

    async def connectAsync(self, host, port, clientId, timeout=2.0, readonly=None,
                           account=""):
        """Open the session. Host and port name a gateway; there is none.

        ``readonly`` is carried to the session, which refuses to send anything
        that places, changes or withdraws an order. Accepted and dropped, a
        program that asked for a read-only connection got one that could trade.
        """
        readonly = self._readonly if readonly is None else bool(readonly)
        self.host, self.port, self.clientId = host, int(port), int(clientId)
        self._callbacks._client_id = self.clientId
        self.connState = IbxClient.CONNECTING
        self._loop = asyncio.get_running_loop()
        self.wrapper.__dict__.setdefault("clientId", self.clientId)

        # Blocking, so it runs off the loop: everything else here has to stay
        # able to answer while the session opens.
        await self._loop.run_in_executor(
            None,
            lambda: self._client.connect(
                username=self._username,
                password=self._password,
                paper=self._paper,
                client_id=self.clientId,
                readonly=readonly,
                session_file=self._session_file,
            ),
        )
        self.connState = IbxClient.CONNECTED

        # What the handshake tells ib_async before it considers the API
        # ready. Asked for rather than composed: the client answers this with
        # every account the login holds, and the default account read off it
        # is the first one — so an advisor with several saw one, standing for
        # all of them.
        self._client.req_managed_accts()
        # Answered on the next pass of dispatch, as every request is, and the
        # pump that makes those passes is not running yet: one pass here, so
        # the answer is in hand before it is read.
        self._pass_once()
        self._accounts = list(getattr(self.wrapper, "accounts", []))
        # Their wrapper's `nextValidId` does nothing; their client seeds the
        # counter itself when it sees one on the wire. `placeOrder` takes its
        # id from that counter, so an id announced and not seeded is an order
        # numbered from one on an account that has already traded.
        # Announced, not seeded. Their client numbers its orders and its
        # requests out of one counter because the client it stands in for
        # carries both as the same signed 32-bit number. This protocol does
        # not: an order id goes as wide as it likes and a request id is four
        # billion wide, and an account that has once been given a wide order
        # id leaves that counter unable to carry a request. The two are
        # counted apart here — requests from where they were, orders from what
        # the account has used, filled in by `placeOrder` below.
        next_valid = self._client.next_order_id()
        self.wrapper.nextValidId(next_valid)
        # Their counter serves requests and the orders they number themselves,
        # so it is seeded with a number that answers for both: past every id
        # an order has spent that a request could also carry.
        self.updateReqId(self._client.next_shared_id())

        self._start_pump()
        self.apiStart.emit()

    def disconnect(self):
        """End the session, as ib_async's own client ends one.

        `connectionClosed` is not called here. Their wrapper treats it as a
        session that went away underneath them: it fails every request still
        waiting and raises on their global error event, which is right for a
        socket that dropped and wrong for a caller who asked to stop.
        """
        self.connState = IbxClient.DISCONNECTED
        self._stop.set()
        self._client.disconnect()
        self.apiEnd.emit()

    def _pass_once(self):
        """One dispatch, then the boundary ib_async flushes on.

        Their wrapper holds ticker updates until the batch of messages ends,
        and emits their events there. Their own transport calls this after
        each socket read; here it is after each pass of dispatch.
        """
        arrived = getattr(self.wrapper, "tcpDataArrived", None)
        if arrived:
            arrived()
        self._client.poll()
        processed = getattr(self.wrapper, "tcpDataProcessed", None)
        if processed:
            processed()

    def _start_pump(self):
        """Drive dispatch, and land every callback on ib_async's own loop.

        ib_async is asyncio end to end: its futures are resolved by wrapper
        callbacks and must be touched from the loop thread.
        """
        self._stop.clear()

        def run():
            while not self._stop.is_set():
                self._loop.call_soon_threadsafe(self._pass_once)
                self._stop.wait(0.01)

        self._pump = threading.Thread(target=run, daemon=True)
        self._pump.start()

    # ── what IB reads directly ──

    def isConnected(self):
        """Whether a session is open, not whether one was opened.

        ``connState`` records what this shim was told to do — it moves on
        connect and on disconnect and nothing else touches it. The engine
        underneath knows when the venue took the session away or a reconnect
        gave up, and the sibling facade already asks it. Reading the flag alone
        answered True for a session that could carry nothing, so a watchdog
        written the ordinary way never fired and every request made after the
        loss waited on an answer that was not coming.
        """
        return (
            self.connState == IbxClient.CONNECTED
            and self._client.is_connected()
        )

    def isReady(self):
        return self.isConnected()

    def serverVersion(self):
        return self.MaxClientVersion

    def getAccounts(self):
        return list(self._accounts)

    def getReqId(self):
        # Hands out the current value and then advances, as their own client
        # does, so an id seeded by `updateReqId` is the next one issued rather
        # than the one after it.
        new_id = self._reqIdSeq
        self._reqIdSeq += 1
        return new_id

    def updateReqId(self, minReqId):
        # Their wrapper raises this counter past every order id it sees, so
        # that the next order their client numbers is not one the account is
        # already working. Their client numbers orders and requests out of it
        # alike, because the client it stands in for carries both as one signed
        # 32-bit number.
        #
        # Here they are two. An order id goes as wide as the venue lets it, a
        # request id is four billion wide with the top of that reserved, and
        # `placeOrder` below numbers an order from the account's own counter
        # rather than from this one — so a raise past what a request can carry
        # buys nothing and costs everything: on an account whose orders are
        # numbered above it, every request afterwards was refused as a number
        # this protocol cannot carry, and an unmodified program could not so
        # much as name a contract. Such a raise is let go of rather than taken
        # to the top of the range, which saturates and steps over the edge on
        # the next request.
        if minReqId > WIDEST_REQUEST_ID:
            return
        self._reqIdSeq = max(self._reqIdSeq, minReqId)

    def connectionStats(self):
        from ib_async.objects import ConnectionStats

        return ConnectionStats(self._since, time.time() - self._since, 0, 0, 0, 0)

    def setConnectOptions(self, options):
        self.connectOptions = options.encode()

    # ── requests: the same shape, all the way down ──

    def reqMktData(self, reqId, contract, genericTickList, snapshot,
                   regulatorySnapshot, mktDataOptions):
        _refuse_options("mktDataOptions", mktDataOptions)
        self._client.req_mkt_data(
            reqId, _as_ours(contract), genericTickList, snapshot,
            regulatorySnapshot,
        )

    def reqHistoricalData(self, reqId, contract, endDateTime, durationStr,
                          barSizeSetting, whatToShow, useRTH, formatDate,
                          keepUpToDate, chartOptions):
        _refuse_options("chartOptions", chartOptions)
        self._client.req_historical_data(
            reqId, _as_ours(contract), endDateTime, durationStr,
            barSizeSetting, whatToShow, 1 if useRTH else 0, formatDate,
            keepUpToDate, [],
        )

    # These two stay written out. `__getattr__` forwards every argument
    # through `_as_ours`, which turns None into an empty list — right for an
    # options list and wrong for a string, which is what these carry.
    def reqAccountUpdates(self, subscribe, acctCode):
        self._client.req_account_updates(subscribe, acctCode)

    def reqAccountSummary(self, reqId, groupName, tags):
        self._client.req_account_summary(reqId, groupName, tags)

    def __getattr__(self, name):
        """Every other request, under the name this engine carries it by.

        Both sides follow the reference client's own signatures, so a request
        with no special handling above is forwarded as it stands rather than
        written out again here. One that this engine does not carry says so,
        naming itself, instead of failing as a missing attribute.
        """
        if name.startswith("_"):
            raise AttributeError(name)

        carried = getattr(self._client, _our_name_for(name, self._client), None)
        if carried is None:
            def missing(*args, **kwargs):
                raise NotImplementedError(
                    f"{name}() is not carried by this client"
                )
            return missing

        def forwarding(*args):
            return carried(*(_as_ours(a) for a in args))

        return forwarding


#: Where the two name the same figure differently. Their order state was
#: written before the venue renamed a commission to include its fees.
_OUR_NAME = {
    "commission": "commissionAndFees",
    "minCommission": "minCommissionAndFees",
    "maxCommission": "maxCommissionAndFees",
    "commissionCurrency": "commissionAndFeesCurrency",
}


def _our_name_for(their_name, carrier):
    """What this engine calls the request they call `their_name`.

    A capital starts a word, which is enough for every name either side spells
    out — but not for the ones that run words together. `reqPnL` split that way
    is `req_pn_l` and matches nothing, so a program asking this engine for a
    running profit was told it carries none. So the split is a first guess, and
    what the engine actually carries decides: one name, ignoring where the
    underscores fell.
    """
    split = "".join("_" + c.lower() if c.isupper() else c for c in their_name)
    if hasattr(carrier, split):
        return split
    flattened = their_name.lower()
    for carried in dir(carrier):
        if carried.replace("_", "") == flattened:
            return carried
    return split


#: Where the two name the same record differently. Their commission report was
#: named before the venue started charging fees through it, and this client
#: names it for what it carries now — so the record that says what a trade cost
#: reached their wrapper as a type nothing there could read, and every fill
#: raised.
_THEIR_TYPE_NAME = {
    "CommissionAndFeesReport": "CommissionReport",
}


def _is_named_tuple(t):
    """Whether a type is one of their record tuples.

    Their historical ticks are `NamedTuple`s rather than dataclasses, so a
    conversion that only knows dataclasses hands those straight through — and
    a caller reading `tick.priceBid` off an ibx record finds a field spelled
    the other way.
    """
    return isinstance(t, type) and issubclass(t, tuple) and hasattr(t, "_fields")


def _their_type(name):
    """The type of theirs that goes by this name, if there is one."""
    import dataclasses

    import ib_async.contract as contract_types
    import ib_async.objects as objects
    import ib_async.order as order_types

    name = _THEIR_TYPE_NAME.get(name, name)
    for module in (contract_types, objects, order_types):
        found = getattr(module, name, None)
        if found is not None and (dataclasses.is_dataclass(found) or _is_named_tuple(found)):
            return found
    return None


def _field_of(value, name):
    """One field of an ibx record, under whichever name it goes by.

    A moment is handed over as their own, because their records declare it as
    a datetime and a number read as one is an instant in 1970.
    """
    got = getattr(value, name, None)
    if got is None:
        got = getattr(value, _OUR_NAME.get(name, name), None)
    if got is None or name not in ("time", "date"):
        return got
    if isinstance(got, int) and not isinstance(got, bool):
        # Seconds since the epoch, which ib_async's records declare as a
        # datetime. Parsed by ib_async's own parser, so the instant matches
        # what the rest of ib_async reads.
        from ib_async.util import parseIBDatetime

        return parseIBDatetime(str(got))
    # A string is handed over as it stands. The engine writes a bar the way
    # their parser reads one — the instant on the exchange's clock with the
    # zone after it — and composing one here from the venue's own stamp, which
    # is UTC, put every bar out by whatever that zone is from UTC.
    return got


def _as_theirs(value):
    """An ibx object, rebuilt as the same-named `ib_async` type.

    Both sides carry the reference client's own field names, so the conversion
    is driven by the `ib_async` dataclass rather than written out per type or
    per callback: a field only `ib_async` declares keeps its default, and
    neither side needs editing when the other gains one. Anything with no
    counterpart there — a number, a string, an ibx-only type — is handed over
    as it is.
    """
    import dataclasses

    if isinstance(value, (str, bytes, int, float, bool, type(None))):
        return value
    if isinstance(value, (list, tuple)):
        return type(value)(_as_theirs(v) for v in value)

    theirs = _their_type(type(value).__name__)
    if theirs is None or dataclasses.is_dataclass(value):
        return value

    # A record tuple is built in one go: its fields are positional and it has
    # no setters, so the field-by-field walk below cannot be used on one.
    if _is_named_tuple(theirs):
        return theirs(*[
            _as_theirs(_field_of(value, name))
            for name in theirs._fields
        ])

    made = theirs()
    for field in dataclasses.fields(theirs):
        ours = getattr(value, field.name, None)
        if ours is None:
            ours = getattr(value, _OUR_NAME.get(field.name, field.name), None)
        if ours is None:
            continue
        try:
            setattr(made, field.name, _as_theirs(ours))
        except (TypeError, ValueError, AttributeError):
            pass
    return made


class _LoopBound:
    """ib_async's wrapper, reached under the names this engine calls.

    A callback carries an ibx object; the `ib_async` wrapper expects its own.
    Every argument is rebuilt on the way through, by its own type name, so a
    callback nobody thought to list is carried too.
    """

    #: The size that goes with each price, by tick type: bid, ask, last, and
    #: the same three delayed.
    _SIZE_OF = {1: 0, 2: 3, 4: 5, 66: 69, 67: 70, 68: 71}
    _PRICE_OF = {size: price for price, size in _SIZE_OF.items()}

    def __init__(self, wrapper, client_id=0):
        self._wrapper = wrapper
        self._client_id = client_id
        self._quotes: dict[int, dict[int, float]] = {}

    def histogram_data(self, req_id, items):
        """The spread of trades across prices, in their own type.

        This engine hands over each entry as a price and a count; their
        wrapper reads two named fields off it.
        """
        from ib_async.objects import HistogramData

        # Built as theirs either way. This engine states a bucket the way the
        # reference client does — `price` and `size` — and their wrapper reads
        # `price` and `count`, so an entry passed straight through carries a
        # name they do not read. Handing back whatever arrived was right only
        # while this engine handed back a pair.
        self._wrapper.histogramData(
            req_id,
            [
                HistogramData(
                    price=item.price if hasattr(item, "price") else item[0],
                    count=int(item.size if hasattr(item, "size") else item[1]),
                )
                for item in items
            ],
        )

    def order_status(self, order_id, status, filled, remaining, avg_fill_price,
                     perm_id, parent_id, last_fill_price, client_id, why_held,
                     mkt_cap_price):
        """Stamped with the client this session opened under.

        Their wrapper holds a trade under the client that placed it, and looks
        it up the same way; stamped with a client the session never used, the
        status reaches nothing.
        """
        self._wrapper.orderStatus(
            order_id, status, filled, remaining, avg_fill_price, perm_id,
            parent_id, last_fill_price, self._client_id, why_held,
            mkt_cap_price,
        )

    def tick_price(self, req_id, tick_type, price, attrib=None):
        """A price, delivered with the size that belongs to it.

        This engine states a price and a size as the reference client does —
        two ticks. ib_async holds a quote as one thing and blanks a side whose
        size is zero, so the two are paired here rather than each arrival
        wiping the other half.
        """
        held = self._quotes.setdefault(req_id, {})
        held[tick_type] = price
        size = held.get(self._SIZE_OF.get(tick_type, -1), 0.0)
        self._wrapper.priceSizeTick(req_id, tick_type, price, size)

    def tick_size(self, req_id, tick_type, size):
        """A size, delivered with the price that belongs to it.

        A size can change while the price does not, so the price half comes
        from what was last stated. Where nothing has stated one, their own
        "no price" is what is sent: a zero there is a market quoted at
        nothing, which is a different thing from a market not yet quoted.
        """
        held = self._quotes.setdefault(req_id, {})
        held[tick_type] = size
        price_type = self._PRICE_OF.get(tick_type)
        if price_type is None:
            self._wrapper.tickSize(req_id, tick_type, size)
            return
        unstated = getattr(self._wrapper, "defaultEmptyPrice", -1)
        self._wrapper.priceSizeTick(
            req_id, price_type, held.get(price_type, unstated), size
        )

    #: Where their wrapper names a callback something other than the
    #: reference client does, and the two it does not carry at all: display
    #: groups belong to a window, and there is none here or there.
    _THEIR_NAME = {
        "real_time_bar": "realtimeBar",
        "commission_and_fees_report": "commissionReport",
        "display_group_list": None,
        "display_group_updated": None,
    }

    # Under both spellings. This engine looks for the reference client's name
    # first, so a callback answered here under one spelling only would be
    # reached past — straight to their wrapper, and the translation skipped.
    orderStatus = order_status
    tickPrice = tick_price
    tickSize = tick_size
    histogramData = histogram_data

    def error(self, req_id, when, code, text, advanced=""):
        """A refusal, in the shape their wrapper declares.

        This engine states when the venue said it, as the current reference
        client does. `ib_async` predates that argument and declares four, so
        their wrapper is handed four: passed five it raises on the first error
        or notice of the session, and a callback that raises here closes the
        session — which turned any refusal at all into a disconnection.
        """
        self._wrapper.error(req_id, code, text, advanced)

    def __getattr__(self, name):
        # Under either spelling: this engine calls a callback by the name it
        # holds it under, and their wrapper declares the reference client's.
        if name in self._THEIR_NAME:
            named = self._THEIR_NAME[name]
            if named is None:
                return lambda *args: None
            # Rebuilt on the way through, like every other callback. Handed
            # over as it stands, a fill carries this engine's own cost record
            # and ib_async's wrapper reads a field its own record spells
            # differently, so the cost is dropped.
            method = getattr(self._wrapper, named)
        else:
            method = getattr(self._wrapper, name, None)
            if method is None:
                words = name.split("_")
                camel = words[0] + "".join(w.title() for w in words[1:])
                method = getattr(self._wrapper, camel, None)
        if method is None:
            raise AttributeError(name)

        def carrying(*args):
            return method(*(_as_theirs(a) for a in args))

        return carrying


#: What they mean by "the caller set nothing".
#:
#: Both sides spell it the same way, and this engine states it as zero on most
#: fields but keeps the sentinel on the ones where zero is a value a caller can
#: mean. Which fields those are is not a list to keep by hand — the engine's own
#: default says so, and a list went stale: an order carrying their unset
#: `basisPoints` was rewritten to zero, which this engine reads as a caller
#: stating a field the protocol cannot carry, so every order through their API
#: was refused for setting something nobody set.
_UNSET_DOUBLE = 1.7976931348623157e308
_UNSET_INTEGER = 2147483647


def _as_ours(value):
    """An `ib_async` object, rebuilt as this engine's type of the same name."""
    import dataclasses

    if value is None:
        # Their optional lists arrive as None; every request here takes a
        # list, and an absent one is an empty one.
        return []

    named = isinstance(value, tuple) and hasattr(value, "_fields")
    if isinstance(value, (list, tuple)) and not named:
        # A list of ib_async objects is a list of objects to rebuild, not a
        # value to hand across whole. Handed across, an algo's parameters and a
        # combination's routing arrive as ib_async types and are refused, so
        # the order carries the strategy without what tunes it.
        return [_as_ours(item) for item in value]

    if named:
        # A tag and its value is a record they spell as a tuple. Read as a
        # sequence it becomes two loose strings, which is not what either side
        # means by one; read as a record it is the pair this engine holds.
        rebuilt = getattr(_ibx, type(value).__name__, None)
        return value if rebuilt is None else rebuilt(*value)

    if not dataclasses.is_dataclass(value):
        return value
    # Under its own name, or the name of what it is a kind of: their `Stock`
    # and `Forex` are contracts, and this engine holds one type for all of
    # them.
    ours = next(
        (
            found
            for kind in type(value).__mro__
            if (found := getattr(_ibx, kind.__name__, None)) is not None
        ),
        None,
    )
    if ours is None:
        return value
    made = ours()
    for field in dataclasses.fields(value):
        held = getattr(value, field.name, None)
        if held is None:
            continue
        # A field still at their default was not stated by whoever placed the
        # order, and the two sides do not agree on what a default is — theirs
        # says an order does not use the automatic hedge price, this engine's
        # says it does, and neither was asked for. Carrying theirs over states
        # a field nobody set, and where the protocol has nowhere to send it the
        # order is refused for it. What the caller actually set is carried; the
        # rest is left to this engine's own default.
        if field.default is not dataclasses.MISSING and held == field.default:
            continue
        # Past the check above, this field was set to something other than
        # their default — including, from a program that spells it out, their
        # unset sentinel itself. That still means nothing was set, so it is
        # translated the same way.
        #
        # `made` is untouched at this field until the line below, so what it
        # holds here is this engine's own default — which is what says whether
        # the sentinel means anything on this field.
        unset_here = getattr(made, field.name, None)
        if held == _UNSET_DOUBLE and unset_here != _UNSET_DOUBLE:
            held = 0.0
        elif (isinstance(held, int) and not isinstance(held, bool)
              and held == _UNSET_INTEGER and unset_here != _UNSET_INTEGER):
            held = 0
        carried = _as_ours(held)
        try:
            setattr(made, field.name, carried)
        except (AttributeError, TypeError, ValueError) as why:
            # Only fields the caller set reach here. A field that cannot be
            # carried is one the order goes out without, so it is raised rather
            # than swallowed: an algo without its parameters or a commission
            # directed nowhere is an order on terms nobody stated.
            raise ValueError(
                f"{type(value).__name__}.{field.name} was set to {held!r}, "
                f"which this client cannot carry: {why}"
            ) from why

    return made


def attach(ib, username="", password="", paper=True, session_file=None,
           client_id=None, readonly=False):
    """Point an `ib_async.IB` at this engine, and hand it back.

    The credentials are this session's; left out, `IB_USERNAME` and
    `IB_PASSWORD` are used.

    The session is kept between runs, under this account's own file in
    ``~/.ibx``. A venue answers a request that names a session it still holds
    with a challenge rather than a whole handshake, so a program that starts
    often is one login rather than one per start, and needs a person to approve
    far fewer of them. Name another path to move it, or pass ``False`` to keep
    nothing and log in fully every time.

    Order ids are counted from what the account is working, which the venue
    names at every connect, so nothing about them is kept between runs.
    """
    if session_file is None:
        who = username or os.environ.get("IB_USERNAME", "")
        kind = "paper" if paper else "live"
        session_file = str(pathlib.Path.home() / ".ibx" / f"session-{who}-{kind}")
    elif session_file is False:
        session_file = None
    ib.client = IbxClient(ib.wrapper, username, password, paper, session_file,
                          client_id, readonly)
    ib.wrapper.client = ib.client

    # An order they leave unnumbered is numbered from what the account has
    # used, rather than from the counter their requests come out of. The venue
    # refuses an order id a fill has spent, and their counter knows nothing
    # about which those are.
    placing = ib.placeOrder

    def place_order(contract, order):
        if not getattr(order, "orderId", 0):
            order.orderId = ib.client.next_order_id()
        return placing(contract, order)

    ib.placeOrder = place_order
    return ib
