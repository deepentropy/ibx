use crate::engine::market_state::MarketState;
use crate::types::*;
use std::collections::HashMap;
use std::time::Instant;


/// TSC-calibrated clock for hot-path timestamps.
pub struct Clock {
    start: std::time::Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }

    /// Monotonic nanoseconds since engine start. Fast, no syscall.
    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Wall-clock Unix timestamp in seconds.
    pub fn now_utc(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

/// The context passed to strategy callbacks. Provides market data access and
/// order management. All hot-path data is pre-allocated.
/// Most finished orders kept for `Context::finished_status`.
pub const FINISHED_ORDERS_MAX: usize = 65_536;

/// What an order gave of its contract when looked up by symbol: the
/// symbol, its type and route, its exchange, its currency and its terms
/// (ibx#718).
pub(crate) type OrderContractKey = (String, (String, String), String, String, SecDefFilters);

/// What a server-reported status did to an order (ibx#212 ibx#473).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusChange {
    /// The status moved forward.
    Changed,
    /// The report restates the current status.
    Same,
    /// A stale or reordered report, dropped by the guard.
    Stale,
    /// The engine does not hold this order.
    Unknown,
}

/// What the reference's order book holds for an order beyond the order
/// itself, for its global cancel: the
/// place the order took in the book, its parent and OCA group, and the API
/// client of an order of another session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BookEntry {
    /// Order of insertion in the book.
    pub seq: u64,
    /// The parent's order id, 0 for none.
    pub parent: OrderId,
    /// The OCA group, empty for none.
    pub oca_group: String,
    /// The API client of an order the server reported from another
    /// session; None for an order of this client.
    pub owner: Option<i64>,
}

/// An order that ended and that the reference's book still holds
/// (ibx#545). A global cancel walks them with the working orders and
/// answers 161 for each:
/// - a cancelled order stays [`CANCELLED_ORDER_KEPT`] after the report
///   that ended it, then a timer takes it out (paper 10/10/2026: 161 at 3,
///   6 and 9 s, none at 10.2 s and later), whether its own cancel, its
///   parent's or a global cancel ended it;
/// - an order the server refused (201) stays as long (paper 10/10/2026:
///   161 at 9 s, none at 14 and 16 s, and its cancel at 19 s finds no
///   order); a global cancel that answers for it does not take it out
///   (paper 11/10/2026: 161 at 3 s and again at 6 s, none at 12 s;
///   ibx#551);
/// - a filled order stays [`FILLED_ORDER_KEPT`] (paper 12/10/2026, regular
///   hours, reference 10.51: 161 at 3, 8, 12 and 20 s after the fill, none
///   at 35 and 65 s; ibx#551);
/// - an order that was never sent (refused before the send, or held and
///   cancelled) gets no report, so nothing takes it out: 161 at every
///   global cancel of the session (paper 09/10/2026, three runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndedOrder {
    pub order_id: OrderId,
    /// The id the reference's book holds it under (its permId).
    pub server_id: OrderId,
    pub entry: BookEntry,
    /// When the reference takes it out of its book; None for never.
    pub until: Option<Instant>,
}

/// How long the reference keeps a cancelled order in its book.
pub(crate) const CANCELLED_ORDER_KEPT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the reference keeps a filled order in its book (ibx#551).
pub(crate) const FILLED_ORDER_KEPT: std::time::Duration = std::time::Duration::from_secs(30);

/// The table size of a `java.util.HashMap` that held `peak` entries: 16,
/// doubled each time the entries pass three quarters of it; it never
/// shrinks.
pub(crate) fn book_table_size(peak: usize) -> usize {
    let mut size = 16;
    while peak > size * 3 / 4 {
        size *= 2;
    }
    size
}

/// The bucket of a permId in the reference's book, a Java hash map keyed
/// by a 64-bit integer: the key's hash,
/// spread, masked by the table size. The book is walked bucket by bucket,
/// each in insertion order.
pub(crate) fn book_bucket(perm_id: OrderId, table: usize) -> usize {
    let v = perm_id as u64;
    let h = (v ^ (v >> 32)) as u32;
    ((h ^ (h >> 16)) as usize) & (table - 1)
}

/// The reference's order id generator: the
/// server's order id of each new order and preview, the id part of its
/// ClOrdID and the permId the client sees. The API order id goes in 6121
/// and is not this id. The first id has random high bits and the seconds
/// of the clock in its low 20 bits; then it counts up, and starts again
/// from a new first id at an id the reference does not take (0 and the
/// int bounds).
pub(crate) struct OrderIdGenerator {
    next: i32,
    rng: u64,
}

impl OrderIdGenerator {
    pub(crate) fn new() -> Self {
        Self { next: 0, rng: crate::engine::bracket::seed() }
    }

    /// The next id; the caller skips the ids in use.
    pub(crate) fn next(&mut self) -> i32 {
        if matches!(self.next, 0 | i32::MAX | i32::MIN) {
            self.next = self.first();
        }
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }

    /// Start the ids at `first` (tests).
    #[cfg(test)]
    pub(crate) fn start_at(&mut self, first: i32) {
        self.next = first;
    }

    /// `Math.abs((random & 0xFFF00000) | (seconds & 0xFFFFF))`.
    fn first(&mut self) -> i32 {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        let random = (x >> 32) as u32;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()) as u32;
        (((random & 0xFFF0_0000) | (secs & 0x000F_FFFF)) as i32).wrapping_abs()
    }
}

/// What the server last reported for a TRAIL LIMIT order. The offset is
/// restated on its replace; all three fill the reports that omit them and
/// show in openOrder (ib-agent#194, ib-agent#195, ibx#491). 0 = not reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrailLimitReported {
    /// Limit offset.
    pub offset: Price,
    /// Limit price.
    pub limit: Price,
    /// Stop price, which the server moves as the market moves.
    pub stop: Price,
}

/// What a future's or an index's definition names (ibx#727): its
/// underlying, the index it names as its reference, the exchange and API
/// security type of its record.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DefLink {
    pub(crate) under_con_id: i64,
    pub(crate) index_con_id: i64,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    /// The key of the trading hours of its class (ibx#730).
    pub(crate) join_key: String,
    pub(crate) currency: String,
}

/// The kinds of request that ask their contract's definition each time
/// they are given by a conId alone (ibx#732).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DefAsk {
    Historical,
    RealTimeBars,
    Depth,
}

pub struct Context {
    pub(crate) market: MarketState,
    positions: [i64; MAX_INSTRUMENTS],
    open_orders: HashMap<OrderId, Order>,
    pub(crate) pending_orders: OrderBuffer,
    pub(crate) account: AccountState,
    clock: Clock,
    next_order_id: OrderId,
    /// ClOrdID version counter per order for modify chaining (orderId.0 → .1 → .2).
    pub(crate) modify_versions: HashMap<OrderId, u32>,
    /// Last ClOrdID the server reported (or we emitted) for each order, exactly as
    /// it appeared on the wire. Used as the OrigClOrdID on cancel/modify so that
    /// legacy orders recorded without a `.{ver}` suffix still match — see ibx#179.
    pub(crate) last_clord: HashMap<OrderId, String>,
    /// Orders gone for good since the order connection last looked: the
    /// records it keeps for them are dropped there (ibx#700).
    pub(crate) purged_orders: Vec<OrderId>,
    /// The server's order id (the id part of the ClOrdID) of each order
    /// held under another key, to that key: the orders of this session,
    /// kept under their API order id (ibx#466), and the orders of an
    /// earlier session kept under the API order id their report gives.
    pub(crate) recovered_keys: HashMap<OrderId, OrderId>,
    /// The other way: the key of an order to its server's order id.
    pub(crate) server_ids: HashMap<OrderId, OrderId>,
    /// The keys of the orders of earlier sessions held under their API
    /// order id (ibx#538): a new order of this session with that id is not
    /// that order.
    pub(crate) recovered: std::collections::HashSet<OrderId>,
    /// The reference's order id generator: the server's order id of each
    /// new order and preview.
    pub(crate) order_ids: OrderIdGenerator,
    /// ClOrdID of the cancel sent for each order, until the order ends or
    /// the cancel is rejected. Reports carrying it are about the cancel, not
    /// a new version of the order (ibx#464).
    pub(crate) cancel_clord: HashMap<OrderId, String>,
    /// Orders whose cancel or modify the server refused, until the answer
    /// to the status request sent for them (ibx#252).
    pub(crate) status_queries: std::collections::HashSet<OrderId>,
    /// The last limit offset, limit price and stop price the server
    /// reported for a TRAIL LIMIT order (ib-agent#194, ibx#491).
    pub(crate) trail_limit_reported: HashMap<OrderId, TrailLimitReported>,
    /// The scale orders whose auto-reset a report gave (ibx#751).
    pub(crate) scale_reset_reported: std::collections::HashSet<OrderId>,
    /// What a scale order that started again has to fill in all, when more
    /// than its quantity: filled plus left, as its last report gave them
    /// (ibx#751).
    pub(crate) scale_totals: HashMap<OrderId, crate::types::Qty>,
    /// Orders sent with the price management flag: a report that leaves
    /// it out before any other (a refusal) shows the order as sent
    /// (ibx#605).
    pub(crate) price_mgmt_sent: std::collections::HashSet<OrderId>,
    /// Orders a report gave as held: their compete values show
    /// unset from then on (ibx#605).
    pub(crate) compete_cleared: std::collections::HashSet<OrderId>,
    /// The last stop price the server reported for an order, the
    /// reference's last known value a trailing replace compares with
    /// (ibx#263). A replace does not change it.
    pub(crate) reported_stop: HashMap<OrderId, crate::types::Price>,
    /// What decides outside-RTH, by conId and order exchange (ibx#465).
    pub(crate) rth_types: HashMap<(i64, String), crate::engine::outside_rth::RthTypes>,
    /// The minimum order size of a contract whose definition gives one, in
    /// whole units (ibx#688: 100 for a Tokyo stock).
    pub(crate) min_sizes: HashMap<i64, u64>,
    /// Definition lookups in flight for `rth_types`: (320 id, key, deadline).
    pub(crate) rth_lookups: Vec<(String, (i64, String), std::time::Instant)>,
    /// What-if previews waiting for their answer, by the ClOrdID each was
    /// sent under: the API order id and the instrument (ibx#462). They are
    /// not in the order table.
    pub(crate) what_ifs: HashMap<String, (OrderId, InstrumentId)>,
    /// Set while a what-if is encoded: the ClOrdID it goes out under.
    pub(crate) what_if_send: Option<String>,
    /// The contract lookups of orders given without a conId: request
    /// number and the order's slot (ibx#486).
    pub(crate) order_lookups: Vec<(u32, InstrumentId)>,
    /// The API precaution "Bypass Redirect Order warning for Stock API
    /// Orders" (`IBX_BYPASS_REDIRECT_ORDER_WARNING`, off by default as the
    /// reference's, ibx#486).
    /// The precaution bypass of routeMarketableToBbo (ibx#608).
    pub(crate) bypass_route_bbo_warning: bool,
    pub(crate) bypass_redirect_warning: bool,
    /// Sequence of those lookups.
    pub(crate) next_order_lookup: u32,
    /// The records of contracts the session has, by conId and the exchange
    /// the answer named (ibx#706): an order needs the one of its exchange
    /// (the primary exchange for SMART) and asks it when missing.
    pub(crate) order_records: std::collections::HashSet<(i64, String)>,
    /// Contracts whose records are taken as known on every exchange (a
    /// replay that starts after the reference had them).
    pub(crate) order_records_any: std::collections::HashSet<i64>,
    /// The record lookups of orders on their way: internal name, conId,
    /// the exchange asked.
    pub(crate) order_record_lookups: Vec<(String, i64, String)>,
    /// Orders whose record lookup was answered: they go on.
    pub(crate) order_records_cleared: std::collections::HashSet<crate::types::OrderId>,
    /// The orders held with transmit off whose placeOrder's lookups have
    /// not all answered (ibx#718), with whether the record lookup was sent
    /// for them.
    pub(crate) held_lookups: Vec<(crate::types::OrderId, bool)>,
    /// The contracts the order lookups of the session named, by what the
    /// order gave of them (ibx#718): a later held order of one of them is
    /// known at once.
    pub(crate) order_contracts_named: Vec<(OrderContractKey, i64)>,
    /// The clearing rule lookups to send, and those on their way, and how
    /// many the session made (ibx#709).
    pub(crate) order_clearing_to_send: Vec<(i64, String)>,
    pub(crate) order_clearing_lookups: Vec<String>,
    pub(crate) next_clearing_rule: u32,

