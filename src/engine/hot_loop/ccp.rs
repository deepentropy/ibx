use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::bridge::{Event, RichOrderInfo, SharedState};
use crate::api::types as api;
use crate::engine::context::Context;
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::types::{
    CompletedOrder, Fill, InstrumentId, MidnightSeed, NewsBulletin, OrderId, ReqId,
    PositionInfo, Price, Qty, Side, PRICE_SCALE, QTY_SCALE,
};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, clone_for_event, parse_price_tag, parse_qty, decode_tif};

/// Bound for an in-flight contract-details request (secdef reply or
/// per-exchange fan-out). Refreshed on fan-out activity; on expiry the
/// request surfaces error 200 + contract_details_end instead of hanging
/// forever (ibx#227). A gateway rejection arrives in well under a second,
/// and a full 27-exchange fan-out completes within a few.
/// API error 10159 of a matching-symbols request that could not be sent
/// (ibx#369).
pub(crate) const MATCHING_SYMBOLS_SEND_FAILED: &str =
    "Failed to request matching symbols:Error sending message to a CCP.";

const SECDEF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Error 200 text for a lookup that found no contract (ibx#400).
pub(crate) const NO_SECURITY_DEFINITION: &str = "No security definition has been found for the request";

/// Number of most-recent ExecIDs retained for fill deduplication. Bounds the
/// memory of `seen_exec_ids` while staying large enough that a server replay
/// after a reconnect burst still hits the window.
const EXEC_ID_WINDOW: usize = 1024;

/// Convert a FIX OrderID hex string (e.g. "00cf16ed.000225ed.69ca0941.0001") to a stable i64 permId.
/// Uses FNV-1a hash of the first 3 dot-segments (the stable prefix) so that permId
/// remains constant across modifications (the last segment increments on each modify).
/// Extract the value of a single FIX tag from a raw message.
/// `prefix` should include the tag number and `=` (e.g. `b"6256="`).
fn extract_tag_value(msg: &[u8], prefix: &[u8]) -> Option<String> {
    use crate::protocol::fix::SOH;
    for part in msg.split(|&b| b == SOH) {
        if part.starts_with(prefix) {
            return Some(String::from_utf8_lossy(&part[prefix.len()..]).into_owned());
        }
    }
    None
}

/// A UTC time as the reports write it, `YYYYMMDD-HH:MM:SS` with optional
/// `.sss`, to Unix seconds.
pub(crate) fn fix_utc_to_unix_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 17 || b[8] != b'-' || b[11] != b':' || b[14] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, m, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hh, mm, ss) = (num(9..11)?, num(12..14)?, num(15..17)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil date (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Text of warning 399 for an order message (ibx#465): "Order Message:
/// {action} {quantity} {symbol} {exchange} {text}". The exchange is the
/// listing as the contract details give it, `{primaryExchange}.{marketName}`
/// (NASDAQ.NMS for AAPL, as in the capture; the form is only checked for
/// AAPL), else the primary exchange, else the order's exchange.
fn order_message_399(
    parsed: &std::collections::HashMap<u32, String>,
    text: &str,
    context: &Context,
    clord_id: OrderId,
    shared: &SharedState,
) -> String {
    let action = match parsed.get(&54).map(|s| s.as_str()) {
        Some("1") => "BUY",
        Some("5") => "SSHORT",
        Some(_) => "SELL",
        None => match context.order(clord_id).map(|o| o.side) {
            Some(Side::Buy) => "BUY",
            Some(Side::ShortSell) => "SSHORT",
            _ => "SELL",
        },
    };
    let quantity = parsed.get(&38).cloned().unwrap_or_default();
    let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
    let cached = shared.reference.get_contract(con_id);
    let symbol = parsed.get(&55).cloned()
        .or_else(|| cached.as_ref().map(|c| c.symbol.clone()))
        .unwrap_or_default();
    let primary = cached.as_ref().map(|c| c.primary_exchange.clone()).filter(|p| !p.is_empty());
    let exchange = match (primary, shared.reference.market_name(con_id)) {
        (Some(p), Some(m)) => format!("{}.{}", p, m),
        (Some(p), None) => p,
        (None, _) => parsed.get(&207).or_else(|| parsed.get(&6004))
            .map(|e| crate::control::contracts::exchange_from_fix(e).to_string())
            .unwrap_or_default(),
    };
    format!("Order Message: {} {} {} {} {}", action, quantity, symbol, exchange, text)
}

/// Split an execution id into the id without its revision and the
/// revision, the last dot segment read as hex ("0000e0d5.6ab5f36f.01.01" →
/// ("0000e0d5.6ab5f36f.01", 1)). An id with no dot has revision 0.
pub(crate) fn split_exec_revision(exec_id: &str) -> (&str, u64) {
    match exec_id.rsplit_once('.') {
        Some((base, rev)) => match u64::from_str_radix(rev, 16) {
            Ok(r) => (base, r),
            Err(_) => (exec_id, 0),
        },
        None => (exec_id, 0),
    }
}

/// What a fill report says about its execution beyond the fill numbers.
fn fill_exec_of(parsed: &std::collections::HashMap<u32, String>, exec_id: &str) -> crate::bridge::FillExec {
    let tag = |t: u32| parsed.get(&t).filter(|s| !s.is_empty());
    crate::bridge::FillExec {
        exec_id: exec_id.to_string(),
        time_secs: tag(6699).or_else(|| tag(60)).or_else(|| tag(52))
            .and_then(|s| fix_utc_to_unix_secs(s)),
        exchange: tag(100).or_else(|| tag(207)).cloned().unwrap_or_default(),
        client_id: tag(6119).and_then(|s| s.parse().ok()).unwrap_or(0),
        model_code: tag(6700).cloned().unwrap_or_default(),
        order_ref: tag(6010).cloned().unwrap_or_default(),
    }
}

/// The API order id of a report's parent (ibx#329): the parent link
/// carries the parent's order id with its version; the id part is mapped
/// back to the API order id of the order the server holds under it (an
/// order recovered from another session is kept under its API id). 0 when
/// the report has no parent. The OCA group is not a parent.
fn parent_order_id(parsed: &std::collections::HashMap<u32, String>, context: &Context) -> i64 {
    let Some(link) = parsed.get(&6107).filter(|s| !s.is_empty()) else {
        return 0;
    };
    let id_part = link.split('.').next().unwrap_or(link);
    let Ok(id) = id_part.parse::<OrderId>() else {
        return 0;
    };
    let same_id = |clord: &String| clord.split('.').next() == Some(id_part);
    if context.order(id).is_some() || context.last_clord.get(&id).is_some_and(same_id) {
        return id;
    }
    context.last_clord.iter()
        .find(|(_, clord)| same_id(clord))
        .map_or(id, |(&order_id, _)| order_id)
}

fn perm_id_from_fix_order_id(s: &str) -> i64 {
    // Hash only the stable prefix: "00cf16ed.000225ed.69ca0941" (drop ".0001")
    let stable = match s.rmatch_indices('.').next() {
        Some((idx, _)) if s[..idx].contains('.') => &s[..idx],
        _ => s, // no dots or only one segment — hash entire string
    };
    let mut h: u64 = 0xcbf29ce484222325;
    for b in stable.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h >> 1) as i64
}

pub(crate) struct CcpState {
    pub(crate) seen_exec_ids: HashSet<String>,
    /// Insertion order for `seen_exec_ids`, oldest at the front. Used to evict
    /// one entry at a time once the dedup window is full, instead of clearing
    /// the whole set — a wholesale clear would let a post-reconnect server
    /// replay of a recently-seen ExecID double-count a fill (ibx#198).
    pub(crate) exec_id_order: VecDeque<String>,
    /// Commission reports seen, by execution id without its revision:
    /// the highest revision (ibx#471). Bounded like `seen_exec_ids`.
    pub(crate) commission_revisions: std::collections::HashMap<String, u64>,
    pub(crate) commission_order: VecDeque<String>,
    /// conId of this session's fills and the realized P&L counted for each,
    /// by execution id without its revision (ibx#478). Bounded like the
    /// commission window.
    pub(crate) exec_realized: std::collections::HashMap<String, (i64, f64)>,
    /// Order messages already reported as 399, by order and text (ibx#465).
    pub(crate) order_messages_sent: HashSet<(OrderId, String)>,
    pub(crate) exec_realized_order: VecDeque<String>,
    pub(crate) news_subscriptions: Vec<(InstrumentId, u32)>,
    pub(crate) disconnected: bool,
    /// (req_id, is_single_shot). Single-shot = known-conId lookup whose
    /// first 35=d reply is also the last (server emits no 323=5/6 terminator
    /// for these). Multi-record by-symbol/matching-symbols requests push
    /// `false` and rely on the response-type sentinel for is_last.
    /// In-flight secdef requests: (req_id, single_shot, deadline). The
    /// deadline is swept by `sweep_contract_details` so a request the
    /// gateway never answers (SessionReject, dead socket, lost reply)
    /// surfaces error 200 + contract_details_end instead of hanging
    /// forever (ibx#227).
    pub(crate) pending_secdef: Vec<(ReqId, bool, Instant)>,
    /// By-symbol lookups in flight: the request multiplier and the strike
    /// retry (ibx#410, ibx#435).
    pub(crate) pending_lookups: Vec<PendingLookup>,
    /// Known market rule per (conId, exchange), from the records of every
    /// definition reply; a fan-out asks only for the unknown ones (ibx#435).
    pub(crate) market_rule_by_exchange: std::collections::HashMap<(i64, String), u32>,
    /// Matching-symbols requests sent and not answered: (own request id
    /// sent, API reqId). No deadline, as in the reference; cleared without
    /// an answer when the auth link drops (ibx#369).
    pub(crate) pending_matching_symbols: Vec<(u32, ReqId)>,
    /// Next own request id of a matching-symbols request (ibx#369).
    pub(crate) next_matching_symbols_id: u32,
    /// keepUpToDate historical queries routed through CCP: (query_id, req_id)
    pub(crate) pending_kut_historical: Vec<(String, ReqId)>,
    /// tickerId → req_id mapping for keepUpToDate 35=G bar updates
    pub(crate) kut_ticker_map: std::collections::HashMap<u32, ReqId>,
    /// tickerId → minTick for bar decoding
    pub(crate) kut_min_tick: std::collections::HashMap<u32, f64>,
    /// HMAC signing key for XML-carrying CCP messages (selective signing).
    pub(crate) ccp_sign_key: Vec<u8>,
    /// HMAC signing IV — advances only for signed messages, independent of unsigned ones.
    pub(crate) ccp_sign_iv: std::sync::Mutex<Vec<u8>>,
    /// Secdef replies awaiting paired schedule reply (joined by tag 6256).
    pub(crate) pending_schedule_pair: Vec<PendingSchedulePair>,
    /// Counter for internal schedule subscribe req IDs.
    pub(crate) next_schedule_sub_id: u32,
    /// Fan-out state for by-symbol lookups: the records wait for the
    /// per-exchange replies, then become the rows (ibx#435).
    pub(crate) pending_fanout: Vec<PendingFanout>,
    /// Counter for internal fan-out req IDs (tag 320 on per-exchange `35=c`).
    pub(crate) next_fanout_id: u32,
    /// Counter for internal secdef req IDs (auto-fetch on cold-cache positions).
    pub(crate) next_internal_secdef_id: u32,
    /// Option calculations and their model inputs (ibx#442).
    pub(crate) optcalc: super::optcalc::OptCalc,
    /// conIds we've already auto-fetched secdef for, keyed by con_id (dedup).
    pub(crate) auto_fetched_conids: HashSet<i64>,
    /// Scanner results awaiting per-conId contract-detail enrichment.
    /// Each entry parks a parsed `<ScanResponse>` until every cache-miss
    /// con_id has been resolved via the same 35=d path that user-initiated
    /// `reqContractDetails` uses. See ibx#156, ib-agent#142.
    pub(crate) pending_scanner_enrichment: Vec<PendingScannerEnrichment>,
    /// Historical-data requests waiting for the conId of their contract
    /// (ibx#427).
    pub(crate) pending_resolves: Vec<PendingResolve>,
    pub(crate) next_resolve_id: u32,
    /// Requests whose contract was found, with its conId, for the engine
    /// to send.
    pub(crate) resolved_requests: Vec<crate::types::ControlCommand>,
    /// Last execution of the session and its server time, for the fill-up
    /// request after a reconnect (ibx#399).
    pub(crate) last_exec: Option<(String, String)>,
    /// Running number of the trades requests of the session.
    pub(crate) next_trades_request: u32,
    /// Set by a reconnect until the end frame of the order status replay.
    pub(crate) awaiting_status_replay: bool,
    /// When the end frame of the post-reconnect status replay came; the
    /// engine reports the restored link from it (ibx#399).
    pub(crate) status_replay_end_at: Option<Instant>,
}

/// A historical-data request waiting for its contract lookup (ibx#427).
pub(crate) struct PendingResolve {
    /// Request number of the lookup.
    pub lookup_id: u32,
    /// API request id of the waiting request.
    pub req_id: ReqId,
    pub request: crate::types::ControlCommand,
    pub deadline: Instant,
}

/// Request numbers of the contract lookups of historical-data requests
/// (ibx#427): a range of their own, below the market data lookups
/// (0xE000_0000) and the internal lookups (0xF000_0000).
pub(crate) const HIST_LOOKUP_FIRST_ID: u32 = 0xD000_0000;
pub(crate) const HIST_LOOKUP_IDS: u32 = 0x1000_0000;

/// `request` with its contract conId set (ibx#427).
pub(crate) fn request_with_con_id(request: crate::types::ControlCommand, con_id: i64) -> crate::types::ControlCommand {
    use crate::types::ControlCommand as C;
    match request {
        C::FetchHistorical { req_id, symbol, sec_type, exchange, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date, include_expired, .. } =>
            C::FetchHistorical { req_id, con_id, symbol, sec_type, exchange, end_date_time, duration, bar_size, what_to_show, use_rth, keep_up_to_date, include_expired },
        C::FetchHeadTimestamp { req_id, sec_type, exchange, what_to_show, use_rth, .. } =>
            C::FetchHeadTimestamp { req_id, con_id, sec_type, exchange, what_to_show, use_rth },
        C::FetchHistogramData { req_id, sec_type, exchange, use_rth, period, .. } =>
            C::FetchHistogramData { req_id, con_id, sec_type, exchange, use_rth, period },
        C::FetchHistoricalTicks { req_id, sec_type, exchange, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth, .. } =>
            C::FetchHistoricalTicks { req_id, con_id, sec_type, exchange, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth },
        C::FetchHistoricalSchedule { req_id, sec_type, exchange, end_date_time, duration, use_rth, .. } =>
            C::FetchHistoricalSchedule { req_id, con_id, sec_type, exchange, end_date_time, duration, use_rth },
        C::FetchFundamentalData { req_id, report_type, .. } =>
            C::FetchFundamentalData { req_id, con_id, report_type },
        C::CalcOption { req_id, kind, under_price, .. } =>
            C::CalcOption { req_id, con_id, kind, under_price },
        other => other,
    }
}

/// Scanner result parked for contract-detail fan-out.
pub(crate) struct PendingScannerEnrichment {
    pub api_req_id: ReqId,
    pub result: crate::control::scanner::ScannerResult,
    pub awaiting: HashSet<i64>,
    pub deadline: Instant,
}

/// A contract row waiting for its trading schedule. The lookup's
/// contract_details_end follows once none of its rows waits.
pub(crate) struct PendingSchedulePair {
    pub api_req_id: ReqId,
    pub join_key: String,
    pub def: crate::control::contracts::ContractDefinition,
    pub deadline: Instant,
}

/// In-flight by-symbol fan-out: the per-exchange requests sent for the
/// records of a lookup whose market rule on that exchange is not known.
/// Their replies only fill the market rules; the records wait here and
/// become the rows once every request is answered (ibx#435).
pub(crate) struct PendingFanout {
    pub api_req_id: ReqId,
    /// Requests not answered yet: (request id, conId, exchange).
    pub outstanding: Vec<(String, i64, String)>,
    /// Number of requests sent.
    pub total: usize,
    /// The records of the lookup, in reply order.
    pub records: Vec<crate::control::contracts::ContractDefinition>,
    /// Idle deadline, refreshed on every per-exchange reply. One lost or
    /// unparseable fan-out reply out of ~27 previously left the lookup
    /// waiting forever (ibx#227).
    pub deadline: Instant,
}

/// A by-symbol lookup in flight, kept for its answer (ibx#410, ibx#435).
pub(crate) struct PendingLookup {
    pub req_id: ReqId,
    pub lookup: SymbolLookup,
    /// Strike text of the one retry still allowed.
    pub retry_strike: Option<String>,
}

/// A by-symbol contract lookup as the caller asked it.
#[derive(Debug, Clone)]
pub(crate) struct SymbolLookup {
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub currency: String,
    pub filters: crate::types::SecDefFilters,
}

impl SymbolLookup {
    /// Source code of a known identifier type (ib-agent#174); empty
    /// otherwise.
    fn sec_id_source(&self) -> &'static str {
        match self.filters.sec_id_type.to_uppercase().as_str() {
            "ISIN" => "4",
            "CUSIP" => "1",
            _ => "",
        }
    }

    /// The lookup rides a known identifier instead of the symbol.
    fn is_identifier(&self) -> bool {
        !self.filters.sec_id.is_empty() && !self.sec_id_source().is_empty()
    }
}

/// Fields of a by-symbol lookup, message type first, without the sending
/// time. `strike` is the strike text (empty: none). Which fields are
/// written, and their order, follow the reference (ibx#410): a lookup with
/// a strike has no source name, the contract is named by the name fields
/// the caller gave, and a contract month and a full date do not share a
/// field. The request id is the request number after the name of the
/// lookup, and a lookup by symbol with a source asks for expired contracts
/// when the caller does (ibx#229).
fn secdef_by_symbol_fields(req_id: ReqId, lookup: &SymbolLookup, strike: &str) -> Vec<(u32, String)> {
    use crate::control::contracts::{lookup_symbol, SECDEF_BY_IDENTIFIER_NAME, SECDEF_BY_SYMBOL_NAME,
        TAG_IB_LOCAL_SYMBOL, TAG_IB_SOURCE, TAG_IB_TRADING_CLASS, TAG_SYMBOL};
    let f = &lookup.filters;
    let identifier = lookup.is_identifier();
    let name = if identifier { SECDEF_BY_IDENTIFIER_NAME } else { SECDEF_BY_SYMBOL_NAME };
    let mut fields: Vec<(u32, String)> = Vec::with_capacity(16);
    fields.push((fix::TAG_MSG_TYPE, "c".into()));
    fields.push((320, format!("{}{}", name, req_id)));
    fields.push((321, "2".into()));
    if strike.is_empty() {
        fields.push((TAG_IB_SOURCE, "Socket".into()));
        if f.include_expired && !identifier {
            fields.push((6320, "1".into()));
        }
    }
    if identifier {
        // Identifier lookup: the identifier and its source replace the
        // symbol/secType/filters; exchange and currency still ride
        // (ib-agent#174).
        fields.push((22, lookup.sec_id_source().into()));
        fields.push((48, f.sec_id.clone()));
    } else {
        let symbol = lookup_symbol(&lookup.symbol);
        let has_class = !f.trading_class.is_empty();
        let has_local = !f.local_symbol.is_empty();
        if has_class {
            fields.push((TAG_IB_TRADING_CLASS, f.trading_class.clone()));
        }
        if has_local {
            fields.push((TAG_IB_LOCAL_SYMBOL, f.local_symbol.clone()));
        }
        // Trading class alone names the contract without the symbol.
        if !symbol.is_empty() && (has_local || !has_class) {
            fields.push((TAG_SYMBOL, symbol.into_owned()));
        }
        let fix_sec_type = match lookup.sec_type.as_str() {
            "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND", other => other,
        };
        fields.push((167, fix_sec_type.into()));
        let expiry = &f.last_trade_date_or_contract_month;
        if expiry.eq_ignore_ascii_case("NOEXP") {
            fields.push((541, "NOEXP".into()));
        } else if expiry.len() == 6 {
            fields.push((200, expiry.clone()));
        } else if expiry.len() > 6 {
            fields.push((541, expiry.clone()));
        }
        // Right code (ib-agent#171).
        match f.right.to_uppercase().as_str() {
            "C" | "CALL" => fields.push((201, "1".into())),
            "P" | "PUT" => fields.push((201, "0".into())),
            _ => {}
        }
        if !strike.is_empty() {
            fields.push((202, strike.into()));
        }
        if !f.multiplier.is_empty() {
            fields.push((231, f.multiplier.clone()));
        }
    }
    // Exchange and primary exchange are two fields (ibx#229).
    let exchange = if lookup.exchange == "SMART" { "BEST" } else { lookup.exchange.as_str() };
    fields.push((100, exchange.into()));
    if !identifier && !f.primary_exchange.is_empty() {
        fields.push((207, f.primary_exchange.clone()));
    }
    fields.push((15, lookup.currency.clone()));
    fields
}

/// Write one by-symbol lookup on `conn` (ibx#410; also the conId lookup of
/// a market data request, ibx#278).
pub(crate) fn send_symbol_lookup_on(conn: &mut Connection, req_id: ReqId, lookup: &SymbolLookup, strike: &str) {
    let ts = chrono_free_timestamp();
    let body = secdef_by_symbol_fields(req_id, lookup, strike);
    let mut fields: Vec<(u32, &str)> = Vec::with_capacity(body.len() + 1);
    fields.push((fix::TAG_MSG_TYPE, "c"));
    fields.push((fix::TAG_SENDING_TIME, &ts));
    fields.extend(body.iter().skip(1).map(|(t, v)| (*t, v.as_str())));
    let _ = conn.send_fix(&fields);
}

/// One contract_details row.
fn push_contract_row(
    shared: &SharedState,
    event_tx: &Option<Sender<Event>>,
    req_id: ReqId,
    def: crate::control::contracts::ContractDefinition,
) {
    // The zone of its trading hours, for the zone rule of an order's
    // expiry (ibx#335).
    if let Some(zone) = &def.time_zone_id {
        shared.reference.cache_time_zone_id(def.con_id, zone);
    }
    let for_event = clone_for_event(event_tx, &def);
    shared.reference.push_contract_details(req_id, def);
    if let Some(details) = for_event {
        emit(event_tx, Event::ContractDetails { req_id, details });
    }
}

/// Error 200 for a user lookup that found no contract, with no end, as
/// the reference (ibx#400). Internal lookups end silently.
fn push_not_found(req_id: ReqId, shared: &SharedState) {
    if req_id < 0xF000_0000 {
        log::info!("Secdef lookup req_id={}: no security definition", req_id);
        shared.reference.push_historical_error(req_id, 200, NO_SECURITY_DEFINITION.to_string());
    }
}

/// Strike text divided by 100 by moving the decimal point, so the value
/// is exact ("342.8" gives "3.428", "220" gives "2.2").
fn strike_divided_by_100(strike: &str) -> String {
    let (int, frac) = strike.split_once('.').unwrap_or((strike, ""));
    let int = format!("{:0>3}", int);
    let (head, moved) = int.split_at(int.len() - 2);
    let head = head.trim_start_matches('0');
    let head = if head.is_empty() { "0" } else { head };
    let frac = format!("{}{}", moved, frac);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() { head.to_string() } else { format!("{}.{}", head, frac) }
}

impl CcpState {
    pub(crate) fn new() -> Self {
        Self {
            seen_exec_ids: HashSet::with_capacity(256),
            exec_id_order: VecDeque::with_capacity(256),
            commission_revisions: std::collections::HashMap::with_capacity(256),
            commission_order: VecDeque::with_capacity(256),
            exec_realized: std::collections::HashMap::with_capacity(256),
            order_messages_sent: HashSet::new(),
            exec_realized_order: VecDeque::with_capacity(256),
            news_subscriptions: Vec::new(),
            disconnected: false,
            pending_secdef: Vec::new(),
            optcalc: super::optcalc::OptCalc::default(),
            pending_lookups: Vec::new(),
            market_rule_by_exchange: std::collections::HashMap::new(),
            pending_matching_symbols: Vec::new(),
            next_matching_symbols_id: 1,
            pending_kut_historical: Vec::new(),
            kut_ticker_map: std::collections::HashMap::new(),
            kut_min_tick: std::collections::HashMap::new(),
            ccp_sign_key: Vec::new(),
            ccp_sign_iv: std::sync::Mutex::new(Vec::new()),
            pending_schedule_pair: Vec::new(),
            next_schedule_sub_id: 1,
            pending_fanout: Vec::new(),
            next_fanout_id: 1,
            next_internal_secdef_id: 0xF000_0000,
            auto_fetched_conids: HashSet::new(),
            pending_scanner_enrichment: Vec::new(),
            pending_resolves: Vec::new(),
            next_resolve_id: 0,
            resolved_requests: Vec::new(),
            last_exec: None,
            // The login sends the first trades request.
            next_trades_request: 5,
            awaiting_status_replay: false,
            status_replay_end_at: None,
        }
    }

    /// Record `exec_id` in the fill-dedup window. Returns `true` if it is new
    /// (the fill should be processed) and `false` if it was already seen (a
    /// duplicate to skip).
    ///
    /// Backed by a bounded rolling window: once `EXEC_ID_WINDOW` IDs are held,
    /// the oldest is evicted one at a time. This replaces a previous wholesale
    /// `clear()` that dropped the entire history at the cap, which let a
    /// post-reconnect server replay of a recently-seen ExecID double-count the
    /// fill and corrupt the position (ibx#198).
    pub(crate) fn record_exec_id(&mut self, exec_id: &str) -> bool {
        if !self.seen_exec_ids.insert(exec_id.to_string()) {
            return false;
        }
        self.exec_id_order.push_back(exec_id.to_string());
        while self.exec_id_order.len() > EXEC_ID_WINDOW {
            if let Some(old) = self.exec_id_order.pop_front() {
                self.seen_exec_ids.remove(&old);
            }
        }
        true
    }

    /// Remember the conId of a fill of this session, for the realized P&L
    /// its commission frame carries (ibx#478).
    pub(crate) fn record_exec_con_id(&mut self, exec_id: &str, con_id: i64) {
        if exec_id.is_empty() {
            return;
        }
        let (base, _) = split_exec_revision(exec_id);
        if self.exec_realized.insert(base.to_string(), (con_id, 0.0)).is_none() {
            self.exec_realized_order.push_back(base.to_string());
            while self.exec_realized_order.len() > EXEC_ID_WINDOW {
                if let Some(old) = self.exec_realized_order.pop_front() {
                    self.exec_realized.remove(&old);
                }
            }
        }
    }

    /// Record a commission report for `exec_id`. Returns `false` for a
    /// report to skip, as the reference does (ibx#471): the same execution
    /// id again, or a lower revision than one already reported. The revision
    /// is the last dot segment, in hex; a higher one replaces the report.
    pub(crate) fn record_commission(&mut self, exec_id: &str) -> bool {
        let (base, revision) = split_exec_revision(exec_id);
        match self.commission_revisions.get(base) {
            Some(&seen) if revision <= seen => return false,
            Some(_) => {}
            None => {
                self.commission_order.push_back(base.to_string());
                while self.commission_order.len() > EXEC_ID_WINDOW {
                    if let Some(old) = self.commission_order.pop_front() {
                        self.commission_revisions.remove(&old);
                    }
                }
            }
        }
        self.commission_revisions.insert(base.to_string(), revision);
        true
    }

    /// The server's commission frame for one execution (ibx#471). The fill
    /// report carries no commission; this frame follows it (captured
    /// 25/09/2026, 25 ms after the fill). A frame with neither the
    /// commission nor tag 8189 is dropped, like the reference.
    fn handle_commission_report(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        let Some(exec_id) = parsed.get(&17).filter(|s| !s.is_empty()) else {
            log::debug!("Commission report without an execution id: dropped");
            return;
        };
        let commission = parsed.get(&6378).and_then(|s| s.parse::<f64>().ok());
        if commission.is_none() && !parsed.contains_key(&8189) {
            log::debug!("Commission report for {} without a commission: dropped", exec_id);
            return;
        }
        if !self.record_commission(exec_id) {
            log::debug!("Commission report for {} skipped: already reported", exec_id);
            return;
        }
        let unset_when_zero = |v: Option<f64>| v.filter(|x| *x != 0.0).unwrap_or(f64::MAX);
        let report = api::CommissionAndFeesReport {
            exec_id: exec_id.clone(),
            commission_and_fees: commission.unwrap_or(0.0),
            currency: parsed.get(&6381).cloned().unwrap_or_default(),
            realized_pnl: unset_when_zero(parsed.get(&6099).and_then(|s| s.parse().ok())),
            yield_amount: parsed.get(&236).and_then(|s| s.parse().ok()).unwrap_or(f64::MAX),
            yield_redemption_date: parsed.get(&696).filter(|s| s.len() == 8).cloned().unwrap_or_default(),
        };
        log::info!("Commission report: exec={} commission={} {}", report.exec_id, report.commission_and_fees, report.currency);
        // The realized P&L of a fill of this session adds to P&L (ibx#478);
        // a higher revision replaces the amount counted before.
        if report.realized_pnl != f64::MAX {
            let (base, _) = split_exec_revision(exec_id);
            if let Some((con_id, counted)) = self.exec_realized.get_mut(base) {
                let delta = report.realized_pnl - *counted;
                *counted = report.realized_pnl;
                shared.portfolio.add_realized_since_seed(*con_id, delta);
            } else {
                log::debug!("Commission report for {}: no fill of this session, realized P&L not counted", exec_id);
            }
        }
        shared.orders.push_commission_report(report);
    }

    pub(crate) fn poll_executions(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        if self.disconnected { return; }
        let (messages, bad_signature) = match ccp_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {}
                    Err(e) => {
                        log::error!("CCP connection lost: {}", e);
                        self.handle_disconnect(context, event_tx);
                        return;
                    }
                    Ok(_) => {
                        hb.last_ccp_recv = Instant::now();
                        // RTT sample (ibx#158): interval from the test request
                        // to the first inbound traffic after it. On a quiet
                        // link (the ping use case) that is the echo itself.
                        if let Some((_, sent_at)) = hb.pending_ccp_test.take() {
                            shared.set_ccp_rtt(hb.last_ccp_recv.duration_since(sent_at));
                        }
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                let mut bad_signature = false;
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, valid) = conn.unsign(&raw);
                            if !valid { bad_signature = true; break; }
                            match fixcomp::fixcomp_decompress(&unsigned) {
                                Ok(inner) => {
                                    if log::log_enabled!(log::Level::Trace) {
                                        for m in &inner {
                                            log::trace!("WIRE< ccp/comp {}", fix::fmt_pipe(m));
                                        }
                                    }
                                    msgs.extend(inner);
                                }
                                Err(e) => {
                                    log::warn!(
                                        "CCP: dropping malformed FIXCOMP frame ({} bytes): {}",
                                        unsigned.len(), e,
                                    );
                                }
                            }
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, valid) = conn.unsign(&raw);
                            if !valid { bad_signature = true; break; }
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/fix {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, valid) = conn.unsign(&raw);
                            if !valid { bad_signature = true; break; }
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/bin {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Control(_) => {
                            // 8=1 / 8=X control state — not consumed on the order path (ibx#185).
                        }
                    }
                }
                (msgs, bad_signature)
            }
        };
        for msg in &messages {
            self.process_ccp_message(msg, ccp_conn, context, shared, event_tx, hb, account_id);
        }
        // A signature mismatch drops the connection, as the reference does;
        // the reconnect path follows (ibx#275).
        if bad_signature {
            log::error!("CCP frame signature mismatch: connection dropped, reconnecting");
            if let Some(conn) = ccp_conn.as_mut() {
                conn.shutdown();
            }
            self.handle_disconnect(context, event_tx);
        }
    }

    pub(crate) fn process_ccp_message(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };
        match msg_type {
            fix::MSG_EXEC_REPORT => self.handle_exec_report(&parsed, context, shared, event_tx, account_id),
            fix::MSG_CANCEL_REJECT => self.handle_cancel_reject(&parsed, ccp_conn, context, shared, event_tx, hb, account_id),
            fix::MSG_NEWS => self.handle_news_bulletin(&parsed, shared),
            fix::MSG_HEARTBEAT => {}
            fix::MSG_TEST_REQUEST => {
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = ccp_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    hb.last_ccp_sent = Instant::now();
                }
            }
            "3" => {
                let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                let ref_tag = parsed.get(&371).map(|s| s.as_str()).unwrap_or("?");
                // The reject carries no request id: like the reference, it is
                // only logged, and a lookup it hit gets neither an error nor
                // an end from it (ibx#400). Such a lookup stays pending and
                // the deadline sweep (ibx#227) still ends it.
                log::warn!("SessionReject: reason='{}' refTag={}", reason, ref_tag);
            }
            "U" => {
                if let Some(comm) = parsed.get(&6040) {
                    match comm.as_str() {
                        "75" => {
                            // Position + market price feed (init burst + after each fill)
                            self.handle_position_feed(msg, ccp_conn, context, shared, event_tx, hb);
                            shared.portfolio.set_account_download_complete();
                        }
                        "77" => {
                            self.handle_account_summary(&parsed, context, shared);
                            shared.portfolio.set_account_download_complete();
                        }
                        "143" => {
                            // P&L midnight seed — store for client-side daily P&L computation
                            handle_pnl_response(msg, shared);
                        }
                        "20" => {
                            // Reference-data answer: inputs of the option model (ibx#442).
                            if let (Some(id), Some(xml)) = (parsed.get(&320), parsed.get(&6118)) {
                                let (id, xml) = (id.clone(), xml.clone());
                                self.optcalc.xml_reply(&id, &xml, ccp_conn, hb, shared);
                            }
                        }
                        "186" => {
                            if let Some(matches) = crate::control::contracts::parse_matching_symbols_response(msg) {
                                // A 186 frame is the real answer only when it
                                // carries the match-count tag 146 — present
                                // even when the count is zero. Frames without
                                // it are not-ready acks: popping on one would
                                // deliver a bogus empty answer and orphan the
                                // data frame that follows (observed live; the
                                // same ack-then-data shape as the what-if
                                // path). See ibx#228.
                                if extract_tag_value(msg, b"146=").is_none() {
                                    log::debug!("matching-symbols ack frame (no tag 146) — awaiting data frame");
                                } else {
                                // Match the reply to its request by the own
                                // request id it echoes, never by position: an
                                // unknown or missing id is dropped, as in the
                                // reference (ibx#228, ibx#369).
                                let echoed = extract_tag_value(msg, b"320=")
                                    .and_then(|v| v.parse::<u32>().ok());
                                let pos = echoed.and_then(|rid| {
                                    self.pending_matching_symbols.iter().position(|p| p.0 == rid)
                                });
                                if let Some(pos) = pos {
                                    let (_, req_id) = self.pending_matching_symbols.remove(pos);
                                    // An empty result is a legitimate answer
                                    // ("no such symbol") and MUST be delivered:
                                    // dropping it left the caller waiting forever
                                    // and the stale queue head misattributed
                                    // every later reply (ibx#228).
                                    shared.reference.push_matching_symbols(req_id, matches);
                                } else {
                                    log::warn!(
                                        "matching-symbols reply dropped: Unknown request ID {:?} (pending {:?})",
                                        echoed, self.pending_matching_symbols,
                                    );
                                }
                                }
                            }
                        }
                        "60" => self.handle_commission_report(&parsed, shared),
                        // An algo definition answer (ibx#263).
                        "54" => if let Some(xml) = parsed.get(&6118) {
                            log::info!("Algo definitions received for {:?}", parsed.get(&6364));
                            shared.reference.add_algo_definitions(xml);
                        },
                        // The exchange directory: not the depth exchanges,
                        // which come from the routing table (#453).
                        "102" => {}
                        "107" => self.handle_schedule_reply(msg, shared, event_tx),
                        _ => {}
                    }
                }
            }
            "W" => {
                // keepUpToDate historical data responses routed through CCP
                if let Some(xml_tag) = parsed.get(&6118) {
                    if let Some(resp) = crate::control::historical::parse_bar_response(xml_tag) {
                        if let Some(pos) = self.pending_kut_historical.iter().position(|(qid, _)| *qid == resp.query_id) {
                            let (_, req_id) = self.pending_kut_historical[pos];
                            shared.reference.push_historical_data(req_id, resp.clone());
                            if resp.is_complete {
                                // Initial batch done — keep entry for streaming
                            }
                        }
                    }
                    else if let Some(ticker_id_str) = crate::control::historical::parse_ticker_id(xml_tag) {
                        let ticker_id: u32 = ticker_id_str.parse().unwrap_or(0);
                        let min_tick = crate::control::historical::extract_xml_tag(xml_tag, "minTick")
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.01);
                        // Match ticker to a pending keepUpToDate query
                        for (qid, req_id) in &self.pending_kut_historical {
                            if xml_tag.contains(qid) {
                                self.kut_ticker_map.insert(ticker_id, *req_id);
                                self.kut_min_tick.insert(ticker_id, min_tick);
                                break;
                            }
                        }
                    }
                }
            }
            "G" => {
                // keepUpToDate streaming bar updates (same binary format as rtbar)
                let body = match super::find_body_after_tag(msg, b"35=G\x01") {
                    Some(b) => b,
                    None => return,
                };
                let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
                let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
                if body.len() >= 11 {
                    let ticker_id = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                    let timestamp = u32::from_be_bytes([body[6], body[7], body[8], body[9]]);
                    let payload_len = body[10] as usize;
                    if body.len() >= 11 + payload_len {
                        if let Some(&req_id) = self.kut_ticker_map.get(&ticker_id) {
                            let min_tick = self.kut_min_tick.get(&ticker_id).copied().unwrap_or(0.01);
                            let payload = &body[11..11 + payload_len];
                            if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
                                bar.timestamp = timestamp;
                                let hist_bar = crate::control::historical::HistoricalBar {
                                    time: format!("{}", timestamp),
                                    open: bar.open,
                                    high: bar.high,
                                    low: bar.low,
                                    close: bar.close,
                                    volume: bar.volume as i64,
                                    wap: bar.wap,
                                    count: bar.count as u32,
                                };
                                let resp = crate::control::historical::HistoricalResponse {
                                    query_id: String::new(),
                                    timezone: String::new(),
                                    bars: vec![hist_bar],
                                    is_complete: true,
                                };
                                shared.reference.push_historical_data(req_id, resp);
                            }
                        }
                    }
                }
            }
            "UT" | "UM" | "RL" => handle_account_update(msg, context, shared),
            "EB" => handle_account_end(msg, shared),
            "UP" => handle_portfolio_message(msg, context, shared, event_tx),
            "d" => self.handle_secdef_reply(msg, ccp_conn, context, shared, event_tx, hb),
            other => {
                log::debug!("CCP unhandled 35={}: {} bytes", other, msg.len());
            }
        }
    }

    /// A reply to a what-if preview (ibx#462). The gateway may first send a
    /// not-ready frame whose margin fields carry the literal string "n/a"
    /// (parse fails), then a data frame with numbers. Discriminate on
    /// parse-success, NOT positivity: a margin-reducing preview (closing a
    /// position, cash-account sell) legitimately resolves to
    /// init_margin_after == 0, sent as a numeric "0" which must be delivered
    /// (ibx#205). The not-ready frame is not always sent, so the first data
    /// frame is taken with no assumption that one precedes it. A frame is
    /// the real preview when ANY of the six margin fields (three before,
    /// three after) parses as a finite number, as the
    /// reference: each field is "set" when it parses, unset on
    /// nan/unparseable (ibx#213, ibx#214; captured in ib-agent#160). A
    /// reject ends the preview with error 201 and the reason. A reply for a
    /// preview this session did not send is dropped, as the reference does.
    fn handle_what_if(
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let Some(clord) = parsed.get(&11) else { return };
        let Some(&(order_id, instrument)) = context.what_ifs.get(clord.as_str()) else {
            log::info!("What-if reply for {} that this session did not send: dropped", clord);
            return;
        };
        if parsed.get(&39).map(|s| s.as_str()) == Some("8") {
            context.what_ifs.remove(clord.as_str());
            let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("");
            shared.orders.push_order_error(order_id, 201, format!("Order rejected - reason:{}", reason));
            return;
        }
        const MARGIN_TAGS: [u32; 6] = [6826, 6827, 6828, 6092, 6093, 6094];
        let number = |tag: u32| parsed.get(&tag)
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|f| f.is_finite());
        let is_data_frame = MARGIN_TAGS.iter().any(|tag| number(*tag).is_some());
        // A frame with an order message gives an open order with that
        // message as its warning text, and the preview waits for its data
        // frame (ibx#462, `trader.order.bQ.a(gi, e3, fq)@263-321`).
        let warning_text = parsed.get(&6361).cloned().unwrap_or_default();
        if !is_data_frame && warning_text.is_empty() {
            return;
        }
        let final_reply = warning_text.is_empty();
        if final_reply {
            context.what_ifs.remove(clord.as_str());
        }
        let text = |tag: u32| parsed.get(&tag).cloned().unwrap_or_default();
        let state = crate::types::WhatIfState {
            status: what_if_status(parsed).to_string(),
            init_margin_before: number(6826),
            maint_margin_before: number(6827),
            equity_with_loan_before: number(6828),
            init_margin_after: number(6092),
            maint_margin_after: number(6093),
            equity_with_loan_after: number(6094),
            commission: number(6378),
            commission_currency: text(6381),
            margin_currency: text(8130),
            suggested_size: text(6552),
            warning_text,
            // A data frame with a reason: the open order, then error 201
            // (captured 02/10/2026: a what-if of 10,000,000 shares).
            reject_reason: if final_reply { text(58) } else { String::new() },
        };
        let response = crate::types::WhatIfResponse {
            order_id,
            instrument,
            init_margin_before: parse_price_tag(parsed.get(&6826)),
            maint_margin_before: parse_price_tag(parsed.get(&6827)),
            equity_with_loan_before: parse_price_tag(parsed.get(&6828)),
            init_margin_after: parse_price_tag(parsed.get(&6092)),
            maint_margin_after: parse_price_tag(parsed.get(&6093)),
            equity_with_loan_after: parse_price_tag(parsed.get(&6094)),
            commission: parse_price_tag(parsed.get(&6378)),
            state,
            final_reply,
        };
        log::info!("WhatIf response: clord={} order={} initMargin={:.2}->{:.2} commission={:.2}",
            clord, order_id,
            response.init_margin_before as f64 / PRICE_SCALE as f64,
            response.init_margin_after as f64 / PRICE_SCALE as f64,
            response.commission as f64 / PRICE_SCALE as f64);
        // The engine event is the preview's answer: the data reply only.
        // An order-message reply before it goes to the API's open_order
        // alone (phase 72 took it for the answer, paper 02/10/2026).
        shared.orders.push_what_if(response.clone());
        if response.final_reply {
            emit(event_tx, Event::WhatIf(response));
        }
    }

    fn handle_exec_report(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        account_id: &str,
    ) {
        // End markers are not orders (ibx#399): the end of a trades reply
        // (it carries the request id), and the end of the order status
        // replay (wildcard order id).
        if let Some(request) = parsed.get(&6556).filter(|r| !r.starts_with("PT.")) {
            log::info!("ExecReport: end of trades request {}", request);
            return;
        }
        if parsed.get(&11).map(|s| s.as_str()) == Some("*") {
            if self.awaiting_status_replay {
                self.awaiting_status_replay = false;
                self.status_replay_end_at = Some(Instant::now());
                // The held open-order requests are answered from the
                // corrected orders (ibx#251).
                shared.orders.set_open_orders_held(false);
                shared.notify();
            }
            log::debug!("ExecReport: end of order status replay");
            return;
        }

        // A what-if reply goes to the preview, before anything reads the
        // frame as an order: by the ClOrdID the preview was sent under, or
        // by a positive preview flag, as the reference routes it (ibx#462).
        let what_if_clord = parsed.get(&11).is_some_and(|c| context.what_ifs.contains_key(c.as_str()));
        let what_if_flag = parsed.get(&6091).and_then(|v| v.parse::<i64>().ok()).is_some_and(|v| v > 0);
        if what_if_clord || what_if_flag {
            Self::handle_what_if(parsed, context, shared, event_tx);
            return;
        }

        // Orders are looked up by the server's order id, the part of the
        // ClOrdID before its version, as the reference does (ibx#466). An
        // order of an earlier session is kept under the API order id its
        // report gives, so cancel_order(<that id>) finds it; its reports
        // all come under the server's id.
        let server_id = parsed.get(&11).and_then(|s| {
            let stripped = s.strip_prefix('C').unwrap_or(s);
            // Strip versioned suffix (.0, .1, .2) from modify-chained ClOrdIDs
            let base = stripped.split('.').next().unwrap_or(stripped);
            base.parse::<OrderId>().ok()
        }).unwrap_or(0);
        let mut clord_id = context.recovered_keys.get(&server_id).copied().unwrap_or(server_id);

        // Recovery insert: a 35=8 with status New/New (150=0/39=0) for an order
        // that is NOT in this session's context is a cross-session recovery entry
        // pushed by CCP on session establishment. Insert into context.open_orders
        // so subsequent cancel/modify ACKs at ~line 668 can match via
        // context.order(clord_id) and emit OrderUpdate events to the user. ibx#191.
        let is_new_ack = parsed.get(&150).map(|s| s.as_str()) == Some("0")
            && parsed.get(&39).map(|s| s.as_str()) == Some("0");
        if is_new_ack && context.order(clord_id).is_none() {
            // The API order id only names the order to the caller (ibx#466):
            // taken when no order of this session has it.
            let api_id = parsed.get(&6121).and_then(|s| s.parse::<OrderId>().ok())
                .filter(|&id| id != 0 && id != server_id && context.order(id).is_none()
                    && !context.recovered_keys.values().any(|&k| k == id));
            if let Some(api_id) = api_id {
                context.recovered_keys.insert(server_id, api_id);
                clord_id = api_id;
            }
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let side = match parsed.get(&54).map(|s| s.as_str()) {
                Some("1") => Side::Buy,
                Some("5") => Side::ShortSell,
                _ => Side::Sell,
            };
            let qty: Qty = parsed.get(&38).and_then(|s| parse_qty(s)).unwrap_or(0);
            let limit_price_i64: i64 = parsed.get(&44)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * PRICE_SCALE as f64) as i64)
                .unwrap_or(0);
            let stop_price_i64: i64 = parsed.get(&99)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * PRICE_SCALE as f64) as i64)
                .unwrap_or(0);
            let ord_type_byte: u8 = parsed.get(&40).and_then(|s| s.bytes().next()).unwrap_or(b'2');
            let tif_byte: u8 = parsed.get(&59).and_then(|s| s.bytes().next()).unwrap_or(b'1');
            // A full instrument table must not stop the engine: register()
            // panics there, and the panic ended the hot loop (ibx#257).
            let instrument = if con_id != 0 && qty > 0 {
                let slot = context.market.try_register(con_id);
                if slot.is_none() {
                    log::error!("CCP recovery: instrument table full ({} contracts), order {} on con_id {} is not tracked",
                        crate::types::MAX_INSTRUMENTS, clord_id, con_id);
                }
                slot
            } else {
                None
            };
            if let Some(instrument) = instrument {
                // req_global_cancel walks ids below this count; without it a
                // recovered order on a new contract was never cancelled.
                shared.market.set_instrument_count(context.market.count());
                if let Some(sym) = parsed.get(&55) {
                    context.set_symbol(instrument, sym.clone());
                }
                context.insert_order(crate::types::Order {
                    order_id: clord_id,
                    instrument,
                    side,
                    price: limit_price_i64,
                    qty_fixed: qty,
                    filled_fixed: 0,
                    status: crate::types::OrderStatus::Submitted,
                    ord_type: ord_type_byte,
                    tif: tif_byte,
                    stop_price: stop_price_i64,
                });
                log::info!("CCP recovery: inserted orderId={} sym={:?} side={:?} qty={} px={}",
                    clord_id, parsed.get(&55), side, qty as f64 / QTY_SCALE as f64,
                    limit_price_i64 as f64 / PRICE_SCALE as f64);
            }
        }

        // Drop the sentinel/end-of-stream record (ClOrdID="*"/"0"/absent → parses
        // to 0). Real orders are assigned monotonic IDs via next_order_id and
        // never collide with 0. The recovery-push terminator (11='*') lands here.
        if clord_id == 0 {
            log::debug!("ExecReport: dropping sentinel record (ClOrdID=0/*) sym={:?} status={:?}",
                parsed.get(&55), parsed.get(&39));
            return;
        }

        // Record the ClOrdID exactly as the server reports it so subsequent
        // cancel/modify can echo back the same string. Skip reports of a
        // cancel: they carry the cancel request's own id, not the order's
        // (ibx#179). A cancel's id is the one ibx sent (ibx#464), or starts
        // with 'C' (the earlier form, on orders of a previous session).
        let is_cancel_request = parsed.get(&11).is_some_and(|s| {
            s.starts_with('C') || context.cancel_clord.get(&clord_id) == Some(s)
        });
        if let Some(raw_clord) = parsed.get(&11) {
            if !is_cancel_request && raw_clord != "*" {
                context.last_clord.insert(clord_id, raw_clord.clone());
            }
        }

        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let exec_id = parsed.get(&17).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        // Fixed-point: a fraction of a share is kept (ibx#313).
        let last_shares: Qty = parsed.get(&32).and_then(|s| parse_qty(s)).unwrap_or(0);
        let leaves_qty: Qty = parsed.get(&151).and_then(|s| parse_qty(s)).unwrap_or(0);
        let commission = parsed.get(&12).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

        if ord_status == "8" {
            log::warn!("ExecReport REJECTED: clord={} reason='{}' 103={}",
                clord_id,
                parsed.get(&58).map(|s| s.as_str()).unwrap_or("?"),
                parsed.get(&103).map(|s| s.as_str()).unwrap_or("?"));
        } else {
            log::info!("ExecReport: 39={} 150={} 11={} 58={} 103={}",
                ord_status, exec_type, clord_id,
                parsed.get(&58).map(|s| s.as_str()).unwrap_or(""),
                parsed.get(&103).map(|s| s.as_str()).unwrap_or(""));
        }

        // A bracket key on a report: kept for an order that has none, and
        // the next bracket group goes past it, as the reference (ibx#248).
        if let Some(key) = parsed.get(&6531).and_then(|k| crate::engine::bracket::BracketKey::parse(k)) {
            let parent = parent_order_id(parsed, context);
            if context.order(clord_id).is_some() {
                context.brackets().reported(clord_id, (parent > 0).then_some(parent), key);
            } else {
                context.bracket_groups = context.bracket_groups.max(key.group);
            }
        }

        // 39=0 is New on the wire, but the gateway reports PreSubmitted
        // until the order is actually routed to and acknowledged by an
        // exchange (for example a limit order resting pre-market). Routing
        // shows up on the same exec report as a non-empty ExDestination
        // (tag 100) plus an exec ref (tag 198) other than "NONE"; before
        // routing both are absent/"NONE". Captured in ib-agent#162 (ibx#210).
        let working = || {
            let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
                || parsed.get(&198).is_some_and(|s| s != "NONE" && !s.is_empty());
            if routed {
                crate::types::OrderStatus::Submitted
            } else {
                crate::types::OrderStatus::PreSubmitted
            }
        };
        let is_status_report = parsed.get(&20).map(|s| s.as_str()) == Some("3");
        let status = match ord_status {
            "0" => working(),
            // Replaced: back to working, by the same routing rule. The
            // reference reports no status change across a replace
            // (ib-agent#192 A1a: PreSubmitted before and after).
            "5" => working(),
            "A" => crate::types::OrderStatus::PreSubmitted,
            // Not a status the reference reports: it logs the frame and
            // keeps the order's status (ibx#472). A fill on the same frame
            // still counts.
            "E" | "I" => match context.order(clord_id) {
                Some(o) => o.status,
                None => {
                    log::info!("ExecReport: 39={} for order {} not tracked, no status change", ord_status, clord_id);
                    return;
                }
            },
            // Pending on a replace: the reference reports nothing, so keep
            // the current status. Reporting it as PendingCancel left the
            // order looking cancelled, since nothing moves it back (ibx#247).
            "6" if !is_cancel_request => match context.order(clord_id) {
                Some(o) => o.status,
                None => crate::types::OrderStatus::PendingReplace,
            },
            "6" => crate::types::OrderStatus::PendingCancel,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" => crate::types::OrderStatus::Cancelled,
            // Pending cancel: the order stays open until 39=4 or a fill
            // (ibx#472, a server-cancelled IOC sends 39=D then 39=4).
            "D" => crate::types::OrderStatus::PendingCancel,
            // A status report in the rejected state: the reference reads it
            // as cancelled, with no reject error. It answers the status
            // request sent after a refused cancel of an order the server no
            // longer has (ib-agent#192 C2b, ibx#252).
            "8" if is_status_report => crate::types::OrderStatus::Cancelled,
            "8" => crate::types::OrderStatus::Rejected,
            _ => {
                log::warn!("Unknown order status 39={} for order {}", ord_status, clord_id);
                return;
            }
        };

        // The guard's verdict (ibx#212): a stale frame the guard rejects
        // must not surface as an order_status. A frame that restates the
        // current status does: the reference reports every report of a known
        // order (ibx#473), except the statuses it reads as invalid (39=E,
        // 39=I, ibx#472).
        // The answer to a status request after a refused cancel or modify
        // sets the status, back to working too (ibx#252).
        // The replay after a reconnect answers a status request for every
        // working order: its status is set as the server gives it, as for
        // a single status request (ibx#251).
        let replayed = is_status_report && self.awaiting_status_replay;
        let queried = replayed || (is_status_report && !context.status_queries.is_empty()
            && context.status_queries.remove(&clord_id));
        let change = if queried {
            context.apply_queried_status(clord_id, status)
        } else {
            context.apply_order_status(clord_id, status)
        };
        let status_changed = change == crate::engine::context::StatusChange::Changed;
        let report_status = matches!(change,
            crate::engine::context::StatusChange::Changed | crate::engine::context::StatusChange::Same)
            && !matches!(ord_status, "E" | "I");

        // The reference reports a server reject as error 201 with the
        // server's reason (ib-agent#192 C1, ibx#250). Only on the first
        // transition of an order this session tracks: the "No such order"
        // frames that follow a cancel of an order already gone are not
        // shown by the reference (C2).
        if status == crate::types::OrderStatus::Rejected && status_changed {
            let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("");
            shared.orders.push_order_error(clord_id, 201, format!("Order rejected - reason:{}", reason));
        }
        // The limit offset, limit price and stop price the server reports
        // for a TRAIL LIMIT (6370, 44, 6117): the offset is restated on its
        // replace (ib-agent#194), the stop moves with the market (ibx#491).
        if context.order(clord_id).is_some() {
            let reported = |tag: u32| parsed.get(&tag).and_then(|s| s.parse::<f64>().ok())
                .map(|v| (v * PRICE_SCALE as f64).round() as i64);
            let stop = reported(6117);
            if let Some(offset) = reported(6370) {
                let previous_stop = context.trail_limit_reported.get(&clord_id).map_or(0, |r| r.stop);
                context.trail_limit_reported.insert(clord_id, crate::engine::context::TrailLimitReported {
                    offset,
                    limit: reported(44).unwrap_or(0),
                    stop: stop.unwrap_or(previous_stop),
                });
            } else if let (Some(stop), Some(r)) = (stop, context.trail_limit_reported.get_mut(&clord_id)) {
                r.stop = stop;
            }
        }
        // A cancel of an order of this session: error 202 with the server's
        // reason, before the Cancelled status, as the reference (ibx#465;
        // captured 25/09/2026, empty reason for a user cancel). Not for a
        // status report in the rejected state: the reference sets that one
        // cancelled without the notice (ibx#252).
        if status == crate::types::OrderStatus::Cancelled && status_changed && ord_status != "8" {
            let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("");
            shared.orders.push_order_error(clord_id, 202, format!("Order Canceled - reason:{}", reason));
        }
        // An order message of the server (6360 type, 6361 text): the
        // reference reports the TIME one as warning 399, once, and the order
        // keeps its status (ibx#465; captured 25/09/2026 on an OPG order:
        // "Order Message: BUY 1 AAPL NASDAQ.NMS Warning: your order will not
        // be placed at the exchange until ..."). The other types (PRICECAP
        // in the capture) did not reach the API.
        if parsed.get(&6360).map(|s| s.as_str()) == Some("TIME") && context.order(clord_id).is_some() {
            if let Some(text) = parsed.get(&6361).filter(|t| !t.is_empty()) {
                if self.order_messages_sent.insert((clord_id, text.clone())) {
                    let message = order_message_399(parsed, text, context, clord_id, shared);
                    shared.orders.push_order_error(clord_id, 399, message);
                }
            }
        }

        // The fill or status update of this frame, pushed once the order
        // cache below holds this frame's order state (ibx#473).
        let mut fill_out: Option<(Fill, crate::bridge::FillExec)> = None;
        let mut update_out: Option<crate::types::OrderUpdate> = None;
        let mut had_fill = false;
        let mut untracked_out: Option<(api::Execution, crate::bridge::FillExec)> = None;
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
            let tracked = context.order(clord_id).copied();
            // A duplicate execution is not booked again, but the report still
            // runs the order state below: status, order cache, and the end
            // of a terminal order (ibx#330). The id of an untracked fill is
            // recorded only once the fill is stored, so a fill that could not
            // be booked stays replayable (ibx#314).
            let duplicate = !exec_id.is_empty() && match tracked {
                Some(_) => !self.record_exec_id(exec_id),
                None => self.seen_exec_ids.contains(exec_id),
            };
            if !duplicate && !exec_id.is_empty() {
                // Last execution of the session, for the fill-up after a reconnect (ibx#399).
                let time = parsed.get(&60).or_else(|| parsed.get(&52)).cloned().unwrap_or_default();
                self.last_exec = Some((exec_id.to_string(), time));
            }
            if duplicate {
                log::warn!("Duplicate ExecID={}: fill not booked again", exec_id);
            } else if let Some(order) = tracked {
                context.update_order_filled_fixed(clord_id, last_shares);
                // Order totals ride on every fill report next to the print
                // (ib-agent#192 C3): the callback's filled and average price
                // are these, not the print (ibx#315).
                let cum_qty: Qty = parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(0);
                let avg_px = parsed.get(&6).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let fill = Fill {
                    instrument: order.instrument,
                    order_id: clord_id,
                    side: order.side,
                    price: (last_px * PRICE_SCALE as f64) as i64,
                    qty_fixed: last_shares,
                    remaining_fixed: leaves_qty,
                    cum_qty_fixed: cum_qty,
                    avg_price: (avg_px * PRICE_SCALE as f64).round() as i64,
                    commission: (commission * PRICE_SCALE as f64) as i64,
                    timestamp_ns: context.now_ns(),
                };
                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell | Side::ShortSell => -last_shares,
                };
                context.update_position_fixed(order.instrument, delta);
                // notify_fill inlined
                let exec = fill_exec_of(parsed, exec_id);
                shared.portfolio.set_position_fixed(fill.instrument, context.position_fixed(fill.instrument));
                if let Some(con_id) = context.market.con_id(order.instrument) {
                    self.record_exec_con_id(&exec.exec_id, con_id);
                    // The daily P&L counts the cash of this session's fills
                    // (sell positive, buy negative). Stock orders only, so
                    // no multiplier.
                    let shares = last_shares as f64 / QTY_SCALE as f64;
                    let cash = match order.side {
                        Side::Buy => -shares * last_px,
                        Side::Sell | Side::ShortSell => shares * last_px,
                    };
                    shared.portfolio.add_money_since_seed(con_id, cash);
                    // The position moves with the fill, as the reference's
                    // position store does; the server's average cost follows
                    // with its position feed.
                    shared.portfolio.apply_fill_to_position(con_id, delta, fill.price);
                }
                fill_out = Some((fill, exec));
                had_fill = true;
            } else {
                untracked_out = self.book_untracked_fill(
                    parsed, context, shared, clord_id, exec_id, last_px, last_shares, account_id);
                if untracked_out.is_some() && !exec_id.is_empty() {
                    self.record_exec_id(exec_id);
                }
            }
        }

        if report_status && !had_fill {
            if let Some(order) = context.order(clord_id).copied() {
                let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                let parent_id = parent_order_id(parsed, context);
                // Average fill price rides on status reports too (ibx#315).
                let avg_px = parsed.get(&6).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                // Filled so far as the server counts it (tag 14): an order
                // recovered at session start has no prints in this session.
                let filled = parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(order.filled_fixed);
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status: order.status,
                    filled_qty_fixed: filled,
                    remaining_qty_fixed: leaves_qty,
                    avg_fill_price: (avg_px * PRICE_SCALE as f64).round() as i64,
                    perm_id,
                    parent_id,
                    timestamp_ns: context.now_ns(),
                };
                update_out = Some(update);
            }
        }

        // Enrich order/contract caches block
        {
            let account = parsed.get(&1).cloned().unwrap_or_default();
            let symbol = parsed.get(&55).cloned().unwrap_or_default();
            let exchange = parsed.get(&207).cloned().unwrap_or_default();
            let sec_type = parsed.get(&167).cloned().unwrap_or_default();
            let currency = parsed.get(&15).cloned().unwrap_or_default();
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let local_symbol = parsed.get(&6035).cloned().unwrap_or_default();
            let _routing_exchange = parsed.get(&6004).cloned().unwrap_or_default();
            let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
            let total_qty: f64 = parsed.get(&38).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let ord_type_tag = parsed.get(&40).map(|s| s.as_str()).unwrap_or("");
            let limit_price: f64 = parsed.get(&44).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let stop_px: f64 = parsed.get(&99).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let outside_rth = parsed.get(&6433).map(|s| s == "1").unwrap_or(false);
            let clearing_intent = parsed.get(&6419).cloned().unwrap_or_default();
            let auto_cancel_date = parsed.get(&6596).cloned().unwrap_or_default();
            let exec_exchange = parsed.get(&30).cloned().unwrap_or_default();
            let transact_time = parsed.get(&60).cloned().unwrap_or_default();
            let avg_px: f64 = parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let cum_qty: f64 = parsed.get(&14).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let last_liq: i32 = parsed.get(&851).and_then(|s| s.parse().ok()).unwrap_or(0);

            let sec_type_str = match sec_type.as_str() {
                "CS" | "COMMON" => "STK",
                "FUT" => "FUT",
                "OPT" => "OPT",
                "FOR" | "CASH" => "CASH",
                "IND" => "IND",
                "FOP" => "FOP",
                "WAR" => "WAR",
                "BAG" => "BAG",
                "BOND" => "BOND",
                "CMDTY" => "CMDTY",
                "NEWS" => "NEWS",
                "FUND" => "FUND",
                _ => &sec_type,
            };

            let order_type_str = match ord_type_tag {
                "1" => "MKT", "2" => "LMT", "3" => "STP", "4" => "STP LMT",
                "P" => "TRAIL", "5" => "MOC", "B" => "LOC", "J" => "MIT",
                "K" => "MTL", "R" => "REL", _ => ord_type_tag,
            };

            // As the reference: an unknown code is kept ("???"), not read
            // as DAY; each report sets the order's time in force (ibx#307).
            let tif_str = decode_tif(super::report_tif(&parsed));

            let action = match parsed.get(&54).map(|s| s.as_str()) {
                Some("1") => "BUY",
                Some("2") => "SELL",
                Some("5") => "SSHORT",
                _ => if let Some(order) = context.order(clord_id) {
                    match order.side {
                        Side::Buy => "BUY",
                        Side::Sell => "SELL",
                        Side::ShortSell => "SSHORT",
                    }
                } else { "" },
            };

            let status_str = crate::client_core::order_status_str(status);

            let resolved_con_id = if con_id != 0 {
                con_id
            } else if let Some(order) = context.order(clord_id) {
                context.market.con_id(order.instrument).unwrap_or(0)
            } else {
                0
            };

            let contract = if resolved_con_id != 0 {
                if let Some(mut cached) = shared.reference.get_contract(resolved_con_id) {
                    if !symbol.is_empty() { cached.symbol = symbol.clone(); }
                    if !sec_type_str.is_empty() { cached.sec_type = sec_type_str.to_string(); }
                    if !exchange.is_empty() { cached.exchange = exchange.clone(); }
                    if !currency.is_empty() { cached.currency = currency.clone(); }
                    if !local_symbol.is_empty() { cached.local_symbol = local_symbol.clone(); }
                    cached
                } else {
                    api::Contract {
                        con_id: resolved_con_id,
                        symbol: symbol.clone(),
                        sec_type: sec_type_str.to_string(),
                        exchange: exchange.clone(),
                        currency: currency.clone(),
                        local_symbol: local_symbol.clone(),
                        ..Default::default()
                    }
                }
            } else {
                api::Contract {
                    symbol: symbol.clone(),
                    sec_type: sec_type_str.to_string(),
                    exchange: exchange.clone(),
                    currency: currency.clone(),
                    local_symbol: local_symbol.clone(),
                    ..Default::default()
                }
            };

            let (fb_action, fb_ord_type) = if let Some(ctx_order) = context.order(clord_id) {
                let a = match ctx_order.side {
                    crate::types::Side::Buy => "BUY",
                    crate::types::Side::Sell | crate::types::Side::ShortSell => "SELL",
                };
                let o = match ctx_order.ord_type {
                    b'1' => "MKT", b'2' => "LMT", b'3' => "STP", b'4' => "STP LMT",
                    b'P' => "TRAIL", _ => "",
                };
                (a, o)
            } else {
                ("", "")
            };

            // Derive 3 order-dependent fields from FIX tags
            let oca_type: i32 = match parsed.get(&6209).map(|s| s.as_str()) {
                Some("CancelOnFillWBlock") => 1,
                Some("ReduceOnFillWBlock") => 2,
                Some("ReduceOnFillNonBlock") => 3,
                Some("ReduceOnFillWBlockFromTotal") => 4,
                _ => 3, // default
            };
            let algo_strategy = parsed.get(&847).cloned().unwrap_or_default();
            // The price management flag the server echoes, 0 without it
            // (ibx#492).
            let use_price_mgmt_algo: i32 = i32::from(parsed.get(&8339).is_some_and(|v| v == "1"));
            let trail_stop_price: f64 = parsed.get(&6117)
                .and_then(|s| s.parse().ok())
                .unwrap_or(f64::MAX);

            // A TRAIL LIMIT report without its offset, limit price or stop
            // price keeps the last ones (ib-agent#194, ibx#491).
            let trail_limit = context.trail_limit_reported.get(&clord_id).copied();
            let limit_price = match trail_limit {
                Some(r) if limit_price == 0.0 && r.limit != 0 => r.limit as f64 / PRICE_SCALE as f64,
                _ => limit_price,
            };
            let trail_stop_price = match trail_limit {
                Some(r) if trail_stop_price == f64::MAX && r.stop != 0 => r.stop as f64 / PRICE_SCALE as f64,
                _ => trail_stop_price,
            };
            let order = api::Order {
                order_id: clord_id,
                action: if action.is_empty() { fb_action.to_string() } else { action.to_string() },
                total_quantity: total_qty,
                order_type: if order_type_str.is_empty() { fb_ord_type.to_string() } else { order_type_str.to_string() },
                lmt_price: limit_price,
                aux_price: stop_px,
                tif: tif_str.to_string(),
                account: if account.is_empty() { account_id.to_string() } else { account.clone() },
                perm_id,
                parent_id: parent_order_id(parsed, context),
                // Filled so far, not the quantity still working (ibx#309).
                filled_quantity: cum_qty,
                outside_rth,
                clearing_intent,
                auto_cancel_date,
                submitter: account_id.to_string(),
                oca_type,
                use_price_mgmt_algo,
                trail_stop_price,
                algo_strategy,
                // The orderRef the server echoes (ibx#466).
                order_ref: parsed.get(&6010).cloned().unwrap_or_default(),
                // A TRAIL LIMIT's offset as the server reports it (ib-agent#194).
                lmt_price_offset: trail_limit.map_or(f64::MAX, |r| r.offset as f64 / PRICE_SCALE as f64),
                ..Default::default()
            };

            let completed_time = if matches!(status,
                crate::types::OrderStatus::Filled |
                crate::types::OrderStatus::Cancelled |
                crate::types::OrderStatus::Rejected
            ) {
                parsed.get(&52).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            let completed_status = match status {
                crate::types::OrderStatus::Filled => "Filled".to_string(),
                crate::types::OrderStatus::Cancelled => "Cancelled".to_string(),
                crate::types::OrderStatus::Rejected => {
                    parsed.get(&58).cloned().unwrap_or_else(|| "Rejected".to_string())
                }
                _ => String::new(),
            };

            let order_state = api::OrderState {
                status: status_str.to_string(),
                commission_and_fees: commission,
                completed_time,
                completed_status,
                ..Default::default()
            };

            let last_exec = api::Execution {
                exec_id: exec_id.to_string(),
                time: transact_time,
                acct_number: account,
                exchange: exec_exchange,
                side: if let Some(o) = context.order(clord_id) {
                    match o.side { Side::Buy => "BOT", Side::Sell | Side::ShortSell => "SLD" }.to_string()
                } else { String::new() },
                shares: last_shares as f64,
                price: last_px,
                order_id: clord_id,
                cum_qty,
                avg_price: avg_px,
                last_liquidity: last_liq,
                ..Default::default()
            };

            if con_id != 0 {
                shared.reference.cache_contract(con_id, contract.clone());
            }
            if let Some((execution, exec)) = untracked_out.take() {
                shared.orders.push_untracked_execution(contract.clone(), execution, exec);
            }

            shared.orders.push_order_info(clord_id, RichOrderInfo {
                contract, order, order_state, last_exec,
            });
        }

        if let Some((fill, exec)) = fill_out {
            shared.orders.push_fill_with_exec(fill, exec);
            emit(event_tx, Event::Fill(fill));
        }
        if let Some(update) = update_out {
            shared.orders.push_order_update(update);
            emit(event_tx, Event::OrderUpdate(update));
        }

        if matches!(status,
            crate::types::OrderStatus::Filled |
            crate::types::OrderStatus::Cancelled |
            crate::types::OrderStatus::Rejected
        ) {
            if let Some(order) = context.order(clord_id).copied() {
                shared.orders.push_completed_order(CompletedOrder {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty_fixed: order.filled_fixed,
                    timestamp_ns: context.now_ns(),
                });
            }
            context.finish_order(clord_id, status);
        }
    }

    /// Book a fill of an order the engine does not track (ibx#314): an
    /// order of another client or of a previous session, one already ended
    /// here, or a fill the server replays at session start. The execution
    /// is kept for `req_executions` and moves the position; there is no live
    /// fill event, which needs a known order. `None` when the report has no
    /// contract or no side: a guessed side would move the position the
    /// wrong way.
    #[cold]
    #[allow(clippy::too_many_arguments)]
    fn book_untracked_fill(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        clord_id: OrderId,
        exec_id: &str,
        last_px: f64,
        last_shares: Qty,
        account_id: &str,
    ) -> Option<(api::Execution, crate::bridge::FillExec)> {
        let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
        let side = match parsed.get(&54).map(|s| s.as_str()) {
            Some("1") => Some(Side::Buy),
            Some("2") => Some(Side::Sell),
            Some("5") => Some(Side::ShortSell),
            _ => None,
        };
        let Some(side) = side.filter(|_| con_id != 0) else {
            log::warn!("Fill {} of untracked order {}: no contract or side, not booked", exec_id, clord_id);
            return None;
        };
        let delta = match side {
            Side::Buy => last_shares,
            Side::Sell | Side::ShortSell => -last_shares,
        };
        match context.market.try_register(con_id) {
            Some(instrument) => {
                shared.market.set_instrument_count(context.market.count());
                if let Some(sym) = parsed.get(&55) {
                    context.set_symbol(instrument, sym.clone());
                }
                context.update_position_fixed(instrument, delta);
                shared.portfolio.set_position_fixed(instrument, context.position_fixed(instrument));
            }
            None => log::error!("Fill {} of untracked order {}: instrument table full ({} contracts), engine position of con_id {} not moved",
                exec_id, clord_id, crate::types::MAX_INSTRUMENTS, con_id),
        }
        let shares = last_shares as f64 / QTY_SCALE as f64;
        self.record_exec_con_id(exec_id, con_id);
        let cash = match side {
            Side::Buy => -shares * last_px,
            Side::Sell | Side::ShortSell => shares * last_px,
        };
        shared.portfolio.add_money_since_seed(con_id, cash);
        shared.portfolio.apply_fill_to_position(con_id, delta, (last_px * PRICE_SCALE as f64) as i64);

        let execution = api::Execution {
            exec_id: exec_id.to_string(),
            acct_number: parsed.get(&1).filter(|s| !s.is_empty()).cloned()
                .unwrap_or_else(|| account_id.to_string()),
            side: match side { Side::Buy => "BOT", Side::Sell | Side::ShortSell => "SLD" }.to_string(),
            shares,
            price: last_px,
            perm_id: parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0),
            // The placing client's order id when the report carries it.
            order_id: parsed.get(&6121).and_then(|s| s.parse().ok()).unwrap_or(clord_id),
            cum_qty: parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(0) as f64 / QTY_SCALE as f64,
            avg_price: parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            last_liquidity: parsed.get(&851).and_then(|s| s.parse().ok()).unwrap_or(0),
            ..Default::default()
        };
        log::info!("Fill {} of untracked order {}: con_id={} {:?} {} @ {}, stored",
            exec_id, clord_id, con_id, side, shares, last_px);
        Some((execution, fill_exec_of(parsed, exec_id)))
    }

    /// A cancel or modify the server refused (ibx#252). As the reference:
    /// the order is the one whose current ClOrdID the reject carries (a
    /// reject of an older version changes nothing); its ClOrdID version
    /// goes back by one, its status and its record stay, and a status
    /// request for it is sent. The answer to that request sets the status.
    #[allow(clippy::too_many_arguments)]
    fn handle_cancel_reject(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("Cancel rejected");
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        let clord = parsed.get(&11).map(|s| s.as_str()).unwrap_or("");
        log::warn!("CancelReject: clord={} type={} code={} reason={}",
            clord, reject_type, reason_code, reason);

        let Some((oid, version)) = clord.split_once('.')
            .and_then(|(id, ver)| Some((id.parse::<OrderId>().ok()?, ver.parse::<u32>().ok()?)))
        else { return };
        if version == 0 || context.modify_versions.get(&oid) != Some(&version) {
            log::info!("CancelReject: {} is not the current version of order {}, ignored", clord, oid);
            return;
        }
        let lowered = format!("{}.{}", oid, version - 1);
        context.modify_versions.insert(oid, version - 1);
        // A refused modify had set the order's ClOrdID on record.
        if context.last_clord.get(&oid).map(String::as_str) == Some(clord) {
            context.last_clord.insert(oid, lowered.clone());
        }
        // The cancel is over; a later cancel sends a new id (ibx#464).
        if context.cancel_clord.get(&oid).map(String::as_str) == Some(clord) {
            context.cancel_clord.remove(&oid);
        }
        let instrument = match context.order(oid).copied() {
            Some(order) => {
                context.status_queries.insert(oid);
                order.instrument
            }
            None => 0,
        };
        if let Some(conn) = ccp_conn.as_mut() {
            let now = chrono_free_timestamp();
            match conn.send_fix(&order_status_request(&lowered, account_id, &now)) {
                Ok(()) => hb.last_ccp_sent = Instant::now(),
                Err(e) => log::warn!("CancelReject: status request for order {} not sent: {}", oid, e),
            }
        }

        let reject = crate::types::CancelReject {
            order_id: oid,
            instrument,
            reject_type,
            reason_code,
            timestamp_ns: context.now_ns(),
        };
        shared.orders.push_cancel_reject(reject);
        emit(event_tx, Event::CancelReject(reject));
    }

    /// A news bulletin, read as the reference does (ibx#461): its type
    /// mapped to the client type (types the client never gets are
    /// dropped), the server's message id (0 when absent), an empty message
    /// dropped; the store drops a repeated id.
    fn handle_news_bulletin(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        static BULLETIN_TYPE_MAP: &[(i32, i32)] = &[
            (1, 1), (2, 3), (3, 2), (8, 4), (9, 5), (10, 6),
        ];
        let fix_type: i32 = parsed.get(&fix::TAG_URGENCY)
            .and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let Some(api_type) = BULLETIN_TYPE_MAP.iter()
            .find(|(k, _)| *k == fix_type)
            .map(|(_, v)| *v) else { return };
        let message = parsed.get(&fix::TAG_HEADLINE).cloned().unwrap_or_default();
        if message.is_empty() {
            return;
        }
        let exchange = parsed.get(&fix::TAG_SECURITY_EXCHANGE).cloned().unwrap_or_default();
        let msg_id = parsed.get(&6143).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let bulletin = NewsBulletin { msg_id, msg_type: api_type, message, exchange };
        if !shared.market.push_news_bulletin(bulletin) {
            log::info!("News bulletin {} already received: dropped", msg_id);
        }
    }

    fn handle_account_summary(&mut self, parsed: &std::collections::HashMap<u32, String>, context: &mut Context, shared: &SharedState) {
        if let Some(val) = parsed.get(&9806).and_then(|s| s.parse::<f64>().ok()) {
            context.account.net_liquidation = (val * PRICE_SCALE as f64) as Price;
        }
        shared.portfolio.set_account(context.account());
    }

    /// Subscribe to the schedule paired with a secdef reply, joined on tag 6256.
    /// Internal subscription (no API-client req_id exposed); reply arrives as
    /// 35=U|6040=107 and is matched back to the secdef via 6256.
    fn send_schedule_subscribe(
        &mut self,
        join_key: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let sub_id = self.next_schedule_sub_id;
            self.next_schedule_sub_id += 1;
            let sub_id_str = format!("SchedSub.{}", sub_id);
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SUB_PROTOCOL,
                    crate::control::contracts::SUB_PROTOCOL_SCHEDULE_SUBSCRIBE),
                (320, &sub_id_str),
                (crate::control::contracts::TAG_SCHEDULE_JOIN_KEY, join_key),
            ]);
            hb.last_ccp_sent = Instant::now();
        }
    }

    /// Drop pending schedule pairs past their deadline, emitting partial details.
    /// Fail contract-details requests whose deadline has passed (ibx#227):
    /// both plain/by-symbol secdef lookups the gateway never answered and
    /// by-symbol fan-outs missing one or more per-exchange replies. On
    /// expiry the caller gets error 200 plus contract_details_end, so a
    /// blocked wait unblocks with no API change. Internal sentinel req_ids
    /// (>= 0xF000_0000: cache auto-fetch, scanner enrichment) are dropped
    /// silently — no user is waiting on them.
    pub(crate) fn sweep_contract_details(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if self.pending_secdef.is_empty() && self.pending_fanout.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<ReqId> = Vec::new();
        self.pending_secdef.retain(|(req_id, _, deadline)| {
            if now >= *deadline {
                if *req_id < 0xF000_0000 {
                    expired.push(*req_id);
                } else {
                    log::warn!("Internal secdef timeout: req_id={:#x}", req_id);
                }
                false
            } else {
                true
            }
        });
        let mut late: Vec<PendingFanout> = Vec::new();
        let mut i = 0;
        while i < self.pending_fanout.len() {
            if now >= self.pending_fanout[i].deadline {
                late.push(self.pending_fanout.swap_remove(i));
            } else {
                i += 1;
            }
        }
        self.pending_lookups.retain(|l| !expired.contains(&l.req_id));
        for fanout in late {
            log::warn!(
                "Contract-details fan-out timeout: api_req_id={} answered {} of {}",
                fanout.api_req_id, fanout.total - fanout.outstanding.len(), fanout.total,
            );
            if fanout.records.is_empty() {
                expired.push(fanout.api_req_id);
            } else {
                // The rows go out with the market rules known so far.
                self.deliver_contract_rows(fanout.api_req_id, fanout.records, shared, event_tx, ccp_conn, hb);
            }
        }
        for req_id in expired {
            log::warn!("Contract-details timeout: req_id={} — no gateway reply within {:?}",
                req_id, SECDEF_TIMEOUT);
            shared.reference.push_historical_error(
                req_id, 200,
                "contract details request timed out — no reply from the gateway".to_string(),
            );
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
    }

    /// Rows whose schedule did not come in time go out without it.
    pub(crate) fn sweep_pending_schedule_pairs(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let now = Instant::now();
        if !self.pending_schedule_pair.iter().any(|p| now >= p.deadline) {
            return;
        }
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_schedule_pair)
            .into_iter()
            .partition(|p| now >= p.deadline);
        self.pending_schedule_pair = waiting;
        let mut req_ids: Vec<ReqId> = Vec::new();
        for mut p in expired {
            log::warn!("Schedule pair timeout: api_req_id={} join_key={}", p.api_req_id, p.join_key);
            p.def.trading_hours = None;
            p.def.liquid_hours = None;
            p.def.time_zone_id = None;
            if !req_ids.contains(&p.api_req_id) {
                req_ids.push(p.api_req_id);
            }
            push_contract_row(shared, event_tx, p.api_req_id, p.def);
        }
        for req_id in req_ids {
            self.end_if_complete(req_id, shared, event_tx);
        }
    }

    /// Match a schedule reply to the rows waiting for its join key and
    /// emit them with the schedule merged. One reply serves every row that
    /// waits for that key.
    fn handle_schedule_reply(
        &mut self,
        msg: &[u8],
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        // The join key of the reply locates the waiting rows.
        let join_key = match extract_tag_value(msg, b"6256=") {
            Some(v) => v,
            None => return,
        };
        if !self.pending_schedule_pair.iter().any(|p| p.join_key == join_key) {
            return;
        }
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_schedule_pair)
            .into_iter()
            .partition(|p| p.join_key == join_key);
        self.pending_schedule_pair = waiting;
        let sched = crate::control::contracts::parse_schedule_response(msg);
        let mut req_ids: Vec<ReqId> = Vec::new();
        for mut pair in ready {
            if let Some(sched) = &sched {
                pair.def.time_zone_id = if sched.timezone.is_empty() {
                    None
                } else {
                    Some(sched.timezone.clone())
                };
                pair.def.trading_hours = Some(
                    crate::control::contracts::format_sessions_string(&sched.trading_hours)
                );
                pair.def.liquid_hours = Some(
                    crate::control::contracts::format_sessions_string(&sched.liquid_hours)
                );
            }
            if !req_ids.contains(&pair.api_req_id) {
                req_ids.push(pair.api_req_id);
            }
            push_contract_row(shared, event_tx, pair.api_req_id, pair.def);
        }
        for req_id in req_ids {
            self.end_if_complete(req_id, shared, event_tx);
        }
    }

    /// Send P&L subscribe: 6040=142 with 6529=PLR.{N}|1={account}|
    pub(crate) fn send_pnl_subscribe(
        &mut self,
        req_id: i64,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let pnl_payload = format!("PLR.{}|1={}|", req_id, account);
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "142"),
                (6529, &pnl_payload),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent P&L subscribe: req_id={} account={}", req_id, account);
        }
    }

    /// Account summary subscription (ibx#479), as the reference writes it:
    /// `6040=55|6036=1|6529={sr_id}|6374={tags}|6160={group}` to subscribe,
    /// `6036=0` with the same id to cancel (`subscribe` is `None`).
    pub(crate) fn send_account_summary(
        &mut self,
        sr_id: &str,
        subscribe: Option<(&str, &str)>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let Some(conn) = ccp_conn.as_mut() else { return };
        let ts = chrono_free_timestamp();
        let mut fields: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "55"),
            (6036, if subscribe.is_some() { "1" } else { "0" }),
            (6529, sr_id),
        ];
        if let Some((tags, group)) = subscribe {
            fields.push((6374, tags));
            fields.push((6160, group));
        }
        let _ = conn.send_fix(&fields);
        hb.last_ccp_sent = Instant::now();
        log::info!("Sent account summary {}: {}", if subscribe.is_some() { "subscribe" } else { "cancel" }, sr_id);
    }

    pub(crate) fn send_news_subscribe(
        &mut self,
        con_id: i64,
        instrument: InstrumentId,
        providers: &str,
        req_id: u32,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        self.news_subscriptions.push((instrument, req_id));
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let con_id_str = con_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (fix::TAG_SENDING_TIME, &ts),
                (263, "1"),
                (146, "1"),
                (262, &req_id_str),
                (6008, &con_id_str),
                (207, "NEWS"),
                (167, "CS"),
                (264, "292"),
                (6472, providers),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent news subscribe: con_id={} req_id={} providers={}", con_id, req_id, providers);
        }
    }

    pub(crate) fn send_news_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let req_id = match self.news_subscriptions.iter().position(|(id, _)| *id == instrument) {
            Some(pos) => {
                let (_, rid) = self.news_subscriptions.remove(pos);
                rid
            }
            None => return,
        };
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "2"),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent news unsubscribe: instrument={:?} req_id={}", instrument, req_id);
        }
    }

    pub(crate) fn send_secdef_request(&mut self, req_id: ReqId, con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let con_id_str = con_id.to_string();
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, &req_id_str),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (crate::control::contracts::TAG_IB_SOURCE, "Socket"),
            ]);
            log::info!("Sent secdef request: req_id={} con_id={}", req_id, con_id);
            hb.last_ccp_sent = Instant::now();
        } else {
            // No CCP socket: the entry still gets a deadline, so the caller
            // receives error 200 + end via the sweep instead of silence (ibx#227).
            log::warn!("secdef request req_id={} queued with no CCP socket", req_id);
        }
        // Known-conId lookup: single record, no paginated terminator.
        self.pending_secdef.push((req_id, true, Instant::now() + SECDEF_TIMEOUT));
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: ReqId, symbol: &str, sec_type: &str, exchange: &str, currency: &str, filters: &crate::types::SecDefFilters, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let strike = if filters.strike > 0.0 { format!("{}", filters.strike) } else { String::new() };
        let lookup = SymbolLookup {
            symbol: symbol.to_string(),
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            currency: currency.to_string(),
            filters: filters.clone(),
        };
        // A lookup with a strike that finds nothing is asked once more with
        // the strike divided by 100, as the reference (ibx#410).
        let retry_strike = (!strike.is_empty() && !lookup.is_identifier())
            .then(|| strike_divided_by_100(&strike));
        self.send_symbol_lookup(req_id, &lookup, &strike, ccp_conn, hb);
        self.pending_lookups.push(PendingLookup { req_id, lookup, retry_strike });
    }

    /// Send one by-symbol lookup with the strike text given (empty: none).
    fn send_symbol_lookup(&mut self, req_id: ReqId, lookup: &SymbolLookup, strike: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            send_symbol_lookup_on(conn, req_id, lookup, strike);
            log::info!("Sent secdef lookup: req_id={} symbol={} sec_type={} identifier={}",
                req_id, lookup.symbol, lookup.sec_type, lookup.is_identifier());
            hb.last_ccp_sent = Instant::now();
        } else {
            // See send_secdef_request: sweep converts this to a visible error.
            log::warn!("secdef-by-symbol request req_id={} queued with no CCP socket", req_id);
        }
        // By-symbol lookup: master reply carries `6046={exch_list}`. The
        // server never emits a 323=5/6 terminator; completion is detected
        // by counting per-exchange fan-out replies (see `pending_fanout`).
        self.pending_secdef.push((req_id, false, Instant::now() + SECDEF_TIMEOUT));
    }

    /// The strike retry of a lookup that found nothing (ibx#410): sent once,
    /// in place of the error. False when the lookup has no retry left.
    fn retry_with_divided_strike(&mut self, req_id: ReqId, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) -> bool {
        let Some(entry) = self.pending_lookups.iter_mut().find(|l| l.req_id == req_id) else { return false };
        let Some(strike) = entry.retry_strike.take() else { return false };
        let lookup = entry.lookup.clone();
        log::info!("Secdef lookup req_id={}: nothing found, retry with strike {}", req_id, strike);
        self.send_symbol_lookup(req_id, &lookup, &strike, ccp_conn, hb);
        true
    }

    /// No record for a pending lookup: its strike retry when one is left,
    /// else error 200 and no contract_details_end, as the reference
    /// (ibx#400, ibx#410). Internal lookups end silently.
    fn no_security_definition(
        &mut self,
        req_id: ReqId,
        shared: &SharedState,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if self.retry_with_divided_strike(req_id, ccp_conn, hb) {
            return;
        }
        self.take_lookup(req_id);
        push_not_found(req_id, shared);
    }

    /// The pending by-symbol lookup of `req_id`, removed.
    fn take_lookup(&mut self, req_id: ReqId) -> Option<SymbolLookup> {
        let pos = self.pending_lookups.iter().position(|l| l.req_id == req_id)?;
        Some(self.pending_lookups.swap_remove(pos).lookup)
    }

    /// A definition reply (ibx#435): one row per contract record of the
    /// reply to a lookup. The per-exchange replies of a fan-out only fill
    /// the market rules of the waiting records; they are never rows.
    fn handle_secdef_reply(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        use crate::control::contracts;
        let response_req_id = contracts::secdef_response_req_id(msg);
        // Asked for an order's outside RTH (ibx#465): not a user reply.
        if let Some(rid) = response_req_id.as_deref() {
            if super::order_builder::rth_definition_reply(context, rid, msg) { return; }
            // Asked for a round lot (ibx#287): not a user reply.
            if super::farm::round_lot_reply(context, rid, msg) { return; }
            // Asked for the conId of a market data request (ibx#278).
            if super::farm::md_contract_reply(context, shared, rid, msg) { return; }
            // Asked for the conId of a historical-data request (ibx#427).
            if self.contract_resolve_reply(rid, msg, shared) { return; }
            // Asked for the option of an option calculation (ibx#442).
            if self.optcalc.secdef_reply(rid, msg, ccp_conn, hb, shared) { return; }
        }
        let rules = contracts::parse_market_rules(msg);
        if !rules.is_empty() {
            shared.reference.push_market_rules(rules);
        }
        let records = contracts::parse_secdef_records(msg).unwrap_or_default();
        // A reply can list one conId once per exchange: the contract cache
        // keeps the first record of each conId, the one on the lookup's own
        // exchange, not the last listed exchange.
        for (i, def) in records.iter().enumerate() {
            if !records[..i].iter().any(|d| d.con_id == def.con_id) {
                self.cache_definition(def, shared);
            }
        }
        let Some(rid) = response_req_id else { return };

        let fanout_idx = self.pending_fanout.iter()
            .position(|p| p.outstanding.iter().any(|(id, _, _)| *id == rid));
        if let Some(idx) = fanout_idx {
            let fanout = &mut self.pending_fanout[idx];
            let pos = fanout.outstanding.iter().position(|(id, _, _)| *id == rid).unwrap_or(0);
            let (_, con_id, exchange) = fanout.outstanding.swap_remove(pos);
            fanout.deadline = Instant::now() + SECDEF_TIMEOUT;
            let done = fanout.outstanding.is_empty();
            if let Some(rule) = records.iter().find(|d| d.con_id == con_id).and_then(|d| d.market_rule_id) {
                self.market_rule_by_exchange.insert((con_id, exchange), rule);
            }
            if done {
                let fanout = self.pending_fanout.swap_remove(idx);
                self.deliver_contract_rows(fanout.api_req_id, fanout.records, shared, event_tx, ccp_conn, hb);
            }
            return;
        }

        // Match the response to its originating pending_secdef entry by the
        // echoed request id: an internal auto-fetch reply landing while a
        // user request is in flight must not leak onto the user's req_id.
        // A lookup's request id carries the name of the lookup (ibx#229).
        let Some(req_id) = contracts::secdef_request_number(&rid) else { return };
        let Some(idx) = self.pending_secdef.iter().position(|(pid, _, _)| *pid == req_id) else { return };
        let (_, single_shot, _) = self.pending_secdef.remove(idx);
        if records.is_empty() {
            // No record: never a conId 0 row (ibx#400).
            self.no_security_definition(req_id, shared, ccp_conn, hb);
            return;
        }
        let multiplier = self.take_lookup(req_id)
            .and_then(|l| l.filters.multiplier.parse::<f64>().ok());
        let mut records = records;
        // With several records, a requested multiplier keeps only the
        // records that have it, as the reference.
        if let (true, Some(m)) = (records.len() > 1, multiplier) {
            records.retain(|d| d.multiplier == m);
            if records.is_empty() {
                push_not_found(req_id, shared);
                return;
            }
        }
        for def in records.iter().filter(|d| d.con_id != 0) {
            if let Some(rule) = def.market_rule_id {
                self.market_rule_by_exchange.insert((def.con_id, def.exchange.clone()), rule);
            }
        }
        // Internal sentinel req_ids (auto-fetch for cold-cache positions,
        // scanner enrichment) start at 0xF000_0000. Their replies populate
        // the contract cache but never surface as contract_details callbacks.
        if req_id >= 0xF000_0000 {
            return;
        }
        if single_shot {
            // A lookup by conId asks for one contract, but its reply lists
            // that contract once per exchange: only the first record is a
            // row. The others still gave their market rules above.
            let mut seen: Vec<i64> = Vec::with_capacity(1);
            records.retain(|d| {
                let first = !seen.contains(&d.con_id);
                seen.push(d.con_id);
                first
            });
        }
        // By-symbol lookup: each record asks for its unknown per-exchange
        // market rules before it becomes a row.
        let by_symbol = !single_shot && !contracts::secdef_response_is_last(msg);
        let mut outstanding: Vec<(String, i64, String)> = Vec::new();
        if by_symbol {
            for def in records.iter().filter(|d| d.con_id != 0) {
                for exch in &def.valid_exchanges {
                    if matches!(exch.as_str(), "" | "SMART" | "BEST")
                        || self.market_rule_by_exchange.contains_key(&(def.con_id, exch.clone()))
                        || outstanding.iter().any(|(_, c, e)| *c == def.con_id && e == exch)
                    {
                        continue;
                    }
                    let fid = format!("ibxfan-{}-{}", req_id, self.next_fanout_id);
                    self.next_fanout_id = self.next_fanout_id.wrapping_add(1);
                    self.send_fanout_secdef_request(&fid, def.con_id, exch, ccp_conn, hb);
                    outstanding.push((fid, def.con_id, exch.clone()));
                }
            }
        }
        if outstanding.is_empty() {
            self.deliver_contract_rows(req_id, records, shared, event_tx, ccp_conn, hb);
        } else {
            log::info!(
                "Secdef by-symbol fan-out: api_req_id={} records={} requests={}",
                req_id, records.len(), outstanding.len(),
            );
            self.pending_fanout.push(PendingFanout {
                api_req_id: req_id,
                total: outstanding.len(),
                outstanding,
                records,
                deadline: Instant::now() + SECDEF_TIMEOUT,
            });
        }
    }

    /// Look up the contract of a historical-data request given without
    /// conId (ibx#427), with the by-symbol lookup of contract details. The
    /// request waits for the answer.
    pub(crate) fn start_contract_resolve(
        &mut self,
        req_id: ReqId,
        lookup: crate::types::ContractLookup,
        request: crate::types::ControlCommand,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // The request form of a contract lookup, with a number from a
        // range of its own: below the market data lookups and the internal
        // lookups, far above caller request ids, so no reply is taken from
        // another lookup.
        let lookup_id = HIST_LOOKUP_FIRST_ID + self.next_resolve_id % HIST_LOOKUP_IDS;
        self.next_resolve_id = self.next_resolve_id.wrapping_add(1);
        let symbol_lookup = SymbolLookup {
            symbol: lookup.symbol,
            sec_type: lookup.sec_type,
            exchange: lookup.exchange,
            currency: lookup.currency,
            filters: lookup.filters,
        };
        let strike = if symbol_lookup.filters.strike > 0.0 {
            format!("{}", symbol_lookup.filters.strike)
        } else {
            String::new()
        };
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            send_symbol_lookup_on(conn, ReqId::from(lookup_id), &symbol_lookup, &strike);
            hb.last_ccp_sent = Instant::now();
            log::info!("Contract lookup {} for historical req_id={}: symbol={} sec_type={}",
                lookup_id, req_id, symbol_lookup.symbol, symbol_lookup.sec_type);
        } else {
            log::warn!("Contract lookup {} for req_id={} queued with no auth connection", lookup_id, req_id);
        }
        self.pending_resolves.push(PendingResolve {
            lookup_id,
            req_id,
            request,
            deadline: Instant::now() + SECDEF_TIMEOUT,
        });
    }

    /// The answer to a contract lookup of a historical-data request
    /// (ibx#427): exactly one contract releases the request with its
    /// conId; none or several give error 200 and no query, as the
    /// reference. False when the reply is not for such a lookup.
    fn contract_resolve_reply(&mut self, lookup_id: &str, msg: &[u8], shared: &SharedState) -> bool {
        // The reply names the lookup as it was asked; its number is the key.
        let Some(number) = crate::control::contracts::secdef_request_number(lookup_id) else { return false };
        let Some(idx) = self.pending_resolves.iter().position(|p| ReqId::from(p.lookup_id) == number) else { return false };
        let pending = self.pending_resolves.swap_remove(idx);
        let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
        let mut con_ids: Vec<i64> = Vec::new();
        for def in &records {
            if def.con_id != 0 && !con_ids.contains(&def.con_id) {
                con_ids.push(def.con_id);
            }
        }
        if let [con_id] = con_ids.as_slice() {
            log::info!("Contract lookup {}: req_id={} con_id={}", lookup_id, pending.req_id, con_id);
            self.resolved_requests.push(request_with_con_id(pending.request, *con_id));
        } else {
            log::info!("Contract lookup {}: req_id={} found {} contracts", lookup_id, pending.req_id, con_ids.len());
            shared.reference.push_historical_error(pending.req_id, 200, NO_SECURITY_DEFINITION.to_string());
        }
        true
    }

    /// Contract lookups of historical-data requests with no answer in time
    /// (ibx#427): the request gets error 200 and no query.
    pub(crate) fn sweep_contract_resolves(&mut self, shared: &SharedState) {
        if self.pending_resolves.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<ReqId> = Vec::new();
        self.pending_resolves.retain(|p| {
            if now >= p.deadline {
                log::warn!("Contract lookup {} timeout: req_id={}", p.lookup_id, p.req_id);
                expired.push(p.req_id);
                false
            } else {
                true
            }
        });
        for req_id in expired {
            shared.reference.push_historical_error(
                req_id, 200,
                "contract details request timed out — no reply from the gateway".to_string(),
            );
        }
    }

    /// Contract cache entry of a definition, for every reply (rows or not).
    fn cache_definition(&mut self, def: &crate::control::contracts::ContractDefinition, shared: &SharedState) {
        if def.con_id == 0 {
            return;
        }
        shared.reference.cache_contract(def.con_id, api::Contract {
            con_id: def.con_id,
            symbol: def.symbol.clone(),
            sec_type: def.sec_type.to_api_str().to_string(),
            exchange: def.exchange.clone(),
            currency: def.currency.clone(),
            local_symbol: def.local_symbol.clone(),
            primary_exchange: def.primary_exchange.clone(),
            trading_class: def.trading_class.clone(),
            ..Default::default()
        });
        shared.reference.cache_market_name(def.con_id, &def.market_name);
        self.try_release_scanner_enrichments(def.con_id, shared);
    }

    /// The rows of a lookup, in record order (ibx#435): each record gets its
    /// market rule ids, and waits for its trading schedule when it has a
    /// join key (one schedule request per key). The end follows the last row.
    fn deliver_contract_rows(
        &mut self,
        req_id: ReqId,
        records: Vec<crate::control::contracts::ContractDefinition>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        let mut subscribed: Vec<String> = Vec::new();
        for mut def in records {
            def.market_rule_ids = self.market_rule_ids(&def);
            if def.join_key.is_empty() {
                push_contract_row(shared, event_tx, req_id, def);
            } else {
                if !subscribed.contains(&def.join_key) {
                    self.send_schedule_subscribe(&def.join_key, ccp_conn, hb);
                    subscribed.push(def.join_key.clone());
                }
                self.pending_schedule_pair.push(PendingSchedulePair {
                    api_req_id: req_id,
                    join_key: def.join_key.clone(),
                    def,
                    deadline,
                });
            }
        }
        self.end_if_complete(req_id, shared, event_tx);
    }

    /// contract_details_end once no row of the lookup waits for its schedule.
    fn end_if_complete(&self, req_id: ReqId, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        if !self.pending_schedule_pair.iter().any(|p| p.api_req_id == req_id) {
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
    }

    /// Market rule id of each valid exchange of a record whose rule is
    /// known, comma-joined (ibx#435).
    fn market_rule_ids(&self, def: &crate::control::contracts::ContractDefinition) -> String {
        let mut ids = String::new();
        for exch in &def.valid_exchanges {
            if let Some(id) = self.market_rule_by_exchange.get(&(def.con_id, exch.clone())) {
                if !ids.is_empty() {
                    ids.push(',');
                }
                ids.push_str(&id.to_string());
            }
        }
        ids
    }

    /// Send a per-exchange fan-out request after a by-symbol master reply.
    /// Wire: `35=c|320={fanout_id}|321=2|146=1|6008={conid}|6004={exch}|`
    pub(crate) fn send_fanout_secdef_request(
        &mut self,
        fanout_req_id: &str,
        con_id: i64,
        exchange: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let con_id_str = con_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, fanout_req_id),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (146, "1"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (6004, exchange),
            ]);
            hb.last_ccp_sent = Instant::now();
        }
    }

    /// A matching-symbols request, as the reference sends it (ibx#369):
    /// with an own request id; refused at once with 10159 when the auth
    /// link is down or the send fails, and then not kept.
    pub(crate) fn send_matching_symbols_request(
        &mut self,
        req_id: ReqId,
        pattern: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        let wire_id = self.next_matching_symbols_id;
        self.next_matching_symbols_id = self.next_matching_symbols_id.wrapping_add(1).max(1);
        let sent = match ccp_conn.as_mut().filter(|_| !self.disconnected) {
            None => false,
            Some(conn) => {
                let wire_id_str = wire_id.to_string();
                let ts = chrono_free_timestamp();
                let result = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, "U"),
                    (fix::TAG_SENDING_TIME, &ts),
                    (6040, "185"),
                    (320, &wire_id_str),
                    (58, pattern),
                ]);
                hb.last_ccp_sent = Instant::now();
                result.is_ok()
            }
        };
        if sent {
            log::info!("Sent matching symbols request: req_id={} id={} pattern='{}'", req_id, wire_id, pattern);
            self.pending_matching_symbols.push((wire_id, req_id));
        } else {
            log::warn!("Matching symbols request {} not sent: auth connection down", req_id);
            shared.reference.push_historical_error(req_id, 10159, MATCHING_SYMBOLS_SEND_FAILED.to_string());
        }
    }

    pub(crate) fn handle_disconnect(&mut self, _context: &mut Context, _event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        // Matching-symbols requests end without an answer and are not sent
        // again, as in the reference (ibx#369).
        self.pending_matching_symbols.clear();
        self.awaiting_status_replay = false;
        self.status_replay_end_at = None;
        // The orders keep their status, as in the reference: the clients get
        // the lost-link message only, and the replay after the new logon
        // corrects each order (ibx#251).
        // Don't emit Event::Disconnected — auto-reconnect handles CCP drops transparently.
        // Python is only notified if reconnect exhausts retries.
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        *ccp_conn = Some(conn);
        self.disconnected = false;
        hb.ccp_connected(Instant::now());

        if let Some(conn) = ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();

            // Re-subscribe to account/position data so server pushes fresh UP/UT/UM messages.
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "91"), (1, account_id), (6556, "DR.1"), (6712, "1"),
            ]);
            // The fills of the gap come only in the answer to this request
            // (ibx#399); they take the normal report path.
            let fill_up = self.fill_up_request(account_id, &ts);
            let fill_up: Vec<(u32, &str)> = fill_up.iter().map(|(t, v)| (*t, v.as_str())).collect();
            let _ = conn.send_fix(&fill_up);
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "6"), (6036, "1"), (6095, account_id), (6529, "AR.3"),
            ]);
            // Status of the working orders; its end frame starts the
            // restored-link report.
            let _ = conn.send_fix(&status_replay_request(&ts));
            self.awaiting_status_replay = true;
            self.status_replay_end_at = None;

            hb.last_ccp_sent = Instant::now();
            log::info!("CCP reconnected, sent account re-subscribe, fill-up and order status requests");
        }
    }

    /// The trades request after a reconnect (ibx#399): the fills since the
    /// last execution of the session, as the reference asks for them; with
    /// no execution yet, the fills of the day.
    pub(crate) fn fill_up_request(&mut self, account_id: &str, now: &str) -> Vec<(u32, String)> {
        let n = self.next_trades_request;
        self.next_trades_request += 1;
        let mut fields: Vec<(u32, String)> = vec![
            (fix::TAG_MSG_TYPE, "U".into()),
            (fix::TAG_SENDING_TIME, now.into()),
            (6040, "72".into()),
        ];
        match &self.last_exec {
            Some((exec_id, time)) => {
                // The reference marks an execution whose commission it has.
                let (base, _) = split_exec_revision(exec_id);
                let exec_ref = if self.commission_revisions.contains_key(base) {
                    format!("{}-CM", exec_id)
                } else {
                    exec_id.clone()
                };
                fields.extend([
                    (6536, time.clone()),
                    (6537, now.into()),
                    (6556, format!("todayfillup{}", n)),
                    (6538, "1".into()),
                    (1, account_id.into()),
                    (6539, time.clone()),
                    (17, exec_ref),
                ]);
            }
            None => {
                let day_start = format!("{}-00:00:00", now.get(..8).unwrap_or(now));
                fields.extend([
                    (6536, day_start),
                    (6537, now.into()),
                    (6556, format!("today{}", n)),
                ]);
            }
        }
        fields
    }
}

