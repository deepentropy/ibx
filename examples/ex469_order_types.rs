//! ibx#469 probe. Paper account only.
//!
//! TRAIL MIT (by percent and by amount) and TRAIL LIT rest with a trigger
//! above the market; each is replaced once, then cancelled. PEG BEST is
//! sent and rejected by the server. RPI is refused with 387 on SPY (no key
//! in its order-type list) and sent on IBM, where the server cancels it.
//! PASSV REL is refused with 387 on both (no list has its key).
//!
//! Env: IB_USERNAME, IB_PASSWORD. Optional: PROBE_REF_PRICE (SPY price; read
//! from the quote when not given).
//! Run with --release: the debug build overflows the main thread's stack
//! after the logon.
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static T0: OnceLock<Instant> = OnceLock::new();
fn ms() -> u128 { T0.get_or_init(Instant::now).elapsed().as_millis() }

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

const SENT: [&str; 14] = ["35=", "11=", "41=", "40=", "44=", "99=", "211=", "6117=", "6370=", "6268=", "6115=", "8339=", "8411=", "8412="];
const RECV: [&str; 10] = ["35=", "11=", "39=", "150=", "40=", "44=", "99=", "6117=", "6370=", "58="];

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        let keep = |m: &str, tags: &[&str]| -> String {
            m.split('|').filter(|t| tags.iter().any(|p| t.starts_with(p))).collect::<Vec<_>>().join("|")
        };
        if m.starts_with("WIRE>") && ["|35=D|", "|35=G|", "|35=F|"].iter().any(|t| m.contains(t)) {
            println!("{:>6} [sent] {}", ms(), keep(&m, &SENT));
        } else if m.starts_with("WIRE<") && m.contains("|35=8|")
            && ["|40=TMIT|", "|40=TLIT|", "|40=E2M|", "|40=RPI|"].iter().any(|t| m.contains(t))
        {
            println!("{:>6} [recv] {}", ms(), keep(&m, &RECV));
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct W { last: f64 }
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        if id >= 0 { println!("{:>6} [error] {} {} {}", ms(), id, code, msg); }
    }
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        if matches!(tick_type, 4 | 9) && price > 0.0 && self.last == 0.0 { self.last = price; }
    }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, w: &str, _m: f64) {
        println!("{:>6} [order_status] {} {} whyHeld={}", ms(), id, status, w);
    }
    fn open_order(&mut self, id: i64, _c: &Contract, o: &Order, s: &ibx::api::types::OrderState) {
        println!("{:>6} [open_order] {} {} {} lmtPrice={} aux={} trailingPercent={} trailStopPrice={} lmtPriceOffset={}",
            ms(), id, s.status, o.order_type, o.lmt_price, o.aux_price, o.trailing_percent, o.trail_stop_price, o.lmt_price_offset);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(w); std::thread::sleep(Duration::from_millis(10)); }
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
    let stock = |con_id: i64, symbol: &str| Contract {
        con_id, symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    };
    let (spy, ibm) = (stock(756733, "SPY"), stock(8314, "IBM"));
    let mut w = W::default();
    pump(&client, &mut w, 2);
    let r: f64 = match env::var("PROBE_REF_PRICE") {
        Ok(v) => v.parse().unwrap(),
        Err(_) => {
            client.req_mkt_data(9001, &spy, "", false, false).unwrap();
            pump(&client, &mut w, 6);
            client.cancel_mkt_data(9001).unwrap();
            w.last
        }
    };
    if r <= 0.0 { client.disconnect(); panic!("no SPY price: give PROBE_REF_PRICE"); }
    println!("{:>6} === SPY reference price {}", ms(), r);
    let cents = |v: f64| (v * 100.0).round() / 100.0;
    // A sell trigger above the market: the order rests.
    let trigger = cents(r + 20.0);
    let sell = Order { action: "SELL".into(), total_quantity: 1.0, tif: "DAY".into(), ..Default::default() };
    let buy = Order { action: "BUY".into(), ..sell.clone() };

    let mut working = Vec::new();
    let pct = Order { order_type: "TRAIL MIT".into(), trailing_percent: 3.0, ..sell.clone() };
    let amount = Order { order_type: "TRAIL MIT".into(), aux_price: 20.0, trail_stop_price: trigger, ..sell.clone() };
    let lit = Order { order_type: "TRAIL LIT".into(), aux_price: 20.0, trail_stop_price: trigger, lmt_price: cents(trigger - 5.0), ..sell.clone() };
    let replaced = [
        ("TRAIL MIT percent", pct.clone(), Order { trailing_percent: 3.1, ..pct }),
        ("TRAIL MIT amount", amount.clone(), Order { aux_price: 20.1, ..amount }),
        ("TRAIL LIT", lit.clone(), Order { lmt_price: cents(trigger - 5.1), ..lit }),
    ];
    for (label, order, replace) in replaced {
        let id = client.next_order_id();
        println!("{:>6} === {} ({})", ms(), label, id);
        client.place_order(id, &spy, &order).unwrap();
        pump(&client, &mut w, 4);
        println!("{:>6} === {} replace", ms(), label);
        client.place_order(id, &spy, &replace).unwrap();
        pump(&client, &mut w, 4);
        working.push(id);
    }
    println!("{:>6} === open orders", ms());
    client.req_open_orders(&mut w);
    pump(&client, &mut w, 3);
    for id in working {
        client.cancel_order(id, "").unwrap();
    }
    pump(&client, &mut w, 4);

    let far = cents(r - 50.0);
    let refused = [
        ("PEG BEST SPY", &spy, Order { order_type: "PEG BEST".into(), lmt_price: far, ..buy.clone() }),
        ("RPI SPY", &spy, Order { order_type: "RPI".into(), lmt_price: far, ..buy.clone() }),
        ("RPI IBM", &ibm, Order { order_type: "RPI".into(), lmt_price: 150.0, ..buy.clone() }),
        ("RPI IBM offset 0.01", &ibm, Order { order_type: "RPI".into(), lmt_price: 150.0, aux_price: 0.01, ..buy.clone() }),
        ("PASSV REL SPY", &spy, Order { order_type: "PASSV REL".into(), lmt_price: far, aux_price: 0.5, ..buy.clone() }),
        ("PASSV REL IBM", &ibm, Order { order_type: "PASSV REL".into(), lmt_price: 150.0, aux_price: 0.5, ..buy.clone() }),
    ];
    for (label, contract, order) in refused {
        let id = client.next_order_id();
        println!("{:>6} === {} ({})", ms(), label, id);
        client.place_order(id, contract, &order).unwrap();
        pump(&client, &mut w, 4);
    }
    client.disconnect();
}
