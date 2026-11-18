//! ibx#689 probe: the time of an option chain contract-details request (MSFT
//! 20261218 calls, no strike), asked twice. Run with `IBX_LOG_DIR` and
//! `IBX_LOG_LEVEL=debug` to read the lookup on the engine's log.
//!
//! Paper: IB_USERNAME, IB_PASSWORD. Stops unless the account id starts with DU.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, ContractDetails};
use ibx::api::wrapper::Wrapper;

struct Log {
    t0: Instant,
    rows: usize,
    ended: bool,
}

impl Wrapper for Log {
    fn contract_details(&mut self, _req_id: i64, _details: &ContractDetails) {
        self.rows += 1;
    }
    fn contract_details_end(&mut self, req_id: i64) {
        println!("{:9.3} contractDetailsEnd {req_id} rows={}", self.t0.elapsed().as_secs_f64(), self.rows);
        self.ended = true;
    }
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        println!("{:9.3} error {req_id} {code} {text}", self.t0.elapsed().as_secs_f64());
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
    let mut w = Log { t0: Instant::now(), rows: 0, ended: false };
    std::thread::sleep(Duration::from_secs(3));
    for k in 0..2 {
        // CHAIN_ALL: the whole AAPL chain, the underlying only (ibx#689's request).
        let all = env::var_os("CHAIN_ALL").is_some();
        let chain = Contract {
            symbol: if all { "AAPL" } else { "MSFT" }.into(), sec_type: "OPT".into(), exchange: "SMART".into(), currency: "USD".into(),
            last_trade_date_or_contract_month: if all { "" } else { "20261218" }.into(), right: if all { "" } else { "C" }.into(),
            // The official client's unset strike, as a Python program gives it.
            strike: if env::var_os("STRIKE_MAX").is_some() { f64::MAX } else { 0.0 },
            ..Default::default()
        };
        w.rows = 0;
        w.ended = false;
        w.t0 = Instant::now();
        client.req_contract_details(650 + k, &chain)?;
        while !w.ended && w.t0.elapsed() < Duration::from_secs(150) {
            client.process_msgs(&mut w);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    client.disconnect();
    Ok(())
}
