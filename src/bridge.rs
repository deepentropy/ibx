//! Bridge module: shared state and events between the HotLoop and external callers.
//!
//! Architecture:
//! - `SharedState` composes four domain-specific containers:
//!   - `MarketDataState` — lock-free quotes (SeqLock), TBT, real-time bars, news ticks.
//!   - `OrderState` — fills, order updates, cancel rejects, what-if, order cache.
//!   - `ReferenceState` — historical data, contracts, scanners, news archives, market rules.
//!   - `PortfolioState` — account snapshot, position info, atomic positions.
//! - `Event` enum carries all events through a crossbeam channel for the `EClient` API.
//! - The HotLoop pushes to SharedState sub-containers directly.
//! - External callers read snapshots and poll events without blocking the hot loop.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::sync::{Condvar, Mutex};
use std::cell::UnsafeCell;

use std::collections::HashMap;
use crate::control::historical::{HistoricalResponse, HeadTimestampResponse};
use crate::control::contracts::{ContractDefinition, SymbolMatch};
use crate::control::scanner::ScannerResult;
use crate::control::news::NewsHeadline;
use crate::control::histogram::HistogramEntry;
use crate::control::contracts::MarketRule;
use crate::types::*;
use crate::api::types as api;

/// Enriched order info from CCP execution reports, for open_order / completed_order callbacks.
#[derive(Clone, Debug)]
pub struct RichOrderInfo {
    pub contract: api::Contract,
    pub order: api::Order,
    pub order_state: api::OrderState,
    /// Last execution details from this order's exec reports.
    pub last_exec: api::Execution,
}

/// Events emitted by the IB engine.
#[derive(Debug, Clone)]
pub enum Event {
    /// Market data tick received. Read the latest quote via `Client::quote()`.
    Tick(InstrumentId),
    /// Order filled (partial or full).
    Fill(Fill),
    /// Order status changed.
    OrderUpdate(OrderUpdate),
    /// Cancel or modify request rejected.
    CancelReject(CancelReject),
    /// Tick-by-tick trade data.
    TbtTrade(TbtTrade),
    /// Tick-by-tick bid/ask quote.
    TbtQuote(TbtQuote),
    /// What-if order response (margin/commission preview).
    WhatIf(WhatIfResponse),
    /// Real-time news headline.
    News(TickNews),
    /// Historical bar data.
    HistoricalData { req_id: u32, data: HistoricalResponse },
    /// Head timestamp response.
    HeadTimestamp { req_id: u32, data: HeadTimestampResponse },
    /// Contract details response.
    ContractDetails { req_id: u32, details: ContractDefinition },
    /// End of contract details for a request.
    ContractDetailsEnd(u32),
    /// Position update.
    PositionUpdate { instrument: InstrumentId, con_id: i64, position: i64, avg_cost: Price },
    /// Connection lost.
    Disconnected,
    /// Gateway logon completed. `ccp_session_id` matches the `x-ccp-session-id` header
    /// expected by webapp REST endpoints. `misc_urls` maps logical names (e.g. `region_dam`)
    /// to host URLs as pushed by the gateway during logon. The map is empty when the
    /// gateway does not push a URL set; callers should fall back to a documented literal
    /// (e.g. `api.ibkr.com`) in that case.
    GatewayLogon {
        ccp_session_id: String,
        misc_urls: HashMap<String, String>,
    },
}

/// SeqLock-protected quote slot. Writer (hot loop) never blocks.
/// Reader retries if it catches a write in progress.
#[repr(C)]
pub struct SeqQuote {
    version: AtomicU64,
    data: UnsafeCell<Quote>,
}

// SAFETY: SeqQuote is designed for single-writer (hot loop) + multiple-reader (Python).
// The version counter ensures readers see consistent data.
unsafe impl Sync for SeqQuote {}
unsafe impl Send for SeqQuote {}

impl SeqQuote {
    pub fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            data: UnsafeCell::new(Quote::default()),
        }
    }

    /// Write a quote (hot loop side). Never blocks.
    #[inline]
    pub fn write(&self, quote: &Quote) {
        let v = self.version.load(Ordering::Relaxed);
        self.version.store(v + 1, Ordering::Release); // odd = writing
        unsafe { *self.data.get() = *quote; }
        self.version.store(v + 2, Ordering::Release); // even = stable
    }

    /// Read a consistent quote snapshot (reader side). Spins on conflict.
    #[inline]
    pub fn read(&self) -> Quote {
        loop {
            let v1 = self.version.load(Ordering::Acquire);
            if v1 & 1 != 0 { continue; } // writer active
            let q = unsafe { *self.data.get() };
            let v2 = self.version.load(Ordering::Acquire);
            if v1 == v2 { return q; }
        }
    }
}

// ── Domain-specific state containers ──

/// Lock-free quotes, TBT streams, real-time bars, depth updates, and news ticks.
pub struct MarketDataState {
    quotes: Box<[SeqQuote; MAX_INSTRUMENTS]>,
    /// InstrumentId counter — set by hot loop on RegisterInstrument.
    instrument_count: AtomicU64,
    tbt_trades: Mutex<Vec<TbtTrade>>,
    tbt_quotes: Mutex<Vec<TbtQuote>>,
    real_time_bars: Mutex<Vec<(u32, RealTimeBar)>>,
    depth_updates: Mutex<Vec<DepthUpdate>>,
    tick_news: Mutex<Vec<TickNews>>,
    news_bulletins: Mutex<Vec<NewsBulletin>>,
}

impl MarketDataState {
    fn new() -> Self {
        Self {
            quotes: Box::new(std::array::from_fn(|_| SeqQuote::new())),
            instrument_count: AtomicU64::new(0),
            tbt_trades: Mutex::new(Vec::with_capacity(256)),
            tbt_quotes: Mutex::new(Vec::with_capacity(256)),
            real_time_bars: Mutex::new(Vec::with_capacity(64)),
            depth_updates: Mutex::new(Vec::with_capacity(64)),
            tick_news: Mutex::new(Vec::with_capacity(32)),
            news_bulletins: Mutex::new(Vec::with_capacity(16)),
        }
    }

    /// Read a quote snapshot (lock-free via SeqLock).
    /// Unchecked hot-path accessor: `id` must be a registered InstrumentId
    /// (< MAX_INSTRUMENTS) or this panics. External surfaces go through
    /// `try_quote` (ibx#234).
    #[inline]
    pub fn quote(&self, id: InstrumentId) -> Quote {
        self.quotes[id as usize].read()
    }

