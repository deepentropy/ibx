//! Staying connected, and what the caller gets to say about it.
//!
//! With no gateway in the way, keeping the session is this library's job rather
//! than a daemon's. That work is automatic and needs no configuration: a client
//! built with defaults recovers from a dropped connection, puts its
//! subscriptions back, and says so. Everything here is for the cases where the
//! default is not what a particular process wants.
//!
//! The shape follows what a drop actually is. A connection can fail for reasons
//! that repeat usefully and reasons that do not, so recovery is bounded by a
//! budget rather than run forever, and the budget resets once the session has
//! been healthy for a while — a connection that drops once an hour is not the
//! same as one failing ten times a minute, and only the second is worth giving
//! up on.

/// Why a connection went away, and how long to wait before trying again.
///
/// Read by the engine deciding whether to retry and by the session deciding
/// whether another address is worth knocking on.
pub mod retry;

use std::time::Duration;

/// Who decides when to reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconnectPolicy {
    /// Recover without being asked. The default, and what a process that must
    /// keep running wants.
    #[default]
    Automatic,
    /// Never recover on its own. The client reports the loss and waits; the
    /// caller decides whether and when to reconnect. For a process that would
    /// rather fail loudly than carry on quietly.
    Manual,
}

/// How hard to try, and for how long.
///
/// The defaults are the ones this client uses when nothing is said, and they
/// suit a process that should stay up: recover promptly, keep trying across an
/// outage long enough to cover a maintenance window, and give up only on
/// something that repeating cannot fix.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// What to do when a connection goes away.
    pub policy: ReconnectPolicy,
    /// Attempts allowed before recovery is abandoned, counted since the last
    /// stable period. `None` keeps trying for as long as the reason is one that
    /// might succeed — which is what a server that comes back on its own
    /// schedule, nightly, requires.
    pub max_attempts: Option<u32>,
    /// How long recovery may run before it is abandoned, counted the same way.
    /// `None` for no limit, for the same reason.
    pub max_elapsed: Option<Duration>,
    /// Healthy runtime after which the attempt budget starts again. A drop an
    /// hour is not the failure mode a budget exists to catch.
    pub stable_window: Duration,
    /// Subscriptions re-sent per burst when a connection comes back, and the
    /// pause between bursts. A server that has just come up is the least able
    /// to take everything at once.
    pub replay_burst: usize,
    /// How fast subscriptions are asked for again once it comes back.
    pub replay_pace: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            policy: ReconnectPolicy::Automatic,
            // Unbounded by default. The servers go down nightly and come back
            // on their own; a client that gave up during that window would need
            // a person to start it again, which is the failure this is here to
            // prevent.
            max_attempts: None,
            max_elapsed: None,
            stable_window: Duration::from_secs(60),
            replay_burst: 50,
            replay_pace: Duration::from_millis(5),
        }
    }
}

impl ReconnectConfig {
    /// Recover, but give up after this many attempts.
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = Some(attempts);
        self
    }

    /// Recover, but give up after this long trying.
    pub fn with_max_elapsed(mut self, elapsed: Duration) -> Self {
        self.max_elapsed = Some(elapsed);
        self
    }

    /// Report a loss and do nothing about it.
    pub fn manual() -> Self {
        Self { policy: ReconnectPolicy::Manual, ..Self::default() }
    }
}

/// What recovery has cost so far, and whether it may continue.
#[derive(Debug, Clone)]
pub struct RecoveryBudget {
    attempts: u32,
    started: Option<std::time::Instant>,
    healthy_since: Option<std::time::Instant>,
}

impl Default for RecoveryBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl RecoveryBudget {
    /// A budget that has spent nothing.
    pub fn new() -> Self {
        Self { attempts: 0, started: None, healthy_since: None }
    }

    /// Count an attempt. The clock starts on the first one.
    pub fn record_attempt(&mut self, now: std::time::Instant) {
        self.attempts += 1;
        self.started.get_or_insert(now);
        self.healthy_since = None;
    }

    /// The connection is up. The budget is not returned immediately — a
    /// connection that comes back and drops again a second later has not
    /// recovered, and counting it as a fresh start is how a client ends up
    /// looping forever on a budget it never spends.
    pub fn record_connected(&mut self, now: std::time::Instant) {
        self.healthy_since.get_or_insert(now);
    }

    /// Give the budget back once the session has held together long enough.
    ///
    /// `whole` is whether every transport this budget bounds is carrying
    /// traffic. One of them coming back while another is still down is not a
    /// session that held: the budget is one between them, and refunding it on
    /// the first recovery let the transport that never came back retry for
    /// ever, past every limit the caller set.
    pub fn settle(&mut self, now: std::time::Instant, stable_window: Duration, whole: bool) {
        if !whole {
            self.healthy_since = None;
            return;
        }
        if let Some(since) = self.healthy_since
            && now.duration_since(since) >= stable_window {
                self.attempts = 0;
                self.started = None;
            }
    }

