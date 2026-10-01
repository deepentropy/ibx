"""Issue #270: KeyboardInterrupt and SystemExit raised in a callback stop
the dispatch and reach the caller; any other exception is logged and the
dispatch goes on.
"""
import pytest
from ibx import EClient, EWrapper

LOST = "Connectivity between client and server has been lost."
RESTORED = "Connectivity between client and server has been restored - data maintained."


class RaisingWrapper(EWrapper):
    def __init__(self, exc):
        super().__init__()
        self.exc = exc
        self.codes = []

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.codes.append(error_code)
        if error_code in (1100, 504):
            raise self.exc


def connected(exc):
    w = RaisingWrapper(exc)
    c = EClient(w)
    c._test_connect("TEST123")
    return w, c


def test_ordinary_exception_is_dropped_and_dispatch_goes_on():
    w, c = connected(ValueError("bad handler"))
    c._test_push_connection_notice(1100, LOST)
    c._test_push_connection_notice(1102, RESTORED)
    c._test_dispatch_once()
    assert w.codes == [1100, 1102]


@pytest.mark.parametrize("exc", [KeyboardInterrupt, SystemExit])
def test_base_exception_stops_dispatch(exc):
    w, c = connected(exc)
    c._test_push_connection_notice(1100, LOST)
    c._test_push_connection_notice(1102, RESTORED)
    with pytest.raises(exc):
        c._test_dispatch_once()
    assert w.codes == [1100]


def test_system_exit_from_callback_ends_run():
    w, c = connected(SystemExit(3))
    c._test_push_connection_notice(1100, LOST)
    with pytest.raises(SystemExit) as info:
        c.run()
    assert info.value.code == 3


@pytest.mark.parametrize("exc", [KeyboardInterrupt, SystemExit])
def test_base_exception_from_not_connected_error_is_raised(exc):
    w = RaisingWrapper(exc)
    c = EClient(w)
    with pytest.raises(exc):
        c.cancel_order(1)
    assert w.codes == [504]


def test_ordinary_exception_from_not_connected_error_is_dropped():
    w = RaisingWrapper(ValueError("bad handler"))
    c = EClient(w)
    c.cancel_order(1)
    assert w.codes == [504]
