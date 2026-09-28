//! ibx#447 / ibx#444 probe. Paper account, read-only (market data only).
//!
//! reqMarketDataType(3), then reqMktData on AAPL (live data allowed) and on
//! 7203 (TSEJ, no live data on paper): 7203 is rejected by the server and
//! goes on with delayed data (marketDataType 3, error 10167). Then
//! reqMarketDataType(1) and 7203 again: the reject stops it (354 / 10089).
//! Also a duplicate request id (322) and a cancel of an unknown id (300).
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::collections::BTreeMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("Sent 35=V subscribe") || m.starts_with("Farm market data reject") {
            println!("[log] {}", m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct W { ticks: Arc<Mutex<BTreeMap<(i64, i32), usize>>> }
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
    fn market_data_type(&mut self, req_id: i64, t: i32) { println!("[market_data_type] {} {}", req_id, t); }
    fn tick_price(&mut self, req_id: i64, tick_type: i32, _p: f64, _a: &TickAttrib) {
        *self.ticks.lock().unwrap().entry((req_id, tick_type)).or_default() += 1;
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(w); std::thread::sleep(Duration::from_millis(10)); }
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
    let mut w = W::default();
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let toyota = Contract { con_id: 13905804, symbol: "7203".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "JPY".into(), ..Default::default() };
    pump(&client, &mut w, 2);

    println!("=== reqMarketDataType(3); AAPL (9001) and 7203 (9003)");
    client.req_market_data_type(3);
    client.req_mkt_data(9001, &aapl, "", false, false)?;
    client.req_mkt_data(9003, &toyota, "", false, false)?;
    pump(&client, &mut w, 8);
    println!("=== duplicate id 9001, cancel of unknown 555");
    client.req_mkt_data(9001, &aapl, "", false, false)?;
    client.cancel_mkt_data(555)?;
    pump(&client, &mut w, 1);
    client.cancel_mkt_data(9001)?;
    client.cancel_mkt_data(9003)?;
    pump(&client, &mut w, 2);

    println!("=== reqMarketDataType(1); 7203 (9004)");
    client.req_market_data_type(1);
    client.req_mkt_data(9004, &toyota, "", false, false)?;
    pump(&client, &mut w, 6);
    println!("=== tick counts (req, tick type) = n: {:?}", w.ticks.lock().unwrap());
    client.disconnect();
    Ok(())
}
