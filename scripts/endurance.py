"""Hold a session open under load, and fail if anything stops arriving.

What only a session finds: a connection that stops answering, a stream that
stops arriving, a request id that answers once. None of it is reachable by a
suite that opens a session, asks one question and closes it — every defect
this has found was invisible to the offline gates and to the live suites
alike.

Subscriptions are taken out and withdrawn every cycle, under request ids that
repeat, because that is what a program does and it is where the defects were.
Quotes and bars are required to have grown by the end of every cycle after the
first, and a book and a trade stream to have arrived at least once before the
run ends: a stream that stalls is the failure, and a run that only printed its
counters would have passed through all of them.

Places no orders. Reads only.

    IB_USERNAME=… IB_PASSWORD=… python scripts/endurance.py --minutes 20
"""

import argparse
import collections
import os
import sys
import threading
import time

from ibx import Contract, EClient, EWrapper

#: Liquid enough to be quoting whenever a market is open, and spread across two
#: asset classes so one venue going quiet does not read as the client stalling.
#: Where a book is asked for.
#:
#: Serving a book and serving it to this account are different questions. The
#: routing table answers the first; only the venue answers the second, and it
#: answers by refusing. Asked for the same share on six exchanges, it refused
#: five and took one — the one this account holds the depth subscription for.
#: The smart destination has no book for a share at all.
BOOK_EXCHANGE = "IEX"

#: Where a book is asked for when New York is shut.
#:
#: A book and a trade stream are held to arriving once before a run ends, and
#: both were asked for on the shares alone — so outside the session neither
#: could arrive, and the run ended saying so whatever the client had done. A
#: currency pair serves a book at any hour this account can reach it: measured,
#: 245 rows in half a minute on a Sunday evening. A crypto pair does not serve
#: one at all, and says nothing rather than refusing; a future's is refused by
#: name, which is the venue's answer for that account and not a fault.
FX_BOOK_EXCHANGE = "IDEALPRO"

SUBJECTS = [
    ("SPY", "STK", "SMART"),
    ("TSLA", "STK", "SMART"),
    ("AAPL", "STK", "SMART"),
    ("QQQ", "STK", "SMART"),
    ("EUR", "CASH", "IDEALPRO"),
    # Trades at every hour, which is the other half of what a shut session
    # cannot answer. Asked for its trades and nothing else: the venue serves
    # this pair no book and no bars, and says nothing rather than refusing, so
    # asking would spend a cycle's silence on a question with no answer.
    ("BTC", "CRYPTO", "PAXOS"),
]

#: What the venue says when a currency pair is asked for trades it does not
#: have. Expected once a cycle, and not a defect.
NO_TRADES_FOR_A_CURRENCY_PAIR = 162

#: Notices about a connection coming and going, which are not errors.
CONNECTION_NOTICES = {2100, 2103, 2104, 2105, 2106, 2107, 2119, 2158}


class Counters(EWrapper):
    """Everything that arrived, by kind."""

    def __init__(self):
        super().__init__()
        self.ready = threading.Event()
        self.lock = threading.Lock()
        self.seen = collections.Counter()
        self.errors = collections.Counter()
        self.examples = {}
        self.last_error = None

    def next_valid_id(self, order_id):
        self.ready.set()

    def managed_accounts(self, accounts):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, *rest):
        self._note("quotes")

    def tick_size(self, *rest):
        self._note("quotes")

    def update_mkt_depth_l2(self, *rest):
        self._note("book")

    def update_mkt_depth(self, *rest):
        self._note("book")

    def tick_by_tick_all_last(self, *rest):
        self._note("trades")

    def historical_data(self, *rest):
        self._note("bars")

    def error(self, req_id, error_time, code, message, advanced=""):
        if code in CONNECTION_NOTICES:
            return
        with self.lock:
            self.errors[code] += 1
            # One example per code, kept from the first of them. Only the most
            # recent was kept before, so a code that fired every cycle was
            # counted and never quoted — and a count on its own is not enough
            # to tell whose refusal it is.
            self.examples.setdefault(code, message[:120])
            self.last_error = (req_id, code, message[:100])

    def _note(self, kind):
        with self.lock:
            self.seen[kind] += 1

    def snapshot(self):
        with self.lock:
            return dict(self.seen), dict(self.errors), self.last_error


#: Held to growing in every cycle after the first: a counter that stops moving
#: is a stream that stopped.
REQUIRED_EVERY_CYCLE = ("quotes", "bars")

#: Held to arriving at all, once, before the run ends. A book and a trade
#: stream are the venue's to grant and are quiet on a contract nobody is
#: trading, so a cycle without one is not a failure — a whole run without one
#: is, and a run that only printed its counters would not have said so.
#:
#: Unless the venue says otherwise. An account without the entitlement is
#: refused depth outright, and holding a run to a book the venue has said it
#: will not send is a check that can never pass — which is as useless as one
#: that can never fail. The refusal is the evidence, and nothing else counts.
REQUIRED_AT_LEAST_ONCE = ("book", "trades")

#: What the venue says when it will not send a book at all, and the number the
#: reference client reports it under.
DEPTH_REFUSED = "refused depth"
MKT_DATA_NOT_SUBSCRIBED = 354


