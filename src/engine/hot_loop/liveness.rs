//! Liveness of the server connections, as the reference checks it (ibx#419).
//!
//! Auth connection: a monitor polled once a second. Every send on the link
//! starts a pause of 60 s during which the monitor checks nothing. Outside
//! the pause, in this order:
//! 1. when the receive silence grew by more than ten polls since the last
//!    check (the process was asleep), the counters are reset and nothing
//!    else is done;
//! 2. receive silence over 35 s: the link is dead;
//! 3. send silence of 10 s: a heartbeat is sent (else, receive silence of
//!    11 s is logged);
//! 4. receive silence of 15 s and no test request waiting: a test request
//!    is sent.
//!
//! Since these sends start a new pause, and the server asks for an answer
//! after about 20 s of silence, the monitor normally never runs while the
//! server is alive. A silent server is found at the first check after the
//! pause, about 62 s after the last send.
//!
//! Farms (market data and historical): a test request every 60 s from the
//! logon; the link is dead when nothing at all arrives within 10 s of it.
//! There is no other own heartbeat on a farm: the server's test requests
//! are answered, and that is all.

use std::time::{Duration, Instant};

/// Poll period of the auth link monitor.
pub const CCP_POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// Pause after every send on the auth link: the monitor checks nothing.
/// It ends at the first poll more than this long after the send; that poll
/// is skipped too.
pub const CCP_SEND_PAUSE: Duration = Duration::from_secs(60);
/// Send silence after which the monitor sends a heartbeat.
pub const CCP_HEARTBEAT_AFTER: Duration = Duration::from_secs(crate::config::CCP_HEARTBEAT);
/// Receive silence that is logged as a missing heartbeat answer.
pub const CCP_SILENCE_WARNING: Duration = Duration::from_secs(11);
/// Receive silence after which the monitor sends a test request.
pub const CCP_TEST_AFTER: Duration = Duration::from_secs(15);
/// Receive silence over which the auth link is dead.
pub const CCP_DEAD_AFTER: Duration = Duration::from_secs(35);
/// Growth of the receive silence between two checks (ten polls) that is
/// taken for a sleep of the process: counters reset, no disconnect.
pub const CCP_SLEEP_JUMP: Duration = Duration::from_millis(10 * 1000);
/// Latest time, after the last send and the last receive on a silent auth
/// link, at which the monitor declares it dead: the pause, the poll that
/// ends it, and the poll that checks.
pub const CCP_DEAD_LIMIT: Duration = Duration::from_millis(60_000 + 2 * 1000);

/// Period of the farm test request, from the farm logon.
pub const FARM_PING_INTERVAL: Duration = Duration::from_secs(60);
/// Time after a farm test request within which something must arrive.
pub const FARM_PING_ANSWER: Duration = Duration::from_secs(10);
/// Latest time after a farm goes silent at which it is declared dead.
pub const FARM_DEAD_LIMIT: Duration = Duration::from_secs(60 + 10);

/// Text of a test request id; the number is the engine's own counter.
pub(crate) const TEST_REQUEST_PREFIX: &str = "FixTestRequest";

/// What one poll of the auth link monitor asks for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CcpCheck {
    /// The link is dead: disconnect.
    pub dead: bool,
    /// Send a heartbeat.
    pub heartbeat: bool,
    /// Send a test request.
    pub test_request: bool,
}

/// What one look at a farm link asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarmCheck {
    Nothing,
    /// Send the periodic test request.
    Ping,
    /// Nothing arrived within the answer time of the last test request.
    Dead,
}

/// Tracks last send/recv times and pending test requests for heartbeat management.
pub struct HeartbeatState {
    pub last_ccp_sent: Instant,
    pub last_ccp_recv: Instant,
    pub last_farm_sent: Instant,
    pub last_farm_recv: Instant,
    pub last_hmds_sent: Instant,
    pub last_hmds_recv: Instant,
    /// Pending test request for auth: (test_req_id, sent_at). Cleared by
    /// any data received.
    pub pending_ccp_test: Option<(String, Instant)>,
    /// Pending test request for farm: (test_req_id, sent_at). Cleared by
    /// any data received.
    pub pending_farm_test: Option<(String, Instant)>,
    /// Pending test request for historical: (test_req_id, sent_at).
    pub pending_hmds_test: Option<(String, Instant)>,
    /// Next poll of the auth link monitor.
    ccp_next_poll: Instant,
    /// The send whose pause the monitor ended. The pause of the last send
    /// is on while this differs from `last_ccp_sent`.
    ccp_pause_ended_for: Option<Instant>,
    /// Receive silence seen by the last check outside the pause; zero
    /// before the first one.
    ccp_last_silence: Duration,
    /// Next periodic test request on the market data farm.
    farm_next_ping: Instant,
    /// Next periodic test request on the historical farm.
    hmds_next_ping: Instant,
    /// Counter for generating unique test request IDs.
    test_req_counter: u32,
}

