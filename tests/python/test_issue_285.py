"""ibx#285: ids are signed and keep their value; a request with an id
outside the reference's 32-bit range is dropped with no error."""

from ibx import Contract, EClient, EWrapper, Order


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code))

    def order_status(self, order_id, status, filled, remaining, avg_fill_price,
                     perm_id, parent_id, last_fill_price, client_id, why_held, mkt_cap_price):
        self.events.append(("order_status", order_id, status))

    def historical_data(self, req_id, bar):
        self.events.append(("historical_data", req_id))

    def historical_data_end(self, req_id, start, end):
        self.events.append(("historical_data_end", req_id))

    def head_timestamp(self, req_id, head_timestamp):
        self.events.append(("head_timestamp", req_id, head_timestamp))

    def next_valid_id(self, order_id):
        self.events.append(("next_valid_id", order_id))


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    return c, w


def test_error_with_id_minus_one():
    c, w = connected()
    c.req_market_data_type(9)
    c._test_dispatch_once()
    assert ("error", -1, 321) in w.events


def test_negative_order_id_round_trips():
    c, w = connected()
    c._test_push_order_update(-7, 0, "Cancelled", 0, 0)
    c._test_dispatch_once()
    assert ("order_status", -7, "Cancelled") in w.events


def test_negative_request_id_round_trips():
    c, w = connected()
    c._test_push_historical_data(-3, [("20260101", 1.0, 2.0, 0.5, 1.5, 10)], True)
    c._test_push_head_timestamp(-4, "20260101-14:30:00")
    c._test_dispatch_once()
    assert ("historical_data", -3) in w.events
    assert ("historical_data_end", -3) in w.events
    assert ("head_timestamp", -4, "20260101-14:30:00") in w.events


def test_ids_outside_the_reference_range_drop_the_request():
    c, w = connected()
    # An unknown ticker id in range gives 300, as the reference.
    c.cancel_mkt_data(5)
    # Outside the 32-bit range: dropped, no error.
    c.cancel_mkt_data(2**31)
    c.cancel_mkt_data(-(2**31) - 1)
    c._test_dispatch_once()
    errors = [e for e in w.events if e[0] == "error"]
    assert errors == [("error", 5, 300)]


def test_order_ids_are_the_highest_used_plus_one():
    """ibx#466: 32-bit order ids, as the reference's: nextValidId is the
    highest order id the client used + 1 (1 for none), the ids the server's
    reports gave for its client id included; reqIds reserves nothing,
    next_order_id reserves the id it gives."""
    c, w = connected()
    c.req_ids()
    c._test_note_reported_order_id(0, 68)
    c._test_note_reported_order_id(5, 500)
    c.req_ids()
    assert [e for e in w.events if e[0] == "next_valid_id"] == [("next_valid_id", 1), ("next_valid_id", 69)]
    assert (c.next_order_id(), c.next_order_id()) == (69, 70)
    w.events.clear()
    c.req_ids()
    assert w.events == [("next_valid_id", 69)]


def test_order_ids_outside_the_reference_range_drop_the_request():
    c, w = connected()
    contract = Contract()
    contract.con_id = 756733
    contract.symbol = "SPY"
    contract.sec_type = "STK"
    contract.exchange = "SMART"
    contract.currency = "USD"
    order = Order()
    order.action = "BUY"
    order.total_quantity = 1
    order.order_type = "LMT"
    order.lmt_price = 1.0
    for order_id in (2**31, -(2**31) - 1):
        c.place_order(order_id, contract, order)
        c.cancel_order(order_id, "")
    c._test_dispatch_once()
    assert [e for e in w.events if e[0] == "error"] == []
