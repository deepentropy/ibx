//! Benchmark: order modify (amend) latency.
//!
//! Submits a far-from-market GTC limit order, then modifies the price N times,
//! measuring modify→ack latency for each. Cancels at the end.
//!
//! Env vars:
//!   BENCH_CON_ID     - contract ID (default: 756733 = SPY)
//!   BENCH_ITERATIONS - number of modify cycles (default: 20)
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

    print_header("Bench: Order Modify (Amend) Latency");
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

    // Subscribe
    session.subscribe(config.con_id, config.symbol);
    let start = Instant::now();
    let instrument = warmup(&session.event_rx, warmup_ticks, start);

    // Submit initial order at $1.00
    let base_order_id = 1u64;
    let mut current_order_id = base_order_id;

    println!(
        "[{:.3}s] Submitting initial GTC limit BUY 1 @ $1.00...",
        start.elapsed().as_secs_f64(),
    );
    session.send_order(OrderRequest::SubmitLimitGtc {
        order_id: current_order_id,
        instrument,
        side: Side::Buy,
        qty: 1,
        price: 1 * PRICE_SCALE,
        outside_rth: true,
    });

    // Wait for initial ack
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            println!("Initial order timed out");
            session.shutdown();
            return;
        }
        match session.event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::OrderUpdate(update)) if update.order_id == current_order_id => {
                if matches!(update.status, OrderStatus::Submitted | OrderStatus::PendingSubmit) {
                    println!(
                        "[{:.3}s] Initial order acked",
                        start.elapsed().as_secs_f64(),
                    );
                    break;
                }
                if update.status == OrderStatus::Rejected {
                    println!("Initial order REJECTED");
                    session.shutdown();
                    return;
                }
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => {
                session.shutdown();
                return;
            }
        }
    }

    // Modify N times, alternating price between $1.01 and $1.02
    let mut modify_stats = LatencyStats::new(iterations as usize);

    for i in 0..iterations {
        let new_price = if i % 2 == 0 {
            102 * (PRICE_SCALE / 100) // $1.02
        } else {
            101 * (PRICE_SCALE / 100) // $1.01
        };

        let new_order_id = current_order_id + 1;
        let modify_time = Instant::now();
        session.send_order(OrderRequest::Modify {
            new_order_id,
            order_id: current_order_id,
            price: new_price,
            qty: 1,
        });

        // Wait for ack on new_order_id
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut acked = false;
        loop {
            if Instant::now() > deadline {
                println!("  Iteration {} modify timed out", i + 1);
                break;
            }
            match session.event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Event::OrderUpdate(update)) if update.order_id == new_order_id => {
                    if matches!(update.status, OrderStatus::Submitted | OrderStatus::PendingSubmit) {
                        let ns = (Instant::now() - modify_time).as_nanos() as u64;
                        modify_stats.push(ns);
                        println!(
                            "[{:.3}s] Modify {}/{}: {}",
                            start.elapsed().as_secs_f64(),
                            i + 1,
                            iterations,
                            format_ns(ns),
                        );
                        acked = true;
                        break;
                    }
                }
                Ok(Event::CancelReject(reject)) if reject.order_id == current_order_id => {
                    println!("  Modify rejected (reason={})", reject.reason_code);
                    break;
                }
                Ok(_) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }

        if acked {
            current_order_id = new_order_id;
        }

        // Brief pause
        std::thread::sleep(Duration::from_millis(100));
    }

    // Cancel the order
    println!(
        "[{:.3}s] Cancelling order...",
        start.elapsed().as_secs_f64(),
    );
    session.send_order(OrderRequest::Cancel {
        order_id: current_order_id,
    });
    std::thread::sleep(Duration::from_millis(500));

    // Report
    print_header("Results: Order Modify Latency");
    modify_stats.report("MODIFY → ACK");

    session.shutdown();
}
