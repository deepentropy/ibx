//! Paper check of a combo (BAG) order through `EClient` (ibx#470).
//!
//! A SMART stock combo, BUY 1 SPY / SELL 1 QQQ, non-guaranteed, at a limit
//! far from the market (-50.00 for a spread near +23), so it never fills
//! and is held or works until it is cancelled. Checked against the
//! reference's captures of 26/09/2026 (ib-agent#105):
//! - the first order of the session sends the combo set-up requests, then
//!   a 35=D with the BAG conId of the logon (6008), 167=BAG, the leg
//!   symbols in 55, the leg block and NonGuaranteed;
//! - openOrder shows the combo contract (conId, symbol and local symbol
//!   QQQ,SPY, trading class COMB, comboLegsDescrip, two legs);
//! - the price change goes out as a 35=G and the server reports the
//!   replace with the new price, then the cancel;
//! - a second order on the same combo sends no set-up request.
//!
//! Requires IB_USERNAME and IB_PASSWORD (paper account) in the environment.
//! Run with: cargo test --test combo_paper -- --ignored --nocapture

use std::env;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order, TagValue};
use ibx::api::types::{ComboLeg, OrderState};
use ibx::api::wrapper::Wrapper;

struct WireLog {
    lines: Mutex<Vec<String>>,
}

impl log::Log for WireLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let msg = record.args().to_string();
        if msg.starts_with("WIRE") {
            self.lines.lock().unwrap().push(msg);
        } else if record.level() <= log::Level::Warn {
            eprintln!("[{}] {}", record.level(), msg);
        }
    }
    fn flush(&self) {}
}

fn wire() -> &'static WireLog {
    static WIRE: OnceLock<&'static WireLog> = OnceLock::new();
    WIRE.get_or_init(|| {
        let w: &'static WireLog = Box::leak(Box::new(WireLog { lines: Mutex::new(Vec::new()) }));
        let _ = log::set_logger(w);
        log::set_max_level(log::LevelFilter::Trace);
        w
    })
}

/// The frames sent with the given message type, as `tag=value|...` text.
fn sent(msg_type: &str) -> Vec<String> {
    let mark = format!("|35={msg_type}|");
    wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with("WIRE>") && l.contains(&mark))
        .cloned()
        .collect()
}

/// The frames received with the given message type, as text.
fn received(msg_type: &str) -> Vec<String> {
    let mark = format!("|35={msg_type}|");
    wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with("WIRE<") && l.contains(&mark))
        .cloned()
        .collect()
}

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String)>,
    errors: Vec<(i64, i64, String)>,
    open: Vec<(i64, Contract, Order)>,
}

struct Probe(Arc<Mutex<State>>);

impl Wrapper for Probe {
    fn order_status(&mut self, order_id: i64, status: &str, _: f64, _: f64, _: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64) {
        self.0.lock().unwrap().statuses.push((order_id, status.into()));
    }
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, _: &OrderState) {
        self.0.lock().unwrap().open.push((order_id, contract.clone(), order.clone()));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _: &str) {
        eprintln!("  [error] id={} code={} {}", req_id, code, msg);
        self.0.lock().unwrap().errors.push((req_id, code, msg.into()));
    }
}

fn get_config() -> Option<EClientConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(EClientConfig { username, password, host, paper: true, core_id: None })
}

fn combo() -> Contract {
    let leg = |con_id, action: &str| ComboLeg { con_id, ratio: 1, action: action.into(), exchange: "SMART".into(), ..Default::default() };
    Contract {
        symbol: "QQQ,SPY".into(), sec_type: "BAG".into(), exchange: "SMART".into(), currency: "USD".into(),
        combo_legs: vec![leg(756733, "BUY"), leg(320227571, "SELL")],
        ..Default::default()
    }
}

fn order(price: f64) -> Order {
    Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: price,
        order_ref: "ibx470".into(),
        smart_combo_routing_params: vec![TagValue { tag: "NonGuaranteed".into(), value: "1".into() }],
        ..Default::default()
    }
}

