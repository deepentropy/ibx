//! The market data callbacks of the API client, step by step (ibx#446).
//!
//! The engine queues each step of each farm message (`md_events`); the
//! dispatch reads them in their order and gives each request of the
//! instrument the callbacks the reference sends for that step, with that
//! step's values, however many messages came since the last dispatch:
//!
//! - a stream sends what changed since it last sent it (a value is sent
//!   when it changes; sizes and volume start empty, so a first 0 is sent; a
//!   price goes out with its size, and a changed size goes out again on its
//!   own), in the fixed order of the reference's sender: bid, ask, last
//!   (each with its size), bid size, ask size, last size, volume, high, low,
//!   close, open, bid and ask exchanges, last time, halted
//!   (`jextend.dL.a(List,s,int,pa,dy,o,SnapshotPreference,Set)`; the
//!   delayed sender has the volume after the low and no exchanges). The
//!   halted state is sent when its halted or volatility-halted bit changes,
//!   then in each step until the next book update. The last exchange is
//!   never sent;
//! - a plain snapshot sends each tick type once, from the steps that set
//!   it, then its end;
//! - a request that joins a quote with data gets all of it in one step
//!   first, at its place in the queue.
//!
//! A request has its place in the queue: it gets the steps written after
//! it was made (a request that joins once the engine found its contract,
//! from that moment), and, once cancelled, still the steps written before
//! the cancel, as the reference had sent them to the API socket already.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::api::types::PRICE_SCALE_F;
use crate::bridge::SharedState;
use crate::control::snapshot::PlainSnapshot;
use crate::md_events::{field, MdEvent, MdStep};
use crate::types::*;

use super::*;

/// One market data callback of the dispatch, in the order they go out.
#[derive(Debug, Clone, PartialEq)]
pub enum MdOut {
    Tick(i64, MdTick),
    /// `market_data_type`, before the first tick of a request.
    MarketDataType(i64, i32),
    /// `tick_snapshot_end`; the request is over.
    SnapshotEnd(i64),
}

/// What a request sent last and where it reads the queue (ibx#446).
#[derive(Debug, Default, Clone)]
pub struct StreamState {
    fields: [i64; field::COUNT],
    /// The sizes and volume sent once (`SizeKind` bits).
    sizes_sent: u8,
    /// The halted and volatility-halted bits of the last trade status.
    halted_bits: u8,
    /// The halted state goes out in each step until the next book update.
    halted_pending: bool,
    /// Steps before this queue position are not the request's.
    start: u64,
    /// The request joined a quote: its first step sends all of it, once
    /// its place in the queue is reached.
    waiting_join: bool,
    /// The snapshot ended: nothing more.
    ended: bool,
    /// Its market data type was looked at, at its first tick.
    pub(crate) mdt_done: bool,
    /// Delayed data: delayed tick types, the delayed sender's order.
    pub(crate) delayed: bool,
    /// The bid and ask auto-execution comes from the farm.
    farm_auto: bool,
    /// `mdoff`: no top of book ticks.
    top_off: bool,
}

/// What a new request is.
pub(crate) struct NewRequest<'a> {
    /// A plain snapshot, of this security type.
    pub snapshot: Option<&'a str>,
    pub top_off: bool,
    pub farm_auto: bool,
}

/// An instrument as the queued steps have it: the reference's record at
/// the step being read.
#[derive(Debug, Clone, Copy)]
struct Mirror {
    fields: [i64; field::COUNT],
    halted: Option<i64>,
    auto: QuoteMarks,
    /// Sizes given once (`SizeKind` bits).
    seen: u8,
    /// False while the catch-up of a quote taken over with data is awaited.
    valid: bool,
    /// A new subscription of the instrument from this queue position: the
    /// record starts empty there (valid or not), after the steps of the
    /// earlier one.
    reset_at: Option<(u64, bool)>,
}

impl Default for Mirror {
    fn default() -> Self {
        Self { fields: [0; field::COUNT], halted: None, auto: QuoteMarks::default(), seen: 0, valid: true, reset_at: None }
    }
}

/// The size kind of a size field.
fn size_kind(idx: usize) -> Option<SizeKind> {
    match idx {
        field::BID_SIZE => Some(SizeKind::Bid),
        field::ASK_SIZE => Some(SizeKind::Ask),
        field::LAST_SIZE => Some(SizeKind::Last),
        field::VOLUME => Some(SizeKind::Volume),
        _ => None,
    }
}

