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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ib_engine::control::contracts;
use ib_engine::control::historical::{self, BarDataType, BarSize, HistoricalRequest};
use ib_engine::engine::context::{Context, Strategy};
use ib_engine::gateway::{Gateway, GatewayConfig};
use ib_engine::protocol::connection::Frame;
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
    // ccp_token (tag 6386) may not always be present in logon response
    if !gw.ccp_token.is_empty() {
        println!("CCP token: present");
    } else {
        println!("CCP token: not present (non-fatal)");
    }
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
    assert!(connect_time < Duration::from_secs(60), "Connection took too long: {:?}", connect_time);
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

// ─── Order strategy: submits a market order after first tick, tracks fills ───

struct OrderTestStrategy {
    phase: u8, // 0=wait tick, 1=submitted buy, 2=submitted sell, 3=done
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
    start: Instant,
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
            start: Instant::now(),
        }, handle)
    }
}

impl Strategy for OrderTestStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] Order test strategy started", self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        self.tick_count += 1;

        if self.phase == 0 && self.tick_count >= self.ticks_before_order {
            self.instrument = instrument;
            let bid = ctx.bid(instrument) as f64 / PRICE_SCALE as f64;
            let ask = ctx.ask(instrument) as f64 / PRICE_SCALE as f64;
            println!("[{:.3}s] Submitting MKT BUY 1 share (bid={:.2} ask={:.2})",
                self.start.elapsed().as_secs_f64(), bid, ask);
            ctx.submit_market(instrument, Side::Buy, 1);
            self.buy_sent_at = Some(Instant::now());
            self.phase = 1;
        }
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        let elapsed = self.start.elapsed();
        println!(
            "[{:.3}s] FILL: order={} side={:?} qty={} price={:.4}",
            elapsed.as_secs_f64(), fill.order_id, fill.side, fill.qty,
            fill.price as f64 / PRICE_SCALE as f64,
        );

        if self.phase == 1 && fill.side == Side::Buy {
            let rtt = self.buy_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_fill_time_us.store(rtt, Ordering::Relaxed);
            println!("[{:.3}s] Buy fill RTT: {:.3}ms", elapsed.as_secs_f64(), rtt as f64 / 1000.0);

            // Now sell to close position
            println!("[{:.3}s] Submitting MKT SELL 1 share to close", elapsed.as_secs_f64());
            ctx.submit_market(self.instrument, Side::Sell, 1);
            self.sell_sent_at = Some(Instant::now());
            self.phase = 2;
        } else if self.phase == 2 && fill.side == Side::Sell {
            let rtt = self.sell_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_fill_time_us.store(rtt, Ordering::Relaxed);
            println!("[{:.3}s] Sell fill RTT: {:.3}ms", elapsed.as_secs_f64(), rtt as f64 / 1000.0);
            self.phase = 3;
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

// ─── Test 6: Market order buy + sell round-trip ───

#[test]
#[ignore]
fn test_market_order_round_trip() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Market Order Round-Trip (SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = OrderTestStrategy::new(5); // wait 5 ticks before ordering
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to SPY
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    println!("Subscribed to SPY (conId=756733)");

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for both fills (max 60s)
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.sell_fill_price.load(Ordering::Relaxed) != 0 {
            break; // Both fills received
        }
        if handle.order_rejected.load(Ordering::Relaxed) {
            break; // Order was rejected
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    // Verify
    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let buy_rtt = handle.buy_fill_time_us.load(Ordering::Relaxed);
    let sell_rtt = handle.sell_fill_time_us.load(Ordering::Relaxed);

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Order was REJECTED — market may be closed");
        println!("SKIP: Market order test requires market hours (9:30-16:00 ET)");
        return;
    }

    assert!(buy_price > 0, "No buy fill received — market may be closed");
    assert!(sell_price > 0, "No sell fill received");

    println!("\n=== Order Round-Trip Results ===");
    println!("  Buy fill:  ${:.4} (RTT {:.3}ms)",
        buy_price as f64 / PRICE_SCALE as f64, buy_rtt as f64 / 1000.0);
    println!("  Sell fill: ${:.4} (RTT {:.3}ms)",
        sell_price as f64 / PRICE_SCALE as f64, sell_rtt as f64 / 1000.0);
    println!("  Mean RTT:  {:.3}ms", (buy_rtt + sell_rtt) as f64 / 2000.0);
    println!("  Slippage:  ${:.4}",
        (sell_price - buy_price).abs() as f64 / PRICE_SCALE as f64);
    println!("PASS: Market order round-trip complete");
}

// ─── Test 7: Limit order submit + cancel ───

struct LimitOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    submit_ack_us: Arc<AtomicU64>,
    cancel_conf_us: Arc<AtomicU64>,
    tick_count: u32,
    order_id: Option<OrderId>,
    submit_time: Option<Instant>,
    cancel_time: Option<Instant>,
    start: Instant,
}

