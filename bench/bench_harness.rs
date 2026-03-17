//! Shared benchmark harness: connection setup, stats helpers, config.
//! Included via `#[path = "bench_harness.rs"] mod harness;` in each bench binary.

#![allow(dead_code)]

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use ibx::bridge::{Event, SharedState};
use ibx::gateway::{Gateway, GatewayConfig};
use ibx::types::*;

// ─── Config ───

pub struct BenchConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
    pub con_id: i64,
    pub symbol: &'static str,
}

impl BenchConfig {
    pub fn from_env() -> Self {
        load_dotenv(r"D:\PycharmProjects\ibgw-headless\.env");

        let con_id: i64 = env::var("BENCH_CON_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(756733);

        let symbol = match con_id {
            756733 => "SPY",
            265598 => "AAPL",
            272093 => "MSFT",
            _ => "?",
        };

        Self {
            username: env::var("IB_USERNAME").expect("IB_USERNAME not set"),
            password: env::var("IB_PASSWORD").expect("IB_PASSWORD not set"),
            host: env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string()),
            paper: true,
            con_id,
            symbol,
        }
    }

    pub fn env_u32(name: &str, default: u32) -> u32 {
        env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    pub fn env_bool(name: &str) -> bool {
        env::var(name).map(|v| v == "1").unwrap_or(false)
    }
}

// ─── Connection ───

pub struct BenchSession {
    pub shared: Arc<SharedState>,
    pub event_rx: Receiver<Event>,
    pub control_tx: Sender<ControlCommand>,
    pub connect_time: Duration,
    pub account_id: String,
    _join: Option<std::thread::JoinHandle<()>>,
}

impl BenchSession {
    pub fn connect(config: &BenchConfig) -> Self {
        let gw_config = GatewayConfig {
            username: config.username.clone(),
            password: config.password.clone(),
            host: config.host.clone(),
            paper: config.paper,
        };

        let connect_start = Instant::now();
        let (gw, farm_conn, ccp_conn, hmds_conn) =
            Gateway::connect(&gw_config).expect("Gateway::connect() failed");
        let connect_time = connect_start.elapsed();
        let account_id = gw.account_id.clone();

        let shared = Arc::new(SharedState::new());
        let (event_tx, event_rx) = bounded::<Event>(65536);
        let (mut hot_loop, control_tx) =
            gw.into_hot_loop(shared.clone(), Some(event_tx), farm_conn, ccp_conn, hmds_conn, None);

        let join = std::thread::spawn(move || {
            hot_loop.run();
        });

        Self {
            shared,
            event_rx,
            control_tx,
            connect_time,
            account_id,
            _join: Some(join),
        }
    }

    pub fn subscribe(&self, con_id: i64, symbol: &str) {
        self.control_tx
            .send(ControlCommand::Subscribe {
                con_id,
                symbol: symbol.to_string(),
            })
            .unwrap();
    }

    pub fn subscribe_tbt(&self, con_id: i64, symbol: &str, tbt_type: TbtType) {
        self.control_tx
            .send(ControlCommand::SubscribeTbt {
                con_id,
                symbol: symbol.to_string(),
                tbt_type,
            })
            .unwrap();
    }

    pub fn unsubscribe(&self, instrument: InstrumentId) {
        let _ = self
            .control_tx
            .send(ControlCommand::Unsubscribe { instrument });
    }

    pub fn unsubscribe_tbt(&self, instrument: InstrumentId) {
        let _ = self
            .control_tx
            .send(ControlCommand::UnsubscribeTbt { instrument });
    }

    pub fn send_order(&self, req: OrderRequest) {
        let _ = self.control_tx.send(ControlCommand::Order(req));
    }

    pub fn shutdown(self) {
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        if let Some(join) = self._join {
            let _ = join.join();
        }
    }
}

// ─── Stats ───

pub struct LatencyStats {
    pub samples: Vec<u64>,
}

impl LatencyStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, ns: u64) {
        self.samples.push(ns);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    pub fn report(&self, label: &str) {
        if self.samples.is_empty() {
            println!("  {} (no samples)", label);
            return;
        }
        let mut sorted = self.samples.clone();
        sorted.sort();
        let n = sorted.len();
        let mean = sorted.iter().sum::<u64>() / n as u64;

        println!("{} ({} samples)", label, n);
        println!("  Min:            {}", format_ns(sorted[0]));
        println!("  P50:            {}", format_ns(percentile(&sorted, 0.50)));
        println!("  P95:            {}", format_ns(percentile(&sorted, 0.95)));
        println!("  P99:            {}", format_ns(percentile(&sorted, 0.99)));
        println!("  P99.9:          {}", format_ns(percentile(&sorted, 0.999)));
        println!("  Max:            {}", format_ns(sorted[n - 1]));
        println!("  Mean:           {}", format_ns(mean));
    }

    pub fn report_throughput(&self, label: &str, duration_secs: f64) {
        self.report(label);
        if !self.samples.is_empty() {
            let mean = self.samples.iter().sum::<u64>() / self.samples.len() as u64;
            let throughput = 1_000_000_000.0 / mean as f64;
            println!("  Throughput:     ~{:.0} events/sec", throughput);
            println!("  Duration:       {:.1}s", duration_secs);
        }
    }
}

pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

pub fn format_ns(ns: u64) -> String {
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

pub fn print_header(name: &str) {
    println!();
    println!("========================================");
    println!("  {}", name);
    println!("========================================");
    println!();
}

/// Drain warmup ticks, return instrument ID from first tick.
pub fn warmup(event_rx: &Receiver<Event>, count: u32, start: Instant) -> InstrumentId {
    let mut instrument = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut received = 0u32;
    loop {
        if Instant::now() > deadline {
            panic!("Warmup timed out — are markets open?");
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Tick(id)) => {
                if received == 0 {
                    instrument = id;
                    println!(
                        "[{:.3}s] First tick (instrument={}), warming up ({} ticks)...",
                        start.elapsed().as_secs_f64(),
                        id,
                        count,
                    );
                }
                received += 1;
                if received >= count {
                    break;
                }
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => panic!("Event channel disconnected during warmup"),
        }
    }
    println!("[{:.3}s] Warmup done", start.elapsed().as_secs_f64());
    instrument
}

// ─── Helpers ───

fn load_dotenv(path: &str) {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if env::var(key.trim()).is_err() {
                    unsafe { env::set_var(key.trim(), value.trim()) };
                }
            }
        }
    }
}