/// The status request for one order, after the server refused its cancel
/// or modify (ibx#252): the order's ClOrdID, any symbol and side, and the
/// account, as the reference writes it.
/// The API status of a what-if reply, from its order status (ibx#462):
/// 39=A is PreSubmitted (captured 02/10/2026), New and Replaced follow the
/// routing rule of an order's reports, a reject is Inactive.
fn what_if_status(parsed: &std::collections::HashMap<u32, String>) -> &'static str {
    let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
        || parsed.get(&198).is_some_and(|s| s != "NONE" && !s.is_empty());
    match parsed.get(&39).map(String::as_str) {
        Some("0" | "5") if routed => "Submitted",
        Some("8") => "Inactive",
        Some("4" | "C") => "Cancelled",
        _ => "PreSubmitted",
    }
}

pub(crate) fn order_status_request<'a>(clord: &'a str, account_id: &'a str, now: &'a str) -> [(u32, &'a str); 7] {
    [(fix::TAG_MSG_TYPE, "H"), (fix::TAG_SENDING_TIME, now), (11, clord), (55, "*"), (54, "*"), (6471, "1"), (1, account_id)]
}

/// The order status replay request: every working order (ibx#399).
pub(crate) fn status_replay_request(now: &str) -> [(u32, &str); 5] {
    [(fix::TAG_MSG_TYPE, "H"), (fix::TAG_SENDING_TIME, now), (11, "*"), (55, "*"), (54, "*")]
}

/// Handle account update messages (cross-cutting, called from CCP message processing).
pub(crate) fn handle_account_update(msg: &[u8], context: &mut Context, shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    // Rows of an account summary subscription go to that request only, not
    // to the account state (ibx#479).
    if let Some(sr_id) = summary_id(text) {
        let (rows, _) = parse_account_rows(text);
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: sr_id.to_string(),
            rows: rows.into_iter()
                .map(|(key, currency, value)| crate::bridge::AccountRow { key, value, currency, ledger: false })
                .collect(),
            ledger: text.split(SOH_CHAR).any(|p| p == "35=RL"),
            end: false,
        });
        return;
    }
    let mut key: Option<&str> = None;
    for part in text.split('\x01') {
        if let Some(val) = part.strip_prefix("8001=") {
            key = Some(val);
        } else if let Some(val) = part.strip_prefix("8004=") {
            if let Some(k) = key {
                match k {
                    "NetLiquidation" => { if let Ok(v) = val.parse::<f64>() { context.account.net_liquidation = (v * PRICE_SCALE as f64) as Price; } }
                    "BuyingPower" => { if let Ok(v) = val.parse::<f64>() { context.account.buying_power = (v * PRICE_SCALE as f64) as Price; } }
                    "MaintMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.margin_used = (v * PRICE_SCALE as f64) as Price; } }
                    "UnrealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.unrealized_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    "RealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.realized_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    "TotalCashValue" => { if let Ok(v) = val.parse::<f64>() { context.account.total_cash_value = (v * PRICE_SCALE as f64) as Price; } }
                    "SettledCash" => { if let Ok(v) = val.parse::<f64>() { context.account.settled_cash = (v * PRICE_SCALE as f64) as Price; } }
                    "AccruedCash" => { if let Ok(v) = val.parse::<f64>() { context.account.accrued_cash = (v * PRICE_SCALE as f64) as Price; } }
                    "EquityWithLoanValue" => { if let Ok(v) = val.parse::<f64>() { context.account.equity_with_loan = (v * PRICE_SCALE as f64) as Price; } }
                    "GrossPositionValue" => { if let Ok(v) = val.parse::<f64>() { context.account.gross_position_value = (v * PRICE_SCALE as f64) as Price; } }
                    "InitMarginReq" | "FullInitMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.init_margin_req = (v * PRICE_SCALE as f64) as Price; } }
                    "FullMaintMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.maint_margin_req = (v * PRICE_SCALE as f64) as Price; } }
                    "AvailableFunds" | "FullAvailableFunds" => { if let Ok(v) = val.parse::<f64>() { context.account.available_funds = (v * PRICE_SCALE as f64) as Price; } }
                    "ExcessLiquidity" | "FullExcessLiquidity" => { if let Ok(v) = val.parse::<f64>() { context.account.excess_liquidity = (v * PRICE_SCALE as f64) as Price; } }
                    "Cushion" => { if let Ok(v) = val.parse::<f64>() { context.account.cushion = (v * PRICE_SCALE as f64) as Price; } }
                    "SMA" => { if let Ok(v) = val.parse::<f64>() { context.account.sma = (v * PRICE_SCALE as f64) as Price; } }
                    "DayTradesRemaining" => { if let Ok(v) = val.parse::<i64>() { context.account.day_trades_remaining = v; } }
                    "Leverage-S" | "Leverage" => { if let Ok(v) = val.parse::<f64>() { context.account.leverage = (v * PRICE_SCALE as f64) as Price; } }
                    "DailyPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.daily_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    _ => {}
                }
                key = None;
            }
        }
    }
    shared.portfolio.set_account(context.account());
    // The account stream's values as sent, for update_account_value (ibx#475).
    if is_account_stream(text) {
        let (rows, time_secs) = parse_account_rows(text);
        let ledger = text.split(SOH_CHAR).any(|p| p == "35=RL");
        shared.portfolio.update_account_rows(|store| {
            for (key, currency, value) in &rows {
                store.set_row(key, currency, value, ledger);
            }
            if let Some(t) = time_secs {
                store.time_secs = store.time_secs.max(t);
            }
        });
    }
}

