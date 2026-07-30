//! ib-agent#134 — verify CCP no longer FINs in the post-burst grace window.
//!
//! Connects to paper, holds the session past the historical ~12 s FIN
//! deadline, then issues a CCP-routed reqContractDetails (AAPL/SMART/STK)
//! and waits for `contract_details_end`. Pass = end callback within 10 s.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::ContractDetails;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    details_count: u64,
    end_seen: bool,
    last_error: Option<(i64, i64, String)>,
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
        let mut s = self.state.lock().unwrap();
        s.details_count += 1;
        println!(
            "[contract_details] req_id={} symbol={} conid={} exch={}",
            req_id, details.contract.symbol, details.contract.con_id, details.contract.exchange,
        );
    }
    fn contract_details_end(&mut self, req_id: i64) {
        println!("[contract_details_end] req_id={}", req_id);
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().last_error = Some((req_id, code, msg.into()));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    let hold_secs: u64 = env::var("HOLD_SECS").unwrap_or_else(|_| "20".to_string()).parse()?;

    println!("== ex134_ccp_grace: connecting to paper ({})...", host);
    let t0 = Instant::now();
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None, code_provider: None,
    })?;
    println!("== Connected in {:.1}s", t0.elapsed().as_secs_f64());

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    println!("== Holding session for {}s (CCP would historically FIN at ~12s)...", hold_secs);
    let hold_deadline = Instant::now() + Duration::from_secs(hold_secs);
    while Instant::now() < hold_deadline {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("== Hold complete, issuing reqContractDetails for AAPL/SMART/STK");

    let aapl = Contract {
        con_id: 265598,
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };
    let req_id: i64 = 1;
    client.req_contract_details(req_id, &aapl)
        .map_err(|e| format!("req_contract_details failed: {}", e))?;

    let probe_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < probe_deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().end_seen { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    let s = state.lock().unwrap();
    println!("\n== Result ==");
    println!("  details_count : {}", s.details_count);
    println!("  end_seen      : {}", s.end_seen);
    if let Some((rid, code, msg)) = &s.last_error {
        println!("  last_error    : req_id={} code={} msg={}", rid, code, msg);
    }
    let pass = s.end_seen && s.details_count > 0;
    drop(s);
    client.disconnect();

    if pass {
        println!("\nPASS: CCP survived the grace window and answered reqContractDetails");
        Ok(())
    } else {
        Err("FAIL: contract_details_end never received within deadline".into())
    }
}
