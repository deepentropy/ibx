use crate::types::{InstrumentId, Price, Qty, Quote, MAX_INSTRUMENTS};

/// Pre-allocated quote storage indexed by InstrumentId.
/// All quotes live in a contiguous array for cache efficiency.
pub struct MarketState {
    quotes: [Quote; MAX_INSTRUMENTS],
    /// Number of active instruments (for iteration bounds).
    active_count: u32,
    /// Maps IB conId → internal InstrumentId.
    con_id_to_instrument: Vec<(i64, InstrumentId)>,
    /// Maps IB server_tag (from 35=Q/35=L) → InstrumentId.
    server_tag_to_instrument: Vec<(u32, InstrumentId)>,
    /// Per-instrument minTick (from 35=Q). Used to scale tick magnitudes to prices.
    min_ticks: [f64; MAX_INSTRUMENTS],
}

impl MarketState {
    pub fn new() -> Self {
        Self {
            quotes: [Quote::default(); MAX_INSTRUMENTS],
            active_count: 0,
            con_id_to_instrument: Vec::new(),
            server_tag_to_instrument: Vec::new(),
            min_ticks: [0.0; MAX_INSTRUMENTS],
        }
    }

    /// Register an IB contract, returns the assigned InstrumentId.
    pub fn register(&mut self, con_id: i64) -> InstrumentId {
        // Check if already registered
        for &(cid, iid) in &self.con_id_to_instrument {
            if cid == con_id {
                return iid;
            }
        }
        let id = self.active_count;
        assert!((id as usize) < MAX_INSTRUMENTS, "too many instruments");
        self.con_id_to_instrument.push((con_id, id));
        self.active_count += 1;
        id
    }

    /// Map an IB server_tag (from 35=Q subscription ack) to an InstrumentId.
    pub fn register_server_tag(&mut self, server_tag: u32, instrument: InstrumentId) {
        for &(st, _) in &self.server_tag_to_instrument {
            if st == server_tag {
                return; // already registered
            }
        }
        self.server_tag_to_instrument.push((server_tag, instrument));
    }

    /// Look up InstrumentId by con_id. Returns None if not registered.
    pub fn instrument_by_con_id(&self, con_id: i64) -> Option<InstrumentId> {
        for &(cid, iid) in &self.con_id_to_instrument {
            if cid == con_id {
                return Some(iid);
            }
        }
        None
    }

    /// Look up InstrumentId by server_tag. Returns None if not registered.
    pub fn instrument_by_server_tag(&self, server_tag: u32) -> Option<InstrumentId> {
        for &(st, iid) in &self.server_tag_to_instrument {
            if st == server_tag {
                return Some(iid);
            }
        }
        None
    }

    /// Set minTick for an instrument (from 35=Q). Price ticks = magnitude * min_tick.
    pub fn set_min_tick(&mut self, id: InstrumentId, min_tick: f64) {
        self.min_ticks[id as usize] = min_tick;
    }

    /// Get minTick for an instrument.
    #[inline(always)]
    pub fn min_tick(&self, id: InstrumentId) -> f64 {
        self.min_ticks[id as usize]
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
    fn server_tag_dedup() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        ms.register_server_tag(42, aapl);
        ms.register_server_tag(42, aapl); // duplicate
        assert_eq!(ms.server_tag_to_instrument.len(), 1);
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
}
