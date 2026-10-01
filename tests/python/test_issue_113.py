"""
Regression test for issue #113 (topics 1+4) and ibx#268:
- a stopped engine (Disconnected event) ends the session with no error code
- connection_closed fires when run loop exits
- is_connected() returns False after disconnect event
- a lost link (1100 notice) keeps the client connected, and disconnect()
  from that error() handler returns (no deadlock)
"""
import os, threading, time
import pytest
from ibx import EClient, EWrapper


class RecordingWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()
        self.error_codes = []
        self.error_messages = []
        self.connection_closed_fired = threading.Event()
        self.got_next_id = threading.Event()
        self.next_order_id = 0

    def connect_ack(self):
        pass

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        pass

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        with self.lock:
            self.error_codes.append(error_code)
            self.error_messages.append(error_string)

    def connection_closed(self):
        self.connection_closed_fired.set()


def test_engine_stop_event_ends_session_without_error():
    """Inject a Disconnected event: the session ends, no error callback."""
    wrapper = RecordingWrapper()
    client = EClient(wrapper)
    client._test_connect("TEST123")

    assert client.is_connected()

    client._test_push_disconnect_event()
    client._test_dispatch_once()

    assert wrapper.error_codes == [], f"Expected no error, got: {wrapper.error_codes}"
    assert not client.is_connected(), "is_connected() should be False after disconnect event"


def test_engine_stop_event_ends_run_with_connection_closed():
    wrapper = RecordingWrapper()
    client = EClient(wrapper)
    client._test_connect("TEST123")

    run_done = threading.Event()

    def run_loop():
        client.run()
        run_done.set()

    t = threading.Thread(target=run_loop, daemon=True)
    t.start()
    client._test_push_disconnect_event()

    assert run_done.wait(timeout=5), "run() did not exit after the engine stopped"
    assert wrapper.connection_closed_fired.is_set()
    assert wrapper.error_codes == []
    t.join(timeout=5)


def test_link_lost_notice_keeps_client_connected():
    wrapper = RecordingWrapper()
    client = EClient(wrapper)
    client._test_connect("TEST123")

    client._test_push_connection_notice(1100, "Connectivity between client and server has been lost.")
    client._test_dispatch_once()

    assert wrapper.error_codes == [1100]
    assert client.is_connected()


class DisconnectOnLinkLost(RecordingWrapper):
    def __init__(self):
        super().__init__()
        self.client = None

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        super().error(req_id, error_code, error_string, advanced_order_reject_json)
        if error_code == 1100:
            self.client.disconnect()


def test_disconnect_from_link_lost_handler_returns():
    """ibx#268: disconnect() inside the 1100 handler must not deadlock."""
    wrapper = DisconnectOnLinkLost()
    client = EClient(wrapper)
    wrapper.client = client
    client._test_connect("TEST123")

    client._test_push_disconnect_event()
    client._test_push_connection_notice(1100, "Connectivity between client and server has been lost.")
    client._test_dispatch_once()

    assert wrapper.error_codes == [1100]
    assert not client.is_connected()


@pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB credentials not set",
)
def test_live_disconnect_fires_connection_closed():
    """Connect live, disconnect, verify connection_closed fires."""
    wrapper = RecordingWrapper()
    client = EClient(wrapper)

    client.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )

    run_done = threading.Event()
    def run_loop():
        client.run()
        run_done.set()

    run_thread = threading.Thread(target=run_loop, daemon=True)
    run_thread.start()

    assert wrapper.got_next_id.wait(timeout=15), "Connection failed"
    time.sleep(2)

    client.disconnect()

    assert run_done.wait(timeout=10), "run() did not exit after disconnect"
    assert wrapper.connection_closed_fired.wait(timeout=5), "connection_closed not fired"
    assert not client.is_connected()

    print("PASS: connection_closed fired on explicit disconnect")
    run_thread.join(timeout=5)
