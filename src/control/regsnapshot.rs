//! Regulatory snapshot of reqMktData (ibx#446): the reference's snapshot
//! fetcher. A snapshot request to the farm, then four steps with their own
//! time limits: the request acknowledgement (5 s), the snapshot permission,
//! the fields (7 s), the exchange map of the BBO exchange (2 s); then one
//! batch of ticks and the snapshot end. No retries.

use std::time::{Duration, Instant};

/// Wait for the acknowledgement that carries the permission.
pub const ACK_TIMEOUT: Duration = Duration::from_millis(5000);
/// Wait for the fields.
pub const FIELDS_TIMEOUT: Duration = Duration::from_millis(7000);
/// Wait for the exchange map of the BBO exchange.
pub const EXCHANGE_MAP_TIMEOUT: Duration = Duration::from_millis(2000);

/// Error of a step that failed in the reference's fetcher with no API code
/// of its own: the largest int as code.
pub const INTERNAL_ERROR: (i64, &str) = (2147483647, "Internal server error");

/// Snapshot permissions of the acknowledgement.
pub const PERM_SNAPSHOT: i32 = 2;
pub const PERM_REALTIME_TOP: i32 = 3;
pub const PERM_SNAPSHOT_NO_API: i32 = 4;

/// Fields of the snapshot, `None` when not read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotFields {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub last: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub last_size: Option<f64>,
    pub bid_exchange: Option<String>,
    pub ask_exchange: Option<String>,
    pub last_exchange: Option<String>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: Option<f64>,
    pub volume: Option<f64>,
    /// Time of the snapshot (only with the snapshot permission).
    pub last_snapshot_time: Option<i64>,
}

impl SnapshotFields {
    fn read_count(&self) -> usize {
        [self.bid, self.ask, self.last, self.bid_size, self.ask_size, self.last_size,
         self.high, self.low, self.close, self.volume].iter().filter(|v| v.is_some()).count()
            + [&self.bid_exchange, &self.ask_exchange, &self.last_exchange].iter().filter(|v| v.is_some()).count()
    }

    fn all_read(&self) -> bool {
        self.read_count() == 13
    }
}

/// One message of the answer batch.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotTick {
    Price { tick_type: i32, price: f64 },
    Size { tick_type: i32, size: f64 },
    Text { tick_type: i32, value: String },
}

/// The answer batch, in the reference's field order, then the end.
pub fn answer_ticks(f: &SnapshotFields) -> Vec<SnapshotTick> {
    let mut out = Vec::new();
    // A price carries its size; a size not read is 1, as the reference.
    let mut price_with_size = |price: Option<f64>, size: Option<f64>, pt: i32, st: i32| {
        if let Some(p) = price {
            out.push(SnapshotTick::Price { tick_type: pt, price: p });
            out.push(SnapshotTick::Size { tick_type: st, size: size.unwrap_or(1.0) });
        }
    };
    price_with_size(f.ask, f.ask_size, 2, 3);
    price_with_size(f.bid, f.bid_size, 1, 0);
    price_with_size(f.last, f.last_size, 4, 5);
    for (v, tt) in [(&f.ask_exchange, 33), (&f.bid_exchange, 32), (&f.last_exchange, 84)] {
        if let Some(s) = v.as_ref().filter(|s| !s.is_empty()) {
            out.push(SnapshotTick::Text { tick_type: tt, value: s.clone() });
        }
    }
    for (v, tt) in [(f.high, 6), (f.low, 7), (f.close, 9)] {
        if let Some(p) = v {
            out.push(SnapshotTick::Price { tick_type: tt, price: p });
        }
    }
    if let Some(v) = f.volume {
        out.push(SnapshotTick::Size { tick_type: 8, size: v });
    }
    if let Some(t) = f.last_snapshot_time {
        out.push(SnapshotTick::Text { tick_type: 85, value: t.to_string() });
    }
    out
}

/// What the fetcher does after one look at its inputs.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait,
    /// Ends the fetch with an error for the request.
    Fail(i64, String),
    /// Ends the fetch with the ticks, then the snapshot end.
    Deliver(Vec<SnapshotTick>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Ack,
    Fields { since: Instant },
    ExchangeMap { since: Instant },
}

/// One running regulatory snapshot.
#[derive(Debug, Clone)]
pub struct Fetch {
    pub req_id: i64,
    pub con_id: i64,
    pub instrument: u32,
    /// Name of the request in the error texts.
    pub description: String,
    started: Instant,
    stage: Stage,
    permission: i32,
    fields: SnapshotFields,
}

impl Fetch {
    pub fn new(req_id: i64, con_id: i64, instrument: u32, description: String, now: Instant) -> Fetch {
        Fetch { req_id, con_id, instrument, description, started: now, stage: Stage::Ack, permission: 0, fields: SnapshotFields::default() }
    }

