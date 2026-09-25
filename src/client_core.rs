//! Shared dispatch core for Rust and Python EClient implementations.
//!
//! `ClientCore` owns all subscription tracking state (reqId maps, change-detection
//! snapshots, PnL/account subscriptions) and exposes "prepare" methods that return
//! intermediate structs. Language-specific EClient adapters convert these into their
//! respective callback formats (Rust `Wrapper` trait calls or PyO3 `call_method`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use crossbeam_channel::Sender;

use crate::api::types::{
    Contract as ApiContract, CommissionAndFeesReport as ApiCommissionAndFeesReport,
    Execution as ApiExecution, ExecutionFilter,
    Order as ApiOrder, OrderState as ApiOrderState,
    PRICE_SCALE_F, QTY_SCALE_F,
};
use crate::bridge::SharedState;
use crate::types::*;

/// The only market data type the engine delivers (1 = realtime, ibx#234).
const MDT_REALTIME: i32 = 1;

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
pub const TICK_LAST_TIMESTAMP: i32 = 45;
pub const TICK_BID_EXCHANGE: i32 = 32;
pub const TICK_ASK_EXCHANGE: i32 = 33;
pub const TICK_LAST_EXCHANGE: i32 = 84;

// ── Shared account field definitions ──



/// Render an exchange-code bitmask to a letter string using the smart components
/// table. Each set bit at position N picks `smart_components[N].exchange_letter`.
///
/// Wire encoding pending live confirmation (deepentropy/ib-agent#120). Bit
/// ordering and width are inferred from TWS-API parity expectations; the
/// dispatch path tolerates an empty result if the mask layout differs.
pub fn render_exchange_mask(mask: i64, shared: &SharedState) -> String {
    if mask == 0 {
        return String::new();
    }
    let components = shared.reference.smart_components();
    let mut out = String::with_capacity(8);
    let mut bits = mask as u64;
    while bits != 0 {
        let bit = bits.trailing_zeros() as i32;
        bits &= bits - 1;
        if let Some(c) = components.iter().find(|c| c.bit_number == bit) {
            out.push_str(&c.exchange_letter);
        }
    }
    out
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

/// String-valued tick (e.g. exchange-code letters for tick_types 32/33/84).
pub struct StringTickEvent {
    pub req_id: i64,
    pub tick_type: i32,
    pub value: String,
}

/// Result of polling quotes for one instrument.
pub struct QuotePollResult {
    pub ticks: Vec<TickEvent>,
    pub string_ticks: Vec<StringTickEvent>,
    pub timestamp: Option<TimestampTick>,
    /// true if any tick was delivered (for snapshot detection).
    pub delivered: bool,
}

/// Net cash traded today on a position, for the daily P&L.
/// moneyTradedSinceMidnight (wire 6822) is signed net cash: SELL positive,
/// BUY negative (ib-agent#163). The seed's value, plus the cash of this
/// session's fills after it (a fill moved the daily P&L by the whole trade
/// value: +770 for 1 SPY bought, seen on paper 25/09/2026). With no seed row:
/// the cash of this session's fills when there are some, else the opening
/// trade's cash synthesized from the average cost (-qty * avgCost).
fn money_traded_today(seed: Option<&MidnightSeed>, since_seed: Option<f64>, qty_now: f64, avg_cost: Price) -> f64 {
    match (seed, since_seed) {
        (Some(s), since) => s.money_traded + since.unwrap_or(0.0),
        (None, Some(since)) => since,
        (None, None) => -(qty_now * avg_cost as f64 / PRICE_SCALE_F),
    }
}

/// Quotes ibx subscribes to by itself while a P&L request runs: the P&L
/// needs the last price and the previous close of each position, and a
/// client that did not subscribe had none (daily P&L unset). They never
/// reach the tick callbacks.
#[derive(Default)]
pub struct PnlQuotes {
    /// conId → instrument of a running internal subscription.
    active: HashMap<i64, InstrumentId>,
    /// conId → registration reply not received yet.
    pending: HashMap<i64, crossbeam_channel::Receiver<Result<InstrumentId, String>>>,
    /// Inputs of the last check: position generation, P&L requests, seeds.
    key: (u64, usize, usize, usize),
    checked_at: Option<std::time::Instant>,
}

impl PnlQuotes {
    /// Time of the last check (tests move it back to force a check).
    #[doc(hidden)]
    pub fn checked_at_mut(&mut self) -> Option<&mut std::time::Instant> {
        self.checked_at.as_mut()
    }
}

/// How often the internal P&L quotes are checked when nothing else changed.
const PNL_QUOTES_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// Last values of a P&L request before its first callback.
const PNL_NOT_SENT: i64 = i64::MIN;

/// The reference's account checks of a P&L request (ibx#478): 321 for an
/// empty account or one this session is not logged in to.
fn pnl_account_refusal(class: &str, account: &str, own_account: &str) -> Result<(), (i64, String)> {
    let cause = if account.is_empty() {
        "Account must not be empty"
    } else if account != own_account {
        "Invalid account code"
    } else {
        return Ok(());
    };
    Err((321, format!("Error validating request.-'{}' : cause - {}", class, cause)))
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
    pub key: String,
    pub value: String,
    pub currency: String,
}

/// Batch of account update results (ibx#475).
pub struct AccountUpdateBatch {
    /// Account values to send: every value for the first image, then only
    /// the ones that changed.
    pub fields: Vec<AccountFieldUpdate>,
    /// The latest row time as `HH:mm`, for update_account_time.
    pub time: String,
    /// This batch is the first image: account_download_end follows it,
    /// once per subscription.
    pub download_end: bool,
}

/// This client's account stream (ibx#475).
#[derive(Default)]
pub struct AccountStream {
    /// Subscribed, and the first image not sent yet.
    image_pending: bool,
    /// Last value sent per (key, currency).
    sent: HashMap<(String, String), String>,
    /// Row store generation last read.
    generation: u64,
}

/// The reference's answer to an unsubscribe from account updates (ibx#475).
pub const ACCOUNT_UNSUBSCRIBED: (i64, &str) = (2100, "API client has been unsubscribed from account data.");

/// A row time as update_account_time carries it: `HH:mm`, in US/Eastern
/// like the execution times (UTC when the zone database has none). Empty
/// before any row time.
pub fn format_account_time(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return String::new();
    }
    let Ok(ts) = jiff::Timestamp::from_second(unix_secs) else {
        return String::new();
    };
    let tz = jiff::tz::TimeZone::get("US/Eastern").unwrap_or(jiff::tz::TimeZone::UTC);
    ts.to_zoned(tz).strftime("%H:%M").to_string()
}

/// Account summary rows to send for one request (ibx#479).
pub struct AccountSummaryBatch {
    pub req_id: i64,
    pub rows: Vec<crate::bridge::AccountRow>,
    /// The server's batch ended: account_summary_end follows the rows.
    pub end: bool,
}

/// Which ledger rows a summary request asked for (ibx#479). `$LEDGER`,
/// `$LEDGER:{CCY}` and `$LEDGER:ALL` are one `$LEDGER` item on the wire;
/// the currency choice stays in the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerChoice {
    None,
    /// `$LEDGER`: the base currency.
    Base,
    /// `$LEDGER:{CCY}`.
    Currency(String),
    /// `$LEDGER:ALL`.
    All,
}

/// A running account summary request (ibx#479).
#[derive(Clone, Debug)]
pub struct AccountSummaryRequest {
    pub req_id: i64,
    /// Subscription id the server echoes on the rows.
    pub sr_id: String,
    pub ledger: LedgerChoice,
}

/// What `req_account_summary` sends: the subscription, and the one it
/// replaces when the request id was already running.
pub struct AccountSummaryPlan {
    pub cancel_sr_id: Option<String>,
    pub sr_id: String,
    pub wire_tags: String,
    pub group: String,
}

/// A running req_positions (ibx#477).
pub struct PositionsSubscription {
    requested_at: std::time::Instant,
    snapshot_sent: bool,
    /// Position and average cost last sent, by conId.
    sent: HashMap<i64, (Qty, Price)>,
    generation: u64,
}

impl PositionsSubscription {
    /// Move the request time back (tests of the 30 s wait).
    #[doc(hidden)]
    pub fn backdate(&mut self, by: std::time::Duration) {
        self.requested_at -= by;
    }
}

impl PositionsSubscription {
    fn new() -> Self {
        Self { requested_at: std::time::Instant::now(), snapshot_sent: false, sent: HashMap::new(), generation: 0 }
    }
}

/// The next rows of a positions subscription (ibx#477 ibx#476): the snapshot
/// and the end once the position data is in; then one row per change of a
/// position or its average cost. Error 2151 when the data is not in after
/// 30 s.
fn advance_positions(sub: &mut PositionsSubscription, shared: &SharedState) -> Option<PositionsBatch> {
    if !sub.snapshot_sent {
        if !shared.portfolio.account_download_complete() {
            if sub.requested_at.elapsed() >= POSITIONS_WAIT {
                return Some(PositionsBatch {
                    rows: Vec::new(), end: false,
                    error: Some((2151, "Positions info is not available yet".into())),
                });
            }
            return None;
        }
        sub.generation = shared.portfolio.position_generation();
        let mut rows = shared.portfolio.position_infos();
        rows.sort_by_key(|p| p.con_id);
        sub.sent = rows.iter().map(|p| (p.con_id, (p.position_fixed, p.avg_cost))).collect();
        sub.snapshot_sent = true;
        return Some(PositionsBatch { rows, end: true, error: None });
    }
    let generation = shared.portfolio.position_generation();
    if generation == sub.generation {
        return None;
    }
    sub.generation = generation;
    let mut rows: Vec<PositionInfo> = shared.portfolio.position_infos().into_iter()
        .filter(|p| sub.sent.get(&p.con_id) != Some(&(p.position_fixed, p.avg_cost)))
        .collect();
    rows.sort_by_key(|p| p.con_id);
    for p in &rows {
        sub.sent.insert(p.con_id, (p.position_fixed, p.avg_cost));
    }
    (!rows.is_empty()).then_some(PositionsBatch { rows, end: false, error: None })
}

/// A running req_positions_multi (ibx#476).
pub struct PositionsMultiSubscription {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    sub: PositionsSubscription,
}

/// A running req_account_updates_multi (ibx#476).
pub struct AccountMultiSubscription {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    /// ledgerAndNLV: the ledger rows only.
    ledger_only: bool,
    image_sent: bool,
    sent: HashMap<(String, String), String>,
    generation: u64,
}

/// Rows of one req_account_updates_multi (ibx#476).
pub struct AccountMultiBatch {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    pub rows: Vec<crate::bridge::AccountRow>,
    /// The snapshot: account_update_multi_end follows the rows.
    pub end: bool,
}

/// Position rows to send for req_positions (ibx#477).
pub struct PositionsBatch {
    pub rows: Vec<PositionInfo>,
    /// The snapshot: position_end follows the rows.
    pub end: bool,
    /// The position data never came: error(-1, code, message), no end.
    pub error: Option<(i64, String)>,
}

