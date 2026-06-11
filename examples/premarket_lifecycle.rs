//! Two-phase premarket order lifecycle test (5-order sample).
//!
//! Phase 1 — `send`: connect, place 5 distinct order types on SPY (all GTC +
//! outside_rth so they survive premarket and the process exit), persist each
//! (orderId, permId) to `.tmp/premarket_orders.json`, then disconnect.
//!
//! Phase 2 — `cancel`: relaunch a fresh process, let the session-recovery push
//! hydrate the open-order cache, then cancel every saved order by permId (the
//! stable cross-session handle). Resumable: each cancelled order is flushed to
//! the state file immediately, so a re-run only retries what is still open.
//!
//! Usage:
//!   IB_USERNAME=... IB_PASSWORD=... cargo run --example premarket_lifecycle send
//!   IB_USERNAME=... IB_PASSWORD=... cargo run --example premarket_lifecycle cancel
//!
//! None of the 5 orders are marketable, so none fill in premarket.

use std::env;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::OrderState;
use ibx::api::wrapper::Wrapper;

const STATE_FILE: &str = ".tmp/premarket_orders.json";
const SPY_CON_ID: i64 = 756733;

/// One order in the 5-sample. (label, action, order_type, lmt_price, aux_price)
const SAMPLE: &[(&str, &str, &str, f64, f64)] = &[
    ("BUY LMT 1.00",         "BUY",  "LMT",     1.00,    0.0),
    ("SELL LMT 9999.00",     "SELL", "LMT",     9999.00, 0.0),
    ("BUY STP 9999.00",      "BUY",  "STP",     0.0,     9999.00),
    ("BUY STP LMT 9999.00",  "BUY",  "STP LMT", 9999.00, 9999.00),
    ("SELL TRAIL 100.00",    "SELL", "TRAIL",   0.0,     100.00),
];

#[derive(Default)]
struct State {
    /// (order_id, status, perm_id) in arrival order.
    statuses: Vec<(i64, String, i64)>,
    /// open orders surfaced by req_all_open_orders: (order_id, perm_id, status)
    open: Vec<(i64, i64, String)>,
}

struct OrderWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for OrderWrapper {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill: f64, perm_id: i64, _parent_id: i64, _last_fill: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[status] oid={order_id} permId={perm_id} status={status}");
        self.state.lock().unwrap().statuses.push((order_id, status.into(), perm_id));
    }
    fn open_order(&mut self, order_id: i64, _c: &Contract, order: &Order, st: &OrderState) {
        self.state.lock().unwrap().open.push((order_id, order.perm_id, st.status.to_string()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        // 2104/2106/2158 are benign farm-connection notices.
        if !matches!(code, 2104 | 2106 | 2158) {
            eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        }
    }
}

fn pump_until<F: Fn(&State) -> bool>(
    client: &EClient, wrapper: &mut OrderWrapper, state: &Arc<Mutex<State>>,
    timeout: Duration, done: F,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        client.process_msgs(wrapper);
        if done(&state.lock().unwrap()) { return true; }
        if Instant::now() >= deadline { return false; }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn connect() -> Result<EClient, Box<dyn std::error::Error>> {
    let host = env::var("IB_HOST").ok().filter(|h| !h.is_empty())
        .unwrap_or_else(|| "cdc1.ibllc.com".into());
    Ok(EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?,
        password: env::var("IB_PASSWORD")?,
        host,
        paper: true,
        core_id: None,
    })?)
}