    /// Set while a short-side order is encoded: its short-sale fields
    /// (ibx#417).
    pub(crate) short_sale_send: Option<crate::types::ShortSale>,
    /// The session's combos and combo orders (ibx#470).
    pub(crate) combos: crate::engine::combo::ComboBook,
    /// Set while a combo order is encoded: what it writes besides the
    /// order's own fields (ibx#470).
    pub(crate) combo_send: Option<crate::engine::combo::ComboSend>,
    /// The scale fields of the order being sent (ibx#751).
    pub(crate) scale_send: Option<Box<crate::types::Scale>>,
    /// The API client id the new orders carry (ibx#466).
    pub(crate) api_client_id: i64,
    /// Requests with outside-RTH waiting for their lookup, in order; later
    /// requests of the same order wait behind them.
    pub(crate) rth_parked: Vec<OrderRequest>,
    /// The orders of each group transmitted together that still waits for
    /// a contract (ibx#547): order id and instrument. No order of a group
    /// goes out before the contract of each is known.
    pub(crate) order_groups: Vec<Vec<(OrderId, InstrumentId)>>,
    pub(crate) next_rth_lookup: u32,
    /// The session counts US stock sizes in round lots (ibx#287).
    pub(crate) scale_us_lots: bool,
    /// The bracket key of each order that has one (ibx#248).
    pub(crate) bracket_keys: HashMap<OrderId, crate::engine::bracket::BracketKey>,
    /// The next child index of each bracket parent (ibx#248).
    pub(crate) bracket_next_child: HashMap<OrderId, u32>,
    /// The last bracket group number given or seen on a report (ibx#248).
    pub(crate) bracket_groups: u32,
    /// State of the bracket colour generator (ibx#248).
    pub(crate) bracket_rng: u64,
    /// The session allows the price management flag (ibx#492).
    pub(crate) price_mgmt_feature: bool,
    /// The session's price management exclusions by exchange, None when
    /// the logon has none (ibx#492).
    pub(crate) price_mgmt_exclusions: Option<HashMap<String, Vec<String>>>,
    /// Set while an order is encoded: it carries the price management flag
    /// unless its order type is one the flag never goes with (ibx#492).
    pub(crate) price_mgmt_send: bool,
    /// MiFID on the session (ibx#500): the logon's MIFID2 feature on a
    /// session that is not a super user's; the account's MiFID config id
    /// and covered groups.
    pub(crate) mifid_manager: bool,
    pub(crate) mifid_config_id: String,
    pub(crate) mifid_groups: Vec<String>,
    /// An FA session (ibx#507): orders carry their allocation.
    pub(crate) fa_session: bool,
    /// The FA exchange with the server and the groups of the orders
    /// (ibx#507).
    pub(crate) fa: crate::engine::hot_loop::fa::FaState,
    /// The logon's soft dollar tiers (name, value) by key, the contract's
    /// market type (ibx#506).
    pub(crate) soft_dollar_tiers: std::collections::HashMap<String, Vec<(String, String)>>,
    /// The soft dollar tier tags of the request being encoded, and each
    /// order's, for its modifications (ibx#506).
    pub(crate) soft_dollar_send: Vec<(u32, String)>,
    pub(crate) soft_dollar_orders: std::collections::HashMap<OrderId, Vec<(u32, String)>>,
    /// The MiFID tags of the request being encoded (ibx#500).
    pub(crate) mifid_send: Vec<(u32, String)>,
    /// The MiFID values each order went out with and whether it is
    /// covered, for its modifications (ibx#500).
    pub(crate) mifid_orders: std::collections::HashMap<OrderId, ([String; 4], bool)>,
    /// Round lot by conId, once its definition was read (ibx#287).
    /// The terms an order's contract is looked up with, by the order's
    /// slot, for a contract other than a stock (ibx#666).
    pub(crate) order_lookup_filters: HashMap<crate::types::InstrumentId, crate::types::SecDefFilters>,
    pub(crate) round_lots: HashMap<i64, i64>,
    /// The size and price increments of each definition read, by conId,
    /// packed (`md_events::Increment`), kept from session level 222, whose
    /// last size and price are given in their steps (ibx#613).
    pub(crate) def_increments: HashMap<i64, (u64, u64)>,
    /// The conIds whose increments came since the last publication to
    /// their market data slots (ibx#613).
    pub(crate) increments_due: Vec<i64>,
    /// The conIds of US stocks and warrants among the definitions read
    /// (ibx#648).
    pub(crate) us_lots: std::collections::HashSet<i64>,
    /// Requests waiting for their contract's definition, by conId, and the
    /// ones whose definition came, to be handled again (ibx#404).
    pub(crate) def_parked: Vec<(i64, crate::types::ControlCommand)>,
    /// The regulatory snapshots whose contract's definition was asked for
    /// them, by conId: handled again once it came, without asking it again
    /// (ibx#645).
    pub(crate) snapshot_def_asked: std::collections::HashSet<i64>,
    pub(crate) def_ready: Vec<crate::types::ControlCommand>,
    /// The client's requests given by a conId alone whose definition was
    /// asked for them, by kind of request and request id: handled again
    /// once it came, they go on without asking it again (ibx#732).
    pub(crate) def_asked: std::collections::HashSet<(crate::engine::context::DefAsk, crate::types::ReqId)>,
    /// The client's requests waiting for the company lookup of their
    /// option's underlying, by the underlying's conId; handled again once
    /// it answered (ibx#733).
    pub(crate) company_parked: Vec<(i64, crate::types::ControlCommand)>,
    /// Listing exchange by conId, once its definition was read (#452).
    pub(crate) listing_exchanges: HashMap<i64, String>,
    /// The terms of the options whose definition was read, for the option
    /// model of their market data (ibx#502).
    pub(crate) option_terms: HashMap<i64, crate::control::optcalc::OptionTerms>,
    /// Option and futures option orders past their permission check, whose
    /// option's market data the engine starts for itself (ibx#693):
    /// (instrument, conId, the order's exchange).
    pub(crate) order_models: Vec<(crate::types::InstrumentId, i64, String)>,
    /// What the definitions of futures and indexes name, by conId, for the
    /// index part of a futures option order's model (ibx#727).
    pub(crate) def_links: HashMap<i64, DefLink>,
    /// Contracts whose definition was asked and answered without them: an
    /// option model waiting for their terms gives up (ibx#727).
    pub(crate) no_definition: std::collections::HashSet<i64>,
    /// API security type and symbol by conId, once its definition was
    /// read: a market data request that gives the conId alone takes them
    /// from it (ibx#643).
    pub(crate) def_kinds: HashMap<i64, (String, String)>,
    /// A stock's type (COMMON, ETF, ...) by conId, from its definition: the
    /// ETF ticks are asked for an ETF only (ibx#641).
    pub(crate) stock_types: HashMap<i64, String>,
    /// Stocks whose definition is not a US stock's, by conId (ibx#674).
    pub(crate) foreign_stocks: std::collections::HashSet<i64>,
    /// Contracts refused for want of an API subscription with delayed data
    /// available: a later request in delayed mode is asked delayed at once
    /// (ibx#674).
    pub(crate) delayed_contracts: std::collections::HashSet<i64>,
    /// The logon's algo providers (ibx#505).
    pub(crate) algo_providers: Vec<String>,
    /// The account's order presets by security type: the time in force
    /// they set, None when they set none (ibx#660). A type asked and not
    /// answered yet is in `presets_asked`.
    pub(crate) order_presets: HashMap<String, Option<String>>,
    /// The limit price of each order preset that names one (ibx#673).
    pub(crate) preset_limits: HashMap<String, crate::engine::hot_loop::order_builder::PresetLimit>,
    pub(crate) presets_asked: std::collections::HashSet<String>,
    pub(crate) next_preset_request: u32,
    /// The order preset kinds the account has stored on the server, from
    /// the list asked at the logon (ibx#729); None until it answered. A kind
    /// stored is asked at the first order of its type and waited for; a kind
    /// not stored is saved after that order's message.
    pub(crate) presets_stored: Option<std::collections::HashSet<String>>,
    /// The order preset kinds to save once the order being written is out
    /// (ibx#729).
    pub(crate) preset_saves_due: Vec<String>,
    /// The copied waits of a stock's first order (ibx#729): off by default,
    /// on with the copied waits setting (ibx#564).
    pub(crate) order_gaps: bool,
    /// A transmitted stock order waits for the records of its contract
    /// (ibx#706) before it goes: on for an engine built by hand (the
    /// replays of the reference's recordings); a session opened by a logon
    /// asks them only with the copied waits setting (ibx#564). A held order
    /// asks them always: a hedge under it waits for them (ibx#718).
    pub(crate) order_records_wait: bool,
    /// The time those waits go by when a test sets it (the replays' clock).
    pub(crate) gap_clock: Option<std::time::Instant>,
    /// When the answer of each order preset kind came, for those waits
    /// (a stock's, a forex pair's, the presets an order waits for before
    /// its refusal).
    pub(crate) preset_answered: HashMap<String, std::time::Instant>,
    /// When the symbol lookup of an order's contract answered, by conId,
    /// for the wait before its first step (ibx#729).
    pub(crate) lookup_answered: HashMap<i64, std::time::Instant>,
    /// The contracts an order gave by their conId alone, named by their
    /// definition: their first order keeps shorter waits (ibx#729).
    pub(crate) by_con_id_contracts: std::collections::HashSet<i64>,
    /// When the record an order asked of its contract came, by conId.
    pub(crate) record_answered: HashMap<i64, std::time::Instant>,
    /// The orders that waited for the stock order preset.
    pub(crate) preset_waiters: std::collections::HashSet<crate::types::OrderId>,
    /// The earliest time an order held by one of those waits goes on.
    pub(crate) order_wake: Option<std::time::Instant>,
    /// The account's trading permission by permission class, as the
    /// server answered (ibx#666); a class asked and not answered yet is in
    /// `permissions_asked`, with the time the order stops waiting.
    pub(crate) trading_permissions: HashMap<String, bool>,
    pub(crate) permissions_asked: HashMap<String, std::time::Instant>,
    /// The trading permission class each contract's definition names, by
    /// conId, once read (ibx#734).
    pub(crate) permission_keys: HashMap<i64, String>,
    /// The permission classes of derivatives asked once the company lookup
    /// of their underlying answered: (underlying's conId, class), in the
    /// order they came (ibx#734).
    pub(crate) classes_after_company: Vec<(i64, String)>,
    /// The cash amount of a fund order bought by cash, in quantity units:
    /// the reference reports it as the order's remaining quantity (ibx#666;
    /// paper, 16/10/2026: Cancelled with 100 remaining for a VFIAX buy of
    /// 100.00).
    pub(crate) cash_orders: HashMap<crate::types::OrderId, i64>,
    pub(crate) next_permission_request: u32,
    /// The CFD checks a refused volatility option order starts (ibx#729):
    /// the underlying symbols checked in the session, and the requests on
    /// their way, by the name ibx gave them (the symbol lookup, then the
    /// definition by conId on SMART).
    pub(crate) cfd_checked: std::collections::HashSet<String>,
    pub(crate) cfd_requests: Vec<String>,
    /// The underlyings the derivative orders of the session looked up
    /// (ibx#730): by conId, while the lookup is on its way the time its
    /// order stops waiting for it, None once answered; and the requests on
    /// their way, by the name ibx gave them, with the underlying's conId.
    /// The order's underlying first (its company lookup, which its order
    /// waits for), then its preferred definition past its permission check
    /// (which the order's option model waits for).
    pub(crate) order_underlyings: HashMap<i64, Option<std::time::Instant>>,
    pub(crate) under_preferred: HashMap<i64, Option<std::time::Instant>>,
    /// The stocks an option order asked as the underlying of index
    /// options (ibx#730): once per underlying.
    pub(crate) index_option_unders: std::collections::HashSet<i64>,
    pub(crate) underlying_requests: Vec<(String, i64)>,
    /// The lookups of the indexes a futures option order's model makes
    /// once its index is known (ibx#730), and the number of the model's
    /// next lookup.
    pub(crate) index_walks: Vec<crate::engine::hot_loop::farm::IndexWalk>,
    pub(crate) next_model_lookup: u32,
    /// The key of the trading hours of a contract's class, by conId, once
    /// its definition was read (ibx#730).
    pub(crate) join_keys: HashMap<i64, String>,
    /// Contracts whose trading hours were asked for an order and have not
    /// come: an order with a zone to check waits for them (ibx#668).
    pub(crate) zone_pending: std::collections::HashSet<i64>,
    /// The trading hours an order's contract answer calls for, not sent
    /// yet (ibx#728): conId, schedule key and request name. The order sends
    /// them at its turn, after its permission question when it has one.
    pub(crate) order_hours_due: Vec<(i64, String, String)>,
    /// Depth requests refused with 10092: their cancel answers nothing
    /// (ibx#678).
    pub(crate) depth_unsupported: std::collections::HashSet<ReqId>,
    /// Aggregate group by conId, once its definition was read (#445).
    pub(crate) agg_groups: HashMap<i64, i32>,
    /// SMART component exchanges by conId, once its definition was read
    /// (#452).
    pub(crate) smart_components: HashMap<i64, Vec<String>>,
    /// Valid exchanges by conId (BEST as SMART), once its
    /// definition was read: the components of its SmartDepth (#452).
    pub(crate) valid_exchanges: HashMap<i64, Vec<String>>,
    /// The contract as a depth refusal names it, by conId (#452).
    pub(crate) depth_descriptions: HashMap<i64, String>,
    /// SmartDepth requests waiting for their components' definitions
    /// (#452).
    pub(crate) depth_gathers: Vec<crate::engine::hot_loop::farm::DepthGather>,
    /// Definition lookups by conId in flight for what a request or a model
    /// needs (round lot, aggregate group, security type): (request id,
    /// conId, deadline).
    pub(crate) lot_lookups: Vec<(String, i64, std::time::Instant)>,
    /// Market data subscriptions waiting for their round lot, in order.
    pub(crate) lot_parked: Vec<crate::engine::hot_loop::farm::MdSubscribe>,
    /// Subscriptions whose round lot is known, to be sent.
    pub(crate) lot_ready: Vec<crate::engine::hot_loop::farm::MdSubscribe>,
    /// The next number of those lookups' range (ibx#731).
    pub(crate) next_lot_lookup: u32,
    /// Market data subscriptions given without a conId, waiting for the
    /// lookup that resolves it: (request number, subscription, deadline)
    /// (ibx#278).
    pub(crate) md_lookups: Vec<(u32, crate::engine::hot_loop::farm::MdSubscribe, std::time::Instant)>,
    /// The instruments of `md_lookups` whose request was cancelled while
    /// the lookup was on its way (ibx#662).
    pub(crate) md_lookup_cancelled: std::collections::HashSet<crate::types::InstrumentId>,
    /// Subscriptions whose conId was resolved, to be sent.
    pub(crate) md_resolved: Vec<crate::engine::hot_loop::farm::MdSubscribe>,
    /// Slots of those subscriptions asked with every API line taken
    /// (ibx#501).
    pub(crate) md_over_limit: Vec<InstrumentId>,
    pub(crate) next_md_lookup: u32,
    /// Final status of orders that left the engine filled, cancelled or
    /// rejected, for the reference's refusal of a later cancel (ibx#464).
    /// Bounded: the oldest are dropped past `FINISHED_ORDERS_MAX`.
    finished_orders: HashMap<OrderId, OrderStatus>,
    finished_order_ids: std::collections::VecDeque<OrderId>,
    /// Orders refused with 387 before anything was sent, with their
    /// instrument, quantity and parent (ibx#542): the reference keeps such
    /// an order, unlisted, until its cancel ends it.
    pub(crate) unsent: HashMap<OrderId, (InstrumentId, Qty, i64)>,
    /// The book entry of each order held with transmit off (ibx#595): the
    /// reference keeps it in its book under its permId from its placing,
    /// with its parent and OCA group.
    pub(crate) held_book: HashMap<OrderId, BookEntry>,
    /// The book entry of each order held (see [`BookEntry`]).
    pub(crate) book: HashMap<OrderId, BookEntry>,
    /// The ended orders the reference's book still holds (ibx#545).
    pub(crate) ended: Vec<EndedOrder>,
    /// Orders of this client the reference keeps in its API pending map
    /// with no order made: a combo refused with 200 at its contract
    /// lookup. A cancel or a global cancel ends them as ApiCancelled.
    pub(crate) api_pending: HashMap<OrderId, OrderRequest>,
    next_book_seq: u64,
    /// Most orders held at once: the reference's book is a hash table
    /// that grows with it and never shrinks.
    pub(crate) book_peak: usize,
    /// Timestamp when the last farm socket recv returned data (for decode latency measurement).
    pub(crate) recv_at: Instant,
    /// Total hot loop iterations since start.
    pub(crate) loop_iterations: u64,
}

