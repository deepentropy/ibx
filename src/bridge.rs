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

use std::sync::atomic::{AtomicU64, Ordering};
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
    #[inline]
    pub fn quote(&self, id: InstrumentId) -> Quote {
        self.quotes[id as usize].read()
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
}

impl OrderState {
    fn new() -> Self {
        Self {
            fills: Mutex::new(Vec::with_capacity(64)),
            order_updates: Mutex::new(Vec::with_capacity(64)),
            cancel_rejects: Mutex::new(Vec::with_capacity(16)),
            what_if_responses: Mutex::new(Vec::with_capacity(8)),
            completed_orders: Mutex::new(Vec::with_capacity(64)),
            order_cache: Mutex::new(HashMap::new()),
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

    /// Snapshot all enriched orders (for open_order callbacks).
    /// Does NOT remove entries — they persist for req_completed_orders lookups.
    pub fn drain_open_orders(&self) -> Vec<(u64, RichOrderInfo)> {
        let lock = self.order_cache.lock().unwrap();
        lock.iter().map(|(&k, v)| (k, v.clone())).collect()
    }

    /// Get enriched order info by order_id.
    pub fn get_order_info(&self, order_id: u64) -> Option<RichOrderInfo> {
        self.order_cache.lock().unwrap().get(&order_id).cloned()
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
        self.completed_orders.lock().unwrap().push(order);
    }

    #[doc(hidden)] pub fn push_order_info(&self, order_id: u64, info: RichOrderInfo) {
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
    market_rules: Mutex<Vec<MarketRule>>,
    /// Contract cache from CCP exec reports (con_id -> api::Contract).
    contract_cache: Mutex<HashMap<i64, api::Contract>>,
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
            market_rules: Mutex::new(Vec::new()),
            contract_cache: Mutex::new(HashMap::new()),
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

    #[doc(hidden)] pub fn push_market_rules(&self, rules: Vec<MarketRule>) {
        let mut lock = self.market_rules.lock().unwrap();
        for rule in rules {
            if !lock.iter().any(|r| r.rule_id == rule.rule_id) {
                lock.push(rule);
            }
        }
    }

    #[doc(hidden)] pub fn cache_contract(&self, con_id: i64, contract: api::Contract) {
        self.contract_cache.lock().unwrap().insert(con_id, contract);
    }
}

/// Account snapshot, per-position info, and atomic instrument positions.
pub struct PortfolioState {
    account: Mutex<AccountState>,
    /// Position info (conId -> PositionInfo) for reqPositions and P&L.
    position_infos: Mutex<HashMap<i64, PositionInfo>>,
    positions: [AtomicU64; MAX_INSTRUMENTS],
}

impl PortfolioState {
    fn new() -> Self {
        Self {
            account: Mutex::new(AccountState::default()),
            position_infos: Mutex::new(HashMap::new()),
            positions: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        *self.account.lock().unwrap()
    }

    /// Get all position infos (for reqPositions).
    pub fn position_infos(&self) -> Vec<PositionInfo> {
        self.position_infos.lock().unwrap().values().copied().collect()
    }

    /// Get position info for a single conId (for pnlSingle).
    pub fn position_info(&self, con_id: i64) -> Option<PositionInfo> {
        self.position_infos.lock().unwrap().get(&con_id).copied()
    }

    /// Read current position for an instrument.
    pub fn position(&self, id: InstrumentId) -> i64 {
        self.positions[id as usize].load(Ordering::Relaxed) as i64
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn set_account(&self, account: &AccountState) {
        *self.account.lock().unwrap() = *account;
    }

    #[doc(hidden)] pub fn set_position_info(&self, info: PositionInfo) {
        self.position_infos.lock().unwrap().insert(info.con_id, info);
    }

    #[doc(hidden)] pub fn set_position(&self, id: InstrumentId, pos: i64) {
        self.positions[id as usize].store(pos as u64, Ordering::Relaxed);
    }
}

/// Shared state between hot loop and external caller.
/// Composed of domain-specific containers for clear ownership boundaries.
pub struct SharedState {
    pub market: MarketDataState,
    pub orders: OrderState,
    pub reference: ReferenceState,
    pub portfolio: PortfolioState,
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
            notify_mutex: Mutex::new(false),
            notify_condvar: Condvar::new(),
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
            filled_qty: 0, remaining_qty: 100, timestamp_ns: 0,
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
}
