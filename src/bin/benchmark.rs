//! IB Gateway benchmark binary.
//!
//! Measures real-world latency characteristics against a live IB gateway:
//! 1. Connection time (CCP auth + farm logon)
//! 2. Tick decode latency (socket recv → on_tick callback)
//! 3. Hot loop iteration throughput
//! 4. Order round-trip (submit → fill)
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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ib_engine::engine::context::{Context, Strategy};
use ib_engine::gateway::{Gateway, GatewayConfig};
use ib_engine::types::*;

// ─── Benchmark phases ───

const PHASE_WARMUP: u8 = 0;
const PHASE_COLLECT: u8 = 1;
const PHASE_ORDER_BUY: u8 = 2;
const PHASE_WAIT_BUY: u8 = 3;
const PHASE_WAIT_SELL: u8 = 5;
const PHASE_DONE: u8 = 6;

// ─── Benchmark strategy ───

struct BenchmarkStrategy {
    // Tick decode latency: on_tick time - recv_at time
    decode_latencies_ns: Vec<u64>,
    // Inter-tick time: delta between consecutive on_tick calls
    inter_tick_ns: Vec<u64>,
    // Loop iterations between consecutive ticks
    loop_iters_per_tick: Vec<u64>,

    last_tick_time: Option<Instant>,
    last_loop_iters: u64,

    // Order round-trip
    target_instrument: InstrumentId,
    buy_submit_time: Option<Instant>,
    sell_submit_time: Option<Instant>,
    buy_rtt_ns: Option<u64>,
    sell_rtt_ns: Option<u64>,

    // Phase control
    tick_count: u32,
    warmup_ticks: u32,
    collect_ticks: u32,
    run_orders: bool,
    phase: Arc<AtomicU8>,
    done: Arc<AtomicBool>,
    start: Instant,
    collect_start: Option<Instant>,
}

impl BenchmarkStrategy {
    fn new(
        warmup_ticks: u32,
        collect_ticks: u32,
        run_orders: bool,
    ) -> (Self, BenchHandle) {
        let phase = Arc::new(AtomicU8::new(PHASE_WARMUP));
        let done = Arc::new(AtomicBool::new(false));

        let handle = BenchHandle {
            phase: phase.clone(),
            done: done.clone(),
        };

        let strategy = Self {
            decode_latencies_ns: Vec::with_capacity(collect_ticks as usize),
            inter_tick_ns: Vec::with_capacity(collect_ticks as usize),
            loop_iters_per_tick: Vec::with_capacity(collect_ticks as usize),
            last_tick_time: None,
            last_loop_iters: 0,
            target_instrument: 0,
            buy_submit_time: None,
            sell_submit_time: None,
            buy_rtt_ns: None,
            sell_rtt_ns: None,
            tick_count: 0,
            warmup_ticks,
            collect_ticks,
            run_orders,
            phase,
            done,
            start: Instant::now(),
            collect_start: None,
        };

        (strategy, handle)
    }

