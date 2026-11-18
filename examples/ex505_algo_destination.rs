//! ibx#505 probe through the Rust client, pre-market or regular hours: an
//! order for an algo provider's destination (FOXRIVER). Paper account only.
//!
//! A far limit BUY 1 AAPL for FOXRIVER: the reference holds it as a SMART
//! order (`open_order` exchange SMART), refuses a modify that names
//! FOXRIVER with 105, accepts the cancel.
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
    bid: f64,
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

impl Wrapper for Log {
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _attrib: &ibx::api::types::TickAttrib) {
        if tick_type == 1 {
            self.bid = price;
        }
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {}", text.chars().take(90).collect::<String>()));
        }
    }
    fn order_status(&mut self, order_id: i64, status: &str, filled: f64, remaining: f64, _avg: f64, _perm_id: i64,
        _parent_id: i64, _last: f64, _client_id: i64, _why_held: &str, _cap: f64) {
        self.line(format!("orderStatus {order_id} {status} filled={filled} remaining={remaining}"));
    }
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, state: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} {} exchange={} lmt={}", order.action, order.order_type,
            state.status, contract.exchange, order.lmt_price));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let mut w = Log { t0: Instant::now(), bid: 0.0 };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let smart = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };
    client.req_mkt_data(1, &smart, "", false, false)?;
    let until = Instant::now() + Duration::from_secs(15);
    while w.bid <= 0.0 && Instant::now() < until {
        pump(&client, &mut w, 0.1);
    }
    client.cancel_mkt_data(1)?;
    if w.bid <= 0.0 {
        client.disconnect();
        return Err("no quote".into());
    }
    let fox = Contract { exchange: "FOXRIVER".into(), ..smart.clone() };
    let limit = |price: f64| Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), tif: "DAY".into(),
        lmt_price: (price * 100.0).round() / 100.0, transmit: true, ..Default::default()
    };
    let id = client.next_order_id();
    let px = w.bid * 0.8;
    w.line(format!("---- LMT for FOXRIVER {id} at {px:.2}"));
    client.place_order(id, &fox, &limit(px))?;
    pump(&client, &mut w, 5.0);
    w.line(format!("---- modify {id} naming FOXRIVER"));
    client.place_order(id, &fox, &limit(px - 0.5))?;
    pump(&client, &mut w, 4.0);
    w.line(format!("---- cancel {id}"));
    client.cancel_order(id, "")?;
    pump(&client, &mut w, 4.0);
    client.disconnect();
    Ok(())
}
