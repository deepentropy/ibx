use std::collections::HashMap;
use crate::types::{InstrumentId, Price, Qty, Quote, PRICE_SCALE, MAX_INSTRUMENTS};

/// Sentinel conId marking a freed instrument slot. Cannot collide
/// with a real conId (0 occurs in practice for conId-less contracts).
const FREE_SLOT: i64 = i64::MIN;

/// Descriptor-field match: a field the caller left blank, or one the slot has
/// never had set, does not distinguish two contracts. `set_routing` stores
/// these uppercased, so the comparison ignores case.
fn blank_or_eq(stored: &Option<String>, incoming: &str) -> bool {
    incoming.is_empty() || stored.as_ref().is_none_or(|s| s.eq_ignore_ascii_case(incoming))
}

/// The contract fields an order restates beyond its symbol.
///
/// The trading class and local symbol are what tell one contract in a family
/// from another where the maturity does not: two futures on the same underlying
/// and month differ by them and by nothing else the order carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderIdentity {
    pub expiry: String,
    pub strike: String,
    pub right: String,
    pub multiplier: String,
    pub trading_class: String,
    pub local_symbol: String,
    /// What the contract is priced in. Empty when the key does not say.
    pub currency: String,
}

/// The trade time the venue last stated as a base for one instrument, in
/// seconds, and how far past it the last offset stood. The venue states the
/// two apart, per stream, and they add; a base replaces the offset with
/// nothing. Held once for the connection instead, a base stated for one
/// stream was what the next offset on every other added to.
#[derive(Clone, Copy, Default)]
pub(crate) struct TradeClock {
    pub base_secs: u64,
    pub offset_secs: u64,
}

/// Pre-allocated quote storage indexed by InstrumentId.
/// All quotes live in a contiguous array for cache efficiency.
pub struct MarketState {
    quotes: Box<[Quote]>,
    /// Per-instrument, alongside the quote it stamps.
    clocks: Box<[TradeClock]>,
    /// High-water mark: slots ever allocated (iteration bound). Freed slots
    /// below this mark are reused via `free_ids` before new ones are taken,
    /// so the MAX_INSTRUMENTS cap bounds CONCURRENT instruments, not the
    /// session's cumulative total.
    active_count: u32,
    /// Freed slot ids available for reuse.
    free_ids: Vec<InstrumentId>,
    /// Maps IB conId → internal InstrumentId. O(1) lookup.
    con_id_to_instrument: HashMap<i64, InstrumentId>,
    /// Reverse map: InstrumentId → conId. Flat array lookup. `FREE_SLOT`
    /// marks a reclaimed slot.
    instrument_to_con_id: Box<[i64]>,
    /// Maps an IB server_tag → InstrumentId. Keyed rather than indexed: the
    /// tag space is the gateway's to choose and a live session starts well
    /// past any fixed bound. One entry per subscription ack, so an instrument
    /// with several requests holds several; entries are dropped on unregister
    /// and cleared wholesale on farm disconnect.
    server_tag_to_instrument: HashMap<u32, InstrumentId>,
    /// Per-instrument minTick (from 35=Q). Used to scale tick magnitudes to prices.
    min_ticks: Box<[f64]>,
    /// Pre-computed min_tick * PRICE_SCALE as integer for hot-path price conversion.
    min_tick_scaled: Box<[i64]>,
    /// Per-instrument size increment, stated on the same acknowledgement as
    /// the price increment. A size on the wire is a count of these, whole ones
    /// for a share and hundred-millionths for a crypto, so a fixed count
    /// reports one of the two wrongly. Zero means the venue stated none, which
    /// is what counting in whole ones means.
    size_ticks: Box<[f64]>,
    /// Per-instrument symbol name. Flat array indexed by InstrumentId.
    symbols: Box<[Option<String>]>,
    /// Per-instrument security type (API string, e.g. `STK`, `CASH`). An empty
    /// slot is unknown and treated as a stock. Carried through registration.
    sec_types: Box<[Option<String>]>,
    /// Per-instrument requested exchange. Empty slot = default routing.
    exchanges: Box<[Option<String>]>,
    /// What separates two conId-less contracts on the same underlying: expiry,
    /// strike, right, multiplier, joined. Symbol, security type and exchange
    /// are equal for an option's call and put at different strikes, so matching
    /// on those alone put both in one slot — the second contract's quotes and
    /// its minTick landed on the first, and minTick is what snaps an order's
    /// price. Empty for anything that carries no such identity.
    option_keys: Box<[Option<String>]>,
    /// What narrowed the lookup that will name a conId-less contract, where
    /// the caller stated anything: where it is listed, the venue's own name
    /// for it, which class of the chain, the identifier it was asked for by.
    ///
    /// None of these is part of what the contract is, so none belongs in the
    /// identity beside it — but each of them decides which listing the venue
    /// answers with, and two descriptions the venue would answer differently
    /// are not one contract. Left out, the second description followed the
    /// first one's subscription and was served the other listing's prices.
    narrowings: Box<[Option<String>]>,
}

