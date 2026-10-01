"""ibx#446: a plain snapshot, as the reference sends it: each tick type
once, no end on a partial batch, the end once bid, ask, last, close and
open came; a snapshot with generic ticks is refused with 321."""

from ibx import Contract, EClient, EWrapper

GENERIC_REFUSAL = ("Error validating request.-'bQ' : cause - "
                   "Snapshot market data subscription is not applicable to generic ticks")


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def tick_price(self, req_id, tick_type, price, attrib):
        self.events.append(("price", req_id, tick_type, price))

    def tick_size(self, req_id, tick_type, size):
        self.events.append(("size", req_id, tick_type, size))

    def tick_snapshot_end(self, req_id):
        self.events.append(("end", req_id))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


def stock():
    c = Contract()
    c.con_id = 756733
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_set_instrument_count(1)
    c._test_serve_commands_after(0)
    return c, w


def test_each_tick_type_once_then_the_end():
    c, w = connected()
    c.req_mkt_data(1, stock(), "", True, False)
    c._test_push_quote(0, bid=100.0, ask=101.0, bid_size=2, ask_size=3)
    c._test_dispatch_once()
    ticks = [e for e in w.events if e[0] in ("price", "size", "end")]
    assert ticks == [("price", 1, 1, 100.0), ("size", 1, 0, 2.0), ("price", 1, 2, 101.0), ("size", 1, 3, 3.0)]
    w.events.clear()
    c._test_push_quote(0, bid=99.0, ask=101.0, last=100.0, bid_size=4, ask_size=3, last_size=1,
                       volume=50, open=99.0, high=102.0, low=97.0, close=98.0)
    c._test_dispatch_once()
    ticks = [e for e in w.events if e[0] in ("price", "size", "end")]
    assert ticks == [
        ("price", 1, 4, 100.0), ("size", 1, 5, 1.0), ("size", 1, 8, 50.0), ("price", 1, 6, 102.0),
        ("price", 1, 7, 97.0), ("price", 1, 9, 98.0), ("price", 1, 14, 99.0), ("end", 1),
    ]


def test_snapshot_with_generic_ticks_is_refused():
    c, w = connected()
    c.req_mkt_data(1, stock(), "233", True, False)
    c._test_dispatch_once()
    assert ("error", 1, 321, GENERIC_REFUSAL) in w.events