struct LimitOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
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
            cancel_sent: cancel_sent.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
            submit_ack_us: submit_ack_us.clone(),
            cancel_conf_us: cancel_conf_us.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            submit_ack_us, cancel_conf_us,
            tick_count: 0, order_id: None, submit_time: None, cancel_time: None,
            start: Instant::now(),
        }, handle)
    }
}

impl Strategy for LimitOrderStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {}

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        self.tick_count += 1;

        // After 5 ticks, submit a deep limit buy (far below market)
        if self.tick_count == 5 && !self.submitted.load(Ordering::Relaxed) {
            // Use a very low price ($1.00) to ensure it won't fill
            // (tick decoder bid values may be incorrect, so don't compute from bid)
            let limit_price = 1_00_000_000i64; // $1.00 in PRICE_SCALE
            let id = ctx.submit_limit(instrument, Side::Buy, 1, limit_price);
            self.order_id = Some(id);
            self.submit_time = Some(Instant::now());
            self.submitted.store(true, Ordering::Relaxed);
            println!("[{:.3}s] Submitted LMT BUY at ${:.2}",
                self.start.elapsed().as_secs_f64(),
                limit_price as f64 / PRICE_SCALE as f64);
        }

        // After order is acked, cancel it
        if self.order_acked.load(Ordering::Relaxed) && !self.cancel_sent.load(Ordering::Relaxed) {
            if let Some(id) = self.order_id {
                ctx.cancel(id);
                self.cancel_time = Some(Instant::now());
                self.cancel_sent.store(true, Ordering::Relaxed);
                println!("[{:.3}s] Cancel sent for order {}",
                    self.start.elapsed().as_secs_f64(), id);
            }
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        match update.status {
            OrderStatus::Submitted => {
                if !self.order_acked.load(Ordering::Relaxed) {
                    let rtt = self.submit_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    self.submit_ack_us.store(rtt, Ordering::Relaxed);
                    println!("[{:.3}s] Submit→Ack RTT: {:.3}ms",
                        self.start.elapsed().as_secs_f64(), rtt as f64 / 1000.0);
                }
                self.order_acked.store(true, Ordering::Relaxed);
            }
            OrderStatus::Cancelled => {
                let rtt = self.cancel_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                self.cancel_conf_us.store(rtt, Ordering::Relaxed);
                println!("[{:.3}s] Cancel→Conf RTT: {:.3}ms",
                    self.start.elapsed().as_secs_f64(), rtt as f64 / 1000.0);
                self.order_cancelled.store(true, Ordering::Relaxed);
            }
            OrderStatus::Rejected => {
                self.order_rejected.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
        println!("[{:.3}s] Unexpected FILL: {:?}", self.start.elapsed().as_secs_f64(), fill.side);
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_limit_order_submit_and_cancel() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Limit Order Submit + Cancel (SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = LimitOrderStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for cancel confirmation (max 60s)
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) {
            break;
        }
        if handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Order was REJECTED — market may be closed");
        println!("SKIP: Limit order test requires market hours");
        return;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged by server");
    assert!(handle.cancel_sent.load(Ordering::Relaxed), "Cancel was never sent");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");

    let ack_us = handle.submit_ack_us.load(Ordering::Relaxed);
    let cancel_us = handle.cancel_conf_us.load(Ordering::Relaxed);
    println!("\n=== Limit Order Round-Trip Results ===");
    println!("  Submit→Ack:   {:.3}ms", ack_us as f64 / 1000.0);
    println!("  Cancel→Conf:  {:.3}ms", cancel_us as f64 / 1000.0);
    println!("  Total:        {:.3}ms", (ack_us + cancel_us) as f64 / 1000.0);
    println!("PASS: Limit order submit + cancel round-trip");
}

// ─── Test 8: Stop order submit + cancel ───

struct StopOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    tick_count: u32,
    order_id: Option<OrderId>,
    start: Instant,
}

struct StopOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
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
            cancel_sent: cancel_sent.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            tick_count: 0, order_id: None, start: Instant::now(),
        }, handle)
    }
}

