"""Python end-to-end integration test against live IB paper account (issue #85).

Requires IB_USERNAME and IB_PASSWORD environment variables.
Run with: pytest tests/python/test_live_e2e.py -v --timeout=120

This test connects the Python EClient → real Gateway → IB and verifies
the full end-to-end flow: connect → contract details → market data → order → disconnect.
"""

import os
import time
import pytest
import threading
from ibx import EWrapper, EClient, Contract, Order


# Skip entire module if credentials not set
pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class LiveWrapper(EWrapper):
    """Records callbacks from live IB connection."""

    def __init__(self):
        super().__init__()
        self.events = []
        self.connected = threading.Event()
        self.got_contract = threading.Event()
        self.got_tick = threading.Event()
        self.got_order_status = threading.Event()
        self.got_next_id = threading.Event()
        self.next_order_id = 0

    def connect_ack(self):
        self.events.append(("connect_ack",))
        self.connected.set()

    def next_valid_id(self, order_id):
        self.events.append(("next_valid_id", order_id))
        self.next_order_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        self.events.append(("managed_accounts", accounts_list))

    def contract_details(self, req_id, contract_details):
        self.events.append(("contract_details", req_id, contract_details))
        self.got_contract.set()

    def contract_details_end(self, req_id):
        self.events.append(("contract_details_end", req_id))

    def tick_price(self, req_id, tick_type, price, attrib):
        self.events.append(("tick_price", req_id, tick_type, price))
        if price > 0:
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        self.events.append(("tick_size", req_id, tick_type, size))

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self.events.append(("order_status", order_id, status, filled, remaining))
        self.got_order_status.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


class TestLiveE2E:
    """End-to-end tests against live IB paper account."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        """Connect to IB and tear down after each test."""
        self.wrapper = LiveWrapper()
        self.client = EClient(self.wrapper)

        username = os.environ["IB_USERNAME"]
        password = os.environ["IB_PASSWORD"]
        host = os.environ.get("IB_HOST", "cdc1.ibllc.com")

        self.client.connect(
            username=username,
            password=password,
            host=host,
            paper=True,
        )

        # Start the run loop in a daemon thread
        self.run_thread = threading.Thread(target=self.client.run, daemon=True)
        self.run_thread.start()

        yield

        self.client.disconnect()
        self.run_thread.join(timeout=5)

    def test_connect_and_next_valid_id(self):
        """Verify connection establishes and next_valid_id callback fires."""
        assert self.client.is_connected()
        assert self.wrapper.got_next_id.wait(timeout=10), "next_valid_id not received"
        assert self.wrapper.next_order_id > 0

    def test_contract_details_spy(self):
        """Request SPY contract details and verify response."""
        assert self.wrapper.got_next_id.wait(timeout=10)

        contract = Contract()
        contract.con_id = 756733
        contract.symbol = "SPY"
        contract.sec_type = "STK"
        contract.exchange = "SMART"
        contract.currency = "USD"

        self.client.req_contract_details(1001, contract)
        assert self.wrapper.got_contract.wait(timeout=15), "Contract details not received"

        cd_events = [e for e in self.wrapper.events if e[0] == "contract_details" and e[1] == 1001]
        assert len(cd_events) > 0, "Should have at least one contract_details callback"

    def test_market_data_ticks(self):
        """Subscribe to SPY market data and verify ticks arrive."""
        assert self.wrapper.got_next_id.wait(timeout=10)

        contract = Contract()
        contract.con_id = 756733
        contract.symbol = "SPY"
        contract.sec_type = "STK"
        contract.exchange = "SMART"
        contract.currency = "USD"

        self.client.req_mkt_data(2001, contract, "", False)

        got_tick = self.wrapper.got_tick.wait(timeout=30)
        self.client.cancel_mkt_data(2001)

        if not got_tick:
            pytest.skip("No ticks received — market may be closed")

        tick_events = [e for e in self.wrapper.events if e[0] == "tick_price" and e[1] == 2001]
        assert len(tick_events) > 0
        # Verify price is reasonable for SPY
        prices = [e[3] for e in tick_events if e[3] > 0]
        assert len(prices) > 0, "Should have positive prices"
        for p in prices:
            assert 50 < p < 1000, f"SPY price {p} out of expected range"

    def test_limit_order_submit_cancel(self):
        """Submit a limit order far from market and cancel it."""
        assert self.wrapper.got_next_id.wait(timeout=10)

        contract = Contract()
        contract.con_id = 756733
        contract.symbol = "SPY"
        contract.sec_type = "STK"
        contract.exchange = "SMART"
        contract.currency = "USD"

        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00  # Far below market
        order.tif = "GTC"
        order.outside_rth = True

        oid = self.wrapper.next_order_id
        self.client.place_order(oid, contract, order)

        # Wait for order acknowledgement
        assert self.wrapper.got_order_status.wait(timeout=30), "No order status received"

        status_events = [e for e in self.wrapper.events if e[0] == "order_status" and e[1] == oid]
        assert len(status_events) > 0, "Should have order_status for our order"

        # Cancel
        self.client.cancel_order(oid, "")

        # Wait for cancel confirmation
        time.sleep(3)
        cancel_events = [e for e in self.wrapper.events
                        if e[0] == "order_status" and e[1] == oid and e[2] == "Cancelled"]
        # Order may also be rejected if market permissions don't allow
        reject_events = [e for e in self.wrapper.events
                        if e[0] == "order_status" and e[1] == oid and e[2] == "Rejected"]
        assert len(cancel_events) > 0 or len(reject_events) > 0, \
            "Order should be cancelled or rejected"

    def test_disconnect_reconnect(self):
        """Verify clean disconnect."""
        assert self.client.is_connected()
        self.client.disconnect()
        time.sleep(1)
        assert not self.client.is_connected()
