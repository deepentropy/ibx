//! Recipe: subscribe to account-level PnL, take one update, then cancel.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_pnl

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    pnl: Option<(f64, f64, f64)>,
}

struct PnlWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for PnlWrapper {
    fn pnl(&mut self, _req_id: i64, daily: f64, unrealized: f64, realized: f64) {
        self.state.lock().unwrap().pnl = Some((daily, unrealized, realized));
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

    let account = client.account_id.clone();
    println!("account: {account}");

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = PnlWrapper { state: state.clone() };

    let req_id = 1;
    client.req_pnl(req_id, &account, "");

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().pnl.is_some() { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    match state.lock().unwrap().pnl {
        Some((daily, unrealized, realized)) => {
            println!("daily={daily:.2}  unrealized={unrealized:.2}  realized={realized:.2}");
        }
        None => println!("no pnl update within 15s — try again with an open position"),
    }

    client.cancel_pnl(req_id);
    client.disconnect();
    Ok(())
}
