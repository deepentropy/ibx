use std::collections::HashMap;
use crate::types::{InstrumentId, Price, Qty, Quote, PRICE_SCALE, MAX_INSTRUMENTS};

/// Sentinel conId marking a freed instrument slot (ibx#233). Cannot collide
/// with a real conId (0 occurs in practice for conId-less contracts).
const FREE_SLOT: i64 = i64::MIN;

/// Pre-allocated quote storage indexed by InstrumentId.
/// All quotes live in a contiguous array for cache efficiency.
pub struct MarketState {
    quotes: [Quote; MAX_INSTRUMENTS],
    /// High-water mark: slots ever allocated (iteration bound). Freed slots
    /// below this mark are reused via `free_ids` before new ones are taken,
    /// so the MAX_INSTRUMENTS cap bounds CONCURRENT instruments, not the
    /// session's cumulative total (ibx#233).
    active_count: u32,
    /// Freed slot ids available for reuse.
    free_ids: Vec<InstrumentId>,
    /// Maps IB conId → internal InstrumentId. O(1) lookup.
    con_id_to_instrument: HashMap<i64, InstrumentId>,
    /// Reverse map: InstrumentId → conId. Flat array lookup. `FREE_SLOT`
    /// marks a reclaimed slot.
    instrument_to_con_id: [i64; MAX_INSTRUMENTS],
    /// Maps an IB server_tag → InstrumentId. Keyed rather than indexed: the
    /// tag space is the gateway's to choose and a live session starts well
    /// past any fixed bound. One entry per subscription ack, so an instrument
    /// with several requests holds several; entries are dropped on unregister
    /// and cleared wholesale on farm disconnect.
    server_tag_to_instrument: HashMap<u32, InstrumentId>,
    /// Per-instrument minTick (from 35=Q). Used to scale tick magnitudes to prices.
    min_ticks: [f64; MAX_INSTRUMENTS],
    /// Pre-computed min_tick * PRICE_SCALE as integer for hot-path price conversion.
    min_tick_scaled: [i64; MAX_INSTRUMENTS],
    /// Per-instrument symbol name. Flat array indexed by InstrumentId.
    symbols: [Option<String>; MAX_INSTRUMENTS],
    /// Per-instrument security type (API string, e.g. "STK"/"CASH"). Empty
    /// slot = unknown, treated as stock. Registration used to DROP this
    /// field silently (ibx#217).
    sec_types: [Option<String>; MAX_INSTRUMENTS],
    /// Per-instrument requested exchange. Empty slot = default routing.
    exchanges: [Option<String>; MAX_INSTRUMENTS],
}

impl MarketState {
    pub fn new() -> Self {
        Self {
            quotes: [Quote::default(); MAX_INSTRUMENTS],
            active_count: 0,
            free_ids: Vec::new(),
            con_id_to_instrument: HashMap::new(),
            instrument_to_con_id: [0; MAX_INSTRUMENTS],
            server_tag_to_instrument: HashMap::new(),
            min_ticks: [0.0; MAX_INSTRUMENTS],
            min_tick_scaled: [0; MAX_INSTRUMENTS],
            symbols: std::array::from_fn(|_| None),
            sec_types: std::array::from_fn(|_| None),
            exchanges: std::array::from_fn(|_| None),
        }
    }

    /// Register an IB contract, returns the assigned InstrumentId, or None
    /// when MAX_INSTRUMENTS distinct contracts are live concurrently
    /// (ibx#233). Freed slots are reused first, so unsubscribed contracts
    /// no longer count against the cap.
    pub fn try_register(&mut self, con_id: i64) -> Option<InstrumentId> {
        if let Some(&id) = self.con_id_to_instrument.get(&con_id) {
            return Some(id);
        }
        let id = match self.free_ids.pop() {
            Some(id) => id,
            None => {
                if (self.active_count as usize) >= MAX_INSTRUMENTS {
                    return None;
                }
                let id = self.active_count;
                self.active_count += 1;
                id
            }
        };
        self.con_id_to_instrument.insert(con_id, id);
        self.instrument_to_con_id[id as usize] = con_id;
        Some(id)
    }

