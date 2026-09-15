//! What is quoted, and what the venue has said about it.

use super::*;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use crate::types::*;

/// How many broadcast notices are kept for a caller who has not asked for them
/// yet. The venue broadcasts these unasked and only a subscriber drains them,
/// so without a bound a session that never subscribes keeps every notice of the
/// day for the life of the process.
pub const NEWS_BULLETIN_LIMIT: usize = 1000;

/// How much of a stream is kept for a caller who has stopped reading it.
///
/// The same reasoning as the bulletins above, and it applies harder: these
/// arrive at market rate rather than a few times an hour, and this library
/// documents a way of reading them that never pumps the callback loop at all.
/// Unbounded, a book or a tick-by-tick stream grows the process until it dies.
/// Bounded, a caller that stopped reading loses the oldest of what it was not
/// reading, which is the lesser of the two.
pub const STREAM_BACKLOG_LIMIT: usize = 100_000;

/// This machine's clock, in unix milliseconds.
fn local_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// Push onto a stream that nobody may be draining, oldest out first.
pub(super) fn push_bounded<T>(queue: &Mutex<Vec<T>>, item: T, limit: usize, what: &str) {
    let mut held = queue.lock().unwrap();
    if held.len() >= limit {
        // A tenth at a time rather than one at a time: dropping a single entry
        // per push leaves every later push doing a full shift of the vector.
        let drop_to = limit - limit / 10;
        let shed = held.len() - drop_to;
        held.drain(..shed);
        log::warn!(
            "{what} has gone past {limit} unread, so the oldest of them were dropped — \
             nothing is draining this stream",
        );
    }
    held.push(item);
}

/// What each contract's numbered-figure tables hold: the slot, the series and
/// which of the two tables, against the figures the venue numbered in it.
type NumberedFiguresHeld =
    std::collections::HashMap<(crate::types::InstrumentId, u32, bool), Vec<(i32, f64)>>;

/// What each contract's stated rows hold: the slot and the series, against
/// the three figures of each row the venue stated.
type StatedRowsHeld =
    std::collections::HashMap<(crate::types::InstrumentId, u32), Vec<(f64, f64, f64)>>;

/// What each contract's paired-figure runs hold: the slot and the series,
/// against the pairs the venue stated in it.
type PairedFiguresHeld =
    std::collections::HashMap<(crate::types::InstrumentId, u32), Vec<(f64, f64)>>;

/// Lock-free quotes, TBT streams, real-time bars, depth updates, and news ticks.
pub struct MarketDataState {
    quotes: Box<[SeqQuote]>,
    /// InstrumentId counter — set by hot loop on RegisterInstrument.
    instrument_count: AtomicU64,
    /// Slots that have been given back, for the surfaces to forget.
    ///
    /// A slot is handed to the next contract that needs one, and a surface
    /// that had cached the old contract's slot went on naming it: the order it
    /// placed was recorded against whatever now holds that slot, and the fill
    /// moved the wrong position.
    ///
    /// Named by the slot rather than by the contract on it, because a contract
    /// the venue has not named yet holds a slot under no id at all — and a
    /// release that could only say "contract nought" named nothing a surface
    /// could act on, for exactly the contracts a caller states by description.
    /// Each with the point in the order of what the client has asked for at
    /// which it was given back.
    ///
    /// A slot number alone says nothing about which occupancy ended: the slot
    /// goes to the next contract that needs one, and a release drained after
    /// that forgot the records of a contract that had just been given it —
    /// live on the wire, and reachable from nothing here.
    released_slots: Mutex<Vec<(crate::types::InstrumentId, u64)>>,
    /// How many callers are on their way onto each slot and have not arrived.
    ///
    /// A caller whose contract turns out to live in another slot is moved onto
    /// it, and is recorded as watching it only once the surface installs the
    /// move. A withdrawal decided in between cannot see it, so the
    /// subscription is held up while this says somebody is coming.
    moves_unread: Mutex<std::collections::HashMap<crate::types::InstrumentId, u32>>,
    /// And how many are on their way off each slot.
    ///
    /// The slot a caller is moving off cannot go back to the table until the
    /// move is installed: the move is the only thing that says where that
    /// caller went, and read off the queue the answer turned false the moment
    /// the queue was emptied — before anything had been moved.
    moves_unread_from: Mutex<std::collections::HashMap<crate::types::InstrumentId, u32>>,
    tbt_trades: Mutex<Vec<TbtTrade>>,
    tbt_quotes: Mutex<Vec<TbtQuote>>,
    /// The point between the two, each time it moved.
    tbt_mids: Mutex<Vec<TbtMid>>,
    real_time_bars: Mutex<Vec<(u32, RealTimeBar)>>,
    depth_updates: Mutex<Vec<DepthUpdate>>,
    /// Books that were dropped for running away unread, and have not been
    /// asked for again.
    ///
    /// A book only means anything whole. Once entries are gone, everything
    /// after them describes positions in a book that no longer exists — so
    /// nothing further is kept for one until the caller withdraws it and asks
    /// again. Handing back what arrives next would be handing back a book that
    /// reads correct and is not.
    depth_dropped: Mutex<std::collections::HashSet<u32>>,
    /// The books given up on that the caller has not been told about yet, and
    /// what happened, under the request each was asked for.
    ///
    /// A dropped book is the one failure here a caller cannot see: the
    /// subscription reads as healthy and the entries simply stop arriving,
    /// which is what a quiet market looks like. Said once per drop.
    depth_drops_unsaid: Mutex<Vec<(u32, String)>>,
    tick_news: Mutex<Vec<TickNews>>,
    news_bulletins: Mutex<Vec<NewsBulletin>>,
    option_computations: Mutex<Vec<crate::types::OptionComputation>>,
    /// The last statement the venue made of its own model, per contract, kept
    /// rather than only handed over.
    last_option_model: Mutex<std::collections::HashMap<crate::types::InstrumentId, crate::types::OptionComputation>>,
    /// Subscriptions the venue was never able to be asked for, and why.
    subscription_failures: Mutex<Vec<(crate::types::InstrumentId, String)>>,
    /// Refusals of the requests that ride beside a quote: the contract, which
    /// companion was refused, and the venue's own reason.
    ///
    /// Held apart from the failures above, which mean the quote itself was
    /// refused. A companion is refused on its own and the quote goes on
    /// ticking, so a caller told on that channel withdrew a subscription that
    /// was working.
    companion_refusals: Mutex<Vec<(crate::types::InstrumentId, u32, String)>>,
    /// Contracts whose per-contract news the venue refused. The engine has
    /// released its own side; the client clears its record of who asked, so a
    /// fresh subscription is sent anew rather than deduped against a claim the
    /// venue already declined. Keyed by con_id, as the client keys its askers.
    news_rejections: Mutex<Vec<i64>>,
    /// The increment each subscription was acknowledged with, for whoever
    /// watches the contract.
    tick_req_params: Mutex<Vec<(crate::types::InstrumentId, f64)>>,
    /// The last minimum increment announced for an instrument, so a request
    /// that follows an existing subscription can be told it too: the venue
    /// sends one tickReqParams per reqMktData, and a follower asked for none.
    last_min_tick: Mutex<std::collections::HashMap<crate::types::InstrumentId, f64>>,
    /// Why the venue refused a contract's subscription, kept for whoever asks
    /// for it next. The failure itself is drained once and told to whoever
    /// held it then; a request that joins the same contract afterwards was
    /// told nothing and received nothing, because the subscription it joined
    /// had already been refused.
    last_subscription_failure: Mutex<std::collections::HashMap<crate::types::InstrumentId, String>>,
    /// A refusal owed to one request that joined a contract already refused.
    subscription_failures_direct: Mutex<Vec<(i64, String)>>,
    /// Why the quote feed is done for the rest of this session, where it is.
    ///
    /// Set once the engine gives up on the feed, and never cleared: the feed
    /// is not coming back within this session, which is what giving up on it
    /// means. Read where a subscription is asked for, so a request that cannot
    /// be served is refused rather than acknowledged into a table nothing will
    /// replay.
    market_data_over: Mutex<Option<&'static str>>,
    /// tickReqParams owed to a single request that followed a live
    /// subscription, delivered to that request alone rather than fanned.
    tick_req_params_direct: Mutex<Vec<(i64, f64)>>,
    /// Lookups that named a contract another slot already holds: the slot the
    /// caller was given, and the one the contract lives in.
    subscription_moves: Mutex<Vec<(crate::types::InstrumentId, crate::types::InstrumentId, u64)>>,
    /// What the venue has said went wrong, in its own words.
    venue_errors: Mutex<Vec<String>>,
    series_ticks: Mutex<std::collections::HashMap<crate::types::InstrumentId, Vec<SeriesTick>>>,
    quote_attribute_masks: Mutex<std::collections::HashMap<crate::types::InstrumentId, (i64, i64)>>,
    /// What the venue last said about a contract itself, beside its prices.
    contract_figures:
        Mutex<std::collections::HashMap<crate::types::InstrumentId, crate::types::ContractFigures>>,
    /// The figures each series has stated for a contract, in the order the
    /// series states them.
    ///
    /// Kept by slot and by series. By slot because a figure belongs to the
    /// contract that was in it, and by series because the venue runs dozens of
    /// them side by side and one record per contract would have each series
    /// overwrite the last.
    stated_figures:
        Mutex<std::collections::HashMap<(crate::types::InstrumentId, u32), Vec<f64>>>,
    /// The numbered figures each series has stated for a contract: the whole
    /// ones under `false` and the fractional ones under `true`, because the
    /// venue numbers the two tables separately and a figure is its number and
    /// the table it stood in.
    numbered_figures: Mutex<NumberedFiguresHeld>,
    /// The venue's option model for a contract as it closed, kept apart from
    /// the standing one.
    closing_option_model: Mutex<
        std::collections::HashMap<crate::types::InstrumentId, crate::types::OptionComputation>,
    >,
    /// The rows of three figures each series has stated for a contract.
    stated_rows: Mutex<StatedRowsHeld>,
    /// The runs of paired figures each series has stated for a contract.
    paired_figures: Mutex<PairedFiguresHeld>,
    /// Which contracts the venue is restricting short sales in.
    ///
    /// Stated on the same record as the halt, and kept here rather than on the
    /// quote: that record is two cache lines exactly, on every slot in the
    /// table, and one more field costs a third.
    short_sale_restricted: Mutex<std::collections::HashSet<crate::types::InstrumentId>>,
    /// How far the venue's clock runs from this machine's, in milliseconds.
    ///
    /// Nothing here ever asks the venue what time it is — this wire carries no
    /// such request. A caller asking for the venue's clock is answered from
    /// this machine's, shifted by this: what the venue has stated about its
    /// own, on the logon it stamps and in the clock it pushes unasked
    /// afterwards. Zero until it states one, so a session that has heard
    /// nothing answers this machine's clock unshifted — the two are not known
    /// to differ, and an answer is owed either way.
    clock_skew_millis: AtomicI64,
    /// Messages the venue sent that nothing here reads, named once each:
    /// which connection, and what it was. Empty is the claim that this client
    /// reads everything this venue sends it, and the only way to check it.
    /// Connection, what the row is about, and the latest thing said about it.
    unread_wire: Mutex<Vec<(&'static str, String, String)>>,
}

impl MarketDataState {
    pub(super) fn new() -> Self {
        Self {
            quotes: (0..MAX_INSTRUMENTS).map(|_| SeqQuote::new()).collect(),
            instrument_count: AtomicU64::new(0),
            released_slots: Mutex::new(Vec::new()),
            moves_unread: Mutex::new(std::collections::HashMap::new()),
            moves_unread_from: Mutex::new(std::collections::HashMap::new()),
            tbt_trades: Mutex::new(Vec::with_capacity(256)),
            tbt_quotes: Mutex::new(Vec::with_capacity(256)),
            tbt_mids: Mutex::new(Vec::with_capacity(256)),
            real_time_bars: Mutex::new(Vec::with_capacity(64)),
            depth_updates: Mutex::new(Vec::with_capacity(64)),
            depth_dropped: Mutex::new(std::collections::HashSet::new()),
            depth_drops_unsaid: Mutex::new(Vec::new()),
            tick_news: Mutex::new(Vec::with_capacity(32)),
            news_bulletins: Mutex::new(Vec::with_capacity(16)),
            option_computations: Mutex::new(Vec::with_capacity(16)),
            last_option_model: Mutex::new(std::collections::HashMap::new()),
            subscription_failures: Mutex::new(Vec::new()),
            companion_refusals: Mutex::new(Vec::new()),
            news_rejections: Mutex::new(Vec::new()),
            tick_req_params: Mutex::new(Vec::new()),
            last_min_tick: Mutex::new(std::collections::HashMap::new()),
            last_subscription_failure: Mutex::new(std::collections::HashMap::new()),
            subscription_failures_direct: Mutex::new(Vec::new()),
            market_data_over: Mutex::new(None),
            tick_req_params_direct: Mutex::new(Vec::new()),
            subscription_moves: Mutex::new(Vec::new()),
            venue_errors: Mutex::new(Vec::new()),
            series_ticks: Mutex::new(std::collections::HashMap::new()),
            quote_attribute_masks: Mutex::new(std::collections::HashMap::new()),
            contract_figures: Mutex::new(std::collections::HashMap::new()),
            stated_figures: Mutex::new(std::collections::HashMap::new()),
            numbered_figures: Mutex::new(std::collections::HashMap::new()),
            paired_figures: Mutex::new(std::collections::HashMap::new()),
            stated_rows: Mutex::new(std::collections::HashMap::new()),
            closing_option_model: Mutex::new(std::collections::HashMap::new()),
            short_sale_restricted: Mutex::new(std::collections::HashSet::new()),
            clock_skew_millis: AtomicI64::new(0),
            unread_wire: Mutex::new(Vec::new()),
        }
    }