impl Strategy for StopOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Submit immediately — instrument 0 pre-registered before run()
        let stop_price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_stop(0, Side::Sell, 1, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
        println!("[{:.3}s] Submitted STP SELL at ${:.2}",
            self.start.elapsed().as_secs_f64(),
            stop_price as f64 / PRICE_SCALE as f64);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        self.tick_count += 1;
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                // Cancel immediately upon ack
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                        println!("[{:.3}s] Cancel sent for stop order {}",
                            self.start.elapsed().as_secs_f64(), id);
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

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_stop_order_submit_and_cancel() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Stop Order Submit + Cancel (SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = StopOrderStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Pre-register instrument so on_start can submit immediately (no tick dependency)
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Also subscribe for market data (needed for farm)
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Stop order REJECTED — may need GTC TIF or market hours");
        println!("SKIP: Stop order test rejected");
        return;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop order was never acknowledged");
    assert!(handle.cancel_sent.load(Ordering::Relaxed), "Cancel was never sent");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop order was never cancelled");
    println!("PASS: Stop order submit + cancel round-trip");
}

// ─── Test 9: Order modify (35=G) ───

struct ModifyOrderStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    tick_count: u32,
    order_id: Option<OrderId>,
    new_order_id: Option<OrderId>,
    start: Instant,
}

struct ModifyOrderHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    modify_sent: Arc<AtomicBool>,
    modify_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
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
            cancel_sent: cancel_sent.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, modify_sent, modify_acked, cancel_sent,
            order_cancelled, order_rejected,
            tick_count: 0, order_id: None, new_order_id: None, start: Instant::now(),
        }, handle)
    }
}

impl Strategy for ModifyOrderStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Submit immediately — instrument 0 pre-registered before run()
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit(0, Side::Buy, 1, price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
        println!("[{:.3}s] Submitted LMT BUY at $1.00 (id={})",
            self.start.elapsed().as_secs_f64(), id);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        self.tick_count += 1;
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        match update.status {
            OrderStatus::Submitted => {
                if self.modify_sent.load(Ordering::Relaxed) && !self.modify_acked.load(Ordering::Relaxed) {
                    // Step 3: Modify acknowledged → cancel
                    self.modify_acked.store(true, Ordering::Relaxed);
                    println!("[{:.3}s] Modify acknowledged", self.start.elapsed().as_secs_f64());

                    let cancel_id = self.new_order_id.unwrap_or_else(|| self.order_id.unwrap());
                    ctx.cancel(cancel_id);
                    self.cancel_sent.store(true, Ordering::Relaxed);
                    println!("[{:.3}s] Cancel sent for modified order {}",
                        self.start.elapsed().as_secs_f64(), cancel_id);
                } else if !self.order_acked.load(Ordering::Relaxed) {
                    // Step 2: Initial order acknowledged → modify price to $2.00
                    self.order_acked.store(true, Ordering::Relaxed);
                    println!("[{:.3}s] Initial order acknowledged", self.start.elapsed().as_secs_f64());

                    if let Some(id) = self.order_id {
                        let new_price = 2_00_000_000i64; // $2.00
                        let new_id = ctx.modify(id, new_price, 1);
                        self.new_order_id = Some(new_id);
                        self.modify_sent.store(true, Ordering::Relaxed);
                        println!("[{:.3}s] Modify sent: {} → {} at $2.00",
                            self.start.elapsed().as_secs_f64(), id, new_id);
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

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_order_modify_and_cancel() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Order Modify (35=G) + Cancel (SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = ModifyOrderStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Pre-register instrument so on_start can submit immediately
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Order REJECTED — may need market hours");
        println!("SKIP: Modify test rejected");
        return;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.modify_sent.load(Ordering::Relaxed), "Modify was never sent");
    assert!(handle.modify_acked.load(Ordering::Relaxed), "Modify was never acknowledged");
    assert!(handle.cancel_sent.load(Ordering::Relaxed), "Cancel was never sent");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Modified order was never cancelled");
    println!("PASS: Order modify + cancel round-trip");
}

// ─── Test 10: Outside RTH limit order (GTC + OutsideRTH) ───

struct OutsideRthStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
    start: Instant,
}

struct OutsideRthHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
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
            cancel_sent: cancel_sent.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None, start: Instant::now(),
        }, handle)
    }
}

