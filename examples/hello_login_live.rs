//! Live-account login recipe: connect with `paper: false`, request next_valid_id,
//! disconnect. Read-only — no orders, no market data.
//!
//! When the live login triggers a second-factor push, approve it on your mobile
//! authenticator. The connect call blocks until the gate clears.
//!
//! Usage: IB_LIVE_USERNAME=... IB_LIVE_PASSWORD=... cargo run --example hello_login_live

use std::env;

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct LoginWrapper {
    next_id: Option<i64>,
}

impl Wrapper for LoginWrapper {
    fn next_valid_id(&mut self, order_id: i64) {
        self.next_id = Some(order_id);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_LIVE_USERNAME")?,
        password: env::var("IB_LIVE_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: false,
        core_id: None,
        code_provider: None,
    })?;

    let mut wrapper = LoginWrapper::default();
    client.req_ids(&mut wrapper);

    let next_id = wrapper.next_id.ok_or("did not receive next_valid_id")?;
    println!("logged in LIVE. account = {}, next_valid_id = {next_id}", client.account_id);

    client.disconnect();
    Ok(())
}