    /// Bounds-checked quote read for user-supplied instrument ids: an
    /// out-of-range id is a caller error, not a reason to panic the process
    /// through the language boundary (ibx#234).
    #[inline]
    pub fn try_quote(&self, id: InstrumentId) -> Option<Quote> {
        if (id as usize) < MAX_INSTRUMENTS {
            Some(self.quotes[id as usize].read())
        } else {
            None
        }
    }

    /// Number of registered instruments.
    pub fn instrument_count(&self) -> u32 {
        self.instrument_count.load(Ordering::Relaxed) as u32
    }

    pub fn drain_tbt_trades(&self) -> Vec<TbtTrade> {
        self.tbt_trades.lock().unwrap().drain(..).collect()
    }

    pub fn drain_tbt_quotes(&self) -> Vec<TbtQuote> {
        self.tbt_quotes.lock().unwrap().drain(..).collect()
    }

    pub fn drain_real_time_bars(&self) -> Vec<(u32, RealTimeBar)> {
        self.real_time_bars.lock().unwrap().drain(..).collect()
    }

    pub fn drain_depth_updates(&self) -> Vec<DepthUpdate> {
        self.depth_updates.lock().unwrap().drain(..).collect()
    }

    pub fn drain_tick_news(&self) -> Vec<TickNews> {
        self.tick_news.lock().unwrap().drain(..).collect()
    }

    pub fn drain_news_bulletins(&self) -> Vec<NewsBulletin> {
        self.news_bulletins.lock().unwrap().drain(..).collect()
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)]
    pub fn push_quote(&self, id: InstrumentId, quote: &Quote) {
        self.quotes[id as usize].write(quote);
    }

    #[doc(hidden)] pub fn push_tbt_trade(&self, trade: TbtTrade) {
        self.tbt_trades.lock().unwrap().push(trade);
    }

    #[doc(hidden)] pub fn push_tbt_quote(&self, quote: TbtQuote) {
        self.tbt_quotes.lock().unwrap().push(quote);
    }


    #[doc(hidden)] pub fn push_real_time_bar(&self, req_id: u32, bar: RealTimeBar) {
        self.real_time_bars.lock().unwrap().push((req_id, bar));
    }

    #[doc(hidden)] pub fn push_depth_update(&self, update: DepthUpdate) {
        self.depth_updates.lock().unwrap().push(update);
    }

    /// Remove all buffered depth updates for a given req_id (called on cancel).
    #[doc(hidden)] pub fn purge_depth_updates(&self, req_id: u32) {
        self.depth_updates.lock().unwrap().retain(|u| u.req_id != req_id);
    }

    #[doc(hidden)] pub fn push_tick_news(&self, news: TickNews) {
        self.tick_news.lock().unwrap().push(news);
    }

    #[doc(hidden)] pub fn push_news_bulletin(&self, bulletin: NewsBulletin) {
        self.news_bulletins.lock().unwrap().push(bulletin);
    }

    #[doc(hidden)] pub fn set_instrument_count(&self, count: u32) {
        self.instrument_count.store(count as u64, Ordering::Relaxed);
    }
}

/// Fills, order status updates, cancel rejects, what-if responses, and order cache.
pub struct OrderState {
    fills: Mutex<Vec<Fill>>,
    order_updates: Mutex<Vec<OrderUpdate>>,
    cancel_rejects: Mutex<Vec<CancelReject>>,
    what_if_responses: Mutex<Vec<WhatIfResponse>>,
    completed_orders: Mutex<Vec<CompletedOrder>>,
    /// Enriched order info from CCP exec reports (order_id -> RichOrderInfo).
    order_cache: Mutex<HashMap<u64, RichOrderInfo>>,
    /// Orders that reached a terminal state, and when. The cache row is evicted
    /// when an order completes, so the cached status alone cannot say an order
    /// is done — a replayed frame would find nothing to refuse and insert it as
    /// open.
    completed: Mutex<HashMap<u64, Instant>>,
}

/// How long a completion is remembered.
///
/// Held by age rather than by count. What this has to outlive is the window in
/// which a stale frame for the order can still arrive — a reconnect replays
/// recent activity within seconds — and that window is a duration, not a number
/// of orders. Counting instead meant a busy session's unrelated completions
/// pushed a still-relevant one out, and the replay it was there to refuse got
/// back in.
const COMPLETED_RETENTION: Duration = Duration::from_secs(300);

/// Hard cap on how many completions are remembered at once, regardless of
/// age. Expired entries are pruned first; a session that completes orders
/// faster than they expire would otherwise leave every young entry in
/// place, so once pruning alone cannot bring the map back under this bound,
/// the oldest survivors are evicted until it does. Generous enough that
/// reaching it at all means completions are arriving far faster than any
/// legitimate replay could still be racing the ones being dropped.
const COMPLETED_MAX: usize = 65_536;

impl OrderState {
    fn new() -> Self {
        Self {
            fills: Mutex::new(Vec::with_capacity(64)),
            order_updates: Mutex::new(Vec::with_capacity(64)),
            cancel_rejects: Mutex::new(Vec::with_capacity(16)),
            what_if_responses: Mutex::new(Vec::with_capacity(8)),
            completed_orders: Mutex::new(Vec::with_capacity(64)),
            order_cache: Mutex::new(HashMap::new()),
            completed: Mutex::new(HashMap::new()),
        }
    }

    pub fn drain_fills(&self) -> Vec<Fill> {
        self.fills.lock().unwrap().drain(..).collect()
    }

    pub fn drain_order_updates(&self) -> Vec<OrderUpdate> {
        self.order_updates.lock().unwrap().drain(..).collect()
    }

    pub fn drain_cancel_rejects(&self) -> Vec<CancelReject> {
        self.cancel_rejects.lock().unwrap().drain(..).collect()
    }

    pub fn drain_what_if_responses(&self) -> Vec<WhatIfResponse> {
        self.what_if_responses.lock().unwrap().drain(..).collect()
    }

    pub fn drain_completed_orders(&self) -> Vec<CompletedOrder> {
        self.completed_orders.lock().unwrap().drain(..).collect()
    }