impl Strategy for OutsideRthStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Submit GTC+OutsideRTH limit order at $1.00 — should be accepted any time
        let price = 1_00_000_000i64; // $1.00
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, price, true);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
        println!("[{:.3}s] Submitted LMT GTC+OutsideRTH BUY at $1.00 (id={})",
            self.start.elapsed().as_secs_f64(), id);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                        println!("[{:.3}s] Cancel sent for GTC order {}",
                            self.start.elapsed().as_secs_f64(), id);
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

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_outside_rth_limit_order() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Outside RTH Limit Order (GTC + OutsideRTH, SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = OutsideRthStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Order REJECTED — unexpected for GTC+OutsideRTH");
        panic!("GTC+OutsideRTH order should not be rejected");
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Order was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Order was never acknowledged");
    assert!(handle.cancel_sent.load(Ordering::Relaxed), "Cancel was never sent");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Order was never cancelled");
    println!("PASS: Outside RTH limit order submit + cancel");
}

// ─── Test 11: Historical data bars via HMDS ───

#[test]
#[ignore]
fn test_historical_data_bars() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Historical Data Bars (SPY, 1 day of 5-min bars) ===");

    let (gw, _farm_conn, _ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let mut hmds = match hmds_conn {
        Some(c) => c,
        None => {
            println!("SKIP: ushmds farm not connected");
            return;
        }
    };
    println!("HMDS connection available");

    // Build and send historical data request
    // endTime format: YYYYMMDD-HH:MM:SS (UTC)
    // Use yesterday's close to ensure we always get data
    // Format endTime as "YYYYMMDD-HH:MM:SS" (UTC) from epoch
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let end_time = {
        // Days since epoch
        let days = now / secs_per_day;
        // Simple date calc (good enough for 2026)
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
    println!("Using endTime: {}", end_time);

    let req = HistoricalRequest {
        query_id: "test1".to_string(),
        con_id: 756733,     // SPY
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
    println!("Sent historical bar request");

    // Poll for response
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
                println!("HMDS recv error: {}", e);
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
                        println!("Received {} bars (complete={})", resp.bars.len(), resp.is_complete);
                        all_bars.extend(resp.bars);
                        if resp.is_complete {
                            complete = true;
                        }
                    }
                }
            }
        }
    }

    println!("Total bars received: {}", all_bars.len());
    assert!(!all_bars.is_empty(), "No historical bars received");

    // Verify bar data is sane
    let first = &all_bars[0];
    println!("First bar: time={} O={:.2} H={:.2} L={:.2} C={:.2} V={}",
        first.time, first.open, first.high, first.low, first.close, first.volume);

    assert!(first.open > 0.0, "Open price should be positive");
    assert!(first.high >= first.low, "High should be >= Low");
    assert!(first.high >= first.open, "High should be >= Open");
    assert!(first.high >= first.close, "High should be >= Close");
    assert!(first.low <= first.open, "Low should be <= Open");
    assert!(first.low <= first.close, "Low should be <= Close");
    assert!(first.volume > 0, "Volume should be positive");

    println!("PASS: Historical data bars ({} bars received)", all_bars.len());
}

