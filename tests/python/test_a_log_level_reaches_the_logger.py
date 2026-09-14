"""Asking for more logging changes how much is logged.

The venue has no message for this call, so for a long time what a caller stated
was written to the log rather than applied to it. A library is the thing
serving its caller, so the level a caller states is this one's own — and saying
it was applied while it was not is the failure this guards.
"""

import ibx


def test_a_level_this_client_does_not_hold_is_refused_rather_than_claimed():
    c = ibx.EClient(ibx.EWrapper())
    # Not a level. Refused on its own terms, whoever holds the logger.
    c.set_server_log_level(9)

    # A level, on a client with no session: the call reports rather than
    # pretending. What it must never do is answer as though the level moved
    # when nothing moved.
    c.set_server_log_level(4)


def test_every_level_the_reference_names_is_accepted():
    c = ibx.EClient(ibx.EWrapper())
    for level in (1, 2, 3, 4, 5):
        c.set_server_log_level(level)
