<p align="center">
  <img src="docs/book/src/banner.png" alt="IBX" width="100%">
</p>

<p align="center">
  <strong>An Interactive Brokers client with no gateway. No JVM, no window, no process to keep alive.</strong>
</p>

<p align="center">
  <a href="https://github.com/userFRM/ibx/actions"><img src="https://github.com/userFRM/ibx/actions/workflows/tests.yml/badge.svg" alt="Build"></a>
  <img src="https://img.shields.io/badge/rust-1.89+-orange.svg" alt="Rust version">
  <img src="https://img.shields.io/badge/python-3.11+-blue.svg" alt="Python version">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://userfrm.github.io/ibx/"><img src="https://img.shields.io/badge/docs-book-green.svg" alt="Docs"></a>
</p>

## Introduction

IBX implements the IBKR client protocol directly. It authenticates, maintains
the market-data, trading, historical and security-definition connections, and
exposes the same API a program would otherwise reach through IB Gateway — with
no gateway process, JVM, or local socket in between.

The API is source-compatible with the TWS API (`EClient` / `EWrapper`) and, in
Python, additionally with [ib_async](https://github.com/ib-api-reloaded/ib_async)
(`IB`). Migrating an existing program changes the connect call:

```diff
- ib.connect("127.0.0.1", 4001, clientId=1)     # requires a running gateway
+ ib.connect(username="...", password="...")    # no external process
```

### What this removes

* **The gateway process** — nothing to install, launch, log into, or restart
* **The JVM** — no heap to size, no crash on bulk data
* **The localhost socket** — ticks are delivered in-process
* **The window** — runs headless, in a container, over ssh

### Requirements

* An Interactive Brokers account, paper or live
* Rust 1.89+ for the Rust client
* Python 3.11+ for the bindings

No IB software is required. The `ibapi` package is not needed either.

## Installation

### Python

```bash
uv venv .venv --python 3.13
source .venv/bin/activate          # .venv\Scripts\activate on Windows
pip install maturin
maturin develop --features python
```

### Rust

```toml
[dependencies]
ibx = { git = "https://github.com/userFRM/ibx" }
```

## Quick start

```python
import ibx

ib = ibx.IB()
ib.connect(username="your_user", password="your_pass", paper=True)

spy = ibx.Contract(symbol="SPY", secType="STK", exchange="SMART", currency="USD")

(ticker,) = ib.reqTickers(spy)
print(ticker.bid, ticker.ask)

order = ibx.Order(action="BUY", orderType="LMT", totalQuantity=1, lmtPrice=1.00)
trade = ib.placeOrder(spy, order)
ib.sleep(2)
print(trade.orderStatus.status)
ib.cancelOrder(order)

ib.disconnect()
```

A contract does not need to be qualified first: a request carrying a contract
rather than a contract id is resolved before transmission.

In Rust:

```rust
use ibx::types::model::{Contract, Order};
use ibx::{Client, Config};

let client = Client::connect(&Config {
    username: "your_user".into(),
    password: "your_pass".into(),
    paper: true,
    ..Default::default()
})?;

let spy = client.qualify(Contract::stock("SPY"))?;

// A quote exists because something is watching it, and updates itself after.
client.watch(&spy)?;
if let Some(quote) = client.ticker(&spy) {
    println!("bid {} ask {}", quote.bid, quote.ask);
}

// The order is the thing you hold. Its number is bookkeeping the client keeps.
let order = client.place(&spy, &Order::limit("BUY", 100.0, 42.50))?;
order.wait_done(Duration::from_secs(30));
println!("{} — {} filled", order.status(), order.fills().len());
order.cancel()?;

// What the session holds, without asking for any of it.
for position in client.positions() { /* ... */ }
for value in client.account_values() { /* ... */ }
```

One thread reads the session and keeps what arrives, so a position, an order, a
fill and a quote are things you look at rather than questions you ask. The
account, its holdings and anything already working are asked for as the session
opens, so they are there to read the moment it returns.

To be told as it happens rather than reading afterwards, take a stream. Both
are iterators, so they read the way anything else in Rust reads:

```rust
for tick in client.ticks(&spy)? {
    println!("{} at {}", tick.size, tick.price);
}

for event in client.order_events() {
    println!("order {} is {}", event.order_id, event.status);
}
```

`ticks` subscribes and hands back the stream in one, and only that contract's
ticks arrive on it — a caller watching one thing does not filter out the rest.
Dropping the stream withdraws the subscription.

**There is one client.** The calls above are the ones with a shape worth
having; every other request the protocol carries — scanners, news, corporate
events, fundamentals, option chains, histograms, market rules, P&L — is on the
same `client`, in the reference client's own shape:

```rust
client.req_scanner_parameters()?;
client.req_historical_news(9001, con_id, "BRFG", "", "", 10)?;
```

Nothing to import, nothing to choose between: `Client` reaches all 135. Where a
name appears on both, the session's own is the one you get, because it is the
better answer — `positions()` reads what the session already holds rather than
asking again.

### Inside an async runtime

The engine is a thread of its own, so a blocking call holds the thread that
made it and nothing else. Inside a runtime that thread is one of a shared pool,
so the `async` feature moves each question onto a thread that may wait. What
does not wait — reading what the session holds — is not awaited:

```toml
ibx = { git = "https://github.com/userFRM/ibx", features = ["async"] }
```

```rust
let client = AsyncClient::connect(config).await?;
let spy = client.qualify(Contract::stock("SPY")).await?;

client.watch(&spy).await?;           // may have to ask about the contract
let quote = client.ticker(&spy);     // a memory read

let order = client.place(&spy, &Order::limit("BUY", 1.0, 1.0)).await?;
client.wait_done(&order, Duration::from_secs(30)).await;
```

Every question has the same name and the same answer on both, and a test fails
if either grows a name the other does not have.

## Running an existing program

### ib_async

An unmodified program written against
[ib_async](https://github.com/ib-api-reloaded/ib_async) runs on this engine
with its connect call changed. Nothing of ib_async is copied or modified —
install it as usual, and attach:

```python
from ib_async import IB, Stock
import ibx.ib_async

ib = ibx.ib_async.attach(IB(), username="your_user", password="your_pass")
ib.connect()                      # names no host: there is no gateway

spy = Stock("SPY", "SMART", "USD")
ib.qualifyContracts(spy)
bars = ib.reqHistoricalData(spy, "", "2 D", "1 hour", "TRADES", useRTH=True)

ib.pendingTickersEvent += lambda tickers: print(len(tickers), "updates")
ib.reqMktData(spy)
ib.sleep(5)
ib.disconnect()
```

Their `IB`, their `Wrapper`, their events, their types — this engine
underneath, and no gateway process.

### TWS API (`EClient` / `EWrapper`)

```python
import threading
from ibx import EWrapper, EClient, Contract

class App(EWrapper):
    def __init__(self):
        super().__init__()
        self.ready = threading.Event()

    def next_valid_id(self, order_id):
        self.next_id = order_id
        self.ready.set()

    def tick_price(self, req_id, tick_type, price, attrib):
        print(f"tick {tick_type}: {price}")

app = App()
client = EClient(app)
client.connect(username="your_user", password="your_pass", paper=True)
threading.Thread(target=client.run, daemon=True).start()
app.ready.wait(timeout=10)

aapl = Contract(symbol="AAPL", secType="STK", exchange="SMART", currency="USD")
client.req_mkt_data(1, aapl, "", False)
```

Both naming conventions resolve on every type and method: `reqMktData` and
`req_mkt_data`, `secType` and `sec_type`, `conId` and `con_id`. Both surfaces
drive one client and one engine — `ibx.IB` is a facade over `EClient`, they
share a session, and either may be used.

## Configuration

The gateway's configuration file is replaced by settings on the client:
announced build, time zone, execution-report scope, and others — 14 in total,
readable at runtime. Ten gateway settings have no counterpart and report why (no
window geometry, no local listening socket, no JVM heap, and no message pacing:
nothing here paces outgoing messages, which the gateway ships with off).

Rust: `EClientConfig.gateway`. Python: `ibx.configure()`.

## Documentation

* [The book](https://userfrm.github.io/ibx/) — guides, recipes and the generated API reference
* [Capabilities](docs/capabilities.md) — one row per capability, one column per client
* [Evidence](docs/evidence.md) — what each claim rests on, and the session that produced it
* [Notebooks](notebooks/) — the seven ib_async subjects, in the TWS API shape and in [ib_async's own](notebooks/ib_async_nogateway/)
* [Examples](examples/) — runnable single-file programs in Rust and Python

## License

[AGPL-3.0](LICENSE)

## Disclaimer

Interactive Brokers®, IBKR®, Trader Workstation®, and IB Gateway® are
registered trademarks of Interactive Brokers Group, Inc. This project is **not
affiliated with, endorsed by, or supported by Interactive Brokers**.

IBX is an independent, open-source project provided "as is", without warranty
of any kind.

### Legal Considerations

- **No warranty.** IBX is provided "as is", without warranty of any kind. See [LICENSE](LICENSE) for full terms.
- **Use at your own risk.** Users are solely responsible for ensuring their use of IBX complies with Interactive Brokers' Terms of Service, Customer Agreement, and any applicable laws or regulations. Using IBX may carry risks including but not limited to account restriction or termination by IB.
- **Not financial software.** IBX is an experimental research project. It is not intended as a replacement for officially supported IB software in production trading environments. The authors accept no liability for financial losses, missed trades, account issues, or any other damages arising from the use of this software.
- **Protocol stability.** IBX relies on an undocumented protocol that IB may change at any time without notice. There is no guarantee of continued functionality.

### EU Interoperability

For users and contributors in the European Union: Article 6 of the EU Software
Directive (2009/24/EC) permits reverse engineering for the purpose of achieving
interoperability with independently created software, provided that specific
conditions are met. IBX was developed with this legal framework in mind,
enabling interoperability with IB's trading infrastructure on platforms where
the official Java-based Gateway cannot run (headless Linux, containers,
embedded systems).