    fn print_report(&self, connect_time: Duration) {
        println!();
        println!("========================================");
        println!("  IB Gateway Benchmark Results");
        println!("========================================");
        println!();

        // Connection time
        println!("CONNECTION");
        println!("  Total:          {}", format_ns(connect_time.as_nanos() as u64));
        println!();

        // Tick decode latency
        if !self.decode_latencies_ns.is_empty() {
            let mut samples = self.decode_latencies_ns.clone();
            samples.sort();
            let n = samples.len();
            let mean = samples.iter().sum::<u64>() / n as u64;
            let collect_dur = self.collect_start
                .map(|s| self.start.elapsed().as_secs_f64() - s.elapsed().as_secs_f64())
                .unwrap_or(0.0);

            println!("TICK DECODE LATENCY ({} samples, {:.1}s collection)", n, collect_dur);
            println!("  Min:            {}", format_ns(samples[0]));
            println!("  P50:            {}", format_ns(percentile(&samples, 0.50)));
            println!("  P95:            {}", format_ns(percentile(&samples, 0.95)));
            println!("  P99:            {}", format_ns(percentile(&samples, 0.99)));
            println!("  P99.9:          {}", format_ns(percentile(&samples, 0.999)));
            println!("  Max:            {}", format_ns(samples[n - 1]));
            println!("  Mean:           {}", format_ns(mean));
            println!();
        }

        // Inter-tick time (approximation of loop iteration time when processing)
        if !self.inter_tick_ns.is_empty() {
            let mut samples = self.inter_tick_ns.clone();
            samples.sort();
            let n = samples.len();
            let mean = samples.iter().sum::<u64>() / n as u64;
            let throughput = 1_000_000_000.0 / mean as f64;

            println!("INTER-TICK TIME ({} samples)", n);
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

        // Loop iterations per tick
        if !self.loop_iters_per_tick.is_empty() {
            let mut samples = self.loop_iters_per_tick.clone();
            samples.sort();
            let n = samples.len();
            let mean = samples.iter().sum::<u64>() as f64 / n as f64;

            println!("LOOP ITERATIONS PER TICK ({} samples)", n);
            println!("  Min:            {}", samples[0]);
            println!("  P50:            {}", percentile(&samples, 0.50));
            println!("  P99:            {}", percentile(&samples, 0.99));
            println!("  Max:            {}", samples[n - 1]);
            println!("  Mean:           {:.1}", mean);
            println!();
        }

        // Order round-trip
        if self.buy_rtt_ns.is_some() || self.sell_rtt_ns.is_some() {
            println!("ORDER ROUND-TRIP");
            if let Some(ns) = self.buy_rtt_ns {
                println!("  Buy:            {}", format_ns(ns));
            }
            if let Some(ns) = self.sell_rtt_ns {
                println!("  Sell:           {}", format_ns(ns));
            }
            if let (Some(b), Some(s)) = (self.buy_rtt_ns, self.sell_rtt_ns) {
                println!("  Mean:           {}", format_ns((b + s) / 2));
            }
            println!();
        } else if self.run_orders {
            println!("ORDER ROUND-TRIP");
            println!("  (no fills received - market may be closed)");
            println!();
        }

        println!("========================================");
    }
}

impl Strategy for BenchmarkStrategy {
    fn on_start(&mut self, _ctx: &mut Context) {
        println!("[{:.3}s] Strategy started", self.start.elapsed().as_secs_f64());
    }

    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        let phase = self.phase.load(Ordering::Relaxed);

        match phase {
            PHASE_WARMUP => {
                if self.tick_count == 0 {
                    self.target_instrument = instrument;
                    println!(
                        "[{:.3}s] First tick received (instrument={}), warming up ({} ticks)...",
                        self.start.elapsed().as_secs_f64(),
                        instrument,
                        self.warmup_ticks,
                    );
                }
                self.tick_count += 1;
                if self.tick_count >= self.warmup_ticks {
                    self.tick_count = 0;
                    self.phase.store(PHASE_COLLECT, Ordering::Relaxed);
                    self.collect_start = Some(Instant::now());
                    println!(
                        "[{:.3}s] Collecting {} tick samples...",
                        self.start.elapsed().as_secs_f64(),
                        self.collect_ticks,
                    );
                }
            }

            PHASE_COLLECT => {
                let now = Instant::now();

                // Decode latency: time from socket recv to this callback
                let decode_ns = (now - ctx.recv_timestamp()).as_nanos() as u64;
                self.decode_latencies_ns.push(decode_ns);

                // Inter-tick time
                if let Some(prev) = self.last_tick_time {
                    self.inter_tick_ns.push((now - prev).as_nanos() as u64);
                }

                // Loop iterations between ticks
                let iters = ctx.loop_iterations();
                if self.last_loop_iters > 0 {
                    self.loop_iters_per_tick.push(iters - self.last_loop_iters);
                }
                self.last_loop_iters = iters;
                self.last_tick_time = Some(now);

                self.tick_count += 1;
                if self.tick_count % 2000 == 0 {
                    println!(
                        "[{:.3}s] Collected {}/{} samples...",
                        self.start.elapsed().as_secs_f64(),
                        self.tick_count,
                        self.collect_ticks,
                    );
                }

                // Timeout after 120s of collection regardless
                let timeout = self.collect_start
                    .map(|s| s.elapsed() > Duration::from_secs(120))
                    .unwrap_or(false);

                if self.tick_count >= self.collect_ticks || timeout {
                    if timeout {
                        println!(
                            "[{:.3}s] Collection timeout, using {} samples",
                            self.start.elapsed().as_secs_f64(),
                            self.tick_count,
                        );
                    }

                    if self.run_orders {
                        self.phase.store(PHASE_ORDER_BUY, Ordering::Relaxed);
                    } else {
                        self.phase.store(PHASE_DONE, Ordering::Relaxed);
                        self.done.store(true, Ordering::Relaxed);
                    }
                }
            }

            PHASE_ORDER_BUY => {
                println!(
                    "[{:.3}s] Submitting market BUY 1 share...",
                    self.start.elapsed().as_secs_f64(),
                );
                self.buy_submit_time = Some(Instant::now());
                ctx.submit_market(self.target_instrument, Side::Buy, 1);
                self.phase.store(PHASE_WAIT_BUY, Ordering::Relaxed);
            }

            PHASE_WAIT_BUY => {
                // Timeout: if waiting > 30s, skip order test
                if self.buy_submit_time
                    .map(|t| t.elapsed() > Duration::from_secs(30))
                    .unwrap_or(false)
                {
                    println!(
                        "[{:.3}s] Buy order timeout (30s) - market may be closed",
                        self.start.elapsed().as_secs_f64(),
                    );
                    self.phase.store(PHASE_DONE, Ordering::Relaxed);
                    self.done.store(true, Ordering::Relaxed);
                }
            }

            PHASE_WAIT_SELL => {
                if self.sell_submit_time
                    .map(|t| t.elapsed() > Duration::from_secs(30))
                    .unwrap_or(false)
                {
                    println!(
                        "[{:.3}s] Sell order timeout (30s)",
                        self.start.elapsed().as_secs_f64(),
                    );
                    self.phase.store(PHASE_DONE, Ordering::Relaxed);
                    self.done.store(true, Ordering::Relaxed);
                }
            }

            _ => {}
        }
    }

