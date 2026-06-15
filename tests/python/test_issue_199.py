"""IBX Test #199: TIF=GTD orders must be accepted, rest, and expire broker-side.

Regression: GTD orders were always rejected -> Inactive because (1) the
good_till_date string was dropped when building order attributes and (2) the
expiry was encoded in the wrong wire form. Fixed by parsing good_till_date into
either a date-only or a UTC instant and emitting the matching expiry field.

This is a LIVE paper test (project rule: order acceptance is server-validated).
It places resting BUY limits far below market on SPY so they never fill.

Run: pytest tests/python/test_issue_199.py -v --timeout=180
"""

import os
import threading
import time
from datetime import datetime, timedelta, timezone

import pytest
from ibx import EWrapper, EClient, Contract, Order

pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)

# Resting states that mean "the gateway accepted the order" (not the bug's Inactive).
ACCEPTED = {"PreSubmitted", "Submitted", "PendingSubmit"}


class Collector(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0
        self.lock = threading.Lock()
        self.status_by_oid = {}  # oid -> list[str] in arrival order

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def order_status(self, order_id, status, *args):
        with self.lock:
            self.status_by_oid.setdefault(order_id, []).append(status)

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] oid={req_id} code={error_code}: {error_string}")

    def statuses(self, oid):
        with self.lock:
            return list(self.status_by_oid.get(oid, []))


def spy():
    c = Contract()
    c.con_id = 756733
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def resting_gtd_order(good_till_date):
    o = Order()
    o.action = "BUY"
    o.total_quantity = 1
    o.order_type = "LMT"
    o.lmt_price = 1.00  # far below market -> never fills, just rests
    o.tif = "GTD"
    o.outside_rth = True  # allow resting outside regular hours
    o.good_till_date = good_till_date
    return o


class TestGtdLifecycle:
    @pytest.fixture(autouse=True)
    def setup_connection(self):
        self.wrapper = Collector()
        self.client = EClient(self.wrapper)
        self.client.connect(
            username=os.environ["IB_USERNAME"],
            password=os.environ["IB_PASSWORD"],
            host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
            paper=True,
        )
        self.thread = threading.Thread(target=self.client.run, daemon=True)
        self.thread.start()
        assert self.wrapper.connected.wait(timeout=20), "Connection failed"
        self._placed = []
        yield
        # Cleanup: cancel anything still resting.
        for oid in self._placed:
            last = self.wrapper.statuses(oid)
            if not last or last[-1] not in ("Cancelled", "Filled", "Rejected", "Inactive"):
                try:
                    self.client.cancel_order(oid, "")
                except Exception:
                    pass
        time.sleep(2)
        self.client.disconnect()
        self.thread.join(timeout=5)

    def _wait_accept(self, oid, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            s = self.wrapper.statuses(oid)
            if any(st in ACCEPTED for st in s):
                return s
            if any(st in ("Inactive", "Rejected") for st in s):
                pytest.fail(f"GTD order {oid} rejected: statuses={s}")
            time.sleep(0.5)
        pytest.fail(f"GTD order {oid} never reached an accepted state: {self.wrapper.statuses(oid)}")

    def test_time_precise_gtd_acks_and_cancels_on_schedule(self):
        """Time-precise GTD (tag 126, UTC) acks, rests, and the gateway
        broker-side cancels it at the requested instant without a client cancel."""
        expiry_dt = datetime.now(timezone.utc) + timedelta(seconds=75)
        gtd = expiry_dt.strftime("%Y%m%d %H:%M:%S UTC")

        oid = self.wrapper.next_id
        self._placed.append(oid)
        self.client.place_order(oid, spy(), resting_gtd_order(gtd))

        s = self._wait_accept(oid)
        print(f"  accepted: oid={oid} statuses={s} expiry={gtd}")

        # Wait past the expiry and confirm a broker-side Cancelled with no client cancel.
        wait_s = (expiry_dt - datetime.now(timezone.utc)).total_seconds() + 25
        time.sleep(max(wait_s, 0))

        final = self.wrapper.statuses(oid)
        assert "Cancelled" in final, (
            f"GTD order should be broker-cancelled at expiry; statuses={final}"
        )

    def test_date_only_gtd_acks(self):
        """Date-only GTD (tag 432) is accepted and rests. Cancelled in teardown."""
        gtd = (datetime.now(timezone.utc) + timedelta(days=1)).strftime("%Y%m%d")

        oid = self.wrapper.next_id + 1  # distinct from any prior order this session
        self._placed.append(oid)
        self.client.place_order(oid, spy(), resting_gtd_order(gtd))

        s = self._wait_accept(oid)
        print(f"  accepted date-only: oid={oid} statuses={s} expiry={gtd}")
        assert "Inactive" not in s and "Rejected" not in s
