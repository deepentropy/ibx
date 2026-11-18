//! ibx#605 probe through the Rust client, any session where orders rest: a
//! far bracket on AAPL (parent BUY 1 LMT, take-profit SELL LMT, stop-loss
//! SELL STP), the take-profit modified, the parent cancelled, then a cancel
//! of each child. The reference refuses each child's cancel locally with
//! 10148 (state Cancelled), the modified take-profit too. Run with
//! `IBX_LOG_DIR` and `IBX_LOG_LEVEL=info` to read the reports.
//!
//! Prints every order callback and error with its time from the start.
//! It stops unless the account id starts with DU.
//!
//! Paper: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{Contract, Order, OrderState};
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
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, state: &OrderState) {
        self.line(format!("openOrder {order_id} {} {} {}", contract.exchange, order.order_type, state.status));
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64, _avg: f64, _perm: i64, _parent: i64,
        _last: f64, _client: i64, _why: &str, _mkt_cap: f64,
    ) {
        self.line(format!("orderStatus {order_id} {status} {filled} {remaining}"));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _json: &str) {
        self.line(format!("error {req_id} {code} {msg}"));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        client_id: 0,
        copied_waits: env::var("IBX_COPIED_WAITS").is_ok_and(|v| v == "1"),
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
    pump(&client, &mut w, 3.0);
    let contract = Contract {
        symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    };
    let parent = client.next_order_id();
    let (tp, sl) = (parent + 1, parent + 2);
    let lmt = |action: &str, price: f64, parent_id: i64, transmit: bool| Order {
        action: action.into(), total_quantity: 1.0, order_type: "LMT".into(), tif: "DAY".into(),
        lmt_price: price, parent_id, transmit, outside_rth: true, ..Default::default()
    };
    w.line(format!("---- bracket {parent} {tp} {sl}"));
    client.place_order(parent, &contract, &lmt("BUY", 150.0, 0, false))?;
    client.place_order(tp, &contract, &lmt("SELL", 600.0, parent, false))?;
    let stop = Order { action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(), tif: "DAY".into(),
        aux_price: 100.0, parent_id: parent, transmit: true, outside_rth: true, ..Default::default() };
    client.place_order(sl, &contract, &stop)?;
    pump(&client, &mut w, 4.0);
    w.line(format!("---- modify {tp}"));
    client.place_order(tp, &contract, &lmt("SELL", 610.0, parent, true))?;
    pump(&client, &mut w, 4.0);
    w.line(format!("---- cancel parent {parent}"));
    client.cancel_order(parent, "")?;
    pump(&client, &mut w, 4.0);
    for id in [parent, tp, sl] {
        w.line(format!("---- cancel {id}"));
        client.cancel_order(id, "")?;
        pump(&client, &mut w, 1.0);
    }
    pump(&client, &mut w, 3.0);
    client.disconnect();
    Ok(())
}
