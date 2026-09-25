//! ibx#478 probe. Paper account only.
//!
//! Two req_pnl (both must get updates), a duplicate request id (102), an
//! empty account (321), a cancel of an unknown id (10185 / 10186), and
//! req_pnl_single for SPY and AAPL. Then a marketable BUY 1 SPY and SELL 1
//! SPY: the SPY realized P&L must move after the sell. Position flat at the
//! end.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY reference price).
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

fn show(v: f64) -> String {
    if v == f64::MAX { "MAX".into() } else { format!("{:.4}", v) }
}

#[derive(Default)]
struct S { status: Vec<(i64, String)>, pnl_count: std::collections::BTreeMap<i64, usize> }
struct W { s: Arc<Mutex<S>>, start: Instant }
impl Wrapper for W {
    fn pnl(&mut self, req_id: i64, daily: f64, unrealized: f64, realized: f64) {
        let mut s = self.s.lock().unwrap();
        let n = s.pnl_count.entry(req_id).or_default();
        *n += 1;
        if *n <= 2 || (req_id == 1 && env::var("PROBE_ALL_PNL").is_ok()) {
            println!("{:6.1}s pnl {} daily={} unrealized={} realized={}", self.start.elapsed().as_secs_f64(), req_id, show(daily), show(unrealized), show(realized));
        }
    }
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily: f64, unrealized: f64, realized: f64, value: f64) {
        println!("{:6.1}s pnl_single {} pos={} daily={} unrealized={} realized={} value={}",
            self.start.elapsed().as_secs_f64(), req_id, pos, show(daily), show(unrealized), show(realized), show(value));
    }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn commission_and_fees_report(&mut self, r: &ibx::api::types::CommissionAndFeesReport) {
        println!("{:6.1}s commission {} realized={}", self.start.elapsed().as_secs_f64(), r.exec_id, show(r.realized_pnl));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.1}s error {} {} {}", self.start.elapsed().as_secs_f64(), id, code, msg);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(w);
        if done() { return; }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let reference: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let acct = client.account_id.clone();
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone(), start: Instant::now() };
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };

    // Quotes for SPY: the P&L uses the last price and the previous close.
    if env::var("PROBE_MKT_DATA").is_ok() {
        client.req_mkt_data(10, &spy, "", false, false).unwrap();
        pump(&client, &mut w, 5, || false);
    }
    client.req_pnl(1, &acct, "");
    client.req_pnl(2, &acct, "");
    client.req_pnl(1, &acct, "");
    client.req_pnl(3, "", "");
    client.cancel_pnl(99);
    client.cancel_pnl_single(98);
    client.req_pnl_single(4, &acct, "", 756733);
    client.req_pnl_single(5, &acct, "", 265598);
    pump(&client, &mut w, 10, || false);

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
        pump(&client, &mut w, 6, || false);
    }
    println!("=== pnl callbacks per request {:?}", s.lock().unwrap().pnl_count);
    client.cancel_pnl(1);
    client.cancel_pnl(2);
    client.cancel_pnl_single(4);
    client.cancel_pnl_single(5);
    client.disconnect();
}
