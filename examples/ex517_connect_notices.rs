//! ibx#517 probe. Paper account, read-only: login, the next valid id, then
//! the notices of the connect (2104, 2106, 2158, and the version warning
//! after them), disconnect.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W;
impl Wrapper for W {
    fn next_valid_id(&mut self, order_id: i64) { println!("[next_valid_id] {}", order_id); }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    println!("logged in paper");
    client.req_ids(&mut W);
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end { client.process_msgs(&mut W); std::thread::sleep(Duration::from_millis(10)); }
    client.disconnect();
    Ok(())
}
