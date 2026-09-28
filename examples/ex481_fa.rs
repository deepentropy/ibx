//! ibx#481 probe. Paper account, read-only: login, requestFA, replaceFA,
//! disconnect. On a session that is not FA both get error 321.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
    fn receive_fa(&mut self, fa_data_type: i32, xml: &str) { println!("[receive_fa] {} {}", fa_data_type, xml); }
    fn replace_fa_end(&mut self, req_id: i64, text: &str) { println!("[replace_fa_end] {} {}", req_id, text); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    println!("logged in paper (account {})", client.account_id);
    client.request_fa(1);
    client.replace_fa(5, 1, "<ListOfGroups/>");
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end { client.process_msgs(&mut W); std::thread::sleep(Duration::from_millis(10)); }
    client.disconnect();
    Ok(())
}