    /// Read a quote snapshot (lock-free via SeqLock).
    /// Unchecked hot-path accessor: `id` must be a registered InstrumentId
    /// (< MAX_INSTRUMENTS) or this panics. External surfaces go through
    /// `try_quote`.
    #[inline]
    pub fn quote(&self, id: InstrumentId) -> Quote {
        self.quotes[id as usize].read()
    }

    /// Bounds-checked quote read for user-supplied instrument ids: an
    /// out-of-range id is a caller error, not a reason to panic the process
    /// through the language boundary.
    #[inline]
    pub fn try_quote(&self, id: InstrumentId) -> Option<Quote> {
        if (id as usize) < MAX_INSTRUMENTS {
            Some(self.quotes[id as usize].read())
        } else {
            None
        }
    }

    /// Say that a slot has been given back, and what the client had asked for
    /// up to then.
    #[doc(hidden)] pub fn note_released_slot(
        &self, instrument: crate::types::InstrumentId, released_at: u64,
    ) {
        self.released_slots.lock().unwrap().push((instrument, released_at));
        self.last_min_tick.lock().unwrap().remove(&instrument);
        self.last_subscription_failure.lock().unwrap().remove(&instrument);
        // And what is still queued under it, not only what is cached. Both of
        // these name a slot rather than a contract, so the next contract to
        // take the slot is who they reach: an increment acknowledged for the
        // contract that left arrives as the new one's, and a move recorded for
        // the old one repoints the new one's watchers at a third contract and
        // takes its own slot out of the polling. A reader stalled in a
        // callback is all it takes for the release to land in between.
        self.tick_req_params.lock().unwrap().retain(|(at, _)| *at != instrument);
        let mut moves = self.subscription_moves.lock().unwrap();
        let dropped: Vec<(crate::types::InstrumentId, crate::types::InstrumentId)> = moves
            .iter()
            .filter(|(from, to, _)| *from == instrument || *to == instrument)
            .map(|(from, to, _)| (*from, *to))
            .collect();
        moves.retain(|(from, to, _)| *from != instrument && *to != instrument);
        drop(moves);
        // A move nobody will read is no longer on its way: counted still, it
        // held the subscription on the slot it named up for the rest of the
        // session, and the slot it moved off out of the table.
        for (from, into) in dropped {
            self.note_a_move_is_read(from, into);
        }
        // And the two streams that carry a slot of their own. A headline is
        // about the contract that was named when it arrived, and a model was
        // solved against that contract's volatility and price: delivered after
        // the release, both read as the next occupant's. The model has a cache
        // beside it that is already dropped with the slot, and the queue in
        // front of that cache was not — so the stale answer was gone from the
        // lookup and still on its way to the caller.
        //
        // An account-wide notice is not among these. It names no contract, so
        // no slot can carry it to the wrong one.
        self.tick_news.lock().unwrap().retain(|n| n.instrument != instrument);
        // An answer worked out here is not one of these. It belongs to the
        // question that asked it and names no contract at all, so it is filed
        // under slot zero — which is a real slot, and dropping that one took
        // every answer waiting on it. The same rule the cache beside this
        // queue already keeps.
        self.option_computations.lock().unwrap()
            .retain(|c| c.answers.is_some() || c.instrument != instrument);
    }