impl Mirror {
    fn apply(&mut self, ev: &MdEvent) {
        let mut set = ev.fields;
        while set != 0 {
            let i = set.trailing_zeros() as usize;
            set &= set - 1;
            self.fields[i] = ev.values[i];
            // A catch-up says which sizes came.
            if ev.step != MdStep::CatchUp && let Some(kind) = size_kind(i) {
                self.seen |= 1 << kind as u8;
            }
        }
        if ev.step == MdStep::CatchUp {
            self.seen = ev.seen;
        }
        if let Some(h) = ev.halted {
            self.halted = Some(h as i64);
        }
        let given = QuoteMarks::from_auto_word(ev.auto);
        if let Some(on) = given.bid_auto() {
            self.auto.set_bid_auto(on);
        }
        if let Some(on) = given.ask_auto() {
            self.auto.set_ask_auto(on);
        }
        if ev.step == MdStep::CatchUp {
            self.valid = true;
        }
    }

    fn seen(&self, kind: SizeKind) -> bool {
        self.seen & (1 << kind as u8) != 0
    }

    /// The new subscription's empty record, once the steps before its
    /// place are read. True when done.
    fn reset_if_due(&mut self, seq: u64) -> bool {
        match self.reset_at {
            Some((at, valid)) if seq >= at => {
                *self = Mirror { valid, ..Mirror::default() };
                true
            }
            _ => false,
        }
    }
}

/// A cancelled request: the steps before its cancel are still its own.
struct Retired {
    req_id: i64,
    instrument: InstrumentId,
    /// The queue position at the cancel.
    end: u64,
    st: StreamState,
    snap: Option<PlainSnapshot>,
}

/// The reader's side of the queue: the instruments' records, and the
/// requests cancelled with steps still to read.
pub(crate) struct MdReader {
    mirrors: Vec<Mirror>,
    /// Records with a reset to make.
    resets: usize,
    retired: Vec<Retired>,
}

impl MdReader {
    pub(crate) fn new() -> Self {
        Self { mirrors: vec![Mirror::default(); MAX_INSTRUMENTS], resets: 0, retired: Vec::new() }
    }
}

/// The fields of a stream step, in the order the reference sends them.
const STEP_ALL: [usize; 14] = [
    field::BID, field::ASK, field::LAST, field::BID_SIZE, field::ASK_SIZE, field::LAST_SIZE, field::VOLUME,
    field::HIGH, field::LOW, field::CLOSE, field::OPEN, field::BID_EXCH, field::ASK_EXCH, field::TIME,
];
/// The delayed data sender's order: the volume after the low; it sends no
/// exchanges (skipped for delayed data).
const STEP_ALL_DELAYED: [usize; 14] = [
    field::BID, field::ASK, field::LAST, field::BID_SIZE, field::ASK_SIZE, field::LAST_SIZE, field::HIGH,
    field::LOW, field::VOLUME, field::CLOSE, field::OPEN, field::BID_EXCH, field::ASK_EXCH, field::TIME,
];

/// Which step a request is given.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Its first step after it joined a quote: all of it.
    Join,
    /// A queued step.
    Step(MdStep, u16),
}

/// The callbacks of one step of one request being written.
struct Out<'a> {
    out: &'a mut Vec<MdOut>,
    req_id: i64,
    fields: &'a [i64; field::COUNT],
    delayed: bool,
    shared: &'a SharedState,
    instrument: InstrumentId,
    /// The queue position of the step.
    seq: u64,
}

impl Out<'_> {
    fn api(&self, tick_type: i32) -> i32 {
        if self.delayed { delayed_tick_type(tick_type) } else { tick_type }
    }

    fn push(&mut self, tick: MdTick) {
        self.out.push(MdOut::Tick(self.req_id, tick));
    }

    fn price(&mut self, tick_type: i32, idx: usize, can_auto_execute: bool) {
        let tick_type = self.api(tick_type);
        self.push(MdTick::Price { tick_type, value: self.fields[idx] as f64 / PRICE_SCALE_F, can_auto_execute });
    }

    fn size(&mut self, tick_type: i32, idx: usize) {
        let tick_type = self.api(tick_type);
        self.push(MdTick::Size { tick_type, value: self.fields[idx] as f64 / QTY_SCALE as f64 });
    }

    fn time(&mut self) {
        let tick_type = self.api(TICK_LAST_TIMESTAMP);
        self.push(MdTick::Time { tick_type, secs: self.fields[field::TIME] / 1_000_000_000 });
    }

    /// An exchange text; an empty one is not sent (`jextend.dK.c(List,int,
    /// int,String)@0-14`): no letters before the exchange map came.
    fn exchanges(&mut self, idx: usize) {
        let tick_type = if idx == field::BID_EXCH { TICK_BID_EXCHANGE } else { TICK_ASK_EXCHANGE };
        let value = render_exchange_mask_at(self.fields[idx], self.instrument, self.shared, self.seq);
        if !value.is_empty() {
            self.push(MdTick::Text { tick_type, value });
        }
    }

    fn halted(&mut self, status: i64) {
        let tick_type = self.api(TICK_HALTED);
        self.push(MdTick::Generic { tick_type, value: QuoteMarks::halted_tick_value(status) });
    }
}