/// The account stream's frames carry `6529=AR.{n}`; other subscriptions
/// (account summary, `SR.{n}`) have their own id.
/// The subscription id of an account summary frame (`6529=SR.{...}`).
fn summary_id(text: &str) -> Option<&str> {
    text.split(SOH_CHAR).find_map(|p| p.strip_prefix("6529=")).filter(|id| id.starts_with("SR."))
}

const SOH_CHAR: char = '\x01';

fn is_account_stream(text: &str) -> bool {
    text.split('\x01').any(|p| p.starts_with("6529=AR."))
}

/// End marker of the account stream (`35=EB|6529=AR.{n}`): the first full
/// image is in (ibx#475). The periodic batches carry none (captured
/// 25/09/2026).
fn handle_account_end(msg: &[u8], shared: &SharedState) {
    let Ok(text) = std::str::from_utf8(msg) else { return };
    // The end of an account summary batch (ibx#479).
    if let Some(sr_id) = summary_id(text) {
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: sr_id.to_string(), rows: Vec::new(), ledger: false, end: true,
        });
        return;
    }
    if is_account_stream(text) {
        shared.portfolio.update_account_rows(|store| {
            if !store.image_complete {
                store.image_complete = true;
                store.generation += 1;
            }
        });
    }
}

/// Ledger tags of a `35=RL` row and the account keys the reference gives
/// them (ibx#475). The row's currency (8002) is the key's currency.
const LEDGER_KEYS: &[(&str, &str)] = &[
    ("9806", "CashBalance"),
    ("9818", "TotalCashBalance"),
    ("6242", "AccruedCash"),
    ("9807", "StockMarketValue"),
    ("9808", "OptionMarketValue"),
    ("9809", "FutureOptionValue"),
    ("9810", "FuturesPNL"),
    ("9819", "NetLiquidationByCurrency"),
    ("6100", "UnrealizedPnL"),
    ("6099", "RealizedPnL"),
    ("9820", "ExchangeRate"),
];

