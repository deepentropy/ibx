//! ibx#609 probe through the Rust client, paper account only: BUY 1 AAPL
//! LMT 100 (far from the market) with attached orders, as cases E, F and G
//! of the reference run of 16/10/2026: both legs from the preset; the
//! profit-taker under the parent's id; a profit-taker type without an id.
//! Prints the errors and order statuses of each case. Nothing can fill.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
}

impl Log {
    fn line(&self, text: String) {
        println!("   +{:6} ms  {text}", self.t0.elapsed().as_millis());
    }
}

impl Wrapper for Log {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {text}"));
        }
    }
    fn order_status(&mut self, order_id: i64, status: &str, _: f64, _: f64, _: f64, perm_id: i64, parent_id: i64,
        _: f64, _: i64, _: &str, _: f64) {
        self.line(format!("orderStatus {order_id} {status} perm={} parent={parent_id}", perm_id != 0));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 198,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let mut w = Log { t0: Instant::now() };
    pump(&client, &mut w, 2.0);
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let none = i32::MAX;
    for (case, sl, pt) in [("E", (1, "PRESET"), (2, "PRESET")), ("F", (1, "preset"), (0, "PRESET")), ("G", (none, ""), (none, "PRESET"))] {
        let oid = client.next_order_id();
        let id = |offset: i32| if offset == none { none } else { oid as i32 + offset };
        let order = Order {
            action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 100.0, tif: "DAY".into(),
            sl_order_id: id(sl.0), sl_order_type: sl.1.into(), pt_order_id: id(pt.0), pt_order_type: pt.1.into(),
            ..Default::default()
        };
        w.t0 = Instant::now();
        println!("== case {case}, order {oid}");
        client.place_order(oid, &aapl, &order)?;
        pump(&client, &mut w, 3.0);
        let _ = client.cancel_order(oid, "");
        pump(&client, &mut w, 1.5);
    }
    client.disconnect();
    Ok(())
}
