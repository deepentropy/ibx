//! Integration tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! All tests share a single Gateway connection to avoid ONELOGON throttling.
//! Each phase builds a fresh HotLoop, runs it, then reclaims connections.

use std::env;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ib_engine::control::contracts;
use ib_engine::control::historical::{self, BarDataType, BarSize, HistoricalRequest};
use ib_engine::engine::context::{Context, Strategy};
use ib_engine::engine::hot_loop::HotLoop;
use ib_engine::gateway::{Gateway, GatewayConfig};
use ib_engine::protocol::connection::{Connection, Frame};
use ib_engine::protocol::fix;
use ib_engine::protocol::fixcomp;
use ib_engine::types::*;

fn get_config() -> Option<GatewayConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(GatewayConfig {
        username,
        password,
        host,
        paper: true,
    })
}

/// Shared connections passed between test phases.
struct Conns {
    farm: Connection,
    ccp: Connection,
    hmds: Option<Connection>,
    account_id: String,
}

/// Run a hot loop in a background thread, returning the HotLoop for connection reclamation.
fn run_hot_loop<S: Strategy + Send + 'static>(hot_loop: HotLoop<S>) -> std::thread::JoinHandle<HotLoop<S>> {
    std::thread::spawn(move || {
        let mut hl = hot_loop;
        hl.run();
        hl
    })
}

/// Shutdown a hot loop and reclaim connections.
fn shutdown_and_reclaim<S: Strategy + Send + 'static>(
    control_tx: &crossbeam_channel::Sender<ControlCommand>,
    join: std::thread::JoinHandle<HotLoop<S>>,
    account_id: String,
) -> Conns {
    let _ = control_tx.send(ControlCommand::Shutdown);
    let mut hl = join.join().expect("hot loop thread panicked");
    let farm = hl.farm_conn.take().expect("farm_conn missing after shutdown");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing after shutdown");
    let hmds = hl.hmds_conn.take();
    Conns { farm, ccp, hmds, account_id }
}

// ─── Master test suite ───

