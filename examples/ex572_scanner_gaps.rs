//! ibx#572 probe through the Rust client: four scanner subscriptions and a
//! scanner parameters request at the start of a session, so that all wait
//! for the parameters; then a second parameters request once they are
//! known. Read-only. Run with `IBX_LOG_DIR` and `IBX_LOG_LEVEL=info` to read
//! the times at which each subscription leaves in the engine log.
//!
//! Prints the scanner callbacks with their time from the first request.
//! It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{ContractDetails, ScannerSubscription};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
}

impl Log {
    fn line(&self, text: String) {
        println!("   +{:6} ms  {text}", self.t0.elapsed().as_millis());
    }
}

impl Wrapper for Log {
    fn scanner_parameters(&mut self, xml: &str) {
        self.line(format!("scannerParameters {} characters", xml.len()));
    }
    fn scanner_data(&mut self, req_id: i64, rank: i32, _: &ContractDetails, _: &str, _: &str, _: &str, _: &str) {
        if rank == 0 {
            self.line(format!("scannerData {req_id} rank 0"));
        }
    }
    fn scanner_data_end(&mut self, req_id: i64) {
        self.line(format!("scannerDataEnd {req_id}"));
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {text}"));
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 198,
        // The scanner waits are copied with the setting only (ibx#564).
        copied_waits: env::var("IBX_COPIED_WAITS").is_ok_and(|v| v == "1"),
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let mut w = Log { t0: Instant::now() };
    for (i, code) in ["TOP_PERC_GAIN", "TOP_PERC_LOSE", "MOST_ACTIVE", "HOT_BY_VOLUME"].into_iter().enumerate() {
        let sub = ScannerSubscription {
            instrument: "STK".into(), location_code: "STK.US.MAJOR".into(), scan_code: code.into(),
            number_of_rows: 5, ..Default::default()
        };
        client.req_scanner_subscription(9101 + i as i64, &sub, &[], &[])?;
    }
    client.req_scanner_parameters()?;
    pump(&client, &mut w, 15.0);
    w.line("---- a second parameters request".into());
    client.req_scanner_parameters()?;
    pump(&client, &mut w, 3.0);
    for i in 0..4 {
        client.cancel_scanner_subscription(9101 + i)?;
    }
    pump(&client, &mut w, 1.0);
    client.disconnect();
    Ok(())
}
