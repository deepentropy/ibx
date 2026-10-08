//! ibx#520 probe. Paper account, read-only: login, then print every notice
//! with its time for PROBE_SECS seconds (default 90). A second login of the
//! same user from another process during that time closes this session's
//! link: the notices of the loss and of the return follow.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_SECS.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct W(Instant);
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[{:7.3}] error {} {} {}", self.0.elapsed().as_secs_f64(), id, code, msg);
    }
    fn connection_closed(&mut self) { println!("[{:7.3}] connection_closed", self.0.elapsed().as_secs_f64()); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secs: u64 = env::var("PROBE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(90);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    println!("logged in paper");
    let mut w = W(Instant::now());
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(&mut w); std::thread::sleep(Duration::from_millis(10)); }
    client.disconnect();
    Ok(())
}
