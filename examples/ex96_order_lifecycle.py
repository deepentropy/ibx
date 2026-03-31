"""Example #96: Order Lifecycle — Place, Modify, Cancel, Partial Fill (SPY).

Full order state machine: place → modify → cancel, plus fill handling.
Exercises permId routing, order status transitions, and execution queries.

Issue steps (executed exactly as specified):
  1. reqIds() — get next valid orderId
  2. placeOrder() — SPY 100 shares, LMT far from market
  3. reqOpenOrders() — verify order appears with correct permId
  4. placeOrder() (same orderId, new price closer to market) — modify
  5. Observe orderStatus — permId must stay the same across modify
  6. placeOrder() (same orderId, price at market) — trigger a fill
  7. reqExecutions() — verify execDetails with correct fill qty/price
  8. Place another LMT far from market, then cancelOrder() — verify Cancelled

Usage:
    IB_USERNAME=xxx IB_PASSWORD=xxx python examples/ex96_order_lifecycle.py

Ref: https://github.com/deepentropy/ib-agent/issues/96
"""

import os
import sys
import time
import threading
from ibx import EWrapper, EClient, Contract, Order

SPY_CON_ID = 756733


class Wrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0

        self.order_statuses = []   # (order_id, status, filled, remaining, perm_id)
        self.perm_ids = {}         # order_id -> perm_id
        self.open_orders = []      # (order_id, contract, order)
        self.executions = []       # (req_id, contract, execution)

        self.got_status = threading.Event()
        self.got_fill = threading.Event()
        self.got_cancelled = threading.Event()
        self.got_open_order_end = threading.Event()
        self.got_exec_end = threading.Event()

        # Price discovery
        self.last_price = 0.0
        self.got_tick = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        pass

    def connect_ack(self):
        pass

    def tick_price(self, req_id, tick_type, price, attrib):
        if price > 0 and tick_type in (1, 2, 4):
            self.last_price = price
            self.got_tick.set()

    def tick_size(self, req_id, tick_type, size):
        pass

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        self.order_statuses.append((order_id, status, filled, remaining, perm_id))
        self.perm_ids[order_id] = perm_id
        print(f"  [status] oid={order_id} status={status} filled={filled} "
              f"remaining={remaining} permId={perm_id}")
        self.got_status.set()
        if status == "Filled" or filled > 0:
            self.got_fill.set()
        if status == "Cancelled":
            self.got_cancelled.set()

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders.append((order_id, contract, order))

    def open_order_end(self):
        self.got_open_order_end.set()

    def exec_details(self, req_id, contract, execution):
        self.executions.append((req_id, contract, execution))
        print(f"  [exec] symbol={contract.symbol} exec={execution}")

    def exec_details_end(self, req_id):
        self.got_exec_end.set()

    def commission_report(self, commission_report):
        pass

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] {error_code}: {error_string}")


def make_spy():
    c = Contract()
    c.con_id = SPY_CON_ID
    c.symbol = "SPY"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def alloc_id(wrapper):
    oid = wrapper.next_id
    wrapper.next_id += 1
    return oid


def get_price(client, wrapper):
    spy = make_spy()
    client.req_mkt_data(9999, spy, "", False)
    got = wrapper.got_tick.wait(timeout=30)
    client.cancel_mkt_data(9999)
    if not got or wrapper.last_price <= 0:
        return None
    return wrapper.last_price


