//! Recipe: subscribe to TOP_PERC_GAIN scanner, print first 10 results.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_scanner

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::ContractDetails;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    rows: Vec<(i32, ContractDetails)>,
    end_seen: bool,
}

struct ScannerWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ScannerWrapper {
    fn scanner_data(
        &mut self, _req_id: i64, rank: i32, details: &ContractDetails,
        _distance: &str, _benchmark: &str, _projection: &str, _legs: &str,
    ) {
        self.state.lock().unwrap().rows.push((rank, details.clone()));
    }
    fn scanner_data_end(&mut self, _req_id: i64) {
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        if !matches!(code, 2104 | 2106 | 2158) {
            eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        }
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

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ScannerWrapper { state: state.clone() };

    let req_id = 1;
    println!("subscribing TOP_PERC_GAIN, STK.US.MAJOR…");
    client.req_scanner_subscription(req_id, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 25)?;

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().end_seen { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut s = state.lock().unwrap();
    s.rows.sort_by_key(|(rank, _)| *rank);
    println!("results: {}", s.rows.len());
    for (rank, d) in s.rows.iter().take(10) {
        println!("  #{rank:<3}  {:<8} {:<6} con_id={}", d.contract.symbol, d.contract.primary_exchange, d.contract.con_id);
    }

    drop(s);
    client.cancel_scanner_subscription(req_id)?;
    client.disconnect();
    Ok(())
}