/// A stream's step: the fields that changed, in the sender's order, then
/// the halted state while it is pending.
fn stream_pass(st: &mut StreamState, m: &Mirror, pass: Pass, out: &mut Out) {
    let fields = &m.fields;
    let mut todo = [false; field::COUNT];
    for (idx, pending) in todo.iter_mut().enumerate() {
        *pending = match idx {
            field::BID_SIZE | field::ASK_SIZE | field::LAST_SIZE | field::VOLUME => {
                let kind = size_kind(idx).unwrap_or(SizeKind::Volume);
                (m.seen(kind) || fields[idx] != 0)
                    && (st.sizes_sent & (1 << kind as u8) == 0 || fields[idx] != st.fields[idx])
            }
            field::BID_EXCH | field::ASK_EXCH => fields[idx] != st.fields[idx],
            field::LAST_EXCH => false,
            _ => fields[idx] != 0 && fields[idx] != st.fields[idx],
        };
    }
    if let Some(status) = m.halted {
        let bits = (status & 3) as u8;
        if bits != st.halted_bits {
            st.halted_bits = bits;
            st.halted_pending = true;
        }
    }
    let delayed = st.delayed;
    let (bid_auto, ask_auto) = if delayed {
        (false, false)
    } else if st.farm_auto {
        (m.auto.bid_auto() == Some(true), m.auto.ask_auto() == Some(true))
    } else {
        (true, true)
    };
    for &idx in if delayed { &STEP_ALL_DELAYED } else { &STEP_ALL } {
        if !todo[idx] {
            continue;
        }
        match idx {
            // A price comes with its size.
            field::BID => { out.price(TICK_BID, idx, bid_auto); out.size(TICK_BID_SIZE, field::BID_SIZE); }
            field::ASK => { out.price(TICK_ASK, idx, ask_auto); out.size(TICK_ASK_SIZE, field::ASK_SIZE); }
            field::LAST => { out.price(TICK_LAST, idx, false); out.size(TICK_LAST_SIZE, field::LAST_SIZE); }
            field::BID_SIZE => out.size(TICK_BID_SIZE, idx),
            field::ASK_SIZE => out.size(TICK_ASK_SIZE, idx),
            field::LAST_SIZE => out.size(TICK_LAST_SIZE, idx),
            field::VOLUME => out.size(TICK_VOLUME, idx),
            field::HIGH => out.price(TICK_HIGH, idx, false),
            field::LOW => out.price(TICK_LOW, idx, false),
            field::CLOSE => out.price(TICK_CLOSE, idx, false),
            field::OPEN => out.price(TICK_OPEN, idx, false),
            field::TIME => out.time(),
            // The delayed sender sends no exchanges.
            field::BID_EXCH | field::ASK_EXCH if delayed => {}
            field::BID_EXCH | field::ASK_EXCH => out.exchanges(idx),
            _ => {}
        }
    }
    // A request that joins gets the halted state when one is known (the
    // reference marks it when the request joins the record).
    if pass == Pass::Join {
        st.halted_pending = m.halted.is_some();
    }
    if st.halted_pending && let Some(status) = m.halted {
        out.halted(status);
    }
    // A book update clears every mark.
    if matches!(pass, Pass::Step(MdStep::Quote | MdStep::CatchUp, _)) {
        st.halted_pending = false;
    }
    st.fields = *fields;
    for idx in [field::BID_SIZE, field::ASK_SIZE, field::LAST_SIZE, field::VOLUME] {
        if let Some(kind) = size_kind(idx)
            && (m.seen(kind) || fields[idx] != 0)
        {
            st.sizes_sent |= 1 << kind as u8;
        }
    }
}

