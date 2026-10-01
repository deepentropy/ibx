"""IBX Test #160 second-half: cancel-reject reconciliation.

When the server rejects a cancel for an order it had listed in the
post-connect status burst ("No such order"), IBX must behave as the
reference (ibx#252):

  - no API error and no status at the reject itself (the reference sends
    none; the old ibx-only error 10147 "cancel/modify rejected" is gone),
  - the order status that answers the follow-up status request sets the
    state: a rejected-state report of a cancelled order means Cancelled,
    with no 202 notice (202 belongs to a normal cancel),
  - the order then leaves req_open_orders.

A local refusal is unchanged: 10147 for an order this client cannot
cancel, 10148 for a finished order; nothing else follows it.

Run: pytest tests/python/test_issue_160_cancel_reject.py -v --timeout=120
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


# Farm connection notices, not about any order.
FARM_NOTICES = (2104, 2106, 2158)

# Local refusals of a cancel, before anything is sent (unchanged).
LOCAL_REFUSALS = (10147, 10148)

TERMINAL = ("Cancelled", "ApiCancelled", "Filled", "Inactive")


class CollectorWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0
        self.open_orders_batch = []
        self.got_open_order_end = threading.Event()
        self.statuses = []
        self.errors = []

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

    def order_status(self, order_id, status, filled, remaining, avg_fill_price,
                     perm_id, parent_id, last_fill_price, client_id, why_held, mkt_cap_price):
        self.statuses.append((order_id, status))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code in FARM_NOTICES:
            return
        self.errors.append((req_id, error_code, error_string))


class TestCancelRejectReconcile:
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
        # Drain post-connect CCP burst.
        time.sleep(5.0)
        yield
        self.client.disconnect()
        self.thread.join(timeout=5)

    def _snapshot_open(self):
        self.wrapper.open_orders_batch = []
        self.wrapper.got_open_order_end.clear()
        self.client.req_open_orders()
        assert self.wrapper.got_open_order_end.wait(timeout=15), "open_order_end never fired"
        return list(self.wrapper.open_orders_batch)

    def test_cancel_reject_gives_no_error_and_purges_cache(self):
        """Walk the open-orders snapshot, cancelling each order until one
        takes the server-reject path (Cancelled with no error at all), then
        assert the order is absent from the next snapshot.

        Every candidate is checked strictly: a normal cancel gives exactly
        the 202 notice and ends Cancelled; a local refusal gives exactly
        one 10147/10148 and no status; the server-reject path gives no
        error, only PendingCancel/Cancelled, and ends Cancelled.
        """
        snap_before = self._snapshot_open()
        if not snap_before:
            pytest.skip("paper account has no open orders to test cancel against")

        rejected_id = None
        for candidate_id, _, _ in snap_before[:10]:
            self.wrapper.statuses = []
            self.wrapper.errors = []
            self.client.cancel_order(candidate_id, "")

            deadline = time.time() + 5
            while time.time() < deadline:
                if any(s[0] == candidate_id and s[1] in TERMINAL for s in self.wrapper.statuses):
                    break
                if any(e[0] == candidate_id and e[1] in LOCAL_REFUSALS for e in self.wrapper.errors):
                    break
                time.sleep(0.1)
            # Let late callbacks of this candidate arrive before judging it.
            time.sleep(0.5)

            errors = [e for e in self.wrapper.errors if e[0] == candidate_id]
            codes = [e[1] for e in errors]
            statuses = [s[1] for s in self.wrapper.statuses if s[0] == candidate_id]

            # The removed ibx-only error for a server reject must not come back.
            assert not any("cancel/modify rejected" in e[2] for e in errors), errors

            if any(c in LOCAL_REFUSALS for c in codes):
                # Local refusal: one error, nothing sent, the status unchanged.
                assert len(codes) == 1, errors
                assert not statuses, (candidate_id, errors, statuses)
                continue
            if 202 in codes:
                # Normal cancel: the notice, then Cancelled.
                assert codes == [202], errors
                assert statuses and statuses[-1] == "Cancelled", (candidate_id, statuses)
                continue
            if not statuses:
                # No answer within the window: try the next candidate.
                assert not errors, errors
                continue
            # Server-reject path: no error at all; the status answer sets
            # the state. A stale order ends Cancelled (never Inactive or
            # Rejected); an order that filled meanwhile ends Filled.
            assert not errors, (candidate_id, errors)
            if statuses[-1] == "Filled":
                continue
            assert set(statuses) <= {"PendingCancel", "Cancelled"}, (candidate_id, statuses)
            assert statuses[-1] == "Cancelled", (candidate_id, statuses)
            rejected_id = candidate_id
            break

        if rejected_id is None:
            pytest.skip(
                "no candidate took the server-reject path: the paper account "
                "has only orders that cancelled normally or were refused "
                "locally. Run scripts/.tmp/discriminator_issue_160.py to verify "
                "the engine path manually if needed."
            )

        # Brief drain so any post-cancel updates settle, then assert the
        # rejected entry was removed from order_cache.
        time.sleep(1.0)
        snap_after = self._snapshot_open()
        assert not any(o[0] == rejected_id for o in snap_after), (
            f"order {rejected_id} ended Cancelled after a server cancel reject "
            f"but is still present in req_open_orders: order_cache was not purged."
        )
