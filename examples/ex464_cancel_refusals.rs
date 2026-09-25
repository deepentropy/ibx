//! ibx#464 probe. Paper account only.
//!
//! A: cancel an order id nobody placed: expect error 10147, nothing sent.
//! B: far limit order, cancel it twice in a row: the second cancel gives
//!    10148 "state: PendingCancel"; then, once cancelled, a third cancel
//!    gives 10148 "state: Cancelled". No server cancel reject may follow.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY reference price).
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if (m.starts_with("WIRE< ccp") && (m.contains("|35=9|") || m.contains("|35=8|")) && !m.contains("97=Y"))
            || m.starts_with("Cancel of order") {
            let short: String = m.chars().take(160).collect();
            println!("[log] {}", short);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { status: Vec<(i64, String)>, errors: Vec<(i64, i64, String)> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("[order_status] {} {}", id, status);
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
        self.s.lock().unwrap().errors.push((id, code, msg.into()));
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
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let reference: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone() };
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, &mut w, 3, || false);

    println!("\n=== A: cancel an unknown order id");
    let unknown = client.next_order_id() + 500;
    client.cancel_order(unknown, "").unwrap();
    pump(&client, &mut w, 5, || false);

    let b = client.next_order_id();
    let far = (reference * 0.8 * 100.0).round() / 100.0;
    println!("\n=== B: far limit {} at {}", b, far);
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: far, tif: "DAY".into(), outside_rth: true, ..Default::default()
    };
    client.place_order(b, &spy, &order).unwrap();
    pump(&client, &mut w, 15, || s.lock().unwrap().status.iter().any(|(i, st)| *i == b && st == "Submitted"));
    println!("\n--- B: two cancels in a row");
    client.cancel_order(b, "").unwrap();
    client.cancel_order(b, "").unwrap();
    pump(&client, &mut w, 15, || s.lock().unwrap().status.iter().any(|(i, st)| *i == b && st == "Cancelled"));
    pump(&client, &mut w, 3, || false);
    println!("\n--- B: cancel after Cancelled");
    client.cancel_order(b, "").unwrap();
    pump(&client, &mut w, 5, || false);

    let errors = s.lock().unwrap().errors.clone();
    println!("\nerrors: {:?}", errors.iter().map(|(i, c, _)| (*i, *c)).collect::<Vec<_>>());
    client.disconnect();
}