    /// Register an IB contract, returns the assigned InstrumentId.
    /// Panics when the table is full — use `try_register` on any path that
    /// must survive that condition (the engine's handlers do; ibx#233).
    pub fn register(&mut self, con_id: i64) -> InstrumentId {
        self.try_register(con_id).expect("too many instruments")
    }

    /// Reclaim an instrument slot (ibx#233): the id becomes reusable by the
    /// next registration. Clears the quote, symbol, tick size, conId maps
    /// and any server tags pointing at the slot. Returns the conId that was
    /// registered, or None if the id is out of range or already free.
    ///
    /// Caller contract: nothing may still reference the id (open orders,
    /// tick-by-tick or news subscriptions) — a reused id would repoint those
    /// references at the wrong contract.
    pub fn unregister(&mut self, instrument: InstrumentId) -> Option<i64> {
        if instrument >= self.active_count {
            return None;
        }
        let con_id = self.instrument_to_con_id[instrument as usize];
        if con_id == FREE_SLOT {
            return None;
        }
        self.con_id_to_instrument.remove(&con_id);
        self.instrument_to_con_id[instrument as usize] = FREE_SLOT;
        self.quotes[instrument as usize] = Quote::default();
        self.symbols[instrument as usize] = None;
        self.sec_types[instrument as usize] = None;
        self.exchanges[instrument as usize] = None;
        self.min_ticks[instrument as usize] = 0.0;
        self.min_tick_scaled[instrument as usize] = 0;
        self.clear_server_tags_for(instrument);
        self.free_ids.push(instrument);
        Some(con_id)
    }

    /// Drop every tag mapped to this instrument. Tags arrive from more than
    /// one exchange — `35=Q` acks for L1 and `35=L` ticker setup for news
    /// routing — so this is only safe where the instrument itself is going
    /// away, not when one of its subscriptions ends.
    pub fn clear_server_tags_for(&mut self, instrument: InstrumentId) {
        self.server_tag_to_instrument.retain(|_, id| *id != instrument);
    }

    /// Map an IB server_tag (from 35=Q subscription ack) to an InstrumentId.
    pub fn register_server_tag(&mut self, server_tag: u32, instrument: InstrumentId) {
        self.server_tag_to_instrument.insert(server_tag, instrument);
    }

    /// Slot iteration bound (high-water mark). Freed slots below this count
    /// exist but hold zeroed data until reused; consumers iterating
    /// `0..count()` read harmless defaults for them.
    pub fn count(&self) -> u32 {
        self.active_count
    }