/// Rows of an account frame as (key, currency, value text), and the latest
/// row time (6066), as the reference reads them (ibx#475):
/// - `35=UT` / `35=UM`: each `8001` key with its `8004` value, currency
///   from tag 15 (UT rows have none). A row with no value is skipped. A
///   `PNL` row ends the frame. `AddAccountCode` is skipped, and
///   `AccountCode` is kept only after an `AccountType` row of the same frame
///   (the periodic frames carry an empty one).
/// - `35=RL`: each `LedgerList` row gives `Currency` and the ledger keys,
///   with the row's currency (8002).
pub(crate) fn parse_account_rows(text: &str) -> (Vec<(String, String, String)>, Option<i64>) {
    let mut rows = Vec::new();
    let mut time: Option<i64> = None;
    let ledger = text.split('\x01').any(|p| p == "35=RL");
    // Current row: key, currency, value, ledger values.
    let mut key: Option<&str> = None;
    let mut currency = "";
    let mut value: Option<&str> = None;
    let mut ledger_values: Vec<(&str, &str)> = Vec::new();
    let mut seen_account_type = false;

    let flush = |key: Option<&str>, currency: &str, value: Option<&str>,
                     ledger_values: &mut Vec<(&str, &str)>, rows: &mut Vec<(String, String, String)>,
                     seen_account_type: &mut bool| -> bool {
        let Some(k) = key else { return true };
        if ledger {
            if k == "LedgerList" && !currency.is_empty() {
                rows.push(("Currency".into(), currency.into(), currency.into()));
                for (tag, name) in LEDGER_KEYS {
                    if let Some((_, v)) = ledger_values.iter().find(|(t, _)| t == tag) {
                        rows.push((name.to_string(), currency.into(), v.to_string()));
                    }
                }
            }
            ledger_values.clear();
            return true;
        }
        match k {
            "PNL" => return false,
            "AddAccountCode" => {}
            "AccountCode" if !*seen_account_type => {}
            _ => {
                if k == "AccountType" {
                    *seen_account_type = true;
                }
                if let Some(v) = value {
                    rows.push((k.into(), currency.into(), v.into()));
                }
            }
        }
        true
    };

    for part in text.split('\x01') {
        let Some((tag, val)) = part.split_once('=') else { continue };
        match tag {
            "8001" => {
                if !flush(key, currency, value, &mut ledger_values, &mut rows, &mut seen_account_type) {
                    return (rows, time);
                }
                key = Some(val);
                currency = "";
                value = None;
            }
            "8002" if ledger => currency = val,
            "15" if !ledger => currency = val,
            "8004" => value = Some(val),
            "6066" => {
                if let Ok(t) = val.parse::<i64>() {
                    time = Some(time.map_or(t, |p: i64| p.max(t)));
                }
            }
            _ if ledger && key.is_some() => ledger_values.push((tag, val)),
            _ => {}
        }
    }
    flush(key, currency, value, &mut ledger_values, &mut rows, &mut seen_account_type);
    (rows, time)
}

/// Handle 6040=143 P&L midnight seed response.
/// Repeating group: 146={count} × (6008=conId, 6064=qtyMidnight, 6822=moneyTraded, 6099=realizedPnl).
/// These are midnight seeds for client-side daily P&L computation — NOT live P&L values.
fn handle_pnl_response(msg: &[u8], shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut seeds = Vec::new();
    let mut con_id: i64 = 0;
    let mut qty_midnight: Qty = 0;
    let mut money_traded: f64 = 0.0;
    let mut realized_pnl: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            if count > 0 && con_id != 0 {
                seeds.push(MidnightSeed { con_id, qty_midnight_fixed: qty_midnight, money_traded, realized_pnl });
            }
            con_id = v.parse().unwrap_or(0);
            qty_midnight = 0;
            money_traded = 0.0;
            realized_pnl = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty_midnight = parse_qty(v).unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("6822=") {
            // moneyTradedSinceMidnight: signed net cash, SELL positive / BUY
            // negative. Stored with the wire sign; poll_pnl adds it (ib-agent#163).
            money_traded = v.parse().unwrap_or(0.0);
        } else if let Some(v) = part.strip_prefix("6099=") {
            realized_pnl = v.parse().unwrap_or(0.0);
        }
    }
    if count > 0 && con_id != 0 {
        seeds.push(MidnightSeed { con_id, qty_midnight_fixed: qty_midnight, money_traded, realized_pnl });
    }
    shared.portfolio.set_midnight_seeds(seeds);
}

/// Handle 6040=75 position + market price feed.
/// Fires at init and after each fill. Contains repeating group: 146=count × (6008=conId, 6064=qty, 6101=avgCost).
/// The wire only carries conId/qty/avgCost — no symbol/secType. For any held conId not yet in the
/// reference cache, we issue an internal secdef request so the wrapper-facing Contract is populated
/// by the time `req_positions` is called (#154).
impl CcpState {
    pub(crate) fn handle_position_feed(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    // Parse repeating group by scanning for 6008= boundaries
    let mut con_id: i64 = 0;
    let mut qty: Qty = 0;
    let mut avg_cost_raw: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            // Flush previous position if any
            if count > 0 && con_id != 0 {
                let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
                shared.portfolio.set_position_info(PositionInfo {
                    con_id, position_fixed: qty, avg_cost, ..Default::default()
                });
                if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                    shared.portfolio.set_position_fixed(instrument, qty);
                    emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
                }
                self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
            }
            con_id = v.parse().unwrap_or(0);
            qty = 0;
            avg_cost_raw = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty = parse_qty(v).unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("6101=") {
            avg_cost_raw = v.parse().unwrap_or(0.0);
        }
    }
    // Flush last position
    if count > 0 && con_id != 0 {
        let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position_fixed: qty, avg_cost, ..Default::default()
        });
        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            shared.portfolio.set_position_fixed(instrument, qty);
            emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
        }
        self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
    }
    }

    /// Issue an internal secdef request for `con_id` if the reference cache is cold and we
    /// haven't already auto-fetched it this session. The reply path populates the cache via
    /// the existing 35=d handler; we don't track the response.
    fn auto_fetch_secdef_if_cold(
        &mut self,
        con_id: i64,
        ccp_conn: &mut Option<Connection>,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        if con_id == 0 { return; }
        if self.auto_fetched_conids.contains(&con_id) { return; }
        if shared.reference.get_contract(con_id).is_some() { return; }
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        self.auto_fetched_conids.insert(con_id);
        self.send_secdef_request(ReqId::from(req_id), con_id, ccp_conn, hb);
    }

    /// Park a scanner result and dispatch concurrent secdef requests for every cache-miss
    /// con_id. Once all replies arrive (via `try_release_scanner_enrichments`) the result
    /// is pushed to the dispatch queue with the now-warm cache. Mirrors what the gateway
    /// does internally for binary-API scanner clients.
    pub(crate) fn start_scanner_enrichment(
        &mut self,
        api_req_id: ReqId,
        result: crate::control::scanner::ScannerResult,
        ccp_conn: &mut Option<Connection>,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        let mut awaiting: HashSet<i64> = HashSet::new();
        for entry in &result.entries {
            let con_id = entry.con_id;
            if con_id == 0 { continue; }
            if shared.reference.get_contract(con_id).is_some() { continue; }
            awaiting.insert(con_id);
        }
        if awaiting.is_empty() {
            shared.reference.push_scanner_data(api_req_id, result);
            return;
        }
        // Issue one secdef request per cold con_id. If another flow has already
        // requested the same con_id (auto_fetched_conids contains it) we skip
        // the send but still wait — its reply will populate the cache and
        // release this entry via try_release_scanner_enrichments.
        for &con_id in &awaiting {
            if !self.auto_fetched_conids.contains(&con_id) {
                let req_id = self.next_internal_secdef_id;
                self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
                self.auto_fetched_conids.insert(con_id);
                self.send_secdef_request(ReqId::from(req_id), con_id, ccp_conn, hb);
            }
        }
        self.pending_scanner_enrichment.push(PendingScannerEnrichment {
            api_req_id,
            result,
            awaiting,
            deadline: Instant::now() + Duration::from_secs(5),
        });
    }

    /// Called from the 35=d reply path after the contract cache has been
    /// populated for `con_id`. Removes `con_id` from any pending scanner
    /// enrichment's awaiting set; entries whose set becomes empty are
    /// dispatched to the scanner_data queue.
    pub(crate) fn try_release_scanner_enrichments(&mut self, con_id: i64, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        let mut idx = 0;
        while idx < self.pending_scanner_enrichment.len() {
            self.pending_scanner_enrichment[idx].awaiting.remove(&con_id);
            if self.pending_scanner_enrichment[idx].awaiting.is_empty() {
                let pe = self.pending_scanner_enrichment.swap_remove(idx);
                shared.reference.push_scanner_data(pe.api_req_id, pe.result);
            } else {
                idx += 1;
            }
        }
    }

    /// Flush scanner enrichments past their deadline, dispatching whatever
    /// entries we have (some may still have blank fields if the secdef reply
    /// never arrived). Prevents indefinite hangs on a missing reply.
    pub(crate) fn sweep_scanner_enrichments(&mut self, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        let now = Instant::now();
        let mut idx = 0;
        while idx < self.pending_scanner_enrichment.len() {
            if self.pending_scanner_enrichment[idx].deadline <= now {
                let pe = self.pending_scanner_enrichment.swap_remove(idx);
                log::warn!(
                    "scanner enrichment timeout: req_id={} missing={} con_ids; dispatching partial",
                    pe.api_req_id,
                    pe.awaiting.len(),
                );
                shared.reference.push_scanner_data(pe.api_req_id, pe.result);
            } else {
                idx += 1;
            }
        }
    }
}

