"""The venue's short-sale restriction reaches a caller.

The circuit breaker a venue puts on a contract that has fallen far enough in a
day, which stops a short from resting below the bid. It is stated on the same
record as the halt and has no field anywhere in the documented API, so it was
decoded and read by nobody: a program routing a short into such a contract had
the order bounced rather than knowing not to send it.

Not the same question as whether the contract can be borrowed, which ticks 46
and 89 answer beside it — a contract can be freely borrowable and still
restricted.
"""

import ibx


def test_a_contract_nobody_has_restricted_says_so():
    c = ibx.EClient(ibx.EWrapper())
    c._test_connect("T")
    c._test_map_instrument(1, 0)
    assert c.shortSaleRestricted(1) is False
    assert c.shortSaleRestrictedByInstrument(0) is False


def test_a_request_naming_no_subscription_is_not_a_restriction():
    c = ibx.EClient(ibx.EWrapper())
    c._test_connect("T")
    assert c.shortSaleRestricted(999) is False
