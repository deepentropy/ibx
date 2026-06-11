//! Preview the margin impact of an order without placing it ("what-if").
//!
//! Setting `Order.what_if = true` asks the broker to return the order's margin
//! and commission impact instead of transmitting it. The preview arrives on the
//! `open_order` callback as an `OrderState` whose margin fields are populated
//! (init / maintenance margin and equity-with-loan, each before / change /
//! after), followed by a single `PreSubmitted` status. Nothing reaches the book.
//!
//! This example resolves AXTI, reads a reference price, then runs a what-if for
//! a 100-share BUY LMT and prints the margin delta.
//!
//! Usage:
//!   IB_USERNAME=... IB_PASSWORD=... cargo run --example account_whatif [qty]

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::{ContractDetails, OrderState, TickAttrib};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    con_id: i64,
    min_tick: f64,
    last: f64,
    close: f64,
    ask: f64,
    /// what-if margin preview, captured from open_order.
    preview: Option<OrderState>,
}

struct WhatIfWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for WhatIfWrapper {
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
            2 | 67 => s.ask = price,   // ASK / DELAYED_ASK
            4 | 68 => s.last = price,  // LAST / DELAYED_LAST
            9 | 75 => s.close = price, // CLOSE / DELAYED_CLOSE
            _ => {}
        }
    }
    fn open_order(&mut self, _order_id: i64, _c: &Contract, _o: &Order, st: &OrderState) {
        self.state.lock().unwrap().preview = Some(st.clone());
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        // 2104/2106/2158 are benign farm-connection notices.
        if !matches!(code, 2104 | 2106 | 2158) {
            eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        }
    }
}

fn pump_until<F: Fn(&State) -> bool>(
    client: &EClient, wrapper: &mut WhatIfWrapper, state: &Arc<Mutex<State>>,
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

fn round_to_tick(price: f64, min_tick: f64) -> f64 {
    if min_tick <= 0.0 { return price; }
    (price / min_tick).round() * min_tick
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let qty: f64 = env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(100.0);

    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = WhatIfWrapper { state: state.clone() };
    let mut contract = axti();

    // 1) Resolve the contract.
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

    // 2) Read a reference price for a realistic limit.
    println!("requesting quote...");
    client.req_mkt_data(2, &contract, "", true, false)?;
    pump_until(&client, &mut w, &state, Duration::from_secs(10), |s| s.last > 0.0 || s.ask > 0.0);
    let (last, close, ask) = {
        let s = state.lock().unwrap();
        (s.last, s.close, s.ask)
    };
    client.cancel_mkt_data(2).ok();
    let reference = if last > 0.0 { last } else if ask > 0.0 { ask } else { close };
    if reference <= 0.0 {
        return Err("no reference price for AXTI — market data unavailable".into());
    }
    let limit = round_to_tick(reference, min_tick);
    println!("  reference={reference} -> limit={limit}");

    // 3) Send the what-if order. It is NOT transmitted — only priced.
    let oid = client.next_order_id();
    let order = Order {
        action: "BUY".into(),
        total_quantity: qty,
        order_type: "LMT".into(),
        lmt_price: limit,
        tif: "DAY".into(),
        what_if: true,
        ..Default::default()
    };
    println!("what-if: BUY {qty} AXTI LMT {limit} oid={oid}");
    client.place_order(oid, &contract, &order)?;

    let got = pump_until(&client, &mut w, &state, Duration::from_secs(20), |s| s.preview.is_some());
    if !got {
        return Err("no what-if margin response within 20s".into());
    }

    let p = state.lock().unwrap().preview.clone().unwrap();
    println!("\n── Margin what-if (BUY {qty} AXTI @ {limit}) ──");
    println!("  initial margin:  before={:>12}  change={:>12}  after={:>12}",
        p.init_margin_before, p.init_margin_change, p.init_margin_after);
    println!("  maint   margin:  before={:>12}  change={:>12}  after={:>12}",
        p.maint_margin_before, p.maint_margin_change, p.maint_margin_after);
    println!("  equity w/ loan:  before={:>12}  change={:>12}  after={:>12}",
        p.equity_with_loan_before, p.equity_with_loan_change, p.equity_with_loan_after);
    println!("  commission & fees: {:.2}", p.commission_and_fees);
    println!("\n(no order was placed — what-if only)");

    client.disconnect();
    Ok(())
}
