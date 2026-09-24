//! Global cancel against the real server on the paper account, through `EClient`.
//!
//! An order left open by an earlier session is listed by the server at
//! connect. `req_global_cancel` walks the instrument ids below the shared
//! instrument count, and that listing took a slot without raising the count,
//! so the global cancel never reached such an order.
//!
//! Session 1 places a GTC limit order far from the market and disconnects.
//! Session 2 connects, lists the open orders, calls `req_global_cancel` and
//! requires a Cancelled status from the server for every listed order.
//!
//! This cancels EVERY open order on the paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD (paper account) in the environment.
//! Run with: cargo test --test global_cancel_paper -- --nocapture

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::OrderState;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String)>,
    open: Vec<i64>,
}

struct Probe {
    state: Arc<Mutex<State>>,
}

impl Wrapper for Probe {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, _parent_id: i64, _last_fill_price: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        self.state.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn open_order(&mut self, order_id: i64, _contract: &Contract, _order: &Order, _state: &OrderState) {
        self.state.lock().unwrap().open.push(order_id);
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("  [error] id={} code={} {}", req_id, code, msg);
    }
}

fn last_status(s: &State, id: i64) -> Option<&str> {
    s.statuses.iter().rev().find(|(o, _)| *o == id).map(|(_, st)| st.as_str())
}

fn pump(client: &EClient, probe: &mut Probe, secs: u64, done: impl Fn(&State) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        client.process_msgs(probe);
        if done(&probe.state.lock().unwrap()) { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn get_config() -> Option<EClientConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(EClientConfig { username, password, host, paper: true, core_id: None })
}

/// Connect, and refuse to go on unless this is a paper account (id starts with DU).
fn connect_paper(config: &EClientConfig) -> EClient {
    let client = EClient::connect(config).expect("connect to paper failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    }
    client
}

#[test]
fn global_cancel_reaches_orders_from_an_earlier_session() {
    let config = match get_config() {
        Some(c) => c,
        None => {
            println!("SKIP: IB_USERNAME / IB_PASSWORD not set");
            return;
        }
    };
    println!("=== Global cancel (paper account) ===");

    // Session 1: leave one GTC order working.
    let client = connect_paper(&config);
    let mut probe = Probe { state: Arc::new(Mutex::new(State::default())) };
    let id = client.next_order_id();
    let spy = Contract {
        con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    };
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: 1.00, tif: "GTC".into(), ..Default::default()
    };
    client.place_order(id, &spy, &order).expect("place_order");
    let up = pump(&client, &mut probe, 20, |s| {
        matches!(last_status(s, id), Some("PreSubmitted" | "Submitted"))
    });
    client.disconnect();
    assert!(up, "session 1: order {} not accepted", id);
    println!("  session 1: order {} working, disconnected", id);

    // Session 2: the server lists it at connect; the global cancel must reach it.
    let client = connect_paper(&config);
    let mut probe = Probe { state: Arc::new(Mutex::new(State::default())) };
    pump(&client, &mut probe, 5, |_| false);
    client.req_all_open_orders(&mut probe);
    let listed = probe.state.lock().unwrap().open.clone();
    println!("  session 2: {} open order(s) listed", listed.len());
    assert!(listed.contains(&id), "session 2: order {} from session 1 not listed", id);

    client.req_global_cancel().expect("req_global_cancel");
    let all = pump(&client, &mut probe, 20, |s| {
        listed.iter().all(|&o| last_status(s, o) == Some("Cancelled"))
    });
    let not_cancelled: Vec<i64> = {
        let s = probe.state.lock().unwrap();
        listed.iter().copied().filter(|&o| last_status(&s, o) != Some("Cancelled")).collect()
    };
    client.disconnect();
    assert!(all, "global cancel left {} of {} listed order(s) not cancelled",
        not_cancelled.len(), listed.len());
    println!("  PASS: all {} listed order(s) cancelled by the server", listed.len());
}
