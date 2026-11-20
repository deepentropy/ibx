use std::collections::HashMap;
use crate::protocol::tick_decoder::{self as td, RawTick};
use crate::types::{InstrumentId, Price, Qty, Quote, QuoteMarks, SizeKind, PRICE_SCALE, QTY_SCALE, MAX_INSTRUMENTS};

const NS_PER_SEC: u64 = 1_000_000_000;

/// Hasher for the server tag map. Tags are integers assigned by the server,
/// not chosen by a peer, so one multiply (Fibonacci hashing) is enough: the
/// high bits it mixes pick the control byte, the low bits stay a bijection
/// of the tag's low bits, so consecutive tags never share a bucket.
#[derive(Default, Clone, Copy)]
struct TagHasher(u64);

const TAG_HASH_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

impl std::hash::Hasher for TagHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(TAG_HASH_MUL);
        }
    }

    #[inline(always)]
    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0 ^ n as u64).wrapping_mul(TAG_HASH_MUL);
    }

    #[inline(always)]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(TAG_HASH_MUL);
    }
}

/// What a server tag routes to: the instrument, and the minimum price tick
/// of that tag times PRICE_SCALE. The reference keeps the tick per server
/// tag, not per contract: the two entries of one contract can tick in
/// different steps (a currency pair's bid/ask book and its last price).
/// `trade` is set for a trade stream tag. `size_tick` is the size
/// increment of the tag times QTY_SCALE: the reference keeps it per server
/// tag too (a value from the first acknowledgement of the tag, another
/// from the latest trade setup; ibx#446).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagRoute {
    pub instrument: InstrumentId,
    pub price_tick: i64,
    pub size_tick: i64,
    pub trade: bool,
}

/// Server tags are numbered by each farm (#445): the key is the farm and
/// the tag.
type TagMap = HashMap<u64, TagRoute, std::hash::BuildHasherDefault<TagHasher>>;

/// A minimum tick times PRICE_SCALE, the integer the hot path multiplies by.
#[inline]
fn scale_tick(min_tick: f64) -> i64 {
    (min_tick * PRICE_SCALE as f64).round() as i64
}

/// A size increment times QTY_SCALE; absent or not a positive number: 1,
/// as the reference uses a default for an invalid one.
#[inline]
fn scale_size(size_min_tick: Option<f64>) -> i64 {
    let scaled = size_min_tick.map(|s| (s * QTY_SCALE as f64).round()).unwrap_or(f64::NAN);
    if scaled.is_finite() && scaled >= 1.0 { scaled as i64 } else { QTY_SCALE }
}

/// A last size of the farm as kept (ibx#613): from session level 222 with
/// the decimals the farm gave it (the 10.51 form's extended entries), down
/// to the quantity's steps of 0.00000001 (ibx#655); below 222 in whole units, `whole`
/// (the value without its decimals) times `sizes`, as before. The 10.51
/// rule, not observed yet: no fractional trade on the paper account.
#[inline(always)]
fn last_size_value(level: i32, tick: &RawTick, whole: i64, sizes: i64) -> Option<i64> {
    if level < crate::client_core::LEVEL_FRACTIONAL_LAST_SIZE || tick.scale == 0 {
        return whole.checked_mul(sizes);
    }
    let v = tick.magnitude as i128 * sizes as i128 / 10i128.pow(tick.scale.min(30) as u32);
    i64::try_from(v).ok()
}

#[inline(always)]
fn tag_key(farm: u8, server_tag: u32) -> u64 {
    ((farm as u64) << 32) | server_tag as u64
}

/// Server tags pre-sized for: two per instrument (quote and trade stream)
/// with room to spare, so the map does not grow in a normal session.
const SERVER_TAG_CAPACITY: usize = MAX_INSTRUMENTS * 4;

/// Sentinel conId marking a freed instrument slot (ibx#233). Cannot collide
/// with a real conId (0 occurs in practice for conId-less contracts).
const FREE_SLOT: i64 = i64::MIN;

/// How many contracts keep their last bid and ask after their last request
/// ended (ibx#508).
pub const KEPT_QUOTES: usize = 64;

/// A contract's last bid, ask, their sizes and auto-execution flags, kept
/// after its last market data request ended, as the reference's contract
/// record keeps them for the session (ibx#508; the reference 10.51, paper,
/// pre-market 13/10/2026: still given after 300 s, to another API
/// connection too, and after the contract was only an option's
/// underlying). Kept until another contract needs the entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeptQuote {
    /// 0: an empty entry.
    pub con_id: i64,
    pub bid: Price,
    pub bid_size: Qty,
    pub ask: Price,
    pub ask_size: Qty,
    /// The last trade's exchange (`Quote::last_exch_mask`), 0 when none.
    pub last_exch: i64,
    /// `QuoteMarks::auto_word`.
    pub auto: u8,
    stamp: u64,
}

impl KeptQuote {
    /// The kept bid and ask of values as the farm sent them, scaled as the
    /// farm path scales them (`MarketState::apply_tick_sized`): the price
    /// tick and size increment of the book entry, the contract's round lot.
    pub fn from_wire(con_id: i64, wire: [Option<i64>; 4], min_tick: f64, size_tick: Option<f64>, round_lot: i64, auto_bits: Option<i64>) -> Self {
        let price = |v: Option<i64>| v.and_then(|m| m.checked_mul(scale_tick(min_tick))).unwrap_or(0);
        let size = |v: Option<i64>| v.and_then(|m| m.checked_mul(scale_size(size_tick) * round_lot)).unwrap_or(0);
        let mut marks = QuoteMarks::default();
        if let Some(bits) = auto_bits {
            marks.set_auto_bits(bits);
        }
        Self::new(con_id, price(wire[0]), size(wire[2]), price(wire[1]), size(wire[3]), marks.auto_word())
    }

    pub fn new(con_id: i64, bid: Price, bid_size: Qty, ask: Price, ask_size: Qty, auto: u8) -> Self {
        Self { con_id, bid, bid_size, ask, ask_size, last_exch: 0, auto, stamp: 0 }
    }
}

/// How many quote tags of freed slots are remembered (ibx#694).
const ENDED_TAGS: usize = 16;

/// A quote tag of a request whose slot was freed (ibx#694): its contract
/// and the scale of its prices and sizes.
#[derive(Debug, Clone, Copy, Default)]
struct EndedTag {
    /// 0: an empty entry.
    key: u64,
    con_id: i64,
    price_tick: i64,
    sizes: i64,
}

