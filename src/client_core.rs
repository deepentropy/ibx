//! Shared dispatch core for Rust and Python EClient implementations.
//!
//! `ClientCore` owns all subscription tracking state (reqId maps, change-detection
//! snapshots, PnL/account subscriptions) and exposes "prepare" methods that return
//! intermediate structs. Language-specific EClient adapters convert these into their
//! respective callback formats (Rust `Wrapper` trait calls or PyO3 `call_method`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;

use crossbeam_channel::Sender;

use crate::api::types::{
    Contract as ApiContract, CommissionReport as ApiCommissionReport,
    Execution as ApiExecution, ExecutionFilter,
    Order as ApiOrder,
    PRICE_SCALE_F,
};
use crate::bridge::SharedState;
use crate::types::*;

// ── Tick type constants matching ibapi ──

pub const TICK_BID: i32 = 1;
pub const TICK_ASK: i32 = 2;
pub const TICK_LAST: i32 = 4;
pub const TICK_HIGH: i32 = 6;
pub const TICK_LOW: i32 = 7;
pub const TICK_CLOSE: i32 = 9;
pub const TICK_OPEN: i32 = 14;
pub const TICK_BID_SIZE: i32 = 0;
pub const TICK_ASK_SIZE: i32 = 3;
pub const TICK_LAST_SIZE: i32 = 5;
pub const TICK_VOLUME: i32 = 8;

// ── Shared account field definitions ──

/// Account update fields: (tag_name, accessor). Used by both `update_account_value`
/// and the subscription-gated account updates dispatch.
pub const ACCOUNT_UPDATE_FIELDS: &[&str] = &[
    "NetLiquidation",
    "TotalCashValue",
    "SettledCash",
    "BuyingPower",
    "EquityWithLoanValue",
    "GrossPositionValue",
    "InitMarginReq",
    "MaintMarginReq",
    "AvailableFunds",
    "ExcessLiquidity",
    "Cushion",
    "SMA",
    "UnrealizedPnL",
    "RealizedPnL",
    "AccruedCash",
    "DailyPnL",
];

/// Extract the 16 price-scaled fields from AccountState in ACCOUNT_UPDATE_FIELDS order.
#[inline]
pub fn account_field_values(acct: &AccountState) -> [i64; 16] {
    [
        acct.net_liquidation,
        acct.total_cash_value,
        acct.settled_cash,
        acct.buying_power,
        acct.equity_with_loan,
        acct.gross_position_value,
        acct.init_margin_req,
        acct.maint_margin_req,
        acct.available_funds,
        acct.excess_liquidity,
        acct.cushion,
        acct.sma,
        acct.unrealized_pnl,
        acct.realized_pnl,
        acct.accrued_cash,
        acct.daily_pnl,
    ]
}

/// Account summary tags (numeric). Superset of update fields + extras.
pub const ACCOUNT_SUMMARY_TAGS: &[&str] = &[
    "NetLiquidation",
    "TotalCashValue",
    "SettledCash",
    "BuyingPower",
    "EquityWithLoanValue",
    "GrossPositionValue",
    "InitMarginReq",
    "MaintMarginReq",
    "AvailableFunds",
    "ExcessLiquidity",
    "Cushion",
    "DayTradesRemaining",
    "Leverage",
    "UnrealizedPnL",
    "RealizedPnL",
    "DailyPnL",
];

/// Extract account summary values in ACCOUNT_SUMMARY_TAGS order.
#[inline]
pub fn account_summary_values(acct: &AccountState) -> [f64; 16] {
    [
        acct.net_liquidation as f64 / PRICE_SCALE_F,
        acct.total_cash_value as f64 / PRICE_SCALE_F,
        acct.settled_cash as f64 / PRICE_SCALE_F,
        acct.buying_power as f64 / PRICE_SCALE_F,
        acct.equity_with_loan as f64 / PRICE_SCALE_F,
        acct.gross_position_value as f64 / PRICE_SCALE_F,
        acct.init_margin_req as f64 / PRICE_SCALE_F,
        acct.maint_margin_req as f64 / PRICE_SCALE_F,
        acct.available_funds as f64 / PRICE_SCALE_F,
        acct.excess_liquidity as f64 / PRICE_SCALE_F,
        acct.cushion as f64 / PRICE_SCALE_F,
        acct.day_trades_remaining as f64,
        acct.leverage as f64 / PRICE_SCALE_F,
        acct.unrealized_pnl as f64 / PRICE_SCALE_F,
        acct.realized_pnl as f64 / PRICE_SCALE_F,
        acct.daily_pnl as f64 / PRICE_SCALE_F,
    ]
}

