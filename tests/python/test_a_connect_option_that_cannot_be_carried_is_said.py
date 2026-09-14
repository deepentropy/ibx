"""An option set for a gateway that is not there is said, not swallowed.

The reference client hands these to its gateway on the greeting, and each one
changes how the session behaves — how fast it may ask, what it is told. There
is no gateway between this client and the venue to read them. A caller who set
one and heard nothing has a session that is not the one they asked for, and no
way to find that out.
"""

import ibx


class Errors(ibx.EWrapper):
    def __init__(self):
        super().__init__()
        self.seen = []

    def error(self, req_id, error_time, code, msg, advanced_order_reject_json=""):
        self.seen.append((code, msg))


def _client():
    w = Errors()
    c = ibx.EClient(w)
    c._test_connect("T")
    return w, c


def test_an_option_that_was_stated_is_said():
    w, c = _client()
    c.setConnectOptions("+PACEAPI")
    assert any("+PACEAPI" in msg for _, msg in w.seen), w.seen


def test_stating_none_says_nothing():
    w, c = _client()
    c.setConnectOptions("")
    assert not w.seen, w.seen
