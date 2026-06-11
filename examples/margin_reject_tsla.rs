//! Submit a TSLA order sized past available margin and capture the rejection.
//!
//! The size is derived, not guessed: a 1-share `what_if` returns the per-share
//! initial margin and the account's equity-with-loan (available funds). The
//! real order is then sized to ~2x available margin so the broker's pre-submit
//! margin check rejects it. The limit is priced below the market so the order
//! is non-marketable and cannot fill — only the margin check fires.
//!
//! Usage:
//!   IB_USERNAME=... IB_PASSWORD=... cargo run --example margin_reject_tsla
//!
//! Paper account only. Nothing fills; the order is expected to be rejected.

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
    preview: Option<OrderState>,
    /// (order_id, status) from order_status.
    statuses: Vec<(i64, String)>,
    /// (order_id, code, msg) from error.
    errors: Vec<(i64, i64, String)>,
}

struct RejectWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for RejectWrapper {
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
            2 | 67 => s.ask = price,
            4 | 68 => s.last = price,
            9 | 75 => s.close = price,
            _ => {}
        }
    }
    fn open_order(&mut self, _order_id: i64, _c: &Contract, _o: &Order, st: &OrderState) {
        self.state.lock().unwrap().preview = Some(st.clone());
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg: f64, _perm: i64, _parent: i64, _last: f64, _cid: i64, _why: &str, _cap: f64,
    ) {
        println!("[status] oid={order_id} status={status}");
        self.state.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        if matches!(code, 2104 | 2106 | 2158) {
            return; // benign farm-connection notices
        }
        println!("[error] oid/req={req_id} code={code} msg={msg}");
        self.state.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn pump_until<F: Fn(&State) -> bool>(
    client: &EClient, wrapper: &mut RejectWrapper, state: &Arc<Mutex<State>>,
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

fn tsla() -> Contract {
    Contract {
        symbol: "TSLA".into(),
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
    let client = connect()?;
    let state = Arc::new(Mutex::new(State::default()));
    let mut w = RejectWrapper { state: state.clone() };
    let mut contract = tsla();

    // 1) Resolve TSLA.
    println!("resolving TSLA...");
    client.req_contract_details(1, &contract)?;
    if !pump_until(&client, &mut w, &state, Duration::from_secs(15), |s| s.con_id != 0) {
        return Err("no contract details for TSLA within 15s".into());
    }
    let (con_id, min_tick) = {
        let s = state.lock().unwrap();
        (s.con_id, s.min_tick)
    };
    contract.con_id = con_id;
    println!("  conId={con_id} minTick={min_tick}");

    // 2) Reference price → a non-marketable BUY limit 5% below market.
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
        return Err("no reference price for TSLA — market data unavailable".into());
    }
    let limit = round_to_tick(reference * 0.95, min_tick);
    println!("  reference={reference} -> non-marketable limit={limit}");

    // 3) what-if a single share to learn per-share margin + available funds.
    let probe_oid = client.next_order_id();
    let probe = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: limit, tif: "DAY".into(), what_if: true, ..Default::default()
    };
    client.place_order(probe_oid, &contract, &probe)?;
    if !pump_until(&client, &mut w, &state, Duration::from_secs(20), |s| s.preview.is_some()) {
        return Err("no what-if margin response within 20s".into());
    }
    let p = state.lock().unwrap().preview.take().unwrap();
    let per_share: f64 = p.init_margin_change.parse().unwrap_or(0.0);
    let available: f64 = p.equity_with_loan_before.parse().unwrap_or(0.0);
    println!("  per-share initial margin={per_share:.2}  available (equity w/ loan)={available:.2}");
    if per_share <= 0.0 || available <= 0.0 {
        return Err("what-if returned no usable margin figures".into());
    }

    // 4) Size to ~2x available margin so the pre-submit check must reject it.
    let qty = ((available / per_share) * 2.0).ceil();
    let notional = qty * limit;
    println!("  sizing {qty} shares (~2x available margin, notional ~${notional:.0})");

    // 5) Submit the real (non-what-if) order and capture the outcome.
    let oid = client.next_order_id();
    let order = Order {
        action: "BUY".into(), total_quantity: qty, order_type: "LMT".into(),
        lmt_price: limit, tif: "DAY".into(), ..Default::default()
    };
    println!("submitting BUY {qty} TSLA LMT {limit} oid={oid}");
    client.place_order(oid, &contract, &order)?;

    let resolved = pump_until(&client, &mut w, &state, Duration::from_secs(20), |s| {
        s.errors.iter().any(|(id, _, _)| *id == oid)
            || s.statuses.iter().any(|(id, st)| *id == oid && matches!(st.as_str(), "Rejected" | "Cancelled" | "Inactive" | "Submitted" | "Filled"))
    });

    let s = state.lock().unwrap();
    let rejected = s.errors.iter().any(|(id, _, _)| *id == oid)
        || s.statuses.iter().any(|(id, st)| *id == oid && matches!(st.as_str(), "Rejected" | "Inactive"));
    let live = s.statuses.iter().any(|(id, st)| *id == oid && matches!(st.as_str(), "Submitted" | "PreSubmitted" | "Filled"));
    drop(s);

    println!();
    if rejected {
        println!("RESULT: order rejected as expected (insufficient margin).");
    } else if live {
        // Should not happen given the sizing, but never leave an oversized order resting.
        eprintln!("RESULT: order was accepted (unexpected) — cancelling to stay flat.");
        client.cancel_order(oid, "").ok();
        pump_until(&client, &mut w, &state, Duration::from_secs(10), |s| {
            s.statuses.iter().any(|(id, st)| *id == oid && st == "Cancelled")
        });
    } else if !resolved {
        eprintln!("RESULT: no terminal status within 20s.");
    }

    client.disconnect();
    Ok(())
}
