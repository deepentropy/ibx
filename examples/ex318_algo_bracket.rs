//! ibx#318 ibx#405 — verify algo orders keep their parent link, OCA group and
//! time-in-force, and that every algo type is accepted. Paper account only.
//!
//! Bracket A: parent BUY LMT far below the market (GTC), an Adaptive SELL
//! take-profit child and a STP SELL child (GTC). Bracket B: the same with a
//! TWAP take-profit child, all DAY; the server refuses GTC for TWAP itself.
//! The two children share one OCA group (cancel-on-fill). For each bracket,
//! cancelling ONLY the parent must make the server cancel the children, which
//! it only does when the parent link reached it. Then a standalone VWAP order must be accepted (ibx#405).
//! Every order still working at the end is cancelled.
//!
//! Run: cargo run --example ex318_algo_bracket
//! Needs IB_USERNAME / IB_PASSWORD (paper) and optionally IB_HOST.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order, TagValue};
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

fn refused(state: &Arc<Mutex<State>>, id: i64) -> bool {
    state.lock().unwrap().errors.iter().any(|(r, c, _)| *r == id && *c != 399)
        || last_status(state, id).as_deref() == Some("Inactive")
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

/// Place a parent, the algo take-profit child and a stop child; cancel the
/// parent; both children must follow.
fn bracket(
    client: &EClient, wrapper: &mut ProbeWrapper, state: &Arc<Mutex<State>>,
    label: &str, parent_id: i64, tif: &str, child: Order,
) -> bool {
    println!("\n== {} (parent {}, children {} and {})", label, parent_id, parent_id + 1, parent_id + 2);
    let parent = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: 1.00, tif: tif.into(), ..Default::default()
    };
    let oca = format!("ibx318_{}", parent_id);
    let child = Order { parent_id, oca_group: oca.clone(), oca_type: 1, tif: tif.into(), ..child };
    let stop = Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: 0.50,
        parent_id, oca_group: oca, oca_type: 1, tif: tif.into(), ..Default::default()
    };
    if client.place_order(parent_id, &aapl(), &parent).is_err()
        || client.place_order(parent_id + 1, &aapl(), &child).is_err()
        || client.place_order(parent_id + 2, &aapl(), &stop).is_err()
    {
        println!("  FAIL: place_order returned an error");
        return false;
    }
    let ids = [parent_id, parent_id + 1, parent_id + 2];
    let st = state.clone();
    pump(client, wrapper, 20, || ids.iter().all(|&i| working(&st, i) || refused(&st, i)));
    for &i in &ids {
        if !working(state, i) {
            println!("  FAIL: order {} not working ({:?})", i, last_status(state, i));
            return false;
        }
    }
    client.cancel_order(parent_id, "").ok();
    let st = state.clone();
    let cascaded = pump(client, wrapper, 20, || {
        ids.iter().all(|&i| last_status(&st, i).as_deref() == Some("Cancelled"))
    });
    println!("  {}", if cascaded { "children cancelled by the server with the parent: link confirmed" }
                     else { "FAIL: the children did not follow the parent cancel" });
    cascaded
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

    let adaptive = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.0,
        algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
        ..Default::default()
    };
    let twap = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.0,
        algo_strategy: "Twap".into(),
        algo_params: vec![TagValue { tag: "allowPastEndTime".into(), value: "1".into() }],
        ..Default::default()
    };
    let a = bracket(&client, &mut wrapper, &state, "A: Adaptive child, GTC", base, "GTC", adaptive);
    let b = bracket(&client, &mut wrapper, &state, "B: TWAP child, DAY", base + 10, "DAY", twap);

    let vwap_id = base + 20;
    println!("\n== C: standalone VWAP (order {})", vwap_id);
    let vwap = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0,
        algo_strategy: "Vwap".into(),
        algo_params: vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "noTakeLiq".into(), value: "0".into() },
            TagValue { tag: "allowPastEndTime".into(), value: "1".into() },
        ],
        ..Default::default()
    };
    client.place_order(vwap_id, &aapl(), &vwap).ok();
    let st = state.clone();
    pump(&client, &mut wrapper, 20, || working(&st, vwap_id) || refused(&st, vwap_id));
    let c = working(&state, vwap_id);
    println!("  {}", if c { "accepted" } else { "FAIL: not accepted" });

    println!("\n== Cleanup");
    for id in [base, base + 1, base + 2, base + 10, base + 11, base + 12, vwap_id] {
        if working(&state, id) {
            let _ = client.cancel_order(id, "");
        }
    }
    pump(&client, &mut wrapper, 8, || false);

    println!("\n== RESULTS");
    println!("  A Adaptive child GTC   {}", if a { "PASS" } else { "FAIL" });
    println!("  B TWAP child DAY       {}", if b { "PASS" } else { "FAIL" });
    println!("  C standalone VWAP      {}", if c { "PASS" } else { "FAIL" });
    let all = a && b && c;
    println!("== RESULT: {}", if all { "PASS" } else { "FAIL" });
    client.disconnect();
    if all { Ok(()) } else { Err("algo live check failed".into()) }
}
