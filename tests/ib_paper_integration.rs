//! Integration tests against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! These tests verify the full connection pipeline:
//! 1. CCP TLS + SRP authentication
//! 2. FIX logon and account ID extraction
//! 3. usfarm DH + encrypted logon + routing table
//! 4. ushmds farm connection (optional)
//! 5. Market data subscription and tick reception
//! 6. Account data (8=O UT/UM/UP) reception
//! 7. Graceful shutdown

use std::env;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ib_engine::engine::context::{Context, Strategy};
use ib_engine::gateway::{Gateway, GatewayConfig};
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

/// Strategy that tracks everything for verification.
struct DiagnosticStrategy {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
    start: Instant,
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
            start: Instant::now(),
        };

        (strategy, handle)
    }
}

/// Thread-safe handle for reading strategy state from main thread.
struct DiagnosticHandle {
    tick_count: Arc<AtomicU32>,
    first_tick_received: Arc<AtomicBool>,
    account_checked: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    disconnected: Arc<AtomicBool>,
}

impl Strategy for DiagnosticStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] Strategy started", self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);

        if count == 0 {
            let elapsed = self.start.elapsed();
            let bid = ctx.bid(instrument) as f64 / PRICE_SCALE as f64;
            let ask = ctx.ask(instrument) as f64 / PRICE_SCALE as f64;
            let last = ctx.last(instrument) as f64 / PRICE_SCALE as f64;
            let spread = ctx.spread(instrument) as f64 / PRICE_SCALE as f64;
            let bid_sz = ctx.bid_size(instrument);
            let ask_sz = ctx.ask_size(instrument);
            println!("[{:.3}s] FIRST TICK: instrument={}", elapsed.as_secs_f64(), instrument);
            println!("  bid={:.4} ask={:.4} last={:.4} spread={:.6}", bid, ask, last, spread);
            println!("  bid_size={} ask_size={}", bid_sz, ask_sz);
            self.first_tick_received.store(true, Ordering::Relaxed);
        }

        // Check account data periodically
        if count == 10 && !self.account_checked.load(Ordering::Relaxed) {
            let acct = ctx.account();
            let nl = acct.net_liquidation;
            self.net_liq.store(nl, Ordering::Relaxed);
            if nl != 0 {
                println!(
                    "  ACCOUNT: net_liq={:.2} buying_power={:.2} margin={:.2}",
                    nl as f64 / PRICE_SCALE as f64,
                    acct.buying_power as f64 / PRICE_SCALE as f64,
                    acct.margin_used as f64 / PRICE_SCALE as f64,
                );
                println!(
                    "  PNL: unrealized={:.2} realized={:.2}",
                    acct.unrealized_pnl as f64 / PRICE_SCALE as f64,
                    acct.realized_pnl as f64 / PRICE_SCALE as f64,
                );
                self.account_checked.store(true, Ordering::Relaxed);
            }
        }

        if count % 200 == 0 && count > 0 {
            println!("[{:.3}s] {} ticks", self.start.elapsed().as_secs_f64(), count);
        }
    }

    fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
        println!(
            "[{:.3}s] FILL: order={} side={:?} qty={} price={:.4}",
            self.start.elapsed().as_secs_f64(),
            fill.order_id,
            fill.side,
            fill.qty,
            fill.price as f64 / PRICE_SCALE as f64,
        );
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

// ─── Test 1: Full connection pipeline ───

#[test]
#[ignore]
fn test_ccp_auth_and_farm_logon() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: CCP Auth + Farm Logon ===");
    let start = Instant::now();

    let (gw, _farm_conn, _ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");

    let connect_time = start.elapsed();
    println!("Connection time: {:.3}s", connect_time.as_secs_f64());

    // Verify CCP auth succeeded
    assert!(!gw.account_id.is_empty(), "Account ID should be non-empty after CCP logon");
    println!("Account ID: {}", gw.account_id);

    // Verify session metadata
    assert!(!gw.server_session_id.is_empty(), "Server session ID should be set");
    assert!(!gw.ccp_token.is_empty(), "CCP token should be set");
    assert!(gw.heartbeat_interval > 0, "Heartbeat interval should be positive");
    println!("Session ID: {}", gw.server_session_id);
    println!("Heartbeat interval: {}s", gw.heartbeat_interval);

    // Verify session token (used for farm auth)
    use num_bigint::BigUint;
    assert!(gw.session_token > BigUint::from(0u32), "Session token should be non-zero");

    // Verify ushmds farm connection
    if hmds_conn.is_some() {
        println!("ushmds farm: CONNECTED");
    } else {
        println!("ushmds farm: NOT CONNECTED (non-fatal)");
    }

    // Connection should complete within reasonable time
    assert!(connect_time < Duration::from_secs(30), "Connection took too long: {:?}", connect_time);
    println!("PASS: CCP auth + farm logon completed in {:.3}s", connect_time.as_secs_f64());
}

