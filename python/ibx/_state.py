"""What the session currently holds, kept current as it runs.

The wrapper this follows hands a caller live objects: ``ib.positions()`` is what
the account holds right now, and ``ib.trades()`` are orders whose status changes
under the caller as fills arrive. That is a different thing from a request that
answers once, and it is the part a program written against that wrapper leans on
most.

It works by keeping a pump running. The pump owns dispatch, and dispatch fires
the callbacks below, which record rather than act. A caller reading a list gets
a snapshot copied under a lock, so the list cannot change while it is being
read — a half-updated list is worse than a slightly old one.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Any

from .ibx import EWrapper


@dataclass
class Position:
    account: str = ""
    contract: Any = None
    position: float = 0.0
    avgCost: float = 0.0


@dataclass
class PortfolioItem:
    contract: Any = None
    position: float = 0.0
    marketPrice: float = 0.0
    marketValue: float = 0.0
    averageCost: float = 0.0
    unrealizedPNL: float = 0.0
    realizedPNL: float = 0.0
    account: str = ""


@dataclass
class AccountValue:
    account: str = ""
    tag: str = ""
    value: str = ""
    currency: str = ""


@dataclass
class CompletedOrder:
    """An order the venue has finished with, as it stated it."""

    contract: Any = None
    order: Any = None
    orderState: Any = None


@dataclass
class ScanData:
    """One row of a scan, named as the widely used wrapper names it."""

    rank: int = 0
    contractDetails: Any = None
    distance: str = ""
    benchmark: str = ""
    projection: str = ""
    legsStr: str = ""


@dataclass
class OrderStatus:
    orderId: int = 0
    status: str = ""
    filled: float = 0.0
    remaining: float = 0.0
    avgFillPrice: float = 0.0
    permId: int = 0
    parentId: int = 0
    lastFillPrice: float = 0.0
    clientId: int = 0
    whyHeld: str = ""
    mktCapPrice: float = 0.0


@dataclass
class Fill:
    contract: Any = None
    execution: Any = None
    commissionReport: Any = None
    time: str = ""


@dataclass
class Trade:
    """An order and what has become of it.

    The status changes under a caller holding this object, which is what makes
    it a trade rather than a receipt.
    """

    contract: Any = None
    order: Any = None
    orderStatus: OrderStatus = field(default_factory=OrderStatus)
    orderState: Any = None
    fills: list = field(default_factory=list)
    log: list = field(default_factory=list)

    def isActive(self) -> bool:
        return self.orderStatus.status in _ACTIVE

    def isDone(self) -> bool:
        return not self.isActive()

    def filled(self) -> float:
        return self.orderStatus.filled

    def remaining(self) -> float:
        return self.orderStatus.remaining


#: Statuses under which an order is still working. Taken from what the venue
#: sends rather than inferred: anything not named here has stopped.
_ACTIVE = frozenset({
    "PendingSubmit", "PendingCancel", "PreSubmitted", "Submitted", "ApiPending",
})


class LiveState(EWrapper):
    """Records what arrives instead of acting on it.

    Every accessor hands back a copy. A caller iterating a list while the pump
    appends to it would otherwise see the list change under the iteration.
    """

    def __init__(self) -> None:
        super().__init__()
        self._lock = threading.RLock()
        self._positions: dict[tuple[str, int], Position] = {}
        self._portfolio: dict[tuple[str, int], PortfolioItem] = {}
        self._account_values: dict[tuple[str, str, str], AccountValue] = {}
        self._summary: dict[int, list[AccountValue]] = {}
        self._summary_done: set[int] = set()
        self._account_done: set[str] = set()
        self._trades: dict[int, Trade] = {}
        self._fills: list[Fill] = []
        self._completed: list[CompletedOrder] = []
        self._completed_done = False
        self._fill_by_id: dict[str, Fill] = {}
        # The fills each execution request named, by its request id.
        self._executions: dict[int, list[Fill]] = {}
        # How many answers of each kind have ended. An answer arrives on the
        # pump, never inside the call that asked, so a caller reads this before
        # asking and waits for it to move.
        self._ended: dict[object, int] = {}
        self._accounts: list[str] = []
        self._tickers: dict[int, Ticker] = {}
        self._pnl: dict[int, tuple] = {}
        self._pnl_single: dict[int, tuple] = {}
        self._scanner: dict[int, list] = {}
        self._bulletins: list = []
        self._news_ticks: list = []
        self._bars: dict[int, list] = {}
        self._pending: set[int] = set()
        self._fields = None
        self._updates = 0
        self._scanner_done: set[int] = set()
        self.errors: list[tuple[int, int, str, str]] = []

    def updates(self) -> int:
        """How much has arrived, counted rather than described.

        A caller waiting for the session to say something compares this before
        and after. Nothing else about the number means anything; what matters
        is that it moves only when the venue has spoken, so a wait that got
        nothing can say so instead of always reporting success.
        """
        with self._lock:
            return self._updates

    # -- what the venue tells us -----------------------------------------

    def error(self, reqId, errorTime, errorCode, errorString, advancedOrderRejectJson=""):
        with self._lock:
            self._updates += 1
            self.errors.append((reqId, errorCode, errorString, advancedOrderRejectJson))

    def _end(self, kind) -> None:
        """One more answer of this kind has ended."""
        with self._lock:
            self._updates += 1
            self._ended[kind] = self._ended.get(kind, 0) + 1

    def ended(self, kind) -> int:
        """How many answers of this kind have ended so far."""
        with self._lock:
            return self._ended.get(kind, 0)

    def managedAccounts(self, accountsList):
        with self._lock:
            self._accounts = [a for a in accountsList.split(",") if a]
            self._end("managed_accounts")

    def position(self, account, contract, position, avgCost):
        with self._lock:
            self._updates += 1
            key = (account, getattr(contract, "conId", 0))
            if position == 0:
                self._positions.pop(key, None)
            else:
                self._positions[key] = Position(account, contract, position, avgCost)

    def positionEnd(self):
        self._end("positions")

    def updatePortfolio(
        self, contract, position, marketPrice, marketValue,
        averageCost, unrealizedPNL, realizedPNL, accountName,
    ):
        with self._lock:
            self._updates += 1
            # Keyed by the account as well as the contract, as the positions
            # above are. Under the contract alone, an advisor holding the same
            # instrument in two accounts saw one entry: whichever arrived last,
            # standing for both.
            key = (accountName, getattr(contract, "conId", 0))
            # A holding closed out is stated as a position of zero, which is
            # the venue saying it is gone. Kept, the portfolio showed an
            # instrument the account no longer held, for as long as the session
            # ran. The position callback beside this one already evicts.
            if position == 0:
                self._portfolio.pop(key, None)
                return
            self._portfolio[key] = PortfolioItem(
                contract, position, marketPrice, marketValue,
                averageCost, unrealizedPNL, realizedPNL, accountName,
            )

    def accountDownloadEnd(self, accountName):
        """The venue has stated every figure it is going to for this account.

        Recorded rather than dropped: the figures arrive one at a time, so a
        reader that stops at the first has one field of an account, and this is
        the only thing that says the rest have landed.
        """
        with self._lock:
            self._updates += 1
            self._account_done.add(accountName)

    def account_download_finished(self) -> bool:
        with self._lock:
            return bool(self._account_done)

    def accountSummary(self, reqId, account, tag, value, currency):
        """The answer to a summary request, which is not the running account
        feed and was reaching neither this class nor the caller."""
        with self._lock:
            self._updates += 1
            self._summary.setdefault(reqId, []).append(
                AccountValue(account, tag, value, currency)
            )

    def accountSummaryEnd(self, reqId):
        with self._lock:
            self._updates += 1
            self._summary_done.add(reqId)

    def account_summary_finished(self, req_id) -> bool:
        with self._lock:
            return req_id in self._summary_done

    def take_account_summary(self, req_id) -> list[AccountValue]:
        with self._lock:
            return list(self._summary.get(req_id, []))

    def forget_account_summary(self, req_id):
        """Drop a finished summary, so asking again is answered by the new one."""
        with self._lock:
            self._summary.pop(req_id, None)
            self._summary_done.discard(req_id)

    def updateAccountValue(self, key, val, currency, accountName):
        with self._lock:
            self._updates += 1
            self._account_values[(accountName, key, currency)] = AccountValue(
                accountName, key, val, currency
            )

    def openOrder(self, orderId, contract, order, orderState):
        with self._lock:
            self._updates += 1
            trade = self._trades.setdefault(orderId, Trade())
            trade.contract = contract
            trade.order = order
            trade.orderState = orderState
            trade.orderStatus.orderId = orderId

    def openOrderEnd(self):
        self._end("open_orders")

    def completedOrder(self, contract, order, orderState):
        """An order the venue has finished with.

        It arrives on a callback of its own and fell into the inherited no-op,
        so `reqCompletedOrders` handed back the session's ordinary trades
        instead and delivery read exactly like silence.
        """
        with self._lock:
            self._updates += 1
            self._completed.append(CompletedOrder(contract, order, orderState))

    def completedOrdersEnd(self):
        with self._lock:
            self._updates += 1
            self._completed_done = True

    def completed_orders_finished(self) -> bool:
        with self._lock:
            return self._completed_done

    def snapshot_completed(self) -> list[CompletedOrder]:
        with self._lock:
            return list(self._completed)

    def forget_completed(self):
        """Drop a finished batch, so asking again is answered by the new one."""
        with self._lock:
            self._completed.clear()
            self._completed_done = False

    def orderStatus(
        self, orderId, status, filled, remaining, avgFillPrice, permId,
        parentId, lastFillPrice, clientId, whyHeld, mktCapPrice=0.0,
    ):
        with self._lock:
            self._updates += 1
            trade = self._trades.setdefault(orderId, Trade())
            before = trade.orderStatus
            # Zero is unstated on a report, for the average and for the
            # permanent id alike: a status arriving after a fill can state no
            # average, and a later report carrying no permId would otherwise
            # erase the name this order is known by across sessions.
            trade.orderStatus = OrderStatus(
                orderId, status, filled, remaining,
                avgFillPrice or before.avgFillPrice, permId or before.permId,
                parentId, lastFillPrice, clientId, whyHeld, mktCapPrice,
            )
            if status != before.status:
                trade.log.append(status)

    def execDetails(self, reqId, contract, execution):
        """Record a fill once, however many times the venue names it.

        A fill arrives live and again in the answer to ``reqExecutions``, and
        both carry the venue's execution id. Appended each time, one fill
        counted twice against the order: a caller adding up ``Trade.fills`` saw
        twice the quantity it held, and asking for its executions a second time
        doubled them again. An execution with no id is kept as it comes, since
        there is nothing to recognise it by.

        The answer to a request still names every fill the request matched,
        seen before or not: that is what was asked. A fill arriving live names
        no request.
        """
        with self._lock:
            self._updates += 1
            exec_id = getattr(execution, "execId", "")
            fill = self._fill_by_id.get(exec_id) if exec_id else None
            if fill is None:
                fill = Fill(contract, execution, None, getattr(execution, "time", ""))
                if exec_id:
                    self._fill_by_id[exec_id] = fill
                self._fills.append(fill)
                order_id = getattr(execution, "orderId", None)
                if order_id is not None and order_id in self._trades:
                    self._trades[order_id].fills.append(fill)
            if reqId >= 0:
                self._executions.setdefault(reqId, []).append(fill)

    def execDetailsEnd(self, reqId):
        self._end(("executions", reqId))

    def take_executions(self, req_id) -> list[Fill]:
        """The fills a request named, handed over once."""
        with self._lock:
            return self._executions.pop(req_id, [])

    def commissionAndFeesReport(self, report):
        """Attach the cost to the fill it belongs to.

        It arrives separately from the fill and names it by execution id. A fill
        whose cost never arrived keeps ``None`` rather than a zero, because a
        zero commission and an unknown one are different facts.
        """
        exec_id = getattr(report, "execId", None)
        if exec_id is None:
            return
        with self._lock:
            self._updates += 1
            fill = self._fill_by_id.get(exec_id)
            if fill is not None:
                fill.commissionReport = report

    def register_order(self, order_id, contract, order) -> Trade:
        """Record an order as it is sent, so the caller holds the object the
        venue's answers will land on."""
        with self._lock:
            trade = self._trades.setdefault(order_id, Trade())
            trade.contract = contract
            trade.order = order
            trade.orderStatus.orderId = order_id
            if not trade.log:
                trade.log.append("PendingSubmit")
                trade.orderStatus.status = "PendingSubmit"
            return trade

    def trade_for(self, order_id) -> Trade | None:
        with self._lock:
            return self._trades.get(order_id)

    def forget_trade(self, order_id) -> None:
        """Drop a record for an order that was never placed.

        A preview travels the order path and is answered on the order
        callbacks, so it leaves a trade behind like any other. Kept, it reads
        as an order working at the venue.
        """
        with self._lock:
            self._trades.pop(order_id, None)

    def pnl(self, reqId, dailyPnL, unrealizedPnL, realizedPnL):
        with self._lock:
            self._updates += 1
            self._pnl[reqId] = (dailyPnL, unrealizedPnL, realizedPnL)

    def pnlSingle(self, reqId, pos, dailyPnL, unrealizedPnL, realizedPnL, value):
        with self._lock:
            self._updates += 1
            self._pnl_single[reqId] = (pos, dailyPnL, unrealizedPnL, realizedPnL, value)

    def scannerData(self, reqId, rank, contractDetails, distance, benchmark, projection, legsStr):
        """A scan row, whole. The venue states six things about it and only the
        rank and the contract were kept, so the distance from the benchmark,
        the benchmark itself, the projection and a combination's legs arrived
        and were dropped with nothing to say they had."""
        with self._lock:
            self._updates += 1
            self._scanner.setdefault(reqId, []).append(
                ScanData(rank, contractDetails, distance, benchmark, projection, legsStr)
            )

    def scannerDataEnd(self, reqId):
        """The venue has named every row it is going to.

        Recorded rather than ignored: the rows arrive one at a time, so a
        reader that stops at the first has a scan of one, and there is nothing
        else that says whether more are coming."""
        with self._lock:
            self._updates += 1
            self._scanner_done.add(reqId)

    def scanner_finished(self, req_id) -> bool:
        with self._lock:
            return req_id in self._scanner_done

    def updateNewsBulletin(self, msgId, msgType, newsMessage, originExch):
        with self._lock:
            self._updates += 1
            self._bulletins.append((msgId, msgType, newsMessage, originExch))

    def tickNews(self, tickerId, timeStamp, providerCode, articleId, headline, extraData):
        with self._lock:
            self._updates += 1
            # Six fields, as the venue states them. The last carries the
            # sentiment and relevance scores a caller filters on, and it was
            # dropped on the way in.
            self._news_ticks.append(
                (tickerId, timeStamp, providerCode, articleId, headline, extraData)
            )

    def realtimeBar(self, reqId, time, open_, high, low, close, volume, wap, count):
        with self._lock:
            self._updates += 1
            self._bars.setdefault(reqId, []).append(
                (time, open_, high, low, close, volume, wap, count)
            )

    def snapshot_pnl(self) -> list:
        with self._lock:
            return list(self._pnl.values())

    def snapshot_pnl_single(self) -> list:
        with self._lock:
            return list(self._pnl_single.values())

    def take_scanner(self, req_id):
        with self._lock:
            return list(self._scanner.get(req_id, []))

    def forget_scanner(self, req_id):
        """Drop a finished scan, so asking again is answered by the new one."""
        with self._lock:
            self._scanner.pop(req_id, None)
            self._scanner_done.discard(req_id)

    def snapshot_bulletins(self) -> list:
        with self._lock:
            return list(self._bulletins)

    def snapshot_news_ticks(self) -> list:
        with self._lock:
            return list(self._news_ticks)

    def snapshot_bars(self) -> list:
        with self._lock:
            return [b for bars in self._bars.values() for b in bars]

    # -- quotes ----------------------------------------------------------

    def _ticker(self, req_id: int) -> Ticker:
        t = self._tickers.get(req_id)
        if t is None:
            t = Ticker()
            self._tickers[req_id] = t
        return t

    def _apply(self, req_id, tick_type, value):
        if self._fields is None:
            self._fields = _tick_fields()
        pair = self._fields.get(tick_type)
        if pair is None:
            # A tick with no field of its own here has nowhere to be kept, but
            # it is still the venue speaking. Returning without counting it
            # made `waitOnUpdate` time out on a contract that was ticking
            # steadily, because every one of its ticks was one this class has
            # no field for. Subclass `tickPrice`/`tickSize` to reach the rest.
            with self._lock:
                self._updates += 1
            return
        name, prev_name = pair
        with self._lock:
            self._updates += 1
            t = self._ticker(req_id)
            if prev_name is not None:
                setattr(t, prev_name, getattr(t, name))
            setattr(t, name, value)
            self._pending.add(req_id)

    def tickPrice(self, reqId, tickType, price, attrib=None):
        # -1 exactly is the venue's "no such price". Every other negative
        # value is a real quote: a spread at minus thirty-five cents, a future
        # trading below zero, a tick index that sits on both sides of it.
        self._apply(reqId, tickType, None if price == -1 else price)

    def tickSize(self, reqId, tickType, size):
        self._apply(reqId, tickType, size)

    def tickGeneric(self, reqId, tickType, value):
        """A tick that is neither a price nor a size, the halt among them.

        ``Ticker`` has carried a ``halted`` field and the tick map has carried
        the tick type all along, and nothing routed to either: the venue said
        trading had stopped and the quote went on showing the last prices
        before it, with nothing to say they were no longer live.
        """
        self._apply(reqId, tickType, value)

    def tickString(self, reqId, tickType, value):
        from .ibx import TickTypeEnum as T

        if tickType in (T.LAST_TIMESTAMP, T.DELAYED_LAST_TIMESTAMP):
            with self._lock:
                self._updates += 1
                self._ticker(reqId).time = value
                self._pending.add(reqId)

    def bind_ticker(self, req_id: int, contract) -> Ticker:
        """Name the contract a request's quote belongs to."""
        with self._lock:
            t = self._ticker(req_id)
            t.contract = contract
            return t

    def snapshot_tickers(self) -> list[Ticker]:
        with self._lock:
            return list(self._tickers.values())

    def ticker_for(self, req_id: int) -> Ticker | None:
        with self._lock:
            return self._tickers.get(req_id)

    def take_pending(self) -> list[Ticker]:
        """The quotes that changed since this was last read, and clear the mark.

        Reading is what clears it, so two readers do not each see half the
        changes. A caller wanting every change should be the only one reading.
        """
        with self._lock:
            out = [self._tickers[r] for r in self._pending if r in self._tickers]
            self._pending.clear()
            return out

    # -- what a caller reads ---------------------------------------------

    def snapshot_positions(self) -> list[Position]:
        with self._lock:
            return list(self._positions.values())

    def snapshot_portfolio(self) -> list[PortfolioItem]:
        with self._lock:
            return list(self._portfolio.values())

    def snapshot_account_values(self) -> list[AccountValue]:
        with self._lock:
            return list(self._account_values.values())

    def snapshot_trades(self) -> list[Trade]:
        with self._lock:
            return list(self._trades.values())

    def snapshot_fills(self) -> list[Fill]:
        with self._lock:
            return list(self._fills)

    def snapshot_accounts(self) -> list[str]:
        with self._lock:
            return list(self._accounts)


