"""Example #99: Bracket Order with Trailing Stop (QQQ).

Parent LMT + take-profit LMT + trailing stop — OCA linked orders.
Exercises parent-child linking via parentId, OCA grouping, TRAIL encoding,
and child activation on parent fill.

Usage:
    IB_USERNAME=xxx IB_PASSWORD=xxx python examples/ex99_bracket_order.py

Ref: https://github.com/deepentropy/ib-agent/issues/99
"""

import os
import sys
import time
import threading
from ibx import EWrapper, EClient, Contract, Order

QQQ_CON_ID = 320227571


class Wrapper(EWrapper):
    def __init__(self):
        super().__init__()
        self.connected = threading.Event()
        self.next_id = 0

        self.order_statuses = {}  # order_id -> [(status, filled, remaining, perm_id)]
        self.perm_ids = {}        # order_id -> perm_id
        self.open_orders_list = []

        self.got_status = threading.Event()
        self.got_fill = threading.Event()
        self.got_open_order_end = threading.Event()

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
        if order_id not in self.order_statuses:
            self.order_statuses[order_id] = []
        self.order_statuses[order_id].append((status, filled, remaining, perm_id))
        self.perm_ids[order_id] = perm_id
        print(f"  [status] oid={order_id} status={status} filled={filled} "
              f"remaining={remaining} permId={perm_id}")
        self.got_status.set()
        if status == "Filled" or filled > 0:
            self.got_fill.set()

    def open_order(self, order_id, contract, order, order_state):
        self.open_orders_list.append((order_id, contract, order))

    def open_order_end(self):
        self.got_open_order_end.set()

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158, 202):
            print(f"  [error] oid={req_id} code={error_code}: {error_string}")


def make_qqq():
    c = Contract()
    c.con_id = QQQ_CON_ID
    c.symbol = "QQQ"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def alloc_id(wrapper):
    oid = wrapper.next_id
    wrapper.next_id += 1
    return oid


def get_price(client, wrapper):
    """Get current QQQ price via a short market data subscription."""
    qqq = make_qqq()
    client.req_mkt_data(7777, qqq, "", False)
    got = wrapper.got_tick.wait(timeout=30)
    client.cancel_mkt_data(7777)
    if not got or wrapper.last_price <= 0:
        return None
    return wrapper.last_price


def make_bracket(wrapper, price):
    """Build a bracket: parent BUY LMT (won't fill) + TP + trailing stop.

    The parent and the take-profit are placed with transmit=False: they
    are held, with no answer, until the stop, the last order of the
    bracket, is placed with transmit=True; the three then go out together
    (ibx#509). The children are linked to the parent through parent_id and
    rest in PreSubmitted until the parent fills.
    """
    parent_id = alloc_id(wrapper)
    tp_id = alloc_id(wrapper)
    sl_id = alloc_id(wrapper)
    oca = f"bracket_{parent_id}"

    parent = Order()
    parent.action = "BUY"
    parent.total_quantity = 1
    parent.order_type = "LMT"
    parent.lmt_price = round(price * 0.80, 2)  # 20% below — won't fill
    parent.tif = "GTC"
    parent.outside_rth = True
    parent.transmit = False

    tp = Order()
    tp.action = "SELL"
    tp.total_quantity = 1
    tp.order_type = "LMT"
    tp.lmt_price = round(price * 1.10, 2)
    tp.tif = "GTC"
    tp.outside_rth = True
    tp.parent_id = parent_id
    tp.oca_group = oca
    tp.oca_type = 1
    tp.transmit = False

    sl = Order()
    sl.action = "SELL"
    sl.total_quantity = 1
    sl.order_type = "TRAIL"
    sl.trailing_percent = 1.0
    sl.tif = "GTC"
    sl.outside_rth = True
    sl.parent_id = parent_id
    sl.oca_group = oca
    sl.oca_type = 1

    return parent_id, tp_id, sl_id, parent, tp, sl