/// Pre-allocated quote storage indexed by InstrumentId.
/// All quotes live in a contiguous array for cache efficiency.
pub struct MarketState {
    quotes: [Quote; MAX_INSTRUMENTS],
    /// What the farm told about each quote beside its fields (ibx#446).
    marks: [QuoteMarks; MAX_INSTRUMENTS],
    /// High-water mark: slots ever allocated (iteration bound). Freed slots
    /// below this mark are reused via `free_ids` before new ones are taken,
    /// so the MAX_INSTRUMENTS cap bounds CONCURRENT instruments, not the
    /// session's cumulative total (ibx#233).
    active_count: u32,
    /// Freed slot ids available for reuse.
    free_ids: Vec<InstrumentId>,
    /// Maps the broker's conId → internal InstrumentId. O(1) lookup.
    con_id_to_instrument: HashMap<i64, InstrumentId>,
    /// Reverse map: InstrumentId → conId. Flat array lookup. `FREE_SLOT`
    /// marks a reclaimed slot.
    instrument_to_con_id: [i64; MAX_INSTRUMENTS],
    /// server_tag → InstrumentId. Server tags climb far past what a flat
    /// table can hold in one session (ibx#281).
    server_tags: TagMap,
    /// Trade stream tags: kept apart from the quote tags, as the reference
    /// keeps them, and freed only when the last slot of the contract goes
    /// (#292).
    trade_tags: TagMap,
    /// The contract's minimum tick: the one of its bid/ask entry, as the
    /// reference stores it in the contract. Wire prices are scaled by the
    /// tick of their server tag instead (`TagRoute`).
    min_ticks: [f64; MAX_INSTRUMENTS],
    /// min_ticks * PRICE_SCALE as an integer.
    min_tick_scaled: [i64; MAX_INSTRUMENTS],
    /// Last trade time base per instrument, epoch seconds; a later delta
    /// is added to it (ibx#448).
    last_ts_base: [i64; MAX_INSTRUMENTS],
    /// Size increment of the market data, times QTY_SCALE (ibx#287). A
    /// wire size times this is the size in the Qty fixed point.
    size_min_tick_scaled: [i64; MAX_INSTRUMENTS],
    /// Round lot the bid, ask and last sizes are counted in: 1, or the
    /// contract's lot for a US stock when the session scales US lots.
    round_lots: [i64; MAX_INSTRUMENTS],
    /// Per-instrument symbol name. Flat array indexed by InstrumentId.
    symbols: [Option<String>; MAX_INSTRUMENTS],
    /// Contract currency for the orders (the currency field, ibx#466).
    currencies: [Option<String>; MAX_INSTRUMENTS],
    /// Per-instrument security type (API string, e.g. "STK"/"CASH"). Empty
    /// slot = unknown, treated as stock. Registration used to DROP this
    /// field silently (ibx#217).
    sec_types: [Option<String>; MAX_INSTRUMENTS],
    /// Per-instrument requested exchange. Empty slot = default routing.
    exchanges: [Option<String>; MAX_INSTRUMENTS],
    /// The terms of an option, written on its orders; None for any other
    /// contract.
    option_terms: [Option<Box<OptionTerms>>; MAX_INSTRUMENTS],
    /// The last bid and ask of contracts whose requests ended (ibx#508),
    /// given to the next request at its acknowledgement. Preallocated: the
    /// cancel path writes it without allocating.
    kept: [KeptQuote; KEPT_QUOTES],
    /// Write counter of `kept`: the entry with the smallest stamp is the
    /// oldest, overwritten when the table is full.
    kept_clock: u64,
    /// The quote tags of requests whose slot was freed, with what their
    /// values need (ibx#694): the farm's data that comes for them after the
    /// cancel goes to the contract's kept bid and ask. Preallocated, written
    /// in turn.
    ended_tags: [EndedTag; ENDED_TAGS],
    ended_next: usize,
    /// The server sends the sizes in round lots (the 10.40 form, replays of
    /// its recordings); false: in shares, as with the entry mark ibx writes
    /// since #648 (ibx#663).
    lot_sizes: bool,
    /// The day volume of a US stock, which comes in shares, is given in
    /// round lots: only when the logon's features ask for it (ibx#763).
    volume_lots: bool,
}

/// The terms of an option contract its orders carry, as the reference
/// writes them after the symbol: the
/// maturity (the definition's own), the right
/// (1 call, 0 put), the strike and the multiplier.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionTerms {
    pub maturity: String,
    pub call: bool,
    pub strike: f64,
    pub multiplier: f64,
}

impl MarketState {
    pub fn new() -> Self {
        Self {
            quotes: [Quote::default(); MAX_INSTRUMENTS],
            marks: [QuoteMarks::default(); MAX_INSTRUMENTS],
            active_count: 0,
            free_ids: Vec::new(),
            con_id_to_instrument: HashMap::new(),
            instrument_to_con_id: [0; MAX_INSTRUMENTS],
            server_tags: TagMap::with_capacity_and_hasher(SERVER_TAG_CAPACITY, Default::default()),
            trade_tags: TagMap::with_capacity_and_hasher(MAX_INSTRUMENTS * 2, Default::default()),
            min_ticks: [0.0; MAX_INSTRUMENTS],
            min_tick_scaled: [0; MAX_INSTRUMENTS],
            last_ts_base: [0; MAX_INSTRUMENTS],
            size_min_tick_scaled: [QTY_SCALE; MAX_INSTRUMENTS],
            round_lots: [1; MAX_INSTRUMENTS],
            symbols: std::array::from_fn(|_| None),
            currencies: std::array::from_fn(|_| None),
            sec_types: std::array::from_fn(|_| None),
            exchanges: std::array::from_fn(|_| None),
            option_terms: std::array::from_fn(|_| None),
            kept: [KeptQuote::default(); KEPT_QUOTES],
            kept_clock: 0,
            ended_tags: [EndedTag::default(); ENDED_TAGS],
            ended_next: 0,
            lot_sizes: false,
            volume_lots: false,
        }
    }

    /// The volumes that come in shares are given in round lots (ibx#763):
    /// the reference does so only when its "volumes in lots for US stocks"
    /// switch is on, or when the logon's features ask for it. Off by
    /// default, as the reference observed on 20/11/2026.
    pub fn set_volume_lots(&mut self, on: bool) {
        self.volume_lots = on;
    }

    /// The volumes that come in shares are given in round lots (ibx#763).
    pub fn volume_lots(&self) -> bool {
        self.volume_lots
    }

    /// The sizes come in round lots (the 10.40 form) or in shares (ibx#663).
    pub fn set_lot_sizes(&mut self, on: bool) {
        self.lot_sizes = on;
    }

    /// The server's sizes come in round lots (the 10.40 form).
    pub fn lot_sizes(&self) -> bool {
        self.lot_sizes
    }

    /// The round lot the sizes of the server are counted in: the
    /// instrument's in the 10.40 form, 1 when they come in shares.
    #[inline(always)]
    pub fn size_lot(&self, round_lot: i64) -> i64 {
        if self.lot_sizes { round_lot.max(1) } else { 1 }
    }

    /// Keep a contract's last bid and ask (ibx#508): in its own entry, else
    /// an empty one, else over the oldest. Nothing without a bid or an ask.
    pub fn keep_quote(&mut self, mut k: KeptQuote) {
        if k.con_id == 0 || (k.bid == 0 && k.ask == 0) {
            return;
        }
        self.kept_clock += 1;
        k.stamp = self.kept_clock;
        let mut at = 0;
        for (i, e) in self.kept.iter().enumerate() {
            if e.con_id == k.con_id {
                at = i;
                break;
            }
            if self.kept[at].con_id != 0 && (e.con_id == 0 || e.stamp < self.kept[at].stamp) {
                at = i;
            }
        }
        self.kept[at] = k;
    }

    /// The bid and ask a contract kept, taken out of the table (ibx#508).
    pub fn take_kept(&mut self, con_id: i64) -> Option<KeptQuote> {
        if con_id == 0 {
            return None;
        }
        let e = self.kept.iter_mut().find(|e| e.con_id == con_id)?;
        Some(std::mem::take(e))
    }

