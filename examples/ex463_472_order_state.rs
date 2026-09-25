//! ibx#463 / ibx#472 probe. Paper account only.
//!
//! A: far limit, user cancel, then a modify of the cancelled id (expect 104, nothing sent).
//! B: far IOC limit cancelled by the server (expect 39=D PendingCancel, then 39=4 Cancelled).
//! C: marketable limit that fills, a modify of the filled id (expect 104, no new order),
//!    then a sell to flatten.
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
        if m.starts_with("WIRE< ccp") && m.contains("|35=8|") {
            println!("[wire] {}", m);
        } else if m.starts_with("ExecReport") || m.contains("refused") || r.level() <= log::Level::Warn {
            println!("[{}] {}", r.level(), m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { status: Vec<(i64, String)>, errors: Vec<(i64, i64)>, fills: Vec<i64> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn order_status(&mut self, id: i64, status: &str, filled: f64, rem: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("[status] {} {} filled={} rem={}", id, status, filled, rem);
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
        self.s.lock().unwrap().errors.push((id, code));
    }
    fn exec_details(&mut self, _r: i64, _c: &Contract, e: &ibx::api::types::Execution) {
        println!("[exec] {} {} {} @ {}", e.order_id, e.side, e.shares, e.price);
        self.s.lock().unwrap().fills.push(e.order_id);
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

fn lmt(action: &str, price: f64, tif: &str) -> Order {
    Order {
        action: action.into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: price, tif: tif.into(), outside_rth: true, ..Default::default()
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
    let far = (reference * 0.8 * 100.0).round() / 100.0;
    let working = |st: Option<String>| matches!(st.as_deref(), Some("Submitted" | "PreSubmitted"));

    // A: user cancel, then a modify of the cancelled id.
    let a = client.next_order_id();
    println!("\n=== A: far limit {} at {}, user cancel", a, far);
    client.place_order(a, &spy, &lmt("BUY", far, "DAY")).unwrap();
    pump(&client, &mut w, 15, || working(last_status(&s, a)));
    client.cancel_order(a, "").unwrap();
    let a_cancelled = pump(&client, &mut w, 20, || last_status(&s, a).as_deref() == Some("Cancelled"));
    println!("A: cancelled={}", a_cancelled);
    client.place_order(a, &spy, &lmt("BUY", far - 1.0, "DAY")).unwrap();
    pump(&client, &mut w, 5, || false);
    let a_104 = s.lock().unwrap().errors.contains(&(a, 104));
    println!("A: modify of the cancelled order gave 104={}", a_104);

    // B: IOC cancelled by the server.
    let b = client.next_order_id();
    println!("\n=== B: far IOC limit {} at {}", b, far);
    client.place_order(b, &spy, &lmt("BUY", far, "IOC")).unwrap();
    let b_done = pump(&client, &mut w, 20, || last_status(&s, b).as_deref() == Some("Cancelled"));
    println!("B: cancelled={}", b_done);

    // C: fill, modify of the filled id, then flatten.
    let c = client.next_order_id();
    let buy_px = (reference * 1.03 * 100.0).round() / 100.0;
    println!("\n=== C: marketable limit {} at {}", c, buy_px);
    client.place_order(c, &spy, &lmt("BUY", buy_px, "DAY")).unwrap();
    let c_filled = pump(&client, &mut w, 30, || last_status(&s, c).as_deref() == Some("Filled"));
    println!("C: filled={}", c_filled);
    if c_filled {
        client.place_order(c, &spy, &lmt("BUY", buy_px - 1.0, "DAY")).unwrap();
        pump(&client, &mut w, 8, || false);
        let c_104 = s.lock().unwrap().errors.contains(&(c, 104));
        let fills_c = s.lock().unwrap().fills.iter().filter(|&&i| i == c).count();
        println!("C: modify of the filled order gave 104={} fills for {}={}", c_104, c, fills_c);

        let f = client.next_order_id();
        let sell_px = (reference * 0.97 * 100.0).round() / 100.0;
        println!("\n=== flatten: sell {} at {}", f, sell_px);
        client.place_order(f, &spy, &lmt("SELL", sell_px, "DAY")).unwrap();
        let flat = pump(&client, &mut w, 30, || last_status(&s, f).as_deref() == Some("Filled"));
        println!("flatten: filled={}", flat);
    } else {
        client.cancel_order(c, "").unwrap();
        pump(&client, &mut w, 10, || last_status(&s, c).as_deref() == Some("Cancelled"));
    }
    client.disconnect();
}
