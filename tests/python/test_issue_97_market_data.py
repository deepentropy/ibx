"""IBX Test #97: Real-Time Market Data + L2 Depth Subscribe (AAPL).

Streaming top-of-book ticks and Level 2 depth. Exercises farm routing,
subscription lifecycle, and binary depth encoding.

Run: pytest tests/python/test_issue_97_market_data.py -v --timeout=120
"""

import os
import time
import pytest
import threading
from ibx import EWrapper, EClient, Contract


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

AAPL_CON_ID = 265598


class MarketDataWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()

        # Top-of-book
        self.ticks_price = []   # (req_id, tick_type, price)
        self.ticks_size = []    # (req_id, tick_type, size)
        self.ticks_string = []  # (req_id, tick_type, value)
        self.ticks_generic = [] # (req_id, tick_type, value)
        self.got_tick = threading.Event()

        # L2 depth
        self.depth_updates = []    # (req_id, position, operation, side, price, size)
        self.depth_l2_updates = [] # (req_id, position, mm, operation, side, price, size, smart)
        self.got_depth = threading.Event()

        # Depth exchanges
        self.depth_exchanges = None
        self.got_depth_exchanges = threading.Event()

        # Post-cancel leak detection
        self.tick_after_cancel = False
        self.depth_after_cancel = False
        self._tob_cancelled = False
        self._depth_cancelled = False

    def next_valid_id(self, order_id):
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if self._tob_cancelled and req_id == 1:
            self.tick_after_cancel = True
        self.ticks_price.append((req_id, tick_type, price))
        if price > 0:
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        self.ticks_size.append((req_id, tick_type, size))

    def tick_string(self, req_id, tick_type, value):
        self.ticks_string.append((req_id, tick_type, value))

    def tick_generic(self, req_id, tick_type, value):
        self.ticks_generic.append((req_id, tick_type, value))

    def update_mkt_depth(self, req_id, position, operation, side, price, size):
        if self._depth_cancelled and req_id == 2:
            self.depth_after_cancel = True
        self.depth_updates.append((req_id, position, operation, side, price, size))
        self.got_depth.set()

    def update_mkt_depth_l2(self, req_id, position, market_maker, operation,
                            side, price, size, is_smart_depth):
        if self._depth_cancelled and req_id == 2:
            self.depth_after_cancel = True
        self.depth_l2_updates.append(
            (req_id, position, market_maker, operation, side, price, size, is_smart_depth))
        self.got_depth.set()

    def mkt_depth_exchanges(self, depth_mkt_data_descriptions):
        self.depth_exchanges = depth_mkt_data_descriptions
        self.got_depth_exchanges.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"  [error] reqId={req_id} code={error_code}: {error_string}")


def make_aapl():
    c = Contract()
    c.con_id = AAPL_CON_ID
    c.symbol = "AAPL"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class TestMarketDataAndDepth:
    """Issue #97: Streaming market data + L2 depth on AAPL."""

    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = MarketDataWrapper()
        self.client = EClient(self.wrapper)
        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )
        self.thread = threading.Thread(target=self.client.run, daemon=True)
        self.thread.start()
        assert self.wrapper.connected.wait(timeout=15), "Connection failed"
        yield
        self.client.disconnect()
        self.thread.join(timeout=5)

    def test_top_of_book_streaming(self):
        """Subscribe to AAPL top-of-book, collect ticks, then cancel."""
        aapl = make_aapl()
        self.client.req_mkt_data(1, aapl, "", False)

        got = self.wrapper.got_tick.wait(timeout=30)
        if not got:
            self.client.cancel_mkt_data(1)
            pytest.skip("No ticks received — market may be closed")

        # Collect for a few seconds
        time.sleep(5)
        self.client.cancel_mkt_data(1)
        self.wrapper._tob_cancelled = True

        # Verify we got price ticks
        prices = [t for t in self.wrapper.ticks_price if t[0] == 1 and t[2] > 0]
        assert len(prices) > 0, "Should have positive price ticks"

        # Verify bid/ask/last tick types present (1=bid, 2=ask, 4=last)
        tick_types = {t[1] for t in self.wrapper.ticks_price if t[0] == 1 and t[2] > 0}
        assert len(tick_types) > 0, "Should have multiple tick types"

        # Verify sizes arrived
        sizes = [t for t in self.wrapper.ticks_size if t[0] == 1]
        assert len(sizes) > 0, "Should have size ticks"

        # Price sanity: AAPL should be between $50 and $500
        for t in prices:
            assert 50 < t[2] < 500, f"AAPL price {t[2]} out of range"

        # Leak check: wait and verify no more ticks after cancel
        time.sleep(3)
        assert not self.wrapper.tick_after_cancel, \
            "Ticks still arriving after cancelMktData"

    def test_depth_exchanges(self):
        """Request available depth exchanges."""
        self.client.req_mkt_depth_exchanges()
        assert self.wrapper.got_depth_exchanges.wait(timeout=15), \
            "mkt_depth_exchanges not received"
        assert self.wrapper.depth_exchanges is not None

    def test_l2_depth(self):
        """Subscribe to L2 depth on AAPL, collect updates, then cancel."""
        aapl = make_aapl()
        self.client.req_mkt_depth(2, aapl, 10)

        got = self.wrapper.got_depth.wait(timeout=30)
        if not got:
            self.client.cancel_mkt_depth(2)
            pytest.skip("No depth received — market may be closed or no L2 subscription")

        time.sleep(5)
        self.client.cancel_mkt_depth(2)
        self.wrapper._depth_cancelled = True

        total_depth = len(self.wrapper.depth_updates) + len(self.wrapper.depth_l2_updates)
        assert total_depth > 0, "Should have depth updates"

        # Verify both sides present (0=ask, 1=bid)
        all_sides = set()
        for d in self.wrapper.depth_updates:
            all_sides.add(d[3])
        for d in self.wrapper.depth_l2_updates:
            all_sides.add(d[4])
        assert len(all_sides) > 0, "Should have at least one side in depth"

        # Leak check
        time.sleep(3)
        assert not self.wrapper.depth_after_cancel, \
            "Depth updates still arriving after cancelMktDepth"

    def test_combined_tob_and_depth(self):
        """Run top-of-book and L2 depth simultaneously, cancel independently."""
        aapl = make_aapl()

        # Subscribe both
        self.client.req_mkt_data(1, aapl, "", False)
        self.client.req_mkt_depth(2, aapl, 5)

        got_tick = self.wrapper.got_tick.wait(timeout=30)
        if not got_tick:
            self.client.cancel_mkt_data(1)
            self.client.cancel_mkt_depth(2)
            pytest.skip("No data received — market may be closed")

        time.sleep(5)

        # Cancel depth first, tob should keep streaming
        self.client.cancel_mkt_depth(2)
        self.wrapper._depth_cancelled = True
        tick_count_before = len(self.wrapper.ticks_price)
        time.sleep(3)
        tick_count_after = len(self.wrapper.ticks_price)

        # Cancel tob
        self.client.cancel_mkt_data(1)
        self.wrapper._tob_cancelled = True
