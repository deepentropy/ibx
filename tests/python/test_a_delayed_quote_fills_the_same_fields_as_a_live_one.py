"""A delayed feed is the same quote under numbers of its own.

An account without the realtime entitlement — or any session that asked for
delayed data — is sent bid, ask, last and the rest under a second set of tick
numbers. Filed nowhere, every field of the quote stayed empty, and an empty
quote is indistinguishable from a market that is not trading: `hasBidAsk()`
was permanently false, the midpoint was nothing, and a call that waits for a
quote to arrive waited out its whole deadline.
"""

from ibx._state import LiveState
from ibx.ibx import TickTypeEnum as T


def test_a_delayed_quote_fills_the_same_fields_as_a_live_one():
    s = LiveState()
    s.tickPrice(1, T.DELAYED_BID, 100.0, None)
    s.tickPrice(1, T.DELAYED_ASK, 100.5, None)
    s.tickPrice(1, T.DELAYED_LAST, 100.25, None)
    s.tickSize(1, T.DELAYED_BID_SIZE, 5.0)
    s.tickSize(1, T.DELAYED_ASK_SIZE, 7.0)
    s.tickPrice(1, T.DELAYED_CLOSE, 99.0, None)
    s.tickString(1, T.DELAYED_LAST_TIMESTAMP, "1700000000")

    t = s.ticker_for(1)
    assert t.bid == 100.0
    assert t.ask == 100.5
    assert t.last == 100.25
    assert t.bidSize == 5.0
    assert t.askSize == 7.0
    assert t.close == 99.0
    assert t.time == "1700000000"
    assert t.hasBidAsk()
    assert t.midpoint() == 100.25
