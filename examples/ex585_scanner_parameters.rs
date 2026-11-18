//! ibx#585 probe. Read-only: login, the scanner parameters as the Rust
//! client gives them to its wrapper, saved to a file, disconnect.
//!
//! Usage: ex585_scanner_parameters <output file>
//! Compare the file with the text the reference gives its client.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct Params(Option<String>);

impl Wrapper for Params {
    fn scanner_parameters(&mut self, xml: &str) {
        self.0 = Some(xml.to_string());
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = env::args().nth(1).ok_or("usage: ex585_scanner_parameters <output file>")?;
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    println!("logged in paper (account {})", client.account_id);

    client.req_scanner_parameters()?;
    let mut params = Params::default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while params.0.is_none() && Instant::now() < deadline {
        client.process_msgs(&mut params);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();
    match params.0 {
        Some(xml) => {
            println!("scanner parameters: {} characters, saved to {out}", xml.chars().count());
            std::fs::write(&out, xml)?;
        }
        None => println!("no scanner parameters in 30 s"),
    }
    Ok(())
}