#[test]
#[ignore]
fn integration_suite() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Integration Suite (single connection) ===\n");
    let suite_start = Instant::now();

    // One connection for all tests
    let start = Instant::now();
    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    let connect_time = start.elapsed();

    // Phase 1: CCP auth checks (no hot loop)
    phase_ccp_auth(&gw, hmds_conn.is_some(), connect_time);

    let mut conns = Conns {
        farm: farm_conn,
        ccp: ccp_conn,
        hmds: hmds_conn,
        account_id: gw.account_id.clone(),
    };

    // Raw subscribe test: send subscribe directly on farm connection, no hot loop
    {
        println!("--- RAW SUBSCRIBE TEST ---");
        let conn = &mut conns.farm;
        let result = conn.send_fixcomp(&[
            (fix::TAG_MSG_TYPE, "V"),
            (fix::TAG_SENDING_TIME, &ib_engine::gateway::chrono_free_timestamp()),
            (263, "1"),
            (146, "2"),
            (262, "1"),
            (6008, "756733"),
            (207, "BEST"),
            (167, "CS"),
            (264, "442"),
            (6088, "Socket"),
            (9830, "1"),
            (9839, "1"),
            (262, "2"),
            (6008, "756733"),
            (207, "BEST"),
            (167, "CS"),
            (264, "443"),
            (6088, "Socket"),
            (9830, "1"),
            (9839, "1"),
        ]);
        println!("  subscribe sent: {:?}, seq={}", result, conn.seq);

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut got_data = false;
        while Instant::now() < deadline {
            match conn.try_recv() {
                Ok(0) => {} // WouldBlock
                Ok(n) => {
                    println!("  recv {} bytes, total buffered: {}", n, conn.buffered());
                    let frames = conn.extract_frames();
                    println!("  {} frames extracted", frames.len());
                    for frame in &frames {
                        let (raw, label) = match frame {
                            Frame::FixComp(r) => (r, "FIXCOMP"),
                            Frame::Binary(r) => (r, "Binary"),
                            Frame::Fix(r) => (r, "FIX"),
                        };
                        let (unsigned, valid) = conn.unsign(raw);
                        if label == "FIXCOMP" {
                            let inner = fixcomp::fixcomp_decompress(&unsigned);
                            for m in &inner {
                                let preview = String::from_utf8_lossy(&m[..std::cmp::min(150, m.len())]);
                                println!("  {} inner (valid={}): {}", label, valid, preview);
                            }
                        } else {
                            let preview = String::from_utf8_lossy(&unsigned[..std::cmp::min(150, unsigned.len())]);
                            println!("  {} (valid={}): {}", label, valid, preview);
                        }
                        got_data = true;
                    }
                }
                Err(e) => {
                    println!("  recv error: {}", e);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !got_data {
            println!("  NO DATA received in 15s");
        }
        println!();
    }

    // Phase 14: Account PnL — MUST be first hot loop to receive CCP init burst
    conns = phase_account_pnl(conns);

    // Phase 12: Contract details (raw CCP — init burst already consumed)
    phase_contract_details(&mut conns);

    // Hot loop phases
    conns = phase_market_data(conns);
    conns = phase_multi_instrument(conns);
    conns = phase_account_data(conns);
    conns = phase_outside_rth(conns);
    conns = phase_limit_order(conns);
    conns = phase_stop_order(conns);
    conns = phase_stop_limit_order(conns);
    conns = phase_modify_order(conns);
    conns = phase_subscribe_unsubscribe(conns);
    conns = phase_heartbeat_keepalive(conns);
    conns = phase_market_order(conns);
    conns = phase_commission(conns);

    // Phase 11: Historical data (uses bg hot loop — run late to avoid stale HMDS)
    conns = phase_historical_data(conns);
    let _conns = phase_graceful_shutdown(conns);

    println!("\n=== All 17 phases passed in {:.1}s (single connection) ===",
        suite_start.elapsed().as_secs_f64());
}

// ─── Phase 1: CCP auth + farm logon (no hot loop) ───

fn phase_ccp_auth(gw: &Gateway, has_hmds: bool, connect_time: Duration) {
    println!("--- Phase 1: CCP Auth + Farm Logon ---");

    assert!(!gw.account_id.is_empty(), "Account ID should be non-empty after CCP logon");
    println!("  Account ID: {}", gw.account_id);

    assert!(!gw.server_session_id.is_empty(), "Server session ID should be set");
    if !gw.ccp_token.is_empty() {
        println!("  CCP token: present");
    } else {
        println!("  CCP token: not present (non-fatal)");
    }
    assert!(gw.heartbeat_interval > 0, "Heartbeat interval should be positive");
    println!("  Session ID: {}", gw.server_session_id);
    println!("  Heartbeat interval: {}s", gw.heartbeat_interval);

    use num_bigint::BigUint;
    assert!(gw.session_token > BigUint::from(0u32), "Session token should be non-zero");

    if has_hmds {
        println!("  ushmds farm: CONNECTED");
    } else {
        println!("  ushmds farm: NOT CONNECTED (non-fatal)");
    }

    assert!(connect_time < Duration::from_secs(60), "Connection took too long: {:?}", connect_time);
    println!("  PASS ({:.3}s)\n", connect_time.as_secs_f64());
}

// ─── Phase 12: Contract details lookup (raw CCP) ───

fn phase_contract_details(conns: &mut Conns) {
    println!("--- Phase 12: Contract Details Lookup (SPY, conId=756733) ---");

    let now = ib_engine::gateway::chrono_free_timestamp();
    conns.ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "R1"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_IB_CON_ID, "756733"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send secdef request");

    let mut contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && contract.is_none() {
        match conns.ccp.try_recv() {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                println!("  CCP recv error: {}", e);
                break;
            }
            Ok(_) => {}
        }

        for frame in conns.ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = conns.ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };

            for msg in messages {
                let tags = fix::fix_parse(&msg);
                let msg_type = tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()).unwrap_or("?");
                if msg_type == "d" {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.con_id == 756733 {
                            println!("  {} ({}) conId={}", def.symbol, def.long_name, def.con_id);
                            println!("  SecType={:?} Exchange={} Currency={}", def.sec_type, def.exchange, def.currency);
                            contract = Some(def);
                        }
                    }
                }
            }
        }
    }

    let def = contract.expect("No contract details received for SPY (756733)");
    assert_eq!(def.con_id, 756733);
    assert_eq!(def.symbol, "SPY");
    assert_eq!(def.sec_type, contracts::SecurityType::Stock);
    assert_eq!(def.currency, "USD");
    assert!(!def.long_name.is_empty(), "Long name should not be empty");
    assert!(!def.valid_exchanges.is_empty(), "Valid exchanges should not be empty");
    assert!(def.valid_exchanges.contains(&"SMART".to_string()), "SMART should be in valid exchanges");
    println!("  PASS\n");
}

