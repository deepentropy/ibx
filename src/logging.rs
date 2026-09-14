//! Where this client writes what it did.
//!
//! Not the caller-facing surface. What a program written against this client
//! touches is [`crate::api`], which is documented in full and gated on staying
//! that way. This module is the engine underneath it, exported because the
//! binaries, benchmarks and integration tests in this repository reach it.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::EnvFilter;

use crate::protocol::datetime::days_to_ymd;

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

    /// The logging a caller stated, else what the environment holds.
    ///
    /// The same order every other setting resolves in: what the caller states
    /// wins over the environment, which wins over the default. Taken and
    /// dropped instead, a caller who stated a level or a directory got
    /// whatever the environment held and no word about it.
    pub fn stated(settings: &crate::settings::GatewaySettings) -> Self {
        let environment = Self::from_env();
        let named = |value: Option<&String>| {
            value.filter(|v| !v.is_empty()).cloned()
        };
        Self {
            log_dir: named(settings.log_dir.as_ref())
                .map(PathBuf::from)
                .or(environment.log_dir),
            level: named(settings.log_level.as_ref()).or(environment.level),
            queue_capacity: settings.log_queue.unwrap_or(environment.queue_capacity),
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

/// An account identifier as a log may carry it.
///
/// Enough to tell two logins apart in a log and no more. A session's records
/// are read by whoever can read the run that produced them, and for a run in
/// public continuous integration that is everybody; the name a session logs in
/// under is not a secret but it is an account detail, and a line that prints
/// one publishes it.
pub fn redacted(identifier: &str) -> String {
    match identifier.chars().count() {
        0 => String::new(),
        n if n <= 4 => "*".repeat(n),
        n => {
            let kept: String = identifier.chars().take(2).collect();
            format!("{kept}{}", "*".repeat(n - 2))
        }
    }
}

/// Install a console logger if none is installed yet.
///
/// For a process that runs for its own lifetime and has nowhere to keep a
/// guard: records go straight to stderr rather than through the background
/// writer, so there is nothing to flush and nothing to hold. Idempotent, so a
/// second call while a logger is installed does nothing and reports that it
/// did nothing.
///
/// `default_level` applies only when `RUST_LOG` says nothing.
pub fn try_init_from_env(default_level: &str) -> bool {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(NanoTimestamp)
                .with_writer(std::io::stderr),
        )
        .try_init()
        .is_ok();
    if installed {
        let _ = LEVEL.set(handle);
    }
    installed
}

/// The filter this client installed, where it installed one.
///
/// Held so a caller can move the level while the session runs, which is what
/// asking the thing serving you to log more loudly means when the thing
/// serving you is a library. Empty where a program installed its own logger:
/// that one is not this client's to move.
static LEVEL: std::sync::OnceLock<
    tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>,
> = std::sync::OnceLock::new();

/// Move the level of the logger this client installed.
///
/// `false` where there is none to move — a program that installed its own
/// logger keeps it, and saying otherwise would tell a caller the level changed
/// when it did not.
pub fn set_level(level: &str) -> bool {
    let Some(handle) = LEVEL.get() else { return false };
    let Ok(filter) = EnvFilter::try_new(level) else { return false };
    handle.reload(filter).is_ok()
}

/// Install the logger the environment asks for, when there may already be one.
///
/// `None` means a logger was already installed and this call did nothing, which
/// is the answer a module initialiser wants rather than a panic.
/// Initialize the logging subsystem. Returns a [`LogGuard`] that **must** be
/// held until process exit — dropping it flushes buffered records and joins the
/// background writer thread.
///
/// Existing `log::info!()` etc. calls are bridged automatically via `tracing-log`.
pub fn init(config: &LogConfig) -> LogGuard {
    try_init(config).expect("a logger is already installed")
}

pub fn try_init(config: &LogConfig) -> Option<LogGuard> {
    let filter = match &config.level {
        Some(level) => EnvFilter::try_new(level)
            .unwrap_or_else(|_| EnvFilter::new("info")),
        None => EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")),
    };

    // Records the writer may hold before it starts dropping them. Left to the
    // appender's own default, `queue_capacity` was read from the environment
    // and thrown away, so the one setting that says how much a busy session
    // may buffer said nothing.
    let buffered = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(config.queue_capacity);
    let (writer, guard) = match &config.log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).expect("failed to create log directory");
            buffered.finish(tracing_appender::rolling::daily(dir, "ibx.log"))
        }
        None => buffered.finish(std::io::stdout()),
    };

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // Installed behind a handle this client keeps, so the level can be moved
    // while the session runs rather than only as it opens.
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(NanoTimestamp)
                .with_writer(writer)
                .with_ansi(config.log_dir.is_none()),
        )
        .try_init()
        .ok()
        .map(|()| {
            let _ = LEVEL.set(handle);
            LogGuard { _guard: guard }
        })
}

/// Install the logger a session's settings ask for, as the session opens.
///
/// A gateway reads its logging configuration once, as the process starts, and
/// this is the same moment: the first session that states any of the three
/// installs the logger from what its caller stated, else from the environment.
/// A process has one logger and whoever installed it holds what flushes it, so
/// a session that opens later cannot move it — what that session stated is
/// named in a warning rather than dropped, which is what stating a log level
/// and getting neither the level nor a word about it used to do.
///
/// A session that states none of the three installs nothing, so a program that
/// installs its own logger keeps it.
pub fn apply(settings: &crate::settings::GatewaySettings) {
    let stated: Vec<&str> = [
        ("log_level", settings.log_level.is_some()),
        ("log_dir", settings.log_dir.is_some()),
        ("log_queue", settings.log_queue.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, stated)| stated.then_some(name))
    .collect();
    if stated.is_empty() {
        return;
    }
    match try_init(&LogConfig::stated(settings)) {
        Some(guard) => guard.keep_for_the_process(),
        None => log::warn!(
            "{} stated after this process installed its logger, and a process has \
             one: logging is settled before the first session opens, and this \
             session runs under the logger that is already installed",
            stated.join(", "),
        ),
    }
}

impl LogGuard {
    /// The guard a process keeps for its own lifetime.
    ///
    /// A module initialiser has no scope to hold one in, and the writer behind
    /// a log directory runs on a thread whose records outlive that scope, so
    /// the guard is kept rather than dropped at the end of the call.
    pub fn keep_for_the_process(self) {
        static KEPT: std::sync::OnceLock<LogGuard> = std::sync::OnceLock::new();
        let _ = KEPT.set(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A level moves the logger this client installed, and nothing else.
    ///
    /// This call was taken and not applied for as long as it existed: what a
    /// caller stated was written to the log rather than applied to it. The
    /// venue has no message for it at all, so a level a caller states is about
    /// the thing serving that caller rather than about the venue — and on a
    /// client that runs in the caller's own process, that is this library.
    ///
    /// A program that installed its own logger keeps it, and this must say so
    /// rather than report a level it did not move.
    #[test]
    fn a_level_moves_the_logger_this_client_installed() {
        // Whether this process already has one is not this test's to decide:
        // it shares a process with every other test, and whoever got there
        // first holds it.
        let _ = try_init_from_env("info");
        match LEVEL.get() {
            Some(_) => {
                assert!(set_level("debug"), "the level this client holds is its own to move");
                assert!(set_level("info"), "and moves back");
            }
            // Somebody else's logger. The call answers that it did not move it,
            // which is the whole of what this guards.
            None => assert!(!set_level("debug"), "a logger this client did not install is not moved"),
        }
    }
}
