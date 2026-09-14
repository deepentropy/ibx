"""Every figure the venue's model states reaches a caller, not only eight.

`tickOptionComputation` carries eight, which is what the documented callback
has room for. The venue states eighteen on the same tick, and the ten that had
nowhere to go were decoded and dropped — the rate greek among them, which is the
one first-order greek the documented surface cannot answer at all.
"""

import ibx


def _client():
    c = ibx.EClient(ibx.EWrapper())
    c._test_connect("T")
    return c


def test_the_whole_model_is_readable_under_the_request_that_asked():
    c = _client()
    c._test_map_instrument(1, 0)
    c._test_push_option_model(0, 0.31, 9.25, 311.95, 0.42, 3.64)

    model = c.optionModel(1)
    assert model is not None, "the request names a subscription"
    assert model["impliedVol"] == 0.31
    assert model["optPrice"] == 9.25
    assert model["undPrice"] == 311.95
    # The two the documented callback has no field for.
    assert model["rho"] == 0.42, model
    assert model["fugit"] == 3.64, model
    # And every other one it has no field for is named, so a caller can tell an
    # unstated figure from a real nought.
    for named in ("exerciseBoundary", "forwardCoeff", "modelYield", "bridgeYield",
                  "timeValue", "calDays", "rate", "priceBasedVol"):
        assert named in model, f"{named} missing from {sorted(model)}"

    assert c.optionModelByInstrument(0) == model, "the same record, by instrument"


def test_a_contract_with_no_model_stated_reads_as_nothing():
    c = _client()
    assert c.optionModelByInstrument(0) is None
    assert c.optionModel(999) is None, "a request naming no subscription"


def test_a_quote_says_whether_the_contract_is_halted():
    c = _client()
    c._test_push_quote(0, 10.0, 11.0, 10.5, 1, 1, 1, 100, 10.0, 11.0, 9.0, 10.0)
    q = c.quoteByInstrument(0)
    assert q["halted"] == 0, q