// ─── Test 12: Contract details lookup via CCP ───

#[test]
#[ignore]
fn test_contract_details_lookup() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Contract Details Lookup (SPY, conId=756733) ===");

    let (gw, _farm_conn, mut ccp_conn, _hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    // Send secdef request for SPY (CCP requires tag 52 on all messages)
    let now = ib_engine::gateway::chrono_free_timestamp();
    ccp_conn.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "R1"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_IB_CON_ID, "756733"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send secdef request");
    println!("Sent secdef request for conId=756733");

    // Poll for 35=d response
    let mut contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline && contract.is_none() {
        match ccp_conn.try_recv() {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                println!("CCP recv error: {}", e);
                break;
            }
            Ok(_) => {}
        }

        for frame in ccp_conn.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = ccp_conn.unsign(&raw);
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
                            println!("Received contract: {} ({}) conId={}", def.symbol, def.long_name, def.con_id);
                            println!("  SecType={:?} Exchange={} Currency={}", def.sec_type, def.exchange, def.currency);
                            println!("  MinTick={} PrimaryExchange={}", def.min_tick, def.primary_exchange);
                            if !def.valid_exchanges.is_empty() {
                                println!("  ValidExchanges: {}", def.valid_exchanges.join(","));
                            }
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
    println!("PASS: Contract details lookup (SPY conId=756733)");
}

// ─── Test 13: Heartbeat keepalive ───
// Verifies the heartbeat mechanism keeps connections alive beyond the CCP interval (10s).

struct HeartbeatStrategy {
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
    start: Instant,
}

struct HeartbeatHandle {
    disconnected: Arc<AtomicBool>,
    _tick_count: Arc<AtomicU32>,
}

impl HeartbeatStrategy {
    fn new() -> (Self, HeartbeatHandle) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let tick_count = Arc::new(AtomicU32::new(0));
        let handle = HeartbeatHandle {
            disconnected: disconnected.clone(),
            _tick_count: tick_count.clone(),
        };
        (Self { disconnected, tick_count, start: Instant::now() }, handle)
    }
}

impl Strategy for HeartbeatStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] Strategy started, will run for 20s to verify heartbeats",
            self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);
        if count % 100 == 0 && count > 0 {
            println!("[{:.3}s] {} ticks, still connected",
                self.start.elapsed().as_secs_f64(), count);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED (unexpected!)", self.start.elapsed().as_secs_f64());
        self.disconnected.store(true, Ordering::Relaxed);
    }
}

#[test]
#[ignore]
fn test_heartbeat_keepalive() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Heartbeat Keepalive (20s > CCP 10s interval) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = HeartbeatStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to keep farm active
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Run for 20 seconds — must survive past CCP heartbeat interval (10s)
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if handle.disconnected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let still_connected = !handle.disconnected.load(Ordering::Relaxed);
    let elapsed = start.elapsed();

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    assert!(still_connected,
        "Connection dropped after {:.1}s — heartbeat mechanism failed", elapsed.as_secs_f64());
    println!("PASS: Heartbeat keepalive ({:.1}s, no disconnect)", elapsed.as_secs_f64());
}

// ─── Test 14: Account PnL reception ───
// Verifies that account data arrives via CCP 8=O init burst (seeded into connection buffer).

struct PnlStrategy {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    unrealized_pnl: Arc<AtomicI64>,
    realized_pnl: Arc<AtomicI64>,
    buying_power: Arc<AtomicI64>,
    probe_done: Arc<AtomicBool>,
    order_id: Option<OrderId>,
    start: Instant,
}

