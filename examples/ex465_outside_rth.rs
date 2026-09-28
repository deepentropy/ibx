//! ibx#465 probe. Paper account only.
//!
//! Outside RTH follows the reference's rule (ib-agent#199): the first order
//! with outside RTH on a contract asks the definition of its exchange, then
//! a STP on a US stock is sent without outside RTH and gets warning 2109, a
//! LMT keeps it, and the replace of the STP has none. Orders are far from
//! the market and cancelled at the end.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (AAPL price).
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

static T0: OnceLock<Instant> = OnceLock::new();
fn ms() -> u128 { T0.get_or_init(Instant::now).elapsed().as_millis() }

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        let keep = |m: &str, tags: &[&str]| -> String {
            m.split('|').filter(|t| tags.iter().any(|p| t.starts_with(p))).collect::<Vec<_>>().join("|")
        };
        if m.starts_with("WIRE>") && m.contains("|35=c|") && m.contains("ibxrth") {
            println!("{:>6} [sent] {}", ms(), keep(&m, &["35=", "320=", "146=", "6008=", "6004="]));
        } else if m.starts_with("WIRE<") && m.contains("|35=d|") && m.contains("ibxrth") {
            let tokens: Vec<&str> = m.split('|').find(|t| t.starts_with("6431=")).map(|t| t[5..].split(',')
                .filter(|x| ["RTH/", "LTH/", "RTHONLY/", "LTHONLY/", "ELHONLY/", "ERHONLY/", "RTH4MKT/"].iter().any(|p| x.starts_with(p))).collect())
                .unwrap_or_default();
            println!("{:>6} [recv] {} tokens={:?}", ms(), keep(&m, &["35=", "320=", "207=", "6523=", "167=", "15="]), tokens);
        } else if m.starts_with("WIRE>") && (m.contains("|35=D|") || m.contains("|35=G|")) {
            println!("{:>6} [sent] {}", ms(), keep(&m, &["35=", "11=", "40=", "44=", "99=", "59=", "6433="]));
        }
    }
    fn flush(&self) {}
}

struct W;
impl Wrapper for W {
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) { println!("{:>6} [error] {} {} {}", ms(), id, code, msg); }
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        println!("{:>6} [order_status] {} {}", ms(), id, status);
    }
}

fn pump(client: &EClient, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end { client.process_msgs(&mut W); std::thread::sleep(Duration::from_millis(10)); }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    ms();
    let r: f64 = env::var("PROBE_REF_PRICE")?.parse()?;
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
    })?;
    if !client.account_id.starts_with("DU") { client.disconnect(); return Err("not a paper account".into()); }
    let aapl = Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    pump(&client, 2);
    let round = |x: f64| (x * 100.0).round() / 100.0;
    let stp = Order { action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: round(r * 0.7),
        tif: "GTC".into(), outside_rth: true, ..Default::default() };
    let lmt = Order { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: round(r * 0.7),
        tif: "GTC".into(), outside_rth: true, ..Default::default() };
    let stp_id = client.next_order_id();
    let lmt_id = client.next_order_id();
    println!("{:>6} === STP GTC outsideRth ({}) and LMT GTC outsideRth ({})", ms(), stp_id, lmt_id);
    client.place_order(stp_id, &aapl, &stp)?;
    client.place_order(lmt_id, &aapl, &lmt)?;
    pump(&client, 5);
    println!("{:>6} === replace STP, stop -1", ms());
    client.place_order(stp_id, &aapl, &Order { aux_price: stp.aux_price - 1.0, ..stp.clone() })?;
    pump(&client, 4);
    for id in [stp_id, lmt_id] { client.cancel_order(id, "")?; }
    pump(&client, 5);
    client.disconnect();
    Ok(())
}