    /// How many times recovery has been tried.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether recovery has been running longer than the caller allowed.
    ///
    /// Asked on its own as well as by `may_retry`, because the two limits
    /// bound different things: the attempt limit says how many more attempts
    /// may start, and the elapsed limit says how long recovery may go on —
    /// which includes the attempt that is running. One attempt outlasts a
    /// short limit easily, a handshake waiting on an approval somebody gives
    /// on their phone most of all.
    pub fn out_of_time(&self, cfg: &ReconnectConfig, now: std::time::Instant) -> bool {
        matches!(
            (cfg.max_elapsed, self.started),
            (Some(max), Some(started)) if now.duration_since(started) >= max
        )
    }

    /// Whether another attempt is within what the caller allowed.
    pub fn may_retry(&self, cfg: &ReconnectConfig, now: std::time::Instant) -> bool {
        if cfg.policy == ReconnectPolicy::Manual {
            return false;
        }
        if cfg.max_attempts.is_some_and(|max| self.attempts >= max) {
            return false;
        }
        !self.out_of_time(cfg, now)
    }

    /// Whether what the caller allowed has actually run out.
    ///
    /// Not the same question as whether to dial, and the difference is the
    /// whole of this: a caller who asked to do the reconnecting itself has
    /// allowed no attempts and spent none. Read off `may_retry`, that caller's
    /// first dropped connection was reported as limits running out — limits it
    /// never set — and the session was declared over for good on it, with the
    /// words "the recovery limits the caller set are spent" handed to every
    /// reader. What that caller asked for was to be told and left alone.
    pub fn is_spent(&self, cfg: &ReconnectConfig, now: std::time::Instant) -> bool {
        if cfg.max_attempts.is_some_and(|max| self.attempts >= max) {
            return true;
        }
        self.out_of_time(cfg, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A client that is given no instructions keeps trying, because the servers
    /// this talks to go down nightly and come back by themselves.
    #[test]
    fn the_default_is_to_keep_trying() {
        let cfg = ReconnectConfig::default();
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        for _ in 0..10_000 {
            assert!(budget.may_retry(&cfg, t));
            budget.record_attempt(t);
        }
    }

    #[test]
    fn a_budget_is_spent_and_then_refused() {
        let cfg = ReconnectConfig::default().with_max_attempts(3);
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        for _ in 0..3 {
            assert!(budget.may_retry(&cfg, t));
            budget.record_attempt(t);
        }
        assert!(!budget.may_retry(&cfg, t), "the fourth is past what was allowed");
    }

    /// A drop an hour is not the failure a budget exists to catch. Only a
    /// session that cannot hold together should exhaust one.
    #[test]
    fn a_session_that_holds_gets_its_budget_back() {
        let cfg = ReconnectConfig::default().with_max_attempts(2);
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        budget.record_attempt(t);
        budget.record_attempt(t);
        assert!(!budget.may_retry(&cfg, t));

        budget.record_connected(t);
        budget.settle(t + cfg.stable_window, cfg.stable_window, true);
        assert!(budget.may_retry(&cfg, t), "an hour of health is a fresh start");
        assert_eq!(budget.attempts(), 0);
    }

    /// Half a session is not a session that held.
    ///
    /// The budget bounds the recovery of both transports together. One coming
    /// back started the clock on its own, and the refund that followed put the
    /// whole budget back while the other was still down and still being
    /// retried, so a caller who bounded the effort never saw it stop.
    #[test]
    fn a_session_that_is_half_back_does_not_get_its_budget_back() {
        let cfg = ReconnectConfig::default().with_max_attempts(2);
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        budget.record_attempt(t);
        budget.record_attempt(t);
        // One transport is carrying traffic again; the other is not.
        budget.record_connected(t);
        budget.settle(t + cfg.stable_window, cfg.stable_window, false);
        assert!(!budget.may_retry(&cfg, t), "the budget came back on half a session");

        // And once it is whole, the clock starts from there rather than from
        // the half-recovery that preceded it.
        budget.record_connected(t + cfg.stable_window);
        budget.settle(t + cfg.stable_window, cfg.stable_window, true);
        assert!(!budget.may_retry(&cfg, t), "the window had not passed yet");
        budget.settle(t + cfg.stable_window * 2, cfg.stable_window, true);
        assert!(budget.may_retry(&cfg, t), "a whole session that held is a fresh start");
    }

    /// Coming back for a moment is not recovering.
    #[test]
    fn a_connection_that_flaps_does_not_get_its_budget_back() {
        let cfg = ReconnectConfig::default().with_max_attempts(2);
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        budget.record_attempt(t);
        budget.record_attempt(t);
        budget.record_connected(t);
        // Down again well inside the window.
        budget.settle(t + Duration::from_secs(1), cfg.stable_window, true);
        assert!(!budget.may_retry(&cfg, t), "a second up is not a stable session");
    }

    #[test]
    fn a_manual_policy_recovers_nothing_by_itself() {
        let cfg = ReconnectConfig::manual();
        let budget = RecoveryBudget::new();
        assert!(!budget.may_retry(&cfg, Instant::now()));
    }

    #[test]
    fn elapsed_time_bounds_recovery_as_well_as_attempts() {
        let cfg = ReconnectConfig::default().with_max_elapsed(Duration::from_secs(30));
        let mut budget = RecoveryBudget::new();
        let t = Instant::now();
        budget.record_attempt(t);
        assert!(budget.may_retry(&cfg, t + Duration::from_secs(29)));
        assert!(!budget.may_retry(&cfg, t + Duration::from_secs(31)));
    }
}
