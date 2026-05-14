//! ibx#179 — place → restart → cancel wire capture.
//!
//! Run in two phases against the paper account, with `RUST_LOG=trace` to dump
//! every outbound/inbound FIX message. Phase A places, persists state, exits.
//! Phase B reads state, attempts cancel against the prior session's order_id,
//! logs whatever CCP returns (cancel-ack, 35=3 reject, 35=j business reject).
//!
//! Phase A:
//!   $env:RUST_LOG="trace"; $env:IB_USERNAME="..."; $env:IB_PASSWORD="..."
//!   cargo run --release --example ex179_wire_repro -- place 2> .tmp/ex179_a.log
//!
//! Phase B (fresh process):
//!   cargo run --release --example ex179_wire_repro -- cancel 2> .tmp/ex179_b.log
//!
//! State file: .tmp/ex179_state.txt (order_id=, perm_id=, status=).

use std::env;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::OrderState;
use ibx::api::wrapper::Wrapper;

const STATE_PATH: &str = ".tmp/ex179_state.txt";

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String, i64)>,
    errors: Vec<(i64, i64, String)>,
    open_orders: Vec<(i64, String, String)>,
}

struct W { state: Arc<Mutex<State>> }

impl Wrapper for W {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill: f64, perm_id: i64, _parent_id: i64, _last_fill: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[status] oid={order_id} perm={perm_id} status={status}");
        self.state.lock().unwrap().statuses.push((order_id, status.into(), perm_id));
    }
    fn open_order(
        &mut self, order_id: i64, contract: &Contract,
        order: &Order, _state: &OrderState,
    ) {
        println!("[open_order] oid={order_id} sym={} action={} qty={}",
                 contract.symbol, order.action, order.total_quantity);
        self.state.lock().unwrap().open_orders.push((
            order_id, contract.symbol.clone(), order.action.clone(),
        ));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        self.state.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn pump<F: Fn(&State) -> bool>(c: &EClient, w: &mut W, s: &Arc<Mutex<State>>,
                                t: Duration, done: F) -> bool {
    let dl = Instant::now() + t;
    while Instant::now() < dl {
        c.process_msgs(w);
        if done(&s.lock().unwrap()) { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn connect() -> Result<EClient, Box<dyn std::error::Error>> {
    Ok(EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?,
        password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true,
        core_id: None,
    })?)
}

fn spy() -> Contract {
    Contract {
        con_id: 756733,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn write_state(order_id: i64, perm_id: i64, status: &str) -> std::io::Result<()> {
    fs::create_dir_all(".tmp")?;
    fs::write(STATE_PATH, format!("order_id={order_id}\nperm_id={perm_id}\nstatus={status}\n"))
}

fn read_state() -> Result<(i64, i64, String), Box<dyn std::error::Error>> {
    let text = fs::read_to_string(STATE_PATH)?;
    let mut order_id = 0i64; let mut perm_id = 0i64; let mut status = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("order_id=") { order_id = v.parse()?; }
        else if let Some(v) = line.strip_prefix("perm_id=") { perm_id = v.parse()?; }
        else if let Some(v) = line.strip_prefix("status=") { status = v.into(); }
    }
    Ok((order_id, perm_id, status))
}

fn phase_place() -> Result<(), Box<dyn std::error::Error>> {
    let client = connect()?;
    let order_id = client.next_order_id();
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = W { state: state.clone() };

    let order = Order {
        action: "BUY".into(), total_quantity: 1.0,
        order_type: "LMT".into(), lmt_price: 1.00,
        tif: "DAY".into(), outside_rth: true,
        ..Default::default()
    };

    println!("=== PHASE A: place oid={order_id} BUY 1 SPY LMT 1.00 ===");
    client.place_order(order_id, &spy(), &order)?;

    let got = pump(&client, &mut w, &state, Duration::from_secs(15), |s| {
        s.statuses.iter().any(|(id, st, _)| *id == order_id
            && (st == "Submitted" || st == "PreSubmitted"))
    });
    if !got { eprintln!("WARN: never saw Submitted/PreSubmitted within 15s"); }

    let snap = state.lock().unwrap();
    let (final_status, perm) = snap.statuses.iter()
        .filter(|(id, _, _)| *id == order_id)
        .last()
        .map(|(_, s, p)| (s.clone(), *p))
        .unwrap_or_else(|| ("Unknown".into(), 0));
    write_state(order_id, perm, &final_status)?;
    println!("=== PHASE A: persisted oid={order_id} perm={perm} status={final_status} ===");
    println!("=== Now disconnect, restart, run phase B ===");

    client.disconnect();
    Ok(())
}

fn phase_cancel() -> Result<(), Box<dyn std::error::Error>> {
    let (order_id, perm_id, prior_status) = read_state()?;
    println!("=== PHASE B: read oid={order_id} perm={perm_id} prior={prior_status} ===");

    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = W { state: state.clone() };

    // Drain any unsolicited inbound for 2s before cancelling — gives the
    // gateway time to push openOrder/35=8 for prior-session orders if it does.
    println!("=== PHASE B: draining inbound for 2s ===");
    pump(&client, &mut w, &state, Duration::from_secs(2), |_| false);

    println!("=== PHASE B: cancel oid={order_id} ===");
    client.cancel_order(order_id, "")?;

    pump(&client, &mut w, &state, Duration::from_secs(10), |s| {
        s.statuses.iter().any(|(id, st, _)| *id == order_id && st == "Cancelled")
            || s.errors.iter().any(|(rid, _, _)| *rid == order_id)
    });

    let snap = state.lock().unwrap();
    println!("=== PHASE B: summary ===");
    for (id, st, p) in &snap.statuses { println!("  status oid={id} perm={p} status={st}"); }
    for (rid, c, m) in &snap.errors { println!("  error req={rid} code={c} msg={m}"); }
    for (id, sym, act) in &snap.open_orders { println!("  open_order oid={id} sym={sym} act={act}"); }

    client.disconnect();
    Ok(())
}

fn load_dotenv() {
    if let Ok(text) = fs::read_to_string(".env") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if env::var_os(k).is_none() {
                    unsafe { env::set_var(k, v); }
                }
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    load_dotenv();

    let mode = env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "place" => phase_place(),
        "cancel" => phase_cancel(),
        _ => {
            eprintln!("usage: ex179_wire_repro <place|cancel>");
            eprintln!("state file: {}", Path::new(STATE_PATH).display());
            std::process::exit(2);
        }
    }
}
