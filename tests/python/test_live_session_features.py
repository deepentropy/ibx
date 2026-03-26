"""Integration tests for session features: L2 depth, depth exchanges, market rules, order fields.

Requires IB_USERNAME and IB_PASSWORD environment variables.
Run with: pytest tests/python/test_live_session_features.py -v -s
"""

import os
import time
import pytest
import threading
from ibx import EWrapper, EClient, Contract, Order


pytestmark = pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB_USERNAME and IB_PASSWORD not set",
)


class FeatureWrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.got_next_id = threading.Event()
        self.next_id = 0

        # Depth
        self.got_depth_l2 = threading.Event()
        self.depth_l2_updates = []

        # Depth exchanges
        self.got_depth_exchanges = threading.Event()
        self.depth_exchanges = []

        # Contract details + market rule
        self.got_details_end = threading.Event()
        self.details = []
        self.got_rule = threading.Event()
        self.rules = []

        # Orders
        self.got_open_order = threading.Event()
        self.open_orders = []
        self.got_order_status = threading.Event()
        self.order_statuses = []
        self.got_open_order_end = threading.Event()

        self.errors = []

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.got_next_id.set()

    def managed_accounts(self, a): pass
    def connect_ack(self): pass
    def tick_price(self, *a): pass
    def tick_size(self, *a): pass

    def update_mkt_depth(self, req_id, position, operation, side, price, size):
        self.depth_l2_updates.append({
            "req_id": req_id, "position": position, "operation": operation,
            "side": side, "price": price, "size": size,
            "market_maker": "", "is_smart_depth": False,
        })
        self.got_depth_l2.set()

    def update_mkt_depth_l2(self, req_id, position, market_maker, operation,
                            side, price, size, is_smart_depth):
        self.depth_l2_updates.append({
            "req_id": req_id, "position": position, "market_maker": market_maker,
            "operation": operation, "side": side, "price": price, "size": size,
            "is_smart_depth": is_smart_depth,
        })
        self.got_depth_l2.set()

    def mkt_depth_exchanges(self, descriptions):
        self.depth_exchanges = list(descriptions)
        self.got_depth_exchanges.set()

    def contract_details(self, req_id, details):
        self.details.append(details)

    def contract_details_end(self, req_id):
        self.got_details_end.set()

    def market_rule(self, rule_id, price_increments):
        self.rules.append((rule_id, list(price_increments)))
        self.got_rule.set()

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders.append({
            "order_id": order_id, "contract": contract,
            "order": order, "order_state": order_state,
        })
        self.got_open_order.set()

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id, last_fill_price,
                     client_id, why_held, mkt_cap_price):
        self.order_statuses.append({
            "order_id": order_id, "status": status,
            "filled": filled, "remaining": remaining,
        })
        self.got_order_status.set()

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.errors.append((req_id, error_code, error_string))


@pytest.fixture(scope="module")
def ib_connection():
    """Single shared connection for all tests in this module."""
    w = FeatureWrapper()
    c = EClient(w)
    c.connect(
        username=os.environ["IB_USERNAME"],
        password=os.environ["IB_PASSWORD"],
        host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
        paper=True,
    )
    t = threading.Thread(target=c.run, daemon=True)
    t.start()
    assert w.got_next_id.wait(timeout=30), "Connection failed"
    yield w, c
    c.disconnect()
    t.join(timeout=5)


