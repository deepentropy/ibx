//! ibx#468 probe. Paper account only.
//!
//! Orders the reference refuses before sending, answered with error 321 and
//! no frame: MIDPRICE with outsideRth, TRAIL LIMIT with both a limit price
//! and a limit price offset, TRAIL LIMIT with neither. A TRAIL LIMIT with
//! the offset only is sent (and cancelled at once).
//!
//! ib-agent#194: TRAIL LIMIT without a stop price is refused. A TRAIL LIMIT
//! with the limit price only is sent with 44 and no 6370; its replace
//! restates 44 and the offset the server reported; openOrder shows that
//! offset.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY price).
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static T0: OnceLock<Instant> = OnceLock::new();
fn ms() -> u128 { T0.get_or_init(Instant::now).elapsed().as_millis() }

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        let keep = |m: &str| -> String {
            m.split('|').filter(|t| ["35=", "11=", "39=", "150=", "40=", "44=", "99=", "6117=", "6370=", "6433="].iter().any(|p| t.starts_with(p)))
                .collect::<Vec<_>>().join("|")
        };
        if m.starts_with("WIRE>") && (m.contains("|35=D|") || m.contains("|35=G|")) {
            println!("{:>6} [sent] {}", ms(), keep(&m));
        } else if m.starts_with("WIRE<") && m.contains("|35=8|") && m.contains("|40=TSL|") {
            println!("{:>6} [recv] {}", ms(), keep(&m));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("{:>6} [error] {} {} {}", ms(), id, code, msg); }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("{:>6} [order_status] {} {}", ms(), id, status);
    }
    fn open_order(&mut self, id: i64, _c: &Contract, o: &Order, s: &ibx::api::types::OrderState) {
        if o.order_type == "TRAIL LIMIT" {
            println!("{:>6} [open_order] {} {} lmtPrice={} lmtPriceOffset={} trailStopPrice={} aux={}",
                ms(), id, s.status, o.lmt_price, o.lmt_price_offset, o.trail_stop_price, o.aux_price);
        }
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
    let base = Order { action: "SELL".into(), total_quantity: 1.0, tif: "DAY".into(), ..Default::default() };
    let cases = [
        ("MIDPRICE outsideRth", Order { order_type: "MIDPRICE".into(), action: "BUY".into(), lmt_price: 600.0, outside_rth: true, ..base.clone() }),
        ("TRAIL LIMIT both", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, trail_stop_price: 600.0, lmt_price: 600.0, lmt_price_offset: 0.5, ..base.clone() }),
        ("TRAIL LIMIT neither", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, trail_stop_price: 600.0, ..base.clone() }),
        ("TRAIL LIMIT no stop price", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, lmt_price_offset: 0.5, ..base.clone() }),
        ("TRAIL LIMIT offset only", Order { order_type: "TRAIL LIMIT".into(), aux_price: 50.0, lmt_price_offset: 0.5, trail_stop_price: 600.0, ..base.clone() }),
    ];
    let mut sent_ok = None;
    for (label, order) in cases {
        let id = client.next_order_id();
        println!("{:>6} === {} ({})", ms(), label, id);
        client.place_order(id, &spy, &order).unwrap();
        pump(&client, 3);
        if label.ends_with("offset only") { sent_ok = Some(id); }
    }
    // ib-agent#194: the limit price only, trail 20 below the reference price.
    let r: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let stop = ((r - 20.0) * 100.0).round() / 100.0;
    let price_only = Order { order_type: "TRAIL LIMIT".into(), aux_price: 20.0, trail_stop_price: stop, lmt_price: stop - 5.0, ..base.clone() };
    let pid = client.next_order_id();
    println!("{:>6} === TRAIL LIMIT price only ({})", ms(), pid);
    client.place_order(pid, &spy, &price_only).unwrap();
    pump(&client, 4);
    println!("{:>6} === open orders", ms());
    client.req_open_orders(&mut W);
    pump(&client, 3);
    println!("{:>6} === replace, limit price +0.10", ms());
    client.place_order(pid, &spy, &Order { lmt_price: stop - 4.9, ..price_only.clone() }).unwrap();
    pump(&client, 4);
    // ibx#490: the replace right after the new order, before any report,
    // computes the offset from the stop price and the new limit price.
    let qid = client.next_order_id();
    println!("{:>6} === TRAIL LIMIT price only, immediate replace +0.10 ({})", ms(), qid);
    client.place_order(qid, &spy, &price_only).unwrap();
    client.place_order(qid, &spy, &Order { lmt_price: stop - 4.9, ..price_only.clone() }).unwrap();
    pump(&client, 4);
    for id in sent_ok.into_iter().chain([pid, qid]) {
        client.cancel_order(id, "").unwrap();
    }
    pump(&client, 5);
    client.disconnect();
}