/// Handle position update messages (cross-cutting, called from CCP message processing).
/// A portfolio message carries one row per position, each row starting with
/// the symbol field and repeating the same fields (ib-agent#192 D4:
/// most messages carry 2 or 3 rows). Parsing the whole message into one map
/// kept only the last row (ibx#411). A message with no symbol field is one row.
pub(crate) fn handle_portfolio_message(
    msg: &[u8],
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<Sender<Event>>,
) {
    let mut header = std::collections::HashMap::new();
    let mut rows: Vec<std::collections::HashMap<u32, String>> = Vec::new();
    for field in msg.split(|&b| b == fix::SOH) {
        let Some(eq) = field.iter().position(|&b| b == b'=') else { continue };
        let Some(tag) = std::str::from_utf8(&field[..eq]).ok().and_then(|t| t.parse::<u32>().ok()) else { continue };
        let value = String::from_utf8_lossy(&field[eq + 1..]).into_owned();
        if tag == 6068 {
            rows.push(std::collections::HashMap::new());
        }
        match rows.last_mut() {
            Some(row) => { row.insert(tag, value); }
            None => { header.insert(tag, value); }
        }
    }
    if rows.is_empty() {
        rows.push(header);
    }
    for row in &rows {
        handle_position_update(row, context, shared, event_tx);
    }
}

