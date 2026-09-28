//! ibx#465 probe: warning 399 on an order placed before the open. Paper
//! account only, US pre-market.
//!
//! A LOO order (LMT, TIF OPG) below the market (inside the price cap) gets the server's order
//! message (6360=TIME, 6361=...), which ibx reports as warning 399; the order
//! stays PreSubmitted. It is cancelled at the end.
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
        let keep = |m: &str| m.split('|').filter(|t| ["35=", "11=", "39=", "150=", "40=", "44=", "59=", "6360=", "6361="]
            .iter().any(|p| t.starts_with(p))).collect::<Vec<_>>().join("|");
        if m.starts_with("WIRE>") && m.contains("|35=D|") {
            println!("[sent] {}", keep(&m));
        } else if m.starts_with("WIRE<") && m.contains("|35=8|") {
            println!("[recv] {}", keep(&m));
            for t in m.split('|').filter(|t| t.starts_with("6360=") || t.starts_with("6361=")) {
                println!("       {}", t);
            }
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, 2);
    let id = client.next_order_id();
    println!("=== LOO BUY 1 AAPL 320 ({})", id);
    client.place_order(id, &aapl, &Order { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: 320.0, tif: "OPG".into(), ..Default::default() })?;
    pump(&client, 6);
    println!("=== cancel");
    client.cancel_order(id, "")?;
    pump(&client, 4);
    client.disconnect();
    Ok(())
}
