"""ibx#444: several request ids on one contract share its subscription, as
the reference: the second gets at once the market data type and the request
parameters, both get the quote, and a cancel of one leaves the other.
ibx#450: an invalid generic tick list is refused with 321."""

from ibx import Contract, EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def tick_price(self, req_id, tick_type, price, attrib):
        self.events.append(("price", req_id, tick_type, price))

    def tick_req_params(self, req_id, min_tick, bbo_exchange, snapshot_permissions):
        self.events.append(("params", req_id, min_tick, bbo_exchange, snapshot_permissions))

    def market_data_type(self, req_id, market_data_type):
        self.events.append(("mdt", req_id, market_data_type))

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


def test_two_requests_share_one_contract():
    c, w = connected()
    c.req_mkt_data(1, stock(), "", False, False)
    c._test_push_tick_req_params(0, 0.01, "9c0001", 3)
    c._test_push_quote(0, bid=100.0, ask=101.0, bid_size=2, ask_size=3)
    c._test_dispatch_once()
    assert ("params", 1, 0.01, "9c0001", 3) in w.events
    w.events.clear()

    c.req_mkt_data(2, stock(), "", False, False)
    c._test_dispatch_once()
    assert w.events[:2] == [("mdt", 2, 1), ("params", 2, 0.01, "9c0001", 3)]
    assert ("price", 2, 1, 100.0) in w.events
    assert not any(e[0] == "error" for e in w.events)
    w.events.clear()

    c.cancel_mkt_data(1)
    c._test_push_quote(0, bid=99.0, ask=101.0, bid_size=2, ask_size=3)
    c._test_dispatch_once()
    assert ("price", 2, 1, 99.0) in w.events
    assert not any(e[1] == 1 for e in w.events if e[0] == "price")


def test_invalid_generic_tick_list_is_refused():
    c, w = connected()
    c.req_mkt_data(9, stock(), "999", False, False)
    c._test_dispatch_once()
    errors = [e for e in w.events if e[0] == "error"]
    assert len(errors) == 1
    assert errors[0][2] == 321
    assert errors[0][3].startswith("Error validating request.-'bQ' : cause - Incorrect generic tick list of 999.  "
                                   "Legal ones for (STK) are: 100(Option Volume),101(Option Open Interest),")