@dataclass
class HistoricalNews:
    time: str
    providerCode: str
    articleId: str
    headline: str


@dataclass
class TradingSession:
    startDateTime: str
    endDateTime: str
    refDate: str


@dataclass
class HistoricalSchedule:
    startDateTime: str
    endDateTime: str
    timeZone: str
    sessions: list[TradingSession]


@dataclass
class Ticker:
    """A contract's current quote, accumulated from the ticks that make it up.

    A quote does not arrive as a quote. It arrives as a bid, then a size, then a
    trade, each on its own, and a caller wanting "the current market" has to
    hold them together. This does that.

    A field nobody has sent stays ``None`` rather than becoming zero. A bid of
    zero and no bid at all are different markets, and the difference decides
    whether an order should be sent.
    """

    contract: Any = None
    time: str = ""
    bid: float | None = None
    bidSize: float | None = None
    ask: float | None = None
    askSize: float | None = None
    last: float | None = None
    lastSize: float | None = None
    prevBid: float | None = None
    prevAsk: float | None = None
    prevLast: float | None = None
    volume: float | None = None
    open: float | None = None
    high: float | None = None
    low: float | None = None
    close: float | None = None
    halted: float | None = None

    def hasBidAsk(self) -> bool:
        return self.bid is not None and self.ask is not None

    def midpoint(self) -> float | None:
        """Half way between the two sides, or nothing if there are not two."""
        if not self.hasBidAsk():
            return None
        return (self.bid + self.ask) / 2

    def marketPrice(self) -> float | None:
        """The price to value a holding at.

        The last trade when it sits inside the current spread, the midpoint when
        it does not. A last trade outside the spread is stale — the market moved
        away from it — and valuing at it is how a position is marked to a price
        nobody would deal at.
        """
        mid = self.midpoint()
        if self.last is not None and (mid is None or self.bid <= self.last <= self.ask):
            return self.last
        if mid is not None:
            return mid
        return self.close


