"""A callback the reference client defines exists here, even where it never fires.

Nine of them cannot fire on this connection and each says why: there is no
terminal to make a verification handshake with, no socket layer of the reference
client's own to report a Windows error from, and nothing on this wire has been
seen to state a rerouted contract. But a program written against that client
implements them, and a callback that is not there is a port that does not run —
so they are there, and silent, rather than missing.
"""

import inspect

import ibx

NEVER_FIRED = [
    "delta_neutral_validation",
    "reroute_mkt_data_req",
    "reroute_mkt_depth_req",
    "tick_efp",
    "verify_message_api",
    "verify_completed",
    "verify_and_auth_message_api",
    "verify_and_auth_completed",
    "win_error",
]


def test_each_one_is_there_to_implement():
    w = ibx.EWrapper()
    for name in NEVER_FIRED:
        assert hasattr(w, name), f"{name} is not on the wrapper"
        assert callable(getattr(w, name)), name


def test_a_program_that_implements_one_is_not_refused():
    """Overriding one is what a ported program does; it must simply be quiet."""

    class Ported(ibx.EWrapper):
        def __init__(self):
            super().__init__()
            self.heard = []

        def win_error(self, text, last_error):
            self.heard.append((text, last_error))

        def reroute_mkt_data_req(self, req_id, con_id, exchange):
            self.heard.append((req_id, con_id, exchange))

    w = Ported()
    c = ibx.EClient(w)
    c._test_connect("T")
    # Nothing fires them, which is the documented behaviour — not an error.
    assert w.heard == []


def test_each_says_why_it_stays_silent():
    """A callback that exists and never fires has to say so, or a caller waits."""
    for name in NEVER_FIRED:
        doc = inspect.getdoc(getattr(ibx.EWrapper, name)) or ""
        assert doc.strip(), f"{name} says nothing about itself"
