//! ibx#476 probe. Paper account only. Read-only: no orders.
//!
//! req_account_updates_multi (all values, and ledger only), a duplicate
//! request id (322), and req_positions_multi. Expect the multi callbacks
//! with the request id and model code, one end per request, and none of the
//! single-account callbacks.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct W { rows: std::collections::BTreeMap<i64, usize>, ends: Vec<String>, single: usize }
impl Wrapper for W {
    fn account_update_multi(&mut self, req_id: i64, account: &str, model: &str, key: &str, value: &str, currency: &str) {
        let n = self.rows.entry(req_id).or_default();
        *n += 1;
        if *n <= 3 || key == "CashBalance" {
            let account = if account.starts_with("DU") { "DU***" } else { account };
            println!("account_update_multi {} {} '{}' {} = {} [{}]", req_id, account, model, key, value, currency);
        }
    }
    fn account_update_multi_end(&mut self, req_id: i64) {
        println!("account_update_multi_end {}", req_id);
        self.ends.push(format!("acct:{req_id}"));
    }
    fn position_multi(&mut self, req_id: i64, _a: &str, model: &str, c: &Contract, pos: f64, avg: f64) {
        *self.rows.entry(req_id).or_default() += 1;
        println!("position_multi {} '{}' {} {} pos={} avg={}", req_id, model, c.con_id, c.symbol, pos, avg);
    }
    fn position_multi_end(&mut self, req_id: i64) {
        println!("position_multi_end {}", req_id);
        self.ends.push(format!("pos:{req_id}"));
    }
    fn update_account_value(&mut self, _k: &str, _v: &str, _c: &str, _a: &str) { self.single += 1; }
    fn account_download_end(&mut self, _a: &str) { self.single += 1; }
    fn position(&mut self, _a: &str, _c: &Contract, _p: f64, _avg: f64) { self.single += 1; }
    fn position_end(&mut self) { self.single += 1; }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("error {} {} {}", id, code, msg);
    }
}

fn main() {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let mut w = W::default();
    client.req_account_updates_multi(9001, "", "", false, &mut w);
    client.req_account_updates_multi(9002, "", "", true, &mut w);
    client.req_account_updates_multi(9001, "", "", false, &mut w);
    client.req_positions_multi(9003, "", "", &mut w);
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(10));
    }
    client.cancel_account_updates_multi(9001);
    client.cancel_account_updates_multi(9002);
    client.cancel_positions_multi(9003);
    println!("=== rows per request {:?}, ends {:?}, single-account callbacks {}", w.rows, w.ends, w.single);
    client.disconnect();
}