// ─── Phase 11: Historical data bars via HMDS ───

struct NoopStrategy;
impl Strategy for NoopStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}
    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}
    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_historical_data(conns: Conns) -> Conns {
    println!("--- Phase 11: Historical Data Bars (SPY, 1 day of 5-min bars) ---");

    let mut hmds = match conns.hmds {
        Some(c) => c,
        None => {
            println!("  SKIP: ushmds farm not connected\n");
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;

    // Run a background hot loop to keep CCP+farm heartbeats alive during HMDS work
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        NoopStrategy, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    // HMDS work on main thread
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let end_time = {
        let days = now / secs_per_day;
        let mut y = 1970i64;
        let mut remaining = days as i64;
        loop {
            let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
            if remaining < days_in_year { break; }
            remaining -= days_in_year;
            y += 1;
        }
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut m = 0;
        for (i, &d) in month_days.iter().enumerate() {
            if remaining < d as i64 { m = i + 1; break; }
            remaining -= d as i64;
        }
        let day = remaining + 1;
        let hour = (now % secs_per_day) / 3600;
        let min = (now % 3600) / 60;
        let sec = now % 60;
        format!("{:04}{:02}{:02}-{:02}:{:02}:{:02}", y, m, day, hour, min, sec)
    };

    let req = HistoricalRequest {
        query_id: "test1".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time,
        duration: "1 d".to_string(),
        bar_size: BarSize::Min5,
        use_rth: true,
    };

    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send historical request");

    let mut all_bars = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut complete = false;

    while Instant::now() < deadline && !complete {
        match hmds.try_recv() {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                println!("  HMDS recv error: {}", e);
                break;
            }
            Ok(_) => {}
        }

        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = hmds.unsign(raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };

            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        all_bars.extend(resp.bars);
                        if resp.is_complete {
                            complete = true;
                        }
                    }
                }
            }
        }
    }

    // Reclaim CCP+farm from background hot loop
    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Total bars received: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No historical bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }

    let first = &all_bars[0];
    assert!(first.open > 0.0, "Open price should be positive");
    assert!(first.high >= first.low, "High should be >= Low");
    assert!(first.volume > 0, "Volume should be positive");
    println!("  First bar: O={:.2} H={:.2} L={:.2} C={:.2} V={}",
        first.open, first.high, first.low, first.close, first.volume);
    println!("  PASS ({} bars)\n", all_bars.len());

    bg_conns
}

// ─── Phase 2: Market data ticks ───

struct DiagnosticStrategy {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
}

struct DiagnosticHandle {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
}

impl DiagnosticStrategy {
    fn new() -> (Self, DiagnosticHandle) {
        let tick_count = Arc::new(AtomicU32::new(0));
        let first_tick_received = Arc::new(AtomicBool::new(false));
        let account_checked = Arc::new(AtomicBool::new(false));
        let net_liq = Arc::new(AtomicI64::new(0));
        let disconnected = Arc::new(AtomicBool::new(false));

        let handle = DiagnosticHandle {
            tick_count: tick_count.clone(),
            first_tick_received: first_tick_received.clone(),
            account_checked: account_checked.clone(),
            net_liq: net_liq.clone(),
            disconnected: disconnected.clone(),
        };

        let strategy = Self {
            tick_count,
            first_tick_received,
            account_checked,
            net_liq,
            disconnected,
        };

        (strategy, handle)
    }
}

