//! Benchmark: limit order submit→ack→cancel round-trip.
//!
//! Submits a far-from-market limit order (price $1.00), waits for ack,
//! cancels it, waits for cancel confirm. Repeats N times for statistics.
//! Works outside market hours (uses GTC TIF); with the market closed set
//! BENCH_WARMUP=1, as few ticks come.
//!
//! The ack is the first status of the order. The routed status that may
//! follow is reported as its own figure: the server holds it back after a
//! number of fast cycles, for any client. Every order placed is cancelled,
//! the order ids start above those of the earlier runs, and the run ends
//! with the count of orders left working (exit code 1 when not 0).
//!
//! Env vars:
//!   BENCH_CON_ID         - contract ID (default: 756733 = SPY)
//!   BENCH_ITERATIONS     - number of order cycles (default: 20)
//!   BENCH_WARMUP         - warmup ticks before starting (default: 50)
//!   BENCH_PAUSE_MS       - pause between cycles (default: 100)
//!   BENCH_ROUTED_WAIT_MS - wait for the routed status (default: 1000)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use ibx::bridge::Event;
use ibx::types::*;

use harness::*;

fn main() {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());

    let config = BenchConfig::from_env();
    let iterations = BenchConfig::env_u32("BENCH_ITERATIONS", 20);
    let warmup_ticks = BenchConfig::env_u32("BENCH_WARMUP", 50);
    let pause = Duration::from_millis(BenchConfig::env_u32("BENCH_PAUSE_MS", 100) as u64);
    let routed_wait = Duration::from_millis(BenchConfig::env_u32("BENCH_ROUTED_WAIT_MS", 1000) as u64);

    print_header("Bench: Limit Order Submit/Cancel RTT");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Iterations:     {}", iterations);
    println!("  Warmup:         {} ticks", warmup_ticks);
    println!();

    // Connect
    println!("Connecting to IB...");
    let session = BenchSession::connect(&config);
    println!(
        "Connected in {:.3}s (account: {})",
        session.connect_time.as_secs_f64(),
        session.account_id,
    );

    // Subscribe to get instrument ID
    session.subscribe(config.con_id, config.symbol);
    let start = Instant::now();
    let instrument = warmup(&session.event_rx, warmup_ticks, start);

    let mut submit_stats = LatencyStats::new(iterations as usize);
    let mut routed_stats = LatencyStats::new(iterations as usize);
    let mut cancel_stats = LatencyStats::new(iterations as usize);
    let mut total_stats = LatencyStats::new(iterations as usize);
    let first_order_id = session.first_order_id();
    let mut order_id = first_order_id;
    println!("  First order id: {}", first_order_id);
    // Orders placed whose cancel was not confirmed.
    let mut working: Vec<i64> = Vec::new();
    let mut rejected = 0u32;
    let mut held_back = 0u32;
    // The routed status is waited for until the server stops giving it.
    let mut wait_routed = true;

    for i in 0..iterations {
        // Submit limit order at $1.00 (far from market, won't fill)
        let submit_time = Instant::now();
        session.send_order(OrderRequest::SubmitLimitGtc {
            order_id,
            instrument,
            side: Side::Buy,
            qty: 1,
            price: 1 * PRICE_SCALE,
            outside_rth: true,
        });
        session.keep_order_id(order_id);

        // The ack is the first status of the order. The routed status
        // (Submitted) behind it is the server's own time: it holds it back
        // after a number of fast place and cancel cycles, as it does for
        // any client (ibx#539).
        let mut submit_latency_ns = None;
        let mut is_rejected = false;
        let mut is_routed = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                println!("  Iteration {} submit timed out", i + 1);
                break;
            }
            match session.event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::OrderUpdate(update)) if update.order_id == order_id => {
                    match update.status {
                        OrderStatus::PendingSubmit | OrderStatus::PreSubmitted | OrderStatus::Submitted => {
                            submit_latency_ns = Some(submit_time.elapsed().as_nanos() as u64);
                            is_routed = update.status == OrderStatus::Submitted;
                            break;
                        }
                        OrderStatus::Rejected => {
                            println!("  Iteration {} order REJECTED", i + 1);
                            is_rejected = true;
                            break;
                        }
                        _ => continue,
                    }
                }
                Ok(_) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }

        if is_rejected {
            rejected += 1;
            order_id += 1;
            continue;
        }

        // The routed status, for its own figure.
        let mut routed_latency_ns = is_routed.then(|| submit_time.elapsed().as_nanos() as u64);
        if submit_latency_ns.is_some() && !is_routed && wait_routed {
            let deadline = Instant::now() + routed_wait;
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match session.event_rx.recv_timeout(left) {
                    Ok(Event::OrderUpdate(update)) if update.order_id == order_id && update.status == OrderStatus::Submitted => {
                        routed_latency_ns = Some(submit_time.elapsed().as_nanos() as u64);
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            if routed_latency_ns.is_none() {
                held_back += 1;
                wait_routed = false;
                println!(
                    "  Iteration {}: no routed status in {} ms, held back by the server; not waited for in the next cycles",
                    i + 1, routed_wait.as_millis(),
                );
            }
        }

        // Cancel the order, acknowledged or not: none is left working.
        let cancel_time = Instant::now();
        session.send_order(OrderRequest::Cancel { order_id });
        let cancel_latency_ns = wait_cancelled(&session, order_id, Duration::from_secs(30))
            .then(|| cancel_time.elapsed().as_nanos() as u64);
        if cancel_latency_ns.is_none() {
            println!("  Iteration {} cancel timed out", i + 1);
            working.push(order_id);
        }

        if let (Some(sub_ns), Some(can_ns)) = (submit_latency_ns, cancel_latency_ns) {
            submit_stats.push(sub_ns);
            cancel_stats.push(can_ns);
            total_stats.push(sub_ns + can_ns);
            if let Some(ns) = routed_latency_ns {
                routed_stats.push(ns);
            }
            println!(
                "[{:.3}s] Iteration {}/{}: submit={} routed={} cancel={} total={}",
                start.elapsed().as_secs_f64(),
                i + 1,
                iterations,
                format_ns(sub_ns),
                routed_latency_ns.map_or("-".to_string(), format_ns),
                format_ns(can_ns),
                format_ns(sub_ns + can_ns),
            );
        }

        order_id += 1;
        // Brief pause between cycles
        std::thread::sleep(pause);
    }

    // The orders whose cancel was not confirmed are cancelled once more.
    working.retain(|&id| {
        session.send_order(OrderRequest::Cancel { order_id: id });
        !wait_cancelled(&session, id, Duration::from_secs(10))
    });

    // Report
    print_header("Results: Limit Order Submit/Cancel RTT");
    submit_stats.report("SUBMIT → ACK (first status)");
    println!();
    routed_stats.report("SUBMIT → ROUTED (server's time)");
    println!();
    cancel_stats.report("CANCEL → CONFIRM");
    println!();
    total_stats.report("TOTAL ROUND-TRIP (ack + cancel)");
    println!();
    println!("  Order ids:           {} to {}", first_order_id, order_id - 1);
    println!("  Rejected:            {}", rejected);
    println!("  Routed held back:    {}", held_back);
    println!("  Orders left working: {} {:?}", working.len(), working);

    let left = working.len();
    session.shutdown();
    if left > 0 {
        std::process::exit(1);
    }
}

/// Wait for the cancel of an order to be confirmed. A reject ends it too.
fn wait_cancelled(session: &BenchSession, order_id: i64, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match session.event_rx.recv_timeout(left) {
            Ok(Event::OrderUpdate(update)) if update.order_id == order_id
                && matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected) => return true,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    false
}