    /// The farm's data for a quote tag of a freed slot (ibx#694): its bid,
    /// ask and sizes go to the contract's kept quote, as the reference's
    /// contract record takes them after the cancel (paper, 14/10/2026,
    /// regular hours: the image that came 0.1 s after the cancel gave the
    /// next request its sizes). False when the tag is not such a tag.
    pub fn apply_ended_tick(&mut self, farm: u8, tick: &RawTick) -> bool {
        let key = tag_key(farm, tick.server_tag);
        let Some(e) = self.ended_tags.iter().find(|e| e.key == key && e.key != 0).copied() else { return false };
        let Some(k) = self.kept.iter_mut().find(|k| k.con_id == e.con_id) else { return true };
        let m = if tick.scale == 0 { tick.magnitude } else { tick.magnitude / 10i64.pow(tick.scale.min(18) as u32) };
        match tick.tick_type {
            td::O_BID_PRICE => if let Some(v) = m.checked_mul(e.price_tick) { k.bid = v },
            td::O_ASK_PRICE => if let Some(v) = m.checked_mul(e.price_tick) { k.ask = v },
            td::O_BID_SIZE => if let Some(v) = m.checked_mul(e.sizes) { k.bid_size = v },
            td::O_ASK_SIZE => if let Some(v) = m.checked_mul(e.sizes) { k.ask_size = v },
            _ => {}
        }
        true
    }

    /// A kept bid and ask become the quote of a new request's slot, with
    /// their sizes seen and their auto-execution flags (ibx#508).
    pub fn seed_kept(&mut self, id: InstrumentId, k: &KeptQuote) {
        let i = id as usize;
        if i >= MAX_INSTRUMENTS {
            return;
        }
        let q = &mut self.quotes[i];
        q.bid = k.bid;
        q.bid_size = k.bid_size;
        q.ask = k.ask;
        q.ask_size = k.ask_size;
        q.last_exch_mask = k.last_exch;
        let m = &mut self.marks[i];
        let auto = QuoteMarks::from_auto_word(k.auto);
        if let Some(on) = auto.bid_auto() { m.set_bid_auto(on); }
        if let Some(on) = auto.ask_auto() { m.set_ask_auto(on); }
        // A kept -1 has no size: the farm's first size, also 0, is then
        // given (ibx#647; the reference 10.51, 13/10/2026 pre-market).
        if k.bid > 0 { m.set_seen(SizeKind::Bid); }
        if k.ask > 0 { m.set_seen(SizeKind::Ask); }
    }

    /// Register a broker contract, returns the assigned InstrumentId, or None
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

    /// A slot for a contract given without a conId (ibx#278): never shared
    /// and not found by conId until `resolve_con_id`; conId 0 until then.
    /// None when the table is full.
    pub fn try_register_unresolved(&mut self) -> Option<InstrumentId> {
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
        self.instrument_to_con_id[id as usize] = 0;
        Some(id)
    }

    /// The conId a lookup found for an unresolved slot (ibx#278). When
    /// another slot already has that conId, lookups by conId keep finding
    /// the other one.
    pub fn resolve_con_id(&mut self, instrument: InstrumentId, con_id: i64) {
        if self.con_id(instrument).is_none() {
            return;
        }
        self.instrument_to_con_id[instrument as usize] = con_id;
        self.con_id_to_instrument.entry(con_id).or_insert(instrument);
    }

