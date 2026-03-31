"""
Regression test for issue #116: place_order() during CCP disconnect.

The engine fix (disconnect guard + send_fix error → Rejected OrderUpdate) is
tested at the Rust unit test level. This Python test verifies:

1. Normal orders produce order_status callbacks (not silently dropped)
2. Orders on an unconnected client raise RuntimeError (not silent None)
3. Rapid-fire orders all get acknowledged (no silent drops in normal operation)

Requires IB_USERNAME and IB_PASSWORD for live tests.
Run with: pytest tests/python/test_issue_116.py -v
"""
import os, threading, time
import pytest
from ibx import EClient, EWrapper, Contract, Order


SPY_CON_ID = 756733


def make_spy():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


class Wrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.lock = threading.Lock()
        self.events = []
        self.connected = threading.Event()
        self.got_next_id = threading.Event()
        self.next_order_id = 0
        self.order_statuses = {}  # oid -> list of statuses

    def connect_ack(self):
        self.connected.set()

    def next_valid_id(self, order_id):
        self.next_order_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, accounts_list):
        pass

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        with self.lock:
            self.events.append(("error", req_id, error_code, error_string))

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        with self.lock:
            self.order_statuses.setdefault(order_id, []).append(status)
            self.events.append(("order_status", order_id, status))

    def open_order(self, order_id, contract, order, order_state):
        pass

    def _statuses_for(self, oid):
        with self.lock:
            return list(self.order_statuses.get(oid, []))


@pytest.fixture(scope="module")
def ib_connection():
    if not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")):
        pytest.skip("IB_USERNAME and IB_PASSWORD not set")
    wrapper = Wrapper()
    client = EClient(wrapper)
    client.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )
    run_thread = threading.Thread(target=client.run, daemon=True)
    run_thread.start()
    assert wrapper.got_next_id.wait(timeout=15), "Connection failed"
    yield wrapper, client
    client.disconnect()
    run_thread.join(timeout=5)


def _next_oid(wrapper):
    oid = wrapper.next_order_id
    wrapper.next_order_id += 1
    return oid


# ═══════════════════════════════════════════════════════════════════
#  Unconnected client — verify place_order raises, not silent None
# ═══════════════════════════════════════════════════════════════════

class TestUnconnectedClient:
    """place_order on an unconnected client must raise, not return None silently."""

    def test_place_order_raises_when_not_connected(self):
        wrapper = Wrapper()
        client = EClient(wrapper)
        # Not connected — place_order must raise RuntimeError
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 100.0
        with pytest.raises(RuntimeError):
            client.place_order(1, make_spy(), order)


# ═══════════════════════════════════════════════════════════════════
#  Live tests — verify orders produce callbacks (no silent drops)
# ═══════════════════════════════════════════════════════════════════

class TestOrderCallbackGuarantee:
    """Every place_order must produce at least one order_status callback."""

    def test_single_order_produces_status(self, ib_connection):
        """A single LMT order must get an order_status callback."""
        wrapper, client = ib_connection

        oid = _next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00  # far below market — won't fill
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        time.sleep(5)  # wait for callbacks

        statuses = wrapper._statuses_for(oid)
        assert len(statuses) > 0, f"Order {oid} produced no order_status callbacks — silent drop"

        client.cancel_order(oid, "")
        time.sleep(2)

    def test_rapid_fire_orders_all_acknowledged(self, ib_connection):
        """3 orders sent in rapid succession must all get order_status callbacks."""
        wrapper, client = ib_connection

        oids = [_next_oid(wrapper) for _ in range(3)]
        for oid in oids:
            order = Order()
            order.action = "BUY"
            order.total_quantity = 1
            order.order_type = "LMT"
            order.lmt_price = 1.00
            order.tif = "GTC"
            order.outside_rth = True
            client.place_order(oid, make_spy(), order)

        time.sleep(10)  # wait for all callbacks

        for oid in oids:
            statuses = wrapper._statuses_for(oid)
            assert len(statuses) > 0, \
                f"Order {oid} produced no order_status callbacks — silent drop in rapid-fire"

        # Cancel all
        for oid in oids:
            client.cancel_order(oid, "")
        time.sleep(3)
