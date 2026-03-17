"""Tests for ibapi-compatible connection lifecycle (issue #55).

Unit tests verify API signatures and error handling without a live gateway.
Compatibility tests (marked @pytest.mark.live) require IB paper trading gateway.
"""

import os
import pytest
from ibx import EWrapper, EClient, TickAttrib, TickTypeEnum


# ── EWrapper connection callback signatures ──

class ConnectionWrapper(EWrapper):
    """Records all connection-related callbacks."""

    def __init__(self):
        super().__init__()
        self.events = []

    def connect_ack(self):
        self.events.append(("connect_ack",))

    def connection_closed(self):
        self.events.append(("connection_closed",))

    def next_valid_id(self, order_id):
        self.events.append(("next_valid_id", order_id))

    def managed_accounts(self, accounts_list):
        self.events.append(("managed_accounts", accounts_list))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))

    def current_time(self, time):
        self.events.append(("current_time", time))


def test_connect_ack_callback():
    w = ConnectionWrapper()
    w.connect_ack()
    assert w.events == [("connect_ack",)]


def test_connection_closed_callback():
    w = ConnectionWrapper()
    w.connection_closed()
    assert w.events == [("connection_closed",)]


def test_next_valid_id_callback():
    w = ConnectionWrapper()
    w.next_valid_id(42)
    assert w.events == [("next_valid_id", 42)]


def test_managed_accounts_callback():
    w = ConnectionWrapper()
    w.managed_accounts("DU12345,DU67890")
    assert w.events == [("managed_accounts", "DU12345,DU67890")]


def test_error_callback():
    w = ConnectionWrapper()
    w.error(-1, 2104, "Market data farm connection is OK", "")
    assert w.events == [("error", -1, 2104, "Market data farm connection is OK")]


def test_error_callback_default_json():
    """error() advanced_order_reject_json defaults to empty string."""
    w = ConnectionWrapper()
    w.error(-1, 2104, "test")
    assert len(w.events) == 1


def test_current_time_callback():
    w = ConnectionWrapper()
    w.current_time(1741785600)
    assert w.events == [("current_time", 1741785600)]


# ── EClient connection methods ──

def test_eclient_is_connected_default():
    client = EClient(EWrapper())
    assert client.is_connected() is False


def test_eclient_disconnect_idempotent():
    """disconnect() should not raise even when not connected."""
    client = EClient(EWrapper())
    client.disconnect()
    client.disconnect()  # second call also safe
    assert client.is_connected() is False


def test_eclient_get_account_id_empty():
    """get_account_id() returns empty string when not connected."""
    client = EClient(EWrapper())
    assert client.get_account_id() == ""


def test_eclient_req_ids_not_connected():
    """req_ids() dispatches next_valid_id even without connection."""
    w = ConnectionWrapper()
    client = EClient(w)
    client.req_ids()
    assert len(w.events) == 1
    assert w.events[0][0] == "next_valid_id"


def test_eclient_req_ids_with_num():
    """req_ids() accepts num_ids parameter."""
    w = ConnectionWrapper()
    client = EClient(w)
    client.req_ids(5)
    assert w.events[0][0] == "next_valid_id"


# ── EWrapper base class no-op defaults ──

def test_ewrapper_base_connect_ack_noop():
    """Base EWrapper methods are no-ops and don't raise."""
    w = EWrapper()
    w.connect_ack()
    w.connection_closed()
    w.next_valid_id(1)
    w.managed_accounts("DU12345")
    w.error(-1, 0, "test", "")
    w.current_time(0)


# ── Full connection callback sequence ──

def test_all_connection_callbacks_sequence():
    """Verify that a typical connection callback sequence works."""
    w = ConnectionWrapper()
    w.next_valid_id(1000)
    w.managed_accounts("DU12345")
    w.connect_ack()

    assert len(w.events) == 3
    assert w.events[0] == ("next_valid_id", 1000)
    assert w.events[1] == ("managed_accounts", "DU12345")
    assert w.events[2] == ("connect_ack",)


# ── Compatibility tests requiring live gateway ──

live = pytest.mark.skipif(
    not os.environ.get("IBX_LIVE_TEST"),
    reason="Set IBX_LIVE_TEST=1 to run live gateway tests"
)
