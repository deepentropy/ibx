//! ibx#674 probe through the Rust client, in Toronto's regular hours: RY on
//! TSE, which the paper account cannot get in real time for the API. In
//! delayed mode (type 3): a snapshot, then a stream; then a real-time
//! request with generic ticks. Run with `IBX_LOG_DIR` and
//! `IBX_LOG_LEVEL=debug` to read the farm's rejects.
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
}

impl Log {
    fn line(&self, text: String) {
        println!("{:9.3} {text}", self.t0.elapsed().as_secs_f64());
    }
}

impl Wrapper for Log {
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        self.line(format!("tickPrice {req_id} {tick_type} {price}"));
    }
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
        self.line(format!("tickSize {req_id} {tick_type} {size}"));
    }
    fn tick_snapshot_end(&mut self, req_id: i64) {
        self.line(format!("tickSnapshotEnd {req_id}"));
    }
    fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {
        self.line(format!("marketDataType {req_id} {market_data_type}"));
    }
    fn tick_req_params(&mut self, ticker_id: i64, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i64) {
        self.line(format!("tickReqParams {ticker_id} {min_tick} {bbo_exchange} {snapshot_permissions}"));
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if !matches!(code, 2104 | 2106 | 2107 | 2108 | 2119 | 2158) {
            self.line(format!("error {req_id} {code} {text}"));
        }
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
    let mut w = Log { t0: Instant::now() };
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let ry = Contract {
        symbol: "RY".into(), sec_type: "STK".into(), currency: "CAD".into(),
        // SMART (listed on TSE) by default; RY_EXCHANGE=TSE directs it.
        exchange: env::var("RY_EXCHANGE").unwrap_or_else(|_| "SMART".into()),
        primary_exchange: "TSE".into(), ..Default::default()
    };
    client.req_market_data_type(3);
    w.line("---- type 3 snapshot".into());
    client.req_mkt_data(816, &ry, "", true, false)?;
    pump(&client, &mut w, 12.0);
    w.line("---- type 3 stream".into());
    client.req_mkt_data(811, &ry, "", false, false)?;
    pump(&client, &mut w, 6.0);
    client.cancel_mkt_data(811)?;
    client.req_market_data_type(1);
    w.line("---- type 1 stream, 233,236".into());
    client.req_mkt_data(813, &ry, "233,236", false, false)?;
    pump(&client, &mut w, 6.0);
    client.cancel_mkt_data(813)?;
    pump(&client, &mut w, 1.0);
    client.disconnect();
    Ok(())
}
