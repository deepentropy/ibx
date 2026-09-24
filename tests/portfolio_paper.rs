//! Portfolio rows against the real server on the paper account.
//!
//! A portfolio message carries one row per position (ib-agent#192 D4).
//! ibx parsed the whole message into one map and applied only the last row
//! (ibx#411). This runs the engine on a paper session, reads the portfolio
//! messages the server sends, and requires every position to end with the
//! values of its last row: market price, average cost and unrealized P&L.
//!
//! A run usually sees one message within the 30 s window; the old code then
//! fails on every row but the last (2 of 3 on the paper account, 24/09/2026).
//! When several messages arrive, the rows come in a different order each
//! time, so with unchanged prices the old code can also end right; the
//! offline test on a captured message
//! (`a_portfolio_message_applies_every_position_row`) stays the strict check.
//!
//! A second test checks that the Rust client delivers update_portfolio.
//! The two tests log in to the same account and run one after the other.
//!
//! Requires IB_USERNAME and IB_PASSWORD (paper account) in the environment.
//! Run with: cargo test --test portfolio_paper -- --nocapture

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ibx::bridge::SharedState;
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::types::{ControlCommand, PRICE_SCALE};

struct WireLog {
    lines: Mutex<Vec<String>>,
}

impl log::Log for WireLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let msg = record.args().to_string();
        if msg.starts_with("WIRE<") && msg.contains("|35=UP|") {
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

/// The rows of one portfolio message: each starts at the symbol field.
fn rows(line: &str) -> Vec<BTreeMap<u32, String>> {
    let body = line.find("8=").map(|i| &line[i..]).unwrap_or("");
    let mut out: Vec<BTreeMap<u32, String>> = Vec::new();
    for f in body.split('|') {
        let Some((t, v)) = f.split_once('=') else { continue };
        let Ok(tag) = t.parse::<u32>() else { continue };
        if tag == 6068 {
            out.push(BTreeMap::new());
        }
        if let Some(row) = out.last_mut() {
            row.insert(tag, v.to_string());
        }
    }
    out
}

/// One paper session at a time: the first test reads the server messages
/// from a process-wide log.
static SERIAL: Mutex<()> = Mutex::new(());

fn scaled(row: &BTreeMap<u32, String>, tag: u32) -> i64 {
    row.get(&tag).and_then(|v| v.parse::<f64>().ok()).map(|v| (v * PRICE_SCALE as f64) as i64).unwrap_or(0)
}

#[test]
fn every_portfolio_row_is_applied() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    wire();
    let (Ok(username), Ok(password)) = (std::env::var("IB_USERNAME"), std::env::var("IB_PASSWORD")) else {
        println!("SKIP: IB_USERNAME / IB_PASSWORD not set");
        return;
    };
    let cfg = GatewayConfig {
        username,
        password: zeroize::Zeroizing::new(password),
        host: std::env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string()),
        paper: true,
        accept_invalid_certs: false,
        ib_key_timeout_secs: ibx::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: ibx::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: None,
    };
    println!("=== Portfolio rows (paper account) ===");
    let (gw, farm_conn, ccp_conn, hmds) = Gateway::connect(&cfg).expect("connect to paper failed");
    assert!(gw.account_id.starts_with("DU"), "refusing to run: the logged-in account is not a paper account (its id does not start with DU)");

    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = gw.into_hot_loop_with_farms(shared.clone(), None, farm_conn, ccp_conn, hmds, None);
    // The engine is large; run it on a thread with a big stack, as the paper suite does.
    let join = std::thread::Builder::new().stack_size(64 * 1024 * 1024)
        .spawn(move || { let mut hl = hot_loop; hl.run(); })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while wire().lines.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Let the engine finish applying the last message it received.
    std::thread::sleep(Duration::from_millis(500));
    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    let messages: Vec<Vec<BTreeMap<u32, String>>> = wire().lines.lock().unwrap().iter().map(|l| rows(l)).collect();
    let widest = messages.iter().map(|m| m.len()).max().unwrap_or(0);
    println!("  {} portfolio message(s), up to {} position(s) per message", messages.len(), widest);
    assert!(!messages.is_empty(), "no portfolio message received in 30 s");
    if widest < 2 {
        println!("  SKIP: no portfolio message with 2 or more positions on this account");
        return;
    }

    // The last row seen for each position is what the engine must hold.
    let mut last: BTreeMap<i64, BTreeMap<u32, String>> = BTreeMap::new();
    for row in messages.into_iter().flatten() {
        if let Some(con_id) = row.get(&6008).and_then(|v| v.parse().ok()) {
            last.insert(con_id, row);
        }
    }
    let mut wrong = Vec::new();
    for (con_id, row) in &last {
        let held = shared.portfolio.position_info(*con_id);
        let ok = held.as_ref().is_some_and(|p| p.market_price == scaled(row, 6065)
            && p.avg_cost == scaled(row, 6101)
            && p.unrealized_pnl == scaled(row, 6100));
        if !ok {
            wrong.push(*con_id);
        }
    }
    println!("  {} position(s) checked, {} with values that differ from their last row", last.len(), wrong.len());
    assert!(wrong.is_empty(), "positions not matching their last portfolio row: {:?}", wrong);
    println!("  PASS");
}

