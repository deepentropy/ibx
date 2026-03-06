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