    /// Iterate over all live (InstrumentId, con_id) pairs, skipping freed slots.
    pub fn active_instruments(&self) -> impl Iterator<Item = (InstrumentId, i64)> + '_ {
        (0..self.active_count)
            .map(move |id| (id, self.instrument_to_con_id[id as usize]))
            .filter(|(_, con_id)| *con_id != FREE_SLOT)
    }

    /// Look up con_id by InstrumentId. O(1) flat array lookup. None for
    /// out-of-range ids and freed slots.
    pub fn con_id(&self, instrument: InstrumentId) -> Option<i64> {
        if instrument < self.active_count {
            let con_id = self.instrument_to_con_id[instrument as usize];
            if con_id != FREE_SLOT { Some(con_id) } else { None }
        } else {
            None
        }
    }

    /// Look up InstrumentId by con_id. O(1) HashMap lookup.
    pub fn instrument_by_con_id(&self, con_id: i64) -> Option<InstrumentId> {
        self.con_id_to_instrument.get(&con_id).copied()
    }

    /// Look up InstrumentId by server_tag. O(1) hash lookup.
    #[inline(always)]
    pub fn instrument_by_server_tag(&self, server_tag: u32) -> Option<InstrumentId> {
        self.server_tag_to_instrument.get(&server_tag).copied()
    }

    /// Set symbol name for an instrument (e.g. "AAPL"). Used for orders.
    pub fn set_symbol(&mut self, id: InstrumentId, symbol: String) {
        self.symbols[id as usize] = Some(symbol);
    }

    /// Record the security type and requested exchange for an instrument
    /// (ibx#217): order encoders derive their routing tags from these
    /// instead of hardcoding stock-on-SMART. Empty strings leave the
    /// defaults in place.
    pub fn set_routing(&mut self, id: InstrumentId, sec_type: &str, exchange: &str) {
        if !sec_type.is_empty() {
            self.sec_types[id as usize] = Some(sec_type.to_uppercase());
        }
        if !exchange.is_empty() {
            self.exchanges[id as usize] = Some(exchange.to_uppercase());
        }
    }

    /// Routing tags for an outbound order on this instrument (ibx#217):
    /// (security type, destination). Rules: the security type is the
    /// instrument's real one (default STK); IBKRATS resolves to IDEALPRO
    /// for CASH and BEST otherwise; CASH without an explicit venue routes
    /// to IDEALPRO; any other explicit non-SMART exchange is respected;
    /// everything else routes BEST — the wire form of default routing.
    /// The reference encoder structurally cannot emit "SMART": it
    /// canonicalizes to it internally and translates to "BEST" at the
    /// encode boundary (ib-agent#165), and sending "SMART" was observed to
    /// produce NO ack at all for pre-market opening-auction orders while
    /// the gateway answers "BEST" in ~130ms (ib-agent#164).
    pub fn order_routing(&self, id: InstrumentId) -> (String, String) {
        let sec_type = self.sec_types[id as usize]
            .clone()
            .unwrap_or_else(|| "STK".to_string());
        let exchange = self.exchanges[id as usize].as_deref().unwrap_or("");
        let is_cash = sec_type == "CASH";
        let destination = match exchange {
            "" | "SMART" | "IBKRATS" | "BEST" => {
                if is_cash { "IDEALPRO" } else { "BEST" }.to_string()
            }
            other => other.to_string(),
        };
        (sec_type, destination)
    }

    /// Get symbol name for an instrument. O(1) flat array lookup.
    pub fn symbol(&self, id: InstrumentId) -> &str {
        self.symbols[id as usize].as_deref().unwrap_or("?")
    }

    /// Set minTick for an instrument (from 35=Q). Price ticks = magnitude * min_tick.
    pub fn set_min_tick(&mut self, id: InstrumentId, min_tick: f64) {
        self.min_ticks[id as usize] = min_tick;
        self.min_tick_scaled[id as usize] = (min_tick * PRICE_SCALE as f64).round() as i64;
    }

    /// Get minTick for an instrument.
    #[inline(always)]
    pub fn min_tick(&self, id: InstrumentId) -> f64 {
        self.min_ticks[id as usize]
    }

    /// Get pre-computed min_tick * PRICE_SCALE for integer price conversion.
    #[inline(always)]
    pub fn min_tick_scaled(&self, id: InstrumentId) -> i64 {
        self.min_tick_scaled[id as usize]
    }

    #[inline(always)]
    pub fn quote(&self, id: InstrumentId) -> &Quote {
        &self.quotes[id as usize]
    }

    #[inline(always)]
    pub fn quote_mut(&mut self, id: InstrumentId) -> &mut Quote {
        &mut self.quotes[id as usize]
    }

    #[inline(always)]
    pub fn bid(&self, id: InstrumentId) -> Price {
        self.quotes[id as usize].bid
    }

    #[inline(always)]
    pub fn ask(&self, id: InstrumentId) -> Price {
        self.quotes[id as usize].ask
    }

    #[inline(always)]
    pub fn last(&self, id: InstrumentId) -> Price {
        self.quotes[id as usize].last
    }

    #[inline(always)]
    pub fn bid_size(&self, id: InstrumentId) -> Qty {
        self.quotes[id as usize].bid_size
    }

    #[inline(always)]
    pub fn ask_size(&self, id: InstrumentId) -> Qty {
        self.quotes[id as usize].ask_size
    }

    #[inline(always)]
    pub fn mid(&self, id: InstrumentId) -> Price {
        let q = &self.quotes[id as usize];
        (q.bid + q.ask) / 2
    }

    #[inline(always)]
    pub fn spread(&self, id: InstrumentId) -> Price {
        let q = &self.quotes[id as usize];
        q.ask - q.bid
    }

    /// Clear server tag mappings (called on farm disconnect — old tags are invalid).
    pub fn clear_server_tags(&mut self) {
        self.server_tag_to_instrument.clear();
    }

    /// Zero all quote data to prevent stale price trading after farm disconnect.
    pub fn zero_all_quotes(&mut self) {
        for i in 0..self.active_count as usize {
            self.quotes[i] = Quote::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PRICE_SCALE;

    #[test]
    fn register_returns_sequential_ids() {
        let mut ms = MarketState::new();
        assert_eq!(ms.register(265598), 0); // AAPL
        assert_eq!(ms.register(272093), 1); // MSFT
        assert_eq!(ms.register(756733), 2); // SPY
    }

    #[test]
    fn server_tags_resolve_far_above_any_fixed_bound() {
        // A live session does not start its tags near zero. These are the
        // first four a real one assigned, and a table sized by the tag space
        // dropped every tick for all of them.
        let mut ms = MarketState::new();
        let nq = ms.register(563947733);
        let mnq = ms.register(497222760);
        ms.register_server_tag(274555, nq);
        ms.register_server_tag(274556, mnq);
        assert_eq!(ms.instrument_by_server_tag(274555), Some(nq));
        assert_eq!(ms.instrument_by_server_tag(274556), Some(mnq));
        assert_eq!(ms.instrument_by_server_tag(274557), None);
        // Tag reuse after a farm reconnect must not resurrect the old mapping.
        ms.clear_server_tags();
        assert_eq!(ms.instrument_by_server_tag(274555), None);
    }

    #[test]
    fn clearing_tags_for_one_instrument_leaves_it_registered() {
        // An instrument pinned by an open order, tbt or news subscription
        // outlives its L1 requests. Its tags are dead the moment the last one
        // is unsubscribed, and slot reclamation — the only other thing that
        // drops them — will not run while it is pinned.
        let mut ms = MarketState::new();
        let a = ms.register(1111);
        let b = ms.register(2222);
        ms.register_server_tag(400000, a);
        ms.register_server_tag(400001, a);
        ms.register_server_tag(400002, b);

        ms.clear_server_tags_for(a);
        assert_eq!(ms.instrument_by_server_tag(400000), None);
        assert_eq!(ms.instrument_by_server_tag(400001), None);
        assert_eq!(ms.instrument_by_server_tag(400002), Some(b), "another instrument keeps its own");
        assert_eq!(ms.con_id(a), Some(1111), "the instrument itself stays registered");
    }

    #[test]
    fn unregister_drops_only_its_own_server_tags() {
        let mut ms = MarketState::new();
        let a = ms.register(1111);
        let b = ms.register(2222);
        // One instrument can hold several tags — a request per subscription
        // ack. Unregister has to drop all of them, not just the first found.
        ms.register_server_tag(300000, a);
        ms.register_server_tag(300002, a);
        ms.register_server_tag(300001, b);
        ms.unregister(a);
        assert_eq!(ms.instrument_by_server_tag(300000), None);
        assert_eq!(ms.instrument_by_server_tag(300002), None, "a leftover tag repaints the next contract that reuses the slot");
        assert_eq!(ms.instrument_by_server_tag(300001), Some(b));
    }

    #[test]
    fn register_same_conid_returns_same_id() {
        let mut ms = MarketState::new();
        let id1 = ms.register(265598);
        let id2 = ms.register(265598);
        assert_eq!(id1, id2);
    }

    #[test]
    fn quote_default_is_zero() {
        let ms = MarketState::new();
        let q = ms.quote(0);
        assert_eq!(q.bid, 0);
        assert_eq!(q.ask, 0);
        assert_eq!(q.last, 0);
    }

    #[test]
    fn update_quote_and_read_back() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.bid = 150 * PRICE_SCALE;
        q.ask = 15010 * (PRICE_SCALE / 100);
        q.last = 15005 * (PRICE_SCALE / 100);

        assert_eq!(ms.bid(id), 150 * PRICE_SCALE);
        assert_eq!(ms.ask(id), 15010 * (PRICE_SCALE / 100));
        assert_eq!(ms.last(id), 15005 * (PRICE_SCALE / 100));
    }

    #[test]
    fn bid_ask_size() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.bid_size = 500;
        q.ask_size = 300;

        assert_eq!(ms.bid_size(id), 500);
        assert_eq!(ms.ask_size(id), 300);
    }

    #[test]
    fn mid_price() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.bid = 100 * PRICE_SCALE;
        q.ask = 102 * PRICE_SCALE;

        // Mid = (100 + 102) / 2 = 101
        assert_eq!(ms.mid(id), 101 * PRICE_SCALE);
    }

    #[test]
    fn spread_calculation() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.bid = 15000 * (PRICE_SCALE / 100);
        q.ask = 15010 * (PRICE_SCALE / 100);

        // Spread = 150.10 - 150.00 = 0.10
        assert_eq!(ms.spread(id), 10 * (PRICE_SCALE / 100));
    }

    #[test]
    fn multiple_instruments_independent() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        let msft = ms.register(272093);

        ms.quote_mut(aapl).bid = 150 * PRICE_SCALE;
        ms.quote_mut(msft).bid = 400 * PRICE_SCALE;

        assert_eq!(ms.bid(aapl), 150 * PRICE_SCALE);
        assert_eq!(ms.bid(msft), 400 * PRICE_SCALE);
    }

    #[test]
    #[should_panic(expected = "too many instruments")]
    fn register_overflow_panics() {
        let mut ms = MarketState::new();
        for i in 0..=MAX_INSTRUMENTS as i64 {
            ms.register(i);
        }
    }

    // ── ibx#217: routing derivation ──

    #[test]
    fn order_routing_rules() {
        let mut ms = MarketState::new();
        let stk = ms.register(1);
        // Unset routing = the historical defaults: a stock on SMART.
        assert_eq!(ms.order_routing(stk), ("STK".into(), "BEST".into()));

        // The registered security type is what goes on the wire.
        let fx = ms.register(2);
        ms.set_routing(fx, "CASH", "");
        assert_eq!(ms.order_routing(fx), ("CASH".into(), "IDEALPRO".into()));

        // IBKRATS resolves to IDEALPRO for CASH, SMART otherwise.
        let fx2 = ms.register(3);
        ms.set_routing(fx2, "CASH", "IBKRATS");
        assert_eq!(ms.order_routing(fx2), ("CASH".into(), "IDEALPRO".into()));
        let stk2 = ms.register(4);
        ms.set_routing(stk2, "STK", "IBKRATS");
        assert_eq!(ms.order_routing(stk2), ("STK".into(), "BEST".into()));

        // An explicit directed exchange is respected.
        let directed = ms.register(5);
        ms.set_routing(directed, "STK", "NYSE");
        assert_eq!(ms.order_routing(directed), ("STK".into(), "NYSE".into()));

        // Unregister clears the routing for slot reuse.
        ms.unregister(fx);
        let reused = ms.register(6);
        assert_eq!(reused, fx);
        assert_eq!(ms.order_routing(reused), ("STK".into(), "BEST".into()));
    }

    // ── ibx#233: unregister + slot reuse ──

    #[test]
    fn try_register_full_returns_none_not_panic() {
        let mut ms = MarketState::new();
        for i in 0..MAX_INSTRUMENTS as i64 {
            assert!(ms.try_register(i).is_some());
        }
        assert_eq!(ms.try_register(9999), None, "full table must reject, not panic");
        // Existing conIds still resolve at the cap.
        assert!(ms.try_register(0).is_some());
    }

    #[test]
    fn unregister_frees_slot_for_reuse() {
        let mut ms = MarketState::new();
        let a = ms.register(100);
        let b = ms.register(200);
        let c = ms.register(300);
        assert_eq!(ms.unregister(b), Some(200));
        // Freed id is reused before a new slot is taken.
        let d = ms.try_register(400).unwrap();
        assert_eq!(d, b);
        assert_eq!(ms.con_id(d), Some(400));
        assert_eq!(ms.instrument_by_con_id(200), None, "old conId must not resolve");
        assert_eq!(ms.con_id(a), Some(100));
        assert_eq!(ms.con_id(c), Some(300));
    }

    #[test]
    fn cap_bounds_concurrent_not_cumulative() {
        // The ibx#233 watchlist scenario: cycle far more than MAX_INSTRUMENTS
        // distinct contracts through one session, one live at a time.
        let mut ms = MarketState::new();
        for i in 0..(MAX_INSTRUMENTS as i64 * 4) {
            let id = ms.try_register(1000 + i).expect("cycling one instrument must never exhaust the table");
            assert_eq!(ms.unregister(id), Some(1000 + i));
        }
        assert_eq!(ms.active_instruments().count(), 0);
    }

    #[test]
    fn unregister_clears_slot_state() {
        let mut ms = MarketState::new();
        let id = ms.register(100);
        ms.set_symbol(id, "AAPL".to_string());
        ms.set_min_tick(id, 0.01);
        ms.register_server_tag(42, id);
        ms.quote_mut(id).bid = 150 * PRICE_SCALE;

        assert_eq!(ms.unregister(id), Some(100));
        assert_eq!(ms.symbol(id), "?");
        assert_eq!(ms.min_tick(id), 0.0);
        assert_eq!(ms.min_tick_scaled(id), 0);
        assert_eq!(ms.instrument_by_server_tag(42), None);
        assert_eq!(ms.quote(id).bid, 0, "stale quote must not survive into a reused slot");
    }

    #[test]
    fn unregister_unknown_or_freed_returns_none() {
        let mut ms = MarketState::new();
        assert_eq!(ms.unregister(0), None, "never-registered id");
        let id = ms.register(100);
        assert_eq!(ms.unregister(id), Some(100));
        assert_eq!(ms.unregister(id), None, "double unregister");
        assert_eq!(ms.unregister(250), None, "out of range");
    }

    #[test]
    fn active_instruments_skips_freed_slots() {
        let mut ms = MarketState::new();
        let a = ms.register(100);
        let b = ms.register(200);
        let c = ms.register(300);
        ms.unregister(b);
        let live: Vec<_> = ms.active_instruments().collect();
        assert_eq!(live, vec![(a, 100), (c, 300)]);
        // count() stays the iteration bound (high-water), not the live count.
        assert_eq!(ms.count(), 3);
    }

    #[test]
    fn server_tag_mapping() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        ms.register_server_tag(42, aapl);
        assert_eq!(ms.instrument_by_server_tag(42), Some(aapl));
        assert_eq!(ms.instrument_by_server_tag(99), None);
    }

    #[test]
    fn server_tag_overwrite() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        ms.register_server_tag(42, aapl);
        ms.register_server_tag(42, aapl); // same value, overwrites
        assert_eq!(ms.instrument_by_server_tag(42), Some(aapl));
    }

    #[test]
    fn min_tick_default_zero() {
        let ms = MarketState::new();
        assert_eq!(ms.min_tick(0), 0.0);
    }

    #[test]
    fn min_tick_set_and_get() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_min_tick(id, 0.01);
        assert!((ms.min_tick(id) - 0.01).abs() < 1e-10);
    }

    // --- instrument_by_con_id ---

    #[test]
    fn instrument_by_con_id_found() {
        let mut ms = MarketState::new();
        ms.register(265598);
        assert_eq!(ms.instrument_by_con_id(265598), Some(0));
    }

    #[test]
    fn instrument_by_con_id_not_found() {
        let ms = MarketState::new();
        assert_eq!(ms.instrument_by_con_id(999999), None);
    }

    #[test]
    fn instrument_by_con_id_multiple() {
        let mut ms = MarketState::new();
        ms.register(265598);
        ms.register(272093);
        ms.register(756733);
        assert_eq!(ms.instrument_by_con_id(272093), Some(1));
        assert_eq!(ms.instrument_by_con_id(756733), Some(2));
    }

    // --- active_instruments ---

    #[test]
    fn active_instruments_empty() {
        let ms = MarketState::new();
        assert_eq!(ms.active_instruments().count(), 0);
    }

    #[test]
    fn active_instruments_returns_all() {
        let mut ms = MarketState::new();
        ms.register(265598);
        ms.register(272093);
        ms.register(756733);
        let active: Vec<_> = ms.active_instruments().collect();
        assert_eq!(active.len(), 3);
        assert_eq!(active[0], (0, 265598));
        assert_eq!(active[1], (1, 272093));
        assert_eq!(active[2], (2, 756733));
    }

    #[test]
    fn active_instruments_iterable_twice() {
        let mut ms = MarketState::new();
        ms.register(265598);
        let first: Vec<_> = ms.active_instruments().collect();
        let second: Vec<_> = ms.active_instruments().collect();
        assert_eq!(first, second);
    }

    // --- Multiple server tags ---

    #[test]
    fn multiple_server_tags_different_instruments() {
        let mut ms = MarketState::new();
        let a = ms.register(265598);
        let b = ms.register(272093);
        ms.register_server_tag(10, a);
        ms.register_server_tag(20, b);
        assert_eq!(ms.instrument_by_server_tag(10), Some(a));
        assert_eq!(ms.instrument_by_server_tag(20), Some(b));
    }

    // --- Quote OHLCV fields ---

    #[test]
    fn quote_ohlcv_fields() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.open = 148 * PRICE_SCALE;
        q.high = 155 * PRICE_SCALE;
        q.low = 147 * PRICE_SCALE;
        q.close = 152 * PRICE_SCALE;
        q.volume = 50_000_000;
        q.timestamp_ns = 1709654400_000_000_000;

        let q_ref = ms.quote(id);
        assert_eq!(q_ref.open, 148 * PRICE_SCALE);
        assert_eq!(q_ref.high, 155 * PRICE_SCALE);
        assert_eq!(q_ref.low, 147 * PRICE_SCALE);
        assert_eq!(q_ref.close, 152 * PRICE_SCALE);
        assert_eq!(q_ref.volume, 50_000_000);
        assert_eq!(q_ref.timestamp_ns, 1709654400_000_000_000);
    }

    // --- Spread edge cases ---

    #[test]
    fn spread_with_zero_bid_ask() {
        let ms = MarketState::new();
        // Before any data, spread is 0
        assert_eq!(ms.spread(0), 0);
    }

    #[test]
    fn mid_with_odd_spread() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        let q = ms.quote_mut(id);
        q.bid = 99;
        q.ask = 100;
        // Mid = (99 + 100) / 2 = 99 (integer division truncates)
        assert_eq!(ms.mid(id), 99);
    }

    // --- Min tick for different instruments ---

    #[test]
    fn min_tick_per_instrument() {
        let mut ms = MarketState::new();
        let a = ms.register(265598);
        let b = ms.register(272093);
        ms.set_min_tick(a, 0.01);
        ms.set_min_tick(b, 0.05);
        assert!((ms.min_tick(a) - 0.01).abs() < 1e-10);
        assert!((ms.min_tick(b) - 0.05).abs() < 1e-10);
    }

    // --- clear_server_tags ---

    #[test]
    fn clear_server_tags_removes_all() {
        let mut ms = MarketState::new();
        let a = ms.register(265598);
        let b = ms.register(272093);
        ms.register_server_tag(10, a);
        ms.register_server_tag(20, b);
        ms.clear_server_tags();
        assert_eq!(ms.instrument_by_server_tag(10), None);
        assert_eq!(ms.instrument_by_server_tag(20), None);
    }

    // --- zero_all_quotes ---

    #[test]
    fn zero_all_quotes_clears_active() {
        let mut ms = MarketState::new();
        let a = ms.register(265598);
        let b = ms.register(272093);
        ms.quote_mut(a).bid = 150 * PRICE_SCALE;
        ms.quote_mut(a).ask = 151 * PRICE_SCALE;
        ms.quote_mut(b).last = 400 * PRICE_SCALE;
        ms.zero_all_quotes();
        assert_eq!(ms.bid(a), 0);
        assert_eq!(ms.ask(a), 0);
        assert_eq!(ms.last(b), 0);
    }

    #[test]
    fn zero_all_quotes_no_registered_is_noop() {
        let mut ms = MarketState::new();
        ms.zero_all_quotes(); // should not panic
    }
}