// ── update_portfolio through EClient ──
//
// The Rust client never called update_portfolio; only the Python client
// did. Every position req_positions lists must reach update_portfolio with
// its market price and its symbol.

#[derive(Default)]
struct Seen {
    positions: Vec<i64>,
    priced: std::collections::BTreeSet<i64>,
    /// con_id -> symbol carried by update_portfolio.
    symbols: BTreeMap<i64, String>,
    /// Multipliers carried by position and update_portfolio.
    multipliers: Vec<String>,
}

struct Probe(Arc<Mutex<Seen>>);

impl ibx::api::wrapper::Wrapper for Probe {
    fn position(&mut self, _account: &str, contract: &ibx::api::client::Contract, _pos: f64, _avg: f64) {
        let mut s = self.0.lock().unwrap();
        s.positions.push(contract.con_id);
        s.multipliers.push(contract.multiplier.clone());
    }
    fn update_portfolio(
        &mut self, contract: &ibx::api::client::Contract, _position: f64, market_price: f64,
        _market_value: f64, _average_cost: f64, _unrealized_pnl: f64,
        _realized_pnl: f64, _account_name: &str,
    ) {
        let mut s = self.0.lock().unwrap();
        if market_price > 0.0 {
            s.priced.insert(contract.con_id);
        }
        s.symbols.insert(contract.con_id, contract.symbol.clone());
        s.multipliers.push(contract.multiplier.clone());
    }
}

#[test]
fn eclient_delivers_update_portfolio() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (Ok(username), Ok(password)) = (std::env::var("IB_USERNAME"), std::env::var("IB_PASSWORD")) else {
        println!("SKIP: IB_USERNAME / IB_PASSWORD not set");
        return;
    };
    let host = std::env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    println!("=== update_portfolio through EClient (paper account) ===");
    let client = ibx::api::client::EClient::connect(&ibx::api::client::EClientConfig {
        username, password, host, paper: true, core_id: None,
    }).expect("connect to paper failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut probe = Probe(seen.clone());
    client.req_positions(&mut probe);
    client.req_account_updates(true, "");
    let listed: Vec<i64> = seen.lock().unwrap().positions.clone();
    println!("  {} position(s) listed", listed.len());

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        client.process_msgs(&mut probe);
        if listed.iter().all(|c| seen.lock().unwrap().priced.contains(c)) { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    client.disconnect();
    if listed.is_empty() {
        println!("  SKIP: no position on this account");
        return;
    }
    let priced = seen.lock().unwrap().priced.clone();
    let missing: Vec<&i64> = listed.iter().filter(|c| !priced.contains(c)).collect();
    println!("  {} of {} position(s) delivered with a market price", listed.len() - missing.len(), listed.len());
    assert!(missing.is_empty(), "update_portfolio missing for {:?}", missing);
    // The contract carries the symbol even for a contract the session never
    // looked up (it comes from the portfolio row).
    let symbols = seen.lock().unwrap().symbols.clone();
    let unnamed: Vec<&i64> = listed.iter().filter(|c| symbols.get(c).is_none_or(|s| s.is_empty())).collect();
    println!("  symbols: {:?}", symbols.values().collect::<Vec<_>>());
    assert!(unnamed.is_empty(), "update_portfolio without a symbol for {:?}", unnamed);
    // The multiplier was the whole portfolio row key (symbol, currency,
    // multiplier and contract id joined).
    let multipliers = seen.lock().unwrap().multipliers.clone();
    let mut distinct = multipliers.clone();
    distinct.sort();
    distinct.dedup();
    println!("  multipliers: {:?}", distinct);
    assert!(!multipliers.iter().any(|m| m.contains('/')), "multiplier holds the row key: {:?}", distinct);
    println!("  PASS");
}
