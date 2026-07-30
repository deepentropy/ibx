//! Verify issue #111: live login through the second-factor approval gate,
//! then a single read-only `req_contract_details` for AAPL. No orders, no
//! market data.
//!
//! Run:
//!   $env:RUST_LOG="info"; cargo run --release --example ex111_live_2fa_aapl
//!
//! Requires `IB_LIVE_USERNAME` / `IB_LIVE_PASSWORD` in `.env` (auto-loaded).
//! Approve the push on your mobile authenticator when prompted.

use std::env;
use std::fs;
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

const REQ_ID: i64 = 1;

struct ProbeWrapper { state: Arc<Mutex<State>> }

impl Wrapper for ProbeWrapper {
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        if req_id != REQ_ID {
            // Engine-internal auto-fetches for cold-cache positions use
            // synthetic req_ids starting at 0xF000_0000 (see ccp.rs:136).
            return;
        }
        let mut s = self.state.lock().unwrap();
        s.details_count += 1;
        println!(
            "[contract_details] req_id={} symbol={} conid={} exch={} primary={} currency={} long_name={}",
            req_id, d.contract.symbol, d.contract.con_id, d.contract.exchange,
            d.contract.primary_exchange, d.contract.currency, d.long_name,
        );
    }
    fn contract_details_end(&mut self, req_id: i64) {
        if req_id != REQ_ID { return; }
        println!("[contract_details_end] req_id={}", req_id);
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().last_error = Some((req_id, code, msg.into()));
    }
}

/// Minimal `.env` loader (KEY=VALUE per line, `#` comments, no quoting tricks).
fn load_dotenv() {
    if let Ok(text) = fs::read_to_string(".env") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if env::var_os(k).is_none() {
                    // SAFETY: single-threaded at startup, before any thread spawns.
                    unsafe { env::set_var(k, v); }
                }
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    load_dotenv();

    let username = env::var("IB_LIVE_USERNAME")
        .map_err(|_| "IB_LIVE_USERNAME not set (.env or shell)")?;
    let password = env::var("IB_LIVE_PASSWORD")
        .map_err(|_| "IB_LIVE_PASSWORD not set (.env or shell)")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    println!("== Connecting LIVE ({}). Approve the second-factor push on your phone when it arrives.", host);
    let t0 = Instant::now();
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: false, core_id: None, code_provider: None,
    })?;
    println!("== Connected in {:.1}s", t0.elapsed().as_secs_f64());

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    let aapl = Contract {
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        primary_exchange: "NASDAQ".into(),
        ..Default::default()
    };
    println!("== reqContractDetails AAPL/SMART/STK/USD (req_id={})", REQ_ID);
    let t_req = Instant::now();
    client.req_contract_details(REQ_ID, &aapl)
        .map_err(|e| format!("req_contract_details failed: {}", e))?;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
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
        println!("\nPASS — issue #111 verified on live account");
        Ok(())
    } else {
        Err("FAIL: contract_details_end not received".into())
    }
}
