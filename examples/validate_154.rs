//! End-to-end validation for ibx#154: cold-start req_positions with held SPY.
//!
//! Connects fresh (triggers init burst → batch with held positions →
//! auto-secdef fires for any conId not in cache), then calls req_positions
//! and prints the wrapper-facing Contract fields. PASS iff symbol/secType
//! arrive populated.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::Contract;
use ibx::api::wrapper::Wrapper;

#[derive(Default, Debug)]
struct State {
    positions: Vec<(String, Contract, f64, f64)>,
    end_seen: bool,
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn position(&mut self, account: &str, contract: &Contract, pos: f64, avg_cost: f64) {
        self.state.lock().unwrap().positions.push((
            account.into(), contract.clone(), pos, avg_cost,
        ));
    }
    fn position_end(&mut self) {
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    println!("== Connecting fresh to paper ({})", host);
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None, code_provider: None,
    })?;
    println!("== Connected.");

    // Give the init burst time to arrive and the auto-secdef reply to populate.
    std::thread::sleep(Duration::from_secs(2));

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    println!("== Calling req_positions");
    client.req_positions(&mut wrapper);

    // Drain remaining callbacks/events.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < drain_deadline {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(Duration::from_millis(50));
    }

    let s = state.lock().unwrap();
    println!("\n== Result ==");
    println!("  positions delivered : {}", s.positions.len());
    println!("  end_seen            : {}", s.end_seen);

    let mut pass = true;
    for (acct, c, pos, avg_cost) in &s.positions {
        println!(
            "  account={} conId={} symbol={:?} secType={:?} currency={:?} exch={:?} localSym={:?} pos={} avgCost={:.3}",
            acct, c.con_id, c.symbol, c.sec_type, c.currency, c.exchange, c.local_symbol, pos, avg_cost,
        );
        if c.con_id != 0 && c.symbol.is_empty() {
            println!("    FAIL: conId {} delivered with empty symbol", c.con_id);
            pass = false;
        }
        if c.con_id != 0 && c.sec_type.is_empty() {
            println!("    FAIL: conId {} delivered with empty secType", c.con_id);
            pass = false;
        }
    }

    let any_positions = !s.positions.is_empty();
    if !any_positions {
        println!("  WARN: no positions in account — can't validate enrichment");
    }
    drop(s);
    client.disconnect();

    if pass && any_positions {
        println!("\nPASS: every position carried symbol+secType");
        Ok(())
    } else if pass {
        println!("\nINCONCLUSIVE: no held positions to validate against");
        Ok(())
    } else {
        Err("validation failed — see FAIL lines above".into())
    }
}