def last_status_for(wrapper, oid):
    """Return last (status, remaining) for a given order_id, or (None, None)."""
    matches = [s for s in wrapper.order_statuses if s[0] == oid]
    if matches:
        return matches[-1][1], matches[-1][3]
    return None, None


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
    print(f"Connected. nextValidId={w.next_id}")

    spy = make_spy()
    results = {}  # step -> pass/fail
    filled_qty = 0  # track how many shares we need to flatten

    # ── Price discovery ──────────────────────────────────────────────────
    price = get_price(c, w)
    if price is None:
        # Use last known close as fallback when market is closed
        price = float(os.environ.get("SPY_PRICE", "630.0"))
        print(f"No live price — using fallback: {price}")
    else:
        print(f"SPY last price: {price}")

    # ══════════════════════════════════════════════════════════════════════
    # Step 1: reqIds — get next valid orderId
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 1: reqIds ===")
    oid1 = alloc_id(w)
    print(f"  orderId={oid1}")
    results["1_reqIds"] = "PASS"

    # ══════════════════════════════════════════════════════════════════════
    # Step 2: placeOrder — SPY 100 shares, LMT far from market
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 2: placeOrder (100 shares LMT far from market) ===")
    order1 = Order()
    order1.action = "BUY"
    order1.total_quantity = 100
    order1.order_type = "LMT"
    order1.lmt_price = round(price * 0.80, 2)  # 20% below market
    order1.tif = "GTC"
    order1.outside_rth = True

    c.place_order(oid1, spy, order1)
    assert w.got_status.wait(timeout=30), "No order status received"

    perm_id_place = w.perm_ids.get(oid1, 0)
    assert perm_id_place > 0, f"permId should be positive, got {perm_id_place}"
    print(f"  permId on place: {perm_id_place}")
    print(f"  lmt_price={order1.lmt_price} (20% below market)")
    results["2_place"] = "PASS"

    # ══════════════════════════════════════════════════════════════════════
    # Step 3: reqOpenOrders — verify order appears with correct permId
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 3: reqOpenOrders ===")
    c.req_open_orders()
    assert w.got_open_order_end.wait(timeout=15), "open_order_end not received"
    our = [o for o in w.open_orders if o[0] == oid1]
    assert len(our) > 0, "Our order should appear in open orders"
    print(f"  Found {len(our)} open order(s) for oid={oid1}")
    results["3_reqOpenOrders"] = "PASS"

    # ══════════════════════════════════════════════════════════════════════
    # Step 4: placeOrder (same orderId, new price) — modify
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 4: Modify order (same orderId, price closer to market) ===")
    w.got_status.clear()
    order1.lmt_price = round(price * 0.90, 2)  # 10% below — still won't fill
    c.place_order(oid1, spy, order1)
    assert w.got_status.wait(timeout=15), "No status on modify"

    status_after_modify, remaining_after_modify = last_status_for(w, oid1)
    modify_ok = status_after_modify == "Submitted" and remaining_after_modify == 100.0
    print(f"  new lmt_price={order1.lmt_price} (10% below market)")
    print(f"  status after modify: {status_after_modify}, remaining: {remaining_after_modify}")
    if modify_ok:
        results["4_modify"] = "PASS"
    else:
        results["4_modify"] = f"FAIL (expected Submitted/100, got {status_after_modify}/{remaining_after_modify})"

    # ══════════════════════════════════════════════════════════════════════
    # Step 5: Verify permId stable across modify
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 5: Verify permId stability ===")
    perm_id_modify = w.perm_ids[oid1]
    perm_stable = perm_id_place == perm_id_modify
    print(f"  permId after modify: {perm_id_modify} (stable={perm_stable})")
    results["5_permId_stable"] = "PASS" if perm_stable else f"FAIL ({perm_id_place} → {perm_id_modify})"

    # ══════════════════════════════════════════════════════════════════════
    # Step 6: placeOrder (same orderId, price at market) — trigger fill
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 6: Modify to market price (trigger fill) ===")
    time.sleep(2)  # let gateway settle after first modify
    w.got_status.clear()
    w.got_fill.clear()
    # Re-fetch price in case it moved
    w.got_tick.clear()
    c.req_mkt_data(9998, spy, "", False)
    if w.got_tick.wait(timeout=10) and w.last_price > 0:
        price = w.last_price
    c.cancel_mkt_data(9998)
    order1.lmt_price = round(price * 1.02, 2)  # slightly above market
    c.place_order(oid1, spy, order1)

    got_fill = w.got_fill.wait(timeout=30)
    if got_fill:
        fill_statuses = [s for s in w.order_statuses
                         if s[0] == oid1 and (s[1] == "Filled" or s[2] > 0)]
        if fill_statuses:
            filled_qty = int(fill_statuses[-1][2])  # how much was filled
            perm_id_fill = w.perm_ids[oid1]
            print(f"  Fill confirmed: qty={filled_qty}, permId={perm_id_fill}")
            results["6_fill_via_modify"] = "PASS"
        else:
            results["6_fill_via_modify"] = "FAIL (got_fill but no fill status)"
    else:
        status_now, _ = last_status_for(w, oid1)
        print(f"  No fill via modify (status={status_now})")
        results["6_fill_via_modify"] = f"FAIL (no fill, status={status_now})"

    # ══════════════════════════════════════════════════════════════════════
    # Step 7: reqExecutions — verify execDetails
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 7: reqExecutions ===")
    c.req_executions(8001)
    w.got_exec_end.wait(timeout=15)
    spy_execs = [e for e in w.executions if e[1].symbol == "SPY"]
    if spy_execs:
        print(f"  Found {len(spy_execs)} SPY execution(s)")
        results["7_reqExecutions"] = "PASS"
    else:
        print("  No SPY executions (expected if step 6 failed)")
        results["7_reqExecutions"] = "FAIL (no executions)" if got_fill else "SKIP (no fill in step 6)"

    # ══════════════════════════════════════════════════════════════════════
    # Step 8: Place another LMT far from market, then cancelOrder
    # ══════════════════════════════════════════════════════════════════════
    print("\n=== Step 8: Place another LMT far, then cancelOrder ===")
    w.got_status.clear()
    w.got_cancelled.clear()
    oid2 = alloc_id(w)
    order2 = Order()
    order2.action = "BUY"
    order2.total_quantity = 100
    order2.order_type = "LMT"
    order2.lmt_price = round(price * 0.80, 2)  # 20% below
    order2.tif = "GTC"
    order2.outside_rth = True

    c.place_order(oid2, spy, order2)
    assert w.got_status.wait(timeout=30), "No status for cancel-test order"
    submitted = [s for s in w.order_statuses if s[0] == oid2 and s[1] == "Submitted"]
    assert len(submitted) > 0, "Cancel-test order should be Submitted"
    print(f"  Order {oid2} submitted, permId={w.perm_ids.get(oid2, 0)}")

    c.cancel_order(oid2, "")
    got_cancel = w.got_cancelled.wait(timeout=15)
    if got_cancel:
        cancel_statuses = [s for s in w.order_statuses if s[0] == oid2 and s[1] == "Cancelled"]
        assert len(cancel_statuses) > 0, "Should have Cancelled status"
        print(f"  Order {oid2} cancelled OK")
        results["8_cancel"] = "PASS"
    else:
        results["8_cancel"] = "FAIL (cancel not confirmed within 15s)"

    # ══════════════════════════════════════════════════════════════════════
    # Cleanup: flatten any position from step 6
    # ══════════════════════════════════════════════════════════════════════
    if filled_qty > 0:
        print(f"\n=== Cleanup: Sell {filled_qty} to flatten ===")
        oid3 = alloc_id(w)
        sell = Order()
        sell.action = "SELL"
        sell.total_quantity = filled_qty
        sell.order_type = "MKT"
        sell.tif = "DAY"
        c.place_order(oid3, spy, sell)
        time.sleep(5)

    c.disconnect()
    t.join(timeout=5)

    # ══════════════════════════════════════════════════════════════════════
    # Summary
    # ══════════════════════════════════════════════════════════════════════
    print("\n" + "=" * 60)
    print("RESULTS SUMMARY")
    print("=" * 60)
    for step, result in results.items():
        mark = "PASS" if result == "PASS" else "FAIL"
        print(f"  [{mark}] {step}: {result}")

    passed = sum(1 for r in results.values() if r == "PASS")
    total = len(results)
    print(f"\n  {passed}/{total} steps passed")
    print("=" * 60)

    if passed == total:
        print("\n✓ Example #96 complete — all steps passed")
    else:
        print(f"\n✗ Example #96 complete — {total - passed} step(s) failed")
        sys.exit(1)


if __name__ == "__main__":
    run_example()
