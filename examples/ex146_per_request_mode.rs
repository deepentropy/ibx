//! Issue #146 — per-request market-data mode (FIX field 9887).
//!
//! Subscribes to a thinly-traded ticker with parallel realtime + frozen subs
//! and prints which feed delivers ticks. Frozen sub keeps streaming after-hours
//! when realtime is silent.
//!
//! Usage:
//!   IB_USERNAME=user IB_PASSWORD=pass cargo run --example ex146_per_request_mode

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig};
use ibx::api::types::TickAttrib;
use ibx::api::wrapper::Wrapper;

#[derive(Default)]
struct Counts {
    realtime_ticks: u64,
    frozen_ticks: u64,
    delayed_frozen_ticks: u64,
}

struct PrintWrapper {
    counts: Arc<Mutex<Counts>>,
}

impl Wrapper for PrintWrapper {
    fn tick_price(&mut self, req_id: i64, _tick_type: i32, price: f64, _attrib: &TickAttrib) {
        let mut c = self.counts.lock().unwrap();
        let label = match req_id {
            1 => { c.realtime_ticks += 1; "realtime" }
            2 => { c.frozen_ticks += 1; "frozen" }
            3 => { c.delayed_frozen_ticks += 1; "delayed_frozen" }
            _ => "?",
        };
        println!("[{:>14}] req_id={} price={:.4}", label, req_id, price);
    }

    fn tick_size(&mut self, req_id: i64, _tick_type: i32, size: f64) {
        let mut c = self.counts.lock().unwrap();
        let label = match req_id {
            1 => { c.realtime_ticks += 1; "realtime" }
            2 => { c.frozen_ticks += 1; "frozen" }
            3 => { c.delayed_frozen_ticks += 1; "delayed_frozen" }
            _ => "?",
        };
        println!("[{:>14}] req_id={} size={:.0}", label, req_id, size);
    }

    fn market_data_type(&mut self, req_id: i64, mdt: i32) {
        println!("[market_data_type] req_id={} mdt={}", req_id, mdt);
    }

    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    let symbol = env::var("SYMBOL").unwrap_or_else(|_| "AAPL".to_string());
    let con_id: i64 = env::var("CON_ID").unwrap_or_else(|_| "265598".to_string()).parse()?;
    let duration: u64 = env::var("DURATION_SECS").unwrap_or_else(|_| "20".to_string()).parse()?;

    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None, code_provider: None,
    })?;

    let contract = Contract { con_id, symbol: symbol.clone(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };

    println!("Subscribing to {} (con_id={}) on 3 parallel feeds...", symbol, con_id);
    client.req_mkt_data_ex(1, &contract, "", false, false, 0)?;  // realtime
    client.req_mkt_data_ex(2, &contract, "", false, false, 2)?;  // frozen
    client.req_mkt_data_ex(3, &contract, "", false, false, 3)?;  // delayed_frozen

    let counts = Arc::new(Mutex::new(Counts::default()));
    let mut wrapper = PrintWrapper { counts: counts.clone() };

    let deadline = Instant::now() + Duration::from_secs(duration);
    while Instant::now() < deadline {
        client.process_msgs(&mut wrapper);
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = client.cancel_mkt_data(1);
    let _ = client.cancel_mkt_data(2);
    let _ = client.cancel_mkt_data(3);
    client.disconnect();

    let c = counts.lock().unwrap();
    println!("\n── Tick counts after {}s ──", duration);
    println!("  realtime       : {}", c.realtime_ticks);
    println!("  frozen         : {}", c.frozen_ticks);
    println!("  delayed_frozen : {}", c.delayed_frozen_ticks);

    Ok(())
}