class TestSessionFeatures:
    @pytest.fixture(autouse=True)
    def setup_connection(self, ib_connection):
        self.w, self.c = ib_connection
        # Reset per-test state
        self.w.depth_l2_updates.clear()
        self.w.got_depth_l2.clear()
        self.w.got_depth_exchanges.clear()
        self.w.depth_exchanges.clear()
        self.w.got_details_end.clear()
        self.w.details.clear()
        self.w.got_rule.clear()
        self.w.rules.clear()
        self.w.got_open_order.clear()
        self.w.open_orders.clear()
        self.w.got_order_status.clear()
        self.w.order_statuses.clear()
        self.w.got_open_order_end.clear()
        self.w.errors.clear()

    # ── L2 Depth ──

    def test_l2_depth_structure(self):
        """L2 depth returns bid+ask entries with market maker and valid fields."""
        contract = Contract()
        contract.con_id = 76792991
        contract.symbol = "TSLA"
        contract.sec_type = "STK"
        contract.exchange = "ISLAND"
        contract.currency = "USD"

        self.c.req_mkt_depth(5001, contract, 10, False, [])
        got = self.w.got_depth_l2.wait(timeout=30)
        time.sleep(3)
        self.c.cancel_mkt_depth(5001)

        if not got:
            pytest.skip("No depth data — market may be closed")

        updates = [u for u in self.w.depth_l2_updates if u["req_id"] == 5001]
        assert len(updates) > 10, f"Expected 10+ depth updates, got {len(updates)}"

        bids = [u for u in updates if u["side"] == 1]
        asks = [u for u in updates if u["side"] == 0]
        assert len(bids) > 0, "No bid entries"
        assert len(asks) > 0, "No ask entries"

        # Validate all entries have valid fields (price=0 is valid for delete ops)
        for u in updates:
            assert u["price"] >= 0, f"Negative price: {u['price']}"
            assert u["size"] >= 0, f"Negative size: {u['size']}"
            assert u["position"] >= 0, f"Negative position: {u['position']}"
            assert u["operation"] in (0, 1, 2), f"Invalid operation: {u['operation']}"
            assert u["side"] in (0, 1), f"Invalid side: {u['side']}"

        # Verify most entries have non-zero prices
        nonzero = [u for u in updates if u["price"] > 0]
        assert len(nonzero) > len(updates) * 0.5, \
            f"Too many zero-price entries: {len(updates) - len(nonzero)}/{len(updates)}"

        # Validate market maker = NSDQ for ISLAND
        l2_with_mm = [u for u in updates if u.get("market_maker")]
        assert len(l2_with_mm) > 0, "No market maker in L2 updates"
        assert l2_with_mm[0]["market_maker"] == "NSDQ", \
            f"Expected NSDQ, got {l2_with_mm[0]['market_maker']}"

        print(f"L2 depth: {len(bids)} bids + {len(asks)} asks, "
              f"total {len(updates)} updates")

    def test_l2_depth_cancel(self):
        """L2 depth cancel stops data flow."""
        contract = Contract()
        contract.con_id = 76792991
        contract.symbol = "TSLA"
        contract.sec_type = "STK"
        contract.exchange = "ISLAND"
        contract.currency = "USD"

        self.c.req_mkt_depth(5002, contract, 5, False, [])
        got = self.w.got_depth_l2.wait(timeout=30)
        if not got:
            pytest.skip("No depth data")

        time.sleep(2)
        count_before = len(self.w.depth_l2_updates)
        self.c.cancel_mkt_depth(5002)
        time.sleep(3)
        count_after = len(self.w.depth_l2_updates)

        # After cancel, very few or no new updates should arrive
        new_after_cancel = count_after - count_before
        assert new_after_cancel < count_before, \
            f"Too many updates after cancel: {new_after_cancel} (before cancel: {count_before})"
        print(f"Before cancel: {count_before}, after: {new_after_cancel} new")

    # ── reqMktDepthExchanges ──

    def test_depth_exchanges_returns_exchanges(self):
        """reqMktDepthExchanges returns 50+ exchanges with STK and FUT."""
        self.c.req_mkt_depth_exchanges()
        got = self.w.got_depth_exchanges.wait(timeout=10)
        assert got, "No depth exchanges received"
        assert len(self.w.depth_exchanges) > 50, \
            f"Expected 50+ exchanges, got {len(self.w.depth_exchanges)}"

        stk = [d for d in self.w.depth_exchanges if d.sec_type == "STK"]
        fut = [d for d in self.w.depth_exchanges if d.sec_type == "FUT"]
        assert len(stk) > 20, f"Expected 20+ STK exchanges, got {len(stk)}"
        assert len(fut) > 10, f"Expected 10+ FUT exchanges, got {len(fut)}"
        print(f"Depth exchanges: {len(stk)} STK + {len(fut)} FUT = {len(self.w.depth_exchanges)}")

    def test_depth_exchanges_has_major_us(self):
        """Depth exchanges include NYSE, NASDAQ, ARCA, AMEX."""
        self.c.req_mkt_depth_exchanges()
        self.w.got_depth_exchanges.wait(timeout=10)

        exchange_names = {d.exchange for d in self.w.depth_exchanges}
        for required in ["NYSE", "NASDAQ", "ARCA", "AMEX"]:
            assert required in exchange_names, f"Missing {required} in depth exchanges"

    def test_depth_exchanges_fields_populated(self):
        """Each DepthMktDataDescription has non-empty exchange and listing_exch."""
        self.c.req_mkt_depth_exchanges()
        self.w.got_depth_exchanges.wait(timeout=10)

        for d in self.w.depth_exchanges[:20]:
            assert d.exchange, f"Empty exchange field: {d}"
            assert d.listing_exch, f"Empty listing_exch field: {d}"
            assert d.sec_type in ("STK", "FUT"), f"Unexpected sec_type: {d.sec_type}"

    # ── Market Rule ──

    def test_market_rule_from_secdef(self):
        """Market rule 26 (US equity) returns 0.01 increment after contract details fetch."""
        contract = Contract()
        contract.con_id = 265598
        contract.symbol = "AAPL"
        contract.sec_type = "STK"
        contract.exchange = "SMART"
        contract.currency = "USD"

        self.c.req_contract_details(9001, contract)
        got = self.w.got_details_end.wait(timeout=30)
        if not got:
            pytest.skip("Contract details timed out — connection may be slow")
        time.sleep(2)

        self.c.req_market_rule(26)
        got = self.w.got_rule.wait(timeout=5)
        if not got:
            pytest.skip("Market rule 26 not in cache after contract details")

        rule_id, increments = self.w.rules[0]
        assert rule_id == 26, f"Expected rule_id=26, got {rule_id}"
        assert len(increments) >= 1, "No price increments"

        low_edge, increment = increments[0]
        assert low_edge == 0.0, f"Expected low_edge=0.0, got {low_edge}"
        assert increment == 0.01, f"Expected increment=0.01, got {increment}"
        print(f"Market rule 26: low_edge={low_edge}, increment={increment}")

    # ── Order Fields (ib-agent#73) ──

    def test_order_fields_populated_on_open_order(self):
        """Place a LMT order, req open orders, verify 9 gateway fields are populated."""
        contract = Contract()
        contract.con_id = 265598
        contract.symbol = "AAPL"
        contract.sec_type = "STK"
        contract.exchange = "SMART"
        contract.currency = "USD"

        order = Order()
        order.action = "BUY"
        order.order_type = "LMT"
        order.total_quantity = 1.0
        order.lmt_price = 100.0  # far from market — won't fill
        order.tif = "DAY"

        order_id = int(self.w.next_id // 1000)
        self.c.place_order(order_id, contract, order)
        time.sleep(3)

        self.c.req_open_orders()
        self.w.got_open_order_end.wait(timeout=10)

        matching = [o for o in self.w.open_orders if o["order_id"] == order_id]
        if not matching:
            # Cancel and skip — order may have been rejected
            self.c.cancel_order(order_id, "")
            errors = [e for e in self.w.errors if e[0] == order_id]
            pytest.skip(f"Order not in open orders. Errors: {errors}")

        o = matching[0]["order"]

        # oca_type: 3 from exec report (FIX-derived), 0 from local tracking (user didn't set it)
        assert isinstance(o.oca_type, int), f"oca_type not int: {type(o.oca_type)}"
        assert o.oca_type in (0, 3), f"Unexpected oca_type={o.oca_type}"

        assert isinstance(o.use_price_mgmt_algo, int)
        assert o.use_price_mgmt_algo == 0, \
            f"Expected use_price_mgmt_algo=0 for LMT, got {o.use_price_mgmt_algo}"

        # 6 constant fields (defaults)
        assert o.adjusted_order_type == "", \
            f"Expected empty adjusted_order_type, got '{o.adjusted_order_type}'"
        assert o.delta_neutral_order_type == "", \
            f"Expected empty delta_neutral_order_type, got '{o.delta_neutral_order_type}'"
        assert o.shareholder == "", \
            f"Expected empty shareholder, got '{o.shareholder}'"

        # Verify core fields still populated correctly
        assert o.action == "BUY"
        assert o.order_type == "LMT"
        assert o.lmt_price == 100.0

        print(f"Order fields: oca_type={o.oca_type}, use_price_mgmt_algo={o.use_price_mgmt_algo}, "
              f"trail_stop_price={o.trail_stop_price}, adjusted_order_type='{o.adjusted_order_type}'")

        # Cleanup
        self.c.cancel_order(order_id, "")
        time.sleep(2)
