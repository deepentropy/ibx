//! Benchmark: multi-instrument tick throughput scaling.
//!
//! Subscribes to N instruments and measures aggregate tick throughput,
//! per-instrument tick counts, and inter-tick latency under load.
//!
//! Env vars:
//!   BENCH_TICKS    - total ticks to collect across all instruments (default: 10000)
//!   BENCH_WARMUP   - warmup ticks (default: 200)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ibx::bridge::Event;
use ibx::types::InstrumentId;

use harness::*;

/// Instruments to subscribe: (con_id, symbol)
const INSTRUMENTS: &[(i64, &str)] = &[
    (756733, "SPY"),
    (265598, "AAPL"),
    (272093, "MSFT"),
    (15016062, "QQQ"),
    (9579970, "IWM"),
];

fn main() {
    env_logger::init();

    let config = BenchConfig::from_env();
    let collect_ticks = BenchConfig::env_u32("BENCH_TICKS", 10_000);
    let warmup_ticks = BenchConfig::env_u32("BENCH_WARMUP", 200);
    let n_instruments = BenchConfig::env_u32("BENCH_INSTRUMENTS", 5).min(INSTRUMENTS.len() as u32);

    let instruments = &INSTRUMENTS[..n_instruments as usize];

    print_header("Bench: Multi-Instrument Tick Throughput");
    println!("  Instruments:    {} ({:?})", n_instruments,
        instruments.iter().map(|(_, s)| *s).collect::<Vec<_>>());
    println!("  Warmup:         {} ticks", warmup_ticks);
    println!("  Collect:        {} ticks (aggregate)", collect_ticks);
    println!();

    // Connect
    println!("Connecting to IB...");
    let session = BenchSession::connect(&config);
    println!(
        "Connected in {:.3}s (account: {})",
        session.connect_time.as_secs_f64(),
        session.account_id,
    );

    // Subscribe to all instruments
    for &(con_id, symbol) in instruments {
        session.subscribe(con_id, symbol);
        println!("  Subscribed: {} ({})", symbol, con_id);
    }

    let start = Instant::now();

    // Warmup — wait for warmup_ticks from any instrument
    let _instrument = warmup(&session.event_rx, warmup_ticks, start);

    // Collect
    println!(
        "[{:.3}s] Collecting {} ticks across {} instruments...",
        start.elapsed().as_secs_f64(),
        collect_ticks,
        n_instruments,
    );

    let collect_start = Instant::now();
    let mut stats = LatencyStats::new(collect_ticks as usize);
    let mut per_instrument: HashMap<InstrumentId, u32> = HashMap::new();
    let mut last_tick = Instant::now();
    let mut count = 0u32;
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if Instant::now() > deadline {
            println!("Collection timed out, using {} samples", count);
            break;
        }

        match session.event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Tick(instrument)) => {
                let now = Instant::now();
                stats.push((now - last_tick).as_nanos() as u64);
                last_tick = now;
                *per_instrument.entry(instrument).or_insert(0) += 1;
                count += 1;

                if count % 2000 == 0 {
                    println!(
                        "[{:.3}s] {}/{} ticks...",
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
    print_header("Results: Multi-Instrument Tick Throughput");
    println!("CONNECTION");
    println!(
        "  Total:          {}",
        format_ns(session.connect_time.as_nanos() as u64),
    );
    println!();

    stats.report_throughput("AGGREGATE INTER-TICK TIME", collect_dur);
    println!();

    println!("PER-INSTRUMENT TICK COUNTS ({:.1}s)", collect_dur);
    let mut entries: Vec<_> = per_instrument.iter().collect();
    entries.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (instrument, count) in &entries {
        let rate = **count as f64 / collect_dur;
        println!("  instrument={:<3}  ticks={:<6}  rate={:.1}/sec", instrument, count, rate);
    }

    session.shutdown();
}