    /// Register a broker contract, returns the assigned InstrumentId.
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
        // Only its own entry: an unresolved slot has none, and a resolved
        // one can share its conId with another slot (ibx#278).
        if self.con_id_to_instrument.get(&con_id) == Some(&instrument) {
            self.con_id_to_instrument.remove(&con_id);
        }
        self.instrument_to_con_id[instrument as usize] = FREE_SLOT;
        // Its quote tags are remembered: the farm's data that comes after
        // the cancel still goes to the kept bid and ask (ibx#694).
        if con_id > 0 {
            let lot = if self.lot_sizes { self.round_lots[instrument as usize].max(1) } else { 1 };
            for (key, r) in self.server_tags.iter().filter(|(_, r)| r.instrument == instrument) {
                self.ended_tags[self.ended_next] = EndedTag {
                    key: *key, con_id, price_tick: r.price_tick, sizes: r.size_tick * lot,
                };
                self.ended_next = (self.ended_next + 1) % ENDED_TAGS;
            }
        }
        // Its last bid and ask stay for the next request (ibx#508).
        let q = self.quotes[instrument as usize];
        self.keep_quote(KeptQuote {
            last_exch: q.last_exch_mask,
            ..KeptQuote::new(con_id, q.bid, q.bid_size, q.ask, q.ask_size, self.marks[instrument as usize].auto_word())
        });
        self.quotes[instrument as usize] = Quote::default();
        self.marks[instrument as usize] = QuoteMarks::default();
        self.symbols[instrument as usize] = None;
        self.currencies[instrument as usize] = None;
        self.sec_types[instrument as usize] = None;
        self.exchanges[instrument as usize] = None;
        self.option_terms[instrument as usize] = None;
        self.min_ticks[instrument as usize] = 0.0;
        self.min_tick_scaled[instrument as usize] = 0;
        self.last_ts_base[instrument as usize] = 0;
        self.size_min_tick_scaled[instrument as usize] = QTY_SCALE;
        self.round_lots[instrument as usize] = 1;
        self.server_tags.retain(|_, r| r.instrument != instrument);
        // The trade stream tags belong to the contract: they go with its
        // last slot (#292).
        let other = (con_id != 0)
            .then(|| (0..self.active_count).find(|i| self.instrument_to_con_id[*i as usize] == con_id))
            .flatten();
        match other {
            Some(other) => self.trade_tags.values_mut().filter(|r| r.instrument == instrument).for_each(|r| r.instrument = other),
            None => self.trade_tags.retain(|_, r| r.instrument != instrument),
        }
        self.free_ids.push(instrument);
        Some(con_id)
    }

    /// Map a quote tag of the primary market data farm to an instrument,
    /// with the minimum tick of its acknowledgement.
    pub fn register_server_tag(&mut self, server_tag: u32, instrument: InstrumentId, min_tick: f64) {
        self.register_farm_tag(0, server_tag, instrument, min_tick);
    }

    /// Map a quote tag (subscription acknowledgement) of a farm to an
    /// instrument (#445), with the minimum tick of the acknowledgement.
    /// The reference makes one record per server tag on the first
    /// acknowledgement of the tag: a later acknowledgement of the same tag
    /// (the other entry of the contract) joins that record and the first
    /// tick stays.
    pub fn register_farm_tag(&mut self, farm: u8, server_tag: u32, instrument: InstrumentId, min_tick: f64) {
        self.register_farm_tag_sized(farm, server_tag, instrument, min_tick, None);
    }

    /// The same with the size increment of the acknowledgement, which
    /// also stays from the first acknowledgement of the tag.
    pub fn register_farm_tag_sized(
        &mut self, farm: u8, server_tag: u32, instrument: InstrumentId, min_tick: f64, size_min_tick: Option<f64>,
    ) {
        let key = tag_key(farm, server_tag);
        for e in self.ended_tags.iter_mut().filter(|e| e.key == key) {
            *e = EndedTag::default();
        }
        self.server_tags.entry(key)
            .and_modify(|r| r.instrument = instrument)
            .or_insert(TagRoute {
                instrument, price_tick: scale_tick(min_tick), size_tick: scale_size(size_min_tick), trade: false,
            });
    }

    /// Map a trade stream tag of a farm to an instrument (#292), with the
    /// minimum tick of the trade setup. A later setup of the same tag
    /// replaces it, as in the reference.
    pub fn register_trade_tag(&mut self, farm: u8, server_tag: u32, instrument: InstrumentId, min_tick: f64) {
        self.register_trade_tag_sized(farm, server_tag, instrument, min_tick, None);
    }

    /// The same with the size increment of the trade setup.
    pub fn register_trade_tag_sized(
        &mut self, farm: u8, server_tag: u32, instrument: InstrumentId, min_tick: f64, size_min_tick: Option<f64>,
    ) {
        self.trade_tags.insert(tag_key(farm, server_tag), TagRoute {
            instrument, price_tick: scale_tick(min_tick), size_tick: scale_size(size_min_tick), trade: true,
        });
    }

    /// Free the quote tags of an instrument whose market data record is no
    /// longer used (#292); its trade stream tags stay with the contract.
    pub fn drop_quote_tags(&mut self, instrument: InstrumentId) -> usize {
        let before = self.server_tags.len();
        self.server_tags.retain(|_, r| r.instrument != instrument);
        before - self.server_tags.len()
    }

    /// Number of quote and trade tags held.
    pub fn tag_counts(&self) -> (usize, usize) {
        (self.server_tags.len(), self.trade_tags.len())
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
    /// The slots of a contract (ibx#613).
    pub fn instruments_of(&self, con_id: i64) -> impl Iterator<Item = InstrumentId> + '_ {
        (0..self.active_count).filter(move |i| self.instrument_to_con_id[*i as usize] == con_id)
    }

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

    /// Look up the instrument of a server tag of the primary market data
    /// farm. O(1) hash lookup.
    #[inline(always)]
    pub fn instrument_by_server_tag(&self, server_tag: u32) -> Option<InstrumentId> {
        self.instrument_by_farm_tag(0, server_tag)
    }

    /// Look up the instrument of a server tag of a farm (#445).
    #[inline(always)]
    pub fn instrument_by_farm_tag(&self, farm: u8, server_tag: u32) -> Option<InstrumentId> {
        self.route_farm_tag(farm, server_tag).map(|r| r.instrument)
    }

    /// The instrument and price tick of a server tag of the primary market
    /// data farm.
    #[inline(always)]
    pub fn route_server_tag(&self, server_tag: u32) -> Option<TagRoute> {
        self.route_farm_tag(0, server_tag)
    }

    /// The instrument and price tick of a server tag of a farm: a quote
    /// tag, else a trade stream tag. O(1) hash lookup.
    #[inline(always)]
    pub fn route_farm_tag(&self, farm: u8, server_tag: u32) -> Option<TagRoute> {
        let key = tag_key(farm, server_tag);
        match self.server_tags.get(&key) {
            Some(r) => Some(*r),
            None => self.trade_tags.get(&key).copied(),
        }
    }

    /// Set symbol name for an instrument (e.g. "AAPL"). Used for orders.
    pub fn set_symbol(&mut self, id: InstrumentId, symbol: String) {
        self.symbols[id as usize] = Some(symbol);
    }

    /// Record the contract currency of an instrument (ibx#466).
    pub fn set_currency(&mut self, id: InstrumentId, currency: &str) {
        if !currency.is_empty() {
            self.currencies[id as usize] = Some(currency.to_uppercase());
        }
    }

    /// The exchange the instrument was registered with, upper case; empty
    /// when none (SMART).
    pub fn exchange(&self, id: InstrumentId) -> &str {
        self.exchanges[id as usize].as_deref().unwrap_or("")
    }

    /// Currency for an order on this instrument: the contract's, USD when
    /// unknown (ibx#466).
    pub fn currency(&self, id: InstrumentId) -> &str {
        self.currencies[id as usize].as_deref().unwrap_or("USD")
    }

    /// Record the security type and requested exchange for an instrument
    /// (ibx#217): order encoders derive their routing tags from these
    /// instead of hardcoding stock-on-SMART. Empty strings leave the
    /// defaults in place.
    /// Record the terms of an option instrument, for its orders.
    pub fn set_option_terms(&mut self, id: InstrumentId, terms: OptionTerms) {
        self.option_terms[id as usize] = Some(Box::new(terms));
    }

    /// The terms of an option instrument, None for any other.
    pub fn option_terms(&self, id: InstrumentId) -> Option<&OptionTerms> {
        self.option_terms[id as usize].as_deref()
    }

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
    /// Whether an instrument was registered with an empty symbol: a
    /// contract an order gives by its conId alone (ibx#666).
    pub fn blank_symbol(&self, id: InstrumentId) -> bool {
        self.symbols.get(id as usize).and_then(|s| s.as_deref()).is_some_and(str::is_empty)
    }

    pub fn symbol(&self, id: InstrumentId) -> &str {
        self.symbols[id as usize].as_deref().unwrap_or("?")
    }

    /// Set the contract's minimum tick: the one of its bid/ask entry
    /// acknowledgement. Wire prices do not use it (see `TagRoute`).
    pub fn set_min_tick(&mut self, id: InstrumentId, min_tick: f64) {
        self.min_ticks[id as usize] = min_tick;
        self.min_tick_scaled[id as usize] = scale_tick(min_tick);
    }

    /// Set the size increment `apply_tick` uses for the instrument
    /// (ibx#287). Not a positive number: 1, as the reference uses a
    /// default for an invalid one. The farm path uses the increment of
    /// each server tag instead (`TagRoute`, ibx#446).
    pub fn set_size_min_tick(&mut self, id: InstrumentId, size_min_tick: f64) {
        let scaled = (size_min_tick * QTY_SCALE as f64).round();
        let scaled = if scaled.is_finite() && scaled >= 1.0 { scaled as i64 } else { QTY_SCALE };
        self.size_min_tick_scaled[id as usize] = scaled;
    }

    /// Set the round lot the bid, ask and last sizes are multiplied by
    /// (ibx#287). Volume never is. Below 1 counts as 1.
    pub fn set_round_lot(&mut self, id: InstrumentId, round_lot: i64) {
        let lot = round_lot.max(1);
        self.round_lots[id as usize] = lot;
    }

    /// The round lot of an instrument's sizes (1 unless set).
    #[inline(always)]
    pub fn round_lot(&self, id: InstrumentId) -> i64 {
        self.round_lots[id as usize]
    }

    /// The contract's minimum tick (its bid/ask entry's), 0 before one.
    #[inline(always)]
    pub fn min_tick(&self, id: InstrumentId) -> f64 {
        self.min_ticks[id as usize]
    }

    /// Get pre-computed min_tick * PRICE_SCALE for integer price conversion.
    #[inline(always)]
    pub fn min_tick_scaled(&self, id: InstrumentId) -> i64 {
        self.min_tick_scaled[id as usize]
    }

    /// Apply one decoded tick to the instrument's quote (ibx#448). Prices
    /// are the magnitude times `price_tick`: the minimum tick of the
    /// tick's server tag times PRICE_SCALE (`TagRoute`). Sizes are the magnitude
    /// times the size increment, in the Qty fixed point, and for bid, ask
    /// and last also times the round lot (ibx#287). The last trade time is
    /// its base, or the base plus a delta, in epoch seconds, kept in ns.
    /// A value whose scaling overflows is not stored: the field keeps its
    /// last value rather than a wrapped number (ibx#272).
    /// `trade` is set for a tick of a trade stream tag: there the attribute
    /// type is the trade's status; on a quote it carries, as the
    /// auto-execution type does, whether the bid and the ask can execute
    /// automatically (ibx#446).
    #[inline]
    pub fn apply_tick(&mut self, id: InstrumentId, price_tick: i64, trade: bool, tick: &RawTick) {
        let size_tick = self.size_min_tick_scaled[id as usize];
        self.apply_tick_sized(id, price_tick, size_tick, trade, tick);
    }

    /// `apply_tick` with the size increment of the tick's server tag times
    /// QTY_SCALE (`TagRoute`), as the farm path applies it (ibx#446).
    #[inline]
    pub fn apply_tick_sized(&mut self, id: InstrumentId, price_tick: i64, size_tick: i64, trade: bool, tick: &RawTick) {
        #[inline(always)]
        fn set(field: &mut i64, value: Option<i64>) {
            if let Some(v) = value { *field = v; }
        }
        let i = id as usize;
        let mts = price_tick;
        // In shares, or in round lots in the 10.40 form (ibx#663).
        let lot = self.round_lots[i];
        // Saturating: a bad frame can carry any increment (robustness tests).
        let sizes = size_tick.saturating_mul(if self.lot_sizes { lot } else { 1 });
        // Decimal places of the value (the 10.51 form's extended entries).
        let m = if tick.scale == 0 || tick.tick_type == td::O_VOLUME {
            tick.magnitude
        } else {
            tick.magnitude / 10i64.pow(tick.scale.min(18) as u32)
        };
        let q = &mut self.quotes[i];
        match tick.tick_type {
            td::O_BID_PRICE => set(&mut q.bid, m.checked_mul(mts)),
            td::O_ASK_PRICE => set(&mut q.ask, m.checked_mul(mts)),
            td::O_LAST_PRICE => set(&mut q.last, m.checked_mul(mts)),
            td::O_CLOSE_PRICE => set(&mut q.close, m.checked_mul(mts)),
            td::O_HIGH_PRICE => set(&mut q.high, m.checked_mul(mts)),
            td::O_LOW_PRICE => set(&mut q.low, m.checked_mul(mts)),
            td::O_OPEN_PRICE => set(&mut q.open, m.checked_mul(mts)),
            td::O_BID_SIZE => {
                set(&mut q.bid_size, m.checked_mul(sizes));
                self.marks[i].set_seen(SizeKind::Bid);
            }
            td::O_ASK_SIZE => {
                set(&mut q.ask_size, m.checked_mul(sizes));
                self.marks[i].set_seen(SizeKind::Ask);
            }
            td::O_LAST_SIZE => {
                set(&mut q.last_size, last_size_value(crate::client_core::session_level(), tick, m, sizes));
                self.marks[i].set_seen(SizeKind::Last);
            }
            // The 10.51 form gives the volume in shares with decimals; the
            // client gets whole shares, or round lots when the session asks
            // for volumes in lots, rounded down either way (the reference
            // 10.51, paper: 13/10/2026 with volumes in lots, 64962.113269
            // shares of a lot of 40 gave 1624, eight of eight values floored;
            // 20/11/2026 without, AAPL's day volume in shares; ibx#763).
            td::O_VOLUME if tick.scale > 0 => {
                let lots = if self.volume_lots { lot.max(1) } else { 1 };
                let unit = 10i64.pow(tick.scale.min(18) as u32).saturating_mul(lots);
                set(&mut q.volume, (m / unit).checked_mul(size_tick));
                self.marks[i].set_seen(SizeKind::Volume);
            }
            td::O_VOLUME => {
                set(&mut q.volume, m.checked_mul(size_tick));
                self.marks[i].set_seen(SizeKind::Volume);
            }
            td::O_BID_EXCH => q.bid_exch_mask = m,
            td::O_ASK_EXCH => q.ask_exch_mask = m,
            td::O_LAST_EXCH => q.last_exch_mask = m,
            td::O_AUTO_EXEC if !trade && !tick.stats_block => self.marks[i].set_auto_bits(m),
            td::O_PRE_OPEN if !trade && !tick.stats_block => self.marks[i].set_pre_open(m != 0),
            td::O_ATTRIBUTES if !tick.stats_block => {
                if trade { self.marks[i].set_halted(m) } else { self.marks[i].set_auto_bits(m) }
            }
            // On a daily-stats block this type is the close date, not a time.
            td::O_TIMESTAMP_BASE if !tick.stats_block && m > 0 => {
                if let Some(ns) = (m as u64).checked_mul(NS_PER_SEC) {
                    self.last_ts_base[i] = m;
                    q.timestamp_ns = ns;
                }
            }
            td::O_TIMESTAMP_DELTA if m > 0 => {
                if let Some(ns) = self.last_ts_base[i].checked_add(m).and_then(|s| (s as u64).checked_mul(NS_PER_SEC)) {
                    q.timestamp_ns = ns;
                }
            }
            _ => {}
        }
    }

    #[inline(always)]
    pub fn quote(&self, id: InstrumentId) -> &Quote {
        &self.quotes[id as usize]
    }

    /// What the farm told about the quote beside its fields (ibx#446).
    #[inline(always)]
    pub fn marks(&self, id: InstrumentId) -> QuoteMarks {
        self.marks[id as usize]
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
        self.server_tags.clear();
        self.trade_tags.clear();
    }

    /// Clear the server tags of one farm, whose connection was lost (#445).
    pub fn clear_farm_tags(&mut self, farm: u8) {
        self.server_tags.retain(|k, _| (k >> 32) as u8 != farm);
        self.trade_tags.retain(|k, _| (k >> 32) as u8 != farm);
    }

    /// Zero the quote of one instrument, whose farm was lost (#445).
    pub fn zero_quote(&mut self, id: InstrumentId) {
        if (id as usize) < MAX_INSTRUMENTS {
            self.quotes[id as usize] = Quote::default();
            self.last_ts_base[id as usize] = 0;
        }
    }

    /// Zero all quote data to prevent stale price trading after farm disconnect.
    pub fn zero_all_quotes(&mut self) {
        for i in 0..self.active_count as usize {
            self.quotes[i] = Quote::default();
            self.last_ts_base[i] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PRICE_SCALE;

    // ibx#508: a freed slot keeps its contract's bid and ask for the next
    // request, by conId; the table is fixed and overwrites the oldest.
    #[test]
    fn a_freed_slot_keeps_its_bid_and_ask() {
        let mut m = MarketState::new();
        let id = m.try_register(265598).unwrap();
        let q = &mut m.quotes[id as usize];
        q.bid = 34045 * PRICE_SCALE / 100;
        q.ask = 34052 * PRICE_SCALE / 100;
        q.bid_size = 320 * QTY_SCALE;
        q.ask_size = 40 * QTY_SCALE;
        q.last_exch_mask = 8;
        m.marks[id as usize].set_auto_bits(12);
        m.unregister(id);
        assert_eq!(m.quote(id).bid, 0);
        let k = m.take_kept(265598).unwrap();
        assert_eq!((k.bid, k.bid_size, k.ask, k.ask_size, k.last_exch), (q_bid(), 320 * QTY_SCALE, 34052 * PRICE_SCALE / 100, 40 * QTY_SCALE, 8));
        assert_eq!(QuoteMarks::from_auto_word(k.auto).ask_auto(), Some(true));
        // Taken once.
        assert!(m.take_kept(265598).is_none());
        // A slot without a bid or an ask keeps nothing.
        let id = m.try_register(1).unwrap();
        m.unregister(id);
        assert!(m.take_kept(1).is_none());
        fn q_bid() -> i64 { 34045 * PRICE_SCALE / 100 }
    }

    // ibx#694 (the reference 10.51, paper, 14/10/2026): the farm's data for
    // a request whose slot was freed goes to the kept bid and ask, scaled as
    // the request's; once the tag is acknowledged again it routes as usual.
    #[test]
    fn data_after_the_cancel_goes_to_the_kept_quote() {
        let mut m = MarketState::new();
        let id = m.try_register(9599491).unwrap();
        m.register_farm_tag(0, 4172, id, 0.01);
        let q = &mut m.quotes[id as usize];
        q.bid = 1195 * PRICE_SCALE / 100;
        q.ask = 1196 * PRICE_SCALE / 100;
        q.bid_size = 32800 * QTY_SCALE;
        q.ask_size = 96700 * QTY_SCALE;
        m.unregister(id);
        let tick = |tick_type, magnitude| RawTick { server_tag: 4172, tick_type, magnitude, stats_block: false, first: false, scale: 0 };
        assert!(m.apply_ended_tick(0, &tick(td::O_BID_SIZE, 15800)));
        assert!(m.apply_ended_tick(0, &tick(td::O_ASK_SIZE, 96900)));
        assert!(m.apply_ended_tick(0, &tick(td::O_BID_PRICE, 1194)));
        assert!(!m.apply_ended_tick(1, &tick(td::O_BID_SIZE, 1)), "another farm's tag");
        let k = m.take_kept(9599491).unwrap();
        assert_eq!((k.bid, k.bid_size, k.ask, k.ask_size),
            (1194 * PRICE_SCALE / 100, 15800 * QTY_SCALE, 1196 * PRICE_SCALE / 100, 96900 * QTY_SCALE));
        let id = m.try_register(9599491).unwrap();
        m.register_farm_tag(0, 4172, id, 0.01);
        assert!(!m.apply_ended_tick(0, &tick(td::O_BID_SIZE, 1)), "routed again");
    }

    #[test]
    fn the_kept_table_overwrites_the_oldest_and_seeds_a_slot() {
        let mut m = MarketState::new();
        for c in 1..=(KEPT_QUOTES as i64 + 1) {
            m.keep_quote(KeptQuote::new(c, c * PRICE_SCALE, QTY_SCALE, c * PRICE_SCALE + 1, QTY_SCALE, 0));
        }
        // The first one was the oldest.
        assert!(m.take_kept(1).is_none());
        assert!(m.take_kept(2).is_some());
        // The same conId again: its own entry, not a second one.
        m.keep_quote(KeptQuote::new(3, 7, 1, 8, 1, 0));
        assert_eq!(m.take_kept(3).map(|k| k.bid), Some(7));
        assert!(m.take_kept(3).is_none());
        // Seeded into a new slot: the quote and its sizes seen.
        let id = m.try_register(9).unwrap();
        m.seed_kept(id, &KeptQuote::new(9, 5, 2, 6, 3, 0));
        assert_eq!((m.quote(id).bid, m.quote(id).ask_size), (5, 3));
        assert!(m.marks(id).seen(SizeKind::Bid) && m.marks(id).seen(SizeKind::Ask));
    }

    // ibx#508: an option's underlying book, as the farm sent it, scaled as
    // the farm path scales it.
    #[test]
    fn a_kept_quote_from_the_wire_is_scaled_as_the_farm_path() {
        let k = KeptQuote::from_wire(265598, [Some(34188), Some(34207), Some(2), Some(1)], 0.01, Some(1.0), 40, Some(12));
        assert_eq!((k.bid, k.ask), (34188 * scale_tick(0.01), 34207 * scale_tick(0.01)));
        assert_eq!((k.bid_size, k.ask_size), (80 * QTY_SCALE, 40 * QTY_SCALE));
        assert_eq!(QuoteMarks::from_auto_word(k.auto).bid_auto(), Some(true));
        // No ask: kept as 0.
        assert_eq!(KeptQuote::from_wire(1, [Some(1), None, None, None], 0.01, None, 1, None).ask, 0);
    }

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
        ms.register_server_tag(42, id, 0.01);
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
        ms.register_server_tag(42, aapl, 0.01);
        assert_eq!(ms.instrument_by_server_tag(42), Some(aapl));
        assert_eq!(ms.instrument_by_server_tag(99), None);
    }

    // ── ibx#281: server tags above 65,535 ──

    #[test]
    fn server_tag_large_values_resolve() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        let opt = ms.register(924059488);
        // Quote ack tag and trade stream tag seen in one session.
        ms.register_server_tag(419_808, aapl, 0.01);
        ms.register_server_tag(128_516, opt, 0.01);
        assert_eq!(ms.instrument_by_server_tag(419_808), Some(aapl));
        assert_eq!(ms.instrument_by_server_tag(128_516), Some(opt));
        // Tags that alias the old 65,536-slot table must not match.
        assert_eq!(ms.instrument_by_server_tag(419_808 % 65_536), None);
        assert_eq!(ms.instrument_by_server_tag(128_516 % 65_536), None);
        // The largest tag the decoder can read.
        ms.register_server_tag(0x7FFF_FFFF, aapl, 0.01);
        assert_eq!(ms.instrument_by_server_tag(0x7FFF_FFFF), Some(aapl));
    }

    #[test]
    fn server_tag_large_values_cleared_and_unregistered() {
        let mut ms = MarketState::new();
        let a = ms.register(1);
        let b = ms.register(2);
        ms.register_server_tag(419_808, a, 0.01);
        ms.register_server_tag(128_516, b, 0.01);
        ms.unregister(a);
        assert_eq!(ms.instrument_by_server_tag(419_808), None);
        assert_eq!(ms.instrument_by_server_tag(128_516), Some(b));
        ms.clear_server_tags();
        assert_eq!(ms.instrument_by_server_tag(128_516), None);
    }

    #[test]
    fn tag_hasher_spreads_consecutive_tags() {
        use std::hash::{BuildHasher, BuildHasherDefault};
        let bh = BuildHasherDefault::<TagHasher>::default();
        // Consecutive tags keep distinct low bits (bucket index).
        let mut low: Vec<u64> = (128_500u32..128_564).map(|t| bh.hash_one(t) & 63).collect();
        low.sort_unstable();
        low.dedup();
        assert_eq!(low.len(), 64);
    }

    #[test]
    fn server_tag_overwrite() {
        let mut ms = MarketState::new();
        let aapl = ms.register(265598);
        ms.register_server_tag(42, aapl, 0.01);
        ms.register_server_tag(42, aapl, 0.01); // same value, overwrites
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
        ms.register_server_tag(10, a, 0.01);
        ms.register_server_tag(20, b, 0.01);
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
        ms.register_server_tag(10, a, 0.01);
        ms.register_server_tag(20, b, 0.01);
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

    // ── The minimum price tick is kept per server tag ──

    #[test]
    fn each_tag_keeps_its_own_tick() {
        let mut ms = MarketState::new();
        let eur = ms.register(12087792);
        ms.register_farm_tag(3, 24, eur, 0.00001);
        ms.register_farm_tag(3, 25, eur, 0.00005);
        ms.register_trade_tag(3, 26, eur, 0.00005);
        let route = |tag| ms.route_farm_tag(3, tag).map(|r| (r.instrument, r.price_tick));
        assert_eq!(route(24), Some((eur, 1_000)));
        assert_eq!(route(25), Some((eur, 5_000)));
        assert_eq!(route(26), Some((eur, 5_000)));
        assert_eq!(ms.route_farm_tag(0, 24), None, "another farm's tag");
        assert_eq!(ms.min_tick(eur), 0.0, "acks set no contract tick here");
    }

    // Two entries acked with one tag share one record: the first tick
    // stays. A trade setup of a tag replaces the earlier one.
    #[test]
    fn a_shared_quote_tag_keeps_the_first_tick_a_trade_tag_the_last() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.register_server_tag(1101, id, 0.01);
        ms.register_server_tag(1101, id, 0.05);
        assert_eq!(ms.route_server_tag(1101), Some(TagRoute { instrument: id, price_tick: CENT, size_tick: QTY_SCALE, trade: false }));
        ms.register_trade_tag(0, 1098, id, 0.05);
        ms.register_trade_tag(0, 1098, id, 0.01);
        assert_eq!(ms.route_server_tag(1098), Some(TagRoute { instrument: id, price_tick: CENT, size_tick: QTY_SCALE, trade: true }));
    }

    // A trade tag moved to the contract's other slot keeps its tick.
    #[test]
    fn a_trade_tag_moved_to_another_slot_keeps_its_tick() {
        let mut ms = MarketState::new();
        let a = ms.register(12087792);
        let b = ms.try_register_unresolved().unwrap();
        ms.resolve_con_id(b, 12087792);
        ms.register_trade_tag(0, 26, a, 0.00005);
        ms.unregister(a);
        assert_eq!(ms.route_server_tag(26), Some(TagRoute { instrument: b, price_tick: 5_000, size_tick: QTY_SCALE, trade: true }));
    }

    // ── ibx#448: wire tick types land in the right quote fields ──

    const CENT: i64 = PRICE_SCALE / 100;

    fn raw(tick_type: u64, magnitude: i64) -> RawTick {
        RawTick { server_tag: 7, tick_type, magnitude, stats_block: false, first: true, scale: 0 }
    }

    #[test]
    fn apply_tick_daily_stats_types() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_min_tick(id, 0.01);
        // One stats block: close, last size, high, volume, open.
        for t in [raw(3, 25_512), raw(6, 3), raw(8, 25_730), raw(10, 1466), raw(22, 25_401)] {
            ms.apply_tick(id, CENT, false, &RawTick { stats_block: true, ..t });
        }
        let q = ms.quote(id);
        assert_eq!(q.close, 25_512 * PRICE_SCALE / 100);
        assert_eq!(q.last_size, 3 * QTY_SCALE);
        assert_eq!(q.high, 25_730 * PRICE_SCALE / 100);
        assert_eq!(q.volume, 1466 * QTY_SCALE);
        assert_eq!(q.open, 25_401 * PRICE_SCALE / 100);
        // Nothing else moved.
        assert_eq!((q.bid, q.ask, q.last, q.low), (0, 0, 0, 0));
        assert_eq!((q.bid_size, q.ask_size), (0, 0));
        assert_eq!(q.timestamp_ns, 0);
    }

    // ibx#272: a magnitude whose scaling overflows leaves the field as it
    // was, in debug and release alike.
    #[test]
    fn apply_tick_overflow_keeps_the_last_value() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_min_tick(id, 0.01);
        ms.apply_tick(id, CENT, false, &raw(0, 25_500));
        ms.apply_tick(id, CENT, false, &raw(4, 57));
        ms.apply_tick(id, CENT, false, &raw(20, 1_790_159_184));
        for t in [raw(0, i64::MAX), raw(4, i64::MAX), raw(10, i64::MIN), raw(20, i64::MAX), raw(21, i64::MAX)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!(q.bid, 25_500 * PRICE_SCALE / 100);
        assert_eq!(q.bid_size, 57 * QTY_SCALE);
        assert_eq!(q.volume, 0);
        assert_eq!(q.timestamp_ns, 1_790_159_184 * NS_PER_SEC);
    }

    #[test]
    fn apply_tick_quote_and_trade_types() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_min_tick(id, 0.01);
        for t in [raw(0, 25_500), raw(1, 25_502), raw(2, 25_501), raw(4, 57), raw(5, 12), raw(9, 25_300),
                  raw(16, 512), raw(17, 64), raw(27, 8)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!(q.bid, 25_500 * PRICE_SCALE / 100);
        assert_eq!(q.ask, 25_502 * PRICE_SCALE / 100);
        assert_eq!(q.last, 25_501 * PRICE_SCALE / 100);
        assert_eq!(q.low, 25_300 * PRICE_SCALE / 100);
        assert_eq!((q.bid_size, q.ask_size), (57 * QTY_SCALE, 12 * QTY_SCALE));
        assert_eq!((q.bid_exch_mask, q.ask_exch_mask, q.last_exch_mask), (512, 64, 8));
        // Attribute bits and the unknown int change no field.
        let before = (q.bid, q.ask, q.last, q.bid_size, q.volume, q.timestamp_ns);
        for t in [raw(7, 12), raw(11, 1), raw(12, 99), raw(13, 3)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!((q.bid, q.ask, q.last, q.bid_size, q.volume, q.timestamp_ns), before);
    }

    // ── ibx#287: sizes in the Qty fixed point, round lot on bid/ask/last ──

    #[test]
    fn apply_tick_sizes_are_fixed_point() {
        let mut ms = MarketState::new();
        let id = ms.register(551601503); // MNQ: sizes as on the wire
        for t in [raw(4, 1), raw(5, 7), raw(6, 2), raw(10, 163)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!(q.bid_size, QTY_SCALE, "a size of 1 is 1.0, not 0.0001");
        assert_eq!(q.ask_size, 7 * QTY_SCALE);
        assert_eq!(q.last_size, 2 * QTY_SCALE);
        assert_eq!(q.volume, 163 * QTY_SCALE);
    }

    #[test]
    fn apply_tick_takes_shares_and_the_round_lot_only_in_the_lot_form() {
        // The sizes of 10.51 come in shares: as they are (ibx#663, ibx#604).
        let mut ms = MarketState::new();
        let id = ms.register(265598); // AAPL, round lot 40
        ms.set_size_min_tick(id, 1.0);
        ms.set_round_lot(id, 40);
        for t in [raw(4, 57), raw(5, 3), raw(6, 2), raw(10, 1466)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!((q.bid_size, q.ask_size, q.last_size, q.volume),
                   (57 * QTY_SCALE, 3 * QTY_SCALE, 2 * QTY_SCALE, 1466 * QTY_SCALE));
        // The 10.40 form (its replays): bid, ask and last in round lots.
        ms.set_lot_sizes(true);
        for t in [raw(4, 57), raw(5, 3), raw(6, 2), raw(10, 1466)] {
            ms.apply_tick(id, CENT, false, &t);
        }
        let q = ms.quote(id);
        assert_eq!(q.bid_size, 2280 * QTY_SCALE);
        assert_eq!(q.ask_size, 120 * QTY_SCALE);
        assert_eq!(q.last_size, 80 * QTY_SCALE);
        assert_eq!(q.volume, 1466 * QTY_SCALE, "volume is never multiplied by the lot");
    }

    // ibx#446: the size increment is the server tag's, as the price tick:
    // a quote tag keeps the first acknowledgement's, a trade tag the
    // latest setup's; the round lot stays the contract's.
    #[test]
    fn size_increment_per_server_tag() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_round_lot(id, 40);
        ms.set_lot_sizes(true); // the 10.40 form (ibx#663)
        ms.register_farm_tag_sized(0, 1101, id, 0.01, Some(0.01));
        ms.register_farm_tag_sized(0, 1101, id, 0.01, Some(1.0));
        ms.register_trade_tag_sized(0, 1098, id, 0.01, Some(0.5));
        ms.register_trade_tag_sized(0, 1098, id, 0.01, Some(2.0));
        ms.register_farm_tag_sized(0, 7, id, 0.01, None);
        let route = |ms: &MarketState, tag| ms.route_server_tag(tag).unwrap();
        assert_eq!(route(&ms, 1101).size_tick, QTY_SCALE / 100, "the first ack stays");
        assert_eq!(route(&ms, 1098).size_tick, 2 * QTY_SCALE, "the latest setup");
        assert_eq!(route(&ms, 7).size_tick, QTY_SCALE, "absent: 1");
        let apply = |ms: &mut MarketState, tag, t: RawTick| {
            let r = route(ms, tag);
            ms.apply_tick_sized(r.instrument, r.price_tick, r.size_tick, r.trade, &t);
        };
        apply(&mut ms, 1101, raw(4, 500));
        apply(&mut ms, 1098, raw(6, 3));
        apply(&mut ms, 1098, RawTick { stats_block: true, ..raw(10, 1000) });
        let q = ms.quote(id);
        assert_eq!(q.bid_size, 200 * QTY_SCALE); // 500 x 0.01 x 40
        assert_eq!(q.last_size, 240 * QTY_SCALE); // 3 x 2 x 40
        assert_eq!(q.volume, 2000 * QTY_SCALE); // 1000 x 2, no lot
    }

    #[test]
    fn size_min_tick_scales_every_size() {
        let mut ms = MarketState::new();
        let id = ms.register(1);
        ms.set_size_min_tick(id, 0.01);
        ms.set_round_lot(id, 100);
        ms.set_lot_sizes(true); // the 10.40 form (ibx#663)
        ms.apply_tick(id, CENT, false, &raw(4, 5));
        ms.apply_tick(id, CENT, false, &raw(10, 250));
        assert_eq!(ms.quote(id).bid_size, 5 * QTY_SCALE); // 5 x 0.01 x 100
        assert_eq!(ms.quote(id).volume, 25 * QTY_SCALE / 10); // 250 x 0.01
        // An invalid increment falls back to 1.
        ms.set_size_min_tick(id, 0.0);
        ms.set_round_lot(id, 0);
        assert_eq!(ms.round_lot(id), 1);
        ms.apply_tick(id, CENT, false, &raw(4, 5));
        assert_eq!(ms.quote(id).bid_size, 5 * QTY_SCALE);
    }

    // ibx#655: a size increment finer than 0.0001 (a crypto pair, 1e-8) is
    // kept, no longer read as an invalid one (1).
    #[test]
    fn a_fine_size_increment_is_kept() {
        assert_eq!(scale_size(Some(1e-8)), 1);
        assert_eq!(scale_size(Some(1e-5)), 1_000);
        assert_eq!(scale_size(Some(0.0)), QTY_SCALE);
        let mut ms = MarketState::new();
        let id = ms.register(1);
        ms.set_size_min_tick(id, 1e-8);
        assert_eq!(ms.size_min_tick_scaled[id as usize], 1);
    }

    #[test]
    fn last_size_decimals_from_level_222() {
        // ibx#613: a last size of 0.4 shares, one decimal, on an increment of
        // 1: whole units below 222 (0), its decimals from 222.
        let tick = RawTick { server_tag: 1, tick_type: td::O_LAST_SIZE, magnitude: 4, stats_block: false, first: true, scale: 1 };
        assert_eq!(last_size_value(214, &tick, 0, QTY_SCALE), Some(0));
        assert_eq!(last_size_value(222, &tick, 0, QTY_SCALE), Some(QTY_SCALE * 4 / 10));
        // To the quantity's steps of 0.00000001 (ibx#655): 0.00123456.
        let fine = RawTick { magnitude: 123_456, scale: 8, ..tick };
        assert_eq!(last_size_value(222, &fine, 0, QTY_SCALE), Some(123_456));
        // A whole value: as before.
        let whole = RawTick { magnitude: 80, scale: 0, ..tick };
        assert_eq!(last_size_value(222, &whole, 80, QTY_SCALE), Some(80 * QTY_SCALE));
    }

    #[test]
    fn unregister_resets_size_scale() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.set_size_min_tick(id, 0.5);
        ms.set_round_lot(id, 40);
        ms.unregister(id);
        let reused = ms.register(999);
        assert_eq!(reused, id);
        assert_eq!(ms.round_lot(reused), 1);
        ms.apply_tick(reused, CENT, false, &raw(4, 3));
        assert_eq!(ms.quote(reused).bid_size, 3 * QTY_SCALE);
    }

    #[test]
    fn apply_tick_last_trade_time_is_base_plus_delta() {
        let mut ms = MarketState::new();
        let id = ms.register(265598);
        ms.apply_tick(id, CENT, false, &raw(20, 1_790_159_184));
        assert_eq!(ms.quote(id).timestamp_ns, 1_790_159_184 * NS_PER_SEC);
        ms.apply_tick(id, CENT, false, &raw(21, 3));
        assert_eq!(ms.quote(id).timestamp_ns, 1_790_159_187 * NS_PER_SEC);
        // A later delta is from the same base, not from the last time.
        ms.apply_tick(id, CENT, false, &raw(21, 5));
        assert_eq!(ms.quote(id).timestamp_ns, 1_790_159_189 * NS_PER_SEC);
        // The close date on a stats block is not a time.
        ms.apply_tick(id, CENT, false, &RawTick { stats_block: true, ..raw(20, 20_260_922) });
        assert_eq!(ms.quote(id).timestamp_ns, 1_790_159_189 * NS_PER_SEC);
        // A new base resets the time to it.
        ms.apply_tick(id, CENT, false, &raw(20, 1_790_159_300));
        assert_eq!(ms.quote(id).timestamp_ns, 1_790_159_300 * NS_PER_SEC);
    }

    #[test]
    fn zero_all_quotes_no_registered_is_noop() {
        let mut ms = MarketState::new();
        ms.zero_all_quotes(); // should not panic
    }
}