impl HeartbeatState {
    pub(crate) fn new() -> Self {
        Self::new_at(Instant::now())
    }

    /// State of links that just logged on at `now`: the logon counts as the
    /// last send and the last receive.
    pub fn new_at(now: Instant) -> Self {
        Self {
            last_ccp_sent: now,
            last_ccp_recv: now,
            last_farm_sent: now,
            last_farm_recv: now,
            last_hmds_sent: now,
            last_hmds_recv: now,
            pending_ccp_test: None,
            pending_farm_test: None,
            pending_hmds_test: None,
            ccp_next_poll: now + CCP_POLL_INTERVAL,
            ccp_pause_ended_for: None,
            ccp_last_silence: Duration::ZERO,
            farm_next_ping: now + FARM_PING_INTERVAL,
            hmds_next_ping: now + FARM_PING_INTERVAL,
            test_req_counter: 0,
        }
    }

    pub(crate) fn next_test_id(&mut self) -> String {
        let id = format!("{}{}", TEST_REQUEST_PREFIX, self.test_req_counter);
        self.test_req_counter = self.test_req_counter.wrapping_add(1);
        id
    }

    /// A new auth connection at `now`: its logon was the last send and the
    /// last receive, the monitor starts again.
    pub(crate) fn ccp_connected(&mut self, now: Instant) {
        self.last_ccp_sent = now;
        self.last_ccp_recv = now;
        self.pending_ccp_test = None;
        self.ccp_next_poll = now + CCP_POLL_INTERVAL;
        self.ccp_pause_ended_for = None;
        self.ccp_last_silence = Duration::ZERO;
    }

    /// A new market data farm connection at `now`.
    pub(crate) fn farm_connected(&mut self, now: Instant) {
        self.last_farm_sent = now;
        self.last_farm_recv = now;
        self.pending_farm_test = None;
        self.farm_next_ping = now + FARM_PING_INTERVAL;
    }

    /// A new historical farm connection at `now`.
    pub(crate) fn hmds_connected(&mut self, now: Instant) {
        self.last_hmds_sent = now;
        self.last_hmds_recv = now;
        self.pending_hmds_test = None;
        self.hmds_next_ping = now + FARM_PING_INTERVAL;
    }

    /// One poll of the auth link monitor, at most once per
    /// `CCP_POLL_INTERVAL`. The caller sends what it asks for and records
    /// the sends (`last_ccp_sent`, `pending_ccp_test`).
    pub fn poll_ccp(&mut self, now: Instant) -> CcpCheck {
        let mut check = CcpCheck::default();
        if now < self.ccp_next_poll {
            return check;
        }
        self.ccp_next_poll = now + CCP_POLL_INTERVAL;

        // The pause of the last send: skip, up to and including the first
        // poll more than the pause after it.
        if self.ccp_pause_ended_for != Some(self.last_ccp_sent) {
            if now.saturating_duration_since(self.last_ccp_sent) > CCP_SEND_PAUSE {
                self.ccp_pause_ended_for = Some(self.last_ccp_sent);
            }
            return check;
        }

        let silence = now.saturating_duration_since(self.last_ccp_recv);
        let before = std::mem::replace(&mut self.ccp_last_silence, silence);
        if !before.is_zero() && silence > before + CCP_SLEEP_JUMP {
            log::info!("Auth link: receive silence jumped by {:?} (asleep?), counters reset", silence - before);
            self.last_ccp_recv = now;
            self.pending_ccp_test = None;
            return check;
        }
        if silence > CCP_DEAD_AFTER {
            check.dead = true;
            return check;
        }
        if now.saturating_duration_since(self.last_ccp_sent) >= CCP_HEARTBEAT_AFTER {
            check.heartbeat = true;
        } else if silence >= CCP_SILENCE_WARNING {
            log::warn!("Auth link: no heartbeat answer, nothing received for {:?}", silence);
        }
        if silence >= CCP_TEST_AFTER && self.pending_ccp_test.is_none() {
            check.test_request = true;
        }
        check
    }

    /// Look at the market data farm link.
    pub fn poll_farm(&mut self, now: Instant) -> FarmCheck {
        farm_check(now, &mut self.farm_next_ping, &self.pending_farm_test)
    }

    /// Look at the historical farm link.
    pub fn poll_hmds(&mut self, now: Instant) -> FarmCheck {
        farm_check(now, &mut self.hmds_next_ping, &self.pending_hmds_test)
    }
}

