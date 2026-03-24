use std::collections::HashMap;
use crate::types::{InstrumentId, Price, Qty, Quote, PRICE_SCALE, MAX_INSTRUMENTS};

/// Max server_tag value for flat lookup array. Server tags are small integers from IB.
const MAX_SERVER_TAG: usize = 1024;

/// Sentinel value for empty server_tag slots.
const NO_INSTRUMENT: u32 = u32::MAX;

/// Pre-allocated quote storage indexed by InstrumentId.
/// All quotes live in a contiguous array for cache efficiency.
pub struct MarketState {
    quotes: [Quote; MAX_INSTRUMENTS],
    /// Number of active instruments (for iteration bounds).
    active_count: u32,
    /// Maps IB conId → internal InstrumentId. O(1) lookup.
    con_id_to_instrument: HashMap<i64, InstrumentId>,
    /// Reverse map: InstrumentId → conId. Flat array lookup.
    instrument_to_con_id: [i64; MAX_INSTRUMENTS],
    /// O(1) server_tag → InstrumentId lookup. Server tags are small integers.
    server_tag_table: Box<[u32; MAX_SERVER_TAG]>,
    /// Per-instrument minTick (from 35=Q). Used to scale tick magnitudes to prices.
    min_ticks: [f64; MAX_INSTRUMENTS],
    /// Pre-computed min_tick * PRICE_SCALE as integer for hot-path price conversion.
    min_tick_scaled: [i64; MAX_INSTRUMENTS],
    /// Per-instrument symbol name. Flat array indexed by InstrumentId.
    symbols: [Option<String>; MAX_INSTRUMENTS],
}

impl MarketState {
    pub fn new() -> Self {
        Self {
            quotes: [Quote::default(); MAX_INSTRUMENTS],
            active_count: 0,
            con_id_to_instrument: HashMap::new(),
            instrument_to_con_id: [0; MAX_INSTRUMENTS],
            server_tag_table: Box::new([NO_INSTRUMENT; MAX_SERVER_TAG]),
            min_ticks: [0.0; MAX_INSTRUMENTS],
            min_tick_scaled: [0; MAX_INSTRUMENTS],
            symbols: std::array::from_fn(|_| None),
        }
    }

    /// Register an IB contract, returns the assigned InstrumentId.
    pub fn register(&mut self, con_id: i64) -> InstrumentId {
        if let Some(&id) = self.con_id_to_instrument.get(&con_id) {
            return id;
        }
        let id = self.active_count;
        assert!((id as usize) < MAX_INSTRUMENTS, "too many instruments");
        self.con_id_to_instrument.insert(con_id, id);
        self.instrument_to_con_id[id as usize] = con_id;
        self.active_count += 1;
        id
    }

    /// Map an IB server_tag (from 35=Q subscription ack) to an InstrumentId.
    pub fn register_server_tag(&mut self, server_tag: u32, instrument: InstrumentId) {
        if (server_tag as usize) < MAX_SERVER_TAG {
            self.server_tag_table[server_tag as usize] = instrument;
        }
    }

    /// Number of registered instruments.
    pub fn count(&self) -> u32 {
        self.active_count
    }

    /// Iterate over all registered (InstrumentId, con_id) pairs.
    pub fn active_instruments(&self) -> impl Iterator<Item = (InstrumentId, i64)> + '_ {
        (0..self.active_count).map(move |id| (id, self.instrument_to_con_id[id as usize]))
    }

    /// Look up con_id by InstrumentId. O(1) flat array lookup.
    pub fn con_id(&self, instrument: InstrumentId) -> Option<i64> {
        if instrument < self.active_count {
            Some(self.instrument_to_con_id[instrument as usize])
        } else {
            None
        }
    }

    /// Look up InstrumentId by con_id. O(1) HashMap lookup.
    pub fn instrument_by_con_id(&self, con_id: i64) -> Option<InstrumentId> {
        self.con_id_to_instrument.get(&con_id).copied()
    }

    /// Look up InstrumentId by server_tag. O(1) flat array lookup.
    #[inline(always)]
    pub fn instrument_by_server_tag(&self, server_tag: u32) -> Option<InstrumentId> {
        if (server_tag as usize) < MAX_SERVER_TAG {
            let id = self.server_tag_table[server_tag as usize];
            if id != NO_INSTRUMENT { Some(id) } else { None }
        } else {
            None
        }
    }

    /// Set symbol name for an instrument (e.g. "AAPL"). Used for orders.
    pub fn set_symbol(&mut self, id: InstrumentId, symbol: String) {
        self.symbols[id as usize] = Some(symbol);
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
        self.server_tag_table.fill(NO_INSTRUMENT);
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