    /// Snapshot enriched entries whose latest status is an open IB state.
    /// Terminal entries (Filled / Cancelled / Inactive / etc.) are filtered out
    /// so `req_open_orders` does not leak historical orders that are still cached
    /// for `req_completed_orders` lookups.
    pub fn drain_open_orders(&self) -> Vec<(u64, RichOrderInfo)> {
        let lock = self.order_cache.lock().unwrap();
        lock.iter()
            .filter(|(_, v)| crate::client_core::is_open_status(&v.order_state.status))
            .map(|(&k, v)| (k, v.clone()))
            .collect()
    }

    /// Get enriched order info by order_id.
    pub fn get_order_info(&self, order_id: u64) -> Option<RichOrderInfo> {
        self.order_cache.lock().unwrap().get(&order_id).cloned()
    }

    /// Remove an enriched entry. Called after a completed order has been
    /// delivered to the user, to bound `order_cache` growth in long sessions.
    pub fn remove_order_info(&self, order_id: u64) {
        self.order_cache.lock().unwrap().remove(&order_id);
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn push_fill(&self, fill: Fill) {
        self.fills.lock().unwrap().push(fill);
    }

    #[doc(hidden)] pub fn push_order_update(&self, update: OrderUpdate) {
        self.order_updates.lock().unwrap().push(update);
    }

    #[doc(hidden)] pub fn push_cancel_reject(&self, reject: CancelReject) {
        self.cancel_rejects.lock().unwrap().push(reject);
    }

    #[doc(hidden)] pub fn push_what_if(&self, response: WhatIfResponse) {
        self.what_if_responses.lock().unwrap().push(response);
    }

    #[doc(hidden)] pub fn push_completed_order(&self, order: CompletedOrder) {
        {
            let now = Instant::now();
            let mut completed = self.completed.lock().unwrap();
            completed.insert(order.order_id, now);
            // Pruned here rather than on every read: this runs once per order,
            // and a read is on the message path.
            if completed.len() > COMPLETED_MAX {
                completed.retain(|_, at| now.duration_since(*at) < COMPLETED_RETENTION);
            }
            // A burst faster than the retention window leaves nothing expired
            // for `retain` to find, so the map can still be over the cap here.
            // Evict the oldest survivors until it isn't — the actual bound,
            // not just the common case.
            if completed.len() > COMPLETED_MAX {
                let mut by_age: Vec<(u64, Instant)> = completed.iter().map(|(&id, &at)| (id, at)).collect();
                by_age.sort_unstable_by_key(|&(_, at)| at);
                for (id, _) in by_age.into_iter().take(completed.len() - COMPLETED_MAX) {
                    completed.remove(&id);
                }
            }
        }
        self.completed_orders.lock().unwrap().push(order);
    }

    /// Whether this order completed recently enough that a frame reopening it
    /// is a replay rather than news.
    fn recently_completed(&self, order_id: u64) -> bool {
        self.completed.lock().unwrap().get(&order_id)
            .is_some_and(|at| at.elapsed() < COMPLETED_RETENTION)
    }

    /// Whether a status ends an order's life.
    ///
    /// These are the three the engine acts on by removing the order from its
    /// book. `Inactive` is not among them — it returns to working when the
    /// condition holding the order clears — and neither is `Uncertain`, which
    /// states the opposite of a conclusion.
    fn is_terminal_status(status: &str) -> bool {
        matches!(status, "Filled" | "Cancelled" | "Rejected")
    }

    /// Cache the enriched view of an order.
    ///
    /// An order that has completed is not returned to a working status. Nothing
    /// remembered that an order was done, so a replayed frame — the reconnect
    /// open-order burst racing a fill, or any message the gateway resends —
    /// wrote `Submitted` over the terminal entry, and `req_open_orders` then
    /// reported a completed order as live (ibx#262).
    ///
    /// The cached status alone cannot carry that knowledge, because completing
    /// an order evicts its cache row: the replayed frame finds nothing to refuse
    /// and inserts itself. The completed-id memory is what survives the
    /// eviction, and an intervening terminal report cannot overwrite the
    /// evidence the way a cached string could.
    ///
    /// A correction from the gateway is not a replay and goes through
    /// [`push_order_correction`](Self::push_order_correction).
    #[doc(hidden)] pub fn push_order_info(&self, order_id: u64, info: RichOrderInfo) {
        if crate::client_core::is_open_status(&info.order_state.status) {
            if self.recently_completed(order_id) {
                return;
            }
            let cache = self.order_cache.lock().unwrap();
            if cache.get(&order_id)
                .is_some_and(|e| Self::is_terminal_status(&e.order_state.status))
            {
                return;
            }
        }
        self.order_cache.lock().unwrap().insert(order_id, info);
    }

    /// Cache a view that supersedes a completed one.
    ///
    /// A trade cancel or trade correction restates an execution the gateway has
    /// already reported, so it can legitimately return a filled order to a
    /// working quantity. That is the gateway's own statement rather than a
    /// replay of an older one, so it is not refused, and the order stops being
    /// remembered as completed.
    #[doc(hidden)] pub fn push_order_correction(&self, order_id: u64, info: RichOrderInfo) {
        self.completed.lock().unwrap().remove(&order_id);
        self.order_cache.lock().unwrap().insert(order_id, info);
    }
}

