//! ibx#467 probe. Paper account only.
//!
//! A DTC limit order is sent as GTC with the DTC flag, and its replace too.
//! A limit order with a goodAfterTime is sent with the time in UTC. A bad
//! goodAfterTime is refused with 337 and nothing is sent. Orders are far
//! from the market and cancelled at the end.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

static T0: OnceLock<Instant> = OnceLock::new();
fn ms() -> u128 { T0.get_or_init(Instant::now).elapsed().as_millis() }

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        let keep = |m: &str| -> String {
            m.split('|').filter(|t| ["35=", "11=", "39=", "150=", "44=", "59=", "6436=", "168=", "126=", "58="].iter().any(|p| t.starts_with(p)))
                .collect::<Vec<_>>().join("|")
        };
        if m.starts_with("WIRE>") && (m.contains("|35=D|") || m.contains("|35=G|")) {
            println!("{:>6} [sent] {}", ms(), keep(&m));
        } else if m.starts_with("WIRE<") && m.contains("|35=8|") {
            println!("{:>6} [recv] {}", ms(), keep(&m));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:>6} [error] {} {} {}", ms(), id, code, msg.lines().next().unwrap_or(""));
    }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("{:>6} [order_status] {} {}", ms(), id, status);
    }
    fn open_order(&mut self, id: i64, _c: &Contract, o: &Order, s: &ibx::api::types::OrderState) {
        println!("{:>6} [open_order] {} {} tif={} goodAfterTime={:?} lmtPrice={}", ms(), id, s.status, o.tif, o.good_after_time, o.lmt_price);
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
    ms();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, 2);
    let lmt = Order { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 500.0, tif: "DAY".into(), ..Default::default() };

    let dtc_id = client.next_order_id();
    println!("{:>6} === DTC LMT ({})", ms(), dtc_id);
    client.place_order(dtc_id, &spy, &Order { tif: "DTC".into(), ..lmt.clone() }).unwrap();
    pump(&client, 4);
    println!("{:>6} === DTC replace, price 500.10", ms());
    client.place_order(dtc_id, &spy, &Order { tif: "DTC".into(), lmt_price: 500.10, ..lmt.clone() }).unwrap();
    pump(&client, 4);

    let gat_id = client.next_order_id();
    println!("{:>6} === LMT goodAfterTime 20260928 09:30:00 US/Eastern ({})", ms(), gat_id);
    client.place_order(gat_id, &spy, &Order { good_after_time: "20260928 09:30:00 US/Eastern".into(), ..lmt.clone() }).unwrap();
    pump(&client, 4);

    let bad_id = client.next_order_id();
    println!("{:>6} === LMT bad goodAfterTime ({})", ms(), bad_id);
    client.place_order(bad_id, &spy, &Order { good_after_time: "20260928 25:00:00 US/Eastern".into(), ..lmt.clone() }).unwrap();
    pump(&client, 2);

    println!("{:>6} === open orders", ms());
    client.req_open_orders(&mut W);
    for id in [dtc_id, gat_id] { client.cancel_order(id, "").unwrap(); }
    pump(&client, 5);
    client.disconnect();
}
