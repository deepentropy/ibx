//! ibx#597 probe, for regular hours: the order answers around a fill through
//! the Rust client. Paper account only.
//!
//! Buys 1 AAPL with a marketable limit and sells it back, then, timed from
//! the sell's fill: a global cancel at 3, 8, 12, 20, 35 and 65 s, and a
//! `cancel_order` of both filled orders at 5, 10, 14, 24, 40 and 70 s. The
//! reference (12/10/2026): 161 for each filled order until 20 s and nothing
//! at 35 s; 10148 "state: Filled" until 24 s, 10147 from 40 s; the order
//! given again with its commission report shows the commission.
//!
//! Prints every order callback with its time from the start.
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
    bid: f64,
    ask: f64,
    filled: Vec<(i64, f64)>,
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

fn num(v: f64) -> String {
    if v == f64::MAX { "UNSET".into() } else { format!("{v}") }
}

impl Wrapper for Log {
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _attrib: &ibx::api::types::TickAttrib) {
        match tick_type {
            1 => self.bid = price,
            2 => self.ask = price,
            _ => {}
        }
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {}", text.chars().take(90).collect::<String>()));
        }
    }
    fn order_status(&mut self, order_id: i64, status: &str, filled: f64, remaining: f64, _avg: f64, _perm_id: i64,
        parent_id: i64, _last: f64, _client_id: i64, why_held: &str, _cap: f64) {
        self.line(format!("orderStatus {order_id} {status} filled={filled} remaining={remaining} parent={parent_id} why={why_held:?}"));
        if status == "Filled" && !self.filled.iter().any(|(id, _)| *id == order_id) {
            self.filled.push((order_id, self.t0.elapsed().as_secs_f64()));
        }
    }
    fn open_order(&mut self, order_id: i64, _contract: &Contract, order: &Order, state: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} {} comm={} {:?}", order.action, order.order_type, state.status,
            num(state.commission_and_fees), state.commission_and_fees_currency));
    }
    fn exec_details(&mut self, req_id: i64, _contract: &Contract, e: &Execution) {
        self.line(format!("execDetails {req_id} order={} {} {}", e.order_id, e.side, e.shares));
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
    let mut w = Log { t0: Instant::now(), bid: 0.0, ask: 0.0, filled: Vec::new() };
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
    // A quote for the marketable limits, outside the timed part.
    client.req_mkt_data(1, &aapl, "", false, false)?;
    let quote_until = Instant::now() + Duration::from_secs(15);
    while (w.bid <= 0.0 || w.ask <= 0.0) && Instant::now() < quote_until {
        pump(&client, &mut w, 0.1);
    }
    client.cancel_mkt_data(1)?;
    if w.bid <= 0.0 || w.ask <= 0.0 {
        client.disconnect();
        return Err("no quote".into());
    }
    let limit = |action: &str, price: f64| Order {
        action: action.into(), total_quantity: 1.0, order_type: "LMT".into(), tif: "DAY".into(),
        lmt_price: (price * 100.0).round() / 100.0, transmit: true, ..Default::default()
    };
    let buy = client.next_order_id();
    let sell = buy + 1;
    w.line(format!("place BUY {buy} (bid {} ask {})", w.bid, w.ask));
    client.place_order(buy, &aapl, &limit("BUY", w.ask * 1.01))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !w.filled.iter().any(|(id, _)| *id == buy) && Instant::now() < deadline {
        pump(&client, &mut w, 0.05);
    }
    if !w.filled.iter().any(|(id, _)| *id == buy) {
        client.cancel_order(buy, "")?;
        pump(&client, &mut w, 3.0);
        client.disconnect();
        return Err("the buy was not filled in 20 s".into());
    }
    w.line(format!("place SELL {sell}"));
    client.place_order(sell, &aapl, &limit("SELL", w.bid * 0.99))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !w.filled.iter().any(|(id, _)| *id == sell) && Instant::now() < deadline {
        pump(&client, &mut w, 0.05);
    }
    let Some(&(_, ts)) = w.filled.iter().find(|(id, _)| *id == sell) else {
        client.disconnect();
        return Err("WARNING: the sell was not filled in 20 s: check the AAPL position".into());
    };
    let steps = [(3.0, true), (5.0, false), (8.0, true), (10.0, false), (12.0, true), (14.0, false),
        (20.0, true), (24.0, false), (35.0, true), (40.0, false), (65.0, true), (70.0, false)];
    for (off, global) in steps {
        let wait = ts + off - w.t0.elapsed().as_secs_f64();
        pump(&client, &mut w, wait);
        if global {
            w.line(format!("---- reqGlobalCancel at +{off} s"));
            client.req_global_cancel()?;
        } else {
            w.line(format!("---- cancel_order {sell} and {buy} at +{off} s"));
            client.cancel_order(sell, "")?;
            client.cancel_order(buy, "")?;
        }
    }
    pump(&client, &mut w, 3.0);
    client.disconnect();
    Ok(())
}
