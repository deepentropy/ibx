//! ibx#605 probe on paper, read-only: the contract rows and refusals the
//! comparison with the reference 10.51 of 18/10/2026 (market closed) found
//! different.
//!
//! - AAPL: minimum algo size 0, last price and size precisions 0.000001.
//! - MNQ continuous future: on a Sunday the trading hours start with an
//!   empty first day (`;<date>:1700-...`), the liquid hours with `<date>:CLOSED`.
//! - An AAPL option (by conId): two trading sessions a day (07:30-09:25 and
//!   09:30-16:15).
//! - AAPL with an invalid generic tick list holding 258: 321, not 10358;
//!   the 321's legal ticks end with the odd-lot entry.
//!
//! Env: IB_USERNAME, IB_PASSWORD; IBX605_OPTION_CONID (default 933961335).
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::ContractDetails;
use ibx::api::wrapper::Wrapper;

struct W { start: Instant, ends: usize }

/// A decimal value, or "unset".
fn shown(v: f64) -> String {
    if v == f64::MAX { "unset".into() } else { v.to_string() }
}
impl Wrapper for W {
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        let c = &d.contract;
        println!("+{:6} ms  contractDetails {} {} {} {} minAlgoSize={} lastPricePrecision={} lastSizePrecision={}",
            self.start.elapsed().as_millis(), req_id, c.symbol, c.sec_type, c.con_id,
            shown(d.min_algo_size), shown(d.last_price_precision), shown(d.last_size_precision));
        println!("            tradingHours={}", d.trading_hours.chars().take(120).collect::<String>());
        println!("            liquidHours={}", d.liquid_hours.chars().take(120).collect::<String>());
    }
    fn contract_details_end(&mut self, _req_id: i64) { self.ends += 1; }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        if id >= 0 {
            let tail: String = msg.chars().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
            println!("+{:6} ms  error {} {} {:?} ... {:?}", self.start.elapsed().as_millis(), id, code,
                msg.chars().take(90).collect::<String>(), tail);
        }
    }
}

fn pump(client: &EClient, w: &mut W, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end { client.process_msgs(w); std::thread::sleep(Duration::from_millis(5)); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    let mut w = W { start: Instant::now(), ends: 0 };
    pump(&client, &mut w, 2000);
    let option: i64 = env::var("IBX605_OPTION_CONID").ok().and_then(|v| v.parse().ok()).unwrap_or(933961335);
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let mnq = Contract { symbol: "MNQ".into(), sec_type: "CONTFUT".into(), exchange: "CME".into(), currency: "USD".into(), ..Default::default() };
    let opt = Contract { con_id: option, sec_type: "OPT".into(), exchange: "SMART".into(), ..Default::default() };
    w.start = Instant::now();
    client.req_contract_details(1, &aapl)?;
    client.req_contract_details(2, &mnq)?;
    client.req_contract_details(3, &opt)?;
    let end = Instant::now() + Duration::from_secs(20);
    while w.ends < 3 && Instant::now() < end { pump(&client, &mut w, 50); }
    println!("-- AAPL with 162,221,232,258,460,577,586,587,595,614,619,623");
    client.req_mkt_data(9740, &aapl, "162,221,232,258,460,577,586,587,595,614,619,623", false, false)?;
    pump(&client, &mut w, 3000);
    let _ = client.cancel_mkt_data(9740);
    pump(&client, &mut w, 1000);
    client.disconnect();
    Ok(())
}
