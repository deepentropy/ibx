//! IB Gateway benchmark binary.
//!
//! Measures real-world latency characteristics against a live IB gateway:
//! 1. Connection time (auth + farm logon)
//! 2. Tick event delivery latency (inter-tick time from event channel)
//! 3. Order round-trip (submit → fill)
//!
//! Usage:
//!   IB_USERNAME=xxx IB_PASSWORD=xxx cargo run --release --bin benchmark
//!
//! Optional env vars:
//!   BENCH_CON_ID   - contract ID to subscribe (default: 756733 = SPY)
//!   BENCH_TICKS    - number of ticks to collect (default: 10000)
//!   BENCH_WARMUP   - warmup ticks to skip (default: 200)
//!   BENCH_ORDERS   - set to "1" to run order round-trip test (places real paper orders)

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;

use ibx::bridge::{Event, SharedState};
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::types::*;

// ─── Helpers ───

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn format_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{}ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.1}us", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.3}s", ns as f64 / 1_000_000_000.0)
    }
}

fn print_report(
    connect_time: Duration,
    inter_tick_ns: &[u64],
    collect_dur_secs: f64,
    buy_rtt_ns: Option<u64>,
    sell_rtt_ns: Option<u64>,
    run_orders: bool,
) {
    println!();
    println!("========================================");
    println!("  IB Gateway Benchmark Results");
    println!("========================================");
    println!();

    // Connection time
    println!("CONNECTION");
    println!("  Total:          {}", format_ns(connect_time.as_nanos() as u64));
    println!();

    // Inter-tick time
    if !inter_tick_ns.is_empty() {
        let mut samples = inter_tick_ns.to_vec();
        samples.sort();
        let n = samples.len();
        let mean = samples.iter().sum::<u64>() / n as u64;
        let throughput = 1_000_000_000.0 / mean as f64;

        println!("INTER-TICK TIME ({} samples, {:.1}s collection)", n, collect_dur_secs);
        println!("  Min:            {}", format_ns(samples[0]));
        println!("  P50:            {}", format_ns(percentile(&samples, 0.50)));
        println!("  P95:            {}", format_ns(percentile(&samples, 0.95)));
        println!("  P99:            {}", format_ns(percentile(&samples, 0.99)));
        println!("  P99.9:          {}", format_ns(percentile(&samples, 0.999)));
        println!("  Max:            {}", format_ns(samples[n - 1]));
        println!("  Mean:           {}", format_ns(mean));
        println!("  Throughput:     ~{:.0} ticks/sec", throughput);
        println!();
    }

    // Order round-trip
    if buy_rtt_ns.is_some() || sell_rtt_ns.is_some() {
        println!("ORDER ROUND-TRIP");
        if let Some(ns) = buy_rtt_ns {
            println!("  Buy:            {}", format_ns(ns));
        }
        if let Some(ns) = sell_rtt_ns {
            println!("  Sell:           {}", format_ns(ns));
        }
        if let (Some(b), Some(s)) = (buy_rtt_ns, sell_rtt_ns) {
            println!("  Mean:           {}", format_ns((b + s) / 2));
        }
        println!();
    } else if run_orders {
        println!("ORDER ROUND-TRIP");
        println!("  (no fills received - market may be closed)");
        println!();
    }

    println!("========================================");
}

// ─── Main ───

/// Load .env file (key=value lines) into environment if vars not already set.
fn load_dotenv(path: &str) {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if env::var(key.trim()).is_err() {
                    // Safety: called at startup before any threads are spawned.
                    unsafe { env::set_var(key.trim(), value.trim()) };
                }
            }
        }
    }
}

