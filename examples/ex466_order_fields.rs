//! ibx#466 probe. Paper account only.
//!
//! A far limit order and a far stop order, each with an orderRef; a price
//! modify of the limit; then both are cancelled. Prints the sent frames
//! (6010 orderRef, 15 currency, 6117 stop trigger) and the reports that
//! echo 6010.
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
        let sent = m.starts_with("WIRE>") && (m.contains("|35=D|") || m.contains("|35=G|"));
        let echo = m.starts_with("WIRE< ccp") && m.contains("|35=8|") && m.contains("|6010=");
        if sent || echo {
            let keep: Vec<&str> = m.split('|')
                .filter(|t| ["35=", "11=", "40=", "44=", "99=", "6117=", "6010=", "15=", "150=", "39="].iter().any(|p| t.starts_with(p)))
                .collect();
            println!("[{}] {}", if sent { "sent" } else { "echo" }, keep.join("|"));
        } else if r.level() <= log::Level::Warn && !m.contains("soft dollar") && !m.contains("HMDS") {
            println!("[{}] {}", r.level(), m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { status: Vec<(i64, String)> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn open_order(&mut self, id: i64, _c: &Contract, o: &Order, st: &OrderState) {
        println!("[open_order] {} {} ref='{}' status={}", id, o.order_type, o.order_ref, st.status);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
    }
}

fn has(s: &Arc<Mutex<S>>, id: i64, st: &str) -> bool {
    s.lock().unwrap().status.iter().any(|(i, x)| *i == id && x == st)
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
    let round = |p: f64| (p * 100.0).round() / 100.0;
    pump(&client, &mut w, 3, || false);

    let l = client.next_order_id();
    let lmt = |px: f64| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: px,
        tif: "DAY".into(), order_ref: "ex466-lmt".into(), ..Default::default()
    };
    println!("=== limit {}", l);
    client.place_order(l, &spy, &lmt(round(reference * 0.8))).unwrap();
    pump(&client, &mut w, 15, || has(&s, l, "Submitted"));
    println!("=== modify {}", l);
    client.place_order(l, &spy, &lmt(round(reference * 0.8) - 1.0)).unwrap();
    pump(&client, &mut w, 6, || false);

    let t = client.next_order_id();
    let stop = Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0,
        aux_price: round(reference * 0.8), tif: "DAY".into(), order_ref: "ex466-stp".into(), ..Default::default()
    };
    println!("=== stop {}", t);
    client.place_order(t, &spy, &stop).unwrap();
    pump(&client, &mut w, 15, || has(&s, t, "Submitted") || has(&s, t, "PreSubmitted"));
    pump(&client, &mut w, 2, || false);

    println!("=== cancel both");
    client.cancel_order(l, "").unwrap();
    client.cancel_order(t, "").unwrap();
    pump(&client, &mut w, 15, || has(&s, l, "Cancelled") && has(&s, t, "Cancelled"));
    client.disconnect();
}
