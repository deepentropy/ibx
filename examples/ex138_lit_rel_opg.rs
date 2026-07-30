//! ib-agent#138 — verify LIT / REL / LMT-OPG wire fixes on paper.
//!
//! Submits one of each via EClient and waits for a non-rejected status,
//! then cancels. PASS = orderStatus reaches PreSubmitted/Submitted/
//! PendingSubmit (the latter for OPG, which never advances off-hours).

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order};
use ibx::api::wrapper::Wrapper;

#[derive(Default, Clone, Debug)]
struct State {
    statuses: Vec<(i64, String)>,
    rejects: Vec<(i64, i64, String)>,
}

struct ProbeWrapper {
    state: Arc<Mutex<State>>,
}

impl Wrapper for ProbeWrapper {
    fn order_status(
        &mut self,
        order_id: i64,
        status: &str,
        _filled: f64,
        _remaining: f64,
        _avg_fill_price: f64,
        _perm_id: i64,
        _parent_id: i64,
        _last_fill_price: f64,
        _client_id: i64,
        _why_held: &str,
        _mkt_cap_price: f64,
    ) {
        println!("[order_status] id={} status={}", order_id, status);
        self.state.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("[error] req_id={} code={} msg={}", req_id, code, msg);
        self.state.lock().unwrap().rejects.push((req_id, code, msg.into()));
    }
}

fn aapl() -> Contract {
    Contract {
        con_id: 265598,
        symbol: "AAPL".into(),
        sec_type: "STK".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Default::default()
    }
}

fn run_one(
    client: &EClient,
    state: &Arc<Mutex<State>>,
    wrapper: &mut ProbeWrapper,
    label: &str,
    order_id: i64,
    order: Order,
    accept_pending_submit: bool,
) -> bool {
    println!("\n== {} (order_id={})", label, order_id);
    state.lock().unwrap().statuses.clear();
    state.lock().unwrap().rejects.clear();

    if let Err(e) = client.place_order(order_id, &aapl(), &order) {
        eprintln!("  place_order failed: {}", e);
        return false;
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut accepted = false;
    let mut rejected_for_us = false;
    while Instant::now() < deadline {
        client.process_msgs(wrapper);
        let s = state.lock().unwrap();
        for (rid, _, _) in &s.rejects {
            if *rid == order_id { rejected_for_us = true; }
        }
        for (oid, st) in &s.statuses {
            if *oid != order_id { continue; }
            let ok = st == "PreSubmitted" || st == "Submitted"
                || (accept_pending_submit && st == "PendingSubmit");
            if ok { accepted = true; }
        }
        drop(s);
        if accepted || rejected_for_us { break; }
        std::thread::sleep(Duration::from_millis(20));
    }

    if accepted {
        println!("  -> accepted (will cancel)");
        let _ = client.cancel_order(order_id, "");
        let cancel_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < cancel_deadline {
            client.process_msgs(wrapper);
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    } else if rejected_for_us {
        println!("  -> REJECTED");
        false
    } else {
        println!("  -> TIMEOUT (no ack and no reject)");
        false
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let username = env::var("IB_USERNAME")?;
    let password = env::var("IB_PASSWORD")?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    println!("== Connecting to paper ({})...", host);
    let t0 = Instant::now();
    let client = EClient::connect(&EClientConfig {
        username, password, host, paper: true, core_id: None, code_provider: None,
    })?;
    println!("== Connected in {:.1}s", t0.elapsed().as_secs_f64());

    let state = Arc::new(Mutex::new(State::default()));
    let mut wrapper = ProbeWrapper { state: state.clone() };

    let next_id = || -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as i64
    };

    // 1) LIT — Buy 1 AAPL, lmt=$300, trigger=$280
    let mut lit = Order::default();
    lit.action = "BUY".into();
    lit.order_type = "LIT".into();
    lit.total_quantity = 1.0;
    lit.lmt_price = 300.0;
    lit.aux_price = 280.0;
    lit.tif = "DAY".into();
    let p1 = run_one(&client, &state, &mut wrapper, "LIT", next_id(), lit, false);

    std::thread::sleep(Duration::from_millis(500));

    // 2) Relative — Buy 1 AAPL, peg offset $0.05
    let mut rel = Order::default();
    rel.action = "BUY".into();
    rel.order_type = "REL".into();
    rel.total_quantity = 1.0;
    rel.aux_price = 0.05;
    rel.tif = "DAY".into();
    let p2 = run_one(&client, &state, &mut wrapper, "Relative", next_id(), rel, false);

    std::thread::sleep(Duration::from_millis(500));

    // 3) LMT-OPG — Buy 1 AAPL @ $1.00, TIF=OPG
    let mut opg = Order::default();
    opg.action = "BUY".into();
    opg.order_type = "LMT".into();
    opg.total_quantity = 1.0;
    opg.lmt_price = 1.0;
    opg.tif = "OPG".into();
    let p3 = run_one(&client, &state, &mut wrapper, "LMT-OPG", next_id(), opg, true);

    println!("\n== Summary ==");
    println!("  LIT      : {}", if p1 { "PASS" } else { "FAIL" });
    println!("  Relative : {}", if p2 { "PASS" } else { "FAIL" });
    println!("  LMT-OPG  : {}", if p3 { "PASS" } else { "FAIL" });

    client.disconnect();

    if p1 && p2 && p3 { Ok(()) } else { Err("One or more order types failed".into()) }
}
