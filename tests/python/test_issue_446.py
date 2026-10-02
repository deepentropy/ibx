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


class OrderRecorder(EWrapper):
    """The market data callbacks in their order, with the price attribute."""

    def __init__(self):
        super().__init__()
        self.events = []

    def tick_price(self, req_id, tick_type, price, attrib):
        self.events.append(("price", tick_type, price, attrib.can_auto_execute))

    def tick_size(self, req_id, tick_type, size):
        self.events.append(("size", tick_type, size))

    def tick_string(self, req_id, tick_type, value):
        self.events.append(("string", tick_type, value))

    def tick_generic(self, req_id, tick_type, value):
        self.events.append(("generic", tick_type, value))

    def market_data_type(self, req_id, market_data_type):
        self.events.append(("mdt", market_data_type))

    def tick_req_params(self, req_id, min_tick, bbo_exchange, snapshot_permissions):
        self.events.append(("params", min_tick))

    def tick_snapshot_end(self, req_id):
        self.events.append(("end",))


def currency_pair():
    c = Contract()
    c.con_id = 12087792
    c.symbol = "EUR"
    c.sec_type = "CASH"
    c.exchange = "IDEALPRO"
    c.currency = "USD"
    return c


def push_captured_eur_usd(c):
    """The first EUR.USD message captured 02/10/2026: book, trade with
    status 0, then daily figures; no auto-execution flag."""
    c._test_push_quote(0, bid=1.12547, ask=1.12549, last=1.1255, bid_size=4_000_000, ask_size=12_000_000,
                       high=1.12585, low=1.1232, close=1.1243, timestamp=1790921778)
    c._test_push_marks(0, halted=0)
    c._test_push_tick_req_params(0, 0.00001, "", 0)


def test_currency_pair_snapshot_in_the_reference_order():
    w = OrderRecorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_set_instrument_count(1)
    c._test_serve_commands_after(0)
    c.req_mkt_data(1, currency_pair(), "", True, False)
    push_captured_eur_usd(c)
    c._test_dispatch_once()
    assert w.events == [
        ("mdt", 1), ("params", 0.00001),
        ("string", 45, "1790921778"), ("generic", 49, 0.0),
        ("price", 6, 1.12585, False), ("price", 7, 1.1232, False), ("price", 9, 1.1243, False),
        ("price", 1, 1.12547, False), ("size", 0, 4_000_000.0),
        ("price", 2, 1.12549, False), ("size", 3, 12_000_000.0),
        ("end",),
    ]


def test_currency_pair_stream_bid_ask_execute_automatically():
    w = OrderRecorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_set_instrument_count(1)
    c._test_serve_commands_after(0)
    c.req_mkt_data(1, currency_pair(), "", False, False)
    push_captured_eur_usd(c)
    c._test_dispatch_once()
    assert w.events[:2] == [("mdt", 1), ("params", 0.00001)]
    prices = [e for e in w.events if e[0] == "price"]
    assert prices == [
        ("price", 1, 1.12547, True), ("price", 2, 1.12549, True), ("price", 4, 1.1255, False),
        ("price", 6, 1.12585, False), ("price", 7, 1.1232, False), ("price", 9, 1.1243, False),
    ]
    assert not [e for e in w.events if e[0] == "generic"]
