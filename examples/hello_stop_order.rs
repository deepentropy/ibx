//! Recipe: place a BUY STP on SPY far above market, watch Submitted, then cancel.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_stop_order

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String)>,
}

struct OrderWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for OrderWrapper {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill: f64, _perm_id: i64, _parent_id: i64, _last_fill: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        println!("[status] oid={order_id} status={status}");
        self.state.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={req_id} code={code} msg={msg}");
    }
}

fn pump_until<F: Fn(&State) -> bool>(
    client: &EClient, wrapper: &mut OrderWrapper, state: &Arc<Mutex<State>>,
    timeout: Duration, done: F,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        client.process_msgs(wrapper);
        if done(&state.lock().unwrap()) { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?,
        password: env::var("IB_PASSWORD")?,
        host: "cdc1.ibllc.com".into(),
        paper: true,
        core_id: None,
        code_provider: None,
    })?;

    let order_id = client.next_order_id();

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = OrderWrapper { state: state.clone() };

    let spy = Contract {
        con_id: 756733,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };
    let order = Order {
        action: "BUY".into(),
        total_quantity: 1.0,
        order_type: "STP".into(),
        aux_price: 9999.0,
        tif: "GTC".into(),
        outside_rth: true,
        ..Default::default()
    };

    println!("placing BUY 1 SPY STP 9999.00 (oid={order_id})");
    client.place_order(order_id, &spy, &order)?;

    pump_until(&client, &mut wrapper, &state, Duration::from_secs(15),
               |s| s.statuses.iter().any(|(id, st)| *id == order_id && st == "Submitted"));

    println!("cancelling oid={order_id}");
    client.cancel_order(order_id, "")?;

    pump_until(&client, &mut wrapper, &state, Duration::from_secs(15),
               |s| s.statuses.iter().any(|(id, st)| *id == order_id && st == "Cancelled"));

    client.disconnect();
    Ok(())
}