impl Context {
    pub fn new() -> Self {
        Self {
            market: MarketState::new(),
            positions: [0i64; MAX_INSTRUMENTS],
            open_orders: HashMap::with_capacity(128),
            pending_orders: OrderBuffer::new(),
            modify_versions: HashMap::new(),
            last_clord: HashMap::new(),
            purged_orders: Vec::new(),
            recovered_keys: HashMap::new(),
            server_ids: HashMap::new(),
            recovered: std::collections::HashSet::new(),
            order_ids: OrderIdGenerator::new(),
            cancel_clord: HashMap::new(),
            status_queries: std::collections::HashSet::new(),
            trail_limit_reported: HashMap::new(),
            scale_reset_reported: std::collections::HashSet::new(),
            scale_totals: HashMap::new(),
            price_mgmt_sent: std::collections::HashSet::new(),
            compete_cleared: std::collections::HashSet::new(),
            reported_stop: HashMap::new(),
            rth_types: HashMap::new(),
            min_sizes: HashMap::new(),
            rth_lookups: Vec::new(),
            rth_parked: Vec::new(),
            order_groups: Vec::new(),
            next_rth_lookup: 0,
            what_ifs: HashMap::new(),
            what_if_send: None,
            order_lookups: Vec::new(),
            bypass_redirect_warning: crate::config::bypass_redirect_order_warning(),
            bypass_route_bbo_warning: crate::config::bypass_route_marketable_to_bbo_warning(),
            next_order_lookup: 0,
            order_records: std::collections::HashSet::new(),
            order_records_any: std::collections::HashSet::new(),
            order_record_lookups: Vec::new(),
            order_records_cleared: std::collections::HashSet::new(),
            held_lookups: Vec::new(),
            order_contracts_named: Vec::new(),
            order_clearing_to_send: Vec::new(),
            order_clearing_lookups: Vec::new(),
            next_clearing_rule: 0,

            short_sale_send: None,
            combos: Default::default(),
            combo_send: None,
            scale_send: None,
            api_client_id: 0,
            scale_us_lots: false,
            bracket_keys: HashMap::new(),
            bracket_next_child: HashMap::new(),
            bracket_groups: 0,
            bracket_rng: crate::engine::bracket::seed(),
            price_mgmt_feature: false,
            price_mgmt_exclusions: None,
            price_mgmt_send: false,
            mifid_manager: false,
            mifid_config_id: String::new(),
            mifid_groups: Vec::new(),
            mifid_send: Vec::new(),
            fa_session: false,
            fa: Default::default(),
            soft_dollar_tiers: std::collections::HashMap::new(),
            soft_dollar_send: Vec::new(),
            soft_dollar_orders: std::collections::HashMap::new(),
            mifid_orders: std::collections::HashMap::new(),
            round_lots: HashMap::new(),
            def_increments: HashMap::new(),
            increments_due: Vec::new(),
            order_lookup_filters: HashMap::new(),
            us_lots: std::collections::HashSet::new(),
            agg_groups: HashMap::new(),
            listing_exchanges: HashMap::new(),
            option_terms: HashMap::new(),
            order_models: Vec::new(),
            def_links: HashMap::new(),
            no_definition: std::collections::HashSet::new(),
            def_kinds: HashMap::new(),
            stock_types: HashMap::new(),
            foreign_stocks: std::collections::HashSet::new(),
            delayed_contracts: std::collections::HashSet::new(),
            algo_providers: Vec::new(),
            order_presets: HashMap::new(),
            preset_limits: HashMap::new(),
            presets_asked: std::collections::HashSet::new(),
            next_preset_request: 3,
            presets_stored: None,
            preset_saves_due: Vec::new(),
            order_gaps: false,
            order_records_wait: true,
            gap_clock: None,
            preset_answered: HashMap::new(),
            lookup_answered: HashMap::new(),
            by_con_id_contracts: std::collections::HashSet::new(),
            record_answered: HashMap::new(),
            preset_waiters: std::collections::HashSet::new(),
            order_wake: None,
            trading_permissions: HashMap::new(),
            permissions_asked: HashMap::new(),
            permission_keys: HashMap::new(),
            classes_after_company: Vec::new(),
            cash_orders: HashMap::new(),
            next_permission_request: 0,
            cfd_checked: std::collections::HashSet::new(),
            cfd_requests: Vec::new(),
            order_underlyings: HashMap::new(),
            under_preferred: HashMap::new(),
            index_option_unders: std::collections::HashSet::new(),
            underlying_requests: Vec::new(),
            index_walks: Vec::new(),
            next_model_lookup: 0,
            join_keys: HashMap::new(),
            zone_pending: std::collections::HashSet::new(),
            order_hours_due: Vec::new(),
            depth_unsupported: std::collections::HashSet::new(),
            def_parked: Vec::new(),
            snapshot_def_asked: std::collections::HashSet::new(),
            def_ready: Vec::new(),
            def_asked: std::collections::HashSet::new(),
            company_parked: Vec::new(),
            smart_components: HashMap::new(),
            valid_exchanges: HashMap::new(),
            depth_descriptions: HashMap::new(),
            depth_gathers: Vec::new(),
            lot_lookups: Vec::new(),
            lot_parked: Vec::new(),
            lot_ready: Vec::new(),
            next_lot_lookup: 0,
            md_lookups: Vec::new(),
            md_lookup_cancelled: std::collections::HashSet::new(),
            md_resolved: Vec::new(),
            md_over_limit: Vec::new(),
            next_md_lookup: 0,
            finished_orders: HashMap::new(),
            unsent: HashMap::new(),
            held_book: HashMap::new(),
            finished_order_ids: std::collections::VecDeque::new(),
            book: HashMap::new(),
            api_pending: HashMap::new(),
            next_book_seq: 0,
            ended: Vec::new(),
            book_peak: 0,
            account: AccountState::default(),
            clock: Clock::new(),
            next_order_id: {
                // The engine's own orders (not the API's): epoch-based
                // keys, above the API's int range, so they never take an
                // API order id. Their ClOrdIDs come from `order_ids`.
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                secs as i64 * 1000
            },
            recv_at: Instant::now(),
            loop_iterations: 0,
        }
    }

