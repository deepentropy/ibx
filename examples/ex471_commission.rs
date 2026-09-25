//! ibx#471 / ibx#474 probe. Paper account only.
//!
//! Buys 1 SPY at a marketable limit, then sells it. Checks that each fill
//! gives exec_details then a commission report with the server's commission
//! and currency (not 0.0), and that req_executions replays both. Prints the
//! exec_details fields of ibx#474 (reqId, time, exchange, permId, clientId,
//! orderRef) and runs req_executions with side filters.
//!
//! Env: IB_USERNAME, IB_PASSWORD, PROBE_REF_PRICE (SPY reference price).
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::types::{CommissionAndFeesReport, Execution, ExecutionFilter};
use ibx::api::wrapper::Wrapper;

struct L;
impl log::Log for L {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, r: &log::Record) {
        let m = r.args().to_string();
        if m.starts_with("WIRE< ccp") && m.contains("|6040=60|") {
            println!("[wire] {}", m);
        } else if m.starts_with("Commission report") || r.level() <= log::Level::Warn {
            println!("[{}] {}", r.level(), m);
        }
    }
    fn flush(&self) {}
}

#[derive(Default)]
struct S { status: Vec<(i64, String)>, execs: Vec<String>, reports: Vec<(String, f64, String)> }
struct W { s: Arc<Mutex<S>> }
impl Wrapper for W {
    fn order_status(&mut self, id: i64, status: &str, _f: f64, _r: f64, _a: f64, _p: i64, _pa: i64, _l: f64, _c: i64, _w: &str, _m: f64) {
        self.s.lock().unwrap().status.push((id, status.into()));
    }
    fn error(&mut self, id: i64, code: i64, msg: &str, _a: &str) {
        println!("[error] {} {} {}", id, code, msg);
    }
    fn exec_details(&mut self, req_id: i64, _c: &Contract, e: &Execution) {
        println!("[exec] req={} order={} exec_id={} {} {} @ {} time='{}' exch={} perm={} client={} ref='{}'",
            req_id, e.order_id, e.exec_id, e.side, e.shares, e.price, e.time, e.exchange, e.perm_id, e.client_id, e.order_ref);
        self.s.lock().unwrap().execs.push(e.exec_id.clone());
    }
    fn commission_and_fees_report(&mut self, r: &CommissionAndFeesReport) {
        println!("[commission] exec_id={} commission={} currency={} realized={}", r.exec_id, r.commission_and_fees, r.currency, r.realized_pnl);
        self.s.lock().unwrap().reports.push((r.exec_id.clone(), r.commission_and_fees, r.currency.clone()));
    }
    fn exec_details_end(&mut self, req_id: i64) {
        println!("[exec_end] {}", req_id);
    }
}

fn pump(client: &EClient, w: &mut W, secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        client.process_msgs(w);
        if done() { return true; }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn main() {
    static LOGGER: L = L;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let reference: f64 = env::var("PROBE_REF_PRICE").expect("PROBE_REF_PRICE").parse().unwrap();
    let client = EClient::connect(&EClientConfig {
        username: env::var("IB_USERNAME").unwrap(), password: env::var("IB_PASSWORD").unwrap(),
        host: "cdc1.ibllc.com".into(), paper: true, core_id: None,
    }).unwrap();
    if !client.account_id.starts_with("DU") { client.disconnect(); panic!("not a paper account"); }
    let s = Arc::new(Mutex::new(S::default()));
    let mut w = W { s: s.clone() };
    let spy = Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    // Let the session-start replay pass first.
    pump(&client, &mut w, 3, || false);
    s.lock().unwrap().execs.clear();
    s.lock().unwrap().reports.clear();

    for (action, factor) in [("BUY", 1.03), ("SELL", 0.97)] {
        let id = client.next_order_id();
        let px = (reference * factor * 100.0).round() / 100.0;
        println!("\n=== {} 1 SPY at {} (order {})", action, px, id);
        let order = Order {
            action: action.into(), order_type: "LMT".into(), total_quantity: 1.0,
            lmt_price: px, tif: "DAY".into(), outside_rth: true,
            order_ref: format!("ex474-{}", action.to_lowercase()), ..Default::default()
        };
        client.place_order(id, &spy, &order).unwrap();
        let filled = pump(&client, &mut w, 30, || {
            s.lock().unwrap().status.iter().any(|(i, st)| *i == id && st == "Filled")
        });
        println!("{}: filled={}", action, filled);
        // Wait for the commission report of this fill.
        let n = s.lock().unwrap().execs.len();
        let got = pump(&client, &mut w, 10, || s.lock().unwrap().reports.len() >= n);
        println!("{}: commission report received={}", action, got);
    }

    println!("\n=== req_executions");
    client.req_executions(9, &ExecutionFilter::default(), &mut w);
    println!("\n=== req_executions side=BUY");
    client.req_executions(10, &ExecutionFilter { side: "BUY".into(), ..Default::default() }, &mut w);
    println!("\n=== req_executions side=SELL");
    client.req_executions(11, &ExecutionFilter { side: "SELL".into(), ..Default::default() }, &mut w);

    let st = s.lock().unwrap();
    let matched = st.execs.iter().filter(|e| st.reports.iter().any(|(r, _, _)| r == *e)).count();
    println!("\nexecutions={} reports={} matched exec_ids={}", st.execs.len(), st.reports.len(), matched);
    drop(st);
    client.disconnect();
}
