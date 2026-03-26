"""Live L2 order book for TSLA on NASDAQ TotalView."""
import os, sys, time, threading, pathlib

# Auto-load .env from project root
env_file = pathlib.Path(__file__).resolve().parent.parent / ".env"
if env_file.exists():
    for line in env_file.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            os.environ.setdefault(k.strip(), v.strip())

from ibx import EWrapper, EClient, Contract

ROWS = 20

class Book(EWrapper):
    def __init__(self):
        super().__init__()
        self.ready = threading.Event()
        self.bids = {}  # pos -> (price, size, mm)
        self.asks = {}
        self.lock = threading.Lock()
        self.count = 0

    def next_valid_id(self, o): self.ready.set()
    def managed_accounts(self, a): pass
    def connect_ack(self): pass

    def update_mkt_depth_l2(self, req_id, pos, mm, op, side, price, size, smart):
        with self.lock:
            book = self.bids if side == 1 else self.asks
            if op == 2:  # delete
                book.pop(pos, None)
            else:
                book[pos] = (price, size, mm)
            self.count += 1

    def error(self, req_id, code, msg, adv=""):
        if code not in (2104, 2106, 2158):
            print(f"\r  error({req_id}, {code}, {msg})", flush=True)

    def snapshot(self):
        with self.lock:
            b = sorted(self.bids.items())
            a = sorted(self.asks.items())
        return b, a

w = Book()
c = EClient(w)
c.connect(
    username=os.environ["IB_USERNAME"],
    password=os.environ["IB_PASSWORD"],
    host=os.environ.get("IB_HOST", "cdc1.ibllc.com"),
    paper=True,
)
threading.Thread(target=c.run, daemon=True).start()
assert w.ready.wait(30), "Connection failed"

tsla = Contract()
tsla.con_id = 76792991
tsla.symbol = "TSLA"
tsla.sec_type = "STK"
tsla.exchange = "SMART"
tsla.currency = "USD"

c.req_mkt_depth(1, tsla, ROWS, True, [])  # SmartDepth: multi-exchange aggregated book
print("Waiting for depth data...", end="", flush=True)
time.sleep(3)

try:
    while True:
        bids, asks = w.snapshot()
        lines = []
        lines.append(f"\033[2J\033[H")  # clear screen
        lines.append(f"  TSLA L2 — SmartDepth (multi-exchange)    updates: {w.count}")
        lines.append(f"  {'BID':>36}  |  {'ASK':<36}")
        lines.append(f"  {'Size':>8}  {'Price':>10}  {'MM':<6}  |  {'MM':<6}  {'Price':<10}  {'Size':<8}")
        lines.append(f"  {'-'*36}  |  {'-'*36}")
        n = max(len(bids), len(asks), 1)
        for i in range(min(n, ROWS)):
            _, (bp, bs, bm) = bids[i] if i < len(bids) else (0, ("", "", ""))
            _, (ap, az, am) = asks[i] if i < len(asks) else (0, ("", "", ""))
            b_str = f"{bs:>8.0f}  {bp:>10.2f}  {bm:<6}" if bp else " " * 28
            a_str = f"{am:<6}  {ap:<10.2f}  {az:<8.0f}" if ap else ""
            lines.append(f"  {b_str}  |  {a_str}")
        sys.stdout.write("\n".join(lines) + "\n")
        sys.stdout.flush()
        time.sleep(0.3)
except KeyboardInterrupt:
    pass
finally:
    c.cancel_mkt_depth(1)
    c.disconnect()
