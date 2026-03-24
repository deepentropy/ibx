//! Benchmark: limit order submit→ack→cancel round-trip.
//!
//! Submits a far-from-market limit order (price $1.00), waits for ack,
//! cancels it, waits for cancel confirm. Repeats N times for statistics.
//! Works outside market hours (uses GTC TIF).
//!
//! Env vars:
//!   BENCH_CON_ID     - contract ID (default: 756733 = SPY)
//!   BENCH_ITERATIONS - number of order cycles (default: 20)
//!   BENCH_WARMUP     - warmup ticks before starting (default: 50)

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
    let mut cancel_stats = LatencyStats::new(iterations as usize);
    let mut total_stats = LatencyStats::new(iterations as usize);
    let mut order_id = 1u64;

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

        // Wait for ack (Submitted or PreSubmitted)
        let mut submit_latency_ns = None;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                println!("  Iteration {} submit timed out", i + 1);
                break;
            }
            match session.event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::OrderUpdate(update)) if update.order_id == order_id => {
                    match update.status {
                        OrderStatus::Submitted | OrderStatus::PendingSubmit => {
                            let ns = (Instant::now() - submit_time).as_nanos() as u64;
                            submit_latency_ns = Some(ns);
                            break;
                        }
                        OrderStatus::Rejected => {
                            println!("  Iteration {} order REJECTED", i + 1);
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

        if submit_latency_ns.is_none() {
            order_id += 1;
            continue;
        }

        // Cancel order
        let cancel_time = Instant::now();
        session.send_order(OrderRequest::Cancel { order_id });

        // Wait for cancel confirm
        let mut cancel_latency_ns = None;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                println!("  Iteration {} cancel timed out", i + 1);
                break;
            }
            match session.event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::OrderUpdate(update)) if update.order_id == order_id => {
                    if update.status == OrderStatus::Cancelled {
                        let ns = (Instant::now() - cancel_time).as_nanos() as u64;
                        cancel_latency_ns = Some(ns);
                        break;
                    }
                }
                Ok(_) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }

        if let (Some(sub_ns), Some(can_ns)) = (submit_latency_ns, cancel_latency_ns) {
            submit_stats.push(sub_ns);
            cancel_stats.push(can_ns);
            total_stats.push(sub_ns + can_ns);
            println!(
                "[{:.3}s] Iteration {}/{}: submit={} cancel={} total={}",
                start.elapsed().as_secs_f64(),
                i + 1,
                iterations,
                format_ns(sub_ns),
                format_ns(can_ns),
                format_ns(sub_ns + can_ns),
            );
        }

        order_id += 1;
        // Brief pause between cycles
        std::thread::sleep(Duration::from_millis(100));
    }

    // Report
    print_header("Results: Limit Order Submit/Cancel RTT");
    submit_stats.report("SUBMIT → ACK");
    println!();
    cancel_stats.report("CANCEL → CONFIRM");
    println!();
    total_stats.report("TOTAL ROUND-TRIP");

    session.shutdown();
}