// ── Intermediate dispatch structs ──

/// A single tick event produced by quote change detection.
pub struct TickEvent {
    pub req_id: i64,
    pub tick_type: i32,
    pub value: f64,
    /// true = tick_price, false = tick_size
    pub is_price: bool,
}

/// Timestamp tick from quote polling.
pub struct TimestampTick {
    pub req_id: i64,
    pub timestamp_ns: i64,
}

/// Result of polling quotes for one instrument.
pub struct QuotePollResult {
    pub ticks: Vec<TickEvent>,
    pub timestamp: Option<TimestampTick>,
    /// true if any tick was delivered (for snapshot detection).
    pub delivered: bool,
}

/// PnL update (account-level).
pub struct PnlUpdate {
    pub req_id: i64,
    pub daily_pnl: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

/// PnL single update (per-position).
pub struct PnlSingleUpdate {
    pub req_id: i64,
    pub pos: f64,
    pub daily_pnl: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
    pub value: f64,
}

/// A single changed account field.
pub struct AccountFieldUpdate {
    pub key: &'static str,
    pub value: String,
    pub currency: &'static str,
}

/// Batch of account update results.
pub struct AccountUpdateBatch {
    pub fields: Vec<AccountFieldUpdate>,
    /// Whether any field was delivered (triggers account_download_end).
    pub delivered: bool,
}

/// Prepared account summary response.
pub struct AccountSummaryBatch {
    pub req_id: i64,
    pub entries: Vec<AccountSummaryEntry>,
}

pub struct AccountSummaryEntry {
    pub tag: &'static str,
    pub value: String,
    pub currency: &'static str,
}

/// Convert OrderStatus enum to ibapi-compatible string.
#[inline]
pub fn order_status_str(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::PendingSubmit => "PendingSubmit",
        OrderStatus::Submitted => "Submitted",
        OrderStatus::Filled => "Filled",
        OrderStatus::PartiallyFilled => "PreSubmitted",
        OrderStatus::Cancelled => "Cancelled",
        OrderStatus::Rejected => "Inactive",
        OrderStatus::Uncertain => "Unknown",
    }
}

// ── Execution storage ──

/// A stored execution + commission pair for `req_executions` replay.
/// Shared between Rust and Python adapters via `ClientCore`.
pub struct StoredExecution {
    pub req_id: i64,
    pub contract: ApiContract,
    pub execution: ApiExecution,
    pub commission: ApiCommissionReport,
}

// ── Order tracking ──

/// A locally tracked order for `req_open_orders` / dispatch status updates.
pub struct TrackedOrder {
    pub contract: ApiContract,
    pub order: ApiOrder,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub instrument: InstrumentId,
}

// ── ClientCore ──

/// Shared subscription tracking and dispatch preparation logic.
///
/// Both Rust and Python EClient own a `ClientCore` and delegate state tracking
/// and data preparation to it. Only the final callback invocation is language-specific.
pub struct ClientCore {
    // reqId <-> InstrumentId mapping
    pub req_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    pub instrument_to_req: Mutex<HashMap<InstrumentId, i64>>,
    // con_id → InstrumentId for find_or_register_instrument lookup
    pub con_id_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    // Change detection for quote polling
    pub last_quotes: Mutex<HashMap<InstrumentId, [i64; 12]>>,
    // Snapshot req_ids — deliver first ticks then auto-cancel
    pub snapshot_reqs: Mutex<HashSet<i64>>,

    // PnL subscription state
    pub pnl_req_id: Mutex<Option<i64>>,
    pub pnl_single_reqs: Mutex<HashMap<i64, i64>>, // req_id → con_id
    pub last_pnl: Mutex<[i64; 3]>, // [daily, unrealized, realized]

    // Account summary subscription state (req_id, tags)
    pub account_summary_req: Mutex<Option<(i64, Vec<String>)>>,

    // News bulletin subscription
    pub bulletin_subscribed: AtomicBool,

    // Account updates subscription
    pub account_updates_subscribed: AtomicBool,
    pub last_account: Mutex<Option<AccountState>>,

    // Execution replay store
    pub executions: Mutex<Vec<StoredExecution>>,

    // Open order tracking
    pub open_orders: Mutex<HashMap<u64, TrackedOrder>>,

    // Market data type callback tracking
    pub market_data_type: AtomicI32,
    pub mdt_sent: Mutex<HashSet<i64>>,

    // News subscription state
    pub news_providers: Mutex<String>,
    pub news_instruments: Mutex<HashSet<InstrumentId>>,

