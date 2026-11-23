//! ibx#765 check, for regular hours: the last fill of a scale order through
//! the Rust client. Paper account only.
//!
//! Buys 3 AAPL as a scale order (levels of 1, increment 0.10, price
//! adjustment 0.05 every 10 s) at a marketable limit, waits for it to fill,
//! then sells the 3 shares back at market. The server reports the order
//! Filled before the report of its last print; the reference gives that
//! print's execDetails between the first Filled status and the two that
//! follow (23/11/2026). Prints every order callback with its time; ends with
//! the count of execDetails of the scale order (3 expected).
//!
//! It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{CommissionAndFeesReport, Contract, Execution, Order, OrderState};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
    ask: f64,
    filled: Vec<i64>,
    execs: Vec<i64>,
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

impl Wrapper for Log {
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _attrib: &ibx::api::types::TickAttrib) {
        if tick_type == 2 {
            self.ask = price;
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
        if status == "Filled" && !self.filled.contains(&order_id) {
            self.filled.push(order_id);
        }
    }
    fn open_order(&mut self, order_id: i64, _contract: &Contract, order: &Order, state: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} lmt={} {}", order.action, order.order_type, order.lmt_price, state.status));
    }
    fn exec_details(&mut self, req_id: i64, _contract: &Contract, e: &Execution) {
        self.line(format!("execDetails {req_id} order={} {} {} @ {}", e.order_id, e.side, e.shares, e.price));
        self.execs.push(e.order_id);
    }
    fn commission_and_fees_report(&mut self, r: &CommissionAndFeesReport) {
        self.line(format!("commissionReport {} {}", r.commission_and_fees, r.currency));
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
    let mut w = Log { t0: Instant::now(), ask: 0.0, filled: Vec::new(), execs: Vec::new() };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let aapl = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };
    client.req_mkt_data(1, &aapl, "", false, false)?;
    let quote_until = Instant::now() + Duration::from_secs(15);
    while w.ask <= 0.0 && Instant::now() < quote_until {
        pump(&client, &mut w, 0.1);
    }
    client.cancel_mkt_data(1)?;
    if w.ask <= 0.0 {
        client.disconnect();
        return Err("no quote".into());
    }
    let scale = client.next_order_id();
    let order = Order {
        action: "BUY".into(), total_quantity: 3.0, order_type: "LMT".into(), tif: "DAY".into(),
        lmt_price: ((w.ask + 1.0) * 100.0).round() / 100.0, transmit: true,
        scale_init_level_size: 1, scale_subs_level_size: 1, scale_price_increment: 0.1,
        scale_price_adjust_value: 0.05, scale_price_adjust_interval: 10,
        ..Default::default()
    };
    w.line(format!("place scale BUY {scale} (ask {})", w.ask));
    client.place_order(scale, &aapl, &order)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while !w.filled.contains(&scale) && Instant::now() < deadline {
        pump(&client, &mut w, 0.05);
    }
    pump(&client, &mut w, 3.0);
    let filled = w.filled.contains(&scale);
    if !filled {
        client.cancel_order(scale, "")?;
        pump(&client, &mut w, 3.0);
    }
    let execs = w.execs.iter().filter(|id| **id == scale).count();
    w.line(format!("RESULT scale order {scale}: filled={filled} execDetails={execs}"));
    // Put the shares back: sell what the scale order bought.
    if execs > 0 {
        let back = client.next_order_id();
        let sell = Order {
            action: "SELL".into(), total_quantity: execs as f64, order_type: "MKT".into(), tif: "DAY".into(),
            transmit: true, ..Default::default()
        };
        client.place_order(back, &aapl, &sell)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !w.filled.contains(&back) && Instant::now() < deadline {
            pump(&client, &mut w, 0.05);
        }
        if !w.filled.contains(&back) {
            w.line("WARNING: the sell back was not filled: check the AAPL position".into());
        }
        pump(&client, &mut w, 2.0);
    }
    client.disconnect();
    Ok(())
}
