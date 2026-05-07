"""IBX Test #160: req_open_orders must not surface gateway sentinel records.

Regression: the mass-order-status request sent on (re)connect uses ClOrdID="*",
Symbol="*". The gateway's reply burst includes an echo of that wildcard with
ClOrdID=0 / Symbol="*" / Qty=0 that was being parsed as if it were a real
order, then surfaced through wrapper.open_order.

Run: pytest tests/python/test_issue_160.py -v --timeout=60
"""

import os
import threading
import time

import pytest
from ibx import EClient, EWrapper


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class CollectorWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0
        self.open_orders_batch = []
        self.got_open_order_end = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders_batch.append((order_id, contract.symbol, order_state.status))

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] oid={req_id} code={error_code}: {error_string}")


class TestOpenOrdersNoSentinel:
    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = CollectorWrapper()
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

    def _snapshot_open(self):
        self.wrapper.open_orders_batch = []
        self.wrapper.got_open_order_end.clear()
        self.client.req_open_orders()
        assert self.wrapper.got_open_order_end.wait(timeout=15), "open_order_end never fired"
        return list(self.wrapper.open_orders_batch)

    def test_no_wildcard_sentinel_in_open_orders(self):
        # Let the post-connect mass-status burst drain into order_cache.
        time.sleep(5.0)

        for snapshot in (self._snapshot_open(), self._snapshot_open()):
            for order_id, symbol, status in snapshot:
                assert order_id != 0, (
                    f"sentinel ClOrdID=0 leaked into req_open_orders: "
                    f"sym={symbol!r} status={status!r}"
                )
                assert symbol not in ("", "*"), (
                    f"sentinel symbol {symbol!r} leaked into req_open_orders: "
                    f"order_id={order_id} status={status!r}"
                )
