//! Benchmark: subscribe-to-first-tick latency.
//!
//! Measures the time from sending a Subscribe command to receiving the first
//! Tick event, repeated N times with unsubscribe/resubscribe cycles.
//!
//! Env vars:
//!   BENCH_CON_ID     - contract ID (default: 756733 = SPY)
//!   BENCH_ITERATIONS - number of sub/unsub cycles (default: 10)

#[path = "../../bench/bench_harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use ibx::bridge::Event;

use harness::*;

fn main() {
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());

    let config = BenchConfig::from_env();
    let iterations = BenchConfig::env_u32("BENCH_ITERATIONS", 10);

    print_header("Bench: Subscribe-to-First-Tick Latency");
    println!("  Contract:       {} (con_id={})", config.symbol, config.con_id);
    println!("  Iterations:     {}", iterations);
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
    let mut stats = LatencyStats::new(iterations as usize);
    let mut last_instrument = 0;

    for i in 0..iterations {
        // Subscribe and time until first tick
        let sub_time = Instant::now();
        session.subscribe(config.con_id, config.symbol);

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if Instant::now() > deadline {
                println!("  Iteration {} timed out", i + 1);
                break;
            }
            match session.event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::Tick(id)) => {
                    let latency_ns = (Instant::now() - sub_time).as_nanos() as u64;
                    stats.push(latency_ns);
                    last_instrument = id;
                    println!(
                        "[{:.3}s] Iteration {}/{}: first tick in {}",
                        start.elapsed().as_secs_f64(),
                        i + 1,
                        iterations,
                        format_ns(latency_ns),
                    );
                    break;
                }
                Ok(_) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => {
                    println!("Channel disconnected");
                    break;
                }
            }
        }

        // Unsubscribe before next cycle
        session.unsubscribe(last_instrument);

        // Brief pause to let unsubscribe propagate
        std::thread::sleep(Duration::from_millis(500));

        // Drain any remaining ticks from previous subscription
        while session.event_rx.try_recv().is_ok() {}
    }

    // Report
    print_header("Results: Subscribe-to-First-Tick Latency");
    println!("CONNECTION");
    println!(
        "  Total:          {}",
        format_ns(session.connect_time.as_nanos() as u64),
    );
    println!();
    stats.report("FIRST-TICK LATENCY");

    session.shutdown();
}