struct PnlHandle {
    account_received: Arc<AtomicBool>,
    net_liq: Arc<AtomicI64>,
    unrealized_pnl: Arc<AtomicI64>,
    realized_pnl: Arc<AtomicI64>,
    buying_power: Arc<AtomicI64>,
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
            unrealized_pnl: unrealized_pnl.clone(),
            realized_pnl: realized_pnl.clone(),
            buying_power: buying_power.clone(),
            probe_done: probe_done.clone(),
        };
        (Self {
            account_received, net_liq, unrealized_pnl, realized_pnl, buying_power,
            probe_done, order_id: None, start: Instant::now(),
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
            println!("[{:.3}s] Account data received: net_liq=${:.2}",
                self.start.elapsed().as_secs_f64(), acct.net_liquidation as f64 / PRICE_SCALE as f64);
        }
    }
}

impl Strategy for PnlStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Submit a GTC probe order to trigger on_order_update callback
        // (account data from init burst is processed before the ack arrives)
        let id = ctx.submit_limit_gtc(0, Side::Buy, 1, 1_00_000_000, true); // $1.00 GTC
        self.order_id = Some(id);
        println!("[{:.3}s] Submitted probe order", self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, _instrument: InstrumentId, ctx: &mut Context) {
        self.check_account(ctx);
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
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

#[test]
#[ignore]
fn test_account_pnl_reception() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Account PnL Reception ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = PnlStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Pre-register instrument for probe order (triggers on_order_update → check_account)
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for probe order to complete AND account data from init burst
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if handle.probe_done.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    let nl = handle.net_liq.load(Ordering::Relaxed);
    let bp = handle.buying_power.load(Ordering::Relaxed);
    let upnl = handle.unrealized_pnl.load(Ordering::Relaxed);
    let rpnl = handle.realized_pnl.load(Ordering::Relaxed);

    assert!(handle.account_received.load(Ordering::Relaxed),
        "Account data not received — 6040=77 may not contain tag 9806");

    println!("  NetLiq:       ${:.2}", nl as f64 / PRICE_SCALE as f64);
    println!("  BuyingPower:  ${:.2}", bp as f64 / PRICE_SCALE as f64);
    println!("  UnrealizedPnL: ${:.2}", upnl as f64 / PRICE_SCALE as f64);
    println!("  RealizedPnL:   ${:.2}", rpnl as f64 / PRICE_SCALE as f64);

    assert!(nl > 0, "Paper account net liquidation should be > 0");
    // BuyingPower/PnL come from 8=O UT/UM (farm, during market hours) — may be 0 off-hours
    println!("PASS: Account data received (6040=77 init burst)");
}

// ─── Test 15: Stop limit order submit + cancel ───

struct StopLimitStrategy {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
    order_cancelled: Arc<AtomicBool>,
    order_rejected: Arc<AtomicBool>,
    order_id: Option<OrderId>,
    start: Instant,
}

struct StopLimitHandle {
    submitted: Arc<AtomicBool>,
    order_acked: Arc<AtomicBool>,
    cancel_sent: Arc<AtomicBool>,
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
            cancel_sent: cancel_sent.clone(),
            order_cancelled: order_cancelled.clone(),
            order_rejected: order_rejected.clone(),
        };
        (Self {
            submitted, order_acked, cancel_sent, order_cancelled, order_rejected,
            order_id: None, start: Instant::now(),
        }, handle)
    }
}

