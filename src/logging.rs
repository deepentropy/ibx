use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::EnvFilter;

use crate::config::days_to_ymd;

/// Logging configuration.
pub struct LogConfig {
    /// Directory for log files. `None` = console only.
    pub log_dir: Option<PathBuf>,
    /// Filter directive (e.g. `"info"`, `"ibx=debug,warn"`).
    /// Falls back to `RUST_LOG` env var, then `"info"`.
    pub level: Option<String>,
    /// Non-blocking channel capacity (records before dropping). Default: 65536.
    pub queue_capacity: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            log_dir: None,
            level: None,
            queue_capacity: 65_536,
        }
    }
}

impl LogConfig {
    /// Build from environment variables:
    /// - `IBX_LOG_DIR`   — log file directory (omit for console-only)
    /// - `IBX_LOG_LEVEL` — filter directive (falls back to `RUST_LOG`, then `info`)
    /// - `IBX_LOG_QUEUE` — non-blocking buffer capacity (default: 65536)
    pub fn from_env() -> Self {
        Self {
            log_dir: std::env::var("IBX_LOG_DIR").ok().map(PathBuf::from),
            level: std::env::var("IBX_LOG_LEVEL").ok(),
            queue_capacity: std::env::var("IBX_LOG_QUEUE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(65_536),
        }
    }
}

/// Nanosecond-precision UTC timestamp.
///
/// Format: `2026-03-24T15:30:45.123456789Z`
///
/// Uses `SystemTime` for wall-clock nanoseconds. On Windows (QPC-backed),
/// typical resolution is ~100ns. On Linux, ~1ns.
struct NanoTimestamp;

impl FormatTime for NanoTimestamp {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch");
        let total_secs = dur.as_secs();
        let nanos = dur.subsec_nanos();
        let days = total_secs / 86_400;
        let day_secs = total_secs % 86_400;
        let h = day_secs / 3_600;
        let m = (day_secs % 3_600) / 60;
        let s = day_secs % 60;
        let (y, mo, d) = days_to_ymd(days);
        write!(w, "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{nanos:09}Z")
    }
}

/// Holds the non-blocking writer's background thread. Must outlive all logging.
/// Dropping this flushes pending records and joins the writer thread.
pub struct LogGuard {
    _guard: WorkerGuard,
}

/// Initialize the logging subsystem. Returns a [`LogGuard`] that **must** be
/// held until process exit — dropping it flushes buffered records and joins the
/// background writer thread.
///
/// Existing `log::info!()` etc. calls are bridged automatically via `tracing-log`.
pub fn init(config: &LogConfig) -> LogGuard {
    let filter = match &config.level {
        Some(level) => EnvFilter::try_new(level)
            .unwrap_or_else(|_| EnvFilter::new("info")),
        None => EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")),
    };

    let (writer, guard) = match &config.log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).expect("failed to create log directory");
            let appender = tracing_appender::rolling::daily(dir, "ibx.log");
            tracing_appender::non_blocking(appender)
        }
        None => tracing_appender::non_blocking(std::io::stdout()),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(NanoTimestamp)
        .with_writer(writer)
        .with_ansi(config.log_dir.is_none())
        .init();

    LogGuard { _guard: guard }
}