/// How long req_positions waits for the position data before error 2151,
/// as the reference (ibx#477).
pub const POSITIONS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Most account summary requests per client (ibx#479).
pub const ACCOUNT_SUMMARY_MAX: usize = 2;

/// The reference's refusals of an account summary request (ibx#479).
fn summary_refusal(cause: &str) -> (i64, String) {
    (321, format!("Error validating request.-'b2' : cause - {}", cause))
}

/// A single portfolio position update.
pub struct PortfolioUpdateEntry {
    pub con_id: i64,
    pub position: f64,
    pub avg_cost: f64,
    pub market_price: f64,
    pub market_value: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

/// True when `status` names an IB order state that is still working on the broker.
/// Whitelist (rather than blacklist) so non-canonical or empty strings — and
/// any future terminal states added by IB — are treated as "not open".
#[inline]
pub fn is_open_status(status: &str) -> bool {
    matches!(
        status,
        "ApiPending"
            | "PendingSubmit"
            | "PendingCancel"
            | "PreSubmitted"
            | "Submitted"
            | "PartiallyFilled"
    )
}

/// Convert OrderStatus enum to ibapi-compatible string.
#[inline]
pub fn order_status_str(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::PendingSubmit => "PendingSubmit",
        OrderStatus::PreSubmitted => "PreSubmitted",
        OrderStatus::Submitted => "Submitted",
        OrderStatus::PendingCancel => "PendingCancel",
        OrderStatus::PendingReplace => "PendingCancel", // IB API has no PendingReplace string
        OrderStatus::Filled => "Filled",
        // The reference has no partially-filled status: an order with part
        // of it filled is still working (ib-agent#192 C8).
        OrderStatus::PartiallyFilled => "Submitted",
        OrderStatus::Cancelled => "Cancelled",
        // ibapi has no "Rejected" status string — rejected orders surface as "Inactive"
        // with the rejection reason carried separately on OrderState.completedStatus.
        OrderStatus::Rejected => "Inactive",
        OrderStatus::Inactive => "Inactive",
        OrderStatus::Uncertain => "Unknown",
    }
}

// ── Execution storage ──

/// Buy or sell from an API side string: `BUY` / `BOT` buy, `SELL` / `SLD` /
/// `SSHORT` sell. `None` for anything else.
fn side_is_buy(side: &str) -> Option<bool> {
    match side.to_ascii_uppercase().as_str() {
        "BUY" | "BOT" => Some(true),
        "SELL" | "SLD" | "SSHORT" => Some(false),
        _ => None,
    }
}

/// An execution time as the reference writes it in execDetails:
/// `yyyyMMdd HH:mm:ss US/Eastern` (ibx#474; the reference uses the zone of
/// its API time setting, US/Eastern in the captures). UTC when the zone
/// database has no US/Eastern.
pub fn format_exec_time(unix_secs: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(unix_secs) else {
        return String::new();
    };
    match jiff::tz::TimeZone::get("US/Eastern") {
        Ok(tz) => format!("{} US/Eastern", ts.to_zoned(tz).strftime("%Y%m%d %H:%M:%S")),
        Err(_) => format!("{} UTC", ts.to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d %H:%M:%S")),
    }
}

/// A stored execution and its commission report for `req_executions` replay.
/// Shared between Rust and Python adapters via `ClientCore`. The report
/// comes from its own server frame after the fill; `None` until then (ibx#471).
pub struct StoredExecution {
    pub req_id: i64,
    pub contract: ApiContract,
    pub execution: ApiExecution,
    /// Execution time, Unix seconds, for the time filter of `req_executions`.
    pub time_secs: Option<i64>,
    pub commission_and_fees: Option<ApiCommissionAndFeesReport>,
}

// ── Order tracking ──

/// A locally tracked order for `req_open_orders` / dispatch status updates.
#[derive(Clone)]
pub struct TrackedOrder {
    pub contract: ApiContract,
    pub order: ApiOrder,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub instrument: InstrumentId,
    /// Price of the order's last print; 0 before any fill (ibx#473).
    pub last_fill_price: f64,
}

/// What the client reports for an order in `open_order` / `order_status`
/// after a server report (ibx#473).
pub struct OrderView {
    pub contract: ApiContract,
    pub order: ApiOrder,
    pub state: ApiOrderState,
    pub last_fill_price: f64,
    /// This client's id for an order it placed; 0 otherwise.
    pub client_id: i64,
}

/// The reference's answer to a place or modify on an order id that is no
/// longer working: filled, cancelled, or with a cancel pending. Nothing is
/// sent (ibx#463; captured 25/09/2026).
pub const MODIFY_OF_FINISHED_ORDER: (i64, &str) = (104, "Cannot modify a filled order.");

/// Most commission reports kept while waiting for their execution.
pub const PENDING_COMMISSIONS_MAX: usize = 1024;

/// Commission reports waiting for their execution, oldest dropped first
/// past `PENDING_COMMISSIONS_MAX` (ibx#471).
#[derive(Default)]
pub struct PendingCommissions {
    reports: HashMap<String, (u64, ApiCommissionAndFeesReport)>,
    /// (insertion number, key), oldest at the front. An entry whose number
    /// no longer matches `reports` was removed or replaced, and is skipped.
    order: std::collections::VecDeque<(u64, String)>,
    next: u64,
}

impl PendingCommissions {
    pub fn insert(&mut self, key: String, report: ApiCommissionAndFeesReport) {
        let seq = self.next;
        self.next += 1;
        self.order.push_back((seq, key.clone()));
        self.reports.insert(key, (seq, report));
        while self.order.len() > PENDING_COMMISSIONS_MAX {
            if let Some((old_seq, old_key)) = self.order.pop_front() {
                if self.reports.get(&old_key).is_some_and(|(s, _)| *s == old_seq) {
                    self.reports.remove(&old_key);
                }
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<ApiCommissionAndFeesReport> {
        self.reports.remove(key).map(|(_, report)| report)
    }

    pub fn len(&self) -> usize {
        self.reports.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }

    pub fn clear(&mut self) {
        self.reports.clear();
        self.order.clear();
    }
}

/// What `place_order` does with an order id that is already working (ibx#247).
pub enum ModifyPlan {
    /// Send this replace.
    Send(ControlCommand),
    /// Refused before sending, as the reference does; report it through `error()`.
    Refused { code: i64, message: String },
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
    pub last_quotes: Mutex<HashMap<InstrumentId, [i64; 15]>>,
    // Snapshot req_ids — deliver first ticks then auto-cancel
    pub snapshot_reqs: Mutex<HashSet<i64>>,

    // PnL subscription state
    /// Running req_pnl requests, with the last values sent (ibx#478).
    pub pnl_reqs: Mutex<HashMap<i64, [i64; 3]>>,
    pub pnl_quotes: Mutex<PnlQuotes>,
    /// Contract currency already sent to the engine, by conId (ibx#466).
    pub currency_sent: Mutex<HashMap<i64, String>>,
    pub pnl_single_reqs: Mutex<HashMap<i64, i64>>, // req_id → con_id
    // Per-req_id change detection for pnl_single: [pos, daily, unrealized, realized, value] scaled.
    pub last_pnl_single: Mutex<HashMap<i64, [i64; 5]>>,

    // Account summary subscription state (req_id, tags)
    pub account_summaries: Mutex<Vec<AccountSummaryRequest>>,
    pub positions_sub: Mutex<Option<PositionsSubscription>>,
    pub positions_multi: Mutex<Vec<PositionsMultiSubscription>>,
    pub account_multi: Mutex<Vec<AccountMultiSubscription>>,
    pub next_account_summary: AtomicU64,

    // News bulletin subscription
    pub bulletin_subscribed: AtomicBool,

    // Account updates subscription
    pub account_updates_subscribed: AtomicBool,
    pub account_stream: Mutex<AccountStream>,
    pub last_portfolio: Mutex<Option<Vec<PositionInfo>>>,

    // Execution replay store
    pub executions: Mutex<Vec<StoredExecution>>,
    // The API client id given at connect (0 in the Rust client, which has
    // none): the clientId of this client's executions (ibx#474).
    pub client_id: AtomicI64,
    // Commission reports that came before their execution, by execution id
    // without its revision (ibx#471). Bounded: reports for executions this
    // client never sees (earlier sessions, other clients) are dropped oldest
    // first.
    pub pending_commissions: Mutex<PendingCommissions>,

    // Open order tracking
    pub open_orders: Mutex<HashMap<u64, TrackedOrder>>,
    // Ids of tracked orders that were filled or cancelled: never sent again (ibx#463).
    pub finished_orders: Mutex<HashSet<u64>>,

    // Market data type callback tracking
    pub market_data_type: AtomicI32,
    pub mdt_sent: Mutex<HashSet<i64>>,

    // Historical data keepUpToDate: req_ids that have completed initial batch.
    // Subsequent bars for these req_ids dispatch as historical_data_update.
    pub hist_initial_complete: Mutex<HashSet<u32>>,

    // News subscription state
    pub news_providers: Mutex<String>,
    pub news_instruments: Mutex<HashSet<InstrumentId>>,

    // Contract cache for enrichment
    pub contract_cache: Mutex<HashMap<i64, ApiContract>>,
}

/// The reference's other names for order types ibx supports, and the name
/// ibx uses (ibx#469, from the reference's order-type map).
const ORDER_TYPE_ALIASES: [(&str, &str); 14] = [
    ("LIMIT", "LMT"),
    ("MARKET", "MKT"),
    ("STOP", "STP"),
    ("STPLMT", "STP LMT"),
    ("STOP LIMIT", "STP LMT"),
    ("MKT CLS", "MOC"),
    ("LMT CLS", "LOC"),
    ("MKT TO LMT", "MTL"),
    ("RELATIVE", "REL"),
    ("PEG PRIM", "REL"),
    ("PEGMKT", "PEG MKT"),
    ("PEGMID", "PEG MID"),
    ("TRAILING STOP", "TRAIL"),
    ("TRAILLMT", "TRAIL LIMIT"),
];

/// The reference's text for an invalid date or time (errors 337 and 343);
/// %s is the field's label.
const INVALID_DATE_TIME: &str = "%s: The date, time, or time-zone entered is invalid.\n\
The correct format is yyyymmdd hh:mm:ss xx/xxxx\n\
where yyyymmdd and xx/xxxx are optional.\n\
E.g.: 20031126 15:59:00 US/Eastern\n\
\n\
Note that there is a space between the date and time,\n\
and between the time and time-zone.\n\
\n\
If no date is specified, current date is assumed.\n\
If no time-zone is specified, local time-zone is assumed(deprecated).\n\
\n\
You can also provide yyyymmddd-hh:mm:ss time is in UTC.\n\
Note that there is a dash between the date and time in UTC notation.";

/// A TRAIL LIMIT's limit price, offset and stop price are what the server
/// reports (44, 6370, 6117), as the reference's openOrder (ib-agent#194,
/// ibx#491).
fn reported_trail_limit(order: &mut ApiOrder, reported: &ApiOrder) {
    if !order.order_type.eq_ignore_ascii_case("TRAIL LIMIT") { return; }
    if reported.lmt_price != 0.0 { order.lmt_price = reported.lmt_price; }
    if reported.lmt_price_offset != f64::MAX { order.lmt_price_offset = reported.lmt_price_offset; }
    if reported.trail_stop_price != f64::MAX { order.trail_stop_price = reported.trail_stop_price; }
}

impl ClientCore {
    pub fn new() -> Self {
        Self {
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            con_id_to_instrument: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
            snapshot_reqs: Mutex::new(HashSet::new()),
            pnl_reqs: Mutex::new(HashMap::new()),
            pnl_quotes: Mutex::new(PnlQuotes::default()),
            currency_sent: Mutex::new(HashMap::new()),
            pnl_single_reqs: Mutex::new(HashMap::new()),
            last_pnl_single: Mutex::new(HashMap::new()),
            account_summaries: Mutex::new(Vec::new()),
            positions_sub: Mutex::new(None),
            positions_multi: Mutex::new(Vec::new()),
            account_multi: Mutex::new(Vec::new()),
            next_account_summary: AtomicU64::new(1),
            bulletin_subscribed: AtomicBool::new(false),
            account_updates_subscribed: AtomicBool::new(false),
            account_stream: Mutex::new(AccountStream::default()),
            last_portfolio: Mutex::new(None),
            executions: Mutex::new(Vec::new()),
            client_id: AtomicI64::new(0),
            pending_commissions: Mutex::new(PendingCommissions::default()),
            open_orders: Mutex::new(HashMap::new()),
            finished_orders: Mutex::new(HashSet::new()),
            market_data_type: AtomicI32::new(1),
            mdt_sent: Mutex::new(HashSet::new()),
            hist_initial_complete: Mutex::new(HashSet::new()),
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
        self.pnl_reqs.lock().unwrap().clear();
        *self.pnl_quotes.lock().unwrap() = PnlQuotes::default();
        self.currency_sent.lock().unwrap().clear();
        self.pnl_single_reqs.lock().unwrap().clear();
        self.last_pnl_single.lock().unwrap().clear();
        self.account_summaries.lock().unwrap().clear();
        *self.positions_sub.lock().unwrap() = None;
        self.positions_multi.lock().unwrap().clear();
        self.account_multi.lock().unwrap().clear();
        self.bulletin_subscribed.store(false, Ordering::Relaxed);
        self.account_updates_subscribed.store(false, Ordering::Relaxed);
        *self.account_stream.lock().unwrap() = AccountStream::default();
        *self.last_portfolio.lock().unwrap() = None;
        self.executions.lock().unwrap().clear();
        self.pending_commissions.lock().unwrap().clear();
        self.open_orders.lock().unwrap().clear();
        // `finished_orders` is kept: the server still knows those orders
        // after a reconnect, so their ids must not be sent as new orders.
        self.market_data_type.store(1, Ordering::Relaxed);
        self.mdt_sent.lock().unwrap().clear();
        self.hist_initial_complete.lock().unwrap().clear();
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

    /// Wait for the hot loop to process a registration command and return the
    /// assigned ID. The engine replies Err when the instrument table is full
    /// (ibx#233) — previously that condition killed the hot loop.
    fn recv_registration(reply_rx: crossbeam_channel::Receiver<Result<InstrumentId, String>>) -> Result<InstrumentId, String> {
        reply_rx.recv_timeout(Self::REGISTRATION_TIMEOUT)
            .map_err(|_| "Registration timed out".to_string())?
    }

    /// Find instrument ID for a contract, registering if needed.
    /// Returns `Err` if the control channel is closed.
    /// Tell the engine the currency of a contract before an order on it,
    /// when it does not have it yet (tag 15, ibx#466).
    pub fn note_currency(&self, control_tx: &Sender<ControlCommand>, con_id: i64, currency: &str) {
        if currency.is_empty() || con_id == 0 {
            return;
        }
        let mut sent = self.currency_sent.lock().unwrap();
        if sent.get(&con_id).map(String::as_str) == Some(currency) {
            return;
        }
        if control_tx.send(ControlCommand::SetInstrumentCurrency { con_id, currency: currency.to_string() }).is_ok() {
            sent.insert(con_id, currency.to_string());
        }
    }

    pub fn find_or_register_instrument(
        &self,
        control_tx: &Sender<ControlCommand>,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
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
        control_tx.send(ControlCommand::RegisterInstrument {
            con_id, symbol: symbol.to_string(),
            sec_type: sec_type.to_string(), exchange: exchange.to_string(),
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

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
        last_trade_date: &str,
        strike: f64,
        right: &str,
        multiplier: &str,
        snapshot: bool,
        generic_tick_list: &str,
        mode_9887: i32,
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

        // A quote ibx subscribed to for the P&L becomes the caller's
        // subscription: no second subscription to the server.
        if let Some(instrument_id) = self.pnl_quotes.lock().unwrap().active.remove(&con_id) {
            self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
            self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
            self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);
            if snapshot {
                self.snapshot_reqs.lock().unwrap().insert(req_id);
            }
            if wants_news {
                self.news_instruments.lock().unwrap().insert(instrument_id);
            }
            return Ok(instrument_id);
        }

        // instrument_to_req maps ONE req_id per instrument: a second live
        // subscription would clobber the first's reverse mapping and orphan
        // it silently — no ticks, no error (ibx#233). Reject up front via
        // the client-side conId cache, before anything reaches the engine.
        {
            let cache = self.con_id_to_instrument.lock().unwrap();
            if let Some(&iid) = cache.get(&con_id) {
                if let Some(&existing) = self.instrument_to_req.lock().unwrap().get(&iid) {
                    if existing != req_id {
                        return Err(format!(
                            "contract (con_id {}) already has a live market-data \
                             subscription under req_id {}: cancel it first or \
                             reuse that req_id", con_id, existing,
                        ));
                    }
                }
            }
        }

        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument {
            con_id, symbol: symbol.to_string(),
            sec_type: sec_type.to_string(), exchange: exchange.to_string(),
            reply_tx: None,
        }).map_err(|e| format!("Engine stopped: {}", e))?;
        control_tx.send(ControlCommand::Subscribe {
            con_id,
            symbol: symbol.to_string(),
            exchange: exchange.to_string(),
            sec_type: sec_type.to_string(),
            last_trade_date: last_trade_date.to_string(),
            strike,
            right: right.to_string(),
            multiplier: multiplier.to_string(),
            mode_9887,
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let instrument_id = Self::recv_registration(reply_rx)?;
        self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
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

    /// Keep a quote for each stock position the running P&L requests need
    /// (held now or at midnight, and each pnl_single contract), subscribed
    /// by ibx itself when the caller has none; cancel them when no P&L
    /// request needs them. Never waits: a registration reply is read on a
    /// later call. Checked when positions or requests change, and every
    /// second.
    pub fn maintain_pnl_quotes(&self, shared: &SharedState, control_tx: &Sender<ControlCommand>) {
        let n_pnl = self.pnl_reqs.lock().unwrap().len();
        let singles: Vec<i64> = self.pnl_single_reqs.lock().unwrap().values().copied().collect();
        let mut q = self.pnl_quotes.lock().unwrap();
        if n_pnl == 0 && singles.is_empty() && q.active.is_empty() && q.pending.is_empty() {
            return;
        }

        let done: Vec<(i64, Result<InstrumentId, String>)> = q.pending.iter()
            .filter_map(|(&con_id, rx)| rx.try_recv().ok().map(|r| (con_id, r)))
            .collect();
        for (con_id, reply) in done {
            q.pending.remove(&con_id);
            match reply {
                Ok(instrument) => {
                    q.active.insert(con_id, instrument);
                    self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument);
                }
                Err(e) => log::warn!("P&L quote for conId {} not subscribed: {}", con_id, e),
            }
        }

        let seeds = shared.portfolio.midnight_seeds();
        let key = (shared.portfolio.position_generation(), n_pnl, singles.len(), seeds.len());
        if key == q.key && q.checked_at.is_some_and(|t| t.elapsed() < PNL_QUOTES_CHECK) {
            return;
        }
        q.key = key;
        q.checked_at = Some(std::time::Instant::now());

        let infos: HashMap<i64, PositionInfo> = shared.portfolio.position_infos()
            .into_iter().map(|p| (p.con_id, p)).collect();
        let is_stock = |con_id: i64| infos.get(&con_id).is_none_or(|p| p.sec_type.is_empty() || p.sec_type == "STK");
        let mut wanted: HashSet<i64> = HashSet::new();
        if n_pnl > 0 {
            wanted.extend(infos.values().filter(|p| p.position_fixed != 0).map(|p| p.con_id));
            wanted.extend(seeds.iter().filter(|s| s.qty_midnight_fixed != 0).map(|s| s.con_id));
        }
        wanted.extend(singles.iter().copied());
        wanted.retain(|&c| c != 0 && is_stock(c));

        let caller_has = |con_id: i64| -> bool {
            let map = self.con_id_to_instrument.lock().unwrap();
            map.get(&con_id).is_some_and(|iid| self.instrument_to_req.lock().unwrap().contains_key(iid))
        };

        for &con_id in &wanted {
            if q.active.contains_key(&con_id) || q.pending.contains_key(&con_id) || caller_has(con_id) {
                continue;
            }
            let symbol = infos.get(&con_id).map(|p| p.symbol.clone()).unwrap_or_default();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            let registered = control_tx.send(ControlCommand::RegisterInstrument {
                con_id, symbol: symbol.clone(), sec_type: "STK".into(), exchange: "SMART".into(), reply_tx: None,
            }).and_then(|_| control_tx.send(ControlCommand::Subscribe {
                con_id, symbol, exchange: "SMART".into(), sec_type: "STK".into(),
                last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
                mode_9887: 0, reply_tx: Some(reply_tx),
            }));
            if registered.is_ok() {
                q.pending.insert(con_id, reply_rx);
            }
        }

        let unwanted: Vec<(i64, InstrumentId)> = q.active.iter()
            .filter(|(c, _)| !wanted.contains(c))
            .map(|(&c, &i)| (c, i))
            .collect();
        for (con_id, instrument) in unwanted {
            q.active.remove(&con_id);
            if !caller_has(con_id) {
                let _ = control_tx.send(ControlCommand::Unsubscribe { instrument });
                // The engine may reuse the slot (ibx#233).
                self.forget_instrument(instrument);
            }
        }
    }

    /// Unregister a market data subscription.
    /// Returns `(instrument_id, needs_news_unsub)`.
    pub fn unregister_mkt_data(&self, req_id: i64) -> (Option<InstrumentId>, bool) {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            self.last_quotes.lock().unwrap().remove(&instrument);
            self.mdt_sent.lock().unwrap().remove(&req_id);
            let needs_news = self.news_instruments.lock().unwrap().remove(&instrument);
            self.forget_instrument(instrument);
            (Some(instrument), needs_news)
        } else {
            (None, false)
        }
    }

    /// Drop the client-side conId cache entries for an instrument id. The
    /// engine may reclaim and reuse the slot after an unsubscribe (ibx#233);
    /// a stale cache entry would silently point the old conId at whatever
    /// contract inherits the id. A later request for that conId simply
    /// re-registers.
    pub fn forget_instrument(&self, instrument: InstrumentId) {
        let mut sent = self.currency_sent.lock().unwrap();
        self.con_id_to_instrument.lock().unwrap().retain(|con_id, iid| {
            let keep = *iid != instrument;
            if !keep {
                // The engine forgets the slot's currency too.
                sent.remove(con_id);
            }
            keep
        });
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

    /// Contract for a portfolio row: the cached contract when the session
    /// has one, else the symbol, security type and currency the server's
    /// portfolio row carries. The reference fills these in updatePortfolio;
    /// ibx sent only the contract id for a contract it had not looked up.
    pub fn position_contract(&self, con_id: i64, shared: &SharedState) -> ApiContract {
        self.get_contract(con_id, shared).unwrap_or_else(|| {
            let pi = shared.portfolio.position_info(con_id).unwrap_or_default();
            ApiContract {
                con_id, symbol: pi.symbol, sec_type: pi.sec_type, currency: pi.currency,
                multiplier: pi.multiplier, ..Default::default()
            }
        })
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
        self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
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
        self.pnl_reqs.lock().unwrap().entry(req_id).or_insert([PNL_NOT_SENT; 3]);
    }

    pub fn unsubscribe_pnl(&self, req_id: i64) -> bool {
        self.pnl_reqs.lock().unwrap().remove(&req_id).is_some()
    }

    pub fn subscribe_pnl_single(&self, req_id: i64, con_id: i64) {
        self.pnl_single_reqs.lock().unwrap().insert(req_id, con_id);
    }

    pub fn unsubscribe_pnl_single(&self, req_id: i64) -> bool {
        self.last_pnl_single.lock().unwrap().remove(&req_id);
        self.pnl_single_reqs.lock().unwrap().remove(&req_id).is_some()
    }

    /// Check and start a req_pnl (ibx#478), as the reference: an empty or
    /// unknown account gives 321, a request id already running gives 102.
    pub fn request_pnl(&self, req_id: i64, account: &str, own_account: &str) -> Result<(), (i64, String)> {
        pnl_account_refusal("cj", account, own_account)?;
        if self.pnl_reqs.lock().unwrap().contains_key(&req_id) {
            return Err((102, "Duplicate ticker id".into()));
        }
        self.subscribe_pnl(req_id);
        Ok(())
    }

    /// Check and start a req_pnl_single (ibx#478); same checks as req_pnl.
    pub fn request_pnl_single(&self, req_id: i64, account: &str, own_account: &str, con_id: i64) -> Result<(), (i64, String)> {
        pnl_account_refusal("ck", account, own_account)?;
        if self.pnl_single_reqs.lock().unwrap().contains_key(&req_id) {
            return Err((102, "Duplicate ticker id".into()));
        }
        self.subscribe_pnl_single(req_id, con_id);
        Ok(())
    }

    /// cancel_pnl: 10185 for a request id not running (ibx#478).
    pub fn cancel_pnl_request(&self, req_id: i64) -> Option<(i64, String)> {
        (!self.unsubscribe_pnl(req_id)).then(|| (10185, "Failed to cancel PNL (not subscribed)".to_string()))
    }

    /// cancel_pnl_single: 10186 for a request id not running (ibx#478).
    pub fn cancel_pnl_single_request(&self, req_id: i64) -> Option<(i64, String)> {
        (!self.unsubscribe_pnl_single(req_id)).then(|| (10186, "Failed to cancel PNL single (not subscribed)".to_string()))
    }

    // ── Account summary subscription management ──

    /// Check and register an account summary request (ibx#479), as the
    /// reference does: empty tags or group, or a group other than `All` /
    /// `AllNonProp` on a single account, give 321; a third request gives 322.
    /// A request id already running replaces that request.
    pub fn subscribe_account_summary(&self, req_id: i64, group: &str, tags: &str) -> Result<AccountSummaryPlan, (i64, String)> {
        if tags.trim().is_empty() {
            return Err(summary_refusal("Tags cannot be null"));
        }
        if group.is_empty() {
            return Err(summary_refusal("Group name cannot be null"));
        }
        if group != "All" && group != "AllNonProp" {
            return Err(summary_refusal("Group name is invalid"));
        }
        let mut ledger = LedgerChoice::None;
        let mut wire: Vec<&str> = Vec::new();
        for item in tags.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(rest) = item.strip_prefix("$LEDGER") {
                ledger = match rest.strip_prefix(':') {
                    None => LedgerChoice::Base,
                    Some("ALL") => LedgerChoice::All,
                    Some(ccy) => LedgerChoice::Currency(ccy.to_string()),
                };
                if !wire.contains(&"$LEDGER") {
                    wire.push("$LEDGER");
                }
            } else {
                wire.push(item);
            }
        }
        let mut reqs = self.account_summaries.lock().unwrap();
        let cancel_sr_id = reqs.iter().position(|r| r.req_id == req_id).map(|i| reqs.remove(i).sr_id);
        if reqs.len() >= ACCOUNT_SUMMARY_MAX {
            return Err((322, "Maximum number of account summary requests exceeded; desubscribe to previous request first".into()));
        }
        let sr_id = format!("SR.Socket.{}", self.next_account_summary.fetch_add(1, Ordering::Relaxed));
        reqs.push(AccountSummaryRequest { req_id, sr_id: sr_id.clone(), ledger });
        Ok(AccountSummaryPlan { cancel_sr_id, sr_id, wire_tags: wire.join(","), group: group.to_string() })
    }

    /// Forget an account summary request; returns its subscription id to
    /// cancel on the server.
    pub fn unsubscribe_account_summary(&self, req_id: i64) -> Option<String> {
        let mut reqs = self.account_summaries.lock().unwrap();
        let i = reqs.iter().position(|r| r.req_id == req_id)?;
        Some(reqs.remove(i).sr_id)
    }

    // ── Positions subscription (ibx#477) ──

    /// Start req_positions: a subscription, as the reference. One per
    /// client; a new request replaces the running one.
    pub fn subscribe_positions(&self) {
        *self.positions_sub.lock().unwrap() = Some(PositionsSubscription::new());
    }

    pub fn unsubscribe_positions(&self) {
        *self.positions_sub.lock().unwrap() = None;
    }

    /// Position rows to send (ibx#477): the snapshot and the end once the
    /// position data is in; then one row each time a position or its
    /// average cost changes. When the data is not in after 30 s: error 2151
    /// and no end, and the request ends.
    pub fn prepare_positions(&self, shared: &SharedState) -> Option<PositionsBatch> {
        let mut guard = self.positions_sub.lock().unwrap();
        let sub = guard.as_mut()?;
        let batch = advance_positions(sub, shared);
        if batch.as_ref().is_some_and(|b| b.error.is_some()) {
            *guard = None;
        }
        batch
    }

    // ── Multi-account requests (ibx#476) ──

    /// Start req_positions_multi: the same subscription as req_positions,
    /// with its request id and model code. The same id again replaces it.
    pub fn subscribe_positions_multi(&self, req_id: i64, account: &str, model_code: &str) {
        let mut subs = self.positions_multi.lock().unwrap();
        subs.retain(|m| m.req_id != req_id);
        subs.push(PositionsMultiSubscription {
            req_id, account: account.to_string(), model_code: model_code.to_string(),
            sub: PositionsSubscription::new(),
        });
    }

    pub fn unsubscribe_positions_multi(&self, req_id: i64) {
        self.positions_multi.lock().unwrap().retain(|m| m.req_id != req_id);
    }

    /// Rows of each running req_positions_multi: the snapshot and the end,
    /// then one row per change (ibx#476). No position data after 30 s:
    /// error 2151 for that request, and it ends.
    pub fn prepare_positions_multi(&self, shared: &SharedState) -> Vec<(i64, String, String, PositionsBatch)> {
        let mut subs = self.positions_multi.lock().unwrap();
        let mut out = Vec::new();
        subs.retain_mut(|m| match advance_positions(&mut m.sub, shared) {
            Some(batch) => {
                let expired = batch.error.is_some();
                out.push((m.req_id, m.account.clone(), m.model_code.clone(), batch));
                !expired
            }
            None => true,
        });
        out
    }

    /// Start req_account_updates_multi (ibx#476). A request id already
    /// running gives 322, as the reference.
    pub fn subscribe_account_multi(&self, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool) -> Result<(), (i64, String)> {
        let mut subs = self.account_multi.lock().unwrap();
        if subs.iter().any(|m| m.req_id == req_id) {
            return Err((322, "Duplicate ticker id".into()));
        }
        subs.push(AccountMultiSubscription {
            req_id, account: account.to_string(), model_code: model_code.to_string(),
            ledger_only: ledger_and_nlv, image_sent: false, sent: HashMap::new(), generation: 0,
        });
        Ok(())
    }

    /// Cancel sends nothing, as the reference.
    pub fn unsubscribe_account_multi(&self, req_id: i64) {
        self.account_multi.lock().unwrap().retain(|m| m.req_id != req_id);
    }

    /// Rows of each running req_account_updates_multi (ibx#476): the
    /// account values (unless ledgerAndNLV) and the ledger rows as the server
    /// sent them, then the end, once the account image is complete; then the
    /// rows that change, with no end.
    pub fn prepare_account_multi(&self, shared: &SharedState) -> Vec<AccountMultiBatch> {
        let mut subs = self.account_multi.lock().unwrap();
        if subs.is_empty() {
            return Vec::new();
        }
        let (generation, complete, _) = shared.portfolio.account_rows_generation();
        if !complete {
            return Vec::new();
        }
        let mut store = None;
        let mut out = Vec::new();
        for m in subs.iter_mut() {
            if m.image_sent && m.generation == generation {
                continue;
            }
            let rows_now = store.get_or_insert_with(|| shared.portfolio.account_rows());
            let mut rows = Vec::new();
            for row in rows_now.rows.iter().filter(|r| !m.ledger_only || r.ledger) {
                let id = (row.key.clone(), row.currency.clone());
                if m.sent.get(&id) != Some(&row.value) {
                    m.sent.insert(id, row.value.clone());
                    rows.push(row.clone());
                }
            }
            let end = !m.image_sent;
            m.image_sent = true;
            m.generation = generation;
            if end || !rows.is_empty() {
                out.push(AccountMultiBatch {
                    req_id: m.req_id, account: m.account.clone(), model_code: m.model_code.clone(), rows, end,
                });
            }
        }
        out
    }

    // ── Account updates subscription management ──

    /// Subscribe to or unsubscribe from account updates. As the reference
    /// (ibx#475): a subscribe while already subscribed changes nothing (no
    /// second image, no second end); an unsubscribe of a subscription
    /// answers error 2100, which the caller reports with id -1.
    pub fn subscribe_account_updates(&self, subscribe: bool) -> Option<(i64, String)> {
        let was = self.account_updates_subscribed.swap(subscribe, Ordering::AcqRel);
        if subscribe {
            if !was {
                *self.account_stream.lock().unwrap() = AccountStream { image_pending: true, ..Default::default() };
                *self.last_portfolio.lock().unwrap() = None;
            }
            return None;
        }
        *self.account_stream.lock().unwrap() = AccountStream::default();
        *self.last_portfolio.lock().unwrap() = None;
        was.then(|| (ACCOUNT_UNSUBSCRIBED.0, ACCOUNT_UNSUBSCRIBED.1.to_string()))
    }

    // ── Market data type tracking ──

    /// Store the requested market data type. NOT sent to the gateway — the
    /// engine has no wire path for it, so subscriptions always deliver
    /// realtime data (ibx#234). Requesting anything else warns loudly
    /// instead of pretending.
    pub fn set_market_data_type(&self, mdt: i32) {
        if mdt != MDT_REALTIME {
            log::warn!(
                "req_market_data_type({}) is not supported: the type is not \
                 sent to the gateway and subscriptions remain realtime; \
                 delayed tick variants are never emitted (ibx#234)",
                mdt,
            );
        }
        self.market_data_type.store(mdt, Ordering::Relaxed);
    }

    /// Check if the `market_data_type` callback should fire for this req_id.
    /// Returns `Some(type)` on the first call per req_id that has data, `None`
    /// thereafter. Always reports realtime — the DELIVERED type — rather than
    /// echoing a requested type the engine never transmitted; the old echo
    /// confirmed a state that did not exist (ibx#234).
    pub fn check_mdt_needed(&self, req_id: i64, has_data: bool) -> Option<i32> {
        if has_data && self.mdt_sent.lock().unwrap().insert(req_id) {
            Some(MDT_REALTIME)
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

    /// Store an execution for `req_executions`. Returns the commission
    /// report that came before it, to send after `exec_details`.
    pub fn push_execution(&self, req_id: i64, contract: ApiContract, execution: ApiExecution, time_secs: Option<i64>) -> Option<ApiCommissionAndFeesReport> {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&execution.exec_id);
        let commission_and_fees = if execution.exec_id.is_empty() {
            None
        } else {
            self.pending_commissions.lock().unwrap().remove(base)
        };
        self.executions.lock().unwrap().push(StoredExecution {
            req_id, contract, execution, time_secs, commission_and_fees: commission_and_fees.clone(),
        });
        commission_and_fees
    }

    /// Set what the fill report says about this execution (ibx#474): the
    /// server's execution id, the time as the reference writes it, tag 100 /
    /// 207 as exchange, the placing client (this client when the report has
    /// none), modelCode, and the orderRef of the report or of the tracked
    /// order. Fields the report did not carry keep their value.
    pub fn apply_fill_exec(&self, ex: &mut ApiExecution, fe: &crate::bridge::FillExec, order_id: u64) {
        if !fe.exec_id.is_empty() {
            ex.exec_id = fe.exec_id.clone();
        }
        if let Some(secs) = fe.time_secs {
            ex.time = format_exec_time(secs);
        }
        if !fe.exchange.is_empty() {
            ex.exchange = fe.exchange.clone();
        }
        ex.client_id = if fe.client_id != 0 { fe.client_id } else { self.client_id.load(Ordering::Relaxed) };
        if !fe.model_code.is_empty() {
            ex.model_code = fe.model_code.clone();
        }
        ex.order_ref = if !fe.order_ref.is_empty() {
            fe.order_ref.clone()
        } else {
            self.open_orders.lock().unwrap().get(&order_id)
                .map(|t| t.order.order_ref.clone())
                .unwrap_or_default()
        };
    }

    /// Attach a commission report to its stored execution (a higher revision
    /// of the execution id replaces the report). Returns `true` when the
    /// execution is known, and the report is to be sent now. Otherwise the
    /// report waits for its execution, as the reference sends a live report
    /// only for a known execution (ibx#471).
    pub fn apply_commission(&self, report: &ApiCommissionAndFeesReport) -> bool {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&report.exec_id);
        let mut execs = self.executions.lock().unwrap();
        let stored = execs.iter_mut().rev().find(|se| {
            !se.execution.exec_id.is_empty()
                && crate::engine::hot_loop::ccp::split_exec_revision(&se.execution.exec_id).0 == base
        });
        match stored {
            Some(se) => {
                se.commission_and_fees = Some(report.clone());
                true
            }
            None => {
                self.pending_commissions.lock().unwrap().insert(base.to_string(), report.clone());
                false
            }
        }
    }

    /// Return executions matching the given filter.
    ///
    /// As the reference (ibx#474): symbol, secType and exchange match
    /// exactly; the side is read as buy or sell, so `BUY` matches `BOT`;
    /// clientId 0 is every client; the time keeps executions at or after it.
    /// A time that does not parse is logged and not applied.
    pub fn filter_executions(&self, filter: &ExecutionFilter) -> Vec<usize> {
        let side = if filter.side.is_empty() { None } else { Some(side_is_buy(&filter.side)) };
        let since = if filter.time.trim().is_empty() {
            None
        } else {
            match crate::config::parse_ib_time(&filter.time) {
                Ok(Some(secs)) => Some(secs),
                _ => {
                    log::warn!("req_executions: filter time '{}' is not a valid time; not applied", filter.time);
                    None
                }
            }
        };
        let execs = self.executions.lock().unwrap();
        execs.iter().enumerate().filter_map(|(i, se)| {
            if !filter.symbol.is_empty() && se.contract.symbol != filter.symbol {
                return None;
            }
            if !filter.sec_type.is_empty() && se.contract.sec_type != filter.sec_type {
                return None;
            }
            if !filter.exchange.is_empty() && se.execution.exchange != filter.exchange {
                return None;
            }
            if let Some(buy) = side {
                if buy.is_none() || buy != side_is_buy(&se.execution.side) {
                    return None;
                }
            }
            if let Some(since) = since {
                if !se.time_secs.is_some_and(|t| t >= since) {
                    return None;
                }
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

    /// Track a newly placed order. For a modify of a tracked order, the
    /// status and fill counts stay as the server last reported them: the
    /// engine can still refuse the modify (ibx#463).
    pub fn track_order(&self, order_id: u64, contract: ApiContract, order: ApiOrder, instrument: InstrumentId) {
        let mut orders = self.open_orders.lock().unwrap();
        if let Some(o) = orders.get_mut(&order_id) {
            o.contract = contract;
            o.order = order;
            o.instrument = instrument;
            return;
        }
        let remaining = order.total_quantity;
        orders.insert(order_id, TrackedOrder {
            contract, order, status: "PendingSubmit".into(), filled: 0.0, remaining, instrument,
            last_fill_price: 0.0,
        });
    }

    /// The order as `open_order` reports it after a server report, with
    /// `status`: the tracked order as the caller placed it when this client
    /// placed it, else the order the server reports; the order state of the
    /// latest report (ibx#473). `None` for an order known to neither.
    pub fn order_view(&self, order_id: u64, shared: &SharedState, status: &str) -> Option<OrderView> {
        let tracked = self.open_orders.lock().unwrap().get(&order_id).cloned();
        let info = shared.orders.get_order_info(order_id);
        let mut state = info.as_ref().map(|i| i.order_state.clone()).unwrap_or_default();
        state.status = status.into();
        let (contract, order, last_fill_price, client_id) = match (tracked, info) {
            (Some(t), info) => {
                let mut order = t.order;
                order.order_id = order_id as i64;
                if let Some(i) = &info {
                    if order.perm_id == 0 { order.perm_id = i.order.perm_id; }
                    if order.account.is_empty() { order.account = i.order.account.clone(); }
                    reported_trail_limit(&mut order, &i.order);
                }
                (t.contract, order, t.last_fill_price, self.client_id.load(Ordering::Relaxed))
            }
            (None, Some(i)) => (i.contract, i.order, 0.0, 0),
            (None, None) => return None,
        };
        let contract = if contract.con_id != 0 {
            self.get_contract(contract.con_id, shared).unwrap_or(contract)
        } else {
            contract
        };
        Some(OrderView { contract, order, state, last_fill_price, client_id })
    }

    /// Keep the price of an order's last print for later order_status
    /// callbacks (ibx#473).
    pub fn record_last_fill_price(&self, order_id: u64, price: f64) {
        if let Some(o) = self.open_orders.lock().unwrap().get_mut(&order_id) {
            o.last_fill_price = price;
        }
    }

    /// Update a tracked order after a fill. Removes the order if fully filled.
    pub fn update_order_fill(&self, order_id: u64, status: &str, filled: f64, remaining: f64) {
        let mut orders = self.open_orders.lock().unwrap();
        if remaining == 0.0 {
            if orders.remove(&order_id).is_some() {
                self.finished_orders.lock().unwrap().insert(order_id);
            }
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
            if matches!(status, "Filled" | "Cancelled") {
                self.finished_orders.lock().unwrap().insert(order_id);
            }
        }
    }

    /// The reference's answer when `place_order` names an order this client
    /// tracked that is filled or cancelled: error 104 and nothing sent
    /// (ibx#463). A new order with that id would be a second real order.
    /// A pending cancel is checked by the engine, which holds the current
    /// status. A what-if is not a modify and is not checked.
    pub fn refusal_for_order_id(&self, order_id: u64, order: &ApiOrder) -> Option<(i64, String)> {
        if order.what_if {
            return None;
        }
        if self.finished_orders.lock().unwrap().contains(&order_id) {
            let (code, message) = MODIFY_OF_FINISHED_ORDER;
            return Some((code, message.into()));
        }
        None
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
                if is_open_status(&o.status) {
                    let contract = if o.contract.con_id != 0 {
                        self.get_contract(o.contract.con_id, shared).unwrap_or_else(|| o.contract.clone())
                    } else {
                        o.contract.clone()
                    };
                    let mut order = o.order.clone();
                    if let Some(info) = shared.orders.get_order_info(oid) {
                        reported_trail_limit(&mut order, &info.order);
                    }
                    result.push((oid, TrackedOrder {
                        contract,
                        order,
                        status: o.status.clone(),
                        filled: o.filled,
                        remaining: o.remaining,
                        instrument: o.instrument,
                        last_fill_price: o.last_fill_price,
                    }));
                }
            }
        }

        // Add shared-only entries not already present from local
        for (oid, info) in shared_orders {
            if !is_open_status(&info.order_state.status) {
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
                    last_fill_price: 0.0,
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
            q.bid_exch_mask, q.ask_exch_mask, q.last_exch_mask,
        ];

        // Single lock acquisition for both read and write of last_quotes.
        let mut map = self.last_quotes.lock().unwrap();
        let last = map.get(&iid).copied().unwrap_or([0i64; 15]);

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

        // Exchange-code string ticks: rendering is left to dispatch since it
        // depends on shared.reference.smart_components(). Emit a delta record
        // when the bitmask changes; dispatch resolves the letter string.
        let mut string_ticks = Vec::new();
        const EXCH_TICKS: &[(usize, i32)] = &[
            (12, TICK_BID_EXCHANGE), (13, TICK_ASK_EXCHANGE), (14, TICK_LAST_EXCHANGE),
        ];
        for &(idx, tt) in EXCH_TICKS {
            if fields[idx] != last[idx] {
                let letters = render_exchange_mask(fields[idx], shared);
                string_ticks.push(StringTickEvent {
                    req_id, tick_type: tt, value: letters,
                });
                delivered = true;
            }
        }

        map.insert(iid, fields);

        QuotePollResult { ticks, string_ticks, timestamp, delivered }
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
    /// Computes daily P&L client-side from midnight seeds + live quotes.
    /// Formula: dailyPnL = Σ(qtyNow × priceNow - qtyMidnight × prevClose - moneyTraded)
    /// For positions opened intraday (no seed), synthesizes
    /// moneyTraded = qtyNow × avgCost so the formula collapses to unrealized P&L.
    pub fn poll_pnl(&self, shared: &SharedState) -> Vec<PnlUpdate> {
        if self.pnl_reqs.lock().unwrap().is_empty() {
            return Vec::new();
        }
        let realized_since = shared.portfolio.realized_since_seed();
        let money_since = shared.portfolio.money_since_seed();

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        let positions = shared.portfolio.position_infos();

        let mut con_ids: HashSet<i64> = seeds.keys().copied().collect();
        for pi in &positions {
            con_ids.insert(pi.con_id);
        }
        con_ids.extend(realized_since.keys().copied());
        con_ids.extend(money_since.keys().copied());
        // Sum in a fixed order: a set's order changes between polls, and the
        // float sums with it, so the same P&L passed the change filter again
        // and again (a callback every poll, seen on paper).
        let mut con_ids: Vec<i64> = con_ids.into_iter().collect();
        con_ids.sort_unstable();
        // No early return on an empty set: an account with no position still
        // has account-level P&L (for example realized today), delivered by the
        // fallback below (ibx#239, ibx#301).

        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut total_daily: f64 = 0.0;
        let mut total_unrealized: f64 = 0.0;
        let mut total_realized: f64 = 0.0;
        let mut live_priced = 0usize;
        let mut daily_known = true;
        let mut unrealized_known = true;

        for con_id in con_ids {
            let seed = seeds.get(&con_id);
            let pi = shared.portfolio.position_info(con_id);
            // Positions are fixed-point; the P&L works in shares.
            let qty_now = pi.as_ref().map(|p| p.position_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;
            let avg_cost = pi.as_ref().map(|p| p.avg_cost).unwrap_or(0);
            let qty_midnight = seed.map(|s| s.qty_midnight_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;

            // The seed's realized P&L, plus this session's fills (ibx#478).
            total_realized += seed.map(|s| s.realized_pnl).unwrap_or(0.0)
                + realized_since.get(&con_id).copied().unwrap_or(0.0);

            let money_traded = money_traded_today(seed, money_since.get(&con_id).copied(), qty_now, avg_cost);
            // A position flat at midnight and now: its daily P&L is its cash.
            if qty_now == 0.0 && qty_midnight == 0.0 {
                total_daily += money_traded;
                continue;
            }

            // Price: this client's quote, else the server's mark on the
            // position (ibx#238). The previous close comes with a quote only.
            let quote = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            let live = quote.map(|q| q.last).filter(|&p| p != 0);
            if live.is_some() {
                live_priced += 1;
            }
            let Some(price_now) = live.or_else(|| pi.as_ref().map(|p| p.market_price).filter(|&p| p != 0)) else {
                daily_known = false;
                unrealized_known = false;
                continue;
            };
            let prev_close = quote.map(|q| q.close).unwrap_or(0);

            // Daily P&L = value change since midnight plus today's net cash.
            // A position held at midnight needs the previous close.
            if qty_midnight != 0.0 && prev_close == 0 {
                daily_known = false;
            } else {
                let mv_now = qty_now * price_now as f64 / PRICE_SCALE_F;
                let mv_midnight = qty_midnight * prev_close as f64 / PRICE_SCALE_F;
                total_daily += mv_now - mv_midnight + money_traded;
            }

            if qty_now != 0.0 {
                if avg_cost != 0 {
                    total_unrealized += qty_now * (price_now - avg_cost) as f64 / PRICE_SCALE_F;
                } else {
                    unrealized_known = false;
                }
            }
        }

        // No live quote at all: the server's own account P&L keys, when it
        // sends them (ibx#239); the paper account's frames carry none.
        if live_priced == 0 {
            let rows = shared.portfolio.account_rows();
            let server = |key: &str| rows.rows.iter()
                .filter(|r| r.key == key)
                .min_by_key(|r| r.currency != "BASE")
                .and_then(|r| r.value.parse::<f64>().ok());
            if let (Some(daily), Some(unrealized), Some(realized)) =
                (server("DailyPnL"), server("UnrealizedPnL"), server("RealizedPnL"))
            {
                total_daily = daily;
                total_unrealized = unrealized;
                total_realized = realized;
                daily_known = true;
                unrealized_known = true;
            }
        }
        // A value that cannot be computed is unset, as the reference (ibx#478).
        if !daily_known {
            total_daily = f64::MAX;
        }
        if !unrealized_known {
            total_unrealized = f64::MAX;
        }

        let scaled = |v: f64| if v == f64::MAX { i64::MAX } else { (v * PRICE_SCALE_F) as i64 };
        let pnl = [scaled(total_daily), scaled(total_unrealized), scaled(total_realized)];
        // Every running request gets the values; each its own change filter.
        let mut reqs = self.pnl_reqs.lock().unwrap();
        let mut out: Vec<PnlUpdate> = reqs.iter_mut().filter_map(|(&req_id, last)| {
            if *last == pnl {
                return None;
            }
            *last = pnl;
            Some(PnlUpdate {
                req_id,
                daily_pnl: total_daily,
                unrealized_pnl: total_unrealized,
                realized_pnl: total_realized,
            })
        }).collect();
        out.sort_by_key(|u| u.req_id);
        out
    }

    /// Poll per-position PnL and return updates whose values changed.
    /// Routes the quote lookup by con_id (not first-non-zero across all subscriptions),
    /// computes daily/realized from the matching midnight seed, and synthesizes
    /// money_traded = qty_now × avg_cost for intraday-opened positions.
    pub fn poll_pnl_single(&self, shared: &SharedState) -> Vec<PnlSingleUpdate> {
        let reqs: Vec<(i64, i64)> = self.pnl_single_reqs.lock().unwrap()
            .iter().map(|(&r, &c)| (r, c)).collect();
        if reqs.is_empty() {
            return Vec::new();
        }

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        let realized_since = shared.portfolio.realized_since_seed();
        let money_since = shared.portfolio.money_since_seed();
        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut last_cache = self.last_pnl_single.lock().unwrap();
        let mut results = Vec::new();

        for (req_id, con_id) in reqs {
            let Some(pi) = shared.portfolio.position_info(con_id) else { continue; };
            // Positions are fixed-point; the P&L works in shares.
            let qty_fx = pi.position_fixed;
            let qty_now = qty_fx as f64 / QTY_SCALE_F;
            let avg_cost = pi.avg_cost;

            // Price from this client's market-data subscription when there is
            // one, else the server's own mark on the position (ibx#238). A
            // req_pnl_single-only client has no subscription, so without the
            // mark it never got a callback (same gap as ibx#239 for req_pnl).
            let q = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            let price_now = match q.map(|q| q.last) {
                Some(last) if last != 0 => last,
                _ => pi.market_price,
            };
            if price_now == 0 {
                continue;
            }

            let seed = seeds.get(&con_id);
            let qty_midnight = seed.map(|s| s.qty_midnight_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;
            let prev_close = q.map(|q| q.close).unwrap_or(0);

            let money_traded = money_traded_today(seed, money_since.get(&con_id).copied(), qty_now, avg_cost);

            let mv_now = qty_now * price_now as f64 / PRICE_SCALE_F;
            let mv_midnight = qty_midnight * prev_close as f64 / PRICE_SCALE_F;
            // A position held at midnight needs the previous close, which
            // comes with a quote only: without one, the daily P&L is unset.
            let daily = if qty_midnight != 0.0 && prev_close == 0 {
                f64::MAX
            } else {
                mv_now - mv_midnight + money_traded
            };
            // A value that cannot be computed is unset (f64::MAX), as the
            // reference (ibx#478): no average cost, or no realized P&L on
            // this position today (captured 25/09/2026).
            let unrealized = if avg_cost != 0 {
                qty_now * (price_now - avg_cost) as f64 / PRICE_SCALE_F
            } else { f64::MAX };
            let realized_fills = realized_since.get(&con_id).copied();
            let realized = match (seed.map(|s| s.realized_pnl), realized_fills) {
                (None, None) => f64::MAX,
                (seed_value, fills) => seed_value.unwrap_or(0.0) + fills.unwrap_or(0.0),
            };
            let value = mv_now;

            let scaled = |v: f64| if v == f64::MAX { i64::MAX } else { (v * PRICE_SCALE_F) as i64 };
            let snapshot: [i64; 5] = [
                qty_fx,
                scaled(daily),
                scaled(unrealized),
                scaled(realized),
                scaled(value),
            ];
            if last_cache.get(&req_id) == Some(&snapshot) {
                continue;
            }
            last_cache.insert(req_id, snapshot);

            results.push(PnlSingleUpdate {
                req_id,
                pos: qty_now,
                daily_pnl: daily,
                unrealized_pnl: unrealized,
                realized_pnl: realized,
                value,
            });
        }
        results
    }

    /// Prepare account update fields (subscription-gated, change-detected).
    /// Account values to send (ibx#475). The first batch after a subscribe
    /// is the full image, once the server's image is complete: every value
    /// the server sent, with its text and currency, then the end. Later
    /// batches carry the values that changed, and no end. `None` while not
    /// subscribed or before the image is complete.
    pub fn prepare_account_updates(&self, shared: &SharedState) -> Option<AccountUpdateBatch> {
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return None;
        }
        let (generation, complete, time_secs) = shared.portfolio.account_rows_generation();
        let mut stream = self.account_stream.lock().unwrap();
        if stream.image_pending && !complete {
            return None;
        }
        let first = stream.image_pending;
        let mut fields = Vec::new();
        if first || generation != stream.generation {
            let store = shared.portfolio.account_rows();
            for row in &store.rows {
                let id = (row.key.clone(), row.currency.clone());
                if stream.sent.get(&id) != Some(&row.value) {
                    stream.sent.insert(id, row.value.clone());
                    fields.push(AccountFieldUpdate {
                        key: row.key.clone(),
                        value: row.value.clone(),
                        currency: row.currency.clone(),
                    });
                }
            }
            stream.generation = store.generation;
        }
        stream.image_pending = false;
        Some(AccountUpdateBatch { fields, time: format_account_time(time_secs), download_end: first })
    }

    /// Prepare portfolio updates (position entries) for account streaming.
    /// Returns changed/new position infos when account updates are subscribed.
    pub fn prepare_portfolio_updates(&self, shared: &SharedState) -> Vec<PortfolioUpdateEntry> {
        // Called after `prepare_account_updates`, so only once the account
        // image is complete (ibx#475).
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return Vec::new();
        }

        let current = shared.portfolio.position_infos();
        let mut prev_guard = self.last_portfolio.lock().unwrap();
        let is_first = prev_guard.is_none();

        let to_entry = |pi: &PositionInfo| PortfolioUpdateEntry {
            con_id: pi.con_id,
            position: pi.position_fixed as f64 / QTY_SCALE_F,
            avg_cost: pi.avg_cost as f64 / PRICE_SCALE_F,
            market_price: pi.market_price as f64 / PRICE_SCALE_F,
            market_value: pi.market_value as f64 / PRICE_SCALE_F,
            unrealized_pnl: pi.unrealized_pnl as f64 / PRICE_SCALE_F,
            realized_pnl: pi.realized_pnl as f64 / PRICE_SCALE_F,
        };

        let changed = if is_first {
            current.iter().map(&to_entry).collect()
        } else {
            let prev = prev_guard.as_ref().unwrap();
            // Marks are part of the row: a mark move (each account-updates
            // snapshot) is a genuine update, so compare them too (ibx#238).
            current.iter().filter(|pi| {
                !prev.iter().any(|pp| pp.con_id == pi.con_id
                    && pp.position_fixed == pi.position_fixed
                    && pp.avg_cost == pi.avg_cost
                    && pp.market_price == pi.market_price
                    && pp.market_value == pi.market_value
                    && pp.unrealized_pnl == pi.unrealized_pnl
                    && pp.realized_pnl == pi.realized_pnl)
            }).map(&to_entry).collect()
        };

        *prev_guard = Some(current);
        changed
    }

    /// Account summary rows and ends the server sent, by request (ibx#479):
    /// every row of the request's subscription, with its text and currency;
    /// ledger rows only for the currency the request chose; the end at each
    /// end marker. Rows of a cancelled subscription are dropped.
    pub fn prepare_account_summary(&self, shared: &SharedState) -> Vec<AccountSummaryBatch> {
        let events = shared.portfolio.drain_account_summary_events();
        if events.is_empty() {
            return Vec::new();
        }
        let reqs = self.account_summaries.lock().unwrap();
        let mut out = Vec::new();
        for event in events {
            let Some(req) = reqs.iter().find(|r| r.sr_id == event.sr_id) else { continue };
            let rows = if event.ledger {
                event.rows.into_iter().filter(|row| match &req.ledger {
                    LedgerChoice::None => false,
                    LedgerChoice::Base => row.currency == "BASE",
                    LedgerChoice::Currency(ccy) => &row.currency == ccy,
                    LedgerChoice::All => true,
                }).collect()
            } else {
                event.rows
            };
            out.push(AccountSummaryBatch { req_id: req.req_id, rows, end: event.end });
        }
        out
    }

    // ── Order routing ──

    /// The name ibx uses for an order type the reference also accepts under
    /// another name (ibx#469), or None when the name is not one of these.
    pub fn canonical_order_type(order_type: &str) -> Option<&'static str> {
        ORDER_TYPE_ALIASES.iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(order_type))
            .map(|&(_, name)| name)
    }

    /// The order with its type under the name ibx uses (ibx#469). Copied
    /// only when the type is one of the other names.
    pub fn with_canonical_order_type(order: &ApiOrder) -> std::borrow::Cow<'_, ApiOrder> {
        match Self::canonical_order_type(&order.order_type) {
            Some(name) => std::borrow::Cow::Owned(ApiOrder { order_type: name.to_string(), ..order.clone() }),
            None => std::borrow::Cow::Borrowed(order),
        }
    }

    /// Pre-validate order fields that don't depend on instrument ID.
    /// Call this before `find_or_register_instrument` to fail fast.
    pub fn validate_order(order: &ApiOrder) -> Result<(), String> {
        order.side()?;

        // transmit=false cannot be honoured: every order is sent to the
        // broker immediately when place_order is called; there is no
        // staging concept. Accepting it would send a "staged" bracket
        // parent live on its own, so reject loudly at the call instead.
        // See: https://github.com/deepentropy/ibx/issues/226
        if !order.transmit {
            return Err(
                "transmit=false is not supported: orders are transmitted \
                 immediately on place_order; there is no staging concept, so \
                 the order would go live despite transmit=false. Place child \
                 orders with parent_id/oca_group set and keep transmit=true \
                 (the engine links them server-side)."
                    .into(),
            );
        }

        // An unrecognized tif would otherwise be sent as DAY silently.
        match order.tif.as_str() {
            "" | "DAY" | "GTC" | "IOC" | "FOK" | "OPG" | "GTD" | "DTC" | "AUC" => {}
            other => {
                return Err(format!(
                    "Unsupported tif '{}': use DAY, GTC, IOC, FOK, OPG, GTD, DTC or AUC",
                    other
                ));
            }
        }

        let order_type = order.order_type.to_uppercase();

        // These order types carry a type-specific instruction in the same
        // slot all-or-none uses, so the two cannot be combined.
        if order.all_or_none && matches!(order_type.as_str(), "TRAIL" | "REL") {
            return Err(format!(
                "all_or_none is not supported with {} orders",
                order.order_type
            ));
        }

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
            | "BOX TOP" => {}
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        }

        // Reject orders that require aux_price when it is zero — prevents silent no-trigger bugs.
        // See: https://github.com/deepentropy/ibx/issues/115
        match order_type.as_str() {
            "STP" | "STP PRT" | "MIT" if order.aux_price == 0.0 => {
                return Err(format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0 — \
                     set aux_price to the desired trigger price, not lmt_price",
                    order.order_type
                ));
            }
            "STP LMT" | "LIT" if order.aux_price == 0.0 => {
                return Err(format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0",
                    order.order_type
                ));
            }
            "TRAIL" if order.trailing_percent == 0.0 && order.aux_price == 0.0 => {
                return Err(
                    "TRAIL order requires either trailing_percent or aux_price (trail amount) \
                     but both are 0.0".into()
                );
            }
            "TRAIL LIMIT" if order.aux_price == 0.0 => {
                return Err(
                    "TRAIL LIMIT order requires aux_price (trail amount) but got 0.0".into()
                );
            }
            _ => {}
        }

        Ok(())
    }

    /// Validate historical-request arguments before anything reaches the
    /// engine (ibx#232): an unrecognized bar_size previously fell back to
    /// 5-minute bars silently (via TWO divergent tables), and an
    /// unrecognized what_to_show fell back to TRADES. The caller gets a
    /// synchronous Err at the call instead of plausible, wrong candles.
    pub fn validate_historical_args(
        bar_size: &str,
        what_to_show: &str,
        keep_up_to_date: bool,
    ) -> Result<(), String> {
        let bs = crate::control::historical::BarSize::from_api_str(bar_size)?;
        crate::control::historical::BarDataType::from_api_str(what_to_show)?;
        if keep_up_to_date && !bs.supports_keep_up_to_date() {
            return Err(format!(
                "bar_size '{}' is not supported with keep_up_to_date=true: \
                 supported sizes are 1 secs, 5 secs, 5 mins, 1 hour, 1 day",
                bar_size,
            ));
        }
        Ok(())
    }

    /// Reject orders whose contract is not a common stock.
    ///
    /// The outbound order encoding in `engine::hot_loop::order_builder` only
    /// supports common stock, and the instrument registry drops
    /// `sec_type`/`exchange`. A non-STK contract (OPT/FUT/BAG/…) would
    /// therefore be sent as a stock order on the underlying symbol with no
    /// error surfaced. Until non-STK encoding lands, reject those contracts
    /// up front.
    ///
    /// An empty `sec_type` is treated as STK (the engine default), so existing
    /// stock callers that omit the field are unaffected.
    /// See: https://github.com/deepentropy/ibx/issues/202
    pub fn validate_order_contract(sec_type: &str) -> Result<(), String> {
        if sec_type.is_empty() || sec_type.eq_ignore_ascii_case("STK") {
            return Ok(());
        }
        Err(format!(
            "Unsupported contract sec_type '{}': only STK orders are supported. \
             Non-STK contracts (OPT/FUT/BAG/…) are not yet wire-encoded and would \
             otherwise be silently sent as a stock order on the underlying symbol. \
             See https://github.com/deepentropy/ibx/issues/202",
            sec_type
        ))
    }

    /// The reference refuses a fractional quantity before sending anything,
    /// with error 10243 (ib-agent#192 B3). ibx used to cut it to a whole
    /// number and send it, so 1.5 shares went out as 1 (ibx#313).
    pub fn fractional_quantity_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        if order.total_quantity.fract() != 0.0 {
            Some((10243, "Fractional-sized order cannot be placed via API. \
                Please use desktop version to place this order.".to_string()))
        } else {
            None
        }
    }

    /// Every order the reference refuses before sending anything, with its
    /// error code and text. Nothing is sent for such an order.
    pub fn refusal_before_sending(order: &ApiOrder) -> Option<(i64, String)> {
        Self::fractional_quantity_refusal(order)
            .or_else(|| Self::algo_param_refusal(order))
            .or_else(|| Self::good_after_time_refusal(order))
            .or_else(|| Self::order_rule_refusal(order))
    }

    /// A goodAfterTime that is not a date and time: error 337 with the
    /// reference's text, whose label for this field is "Start Time"
    /// (ibx#467). ibx needs the date; the reference also takes a time alone
    /// (today assumed).
    fn good_after_time_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        match crate::config::parse_ib_expiry(&order.good_after_time) {
            Ok(None) | Ok(Some(crate::config::IbExpiry::Instant(_))) => None,
            _ => Some((337, INVALID_DATE_TIME.replace("%s", "Start Time"))),
        }
    }

    /// Order rules the reference checks before sending, answered as error
    /// 321 with the rule text (ibx#468). The text after "cause - " is the
    /// rule's; for the TRAIL LIMIT rule only its end is known.
    fn order_rule_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        let refuse = |cause: &str| Some((321, format!("Error validating request.-'bH' : cause - {}", cause)));
        let order_type = order.order_type.to_uppercase();
        // Midprice outside regular hours: refused whatever the time of day
        // (the flag alone, reference refusal 10210).
        if matches!(order_type.as_str(), "MIDPRICE" | "MIDPX") && order.outside_rth {
            return refuse("Midprice orders are not supported outside of regular trading hours.");
        }
        // TRAIL LIMIT: exactly one of lmtPrice and lmtPriceOffset (also on a
        // replace: sending back the computed lmtPrice with the offset was
        // refused, captured 23/09/2026). lmtPrice is unset at 0 or MAX,
        // lmtPriceOffset at MAX.
        // A TRAIL LIMIT needs its stop price (trailStopPrice), checked
        // before the limit fields (ib-agent#194, no final period).
        if order_type == "TRAIL LIMIT" && (order.trail_stop_price == f64::MAX || order.trail_stop_price == 0.0) {
            return refuse("Please enter a stop price");
        }
        if order_type == "TRAIL LIMIT" {
            let price_set = order.lmt_price != 0.0 && order.lmt_price != f64::MAX;
            let offset_set = order.lmt_price_offset != f64::MAX;
            if price_set == offset_set {
                return refuse("You must specify one value: limit price or limit price offset value.");
            }
        }
        None
    }

    /// Algo parameter values the reference refuses before sending
    /// (ib-agent#192 B10). ibx used to turn them into defaults: an unknown
    /// choice became Normal or Neutral, a negative percentage went out
    /// (ibx#263). Only the rules the capture showed; a value that does not
    /// parse as a number is left to the existing handling.
    fn algo_param_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        if order.algo_strategy.is_empty() {
            return None;
        }
        let limit = |label: &str, what: &str, bound: &str| {
            Some((441, format!(
                "Algo attributes validation failed: '{}' is invalid: Value is {} {}.. ", label, what, bound)))
        };
        for tv in &order.algo_params {
            let value = tv.value.as_str();
            if value.is_empty() {
                continue;
            }
            let known_choice = match tv.tag.as_str() {
                "adaptivePriority" => Some(matches!(value, "Urgent" | "Normal" | "Patient")),
                "riskAversion" => Some(matches!(value.to_lowercase().as_str(),
                    "get_done" | "getdone" | "aggressive" | "neutral" | "passive")),
                _ => None,
            };
            if known_choice == Some(false) {
                return Some((145, format!("Error in validating entry fields -{}", value)));
            }
            let Ok(number) = value.parse::<f64>() else { continue };
            match tv.tag.as_str() {
                // A NaN fails the upper bound check in the reference.
                "maxPctVol" if number.is_nan() || number > 50.0 =>
                    return limit("Max Percentage", "greater than maximum value", "50.0"),
                "maxPctVol" if number < 0.01 =>
                    return limit("Max Percentage", "less than minimum value", "0.01"),
                "pctVol" if number < 0.01 =>
                    return limit("Target Percentage", "less than minimum value", "0.01"),
                _ => {}
            }
        }
        None
    }

    /// Status to report with a fill that leaves part of the order open.
    /// The reference keeps the order's working status on a fill and has no
    /// partially-filled status: a fill on an order last reported as
    /// PreSubmitted stays PreSubmitted, otherwise Submitted (ib-agent#192 C8).
    pub fn partial_fill_status(&self, order_id: u64) -> &'static str {
        match self.open_orders.lock().unwrap().get(&order_id).map(|t| t.status.as_str()) {
            Some("PreSubmitted") => "PreSubmitted",
            _ => "Submitted",
        }
    }

    /// Order type of a tracked order, as the caller placed it.
    pub fn tracked_order_type(&self, order_id: u64) -> Option<String> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.order.order_type.clone())
    }

    /// The order kind with its prices, as the extended submit path builds it
    /// from the same `Order` fields. Used for a replace, which restates the
    /// order type and its prices (ibx#247).
    pub fn order_kind(order: &ApiOrder) -> Result<OrderKind, String> {
        let scale = |v: f64| (v * PRICE_SCALE_F) as i64;
        if !order.adjusted_order_type.is_empty() {
            let adjusted = match order.adjusted_order_type.to_uppercase().as_str() {
                "STP" => AdjustedOrderType::Stop,
                "STP LMT" => AdjustedOrderType::StopLimit,
                "TRAIL" => AdjustedOrderType::Trail,
                "TRAIL LIMIT" => AdjustedOrderType::TrailLimit,
                other => return Err(format!("unknown adjustedOrderType '{}'", other)),
            };
            let adj_trail = if order.adjusted_trailing_amount == f64::MAX {
                0.0
            } else {
                order.adjusted_trailing_amount
            };
            return Ok(OrderKind::AdjustableStop {
                stop_price: scale(order.aux_price),
                trigger_price: scale(order.trigger_price),
                adjusted_order_type: adjusted,
                adjusted_stop_price: scale(order.adjusted_stop_price),
                adjusted_stop_limit_price: scale(order.adjusted_stop_limit_price),
                adjusted_trailing_amount: scale(adj_trail),
                adjustable_trailing_unit: order.adjustable_trailing_unit,
            });
        }
        let trail_stop = if order.trail_stop_price == f64::MAX { 0 } else { scale(order.trail_stop_price) };
        Ok(match order.order_type.to_uppercase().as_str() {
            "MKT" => OrderKind::Market,
            "LMT" => OrderKind::Limit { price: scale(order.lmt_price) },
            "STP" => OrderKind::Stop { stop_price: scale(order.aux_price) },
            "STP LMT" => OrderKind::StopLimit {
                price: scale(order.lmt_price), stop_price: scale(order.aux_price),
            },
            "TRAIL" => {
                if order.trailing_percent > 0.0 {
                    OrderKind::TrailPct {
                        trail_pct: (order.trailing_percent * 100.0).round() as u32,
                        trail_stop_price: trail_stop,
                    }
                } else {
                    OrderKind::TrailingStop { trail_amt: scale(order.aux_price), trail_stop_price: trail_stop }
                }
            }
            "TRAIL LIMIT" => {
                // Exactly one of the two (refused otherwise, ibx#468): the
                // offset, or the absolute limit price (ib-agent#194).
                let (lmt_offset, lmt_price) = if order.lmt_price_offset != f64::MAX {
                    (scale(order.lmt_price_offset), None)
                } else {
                    (0, Some(scale(order.lmt_price)))
                };
                OrderKind::TrailingStopLimit {
                    lmt_offset,
                    lmt_price,
                    trail_amt: scale(order.aux_price),
                    trail_stop_price: trail_stop,
                }
            }
            "MOC" => OrderKind::Moc,
            "LOC" => OrderKind::Loc { price: scale(order.lmt_price) },
            "MIT" => OrderKind::Mit { stop_price: scale(order.aux_price) },
            "LIT" => OrderKind::Lit { price: scale(order.lmt_price), stop_price: scale(order.aux_price) },
            "MTL" | "BOX TOP" => OrderKind::Mtl,
            "MKT PRT" => OrderKind::MktPrt,
            "STP PRT" => OrderKind::StpPrt { stop_price: scale(order.aux_price) },
            "REL" => OrderKind::Rel { offset: scale(order.aux_price) },
            "PEG MKT" => OrderKind::PegMkt { offset: scale(order.aux_price) },
            "PEG MID" | "PEG MIDPT" => OrderKind::PegMid { offset: scale(order.aux_price) },
            "MIDPX" | "MIDPRICE" => OrderKind::MidPrice { price_cap: scale(order.lmt_price) },
            "SNAP MKT" => OrderKind::SnapMkt,
            "SNAP MID" | "SNAP MIDPT" => OrderKind::SnapMid,
            "SNAP PRI" | "SNAP PRIM" => OrderKind::SnapPri,
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        })
    }

    /// Build the replace for an order that is already working, from the full
    /// `Order` the caller passed (ibx#247 ibx#324 ibx#334 ibx#349).
    ///
    /// The reference refuses a change of order type before sending anything
    /// (error 329, ib-agent#192 A4b); so does this.
    pub fn build_modify_request(
        order: &ApiOrder,
        order_id: u64,
        working_order_type: &str,
    ) -> Result<ModifyPlan, String> {
        if !working_order_type.eq_ignore_ascii_case(&order.order_type) {
            return Ok(ModifyPlan::Refused {
                code: 329,
                message: format!(
                    "Order modify failed. Cannot change to the new order type.{}",
                    order.order_type.to_uppercase(),
                ),
            });
        }
        Ok(ModifyPlan::Send(ControlCommand::Order(OrderRequest::Modify {
            new_order_id: order_id,
            order_id,
            qty: order.total_quantity as u32,
            kind: Self::order_kind(order)?,
            tif: order.tif_byte(),
            attrs: order.attrs(),
        })))
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
                tif: order.tif_byte(), attrs: order.attrs(),
            }));
        }

        // Algo orders
        if !order.algo_strategy.is_empty() {
            let algo = crate::api::client::parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            return Ok(ControlCommand::Order(OrderRequest::SubmitAlgo {
                order_id, instrument, side, qty, price, algo,
                tif: order.tif_byte(), attrs: order.attrs(),
            }));
        }

        // What-if orders
        if order.what_if {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            return Ok(ControlCommand::Order(OrderRequest::SubmitWhatIf {
                order_id, instrument, side, qty, price,
                tif: order.tif_byte(), attrs: order.attrs(),
            }));
        }

        // Every order type must carry extended attributes and a non-DAY tif
        // when the caller sets them — dropping them silently produced
        // unlinked, immediate-DAY bracket children (ibx#224). An empty tif
        // is treated as DAY, matching the official API default.
        let extended = order.has_extended_attrs()
            || !matches!(order.tif.as_str(), "" | "DAY");
        let ex = |kind: OrderKind| OrderRequest::SubmitEx {
            order_id, instrument, side, qty,
            kind,
            tif: order.tif_byte(),
            attrs: order.attrs(),
        };

        // Adjustable stop: a base STP that converts to another order type when
        // its trigger is reached. Signalled by a non-empty adjustedOrderType,
        // which is empty on every ordinary order, so this affects nothing else.
        // A Trail/TrailLimit conversion carries the trailing amount + unit
        // (tags 6260/6269, ib-agent#167). (ibx#225)
        // With extended attributes or a non-DAY tif it takes the extended path,
        // so a bracket child keeps its parent, OCA group and tif (ibx#240).
        if !order.adjusted_order_type.is_empty() {
            let adjusted = match order.adjusted_order_type.to_uppercase().as_str() {
                "STP" => AdjustedOrderType::Stop,
                "STP LMT" => AdjustedOrderType::StopLimit,
                "TRAIL" => AdjustedOrderType::Trail,
                "TRAIL LIMIT" => AdjustedOrderType::TrailLimit,
                other => return Err(format!("unknown adjustedOrderType '{}'", other)),
            };
            let scale = |v: f64| (v * PRICE_SCALE_F) as i64;
            // adjusted_trailing_amount defaults to f64::MAX when unset.
            let adj_trail = if order.adjusted_trailing_amount == f64::MAX {
                0.0
            } else {
                order.adjusted_trailing_amount
            };
            let stop_price = scale(order.aux_price);
            let trigger_price = scale(order.trigger_price);
            let adjusted_stop_price = scale(order.adjusted_stop_price);
            let adjusted_stop_limit_price = scale(order.adjusted_stop_limit_price);
            let adjusted_trailing_amount = scale(adj_trail);
            let adjustable_trailing_unit = order.adjustable_trailing_unit;
            let req = if extended {
                ex(OrderKind::AdjustableStop {
                    stop_price, trigger_price, adjusted_order_type: adjusted,
                    adjusted_stop_price, adjusted_stop_limit_price,
                    adjusted_trailing_amount, adjustable_trailing_unit,
                })
            } else {
                OrderRequest::SubmitAdjustableStop {
                    order_id, instrument, side, qty,
                    stop_price, trigger_price, adjusted_order_type: adjusted,
                    adjusted_stop_price, adjusted_stop_limit_price,
                    adjusted_trailing_amount, adjustable_trailing_unit,
                }
            };
            return Ok(ControlCommand::Order(req));
        }

        let req = match order_type.as_str() {
            "MKT" => {
                if extended { ex(OrderKind::Market) }
                else { OrderRequest::SubmitMarket { order_id, instrument, side, qty } }
            }
            "LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                if extended {
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
                if extended { ex(OrderKind::Stop { stop_price: stop }) }
                else { OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price: stop } }
            }
            "STP LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::StopLimit { price, stop_price: stop }) }
                else { OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price: stop } }
            }
            "TRAIL" => {
                // Optional initial stop trigger (tag 6117); default f64::MAX = unset.
                let trail_stop = if order.trail_stop_price == f64::MAX { 0 } else { (order.trail_stop_price * PRICE_SCALE_F) as i64 };
                if order.trailing_percent > 0.0 {
                    let pct = (order.trailing_percent * 100.0).round() as u32;
                    if extended {
                        OrderRequest::SubmitTrailingStopPctEx {
                            order_id, instrument, side, qty, trail_pct: pct,
                            tif: order.tif_byte(),
                            attrs: order.attrs(),
                            trail_stop_price: trail_stop,
                        }
                    } else {
                        OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct: pct, trail_stop_price: trail_stop }
                    }
                } else {
                    let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                    if extended { ex(OrderKind::TrailingStop { trail_amt: trail, trail_stop_price: trail_stop }) }
                    else { OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt: trail, trail_stop_price: trail_stop } }
                }
            }
            "TRAIL LIMIT" => {
                // The offset (6370), or the absolute limit price (44, no
                // 6370), as the caller gave it (ib-agent#194): exactly one of
                // them, else refused before this (ibx#468).
                let (lmt_offset, lmt_price) = if order.lmt_price_offset != f64::MAX {
                    ((order.lmt_price_offset * PRICE_SCALE_F) as i64, None)
                } else {
                    (0, Some((order.lmt_price * PRICE_SCALE_F) as i64))
                };
                let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                let trail_stop = if order.trail_stop_price == f64::MAX { 0 } else { (order.trail_stop_price * PRICE_SCALE_F) as i64 };
                if extended { ex(OrderKind::TrailingStopLimit { lmt_offset, lmt_price, trail_amt: trail, trail_stop_price: trail_stop }) }
                else { OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, lmt_offset, lmt_price, trail_amt: trail, trail_stop_price: trail_stop } }
            }
            "MOC" => {
                if extended { ex(OrderKind::Moc) }
                else { OrderRequest::SubmitMoc { order_id, instrument, side, qty } }
            }
            "LOC" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::Loc { price }) }
                else { OrderRequest::SubmitLoc { order_id, instrument, side, qty, price } }
            }
            "MIT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::Mit { stop_price: stop }) }
                else { OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price: stop } }
            }
            "LIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::Lit { price, stop_price: stop }) }
                else { OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price: stop } }
            }
            "MTL" | "BOX TOP" => {
                if extended { ex(OrderKind::Mtl) }
                else { OrderRequest::SubmitMtl { order_id, instrument, side, qty } }
            }
            "MKT PRT" => {
                if extended { ex(OrderKind::MktPrt) }
                else { OrderRequest::SubmitMktPrt { order_id, instrument, side, qty } }
            }
            "STP PRT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::StpPrt { stop_price: stop }) }
                else { OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price: stop } }
            }
            "REL" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::Rel { offset }) }
                else { OrderRequest::SubmitRel { order_id, instrument, side, qty, offset } }
            }
            "PEG MKT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::PegMkt { offset }) }
                else { OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, offset } }
            }
            "PEG MID" | "PEG MIDPT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::PegMid { offset }) }
                else { OrderRequest::SubmitPegMid { order_id, instrument, side, qty, offset } }
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = (order.lmt_price * PRICE_SCALE_F) as i64;
                if extended { ex(OrderKind::MidPrice { price_cap: cap }) }
                else { OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap: cap } }
            }
            "SNAP MKT" => {
                if extended { ex(OrderKind::SnapMkt) }
                else { OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty } }
            }
            "SNAP MID" | "SNAP MIDPT" => {
                if extended { ex(OrderKind::SnapMid) }
                else { OrderRequest::SubmitSnapMid { order_id, instrument, side, qty } }
            }
            "SNAP PRI" | "SNAP PRIM" => {
                if extended { ex(OrderKind::SnapPri) }
                else { OrderRequest::SubmitSnapPri { order_id, instrument, side, qty } }
            }
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        };

        Ok(ControlCommand::Order(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SmartComponent;

    fn shared_with_components(comps: Vec<(i32, &str)>) -> SharedState {
        let s = SharedState::new();
        s.reference.set_smart_components(
            comps.into_iter().map(|(bit, letter)| SmartComponent {
                bit_number: bit,
                exchange: format!("EX{bit}"),
                exchange_letter: letter.to_string(),
            }).collect()
        );
        s
    }

    #[test]
    fn render_exchange_mask_zero_is_empty() {
        let s = shared_with_components(vec![(0, "Q"), (1, "N")]);
        assert_eq!(render_exchange_mask(0, &s), "");
    }

    #[test]
    fn render_exchange_mask_single_bit() {
        let s = shared_with_components(vec![(0, "Q"), (1, "N"), (2, "P")]);
        assert_eq!(render_exchange_mask(0b001, &s), "Q");
        assert_eq!(render_exchange_mask(0b100, &s), "P");
    }

    #[test]
    fn render_exchange_mask_multiple_bits() {
        let s = shared_with_components(vec![
            (0, "Q"), (1, "N"), (2, "P"), (3, "Z"),
        ]);
        // bits 0, 2, 3 set → letters in bit-order: Q, P, Z
        assert_eq!(render_exchange_mask(0b1101, &s), "QPZ");
    }

    #[test]
    fn render_exchange_mask_unknown_bit_skipped() {
        let s = shared_with_components(vec![(0, "Q")]);
        // bit 5 set, no component at bit 5 — skipped
        assert_eq!(render_exchange_mask(0b100000, &s), "");
    }

    // ── poll_pnl regression tests (#166) ──

    fn seed_pnl_position(
        core: &ClientCore,
        shared: &SharedState,
        con_id: i64,
        iid: InstrumentId,
        position: i64,
        avg_cost_dollars: f64,
        last_dollars: f64,
        close_dollars: f64,
    ) {
        core.con_id_to_instrument.lock().unwrap().insert(con_id, iid);
        core.instrument_to_req.lock().unwrap().insert(iid, 1);
        shared.portfolio.set_position_info(PositionInfo {
            con_id,
            position_fixed: position as i64 * crate::types::QTY_SCALE,
            avg_cost: (avg_cost_dollars * PRICE_SCALE_F) as i64,
            symbol: format!("SYM{con_id}"),
            sec_type: "STK".into(),
            currency: "USD".into(),
            multiplier: String::new(),
            ..Default::default()
        });
        let mut q = Quote::default();
        q.last = (last_dollars * PRICE_SCALE_F) as i64;
        q.close = (close_dollars * PRICE_SCALE_F) as i64;
        shared.market.push_quote(iid, &q);
    }

    #[test]
    fn poll_pnl_no_subscription_returns_none() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        assert!(core.poll_pnl(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_intraday_opened_position_fires_callback() {
        // #166: flat-at-midnight account opens an intraday position.
        // Before fix: poll_pnl early-returned on empty seeds → no callback.
        // After fix: position iterated, money_traded synthesized, daily P&L = unrealized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(42);

        // 1 share bought at $735.00, now $735.07. No midnight seed (flat at midnight).
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        assert_eq!(update.req_id, 42);
        assert!((update.daily_pnl - 0.07).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 0.07).abs() < 1e-6);
        assert!((update.realized_pnl - 0.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_overnight_position_with_seed_unchanged() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(99);

        // Held 10 SPY through midnight: qty_midnight=10, prev_close=$730, avg_cost=$700.
        // No fills today (money_traded=0). Current price $735.
        seed_pnl_position(&core, &shared, 756733, 0, 10, 700.00, 735.00, 730.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 0.0,
            realized_pnl: 0.0,
        }]);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        // daily = 10×735 - 10×730 - 0 = 50
        assert!((update.daily_pnl - 50.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        // unrealized = 10 × (735 - 700) = 350
        assert!((update.unrealized_pnl - 350.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_seeded_position_traded_intraday_uses_signed_net_cash() {
        // ibx#221 / ib-agent#163: a position held at midnight AND traded intraday
        // carries a non-zero moneyTradedSinceMidnight (6822), signed SELL+/BUY-.
        // The daily formula must ADD it. Sold 3 of 10 at $110 (avg $100): the
        // seed carries +330 net cash (sell proceeds) and +30 realized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(31);

        // Now holding 7 (was 10 at midnight), avg $100, last $110, prev close $100.
        seed_pnl_position(&core, &shared, 1, 0, 7, 100.00, 110.00, 100.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 1,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 330.0,   // +330 = sold 3 @ $110 (wire sign, SELL positive)
            realized_pnl: 30.0,
        }]);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        // daily = 7×110 - 10×100 + 330 = 100 (70 remaining unrealized + 30 realized)
        assert!((update.daily_pnl - 100.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        // unrealized = 7 × (110 - 100) = 70
        assert!((update.unrealized_pnl - 70.0).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
        assert!((update.realized_pnl - 30.0).abs() < 1e-6, "real={}", update.realized_pnl);
    }

    #[test]
    fn poll_pnl_change_detection_suppresses_duplicate() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(7);
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        assert!(!core.poll_pnl(&shared).is_empty());
        // Same inputs → no callback.
        assert!(core.poll_pnl(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_without_market_data_uses_marks_and_unset() {
        // The paper case (25/09/2026): no quote, no server P&L keys. The
        // unrealized P&L comes from the server's mark; the realized P&L from
        // the seed; the daily P&L of a position held at midnight needs the
        // previous close, so it is unset (it was 0.0 for all three).
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733, position_fixed: 31 * crate::types::QTY_SCALE,
            avg_cost: (764.59 * PRICE_SCALE_F) as i64, ..Default::default()
        });
        shared.portfolio.set_position_marks(756733, (770.0 * PRICE_SCALE_F) as i64, 0, 0, 0);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 37.12,
        }]);
        let u = core.poll_pnl(&shared).pop().unwrap();
        assert_eq!(u.daily_pnl, f64::MAX);
        assert!((u.unrealized_pnl - 31.0 * (770.0 - 764.59)).abs() < 1e-3, "unreal={}", u.unrealized_pnl);
        assert!((u.realized_pnl - 37.12).abs() < 1e-9);
    }

    // The same inputs give the same P&L, whatever the order of the positions
    // in the stores: no repeated callback.
    #[test]
    fn poll_pnl_does_not_repeat_unchanged_values() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);
        for con_id in 1..40 {
            shared.portfolio.set_position_info(PositionInfo {
                con_id, position_fixed: crate::types::QTY_SCALE / 3,
                avg_cost: (100.1 * PRICE_SCALE_F) as i64, ..Default::default()
            });
            shared.portfolio.set_position_marks(con_id, (100.7 * PRICE_SCALE_F) as i64, 0, 0, 0);
            shared.portfolio.add_realized_since_seed(con_id, 0.1);
        }
        assert_eq!(core.poll_pnl(&shared).len(), 1);
        for _ in 0..50 {
            assert!(core.poll_pnl(&shared).is_empty());
        }
    }

    #[test]
    fn pnl_single_without_market_data_sends_the_position_with_daily_unset() {
        // It used to skip a position held at midnight when there was no
        // quote: no callback at all.
        let core = ClientCore::new();
        let shared = SharedState::new();
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733, position_fixed: 31 * crate::types::QTY_SCALE,
            avg_cost: (764.59 * PRICE_SCALE_F) as i64, ..Default::default()
        });
        shared.portfolio.set_position_marks(756733, (770.0 * PRICE_SCALE_F) as i64, 0, 0, 0);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 37.12,
        }]);
        core.subscribe_pnl_single(4, 756733);
        let u = core.poll_pnl_single(&shared).pop().expect("a callback");
        assert_eq!(u.daily_pnl, f64::MAX);
        assert!((u.unrealized_pnl - 31.0 * (770.0 - 764.59)).abs() < 1e-3);
        assert!((u.realized_pnl - 37.12).abs() < 1e-9);
        assert!((u.value - 31.0 * 770.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_falls_back_to_account_level_without_market_data() {
        // #239: a req_pnl-only client never subscribes to market data, so no
        // position has a live quote (con_id_to_instrument is empty and every
        // position hits `continue`). poll_pnl must then emit the gateway's
        // account-level P&L instead of returning None forever.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);

        // Open position, but NO instrument mapping and NO quote pushed.
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733,
            position_fixed: (10) as i64 * crate::types::QTY_SCALE,
            avg_cost: (700.00 * PRICE_SCALE_F) as i64,
            symbol: "SPY".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            multiplier: String::new(),
            ..Default::default()
        });

        // Server-sent account-level P&L keys (the account values as sent,
        // ibx#475); only used when the server sends them.
        shared.portfolio.update_account_rows(|store| {
            store.set("DailyPnL", "BASE", "12.50");
            store.set("UnrealizedPnL", "BASE", "35.00");
            store.set("RealizedPnL", "BASE", "4.00");
        });

        let update = core.poll_pnl(&shared).pop().expect("callback must fire from account-level P&L");
        assert_eq!(update.req_id, 21);
        assert!((update.daily_pnl - 12.50).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 35.00).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
        assert!((update.realized_pnl - 4.00).abs() < 1e-6, "real={}", update.realized_pnl);
    }

    #[test]
    fn poll_pnl_prefers_quotes_over_account_level_when_priced() {
        // When market data IS subscribed, the per-position quote synthesis wins;
        // the account-level fallback must not override it.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(22);

        // Priced position: 1 share, avg 100, last 101 → daily/unrealized = 1.00.
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);

        // Divergent account-level values that must be ignored while priced.
        let mut acct = AccountState::default();
        acct.daily_pnl = (999.0 * PRICE_SCALE_F) as i64;
        acct.unrealized_pnl = (999.0 * PRICE_SCALE_F) as i64;
        shared.portfolio.set_account(&acct);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        assert!((update.daily_pnl - 1.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 1.0).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
    }

    // ── poll_pnl_single regression tests (#168) ──

    #[test]
    fn poll_pnl_single_routes_quote_by_con_id() {
        // #168 (bug 3): two subscribed instruments, different prices — each req_id
        // must see the price of its own con_id, not the first non-zero quote.
        let core = ClientCore::new();
        let shared = SharedState::new();

        seed_pnl_position(&core, &shared, 111, 0, 1, 100.0, 105.0, 0.0);  // SPY
        seed_pnl_position(&core, &shared, 222, 1, 1, 200.0, 210.0, 0.0);  // QQQ

        core.subscribe_pnl_single(50, 111);
        core.subscribe_pnl_single(51, 222);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 2);

        let spy = updates.iter().find(|u| u.req_id == 50).expect("SPY update");
        let qqq = updates.iter().find(|u| u.req_id == 51).expect("QQQ update");
        // Unrealized = qty × (last - avg_cost). SPY: 1×(105-100)=5; QQQ: 1×(210-200)=10.
        assert!((spy.unrealized_pnl - 5.0).abs() < 1e-6);
        assert!((qqq.unrealized_pnl - 10.0).abs() < 1e-6);
        // Value = qty × last. SPY: 105; QQQ: 210.
        assert!((spy.value - 105.0).abs() < 1e-6);
        assert!((qqq.value - 210.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_single_intraday_opened_position() {
        // #168 (bug 1): daily_pnl must be computed, not hardcoded 0.
        // No seed → money_traded synthesized, daily collapses to unrealized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        assert_eq!(u.req_id, 42);
        assert!((u.daily_pnl - 0.07).abs() < 1e-6, "daily={}", u.daily_pnl);
        assert!((u.unrealized_pnl - 0.07).abs() < 1e-6);
        // No realized P&L on this position today: unset, as the reference
        // (captured 25/09/2026: realized 1.7976931348623157e+308; ibx#478).
        assert_eq!(u.realized_pnl, f64::MAX);
    }

    // ibx#478: the realized P&L of this session's fills adds to the seed's;
    // a new seed includes them.
    #[test]
    fn poll_pnl_single_adds_the_realized_pnl_of_fills() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);
        shared.portfolio.add_realized_since_seed(756733, 5.645252);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert!((u.realized_pnl - 5.645252).abs() < 1e-9);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 0, money_traded: 0.0, realized_pnl: 5.645252,
        }]);
        let _ = core.poll_pnl_single(&shared);
        assert!(shared.portfolio.realized_since_seed().is_empty(), "the new seed includes the fill");
    }

    // The paper case of 25/09/2026: 31 SPY held overnight, then BUY 1 at the
    // last price. The daily P&L must not move by the trade value (it jumped
    // by about 770 when the fill's cash was left out).
    #[test]
    fn a_fill_does_not_move_the_daily_pnl_by_its_value() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 31, 764.59, 770.00, 760.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 0.0,
        }]);
        core.subscribe_pnl_single(4, 756733);
        let before = core.poll_pnl_single(&shared).pop().unwrap().daily_pnl;
        assert!((before - 310.0).abs() < 1e-6, "31 × (770 - 760): {before}");

        seed_pnl_position(&core, &shared, 756733, 0, 32, 764.78, 770.00, 760.00);
        shared.portfolio.add_money_since_seed(756733, -770.0);
        let after = core.poll_pnl_single(&shared).pop().unwrap().daily_pnl;
        assert!((after - 310.0).abs() < 1e-6, "bought at the last price: no change, got {after}");
    }

    // No seed row, fills of this session only: the daily P&L is exact from
    // their cash (buy 1 at 100, sell 1 at 103: +3).
    #[test]
    fn daily_pnl_of_a_position_traded_this_session_only() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 0, 0.0, 103.0, 0.0);
        shared.portfolio.add_money_since_seed(1, -100.0);
        shared.portfolio.add_money_since_seed(1, 103.0);
        core.subscribe_pnl_single(4, 1);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert!((u.daily_pnl - 3.0).abs() < 1e-6, "daily={}", u.daily_pnl);
    }

    #[test]
    fn poll_pnl_single_unrealized_is_unset_without_avg_cost() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 0.0, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert_eq!(u.unrealized_pnl, f64::MAX);
    }

    // ibx#478: several req_pnl run together; each gets the values.
    #[test]
    fn several_pnl_requests_each_get_the_values() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.request_pnl(1, "DU1", "DU1").unwrap();
        core.request_pnl(2, "DU1", "DU1").unwrap();
        let ids: Vec<i64> = core.poll_pnl(&shared).iter().map(|u| u.req_id).collect();
        assert_eq!(ids, [1, 2]);
        assert!(core.poll_pnl(&shared).is_empty(), "unchanged: nothing");
        core.request_pnl(3, "DU1", "DU1").unwrap();
        let ids: Vec<i64> = core.poll_pnl(&shared).iter().map(|u| u.req_id).collect();
        assert_eq!(ids, [3], "a new request gets the current values");
    }

    #[test]
    fn pnl_request_checks() {
        let core = ClientCore::new();
        assert_eq!(core.request_pnl(1, "", "DU1").unwrap_err(),
            (321, "Error validating request.-'cj' : cause - Account must not be empty".to_string()));
        assert_eq!(core.request_pnl(1, "DU2", "DU1").unwrap_err(),
            (321, "Error validating request.-'cj' : cause - Invalid account code".to_string()));
        core.request_pnl(1, "DU1", "DU1").unwrap();
        assert_eq!(core.request_pnl(1, "DU1", "DU1").unwrap_err().0, 102);
        assert_eq!(core.request_pnl_single(5, "", "DU1", 1).unwrap_err().1,
            "Error validating request.-'ck' : cause - Account must not be empty");
        assert_eq!(core.cancel_pnl_request(99), Some((10185, "Failed to cancel PNL (not subscribed)".to_string())));
        assert_eq!(core.cancel_pnl_request(1), None);
        assert_eq!(core.cancel_pnl_single_request(98), Some((10186, "Failed to cancel PNL single (not subscribed)".to_string())));
    }

    #[test]
    fn poll_pnl_single_overnight_position_with_seed() {
        // #168 (bug 2): realized_pnl must come from the seed, not hardcoded 0.
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 10, 700.00, 735.00, 730.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 0.0,
            realized_pnl: 12.34,
        }]);
        core.subscribe_pnl_single(99, 756733);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        // daily = 10×735 − 10×730 − 0 = 50
        assert!((u.daily_pnl - 50.0).abs() < 1e-6);
        // unrealized = 10 × (735 − 700) = 350
        assert!((u.unrealized_pnl - 350.0).abs() < 1e-6);
        assert!((u.realized_pnl - 12.34).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_single_change_detection_suppresses_duplicate() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.subscribe_pnl_single(7, 1);
        assert_eq!(core.poll_pnl_single(&shared).len(), 1);
        // Same inputs → no emit.
        assert!(core.poll_pnl_single(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_single_unsubscribe_clears_cache() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.subscribe_pnl_single(7, 1);
        let _ = core.poll_pnl_single(&shared);
        core.unsubscribe_pnl_single(7);
        // Re-subscribing with same req_id must re-emit (cache cleared on unsubscribe).
        core.subscribe_pnl_single(7, 1);
        assert_eq!(core.poll_pnl_single(&shared).len(), 1);
    }

    // ibx#466: an order with an orderRef goes to the extended encoder,
    // which sends 6010; the currency reaches the engine once per contract.
    #[test]
    fn order_ref_uses_the_extended_encoder_and_currency_is_sent_once() {
        let order = ApiOrder { order_ref: "t1".into(), ..lmt(100.0) };
        assert!(order.has_extended_attrs());
        assert_eq!(order.attrs().order_ref, "t1");

        let core = ClientCore::new();
        let (tx, rx) = crossbeam_channel::unbounded();
        core.note_currency(&tx, 1, "EUR");
        core.note_currency(&tx, 1, "EUR");
        core.note_currency(&tx, 1, "");
        let sent: Vec<ControlCommand> = rx.try_iter().collect();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], ControlCommand::SetInstrumentCurrency { con_id: 1, currency } if currency == "EUR"));
    }

    // ibx#468: two local refusals of the reference.
    #[test]
    fn midprice_outside_rth_and_trail_limit_fields_are_refused() {
        let midprice = ApiOrder { order_type: "MIDPRICE".into(), outside_rth: true, ..lmt(100.0) };
        let (code, text) = ClientCore::refusal_before_sending(&midprice).expect("refused");
        assert_eq!(code, 321);
        assert!(text.ends_with("Midprice orders are not supported outside of regular trading hours."), "{text}");
        let rth = ApiOrder { order_type: "MIDPRICE".into(), outside_rth: false, ..lmt(100.0) };
        assert!(ClientCore::refusal_before_sending(&rth).is_none());

        let trail = |lmt_price: f64, offset: f64| ApiOrder {
            order_type: "TRAIL LIMIT".into(), aux_price: 1.0, trail_stop_price: 150.0,
            lmt_price, lmt_price_offset: offset, ..lmt(0.0)
        };
        let both = ClientCore::refusal_before_sending(&trail(100.0, 0.30)).expect("both: refused");
        assert_eq!(both.0, 321);
        assert!(both.1.ends_with("You must specify one value: limit price or limit price offset value."));
        assert!(ClientCore::refusal_before_sending(&trail(0.0, f64::MAX)).is_some(), "neither: refused");
        assert!(ClientCore::refusal_before_sending(&trail(0.0, 0.30)).is_none(), "offset only");
        assert!(ClientCore::refusal_before_sending(&trail(100.0, f64::MAX)).is_none(), "limit price only");
    }

    fn lmt(price: f64) -> ApiOrder {
        ApiOrder { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 100.0, lmt_price: price, ..Default::default() }
    }

    // ibx#463: a filled order was forgotten, so place_order with its id sent
    // a new order. The reference answers 104 and sends nothing (captured
    // 25/09/2026).
    #[test]
    fn place_on_a_filled_order_id_is_refused() {
        let core = ClientCore::new();
        core.track_order(7, ApiContract::default(), lmt(100.0), 0);
        assert!(core.refusal_for_order_id(7, &lmt(101.0)).is_none(), "working: a modify");
        core.update_order_fill(7, "Filled", 100.0, 0.0);
        assert_eq!(core.tracked_order_type(7), None);
        assert_eq!(core.refusal_for_order_id(7, &lmt(101.0)),
            Some((104, "Cannot modify a filled order.".to_string())));
    }

    #[test]
    fn place_on_a_cancelled_order_id_is_refused() {
        let core = ClientCore::new();
        core.track_order(8, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(8, "Cancelled", 0.0, 100.0);
        assert_eq!(core.refusal_for_order_id(8, &lmt(101.0)).map(|r| r.0), Some(104));
    }

    #[test]
    fn what_if_on_a_finished_order_id_is_not_refused() {
        let core = ClientCore::new();
        core.track_order(9, ApiContract::default(), lmt(100.0), 0);
        core.update_order_fill(9, "Filled", 100.0, 0.0);
        let what_if = ApiOrder { what_if: true, ..lmt(101.0) };
        assert!(core.refusal_for_order_id(9, &what_if).is_none());
    }

    #[test]
    fn finished_order_ids_survive_a_reset() {
        let core = ClientCore::new();
        core.track_order(10, ApiContract::default(), lmt(100.0), 0);
        core.update_order_fill(10, "Filled", 100.0, 0.0);
        core.reset();
        assert!(core.refusal_for_order_id(10, &lmt(101.0)).is_some());
    }

    // Reports for executions this client never sees are dropped oldest
    // first (ibx#471).
    #[test]
    fn pending_commissions_are_bounded_oldest_first() {
        let mut pending = PendingCommissions::default();
        let report = |id: &str| ApiCommissionAndFeesReport { exec_id: id.into(), ..Default::default() };
        for i in 0..PENDING_COMMISSIONS_MAX + 10 {
            pending.insert(format!("e{i}"), report(&format!("e{i}")));
        }
        assert_eq!(pending.len(), PENDING_COMMISSIONS_MAX);
        assert!(pending.remove("e0").is_none(), "the oldest is dropped");
        assert!(pending.remove(&format!("e{}", PENDING_COMMISSIONS_MAX + 9)).is_some(), "the newest is kept");
    }

    // A key replaced later is not dropped by its first, stale entry.
    #[test]
    fn pending_commission_replaced_keeps_its_new_place() {
        let mut pending = PendingCommissions::default();
        let report = |id: &str| ApiCommissionAndFeesReport { exec_id: id.into(), ..Default::default() };
        pending.insert("a".into(), report("a.01"));
        for i in 0..PENDING_COMMISSIONS_MAX - 1 {
            pending.insert(format!("e{i}"), report("x"));
        }
        pending.insert("a".into(), report("a.02"));
        pending.insert("b".into(), report("b"));
        assert_eq!(pending.remove("a").map(|r| r.exec_id), Some("a.02".to_string()));
    }

    // A modify keeps the status the server last reported: the engine can
    // still refuse the modify (ibx#463).
    #[test]
    fn a_modify_keeps_the_tracked_status() {
        let core = ClientCore::new();
        core.track_order(11, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(11, "Submitted", 0.0, 100.0);
        core.track_order(11, ApiContract::default(), lmt(101.0), 0);
        let tracked = core.open_orders.lock().unwrap().get(&11).cloned().unwrap();
        assert_eq!(tracked.status, "Submitted");
        assert_eq!(tracked.order.lmt_price, 101.0);
    }
}