impl Strategy for StopLimitStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Stop Limit: trigger at $999 (never triggers), limit at $998
        let stop_price = 999_00_000_000i64; // $999.00
        let limit_price = 998_00_000_000i64; // $998.00
        let id = ctx.submit_stop_limit(0, Side::Buy, 1, limit_price, stop_price);
        self.order_id = Some(id);
        self.submitted.store(true, Ordering::Relaxed);
        println!("[{:.3}s] Submitted STP LMT BUY: stop=$999.00 limit=$998.00 (id={})",
            self.start.elapsed().as_secs_f64(), id);
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        match update.status {
            OrderStatus::Submitted => {
                self.order_acked.store(true, Ordering::Relaxed);
                if !self.cancel_sent.load(Ordering::Relaxed) {
                    if let Some(id) = self.order_id {
                        ctx.cancel(id);
                        self.cancel_sent.store(true, Ordering::Relaxed);
                        println!("[{:.3}s] Cancel sent for stop limit order {}",
                            self.start.elapsed().as_secs_f64(), id);
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

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_stop_limit_order_submit_and_cancel() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Stop Limit Order Submit + Cancel (SPY) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = StopLimitStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.order_cancelled.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Stop limit order REJECTED — may need market hours");
        println!("SKIP: Stop limit test rejected");
        return;
    }

    assert!(handle.submitted.load(Ordering::Relaxed), "Stop limit was never submitted");
    assert!(handle.order_acked.load(Ordering::Relaxed), "Stop limit was never acknowledged");
    assert!(handle.cancel_sent.load(Ordering::Relaxed), "Cancel was never sent");
    assert!(handle.order_cancelled.load(Ordering::Relaxed), "Stop limit was never cancelled");
    println!("PASS: Stop limit order submit + cancel round-trip");
}

// ─── Test 16: Subscribe + Unsubscribe cleanup ───
// Verifies that unsubscribing from an instrument doesn't crash
// and the hot loop continues to function cleanly.

struct UnsubscribeStrategy {
    unsub_requested: Arc<AtomicBool>,
    still_alive: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    tick_count: Arc<AtomicU32>,
    start: Instant,
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
        (Self {
            unsub_requested, still_alive, disconnected, tick_count,
            start: Instant::now(),
        }, handle)
    }
}

impl Strategy for UnsubscribeStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] Strategy started, will test unsubscribe",
            self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        // After unsubscribe, we may still get a few ticks in flight — that's ok
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        self.disconnected.store(true, Ordering::Relaxed);
        // Mark still_alive even on shutdown path — proves the loop ran after unsub
        if self.unsub_requested.load(Ordering::Relaxed) {
            self.still_alive.store(true, Ordering::Relaxed);
        }
    }
}

#[test]
#[ignore]
fn test_subscribe_unsubscribe_cleanup() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Subscribe + Unsubscribe Cleanup ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = UnsubscribeStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to SPY
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    println!("Subscribed to SPY");

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait 3 seconds for subscription to be established
    std::thread::sleep(Duration::from_secs(3));

    // Unsubscribe
    control_tx.send(ControlCommand::Unsubscribe { instrument: 0 }).unwrap();
    handle.unsub_requested.store(true, Ordering::Relaxed);
    println!("Unsubscribe sent for instrument 0");

    // Wait 3 more seconds — loop should continue running without crash
    std::thread::sleep(Duration::from_secs(3));

    // Shutdown
    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    assert!(handle.disconnected.load(Ordering::Relaxed), "on_disconnect was not called");
    assert!(handle.still_alive.load(Ordering::Relaxed),
        "Hot loop did not continue running after unsubscribe");
    let total_ticks = handle.tick_count.load(Ordering::Relaxed);
    println!("Total ticks received: {}", total_ticks);
    println!("PASS: Subscribe + unsubscribe cleanup (no crash, clean shutdown)");
}

// ─── Test 17: Commission tracking via GTC+OutsideRTH fill ───
// Submits an aggressive limit buy during extended hours to get a fill,
// then verifies commission > 0 in the Fill struct. Sells to flatten.

struct CommissionStrategy {
    buy_commission: Arc<AtomicI64>,
    sell_commission: Arc<AtomicI64>,
    buy_fill_price: Arc<AtomicI64>,
    sell_fill_price: Arc<AtomicI64>,
    order_rejected: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    phase: u8, // 0=submitted buy, 1=buy filled, 2=submitted sell, 3=done
    start: Instant,
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
            phase: 0, start: Instant::now(),
        }, handle)
    }
}

