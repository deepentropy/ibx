//! Place a marketable premarket BUY limit on AXTI that fills immediately.
//!
//! A limit order only fills in premarket if (a) `outside_rth` is set so it is
//! eligible outside regular hours, and (b) the limit price crosses the book —
//! for a BUY that means the limit sits at or above the current ask. This
//! example resolves AXTI, reads its live quote, then prices a BUY limit a few
//! percent above the ask so it is marketable the instant it reaches the book.
//!
//! Usage:
//!   IB_USERNAME=... IB_PASSWORD=... cargo run --example premarket_marketable_axti [buy|sell]
//!
//! `buy` (default) opens a 1-share long above the ask; `sell` closes it below
//! the bid. Sends a single 1-share order against the paper account. Run only
//! during the premarket session — outside it the order rests until it opens.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::{ContractDetails, TickAttrib};
use ibx::api::wrapper::Wrapper;

/// Markup past the touch so the marketable limit reliably crosses the book.
const CROSS_MARKUP: f64 = 0.05; // 5%

#[derive(Default)]
struct State {
    con_id: i64,
    min_tick: f64,
    bid: f64,
    ask: f64,
    last: f64,
    close: f64,
    /// (order_id, status, filled, avg_fill, perm_id) in arrival order.
    statuses: Vec<(i64, String, f64, f64, i64)>,
}

struct AxtiWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for AxtiWrapper {
    fn contract_details(&mut self, _req_id: i64, details: &ContractDetails) {
        let mut s = self.state.lock().unwrap();
        s.con_id = details.contract.con_id;
        s.min_tick = details.min_tick;
    }
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _attrib: &TickAttrib) {
        if price <= 0.0 {
            return;
        }
        let mut s = self.state.lock().unwrap();
        match tick_type {
            1 | 66 => s.bid = price,   // BID / DELAYED_BID
            2 | 67 => s.ask = price,   // ASK / DELAYED_ASK
            4 | 68 => s.last = price,  // LAST / DELAYED_LAST
            9 | 75 => s.close = price, // CLOSE / DELAYED_CLOSE
            _ => {}
        }
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, _remaining: f64,
        avg_fill: f64, perm_id: i64, _parent_id: i64, _last_fill: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[status] oid={order_id} status={status} filled={filled} avg={avg_fill} permId={perm_id}");
        self.state.lock().unwrap().statuses.push((order_id, status.into(), filled, avg_fill, perm_id));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        // 2104/2106/2158 are benign farm-connection notices.
        if !matches!(code, 2104 | 2106 | 2158) {
            eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        }
    }
}

fn pump_until<F: Fn(&State) -> bool>(
    client: &EClient, wrapper: &mut AxtiWrapper, state: &Arc<Mutex<State>>,
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

fn axti() -> Contract {
    Contract {
        symbol: "AXTI".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

/// Round a price to the nearest valid tick.
fn round_to_tick(price: f64, min_tick: f64) -> f64 {
    if min_tick <= 0.0 { return price; }
    (price / min_tick).round() * min_tick
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `buy` (default) opens the position above the ask; `sell` closes it below
    // the bid. Both are marketable so they cross and fill immediately.
    let side = env::args().nth(1).unwrap_or_else(|| "buy".into());
    let action = match side.as_str() {
        "buy" => "BUY",
        "sell" => "SELL",
        other => {
            eprintln!("unknown side '{other}' — use `buy` or `sell`");
            std::process::exit(2);
        }
    };

    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = AxtiWrapper { state: state.clone() };
    let mut contract = axti();

    // 1) Resolve the contract to get conId + min_tick.
    println!("resolving AXTI...");
    client.req_contract_details(1, &contract)?;
    if !pump_until(&client, &mut w, &state, Duration::from_secs(15), |s| s.con_id != 0) {
        return Err("no contract details for AXTI within 15s".into());
    }
    let (con_id, min_tick) = {
        let s = state.lock().unwrap();
        (s.con_id, s.min_tick)
    };
    contract.con_id = con_id;
    println!("  conId={con_id} minTick={min_tick}");

    // 2) Read the live quote. Snapshot delivers the first available tick set.
    //    A BUY needs the ask; a SELL needs the bid.
    println!("requesting quote...");
    client.req_mkt_data(2, &contract, "", true, false)?;
    let want_touch = |s: &State| if action == "BUY" { s.ask > 0.0 } else { s.bid > 0.0 };
    pump_until(&client, &mut w, &state, Duration::from_secs(10), want_touch);

    // 3) Pick a reference price: the relevant touch, else last, else close.
    let (bid, ask, last, close) = {
        let s = state.lock().unwrap();
        (s.bid, s.ask, s.last, s.close)
    };
    let touch = if action == "BUY" { ask } else { bid };
    let reference = if touch > 0.0 { touch } else if last > 0.0 { last } else { close };
    if reference <= 0.0 {
        client.cancel_mkt_data(2).ok();
        return Err("no touch/last/close price for AXTI — market data unavailable".into());
    }
    println!("  bid={bid} ask={ask} last={last} close={close} -> reference={reference}");

    // 4) Price past the touch so the limit crosses and fills immediately:
    //    BUY above the ask, SELL below the bid.
    let factor = if action == "BUY" { 1.0 + CROSS_MARKUP } else { 1.0 - CROSS_MARKUP };
    let limit = round_to_tick(reference * factor, min_tick);
    println!("  pricing {action} LMT at {limit} ({:.0}% past reference)", CROSS_MARKUP * 100.0);

    let oid = client.next_order_id();
    let order = Order {
        action: action.into(),
        total_quantity: 1.0,
        order_type: "LMT".into(),
        lmt_price: limit,
        tif: "DAY".into(),
        outside_rth: true, // required for the order to be eligible in premarket
        ..Default::default()
    };
    println!("placing {action} 1 AXTI LMT {limit} oid={oid}");
    client.place_order(oid, &contract, &order)?;

    // 5) Wait for the fill.
    let filled = pump_until(&client, &mut w, &state, Duration::from_secs(20), |s| {
        s.statuses.iter().any(|(id, st, _, _, _)| *id == oid && st == "Filled")
    });

    let s = state.lock().unwrap();
    if let Some((_, st, fq, avg, perm)) = s.statuses.iter().rev().find(|(id, _, _, _, _)| *id == oid) {
        println!("\nfinal: status={st} filled={fq} avg={avg} permId={perm}");
    }
    if filled {
        println!("AXTI {action} filled immediately.");
    } else {
        eprintln!("WARNING: not Filled within 20s — premarket may be closed or liquidity thin.");
    }
    drop(s);

    client.cancel_mkt_data(2).ok();
    client.disconnect();
    Ok(())
}
