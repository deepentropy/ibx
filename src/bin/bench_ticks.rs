//! Benchmark: single-instrument inter-tick latency and throughput.
//!
//! Replaces the tick portion of the old monolithic benchmark.
//!
//! Env vars:
//!   BENCH_CON_ID   - contract ID (default: 756733 = SPY)
//!   BENCH_TICKS    - ticks to collect (default: 10000)
//!   BENCH_WARMUP   - warmup ticks (default: 200)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use ibx::bridge::Event;

use harness::*;

fn main() {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());

    let config = BenchConfig::from_env();
    let collect_ticks = BenchConfig::env_u32("BENCH_TICKS", 10_000);
    let warmup_ticks = BenchConfig::env_u32("BENCH_WARMUP", 200);

    print_header("Bench: Single-Instrument Tick Latency");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Warmup:         {} ticks", warmup_ticks);
    println!("  Collect:        {} ticks", collect_ticks);
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

    // Warmup
    let _instrument = warmup(&session.event_rx, warmup_ticks, start);

    // Collect
    println!(
        "[{:.3}s] Collecting {} tick samples...",
        start.elapsed().as_secs_f64(),
        collect_ticks,
    );

    let collect_start = Instant::now();
    let mut stats = LatencyStats::new(collect_ticks as usize);
    let mut last_tick = Instant::now();
    let mut count = 0u32;
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if Instant::now() > deadline {
            println!("Collection timed out, using {} samples", count);
            break;
        }

        match session.event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Tick(_)) => {
                let now = Instant::now();
                stats.push((now - last_tick).as_nanos() as u64);
                last_tick = now;
                count += 1;

                if count % 2000 == 0 {
                    println!(
                        "[{:.3}s] {}/{} samples...",
                        start.elapsed().as_secs_f64(),
                        count,
                        collect_ticks,
                    );
                }
                if count >= collect_ticks {
                    break;
                }
            }
            Ok(Event::Disconnected) => {
                println!("Disconnected");
                break;
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    let collect_dur = collect_start.elapsed().as_secs_f64();

    // Report
    print_header("Results: Single-Instrument Tick Latency");
    println!("CONNECTION");
    println!(
        "  Total:          {}",
        format_ns(session.connect_time.as_nanos() as u64),
    );
    println!();
    stats.report_throughput("INTER-TICK TIME", collect_dur);

    session.shutdown();
}