pub(crate) fn handle_position_update(
    parsed: &std::collections::HashMap<u32, String>,
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<Sender<Event>>,
) {
    let con_id: i64 = match parsed.get(&6008).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    // Fixed-point: a fractional position is kept (ibx#313).
    let position: Qty = parsed.get(&6064).and_then(|s| parse_qty(s)).unwrap_or(0);
    // Tag map verified against the updatePortfolio callback (ib-agent#172):
    // 6101 = averageCost, 6065 = marketPrice (per share), 6067 = marketValue,
    // 6100 = unrealizedPNL, 6099 = realizedPNL. Earlier code read 6065 as the
    // average cost, which is actually the market price.
    let price_tag = |tag: u32| parsed.get(&tag)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| (v * PRICE_SCALE as f64) as Price)
        .unwrap_or(0);
    let avg_cost: Price = price_tag(6101);
    let market_price: Price = price_tag(6065);
    let market_value: Price = price_tag(6067);
    let unrealized_pnl: Price = price_tag(6100);
    let realized_pnl: Price = price_tag(6099);
    // Symbol arrives space-padded; trim trailing whitespace.
    let symbol = parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default();
    let sec_type = parsed.get(&167).cloned().unwrap_or_default();
    let currency = parsed.get(&15).cloned().unwrap_or_default();
    // Only the multiplier part of the row key; the whole key used to be
    // reported as the contract's multiplier.
    let multiplier = parsed.get(&8002)
        .and_then(|key| {
            let parts: Vec<&str> = key.split('/').collect();
            (parts.len() == 4).then(|| parts[2].to_string())
        })
        .unwrap_or_default();

    // Always store position info for reqPositions/pnlSingle, regardless of instrument registry.
    shared.portfolio.set_position_info(PositionInfo {
        con_id, position_fixed: position, avg_cost,
        symbol, sec_type, currency, multiplier,
        ..Default::default()
    });
    shared.portfolio.set_position_marks(con_id, market_price, market_value, unrealized_pnl, realized_pnl);

    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
        let current = context.position_fixed(instrument);
        let delta = position - current;
        if delta != 0 {
            context.update_position_fixed(instrument, delta);
        }
        shared.portfolio.set_position_fixed(instrument, position);
        emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: position, avg_cost });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression for ibx#198: the fill-dedup set must NOT be wiped wholesale
    // when it reaches its cap. A recently-seen ExecID has to stay deduplicated
    // so a post-reconnect server replay can't double-count the fill.
    /// The commission frame captured 25/09/2026, 25 ms after a BUY 100 AAPL
    /// fill that carried no commission (ibx#471).
    fn commission_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m: std::collections::HashMap<u32, String> = [
            (35u32, "U"), (6040, "60"), (17, "0000e0d5.6ab5f36f.01.01"), (37, "1"),
            (6381, "USD"), (6378, "1.0003"), (6099, "0"),
        ].iter().map(|(t, v)| (*t, v.to_string())).collect();
        for (tag, val) in pairs {
            m.insert(*tag, val.to_string());
        }
        m
    }

    #[test]
    fn commission_frame_gives_the_commission_report() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[]), &shared);
        let reports = shared.orders.drain_commission_reports();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.exec_id, "0000e0d5.6ab5f36f.01.01");
        assert_eq!(r.commission_and_fees, 1.0003);
        assert_eq!(r.currency, "USD");
        assert_eq!(r.realized_pnl, f64::MAX, "a realized P&L of 0 is sent as unset");
        assert_eq!(r.yield_amount, f64::MAX);
        assert_eq!(r.yield_redemption_date, "");
    }

    #[test]
    fn commission_frame_carries_realized_pnl_and_yield() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[(6099, "-12.5"), (236, "4.25"), (696, "20301231")]), &shared);
        let r = shared.orders.drain_commission_reports().remove(0);
        assert_eq!(r.realized_pnl, -12.5);
        assert_eq!(r.yield_amount, 4.25);
        assert_eq!(r.yield_redemption_date, "20301231");
    }

    #[test]
    fn commission_frame_duplicates_and_lower_revisions_are_skipped() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")]), &shared);
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")]), &shared);
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01")]), &shared);
        assert_eq!(shared.orders.drain_commission_reports().len(), 1);
        // A higher revision replaces the report.
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.0a"), (6378, "1.5")]), &shared);
        let r = shared.orders.drain_commission_reports();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].commission_and_fees, 1.5);
    }

    #[test]
    fn commission_frame_without_a_commission_is_dropped() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut frame = commission_frame(&[]);
        frame.remove(&6378);
        ccp.handle_commission_report(&frame, &shared);
        assert!(shared.orders.drain_commission_reports().is_empty());
    }

    #[test]
    fn a_fill_carries_its_execution_id() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let fills = shared.orders.drain_fills_with_exec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].1.exec_id, "0000e0d5.6ab5f36f.01.01");
    }

    // ibx#474: the captured fill of 25/09/2026 (clientId 250, orderRef
    // pm0925-fill-BUY): time from tag 60 when 6699 is absent, exchange from
    // tag 100, clientId from 6119, orderRef from 6010.
    #[test]
    fn a_fill_carries_its_execution_details() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25"),
            (100, "ARCA"), (207, "ARCA"), (30, "ARCA"), (6119, "250"), (6010, "pm0925-fill-BUY"),
            (60, "20260925-08:49:54"), (52, "20260925-08:49:55.123")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let (_, exec) = shared.orders.drain_fills_with_exec().remove(0);
        assert_eq!(exec.time_secs, Some(1790326194));
        assert_eq!(exec.exchange, "ARCA");
        assert_eq!(exec.client_id, 250);
        assert_eq!(exec.order_ref, "pm0925-fill-BUY");
        assert_eq!(exec.model_code, "");
    }

    #[test]
    fn a_fill_time_prefers_6699_and_exchange_falls_back_to_207() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "e2"),
            (31, "1"), (32, "1"), (14, "1"), (151, "0"), (207, "NASDAQ"),
            (6699, "20260925-08:49:53.500"), (60, "20260925-08:49:54")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let (_, exec) = shared.orders.drain_fills_with_exec().remove(0);
        assert_eq!(exec.time_secs, Some(1790326193));
        assert_eq!(exec.exchange, "NASDAQ");
    }

    #[test]
    fn fix_utc_time_to_unix_seconds() {
        assert_eq!(fix_utc_to_unix_secs("19700101-00:00:00"), Some(0));
        assert_eq!(fix_utc_to_unix_secs("20000301-12:30:15.250"), Some(951913815));
        assert_eq!(fix_utc_to_unix_secs("20260925 08:49:54"), None);
        assert_eq!(fix_utc_to_unix_secs("2026"), None);
    }

    #[test]
    fn split_exec_revision_reads_the_last_segment_as_hex() {
        assert_eq!(split_exec_revision("0000e0d5.6ab5f36f.01.0a"), ("0000e0d5.6ab5f36f.01", 10));
        assert_eq!(split_exec_revision("plain"), ("plain", 0));
    }

    #[test]
    fn record_exec_id_dedupes_within_window() {
        let mut ccp = CcpState::new();
        assert!(ccp.record_exec_id("exec-A"), "first sighting is new");
        assert!(!ccp.record_exec_id("exec-A"), "immediate replay is a duplicate");
    }

    #[test]
    fn record_exec_id_evicts_oldest_not_whole_set() {
        let mut ccp = CcpState::new();
        // The very first ExecID — the one a reconnect is most likely to replay.
        assert!(ccp.record_exec_id("exec-first"));
        // Push the window exactly to its cap. Together with "exec-first" this is
        // EXEC_ID_WINDOW + 1 inserts, which evicts exactly one entry: the oldest
        // ("exec-first"). Every other recent ID must remain deduplicated.
        for i in 0..EXEC_ID_WINDOW {
            assert!(ccp.record_exec_id(&format!("exec-{i}")));
        }
        assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
        // Oldest was evicted, so a replay now reads as new (unavoidable past the
        // window) — but the most recent IDs are still caught as duplicates.
        assert!(!ccp.record_exec_id("exec-0"), "recent ID still deduped");
        assert!(!ccp.record_exec_id(&format!("exec-{}", EXEC_ID_WINDOW - 1)),
            "newest ID still deduped");
    }

    // A wholesale clear() would have made "exec-first" re-insertable as new
    // after just one extra fill past the cap; assert the rolling window keeps
    // the bound without that cliff.
    #[test]
    fn record_exec_id_window_is_bounded() {
        let mut ccp = CcpState::new();
        for i in 0..(EXEC_ID_WINDOW * 3) {
            ccp.record_exec_id(&format!("exec-{i}"));
        }
        assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
        assert_eq!(ccp.exec_id_order.len(), EXEC_ID_WINDOW);
    }

    // Build a what-if (6091=1) ExecReport map for order 42. `margin_fields`
    // holds (tag, literal wire value) pairs exactly as the gateway puts them
    // on the wire (ib-agent#160).
    fn what_if_frame(margin_fields: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(11u32, WHAT_IF_CLORD.to_string()); // the preview's own ClOrdID
        m.insert(6091u32, "1".to_string()); // what-if marker
        for (tag, val) in margin_fields {
            m.insert(*tag, val.to_string());
        }
        m
    }

    // The full six margin fields of the captured true-zero close preview
    // (ib-agent#160 scenario 2b).
    const ZERO_CLOSE_FIELDS: [(u32, &str); 6] = [
        (6826, "976.07"), (6827, "887.34"), (6828, "945924.53"),
        (6092, "0"), (6093, "0"), (6094, "945923.47"),
    ];

    /// The ClOrdID the preview of order 42 was sent under (ibx#462).
    const WHAT_IF_CLORD: &str = "42.1000000";

    /// A preview of order 42 in flight, and a working order 42 the preview
    /// must not touch (ibx#462).
    fn what_if_test_state() -> (CcpState, Context, SharedState) {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.what_ifs.insert(WHAT_IF_CLORD.to_string(), (42, instrument));
        context.insert_order(crate::types::Order {
            order_id: 42,
            instrument,
            side: Side::Buy,
            price: 0,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        (CcpState::new(), context, SharedState::new())
    }

    // ibx#462, captured 02/10/2026 (b1_462_whatif, a what-if of 10,000,000
    // shares): the data reply keeps every value as sent, its status from
    // 39=A, its reason for the 201 after the open order; no commission.
    #[test]
    fn what_if_reply_values_status_and_reason() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (39, "A"), (58, "YOUR ORDER IS NOT ACCEPTED."), (6094, "823189.11"), (6092, "1093819889.17"),
            (6093, "994381596.62"), (6828, "954395.81"), (6826, "4943.27"), (6827, "4125.25"),
            (8130, "USD"), (6552, "8600"),
        ]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        let s = &responses[0].state;
        assert!(responses[0].final_reply);
        assert_eq!(s.status, "PreSubmitted");
        assert_eq!((s.init_margin_before, s.init_margin_after), (Some(4943.27), Some(1093819889.17)));
        assert_eq!((s.commission, s.margin_currency.as_str(), s.suggested_size.as_str()), (None, "USD", "8600"));
        assert_eq!(s.reject_reason, "YOUR ORDER IS NOT ACCEPTED.");
        assert!(context.what_ifs.is_empty());
    }

    // ibx#462 (`trader.order.bQ.a(gi, e3, fq)@263-321`): a reply with an
    // order message gives an open order with that warning text, and the
    // preview waits for its data reply.
    #[test]
    fn what_if_order_message_reply_keeps_the_preview() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let event_tx = Some(event_tx);
        let text = "Warning: your order will not be placed at the exchange until 2026-10-02 09:30:00 US/Eastern";
        ccp.handle_exec_report(&what_if_frame(&[(39, "A"), (6360, "TIME"), (6361, text), (6378, "0")]),
            &mut context, &shared, &event_tx, "");
        assert!(event_rx.try_recv().is_err(), "the order-message reply is not the preview's answer");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        assert!(!responses[0].final_reply);
        assert_eq!((responses[0].state.warning_text.as_str(), responses[0].state.init_margin_after), (text, None));
        assert!(context.what_ifs.contains_key(WHAT_IF_CLORD), "the preview still waits");
        ccp.handle_exec_report(&what_if_frame(&ZERO_CLOSE_FIELDS), &mut context, &shared, &event_tx, "");
        let responses = shared.orders.drain_what_if_responses();
        assert!(responses[0].final_reply && responses[0].state.warning_text.is_empty());
        match event_rx.try_recv() {
            Ok(Event::WhatIf(answer)) => assert_eq!(answer.state.equity_with_loan_after, Some(945923.47)),
            other => panic!("the data reply is the answer: {other:?}"),
        }
        assert!(context.what_ifs.is_empty());
    }

    // ibx#205: a margin-reducing preview (close, cash-account sell) resolves to a
    // post-trade init margin of exactly 0, which the gateway sends as numeric "0"
    // (ib-agent#160). The old `> 0.0` guard dropped it and the caller timed out.
    #[test]
    fn what_if_zero_init_margin_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&ZERO_CLOSE_FIELDS);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1, "zero-margin preview must be delivered");
        assert_eq!(responses[0].init_margin_after, 0);
        // The completed preview is consumed; the working order is not.
        assert!(context.what_ifs.is_empty());
        assert!(context.order(42).is_some());
    }

    // The not-ready ack carries the literal "n/a" in all six margin fields
    // (ib-agent#160); it must be skipped so only the real data frame surfaces.
    #[test]
    fn what_if_not_ready_ack_is_skipped() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (6826, "n/a"), (6827, "n/a"), (6828, "n/a"),
            (6092, "n/a"), (6093, "n/a"), (6094, "n/a"),
        ]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(),
            "n/a ack must not surface as a response");
        // The preview stays pending for the subsequent data frame.
        assert!(context.what_ifs.contains_key(WHAT_IF_CLORD));
    }

    // ibx#213: the gateway's real-frame test is "any of the six margin fields
    // is set", not "6092 is set". A preview that omits 6092 but carries
    // numeric siblings must be delivered, with the absent field read as 0.
    #[test]
    fn what_if_without_6092_but_numeric_siblings_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[(6093, "0"), (6094, "945923.47")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1, "sibling-only preview must be delivered");
        assert_eq!(responses[0].init_margin_after, 0);
        assert_eq!(responses[0].equity_with_loan_after,
            (945923.47 * PRICE_SCALE as f64) as Price);
        assert!(context.what_ifs.is_empty());
    }

    // ibx#214: "nan" parses as f64::NAN, so it passed the old parse-success
    // gate and surfaced as a bogus zero-margin preview. The gateway treats
    // nan as unset, so an all-nan frame is not a data frame.
    #[test]
    fn what_if_nan_sentinels_are_skipped() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (6826, "nan"), (6827, "nan"), (6828, "nan"),
            (6092, "nan"), (6093, "nan"), (6094, "nan"),
        ]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(),
            "all-nan frame must not surface as a response");
        assert!(context.what_ifs.contains_key(WHAT_IF_CLORD));
    }

    // Mixed frame: a nan field is unset, but one finite sibling makes the
    // frame real. The nan field itself must read as 0, not poison the price.
    #[test]
    fn what_if_nan_field_with_finite_sibling_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[(6092, "nan"), (6094, "945923.47")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].init_margin_after, 0, "nan field reads as unset/0");
        assert_eq!(responses[0].equity_with_loan_after,
            (945923.47 * PRICE_SCALE as f64) as Price);
    }

    // ibx#462: a reply is routed by the preview's ClOrdID even without the
    // flag; it never touches the working order of the same id, nor the
    // ClOrdID recorded for it. A reject ends the preview with 201.
    #[test]
    fn what_if_reply_never_touches_the_working_order() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        context.last_clord.insert(42, "42.0".to_string());
        let mut frame = what_if_frame(&ZERO_CLOSE_FIELDS);
        frame.remove(&6091);
        frame.insert(39, "A".to_string());
        frame.insert(150, "A".to_string());
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_what_if_responses().len(), 1);
        assert_eq!(context.order(42).map(|o| o.status), Some(crate::types::OrderStatus::Submitted));
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.0"));
        assert!(shared.orders.drain_order_updates().is_empty(), "no status for the preview");

        context.what_ifs.insert(WHAT_IF_CLORD.to_string(), (42, 0));
        let mut reject = what_if_frame(&[]);
        reject.insert(39, "8".to_string());
        reject.insert(150, "8".to_string());
        reject.insert(58, "YOUR ORDER IS NOT ACCEPTED.".to_string());
        ccp.handle_exec_report(&reject, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(),
            [(42, 201, "Order rejected - reason:YOUR ORDER IS NOT ACCEPTED.".to_string())]);
        assert!(context.what_ifs.is_empty());
        assert!(context.order(42).is_some());

        // A flagged reply for a preview this session did not send.
        ccp.handle_exec_report(&what_if_frame(&ZERO_CLOSE_FIELDS), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty());
    }

    // ibx#210: a working order carries wire 39=0 whether it is routed or not.
    // The gateway reports PreSubmitted while it waits (e.g. placed pre-market)
    // and Submitted only once routed to an exchange. The discriminator is the
    // routing tags on the same exec report, not a distinct wire status
    // (ib-agent#162).
    fn ord_status_test_state() -> (CcpState, Context, SharedState) {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            42, instrument, Side::Buy, 1, 100 * PRICE_SCALE, b'2', b'0', 0,
        )); // starts at PendingSubmit
        (CcpState::new(), context, SharedState::new())
    }

    fn exec_report_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(11u32, "42".to_string()); // ClOrdID
        for (tag, val) in pairs {
            m.insert(*tag, val.to_string());
        }
        m
    }

    // ibx#329: the parent id came from the OCA group, so every order of an
    // OCA group reported the same made-up parent. It comes from the parent
    // link.
    #[test]
    fn parent_id_comes_from_the_parent_link_not_the_oca_group() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let child = context.market.try_register(756733).unwrap();
        context.insert_order(crate::types::Order::new(43, child, Side::Sell, 1, 110 * PRICE_SCALE, b'2', b'1', 0));

        // Plain OCA order, no parent.
        let oca = exec_report_frame(&[(20, "0"), (39, "0"), (150, "0"), (583, "PROBE-OCA-1"),
            (6209, "ReduceOnFillNonBlock")]);
        ccp.handle_exec_report(&oca, &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.last().unwrap().parent_id, 0);

        // Bracket child of order 42 whose parent was modified once.
        let report = [
            (11u32, "43.0"), (20, "0"), (39, "0"), (150, "0"), (583, "42"),
            (6209, "CancelOnFillWBlock"), (6107, "42.1"),
        ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
        ccp.handle_exec_report(&report, &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.last().unwrap().order_id, 43);
        assert_eq!(updates.last().unwrap().parent_id, 42);
        assert_eq!(shared.orders.get_order_info(43).unwrap().order.parent_id, 42);
    }

    // A parent the server holds under another id (recovered from another
    // session) is reported by its API order id.
    #[test]
    fn a_parent_link_to_a_recovered_order_gives_its_order_id() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        context.last_clord.insert(15, "1626578573.1".into());
        let report = exec_report_frame(&[(20, "0"), (39, "0"), (150, "0"), (6107, "1626578573.0")]);
        ccp.handle_exec_report(&report, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_updates().last().unwrap().parent_id, 15);
    }

    #[test]
    fn ord_status_new_unrouted_is_presubmitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // 39=0, no ExDestination, exec ref "NONE" — waiting, not yet routed.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
    }

    #[test]
    fn ord_status_new_routed_is_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // 39=0 with the order routed to ARCA — working.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_presubmitted_then_routed_advances_to_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&waiting, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    // ibx#247: a limit replace is acked with a pending report (39=6 on the
    // new version) and then Replaced (39=5). The reference reports no status
    // change across it (ib-agent#192 A1a); ibx used to report PendingCancel
    // and never move back, so the order looked cancelled.
    #[test]
    fn ord_status_replace_keeps_the_working_status() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&waiting, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();

        let pending = exec_report_frame(&[(11, "42.1"), (39, "6"), (150, "6"), (198, "NONE")]);
        ccp.handle_exec_report(&pending, &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&replaced, &mut context, &shared, &None, "");

        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PreSubmitted);
        // Each report is reported with the unchanged status, as the
        // reference does for a modify confirm (ibx#473).
        let statuses: Vec<_> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [crate::types::OrderStatus::PreSubmitted; 2]);
    }

    // ibx#473: a stale report is still not reported (ibx#212), and the
    // update carries the server's filled quantity (tag 14).
    #[test]
    fn a_stale_report_is_not_reported_and_filled_is_the_server_count() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        let stale = exec_report_frame(&[(39, "A"), (150, "A")]);
        ccp.handle_exec_report(&stale, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_updates().is_empty(), "stale PreSubmitted after Submitted");
        let restated = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0.5"), (151, "0.5")]);
        ccp.handle_exec_report(&restated, &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, crate::types::OrderStatus::Submitted);
        assert_eq!(updates[0].filled_qty_fixed, QTY_SCALE / 2);
    }

    // ibx#473: the update is pushed after the order cache holds this
    // report, so the client reads the same frame's order state.
    #[test]
    fn the_order_cache_holds_the_report_before_the_update() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (44, "101.5")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_updates().len(), 1);
        let info = shared.orders.get_order_info(42).expect("cached");
        assert_eq!(info.order_state.status, "Submitted");
        assert_eq!(info.order.lmt_price, 101.5);
    }

    #[test]
    fn ord_status_replace_of_a_routed_order_stays_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&replaced, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_pending_on_a_cancel_request_is_pending_cancel() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        let pending = exec_report_frame(&[(11, "C42"), (39, "6"), (150, "6")]);
        ccp.handle_exec_report(&pending, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PendingCancel);
    }

    // ibx#472: the captured server cancel of an IOC order (25/09/2026):
    // 39=A, 39=0, 150=D|39=D, then 150=4|39=4. The 39=D report is a pending
    // cancel, and the order stays open, so a fill between the two reports is
    // applied.
    #[test]
    fn pending_cancel_report_keeps_the_order_until_cancelled() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        for pairs in [
            &[(20, "3"), (39, "A"), (150, "A")][..],
            &[(20, "0"), (39, "0"), (150, "0"), (100, "ARCA")][..],
            &[(20, "3"), (39, "D"), (150, "D"), (378, "5")][..],
        ] {
            ccp.handle_exec_report(&exec_report_frame(pairs), &mut context, &shared, &None, "");
        }
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
        let statuses: Vec<OrderStatus> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [OrderStatus::PreSubmitted, OrderStatus::Submitted, OrderStatus::PendingCancel]);

        let instrument = context.order(42).unwrap().instrument;
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "e1"),
            (31, "100"), (32, "1"), (14, "1"), (151, "0"), (6, "100")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_fills().len(), 1, "the fill after 39=D is applied");
        assert_eq!(context.position_fixed(instrument), QTY_SCALE);
    }

    #[test]
    fn cancelled_report_after_pending_cancel_ends_the_order() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        for pairs in [
            &[(20, "0"), (39, "0"), (150, "0"), (100, "ARCA")][..],
            &[(20, "3"), (39, "D"), (150, "D")][..],
            &[(20, "3"), (39, "4"), (150, "4")][..],
        ] {
            ccp.handle_exec_report(&exec_report_frame(pairs), &mut context, &shared, &None, "");
        }
        assert!(context.order(42).is_none(), "a cancelled order leaves the engine");
        // Its state stays known for a later cancel (ibx#464).
        assert_eq!(context.finished_status(42), Some(crate::types::OrderStatus::Cancelled));
        let last = shared.orders.drain_order_updates().last().map(|u| u.status);
        assert_eq!(last, Some(crate::types::OrderStatus::Cancelled));
    }

    // ibx#464: a cancel now carries the order's next ClOrdID version, not a
    // 'C' prefix. Its reports are read as the cancel's: the pending report is
    // PendingCancel, and the order's ClOrdID on record does not move.
    #[test]
    fn reports_of_a_versioned_cancel_are_read_as_the_cancel() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(11, "42.0"), (39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        context.cancel_clord.insert(42, "42.1".to_string());
        let pending = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "6"), (150, "6")]);
        ccp.handle_exec_report(&pending, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.0"));
        let done = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "4"), (150, "4")]);
        ccp.handle_exec_report(&done, &mut context, &shared, &None, "");
        assert!(context.order(42).is_none());
        assert!(context.cancel_clord.get(&42).is_none());
    }

    #[test]
    fn a_cancel_reject_ends_the_cancel() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        context.modify_versions.insert(42, 1);
        context.cancel_clord.insert(42, "42.1".to_string());
        let mut reject = std::collections::HashMap::new();
        for (t, v) in [(35u32, "9"), (11, "42.1"), (41, "42.0"), (434, "1"), (102, "0")] {
            reject.insert(t, v.to_string());
        }
        ccp.handle_cancel_reject(&reject, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(context.cancel_clord.get(&42).is_none());
    }

    fn pipe_frame(text: &str) -> Vec<u8> {
        text.replace('|', "\x01").into_bytes()
    }

    // ibx#252, captured (ib-agent#192 C2b): the cancel of a child the
    // server had already cancelled with its parent is refused, then the
    // answer to the status request the refusal triggers comes in the
    // rejected state. The reference lowers the ClOrdID version, sends the
    // status request, and reads the answer as Cancelled: no reject, no 201.
    #[test]
    fn refused_cancel_of_a_cancelled_order_asks_its_status_and_stays_cancelled() {
        use crate::types::OrderStatus;
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut ccp = CcpState::new();
        let instrument = context.register_instrument(265598);
        context.insert_order(crate::types::Order::new(
            1626578655, instrument, Side::Sell, 1, 50962 * PRICE_SCALE / 100, b'2', b'0', 0,
        ));
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        // The cancel ibx sent: version 1.
        context.modify_versions.insert(1626578655, 1);
        context.cancel_clord.insert(1626578655, "1626578655.1".to_string());
        context.last_clord.insert(1626578655, "1626578655.0".to_string());

        let cancelled = pipe_frame("8=FIX.4.1|9=000518|35=8|34=000769|43=N|52=20260923-10:09:51|11=1626578655.0|17=140781.1790158191.3|150=4|20=3|378=5|39=4|167=CS|55=AAPL|6210=BEST|38=1|44=509.62|32=0|31=0.00|14=0|151=0|6=0|54=2|37=00cf16ed.000225ed.6ab35319.0001|1=DU1|60=20260923-10:09:51|6571=20260923-10:09:51|6596=20261231-21:00:00|583=1626578654|6209=ReduceOnFillNonBlock|40=2|6119=192|6121=118|59=1|6008=265598|15=USD|6004=BEST|6122=c|6107=1626578654.0|6531=11/1/-7625079|6205=1|6236=CHILD|198=NONE|6115=0|6088=Socket|6035=AAPL|6817=20260923-10:09:46|10=052|");
        ccp.process_ccp_message(&cancelled, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.finished_status(1626578655), Some(crate::types::OrderStatus::Cancelled));
        shared.orders.drain_order_updates();
        shared.orders.drain_order_errors();

        let refused = pipe_frame("8=FIX.4.1|9=000106|35=9|34=000770|43=N|52=20260923-10:09:51|37=0|11=1626578655.1|41=1626578655.0|39=8|102=1|58=No such order|10=200|");
        ccp.process_ccp_message(&refused, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ccp_messages_sent(&mut server),
            ["35=H|11=1626578655.0|55=*|54=*|6471=1|1=DU1"]);
        assert_eq!(context.modify_versions.get(&1626578655), Some(&0));
        assert!(context.cancel_clord.get(&1626578655).is_none());

        let answer = pipe_frame("8=FIX.4.1|9=000183|35=8|34=000771|43=N|52=20260923-10:09:51|11=1626578655.0|17=140781.1790158191.7|150=8|20=3|103=0|39=8|38=0|32=0|31=0.00|14=0|151=0|6=0|37=0|58=No such order|60=20260923-10:09:51|40=2|10=120|");
        ccp.process_ccp_message(&answer, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.orders.drain_order_errors().is_empty(), "no 201 and no 202");
        assert!(shared.orders.drain_order_updates().iter().all(|u| u.status != OrderStatus::Rejected));
        assert_eq!(context.finished_status(1626578655), Some(crate::types::OrderStatus::Cancelled));
        let info = shared.orders.get_order_info(1626578655).unwrap();
        assert_eq!(info.order_state.status, "Cancelled");
    }

    // ibx#252: a refused cancel of a working order changes neither its
    // status nor its record; the answer to the status request sets the
    // status, here back to working.
    #[test]
    fn refused_cancel_keeps_the_order_until_the_status_answer() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        let routed = exec_report_frame(&[(11, "42.0"), (39, "0"), (150, "0"), (20, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "DU1");
        context.modify_versions.insert(42, 1);
        context.cancel_clord.insert(42, "42.1".to_string());
        context.apply_order_status(42, OrderStatus::PendingCancel);
        shared.orders.drain_order_updates();

        ccp.process_ccp_message(&pipe_frame("35=9|11=42.1|41=42.0|39=0|102=0|58=Too late to cancel|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel, "no state change");
        assert!(shared.orders.get_order_info(42).is_some(), "the record stays");
        assert!(shared.orders.drain_order_updates().is_empty());
        assert_eq!(ccp_messages_sent(&mut server), ["35=H|11=42.0|55=*|54=*|6471=1|1=DU1"]);

        // The answer: still working.
        ccp.process_ccp_message(&pipe_frame("35=8|11=42.0|150=0|20=3|39=0|100=ARCA|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted);
        let last = shared.orders.drain_order_updates().last().map(|u| u.status);
        assert_eq!(last, Some(OrderStatus::Submitted));
        assert!(context.status_queries.is_empty());

        // A later status report is guarded again.
        context.apply_order_status(42, OrderStatus::PendingCancel);
        ccp.process_ccp_message(&pipe_frame("35=8|11=42.0|150=0|20=3|39=0|100=ARCA|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
    }

    // ibx#252: the reference acts only on a reject that carries the order's
    // current ClOrdID, and reads the order from it, not from the previous id.
    #[test]
    fn a_reject_of_an_older_version_changes_nothing() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        context.modify_versions.insert(42, 2);
        context.last_clord.insert(42, "42.2".to_string());
        ccp.process_ccp_message(&pipe_frame("35=9|11=42.1|41=42.0|39=0|102=1|58=No such order|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        // Only the reject's own id names the order.
        ccp.process_ccp_message(&pipe_frame("35=9|41=42.2|39=0|102=1|58=No such order|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).is_empty());
        assert_eq!(context.modify_versions.get(&42), Some(&2));
        assert!(shared.orders.drain_cancel_rejects().is_empty());
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingSubmit);

        // A refused modify: the version and the ClOrdID on record go back.
        ccp.process_ccp_message(&pipe_frame("35=9|11=42.2|41=42.1|39=0|102=0|434=2|58=Order modify rejected|"),
            &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.modify_versions.get(&42), Some(&1));
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.1"));
        assert_eq!(ccp_messages_sent(&mut server), ["35=H|11=42.1|55=*|54=*|6471=1|1=DU1"]);
    }

    // ibx#252: the rejected state of an ordinary report is still a reject.
    #[test]
    fn a_rejected_new_order_is_still_rejected() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let frame = exec_report_frame(&[(11, "42.0"), (20, "0"), (39, "8"), (150, "8"), (58, "no")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "DU1");
        assert_eq!(context.finished_status(42), Some(OrderStatus::Rejected));
        assert_eq!(shared.orders.drain_order_errors()[0].1, 201);
    }

    // ibx#472: the reference handles 39=E and 39=I as invalid statuses: no
    // status change and no callback.
    #[test]
    fn pending_replace_and_inactive_reports_keep_the_status() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        for code in ["E", "I"] {
            let frame = exec_report_frame(&[(20, "3"), (39, code), (150, code)]);
            ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
            assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted, "39={}", code);
        }
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    // ibx#411: one portfolio message carries a row per position (ib-agent#192
    // D4). This one is captured, 3 rows; only the last was applied.
    #[test]
    fn a_portfolio_message_applies_every_position_row() {
        let mut context = Context::new();
        let shared = SharedState::new();
        let msft = context.market.try_register(272093).unwrap();
        let captured = "8=FIX.4.1|9=000999|35=UP|6529=AR.3|\
            6068=AXTI                  |6288=0|8001=PositionList|8002=AXTI/USD/1/4726868|6064=1|6067=107.50|6065=107.5|6066=1781529403|15=USD|6008=4726868|167=STK|6101=117.79|6235=107.5|6099=0.00|6100=-10.29|9821=0|6627=0|6920=0|8136=107.485466|8152=107.5|\
            6068=MSFT                  |6288=0|8001=PositionList|8002=MSFT/USD/1/272093|6064=-10|6067=-3964.40|6065=396.44000245|6066=1781529403|15=USD|6008=272093|167=STK|6101=423.53108|6235=396.44000245|6099=0.00|6100=270.91|9821=0|6627=0|6920=0|8136=396.4232483|8152=-3964.4000244|\
            6068=SPY                   |6288=0|8001=PositionList|8002=SPY/USD/1/756733|6064=18|6067=13530.35|6065=751.6859131|6066=1781529403|15=USD|6008=756733|167=STK|6101=723.43166665|6235=751.6859131|6099=0.00|6100=508.58|9821=0|6627=0|6920=0|8136=-1|8152=13530.34643555|10=000|";
        let msg = captured.replace('|', "\x01");

        handle_portfolio_message(msg.as_bytes(), &mut context, &shared, &None);

        let row = |con_id: i64| shared.portfolio.position_info(con_id).expect("row applied");
        assert_eq!((row(4726868).position_fixed / crate::types::QTY_SCALE, row(4726868).symbol.as_str()), (1, "AXTI"));
        assert_eq!(row(4726868).avg_cost, (117.79 * PRICE_SCALE as f64) as Price);
        assert_eq!((row(272093).position_fixed / crate::types::QTY_SCALE, row(272093).symbol.as_str()), (-10, "MSFT"));
        assert_eq!(row(272093).unrealized_pnl, (270.91 * PRICE_SCALE as f64) as Price);
        assert_eq!((row(756733).position_fixed / crate::types::QTY_SCALE, row(756733).symbol.as_str()), (18, "SPY"));
        // The multiplier is one part of the row key, not the whole key.
        assert_eq!(row(4726868).multiplier, "1");
        assert_eq!(context.position_fixed(msft) / crate::types::QTY_SCALE, -10, "a registered instrument's position follows its row");
    }

    // ibx#238 / ib-agent#172: in the UP portfolio snapshot the average cost is
    // tag 6101 and 6065 is the market price. The handler previously read 6065 as
    // the average cost. Verify the mapping and that all marks are stored.
    #[test]
    fn position_update_maps_marks_and_avg_cost_from_correct_tags() {
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut m = std::collections::HashMap::new();
        m.insert(6008u32, "756733".to_string());   // conId
        m.insert(6064u32, "10".to_string());        // position
        m.insert(6101u32, "100.50".to_string());    // averageCost
        m.insert(6065u32, "110.25".to_string());    // marketPrice
        m.insert(6067u32, "1102.50".to_string());   // marketValue
        m.insert(6100u32, "97.50".to_string());     // unrealizedPNL
        m.insert(6099u32, "5.00".to_string());      // realizedPNL
        handle_position_update(&m, &mut context, &shared, &None);

        let pi = shared.portfolio.position_info(756733).expect("position stored");
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 10);
        assert_eq!(pi.avg_cost, (100.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.market_price, (110.25 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.market_value, (1102.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.unrealized_pnl, (97.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.realized_pnl, (5.00 * PRICE_SCALE as f64) as Price);
    }

    // The lean position feed carries no marks; it must not zero the marks the
    // portfolio snapshot set (ibx#238).
    #[test]
    fn lean_position_feed_does_not_clobber_marks() {
        let shared = SharedState::new();
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 1, position_fixed: (10) as i64 * crate::types::QTY_SCALE, avg_cost: 100 * PRICE_SCALE, ..Default::default()
        });
        shared.portfolio.set_position_marks(1, 110 * PRICE_SCALE, 1100 * PRICE_SCALE, 100 * PRICE_SCALE, 5 * PRICE_SCALE);
        // Lean feed updates position + avg_cost only.
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 1, position_fixed: (12) as i64 * crate::types::QTY_SCALE, avg_cost: 101 * PRICE_SCALE, ..Default::default()
        });
        let pi = shared.portfolio.position_info(1).unwrap();
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 12);
        assert_eq!(pi.avg_cost, 101 * PRICE_SCALE);
        assert_eq!(pi.market_price, 110 * PRICE_SCALE, "marks survive the lean feed");
        assert_eq!(pi.market_value, 1100 * PRICE_SCALE);
        assert_eq!(pi.unrealized_pnl, 100 * PRICE_SCALE);
    }

    // ibx#220: the TIF decoder must be the exact inverse of the outbound
    // encoder. The old map decoded '7' (never emitted) as OPG and dropped
    // OPG and AUC to "".
    #[test]
    fn tif_round_trips_through_encoder_and_decoder() {
        for tif in ["DAY", "GTC", "OPG", "IOC", "FOK", "GTD", "AUC"] {
            let order = api::Order { tif: tif.to_string(), ..Default::default() };
            assert_eq!(decode_tif(order.tif_byte()), tif,
                "TIF {tif} must survive encode->decode");
        }
        // DTC has its own code (ibx#467).
        let dtc = api::Order { tif: "DTC".to_string(), ..Default::default() };
        assert_eq!(decode_tif(dtc.tif_byte()), "DTC");
        // An unknown code is "???", as the reference, not a wrong TIF (ibx#307).
        assert_eq!(decode_tif(b'7'), "???");
    }

    // ── ibx#227: contract-details deadline sweep ──

    #[test]
    fn sweep_times_out_pending_secdef_with_error_and_end() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        ccp.pending_secdef.push((7, true, past));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert!(ccp.pending_secdef.is_empty(), "expired entry must be reclaimed");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 7);
        assert_eq!(errors[0].1, 200);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![7],
            "end must fire so a blocked wait unblocks");
    }

    // ibx#285: a negative API request id is the caller's, not an internal
    // lookup: cast to an unsigned type it fell in the internal range and
    // its timeout was dropped.
    #[test]
    fn sweep_times_out_a_negative_request_id_as_the_callers() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        ccp.pending_secdef.push((-7, true, past));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), vec![(-7, 200)]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![-7]);
    }

    #[test]
    fn sweep_drops_internal_secdef_silently() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        // Internal sentinel (cache auto-fetch): no user is waiting on it.
        ccp.pending_secdef.push((0xF000_0001, true, past));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert!(ccp.pending_secdef.is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    #[test]
    fn sweep_times_out_incomplete_fanout() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.pending_fanout.push(PendingFanout {
            api_req_id: 9,
            outstanding: vec![("ibxfan-9-26".to_string(), 1, "IEX".to_string())],
            total: 27, // one reply lost — previously hung forever
            records: Vec::new(),
            deadline: Instant::now() - std::time::Duration::from_secs(1),
        });

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert!(ccp.pending_fanout.is_empty());
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 9);
        assert_eq!(errors[0].1, 200);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![9]);
    }

    // ── ibx#400: empty reply and session reject ──

    fn empty_secdef_reply(req_id: &str) -> Vec<u8> {
        crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "d"), (43, "N"), (320, req_id), (322, "*"),
            (323, "4"), (6038, "Y"), (6019, "0"), (6344, "0"),
        ], 1)
    }

    #[test]
    fn empty_secdef_reply_gives_error_200_and_no_end() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let deadline = Instant::now() + SECDEF_TIMEOUT;
        ccp.pending_secdef.push((1005, false, deadline));
        ccp.pending_secdef.push((1006, true, deadline));

        for rid in ["1005", "1006"] {
            ccp.process_ccp_message(&empty_secdef_reply(rid), &mut None, &mut context, &shared, &None,
                &mut HeartbeatState::new(), "DU1");
        }

        assert!(shared.reference.drain_contract_details().is_empty(), "no conId 0 row");
        assert!(shared.reference.drain_contract_details_end().is_empty(), "no end after error 200");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 2);
        for (err, rid) in errors.iter().zip([1005, 1006]) {
            assert_eq!((err.0, err.1, err.2.as_str()), (rid, 200, NO_SECURITY_DEFINITION));
        }
        assert!(ccp.pending_secdef.is_empty(), "the lookups are finished");
    }

    #[test]
    fn empty_secdef_reply_to_an_internal_lookup_is_silent() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_secdef.push((0xF000_0002, true, Instant::now() + SECDEF_TIMEOUT));
        let rid = 0xF000_0002u32.to_string();
        ccp.process_ccp_message(&empty_secdef_reply(&rid), &mut None, &mut context, &shared, &None,
            &mut HeartbeatState::new(), "DU1");
        assert!(ccp.pending_secdef.is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    #[test]
    fn session_reject_sends_no_error_and_no_end() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_secdef.push((1006, false, Instant::now() + SECDEF_TIMEOUT));
        let reject = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "3"), (45, "12"), (58, "Invalid value in field # 55"),
        ], 1);
        ccp.process_ccp_message(&reject, &mut None, &mut context, &shared, &None,
            &mut HeartbeatState::new(), "DU1");
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
        assert_eq!(ccp.pending_secdef.len(), 1, "left to the deadline sweep");
    }

    // ── ibx#410: by-symbol lookup frames and the strike retry ──

    fn symbol_lookup(symbol: &str, sec_type: &str, exchange: &str, filters: crate::types::SecDefFilters) -> SymbolLookup {
        SymbolLookup {
            symbol: symbol.into(), sec_type: sec_type.into(), exchange: exchange.into(),
            currency: "USD".into(), filters,
        }
    }

    fn lookup_frame(lookup: &SymbolLookup) -> String {
        let strike = if lookup.filters.strike > 0.0 { format!("{}", lookup.filters.strike) } else { String::new() };
        frame_text(&secdef_by_symbol_fields(7, lookup, &strike))
    }

    fn frame_text(fields: &[(u32, String)]) -> String {
        fields.iter().map(|(t, v)| format!("{}={}", t, v)).collect::<Vec<_>>().join("|")
    }

    fn option_filters(local_symbol: &str, trading_class: &str, multiplier: &str) -> crate::types::SecDefFilters {
        crate::types::SecDefFilters {
            local_symbol: local_symbol.into(),
            trading_class: trading_class.into(),
            last_trade_date_or_contract_month: "20261005".into(),
            strike: 342.5,
            right: "C".into(),
            multiplier: multiplier.into(),
            ..Default::default()
        }
    }

    #[test]
    fn option_lookup_frames_match_the_reference() {
        // Trading class, local symbol and symbol, with a strike: no source.
        let l = symbol_lookup("AAPL", "OPT", "SMART", option_filters("AAPL  261005C00342500", "AAPL", ""));
        assert_eq!(lookup_frame(&l),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6058=AAPL|6035=AAPL  261005C00342500|55=AAPL|167=OPT|541=20261005|201=1|202=342.5|100=BEST|15=USD");
        // Symbol only, with a multiplier.
        let l = symbol_lookup("AAPL", "OPT", "SMART", option_filters("", "", "100"));
        assert_eq!(lookup_frame(&l),
            "35=c|320=FixSecDefReqBySymbol7|321=2|55=AAPL|167=OPT|541=20261005|201=1|202=342.5|231=100|100=BEST|15=USD");
        // The request of the issue.
        let mut f = option_filters("AAPL  261218C00220000", "AAPL", "100");
        f.last_trade_date_or_contract_month = "20261218".into();
        f.strike = 220.0;
        let l = symbol_lookup("AAPL", "OPT", "SMART", f);
        assert_eq!(lookup_frame(&l),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6058=AAPL|6035=AAPL  261218C00220000|55=AAPL|167=OPT|541=20261218|201=1|202=220|231=100|100=BEST|15=USD");
    }

    #[test]
    fn local_symbol_only_lookup_has_the_source_and_no_symbol() {
        let f = crate::types::SecDefFilters { local_symbol: "AAPL  261005C00342500".into(), ..Default::default() };
        let l = symbol_lookup("", "OPT", "SMART", f);
        assert_eq!(lookup_frame(&l),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|6035=AAPL  261005C00342500|167=OPT|100=BEST|15=USD");
    }

    #[test]
    fn future_lookup_puts_the_month_and_the_date_on_different_fields() {
        let month = crate::types::SecDefFilters { last_trade_date_or_contract_month: "202612".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("MNQ", "FUT", "CME", month)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=MNQ|167=FUT|200=202612|100=CME|15=USD");
        let date = crate::types::SecDefFilters { last_trade_date_or_contract_month: "20261218".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("MNQ", "FUT", "CME", date)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=MNQ|167=FUT|541=20261218|100=CME|15=USD");
        let noexp = crate::types::SecDefFilters { last_trade_date_or_contract_month: "noexp".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("MNQ", "FUT", "CME", noexp)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=MNQ|167=FUT|541=NOEXP|100=CME|15=USD");
        let short = crate::types::SecDefFilters { last_trade_date_or_contract_month: "2026".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("MNQ", "FUT", "CME", short)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=MNQ|167=FUT|100=CME|15=USD");
    }

    #[test]
    fn trading_class_only_lookup_has_no_symbol() {
        let f = crate::types::SecDefFilters { trading_class: "NMS".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("AAPL", "STK", "SMART", f)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|6058=NMS|167=CS|100=BEST|15=USD");
    }

    #[test]
    fn stock_lookup_strips_the_slash_and_keeps_the_primary_exchange() {
        let f = crate::types::SecDefFilters { primary_exchange: "NYSE".into(), ..Default::default() };
        assert_eq!(lookup_frame(&symbol_lookup("BRK/A", "STK", "SMART", f)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=BRKA|167=CS|100=BEST|207=NYSE|15=USD");
        assert_eq!(lookup_frame(&symbol_lookup("BRK A", "STK", "SMART", Default::default())),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|55=BRK A|167=CS|100=BEST|15=USD");
    }

    #[test]
    fn identifier_lookup_frame() {
        let f = crate::types::SecDefFilters {
            sec_id: "US0378331005".into(), sec_id_type: "ISIN".into(), ..Default::default()
        };
        assert_eq!(lookup_frame(&symbol_lookup("AAPL", "STK", "SMART", f)),
            "35=c|320=FixSecDefReqByIdTypeValue7|321=2|6088=Socket|22=4|48=US0378331005|100=BEST|15=USD");
    }

    // ibx#229: expired contracts are asked for after the source, only on
    // a lookup by symbol that has a source.
    #[test]
    fn include_expired_rides_a_symbol_lookup_with_a_source() {
        let f = crate::types::SecDefFilters {
            last_trade_date_or_contract_month: "202612".into(), include_expired: true, ..Default::default()
        };
        assert_eq!(lookup_frame(&symbol_lookup("MNQ", "FUT", "CME", f)),
            "35=c|320=FixSecDefReqBySymbol7|321=2|6088=Socket|6320=1|55=MNQ|167=FUT|200=202612|100=CME|15=USD");
        // A lookup with a strike has no source, so no such field either.
        let mut f = option_filters("", "", "");
        f.include_expired = true;
        assert!(!lookup_frame(&symbol_lookup("AAPL", "OPT", "SMART", f)).contains("6320="));
        // Nor a lookup by identifier.
        let f = crate::types::SecDefFilters {
            sec_id: "US0378331005".into(), sec_id_type: "ISIN".into(), include_expired: true, ..Default::default()
        };
        assert_eq!(lookup_frame(&symbol_lookup("AAPL", "STK", "SMART", f)),
            "35=c|320=FixSecDefReqByIdTypeValue7|321=2|6088=Socket|22=4|48=US0378331005|100=BEST|15=USD");
    }

    // ibx#229: the request number of a reply, with or without the name.
    #[test]
    fn secdef_request_number_strips_the_lookup_name() {
        use crate::control::contracts::secdef_request_number;
        assert_eq!(secdef_request_number("FixSecDefReqBySymbol42"), Some(42));
        assert_eq!(secdef_request_number("FixSecDefReqByIdTypeValue7"), Some(7));
        assert_eq!(secdef_request_number("100"), Some(100));
        assert_eq!(secdef_request_number("ibxfan-1-2"), None);
        assert_eq!(secdef_request_number("FixSecDefReqBySymbol"), None);
    }

    #[test]
    fn strike_divided_by_100_moves_the_decimal_point() {
        assert_eq!(strike_divided_by_100("342.8"), "3.428");
        assert_eq!(strike_divided_by_100("342.5"), "3.425");
        assert_eq!(strike_divided_by_100("220"), "2.2");
        assert_eq!(strike_divided_by_100("100"), "1");
        assert_eq!(strike_divided_by_100("5"), "0.05");
        assert_eq!(strike_divided_by_100("0.5"), "0.005");
        assert_eq!(strike_divided_by_100("1234.25"), "12.3425");
    }

    fn socket_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// Every message written to `server`, without the framing, sequence
    /// and time fields.
    fn ccp_messages_sent(server: &mut std::net::TcpStream) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        text.split("8=FIX.4.1\x01").filter(|m| !m.is_empty()).map(|m| {
            m.split('\x01')
                .filter(|f| !f.is_empty())
                .filter(|f| !["9=", "34=", "52=", "10="].iter().any(|p| f.starts_with(p)))
                .collect::<Vec<_>>().join("|")
        }).collect()
    }

    #[test]
    fn empty_reply_to_a_strike_lookup_retries_once_then_gives_error_200() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        let mut f = option_filters("", "", "");
        f.strike = 342.8;
        ccp.send_secdef_request_by_symbol(9, "AAPL", "OPT", "SMART", "USD", &f, &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server),
            ["35=c|320=FixSecDefReqBySymbol9|321=2|55=AAPL|167=OPT|541=20261005|201=1|202=342.8|100=BEST|15=USD"]);

        ccp.process_ccp_message(&empty_secdef_reply("9"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ccp_messages_sent(&mut server),
            ["35=c|320=FixSecDefReqBySymbol9|321=2|55=AAPL|167=OPT|541=20261005|201=1|202=3.428|100=BEST|15=USD"]);
        assert!(shared.reference.drain_historical_errors().is_empty(), "no error before the retry answers");
        assert_eq!(ccp.pending_secdef.len(), 1);

        ccp.process_ccp_message(&empty_secdef_reply("FixSecDefReqBySymbol9"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).is_empty(), "one retry only");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].0, errors[0].1), (9, 200));
        assert!(shared.reference.drain_contract_details_end().is_empty());
        assert!(ccp.pending_secdef.is_empty() && ccp.pending_lookups.is_empty());
    }

    #[test]
    fn lookup_without_a_strike_has_no_retry() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let mut hb = HeartbeatState::new();
        ccp.send_secdef_request_by_symbol(11, "ZZZZQQ", "STK", "SMART", "USD", &Default::default(), &mut None, &mut hb);
        assert!(ccp.pending_lookups[0].retry_strike.is_none());
        ccp.process_ccp_message(&empty_secdef_reply("11"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shared.reference.drain_historical_errors().len(), 1);
    }

    // ── ibx#435: one row per record, fan-out replies are not rows ──

    use crate::control::contracts::tests::{five_future_records, pipe_msg};

    fn reply(ccp: &mut CcpState, context: &mut Context, shared: &SharedState, msg: &[u8]) {
        ccp.process_ccp_message(msg, &mut None, context, shared, &None, &mut HeartbeatState::new(), "DU1");
    }

    #[test]
    fn multi_record_reply_gives_one_row_per_record_then_end() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.send_secdef_request_by_symbol(21, "MNQ", "FUT", "CME", "USD", &Default::default(), &mut None,
            &mut HeartbeatState::new());

        reply(&mut ccp, &mut context, &shared, &five_future_records("21", ""));

        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 5, "one row per record");
        assert!(rows.iter().all(|(rid, _)| *rid == 21));
        let ids: Vec<i64> = rows.iter().map(|(_, d)| d.con_id).collect();
        assert_eq!(ids, [815824267, 840227399, 866514785, 893091676, 925800444]);
        // The record's own exchange rule is known: no fan-out.
        assert!(ccp.pending_fanout.is_empty());
        assert!(rows.iter().all(|(_, d)| d.market_rule_ids == "67"));
        assert_eq!(shared.reference.drain_contract_details_end(), vec![21]);
        assert!(ccp.pending_secdef.is_empty() && ccp.pending_lookups.is_empty());
    }

    #[test]
    fn records_sharing_a_join_key_ask_their_schedule_once() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        ccp.send_secdef_request_by_symbol(22, "MNQ", "FUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);

        ccp.process_ccp_message(&five_future_records("22", "CME/FUT"), &mut conn, &mut context, &shared, &None,
            &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "one schedule request for the shared key: {sent:?}");
        assert!(sent[0].contains("6256=CME/FUT"), "{}", sent[0]);
        assert!(shared.reference.drain_contract_details().is_empty(), "rows wait for the schedule");
        assert!(shared.reference.drain_contract_details_end().is_empty());

        let schedule = pipe_msg("35=U|6040=107|6256=CME/FUT");
        ccp.process_ccp_message(&schedule, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shared.reference.drain_contract_details().len(), 5);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![22]);
        assert!(ccp.pending_schedule_pair.is_empty());
    }

    fn stock_reply(req_id: &str) -> Vec<u8> {
        pipe_msg(&format!(
            "35=d|43=N|320={req_id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|6031=4563|15=USD|58=NMS|\
             6035=AAPL|6058=NMS|6430=1/STK/NASDAQ|146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|\
             6008=265598|6470=NASDAQ|306=APPLE INC|6046=BEST,AMEX,NYSE,"
        ))
    }

    fn exchange_reply(req_id: &str, exchange: &str, rule: &str) -> Vec<u8> {
        pipe_msg(&format!(
            "35=d|43=N|320={req_id}|322=*|323=4|55=AAPL|167=STK|207={exchange}|6008=265598|6031={rule}|15=USD|\
             58=NMS|6035=AAPL|6058=NMS|146=1|6038=Y|6019=1|6031={rule}|6026=1|6023=0|6027=0.01|6030=1"
        ))
    }

    #[test]
    fn fanout_replies_fill_the_rules_and_are_not_rows() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.send_secdef_request_by_symbol(31, "AAPL", "STK", "SMART", "USD", &Default::default(), &mut None,
            &mut HeartbeatState::new());

        reply(&mut ccp, &mut context, &shared, &stock_reply("31"));
        assert_eq!(ccp.pending_fanout.len(), 1);
        let asked: Vec<(i64, String)> = ccp.pending_fanout[0].outstanding.iter()
            .map(|(_, c, e)| (*c, e.clone())).collect();
        assert_eq!(asked, [(265598, "AMEX".to_string()), (265598, "NYSE".to_string())],
            "only the exchanges with no known rule");
        let ids: Vec<String> = ccp.pending_fanout[0].outstanding.iter().map(|(id, _, _)| id.clone()).collect();
        assert!(shared.reference.drain_contract_details().is_empty(), "the row waits for the fan-out");

        reply(&mut ccp, &mut context, &shared, &exchange_reply(&ids[0], "AMEX", "109"));
        assert!(shared.reference.drain_contract_details().is_empty(), "a fan-out reply is not a row");
        assert!(shared.reference.drain_contract_details_end().is_empty());
        reply(&mut ccp, &mut context, &shared, &exchange_reply(&ids[1], "NYSE", "110"));

        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1, "one row for one record");
        let def = &rows[0].1;
        assert_eq!((rows[0].0, def.con_id, def.exchange.as_str()), (31, 265598, "SMART"));
        assert_eq!(def.long_name, "APPLE INC");
        assert_eq!(def.market_rule_ids, "4563,109,110");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![31]);
        assert!(ccp.pending_fanout.is_empty());

        // The rules are known now: the same lookup again needs no fan-out.
        ccp.send_secdef_request_by_symbol(32, "AAPL", "STK", "SMART", "USD", &Default::default(), &mut None,
            &mut HeartbeatState::new());
        reply(&mut ccp, &mut context, &shared, &stock_reply("32"));
        assert!(ccp.pending_fanout.is_empty());
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.market_rule_ids, "4563,109,110");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![32]);
    }

    /// One record of `symbol` on `exchange` with its schedule key and rule.
    fn listing(symbol: &str, con_id: &str, exchange: &str, key: &str, rule: &str) -> String {
        format!("55={symbol}|167=STK|207={exchange}|6008={con_id}|6256={key}|6031={rule}|15=USD|58=NMS|\
                 6035={symbol}|6058=NMS|")
    }

    // Two lookups in flight at once, one by conId and one by symbol, whose
    // replies interleave and whose rows share a schedule key: each request
    // gets its own row only, then its end. The reply to the conId lookup
    // lists the contract once per exchange; it is still one row.
    #[test]
    fn concurrent_lookups_sharing_a_schedule_key_keep_their_own_rows() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        let key = "1/STK/NASDAQ#LITE";
        ccp.send_secdef_request(100, 756733, &mut conn, &mut hb);
        ccp.send_secdef_request_by_symbol(101, "MSFT", "STK", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);

        // The symbol lookup answers first and asks one exchange rule.
        let msft = pipe_msg(&format!(
            "35=d|43=N|320=FixSecDefReqBySymbol101|322=*|323=4|{}146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|\
             6008=272093|306=MICROSOFT CORP|6046=BEST,AMEX,",
            listing("MSFT", "272093", "BEST", key, "4563"),
        ));
        ccp.process_ccp_message(&msft, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ccp.pending_fanout.len(), 1);
        let fid = ccp.pending_fanout[0].outstanding[0].0.clone();

        // Then the conId lookup: the contract once per exchange.
        let spy = pipe_msg(&format!(
            "35=d|43=N|320=100|322=*|323=4|{}{}{}146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|\
             6008=756733|306=SPDR S&P 500 ETF TRUST|6046=BEST,AMEX,NYSE,",
            listing("SPY", "756733", "BEST", key, "4563"),
            listing("SPY", "756733", "AMEX", "AMEX/STK#NOCROSS#LITE", "109"),
            listing("SPY", "756733", "NYSE", "NYSE/STK#NOCROSS#LITE", "110"),
        ));
        ccp.process_ccp_message(&spy, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent: Vec<String> = ccp_messages_sent(&mut server).into_iter()
            .filter(|m| m.contains("6040=106")).collect();
        assert_eq!(sent.len(), 1, "one schedule request for the one row: {sent:?}");
        assert!(sent[0].contains(&format!("6256={key}")), "{}", sent[0]);

        // The fan-out reply of the symbol lookup, then the shared schedule.
        let amex = pipe_msg(&format!("35=d|43=N|320={fid}|322=*|323=4|{}", listing("MSFT", "272093", "AMEX", "AMEX/STK", "109")));
        ccp.process_ccp_message(&amex, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.drain_contract_details().is_empty(), "both rows wait for their schedule");
        let schedule = pipe_msg(&format!("35=U|6040=107|6256={key}"));
        ccp.process_ccp_message(&schedule, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");

        let rows: Vec<(ReqId, i64, String, String)> = shared.reference.drain_contract_details().into_iter()
            .map(|(rid, d)| (rid, d.con_id, d.symbol, d.market_rule_ids)).collect();
        assert_eq!(rows, [
            (100, 756733, "SPY".to_string(), "4563,109,110".to_string()),
            (101, 272093, "MSFT".to_string(), "4563,109".to_string()),
        ]);
        let mut ends = shared.reference.drain_contract_details_end();
        ends.sort();
        assert_eq!(ends, [100, 101]);

        // A second schedule reply for the key finds nothing waiting.
        ccp.process_ccp_message(&schedule, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.drain_contract_details().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
        assert!(ccp.pending_schedule_pair.is_empty() && ccp.pending_secdef.is_empty() && ccp.pending_fanout.is_empty());
        // The contract cache keeps the lookup's own exchange.
        assert_eq!(shared.reference.get_contract(756733).map(|c| c.exchange), Some("SMART".to_string()));
    }

    #[test]
    fn a_request_multiplier_keeps_the_records_that_have_it() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let f = crate::types::SecDefFilters { multiplier: "100".into(), ..Default::default() };
        ccp.send_secdef_request_by_symbol(41, "XYZ", "OPT", "SMART", "USD", &f, &mut None, &mut HeartbeatState::new());
        let msg = pipe_msg(
            "35=d|320=41|323=4|55=XYZ|167=OPT|207=BEST|6008=1|231=100|15=USD|55=XYZ|167=OPT|207=BEST|6008=2|231=10|15=USD"
        );
        reply(&mut ccp, &mut context, &shared, &msg);
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.con_id, 1);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![41]);
    }

    #[test]
    fn a_fanout_timeout_sends_the_waiting_rows_then_end() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.send_secdef_request_by_symbol(51, "AAPL", "STK", "SMART", "USD", &Default::default(), &mut None,
            &mut HeartbeatState::new());
        reply(&mut ccp, &mut context, &shared, &stock_reply("51"));
        assert_eq!(ccp.pending_fanout.len(), 1);
        ccp.pending_fanout[0].deadline = Instant::now() - std::time::Duration::from_secs(1);

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.market_rule_ids, "4563", "the rules known so far");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![51]);
        assert!(shared.reference.drain_historical_errors().is_empty());
    }

    // ── ibx#228: matching-symbols attribution ──

    fn matching_symbols_msg(req_id: &str, symbols: &[(&str, &str)]) -> Vec<u8> {
        let count = symbols.len().to_string();
        let mut fields: Vec<(u32, &str)> = vec![
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (320, req_id),
            (146, &count), // match count — marks a data frame (even when 0)
        ];
        for (sym, con_id) in symbols {
            fields.push((55, sym));
            fields.push((167, "CS"));
            fields.push((15, "USD"));
            fields.push((6008, con_id));
        }
        crate::protocol::fix::fix_build(&fields, 1)
    }

    /// A 186 frame with no match-count tag: the not-ready ack that precedes
    /// the data frame.
    fn matching_symbols_ack(req_id: &str) -> Vec<u8> {
        crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (320, req_id),
        ], 1)
    }

    /// The event channel gets the row and then the end of a plain lookup by
    /// conId (its row waits for its schedule) and of a plain lookup by
    /// symbol (named request id), while a historical-data lookup is still
    /// waiting: that lookup is not answered by these replies.
    #[test]
    fn plain_lookups_give_their_row_and_end_on_the_event_channel() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let event_tx = Some(event_tx);
        let mut hb = HeartbeatState::new();
        ccp.pending_resolves.push(PendingResolve {
            lookup_id: HIST_LOOKUP_FIRST_ID,
            req_id: 77,
            request: crate::types::ControlCommand::Shutdown,
            deadline: Instant::now() + SECDEF_TIMEOUT,
        });
        let key = "1/STK/ARCA#LITE";
        ccp.send_secdef_request(1001, 756733, &mut None, &mut hb);
        ccp.send_secdef_request_by_symbol(1002, "AAPL", "STK", "SMART", "USD", &Default::default(), &mut None, &mut hb);

        let spy = pipe_msg(&format!(
            "35=d|43=N|320=1001|322=*|323=4|{}{}146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|\
             6008=756733|306=SPDR S&P 500 ETF TRUST|6046=BEST,ARCA,",
            listing("SPY", "756733", "BEST", key, "4563"),
            listing("SPY", "756733", "ARCA", "ARCA/STK#NOCROSS#LITE", "109"),
        ));
        ccp.process_ccp_message(&spy, &mut None, &mut context, &shared, &event_tx, &mut hb, "DU1");
        let aapl = pipe_msg(
            "35=d|43=N|320=FixSecDefReqBySymbol1002|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|6031=4563|15=USD|\
             146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|6008=265598|306=APPLE INC|6046=BEST,",
        );
        ccp.process_ccp_message(&aapl, &mut None, &mut context, &shared, &event_tx, &mut hb, "DU1");
        let schedule = pipe_msg(&format!("35=U|6040=107|6256={key}"));
        ccp.process_ccp_message(&schedule, &mut None, &mut context, &shared, &event_tx, &mut hb, "DU1");

        let events: Vec<String> = event_rx.try_iter().filter_map(|e| match e {
            Event::ContractDetails { req_id, details } => Some(format!("row:{}:{}:{}", req_id, details.symbol, details.con_id)),
            Event::ContractDetailsEnd(req_id) => Some(format!("end:{}", req_id)),
            _ => None,
        }).collect();
        assert_eq!(events, [
            "row:1002:AAPL:265598", "end:1002",
            "row:1001:SPY:756733", "end:1001",
        ]);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert_eq!(ccp.pending_resolves.len(), 1, "the historical-data lookup still waits");
        assert!(ccp.pending_secdef.is_empty() && ccp.pending_schedule_pair.is_empty());
    }

    fn u186_test_state() -> (CcpState, Context, SharedState) {
        (CcpState::new(), Context::new(), SharedState::new())
    }

    #[test]
    fn matching_symbols_matched_by_echoed_req_id_not_fifo() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));
        ccp.pending_matching_symbols.push((2, 12));

        // Request 2's reply arrives FIRST (out of order).
        let msg = matching_symbols_msg("2", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 12, "reply must land on the echoed id, not the queue head");
        assert_eq!(delivered[0].1.len(), 1);
        assert_eq!(ccp.pending_matching_symbols, vec![(1, 11)]);
    }

    #[test]
    fn matching_symbols_empty_result_pops_and_delivers() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));
        ccp.pending_matching_symbols.push((2, 12));

        // Unknown pattern: zero matches. Must still pop req 1 and deliver
        // the empty answer — previously this poisoned the queue head and
        // every later reply was off by one, forever (ibx#228).
        let msg = matching_symbols_msg("1", &[]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 11);
        assert!(delivered[0].1.is_empty(), "empty result is a legitimate answer");
        assert_eq!(ccp.pending_matching_symbols, vec![(2, 12)],
            "queue must not be poisoned by an empty result");

        // The next reply attributes correctly.
        let msg = matching_symbols_msg("2", &[("MSFT", "272093")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered[0].0, 12);
        assert!(ccp.pending_matching_symbols.is_empty());
    }

    #[test]
    fn matching_symbols_ack_frame_does_not_consume_the_request() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));

        // The not-ready ack (no tag 146) arrives first — it must not pop the
        // request; delivering it as an empty answer orphans the data frame
        // that follows (observed live, ibx#228).
        let msg = matching_symbols_ack("1");
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert_eq!(ccp.pending_matching_symbols, vec![(1, 11)]);

        // The data frame then delivers.
        let msg = matching_symbols_msg("1", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 11);
        assert_eq!(delivered[0].1.len(), 1);
        assert!(ccp.pending_matching_symbols.is_empty());
    }

    #[test]
    fn matching_symbols_unattributable_reply_is_dropped_not_misattributed() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));
        ccp.pending_matching_symbols.push((2, 12));

        // Echoed id matches nothing pending: with two in flight, guessing
        // would cross-attribute — drop with a warn instead.
        let msg = matching_symbols_msg("99", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert_eq!(ccp.pending_matching_symbols, vec![(1, 11), (2, 12)]);
    }

    fn matching_symbols_without_id(symbols: &[(&str, &str)]) -> Vec<u8> {
        let count = symbols.len().to_string();
        let mut fields: Vec<(u32, &str)> = vec![
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (146, &count),
        ];
        for (sym, con_id) in symbols {
            fields.push((55, sym));
            fields.push((6008, con_id));
        }
        crate::protocol::fix::fix_build(&fields, 1)
    }

    // ibx#369: a reply without an id is dropped, even with one request in
    // flight: never matched by position.
    #[test]
    fn matching_symbols_reply_without_id_is_dropped() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));
        let msg = matching_symbols_without_id(&[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert_eq!(ccp.pending_matching_symbols, vec![(1, 11)]);
    }

    fn sent_frames(server: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 { break; }
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&out).replace('\x01', "|")
    }

    // ibx#369: requests carry the engine's own ids, not the API reqId; the
    // reply is delivered under the API reqId.
    #[test]
    fn matching_symbols_requests_carry_own_ids() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let mut hb = HeartbeatState::new();
        ccp.send_matching_symbols_request(500, "AAPL", &mut conn, &mut hb, &shared);
        ccp.send_matching_symbols_request(501, "MSFT", &mut conn, &mut hb, &shared);
        let wire = sent_frames(&mut server);
        assert!(wire.contains("|320=1|58=AAPL|") && wire.contains("|320=2|58=MSFT|"), "{}", wire);
        assert!(!wire.contains("320=500"));
        assert_eq!(ccp.pending_matching_symbols, vec![(1, 500), (2, 501)]);
        assert!(shared.reference.drain_historical_errors().is_empty());

        let msg = matching_symbols_msg("2", &[("MSFT", "272093")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.iter().map(|d| d.0).collect::<Vec<_>>(), vec![501]);
    }

    // ibx#369: with the auth link down, or when the send fails, 10159 at
    // once and nothing kept.
    #[test]
    fn matching_symbols_not_sent_gives_10159_at_once() {
        let (mut ccp, _context, shared) = u186_test_state();
        let mut hb = HeartbeatState::new();
        ccp.send_matching_symbols_request(7, "AAPL", &mut None, &mut hb, &shared);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let _server = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        ccp.disconnected = true;
        ccp.send_matching_symbols_request(8, "AAPL", &mut conn, &mut hb, &shared);
        ccp.disconnected = false;
        conn.as_mut().unwrap().shutdown();
        ccp.send_matching_symbols_request(9, "AAPL", &mut conn, &mut hb, &shared);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, vec![
            (7, 10159, MATCHING_SYMBOLS_SEND_FAILED.to_string()),
            (8, 10159, MATCHING_SYMBOLS_SEND_FAILED.to_string()),
            (9, 10159, MATCHING_SYMBOLS_SEND_FAILED.to_string()),
        ]);
        assert!(ccp.pending_matching_symbols.is_empty());
    }

    // ibx#369: when the auth link drops, waiting requests end with no
    // answer and no error, and are not sent again.
    #[test]
    fn matching_symbols_cleared_silently_on_link_loss() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));
        ccp.handle_disconnect(&mut context, &None);
        assert!(ccp.pending_matching_symbols.is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_matching_symbols().is_empty());
        // A late reply for it is dropped.
        let msg = matching_symbols_msg("1", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(shared.reference.drain_matching_symbols().is_empty());
    }

    #[test]
    fn sweep_spares_live_entries() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let future = Instant::now() + SECDEF_TIMEOUT;
        ccp.pending_secdef.push((7, true, future));
        ccp.pending_fanout.push(PendingFanout {
            api_req_id: 9,
            outstanding: vec![("ibxfan-9-0".to_string(), 1, "IEX".to_string())],
            total: 1,
            records: Vec::new(),
            deadline: future,
        });

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert_eq!(ccp.pending_secdef.len(), 1);
        assert_eq!(ccp.pending_fanout.len(), 1);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    // A session-start recovery entry (150=0/39=0) for an order this session
    // does not know.
    fn recovery_frame(order_id: OrderId, con_id: i64) -> std::collections::HashMap<u32, String> {
        [
            (11u32, format!("{}.0", order_id)), (150, "0".into()), (39, "0".into()),
            (6008, con_id.to_string()), (55, "TEST".into()), (54, "1".into()),
            (38, "1".into()), (44, "1".into()), (40, "2".into()), (59, "1".into()),
        ].into_iter().collect()
    }

    // ibx#492: openOrder reads the price management flag the server
    // echoes, 0 without it, as the reference (ORDER-PRICEMGMT.md 5);
    // ibx#248: a reported bracket key is kept and moves the group counter.
    #[test]
    fn reports_give_the_price_management_flag_and_the_bracket_key() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut entry = recovery_frame(900_010, 1_005);
        entry.insert(8339, "1".into());
        entry.insert(6531, "7/0/-6183061".into());
        ccp.handle_exec_report(&entry, &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(900_010).expect("order info");
        assert_eq!(info.order.use_price_mgmt_algo, 1);
        assert_eq!(context.bracket_keys.get(&900_010).map(|k| k.to_string()).as_deref(), Some("7/0/-6183061"));
        assert_eq!(context.bracket_groups, 7);

        let entry = recovery_frame(900_011, 1_005);
        ccp.handle_exec_report(&entry, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(900_011).expect("order info").order.use_price_mgmt_algo, 0);
    }

    // ibx#466: an order of an earlier session is looked up by the server's
    // order id in every report; the API order id it carries only names it.
    #[test]
    fn recovered_orders_are_found_by_the_server_order_id() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut entry = recovery_frame(900_001, 1_005);
        entry.insert(6121, "15".into());
        entry.insert(6119, "7".into());
        ccp.handle_exec_report(&entry, &mut context, &shared, &None, "");
        assert!(context.order(15).is_some(), "kept under its API order id");
        assert!(context.order(900_001).is_none());

        // A later report of the server's id reaches the same order.
        let mut cancelled = recovery_frame(900_001, 1_005);
        cancelled.insert(150, "4".into());
        cancelled.insert(39, "4".into());
        cancelled.insert(11, "900001.0".into());
        ccp.handle_exec_report(&cancelled, &mut context, &shared, &None, "");
        assert_eq!(context.finished_status(15), Some(crate::types::OrderStatus::Cancelled));
        assert!(context.order(900_001).is_none(), "no second order");

        // An API order id an order of this session has is not taken.
        context.insert_order(crate::types::Order::new(16, 0, Side::Buy, 1, 0, b'2', b'0', 0));
        let mut other = recovery_frame(900_002, 1_005);
        other.insert(6121, "16".into());
        ccp.handle_exec_report(&other, &mut context, &shared, &None, "");
        assert!(context.order(900_002).is_some(), "kept under the server's id");
    }

    // ibx#285: order ids are signed; an earlier session's order with a
    // negative API order id (an auto-bound order) is kept under that id and
    // its later reports reach it, as with a positive one.
    #[test]
    fn a_negative_api_order_id_names_a_recovered_order() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut entry = recovery_frame(900_003, 1_005);
        entry.insert(6121, "-2".into());
        ccp.handle_exec_report(&entry, &mut context, &shared, &None, "");
        assert!(context.order(-2).is_some(), "kept under its API order id");
        assert!(context.order(900_003).is_none());

        let mut cancelled = recovery_frame(900_003, 1_005);
        cancelled.insert(150, "4".into());
        cancelled.insert(39, "4".into());
        ccp.handle_exec_report(&cancelled, &mut context, &shared, &None, "");
        assert_eq!(context.finished_status(-2), Some(crate::types::OrderStatus::Cancelled));
        assert!(shared.orders.drain_order_updates().iter().any(|u| u.order_id == -2));
    }

    // ibx#257: a recovered order on a new contract, with the instrument table
    // full, panicked in register() and the panic stopped the engine.
    #[test]
    fn recovery_on_a_full_instrument_table_does_not_panic() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        for i in 0..crate::types::MAX_INSTRUMENTS as i64 {
            context.market.try_register(1_000 + i).unwrap();
        }

        ccp.handle_exec_report(&recovery_frame(77, 999_999), &mut context, &shared, &None, "");
        assert!(context.order(77).is_none(), "no slot, so the order is not tracked");

        // A recovered order on a contract that already has a slot is still tracked.
        ccp.handle_exec_report(&recovery_frame(78, 1_005), &mut context, &shared, &None, "");
        let order = context.order(78).expect("tracked on its existing slot");
        assert_eq!(context.market.con_id(order.instrument), Some(1_005));
    }

    // A fill report for order 90 (BUY 300), as the server sends it: the
    // print and the order totals side by side.
    fn fill_frame(exec_id: &str, last_qty: u32, last_px: &str, cum_qty: u32, avg_px: &str, leaves: u32)
        -> std::collections::HashMap<u32, String>
    {
        let status = if leaves == 0 { "2" } else { "1" };
        [
            (11u32, "90.0".to_string()), (17, exec_id.into()), (150, status.into()), (39, status.into()),
            (55, "TEST".into()), (54, "1".into()), (38, "300".into()), (40, "2".into()), (44, "15".into()),
            (32, last_qty.to_string()), (31, last_px.into()), (14, cum_qty.to_string()),
            (6, avg_px.into()), (151, leaves.to_string()), (6008, "1005".into()),
        ].into_iter().collect()
    }

    // ibx#315: the fill carries the order totals next to the print.
    // ibx#309: the order cache's filled quantity is the filled total, not
    // the quantity still working.
    #[test]
    fn a_multi_print_fill_carries_the_order_totals() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(90, instrument, Side::Buy, 300, 15 * PRICE_SCALE, b'2', b'0', 0));

        ccp.handle_exec_report(&fill_frame("e1", 100, "10", 100, "10", 200), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fill_frame("e2", 100, "12", 200, "11", 100), &mut context, &shared, &None, "");

        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 2);
        let second = fills[1];
        assert_eq!((second.qty_fixed / crate::types::QTY_SCALE, second.price), (100, 12 * PRICE_SCALE), "the print");
        assert_eq!((second.cum_qty_fixed / crate::types::QTY_SCALE, second.avg_price), (200, 11 * PRICE_SCALE), "the order totals");
        let info = shared.orders.get_order_info(90).expect("order cached");
        assert_eq!(info.order.filled_quantity, 200.0);
    }

    // ibx#330: a final fill delivered twice is booked once, and the order
    // ends Filled and leaves the open orders.
    #[test]
    fn a_final_fill_delivered_twice_is_booked_once_and_ends_the_order() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(90, instrument, Side::Buy, 300, 15 * PRICE_SCALE, b'2', b'0', 0));

        let frame = fill_frame("e-final", 300, "10", 300, "10", 0);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");

        assert_eq!(shared.orders.drain_fills().len(), 1, "one fill booked");
        assert_eq!(context.position_fixed(instrument), 300 * crate::types::QTY_SCALE);
        assert!(context.order(90).is_none(), "the order is retired");
        assert_eq!(context.finished_status(90), Some(crate::types::OrderStatus::Filled));
        assert_eq!(shared.orders.drain_completed_orders().len(), 1);
    }

    // ibx#330: a final fill whose execution was already seen while the order
    // is still open (an earlier copy of a replay) was a return out of the
    // whole report: the order stayed open forever. The report now ends the
    // order and gives its status, with no second fill.
    #[test]
    fn a_duplicate_final_fill_still_ends_an_open_order() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(90, instrument, Side::Buy, 300, 15 * PRICE_SCALE, b'2', b'0', 0));
        assert!(ccp.record_exec_id("e-final"), "seen before");

        ccp.handle_exec_report(&fill_frame("e-final", 300, "10", 300, "10", 0), &mut context, &shared, &None, "");

        assert!(shared.orders.drain_fills().is_empty(), "not booked again");
        assert_eq!(context.position_fixed(instrument), 0);
        assert!(context.order(90).is_none(), "the order is retired");
        assert_eq!(context.finished_status(90), Some(crate::types::OrderStatus::Filled));
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1, "the status is still reported");
        assert_eq!(updates[0].status, crate::types::OrderStatus::Filled);
        assert_eq!(updates[0].filled_qty_fixed, 300 * crate::types::QTY_SCALE);
        assert_eq!(shared.orders.drain_completed_orders().len(), 1);
        let info = shared.orders.get_order_info(90).expect("order cache updated");
        assert_eq!(info.order_state.status, "Filled");
    }

    // A replayed fill of an order placed in a previous session, as the
    // server sends it at session start: its own order id, the placing
    // client's ids, and the fill time.
    fn untracked_fill_frame(exec_id: &str) -> std::collections::HashMap<u32, String> {
        [
            (11u32, "1183455398.0"), (17, exec_id), (97, "Y"), (43, "N"),
            (52, "20260928-09:04:56"), (60, "20260928-09:04:56"),
            (150, "2"), (20, "0"), (39, "2"), (167, "CS"), (55, "AAPL"), (100, "MEMX"), (207, "MEMX"),
            (38, "1"), (44, "342.22"), (32, "1"), (31, "340.52"), (14, "1"), (151, "0"), (851, "2"),
            (6, "340.52"), (54, "1"), (37, "00cf16ed.000225ed.6ab9e9f6.0001"), (1, "DU123"),
            (40, "2"), (6119, "261"), (6121, "15"), (59, "0"), (6008, "265598"), (15, "USD"),
            (6010, "ref-1"),
        ].into_iter().map(|(t, v)| (t, v.to_string())).collect()
    }

    // ibx#314: a fill of an order the engine does not track was dropped
    // (no stored execution, no position change) and its id was consumed.
    // It is now stored for req_executions and moves the position, with no
    // live fill event.
    #[test]
    fn an_untracked_fill_is_stored_and_moves_the_position() {
        let q = crate::types::QTY_SCALE;
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();

        ccp.handle_exec_report(&untracked_fill_frame("0000e0d5.6ab9ea82.01.01"), &mut context, &shared, &None, "DU123");

        assert!(shared.orders.drain_fills().is_empty(), "no live fill without a known order");
        let stored = shared.orders.drain_untracked_executions();
        assert_eq!(stored.len(), 1);
        let (contract, exec, fe) = &stored[0];
        assert_eq!(contract.con_id, 265598);
        assert_eq!(contract.symbol, "AAPL");
        assert_eq!(exec.exec_id, "0000e0d5.6ab9ea82.01.01");
        assert_eq!(exec.side, "BOT");
        assert_eq!((exec.shares, exec.price, exec.cum_qty, exec.avg_price), (1.0, 340.52, 1.0, 340.52));
        assert_eq!(exec.order_id, 15, "the placing client's order id");
        assert_eq!(exec.acct_number, "DU123");
        assert_eq!(exec.last_liquidity, 2);
        assert_eq!(fe.client_id, 261);
        assert_eq!(fe.order_ref, "ref-1");
        assert_eq!(fe.exchange, "MEMX");
        assert_eq!(fe.time_secs, fix_utc_to_unix_secs("20260928-09:04:56"));

        let instrument = context.market.instrument_by_con_id(265598).expect("contract registered");
        assert_eq!(context.position_fixed(instrument), q);
        assert_eq!(shared.portfolio.position_fixed(instrument), q);
        assert_eq!(shared.portfolio.position_info(265598).unwrap().position_fixed, q);
        assert!(ccp.seen_exec_ids.contains("0000e0d5.6ab9ea82.01.01"), "recorded once stored");
    }

    #[test]
    fn an_untracked_fill_delivered_twice_is_stored_once() {
        let q = crate::types::QTY_SCALE;
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();

        let frame = untracked_fill_frame("0000e0d5.6ab9ea82.01.01");
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "DU123");
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "DU123");

        assert_eq!(shared.orders.drain_untracked_executions().len(), 1);
        assert_eq!(shared.portfolio.position_info(265598).unwrap().position_fixed, q, "moved once");
        let instrument = context.market.instrument_by_con_id(265598).unwrap();
        assert_eq!(context.position_fixed(instrument), q);
    }

    // A sell of an untracked order moves the position down.
    #[test]
    fn an_untracked_sell_moves_the_position_down() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut frame = untracked_fill_frame("e-sell");
        frame.insert(54, "2".into());
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let (_, exec, _) = shared.orders.drain_untracked_executions().remove(0);
        assert_eq!(exec.side, "SLD");
        assert_eq!(shared.portfolio.position_info(265598).unwrap().position_fixed, -crate::types::QTY_SCALE);
    }

    // Without the side the fill is not booked, and its id stays free for
    // a later copy that carries it.
    #[test]
    fn an_untracked_fill_without_a_side_stays_replayable() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut frame = untracked_fill_frame("e-noside");
        frame.remove(&54);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_untracked_executions().is_empty());
        assert!(shared.portfolio.position_info(265598).is_none());
        assert!(!ccp.seen_exec_ids.contains("e-noside"));

        ccp.handle_exec_report(&untracked_fill_frame("e-noside"), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_untracked_executions().len(), 1);
    }

    // ibx#313: fill quantities were read as whole numbers, so a fill of a
    // fraction of a share parsed to 0 and was dropped: no fill, no position
    // change. Quantities are fixed-point (QTY_SCALE) end to end.
    #[test]
    fn a_fractional_fill_is_applied() {
        let q = crate::types::QTY_SCALE;
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        // An order of 1.5 shares listed at connect (placed in another application).
        ccp.handle_exec_report(&[
            (11u32, "93.0".to_string()), (150, "0".into()), (39, "0".into()), (6008, "1005".into()),
            (55, "TEST".into()), (54, "1".into()), (38, "1.5".into()), (44, "15".into()), (40, "2".into()),
        ].into_iter().collect(), &mut context, &shared, &None, "");
        let order = context.order(93).copied().expect("listed order tracked");
        assert_eq!(order.qty_fixed, 3 * q / 2, "1.5 shares kept");

        let fill: std::collections::HashMap<u32, String> = [
            (11u32, "93.0".to_string()), (17, "e93".into()), (150, "1".into()), (39, "1".into()),
            (55, "TEST".into()), (54, "1".into()), (38, "1.5".into()), (40, "2".into()), (44, "15".into()),
            (32, "0.5".into()), (31, "15".into()), (14, "0.5".into()), (6, "15".into()), (151, "1".into()),
            (6008, "1005".into()),
        ].into_iter().collect();
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");

        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 1, "the fill is not dropped");
        assert_eq!((fills[0].qty_fixed, fills[0].cum_qty_fixed, fills[0].remaining_fixed), (q / 2, q / 2, q));
        assert_eq!(context.order(93).unwrap().filled_fixed, q / 2);
        assert_eq!(context.position_fixed(order.instrument), q / 2, "position moved by half a share");
        assert_eq!(shared.portfolio.position_fixed(order.instrument), q / 2);
    }

    // ibx#313: a fractional position from the server is kept, in both the
    // portfolio rows and the lean position feed.
    #[test]
    fn a_fractional_position_is_kept() {
        let q = crate::types::QTY_SCALE;
        let mut context = Context::new();
        let shared = SharedState::new();
        let msg = "8=FIX.4.1|35=UP|6068=TEST|6008=1005|6064=2.5|6101=10|6065=11|15=USD|167=STK|10=000|".replace('|', "\x01");
        handle_portfolio_message(msg.as_bytes(), &mut context, &shared, &None);
        assert_eq!(shared.portfolio.position_info(1005).unwrap().position_fixed, 5 * q / 2);

        let mut ccp = CcpState::new();
        let feed = "8=FIX.4.1|35=U|6040=75|146=1|6008=1006|6064=0.25|6101=10|10=000|".replace('|', "\x01");
        ccp.handle_position_feed(feed.as_bytes(), &mut None, &mut context, &shared, &None, &mut HeartbeatState::new());
        assert_eq!(shared.portfolio.position_info(1006).unwrap().position_fixed, q / 4);
    }

    // ibx#250: a server reject reaches the caller as error 201 with the
    // server's reason (ib-agent#192 C1), once, and only for an order this
    // session tracks.
    #[test]
    fn a_server_reject_is_reported_as_error_201_with_the_reason() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(91, instrument, Side::Buy, 1000, 15 * PRICE_SCALE, b'2', b'0', 0));
        let frame = |order: &str, status: &str| -> std::collections::HashMap<u32, String> {
            let mut m: std::collections::HashMap<u32, String> = [
                (11u32, format!("{}.0", order)), (150, status.to_string()), (39, status.to_string()),
                (55, "TEST".into()), (38, "1000".into()), (151, "1000".into()),
            ].into_iter().collect();
            if status == "8" {
                m.insert(58, "Display size should be a multiple of lot size".into());
            }
            m
        };

        ccp.handle_exec_report(&frame("91", "A"), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty(), "no error on the acknowledgement");
        ccp.handle_exec_report(&frame("91", "8"), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(), vec![
            (91, 201, "Order rejected - reason:Display size should be a multiple of lot size".to_string()),
        ]);

        // A repeat, and a reject for an order this session does not track.
        ccp.handle_exec_report(&frame("91", "8"), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&frame("92", "8"), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // req_global_cancel sends a cancel-all for each id below the shared
    // instrument count. A recovered order on a new contract took a slot
    // without raising that count, so the global cancel never reached it.
    #[test]
    fn recovered_order_is_inside_the_global_cancel_range() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        context.market.try_register(265598).unwrap();
        shared.market.set_instrument_count(context.market.count());

        ccp.handle_exec_report(&recovery_frame(79, 756733), &mut context, &shared, &None, "");

        let order = context.order(79).copied().expect("recovered order tracked");
        let count = shared.market.instrument_count();
        assert!(order.instrument < count, "instrument {} outside 0..{}", order.instrument, count);
        for instrument in 0..count {
            context.cancel_all(instrument);
        }
        assert!(context.pending_orders.drain().any(|r| matches!(r,
            crate::types::OrderRequest::CancelAll { instrument } if instrument == order.instrument)));
    }

    // ibx#475: account frames captured on paper 25/09/2026 (account masked,
    // rows shortened), '|' for SOH.
    const UT_IMAGE: &str = "8=O|9=000000|35=UT|6529=AR.1|8001=AccountType|6066=1790323947|8004=INDIVIDUAL|6288=0|8001=AccountCode|6066=1790323947|8004=DU0000001|6288=0|8001=AccountReady|6066=1790323947|8004=true|6288=0|8001=DepositOnCreditHold|6066=1790323947|6288=0|8001=Leverage-S|6066=1790323947|8004=0.07|6288=0|8001=SettledCashByDate|8002=BASE|6066=1790323947|8004=20260925:932758.72933742;20260928:899133.49933742|6288=0|";
    const UM_IMAGE: &str = "8=O|9=000000|35=UM|6529=AR.1|8001=AccruedCash|15=USD|6066=1790323947|6288=0|8004=1893.50|8001=FullMaintMarginReq|15=USD|6066=1790323947|6288=0|8004=11647.75|8001=MaintMarginReq|15=USD|6066=1790323947|6288=0|8004=11647.75|8001=NetLiquidation|15=USD|6066=1790323947|6288=0|8004=953633.06|";
    const RL_IMAGE: &str = "8=O|9=000000|35=RL|6529=AR.1|8001=LedgerList|8002=BASE|15=BASE|9806=899133.4993|9819=953633.0601|9818=899133.4993|6711=0.0000|9807=52606.06|9808=0.00|9809=0.00|8007=0.00|9810=0.00|6099=26.57|6100=-791.71|9820=1|6242=1893.5|6066=1790338477|6288=0|8001=LedgerList|8002=USD|15=USD|9806=899133.4993|9819=953633.0601|9818=899133.4993|9807=52606.06|9808=0.00|9809=0.00|9810=0.00|6099=26.57|6100=-791.71|9820=1|6242=1893.5|6066=1790338477|6288=0|";
    const UT_PERIODIC: &str = "8=O|9=000105|35=UT|6529=AR.1|8001=AccountCode|6066=1790338657|8004=|8001=Cushion|6066=1790338657|8004=0.987782|6288=0|";

    fn soh(s: &str) -> String { s.replace('|', "\x01") }

    fn rows_of(frame: &str) -> Vec<(String, String, String)> {
        parse_account_rows(&soh(frame)).0
    }

    fn row(k: &str, c: &str, v: &str) -> (String, String, String) {
        (k.into(), c.into(), v.into())
    }

    #[test]
    fn account_rows_keep_the_key_the_text_and_the_currency() {
        let ut = rows_of(UT_IMAGE);
        assert_eq!(ut, [
            row("AccountType", "", "INDIVIDUAL"),
            row("AccountCode", "", "DU0000001"),
            row("AccountReady", "", "true"),
            row("Leverage-S", "", "0.07"),
            row("SettledCashByDate", "", "20260925:932758.72933742;20260928:899133.49933742"),
        ], "a row with no value is skipped; UT rows have no currency");
        let um = rows_of(UM_IMAGE);
        assert_eq!(um, [
            row("AccruedCash", "USD", "1893.50"),
            row("FullMaintMarginReq", "USD", "11647.75"),
            row("MaintMarginReq", "USD", "11647.75"),
            row("NetLiquidation", "USD", "953633.06"),
        ], "each key as sent, the Full keys too");
        assert_eq!(parse_account_rows(&soh(UM_IMAGE)).1, Some(1790323947));
    }

    #[test]
    fn ledger_rows_give_the_keys_per_currency() {
        let rl = rows_of(RL_IMAGE);
        assert!(rl.contains(&row("Currency", "BASE", "BASE")));
        assert!(rl.contains(&row("CashBalance", "BASE", "899133.4993")));
        assert!(rl.contains(&row("NetLiquidationByCurrency", "USD", "953633.0601")));
        assert!(rl.contains(&row("RealizedPnL", "USD", "26.57")));
        assert!(rl.contains(&row("UnrealizedPnL", "BASE", "-791.71")));
        assert!(rl.contains(&row("ExchangeRate", "USD", "1")));
        assert!(!rl.iter().any(|(k, _, _)| k == "LedgerList"));
        assert_eq!(rl.iter().filter(|(_, c, _)| c == "USD").count(), 12);
    }

    // The periodic frame's empty AccountCode is skipped: no AccountType
    // before it in the frame.
    #[test]
    fn a_periodic_frame_skips_the_empty_account_code() {
        assert_eq!(rows_of(UT_PERIODIC), [row("Cushion", "", "0.987782")]);
    }

    #[test]
    fn a_pnl_row_ends_the_frame() {
        let frame = "8=O|35=UM|6529=AR.1|8001=NetLiquidation|15=USD|8004=1|8001=PNL|15=USD|8004=2|8001=Cushion|8004=3|";
        assert_eq!(rows_of(frame), [row("NetLiquidation", "USD", "1")]);
    }

    #[test]
    fn the_end_marker_completes_the_image_of_the_account_stream_only() {
        let (mut context, shared) = (Context::new(), SharedState::new());
        handle_account_update(soh(UM_IMAGE).as_bytes(), &mut context, &shared);
        assert_eq!(shared.portfolio.account_rows_generation().1, false);
        handle_account_end(soh("8=O|9=000016|35=EB|6529=SR.3|").as_bytes(), &shared);
        assert_eq!(shared.portfolio.account_rows_generation().1, false, "another subscription's end");
        handle_account_end(soh("8=O|9=000016|35=EB|6529=AR.1|").as_bytes(), &shared);
        let (_, complete, time) = shared.portfolio.account_rows_generation();
        assert!(complete);
        assert_eq!(time, 1790323947);
        assert_eq!(shared.portfolio.account_rows().rows.len(), 4);
    }

    // ibx#479: frames of an account summary subscription (6529=SR.*) go to
    // that request only, not to the account stream or the account state.
    #[test]
    fn summary_frames_go_to_their_subscription_only() {
        let (mut context, shared) = (Context::new(), SharedState::new());
        let um = UM_IMAGE.replace("6529=AR.1", "6529=SR.Socket.7");
        handle_account_update(soh(&um).as_bytes(), &mut context, &shared);
        let rl = RL_IMAGE.replace("6529=AR.1", "6529=SR.Socket.7");
        handle_account_update(soh(&rl).as_bytes(), &mut context, &shared);
        handle_account_end(soh("8=O|9=000016|35=EB|6529=SR.Socket.7|").as_bytes(), &shared);

        assert!(shared.portfolio.account_rows().rows.is_empty(), "not the account stream");
        assert_eq!(shared.portfolio.account().net_liquidation, 0, "not the account state");
        let events = shared.portfolio.drain_account_summary_events();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.sr_id == "SR.Socket.7"));
        assert_eq!(events[0].rows.len(), 4);
        assert!(!events[0].ledger && events[1].ledger && events[2].end);
        assert!(events[1].rows.iter().any(|r| r.key == "CashBalance" && r.currency == "USD"));
    }

    // ibx#478: the realized P&L of a fill of this session (6099 of its
    // commission frame, 5.645252 on the closing SELL seen on paper
    // 25/09/2026) adds to the position's realized P&L; a higher revision
    // replaces the amount; a fill of an earlier session is not counted.
    #[test]
    fn a_fill_realized_pnl_adds_to_its_position() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "00025b49.6ab659f3.01.01"),
            (31, "770.56"), (32, "1"), (14, "1"), (151, "0"), (6, "770.56")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        // The fill's cash, for the daily P&L: a buy of 1 at 770.56.
        let cash = shared.portfolio.money_since_seed().get(&756733).copied().unwrap();
        assert!((cash + 770.56).abs() < 1e-9, "cash={cash}");
        // The position moves with the fill; a new position takes the fill
        // price as average cost.
        let pi = shared.portfolio.position_info(756733).unwrap();
        assert_eq!(pi.position_fixed, QTY_SCALE);
        assert_eq!(pi.avg_cost, 77056 * PRICE_SCALE / 100);
        ccp.handle_commission_report(&commission_frame(&[(17, "00025b49.6ab659f3.01.01"), (6099, "5.645252")]), &shared);
        assert_eq!(shared.portfolio.realized_since_seed().get(&756733).copied(), Some(5.645252));

        ccp.handle_commission_report(&commission_frame(&[(17, "00025b49.6ab659f3.01.02"), (6099, "6.0")]), &shared);
        let total = shared.portfolio.realized_since_seed().get(&756733).copied().unwrap();
        assert!((total - 6.0).abs() < 1e-9, "the revision replaces: {total}");

        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01"), (6099, "3.0")]), &shared);
        let total = shared.portfolio.realized_since_seed().get(&756733).copied().unwrap();
        assert!((total - 6.0).abs() < 1e-9, "an earlier session's fill is not counted");
    }

    // ibx#465: every cancel of an order of this session gives 202 with the
    // server's reason (empty for a user cancel, captured 25/09/2026).
    #[test]
    fn a_cancel_gives_202_with_the_reason() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        let done = exec_report_frame(&[(39, "4"), (150, "4")]);
        ccp.handle_exec_report(&done, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(), [(42, 202, "Order Canceled - reason:".to_string())]);

        let (mut ccp, mut context, shared) = ord_status_test_state();
        let done = exec_report_frame(&[(39, "4"), (150, "4"), (58, "Order expired")]);
        ccp.handle_exec_report(&done, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(), [(42, 202, "Order Canceled - reason:Order expired".to_string())]);
    }

    // ibx#465: the captured order message of an OPG order before the open.
    #[test]
    fn a_time_order_message_gives_399_once() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        shared.reference.cache_contract(265598, api::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        shared.reference.cache_market_name(265598, "NMS");
        let text = "Warning: your order will not be placed at the exchange until 2026-09-25 09:30:00 US/Eastern";
        let ack = exec_report_frame(&[(39, "A"), (150, "A"), (20, "3"), (54, "1"), (38, "1"), (55, "AAPL"),
            (6008, "265598"), (59, "2"), (6360, "TIME"), (6361, text)]);
        ccp.handle_exec_report(&ack, &mut context, &shared, &None, "");
        ccp.handle_exec_report(&ack, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(),
            [(42, 399, format!("Order Message: BUY 1 AAPL NASDAQ.NMS {}", text))], "once");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PreSubmitted, "the order keeps working");

        // Another message type did not reach the API in the capture.
        let price_cap = exec_report_frame(&[(39, "A"), (150, "A"), (6360, "PRICECAP"), (6361, "long text")]);
        ccp.handle_exec_report(&price_cap, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ib-agent#194: the server reports the offset of a TRAIL LIMIT set by
    // its limit price (6370 = trailStopPrice - lmtPrice); it is kept for the
    // replace and shown as lmtPriceOffset.
    #[test]
    fn the_server_trail_limit_offset_is_kept() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL"), (6370, "5"), (44, "745.66"), (6117, "750.66")]);
        ccp.handle_exec_report(&ack, &mut context, &shared, &None, "");
        assert_eq!(context.trail_limit_reported.get(&42).copied(), Some(crate::engine::context::TrailLimitReported {
            offset: 5 * PRICE_SCALE, limit: 74566 * PRICE_SCALE / 100, stop: 75066 * PRICE_SCALE / 100,
        }));
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.lmt_price_offset, 5.0);
        assert_eq!(info.order.lmt_price, 745.66);
        assert_eq!(info.order.trail_stop_price, 750.66);

        // A short report without them (seen on paper 25/09/2026) keeps them.
        let short = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL")]);
        ccp.handle_exec_report(&short, &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.lmt_price_offset, 5.0);
        assert_eq!(info.order.lmt_price, 745.66);
        assert_eq!(info.order.trail_stop_price, 750.66);

        // ibx#491: the server moves the stop and the limit with the market
        // (ib-agent#195: 6117=752.01, 44=747.01); the moved values show.
        let moved = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL"), (6370, "5"), (44, "747.01"), (6117, "752.01")]);
        ccp.handle_exec_report(&moved, &mut context, &shared, &None, "");
        ccp.handle_exec_report(&short, &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.trail_stop_price, 752.01);
        assert_eq!(info.order.lmt_price, 747.01);
    }

    // ibx#467: a report with 59=1 and the DTC flag is a DTC order.
    #[test]
    fn a_report_with_the_dtc_flag_is_dtc() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "2"), (59, "1"), (6436, "1")]);
        ccp.handle_exec_report(&ack, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(42).unwrap().order.tif, "DTC");
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "2"), (59, "1")]);
        ccp.handle_exec_report(&ack, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(42).unwrap().order.tif, "GTC");
    }

    // ibx#307: the report's time in force as the reference reads it: DAY
    // when 59 is absent, DTC with the DTC flag, the overnight values, and
    // an unknown code kept as "???" (not DAY).
    #[test]
    fn the_report_time_in_force_is_read_as_the_reference() {
        let tif = |fields: &[(u32, &str)]| {
            let (mut ccp, mut context, shared) = ord_status_test_state();
            let mut all = vec![(39, "0"), (150, "0"), (40, "2")];
            all.extend_from_slice(fields);
            let frame = exec_report_frame(&all);
            ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
            shared.orders.get_order_info(42).unwrap().order.tif
        };
        assert_eq!(tif(&[(59, "1")]), "GTC");
        assert_eq!(tif(&[(59, "5")]), "GTX");
        assert_eq!(tif(&[(59, "1"), (6436, "1")]), "DTC");
        assert_eq!(tif(&[(59, "0"), (8534, "1")]), "OVERNIGHT + DAY");
        assert_eq!(tif(&[(59, "0"), (6004, "OVERNIGHT")]), "OVERNIGHT");
        assert_eq!(tif(&[(59, "Z")]), "???");
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;

    fn tag_order(msg: &[u8]) -> Vec<u32> {
        msg.split(|&b| b == fix::SOH)
            .filter_map(|f| f.iter().position(|&b| b == b'=').map(|i| &f[..i]))
            .filter_map(|t| std::str::from_utf8(t).ok()?.parse().ok())
            .collect()
    }

    fn build(fields: &[(u32, String)]) -> Vec<u8> {
        let refs: Vec<(u32, &str)> = fields.iter().map(|(t, v)| (*t, v.as_str())).collect();
        fix::fix_build(&refs, 4)
    }

    fn frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        pairs.iter().map(|(t, v)| (*t, v.to_string())).collect()
    }

    // ibx#399: after a reconnect the fills since the last execution of the
    // session are asked for, in the reference form.
    #[test]
    fn fill_up_request_after_a_known_execution() {
        let mut ccp = CcpState::new();
        ccp.last_exec = Some(("00025b49.6abe16e9.01.01.01".into(), "20260930-19:03:49".into()));
        let fields = ccp.fill_up_request("DU1", "20260930-19:05:26");
        let msg = build(&fields);
        assert_eq!(tag_order(&msg), [8, 9, 35, 34, 52, 6040, 6536, 6537, 6556, 6538, 1, 6539, 17, 10]);
        let parsed = fix::fix_parse(&msg);
        assert_eq!(parsed[&35], "U");
        assert_eq!(parsed[&6040], "72");
        assert_eq!(parsed[&6536], "20260930-19:03:49");
        assert_eq!(parsed[&6537], "20260930-19:05:26");
        assert_eq!(parsed[&6556], "todayfillup5");
        assert_eq!(parsed[&6538], "1");
        assert_eq!(parsed[&1], "DU1");
        assert_eq!(parsed[&6539], "20260930-19:03:49");
        assert_eq!(parsed[&17], "00025b49.6abe16e9.01.01.01", "no commission seen: no suffix");

        // With its commission report, the execution id carries the suffix;
        // the request number runs on.
        assert!(ccp.record_commission("00025b49.6abe16e9.01.01.01"));
        let parsed = fix::fix_parse(&build(&ccp.fill_up_request("DU1", "20260930-19:05:27")));
        assert_eq!(parsed[&17], "00025b49.6abe16e9.01.01.01-CM");
        assert_eq!(parsed[&6556], "todayfillup6");
    }

    #[test]
    fn fill_up_request_without_an_execution_asks_for_the_day() {
        let mut ccp = CcpState::new();
        let msg = build(&ccp.fill_up_request("DU1", "20260930-19:05:26"));
        assert_eq!(tag_order(&msg), [8, 9, 35, 34, 52, 6040, 6536, 6537, 6556, 10]);
        let parsed = fix::fix_parse(&msg);
        assert_eq!(parsed[&6536], "20260930-00:00:00");
        assert_eq!(parsed[&6556], "today5");
    }

    #[test]
    fn status_replay_request_asks_for_every_working_order() {
        let msg = fix::fix_build(&status_replay_request("20260930-19:05:26"), 5);
        assert_eq!(tag_order(&msg), [8, 9, 35, 34, 52, 11, 55, 54, 10]);
        let parsed = fix::fix_parse(&msg);
        assert_eq!((parsed[&35].as_str(), parsed[&11].as_str(), parsed[&55].as_str(), parsed[&54].as_str()), ("H", "*", "*", "*"));
    }

    // A new fill of the session is the start of the next fill-up.
    #[test]
    fn a_new_fill_is_the_last_execution() {
        let mut context = Context::new();
        let instrument = context.register_instrument(265598);
        context.insert_order(crate::types::Order::new(42, instrument, Side::Buy, 1, 100 * PRICE_SCALE, b'2', b'0', 0));
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let fill = frame(&[(11, "42"), (20, "0"), (39, "2"), (150, "2"), (97, "Y"), (17, "00025b49.6abe16e9.01.01.01"),
            (31, "254.2"), (32, "1"), (14, "1"), (151, "0"), (6, "254.2"),
            (52, "20260930-19:05:27"), (60, "20260930-19:04:31")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "DU1");
        assert_eq!(shared.orders.drain_fills_with_exec().len(), 1, "a fill of the gap takes the normal path");
        assert_eq!(ccp.last_exec, Some(("00025b49.6abe16e9.01.01.01".into(), "20260930-19:04:31".into())));
        // The same execution again is a duplicate and changes nothing.
        ccp.last_exec = None;
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "DU1");
        assert!(shared.orders.drain_fills_with_exec().is_empty());
        assert_eq!(ccp.last_exec, None);
    }

    // ibx#399: the end markers of both replies create no order and no fill.
    #[test]
    fn end_markers_create_no_order() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.awaiting_status_replay = true;

        let trades_end = frame(&[(43, "N"), (52, "20260930-19:05:27"), (6556, "todayfillup88"),
            (17, "140781.1790795127.0"), (32, "*"), (150, "0"), (39, "0"), (6008, "265598"), (38, "1")]);
        ccp.handle_exec_report(&trades_end, &mut context, &shared, &None, "DU1");
        assert!(ccp.status_replay_end_at.is_none(), "not the status replay end");

        shared.orders.set_open_orders_held(true);
        let status_end = frame(&[(11, "*"), (55, "*"), (37, "*"), (20, "3"), (150, "0"), (39, "0"), (6008, "265598"), (38, "1")]);
        ccp.handle_exec_report(&status_end, &mut context, &shared, &None, "DU1");
        assert!(!shared.orders.open_orders_held(), "open-order requests are answered from here (ibx#251)");

        assert!(context.order(0).is_none());
        assert_eq!(context.market.count(), 0, "no instrument registered for a marker");
        assert!(shared.orders.drain_fills_with_exec().is_empty());
        assert!(shared.orders.drain_order_updates().is_empty());
        assert!(!ccp.awaiting_status_replay);
        assert!(ccp.status_replay_end_at.is_some(), "the status replay end is seen");
    }

    // Outside a reconnect the status replay end marks nothing.
    #[test]
    fn status_replay_end_outside_a_reconnect_marks_nothing() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        shared.orders.set_open_orders_held(true);
        ccp.handle_exec_report(&frame(&[(11, "*"), (55, "*")]), &mut context, &shared, &None, "DU1");
        assert!(ccp.status_replay_end_at.is_none());
        assert!(shared.orders.open_orders_held());
    }

    // ibx#251: a lost auth link leaves every order with its status, and
    // nothing is reported for the orders.
    #[test]
    fn link_loss_keeps_the_order_status() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
        let instrument = context.market.try_register(1005).unwrap();
        for (id, status) in [(90, OrderStatus::Submitted), (91, OrderStatus::PendingCancel), (92, OrderStatus::PreSubmitted)] {
            context.insert_order(crate::types::Order::new(id, instrument, Side::Buy, 1, 15 * PRICE_SCALE, b'2', b'0', 0));
            context.set_order_status_forced(id, status);
        }
        ccp.handle_disconnect(&mut context, &None);
        assert_eq!(context.order(90).unwrap().status, OrderStatus::Submitted);
        assert_eq!(context.order(91).unwrap().status, OrderStatus::PendingCancel);
        assert_eq!(context.order(92).unwrap().status, OrderStatus::PreSubmitted);
        assert!(shared.orders.drain_order_updates().is_empty(), "no status at the drop");
    }

    // ibx#251: each replayed report of a known order gives its status, set
    // as the server reports it, back to working too.
    #[test]
    fn replay_reports_set_the_status_as_reported() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
        let instrument = context.market.try_register(1005).unwrap();
        for id in [90, 91] {
            context.insert_order(crate::types::Order::new(id, instrument, Side::Buy, 1, 15 * PRICE_SCALE, b'2', b'0', 0));
        }
        context.set_order_status_forced(90, OrderStatus::Submitted);
        context.set_order_status_forced(91, OrderStatus::PendingCancel);
        ccp.handle_disconnect(&mut context, &None);
        ccp.awaiting_status_replay = true;

        let working = |id: &str| frame(&[(11, id), (20, "3"), (150, "0"), (39, "0"), (37, "57311390"),
            (100, "NASDAQ"), (14, "0"), (151, "1"), (6008, "1005"), (38, "1")]);
        ccp.handle_exec_report(&working("90.0"), &mut context, &shared, &None, "DU1");
        ccp.handle_exec_report(&working("91.0"), &mut context, &shared, &None, "DU1");
        ccp.handle_exec_report(&frame(&[(11, "*"), (55, "*")]), &mut context, &shared, &None, "DU1");

        assert_eq!(context.order(91).unwrap().status, OrderStatus::Submitted, "the server's status wins");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(),
            vec![(90, OrderStatus::Submitted), (91, OrderStatus::Submitted)], "one status per replayed order");

        // After the replay the status guard applies again.
        context.set_order_status_forced(91, OrderStatus::PendingCancel);
        ccp.handle_exec_report(&working("91.0"), &mut context, &shared, &None, "DU1");
        assert_eq!(context.order(91).unwrap().status, OrderStatus::PendingCancel);
    }

    // ── ibx#427: a historical request without conId looks the contract up ──
    mod contract_resolve {
        use super::*;
        use crate::control::contracts::tests::pipe_msg;
        use crate::types::{ContractLookup, ControlCommand};

        fn bars(req_id: ReqId) -> ControlCommand {
            ControlCommand::FetchHistorical {
                req_id, con_id: 0, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
                end_date_time: String::new(), duration: "1 D".into(), bar_size: "1 hour".into(),
                what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: false, include_expired: true,
            }
        }

        fn lookup() -> ContractLookup {
            ContractLookup { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() }
        }

        #[test]
        fn one_contract_releases_the_request_with_its_con_id() {
            let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
            ccp.start_contract_resolve(5, lookup(), bars(5), &mut None, &mut HeartbeatState::new());
            assert_eq!(ccp.pending_resolves[0].lookup_id, HIST_LOOKUP_FIRST_ID);
            // The reply lists the contract once per exchange: one contract.
            let reply = pipe_msg("35=d|43=N|320=FixSecDefReqBySymbol3489660928|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|\
                55=AAPL|167=STK|207=NASDAQ|6008=265598|15=USD");
            ccp.process_ccp_message(&reply, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
            assert!(ccp.pending_resolves.is_empty());
            assert!(shared.reference.drain_historical_errors().is_empty());
            assert!(shared.reference.drain_contract_details().is_empty(), "not a contract details answer");
            match ccp.resolved_requests.as_slice() {
                [ControlCommand::FetchHistorical { req_id: 5, con_id: 265598, include_expired: true, symbol, .. }] =>
                    assert_eq!(symbol, "AAPL"),
                other => panic!("{:?}", other),
            }
        }

        #[test]
        fn none_or_several_contracts_give_200_and_no_query() {
            let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
            ccp.start_contract_resolve(6, lookup(), bars(6), &mut None, &mut HeartbeatState::new());
            ccp.start_contract_resolve(7, lookup(), bars(7), &mut None, &mut HeartbeatState::new());
            let none = pipe_msg("35=d|43=N|320=FixSecDefReqBySymbol3489660928|322=*|323=4");
            let two = pipe_msg("35=d|43=N|320=FixSecDefReqBySymbol3489660929|322=*|323=4|55=MNQ|167=FUT|207=CME|6008=815824267|15=USD|\
                55=MNQ|167=FUT|207=CME|6008=840227399|15=USD");
            ccp.process_ccp_message(&none, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
            ccp.process_ccp_message(&two, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
            assert!(ccp.resolved_requests.is_empty());
            assert_eq!(shared.reference.drain_historical_errors(), vec![
                (6, 200, NO_SECURITY_DEFINITION.to_string()),
                (7, 200, NO_SECURITY_DEFINITION.to_string()),
            ]);
        }

        #[test]
        fn only_replies_of_its_own_range_are_taken() {
            let (mut ccp, shared) = (CcpState::new(), SharedState::new());
            ccp.start_contract_resolve(9, lookup(), bars(9), &mut None, &mut HeartbeatState::new());
            // Same low bits in the market data and internal ranges, and a
            // caller lookup number: none of them is this lookup's reply.
            let reply = |id: &str| pipe_msg(&format!("35=d|43=N|320={}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD", id));
            for other in ["FixSecDefReqBySymbol3758096384", "FixSecDefReqBySymbol4026531840", "FixSecDefReqBySymbol0", "ibxlot0"] {
                assert!(!ccp.contract_resolve_reply(other, &reply(other), &shared), "{}", other);
            }
            assert_eq!(ccp.pending_resolves.len(), 1);
            assert!(HIST_LOOKUP_FIRST_ID + HIST_LOOKUP_IDS <= crate::engine::hot_loop::farm::MD_LOOKUP_FIRST_ID);
        }

        #[test]
        fn a_lookup_with_no_answer_gives_200() {
            let (mut ccp, shared) = (CcpState::new(), SharedState::new());
            ccp.start_contract_resolve(8, lookup(), bars(8), &mut None, &mut HeartbeatState::new());
            ccp.pending_resolves[0].deadline = Instant::now() - std::time::Duration::from_secs(1);
            ccp.sweep_contract_resolves(&shared);
            assert!(ccp.pending_resolves.is_empty());
            let errors = shared.reference.drain_historical_errors();
            assert_eq!((errors[0].0, errors[0].1), (8, 200));
        }

        #[test]
        fn every_historical_request_kind_gets_the_con_id() {
            let kinds = vec![
                ControlCommand::FetchHeadTimestamp { req_id: 1, con_id: 0, sec_type: String::new(), exchange: String::new(), what_to_show: "TRADES".into(), use_rth: true },
                ControlCommand::FetchHistogramData { req_id: 2, con_id: 0, sec_type: String::new(), exchange: String::new(), use_rth: true, period: "1 week".into() },
                ControlCommand::FetchHistoricalTicks { req_id: 3, con_id: 0, sec_type: String::new(), exchange: String::new(), start_date_time: String::new(), end_date_time: String::new(), number_of_ticks: 10, what_to_show: "TRADES".into(), use_rth: true },
                ControlCommand::FetchHistoricalSchedule { req_id: 4, con_id: 0, sec_type: String::new(), exchange: String::new(), end_date_time: String::new(), duration: "1 M".into(), use_rth: true },
                ControlCommand::FetchFundamentalData { req_id: 5, con_id: 0, report_type: "ReportSnapshot".into() },
            ];
            for cmd in kinds {
                let got = format!("{:?}", request_with_con_id(cmd, 265598));
                assert!(got.contains("con_id: 265598"), "{}", got);
            }
        }
    }
}