/// A plain snapshot's step: each tick type once, from the fields the step
/// set (all of them when the request joins or catches up), in the sender's
/// order; the halted state from a trade step, from the first status known,
/// as the reference's snapshot record marks it. A snapshot's bid or ask
/// executes automatically only when the farm said so. The delayed sender
/// sends no exchanges and no halted state.
fn snapshot_pass(snap: &mut PlainSnapshot, m: &Mirror, pass: Pass, out: &mut Out) {
    let (set, trade) = match pass {
        Pass::Join | Pass::Step(MdStep::CatchUp, _) => (field::ALL, true),
        Pass::Step(step, set) => (set, step == MdStep::Last),
    };
    let delayed = out.delayed;
    let f = m.fields;
    let has = |idx: usize| set & (1 << idx) != 0 && f[idx] != 0;
    for (idx, size, pt, st, auto) in [
        (field::BID, field::BID_SIZE, TICK_BID, TICK_BID_SIZE, m.auto.bid_auto()),
        (field::ASK, field::ASK_SIZE, TICK_ASK, TICK_ASK_SIZE, m.auto.ask_auto()),
    ] {
        if has(idx) && snap.take(out.api(pt)) {
            out.price(pt, idx, !delayed && auto == Some(true));
            out.size(st, size);
        }
    }
    if has(field::LAST) && snap.take(out.api(TICK_LAST)) {
        out.price(TICK_LAST, field::LAST, false);
        out.size(TICK_LAST_SIZE, field::LAST_SIZE);
    }
    let volume = has(field::VOLUME) && snap.take(out.api(TICK_VOLUME));
    if volume && !delayed {
        out.size(TICK_VOLUME, field::VOLUME);
    }
    for (idx, tt) in [(field::HIGH, TICK_HIGH), (field::LOW, TICK_LOW)] {
        if has(idx) && snap.take(out.api(tt)) {
            out.price(tt, idx, false);
        }
    }
    if volume && delayed {
        out.size(TICK_VOLUME, field::VOLUME);
    }
    for (idx, tt) in [(field::CLOSE, TICK_CLOSE), (field::OPEN, TICK_OPEN)] {
        if has(idx) && snap.take(out.api(tt)) {
            out.price(tt, idx, false);
        }
    }
    if !delayed {
        for (idx, tt) in [(field::BID_EXCH, TICK_BID_EXCHANGE), (field::ASK_EXCH, TICK_ASK_EXCHANGE)] {
            if has(idx) && snap.take(tt) {
                out.exchanges(idx);
            }
        }
    }
    if has(field::TIME) && snap.take(out.api(TICK_LAST_TIMESTAMP)) {
        out.time();
    }
    if trade && !delayed && let Some(status) = m.halted && snap.take(TICK_HALTED) {
        out.halted(status);
    }
}

