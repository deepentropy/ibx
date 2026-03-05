//! Integration test against IB paper account.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test ib_paper_integration -- --ignored --nocapture
//!
//! This test connects to IB, subscribes to AAPL market data, and verifies
//! that ticks flow through to the strategy within 30 seconds.

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ib_engine::engine::context::{Context, Strategy};
use ib_engine::gateway::{Gateway, GatewayConfig};
use ib_engine::types::*;

/// Strategy that counts ticks and records latency.
struct TickCounter {
    tick_count: Arc<AtomicU32>,
    first_tick: Arc<AtomicBool>,
    start: Instant,
}

impl Strategy for TickCounter {
    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let count = self.tick_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            let elapsed = self.start.elapsed();
            println!("First tick received after {:.3}s", elapsed.as_secs_f64());
            println!(
                "  instrument={} bid={} ask={} last={}",
                instrument,
                ctx.bid(instrument) as f64 / PRICE_SCALE as f64,
                ctx.ask(instrument) as f64 / PRICE_SCALE as f64,
                ctx.last(instrument) as f64 / PRICE_SCALE as f64,
            );
            self.first_tick.store(true, Ordering::Relaxed);
        }
        if count % 100 == 0 && count > 0 {
            println!("  {} ticks received", count);
        }
    }

    fn on_fill(&mut self, fill: &Fill, _ctx: &mut Context) {
        println!("Fill: {:?}", fill);
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!("Disconnected!");
    }
}

#[test]
#[ignore] // Only run manually with IB credentials
fn connect_and_receive_ticks() {
    env_logger::init();

    let username = match env::var("IB_USERNAME") {
        Ok(u) => u,
        Err(_) => {
            println!("Skipping: IB_USERNAME not set");
            return;
        }
    };
    let password = match env::var("IB_PASSWORD") {
        Ok(p) => p,
        Err(_) => {
            println!("Skipping: IB_PASSWORD not set");
            return;
        }
    };
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());

    let config = GatewayConfig {
        username,
        password,
        host,
        paper: true,
    };

    println!("Connecting to IB paper account...");
    let start = Instant::now();
    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Failed to connect to IB");
    println!("Connected in {:.3}s. Account: {}", start.elapsed().as_secs_f64(), gw.account_id);

    let tick_count = Arc::new(AtomicU32::new(0));
    let first_tick = Arc::new(AtomicBool::new(false));

    let strategy = TickCounter {
        tick_count: tick_count.clone(),
        first_tick: first_tick.clone(),
        start: Instant::now(),
    };

    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to AAPL (conId = 265598)
    control_tx
        .send(ControlCommand::Subscribe { con_id: 265598 })
        .expect("Failed to send subscribe");

    println!("Subscribed to AAPL. Waiting for ticks (max 30s)...");

    // Run hot loop in a separate thread
    let first_tick_clone = first_tick.clone();
    let handle = std::thread::spawn(move || {
        hot_loop.run();
    });

    // Wait for first tick or timeout
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if first_tick_clone.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Let it run a bit more to collect ticks
    if first_tick.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(3));
    }

    let total = tick_count.load(Ordering::Relaxed);
    println!("Total ticks received: {}", total);

    // Shutdown
    let _ = control_tx.send(ControlCommand::Shutdown);
    let _ = handle.join();

    assert!(total > 0, "Expected at least one tick within 30 seconds");
}