fn farm_check(now: Instant, next_ping: &mut Instant, pending: &Option<(String, Instant)>) -> FarmCheck {
    if let Some((_, sent_at)) = pending {
        if now.saturating_duration_since(*sent_at) >= FARM_PING_ANSWER {
            return FarmCheck::Dead;
        }
    }
    if now >= *next_ping {
        *next_ping += FARM_PING_INTERVAL;
        if *next_ping <= now {
            *next_ping = now + FARM_PING_INTERVAL;
        }
        return FarmCheck::Ping;
    }
    FarmCheck::Nothing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration { Duration::from_secs(s) }

    /// Poll every second from `from` to `to` (seconds after `t0`), with
    /// the sends the monitor asks for recorded as the hot loop does.
    /// Returns the second of the first dead check.
    fn run_ccp(hb: &mut HeartbeatState, t0: Instant, from: u64, to: u64, sends: &mut Vec<(u64, CcpCheck)>) -> Option<u64> {
        for s in from..=to {
            let now = t0 + secs(s);
            let check = hb.poll_ccp(now);
            if check.dead {
                return Some(s);
            }
            if check.heartbeat || check.test_request {
                hb.last_ccp_sent = now;
                if check.test_request {
                    hb.pending_ccp_test = Some(("t".into(), now));
                }
                sends.push((s, check));
            }
        }
        None
    }

    // A silent auth link is dead at the first check after the pause of the
    // last send: about 62 s, never at 35 s.
    #[test]
    fn silent_auth_link_is_dead_after_the_send_pause() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        let mut sends = Vec::new();
        let dead = run_ccp(&mut hb, t0, 1, 120, &mut sends);
        assert_eq!(dead, Some(62));
        assert!(sends.is_empty(), "nothing is sent during the pause: {:?}", sends);
        assert!(secs(62) <= CCP_DEAD_LIMIT);
    }

    // Every send restarts the pause: a link that keeps sending is never
    // checked.
    #[test]
    fn every_send_restarts_the_pause() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        for s in 1..=300 {
            let now = t0 + secs(s);
            if s % 50 == 0 && s <= 250 {
                hb.last_ccp_sent = now; // an order, a request, an echo
            }
            assert_eq!(hb.poll_ccp(now), CcpCheck::default(), "second {}", s);
        }
        // Silent from 250: dead at 250 + 62.
        let mut sends = Vec::new();
        assert_eq!(run_ccp(&mut hb, t0, 301, 400, &mut sends), Some(312));
    }

    // Outside the pause, with data still arriving: a heartbeat after 10 s
    // of send silence, which starts a new pause.
    #[test]
    fn heartbeat_when_the_pause_ends_with_a_live_server() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        let mut sends = Vec::new();
        for s in 1..=200 {
            let now = t0 + secs(s);
            hb.last_ccp_recv = now; // the server keeps talking
            let check = hb.poll_ccp(now);
            assert!(!check.dead);
            if check.heartbeat {
                hb.last_ccp_sent = now;
                sends.push(s);
            }
            assert!(!check.test_request, "data arrives: no test request");
        }
        assert_eq!(sends, vec![62, 124, 186]);
    }

    // A server that went silent while the pause ran, with less than 35 s
    // of silence at the first check after it: a heartbeat and a test
    // request in one poll (the send silence is always over 10 s there).
    // They start a new pause, and the check after it sees the silence
    // jump by more than ten polls: the reference takes that for a sleep and
    // resets its counters, so the link is not reset by the monitor. This is
    // the reference's rule as written; only a socket error ends such a link.
    #[test]
    fn heartbeat_and_test_request_in_one_poll() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        let mut sends = Vec::new();
        for s in 1..=40 {
            hb.last_ccp_recv = t0 + secs(s);
            assert_eq!(hb.poll_ccp(t0 + secs(s)), CcpCheck::default());
        }
        let dead = run_ccp(&mut hb, t0, 41, 200, &mut sends);
        let hb_only = CcpCheck { dead: false, heartbeat: true, test_request: false };
        assert_eq!(sends, vec![
            (62, CcpCheck { dead: false, heartbeat: true, test_request: true }),
            (125, hb_only),
            (188, hb_only),
        ]);
        assert_eq!(dead, None);
    }

    // One test request at a time: none while one waits for an answer.
    #[test]
    fn one_test_request_at_a_time() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        hb.ccp_pause_ended_for = Some(hb.last_ccp_sent);
        hb.pending_ccp_test = Some(("t".into(), t0));
        let check = hb.poll_ccp(t0 + secs(20));
        assert_eq!(check, CcpCheck { dead: false, heartbeat: true, test_request: false });
    }

    // A jump of the silence by more than ten polls (sleep) resets the
    // counters instead of killing the link.
    #[test]
    fn a_sleep_resets_the_counters() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        hb.last_ccp_recv = t0 + secs(100);
        hb.last_ccp_sent = t0 + secs(100);
        hb.ccp_pause_ended_for = Some(hb.last_ccp_sent);
        assert_eq!(hb.poll_ccp(t0 + secs(101)), CcpCheck::default());
        // Asleep for 50 s: the next poll sees 51 s of silence.
        assert_eq!(hb.poll_ccp(t0 + secs(151)), CcpCheck::default());
        assert_eq!(hb.last_ccp_recv, t0 + secs(151), "counters reset");
        let check = hb.poll_ccp(t0 + secs(152));
        assert!(!check.dead);
    }

    // The monitor runs once a second, not on every loop pass.
    #[test]
    fn auth_monitor_polls_once_a_second() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        hb.ccp_pause_ended_for = Some(hb.last_ccp_sent);
        hb.ccp_next_poll = t0 + secs(40);
        assert_eq!(hb.poll_ccp(t0 + Duration::from_millis(39_999)), CcpCheck::default());
        assert!(hb.poll_ccp(t0 + secs(40)).dead);
    }

    // Farm: a test request every 60 s from the logon, dead when nothing
    // arrived within 10 s of it.
    #[test]
    fn farm_ping_every_minute_and_dead_ten_seconds_later() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        assert_eq!(hb.poll_farm(t0 + secs(59)), FarmCheck::Nothing);
        assert_eq!(hb.poll_farm(t0 + secs(60)), FarmCheck::Ping);
        hb.pending_farm_test = Some(("t".into(), t0 + secs(60)));
        assert_eq!(hb.poll_farm(t0 + secs(69)), FarmCheck::Nothing);
        // An answer (any data) clears the wait.
        hb.pending_farm_test = None;
        assert_eq!(hb.poll_farm(t0 + secs(75)), FarmCheck::Nothing);
        assert_eq!(hb.poll_farm(t0 + secs(120)), FarmCheck::Ping, "fixed period from the logon");
        hb.pending_farm_test = Some(("t".into(), t0 + secs(120)));
        assert_eq!(hb.poll_farm(t0 + secs(130)), FarmCheck::Dead);
        assert!(secs(130 - 60) <= FARM_DEAD_LIMIT);
    }

    #[test]
    fn historical_farm_has_its_own_ping() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        hb.hmds_connected(t0 + secs(30));
        assert_eq!(hb.poll_hmds(t0 + secs(60)), FarmCheck::Nothing);
        assert_eq!(hb.poll_hmds(t0 + secs(90)), FarmCheck::Ping);
        assert_eq!(hb.poll_farm(t0 + secs(60)), FarmCheck::Ping);
    }

    #[test]
    fn reconnect_starts_the_auth_monitor_again() {
        let t0 = Instant::now();
        let mut hb = HeartbeatState::new_at(t0);
        let mut sends = Vec::new();
        assert_eq!(run_ccp(&mut hb, t0, 1, 100, &mut sends), Some(62));
        hb.ccp_connected(t0 + secs(70));
        assert_eq!(run_ccp(&mut hb, t0, 71, 200, &mut sends), Some(132));
    }

    #[test]
    fn test_request_ids_count_up() {
        let mut hb = HeartbeatState::new_at(Instant::now());
        assert_eq!(hb.next_test_id(), "FixTestRequest0");
        assert_eq!(hb.next_test_id(), "FixTestRequest1");
    }

    #[test]
    fn thresholds_are_the_reference_values() {
        assert_eq!(CCP_POLL_INTERVAL, Duration::from_secs(1));
        assert_eq!(CCP_HEARTBEAT_AFTER, Duration::from_secs(10));
        assert_eq!(CCP_TEST_AFTER, Duration::from_secs(15));
        assert_eq!(CCP_DEAD_AFTER, Duration::from_secs(35));
        assert_eq!(CCP_SEND_PAUSE, Duration::from_secs(60));
        assert_eq!(CCP_DEAD_LIMIT, CCP_SEND_PAUSE + 2 * CCP_POLL_INTERVAL);
        assert_eq!(FARM_PING_INTERVAL, Duration::from_secs(60));
        assert_eq!(FARM_PING_ANSWER, Duration::from_secs(10));
        assert_eq!(FARM_DEAD_LIMIT, FARM_PING_INTERVAL + FARM_PING_ANSWER);
    }
}
