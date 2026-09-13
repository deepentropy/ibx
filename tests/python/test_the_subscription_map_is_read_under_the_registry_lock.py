"""The map of open quote subscriptions is read under the same guard it is
written under.

The wheel is free-threaded, so a subscription opened or withdrawn on one
thread really does resize this map while another thread is scanning it for a
contract's quote. Scanned unguarded, a plain quote read raised
`RuntimeError: dictionary changed size during iteration` on the reading
thread — which is neither the thread that changed anything nor a failure the
caller can do anything about.
"""

import ibx


class FakeContract:
    def __init__(self, con_id):
        self.conId = con_id


def test_the_subscription_map_is_read_under_the_registry_lock():
    ib = ibx.IB()
    held = []

    class Watched(dict):
        def items(self):
            held.append(ib._registry.locked())
            return super().items()

    ib._subscribed = Watched({1: FakeContract(756733)})

    # A contract the per-contract registry does not name, which is what sends
    # the read to the map.
    assert ib.ticker(FakeContract(265598)) is None
    assert held == [True], "the map was scanned without the guard"