def make_bracket_at_market(wrapper, price):
    """Build a bracket with MKT parent (will fill) + TP + trailing stop.

    Staged as in make_bracket: the MKT parent is held until the stop is
    placed with transmit=True, so it cannot fill before its children are
    attached.
    """
    parent_id = alloc_id(wrapper)
    tp_id = alloc_id(wrapper)
    sl_id = alloc_id(wrapper)
    oca = f"bracket_{parent_id}"

    parent = Order()
    parent.action = "BUY"
    parent.total_quantity = 1
    parent.order_type = "MKT"
    parent.tif = "DAY"
    parent.transmit = False

    tp = Order()
    tp.action = "SELL"
    tp.total_quantity = 1
    tp.order_type = "LMT"
    tp.lmt_price = round(price * 1.20, 2)
    tp.tif = "GTC"
    tp.outside_rth = True
    tp.parent_id = parent_id
    tp.oca_group = oca
    tp.oca_type = 1
    tp.transmit = False

    sl = Order()
    sl.action = "SELL"
    sl.total_quantity = 1
    sl.order_type = "TRAIL"
    sl.trailing_percent = 2.0
    sl.tif = "GTC"
    sl.outside_rth = True
    sl.parent_id = parent_id
    sl.oca_group = oca
    sl.oca_type = 1

    return parent_id, tp_id, sl_id, parent, tp, sl


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

    qqq = make_qqq()

    # Get current price
    price = get_price(c, w)
    if price is None:
        print("No price data — market may be closed.")
        c.disconnect()
        t.join(timeout=5)
        sys.exit(0)
    print(f"QQQ last price: {price}")

    # ── Test 1: Place bracket (far from market) and cancel ────────────────
    print("\n=== Test 1: Bracket place + cancel ===")
    pid, tid, sid, parent, tp, sl = make_bracket(w, price)
    c.place_order(pid, qqq, parent)
    c.place_order(tid, qqq, tp)
    c.place_order(sid, qqq, sl)

    assert w.got_status.wait(timeout=30), "No order status received"
    time.sleep(3)

    # Parent should have status
    parent_st = w.order_statuses.get(pid, [])
    assert len(parent_st) > 0, "Parent should have status"
    print(f"  Parent statuses: {[s[0] for s in parent_st]}")

    # Children should be PreSubmitted or Inactive
    for child_id, label in ((tid, "TP"), (sid, "SL")):
        child_st = w.order_statuses.get(child_id, [])
        if child_st:
            last = child_st[-1][0]
            print(f"  {label} (oid={child_id}) last status: {last}")
            assert last in ("PreSubmitted", "Inactive", "Submitted"), \
                f"{label} unexpected: {last}"

    # All should have permIds
    for oid in (pid, tid, sid):
        if oid in w.perm_ids:
            assert w.perm_ids[oid] > 0
    print("  All permIds positive ✓")

    # Verify via reqOpenOrders
    c.req_open_orders()
    w.got_open_order_end.wait(timeout=15)
    our = {o[0] for o in w.open_orders_list if o[0] in (pid, tid, sid)}
    print(f"  Open order IDs: {our}")

    # Cancel parent — children should auto-cancel
    c.cancel_order(pid, "")
    time.sleep(3)
    print("  Cancelled parent (children should cascade)")

    # ── Test 2: Bracket at market → fill → children activate ─────────────
    print("\n=== Test 2: Bracket at market (fill parent) ===")
    w.got_status.clear()
    w.got_fill.clear()
    w.order_statuses.clear()

    pid2, tid2, sid2, parent2, tp2, sl2 = make_bracket_at_market(w, price)
    c.place_order(pid2, qqq, parent2)
    c.place_order(tid2, qqq, tp2)
    c.place_order(sid2, qqq, sl2)

    got_fill = w.got_fill.wait(timeout=30)
    if not got_fill:
        c.cancel_order(pid2, "")
        time.sleep(2)
        print("  No fill — market may be closed. Skipping fill test.")
    else:
        time.sleep(5)
        for child_id, label in ((tid2, "TP"), (sid2, "SL")):
            child_st = w.order_statuses.get(child_id, [])
            if child_st:
                names = [s[0] for s in child_st]
                print(f"  {label} statuses after parent fill: {names}")
                assert any(s in ("Submitted", "PreSubmitted") for s in names), \
                    f"{label} should be active, got: {names}"

        # Cancel children
        c.cancel_order(tid2, "")
        c.cancel_order(sid2, "")
        time.sleep(3)

        # Flatten position
        print("\n  Cleanup: sell to flatten")
        sell_id = alloc_id(w)
        sell = Order()
        sell.action = "SELL"
        sell.total_quantity = 1
        sell.order_type = "MKT"
        sell.tif = "DAY"
        c.place_order(sell_id, qqq, sell)
        time.sleep(5)

    c.disconnect()
    t.join(timeout=5)
    print("\n✓ Example #99 complete")


if __name__ == "__main__":
    run_example()
