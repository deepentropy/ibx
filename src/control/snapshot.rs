//! Plain snapshot of reqMktData (ibx#446), as the reference's API
//! subscriber does it: each tick type of the answer is sent once; the
//! snapshot ends when the bid, ask, last, open and close were sent (with the
//! option computations for an option), or 11 s after it started. A snapshot
//! with generic ticks is refused, and so is a snapshot beyond the per-second
//! limit.

use std::time::{Duration, Instant};

/// A snapshot that is not complete ends this long after it started.
pub const TIMEOUT: Duration = Duration::from_millis(11_000);

/// The refusal of a snapshot with generic ticks.
pub const GENERIC_TICKS_REFUSED: &str =
    "Error validating request.-'bQ' : cause - Snapshot market data subscription is not applicable to generic ticks";

/// The refusal of a snapshot beyond the limit, for `limit` per second.
pub fn rate_refusal(limit: u32) -> String {
    format!("Error validating request.-'bQ' : cause - Snapshot requests limitation exceeded:{} per 1 second(s)", limit)
}

/// The bit of an API tick type in a snapshot; 0 for the types a snapshot
/// never sends on their own (the sizes of bid, ask and last, which come
/// with their price).
pub fn tick_bit(tick_type: i32) -> u64 {
    match tick_type {
        1 => 1 << 0,
        2 => 1 << 1,
        4 => 1 << 2,
        14 => 1 << 3,
        9 => 1 << 4,
        13 => 1 << 5,
        10 => 1 << 6,
        11 => 1 << 7,
        12 => 1 << 8,
        6 => 1 << 9,
        7 => 1 << 10,
        8 => 1 << 11,
        49 => 1 << 12,
        45 => 1 << 13,
        32 => 1 << 14,
        33 => 1 << 15,
        84 => 1 << 16,
        50 => 1 << 17,
        51 => 1 << 18,
        52 => 1 << 19,
        66 => 1 << 20,
        67 => 1 << 21,
        68 => 1 << 22,
        76 => 1 << 23,
        75 => 1 << 24,
        88 => 1 << 25,
        80 => 1 << 26,
        81 => 1 << 27,
        82 => 1 << 28,
        83 => 1 << 29,
        72 => 1 << 30,
        73 => 1 << 31,
        74 => 1 << 32,
        90 => 1 << 33,
        103 => 1 << 34,
        104 => 1 << 35,
        _ => 0,
    }
}

/// Bid, ask, last, open, close.
const DONE: u64 = 0x1F;
/// The same with the four option computations.
const DONE_OPTION: u64 = 0x1FF;
/// The delayed bid, ask, last, open, close and last time.
const DONE_DELAYED: u64 = 0x3F0_0000;
/// The same with the four delayed option computations.
const DONE_DELAYED_OPTION: u64 = 0x3FF0_0000;

fn sec_type_in(sec_type: &str, set: &[&str]) -> bool {
    set.iter().any(|s| s.eq_ignore_ascii_case(sec_type))
}

/// Security types with option computations in the snapshot end.
fn is_option(sec_type: &str) -> bool {
    sec_type_in(sec_type, &["OPT", "FOP", "WAR", "IOPT"])
}

/// The tick types a snapshot of this security type never waits for, and
/// never sends: the open where the type has none, the volume and the last
/// size where it has no volume, the last where it has no last price.
pub fn premarked(sec_type: &str) -> u64 {
    let mut bits = 0;
    if !sec_type_in(sec_type, &["STK", "FUT", "CMDTY", "CRYPTO"]) {
        bits |= tick_bit(14) | tick_bit(76);
    }
    if !sec_type_in(sec_type, &["STK", "CFD", "OPT", "FOP", "WAR", "IOPT", "FUT", "FWD", "BOND", "BILL", "SLB", "CRYPTO"]) {
        bits |= tick_bit(8) | tick_bit(74);
    }
    if !sec_type_in(sec_type, &[
        "STK", "CFD", "OPT", "FOP", "WAR", "IOPT", "FUT", "FWD", "BAG", "IND", "BOND", "BILL", "SLB", "FUND",
        "CRYPTO", "PDC",
    ]) {
        bits |= tick_bit(4) | tick_bit(84) | tick_bit(68);
    }
    bits
}

/// A running plain snapshot.
#[derive(Debug, Clone)]
pub struct PlainSnapshot {
    pub started: Instant,
    /// The tick types sent (or never to be sent), as `tick_bit`s.
    pub sent: u64,
    option: bool,
}

impl PlainSnapshot {
    pub fn new(sec_type: &str, started: Instant) -> Self {
        Self { started, sent: premarked(sec_type), option: is_option(sec_type) }
    }

    /// Whether the tick type is to be sent now: the first time only, and
    /// never for a type without a bit. Marks it sent.
    pub fn take(&mut self, tick_type: i32) -> bool {
        let bit = tick_bit(tick_type);
        if bit == 0 || self.sent & bit != 0 {
            return false;
        }
        self.sent |= bit;
        true
    }

    /// Every tick type the end waits for was sent, in real-time or in
    /// delayed data.
    pub fn complete(&self) -> bool {
        let (done, delayed) = if self.option { (DONE_OPTION, DONE_DELAYED_OPTION) } else { (DONE, DONE_DELAYED) };
        self.sent & done == done || self.sent & delayed == delayed
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= TIMEOUT
    }
}

