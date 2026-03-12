<p align="center">
  <img src="assets/banner.png" alt="IBX" width="100%">
</p>

<p align="center">
  <strong>Direct IB connection engine. No Java Gateway. No middleman.</strong>
</p>

<p align="center">
  <a href="#benchmarks">Benchmarks</a> &bull;
  <a href="#rust-usage">Rust</a> &bull;
  <a href="#python-usage">Python</a> &bull;
  <a href="#notebooks">Notebooks</a> &bull;
  <a href="#architecture">Architecture</a>
</p>

---

IBX connects directly to Interactive Brokers servers using IB's native protocol — bypassing the official Java Gateway entirely. Built in Rust for ultra-low-latency, available as both a Rust library (`Client` + channel-based events) and a Python library via PyO3 (ibapi-compatible `EClient`/`EWrapper`).

## Benchmarks

> **Note:** These benchmarks were run on a paper trading account with limited sample sizes and no full statistical coverage. Results are indicative, not definitive. Comprehensive benchmarking on a live account is a TODO.

SPY on IB paper account, public internet (not colocated). Compared to the official C++ TWS API connecting through IB Gateway on localhost.

### Order Latency

| Metric | IBX | C++ TWS API | Ratio |
|---|---|---|---|
| Limit submit → ack | 114.8ms | 632.9ms | **5.5x faster** |
| Limit cancel → confirm | 125.7ms | 148.2ms | 1.2x faster |
| **Limit full round-trip** | **240.5ms** | **781.1ms** | **3.2x faster** |
| Market order mean RTT | 1,113ms | — | — |
| Market order slippage | $0.09 | — | — |

### Tick Decode Latency

| Percentile | IBX | C++ TWS API | Ratio |
|---|---|---|---|
| P50 | 14.2us | 8.9us | 1.6x* |
| P99 | 25.4us | 22.4us | 1.1x* |
| Max | 41.6us | 27.6us | — |

*IBX decodes the full protocol stack (TLS → HMAC → FIXCOMP → binary ticks). IB Gateway pre-parses and feeds callbacks over localhost — no crypto, no decompression.

### Analysis

The biggest win is **order latency**: IBX saves ~500ms per order vs the official gateway. The IB Gateway Java app adds overhead from GC pauses, FIX serialization through the Java stack, and the extra localhost socket hop.

Tick decode is 1.6x slower — expected and acceptable. At IB's ~4 ticks/sec paper rate, the 5us difference is negligible. The real win is eliminating the Java gateway as a dependency entirely.

## Rust Usage

Add to `Cargo.toml`:

```toml
[dependencies]
ibx = { git = "https://github.com/deepentropy/ibx" }
```

Connect and receive events:

```rust
use ibx::{Client, ClientConfig, Event};
use ibx::types::*;

fn main() {
    let client = Client::connect(&ClientConfig {
        username: "your_username".into(),
        password: "your_password".into(),
        host: "cdc1.ibllc.com".into(),
        paper: true,
        core_id: None,
    }).unwrap();

    let spy = client.subscribe(756733, "SPY");

    while let Ok(event) = client.recv() {
        match event {
            Event::Tick(instrument) if instrument == spy => {
                let q = client.quote(spy);
                // q.bid, q.ask, q.last are i64 fixed-point (PRICE_SCALE = 10^8)
            }
            Event::Fill(fill) => {
                println!("Filled {} @ {}", fill.qty, fill.price);
            }
            Event::OrderUpdate(update) => {
                println!("Order {}: {:?}", update.order_id, update.status);
            }
            _ => {}
        }
    }
}
```

### Place and manage orders

```rust
// Limit order
let id = client.next_order_id();
client.place_order(OrderRequest::SubmitLimit {
    order_id: id, instrument: spy, side: Side::Buy, qty: 1,
    price: client.quote(spy).bid,
});

// Modify price
let new_id = client.modify_order(id, new_price, 1);

// Cancel
client.cancel_order(new_id);
```

## Python Usage