    // Contract cache for enrichment
    pub contract_cache: Mutex<HashMap<i64, ApiContract>>,
}

impl ClientCore {
    pub fn new() -> Self {
        Self {
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            con_id_to_instrument: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
            snapshot_reqs: Mutex::new(HashSet::new()),
            pnl_req_id: Mutex::new(None),
            pnl_single_reqs: Mutex::new(HashMap::new()),
            last_pnl: Mutex::new([0; 3]),
            account_summary_req: Mutex::new(None),
            bulletin_subscribed: AtomicBool::new(false),
            account_updates_subscribed: AtomicBool::new(false),
            last_account: Mutex::new(None),
            executions: Mutex::new(Vec::new()),
            open_orders: Mutex::new(HashMap::new()),
            market_data_type: AtomicI32::new(1),
            mdt_sent: Mutex::new(HashSet::new()),
            news_providers: Mutex::new("BRFG*BRFUPDN".into()),
            news_instruments: Mutex::new(HashSet::new()),
            contract_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Clear all per-session state so the owning client can reconnect.
    pub fn reset(&self) {
        self.req_to_instrument.lock().unwrap().clear();
        self.instrument_to_req.lock().unwrap().clear();
        self.con_id_to_instrument.lock().unwrap().clear();
        self.last_quotes.lock().unwrap().clear();
        self.snapshot_reqs.lock().unwrap().clear();
        *self.pnl_req_id.lock().unwrap() = None;
        self.pnl_single_reqs.lock().unwrap().clear();
        *self.last_pnl.lock().unwrap() = [0; 3];
        *self.account_summary_req.lock().unwrap() = None;
        self.bulletin_subscribed.store(false, Ordering::Relaxed);
        self.account_updates_subscribed.store(false, Ordering::Relaxed);
        *self.last_account.lock().unwrap() = None;
        self.executions.lock().unwrap().clear();
        self.open_orders.lock().unwrap().clear();
        self.market_data_type.store(1, Ordering::Relaxed);
        self.mdt_sent.lock().unwrap().clear();
        *self.news_providers.lock().unwrap() = "BRFG*BRFUPDN".into();
        self.news_instruments.lock().unwrap().clear();
        self.contract_cache.lock().unwrap().clear();
    }

    // ── Registration helpers ──

    /// Registration reply timeout.
    #[cfg(not(test))]
    const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    #[cfg(test)]
    const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1);

    /// Wait for the hot loop to process a registration command and return the assigned ID.
    fn recv_registration(reply_rx: crossbeam_channel::Receiver<InstrumentId>) -> Result<InstrumentId, String> {
        reply_rx.recv_timeout(Self::REGISTRATION_TIMEOUT)
            .map_err(|_| "Registration timed out".to_string())
    }

    /// Find instrument ID for a contract, registering if needed.
    /// Returns `Err` if the control channel is closed.
    pub fn find_or_register_instrument(
        &self,
        control_tx: &Sender<ControlCommand>,
        con_id: i64,
        symbol: &str,
        _exchange: &str,
        _sec_type: &str,
    ) -> Result<InstrumentId, String> {
        // Check if already mapped by con_id
        {
            let map = self.con_id_to_instrument.lock().unwrap();
            if let Some(&iid) = map.get(&con_id) {
                return Ok(iid);
            }
        }

        // Register new — only allocates an InstrumentId slot, does not subscribe to market data.
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument { con_id, symbol: symbol.to_string(), reply_tx: Some(reply_tx) })
            .map_err(|e| format!("Engine stopped: {}", e))?;

        let id = Self::recv_registration(reply_rx)?;
        self.con_id_to_instrument.lock().unwrap().insert(con_id, id);
        Ok(id)
    }

    // ── Subscription management ──

    /// Register a market data subscription mapping.
    /// If `generic_tick_list` contains "292", also subscribes to per-contract news.
    pub fn register_mkt_data(
        &self,
        _shared: &SharedState,
        control_tx: &Sender<ControlCommand>,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        snapshot: bool,
        generic_tick_list: &str,
    ) -> Result<InstrumentId, String> {
        // News subscription if generic_tick_list contains 292
        let wants_news = generic_tick_list.split(',')
            .any(|t| t.trim() == "292" || t.trim() == "mdoff,292" || t.trim().ends_with("292"));
        if wants_news {
            let providers = self.news_providers.lock().unwrap().clone();
            let _ = control_tx.send(ControlCommand::SubscribeNews {
                con_id,
                symbol: symbol.to_string(),
                providers,
                reply_tx: None,
            });
        }

        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument { con_id, symbol: symbol.to_string(), reply_tx: None })
            .map_err(|e| format!("Engine stopped: {}", e))?;
        control_tx.send(ControlCommand::Subscribe {
            con_id,
            symbol: symbol.to_string(),
            exchange: exchange.to_string(),
            sec_type: sec_type.to_string(),
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let instrument_id = Self::recv_registration(reply_rx)?;
        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);
        if snapshot {
            self.snapshot_reqs.lock().unwrap().insert(req_id);
        }
        if wants_news {
            self.news_instruments.lock().unwrap().insert(instrument_id);
        }
        Ok(instrument_id)
    }

