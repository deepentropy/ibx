//! ibx#247 ibx#324 ibx#334 ibx#349 — verify order replaces on a paper account.
//!
//! For each case: place an order far from the market, then place it again
//! with the same id and a changed price, stop, trail, time-in-force or
//! outside-RTH flag. Pass = the replace is accepted (no error for that id
//! and the order is working again). A change of order type must be refused
//! before sending, with error 329, like the reference client. Every order is
//! cancelled at the end.
//!
//! Run with `RUST_LOG=ibx::protocol::connection=trace,ibx::engine::hot_loop::ccp=trace`
//! to see the replace sent and the server's confirmation.
//!
//! Run: cargo run --example ex247_modify_replace
//! Needs IB_USERNAME / IB_PASSWORD (paper) and optionally IB_HOST.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

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
        con_id: 265598,
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn working(s: &str) -> bool {
    matches!(s, "PreSubmitted" | "Submitted")
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

/// Place `first`, wait until it works, then place `second` with the same id.
fn run_case(
    client: &EClient, wrapper: &mut ProbeWrapper, state: &Arc<Mutex<State>>,
    label: &str, id: i64, first: Order, second: Order, expect_refused: bool,
) -> bool {
    println!("\n== {} (order {})", label, id);
    if let Err(e) = client.place_order(id, &aapl(), &first) {
        println!("  place failed: {}", e);
        return false;
    }
    let st = state.clone();
    let up = pump(client, wrapper, 15, || {
        st.lock().unwrap().statuses.iter().any(|(o, s)| *o == id && working(s))
    });
    if !up {
        println!("  FAIL: original order not working");
        return false;
    }
    let before = state.lock().unwrap().statuses.len();
    std::thread::sleep(Duration::from_millis(500));
    if let Err(e) = client.place_order(id, &aapl(), &second) {
        println!("  modify failed: {}", e);
        return false;
    }
    let st = state.clone();
    if expect_refused {
        let refused = pump(client, wrapper, 5, || {
            st.lock().unwrap().errors.iter().any(|(r, c, _)| *r == id && *c == 329)
        });
        println!("  {}", if refused { "refused with 329 as expected" } else { "FAIL: no 329" });
        return refused;
    }
    // Accepted = no error for this id and a working status after the modify.
    pump(client, wrapper, 10, || false);
    let s = state.lock().unwrap();
    let err = s.errors.iter().find(|(r, _, _)| *r == id);
    let after: Vec<&String> = s.statuses[before..].iter().filter(|(o, _)| *o == id).map(|(_, st)| st).collect();
    let ok = err.is_none() && after.last().map(|x| working(x)).unwrap_or(true);
    println!("  statuses after modify: {:?}; error: {:?} -> {}", after, err, if ok { "PASS" } else { "FAIL" });
    ok
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
    let lmt = |action: &str, px: f64, rth: bool, tif: &str| Order {
        action: action.into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: px, outside_rth: rth, tif: tif.into(), ..Default::default()
    };
    let stp = |aux: f64| Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: aux, ..Default::default()
    };
    let stp_lmt = |l: f64, aux: f64| Order {
        action: "SELL".into(), order_type: "STP LMT".into(), total_quantity: 1.0,
        lmt_price: l, aux_price: aux, ..Default::default()
    };
    let trail = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL".into(), total_quantity: 1.0, aux_price: aux, ..Default::default()
    };
    let trail_lmt = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL LIMIT".into(), total_quantity: 1.0,
        aux_price: aux, lmt_price_offset: 0.50, ..Default::default()
    };
    let gtd_stp = |aux: f64| Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: aux,
        tif: "GTD".into(), good_till_date: "20261230 16:00:00 US/Eastern".into(), ..Default::default()
    };

    let cases: Vec<(&str, Order, Order, bool)> = vec![
        ("A1a LMT price, outside-RTH off", lmt("BUY", 200.0, false, "DAY"), lmt("BUY", 201.0, false, "DAY"), false),
        ("A1b LMT price, outside-RTH on", lmt("BUY", 200.0, true, "DAY"), lmt("BUY", 201.0, true, "DAY"), false),
        ("A2a STP trigger", stp(200.0), stp(195.0), false),
        ("A2b STP LMT both prices", stp_lmt(194.0, 195.0), stp_lmt(189.0, 190.0), false),
        ("A3a TRAIL amount", trail(100.0), trail(110.0), false),
        ("A3b TRAIL LIMIT amount", trail_lmt(100.0), trail_lmt(110.0), false),
        // A percent trail replace is encoded and unit-tested, but a percent
        // trail cannot be placed on the server until ibx#339 is fixed.
        ("A4a LMT DAY -> GTC", lmt("BUY", 200.0, false, "DAY"), lmt("BUY", 200.0, false, "GTC"), false),
        ("A4b LMT -> STP (refused)", lmt("BUY", 200.0, false, "DAY"), stp(195.0), true),
        ("A5 GTD STP trigger", gtd_stp(200.0), gtd_stp(195.0), false),
    ];

    let mut results = Vec::new();
    let mut ids = Vec::new();
    for (i, (label, first, second, refused)) in cases.into_iter().enumerate() {
        let id = base + i as i64;
        ids.push(id);
        let ok = run_case(&client, &mut wrapper, &state, label, id, first, second, refused);
        results.push((label, id, ok));
    }

    println!("\n== Cleanup: cancelling every order");
    for &id in &ids {
        let _ = client.cancel_order(id, "");
    }
    pump(&client, &mut wrapper, 8, || false);

    println!("\n== RESULTS");
    for (label, id, ok) in &results {
        println!("  {:<34} {}  {}", label, id, if *ok { "PASS" } else { "FAIL" });
    }
    let all = results.iter().all(|(_, _, ok)| *ok);
    println!("== RESULT: {}", if all { "PASS" } else { "FAIL" });
    client.disconnect();
    if all { Ok(()) } else { Err("modify live check failed".into()) }
}
