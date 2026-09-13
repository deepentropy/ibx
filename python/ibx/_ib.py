"""The shape of the widely used asynchronous wrapper, over this client.

Method names, argument names and return types follow that wrapper so a program
written against it runs here. Where it returns a list, this returns a list; a
method it spells ``reqHistoricalData`` is spelled that here too.

This is a facade over ``EClient``, not a second client. It holds one session and
answers from it. What it cannot yet answer it says so plainly rather than
returning an empty list, because an empty list is indistinguishable from a
market with nothing in it.
"""

from __future__ import annotations

import threading
import time

from ._state import BracketOrder, HistoricalNews, HistoricalSchedule, LiveState, TradingSession
from .ibx import Contract, EClient


def _refuse_options(named: str, given) -> None:
    """A free-form option list this request cannot carry.

    Every one of these was accepted and dropped on the floor, so a caller who
    tuned a request with one was answered by an untuned request and had no way
    to tell. Empty or absent is what every ordinary call passes, and that is
    taken; anything in it is said out loud.
    """
    if given:
        raise NotImplementedError(
            f"{named}={given!r} is not carried here: this request has no "
            "free-form option list to send it under, so the request would go "
            "out without it and answer something other than what was asked"
        )


def _wait_until(done, timeout: float, every: float = 0.01) -> bool:
    """Poll until it reports done or the time is up, and say which happened.

    The answer is taken whether or not it finished, so a caller that asks with
    a short timeout gets what arrived rather than an error. Four requests waited
    like this with the loop written out each time.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and not done():
        time.sleep(every)
    return done()


class Client:
    """One session, asked questions directly."""

    def __init__(self) -> None:
        self.wrapper = LiveState()
        self.client = EClient(self.wrapper)
        self._pump: threading.Thread | None = None
        self._stop = threading.Event()
        # Quote subscriptions only. `ticker()` falls back to scanning this
        # for a matching contract id, so anything else in here answers a
        # question about a quote with a request that is not one. Depth,
        # bars and tick-by-tick were written here and never read, and two
        # of them were never removed, so a cancelled stream went on
        # matching that scan.
        self._subscribed: dict[int, object] = {}
        # Keyed by the kind of stream as well as the contract. One contract can
        # carry a quote, a book, bars and a tick stream at once, and one slot
        # per contract meant the newest request overwrote the rest: cancelling
        # any of them sent the wrong kind of cancel under another request's id,
        # withdrew a subscription the caller still wanted, and left the one
        # they asked to stop running at the venue.
        self._by_contract: dict[tuple[str, int], tuple[int, object]] = {}
        # Which request id each P&L subscription was made under, so a cancel
        # names the one it was asked about.
        self._pnl_reqs: dict[tuple[str, str], int] = {}
        self._pnl_single_reqs: dict[tuple[str, str, int], int] = {}
        # Which id the calendar was asked for under, so a cancel names it.
        self._wsh_meta = 0
        self._wsh_event = 0
        self._req_id = 0
        # One guard over the counter and the registry beside it. The wheel is
        # built free-threaded, so two callers really do run here at once: the
        # counter handed out the same number to both — thirty thousand
        # collisions in forty thousand calls, measured — and two subscriptions
        # under one number overwrite each other, so a cancel for the first
        # stopped the second and left the first running at the venue.
        self._registry = threading.Lock()
        # What a wait with no timeout of its own waits for. `setTimeout` names
        # another.
        self._timeout = 60

    def _next_req_id(self) -> int:
        with self._registry:
            self._req_id += 1
            return self._req_id

    def _remember(self, kind: str, contract, req_id: int) -> None:
        """Keep the contract, not only its address.

        The key is the address, and an address is only that object's for as
        long as the object lives. Kept without it, the entry outlived the
        contract, the next contract was built at the same address, and a
        cancel naming that one stopped the stream belonging to the first.
        """
        with self._registry:
            self._by_contract[(kind, id(contract))] = (req_id, contract)

    def _recall(self, kind: str, contract) -> int | None:
        with self._registry:
            entry = self._by_contract.get((kind, id(contract)))
        return entry[0] if entry else None

    def _forget(self, kind: str, contract) -> int | None:
        with self._registry:
            entry = self._by_contract.pop((kind, id(contract)), None)
        return entry[0] if entry else None

    # -- session ---------------------------------------------------------

    def connect(
        self,
        host="",
        port=0,
        clientId=1,
        timeout=4,
        readonly=False,
        account="",
        username="",
        password="",
        paper=True,
        settings=None,
    ):
        """Open the session.

        The host and port name a local process in the wrapper this follows.
        There is no local process here — this client logs in itself — so they
        are accepted and ignored, and the credentials are given here instead.
        A program that already holds a session needs no edit; one that relied
        on a running gateway to have logged in supplies the login here.

        ``timeout`` is accepted and ignored, and this is the one place that
        matters. In the wrapper this follows it bounds a socket opening to a
        process on the same machine, which takes milliseconds. Here the wait it
        would bound is a person approving a login on their phone, and four
        seconds of that is every first login refused. What bounds this call is
        ``ib_key_timeout_secs`` on the client underneath, which is that wait
        and defaults to about eighteen minutes; a program that cannot block for
        it runs this on a worker of its own.

        ``readonly`` is carried through: a read-only session refuses to send
        anything that changes a position, and says so rather than appearing to
        have sent it.

        ``settings`` is what this session runs under, by the names
        :func:`ibx.configure` uses — ``{"timezone": "Europe/Zurich"}``. Stated
        here they belong to this session; stated through ``configure`` they
        belong to the process, and are what a session that states none falls
        back to.
        """
        # `timeout` is not carried: see above. It names a socket opening in
        # the wrapper this follows and a person's phone here, and the two do
        # not take the same number.
        del host, port, timeout, account
        self.client.connect(
            client_id=clientId,
            username=username,
            password=password,
            paper=paper,
            readonly=readonly,
            settings={k: str(v) for k, v in (settings or {}).items()},
        )
        self._start_pump()
        # The wrapper this follows subscribes to the account as it connects,
        # and a program reads `accountValues()` straight afterwards. Without
        # this the account is silent until something asks, and an empty list
        # reads as an account holding nothing.
        self.client.req_account_updates(True, "")
        # And what the account holds. The wrapper this follows asks for both as
        # it connects, and a program reads `positions()` straight afterwards.
        # Unasked, that answered with an empty list on an account holding five
        # positions — which reads as a flat account, not as one that has not
        # been asked, and `portfolio()` beside it said five all along.
        self.client.req_positions()
        # And what it already has working. The wrapper this follows asks for
        # these as it connects too, and only where the session may act on them:
        # a read-only one is not going to modify an order. Unasked,
        # `openOrders()` answered with an empty list on an account with an
        # order resting — ten seconds after connecting, not merely before the
        # answer landed — while `reqAllOpenOrders()` beside it named it.
        if not readonly:
            self.client.req_all_open_orders()
            self.client.req_completed_orders(False)
        return self

    def _start_pump(self) -> None:
        """Keep dispatch running so the live state stays current.

        A daemon thread, so it never holds a program open. It owns dispatch;
        the calls that answer take their replies by request id and are left
        alone by it, so the two run together rather than competing.
        """
        if self._pumping():
            return
        self._stop.clear()

        def pump():
            try:
                # Loops until the session closes, releasing the interpreter
                # lock while it waits, so it costs a thread and not a core.
                self.client.run()
            except Exception:
                # A session that has gone ends the pump. connect() starts a new
                # one; nothing here tries to reconnect behind the caller's back.
                return

        self._pump = threading.Thread(target=pump, name="ibx-pump", daemon=True)
        self._pump.start()

    def _pumping(self) -> bool:
        return self._pump is not None and self._pump.is_alive()

    def _answered(self, ask, kind) -> None:
        """Ask, and wait for the answer to end.

        Every answer arrives on the pump, in the order asked, and never inside
        the call that asked: read straight after asking, the state held what
        it held before the question. A session whose pump has ended is not
        waited on — the answer is not coming, and the caller gets what stands.
        """
        before = self.wrapper.ended(kind)
        ask()
        _wait_until(
            lambda: self.wrapper.ended(kind) > before or not self._pumping(),
            self._timeout,
        )

    def disconnect(self) -> None:
        self._stop.set()
        self.client.disconnect()
        if self._pump is not None:
            self._pump.join(timeout=2.0)
            self._pump = None

    # -- what the session currently holds --------------------------------

    def positions(self):
        """What the account holds.

        Asked for as the session opens and waited for here, the same way the
        account's figures are: an empty list is what a flat account looks like,
        so handing one back before the venue has answered says the wrong thing
        with no way to tell.
        """
        deadline = time.monotonic() + 5
        while (
            time.monotonic() < deadline
            and not self.wrapper.account_download_finished()
        ):
            time.sleep(0.05)
        return self.wrapper.snapshot_positions()

    def portfolio(self):
        return self.wrapper.snapshot_portfolio()

    def accountValues(self):
        """Every figure the venue states about the account.

        The same figures :meth:`accountSummary` hands back, and waited for the
        same way. The subscription is made as the session opens and the answer
        lands a moment later, so a program that connects and reads in the same
        breath — which is how one is written — was handed an empty list and
        nothing to say whether the account held nothing or had not yet spoken.
        """
        return self._account_figures()

    def trades(self):
        return self.wrapper.snapshot_trades()

    def openTrades(self):
        return [t for t in self.wrapper.snapshot_trades() if t.isActive()]

    def orders(self):
        return [t.order for t in self.wrapper.snapshot_trades() if t.order is not None]

    def openOrders(self):
        return [t.order for t in self.openTrades() if t.order is not None]

    def fills(self):
        """Every fill this session has seen.

        The account's own history is not among them: the venue names
        executions alongside the orders it replays when a session opens, and
        one whose order has since completed is not replayed. See
        :meth:`reqExecutions`.
        """
        return self.wrapper.snapshot_fills()

    def executions(self):
        return [f.execution for f in self.wrapper.snapshot_fills()]

    def managedAccounts(self):
        return self.wrapper.snapshot_accounts()

    def tickers(self):
        return self.wrapper.snapshot_tickers()

    def ticker(self, contract):
        """The quote for a contract already subscribed to, or nothing."""
        req_id = self._recall("quote", contract)
        if req_id is None:
            # Under the same guard as everything else the registry holds. The
            # wheel is free-threaded, so a subscription opened or withdrawn on
            # another thread really does resize this map mid-scan, and the
            # scan is a plain quote read that would have raised on the
            # caller's own thread.
            with self._registry:
                pairs = list(self._subscribed.items())
            for rid, c in pairs:
                if getattr(c, "conId", None) and getattr(c, "conId", None) == getattr(contract, "conId", None):
                    req_id = rid
                    break
        return self.wrapper.ticker_for(req_id) if req_id is not None else None

    @property
    def pendingTickers(self):
        return self.wrapper.take_pending()

    def reqMktData(
        self, contract, genericTickList="", snapshot=False,
        regulatorySnapshot=False, mktDataOptions=None,
    ):
        """Subscribe to a contract's quote and hand back the object it fills.

        The object is returned before anything has arrived, and fills in as
        ticks reach it. Its fields are ``None`` until the venue sends them, so a
        caller can tell "no bid yet" from "a bid of zero".
        """
        _refuse_options("mktDataOptions", mktDataOptions)
        # One stream per contract object, and the same one back. The registry
        # holds a single request against the object, so opening a second left
        # the first running with nothing holding its id: the cancel names the
        # object and withdraws whichever was registered last.
        #
        # A snapshot is not registered and never answers a later ask. It ends
        # on its own, so the entry would name a request that is already over —
        # and it would take a running stream's place, as `reqTickers` is
        # careful not to.
        # A regulatory snapshot is one too, and the request builder treats it
        # as one, so it takes the same path.
        if not (snapshot or regulatorySnapshot):
            already = self._recall("quote", contract)
            if already is not None:
                return self.wrapper.ticker_for(already)
        req_id, ticker = self._start_quote(
            contract, genericTickList, snapshot, regulatorySnapshot
        )
        # Under its own name, so it neither takes the stream's place nor
        # answers a later ask for one — and a snapshot on a contract that
        # never quotes can still be withdrawn by naming the contract, which
        # is the only handle a caller who did not keep the id has.
        one_shot = snapshot or regulatorySnapshot
        self._remember("snapshot" if one_shot else "quote", contract, req_id)
        return ticker

    def _start_quote(
        self, contract, genericTickList="", snapshot=False, regulatory=False
    ):
        """Subscribe and hand back the request id along with the quote.

        The id is what a cancel names. A caller who owns it can withdraw its
        own subscription without going through the per-contract registry,
        which holds one quote per contract object and would otherwise lose
        whichever came first.
        """
        req_id = self._next_req_id()
        with self._registry:
            self._subscribed[req_id] = contract
        ticker = self.wrapper.bind_ticker(req_id, contract)
        self.client.req_mkt_data(
            req_id, contract, genericTickList, snapshot, regulatory, []
        )
        return req_id, ticker

    def cancelMktData(self, contract):
        # The stream first: a snapshot beside it ends on its own, and a caller
        # cancelling a contract it is streaming means the stream.
        req_id = self._forget("quote", contract)
        if req_id is None:
            req_id = self._forget("snapshot", contract)
        if req_id is None:
            return
        with self._registry:
            self._subscribed.pop(req_id, None)
        self.client.cancel_mkt_data(req_id)



    # -- the rest of the live state ---------------------------------------

    def accountSummary(self, account="", timeout=5):
        """What the venue says the account is worth.

        The subscription is made as the session opens, so this normally has
        its answer already. On the first call it may not have arrived yet;
        waiting for it is what tells an account with nothing in it from one
        that has not spoken.
        """
        del account
        return self._account_figures(timeout)

    def _account_figures(self, timeout=5):
        """The account's figures, once the venue has stated all of them.

        Waits for the venue to say it has stated every figure, not for the
        first of them. Stopping at the first handed back one field of an
        account and read the same as a whole one, so a caller sizing a
        position off net liquidation sized it off whatever had landed.
        """
        _wait_until(self.wrapper.account_download_finished, timeout, every=0.05)
        return self.wrapper.snapshot_account_values()

    def pnl(self, account="", modelCode=""):
        del account, modelCode
        return self.wrapper.snapshot_pnl()

    def pnlSingle(self, account="", modelCode="", conId=0):
        del account, modelCode, conId
        return self.wrapper.snapshot_pnl_single()

    def newsBulletins(self):
        return self.wrapper.snapshot_bulletins()

    def newsTicks(self):
        return self.wrapper.snapshot_news_ticks()

    def realtimeBars(self):
        return self.wrapper.snapshot_bars()

    def cancelPnL(self, account="", modelCode=""):
        """Withdraw the P&L subscription this account and model asked for.

        The id the request was made under is what the venue and the client
        underneath both match a cancel against. This used to send the newest
        request id of any kind, so any request in between made the cancel name
        something else: the P&L stream carried on and nothing said so.
        """
        req_id = self._pnl_reqs.pop((account, modelCode), None)
        if req_id is not None:
            self.client.cancel_pnl(req_id)

    def cancelPnLSingle(self, account="", modelCode="", conId=0):
        """As above, for one position's P&L."""
        req_id = self._pnl_single_reqs.pop((account, modelCode, conId), None)
        if req_id is not None:
            self.client.cancel_pnl_single(req_id)

    # -- waiting -----------------------------------------------------------

    def waitOnUpdate(self, timeout=0):
        """Wait for something to arrive, and say whether anything did.

        A pump is already running, so this watches what it records rather than
        turning an event loop. False means the wait ran out with the session
        silent, which is what a program checking that its feed is alive is
        asking. Returning True regardless made that check one that could not
        fail, so a dead feed read exactly like a busy one.

        With no timeout, the one set by ``setTimeout`` is used.
        """
        seen = self.wrapper.updates()
        deadline = time.monotonic() + (timeout or self._timeout)
        while time.monotonic() < deadline:
            if self.wrapper.updates() != seen:
                return True
            time.sleep(0.005)
        return False

    def sleep(self, secs=0.02):
        time.sleep(secs)
        return True

    def setTimeout(self, timeout=60):
        """How long a wait with no timeout of its own waits for."""
        self._timeout = timeout

    def loopUntil(self, condition=None, timeout=0):
        """Run until a condition holds, or until the time is up.

        Yields as the wrapper this follows does, so ``for _ in ib.loopUntil(...)``
        reads the same.
        """
        deadline = time.monotonic() + timeout if timeout else None
        while True:
            if condition is not None and condition():
                return
            if deadline is not None and time.monotonic() > deadline:
                return
            yield self
            time.sleep(0.01)

    # -- questions that answer ---------------------------------------------

    def reqTickers(self, *contracts, regulatorySnapshot=False, timeout=5):
        """Subscribe, wait for each quote to arrive, then unsubscribe.

        A snapshot rather than a stream. A contract whose quote never arrives is
        returned with its fields unset rather than dropped, so the result lines
        up with what was asked for.
        """
        # Its own ids, kept here rather than in the per-contract registry. A
        # snapshot on a contract already streaming would otherwise overwrite
        # the stream's id there, and cancelling the snapshot would drop the
        # entry and leave the stream running with nothing to name it by.
        started = [
            self._start_quote(c, snapshot=True, regulatory=regulatorySnapshot)
            for c in contracts
        ]
        tickers = [t for _, t in started]
        deadline = time.monotonic() + timeout
        # A snapshot is asked for the quote, so that is what this waits for.
        # Waiting for any field at all is satisfied by the previous close,
        # which arrives first, and cancels before the quote lands.
        while time.monotonic() < deadline:
            if all(t.hasBidAsk() or t.last is not None for t in tickers):
                break
            time.sleep(0.01)
        for req_id, _ in started:
            with self._registry:
                self._subscribed.pop(req_id, None)
            self.client.cancel_mkt_data(req_id)
        return tickers

    def whatIfOrder(self, contract, order, timeout=5):
        """What the venue says an order would cost, without sending it.

        The order is marked as a question rather than an instruction, so nothing
        reaches the market. Returns what the venue answered about margin and
        commission, or raises if it answered nothing.

        The caller's order is handed back as they wrote it. The mark used to
        stay on it, so the next time they placed that same order it went out as
        another question and nothing reached the market. The record the preview
        left behind is dropped for the same reason: a question is not an order,
        and the session reported one working that nobody had sent.
        """
        stated = getattr(order, "whatIf", False), getattr(order, "orderId", 0)
        order_id = stated[1] or self.client.next_order_id()
        order.whatIf = True
        order.orderId = order_id
        try:
            trade = self.placeOrder(contract, order)
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if trade.orderState is not None:
                    return trade.orderState
                time.sleep(0.01)
            raise TimeoutError("the venue did not answer what the order would cost")
        finally:
            order.whatIf, order.orderId = stated
            self.wrapper.forget_trade(order_id)

    def reqScannerData(self, subscription, scannerSubscriptionOptions=None, scannerSubscriptionFilterOptions=None, timeout=5):
        """Run a scan and hand back its rows, in the order the venue ranked them.

        Waits for the venue to say it has named every row. Stopping at the
        first one it had named was a scan of one out of fifty, with nothing to
        say the rest were still coming.
        """
        req_id = self.reqScannerSubscription(
            subscription, scannerSubscriptionOptions, scannerSubscriptionFilterOptions,
        )
        _wait_until(lambda: self.wrapper.scanner_finished(req_id), timeout)
        rows = self.wrapper.take_scanner(req_id)
        self.cancelScannerSubscription(req_id)
        self.wrapper.forget_scanner(req_id)
        return rows

    def reqScannerSubscription(self, subscription, scannerSubscriptionOptions=None, scannerSubscriptionFilterOptions=None):
        """Start a scan. The filters go with it; dropped, the scan that runs is
        a broader one than the caller described."""
        req_id = self._next_req_id()
        self.client.req_scanner_subscription(
            req_id, subscription,
            scannerSubscriptionOptions or [],
            scannerSubscriptionFilterOptions or [],
        )
        return req_id

    def cancelScannerSubscription(self, req_id):
        self.client.cancel_scanner_subscription(req_id)

    # -- orders built here rather than asked for ---------------------------

    def bracketOrder(
        self, action, quantity, limitPrice, takeProfitPrice, stopLossPrice, **kwargs
    ):
        """A parent and its two exits, each numbered and each naming its parent.

        Built here; nothing is sent. Each order is given its own id and both
        exits name the parent's, which is what holds them at the venue until
        the parent fills and what makes filling one withdraw the other. Left
        unnumbered they are three unrelated orders, and placing them sends a
        stop-loss to the market with no position behind it.

        The wrapper this follows holds the first two back with transmit=False
        as well, and so does this: the field is one that client sends to its
        counterpart rather than to the venue, and the counterpart keeps such an
        order without working it. Kept here for the same reason, and sent when
        an order in the same family transmits. The parent id is what links them
        once they go.
        """
        from .ibx import Order

        reverse = "SELL" if action.upper() == "BUY" else "BUY"
        parent = Order()
        parent.orderId = self.client.next_order_id()
        parent.action = action
        parent.orderType = "LMT"
        parent.totalQuantity = quantity
        parent.lmtPrice = limitPrice

        take_profit = Order()
        take_profit.orderId = self.client.next_order_id()
        take_profit.parentId = parent.orderId
        take_profit.action = reverse
        take_profit.orderType = "LMT"
        take_profit.totalQuantity = quantity
        take_profit.lmtPrice = takeProfitPrice

        stop_loss = Order()
        stop_loss.orderId = self.client.next_order_id()
        stop_loss.parentId = parent.orderId
        stop_loss.action = reverse
        stop_loss.orderType = "STP"
        stop_loss.totalQuantity = quantity
        stop_loss.auxPrice = stopLossPrice

        for o in (parent, take_profit, stop_loss):
            for k, v in kwargs.items():
                setattr(o, k, v)
        return BracketOrder(parent, take_profit, stop_loss)

    def oneCancelsAll(self, orders, ocaGroup, ocaType):
        """Link orders so that a fill on one withdraws the rest."""
        for o in orders:
            o.ocaGroup = ocaGroup
            o.ocaType = ocaType
        return orders

    # -- carried, and answered by the venue as not served ------------------

    def calculateImpliedVolatility(self, contract, optionPrice, underPrice, **kwargs):
        del kwargs
        return self.client.calculate_implied_volatility(
            self._next_req_id(), contract, optionPrice, underPrice, []
        )

    def calculateOptionPrice(self, contract, volatility, underPrice, **kwargs):
        del kwargs
        return self.client.calculate_option_price(
            self._next_req_id(), contract, volatility, underPrice, []
        )

    def requestFA(self, faDataType):
        return self.client.request_fa(faDataType)

    def replaceFA(self, faDataType, xml):
        return self.client.replace_fa(self._next_req_id(), faDataType, xml)

    def reqWshMetaData(self):
        self._wsh_meta = self._next_req_id()
        return self.client.req_wsh_meta_data(self._wsh_meta)

    def cancelWshMetaData(self, reqId=0):
        """Withdraw the calendar request this session made.

        Falling back to the newest request id of any kind, any request in
        between made the cancel name something else: the calendar carried on
        and an unrelated subscription was withdrawn in its place.
        """
        return self.client.cancel_wsh_meta_data(reqId or self._wsh_meta)

    def reqWshEventData(self, data):
        self._wsh_event = self._next_req_id()
        return self.client.req_wsh_event_data(self._wsh_event, data)

    def cancelWshEventData(self, reqId=0):
        """As above, for the events themselves."""
        return self.client.cancel_wsh_event_data(reqId or self._wsh_event)

    def getWshMetaData(self):
        return self.reqWshMetaData()

    def getWshEventData(self, data):
        return self.reqWshEventData(data)

    # -- orders ----------------------------------------------------------

    def placeOrder(self, contract, order):
        """Send an order and hand back the record of it.

        The record is returned before the venue has said anything, and its
        status moves under the caller as the venue answers. That is what makes
        it worth holding on to rather than reading a return code.

        A contract with no ``conId`` is resolved before the order is sent. That
        resolution is a request and an answer, so this call blocks until it
        completes. Call ``qualifyContracts`` once and place against the result,
        as the reference client's examples do, and this call does not block.
        """
        order_id = getattr(order, "orderId", 0) or self.client.next_order_id()
        try:
            order.orderId = order_id
        except AttributeError:
            pass
        trade = self.wrapper.register_order(order_id, contract, order)
        # A transport that will not carry the order says so on the error
        # callback and returns, which is how the reference client answers a
        # request it refuses. Said during the call, it is about this order not
        # being sent — a rejection from the venue arrives later — so the record
        # goes with it. Kept, `openTrades()` lists an order working at a venue
        # that never received one.
        before = len(self.wrapper.errors)
        self.client.place_order(order_id, contract, order)
        refused = any(
            err[0] == order_id for err in self.wrapper.errors[before:]
        )
        if refused:
            self.wrapper.forget_trade(order_id)
        return trade

    def cancelOrder(self, order, manualCancelOrderTime=""):
        order_id = getattr(order, "orderId", order)
        self.client.cancel_order(order_id, manualCancelOrderTime)
        return self.wrapper.trade_for(order_id)

    def reqGlobalCancel(self):
        self.client.req_global_cancel()

    def reqAutoOpenOrders(self, autoBind=True):
        self.client.req_auto_open_orders(autoBind)

    def reqCompletedOrders(self, apiOnly=False, timeout=5):
        """The orders the venue has finished with.

        Waits for the venue to say it has named them all, and hands back what
        it named. This used to return the session's ordinary trades, so an
        answer that arrived and an answer that never came looked the same.
        """
        self.wrapper.forget_completed()
        self.client.req_completed_orders(apiOnly)
        _wait_until(self.wrapper.completed_orders_finished, timeout)
        return self.wrapper.snapshot_completed()

    def exerciseOptions(
        self, contract, exerciseAction, exerciseQuantity, account="", override=False,
    ):
        self.client.exercise_options(
            self._next_req_id(), contract, exerciseAction, exerciseQuantity,
            account, 1 if override else 0,
        )

    # -- streams the caller reads through the live state ------------------

    def reqMktDepth(self, contract, numRows=5, isSmartDepth=False, mktDepthOptions=None):
        _refuse_options("mktDepthOptions", mktDepthOptions)
        req_id = self._next_req_id()
        self._remember("depth", contract, req_id)
        self.client.req_mkt_depth(req_id, contract, numRows, isSmartDepth, [])
        return req_id

    def cancelMktDepth(self, contract, isSmartDepth=False):
        req_id = self._forget("depth", contract)
        if req_id is not None:
            self.client.cancel_mkt_depth(req_id, isSmartDepth)

    def reqRealTimeBars(self, contract, barSize=5, whatToShow="TRADES", useRTH=True, realTimeBarsOptions=None):
        _refuse_options("realTimeBarsOptions", realTimeBarsOptions)
        req_id = self._next_req_id()
        self._remember("bars", contract, req_id)
        self.client.req_real_time_bars(req_id, contract, barSize, whatToShow, useRTH, [])
        return req_id

    def cancelRealTimeBars(self, bars):
        req_id = bars if isinstance(bars, int) else self._forget("bars", bars)
        if req_id is not None:
            self.client.cancel_real_time_bars(req_id)

    def reqTickByTickData(self, contract, tickType="Last", numberOfTicks=0, ignoreSize=False):
        req_id = self._next_req_id()
        self._remember("ticks", contract, req_id)
        self.client.req_tick_by_tick_data(req_id, contract, tickType, numberOfTicks, ignoreSize)
        return req_id

    def cancelTickByTickData(self, contract, tickType="Last"):
        del tickType
        req_id = self._forget("ticks", contract)
        if req_id is not None:
            self.client.cancel_tick_by_tick_data(req_id)

    def reqMarketDataType(self, marketDataType):
        self.client.req_market_data_type(marketDataType)

    # -- account ----------------------------------------------------------

    def reqAccountSummary(self, group="All", tags="", timeout=5):
        """What the venue says the account is worth, tag by tag.

        The request is answered on its own callback, which this records. It
        used to hand back the running account values instead — a different set,
        arriving from a different subscription — while the rows actually asked
        for reached a callback nothing implemented and were dropped.
        """
        req_id = self._next_req_id()
        self.wrapper.forget_account_summary(req_id)
        self.client.req_account_summary(req_id, group, tags)
        _wait_until(lambda: self.wrapper.account_summary_finished(req_id), timeout)
        return self.wrapper.take_account_summary(req_id)

    def reqPnL(self, account="", modelCode=""):
        req_id = self._next_req_id()
        self._pnl_reqs[(account, modelCode)] = req_id
        self.client.req_pnl(req_id, account, modelCode)
        return req_id

    def reqPnLSingle(self, account, modelCode, conId):
        req_id = self._next_req_id()
        self._pnl_single_reqs[(account, modelCode, conId)] = req_id
        self.client.req_pnl_single(req_id, account, modelCode, conId)
        return req_id

    def reqAccountUpdatesMulti(self, account="", modelCode="", ledgerAndNLV=False):
        self.client.req_account_updates_multi(self._next_req_id(), account, modelCode, ledgerAndNLV)

    def reqUserInfo(self):
        self.client.req_user_info(self._next_req_id())

    # -- reference and news -----------------------------------------------

    def reqCurrentTime(self):
        self.client.req_current_time()

    def reqMarketRule(self, marketRuleId):
        self.client.req_market_rule(marketRuleId)

    def reqSecDefOptParams(self, underlyingSymbol, futFopExchange, underlyingSecType, underlyingConId):
        """The option chains an underlying has, one per venue.

        Returns them, as the client this follows does. Sending the request and
        returning nothing leaves a program that assigns the result holding
        nothing, indistinguishable from an underlying with no options.
        """
        return self.client.option_chains(
            underlyingSymbol, futFopExchange, underlyingSecType, underlyingConId,
        )

    def reqSmartComponents(self, bboExchange):
        self.client.req_smart_components(self._next_req_id(), bboExchange)

    def reqMktDepthExchanges(self):
        self.client.req_mkt_depth_exchanges()

    def reqNewsProviders(self):
        self.client.req_news_providers()

    def reqNewsArticle(self, providerCode, articleId, newsArticleOptions=None):
        _refuse_options("newsArticleOptions", newsArticleOptions)
        self.client.req_news_article(self._next_req_id(), providerCode, articleId, [])

    def reqHistoricalNews(
        self, conId, providerCodes, startDateTime, endDateTime,
        totalResults=100, historicalNewsOptions=None,
    ):
        _refuse_options("historicalNewsOptions", historicalNewsOptions)
        return [
            HistoricalNews(*row)
            for row in self.client.news_headlines(
                conId, providerCodes, startDateTime, endDateTime, totalResults,
            )
        ]

    def reqNewsBulletins(self, allMessages=True):
        self.client.req_news_bulletins(allMessages)

    def cancelNewsBulletins(self):
        self.client.cancel_news_bulletins()

    def reqScannerParameters(self):
        self.client.req_scanner_parameters()

    def reqHistoricalSchedule(self, contract, endDateTime="", durationStr="1 M", useRTH=True):
        """When a contract trades over a stretch of days.

        Each session is its opening, its close, and the day it belongs to, in
        the time zone the venue states them in, and the schedule states the
        window it actually covered — which is not the one asked for, since the
        venue answers a duration in its own trading days.
        """
        start, end, timezone, sessions = self.client.trading_schedule(
            contract, endDateTime, durationStr, useRTH
        )
        return HistoricalSchedule(
            startDateTime=start,
            endDateTime=end,
            timeZone=timezone,
            sessions=[TradingSession(*row) for row in sessions],
        )

    def reqHistoricalTicks(
        self, contract, startDateTime="", endDateTime="", numberOfTicks=1000,
        whatToShow="TRADES", useRth=True, ignoreSize=False, miscOptions=None,
    ):
        _refuse_options("miscOptions", miscOptions)
        self.client.req_historical_ticks(
            self._next_req_id(), contract, startDateTime, endDateTime,
            numberOfTicks, whatToShow, 1 if useRth else 0, ignoreSize, [],
        )

    def cancelHistoricalData(self, bars):
        req_id = bars if isinstance(bars, int) else self._by_contract.pop(("history", id(bars)), None)
        if req_id is not None:
            self.client.cancel_historical_data(req_id)

    # -- asking the venue to start sending it ----------------------------

    def reqPositions(self):
        self._answered(self.client.req_positions, "positions")
        return self.positions()

    def reqAccountUpdates(self, account=""):
        self.client.req_account_updates(True, account)
        return self.accountValues()

    def reqOpenOrders(self):
        self._answered(self.client.req_open_orders, "open_orders")
        return self.openTrades()

    def reqAllOpenOrders(self):
        self._answered(self.client.req_all_open_orders, "open_orders")
        return self.trades()

    def reqExecutions(self, execFilter=None):
        """The fills the filter names, out of what this session has seen.

        The filter goes to the client, which decides what the request replays.
        Dropped, the answer was every fill the session had seen: another
        client's, and ones before the cutoff the caller asked from.

        What it replays is this session's own record. The wrapper this follows
        asks the venue and is answered with the account's executions whoever
        made them; this protocol carries no such question, and the venue names
        executions only alongside the orders it replays when a session opens.
        So a fill made before this session opened, on an order since completed,
        is not among these — an empty answer means this session has seen none,
        not that the account has none.
        """
        req_id = self._next_req_id()
        self._answered(
            lambda: self.client.req_executions(req_id, execFilter),
            ("executions", req_id),
        )
        return self.wrapper.take_executions(req_id)

    def reqManagedAccts(self):
        self._answered(self.client.req_managed_accts, "managed_accounts")
        return self.managedAccounts()

    def isConnected(self) -> bool:
        return self.client.isConnected()

    # -- reference data --------------------------------------------------

    def reqContractDetails(self, contract: Contract):
        return self.client.contract_details(contract)

    def qualifyContracts(self, *contracts: Contract):
        """Fill in what the venue knows about each contract, above all its id.

        Returns the contracts it could resolve, and updates each argument in
        place, which is what the wrapper this follows does.
        """
        resolved = []
        for c in contracts:
            filled = self.client.qualify_contract(c)
            for name in (
                "conId", "symbol", "secType", "lastTradeDateOrContractMonth",
                "strike", "right", "multiplier", "exchange", "primaryExchange",
                "currency", "localSymbol", "tradingClass",
            ):
                try:
                    setattr(c, name, getattr(filled, name))
                except AttributeError:
                    pass
            resolved.append(c)
        return resolved

    def reqMatchingSymbols(self, pattern: str):
        return self.client.matching_symbols(pattern)

    # -- historical data -------------------------------------------------

    def reqHistoricalData(
        self,
        contract: Contract,
        endDateTime="",
        durationStr="1 D",
        barSizeSetting="1 min",
        whatToShow="TRADES",
        useRTH=True,
        formatDate=1,
        keepUpToDate=False,
        chartOptions=None,
    ):
        _refuse_options("chartOptions", chartOptions)
        # Refused rather than dropped. This answers once with the bars the
        # venue has, so a caller asking for a series that keeps updating would
        # be handed a snapshot that never changes; and it hands back the
        # moments as the venue spelled them, so one asking for seconds since
        # the epoch would be reading dates. Both are carried by the client
        # underneath, where the answer arrives on a callback.
        if keepUpToDate:
            raise NotImplementedError(
                "keepUpToDate is not carried here: this answers once with the "
                "bars the venue has. Ask the client for a series that keeps "
                "updating: ib.client.req_historical_data(...), whose bars "
                "arrive on historicalDataUpdate"
            )
        if formatDate != 1:
            raise NotImplementedError(
                f"formatDate={formatDate} is not carried here: the bars come "
                "back stating the moment as the venue spells it. Ask the "
                "client for another shape: ib.client.req_historical_data(...)"
            )
        return self.client.historical_data(
            contract,
            endDateTime,
            durationStr,
            barSizeSetting,
            whatToShow,
            1 if useRTH else 0,
        )

    def reqHeadTimeStamp(self, contract: Contract, whatToShow="TRADES", useRTH=True, formatDate=1):
        del formatDate
        return self.client.head_timestamp(contract, whatToShow, 1 if useRTH else 0)

    def reqHistogramData(self, contract: Contract, useRTH=True, period="3 days"):
        return self.client.histogram_data(contract, useRTH, period)

    def reqFundamentalData(self, contract: Contract, reportType: str, fundamentalDataOptions=None):
        _refuse_options("fundamentalDataOptions", fundamentalDataOptions)
        return self.client.fundamental_data(contract, reportType)

    # -- what has not been carried across yet ----------------------------

    def __getattr__(self, name: str):
        """Everything this does not name itself is the client's own.

        A session is the reference client with what it has been told kept
        beside it, so every request that client carries is a request this one
        carries. Reached this way it has the client's own shape — a request id
        to state, an answer arriving on a callback — because that is what it
        is; the calls named on this class are the ones with a shape of their
        own worth having.

        Python looks here only after failing to find the name on this class, so
        nothing defined above can be hidden by it.

        A name opening on an underscore is not carried across. The client keeps
        its own workings under those names and a session is not the place to
        reach them; forwarding them published the whole test-injection surface
        as though it were this class's own API.
        """
        if name.startswith("_"):
            raise AttributeError(name)
        try:
            return getattr(self.client, name)
        except AttributeError:
            raise AttributeError(
                f"neither this session nor the client underneath has {name!r}"
            ) from None


#: The session under the name the widely used asynchronous wrapper gives it, so
#: a program written against that one — and a test written against this one —
#: finds what it is looking for. The same class either way.
IB = Client
