"""
Regression test for issue #112: place_order routes to wrong contract
when called in rapid succession for different symbols.

Sends 3 LMT BUY orders (far below market) for SPY, AAPL, MSFT in a tight
loop, then verifies via order_status that each order_id got acknowledged
(not rejected with wrong symbol).  Cancels all at the end.
"""
import os, threading, time
import pytest
from ibx import EClient, EWrapper, Contract, Order

SPY  = (756733,  "SPY")
AAPL = (265598,  "AAPL")
MSFT = (272093,  "MSFT")

SYMBOLS = [SPY, AAPL, MSFT]

def make_contract(con_id, symbol):
    c = Contract()
    c.con_id = con_id
    c.symbol = symbol
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
        self.order_statuses = {}       # oid -> list of statuses
        self.all_acked = threading.Event()
        self.next_order_id = 0
        self._expected_oids = set()

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
            if self._expected_oids and self._expected_oids.issubset(self.order_statuses.keys()):
                self.all_acked.set()


@pytest.mark.skipif(
    not (os.environ.get("IB_USERNAME") and os.environ.get("IB_PASSWORD")),
    reason="IB credentials not set",
)
def test_rapid_multi_symbol_order_routing():
    """Send 3 orders for 3 different symbols rapidly, verify each is acked (not misrouted)."""
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

    oids = []
    for con_id, symbol in SYMBOLS:
        oid = wrapper.next_order_id
        wrapper.next_order_id += 1
        oids.append((oid, symbol))

    wrapper._expected_oids = {oid for oid, _ in oids}

    # Fire all 3 orders as fast as possible
    for oid, (con_id, symbol) in zip([o for o, _ in oids], SYMBOLS):
        order = Order()
        order.action = "BUY"
        order.total_quantity = 1
        order.order_type = "LMT"
        order.lmt_price = 1.00       # far below market, won't fill
        order.tif = "GTC"
        order.outside_rth = True
        client.place_order(oid, make_contract(con_id, symbol), order)

    # Wait for all 3 to be acknowledged
    assert wrapper.all_acked.wait(timeout=30), (
        f"Not all orders acked. Got statuses for: {list(wrapper.order_statuses.keys())}, "
        f"expected: {[o for o, _ in oids]}"
    )

    # Verify: each order should have at least one status that is NOT "Rejected"
    for oid, symbol in oids:
        statuses = wrapper.order_statuses.get(oid, [])
        assert len(statuses) > 0, f"Order {oid} ({symbol}): no status received"
        assert not all(s == "Rejected" for s in statuses), (
            f"Order {oid} ({symbol}): rejected — likely misrouted. Statuses: {statuses}"
        )
        print(f"  {symbol} (oid={oid}): {statuses}")

    # Check errors — no "invalid symbol" or "ambiguous" errors for our order IDs
    errors = [(eid, code, msg) for ev, eid, code, msg in wrapper.events
              if ev == "error" and eid in {o for o, _ in oids}]
    for eid, code, msg in errors:
        # Code 200 = "No security definition has been found" (wrong symbol sent)
        assert code != 200, f"Order {eid}: got error 200 (no security def) — symbol was misrouted: {msg}"

    # Cleanup: cancel all
    for oid, _ in oids:
        client.cancel_order(oid, "")
    time.sleep(2)

    client.disconnect()
    run_thread.join(timeout=5)
    print("PASS: All 3 symbols routed correctly")
