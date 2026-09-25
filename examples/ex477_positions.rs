//! ibx#477 probe. Paper account only.
//!
//! req_positions once, then a marketable BUY 1 SPY and a SELL 1 SPY (the
//! position is flat again at the end). Expect the snapshot and one
//! position_end, then position rows for SPY after each fill, with no new
//! req_positions.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY reference price).
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct S { status: Vec<(i64, String)>, ends: usize }
struct W { s: Arc<Mutex<S>>, start: Instant }
impl Wrapper for W {
    fn position(&mut self, _a: &str, c: &Contract, pos: f64, avg: f64) {
        println!("{:6.1}s position {} {} pos={} avg={}", self.start.elapsed().as_secs_f64(), c.con_id, c.symbol, pos, avg);
    }
    fn position_end(&mut self) {
        self.s.lock().unwrap().ends += 1;
        println!("{:6.1}s position_end", self.start.elapsed().as_secs_f64());
    }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn exec_details(&mut self, _r: i64, c: &Contract, e: &ibx::api::types::Execution) {
        println!("{:6.1}s exec {} {} {} @ {}", self.start.elapsed().as_secs_f64(), c.symbol, e.side, e.shares, e.price);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.1}s error {} {} {}", self.start.elapsed().as_secs_f64(), id, code, msg);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(w);
        if done() { return true; }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn main() {
    let reference: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone(), start: Instant::now() };
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };

    println!("=== req_positions");
    client.req_positions(&mut w);
    pump(&client, &mut w, 10, || s.lock().unwrap().ends > 0);

    for (action, factor) in [("BUY", 1.03), ("SELL", 0.97)] {
        let id = client.next_order_id();
        let px = (reference * factor * 100.0).round() / 100.0;
        println!("=== {} 1 SPY at {}", action, px);
        let order = Order {
            action: action.into(), order_type: "LMT".into(), total_quantity: 1.0,
            lmt_price: px, tif: "DAY".into(), outside_rth: true, ..Default::default()
        };
        client.place_order(id, &spy, &order).unwrap();
        pump(&client, &mut w, 30, || s.lock().unwrap().status.iter().any(|(i, st)| *i == id && st == "Filled"));
        pump(&client, &mut w, 5, || false);
    }
    println!("=== position_end count = {}", s.lock().unwrap().ends);
    client.cancel_positions();
    client.disconnect();
}