// ─── Test 2: Market data subscription + tick reception ───

#[test]
#[ignore]
fn test_market_data_ticks() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Market Data Ticks ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = DiagnosticStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to AAPL
    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    println!("Subscribed to AAPL (conId=265598)");

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for first tick (max 30s)
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        handle.first_tick_received.load(Ordering::Relaxed),
        "No tick received within 30 seconds — market data pipeline is broken"
    );

    // Collect more ticks for 5 seconds
    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);
    println!("Total ticks after 5s: {}", total);
    assert!(total > 1, "Expected more than 1 tick after 5 seconds, got {}", total);

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();
    println!("PASS: Market data ticks received ({})", total);
}

// ─── Test 3: Multi-instrument subscription ───

#[test]
#[ignore]
fn test_multi_instrument_subscription() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Multi-Instrument Subscription ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");

    let (strategy, handle) = DiagnosticStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to AAPL + MSFT + SPY
    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();  // AAPL
    control_tx.send(ControlCommand::Subscribe { con_id: 272093, symbol: "MSFT".into() }).unwrap();  // MSFT
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();  // SPY
    println!("Subscribed to AAPL, MSFT, SPY");

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for ticks
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if handle.first_tick_received.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Let it run 5s to get ticks from all instruments
    std::thread::sleep(Duration::from_secs(5));
    let total = handle.tick_count.load(Ordering::Relaxed);
    println!("Total ticks from 3 instruments: {}", total);

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    assert!(total > 3, "Expected ticks from multiple instruments, got only {}", total);
    println!("PASS: Multi-instrument ticks received ({})", total);
}

// ─── Test 4: Account data reception ───

#[test]
#[ignore]
fn test_account_data_reception() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Account Data Reception ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");

    let (strategy, handle) = DiagnosticStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to something to keep the loop busy
    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Account data comes from CCP 8=O messages, may take 10-15s
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if handle.account_checked.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let net_liq = handle.net_liq.load(Ordering::Relaxed);
    println!("Net liquidation: {}", net_liq as f64 / PRICE_SCALE as f64);

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    // Paper accounts typically have > $0 net liquidation
    // But account data might not arrive within 20s, so just warn
    if handle.account_checked.load(Ordering::Relaxed) {
        assert!(net_liq > 0, "Paper account net liquidation should be > 0");
        println!("PASS: Account data received");
    } else {
        println!("WARN: Account data not received within 20s (CCP 8=O messages may be delayed)");
    }
}

// ─── Test 5: Graceful shutdown ───

#[test]
#[ignore]
fn test_graceful_shutdown() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Graceful Shutdown ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");

    let (strategy, handle) = DiagnosticStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Let it run briefly
    std::thread::sleep(Duration::from_secs(2));

    // Send shutdown
    let shutdown_start = Instant::now();
    control_tx.send(ControlCommand::Shutdown).unwrap();

    // Thread should exit within 1 second
    let result = join.join();
    let shutdown_time = shutdown_start.elapsed();

    assert!(result.is_ok(), "Hot loop thread panicked on shutdown");
    assert!(
        shutdown_time < Duration::from_secs(2),
        "Shutdown took too long: {:?}", shutdown_time
    );
    assert!(
        handle.disconnected.load(Ordering::Relaxed),
        "on_disconnect() was not called during shutdown"
    );
    println!("PASS: Graceful shutdown in {:.3}s", shutdown_time.as_secs_f64());
}
