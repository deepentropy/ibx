//! ibx#511 probe through the Rust client: the first P&L requests of a
//! session, as case A of the reference run of 15/10/2026. reqPnL and
//! reqPnLSingle on AAPL, their cancel after 12 s, then a second reqPnL for
//! 5 s. Read-only: no order. Run with `IBX_LOG_DIR` and
//! `IBX_LOG_LEVEL=debug` to read the P&L quotes and the partition's account
//! request in the engine log.
//!
//! Prints the P&L callbacks and their errors with their time from the
//! first request. It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
}

impl Log {
    fn line(&self, text: String) {
        println!("   +{:6} ms  {text}", self.t0.elapsed().as_millis());
    }
}

fn show(v: f64) -> String {
    if v == f64::MAX { "MAX".into() } else { format!("{v}") }
}

impl Wrapper for Log {
    fn pnl(&mut self, req_id: i64, daily: f64, unrealized: f64, realized: f64) {
        self.line(format!("pnl {req_id} {} {} {}", show(daily), show(unrealized), show(realized)));
    }
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily: f64, unrealized: f64, realized: f64, value: f64) {
        self.line(format!("pnlSingle {req_id} {pos} {} {} {} {}", show(daily), show(unrealized), show(realized), show(value)));
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
        client_id: 198,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    })?;
    let account = client.account_id.clone();
    if !account.starts_with("DU") {
        client.disconnect();
        return Err("not a paper account: nothing sent".into());
    }
    let pump = |client: &EClient, w: &mut Log, secs: f64| {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let mut w = Log { t0: Instant::now() };
    pump(&client, &mut w, 3.0);
    w.t0 = Instant::now();
    client.req_pnl(9003, &account, "");
    client.req_pnl_single(9004, &account, "", 265598);
    pump(&client, &mut w, 12.0);
    client.cancel_pnl(9003);
    client.cancel_pnl_single(9004);
    pump(&client, &mut w, 3.0);
    client.req_pnl(9005, &account, "");
    pump(&client, &mut w, 5.0);
    client.cancel_pnl(9005);
    pump(&client, &mut w, 2.0);
    client.disconnect();
    Ok(())
}