impl ClientCore {
    /// Set up the stream state of a new request and, for the first request
    /// of an instrument, the instrument's record and its steps (ibx#446).
    /// The request reads the queue from its current end, or from `at`. A
    /// request that joins (another request's quote, or the P&L quote,
    /// `had_data`) gets all of the quote first; the P&L quote's comes from
    /// a catch-up the engine writes, as no step of it was queued. Called
    /// with the instrument's requests locked.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_md_stream(
        &self, shared: &SharedState, req_id: i64, instrument: InstrumentId, joining: bool, had_data: bool,
        kind: Option<NewRequest>, at: Option<u64>,
    ) {
        let queue = &shared.market.md_events;
        let head = at.unwrap_or_else(|| queue.head());
        if !joining {
            let mut reader = self.md_reader.lock().unwrap();
            let MdReader { mirrors, resets, .. } = &mut *reader;
            if let Some(m) = mirrors.get_mut(instrument as usize) {
                if m.reset_at.is_none() {
                    *resets += 1;
                }
                m.reset_at = Some((head, !had_data));
            }
            drop(reader);
            queue.listen(instrument, true);
            if had_data {
                queue.request_catch_up(instrument);
            }
        }
        if let Some(sec_type) = kind.as_ref().and_then(|k| k.snapshot) {
            let mut snaps = self.snapshot_reqs.lock().unwrap();
            snaps.insert(req_id, PlainSnapshot::new(sec_type, std::time::Instant::now()));
            self.snapshot_count.store(snaps.len(), Ordering::Release);
        }
        let delayed = self.delayed_reqs.lock().unwrap().contains(&req_id);
        let mut streams = self.last_quotes.lock().unwrap();
        let old = streams.remove(&req_id).unwrap_or_default();
        if old.waiting_join {
            self.md_joins_waiting.fetch_sub(1, Ordering::AcqRel);
        }
        let waiting_join = joining || had_data;
        if waiting_join {
            self.md_joins_waiting.fetch_add(1, Ordering::AcqRel);
        }
        let (top_off, farm_auto) = kind.map_or((old.top_off, old.farm_auto), |k| (k.top_off, k.farm_auto));
        streams.insert(req_id, StreamState {
            start: head, waiting_join, delayed, top_off, farm_auto, mdt_done: old.mdt_done, ..StreamState::default()
        });
    }

    /// A request joins a quote that already has data, from the current end
    /// of the queue (tests).
    #[cfg(test)]
    pub(crate) fn join_stream(&self, req_id: i64, head: u64) {
        let mut streams = self.last_quotes.lock().unwrap();
        let st = streams.entry(req_id).or_default();
        if !st.waiting_join {
            self.md_joins_waiting.fetch_add(1, Ordering::AcqRel);
        }
        st.waiting_join = true;
        st.start = head;
    }

    /// A request ends. Cancelled by the client (`cancelled`), the steps
    /// queued until now are still its own (its snapshot too). Called with
    /// the instrument's requests locked.
    pub(super) fn end_md_stream(&self, shared: &SharedState, req_id: i64, instrument: InstrumentId, cancelled: bool) {
        let st = self.last_quotes.lock().unwrap().remove(&req_id);
        let snap = if self.snapshot_count.load(Ordering::Acquire) > 0 {
            let mut snaps = self.snapshot_reqs.lock().unwrap();
            let snap = snaps.remove(&req_id);
            self.snapshot_count.store(snaps.len(), Ordering::Release);
            snap
        } else {
            None
        };
        let Some(st) = st else { return };
        if st.waiting_join {
            self.md_joins_waiting.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        let end = shared.market.md_events.head();
        if cancelled && !st.ended && end > shared.market.md_events.tail() {
            self.md_reader.lock().unwrap().retired.push(Retired { req_id, instrument, end, st, snap });
        }
    }

    /// The market data callbacks of the steps queued since the last call,
    /// in their order, for every request (see the module), and the ends of
    /// the plain snapshots that completed or reached their time limit
    /// (11 s, at `now`, the current time when None). A request with
    /// `mdoff` gets no top of book tick; its snapshot still ends as the
    /// quote completes (ibx#444).
    pub fn poll_market_data(&self, shared: &SharedState, now: Option<std::time::Instant>, out: &mut Vec<MdOut>) {
        let queue = &shared.market.md_events;
        let head = queue.head();
        let tail = queue.tail();
        let snapshots = self.snapshot_count.load(Ordering::Acquire) > 0;
        if head == tail && !snapshots && self.md_joins_waiting.load(Ordering::Acquire) == 0 {
            return;
        }
        let observers = self.instrument_to_req.lock().unwrap();
        let mut reader = self.md_reader.lock().unwrap();
        let mut streams = self.last_quotes.lock().unwrap();
        let mut snaps = self.snapshot_reqs.lock().unwrap();
        let MdReader { mirrors, resets, retired } = &mut *reader;
        let mut d = Drain { core: self, shared, out, streams: &mut streams, snaps: &mut snaps };
        let mut step = |seq: u64| {
            let ev = queue.read(seq);
            let instrument = ev.instrument;
            let Some(m) = mirrors.get_mut(instrument as usize) else { return };
            if m.reset_if_due(seq) {
                *resets -= 1;
            }
            // A new subscription's steps are its requests'; the steps of
            // an earlier one are only for the requests cancelled after them.
            let reqs = match m.reset_at {
                Some(_) => &[][..],
                None => observers.get(&instrument).map_or(&[][..], Vec::as_slice),
            };
            // Requests that joined before this step: all of the quote first.
            if m.valid && self.md_joins_waiting.load(Ordering::Relaxed) > 0 {
                for &req_id in reqs {
                    d.join_if_due(req_id, instrument, m, seq);
                }
            }
            m.apply(&ev);
            for &req_id in reqs {
                d.step(req_id, instrument, m, &ev, seq);
            }
            for r in retired.iter_mut().filter(|r| r.instrument == instrument && seq < r.end && seq >= r.st.start) {
                if !r.st.waiting_join {
                    d.retired_step(r, m, &ev, seq);
                }
            }
        };
        // Message by message: its book updates after its other steps, as
        // the reference applies them once it read the message.
        let mut start = tail;
        while start < head {
            let mut end = start + 1;
            let mut books = false;
            let mut others = false;
            for seq in start..head {
                let (kind, first) = queue.step_at(seq);
                if seq > start && first {
                    break;
                }
                end = seq + 1;
                if kind == MdStep::Quote { books = true } else { others = true }
            }
            if books && others {
                for quotes in [false, true] {
                    for seq in start..end {
                        if (queue.step_at(seq).0 == MdStep::Quote) == quotes {
                            step(seq);
                        }
                    }
                }
            } else {
                for seq in start..end {
                    step(seq);
                }
            }
            start = end;
        }
        queue.release(head);
        retired.clear();
        // New subscriptions with no step yet, and the requests that joined
        // after the last step.
        if *resets > 0 {
            for m in mirrors.iter_mut() {
                if m.reset_if_due(head) {
                    *resets -= 1;
                }
            }
        }
        if self.md_joins_waiting.load(Ordering::Acquire) > 0 {
            for (&instrument, reqs) in observers.iter() {
                let Some(m) = mirrors.get(instrument as usize).filter(|m| m.valid && m.reset_at.is_none()) else { continue };
                for &req_id in reqs {
                    d.join_if_due(req_id, instrument, m, head);
                }
            }
        }
        if snapshots {
            d.snapshot_timeouts(now.unwrap_or_else(std::time::Instant::now));
        }
    }
}