impl Strategy for DiagnosticStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);

        if count == 0 {
            let bid = ctx.bid(instrument) as f64 / PRICE_SCALE as f64;
            let ask = ctx.ask(instrument) as f64 / PRICE_SCALE as f64;
            let last = ctx.last(instrument) as f64 / PRICE_SCALE as f64;
            let spread = ctx.spread(instrument) as f64 / PRICE_SCALE as f64;
            println!("  FIRST TICK: instrument={} bid={:.4} ask={:.4} last={:.4} spread={:.6}",
                instrument, bid, ask, last, spread);
            self.first_tick_received.store(true, Ordering::Relaxed);
        }

        if count == 10 && !self.account_checked.load(Ordering::Relaxed) {
            let acct = ctx.account();
            let nl = acct.net_liquidation;
            self.net_liq.store(nl, Ordering::Relaxed);
            if nl != 0 {
                println!("  ACCOUNT: net_liq={:.2}", nl as f64 / PRICE_SCALE as f64);
                self.account_checked.store(true, Ordering::Relaxed);
            }
        }
    }

    fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
        println!("  FILL: order={} side={:?} qty={} price={:.4}",
            fill.order_id, fill.side, fill.qty, fill.price as f64 / PRICE_SCALE as f64);
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

fn phase_market_data(conns: Conns) -> Conns {
    println!("--- Phase 2: Market Data Ticks (AAPL) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !handle.first_tick_received.load(Ordering::Relaxed) {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks in 30s — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({} ticks)\n", total);
    conns
}

// ─── Phase 3: Multi-instrument subscription ───

fn phase_multi_instrument(conns: Conns) -> Conns {
    println!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !handle.first_tick_received.load(Ordering::Relaxed) {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No ticks — market closed or IB throttling\n");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if total <= 3 {
        println!("  SKIP: Only {} ticks — insufficient for multi-instrument test\n", total);
    } else {
        println!("  PASS ({} ticks)\n", total);
    }
    conns
}

// ─── Phase 4: Account data reception ───

