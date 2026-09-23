//! ibx#240 — verify an adjustable stop keeps its parent link, OCA group and
//! tif when used as a bracket child. Paper account only.
//!
//! Places a parent BUY LMT far below the market (never fills), an adjustable
//! STP child and a LMT take-profit child, both linked to the parent and in one
//! OCA group, all GTC. Then cancels ONLY the parent. Pass = every order is
//! accepted and the server cancels both children on its own, which it only
//! does when the parent link reached it. Any order still working at the end
//! is cancelled.
//!
//! The optional argument picks what the stop converts to: `stp` (default),
//! `trail` or `trail-limit`, to check each adjusted-type code on the server.
//!
//! Run: cargo run --example ex240_adjustable_stop_bracket [stp|trail|trail-limit]
//! Needs IB_USERNAME / IB_PASSWORD (paper) and optionally IB_HOST.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String, i64)>, // (order_id, status, parent_id)
    errors: Vec<(i64, i64, String)>,   // (req_id, code, msg)
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, parent_id: i64, _last_fill_price: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[order_status] id={} status={} parent_id={}", order_id, status, parent_id);
        self.state.lock().unwrap().statuses.push((order_id, status.into(), parent_id));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn aapl() -> Contract {
    Contract {
        con_id: 265598,
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn last_status(state: &Arc<Mutex<State>>, id: i64) -> Option<String> {
    state.lock().unwrap().statuses.iter().rev()
        .find(|(oid, _, _)| *oid == id).map(|(_, s, _)| s.clone())
}

fn rejected(state: &Arc<Mutex<State>>, id: i64) -> bool {
    state.lock().unwrap().errors.iter().any(|(rid, _, _)| *rid == id)
        || last_status(state, id).as_deref() == Some("Inactive")
}

/// Pump messages until `done` holds or the timeout expires.
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
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None,
    })?;

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    let (parent_id, stop_id, tp_id) = (base, base + 1, base + 2);
    let oca = format!("ibx240_{}", base);

    // Parent: BUY 1 @ $1.00 GTC — far below the market, never fills.
    let parent = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: 1.00, tif: "GTC".into(),
        ..Default::default()
    };
    // Child 1: adjustable STP. Stop $0.50; if $900 trades, move the stop to
    // $0.60 and convert it to the chosen type.
    let mode = env::args().nth(1).unwrap_or_else(|| "stp".into());
    let mut stop = Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0,
        aux_price: 0.50,
        adjusted_order_type: "STP".into(),
        trigger_price: 900.00,
        adjusted_stop_price: 0.60,
        parent_id, oca_group: oca.clone(), tif: "GTC".into(),
        ..Default::default()
    };
    match mode.as_str() {
        "stp" => {}
        "trail" => {
            stop.adjusted_order_type = "TRAIL".into();
            stop.adjusted_trailing_amount = 0.10;
            stop.adjustable_trailing_unit = 0;
        }
        "trail-limit" => {
            stop.adjusted_order_type = "TRAIL LIMIT".into();
            stop.adjusted_stop_limit_price = 0.55;
            stop.adjusted_trailing_amount = 0.10;
            stop.adjustable_trailing_unit = 0;
        }
        other => return Err(format!("unknown mode '{}': use stp, trail or trail-limit", other).into()),
    }
    println!("== Adjustable stop converts to: {}", stop.adjusted_order_type);
    // Child 2: take-profit LMT, same parent and OCA group.
    let tp = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: 5000.00,
        parent_id, oca_group: oca.clone(), tif: "GTC".into(),
        ..Default::default()
    };

    println!("\n== Placing parent={} stop={} tp={} oca={}", parent_id, stop_id, tp_id, oca);
    client.place_order(parent_id, &aapl(), &parent)?;
    client.place_order(stop_id, &aapl(), &stop)?;
    client.place_order(tp_id, &aapl(), &tp)?;

    let ids = [parent_id, stop_id, tp_id];
    let working = |id: i64| matches!(last_status(&state, id).as_deref(), Some("PreSubmitted" | "Submitted"));
    let settled = pump(&client, &mut wrapper, 20, || ids.iter().all(|&id| working(id) || rejected(&state, id)));

    let mut pass = true;
    for &id in &ids {
        let ok = working(id) && !rejected(&state, id);
        println!("  order {} -> {:?}{}", id, last_status(&state, id), if ok { "" } else { "  FAIL" });
        pass &= ok;
    }
    if !settled { println!("  FAIL: not every order was acknowledged within 20s"); }
    pass &= settled;

    if pass {
        println!("\n== Cancelling ONLY the parent {}", parent_id);
        client.cancel_order(parent_id, "")?;
        let is_cancelled = |id: i64| last_status(&state, id).as_deref() == Some("Cancelled");
        let cascaded = pump(&client, &mut wrapper, 20, || ids.iter().all(|&id| is_cancelled(id)));
        for &id in &ids {
            println!("  order {} -> {:?}", id, last_status(&state, id));
        }
        if cascaded {
            println!("  children cancelled by the server with the parent: link confirmed");
        } else {
            println!("  FAIL: children did not follow the parent cancel");
            pass = false;
        }
    }

    // Safety net: cancel anything still working.
    for &id in &ids {
        if working(id) {
            println!("  cleanup: cancelling {}", id);
            let _ = client.cancel_order(id, "");
        }
    }
    pump(&client, &mut wrapper, 5, || false);

    println!("\n== RESULT: {}", if pass { "PASS" } else { "FAIL" });
    client.disconnect();
    if pass { Ok(()) } else { Err("ibx#240 live check failed".into()) }
}