/// Historical data, contract definitions, scanners, news archives, market rules, contract cache.
pub struct ReferenceState {
    historical_data: Mutex<Vec<(u32, HistoricalResponse)>>,
    head_timestamps: Mutex<Vec<(u32, HeadTimestampResponse)>>,
    contract_details: Mutex<Vec<(u32, ContractDefinition)>>,
    contract_details_end: Mutex<Vec<u32>>,
    matching_symbols: Mutex<Vec<(u32, Vec<SymbolMatch>)>>,
    scanner_params: Mutex<Vec<String>>,
    scanner_data: Mutex<Vec<(u32, ScannerResult)>>,
    historical_news: Mutex<Vec<(u32, Vec<NewsHeadline>, bool)>>,
    news_articles: Mutex<Vec<(u32, i32, String)>>,
    fundamental_data: Mutex<Vec<(u32, String)>>,
    histogram_data: Mutex<Vec<(u32, Vec<HistogramEntry>)>>,
    historical_ticks: Mutex<Vec<(u32, HistoricalTickData, String, bool)>>,
    historical_schedules: Mutex<Vec<(u32, HistoricalScheduleResponse)>>,
    /// Errors surfaced by HMDS for in-flight reference queries (req_id, code, message).
    /// Drained by the dispatcher and forwarded to `Wrapper::error`. ibx#186.
    historical_errors: Mutex<Vec<(u32, i32, String)>>,
    market_rules: Mutex<Vec<MarketRule>>,
    depth_exchanges_cache: Mutex<Vec<DepthMktDataDescription>>,
    depth_exchanges_pending: Mutex<bool>,
    /// Contract cache from CCP exec reports (con_id -> api::Contract).
    contract_cache: Mutex<HashMap<i64, api::Contract>>,
    /// Gateway-local init data (populated during connection, read-only after).
    smart_components: Mutex<Vec<crate::types::SmartComponent>>,
    news_providers: Mutex<Vec<crate::types::NewsProvider>>,
    soft_dollar_tiers: Mutex<Vec<crate::types::SoftDollarTier>>,
    family_codes: Mutex<Vec<crate::types::FamilyCode>>,
    white_branding_id: Mutex<String>,
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    ccp_session_id: Mutex<String>,
    /// Logical-name → host URL map pushed by the gateway during logon.
    misc_urls: Mutex<HashMap<String, String>>,
}

impl ReferenceState {
    fn new() -> Self {
        Self {
            historical_data: Mutex::new(Vec::with_capacity(16)),
            head_timestamps: Mutex::new(Vec::with_capacity(8)),
            contract_details: Mutex::new(Vec::with_capacity(16)),
            contract_details_end: Mutex::new(Vec::with_capacity(8)),
            matching_symbols: Mutex::new(Vec::with_capacity(8)),
            scanner_params: Mutex::new(Vec::new()),
            scanner_data: Mutex::new(Vec::with_capacity(8)),
            historical_news: Mutex::new(Vec::with_capacity(8)),
            news_articles: Mutex::new(Vec::with_capacity(8)),
            fundamental_data: Mutex::new(Vec::with_capacity(4)),
            histogram_data: Mutex::new(Vec::with_capacity(4)),
            historical_ticks: Mutex::new(Vec::with_capacity(4)),
            historical_schedules: Mutex::new(Vec::with_capacity(4)),
            historical_errors: Mutex::new(Vec::with_capacity(4)),
            market_rules: Mutex::new(Vec::new()),
            depth_exchanges_cache: Mutex::new(Vec::new()),
            depth_exchanges_pending: Mutex::new(false),
            contract_cache: Mutex::new(HashMap::new()),
            smart_components: Mutex::new(Vec::new()),
            news_providers: Mutex::new(Vec::new()),
            soft_dollar_tiers: Mutex::new(Vec::new()),
            family_codes: Mutex::new(Vec::new()),
            white_branding_id: Mutex::new(String::new()),
            ccp_session_id: Mutex::new(String::new()),
            misc_urls: Mutex::new(HashMap::new()),
        }
    }

    pub fn drain_historical_data(&self) -> Vec<(u32, HistoricalResponse)> {
        self.historical_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_head_timestamps(&self) -> Vec<(u32, HeadTimestampResponse)> {
        self.head_timestamps.lock().unwrap().drain(..).collect()
    }

    pub fn drain_contract_details(&self) -> Vec<(u32, ContractDefinition)> {
        self.contract_details.lock().unwrap().drain(..).collect()
    }

    pub fn drain_contract_details_end(&self) -> Vec<u32> {
        self.contract_details_end.lock().unwrap().drain(..).collect()
    }

    pub fn drain_matching_symbols(&self) -> Vec<(u32, Vec<SymbolMatch>)> {
        self.matching_symbols.lock().unwrap().drain(..).collect()
    }

    pub fn drain_scanner_params(&self) -> Vec<String> {
        self.scanner_params.lock().unwrap().drain(..).collect()
    }

    pub fn drain_scanner_data(&self) -> Vec<(u32, ScannerResult)> {
        self.scanner_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_news(&self) -> Vec<(u32, Vec<NewsHeadline>, bool)> {
        self.historical_news.lock().unwrap().drain(..).collect()
    }

    pub fn drain_news_articles(&self) -> Vec<(u32, i32, String)> {
        self.news_articles.lock().unwrap().drain(..).collect()
    }

    pub fn drain_fundamental_data(&self) -> Vec<(u32, String)> {
        self.fundamental_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_histogram_data(&self) -> Vec<(u32, Vec<HistogramEntry>)> {
        self.histogram_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_ticks(&self) -> Vec<(u32, HistoricalTickData, String, bool)> {
        self.historical_ticks.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_schedules(&self) -> Vec<(u32, HistoricalScheduleResponse)> {
        self.historical_schedules.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_errors(&self) -> Vec<(u32, i32, String)> {
        self.historical_errors.lock().unwrap().drain(..).collect()
    }

    /// Get cached market rules.
    pub fn market_rules(&self) -> Vec<MarketRule> {
        self.market_rules.lock().unwrap().clone()
    }

    /// Get a market rule by ID.
    pub fn market_rule(&self, rule_id: i32) -> Option<MarketRule> {
        self.market_rules.lock().unwrap().iter().find(|r| r.rule_id == rule_id).cloned()
    }

    /// Get cached contract by con_id.
    pub fn get_contract(&self, con_id: i64) -> Option<api::Contract> {
        self.contract_cache.lock().unwrap().get(&con_id).cloned()
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn push_historical_data(&self, req_id: u32, response: HistoricalResponse) {
        self.historical_data.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_head_timestamp(&self, req_id: u32, response: HeadTimestampResponse) {
        self.head_timestamps.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_contract_details(&self, req_id: u32, def: ContractDefinition) {
        self.contract_details.lock().unwrap().push((req_id, def));
    }

    #[doc(hidden)] pub fn push_contract_details_end(&self, req_id: u32) {
        self.contract_details_end.lock().unwrap().push(req_id);
    }

    #[doc(hidden)] pub fn push_matching_symbols(&self, req_id: u32, matches: Vec<SymbolMatch>) {
        self.matching_symbols.lock().unwrap().push((req_id, matches));
    }

    #[doc(hidden)] pub fn push_scanner_params(&self, xml: String) {
        self.scanner_params.lock().unwrap().push(xml);
    }

    #[doc(hidden)] pub fn push_scanner_data(&self, req_id: u32, result: ScannerResult) {
        self.scanner_data.lock().unwrap().push((req_id, result));
    }

    #[doc(hidden)] pub fn push_historical_news(&self, req_id: u32, headlines: Vec<NewsHeadline>, has_more: bool) {
        self.historical_news.lock().unwrap().push((req_id, headlines, has_more));
    }

    #[doc(hidden)] pub fn push_news_article(&self, req_id: u32, article_type: i32, article_text: String) {
        self.news_articles.lock().unwrap().push((req_id, article_type, article_text));
    }

    #[doc(hidden)] pub fn push_fundamental_data(&self, req_id: u32, data: String) {
        self.fundamental_data.lock().unwrap().push((req_id, data));
    }

    #[doc(hidden)] pub fn push_histogram_data(&self, req_id: u32, entries: Vec<HistogramEntry>) {
        self.histogram_data.lock().unwrap().push((req_id, entries));
    }

    #[doc(hidden)] pub fn push_historical_ticks(&self, req_id: u32, data: HistoricalTickData, what_to_show: String, done: bool) {
        self.historical_ticks.lock().unwrap().push((req_id, data, what_to_show, done));
    }