impl Default for MarketState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketState {
    pub fn new() -> Self {
        Self {
            quotes: vec![Quote::default(); MAX_INSTRUMENTS].into(),
            clocks: vec![TradeClock::default(); MAX_INSTRUMENTS].into(),
            active_count: 0,
            free_ids: Vec::new(),
            con_id_to_instrument: HashMap::new(),
            instrument_to_con_id: vec![0; MAX_INSTRUMENTS].into(),
            server_tag_to_instrument: HashMap::new(),
            min_ticks: vec![0.0; MAX_INSTRUMENTS].into(),
            min_tick_scaled: vec![0; MAX_INSTRUMENTS].into(),
            size_ticks: vec![0.0; MAX_INSTRUMENTS].into(),
            symbols: vec![None; MAX_INSTRUMENTS].into(),
            sec_types: vec![None; MAX_INSTRUMENTS].into(),
            exchanges: vec![None; MAX_INSTRUMENTS].into(),
            option_keys: vec![None; MAX_INSTRUMENTS].into(),
            narrowings: vec![None; MAX_INSTRUMENTS].into(),
        }
    }

    /// Register an IB contract by conId, returns the assigned InstrumentId, or
    /// None when MAX_INSTRUMENTS distinct contracts are live concurrently. Freed slots
    /// are reused first, so unsubscribed contracts
    /// no longer count against the cap. Callers holding a contract that may
    /// carry no conId want `try_register_contract`.
    pub fn try_register(&mut self, con_id: i64) -> Option<InstrumentId> {
        if let Some(&id) = self.con_id_to_instrument.get(&con_id) {
            return Some(id);
        }
        let id = self.alloc_slot()?;
        self.con_id_to_instrument.insert(con_id, id);
        self.instrument_to_con_id[id as usize] = con_id;
        Some(id)
    }

    /// Give a slot the contract id a lookup found for it.
    ///
    /// A caller who names a contract by symbol gets a slot with no id, and the
    /// venue answers a market data subscription only when it is named by id.
    /// The lookup that resolves it lands here. A slot that already has an id
    /// keeps it, and an id already held by another slot is not stolen: either
    /// would leave two slots claiming one contract.
    pub fn adopt_con_id(&mut self, id: InstrumentId, con_id: i64) -> bool {
        // Bounded by the number of slots handed out, not by the array size.
        // An unallocated slot is still within the arrays, and adopting one
        // writes a contract the count, the live list and the reverse map
        // cannot see.
        if con_id == 0 || id >= self.active_count {
            return false;
        }
        if self.instrument_to_con_id[id as usize] != 0 {
            return false;
        }
        if self.con_id_to_instrument.contains_key(&con_id) {
            return false;
        }
        self.con_id_to_instrument.insert(con_id, id);
        self.instrument_to_con_id[id as usize] = con_id;
        true
    }

    /// Register a client-supplied contract, which may carry no conId. `0` is
    /// not an identity: keyed on it, every contract specified the
    /// ordinary way — symbol, secType, exchange — collapses into the slot the
    /// first one took, and orders on it go out under that contract's symbol.
    /// Match the descriptor across the live slots instead, and leave `0` out
    /// of the conId map.
    ///
    /// A field the caller left blank matches whatever the slot holds:
    /// tick-by-tick and news register with neither secType nor exchange, and
    /// must land on the slot the L1 subscription for that symbol already has.
    pub fn try_register_contract(
        &mut self, con_id: i64, symbol: &str, sec_type: &str, exchange: &str, option_key: &str,
    ) -> Option<InstrumentId> {
        self.try_register_described(con_id, symbol, sec_type, exchange, option_key, "")
    }

