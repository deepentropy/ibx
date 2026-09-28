//! ibx#307 probe. Paper account only.
//!
//! GTX and NMIN go out as GTC (59=1) and read back as GTC, as the
//! reference. Limit orders far from the market, cancelled at the end.
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
        if (m.starts_with("WIRE>") && m.contains("|35=D|")) || (m.starts_with("WIRE<") && m.contains("|35=8|") && m.contains("|39=")) {
            let keep: Vec<&str> = m.split('|').filter(|t| ["35=", "11=", "150=", "59=", "6436="].iter().any(|p| t.starts_with(p))).collect();
            println!("[{}] {}", if m.starts_with("WIRE>") { "sent" } else { "recv" }, keep.join("|"));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
    fn open_order(&mut self, id: i64, _c: &Contract, o: &Order, s: &ibx::api::types::OrderState) {
        println!("[open_order] {} {} tif={}", id, s.status, o.tif);
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
    let mut ids = Vec::new();
    for tif in ["GTX", "NMIN"] {
        let id = client.next_order_id();
        println!("=== LMT {} ({})", tif, id);
        client.place_order(id, &aapl, &Order { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
            lmt_price: 200.0, tif: tif.into(), ..Default::default() })?;
        ids.push(id);
        pump(&client, 3);
    }
    for id in ids { client.cancel_order(id, "")?; }
    pump(&client, 4);
    client.disconnect();
    Ok(())
}
