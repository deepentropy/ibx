# Getting Started

## Install

### Rust

```toml
[dependencies]
ibx = { git = "https://github.com/deepentropy/ibx" }
```

### Python

```bash
pip install git+https://github.com/deepentropy/ibx
```

> Python wheels are built from the same Rust core via [PyO3](https://pyo3.rs) / [maturin](https://www.maturin.rs/). You need a working Rust toolchain to install from source.

## Credentials

IBX connects directly to IB servers — there is no separate gateway process. Set your account credentials via environment variables:

```bash
export IB_USERNAME="your_username"
export IB_PASSWORD="your_password"
export IB_HOST="cdc1.ibllc.com"   # paper-trading host
```

> **Never** use a live account for testing. Use the paper account for unit and integration tests. The live account is only for read-only validation (login, contract details).

## Hello, world

### Rust

```rust
use ibx::api::client::{EClient, EClientConfig, Contract};
use ibx::api::wrapper::Wrapper;
use ibx::api::types::TickAttrib;

struct MyWrapper;
impl Wrapper for MyWrapper {
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        println!("tick_price req_id={req_id} type={tick_type} price={price}");
    }
}

fn main() {
    let mut client = EClient::connect(&EClientConfig {
        username: std::env::var("IB_USERNAME").unwrap(),
        password: std::env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(),
        paper: true,
        core_id: None,
    }).unwrap();

    let spy = Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() };
    client.req_mkt_data(1, &spy, "", false, false);

    std::thread::sleep(std::time::Duration::from_secs(10));
}
```

### Python

```python
import os, threading
from ibx import EClient, EWrapper, Contract

class MyWrapper(EWrapper):
    def tick_price(self, req_id, tick_type, price, attrib):
        print(f"tick_price req_id={req_id} type={tick_type} price={price}")

w = MyWrapper()
c = EClient(w)
c.connect(
    username=os.environ["IB_USERNAME"],
    password=os.environ["IB_PASSWORD"],
    host="cdc1.ibllc.com",
    paper=True,
)
threading.Thread(target=c.run, daemon=True).start()

spy = Contract()
spy.con_id, spy.symbol, spy.sec_type = 756733, "SPY", "STK"
spy.exchange, spy.currency = "SMART", "USD"
c.req_mkt_data(1, spy, "", False)
```

## Next steps

- [Streaming L2 Market Depth](./recipes/streaming-l2.md) — full L2 order book for two tickers
- [Order Lifecycle](./recipes/order-lifecycle.md) — place / modify / cancel / fill on a single order