    /// The same, for a caller that also stated what narrows the lookup naming
    /// the contract — see [`MarketState::narrowings`].
    pub fn try_register_described(
        &mut self, con_id: i64, symbol: &str, sec_type: &str, exchange: &str, option_key: &str,
        narrowing: &str,
    ) -> Option<InstrumentId> {
        if con_id != 0 {
            let id = self.try_register(con_id)?;
            // The caller stated what this contract is; recording only its
            // option key and dropping the rest left every order on it going out
            // as a stock on the default venue, because that is what an unset
            // security type reads as.
            // Tests the stored value rather than the accessor, which
            // substitutes a placeholder for an unregistered symbol.
            if !symbol.is_empty() && self.symbols[id as usize].is_none() {
                self.set_symbol(id, symbol.to_string());
            }
            self.set_routing(id, sec_type, exchange);
            // A conId names the contract to the gateway, but an order still has
            // to restate the identity on the wire, and this is where an order
            // reads it from. Recorded here too, or a future known by conId went
            // out naming its exchange and not its month.
            if !option_key.is_empty() && self.option_keys[id as usize].is_none() {
                self.option_keys[id as usize] = Some(option_key.to_string());
            }
            return Some(id);
        }
        if let Some(id) =
            self.instrument_by_descriptor(symbol, sec_type, exchange, option_key, narrowing)
        {
            // First caller to state an identity fixes it, so the next contract
            // on the same underlying no longer matches this slot.
            if !option_key.is_empty() && self.option_keys[id as usize].is_none() {
                self.option_keys[id as usize] = Some(option_key.to_string());
            }
            if !narrowing.is_empty() && self.narrowings[id as usize].is_none() {
                self.narrowings[id as usize] = Some(narrowing.to_string());
            }
            return Some(id);
        }
        let id = self.alloc_slot()?;
        self.instrument_to_con_id[id as usize] = 0;
        self.option_keys[id as usize] =
            if option_key.is_empty() { None } else { Some(option_key.to_string()) };
        self.narrowings[id as usize] =
            if narrowing.is_empty() { None } else { Some(narrowing.to_string()) };
        // The descriptor this slot holds. Without it the slot matches no
        // descriptor and the same contract requested twice takes two slots.
        if !symbol.is_empty() {
            self.set_symbol(id, symbol.to_string());
        }
        self.set_routing(id, sec_type, exchange);
        Some(id)
    }

    /// The live conId-less slot this descriptor names, if it has one. Bounded
    /// by MAX_INSTRUMENTS and only ever reached from a control command, so a
    /// scan costs less than the map it would otherwise need.
    fn instrument_by_descriptor(
        &self, symbol: &str, sec_type: &str, exchange: &str, option_key: &str, narrowing: &str,
    ) -> Option<InstrumentId> {
        (0..self.active_count).find(|&id| {
            let i = id as usize;
            self.instrument_to_con_id[i] == 0
                && self.symbols[i].as_deref().unwrap_or("").eq_ignore_ascii_case(symbol)
                && blank_or_eq(&self.sec_types[i], sec_type)
                && blank_or_eq(&self.exchanges[i], exchange)
                // A slot that has no identity yet matches anything and adopts
                // the caller's below: the pre-flight registration carries none,
                // so requiring an exact match there would strand its slot and
                // allocate a second one for the same contract. A slot that does
                // have one must match exactly, which is what keeps the call and
                // the put apart.
                && match self.option_keys[i].as_deref() {
                    None | Some("") => true,
                    Some(k) => k == option_key,
                }
                // And what narrowed the lookup, on the same terms. A slot that
                // states none adopts the caller's; a slot that states one is
                // the answer to that description and to no other.
                && match self.narrowings[i].as_deref() {
                    None | Some("") => true,
                    Some(n) => n == narrowing,
                }
        })
    }