    #[doc(hidden)] pub fn push_historical_schedule(&self, req_id: u32, response: HistoricalScheduleResponse) {
        self.historical_schedules.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_historical_error(&self, req_id: u32, code: i32, message: String) {
        self.historical_errors.lock().unwrap().push((req_id, code, message));
    }

    #[doc(hidden)] pub fn push_market_rules(&self, rules: Vec<MarketRule>) {
        let mut lock = self.market_rules.lock().unwrap();
        for rule in rules {
            if !lock.iter().any(|r| r.rule_id == rule.rule_id) {
                lock.push(rule);
            }
        }
    }

    pub fn drain_depth_exchanges(&self) -> Vec<DepthMktDataDescription> {
        let mut pending = self.depth_exchanges_pending.lock().unwrap();
        if *pending {
            *pending = false;
            self.depth_exchanges_cache.lock().unwrap().clone()
        } else {
            Vec::new()
        }
    }

    #[doc(hidden)] pub fn push_depth_exchanges(&self, descs: Vec<DepthMktDataDescription>) {
        self.depth_exchanges_cache.lock().unwrap().extend(descs);
    }

    #[doc(hidden)] pub fn notify_depth_exchanges(&self) {
        *self.depth_exchanges_pending.lock().unwrap() = true;
    }

    #[doc(hidden)] pub fn cache_contract(&self, con_id: i64, contract: api::Contract) {
        let mut cache = self.contract_cache.lock().unwrap();
        if let Some(existing) = cache.get_mut(&con_id) {
            // Merge: only overwrite fields that are non-empty in the new contract
            if !contract.symbol.is_empty() { existing.symbol = contract.symbol; }
            if !contract.sec_type.is_empty() { existing.sec_type = contract.sec_type; }
            if !contract.exchange.is_empty() { existing.exchange = contract.exchange; }
            if !contract.currency.is_empty() { existing.currency = contract.currency; }
            if !contract.local_symbol.is_empty() { existing.local_symbol = contract.local_symbol; }
            if !contract.primary_exchange.is_empty() { existing.primary_exchange = contract.primary_exchange; }
            if !contract.trading_class.is_empty() { existing.trading_class = contract.trading_class; }
        } else {
            cache.insert(con_id, contract);
        }
    }

    // ── Gateway-local init data ──

    pub fn smart_components(&self) -> Vec<crate::types::SmartComponent> {
        self.smart_components.lock().unwrap().clone()
    }

    pub fn news_providers(&self) -> Vec<crate::types::NewsProvider> {
        self.news_providers.lock().unwrap().clone()
    }

    pub fn soft_dollar_tiers(&self) -> Vec<crate::types::SoftDollarTier> {
        self.soft_dollar_tiers.lock().unwrap().clone()
    }

    pub fn family_codes(&self) -> Vec<crate::types::FamilyCode> {
        self.family_codes.lock().unwrap().clone()
    }

    pub fn white_branding_id(&self) -> String {
        self.white_branding_id.lock().unwrap().clone()
    }

    /// Session ID surfaced to webapp REST clients as the `x-ccp-session-id` header.
    /// Empty until gateway logon completes.
    pub fn ccp_session_id(&self) -> String {
        self.ccp_session_id.lock().unwrap().clone()
    }

    /// Logical-name → host URL map pushed by the gateway during logon. Empty when
    /// no URL set was pushed; consumers should fall back to a documented literal
    /// (e.g. `api.ibkr.com` for `region_dam`).
    pub fn misc_urls(&self) -> HashMap<String, String> {
        self.misc_urls.lock().unwrap().clone()
    }

    /// Single lookup against the URL map. Returns `None` when missing.
    pub fn misc_url(&self, key: &str) -> Option<String> {
        self.misc_urls.lock().unwrap().get(key).cloned()
    }

    #[doc(hidden)] pub fn set_smart_components(&self, components: Vec<crate::types::SmartComponent>) {
        *self.smart_components.lock().unwrap() = components;
    }

    #[doc(hidden)] pub fn set_news_providers(&self, providers: Vec<crate::types::NewsProvider>) {
        *self.news_providers.lock().unwrap() = providers;
    }

    #[doc(hidden)] pub fn set_soft_dollar_tiers(&self, tiers: Vec<crate::types::SoftDollarTier>) {
        *self.soft_dollar_tiers.lock().unwrap() = tiers;
    }

    #[doc(hidden)] pub fn set_family_codes(&self, codes: Vec<crate::types::FamilyCode>) {
        *self.family_codes.lock().unwrap() = codes;
    }

    #[doc(hidden)] pub fn set_white_branding_id(&self, id: String) {
        *self.white_branding_id.lock().unwrap() = id;
    }

    #[doc(hidden)] pub fn set_ccp_session_id(&self, id: String) {
        *self.ccp_session_id.lock().unwrap() = id;
    }

    #[doc(hidden)] pub fn set_misc_urls(&self, urls: HashMap<String, String>) {
        *self.misc_urls.lock().unwrap() = urls;
    }
}

