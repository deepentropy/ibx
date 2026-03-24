"""Live test for L2 market depth (reqMktDepth + reqMktDepthExchanges).

Requires IB_USERNAME and IB_PASSWORD environment variables + NASDAQ TotalView subscription.
Run with: pytest tests/python/test_live_depth.py -v -s --timeout=120
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


class DepthWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.got_next_id = threading.Event()
        self.got_depth = threading.Event()
        self.got_depth_l2 = threading.Event()
        self.got_depth_exchanges = threading.Event()
        self.depth_updates = []
        self.depth_l2_updates = []
        self.depth_exchanges = []
        self.errors = []

    def next_valid_id(self, order_id):
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def update_mkt_depth(self, req_id, position, operation, side, price, size):
        self.depth_updates.append({
            "req_id": req_id, "position": position, "operation": operation,
            "side": side, "price": price, "size": size,
        })
        self.got_depth.set()

    def update_mkt_depth_l2(self, req_id, position, market_maker, operation,
                            side, price, size, is_smart_depth):
        self.depth_l2_updates.append({
            "req_id": req_id, "position": position, "market_maker": market_maker,
            "operation": operation, "side": side, "price": price, "size": size,
            "is_smart_depth": is_smart_depth,
        })
        self.got_depth_l2.set()

    def mkt_depth_exchanges(self, depth_mkt_data_descriptions):
        self.depth_exchanges = list(depth_mkt_data_descriptions)
        self.got_depth_exchanges.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.errors.append((req_id, error_code, error_string))
        print(f"  error({req_id}, {error_code}, {error_string})")


class TestLiveDepth:
    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = DepthWrapper()
        self.client = EClient(self.wrapper)

        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )

        self.run_thread = threading.Thread(target=self.client.run, daemon=True)
        self.run_thread.start()

        assert self.wrapper.got_next_id.wait(timeout=30), "Connection failed"
        yield
        self.client.disconnect()
        self.run_thread.join(timeout=5)

    def test_req_mkt_depth_aapl(self):
        """Subscribe to AAPL depth and verify updates arrive."""
        contract = Contract()
        contract.con_id = 265598
        contract.symbol = "AAPL"
        contract.sec_type = "STK"
        contract.exchange = "ISLAND"  # NASDAQ
        contract.currency = "USD"

        self.client.req_mkt_depth(3001, contract, 5, False, [])

        got = self.wrapper.got_depth.wait(timeout=30) or self.wrapper.got_depth_l2.wait(timeout=1)
        self.client.cancel_mkt_depth(3001)

        if not got:
            # Print any errors for debugging
            depth_errors = [e for e in self.wrapper.errors if e[0] == 3001]
            print(f"Depth errors: {depth_errors}")
            print(f"All depth updates: {self.wrapper.depth_updates}")
            print(f"All L2 updates: {self.wrapper.depth_l2_updates}")
            pytest.skip("No depth updates received — market may be closed or no L2 subscription")

        total = len(self.wrapper.depth_updates) + len(self.wrapper.depth_l2_updates)
        print(f"Received {len(self.wrapper.depth_updates)} L1 + {len(self.wrapper.depth_l2_updates)} L2 depth updates")
        assert total > 0

        # Check structure of first update
        if self.wrapper.depth_updates:
            u = self.wrapper.depth_updates[0]
            assert u["req_id"] == 3001
            assert u["side"] in (0, 1)
            assert u["operation"] in (0, 1, 2)
            assert u["price"] > 0

        if self.wrapper.depth_l2_updates:
            u = self.wrapper.depth_l2_updates[0]
            assert u["req_id"] == 3001
            assert isinstance(u["market_maker"], str)
