//! Benchmark: market order round-trip (submit → fill), multiple iterations.
//!
//! Submits BUY→fill then SELL→fill, repeated N times for statistical significance.
//! CAUTION: Places real paper orders. Only run during market hours.
//!
//! Env vars:
//!   BENCH_CON_ID     - contract ID (default: 756733 = SPY)
//!   BENCH_ITERATIONS - number of buy/sell pairs (default: 5)
//!   BENCH_WARMUP     - warmup ticks before starting (default: 50)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use ibx::bridge::Event;
use ibx::types::*;

use harness::*;

fn main() {
    env_logger::init();

    let config = BenchConfig::from_env();
    let iterations = BenchConfig::env_u32("BENCH_ITERATIONS", 5);
    let warmup_ticks = BenchConfig::env_u32("BENCH_WARMUP", 50);

    print_header("Bench: Market Order Round-Trip");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Iterations:     {} buy/sell pairs", iterations);
    println!("  Warmup:         {} ticks", warmup_ticks);
    println!("  WARNING:        Places REAL paper orders!");
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

    let mut buy_stats = LatencyStats::new(iterations as usize);
    let mut sell_stats = LatencyStats::new(iterations as usize);
    let mut rtt_stats = LatencyStats::new(iterations as usize);
    let mut order_id = 1u64;

    for i in 0..iterations {
        // BUY
        let buy_time = Instant::now();
        session.send_order(OrderRequest::SubmitMarket {
            order_id,
            instrument,
            side: Side::Buy,
            qty: 1,
        });

        let buy_ns = wait_for_fill(&session.event_rx, order_id, buy_time, &start, "BUY");
        order_id += 1;

        if buy_ns.is_none() {
            println!("  Iteration {} BUY timed out — market closed?", i + 1);
            break;
        }

        // SELL
        let sell_time = Instant::now();
        session.send_order(OrderRequest::SubmitMarket {
            order_id,
            instrument,
            side: Side::Sell,
            qty: 1,
        });

        let sell_ns = wait_for_fill(&session.event_rx, order_id, sell_time, &start, "SELL");
        order_id += 1;

        if sell_ns.is_none() {
            println!("  Iteration {} SELL timed out", i + 1);
            break;
        }

        let b = buy_ns.unwrap();
        let s = sell_ns.unwrap();
        buy_stats.push(b);
        sell_stats.push(s);
        rtt_stats.push(b + s);

        println!(
            "[{:.3}s] Pair {}/{}: buy={} sell={} total={}",
            start.elapsed().as_secs_f64(),
            i + 1,
            iterations,
            format_ns(b),
            format_ns(s),
            format_ns(b + s),
        );

        // Brief pause between pairs
        std::thread::sleep(Duration::from_millis(200));
    }

    // Report
    print_header("Results: Market Order Round-Trip");
    buy_stats.report("BUY → FILL");
    println!();
    sell_stats.report("SELL → FILL");
    println!();
    rtt_stats.report("BUY+SELL ROUND-TRIP");

    session.shutdown();
}

fn wait_for_fill(
    event_rx: &crossbeam_channel::Receiver<Event>,
    order_id: OrderId,
    submit_time: Instant,
    start: &Instant,
    label: &str,
) -> Option<u64> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return None;
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Fill(fill)) if fill.order_id == order_id => {
                let ns = (Instant::now() - submit_time).as_nanos() as u64;
                println!(
                    "[{:.3}s] {} filled in {} @ ${:.2}",
                    start.elapsed().as_secs_f64(),
                    label,
                    format_ns(ns),
                    fill.price as f64 / PRICE_SCALE as f64,
                );
                return Some(ns);
            }
            Ok(Event::OrderUpdate(u)) if u.order_id == order_id && u.status == OrderStatus::Rejected => {
                println!(
                    "[{:.3}s] {} REJECTED",
                    start.elapsed().as_secs_f64(),
                    label,
                );
                return None;
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => return None,
        }
    }
}