    /// Unregister a market data subscription.
    /// Returns `(instrument_id, needs_news_unsub)`.
    pub fn unregister_mkt_data(&self, req_id: i64) -> (Option<InstrumentId>, bool) {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            self.last_quotes.lock().unwrap().remove(&instrument);
            self.mdt_sent.lock().unwrap().remove(&req_id);
            let needs_news = self.news_instruments.lock().unwrap().remove(&instrument);
            (Some(instrument), needs_news)
        } else {
            (None, false)
        }
    }

    pub fn set_news_providers(&self, providers: &str) {
        *self.news_providers.lock().unwrap() = providers.to_string();
    }

    // ── Contract cache ──

    /// Cache a contract for later enrichment.
    pub fn cache_contract(&self, con_id: i64, contract: ApiContract) {
        self.contract_cache.lock().unwrap().insert(con_id, contract);
    }

    /// Look up a contract: merge local cache with shared reference for richest data.
    pub fn get_contract(&self, con_id: i64, shared: &SharedState) -> Option<ApiContract> {
        let local = self.contract_cache.lock().unwrap().get(&con_id).cloned();
        let shared_ref = shared.reference.get_contract(con_id);
        match (local, shared_ref) {
            (Some(mut l), Some(s)) => {
                // Enrich local with shared reference fields (secdef has richer data)
                if l.local_symbol.is_empty() { l.local_symbol = s.local_symbol; }
                if l.trading_class.is_empty() { l.trading_class = s.trading_class; }
                if l.primary_exchange.is_empty() { l.primary_exchange = s.primary_exchange; }
                Some(l)
            }
            (Some(l), None) => Some(l),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
    }

    /// Register a TBT subscription mapping.
    pub fn register_tbt(
        &self,
        _shared: &SharedState,
        control_tx: &Sender<ControlCommand>,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        tbt_type: TbtType,
    ) -> Result<InstrumentId, String> {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::SubscribeTbt {
            con_id,
            symbol: symbol.to_string(),
            tbt_type,
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let instrument_id = Self::recv_registration(reply_rx)?;
        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);
        Ok(instrument_id)
    }

    /// Look up req_id for an instrument.
    pub fn req_id_for_instrument(&self, instrument: InstrumentId) -> i64 {
        self.instrument_to_req.lock().unwrap()
            .get(&instrument).copied().unwrap_or(-1)
    }

    // ── PnL subscription management ──

    pub fn subscribe_pnl(&self, req_id: i64) {
        *self.pnl_req_id.lock().unwrap() = Some(req_id);
    }

    pub fn unsubscribe_pnl(&self, req_id: i64) {
        let mut pnl = self.pnl_req_id.lock().unwrap();
        if *pnl == Some(req_id) {
            *pnl = None;
        }
    }

    pub fn subscribe_pnl_single(&self, req_id: i64, con_id: i64) {
        self.pnl_single_reqs.lock().unwrap().insert(req_id, con_id);
    }

    pub fn unsubscribe_pnl_single(&self, req_id: i64) {
        self.pnl_single_reqs.lock().unwrap().remove(&req_id);
    }

    // ── Account summary subscription management ──

    pub fn subscribe_account_summary(&self, req_id: i64, tags: &str) {
        let tag_list: Vec<String> = tags.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        *self.account_summary_req.lock().unwrap() = Some((req_id, tag_list));
    }

    pub fn unsubscribe_account_summary(&self, req_id: i64) {
        let mut req = self.account_summary_req.lock().unwrap();
        if req.as_ref().map(|(r, _)| *r) == Some(req_id) {
            *req = None;
        }
    }

    // ── Account updates subscription management ──

    pub fn subscribe_account_updates(&self, subscribe: bool) {
        self.account_updates_subscribed.store(subscribe, Ordering::Release);
        if !subscribe {
            *self.last_account.lock().unwrap() = None;
        }
    }

    // ── Market data type tracking ──

    pub fn set_market_data_type(&self, mdt: i32) {
        self.market_data_type.store(mdt, Ordering::Relaxed);
    }

    /// Check if the `market_data_type` callback should fire for this req_id.
    /// Returns `Some(mdt)` on the first call per req_id that has data, `None` thereafter.
    pub fn check_mdt_needed(&self, req_id: i64, has_data: bool) -> Option<i32> {
        if has_data && self.mdt_sent.lock().unwrap().insert(req_id) {
            Some(self.market_data_type.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    // ── Bulletin subscription management ──

    pub fn subscribe_bulletins(&self) {
        self.bulletin_subscribed.store(true, Ordering::Release);
    }

    pub fn unsubscribe_bulletins(&self) {
        self.bulletin_subscribed.store(false, Ordering::Release);
    }

    pub fn bulletins_subscribed(&self) -> bool {
        self.bulletin_subscribed.load(Ordering::Acquire)
    }

    // ── Execution replay store ──

    /// Store an execution for later replay via `req_executions`.
    pub fn push_execution(&self, req_id: i64, contract: ApiContract, execution: ApiExecution, commission: ApiCommissionReport) {
        self.executions.lock().unwrap().push(StoredExecution {
            req_id, contract, execution, commission,
        });
    }

    /// Return executions matching the given filter.
    pub fn filter_executions(&self, filter: &ExecutionFilter) -> Vec<usize> {
        let execs = self.executions.lock().unwrap();
        execs.iter().enumerate().filter_map(|(i, se)| {
            if !filter.symbol.is_empty() && !se.contract.symbol.eq_ignore_ascii_case(&filter.symbol) {
                return None;
            }
            if !filter.sec_type.is_empty() && !se.contract.sec_type.eq_ignore_ascii_case(&filter.sec_type) {
                return None;
            }
            if !filter.exchange.is_empty() && !se.execution.exchange.eq_ignore_ascii_case(&filter.exchange) {
                return None;
            }
            if !filter.side.is_empty() && !se.execution.side.eq_ignore_ascii_case(&filter.side) {
                return None;
            }
            if !filter.acct_code.is_empty() && !se.execution.acct_number.eq_ignore_ascii_case(&filter.acct_code) {
                return None;
            }
            if filter.client_id != 0 && se.execution.client_id != filter.client_id {
                return None;
            }
            Some(i)
        }).collect()
    }

    // ── Open order tracking ──

    /// Check if an order with this ID is currently tracked (for modify detection).
    pub fn is_order_tracked(&self, order_id: u64) -> bool {
        self.open_orders.lock().unwrap().contains_key(&order_id)
    }

    /// Track a newly placed order.
    pub fn track_order(&self, order_id: u64, contract: ApiContract, order: ApiOrder, instrument: InstrumentId) {
        let remaining = order.total_quantity;
        self.open_orders.lock().unwrap().insert(order_id, TrackedOrder {
            contract, order, status: "PendingSubmit".into(), filled: 0.0, remaining, instrument,
        });
    }

    /// Update a tracked order after a fill. Removes the order if fully filled.
    pub fn update_order_fill(&self, order_id: u64, status: &str, filled: f64, remaining: f64) {
        let mut orders = self.open_orders.lock().unwrap();
        if remaining == 0.0 {
            orders.remove(&order_id);
        } else if let Some(o) = orders.get_mut(&order_id) {
            o.status = status.into();
            o.filled = filled;
            o.remaining = remaining;
        }
    }

    /// Update a tracked order status from an order update event.
    pub fn update_order_status(&self, order_id: u64, status: &str, filled: f64, remaining: f64) {
        let mut orders = self.open_orders.lock().unwrap();
        if let Some(o) = orders.get_mut(&order_id) {
            o.status = status.into();
            o.filled = filled;
            o.remaining = remaining;
        }
    }

    /// Collect open orders: merge local tracking with shared state.
    /// Returns (order_id, contract, order, status, filled, remaining) for non-terminal orders.
    pub fn collect_open_orders(&self, shared: &SharedState) -> Vec<(u64, TrackedOrder)> {
        let mut result: Vec<(u64, TrackedOrder)> = Vec::new();

        // Drain shared order cache first to enrich local tracking
        let shared_orders = shared.orders.drain_open_orders();
        {
            let mut orders = self.open_orders.lock().unwrap();
            for (oid, info) in &shared_orders {
                if let Some(o) = orders.get_mut(oid) {
                    if o.order.account.is_empty() {
                        o.order.account = info.order.account.clone();
                    }
                    if o.order.perm_id == 0 {
                        o.order.perm_id = info.order.perm_id;
                    }
                }
            }
        }

        // Local tracked orders (non-terminal), enriched from secdef cache
        {
            let orders = self.open_orders.lock().unwrap();
            for (&oid, o) in orders.iter() {
                if !matches!(o.status.as_str(), "Filled" | "Cancelled" | "Inactive") {
                    let contract = if o.contract.con_id != 0 {
                        self.get_contract(o.contract.con_id, shared).unwrap_or_else(|| o.contract.clone())
                    } else {
                        o.contract.clone()
                    };
                    result.push((oid, TrackedOrder {
                        contract,
                        order: o.order.clone(),
                        status: o.status.clone(),
                        filled: o.filled,
                        remaining: o.remaining,
                        instrument: o.instrument,
                    }));
                }
            }
        }

        // Add shared-only entries not already present from local
        for (oid, info) in shared_orders {
            if matches!(info.order_state.status.as_str(), "Filled" | "Cancelled" | "Inactive") {
                continue;
            }
            if !result.iter().any(|(id, _)| *id == oid) {
                let contract = if info.contract.con_id != 0 {
                    shared.reference.get_contract(info.contract.con_id).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                result.push((oid, TrackedOrder {
                    contract,
                    order: info.order,
                    status: info.order_state.status.clone(),
                    filled: 0.0,
                    remaining: 0.0,
                    instrument: 0,
                }));
            }
        }

        result
    }

    // ── Dispatch preparation methods ──

    /// Poll quotes for a single instrument and return tick events.
    /// Updates last_quotes internally.
    pub fn poll_instrument_ticks(
        &self,
        shared: &SharedState,
        iid: InstrumentId,
        req_id: i64,
    ) -> QuotePollResult {
        let q = shared.market.quote(iid);
        let fields = [
            q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
            q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
        ];

        // Single lock acquisition for both read and write of last_quotes.
        let mut map = self.last_quotes.lock().unwrap();
        let last = map.get(&iid).copied().unwrap_or([0i64; 12]);

        let mut ticks = Vec::new();
        let mut delivered = false;

        // Price ticks: (field_index, tick_type)
        const PRICE_TICKS: &[(usize, i32)] = &[
            (0, TICK_BID), (1, TICK_ASK), (2, TICK_LAST),
            (6, TICK_HIGH), (7, TICK_LOW), (9, TICK_CLOSE), (10, TICK_OPEN),
        ];
        for &(idx, tt) in PRICE_TICKS {
            if fields[idx] != last[idx] {
                ticks.push(TickEvent {
                    req_id, tick_type: tt,
                    value: fields[idx] as f64 / PRICE_SCALE_F,
                    is_price: true,
                });
                delivered = true;
            }
        }

        // Size ticks: (field_index, tick_type)
        const SIZE_TICKS: &[(usize, i32)] = &[
            (3, TICK_BID_SIZE), (4, TICK_ASK_SIZE), (5, TICK_LAST_SIZE), (8, TICK_VOLUME),
        ];
        for &(idx, tt) in SIZE_TICKS {
            if fields[idx] != last[idx] {
                ticks.push(TickEvent {
                    req_id, tick_type: tt,
                    value: fields[idx] as f64 / QTY_SCALE as f64,
                    is_price: false,
                });
                delivered = true;
            }
        }

        // Timestamp tick
        let timestamp = if fields[11] != last[11] && fields[11] != 0 {
            Some(TimestampTick { req_id, timestamp_ns: fields[11] })
        } else {
            None
        };

        map.insert(iid, fields);

        QuotePollResult { ticks, timestamp, delivered }
    }

    /// Check and consume snapshot completion for a req_id.
    /// Returns true if this was a snapshot that just completed.
    pub fn check_snapshot_done(&self, req_id: i64, delivered: bool) -> bool {
        delivered && self.snapshot_reqs.lock().unwrap().remove(&req_id)
    }

    /// Snapshot the current instrument→req_id mapping.
    pub fn snapshot_instruments(&self) -> Vec<(InstrumentId, i64)> {
        let map = self.instrument_to_req.lock().unwrap();
        map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
    }

    /// Poll PnL and return update if values changed.
    pub fn poll_pnl(&self, shared: &SharedState) -> Option<PnlUpdate> {
        let req_id = *self.pnl_req_id.lock().unwrap();
        let req_id = req_id?;

        let acct = shared.portfolio.account();
        let pnl = [acct.daily_pnl, acct.unrealized_pnl, acct.realized_pnl];
        // Single lock acquisition for both read and write.
        let mut last = self.last_pnl.lock().unwrap();
        if pnl == *last {
            return None;
        }
        *last = pnl;
        Some(PnlUpdate {
            req_id,
            daily_pnl: acct.daily_pnl as f64 / PRICE_SCALE_F,
            unrealized_pnl: acct.unrealized_pnl as f64 / PRICE_SCALE_F,
            realized_pnl: acct.realized_pnl as f64 / PRICE_SCALE_F,
        })
    }

    /// Poll per-position PnL and return updates.
    pub fn poll_pnl_single(&self, shared: &SharedState) -> Vec<PnlSingleUpdate> {
        let reqs: Vec<(i64, i64)> = self.pnl_single_reqs.lock().unwrap()
            .iter().map(|(&r, &c)| (r, c)).collect();

        let mut results = Vec::new();
        for (req_id, con_id) in reqs {
            if let Some(pi) = shared.portfolio.position_info(con_id) {
                let pos = pi.position as f64;
                let last_price = {
                    let imap = self.instrument_to_req.lock().unwrap();
                    imap.keys()
                        .find_map(|&iid| {
                            let q = shared.market.quote(iid);
                            if q.last != 0 { Some(q.last) } else { None }
                        })
                        .unwrap_or(0)
                };
                let (unrealized, value) = if last_price != 0 && pi.avg_cost != 0 {
                    let u = (last_price - pi.avg_cost) * pi.position;
                    let v = last_price * pi.position;
                    (u as f64 / PRICE_SCALE_F, v as f64 / PRICE_SCALE_F)
                } else {
                    (0.0, pi.avg_cost as f64 / PRICE_SCALE_F * pos)
                };
                results.push(PnlSingleUpdate {
                    req_id, pos, daily_pnl: 0.0, unrealized_pnl: unrealized,
                    realized_pnl: 0.0, value,
                });
            }
        }
        results
    }

    /// Prepare account update fields (subscription-gated, change-detected).
    /// Updates internal last_account state.
    pub fn prepare_account_updates(&self, shared: &SharedState) -> Option<AccountUpdateBatch> {
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return None;
        }

        let acct = shared.portfolio.account();
        let mut prev_guard = self.last_account.lock().unwrap();
        let is_first = prev_guard.is_none();
        let prev = prev_guard.unwrap_or_default();

        let cur_vals = account_field_values(&acct);
        let prev_vals = account_field_values(&prev);

        let mut fields = Vec::new();
        let mut delivered = false;

        for (i, &key) in ACCOUNT_UPDATE_FIELDS.iter().enumerate() {
            if is_first || cur_vals[i] != prev_vals[i] {
                fields.push(AccountFieldUpdate {
                    key,
                    value: format!("{:.2}", cur_vals[i] as f64 / PRICE_SCALE_F),
                    currency: "USD",
                });
                delivered = true;
            }
        }

        // Integer fields
        if is_first || acct.day_trades_remaining != prev.day_trades_remaining {
            fields.push(AccountFieldUpdate {
                key: "DayTradesRemaining",
                value: acct.day_trades_remaining.to_string(),
                currency: "",
            });
            delivered = true;
        }
        if is_first || acct.leverage != prev.leverage {
            fields.push(AccountFieldUpdate {
                key: "Leverage-S",
                value: format!("{:.4}", acct.leverage as f64 / PRICE_SCALE_F),
                currency: "",
            });
            delivered = true;
        }

        *prev_guard = Some(acct);

        Some(AccountUpdateBatch { fields, delivered })
    }

    /// Prepare account summary response (one-shot, consumes the request).
    pub fn prepare_account_summary(&self, shared: &SharedState, _account_id: &str) -> Option<AccountSummaryBatch> {
        let req = self.account_summary_req.lock().unwrap().take();
        let (req_id, tags) = req?;

        let acct = shared.portfolio.account();
        let values = account_summary_values(&acct);

        let mut entries = Vec::new();
        for (i, &tag) in ACCOUNT_SUMMARY_TAGS.iter().enumerate() {
            if !tags.is_empty() && !tags.iter().any(|t| t == tag) {
                continue;
            }
            entries.push(AccountSummaryEntry {
                tag,
                value: format!("{:.2}", values[i]),
                currency: "USD",
            });
        }

        Some(AccountSummaryBatch { req_id, entries })
    }

    // ── Order routing ──

    /// Pre-validate order fields that don't depend on instrument ID.
    /// Call this before `find_or_register_instrument` to fail fast.
    pub fn validate_order(order: &ApiOrder) -> Result<(), String> {
        order.side()?;
        let order_type = order.order_type.to_uppercase();
        if order.algo_strategy.eq_ignore_ascii_case("Adaptive") {
            return Ok(());
        }
        if !order.algo_strategy.is_empty() {
            crate::api::client::parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            return Ok(());
        }
        if order.what_if {
            return Ok(());
        }
        match order_type.as_str() {
            "MKT" | "LMT" | "STP" | "STP LMT" | "TRAIL" | "TRAIL LIMIT"
            | "MOC" | "LOC" | "MIT" | "LIT" | "MTL" | "MKT PRT" | "STP PRT"
            | "REL" | "PEG MKT" | "PEG MID" | "PEG MIDPT" | "MIDPX" | "MIDPRICE"
            | "SNAP MKT" | "SNAP MID" | "SNAP MIDPT" | "SNAP PRI" | "SNAP PRIM"
            | "BOX TOP" => Ok(()),
            _ => Err(format!("Unsupported order type: '{}'", order.order_type)),
        }
    }

    /// Build an `OrderRequest` from an API `Order`, handling all order types.
    /// This is the shared order-type match block used by both Rust and Python.
    pub fn build_order_request(
        order: &ApiOrder,
        order_id: u64,
        instrument: InstrumentId,
    ) -> Result<ControlCommand, String> {
        let side = order.side()?;
        let qty = order.total_quantity as u32;
        let order_type = order.order_type.to_uppercase();

        // Adaptive orders (special-cased before generic algo)
        if order.algo_strategy.eq_ignore_ascii_case("Adaptive") {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            let priority_str = order.algo_params.iter()
                .find(|tv| tv.tag == "adaptivePriority")
                .map(|tv| tv.value.as_str())
                .unwrap_or("Normal");
            let priority = match priority_str {
                "Patient" => AdaptivePriority::Patient,
                "Urgent" => AdaptivePriority::Urgent,
                _ => AdaptivePriority::Normal,
            };
            return Ok(ControlCommand::Order(OrderRequest::SubmitAdaptive {
                order_id, instrument, side, qty, price, priority,
            }));
        }

        // Algo orders
        if !order.algo_strategy.is_empty() {
            let algo = crate::api::client::parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            return Ok(ControlCommand::Order(OrderRequest::SubmitAlgo {
                order_id, instrument, side, qty, price, algo,
            }));
        }

        // What-if orders
        if order.what_if {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            return Ok(ControlCommand::Order(OrderRequest::SubmitWhatIf {
                order_id, instrument, side, qty, price,
            }));
        }

        let req = match order_type.as_str() {
            "MKT" => OrderRequest::SubmitMarket { order_id, instrument, side, qty },
            "LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                if order.has_extended_attrs() || order.tif != "DAY" {
                    OrderRequest::SubmitLimitEx {
                        order_id, instrument, side, qty, price,
                        tif: order.tif_byte(),
                        attrs: order.attrs(),
                    }
                } else {
                    OrderRequest::SubmitLimit { order_id, instrument, side, qty, price }
                }
            }
            "STP" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price: stop }
            }
            "STP LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price: stop }
            }
            "TRAIL" => {
                if order.trailing_percent > 0.0 {
                    let pct = (order.trailing_percent * 100.0) as u32;
                    OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct: pct }
                } else {
                    let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                    OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt: trail }
                }
            }
            "TRAIL LIMIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, price, trail_amt: trail }
            }
            "MOC" => OrderRequest::SubmitMoc { order_id, instrument, side, qty },
            "LOC" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLoc { order_id, instrument, side, qty, price }
            }
            "MIT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price: stop }
            }
            "LIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price: stop }
            }
            "MTL" => OrderRequest::SubmitMtl { order_id, instrument, side, qty },
            "MKT PRT" => OrderRequest::SubmitMktPrt { order_id, instrument, side, qty },
            "STP PRT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price: stop }
            }
            "REL" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitRel { order_id, instrument, side, qty, offset }
            }
            "PEG MKT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, offset }
            }
            "PEG MID" | "PEG MIDPT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMid { order_id, instrument, side, qty, offset }
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap: cap }
            }
            "SNAP MKT" => OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty },
            "SNAP MID" | "SNAP MIDPT" => OrderRequest::SubmitSnapMid { order_id, instrument, side, qty },
            "SNAP PRI" | "SNAP PRIM" => OrderRequest::SubmitSnapPri { order_id, instrument, side, qty },
            "BOX TOP" => OrderRequest::SubmitMtl { order_id, instrument, side, qty },
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        };

        Ok(ControlCommand::Order(req))
    }
}
