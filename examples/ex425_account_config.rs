//! ibx#425 probe. Paper account only.
//!
//! The account config (6040=210) is read at login. On the paper account it
//! has no CUSTACCT, so an order with customerAccount or with
//! professionalCustomer=true is refused with error 145 and nothing is sent.
//!
//! Env: IB_USERNAME, IB_PASSWORD.
use std::env;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("Account config") || m.starts_with("No account config") {
            println!("[log] {}", m);
        } else if m.starts_with("WIRE>") && m.contains("|35=D|") {
            println!("[sent] a new order: {}", m.split('|').filter(|t| t.starts_with("11=")).collect::<Vec<_>>().join(""));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("[error] {} {} {}", id, code, msg); }
}

fn pump(client: &EClient, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(&mut W); std::thread::sleep(Duration::from_millis(10)); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, 2);
    let lmt = Order { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0, tif: "DAY".into(), ..Default::default() };
    let a = client.next_order_id();
    client.place_order(a, &aapl, &Order { customer_account: "C123".into(), ..lmt.clone() })?;
    let b = client.next_order_id();
    client.place_order(b, &aapl, &Order { professional_customer: true, ..lmt.clone() })?;
    pump(&client, 3);
    client.disconnect();
    Ok(())
}
