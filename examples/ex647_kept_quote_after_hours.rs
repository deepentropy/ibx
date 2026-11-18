//! ibx#647 probe through the Rust client, after hours: an AAPL option given
//! by its full description, three rounds of a 12 s stream then a snapshot.
//! From the second round the stream starts with the bid and ask the
//! contract kept (-1 after hours); the reference then gives the farm's own
//! sizes of 0. Run with `IBX_LOG_DIR` and `IBX_LOG_LEVEL=debug` to read the
//! farm messages of each round.
//!
//! `IBX647_STRIKES` gives the strike of each round (default 340,340,340);
//! another strike of the same underlying shows where the second option's
//! entries go (the reference keeps one of them on the first option).
//!
//! Prints every market data callback with its time from the start.
//! It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, TickAttrib};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
    ended: bool,
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

impl Wrapper for Log {
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {
        self.line(format!("tickPrice {req_id} {tick_type} {price} auto={}", attrib.can_auto_execute));
    }
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
        self.line(format!("tickSize {req_id} {tick_type} {size}"));
    }
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {
        self.line(format!("tickString {req_id} {tick_type} {value}"));
    }
    fn tick_option_computation(
        &mut self, req_id: i64, tick_type: i32, _: i32, implied_vol: f64, delta: f64, opt_price: f64, _: f64,
        _: f64, _: f64, _: f64, und_price: f64,
    ) {
        self.line(format!("tickOptionComputation {req_id} {tick_type} vol={implied_vol} delta={delta} price={opt_price} und={und_price}"));
    }
    fn tick_snapshot_end(&mut self, req_id: i64) {
        self.ended = true;
        self.line(format!("tickSnapshotEnd {req_id}"));
    }
    fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {
        self.line(format!("marketDataType {req_id} {market_data_type}"));
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        self.line(format!("error {req_id} {code} {}", text.chars().take(90).collect::<String>()));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let mut w = Log { t0: Instant::now(), ended: false };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let strikes: Vec<f64> = env::var("IBX647_STRIKES").unwrap_or_else(|_| "340,340,340".into())
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    for (round, &strike) in (0i64..).zip(&strikes) {
        let option = Contract {
            symbol: "AAPL".into(), sec_type: "OPT".into(), exchange: "SMART".into(), currency: "USD".into(),
            last_trade_date_or_contract_month: "20261218".into(), strike, right: "C".into(),
            multiplier: "100".into(), ..Default::default()
        };
        let stream = 211 + 10 * round;
        w.line(format!("---- round {} stream {stream} strike {strike}", round + 1));
        client.req_mkt_data(stream, &option, "", false, false)?;
        pump(&client, &mut w, 12.0);
        client.cancel_mkt_data(stream)?;
        pump(&client, &mut w, 1.0);
        let snapshot = stream + 1;
        w.line(format!("---- round {} snapshot {snapshot}", round + 1));
        w.ended = false;
        client.req_mkt_data(snapshot, &option, "", true, false)?;
        let until = Instant::now() + Duration::from_secs(14);
        while !w.ended && Instant::now() < until {
            pump(&client, &mut w, 0.1);
        }
        pump(&client, &mut w, 1.0);
    }
    client.disconnect();
    Ok(())
}
