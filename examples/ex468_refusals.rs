//! ibx#468 probe. Paper account only.
//!
//! Orders the reference refuses before sending, answered with error 321 and
//! no frame: MIDPRICE with outsideRth, TRAIL LIMIT with both a limit price
//! and a limit price offset, TRAIL LIMIT with neither. A TRAIL LIMIT with
//! the offset only is sent (and cancelled at once).
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("WIRE>") && m.contains("|35=D|") {
            let keep: Vec<&str> = m.split('|').filter(|t| ["35=", "11=", "40=", "44=", "99=", "6370=", "6433="].iter().any(|p| t.starts_with(p))).collect();
            println!("[sent] {}", keep.join("|"));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("[order_status] {} {}", id, status);
    }
}

fn pump(client: &EClient, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(&mut W); std::thread::sleep(Duration::from_millis(10)); }
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
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, 2);
    let base = Order { action: "SELL".into(), total_quantity: 1.0, tif: "DAY".into(), ..Default::default() };
    let cases = [
        ("MIDPRICE outsideRth", Order { order_type: "MIDPRICE".into(), action: "BUY".into(), lmt_price: 600.0, outside_rth: true, ..base.clone() }),
        ("TRAIL LIMIT both", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, lmt_price: 600.0, lmt_price_offset: 0.5, ..base.clone() }),
        ("TRAIL LIMIT neither", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, ..base.clone() }),
        ("TRAIL LIMIT offset only", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, lmt_price_offset: 0.5, ..base.clone() }),
    ];
    let mut sent_ok = None;
    for (label, order) in cases {
        let id = client.next_order_id();
        println!("=== {} ({})", label, id);
        client.place_order(id, &spy, &order).unwrap();
        pump(&client, 3);
        if label.ends_with("offset only") { sent_ok = Some(id); }
    }
    if let Some(id) = sent_ok {
        client.cancel_order(id, "").unwrap();
        pump(&client, 5);
    }
    client.disconnect();
}
