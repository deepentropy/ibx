//! ibx#444 probe: error 10197. Paper account, read-only (market data only).
//!
//! As the reference on 07/10/2026: a streaming request ended by a refused
//! news tick (10094) gets 10197 5 s later, unless a request of its
//! contract runs then; a request with data gets none.
//!
//! Env: IB_USERNAME, IB_PASSWORD; PROBE_FUT_MONTH (default 202612).
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

struct W { start: Instant, ticked: Vec<i64> }
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        if id >= 0 {
            println!("[{:6.2}s error] {} {} {:.60}", self.start.elapsed().as_secs_f64(), id, code, msg);
        }
    }
    fn tick_price(&mut self, req_id: i64, _t: i32, _p: f64, _a: &TickAttrib) {
        if !self.ticked.contains(&req_id) {
            self.ticked.push(req_id);
            println!("[{:6.2}s first tick] {}", self.start.elapsed().as_secs_f64(), req_id);
        }
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(w); std::thread::sleep(Duration::from_millis(10)); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    let mut w = W { start: Instant::now(), ticked: Vec::new() };
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    let mnq = Contract {
        symbol: "MNQ".into(), sec_type: "FUT".into(), exchange: "CME".into(), currency: "USD".into(),
        last_trade_date_or_contract_month: env::var("PROBE_FUT_MONTH").unwrap_or_else(|_| "202612".into()),
        ..Default::default()
    };
    pump(&client, &mut w, 2);

    let mut step = |title: &str, w: &mut W| { w.start = Instant::now(); println!("=== {title}"); };

    step("1: MNQ mdoff,292 alone -> 10094, 10197 at 5 s, 300 on cancel", &mut w);
    client.req_mkt_data(1, &mnq, "mdoff,292", false, false)?;
    pump(&client, &mut w, 8);
    let _ = client.cancel_mkt_data(1);
    pump(&client, &mut w, 1);

    step("2: AAPL mdoff,292:XYZ alone -> 10094, 10197 at 5 s", &mut w);
    client.req_mkt_data(2, &aapl, "mdoff,292:XYZ", false, false)?;
    pump(&client, &mut w, 8);

    step("3: MNQ plain (10), then MNQ mdoff,292 (11) -> 10094, no 10197", &mut w);
    client.req_mkt_data(10, &mnq, "", false, false)?;
    pump(&client, &mut w, 3);
    w.start = Instant::now();
    client.req_mkt_data(11, &mnq, "mdoff,292", false, false)?;
    pump(&client, &mut w, 8);
    let _ = client.cancel_mkt_data(10);
    pump(&client, &mut w, 2);

    step("4: AAPL mdoff,292:XYZ (20), AAPL plain 2 s later (21) -> 10094, no 10197", &mut w);
    client.req_mkt_data(20, &aapl, "mdoff,292:XYZ", false, false)?;
    pump(&client, &mut w, 2);
    client.req_mkt_data(21, &aapl, "", false, false)?;
    pump(&client, &mut w, 7);
    let _ = client.cancel_mkt_data(21);
    pump(&client, &mut w, 1);
    client.disconnect();
    Ok(())
}