    /// One step: `ack` is the permission and BBO exchange code of the
    /// acknowledgement once received; `fields` what the record holds now;
    /// `exchange_map` whether the exchange map of the BBO exchange is known.
    pub fn poll(&mut self, now: Instant, ack: Option<(i32, &str)>, fields: &SnapshotFields, exchange_map: bool) -> Step {
        loop {
            match self.stage {
                Stage::Ack => {
                    let Some((permission, bbo)) = ack else {
                        if now.duration_since(self.started) >= ACK_TIMEOUT {
                            return Step::Fail(INTERNAL_ERROR.0, INTERNAL_ERROR.1.into());
                        }
                        return Step::Wait;
                    };
                    match permission {
                        PERM_SNAPSHOT | PERM_REALTIME_TOP => {}
                        PERM_SNAPSHOT_NO_API => {
                            return Step::Fail(10213, format!("API access is restricted on Regulatory snapshot for {}", self.description));
                        }
                        _ => return Step::Fail(10170, format!("No permissions on Regulatory snapshot for {}", self.description)),
                    }
                    self.permission = permission;
                    let _ = bbo;
                    self.stage = Stage::Fields { since: now };
                }
                Stage::Fields { since } => {
                    self.fields = fields.clone();
                    let done = match self.permission {
                        PERM_SNAPSHOT => self.fields.last_snapshot_time.is_some(),
                        _ => self.fields.all_read(),
                    };
                    if !done {
                        if now.duration_since(since) < FIELDS_TIMEOUT {
                            return Step::Wait;
                        }
                        if self.fields.read_count() == 0 && self.fields.last_snapshot_time.is_none() {
                            return Step::Fail(INTERNAL_ERROR.0, INTERNAL_ERROR.1.into());
                        }
                        log::warn!("Regulatory snapshot req_id={}: unable to fully fetch snapshot", self.req_id);
                    }
                    let bbo_valid = ack.map(|(_, b)| !b.is_empty() && b != "ffffffff").unwrap_or(false);
                    if !bbo_valid {
                        return Step::Fail(INTERNAL_ERROR.0, INTERNAL_ERROR.1.into());
                    }
                    self.stage = Stage::ExchangeMap { since: now };
                }
                Stage::ExchangeMap { since } => {
                    if exchange_map {
                        return Step::Deliver(answer_ticks(&self.fields));
                    }
                    if now.duration_since(since) >= EXCHANGE_MAP_TIMEOUT {
                        return Step::Fail(10171, "Unable to fetch %: regulatory snapshot".into());
                    }
                    return Step::Wait;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> SnapshotFields {
        SnapshotFields {
            bid: Some(1.0), ask: Some(1.1), last: Some(1.05), bid_size: Some(5.0), ask_size: Some(6.0),
            last_size: Some(7.0), bid_exchange: Some("Q".into()), ask_exchange: Some("P".into()),
            last_exchange: Some("Z".into()), high: Some(2.0), low: Some(0.5), close: Some(1.0),
            volume: Some(100.0), last_snapshot_time: None,
        }
    }

    #[test]
    fn realtime_permission_delivers_when_all_fields_are_read() {
        let t0 = Instant::now();
        let mut f = Fetch::new(1, 265598, 3, "AAPL".into(), t0);
        assert_eq!(f.poll(t0, None, &SnapshotFields::default(), true), Step::Wait);
        assert_eq!(f.poll(t0, Some((3, "9c")), &SnapshotFields::default(), true), Step::Wait);
        let Step::Deliver(ticks) = f.poll(t0, Some((3, "9c")), &full(), true) else { panic!() };
        assert_eq!(ticks[0], SnapshotTick::Price { tick_type: 2, price: 1.1 });
        assert_eq!(ticks[1], SnapshotTick::Size { tick_type: 3, size: 6.0 });
        assert!(ticks.contains(&SnapshotTick::Text { tick_type: 32, value: "Q".into() }));
        assert_eq!(ticks.last(), Some(&SnapshotTick::Size { tick_type: 8, size: 100.0 }));
    }

    #[test]
    fn no_ack_in_5_seconds_is_an_internal_error() {
        let t0 = Instant::now();
        let mut f = Fetch::new(1, 1, 1, "X".into(), t0);
        assert_eq!(f.poll(t0 + ACK_TIMEOUT, None, &full(), true), Step::Fail(2147483647, "Internal server error".into()));
    }

    #[test]
    fn permissions_refuse_with_their_codes() {
        let t0 = Instant::now();
        for (p, code) in [(0, 10170), (1, 10170), (4, 10213)] {
            let mut f = Fetch::new(1, 1, 1, "AAPL".into(), t0);
            let Step::Fail(c, text) = f.poll(t0, Some((p, "9c")), &full(), true) else { panic!() };
            assert_eq!(c, code);
            assert!(text.ends_with("Regulatory snapshot for AAPL"), "{text}");
        }
    }

    #[test]
    fn partial_fields_go_out_after_7_seconds_and_none_is_an_error() {
        let t0 = Instant::now();
        let partial = SnapshotFields { bid: Some(1.0), ..Default::default() };
        let mut f = Fetch::new(1, 1, 1, "X".into(), t0);
        assert_eq!(f.poll(t0, Some((2, "9c")), &partial, true), Step::Wait);
        let Step::Deliver(ticks) = f.poll(t0 + FIELDS_TIMEOUT, Some((2, "9c")), &partial, true) else { panic!() };
        assert_eq!(ticks, vec![SnapshotTick::Price { tick_type: 1, price: 1.0 }, SnapshotTick::Size { tick_type: 0, size: 1.0 }]);
        let mut f = Fetch::new(1, 1, 1, "X".into(), t0);
        assert_eq!(f.poll(t0, Some((2, "9c")), &SnapshotFields::default(), true), Step::Wait);
        assert!(matches!(f.poll(t0 + FIELDS_TIMEOUT, Some((2, "9c")), &SnapshotFields::default(), true), Step::Fail(2147483647, _)));
    }

    #[test]
    fn bbo_exchange_and_exchange_map() {
        let t0 = Instant::now();
        let mut f = Fetch::new(1, 1, 1, "X".into(), t0);
        assert!(matches!(f.poll(t0, Some((3, "ffffffff")), &full(), true), Step::Fail(2147483647, _)));
        let mut f = Fetch::new(1, 1, 1, "X".into(), t0);
        assert_eq!(f.poll(t0, Some((3, "9c")), &full(), false), Step::Wait);
        assert_eq!(f.poll(t0 + EXCHANGE_MAP_TIMEOUT, Some((3, "9c")), &full(), false),
            Step::Fail(10171, "Unable to fetch %: regulatory snapshot".into()));
    }
}
