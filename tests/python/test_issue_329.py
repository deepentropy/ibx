"""IBX Test #329: a bracket child must report the parent it was placed with.

The engine reads no parent from an execution report, so for an order this
client placed its own record is the only source. The Rust client prefers that
record on all three callback paths; these cover the two the Python client also
has, which its own tests did not reach.

Reverting either Python lookup leaves every Rust test passing, which is how
the gap was found.

Run: pytest tests/python/test_issue_329.py -v
"""

from ibx import EWrapper, EClient

PARENT_ID = 4242
CHILD_ID = 9401
CON_ID = 756733


class Recorder(EWrapper):
    def __init__(self):
        self.parents = []

    def order_status(self, order_id, status, filled, remaining, avg_fill_price,
                     perm_id, parent_id, last_fill_price, client_id, why_held,
                     mkt_cap_price):
        self.parents.append((order_id, parent_id))


def tracked_child():
    """A connected client with one tracked child of PARENT_ID."""
    w = Recorder()
    c = EClient(w)
    c._test_connect()
    c._test_map_instrument(1, 0)
    c._test_track_order(CHILD_ID, 0, "SPY", "SELL", 1.0, 110.0, PARENT_ID)
    return w, c


def test_a_fill_reports_the_parent_the_child_was_given():
    """A fill emits its own order_status from a separate branch.

    This is the callback a caller is most likely to act on, and it reported
    zero for every bracket child.
    """
    w, c = tracked_child()
    c._test_push_fill(0, CHILD_ID, "SELL", 110.0, 1, 0, 0.0)
    c._test_dispatch_once()

    assert w.parents, "a fill must produce an order_status"
    assert w.parents[0] == (CHILD_ID, PARENT_ID)


def test_a_what_if_preview_reports_the_parent_the_child_was_given():
    """The margin preview emits a third order_status, which hard-coded zero."""
    w, c = tracked_child()
    c._test_push_what_if(CHILD_ID, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
    c._test_dispatch_once()

    assert w.parents, "a preview must produce an order_status"
    assert w.parents[0] == (CHILD_ID, PARENT_ID)


def test_an_order_this_client_did_not_place_keeps_the_engines_answer():
    """The positive control for the two above: no parent is invented.

    Without this, both assertions would pass just as well against a client
    that reported PARENT_ID for everything.
    """
    w = Recorder()
    c = EClient(w)
    c._test_connect()
    c._test_map_instrument(1, 0)
    c._test_push_fill(0, 9999, "SELL", 110.0, 1, 0, 0.0)
    c._test_dispatch_once()

    assert w.parents, "an untracked fill still produces an order_status"
    assert w.parents[0] == (9999, 0)