    fn on_fill(&mut self, _fill: &Fill, ctx: &mut Context) {
        let phase = self.phase.load(Ordering::Relaxed);

        match phase {
            PHASE_WAIT_BUY => {
                let now = Instant::now();
                let rtt = (now - self.buy_submit_time.unwrap()).as_nanos() as u64;
                self.buy_rtt_ns = Some(rtt);
                println!(
                    "[{:.3}s] BUY filled in {}",
                    self.start.elapsed().as_secs_f64(),
                    format_ns(rtt),
                );

                // Immediately submit sell to close position
                println!(
                    "[{:.3}s] Submitting market SELL 1 share...",
                    self.start.elapsed().as_secs_f64(),
                );
                self.sell_submit_time = Some(Instant::now());
                ctx.submit_market(self.target_instrument, Side::Sell, 1);
                self.phase.store(PHASE_WAIT_SELL, Ordering::Relaxed);
            }

            PHASE_WAIT_SELL => {
                let now = Instant::now();
                let rtt = (now - self.sell_submit_time.unwrap()).as_nanos() as u64;
                self.sell_rtt_ns = Some(rtt);
                println!(
                    "[{:.3}s] SELL filled in {}",
                    self.start.elapsed().as_secs_f64(),
                    format_ns(rtt),
                );

                self.phase.store(PHASE_DONE, Ordering::Relaxed);
                self.done.store(true, Ordering::Relaxed);
            }

            _ => {}
        }
    }

    fn on_disconnect(&mut self, _ctx: &mut Context) {
        println!(
            "[{:.3}s] Disconnected",
            self.start.elapsed().as_secs_f64(),
        );
    }
}

/// Thread-safe handle for the main thread to monitor benchmark progress.
struct BenchHandle {
    phase: Arc<AtomicU8>,
    done: Arc<AtomicBool>,
}

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

    // Try loading credentials from ibgw-headless .env
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

    // 2. Create hot loop with benchmark strategy
    let (strategy, handle) = BenchmarkStrategy::new(warmup_ticks, collect_ticks, run_orders);
    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, None);

    // Subscribe to instrument
    control_tx.send(ControlCommand::Subscribe { con_id, symbol: symbol.to_string() }).unwrap();

    // Run hot loop in dedicated thread
    let join = std::thread::spawn(move || {
        hot_loop.run();
        // Return the strategy to read results
        hot_loop
    });

    // Wait for benchmark to complete (max 5 minutes)
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline && !handle.done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    if !handle.done.load(Ordering::Relaxed) {
        let phase = handle.phase.load(Ordering::Relaxed);
        println!("\nBenchmark timed out in phase {} - are markets open?", phase);
    }

    // Shutdown
    let _ = control_tx.send(ControlCommand::Shutdown);
    let hot_loop = join.join().expect("hot loop thread panicked");

    // Print results
    hot_loop.strategy().print_report(connect_time);
}
