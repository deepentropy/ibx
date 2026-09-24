//! Order status probe. Paper account only.
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
        let t = r.target();
        if r.level() <= log::Level::Info && t.contains("ccp") {
            println!("[{}] {}", r.level(), m);
        } else if r.level() <= log::Level::Warn {
            println!("[{}] {}", r.level(), m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { st: Vec<(i64, String)> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn order_status(&mut self, id: i64, status: &str, filled: f64, rem: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("[status] {} {} filled={} rem={}", id, status, filled, rem);
        self.s.lock().unwrap().st.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
    }
}

fn main() {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone() };
    let id = client.next_order_id();
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let order = Order { action: "BUY".into(), order_type: env::var("PROBE_TYPE").unwrap_or("MTL".into()), total_quantity: 1.0, ..Default::default() };
    println!("placing {}", id);
    client.place_order(id, &spy, &order).unwrap();
    let mut cancelled = false;
    let end = Instant::now() + Duration::from_secs(25);
    while Instant::now() < end {
        client.process_msgs(&mut w);
        if !cancelled && !s.lock().unwrap().st.is_empty() && env::var("PROBE_NO_CANCEL").is_err() {
            println!("cancelling {}", id);
            client.cancel_order(id, "").unwrap();
            cancelled = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    client.disconnect();
}
