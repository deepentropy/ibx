"""
Regression test for issue #115: STP/TRAIL orders with zero aux_price are
rejected with a clear error instead of silently submitting with stop_price=$0.

Tests both the client-side validation (no server needed) and live server
round-trips for correctly-formed stop orders.

Requires IB_USERNAME and IB_PASSWORD environment variables for live tests.
Run with: pytest tests/python/test_issue_115.py -v --timeout=120
"""
import os, threading, time
import pytest
from ibx import EClient, EWrapper, Contract, Order


live_only = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

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
        self.got_order_status = threading.Event()
        self.got_order_cancelled = threading.Event()
        self.next_order_id = 0

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
            self.events.append(("order_status", order_id, status))
        self.got_order_status.set()
        if status == "Cancelled":
            self.got_order_cancelled.set()

    def open_order(self, order_id, contract, order, order_state):
        with self.lock:
            self.events.append(("open_order", order_id, contract, order, order_state))

    def _get_events(self, name):
        with self.lock:
            return [e for e in self.events if e[0] == name]


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
#  Client-side validation — these reject BEFORE hitting the server
#  No live connection needed: validation runs inside place_order()
# ═══════════════════════════════════════════════════════════════════

@pytest.fixture(scope="module")
def local_client():
    """Unconnected client — sufficient for testing pre-send validation."""
    wrapper = Wrapper()
    client = EClient(wrapper)
    return wrapper, client


class TestAuxPriceValidation:
    """Verify that orders requiring aux_price raise RuntimeError when aux_price=0."""

    def test_stp_order_zero_aux_price_rejected(self, local_client):
        """STP with lmt_price but no aux_price must raise, not silently submit."""
        wrapper, client = local_client
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "STP"
        order.lmt_price = 145.0  # common mistake
        order.aux_price = 0.0  # unset (the API's default) gets the reference's 321 instead
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(1, make_spy(), order)

    def test_stp_lmt_order_zero_aux_price_rejected(self, local_client):
        """STP LMT with only lmt_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "STP LMT"
        order.lmt_price = 144.0
        order.aux_price = 0.0  # unset (the API's default) gets the reference's 321 instead
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(2, make_spy(), order)

    def test_trail_order_zero_both_rejected(self, local_client):
        """TRAIL with neither trailing_percent nor aux_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "TRAIL"
        # neither trailing_percent nor aux_price set
        with pytest.raises(RuntimeError, match="trailing_percent"):
            client.place_order(3, make_spy(), order)

    def test_trail_limit_order_zero_aux_price_rejected(self, local_client):
        """TRAIL LIMIT with only lmt_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "TRAIL LIMIT"
        order.lmt_price = 148.0
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(4, make_spy(), order)

    def test_mit_order_zero_aux_price_rejected(self, local_client):
        """MIT with no aux_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "MIT"
        order.aux_price = 0.0  # unset (the API's default) gets the reference's 321 instead
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(5, make_spy(), order)

    def test_lit_order_zero_aux_price_rejected(self, local_client):
        """LIT with only lmt_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LIT"
        order.lmt_price = 150.0
        order.aux_price = 0.0  # unset (the API's default) gets the reference's 321 instead
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(6, make_spy(), order)

    def test_stp_prt_order_zero_aux_price_rejected(self, local_client):
        """STP PRT with no aux_price must raise."""
        _, client = local_client
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "STP PRT"
        order.aux_price = 0.0  # unset (the API's default) gets the reference's 321 instead
        with pytest.raises(RuntimeError, match="aux_price"):
            client.place_order(7, make_spy(), order)


# ═══════════════════════════════════════════════════════════════════
#  Live server round-trips — correct aux_price usage
# ═══════════════════════════════════════════════════════════════════

class TestStopOrderLive:
    """Submit correctly-formed stop orders to the live server, verify ack, then cancel."""

    def test_stp_order_with_aux_price_accepted(self, ib_connection):
        """STP order with correct aux_price is accepted by the server."""
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = _next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "STP"
        order.aux_price = 999.00  # far above market — won't trigger
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=30), "No order_status for STP order"

        # Verify it was acknowledged (not rejected)
        statuses = [e[2] for e in wrapper._get_events("order_status") if e[1] == oid]
        assert any(s in ("Submitted", "PreSubmitted") for s in statuses), \
            f"STP order not acknowledged, got: {statuses}"

        # Cancel
        client.cancel_order(oid, "")
        wrapper.got_order_cancelled.wait(timeout=15)

    def test_stp_lmt_order_with_aux_price_accepted(self, ib_connection):
        """STP LMT order with correct aux_price + lmt_price is accepted."""
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = _next_oid(wrapper)
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "STP LMT"
        order.aux_price = 999.00  # stop trigger far above market
        order.lmt_price = 1000.00  # limit price
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=30), "No order_status for STP LMT order"

        statuses = [e[2] for e in wrapper._get_events("order_status") if e[1] == oid]
        assert any(s in ("Submitted", "PreSubmitted") for s in statuses), \
            f"STP LMT order not acknowledged, got: {statuses}"

        client.cancel_order(oid, "")
        wrapper.got_order_cancelled.wait(timeout=15)

    def test_trail_order_with_trailing_percent_accepted(self, ib_connection):
        """TRAIL order with trailing_percent is accepted by the server.
        Server may return Inactive if no position exists (SELL side) — that still
        proves the order was validly formed and reached the server."""
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = _next_oid(wrapper)
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "TRAIL"
        order.trailing_percent = 5.0
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=30), "No order_status for TRAIL order"

        # Inactive = server processed order (valid format) but rejected on business logic (no position)
        statuses = [e[2] for e in wrapper._get_events("order_status") if e[1] == oid]
        assert any(s in ("Submitted", "PreSubmitted", "Inactive") for s in statuses), \
            f"TRAIL order not processed by server, got: {statuses}"

        if any(s in ("Submitted", "PreSubmitted") for s in statuses):
            client.cancel_order(oid, "")
            wrapper.got_order_cancelled.wait(timeout=15)

    def test_trail_order_with_aux_price_amount_accepted(self, ib_connection):
        """TRAIL order with aux_price (dollar trail amount) is accepted.
        Server may return Inactive if no position exists — still proves valid format."""
        wrapper, client = ib_connection
        wrapper.got_order_status.clear()
        wrapper.got_order_cancelled.clear()

        oid = _next_oid(wrapper)
        order = Order()
        order.action = "SELL"
        order.total_quantity = 1
        order.order_type = "TRAIL"
        order.aux_price = 5.0  # $5 trailing amount
        order.tif = "GTC"
        order.outside_rth = True

        client.place_order(oid, make_spy(), order)
        assert wrapper.got_order_status.wait(timeout=30), "No order_status for TRAIL (amount) order"

        statuses = [e[2] for e in wrapper._get_events("order_status") if e[1] == oid]
        assert any(s in ("Submitted", "PreSubmitted", "Inactive") for s in statuses), \
            f"TRAIL (amount) order not processed by server, got: {statuses}"

        if any(s in ("Submitted", "PreSubmitted") for s in statuses):
            client.cancel_order(oid, "")
            wrapper.got_order_cancelled.wait(timeout=15)