/// Account snapshot, per-position info, and atomic instrument positions.
pub struct PortfolioState {
    account: Mutex<AccountState>,
    /// True once the first gateway account message ("UT"/"UM"/"RL") has been received.
    account_data_received: AtomicBool,
    /// True once the CCP init burst has been fully processed.
    account_download_complete: AtomicBool,
    /// Position info (conId -> PositionInfo) for reqPositions and P&L.
    position_infos: Mutex<HashMap<i64, PositionInfo>>,
    positions: [AtomicU64; MAX_INSTRUMENTS],
    /// Midnight seeds from 6040=143 for client-side daily P&L computation.
    midnight_seeds: Mutex<HashMap<i64, MidnightSeed>>,
}

impl PortfolioState {
    fn new() -> Self {
        Self {
            account: Mutex::new(AccountState::default()),
            account_data_received: AtomicBool::new(false),
            account_download_complete: AtomicBool::new(false),
            position_infos: Mutex::new(HashMap::new()),
            positions: std::array::from_fn(|_| AtomicU64::new(0)),
            midnight_seeds: Mutex::new(HashMap::new()),
        }
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        *self.account.lock().unwrap()
    }

    /// Get all position infos (for reqPositions).
    pub fn position_infos(&self) -> Vec<PositionInfo> {
        self.position_infos.lock().unwrap().values().cloned().collect()
    }

    /// Get position info for a single conId (for pnlSingle).
    pub fn position_info(&self, con_id: i64) -> Option<PositionInfo> {
        self.position_infos.lock().unwrap().get(&con_id).cloned()
    }

    /// Read current position for an instrument.
    pub fn position(&self, id: InstrumentId) -> i64 {
        self.positions[id as usize].load(Ordering::Relaxed) as i64
    }

    // ── Hot-loop-side writers ──

    /// True once at least one gateway account message has been processed.
    pub fn account_data_received(&self) -> bool {
        self.account_data_received.load(Ordering::Acquire)
    }

    #[doc(hidden)] pub fn set_account(&self, account: &AccountState) {
        *self.account.lock().unwrap() = *account;
        self.account_data_received.store(true, Ordering::Release);
    }

    /// Mark account download as complete (init burst processed).
    #[doc(hidden)] pub fn set_account_download_complete(&self) {
        self.account_download_complete.store(true, Ordering::Release);
    }

    /// True once the CCP init burst has been fully processed.
    pub fn account_download_complete(&self) -> bool {
        self.account_download_complete.load(Ordering::Acquire)
    }

    #[doc(hidden)] pub fn set_position_info(&self, info: PositionInfo) {
        let mut map = self.position_infos.lock().unwrap();
        match map.get_mut(&info.con_id) {
            Some(existing) => {
                existing.position = info.position;
                existing.avg_cost = info.avg_cost;
                if !info.symbol.is_empty() { existing.symbol = info.symbol; }
                if !info.sec_type.is_empty() { existing.sec_type = info.sec_type; }
                if !info.currency.is_empty() { existing.currency = info.currency; }
                if !info.multiplier.is_empty() { existing.multiplier = info.multiplier; }
                // Marks are owned by set_position_marks; leave them untouched so
                // the lean position feed can't zero them (ib-agent#172).
            }
            None => { map.insert(info.con_id, info); }
        }
    }

    /// Update the per-position marks (from the account-updates portfolio message).
    /// Kept separate from set_position_info so the lean position feed, which has
    /// no marks, does not overwrite them (ib-agent#172).
    #[doc(hidden)] pub fn set_position_marks(&self, con_id: i64, market_price: Price, market_value: Price, unrealized_pnl: Price, realized_pnl: Price) {
        let mut map = self.position_infos.lock().unwrap();
        let entry = map.entry(con_id).or_insert_with(|| PositionInfo { con_id, ..Default::default() });
        entry.market_price = market_price;
        entry.market_value = market_value;
        entry.unrealized_pnl = unrealized_pnl;
        entry.realized_pnl = realized_pnl;
    }

    #[doc(hidden)] pub fn set_position(&self, id: InstrumentId, pos: i64) {
        self.positions[id as usize].store(pos as u64, Ordering::Relaxed);
    }

    /// Store midnight seeds from 6040=143 P&L response.
    #[doc(hidden)] pub fn set_midnight_seeds(&self, seeds: Vec<MidnightSeed>) {
        let mut map = self.midnight_seeds.lock().unwrap();
        map.clear();
        for s in seeds {
            map.insert(s.con_id, s);
        }
    }

    /// Read midnight seeds for client-side P&L computation.
    pub fn midnight_seeds(&self) -> Vec<MidnightSeed> {
        self.midnight_seeds.lock().unwrap().values().copied().collect()
    }
}

