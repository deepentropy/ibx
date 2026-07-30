//! Recipe: req_contract_details for AAPL on paper, print con_id and primary exchange.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_contract_details

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::ContractDetails;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    rows: Vec<ContractDetails>,
    end_seen: bool,
}

struct DetailsWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for DetailsWrapper {
    fn contract_details(&mut self, _req_id: i64, details: &ContractDetails) {
        self.state.lock().unwrap().rows.push(details.clone());
    }
    fn contract_details_end(&mut self, _req_id: i64) {
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={req_id} code={code} msg={msg}");
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
    let mut wrapper = DetailsWrapper { state: state.clone() };

    let aapl = Contract {
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };
    client.req_contract_details(1, &aapl)?;

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().end_seen { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    let s = state.lock().unwrap();
    println!("matches: {}", s.rows.len());
    for d in &s.rows {
        println!(
            "  con_id={:>8}  primary={:<10}  trading_class={}",
            d.contract.con_id, d.contract.primary_exchange, d.contract.trading_class,
        );
    }

    drop(s);
    client.disconnect();
    Ok(())
}