/// One step of one request: its callbacks, the market data type before its
/// first tick, and a snapshot's end once complete (true then).
#[allow(clippy::too_many_arguments)]
fn pass_one(
    core: &ClientCore, shared: &SharedState, out: &mut Vec<MdOut>, req_id: i64, st: &mut StreamState,
    snap: Option<&mut PlainSnapshot>, instrument: InstrumentId, m: &Mirror, pass: Pass, seq: u64,
) -> bool {
    if st.ended {
        return false;
    }
    let mark = out.len();
    let complete = {
        let mut o = Out { out: &mut *out, req_id, fields: &m.fields, delayed: st.delayed, shared, instrument, seq };
        match snap {
            Some(snap) => {
                snapshot_pass(snap, m, pass, &mut o);
                snap.complete()
            }
            None => {
                stream_pass(st, m, pass, &mut o);
                false
            }
        }
    };
    // `mdoff`: the reference's sender skips the top of book pass of the
    // request (`jextend.dL.a(s,int,pa,Map,Set)@136-220`).
    if st.top_off {
        out.truncate(mark);
    }
    if out.len() > mark && !st.mdt_done {
        st.mdt_done = true;
        if let Some(mdt) = core.check_mdt_needed(req_id, true) {
            out.insert(mark, MdOut::MarketDataType(req_id, mdt));
        }
    }
    if complete {
        st.ended = true;
        out.push(MdOut::SnapshotEnd(req_id));
    }
    complete
}

/// The dispatch reading the queue, with the request states locked.
struct Drain<'a> {
    core: &'a ClientCore,
    shared: &'a SharedState,
    out: &'a mut Vec<MdOut>,
    streams: &'a mut HashMap<i64, StreamState>,
    snaps: &'a mut HashMap<i64, PlainSnapshot>,
}