fn spy() -> Contract {
    Contract {
        con_id: SPY_CON_ID,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn save_state(orders: &serde_json::Value) {
    fs::create_dir_all(".tmp").ok();
    fs::write(STATE_FILE, serde_json::to_string_pretty(orders).unwrap()).unwrap();
}

fn run_send() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = OrderWrapper { state: state.clone() };
    let contract = spy();

    let mut saved = Vec::new();
    for (label, action, otype, lmt, aux) in SAMPLE {
        let oid = client.next_order_id();
        let order = Order {
            action: (*action).into(),
            total_quantity: 1.0,
            order_type: (*otype).into(),
            lmt_price: *lmt,
            aux_price: *aux,
            tif: "GTC".into(),
            outside_rth: true,
            ..Default::default()
        };
        println!("placing [{label}] oid={oid}");
        client.place_order(oid, &contract, &order)?;

        // Wait for the broker to ack with a permId for this order.
        let perm = {
            pump_until(&client, &mut w, &state, Duration::from_secs(20), |s| {
                s.statuses.iter().any(|(id, _, p)| *id == oid && *p > 0)
            });
            state.lock().unwrap().statuses.iter().rev()
                .find(|(id, _, p)| *id == oid && *p > 0)
                .map(|(_, _, p)| *p)
                .unwrap_or(0)
        };
        let status = state.lock().unwrap().statuses.iter().rev()
            .find(|(id, _, _)| *id == oid)
            .map(|(_, st, _)| st.clone())
            .unwrap_or_else(|| "NO_STATUS".into());

        if perm == 0 {
            eprintln!("  WARNING: no permId for [{label}] oid={oid} (status={status})");
        } else {
            println!("  acked [{label}] oid={oid} permId={perm} status={status}");
        }
        saved.push(serde_json::json!({
            "label": label, "order_id": oid, "perm_id": perm,
            "sent_status": status, "cancelled": false,
        }));
    }

    save_state(&serde_json::json!({ "orders": saved }));
    println!("\nWrote {} orders to {STATE_FILE}", saved.len());
    println!("Relaunch and run `cargo run --example premarket_lifecycle cancel` to cancel them.");

    client.disconnect();
    Ok(())
}

fn run_cancel() -> Result<(), Box<dyn std::error::Error>> {
    let raw = fs::read_to_string(STATE_FILE)
        .map_err(|e| format!("cannot read {STATE_FILE}: {e} (run `send` first)"))?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;

    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = OrderWrapper { state: state.clone() };

    // Give the session-recovery push time to hydrate the open-order cache, then
    // surface what hydrated so the permId lookup below can resolve.
    println!("waiting for open-order hydration...");
    pump_until(&client, &mut w, &state, Duration::from_secs(6), |_| false);
    client.req_all_open_orders(&mut w);
    pump_until(&client, &mut w, &state, Duration::from_secs(2), |_| false);
    {
        let s = state.lock().unwrap();
        println!("hydrated {} open order(s):", s.open.len());
        for (oid, perm, st) in &s.open {
            println!("  oid={oid} permId={perm} status={st}");
        }
    }

    let count = doc["orders"].as_array().unwrap().len();
    for i in 0..count {
        let entry = &doc["orders"][i];
        if entry["cancelled"].as_bool().unwrap_or(false) { continue; }
        let label = entry["label"].as_str().unwrap_or("?").to_string();
        let perm = entry["perm_id"].as_i64().unwrap_or(0);
        if perm == 0 {
            eprintln!("[skip] [{label}] has no permId — cannot cancel cross-session");
            continue;
        }
        println!("cancelling [{label}] permId={perm}");
        match client.cancel_order_by_perm_id(perm) {
            Ok(()) => {}
            Err(e) => { eprintln!("  cancel failed for permId={perm}: {e}"); continue; }
        }
        let done = pump_until(&client, &mut w, &state, Duration::from_secs(15), |s| {
            s.statuses.iter().any(|(_, st, p)| *p == perm && st == "Cancelled")
        });
        if done {
            println!("  cancelled [{label}] permId={perm}");
            doc["orders"][i]["cancelled"] = serde_json::Value::Bool(true);
            save_state(&doc); // flush after each — resumable
        } else {
            eprintln!("  WARNING: no Cancelled confirmation for permId={perm} within 15s");
        }
    }

    let remaining = doc["orders"].as_array().unwrap().iter()
        .filter(|e| !e["cancelled"].as_bool().unwrap_or(false)).count();
    println!("\nDone. {remaining} order(s) still open (re-run to retry).");

    client.disconnect();
    Ok(())
}

fn run_verify() -> Result<(), Box<dyn std::error::Error>> {
    let raw = fs::read_to_string(STATE_FILE)
        .map_err(|e| format!("cannot read {STATE_FILE}: {e} (run `send` first)"))?;
    let doc: serde_json::Value = serde_json::from_str(&raw)?;

    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = OrderWrapper { state: state.clone() };

    println!("waiting for open-order hydration...");
    pump_until(&client, &mut w, &state, Duration::from_secs(6), |_| false);
    client.req_all_open_orders(&mut w);
    pump_until(&client, &mut w, &state, Duration::from_secs(2), |_| false);

    let open = state.lock().unwrap().open.clone();
    let mut found = 0;
    for entry in doc["orders"].as_array().unwrap() {
        let label = entry["label"].as_str().unwrap_or("?");
        let perm = entry["perm_id"].as_i64().unwrap_or(0);
        match open.iter().find(|(_, p, _)| *p == perm) {
            Some((oid, _, st)) => {
                println!("  [LIVE] [{label}] permId={perm} oid={oid} status={st}");
                found += 1;
            }
            None => println!("  [MISSING] [{label}] permId={perm} not in open orders"),
        }
    }
    println!("\n{found}/{} order(s) confirmed live on the gateway.", doc["orders"].as_array().unwrap().len());

    client.disconnect();
    Ok(())
}

fn run_purge() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = OrderWrapper { state: state.clone() };

    println!("waiting for open-order hydration...");
    pump_until(&client, &mut w, &state, Duration::from_secs(6), |_| false);
    client.req_all_open_orders(&mut w);
    pump_until(&client, &mut w, &state, Duration::from_secs(2), |_| false);

    let open = state.lock().unwrap().open.clone();
    println!("cancelling {} open order(s):", open.len());
    for (oid, perm, _) in &open {
        println!("cancelling oid={oid} permId={perm}");
        if let Err(e) = client.cancel_order(*oid, "") {
            eprintln!("  cancel failed for oid={oid}: {e}");
            continue;
        }
        let done = pump_until(&client, &mut w, &state, Duration::from_secs(15), |s| {
            s.statuses.iter().any(|(id, st, _)| id == oid && st == "Cancelled")
        });
        println!("  {}", if done { "cancelled" } else { "WARNING: no Cancelled confirmation" });
    }

    client.disconnect();
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let phase = env::args().nth(1).unwrap_or_else(|| "send".into());
    match phase.as_str() {
        "send" => run_send(),
        "verify" => run_verify(),
        "cancel" => run_cancel(),
        "purge" => run_purge(),
        other => {
            eprintln!("unknown phase '{other}' — use `send`, `verify`, `cancel`, or `purge`");
            std::process::exit(2);
        }
    }
}
