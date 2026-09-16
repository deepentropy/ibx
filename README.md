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

## Contents

**Start here** — [Introduction](#introduction) · [Why this exists](#why-this-exists) · [Installation](#installation) · [Quick start](#quick-start)

**What you get** — [Beyond the documented API](#beyond-the-documented-api) · [Capabilities](#capabilities) · [Calls](#calls) · [Callbacks](#callbacks) · [Beyond the canonical list](#beyond-the-canonical-list)

**Moving across** — [Running an existing program](#running-an-existing-program) · [ib_async](#ib_async) · [TWS API](#tws-api-eclient--ewrapper)

**Under the hood** — [How it works](#how-it-works) · [Configuration](#configuration)

**The honest parts** — [What is not covered](#what-is-not-covered) · [Questions](#questions) · [Testing](#testing)

**Working on it** — [Contributing](#contributing) · [Security](#security) · [License](#license)

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

> [!TIP]
> Everything else in an existing program stays as it is — the same calls, the
> same callbacks, the same order objects. What changes is the line above and
> the removal of whatever started the gateway.

## Why this exists

A program that trades through Interactive Brokers normally talks to a local
process — IB Gateway or Trader Workstation — which talks to the venue. That
process is a Java application. It holds a heap, it holds a window unless you
fight it, it has to be logged in, and it has to stay alive for as long as your
program does.

That arrangement costs you four things:

1. **An operational dependency.** Something has to start it, watch it, restart
   it, and log in again when it drops. In a container or over ssh that is work
   you did not want.
2. **A second failure mode.** Your program can be healthy while the thing it
   depends on is wedged, and the two do not agree about it.
3. **A ceiling.** The heap is finite and bulk historical data is what finds the
   edge of it.
4. **A narrower view than the terminal's.** The gateway receives far more from
   the venue than it forwards. Whatever has no message in the documented API
   simply never reaches you.

This client speaks the venue's own protocol, so none of those apply. It
authenticates, holds the connections a session runs on, and answers the same
calls a program already makes — and because it sits where the gateway sat, what
the gateway kept to itself is reachable too.

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

> [!IMPORTANT]
> A live login enters the venue's second-factor approval, which waits on a
> device. Paper logins do not. One session per login: opening a second takes
> the first away, and the venue says which host took it.

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

> [!NOTE]
> Wheels are built for Linux, macOS and Windows; the workflow builds and tests
> on all three. Rust 1.89+ and Python 3.11+ are the floors, and the Python
> bindings are a compiled extension rather than a pure-Python package.

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

## Beyond the documented API

A quote subscription carries far more than prices. The venue publishes well over
a hundred series against a contract, and the documented API forwards a handful
of them — the rest have no message, so a program had no way to ask. This client
sits where the gateway sat and reads them.

Everything below is stated by the venue on an ordinary session. The figures are
from one paper session and will differ with the account and the day; what does
not differ is that none of it is reachable through `ibapi` or `ib_async`.

### What a company is worth, and what it is

```python
client.req_mkt_data(1, aapl, "454,678,705", False, False, [])

client.company_data(265598, 678)   # 212 fields, among them:
#   FLOAT      = 1.4352417904E10      the float, to the share
#   FLOAT_DATE = 20260901
client.company_data(265598, 454)
#   IISHOS     = 14594180000          shares on issue
#   IITPCHL    = 47.7348              percent held by institutions
client.company_data(265598, 705)
#   TRESGS     = 7.10404              how it scores against screening principles
```

> [!TIP]
> The float is the example worth giving. It is not derived from anything — the
> venue states it outright on a series the documented API never named, and it
> matched a terminal's own figure to the share.

### Where a contract's volatility stands against its own past

```python
client.numbered_figures(1, 661, fractional=True)
#   [(13, 56.155), (26, 56.155), (52, 56.155)]      implied-volatility rank
client.numbered_figures(1, 664, fractional=True)
#   [(13, 2.436), (26, 6.450), (52, 31.749)]        the same for realised
```

The number each figure is kept under is **weeks** — a quarter, a half year, a
year. Two of these series state a high and a low, and there the window is
written negative for the low and positive for the high, so a year's range is the
pair numbered `-52` and `52`.

### Five years of price history, without a history request

```python
client.numbered_figures(1, 562, fractional=False)   # the date of each point
client.numbered_figures(1, 562, fractional=True)    # the price on it
#   [(7, 765.96), (30, 776.34), … (1826, 419.586)]
```

The key is a span of days: a week, a month, a quarter, a year, three years,
five. It arrives on a quote subscription with no historical-data request behind
it at all.

### Figures with no call anywhere in the API

```python
client.stated_figures(1, 504)    # [383820.0, 1.0]   what one contract delivers
client.stated_figures(1, 527)    # [0.0154349]       volatility over twenty days
client.stated_figures(1, 407)    # four margin figures for a future
```

> [!NOTE]
> On a future, *what one contract delivers* came back as its multiplier times
> its own modelled price; on an option, a hundred times the underlying; on a
> share, the price itself. That is the kind of check every reader here is held
> to — a figure is only claimed once the wire agrees with it.

### The option model the terminal sees

`tick_option_computation` carries what the documented API names. The venue states
more on the same record, and it is all here: **rho**, **fugit**, the exercise
boundary, the forward coefficient, the model and bridge yields — and the same
model again as of the close, kept apart from the standing one.

```python
m = client.option_model(1)
m["rho"], m["fugit"], m["exerciseBoundary"], m["modelYield"]
client.closing_option_model(1)      # the same, worked out as the contract closed
```

### Asking the venue to find a trade

```python
scan = ibx.SpreadScan(version=6, request=0, under_con_id=265598,
                      account="DU1234567", min_delta=0.25)
client.req_spread_scan(1, aapl, scan)
for s in client.scanned_strategies(1):
    s["legs"], s["figures"], s["breakEvens"]
```

### And the session itself

```python
client.enabled_features()          # what the venue permits this account
client.order_permissions()         # which order types, per security type
client.algorithms_for("STK")       # which algorithms it may use
client.order_presets()             # the defaults it fills an order's blanks from
client.req_ping(); client.last_rtt_ms()
```

The full list, with what each returns, is in
[Beyond the API](https://userfrm.github.io/ibx/reference/beyond-the-api.html).

<!-- capabilities:begin — written by scripts/gen_parity_matrix.py -->

## Capabilities

One row per capability, one column per client — every one of the 78 calls and 90 callbacks the documented API names, read from each client rather than recalled.

| Client | Calls carried | Callbacks carried | |
| --- | ---: | ---: | --- |
| TWS API | 78 / 78 | 90 / 90 | nothing missing |
| ibapi | 73 / 78 | 85 / 90 | 5 absent, 5 callbacks absent |
| ib_async | 77 / 78 | 78 / 90 | 1 absent, 12 callbacks absent |
| **ibx Rust** | **78 / 78** | **81 / 90** | 9 callbacks taken, not applied |
| **ibx Python** | **78 / 78** | **81 / 90** | 9 callbacks taken, not applied |

**Nothing on that list is absent here.** 9 exist and never fire, because the venue states nothing on this connection for them to carry: there is no terminal between this client and the venue to make a verification handshake with, no socket layer of the reference client's own to report an error from, this connection does not reroute a request to another contract, and neither an exchange-for-physical quote nor a delta-neutral pairing is stated on it — a share, a fund and two futures were read together and the venue stated fifteen kinds of tick, none of them those. Each says so where it is declared, so a program that implements one still compiles and runs. Everything else is carried.

**And 77 more beyond that list.** The connection a terminal opens carries more than the documented calls describe — what the venue permits this account, which algorithms it offers, the order defaults it fills an order's blanks from, what it says about an issuer, which session holds the account — and a client that speaks that connection can answer them. Most have no call in the documented API at all; a few are one a reference client happens to name too, and the table below marks which is which, under *Beyond the canonical list*.

Every figure here is read from the client it names, on the machine that generated it. A client that is not installed is left out rather than filled in from memory.

<details>
<summary><b>The whole table — every call, every callback, and what the gateway connection carries beyond them</b></summary>


One row per capability, one column per client. This client exists to be
put where a gateway was, so the question is not what it can do but
whether anything you already use is missing — which is a question about
the columns, not about the rows.

**Every row is something the gateway connection carries.** Most of them
the documented API names too, and those are the rows a port has to
match. The last table is the rest: what that connection carries and the
documented API never named — the terminal reads it, so it is on the
wire, and a client that speaks the wire can answer it.

| Mark | Meaning |
| :---: | --- |
| ● | Carried: the call exists and does what it says |
| ◐ | Taken and not applied: the call exists and reports why it cannot be served, rather than failing to exist |
| · | Absent: no such call |

Each column is read from the client it names, on the machine that
generated this page:

- **Gateway wire** — the connection a terminal opens. Every row is something it carries: either the documented API names it, or this client was written after reading it off that connection.
- **TWS API** — the documented surface, as this repository generates it from source. Where this column is empty and the one beside it is not, the connection carries something the documented API never named.
- **ibapi** — IBKR's own Python client, version `9.81.1-1`, imported and enumerated. This is the copy published to PyPI; the version IBKR distributes directly is numbered 10.x and names calls this one predates, so a gap in this column is a gap in the copy that was read and not necessarily in the client you have.
- **ib_async** — version `2.1.0`, imported and enumerated across both the transport and the facade, because it carries some calls on one and some on the other.

A client that is not installed is left out of the table rather than
filled in from memory. A mark here is a thing that was read.

## Calls

What a program asks the venue for.

| Category | Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Connection | `connect` | ● | ● | ● | ● | ● | ● |
|  | `disconnect` | ● | ● | ● | ● | ● | ● |
|  | `is_connected` | ● | ● | ● | ● | ● | ● |
|  | `set_server_log_level` | ● | ● | ● | ● | ● | ● |
|  | `req_current_time` | ● | ● | ● | ● | ● | ● |
|  | `req_current_time_in_millis` | ● | ● | · | · | ● | ● |
| Market Data | `req_mkt_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_mkt_data` | ● | ● | ● | ● | ● | ● |
|  | `req_market_data_type` | ● | ● | ● | ● | ● | ● |
|  | `req_tick_by_tick_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_tick_by_tick_data` | ● | ● | ● | ● | ● | ● |
|  | `req_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `cancel_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `req_mkt_depth_exchanges` | ● | ● | ● | ● | ● | ● |
|  | `req_smart_components` | ● | ● | ● | ● | ● | ● |
|  | `req_real_time_bars` | ● | ● | ● | ● | ● | ● |
|  | `cancel_real_time_bars` | ● | ● | ● | ● | ● | ● |
| Historical Data | `req_historical_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_historical_data` | ● | ● | ● | ● | ● | ● |
|  | `req_head_time_stamp` | ● | ● | ● | ● | ● | ● |
|  | `cancel_head_time_stamp` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_ticks` | ● | ● | ● | ● | ● | ● |
|  | `req_histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_schedule` | ● | ● | · | ● | ● | ● |
| Orders | `place_order` | ● | ● | ● | ● | ● | ● |
|  | `cancel_order` | ● | ● | ● | ● | ● | ● |
|  | `req_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_all_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_auto_open_orders` | ● | ● | ● | ● | ● | ● |
|  | `req_ids` | ● | ● | ● | ● | ● | ● |
|  | `req_global_cancel` | ● | ● | ● | ● | ● | ● |
|  | `req_completed_orders` | ● | ● | ● | ● | ● | ● |
| Executions | `req_executions` | ● | ● | ● | ● | ● | ● |
| Account | `req_account_updates` | ● | ● | ● | ● | ● | ● |
|  | `req_account_summary` | ● | ● | ● | ● | ● | ● |
|  | `cancel_account_summary` | ● | ● | ● | ● | ● | ● |
|  | `req_positions` | ● | ● | ● | ● | ● | ● |
|  | `cancel_positions` | ● | ● | ● | ● | ● | ● |
|  | `req_pnl` | ● | ● | ● | ● | ● | ● |
|  | `cancel_pnl` | ● | ● | ● | ● | ● | ● |
|  | `req_pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `cancel_pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `req_managed_accts` | ● | ● | ● | ● | ● | ● |
|  | `req_account_updates_multi` | ● | ● | ● | ● | ● | ● |
|  | `cancel_account_updates_multi` | ● | ● | ● | ● | ● | ● |
|  | `req_positions_multi` | ● | ● | ● | ● | ● | ● |
|  | `cancel_positions_multi` | ● | ● | ● | ● | ● | ● |
| Contract | `req_contract_details` | ● | ● | ● | ● | ● | ● |
|  | `req_matching_symbols` | ● | ● | ● | ● | ● | ● |
|  | `req_market_rule` | ● | ● | ● | ● | ● | ● |
| Scanner | `req_scanner_parameters` | ● | ● | ● | ● | ● | ● |
|  | `req_scanner_subscription` | ● | ● | ● | ● | ● | ● |
|  | `cancel_scanner_subscription` | ● | ● | ● | ● | ● | ● |
| News | `req_news_providers` | ● | ● | ● | ● | ● | ● |
|  | `req_news_article` | ● | ● | ● | ● | ● | ● |
|  | `req_historical_news` | ● | ● | ● | ● | ● | ● |
|  | `req_news_bulletins` | ● | ● | ● | ● | ● | ● |
|  | `cancel_news_bulletins` | ● | ● | ● | ● | ● | ● |
| Fundamental | `req_fundamental_data` | ● | ● | ● | ● | ● | ● |
|  | `cancel_fundamental_data` | ● | ● | ● | ● | ● | ● |
| Options | `calculate_implied_volatility` | ● | ● | ● | ● | ● | ● |
|  | `cancel_calculate_implied_volatility` | ● | ● | ● | ● | ● | ● |
|  | `calculate_option_price` | ● | ● | ● | ● | ● | ● |
|  | `cancel_calculate_option_price` | ● | ● | ● | ● | ● | ● |
|  | `exercise_options` | ● | ● | ● | ● | ● | ● |
|  | `req_sec_def_opt_params` | ● | ● | ● | ● | ● | ● |
| Reference | `req_soft_dollar_tiers` | ● | ● | ● | ● | ● | ● |
|  | `req_family_codes` | ● | ● | ● | ● | ● | ● |
|  | `req_user_info` | ● | ● | · | ● | ● | ● |
| Financial Advisor | `request_fa` | ● | ● | ● | ● | ● | ● |
|  | `replace_fa` | ● | ● | ● | ● | ● | ● |
| Display Groups | `query_display_groups` | ● | ● | ● | ● | ● | ● |
|  | `subscribe_to_group_events` | ● | ● | ● | ● | ● | ● |
|  | `unsubscribe_from_group_events` | ● | ● | ● | ● | ● | ● |
|  | `update_display_group` | ● | ● | ● | ● | ● | ● |
| WSH | `req_wsh_meta_data` | ● | ● | · | ● | ● | ● |
|  | `req_wsh_event_data` | ● | ● | · | ● | ● | ● |

## Callbacks

What the venue says back. `ib_async` delivers these as events as well as methods, so a mark here says the method exists on its wrapper, not that the information is unavailable by another route.

| Category | Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | --- | :---: | :---: | :---: | :---: | :---: | :---: |
| Connection | `connect_ack` | ● | ● | ● | ● | ● | ● |
|  | `connection_closed` | ● | ● | ● | ● | ● | ● |
|  | `next_valid_id` | ● | ● | ● | ● | ● | ● |
|  | `managed_accounts` | ● | ● | ● | ● | ● | ● |
|  | `error` | ● | ● | ● | ● | ● | ● |
|  | `current_time` | ● | ● | ● | ● | ● | ● |
|  | `current_time_in_millis` | ● | ● | · | · | ● | ● |
| Market Data | `tick_price` | ● | ● | ● | · | ● | ● |
|  | `tick_size` | ● | ● | ● | ● | ● | ● |
|  | `tick_string` | ● | ● | ● | ● | ● | ● |
|  | `tick_generic` | ● | ● | ● | ● | ● | ● |
|  | `tick_snapshot_end` | ● | ● | ● | ● | ● | ● |
|  | `market_data_type` | ● | ● | ● | ● | ● | ● |
|  | `tick_req_params` | ● | ● | ● | ● | ● | ● |
| Orders | `order_status` | ● | ● | ● | ● | ● | ● |
|  | `open_order` | ● | ● | ● | ● | ● | ● |
|  | `open_order_end` | ● | ● | ● | ● | ● | ● |
|  | `order_bound` | ● | ● | ● | ● | ● | ● |
| Executions | `exec_details` | ● | ● | ● | ● | ● | ● |
|  | `exec_details_end` | ● | ● | ● | ● | ● | ● |
|  | `commission_and_fees_report` | ● | ● | ● | ● | ● | ● |
| Account | `update_account_value` | ● | ● | ● | ● | ● | ● |
|  | `update_portfolio` | ● | ● | ● | ● | ● | ● |
|  | `update_account_time` | ● | ● | ● | ● | ● | ● |
|  | `account_download_end` | ● | ● | ● | ● | ● | ● |
|  | `account_summary` | ● | ● | ● | ● | ● | ● |
|  | `account_summary_end` | ● | ● | ● | ● | ● | ● |
|  | `position` | ● | ● | ● | ● | ● | ● |
|  | `position_end` | ● | ● | ● | ● | ● | ● |
|  | `pnl` | ● | ● | ● | ● | ● | ● |
|  | `pnl_single` | ● | ● | ● | ● | ● | ● |
|  | `position_multi` | ● | ● | ● | ● | ● | ● |
|  | `position_multi_end` | ● | ● | ● | ● | ● | ● |
|  | `account_update_multi` | ● | ● | ● | ● | ● | ● |
|  | `account_update_multi_end` | ● | ● | ● | ● | ● | ● |
| Contract | `contract_details` | ● | ● | ● | ● | ● | ● |
|  | `contract_details_end` | ● | ● | ● | ● | ● | ● |
|  | `bond_contract_details` | ● | ● | ● | ● | ● | ● |
|  | `symbol_samples` | ● | ● | ● | ● | ● | ● |
| Historical Data | `historical_data` | ● | ● | ● | ● | ● | ● |
|  | `historical_data_end` | ● | ● | ● | ● | ● | ● |
|  | `historical_data_update` | ● | ● | ● | ● | ● | ● |
|  | `head_timestamp` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks_bid_ask` | ● | ● | ● | ● | ● | ● |
|  | `historical_ticks_last` | ● | ● | ● | ● | ● | ● |
|  | `histogram_data` | ● | ● | ● | ● | ● | ● |
|  | `historical_schedule` | ● | ● | · | ● | ● | ● |
| Market Depth | `update_mkt_depth` | ● | ● | ● | ● | ● | ● |
|  | `update_mkt_depth_l2` | ● | ● | ● | ● | ● | ● |
|  | `mkt_depth_exchanges` | ● | ● | ● | ● | ● | ● |
| Tick-by-Tick | `tick_by_tick_all_last` | ● | ● | ● | ● | ● | ● |
|  | `tick_by_tick_bid_ask` | ● | ● | ● | ● | ● | ● |
|  | `tick_by_tick_mid_point` | ● | ● | ● | ● | ● | ● |
| Scanner | `scanner_data` | ● | ● | ● | ● | ● | ● |
|  | `scanner_data_end` | ● | ● | ● | ● | ● | ● |
|  | `scanner_parameters` | ● | ● | ● | ● | ● | ● |
| News | `news_providers` | ● | ● | ● | ● | ● | ● |
|  | `news_article` | ● | ● | ● | ● | ● | ● |
|  | `historical_news` | ● | ● | ● | ● | ● | ● |
|  | `historical_news_end` | ● | ● | ● | ● | ● | ● |
|  | `tick_news` | ● | ● | ● | ● | ● | ● |
|  | `update_news_bulletin` | ● | ● | ● | ● | ● | ● |
| Real-Time Bars | `real_time_bar` | ● | ● | ● | ● | ● | ● |
| Fundamental | `fundamental_data` | ● | ● | ● | ● | ● | ● |
| Market Rules | `market_rule` | ● | ● | ● | ● | ● | ● |
| Completed Orders | `completed_order` | ● | ● | ● | ● | ● | ● |
|  | `completed_orders_end` | ● | ● | ● | ● | ● | ● |
| Options | `tick_option_computation` | ● | ● | ● | ● | ● | ● |
|  | `security_definition_option_parameter` | ● | ● | ● | ● | ● | ● |
|  | `security_definition_option_parameter_end` | ● | ● | ● | ● | ● | ● |
| Reference | `smart_components` | ● | ● | ● | ● | ● | ● |
|  | `soft_dollar_tiers` | ● | ● | ● | ● | ● | ● |
|  | `family_codes` | ● | ● | ● | ● | ● | ● |
|  | `user_info` | ● | ● | · | ● | ● | ● |
| FA | `receive_fa` | ● | ● | ● | ● | ● | ● |
|  | `replace_fa_end` | ● | ● | ● | · | ● | ● |
| Display Groups | `display_group_list` | ● | ● | ● | · | ● | ● |
|  | `display_group_updated` | ● | ● | ● | · | ● | ● |
| Other | `delta_neutral_validation` | ● | ● | ● | ● | ◐ | ◐ |
| WSH | `wsh_meta_data` | ● | ● | · | ● | ● | ● |
|  | `wsh_event_data` | ● | ● | · | ● | ● | ● |
| Market Data | `reroute_mkt_data_req` | ● | ● | ● | · | ◐ | ◐ |
|  | `reroute_mkt_depth_req` | ● | ● | ● | · | ◐ | ◐ |
|  | `tick_efp` | ● | ● | ● | ● | ◐ | ◐ |
| Connection | `verify_message_api` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_completed` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_and_auth_message_api` | ● | ● | ● | · | ◐ | ◐ |
|  | `verify_and_auth_completed` | ● | ● | ● | · | ◐ | ◐ |
|  | `win_error` | ● | ● | ● | · | ◐ | ◐ |

## Beyond the canonical list

The terminal's own connection carries more than the documented
surface names, and this client speaks that connection — so some of
what it answers has no call in the API at all. These fall into three
kinds, and the table does not try to sort them: things the venue
states that no documented call asks for (what it permits this
account, which algorithms it offers, the order defaults it holds,
what it says about an issuer, which session holds the account); the
same question answered rather than delivered on a callback; and this
client's own instrumentation, which is about the client and not the
venue.

A mark against a reference client here means it happens to name the
same thing, not that the documented API does.

| Call | Gateway wire | TWS API | ibapi | ib_async | ibx Rust | ibx Python |
| --- | :---: | :---: | :---: | :---: | :---: | :---: |
| `account` | ● | · | · | · | ● | · |
| `accountSnapshot` | ● | · | · | · | · | ● |
| `adjustments` | ● | · | · | · | ● | · |
| `algorithms` | ● | · | · | · | ● | · |
| `algorithmsFor` | ● | · | · | · | ● | ● |
| `await_order` | ● | · | · | · | ● | · |
| `calendar_events` | ● | · | · | · | ● | · |
| `calendar_schema` | ● | · | · | · | ● | · |
| `cancel_historical_news` | ● | · | · | · | ● | · |
| `cancelOrderByPermId` | ● | · | · | · | ● | ● |
| `cancelWshEventData` | ● | ● | · | ● | ● | ● |
| `cancelWshMetaData` | ● | ● | · | ● | ● | ● |
| `ccpSessionId` | ● | · | · | · | ● | ● |
| `checkConnected` | ● | · | · | · | · | ● |
| `closingOptionModel` | ● | · | · | · | ● | ● |
| `closingOptionModelByInstrument` | ● | · | · | · | ● | ● |
| `companyData` | ● | · | · | · | ● | ● |
| `companyDataSeries` | ● | · | · | · | ● | ● |
| `competingSession` | ● | · | · | · | · | ● |
| `connect_with_events` | ● | · | · | · | ● | · |
| `contractFigures` | ● | · | · | · | ● | ● |
| `contractFiguresByInstrument` | ● | · | · | · | ● | ● |
| `corporateActions` | ● | · | · | · | ● | ● |
| `enabledFeatures` | ● | · | · | · | ● | ● |
| `eventsLost` | ● | · | · | · | ● | ● |
| `getAccountId` | ● | · | · | · | · | ● |
| `instrument_of` | ● | · | · | · | ● | · |
| `last_rtt` | ● | · | · | · | ● | · |
| `lastRttMs` | ● | · | · | · | · | ● |
| `matchingSymbols` | ● | · | · | · | ● | ● |
| `miscUrl` | ● | · | · | · | ● | ● |
| `newsHeadlines` | ● | · | · | · | ● | ● |
| `nextOrderId` | ● | · | · | · | ● | ● |
| `nextSharedId` | ● | · | · | · | · | ● |
| `numberedFigures` | ● | · | · | · | ● | ● |
| `numberedFiguresSeries` | ● | · | · | · | ● | ● |
| `option_chain` | ● | · | · | · | ● | · |
| `optionChains` | ● | · | · | · | · | ● |
| `optionModel` | ● | · | · | · | ● | ● |
| `optionModelByInstrument` | ● | · | · | · | ● | ● |
| `orderPermissions` | ● | · | · | · | ● | ● |
| `orderPresets` | ● | · | · | · | ● | ● |
| `pairedFigures` | ● | · | · | · | ● | ● |
| `pairedFiguresSeries` | ● | · | · | · | ● | ● |
| `parse_algo_params` | ● | · | · | · | ● | · |
| `permittedOrderTypes` | ● | · | · | · | ● | ● |
| `positions` | ● | ● | · | ● | ● | · |
| `positions_elsewhere` | ● | · | · | · | ● | · |
| `qualifyContract` | ● | · | · | · | ● | ● |
| `qualifyContracts` | ● | ● | · | ● | ● | ● |
| `quote` | ● | · | · | · | ● | · |
| `quoteByInstrument` | ● | · | · | · | ● | ● |
| `reqAdjustments` | ● | · | · | · | ● | ● |
| `reqMktDataEx` | ● | · | · | · | ● | ● |
| `reqPing` | ● | · | · | · | ● | ● |
| `req_spread_scan` | ● | · | · | · | ● | · |
| `scan` | ● | · | · | · | ● | · |
| `scannedStrategies` | ● | · | · | · | ● | ● |
| `schedule` | ● | ● | · | ● | ● | · |
| `serverVersion` | ● | ● | ● | ● | · | ● |
| `session` | ● | · | · | · | ● | · |
| `session_over` | ● | · | · | · | ● | · |
| `session_token_bytes` | ● | · | · | · | ● | · |
| `setConnectOptions` | ● | ● | · | ● | · | ◐ |
| `setNewsProviders` | ● | · | · | · | ● | ● |
| `shared_state` | ● | · | · | · | ● | · |
| `shortSaleRestricted` | ● | · | · | · | ● | ● |
| `shortSaleRestrictedByInstrument` | ● | · | · | · | ● | ● |
| `startApi` | ● | ● | ● | ● | · | ● |
| `statedFigures` | ● | · | · | · | ● | ● |
| `statedFiguresSeries` | ● | · | · | · | ● | ● |
| `statedRows` | ● | · | · | · | ● | ● |
| `tradingSchedule` | ● | · | · | · | · | ● |
| `twsConnectionTime` | ● | ● | ● | · | · | ● |
| `unread_wire` | ● | · | · | · | ● | · |
| `values_elsewhere` | ● | · | · | · | ● | · |
| `what_if_order` | ● | ● | · | ● | ● | · |

## Calls, counted

| Client | Carried | Taken, not applied | Absent |
| --- | ---: | ---: | ---: |
| Gateway wire | 78 | 0 | 0 |
| TWS API | 78 | 0 | 0 |
| ibapi | 73 | 0 | 5 |
| ib_async | 77 | 0 | 1 |
| ibx Rust | 78 | 0 | 0 |
| ibx Python | 78 | 0 | 0 |

</details>

The same table stands on its own in [docs/capabilities.md](docs/capabilities.md), and what each claim rests on is in [docs/evidence.md](docs/evidence.md).

<!-- capabilities:end -->

## What is not covered

Everything this client reads is listed above. This is the other half of that
list, so nobody has to discover it by finding an empty result.

> [!NOTE]
> **Six series are read but have never been seen.** `490`, `546`, `669`, `700`,
> `726` and `733` — the price-distribution weights, the volatility curve, the
> historical ratios, the two screening dashboards, and the option model as of
> the close. Each subscribes cleanly: the venue acknowledges it, assigns it a
> tag, and then states nothing, which is how it answers a series an account is
> not entitled to. Measured the same on a paper and a live login for one
> account, minutes apart. The readers are written and tested; they fill in the
> moment the data flows. The same is true of the spread scan.

> [!NOTE]
> **Some readers have not met their instrument.** The series for bonds,
> municipals, warrants and perpetuals are read from the record's own shape and
> have not been exercised against one of those contracts. Where a reader has
> been run against live data, it says so in its own documentation.

> [!IMPORTANT]
> **An empty result means one of two things** and this client cannot tell them
> apart: the venue holds nothing for that contract, or the account cannot see
> that series. The subscription list in account management is what separates
> them.

Four things the venue declares and this client deliberately does not read:

| | Why |
| --- | --- |
| Three series | Their own readers never touch the payload — one logs that it cannot be served and returns. |
| The dividend series | This client already asks what a contract pays out and is answered with named fields. Reading the same thing again as letter-tagged lines would be worse than what a caller already has. |
| Four numbers | Second numbers for series already read under their first. |
| Nine callbacks | Declared so a program written against the reference client still compiles, and never fired because the venue states nothing for them — no terminal to make a verification handshake with, no socket layer of the reference client's own, no reroute on this connection, and neither an exchange-for-physical quote nor a delta-neutral pairing. |

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

## How it works

There is no process between your program and the venue. The client opens the
connections a session runs on and keeps them:

| Connection | Carries |
| --- | --- |
| Trading | Orders, executions, positions, account values, news bulletins |
| Market data | Quotes, depth, the extra series, tick-by-tick |
| Historical | Bars, historical ticks, head timestamps |
| Security definition | Contract details, option chains, matching symbols |

Each is authenticated at logon, kept alive, and rebuilt on its own if it drops —
a subscription the venue was serving is asked for again, under the same request
the caller made, so a caller does not see the seam.

> [!NOTE]
> Reconnection is per connection, not per session. A market-data connection that
> drops does not take the trading connection with it, and an order in flight is
> not orphaned by a quote feed reconnecting.

**A quote is a value, not a stream of events.** The client keeps the current
state of every contract it watches in a lock-free table and hands you a snapshot
when you ask. Callers who want events get them too — the point is that reading a
quote does not mean draining a queue first.

**Nothing is derived.** Every figure a caller reads was stated by the venue. A
figure the venue holds nothing for comes back as the largest number its field
carries — the venue's own way of saying so — rather than as a zero that reads
like a price.

### Second factor and sessions

> [!IMPORTANT]
> A live login enters the venue's second-factor approval and waits on a device.
> Paper logins do not. **One session per login**: opening a second takes the
> first away, and the venue names the host that took it.

A session can be resumed rather than re-authenticated, which is what makes a
restart cheap. See
[Login](https://userfrm.github.io/ibx/recipes/python/login.html).

## Configuration

The gateway's configuration file is replaced by settings on the client:
announced build, time zone, execution-report scope, and others — 14 in total,
readable at runtime. Ten gateway settings have no counterpart and report why (no
window geometry, no local listening socket, no JVM heap, and no message pacing:
nothing here paces outgoing messages, which the gateway ships with off).

Rust: `EClientConfig.gateway`. Python: `ibx.configure()`.

> [!TIP]
> The calls under *Beyond the canonical list* are the point of this client, not
> a shortfall in it. The venue states all of it on an ordinary session and the
> documented API never gave it a message, so a program had no way to ask — a
> contract's float, where its volatility stands against its own year, what
> margin it takes, five years of its price with the date of each point. They are
> reached here the way every other call is.
>
> The one thing to know is that the door only opens one way: **a program can
> move to this client without changing a line, but a program that then calls one
> of these cannot move back to a gateway**, because a gateway has no message to
> carry it. Nothing else about it is different — the extras are named under
> [Limits](https://userfrm.github.io/ibx/reference/limits.html) so that the
> trade is visible before it is made.

## Documentation

* [The book](https://userfrm.github.io/ibx/) — guides, recipes and the generated API reference
* [Capabilities](docs/capabilities.md) — one row per capability, one column per client
* [Evidence](docs/evidence.md) — what each claim rests on, and the session that produced it
* [Notebooks](notebooks/) — the seven ib_async subjects, in the TWS API shape and in [ib_async's own](notebooks/ib_async_nogateway/)
* [Examples](examples/) — 45 runnable single-file programs, 29 in Rust and 15 in Python
* [Beyond the API](https://userfrm.github.io/ibx/reference/beyond-the-api.html) — what the session states that no documented call asks for
* [Limits](https://userfrm.github.io/ibx/reference/limits.html) — what this client will not do, and why

## Questions

<details>
<summary><b>Do I still need IB Gateway or TWS installed?</b></summary>

No. Nothing is installed, launched or logged into. The `ibapi` package is not
needed either — this client provides that surface itself.
</details>

<details>
<summary><b>Will my existing program work unchanged?</b></summary>

The connect call changes; nothing else has to. The same calls, the same
callbacks, the same order objects. See
[Running an existing program](#running-an-existing-program).
</details>

<details>
<summary><b>Can I run this and a gateway at the same time?</b></summary>

Not on the same login. One session per login — opening a second takes the first
away, and the venue names the host that took it. Use a second login if you need
both at once.
</details>

<details>
<summary><b>I asked for a series and got nothing back. Is it broken?</b></summary>

Probably not. An empty result means one of two things and this client cannot
tell them apart: the venue holds nothing for that contract, or the account is
not entitled to that series. The venue answers a series an account cannot see
with silence rather than a refusal, so there is no error to read. The
subscription list in account management is what separates them. See
[What is not covered](#what-is-not-covered).
</details>

<details>
<summary><b>Is paper different from live?</b></summary>

Not in what arrives. The same wire, the same API, the same entitlements — the
money is what differs. A live login additionally enters the second-factor
approval, which paper does not.
</details>

<details>
<summary><b>What happens when a connection drops?</b></summary>

It is rebuilt on its own and the subscriptions it was serving are asked for
again, under the request the caller made. Connections are independent: a quote
feed reconnecting does not disturb an order in flight.
</details>

<details>
<summary><b>Why does a figure come back as 1.7976931348623157e308?</b></summary>

That is the largest number a double carries, and it is how the venue says it
holds nothing for that field. It is passed through rather than turned into a
zero, because zero is a real price and a real greek.
</details>

<details>
<summary><b>Can I go back to a gateway after using the extra calls?</b></summary>

Moving to this client changes nothing but the connect call. Moving back does, if
your program has come to use a call the documented API never named — a gateway
has no message to carry it. The extras are listed under
[Limits](https://userfrm.github.io/ibx/reference/limits.html) so the trade is
visible before it is made.
</details>

<details>
<summary><b>Rust or Python — is one behind the other?</b></summary>

Neither. The same request produces the same call on both, checked against live
responses, and the capability table is generated by reading each surface rather
than from memory. Where the two differ the count is zero.
</details>

## Testing

Claims here rest on tests, and the tests are counted rather than described:

| Suite | Count | Needs a session |
| --- | ---: | :---: |
| Rust, unit and integration | 2,746 | No |
| Python | 914 | No |
| Rust, live | 9 | Yes |
| Python, live | 137 | Yes |
| Paper compatibility, 154 phases | 51 | Yes |

Every published count is checked against what is actually there, so a number in
this file cannot drift from the suite that produced it. What each claim rests on
is in [Evidence](docs/evidence.md).

Readers of the venue's own records are held to a harder standard than passing:
a test that would pass against a broken reader is not a test. Each is checked by
breaking the reader deliberately and confirming the test fails.

## Contributing

Issues and pull requests are welcome.

> [!TIP]
> The fastest way to be useful is a wire observation: a contract, a request, and
> what the venue answered with. A reader is only as good as the records it has
> been held against, and the ones that have met the fewest live records are
> named in [What is not covered](#what-is-not-covered).

Before opening a pull request, run the same gate the workflow runs. It builds
both surfaces, runs every suite, regenerates the documentation and checks that
what is published matches what is there:

```sh
python scripts/gate.py
```

## Security

> [!CAUTION]
> Credentials are the account. Never commit them, never paste them into an
> issue, and never put them in a file the repository tracks.

Pass them at connect time from the environment or a secret store. A resumed
session is written to disk encrypted with a password you supply and never in the
clear. Nothing in this repository ships credentials, and nothing that fabricates
a session is compiled into the published wheel.

If you find a security problem, please report it privately through the
repository's security advisories rather than in a public issue.

## License

[AGPL-3.0](LICENSE)

## Disclaimer

Interactive Brokers®, IBKR®, Trader Workstation®, and IB Gateway® are
registered trademarks of Interactive Brokers Group, Inc. This project is **not
affiliated with, endorsed by, or supported by Interactive Brokers**.

IBX is an independent, open-source project provided "as is", without warranty
of any kind.

> [!CAUTION]
> This client places orders against a real account. Test against a paper login
> first, and satisfy yourself that an order reads the way you meant it before
> pointing it at money.

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
