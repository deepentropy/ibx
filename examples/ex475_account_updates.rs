//! ibx#475 probe. Paper account only. Read-only: no orders.
//!
//! Subscribes to account updates and records the callbacks for about 90 s,
//! long enough for the first periodic batch (about 66 s after connect).
//! Expect: every value as the server sends it (text and currency, ledger
//! keys with BASE and USD), the portfolio rows each followed by the time,
//! the time as HH:mm, account_download_end once. A second subscribe at 20 s
//! sends nothing. The unsubscribe at the end answers error 2100 with id -1.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W { start: Instant, values: usize, ends: usize }
impl W {
    fn t(&self) -> f64 { self.start.elapsed().as_secs_f64() }
}
impl Wrapper for W {
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, _a: &str) {
        self.values += 1;
        println!("{:6.1}s value {} = {} [{}]", self.t(), key, value, currency);
    }
    fn update_portfolio(&mut self, c: &Contract, position: f64, mp: f64, _mv: f64, avg: f64, _u: f64, _r: f64, _a: &str) {
        println!("{:6.1}s portfolio {} {} pos={} mark={} avg={} primary='{}' local='{}' class='{}'",
            self.t(), c.con_id, c.symbol, position, mp, avg, c.primary_exchange, c.local_symbol, c.trading_class);
    }
    fn update_account_time(&mut self, time: &str) {
        println!("{:6.1}s time {}", self.t(), time);
    }
    fn account_download_end(&mut self, _a: &str) {
        self.ends += 1;
        println!("{:6.1}s account_download_end (#{})", self.t(), self.ends);
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("{:6.1}s error {} {} {}", self.t(), id, code, msg);
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
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let mut w = W { start: Instant::now(), values: 0, ends: 0 };
    println!("=== subscribe");
    client.req_account_updates(true, "");
    pump(&client, &mut w, 20);
    println!("=== second subscribe ({} values so far)", w.values);
    client.req_account_updates(true, "");
    pump(&client, &mut w, 70);
    println!("=== unsubscribe");
    client.req_account_updates(false, "");
    pump(&client, &mut w, 3);
    println!("=== values={} ends={}", w.values, w.ends);
    client.disconnect();
}
