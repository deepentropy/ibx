//! Seed a held SPY position on the paper account so Phase 132 of the compat
//! suite has something to enrich. Places 1 share BUY at LMT $1000 GTC
//! (marketable — far above market for a buy), outside-RTH so it can fill
//! pre-market. Waits up to 60s for Filled status, then disconnects leaving
//! the position held. Safe to no-op-rerun: if already holding SPY, the new
//! fill just adds 1 more share.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example seed_spy_position

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

#[derive(Default, Debug)]
struct State {
    statuses: Vec<(i64, String, f64)>,
    errors: Vec<(i64, i64, String)>,
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, _remaining: f64,
        avg_fill: f64, _perm_id: i64, _parent_id: i64, _last_fill: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[order_status] id={} status={} filled={} avgFill={:.2}", order_id, status, filled, avg_fill);
        self.state.lock().unwrap().statuses.push((order_id, status.into(), filled));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    println!("== Connecting to paper ({})...", host);
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None, code_provider: None,
    })?;
    println!("== Connected.");

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    let spy = Contract {
        con_id: 756733,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };

    // BUY 1 share at LMT $760 — aggressive over yesterday's close (~$725)
    // to cross the wider pre-market spread, but well within IB's
    // price-band protection. GTC + outside_rth=true so it can fill pre-market.
    let order = Order {
        action: "BUY".into(),
        total_quantity: 1.0,
        order_type: "LMT".into(),
        lmt_price: 760.0,
        tif: "GTC".into(),
        outside_rth: true,
        ..Default::default()
    };

    let order_id = std::process::id() as i64;
    println!("== Placing BUY 1 SPY @ LMT 760 GTC outsideRTH (order_id={})", order_id);
    client.place_order(order_id, &spy, &order)
        .map_err(|e| format!("place_order failed: {}", e))?;

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut filled = false;
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        let s = state.lock().unwrap();
        for (oid, st, f) in &s.statuses {
            if *oid == order_id && st == "Filled" && *f >= 1.0 {
                filled = true;
            }
        }
        drop(s);
        if filled { break; }
        std::thread::sleep(Duration::from_millis(50));
    }

    if filled {
        println!("\nPASS: 1 share SPY filled — position seeded for Phase 132 validation");
    } else {
        println!("\nFAIL: no fill in 180s — cancelling stray order");
        let _ = client.cancel_order(order_id, "");
        std::thread::sleep(Duration::from_secs(2));
        client.process_msgs(&mut wrapper);
    }

    client.disconnect();
    if filled { Ok(()) } else { Err("seed failed".into()) }
}
