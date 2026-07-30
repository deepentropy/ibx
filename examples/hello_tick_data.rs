//! Recipe: stream SPY market data for 5 seconds, print latest bid/ask/last.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_tick_data

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    bid: f64,
    ask: f64,
    last: f64,
    ticks: u64,
}

struct TickWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for TickWrapper {
    fn tick_price(&mut self, _req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        let mut s = self.state.lock().unwrap();
        s.ticks += 1;
        match tick_type {
            1 => s.bid = price,
            2 => s.ask = price,
            4 => s.last = price,
            _ => {}
        }
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        if !matches!(code, 2104 | 2106 | 2158) {
            eprintln!("[error] req_id={req_id} code={code} msg={msg}");
        }
    }
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

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = TickWrapper { state: state.clone() };

    let spy = Contract {
        con_id: 756733,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };

    let req_id = 1;
    println!("streaming SPY for 5s…");
    client.req_mkt_data(req_id, &spy, "", false, false)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(Duration::from_millis(20));
    }

    client.cancel_mkt_data(req_id)?;

    let s = state.lock().unwrap();
    println!("ticks: {}  bid={:.2}  ask={:.2}  last={:.2}", s.ticks, s.bid, s.ask, s.last);

    drop(s);
    client.disconnect();
    Ok(())
}