    // ── Market data (read, zero-copy) ──

    #[inline(always)]
    pub fn bid(&self, id: InstrumentId) -> Price {
        self.market.bid(id)
    }

    #[inline(always)]
    pub fn ask(&self, id: InstrumentId) -> Price {
        self.market.ask(id)
    }

    #[inline(always)]
    pub fn last(&self, id: InstrumentId) -> Price {
        self.market.last(id)
    }

    #[inline(always)]
    pub fn bid_size(&self, id: InstrumentId) -> Qty {
        self.market.bid_size(id)
    }

    #[inline(always)]
    pub fn ask_size(&self, id: InstrumentId) -> Qty {
        self.market.ask_size(id)
    }

    #[inline(always)]
    pub fn mid(&self, id: InstrumentId) -> Price {
        self.market.mid(id)
    }

    #[inline(always)]
    pub fn spread(&self, id: InstrumentId) -> Price {
        self.market.spread(id)
    }

    #[inline(always)]
    pub fn quote(&self, id: InstrumentId) -> &Quote {
        self.market.quote(id)
    }

    // ── Positions & orders (read) ──

    #[inline(always)]
    /// Fixed-point (QTY_SCALE).
    pub fn position_fixed(&self, id: InstrumentId) -> Qty {
        self.positions[id as usize]
    }

    pub fn open_orders_for(&self, id: InstrumentId) -> Vec<&Order> {
        self.open_orders
            .values()
            .filter(|o| o.instrument == id && matches!(o.status,
                OrderStatus::PendingSubmit | OrderStatus::PreSubmitted | OrderStatus::Submitted |
                OrderStatus::PendingCancel | OrderStatus::PendingReplace |
                OrderStatus::PartiallyFilled))
            .collect()
    }

    /// Set the quantity an order is held with (ibx#751: a scale order with
    /// an initial position).
    pub(crate) fn set_order_qty(&mut self, order_id: OrderId, qty_fixed: crate::types::Qty) {
        if let Some(o) = self.open_orders.get_mut(&order_id) {
            o.qty_fixed = qty_fixed;
        }
    }

    pub fn order(&self, order_id: OrderId) -> Option<&Order> {
        self.open_orders.get(&order_id)
    }

    pub fn account(&self) -> &AccountState {
        &self.account
    }

    // ── Order management (write to pre-allocated buffer) ──

    pub fn submit_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_market(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMarket {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStop {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
        });
        id
    }

