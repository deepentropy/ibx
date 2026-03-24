"""
Regression test for issue #113 (topics 1+4):
- error(1100) fires when engine emits Disconnected event (heartbeat timeout, etc.)
- connection_closed fires when run loop exits
- is_connected() returns False after disconnect event
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


def test_disconnect_event_fires_error_1100():
    """Inject a Disconnected event via test helper, verify error(1100) callback."""
    wrapper = RecordingWrapper()
    client = EClient(wrapper)
    client._test_connect("TEST123")

    assert client.is_connected()

    # Inject a disconnect event (simulates heartbeat timeout)
    client._test_push_disconnect_event()

    # Run one dispatch cycle — should drain the event and fire error(1100)
    client._test_dispatch_once()

    assert 1100 in wrapper.error_codes, f"Expected error 1100, got: {wrapper.error_codes}"
    assert not client.is_connected(), "is_connected() should be False after disconnect event"
    print(f"Error codes: {wrapper.error_codes}")
    print(f"Error messages: {wrapper.error_messages}")
    print("PASS: error(1100) fired and is_connected() returned False")


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
