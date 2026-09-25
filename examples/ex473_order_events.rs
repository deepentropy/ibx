//! ibx#473 probe. Paper account only.
//!
//! A: far limit order, modify its price, then cancel. Expect open_order then
//!    order_status for the ack, the routing report and the modify confirm
//!    (same status), and order_status only for the cancel.
//! B: marketable limit that fills, then a sell to flatten. Expect open_order
//!    before order_status on the fill, with lastFillPrice.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY reference price).
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::OrderState;
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("ExecReport") || r.level() <= log::Level::Warn {
            println!("[{}] {}", r.level(), m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { status: Vec<(i64, String)> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn open_order(&mut self, id: i64, c: &Contract, o: &Order, st: &OrderState) {
        println!("[open_order] {} {} {} {} {} @ {} status={} ref='{}'", id, c.symbol, o.action, o.total_quantity, o.order_type, o.lmt_price, st.status, o.order_ref);
    }
    fn order_status(&mut self, id: i64, status: &str, filled: f64, rem: f64, avg: f64, _p: i64, _pa: i64, last: f64, client: i64, _w: &str, _m: f64) {
        println!("[order_status] {} {} filled={} rem={} avg={} last={} client={}", id, status, filled, rem, avg, last, client);
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
    }
}

fn last_status(s: &Arc<Mutex<S>>, id: i64) -> Option<String> {
    s.lock().unwrap().status.iter().rev().find(|(i, _)| *i == id).map(|(_, st)| st.clone())
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

fn lmt(action: &str, price: f64, order_ref: &str) -> Order {
    Order {
        action: action.into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: price, tif: "DAY".into(), outside_rth: true, order_ref: order_ref.into(), ..Default::default()
    }
}

fn main() {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    let reference: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone() };
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let round = |p: f64| (p * 100.0).round() / 100.0;
    pump(&client, &mut w, 3, || false);

    let a = client.next_order_id();
    let far = round(reference * 0.8);
    println!("\n=== A: far limit {} at {}", a, far);
    client.place_order(a, &spy, &lmt("BUY", far, "ex473-a")).unwrap();
    pump(&client, &mut w, 15, || last_status(&s, a).as_deref() == Some("Submitted"));
    pump(&client, &mut w, 2, || false);
    println!("\n--- A: modify price to {}", far - 1.0);
    client.place_order(a, &spy, &lmt("BUY", far - 1.0, "ex473-a")).unwrap();
    pump(&client, &mut w, 8, || false);
    println!("\n--- A: cancel");
    client.cancel_order(a, "").unwrap();
    pump(&client, &mut w, 15, || last_status(&s, a).as_deref() == Some("Cancelled"));

    let b = client.next_order_id();
    let buy = round(reference * 1.03);
    println!("\n=== B: marketable limit {} at {}", b, buy);
    client.place_order(b, &spy, &lmt("BUY", buy, "ex473-b")).unwrap();
    let filled = pump(&client, &mut w, 30, || last_status(&s, b).as_deref() == Some("Filled"));
    pump(&client, &mut w, 3, || false);
    if filled {
        let f = client.next_order_id();
        let sell = round(reference * 0.97);
        println!("\n=== flatten: sell {} at {}", f, sell);
        client.place_order(f, &spy, &lmt("SELL", sell, "ex473-flat")).unwrap();
        pump(&client, &mut w, 30, || last_status(&s, f).as_deref() == Some("Filled"));
        pump(&client, &mut w, 3, || false);
    } else {
        client.cancel_order(b, "").unwrap();
        pump(&client, &mut w, 10, || last_status(&s, b).as_deref() == Some("Cancelled"));
    }
    client.disconnect();
}