fn main() {
    env_logger::init();

    // Try loading credentials from .env
    load_dotenv(r"D:\PycharmProjects\ibgw-headless\.env");

    let username = env::var("IB_USERNAME").expect("IB_USERNAME not set");
    let password = env::var("IB_PASSWORD").expect("IB_PASSWORD not set");
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    let con_id: i64 = env::var("BENCH_CON_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(756733); // SPY
    let collect_ticks: u32 = env::var("BENCH_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let warmup_ticks: u32 = env::var("BENCH_WARMUP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let run_orders = env::var("BENCH_ORDERS").map(|v| v == "1").unwrap_or(false);

    let config = GatewayConfig {
        username,
        password,
        host,
        paper: true,
    };

    println!("========================================");
    println!("  IB Gateway Benchmark");
    println!("========================================");
    let symbol = match con_id { 756733 => "SPY", 265598 => "AAPL", 272093 => "MSFT", _ => "?" };
    println!("  Contract:       {} (con_id={})", symbol, con_id);
    println!("  Warmup ticks:   {}", warmup_ticks);
    println!("  Collect ticks:  {}", collect_ticks);
    println!("  Order test:     {}", if run_orders { "YES (paper)" } else { "no (set BENCH_ORDERS=1)" });
    println!("========================================");
    println!();

    // 1. Connection time
    println!("Connecting to IB...");
    let connect_start = Instant::now();
    let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&config)
        .expect("Gateway::connect() failed");
    let connect_time = connect_start.elapsed();
    println!("Connected in {:.3}s (account: {})", connect_time.as_secs_f64(), gw.account_id);

    // 2. Create hot loop with event channel
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = bounded::<Event>(65536);
    let (mut hot_loop, control_tx) = gw.into_hot_loop(
        shared, Some(event_tx), farm_conn, ccp_conn, hmds_conn, None,
    );

    // Subscribe to instrument
    control_tx.send(ControlCommand::Subscribe { con_id, symbol: symbol.to_string() }).unwrap();

    // Run hot loop in dedicated thread
    let control_tx2 = control_tx.clone();
    let join = std::thread::spawn(move || {
        hot_loop.run();
    });

    // 3. Consume events on main thread
    let start = Instant::now();
    let mut tick_count: u32 = 0;
    let mut target_instrument: InstrumentId = 0;
    let mut last_tick_time: Option<Instant> = None;
    let mut inter_tick_ns: Vec<u64> = Vec::with_capacity(collect_ticks as usize);
    let mut collect_start: Option<Instant> = None;

    // Order tracking
    let mut buy_submit_time: Option<Instant> = None;
    let mut sell_submit_time: Option<Instant> = None;
    let mut buy_rtt_ns: Option<u64> = None;
    let mut sell_rtt_ns: Option<u64> = None;
    let mut order_phase = 0u8; // 0=not started, 1=buy sent, 2=sell sent, 3=done

    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if Instant::now() > deadline {
            println!("\nBenchmark timed out - are markets open?");
            break;
        }

        let event = match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => break, // channel disconnected
        };

        match event {
            Event::Tick(instrument) => {
                let now = Instant::now();

                // Warmup phase
                if tick_count < warmup_ticks {
                    if tick_count == 0 {
                        target_instrument = instrument;
                        println!(
                            "[{:.3}s] First tick received (instrument={}), warming up ({} ticks)...",
                            start.elapsed().as_secs_f64(), instrument, warmup_ticks,
                        );
                    }
                    tick_count += 1;
                    if tick_count >= warmup_ticks {
                        tick_count = 0;
                        collect_start = Some(now);
                        println!(
                            "[{:.3}s] Collecting {} tick samples...",
                            start.elapsed().as_secs_f64(), collect_ticks,
                        );
                    }
                    continue;
                }

                // Collection phase
                if let Some(prev) = last_tick_time {
                    inter_tick_ns.push((now - prev).as_nanos() as u64);
                }
                last_tick_time = Some(now);
                tick_count += 1;

                if tick_count % 2000 == 0 {
                    println!(
                        "[{:.3}s] Collected {}/{} samples...",
                        start.elapsed().as_secs_f64(), tick_count, collect_ticks,
                    );
                }

                let timeout = collect_start
                    .map(|s| s.elapsed() > Duration::from_secs(120))
                    .unwrap_or(false);

                if tick_count >= collect_ticks || timeout {
                    if timeout {
                        println!(
                            "[{:.3}s] Collection timeout, using {} samples",
                            start.elapsed().as_secs_f64(), tick_count,
                        );
                    }

                    if run_orders && order_phase == 0 {
                        // Start order test
                        println!(
                            "[{:.3}s] Submitting market BUY 1 share...",
                            start.elapsed().as_secs_f64(),
                        );
                        buy_submit_time = Some(Instant::now());
                        let _ = control_tx2.send(ControlCommand::Order(
                            OrderRequest::SubmitMarket {
                                order_id: 1,
                                instrument: target_instrument,
                                side: Side::Buy,
                                qty: 1,
                            },
                        ));
                        order_phase = 1;
                    } else if !run_orders {
                        break;
                    }
                }
            }

            Event::Fill(fill) => {
                let now = Instant::now();
                match order_phase {
                    1 => {
                        // Buy filled
                        let rtt = (now - buy_submit_time.unwrap()).as_nanos() as u64;
                        buy_rtt_ns = Some(rtt);
                        println!(
                            "[{:.3}s] BUY filled in {}",
                            start.elapsed().as_secs_f64(), format_ns(rtt),
                        );

                        // Submit sell to close
                        println!(
                            "[{:.3}s] Submitting market SELL 1 share...",
                            start.elapsed().as_secs_f64(),
                        );
                        sell_submit_time = Some(Instant::now());
                        let _ = control_tx2.send(ControlCommand::Order(
                            OrderRequest::SubmitMarket {
                                order_id: 2,
                                instrument: fill.instrument,
                                side: Side::Sell,
                                qty: 1,
                            },
                        ));
                        order_phase = 2;
                    }
                    2 => {
                        // Sell filled
                        let rtt = (now - sell_submit_time.unwrap()).as_nanos() as u64;
                        sell_rtt_ns = Some(rtt);
                        println!(
                            "[{:.3}s] SELL filled in {}",
                            start.elapsed().as_secs_f64(), format_ns(rtt),
                        );
                        break;
                    }
                    _ => {}
                }
            }

            Event::Disconnected => {
                println!("[{:.3}s] Disconnected", start.elapsed().as_secs_f64());
                break;
            }

            _ => {} // ignore other events
        }

        // Order timeout check
        if order_phase == 1 {
            if buy_submit_time.map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(false) {
                println!(
                    "[{:.3}s] Buy order timeout (30s) - market may be closed",
                    start.elapsed().as_secs_f64(),
                );
                break;
            }
        } else if order_phase == 2 {
            if sell_submit_time.map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(false) {
                println!("[{:.3}s] Sell order timeout (30s)", start.elapsed().as_secs_f64());
                break;
            }
        }
    }

    // Shutdown
    let _ = control_tx2.send(ControlCommand::Shutdown);
    let _ = join.join();

    // Print results
    let collect_dur = collect_start.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    print_report(connect_time, &inter_tick_ns, collect_dur, buy_rtt_ns, sell_rtt_ns, run_orders);
}
