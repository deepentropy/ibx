//! ibx#479 probe. Paper account only. Read-only: no orders.
//!
//! Two account summary requests (one with `$LEDGER`, one with
//! `$LEDGER:ALL`), a third that must give 322, and one with an empty group
//! that must give 321. Rows come as the server sends them; each batch ends
//! with account_summary_end. Both requests are cancelled at the end.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_SECS (default 30).
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W { start: Instant }
impl Wrapper for W {
    fn account_summary(&mut self, req_id: i64, _a: &str, tag: &str, value: &str, currency: &str) {
        let value = if tag == "AccountCode" { "DU***" } else { value };
        println!("{:6.1}s summary {} {} = {} [{}]", self.start.elapsed().as_secs_f64(), req_id, tag, value, currency);
    }
    fn account_summary_end(&mut self, req_id: i64) {
        println!("{:6.1}s summary_end {}", self.start.elapsed().as_secs_f64(), req_id);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.1}s error {} {} {}", self.start.elapsed().as_secs_f64(), id, code, msg);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(w);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let secs: u64 = env::var("PROBE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let mut w = W { start: Instant::now() };
    client.req_account_summary(9001, "All", "AccountType,NetLiquidation,TotalCashValue,Cushion,$LEDGER");
    client.req_account_summary(9002, "All", "BuyingPower,$LEDGER:ALL");
    client.req_account_summary(9003, "All", "NetLiquidation");
    client.req_account_summary(9004, "", "NetLiquidation");
    pump(&client, &mut w, secs);
    println!("=== cancel");
    client.cancel_account_summary(9001);
    client.cancel_account_summary(9002);
    pump(&client, &mut w, 3);
    client.disconnect();
}