/// The generic tick ids a snapshot of this security type may name; a list
/// of them makes the snapshot refused. The news tick also depends on the
/// session's news service, which is taken as on.
fn generic_tick_legal(id: i32, sec_type: &str) -> bool {
    match id {
        100 | 101 | 104 | 105 | 106 | 165 | 225 | 233 | 236 | 258 | 47 | 292 | 293 | 294 | 295 | 318 | 375
        | 411 | 460 | 512 | 577 | 586 | 588 | 614 | 619 | 623 | 220 | 221 | 232 => true,
        456 | 59 => sec_type_in(sec_type, &["STK", "FUT", "OPT", "IND", "FOP", "CFD", "SLB"]),
        162 => sec_type_in(sec_type, &["IND"]),
        595 => sec_type_in(sec_type, &["STK", "CFD", "OPT", "FOP", "WAR", "IOPT", "FUT", "FWD", "BOND", "BILL", "SLB", "CRYPTO"]),
        _ => false,
    }
}

/// A snapshot with this generic tick list is refused: the list names at
/// least one tick and every one is a legal tick for the security type. A
/// list with one unknown or illegal tick is dropped as a whole, so the
/// snapshot goes on, as the reference's parser gives no list then. The
/// `mdoff` word is no tick; a tick may carry a `:` parameter.
pub fn generic_ticks_refused(list: &str, sec_type: &str) -> bool {
    let mut any = false;
    for token in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if token.eq_ignore_ascii_case("mdoff") {
            continue;
        }
        let id = token.split(':').next().unwrap_or("");
        match id.parse::<i32>() {
            Ok(id) if generic_tick_legal(id, sec_type) => any = true,
            _ => return false,
        }
    }
    any
}

/// The snapshot requests of the session in the current second, as the
/// reference counts them: per wall-clock second.
#[derive(Debug, Default)]
pub struct RateLimiter {
    second: i64,
    count: u32,
}

impl RateLimiter {
    /// One more snapshot at `now_secs` (seconds since the epoch), or no
    /// when `limit` were already asked in that second.
    pub fn allow(&mut self, now_secs: i64, limit: u32) -> bool {
        if self.second == 0 {
            self.second = now_secs;
        }
        if now_secs != self.second {
            self.second = now_secs;
            self.count = 1;
            return true;
        }
        if self.count >= limit {
            return false;
        }
        self.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_tick_type_once_and_sizes_never_alone() {
        let mut s = PlainSnapshot::new("STK", Instant::now());
        assert!(s.take(1));
        assert!(!s.take(1));
        for size in [0, 3, 5, 69, 70, 71] {
            assert!(!s.take(size), "size {size}");
        }
        assert!(s.take(8));
        assert!(!s.take(8));
        assert!(s.take(45) && s.take(32) && s.take(33) && s.take(84));
    }

    #[test]
    fn the_end_waits_for_bid_ask_last_open_close() {
        let mut s = PlainSnapshot::new("STK", Instant::now());
        for t in [1, 2, 4, 9] {
            s.take(t);
            assert!(!s.complete());
        }
        s.take(14);
        assert!(s.complete());
        // Delayed data: the delayed types and the delayed last time.
        let mut d = PlainSnapshot::new("STK", Instant::now());
        for t in [66, 67, 68, 75, 76] {
            d.take(t);
        }
        assert!(!d.complete());
        d.take(88);
        assert!(d.complete());
        // No open for an index, no last nor open for a currency pair.
        let mut ind = PlainSnapshot::new("IND", Instant::now());
        for t in [1, 2, 4, 9] {
            ind.take(t);
        }
        assert!(ind.complete());
        let mut cash = PlainSnapshot::new("CASH", Instant::now());
        for t in [1, 2, 9] {
            cash.take(t);
        }
        assert!(cash.complete());
        assert!(!cash.take(4) && !cash.take(8), "never sent for a currency pair");
        // An option waits for its four computations.
        let mut opt = PlainSnapshot::new("OPT", Instant::now());
        for t in [1, 2, 4, 9] {
            opt.take(t);
        }
        assert!(!opt.complete());
        for t in [10, 11, 12, 13] {
            opt.take(t);
        }
        assert!(opt.complete());
    }

    #[test]
    fn eleven_seconds_end_it() {
        let start = Instant::now();
        let s = PlainSnapshot::new("STK", start);
        assert!(!s.timed_out(start + Duration::from_millis(10_999)));
        assert!(s.timed_out(start + Duration::from_millis(11_000)));
    }

    #[test]
    fn generic_ticks_refuse_only_a_legal_list() {
        assert!(generic_ticks_refused("233", "STK"));
        assert!(generic_ticks_refused(" 100, 101,104 ,", "OPT"));
        assert!(generic_ticks_refused("mdoff,233:x", "STK"));
        assert!(!generic_ticks_refused("", "STK"));
        assert!(!generic_ticks_refused("mdoff", "STK"));
        // One unknown or illegal id drops the whole list: no refusal.
        assert!(!generic_ticks_refused("233,13", "STK"));
        assert!(!generic_ticks_refused("abc", "STK"));
        assert!(!generic_ticks_refused("162", "STK"));
        assert!(generic_ticks_refused("162", "IND"));
        assert!(!generic_ticks_refused("456", "CASH"));
        assert!(generic_ticks_refused("456", "STK"));
        assert!(!generic_ticks_refused("595", "IND"));
    }

    #[test]
    fn the_limit_counts_per_second() {
        let mut r = RateLimiter::default();
        assert!(r.allow(1_000, 2));
        assert!(r.allow(1_000, 2));
        assert!(!r.allow(1_000, 2));
        assert!(r.allow(1_001, 2));
        assert!(r.allow(1_001, 2));
        assert!(!r.allow(1_001, 2));
        assert_eq!(rate_refusal(100),
            "Error validating request.-'bQ' : cause - Snapshot requests limitation exceeded:100 per 1 second(s)");
    }
}