/// Shared state between hot loop and external caller.
/// Composed of domain-specific containers for clear ownership boundaries.
pub struct SharedState {
    pub market: MarketDataState,
    pub orders: OrderState,
    pub reference: ReferenceState,
    pub portfolio: PortfolioState,
    /// Last measured auth-connection round-trip time in nanoseconds
    /// (0 = never measured). Sampled from the test-request/echo cycle —
    /// see `HotLoop` liveness and `ControlCommand::Ping` (ibx#158).
    ccp_rtt_ns: AtomicU64,
    /// Set by the hot loop when the session is over (connection lost, engine
    /// stopped, or reconnect exhausted). Read-and-clear by the client so the
    /// `connection_closed` callback can fire without an event channel
    /// (ibx#242). The `Event::Disconnected` channel path is optional; this
    /// flag is always populated.
    connection_lost: AtomicBool,
    /// Notifier for waking consumers (e.g. Python event loop) when data arrives.
    notify_mutex: Mutex<bool>,
    notify_condvar: Condvar,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            market: MarketDataState::new(),
            orders: OrderState::new(),
            reference: ReferenceState::new(),
            portfolio: PortfolioState::new(),
            ccp_rtt_ns: AtomicU64::new(0),
            connection_lost: AtomicBool::new(false),
            notify_mutex: Mutex::new(false),
            notify_condvar: Condvar::new(),
        }
    }

    /// Signal that the session is over. Hot-loop side (ibx#242).
    #[doc(hidden)]
    #[inline]
    pub fn set_connection_lost(&self) {
        self.connection_lost.store(true, Ordering::Release);
        self.notify();
    }

    /// Read and clear the connection-lost flag. Returns `true` at most once per
    /// signal, so the caller can fire `connection_closed` exactly once.
    #[inline]
    pub fn take_connection_lost(&self) -> bool {
        self.connection_lost.swap(false, Ordering::AcqRel)
    }

    /// Record an auth-connection RTT sample (ibx#158). Hot-loop side.
    #[inline]
    pub fn set_ccp_rtt(&self, rtt: std::time::Duration) {
        self.ccp_rtt_ns.store(rtt.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    /// Last measured auth-connection round-trip time, if any (ibx#158).
    /// A gauge, not a benchmark: the sample is the interval from a test
    /// request to the first inbound traffic that followed it, which on an
    /// active feed can undercount by racing data already in flight.
    #[inline]
    pub fn last_ccp_rtt(&self) -> Option<std::time::Duration> {
        match self.ccp_rtt_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(std::time::Duration::from_nanos(ns)),
        }
    }

    /// Signal that new data is available. Called by hot loop after pushing data.
    #[inline]
    pub fn notify(&self) {
        let mut pending = self.notify_mutex.lock().unwrap();
        *pending = true;
        self.notify_condvar.notify_one();
    }

    /// Wait for data notification with a timeout. Returns true if notified, false if timed out.
    pub fn wait_for_data(&self, timeout: std::time::Duration) -> bool {
        let mut pending = self.notify_mutex.lock().unwrap();
        if *pending {
            *pending = false;
            return true;
        }
        let (lock, result) = self.notify_condvar.wait_timeout(pending, timeout).unwrap();
        let had_data = *lock;
        if had_data {
            // Reset the flag via a mutable reference obtained from the MutexGuard's deref.
            drop(lock);
            *self.notify_mutex.lock().unwrap() = false;
        }
        had_data || !result.timed_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqquote_write_read_roundtrip() {
        let sq = SeqQuote::new();
        let mut q = Quote::default();
        q.bid = 150 * PRICE_SCALE;
        q.ask = 151 * PRICE_SCALE;
        sq.write(&q);
        let read = sq.read();
        assert_eq!(read.bid, 150 * PRICE_SCALE);
        assert_eq!(read.ask, 151 * PRICE_SCALE);
    }

    #[test]
    fn seqquote_default_is_zero() {
        let sq = SeqQuote::new();
        let q = sq.read();
        assert_eq!(q.bid, 0);
        assert_eq!(q.ask, 0);
    }

    #[test]
    fn shared_state_fills_drain() {
        let ss = SharedState::new();
        ss.orders.push_fill(Fill {
            instrument: 0, order_id: 1, side: Side::Buy,
            price: 100 * PRICE_SCALE, qty: 10, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
        ss.orders.push_fill(Fill {
            instrument: 0, order_id: 2, side: Side::Sell,
            price: 101 * PRICE_SCALE, qty: 5, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
        let fills = ss.orders.drain_fills();
        assert_eq!(fills.len(), 2);
        // Second drain should be empty
        assert!(ss.orders.drain_fills().is_empty());
    }

    #[test]
    fn shared_state_order_updates_drain() {
        let ss = SharedState::new();
        ss.orders.push_order_update(OrderUpdate {
            order_id: 1, instrument: 0, status: OrderStatus::Submitted,
            filled_qty: 0, remaining_qty: 100, perm_id: 0, parent_id: 0, timestamp_ns: 0,
        });
        let updates = ss.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert!(ss.orders.drain_order_updates().is_empty());
    }

    #[test]
    fn shared_state_position_roundtrip() {
        let ss = SharedState::new();
        assert_eq!(ss.portfolio.position(0), 0);
        ss.portfolio.set_position(0, 42);
        assert_eq!(ss.portfolio.position(0), 42);
        ss.portfolio.set_position(0, -10);
        assert_eq!(ss.portfolio.position(0), -10);
    }

    #[test]
    fn shared_state_account_roundtrip() {
        let ss = SharedState::new();
        let mut a = AccountState::default();
        a.net_liquidation = 100_000 * PRICE_SCALE;
        ss.portfolio.set_account(&a);
        let read = ss.portfolio.account();
        assert_eq!(read.net_liquidation, 100_000 * PRICE_SCALE);
    }

    #[test]
    fn reference_state_ccp_session_id_roundtrip() {
        let ss = SharedState::new();
        assert!(ss.reference.ccp_session_id().is_empty());
        ss.reference.set_ccp_session_id("abc.0001".to_string());
        assert_eq!(ss.reference.ccp_session_id(), "abc.0001");
    }

    #[test]
    fn reference_state_misc_urls_roundtrip() {
        let ss = SharedState::new();
        assert!(ss.reference.misc_urls().is_empty());
        assert!(ss.reference.misc_url("region_dam").is_none());
        let mut urls = HashMap::new();
        urls.insert("region_dam".to_string(), "api-east.example.com".to_string());
        urls.insert("margin".to_string(), "margin.example.com".to_string());
        ss.reference.set_misc_urls(urls);
        let map = ss.reference.misc_urls();
        assert_eq!(map.len(), 2);
        assert_eq!(ss.reference.misc_url("region_dam").as_deref(), Some("api-east.example.com"));
        assert_eq!(ss.reference.misc_url("missing"), None);
    }

    #[test]
    fn event_gateway_logon_carries_fields() {
        let mut urls = HashMap::new();
        urls.insert("region_dam".to_string(), "api.example.com".to_string());
        let event = Event::GatewayLogon {
            ccp_session_id: "sid.abcd".to_string(),
            misc_urls: urls,
        };
        match event {
            Event::GatewayLogon { ccp_session_id, misc_urls } => {
                assert_eq!(ccp_session_id, "sid.abcd");
                assert_eq!(misc_urls.get("region_dam").map(String::as_str), Some("api.example.com"));
            }
            _ => panic!("expected GatewayLogon"),
        }
    }

    #[test]
    fn seqquote_concurrent_read_write() {
        use std::sync::Arc;
        use std::thread;

        let sq = Arc::new(SeqQuote::new());
        let sq_writer = sq.clone();
        let sq_reader = sq.clone();

        let writer = thread::spawn(move || {
            for i in 0..1000 {
                let mut q = Quote::default();
                q.bid = i * PRICE_SCALE;
                q.ask = (i + 1) * PRICE_SCALE;
                sq_writer.write(&q);
            }
        });

        let reader = thread::spawn(move || {
            for _ in 0..1000 {
                let q = sq_reader.read();
                // bid and ask should be consistent (ask = bid + PRICE_SCALE)
                if q.bid != 0 {
                    assert_eq!(q.ask, q.bid + PRICE_SCALE);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }

    fn info(status: &str) -> RichOrderInfo {
        RichOrderInfo {
            contract: api::Contract::default(),
            order: api::Order::default(),
            order_state: api::OrderState { status: status.to_string(), ..Default::default() },
            last_exec: api::Execution::default(),
        }
    }

    fn completed(order_id: u64) -> CompletedOrder {
        CompletedOrder {
            order_id, instrument: 0, status: crate::types::OrderStatus::Filled,
            filled_qty: 100, timestamp_ns: 0,
        }
    }

    /// ibx#262: nothing remembered that an order had completed, so a replayed
    /// frame wrote `Submitted` over the terminal entry and `req_open_orders`
    /// reported a completed order as live. A strategy then re-manages a position
    /// it already has, or cancels an order that no longer exists, with the
    /// open-order snapshot corroborating the wrong picture.
    #[test]
    fn a_completed_order_is_not_returned_to_the_open_book() {
        for terminal in ["Filled", "Cancelled", "Rejected"] {
            let shared = SharedState::new();
            shared.orders.push_order_info(7, info(terminal));

            for open in ["Submitted", "PreSubmitted", "PartiallyFilled", "PendingCancel"] {
                shared.orders.push_order_info(7, info(open));
                assert_eq!(
                    shared.orders.get_order_info(7).unwrap().order_state.status, terminal,
                    "{open} must not overwrite {terminal}",
                );
            }
            assert!(
                shared.orders.drain_open_orders().is_empty(),
                "and {terminal} stays out of the open-order snapshot",
            );
        }
    }

    /// Completing an order evicts its cache row, so the cached status cannot be
    /// what remembers the order is done — the replayed frame finds nothing to
    /// refuse and inserts itself. This is the ordinary path, not an edge case.
    #[test]
    fn a_completion_outlives_the_cache_row_it_evicts() {
        let shared = SharedState::new();
        shared.orders.push_order_info(7, info("Filled"));
        shared.orders.push_completed_order(completed(7));
        shared.orders.remove_order_info(7);

        shared.orders.push_order_info(7, info("Submitted"));
        assert!(
            shared.orders.get_order_info(7).is_none(),
            "a replayed frame must not re-open an order whose row has been evicted",
        );
        assert!(shared.orders.drain_open_orders().is_empty());
    }

    /// A terminal report arriving between the completion and the replay must
    /// not become the thing the guard compares against — a cached string is
    /// overwritten by the next terminal report, and the replay then passes.
    #[test]
    fn an_intervening_report_does_not_erase_the_completion() {
        let shared = SharedState::new();
        shared.orders.push_order_info(7, info("Filled"));
        shared.orders.push_completed_order(completed(7));

        shared.orders.push_order_info(7, info("Cancelled"));
        shared.orders.push_order_info(7, info("Submitted"));

        assert_ne!(
            shared.orders.get_order_info(7).unwrap().order_state.status, "Submitted",
            "the completion survives whatever terminal report lands on top of it",
        );
        assert!(shared.orders.drain_open_orders().is_empty());
    }

    /// A trade cancel or correction restates an execution the gateway already
    /// reported, so it can return a filled order to a working quantity. It is
    /// the gateway's own statement, not a replay of an older one.
    #[test]
    fn a_trade_correction_can_reopen_a_completed_order() {
        let shared = SharedState::new();
        shared.orders.push_order_info(7, info("Filled"));
        shared.orders.push_completed_order(completed(7));
        shared.orders.remove_order_info(7);

        shared.orders.push_order_correction(7, info("PartiallyFilled"));
        assert_eq!(
            shared.orders.get_order_info(7).unwrap().order_state.status, "PartiallyFilled",
            "a correction is not a replay",
        );

        // And the order stops being remembered as completed, so its subsequent
        // ordinary reports are not refused either.
        shared.orders.push_order_info(7, info("Submitted"));
        assert_eq!(shared.orders.get_order_info(7).unwrap().order_state.status, "Submitted");
    }

    /// The ordinary direction still works — without this the guards above would
    /// pass against a cache that refuses every update.
    #[test]
    fn a_fill_still_writes_over_a_working_status() {
        let shared = SharedState::new();
        shared.orders.push_order_info(9, info("Submitted"));
        shared.orders.push_order_info(9, info("Filled"));
        assert_eq!(shared.orders.get_order_info(9).unwrap().order_state.status, "Filled");
    }

    /// Held by age, not by count. What the memory has to outlive is the window
    /// in which a stale frame for the order can still arrive; counting instead
    /// meant a busy session's unrelated completions pushed a still-relevant
    /// entry out, and the replay it was there to refuse got back in. Stays one
    /// short of COMPLETED_MAX so this exercises time-based retention only —
    /// the hard cap itself is a separate concern, proved below.
    #[test]
    fn a_completion_is_remembered_for_a_window_not_a_quota() {
        let shared = SharedState::new();
        shared.orders.push_completed_order(completed(7));

        // Far more unrelated completions than any small count-based bound
        // would hold, without reaching the hard cap.
        for id in 1000..(1000 + COMPLETED_MAX as u64 - 1) {
            shared.orders.push_completed_order(completed(id));
        }

        shared.orders.push_order_info(7, info("Submitted"));
        assert!(
            shared.orders.get_order_info(7).is_none(),
            "the completion survives however many other orders complete beside it",
        );
        assert!(shared.orders.drain_open_orders().is_empty());
    }

    /// And it does not accumulate for the life of the process: past the cap
    /// the oldest entries are dropped. None of these expire during the test
    /// (COMPLETED_RETENTION is minutes), so expiry-based pruning alone is a
    /// no-op here — only the hard eviction fallback can keep the map bounded.
    #[test]
    fn the_completed_memory_does_not_grow_without_limit() {
        let shared = SharedState::new();
        for id in 0..(COMPLETED_MAX as u64 + 10) {
            shared.orders.push_completed_order(completed(id));
        }
        let held = shared.orders.completed.lock().unwrap().len();
        assert!(held <= COMPLETED_MAX, "hard cap must hold even with nothing expired, held {held}");
    }
}
