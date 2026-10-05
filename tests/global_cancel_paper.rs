//! Global cancel against the real server on the paper account, through `EClient`.
//!
//! An order left open by an earlier session is listed by the server at
//! connect (the logon replay). `req_global_cancel` cancels every order of
//! the book, as the reference: those of this session, of other clients and
//! of earlier sessions. An order the replay gave as not routed yet (39=A)
//! was not in the book, so the global cancel sent nothing for it (paper
//! 04/10/2026).
//!
//! Session 1 places a GTC limit order far from the market and disconnects.
//! Session 2 connects, lists the open orders, calls `req_global_cancel` and
//! requires a Cancelled status from the server for every listed order.
//!
//! The orders are told apart by permId. An order id above the int range
//! (the Rust client's ids) is not sent to the server (no 6121), so a later
//! session shows the order with order id 0, as the reference shows an
//! order whose report has none (captured 01/10/2026); its permId is the
//! id part of its ClOrdID, the order id of session 1.
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
    /// (order id, permId, status) of each orderStatus.
    statuses: Vec<(i64, i64, String)>,
    /// (order id, permId) of each openOrder.
    open: Vec<(i64, i64)>,
}

struct Probe {
    state: Arc<Mutex<State>>,
}

impl Wrapper for Probe {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, perm_id: i64, _parent_id: i64, _last_fill_price: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        self.state.lock().unwrap().statuses.push((order_id, perm_id, status.into()));
    }
    fn open_order(&mut self, order_id: i64, _contract: &Contract, order: &Order, _state: &OrderState) {
        self.state.lock().unwrap().open.push((order_id, order.perm_id));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("  [error] id={} code={} {}", req_id, code, msg);
    }
}

fn last_status(s: &State, id: i64) -> Option<&str> {
    s.statuses.iter().rev().find(|(o, ..)| *o == id).map(|(.., st)| st.as_str())
}

fn last_status_of_perm(s: &State, perm_id: i64) -> Option<&str> {
    s.statuses.iter().rev().find(|(_, p, _)| *p == perm_id).map(|(.., st)| st.as_str())
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
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn global_cancel_reaches_orders_from_an_earlier_session() {
    let config = get_config().expect("IB_USERNAME / IB_PASSWORD not set: a live test fails without credentials");
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
    let perm_id = probe.state.lock().unwrap().statuses.iter().rev()
        .find(|(o, ..)| *o == id).map_or(0, |(_, p, _)| *p);
    client.disconnect();
    assert!(up, "session 1: order {} not accepted", id);
    assert_eq!(perm_id, id, "session 1: the permId is the id part of the ClOrdID, the order id");
    println!("  session 1: order {} (permId {}) working, disconnected", id, perm_id);

    // Session 2: the server lists it at connect; the global cancel must reach it.
    let client = connect_paper(&config);
    let mut probe = Probe { state: Arc::new(Mutex::new(State::default())) };
    pump(&client, &mut probe, 5, |_| false);
    // The listing only (the replay's reports gave openOrder already).
    probe.state.lock().unwrap().open.clear();
    client.req_all_open_orders(&mut probe);
    let open = probe.state.lock().unwrap().open.clone();
    println!("  session 2: {} open order(s) listed: {:?}", open.len(), open);
    let listed: Vec<i64> = open.iter().map(|&(_, p)| p).collect();
    assert!(listed.contains(&perm_id), "session 2: order of permId {} from session 1 not listed", perm_id);
    assert!(open.iter().any(|&(o, p)| p == perm_id && o == 0),
        "session 2: the order of session 1 has no API order id on the server: order id 0");

    client.req_global_cancel().expect("req_global_cancel");
    let all = pump(&client, &mut probe, 20, |s| {
        listed.iter().all(|&p| last_status_of_perm(s, p) == Some("Cancelled"))
    });
    let not_cancelled: Vec<i64> = {
        let s = probe.state.lock().unwrap();
        listed.iter().copied().filter(|&p| last_status_of_perm(&s, p) != Some("Cancelled")).collect()
    };
    client.disconnect();
    assert!(all, "global cancel left {} of {} listed order(s) not cancelled",
        not_cancelled.len(), listed.len());
    println!("  PASS: all {} listed order(s) cancelled by the server", listed.len());
}
