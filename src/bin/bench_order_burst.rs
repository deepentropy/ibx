//! Benchmark: order burst throughput.
//!
//! Submits N limit orders as fast as possible, measures individual ack latencies
//! and aggregate throughput. Then cancels all. Works outside market hours.
//!
//! Env vars:
//!   BENCH_CON_ID     - contract ID (default: 756733 = SPY)
//!   BENCH_BURST_SIZE - number of orders in burst (default: 20)
//!   BENCH_WARMUP     - warmup ticks before starting (default: 50)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ibx::bridge::Event;
use ibx::types::*;

use harness::*;

fn main() {
    env_logger::init();

    let config = BenchConfig::from_env();
    let burst_size = BenchConfig::env_u32("BENCH_BURST_SIZE", 20);
    let warmup_ticks = BenchConfig::env_u32("BENCH_WARMUP", 50);

    print_header("Bench: Order Burst Throughput");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Burst size:     {} orders", burst_size);
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

    // Subscribe
    session.subscribe(config.con_id, config.symbol);
    let start = Instant::now();
    let instrument = warmup(&session.event_rx, warmup_ticks, start);

    // Submit burst of limit orders at different prices ($1.00, $1.01, $1.02, ...)
    let mut submit_times: HashMap<OrderId, Instant> = HashMap::new();
    let burst_start = Instant::now();

    println!(
        "[{:.3}s] Submitting {} limit orders...",
        start.elapsed().as_secs_f64(),
        burst_size,
    );

    for i in 0..burst_size {
        let order_id = (i + 1) as u64;
        let price = (100 + i as i64) * (PRICE_SCALE / 100); // $1.00, $1.01, ...
        submit_times.insert(order_id, Instant::now());
        session.send_order(OrderRequest::SubmitLimitGtc {
            order_id,
            instrument,
            side: Side::Buy,
            qty: 1,
            price,
            outside_rth: true,
        });
    }

    let submit_dur = burst_start.elapsed();
    println!(
        "[{:.3}s] All {} orders submitted in {}",
        start.elapsed().as_secs_f64(),
        burst_size,
        format_ns(submit_dur.as_nanos() as u64),
    );

    // Wait for all acks
    let mut ack_stats = LatencyStats::new(burst_size as usize);
    let mut acked = 0u32;
    let deadline = Instant::now() + Duration::from_secs(60);

    loop {
        if Instant::now() > deadline || acked >= burst_size {
            break;
        }
        match session.event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::OrderUpdate(update)) => {
                if let Some(sub_time) = submit_times.get(&update.order_id) {
                    if matches!(update.status, OrderStatus::Submitted | OrderStatus::PendingSubmit) {
                        let ns = (Instant::now() - *sub_time).as_nanos() as u64;
                        ack_stats.push(ns);
                        acked += 1;
                    }
                }
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    let all_acked_dur = burst_start.elapsed();
    println!(
        "[{:.3}s] {}/{} orders acked in {}",
        start.elapsed().as_secs_f64(),
        acked,
        burst_size,
        format_ns(all_acked_dur.as_nanos() as u64),
    );

    // Cancel all
    println!(
        "[{:.3}s] Cancelling all orders...",
        start.elapsed().as_secs_f64(),
    );
    for i in 0..burst_size {
        let order_id = (i + 1) as u64;
        session.send_order(OrderRequest::Cancel { order_id });
    }

    // Wait for cancels
    let mut cancelled = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline || cancelled >= acked {
            break;
        }
        match session.event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::OrderUpdate(update)) if update.status == OrderStatus::Cancelled => {
                cancelled += 1;
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    // Report
    print_header("Results: Order Burst Throughput");
    println!("SUBMIT PHASE");
    println!(
        "  {} orders submitted in {}",
        burst_size,
        format_ns(submit_dur.as_nanos() as u64),
    );
    let submit_rate = burst_size as f64 / submit_dur.as_secs_f64();
    println!("  Submit rate:    {:.0} orders/sec", submit_rate);
    println!();

    ack_stats.report("ACK LATENCY (submit → ack, per order)");
    println!();

    println!("TOTAL");
    println!(
        "  All acked in:   {}",
        format_ns(all_acked_dur.as_nanos() as u64),
    );
    let ack_rate = acked as f64 / all_acked_dur.as_secs_f64();
    println!("  Ack throughput: {:.1} orders/sec", ack_rate);
    println!("  Cancelled:      {}/{}", cancelled, acked);

    session.shutdown();
}