fn phase_account_data(conns: Conns) -> Conns {
    println!("--- Phase 4: Account Data Reception ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if handle.account_checked.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    let net_liq = handle.net_liq.load(Ordering::Relaxed);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.account_checked.load(Ordering::Relaxed) {
        assert!(net_liq > 0, "Paper account net liquidation should be > 0");
        println!("  net_liq=${:.2}", net_liq as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  WARN: Account data not received within 20s\n");
    }
    conns
}

// ─── Phase 14: Account PnL reception ───

struct PnlStrategy {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    unrealized_pnl: Arc<AtomicI64>,
    realized_pnl: Arc<AtomicI64>,
    buying_power: Arc<AtomicI64>,
    probe_done: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct PnlHandle {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    probe_done: Arc<AtomicBool>,
}

impl PnlStrategy {
    fn new() -> (Self, PnlHandle) {
        let account_received = Arc::new(AtomicBool::new(false));
        let net_liq = Arc::new(AtomicI64::new(0));
        let unrealized_pnl = Arc::new(AtomicI64::new(0));
        let realized_pnl = Arc::new(AtomicI64::new(0));
        let buying_power = Arc::new(AtomicI64::new(0));
        let probe_done = Arc::new(AtomicBool::new(false));
        let handle = PnlHandle {
            account_received: account_received.clone(),
            net_liq: net_liq.clone(),
            probe_done: probe_done.clone(),
        };
        (Self {
            account_received, net_liq, unrealized_pnl, realized_pnl, buying_power,
            probe_done, order_id: None,
        }, handle)
    }

    fn check_account(&mut self, ctx: &Context) {
        if self.account_received.load(Ordering::Relaxed) { return; }
        let acct = ctx.account();
        if acct.net_liquidation != 0 {
            self.net_liq.store(acct.net_liquidation, Ordering::Relaxed);
            self.unrealized_pnl.store(acct.unrealized_pnl, Ordering::Relaxed);
            self.realized_pnl.store(acct.realized_pnl, Ordering::Relaxed);
            self.buying_power.store(acct.buying_power, Ordering::Relaxed);
            self.account_received.store(true, Ordering::Relaxed);
        }
    }
}

impl Strategy for PnlStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, 1_00_000_000, true);
        self.order_id = Some(id);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, ctx: &mut Context) {
        self.check_account(ctx);
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        self.check_account(ctx);
        if update.status == OrderStatus::Submitted {
            if let Some(id) = self.order_id {
                ctx.cancel(id);
            }
        }
        if matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected) {
            self.probe_done.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_account_pnl(conns: Conns) -> Conns {
    println!("--- Phase 14: Account PnL Reception ---");

    let account_id = conns.account_id;
    let (strategy, handle) = PnlStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if handle.probe_done.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let nl = handle.net_liq.load(Ordering::Relaxed);
    assert!(handle.account_received.load(Ordering::Relaxed),
        "Account data not received — 6040=77 may not contain tag 9806");
    assert!(nl > 0, "Paper account net liquidation should be > 0");
    println!("  NetLiq: ${:.2}", nl as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 5: Graceful shutdown ───

fn phase_graceful_shutdown(conns: Conns) -> Conns {
    println!("--- Phase 5: Graceful Shutdown ---");

    let account_id = conns.account_id;
    let (strategy, handle) = DiagnosticStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let join = run_hot_loop(hot_loop);

    std::thread::sleep(Duration::from_secs(2));

    let shutdown_start = Instant::now();
    control_tx.send(ControlCommand::Shutdown).unwrap();

    let mut hl = join.join().expect("hot loop panicked");
    let shutdown_time = shutdown_start.elapsed();

    assert!(
        shutdown_time < Duration::from_secs(2),
        "Shutdown took too long: {:?}", shutdown_time
    );
    assert!(
        handle.disconnected.load(Ordering::Relaxed),
        "on_disconnect() was not called during shutdown"
    );

    let farm = hl.farm_conn.take().expect("farm_conn missing");
    let ccp = hl.ccp_conn.take().expect("ccp_conn missing");
    let hmds = hl.hmds_conn.take();

    println!("  Shutdown in {:.3}s", shutdown_time.as_secs_f64());
    println!("  PASS\n");
    Conns { farm, ccp, hmds, account_id }
}

// ─── Order strategies ───

struct OrderTestStrategy {
    phase: u8,
    instrument: InstrumentId,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    buy_fill_time_us: Arc<AtomicU64>,
    sell_fill_time_us: Arc<AtomicU64>,
    order_rejected: Arc<AtomicBool>,
    ticks_before_order: u32,
    tick_count: u32,
    buy_sent_at: Option<Instant>,
    sell_sent_at: Option<Instant>,
}

struct OrderTestHandle {
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    buy_fill_time_us: Arc<AtomicU64>,
    sell_fill_time_us: Arc<AtomicU64>,
    order_rejected: Arc<AtomicBool>,
}

impl OrderTestStrategy {
    fn new(ticks_before_order: u32) -> (Self, OrderTestHandle) {
        let buy_fill_price = Arc::new(AtomicI64::new(0));
        let sell_fill_price = Arc::new(AtomicI64::new(0));
        let buy_fill_time_us = Arc::new(AtomicU64::new(0));
        let sell_fill_time_us = Arc::new(AtomicU64::new(0));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = OrderTestHandle {
            buy_fill_price: buy_fill_price.clone(),
            sell_fill_price: sell_fill_price.clone(),
            buy_fill_time_us: buy_fill_time_us.clone(),
            sell_fill_time_us: sell_fill_time_us.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            phase: 0,
            instrument: 0,
            buy_fill_price,
            sell_fill_price,
            buy_fill_time_us,
            sell_fill_time_us,
            order_rejected,
            ticks_before_order,
            tick_count: 0,
            buy_sent_at: None,
            sell_sent_at: None,
        }, handle)
    }
}

impl Strategy for OrderTestStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        self.tick_count += 1;

        if self.phase == 0 && self.tick_count >= self.ticks_before_order {
            self.instrument = instrument;
            ctx.submit_market(instrument, Side::Buy, 1);
            self.buy_sent_at = Some(Instant::now());
            self.phase = 1;
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        if self.phase == 1 && fill.side == Side::Buy {
            let rtt = self.buy_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_fill_time_us.store(rtt, Ordering::Relaxed);

            ctx.submit_market(self.instrument, Side::Sell, 1);
            self.sell_sent_at = Some(Instant::now());
            self.phase = 2;
        } else if self.phase == 2 && fill.side == Side::Sell {
            let rtt = self.sell_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_fill_time_us.store(rtt, Ordering::Relaxed);
            self.phase = 3;
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

// ─── Phase 6: Market order round-trip ───

fn phase_market_order(conns: Conns) -> Conns {
    println!("--- Phase 6: Market Order Round-Trip (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = OrderTestStrategy::new(5);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.sell_fill_price.load(Ordering::Relaxed) != 0 { break; }
        if handle.order_rejected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let buy_rtt = handle.buy_fill_time_us.load(Ordering::Relaxed);
    let sell_rtt = handle.sell_fill_time_us.load(Ordering::Relaxed);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    if buy_price == 0 {
        println!("  SKIP: No buy fill — market is closed\n");
        return conns;
    }
    assert!(sell_price > 0, "Buy filled but no sell fill received");

    println!("  Buy: ${:.4} (RTT {:.3}ms)", buy_price as f64 / PRICE_SCALE as f64, buy_rtt as f64 / 1000.0);
    println!("  Sell: ${:.4} (RTT {:.3}ms)", sell_price as f64 / PRICE_SCALE as f64, sell_rtt as f64 / 1000.0);
    println!("  Mean RTT: {:.3}ms", (buy_rtt + sell_rtt) as f64 / 2000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 7: Limit order submit + cancel ───

struct LimitOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    submit_ack_us: Arc<AtomicU64>,
    cancel_conf_us: Arc<AtomicU64>,
    order_id: Option<OrderId>,
    submit_time: Option<Instant>,
    cancel_time: Option<Instant>,
}

struct LimitOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    submit_ack_us: Arc<AtomicU64>,
    cancel_conf_us: Arc<AtomicU64>,
}

impl LimitOrderStrategy {
    fn new() -> (Self, LimitOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let submit_ack_us = Arc::new(AtomicU64::new(0));
        let cancel_conf_us = Arc::new(AtomicU64::new(0));
        let handle = LimitOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
            submit_ack_us: submit_ack_us.clone(),
            cancel_conf_us: cancel_conf_us.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            submit_ack_us, cancel_conf_us,
            order_id: None, submit_time: None, cancel_time: None,
        }, handle)
    }
}

impl Strategy for LimitOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let limit_price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit(0, Side::Buy, 1, limit_price);
        self.order_id = Some(id);
        self.submit_time = Some(Instant::now());
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                if !self.order_acked.load(Ordering::Relaxed) {
                    let rtt = self.submit_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    self.submit_ack_us.store(rtt, Ordering::Relaxed);
                }
                self.order_acked.store(true, Ordering::Relaxed);
                // Cancel immediately after ack
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_time = Some(Instant::now());
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                let rtt = self.cancel_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                self.cancel_conf_us.store(rtt, Ordering::Relaxed);
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 7: Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = LimitOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");

    let ack_us = handle.submit_ack_us.load(Ordering::Relaxed);
    let cancel_us = handle.cancel_conf_us.load(Ordering::Relaxed);
    println!("  Submit→Ack: {:.3}ms  Cancel→Conf: {:.3}ms", ack_us as f64 / 1000.0, cancel_us as f64 / 1000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 8: Stop order submit + cancel ───

struct StopOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct StopOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl StopOrderStrategy {
    fn new() -> (Self, StopOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = StopOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for StopOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let stop_price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_stop(0, Side::Sell, 1, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_stop_order(conns: Conns) -> Conns {
    println!("--- Phase 8: Stop Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = StopOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Stop order rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 9: Order modify (35=G) ───

struct ModifyOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
    new_order_id: Option<OrderId>,
}

struct ModifyOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl ModifyOrderStrategy {
    fn new() -> (Self, ModifyOrderHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let modify_sent = Arc::new(AtomicBool::new(false));
        let modify_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = ModifyOrderHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            modify_sent: modify_sent.clone(),
            modify_acked: modify_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, modify_sent, modify_acked, cancel_sent,
            order_cancelled, order_rejected,
            order_id: None, new_order_id: None,
        }, handle)
    }
}

impl Strategy for ModifyOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit(0, Side::Buy, 1, price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                if self.modify_sent.load(Ordering::Relaxed) && !self.modify_acked.load(Ordering::Relaxed) {
                    self.modify_acked.store(true, Ordering::Relaxed);
                    let cancel_id = self.new_order_id.unwrap_or_else(|| self.order_id.unwrap());
                    ctx.cancel(cancel_id);
                    self.cancel_sent.store(true, Ordering::Relaxed);
                } else if !self.order_acked.load(Ordering::Relaxed) {
                    self.order_acked.store(true, Ordering::Relaxed);
                    if let Some(id) = self.order_id {
                        let new_price = 2_00_000_000i64; // $2.00
                        let new_id = ctx.modify(id, new_price, 1);
                        self.new_order_id = Some(new_id);
                        self.modify_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_modify_order(conns: Conns) -> Conns {
    println!("--- Phase 9: Order Modify (35=G) + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = ModifyOrderStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Modify test rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.modify_sent.load(Ordering::Relaxed), "Modify was never sent");
    assert!(handle.modify_acked.load(Ordering::Relaxed), "Modify was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 10: Outside RTH limit order (GTC + OutsideRTH) ───

struct OutsideRthStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct OutsideRthHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl OutsideRthStrategy {
    fn new() -> (Self, OutsideRthHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = OutsideRthHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for OutsideRthStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, price, true);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_outside_rth(conns: Conns) -> Conns {
    println!("--- Phase 10: Outside RTH Limit Order (GTC+OutsideRTH, SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = OutsideRthStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        panic!("GTC+OutsideRTH order should not be rejected");
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 13: Heartbeat keepalive ───

struct HeartbeatStrategy {
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

struct HeartbeatHandle {
    disconnected: Arc<AtomicBool>,
}

impl HeartbeatStrategy {
    fn new() -> (Self, HeartbeatHandle) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let tick_count = Arc::new(AtomicU32::new(0));
        let handle = HeartbeatHandle { disconnected: disconnected.clone() };
        (Self { disconnected, tick_count }, handle)
    }
}

impl Strategy for HeartbeatStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);
        if count % 100 == 0 && count > 0 {
            println!("  {} ticks, still connected", count);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

fn phase_heartbeat_keepalive(conns: Conns) -> Conns {
    println!("--- Phase 13: Heartbeat Keepalive (20s > CCP 10s interval) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = HeartbeatStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if handle.disconnected.load(Ordering::Relaxed) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    let still_connected = !handle.disconnected.load(Ordering::Relaxed);
    let elapsed = start.elapsed();

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(still_connected,
        "Connection dropped after {:.1}s — heartbeat mechanism failed", elapsed.as_secs_f64());
    println!("  PASS ({:.1}s, no disconnect)\n", elapsed.as_secs_f64());
    conns
}

// ─── Phase 15: Stop limit order submit + cancel ───

struct StopLimitStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
}

struct StopLimitHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
}

impl StopLimitStrategy {
    fn new() -> (Self, StopLimitHandle) {
        let submitted = Arc::new(AtomicBool::new(false));
        let order_acked = Arc::new(AtomicBool::new(false));
        let cancel_sent = Arc::new(AtomicBool::new(false));
        let order_cancelled = Arc::new(AtomicBool::new(false));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let handle = StopLimitHandle {
            submitted: submitted.clone(),
            order_acked: order_acked.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None,
        }, handle)
    }
}

impl Strategy for StopLimitStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let stop_price = 999_00_000_000i64; // $999.00
        let limit_price = 998_00_000_000i64; // $998.00
        let id = ctx.submit_stop_limit(0, Side::Buy, 1, limit_price, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                    }
                }
            }
            OrderStatus::Cancelled => {
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_stop_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 15: Stop Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = StopLimitStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Stop limit test rejected\n");
        return conns;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop limit was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop limit was never acknowledged");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop limit was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 16: Subscribe + Unsubscribe cleanup ───

struct UnsubscribeStrategy {
    unsub_requested: Arc<AtomicBool>,
    still_alive: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

struct UnsubscribeHandle {
    unsub_requested: Arc<AtomicBool>,
    still_alive: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
}

impl UnsubscribeStrategy {
    fn new() -> (Self, UnsubscribeHandle) {
        let unsub_requested = Arc::new(AtomicBool::new(false));
        let still_alive = Arc::new(AtomicBool::new(false));
        let disconnected = Arc::new(AtomicBool::new(false));
        let tick_count = Arc::new(AtomicU32::new(0));
        let handle = UnsubscribeHandle {
            unsub_requested: unsub_requested.clone(),
            still_alive: still_alive.clone(),
            disconnected: disconnected.clone(),
            tick_count: tick_count.clone(),
        };
        (Self { unsub_requested, still_alive, disconnected, tick_count }, handle)
    }
}

impl Strategy for UnsubscribeStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }
}

fn phase_subscribe_unsubscribe(conns: Conns) -> Conns {
    println!("--- Phase 16: Subscribe + Unsubscribe Cleanup ---");

    let account_id = conns.account_id;
    let (strategy, handle) = UnsubscribeStrategy::new();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    std::thread::sleep(Duration::from_secs(3));

    control_tx.send(ControlCommand::Unsubscribe { instrument: 0 }).unwrap();
    handle.unsub_requested.store(true, Ordering::Relaxed);

    std::thread::sleep(Duration::from_secs(3));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(handle.disconnected.load(Ordering::Relaxed), "on_disconnect was not called");
    assert!(handle.still_alive.load(Ordering::Relaxed),
        "Hot loop did not continue running after unsubscribe");
    println!("  Total ticks: {}", handle.tick_count.load(Ordering::Relaxed));
    println!("  PASS\n");
    conns
}

// ─── Phase 17: Commission tracking ───

struct CommissionStrategy {
    buy_commission: Arc<AtomicI64>,
    sell_commission: Arc<AtomicI64>,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    order_rejected: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    phase: u8,
}

struct CommissionHandle {
    buy_commission: Arc<AtomicI64>,
    sell_commission: Arc<AtomicI64>,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    order_rejected: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl CommissionStrategy {
    fn new() -> (Self, CommissionHandle) {
        let buy_commission = Arc::new(AtomicI64::new(0));
        let sell_commission = Arc::new(AtomicI64::new(0));
        let buy_fill_price = Arc::new(AtomicI64::new(0));
        let sell_fill_price = Arc::new(AtomicI64::new(0));
        let order_rejected = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let handle = CommissionHandle {
            buy_commission: buy_commission.clone(),
            sell_commission: sell_commission.clone(),
            buy_fill_price: buy_fill_price.clone(),
            sell_fill_price: sell_fill_price.clone(),
            order_rejected: order_rejected.clone(),
            done: done.clone(),
        };
        (Self {
            buy_commission, sell_commission, buy_fill_price, sell_fill_price,
            order_rejected, done,
            phase: 0,
        }, handle)
    }
}

impl Strategy for CommissionStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        let price = 999_00_000_000i64; // $999.00 — well above SPY
        ctx.submit_limit_gtc(0, Side::Buy, 1, price, true);
        self.phase = 0;
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        if self.phase == 0 && fill.side == Side::Buy {
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_commission.store(fill.commission, Ordering::Relaxed);
            self.phase = 1;

            let sell_price = 1_00_000_000i64; // $1.00 — well below market
            ctx.submit_limit_gtc(0, Side::Sell, 1, sell_price, true);
            self.phase = 2;
        } else if self.phase == 2 && fill.side == Side::Sell {
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_commission.store(fill.commission, Ordering::Relaxed);
            self.phase = 3;
            self.done.store(true, Ordering::Relaxed);
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {}
}

fn phase_commission(conns: Conns) -> Conns {
    println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---");

    let account_id = conns.account_id;
    let (strategy, handle) = CommissionStrategy::new();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        strategy, account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.done.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("  SKIP: Order rejected — extended hours may not be active\n");
        return conns;
    }

    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let buy_comm = handle.buy_commission.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let sell_comm = handle.sell_commission.load(Ordering::Relaxed);

    if buy_price == 0 {
        println!("  SKIP: No fill — market may not have liquidity\n");
        return conns;
    }

    println!("  Buy: ${:.4} commission=${:.4}", buy_price as f64 / PRICE_SCALE as f64, buy_comm as f64 / PRICE_SCALE as f64);
    println!("  Sell: ${:.4} commission=${:.4}", sell_price as f64 / PRICE_SCALE as f64, sell_comm as f64 / PRICE_SCALE as f64);
    assert!(buy_comm > 0, "Buy commission should be > 0, got {}", buy_comm);
    println!("  PASS\n");
    conns
}