#: Which tick number sets which field, and whether the previous value is kept.
#: Taken from the enum this client publishes rather than written out again, so
#: the two cannot drift apart.
def _tick_fields():
    from .ibx import TickTypeEnum as T

    return {
        T.BID: ("bid", "prevBid"),
        T.ASK: ("ask", "prevAsk"),
        T.LAST: ("last", "prevLast"),
        T.BID_SIZE: ("bidSize", None),
        T.ASK_SIZE: ("askSize", None),
        T.LAST_SIZE: ("lastSize", None),
        T.VOLUME: ("volume", None),
        T.OPEN: ("open", None),
        T.HIGH: ("high", None),
        T.LOW: ("low", None),
        T.CLOSE: ("close", None),
        T.HALTED: ("halted", None),
        # A delayed reading is the same figure under a number of its own, and
        # the venue sends nothing else to an account without the realtime
        # entitlement. Filed nowhere, every field of the quote stayed empty on
        # such a feed — which reads exactly like a market that is not trading.
        T.DELAYED_BID: ("bid", "prevBid"),
        T.DELAYED_ASK: ("ask", "prevAsk"),
        T.DELAYED_LAST: ("last", "prevLast"),
        T.DELAYED_BID_SIZE: ("bidSize", None),
        T.DELAYED_ASK_SIZE: ("askSize", None),
        T.DELAYED_LAST_SIZE: ("lastSize", None),
        T.DELAYED_VOLUME: ("volume", None),
        T.DELAYED_OPEN: ("open", None),
        T.DELAYED_HIGH: ("high", None),
        T.DELAYED_LOW: ("low", None),
        T.DELAYED_CLOSE: ("close", None),
        T.DELAYED_HALTED: ("halted", None),
    }


@dataclass
class BracketOrder:
    """A parent and the two exits that close it, one cancelling the other."""

    parent: Any = None
    takeProfit: Any = None
    stopLoss: Any = None

    def __iter__(self):
        return iter((self.parent, self.takeProfit, self.stopLoss))
