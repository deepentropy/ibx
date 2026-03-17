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

IBX connects directly to Interactive Brokers servers — without requiring the official Java Gateway. Built in Rust for ultra-low-latency, available as both a Rust library and a Python library via PyO3. Both expose an ibapi-compatible `EClient`/`Wrapper` API.

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

*IBX handles the full connection stack (encryption, authentication, compression, binary decoding). IB Gateway pre-parses and feeds callbacks over localhost — no crypto, no decompression.

### Analysis

The biggest win is **order latency**: IBX measured ~500ms faster round-trip times compared to routing through the Gateway. Eliminating the extra localhost hop and Java intermediary accounts for most of the difference.

Tick decode is 1.6x slower — expected and acceptable. At IB's ~4 ticks/sec paper rate, the 5us difference is negligible. The real win is eliminating the Java gateway as a dependency entirely.

## Rust Usage

Add to `Cargo.toml`:

```toml
[dependencies]
ibx = { git = "https://github.com/deepentropy/ibx" }
```

ibapi-compatible `EClient`/`Wrapper` API — same callback pattern as C++ `EClientSocket`:

```rust
use ibx::api::{EClient, EClientConfig, Wrapper, Contract, Order};
use ibx::api::types::TickAttrib;

struct MyWrapper;
impl Wrapper for MyWrapper {
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {
        println!("tick_price: req_id={req_id} type={tick_type} price={price}");
    }
}

fn main() {
    let mut client = EClient::connect(&EClientConfig {
        username: "your_username".into(),
        password: "your_password".into(),
        host: "your_ib_host".into(),
        paper: true,
        core_id: None,
    }).unwrap();

    // Market data
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() };
    client.req_mkt_data(1, &spy, "", false, false);

    // SeqLock quote read (lock-free, any thread)
    if let Some(q) = client.quote(1) {
        // q.bid, q.ask, q.last are i64 fixed-point (PRICE_SCALE = 10^8)
    }

    // Process events via Wrapper callbacks
    let mut wrapper = MyWrapper;
    loop {
        client.process_msgs(&mut wrapper);
    }
}
```

### Place and manage orders

```rust
// Limit order
let id = client.next_order_id();
let order = Order { order_type: "LMT".into(), action: "BUY".into(),
    total_quantity: 1.0, lmt_price: 550.00, ..Default::default() };
client.place_order(id, &spy, &order).unwrap();

// Cancel
client.cancel_order(id, "");
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
| **Market Data** | `req_mkt_data`, `cancel_mkt_data`, `req_tick_by_tick_data`, `cancel_tick_by_tick_data`, `req_mkt_depth`, `cancel_mkt_depth`, `req_market_data_type` |
| **Orders** | `place_order`, `cancel_order`, `req_global_cancel`, `req_ids`, `req_open_orders`, `req_all_open_orders`, `req_completed_orders`, `req_executions` |
| **Account** | `req_positions`, `cancel_positions`, `req_positions_multi`, `cancel_positions_multi`, `req_account_summary`, `cancel_account_summary`, `req_account_updates`, `req_account_updates_multi`, `cancel_account_updates_multi`, `req_pnl`, `cancel_pnl`, `req_pnl_single`, `cancel_pnl_single`, `req_managed_accts` |
| **Historical** | `req_historical_data`, `cancel_historical_data`, `req_head_time_stamp`, `cancel_head_time_stamp`, `req_historical_ticks`, `req_real_time_bars`, `cancel_real_time_bars`, `req_histogram_data`, `cancel_histogram_data` |
| **Reference** | `req_contract_details`, `req_matching_symbols`, `req_sec_def_opt_params`, `req_market_rule`, `req_smart_components` |
| **Scanner** | `req_scanner_parameters`, `req_scanner_subscription`, `cancel_scanner_subscription` |
| **News** | `req_news_providers`, `req_news_article`, `req_historical_news`, `req_news_bulletins`, `cancel_news_bulletins` |
| **Fundamental** | `req_fundamental_data`, `cancel_fundamental_data` |
| **Options** | `calculate_implied_volatility`, `cancel_calculate_implied_volatility`, `calculate_option_price`, `cancel_calculate_option_price`, `exercise_options` |
| **Other** | `req_current_time`, `req_user_info`, `req_family_codes`, `req_soft_dollar_tiers`, `set_server_log_level`, `req_wsh_meta_data`, `req_wsh_event_data` |

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
    │  process_msgs() → Wrapper callbacks          │
    │  client.quote(id) → SeqLock read             │
    │  client.place_order(id,c,o) → control chan   │
    └─────────┬──────────────────────┬─────────────┘
              │ events (crossbeam)   │ commands
    ┌─────────▼──────────────────────▼─────────────┐
    │              IBX Engine (pinned thread)       │
    │  ┌────────────────────────────────────────┐  │
    │  │   Protocol Stack                       │  │
    │  │   Encryption → Auth → Compression → Decode │  │
    │  └────────────┬───────────────┬───────────┘  │
    └───────────────┼───────────────┼──────────────┘
               ┌────▼───┐     ┌────▼───┐
               │ market │     │  auth  │
               │  data  │     │ orders │
               │  feed  │     │ control│
               └────┬───┘     └────┬───┘
                    │              │
              ──────▼──────────────▼──────
                     IB Servers
```

