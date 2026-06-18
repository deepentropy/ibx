//! Verification: req_contract_details for AXTI on paper.

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
            "[contract_details] req_id={} symbol={} conid={} exch={} primary={} currency={} min_tick={}",
            req_id,
            details.contract.symbol,
            details.contract.con_id,
            details.contract.exchange,
            details.contract.primary_exchange,
            details.contract.currency,
            details.min_tick,
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

    println!("== Connecting to paper ({})...", host);
    let t0 = Instant::now();
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None,
    })?;
    println!("== Connected in {:.1}s", t0.elapsed().as_secs_f64());

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    let axti = Contract {
        symbol: "AXTI".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };
    let req_id: i64 = 1;
    println!("== reqContractDetails AXTI/SMART/STK/USD by symbol (req_id={})", req_id);
    let t_req = Instant::now();
    client.req_contract_details(req_id, &axti)
        .map_err(|e| format!("req_contract_details failed: {}", e))?;

    let probe_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < probe_deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().end_seen { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    let s = state.lock().unwrap();
    println!("\n== Result (after {:.2}s) ==", t_req.elapsed().as_secs_f64());
    println!("  details_count : {}", s.details_count);
    println!("  end_seen      : {}", s.end_seen);
    if let Some((rid, code, msg)) = &s.last_error {
        println!("  last_error    : req_id={} code={} msg={}", rid, code, msg);
    }
    let pass = s.end_seen && s.details_count > 0;
    drop(s);
    client.disconnect();

    if pass {
        println!("\nPASS");
        Ok(())
    } else {
        Err("FAIL: contract_details_end not received".into())
    }
}
