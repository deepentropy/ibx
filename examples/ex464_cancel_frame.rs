//! ibx#464 step 2 probe. Paper account only.
//!
//! The cancel is written like the reference: 11 = the order's next ClOrdID
//! version, 41, 38, 54, 1, 6008, 6088=Socket, 6944=SEL.
//! A: far limit order, cancel. B: far limit order, modify the price, cancel
//! (the cancel's version follows the modify's). Each must end Cancelled.
//! The global cancel (6944=ALL) is not run here: it would also cancel the
//! account's other working orders.
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
        let order_frame = m.contains("|35=F|") || m.contains("|35=G|") || m.contains("|35=D|")
            || ((m.contains("|35=8|") || m.contains("|35=9|")) && !m.contains("97=Y") && !m.contains("|11=*|"));
        if (m.starts_with("WIRE>") || m.starts_with("WIRE< ccp")) && order_frame {
            println!("[wire] {}", m);
        } else if r.level() <= log::Level::Warn && !m.contains("soft dollar") {
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
        println!("[order_status] {} {}", id, status);
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
    }
}

fn has(s: &Arc<Mutex<S>>, id: i64, st: &str) -> bool {
    s.lock().unwrap().status.iter().any(|(i, x)| *i == id && x == st)
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
    let far = (reference * 0.8 * 100.0).round() / 100.0;
    let lmt = |price: f64| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: price, tif: "DAY".into(), outside_rth: true, ..Default::default()
    };
    pump(&client, &mut w, 3, || false);

    let a = client.next_order_id();
    println!("\n=== A: far limit {} at {}, cancel", a, far);
    client.place_order(a, &spy, &lmt(far)).unwrap();
    pump(&client, &mut w, 15, || has(&s, a, "Submitted"));
    client.cancel_order(a, "").unwrap();
    let a_ok = pump(&client, &mut w, 15, || has(&s, a, "Cancelled"));
    println!("A: cancelled={}", a_ok);

    let b = client.next_order_id();
    println!("\n=== B: far limit {} at {}, modify, cancel", b, far);
    client.place_order(b, &spy, &lmt(far)).unwrap();
    pump(&client, &mut w, 15, || has(&s, b, "Submitted"));
    client.place_order(b, &spy, &lmt(far - 1.0)).unwrap();
    pump(&client, &mut w, 6, || false);
    client.cancel_order(b, "").unwrap();
    let b_ok = pump(&client, &mut w, 15, || has(&s, b, "Cancelled"));
    println!("B: cancelled={}", b_ok);
    client.disconnect();
}
