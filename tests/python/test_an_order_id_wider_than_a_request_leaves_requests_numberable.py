"""An account whose orders outgrow a request id can still make requests.

`ib_async` numbers its orders and its requests out of one counter, because the
client it stands in for carries both as one signed 32-bit number, and its
wrapper raises that counter past every order id it sees so the next order is
not one the account is already working.

This protocol carries the two apart: an order id goes as wide as the venue
lets it, a request id is four billion wide, and the top quarter of that is
this client's own. An account the venue numbers orders above that is ordinary
— and the raise, taken at face value, left every request afterwards refused as
a number this protocol cannot carry.
"""

import ib_async

from ibx.ib_async import WIDEST_REQUEST_ID, IbxClient


def _client():
    return IbxClient(ib_async.IB().wrapper)


def test_a_raise_past_what_a_request_carries_is_let_go_of():
    c = _client()
    c.updateReqId(1000)
    assert c.getReqId() == 1000, "a raise a request can carry is taken"

    c.updateReqId(1787685160171219)
    assert c.getReqId() <= WIDEST_REQUEST_ID, \
        "an order id wider than a request does not become one"

    # And every request after it, which is where saturating at the top of the
    # range fails: the next one steps over the edge.
    for _ in range(4):
        assert c.getReqId() <= WIDEST_REQUEST_ID


def test_the_top_of_the_range_is_this_clients_own():
    c = _client()
    c.updateReqId(WIDEST_REQUEST_ID)
    assert c.getReqId() == WIDEST_REQUEST_ID, "the widest one a caller may use"
