"""IBX Test #160 second-half: cancel-reject reconciliation.

Regression: when the gateway rejects a cancel for an order it had previously
listed in the post-connect mass-status burst (CxlRejReason=1, "No such
order"), IBX must:

  - parse OrigClOrdID (tag 41) correctly despite the gateway's "C" prefix
    and ".0/.1/.2" modify-chain suffix,
  - surface the reject through wrapper.error (code 202),
  - purge the stale entry from order_cache so subsequent req_open_orders
    stops returning it.

Pre-fix: tag 41 was parsed without the prefix/suffix strip → orig_clord
was None → no reject pushed → silent. order_cache was untouched, so
req_open_orders kept returning the stale entry forever.

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
        if error_code in (2104, 2106, 2158, 202):
            # 202 IS the cancel-reject we may want; record it but also keep
            # the noise filters in sync with the other tests.
            if error_code == 202:
                self.errors.append((req_id, error_code, error_string))
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

    def test_cancel_reject_surfaces_and_purges_cache(self):
        """Walk the open-orders snapshot, cancelling each order until one
        produces a 202 cancel-reject error (the bug's signature path), then
        assert the rejected order is absent from the next snapshot.

        Pre-fix: handle_cancel_reject failed to parse tag 41 (C-prefix +
        version suffix not stripped), so no 202 was ever emitted and the
        cache was never purged on reject.
        """
        snap_before = self._snapshot_open()
        if not snap_before:
            pytest.skip("paper account has no open orders to test cancel against")

        # Cap the candidate walk so a regression (no 202 ever emitted) fails
        # within the test timeout instead of running through the whole snapshot.
        rejected_id = None
        for candidate_id, _, _ in snap_before[:10]:
            self.wrapper.statuses = []
            self.wrapper.errors = []
            self.client.cancel_order(candidate_id, "")

            # Wait briefly for resolution. The gateway responds within ~1s
            # for stale-order rejects; some IDs simply don't respond, so we
            # try the next candidate.
            deadline = time.time() + 3
            while time.time() < deadline:
                if any(e[0] == candidate_id and e[1] == 202 for e in self.wrapper.errors):
                    rejected_id = candidate_id
                    break
                if any(s[0] == candidate_id and s[1] in ("Cancelled", "ApiCancelled", "Inactive")
                       for s in self.wrapper.statuses):
                    # Genuine cancel ack — not the reject path we want to test.
                    break
                time.sleep(0.1)

            if rejected_id is not None:
                break

        if rejected_id is None:
            # Pre-fix this is the bug's signature: cancel-reject was silently
            # swallowed for every candidate, no 202 ever emitted. Distinguish
            # from "no stale entries" by asserting at least one of the
            # candidates is still in the cache (i.e. the gateway responded
            # but we ignored it).
            still_present = [c for c, _, _ in snap_before[:10]
                             if any(o[0] == c for o in self._snapshot_open())]
            assert not still_present, (
                f"cancelled {len(snap_before[:10])} candidates but received "
                f"no 202 cancel-rejects; {len(still_present)} are still in "
                f"req_open_orders. handle_cancel_reject is silently swallowing "
                f"the gateway's reject. still-present sample: {still_present[:3]}"
            )
            pytest.skip(
                "no 202 cancel-reject produced and no stale entries to test "
                "against — paper account has only live orders that all cancelled "
                "normally. Run scripts/.tmp/discriminator_issue_160.py to verify "
                "the engine path manually if needed."
            )

        # Brief drain so any post-cancel CCP updates settle, then assert the
        # rejected entry was removed from order_cache.
        time.sleep(1.0)
        snap_after = self._snapshot_open()
        assert not any(o[0] == rejected_id for o in snap_after), (
            f"order {rejected_id} got a 202 cancel-reject but is still "
            f"present in req_open_orders — order_cache was not purged."
        )