    pub fn submit_stop_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopLimit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
        });
        id
    }

    pub fn submit_limit_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitGtc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            outside_rth,
        });
        id
    }

    pub fn submit_stop_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopGtc {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
            outside_rth,
        });
        id
    }

    pub fn submit_stop_limit_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopLimitGtc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
            outside_rth,
        });
        id
    }

    pub fn submit_limit_ioc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitIoc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_limit_fok(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitFok {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_trailing_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_amt: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitTrailingStop {
            order_id: id,
            instrument,
            side,
            qty,
            trail_amt,
            trail_stop_price: 0,
        });
        id
    }

    pub fn submit_trailing_stop_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        lmt_offset: Price,
        trail_amt: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitTrailingStopLimit {
            lmt_price: None,
            order_id: id,
            instrument,
            side,
            qty,
            lmt_offset,
            trail_amt,
            trail_stop_price: 0,
        });
        id
    }

    pub fn submit_trailing_stop_pct(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_percent: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitTrailingStopPct {
            order_id: id,
            instrument,
            side,
            qty,
            trail_percent,
            trail_stop_price: 0,
        });
        id
    }

    pub fn submit_moc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMoc {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_loc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLoc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_mit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMit {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
        });
        id
    }

    pub fn submit_lit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
        });
        id
    }

    /// Submit a bracket order: limit entry + take-profit limit + stop-loss stop.
    /// Returns (parent_id, take_profit_id, stop_loss_id).
    pub fn submit_bracket(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        entry_price: Price,
        take_profit: Price,
        stop_loss: Price,
    ) -> (OrderId, OrderId, OrderId) {
        let parent_id = self.next_order_id;
        let tp_id = self.next_order_id + 1;
        let sl_id = self.next_order_id + 2;
        self.next_order_id += 3;
        self.pending_orders.push(OrderRequest::SubmitBracket {
            parent_id,
            tp_id,
            sl_id,
            instrument,
            side,
            qty,
            entry_price,
            take_profit,
            stop_loss,
        });
        (parent_id, tp_id, sl_id)
    }

    /// Submit a limit order with extended attributes (display size, hidden, GAT, GTD, outside RTH).
    /// Use `tif`: b'0' = DAY, b'1' = GTC, b'6' = GTD (auto-set if good_till > 0).
    pub fn submit_limit_ex(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        tif: u8,
        attrs: OrderAttrs,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitEx {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            tif,
            attrs,
        });
        id
    }

    pub fn submit_rel(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitRel {
            order_id: id,
            instrument,
            side,
            qty,
            offset,
        });
        id
    }

    pub fn submit_limit_opg(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitOpg {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_adaptive(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        priority: AdaptivePriority,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitAdaptive {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            priority,
            tif: b'0',
            attrs: OrderAttrs::default(),
        });
        id
    }

    pub fn submit_mtl(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMtl {
            order_id: id, instrument, side, qty,
        });
        id
    }

    pub fn submit_mkt_prt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMktPrt {
            order_id: id, instrument, side, qty,
        });
        id
    }

    pub fn submit_stp_prt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStpPrt {
            order_id: id, instrument, side, qty, stop_price,
        });
        id
    }

    pub fn submit_mid_price(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price_cap: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMidPrice {
            order_id: id, instrument, side, qty, price_cap,
        });
        id
    }

    /// `offset` is the snap offset (the API auxPrice), 0 when unset.
    pub fn submit_snap_mkt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapMkt {
            order_id: id, instrument, side, qty, offset,
        });
        id
    }

    /// `offset` is the snap offset (the API auxPrice), 0 when unset.
    pub fn submit_snap_mid(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapMid {
            order_id: id, instrument, side, qty, offset,
        });
        id
    }

    /// `offset` is the snap offset (the API auxPrice), 0 when unset.
    pub fn submit_snap_pri(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapPri {
            order_id: id, instrument, side, qty, offset,
        });
        id
    }

    /// `price` is the limit price, 0 = unset.
    pub fn submit_peg_mkt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegMkt {
            order_id: id, instrument, side, qty, price, offset,
        });
        id
    }

    /// `price` is the limit price, 0 = unset. The offset goes out as zero,
    /// as the reference sends it.
    pub fn submit_peg_mid(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegMid {
            order_id: id, instrument, side, qty, price, offset,
        });
        id
    }

    /// Submit an algorithmic order (VWAP, TWAP, Arrival Price, etc.).
    pub fn submit_algo(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        algo: AlgoParams,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitAlgo {
            order_id: id, instrument, side, qty, price, algo,
            tif: b'0', attrs: OrderAttrs::default(),
        });
        id
    }

    /// Submit a Pegged to Benchmark order.
    /// Pegs to a benchmark instrument's price with change amounts. `price`
    /// is the starting price and `stock_ref_price` the stock reference
    /// price (0 = unset for both); `ref_exchange` the reference contract's
    /// exchange (empty = not sent).
    #[allow(clippy::too_many_arguments)]
    pub fn submit_peg_bench(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        ref_con_id: i64,
        is_peg_decrease: bool,
        pegged_change_amount: Price,
        ref_change_amount: Price,
        stock_ref_price: Price,
        ref_exchange: &str,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegBench {
            order_id: id, instrument, side, qty, price,
            ref_con_id, is_peg_decrease, pegged_change_amount, ref_change_amount,
            stock_ref_price, ref_exchange: ref_exchange.to_string(),
        });
        id
    }

    /// Submit a limit order for exchange auction (TIF=AUC).
    pub fn submit_limit_auc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitAuc {
            order_id: id, instrument, side, qty, price,
        });
        id
    }

    /// Submit a Market-to-Limit order for exchange auction (TIF=AUC).
    pub fn submit_mtl_auc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMtlAuc {
            order_id: id, instrument, side, qty,
        });
        id
    }

    /// Submit a Box Top order (wire-identical to MTL, OrdType K). BOX exchange only.
    pub fn submit_box_top(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    ) -> OrderId {
        self.submit_mtl(instrument, side, qty)
    }

    /// Submit a what-if preview of a limit order for margin/commission. The
    /// order is NOT placed. Response delivered via `Event::WhatIf`.
    pub fn submit_what_if(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitWhatIf {
            request: Box::new(OrderRequest::SubmitLimit { order_id: id, instrument, side, qty, price }),
        });
        id
    }

    /// Submit a fractional shares limit order. Qty is fixed-point (QTY_SCALE = 10^4).
    /// E.g., 0.5 shares = 5000, 1.25 shares = 12500.
    pub fn submit_limit_fractional(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitFractional {
            order_id: id, instrument, side, qty, price,
        });
        id
    }

    /// Submit an adjustable stop order. Adjusts to a different order type when trigger is hit.
    pub fn submit_adjustable_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        trigger_price: Price,
        adjusted_order_type: AdjustedOrderType,
        adjusted_stop_price: Price,
        adjusted_stop_limit_price: Price,
        adjusted_trailing_amount: Price,
        adjustable_trailing_unit: i32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitAdjustableStop {
            order_id: id, instrument, side, qty, stop_price, trigger_price,
            adjusted_order_type, adjusted_stop_price, adjusted_stop_limit_price,
            adjusted_trailing_amount, adjustable_trailing_unit,
        });
        id
    }

    pub fn cancel(&mut self, order_id: OrderId) {
        self.pending_orders.push(OrderRequest::Cancel { order_id });
    }

    pub fn cancel_all(&mut self, instrument: InstrumentId) {
        self.pending_orders
            .push(OrderRequest::CancelAll { instrument });
    }

    /// Replace a working order with the full wanted state: quantity, order
    /// kind with its prices, time-in-force and attributes (ibx#247).
    pub fn modify(
        &mut self,
        order_id: OrderId,
        qty: u32,
        kind: OrderKind,
        tif: u8,
        attrs: OrderAttrs,
    ) -> OrderId {
        let new_id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::Modify {
            new_order_id: new_id,
            order_id,
            qty,
            kind,
            tif,
            attrs,
        });
        new_id
    }

    // ── Timing ──

    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    pub fn now_utc(&self) -> i64 {
        self.clock.now_utc()
    }

    /// The time the copied waits of the orders go by (ibx#729): the test's
    /// clock when one is set, else now.
    pub(crate) fn gap_now(&self) -> std::time::Instant {
        self.gap_clock.unwrap_or_else(std::time::Instant::now)
    }

    /// Timestamp when the last farm socket recv returned data.
    #[inline(always)]
    pub fn recv_timestamp(&self) -> Instant {
        self.recv_at
    }

    /// Total hot loop iterations since start.
    #[inline(always)]
    pub fn loop_iterations(&self) -> u64 {
        self.loop_iterations
    }

    // ── Instrument management ──

    pub fn register_instrument(&mut self, con_id: i64) -> InstrumentId {
        self.market.register(con_id)
    }

    pub fn set_symbol(&mut self, id: InstrumentId, symbol: String) {
        self.market.set_symbol(id, symbol);
    }

    /// The security type and exchange of an instrument, which its orders
    /// are routed by.
    pub fn set_routing(&mut self, id: InstrumentId, sec_type: &str, exchange: &str) {
        self.market.set_routing(id, sec_type, exchange);
    }

    /// The currency of an instrument, written on its orders.
    pub fn set_currency(&mut self, id: InstrumentId, currency: &str) {
        self.market.set_currency(id, currency);
    }

    /// The terms of an option instrument, written on its orders.
    pub fn set_option_terms(&mut self, id: InstrumentId, terms: crate::engine::market_state::OptionTerms) {
        self.market.set_option_terms(id, terms);
    }

    pub fn set_quote(&mut self, id: InstrumentId, quote: Quote) {
        *self.market.quote_mut(id) = quote;
    }

    pub fn quote_mut(&mut self, id: InstrumentId) -> &mut Quote {
        self.market.quote_mut(id)
    }

    // ── Engine-internal methods ──

    pub fn drain_pending_orders(&mut self) -> std::vec::Drain<'_, OrderRequest> {
        self.pending_orders.drain()
    }

    /// `delta` is fixed-point (QTY_SCALE).
    pub fn update_position_fixed(&mut self, instrument: InstrumentId, delta: Qty) {
        self.positions[instrument as usize] += delta;
    }

    pub fn insert_order(&mut self, order: Order) {
        let oid = order.order_id;
        self.open_orders.insert(oid, order);
        // Initialize modify version to 0 for new orders (don't reset on modify).
        self.modify_versions.entry(oid).or_insert(0);
        if !self.book.contains_key(&oid) {
            self.book.insert(oid, BookEntry { seq: self.next_book_seq, ..Default::default() });
            self.next_book_seq += 1;
        }
        // The ended orders the reference still holds are in its book too.
        if !self.ended.is_empty() {
            let now = Instant::now();
            self.ended.retain(|e| e.until.is_none_or(|until| now < until));
        }
        self.book_peak = self.book_peak.max(self.open_orders.len() + self.ended.len());
    }

    /// The server's id of an order, the id part of its ClOrdID: the id
    /// the server holds it under when the order is kept under another key
    /// (an order of this session, or of another session kept under its API
    /// order id, ibx#466), else the order id itself.
    pub(crate) fn server_id(&self, order_id: OrderId) -> OrderId {
        self.server_ids.get(&order_id).copied().unwrap_or(order_id)
    }

    /// The key an order is held under for the server's id of its ClOrdID.
    pub(crate) fn key_of(&self, server_id: OrderId) -> OrderId {
        self.recovered_keys.get(&server_id).copied().unwrap_or(server_id)
    }

    /// Hold the order of server id `server` under `key`.
    pub(crate) fn bind_server_id(&mut self, key: OrderId, server: OrderId) {
        if key == server { return; }
        self.recovered_keys.insert(server, key);
        self.server_ids.insert(key, server);
    }

    /// Hold an order of an earlier session, of server id `server`, under its
    /// API order id `key` (ibx#466), and remember that it is not an order
    /// of this session (ibx#538).
    pub(crate) fn hold_recovered(&mut self, key: OrderId, server: OrderId) {
        self.bind_server_id(key, server);
        if key != server {
            self.recovered.insert(key);
        }
    }

    /// A working order of an earlier session held under `key`: whether it
    /// is this client's (same client id). None for any other key.
    pub(crate) fn earlier_session_order(&self, key: OrderId) -> Option<bool> {
        (self.recovered.contains(&key) && self.open_orders.contains_key(&key)).then(|| self.owned(key))
    }

    /// Put the order of an earlier session held under `key` back under its
    /// server id, so the key is free for an order of this session
    /// (ibx#538: the reference keeps the orders of each client id apart).
    /// The server id it is now held under.
    pub(crate) fn release_recovered_key(&mut self, key: OrderId) -> Option<OrderId> {
        self.recovered.remove(&key);
        let server = self.server_ids.remove(&key)?;
        self.recovered_keys.remove(&server);
        if let Some(mut order) = self.open_orders.remove(&key) {
            order.order_id = server;
            self.open_orders.insert(server, order);
        }
        if let Some(v) = self.book.remove(&key) { self.book.insert(server, v); }
        if let Some(v) = self.modify_versions.remove(&key) { self.modify_versions.insert(server, v); }
        if let Some(v) = self.last_clord.remove(&key) { self.last_clord.insert(server, v); }
        if let Some(v) = self.cancel_clord.remove(&key) { self.cancel_clord.insert(server, v); }
        if let Some(v) = self.trail_limit_reported.remove(&key) { self.trail_limit_reported.insert(server, v); }
        if let Some(v) = self.reported_stop.remove(&key) { self.reported_stop.insert(server, v); }
        Some(server)
    }

    /// A new order goes out under a server id of its own, from the
    /// reference's order id generator; its API order id
    /// stays its key. An order already held, or already given a server id,
    /// keeps its id.
    /// Keep an order refused before it was sent, for its cancel, with the
    /// id it is given now, as the reference numbers an order when it is
    /// placed: its permId (ibx#748; the reference 10.51, paper, 18/10/2026:
    /// a kept hedge's permId right after its parent's).
    pub(crate) fn keep_unsent(&mut self, order_id: OrderId, entry: (crate::types::InstrumentId, crate::types::Qty, i64)) {
        self.assign_server_id(order_id);
        self.unsent.insert(order_id, entry);
    }

    pub(crate) fn assign_server_id(&mut self, key: OrderId) {
        if self.open_orders.contains_key(&key) || self.server_ids.contains_key(&key) {
            return;
        }
        let server = self.new_server_id();
        self.bind_server_id(key, server);
    }

    /// The next id of the generator that names no order held: the
    /// reference skips the ids it gave.
    pub(crate) fn new_server_id(&mut self) -> OrderId {
        loop {
            let id = OrderId::from(self.order_ids.next());
            if !self.recovered_keys.contains_key(&id) && !self.open_orders.contains_key(&id) {
                return id;
            }
        }
    }

    /// The book entry of an order held with transmit off (ibx#595): it
    /// takes its place in the book when first held; held again, it keeps
    /// that place with its new links.
    pub(crate) fn hold_entry(&mut self, order_id: OrderId, parent: OrderId, oca_group: &str) {
        let seq = match self.held_book.get(&order_id) {
            Some(e) => e.seq,
            None => {
                self.next_book_seq += 1;
                self.next_book_seq - 1
            }
        };
        self.held_book.insert(order_id, BookEntry { seq, parent, oca_group: oca_group.to_string(), owner: None });
        self.book_peak = self.book_peak.max(self.open_orders.len() + self.ended.len() + self.unsent.len());
    }

    /// An order held with transmit off ended (ibx#595): the book keeps it
    /// for the session in the place it had.
    pub(crate) fn keep_held_ended(&mut self, order_id: OrderId, server_id: OrderId, entry: BookEntry) {
        self.ended.retain(|e| e.order_id != order_id);
        self.ended.push(EndedOrder { order_id, server_id, entry, until: None });
    }

    /// The parent and OCA group of a held order, when known.
    pub(crate) fn set_links(&mut self, order_id: OrderId, parent: Option<OrderId>, oca_group: Option<&str>) {
        if let Some(entry) = self.book.get_mut(&order_id) {
            if let Some(parent) = parent { entry.parent = parent; }
            if let Some(group) = oca_group { entry.oca_group = group.to_string(); }
        }
    }

    /// Whether this client gets the reports of an order (openOrder,
    /// orderStatus, executions, commissions): those of its own orders, and
    /// as client 0 those of a client that is not connected
    /// (the order's client, else client 0; no master client, its master
    /// client id -1 by default). ibx is the only client of its engine.
    pub(crate) fn delivered(&self, order_id: OrderId) -> bool {
        self.api_client_id == 0 || self.owned(order_id)
    }

    /// Whether this client may act on an order by its id: an order of
    /// this client, or one the server reported for this client's id. The
    /// reference finds an API order by (client id, order id)
    ///.
    pub(crate) fn owned(&self, order_id: OrderId) -> bool {
        self.book.get(&order_id).and_then(|e| e.owner).is_none_or(|owner| owner == self.api_client_id)
    }

    /// Apply a server-reported status. Returns true when the stored status
    /// actually changed. Guarded (ibx#212): a stale or reordered frame must
    /// not regress the lifecycle — terminal states are absorbing, and a
    /// lower-rank status never overwrites a higher one. Deliberate
    /// regressions go through `set_order_status_forced`.
    pub fn update_order_status(&mut self, order_id: OrderId, status: OrderStatus) -> bool {
        self.apply_order_status(order_id, status) == StatusChange::Changed
    }

    /// `update_order_status`, telling a report that restates the current
    /// status apart from a stale one: the reference reports the first and
    /// not the second (ibx#473).
    pub fn apply_order_status(&mut self, order_id: OrderId, status: OrderStatus) -> StatusChange {
        let Some(order) = self.open_orders.get_mut(&order_id) else {
            return StatusChange::Unknown;
        };
        let prev = order.status;
        if prev == status {
            return StatusChange::Same;
        }
        if prev.is_terminal() || status.rank() < prev.rank() {
            log::debug!(
                "Order {} status guard: keeping {:?}, dropping stale {:?} (ibx#212)",
                order_id, prev, status,
            );
            return StatusChange::Stale;
        }
        order.status = status;
        StatusChange::Changed
    }

    /// The status the server gives in answer to a status request, after
    /// it refused a cancel or modify (ibx#252): the order kept its status
    /// until then, and the answer sets it, back to working too. A finished
    /// status stays.
    pub fn apply_queried_status(&mut self, order_id: OrderId, status: OrderStatus) -> StatusChange {
        let Some(order) = self.open_orders.get_mut(&order_id) else {
            return StatusChange::Unknown;
        };
        if order.status == status {
            return StatusChange::Same;
        }
        if order.status.is_terminal() {
            return StatusChange::Stale;
        }
        order.status = status;
        StatusChange::Changed
    }

    /// Set a status unconditionally — for deliberate lifecycle regressions
    /// only (disconnect reconciliation).
    pub fn set_order_status_forced(&mut self, order_id: OrderId, status: OrderStatus) {
        if let Some(order) = self.open_orders.get_mut(&order_id) {
            order.status = status;
        }
    }

    /// `last_qty` is fixed-point (QTY_SCALE).
    pub fn update_order_filled_fixed(&mut self, order_id: OrderId, last_qty: Qty) {
        if let Some(order) = self.open_orders.get_mut(&order_id) {
            order.filled_fixed += last_qty;
        }
    }

    /// The session's bracket keys and counters (ibx#248).
    pub(crate) fn brackets(&mut self) -> crate::engine::bracket::Brackets<'_> {
        crate::engine::bracket::Brackets {
            keys: &mut self.bracket_keys,
            next_child: &mut self.bracket_next_child,
            groups: &mut self.bracket_groups,
            rng: &mut self.bracket_rng,
        }
    }

    pub fn remove_order(&mut self, order_id: OrderId) {
        self.open_orders.remove(&order_id);
        self.book.remove(&order_id);
    }

    /// Remove an order that ended with `status`, and keep that status for a
    /// later cancel of the same id (ibx#464).
    pub fn finish_order(&mut self, order_id: OrderId, status: OrderStatus) {
        let keep = match status {
            OrderStatus::Cancelled | OrderStatus::Rejected => Some(CANCELLED_ORDER_KEPT),
            OrderStatus::Filled => Some(FILLED_ORDER_KEPT),
            _ => None,
        };
        let kept = keep.and_then(|d| self.book.get(&order_id).cloned().map(|e| (e, d)));
        if !self.forget_order(order_id) {
            return;
        }
        if let Some((entry, keep)) = kept {
            self.keep_ended(order_id, self.server_id(order_id), entry, Some(Instant::now() + keep));
        }
        if self.finished_orders.insert(order_id, status).is_none() {
            self.finished_order_ids.push_back(order_id);
            self.trim_finished();
        }
    }

    /// Keep `status` as the end of an order that was never held (refused
    /// before it was sent, ibx#542), for a later cancel of the same id.
    pub(crate) fn finish_unsent(&mut self, order_id: OrderId, status: OrderStatus) {
        if self.finished_orders.insert(order_id, status).is_none() {
            self.finished_order_ids.push_back(order_id);
            self.trim_finished();
        }
    }

    /// The oldest ended orders past the last `FINISHED_ORDERS_MAX` are gone
    /// for good, with everything kept for them (ibx#700). An ended order's
    /// last ClOrdID stays until then: a child's report can name a parent
    /// that has ended (`parent_order_id`).
    fn trim_finished(&mut self) {
        while self.finished_order_ids.len() > FINISHED_ORDERS_MAX {
            let Some(old) = self.finished_order_ids.pop_front() else { break };
            self.finished_orders.remove(&old);
            if let Some(server) = self.server_ids.remove(&old) {
                self.recovered_keys.remove(&server);
            }
            self.purge_order_records(old);
        }
    }

    /// Drop what is kept for an order no report names again: its modify
    /// version and last ClOrdID here, and (through `purged_orders`) what
    /// the order connection keeps for it (ibx#700).
    fn purge_order_records(&mut self, order_id: OrderId) {
        self.modify_versions.remove(&order_id);
        self.last_clord.remove(&order_id);
        self.purged_orders.push(order_id);
    }

    /// An order that failed to send: removed with everything kept for it;
    /// it never reached the server, so no report names it (ibx#116,
    /// ibx#700).
    pub(crate) fn drop_unsent_order(&mut self, order_id: OrderId) {
        self.forget_order(order_id);
        self.purge_order_records(order_id);
    }

    /// Keep an ended order as the reference's book does (ibx#545), in the
    /// place a new entry of the book takes.
    pub(crate) fn keep_ended(&mut self, order_id: OrderId, server_id: OrderId, mut entry: BookEntry, until: Option<Instant>) {
        self.ended.retain(|e| e.order_id != order_id);
        if until.is_none() {
            entry.seq = self.next_book_seq;
            self.next_book_seq += 1;
        }
        self.ended.push(EndedOrder { order_id, server_id, entry, until });
        self.book_peak = self.book_peak.max(self.open_orders.len() + self.ended.len());
    }

    /// The ended orders the reference's book holds at `now`, for a global
    /// cancel; those whose time is over are dropped.
    pub(crate) fn ended_orders(&mut self, now: Instant) -> Vec<EndedOrder> {
        self.ended.retain(|e| e.until.is_none_or(|until| now < until));
        self.ended.clone()
    }

    /// True while the reference's book still holds this ended order
    /// (ibx#551): its cancel is then answered with the order's state, and
    /// finds no order afterwards.
    pub(crate) fn ended_is_kept(&self, order_id: OrderId, now: Instant) -> bool {
        self.ended.iter().any(|e| e.order_id == order_id && e.until.is_none_or(|until| now < until))
    }

    /// Remove an order and what is kept for it, without keeping a final
    /// status: a later cancel of its id finds no order (10147). False when
    /// the order was not there.
    pub fn forget_order(&mut self, order_id: OrderId) -> bool {
        if self.open_orders.remove(&order_id).is_none() {
            return false;
        }
        self.cancel_clord.remove(&order_id);
        self.status_queries.remove(&order_id);
        self.trail_limit_reported.remove(&order_id);
        self.reported_stop.remove(&order_id);
        self.bracket_keys.remove(&order_id);
        self.bracket_next_child.remove(&order_id);
        self.book.remove(&order_id);
        true
    }

    /// Final status of an order that left the engine (ibx#464).
    pub fn finished_status(&self, order_id: OrderId) -> Option<OrderStatus> {
        self.finished_orders.get(&order_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Order submission & drain ---

    #[test]
    fn submit_limit_returns_incrementing_ids() {
        let mut ctx = Context::new();
        let id1 = ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let id2 = ctx.submit_limit(0, Side::Sell, 50, 151 * PRICE_SCALE);
        assert_eq!(id2, id1 + 1, "IDs should be sequential");
    }

    #[test]
    fn submit_limit_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit {
                instrument,
                side,
                qty,
                price,
                ..
            } => {
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Buy);
                assert_eq!(qty, 100);
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }

    #[test]
    fn submit_market_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_market(1, Side::Sell, 200);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitMarket {
                instrument,
                side,
                qty,
                ..
            } => {
                assert_eq!(instrument, 1);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 200);
            }
            _ => panic!("expected SubmitMarket"),
        }
    }

    #[test]
    fn cancel_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel(42);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::Cancel { order_id } => assert_eq!(order_id, 42),
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn cancel_all_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel_all(5);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::CancelAll { instrument } => assert_eq!(instrument, 5),
            _ => panic!("expected CancelAll"),
        }
    }

    #[test]
    fn modify_drains_correctly() {
        let mut ctx = Context::new();
        ctx.modify(7, 50, OrderKind::Limit { price: 200 * PRICE_SCALE }, b'1', OrderAttrs::default());

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match &orders[0] {
            OrderRequest::Modify {
                order_id,
                kind,
                qty,
                tif,
                ..
            } => {
                assert_eq!(*order_id, 7);
                assert!(matches!(kind, OrderKind::Limit { price } if *price == 200 * PRICE_SCALE));
                assert_eq!(*qty, 50);
                assert_eq!(*tif, b'1');
            }
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn drain_clears_buffer() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let _: Vec<_> = ctx.drain_pending_orders().collect();
        // Second drain should be empty
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert!(orders.is_empty());
    }

    #[test]
    fn multiple_orders_per_tick() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        ctx.submit_limit(0, Side::Sell, 50, 152 * PRICE_SCALE);
        ctx.cancel(99);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 3);
    }

    // --- Position tracking ---

    #[test]
    fn position_starts_at_zero() {
        let ctx = Context::new();
        assert_eq!(ctx.position_fixed(0) / crate::types::QTY_SCALE, 0);
        assert_eq!(ctx.position_fixed(255) / crate::types::QTY_SCALE, 0);
    }

    #[test]
    fn update_position_accumulates() {
        let mut ctx = Context::new();
        ctx.update_position_fixed(0, (100) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.position_fixed(0) / crate::types::QTY_SCALE, 100);
        ctx.update_position_fixed(0, (-30) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.position_fixed(0) / crate::types::QTY_SCALE, 70);
        ctx.update_position_fixed(0, (-70) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.position_fixed(0) / crate::types::QTY_SCALE, 0);
    }

    #[test]
    fn positions_per_instrument() {
        let mut ctx = Context::new();
        ctx.update_position_fixed(0, (100) as i64 * crate::types::QTY_SCALE);
        ctx.update_position_fixed(1, (-50) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.position_fixed(0) / crate::types::QTY_SCALE, 100);
        assert_eq!(ctx.position_fixed(1) / crate::types::QTY_SCALE, -50);
    }

    // --- Open orders ---

    #[test]
    fn insert_and_query_order() {
        let mut ctx = Context::new();
        let order = Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        };
        ctx.insert_order(order);
        assert!(ctx.order(1).is_some());
        assert_eq!(ctx.order(1).unwrap().qty_fixed / crate::types::QTY_SCALE, 100);
    }

    #[test]
    fn open_orders_for_instrument() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2,
            instrument: 1,
            side: Side::Sell,
            price: 400 * PRICE_SCALE,
            qty_fixed: (50) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });

        let inst0_orders = ctx.open_orders_for(0);
        assert_eq!(inst0_orders.len(), 1);
        assert_eq!(inst0_orders[0].order_id, 1);
    }

    #[test]
    fn update_order_status() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.update_order_status(1, OrderStatus::Cancelled);
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Cancelled);

        // Cancelled orders not in open_orders_for (filters by Submitted)
        assert!(ctx.open_orders_for(0).is_empty());
    }

    // ── ibx#212: monotonic status guard ──

    fn submitted_order(ctx: &mut Context, oid: OrderId) {
        ctx.insert_order(Order {
            order_id: oid, instrument: 0, side: Side::Buy, price: 100,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE, status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
    }

    #[test]
    fn stale_presubmitted_does_not_regress_submitted() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        assert!(!ctx.update_order_status(1, OrderStatus::PreSubmitted),
            "regression must be rejected");
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Submitted);
    }

    #[test]
    fn terminal_states_are_absorbing() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        assert!(ctx.update_order_status(1, OrderStatus::Filled));
        // A late mass-status snapshot must not resurrect the order.
        for stale in [OrderStatus::Submitted, OrderStatus::Cancelled, OrderStatus::PendingCancel] {
            assert!(!ctx.update_order_status(1, stale), "{:?} must not overwrite Filled", stale);
        }
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Filled);
    }

    #[test]
    fn cancel_and_fill_progressions_still_flow() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        // Cancel of a partially filled order, and a fill landing while the
        // cancel is pending, are both legitimate.
        assert!(ctx.update_order_status(1, OrderStatus::PartiallyFilled));
        assert!(ctx.update_order_status(1, OrderStatus::PendingCancel));
        assert!(ctx.update_order_status(1, OrderStatus::Filled));
    }

    #[test]
    fn modify_ack_returns_to_submitted() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        assert!(ctx.update_order_status(1, OrderStatus::PendingReplace));
        assert!(ctx.update_order_status(1, OrderStatus::Submitted),
            "modify ack returns the order to working");
    }

    #[test]
    fn forced_setter_bypasses_guard() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        assert!(ctx.update_order_status(1, OrderStatus::PendingCancel));
        // The ibx#212 guard blocks the ordinary path...
        assert!(!ctx.update_order_status(1, OrderStatus::Submitted));
        // ...but a cancel reject restores the working status deliberately.
        ctx.set_order_status_forced(1, OrderStatus::Submitted);
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Submitted);
    }

    #[test]
    fn unchanged_status_reports_no_change() {
        let mut ctx = Context::new();
        submitted_order(&mut ctx, 1);
        assert!(!ctx.update_order_status(1, OrderStatus::Submitted));
        assert!(!ctx.update_order_status(999, OrderStatus::Cancelled), "unknown order");
    }

    #[test]
    fn remove_order() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.remove_order(1);
        assert!(ctx.order(1).is_none());
    }

    // --- Market data through context ---

    #[test]
    fn context_market_data_accessors() {
        let mut ctx = Context::new();
        let id = ctx.market.register(265598);
        let q = ctx.market.quote_mut(id);
        q.bid = 15000 * (PRICE_SCALE / 100);
        q.ask = 15010 * (PRICE_SCALE / 100);

        assert_eq!(ctx.bid(id), 15000 * (PRICE_SCALE / 100));
        assert_eq!(ctx.ask(id), 15010 * (PRICE_SCALE / 100));
        assert_eq!(ctx.spread(id), 10 * (PRICE_SCALE / 100));
        assert_eq!(ctx.mid(id), 15005 * (PRICE_SCALE / 100));
    }

    // --- Clock ---

    #[test]
    fn clock_monotonic() {
        let ctx = Context::new();
        let t1 = ctx.now_ns();
        let t2 = ctx.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn clock_utc_reasonable() {
        let ctx = Context::new();
        let ts = ctx.now_utc();
        // Should be after 2025-01-01 (1735689600)
        assert!(ts > 1_735_689_600);
    }

    // --- submit_limit uses current bid ---

    #[test]
    fn submit_limit_uses_current_bid() {
        let mut ctx = Context::new();
        ctx.market.register(265598);
        ctx.market.quote_mut(0).bid = 150 * PRICE_SCALE;

        ctx.submit_limit(0, Side::Buy, 100, ctx.bid(0));

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit { price, .. } => {
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }

    // --- register_instrument ---

    #[test]
    fn register_instrument_returns_id() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        assert_eq!(id, 0);
        let id2 = ctx.register_instrument(272093);
        assert_eq!(id2, 1);
    }

    #[test]
    fn register_instrument_idempotent() {
        let mut ctx = Context::new();
        let id1 = ctx.register_instrument(265598);
        let id2 = ctx.register_instrument(265598);
        assert_eq!(id1, id2);
    }

    // --- set_quote ---

    #[test]
    fn set_quote_replaces_entire_quote() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        let q = Quote {
            bid: 150 * PRICE_SCALE,
            ask: 151 * PRICE_SCALE,
            last: 150_50 * (PRICE_SCALE / 100),
            bid_size: 500,
            ask_size: 300,
            ..Quote::default()
        };
        ctx.set_quote(id, q);
        assert_eq!(ctx.bid(id), 150 * PRICE_SCALE);
        assert_eq!(ctx.ask(id), 151 * PRICE_SCALE);
        assert_eq!(ctx.bid_size(id), 500);
        assert_eq!(ctx.ask_size(id), 300);
    }

    // --- quote_mut ---

    #[test]
    fn quote_mut_modifies_in_place() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        ctx.quote_mut(id).bid = 42 * PRICE_SCALE;
        assert_eq!(ctx.bid(id), 42 * PRICE_SCALE);
    }

    // --- bid_size, ask_size ---

    #[test]
    fn bid_size_ask_size_delegates() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        ctx.quote_mut(id).bid_size = 123;
        ctx.quote_mut(id).ask_size = 456;
        assert_eq!(ctx.bid_size(id), 123);
        assert_eq!(ctx.ask_size(id), 456);
    }

    // --- account ---

    #[test]
    fn account_default_zeros() {
        let ctx = Context::new();
        let a = ctx.account();
        assert_eq!(a.net_liquidation, 0);
        assert_eq!(a.buying_power, 0);
    }

    #[test]
    fn account_writable() {
        let mut ctx = Context::new();
        ctx.account.net_liquidation = 100_000 * PRICE_SCALE;
        assert_eq!(ctx.account().net_liquidation, 100_000 * PRICE_SCALE);
    }

    // --- Timing ---

    #[test]
    fn now_ns_monotonic() {
        let ctx = Context::new();
        let t1 = ctx.now_ns();
        let t2 = ctx.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn now_utc_positive() {
        let ctx = Context::new();
        let ts = ctx.now_utc();
        // Should be after 2024-01-01 in seconds since epoch
        assert!(ts > 1704067200);
    }

    // --- Multiple orders per instrument ---

    #[test]
    fn multiple_orders_same_instrument() {
        let mut ctx = Context::new();
        ctx.register_instrument(265598);

        ctx.insert_order(Order {
            order_id: 1, instrument: 0, side: Side::Buy,
            price: 150 * PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2, instrument: 0, side: Side::Sell,
            price: 155 * PRICE_SCALE, qty_fixed: (50) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Submitted,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 3, instrument: 0, side: Side::Buy,
            price: 149 * PRICE_SCALE, qty_fixed: (200) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Filled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });

        // open_orders_for only returns Submitted
        let open = ctx.open_orders_for(0);
        assert_eq!(open.len(), 2);
    }

    // --- Update order status edge case ---

    #[test]
    fn update_order_status_nonexistent_no_panic() {
        let mut ctx = Context::new();
        // Should not panic when order doesn't exist
        ctx.update_order_status(999, OrderStatus::Cancelled);
    }

    // ibx#466: the reference's order id generator: a positive first id,
    // then one up per id; at an id the reference does not take (0, the int
    // bounds) it starts again from a new first id.
    #[test]
    fn order_ids_count_up_from_a_positive_first_id() {
        let mut ids = OrderIdGenerator::new();
        let first = ids.next();
        assert!(first > 0, "{first}");
        assert_eq!(ids.next(), first.wrapping_add(1));
        for bound in [i32::MAX, i32::MIN, 0] {
            ids.start_at(bound);
            let again = ids.next();
            assert!(again > 0 && again != i32::MAX, "{bound}: {again}");
        }
    }

    // ibx#538: a working order of an earlier session held under its API
    // order id. Another client's gives the id back to a new order of this
    // session and goes on under its server id; this client's own is the
    // order with that id.
    #[test]
    fn an_order_of_an_earlier_session_gives_its_key_back_to_a_new_order_of_another_client() {
        let mut ctx = Context::new();
        ctx.api_client_id = 93;
        ctx.order_ids.start_at(1_000_000_001);
        ctx.hold_recovered(21, 1_459_034_036);
        ctx.insert_order(Order::new(21, 0, Side::Buy, 1, 100, b'2', b'1', 0));
        ctx.book.get_mut(&21).unwrap().owner = Some(0);
        ctx.modify_versions.insert(21, 2);
        assert_eq!(ctx.earlier_session_order(21), Some(false), "an order of client 0");
        assert_eq!(ctx.earlier_session_order(22), None);

        assert_eq!(ctx.release_recovered_key(21), Some(1_459_034_036));
        assert!(ctx.order(21).is_none(), "the key is free");
        assert_eq!(ctx.order(1_459_034_036).map(|o| o.order_id), Some(1_459_034_036));
        assert_eq!((ctx.key_of(1_459_034_036), ctx.server_id(1_459_034_036)), (1_459_034_036, 1_459_034_036));
        assert_eq!(ctx.modify_versions.get(&1_459_034_036), Some(&2), "its version goes with it");
        assert_eq!(ctx.book.get(&1_459_034_036).and_then(|e| e.owner), Some(0));
        assert_eq!(ctx.earlier_session_order(21), None);

        ctx.assign_server_id(21);
        assert_eq!(ctx.server_id(21), 1_000_000_001, "the new order has an identity of its own");
    }

    #[test]
    fn an_order_of_an_earlier_session_of_this_client_is_the_order_with_that_id() {
        let mut ctx = Context::new();
        ctx.hold_recovered(21, 1_459_034_036);
        ctx.insert_order(Order::new(21, 0, Side::Buy, 1, 100, b'2', b'1', 0));
        ctx.book.get_mut(&21).unwrap().owner = Some(0);
        assert_eq!(ctx.earlier_session_order(21), Some(true), "client 0, its own order");
        // Once it is finished the id is an id like any other.
        ctx.finish_order(21, OrderStatus::Cancelled);
        assert_eq!(ctx.earlier_session_order(21), None);
    }

    // ibx#466: a new order is held under its API order id and gets a
    // server id of the generator, kept while it lives; an order held, or
    // one that has a server id, keeps its id. The generator skips the ids
    // of the orders held.
    #[test]
    fn a_new_order_gets_a_server_id_that_names_no_order_held() {
        let mut ctx = Context::new();
        ctx.order_ids.start_at(1_000_000_001);
        ctx.insert_order(Order::new(1_000_000_001, 0, Side::Buy, 1, 100, b'2', b'0', 0));
        ctx.bind_server_id(9, 1_000_000_002);
        ctx.assign_server_id(5);
        assert_eq!(ctx.server_id(5), 1_000_000_003);
        assert_eq!(ctx.key_of(1_000_000_003), 5);
        ctx.assign_server_id(5);
        assert_eq!(ctx.server_id(5), 1_000_000_003, "kept");
        ctx.insert_order(Order::new(6, 0, Side::Buy, 1, 100, b'2', b'0', 0));
        ctx.assign_server_id(6);
        assert_eq!(ctx.server_id(6), 6, "an order held keeps its id");
        assert_eq!((ctx.key_of(1_000_000_002), ctx.server_id(9)), (9, 1_000_000_002));
        assert_eq!(ctx.key_of(77), 77);
    }

    #[test]
    fn remove_order_nonexistent_no_panic() {
        let mut ctx = Context::new();
        ctx.remove_order(999); // should not panic
    }

    #[test]
    fn submit_stop_returns_id_and_drains() {
        let mut ctx = Context::new();
        let id = ctx.submit_stop(0, Side::Sell, 50, 140 * PRICE_SCALE);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price } => {
                assert_eq!(order_id, id);
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 50);
                assert_eq!(stop_price, 140 * PRICE_SCALE);
            }
            _ => panic!("Expected SubmitStop"),
        }
    }

    #[test]
    fn update_order_filled_accumulates() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::PendingSubmit,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        ctx.update_order_filled_fixed(1, (30) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.order(1).unwrap().filled_fixed / crate::types::QTY_SCALE, 30);
        ctx.update_order_filled_fixed(1, (50) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ctx.order(1).unwrap().filled_fixed / crate::types::QTY_SCALE, 80);
    }

    #[test]
    fn open_orders_for_includes_pending_and_partial() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::PendingSubmit,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (50) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 3, instrument: 0, side: Side::Buy,
            price: PRICE_SCALE, qty_fixed: (100) as i64 * crate::types::QTY_SCALE, filled_fixed: (100) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::Filled,
            ord_type: b'2', tif: b'0', stop_price: 0,
        });
        let open = ctx.open_orders_for(0);
        // PendingSubmit and PartiallyFilled count as open; Filled does not
        assert_eq!(open.len(), 2);
    }

    #[test]
    fn submit_peg_bench_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_peg_bench(0, Side::Buy, 100, 150 * PRICE_SCALE, 12345, false, 50_000_000, 50_000_000,
            151 * PRICE_SCALE, "ARCA");
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitPegBench { order_id, instrument, side, qty, price,
                ref_con_id, is_peg_decrease, pegged_change_amount, ref_change_amount,
                stock_ref_price, ref_exchange } => {
                assert_eq!(*stock_ref_price, 151 * PRICE_SCALE);
                assert_eq!(ref_exchange, "ARCA");
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 150 * PRICE_SCALE);
                assert_eq!(*ref_con_id, 12345);
                assert!(!*is_peg_decrease);
                assert_eq!(*pegged_change_amount, 50_000_000);
                assert_eq!(*ref_change_amount, 50_000_000);
            }
            _ => panic!("expected SubmitPegBench"),
        }
    }

    #[test]
    fn submit_limit_auc_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_limit_auc(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitLimitAuc { order_id, instrument, side, qty, price } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimitAuc"),
        }
    }

    #[test]
    fn submit_mtl_auc_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_mtl_auc(0, Side::Buy, 100);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitMtlAuc { order_id, instrument, side, qty } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
            }
            _ => panic!("expected SubmitMtlAuc"),
        }
    }

    #[test]
    fn submit_box_top_reuses_mtl() {
        let mut ctx = Context::new();
        let id = ctx.submit_box_top(0, Side::Buy, 100);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitMtl { order_id, instrument, side, qty } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
            }
            _ => panic!("expected SubmitMtl from box_top"),
        }
    }

    #[test]
    fn submit_what_if_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_what_if(0, Side::Buy, 100, 256_20 * (PRICE_SCALE / 100));
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitWhatIf { request } => {
                let OrderRequest::SubmitLimit { order_id, instrument, side, qty, price } = request.as_ref() else {
                    panic!("expected a limit order under the preview");
                };
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 256_20 * (PRICE_SCALE / 100));
            }
            _ => panic!("expected SubmitWhatIf"),
        }
    }

    #[test]
    fn submit_limit_fractional_drains_correctly() {
        let mut ctx = Context::new();
        // 0.5 shares = 5000 in QTY_SCALE
        let id = ctx.submit_limit_fractional(0, Side::Buy, QTY_SCALE / 2, 150 * PRICE_SCALE);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitLimitFractional { order_id, instrument, side, qty, price } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, crate::types::QTY_SCALE / 2);
                assert_eq!(*price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimitFractional"),
        }
    }

    #[test]
    fn submit_adjustable_stop_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_adjustable_stop(
            0, Side::Sell, 1,
            251_20 * (PRICE_SCALE / 100), // stop_price
            256_20 * (PRICE_SCALE / 100), // trigger_price
            AdjustedOrderType::StopLimit,
            253_20 * (PRICE_SCALE / 100), // adjusted_stop
            252_20 * (PRICE_SCALE / 100), // adjusted_limit
            0,                             // adjusted_trailing_amount (StopLimit: unused)
            0,                             // adjustable_trailing_unit
        );
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitAdjustableStop { order_id, side, qty, stop_price,
                trigger_price, adjusted_order_type, adjusted_stop_price, adjusted_stop_limit_price, .. } => {
                assert_eq!(*order_id, id);
                assert_eq!(*side, Side::Sell);
                assert_eq!(*qty, 1);
                assert_eq!(*stop_price, 251_20 * (PRICE_SCALE / 100));
                assert_eq!(*trigger_price, 256_20 * (PRICE_SCALE / 100));
                assert_eq!(*adjusted_order_type, AdjustedOrderType::StopLimit);
                assert_eq!(*adjusted_stop_price, 253_20 * (PRICE_SCALE / 100));
                assert_eq!(*adjusted_stop_limit_price, 252_20 * (PRICE_SCALE / 100));
            }
            _ => panic!("expected SubmitAdjustableStop"),
        }
    }
}
