//! Hello-world recipe: connect, request next_valid_id, disconnect.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_login

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
        username: env::var("IB_USERNAME")?,
        password: env::var("IB_PASSWORD")?,
        host: "cdc1.ibllc.com".into(),
        paper: true,
        core_id: None,
        code_provider: None,
    })?;

    let mut wrapper = LoginWrapper::default();
    client.req_ids(&mut wrapper);

    let next_id = wrapper.next_id.ok_or("did not receive next_valid_id")?;
    println!("logged in. account = {}, next_valid_id = {next_id}", client.account_id);

    client.disconnect();
    Ok(())
}
