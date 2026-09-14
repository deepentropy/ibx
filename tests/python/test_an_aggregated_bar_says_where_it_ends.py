"""A bar the venue aggregated states its own bounds, and they reach a caller.

A week runs Monday to Friday and a month the first to the last, and neither is
derivable from the bar's start. This client already reasons from that: it
refuses to keep a week or a month up to date precisely because a bar folded
locally opens on a Thursday and runs thirty days from 1970 where the venue's
does not. And then it read the start under both spellings and the end under
neither, so the bound it reasoned from reached nobody.

The last bar of any series is normally partial, so this is what tells a
finished week from a running one.
"""

import ibx


def test_a_bar_carries_the_end_the_venue_stated():
    bar = ibx.BarData("20260309", 1.0, 2.0, 0.5, 1.5, 10, 1.2, 3, "US/Eastern", "20260314")
    assert bar.end == "20260314"
    # And under the reference client's own spelling of a field, as every other
    # field on this object answers.
    assert bar.date == "20260309"


def test_a_bar_that_states_no_end_carries_none():
    bar = ibx.BarData("20260309-14:30:00", 1.0, 2.0, 0.5, 1.5, 10, 1.2, 3, "US/Eastern")
    assert bar.end == ""
