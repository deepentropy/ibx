//! Shared dispatch core for Rust and Python EClient implementations.
//!
//! `ClientCore` owns all subscription tracking state (reqId maps, change-detection
//! snapshots, PnL/account subscriptions) and exposes "prepare" methods that return
//! intermediate structs. Language-specific EClient adapters convert these into their
//! respective callback formats (Rust `Wrapper` trait calls or PyO3 `call_method`).

// The order-status vocabulary moved to the types it describes. Public here
// because that is the path a program written against this client already
// names, and used here for the same reason it was written.
pub use crate::types::order_status::{is_open_or_reactivatable, is_open_status, order_status_str};
use crate::types::NewsSubject;
use std::collections::{HashMap, HashSet};
use crate::error_codes::{
    CHANGE_CANNOT_CHANGE_TYPE, COMBINATION_LEG_INVALID, COMBINATION_NEEDS_LEGS,
    CONDITION_CONTRACT_INCOMPLETE, DUPLICATE_TICKER_ID, GOOD_TILL_DATE_INVALID, NO_SUCH_BOOK,
    ORDER_TYPE_UNSUPPORTED, Refusal,
    SECURITY_NOT_PERMITTED, TRIGGER_METHOD_INVALID, TRIGGER_PRICE_MISSING,
};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::sync::LazyLock;

use std::sync::mpsc::SyncSender;

use crate::types::model::{
    Contract as ApiContract, CommissionAndFeesReport as ApiCommissionAndFeesReport,
    Execution as ApiExecution, ExecutionFilter,
    Order as ApiOrder, TagValue,
    PRICE_SCALE_F,
};
use crate::bridge::SharedState;
use crate::types::*;

/// The only market data type the engine delivers (1 = realtime).
const MDT_REALTIME: i32 = 1;
const MDT_FROZEN: i32 = 2;
const MDT_DELAYED: i32 = 3;
const MDT_DELAYED_FROZEN: i32 = 4;

// ── Tick type constants matching ibapi ──

/// Tick type 1: the bid.
pub const TICK_BID: i32 = 1;
/// Tick type 2: the ask.
pub const TICK_ASK: i32 = 2;
/// Tick type 4: the last.
pub const TICK_LAST: i32 = 4;
/// Tick type 6: the high.
pub const TICK_HIGH: i32 = 6;
/// Tick type 7: the low.
pub const TICK_LOW: i32 = 7;
/// Tick type 9: the close.
pub const TICK_CLOSE: i32 = 9;
/// Tick type 14: the open.
pub const TICK_OPEN: i32 = 14;
/// Tick type 0: the bid size.
pub const TICK_BID_SIZE: i32 = 0;
/// Tick type 3: the ask size.
pub const TICK_ASK_SIZE: i32 = 3;
/// Tick type 5: the last size.
pub const TICK_LAST_SIZE: i32 = 5;
/// Tick type 8: the volume.
pub const TICK_VOLUME: i32 = 8;
/// Tick type 45: the last timestamp.
pub const TICK_LAST_TIMESTAMP: i32 = 45;
/// Whether the venue has halted trading. Stated by the venue and delivered as
/// a generic tick, which is where the reference client puts it.
pub const TICK_HALTED: i32 = 49;
/// Tick type 32: the bid exchange.
pub const TICK_BID_EXCHANGE: i32 = 32;
/// Tick type 33: the ask exchange.
pub const TICK_ASK_EXCHANGE: i32 = 33;
/// Tick type 84: the last exchange.
pub const TICK_LAST_EXCHANGE: i32 = 84;

/// Whether one contract of this holding is worth more than one unit of the
/// price the venue quotes it at.
///
/// A quote is per unit, so an option or a future priced from one alone is
/// valued at a fraction of what it is worth — a hundredth, for the commonest
/// option multiplier. The venue states its own figure for such a position, and
/// that is what is reported rather than a price this arithmetic cannot use.
fn position_is_multiplied(pi: &PositionInfo) -> bool {
    match pi.multiplier.trim() {
        "" => false,
        stated => stated.parse::<f64>().is_ok_and(|m| m != 1.0),
    }
}

/// Render an exchange-code bitmask to a letter string using the smart components
/// table. Each set bit at position N picks `smart_components[N].exchange_letter`.
///
/// Bit ordering and width follow the TWS-API convention; the dispatch path
/// tolerates an empty result where the mask layout differs.
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
    /// The request this answers.
    pub req_id: i64,
    /// Which tick this is.
    pub tick_type: i32,
    /// What it is.
    pub value: f64,
    /// true = tick_price, false = tick_size
    pub is_price: bool,
}

/// Timestamp tick from quote polling.
pub struct TimestampTick {
    /// The request this answers.
    pub req_id: i64,
    /// When, in nanoseconds since the epoch.
    pub timestamp_ns: i64,
}

/// String-valued tick (e.g. exchange-code letters for tick_types 32/33/84).
pub struct StringTickEvent {
    /// The request this answers.
    pub req_id: i64,
    /// Which tick this is.
    pub tick_type: i32,
    /// What it is.
    pub value: String,
}

/// The number a delayed feed carries a tick under, as the reference client
/// numbers one: the bid, the ask and the last from 66, the sizes and the rest
/// after them, and the halt and the timestamp under their own.
fn as_delayed(tick_type: i32) -> i32 {
    match tick_type {
        TICK_BID => 66, TICK_ASK => 67, TICK_LAST => 68,
        TICK_BID_SIZE => 69, TICK_ASK_SIZE => 70, TICK_LAST_SIZE => 71,
        TICK_HIGH => 72, TICK_LOW => 73, TICK_VOLUME => 74, TICK_CLOSE => 75, TICK_OPEN => 76,
        TICK_LAST_TIMESTAMP => 88, TICK_HALTED => 90,
        other => other,
    }
}

impl ClientCore {
    /// Note this contract's feed as delayed, so a test can read what a caller
    /// who asked for delayed data reads.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn mark_feed_delayed_for_test(&self, instrument: InstrumentId) {
        self.mdt_by_instrument.lock().unwrap().insert(instrument, MDT_DELAYED);
    }

    /// Whether this contract's readings are the delayed feed's.
    ///
    /// The reference client numbers a delayed reading apart from a live one,
    /// so what is delivered under 13 on a live feed is delivered under 83 on a
    /// delayed one — a program that asked for delayed data reads it there.
    pub fn feed_is_delayed(&self, instrument: InstrumentId) -> bool {
        matches!(
            self.mdt_by_instrument.lock().unwrap().get(&instrument),
            Some(&MDT_DELAYED) | Some(&MDT_DELAYED_FROZEN)
        )
    }
}

/// Result of polling quotes for one instrument.
pub struct QuotePollResult {
    /// Numeric ticks that arrived.
    pub ticks: Vec<TickEvent>,
    /// Ticks the venue states under a number of its own rather than as a price
    /// or a size, delivered on `tick_generic`.
    /// What the venue says about the two prices rather than what they are, as
    /// it states them: one mask for whether each side may be dealt on without
    /// a human, one for pre-open and past-limit. Read per side where each
    /// price is handed over.
    pub eligible_mask: i64,
    /// The second of those, carried beside it.
    pub quote_state_mask: i64,
    pub generic_ticks: Vec<TickEvent>,
    /// Ticks whose value is text.
    pub string_ticks: Vec<StringTickEvent>,
    /// The moment the venue stamped the quote with, if it stated one.
    pub timestamp: Option<TimestampTick>,
    /// Whether this request's feed is delayed, so what follows goes out under
    /// the numbers the reference client gives a delayed feed.
    pub delayed: bool,
    /// true if any tick was delivered (for snapshot detection).
    pub delivered: bool,
}

/// PnL update (account-level).
pub struct PnlUpdate {
    /// The request this answers.
    pub req_id: i64,
    /// What the account has made today.
    pub daily_pnl: f64,
    /// What its positions have made and not realised.
    pub unrealized_pnl: f64,
    /// What it has realised.
    pub realized_pnl: f64,
}

/// PnL single update (per-position).
pub struct PnlSingleUpdate {
    /// The request this answers.
    pub req_id: i64,
    /// How much is held.
    pub pos: f64,
    /// What the account has made today.
    pub daily_pnl: f64,
    /// What its positions have made and not realised.
    pub unrealized_pnl: f64,
    /// What it has realised.
    pub realized_pnl: f64,
    /// What it is.
    pub value: f64,
}

/// A single changed account field.
pub struct AccountFieldUpdate {
    /// Which figure.
    pub key: String,
    /// What it is.
    pub value: String,
    /// What currency it is stated in.
    pub currency: String,
}

/// Batch of account update results.
pub struct AccountUpdateBatch {
    /// Each figure that changed.
    pub fields: Vec<AccountFieldUpdate>,
    /// Whether the account has just been stated whole: true on the pass after
    /// the venue ended the download it was sending, once per download, so a
    /// rebuilt connection's download ends again.
    pub finished: bool,
}

/// Prepared account summary response.
pub struct AccountSummaryBatch {
    /// The request this answers.
    pub req_id: i64,
    /// Each figure answering the request.
    pub entries: Vec<AccountSummaryEntry>,
}

/// One figure answering a summary request.
pub struct AccountSummaryEntry {
    /// Which figure this is, under the venue's name for it. Owned for the
    /// same reason the currency is: the set is the venue's, not a fixed list
    /// known here, and a summary built from such a list reported nothing for
    /// every figure that was not on it.
    pub tag: String,
    /// What it is.
    pub value: String,
    /// As the venue stated it for this figure. Owned rather than borrowed
    /// because it is the venue's word, not one of a fixed set known here.
    pub currency: String,
}

/// A single portfolio position update.
pub struct PortfolioUpdateEntry {
    /// The contract.
    pub con_id: i64,
    /// How much is held.
    pub position: f64,
    /// What it cost on average.
    pub avg_cost: f64,
    /// What it is worth now, each.
    pub market_price: f64,
    /// What the holding is worth.
    pub market_value: f64,
    /// What its positions have made and not realised.
    pub unrealized_pnl: f64,
    /// What it has realised.
    pub realized_pnl: f64,
}

/// One summary's last pass: when it ran, and what it stated then, keyed by the
/// figure and the currency it was stated in.
type SummaryPass = (std::time::Instant, HashMap<(String, String), String>);

/// Fold the risk levels modelled here to the venue's names. Anything else
/// travels as text in the parameter list; the venue owns that vocabulary.
fn parse_risk_aversion(raw: Option<&str>) -> Option<RiskAversion> {
    match raw?.to_lowercase().as_str() {
        "neutral" => Some(RiskAversion::Neutral),
        "get_done" | "getdone" => Some(RiskAversion::GetDone),
        "aggressive" => Some(RiskAversion::Aggressive),
        "passive" => Some(RiskAversion::Passive),
        _ => None,
    }
}

/// Fold the flag spellings modelled here to the `1`/`0` the venue is known to
/// take. Anything else travels as text, for the reason a risk level does.
fn parse_algo_flag(raw: Option<&str>) -> Option<bool> {
    match raw?.to_lowercase().as_str() {
        "0" | "false" => Some(false),
        "1" | "true" => Some(true),
        _ => None,
    }
}

/// The parameters a strategy this client models reads off the caller's list.
///
/// A strategy that is not here is handed to the venue with the caller's list
/// as written, so it has no set to state. So is one that is here and was given
/// a key outside its set: the set decides whether the list is re-encoded from
/// named fields or forwarded whole, not whether the caller may state a key.
fn algo_param_names(strategy: &str) -> Option<&'static [&'static str]> {
    Some(match strategy {
        "vwap" => &["maxPctVol", "noTakeLiq", "allowPastEndTime", "startTime", "endTime"],
        "twap" => &["allowPastEndTime", "startTime", "endTime"],
        "arrivalpx" | "arrival_price" => &[
            "maxPctVol", "riskAversion", "allowPastEndTime", "forceCompletion",
            "startTime", "endTime",
        ],
        "closepx" | "close_price" => {
            &["maxPctVol", "riskAversion", "forceCompletion", "startTime"]
        }
        "darkice" | "dark_ice" => {
            &["allowPastEndTime", "displaySize", "startTime", "endTime"]
        }
        "pctvol" | "pct_vol" => &["pctVol", "noTakeLiq", "startTime", "endTime"],
        _ => return None,
    })
}