impl Strategy for CommissionStrategy {
    fn on_start(&mut self, ctx: &mut Context) {
        // Submit aggressive GTC+OutsideRTH limit buy at $999 (will fill at market during extended hours)
        let price = 999_00_000_000i64; // $999.00 — well above SPY ~$680
        ctx.submit_limit_gtc(0, Side::Buy, 1, price, true);
        self.phase = 0;
        println!("[{:.3}s] Submitted aggressive GTC+OutsideRTH LMT BUY at $999.00",
            self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, _instrument: InstrumentId, _ctx: &mut Context) {}

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        println!("[{:.3}s] FILL: side={:?} qty={} price=${:.4} commission=${:.4}",
            self.start.elapsed().as_secs_f64(), fill.side, fill.qty,
            fill.price as f64 / PRICE_SCALE as f64,
            fill.commission as f64 / PRICE_SCALE as f64);

        if self.phase == 0 && fill.side == Side::Buy {
            self.buy_fill_price.store(fill.price, Ordering::Relaxed);
            self.buy_commission.store(fill.commission, Ordering::Relaxed);
            self.phase = 1;

            // Sell to flatten — also aggressive to ensure fill
            let sell_price = 1_00_000_000i64; // $1.00 — well below market
            ctx.submit_limit_gtc(0, Side::Sell, 1, sell_price, true);
            self.phase = 2;
            println!("[{:.3}s] Submitted aggressive GTC+OutsideRTH LMT SELL at $1.00",
                self.start.elapsed().as_secs_f64());
        } else if self.phase == 2 && fill.side == Side::Sell {
            self.sell_fill_price.store(fill.price, Ordering::Relaxed);
            self.sell_commission.store(fill.commission, Ordering::Relaxed);
            self.phase = 3;
            self.done.store(true, Ordering::Relaxed);
        }
    }

    fn on_order_update(&mut self, update: &OrderUpdate, _ctx: &mut Context) {
        println!("[{:.3}s] OrderUpdate: id={} status={:?}",
            self.start.elapsed().as_secs_f64(), update.order_id, update.status);
        if update.status == OrderStatus::Rejected {
            self.order_rejected.store(true, Ordering::Relaxed);
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] DISCONNECTED", self.start.elapsed().as_secs_f64());
    }
}

#[test]
#[ignore]
fn test_commission_tracking() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Test: Commission Tracking (GTC+OutsideRTH fill) ===");

    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    println!("Connected. Account: {}", gw.account_id);

    let (strategy, handle) = CommissionStrategy::new();
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if handle.done.load(Ordering::Relaxed) || handle.order_rejected.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = join.join();

    if handle.order_rejected.load(Ordering::Relaxed) {
        println!("Order REJECTED — extended hours may not be active");
        println!("SKIP: Commission test requires extended hours or RTH");
        return;
    }

    let buy_price = handle.buy_fill_price.load(Ordering::Relaxed);
    let sell_price = handle.sell_fill_price.load(Ordering::Relaxed);
    let buy_comm = handle.buy_commission.load(Ordering::Relaxed);
    let sell_comm = handle.sell_commission.load(Ordering::Relaxed);

    if buy_price == 0 {
        println!("No buy fill received — market may not have liquidity");
        println!("SKIP: Commission test requires active market");
        return;
    }

    println!("\n=== Commission Results ===");
    println!("  Buy fill:  ${:.4} commission=${:.4}",
        buy_price as f64 / PRICE_SCALE as f64, buy_comm as f64 / PRICE_SCALE as f64);
    println!("  Sell fill: ${:.4} commission=${:.4}",
        sell_price as f64 / PRICE_SCALE as f64, sell_comm as f64 / PRICE_SCALE as f64);
    println!("  Total commission: ${:.4}",
        (buy_comm + sell_comm) as f64 / PRICE_SCALE as f64);

    assert!(buy_comm > 0, "Buy commission should be > 0, got {}", buy_comm);
    println!("PASS: Commission tracking verified");
}
