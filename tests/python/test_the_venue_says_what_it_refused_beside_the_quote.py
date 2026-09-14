"""What the venue says about the contract itself reaches a caller.

Two figures ride the tick that carries a contract's price extremes and neither
has a tick of its own in the documented API: how many shares are on issue, which
is the multiplier that turns a price into a market capitalisation, and what the
contract opened at a year ago. Both were read past to step the cursor.
"""

import ibx


def _client():
    c = ibx.EClient(ibx.EWrapper())
    c._test_connect("T")
    return c


def test_a_contract_nobody_has_figures_for_says_nothing():
    c = _client()
    assert c.contractFiguresByInstrument(0) is None
    assert c.contractFigures(999) is None, "a request naming no subscription"