impl Drain<'_> {
    /// The join step of a request whose place in the queue is reached.
    fn join_if_due(&mut self, req_id: i64, instrument: InstrumentId, m: &Mirror, seq: u64) {
        let st = self.streams.entry(req_id).or_default();
        if st.waiting_join && seq >= st.start {
            st.waiting_join = false;
            self.core.md_joins_waiting.fetch_sub(1, Ordering::AcqRel);
            Self::pass(self.core, self.shared, self.out, self.snaps, req_id, st, instrument, m, Pass::Join, seq);
        }
    }

    /// A queued step for a request: its callbacks; a request waiting for
    /// the catch-up of a taken-over quote takes it as its join step.
    fn step(&mut self, req_id: i64, instrument: InstrumentId, m: &Mirror, ev: &MdEvent, seq: u64) {
        let st = self.streams.entry(req_id).or_default();
        if seq < st.start || st.ended {
            return;
        }
        let pass = if st.waiting_join {
            if !m.valid {
                return;
            }
            st.waiting_join = false;
            self.core.md_joins_waiting.fetch_sub(1, Ordering::AcqRel);
            Pass::Join
        } else {
            Pass::Step(ev.step, ev.fields)
        };
        Self::pass(self.core, self.shared, self.out, self.snaps, req_id, st, instrument, m, pass, seq);
    }

    /// One step of one request.
    #[allow(clippy::too_many_arguments)]
    fn pass(
        core: &ClientCore, shared: &SharedState, out: &mut Vec<MdOut>, snaps: &mut HashMap<i64, PlainSnapshot>,
        req_id: i64, st: &mut StreamState, instrument: InstrumentId, m: &Mirror, pass: Pass, seq: u64,
    ) {
        let snap = if snaps.is_empty() { None } else { snaps.get_mut(&req_id) };
        if pass_one(core, shared, out, req_id, st, snap, instrument, m, pass, seq) {
            snaps.remove(&req_id);
            core.snapshot_count.store(snaps.len(), Ordering::Release);
        }
    }

    /// A step for a cancelled request, written before its cancel.
    fn retired_step(&mut self, r: &mut Retired, m: &Mirror, ev: &MdEvent, seq: u64) {
        let complete = pass_one(
            self.core, self.shared, self.out, r.req_id, &mut r.st, r.snap.as_mut(), r.instrument, m,
            Pass::Step(ev.step, ev.fields), seq,
        );
        if complete {
            r.snap = None;
        }
    }

    /// The snapshots that reached their time limit end.
    fn snapshot_timeouts(&mut self, now: std::time::Instant) {
        let streams = &mut *self.streams;
        let out = &mut *self.out;
        self.snaps.retain(|&req_id, snap| {
            if !snap.timed_out(now) {
                return true;
            }
            if let Some(st) = streams.get_mut(&req_id) {
                st.ended = true;
            }
            out.push(MdOut::SnapshotEnd(req_id));
            false
        });
        self.core.snapshot_count.store(self.snaps.len(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md_events::TestMessage;

    /// The callbacks as text.
    fn lines(out: &[MdOut]) -> Vec<String> {
        out.iter().map(|o| match o {
            MdOut::Tick(r, MdTick::Price { tick_type, value, .. }) => format!("price:{r}:{tick_type}:{value}"),
            MdOut::Tick(r, MdTick::Size { tick_type, value }) => format!("size:{r}:{tick_type}:{value}"),
            MdOut::Tick(r, MdTick::Text { tick_type, value }) => format!("string:{r}:{tick_type}:{value}"),
            MdOut::Tick(r, MdTick::Time { tick_type, secs }) => format!("string:{r}:{tick_type}:{secs}"),
            MdOut::Tick(r, MdTick::Generic { tick_type, value }) => format!("generic:{r}:{tick_type}:{value}"),
            MdOut::MarketDataType(r, t) => format!("mdt:{r}:{t}"),
            MdOut::SnapshotEnd(r) => format!("end:{r}"),
        }).collect()
    }

    fn poll(core: &ClientCore, shared: &SharedState) -> Vec<String> {
        let mut out = Vec::new();
        core.poll_market_data(shared, None, &mut out);
        lines(&out)
    }

    /// A book update of instrument `id` with this bid (ask 101, sizes 1).
    fn book(shared: &SharedState, id: InstrumentId, bid: i64) {
        let q = Quote { bid: bid * PRICE_SCALE, ask: 101 * PRICE_SCALE, bid_size: QTY_SCALE, ask_size: QTY_SCALE, ..Default::default() };
        shared.market.push_test_message(id, &q, &TestMessage { steps: Some(&[MdStep::Quote]), ..Default::default() });
    }

    fn stream(core: &ClientCore, shared: &SharedState, req_id: i64, instrument: InstrumentId) {
        assert!(core.attach_md_request(shared, req_id, instrument, false, "STK", "SMART", None, false, false));
    }

    // ibx#446: the reference sent a request's ticks before its cancel: a
    // client that reads after the cancel still gets them, and nothing of
    // the steps after.
    #[test]
    fn a_cancelled_request_gets_the_steps_before_its_cancel() {
        let (core, shared) = (ClientCore::new(), SharedState::new());
        stream(&core, &shared, 1, 5);
        book(&shared, 5, 100);
        book(&shared, 5, 99);
        assert!(core.unregister_mkt_data(&shared, 1).is_some());
        assert!(!shared.market.md_events.listened(5), "no step of the contract is written any more");
        book(&shared, 5, 98);
        assert_eq!(poll(&core, &shared), [
            "mdt:1:1", "price:1:1:100", "size:1:0:1", "price:1:2:101", "size:1:3:1", "size:1:0:1", "size:1:3:1",
            "price:1:1:99", "size:1:0:1",
        ]);
        assert!(poll(&core, &shared).is_empty());
    }

    // ibx#446: a slot the engine gives to another contract: the new
    // request gets only the new contract's steps, from an empty record;
    // the earlier contract's steps go to its cancelled request.
    #[test]
    fn a_reused_slot_starts_empty_after_the_earlier_steps() {
        let (core, shared) = (ClientCore::new(), SharedState::new());
        stream(&core, &shared, 1, 5);
        let q = Quote { bid: 100 * PRICE_SCALE, high: 120 * PRICE_SCALE, ..Default::default() };
        shared.market.push_test_message(5, &q, &TestMessage::default());
        core.unregister_mkt_data(&shared, 1);
        stream(&core, &shared, 2, 5);
        book(&shared, 5, 50);
        assert_eq!(poll(&core, &shared), [
            "mdt:1:1", "price:1:6:120", "price:1:1:100", "size:1:0:0",
            "mdt:2:1", "price:2:1:50", "size:2:0:1", "price:2:2:101", "size:2:3:1", "size:2:0:1", "size:2:3:1",
        ]);
    }

    // ibx#446: a request that takes over the P&L quote (data, but no step
    // queued) waits for the engine's catch-up, the quote as the engine has
    // it, and gets all of it there in one step; the steps written before
    // the catch-up are not its own.
    #[test]
    fn a_request_on_the_pnl_quote_starts_from_a_catch_up() {
        let (core, shared) = (ClientCore::new(), SharedState::new());
        let (tx, _rx) = crossbeam_channel::unbounded();
        core.pnl_quotes.lock().unwrap().active.insert(756733, 5);
        core.register_mkt_data(&shared, &tx, 1, 756733, "SPY", "SMART", "STK", "USD", &Default::default(), false, "", 0)
            .unwrap();
        let queue = &shared.market.md_events;
        assert!(queue.listened(5) && queue.needs_catch_up(5));
        book(&shared, 5, 99);
        assert!(poll(&core, &shared).is_empty(), "waits for the catch-up");
        let q = Quote { bid: 100 * PRICE_SCALE, last: 100 * PRICE_SCALE, last_size: 2 * QTY_SCALE, ..Default::default() };
        let mut marks = QuoteMarks::default();
        marks.set_halted(0);
        queue.write_catch_ups(|id| MdEvent::catch_up(id, &q, marks));
        assert!(!queue.needs_catch_up(5));
        book(&shared, 5, 101);
        assert_eq!(poll(&core, &shared), [
            "mdt:1:1", "price:1:1:100", "size:1:0:0", "price:1:4:100", "size:1:5:2", "size:1:5:2", "generic:1:49:0",
            "price:1:1:101", "size:1:0:1", "price:1:2:101", "size:1:3:1", "size:1:0:1", "size:1:3:1", "generic:1:49:0",
        ]);
    }

    // ibx#444, ibx#446: a request without a conId joins the subscription of
    // the contract the engine found at the moment it found it: the quote as
    // it was then, then the later steps, even when the client reads later.
    #[test]
    fn a_symbol_only_request_joins_where_the_engine_found_its_contract() {
        let (core, shared) = (ClientCore::new(), SharedState::new());
        stream(&core, &shared, 1, 5);
        stream(&core, &shared, 2, 6);
        book(&shared, 5, 100);
        shared.market.push_md_merge(6, 5);
        book(&shared, 5, 99);
        let (notices, _) = core.take_md_rejects(&shared);
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(poll(&core, &shared), [
            "mdt:1:1", "price:1:1:100", "size:1:0:1", "price:1:2:101", "size:1:3:1", "size:1:0:1", "size:1:3:1",
            "mdt:2:1", "price:2:1:100", "size:2:0:1", "price:2:2:101", "size:2:3:1", "size:2:0:1", "size:2:3:1",
            "price:1:1:99", "size:1:0:1", "price:2:1:99", "size:2:0:1",
        ]);
    }
}
