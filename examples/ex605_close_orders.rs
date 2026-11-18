//! ibx#605 probe through the Rust client, regular hours before the close
//! orders' cut-off: a MOC BUY 1 SPY, then a LOC BUY 1 SPY at 1.00, each held
//! `IBX605_HOLD` seconds (default 75) and cancelled. The server reports
//! nothing about such orders until their cancel; the auth connection must
//! stay up meanwhile. Run with `IBX_LOG_DIR` and `IBX_LOG_LEVEL=info` to
//! read the link's traffic in the engine log.
//!
//! Prints every order callback and error with its time from the start.
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
        copied_waits: env::var("IBX_COPIED_WAITS").is_ok_and(|v| v == "1"),
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let hold: f64 = env::var("IBX605_HOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(75.0);
    let mut w = Log { t0: Instant::now() };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    pump(&client, &mut w, 3.0);
    let contract = Contract {
        symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    };
    let orders = [
        Order { action: "BUY".into(), total_quantity: 1.0, order_type: "MOC".into(), tif: "DAY".into(),
            transmit: true, ..Default::default() },
        Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LOC".into(), tif: "DAY".into(),
            lmt_price: 1.0, transmit: true, ..Default::default() },
    ];
    for order in &orders {
        let id = client.next_order_id();
        w.line(format!("---- place {id} {}", order.order_type));
        client.place_order(id, &contract, order)?;
        pump(&client, &mut w, hold);
        w.line(format!("---- cancel {id}"));
        client.cancel_order(id, "")?;
        pump(&client, &mut w, 5.0);
    }
    client.disconnect();
    Ok(())
}
