"""Example #97: Real-Time Market Data + L2 Depth Subscribe (AAPL).

Streaming top-of-book ticks and Level 2 depth. Exercises farm routing,
subscription lifecycle, binary depth encoding, and unsubscribe cleanup.

Usage:
    IB_USERNAME=xxx IB_PASSWORD=xxx python examples/ex97_market_data_depth.py

Ref: https://github.com/deepentropy/ib-agent/issues/97
"""

import os
import sys
import time
import threading
from ibx import EWrapper, EClient, Contract

AAPL_CON_ID = 265598


class Wrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()

        # Top-of-book
        self.ticks_price = []    # (req_id, tick_type, price)
        self.ticks_size = []     # (req_id, tick_type, size)
        self.ticks_string = []   # (req_id, tick_type, value)
        self.ticks_generic = []  # (req_id, tick_type, value)
        self.got_tick = threading.Event()

        # L2 depth
        self.depth_updates = []     # (req_id, position, operation, side, price, size)
        self.depth_l2_updates = []  # (req_id, position, mm, operation, side, price, size, smart)
        self.got_depth = threading.Event()

        # Depth exchanges
        self.depth_exchanges = None
        self.got_depth_exchanges = threading.Event()

        # Leak detection
        self._tob_cancelled = False
        self._depth_cancelled = False
        self.tick_after_cancel = False
        self.depth_after_cancel = False

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


def run_example():
    username = os.environ.get("IB_USERNAME", "")
    password = os.environ.get("IB_PASSWORD", "")
    if not username or not password:
        print("Set IB_USERNAME and IB_PASSWORD")
        sys.exit(1)

    w = Wrapper()
    c = EClient(w)
    c.connect(username=username, password=password,
              host=os.environ.get("IB_HOST", "cdc1.ibllc.com"), paper=True)
    t = threading.Thread(target=c.run, daemon=True)
    t.start()
    assert w.connected.wait(timeout=15), "Connection failed"
    print("Connected.")

    aapl = make_aapl()

    # ── Step 1: Top-of-book streaming ─────────────────────────────────────
    print("\n=== Step 1: reqMktData (top-of-book) ===")
    c.req_mkt_data(1, aapl, "", False)

    got = w.got_tick.wait(timeout=30)
    if not got:
        c.cancel_mkt_data(1)
        print("  No ticks — market may be closed. Skipping TOB checks.")
    else:
        time.sleep(5)
        c.cancel_mkt_data(1)
        w._tob_cancelled = True

        prices = [p for p in w.ticks_price if p[0] == 1 and p[2] > 0]
        sizes = [s for s in w.ticks_size if s[0] == 1]
        tick_types = {p[1] for p in prices}

        print(f"  Price ticks: {len(prices)}, Size ticks: {len(sizes)}")
        print(f"  Tick types seen: {tick_types}")
        assert len(prices) > 0, "Should have positive price ticks"
        assert len(sizes) > 0, "Should have size ticks"

        # Price sanity
        for p in prices:
            assert 50 < p[2] < 500, f"AAPL price {p[2]} out of range"
        print(f"  Sample price: {prices[0][2]}")

        # Leak check
        time.sleep(3)
        assert not w.tick_after_cancel, "Ticks still arriving after cancelMktData"
        print("  No leak after cancel ✓")

    # ── Step 2: Depth exchanges ───────────────────────────────────────────
    print("\n=== Step 2: reqMktDepthExchanges ===")
    c.req_mkt_depth_exchanges()
    assert w.got_depth_exchanges.wait(timeout=15), "mkt_depth_exchanges not received"
    assert w.depth_exchanges is not None
    print(f"  Depth exchanges received: {len(w.depth_exchanges)} entries")

    # ── Step 3: L2 depth ──────────────────────────────────────────────────
    print("\n=== Step 3: reqMktDepth (L2) ===")
    c.req_mkt_depth(2, aapl, 10)

    got = w.got_depth.wait(timeout=30)
    if not got:
        c.cancel_mkt_depth(2)
        print("  No depth — market may be closed or no L2 subscription.")
    else:
        time.sleep(5)
        c.cancel_mkt_depth(2)
        w._depth_cancelled = True

        total = len(w.depth_updates) + len(w.depth_l2_updates)
        all_sides = set()
        for d in w.depth_updates:
            all_sides.add(d[3])
        for d in w.depth_l2_updates:
            all_sides.add(d[4])

        print(f"  L1 depth updates: {len(w.depth_updates)}")
        print(f"  L2 depth updates: {len(w.depth_l2_updates)}")
        print(f"  Sides seen: {all_sides}")
        assert total > 0, "Should have depth updates"

        # Leak check
        time.sleep(3)
        assert not w.depth_after_cancel, "Depth still arriving after cancelMktDepth"
        print("  No leak after cancel ✓")

    # ── Step 4: Combined TOB + depth, cancel independently ────────────────
    print("\n=== Step 4: Combined TOB + depth ===")
    w.got_tick.clear()
    w.got_depth.clear()
    w._tob_cancelled = False
    w._depth_cancelled = False

    c.req_mkt_data(1, aapl, "", False)
    c.req_mkt_depth(2, aapl, 5)

    got_tick = w.got_tick.wait(timeout=30)
    if not got_tick:
        c.cancel_mkt_data(1)
        c.cancel_mkt_depth(2)
        print("  No data — market may be closed.")
    else:
        time.sleep(5)

        # Cancel depth first — TOB should keep streaming
        c.cancel_mkt_depth(2)
        w._depth_cancelled = True
        tick_count_before = len(w.ticks_price)
        time.sleep(3)
        tick_count_after = len(w.ticks_price)

        if tick_count_after > tick_count_before:
            print(f"  TOB kept streaming after depth cancel: "
                  f"{tick_count_before} → {tick_count_after} ticks ✓")
        else:
            print(f"  TOB tick count unchanged ({tick_count_before}) — market may be slow")

        c.cancel_mkt_data(1)
        w._tob_cancelled = True

    c.disconnect()
    t.join(timeout=5)
    print("\n✓ Example #97 complete")


if __name__ == "__main__":
    run_example()
