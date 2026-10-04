"""ibx#432, ibx#429, ibx#431: historical ticks as the official tick objects,
the local answers of a ticks request, keepUpToDate updates and the
historicalDataEnd strings, as the reference gives them (capture of
02/10/2026)."""

from decimal import Decimal

from ibx import Contract, EClient, EWrapper

T15 = 1790881200  # 20261001 15:00:00 US/Eastern


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def historical_ticks(self, req_id, ticks, done):
        self.events.append(("mid", req_id, [(t.time, t.price, t.size) for t in ticks], done))

    def historical_ticks_last(self, req_id, ticks, done):
        self.events.append(("last", req_id, [
            (t.time, t.tick_attrib_last.past_limit, t.tick_attrib_last.unreported, t.price, t.size,
             t.exchange, t.special_conditions) for t in ticks], done))

    def historical_ticks_bid_ask(self, req_id, ticks, done):
        self.events.append(("bidask", req_id, [
            (t.time, t.tick_attrib_bid_ask.bid_past_low, t.tick_attrib_bid_ask.ask_past_high,
             t.price_bid, t.price_ask, t.size_bid, t.size_ask) for t in ticks], done))

    def historical_data_end(self, req_id, start, end):
        self.events.append(("end", req_id, start, end))

    def historical_data_update(self, req_id, bar):
        self.events.append(("update", req_id, bar.date, bar.close, bar.volume, bar.wap, bar.bar_count))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


def aapl():
    c = Contract()
    c.con_id = 265598
    c.symbol = "AAPL"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_serve_commands_after(0)
    return c, w


def test_tick_objects_carry_the_api_fields():
    c, w = connected()
    c._test_push_historical_ticks_last(1, [(T15, False, True, 329.82, 1.0, "FINRA", "   I")], False)
    c._test_push_historical_ticks_last(1, [(T15, False, False, 329.82, 41.0, "FINRA", "")], True)
    c._test_push_historical_ticks_bid_ask(2, [(T15 - 1, False, False, 329.8, 329.84, 280.0, 240.0)], True)
    c._test_push_historical_ticks_midpoint(3, [(T15 - 1, 1.12347)], True)
    c._test_dispatch_once()
    assert w.events == [
        ("last", 1, [(T15, False, True, 329.82, 1.0, "FINRA", "   I")], False),
        ("last", 1, [(T15, False, False, 329.82, 41.0, "FINRA", "")], True),
        ("bidask", 2, [(T15 - 1, False, False, 329.8, 329.84, 280.0, 240.0)], True),
        ("mid", 3, [(T15 - 1, 1.12347, 0.0)], True),
    ]


def test_ticks_request_local_answers():
    c, w = connected()
    t15 = "20261001 15:00:00 US/Eastern"
    c.req_historical_ticks(1, aapl(), t15, "", 0, "TRADES", 1, False, [])
    c.req_historical_ticks(2, aapl(), t15, "", 10, "FOO", 1, False, [])
    c.req_historical_ticks(3, aapl(), "not a date", "", 10, "TRADES", 1, False, [])
    c.req_historical_ticks(4, aapl(), t15, "", 10, "AGGTRADES", 1, False, [])
    c._test_dispatch_once()
    errors = [(e[1], e[2], e[3][:40]) for e in w.events if e[0] == "error"]
    assert errors == [
        (1, 321, "Error validating request.-'bP' : cause -"),
        (2, 321, "Error validating request.-'bP' : cause -"),
        (3, 10314, "Start Date/Time: The date, time, or time"),
        (4, 10299, "Expected what to show is TRADES, please "),
    ]
    texts = [e[3] for e in w.events if e[0] == "error"]
    assert texts[0].endswith("Number of ticks must be > 0")
    assert texts[1].endswith("Invalid source price")


def test_historical_data_end_strings_and_updates():
    c, w = connected()
    c._test_push_historical_data(5, [("20261002 04:00:00 US/Eastern", 331.05, 331.59, 330.55, 331.45, 49528)], True,
                                 start="20261001 04:56:41 US/Eastern", end="20261002 04:56:41 US/Eastern")
    c._test_push_historical_update(5, "20261002 04:00:00 US/Eastern", 331.05, 331.59, 330.55, 331.45, 49528, 331.285, 522)
    c._test_dispatch_once()
    assert ("end", 5, "20261001 04:56:41 US/Eastern", "20261002 04:56:41 US/Eastern") in w.events
    # The volume and the WAP are the official API's Decimals.
    assert w.events[-1] == ("update", 5, "20261002 04:00:00 US/Eastern", 331.45, Decimal("49528"), Decimal("331.285"), 522)


def test_keep_up_to_date_refusals():
    c, w = connected()
    c.req_historical_data(6, aapl(), "20261001 10:00:00 US/Eastern", "1 D", "1 hour", "TRADES", 0, 1, True, [])
    c.req_historical_data(7, aapl(), "", "1 D", "1 hour", "BID_ASK", 0, 1, True, [])
    c._test_dispatch_once()
    assert [e for e in w.events if e[0] == "error"] == [
        ("error", 6, 321, "Error validating request.-'bM' : cause - End date not supported with live updates"),
        ("error", 7, 321, "Error validating request.-'bM' : cause - Source price not supported with live updates"),
    ]