IBX exposes an [ibapi](https://github.com/InteractiveBrokers/tws-api)-compatible `EClient`/`EWrapper` API. Same callback pattern, same method names — but connecting directly through the Rust engine instead of through TWS or IB Gateway. Drop-in compatible with existing ibapi and [ib_async](https://github.com/ib-api-reloaded/ib_async) code.

### Install

```bash
# Create venv and build
uv venv .venv --python 3.13
source .venv/bin/activate  # or .venv\Scripts\activate on Windows
pip install maturin
maturin develop --features python
```

### Connect and trade

```python
import threading
from ibx import EWrapper, EClient, Contract, Order

class App(EWrapper):
    def __init__(self):
        super().__init__()
        self.next_id = None
        self.connected = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.connected.set()

    def managed_accounts(self, accounts_list):
        print(f"Account: {accounts_list}")

    def order_status(self, order_id, status, filled, remaining,
                     avg_fill_price, perm_id, parent_id,
                     last_fill_price, client_id, why_held, mkt_cap_price):
        print(f"Order {order_id}: {status} filled={filled}")

    def tick_price(self, req_id, tick_type, price, attrib):
        print(f"Tick {tick_type}: {price}")

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        if error_code not in (2104, 2106, 2158):
            print(f"Error {error_code}: {error_string}")

app = App()
client = EClient(app)
client.connect(username="your_user", password="your_pass", paper=True)

thread = threading.Thread(target=client.run, daemon=True)
thread.start()
app.connected.wait(timeout=10)

# Market data
aapl = Contract(con_id=265598, symbol="AAPL")
client.req_mkt_data(1, aapl)

# Orders
order = Order(order_id=app.next_id, action="BUY", total_quantity=1,
              order_type="LMT", lmt_price=150.00)
client.place_order(app.next_id, aapl, order)

# Account
client.req_positions()
client.req_account_summary(1, "All", "NetLiquidation,BuyingPower")

client.disconnect()
```

### Supported EClient Methods

| Category | Methods |
|---|---|
| **Connection** | `connect`, `disconnect`, `is_connected`, `run`, `get_account_id` |
| **Market Data** | `req_mkt_data`, `cancel_mkt_data`, `req_tick_by_tick_data`, `cancel_tick_by_tick_data` |
| **Orders** | `place_order`, `cancel_order`, `req_global_cancel`, `req_ids` |
| **Account** | `req_positions`, `cancel_positions`, `req_account_summary`, `cancel_account_summary`, `req_account_updates`, `req_pnl`, `cancel_pnl`, `req_pnl_single`, `cancel_pnl_single` |
| **Historical** | `req_historical_data`, `cancel_historical_data`, `req_head_time_stamp` |
| **Reference** | `req_contract_details` |

### Supported Order Types

MKT, LMT, STP, STP LMT, TRAIL, TRAIL LIMIT, MOC, LOC, MTL, MIT, LIT, MKT PRT, STP PRT, REL, PEG MKT, PEG MID, MIDPRICE, SNAP MKT, SNAP MID, SNAP PRI, BOX TOP. Algo orders: VWAP, TWAP, Arrival Price, Close Price, Dark Ice, PctVol.

## Notebooks

Jupyter notebooks adapted from [ib_async's examples](https://ib-api-reloaded.github.io/ib_async/notebooks.html), using the ibapi-compatible `EClient`/`EWrapper` pattern. All connect through the Rust engine — no TWS or IB Gateway needed.

| Notebook | Description |
|---|---|
| [basics](notebooks/basics.ipynb) | Connect, positions, account summary |
| [contract_details](notebooks/contract_details.ipynb) | Request contract metadata (AAPL, TSLA) |
| [bar_data](notebooks/bar_data.ipynb) | Head timestamp, historical bars, pandas/matplotlib plot |
| [tick_data](notebooks/tick_data.ipynb) | L1 streaming, live quote table, tick-by-tick last & bid/ask |
| [ordering](notebooks/ordering.ipynb) | Limit orders, cancel, market orders, sell to flatten |

> **Note:** `market_depth`, `option_chain`, and `scanners` notebooks are placeholders — blocked on L2 depth (#31), multi-asset options (#38), and scanner bridging respectively.

## Architecture

```
    ┌─────────────────────────────────────────────┐
    │           Your Code (Rust / Python)          │
    │  client.recv() → Event::Tick / Fill / ...    │
    │  client.quote(spy) → SeqLock read            │
    │  client.place_order(req) → control channel   │
    └─────────┬──────────────────────┬─────────────┘
              │ events (crossbeam)   │ commands
    ┌─────────▼──────────────────────▼─────────────┐
    │              IBX Engine (pinned thread)       │
    │  ┌────────────────────────────────────────┐  │
    │  │   Protocol Stack                       │  │
    │  │   TLS → HMAC → FIXCOMP → Binary Ticks  │  │
    │  └────────────┬───────────────┬───────────┘  │
    └───────────────┼───────────────┼──────────────┘
               ┌────▼───┐     ┌────▼───┐
               │ usfarm │     │  CCP   │
               │ market │     │ orders │
               │  data  │     │  auth  │
               └────┬───┘     └────┬───┘
                    │              │
              ──────▼──────────────▼──────
                     IB Servers
```

**Hot path**: Poll farm socket (non-blocking) → HMAC verify → zlib decompress → binary tick decode → update SeqLock quotes → push `Event::Tick` to channel → drain orders → FIX encode → send to CCP. All on a single pinned core, zero allocations.

**Rust client**: Events arrive via crossbeam channel (`client.recv()`). Quotes readable anytime via lock-free SeqLock (`client.quote()`). Orders sent via SPSC channel. No shared mutable state.

**Python bridge**: Same engine, ibapi-compatible `EClient`/`EWrapper` API. SeqLock quote reads, crossbeam order channel. No GIL contention on the hot path.

## Testing

```bash
# Unit tests (542+)
cargo test

# Integration tests (requires IB credentials in .env)
cargo test --test ib_paper_integration -- --ignored --nocapture

# Python module
maturin develop --features python
python -c "import ibx; print('ok')"
```

## Requirements

- Rust 2024 edition (1.85+)
- Python 3.11+ (for PyO3 bindings)
- Interactive Brokers account (paper or live)

## License

[MIT](LICENSE)
