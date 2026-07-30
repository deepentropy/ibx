//! Recipe: fetch 1 day of 5-minute SPY bars, print first/last bar.
//!
//! Usage: IB_USERNAME=... IB_PASSWORD=... cargo run --example hello_bar_data

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::BarData;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct State {
    bars: Vec<BarData>,
    end_seen: bool,
}

struct BarsWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for BarsWrapper {
    fn historical_data(&mut self, _req_id: i64, bar: &BarData) {
        self.state.lock().unwrap().bars.push(bar.clone());
    }
    fn historical_data_end(&mut self, _req_id: i64, _start: &str, _end: &str) {
        self.state.lock().unwrap().end_seen = true;
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={req_id} code={code} msg={msg}");
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
    let mut wrapper = BarsWrapper { state: state.clone() };

    let spy = Contract {
        con_id: 756733,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    };
    client.req_historical_data(1, &spy, "", "1 D", "5 mins", "TRADES", true, 1, false)?;

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        if state.lock().unwrap().end_seen { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    let s = state.lock().unwrap();
    println!("bars: {}", s.bars.len());
    if let (Some(first), Some(last)) = (s.bars.first(), s.bars.last()) {
        println!("  first: {}  O={} H={} L={} C={} V={}",
                 first.date, first.open, first.high, first.low, first.close, first.volume);
        println!("  last : {}  O={} H={} L={} C={} V={}",
                 last.date, last.open, last.high, last.low, last.close, last.volume);
    }

    drop(s);
    client.disconnect();
    Ok(())
}
