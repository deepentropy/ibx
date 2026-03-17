//! Benchmark: tick-by-tick (35=E) latency.
//!
//! Subscribes to tick-by-tick Last trades and BidAsk quotes via the historical data connection,
//! measures inter-event latency for each stream.
//!
//! Env vars:
//!   BENCH_CON_ID   - contract ID (default: 756733 = SPY)
//!   BENCH_TICKS    - events to collect per stream (default: 5000)
//!   BENCH_WARMUP   - warmup events (default: 100)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use ibx::bridge::Event;
use ibx::types::*;

use harness::*;

fn main() {
    env_logger::init();

    let config = BenchConfig::from_env();
    let collect_ticks = BenchConfig::env_u32("BENCH_TICKS", 5_000);
    let warmup_count = BenchConfig::env_u32("BENCH_WARMUP", 100);

    print_header("Bench: Tick-by-Tick (35=E) Latency");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Collect:        {} events per stream", collect_ticks);
    println!("  Warmup:         {} events", warmup_count);
    println!();

    // Connect
    println!("Connecting to IB...");
    let session = BenchSession::connect(&config);
    println!(
        "Connected in {:.3}s (account: {})",
        session.connect_time.as_secs_f64(),
        session.account_id,
    );

    let start = Instant::now();

    // --- Phase 1: TBT Last trades ---
    println!(
        "[{:.3}s] Subscribing to TBT Last trades...",
        start.elapsed().as_secs_f64(),
    );
    session.subscribe_tbt(config.con_id, config.symbol, TbtType::Last);

    let trade_stats = collect_tbt_trades(
        &session.event_rx,
        collect_ticks,
        warmup_count,
        &start,
    );

    // Unsubscribe TBT Last — need instrument ID. Use 0 as first instrument.
    session.unsubscribe_tbt(0);
    std::thread::sleep(Duration::from_millis(500));
    while session.event_rx.try_recv().is_ok() {}

    // --- Phase 2: TBT BidAsk quotes ---
    println!(
        "[{:.3}s] Subscribing to TBT BidAsk quotes...",
        start.elapsed().as_secs_f64(),
    );
    session.subscribe_tbt(config.con_id, config.symbol, TbtType::BidAsk);

    let quote_stats = collect_tbt_quotes(
        &session.event_rx,
        collect_ticks,
        warmup_count,
        &start,
    );

    // Report
    print_header("Results: Tick-by-Tick Latency");
    println!("CONNECTION");
    println!(
        "  Total:          {}",
        format_ns(session.connect_time.as_nanos() as u64),
    );
    println!();
    trade_stats.report_throughput("TBT LAST TRADE INTER-EVENT TIME", 0.0);
    println!();
    quote_stats.report_throughput("TBT BIDASK INTER-EVENT TIME", 0.0);

    session.shutdown();
}

fn collect_tbt_trades(
    event_rx: &crossbeam_channel::Receiver<Event>,
    collect: u32,
    warmup_count: u32,
    start: &Instant,
) -> LatencyStats {
    let mut stats = LatencyStats::new(collect as usize);
    let mut last_time: Option<Instant> = None;
    let mut warmed_up = 0u32;
    let mut collected = 0u32;
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if Instant::now() > deadline || collected >= collect {
            break;
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::TbtTrade(_)) => {
                let now = Instant::now();
                if warmed_up < warmup_count {
                    warmed_up += 1;
                    if warmed_up == warmup_count {
                        println!(
                            "[{:.3}s] TBT Last warmup done, collecting {}...",
                            start.elapsed().as_secs_f64(),
                            collect,
                        );
                    }
                    continue;
                }
                if let Some(prev) = last_time {
                    stats.push((now - prev).as_nanos() as u64);
                }
                last_time = Some(now);
                collected += 1;
                if collected % 1000 == 0 {
                    println!(
                        "[{:.3}s] TBT Last: {}/{} samples...",
                        start.elapsed().as_secs_f64(),
                        collected,
                        collect,
                    );
                }
            }
            Ok(Event::Disconnected) => break,
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if warmed_up == 0 {
                    // No TBT trades coming — market might be closed
                    println!(
                        "[{:.3}s] No TBT Last events received (market closed?), skipping",
                        start.elapsed().as_secs_f64(),
                    );
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }
    stats
}

fn collect_tbt_quotes(
    event_rx: &crossbeam_channel::Receiver<Event>,
    collect: u32,
    warmup_count: u32,
    start: &Instant,
) -> LatencyStats {
    let mut stats = LatencyStats::new(collect as usize);
    let mut last_time: Option<Instant> = None;
    let mut warmed_up = 0u32;
    let mut collected = 0u32;
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if Instant::now() > deadline || collected >= collect {
            break;
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::TbtQuote(_)) => {
                let now = Instant::now();
                if warmed_up < warmup_count {
                    warmed_up += 1;
                    if warmed_up == warmup_count {
                        println!(
                            "[{:.3}s] TBT BidAsk warmup done, collecting {}...",
                            start.elapsed().as_secs_f64(),
                            collect,
                        );
                    }
                    continue;
                }
                if let Some(prev) = last_time {
                    stats.push((now - prev).as_nanos() as u64);
                }
                last_time = Some(now);
                collected += 1;
                if collected % 1000 == 0 {
                    println!(
                        "[{:.3}s] TBT BidAsk: {}/{} samples...",
                        start.elapsed().as_secs_f64(),
                        collected,
                        collect,
                    );
                }
            }
            Ok(Event::Disconnected) => break,
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if warmed_up == 0 {
                    println!(
                        "[{:.3}s] No TBT BidAsk events received (market closed?), skipping",
                        start.elapsed().as_secs_f64(),
                    );
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }
    stats
}