    /// Take a slot: a reclaimed one first, else the next unused one, else None
    /// at the cap.
    fn alloc_slot(&mut self) -> Option<InstrumentId> {
        if let Some(id) = self.free_ids.pop() {
            return Some(id);
        }
        if (self.active_count as usize) >= MAX_INSTRUMENTS {
            return None;
        }
        let id = self.active_count;
        self.active_count += 1;
        Some(id)
    }

    /// Register an IB contract, returns the assigned InstrumentId.
    /// Panics when the table is full — use `try_register` on any path that
    /// must survive that condition (the engine's handlers do;).
    pub fn register(&mut self, con_id: i64) -> InstrumentId {
        self.try_register(con_id).expect("too many instruments")
    }

    /// Reclaim an instrument slot: the id becomes reusable by the
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
        self.clocks[instrument as usize] = TradeClock::default();
        self.symbols[instrument as usize] = None;
        self.sec_types[instrument as usize] = None;
        self.exchanges[instrument as usize] = None;
        self.min_ticks[instrument as usize] = 0.0;
        self.min_tick_scaled[instrument as usize] = 0;
        self.size_ticks[instrument as usize] = 0.0;
        // The contract identity goes with the rest of the slot: a call left
        // behind here would match the put that reused it.
        self.option_keys[instrument as usize] = None;
        self.narrowings[instrument as usize] = None;
        self.clear_server_tags_for(instrument);
        self.free_ids.push(instrument);
        Some(con_id)
    }

    /// Drop every tag mapped to this instrument. Tags arrive from more than
    /// one exchange — `35=Q` acks for L1 and `35=L` ticker setup for news
    /// routing — and only tick and news routing read them back, so this is
    /// safe where the instrument is going away and where its L1 subscription
    /// ends with no news subscription left on it.
    ///
    /// Forgotten, not held against the venue. These numbers were kept in a set
    /// of ones this session had given up, and an answer naming any of them was
    /// refused wherever it arrived. The venue hands its numbers out again: a
    /// subscription opened after another was withdrawn is routinely answered
    /// under the number the withdrawn one held, and every one of those was
    /// acknowledged, bound to nothing and left silent — no quotes, no refusal,
    /// for the rest of the session. What says an answer is a late one is the
    /// request it names, which the paths that read these already look up.
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
    /// Order encoders derive their routing tags from these rather than
    /// stating stock-on-SMART. Empty strings leave the defaults in place.
    pub fn set_routing(&mut self, id: InstrumentId, sec_type: &str, exchange: &str) {
        if !sec_type.is_empty() {
            self.sec_types[id as usize] = Some(sec_type.to_uppercase());
        }
        if !exchange.is_empty() {
            self.exchanges[id as usize] = Some(exchange.to_uppercase());
        }
    }

    /// State the identity an order has to restate: expiry, strike, right and
    /// multiplier, as the same `|`-separated key a registration carries, and
    /// optionally the trading class and local symbol after them.
    pub fn set_order_identity(&mut self, id: InstrumentId, key: &str) {
        if !key.is_empty() {
            self.option_keys[id as usize] = Some(key.to_string());
        }
    }

    /// What an order on this contract is denominated in.
    ///
    /// The venue infers this from the contract id on the orders that carry
    /// one, so most paths state nothing. Where an order does state it, stating
    /// a literal instead put every leg of a bracket on a European or Japanese
    /// contract into dollars.
    ///
    /// Empty where the contract stated none. Tag 15 carries what the contract
    /// was registered with; substituting a currency states one the caller did
    /// not, and the venue reads the order as being for a different listing.
    pub fn order_currency(&self, id: InstrumentId) -> String {
        self.order_identity(id).map(|identity| identity.currency).unwrap_or_default()
    }

    /// The currency this contract was registered with, where one was stated.
    ///
    /// Absent where the contract stated none; `order_currency` returns an
    /// empty string in that case. A contract registered by its id alone names
    /// no currency, so an order that needs one takes it from the definition.
    pub fn order_currency_stated(&self, id: InstrumentId) -> Option<String> {
        let key = self.option_keys.get(id as usize)?.as_deref()?;
        key.split('|').nth(6).filter(|c| !c.is_empty()).map(str::to_string)
    }

    pub fn order_identity(&self, id: InstrumentId) -> Option<OrderIdentity> {
        let key = self.option_keys.get(id as usize)?.as_deref()?;
        let mut it = key.split('|');
        let expiry = it.next().unwrap_or("").to_string();
        let strike = it.next().unwrap_or("").to_string();
        let right = it.next().unwrap_or("").to_string();
        let multiplier = it.next().unwrap_or("").to_string();
        // A key written before these existed simply has neither.
        let trading_class = it.next().unwrap_or("").to_string();
        let local_symbol = it.next().unwrap_or("").to_string();
        let currency = it.next().unwrap_or("").to_string();
        // No guard on "does this look like an option": every field is checked
        // for emptiness where it is written, and a stock that states only its
        // currency has an identity worth returning.
        Some(OrderIdentity {
            expiry, strike, right, multiplier, trading_class, local_symbol, currency,
        })
    }

    /// The security type and destination an order for this instrument states.
    ///
    /// The security type is empty where the contract stated none. A substituted type
    /// describes a different instrument, and tag 167 carries the contract's own
    /// or nothing.
    pub fn order_routing(&self, id: InstrumentId) -> (String, String) {
        let sec_type = self.sec_types[id as usize].clone().unwrap_or_default();
        let sec_type = crate::control::contracts::SecurityType::from_fix(&sec_type)
            .to_fix()
            .to_string();
        let exchange = self.exchanges[id as usize].as_deref().unwrap_or("");
        let is_cash = sec_type == "CASH";
        let destination = match exchange {
            "" | "SMART" | "IBKRATS" | "BEST" => {
                if is_cash { "IDEALPRO" } else { "BEST" }.to_string()
            }
            other => other.to_string(),
        };
        // Under the name the venue routes by, not the one a caller was handed.
        // A US stock on Nasdaq is handed back under its older spelling, and a
        // caller that passes that contract straight back would otherwise be
        // routed to a name that reaches nothing.
        let destination =
            crate::control::contracts::exchange_to_fix(&destination).to_string();
        (sec_type, destination)
    }

    /// The symbol an instrument was registered under, or nothing.
    ///
    /// `None` when unregistered. A placeholder here reaches tag 55 as the
    /// contract's name; the order path refuses a contract it cannot name.
    pub fn symbol(&self, id: InstrumentId) -> &str {
        self.symbols[id as usize].as_deref().unwrap_or("")
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

    /// What the venue counts this instrument's sizes in.
    #[inline(always)]
    pub fn size_tick(&self, id: InstrumentId) -> f64 {
        self.size_ticks[id as usize]
    }

    /// Record what the venue counts this instrument's sizes in.
    pub fn set_size_tick(&mut self, id: InstrumentId, size_tick: f64) {
        self.size_ticks[id as usize] = size_tick;
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

    /// The quote and the trade clock it is stamped from, together: one tick
    /// writes both, and the two are the same instrument's.
    pub(crate) fn quote_and_clock_mut(&mut self, instrument: InstrumentId) -> (&mut Quote, &mut TradeClock) {
        (&mut self.quotes[instrument as usize], &mut self.clocks[instrument as usize])
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

    /// Forget the venue's numbers, which the connection they came on took with
    /// it.
    ///
    /// Both halves. A number is given up so that an answer arriving after the
    /// subscription it belonged to is over does not point a later request at a
    /// number nothing comes on — and that is a question about one connection.
    /// Across a new one the venue's numbers start again, so a number given up
    /// on the old connection can be issued again on this one, and the answer
    /// carrying it was refused: the subscription it belonged to was silently
    /// dead for the rest of the session, on a connection reporting healthy.
    /// Kept, the set also only ever grew.
    pub fn clear_server_tags(&mut self) {
        self.server_tag_to_instrument.clear();
    }

    /// Zero all quote data to prevent stale price trading after farm disconnect.
    /// The clocks go with them: the venue states a base again on the next
    /// connection, and an offset arriving before it has nothing to add to.
    pub fn zero_all_quotes(&mut self) {
        for i in 0..self.active_count as usize {
            self.quotes[i] = Quote::default();
            self.clocks[i] = TradeClock::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A number the venue hands out again names whatever it now names.
    ///
    /// These numbers were kept in a set of ones this session had given up, and
    /// an answer naming any of them was refused. The venue reuses them: a
    /// subscription opened after another was withdrawn is routinely answered
    /// under the number the withdrawn one held, and refusing it left that
    /// subscription bound to nothing and silent — no quotes and no refusal —
    /// for the rest of the session.
    #[test]
    fn a_number_the_venue_hands_out_again_names_what_it_now_names() {
        let mut ms = MarketState::new();
        let first = ms.register(1);
        ms.register_server_tag(274555, first);
        ms.clear_server_tags_for(first);
        assert_eq!(ms.instrument_by_server_tag(274555), None, "the old mapping is gone");

        // The same number, on the next subscription.
        let next = ms.register(2);
        ms.register_server_tag(274555, next);
        assert_eq!(
            ms.instrument_by_server_tag(274555), Some(next),
            "and the number routes to the contract holding it now",
        );
    }
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

    // ── routing derivation ──

    /// An order that states a currency states the contract's, not the one most
    /// contracts happen to use.
    ///
    /// Every leg of a bracket carries tag 15. It was the literal `USD`, so a
    /// bracket on a contract denominated in anything else went out declaring
    /// dollars — on a contract the venue knows is not.
    #[test]
    fn an_order_is_denominated_in_the_contracts_own_currency() {
        let mut ms = MarketState::new();

        // An instrument registered by id alone states no currency.
        let bare = ms.register(101);
        assert_eq!(ms.order_currency(bare), "");

        // expiry|strike|right|multiplier|trading class|local symbol|currency
        let eu = ms
            .try_register_contract(102, "SAP", "STK", "IBIS", "||||||EUR")
            .expect("registers");
        assert_eq!(ms.order_currency(eu), "EUR");
        assert_eq!(ms.order_currency_stated(eu), Some("EUR".to_string()));

        let jp = ms
            .try_register_contract(103, "7203", "STK", "TSEJ", "||||||JPY")
            .expect("registers");
        assert_eq!(ms.order_currency(jp), "JPY");

        // A contract that names no currency states none. The venue infers it
        // from the contract id, and a substituted currency describes a
        // different listing.
        let unstated = ms
            .try_register_contract(104, "AAPL", "STK", "SMART", "|||||")
            .expect("registers");
        assert_eq!(ms.order_currency(unstated), "");
        assert_eq!(
            ms.order_currency_stated(unstated), None,
            "a contract registered without a currency stated none, which is not the \
             same as stating dollars: the venue's definition is asked next",
        );
    }

    #[test]
    fn order_routing_rules() {
        let mut ms = MarketState::new();
        let stk = ms.register(1);
        // An instrument registered without a security type states none. The
        // destination still resolves, because an unnamed venue is the smart
        // one; the type is the contract's own or nothing.
        assert_eq!(ms.order_routing(stk), (String::new(), "BEST".into()));

        // The registered security type is what goes on the wire.
        let fx = ms.register(2);
        ms.set_routing(fx, "CASH", "");
        assert_eq!(ms.order_routing(fx), ("CASH".into(), "IDEALPRO".into()));

        // An explicit stock converts the same way.
        let stk2 = ms.register(20);
        ms.set_routing(stk2, "STK", "NYSE");
        assert_eq!(ms.order_routing(stk2), ("CS".into(), "NYSE".into()));

        // A type the client cannot classify is sent empty rather than as a
        // stock, so the gateway rejects it visibly.
        let unknown = ms.register(21);
        ms.set_routing(unknown, "WIDGET", "NYSE");
        assert_eq!(ms.order_routing(unknown), ("".into(), "NYSE".into()));

        // IBKRATS resolves to IDEALPRO for CASH, SMART otherwise.
        let fx2 = ms.register(3);
        ms.set_routing(fx2, "CASH", "IBKRATS");
        assert_eq!(ms.order_routing(fx2), ("CASH".into(), "IDEALPRO".into()));
        let stk2 = ms.register(4);
        ms.set_routing(stk2, "STK", "IBKRATS");
        assert_eq!(ms.order_routing(stk2), ("CS".into(), "BEST".into()));

        // An explicit directed exchange is respected.
        let directed = ms.register(5);
        ms.set_routing(directed, "STK", "NYSE");
        assert_eq!(ms.order_routing(directed), ("CS".into(), "NYSE".into()));

        // Unregister clears the routing for slot reuse.
        ms.unregister(fx);
        let reused = ms.register(6);
        assert_eq!(reused, fx);
        assert_eq!(ms.order_routing(reused), (String::new(), "BEST".into()));
    }

    // ── unregister + slot reuse ──

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
        // The watchlist scenario: cycle far more than MAX_INSTRUMENTS
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
        ms.try_register_contract(100, "AAPL", "OPT", "SMART", "20260918|150|C|100");

        assert_eq!(ms.unregister(id), Some(100));
        // `None` rather than a placeholder, which would reach tag 55 as the
        // contract's name.
        assert_eq!(ms.symbol(id), "");
        assert_eq!(ms.min_tick(id), 0.0);
        assert_eq!(ms.min_tick_scaled(id), 0);
        assert_eq!(ms.instrument_by_server_tag(42), None);
        assert_eq!(ms.quote(id).bid, 0, "stale quote must not survive into a reused slot");
        assert_eq!(ms.order_identity(id), None, "nor the contract identity");
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
        q.timestamp_ns = 1_709_654_400_000_000_000;

        let q_ref = ms.quote(id);
        assert_eq!(q_ref.open, 148 * PRICE_SCALE);
        assert_eq!(q_ref.high, 155 * PRICE_SCALE);
        assert_eq!(q_ref.low, 147 * PRICE_SCALE);
        assert_eq!(q_ref.close, 152 * PRICE_SCALE);
        assert_eq!(q_ref.volume, 50_000_000);
        assert_eq!(q_ref.timestamp_ns, 1_709_654_400_000_000_000);
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

    /// A withdrawn subscription's number routes to nothing, and its neighbour
    /// is untouched.
    #[test]
    fn a_withdrawn_number_routes_to_nothing_and_leaves_the_others_alone() {
        let mut ms = MarketState::new();
        let a = ms.register(265598);
        let b = ms.register(272093);
        ms.register_server_tag(10, a);
        ms.register_server_tag(20, b);

        ms.clear_server_tags_for(a);

        assert_eq!(ms.instrument_by_server_tag(10), None, "the withdrawn one routes to nothing");
        assert_eq!(ms.instrument_by_server_tag(20), Some(b), "while the other still does");
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
        ms.quote_and_clock_mut(a).1.base_secs = 1_785_325_000;
        ms.zero_all_quotes();
        assert_eq!(ms.bid(a), 0);
        assert_eq!(ms.ask(a), 0);
        assert_eq!(ms.last(b), 0);
        assert_eq!(ms.quote_and_clock_mut(a).1.base_secs, 0, "the clock goes with the quote");
    }

    #[test]
    fn zero_all_quotes_no_registered_is_noop() {
        let mut ms = MarketState::new();
        ms.zero_all_quotes(); // should not panic
    }

    #[test]
    fn a_slot_named_by_symbol_adopts_the_id_a_lookup_finds() {
        let mut m = MarketState::new();
        let id = m.try_register_contract(0, "SPY", "STK", "SMART", "").unwrap();
        // A live slot with no id reads as zero, which is not an identity.
        assert_eq!(m.con_id(id), Some(0), "named by symbol, so it has no id yet");

        assert!(m.adopt_con_id(id, 756733));
        assert_eq!(m.con_id(id), Some(756733));
        assert_eq!(m.instrument_by_con_id(756733), Some(id), "and it is found by it");

        assert!(!m.adopt_con_id(id, 999), "a slot that has an id keeps it");
        assert_eq!(m.con_id(id), Some(756733));

        let other = m.try_register_contract(0, "QQQ", "STK", "SMART", "").unwrap();
        assert!(!m.adopt_con_id(other, 756733), "and an id is not taken from the slot holding it");
        assert_eq!(m.instrument_by_con_id(756733), Some(id));
        assert!(!m.adopt_con_id(other, 0), "nothing is not an identity");

        // An unallocated slot is still within the arrays. Adopting one writes
        // a contract the count and the live list cannot see.
        let unhandled = m.count() as crate::types::InstrumentId;
        assert!(
            !m.adopt_con_id(unhandled, 111_111),
            "a slot nobody holds adopts nothing",
        );
        assert_eq!(m.instrument_by_con_id(111_111), None, "and nothing points at it");
    }

    /// Two descriptions the venue would answer differently are not one
    /// contract.
    ///
    /// Symbol, security type and exchange are equal for one ticker listed on
    /// two venues, or asked for by two identifiers, and none of what tells
    /// them apart is part of what a contract is. Matched on the rest alone,
    /// the second description followed the first one's subscription: it was
    /// never looked up, and the other listing's prices arrived under its
    /// number.
    #[test]
    fn two_descriptions_the_venue_would_answer_differently_do_not_share_a_slot() {
        let mut m = MarketState::new();
        let on_one = m
            .try_register_described(0, "ABC", "STK", "SMART", "", "NASDAQ|||||")
            .expect("the first");
        let on_another = m
            .try_register_described(0, "ABC", "STK", "SMART", "", "NYSE|||||")
            .expect("the second");
        assert_ne!(on_one, on_another, "one listing's prices would arrive for both");

        // The same description finds the slot it took.
        assert_eq!(
            m.try_register_described(0, "ABC", "STK", "SMART", "", "NASDAQ|||||"),
            Some(on_one),
        );

        // A slot that narrows nothing adopts the caller's, the way one with no
        // identity adopts that: the pre-flight registration carries neither,
        // and stranding its slot would take a second one for the same
        // contract.
        let plain = m.try_register_contract(0, "XYZ", "STK", "SMART", "").expect("pre-flight");
        assert_eq!(
            m.try_register_described(0, "XYZ", "STK", "SMART", "", "ARCA|||||"),
            Some(plain),
            "the description the pre-flight registration stood in for",
        );
        assert_ne!(
            m.try_register_described(0, "XYZ", "STK", "SMART", "", "BATS|||||"),
            Some(plain),
            "and having adopted one, it is that description's slot and no other's",
        );
    }

    /// A slot taken for a descriptor is matched by that descriptor. Without
    /// it, the same contract requested twice takes two slots.
    #[test]
    fn a_slot_taken_for_a_descriptor_is_the_slot_that_descriptor_finds() {
        let mut m = MarketState::new();
        let first = m.try_register_contract(0, "SPY", "STK", "SMART", "").unwrap();
        let again = m.try_register_contract(0, "SPY", "STK", "SMART", "").unwrap();
        assert_eq!(first, again, "the same contract is the same instrument");
        assert_eq!(m.count(), 1, "and it took one slot: {}", m.count());
        assert_eq!(m.symbol(first), "SPY", "the slot knows what it was taken for");
    }
}

#[cfg(test)]
mod routing_name_tests {
    use super::MarketState;

    /// A contract is handed to a caller under the name the venue uses,
    /// and a caller passing it straight back must still be routed under the
    /// name the venue knows. Routed under the one it was handed, a request for
    /// a Nasdaq listing reaches nothing.
    #[test]
    fn a_contract_is_routed_under_the_name_the_venue_knows() {
        let mut market = MarketState::new();
        let instrument = market.register(265598);
        market.set_routing(instrument, "STK", "ISLAND");
        let (_, destination) = market.order_routing(instrument);
        assert_eq!(destination, "NASDAQ", "routed under a name that reaches nothing");
    }
}

#[cfg(test)]
mod registration_symbol_tests {
    use super::MarketState;

    /// A contract registered by its id keeps the symbol it was registered
    /// with. The accessor substitutes a placeholder for an unregistered
    /// symbol, so the check tests the stored value.
    #[test]
    fn a_contract_registered_by_id_keeps_its_symbol() {
        let mut market = MarketState::new();
        let instrument = market
            .try_register_contract(756733, "SPY", "STK", "SMART", "")
            .expect("register a contract");
        assert_eq!(market.symbol(instrument), "SPY");
    }
}