**Hot path**: Poll socket (non-blocking) → verify → decompress → decode ticks → update SeqLock quotes → push `Event::Tick` to channel → drain orders → encode → send. All on a single pinned core, zero allocations.

**Rust client**: Events dispatched via `process_msgs()` → `Wrapper` callbacks. Quotes readable anytime via lock-free SeqLock (`client.quote()`). Orders sent via control channel. No shared mutable state.

**Python bridge**: Same engine, ibapi-compatible `EClient`/`EWrapper` API. SeqLock quote reads, crossbeam order channel. No GIL contention on the hot path.

## Testing

```bash
# Unit tests (813+)
cargo test

# Compatibility tests — 125 phases (requires IB_USERNAME/IB_PASSWORD env vars)
cargo test --test ib_paper_compat -- --nocapture

# Python module
maturin develop --features python
python -c "import ibx; print('ok')"
```

## Requirements

- Rust 2024 edition (1.85+)
- Python 3.11+ (for PyO3 bindings)
- Interactive Brokers account (paper or live)

## License

[AGPL-3.0](LICENSE)

## Disclaimer

Interactive Brokers®, IBKR®, Trader Workstation®, and IB Gateway® are registered trademarks of Interactive Brokers Group, Inc. This project is **not affiliated with, endorsed by, or supported by Interactive Brokers**.

IBX is an independent, open-source project provided "as is", without warranty of any kind.

### How IBX Was Built

IBX was developed through **independent analysis of network traffic** between the official IB Gateway client and IB servers. No IB software was decompiled, disassembled, or modified. The protocol implementation was built from scratch in Rust based solely on observed wire-level behavior.

This approach is consistent with the principle of **interoperability through protocol analysis** — the same method used by projects like Samba (SMB/CIFS), open-source Exchange clients, and countless other third-party implementations of proprietary network protocols.

### Legal Considerations

- **No warranty.** IBX is provided "as is", without warranty of any kind. See [LICENSE](LICENSE) for full terms.
- **Use at your own risk.** Users are solely responsible for ensuring their use of IBX complies with Interactive Brokers' Terms of Service, Customer Agreement, and any applicable laws or regulations. Using IBX may carry risks including but not limited to account restriction or termination by IB.
- **Not financial software.** IBX is an experimental research project. It is not intended as a replacement for officially supported IB software in production trading environments. The authors accept no liability for financial losses, missed trades, account issues, or any other damages arising from the use of this software.
- **Protocol stability.** IBX relies on an undocumented protocol that IB may change at any time without notice. There is no guarantee of continued functionality.

### EU Interoperability

For users and contributors in the European Union: Article 6 of the EU Software Directive (2009/24/EC) permits reverse engineering for the purpose of achieving interoperability with independently created software, provided that specific conditions are met. IBX was developed with this legal framework in mind, enabling interoperability with IB's trading infrastructure on platforms where the official Java-based Gateway cannot run (headless Linux, containers, embedded systems).