    /// The slots given back since this was last asked.
    pub fn take_released_slots(&self) -> Vec<(crate::types::InstrumentId, u64)> {
        std::mem::take(&mut *self.released_slots.lock().unwrap())
    }

    /// Drop a subscription failure still waiting under a slot that has gone
    /// back to the table.
    ///
    /// A failure is resolved to a request through the slot it names, and the
    /// slot's next occupant has its own requests: left queued, the reason the
    /// last contract could not be subscribed was reported to whoever is
    /// watching this one.
    #[doc(hidden)] pub fn forget_subscription_failures(&self, id: crate::types::InstrumentId) {
        self.subscription_failures.lock().unwrap().retain(|(at, _)| *at != id);
        // A refusal belongs to the contract that was in the slot, not to the
        // slot: left behind, the next contract to take it is answered with the
        // last one's refusal.
        self.companion_refusals.lock().unwrap().retain(|(at, ..)| *at != id);
        self.last_subscription_failure.lock().unwrap().remove(&id);
    }

    /// Drop the model last published for a slot, because the slot has gone
    /// back to the table.
    ///
    /// Kept by slot rather than by contract, so nothing else can drop it: the
    /// next contract handed this slot was solved against the previous one's
    /// volatility, price and dividend, and the answer came back finite and
    /// wrong.
    #[doc(hidden)] pub fn forget_option_model(&self, instrument: crate::types::InstrumentId) {
        self.last_option_model.lock().unwrap().remove(&instrument);
        // These belong to the contract that was in the slot, not to the slot.
        self.contract_figures.lock().unwrap().remove(&instrument);
        self.stated_figures.lock().unwrap().retain(|(at, _), _| *at != instrument);
        self.numbered_figures.lock().unwrap().retain(|(at, ..), _| *at != instrument);
        self.paired_figures.lock().unwrap().retain(|(at, _), _| *at != instrument);
        self.stated_rows.lock().unwrap().retain(|(at, _), _| *at != instrument);
        self.closing_option_model.lock().unwrap().remove(&instrument);
        // A restriction belongs to the contract that was in the slot, not to
        // the slot: left behind, the next contract to take it reads as
        // restricted on the strength of the last one.
        self.short_sale_restricted.lock().unwrap().remove(&instrument);
    }

    /// Number of registered instruments.
    pub fn instrument_count(&self) -> u32 {
        self.instrument_count.load(Ordering::Relaxed) as u32
    }

    /// Take every tbt trades waiting, leaving none.
    pub fn drain_tbt_trades(&self) -> Vec<TbtTrade> {
        self.tbt_trades.lock().unwrap().drain(..).collect()
    }

    /// Take every tbt quotes waiting, leaving none.
    pub fn drain_tbt_quotes(&self) -> Vec<TbtQuote> {
        self.tbt_quotes.lock().unwrap().drain(..).collect()
    }

    /// Take every midpoint waiting, leaving none.
    pub fn drain_tbt_mids(&self) -> Vec<TbtMid> {
        self.tbt_mids.lock().unwrap().drain(..).collect()
    }

    /// Take every real time bars waiting, leaving none.
    pub fn drain_real_time_bars(&self) -> Vec<(u32, RealTimeBar)> {
        self.real_time_bars.lock().unwrap().drain(..).collect()
    }

    /// Take the bars a dispatch loop should deliver, leaving behind those a
    /// stream is going to read by id.
    ///
    /// A stream cannot hold the session's turn — it outlives any one read of
    /// it — so its records are left where it will find them, the way the
    /// answering calls' own are. `mine` says which ids this session is reading
    /// for itself; see `ReferenceState::is_ours`.
    pub fn drain_real_time_bars_for_dispatch(
        &self, mine: impl Fn(u32) -> bool,
    ) -> Vec<(u32, RealTimeBar)> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.real_time_bars.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(mine(e.0)));
        *held = kept;
        out
    }

    /// Bars answering one request, leaving other requests' alone.
    pub fn take_real_time_bars_for(&self, req_id: u32) -> Vec<RealTimeBar> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so draining a request that holds most of a full queue costs
        // the square of it — on exactly the path a caller takes when its own
        // stream has grown large.
        let mut q = self.real_time_bars.lock().unwrap();
        let (mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *q).into_iter().partition(|b| b.0 == req_id);
        *q = rest;
        mine.into_iter().map(|b| b.1).collect()
    }

    /// Book changes answering one request.
    pub fn take_depth_updates_for(&self, req_id: u32) -> Vec<DepthUpdate> {
        // Partitioned, not removed one at a time: see the bars above.
        let mut q = self.depth_updates.lock().unwrap();
        let (mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *q).into_iter().partition(|u| u.req_id == req_id);
        *q = rest;
        mine
    }

    /// Take every depth updates waiting, leaving none.
    pub fn drain_depth_updates(&self) -> Vec<DepthUpdate> {
        self.depth_updates.lock().unwrap().drain(..).collect()
    }

    /// Take the depth updates a dispatch loop should deliver, leaving behind
    /// those a stream is going to read by id — see
    /// [`drain_real_time_bars_for_dispatch`](Self::drain_real_time_bars_for_dispatch).
    pub fn drain_depth_updates_for_dispatch(
        &self, mine: impl Fn(u32) -> bool,
    ) -> Vec<DepthUpdate> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.depth_updates.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(mine(e.req_id)));
        *held = kept;
        out
    }

    /// Take every tick news waiting, leaving none.
    pub fn drain_tick_news(&self) -> Vec<TickNews> {
        self.tick_news.lock().unwrap().drain(..).collect()
    }

    /// Take every news bulletins waiting, leaving none.
    pub fn drain_news_bulletins(&self) -> Vec<NewsBulletin> {
        self.news_bulletins.lock().unwrap().drain(..).collect()
    }

    /// Take every option computations waiting, leaving none.
    pub fn drain_option_computations(&self) -> Vec<crate::types::OptionComputation> {
        self.option_computations.lock().unwrap().drain(..).collect()
    }

    /// Everything the venue has sent this session that nothing reads.
    pub fn unread_wire(&self) -> Vec<(&'static str, String)> {
        self.unread_wire.lock().unwrap().iter()
            .map(|(connection, _, what)| (*connection, what.clone()))
            .collect()
    }

    #[doc(hidden)] pub fn note_unread_wire(&self, connection: &'static str, what: String) {
        // The sentence is its own key, which is what this has always done: one
        // row per distinct thing said.
        self.note_unread_wire_under(connection, what.clone(), what);
    }

    /// The same, where the thing being recorded carries a reading that changes.
    ///
    /// Keyed on what it is about rather than on the whole sentence, and the
    /// latest reading replaces the last. Deduplicating on the sentence, a
    /// record that names a figure grows a row every time that figure moves —
    /// which for an account held in another currency is every time the rate
    /// does. This is a vector scanned linearly on the trading loop, so that is
    /// unbounded memory and quadratic work, from ordinary traffic and no
    /// malformed input at all.
    #[doc(hidden)] pub fn note_unread_wire_under(
        &self, connection: &'static str, key: String, what: String,
    ) {
        let mut seen = self.unread_wire.lock().unwrap();
        match seen.iter().position(|(c, k, _)| *c == connection && *k == key) {
            Some(at) => seen[at].2 = what,
            None => seen.push((connection, key, what)),
        }
    }

    /// Learn the venue's clock from a time it stated in its own stamp form.
    ///
    /// A stamp nothing can read leaves the skew where it was: the last thing
    /// the venue said about its clock is a better answer than no answer.
    pub fn note_venue_time(&self, stamped: &str) {
        if let Some(millis) = crate::protocol::datetime::ib_datetime_to_unix_millis(stamped) {
            self.note_venue_millis(millis);
        }
    }

    /// The same, where the venue states its clock as a number of its own
    /// rather than as a stamp on something else.
    pub fn note_venue_millis(&self, venue_millis: i64) {
        // Saturating for the same reason the conversion above is: the
        // difference between a stated clock at the end of the range and this
        // machine's is not representable, and wrapping it puts the session on
        // a clock neither side named.
        self.clock_skew_millis.store(venue_millis.saturating_sub(local_millis()), Ordering::Relaxed);
    }

    /// What the venue's clock reads now: this machine's, shifted by what the
    /// venue has stated about the difference.
    ///
    /// Read rather than remembered, so the answer keeps moving on a connection
    /// that has gone quiet. Held as the last stamp seen, it stood still for as
    /// long as the venue said nothing, and a caller reading it twice a minute
    /// apart was told the same instant twice.
    pub fn venue_time_millis(&self) -> i64 {
        local_millis().saturating_add(self.clock_skew_millis.load(Ordering::Relaxed))
    }

    /// Take every venue errors waiting, leaving none.
    pub fn drain_venue_errors(&self) -> Vec<String> {
        self.venue_errors.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_venue_error(&self, text: String) {
        self.venue_errors.lock().unwrap().push(text);
    }

    /// Take every subscription failures waiting, leaving none.
    pub fn drain_subscription_failures(&self) -> Vec<(crate::types::InstrumentId, String)> {
        self.subscription_failures.lock().unwrap().drain(..).collect()
    }

    /// Take every companion refusal waiting, leaving none.
    ///
    /// The venue names the request it is refusing and says why — the one
    /// refusal channel on this wire that names its request. Logged and dropped,
    /// a caller that asked for the option model on a class the venue has no
    /// model for watched an acknowledged subscription that could never produce
    /// a computation, and was told nothing while the venue had said why at
    /// once.
    pub fn drain_companion_refusals(&self) -> Vec<(crate::types::InstrumentId, u32, String)> {
        self.companion_refusals.lock().unwrap().drain(..).collect()
    }

    /// Take every con_id whose news the venue refused, leaving none. The
    /// client clears its askers for each, so a re-ask is sent anew.
    pub fn drain_news_rejections(&self) -> Vec<i64> {
        self.news_rejections.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_news_rejection(&self, con_id: i64) {
        self.news_rejections.lock().unwrap().push(con_id);
    }

    /// The increment a subscription was acknowledged with, kept for whoever
    /// watches the contract. Engine side.
    #[doc(hidden)] pub fn push_tick_req_params(&self, instrument: crate::types::InstrumentId, min_tick: f64) {
        // A follower joining after dispatch takes the acknowledgement reads
        // this cache, so it is ready before the acknowledgement can be read.
        self.last_min_tick.lock().unwrap().insert(instrument, min_tick);
        self.tick_req_params.lock().unwrap().push((instrument, min_tick));
    }

    /// The increment a follower should be told, if the subscription it follows
    /// was already acknowledged. `None` before that — the pending tickReqParams
    /// fans out to the follower when it arrives.
    pub fn min_tick_for_follower(&self, instrument: crate::types::InstrumentId) -> Option<f64> {
        self.last_min_tick.lock().unwrap().get(&instrument).copied()
    }

    /// tickReqParams owed to one request that followed a live subscription.
    #[doc(hidden)] pub fn push_tick_req_params_for(&self, req_id: i64, min_tick: f64) {
        self.tick_req_params_direct.lock().unwrap().push((req_id, min_tick));
    }

    /// Take those, in the order they came. Client side.
    pub fn drain_tick_req_params_direct(&self) -> Vec<(i64, f64)> {
        self.tick_req_params_direct.lock().unwrap().drain(..).collect()
    }

    /// Take the acknowledged increments, in the order they came. Client side.
    pub fn drain_tick_req_params(&self) -> Vec<(crate::types::InstrumentId, f64)> {
        self.tick_req_params.lock().unwrap().drain(..).collect()
    }

    /// Whether a move away from this slot is still waiting to be read.
    ///
    /// The slot a move points away from cannot be given back while the move is
    /// still queued: the release purges the moves that name it, and the move is
    /// the only thing telling this slot's watchers where their contract went.
    /// Given back afterwards, once the move has been read, both hold.
    pub fn a_move_is_pending_from(&self, instrument: crate::types::InstrumentId) -> bool {
        self.moves_unread_from.lock().unwrap().get(&instrument).is_some_and(|w| *w > 0)
    }

    /// Whether a reason this slot's subscription could not be made is still
    /// waiting to be read. Asked for the same cause a pending move is: giving
    /// the slot back drops it, and it is the only thing the caller who asked
    /// will ever be told.
    pub fn a_failure_is_pending_from(&self, instrument: crate::types::InstrumentId) -> bool {
        self.subscription_failures.lock().unwrap().iter().any(|(at, _)| *at == instrument)
    }

    /// Where a caller's slot has to follow, because the contract it named is
    /// already held by another. Read the way a refusal is.
    pub fn drain_subscription_moves(
        &self,
    ) -> Vec<(crate::types::InstrumentId, crate::types::InstrumentId, u64)> {
        self.subscription_moves.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)]
    pub fn push_subscription_move(
        &self,
        from: crate::types::InstrumentId,
        into: crate::types::InstrumentId,
        took_it: u64,
    ) {
        // Both under one acquisition, the count first. Published apart, a
        // surface could take the move off the queue and say it had been read
        // before the count existed — and the count created afterwards was
        // discharged by no move, so the subscription on the slot it named was
        // held up for the rest of the session with nobody watching it.
        let mut moves = self.subscription_moves.lock().unwrap();
        *self.moves_unread.lock().unwrap().entry(into).or_insert(0) += 1;
        *self.moves_unread_from.lock().unwrap().entry(from).or_insert(0) += 1;
        moves.push((from, into, took_it));
    }

    /// Whether a caller is on its way onto this slot and has not arrived yet.
    ///
    /// Such a caller has asked for the contract and is not yet recorded as
    /// watching the slot it holds, so a withdrawal decided in the meantime
    /// cannot see it: the subscription went, and the caller arrived on a slot
    /// with nothing on the wire.
    ///
    /// Counted rather than read off the queue, because the queue is emptied
    /// before the move is installed: read there, the answer turned false in
    /// exactly the window this is for.
    #[doc(hidden)] pub fn a_move_is_on_its_way_into(
        &self, instrument: crate::types::InstrumentId,
    ) -> bool {
        self.moves_unread.lock().unwrap().get(&instrument).is_some_and(|waiting| *waiting > 0)
    }

    /// Say that a move has been installed, whatever became of the requests it
    /// named.
    #[doc(hidden)] pub fn note_a_move_is_read(
        &self, from: crate::types::InstrumentId, into: crate::types::InstrumentId,
    ) {
        for (map, slot) in [(&self.moves_unread, into), (&self.moves_unread_from, from)] {
            let mut waiting = map.lock().unwrap();
            if let Some(left) = waiting.get_mut(&slot) {
                *left = left.saturating_sub(1);
                if *left == 0 {
                    waiting.remove(&slot);
                }
            }
        }
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)]
    pub fn push_quote(&self, id: InstrumentId, quote: &Quote) {
        self.quotes[id as usize].write(quote);
    }

    /// Zero every quote a caller can read, as the engine zeroes its own copy at
    /// the same moment.
    ///
    /// The engine zeroing its own is what stops a price from before a drop
    /// being read as current — but the copy a caller reads is this one, and it
    /// was left standing. Against a baseline the drop had just cleared, every
    /// field of that stale quote read as a move and went out again as a fresh
    /// tick; then whatever the venue had not restated by the next connection
    /// went out a second time as nought.
    #[doc(hidden)] pub fn zero_all_quotes(&self) {
        let blank = Quote::default();
        let held = self.instrument_count.load(Ordering::Relaxed) as usize;
        for slot in self.quotes.iter().take(held.min(self.quotes.len())) {
            slot.write(&blank);
        }
    }

    #[doc(hidden)] pub fn push_tbt_trade(&self, trade: TbtTrade) {
        push_bounded(&self.tbt_trades, trade, STREAM_BACKLOG_LIMIT, "tbt_trades");
    }

    #[doc(hidden)] pub fn push_tbt_quote(&self, quote: TbtQuote) {
        push_bounded(&self.tbt_quotes, quote, STREAM_BACKLOG_LIMIT, "tbt_quotes");
    }

    #[doc(hidden)] pub fn push_tbt_mid(&self, mid: TbtMid) {
        push_bounded(&self.tbt_mids, mid, STREAM_BACKLOG_LIMIT, "tbt_mids");
    }


    #[doc(hidden)] pub fn push_real_time_bar(&self, req_id: u32, bar: RealTimeBar) {
        push_bounded(&self.real_time_bars, (req_id, bar), STREAM_BACKLOG_LIMIT, "real_time_bars");
    }

    #[doc(hidden)] pub fn push_depth_update(&self, update: DepthUpdate) {
        // A book is not a stream of independent rows: each entry says insert,
        // change or delete AT a position, so it only means anything against
        // every entry before it. Shedding the oldest of these the way a quote
        // or a trade is shed does not lose old rows — it leaves every later
        // position pointing into a book missing its start, and the reader gets
        // a well-formed book with the wrong prices in it.
        //
        // So a book that has run away is dropped whole, per request, and the
        // caller is told. Nothing is a book it can trust; a wrong one reads
        // like a right one.
        // The one that has run away is dropped, which is not the same as the
        // one that pushed. Every book shares this queue and each is drained on
        // its own, so measuring the whole and dropping whoever arrives next
        // destroys the book of a caller reading diligently because of one that
        // is not — and leaves the one that is not still flooding.
        //
        // Reading the queue costs walking it, so it is only walked once the
        // whole has run out of room, and what it drops then is the longest
        // book. That is the one nobody is draining, and dropping it puts the
        // queue back under its bound, so the next push is cheap again.
        {
            // Nothing is kept for a book already given up on. What arrives
            // now describes positions in a book that no longer exists, and
            // kept, it would be handed back as though it were one.
            if self.depth_dropped.lock().unwrap().contains(&update.req_id) {
                return;
            }
            let mut held = self.depth_updates.lock().unwrap();
            if held.len() >= STREAM_BACKLOG_LIMIT {
                let mut per_book: std::collections::HashMap<u32, usize> =
                    std::collections::HashMap::new();
                for u in held.iter() {
                    *per_book.entry(u.req_id).or_insert(0) += 1;
                }
                if let Some((&worst, &how_many)) = per_book.iter().max_by_key(|(_, n)| **n) {
                    held.retain(|u| u.req_id != worst);
                    self.depth_dropped.lock().unwrap().insert(worst);
                    // Told on the request that asked for it, once. The venue
                    // goes on sending this book and nothing further is kept,
                    // so a caller not told reads a subscription that is up and
                    // a book that has stopped moving — which is what a quiet
                    // market looks like.
                    self.depth_drops_unsaid.lock().unwrap().push((
                        worst,
                        format!(
                            "the book on this request went past the {STREAM_BACKLOG_LIMIT} \
                             entries kept for one and was given up whole ({how_many} \
                             entries), because part of a book is not a book — withdraw it \
                             and ask again to start another",
                        ),
                    ));
                    if worst == update.req_id {
                        // Including the one that arrived. It is usually the
                        // book that ran away that pushes next, and kept, it
                        // would be the first entry of a book starting from
                        // the middle — which is the thing being prevented.
                        log::warn!(
                            "the book on request {worst} has gone past what is kept for \
                             one and was dropped whole ({how_many} entries), because part \
                             of a book is not a book — withdraw it and ask again to start \
                             another",
                        );
                        return;
                    }
                    log::warn!(
                        "the book on request {worst} has gone past what is kept for one and \
                         was dropped whole ({how_many} entries), because part of a book is \
                         not a book — resubscribe to start it again",
                    );
                }
            }
            held.push(update);
        }
    }

    /// Throw away bars still queued under a request.
    ///
    /// Withdrawing stops the venue sending more; it does not unsend what has
    /// already arrived and nobody has read. Left there, the next request under
    /// the same number is served the previous stream's bars.
    #[doc(hidden)] pub fn purge_real_time_bars(&self, req_id: u32) {
        self.real_time_bars.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Throw away tick-by-tick records still queued under a request.
    #[doc(hidden)] pub fn purge_tbt_for(&self, req_id: i64) {
        self.tbt_trades.lock().unwrap().retain(|t| t.req_id != req_id);
        self.tbt_quotes.lock().unwrap().retain(|q| q.req_id != req_id);
        // The midpoints as well. Left queued, a stream reopened under the same
        // number before the next read was served the withdrawn stream's
        // midpoints, under its own number and about another contract.
        self.tbt_mids.lock().unwrap().retain(|m| m.req_id != req_id);
    }

    /// Remove all buffered depth updates for a given req_id (called on cancel).
    #[doc(hidden)] pub fn purge_depth_updates(&self, req_id: u32) {
        self.depth_updates.lock().unwrap().retain(|u| u.req_id != req_id);
        // Withdrawing is how a caller starts again, so this is where a book
        // that was dropped stops being refused — and where the notice of that
        // drop goes with it. Left queued, a subscription started again under
        // the same number opened with the failure of the one before it: the
        // caller was told a healthy book had been given up on.
        self.depth_dropped.lock().unwrap().remove(&req_id);
        self.depth_drops_unsaid.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Whether a book was dropped for running away and has not been asked for
    /// again. Nothing is kept for one until it is.
    #[doc(hidden)] pub fn depth_was_dropped(&self, req_id: u32) -> bool {
        self.depth_dropped.lock().unwrap().contains(&req_id)
    }

    /// Take the books given up on that the caller has not been told about,
    /// leaving none.
    pub fn drain_depth_drops(&self) -> Vec<(u32, String)> {
        self.depth_drops_unsaid.lock().unwrap().drain(..).collect()
    }

    /// The same, leaving behind the books a stream is going to read by id.
    ///
    /// A dropped book is the one failure a stream cannot tell from a quiet
    /// market, so it has to reach the stream that asked for the book. Drained
    /// whole beside one, it was reported to a callback and the stream sat
    /// through its idle span and ended saying nothing had gone wrong.
    pub fn drain_depth_drops_for_dispatch(
        &self, mine: impl Fn(u32) -> bool,
    ) -> Vec<(u32, String)> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.depth_drops_unsaid.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(mine(e.0)));
        *held = kept;
        out
    }

    /// The book given up on under one request, if there is one, leaving the
    /// rest.
    pub fn take_depth_drop_for(&self, req_id: u32) -> Option<String> {
        let mut held = self.depth_drops_unsaid.lock().unwrap();
        let at = held.iter().position(|(id, _)| *id == req_id)?;
        Some(held.remove(at).1)
    }

    #[doc(hidden)] pub fn push_tick_news(&self, news: TickNews) {
        push_bounded(&self.tick_news, news, STREAM_BACKLOG_LIMIT, "tick_news");
    }

    /// A series the caller asked for, decoded, waiting to be delivered.
    ///
    /// The extra series ride on the same subscription as the prices but arrive
    /// on records of their own, so they are queued here rather than written
    /// into the quote: a quote holds one value per field and these are not
    /// fields of a quote. The poll that delivers the quote drains them and
    /// hands each to the caller under the number the reference client uses.
    ///
    /// Bounded like every other stream the venue pushes unasked: a caller who
    /// asked for a busy series and stopped reading would otherwise hold every
    /// record of the day.
    #[doc(hidden)] pub fn push_series_tick(&self, tick: SeriesTick) {
        let mut held = self.series_ticks.lock().unwrap();
        let queued = held.entry(tick.instrument).or_default();
        // Bounded per contract, because the venue serves a series whether or
        // not the caller polls: a busy series on a contract nobody is reading
        // would otherwise hold every record of the day. Past the bound the
        // oldest go, so what a late reader gets is the most recent rather than
        // everything or nothing.
        // A tenth at a time rather than one at a time, as every other stream
        // here sheds: dropping one per push leaves every later push shifting
        // the whole vector — on the thread reading the socket, under this
        // lock, for every record of a busy series.
        if queued.len() >= STREAM_BACKLOG_LIMIT {
            let keep_to = STREAM_BACKLOG_LIMIT - STREAM_BACKLOG_LIMIT / 10;
            let shed = queued.len() - keep_to;
            queued.drain(..shed);
            log::warn!(
                "the extra series on one contract have gone past {STREAM_BACKLOG_LIMIT} \
                 unread, so the oldest of them were dropped — nothing is draining them",
            );
        }
        queued.push(tick);
    }

    /// What this contract's extra series have stated since the last read.
    pub fn drain_series_ticks(&self, instrument: crate::types::InstrumentId) -> Vec<SeriesTick> {
        self.series_ticks.lock().unwrap().remove(&instrument).unwrap_or_default()
    }

    /// What the venue says about a contract's two prices rather than what they
    /// are: one mask for whether each side may be dealt on without a human,
    /// one for pre-open and past-limit.
    ///
    /// Kept beside the quote rather than in it. A quote is read on the hot
    /// path and sized to the cache lines it occupies; these change seldom —
    /// a side becomes eligible or stops being, once — so they are held where
    /// widening costs nothing.
    #[doc(hidden)] pub fn note_quote_attributes(
        &self, instrument: crate::types::InstrumentId, eligible: i64, state: i64,
    ) {
        self.quote_attribute_masks.lock().unwrap().insert(instrument, (eligible, state));
    }

    /// The two masks as the venue last stated them, or nothing stated.
    pub fn quote_attribute_masks(&self, instrument: crate::types::InstrumentId) -> (i64, i64) {
        self.quote_attribute_masks.lock().unwrap().get(&instrument).copied().unwrap_or((0, 0))
    }

    /// Everything held for a contract goes when its subscription does.
    #[doc(hidden)] pub fn forget_series_ticks(&self, instrument: crate::types::InstrumentId) {
        self.series_ticks.lock().unwrap().remove(&instrument);
        self.quote_attribute_masks.lock().unwrap().remove(&instrument);
    }

    /// A broadcast notice, kept until someone reads it.
    ///
    /// Bounded, because the venue broadcasts these whether or not anyone
    /// subscribed and the drain only runs once someone has: a session that
    /// never asks for bulletins would otherwise hold every notice of the day
    /// for the life of the process and free none of them. Past the bound the
    /// oldest are dropped, so a late subscriber is handed the most recent
    /// [`NEWS_BULLETIN_LIMIT`] rather than everything or nothing.
    #[doc(hidden)] pub fn push_news_bulletin(&self, bulletin: NewsBulletin) {
        let mut held = self.news_bulletins.lock().unwrap();
        if held.len() >= NEWS_BULLETIN_LIMIT {
            // ponytail: O(n) shift on a queue of a thousand, on an event that
            // arrives a few times an hour. A VecDeque if bulletins ever became
            // a hot path.
            held.remove(0);
        }
        held.push(bulletin);
    }

    /// Whether the venue is restricting short sales in this contract.
    ///
    /// The circuit breaker a venue puts on a contract that has fallen far
    /// enough in a day, which stops a short from resting below the bid. Stated
    /// on the same record as the halt, decoded, and read by nobody: a program
    /// routing a short into such a contract had the order bounced rather than
    /// knowing not to send it.
    ///
    /// Not the same question as whether the contract can be borrowed, which
    /// this client already answers beside it — a contract can be freely
    /// borrowable and still restricted.
    pub fn short_sale_restricted(&self, instrument: crate::types::InstrumentId) -> bool {
        self.short_sale_restricted.lock().unwrap().contains(&instrument)
    }

    /// Say what the venue's status record said about short sales in a
    /// contract.
    #[doc(hidden)]
    pub fn note_short_sale_restriction(
        &self, instrument: crate::types::InstrumentId, restricted: bool,
    ) {
        let mut held = self.short_sale_restricted.lock().unwrap();
        if restricted {
            held.insert(instrument);
        } else {
            held.remove(&instrument);
        }
    }

    /// What the venue last said about a contract itself: how many shares are on
    /// issue and what it opened at a year ago.
    ///
    /// Stated on the tick that carries the price extremes, and read past. The
    /// documented API reaches neither from a quote subscription — share count
    /// is a fundamentals request of its own there, and a year-ago open has no
    /// call at all.
    pub fn contract_figures(
        &self, instrument: crate::types::InstrumentId,
    ) -> Option<crate::types::ContractFigures> {
        self.contract_figures.lock().unwrap().get(&instrument).copied()
    }

    /// What one series last stated for a contract, in the order it states it.
    ///
    /// Empty until that series has been asked for and answered. A figure the
    /// venue holds nothing for arrives as the largest number its field carries,
    /// which is how the venue says it has nothing rather than saying nothing at
    /// all, and a record that stopped short states the figures before it and no
    /// more.
    pub fn stated_figures(
        &self, instrument: crate::types::InstrumentId, series: u32,
    ) -> Vec<f64> {
        self.stated_figures.lock().unwrap().get(&(instrument, series)).cloned().unwrap_or_default()
    }

    /// Which series have stated figures for a contract, in order.
    pub fn stated_figures_series(&self, instrument: crate::types::InstrumentId) -> Vec<u32> {
        let mut series: Vec<u32> = self.stated_figures.lock().unwrap()
            .keys()
            .filter(|(at, _)| *at == instrument)
            .map(|(_, series)| *series)
            .collect();
        series.sort_unstable();
        series
    }

    /// The figures one series stated for a contract under the venue's own
    /// numbering, whole or fractional.
    ///
    /// Four series state their figures as two numbered tables. Two of the
    /// numbers have a documented call to reach a caller on and the rest have
    /// none, so the rest are kept here under the number the venue gave them
    /// rather than sent as a tick number of this client's own choosing.
    ///
    /// Empty until that series has been asked for and answered.
    pub fn numbered_figures(
        &self, instrument: crate::types::InstrumentId, series: u32, fractional: bool,
    ) -> Vec<(i32, f64)> {
        self.numbered_figures
            .lock()
            .unwrap()
            .get(&(instrument, series, fractional))
            .cloned()
            .unwrap_or_default()
    }

    /// Which series have stated numbered figures for a contract, in order.
    pub fn numbered_figures_series(&self, instrument: crate::types::InstrumentId) -> Vec<u32> {
        let mut series: Vec<u32> = self.numbered_figures.lock().unwrap()
            .keys()
            .filter(|(at, ..)| *at == instrument)
            .map(|(_, series, _)| *series)
            .collect();
        series.sort_unstable();
        series.dedup();
        series
    }

    /// The run of paired figures one series stated for a contract, in the
    /// order it stated them.
    ///
    /// Two series state their figures this way: a count, then that many pairs.
    /// One holds the volatility the venue's own model puts on each point of a
    /// curve; the other holds the weight it puts on each price a contract might
    /// reach. Neither has a documented call to arrive on.
    ///
    /// Empty until that series has been asked for and answered.
    pub fn paired_figures(
        &self, instrument: crate::types::InstrumentId, series: u32,
    ) -> Vec<(f64, f64)> {
        self.paired_figures.lock().unwrap().get(&(instrument, series)).cloned().unwrap_or_default()
    }

    /// Which series have stated paired figures for a contract, in order.
    pub fn paired_figures_series(&self, instrument: crate::types::InstrumentId) -> Vec<u32> {
        let mut series: Vec<u32> = self.paired_figures.lock().unwrap()
            .keys()
            .filter(|(at, _)| *at == instrument)
            .map(|(_, series)| *series)
            .collect();
        series.sort_unstable();
        series
    }

    /// What the venue's model made of an option as it closed.
    ///
    /// The same model as the standing one and in the same shape — every greek
    /// it states, including the ones the documented API has no field for — but
    /// worked out as the contract closed rather than as it stands. The
    /// documented API has no call for it at all.
    ///
    /// `None` until the series has been asked for and answered.
    pub fn closing_option_model(
        &self, instrument: crate::types::InstrumentId,
    ) -> Option<crate::types::OptionComputation> {
        self.closing_option_model.lock().unwrap().get(&instrument).copied()
    }

    #[doc(hidden)] pub fn note_closing_option_model(
        &self, comp: crate::types::OptionComputation,
    ) {
        self.closing_option_model.lock().unwrap().insert(comp.instrument, comp);
    }

    /// The rows of three figures one series stated for a contract, in the
    /// order it stated them. What the three are is the series' own:
    ///
    /// | Series | The three figures |
    /// | --- | --- |
    /// | 547 | A quantity, what it is offered at, and a second price where the form states one |
    /// | 491 | Which strategy the leg belongs to, the contract it names, and its size |
    ///
    /// Neither has a documented call to arrive on. `f64::MAX` stands where a
    /// form states no third figure. Empty until the series has been asked for
    /// and answered.
    pub fn stated_rows(
        &self, instrument: crate::types::InstrumentId, series: u32,
    ) -> Vec<(f64, f64, f64)> {
        self.stated_rows.lock().unwrap().get(&(instrument, series)).cloned().unwrap_or_default()
    }

    #[doc(hidden)] pub fn note_stated_rows(
        &self, instrument: crate::types::InstrumentId, series: u32, rows: Vec<(f64, f64, f64)>,
    ) {
        self.stated_rows.lock().unwrap().insert((instrument, series), rows);
    }

    #[doc(hidden)] pub fn note_paired_figures(
        &self, instrument: crate::types::InstrumentId, series: u32, pairs: Vec<(f64, f64)>,
    ) {
        self.paired_figures.lock().unwrap().insert((instrument, series), pairs);
    }

    #[doc(hidden)] pub fn note_numbered_figures(
        &self, instrument: crate::types::InstrumentId, series: u32,
        whole: &[(i32, f64)], fractional: &[(i32, f64)],
    ) {
        // A table the record did not carry is not a table the venue emptied:
        // left out of one message, what it last stated still stands.
        let mut held = self.numbered_figures.lock().unwrap();
        for (table, stated) in [(false, whole), (true, fractional)] {
            if !stated.is_empty() {
                held.insert((instrument, series, table), stated.to_vec());
            }
        }
    }

    #[doc(hidden)] pub fn note_stated_figures(
        &self, instrument: crate::types::InstrumentId, series: u32, figures: Vec<f64>,
    ) {
        self.stated_figures.lock().unwrap().insert((instrument, series), figures);
    }

    /// Keep what the venue stated about a contract, merging with what it said
    /// before: the two figures arrive on the same tick but either may be
    /// absent from a given one, and a figure left out is not a figure withdrawn.
    #[doc(hidden)] pub fn note_contract_figures(
        &self,
        instrument: crate::types::InstrumentId,
        shares_outstanding: Option<f64>,
        open_a_year_ago: Option<f64>,
    ) {
        let mut held = self.contract_figures.lock().unwrap();
        let entry = held.entry(instrument).or_default();
        if let Some(v) = shares_outstanding {
            entry.shares_outstanding = v;
        }
        if let Some(v) = open_a_year_ago {
            entry.open_a_year_ago = v;
        }
    }

    /// What the venue last said its own model made of a contract.
    ///
    /// Kept as well as delivered. Delivered alone it is gone the moment a
    /// caller reads it, and answering "what would this be worth at another
    /// volatility" needs the venue's statement still to hand.
    pub fn option_model(&self, instrument: crate::types::InstrumentId) -> Option<crate::types::OptionComputation> {
        self.last_option_model.lock().unwrap().get(&instrument).copied()
    }

    #[doc(hidden)] pub fn push_option_computation(&self, comp: crate::types::OptionComputation) {
        // Only the venue's own statement becomes the model for a contract. An
        // answer worked out here belongs to the question that asked it and
        // names no contract at all — stored, it lands on slot zero, which is a
        // real contract, and the next question about that contract is answered
        // against the last caller's own volatility and price instead of the
        // venue's. It also says nothing about which model the venue used, so
        // the refusal that guards the one this client cannot solve with reads
        // as though there were nothing to guard.
        if comp.answers.is_none() {
            self.last_option_model.lock().unwrap().insert(comp.instrument, comp);
        }
        push_bounded(&self.option_computations, comp, STREAM_BACKLOG_LIMIT, "option_computations");
    }

    #[doc(hidden)] pub fn push_companion_refusal(
        &self, instrument: crate::types::InstrumentId, kind: u32, reason: String,
    ) {
        self.companion_refusals.lock().unwrap().push((instrument, kind, reason));
    }

    #[doc(hidden)] pub fn push_subscription_failure(&self, instrument: crate::types::InstrumentId, reason: String) {
        self.last_subscription_failure.lock().unwrap()
            .insert(instrument, reason.clone());
        self.subscription_failures.lock().unwrap().push((instrument, reason));
    }

    /// Why a contract a request is about to join was refused, if it was.
    /// `None` where the subscription it joins is live.
    pub fn failure_for_follower(&self, instrument: crate::types::InstrumentId) -> Option<String> {
        self.last_subscription_failure.lock().unwrap().get(&instrument).cloned()
    }

    /// Why the quote feed is done for the rest of this session, if it is.
    ///
    /// A subscription asked for after this is set cannot be served: there is
    /// no connection to write it to and no reconnect coming to replay it.
    pub fn market_data_over(&self) -> Option<&'static str> {
        *self.market_data_over.lock().unwrap()
    }

    /// Say the quote feed is done for the rest of this session.
    #[doc(hidden)] pub fn set_market_data_over(&self, why: &'static str) {
        *self.market_data_over.lock().unwrap() = Some(why);
    }

    /// And take it back, for a feed a caller has rebuilt by hand.
    ///
    /// The engine gives up on its own recovery and never picks it up again,
    /// which is what giving up means — but a caller handing in a transport of
    /// its own is not the engine's recovery, and the feed it hands in is live.
    /// Left standing, every subscription on that live feed was refused for the
    /// rest of the session.
    #[doc(hidden)] pub fn clear_market_data_over(&self) {
        *self.market_data_over.lock().unwrap() = None;
    }

    /// The venue has taken this contract's subscription, so the reason it
    /// last refused one is no longer what a joining request is owed.
    ///
    /// Only the copy kept for joiners is dropped. The queued refusals are
    /// deliveries the caller that asked has not read yet, and a later success
    /// does not unsay them — the request they name was refused.
    ///
    /// Kept until the slot went back to the table, the reason a contract could
    /// not be subscribed before a reconnect was still there afterwards, and
    /// every request joining the subscription that had since come up was told
    /// it had been refused.
    #[doc(hidden)] pub fn note_subscription_accepted(&self, id: crate::types::InstrumentId) {
        self.last_subscription_failure.lock().unwrap().remove(&id);
    }

    /// A refusal owed to one request that joined a contract already refused.
    #[doc(hidden)] pub fn push_subscription_failure_for(&self, req_id: i64, reason: String) {
        self.subscription_failures_direct.lock().unwrap().push((req_id, reason));
    }

    /// Take every refusal owed to a single request, leaving none.
    pub fn drain_subscription_failures_direct(&self) -> Vec<(i64, String)> {
        self.subscription_failures_direct.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn set_instrument_count(&self, count: u32) {
        self.instrument_count.store(count as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod follower_tick_req_params_tests {
    use super::*;

    /// A request that follows a live subscription is owed the increment that
    /// subscription was acknowledged with — the venue sends one tickReqParams
    /// per reqMktData, and a follower asked for none. Cleared when the slot is
    /// given back, so a reclaimed contract does not carry a stale increment.
    #[test]
    fn a_follower_is_owed_the_cached_increment() {
        let m = MarketDataState::new();
        let instrument = 5;

        assert_eq!(m.min_tick_for_follower(instrument), None, "none before the acknowledgement");
        m.push_tick_req_params(instrument, 0.01);
        assert_eq!(m.min_tick_for_follower(instrument), Some(0.01), "cached from the acknowledgement");

        // The follower is owed it, delivered to that request alone.
        m.push_tick_req_params_for(2, 0.01);
        assert_eq!(m.drain_tick_req_params_direct(), vec![(2, 0.01)]);
        assert!(m.drain_tick_req_params_direct().is_empty(), "taken once");

        m.note_released_slot(instrument, u64::MAX);
        assert_eq!(m.min_tick_for_follower(instrument), None, "cleared when the slot is given back");
    }

    /// Publishing waits for the follower's copy to be ready, so a follower
    /// joining after dispatch has taken its recipients still has an answer.
    #[test]
    fn the_followers_increment_is_ready_before_the_acknowledgement() {
        let market = MarketDataState::new();
        let queued = market.tick_req_params.lock().unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| market.push_tick_req_params(5, 0.025));
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let cached = loop {
                let cached = market.min_tick_for_follower(5);
                if cached.is_some() || std::time::Instant::now() >= deadline {
                    break cached;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            assert!(queued.is_empty());
            drop(queued);
            writer.join().unwrap();
            assert_eq!(cached, Some(0.025), "publication cannot precede the follower's copy");
        });
        assert_eq!(market.drain_tick_req_params(), vec![(5, 0.025)]);
        assert_eq!(market.min_tick_for_follower(5), Some(0.025));
    }
}

#[cfg(test)]
mod depth_backlog_tests {
    use super::*;

    fn entry(req_id: u32) -> DepthUpdate {
        DepthUpdate {
            req_id,
            position: 0,
            market_maker: String::new(),
            operation: 0,
            side: 1,
            price: 1.0,
            size: 1.0,
            is_smart_depth: false,
        }
    }

    /// The book that is dropped is the longest one, not whoever happened to
    /// push at the moment the queue filled.
    ///
    /// Kept apart from the test below because there the flooder is also the
    /// pusher, so dropping the longest and dropping the pusher pick the same
    /// book and neither rule is pinned. Here a caller reading its own small
    /// book is the one that pushes at the bound: taking the pusher would give
    /// up the book that was being read and leave the runaway streaming.
    #[test]
    fn the_longest_book_is_dropped_and_not_the_one_that_pushed() {
        let market = MarketDataState::new();
        let flooding = 7;
        let reading = 9;

        for _ in 0..STREAM_BACKLOG_LIMIT - 1 {
            market.push_depth_update(entry(flooding));
        }
        // Fills the queue exactly, so the next push is the one that walks it.
        market.push_depth_update(entry(reading));
        market.push_depth_update(entry(reading));

        assert!(market.depth_was_dropped(flooding), "the longest book was the one given up");
        assert!(!market.depth_was_dropped(reading), "not the one that pushed");
        assert_eq!(
            market.take_depth_updates_for(reading).len(), 2,
            "and the pusher's own book is whole, the update that triggered it included",
        );
    }

    /// A book that runs away unread is dropped whole, and nothing further is
    /// kept for it until the caller asks again.
    ///
    /// A book only means anything whole: each entry names a position, and
    /// once entries are gone everything after them describes a book that no
    /// longer exists. Kept, those would be handed back reading exactly like a
    /// real book. And the one dropped is the one that ran away, not whoever
    /// happened to push next — they share a queue, and each is drained on its
    /// own.
    #[test]
    fn a_runaway_book_is_dropped_and_not_quietly_restarted() {
        let market = MarketDataState::new();

        // One caller reads nothing; another keeps one entry outstanding.
        let flooding = 7;
        let reading = 9;
        market.push_depth_update(entry(reading));
        for _ in 0..STREAM_BACKLOG_LIMIT {
            market.push_depth_update(entry(flooding));
        }

        assert!(market.depth_was_dropped(flooding), "the book that ran away was given up");
        assert!(!market.depth_was_dropped(reading), "and the one being read was not");
        assert_eq!(
            market.take_depth_updates_for(reading).len(), 1,
            "a caller that was reading keeps its book",
        );

        // Nothing further is kept for the dropped one: a part of a book is
        // not a book.
        market.push_depth_update(entry(flooding));
        assert!(
            market.take_depth_updates_for(flooding).is_empty(),
            "nothing is handed back for a book that was given up",
        );

        // Withdrawing is how it starts again.
        market.purge_depth_updates(flooding);
        assert!(!market.depth_was_dropped(flooding));
        market.push_depth_update(entry(flooding));
        assert_eq!(market.take_depth_updates_for(flooding).len(), 1, "and it does");
    }
}

#[cfg(test)]
mod option_model_tests {
    use super::*;
    use crate::types::OptionComputation;

    /// A book started again does not open with the failure of the one before it.
    ///
    /// Withdrawing is how a caller starts again after a book was given up on,
    /// and the notice of that giving up was left queued. The next subscription
    /// under the same number then opened with it: a healthy book, receiving a
    /// fresh picture, reported as one this client had abandoned.
    #[test]
    fn a_withdrawal_takes_the_notice_of_a_dropped_book_with_it() {
        let market = MarketDataState::new();
        for _ in 0..STREAM_BACKLOG_LIMIT + 1 {
            market.push_depth_update(DepthUpdate {
                req_id: 4, position: 0, market_maker: String::new(), operation: 0,
                side: 1, price: 100.0, size: 1.0, is_smart_depth: false,
            });
        }
        assert!(market.depth_was_dropped(4), "the book was given up on");
        assert!(
            !market.depth_drops_unsaid.lock().unwrap().is_empty(),
            "and the caller has not been told yet",
        );

        market.purge_depth_updates(4);

        assert!(!market.depth_was_dropped(4), "asked for again, it is not refused");
        assert!(
            market.depth_drops_unsaid.lock().unwrap().is_empty(),
            "and the notice of the last one does not open the next",
        );
    }

    /// An answer worked out here does not become the venue's model for a
    /// contract.
    ///
    /// It names no contract — a solve answers a request, not an instrument —
    /// so stored it lands on slot zero, which is a real contract. The next
    /// question about that contract would then be answered against the last
    /// caller's own volatility and price, and against a record saying nothing
    /// about which model the venue used.
    #[test]
    fn a_local_answer_does_not_become_the_venues_model() {
        let market = MarketDataState::new();

        // The venue's own statement for the contract in slot zero.
        market.push_option_computation(OptionComputation {
            instrument: 0,
            implied_vol: 0.25,
            price_based_vol: true,
            ..Default::default()
        });
        // And a caller's question answered here, which names no contract.
        market.push_option_computation(OptionComputation {
            implied_vol: 0.99,
            ..OptionComputation::solved(7)
        });

        let stated = market.option_model(0).expect("the venue's statement stands");
        assert_eq!(stated.implied_vol, 0.25, "the venue's volatility, not the answer's");
        assert!(stated.price_based_vol, "and what it says about the model it used");
    }
}

#[cfg(test)]
mod venue_clock_tests {
    use super::{MarketDataState, local_millis};

    /// A session that has been told nothing about the venue's clock still
    /// answers, with this machine's.
    ///
    /// The question is never put to the venue, so there is nothing to wait
    /// for and nothing to refuse over. Refused, a caller on a connected
    /// session was handed a failure for a call that cannot fail.
    #[test]
    fn an_unstated_clock_answers_this_machines_own() {
        let market = MarketDataState::new();
        assert!(
            (market.venue_time_millis() - local_millis()).abs() < 1_000,
            "no skew is no shift, not no answer",
        );
    }

    /// What the venue states shifts the answer to the venue's clock.
    #[test]
    fn a_stated_clock_shifts_the_answer_onto_it() {
        let market = MarketDataState::new();
        market.note_venue_time("20260815-12:00:00");
        assert!(
            (market.venue_time_millis() - 1_786_795_200_000).abs() < 2_000,
            "the venue's clock, from a stamp days away from this machine's",
        );

        // And stated as a number of its own, which is how the venue pushes it.
        market.note_venue_millis(1_786_795_200_000 + 86_400_000);
        assert!(
            (market.venue_time_millis() - (1_786_795_200_000 + 86_400_000)).abs() < 2_000,
            "the later statement is the one in force",
        );
    }

    /// A clock at the end of what the type holds is clamped, not wrapped.
    ///
    /// The venue states its clock in seconds and it is held in milliseconds, so
    /// a second count near the end of the range does not survive the
    /// conversion. Wrapped, the product came out negative and the session ran a
    /// thousand years behind a clock nobody stated — every stamp this client
    /// puts on a request, and every answer it reads a time off, with it. The
    /// reference conversion saturates and so does this one.
    #[test]
    fn a_clock_past_what_the_type_holds_is_clamped() {
        let market = MarketDataState::new();
        market.note_venue_millis(i64::MAX);
        assert!(
            market.venue_time_millis() > 0,
            "the clock stays a clock, however far out the statement is",
        );
        market.note_venue_millis(i64::MIN);
        assert!(
            market.venue_time_millis() < 0,
            "and the same at the other end, without wrapping back to a future date",
        );
    }

    /// The answer keeps moving while the venue says nothing.
    ///
    /// Held as the stamp on the last message seen, it stood still on a quiet
    /// connection: two readings a moment apart named the same instant, and a
    /// caller measuring the difference between the clocks watched it drift by
    /// exactly the time it had been waiting.
    #[test]
    fn the_answer_advances_on_a_quiet_connection() {
        let market = MarketDataState::new();
        market.note_venue_time("20260815-12:00:00");
        let first = market.venue_time_millis();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            market.venue_time_millis() > first,
            "the clock ran on though the venue stated nothing further",
        );
    }
    /// The point between the two goes in and comes out, like every other
    /// stream beside it.
    #[test]
    fn a_midpoint_pushed_is_a_midpoint_drained() {
        let market = MarketDataState::new();
        market.push_tbt_mid(crate::types::TbtMid {
            instrument: 0, req_id: 7, price: 150_250_000_000, timestamp: 1,
        });
        let out = market.drain_tbt_mids();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].req_id, 7);
        assert!(market.drain_tbt_mids().is_empty(), "and only once");
    }

}