fn pump(client: &EClient, probe: &mut Probe, secs: u64, done: impl Fn(&State) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        client.process_msgs(probe);
        if done(&probe.0.lock().unwrap()) { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn last_status(s: &State, id: i64) -> Option<String> {
    s.statuses.iter().rev().find(|(o, _)| *o == id).map(|(_, st)| st.clone())
}

#[test]
#[ignore]
fn combo_order_on_paper() {
    wire();
    let Some(config) = get_config() else {
        println!("Skipping: IB_USERNAME / IB_PASSWORD not set");
        return;
    };
    let client = EClient::connect(&config).expect("connect to paper failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    }
    let state = Arc::new(Mutex::new(State::default()));
    let mut probe = Probe(state.clone());
    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: &str| {
        if ok { println!("    ok: {what}") } else { println!("    FAIL: {what}"); failures.push(what.into()) }
    };
    let id = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 1_000_000) as i64 * 10;
    let working = |s: &State, id: i64| matches!(last_status(s, id).as_deref(), Some("PreSubmitted" | "Submitted"));
    let refused = |s: &State, id: i64| s.errors.iter().any(|(r, c, _)| *r == id && *c != 399);

    println!("=== Combo order (paper account) ===");
    client.place_order(id, &combo(), &order(-50.00)).expect("place_order");
    let up = pump(&client, &mut probe, 30, |s| working(s, id) || refused(s, id)) && working(&state.lock().unwrap(), id);
    check(up, "the combo order works (PreSubmitted or Submitted)");
    let setup = sent("c").iter().filter(|l| l.contains("SecDefReqMsgReqByConid")).count()
        + sent("U").iter().filter(|l| l.contains("|6040=7|")).count();
    check(setup >= 2, "the set-up requests went out (BAG definition, leg confirmation)");
    let d = sent("D").into_iter().find(|l| l.contains(&format!("|11={id}.0|"))).unwrap_or_default();
    println!("    35=D: {d}");
    for part in ["|167=BAG|", "|55=QQQ,SPY|", "|6079=2|", "|6080=756733|6081=1|6082=1|", "|6080=320227571|6081=1|6082=0|", "|6175=0|", "|6134=9|", "|6248=1|"] {
        check(d.contains(part), &format!("35=D has {part}"));
    }
    {
        let s = state.lock().unwrap();
        let last = s.open.iter().rev().find(|(o, _, _)| *o == id);
        check(last.is_some_and(|(_, c, _)| c.sec_type == "BAG" && c.symbol == "QQQ,SPY" && c.local_symbol == "QQQ,SPY"
            && c.trading_class == "COMB" && c.combo_legs.len() == 2 && c.combo_legs_descrip == "756733|1,320227571|-1"
            && c.con_id > 0),
            "openOrder shows the combo contract");
    }
    if up {
        client.place_order(id, &combo(), &order(-50.10)).expect("modify");
        // The server's report of the replace (11 = the order's version 1,
        // 150=5), not ibx's own openOrder echo of the tracked order.
        let replace = format!("|11={id}.1|");
        let replaced = || received("8").into_iter()
            .find(|l| l.contains(&replace) && l.contains("|150=5|"));
        let deadline = Instant::now() + Duration::from_secs(20);
        while replaced().is_none() && Instant::now() < deadline {
            client.process_msgs(&mut probe);
            std::thread::sleep(Duration::from_millis(20));
        }
        let report = replaced();
        for l in sent("G") {
            println!("    35=G: {l}");
        }
        for l in received("8").iter().filter(|l| l.contains(&replace)) {
            println!("    35=8: {l}");
        }
        check(report.as_deref().is_some_and(|l| ["|44=-50.1|", "|44=-50.10|"].iter().any(|p| l.contains(p))),
            "the server reports the replace with the new price");
        check(sent("G").iter().any(|l| l.contains(&format!("|11={id}.1|")) && l.contains("|44=-50.10|") && !l.contains("|6079=")),
            "35=G with the new price and no leg block");
        pump(&client, &mut probe, 2, |_| false);
        check(state.lock().unwrap().open.iter().any(|(o, _, ord)| *o == id && (ord.lmt_price - -50.10).abs() < 1e-9),
            "openOrder shows the new price");
        client.cancel_order(id, "").ok();
        let gone = pump(&client, &mut probe, 20, |s| last_status(s, id).as_deref() == Some("Cancelled"));
        check(gone, "the cancel ends the order");
    }

    // A second order on the same combo: no set-up request.
    let before = sent("c").len();
    let id2 = id + 1;
    client.place_order(id2, &combo(), &order(-51.00)).expect("place_order");
    let up2 = pump(&client, &mut probe, 30, |s| working(s, id2) || refused(s, id2)) && working(&state.lock().unwrap(), id2);
    check(up2, "the second combo order works");
    check(sent("c").len() == before, "the second order sends no set-up request");
    client.cancel_order(id2, "").ok();
    pump(&client, &mut probe, 20, |s| last_status(s, id2).as_deref() == Some("Cancelled"));
    client.disconnect();
    assert!(failures.is_empty(), "{} failure(s): {:?}", failures.len(), failures);
}