/// Parse algo strategy and TagValue params into internal AlgoParams.
///
/// A key the caller never set is not stated: the venue's own default for it
/// is not known here, and a value sent in its place — `0`, an empty time,
/// Neutral — is a claim the caller did not make.
///
/// A strategy modelled here is re-encoded from the fields it names, and a key
/// it does not name has no field to be re-encoded into. That is a limit of the
/// re-encoding, not of the protocol: there is no tag per parameter — a name and
/// a value travel as a pair in a repeating group, and the venue reads a pair
/// whose name this client never modelled exactly as it reads one it did. So a
/// list carrying such a key is forwarded whole, as the caller wrote it and in
/// the order they wrote it, rather than refused or quietly shortened.
///
/// A value is checked, not re-spelled. A parameter is text on the wire, and
/// the text the caller wrote is what reaches the venue, as the reference
/// client forwards it; the parse here is this client's own check that it
/// reads. Two kinds are sent in the venue's spelling instead, each said where
/// it is read: a known flag goes as `1`/`0`, and a known `riskAversion` as the
/// venue names it. Other spellings travel as written.
pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, Refusal> {
    let folded = strategy.to_lowercase();
    // The caller's list as they wrote it: name then value, in their order. The
    // caller's own spelling of the strategy too, since the venue is handed that
    // name and does not know a lower-cased one.
    let as_written = || AlgoParams::Named {
        strategy: strategy.to_string(),
        params: params
            .iter()
            .flat_map(|tv| [tv.tag.clone(), tv.value.clone()])
            .collect(),
    };
    // A value outside the vocabulary folded here takes the route a key outside
    // the set takes: the list goes as the caller wrote it, and the venue answers
    // for a spelling it does not know. Folded to "unset" instead, the parameter
    // was dropped from the order and nothing said so — the caller asked for a
    // risk level or a flag and the order went without one.
    let outside_the_vocabulary = |tv: &TagValue| match tv.tag.as_str() {
        "riskAversion" => parse_risk_aversion(Some(&tv.value)).is_none(),
        "noTakeLiq" | "allowPastEndTime" | "forceCompletion" => {
            parse_algo_flag(Some(&tv.value)).is_none()
        }
        _ => false,
    };
    if let Some(known) = algo_param_names(&folded)
        && params.iter().any(|tv| {
            !known.contains(&tv.tag.as_str()) || outside_the_vocabulary(tv)
        })
    {
        return Ok(as_written());
    }
    let get = |key: &str| -> Option<String> {
        params.iter().find(|tv| tv.tag == key).map(|tv| tv.value.clone())
    };
    let get_num = |key: &str| -> Result<Option<String>, Refusal> {
        let raw = match get(key) {
            None => return Ok(None),
            Some(raw) => raw,
        };
        let v: f64 = raw.parse()
            .map_err(|_| Refusal::validation(format!("Invalid {key} '{raw}': expected a number")))?;
        if !v.is_finite() {
            return Err(Refusal::validation(
                format!("Invalid {key} '{raw}': must be a finite number"),
            ));
        }
        Ok(Some(raw))
    };
    // Every flag reaching here is one the fold above recognised: a spelling it
    // did not took the whole list down the text path.
    let get_bool = |raw: Option<&str>| parse_algo_flag(raw);

    let algo: Result<AlgoParams, Refusal> = match folded.as_str() {
        "vwap" => Ok(AlgoParams::Vwap {
            max_pct_vol: get_num("maxPctVol")?,
            no_take_liq: get_bool(get("noTakeLiq").as_deref()),
            allow_past_end_time: get_bool(get("allowPastEndTime").as_deref()),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "twap" => Ok(AlgoParams::Twap {
            allow_past_end_time: get_bool(get("allowPastEndTime").as_deref()),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "arrivalpx" | "arrival_price" => Ok(AlgoParams::ArrivalPx {
            max_pct_vol: get_num("maxPctVol")?,
            risk_aversion: parse_risk_aversion(get("riskAversion").as_deref()),
            allow_past_end_time: get_bool(get("allowPastEndTime").as_deref()),
            force_completion: get_bool(get("forceCompletion").as_deref()),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "closepx" | "close_price" => Ok(AlgoParams::ClosePx {
            max_pct_vol: get_num("maxPctVol")?,
            risk_aversion: parse_risk_aversion(get("riskAversion").as_deref()),
            force_completion: get_bool(get("forceCompletion").as_deref()),
            start_time: get("startTime"),
        }),
        "darkice" | "dark_ice" => {
            // Stated, not chosen here. A display size is how much of the
            // order the book shows; a default would show a size the caller
            // never asked to show.
            let display_size = match get("displaySize") {
                None => return Err(Refusal::validation(
                    "DarkIce needs a displaySize: it is how much of the order the \
                     book shows, and this client will not choose it for you",
                )),
                Some(raw) => {
                    raw.parse::<u32>().map_err(|_| format!("Invalid displaySize '{raw}': expected a non-negative integer"))?;
                    raw
                }
            };
            Ok(AlgoParams::DarkIce {
                allow_past_end_time: get_bool(get("allowPastEndTime").as_deref()),
                display_size,
                start_time: get("startTime"),
                end_time: get("endTime"),
            })
        }
        "pctvol" | "pct_vol" => Ok(AlgoParams::PctVol {
            pct_vol: get_num("pctVol")?,
            no_take_liq: get_bool(get("noTakeLiq").as_deref()),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        // Anything else goes as the caller wrote it.
        //
        // Refused here instead, a caller could use only the algorithms this
        // match happens to name — five of the thirteen an ordinary session is
        // offered. Which ones an account may use is the venue's answer, stated
        // at logon and enforced by it, and the reference client does not
        // interpret these either.
        _ => return Ok(as_written()),
    };
    algo
}

// ── Order field validation ──

/// Reject a price/amount field that a saturating float-to-int cast would
/// otherwise turn into a different, valid-looking number: NaN becomes 0,
/// +/-Infinity becomes i64::MAX/MIN, and a finite value whose fixed-point
/// form overflows i64 saturates the same way.
pub(crate) fn require_finite_price(field: &str, v: f64) -> Result<(), String> {
    // `i64::MAX as f64` itself rounds up to 2^63 (f64 cannot represent
    // i64::MAX exactly), so a strict `>` lets a scaled value of exactly
    // 2^63 through and the subsequent `as i64` cast saturates to i64::MAX
    // instead of being refused. `>=` excludes that boundary.
    if !v.is_finite() || (v * PRICE_SCALE_F).abs() >= i64::MAX as f64 {
        return Err(format!(
            "{field} must be a finite number representable on the wire, got {v}"
        ));
    }
    Ok(())
}

/// Parse the Adaptive algo's `adaptivePriority` tag. A missing tag defaults
/// to Normal (IB's own default); a present-but-unrecognized value is
/// refused instead of silently defaulting to Normal.
fn adaptive_priority(params: &[TagValue]) -> Result<AdaptivePriority, String> {
    match params.iter().find(|tv| tv.tag == "adaptivePriority") {
        None => Ok(AdaptivePriority::Normal),
        Some(tv) => match tv.value.as_str() {
            "Patient" => Ok(AdaptivePriority::Patient),
            "Normal" => Ok(AdaptivePriority::Normal),
            "Urgent" => Ok(AdaptivePriority::Urgent),
            other => Err(format!(
                "Unknown adaptivePriority '{other}': expected Patient, Normal or Urgent"
            )),
        },
    }
}

// ── Execution storage ──

/// Does a stored execution satisfy an `ExecutionFilter`? Shared by the index
/// form and the snapshot form so the two cannot disagree about what matches.
fn execution_matches(se: &StoredExecution, filter: &ExecutionFilter) -> bool {
    if !filter.symbol.is_empty() && !se.contract.symbol.eq_ignore_ascii_case(&filter.symbol) {
        return false;
    }
    if !filter.sec_type.is_empty() && !se.contract.sec_type.eq_ignore_ascii_case(&filter.sec_type) {
        return false;
    }
    if !filter.exchange.is_empty() && !se.execution.exchange.eq_ignore_ascii_case(&filter.exchange) {
        return false;
    }
    // A stored execution carries the venue's word for the side, and a filter
    // states the order action. Compared as written, a filter for buys matched
    // nothing and the caller read an empty answer as "no fills". Both
    // vocabularies are accepted here, in the one place both surfaces compare,
    // rather than mapped on the way in by one of them and not the other.
    if !filter.side.is_empty() {
        let wanted = match filter.side.to_ascii_uppercase().as_str() {
            "BUY" | "BOT" => "BOT",
            "SELL" | "SSHORT" | "SLD" => "SLD",
            _ => &filter.side,
        };
        if !se.execution.side.eq_ignore_ascii_case(wanted) {
            return false;
        }
    }
    if !filter.acct_code.is_empty() && !se.execution.acct_number.eq_ignore_ascii_case(&filter.acct_code) {
        return false;
    }
    if filter.client_id != 0 && se.execution.client_id != filter.client_id {
        return false;
    }
    // ibapi treats `time` as a lower bound — executions at or after it. The
    // two sides can be punctuated differently ("20260729-10:00:00" against
    // "20260729 10:00:00"), so compare on digits alone; both are yyyyMMdd
    // first, so that ordering is chronological. A bound carrying less
    // precision than the timestamp compares against the same prefix, so a
    // date-only filter keeps that whole day rather than dropping it. An
    // execution the venue stated no time for compares on nothing and is kept:
    // it cannot be placed either side of a bound, and a caller asking what has
    // traded is worse served by a fill they are never shown.
    if !filter.time.is_empty() {
        let digits = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>();
        let lo = digits(&filter.time);
        let at = digits(&se.execution.time);
        let n = lo.len().min(at.len());
        if at.get(..n).unwrap_or("") < lo.get(..n).unwrap_or("") {
            return false;
        }
    }
    true
}

/// A stored execution + commission_and_fees pair for `req_executions` replay.
/// Shared between Rust and Python adapters via `ClientCore`.
#[derive(Clone)]
pub struct StoredExecution {
    /// The contract it is on.
    pub contract: ApiContract,
    /// The fill itself.
    pub execution: ApiExecution,
    /// What the fill cost.
    pub commission_and_fees: ApiCommissionAndFeesReport,
}

/// The session's executions, and where each named one sits.
///
/// Indexed by the venue's id: a fill and its charge arrive as two messages,
/// and the charge finds its execution by name. Scanned instead, every fill
/// and every charge cost a pass over the day so far, on the thread that also
/// delivers the callbacks. Nothing leaves the rows but a `reset`, so an index
/// into them stays good.
#[derive(Default)]
pub struct ExecutionStore {
    rows: Vec<StoredExecution>,
    by_id: HashMap<String, usize>,
}

// ── Order tracking ──

/// A locally tracked order for `req_open_orders` / dispatch status updates.
#[derive(Clone)]
pub struct TrackedOrder {
    /// The contract it is on.
    pub contract: ApiContract,
    /// The order as this client sent it.
    pub order: ApiOrder,
    /// Where it stands.
    pub status: String,
    /// How much has filled.
    pub filled: f64,
    /// How much has not.
    pub remaining: f64,
    /// What everything filled so far averaged, as the venue stated it, and
    /// what the last of it went at.
    ///
    /// Nought where the venue has stated nothing — which is what nought means
    /// on the callback that carries them. Answered as nought on an order the
    /// venue had stated an average for, a caller read "filled three hundred
    /// at an average of nothing", which is not a reading it can tell from a
    /// real one.
    pub avg_fill_price: f64,
    /// What the last fill on it went at.
    pub last_fill_price: f64,
    /// The engine's own slot for the contract.
    pub instrument: InstrumentId,
    /// The terms the venue is known to hold, kept while a replacement of them
    /// is outstanding.
    ///
    /// The record takes the attempt ahead of the venue's answer, because every
    /// later action restates from it. Where the answer is a refusal the attempt
    /// must not stand: without this the record kept terms the venue had said no
    /// to, and the next cancel or replace went out restating them.
    pub before_the_replace: Option<Box<ApiOrder>>,
    /// True once this order's last transition was a genuine Rejected (FIX
    /// 39=8). Rejected and Inactive both stringify to `status == "Inactive"`
    /// (ibapi has no Rejected string), so that string alone cannot tell a
    /// dead order from a parked, reactivatable one — `collect_open_orders`
    /// uses this flag as the discriminator instead of widening
    /// `is_open_status`.
    pub rejected: bool,
    /// Whether this client placed the order, as against learning of it from
    /// the venue — through a status, or by restating one the venue named.
    /// A replace of an order placed here restates the engine's own record;
    /// a replace of any other goes behind the caller's statement of it.
    pub placed_here: bool,
}

// ── ClientCore ──

/// An answer owed to a caller about a display group.
#[derive(Debug, Clone)]
pub enum GroupEvent {
    /// The groups on offer, `|`-separated.
    List(i64, String),
    /// What a group now holds.
    Updated(i64, String),
}

/// What both client surfaces share: which request is on which
/// contract, what the venue last said, and what is still subscribed.
/// An order built and not sent, waiting for one that transmits.
pub struct HeldOrder {
    /// The id it was placed under.
    pub order_id: u64,
    /// The order it hangs from, or zero.
    pub parent_id: i64,
    /// What will go out, exactly as it would have.
    pub command: ControlCommand,
}

impl HeldOrder {
    /// Whether what is held would place the order, rather than revise one the
    /// venue is already working.
    ///
    /// The two are not the same withdrawal. Forgetting a placement is the
    /// whole of it, because the venue was never given the order; forgetting a
    /// revision leaves the order it was a revision to live at the venue.
    fn places_the_order(&self) -> bool {
        places_the_order(&self.command)
    }
}

/// Put back what a restatement replaced, and square the outstanding quantity
/// with it. Does nothing where nothing was kept.
fn put_back_the_terms(tracked: &mut TrackedOrder) {
    if let Some(held) = tracked.before_the_replace.take() {
        tracked.order = *held;
        tracked.remaining = (tracked.order.total_quantity - tracked.filled).max(0.0);
    }
}

/// The record of an order as this client has just placed it.
///
/// Written by more than one path — held back, or sent — so the shape of a
/// freshly placed order is stated once.
fn tracked_as_placed(
    contract: ApiContract, order: ApiOrder, instrument: InstrumentId,
) -> TrackedOrder {
    let remaining = order.total_quantity;
    TrackedOrder {
        contract, order, status: "PendingSubmit".into(), filled: 0.0, remaining, instrument,
        // Nothing has filled, so there is no price the venue has stated for
        // one, which is what nought means on the callback that carries them.
        avg_fill_price: 0.0, last_fill_price: 0.0,
        rejected: false, before_the_replace: None, placed_here: true,
    }
}

/// Whether a command would place an order, rather than revise one the venue is
/// already working.
///
/// Asked of the command rather than of the hold, because the order that
/// transmits is sent from the caller's hand and is never in the hold by the
/// time its send is accounted for.
fn places_the_order(command: &ControlCommand) -> bool {
    matches!(
        command,
        ControlCommand::Order(
            OrderRequest::SubmitEx { .. } | OrderRequest::SubmitBracket { .. },
        ),
    )
}

/// What one historical request asked for.
///
/// Kept because the reply states neither. The range written beside the last
/// bar is the request's own, and the bar times are written in the form the
/// request named.
#[derive(Clone, Default)]
pub struct HistoricalAsk {
    /// 2 for seconds since the epoch, anything else for the venue's spelling.
    pub format_date: i32,
    /// The end the caller named, or empty for the moment of asking.
    pub end_date_time: String,
    /// How far back from that end, in the venue's own units.
    pub duration: String,
    /// The zone the venue stated the series on, once it has, for the bars
    /// that continue it.
    pub zone: String,
}

/// A request number held for the length of a registration.
///
/// Given back when this drops, which is every way out of the call that took it
/// — the refusals before the engine is asked, the wait timing out, the `?` on
/// a send. A release written at each of those instead is a release somebody
/// adds a path around later. The one path that gives it back itself is the
/// success below, which has to read a withdrawal under the same acquisition.
struct Registering<'a> {
    held: &'a Mutex<std::collections::HashSet<(u8, i64)>>,
    withdrawn: &'a Mutex<std::collections::HashSet<(u8, i64)>>,
    key: (u8, i64),
}

impl Registering<'_> {
    /// Give the number back, and say whether a withdrawal arrived while it was
    /// held.
    ///
    /// Both under one acquisition. Given back first, a withdrawal landing in
    /// between finds the claim gone and the record not yet written, and is
    /// told there is nothing to withdraw — which is the answer that leaves a
    /// caller holding a live subscription it believes is gone.
    fn withdrawn_meanwhile(&self) -> bool {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let asked = self.withdrawn.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
        held.remove(&self.key);
        asked
    }
}

impl Drop for Registering<'_> {
    fn drop(&mut self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        // A registration that never reached the point of reading it leaves no
        // withdrawal behind for the next one under this number, which would
        // otherwise take itself down having been asked for nothing of the kind.
        self.withdrawn.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
        held.remove(&self.key);
    }
}

/// Which record a registration is taking: the quotes, or the tick stream.
const TAKING_QUOTES: u8 = 0;
const TAKING_TICKS: u8 = 1;

/// What a withdrawal leaves for its caller to send.
///
/// Three questions, and they are not the same one: the subscription to
/// withdraw, the contract whose headlines stop, and the series nobody
/// watching that contract asks for any more. The quotes stay up for another
/// caller while the other two end, so a caller told only about the quotes
/// sent nothing and left both running.
pub struct WhatAWithdrawalLeaves {
    /// The subscription to withdraw, where this was the last caller watching
    /// it.
    pub subscription: Option<InstrumentId>,
    /// The contract whose headlines stop.
    pub headlines: Option<NewsSubject>,
    /// The contract, and the series nobody watching it asks for any more.
    ///
    /// Said whether or not the subscription itself goes. Where it goes, they
    /// ride with it, because the subscription the engine finds may not be the
    /// one this was decided against — and then these are the only part of the
    /// withdrawal that is still about what the caller asked for.
    pub series: Option<(InstrumentId, Vec<u32>)>,
    /// The number the request that took that slot asked under.
    ///
    /// What names one occupancy of a reusable slot. Read under the maps that
    /// decide the withdrawal, so it is the occupancy this decision was about
    /// and not whichever one holds the slot by the time the engine reads it.
    pub took_it: u64,
    /// The contract this client believed that slot held.
    ///
    /// What makes the withdrawal about one occupancy of a reusable slot rather
    /// than about the slot: the engine keeps the contract each subscription
    /// went out under and compares them. Zero where the venue has not
    /// identified the contract yet, and then the number below is all there is.
    pub con_id: i64,
    /// Where this decision falls in the order of everything this client has
    /// asked for.
    ///
    /// Taken as the decision is made, and sent with the commands that carry it
    /// out. Taken as those commands are sent instead, a caller that asked for
    /// the same contract in between had its brand-new subscription withdrawn
    /// by this one: the engine cannot tell the subscription that was decided
    /// against from the one that replaced it, and this number is what tells
    /// them apart.
    pub decided_at: u64,
}

/// What a request asking for a contract ended up as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Joined {
    /// It holds the subscription: nobody held the contract, or it took the
    /// slot from the venue's one-shot.
    Took,
    /// It took the slot from the one-shot that held it, which now watches it.
    TookFromTheOneShot,
    /// Somebody else holds the subscription and this one watches it.
    Watching,
}

/// The maps that say who is watching what, held together for one decision.
///
/// Held for as long as the decision takes and no longer. Nothing that can take
/// one of them again — the news record, a callback, anything reached through
/// the engine — may be called while this is alive.
struct Ownership<'a> {
    /// The one request each slot's subscription belongs to.
    holders: std::sync::MutexGuard<'a, HashMap<InstrumentId, i64>>,
    /// Every other request watching that slot.
    following: std::sync::MutexGuard<'a, HashMap<InstrumentId, Vec<i64>>>,
    /// Which slot a request is watching.
    by_req: std::sync::MutexGuard<'a, HashMap<i64, InstrumentId>>,
    /// The requests that asked for the venue's chargeable one-shot.
    one_shot: std::sync::MutexGuard<'a, HashSet<i64>>,
    /// What each request asked for beyond the quote.
    series: std::sync::MutexGuard<'a, HashMap<i64, Vec<u32>>>,
    /// When each slot was taken, in the order of what this client has asked
    /// for.
    taken_on: std::sync::MutexGuard<'a, HashMap<InstrumentId, u64>>,
    /// Which subscription each request is holding, as a figure that changes
    /// every time it takes a new one.
    epoch: std::sync::MutexGuard<'a, HashMap<i64, u64>>,
}

impl Ownership<'_> {
    /// Record a request as watching a slot somebody else holds.
    fn watches(&mut self, instrument: InstrumentId, req_id: i64) {
        let watchers = self.following.entry(instrument).or_default();
        if !watchers.contains(&req_id) {
            watchers.push(req_id);
        }
    }

    /// Hold this contract, or watch whoever took it first.
    ///
    /// The venue's one-shot was sent as a request of its own, so it is not
    /// what a stream is served off: where it holds the slot and a stream
    /// arrives, the stream takes the slot and the one-shot watches beside it.
    /// Left holding, the stream was served off a one-shot that is withdrawn
    /// the moment it completes, and heard nothing after that.
    ///
    /// Either way both are recorded as watching, because what is watching is
    /// what a reading is delivered to: recorded as neither, the caller that
    /// asked for the one-shot was sent nothing at all.
    fn take_or_follow(
        &mut self, instrument: InstrumentId, req_id: i64, series: &[u32], taken_on: u64,
    ) -> Joined {
        // What this request asked for, written down as it becomes one of the
        // watchers rather than before. Written first, a withdrawal deciding
        // which series nobody asks for any more read a list belonging to a
        // request that was not watching anything yet, and either kept a series
        // for a caller that never arrived or withdrew one the arriving caller
        // had just asked for.
        if !series.is_empty() {
            self.series.insert(req_id, series.to_vec());
        }
        let held = self.holders.get(&instrument).copied();
        if !self.one_shot.contains(&req_id)
            && held.is_some_and(|existing| self.one_shot.contains(&existing))
        {
            self.holders.insert(instrument, req_id);
            self.taken_on.insert(instrument, taken_on);
            if let Some(displaced) = held {
                self.watches(instrument, displaced);
            }
            self.by_req.insert(req_id, instrument);
            return Joined::TookFromTheOneShot;
        }
        match held {
            Some(existing) if existing != req_id => {
                self.watches(instrument, req_id);
                self.by_req.insert(req_id, instrument);
                Joined::Watching
            }
            _ => {
                self.holders.insert(instrument, req_id);
                self.taken_on.insert(instrument, taken_on);
                // Under the same acquisition as the holder map, not after it.
                // Written by the caller once this returned, a slot given back
                // in between was forgotten while this request pointed at
                // nothing — and the mapping landed afterwards, naming a slot
                // whose next contract this caller never asked about.
                self.by_req.insert(req_id, instrument);
                Joined::Took
            }
        }
    }
}

pub struct ClientCore {
    /// How long a caller waits for the engine to name an instrument.
    ///
    /// Stated by the session. It was read from the process once and cached for
    /// the life of it, so the first session to ask fixed the wait for every
    /// session after it.
    pub registration_timeout: std::sync::Mutex<std::time::Duration>,
    /// Whether this session refuses to send anything that changes a position.
    ///
    /// The reference API carries the same control. A research or reporting
    /// program gets the guarantee at the client rather than by discipline.
    /// Set once when the session opens.
    pub readonly: std::sync::atomic::AtomicBool,
    // reqId <-> InstrumentId mapping
    /// Which contract each quote request is on.
    pub req_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    /// Numbers a registration is in the middle of taking.
    ///
    /// The map above answers which contract a number watches, and it cannot be
    /// written until the engine has named the slot — which is a wait, and a
    /// long one where the engine is busy. Two callers registering one number
    /// in that window both read the map as free and both went on: two slots
    /// ended up holding contracts under one number, the map kept whichever
    /// finished last, and the other slot's subscription stayed live on the
    /// wire with nothing able to withdraw it. That is the failure the check
    /// beside the map is written to prevent, and checking a map nobody has
    /// written yet cannot prevent it.
    /// Keyed by which record is being taken as well as by the number: a
    /// number may carry a quote subscription and a tick stream at once, and
    /// the two are refused against their own records, so a claim that did not
    /// say which would refuse a pair the client allows.
    registering: Mutex<std::collections::HashSet<(u8, i64)>>,
    /// Numbers withdrawn while the registration above was still in flight.
    ///
    /// A withdrawal that arrives in that window has no record to act on. The
    /// venue withdraws it all the same, so it is written down here and the
    /// registration reads it as it gives the number back: what it opened is
    /// taken down again instead of published, and no record is left behind.
    /// Keyed as the claim is, because the quotes and the tick stream under one
    /// number are withdrawn separately.
    withdrawn_while_registering: Mutex<std::collections::HashSet<(u8, i64)>>,
    /// Which registration a number is currently holding, counted.
    ///
    /// A number outlives the subscriptions made under it: a callback may
    /// withdraw what it was told about and ask for something else on the spot,
    /// and the new one can watch the very same contract — so neither the
    /// number nor what it watches tells the two apart. This does.
    registration_epoch: Mutex<HashMap<i64, u64>>,
    epochs: std::sync::atomic::AtomicU64,
    /// Which request owns each contract's quotes. One per contract:
    /// later callers follow it rather than opening a second.
    pub instrument_to_req: Mutex<HashMap<InstrumentId, i64>>,
    /// Which contract each tick-by-tick request is on.
    ///
    /// Its own, because a trade stream is not a quote subscription. Kept in
    /// the quote maps, a request for trades was handed the contract's quotes,
    /// and withdrawing it took away the quotes another caller was watching.
    pub tbt_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    /// The other requests watching a contract that is already subscribed.
    ///
    /// One contract holds one subscription on the wire, and the same quote is
    /// handed to every caller that asked for it. Two parts of one program may
    /// watch the same contract.
    pub instrument_followers: Mutex<HashMap<InstrumentId, Vec<i64>>>,
    // con_id → InstrumentId for find_or_register_instrument lookup
    /// The engine slot each contract id was given.
    pub con_id_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    /// What each display group currently holds.
    ///
    /// A group is a way for several callers on one session to agree on a
    /// contract. Nothing about one crosses the wire, so they are kept here and
    /// served to callers from here.
    display_groups: Mutex<HashMap<i32, String>>,
    /// Which group each subscribing request follows.
    group_subscriptions: Mutex<HashMap<i64, i32>>,
    /// Group answers waiting to be delivered on the next dispatch, so a caller
    /// hears them where it hears everything else.
    pending_group_events: Mutex<Vec<GroupEvent>>,
    // Change detection for quote polling
    /// The most recent quote per contract, so a caller asking twice is
    /// answered the same way twice.
    pub last_quotes: Mutex<HashMap<InstrumentId, [i64; 16]>>,
    /// Requests that asked for a snapshot rather than a stream, and when each
    /// last heard something.
    ///
    /// A snapshot ends when the venue has finished sending it. There is no
    /// marker for that on this protocol, so it ends when the ticks stop:
    /// ending at the first one cancelled the subscription on whatever arrived
    /// first — often the previous close — and the bid and ask that the caller
    /// asked for never came.
    /// Snapshots being waited on: when each was asked for, and which of the
    /// kinds one is made of the venue has stated so far.
    pub snapshot_reqs: Mutex<HashMap<i64, (std::time::Instant, u8)>>,
    /// The requests that asked for the venue's one-shot snapshot.
    ///
    /// One of those is a request of its own and not a stream: the venue
    /// answers it once, under a request type of its own, and it is withdrawn
    /// as soon as it completes. Nothing follows one — a stream that did was
    /// never sent, and when the one-shot was withdrawn the follower was
    /// promoted onto the one-shot's own row and heard no quotes at all.
    chargeable_snapshot_reqs: Mutex<std::collections::HashSet<i64>>,
    /// Which series each request asked for, by the venue's number for each.
    ///
    /// A subscription is shared here and its series are not: a caller joining
    /// a contract brings its own list, and what it brought goes with it when
    /// it withdraws. Without a record of who asked for what, the series a
    /// joiner added were served for as long as the subscription it joined
    /// outlived it — counted against the allowance, and asked for again by
    /// every rebuild after a reconnect.
    series_by_req: Mutex<HashMap<i64, Vec<u32>>>,
    /// When each slot was taken, in the order of what this client has asked
    /// for.
    ///
    /// Read against the number a slot was given back under. A slot the engine
    /// frees goes to the next contract that needs one, so a release read after
    /// that named a slot this client had just been given again: every record
    /// of the contract now on it was forgotten, the venue went on streaming
    /// it, and no request could be found to deliver it to or to withdraw it.
    slot_taken_on: Mutex<HashMap<InstrumentId, u64>>,

    // PnL subscription state
    /// The request a running profit is reported under.
    pub pnl_req_id: Mutex<Option<i64>>,
    /// Which contract each single-position profit request is on.
    pub pnl_single_reqs: Mutex<HashMap<i64, i64>>, // req_id → con_id
    /// The last running profit stated: daily, unrealised, realised.
    pub last_pnl: Mutex<[i64; 3]>, // [daily, unrealized, realized]
    // Per-req_id change detection for pnl_single: [pos, daily, unrealized, realized,
    // value] scaled.
    /// The same per position.
    pub last_pnl_single: Mutex<HashMap<i64, [i64; 5]>>,

    // Account summary subscription state (req_id, tags)
    /// The first summary subscription and the tags it asked for.
    pub account_summary_req: Mutex<Option<(i64, Vec<String>)>>,
    /// The venue serves two concurrent summary subscriptions.
    account_summary_other_req: Mutex<Option<(i64, Vec<String>)>>,
    /// When each summary last ran and what it stated, so only changed values
    /// are delivered at the venue's three-minute interval.
    last_account_summary: Mutex<HashMap<i64, SummaryPass>>,

    // News bulletin subscription
    /// Whether broadcast notices were asked for.
    pub bulletin_subscribed: AtomicBool,

    // Account updates subscription
    /// Whether the account's own figures were asked for.
    pub account_updates_subscribed: AtomicBool,
    /// What has already been delivered of what the venue stated, by figure and
    /// currency, so each is delivered once and again when it changes.
    pub last_stated_account: Mutex<HashMap<(String, String), String>>,
    /// Whether the caller has been told the account is fully stated.
    pub account_end_sent: AtomicBool,
    /// Its positions as last stated.
    pub last_portfolio: Mutex<Option<Vec<PositionInfo>>>,

    // Execution replay store
    /// Fills held for a caller who asks for them again.
    pub executions: Mutex<ExecutionStore>,

    // Open order tracking
    /// Every order this client placed and the venue has not finished.
    pub open_orders: Mutex<HashMap<u64, TrackedOrder>>,

    /// Every number holding a book.
    ///
    /// Kept here rather than in the engine because the refusal has to reach
    /// the caller before anything is sent, and because both surfaces read it.
    pub depth_reqs: std::sync::Arc<Mutex<HashSet<i64>>>,

    /// Every number the venue has already worked an order under this session.
    ///
    /// An order that finishes leaves the book, so its number stops naming
    /// anything -- and a placement under it read as a new order rather than
    /// as the duplicate it is. The venue refuses a repeated number only while
    /// it is still working one, so a caller retrying after a fill was given a
    /// second live order instead of a refusal.
    pub spent_order_ids: Mutex<HashSet<u64>>,

    // Market data type callback tracking
    /// Which feed subscriptions default to.
    pub market_data_type: AtomicI32,
    /// Which requests have already been told which feed they are on.
    pub mdt_sent: Mutex<HashSet<i64>>,
    /// The type sent with each instrument's subscription. Every watcher reads
    /// that feed, even when it asked for another type or takes over as holder.
    mdt_by_instrument: Mutex<HashMap<InstrumentId, i32>>,
    /// Which requests asked for their bar times as seconds since the epoch.
    ///
    /// The venue states a time in one form and the client formats it for the
    /// caller. A caller handed the other form is reading a date as a number or
    /// a number as a date. Only the request that asked is affected, so this is
    /// kept per request rather than for the session.
    /// What each historical request asked for: the form its bar times are
    /// wanted in, and the end and duration its range is derived from. The
    /// reply states neither, and the range stated beside the last bar is the
    /// request's own, so the request is what has to be kept.
    historical_asks: Mutex<HashMap<i64, HistoricalAsk>>,
    /// Orders built and not sent, waiting for one that transmits.
    ///
    /// The field saying whether an order goes now is written into the
    /// reference client's own message and never reaches the venue, so nothing
    /// on the wire can hold an order back and this client holds it instead.
    /// Kept in the order they were placed, which is the order they go out in.
    held_orders: Mutex<Vec<HeldOrder>>,

    // Historical data keepUpToDate: req_ids that have completed initial batch.
    // Subsequent bars for these req_ids dispatch as historical_data_update.
    // Cleared when a request is made under the id again
    // `historical_request_is_new`.
    /// Which historical requests have finished their first batch, so
    /// a later bar under the same id is a continuation rather than a new answer.
    pub hist_initial_complete: Mutex<HashSet<u32>>,

    // News subscription state
    /// Every provider this account may read.
    pub news_providers: Mutex<String>,
    /// Which contracts news was asked for on.
    /// Which requests asked for the headlines on a contract.
    ///
    /// Held by request, because the headlines stop when the last caller that
    /// asked for them goes — not when the first one does, and not when the
    /// quotes happen to end. Keyed by the contract rather than by the
    /// instrument, because the venue is asked by contract and the decision to
    /// ask is made before the instrument is known: keyed by instrument, two
    /// requests racing for a contract neither had registered yet both found
    /// nobody had asked, and both asked.
    pub news_askers: Mutex<HashMap<i64, HashSet<i64>>>,

    // Contract cache for enrichment
    /// What the venue has said about each contract, kept so a second
    /// request need not ask again.
    pub contract_cache: Mutex<HashMap<i64, ApiContract>>,
    /// Contracts the venue has named, under the description that was asked
    /// about rather than the one it answered with.
    ///
    /// An order may name its contract by description, and the venue only takes
    /// orders that name it by id, so the description has to be looked up. Asked
    /// again for every order, a program that places a hundred on one contract
    /// sends a hundred lookups for a name that has not changed since the first.
    named_by_description: Mutex<HashMap<String, ApiContract>>,
}

impl Default for ClientCore {
    fn default() -> Self {
        Self::new()
    }
}

/// Why neither question can be answered without the venue having spoken.
pub(crate) const OPTION_MODEL_UNSTATED: &str =
    "the venue has not stated its own model for this contract on this session. Ask for the \
     option's model first — a market-data subscription on the option carries it — and both \
     questions can then be answered against what it said";

/// Years between now and a stated expiry, as `yyyymmdd`.
pub(crate) fn years_to_expiry(expiry: &str) -> Option<f64> {
    let expiry_day = crate::protocol::datetime::day_number(expiry)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let today = now / 86_400;
    let days = expiry_day - today;
    (days > 0).then(|| days as f64 / 365.0)
}


/// The reference client's protocol level this client implements.
///
/// The newest of that client's `MIN_SERVER_VER_*` gates whose feature is
/// carried here. The number was never the venue's — it is the level of the
/// process a program is talking to, which in that client is its gateway and
/// here is this client — so every surface that answers the question answers
/// with this, and a program is told one thing about one client.
///
/// What it overstates is listed on `EClient::server_version`, which is the
/// call a program reads it through.
pub const PROTOCOL_LEVEL: i32 = 217;

/// What an exercise states beyond the instruction itself.
///
/// Three fields an order carries and an exercise did not take, though the same
/// request goes out either way and the same tags carry them: when a person
/// entered it, whose account it is for, and whether that person is a
/// professional. A caller that named any of them was answered as though they
/// had named none.
#[derive(Debug, Default, Clone)]
pub struct ExerciseStates {
    /// When a person entered it, where a person did.
    pub manual_order_time: String,
    /// The account it is taken for, where that is not the one connected.
    pub customer_account: String,
    /// Whether the person it is for is a professional.
    pub professional_customer: bool,
}

impl ClientCore {
    /// An empty one.
    pub fn new() -> Self {
        Self {
            // What a session that never states one waits. The library's own
            // tests state a millisecond; see `set_registration_timeout`.
            registration_timeout: Mutex::new(if cfg!(test) {
                std::time::Duration::from_millis(1)
            } else {
                std::time::Duration::from_secs(5)
            }),
            readonly: std::sync::atomic::AtomicBool::new(false),
            req_to_instrument: Mutex::new(HashMap::new()),
            registering: Mutex::new(std::collections::HashSet::new()),
            withdrawn_while_registering: Mutex::new(std::collections::HashSet::new()),
            registration_epoch: Mutex::new(HashMap::new()),
            epochs: std::sync::atomic::AtomicU64::new(0),
            instrument_to_req: Mutex::new(HashMap::new()),
            tbt_to_instrument: Mutex::new(HashMap::new()),
            instrument_followers: Mutex::new(HashMap::new()),
            con_id_to_instrument: Mutex::new(HashMap::new()),
            display_groups: Mutex::new(HashMap::new()),
            group_subscriptions: Mutex::new(HashMap::new()),
            pending_group_events: Mutex::new(Vec::new()),
            last_quotes: Mutex::new(HashMap::new()),
            snapshot_reqs: Mutex::new(HashMap::new()),
            chargeable_snapshot_reqs: Mutex::new(std::collections::HashSet::new()),
            series_by_req: Mutex::new(HashMap::new()),
            slot_taken_on: Mutex::new(HashMap::new()),
            pnl_req_id: Mutex::new(None),
            pnl_single_reqs: Mutex::new(HashMap::new()),
            last_pnl: Mutex::new([0; 3]),
            last_pnl_single: Mutex::new(HashMap::new()),
            account_summary_req: Mutex::new(None),
            account_summary_other_req: Mutex::new(None),
            last_account_summary: Mutex::new(HashMap::new()),
            bulletin_subscribed: AtomicBool::new(false),
            account_updates_subscribed: AtomicBool::new(false),
            last_stated_account: Mutex::new(HashMap::new()),
            account_end_sent: AtomicBool::new(false),
            last_portfolio: Mutex::new(None),
            executions: Mutex::new(ExecutionStore::default()),
            open_orders: Mutex::new(HashMap::new()),
            spent_order_ids: Mutex::new(HashSet::new()),
            depth_reqs: std::sync::Arc::new(Mutex::new(HashSet::new())),
            market_data_type: AtomicI32::new(1),
            mdt_sent: Mutex::new(HashSet::new()),
            mdt_by_instrument: Mutex::new(HashMap::new()),
            historical_asks: Mutex::new(HashMap::new()),
            held_orders: Mutex::new(Vec::new()),
            hist_initial_complete: Mutex::new(HashSet::new()),
            // Empty until something states them. Which providers an account
            // may read is the venue's answer, given at logon; a pair of codes
            // standing in for it asked for news from providers the account
            // may not be entitled to and left out the ones it is.
            news_providers: Mutex::new(String::new()),
            news_askers: Mutex::new(HashMap::new()),
            contract_cache: Mutex::new(HashMap::new()),
            named_by_description: Mutex::new(HashMap::new()),
        }
    }

    /// Clear all per-session state so the owning client can reconnect.
    /// Refuse this session anything that changes a position.
    pub fn set_readonly(&self, on: bool) {
        self.readonly.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this client refuses anything that would trade.
    pub fn is_readonly(&self) -> bool {
        self.readonly.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The refusal a read-only session gives, naming the call that was made.
    ///
    /// Loud rather than silent: a program that believes it placed an order and
    /// did not is worse off than one that stops.
    pub fn refuse_if_readonly(&self, what: &str) -> Result<(), String> {
        if self.is_readonly() {
            return Err(format!("this session is read-only; {what} was not sent"));
        }
        Ok(())
    }

    /// Keep an order back until one that transmits releases it.
    ///
    /// Nothing is sent and nothing is refused. The reference client's own
    /// bracket sample places a parent and a take-profit this way and lets the
    /// stop-loss send all three, so refusing it here made that sample — the
    /// documented way to place a bracket — impossible.
    pub fn hold_until_transmitted(&self, order_id: u64, parent_id: i64, command: ControlCommand) {
        let mut held = self.held_orders.lock().unwrap();
        held.retain(|other| other.order_id != order_id);
        held.push(HeldOrder { order_id, parent_id, command });
    }

    /// Hold an order back and record it, as one step.
    ///
    /// The hold and the record together are what say an order exists here and
    /// has not been sent, and taken one after the other they were read
    /// between: a withdrawal running in the gap found the hold, took it, found
    /// no record to remove and reported the order withdrawn — and the record
    /// was written behind it, leaving an id that reads as an order the venue
    /// is working while nothing was ever sent. Placing under that id again
    /// then revises an order the venue has never been given.
    pub fn hold_and_track(
        &self,
        order_id: u64,
        parent_id: i64,
        command: ControlCommand,
        contract: ApiContract,
        order: ApiOrder,
        instrument: InstrumentId,
    ) {
        let mut held = self.held_orders.lock().unwrap();
        let mut orders = self.open_orders.lock().unwrap();
        held.retain(|other| other.order_id != order_id);
        held.push(HeldOrder { order_id, parent_id, command });
        orders.insert(order_id, tracked_as_placed(contract, order, instrument));
    }

    /// Forget a placement that was never sent, its record with it, as one
    /// step, and say whether the order is now gone entirely.
    ///
    /// `false` where the id names something else — a revision waiting to be
    /// transmitted, or nothing at all — and the caller still has a cancel to
    /// send. The revision is taken out either way: the order it revises is
    /// live, and the cancel composed for it has an answer coming.
    ///
    /// One step for the reason `hold_and_track` is: the two taken apart let a
    /// placement running between them leave a record behind the withdrawal
    /// that had just reported the order gone.
    pub fn withdraw_held_placement(&self, order_id: u64) -> bool {
        let mut held = self.held_orders.lock().unwrap();
        let mut orders = self.open_orders.lock().unwrap();
        let placement = held.iter()
            .any(|h| h.order_id == order_id && h.places_the_order());
        let before = held.len();
        let mut a_revision_went = false;
        held.retain(|other| {
            if other.order_id != order_id {
                return true;
            }
            if !other.places_the_order() {
                a_revision_went = true;
            }
            false
        });
        let withdrew = held.len() != before;
        if withdrew && placement {
            orders.remove(&order_id);
            // What hangs from a withdrawn placement goes with it. A child left
            // held under it left the hold later, sent as an exit naming a
            // parent the venue was never given, with its record standing here
            // as a working order's.
            let mut parents = vec![order_id];
            while let Some(parent) = parents.pop() {
                let hangs_from = |h: &HeldOrder| h.parent_id == parent as i64 && h.places_the_order();
                let children: Vec<u64> = held.iter().filter(|h| hangs_from(h)).map(|h| h.order_id).collect();
                held.retain(|h| !hangs_from(h));
                for child in children {
                    orders.remove(&child);
                    parents.push(child);
                }
            }
        } else if a_revision_went
            && let Some(tracked) = orders.get_mut(&order_id)
        {
            // A revision that never left this process is not a revision the
            // record may state. Left standing, every later cancel and replace
            // restated a price nothing had ever been given.
            put_back_the_terms(tracked);
        }
        withdrew && placement
    }

    /// Take back an order that never went, and say whether it was held.
    ///
    /// A held order is one the venue was never given, so withdrawing it is not
    /// a message to the venue — it is forgetting a command that had not been
    /// sent. Sent anyway, the venue answers that it knows no such order, and
    /// the command stayed queued to go out later behind something that
    /// transmits: a caller that withdrew a parent and then sent its stop-loss
    /// had the parent it had cancelled placed for it.
    /// Takes what is held under an id out of the hold, and says whether there
    /// was any.
    ///
    /// Only that. What is held leaves the hold to be sent as often as it
    /// leaves to be thrown away, and this cannot tell the two apart — a
    /// restatement put back here was put back on the way out, so the venue
    /// worked the new terms while the record stated the old ones. The two
    /// paths that do throw a revision away say so themselves.
    pub fn withdraw_held(&self, order_id: u64) -> bool {
        let mut held = self.held_orders.lock().unwrap();
        let before = held.len();
        held.retain(|other| other.order_id != order_id);
        held.len() != before
    }

    /// Take back everything held, and say how many there were.
    ///
    /// What a withdrawal of everything means for orders that were never sent.
    /// The record of a placement goes with it: left standing, an id reads as a
    /// working order's, and placing under it again becomes a modify of an order
    /// nothing has ever submitted.
    ///
    /// A held revision of an order the venue is working keeps its record. The
    /// revision goes, because it was never sent; the order it was a revision to
    /// is live, and the withdrawal composed for it has an answer coming.
    /// Dropped along with the revision, that order read as one that was never
    /// placed, and placing under its id again built a fresh submission for an
    /// order already on the market.
    pub fn withdraw_all_held(&self) -> usize {
        let taken: Vec<HeldOrder> = std::mem::take(&mut *self.held_orders.lock().unwrap());
        let mut orders = self.open_orders.lock().unwrap();
        for h in taken.iter().filter(|h| h.places_the_order()) {
            orders.remove(&h.order_id);
        }
        // And the record of each order a discarded revision had restated goes
        // back to what the venue holds: the revision never left this process,
        // so nothing at the venue ever stated its terms.
        for h in taken.iter().filter(|h| !h.places_the_order()) {
            if let Some(tracked) = orders.get_mut(&h.order_id) {
                put_back_the_terms(tracked);
            }
        }
        taken.len()
    }

    /// The family an order that transmits would release, in the order it
    /// goes, each still held under the id it was placed under.
    ///
    /// Gathered without taking anything out of the hold: a sender has to say
    /// what reached the engine and what did not, and can only say it if what
    /// has not gone yet is still there to be withdrawn or sent again. Each
    /// member leaves the hold by its id, once its send is accounted for.
    pub fn family_before(&self, order_id: u64, parent_id: i64) -> Vec<HeldOrder> {
        let held = self.held_orders.lock().unwrap();
        let mut going: Vec<HeldOrder> = Vec::new();
        if parent_id != 0
            && let Some(parent) = held.iter().find(|h| h.order_id == parent_id as u64)
        {
            going.push(HeldOrder {
                order_id: parent.order_id,
                parent_id: parent.parent_id,
                command: parent.command.clone(),
            });
        }
        for h in held.iter() {
            // Beside it under the same parent, or hanging from this one. The
            // parent already gathered cannot also count as a sibling.
            let same_family = (parent_id != 0 && h.parent_id == parent_id)
                || h.parent_id == order_id as i64;
            let same_family = same_family
                && h.order_id != order_id
                && !going.iter().any(|g| g.order_id == h.order_id);
            if same_family {
                going.push(HeldOrder {
                    order_id: h.order_id,
                    parent_id: h.parent_id,
                    command: h.command.clone(),
                });
            }
        }
        going
    }

    /// Send an order and whatever of its family was held, accounting for each.
    ///
    /// The family goes out as one thing or not at all, as far as the engine
    /// allows: it stays held while it is sent, and each member leaves the hold
    /// only once its send is accounted for. Taken out of the hold first and
    /// then sent one by one, a send that failed partway left the parent live
    /// at the venue with its protective children already forgotten, and the
    /// caller was told only that the call had failed.
    ///
    /// `send` says whether the command reached the engine. Answers `Ok` when
    /// everything went, and otherwise the message naming what reached the
    /// engine and what did not.
    pub fn transmit_family(
        &self,
        order_id: u64,
        parent_id: i64,
        own: ControlCommand,
        mut send: impl FnMut(ControlCommand) -> bool,
    ) -> Result<(), String> {
        // The transmitting order leaves the hold whatever happens: it is the
        // one that asked to go, and it cannot stay queued to go out behind a
        // later transmit under the terms this call has just replaced.
        self.withdraw_held(order_id);
        // What a placement releases, and only a placement. A replace states new
        // terms for an order the venue is already working, so nothing is
        // waiting on it: gathered as a placement's family, a caller moving a
        // parent's price had the exit it was still building sent for it,
        // unpaired and unasked.
        let family = if matches!(own, ControlCommand::Order(OrderRequest::Modify { .. })) {
            Vec::new()
        } else {
            self.family_before(order_id, parent_id)
        };
        let mut reached: Vec<u64> = Vec::new();
        for member in family.iter() {
            // The engine takes its commands in order, so a send that did not
            // reach it means none behind it will.
            if !send(member.command.clone()) {
                break;
            }
            reached.push(member.order_id);
        }
        let all_before_went = reached.len() == family.len();
        let own_places = places_the_order(&own);
        let own_went = all_before_went && send(own);
        if all_before_went && own_went {
            for member in &family {
                self.withdraw_held(member.order_id);
            }
            return Ok(());
        }
        // A placement out of the hold that did not reach the engine is not an
        // order the venue is working, and its record goes with the hold. An
        // order is read as live here by being tracked with no placement held
        // for it, so a record left behind put an id that was never sent among
        // the open orders — and placing under it again revised an order the
        // venue has never been given, beside a parent that may be resting
        // there with nothing protecting it.
        let mut forgotten: Vec<u64> = Vec::new();
        // What did not reach the engine comes out of the hold, as what did:
        // left queued, it would go out behind the next thing that transmits,
        // after the caller had been told it did not go. Left held where
        // nothing went at all, so a caller that opens another session on this
        // client can send the family again.
        if !reached.is_empty() {
            for member in &family {
                self.withdraw_held(member.order_id);
                if reached.contains(&member.order_id) {
                    continue;
                }
                if places_the_order(&member.command) {
                    forgotten.push(member.order_id);
                } else {
                    // A revision that did not reach the engine is a revision
                    // nothing has been given, and the record must not state
                    // its terms: every later cancel and replace restates from
                    // there.
                    self.undo_restatement(member.order_id);
                }
            }
        }
        // The transmitting order left the hold at the top whatever happened,
        // so its own record goes here where it did not reach the engine.
        if !own_went && own_places {
            forgotten.push(order_id);
        }
        for id in forgotten {
            self.untrack_order(id);
        }
        let name = |ids: &[u64]| {
            let list = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ");
            if ids.len() == 1 { format!("order {list}") } else { format!("orders {list}") }
        };
        Err(if reached.is_empty() {
            if family.is_empty() {
                format!("the engine stopped before order {order_id} went out: nothing was sent")
            } else {
                let ids: Vec<u64> = family.iter().map(|m| m.order_id).collect();
                format!(
                    "the engine stopped before the family of order {order_id} went out: \
                     nothing was sent, and {} {} still held",
                    name(&ids),
                    if ids.len() == 1 { "is" } else { "are" },
                )
            }
        } else {
            let mut missed: Vec<u64> = family.iter()
                .map(|m| m.order_id)
                .filter(|id| !reached.contains(id))
                .collect();
            if !own_went {
                missed.push(order_id);
            }
            format!(
                "the engine stopped while the family of order {order_id} was going out: \
                 {} reached the engine and may be live at the venue; {} did not reach it",
                name(&reached), name(&missed),
            )
        })
    }

    /// Forget everything this session held, so the next one starts clean.
    pub fn reset(&self) {
        self.req_to_instrument.lock().unwrap().clear();
        // An order held back never reached the venue. Carried into the next
        // session it would go out under an id that session never issued.
        self.held_orders.lock().unwrap().clear();
        self.instrument_to_req.lock().unwrap().clear();
        self.tbt_to_instrument.lock().unwrap().clear();
        self.instrument_followers.lock().unwrap().clear();
        self.con_id_to_instrument.lock().unwrap().clear();
        self.last_quotes.lock().unwrap().clear();
        self.snapshot_reqs.lock().unwrap().clear();
        // And which of them were the venue's one-shot. Kept, a number reused
        // for an ordinary stream on the next session read as a one-shot, and
        // the callers on that contract were served as though it were one.
        self.chargeable_snapshot_reqs.lock().unwrap().clear();
        // And what each of them had asked for. Kept, the next session's
        // request under the same number is read as asking for the series the
        // last one named.
        self.series_by_req.lock().unwrap().clear();
        self.slot_taken_on.lock().unwrap().clear();
        // And which subscription each number was holding. Kept, the next
        // session's first withdrawal under a number reads a figure from the
        // session before it.
        self.registration_epoch.lock().unwrap().clear();
        *self.pnl_req_id.lock().unwrap() = None;
        self.pnl_single_reqs.lock().unwrap().clear();
        *self.last_pnl.lock().unwrap() = [0; 3];
        self.last_pnl_single.lock().unwrap().clear();
        *self.account_summary_req.lock().unwrap() = None;
        *self.account_summary_other_req.lock().unwrap() = None;
        self.last_account_summary.lock().unwrap().clear();
        self.bulletin_subscribed.store(false, Ordering::Relaxed);
        self.account_updates_subscribed.store(false, Ordering::Relaxed);
        self.last_stated_account.lock().unwrap().clear();
        self.account_end_sent.store(false, Ordering::Release);
        *self.last_portfolio.lock().unwrap() = None;
        *self.executions.lock().unwrap() = ExecutionStore::default();
        self.open_orders.lock().unwrap().clear();
        // A new session draws its numbers from the venue again, so what the
        // last one spent says nothing about this one.
        self.spent_order_ids.lock().unwrap().clear();
        self.depth_reqs.lock().unwrap().clear();
        self.market_data_type.store(1, Ordering::Relaxed);
        self.mdt_sent.lock().unwrap().clear();
        self.mdt_by_instrument.lock().unwrap().clear();
        self.historical_asks.lock().unwrap().clear();
        self.hist_initial_complete.lock().unwrap().clear();
        self.news_providers.lock().unwrap().clear();
        self.news_askers.lock().unwrap().clear();
        self.contract_cache.lock().unwrap().clear();
        // What the venue named for a description belongs to the session that
        // asked. Kept across a reconnect — or a login as somebody else — the
        // next order goes out under an id this session was never given.
        self.named_by_description.lock().unwrap().clear();
        // A group this session joined, and what it was told about it. Kept,
        // the next session is called back about a group under a request id it
        // never subscribed with.
        self.display_groups.lock().unwrap().clear();
        self.group_subscriptions.lock().unwrap().clear();
        self.pending_group_events.lock().unwrap().clear();
    }

    // ── Registration helpers ──

    /// What this session waits, stated when it opened.
    ///
    /// The library's own tests wait a millisecond: a test with no engine to
    /// answer would otherwise wait the full default on every call.
    pub fn set_registration_timeout(&self, waiting: std::time::Duration) {
        *self.registration_timeout.lock().unwrap() = waiting;
    }

    fn registration_timeout(&self) -> std::time::Duration {
        *self.registration_timeout.lock().unwrap()
    }

    /// Wait for the hot loop to register a contract, and answer with the slot
    /// it gave. A full instrument table comes back as an `Err` for this
    /// request alone; the engine keeps running.
    fn recv_registration<E: Into<Refusal>>(
        &self, reply_rx: std::sync::mpsc::Receiver<Result<InstrumentId, E>>,
    ) -> Result<InstrumentId, Refusal> {
        use std::sync::mpsc::RecvTimeoutError;
        reply_rx.recv_timeout(self.registration_timeout())
            .map_err(|why| match why {
                // The engine took the command and then went. That is a session
                // to reopen, not a venue that stayed silent, and a caller
                // branching on the code has to be able to tell them apart.
                RecvTimeoutError::Disconnected => {
                    Refusal::not_connected("Engine stopped before it answered")
                }
                RecvTimeoutError::Timeout => Refusal::no_answer("Registration timed out"),
            })?
            // Keep the number the engine refused this request under.
            .map_err(Into::into)
    }

    /// Which slot this contract holds, as far as this client knows.
    ///
    /// `0` means the contract carries no conId and answers for no one: the
    /// engine resolves those by descriptor, so only it can say which slot
    /// they got.
    ///
    /// Takes the session's state because it drops what the engine has given
    /// back before it answers. Every reader needs that and one of them will
    /// always forget, so the drop happens here rather than at each of them: a
    /// freed slot goes to the next contract that needs one, and an answer from
    /// here after that names a contract the caller never asked about.
    pub(crate) fn cached_instrument(&self, shared: &SharedState, con_id: i64) -> Option<InstrumentId> {
        self.forget_released_slots(shared);
        if con_id == 0 {
            return None;
        }
        self.con_id_to_instrument.lock().unwrap().get(&con_id).copied()
    }

    /// Cache the engine's answer for later lookups. Caching it under `0` would
    /// point every conId-less contract at the first one's slot.
    fn cache_instrument(&self, con_id: i64, instrument: InstrumentId) {
        if con_id != 0 {
            self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument);
        }
    }

    /// Whether somebody already holds this contract, in which case this
    /// request watches theirs.
    ///
    /// Asks only. A contract nobody holds is not taken here: this runs as a
    /// question about a cached contract, and a subscription that goes on to
    /// fail would leave a holder recorded for a request that never started —
    /// which nothing then cancels. [`take_or_follow`](Self::take_or_follow) is
    /// where it is taken.
    pub(crate) fn follows_existing_subscription(
        &self, instrument: InstrumentId, req_id: i64, series: &[u32],
    ) -> bool {
        let mut own = self.ownership();
        let held = own.holders.get(&instrument).copied();
        match held {
            // Nothing follows the venue's chargeable one-shot. It is answered
            // once and withdrawn as soon as it completes, so a stream that
            // followed one was never sent — and when the one-shot went, the
            // follower was promoted onto its row and heard no quotes at all.
            Some(existing) if existing != req_id && own.one_shot.contains(&existing) => false,
            Some(existing) if existing != req_id => {
                own.watches(instrument, req_id);
                own.by_req.insert(req_id, instrument);
                // With the join, not before it. See `take_or_follow`.
                if !series.is_empty() {
                    own.series.insert(req_id, series.to_vec());
                }
                drop(own);
                self.stamp_registration(req_id);
                true
            }
            _ => false,
        }
    }

    /// The maps that say who is watching what, taken together.
    ///
    /// Four maps answer one question between them, and every decision about a
    /// subscription reads some of them and writes others. Taken one at a time,
    /// two decisions interleave inside one answer: a withdrawal that read the
    /// watchers before it took the holder map took a subscription down under a
    /// request that had just joined it; a request put back on a new slot was
    /// put back after its caller had withdrawn it; and a one-shot that gave up
    /// its kind before it gave up the slot handed its finite burst to a stream
    /// that was never sent. So they are taken together, always in this order,
    /// and nothing between here and the end of a decision takes any of them
    /// again.
    fn ownership(&self) -> Ownership<'_> {
        Ownership {
            holders: self.instrument_to_req.lock().unwrap(),
            following: self.instrument_followers.lock().unwrap(),
            by_req: self.req_to_instrument.lock().unwrap(),
            one_shot: self.chargeable_snapshot_reqs.lock().unwrap(),
            series: self.series_by_req.lock().unwrap(),
            taken_on: self.slot_taken_on.lock().unwrap(),
            epoch: self.registration_epoch.lock().unwrap(),
        }
    }

    /// Hold this contract, or follow whoever took it first.
    ///
    /// `instrument_to_req` maps one request per instrument: a second holder
    /// would clobber the first's reverse mapping and orphan it silently — no
    /// ticks, no error. Deciding and taking happen under one acquisition, so
    /// two callers subscribing the same unheld contract cannot both take it
    /// and leave the loser cancelling the winner's feed.
    ///
    /// Answers whether this request ended up a follower.
    pub(crate) fn take_or_follow(
        &self, instrument: InstrumentId, req_id: i64, series: &[u32], asked_on: u64,
    ) -> bool {
        let joined = self.ownership().take_or_follow(instrument, req_id, series, asked_on);
        // The request that takes a slot nobody held is written down by whoever
        // asked for it, once the venue has answered; the other two are already
        // watching something and are stamped here.
        if joined != Joined::Took {
            self.stamp_registration(req_id);
        }
        joined == Joined::Watching
    }

    /// Point everything watching one slot at another.
    ///
    /// The engine says so when a lookup names a contract another slot already
    /// holds: only one subscription per contract exists on the wire, so the
    /// callers given the second slot have to read the first, or their quotes
    /// arrive on a slot nothing is watching.
    ///
    /// Answers with the contract on the destination and the number to withdraw
    /// it under, where nothing is watching it once the move is read. Nothing may be: the subscription
    /// there is held up while a caller is on its way onto it, and that caller
    /// can withdraw before it arrives — so this is what says the subscription
    /// is nobody's now.
    ///
    /// The number comes from the same acquisition that finds nobody watching,
    /// not from the moment the withdrawal is sent: taken at the send, a caller
    /// that asked for the contract in between had asked first and this
    /// withdrawal still read as the later of the two, so its brand-new
    /// subscription was taken down under it.
    pub(crate) fn move_watchers(
        &self, shared: &SharedState, from: InstrumentId, into: InstrumentId, held_under: u64,
    ) -> Option<(i64, u64)> {
        {
            let mut modes = self.mdt_by_instrument.lock().unwrap();
            if let Some(mode) = modes.remove(&from) {
                modes.entry(into).or_insert(mode);
            }
        }
        // The whole move under one acquisition of the ownership maps: a
        // withdrawal that ran between taking the watchers off the old slot and
        // putting them on the new one found nobody watching either, so it took
        // the subscription down — and the request it had just withdrawn was
        // put back on the new slot, live again under a number its caller had
        // given up.
        let mut moved: Vec<i64> = Vec::new();
        // The occupancy the slot they are moving onto is already held under,
        // not a number of this client's own: a caller that arrives holding a
        // number the engine never heard of could never withdraw what it is
        // being served off.
        let taken_on = if held_under != 0 { held_under } else { self.in_order() };
        {
            let mut own = self.ownership();
            let held = own.holders.remove(&from);
            own.taken_on.remove(&from);
            let watchers = own.following.remove(&from).unwrap_or_default();
            for req_id in held.into_iter().chain(watchers) {
                // Only the ones still watching the slot that is moving. One
                // withdrawn while this was under way is watching nothing.
                if own.by_req.get(&req_id) != Some(&from) {
                    continue;
                }
                own.take_or_follow(into, req_id, &[], taken_on);
                own.taken_on.insert(into, taken_on);
                own.by_req.insert(req_id, into);
                // Under the same acquisition that moves it: a number stamped
                // after the maps were released was stamped for a request that
                // had since been withdrawn.
                own.epoch.insert(req_id, self.epochs.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                moved.push(req_id);
            }
        }
        for req_id in moved {
            // Only where it is still watching what it was moved onto. A
            // withdrawal running since the move was recorded leaves a number
            // watching nothing, and what is owed a joiner was queued under
            // that number anyway: the caller was answered after it had given
            // the number up, and where the number had been handed out again it
            // was answered about a contract it never asked for.
            if self.watching(req_id) != Some(into) {
                continue;
            }
            // Moved onto somebody else's subscription is joining one, and what
            // a joiner is owed is owed here too. Only the slot they left was
            // cleared, so they arrived on a contract whose baseline already
            // matched its quote and heard nothing until it next moved, were
            // never told the increment it was acknowledged with, and where it
            // had been refused were not told that either.
            self.mdt_sent.lock().unwrap().remove(&req_id);
            self.pay_a_joiner(shared, into, req_id);
        }
        self.last_quotes.lock().unwrap().remove(&from);
        // Whether anything is being served off the destination now. Nothing
        // may be: the request this move was for can withdraw before the move
        // is read, and the subscription was kept up for it.
        // Both under one acquisition: a caller that asks for the contract after
        // this answer is numbered later than the withdrawal the answer calls
        // for, which is what keeps its subscription standing.
        let on_the_slot = self.cached_con_id_of(into);
        let own = self.ownership();
        let watched = own.holders.contains_key(&into)
            || own.following.get(&into).is_some_and(|watchers| !watchers.is_empty());
        if watched {
            return None;
        }
        Some((on_the_slot, self.in_order()))
    }

    /// What a request that joins a subscription somebody else opened is owed.
    ///
    /// It asked the venue for nothing, so the venue answers it with nothing:
    /// the acknowledgement went to whoever opened the subscription, a refusal
    /// was drained when it arrived, and the quote is only stated where it
    /// differs from a baseline this request had no part in setting. Each of
    /// those has to be handed over here or the caller holds a number that
    /// reads as subscribed and hears nothing on it.
    ///
    /// There is more than one way into following — the contract may be known
    /// here, or the engine may be the first to know which slot it holds, which
    /// is every contract named by symbol alone. Paid on one of them and not
    /// the other, the second was the ordinary path for those contracts.
    fn pay_a_joiner(&self, shared: &SharedState, instrument: InstrumentId, req_id: i64) {
        // The venue sends a tickReqParams per reqMktData; a follower asked for
        // none, so it is owed the increment the live subscription was
        // acknowledged with. Before that acknowledgement there is none yet,
        // and the pending one fans out to this follower when it arrives.
        if let Some(min_tick) = shared.market.min_tick_for_follower(instrument) {
            shared.market.push_tick_req_params_for(req_id, min_tick);
        }
        // And where the subscription this one joins was refused, it is refused
        // too. The refusal is drained once and told to whoever held the
        // contract then; a request joining afterwards heard nothing and
        // received nothing — it had joined a subscription the venue had
        // already declined, and nothing was ever going to arrive on it.
        if let Some(reason) = shared.market.failure_for_follower(instrument) {
            shared.market.push_subscription_failure_for(req_id, reason);
        }
        // And it is owed the quote as it stands. The ticks are worked out once
        // per contract, against what was last sent for that contract, and
        // fanned to everyone watching it — so a request joining a contract
        // whose baseline already matches the quote was sent nothing at all,
        // and on a contract that is not moving it stayed that way. Forgetting
        // the baseline is what makes the next pass state everything the venue
        // has said, which is what a subscription is answered with; it is the
        // same mechanism a market-data drop uses. Everyone already watching
        // hears those values restated, which is what they are holding.
        self.last_quotes.lock().unwrap().remove(&instrument);
    }

    /// Forget everyone recorded as watching a slot the engine has taken back.
    ///
    /// The slot goes to the next contract that needs one, and the requests
    /// that named it went on naming it — so the next contract found one of
    /// them already holding its slot: it arrived as that request's follower,
    /// and its quotes went to a caller whose own subscription was over.
    ///
    /// Called only from [`ClientCore::forget_released_slots`], because the
    /// engine giving the slot back is the one thing that means this. A
    /// subscription can be refused and stand — one acknowledged with an
    /// increment prices cannot be counted in is refused to its caller and
    /// keeps its slot — and forgetting on the refusal would strand it.
    fn forget_watchers_of(&self, instrument: InstrumentId, released_at: u64) -> bool {
        // One acquisition for the lot. See `ownership`.
        let forgotten: Vec<i64> = {
            let mut own = self.ownership();
            // Not a slot this client has since been given again. The release
            // names the occupancy that ended, so what is compared is which
            // occupancy this is and not which of two decisions came first.
            // Where the release names none — a slot given back for a reason
            // that never reached a subscription — there is nothing to hold on
            // to and the records go.
            if released_at != 0
                && own.taken_on.get(&instrument).is_some_and(|taken| *taken != released_at)
            {
                return false;
            }
            own.taken_on.remove(&instrument);
            let held = own.holders.remove(&instrument);
            let watchers = own.following.remove(&instrument).unwrap_or_default();
            let watching: Vec<i64> = held.into_iter().chain(watchers).collect();
            for req_id in &watching {
                // Only where it still points here. A request that has since
                // been pointed somewhere else is watching that, not this.
                if own.by_req.get(req_id) == Some(&instrument) {
                    own.by_req.remove(req_id);
                }
                own.one_shot.remove(req_id);
                own.series.remove(req_id);
                own.epoch.remove(req_id);
            }
            watching
        };
        self.mdt_by_instrument.lock().unwrap().remove(&instrument);
        for req_id in forgotten {
            self.mdt_sent.lock().unwrap().remove(&req_id);
            // And everything else filed under that number. The slot going back
            // ends the request, and a number that outlives its request with
            // its marks still standing is read as the request it was: reused
            // for an ordinary stream it was withdrawn as a snapshot the moment
            // it had both sides of a quote, or read as the venue's one-shot by
            // every later join, or counted as still asking for a series its
            // caller has no subscription to hear.
            self.snapshot_reqs.lock().unwrap().remove(&req_id);

        }
        self.last_quotes.lock().unwrap().remove(&instrument);
        true
    }

    /// Every request watching a contract, the one holding the subscription
    /// first.
    ///
    /// Read under one acquisition, because the holder and the watchers are one
    /// answer. Read separately, a withdrawal running between the two left a
    /// list that never existed: the request that had just handed the
    /// subscription on was told what the venue said and the request that had
    /// just taken it was not.
    pub fn watchers_of(&self, instrument: InstrumentId) -> Vec<i64> {
        let own = self.ownership();
        own.holders
            .get(&instrument)
            .copied()
            .into_iter()
            .chain(own.following.get(&instrument).cloned().unwrap_or_default())
            .collect()
    }

    /// Every other request watching a contract, so one quote reaches them all.
    pub fn followers_of(&self, instrument: InstrumentId) -> Vec<i64> {
        self.instrument_followers
            .lock()
            .unwrap()
            .get(&instrument)
            .cloned()
            .unwrap_or_default()
    }

    /// Forget the slots the engine has given back.
    ///
    /// The cache below answers "which slot does this contract hold" without
    /// asking the engine, which is what keeps a placement off a round trip. A
    /// slot the engine has freed goes to the next contract that needs one, so
    /// an answer from here after that names another contract altogether: the
    /// order was recorded against the new occupant and its fill moved that
    /// contract's position.
    pub fn forget_released_slots(&self, shared: &SharedState) {
        let released = shared.market.take_released_slots();
        if released.is_empty() {
            return;
        }
        // Everything that named the slot. The cache alone was forgotten, so
        // the requests that had been watching it were still recorded against
        // it when the next contract took it.
        //
        // Each release is read against when this client last took that slot: a
        // slot given back goes to the next contract that needs one, and a
        // release read after that named a slot this client had just been given
        // again. Forgotten then, the contract now on it was streaming with no
        // request able to hear it or withdraw it.
        let mut forgotten: Vec<InstrumentId> = Vec::new();
        for (slot, released_at) in released {
            if self.forget_watchers_of(slot, released_at) {
                forgotten.push(slot);
            }
        }
        if forgotten.is_empty() {
            return;
        }
        // By the slot, so a contract the venue has not named — which holds a
        // slot under no id at all — is forgotten with the rest.
        let mut cache = self.con_id_to_instrument.lock().unwrap();
        cache.retain(|_, held| !forgotten.contains(held));
    }

    /// Find instrument ID for a contract, registering if needed.
    /// Returns `Err` if the control channel is closed.
    pub fn find_or_register_instrument(
        &self,
        shared: &SharedState,
        control_tx: &SyncSender<ControlCommand>,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        identity: &str,
    ) -> Result<InstrumentId, Refusal> {
        // The cache is skipped when the caller states an identity, because the
        // slot may have been allocated by a market-data subscription that had
        // none — and the engine is where the identity is stored. Short-circuiting
        // here sent the order with a correct security type and destination but no
        // expiry, so a future named its exchange and not its month. Registration
        // is idempotent: the engine returns the same slot and adopts the identity.
        if identity.is_empty()
            && let Some(iid) = self.cached_instrument(shared, con_id) {
                return Ok(iid);
            }

        // Register new — only allocates an InstrumentId slot, does not subscribe to
        // market data.
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        control_tx.send(ControlCommand::RegisterInstrument {
            contract: ContractRef { con_id, symbol: symbol.to_string(), sec_type: sec_type.to_string(), exchange: exchange.to_string(), ..Default::default() },
            identity: identity.to_string(),
            reply_tx: Some(reply_tx),
        }).map_err(|e| Refusal::not_connected(format!("Engine stopped: {e}")))?;

        let id = self.recv_registration(reply_rx)?;
        self.cache_instrument(con_id, id);
        Ok(id)
    }

    /// A request is being made under this id, so whatever a request under it
    /// finished before is over.
    ///
    /// Bars answering a fresh request were delivered as though they continued
    /// the last one — as updates, with no completion — because the id had
    /// been marked finished and nothing unmarked it. A caller looping over
    /// contracts under one id was answered once and never again.
    pub fn historical_request_is_new(&self, req_id: u32) {
        self.hist_initial_complete.lock().unwrap().remove(&req_id);
    }

    // ── Subscription management ──

    /// Record that this request asked for the headlines on an instrument.
    ///
    /// Called on every way out of a registration. Registration leaves by more
    /// than one door — the quotes may already be up, and may come up while
    /// this request was being made — and the subscription is sent before any
    /// of them. A door that does not record it sends headlines nothing will
    /// ever withdraw.
    /// Record that this request wants the headlines on a contract, and say
    /// whether it is the first to ask.
    ///
    /// Decided and recorded together, so two requests racing for one contract
    /// cannot both find that nobody has asked. The venue is asked by contract
    /// and withdrawn by contract, so asking twice leaves a subscription the
    /// one withdrawal cannot match.
    pub(crate) fn first_to_ask_for_news(&self, con_id: i64, req_id: i64) -> bool {
        let mut news = self.news_askers.lock().unwrap();
        let askers = news.entry(con_id).or_default();
        let first = askers.is_empty();
        askers.insert(req_id);
        first
    }

    /// Forget every caller that asked for news on a contract the venue
    /// refused. A later ask is then the first again and sends anew, rather
    /// than being deduped against a claim the venue already declined.
    pub(crate) fn release_news_askers(&self, con_id: i64) {
        self.news_askers.lock().unwrap().remove(&con_id);
    }

    /// Give back what a registration took before it was refused.
    ///
    /// Everything written down between the first word of a registration and
    /// the slot it ends with is a lease this number holds, and a caller told
    /// its request did not happen holds none of them. The marks outlive the
    /// request that left them: a number recorded as having bought a one-shot
    /// is never followed, so the same number handed out again for an ordinary
    /// stream keeps the next caller on that contract out of the followers, and
    /// a contract recorded as already asked for headlines is deduped against a
    /// claim nobody holds and never asks again.
    fn give_back_what_this_request_took(&self, con_id: i64, req_id: i64) {
        self.release_news_askers(con_id);
        self.chargeable_snapshot_reqs.lock().unwrap().remove(&req_id);
        self.series_by_req.lock().unwrap().remove(&req_id);
    }

    /// Register a market data subscription mapping.
    /// If `generic_tick_list` contains "292", also subscribes to per-contract news.
    pub fn register_mkt_data(
        &self,
        shared: &SharedState,
        control_tx: &SyncSender<ControlCommand>,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        currency: &str,
        filters: &crate::types::SecDefFilters,
        snapshot: bool,
        regulatory_snapshot: bool,
        generic_tick_list: &str,
        mode_9887: i32,
    ) -> Result<InstrumentId, Refusal> {
        // A number already watching something cannot watch a second thing.
        //
        // Nothing refused it, and the two maps that answer "who is watching
        // this contract" and "what is this request watching" then disagreed:
        // the second contract took the request, and the first was left in the
        // one map the delivery loop reads. The caller was handed both
        // contracts' ticks under one number with nothing to tell them apart,
        // its withdrawal reached only the second, and a second withdrawal
        // reached nothing at all — so the first went on arriving under a
        // number the caller had cancelled, for the life of the session.
        //
        // Refused here, before anything is sent: the venue is asked before the
        // slot this request would take is known, so there is no later point at
        // which refusing leaves nothing behind.
        // Read and claimed under one lock, so a second caller on this number
        // cannot pass the check while the first is still waiting to be given a
        // slot. Held until this call returns, however it returns.
        let claim = {
            let watching = self.req_to_instrument.lock().unwrap();
            let mut taking = self.registering.lock().unwrap();
            if watching.contains_key(&req_id) || !taking.insert((TAKING_QUOTES, req_id)) {
                return Err(Refusal::stated(
                    DUPLICATE_TICKER_ID,
                    format!(
                        "request {req_id} is already watching a contract: withdraw it before \
                         asking for another under the same number",
                    ),
                ));
            }
            Registering {
                held: &self.registering,
                withdrawn: &self.withdrawn_while_registering,
                key: (TAKING_QUOTES, req_id),
            }
        };

        // A quote feed the engine has given up on serves nothing more this
        // session. Refused here rather than beside the socket, because the two
        // ways a request can be told it has a subscription part company before
        // then: a request joining a contract already watched is answered from
        // this side and never reaches the engine at all, so a guard there
        // cannot see it. Both were told they had one — the joiner off a
        // subscription that had stopped, the new one off a slot taken and a
        // request recorded for a replay that is not coming — and both waited
        // out the session for a first tick.
        if let Some(why) = shared.market.market_data_over() {
            return Err(Refusal::not_connected(format!(
                "market data is unavailable for the rest of this session: {why}",
            )));
        }

        // The chargeable snapshot is one burst by construction, so it ends the
        // way an ordinary snapshot does and the caller hears the same end.
        let snapshot = snapshot || regulatory_snapshot;
        // Written down before anything can follow this request: what it asked
        // for decides whether it may be followed at all.
        if regulatory_snapshot {
            self.chargeable_snapshot_reqs.lock().unwrap().insert(req_id);
        }
        // News subscription if generic_tick_list names 292. The whole entry,
        // not its last three characters: "1292" is not 292, and matching on a
        // suffix subscribes to news the caller did not ask for. The list is
        // split on commas first, so a comma-joined entry never matches.
        let wants_news = generic_tick_list.split(',').any(|t| t.trim() == "292");
        // And nothing else is served. Not because the protocol cannot carry
        // it: a tick is asked for as a subscription of its own, under the
        // venue's number for it in the request type, which is how the option
        // model, the trading status and the venue map are already asked for
        // here. What is missing is the venue's number for each of the numbers
        // a caller names, and a reader for what each one answers with.
        //
        // So every other number is reported rather than accepted in silence —
        // option volume, shortable shares. A caller hears that it will not
        // arrive instead of watching for a tick that never comes.
        // Each remaining entry is asked for. The number a caller states is the
        // venue's own number for the series, so there is nothing to translate:
        // it goes out as a subscription of its own under that number, the way
        // the option model, the trading status and the venue map already do.
        //
        // An entry that is not a number is not one of the venue's series, and
        // saying so is better than sending it and having the whole request
        // refused for the sake of one bad word in the list.
        let mut generic_ticks: Vec<u32> = Vec::new();
        let mut unread: Vec<&str> = Vec::new();
        for entry in generic_tick_list.split(',').map(str::trim) {
            if entry.is_empty() || entry == "292" || entry == "mdoff" {
                continue;
            }
            match entry.parse::<u32>() {
                Ok(tick) if !generic_ticks.contains(&tick) => generic_ticks.push(tick),
                Ok(_) => {}
                Err(_) => unread.push(entry),
            }
        }
        if !unread.is_empty() {
            log::warn!(
                "the generic tick list named {}, which is not a number the venue \
                 knows a series by, so nothing was asked for it",
                unread.join(", "),
            );
        }
        // Asked for once per contract, whoever asks. Recorded as the decision
        // is made, so two callers racing for one contract cannot both find
        // that nobody has asked.
        if wants_news && self.first_to_ask_for_news(con_id, req_id) {
            // What the logon said this account may read, unless a caller has
            // named its own set. The venue separates codes with a star.
            let named = self.news_providers.lock().unwrap().clone();
            let providers = if named.is_empty() {
                shared.reference.news_providers()
                    .iter()
                    .map(|p| p.code.as_str())
                    .collect::<Vec<_>>()
                    .join("*")
            } else {
                named
            };
            // Reported, not discarded. A send fails because the engine is
            // gone, and the branch below returns success for a contract
            // somebody else already watches without sending anything else — so
            // a caller that asked for headlines was told it had them while
            // nothing had reached the engine at all. The record of who asked
            // goes back with it, or the next ask is deduped against this one.
            if let Err(gone) = control_tx.send(ControlCommand::SubscribeNews {
                con_id,
                symbol: symbol.to_string(),
                sec_type: sec_type.to_string(),
                providers,
                reply_tx: None,
            }) {
                self.give_back_what_this_request_took(con_id, req_id);
                return Err(Refusal::not_connected(format!("Engine stopped: {gone}")));
            }
        }

        // A contract already being watched needs no second subscription: this
        // caller watches the one that is up, and hears the same quotes under
        // its own request. Nothing goes to the engine.
        //
        // Except a chargeable snapshot, which is a request of its own and not
        // a share of somebody's stream. Following one instead sends nothing,
        // bills nothing, and lets an account with no entitlement hear an end
        // it was never refused — off a stream it did not ask for.
        if !regulatory_snapshot
            && let Some(instrument) = self.cached_instrument(shared, con_id)
            && self.follows_existing_subscription(instrument, req_id, &generic_ticks)
        {
            if snapshot {
                self.snapshot_reqs.lock().unwrap().insert(req_id, (std::time::Instant::now(), 0));
            }
            self.pay_a_joiner(shared, instrument, req_id);
            // And the series this caller named. The list that went to the
            // venue is the first caller's, so a joiner naming a series nobody
            // has asked for waited on a stream that was never requested. What
            // is already being served is not asked for again — the engine
            // holds what was asked and sends only the difference.
            if !generic_ticks.is_empty() {
                let _ = control_tx.send(ControlCommand::AlsoAskForSeries {
                    instrument,
                    con_id,
                    generic_ticks: generic_ticks.clone(),
                    issued: self.in_order(),
                });
            }
            // The news subscription was sent above whether or not the quotes
            // were already up, so it is recorded here as well. Recorded only
            // on the path that also opened the quotes, it was never withdrawn:
            // the caller stopped watching and the headlines kept coming.
            return self.settle_registration(shared, control_tx, &claim, req_id, instrument);
        }

        // The number this registration asks under, kept: the slot it is given
        // is recorded under the same number. Recorded under a fresh one taken
        // when the engine answers, a registration the engine then gave up on
        // was read as a later occupancy than the release that freed it — so
        // nothing was forgotten, and the next contract to take that slot was
        // reachable under the failed request's records.
        let asked_on = self.in_order();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        // Both sends give back what this request took before they report the
        // engine gone. Returned straight, they left the mark that says this
        // number bought a one-shot standing against a number nothing is
        // watching under — so the same number handed out again for an ordinary
        // stream read as a one-shot, and the next caller on that contract was
        // kept out of the followers and heard nothing.
        control_tx.send(ControlCommand::RegisterInstrument {
            contract: ContractRef { con_id, symbol: symbol.to_string(), sec_type: sec_type.to_string(), exchange: exchange.to_string(), ..Default::default() },
            identity: String::new(),
            reply_tx: None,
        }).map_err(|e| {
            self.give_back_what_this_request_took(con_id, req_id);
            Refusal::not_connected(format!("Engine stopped: {e}"))
        })?;
        control_tx.send(ControlCommand::Subscribe {
            contract: ContractRef {
                con_id, symbol: symbol.to_string(), exchange: exchange.to_string(),
                sec_type: sec_type.to_string(), currency: currency.to_string(),
                last_trade_date: filters.last_trade_date_or_contract_month.clone(),
                strike: filters.strike, right: filters.right.clone(),
                multiplier: filters.multiplier.clone(),
            },
            filters: filters.clone(),
            mode_9887,
            regulatory_snapshot,
            generic_ticks: generic_ticks.clone(),
            reply_tx: Some(reply_tx),
            issued: asked_on,
        }).map_err(|e| {
            self.give_back_what_this_request_took(con_id, req_id);
            Refusal::not_connected(format!("Engine stopped: {e}"))
        })?;

        // The engine answers this one. A conId-less contract has no client-side
        // identity, so a duplicate can only be settled against the slot the
        // engine resolved — and refusing here, after `Subscribe` had already
        // gone out, left a live subscription the caller was told did not happen
        // and held no req_id to cancel by. The engine now refuses
        // before the subscribe reaches the wire and that refusal arrives here.
        let instrument_id = match self.recv_registration(reply_rx) {
            Ok(id) => id,
            Err(refused) => {
                // The headlines were asked for before this could fail, and the
                // record of who asked is what decides whether the next caller
                // sends the request at all. Left standing for a request that
                // never started, this contract's headlines are never asked for
                // again — the next caller reads somebody as already watching —
                // and the subscription that did go out cannot be withdrawn,
                // because the path that withdraws it needs a request this one
                // no longer has.
                // Named by contract, because this side has no slot for it: the
                // headlines went out before the contract was registered and
                // the registration is what just failed, so the mapping a slot
                // would come from was never written. Named by slot it resolved
                // to nothing, the withdrawal was never sent, and the headlines
                // ran for the rest of the session with the record of who asked
                // already dropped — so no later withdrawal reached them
                // either, and the next request for the contract opened a
                // second subscription on the wire beside the first.
                if wants_news
                    && let Some(subject) = self.release_news(shared, req_id)
                {
                    let _ = control_tx.send(ControlCommand::UnsubscribeNews { subject });
                }
                // And what this request was marked as and what it had asked
                // for, because no record of it remains for a withdrawal to
                // clean up. Left behind, the number reused for an ordinary
                // stream read as the venue's one-shot and the callers on that
                // contract were served as though it were one — and a series
                // this request never got went on being served because a
                // number nobody holds was still counted as asking for it.
                self.chargeable_snapshot_reqs.lock().unwrap().remove(&req_id);
                self.series_by_req.lock().unwrap().remove(&req_id);
                return Err(refused);
            }
        };
        self.cache_instrument(con_id, instrument_id);
        // The contract may have been named only by symbol, in which case the
        // engine is the first to know which slot it holds — and it may already
        // be watched. This caller watches it too rather than taking it over —
        // unless it asked for the chargeable snapshot, which is its own
        // request and was already sent above.
        if !regulatory_snapshot
            && self.follows_existing_subscription(instrument_id, req_id, &generic_ticks)
        {
            if snapshot {
                self.snapshot_reqs.lock().unwrap().insert(req_id, (std::time::Instant::now(), 0));
            }
            self.pay_a_joiner(shared, instrument_id, req_id);
            // And the series this caller named, as on the path that never
            // reached the engine: the list that went to the venue is the
            // holder's.
            if !generic_ticks.is_empty() {
                let _ = control_tx.send(ControlCommand::AlsoAskForSeries {
                    instrument: instrument_id,
                    con_id,
                    generic_ticks: generic_ticks.clone(),
                    issued: self.in_order(),
                });
            }
            return self.settle_registration(shared, control_tx, &claim, req_id, instrument_id);
        }
        // Somebody may have taken this contract while this request was being
        // registered, in which case this one watches theirs. Either way the
        // record of what it is watching is written there, under the maps that
        // decide it.
        let _ = self.take_or_follow(instrument_id, req_id, &generic_ticks, asked_on);
        self.stamp_registration(req_id);
        // A concurrent registration may already hold the subscription. Its
        // mode still describes the feed everyone on this instrument receives.
        self.mdt_by_instrument.lock().unwrap().entry(instrument_id).or_insert(
            match mode_9887 {
                1 => MDT_DELAYED,
                2 => MDT_FROZEN,
                3 => MDT_DELAYED_FROZEN,
                _ => self.market_data_type.load(Ordering::Relaxed),
            },
        );
        if snapshot {
            self.snapshot_reqs.lock().unwrap().insert(req_id, (std::time::Instant::now(), 0));
        }
        self.settle_registration(shared, control_tx, &claim, req_id, instrument_id)
    }

    /// Publish what a registration opened, or take it back down because it was
    /// withdrawn while it was away.
    ///
    /// The record a withdrawal reads is written when the engine's answer comes
    /// back, so a withdrawal arriving before that has nothing to act on. It is
    /// answered all the same — the venue does not refuse one for arriving
    /// early — and written down against the number instead. This is where that
    /// is read, as the claim is given back and under the same acquisition, so
    /// a withdrawal is either early enough to be seen here or late enough to
    /// find the record it needs.
    ///
    /// Taken down through the ordinary withdrawal, because that is the one
    /// that knows a subscription somebody else is also watching stays up and
    /// passes to the next of them.
    fn settle_registration(
        &self,
        shared: &SharedState,
        control_tx: &SyncSender<ControlCommand>,
        claim: &Registering<'_>,
        req_id: i64,
        instrument: InstrumentId,
    ) -> Result<InstrumentId, Refusal> {
        if !claim.withdrawn_meanwhile() {
            return Ok(instrument);
        }
        let withdrawn = self.unregister_mkt_data(shared, req_id);
        // Nothing is reported from here. The withdrawal was answered when it
        // arrived, and a send failing now fails because the engine has gone —
        // which takes the subscription with it.
        if let Some(subject) = withdrawn.headlines {
            let _ = control_tx.send(ControlCommand::UnsubscribeNews { subject });
        }
        let series = withdrawn.series;
        if let Some(subscription) = withdrawn.subscription {
            let _ = control_tx.send(ControlCommand::Unsubscribe {
                instrument: subscription,
                con_id: withdrawn.con_id,
                took_it: withdrawn.took_it,
                series: series.map(|(_, ticks)| ticks).unwrap_or_default(),
                issued: withdrawn.decided_at,
            });
        } else if let Some((slot, generic_ticks)) = series {
            let _ = control_tx.send(ControlCommand::StopAskingForSeries {
                instrument: slot,
                con_id: withdrawn.con_id,
                took_it: withdrawn.took_it,
                generic_ticks,
                issued: withdrawn.decided_at,
            });
        }
        Ok(instrument)
    }

    /// Drop this request's claim on the headlines, and say whether that was
    /// the last one. Called on every path out of a withdrawal: the quotes may
    /// stay up for another caller while the headlines this one asked for stop.
    pub(crate) fn release_news(&self, shared: &SharedState, req_id: i64) -> Option<NewsSubject> {
        let emptied = {
            let mut news = self.news_askers.lock().unwrap();
            let mut done: Option<i64> = None;
            for (con_id, askers) in news.iter_mut() {
                if askers.remove(&req_id) {
                    if askers.is_empty() {
                        done = Some(*con_id);
                    }
                    break;
                }
            }
            let con_id = done?;
            news.remove(&con_id);
            con_id
        };
        // Named by the slot where this side has one, and by the contract
        // where it does not. It does not always: the headlines are asked for
        // before the contract is registered, and the mapping a slot comes from
        // is written when that succeeds — so a registration that fails leaves
        // this holding nothing to name. Resolved to a slot or nothing, the
        // withdrawal was simply never sent, while the record of who asked had
        // already been dropped above: the headlines ran for the rest of the
        // session with nothing able to stop them, and the next request for the
        // contract opened a second subscription beside the first.
        Some(
            self.cached_instrument(shared, emptied)
                .map_or(NewsSubject::Contract(emptied), NewsSubject::Slot),
        )
    }

    /// Take a request number for a book, or say it already holds one.
    ///
    /// Depth is routed by records the engine keeps, so neither surface could
    /// see that a number already held a book: two contracts' rows arrived
    /// interleaved under one number with nothing to tell them apart, the
    /// withdrawal named only the later contract and left the earlier one being
    /// served, and a reconnect brought back one book where there had been two.
    pub fn hold_the_book(&self, req_id: i64) -> Result<(), Refusal> {
        if !self.depth_reqs.lock().unwrap().insert(req_id) {
            return Err(Refusal::stated(
                DUPLICATE_TICKER_ID,
                format!(
                    "request {req_id} is already holding a book: withdraw it before \
                     asking for another under the same number",
                ),
            ));
        }
        Ok(())
    }

    /// Give the number back, or say it was holding no book.
    pub fn release_the_book(&self, req_id: i64) -> Result<(), Refusal> {
        if !self.depth_reqs.lock().unwrap().remove(&req_id) {
            return Err(Refusal::stated(
                NO_SUCH_BOOK,
                format!("no book is held under request {req_id}"),
            ));
        }
        Ok(())
    }

    /// Whether this number is watching a contract at all.
    ///
    /// Read before a withdrawal, which cannot tell "nothing was held" from
    /// "a follower stopped following" by what it returns -- both are nothing
    /// to send.
    pub fn holds_mkt_data(&self, req_id: i64) -> bool {
        self.req_to_instrument.lock().unwrap().contains_key(&req_id)
    }

    /// Withdraw a number that is in the middle of taking a subscription on
    /// another thread, and say whether there was one to withdraw.
    ///
    /// A registration waits on the engine, and the record an ordinary
    /// withdrawal reads is written when that answer comes back. Between the
    /// two there is nothing to find, so a withdrawal arriving in the gap read
    /// as a number watching nothing. Refused for that, the caller was told its
    /// withdrawal had not happened while the registration finished behind it
    /// and the subscription lived — no answer the venue gives, and the one
    /// state the caller cannot act on. Written down instead, and the
    /// registration takes what it opened back down when it reads it. The gap
    /// is as long as the wait, and the surface that releases the interpreter
    /// lock across it makes a cancel from a timer thread an ordinary thing to
    /// write.
    pub fn withdraw_while_registering(&self, req_id: i64) -> bool {
        self.note_withdrawal(TAKING_QUOTES, req_id)
    }

    /// The same for a tick stream.
    ///
    /// The two are claimed under keys of their own, so the quote answer above
    /// says nothing about a number in the middle of taking one of these — and a
    /// withdrawal that asked the wrong one read a stream still being registered
    /// as a stream that was never there.
    pub fn withdraw_while_registering_tbt(&self, req_id: i64) -> bool {
        self.note_withdrawal(TAKING_TICKS, req_id)
    }

    /// Written under the claim's own lock, and only against a claim that is
    /// still held: the registration reads it as it gives the claim back and
    /// under the same acquisition, so a withdrawal recorded here is one the
    /// registration has not yet stopped looking for.
    fn note_withdrawal(&self, taking: u8, req_id: i64) -> bool {
        let held = self.registering.lock().unwrap();
        if !held.contains(&(taking, req_id)) {
            return false;
        }
        self.withdrawn_while_registering.lock().unwrap().insert((taking, req_id));
        true
    }

    /// Which contract's slot a number is watching, if it is watching one.
    pub fn watching(&self, req_id: i64) -> Option<InstrumentId> {
        self.req_to_instrument.lock().unwrap().get(&req_id).copied()
    }

    /// Say this number now holds a subscription of its own, distinct from any
    /// it held before.
    fn stamp_registration(&self, req_id: i64) {
        let n = self.in_order();
        self.registration_epoch.lock().unwrap().insert(req_id, n);
    }

    /// The next number in the order of everything this client asks for.
    ///
    /// One rising count, taken as a decision is made and carried on the
    /// command that acts on it. The engine keeps the number a subscription
    /// began under and reads it again on a withdrawal: a withdrawal decided
    /// before that subscription began is not about it.
    pub(crate) fn in_order(&self) -> u64 {
        // From one, because zero is what a command says when it names no
        // occupancy at all — a withdrawal carried through a move, or a session
        // closing. Counted from zero, the first subscription of every session
        // was the one occupancy that could not be named.
        self.epochs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Which subscription a number is holding, as a figure that changes every
    /// time it takes a new one.
    ///
    /// Read to tell one subscription under a number from the next. Neither the
    /// number nor the contract can do that: a callback is free to withdraw
    /// what it was just told about and ask for the same contract again, and
    /// what runs after the callback must not then act on the number alone.
    pub fn registration_of(&self, req_id: i64) -> Option<u64> {
        self.registration_epoch.lock().unwrap().get(&req_id).copied()
    }

    /// Unregister a market data subscription.
    ///
    /// Answers with the subscription to withdraw, separately with the
    /// instrument whose headlines stop, and separately again with the series
    /// nobody watching that contract asks for any more. They are not the same
    /// question: the quotes stay up for another caller while the headlines
    /// this one asked for end and the series it brought with it go, and a
    /// caller told only that the quotes stay up sent nothing — leaving the
    /// headlines running and the venue serving series nobody reads.
    pub fn unregister_mkt_data(
        &self, shared: &SharedState, req_id: i64,
    ) -> WhatAWithdrawalLeaves {
        // Whatever this id was waiting to finish, it is not waiting any
        // more. Left behind, the same id handed out again for an ordinary
        // stream reads as a snapshot and is withdrawn as soon as it has both
        // sides of a quote.
        self.snapshot_reqs.lock().unwrap().remove(&req_id);
        // Which contract this number was watching, whether it held the
        // subscription, who takes it over and whether it was the venue's
        // one-shot are one question, answered under one acquisition. Answered
        // a map at a time, a stream asking for the contract while the one-shot
        // holding it was halfway through its withdrawal read the one-shot as
        // an ordinary stream: it was recorded as watching, sent nothing of its
        // own, and served off a burst that was already over.
        let (instrument, take_it_down, series_gone, took_it, decided_at) = {
            let mut own = self.ownership();
            // Taken here, under the maps that decide, so that a subscription
            // taken by another caller after this decision carries a later
            // number than the withdrawal that would take it down.
            let decided_at = self.in_order();
            own.one_shot.remove(&req_id);
            // Which subscription it was holding goes with the record of what it
            // was watching. Removed before the maps were taken, a move running
            // in between wrote a fresh one for a request this withdrawal then
            // removed, and the number was left standing for a request watching
            // nothing at all.
            own.epoch.remove(&req_id);
            let Some(instrument) = own.by_req.remove(&req_id) else {
                own.series.remove(&req_id);
                return WhatAWithdrawalLeaves {
                    subscription: None, headlines: None, series: None,
                    con_id: 0, took_it: 0, decided_at,
                };
            };
            // A caller that was watching someone else's subscription stops
            // watching it, and the subscription stays up for the rest. A
            // caller that held it hands it to the next one watching rather
            // than taking the quotes away from them.
            let mut take_it_down = true;
            if let Some(watchers) = own.following.get_mut(&instrument) {
                let was_following = watchers.contains(&req_id);
                watchers.retain(|&id| id != req_id);
                let next = if was_following { None } else { watchers.first().copied() };
                if let Some(next) = next {
                    watchers.retain(|&id| id != next);
                }
                if watchers.is_empty() {
                    own.following.remove(&instrument);
                }
                if was_following || next.is_some() {
                    if let Some(next) = next {
                        own.holders.insert(instrument, next);
                    }
                    take_it_down = false;
                }
            }
            // Which occupancy of the slot this withdrawal is about, read here
            // rather than left to the engine to work out from the slot number.
            let took_it = own.taken_on.get(&instrument).copied().unwrap_or(0);
            if take_it_down {
                own.holders.remove(&instrument);
                own.taken_on.remove(&instrument);
            }
            // What this caller brought with it goes with it. Only the series
            // nobody else watching the contract named: the subscription stays
            // up for them, and the series they asked for are theirs. Left
            // behind, the venue served them for as long as that subscription
            // outlived this caller, and every rebuild after a reconnect asked
            // for them again.
            let mine = own.series.remove(&req_id).unwrap_or_default();
            let watching: Vec<i64> = own.holders.get(&instrument).copied()
                .into_iter()
                .chain(own.following.get(&instrument).cloned().unwrap_or_default())
                .collect();
            // Said even where the whole subscription goes: the engine may find
            // that subscription already replaced by one this caller knows
            // nothing about, and then the series this caller asked for are the
            // only part of the withdrawal that still stands.
            let series_gone: Vec<u32> = mine.into_iter()
                .filter(|tick| {
                    !watching.iter()
                        .any(|other| own.series.get(other).is_some_and(|s| s.contains(tick)))
                })
                .collect();
            (instrument, take_it_down, series_gone, took_it, decided_at)
        };
        self.mdt_sent.lock().unwrap().remove(&req_id);
        let series = (!series_gone.is_empty()).then_some((instrument, series_gone));
        // Read before the cache is forgotten below, because that is where it
        // comes from.
        let con_id = self.cached_con_id_of(instrument);
        if !take_it_down {
            return WhatAWithdrawalLeaves {
                subscription: None,
                headlines: self.release_news(shared, req_id),
                series,
                con_id,
                took_it,
                decided_at,
            };
        }
        self.last_quotes.lock().unwrap().remove(&instrument);
        self.mdt_by_instrument.lock().unwrap().remove(&instrument);
        let stop_news = self.release_news(shared, req_id);
        self.forget_instrument(instrument);
        // The subscription goes whole, and its series with it: there is
        // nothing left for them to be entries of.
        WhatAWithdrawalLeaves {
            subscription: Some(instrument),
            headlines: stop_news,
            series,
            con_id,
            took_it,
            decided_at,
        }
    }

    /// Which contract this client believes holds a slot, as far as its own
    /// cache says. Zero where the venue has not named one.
    fn cached_con_id_of(&self, instrument: InstrumentId) -> i64 {
        self.con_id_to_instrument
            .lock()
            .unwrap()
            .iter()
            .find(|(_, held)| **held == instrument)
            .map(|(con_id, _)| *con_id)
            .unwrap_or(0)
    }

    /// Drop the client-side conId cache entries for an instrument id. The
    /// engine may reclaim and reuse the slot after an unsubscribe;
    /// a stale cache entry would silently point the old conId at whatever
    /// contract inherits the id. A later request for that conId simply
    /// re-registers.
    pub fn forget_instrument(&self, instrument: InstrumentId) {
        self.con_id_to_instrument.lock().unwrap().retain(|_, iid| *iid != instrument);
    }

    /// Name the providers to ask for news from, overriding what the logon
    /// said this account may read. An empty string returns to that.
    pub fn set_news_providers(&self, providers: &str) {
        *self.news_providers.lock().unwrap() = providers.to_string();
    }

    // ── Contract cache ──

    /// How a contract was asked about, as one string.
    ///
    /// Built from what the caller wrote, not from what the venue answered: a
    /// caller who says `SMART` gets `SMART` back on the next order, and looking
    /// the answer up under the exchange the venue routed it to would never
    /// match.
    pub fn description_key(c: &ApiContract) -> String {
        Self::description_key_of(
            &c.symbol, &c.sec_type, &c.exchange,
            &crate::types::model::contract_identity(
                &c.last_trade_date_or_contract_month, c.strike, &c.right,
                &c.multiplier, &c.currency,
            ),
            &c.primary_exchange, &c.local_symbol, &c.trading_class,
            &c.sec_id_type, &c.sec_id, &c.currency,
        )
    }

    /// The same key from the parts, for surfaces that carry their own contract
    /// type rather than this one.
    ///
    /// Everything the lookup narrows on is in here. Two descriptions that
    /// differ only in a field the key leaves out are one key, and the second
    /// order goes out under the first one's contract.
    pub fn description_key_of(
        symbol: &str, sec_type: &str, exchange: &str, identity: &str,
        primary_exchange: &str, local_symbol: &str, trading_class: &str,
        sec_id_type: &str, sec_id: &str, currency: &str,
    ) -> String {
        // The currency verbatim, beside the identity that has already folded
        // it. A contract's identity treats saying nothing and saying USD as
        // the same thing, which is right for the slot an order is placed
        // through and wrong here: the lookup sends the currency as a filter,
        // so a description that stated none can be answered with a listing in
        // another one — and under a shared key, the next order that does say
        // USD would be placed on it.
        format!(
            "{symbol}|{sec_type}|{exchange}|{identity}|{primary_exchange}|\
             {local_symbol}|{trading_class}|{sec_id_type}|{sec_id}|{currency}"
        )
    }

    /// The contract the venue named for a description, if it has named one.
    pub fn named_for(&self, key: &str) -> Option<ApiContract> {
        self.named_by_description.lock().unwrap().get(key).cloned()
    }

    /// Remember what the venue named a description, for the next order on it.
    pub fn remember_named(&self, key: String, contract: ApiContract) {
        self.named_by_description.lock().unwrap().insert(key, contract);
    }

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
    ///
    /// Answers with no slot where the stream was withdrawn while this was
    /// waiting on the engine: it has been taken back down and nothing is
    /// recorded under the number, so the caller has no stream to name a kind
    /// for either.
    pub fn register_tbt(
        &self,
        _shared: &SharedState,
        control_tx: &SyncSender<ControlCommand>,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        sec_type: &str,
        exchange: &str,
        tbt_type: TbtType,
        number_of_ticks: u32,
        ignore_size: bool,
    ) -> Result<Option<InstrumentId>, Refusal> {
        // A number already carrying a tick stream cannot carry a second.
        //
        // The same shape the quote subscription above refuses: two streams
        // under one number stamped that number on every record, so a caller
        // was handed one contract's trades and another's quotes with nothing
        // to tell them apart and the tick kind of whichever asked last. The
        // withdrawal reads the one record this keeps, so it reached the second
        // stream only -- and with that record gone, a second withdrawal
        // reached nothing at all and the first stream ran under a cancelled
        // number for the life of the session.
        //
        // Refused here rather than where the subscribe is sent: the slot is
        // registered whether or not the subscribe is refused, so a refusal
        // downstream would still leave this record overwritten and the first
        // stream orphaned. Before the send is the only point that leaves
        // nothing behind.
        //
        // Read and claimed under one lock, as the quote path above does it and
        // for the same reason: the record below cannot be written until the
        // engine names the slot, so two callers on this number both read it as
        // free and both went on.
        let claim = {
            let carrying = self.tbt_to_instrument.lock().unwrap();
            let mut taking = self.registering.lock().unwrap();
            if carrying.contains_key(&req_id) || !taking.insert((TAKING_TICKS, req_id)) {
                return Err(Refusal::stated(
                    DUPLICATE_TICKER_ID,
                    format!(
                        "request {req_id} is already carrying a tick stream: withdraw it \
                         before asking for another under the same number",
                    ),
                ));
            }
            Registering {
                held: &self.registering,
                withdrawn: &self.withdrawn_while_registering,
                key: (TAKING_TICKS, req_id),
            }
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        control_tx.send(ControlCommand::SubscribeTbt {
            contract: ContractRef { con_id, symbol: symbol.to_string(), sec_type: sec_type.to_string(), exchange: exchange.to_string(), ..Default::default() },
            req_id,
            tbt_type,
            number_of_ticks,
            ignore_size,
            reply_tx: Some(reply_tx),
        }).map_err(|e| Refusal::not_connected(format!("Engine stopped: {e}")))?;

        let instrument_id = self.recv_registration(reply_rx)?;
        self.cache_instrument(con_id, instrument_id);
        // A withdrawal that arrived while this was away found no record and
        // was answered anyway, as the venue answers it. Read before the record
        // is written and under the acquisition that gives the claim back, so
        // the stream is taken down again rather than published under a number
        // its caller has already stopped watching. Nothing is reported: the
        // withdrawal was answered when it arrived, and a send failing now
        // fails because the engine has gone, which takes the stream with it.
        if claim.withdrawn_meanwhile() {
            let _ = control_tx.send(ControlCommand::UnsubscribeTbt {
                req_id,
                instrument: instrument_id,
            });
            return Ok(None);
        }
        self.tbt_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        Ok(Some(instrument_id))
    }

    /// Look up req_id for an instrument.
    pub fn req_id_for_instrument(&self, instrument: InstrumentId) -> i64 {
        self.instrument_to_req.lock().unwrap()
            .get(&instrument).copied().unwrap_or(-1)
    }

    // ── Display groups ──

    /// The groups this client offers: seven, numbered from one. Nothing about
    /// a group crosses the wire, so the number is this client's own.
    const DISPLAY_GROUPS: i32 = 7;

    /// What a group holds when nothing has been put in it.
    const NO_CONTRACT: &'static str = "none";

    /// Ask which display groups exist.
    pub fn query_display_groups(&self, req_id: i64) {
        let groups = (1..=Self::DISPLAY_GROUPS)
            .map(|g| g.to_string())
            .collect::<Vec<_>>()
            .join("|");
        self.pending_group_events.lock().unwrap().push(GroupEvent::List(req_id, groups));
    }

    /// Follow a group. The caller is told what the group holds now, not only
    /// what it changes to, or a caller that subscribes to a settled group
    /// hears nothing at all.
    pub fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) {
        self.group_subscriptions.lock().unwrap().insert(req_id, group_id);
        let held = self.display_groups.lock().unwrap()
            .get(&group_id)
            .cloned()
            .unwrap_or_else(|| Self::NO_CONTRACT.to_string());
        self.pending_group_events.lock().unwrap().push(GroupEvent::Updated(req_id, held));
    }

    /// Stop the from group events.
    pub fn unsubscribe_from_group_events(&self, req_id: i64) {
        self.group_subscriptions.lock().unwrap().remove(&req_id);
    }

    /// Put a contract in the group the request follows, and tell everyone else
    /// following it. The caller that made the change is told too, which is what
    /// keeps two callers holding the same group in step.
    pub fn update_display_group(&self, req_id: i64, contract_info: &str) -> Result<(), String> {
        let subs = self.group_subscriptions.lock().unwrap();
        let Some(&group_id) = subs.get(&req_id) else {
            return Err(format!(
                "request {req_id} follows no display group, so there is none to put a \
                 contract in: subscribe to a group first"
            ));
        };
        let followers: Vec<i64> = subs.iter()
            .filter(|(_, g)| **g == group_id)
            .map(|(r, _)| *r)
            .collect();
        drop(subs);
        let value = if contract_info.is_empty() { Self::NO_CONTRACT } else { contract_info };
        self.display_groups.lock().unwrap().insert(group_id, value.to_string());
        let mut pending = self.pending_group_events.lock().unwrap();
        for r in followers {
            pending.push(GroupEvent::Updated(r, value.to_string()));
        }
        Ok(())
    }

    /// Take every group events waiting, leaving none.
    pub fn drain_group_events(&self) -> Vec<GroupEvent> {
        self.pending_group_events.lock().unwrap().drain(..).collect()
    }

    // ── PnL subscription management ──

    /// Ask for the pnl.
    ///
    /// Refused while another request holds the subscription. One slot serves
    /// it, and handing that slot to a second asker stopped the first one's
    /// updates without a word to either caller. Asking again under the id
    /// that already holds it is the same subscription, not a second one.
    pub fn subscribe_pnl(&self, req_id: i64) -> Result<(), Refusal> {
        let mut slot = self.pnl_req_id.lock().unwrap();
        if let Some(held) = *slot && held != req_id {
            return Err(Refusal::validation(format!(
                "the account P&L is already subscribed under request {held}; \
                 cancel that one before subscribing under another",
            )));
        }
        *slot = Some(req_id);
        // Nothing has been reported to this subscription yet. Without a value
        // that no account can hold, an account whose P&L is genuinely zero
        // matched the initial state and the caller was told nothing at all.
        *self.last_pnl.lock().unwrap() = [i64::MIN; 3];
        Ok(())
    }

    /// Stop the pnl.
    pub fn unsubscribe_pnl(&self, req_id: i64) {
        let mut pnl = self.pnl_req_id.lock().unwrap();
        if *pnl == Some(req_id) {
            *pnl = None;
        }
    }

    /// Ask for the pnl single.
    pub fn subscribe_pnl_single(&self, req_id: i64, con_id: i64) {
        self.pnl_single_reqs.lock().unwrap().insert(req_id, con_id);
        // What was last reported under this number belonged to whatever it
        // watched before. Kept, a number pointed at another contract inherited
        // the last one's day — or, where the two happened to agree, reported
        // nothing at all until something moved, because a figure is only sent
        // when it differs from what was sent before. The withdrawal beside
        // this one clears it for the same reason; taking the number without
        // withdrawing it did not.
        self.last_pnl_single.lock().unwrap().remove(&req_id);
    }

    /// Stop the pnl single.
    pub fn unsubscribe_pnl_single(&self, req_id: i64) {
        self.pnl_single_reqs.lock().unwrap().remove(&req_id);
        self.last_pnl_single.lock().unwrap().remove(&req_id);
    }

    // ── Account summary subscription management ──

    /// Ask for the account summary.
    ///
    /// Both subscriptions stay until cancelled, including after their first
    /// answers; a third request would exceed the venue's limit.
    pub fn subscribe_account_summary(&self, req_id: i64, tags: &str) -> Result<(), Refusal> {
        let tag_list: Vec<String> = tags.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut slot = self.account_summary_req.lock().unwrap();
        let mut other = self.account_summary_other_req.lock().unwrap();
        let target = if slot.as_ref().is_some_and(|(id, _)| *id == req_id) {
            &mut *slot
        } else if other.as_ref().is_some_and(|(id, _)| *id == req_id) {
            &mut *other
        } else if slot.is_none() {
            &mut *slot
        } else if other.is_none() {
            &mut *other
        } else {
            return Err(Refusal::validation(
                "two account summaries are already subscribed; cancel one before subscribing under another",
            ));
        };
        *target = Some((req_id, tag_list));
        self.last_account_summary.lock().unwrap().remove(&req_id);
        Ok(())
    }

    /// Stop the account summary.
    pub fn unsubscribe_account_summary(&self, req_id: i64) {
        let mut req = self.account_summary_req.lock().unwrap();
        let mut other = self.account_summary_other_req.lock().unwrap();
        if req.as_ref().map(|(r, _)| *r) == Some(req_id) {
            *req = None;
        }
        if other.as_ref().map(|(r, _)| *r) == Some(req_id) {
            *other = None;
        }
        self.last_account_summary.lock().unwrap().remove(&req_id);
    }

    // ── Account updates subscription management ──

    /// Ask for the account updates, or stop asking.
    ///
    /// What was last stated is forgotten either way: asked again, the account
    /// is restated in full and its end said again, as the reference client
    /// answers a second request. Forgotten on the withdrawal alone, a second
    /// ask found every figure already stated and answered with nothing, and
    /// the end a caller was waiting on never came.
    pub fn subscribe_account_updates(&self, subscribe: bool) {
        self.account_updates_subscribed.store(subscribe, Ordering::Release);
        self.last_stated_account.lock().unwrap().clear();
        self.account_end_sent.store(false, Ordering::Release);
        *self.last_portfolio.lock().unwrap() = None;
    }

    // ── Market data type tracking ──

    /// Store the requested market data type.
    ///
    /// The caller names the type once; the wire names it per subscription, on
    /// field 9887. Subscriptions made after this carry the mode it implies, so
    /// a client that asks for delayed data receives delayed data.
    pub fn set_market_data_type(&self, mdt: i32) {
        if !matches!(mdt, MDT_REALTIME | MDT_FROZEN | MDT_DELAYED | MDT_DELAYED_FROZEN) {
            // Kept out rather than kept: subscriptions stay realtime whatever
            // this names, and the callback that reports a subscription's type
            // reads what is stored — so a number nobody recognises, stored,
            // reaches the caller as the venue's word for data that is not on
            // it.
            log::warn!("req_market_data_type({mdt}) names no known type; subscriptions stay realtime");
            return;
        }
        self.market_data_type.store(mdt, Ordering::Relaxed);
    }

    /// The per-subscription mode the requested type implies. Zero is realtime,
    /// which is the shape a subscription has when nothing asked otherwise.
    pub fn subscription_mode(&self) -> i32 {
        match self.market_data_type.load(Ordering::Relaxed) {
            MDT_DELAYED => 1,
            MDT_FROZEN => 2,
            MDT_DELAYED_FROZEN => 3,
            _ => 0,
        }
    }

    /// Check if the `market_data_type` callback should fire for this req_id.
    /// Returns `Some(type)` on the first call per req_id that has data, `None`
    /// thereafter. Every watcher is told the type sent with the subscription
    /// it follows, because joining it sends no request to change the feed.
    pub fn check_mdt_needed(&self, req_id: i64, has_data: bool) -> Option<i32> {
        if has_data && self.mdt_sent.lock().unwrap().insert(req_id) {
            // The type this subscription was made under, which is the type
            // transmitted with it and therefore the type of the data.
            Some(
                self.watching(req_id)
                    .and_then(|instrument| {
                        self.mdt_by_instrument.lock().unwrap().get(&instrument).copied()
                    })
                    .unwrap_or_else(|| self.market_data_type.load(Ordering::Relaxed)),
            )
        } else {
            None
        }
    }

    // ── Bulletin subscription management ──

    /// Ask for the bulletins.
    pub fn subscribe_bulletins(&self) {
        self.bulletin_subscribed.store(true, Ordering::Release);
    }

    /// Stop the bulletins.
    pub fn unsubscribe_bulletins(&self) {
        self.bulletin_subscribed.store(false, Ordering::Release);
    }

    /// Whether broadcast notices are being received.
    pub fn bulletins_subscribed(&self) -> bool {
        self.bulletin_subscribed.load(Ordering::Acquire)
    }

    // ── Execution replay store ──

    /// Store an execution for later replay via `req_executions`.
    ///
    /// Once per execution. The venue restates the day's executions at every
    /// logon, so the same one can arrive again; it is known again by the id
    /// it carries. One carrying no id is known by its content, as the engine
    /// knows it: an absent id is the shape a replay takes, and kept on every
    /// replay a caller summing the day's volume doubled it on every rebuilt
    /// connection. The cumulative quantity is what tells two otherwise
    /// identical prints of one order apart.
    pub fn push_execution(&self, contract: ApiContract, execution: ApiExecution, commission_and_fees: ApiCommissionAndFeesReport) {
        let mut store = self.executions.lock().unwrap();
        if execution.exec_id.is_empty() {
            let same = |stored: &StoredExecution| {
                let e = &stored.execution;
                e.exec_id.is_empty()
                    && e.order_id == execution.order_id
                    && e.time == execution.time
                    && e.shares == execution.shares
                    && e.price == execution.price
                    && e.cum_qty == execution.cum_qty
            };
            if store.rows.iter().any(same) {
                return;
            }
        } else if store.by_id.contains_key(&execution.exec_id) {
            return;
        } else {
            let at = store.rows.len();
            store.by_id.insert(execution.exec_id.clone(), at);
        }
        store.rows.push(StoredExecution { contract, execution, commission_and_fees });
    }

    /// File the executions the venue restated, announcing none of them.
    ///
    /// A restated execution books nothing — the quantity is already held, or
    /// the order was never this session's — so it never becomes a fill, and
    /// the fill path was the only way into the record `req_executions` answers
    /// from. After a restart that record was empty, and a caller asking was
    /// told, silently, that nothing had filled. Called from the dispatch pass
    /// on either surface: a caller that asks is answered, and one that did
    /// not hears nothing.
    pub fn record_restated_executions(&self, shared: &SharedState) {
        for (contract, execution) in shared.orders.drain_restated_executions() {
            // Unsolicited, as a live fill is stored, and costed the same way:
            // what it cost arrives on a record of its own, if it arrives.
            self.push_execution(contract, execution, ApiCommissionAndFeesReport::default());
        }
    }

    /// Note what an execution cost against the execution itself, so a replay
    /// of it carries the charge the venue stated rather than the nothing it
    /// was stored with.
    pub fn record_charge(&self, charge: &ApiCommissionAndFeesReport) {
        // A charge naming no execution stamps none. Matched on the empty name,
        // it was written onto every execution stored without one.
        if charge.exec_id.is_empty() {
            return;
        }
        let mut store = self.executions.lock().unwrap();
        if let Some(at) = store.by_id.get(&charge.exec_id).copied() {
            store.rows[at].commission_and_fees = charge.clone();
        }
    }

    /// Executions matching `filter`, cloned out under one short lock.
    ///
    /// Callers replay these into user callbacks, and a callback may re-enter
    /// any path that locks `executions` — re-requesting from `exec_details` is
    /// an ordinary ibapi pattern, and the dispatch thread pushes fills through
    /// the same mutex. Handing back indices to be dereferenced later also
    /// raced `reset()`, which clears the vector. Snapshotting closes both.
    pub fn snapshot_executions(&self, filter: &ExecutionFilter) -> Vec<StoredExecution> {
        let store = self.executions.lock().unwrap();
        store.rows.iter().filter(|se| execution_matches(se, filter)).cloned().collect()
    }

    // ── Open order tracking ──

    /// The parent this client recorded when it placed the order, if any.
    ///
    /// The engine reads no parent from an execution report, so for an order
    /// this client placed its own record is the only source. An order placed
    /// elsewhere keeps whatever the engine reports.
    pub(crate) fn tracked_parent_id(&self, order_id: u64) -> Option<i64> {
        self.open_orders.lock().unwrap()
            .get(&order_id)
            .map(|t| t.order.parent_id)
            .filter(|p| *p > 0)
    }

    /// Check if an order with this ID is currently tracked (for modify detection).
    pub fn is_order_tracked(&self, order_id: u64) -> bool {
        self.open_orders.lock().unwrap().contains_key(&order_id)
    }

    /// Whether this client placed the order. An order it learnt of from the
    /// venue is tracked here too, once a status has arrived or a replace has
    /// restated it, and is not this.
    pub fn placed_here(&self, order_id: u64) -> bool {
        self.open_orders.lock().unwrap().get(&order_id).is_some_and(|t| t.placed_here)
    }

    /// Whether this id names an order the venue is working.
    ///
    /// A held order is tracked here and unknown to the venue, so the two are
    /// not the same question. Asked as one, placing again under a held id
    /// built a replace of an order nothing had ever submitted: the venue
    /// refuses it, and the submit that was held stays queued to go out behind
    /// the next thing that transmits — under the terms the caller had just
    /// replaced.
    ///
    /// What decides it is a held submission, not a hold of any kind: a live
    /// order can have a revision of its own waiting to be transmitted, and that
    /// order is still one the venue is working.
    ///
    /// The venue's own book is asked beside this client's. An order the venue
    /// replayed at connect was not placed here, so it is in no local book, and
    /// asked of the local book alone a replace of one read as a first
    /// placement: it went out as a new order under a number the venue is
    /// already working, and the record of the order it named was overwritten
    /// on the way.
    pub fn is_working_at_the_venue(&self, order_id: u64, venue: Option<&SharedState>) -> bool {
        (self.is_order_tracked(order_id)
            || venue.is_some_and(|v| v.orders.venue_is_working(order_id)))
            && !self.holds_a_submission(order_id)
    }

    /// Whether a withdrawal under this number names anything.
    ///
    /// Read after the connect-time replay of the account's working set, which
    /// is what makes it safe: an order carried over from a previous session is
    /// not known here until that replay lands, and refusing a withdrawal of a
    /// live order is worse than the silence this replaces. A held placement
    /// has already been taken by the time this is asked.
    pub fn a_withdrawal_names_something(&self, order_id: u64, venue: &SharedState) -> bool {
        self.is_working_at_the_venue(order_id, Some(venue))
    }

    /// Whether this id names an order built and kept rather than sent.
    pub fn is_held(&self, order_id: u64) -> bool {
        self.held_orders.lock().unwrap().iter().any(|h| h.order_id == order_id)
    }

    /// Whether what is held under this id would place the order, rather than
    /// revise one the venue is already working.
    pub fn holds_a_submission(&self, order_id: u64) -> bool {
        self.held_orders.lock().unwrap().iter()
            .any(|h| h.order_id == order_id && h.places_the_order())
    }

    /// Whether a replace names the contract the venue says the order is on.
    ///
    /// A replace names the order and not the contract, so one naming another
    /// contract is refused rather than recorded against it. A combination is
    /// not one contract: what identifies it is the set of legs it is built
    /// from, and after a description is named the id on it is one leg's
    /// underlying — two different combinations on one underlying carry the
    /// same id and register the same slot, so comparing ids could never tell
    /// them apart.
    ///
    /// Where one side states neither an id nor legs there is nothing to
    /// compare, and the replace stands. **No record the venue writes states
    /// legs today** — neither the connect replay nor an execution report does
    /// — so in practice a combination reaches that escape and is not compared
    /// at all. The comparison below is what to do when one does; changing what
    /// the venue's record carries is a question about the wire, not about
    /// this.
    pub(crate) fn names_the_same_contract(known: &ApiContract, stated: &ApiContract) -> bool {
        if !known.combo_legs.is_empty() || !stated.combo_legs.is_empty() {
            if known.combo_legs.is_empty() || stated.combo_legs.is_empty() {
                return true;
            }
            // The legs, whatever order they are stated in: a combination is
            // the same combination however it is written down.
            let legs = |c: &ApiContract| {
                let mut v: Vec<(i64, i32, String)> = c.combo_legs.iter()
                    .map(|l| (l.con_id, l.ratio, l.action.to_ascii_uppercase()))
                    .collect();
                v.sort();
                v
            };
            return legs(known) == legs(stated);
        }
        known.con_id == 0 || stated.con_id == 0 || known.con_id == stated.con_id
    }

    /// The slot a tracked order was placed on, if it is tracked.
    ///
    /// A replace names the order and not the contract, so this is the
    /// instrument a replacement stays on whatever contract it carries.
    pub fn tracked_instrument(&self, order_id: u64) -> Option<InstrumentId> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.instrument)
    }

    /// The order a tracked id was submitted with, if it is tracked.
    pub fn tracked_order(&self, order_id: u64) -> Option<ApiOrder> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.order.clone())
    }

    /// Why a replace cannot restate this order, or `None` if it can.
    ///
    /// The replace states the order type and the prices that type carries, plus
    /// the companions the type needs, restated from the record of the order as
    /// it was placed. What it does not state is the peg offset, the execution
    /// instruction or the algo block. For an order defined by any of those, the
    /// replace describes something other than the order being replaced, and the
    /// venue rejects it — leaving the caller with nothing resting.
    ///
    /// `restating_itself` says the replace keeps the order's type. Only then
    /// can a type be restated from the record of the order that was placed: a
    /// conversion into a trailing stop has no such record, so its trail goes
    /// unstated and the venue refuses the replace naming tag 211.
    ///
    /// The order type alone does not decide this: an adjustable stop is an
    /// ordinary `STP` defined by its conversion, which a replace stating only
    /// the type would leave behind. An algo order and a conditional one were
    /// refused on the same reading until a session placed each and the venue
    /// took the replace — the strategy asked was the adaptive one, and the
    /// block that carries it is restated by the same path for every strategy.
    ///
    /// The adjustable stop was refused here for the same reason and is not any
    /// more. The replace states the whole adjustable block, the same six
    /// numbers the placement states and in the same order: the marker, the type
    /// it adjusts to, the trigger, the adjusted stop, its limit and the
    /// trailing amount. This client's replace is built by the path that writes
    /// them, so a caller stating a new price on such an order has it restated
    /// rather than dropped — and a rule here refusing what the venue was never
    /// asked is the thing this client does not do.
    pub fn replace_cannot_restate(order: &ApiOrder, restating_itself: bool) -> Option<String> {
        // A what-if is a margin preview, not a resting order, so there is
        // nothing on the book for a replace to act on.
        if order.what_if {
            return Some("a what-if order".to_string());
        }
        // The replace is rebuilt from the tracked record, which holds side,
        // price, quantity, order type, time-in-force and trigger — and nothing
        // else. Every attribute below rides a tag the replace does not carry,
        // so a modify would state the order without it.
        //
        // The venue refuses the order itself on the venue and security type a
        // session asked on — "Partial AON orders not supported for this
        // combination of exchange and security type" — so what a replace would
        // do to one is a question it has not been possible to put.
        if order.min_qty > 0 {
            return Some("an order with a minimum quantity".to_string());
        }
        let ty = order.order_type.to_uppercase();
        // `LIT` is submitted as `LT` but tracked under a byte the replace
        // renders as `K`, which is market-to-limit in this dialect — so a
        // replace would describe a different order type entirely.
        //
        // `MTL`, `BOX TOP` and `MKT PRT` are here because the replace renders
        // the same byte they were submitted under, so it restates them as
        // themselves — which is the whole test for membership.
        //
        // The types below it restate themselves on a session's answer rather
        // than on a reading. Each was placed with the market open, replaced,
        // and found still working afterwards.
        //
        // `MIDPRICE` is the reference client's name for `MIDPX`, and the name
        // the venue's own statement of such an order carries. A snap to the
        // market and a snap to the primary were each placed, replaced twice
        // and withdrawn on a paper session, with the market closed.
        //
        // `REL` is the one that was not. Its replace drew no answer at all, and
        // neither did the withdrawal that followed — the order stopped
        // answering for anything, which is the outcome the refusals here were
        // written to prevent. It keeps its reason, and now has the session
        // behind it.
        if matches!(
            ty.as_str(),
            "MKT" | "LMT" | "STP" | "STP LMT" | "MOC" | "LOC" | "MIT" | "STP PRT"
                | "MTL" | "BOX TOP" | "MKT PRT"
        ) || (restating_itself
            && matches!(
                ty.as_str(),
                "TRAIL" | "TRAIL LIMIT" | "PEG MID" | "PEG MIDPT" | "MIDPX" | "MIDPRICE"
                    | "SNAP MID" | "SNAP MIDPT" | "SNAP MKT" | "SNAP PRI" | "SNAP PRIM" | "LIT"
            ))
        {
            return None;
        }
        Some(format!("a {ty} order"))
    }

    /// The defining number a replace cannot carry, where the replacement names
    /// a different one.
    ///
    /// A shape that holds its defining price in the form the submit sent — a
    /// trail, a peg or snap offset, a cap, a limit offset — has it restated
    /// from the record, with whatever the replace names written into it: each
    /// was measured on a paper session, placed, replaced and read back. The
    /// trailing percent is the one number left with nowhere to go: the
    /// replace carries a price and a trigger, and a percent is neither.
    ///
    /// A replacement that names none of them still goes: a zero and an unset
    /// value both leave the placed number in force, which is how a caller
    /// moves the quantity alone.
    fn replace_cannot_state(tracked: &ApiOrder, incoming: &ApiOrder) -> Option<&'static str> {
        let named = |v: f64| v != 0.0 && v != f64::MAX;
        let moved = |placed: f64, asked: f64| named(asked) && asked != placed;
        let by_percent = named(tracked.trailing_percent);
        Some(match tracked.order_type.to_uppercase().as_str() {
            "TRAIL" if moved(tracked.trailing_percent, incoming.trailing_percent) => {
                "the trailing percent"
            }
            "TRAIL" if by_percent && moved(tracked.aux_price, incoming.aux_price) => {
                "the trail amount"
            }
            _ => return None,
        })
    }

    /// The price a replace names, read from the field the shape's own submit
    /// reads it from: a trailing stop limit's limit offset, and every other
    /// type's limit price. Unset names nothing.
    ///
    /// One place, so the two bindings cannot diverge on it.
    pub fn replace_price(order: &ApiOrder) -> i64 {
        let named = if order.order_type.eq_ignore_ascii_case("TRAIL LIMIT")
            && order.lmt_price_offset != f64::MAX
        {
            order.lmt_price_offset
        } else {
            order.lmt_price
        };
        Self::price_or_unset(named)
    }

    /// The trigger a replace names: the auxiliary price, as the submit reads
    /// it. Unset names nothing.
    pub fn replace_trigger(order: &ApiOrder) -> i64 {
        Self::price_or_unset(order.aux_price)
    }

    /// Why a modify of `order_id` cannot be sent, if it cannot.
    ///
    /// Both sides are checked. The resting order is the one the replace has to
    /// restate, and the incoming order is what the caller is asking it to
    /// become — a modify that *adds* a bracket link or an OCA group states an
    /// order that has neither, so examining only the record on the book lets
    /// the attribute through on the very message that was supposed to carry it.
    ///
    /// One place, so the two bindings cannot diverge on either the rule or the
    /// wording.
    ///
    /// The resting order is this client's own record where it placed the
    /// order, and otherwise the venue's statement of it: an order the venue
    /// named at connect is in no book here, and compared against nothing its
    /// replace read as a change of type and was refused before the engine
    /// saw it.
    pub fn modify_refusal(&self, order_id: u64, incoming: &ApiOrder, venue: Option<&SharedState>) -> Option<Refusal> {
        let tracked = self.tracked_order(order_id)
            .or_else(|| venue.and_then(|v| v.orders.get_order_info(order_id)).map(|info| info.order));
        // Whether the replace leaves the order the type it already is.
        let restating_itself = tracked
            .as_ref()
            .is_some_and(|t| t.order_type.eq_ignore_ascii_case(&incoming.order_type));
        // A type the replace can restate can still be asked for a number the
        // replace does not carry. Answered as sent, the venue went on working
        // the number the order was placed with while the record here held the
        // one the caller asked for, and every later action restated from it.
        if restating_itself
            && let Some(tracked) = tracked.as_ref()
            && let Some(field) = Self::replace_cannot_state(tracked, incoming)
        {
            // A field the replace has nowhere to put, on a type it can
            // otherwise restate. The catalogue has no number for that, so it
            // keeps the general one.
            return Some(Refusal::validation(format!(
                "{field} of a {} order cannot be modified: the replace does not carry it, \
                 and the venue would go on working the order as it was placed",
                tracked.order_type,
            )));
        }
        // A change of type on an order with a parent or a group. The replace
        // states the links only where it restates the type the order was
        // placed under; a change of type goes out without them, and the venue
        // reads their absence as their removal — measured, a leg replaced with
        // no group and no parent left its bracket. Refused rather than sent.
        if !restating_itself
            && let Some(t) = tracked.as_ref()
            && (t.parent_id != 0 || !t.oca_group.is_empty())
        {
            return Some(Refusal::stated(CHANGE_CANNOT_CHANGE_TYPE, format!(
                "a {} order with a parent or a group cannot change type: the replace carries \
                 neither across a change of type, and the venue would work the order detached",
                t.order_type,
            )));
        }
        // A parent or a group other than the one an order placed here was
        // placed with. The replace carries neither — the engine restates them
        // from the placement — so the order would go on as it was placed
        // while the caller believed it linked otherwise. An order the venue
        // named is restated from the caller's statement, links included, so
        // there the caller's word is what the venue receives.
        if let Some(t) = tracked.as_ref()
            && self.placed_here(order_id)
            && ((incoming.parent_id != 0 && incoming.parent_id != t.parent_id)
                || (!incoming.oca_group.is_empty() && incoming.oca_group != t.oca_group))
        {
            return Some(Refusal::validation(
                "the parent or the group of an order placed here cannot be modified: the replace \
                 does not carry them, and the venue would go on working the order as it was placed"
                    .to_string(),
            ));
        }
        let why = tracked
            .and_then(|tracked| Self::replace_cannot_restate(&tracked, restating_itself))
            .or_else(|| Self::replace_cannot_restate(incoming, restating_itself))?;
        // A change that would move the order to a type the replace cannot
        // restate, which the catalogue names.
        Some(Refusal::stated(CHANGE_CANNOT_CHANGE_TYPE, format!(
            "{why} cannot be modified: the replace does not carry the fields that \
             define it, and sending one would cancel the order"
        )))
    }

    /// The venue has taken the replacement outstanding on this order, so the
    /// terms kept against a refusal are spent.
    ///
    /// Read off a status before this — the venue working the order again —
    /// which is not the same fact: a fill landing between the attempt and the
    /// answer took the more advanced status and the acknowledgement behind it
    /// was never announced, so the copy outlived the replacement the venue had
    /// taken and the next refusal put back terms from before it.
    pub fn settle_replacement(&self, order_id: u64) {
        if let Some(tracked) = self.open_orders.lock().unwrap().get_mut(&order_id) {
            tracked.before_the_replace = None;
        }
    }

    /// Put back the terms a restatement replaced, where the attempt did not
    /// stand: the venue refused it, or it never left this process.
    pub fn undo_restatement(&self, order_id: u64) {
        if let Some(tracked) = self.open_orders.lock().unwrap().get_mut(&order_id) {
            put_back_the_terms(tracked);
        }
    }

    /// Restate the terms of an order the venue is already working.
    ///
    /// A replace does not place an order, so what the order has done stands:
    /// recorded as a new one, a partly filled order came back from
    /// `req_open_orders` as pending with nothing filled and its whole quantity
    /// outstanding, and a replace the venue then refused left it reading that
    /// way for the rest of the session. The terms follow the attempt, because
    /// every later action restates from here; the execution history does not.
    ///
    /// Falls back to recording it afresh where nothing is held under the id,
    /// which is a caller replacing an order this client did not place.
    pub fn restate_order(
        &self, venue: Option<&SharedState>, order_id: u64, contract: ApiContract,
        mut order: ApiOrder, instrument: InstrumentId,
    ) {
        let mut orders = self.open_orders.lock().unwrap();
        match orders.get_mut(&order_id) {
            Some(tracked) => {
                // Kept against a refusal. Only the first of a run of
                // replacements records it: the second states terms the venue
                // has not answered for either — a change held back never
                // reaches it at all — and falling back to those would put back
                // an attempt rather than what the venue holds.
                if tracked.before_the_replace.is_none() {
                    tracked.before_the_replace = Some(Box::new(tracked.order.clone()));
                }
                tracked.remaining = (order.total_quantity - tracked.filled).max(0.0);
                tracked.contract = contract;
                // What the wire keeps across a replace, kept here too, in both
                // directions — for an order placed here. The engine restates
                // the parent link, the group and its type from the record of
                // the placement whatever the replace states, so a caller's
                // empty value there does not detach the order and a caller's
                // new one does not attach it; a new one is refused before this.
                // An order the venue named is restated from the caller's
                // statement of it on every replace, so there the record
                // follows the caller, as the venue does. The client is the
                // record's either way: a replace states no client, and where
                // the record names none the venue's answer is read in its
                // place wherever the client is reported.
                let kept = (tracked.order.client_id, tracked.order.parent_id, tracked.order.oca_group.clone(), tracked.order.oca_type);
                let placed_here = tracked.placed_here;
                tracked.order = order;
                tracked.order.client_id = kept.0;
                if placed_here {
                    tracked.order.parent_id = kept.1;
                    tracked.order.oca_group = kept.2;
                    tracked.order.oca_type = kept.3;
                }
            }
            None => {
                // A caller replacing an order the venue replayed at connect:
                // this client did not place it, so what the venue has said is
                // the only account of what it has done. Written without that,
                // the record read as a fresh order — nothing filled, its whole
                // quantity outstanding, a status of its own invention, and
                // slot zero, which is a real slot and not this order's, so the
                // next replace was refused for naming another contract.
                let known = venue.and_then(|v| v.orders.get_order_info(order_id));
                let filled = known.as_ref().map_or(0.0, |i| i.order.filled_quantity);
                let status = known.as_ref().map_or_else(
                    || "PendingSubmit".to_string(), |i| i.order_state.status.clone(),
                );
                // And what the venue holds, for a refusal to put back. The
                // client the order went out under comes from there too: this
                // client did not place it, so the caller's own object says
                // nothing about whose order it is.
                let before_the_replace = known.map(|i| {
                    order.client_id = i.order.client_id;
                    Box::new(i.order)
                });
                let remaining = (order.total_quantity - filled).max(0.0);
                orders.insert(order_id, TrackedOrder {
                    contract, order, status, filled, remaining,
                    avg_fill_price: 0.0, last_fill_price: 0.0,
                    instrument, rejected: false, before_the_replace,
                    placed_here: false,
                });
            }
        }
    }

    /// Track a newly placed order.
    pub fn track_order(&self, order_id: u64, contract: ApiContract, order: ApiOrder, instrument: InstrumentId) {
        self.open_orders.lock().unwrap()
            .insert(order_id, tracked_as_placed(contract, order, instrument));
    }

    /// Note that the venue has finished with a number.
    ///
    /// Recorded where an order reaches a state it cannot leave, so a later
    /// placement under the number is read as the duplicate it is rather than
    /// as a new order. Not recorded where a placement never left this client:
    /// nothing at the venue ever carried that number, and it is the caller's
    /// to use again.
    pub fn note_the_number_is_spent(&self, order_id: u64) {
        self.spent_order_ids.lock().unwrap().insert(order_id);
    }

    /// Whether the venue has already worked an order under this number.
    pub fn the_number_is_spent(&self, order_id: u64) -> bool {
        self.spent_order_ids.lock().unwrap().contains(&order_id)
    }

    /// Update a tracked order after a fill. Removes the order if fully filled.
    pub fn update_order_fill(&self, order_id: u64, status: &str, filled: f64, remaining: f64) {
        // Through the one place an order leaves the book, so what was staged
        // against it goes with it here as on every other path.
        if remaining == 0.0 {
            self.note_the_number_is_spent(order_id);
            return self.untrack_order(order_id);
        }
        if let Some(o) = self.open_orders.lock().unwrap().get_mut(&order_id) {
            o.status = status.into();
            o.filled = filled;
            o.remaining = remaining;
        }
    }

    /// Stop tracking an order the venue has said does not exist.
    ///
    /// A cancel rejected as UnknownOrder retires the engine's record, and the
    /// client's own record has to go with it — the open-order snapshot unions
    /// the two, so leaving this one behind kept reporting the order the
    /// rejection was about.
    ///
    /// A revision of it waiting to be transmitted goes too: an order that has
    /// left the book has nothing left to revise, and nothing took the revision
    /// back. It stayed in the hold for the life of the session, and the next
    /// order placed beside it released a replace for an order that had already
    /// finished. A held placement stays: the venue was never given it, and it
    /// is still the order the caller asked to send.
    pub fn untrack_order(&self, order_id: u64) {
        self.open_orders.lock().unwrap().remove(&order_id);
        self.held_orders.lock().unwrap()
            .retain(|h| h.order_id != order_id || h.places_the_order());
    }

    /// An order's permanent id and its parent.
    ///
    /// The parent is the one this client recorded where it placed the order:
    /// the engine reads no parent from a report, but a client that placed the
    /// order was told. An order it did not place keeps the engine's answer.
    pub(crate) fn perm_and_parent(&self, shared: &SharedState, order_id: u64) -> (i64, i64) {
        let (perm_id, engine_parent) = shared.orders.get_order_info(order_id)
            .map(|info| (info.order.perm_id, info.order.parent_id))
            .unwrap_or((0, 0));
        (perm_id, self.tracked_parent_id(order_id).unwrap_or(engine_parent))
    }

    /// Which client placed an order, as the venue states it on tag 109.
    ///
    /// Zero where this session has no record of the order, which is also what
    /// the venue states for an order it names no client for.
    pub(crate) fn placing_client(&self, shared: &SharedState, order_id: u64) -> i32 {
        // What this client placed, first. The record here knows the client the
        // order went out under from the moment it went; the venue's book knows
        // nothing about it until the venue names it, so asked of that alone an
        // order this session had just placed was reported under client zero.
        // A record naming no client — a bracket's leg is recorded with none —
        // defers to the venue, which is what zero means here.
        if let Some(client) = self.open_orders.lock().unwrap().get(&order_id)
            .map(|t| t.order.client_id)
            .filter(|c| *c != 0)
        {
            return client;
        }
        // And for an order this client did not place, what the venue says.
        shared.orders.get_order_info(order_id).map_or(0, |info| info.order.client_id)
    }

    /// What tells two contracts on one underlying apart.
    ///
    /// Written on the model now, since it is derived from a contract and needs
    /// nothing of this client's. Kept here because it is the name a program
    /// written against this client already calls.
    pub fn contract_identity(
        last_trade_date: &str, strike: f64, right: &str, multiplier: &str, currency: &str,
    ) -> String {
        crate::types::model::contract_identity(
            last_trade_date, strike, right, multiplier, currency,
        )
    }

    /// Retire the client's record of an order the venue rejected as unknown,
    /// and say what that rejection reports.
    ///
    /// Reason 1 is UnknownOrder: the venue has said the order does not exist,
    /// and the engine has already retired its record. The client's own record
    /// has to go with it, or the open-order snapshot keeps reporting the order
    /// the rejection was about.
    ///
    /// The code says the cancel was refused, and which of the two ways. 202
    /// means an order that was cancelled, so reporting it for a refused cancel
    /// states the opposite and invites a replacement against an order still
    /// working. 10147 means "not found", which is one reason among several.
    pub(crate) fn retire_rejected(&self, reject: &CancelReject) -> (i64, String) {
        if reject.reason_code == 1 {
            self.untrack_order(reject.order_id);
        } else {
            let mut orders = self.open_orders.lock().unwrap();
            if let Some(tracked) = orders.get_mut(&reject.order_id) {
                // The record took the cancel ahead of the venue's answer, and
                // the answer is that the order stands. Left as it was, the
                // order read as leaving for the rest of the session —
                // `req_open_orders` said so — while the venue went on working
                // it, and no later message corrected it, because a refusal is
                // the last thing this order draws. What it goes back to is the
                // engine's own book, not a guess from a status this record has
                // already overwritten.
                if let Some(status) = reject.still_working {
                    tracked.status =
                        crate::types::order_status::order_status_str(status).into();
                }
                // And the terms, where it was the modification that was
                // refused. The record took the attempt ahead of the answer, so
                // a refusal that put back only the status left it stating a
                // price nothing had accepted — and every later cancel and
                // replace restates from the record. Independent of the status
                // above: a refusal from this side of the wire knows the change
                // did not go without knowing where the order stands. A refused
                // cancellation changed no terms, and rolling them back on one
                // undid a replacement the venue may since have taken — and a
                // refusal of a change the venue has already answered is not
                // about the change now outstanding, so it puts nothing back.
                if reject.reject_type == 2 && reject.answers_a_live_change {
                    put_back_the_terms(tracked);
                }
            }
        }
        // 10147 is the order the venue could not find; 10148 is the order it
        // found and would not act on. The reason it stated picks between them.
        let code = if reject.reason_code == 1 { 10147 } else { 10148 };
        let what = if reject.reject_type == 1 { "cancel" } else { "modify" };
        // A refusal from this side of the wire carries no reason of the
        // venue's, and the sentinel that says so is not a reason to hand a
        // caller. Said as a number, "reason: -1" reads as one the venue
        // stated.
        let stated = if reject.reason_code < 0 {
            String::new()
        } else {
            format!(" by the venue (reason: {})", reject.reason_code)
        };
        (code, format!("Order {} {what} rejected{stated}", reject.order_id))
    }

    /// Update a tracked order status from an order update event.
    ///
    /// Takes the pre-stringification `OrderStatus` rather than the ibapi string
    /// so a Rejected transition stays distinct from a genuinely-Inactive one:
    /// both stringify to "Inactive", and only a genuine Inactive is
    /// reactivatable and belongs back in the open-order snapshot.
    ///
    /// Upserts, because an order recovered from an earlier session was never
    /// submitted by this client and so has no entry here. Doing nothing for it
    /// left `collect_open_orders` unable to tell it had just been withdrawn. A fresh
    /// entry seeds contract and order from the same enriched
    /// cache `collect_open_orders` reads, rather than leaving them blank.
    pub fn update_order_status(&self, shared: &SharedState, order_id: u64, status: OrderStatus, filled: f64, remaining: f64, instrument: InstrumentId) {
        let mut orders = self.open_orders.lock().unwrap();
        let o = orders.entry(order_id).or_insert_with(|| {
            let (contract, order) = match shared.orders.get_order_info(order_id) {
                Some(info) => (info.contract, info.order),
                None => (ApiContract::default(), ApiOrder::default()),
            };
            TrackedOrder {
                contract, order, status: String::new(), rejected: false, filled: 0.0, remaining: 0.0,
                avg_fill_price: 0.0, last_fill_price: 0.0,
                instrument, before_the_replace: None, placed_here: false,
            }
        });
        o.status = order_status_str(status).into();
        o.rejected = status == OrderStatus::Rejected;
        o.filled = filled;
        o.remaining = remaining;
        drop(orders);
        // An order that is done is dropped, the way one that fills already is.
        // Only a fill removed it before, and a fill is not how most orders end:
        // a cancelled or rejected one reports a quantity still outstanding and
        // produces none, so it stayed for the life of the session and the cost
        // of listing what is open grew with every cancel. Filled is here too:
        // a fill removes the record on its own path, but the status can arrive
        // without one, and left standing it reads the id as a working order's
        // — so the next order placed under it becomes a modify of an order
        // that is done. Nothing reads it after this — what is open is
        // answered by status, and these are not open. `Inactive` is not among
        // them: it returns to working when whatever holds the order clears.
        if matches!(status, OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected) {
            self.note_the_number_is_spent(order_id);
            self.untrack_order(order_id);
        }
    }

    /// Collect open orders: merge local tracking with shared state.
    /// Returns (order_id, contract, order, status, filled, remaining) for non-terminal
    /// orders.
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

        // The orders whose status this client has withdrawn. A disconnect marks
        // a tracked order unknown without touching the cached view, so the union
        // below re-imported it as working for the whole outage, contradicting
        // the callback the caller had already been given. Scoped to
        // withdrawn statuses so the cache can still carry a genuinely newer one
        // — a terminal local status the cache has since superseded still wins.
        let status_withdrawn: std::collections::HashSet<u64> = self.open_orders.lock().unwrap()
            .iter()
            .filter(|(_, o)| o.status == "Unknown")
            .map(|(&id, _)| id)
            .collect();

        // Local tracked orders (non-terminal, or genuinely-Inactive and
        // still reactivatable —), enriched from secdef cache
        {
            // The client the venue names, read into the answer where the
            // record names none, as the fill's is — and into the answer
            // alone: written into the record, what a later replace kept
            // depended on whether a read had happened in between.
            let named_client: HashMap<u64, i32> = shared_orders.iter()
                .map(|(oid, info)| (*oid, info.order.client_id))
                .collect();
            // And what the venue said its fills went at, read into the answer
            // the same way: the record here is what this client sent, and the
            // prices belong to what the venue did with it.
            let stated_fills: HashMap<u64, (f64, f64)> = shared_orders.iter()
                .map(|(oid, info)| (*oid, (info.last_exec.avg_price, info.last_exec.price)))
                .collect();
            let orders = self.open_orders.lock().unwrap();
            for (&oid, o) in orders.iter() {
                // A margin preview states what an order would cost; nothing
                // reaches the book, so it is not among what is working. Its
                // record exists while the question runs and is read as an
                // open order otherwise — exposure the account does not have.
                if o.order.what_if {
                    continue;
                }
                // And not one the venue has already finished. The record
                // here is this client's own account of the order and a
                // terminal report moves it on the dispatch pass, so a caller
                // asking in between was handed an order the venue had
                // finished with — as working, with a quantity outstanding
                // that is not outstanding. What the bridge remembers finishing
                // is the wire's own statement and outranks the local record.
                if shared.orders.recently_completed(oid) {
                    continue;
                }
                if is_open_status(&o.status) || (o.status == "Inactive" && !o.rejected) {
                    let contract = if o.contract.con_id != 0 {
                        self.get_contract(o.contract.con_id, shared).unwrap_or_else(|| o.contract.clone())
                    } else {
                        o.contract.clone()
                    };
                    let mut order = o.order.clone();
                    if order.client_id == 0 {
                        order.client_id = named_client.get(&oid).copied().unwrap_or(0);
                    }
                    let (avg_fill_price, last_fill_price) =
                        stated_fills.get(&oid).copied().unwrap_or((0.0, 0.0));
                    result.push((oid, TrackedOrder {
                        contract,
                        order,
                        status: o.status.clone(),
                        filled: o.filled,
                        remaining: o.remaining,
                        avg_fill_price,
                        last_fill_price,
                        instrument: o.instrument,
                        rejected: o.rejected, before_the_replace: None, placed_here: o.placed_here,
                    }));
                }
            }
        }

        // Add shared-only entries not already present from local
        for (oid, info) in shared_orders {
            if !is_open_or_reactivatable(&info.order_state.status, &info.order_state.completed_status) {
                continue;
            }
            if status_withdrawn.contains(&oid) {
                continue;
            }
            if !result.iter().any(|(id, _)| *id == oid) {
                let contract = if info.contract.con_id != 0 {
                    shared.reference.get_contract(info.contract.con_id).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                // The order carries its own filled quantity; reporting zero
                // here made a partially filled order that this client did not
                // place read as untouched.
                let filled = info.order.filled_quantity;
                let remaining = (info.order.total_quantity - filled).max(0.0);
                result.push((oid, TrackedOrder {
                    contract,
                    order: info.order,
                    status: info.order_state.status.clone(),
                    filled,
                    remaining,
                    avg_fill_price: info.last_exec.avg_price,
                    last_fill_price: info.last_exec.price,
                    instrument: 0,
                    rejected: false, before_the_replace: None, placed_here: false,
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
        let (eligible_mask, quote_state_mask) = shared.market.quote_attribute_masks(iid);
        let fields = [
            q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
            q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
            q.bid_exch_mask, q.ask_exch_mask, q.last_exch_mask, q.halted,
        ];

        // Single lock acquisition for both read and write of last_quotes.
        let mut map = self.last_quotes.lock().unwrap();
        let last = map.get(&iid).copied().unwrap_or([0i64; 16]);
        // Numbered as the feed this instrument is subscribed to: a delayed feed
        // goes out under the delayed numbers, which is what the caller was
        // told to expect on `market_data_type`.
        let delayed = matches!(
            self.mdt_by_instrument.lock().unwrap().get(&iid),
            Some(&MDT_DELAYED) | Some(&MDT_DELAYED_FROZEN)
        );
        let numbered = |tick_type: i32| if delayed { as_delayed(tick_type) } else { tick_type };

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
                    req_id, tick_type: numbered(tt),
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
                    req_id, tick_type: numbered(tt),
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
        // depends on shared.reference.smart_components. Emit a delta record
        // when the bitmask changes; dispatch resolves the letter string.
        let mut string_ticks = Vec::new();
        // What is cached for this instrument, which is the quote as it stands
        // except where a field could not be rendered yet.
        let mut cached = fields;
        const EXCH_TICKS: &[(usize, i32)] = &[
            (12, TICK_BID_EXCHANGE), (13, TICK_ASK_EXCHANGE), (14, TICK_LAST_EXCHANGE),
        ];
        for &(idx, tt) in EXCH_TICKS {
            if fields[idx] != last[idx] {
                let letters = render_exchange_mask(fields[idx], shared);
                // A mask with bits set and no letters to show for them is
                // one the venue has not named its exchanges for yet. Caching
                // it as delivered leaves it equal to the next mask, so it is
                // never rendered again once the names arrive and the quote's
                // exchange is lost for the life of the subscription.
                if letters.is_empty() && fields[idx] != 0 {
                    cached[idx] = last[idx];
                    continue;
                }
                string_ticks.push(StringTickEvent {
                    req_id, tick_type: tt, value: letters,
                });
                delivered = true;
            }
        }

        // A halt changes what every other tick in this quote means: the prices
        // standing are the ones from before the venue stopped, not a market
        // anyone can deal on. It arrives on the trading-status tick and was
        // written into the quote, compared here, and then cached without being
        // sent anywhere — so the one transition worth hearing about was
        // consumed and could never be delivered again.
        //
        // The venue states it under a number of its own, so it goes out on
        // `tick_generic` as the reference client delivers it.
        let mut generic_ticks = Vec::new();
        if fields[15] != last[15] {
            generic_ticks.push(TickEvent {
                req_id, tick_type: numbered(TICK_HALTED),
                value: fields[15] as f64,
                is_price: false,
            });
            delivered = true;
        }

        map.insert(iid, cached);

        // The extra series the caller asked for. They arrive on records of
        // their own rather than as quote fields, so they are queued as they
        // are decoded and handed over here, beside the quote they were asked
        // for alongside. Each already knows which of the four callbacks
        // carries it, because the venue's own record says.
        for series in shared.market.drain_series_ticks(iid) {
            match series.value {
                crate::types::SeriesValue::Price(value) => ticks.push(TickEvent {
                    req_id, tick_type: series.tick_type, value, is_price: true,
                }),
                crate::types::SeriesValue::Size(value) => ticks.push(TickEvent {
                    req_id, tick_type: series.tick_type, value, is_price: false,
                }),
                crate::types::SeriesValue::Generic(value) => generic_ticks.push(TickEvent {
                    req_id, tick_type: series.tick_type, value, is_price: false,
                }),
                crate::types::SeriesValue::Text(value) => string_ticks.push(StringTickEvent {
                    req_id, tick_type: series.tick_type, value,
                }),
            }
            delivered = true;
        }

        QuotePollResult {
            delayed, ticks, generic_ticks, string_ticks, timestamp, delivered,
            eligible_mask,
            quote_state_mask,
        }
    }

    /// Whether a snapshot has just finished arriving.
    ///
    /// Answers true once, for the pump that sees the snapshot complete. A
    /// request that is not a snapshot is never one of these.
    ///
    /// Note that the venue has stated one of the kinds a snapshot is made of.
    ///
    /// What counts is that a tick of the kind ARRIVED, not what it carried: a
    /// currency pair states its last as minus one and a contract that has not
    /// opened states its open as nothing, and both of those are the venue
    /// answering. Waiting for a figure above zero instead waits out the clock
    /// on every one of them.
    pub fn note_snapshot_tick(&self, req_id: i64, tick_type: i32) {
        // Numbered as the feed the request was made under, which is what the
        // caller was told to expect: a delayed or frozen subscription states
        // its bid, ask, last, close and open under numbers of their own. Only
        // the realtime ones were read here, so a snapshot on either of those
        // feeds could never be completed by anything the venue said — it ran to
        // the sweep every time, however promptly the venue answered.
        let bit = match tick_type {
            1 | 66 => 1u8,   // bid
            2 | 67 => 2,     // ask
            4 | 68 => 4,     // last
            14 | 76 => 8,    // open
            9 | 75 => 16,    // close
            _ => return,
        };
        if let Some((_, stated)) = self.snapshot_reqs.lock().unwrap().get_mut(&req_id) {
            *stated |= bit;
        }
    }

    /// A snapshot ends when the venue has stated every kind one is made of, or
    /// when long enough has passed since it was asked for.
    ///
    /// Both are the reference client's: it holds a snapshot until the bid, the
    /// ask, the last, the open and the close have each been delivered, and
    /// sweeps anything still waiting eleven seconds after the REQUEST — not
    /// eleven since the last thing heard.
    ///
    /// Waiting on the quiet instead, as this did, ends a snapshot on a pause
    /// rather than on an answer, and a contract the venue never says anything
    /// about was never swept at all: the clock only started on the first
    /// delivery, so one that got none waited for ever.
    pub fn check_snapshot_done(&self, req_id: i64) -> bool {
        /// How long after asking the reference client gives up waiting for the
        /// rest of a snapshot.
        const GIVE_UP_AFTER: std::time::Duration = std::time::Duration::from_secs(11);
        /// Every kind: bid, ask, last, open, close.
        const WHOLE: u8 = 1 | 2 | 4 | 8 | 16;

        let mut waiting = self.snapshot_reqs.lock().unwrap();
        let Some((asked_at, stated)) = waiting.get(&req_id).copied() else {
            return false;
        };
        if stated == WHOLE || asked_at.elapsed() >= GIVE_UP_AFTER {
            waiting.remove(&req_id);
            return true;
        }
        false
    }

    /// Snapshot the current instrument→req_id mapping, each with every other
    /// request watching that contract.
    ///
    /// Both under one acquisition. Read apart — the holders here, the watchers
    /// again per contract as each quote is delivered — a withdrawal running in
    /// between left a quote going to the request that had just handed the
    /// subscription on and not to the one that had just taken it.
    pub fn snapshot_instruments(&self) -> Vec<(InstrumentId, i64, Vec<i64>)> {
        let own = self.ownership();
        own.holders
            .iter()
            .map(|(&iid, &req_id)| {
                (iid, req_id, own.following.get(&iid).cloned().unwrap_or_default())
            })
            .collect()
    }

    /// What the venue last marked a contract at, which is its price at
    /// midnight rather than its price now. Read at the point of use; text that
    /// is not a usable price leaves the contract unmarked, rather than valuing
    /// it at whatever the characters happened to come to.
    fn midnight_price(shared: &SharedState, con_id: i64) -> Option<Price> {
        let raw = shared.portfolio.venue_price(con_id)?;
        let price = raw.trim().parse::<f64>().ok().filter(|p| p.is_finite())?;
        Some(crate::types::price_from_f64(price)).filter(|&p| p != 0)
    }

    /// Poll PnL and return update if values changed.
    /// Formula: dailyPnL = Σ(qtyNow × priceNow - valueAtMidnight + moneyTraded),
    /// where the venue states the midnight value and the price it last marked
    /// the contract at, and the client falls back to qtyMidnight × prevClose
    /// and a live quote for whatever the venue did not state.
    /// For positions opened intraday (no seed), synthesizes
    /// moneyTraded = -qtyNow × avgCost so the formula collapses to unrealized P&L.
    pub fn poll_pnl(&self, shared: &SharedState) -> Option<PnlUpdate> {
        let req_id = (*self.pnl_req_id.lock().unwrap())?;
        // Nothing until the venue has stated the account whole on this
        // connection. A trading-connection drop leaves the quotes flowing
        // while the book is stale, and the sum below multiplied the pre-drop
        // quantities by live prices on every tick: a holding the account
        // closed during the outage went on being valued, and its profit
        // reported, until the download arrived.
        if !shared.portfolio.account_download_complete() {
            return None;
        }

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        let positions = shared.portfolio.position_infos();

        let mut con_ids: HashSet<i64> = seeds.keys().copied().collect();
        for pi in &positions {
            con_ids.insert(pi.con_id);
        }
        // An account holding nothing still has a P&L: what it realised today
        // is already in the venue's figures. Returning here reported
        // nothing at all to a caller that had asked to be told.

        self.forget_released_slots(shared);
        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut total_daily: f64 = 0.0;
        let mut total_unrealized: f64 = 0.0;
        let mut total_realized: f64 = 0.0;
        let mut priced = 0usize;
        let mut unpriceable = 0usize;

        for con_id in con_ids {
            let seed = seeds.get(&con_id);
            let pi = shared.portfolio.position_info(con_id);

            // Realized P&L is stated outright by the row and does not depend on
            // knowing either quantity, so it accrues before the guards below.
            total_realized += seed.map(|s| s.realized_pnl).unwrap_or(0.0);

            // A position held at midnight and not currently sizeable is not
            // a flat one: pricing the absence as zero reports the whole
            // overnight holding as sold. It is counted as unpriceable rather
            // than merely skipped, because a total missing one position is not
            // a smaller correct answer.
            if seed.is_some() && pi.is_none() {
                unpriceable += 1;
                continue;
            }
            let qty_now = pi.as_ref().map(|p| p.position).unwrap_or(0.0);
            let avg_cost = pi.as_ref().map(|p| p.avg_cost).unwrap_or(0);
            // Likewise for the overnight leg: a seed row whose quantity did not
            // parse says the position is not intraday, only that its size is
            // unknown, so there is nothing to price it against.
            let Some(qty_midnight) = seed.map_or(Some(0.0), |s| s.qty_midnight) else {
                unpriceable += 1;
                continue;
            };

            // A price is per unit and a contract may be worth many of them.
            // Multiplied by nothing, an option holding was valued at a
            // hundredth of what it is worth and the account total with it, so
            // such a position is left to the venue's figures rather than
            // valued from a price this arithmetic cannot use.
            let carries_multiplier = pi.as_ref().is_some_and(position_is_multiplied);
            let has_size = qty_now != 0.0 || qty_midnight != 0.0;
            if carries_multiplier && has_size {
                unpriceable += 1;
                continue;
            }

            let quote = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            let prev_close = quote.map_or(0, |q| q.close);
            let Some(price_now) = quote.map(|q| q.last).filter(|&p| p != 0) else {
                // A position with size and no live price is one this total
                // is missing, which is what `unpriceable` counts. Skipping it
                // without counting reports the rest of the account as the
                // whole of it.
                if has_size {
                    unpriceable += 1;
                }
                continue;
            };
            // What the position was worth at midnight. The venue states the
            // mark it closed the contract at, and that is what the overnight
            // leg is valued against; a locally derived previous close is used
            // only where the venue said nothing.
            let prev_close = Self::midnight_price(shared, con_id).unwrap_or(prev_close);
            if seed.and_then(|s| s.cost_midnight).is_none() && prev_close == 0 && qty_midnight != 0.0 {
                // Nothing to value the overnight leg against, so this position
                // is missing from the total too.
                unpriceable += 1;
                continue;
            }

            // A position with no seed row and no cost cannot be sized at all.
            // The opening cash synthesized below is `-qty*avgCost`, which is
            // nought where the cost is unknown, and the midnight value is
            // nought too because there is nothing to seed it from — so the
            // whole of what the position is worth now was booked as the day's
            // profit. The feed states a cost often rather than always, so this
            // is reached with the venue's own rows. Counted as one that could
            // not be priced, which is what it is: the unrealized sum below
            // already declines to use an unknown basis, and counting it as
            // priced kept the venue's own figures from standing in.
            if seed.is_none() && avg_cost == 0 {
                unpriceable += 1;
                continue;
            }

            // moneyTradedSinceMidnight (wire 6822) is signed net cash: SELL
            // positive, BUY negative. An intraday-only position
            // has no seed row, so synthesize the opening trade's net cash:
            // -qty*avgCost (cash paid to open a long, received to open a short).
            let money_traded = match seed {
                Some(s) => s.money_traded,
                None => -(qty_now * avg_cost as f64 / PRICE_SCALE_F),
            };

            let mv_now = qty_now * price_now as f64 / PRICE_SCALE_F;
            // The venue states what the position was worth at midnight. That
            // beats sizing the overnight leg against a previous close the
            // client has to find for itself, which it has for no contract it
            // never quoted.
            let mv_midnight = seed.and_then(|s| s.cost_midnight)
                .unwrap_or_else(|| qty_midnight * prev_close as f64 / PRICE_SCALE_F);
            // Daily P&L = value change since midnight plus today's net cash.
            total_daily += mv_now - mv_midnight + money_traded;

            if avg_cost != 0 {
                total_unrealized +=
                    qty_now * price_now.saturating_sub(avg_cost) as f64 / PRICE_SCALE_F;
            }
            priced += 1;
        }

        // No position carried a live quote (a req_pnl-only client never populates
        // con_id_to_instrument, so every position above hits `continue`). Fall back
        // to the venue's account-level P&L, which the venue pushes independently
        // of any market-data subscription. Without this the quote-derived totals stay
        // [0,0,0] and no callback ever fires.
        // A position that could not be priced makes the client-side sum an
        // incomplete account total, not a smaller correct one — and the realized
        // figure has already accrued for it, so the three would not even agree
        // with each other. The venue's account-level numbers are complete
        // by construction, so one unpriceable position sends the whole account
        // to them rather than reporting a partial sum as if it were the total.
        if priced == 0 || unpriceable > 0 {
            let acct = shared.portfolio.account();
            total_daily = acct.daily_pnl as f64 / PRICE_SCALE_F;
            total_unrealized = acct.unrealized_pnl as f64 / PRICE_SCALE_F;
            total_realized = acct.realized_pnl as f64 / PRICE_SCALE_F;
        }

        let pnl = [
            crate::types::price_from_f64(total_daily),
            crate::types::price_from_f64(total_unrealized),
            crate::types::price_from_f64(total_realized),
        ];
        let mut last = self.last_pnl.lock().unwrap();
        if pnl == *last {
            return None;
        }
        *last = pnl;
        Some(PnlUpdate {
            req_id,
            daily_pnl: total_daily,
            unrealized_pnl: total_unrealized,
            realized_pnl: total_realized,
        })
    }

    /// Poll per-position PnL and return updates whose values changed.
    /// Routes the quote lookup by con_id (not first-non-zero across all subscriptions),
    /// computes daily/realized from the matching midnight seed, and synthesizes
    /// money_traded = qty_now × avg_cost for intraday-opened positions.
    pub fn poll_pnl_single(&self, shared: &SharedState) -> Vec<PnlSingleUpdate> {
        let reqs: Vec<(i64, i64)> = self.pnl_single_reqs.lock().unwrap()
            .iter().map(|(&r, &c)| (r, c)).collect();
        // As for the account's own profit: nothing from a book the download
        // has not restated on this connection.
        if reqs.is_empty() || !shared.portfolio.account_download_complete() {
            return Vec::new();
        }

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        self.forget_released_slots(shared);
        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut last_cache = self.last_pnl_single.lock().unwrap();
        let mut results = Vec::new();

        for (req_id, con_id) in reqs {
            let Some(pi) = shared.portfolio.position_info(con_id) else { continue; };
            let qty_now = pi.position;
            let avg_cost = pi.avg_cost;

            let quote = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            // The venue's mark for the position, stated whether or not
            // anything here subscribed to the contract. A quote is per unit
            // and a contract may be worth many of them, so an option or a
            // future is valued from the venue's figure rather than from a
            // price this arithmetic cannot use. This subscription does not
            // depend on a market-data subscription.
            let stated_mark = (pi.market_price != 0).then_some(pi.market_price);
            let price_now = match quote.map(|q| q.last).filter(|&p| p != 0) {
                Some(live) if !position_is_multiplied(&pi) => live,
                _ => match stated_mark {
                    Some(mark) => mark,
                    None => continue,
                },
            };
            let unit_value = if pi.market_value != 0 {
                pi.market_value as f64 / PRICE_SCALE_F
            } else if qty_now != 0.0 && position_is_multiplied(&pi) {
                // The venue states a value that already carries the contract's
                // multiplier and a price that does not. With no value stated,
                // multiplying the quantity by the price alone values a
                // multiplied contract at a hundredth of what it is worth — and
                // that is the branch an update carrying only the price takes,
                // because an absent value reads as zero. Both neighbours here
                // test for this; this one did not.
                //
                // Only where something is held. A position closed today keeps
                // its row, and its value really is nothing — skipped for
                // carrying a multiplier, the caller would never hear the close
                // or the realized figure that came with it.
                continue;
            } else {
                qty_now * price_now as f64 / PRICE_SCALE_F
            };
            let seed = seeds.get(&con_id);
            // An unparseable overnight quantity leaves nothing to price the
            // day's change against. Unlike the whole-account total, that costs
            // this callback only its daily figure: the position, its value, the
            // unrealized and the realized are all still known, and suppressing
            // the callback would leave every one of them stale on the caller's
            // side rather than reporting one it cannot compute.
            let qty_midnight = seed.map_or(Some(0.0), |s| s.qty_midnight);
            // As in poll_pnl, the venue's mark is what the overnight leg is
            // valued against, so the two callbacks value the same position from
            // the same figures.
            let prev_close = Self::midnight_price(shared, con_id)
                .unwrap_or_else(|| quote.map_or(0, |q| q.close));
            // What the venue says the position was worth at midnight, which the
            // client otherwise has to size from the overnight quantity and a
            // previous close it may not hold.
            let stated_midnight = seed.and_then(|s| s.cost_midnight);
            if stated_midnight.is_none() && prev_close == 0 && qty_midnight.unwrap_or(0.0) != 0.0 {
                continue;
            }

            // moneyTradedSinceMidnight (wire 6822) is signed net cash: SELL
            // positive, BUY negative. Synthesize the opening
            // trade's net cash for an intraday-only position (no seed row).
            let money_traded = match seed {
                Some(s) => s.money_traded,
                None => -(qty_now * avg_cost as f64 / PRICE_SCALE_F),
            };

            let mv_now = unit_value;
            // Held at the value last reported when the overnight size is
            // unknown, rather than recomputed from an assumption that would be
            // wrong in a specific direction: treating the absence as flat
            // reports the whole holding as sold, and treating it as no seed at
            // all reports the day's move as the position's entire unrealized.
            let midnight_value = stated_midnight
                .or_else(|| qty_midnight.map(|q| q * prev_close as f64 / PRICE_SCALE_F));
            // A position with no seed row and no cost is in the same case as an
            // unknown overnight size: the opening cash synthesized above is
            // nought where the cost is unknown, and the midnight value is
            // nought because there is nothing to seed it from, so the whole of
            // what the position is worth now would go out as the day's profit.
            // The feed states a cost often rather than always. Held at what was
            // last reported, as the arm below already holds it.
            let basis_unknown = seed.is_none() && avg_cost == 0;
            let daily = match midnight_value {
                Some(mv_midnight) if !basis_unknown => mv_now - mv_midnight + money_traded,
                _ => last_cache.get(&req_id)
                    .map_or(0.0, |prev| prev[1] as f64 / PRICE_SCALE_F),
            };
            // The venue states what the position has made and not realised,
            // and it is the only figure that is right for a contract worth
            // more than one unit of its own price.
            let unrealized = if pi.unrealized_stated {
                pi.unrealized_pnl as f64 / PRICE_SCALE_F
            } else if avg_cost != 0 && !position_is_multiplied(&pi) {
                qty_now * price_now.saturating_sub(avg_cost) as f64 / PRICE_SCALE_F
            } else { 0.0 };
            let realized = seed.map(|s| s.realized_pnl).unwrap_or(0.0);
            let value = mv_now;

            // The quantity whole, fractions included: held as a whole number,
            // a holding that moved inside one unit read as unchanged.
            let snapshot: [i64; 5] = [
                crate::types::qty_from_f64(qty_now),
                crate::types::price_from_f64(daily),
                crate::types::price_from_f64(unrealized),
                crate::types::price_from_f64(realized),
                crate::types::price_from_f64(value),
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

    /// The account figures to deliver, as the venue stated them.
    ///
    /// Built from the venue's statements rather than from this client's
    /// typed copy, which exists before the venue has stated anything and would
    /// otherwise report every figure as zero in no currency.
    ///
    /// Each figure is delivered once and again whenever it changes, per
    /// currency: a figure stated in two currencies is two figures.
    pub fn prepare_account_updates(&self, shared: &SharedState) -> Option<AccountUpdateBatch> {
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return None;
        }
        let stated = shared.portfolio.stated_account_values();
        if stated.is_empty() {
            return None;
        }

        let mut already = self.last_stated_account.lock().unwrap();
        let mut fields = Vec::new();
        for (key, value, currency) in stated {
            let held = already.get(&(key.clone(), currency.clone()));
            if held.map(String::as_str) == Some(value.as_str()) {
                continue;
            }
            already.insert((key.clone(), currency.clone()), value.clone());
            fields.push(AccountFieldUpdate { key, value, currency });
        }

        // Said once, when the venue ends the batch it was sending. Timed out
        // of a quiet spell instead, an account still arriving was called fully
        // stated because it paused, and one that finished early waited on a
        // clock for permission to say so.
        // On the rising edge, not once for the life of the client. A rebuilt
        // connection states the account again and ends that batch too, and a
        // subscriber that had already been told once was never told again --
        // the second download completed in silence, so a caller waiting on the
        // end to know the book is whole waited for ever.
        // The swap runs whichever way the flag reads, so the falling edge is
        // observed: written with the test first, `&&` short-circuits while a
        // download is running, the latch is never cleared, and the end is
        // still said only once for the life of the client.
        let complete = shared.portfolio.account_download_complete();
        let finished = !self.account_end_sent.swap(complete, Ordering::AcqRel) && complete;
        Some(AccountUpdateBatch { fields, finished })
    }

    /// Prepare portfolio updates (position entries) for account streaming.
    /// Returns changed/new position infos when account updates are subscribed.
    pub fn prepare_portfolio_updates(&self, shared: &SharedState) -> Vec<PortfolioUpdateEntry> {
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return Vec::new();
        }
        // On the download being finished, not on anything having been heard.
        // A drop zeroes every row's marks deliberately, and the first figure
        // of the rebuilt connection raised the older flag -- so every holding
        // went out at a price of nothing, a value of nothing and no profit,
        // including any the account had closed while the connection was down.
        // A caller summing what it was worth read zero exposure until the
        // venue got round to pricing them again.
        if !shared.portfolio.account_download_complete() {
            return Vec::new();
        }

        let current = shared.portfolio.position_infos();
        let mut prev_guard = self.last_portfolio.lock().unwrap();
        let is_first = prev_guard.is_none();

        let to_entry = |pi: &PositionInfo| PortfolioUpdateEntry {
            con_id: pi.con_id,
            position: pi.position,
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
            // snapshot) is a genuine update, so compare them too.
            current.iter().filter(|pi| {
                !prev.iter().any(|pp| pp.con_id == pi.con_id
                    && pp.position == pi.position
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

    /// Whether a figure the venue stated answers a tag the caller named.
    ///
    /// `$LEDGER` is not the name of a figure: it is the venue's word for the
    /// per-currency cash rows, bare for the base-currency bucket, `$LEDGER:EUR`
    /// for that currency's, `$LEDGER:ALL` for every one. Matched as a literal
    /// name it matched nothing, and the standard way to read per-currency cash,
    /// exchange rate and per-currency profit came back as an end with no rows —
    /// which a caller cannot tell from an account holding no cash at all.
    fn answers_tag(tag: &str, key: &str, currency: &str) -> bool {
        let Some(after) = tag.strip_prefix("$LEDGER") else { return tag == key };
        // The figures the venue keeps per currency, which are the rows a ledger
        // request asks for. Every other stated figure is about the account as a
        // whole and is not part of a currency bucket.
        //
        // The names are the venue's own and the set is closed, so it is written
        // down here as the venue writes it down. An insured-deposit balance is
        // among them: the venue publishes it as a figure of its own where the
        // session splits it from the cash balance, and folds it into that
        // balance where it does not.
        const LEDGER: [&str; 26] = [
            "Currency", "CashBalance", "TotalCashBalance", "AccruedCash", "StockMarketValue",
            "OptionMarketValue", "FutureOptionValue", "FuturesPNL", "NetLiquidationByCurrency",
            "UnrealizedPnL", "RealizedPnL", "ExchangeRate", "FundValue", "NetDividend",
            "MutualFundValue", "MoneyMarketFundValue", "CorporateBondValue", "TBondValue",
            "TBillValue", "WarrantValue", "FxCashBalance", "AccountOrGroup", "RealCurrency",
            "IssuerOptionValue", "Cryptocurrency", "InsuredDeposit",
        ];
        // Whatever follows the colon is the currency asked for, and no colon at
        // all is the base bucket — the venue's own reading of the text.
        let wanted = after.strip_prefix(':').unwrap_or("BASE");
        LEDGER.contains(&key) && (wanted == "ALL" || wanted == currency)
    }

    /// Prepare the initial summary or the values changed since its last interval.
    pub fn prepare_account_summary(&self, shared: &SharedState, _account_id: &str) -> Option<AccountSummaryBatch> {
        // Wait for gateway account data before delivering summary.
        // As above: on the download being finished. Answered on the first
        // figure, a summary asked for right after connecting -- which is the
        // ordinary idiom -- was handed the few tags parsed so far and its end,
        // before the tags it actually asked for arrived.
        // A session that has ended lets it through: no download is
        // coming, and parked behind the gate the caller could neither receive
        // its end nor withdraw it on the ended session.
        if !shared.portfolio.account_download_complete() && shared.reference.session_over().is_none() {
            return None;
        }
        let mut req = self.account_summary_req.lock().unwrap();
        let mut other = self.account_summary_other_req.lock().unwrap();
        let mut last = self.last_account_summary.lock().unwrap();
        // A subscription outlives its own first answer, but not its session.
        // Held past the end, the caller can neither be told it finished nor
        // withdraw it on a session that is over.
        let session_over = shared.reference.session_over().is_some();

        // What the venue said, in the currency it said it in. Built from this
        // client's typed copy instead, an account held in a currency the venue
        // states its figures in came back as zero: the copy is filled from the
        // rows the venue sends, and a summary asked for before they arrive
        // reported an empty account rather than nothing.
        let stated = shared.portfolio.stated_account_values();
        // "All" is the venue's word for every figure it holds, and a request
        // naming no tag at all means the same. Matched against a local list of
        // names instead, "All" matches none of them and returns empty, and any
        // figure absent from that list is dropped with it: accrued cash, SMA,
        // look-ahead margin, per-currency ledger rows.
        let asked: Vec<(i64, Vec<String>)> = req.iter().chain(other.iter()).cloned().collect();
        for (req_id, tags) in &asked {
            let initial = !last.contains_key(req_id);
            let (when, already) = last.entry(*req_id)
                .or_insert_with(|| (std::time::Instant::now(), HashMap::new()));
            if !initial && when.elapsed() < std::time::Duration::from_secs(180) {
                continue;
            }
            *when = std::time::Instant::now();
            let wants_all = tags.is_empty() || tags.iter().any(|t| t == "All");
            let entries: Vec<_> = stated
                .iter()
                .filter(|(key, _, currency)| {
                    wants_all || tags.iter().any(|t| Self::answers_tag(t, key, currency))
                })
                .filter_map(|(key, value, currency)| {
                    let previous = already.insert((key.clone(), currency.clone()), value.clone());
                    if previous.as_ref() == Some(value) {
                        return None;
                    }
                    Some(AccountSummaryEntry {
                        tag: key.clone(), value: value.clone(), currency: currency.clone(),
                    })
                })
                .collect();
            if initial || !entries.is_empty() {
                if session_over {
                    if req.as_ref().is_some_and(|(id, _)| id == req_id) {
                        *req = None;
                    } else if other.as_ref().is_some_and(|(id, _)| id == req_id) {
                        *other = None;
                    }
                }
                return Some(AccountSummaryBatch { req_id: *req_id, entries });
            }
        }
        None
    }

    // ── Order routing ──

    /// Pre-validate order fields that don't depend on instrument ID.
    /// Call this before `find_or_register_instrument` to fail fast.
    pub fn validate_order(order: &ApiOrder, connected_account: &str) -> Result<(), Refusal> {
        order.side()?;

        // An execution condition names a symbol, an exchange and a security
        // type, and the venue wants all three. Left short, it accepts the
        // order and holds it Inactive with "Invalid value in field # 6246",
        // which names a tag no caller of this client has heard of.
        for condition in &order.conditions {
            if let crate::types::OrderCondition::Execution { symbol, exchange, sec_type, .. } = condition {
                for (what, value) in
                    [("symbol", symbol), ("exchange", exchange), ("security type", sec_type)]
                {
                    if value.trim().is_empty() {
                        return Err(Refusal::stated(CONDITION_CONTRACT_INCOMPLETE, format!(
                            "an execution condition needs a {what}; the venue refuses one \
                             that leaves any of symbol, exchange or security type out",
                        )));
                    }
                }
            }
            // A price condition's trigger method states the set the order
            // carries and the reference enumerates: 0 to 4, 7 or 8. Another
            // value is a caller's mistake and is reported rather than sent as
            // a different trigger.
            if let crate::types::OrderCondition::Price { trigger_method, .. } = condition
                && !matches!(*trigger_method, 0..=4 | 7 | 8)
            {
                return Err(Refusal::stated(TRIGGER_METHOD_INVALID, format!(
                    "a price condition's trigger method {trigger_method} is not one \
                     the venue carries on a condition: it is 0 to 4, 7 or 8, and \
                     anything else would go out as a different trigger than the one stated",
                )));
            }
        }

        // Reject non-finite and out-of-range numerics up front, before any
        // caller-visible order gets built from a NaN, an Infinity, or a
        // magnitude the wire's fixed-point i64 can't hold.
        require_finite_price("lmt_price", order.lmt_price)?;
        require_finite_price("aux_price", order.aux_price)?;
        require_finite_price("discretionary_amt", order.discretionary_amt)?;
        require_finite_price("cash_qty", order.cash_qty)?;
        require_finite_price("trigger_price", order.trigger_price)?;
        require_finite_price("adjusted_stop_price", order.adjusted_stop_price)?;
        require_finite_price("adjusted_stop_limit_price", order.adjusted_stop_limit_price)?;
        // f64::MAX is the sentinel for "not set" on these three; any other
        // value must be finite and representable.
        if order.trail_stop_price != f64::MAX {
            require_finite_price("trail_stop_price", order.trail_stop_price)?;
        }
        if order.lmt_price_offset != f64::MAX {
            require_finite_price("lmt_price_offset", order.lmt_price_offset)?;
        }
        if order.adjusted_trailing_amount != f64::MAX {
            require_finite_price("adjusted_trailing_amount", order.adjusted_trailing_amount)?;
        }
        // Every other field a saturating cast turns into a different,
        // valid-looking number on its way to the wire. Guarding only the
        // handful above lets a NaN ladder step reach the venue as an increment
        // of zero, and a benchmark reference or hedging leg stated as an
        // infinity reach it as the largest price there is.
        for (field, value) in [
            ("scale_price_increment", order.scale_price_increment),
            ("scale_profit_offset", order.scale_profit_offset),
            ("scale_price_adjust_value", order.scale_price_adjust_value),
            ("delta_neutral_aux_price", order.delta_neutral_aux_price),
            ("pegged_change_amount", order.pegged_change_amount),
            ("reference_change_amount", order.reference_change_amount),
            ("starting_price", order.starting_price),
            ("stock_ref_price", order.stock_ref_price),
            ("stock_range_lower", order.stock_range_lower),
            ("stock_range_upper", order.stock_range_upper),
            ("percent_offset", order.percent_offset),
            ("volatility", order.volatility),
        ] {
            // f64::MAX is this API's "not set" and states nothing.
            if value != f64::MAX {
                require_finite_price(field, value)?;
            }
        }
        for (at, leg) in order.order_combo_legs.iter().enumerate() {
            if *leg != f64::MAX {
                require_finite_price(&format!("order_combo_legs[{at}]"), *leg)?;
            }
        }
        // f64::MAX is this API's "not set" here too, and a caller who states
        // it is stating nothing: the models a caller builds from carry it as
        // the default for this field, so refusing it refused every order that
        // never mentioned a trailing percentage at all.
        if order.trailing_percent != f64::MAX
            && (!order.trailing_percent.is_finite()
                || order.trailing_percent < 0.0
                || order.trailing_percent * 100.0 > u32::MAX as f64)
        {
            return Err(format!(
                "trailing_percent must be a finite, non-negative number, got {}",
                order.trailing_percent
            ).into());
        }
        // The quantity reaches the wire as the decimal it was given, so a
        // fraction of a share is carried rather than refused. The bound is
        // where the fixed-point conversion stops being exact: past it the
        // low digits are lost, and the order goes out for a size nobody
        // asked for rather than being refused.
        if !order.total_quantity.is_finite() {
            return Err("total_quantity must be a finite number".to_string().into());
        }
        if order.total_quantity.abs() > crate::types::MAX_QTY_SHARES {
            return Err(format!("total_quantity {} is too large", order.total_quantity).into());
        }
        // Zero and negative go out as they were given. Both encode exactly —
        // tag 38 carries the sign and carries a zero — so the venue is asked
        // and the venue answers, under its own code. Refused here instead, a
        // caller reading that code was handed one this client made up for a
        // request the venue was never asked about.
        if order.parent_id < 0 {
            return Err(format!("parent_id must not be negative, got {}", order.parent_id).into());
        }

        // A time in force this client does not know becomes DAY, and a DAY
        // order dies at the close. A caller who wrote "gtc" and meant GTC gets
        // an order that quietly stops existing, so the spelling is checked
        // rather than fallen back on.
        const TIME_IN_FORCE: [&str; 10] =
            ["DAY", "GTC", "IOC", "FOK", "OPG", "GTD", "GTX", "DTC", "AUC", "NMIN"];
        if !order.tif.is_empty() && !TIME_IN_FORCE.contains(&order.tif.as_str()) {
            return Err(format!(
                "tif '{}' is not one this venue carries. It is one of {}, \
                 spelled exactly — an unrecognised value would otherwise be \
                 sent as DAY and expire at the close.",
                order.tif,
                TIME_IN_FORCE.join(", "),
            ).into());
        }

        // An expiry this client cannot read used to be logged and dropped, and
        // the order then went out with no expiry at all — which for a GTC-until
        // order is a different order from the one asked for.
        if !order.good_till_date.is_empty()
            && let Err(e) = crate::protocol::datetime::parse_ib_expiry(&order.good_till_date)
        {
            return Err(Refusal::stated(GOOD_TILL_DATE_INVALID, format!(
                "good_till_date '{}' cannot be read: {e}. State it as \
                 `yyyyMMdd HH:mm:ss` with an optional zone, or `yyyyMMdd` for \
                 a date — sent unread, the order would carry no expiry.",
                order.good_till_date,
            )));
        }

        // Everything this protocol has no field for. Each is documented on the
        // field with what is known about the absence; each is refused here,
        // because a caller that set one and was answered anyway would have an
        // order the venue never saw the instruction on, and nothing to say so.
        //
        // Compared against the default rather than against emptiness: a field
        // left alone is not a field asked for, and only what a caller stated
        // is refused. The list is checked against the documented one by
        // `scripts/gen_order_field_reach.py`, so a field that gains or loses
        // its note here cannot drift from the note itself.
        static UNCARRIED: LazyLock<ApiOrder> = LazyLock::new(ApiOrder::default);
        macro_rules! refuse_if_stated {
            // A field with something of its own to say says it. The rest share
            // the sentence below, and both are one list, so the registry the
            // reach check reads stays whole.
            ($($field:ident $(: $why:expr)?),+ $(,)?) => {
                $(if order.$field != UNCARRIED.$field {
                    let stated: Option<&str> = None $(.or(Some($why)))?;
                    return Err(match stated {
                        Some(why) => why.to_string(),
                        None => format!(
                            "{} is not carried by this protocol: there is no \
                             field to send it under, so the order would go out \
                             without it and do something other than what was \
                             asked. It is documented on the field with what is \
                             known about the absence. Leave it at its default \
                             to place the order without it.",
                            stringify!($field),
                        ),
                    }.into());
                })+
            };
        }
        refuse_if_stated!(
            algo_id, auction_strategy, basis_points, basis_points_type,
            bond_accrued_interest, delta_neutral_clearing_account,
            delta_neutral_clearing_intent, delta_neutral_designated_location,
            delta_neutral_open_close, delta_neutral_settling_firm,
            delta_neutral_short_sale, delta_neutral_short_sale_slot,
            delta, dont_use_auto_price_for_hedge, opt_out_smart_routing,
            order_misc_options, origin, override_percentage_constraints,
            parent_perm_id, pt_order_id, pt_order_type, randomize_price,
            scale_init_fill_qty, scale_table, shareholder, sl_order_id,
            sl_order_type, smart_combo_routing_params,
            what_if_type,
        );


        // Held until a stated moment. Unreadable, the delay used to be dropped
        // and the order filled at once, which is the opposite of what was asked.
        if !order.good_after_time.is_empty()
            && let Err(e) = crate::protocol::datetime::parse_ib_expiry(&order.good_after_time)
        {
            return Err(format!(
                "good_after_time '{}' cannot be read: {e}. State it as \
                 `yyyyMMdd HH:mm:ss` with an optional zone, or `yyyyMMdd` for \
                 the start of a day — sent unread, the order would go live at \
                 once instead of waiting.",
                order.good_after_time,
            ).into());
        }

        // A quantity or a slot stated as a negative number is not a smaller
        // one, it is a mistake. The conversion clamps it, so the order goes out
        // asking for none of whatever was asked for — an iceberg with no
        // display size, a minimum of nothing, a leg that borrows from nowhere.
        for (what, stated) in [
            ("display_size", i64::from(order.display_size)),
            ("min_qty", i64::from(order.min_qty)),
            ("scale_init_level_size", i64::from(order.scale_init_level_size)),
            ("scale_subs_level_size", i64::from(order.scale_subs_level_size)),
            ("scale_price_adjust_interval", i64::from(order.scale_price_adjust_interval)),
        ] {
            if stated < 0 {
                return Err(format!(
                    "{what} is {stated}, which is not a quantity. Sent, it goes \
                     out as none at all and the order does something other than \
                     what was asked.",
                ).into());
            }
        }
        // These two go out in a single byte, so a value above it is not a
        // larger one — it arrives as whatever fits.
        for (what, stated) in [
            ("volatility_type", order.volatility_type),
            ("short_sale_slot", order.short_sale_slot),
        ] {
            if !(0..=255).contains(&stated) {
                return Err(format!(
                    "{what} is {stated}, which does not fit the field it goes \
                     out in. Sent, it would arrive as a different value.",
                ).into());
            }
        }
        // A cash quantity is what to spend, so a negative one buys nothing: the
        // encoder omits it and the order goes out sized by its quantity alone.
        if order.cash_qty < 0.0 {
            return Err(format!(
                "cash_qty is {}, which is not an amount to spend. Sent, it is \
                 omitted and the order goes out sized by its quantity instead.",
                order.cash_qty,
            ).into());
        }
        // Both halves or neither: a tier named with nothing against it is not
        // an arrangement, and the encoder writes neither part rather than half.
        if order.soft_dollar_tier_name.is_empty() != order.soft_dollar_tier_val.is_empty() {
            return Err(
                "a soft-dollar arrangement is a tier and what it is worth. \
                 Stated with one of the two, neither goes out and the \
                 commission goes wherever the account's default sends it."
                    .to_string()
                    .into(),
            );
        }
        // These carry a sentinel for "not stated", so only a value that is
        // neither the sentinel nor a quantity is a mistake.
        for (what, stated) in [
            ("min_trade_qty", order.min_trade_qty),
            ("post_to_ats", order.post_to_ats),
            ("min_compete_size", order.min_compete_size),
        ] {
            if stated != i32::MAX && stated < 0 {
                return Err(format!(
                    "{what} is {stated}, which is not a quantity. Sent, it goes \
                     out as none at all. Leave it at its default to state none.",
                ).into());
            }
        }

        // Two fields the conversion narrows to a set, turning anything else
        // into the default. An unknown value reaching the venue is narrowed
        // there the same way, but a value outside the set is a caller's
        // mistake and is reported rather than silently changed.
        if !matches!(order.trigger_method, 0..=4 | 7 | 8) {
            return Err(Refusal::stated(TRIGGER_METHOD_INVALID, format!(
                "trigger_method {} is not one this venue carries. It is 0 to 4, \
                 7 or 8 — anything else becomes 0, which is the default trigger \
                 and not the one asked for.",
                order.trigger_method,
            )));
        }
        if order.oca_type != 0 && !matches!(order.oca_type, 1..=4) {
            return Err(format!(
                "oca_type {} is not one this venue carries. It is 1 to 4, or 0 \
                 to leave it unset — anything else is sent as unset and the \
                 group cancels under the venue's default rather than the rule \
                 asked for.",
                order.oca_type,
            ).into());
        }

        // A hedge is stated as a kind and a parameter that goes with it. A
        // kind this client does not know reads as no hedge at all, and the
        // order goes out unhedged; a beta or a ratio it cannot read becomes
        // zero, which is omitted, and the order goes out hedged against
        // nothing. Both are a different order from the one asked for.
        const HEDGE: [&str; 5] = ["D", "B", "F", "P", "S"];
        if !order.hedge_type.is_empty() {
            let kind = order.hedge_type.to_ascii_uppercase();
            if !HEDGE.contains(&kind.as_str()) {
                return Err(format!(
                    "hedge_type '{}' is not one this venue carries. It is one of \
                     {} — delta, beta, FX, pair, or the venue's pair. An \
                     unrecognised kind would otherwise be dropped and the order \
                     sent unhedged.",
                    order.hedge_type,
                    HEDGE.join(", "),
                ).into());
            }
            // Only these two kinds are struck at a number. Delta and FX take
            // no parameter, so one stated with them is not read.
            if matches!(kind.as_str(), "B" | "P")
                && order.hedge_param.parse::<f64>().is_err()
            {
                return Err(format!(
                    "hedge_type '{}' is struck at a number and hedge_param \
                     '{}' is not one. Stated unreadable, the hedge would be \
                     sent as zero and dropped, leaving the order hedged \
                     against nothing.",
                    order.hedge_type, order.hedge_param,
                ).into());
            }
        }

        // Financial-advisor allocation is not wire-encoded, so an accepted
        // fa_group would put the whole size on the connected account rather
        // than spread it across the group, with nothing to show for it.
        // Same class as the FA fields below, and sharper: no encoder reads
        // `order.account` — every order carries tag 1 from the session account —
        // so the quantity fills on the connected account. The echo then confirms
        // the wrong answer, because the open-order snapshot backfills the account
        // from the report only when the caller left it blank. An FA order at
        // least errors; this one filled elsewhere and reported success.
        if !order.account.is_empty() && order.account != connected_account {
            return Err(format!(
                "order.account {:?} is not carried on the order: the quantity \
                 would fill on the connected account {:?} instead, and the \
                 open-order snapshot would still report {:?}",
                order.account, connected_account, order.account,
            ).into());
        }


        let order_type = order.order_type.to_uppercase();

        // An order carrying an algorithm is encoded as a limit and nothing
        // else: the strategy rides on an order whose type byte is written
        // once, as `2`. The caller's own type must therefore be a limit.
        // Accepting another type and encoding a limit anyway sends an order
        // the caller did not describe — `MKT` with an algorithm becomes a
        // limit at whatever `lmt_price` holds.
        if !order.algo_strategy.is_empty() {
            if order.algo_strategy.eq_ignore_ascii_case("Adaptive") {
                adaptive_priority(&order.algo_params)?;
            } else {
                crate::client_core::parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            }
            if !order_type.is_empty() && order_type != "LMT" {
                return Err(format!(
                    "algo_strategy '{}' is carried on a limit order, and this one \
                     states order_type '{}'. Sent as it stands the venue would \
                     receive a limit at {}, which is not the order described.",
                    order.algo_strategy, order.order_type, order.lmt_price,
                ).into());
            }
            return Ok(());
        }
        // A preview asks about an order this client could send, so it answers
        // for the same set of types. Returning before this match sends an
        // unknown type to the wire as a limit, and the venue answers about an
        // order the caller did not ask about.
        match order_type.as_str() {
            "MKT" | "LMT" | "STP" | "STP LMT" | "TRAIL" | "TRAIL LIMIT"
            | "MOC" | "LOC" | "MIT" | "LIT" | "MTL" | "MKT PRT" | "STP PRT"
            | "REL" | "PASSV REL" | "PEG MKT" | "PEG MID" | "PEG MIDPT" | "PEG BEST"
            | "MIDPX" | "MIDPRICE"
            | "SNAP MKT" | "SNAP MID" | "SNAP MIDPT" | "SNAP PRI" | "SNAP PRIM"
            | "PEG BENCH" | "PEGBENCH" | "BOX TOP" => {}
            _ => return Err(Refusal::stated(
                ORDER_TYPE_UNSUPPORTED,
                format!("Unsupported order type: '{}'", order.order_type),
            )),
        }
        if order.what_if {
            return Ok(());
        }

        // Reject orders that require aux_price when it is zero — prevents silent no-
        // trigger bugs.
        match order_type.as_str() {
            "STP" | "STP PRT" | "MIT" if order.aux_price == 0.0 => {
                return Err(Refusal::stated(TRIGGER_PRICE_MISSING, format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0 — \
                     set aux_price to the desired trigger price, not lmt_price",
                    order.order_type
                )));
            }
            "STP LMT" | "LIT" if order.aux_price == 0.0 => {
                return Err(Refusal::stated(TRIGGER_PRICE_MISSING, format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0",
                    order.order_type
                )));
            }
            "TRAIL" if order.trailing_percent == 0.0 && order.aux_price == 0.0 => {
                return Err(Refusal::stated(
                    TRIGGER_PRICE_MISSING,
                    "TRAIL order requires either trailing_percent or aux_price (trail amount) \
                     but both are 0.0",
                ));
            }
            "TRAIL LIMIT" if order.aux_price == 0.0 => {
                return Err(Refusal::stated(
                    TRIGGER_PRICE_MISSING,
                    "TRAIL LIMIT order requires aux_price (trail amount) but got 0.0",
                ));
            }
            // This type's wire shape has one price and no second tag for it
            // to move to, so an order sent without one is malformed rather
            // than refused by the venue under a code a caller could expect.
            "PEG BEST" if order.lmt_price == 0.0 => {
                return Err(Refusal::validation(
                    "PEG BEST order requires lmt_price (the order's price) but got 0.0",
                ));
            }
            _ => {}
        }

        Ok(())
    }

    /// Remember how a request asked for its bar times to be written.
    ///
    /// The reference client numbers the two forms: 1 for the venue's
    /// spelling, 2 for seconds since the epoch. Anything else is 1, which is
    /// what that client does with a number it does not know.
    pub fn note_date_format(&self, req_id: i64, format_date: i32) {
        self.historical_asks.lock().unwrap().entry(req_id).or_default().format_date = format_date;
    }

    /// The end and the duration a bar request named, which its range is
    /// counted from once its bars have all arrived.
    pub fn note_historical_span(&self, req_id: i64, end_date_time: &str, duration: &str) {
        let mut asks = self.historical_asks.lock().unwrap();
        let ask = asks.entry(req_id).or_default();
        ask.end_date_time = end_date_time.to_string();
        ask.duration = duration.to_string();
    }

    /// The range a finished request covered, as stated beside the last bar.
    /// Empty where nothing was asked under the id.
    pub fn historical_range_for(&self, req_id: i64, zone: &str) -> (String, String) {
        let ask = self.historical_asks.lock().unwrap().get(&req_id).cloned();
        ask.filter(|a| !a.duration.is_empty())
            .and_then(|a| {
                crate::protocol::datetime::historical_range(&a.end_date_time, &a.duration, zone)
            })
            .unwrap_or_default()
    }

    /// The date form the caller asked for under `req_id`: 1 where none was
    /// asked, as the reference client takes it.
    pub fn asked_date_format(&self, req_id: i64) -> i32 {
        self.historical_asks.lock().unwrap().get(&req_id).map_or(1, |ask| ask.format_date)
    }

    /// A bar's time, written the way the request that asked for it wanted.
    ///
    /// Where the stamp cannot be read back to an instant it is handed over as
    /// it came: a time nobody can parse is still what the venue said, and
    /// replacing it with a zero would state an instant in 1970.
    pub fn bar_time_for(&self, req_id: i64, stated: &str, zone: &str) -> String {
        let format_date = self
            .historical_asks
            .lock()
            .unwrap()
            .get(&req_id)
            .map_or(1, |ask| ask.format_date);
        crate::protocol::datetime::bar_date_as_asked(stated, format_date, zone)
    }

    /// Forget what each caller was last told of every quote.
    ///
    /// At a market-data drop the engine zeroes every quote, so nothing reads a
    /// pre-drop price as current. Compared against what the caller had been
    /// told, those noughts read as moves and went out as prices; compared
    /// against nothing, only what the venue restates goes out, and what the
    /// caller last heard stands until then.
    pub fn forget_last_quotes(&self) {
        self.last_quotes.lock().unwrap().clear();
    }

    /// The zone a series was stated on, kept for the bars that continue it.
    pub fn note_historical_zone(&self, req_id: i64, zone: &str) {
        if zone.is_empty() {
            return;
        }
        self.historical_asks.lock().unwrap().entry(req_id).or_default().zone = zone.to_string();
    }

    /// A continuing bar's time as the caller asked bars to be dated, on the
    /// zone its history was stated on.
    pub fn bar_time_for_epoch(&self, req_id: i64, secs: i64) -> String {
        let asks = self.historical_asks.lock().unwrap();
        let (format_date, zone) = asks
            .get(&req_id)
            .map_or((1, ""), |ask| (ask.format_date, ask.zone.as_str()));
        crate::protocol::datetime::bar_epoch_as_asked(secs, format_date, zone)
    }

    /// Validate historical-request arguments before anything reaches the
    /// engine: an unrecognized bar_size falls back to 5-minute bars
    /// silently through two divergent tables, and an unrecognized
    /// what_to_show falls back to TRADES. The caller is answered with a
    /// synchronous Err at the call instead of plausible, wrong candles.
    pub fn validate_historical_args(
        bar_size: &str,
        what_to_show: &str,
        keep_up_to_date: bool,
    ) -> Result<(), String> {
        let bs = crate::control::historical::BarSize::from_api_str(bar_size)?;
        // The adjusted series is folded from the whole raw series and the
        // contract's actions, so it is served from a historical request and not
        // named on the wire — its name is not in the table `from_api_str`
        // checks. Kept up to date it never completes, so there is no whole
        // series to fold and no bar this client could hand over adjusted;
        // refused outright rather than at the first bar that cannot form.
        if crate::control::historical::what_to_show_is_adjusted(what_to_show) {
            if keep_up_to_date {
                return Err(
                    "ADJUSTED_LAST cannot be kept up to date: the adjusted series is \
                     folded from the whole raw series and the contract's actions, and a \
                     request that never completes has no whole series to fold"
                        .to_string(),
                );
            }
            return Ok(());
        }
        crate::control::historical::BarDataType::from_api_str(what_to_show)?;
        if keep_up_to_date && !bs.supports_keep_up_to_date() {
            return Err(format!(
                "bar_size '{bar_size}' cannot be kept up to date: what the venue \
                 keeps sending is five-second bars and a bar still forming is folded \
                 from those, so a size that is a whole number of them can be formed \
                 and one shorter than five seconds cannot. A week and a month are \
                 out too: the fold opens a bar on a multiple of its own length from \
                 the epoch, and the venue's week runs Monday to Friday and its month \
                 is a calendar one",
            ));
        }
        Ok(())
    }

    /// What an order states that this client cannot carry out as stated.
    ///
    /// Two cases, and both would otherwise go out meaning something the caller
    /// did not ask for: a delta-neutral order naming no order type for its
    /// hedging leg, which describes nothing to place, and a hedge parameter
    /// given to a hedge type that takes none.
    ///
    /// Everything else an order can state is encoded. This list was once much
    /// longer — volatility, scale, short-sale slots and the rest were refused
    /// here because no encoder carried them. They are carried now.
    pub fn validate_supported_instructions(o: &ApiOrder) -> Result<(), String> {
        let mut unsent: Vec<&str> = Vec::new();
        // A hedging leg with no order type describes nothing to place.
        if o.delta_neutral_order_type.is_empty()
            && (o.delta_neutral_aux_price != f64::MAX || o.delta_neutral_con_id != 0)
        {
            unsent.push("deltaNeutral without deltaNeutralOrderType");
        }
        // A hedge parameter only means something for the kinds that take one.
        if !o.hedge_param.is_empty()
            && !matches!(o.hedge_type.to_ascii_uppercase().as_str(), "B" | "P")
        {
            unsent.push("hedgeParam for a hedge type that takes none");
        }
        if unsent.is_empty() {
            return Ok(());
        }
        Err(format!(
            "this order sets {}, which is not sent — the order placed would be a \
             different one from the order asked for. Remove it, or place the trade \
             it describes directly.",
            unsent.join(", "),
        ))
    }

    /// A combination states its legs on the order, so an order for one is
    /// placeable. What is refused is a combination that names none: the venue
    /// would be given a security type with nothing to build from.
    pub fn validate_combo_legs(sec_type: &str, leg_count: usize) -> Result<(), Refusal> {
        let names_a_combination = sec_type.eq_ignore_ascii_case("BAG")
            || sec_type.eq_ignore_ascii_case("COMBO");
        if leg_count > 0 || !names_a_combination {
            return Ok(());
        }
        Err(Refusal::stated(
            COMBINATION_NEEDS_LEGS,
            "a combination order has no legs: state them on the contract, \
             or use the security type of the thing you mean to trade",
        ))
    }

    /// What each leg of a combination states, before any of it is converted.
    ///
    /// The conversion takes each leg as it finds it: a side it does not
    /// recognise becomes a buy, a negative ratio becomes none, and a slot
    /// outside a byte is clamped to the nearest one that fits. Each of those is
    /// a leg trading the other way, in no size, or borrowing from somewhere
    /// nobody named —
    /// against the rest of a combination that is priced as one thing.
    pub fn validate_leg(at: usize, leg: &crate::types::model::ComboLeg) -> Result<(), Refusal> {
        if !leg.action.eq_ignore_ascii_case("BUY") && !leg.action.eq_ignore_ascii_case("SELL") {
            return Err(Refusal::stated(COMBINATION_LEG_INVALID, format!(
                "leg {at} states side {:?}, which is BUY or SELL. Anything else \
                 is sent as a buy, and the combination trades the wrong way \
                 round on that leg.",
                leg.action,
            )));
        }
        if leg.ratio <= 0 {
            return Err(Refusal::stated(COMBINATION_LEG_INVALID, format!(
                "leg {at} states a ratio of {}, which is not a quantity. Sent, \
                 the leg goes out in no size at all.",
                leg.ratio,
            )));
        }
        for (what, stated) in [("openClose", leg.open_close), ("shortSaleSlot", leg.shorting_policy)] {
            if !(0..=255).contains(&stated) {
                return Err(Refusal::stated(COMBINATION_LEG_INVALID, format!(
                    "leg {at} states {what} as {stated}, which does not fit the \
                     field it goes out in — sent, it would arrive as a \
                     different value.",
                )));
            }
        }
        Ok(())
    }

    /// `con_id` names one contract on its own. Where the caller gave one,
    /// nothing else has to be stated: the venue accepts an order carrying only
    /// the id and the security type, and answers it with a margin preview.
    /// The checks below exist to catch a contract that names a whole chain or
    /// series, which a contract id never does.
    /// Refuse a security type the venue does not permit this account to trade.
    ///
    /// The venue states its permissions at logon and refuses an order on an
    /// unpermitted type by returning it Inactive with no text at all, so
    /// without this the caller is told nothing. Silence here is not
    /// permission: a session that stated none has nothing to enforce.
    ///
    /// Shared, because a guard on one surface and not the other means the
    /// caller it protects depends on which language they wrote in.
    pub fn refuse_unpermitted_sec_type(
        permitted: &std::collections::HashMap<String, Vec<String>>,
        sec_type: &str,
    ) -> Result<(), Refusal> {
        if sec_type.is_empty() || permitted.is_empty() {
            return Ok(());
        }
        let ty = sec_type.to_ascii_uppercase();
        let key = if matches!(ty.as_str(), "BAG" | "COMBO") { "COMB" } else { ty.as_str() };
        if permitted.contains_key(key) {
            return Ok(());
        }
        let mut named: Vec<&str> = permitted.keys().map(String::as_str).collect();
        named.sort_unstable();
        Err(Refusal::stated(SECURITY_NOT_PERMITTED, format!(
            "the account is not permitted to trade {ty}. It is permitted: {}",
            named.join(", "),
        )))
    }

    /// An order states where it is to be filled.
    ///
    /// The venue does not choose a destination, and neither does this client:
    /// looking a contract up without one answers with whichever listing came
    /// first, which is how an order reaches a venue the caller never named.
    /// The reference client is refused by the server here, by name.
    pub fn validate_order_destination(exchange: &str) -> Result<(), String> {
        if exchange.trim().is_empty() {
            return Err(
                "an order states the exchange it is to be filled on, and this one \
                 names none".to_string(),
            );
        }
        Ok(())
    }

    /// Refuse an order whose contract does not name one contract:
    /// a symbol alone names a whole option chain.
    pub fn validate_order_contract(con_id: i64, sec_type: &str, identity: &str) -> Result<(), String> {
        if con_id != 0 {
            return Ok(());
        }
        // A currency pair is fully identified by what an order already carries:
        // symbol, currency, security type and destination. There is no expiry,
        // strike, right or multiplier to omit, so the silent mistrade this
        // guard exists to prevent cannot happen for CASH — unlike OPT and FUT,
        // whose orders would go out saying nothing about which strike or which
        // contract month. Verified against a live IDEALPRO book: limit, stop
        // limit, market-if-touched, limit-if-touched, trailing stop limit,
        // relative and hidden all acknowledge and cancel cleanly.
        // A symbol names a stock or a currency pair completely. Anything else
        // needs its expiry, strike, right or multiplier, which an order now
        // restates — so the question is not which type this is but whether the
        // caller said enough to identify one contract.
        let ty = sec_type.to_ascii_uppercase();
        // What identifies one contract differs by kind, and only two kinds need
        // more than the caller has already given.
        //
        // An option and a warrant are one of a chain, so they need the expiry,
        // strike or right that says which one. A future is one of a series and
        // needs its maturity. Everything else is named completely by its symbol
        // and the contract id and local symbol that travel with it, which is
        // how the venue itself names them on an order.
        if matches!(ty.as_str(), "OPT" | "FOP" | "WAR" | "IOPT") {
            if identity.is_empty() {
                return Err(format!(
                    "a {ty} contract needs its expiry, strike or right: the symbol alone \
                     names a whole chain, and an order stating only the symbol would be \
                     filled on whichever contract the gateway picked"
                ));
            }
            return Ok(());
        }
        if matches!(ty.as_str(), "FUT" | "FWD") {
            if identity.is_empty() {
                return Err(format!(
                    "a {ty} contract needs its maturity: the symbol alone names a series, \
                     and an order stating only the symbol would be filled on whichever \
                     contract the gateway picked"
                ));
            }
            return Ok(());
        }
        // A combination states its legs on the order itself, so it needs no
        // identity of its own here. An order that names one and states no legs
        // is refused by `validate_combo_legs` before this.
        Ok(())
    }

    /// What an exercise states, checked before the contract is registered so a
    /// refused one reaches nothing. Returns the action and the quantity the
    /// order carries.
    ///
    /// The documented API names a third action, a hold, which the venue does
    /// not take from a client of this kind. It is refused here rather than sent
    /// and rejected, because the caller who asked for it wants to know that the
    /// position was left alone.
    pub fn validate_exercise(
        exercise_action: i32, exercise_quantity: i32,
        account: &str, connected_account: &str,
    ) -> Result<(u8, u32), String> {
        let action = match exercise_action {
            1 | 2 => exercise_action as u8,
            other => {
                return Err(format!(
                    "exercise_action {other} is not served: 1 exercises, 2 lapses"
                ));
            }
        };
        if exercise_quantity <= 0 {
            return Err(format!(
                "exercise_quantity {exercise_quantity} is not a number of contracts"
            ));
        }
        // Same reason an order's own account field is refused: no encoder reads
        // it, every message carries tag 1 from the session account, so an
        // exercise naming another one would take the position on this account
        // and report the account it was asked for.
        if !account.is_empty() && account != connected_account {
            return Err(format!(
                "account {account:?} is not carried on the order: the exercise \
                 would be taken on the connected account {connected_account:?}"
            ));
        }
        Ok((action, exercise_quantity as u32))
    }

    /// An exercise or a lapse, as the order the venue takes it for: the buy
    /// side, no price, and the action on the attributes so the encoder every
    /// other order goes through emits it.
    ///
    /// The override the documented signature takes is not here. It is a
    /// validation bypass the venue's front end applies while it builds the
    /// order, so no tag carries it and there is nothing to send.
    pub fn build_exercise_request(
        order_id: OrderId, instrument: InstrumentId, action: u8, qty: Qty,
        stated: ExerciseStates,
    ) -> OrderRequest {
        OrderRequest::SubmitEx {
            order_id,
            instrument,
            // An exercise is instructed on the slot the option holds, and the
            // caller states no contract id beside it.
            con_id: 0,
            side: Side::Buy,
            qty,
            kind: OrderKind::Limit { price: 0 },
            tif: b'0',
            attrs: OrderAttrs {
                exercise_action: action,
                manual_order_time: stated.manual_order_time,
                customer_account: stated.customer_account,
                professional_customer: stated.professional_customer,
                ..Default::default()
            },
        }
    }

    /// Build an `OrderRequest` from an API `Order`, handling all order types.
    /// This is the shared order-type match block used by both Rust and Python.
    /// A price the caller left alone is `f64::MAX`, which is not a price and
    /// does not survive being scaled into one.
    fn price_or_unset(v: f64) -> i64 {
        if v == f64::MAX { 0 } else { crate::types::price_from_f64(v) }
    }

    /// Turn what a caller set into the request the engine sends.
    pub fn build_order_request(
        order: &ApiOrder,
        order_id: u64,
        instrument: InstrumentId,
        contract: Option<&crate::types::model::Contract>,
    ) -> Result<ControlCommand, Refusal> {
        let side = order.side()?;
        let qty = crate::types::qty_from_f64(order.total_quantity);
        let order_type = order.order_type.to_uppercase();

        // Every order type carries its extended attributes and its time-in-force
        // through one encoder. Choosing per type between an attribute-carrying
        // request and a plain one is how an order type ends up shipping
        // without something the caller set: a bracket child that arrives
        // unlinked and immediate, an adjustable stop that never adjusts, an
        // algo that runs without its parameters.

        // The legs live on the contract, not the order, so they are attached
        // here rather than in `attrs()`.
        // The caller may price the legs separately rather than pricing the
        // combination. The prices are given as their own list, in leg order,
        // so each is put on the leg it belongs to here — kept apart, one would
        // go out against another leg the moment the legs were reordered.
        let leg_prices = order.order_combo_legs.as_slice();
        let leg_specs: Vec<crate::types::ComboLegSpec> =
            contract.map(|c| c.combo_legs.as_slice()).unwrap_or(&[]).iter().enumerate().map(|(at, l)| {
            crate::types::ComboLegSpec {
                con_id: l.con_id,
                ratio: l.ratio.max(0) as u32,
                is_sell: l.action.eq_ignore_ascii_case("SELL"),
                exchange: if l.exchange.eq_ignore_ascii_case("SMART") {
                    String::new()
                } else {
                    l.exchange.clone()
                },
                open_close: l.open_close.clamp(0, 255) as u8,
                short_sale_slot: l.shorting_policy.clamp(0, 255) as u8,
                designated_location: l.designated_location.clone(),
                exempt_code: l.exempt_code,
                price: leg_prices
                    .get(at)
                    .copied()
                    .filter(|p| *p != f64::MAX)
                    .map(crate::types::price_from_f64),
            }
        }).collect();
        // The contract the caller named, so the engine can see that the slot
        // beside it is no longer the one they meant.
        let con_id = contract.map_or(0, |c| c.con_id);
        let ex = |kind: OrderKind| OrderRequest::SubmitEx {
            order_id, instrument, con_id, side, qty,
            kind,
            tif: order.tif_byte(),
            attrs: crate::types::OrderAttrs {
                combo_legs: leg_specs.clone(),
                // The listing exchange and the hedging contract are stated on
                // the contract, not the order, so they are picked up here.
                primary_exchange: contract
                    .map(|c| c.primary_exchange.clone()).unwrap_or_default(),
                delta_neutral_contract: contract
                    .and_then(|c| c.delta_neutral_contract.as_ref())
                    .map(|d| Box::new(crate::types::DeltaNeutralContractSpec {
                        con_id: d.con_id,
                        delta: d.delta,
                        price: d.price,
                    })),
                ..order.attrs()
            },
        };

        // Adaptive orders (special-cased before generic algo)
        if order.algo_strategy.eq_ignore_ascii_case("Adaptive") {
            let price = crate::types::price_from_f64(order.lmt_price);
            let priority = adaptive_priority(&order.algo_params)?;
            return Ok(ControlCommand::Order(ex(OrderKind::Adaptive { price, priority })));
        }

        // Algo orders
        if !order.algo_strategy.is_empty() {
            let algo = crate::client_core::parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            let price = crate::types::price_from_f64(order.lmt_price);
            return Ok(ControlCommand::Order(ex(OrderKind::Algo { price, algo })));
        }

        // Adjustable stop: a base STP that converts to another order type when
        // its trigger is reached. Signalled by a non-empty adjustedOrderType,
        // which is empty on every ordinary order, so this affects nothing else.
        // A Trail/TrailLimit conversion carries the trailing amount + unit
        // (tags 6260/6269).
        if !order.adjusted_order_type.is_empty() {
            let adjusted = match order.adjusted_order_type.to_uppercase().as_str() {
                "STP" => AdjustedOrderType::Stop,
                "STP LMT" => AdjustedOrderType::StopLimit,
                "TRAIL" => AdjustedOrderType::Trail,
                "TRAIL LIMIT" => AdjustedOrderType::TrailLimit,
                other => return Err(Refusal::validation(
                    format!("unknown adjustedOrderType '{other}'"),
                )),
            };
            let scale = |v: f64| crate::types::price_from_f64(v);
            // adjusted_trailing_amount defaults to f64::MAX when unset.
            let adj_trail = if order.adjusted_trailing_amount == f64::MAX {
                0.0
            } else {
                order.adjusted_trailing_amount
            };
            // Through the same construction every other order type uses, so a
            // bracket child keeps its parent link, its OCA group and its tif —
            // and so does everything the contract states rather than the order:
            // its legs, its listing exchange and the contract it hedges
            // against. Built from `order.attrs()` alone, an adjustable stop on
            // a combination reached the encoder with no legs at all.
            return Ok(ControlCommand::Order(ex(OrderKind::AdjustableStop {
                stop_price: scale(order.aux_price),
                trigger_price: scale(order.trigger_price),
                adjusted_order_type: adjusted,
                adjusted_stop_price: scale(order.adjusted_stop_price),
                adjusted_stop_limit_price: scale(order.adjusted_stop_limit_price),
                adjusted_trailing_amount: scale(adj_trail),
                adjustable_trailing_unit: order.adjustable_trailing_unit,
            })));
        }

        let req = match order_type.as_str() {
            "MKT" => {
                ex(OrderKind::Market)
            }
            "LMT" => {
                let price = crate::types::price_from_f64(order.lmt_price);
                ex(OrderKind::Limit { price })
            }
            "STP" => {
                let stop = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::Stop { stop_price: stop })
            }
            "STP LMT" => {
                let price = crate::types::price_from_f64(order.lmt_price);
                let stop = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::StopLimit { price, stop_price: stop })
            }
            "TRAIL" => {
                // Optional initial stop trigger (tag 6117); default f64::MAX = unset.
                let trail_stop = if order.trail_stop_price == f64::MAX { 0 } else { crate::types::price_from_f64(order.trail_stop_price) };
                if order.trailing_percent > 0.0 {
                    // Wire granularity is basis points, so a percentage
                    // stated finer than that is put on the nearest one.
                    // Rounded rather than cut: a hundredth of a per cent is
                    // not exactly a double, and 0.29 times a hundred is
                    // 28.999999999999996, so cutting sends 0.28 for a figure
                    // the wire can carry exactly. Five hundred and
                    // seventy-three of the ten thousand basis points went out
                    // a point low. This is the hazard `price_from_f64` names
                    // and rounds for on the price path. validate_order has
                    // already confirmed the value is finite, non-negative and
                    // fits u32 once scaled.
                    let pct = (order.trailing_percent * 100.0).round() as u32;
                    ex(OrderKind::TrailPct { trail_pct: pct, trail_stop_price: trail_stop })
                } else {
                    let trail = crate::types::price_from_f64(order.aux_price);
                    ex(OrderKind::TrailingStop { trail_amt: trail, trail_stop_price: trail_stop })
                }
            }
            "TRAIL LIMIT" => {
                // Wire-side semantic is `LimitPriceOffset` (tag 6370), not an
                // absolute limit price. Prefer `lmt_price_offset`; fall back
                // to `lmt_price` for callers that haven't migrated.
                let offset_f = if order.lmt_price_offset != f64::MAX {
                    order.lmt_price_offset
                } else {
                    order.lmt_price
                };
                let lmt_offset = crate::types::price_from_f64(offset_f);
                let trail = crate::types::price_from_f64(order.aux_price);
                let trail_stop = if order.trail_stop_price == f64::MAX { 0 } else { crate::types::price_from_f64(order.trail_stop_price) };
                ex(OrderKind::TrailingStopLimit { lmt_offset, trail_amt: trail, trail_stop_price: trail_stop })
            }
            "MOC" => {
                ex(OrderKind::Moc)
            }
            "LOC" => {
                let price = crate::types::price_from_f64(order.lmt_price);
                ex(OrderKind::Loc { price })
            }
            "MIT" => {
                let stop = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::Mit { stop_price: stop })
            }
            "LIT" => {
                let price = crate::types::price_from_f64(order.lmt_price);
                let stop = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::Lit { price, stop_price: stop })
            }
            "MTL" | "BOX TOP" => {
                ex(OrderKind::Mtl)
            }
            "MKT PRT" => {
                ex(OrderKind::MktPrt)
            }
            "STP PRT" => {
                let stop = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::StpPrt { stop_price: stop })
            }
            "REL" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::Rel { offset })
            }
            // Sits away from the best price and follows it, no further than
            // the cap the caller states. The offset is the aux price, as it is
            // for every relative type here.
            "PASSV REL" => {
                ex(OrderKind::PassiveRel {
                    offset: crate::types::price_from_f64(order.aux_price),
                    price_cap: Self::price_or_unset(order.lmt_price),
                })
            }
            // Sits at the best bid or offer, filling no worse than the price
            // the caller states. What it competes with, and how far it will
            // go towards the midpoint, are stated on the order's attributes.
            "PEG BEST" => {
                ex(OrderKind::PegBest {
                    price: crate::types::price_from_f64(order.lmt_price),
                })
            }
            // Every reference field was already carried here and then read by
            // nobody: a caller setting all six got an order that mentioned none
            // of them.
            "PEG BENCH" | "PEGBENCH" => {
                ex(OrderKind::PegBench {
                    price: crate::types::price_from_f64(order.lmt_price),
                    ref_con_id: order.reference_contract_id.max(0) as u32,
                    is_peg_decrease: order.is_pegged_change_amount_decrease,
                    pegged_change_amount: crate::types::price_from_f64(order.pegged_change_amount),
                    ref_change_amount: crate::types::price_from_f64(order.reference_change_amount),
                    starting_price: Self::price_or_unset(order.starting_price),
                    stock_ref_price: Self::price_or_unset(order.stock_ref_price),
                    ref_exchange: order.reference_exchange_id.clone(),
                })
            }
            "PEG MKT" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                let price_cap = crate::types::price_from_f64(order.lmt_price);
                ex(OrderKind::PegMkt { offset, price_cap })
            }
            "PEG MID" | "PEG MIDPT" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                let price_cap = crate::types::price_from_f64(order.lmt_price);
                ex(OrderKind::PegMid { offset, price_cap })
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = crate::types::price_from_f64(order.lmt_price);
                ex(OrderKind::MidPrice { price_cap: cap })
            }
            "SNAP MKT" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::SnapMkt { offset })
            }
            "SNAP MID" | "SNAP MIDPT" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::SnapMid { offset })
            }
            "SNAP PRI" | "SNAP PRIM" => {
                let offset = crate::types::price_from_f64(order.aux_price);
                ex(OrderKind::SnapPri { offset })
            }
            // The catalogue names this: a type this client cannot state is a
            // type this exchange and security type do not support.
            _ => return Err(Refusal::stated(
                ORDER_TYPE_UNSUPPORTED,
                format!("Unsupported order type: '{}'", order.order_type),
            )),
        };

        Ok(ControlCommand::Order(req))
    }
    /// The contract's terms and the venue's model for it, or why neither
    /// question can be answered.
    pub(crate) fn solve_option(
        &self,
        shared: &SharedState,
        contract: &crate::types::model::Contract,
        // The request the model is being watched for, where one is. A contract
        // stated by description carries no id, so there is nothing to look its
        // model up by — and nothing is ever kept under nought, because one
        // entry there would point every conId-less contract at the first one's
        // slot. The watch opened to obtain the model resolves the contract and
        // records the slot under the request that opened it, so that is where
        // it is found.
        //
        // Without it the lookup could not succeed however long it waited, and
        // the refusal it gave is the one refusal here that means "not yet":
        // the question was kept, re-solved on every pass, and its caller told
        // neither an answer nor a reason for the life of the session.
        //
        // `None` on a first ask, which has opened no watch yet. The number is
        // the caller's own and may already be watching something else — and
        // for a contract with no id of its own, that other contract's slot is
        // what the fallback would find. A model belonging to a different
        // contract answers as readily as the right one and is not the right
        // one.
        watched_under: Option<i64>,
        solve: impl Fn(
            crate::control::option_model::OptionTerms,
            crate::control::option_model::VenueModel,
            &[(f64, f64)],
        ) -> Option<f64>,
    ) -> Result<f64, crate::error_codes::Refusal> {
        // Forget released slots before reading the model: another contract
        // can now hold the slot this contract used to name.
        let by_its_own_id = self.cached_instrument(shared, contract.con_id);
        let instrument = by_its_own_id
            .or_else(|| watched_under.and_then(|req_id| self.watching(req_id)))
            .ok_or_else(|| OPTION_MODEL_UNSTATED.to_string())?;
        let stated = shared
            .market
            .option_model(instrument)
            .ok_or_else(|| OPTION_MODEL_UNSTATED.to_string())?;
        // What the venue did not state is not a number. It writes the largest
        // double where it has nothing to say, which this client passes on
        // as-is because the reference client does — so it has to be read back
        // as silence here rather than taken for a value. Taken for one, a
        // contract with no dividend had the largest double in the world
        // subtracted from its underlying.
        let stated_or_none = |v: f64| (v.is_finite() && v != f64::MAX).then_some(v);
        // How long the contract has left, as the venue says it on the same
        // statement as the rest of its model — carried to the fraction, the
        // hours of the last day included. Counted here instead, from whole
        // days off this machine's clock, a contract expiring today has none
        // left and is refused, and so is every contract from the evening
        // before its expiry, because the clock has already turned over in
        // UTC. The venue's own count says 0.73 of a day where this said none.
        //
        // Its basis is 365: the daily rate it states beside this divides the
        // annual one by 365 exactly.
        let years = match stated_or_none(stated.cal_days) {
            Some(days) if days > 0.0 => days / 365.0,
            // Where it stated none, back to counting — a contract with hours
            // left still reads as expired, which is the old behaviour and not
            // worse than refusing outright.
            _ => years_to_expiry(&contract.last_trade_date_or_contract_month)
                .ok_or_else(|| "the contract states no expiry to measure from".to_string())?,
        };
        // The venue states which of two models it priced this contract on, and
        // this library has one of them. Answering a question about the other
        // with this one gives a number, and a number worked out under the
        // wrong distribution is worse than no answer — so it is refused by
        // name rather than quietly produced.
        if stated.price_based_vol {
            return Err(crate::error_codes::Refusal::validation(
                "the venue priced this contract on a volatility stated in its own price \
                 units, which is a different model from the one this client solves with. \
                 Its own figures for the contract are on the model tick and stand as \
                 stated; what cannot be answered is a question about a price or a \
                 volatility other than the ones it published",
            ));
        }
        // The strike the same way every other figure here is read: the largest
        // double is this venue's word for "not stated", and a contract with no
        // strike is not a contract struck at the largest number a double
        // carries. Taken for one, a call there is worth nothing and was
        // answered as exactly that.
        let strike = stated_or_none(contract.strike)
            .filter(|k| *k > 0.0)
            .ok_or_else(|| "the contract states no strike to solve at".to_string())?;
        // A call or a put, and nothing else. Read as "a call if it says call",
        // an option stating a right this client does not know — or stating
        // none at all — was priced as a put, and a put's price for a contract
        // whose pay-off nobody here can name is a number made up about it.
        enum Right { Call, Put }
        let right = match contract.right.to_ascii_uppercase().as_str() {
            "C" | "CALL" => Right::Call,
            "P" | "PUT" => Right::Put,
            other => {
                return Err(crate::error_codes::Refusal::validation(format!(
                    "this contract states its right as {other:?}, which is neither a call \
                     nor a put, so there is no pay-off to price"
                )));
            }
        };
        let terms = crate::control::option_model::OptionTerms {
            strike,
            years_to_expiry: years,
            is_call: match right {
                Right::Call => true,
                Right::Put => false,
            },
            // The venue's own calculator tells these apart, and so must this:
            // an option on a future is priced on one that drifts nowhere and
            // settles at expiry.
            on_a_future: contract.sec_type.eq_ignore_ascii_case("FOP"),
        };
        let model = crate::control::option_model::VenueModel {
            volatility: stated_or_none(stated.implied_vol)
                .ok_or_else(|| OPTION_MODEL_UNSTATED.to_string())?,
            option_price: stated_or_none(stated.opt_price)
                .ok_or_else(|| OPTION_MODEL_UNSTATED.to_string())?,
            underlying_price: stated_or_none(stated.und_price)
                .ok_or_else(|| OPTION_MODEL_UNSTATED.to_string())?,
            // No dividend stated is no dividend, which is what it means.
            present_value_of_dividends: stated_or_none(stated.pv_dividend).unwrap_or(0.0),
            // No rate stated is no discount, which over the days one of these
            // has left moves the price by less than it is quoted in.
            rate: stated_or_none(stated.rate).unwrap_or(0.0),
            // The venue states no yield, and on an underlying whose dividends
            // it carries as one rather than as a present value, leaving it at
            // nothing prices every call dear and every put cheap by the whole
            // of it. Recovered from the price the venue itself published.
            yield_rate: 0.0,
        };
        // What the underlying pays out over the option's life, where the venue
        // has stated its schedule. It states one per contract, and the one
        // that matters is the underlying's rather than the option's — so it is
        // looked up by what the definition said this option is written on.
        let schedule = shared
            .reference
            .under_con_id(contract.con_id as u32)
            .and_then(|under| shared.reference.dividend_schedule(under))
            .map(|schedule| {
                let today = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| (since.as_secs() / 86_400) as i64)
                    .unwrap_or(0);
                // The far end of the window is the contract's own expiry date,
                // which the caller named. Taken off the count of days the
                // venue states the contract has left — a fraction, rounded up
                // — it was the day after expiry for every hour of a session on
                // a contract expiring at the close, and a payment going ex
                // that day was priced into a contract that never sees it.
                let expires_on = crate::protocol::datetime::day_number(
                    &contract.last_trade_date_or_contract_month,
                );
                expires_on
                    .map(|expires_on| {
                        crate::control::dividends::over_the_life(
                            &schedule, today, expires_on, &contract.currency,
                        )
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        // The carry is solved for on top of whatever the schedule says, not
        // instead of it: the schedule states when the underlying drops and by
        // how much, and the carry puts the price back on the figure the venue
        // published. A recovery that fails is not nought — it means no carry in
        // a plausible range reproduces that figure, so the model is not the
        // venue's and a number worked out on it would be this client's own with
        // the venue's name on it. Taken as nought, that is exactly what was
        // published.
        //
        // A future is the exception: the tree prices one on a price that
        // drifts nowhere, so what the underlying yields never enters the
        // arithmetic and nought is what it uses either way.
        let solved = if terms.on_a_future {
            solve(terms, model, &schedule)
        } else {
            crate::control::option_model::recover_yield(terms, model, &schedule)
                .and_then(|yield_rate| {
                    solve(
                        terms,
                        crate::control::option_model::VenueModel { yield_rate, ..model },
                        &schedule,
                    )
                })
        };
        solved.ok_or_else(|| {
            crate::error_codes::Refusal::validation(
            "this contract cannot be solved under the venue's model for it. The model is \
             anchored to the price the venue published, so a figure no rate reproduces leaves \
             nothing to solve against — and an option far enough into the money is worth its \
             intrinsic value and little else, its price hardly moving with volatility at all, \
             so no one volatility is implied either. Naming a number for either would be \
             picking one rather than solving for it")
        })
    }

}

#[cfg(test)]
mod tests;

// ── Opening a session ──
//
// Both surfaces open a session the same way and then hold what comes back
// differently. What they do identically is written once here.

/// Remember this session, so the next start does not need a second factor.
///
/// Best effort: a session that cannot be written is a slower start next time,
/// never a failed connect now. Called with the file the caller named, and does
/// nothing when they named none.
pub fn remember_session(
    file: Option<&std::path::Path>,
    password: &str,
    gateway: &crate::gateway::Gateway,
    username: &str,
    paper: bool,
) -> crate::auth::resume::ResumableSession {
    let session = crate::auth::resume::ResumableSession {
        token: crate::auth::crypto::strip_leading_zeros(
            &gateway.session_token.to_bytes_be(),
        ).to_vec(),
        server_session_id: gateway.server_session_id.clone(),
        hw_info: gateway.hw_info.clone(),
        encoded: gateway.encoded.clone(),
        username: username.to_string(),
        paper,
    };
    if let Some(path) = file
        && let Err(e) = crate::auth::resume::save(path, password, &session)
    {
        log::warn!("session not saved to {}: {e}", path.display());
    }
    session
}
