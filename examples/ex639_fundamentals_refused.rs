//! ibx#639 probe: fundamentals refused with 10358. Paper account, read-only
//! (market data only).
//!
//! As the reference 10.51 on 12/10/2026 (case D of the #495 script): on a
//! session whose logon does not allow fundamentals to an API client, a
//! market data request with generic tick 258 gets 10358, asked twice; the
//! cancels then get 300. reqFundamentalData gets 10358 too.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

struct W { start: Instant }
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        if id >= 0 {
            println!("+{:6} ms  error {} {} {:?}", self.start.elapsed().as_millis(), id, code, msg);
        }
    }
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _a: &TickAttrib) {
        println!("+{:6} ms  tickPrice {} {} {}", self.start.elapsed().as_millis(), req_id, tick_type, price);
    }
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {
        println!("+{:6} ms  tickString {} {} {:?}", self.start.elapsed().as_millis(), req_id, tick_type, value);
    }
    fn fundamental_data(&mut self, req_id: i64, data: &str) {
        println!("+{:6} ms  fundamentalData {} ({} chars)", self.start.elapsed().as_millis(), req_id, data.len());
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
    let mut w = W { start: Instant::now() };
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, &mut w, 2000);

    println!("=== 1: AAPL \"258\" twice, 6 s apart -> 10358 each; cancels -> 300 each");
    w.start = Instant::now();
    client.req_mkt_data(9504, &aapl, "258", false, false)?;
    pump(&client, &mut w, 6000);
    println!("-- second request");
    client.req_mkt_data(9505, &aapl, "258", false, false)?;
    pump(&client, &mut w, 6000);
    let _ = client.cancel_mkt_data(9504);
    let _ = client.cancel_mkt_data(9505);
    pump(&client, &mut w, 2000);

    println!("=== 2: reqFundamentalData ReportSnapshot -> 10358");
    w.start = Instant::now();
    client.req_fundamental_data(9506, &aapl, "ReportSnapshot")?;
    pump(&client, &mut w, 3000);
    client.disconnect();
    Ok(())
}
