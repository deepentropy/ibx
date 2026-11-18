//! ibx#706 and ibx#709 probe through the Rust client, any session where an
//! order rests: the records an order needs (its own exchange's and the
//! primary exchange's), asked by conId when the session has none, and the
//! clearing rule asked after an answer naming another exchange. Paper
//! account only.
//!
//! Far limit BUY 1 AAPL at 100, given by symbol, each cancelled 2 s after
//! it was placed. By default: twice on SMART (the primary's record asked
//! before the first only), twice directed to ISLAND (asked each time: the
//! answer names NASDAQ; then the clearing rule), then directed to NASDAQ
//! (nothing asked). `IBX706_EXCHANGES` sets the list, e.g. ARCA,NYSE,ISLAND
//! (ARCA: the primary NASDAQ asked; NYSE: nothing). The directed orders are
//! discarded by the redirect precaution unless it is bypassed. Run with `IBX_LOG_DIR`
//! and `IBX_LOG_LEVEL=info` to read the lookups in the engine log.
//!
//! The records are asked only with the copied waits setting
//! (`IBX_COPIED_WAITS=1`, ibx#564); without it the orders go at once.
//!
//! Prints every order callback with its time from the start.
//! It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, Order, OrderState};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

impl Wrapper for Log {
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, state: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} {}", contract.exchange, order.order_type, state.status));
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64, _avg: f64, _perm: i64, _parent: i64,
        _last: f64, _client: i64, _why: &str, _mkt_cap: f64,
    ) {
        self.line(format!("orderStatus {order_id} {status} {filled} {remaining}"));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _json: &str) {
        self.line(format!("error {req_id} {code} {msg}"));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        // The records are asked with the copied waits only (ibx#564).
        copied_waits: env::var("IBX_COPIED_WAITS").is_ok_and(|v| v == "1"),
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let mut w = Log { t0: Instant::now() };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    pump(&client, &mut w, 3.0);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), tif: "DAY".into(),
        lmt_price: 100.0, transmit: true, ..Default::default()
    };
    let exchanges = env::var("IBX706_EXCHANGES").unwrap_or_else(|_| "SMART,SMART,ISLAND,ISLAND,NASDAQ".into());
    for exchange in exchanges.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let contract = Contract {
            symbol: "AAPL".into(), sec_type: "STK".into(), exchange: exchange.into(), currency: "USD".into(),
            ..Default::default()
        };
        let id = client.next_order_id();
        w.line(format!("---- place {id} on {exchange}"));
        client.place_order(id, &contract, &order)?;
        pump(&client, &mut w, 2.0);
        w.line(format!("---- cancel {id}"));
        client.cancel_order(id, "")?;
        pump(&client, &mut w, 2.0);
    }
    client.disconnect();
    Ok(())
}
