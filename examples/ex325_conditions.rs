//! ibx#325 ibx#327 — verify an order whose only extra is a condition keeps it,
//! with the condition flags on the right fields. Paper account only.
//!
//! Places three BUY LMT orders far below the market, each with one price
//! condition and nothing else: no flag, cancel-order, and ignore-RTH. Pass =
//! each is accepted. Run with
//! `RUST_LOG=ibx::protocol::connection=trace,ibx::engine::hot_loop::ccp=trace`
//! to see the condition block sent and what the server echoes. Every order is
//! cancelled at the end.
//!
//! Run: cargo run --example ex325_conditions
//! Needs IB_USERNAME / IB_PASSWORD (paper) and optionally IB_HOST.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;
use ibx::types::{OrderCondition, PRICE_SCALE};

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String)>,
    errors: Vec<(i64, i64, String)>,
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, _parent_id: i64, _last_fill_price: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[order_status] id={} status={}", order_id, status);
        self.state.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn aapl() -> Contract {
    Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    }
}

fn last_status(state: &Arc<Mutex<State>>, id: i64) -> Option<String> {
    state.lock().unwrap().statuses.iter().rev().find(|(o, _)| *o == id).map(|(_, s)| s.clone())
}

fn working(state: &Arc<Mutex<State>>, id: i64) -> bool {
    matches!(last_status(state, id).as_deref(), Some("PreSubmitted" | "Submitted"))
}

fn pump(client: &EClient, wrapper: &mut ProbeWrapper, secs: u64, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        client.process_msgs(wrapper);
        if done() { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    println!("== Connecting to paper ({})...", host);
    let client = EClient::connect(&EClientConfig { username, password, host, paper: true, core_id: None })?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };
    let base = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;

    let order = |cancel_order: bool, ignore_rth: bool| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0,
        conditions: vec![OrderCondition::Price {
            con_id: 265598, exchange: "SMART".into(), price: 1000 * PRICE_SCALE,
            is_more: true, trigger_method: 0,
        }],
        conditions_cancel_order: cancel_order,
        conditions_ignore_rth: ignore_rth,
        ..Default::default()
    };
    let cases = [
        ("condition only", base, order(false, false)),
        ("condition + cancel-order", base + 1, order(true, false)),
        ("condition + ignore-RTH", base + 2, order(false, true)),
    ];

    let mut results = Vec::new();
    for (label, id, o) in cases {
        println!("\n== {} (order {})", label, id);
        client.place_order(id, &aapl(), &o).ok();
        let st = state.clone();
        pump(&client, &mut wrapper, 15, || {
            working(&st, id) || st.lock().unwrap().errors.iter().any(|(r, c, _)| *r == id && *c != 399)
        });
        let ok = working(&state, id);
        println!("  {}", if ok { "accepted" } else { "FAIL: not accepted" });
        results.push((label, id, ok));
    }

    println!("\n== Cleanup");
    for (_, id, _) in &results {
        let _ = client.cancel_order(*id, "");
    }
    pump(&client, &mut wrapper, 8, || false);

    println!("\n== RESULTS");
    for (label, id, ok) in &results {
        println!("  {:<26} {}  {}", label, id, if *ok { "PASS" } else { "FAIL" });
    }
    let all = results.iter().all(|(_, _, ok)| *ok);
    println!("== RESULT: {}", if all { "PASS" } else { "FAIL" });
    client.disconnect();
    if all { Ok(()) } else { Err("conditions live check failed".into()) }
}
