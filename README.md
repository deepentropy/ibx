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
  <a href="#architecture">Architecture</a>
</p>

---

IBX connects directly to Interactive Brokers servers using IB's native protocol — bypassing the official Java Gateway entirely. Built in Rust for ultra-low-latency, available as both a Rust library and a Python library via PyO3.

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

Implement the `Strategy` trait:

```rust
use ibx::engine::context::{Context, Strategy};
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::types::*;

struct MyStrategy;

impl Strategy for MyStrategy {
    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let quote = ctx.quote(instrument);
        if ctx.position(instrument) == 0 {
            ctx.submit_limit(instrument, Side::Buy, 1, quote.bid);
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        let tp = fill.price + 50 * PRICE_SCALE / 100; // $0.50 take-profit
        ctx.submit_limit(fill.instrument, Side::Sell, fill.qty as u32, tp);
    }
}

fn main() {
    let config = GatewayConfig {
        username: "your_username".into(),
        password: "your_password".into(),
        host: "cdc1.ibllc.com".into(),
        paper: true,
    };

    let (gw, farm, ccp, hmds) = Gateway::connect(&config).unwrap();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(MyStrategy, farm, ccp, hmds, None);

    hot_loop.context_mut().register_instrument(756733); // SPY
    control_tx.send(ControlCommand::Subscribe {
        con_id: 756733,
        symbol: "SPY".into(),
    }).unwrap();

    hot_loop.run(); // blocks, runs strategy inline
}
```

## Python Usage

### Install

```bash
# Create venv and build
uv venv .venv --python 3.13
source .venv/bin/activate  # or .venv\Scripts\activate on Windows
pip install maturin
maturin develop --features python
```

### Connect and stream market data

```python
import ibx

engine = ibx.connect(username="your_user", password="your_pass", paper=True)
spy = engine.subscribe(conid=756733, symbol="SPY")

# Read quotes (lock-free SeqLock read)
quote = engine.quote(spy)
print(f"SPY bid={quote.bid:.2f} ask={quote.ask:.2f}")

# Account data
acct = engine.account()
print(f"Net liquidation: ${acct.net_liquidation:,.2f}")
```

### Submit and manage orders

```python
# Limit order
order_id = engine.submit_limit(spy, "BUY", qty=1, price=680.50)

# Modify
new_id = engine.modify(order_id, price=681.00, qty=1)

# Cancel
engine.cancel(new_id)

# Poll fills and order updates
for fill in engine.fills():
    print(f"Filled {fill.qty} @ ${fill.price:.2f}")

for update in engine.order_updates():
    print(f"Order {update.order_id}: {update.status}")

engine.shutdown()
```

### Order types

```python
engine.submit_market(spy, "BUY", qty=1)
engine.submit_stop(spy, "SELL", qty=1, stop_price=670.00)
engine.submit_stop_limit(spy, "SELL", qty=1, price=669.50, stop_price=670.00)
engine.submit_limit_gtc(spy, "BUY", qty=1, price=650.00, outside_rth=True)
```

## Architecture

```
                    IBX
    ┌───────────────────────────────┐
    │         HotLoop<S>            │
    │  ┌─────────┐  ┌───────────┐  │
    │  │ Strategy │  │  Context  │  │    Rust: monomorphized, zero-vtable
    │  │ on_tick  │──│ bid/ask   │  │    Python: BridgeStrategy + SharedState
    │  │ on_fill  │  │ orders    │  │
    │  └─────────┘  └───────────┘  │
    │       │              │        │
    │  ┌────▼──────────────▼────┐   │
    │  │   Protocol Stack       │   │
    │  │ TLS → HMAC → FIXCOMP   │   │
    │  │ → FIX → Binary Ticks   │   │
    │  └────────────────────────┘   │
    └──────────┬──────────┬─────────┘
               │          │
          ┌────▼───┐ ┌────▼───┐
          │ usfarm │ │  CCP   │
          │ market │ │ orders │
          │  data  │ │  auth  │
          └────┬───┘ └────┬───┘
               │          │
         ──────▼──────────▼──────
              IB Servers
```

**Hot path**: Poll farm socket (non-blocking) -> HMAC verify -> zlib decompress -> binary tick decode -> update pre-allocated Quote array -> `strategy.on_tick()` -> drain orders -> FIX encode -> send to CCP. All on a single pinned core, zero allocations.

**Python bridge**: The Rust hot loop runs on a dedicated background thread. Python reads market data through SeqLock-protected shared memory (lock-free, writer never blocks). Orders flow through a crossbeam SPSC channel. No GIL contention on the hot path.

## Testing

```bash
# Unit tests (475+)
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