def what_stopped(before, now, cycle):
    """Which streams stopped arriving between one cycle and the next.

    A counter that has not moved is a stream that stopped. That is the whole
    finding: bars stopped after the seventh minute while every other stream
    stayed healthy, and nothing but a session watching its own counters would
    have said so.
    """
    return [
        f"{kind} stopped arriving after cycle {cycle - 1}"
        for kind in REQUIRED_EVERY_CYCLE
        if now.get(kind, 0) <= before.get(kind, 0)
    ]


def contract(symbol, sec_type, exchange):
    made = Contract()
    made.symbol, made.sec_type, made.exchange = symbol, sec_type, exchange
    made.currency = "USD"
    return made


def main():
    parsed = argparse.ArgumentParser(description=__doc__)
    parsed.add_argument("--minutes", type=int, default=20)
    parsed.add_argument("--host", default=os.environ.get("IB_HOST", "cdc1.ibllc.com"))
    asked = parsed.parse_args()

    username = os.environ.get("IB_USERNAME", "")
    password = os.environ.get("IB_PASSWORD", "")
    if not username or not password:
        print("IB_USERNAME and IB_PASSWORD are unset; this needs a session.")
        return 2

    watcher = Counters()
    client = EClient(watcher)
    client.connect(username=username, password=password, host=asked.host, paper=True)
    threading.Thread(target=client.run, daemon=True).start()
    # Nobody else on the account. The venue permits one logon at a time and
    # takes it from the older session without saying which it dropped, so a run
    # that starts beside another measures the contention rather than the
    # client — every stream stops and nothing says why.
    other = client.competing_session()
    if other:
        where, since, read_only = other
        print(f"another session already holds this account, from {where} since {since}"
              + (", and this one may not trade" if read_only else ""))
        print("stop it and start again; this run would be racing it.")
        client.disconnect()
        return 2

    if not watcher.ready.wait(timeout=60):
        print("no session was opened within a minute")
        return 1

    started = time.time()
    cycle = 0
    before = {}
    stalled = []
    while time.time() - started < asked.minutes * 60:
        cycle += 1
        # Ids repeat every seventh cycle, which is what a program does and is
        # where a request id that answers only once shows itself.
        base = 1000 + (cycle % 7) * 100
        for n, (symbol, sec_type, exchange) in enumerate(SUBJECTS):
            what = contract(symbol, sec_type, exchange)
            client.req_mkt_data(base + n, what, "", False, False)
            if sec_type == "CRYPTO":
                # Its trades, and nothing the venue does not serve for it.
                client.req_tick_by_tick_data(base + 70 + n, what, "AllLast", 0, False)
                continue
            if sec_type == "CASH":
                # The book this pair serves at any hour, which is what makes
                # that check mean something outside the New York session.
                client.req_mkt_depth(
                    base + 50 + n, contract(symbol, sec_type, FX_BOOK_EXCHANGE), 10, False,
                )
            if sec_type == "STK":
                # A book is asked for on the exchange the contract trades on.
                # The venue serves no book on its smart destination for a
                # share — it lists the top of one there and nothing deeper —
                # so a book asked for on SMART reached nothing and the silence
                # read as a quiet market. It is refused outright now, which is
                # how this was noticed.
                client.req_mkt_depth(
                    base + 50 + n, contract(symbol, sec_type, BOOK_EXCHANGE), 10, False,
                )
                client.req_tick_by_tick_data(base + 70 + n, what, "AllLast", 0, False)
            client.req_historical_data(
                base + 90 + n, what, "", "1 D", "1 hour", "TRADES", 1, 1, False, [],
            )
        time.sleep(60)
        for n, (symbol, sec_type, _) in enumerate(SUBJECTS):
            client.cancel_mkt_data(base + n)
            if sec_type == "CRYPTO":
                client.cancel_tick_by_tick_data(base + 70 + n)
                continue
            if sec_type == "CASH":
                client.cancel_mkt_depth(base + 50 + n, True)
            if sec_type == "STK":
                client.cancel_mkt_depth(base + 50 + n, True)
                client.cancel_tick_by_tick_data(base + 70 + n)
        time.sleep(5)

        seen, errors, last = watcher.snapshot()
        minutes = (time.time() - started) / 60
        print(f"[{minutes:5.1f}m] cycle {cycle:3d} {seen} errors={errors} last={last}",
              flush=True)

        if cycle > 1:
            stalled.extend(what_stopped(before, seen, cycle))
        before = seen

    seen, errors, _ = watcher.snapshot()
    unexpected = {
        code: n for code, n in errors.items()
        if code not in (NO_TRADES_FOR_A_CURRENCY_PAIR, MKT_DATA_NOT_SUBSCRIBED)
        and DEPTH_REFUSED not in watcher.examples.get(code, "")
    }
    refused_depth = any(DEPTH_REFUSED in text for text in watcher.examples.values())
    never_arrived = [
        kind for kind in REQUIRED_AT_LEAST_ONCE
        if not seen.get(kind) and not (kind == "book" and refused_depth)
    ]
    if refused_depth:
        print("the venue refuses a book on this account, so none was required")
    client.disconnect()

    for kind in never_arrived:
        print(f"NEVER ARRIVED: {kind} did not arrive once in {asked.minutes} minutes")

    for what in stalled:
        print(f"STALLED: {what}")
    if unexpected:
        print(f"ERRORS: {unexpected}")
        for code in sorted(unexpected):
            print(f"  {code}: {watcher.examples.get(code, '(no message)')}")
    if stalled or unexpected or never_arrived:
        return 1
    print(f"held open for {asked.minutes} minutes with nothing stopping")
    return 0


if __name__ == "__main__":
    sys.exit(main())
