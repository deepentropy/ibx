use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::bridge::{Event, RichOrderInfo, SharedState};
use crate::api::types as api;
use crate::engine::context::Context;
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::types::{
    CompletedOrder, Fill, MidnightSeed, NewsBulletin, OrderId, ReqId,
    PositionInfo, Price, Qty, Side, PRICE_SCALE, QTY_SCALE,
};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, clone_for_event, parse_price_tag, parse_qty, decode_tif};

/// API error 10159 of a matching-symbols request that could not be sent
/// (ibx#369).
pub(crate) const MATCHING_SYMBOLS_SEND_FAILED: &str =
    "Failed to request matching symbols:Error sending message to a CCP.";
/// Head of the API error 10159 of a matching-symbols reply that carries an
/// error text: the text follows (ibx#369).
const MATCHING_SYMBOLS_FAILED: &str = "Failed to request matching symbols:";
/// Pause after each matching-symbols send, as the reference (ibx#369).
const MATCHING_SYMBOLS_SEND_GAP: std::time::Duration = std::time::Duration::from_millis(1000);

/// How long an internal contract lookup of ibx (cache fill, scanner
/// enrichment: ids from 0xF000_0000) waits for its answer. A caller's
/// contract details request has no deadline, as in the reference: its
/// contract definition requests (with a 6 s sweep) give up only for an
/// order's lookup (30 s), and the API request waits for its answer
/// (ibx#485).
const SECDEF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The gap between two fan-out requests, as the reference sends them
/// (ibx#689; about 2,670 a second).
const FANOUT_SEND_GAP: std::time::Duration = std::time::Duration::from_micros(375);

/// The API order type of an order report's type code and instructions:
/// the codes ibx's order messages write, read back; the pegged code is a
/// relative order, a midpoint or a market peg by its instruction, a
/// trailing stop otherwise (the reference 10.51, 19/10/2026 pre-market,
/// paper, ibx#604: STP PRT, RPI, LIT, TRAIL LIMIT, MIDPRICE, SNAP MKT,
/// SNAP MID, PEG BENCH, TRAIL MIT, TRAIL LIT, REL, PEG MID, LOC, PEG BEST
/// among the completed orders and the orders of another client). A code
/// not known is kept as it is.
pub(crate) fn report_order_type<'a>(code: &'a str, instructions: &str) -> &'a str {
    match code {
        "P" => match instructions.split(' ').find(|i| matches!(*i, "R" | "M" | "P")) {
            Some("R") => "REL",
            Some("M") => "PEG MID",
            Some(_) => "PEG MKT",
            None => "TRAIL",
        },
        "1" => "MKT", "2" => "LMT", "3" => "STP", "4" => "STP LMT",
        "5" => "MOC", "B" => "LOC", "J" => "MIT",
        "K" => "MTL", "R" => "REL", "U" => "MKT PRT", "SP" => "STP PRT", "LT" => "LIT", "TSL" => "TRAIL LIMIT",
        "MIDPX" => "MIDPRICE", "SMKT" => "SNAP MKT", "SMID" => "SNAP MID", "SREL" => "SNAP PRIM", "PB" => "PEG BENCH",
        "TMIT" => "TRAIL MIT", "TLIT" => "TRAIL LIT", "E2M" => "PEG BEST", "PSVR" => "PASSV REL", "RPI" => "RPI",
        other => other,
    }
}

/// Whether a lookup is an internal one of ibx, not a caller's request.
fn internal_lookup(req_id: ReqId) -> bool {
    req_id >= 0xF000_0000
}
/// Error 200 text for a lookup that found no contract (ibx#400).
pub(crate) const NO_SECURITY_DEFINITION: &str = "No security definition has been found for the request";

/// Number of most-recent ExecIDs retained for fill deduplication. Bounds the
/// memory of `seen_exec_ids` while staying large enough that a server replay
/// after a reconnect burst still hits the window.
const EXEC_ID_WINDOW: usize = 1024;

/// Commission reports held for the `Fill` of their execution, on the
/// event channel (ibx#644): the day's earlier executions replayed at the
/// login never get one, so the oldest are dropped.
const HELD_COMMISSION_EVENTS: usize = 64;

/// Convert a server order id hex string (e.g. "00cf16ed.000225ed.69ca0941.0001") to a stable i64 permId.
/// Uses FNV-1a hash of the first 3 dot-segments (the stable prefix) so that permId
/// remains constant across modifications (the last segment increments on each modify).
/// Extract the value of a single field from a raw wire message.
/// `prefix` should include the field's key and `=` (e.g. the schedule join key field).
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

/// A quantity as the reference writes it in an order message: the whole
/// part in groups of three digits ("5,000,000" for 5000000, paper
/// 11/10/2026; "1" and "100" unchanged in every recording).
fn grouped_quantity(quantity: &str) -> String {
    let (whole, rest) = quantity.split_at(quantity.find('.').unwrap_or(quantity.len()));
    if whole.len() < 4 || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return quantity.to_string();
    }
    let mut out = String::with_capacity(quantity.len() + whole.len() / 3);
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out.push_str(rest);
    out
}

/// Text of warning 399 for an order message (ibx#465): "Order Message:
/// {action} {quantity} {symbol} {exchange}
/// {text}", three lines (the API
/// message of the four-leg recordings, ibx#486). The exchange is the
/// listing of the contract's definition: the primary exchange (6470) and
/// its suffix (8224) when the definition has one (NASDAQ.NMS for AAPL,
/// ARCA for SPY, which has none: the recordings of 26/09 to 02/10/2026),
/// else the order's exchange. A combo
/// shows its own symbol and "Combo" (captured 26/09/2026,
/// i105_combo_stock_smart and i105_combo_leg_prices: "BUY 1 QQQ,SPY Combo"
/// where the report says 55=IECombo; ibx#487).
fn order_message_399(
    parsed: &fix::FieldMap<'_>,
    text: &str,
    context: &Context,
    clord_id: OrderId,
    shared: &SharedState,
    combo: Option<&api::Contract>,
) -> String {
    let action = match parsed.get(&54).copied() {
        Some("1") => "BUY",
        Some("5") => "SSHORT",
        Some(_) => "SELL",
        None => match context.order(clord_id).map(|o| o.side) {
            Some(Side::Buy) => "BUY",
            Some(Side::ShortSell) => "SSHORT",
            _ => "SELL",
        },
    };
    let quantity = grouped_quantity(parsed.get(&38).copied().unwrap_or(""));
    if let Some(c) = combo {
        return format!("Order Message:
{} {} {} Combo
{}", action, quantity, c.symbol, text);
    }
    let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
    // A structured product or a treasury bill by the reference's own
    // text from its definition (ibx#666); a bill gives the order's amount
    // in place of its quantity.
    if let Some(t) = context.rth_types.iter().find(|((c, _), t)| *c == con_id && con_id > 0 && !t.order_text.is_empty()).map(|(_, t)| t) {
        let amount = match t.face_value {
            Some(face) => {
                let total = parsed.get(&38).and_then(|q| q.parse::<f64>().ok()).unwrap_or(0.0) * face as f64;
                if total > 0.0 && total % 1000.0 == 0.0 { format!("${}K", total / 1000.0) } else { format!("${}", total) }
            }
            None => quantity,
        };
        return format!("Order Message:
{} {} {}
{}", action, amount, t.order_text, text);
    }
    // A currency pair by its pair and "Forex" (ibx#688; the reference 10.51,
    // paper, 19/10/2026: "BUY 1 EUR.USD Forex").
    if parsed.get(&167).copied() == Some("CASH")
        && let Some(pair) = parsed.get(&6035).copied().filter(|p| !p.is_empty())
    {
        return format!("Order Message:
{} {} {} Forex
{}", action, quantity, pair, text);
    }
    let cached = shared.reference.get_contract(con_id);
    let symbol = parsed.get(&55).map(|s| s.to_string())
        .or_else(|| cached.as_ref().map(|c| c.symbol.clone()))
        .unwrap_or_default();
    let primary = cached.as_ref().map(|c| c.primary_exchange.clone()).filter(|p| !p.is_empty());
    let exchange = match (primary, shared.reference.market_name(con_id)) {
        (Some(p), Some(m)) if !p.ends_with(&format!(".{m}")) => format!("{}.{}", p, m),
        (Some(p), _) => p,
        (None, _) => parsed.get(&207).or_else(|| parsed.get(&6004))
            .map(|e| crate::control::contracts::exchange_from_fix(e).to_string())
            .unwrap_or_default(),
    };
    format!("Order Message:\n{} {} {} {}\n{}", action, quantity, symbol, exchange, text)
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

/// The report of a combo's leg (ibx#470): 6013 gives the leg index (-1
/// for the combo itself) and the leg count, 442 = 2 on a leg's report.
fn is_leg_report(parsed: &fix::FieldMap<'_>) -> bool {
    match parsed.get(&6013).and_then(|v| v.split(':').next()).and_then(|i| i.parse::<i32>().ok()) {
        Some(index) => index >= 0,
        None => parsed.get(&442).copied() == Some("2"),
    }
}

/// Every value of `tag` in a message, in wire order.
fn all_values(msg: &[u8], tag: u32) -> impl Iterator<Item = &str> {
    let prefix = format!("{tag}=");
    msg.split(|&b| b == fix::SOH)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter_map(move |p| p.strip_prefix(prefix.as_str()))
}

/// What a fill report says about its execution beyond the fill numbers.
fn fill_exec_of(parsed: &fix::FieldMap<'_>, exec_id: &str) -> crate::bridge::FillExec {
    let tag = |t: u32| parsed.get(&t).copied().filter(|s| !s.is_empty());
    crate::bridge::FillExec {
        exec_id: exec_id.to_string(),
        time_secs: tag(6699).or_else(|| tag(60)).or_else(|| tag(52))
            .and_then(|s| fix_utc_to_unix_secs(s)),
        // A smart route is shown as SMART, as on the report of a combo
        // (captured 30/09/2026: 100=BEST, execDetails exchange SMART).
        exchange: tag(100).or_else(|| tag(207))
            .map(|e| if e == "BEST" { "SMART".to_string() } else { e.to_string() })
            .unwrap_or_default(),
        client_id: tag(6119).and_then(|s| s.parse().ok()).unwrap_or(0),
        model_code: tag(6700).map(str::to_string).unwrap_or_default(),
        order_ref: tag(6010).map(str::to_string).unwrap_or_default(),
        last_liquidity: tag(851).and_then(|s| s.parse().ok()).unwrap_or(0),
        combo: None,
        other_client: false,
        replayed: parsed.get(&97).is_some_and(|v| *v == "Y"),
        position_after: None,
    }
}

/// The permId of a report's order: the id part of its ClOrdID (11), as
/// the reference gives it (captured
/// 01/10/2026: openOrder permId 1790865742870063 for 11=1790865742870063.0).
/// 0 for none.
/// The rows of an answer of reqCompletedOrders, then its end (ibx#695);
/// with apiOnly, the orders placed through the API only.
fn push_completed_rows(rows: &[CompletedOrderRow], api_only: bool, shared: &SharedState) {
    for (contract, order, state, api) in rows {
        if api_only && !api {
            continue;
        }
        shared.orders.push_completed_row(Some(Box::new((contract.clone(), order.clone(), state.clone()))));
    }
    shared.orders.push_completed_row(None);
    shared.notify();
}

/// A finished order as the reference's completedOrder shows it, from the
/// server's replayed report (ibx#695; paper, 15/10/2026 pre-market, 64
/// orders: the contract with its conId, the smart route as SMART and its
/// local symbol; the permanent id, no API order id or client; the status,
/// "Cancelled by Trader" for a cancel, and the time of the report in
/// US/Eastern).
fn completed_row(parsed: &fix::FieldMap<'_>, account_id: &str) -> CompletedOrderRow {
    let get = |t: u32| parsed.get(&t).map(|s| s.to_string()).unwrap_or_default();
    let num = |t: u32| parsed.get(&t).and_then(|s| s.parse::<f64>().ok());
    let flag = |t: u32| parsed.get(&t).is_some_and(|s| *s == "1");
    let sec_type = match get(167).as_str() {
        "CS" | "COMMON" => "STK".to_string(),
        "FOR" => "CASH".to_string(),
        other => other.to_string(),
    };
    let exchange = parsed.get(&6004).or_else(|| parsed.get(&6210)).filter(|e| !e.is_empty())
        .map(|e| crate::control::contracts::exchange_from_fix(e).to_string())
        .unwrap_or_default();
    let contract = api::Contract {
        con_id: parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0),
        symbol: get(55),
        sec_type,
        exchange,
        currency: get(15),
        local_symbol: get(6035),
        ..Default::default()
    };
    let order_type = report_order_type(&get(40), &get(18)).to_string();
    let order = api::Order {
        action: match get(54).as_str() { "1" => "BUY", "2" => "SELL", "5" => "SSHORT", _ => "" }.to_string(),
        // The quantity not filled: 0 for a filled order (the reference's
        // answer of 07/10/2026: two filled orders of 1 share show 0, the
        // cancelled ones their quantity).
        total_quantity: (num(38).unwrap_or(0.0) - num(14).unwrap_or(0.0)).max(0.0),
        order_type,
        lmt_price: num(44).unwrap_or(0.0),
        aux_price: num(99).unwrap_or(0.0),
        tif: decode_tif(super::report_tif(parsed)).to_string(),
        account: { let a = get(1); if a.is_empty() { account_id.to_string() } else { a } },
        perm_id: perm_id_of(parsed),
        filled_quantity: num(14).unwrap_or(0.0),
        outside_rth: flag(6433),
        post_only: flag(6605),
        deactivate: flag(6521),
        clearing_intent: get(6419),
        use_price_mgmt_algo: i32::from(flag(8339)),
        order_ref: get(6010),
        seek_price_improvement: flag(6557),
        // A bracket child: its OCA group and its parent's permanent id.
        oca_group: get(583),
        parent_perm_id: parsed.get(&6107).and_then(|s| s.split('.').next()).and_then(|s| s.parse().ok()).unwrap_or(0),
        auto_cancel_parent: flag(6965),
        // A hedge order's hedge, as the reference shows it (ibx#614;
        // paper, 16/10/2026): the letter of its kind, the beta or the
        // ratio as a decimal number ("0.5", "1.0", "2.0"), the max size
        // of a beta hedge (unset `i32::MAX`).
        hedge_type: hedge_letter(&get(6665)).to_string(),
        hedge_param: num(6703).or_else(|| num(6666)).map(hedge_param_text).unwrap_or_default(),
        hedge_max_size: parsed.get(&6690).and_then(|v| v.parse().ok()).unwrap_or(i32::MAX),
        // The stop trigger, which the reference also shows as the trailing
        // stop price of a stop order.
        trail_stop_price: num(6117).unwrap_or(f64::MAX),
        good_till_date: match (parsed.get(&126), parsed.get(&432)) {
            (Some(t), _) => utc_text_to_eastern(t),
            (None, Some(d)) => d.to_string(),
            _ => String::new(),
        },
        good_after_time: parsed.get(&168).map(|t| utc_text_to_eastern(t)).unwrap_or_default(),
        rule80a: "0".into(),
        ..Default::default()
    };
    // A rejected order is cancelled, "Rejected by System" (the reference
    // 10.51, paper, 15/10/2026: three margin refusals).
    let status = match get(39).as_str() {
        "2" => "Filled",
        _ => "Cancelled",
    };
    let completed_status = match get(39).as_str() {
        "2" => "Filled".to_string(),
        "8" => format!("Rejected by System:\n{}", reworded_reject(&get(58))),
        // Cancelled with a restatement reason (a bracket's children when
        // the parent ends): by the system (five orders on 15/10/2026).
        _ if parsed.contains_key(&378) => "Cancelled by System:\n".to_string(),
        _ => parsed.get(&58).filter(|t| !t.is_empty()).map(|t| t.to_string()).unwrap_or_else(|| "Cancelled by Trader".to_string()),
    };
    let completed_time = parsed.get(&60).or_else(|| parsed.get(&52))
        .and_then(|t| jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", t).ok())
        .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
        .map(|z| crate::client_core::format_exec_time(z.timestamp().as_second()))
        .unwrap_or_default();
    let state = api::OrderState {
        status: status.to_string(),
        completed_time,
        completed_status,
        ..Default::default()
    };
    let api = parsed.get(&6088).is_some_and(|s| *s == "Socket");
    (contract, order, state, api)
}

/// A server time ("yyyyMMdd-HH:mm:ss", UTC) as the reference shows an
/// order's dates: "yyyyMMdd HH:mm:ss US/Eastern" (ibx#695).
fn utc_text_to_eastern(text: &str) -> String {
    jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", text).ok()
        .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
        .map(|z| crate::client_core::format_exec_time(z.timestamp().as_second()))
        .unwrap_or_else(|| text.to_string())
}

/// The text of a refusal as the reference words it in a completed order's
/// status (ibx#695; observed for the margin refusal, three orders on
/// 15/10/2026). Another text is kept as the server gave it.
fn reworded_reject(text: &str) -> String {
    let values: Vec<&str> = text.split('[').skip(1).filter_map(|s| s.split(']').next()).collect();
    if text.starts_with("YOUR ORDER IS NOT ACCEPTED. IN ORDER TO OBTAIN THE DESIRED POSITION YOUR EQUITY WITH LOAN VALUE")
        && let [loan, margin] = values[..]
    {
        return format!("We are unable to accept your order. Your Available Funds are in sufficient to cover the change in the <br>\
account's margin requirements if this order executes. In order to obtain the desired position your Equity with<br> \
Loan Value [{loan}] must exceed the new total Initial Margin of  [{margin}].");
    }
    text.to_string()
}

fn perm_id_of(parsed: &fix::FieldMap<'_>) -> i64 {
    parsed.get(&11)
        .map(|s| s.strip_prefix('C').unwrap_or(s))
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Whether the answer known for the trading permission of an order's
/// contract class says no permission (ibx#666).
fn no_class_permission(context: &Context, order_id: OrderId) -> bool {
    let Some(con_id) = context.order(order_id).and_then(|o| context.market.con_id(o.instrument)) else { return false };
    context.rth_types.iter()
        .filter(|((c, _), t)| *c == con_id && !t.permission_key.is_empty())
        .any(|(_, t)| context.trading_permissions.get(&t.permission_key) == Some(&false))
}

/// The API order id a report gives (6121), 0 when it gives none
/// (the reference reads an absent one as the int maximum, shown as 0).
fn report_api_order_id(parsed: &fix::FieldMap<'_>) -> OrderId {
    parsed.get(&6121).and_then(|s| s.parse::<OrderId>().ok())
        .filter(|&id| id != i32::MAX as OrderId)
        .unwrap_or(0)
}

/// Note the API order id (6121) of a report for its API client (6119, 0
/// when absent): the reference keeps the highest order id each client used,
/// and moves it for every order it shows the client. Only positive ids:
/// the reference keeps negative ones apart.
fn note_reported_order_id(parsed: &fix::FieldMap<'_>, shared: &SharedState) {
    let id = report_api_order_id(parsed);
    if id <= 0 || id >= i32::MAX as OrderId {
        return;
    }
    let client = parsed.get(&6119).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    shared.orders.note_reported_order_id(client, id);
}

/// The execution of a fill report, as stored for `req_executions`.
fn report_execution(
    parsed: &fix::FieldMap<'_>,
    exec_id: &str,
    side: Side,
    last_shares: Qty,
    last_px: f64,
    account_id: &str,
    order_id: OrderId,
) -> api::Execution {
    api::Execution {
        exec_id: exec_id.to_string(),
        acct_number: parsed.get(&1).filter(|s| !s.is_empty()).map(|s| s.to_string())
            .unwrap_or_else(|| account_id.to_string()),
        side: match side { Side::Buy => "BOT", Side::Sell | Side::ShortSell => "SLD" }.to_string(),
        shares: last_shares as f64 / QTY_SCALE as f64,
        price: last_px,
        perm_id: perm_id_of(parsed),
        order_id,
        cum_qty: parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(0) as f64 / QTY_SCALE as f64,
        avg_price: parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        last_liquidity: parsed.get(&851).and_then(|s| s.parse().ok()).unwrap_or(0),
        ..Default::default()
    }
}

/// The API order id of a report's parent (ibx#329): the parent link
/// carries the parent's order id with its version; the id part is mapped
/// back to the API order id of the order the server holds under it (an
/// order recovered from another session is kept under its API id). 0 when
/// the report has no parent. The OCA group is not a parent.
fn parent_order_id(parsed: &fix::FieldMap<'_>, context: &Context) -> i64 {
    let Some(link) = parsed.get(&6107).filter(|s| !s.is_empty()) else {
        return 0;
    };
    let id_part = link.split('.').next().unwrap_or(link);
    let Ok(id) = id_part.parse::<OrderId>() else {
        return 0;
    };
    if context.recovered_keys.contains_key(&id) {
        return context.key_of(id);
    }
    let same_id = |clord: &String| clord.split('.').next() == Some(id_part);
    if context.order(id).is_some() || context.last_clord.get(&id).is_some_and(same_id) {
        return id;
    }
    context.last_clord.iter()
        .find(|(_, clord)| same_id(clord))
        .map_or(id, |(&order_id, _)| order_id)
}

/// The parent of a report's order: the report's parent link, or the
/// parent the order is known to have when the report has no link (ibx#509:
/// in the bracket recordings of 26/09/2026 some reports of a child carry
/// no link, and the reference's orderStatus names the parent all the same).
fn known_parent_id(parsed: &fix::FieldMap<'_>, context: &Context, order_id: OrderId) -> i64 {
    match parent_order_id(parsed, context) {
        0 => context.book.get(&order_id).map_or(0, |e| e.parent),
        parent => parent,
    }
}

/// The delay of the P&L partition's account request after the first P&L
/// request (ibx#511).
pub(crate) const PNL_PARTITION_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

pub(crate) struct CcpState {
    /// The group of each account summary subscription, which its cancel
    /// restates (ibx#486).
    pub(crate) summary_groups: std::collections::HashMap<String, String>,
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
    /// The executions the event channel gave a `Fill` for, without their
    /// revision, oldest first (ibx#644): only their commission reports go
    /// to the event channel. Bounded like `seen_exec_ids`.
    pub(crate) fill_events: HashSet<String>,
    pub(crate) fill_event_order: VecDeque<String>,
    /// Commission reports that came before the `Fill` of their execution,
    /// given on the event channel after it (ibx#644). Oldest dropped first.
    pub(crate) held_commission_events: VecDeque<(String, api::CommissionAndFeesReport)>,
    /// conId of this session's fills and the realized P&L counted for each,
    /// by execution id without its revision (ibx#478). Bounded like the
    /// commission window.
    pub(crate) exec_realized: std::collections::HashMap<String, (i64, f64)>,
    /// Order messages already reported as 399, by order and text (ibx#465).
    pub(crate) order_messages_sent: HashMap<OrderId, Vec<String>>,
    /// The per-leg prices (6879) of the report being read, in leg order
    /// (ibx#470): the parsed map keeps one value per tag.
    pub(crate) report_leg_prices: Vec<f64>,
    pub(crate) exec_realized_order: VecDeque<String>,
    pub(crate) disconnected: bool,
    /// (req_id, is_single_shot). Single-shot = known-conId lookup whose
    /// first security definition answer is also the last (server emits no
    /// last-answer response type for these). Multi-record by-symbol/matching-symbols requests push
    /// `false` and rely on the response-type sentinel for is_last.
    /// In-flight secdef requests: (req_id, single_shot, deadline). Only an
    /// internal lookup is dropped at its deadline (`sweep_contract_details`);
    /// a caller's request waits for its answer, as in the reference
    /// (ibx#485).
    pub(crate) pending_secdef: Vec<(ReqId, bool, Instant)>,
    /// By-symbol lookups in flight: the request multiplier and the strike
    /// retry (ibx#410, ibx#435).
    pub(crate) pending_lookups: Vec<PendingLookup>,
    /// CONTFUT and FUT+CONTFUT requests in flight (ibx#438).
    pub(crate) pending_continuous: Vec<ContinuousLookup>,
    /// Known market rule per (conId, exchange), from the records of every
    /// definition reply; a fan-out asks only for the unknown ones (ibx#435).
    pub(crate) market_rule_by_exchange: std::collections::HashMap<(i64, String), u32>,
    /// Industry, category and subcategory of an underlying, from the
    /// company lookups of derivative rows (ibx#436).
    pub(crate) company_by_underlying: std::collections::HashMap<i64, (String, String, String)>,
    /// Company lookups in flight: request id sent, underlying conId.
    pub(crate) pending_company: Vec<(String, i64)>,
    /// Underlyings of contract details rows not known when their company
    /// lookup was sent: its answer asks their preferred definition next
    /// (ibx#733).
    pub(crate) preferred_after_company: HashSet<i64>,
    /// Those preferred definition lookups in flight: request id sent,
    /// underlying conId (ibx#733).
    pub(crate) pending_preferred: Vec<(String, i64)>,
    /// Contract details answers waiting for the company lookup of their
    /// underlying and, when asked, its preferred definition (ibx#733).
    pub(crate) held_details: Vec<HeldDetails>,
    /// Matching-symbols requests sent and not answered: (own request id
    /// sent, API reqId). No deadline, as in the reference; cleared without
    /// an answer when the auth link drops (ibx#369).
    pub(crate) pending_matching_symbols: Vec<(u32, ReqId)>,
    /// Next own request id of a matching-symbols request (ibx#369).
    pub(crate) next_matching_symbols_id: u32,
    /// The matching-symbols request waiting to be sent: only the latest
    /// one is kept, a replaced one gets no answer, as in the reference
    /// (ibx#369).
    pub(crate) matching_waiting: Option<(ReqId, String)>,
    /// Send permits of the matching-symbols pacing (ibx#369): one request
    /// in flight until its pending mark or its answer; the loss of the
    /// auth link gives one back.
    pub(crate) matching_permits: u32,
    /// No matching-symbols request goes out before this time: 1 s after
    /// each send (ibx#369).
    pub(crate) matching_next_send: Option<Instant>,
    /// Requests whose pending mark came (their permit was given back).
    pub(crate) matching_acked: Vec<u32>,
    /// Data permission stamp of the last logon reply or logon update
    /// (6764, ibx#421).
    pub(crate) data_permissions: Option<String>,
    /// The feature list of the last logon update has DENYAPI (ibx#421).
    pub(crate) deny_api: bool,
    /// A logon update or a relogin changed the data permission stamp: the
    /// hot loop runs the reference's permission change (ibx#421).
    pub(crate) permissions_changed: bool,
    /// The SSL farm list (8449) of the last logon update, for the hot
    /// loop (ibx#276); None when no update came since it was taken.
    pub(crate) ssl_farms_update: Option<String>,
    /// HMAC signing key for XML-carrying CCP messages (selective signing).
    pub(crate) ccp_sign_key: Vec<u8>,
    /// HMAC signing IV — advances only for signed messages, independent of unsigned ones.
    pub(crate) ccp_sign_iv: std::sync::Mutex<Vec<u8>>,
    /// Secdef replies awaiting paired schedule reply (joined by the schedule key field).
    pub(crate) pending_schedule_pair: Vec<PendingSchedulePair>,
    /// Trading schedules received this session, by join key (ibx#527): a
    /// row whose schedule is known does not wait and asks nothing. Each
    /// with the UTC day it came: it is asked again on another day, since a
    /// schedule lists a few days only (how long the reference keeps one
    /// is not known).
    pub(crate) schedules: std::collections::HashMap<String, (jiff::civil::Date, Option<crate::control::contracts::ContractSchedule>)>,
    /// The trading hours asked for the orders' option models, by schedule
    /// key (ibx#730): asked once.
    pub(crate) model_schedules: HashSet<String>,
    /// The contracts of orders waiting for the zone of their trading
    /// hours, by schedule key (ibx#636).
    pub(crate) order_zone_waits: Vec<(String, i64)>,
    /// The auto cancel date each order's reports gave last, by order
    /// (ibx#683): the one the order holds, which its openOrder shows only
    /// from a status report or a modify on.
    pub(crate) held_cancel_dates: std::collections::HashMap<crate::types::OrderId, String>,
    /// Fan-out state for by-symbol lookups: the records wait for the
    /// per-exchange replies, then become the rows (ibx#435).
    pub(crate) pending_fanout: Vec<PendingFanout>,
    /// The per-exchange requests of the fan-outs waiting for their turn
    /// (ibx#689): (lookup's request id, request id, conId, exchange).
    fanout_queue: std::collections::VecDeque<(ReqId, String, i64, String)>,
    /// When the queue started to drain and how many have left since.
    fanout_clock: Option<(Instant, u64)>,
    /// The fan-out's requests leave at the reference's pace (on for a
    /// session opened by a logon, as the other copied waits).
    pub(crate) fanout_paced: bool,
    /// reqCompletedOrders (ibx#695).
    pub(crate) completed: CompletedOrders,
    /// Counter for internal fan-out req IDs (the security request id of the
    /// per-exchange security definition requests).
    pub(crate) next_fanout_id: u32,
    /// Counter for internal secdef req IDs (auto-fetch on cold-cache positions).
    pub(crate) next_internal_secdef_id: u32,
    /// Option calculations and their model inputs (ibx#442).
    pub(crate) optcalc: super::optcalc::OptCalc,
    /// Option chain parameter requests and their answers (ibx#440).
    pub(crate) optparams: super::optparams::OptParams,
    /// conIds we've already auto-fetched secdef for, keyed by con_id (dedup).
    pub(crate) auto_fetched_conids: HashSet<i64>,
    /// Scanner results awaiting per-conId contract-detail enrichment.
    /// Each entry parks a parsed `<ScanResponse>` until every cache-miss
    /// con_id has been resolved via the same security definition answer path
    /// that user-initiated
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
    /// The account request of the P&L partition (ibx#511): due 1 s after
    /// the first P&L request of the session, then sent once.
    pub(crate) pnl_partition_due: Option<Instant>,
    /// Its subscription id once sent: its answers feed the partition, not
    /// the account stream.
    pub(crate) pnl_partition_id: Option<String>,
    /// Running number of the partition's account requests.
    pub(crate) next_partition_request: u32,
    /// The P&L seeds were asked again for the account's "Core" model
    /// (ibx#714).
    core_seeds_asked: bool,
    /// A portfolio message was read: the P&L seeds' contracts that no
    /// portfolio row carries can be told apart (ibx#742).
    portfolio_seen: bool,
    /// Set by a reconnect until the end frame of the order status replay.
    pub(crate) awaiting_status_replay: bool,
    /// Set at the logon until the end frame of its order status replay:
    /// open-order requests wait for it, as in the reference (ibx#251).
    pub(crate) awaiting_login_replay: bool,
    /// The request id (6556) of the trades request sent after a reconnect,
    /// until the end frame of its reply: the fills of the outage (ibx#251).
    pub(crate) fill_up_pending: Option<String>,
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
}

/// Request numbers of the contract lookups of historical-data requests
/// (ibx#427): a range of their own, below the market data lookups
/// (0xE000_0000) and the internal lookups (0xF000_0000).
pub(crate) const HIST_LOOKUP_FIRST_ID: u32 = 0xD000_0000;
pub(crate) const HIST_LOOKUP_IDS: u32 = 0x1000_0000;

/// `request` with its contract conId set (ibx#427).
pub(crate) fn request_with_con_id(mut request: crate::types::ControlCommand, con_id: i64) -> crate::types::ControlCommand {
    use crate::types::ControlCommand as C;
    match &mut request {
        C::FetchHistorical { con_id: c, .. }
        | C::FetchHeadTimestamp { con_id: c, .. }
        | C::FetchHistogramData { con_id: c, .. }
        | C::FetchHistoricalTicks { con_id: c, .. }
        | C::FetchHistoricalSchedule { con_id: c, .. }
        | C::FetchFundamentalData { con_id: c, .. }
        | C::SubscribeTbt { con_id: c, .. }
        | C::SubscribeRealTimeBar { con_id: c, .. }
        | C::SubscribeDepth { con_id: c, .. }
        | C::CalcOption { con_id: c, .. } => *c = con_id,
        _ => {}
    }
    request
}

/// Scanner result parked for contract-detail fan-out.
pub(crate) struct PendingScannerEnrichment {
    pub api_req_id: ReqId,
    pub result: crate::control::scanner::ScannerResult,
    pub awaiting: HashSet<i64>,
    pub deadline: Instant,
}

fn utc_day() -> jiff::civil::Date {
    jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
}

/// A contract row waiting for its trading schedule. The lookup's
/// contract_details_end follows once none of its rows waits.
pub(crate) struct PendingSchedulePair {
    pub api_req_id: ReqId,
    pub join_key: String,
    pub def: crate::control::contracts::ContractDefinition,
    pub deadline: Instant,
    /// Its schedule came (or its time ran out); it leaves once no earlier
    /// row of its lookup waits (ibx#527).
    pub ready: bool,
}

/// The requests that waited for the company lookup of `under` go back to
/// the command loop (ibx#733).
fn release_company_parked(context: &mut Context, under: i64) {
    if context.company_parked.is_empty() {
        return;
    }
    let (ready, parked): (Vec<_>, Vec<_>) = std::mem::take(&mut context.company_parked).into_iter().partition(|(u, _)| *u == under);
    context.company_parked = parked;
    context.def_ready.extend(ready.into_iter().map(|(_, request)| request));
}

/// A contract details answer whose rows wait for the company lookup of
/// their underlying and, when asked, its preferred definition (ibx#733):
/// its records as read, and whether its per-exchange lookups follow.
pub(crate) struct HeldDetails {
    pub req_id: ReqId,
    pub records: Vec<crate::control::contracts::ContractDefinition>,
    pub by_symbol: bool,
}

/// In-flight by-symbol fan-out: the per-exchange requests sent for the
/// records of a lookup whose market rule on that exchange is not known.
/// Their replies only fill the market rules (ibx#435). The records wait
/// here and leave as rows in the lookup's order, each once its own
/// requests are answered and the rows before it are given, as the
/// reference 10.51 gives a whole option chain (ibx#689).
pub(crate) struct PendingFanout {
    pub api_req_id: ReqId,
    /// Requests not answered yet: (request id, conId, exchange).
    pub outstanding: Vec<(String, i64, String)>,
    /// The place of each request id in `outstanding`: a chain asks tens of
    /// thousands (ibx#689).
    index: HashMap<String, usize>,
    /// Number of requests sent.
    pub total: usize,
    /// Its requests still waiting for their turn: no row leaves before
    /// the last is sent, as the reference (ibx#689).
    unsent: usize,
    /// The records of the lookup not given yet, in reply order.
    pub records: std::collections::VecDeque<crate::control::contracts::ContractDefinition>,
    /// The requests not answered yet of each conId.
    left: HashMap<i64, u32>,
    /// Idle deadline of an internal lookup, refreshed on every
    /// per-exchange reply; a caller's lookup waits for every reply, as in
    /// the reference (ibx#485).
    pub deadline: Instant,
}

impl PendingFanout {
    pub(crate) fn new(
        api_req_id: ReqId, outstanding: Vec<(String, i64, String)>,
        records: Vec<crate::control::contracts::ContractDefinition>, deadline: Instant,
    ) -> Self {
        let index = outstanding.iter().enumerate().map(|(i, (id, _, _))| (id.clone(), i)).collect();
        let mut left: HashMap<i64, u32> = HashMap::new();
        for (_, con_id, _) in &outstanding {
            *left.entry(*con_id).or_default() += 1;
        }
        Self { api_req_id, total: outstanding.len(), outstanding, index, records: records.into(), left, deadline, unsent: 0 }
    }

    /// The records at the front whose requests are all answered, taken
    /// out in order: the rows that can be given now (ibx#689).
    fn ready_rows(&mut self) -> Vec<crate::control::contracts::ContractDefinition> {
        let mut out = Vec::new();
        if self.unsent > 0 {
            return out;
        }
        while self.records.front().is_some_and(|d| self.left.get(&d.con_id).is_none_or(|n| *n == 0)) {
            out.extend(self.records.pop_front());
        }
        out
    }

    fn waits_for(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    /// The answered request, taken out: its conId and exchange.
    fn answered(&mut self, id: &str) -> Option<(i64, String)> {
        let pos = self.index.remove(id)?;
        let (_, con_id, exchange) = self.outstanding.swap_remove(pos);
        if let Some((moved, _, _)) = self.outstanding.get(pos) {
            self.index.insert(moved.clone(), pos);
        }
        if let Some(n) = self.left.get_mut(&con_id) {
            *n = n.saturating_sub(1);
        }
        Some((con_id, exchange))
    }
}

/// A row of reqCompletedOrders: the order's contract, the order, its state,
/// and whether it was placed through the API (ibx#695).
type CompletedOrderRow = (api::Contract, api::Order, api::OrderState, bool);

/// reqCompletedOrders, as the reference 10.51 makes it (ibx#695; paper,
/// 15/10/2026 pre-market): one request to the server for the finished
/// orders of the trading day, its rows given once the contracts they name
/// are known; a later request of the session is answered from that answer.
#[derive(Default)]
pub(crate) struct CompletedOrders {
    /// The answer being read.
    reading: Option<Vec<CompletedOrderRow>>,
    /// Its end came: it waits for the definitions of `defs` until
    /// `deadline`.
    ended: bool,
    defs: HashSet<i64>,
    /// Definitions to ask at the next sweep, which has the connection.
    to_ask: Vec<i64>,
    /// The request ids of those lookups, until their answer.
    lookups: HashSet<ReqId>,
    deadline: Option<Instant>,
    /// The apiOnly flags of the requests waiting for the answer.
    waiting: Vec<bool>,
    /// The answer, kept for the session's later requests.
    kept: Option<Vec<CompletedOrderRow>>,
}

/// How long the answer waits for the definitions of its contracts.
const COMPLETED_DEFS_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// The continuous future lookup of a CONTFUT request (ibx#438).
    pub continuous: bool,
}

/// A CONTFUT or FUT+CONTFUT request (ibx#438): the continuous future
/// lookup goes first and its records are kept; with FUT+CONTFUT the
/// futures lookup follows, and its rows come after the kept ones.
pub(crate) struct ContinuousLookup {
    pub req_id: ReqId,
    pub with_futures: bool,
    /// The records of the continuous future reply, once it came.
    pub kept: Option<Vec<crate::control::contracts::ContractDefinition>>,
}

/// The value of the lead-futures-only tag of a continuous future lookup,
/// the current lead future (captured 02/10/2026: the value 2).
const CURRENT_LEAD_FUTURE: &str = "2";

/// How an API security type reads for a contract lookup, as the reference
/// decodes it (ibx#438): the security type to ask, whether a continuous
/// future lookup is made, and whether the futures lookup is made too. A
/// bond issuer id makes the lookup one for fixed income.
fn lookup_sec_type<'a>(sec_type: &'a str, filters: &crate::types::SecDefFilters) -> (&'a str, bool, bool) {
    if !filters.issuer_id.trim().is_empty() {
        return ("FIXED", false, false);
    }
    match sec_type {
        "CONTFUT" => ("FUT", true, false),
        "FUT+CONTFUT" | "CONTFUT+FUT" => ("FUT", true, true),
        other => (other, false, false),
    }
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
    // The continuous future lookup is a copy of the request without its
    // source and without expired contracts, as the reference (ibx#438).
    if strike.is_empty() && !lookup.continuous {
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
    } else if lookup.continuous {
        // Continuous future (ibx#438): the symbol and trading class, then
        // the futures fields with the current lead future.
        let symbol = lookup_symbol(&lookup.symbol);
        if !symbol.is_empty() {
            fields.push((TAG_SYMBOL, symbol.into_owned()));
        }
        if !f.trading_class.is_empty() {
            fields.push((8362, f.trading_class.clone()));
        }
        fields.push((167, "FUT".into()));
        let expiry = &f.last_trade_date_or_contract_month;
        if expiry.eq_ignore_ascii_case("NOEXP") {
            fields.push((541, "NOEXP".into()));
        } else if expiry.len() == 6 {
            fields.push((200, expiry.clone()));
        } else if expiry.len() > 6 {
            fields.push((541, expiry.clone()));
        }
        fields.push((6857, CURRENT_LEAD_FUTURE.into()));
        if !f.multiplier.is_empty() {
            fields.push((231, f.multiplier.clone()));
        }
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
    // Exchange and primary exchange are two fields (ibx#229); an empty
    // field is not written, as the reference (ibx#438).
    let exchange = if lookup.exchange == "SMART" { "BEST" } else { lookup.exchange.as_str() };
    if !exchange.is_empty() {
        fields.push((100, exchange.into()));
    }
    if !identifier && !f.primary_exchange.is_empty() {
        fields.push((207, f.primary_exchange.clone()));
    }
    if !lookup.currency.is_empty() {
        fields.push((15, lookup.currency.clone()));
    }
    // The bond issuer, last (ibx#438).
    if !identifier && !f.issuer_id.is_empty() {
        fields.push((6454, f.issuer_id.clone()));
    }
    fields
}

/// Write one by-symbol lookup on `conn` (ibx#410; also the contract lookup
/// of a historical request, ibx#427). A market data request's is asked as
/// an order's (ibx#731).
pub(crate) fn send_symbol_lookup_on(conn: &mut Connection, req_id: ReqId, lookup: &SymbolLookup, strike: &str) {
    send_lookup_body(conn, &secdef_by_symbol_fields(req_id, lookup, strike));
}

/// The by-symbol lookup of an order's contract (ibx#486): the source is
/// written whatever the terms, and an option's strike after its right, as
/// the reference asks it (ibx#666; paper 14/10/2026: an AAPL call given by
/// its description, looked up with its expiry, right, strike and
/// multiplier).
pub(crate) fn order_lookup_fields(req_id: ReqId, lookup: &SymbolLookup) -> Vec<(u32, String)> {
    let mut fields = secdef_by_symbol_fields(req_id, lookup, "");
    let strike = lookup.filters.strike_text();
    if !strike.is_empty() {
        let after_type = fields.iter().position(|(t, _)| *t == 167).map_or(fields.len(), |i| i + 1);
        let at = fields[after_type..].iter().position(|(t, _)| ![541, 200, 201].contains(t))
            .map_or(fields.len(), |i| after_type + i);
        fields.insert(at, (202, strike));
    }
    fields
}

/// Write an order's contract lookup on `conn` ([`order_lookup_fields`]).
pub(crate) fn send_order_lookup_on(conn: &mut Connection, req_id: ReqId, lookup: &SymbolLookup) {
    send_lookup_body(conn, &order_lookup_fields(req_id, lookup));
}

fn send_lookup_body(conn: &mut Connection, body: &[(u32, String)]) {
    let ts = chrono_free_timestamp();
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

/// Code of the warning given with a contract details answer whose records
/// trade on the basis of a currency price with a factor (ibx#754).
pub(crate) const FACTOR_WARNING: i32 = 2130;

/// The warning of an answer's records whose EV rule is a factor, as the
/// reference 10.51 gives it once per answer (paper, 18/10/2026, market
/// closed: bond lists of 49, 26 and 1481 records with 1, 6 and 660 such
/// records, and one bond by conId): the symbol for one record, the count
/// for several. None when no record has one.
fn factor_warning(records: &[crate::control::contracts::ContractDefinition]) -> Option<String> {
    let with: Vec<&crate::control::contracts::ContractDefinition> = records.iter().filter(|d| d.ev_rule == "factor").collect();
    match with.as_slice() {
        [] => None,
        [one] => Some(format!("Warning: {} product is trading on the basis of currency price with factor", one.symbol)),
        many => Some(format!("Warning: {} products are trading on the basis of currency price with factor", many.len())),
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
            summary_groups: std::collections::HashMap::new(),
            seen_exec_ids: HashSet::with_capacity(256),
            exec_id_order: VecDeque::with_capacity(256),
            commission_revisions: std::collections::HashMap::with_capacity(256),
            commission_order: VecDeque::with_capacity(256),
            fill_events: HashSet::new(),
            fill_event_order: VecDeque::new(),
            held_commission_events: VecDeque::new(),
            exec_realized: std::collections::HashMap::with_capacity(256),
            order_messages_sent: HashMap::new(),
            report_leg_prices: Vec::new(),
            exec_realized_order: VecDeque::with_capacity(256),
            disconnected: false,
            pending_secdef: Vec::new(),
            optcalc: super::optcalc::OptCalc::default(),
            optparams: super::optparams::OptParams::default(),
            pending_lookups: Vec::new(),
            pending_continuous: Vec::new(),
            market_rule_by_exchange: std::collections::HashMap::new(),
            company_by_underlying: std::collections::HashMap::new(),
            pending_company: Vec::new(),
            preferred_after_company: HashSet::new(),
            pending_preferred: Vec::new(),
            held_details: Vec::new(),
            pending_matching_symbols: Vec::new(),
            next_matching_symbols_id: 1,
            matching_waiting: None,
            matching_permits: 1,
            matching_next_send: None,
            matching_acked: Vec::new(),
            data_permissions: None,
            deny_api: false,
            permissions_changed: false,
            ssl_farms_update: None,
            ccp_sign_key: Vec::new(),
            ccp_sign_iv: std::sync::Mutex::new(Vec::new()),
            pending_schedule_pair: Vec::new(),
            order_zone_waits: Vec::new(),
            held_cancel_dates: std::collections::HashMap::new(),
            schedules: std::collections::HashMap::new(),
            model_schedules: HashSet::new(),
            pending_fanout: Vec::new(),
            fanout_queue: std::collections::VecDeque::new(),
            fanout_clock: None,
            fanout_paced: false,
            completed: CompletedOrders::default(),
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
            pnl_partition_due: None,
            pnl_partition_id: None,
            // The reference's counter of its account and P&L requests was
            // at 8 for it (observed 15/10/2026); not seen by the API.
            next_partition_request: 8,
            core_seeds_asked: false,
            portfolio_seen: false,
            awaiting_status_replay: false,
            awaiting_login_replay: false,
            fill_up_pending: None,
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

    /// The event channel gave the `Fill` of `exec_id` (ibx#644): its
    /// commission reports go there from now on; one that came before is
    /// returned, to be given after the `Fill`.
    pub(crate) fn note_fill_event(&mut self, exec_id: &str) -> Option<api::CommissionAndFeesReport> {
        let (base, _) = split_exec_revision(exec_id);
        if base.is_empty() {
            return None;
        }
        if self.fill_events.insert(base.to_string()) {
            self.fill_event_order.push_back(base.to_string());
            while self.fill_event_order.len() > EXEC_ID_WINDOW {
                if let Some(old) = self.fill_event_order.pop_front() {
                    self.fill_events.remove(&old);
                }
            }
        }
        let held = self.held_commission_events.iter().position(|(b, _)| b == base)?;
        self.held_commission_events.remove(held).map(|(_, report)| report)
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
    /// commission nor the other field checked below is dropped, like the reference.
    fn handle_commission_report(&mut self, parsed: &fix::FieldMap<'_>, shared: &SharedState) {
        let Some(exec_id) = parsed.get(&17).copied().filter(|s| !s.is_empty()) else {
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
        // No realized P&L and no yield are 0, as the reference gives them
        // (ibx#543: 14 and 24 reports of the order scenarios of 09/10/2026).
        let unset_when_zero = |v: Option<f64>| v.unwrap_or(0.0);
        let report = api::CommissionAndFeesReport {
            exec_id: exec_id.to_string(),
            commission_and_fees: commission.unwrap_or(0.0),
            currency: parsed.get(&6381).map(|s| s.to_string()).unwrap_or_default(),
            realized_pnl: unset_when_zero(parsed.get(&6099).and_then(|s| s.parse().ok())),
            yield_amount: parsed.get(&236).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            yield_redemption_date: parsed.get(&696).filter(|s| s.len() == 8).map(|s| s.to_string()).unwrap_or_default(),
        };
        log::info!("Commission report: exec={} commission={} {}", report.exec_id, report.commission_and_fees, report.currency);
        // The commission of a fill that realized nothing goes into its
        // position's average cost (ibx#707).
        if report.realized_pnl == 0.0 && let Some((con_id, _)) = self.exec_realized.get(split_exec_revision(exec_id).0) {
            shared.portfolio.apply_commission_to_position(*con_id, report.commission_and_fees);
        }
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
        // On the event channel only after the `Fill` of its execution
        // (ibx#644): the server also sends, after the login, the reports of
        // the day's earlier executions, which come with no execution. One
        // that comes before its `Fill` waits for it.
        let (base, _) = split_exec_revision(exec_id);
        let to_events = self.fill_events.contains(base);
        if !to_events && shared.orders.events_attached() {
            if self.held_commission_events.len() >= HELD_COMMISSION_EVENTS {
                self.held_commission_events.pop_front();
            }
            self.held_commission_events.push_back((base.to_string(), report.clone()));
        }
        shared.orders.push_commission_report_to(report, to_events);
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
                            // Control-protocol frames — not consumed on the order path (ibx#185).
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
        // The kinds read from their bytes are given before any map of the
        // message's fields is built: a whole option chain is tens of
        // thousands of definition replies (ibx#701).
        if let Some(kind) = super::fast_extract_msg_type(msg)
            && self.dispatch_unparsed(kind, msg, ccp_conn, context, shared, event_tx, hb)
        {
            return;
        }
        let mut lossy = None;
        let parsed = fix::fix_fields(msg, &mut lossy);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => *t,
            None => return,
        };
        // The server time of a test request or of an eligible custom message
        // refreshes the clock offset, at most every 30 s (ibx#421).
        if crate::control::logon::message_sets_clock(msg_type, parsed.get(&6040).copied(), parsed.contains_key(&1)) {
            match parsed.get(&fix::TAG_SENDING_TIME).and_then(|v| crate::control::logon::server_time_ms(v)) {
                Some(server_ms) => {
                    let zone = crate::gateway::machine_tz();
                    if let Some(offset) = shared.reference.clock().apply_message(server_ms, crate::control::logon::local_now_ms(), &zone) {
                        log::debug!("Setting time offset to {} ms (35={})", offset, msg_type);
                    }
                }
                None => log::error!("No time in the message seqNum:{:?}", parsed.get(&34)),
            }
        }
        match msg_type {
            fix::MSG_LOGON => self.handle_logon_update(&parsed, shared, event_tx),
            fix::MSG_EXEC_REPORT => {
                self.report_leg_prices = all_values(msg, 6879).filter_map(|v| v.parse().ok()).collect();
                self.handle_exec_report(&parsed, context, shared, event_tx, account_id);
                self.report_leg_prices.clear();
            }
            fix::MSG_CANCEL_REJECT => self.handle_cancel_reject(&parsed, ccp_conn, context, shared, event_tx, hb, account_id),
            fix::MSG_NEWS => self.handle_news_bulletin(&parsed, shared),
            fix::MSG_TEST_REQUEST => {
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).map(|s| s.to_string()).unwrap_or_default();
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
                let reason = parsed.get(&58).copied().unwrap_or("unknown");
                let ref_tag = parsed.get(&371).copied().unwrap_or("?");
                // A refusal naming an order's contract lookup: its orders get
                // 200 with the server's text and are dropped, as the
                // reference (ibx#710; paper, 15/10/2026: an AAPL order to
                // ARCA with primary exchange NASDAQ, "Invalid destination
                // exchange specified").
                if let Some(&id) = parsed.get(&320) {
                    let id = ccp_conn.as_ref().map_or_else(|| id.to_string(), |c| c.internal_name(id));
                    if super::order_builder::order_lookup_refused(context, shared, &id, reason) {
                        return;
                    }
                }
                // The reject carries no request id: like the reference, it is
                // only logged, and a lookup it hit gets neither an error nor
                // an end from it (ibx#400). Such a lookup stays pending, as
                // in the reference (no deadline, ibx#485).
                log::warn!("SessionReject: reason='{}' refTag={}", reason, ref_tag);
            }
            "U" => {
                if let Some(&comm) = parsed.get(&6040) {
                    match comm {
                        "75" => {
                            // Position + market price feed (init burst + after each fill)
                            self.handle_position_feed(msg, ccp_conn, context, shared, event_tx, hb);
                            shared.portfolio.set_account_download_complete();
                            // The first position list of the "Core" model: the
                            // P&L seeds are asked again for that model, as the
                            // reference at its login (ibx#714; the reference
                            // 10.51, paper, 16/10/2026: PLR.6 with the model,
                            // 0.2 s after the account's PLR.4, right after this
                            // list; the same seeds answered).
                            if !self.core_seeds_asked && parsed.get(&6700).is_some_and(|m| *m == "Core")
                                && let Some(account) = parsed.get(&1).copied().filter(|a| !a.is_empty())
                                && let Some(conn) = ccp_conn.as_mut()
                            {
                                let ts = chrono_free_timestamp();
                                if conn.send_fix(&[(fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts), (6040, "142"),
                                    (6529, "PLR.6"), (1, account), (6700, "Core")]).is_ok()
                                {
                                    hb.last_ccp_sent = Instant::now();
                                    self.core_seeds_asked = true;
                                    log::info!("P&L seeds asked for the Core model of {}", account);
                                }
                            }
                        }
                        "77" => {
                            self.handle_account_summary(&parsed, context, shared);
                            shared.portfolio.set_account_download_complete();
                        }
                        "143" => {
                            // P&L midnight seed — store for client-side daily P&L computation
                            handle_pnl_response(msg, shared);
                            // A seed's contract no portfolio row carries
                            // is a reqPositions row (ibx#742): its
                            // definition, once the rows are known.
                            if self.portfolio_seen {
                                self.look_up_portfolio_contracts(None, ccp_conn, shared, hb);
                            }
                        }
                        // The prices at midnight of the positions, after the
                        // seeds (ibx#511; observed 15/10/2026 on the reference
                        // 10.51): the daily P&L of a position held at midnight.
                        "152" => handle_midnight_prices(msg, shared),
                        // The FA data exchange (ibx#507): a download or a
                        // save answered.
                        "117" => {
                            if super::fa::answer(&mut context.fa, &parsed, shared) {
                                super::order_builder::release_rth_parked(context);
                            }
                        }
                        "20" => {
                            // Reference-data answer: inputs of the option model (ibx#442).
                            if let (Some(&id), Some(&xml)) = (parsed.get(&320), parsed.get(&6118)) {
                                // Its internal name (ibx#698).
                                let id = ccp_conn.as_ref().map_or_else(|| id.to_string(), |c| c.internal_name(id));
                                let xml = xml.to_string();
                                self.optcalc.xml_reply(&id, &xml, ccp_conn, hb, shared);
                            }
                        }
                        "186" => self.handle_matching_symbols_reply(msg, &parsed, shared, event_tx),
                        // Option chain parameters (ibx#440): the derivative
                        // answer and the chain answer.
                        "5" => {
                            let connected = !self.disconnected;
                            self.optparams.underlying_reply(msg, ccp_conn, connected, hb, shared);
                        }
                        "139" => {
                            let connected = !self.disconnected;
                            self.optparams.chain_reply(msg, ccp_conn, connected, hb, shared);
                        }
                        "60" => self.handle_commission_report(&parsed, shared),
                        // An order preset asked at an order (ibx#660,
                        // ibx#729); not the logon's list.
                        "194" if parsed.get(&8166).is_some_and(|k| *k == "G") => {
                            super::order_builder::preset_reply(context, &parsed);
                            super::order_builder::preset_limit_reply(context, &parsed, msg);
                        }
                        // The logon's list of the order presets the account
                        // has stored (ibx#729).
                        "194" if parsed.get(&8166).is_some_and(|k| *k == "L") => {
                            super::order_builder::preset_list_reply(context, all_values(msg, 8168));
                        }
                        // The combo multiplier and the leg confirmation of
                        // a combo set-up (ibx#470).
                        "36" | "7" => {
                            if let Some(progress) = context.combos.user_reply(&parsed, &chrono_free_timestamp()) {
                                super::order_builder::combo_progress(context, ccp_conn, hb, progress);
                            }
                        }
                        // An algo definition answer (ibx#263).
                        "54" => if let Some(xml) = parsed.get(&6118) {
                            log::info!("Algo definitions received for {:?}", parsed.get(&6364));
                            shared.reference.add_algo_definitions(xml);
                        },
                        // A trading permission answer (ibx#666).
                        "66" => super::order_builder::permission_reply(context, &parsed),
                        // The exchange directory: not the depth exchanges,
                        // which come from the routing table (#453).
                        "102" => {}
                        "107" => {
                            self.handle_schedule_reply(msg, shared, event_tx);
                            let waits = &self.order_zone_waits;
                            super::order_builder::zones_known(context, |con_id| waits.iter().any(|(_, c)| *c == con_id));
                        }
                        _ => {}
                    }
                }
            }
            // The answers of the P&L partition's account request feed the
            // partition's cash (ibx#511), not the account stream.
            "UT" | "UM" | "RL" | "EB" | "UP" if self.is_partition_frame(msg) => {}
            other => {
                if !self.dispatch_unparsed(other.as_bytes(), msg, ccp_conn, context, shared, event_tx, hb) {
                    log::debug!("CCP unhandled 35={}: {} bytes", other, msg.len());
                }
            }
        }
    }

    /// The message kinds read from their bytes, with no map of their fields:
    /// heartbeats, definition replies, and the account and portfolio
    /// updates. None of them moves the clock offset. False for the others.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_unparsed(
        &mut self,
        kind: &[u8],
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) -> bool {
        match kind {
            b"0" => {}
            b"d" => {
                self.handle_secdef_reply(msg, ccp_conn, context, shared, event_tx, hb);
                // Increments of a definition that came, to its slots (ibx#613).
                super::farm::publish_answered_increments(context, shared);
            }
            b"UT" | b"UM" | b"RL" => handle_account_update(msg, context, shared),
            b"EB" => handle_account_end(msg, shared),
            b"UP" => {
                handle_portfolio_message(msg, context, shared, event_tx);
                self.portfolio_seen = true;
                self.look_up_portfolio_contracts(Some(msg), ccp_conn, shared, hb);
            }
            _ => return false,
        }
        true
    }

    /// A logon message on the logged-on auth connection (ibx#421). With a
    /// session epoch (6059) it is a solicited logon, which the reference
    /// ignores with a warning. Without one it is a logon update
    ///, whose steps run in the reference's order:
    /// the pending accounts (8092, when present); the feature
    /// list (6542), where DENYAPI turning on stops the API, as the
    /// reference stops its API connections; the private label misc URLs
    /// (6321, when not empty); a changed data permission
    /// stamp (6764), which makes the hot loop run the
    /// permission change; the SSL farm list (8449), replaced
    /// by the update's (empty when absent).
    pub(crate) fn handle_logon_update(
        &mut self,
        parsed: &fix::FieldMap<'_>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        if parsed.get(&6059).is_some_and(|v| !v.is_empty()) {
            log::warn!("Solicited logon while logged on: ignored");
            return;
        }
        log::info!("Handling logon update");
        if let Some(v) = parsed.get(&crate::gateway::TAG_PENDING_ACCOUNTS) {
            log::debug!("In AccountManager.updatePendingAccounts(): tag={} val=[{}]", crate::gateway::TAG_PENDING_ACCOUNTS, v);
            shared.reference.set_pending_accounts(crate::control::logon::pending_accounts(v));
        }
        if let Some(features) = parsed.get(&6542) {
            let deny_api = crate::control::logon::ApiFeatures::parse(features).deny_api;
            log::info!("Updated Enabled features: {}", features);
            if deny_api != self.deny_api {
                self.deny_api = deny_api;
                if deny_api {
                    log::warn!("we have running API but got disabled in allowed feature - stopping API");
                    shared.set_connection_lost();
                    emit(event_tx, Event::Disconnected);
                }
            }
        }
        if let Some(urls) = parsed.get(&crate::gateway::TAG_MISC_URLS).filter(|v| !v.is_empty()) {
            log::info!("Private label misc URLs updated ({} bytes)", urls.len());
            shared.reference.set_misc_urls(crate::gateway::parse_misc_urls(urls));
        }
        match parsed.get(&crate::gateway::TAG_DATA_PERMISSIONS).filter(|v| !v.is_empty()) {
            Some(stamp) => {
                if self.data_permissions_seen(stamp) {
                    self.permissions_changed = true;
                }
            }
            None => log::info!("Data permissions are not changed"),
        }
        let ssl_farms = parsed.get(&crate::gateway::TAG_SSL_FARMS).map(|s| s.to_string()).unwrap_or_default();
        self.ssl_farms_update = Some(ssl_farms);
    }

    /// A data permission stamp of a logon update: kept and logged when it
    /// changed, as the reference (ibx#421). True when it changed.
    pub(crate) fn data_permissions_seen(&mut self, stamp: &str) -> bool {
        if self.data_permissions.as_deref() == Some(stamp) {
            log::info!("Data permissions are not changed");
            return false;
        }
        self.data_permissions = Some(stamp.to_string());
        log::info!("Data permissions are changed. Market data is to be resubscribed");
        true
    }

    /// The data permission stamp of a relogin's logon reply (ibx#421): a
    /// change only when the old and the new stamp are both set and differ;
    /// the new one is kept in any case, even unset. True when it changed.
    pub(crate) fn relogin_data_permissions(&mut self, stamp: Option<&str>) -> bool {
        let changed = matches!((self.data_permissions.as_deref(), stamp), (Some(old), Some(new)) if old != new);
        if changed {
            log::info!("Data permissions are changed. Market data is to be resubscribed");
        } else {
            log::info!("Data permissions are not changed");
        }
        self.data_permissions = stamp.map(String::from);
        changed
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
        parsed: &fix::FieldMap<'_>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let Some(&clord) = parsed.get(&11) else { return };
        let Some(&(order_id, instrument)) = context.what_ifs.get(clord) else {
            log::info!("What-if reply for {} that this session did not send: dropped", clord);
            return;
        };
        // A reject report (its kind rejected, or its order status rejected:
        // the reference reads the report kind from the order status unless
        // the kind field says rejected) takes the reject path, error 201
        // alone, unless it is a status report: those go to the
        // preview's reply handling whatever their status.
        let tag = |t: u32| parsed.get(&t).copied();
        let reject = tag(150) == Some("8") || tag(39) == Some("8");
        if reject && tag(20) != Some("3") {
            context.what_ifs.remove(clord);
            let reason = parsed.get(&58).copied().unwrap_or("");
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
        // frame (ibx#462).
        let warning_text = parsed.get(&6361).map(|s| s.to_string()).unwrap_or_default();
        // A status report that rejects the preview is its final reply, with
        // or without numbers: the open order, then error 201.
        let is_data_frame = is_data_frame || (reject && parsed.contains_key(&58));
        if !is_data_frame && warning_text.is_empty() {
            return;
        }
        let final_reply = warning_text.is_empty();
        if final_reply {
            context.what_ifs.remove(clord);
        }
        let text = |tag: u32| parsed.get(&tag).map(|s| s.to_string()).unwrap_or_default();
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
            // The reference's preview openOrder has the permId of its order
            // (ibx#486, b1_462_whatif of 02/10/2026).
            perm_id: perm_id_of(parsed),
            con_id: parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0),
            price_mgmt: parsed.get(&8339).is_some_and(|v| *v == "1"),
        };
        let response = crate::types::WhatIfResponse {
            order_id,
            instrument,
            init_margin_before: parse_price_tag(parsed.get(&6826).copied()),
            maint_margin_before: parse_price_tag(parsed.get(&6827).copied()),
            equity_with_loan_before: parse_price_tag(parsed.get(&6828).copied()),
            init_margin_after: parse_price_tag(parsed.get(&6092).copied()),
            maint_margin_after: parse_price_tag(parsed.get(&6093).copied()),
            equity_with_loan_after: parse_price_tag(parsed.get(&6094).copied()),
            commission: parse_price_tag(parsed.get(&6378).copied()),
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
        parsed: &fix::FieldMap<'_>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        account_id: &str,
    ) {
        // This client's id, for whose reports are given to it.
        context.api_client_id = shared.reference.api_client_id();
        // What is kept for the orders gone for good (ibx#700).
        for order_id in context.purged_orders.drain(..) {
            self.held_cancel_dates.remove(&order_id);
            self.order_messages_sent.remove(&order_id);
        }
        // The answer of reqCompletedOrders: its replayed reports are its
        // rows, not order updates; its end marker ends it (ibx#695).
        if self.completed.reading.is_some() && !self.awaiting_login_replay && !self.awaiting_status_replay {
            if parsed.get(&11).copied() == Some("*") {
                self.completed_end(shared);
                return;
            }
            if parsed.get(&97).copied() == Some("Y") {
                let row = completed_row(parsed, account_id);
                // A contract not known yet is looked up at its first
                // report, as the reference (07/10/2026: IBM's definition
                // asked before the end of the answer).
                let con_id = row.0.con_id;
                if con_id != 0 && shared.reference.get_contract(con_id).is_none() && self.completed.defs.insert(con_id) {
                    self.completed.to_ask.push(con_id);
                }
                if let Some(rows) = self.completed.reading.as_mut() {
                    rows.push(row);
                }
                return;
            }
        }
        // End markers are not orders (ibx#399): the end of a trades reply
        // (it carries the request id), and the end of the order status
        // replay (wildcard order id).
        if let Some(request) = parsed.get(&6556).filter(|r| !r.starts_with("PT.")) {
            log::info!("ExecReport: end of trades request {}", request);
            if self.fill_up_pending.as_deref() == Some(*request) {
                self.fill_up_pending = None;
            }
            return;
        }
        if parsed.get(&11).copied() == Some("*") {
            if self.awaiting_status_replay {
                self.awaiting_status_replay = false;
                self.status_replay_end_at = Some(Instant::now());
                // The held open-order requests are answered from the
                // corrected orders (ibx#251).
                shared.orders.set_open_orders_held(false);
                shared.notify();
            }
            // The replay of the logon: the requests made since the connect
            // are answered now, with no restored-link message (ibx#251).
            if std::mem::take(&mut self.awaiting_login_replay) {
                shared.orders.set_login_orders_end();
                shared.orders.set_open_orders_held(false);
                shared.notify();
            }
            log::debug!("ExecReport: end of order status replay");
            return;
        }

        // The API order id the report gives for its client: the order ids
        // a client used, for its next valid id (ibx#466).
        note_reported_order_id(parsed, shared);

        // A what-if reply goes to the preview, before anything reads the
        // frame as an order: by the ClOrdID the preview was sent under, or
        // by a positive preview flag, as the reference routes it (ibx#462).
        let what_if_clord = parsed.get(&11).is_some_and(|c| context.what_ifs.contains_key(*c));
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
        let mut clord_id = context.key_of(server_id);
        // A negative API order id, a scale order's profit taker: the next
        // one of this session is numbered below it (ibx#751).
        if let Some(id) = parsed.get(&6121).and_then(|s| s.strip_prefix('-')).and_then(|s| s.parse::<i64>().ok()) {
            shared.orders.note_child_id(-id);
        }

        // An order this session does not hold, reported working: an order
        // of another session or client, put in the book so it can be
        // cancelled, by its id or by a global cancel, and its reports reach
        // it (ibx#191). The reference makes an order of any report of an
        // unknown order, with the report's API order id, client id
        // and client order id; the
        // logon replay reports an order not routed yet as pending new (captured
        // 01/10/2026: 8 orders of earlier sessions, execution type and order
        // status pending new in status reports), which
        // was left out when only the New status was taken (paper 04/10/2026:
        // 10147 on their cancel, nothing sent by a global cancel).
        let replayed_status = match parsed.get(&39).copied() {
            Some("0" | "5") => {
                let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
                    || parsed.get(&198).is_some_and(|s| *s != "NONE" && !s.is_empty());
                Some(if routed { crate::types::OrderStatus::Submitted } else { crate::types::OrderStatus::PreSubmitted })
            }
            Some("A") => Some(crate::types::OrderStatus::PreSubmitted),
            Some("1") => Some(crate::types::OrderStatus::PartiallyFilled),
            Some("6" | "D") => Some(crate::types::OrderStatus::PendingCancel),
            _ => None,
        };
        // Not from an execution of the day given again at the logon: it
        // tells what traded, not that the order still works. An order
        // partly filled then cancelled was shown working by every later
        // session of the day, and each execution of the day made order
        // callbacks at the connection, where the reference gives none
        // (paper, regular hours 12/11/2026: a fresh process of the reference
        // 10.51 after 30 fills gave no order at the connection). A working
        // order comes with its status report.
        let replayed_status = replayed_status
            .filter(|_| context.order(clord_id).is_none() && context.finished_status(clord_id).is_none()
                && parsed.get(&97).copied() != Some("Y"));
        if let Some(replayed_status) = replayed_status {
            // The API order id only names the order to the caller (ibx#466):
            // taken when no order of this session has it.
            let api_id = parsed.get(&6121).and_then(|s| s.parse::<OrderId>().ok())
                .filter(|&id| id != 0 && id != server_id && context.order(id).is_none()
                    && !context.server_ids.contains_key(&id));
            if let Some(api_id) = api_id {
                context.hold_recovered(api_id, server_id);
                clord_id = api_id;
            }
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let side = match parsed.get(&54).copied() {
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
                // The shared count covers the contract of the order, for the
                // per-instrument requests that walk ids below it.
                shared.market.set_instrument_count(context.market.count());
                if let Some(sym) = parsed.get(&55) {
                    context.set_symbol(instrument, sym.to_string());
                }
                context.insert_order(crate::types::Order {
                    order_id: clord_id,
                    instrument,
                    side,
                    price: limit_price_i64,
                    qty_fixed: qty,
                    filled_fixed: parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(0),
                    status: replayed_status,
                    ord_type: ord_type_byte,
                    tif: tif_byte,
                    stop_price: stop_price_i64,
                });
                // Its ClOrdID version, the one the server gives: the next
                // cancel or replace goes out under the next one.
                let version = parsed.get(&11)
                    .and_then(|c| c.split_once('.')).and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(0);
                context.modify_versions.insert(clord_id, version);
                // Its API client (0 when the report names none, as the
                // reference reads it).
                if let Some(entry) = context.book.get_mut(&clord_id) {
                    entry.owner = Some(parsed.get(&6119).and_then(|s| s.parse().ok()).unwrap_or(0));
                }
                // The API order id the client sees: the report's, 0 for
                // none (captured 01/10/2026: openOrder orderId 0).
                let api_id = report_api_order_id(parsed);
                if api_id != clord_id {
                    shared.orders.set_api_order_id(clord_id, api_id);
                }
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

        // Who gets the reports of this order, as the reference sends them:
        // its client, else client 0; its errors and notices only its client.
        // Captured 01/10/2026: client 193
        // got nothing of the cancel of 8 orders of client 0.
        let delivered = context.delivered(clord_id);
        let owned = context.owned(clord_id);

        // Record the ClOrdID exactly as the server reports it so subsequent
        // cancel/modify can echo back the same string. Skip reports of a
        // cancel: they carry the cancel request's own id, not the order's
        // (ibx#179). A cancel's id is the one ibx sent (ibx#464), or starts
        // with 'C' (the earlier form, on orders of a previous session).
        let is_cancel_request = parsed.get(&11).is_some_and(|s| {
            s.starts_with('C') || context.cancel_clord.get(&clord_id).map(String::as_str) == Some(*s)
        });
        if let Some(&raw_clord) = parsed.get(&11) {
            if !is_cancel_request && raw_clord != "*" {
                context.last_clord.insert(clord_id, raw_clord.to_string());
            }
        }

        // A fill of an order of this session in the trades reply after a
        // reconnect, that is a fill made while the link was lost: the
        // reference books it (executions, position, commission report) and
        // gives no execDetails and no orderStatus; once filled, the order is
        // unknown to the client (a cancel gets 10147). Captured 30/09/2026
        // (ib-agent#192 C4). A combo keeps its own path.
        if self.fill_up_pending.is_some()
            && matches!(parsed.get(&150).copied(), Some("F" | "1" | "2"))
            && parsed.get(&32).and_then(|s| parse_qty(s)).is_some_and(|q| q > 0)
            && context.order(clord_id).is_some()
            && !context.combos.orders.contains_key(&clord_id)
        {
            self.handle_outage_fill(parsed, context, shared, clord_id, account_id);
            return;
        }

        // A combo order (ibx#470): the report of a leg is an execution of
        // the leg, not a report of the order (captured 30/09/2026: after
        // the combo's fill, one fill per leg under the order's id, with
        // 6013 = leg index : leg count). The combo's own reports keep its
        // totals for the legs' callbacks.
        let combo_view = context.combos.orders.contains_key(&clord_id)
            .then(|| shared.orders.combo_view(clord_id)).flatten();
        if let Some(combo) = context.combos.orders.get_mut(&clord_id) {
            if is_leg_report(parsed) {
                let combo = combo.clone();
                self.handle_combo_leg_report(parsed, context, shared, clord_id, &combo);
                return;
            }
            let qty = |tag: u32| parsed.get(&tag).and_then(|s| parse_qty(s));
            let px = |tag: u32| parsed.get(&tag).and_then(|s| s.parse::<f64>().ok())
                .map(|v| (v * PRICE_SCALE as f64).round() as i64);
            if let Some(v) = qty(14) { combo.cum_qty = v; }
            if let Some(v) = qty(151) { combo.leaves_qty = v; }
            if let Some(v) = px(6) { combo.avg_price = v; }
            if qty(32).is_some_and(|q| q > 0) && let Some(v) = px(31) {
                combo.last_price = v;
            }
            // The leg prices the report carries, in leg order (6879 per
            // leg, captured 26/09/2026), or none.
            let legs = parsed.get(&6079).and_then(|n| n.parse::<usize>().ok()).unwrap_or(combo.combo.legs.len());
            let reported = if self.report_leg_prices.len() == legs {
                self.report_leg_prices.clone()
            } else {
                vec![f64::MAX; legs]
            };
            shared.orders.set_combo_leg_prices(clord_id, reported);
        }

        let ord_status = parsed.get(&39).copied().unwrap_or("");
        let exec_type = parsed.get(&150).copied().unwrap_or("");
        let exec_id = parsed.get(&17).copied().unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        // Fixed-point: a fraction of a share is kept (ibx#313).
        let last_shares: Qty = parsed.get(&32).and_then(|s| parse_qty(s)).unwrap_or(0);
        let leaves_qty: Qty = parsed.get(&151).and_then(|s| parse_qty(s)).unwrap_or(0);
        let commission = parsed.get(&12).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

        if ord_status == "8" {
            log::warn!("ExecReport REJECTED: clord={} reason='{}' 103={}",
                clord_id,
                parsed.get(&58).copied().unwrap_or("?"),
                parsed.get(&103).copied().unwrap_or("?"));
        } else {
            log::info!("ExecReport: 39={} 150={} 11={} 58={} 103={}",
                ord_status, exec_type, clord_id,
                parsed.get(&58).copied().unwrap_or(""),
                parsed.get(&103).copied().unwrap_or(""));
        }

        // The parent and OCA group the server gives for a held order, as
        // the reference's book keeps them for its global cancel.
        if context.order(clord_id).is_some() && (parsed.contains_key(&6107) || parsed.contains_key(&583)) {
            let parent = parsed.contains_key(&6107).then(|| parent_order_id(parsed, context)).filter(|&p| p > 0);
            let group = parsed.get(&583).copied();
            context.set_links(clord_id, parent, group);
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

        // The New order status on the wire, but the gateway reports PreSubmitted
        // until the order is actually routed to and acknowledged by an
        // exchange (for example a limit order resting pre-market). Routing
        // shows up on the same exec report as a non-empty ExDestination
        // (the exchange field) plus an exec ref other than "NONE"; before
        // routing both are absent/"NONE". Captured in ib-agent#162 (ibx#210).
        let working = || {
            let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
                || parsed.get(&198).is_some_and(|s| *s != "NONE" && !s.is_empty());
            if routed {
                crate::types::OrderStatus::Submitted
            } else {
                crate::types::OrderStatus::PreSubmitted
            }
        };
        let is_status_report = parsed.get(&20).copied() == Some("3");
        // A status report of a partly filled order that no venue holds: a
        // scale order between two of its levels shows PreSubmitted until the
        // next level is working (ibx#751; the reference 10.51, paper,
        // regular hours 12/11/2026: after each level's fill, then Submitted
        // again with the next level).
        let unrouted_partial = ord_status == "1" && is_status_report
            && parsed.get(&198).copied() == Some("NONE")
            && !parsed.get(&100).is_some_and(|s| !s.is_empty());
        let status = match ord_status {
            "1" if unrouted_partial => crate::types::OrderStatus::PreSubmitted,
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
            // Pending on a replace: a working order shows PreSubmitted until
            // the replace is done (the reference 10.51, paper 12/10/2026:
            // Submitted, then PreSubmitted and Submitted again; ibx#626).
            // Any other status is kept: reporting it as PendingCancel left
            // the order looking cancelled, since nothing moves it back
            // (ibx#247).
            // An order not acknowledged yet whose first reports are pending
            // ones (a scale order with a price adjustment) shows
            // PreSubmitted too (ibx#751; the reference 10.51, paper,
            // 18/10/2026: two such reports, then the New one).
            "6" if !is_cancel_request => match context.order(clord_id) {
                Some(o) if matches!(o.status, crate::types::OrderStatus::Submitted | crate::types::OrderStatus::PreSubmitted
                    | crate::types::OrderStatus::PendingSubmit) =>
                    crate::types::OrderStatus::PreSubmitted,
                Some(o) => o.status,
                None => crate::types::OrderStatus::PendingReplace,
            },
            "6" => crate::types::OrderStatus::PendingCancel,
            // A fill of an order no venue held yet leaves it PreSubmitted (a
            // scale order whose first level trades at once, ibx#751; the
            // reference 10.51, paper, regular hours 12/11/2026).
            "1" if context.order(clord_id).is_some_and(|o| o.status == crate::types::OrderStatus::PreSubmitted) =>
                crate::types::OrderStatus::PreSubmitted,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" => crate::types::OrderStatus::Cancelled,
            // Pending cancel: the order stays open until the Cancelled status
            // or a fill (ibx#472, a server-cancelled IOC sends this status
            // then Cancelled).
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
        // order (ibx#473), except the statuses it reads as invalid (the
        // pending replace and inactive order statuses, ibx#472).
        // The answer to a status request after a refused cancel or modify
        // sets the status, back to working too (ibx#252).
        // The replay after a reconnect answers a status request for every
        // working order: its status is set as the server gives it, as for
        // a single status request (ibx#251).
        let replayed = is_status_report && self.awaiting_status_replay;
        let queried = replayed || (is_status_report && !context.status_queries.is_empty()
            && context.status_queries.remove(&clord_id));
        // The pending report of a replace sets PreSubmitted over a working
        // status, which the guard would take as stale (ibx#626).
        let replace_pending = ord_status == "6" && !is_cancel_request
            && status == crate::types::OrderStatus::PreSubmitted;
        let change = if queried || replace_pending || unrouted_partial {
            context.apply_queried_status(clord_id, status)
        } else {
            context.apply_order_status(clord_id, status)
        };
        let status_changed = change == crate::engine::context::StatusChange::Changed;
        // A working report of an order whose cancel went out: the reference
        // reports it with the order's PendingCancel status (ibx#486,
        // modify_cancelled of 26/09/2026: pending new and New statuses after
        // the cancel message).
        let pending_cancel = change == crate::engine::context::StatusChange::Stale
            && !status.is_terminal()
            && context.order(clord_id).is_some_and(|o| o.status == crate::types::OrderStatus::PendingCancel);
        let report_status = (pending_cancel || matches!(change,
            crate::engine::context::StatusChange::Changed | crate::engine::context::StatusChange::Same))
            && !matches!(ord_status, "E" | "I");

        // The reference reports a server reject as error 201 with the
        // server's reason (ib-agent#192 C1, ibx#250), after the Inactive
        // status (ibx#486). Only on the first transition of an order this
        // session tracks: the "No such order" frames that follow a cancel
        // of an order already gone are not shown by the reference (C2).
        // The notice is queued after this report's status, below.
        let mut notice_out: Option<(i64, String)> = None;
        // The permId of the order given once more after a reject the
        // reference reads as a missing permission (below).
        let mut perm_again: Option<i64> = None;
        if status == crate::types::OrderStatus::Rejected && status_changed {
            let reason = parsed.get(&58).copied().unwrap_or("");
            notice_out = Some((201, format!("Order rejected - reason:{}", reason)));
            // A reject of reason 901 for an order whose class the account
            // has no permission for, by the answer known by then: 460 in
            // place of 201, and the order once more Inactive under a new
            // permId (ibx#666; the reference 10.51, paper, 17/10/2026,
            // market closed: a treasury bill by conId sent with its
            // permission asked, the answer no permission, then the server's
            // reject of reason 901 with its own text, a negative yield).
            if parsed.get(&103).copied() == Some("901") && no_class_permission(context, clord_id) {
                notice_out = Some((460, crate::engine::hot_loop::order_builder::NO_TRADING_PERMISSIONS.to_string()));
                perm_again = Some(context.new_server_id());
            }
        }
        // The limit offset, limit price and stop price the server reports
        // for a TRAIL LIMIT (6370, 44, 6117): the offset is restated on its
        // replace (ib-agent#194), the stop moves with the market (ibx#491).
        if context.order(clord_id).is_some() {
            let reported = |tag: u32| parsed.get(&tag).and_then(|s| s.parse::<f64>().ok())
                .map(|v| (v * PRICE_SCALE as f64).round() as i64);
            let stop = reported(6117);
            if let Some(stop) = stop { context.reported_stop.insert(clord_id, stop); }
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
        // reason, after the Cancelled status, as the reference (ibx#465,
        // ibx#486: every four-leg recording of 26/09 to 02/10/2026; empty
        // reason for a user cancel). Not for a status report in the
        // rejected state: the reference sets that one cancelled without the
        // notice (ibx#252).
        // The reason without its trailing spaces (ibx#688; the reference
        // 10.51, paper, 18/10/2026: an RPI order with no route).
        if status == crate::types::OrderStatus::Cancelled && status_changed && ord_status != "8" {
            let reason = parsed.get(&58).copied().unwrap_or("").trim_end();
            notice_out = Some((202, format!("Order Canceled - reason:{}", reason)));
        }
        // An order message of the server (6360 type, 6361 text): the
        // reference reports the TIME one as warning 399, once, and the order
        // keeps its status (ibx#465; captured 25/09/2026 on an OPG order:
        // "Order Message:\nBUY 1 AAPL NASDAQ.NMS\nWarning: your order will
        // not be placed at the exchange until ..."). The other types (PRICECAP
        // in the capture) did not reach the API. A Limit one too (ibx#688; the
        // reference 10.51, paper, 19/10/2026: "Your order size is below the
        // EUR 20000 IdealPro minimum ..." on a forex order of 1).
        if matches!(parsed.get(&6360).copied(), Some("TIME" | "Limit")) && context.order(clord_id).is_some() && owned {
            if let Some(text) = parsed.get(&6361).copied().filter(|t| !t.is_empty()) {
                let sent = self.order_messages_sent.entry(clord_id).or_default();
                if !sent.iter().any(|s| s == text) {
                    sent.push(text.to_string());
                    let message = order_message_399(parsed, text, context, clord_id, shared, combo_view.as_ref().map(|v| &v.contract));
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
                let time = parsed.get(&60).or_else(|| parsed.get(&52)).map(|t| t.to_string()).unwrap_or_default();
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
                let mut exec = fill_exec_of(parsed, exec_id);
                // The fill of a combo moves no position: its legs' fills do
                // (ibx#470). Its execution shows the combo contract without
                // its legs (captured 30/09/2026).
                if let Some(view) = &combo_view {
                    let contract = api::Contract {
                        combo_legs: Vec::new(), combo_legs_descrip: String::new(), ..view.contract.clone()
                    };
                    exec.combo = Some(Box::new(crate::bridge::ComboExec { contract, leg: None }));
                    if let Some(con_id) = context.market.con_id(order.instrument) {
                        self.record_exec_con_id(&exec.exec_id, con_id);
                    }
                }
                if combo_view.is_none() {
                    context.update_position_fixed(order.instrument, delta);
                    // notify_fill inlined
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
                        // position store does; the fill carries it for its
                        // reqPositions row (ibx#707).
                        exec.position_after = shared.portfolio.apply_fill_to_position(con_id, delta, last_px)
                            .map(|p| (con_id, p.position_fixed, p.avg_cost));
                    }
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
                let perm_id: i64 = perm_id_of(parsed);
                let parent_id = known_parent_id(parsed, context, clord_id);
                // Average fill price rides on status reports too (ibx#315).
                let avg_px = parsed.get(&6).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                // Filled so far as the server counts it (cumulative quantity): an order
                // recovered at session start has no prints in this session.
                let filled = parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(order.filled_fixed);
                // A cancel or a reject reports no leaves quantity; the reference's
                // remaining stays what was not filled (every four-leg
                // recording of 26/09 to 02/10/2026: Cancelled and Inactive
                // with 1 remaining of 1, ibx#486).
                // A fund bought by cash: its amount (ibx#666).
                // The order's quantity as the server reports it, when it
                // does: a beta hedge on another contract is sized by the
                // server, its max size included (ibx#602; the reference 10.51
                // at level 225, paper, 18/10/2026: a hedge of 10 capped at 2,
                // Cancelled with 2 remaining).
                let ordered = match (parsed.get(&38).and_then(|s| parse_qty(s)).filter(|&q| q > 0), order.qty_fixed) {
                    (Some(q), _) => q,
                    (None, 0) => context.cash_orders.get(&clord_id).copied().unwrap_or(0),
                    (None, q) => q,
                };
                // A scale order that starts again after its profit taker
                // filled has more to fill than its quantity: what it filled
                // plus what is left, kept for its cancel (ibx#751; the
                // reference 10.51, paper, regular hours 12/11/2026: an
                // order of 2 with 3 filled and 2 left, Cancelled with 2
                // remaining).
                let restarted = filled + leaves_qty;
                if leaves_qty > 0 && restarted > ordered {
                    context.scale_totals.insert(clord_id, restarted);
                }
                let remaining = match order.status {
                    // Not bounded at 0: a scale order sent with a quantity
                    // below 0 shows it (ibx#751; the reference 10.51,
                    // paper, 18/10/2026: Inactive with -1 remaining).
                    crate::types::OrderStatus::Cancelled | crate::types::OrderStatus::Rejected =>
                        context.scale_totals.remove(&clord_id).map_or(ordered, |total| total.max(ordered)) - filled,
                    _ => leaves_qty,
                };
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status: order.status,
                    filled_qty_fixed: filled,
                    remaining_qty_fixed: remaining,
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
            let account = parsed.get(&1).map(|s| s.to_string()).unwrap_or_default();
            let symbol = parsed.get(&55).map(|s| s.to_string()).unwrap_or_default();
            // The exchange of the order's contract is the one the order
            // was routed to (SMART for the smart route), as the reference's
            // openOrder shows it, not the venue where it rests (ibx#519:
            // an order of an earlier session resting on a venue shows SMART).
            let exchange = parsed.get(&6004).filter(|e| !e.is_empty())
                .map(|e| crate::control::contracts::exchange_from_fix(e).to_string())
                .or_else(|| parsed.get(&207).map(|s| s.to_string()))
                .unwrap_or_default();
            let sec_type = parsed.get(&167).map(|s| s.to_string()).unwrap_or_default();
            let currency = parsed.get(&15).map(|s| s.to_string()).unwrap_or_default();
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let local_symbol = parsed.get(&6035).map(|s| s.to_string()).unwrap_or_default();
            let perm_id: i64 = perm_id_of(parsed);
            let total_qty: f64 = parsed.get(&38).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let ord_type_tag = parsed.get(&40).copied().unwrap_or("");
            let limit_price: f64 = parsed.get(&44).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let stop_px: f64 = parsed.get(&99).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            // Kept once a report gave it: the reference's order shows it
            // from then on, also when later reports leave it out (ibx#486,
            // premarket_order_types of 28/09/2026: an IOC placed without
            // it, a report with the outside-RTH flag set, then openOrder outsideRth true).
            let outside_rth = parsed.get(&6433).is_some_and(|s| *s == "1")
                || shared.orders.get_order_info(clord_id).is_some_and(|i| i.order.outside_rth);
            let clearing_intent = parsed.get(&6419).map(|s| s.to_string()).unwrap_or_default();
            // The date the openOrder shows is the one of the order as it
            // was last sent, which a status report replaces; a new-order
            // report gives the order its date but not the openOrder, until
            // the next modify (ibx#683, 10.51; observed on
            // paper 13/10/2026: a STP whose only report was a new-order
            // one showed no date until its modify, a LMT whose first
            // report was a status one showed it at once). Kept when a later
            // report leaves it out (ibx#546).
            let reported_date = parsed.get(&6596).map(|s| s.to_string());
            let shown_before = shared.orders.get_order_info(clord_id).map(|i| i.order.auto_cancel_date).unwrap_or_default();
            let held_before = self.held_cancel_dates.get(&clord_id).cloned().unwrap_or_default();
            let auto_cancel_date = if parsed.get(&20).is_some_and(|t| *t == "3") {
                reported_date.clone().unwrap_or(shown_before)
            } else if parsed.get(&150).is_some_and(|t| *t == "5") {
                if held_before.is_empty() { shown_before } else { held_before }
            } else {
                shown_before
            };
            if let Some(date) = reported_date {
                self.held_cancel_dates.insert(clord_id, date);
            }
            let exec_exchange = parsed.get(&30).map(|s| s.to_string()).unwrap_or_default();
            let transact_time = parsed.get(&60).map(|s| s.to_string()).unwrap_or_default();
            let avg_px: f64 = parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let cum_qty: f64 = parsed.get(&14).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            // The order's quantity as openOrder shows it: what it filled plus
            // what is left, when that is more than its quantity (a scale
            // order that starts again, ibx#751; the reference 10.51, paper,
            // regular hours 12/11/2026: an order of 2 shown with 4, 3 and 5).
            let total_qty = total_qty.max(cum_qty + leaves_qty as f64 / crate::types::QTY_SCALE as f64);
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

            let order_type_str = report_order_type(ord_type_tag, parsed.get(&18).copied().unwrap_or(""));

            // As the reference: an unknown code is kept ("???"), not read
            // as DAY; each report sets the order's time in force (ibx#307).
            let tif_str = decode_tif(super::report_tif(&parsed));

            let action = match parsed.get(&54).copied() {
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
            // An execution of the day given again at the logon, of an order
            // this session does not hold, is no open order: an open-orders
            // request does not list it (the reference 10.51, paper, regular
            // hours 12/11/2026: none listed by a fresh process after orders
            // partly filled then cancelled).
            let status_str = if parsed.get(&97).copied() == Some("Y") && context.order(clord_id).is_none()
                && crate::client_core::is_open_status(status_str) { "" } else { status_str };

            let resolved_con_id = if con_id != 0 {
                con_id
            } else if let Some(order) = context.order(clord_id) {
                context.market.con_id(order.instrument).unwrap_or(0)
            } else {
                0
            };

            let contract = if let Some(view) = &combo_view {
                // A combo order shows its combo, not the report's contract
                // (55=IECombo) (ibx#470).
                view.contract.clone()
            } else if resolved_con_id != 0 {
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

            // Derive 3 order-dependent fields from the report's fields
            let oca_type: i32 = match parsed.get(&6209).copied() {
                Some("CancelOnFillWBlock") => 1,
                Some("ReduceOnFillWBlock") => 2,
                Some("ReduceOnFillNonBlock") => 3,
                Some("ReduceOnFillWBlockFromTotal") => 4,
                _ => 3, // default
            };
            let algo_strategy = parsed.get(&847).map(|s| s.to_string()).unwrap_or_default();
            // The price management flag the server echoes, 0 without it
            // (ibx#492); a report that leaves it out (a fill) keeps the
            // order's, as the reference shows the order it holds (ibx#546);
            // before any report, the order as sent (a refusal, ibx#605).
            let use_price_mgmt_algo: i32 = match parsed.get(&8339) {
                Some(v) => i32::from(*v == "1"),
                None => shared.orders.get_order_info(clord_id).map_or(
                    i32::from(context.price_mgmt_sent.contains(&clord_id)), |i| i.order.use_price_mgmt_algo),
            };
            let trail_stop_price: f64 = parsed.get(&6117)
                .and_then(|s| s.parse().ok())
                .unwrap_or(f64::MAX);
            // The compete values the reference shows: 100 and 0.02 until a
            // report gives the order as held, unset from then on (the
            // reference 10.51, paper, regular hours, 19/10/2026: 26 orders,
            // every pending-new report is such a report; ibx#605).
            if parsed.get(&6205).copied() == Some("1") {
                context.compete_cleared.insert(clord_id);
            }
            let (min_compete_size, compete_against_best_offset) = if context.compete_cleared.contains(&clord_id) {
                (i32::MAX, f64::MAX)
            } else {
                (100, 0.02)
            };

            // A scale order's auto-reset, as its reports gave it so far: the
            // first report of an order that has it may leave it out
            // (ibx#751; the reference 10.51, paper, 18/10/2026: the first
            // openOrder without it, the later ones with it).
            if parsed.get(&6461).copied() == Some("1") {
                context.scale_reset_reported.insert(clord_id);
            }
            let scale_auto_reset = context.scale_reset_reported.contains(&clord_id);
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
                parent_id: known_parent_id(parsed, context, clord_id),
                // The OCA group the order has on the server, which the
                // reference's openOrder shows: for a child, its parent's
                // number (ibx#509, the bracket recordings of 26/09/2026).
                oca_group: context.book.get(&clord_id).map(|e| e.oca_group.clone()).unwrap_or_default(),
                // Filled so far, not the quantity still working (ibx#309).
                filled_quantity: cum_qty,
                outside_rth,
                clearing_intent,
                auto_cancel_date,
                submitter: account_id.to_string(),
                oca_type,
                use_price_mgmt_algo,
                trail_stop_price,
                min_compete_size,
                compete_against_best_offset,
                algo_strategy,
                // The orderRef the server echoes (ibx#466).
                order_ref: parsed.get(&6010).map(|s| s.to_string()).unwrap_or_default(),
                // The cash quantity the server echoes in 152, which the
                // reference reads into the order's cash quantity
                // (ibx#263).
                cash_qty: parsed.get(&152).and_then(|s| s.parse().ok()).unwrap_or(0.0),
                // A TRAIL LIMIT's offset as the server reports it (ib-agent#194).
                lmt_price_offset: trail_limit.map_or(f64::MAX, |r| r.offset as f64 / PRICE_SCALE as f64),
                scale_auto_reset,
                // A combo's per-leg prices as reported (ibx#470).
                order_combo_legs: shared.orders.combo_view(clord_id).map(|v| v.leg_prices).unwrap_or_default(),
                // The order's API client (6119, 0 when absent) and order id.
                client_id: parsed.get(&6119).and_then(|s| s.parse().ok()).unwrap_or(0),
                // A hedge order's hedge as the server reports it (ibx#614).
                hedge_type: hedge_letter(parsed.get(&6665).copied().unwrap_or("")).to_string(),
                hedge_param: parsed.get(&6703).or_else(|| parsed.get(&6666))
                    .and_then(|v| v.parse::<f64>().ok()).map(hedge_param_text).unwrap_or_default(),
                hedge_max_size: parsed.get(&6690).and_then(|v| v.parse().ok()).unwrap_or(i32::MAX),
                ..Default::default()
            };
            if let Some(entry) = context.book.get(&clord_id) {
                shared.orders.note_book(clord_id, entry.seq, context.book_peak);
            }

            let completed_time = if matches!(status,
                crate::types::OrderStatus::Filled |
                crate::types::OrderStatus::Cancelled |
                crate::types::OrderStatus::Rejected
            ) {
                // In the form of an execution's time: the openOrder of a
                // filled order shows it (the reference 10.51, ibx#605).
                parsed.get(&60).or_else(|| parsed.get(&52))
                    .and_then(|t| jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", t).ok())
                    .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
                    .map(|z| crate::client_core::format_exec_time(z.timestamp().as_second()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let completed_status = match status {
                crate::types::OrderStatus::Filled => "Filled".to_string(),
                crate::types::OrderStatus::Cancelled => "Cancelled".to_string(),
                crate::types::OrderStatus::Rejected => {
                    parsed.get(&58).map(|s| s.to_string()).unwrap_or_else(|| "Rejected".to_string())
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

        if let Some((fill, mut exec)) = fill_out {
            let fill_exec_id = event_tx.as_ref().map(|_| exec.exec_id.clone());
            if delivered {
                shared.orders.push_fill_with_exec(fill, exec);
            } else {
                // An execution of another client's order: stored, no
                // callback.
                exec.other_client = true;
                let execution = report_execution(parsed, exec_id, fill.side, fill.qty_fixed, last_px,
                    account_id, shared.orders.api_order_id(clord_id));
                shared.orders.push_untracked_execution(report_contract(parsed, shared), execution, exec);
            }
            emit(event_tx, Event::Fill(fill));
            if let Some(report) = fill_exec_id.and_then(|id| self.note_fill_event(&id)) {
                emit(event_tx, Event::CommissionReport(report));
            }
        }
        if let Some(update) = update_out {
            if delivered {
                shared.orders.push_order_update(update);
            }
            emit(event_tx, Event::OrderUpdate(update));
        }
        if let Some((code, text)) = notice_out.filter(|_| owned) {
            shared.orders.push_order_notice(clord_id, code, text);
            if let (Some(perm_id), Some(update)) = (perm_again, update_out.filter(|_| delivered)) {
                shared.orders.push_order_update(crate::types::OrderUpdate { perm_id, ..update });
            }
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
            context.combos.finished(clord_id);
        }
    }

    /// The report of one leg of a combo order's fill (ibx#470; captured
    /// 30/09/2026): an execution of the leg, under the combo order, that
    /// moves the leg's position. Its execDetails shows the leg's contract
    /// on the execution's exchange, the leg's side (a sell-short side is a
    /// sale), size,
    /// price and totals; the orderStatus that goes with it keeps the
    /// combo's totals and last price. The order's own state does not move.
    fn handle_combo_leg_report(
        &mut self,
        parsed: &fix::FieldMap<'_>,
        context: &mut Context,
        shared: &SharedState,
        order_id: OrderId,
        combo: &crate::engine::combo::ComboOrder,
    ) {
        let exec_id = parsed.get(&17).copied().unwrap_or("");
        let last_shares: Qty = parsed.get(&32).and_then(|s| parse_qty(s)).unwrap_or(0);
        if last_shares <= 0 { return; }
        if !exec_id.is_empty() && !self.record_exec_id(exec_id) {
            log::warn!("Duplicate ExecID={}: leg fill not booked again", exec_id);
            return;
        }
        if !exec_id.is_empty() {
            let time = parsed.get(&60).or_else(|| parsed.get(&52)).map(|t| t.to_string()).unwrap_or_default();
            self.last_exec = Some((exec_id.to_string(), time));
        }
        let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
        let buy = parsed.get(&54).copied() == Some("1");
        let price = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let shares = last_shares as f64 / QTY_SCALE as f64;
        let delta = if buy { last_shares } else { -last_shares };
        if con_id != 0 {
            if let Some(instrument) = context.market.try_register(con_id) {
                shared.market.set_instrument_count(context.market.count());
                context.update_position_fixed(instrument, delta);
                shared.portfolio.set_position_fixed(instrument, context.position_fixed(instrument));
            }
            self.record_exec_con_id(exec_id, con_id);
            shared.portfolio.add_money_since_seed(con_id, if buy { -shares * price } else { shares * price });
            shared.portfolio.apply_fill_to_position(con_id, delta, price);
        }
        let leg = combo.combo.legs.iter().find(|l| l.con_id == con_id);
        let mut exec = fill_exec_of(parsed, exec_id);
        let sec_type = match parsed.get(&167).copied() {
            Some("CS") | Some("COMMON") | None => "STK".to_string(),
            Some(other) => other.to_string(),
        };
        let contract = api::Contract {
            con_id,
            symbol: parsed.get(&55).map(|s| s.to_string()).or_else(|| leg.map(|l| l.symbol.clone())).unwrap_or_default(),
            sec_type,
            exchange: exec.exchange.clone(),
            currency: parsed.get(&15).map(|s| s.to_string()).unwrap_or_default(),
            local_symbol: parsed.get(&6035).map(|s| s.to_string()).unwrap_or_default(),
            ..Default::default()
        };
        let qty = |tag: u32| parsed.get(&tag).and_then(|s| parse_qty(s)).unwrap_or(0) as f64 / QTY_SCALE as f64;
        exec.combo = Some(Box::new(crate::bridge::ComboExec {
            contract,
            leg: Some(crate::bridge::LegExec {
                side: if buy { "BOT" } else { "SLD" }.to_string(),
                shares,
                price,
                cum_qty: qty(14),
                avg_price: parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            }),
        }));
        let order = context.order(order_id).copied();
        let fill = Fill {
            instrument: order.map_or(0, |o| o.instrument),
            order_id,
            side: order.map_or(Side::Buy, |o| o.side),
            price: combo.last_price,
            qty_fixed: last_shares,
            remaining_fixed: combo.leaves_qty,
            cum_qty_fixed: combo.cum_qty,
            avg_price: combo.avg_price,
            commission: 0,
            timestamp_ns: context.now_ns(),
        };
        log::info!("Combo order {} leg fill: con_id={} {} {} @ {}", order_id, con_id, if buy { "BOT" } else { "SLD" }, shares, price);
        shared.orders.push_fill_with_exec(fill, exec);
    }

    /// A fill made while the auth link was lost, in the trades reply after
    /// the reconnect (ibx#251): booked as the fill of an untracked order
    /// (kept for `req_executions`, moves the position, its commission
    /// report goes out), with no fill event and no status. When it ends the
    /// order, the order leaves the engine and the client without a status,
    /// and is not kept as finished: a later cancel of its id gets 10147, as
    /// the reference's (captured 30/09/2026, ib-agent#192 C4).
    fn handle_outage_fill(
        &mut self,
        parsed: &fix::FieldMap<'_>,
        context: &mut Context,
        shared: &SharedState,
        clord_id: OrderId,
        account_id: &str,
    ) {
        let exec_id = parsed.get(&17).copied().unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let last_shares: Qty = parsed.get(&32).and_then(|s| parse_qty(s)).unwrap_or(0);
        if !exec_id.is_empty() && self.seen_exec_ids.contains(exec_id) {
            log::warn!("Duplicate ExecID={}: fill not booked again", exec_id);
        } else {
            if !exec_id.is_empty() {
                let time = parsed.get(&60).or_else(|| parsed.get(&52)).map(|t| t.to_string()).unwrap_or_default();
                self.last_exec = Some((exec_id.to_string(), time));
            }
            if let Some((execution, exec)) = self.book_untracked_fill(
                parsed, context, shared, clord_id, exec_id, last_px, last_shares, account_id)
            {
                if !exec_id.is_empty() {
                    self.record_exec_id(exec_id);
                }
                shared.orders.push_untracked_execution(report_contract(parsed, shared), execution, exec);
            }
        }
        let leaves: Qty = parsed.get(&151).and_then(|s| parse_qty(s)).unwrap_or(0);
        let ended = leaves == 0 || parsed.get(&39).copied() == Some("2");
        log::info!("Fill {} of order {} made while the link was lost: no fill event{}", exec_id, clord_id,
            if ended { ", the order is dropped" } else { "" });
        if ended {
            context.forget_order(clord_id);
            shared.orders.push_forgotten_order(clord_id);
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
        parsed: &fix::FieldMap<'_>,
        context: &mut Context,
        shared: &SharedState,
        clord_id: OrderId,
        exec_id: &str,
        last_px: f64,
        last_shares: Qty,
        account_id: &str,
    ) -> Option<(api::Execution, crate::bridge::FillExec)> {
        let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
        let side = match parsed.get(&54).copied() {
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
                    context.set_symbol(instrument, sym.to_string());
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
        let position_after = shared.portfolio.apply_fill_to_position(con_id, delta, last_px)
            .map(|p| (con_id, p.position_fixed, p.avg_cost));

        // The placing client's order id when the report carries it.
        let order_id = parsed.get(&6121).and_then(|s| s.parse().ok()).unwrap_or(clord_id);
        let execution = report_execution(parsed, exec_id, side, last_shares, last_px, account_id, order_id);
        log::info!("Fill {} of untracked order {}: con_id={} {:?} {} @ {}, stored",
            exec_id, clord_id, con_id, side, shares, last_px);
        // The execution's reports go to its client, else to client 0
        //.
        let mut exec = fill_exec_of(parsed, exec_id);
        exec.position_after = position_after;
        let me = context.api_client_id;
        exec.other_client = me != 0 && exec.client_id != me;
        Some((execution, exec))
    }

    /// A cancel or modify the server refused (ibx#252). As the reference:
    /// the order is the one whose current ClOrdID the reject carries (a
    /// reject of an older version changes nothing); its ClOrdID version
    /// goes back by one, its status and its record stay, and a status
    /// request for it is sent. The answer to that request sets the status.
    #[allow(clippy::too_many_arguments)]
    fn handle_cancel_reject(
        &mut self,
        parsed: &fix::FieldMap<'_>,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        let reason = parsed.get(&58).copied().unwrap_or("Cancel rejected");
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        let clord = parsed.get(&11).copied().unwrap_or("");
        log::warn!("CancelReject: clord={} type={} code={} reason={}",
            clord, reject_type, reason_code, reason);

        let Some((server, version)) = clord.split_once('.')
            .and_then(|(id, ver)| Some((id.parse::<OrderId>().ok()?, ver.parse::<u32>().ok()?)))
        else { return };
        // The order held under the server's id of the ClOrdID.
        let oid = context.key_of(server);
        if version == 0 || context.modify_versions.get(&oid) != Some(&version) {
            log::info!("CancelReject: {} is not the current version of order {}, ignored", clord, oid);
            return;
        }
        let lowered = format!("{}.{}", server, version - 1);
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
    fn handle_news_bulletin(&mut self, parsed: &fix::FieldMap<'_>, shared: &SharedState) {
        static BULLETIN_TYPE_MAP: &[(i32, i32)] = &[
            (1, 1), (2, 3), (3, 2), (8, 4), (9, 5), (10, 6),
        ];
        let fix_type: i32 = parsed.get(&fix::TAG_URGENCY)
            .and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let Some(api_type) = BULLETIN_TYPE_MAP.iter()
            .find(|(k, _)| *k == fix_type)
            .map(|(_, v)| *v) else { return };
        let message = parsed.get(&fix::TAG_HEADLINE).map(|s| s.to_string()).unwrap_or_default();
        if message.is_empty() {
            return;
        }
        let exchange = parsed.get(&fix::TAG_SECURITY_EXCHANGE).map(|s| s.to_string()).unwrap_or_default();
        let msg_id = parsed.get(&6143).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let bulletin = NewsBulletin { msg_id, msg_type: api_type, message, exchange };
        if !shared.market.push_news_bulletin(bulletin) {
            log::info!("News bulletin {} already received: dropped", msg_id);
        }
    }

    fn handle_account_summary(&mut self, parsed: &fix::FieldMap<'_>, context: &mut Context, shared: &SharedState) {
        if let Some(val) = parsed.get(&9806).and_then(|s| s.parse::<f64>().ok()) {
            context.account.net_liquidation = (val * PRICE_SCALE as f64) as Price;
        }
        shared.portfolio.set_account(context.account());
    }

    /// Subscribe to the schedule paired with a secdef reply, joined on the
    /// schedule key field. Internal subscription (no API-client req_id
    /// exposed); reply arrives as a trading schedule custom message and is
    /// matched back to the secdef via the schedule key.
    fn send_schedule_subscribe(
        &mut self,
        join_key: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            // Named as the reference names it, in the series of the option
            // model's queries (ibx#670; the reference's messages of
            // 12/10 and 13/10/2026: "SubMsgXMLQuery" with a number of the
            // series it shares with its symbol lookups).
            let sub_id_str = self.optcalc.next_query_id();
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

    /// The trading hours of a class an order's option model asks (ibx#730),
    /// unless they are known for the day or on their way. True when asked.
    pub(crate) fn ask_model_schedule(&mut self, join_key: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) -> bool {
        if join_key.is_empty() || self.schedule_known(join_key) || self.model_schedules.contains(join_key) {
            return false;
        }
        self.model_schedules.insert(join_key.to_string());
        log::info!("Trading hours of {} asked for an order's model", join_key);
        self.send_schedule_subscribe(join_key, ccp_conn, hb);
        true
    }

    /// Whether the trading hours of a schedule key came for the day.
    pub(crate) fn schedule_known(&self, join_key: &str) -> bool {
        self.schedules.get(join_key).is_some_and(|(day, _)| *day == utc_day())
    }

    /// Drop the internal lookups of ibx (ids from 0xF000_0000: cache
    /// fill, scanner enrichment) past their deadline, silently: no caller
    /// waits on them. A caller's contract details request, and its
    /// per-exchange fan-out, has no deadline, as in the reference: it waits
    /// for its answer (ibx#485; ibx#227 gave it error 200 with a text of
    /// ibx's own).
    pub(crate) fn sweep_contract_details_and_completed(
        &mut self, shared: &SharedState, event_tx: &Option<Sender<Event>>, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState,
    ) {
        self.sweep_completed_orders(shared, ccp_conn, hb);
        self.sweep_contract_details(shared, event_tx, ccp_conn, hb);
    }

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
        self.pending_secdef.retain(|(req_id, _, deadline)| {
            let expired = internal_lookup(*req_id) && now >= *deadline;
            if expired {
                log::warn!("Internal secdef timeout: req_id={:#x}", req_id);
            }
            !expired
        });
        let mut late: Vec<PendingFanout> = Vec::new();
        let mut i = 0;
        while i < self.pending_fanout.len() {
            if internal_lookup(self.pending_fanout[i].api_req_id) && now >= self.pending_fanout[i].deadline {
                late.push(self.pending_fanout.swap_remove(i));
            } else {
                i += 1;
            }
        }
        for fanout in late {
            log::warn!(
                "Internal fan-out timeout: api_req_id={:#x} answered {} of {}",
                fanout.api_req_id, fanout.total - fanout.outstanding.len(), fanout.total,
            );
            if !fanout.records.is_empty() {
                // The rows go to the cache with the market rules known so far.
                self.deliver_contract_rows(fanout.api_req_id, fanout.records.into(), None, shared, event_tx, ccp_conn, hb);
            }
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
        for p in self.pending_schedule_pair.iter_mut().filter(|p| !p.ready && now >= p.deadline) {
            log::warn!("Schedule pair timeout: api_req_id={} join_key={}", p.api_req_id, p.join_key);
            p.def.trading_hours = None;
            p.def.liquid_hours = None;
            p.def.time_zone_id = None;
            p.ready = true;
        }
        self.release_ready_rows(shared, event_tx);
    }

    /// Give the rows whose schedule came, those of one lookup in the order
    /// of its records: a row leaves once no earlier row of its lookup
    /// waits. The reference asks the schedules it lacks one at a time, in
    /// record order, so its waiting rows come out in that order whatever
    /// the order of the answers (ibx#527; recording of 02/10/2026, a bond
    /// lookup by issuer of 41 rows). The end follows the last row.
    fn release_ready_rows(&mut self, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let mut blocked: Vec<ReqId> = Vec::new();
        let mut done: Vec<ReqId> = Vec::new();
        let mut waiting = Vec::with_capacity(self.pending_schedule_pair.len());
        for p in std::mem::take(&mut self.pending_schedule_pair) {
            if p.ready && !blocked.contains(&p.api_req_id) {
                if !done.contains(&p.api_req_id) {
                    done.push(p.api_req_id);
                }
                push_contract_row(shared, event_tx, p.api_req_id, p.def);
            } else {
                if !blocked.contains(&p.api_req_id) {
                    blocked.push(p.api_req_id);
                }
                waiting.push(p);
            }
        }
        self.pending_schedule_pair = waiting;
        for req_id in done {
            self.end_if_complete(req_id, shared, event_tx);
        }
    }

    /// Match a schedule reply to the rows waiting for its join key and
    /// emit them with the schedule merged. One reply serves every row that
    /// waits for that key.
    /// The trading hours of an order's contract, asked once per schedule
    /// key and session after the order's definition came, as the reference
    /// does for every API order (ibx#636, 10.51 and 10.40, the
    /// request seen 9 to 18 ms after the order's lookup, paper 12/10/2026).
    /// The order does not wait for them; their zone is kept for the
    /// contract, and the order's start time is shown in it (#598).
    /// The conId whose zone is then waited for, when it is not known.
    fn ask_order_schedule(&mut self, def: Option<&crate::control::contracts::ContractDefinition>, shared: &SharedState, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) -> Option<i64> {
        let (con_id, ask) = self.order_schedule_needed(def, shared)?;
        if ask {
            let key = &def?.join_key;
            log::info!("Trading hours of con_id {} asked for its orders ({})", con_id, key);
            self.send_schedule_subscribe(key, ccp_conn, hb);
        }
        Some(con_id)
    }

    /// The trading hours of the contract an order waits for, from the
    /// answer that made it known (ibx#728): as `ask_order_schedule`, but
    /// the request is written at the order's turn, after the question on
    /// its class's permission when it has one (the reference 10.51, paper,
    /// 13-16/10/2026: the permission asked first, the trading hours 1 to
    /// 17 ms after, fourteen first orders of eight types; for a stock, the
    /// hours first and the order preset 1 to 5 ms after). Its name is taken
    /// now, in the series it shares with the lookups.
    fn defer_order_schedule(&mut self, def: Option<&crate::control::contracts::ContractDefinition>, shared: &SharedState, context: &mut Context) -> Option<i64> {
        let (con_id, ask) = self.order_schedule_needed(def, shared)?;
        if ask && let Some(def) = def {
            log::info!("Trading hours of con_id {} due for its orders ({})", con_id, def.join_key);
            context.order_hours_due.push((con_id, def.join_key.clone(), self.optcalc.next_query_id()));
        }
        Some(con_id)
    }

    /// Whether an order's contract needs its trading hours: None when they
    /// are known for the day (their zone is the contract's) or the
    /// definition has no schedule key; else its conId, noted as waiting,
    /// and whether the request is still to make.
    fn order_schedule_needed(&mut self, def: Option<&crate::control::contracts::ContractDefinition>, shared: &SharedState) -> Option<(i64, bool)> {
        let def = def?;
        if def.join_key.is_empty() || def.con_id == 0 {
            return None;
        }
        if let Some((_, known)) = self.schedules.get(&def.join_key).filter(|(day, _)| *day == utc_day()) {
            if let Some(zone) = known.as_ref().map(|s| s.timezone.as_str()).filter(|z| !z.is_empty()) {
                shared.reference.cache_time_zone_id(def.con_id, zone);
            }
            return None;
        }
        let asked = self.order_zone_waits.iter().any(|(k, _)| *k == def.join_key)
            || self.pending_schedule_pair.iter().any(|p| !p.ready && p.join_key == def.join_key);
        self.order_zone_waits.push((def.join_key.clone(), def.con_id));
        Some((def.con_id, !asked))
    }

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
        let sched = crate::control::contracts::parse_schedule_response(msg);
        // The orders' contracts get the zone (ibx#636).
        let zone = sched.as_ref().map(|s| s.timezone.clone()).filter(|z| !z.is_empty());
        self.order_zone_waits.retain(|(key, con_id)| {
            if *key != join_key { return true; }
            if let Some(zone) = &zone { shared.reference.cache_time_zone_id(*con_id, zone); }
            false
        });
        let waited = self.pending_schedule_pair.iter().any(|p| !p.ready && p.join_key == join_key);
        // Kept for the session: the next row with this key does not wait.
        self.schedules.insert(join_key.clone(), (utc_day(), sched.clone()));
        if !waited {
            return;
        }
        for pair in self.pending_schedule_pair.iter_mut().filter(|p| !p.ready && p.join_key == join_key) {
            if let Some(sched) = &sched {
                // In the zone of the schedule, from its current day on, as
                // the reference (ibx#436).
                crate::control::contracts::apply_schedule(&mut pair.def, sched, jiff::Timestamp::now());
            }
            pair.ready = true;
        }
        self.release_ready_rows(shared, event_tx);
    }

    /// The first P&L request of the session (ibx#511): the reference asks
    /// the account of its P&L partition 1 s later, once.
    pub(crate) fn start_pnl_partition(&mut self, now: Instant) {
        if self.pnl_partition_due.is_none() && self.pnl_partition_id.is_none() {
            self.pnl_partition_due = Some(now + PNL_PARTITION_DELAY);
        }
    }

    /// The account request of the P&L partition when it is due (ibx#511),
    /// as the reference writes it (observed 15/10/2026, 10.51, and in the
    /// recordings of 26/09/2026 and 07/10/2026, 10.40): an account
    /// subscription of its own id, with the partition flag and the "Core"
    /// model, 1.0-1.1 s after the first reqPnL. It is never cancelled.
    pub(crate) fn send_due_pnl_partition(
        &mut self,
        now: Instant,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if self.pnl_partition_due.is_none_or(|due| now < due) || self.disconnected {
            return;
        }
        let Some(conn) = ccp_conn.as_mut() else { return };
        let id = format!("AR.{}", self.next_partition_request);
        let ts = chrono_free_timestamp();
        if conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "6"),
            (6036, "1"),
            (6529, &id),
            (6095, account),
            (8063, "1"),
            (6700, "Core"),
        ]).is_ok() {
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent the P&L partition's account request {}", id);
            self.next_partition_request += 1;
            self.pnl_partition_id = Some(id);
            self.pnl_partition_due = None;
        }
    }

    /// A frame of the P&L partition's account subscription (ibx#511).
    fn is_partition_frame(&self, msg: &[u8]) -> bool {
        let Some(id) = &self.pnl_partition_id else { return false };
        let Ok(text) = std::str::from_utf8(msg) else { return false };
        text.split('').any(|p| p.strip_prefix("6529=") == Some(id.as_str()))
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
    /// an account summary custom message with the subscribe flag set, the
    /// subscription id `{sr_id}`, the tags and the group to subscribe; the
    /// flag cleared with the same id and the group to cancel (`subscribe` is
    /// `None`; ibx#486, account_summary of 26/09/2026: a cancel with the
    /// id `SR.Socket.39` and the group `All`).
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
            self.summary_groups.insert(sr_id.to_string(), group.to_string());
        }
        let group = if subscribe.is_none() { self.summary_groups.remove(sr_id) } else { None };
        if let Some(group) = &group {
            fields.push((6160, group));
        }
        let _ = conn.send_fix(&fields);
        hb.last_ccp_sent = Instant::now();
        log::info!("Sent account summary {}: {}", if subscribe.is_some() { "subscribe" } else { "cancel" }, sr_id);
    }

    /// The lookup of a scanner row's contract (ibx#513), in the reference's
    /// form: the preferred contract of the conId, asked for the scanner
    /// (recording of 26/09/2026).
    pub(crate) fn send_scanner_row_lookup(&mut self, req_id: ReqId, con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let con_id_str = con_id.to_string();
            let name = format!("{}{}", crate::control::contracts::SECDEF_PREFERRED_NAME, req_id);
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, &name),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (crate::control::contracts::TAG_IB_SOURCE, "Scanner"),
                (146, "1"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (6004, "ANYEXCH"),
            ]);
            log::info!("Sent scanner row lookup: req_id={} con_id={}", req_id, con_id);
            hb.last_ccp_sent = Instant::now();
        } else {
            log::warn!("scanner row lookup req_id={} queued with no CCP socket", req_id);
        }
        self.pending_secdef.push((req_id, true, Instant::now() + SECDEF_TIMEOUT));
    }

    /// The API lookup by conId (ibx#438), in the reference's by-conId
    /// forms: with an exchange (`SMART` written `BEST`) the request id
    /// `socket-reqContractDetailsReqByConid{id}`, the security request type
    /// the code sends, the source `Socket`, the include-expired flag set, one
    /// related symbol, the contract id and the exchange; without one the
    /// preferred contract of the conId: the request id
    /// `PreferredReqByConid{id}`, the same request type, one related symbol,
    /// the contract id and the exchange `ANYEXCH` (captured 02/10/2026).
    /// The reference answers from its contract cache
    /// when it can; ibx always asks.
    pub(crate) fn send_contract_details_by_con_id(
        &mut self,
        req_id: ReqId,
        con_id: i64,
        exchange: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        use crate::control::contracts::{SECDEF_BY_CONID_NAME, SECDEF_PREFERRED_NAME};
        let con_id_str = con_id.to_string();
        let ts = chrono_free_timestamp();
        let mut fields: Vec<(u32, String)> = vec![
            (fix::TAG_MSG_TYPE, "c".into()),
            (fix::TAG_SENDING_TIME, ts.to_string()),
        ];
        if exchange.is_empty() {
            fields.push((320, format!("{}{}", SECDEF_PREFERRED_NAME, req_id)));
            fields.push((321, "2".into()));
        } else {
            fields.push((320, format!("{}{}", SECDEF_BY_CONID_NAME, req_id)));
            fields.push((321, "2".into()));
            fields.push((crate::control::contracts::TAG_IB_SOURCE, "Socket".into()));
            fields.push((6320, "1".into()));
        }
        fields.push((146, "1".into()));
        fields.push((6008, con_id_str));
        let exchange = match exchange {
            "" => "ANYEXCH",
            "SMART" => "BEST",
            other => other,
        };
        fields.push((6004, exchange.to_string()));
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            let refs: Vec<(u32, &str)> = fields.iter().map(|(t, v)| (*t, v.as_str())).collect();
            let _ = conn.send_fix(&refs);
            log::info!("Sent secdef request by conId: req_id={} con_id={} exchange={}", req_id, con_id, exchange);
            hb.last_ccp_sent = Instant::now();
        } else {
            log::warn!("secdef request req_id={} queued with no CCP socket", req_id);
        }
        self.pending_secdef.push((req_id, true, Instant::now() + SECDEF_TIMEOUT));
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: ReqId, symbol: &str, sec_type: &str, exchange: &str, currency: &str, filters: &crate::types::SecDefFilters, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let strike = filters.strike_text();
        let (sec_type, continuous, with_futures) = lookup_sec_type(sec_type, filters);
        if continuous {
            log::info!("Requested continuous futures contract details: req_id={}", req_id);
            self.pending_continuous.retain(|c| c.req_id != req_id);
            self.pending_continuous.push(ContinuousLookup { req_id, with_futures, kept: None });
        }
        let lookup = SymbolLookup {
            symbol: symbol.to_string(),
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            currency: currency.to_string(),
            filters: filters.clone(),
            continuous,
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
        // By-symbol lookup: master reply carries the valid exchanges list. The
        // server never emits a last-answer response type; completion is detected
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
        // The internal name of the request, which the connection renamed in
        // the session series (ibx#698).
        let response_req_id = contracts::secdef_response_req_id(msg)
            .map(|rid| ccp_conn.as_ref().map_or_else(|| rid.clone(), |c| c.internal_name(&rid)));
        // The market rules and each contract's rule, from every reply, the
        // engine's own lookups too: a contract given by its conId alone is
        // known by such a lookup only (ibx#615).
        let rules = contracts::parse_market_rules(msg);
        if !rules.is_empty() {
            shared.reference.push_market_rules(rules);
        }
        // The reply's records, read once for every use below (ibx#701).
        let records = contracts::parse_secdef_records(msg).unwrap_or_default();
        note_contract_rules(&records, shared);
        // Asked for an order's outside RTH (ibx#465): not a user reply.
        if let Some(rid) = response_req_id.as_deref() {
            // Asked to build a combo (ibx#470): not a user reply.
            if let Some(progress) = context.combos.secdef_reply(rid, msg, &chrono_free_timestamp()) {
                super::order_builder::combo_progress(context, ccp_conn, hb, progress);
                return;
            }
            // The zone of the contract's trading hours, from the order
            // lookups too: an order's start time is shown in it (ibx#598).
            let keep_zone = |shared: &SharedState| {
                if let Some(def) = records.first()
                    && let Some(zone) = &def.time_zone_id
                {
                    shared.reference.cache_time_zone_id(def.con_id, zone);
                }
            };
            if super::order_builder::rth_definition_reply(context, rid, msg) {
                // The contract an openOrder shows for an order given by
                // its conId (ibx#666).
                if let Some(def) = records.first() {
                    self.cache_definition(def, shared);
                }
                // Known from the order's lookup (ibx#732): an option
                // order's model asks no second definition of it (ibx#730;
                // observed on paper, 13-16/10/2026, 4 first option orders
                // of 4).
                super::farm::note_answer(context, &records, msg);
                keep_zone(shared);
                if let Some(con_id) = self.defer_order_schedule(records.first(), shared, context) {
                    context.zone_pending.insert(con_id);
                }
                return;
            }
            // The record of a contract an order needs (ibx#706), and the
            // clearing rule it may ask then (ibx#709).
            if super::order_builder::order_record_reply(context, rid, msg) {
                if let Some(def) = records.first() {
                    self.cache_definition(def, shared);
                }
                super::farm::note_answer(context, &records, msg);
                if let Some(conn) = ccp_conn.as_mut() {
                    super::order_builder::send_clearing_rules(context, conn, hb);
                }
                return;
            }
            if super::order_builder::order_clearing_reply(context, rid) {
                return;
            }
            // A step of the CFD check after a volatility option order's
            // refusal (ibx#729).
            if super::order_builder::cfd_check_reply(context, ccp_conn.as_mut(), hb, rid, msg) {
                return;
            }
            // A derivative order's lookups of its underlying, and the index
            // lookups of a futures option order's model (ibx#730).
            if super::order_builder::underlying_reply(context, ccp_conn.as_mut(), hb, rid, msg) {
                return;
            }
            if super::farm::index_walk_reply(context, ccp_conn.as_mut(), hb, rid, msg) {
                return;
            }
            // Asked for the contract of an order given without a conId
            // (ibx#486): its definition is cached, not a user reply.
            if super::order_builder::order_contract_reply(context, shared, rid, msg) {
                if let Some(def) = records.first() {
                    self.cache_definition(def, shared);
                }
                // Known from the order's lookup (ibx#732): an option
                // order's model asks no second definition of it (ibx#730;
                // observed on paper, 13-16/10/2026, 4 first option orders
                // of 4).
                super::farm::note_answer(context, &records, msg);
                keep_zone(shared);
                if let Some(con_id) = self.defer_order_schedule(records.first(), shared, context) {
                    context.zone_pending.insert(con_id);
                }
                return;
            }
            // Asked for a round lot (ibx#287): not a user reply.
            if super::farm::round_lot_reply(context, rid, msg) { return; }
            // Asked for a SmartDepth component (#452): not a user reply.
            // The class it names on its exchange is asked at once (ibx#734;
            // observed on paper, 13/10/2026: AAPL's depth on SMART, the 5
            // components naming a class, each within 1 ms of its answer).
            if super::farm::depth_component_reply(context, rid, msg) {
                self.ask_class_permissions(&records, context, ccp_conn, hb);
                return;
            }
            // Asked for the conId of a market data request (ibx#278). The
            // class of the contract found: an option's once its company
            // answered, the others' at once (ibx#734; observed on paper,
            // 13-16/10/2026: 9 option requests after the company's answer;
            // 6 stocks off the US markets and 2 futures 0 to 6 ms after the
            // lookup's answer; nothing for an index).
            if super::farm::md_contract_reply(context, shared, rid, msg) {
                self.ask_option_company(&records, ccp_conn, hb);
                self.ask_found_class(&records, true, context, ccp_conn, hb);
                return;
            }
            // Asked for the conId of a historical-data request (ibx#427).
            // Its contract is known from then on (ibx#732): the request
            // asks no second definition for its route. The class of the
            // contract found is asked at once (ibx#734; observed on paper,
            // 15/10/2026: 3 historical requests by description on stocks
            // off the US markets, 1 to 3 ms after the lookup's answer).
            let for_client = self.resolve_for_client(rid);
            if self.contract_resolve_reply(rid, &records, shared) {
                super::farm::note_answer(context, &records, msg);
                if for_client {
                    self.ask_found_class(&records, false, context, ccp_conn, hb);
                }
                return;
            }
            // Asked for the option of an option calculation (ibx#442).
            if self.optcalc.secdef_reply(rid, msg, ccp_conn, hb, shared) { return; }
        }
        // The company lookup of an underlying (ibx#436): its industry is
        // kept for the later rows of its derivatives; not a user reply.
        if let Some(i) = response_req_id.as_deref().and_then(|rid| self.pending_company.iter().position(|(id, _)| id == rid)) {
            let (_, under) = self.pending_company.swap_remove(i);
            let company = records.iter().find(|d| d.con_id == under)
                .map(|d| (d.industry.clone(), d.category.clone(), d.subcategory.clone()))
                .unwrap_or_default();
            self.company_by_underlying.insert(under, company);
            for (i, def) in records.iter().enumerate() {
                if !records[..i].iter().any(|d| d.con_id == def.con_id) {
                    self.cache_definition(def, shared);
                }
            }
            // The classes of the derivatives that waited for it, before
            // anything else of their requests (ibx#734).
            self.ask_classes_after_company(under, context, ccp_conn, hb);
            // A contract details row's underlying not known when the
            // company was asked: its preferred definition now, unless an
            // answer gave it meanwhile (ibx#733).
            if self.preferred_after_company.remove(&under) && !context.agg_groups.contains_key(&under) {
                self.send_underlying_preferred(under, context, ccp_conn, hb);
            }
            // The real-time bar requests that waited for it go on
            // (ibx#733).
            release_company_parked(context, under);
            self.release_held_details(shared, event_tx, ccp_conn, hb);
            return;
        }
        // The preferred definition of an underlying (ibx#733): known from
        // now on, as every answer; the contract details that waited for it
        // go on.
        if let Some(i) = response_req_id.as_deref().and_then(|rid| self.pending_preferred.iter().position(|(id, _)| id == rid)) {
            self.pending_preferred.swap_remove(i);
            super::farm::note_answer(context, &records, msg);
            for (i, def) in records.iter().enumerate() {
                if !records[..i].iter().any(|d| d.con_id == def.con_id) {
                    self.cache_definition(def, shared);
                }
            }
            self.release_held_details(shared, event_tx, ccp_conn, hb);
            return;
        }
        // A reply can list one conId once per exchange: the contract cache
        // keeps the first record of each conId, the one on the lookup's own
        // exchange, not the last listed exchange. The portfolio's batch
        // lookup fills only the contracts not known yet: a contract known
        // from the day's trade reports stays as an order shows it (ibx#705;
        // the reference 10.51's recording of the session start: AAPL's
        // openOrder with no primary exchange after the batch).
        let batch = response_req_id.as_deref()
            .is_some_and(|r| r.starts_with(crate::control::contracts::SECDEF_SMALL_BATCH_NAME));
        for (i, def) in records.iter().enumerate() {
            if records[..i].iter().any(|d| d.con_id == def.con_id) {
                continue;
            }
            if batch && shared.reference.get_contract(def.con_id).is_some() {
                // Its position row still shows the definition's primary
                // exchange and trading class (the reference 10.51, paper,
                // regular hours 12/11/2026: SPY, sold on another venue
                // earlier that day, in a position row of a later process).
                shared.reference.note_position_venue(def.con_id, &def.primary_exchange, &def.trading_class);
            } else {
                self.cache_definition(def, shared);
            }
        }
        let Some(rid) = response_req_id else { return };
        // The lookup of an option chain leg (ibx#440): not a user reply.
        let connected = !self.disconnected;
        if self.optparams.leg_reply(&rid, ccp_conn, connected, hb, shared) { return; }

        let fanout_idx = self.pending_fanout.iter().position(|p| p.waits_for(&rid));
        if let Some(idx) = fanout_idx {
            let fanout = &mut self.pending_fanout[idx];
            let Some((con_id, exchange)) = fanout.answered(&rid) else { return };
            fanout.deadline = Instant::now() + SECDEF_TIMEOUT;
            let done = fanout.outstanding.is_empty();
            if let Some(rule) = records.iter().find(|d| d.con_id == con_id).and_then(|d| d.market_rule_id) {
                self.market_rule_by_exchange.insert((con_id, exchange), rule);
            }
            // The class of the contract on that exchange, at once (ibx#734;
            // observed on paper, 11-16/10/2026: every per-exchange answer of
            // a contract details request naming a class not asked yet, 134
            // of them, options, stocks and a CFD; most within 2 ms).
            self.ask_class_permissions(&records, context, ccp_conn, hb);
            // The rows ready at the front leave now; the end comes with
            // the last of them (ibx#689).
            let ready = self.pending_fanout[idx].ready_rows();
            let api_req_id = self.pending_fanout[idx].api_req_id;
            if done {
                let fanout = self.pending_fanout.swap_remove(idx);
                let rows: Vec<_> = ready.into_iter().chain(fanout.records).collect();
                self.deliver_contract_rows(api_req_id, rows, None, shared, event_tx, ccp_conn, hb);
            } else if !ready.is_empty() {
                self.deliver_contract_rows(api_req_id, ready, None, shared, event_tx, ccp_conn, hb);
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
        // Every contract of a contract details answer, of the portfolio's
        // batch and of the other lookups by conId is known from now on,
        // record by record (ibx#732): the contract cache above keeps what
        // an order shows, this what the engine's lookups need.
        super::farm::note_answer(context, &records, msg);
        let mut records = records;
        // A CONTFUT request (ibx#438): the continuous future records are
        // kept; with FUT+CONTFUT the futures lookup follows and its rows
        // come after them.
        let mut continuous: Vec<crate::control::contracts::ContractDefinition> = Vec::new();
        if let Some(i) = self.pending_continuous.iter().position(|c| c.req_id == req_id) {
            match self.pending_continuous[i].kept.take() {
                None => {
                    for def in &mut records {
                        def.continuous = true;
                    }
                    if self.pending_continuous[i].with_futures {
                        self.pending_continuous[i].kept = Some(records);
                        let lookup = self.pending_lookups.iter().find(|l| l.req_id == req_id).map(|l| l.lookup.clone());
                        if let Some(lookup) = lookup {
                            let futures = SymbolLookup { continuous: false, ..lookup };
                            if let Some(l) = self.pending_lookups.iter_mut().find(|l| l.req_id == req_id) {
                                l.lookup = futures.clone();
                            }
                            self.send_symbol_lookup(req_id, &futures, "", ccp_conn, hb);
                        }
                        return;
                    }
                    self.pending_continuous.swap_remove(i);
                }
                Some(kept) => {
                    self.pending_continuous.swap_remove(i);
                    continuous = kept;
                }
            }
        }
        if records.is_empty() && continuous.is_empty() {
            // No record: never a conId 0 row (ibx#400).
            self.no_security_definition(req_id, shared, ccp_conn, hb);
            return;
        }
        let lookup = self.take_lookup(req_id);
        let multiplier = lookup.as_ref().and_then(|l| l.filters.multiplier.parse::<f64>().ok());
        // The request symbol is the CUSIP of a bond row when it reads as
        // one (as the reference does, ibx#436).
        if let Some(cusip) = lookup.as_ref().map(|l| l.symbol.as_str()).filter(|s| crate::control::contracts::reads_as_cusip(s)) {
            for def in records.iter_mut() {
                def.lookup_cusip = cusip.to_string();
            }
        }
        // With several records, a requested multiplier keeps only the
        // records that have it, as the reference.
        if let (true, Some(m)) = (records.len() > 1, multiplier) {
            records.retain(|d| d.multiplier == m);
            if records.is_empty() && continuous.is_empty() {
                push_not_found(req_id, shared);
                return;
            }
        }
        if !continuous.is_empty() {
            continuous.append(&mut records);
            records = continuous;
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
            // The definition of a completed order's contract: its trading
            // hours are asked next, as the reference (07/10/2026: IBM's
            // right after its definition; ibx#695).
            if self.completed.lookups.remove(&req_id) {
                self.ask_order_schedule(records.first(), shared, ccp_conn, hb);
            }
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
        // A derivative without an industry takes the one of its
        // underlying's company when known; else the company is looked up
        // and only the later rows get it, as the reference (ibx#436).
        // An underlying not known yet has its preferred definition asked
        // once the company answered (ibx#733).
        // A future asks no company: its underlying's preferred definition,
        // when the underlying is not known, comes straight after the
        // question on the future's class, and the rest waits for it, as
        // the reference (ibx#735; paper, 13/10/2026: MES contract details,
        // 3 of 3, the class 0 to 3 ms after the future's answer, the
        // preferred definition 4 to 15 ms after it, the trading hours 4 to
        // 13 ms after its answer).
        let mut futures_preferred: Vec<i64> = Vec::new();
        for def in records.iter_mut().filter(|d| d.under_con_id > 0 && d.industry.is_empty()) {
            let under = def.under_con_id;
            if let Some(company) = self.company_by_underlying.get(&under) {
                crate::control::contracts::apply_company(def, company);
                continue;
            }
            if def.wire_sec_type == "FUT" {
                if !context.agg_groups.contains_key(&under)
                    && !self.pending_preferred.iter().any(|(_, u)| *u == under)
                    && !futures_preferred.contains(&under)
                {
                    futures_preferred.push(under);
                }
                continue;
            }
            if !self.pending_company.iter().any(|(_, u)| *u == under) {
                self.send_company_lookup(under, ccp_conn, hb);
                if !context.agg_groups.contains_key(&under) {
                    self.preferred_after_company.insert(under);
                }
            }
        }
        // The classes the rows name (ibx#734), a derivative's once its
        // underlying's company answered.
        self.ask_class_permissions(&records, context, ccp_conn, hb);
        for under in futures_preferred {
            self.send_underlying_preferred(under, context, ccp_conn, hb);
        }
        let by_symbol = !single_shot && !contracts::secdef_response_is_last(msg);
        // The rest of the request (its per-exchange lookups, its trading
        // hours, its rows) waits for the company's answer and, when asked,
        // for the underlying's preferred definition, as the reference
        // (ibx#733; observed on paper, recordings of 11-15/10/2026: 11
        // contract details requests of 11 that asked a company, by
        // description or by conId; the preferred definition asked 0 to 6
        // ms after the company's answer for the 3 whose underlying was not
        // known before, none for the 8 whose underlying was known; the
        // per-exchange lookups 1 to 26 ms after the last of those answers,
        // 7 of 7).
        if self.waits_for_underlying(&records) {
            log::info!("Contract details req_id={} wait for their underlying's company", req_id);
            self.held_details.push(HeldDetails { req_id, records, by_symbol });
            return;
        }
        self.continue_details(req_id, records, by_symbol, shared, event_tx, ccp_conn, hb);
    }

    /// Whether a row of an answer waits for the company lookup or the
    /// preferred definition of its underlying (ibx#733); a future's row for
    /// the preferred definition alone (ibx#735).
    fn waits_for_underlying(&self, records: &[crate::control::contracts::ContractDefinition]) -> bool {
        records.iter().any(|d| {
            let under = d.under_con_id;
            under > 0 && (d.industry.is_empty() || d.wire_sec_type == "FUT")
                && (self.pending_company.iter().any(|(_, u)| *u == under)
                    || self.pending_preferred.iter().any(|(_, u)| *u == under))
        })
    }

    /// The question on the trading permission of each class the records of
    /// a client request's answer name, the first time the class comes in
    /// the session, as the reference asks it (ibx#734; observed on paper,
    /// recordings of 11-16/10/2026, 205 questions, none asked twice in a
    /// session): at once, in the order the records name them, but for a
    /// derivative whose underlying's company lookup is on its way, whose
    /// class is asked once that answered (contract details, market data,
    /// real-time bars and orders on options, a CFD's contract details: 0 to
    /// 6 ms after the company's answer, before anything else of the
    /// request). An index's class is never asked (7 market data requests
    /// on SPX). Every answer seen was no permission, or none came; the
    /// requests went on all the same; an order reads the answer later.
    pub(crate) fn ask_class_permissions(
        &mut self,
        records: &[crate::control::contracts::ContractDefinition],
        context: &mut Context,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let mut seen: Vec<&str> = Vec::new();
        for def in records {
            let key = def.permission_key.as_str();
            if key.is_empty() || def.wire_sec_type == "IND" || seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if context.trading_permissions.contains_key(key) || context.permissions_asked.contains_key(key) {
                continue;
            }
            let under = def.under_con_id;
            if under > 0 && self.company_pending(under) {
                if !context.classes_after_company.iter().any(|(u, k)| *u == under && k == key) {
                    context.classes_after_company.push((under, key.to_string()));
                }
                continue;
            }
            if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
                super::order_builder::ask_permission(context, conn, hb, key);
            }
        }
    }

    /// The classes that waited for the company lookup of `under`: asked
    /// now, in the order they came (ibx#734).
    fn ask_classes_after_company(&mut self, under: i64, context: &mut Context, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if context.classes_after_company.is_empty() {
            return;
        }
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut context.classes_after_company).into_iter().partition(|(u, _)| *u == under);
        context.classes_after_company = rest;
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            for (_, key) in due {
                super::order_builder::ask_permission(context, conn, hb, &key);
            }
        }
    }

    /// The contract details answers that no longer wait for an
    /// underlying go on, in the order they came (ibx#733).
    fn release_held_details(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let mut i = 0;
        while i < self.held_details.len() {
            if self.waits_for_underlying(&self.held_details[i].records) {
                i += 1;
                continue;
            }
            let held = self.held_details.remove(i);
            self.continue_details(held.req_id, held.records, held.by_symbol, shared, event_tx, ccp_conn, hb);
        }
    }

    /// The preferred definition of a contract details row's underlying,
    /// asked once its company answered when the underlying was not known
    /// (ibx#733), as the API's lookup of a preferred contract: the request
    /// id `PreferredReqByConid{N}`, the security request type the code
    /// sends, one related symbol, the underlying's contract id and the
    /// exchange `ANYEXCH`. Its number is one of the range of the lookups
    /// a request waits for, never a caller's id.
    fn send_underlying_preferred(&mut self, under_con_id: i64, context: &mut Context, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) else { return };
        let rid = super::farm::next_definition_lookup(context, crate::control::contracts::SECDEF_PREFERRED_NAME);
        let con_id_str = under_con_id.to_string();
        let ts = chrono_free_timestamp();
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "c"),
            (fix::TAG_SENDING_TIME, &ts),
            (crate::control::contracts::TAG_SECURITY_REQ_ID, &rid),
            (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
            (146, "1"),
            (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
            (6004, "ANYEXCH"),
        ]);
        hb.last_ccp_sent = Instant::now();
        log::info!("Preferred definition of underlying {} asked ({})", under_con_id, rid);
        self.pending_preferred.push((rid, under_con_id));
    }

    /// The rest of a contract details answer, once nothing holds it: the
    /// per-exchange lookups of a by-symbol answer, else its rows.
    #[allow(clippy::too_many_arguments)]
    fn continue_details(
        &mut self,
        req_id: ReqId,
        records: Vec<crate::control::contracts::ContractDefinition>,
        by_symbol: bool,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        use crate::control::contracts;
        // By-symbol lookup: each record asks for its unknown per-exchange
        // market rules before it becomes a row.
        let mut outstanding: Vec<(String, i64, String)> = Vec::new();
        if by_symbol {
            // Asked once per conId and exchange (ibx#689: a chain has tens
            // of thousands of pairs).
            let mut asked: HashSet<(i64, &str)> = HashSet::new();
            for def in records.iter().filter(|d| d.con_id != 0) {
                for exch in &def.valid_exchanges {
                    if matches!(exch.as_str(), "" | "SMART" | "BEST")
                        || self.market_rule_by_exchange.contains_key(&(def.con_id, exch.clone()))
                        || !asked.insert((def.con_id, exch.as_str()))
                    {
                        continue;
                    }
                    let fid = format!("{}{}", contracts::SECDEF_EXCHANGE_RULE_NAME, self.next_fanout_id);
                    self.next_fanout_id = self.next_fanout_id.wrapping_add(1);
                    if self.fanout_paced {
                        self.fanout_queue.push_back((req_id, fid.clone(), def.con_id, exch.clone()));
                    } else {
                        self.send_fanout_secdef_request(&fid, def.con_id, exch, ccp_conn, hb);
                    }
                    outstanding.push((fid, def.con_id, exch.clone()));
                }
            }
        }
        if outstanding.is_empty() {
            let factor = factor_warning(&records);
            self.deliver_contract_rows(req_id, records, factor, shared, event_tx, ccp_conn, hb);
        } else {
            // The records wait for their exchanges' rules: the warning
            // comes first (ibx#754).
            if let Some(text) = factor_warning(&records) {
                shared.reference.push_historical_error(req_id, FACTOR_WARNING, text);
            }
            log::info!(
                "Secdef by-symbol fan-out: api_req_id={} records={} requests={}",
                req_id, records.len(), outstanding.len(),
            );
            let mut fanout = PendingFanout::new(req_id, outstanding, records, Instant::now() + SECDEF_TIMEOUT);
            fanout.unsent = if self.fanout_paced { fanout.total } else { 0 };
            self.pending_fanout.push(fanout);
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
            continuous: false,
        };
        let strike = symbol_lookup.filters.strike_text();
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            send_symbol_lookup_on(conn, ReqId::from(lookup_id), &symbol_lookup, &strike);
            hb.last_ccp_sent = Instant::now();
            log::info!("Contract lookup {} for historical req_id={}: symbol={} sec_type={}",
                lookup_id, req_id, symbol_lookup.symbol, symbol_lookup.sec_type);
        } else {
            log::warn!("Contract lookup {} for req_id={} queued with no auth connection", lookup_id, req_id);
        }
        // No deadline: the request waits for the lookup's answer, as in
        // the reference (ibx#485).
        self.pending_resolves.push(PendingResolve { lookup_id, req_id, request });
    }

    /// Look up the conId of a display group update (ibx#424), as the
    /// reference does for a contract it has not seen. The answer is read
    /// by `contract_resolve_reply`.
    pub(crate) fn start_display_group_lookup(
        &mut self,
        req_id: ReqId,
        con_id: i64,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let lookup_id = HIST_LOOKUP_FIRST_ID + self.next_resolve_id % HIST_LOOKUP_IDS;
        self.next_resolve_id = self.next_resolve_id.wrapping_add(1);
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            let name = format!("{}{}", crate::control::contracts::SECDEF_MSG_NAME, lookup_id);
            let con_id_str = con_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &name),
                (321, "2"),
                (146, "1"),
                (6008, &con_id_str),
                (6004, "ANYEXCH"),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Contract lookup {} for display group req_id={}: con_id={}", lookup_id, req_id, con_id);
        } else {
            log::warn!("Contract lookup {} for req_id={} queued with no auth connection", lookup_id, req_id);
        }
        self.pending_resolves.push(PendingResolve {
            lookup_id,
            req_id,
            request: crate::types::ControlCommand::DisplayGroupLookup { req_id, con_id },
        });
    }

    /// Whether a lookup is one of a client request by description (not a
    /// display group's), whose answer asks its class (ibx#734).
    fn resolve_for_client(&self, lookup_id: &str) -> bool {
        let Some(number) = crate::control::contracts::secdef_request_number(lookup_id) else { return false };
        self.pending_resolves.iter().any(|p| ReqId::from(p.lookup_id) == number
            && !matches!(p.request, crate::types::ControlCommand::DisplayGroupLookup { .. }))
    }

    /// The class of the one contract a client request's lookup by
    /// description found (ibx#734): its record on the lookup's exchange,
    /// the first one. None when the lookup found several contracts. An
    /// option of a request that asks its company (`option_company`) is
    /// asked once the company answered; another derivative (an option of a
    /// request that asks no company, a futures option, a warrant, a CFD)
    /// is left: not seen.
    fn ask_found_class(
        &mut self,
        records: &[crate::control::contracts::ContractDefinition],
        option_company: bool,
        context: &mut Context,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let Some(first) = records.iter().find(|d| d.con_id != 0) else { return };
        if records.iter().any(|d| d.con_id != 0 && d.con_id != first.con_id) {
            return;
        }
        if first.under_con_id > 0 && first.wire_sec_type != "FUT" && !(option_company && first.wire_sec_type == "OPT") {
            return;
        }
        self.ask_class_permissions(std::slice::from_ref(first), context, ccp_conn, hb);
    }

    /// The answer to a contract lookup of a historical-data request
    /// (ibx#427): exactly one contract releases the request with its
    /// conId; none or several give error 200 and no query, as the
    /// reference. False when the reply is not for such a lookup. The reply's
    /// market rules are kept by the caller.
    fn contract_resolve_reply(&mut self, lookup_id: &str, records: &[crate::control::contracts::ContractDefinition], shared: &SharedState) -> bool {
        // The reply names the lookup as it was asked; its number is the key.
        let Some(number) = crate::control::contracts::secdef_request_number(lookup_id) else { return false };
        let Some(idx) = self.pending_resolves.iter().position(|p| ReqId::from(p.lookup_id) == number) else { return false };
        let pending = self.pending_resolves.swap_remove(idx);
        if matches!(pending.request, crate::types::ControlCommand::DisplayGroupLookup { .. }) {
            // A display group update (ibx#424): a contract is kept and gives
            // no answer, none gives the reference's error.
            match records.iter().find(|def| def.con_id != 0) {
                Some(def) => self.cache_definition(def, shared),
                None => {
                    let (code, text) = crate::client_core::DISPLAY_GROUP_NO_INSTRUMENT;
                    shared.reference.push_historical_error(pending.req_id, code, text.to_string());
                }
            }
            return true;
        }
        // A tick-by-tick request with no exchange whose lookup lists the
        // contract more than once (on several exchanges): 200, the
        // description is ambiguous (the reference 10.51, paper 15/10/2026:
        // AAPL with no exchange, answered at +0.1 s, ibx#702).
        if let crate::types::ControlCommand::SubscribeTbt { exchange, symbol, .. } = &pending.request
            && exchange.is_empty()
            && records.iter().filter(|def| def.con_id != 0).count() > 1
        {
            log::info!("Contract lookup {}: req_id={} {} records with no exchange: ambiguous", lookup_id, pending.req_id, records.len());
            shared.reference.push_historical_error(pending.req_id, 200,
                format!("The contract description specified for {} is ambiguous.", symbol));
            return true;
        }
        let mut con_ids: Vec<i64> = Vec::new();
        for def in records {
            if def.con_id != 0 && !con_ids.contains(&def.con_id) {
                con_ids.push(def.con_id);
            }
        }
        if let [con_id] = con_ids.as_slice() {
            log::info!("Contract lookup {}: req_id={} con_id={}", lookup_id, pending.req_id, con_id);
            // A contract of market 1 (its schedule key "1/...", the US
            // exchanges) has the zone US/Eastern without its schedule being
            // asked: the end of a daily-bar request by symbol is written in
            // it. Other markets' zones are not known until their schedule
            // comes (the reference 10.51, paper 15/10/2026: AAPL in
            // US/Eastern; SAP, VOD, 7203, RY and BHP in the machine's zone;
            // ibx#703).
            if matches!(pending.request, crate::types::ControlCommand::FetchHistorical { .. })
                && shared.reference.time_zone_id(*con_id).is_none()
                && records.iter().find(|d| d.con_id == *con_id).is_some_and(|d| d.join_key.starts_with("1/"))
            {
                shared.reference.cache_time_zone_id(*con_id, "US/Eastern");
            }
            // The contract is kept: the query names it by its local symbol
            // (ibx#719; the reference 10.51, paper, 16/10/2026: "BTC.USD"
            // for BTC given by symbol).
            if shared.reference.get_contract(*con_id).is_none()
                && let Some(def) = records.iter().find(|d| d.con_id == *con_id)
            {
                self.cache_definition(def, shared);
            }
            self.resolved_requests.push(request_with_con_id(pending.request, *con_id));
        } else {
            log::info!("Contract lookup {}: req_id={} found {} contracts", lookup_id, pending.req_id, con_ids.len());
            shared.reference.push_historical_error(pending.req_id, 200, NO_SECURITY_DEFINITION.to_string());
        }
        true
    }

    /// Contract cache entry of a definition, for every reply (rows or not).
    fn cache_definition(&mut self, def: &crate::control::contracts::ContractDefinition, shared: &SharedState) {
        if def.con_id == 0 {
            return;
        }
        let sec_type = def.api_type_name();
        // A derivative's terms, as the reference shows the contract of an
        // order given by its conId (ibx#666; paper 15/10/2026, a SAP
        // warrant: its date, strike, right and multiplier "0.1").
        // A metal and a fund have none either (the reference 10.51, paper,
        // 16/10/2026: openOrder of XAUUSD and VFIAX with no multiplier).
        let derivative = !matches!(sec_type.as_str(), "STK" | "CASH" | "IND" | "BAG" | "CMDTY" | "FUND");
        shared.reference.cache_contract(def.con_id, api::Contract {
            con_id: def.con_id,
            symbol: def.symbol.clone(),
            exchange: def.exchange.clone(),
            currency: def.currency.clone(),
            local_symbol: def.local_symbol.clone(),
            primary_exchange: def.primary_exchange.clone(),
            trading_class: def.trading_class.clone(),
            last_trade_date_or_contract_month: if derivative { def.last_trade_date.clone() } else { String::new() },
            strike: if derivative { def.strike } else { 0.0 },
            right: match def.right.filter(|_| derivative) {
                Some(crate::control::contracts::OptionRight::Call) => "C".into(),
                Some(crate::control::contracts::OptionRight::Put) => "P".into(),
                None => String::new(),
            },
            // A definition with no multiplier shows none (the reference
            // 10.51, paper, 17/10/2026: a treasury bill's openOrder).
            multiplier: if derivative && def.multiplier > 0.0 && !def.multiplier_text.is_empty() {
                format!("{}", def.multiplier)
            } else {
                String::new()
            },
            sec_type,
            ..Default::default()
        });
        // The suffix of the primary exchange, which the 399 text names
        // (ibx#486).
        shared.reference.cache_market_name(def.con_id, &def.primary_suffix);
        self.try_release_scanner_enrichments(def.con_id, shared);
        if self.completed.defs.remove(&def.con_id) && self.completed.defs.is_empty() && self.completed.ended {
            self.completed_deliver(shared);
        }
    }

    /// reqCompletedOrders (ibx#695): answered from the session's answer
    /// when there is one, else asked of the server, once for the requests
    /// that come while it is read.
    pub(crate) fn request_completed_orders(
        &mut self, api_only: bool, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        if let Some(rows) = &self.completed.kept {
            push_completed_rows(rows, api_only, shared);
            return;
        }
        self.completed.waiting.push(api_only);
        if self.completed.reading.is_some() {
            return;
        }
        // The trading day: from midnight to midnight on the machine's
        // clock, in UTC (the reference at 10:59 Paris on 15/10/2026: from
        // 14/10 22:00 to 15/10 22:00).
        let (from, to) = match jiff::Zoned::now().start_of_day() {
            Ok(day) => {
                let fmt = |z: &jiff::Zoned| z.timestamp().to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d-%H:%M:%S").to_string();
                let next = day.checked_add(jiff::Span::new().days(1)).unwrap_or_else(|_| day.clone());
                (fmt(&day), fmt(&next))
            }
            Err(_) => (String::new(), String::new()),
        };
        if let Some(conn) = ccp_conn.as_mut() {
            let now = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "H"), (fix::TAG_SENDING_TIME, &now), (11, "*"), (55, "*"), (54, "*"),
                (6533, "1"), (6536, &from), (6537, &to),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Completed orders asked: {} to {}", from, to);
        } else {
            log::warn!("Completed orders: no auth connection; the request waits for its answer");
        }
        self.completed.reading = Some(Vec::new());
    }

    /// The end of the server's answer: the definitions of the contracts
    /// not known yet are asked, as the reference asks them, and the rows
    /// wait for them (at most `COMPLETED_DEFS_WAIT`).
    fn completed_end(&mut self, shared: &SharedState) {
        self.completed.ended = true;
        if self.completed.defs.is_empty() {
            self.completed_deliver(shared);
            return;
        }
        log::info!("Completed orders: the answer waits for {} contract definitions", self.completed.defs.len());
        self.completed.deadline = Some(Instant::now() + COMPLETED_DEFS_WAIT);
    }

    /// The answer, given to each request waiting for it, and kept.
    fn completed_deliver(&mut self, shared: &SharedState) {
        let mut rows = self.completed.reading.take().unwrap_or_default();
        for row in &mut rows {
            if let Some(known) = shared.reference.get_contract(row.0.con_id) {
                row.0.trading_class = known.trading_class;
            }
        }
        for api_only in std::mem::take(&mut self.completed.waiting) {
            push_completed_rows(&rows, api_only, shared);
        }
        log::info!("Completed orders: {} rows given", rows.len());
        self.completed.kept = Some(rows);
        self.completed.ended = false;
        self.completed.defs.clear();
        self.completed.to_ask.clear();
        self.completed.deadline = None;
    }

    /// An answer whose definitions did not all come in time is given as it
    /// is.
    pub(crate) fn sweep_completed_orders(&mut self, shared: &SharedState, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        for con_id in std::mem::take(&mut self.completed.to_ask) {
            let req_id = self.next_internal_secdef_id;
            self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1).max(0xF000_0000);
            if let Some(conn) = ccp_conn.as_mut() {
                // Named in the session series of the by-conId lookups on
                // the wire; its answer maps back to this name (ibx#698).
                let name = format!("{}{}", crate::control::contracts::SECDEF_LIVE_ORDER_NAME, req_id);
                let con_id_str = con_id.to_string();
                let ts = chrono_free_timestamp();
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
                    (6008, &con_id_str), (6004, "BEST"),
                ]);
                hb.last_ccp_sent = Instant::now();
            }
            self.pending_secdef.push((ReqId::from(req_id), true, Instant::now() + SECDEF_TIMEOUT));
            self.completed.lookups.insert(ReqId::from(req_id));
        }
        if self.completed.ended && self.completed.deadline.is_some_and(|d| Instant::now() >= d) {
            log::warn!("Completed orders: {} definitions did not come in time", self.completed.defs.len());
            self.completed_deliver(shared);
        }
    }

    /// The rows of a lookup, in record order (ibx#435): each record gets its
    /// market rule ids, and waits for its trading schedule when it has a
    /// join key (one schedule request per key). The end follows the last row.
    ///
    /// As the reference (ibx#527): a record whose schedule was received
    /// earlier in the session is given at once, before the records that
    /// wait, and asks nothing; the waiting ones follow in record order.
    /// `factor`: the answer's factor warning (ibx#754), None for the rows
    /// of a fan-out, whose warning went when it started.
    fn deliver_contract_rows(
        &mut self,
        req_id: ReqId,
        records: Vec<crate::control::contracts::ContractDefinition>,
        factor: Option<String>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        let mut subscribed: Vec<String> = Vec::new();
        let (bond_api, ev_api) = shared.reference.contract_details_features();
        for mut def in records {
            def.market_rule_ids = self.market_rule_ids(&def);
            def.bond_api = bond_api;
            def.ev_api = ev_api;
            if def.join_key.is_empty() {
                push_contract_row(shared, event_tx, req_id, def);
            } else if let Some((_, known)) = self.schedules.get(&def.join_key).filter(|(day, _)| *day == utc_day()) {
                if let Some(sched) = known {
                    crate::control::contracts::apply_schedule(&mut def, sched, jiff::Timestamp::now());
                }
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
                    ready: false,
                });
            }
        }
        // The warning of the records trading on a price with a factor
        // follows the rows given at once: before the rows that wait for
        // their schedule, after the end when none waits (ibx#754).
        let waits = self.pending_schedule_pair.iter().any(|p| p.api_req_id == req_id)
            || self.pending_fanout.iter().any(|p| p.api_req_id == req_id);
        if let Some(text) = factor.as_ref().filter(|_| waits) {
            shared.reference.push_historical_error(req_id, FACTOR_WARNING, text.clone());
        }
        self.end_if_complete(req_id, shared, event_tx);
        if let Some(text) = factor.filter(|_| !waits) {
            shared.reference.push_historical_error(req_id, FACTOR_WARNING, text);
        }
    }

    /// contract_details_end once no row of the lookup waits for its schedule.
    fn end_if_complete(&self, req_id: ReqId, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        // Not while a fan-out still holds rows of the request (ibx#689).
        if !self.pending_schedule_pair.iter().any(|p| p.api_req_id == req_id)
            && !self.pending_fanout.iter().any(|p| p.api_req_id == req_id)
        {
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
    }

    /// Market rule id of each valid exchange of a record whose rule is
    /// known, comma-joined (ibx#435).
    fn market_rule_ids(&self, def: &crate::control::contracts::ContractDefinition) -> String {
        crate::control::contracts::market_rule_ids(def, &self.market_rule_by_exchange)
    }

    /// The company lookup of an underlying (ibx#436), as the reference
    /// sends it after a derivative record without an industry: a security
    /// definition request with the request id `UnderlyingECNoDupsNDReqByConid{N}`,
    /// the security request type the code sends, one related symbol, the
    /// underlying's contract id and the exchange `ANYEXCH` (captured 28/09/2026).
    /// The company lookup of the underlying of a client's option market
    /// data request given by description, once its lookup answered
    /// (ibx#732): as for a contract details row, when the option's record
    /// has no industry and the underlying's company is neither known nor
    /// asked. The option model asks the underlying's definitions only after
    /// its answer (observed on paper, recordings of 12-16/10/2026: 11
    /// option requests of 11 whose underlying's company was not known yet
    /// asked it 0 to 7 ms after the option's answer; the underlying's two
    /// lookups, when asked, 1 to 8 ms after the company's answer, 5 of 5;
    /// no later request of the session asked it again; no future's request
    /// asked it, 2 of 2).
    pub(crate) fn ask_option_company(&mut self, records: &[crate::control::contracts::ContractDefinition], ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let Some(def) = records.iter().find(|d| d.con_id != 0) else { return };
        // An answer of several contracts ended its request (error 200).
        if records.iter().any(|d| d.con_id != 0 && d.con_id != def.con_id) {
            return;
        }
        let under = def.under_con_id;
        if def.sec_type.to_api_str() != "OPT" || under <= 0 || !def.industry.is_empty()
            || self.company_by_underlying.contains_key(&under)
            || self.pending_company.iter().any(|(_, u)| *u == under)
        {
            return;
        }
        self.send_company_lookup(under, ccp_conn, hb);
    }

    /// Whether the company lookup of an underlying is on its way.
    pub(crate) fn company_pending(&self, under_con_id: i64) -> bool {
        self.pending_company.iter().any(|(_, u)| *u == under_con_id)
    }

    pub(crate) fn send_company_lookup(&mut self, under_con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let rid = format!("{}{}", crate::control::contracts::SECDEF_UNDERLYING_NAME, self.next_fanout_id);
        self.next_fanout_id = self.next_fanout_id.wrapping_add(1);
        if let Some(conn) = ccp_conn.as_mut().filter(|_| !self.disconnected) {
            let con_id_str = under_con_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, &rid),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (146, "1"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (6004, "ANYEXCH"),
            ]);
            hb.last_ccp_sent = Instant::now();
            self.pending_company.push((rid, under_con_id));
        }
    }

    /// Send a per-exchange fan-out request after a by-symbol master reply.
    /// Wire: a security definition request with the request id `{fanout_id}`,
    /// the security request type the code sends, one related symbol, the
    /// contract id and the exchange.
    /// Send the fan-out requests whose turn has come (ibx#689): the
    /// reference sends a whole option chain's per-exchange requests about
    /// 2,670 a second (paper, 14/10/2026: 70,354 in 26.3 s, 0.17 ms
    /// apart at the median). Sent at once, a chain's 71,000 requests lost
    /// about one answer in twenty (paper, 17/10/2026), and the request
    /// never ended.
    pub(crate) fn pump_fanout(&mut self, now: Instant, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if self.fanout_queue.is_empty() {
            self.fanout_clock = None;
            return;
        }
        if ccp_conn.is_none() || self.disconnected {
            return;
        }
        let (start, sent) = *self.fanout_clock.get_or_insert((now, 0));
        let due = (now.duration_since(start).as_nanos() / FANOUT_SEND_GAP.as_nanos()) as u64 + 1;
        let mut n = due.saturating_sub(sent);
        while n > 0 {
            let Some((api_req_id, fid, con_id, exchange)) = self.fanout_queue.pop_front() else { break };
            self.send_fanout_secdef_request(&fid, con_id, &exchange, ccp_conn, hb);
            if let Some(f) = self.pending_fanout.iter_mut().find(|f| f.api_req_id == api_req_id) {
                f.unsent = f.unsent.saturating_sub(1);
            }
            if let Some(clock) = self.fanout_clock.as_mut() {
                clock.1 += 1;
            }
            n -= 1;
        }
    }

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

    /// A matching-symbols request (ibx#369): it waits for the pacing of
    /// the reference, which keeps only the latest waiting request (the one
    /// it replaces gets nothing), then goes out with an own request id.
    pub(crate) fn send_matching_symbols_request(
        &mut self,
        req_id: ReqId,
        pattern: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        if let Some((replaced, _)) = self.matching_waiting.replace((req_id, pattern.to_string())) {
            log::info!("Matching symbols request {} replaced by {} before it was sent", replaced, req_id);
        }
        self.pump_matching_symbols(Instant::now(), ccp_conn, hb, shared);
    }

    /// Send the waiting matching-symbols request when the pacing allows it
    /// (ibx#369): a send permit (one request in flight until its pending
    /// mark or its answer) and 1 s after the last send. A request that
    /// cannot be sent gets 10159 at once and is not kept; no pause follows.
    pub(crate) fn pump_matching_symbols(
        &mut self,
        now: Instant,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        while self.matching_waiting.is_some()
            && self.matching_permits > 0
            && self.matching_next_send.is_none_or(|at| now >= at)
        {
            let Some((req_id, pattern)) = self.matching_waiting.take() else { return };
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
                        (58, &pattern),
                    ]);
                    hb.last_ccp_sent = Instant::now();
                    result.is_ok()
                }
            };
            if sent {
                log::info!("Sent matching symbols request: req_id={} id={} pattern='{}'", req_id, wire_id, pattern);
                self.pending_matching_symbols.push((wire_id, req_id));
                self.matching_permits -= 1;
                self.matching_next_send = Some(now + MATCHING_SYMBOLS_SEND_GAP);
            } else {
                log::warn!("Matching symbols request {} not sent: auth connection down", req_id);
                shared.reference.push_historical_error(req_id, 10159, MATCHING_SYMBOLS_SEND_FAILED.to_string());
            }
        }
    }

    /// A matching-symbols reply (ibx#369), matched by the own request id
    /// it echoes only: an unknown id is dropped. A pending mark gives the
    /// send permit back; an answer with an error text is error 10159 with
    /// that text, else the rows; the permit comes back with the answer
    /// unless the pending mark already gave it.
    fn handle_matching_symbols_reply(
        &mut self,
        msg: &[u8],
        parsed: &fix::FieldMap<'_>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let echoed = parsed.get(&320).and_then(|v| v.trim().parse::<u32>().ok());
        let Some(pos) = echoed.and_then(|rid| self.pending_matching_symbols.iter().position(|p| p.0 == rid)) else {
            log::warn!(
                "matching-symbols reply dropped: Unknown request ID {:?} (pending {:?})",
                echoed, self.pending_matching_symbols,
            );
            return;
        };
        let wire_id = self.pending_matching_symbols[pos].0;
        let pending_mark = parsed.get(&8164).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0) != 0;
        if pending_mark {
            log::debug!("matching-symbols request {} pending: send permit given back", wire_id);
            if !self.matching_acked.contains(&wire_id) {
                self.matching_acked.push(wire_id);
            }
            self.matching_permits += 1;
            return;
        }
        let (_, req_id) = self.pending_matching_symbols.remove(pos);
        match parsed.get(&58) {
            Some(text) => shared.reference.push_historical_error(req_id, 10159, format!("{}{}", MATCHING_SYMBOLS_FAILED, text)),
            // An empty result is a legitimate answer ("no such symbol") and
            // is delivered (ibx#228), on the event channel too (ibx#387).
            None => {
                let matches = crate::control::contracts::parse_matching_symbols_response(msg).unwrap_or_default();
                let for_event = clone_for_event(event_tx, &matches);
                shared.reference.push_matching_symbols(req_id, matches);
                if let Some(matches) = for_event {
                    emit(event_tx, Event::SymbolSamples { req_id, matches });
                }
            }
        }
        match self.matching_acked.iter().position(|id| *id == wire_id) {
            Some(i) => { self.matching_acked.swap_remove(i); }
            None => self.matching_permits += 1,
        }
    }

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, _event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        // The FA saves not answered fail (ibx#507).
        super::fa::connection_lost(&mut context.fa);
        // The derivative answers go with the link, as in the reference
        // (ibx#440).
        self.optparams.connection_lost();
        // Matching-symbols requests end without an answer and are not sent
        // again, as in the reference (ibx#369); its send permit is given
        // back, and the waiting request stays.
        self.pending_matching_symbols.clear();
        self.matching_acked.clear();
        self.matching_permits += 1;
        self.awaiting_status_replay = false;
        self.awaiting_login_replay = false;
        self.fill_up_pending = None;
        self.status_replay_end_at = None;
        // A company lookup lost with the link is made again by the next
        // derivative row (ibx#436); so is an underlying's preferred
        // definition. The real-time bar requests that waited for a company
        // go on without it (ibx#733).
        self.pending_company.clear();
        self.preferred_after_company.clear();
        self.pending_preferred.clear();
        // The classes that waited for a company lookup lost with the link
        // are asked by the next request that looks it up (ibx#734).
        context.classes_after_company.clear();
        for (_, request) in std::mem::take(&mut context.company_parked) {
            context.def_ready.push(request);
        }
        // A combo set-up in flight starts again after the new logon
        // (ibx#470): its orders are handled again.
        if context.combos.connection_lost() {
            super::order_builder::combo_progress(context, &mut None, &mut super::HeartbeatState::new(),
                crate::engine::combo::Progress { send: Vec::new(), release: true });
        }
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
            self.fill_up_pending = fill_up.iter().find(|(t, _)| *t == 6556).map(|(_, v)| v.clone());
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

/// The contract of an execution report: the cached one when its conId is
/// known, else the report's own fields.
fn report_contract(parsed: &fix::FieldMap<'_>, shared: &SharedState) -> api::Contract {
    let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
    if con_id != 0 && let Some(cached) = shared.reference.get_contract(con_id) {
        return cached;
    }
    let text = |tag: u32| parsed.get(&tag).map(|s| s.to_string()).unwrap_or_default();
    let sec_type = match parsed.get(&167).copied() {
        Some("CS") | Some("COMMON") => "STK".to_string(),
        _ => text(167),
    };
    api::Contract {
        con_id, symbol: text(55), sec_type, exchange: text(207), currency: text(15),
        local_symbol: text(6035), ..Default::default()
    }
}

/// The status request for one order, after the server refused its cancel
/// or modify (ibx#252): the order's ClOrdID, any symbol and side, and the
/// account, as the reference writes it.
/// The API status of a what-if reply, from its order status (ibx#462):
/// pending new is PreSubmitted (captured 02/10/2026), New and Replaced follow the
/// routing rule of an order's reports, a reject is Inactive.
fn what_if_status(parsed: &fix::FieldMap<'_>) -> &'static str {
    let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
        || parsed.get(&198).is_some_and(|s| *s != "NONE" && !s.is_empty());
    match parsed.get(&39).copied() {
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
        let ledger = text.split(SOH_CHAR).any(|p| p == "35=RL");
        // A ledger frame's rows as numbers: the client builds the
        // reference's rows from them (ibx#486).
        let (rows, ledgers) = if ledger {
            (Vec::new(), parse_ledger_rows(text))
        } else {
            (parse_account_rows(text, shared.reference.form_10_40()).0, Vec::new())
        };
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: sr_id.to_string(),
            rows: rows.into_iter()
                .map(|(key, currency, value)| crate::bridge::AccountRow { key, value, currency, ledger: false })
                .collect(),
            ledger,
            end: false,
            ledgers,
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
        let (rows, time_secs) = parse_account_rows(text, shared.reference.form_10_40());
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
            sr_id: sr_id.to_string(), rows: Vec::new(), ledger: false, end: true, ledgers: Vec::new(),
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

/// The account keys of a `35=RL` row and the ledger tag of each, in the
/// order of the reference's table (ibx#475, ibx#487). The
/// row's currency (8002) is the key's currency. `Currency`, `AccountOrGroup`
/// and `RealCurrency` are texts (tag ""); the insured deposit (8174) is
/// added to CashBalance (the API setting that gives it a key of its own is
/// off by default).
const LEDGER_KEYS: &[(&str, &str)] = &[
    ("", "Currency"),
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
    ("6483", "FundValue"),
    ("6681", "NetDividend"),
    ("6682", "MutualFundValue"),
    ("6683", "MoneyMarketFundValue"),
    ("6684", "CorporateBondValue"),
    ("6685", "TBondValue"),
    ("6686", "TBillValue"),
    ("6687", "WarrantValue"),
    ("6711", "FxCashBalance"),
    ("", "AccountOrGroup"),
    ("", "RealCurrency"),
    ("6924", "IssuerOptionValue"),
    ("8406", "Cryptocurrency"),
];

/// Rows of an account frame as (key, currency, value text), and the latest
/// row time, as the reference reads them (ibx#475):
/// - `UT` / `UM` frames: each row key with its value, currency
///   from the currency field (UT rows have none). A row with no value is skipped. A
///   `PNL` row ends the frame. `AddAccountCode` is skipped, and
///   `AccountCode` is kept only after an `AccountType` row of the same frame
///   (the periodic frames carry an empty one).
/// - `RL` frames: each `LedgerList` row gives the keys of `LEDGER_KEYS` with
///   the row's currency: its currency as `Currency` and
///   `RealCurrency`, the frame's account as `AccountOrGroup`, and each
///   number the row has, written as the reference's account values (two to
///   seven decimals; captured 07/10/2026: exchange rate `1.00`, cash
///   balance `896958.4231`). A field the row has not gives no row.
/// `form_10_40`: the rows of the account frames without the currency of
/// their own field, as 10.40 gave them (ibx#637).
pub(crate) fn parse_account_rows(text: &str, form_10_40: bool) -> (Vec<(String, String, String)>, Option<i64>) {
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
                // A value Java does not read as a number (`nan`) is not set.
                let number = |tag: &str| ledger_values.iter().find(|(t, _)| *t == tag)
                    .and_then(|(_, v)| v.parse::<f64>().ok()).filter(|v| v.is_finite());
                for (tag, name) in LEDGER_KEYS {
                    let text = match *name {
                        "Currency" | "RealCurrency" => Some(currency.to_string()),
                        // The account of the subscription: the client fills it.
                        "AccountOrGroup" => Some(String::new()),
                        "CashBalance" => number(tag).map(|c| c + number("8174").unwrap_or(0.0))
                            .map(crate::client_core::java_account_value),
                        _ => number(tag).map(crate::client_core::java_account_value),
                    };
                    if let Some(text) = text {
                        rows.push((name.to_string(), currency.into(), text));
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
            // The account frames of 10.51 give SettledCashByDate once per
            // currency, BASE and USD, in its own field (ibx#637; paper
            // 13/10/2026).
            "8002" if ledger || !form_10_40 => currency = val,
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

/// The rows of a ledger frame (`RL`) of an account summary, as the
/// reference reads them (ibx#486): the account
/// of the frame (the `AccountCode` key then its value), and per `LedgerList`
/// row its currency (the row's and the currency field), and its numeric
/// fields. A value the reference does not read as a number (`nan`) stays
/// unset.
pub(crate) fn parse_ledger_rows(text: &str) -> Vec<crate::bridge::LedgerRow> {
    let mut rows: Vec<crate::bridge::LedgerRow> = Vec::new();
    let mut account = String::new();
    let mut key = "";
    let mut in_row = false;
    for part in text.split(SOH_CHAR) {
        let Some((tag, val)) = part.split_once('=') else { continue };
        match tag {
            "8001" => {
                key = val;
                in_row = val == "LedgerList";
                if in_row {
                    rows.push(crate::bridge::LedgerRow { account: account.clone(), ..Default::default() });
                }
            }
            "8004" if key == "AccountCode" => account = val.to_string(),
            "8002" if in_row => { if let Some(r) = rows.last_mut() { r.currency = val.to_string(); } }
            "15" if in_row => { if let Some(r) = rows.last_mut() { r.real_currency = val.to_string(); } }
            _ if in_row => {
                if let (Ok(t), Ok(v)) = (tag.parse::<u32>(), val.parse::<f64>())
                    && v.is_finite()
                    && let Some(r) = rows.last_mut()
                {
                    r.values.push((t, v));
                }
            }
            _ => {}
        }
    }
    rows
}

/// The market rule of each contract of a definition reply (ibx#615): the
/// historical prices are given in its steps below session level 224.
fn note_contract_rules(records: &[crate::control::contracts::ContractDefinition], shared: &SharedState) {
    for def in records.iter().filter(|d| d.con_id != 0) {
        if let Some(rule) = def.market_rule_id {
            shared.reference.note_contract_rule(def.con_id, rule as i32);
        }
    }
}

/// The API letter of a hedge kind's number (ibx#614).
fn hedge_letter(kind: &str) -> &'static str {
    match kind { "1" => "F", "2" => "D", "3" => "P", "4" => "B", "5" => "S", _ => "" }
}

/// A hedge's beta or ratio as the reference shows it (ibx#614): a decimal
/// number with at least one decimal ("1.0", "0.5").
fn hedge_param_text(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e7 { format!("{:.1}", v) } else { format!("{}", v) }
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

/// The prices at midnight (ibx#511): a count, then (conId, price) for
/// each position held at midnight.
fn handle_midnight_prices(msg: &[u8], shared: &SharedState) {
    let Ok(text) = std::str::from_utf8(msg) else { return };
    let mut prices = Vec::new();
    let mut con_id: i64 = 0;
    for part in text.split('') {
        if let Some(v) = part.strip_prefix("6008=") {
            con_id = v.parse().unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("8057=") {
            if let (true, Ok(price)) = (con_id != 0, v.parse::<f64>()) {
                prices.push((con_id, price));
            }
        }
    }
    shared.portfolio.set_midnight_prices(prices);
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
                let avg_cost = avg_cost_raw;
                shared.portfolio.set_position_info(PositionInfo {
                    con_id, position_fixed: qty, avg_cost, ..Default::default()
                });
                if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                    shared.portfolio.set_position_fixed(instrument, qty);
                    emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
                }
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
        let avg_cost = avg_cost_raw;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position_fixed: qty, avg_cost, ..Default::default()
        });
        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            shared.portfolio.set_position_fixed(instrument, qty);
            emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
        }
    }
    let _ = (ccp_conn, hb);
    }

    /// The contracts of a portfolio message not known yet, looked up in one
    /// batch on any exchange, in the message's order, as the reference
    /// (ibx#705; the reference 10.51, paper, 16/10/2026: one
    /// `SmallBatchReqByConid` of the login's five contracts, right after its
    /// first portfolio message). The answer fills the contract cache.
    /// After them, the contracts of the P&L seeds that no portfolio row
    /// carries: each is a reqPositions row of size 0 (ibx#742).
    fn look_up_portfolio_contracts(&mut self, msg: Option<&[u8]>, ccp_conn: &mut Option<Connection>, shared: &SharedState, hb: &mut HeartbeatState) {
        let text = msg.and_then(|m| std::str::from_utf8(m).ok()).unwrap_or("");
        let mut cold: Vec<i64> = Vec::new();
        for v in text.split('').filter_map(|p| p.strip_prefix("6008=")) {
            let Ok(con_id) = v.parse::<i64>() else { continue };
            // Every contract of the message not asked yet, also one known
            // from the day's trade reports (the reference asked AAPL and
            // SPY, traded that day).
            if con_id == 0 || cold.contains(&con_id) || self.auto_fetched_conids.contains(&con_id) {
                continue;
            }
            cold.push(con_id);
        }
        let mut seeds: Vec<i64> = shared.portfolio.midnight_seeds().iter().map(|s| s.con_id).collect();
        seeds.sort_unstable();
        for con_id in seeds {
            if con_id == 0 || cold.contains(&con_id) || self.auto_fetched_conids.contains(&con_id)
                || shared.portfolio.position_info(con_id).is_some() || shared.reference.get_contract(con_id).is_some()
            {
                continue;
            }
            cold.push(con_id);
        }
        if cold.is_empty() { return; }
        let Some(conn) = ccp_conn.as_mut() else { return };
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        let name = format!("{}{}", crate::control::contracts::SECDEF_SMALL_BATCH_NAME, req_id);
        let count = cold.len().to_string();
        let ids: Vec<String> = cold.iter().map(|c| c.to_string()).collect();
        let ts = chrono_free_timestamp();
        let mut fields: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts),
            (crate::control::contracts::TAG_SECURITY_REQ_ID, &name),
            (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"), (146, &count),
        ];
        for id in &ids {
            fields.extend([(crate::control::contracts::TAG_IB_CON_ID, id.as_str()), (6004, "ANYEXCH")]);
        }
        let _ = conn.send_fix(&fields);
        hb.last_ccp_sent = Instant::now();
        log::info!("Portfolio contracts looked up in one batch ({}): {:?}", name, cold);
        self.auto_fetched_conids.extend(cold);
        self.pending_secdef.push((ReqId::from(req_id), false, Instant::now() + SECDEF_TIMEOUT));
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
        // The cold conIds in the order of the rows: the lookups go out in
        // that order, as the reference's (ibx#513).
        let mut cold: Vec<i64> = Vec::new();
        for entry in &result.entries {
            let con_id = entry.con_id;
            if con_id == 0 { continue; }
            if shared.reference.get_contract(con_id).is_some() { continue; }
            if awaiting.insert(con_id) {
                cold.push(con_id);
            }
        }
        if awaiting.is_empty() {
            shared.reference.push_scanner_data(api_req_id, result);
            return;
        }
        // Issue one secdef request per cold con_id. If another flow has already
        // requested the same con_id (auto_fetched_conids contains it) we skip
        // the send but still wait — its reply will populate the cache and
        // release this entry via try_release_scanner_enrichments.
        for con_id in cold {
            if !self.auto_fetched_conids.contains(&con_id) {
                let req_id = self.next_internal_secdef_id;
                self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
                self.auto_fetched_conids.insert(con_id);
                self.send_scanner_row_lookup(ReqId::from(req_id), con_id, ccp_conn, hb);
            }
        }
        self.pending_scanner_enrichment.push(PendingScannerEnrichment {
            api_req_id,
            result,
            awaiting,
            deadline: Instant::now() + Duration::from_secs(5),
        });
    }

    /// Called from the security definition answer path after the contract cache has been
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
    // The rows borrow their values from the message: copied only when the
    // message is not UTF-8, with replacements, as each field was (ibx#701).
    let text = String::from_utf8_lossy(msg);
    let mut header = fix::FieldMap::new();
    let mut rows: Vec<fix::FieldMap<'_>> = Vec::new();
    for field in text.split(fix::SOH as char) {
        let Some(eq) = field.find('=') else { continue };
        let Some(tag) = field[..eq].parse::<u32>().ok() else { continue };
        let value = &field[eq + 1..];
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
    parsed: &fix::FieldMap<'_>,
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
    shared.portfolio.note_row_size(con_id, position);
    // Tag map verified against the updatePortfolio callback (ib-agent#172):
    // 6101 = averageCost, 6065 = marketPrice (per share), 6067 = marketValue,
    // 6100 = unrealizedPNL, 6099 = realizedPNL. Earlier code read 6065 as the
    // average cost, which is actually the market price.
    let price_tag = |tag: u32| parsed.get(&tag)
        .and_then(|s| s.parse::<f64>().ok())
        // Rounded: 335.1000061 must not become 335.10000609 (ibx#487).
        .map(|v| (v * PRICE_SCALE as f64).round() as Price)
        .unwrap_or(0);
    let avg_cost: f64 = parsed.get(&6101).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let market_price: Price = price_tag(6065);
    let market_value: Price = price_tag(6067);
    let unrealized_pnl: Price = price_tag(6100);
    let realized_pnl: Price = price_tag(6099);
    // Symbol arrives space-padded; trim trailing whitespace.
    let symbol = parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default();
    let sec_type = parsed.get(&167).map(|s| s.to_string()).unwrap_or_default();
    let currency = parsed.get(&15).map(|s| s.to_string()).unwrap_or_default();
    // Only the multiplier part of the row key; the whole key used to be
    // reported as the contract's multiplier.
    let multiplier = parsed.get(&8002)
        .and_then(|key| {
            let parts: Vec<&str> = key.split('/').collect();
            (parts.len() == 4).then(|| parts[2].to_string())
        })
        .unwrap_or_default();

    // The server's cost is rounded (775.4916963): a position whose own cost
    // it rounds keeps its digits, as the reference's position store keeps
    // 775.4916963075 after a commission (ibx#708).
    let avg_cost = match (shared.portfolio.position_info(con_id), parsed.get(&6101)) {
        (Some(p), Some(text)) if p.position_fixed == position && rounds_to(p.avg_cost, text) => p.avg_cost,
        _ => avg_cost,
    };
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

/// Whether `value` written with the decimals of `text` is `text`.
fn rounds_to(value: f64, text: &str) -> bool {
    let Ok(server) = text.parse::<f64>() else { return false };
    let decimals = text.split_once('.').map_or(0, |(_, d)| d.len()) as i32;
    (value - server).abs() <= 0.5 * 10f64.powi(-decimals) + 1e-12
}

#[cfg(test)]
mod tests {
    use super::*;

    // ibx#604: the type names of the server's order reports, as the
    // reference 10.51 reads them (paper, 19/10/2026 pre-market).
    #[test]
    fn report_order_type_names() {
        for (code, inst, name) in [("P", "R", "REL"), ("P", "M", "PEG MID"), ("P", "P", "PEG MKT"), ("P", "", "TRAIL"),
            ("SP", "", "STP PRT"), ("LT", "", "LIT"), ("TSL", "", "TRAIL LIMIT"), ("MIDPX", "", "MIDPRICE"),
            ("SMKT", "", "SNAP MKT"), ("SMID", "", "SNAP MID"), ("SREL", "", "SNAP PRIM"), ("PB", "R", "PEG BENCH"),
            ("TMIT", "", "TRAIL MIT"), ("TLIT", "", "TRAIL LIT"), ("B", "", "LOC"), ("E2M", "", "PEG BEST"), ("RPI", "", "RPI")] {
            assert_eq!(report_order_type(code, inst), name, "{code} {inst}");
        }
    }

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
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[])), &shared);
        let reports = shared.orders.drain_commission_reports();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.exec_id, "0000e0d5.6ab5f36f.01.01");
        assert_eq!(r.commission_and_fees, 1.0003);
        assert_eq!(r.currency, "USD");
        // 0 when the report has none, as the reference gives them (ibx#543).
        assert_eq!(r.realized_pnl, 0.0);
        assert_eq!(r.yield_amount, 0.0);
        assert_eq!(r.yield_redemption_date, "");
    }

    #[test]
    fn commission_frame_carries_realized_pnl_and_yield() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(6099, "-12.5"), (236, "4.25"), (696, "20301231")])), &shared);
        let r = shared.orders.drain_commission_reports().remove(0);
        assert_eq!(r.realized_pnl, -12.5);
        assert_eq!(r.yield_amount, 4.25);
        assert_eq!(r.yield_redemption_date, "20301231");
    }

    #[test]
    fn commission_frame_duplicates_and_lower_revisions_are_skipped() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")])), &shared);
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")])), &shared);
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01")])), &shared);
        assert_eq!(shared.orders.drain_commission_reports().len(), 1);
        // A higher revision replaces the report.
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.0a"), (6378, "1.5")])), &shared);
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
        ccp.handle_commission_report(&fix::field_view(&frame), &shared);
        assert!(shared.orders.drain_commission_reports().is_empty());
    }

    #[test]
    fn a_fill_carries_its_execution_id() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25")]);
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");
        let fills = shared.orders.drain_fills_with_exec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].1.exec_id, "0000e0d5.6ab5f36f.01.01");
    }

    // ibx#644 (paper 12/10/2026, ex578: 2 fills, 14 commission reports on
    // the event channel): the commission reports of the day's earlier
    // executions, sent after the login with no execution, stay off the
    // event channel; the callbacks' queue still has them. A report that
    // comes before its fill is given right after the `Fill`.
    #[test]
    fn no_commission_report_event_without_its_fill() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);
        let event_tx = Some(tx);
        // An execution of an earlier session: its report only.
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01")])), &shared);
        // This session's fill, its report before it.
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f370.01.01"), (6378, "0.5")])), &shared);
        let fill = exec_report_frame(&[(20, "0"), (39, "1"), (150, "1"), (17, "0000e0d5.6ab5f370.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "1"), (6, "336.25")]);
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &event_tx, "");
        // A later revision of the same execution, after its fill.
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f370.01.02"), (6378, "0.6")])), &shared);
        let events: Vec<String> = rx.try_iter().filter_map(|e| match e {
            Event::Fill(f) => Some(format!("fill {}", f.order_id)),
            Event::CommissionReport(r) => Some(format!("commission {} {}", r.exec_id, r.commission_and_fees)),
            _ => None,
        }).collect();
        assert_eq!(events, ["fill 42", "commission 0000e0d5.6ab5f370.01.01 0.5", "commission 0000e0d5.6ab5f370.01.02 0.6"]);
        // The client's queue has the three reports: the callbacks give
        // the ones whose execution they know.
        assert_eq!(shared.orders.drain_commission_reports().len(), 3);
        assert!(ccp.held_commission_events.iter().all(|(b, _)| b == "0000e0d5.6ab5f36f.01"));
    }

    // ibx#474: the captured fill of 25/09/2026 (clientId 250, orderRef
    // pm0925-fill-BUY): time from the transaction time when the gateway's
    // execution time is absent, exchange from the exchange field, clientId
    // and orderRef from their own fields.
    #[test]
    fn a_fill_carries_its_execution_details() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25"),
            (100, "ARCA"), (207, "ARCA"), (30, "ARCA"), (6119, "250"), (6010, "pm0925-fill-BUY"),
            (60, "20260925-08:49:54"), (52, "20260925-08:49:55.123")]);
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");
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

    // Build a what-if (what-if flag set) ExecReport map for order 42. `margin_fields`
    // holds (field, literal wire value) pairs exactly as the gateway puts them
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
    const WHAT_IF_CLORD: &str = "2147483648.0";

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
    // the pending new status, its reason for the 201 after the open order;
    // no commission.
    #[test]
    fn what_if_reply_values_status_and_reason() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (39, "A"), (58, "YOUR ORDER IS NOT ACCEPTED."), (6094, "823189.11"), (6092, "1093819889.17"),
            (6093, "994381596.62"), (6828, "954395.81"), (6826, "4943.27"), (6827, "4125.25"),
            (8130, "USD"), (6552, "8600"),
        ]);
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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

    // ibx#462: a reply with an
    // order message gives an open order with that warning text, and the
    // preview waits for its data reply.
    #[test]
    fn what_if_order_message_reply_keeps_the_preview() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let event_tx = Some(event_tx);
        let text = "Warning: your order will not be placed at the exchange until 2026-10-02 09:30:00 US/Eastern";
        ccp.handle_exec_report(&fix::field_view(&what_if_frame(&[(39, "A"), (6360, "TIME"), (6361, text), (6378, "0")])),
            &mut context, &shared, &event_tx, "");
        assert!(event_rx.try_recv().is_err(), "the order-message reply is not the preview's answer");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        assert!(!responses[0].final_reply);
        assert_eq!((responses[0].state.warning_text.as_str(), responses[0].state.init_margin_after), (text, None));
        assert!(context.what_ifs.contains_key(WHAT_IF_CLORD), "the preview still waits");
        ccp.handle_exec_report(&fix::field_view(&what_if_frame(&ZERO_CLOSE_FIELDS)), &mut context, &shared, &event_tx, "");
        let responses = shared.orders.drain_what_if_responses();
        assert!(responses[0].final_reply && responses[0].state.warning_text.is_empty());
        match event_rx.try_recv() {
            Ok(Event::WhatIf(answer)) => assert_eq!(answer.state.equity_with_loan_after, Some(945923.47)),
            other => panic!("the data reply is the answer: {other:?}"),
        }
        assert!(context.what_ifs.is_empty());
    }

    // ibx#462: a reject report of a preview
    // (execution type or order status rejected, not a status report) gives
    // error 201 alone; a status report with the rejected status is the
    // preview's reply: the open order with
    // the Inactive status, then error 201 with its reason.
    #[test]
    fn what_if_reject_report_and_rejected_status_report() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let reason = "YOUR ORDER IS NOT ACCEPTED";
        ccp.handle_exec_report(&fix::field_view(&what_if_frame(&[(20, "0"), (150, "8"), (39, "8"), (58, reason)])),
            &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(), "no open order");
        assert_eq!(shared.orders.drain_order_errors(),
            vec![(42, 201, format!("Order rejected - reason:{reason}"))]);
        assert!(context.what_ifs.is_empty());

        let (mut ccp, mut context, shared) = what_if_test_state();
        ccp.handle_exec_report(&fix::field_view(&what_if_frame(&[(20, "3"), (150, "A"), (39, "8"), (58, reason)])),
            &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        assert!(responses[0].final_reply);
        assert_eq!((responses[0].state.status.as_str(), responses[0].state.reject_reason.as_str()), ("Inactive", reason));
        assert!(context.what_ifs.is_empty());
    }

    // ibx#205: a margin-reducing preview (close, cash-account sell) resolves to a
    // post-trade init margin of exactly 0, which the gateway sends as numeric "0"
    // (ib-agent#160). The old `> 0.0` guard dropped it and the caller timed out.
    #[test]
    fn what_if_zero_init_margin_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&ZERO_CLOSE_FIELDS);
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_what_if_responses().len(), 1);
        assert_eq!(context.order(42).map(|o| o.status), Some(crate::types::OrderStatus::Submitted));
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.0"));
        assert!(shared.orders.drain_order_updates().is_empty(), "no status for the preview");

        context.what_ifs.insert(WHAT_IF_CLORD.to_string(), (42, 0));
        let mut reject = what_if_frame(&[]);
        reject.insert(39, "8".to_string());
        reject.insert(150, "8".to_string());
        reject.insert(58, "YOUR ORDER IS NOT ACCEPTED.".to_string());
        ccp.handle_exec_report(&fix::field_view(&reject), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(),
            [(42, 201, "Order rejected - reason:YOUR ORDER IS NOT ACCEPTED.".to_string())]);
        assert!(context.what_ifs.is_empty());
        assert!(context.order(42).is_some());

        // A flagged reply for a preview this session did not send.
        ccp.handle_exec_report(&fix::field_view(&what_if_frame(&ZERO_CLOSE_FIELDS)), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty());
    }

    // ibx#210: a working order carries the wire New status whether it is routed or not.
    // The gateway reports PreSubmitted while it waits (e.g. placed pre-market)
    // and Submitted only once routed to an exchange. The discriminator is the
    // routing fields on the same exec report, not a distinct wire status
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

    // ibx#602 (the reference 10.51 at level 225, paper, 18/10/2026): a
    // cancelled order's remaining quantity is the quantity the server
    // reports for it, less what filled: a beta hedge of 10 sized down to its
    // max size of 2 by the server shows 2.
    #[test]
    fn a_cancelled_orders_remaining_is_the_servers_quantity() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let instrument = context.market.try_register(272093).unwrap();
        context.insert_order(crate::types::Order::new(44, instrument, Side::Sell, 10, 0, b'1', b'0', 0));
        let mut cancel = exec_report_frame(&[(20, "0"), (39, "4"), (150, "4"), (38, "2"), (14, "0"), (151, "0")]);
        cancel.insert(11, "44".to_string());
        ccp.handle_exec_report(&fix::field_view(&cancel), &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        let last = updates.iter().rev().find(|u| u.order_id == 44).expect("a status");
        assert_eq!((last.status, last.remaining_qty_fixed), (crate::types::OrderStatus::Cancelled, 2 * crate::types::QTY_SCALE));
    }

    // ibx#751 (the reference 10.51, paper, regular hours 12/11/2026, a scale
    // order of 2 with a profit taker and auto-reset): a status report of a
    // partly filled order that no venue holds shows PreSubmitted until its
    // next level works; the order's quantity grows with what it has filled
    // and has left; cancelled, its remaining is what was left.
    #[test]
    fn a_scale_order_between_its_levels() {
        use crate::types::{OrderStatus, QTY_SCALE};
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let instrument = context.market.try_register(265598).unwrap();
        context.insert_order(crate::types::Order::new(45, instrument, Side::Buy, 2, 355 * PRICE_SCALE, b'2', b'0', 0));
        let mut report = |pairs: &[(u32, &str)], context: &mut Context| {
            let mut frame = exec_report_frame(pairs);
            frame.insert(11, "45.0".to_string());
            frame.entry(38).or_insert_with(|| "2".to_string());
            ccp.handle_exec_report(&fix::field_view(&frame), context, &shared, &None, "");
            (context.order(45).map(|o| o.status), shared.orders.drain_order_updates().last().map(|u| (u.status, u.filled_qty_fixed / QTY_SCALE, u.remaining_qty_fixed / QTY_SCALE)))
        };
        let fill = |id: &'static str, cum: &'static str, leaves: &'static str| vec![(39, "1"), (150, "1"), (20, "0"), (17, id), (32, "1"), (31, "355.57"),
            (14, cum), (151, leaves), (100, "NASDAQ"), (198, "NASDAQ")];
        assert_eq!(report(&[(39, "6"), (150, "6"), (20, "3"), (198, "NONE"), (14, "0"), (151, "2")], &mut context).0, Some(OrderStatus::PreSubmitted));
        assert_eq!(report(&[(39, "0"), (150, "0"), (20, "0"), (198, "IBKRATS"), (14, "0"), (151, "2")], &mut context).0,
            Some(OrderStatus::Submitted));
        // A level fills: the order stays as it was, then no venue holds it.
        assert_eq!(report(&fill("e1", "1", "1"), &mut context).0, Some(OrderStatus::PartiallyFilled));
        assert_eq!(report(&[(39, "1"), (150, "1"), (20, "3"), (198, "NONE"), (14, "1"), (151, "1")], &mut context),
            (Some(OrderStatus::PreSubmitted), Some((OrderStatus::PreSubmitted, 1, 1))));
        // The next level works, fills, and the order starts again: 2 filled
        // and 2 left of an order of 2.
        assert_eq!(report(&[(39, "5"), (150, "5"), (20, "0"), (198, "NASDAQ"), (14, "1"), (151, "1")], &mut context).0,
            Some(OrderStatus::Submitted));
        report(&fill("e2", "2", "2"), &mut context);
        assert_eq!(report(&[(39, "1"), (150, "1"), (20, "3"), (198, "NONE"), (14, "2"), (151, "2")], &mut context),
            (Some(OrderStatus::PreSubmitted), Some((OrderStatus::PreSubmitted, 2, 2))));
        assert_eq!(shared.orders.get_order_info(45).unwrap().order.total_quantity, 4.0);
        // A fill of an order no venue holds leaves it PreSubmitted.
        assert_eq!(report(&fill("e3", "3", "2"), &mut context).0, Some(OrderStatus::PreSubmitted));
        report(&[(39, "1"), (150, "1"), (20, "3"), (198, "NONE"), (38, "3"), (14, "3"), (151, "2")], &mut context);
        assert_eq!(shared.orders.get_order_info(45).unwrap().order.total_quantity, 5.0);
        // Cancelled: 2 remaining, though the report gives none left.
        let (_, last) = report(&[(11, "45.1"), (41, "45.0"), (39, "4"), (150, "4"), (20, "0"), (198, "NONE"), (38, "3"), (14, "3"), (151, "0")], &mut context);
        assert_eq!(last, Some((OrderStatus::Cancelled, 3, 2)));
    }

    // The reference 10.51, paper, regular hours 12/11/2026 (a fresh process
    // after 30 fills of the day, three of them of orders cancelled since,
    // gave no order at the connection and none to reqAllOpenOrders): an
    // execution of the day given again at the logon makes no order of an
    // order this session does not hold, and no open order of its entry; the
    // execution itself is kept for reqExecutions. A negative API order id
    // of a report is noted for the next profit taker (ibx#751).
    #[test]
    fn an_execution_given_again_at_the_logon_makes_no_order() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        context.market.try_register(265598).unwrap();
        let replayed: std::collections::HashMap<u32, String> = [(11u32, "539615664.0"), (97, "Y"), (17, "0000e0d5.01"), (150, "1"), (20, "0"), (39, "1"),
            (55, "AAPL"), (54, "2"), (38, "3"), (40, "2"), (44, "355.63"), (32, "1"), (31, "355.63"), (14, "2"), (6, "355.63"), (151, "1"),
            (6008, "265598"), (6121, "-7"), (6119, "198"), (100, "NASDAQ"), (198, "NASDAQ")]
            .into_iter().map(|(t, v)| (t, v.to_string())).collect();
        ccp.handle_exec_report(&fix::field_view(&replayed), &mut context, &shared, &None, "");
        assert!(context.order(539615664).is_none() && context.order(-7).is_none(), "no order of the session");
        assert!(shared.orders.drain_order_updates().is_empty(), "no order callback");
        assert!(shared.orders.drain_open_orders().is_empty(), "no open order");
        assert_eq!(shared.orders.drain_untracked_executions().len(), 1, "the execution is kept");
        assert_eq!(shared.orders.lowest_child_id(), -7);
        // The same report without the mark is a live one: the order of
        // another session, working.
        let mut live = replayed.clone();
        live.remove(&97);
        live.insert(17, "0000e0d5.02".to_string());
        ccp.handle_exec_report(&fix::field_view(&live), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_open_orders().len(), 1);
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
        ccp.handle_exec_report(&fix::field_view(&oca), &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.last().unwrap().parent_id, 0);

        // Bracket child of order 42 whose parent was modified once.
        let report = [
            (11u32, "43.0"), (20, "0"), (39, "0"), (150, "0"), (583, "42"),
            (6209, "CancelOnFillWBlock"), (6107, "42.1"),
        ].into_iter().collect();
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
        ccp.handle_exec_report(&fix::field_view(&report), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_updates().last().unwrap().parent_id, 15);
    }

    #[test]
    fn ord_status_new_unrouted_is_presubmitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // New status, no ExDestination, exec ref "NONE" — waiting, not yet routed.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
    }

    #[test]
    fn ord_status_new_routed_is_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // New status with the order routed to ARCA — working.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_presubmitted_then_routed_advances_to_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&waiting), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    // ibx#247: a limit replace is acked with a pending report (pending
    // cancel status on the new version) and then the Replaced status.
    // The reference reports no status
    // change across it (ib-agent#192 A1a); ibx used to report PendingCancel
    // and never move back, so the order looked cancelled.
    #[test]
    fn ord_status_replace_keeps_the_working_status() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&waiting), &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();

        let pending = exec_report_frame(&[(11, "42.1"), (39, "6"), (150, "6"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&pending), &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&replaced), &mut context, &shared, &None, "");

        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PreSubmitted);
        // Each report is reported with the unchanged status, as the
        // reference does for a modify confirm (ibx#473).
        let statuses: Vec<_> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [crate::types::OrderStatus::PreSubmitted; 2]);
    }

    // ibx#626 (the reference 10.51, paper 12/10/2026): the replace of a
    // Submitted order shows PreSubmitted while it is pending, then
    // Submitted again once replaced.
    #[test]
    fn a_pending_replace_of_a_working_order_shows_presubmitted() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted);
        shared.orders.drain_order_updates();

        let pending = exec_report_frame(&[(11, "42.1"), (39, "6"), (150, "6"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&pending), &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "5"), (150, "5"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&replaced), &mut context, &shared, &None, "");
        let statuses: Vec<_> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [OrderStatus::PreSubmitted, OrderStatus::Submitted]);
    }

    // ibx#473: a stale report is still not reported (ibx#212), and the
    // update carries the server's filled quantity (cumulative quantity).
    #[test]
    fn a_stale_report_is_not_reported_and_filled_is_the_server_count() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        let stale = exec_report_frame(&[(39, "A"), (150, "A")]);
        ccp.handle_exec_report(&fix::field_view(&stale), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_updates().is_empty(), "stale PreSubmitted after Submitted");
        let restated = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0.5"), (151, "0.5")]);
        ccp.handle_exec_report(&fix::field_view(&restated), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_updates().len(), 1);
        let info = shared.orders.get_order_info(42).expect("cached");
        assert_eq!(info.order_state.status, "Submitted");
        assert_eq!(info.order.lmt_price, 101.5);
    }

    // ibx#263: the reference reads the cash quantity the server echoes in
    // 152 into the order, which
    // openOrder shows.
    #[test]
    fn the_reported_cash_quantity_is_read_from_152() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (152, "1000.00")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(42).expect("cached").order.cash_qty, 1000.0);
    }

    #[test]
    fn ord_status_replace_of_a_routed_order_stays_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&fix::field_view(&replaced), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_pending_on_a_cancel_request_is_pending_cancel() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        let pending = exec_report_frame(&[(11, "C42"), (39, "6"), (150, "6")]);
        ccp.handle_exec_report(&fix::field_view(&pending), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PendingCancel);
    }

    // ibx#472: the captured server cancel of an IOC order (25/09/2026):
    // pending new, New, a pending cancel report, then Cancelled. The
    // pending cancel report leaves the order open, so a fill between the two reports is
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
            ccp.handle_exec_report(&fix::field_view(&exec_report_frame(pairs)), &mut context, &shared, &None, "");
        }
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
        let statuses: Vec<OrderStatus> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [OrderStatus::PreSubmitted, OrderStatus::Submitted, OrderStatus::PendingCancel]);

        let instrument = context.order(42).unwrap().instrument;
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "e1"),
            (31, "100"), (32, "1"), (14, "1"), (151, "0"), (6, "100")]);
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");
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
            ccp.handle_exec_report(&fix::field_view(&exec_report_frame(pairs)), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        context.cancel_clord.insert(42, "42.1".to_string());
        let pending = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "6"), (150, "6")]);
        ccp.handle_exec_report(&fix::field_view(&pending), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.0"));
        let done = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "4"), (150, "4")]);
        ccp.handle_exec_report(&fix::field_view(&done), &mut context, &shared, &None, "");
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
        ccp.handle_cancel_reject(&fix::field_view(&reject), &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(context.cancel_clord.get(&42).is_none());
    }

    fn pipe_frame(text: &str) -> Vec<u8> {
        text.replace('|', "\x01").into_bytes()
    }

    // ibx#466: an order of this session is held under its API order id and
    // goes out under a server id of the order id generator. Its reports
    // come under the server id: they reach the order, with that id as the
    // permId; a refused cancel lowers the order's version and asks the
    // status under the server id.
    #[test]
    fn reports_under_the_server_id_reach_the_order_of_its_api_id() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        context.bind_server_id(42, 1_000_000_007);
        let working = exec_report_frame(&[(11, "1000000007.0"), (39, "0"), (150, "0"), (100, "ARCA"), (6121, "42"), (6119, "0")]);
        ccp.handle_exec_report(&fix::field_view(&working), &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted);
        assert!(context.order(1_000_000_007).is_none());
        let updates = shared.orders.drain_order_updates();
        assert_eq!((updates[0].order_id, updates[0].perm_id), (42, 1_000_000_007));
        assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("1000000007.0"));

        context.modify_versions.insert(42, 1);
        context.cancel_clord.insert(42, "1000000007.1".to_string());
        let mut reject = std::collections::HashMap::new();
        for (t, v) in [(35u32, "9"), (11, "1000000007.1"), (41, "1000000007.0"), (434, "1"), (102, "0")] {
            reject.insert(t, v.to_string());
        }
        ccp.handle_cancel_reject(&fix::field_view(&reject), &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert_eq!(context.modify_versions.get(&42), Some(&0));
        assert!(!context.cancel_clord.contains_key(&42));
        assert!(context.status_queries.contains(&42));
    }

    // ibx#466: the API order id of each report counts for its client's
    // next valid id, as in the reference: the highest positive id per
    // client id (6119, 0 when absent); none for a report without 6121.
    #[test]
    fn reports_note_the_highest_api_order_id_of_their_client() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        for (id, client) in [("68", Some("198")), ("57", Some("198")), ("12", None), ("-3", Some("7")), ("2147483647", Some("7"))] {
            let mut report = exec_report_frame(&[(11, "1500000000.0"), (39, "4"), (150, "4"), (6121, id)]);
            if let Some(c) = client { report.insert(6119, c.to_string()); }
            ccp.handle_exec_report(&fix::field_view(&report), &mut context, &shared, &None, "");
        }
        let no_id = exec_report_frame(&[(11, "1500000001.0"), (39, "4"), (150, "4"), (6119, "5")]);
        ccp.handle_exec_report(&fix::field_view(&no_id), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.reported_order_id(198), 68);
        assert_eq!(shared.orders.reported_order_id(0), 12);
        assert_eq!(shared.orders.reported_order_id(7), 0, "negative ids and the unset value do not count");
        assert_eq!(shared.orders.reported_order_id(5), 0);
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
        let mut conn = Some(Connection::new_mem(client));
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

    // ibx#683 (the reference 10.51, paper, 13/10/2026): the auto cancel
    // date of a new-order report reaches the order but not its openOrder,
    // until the report of its modify; a status report gives it at once.
    #[test]
    fn the_auto_cancel_date_shows_from_a_status_report_or_a_modify() {
        let report = |id: i64, ver: u32, exec: &str, trans: &str, status: &str| pipe_frame(&format!(
            "8=FIX.4.1|9=000300|35=8|34=000100|43=N|52=20261013-18:28:15|11={id}.{ver}|17=140781.1791916.{ver}|150={exec}|20={trans}|39={status}|167=CS|55=AAPL|38=1|99=1000|32=0|31=0.00|14=0|151=1|6=0|54=1|37=0|1=DU1|60=20261013-18:28:15|6596=20270331-20:00:00|40=3|59=1|6008=265598|15=USD|6004=BEST|6119=198|6121=57690|10=000|"));
        let shown = |shared: &SharedState, id: i64| shared.orders.get_order_info(id).map(|i| i.order.auto_cancel_date).unwrap_or_default();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut ccp = CcpState::new();
        let instrument = context.register_instrument(265598);
        let (client, _server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        // A STP: its only report a new-order one; then its modify's.
        context.insert_order(crate::types::Order::new(328103937, instrument, Side::Buy, 1, 0, b'3', b'1', 1000 * PRICE_SCALE));
        ccp.process_ccp_message(&report(328103937, 0, "0", "0", "0"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shown(&shared, 328103937), "");
        context.modify_versions.insert(328103937, 1);
        ccp.process_ccp_message(&report(328103937, 1, "5", "0", "0"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shown(&shared, 328103937), "20270331-20:00:00");
        // A LMT whose first report is a status one.
        context.insert_order(crate::types::Order::new(328103938, instrument, Side::Buy, 1, 240 * PRICE_SCALE, b'2', b'1', 0));
        ccp.process_ccp_message(&report(328103938, 0, "A", "3", "A"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shown(&shared, 328103938), "20270331-20:00:00");
    }

    // ibx#252: a refused cancel of a working order changes neither its
    // status nor its record; the answer to the status request sets the
    // status, here back to working.
    #[test]
    fn refused_cancel_keeps_the_order_until_the_status_answer() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let routed = exec_report_frame(&[(11, "42.0"), (39, "0"), (150, "0"), (20, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "DU1");
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
        let mut conn = Some(Connection::new_mem(client));
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "DU1");
        assert_eq!(context.finished_status(42), Some(OrderStatus::Rejected));
        assert_eq!(shared.orders.drain_order_notices()[0].1, 201);
    }

    // ibx#472: the reference handles the pending replace and inactive order
    // statuses as invalid statuses: no
    // status change and no callback.
    #[test]
    fn pending_replace_and_inactive_reports_keep_the_status() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        for code in ["E", "I"] {
            let frame = exec_report_frame(&[(20, "3"), (39, code), (150, code)]);
            ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
            assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted, "39={}", code);
        }
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    // ibx#705 (the reference 10.51, paper, 16/10/2026): the contracts of a
    // portfolio message not known yet are looked up in one batch, each on
    // any exchange, in the message's order; the answer fills the cache, and
    // a later message asks nothing more.
    #[test]
    fn portfolio_contracts_are_looked_up_in_one_batch() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let up = "35=UP|6529=AR.1|6068=AAPL                  |8001=PositionList|6064=0|6008=265598|167=STK|6068=MSFT                  |8001=PositionList|6064=-10|6008=272093|167=STK|".replace('|', "");
        ccp.process_ccp_message(up.as_bytes(), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let wire = sent_frames(&mut server);
        assert_eq!(wire.matches("|35=c|").count(), 1, "{wire}");
        assert!(wire.contains("|321=2|146=2|6008=265598|6004=ANYEXCH|6008=272093|6004=ANYEXCH|"), "{wire}");
        let name = wire.split("|320=").nth(1).and_then(|r| r.split('|').next()).unwrap().to_string();
        assert!(name.starts_with("SmallBatchReqByConid"), "{name}");
        let reply = pipe_msg(&format!("35=d|43=N|320={name}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|6035=AAPL|55=MSFT|167=STK|207=BEST|6008=272093|15=USD|6035=MSFT"));
        ccp.process_ccp_message(&reply, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.get_contract(265598).is_some() && shared.reference.get_contract(272093).is_some());
        ccp.process_ccp_message(up.as_bytes(), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(!sent_frames(&mut server).contains("|35=c|"), "nothing asked again");
    }

    // ibx#742 (the reference 10.51, paper, 17/10/2026): the contract of a
    // P&L seed that no portfolio row carries is looked up too, after the
    // message's own, whichever of the seeds and the portfolio comes first;
    // a seed's contract a portfolio row carries is not asked twice.
    #[test]
    fn a_seed_contract_without_a_portfolio_row_is_looked_up() {
        let up = pipe_frame("35=UP|6529=AR.1|6068=MSFT                  |8001=PositionList|6064=-10|6008=272093|167=STK|");
        let seeds = pipe_frame("35=U|6040=143|6529=PLR.3|146=2|6008=265598|6064=0|6822=-13.3509|6099=-13.3510601|6008=272093|6064=-10|6822=0|6099=0|");
        for seeds_first in [true, false] {
            let (mut ccp, mut context, shared) = u186_test_state();
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let mut hb = HeartbeatState::new();
            let order: [&[u8]; 2] = if seeds_first { [&seeds, &up] } else { [&up, &seeds] };
            for msg in order {
                ccp.process_ccp_message(msg, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            }
            let wire = sent_frames(&mut server);
            if seeds_first {
                assert_eq!(wire.matches("|35=c|").count(), 1, "{wire}");
                assert!(wire.contains("|321=2|146=2|6008=272093|6004=ANYEXCH|6008=265598|6004=ANYEXCH|"), "{wire}");
            } else {
                assert_eq!(wire.matches("|35=c|").count(), 2, "{wire}");
                assert!(wire.contains("|321=2|146=1|6008=272093|6004=ANYEXCH|"), "{wire}");
                assert!(wire.contains("|321=2|146=1|6008=265598|6004=ANYEXCH|"), "{wire}");
            }
            ccp.process_ccp_message(&seeds, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            assert!(!sent_frames(&mut server).contains("|35=c|"), "nothing asked again");
        }
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
        assert_eq!(row(4726868).avg_cost, 117.79);
        assert_eq!((row(272093).position_fixed / crate::types::QTY_SCALE, row(272093).symbol.as_str()), (-10, "MSFT"));
        assert_eq!(row(272093).unrealized_pnl, (270.91 * PRICE_SCALE as f64) as Price);
        assert_eq!((row(756733).position_fixed / crate::types::QTY_SCALE, row(756733).symbol.as_str()), (18, "SPY"));
        // The multiplier is one part of the row key, not the whole key.
        assert_eq!(row(4726868).multiplier, "1");
        assert_eq!(context.position_fixed(msft) / crate::types::QTY_SCALE, -10, "a registered instrument's position follows its row");
    }

    // ibx#238 / ib-agent#172: in the UP portfolio snapshot the average cost is
    // its own field and another field is the market price. The handler
    // previously read the market price field as the average cost. Verify the
    // mapping and that all marks are stored.
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
        handle_position_update(&fix::field_view(&m), &mut context, &shared, &None);

        let pi = shared.portfolio.position_info(756733).expect("position stored");
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 10);
        assert_eq!(pi.avg_cost, 100.50);
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
            con_id: 1, position_fixed: (10) as i64 * crate::types::QTY_SCALE, avg_cost: 100.0, ..Default::default()
        });
        shared.portfolio.set_position_marks(1, 110 * PRICE_SCALE, 1100 * PRICE_SCALE, 100 * PRICE_SCALE, 5 * PRICE_SCALE);
        // Lean feed updates position + avg_cost only.
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 1, position_fixed: (12) as i64 * crate::types::QTY_SCALE, avg_cost: 101.0, ..Default::default()
        });
        let pi = shared.portfolio.position_info(1).unwrap();
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 12);
        assert_eq!(pi.avg_cost, 101.0);
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

    // ── ibx#485: a caller's contract details request has no deadline ──

    // As in the reference (only an order's lookup gives up),
    // a caller's request waits for its answer: no error 200 of ibx's own,
    // no end. A negative request id is a caller's (ibx#285).
    #[test]
    fn sweep_keeps_a_callers_request_past_any_deadline() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        // Any moment past is past the deadline; an hour back panics in the
        // first hour after the machine starts.
        let past = Instant::now() - std::time::Duration::from_secs(1);
        ccp.pending_secdef.push((7, true, past));
        ccp.pending_secdef.push((-7, true, past));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert_eq!(ccp.pending_secdef.len(), 2, "both still wait");
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
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

    // ibx#485: a caller's fan-out waits for every per-exchange reply, as
    // the reference's contract details end does; an internal one is
    // dropped at its deadline.
    #[test]
    fn sweep_keeps_a_callers_fanout_and_drops_an_internal_one() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let fanout = |api_req_id| PendingFanout {
            total: 27,
            ..PendingFanout::new(api_req_id, vec![("ibxfan-9-26".to_string(), 1, "IEX".to_string())], Vec::new(),
                Instant::now() - std::time::Duration::from_secs(1))
        };
        ccp.pending_fanout.push(fanout(9));
        ccp.pending_fanout.push(fanout(0xF000_0003));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert_eq!(ccp.pending_fanout.iter().map(|f| f.api_req_id).collect::<Vec<_>>(), vec![9]);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
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
            currency: "USD".into(), filters, continuous: false,
        }
    }

    fn lookup_frame(lookup: &SymbolLookup) -> String {
        let strike = lookup.filters.strike_text();
        frame_text(&secdef_by_symbol_fields(7, lookup, &strike))
    }

    fn frame_text(fields: &[(u32, String)]) -> String {
        fields.iter().map(|(t, v)| format!("{}={}", t, v)).collect::<Vec<_>>().join("|")
    }

    // ibx#666, the reference's lookup of an option order (paper 14/10/2026):
    // the source, then the option's terms with its strike, as given.
    #[test]
    fn an_option_order_is_looked_up_with_its_strike() {
        let filters = crate::types::SecDefFilters {
            last_trade_date_or_contract_month: "20261218".into(), strike: 340.0, right: "C".into(),
            multiplier: "100".into(), ..Default::default()
        };
        assert_eq!(frame_text(&order_lookup_fields(13, &symbol_lookup("AAPL", "OPT", "SMART", filters))),
            "35=c|320=FixSecDefReqBySymbol13|321=2|6088=Socket|55=AAPL|167=OPT|541=20261218|201=1|202=340|231=100|100=BEST|15=USD");
        assert_eq!(frame_text(&order_lookup_fields(13, &symbol_lookup("AAPL", "STK", "SMART", Default::default()))),
            "35=c|320=FixSecDefReqBySymbol13|321=2|6088=Socket|55=AAPL|167=CS|100=BEST|15=USD");
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
        assert_eq!(secdef_request_number("getECsForConidExchangePairsReqByConid2"), None);
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

    fn socket_pair() -> (crate::protocol::connection::MemTransport, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (client, server)
    }

    /// Every message written to `server`, without the framing, sequence
    /// and time fields.
    fn ccp_messages_sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
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
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let mut f = option_filters("", "", "");
        f.strike = 342.8;
        ccp.send_secdef_request_by_symbol(9, "AAPL", "OPT", "SMART", "USD", &f, &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server),
            ["35=c|320=FixSecDefReqBySymbol1|321=2|55=AAPL|167=OPT|541=20261005|201=1|202=342.8|100=BEST|15=USD"]);

        ccp.process_ccp_message(&empty_secdef_reply("9"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ccp_messages_sent(&mut server),
            ["35=c|320=FixSecDefReqBySymbol2|321=2|55=AAPL|167=OPT|541=20261005|201=1|202=3.428|100=BEST|15=USD"]);
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

    // ibx#436: a derivative record without an industry looks up the
    // company of its underlying; its row goes out without the industry,
    // the later rows get it (captured 28/09/2026: the first AAPL option row
    // has no industry, the next ones have Technology).
    // ibx#435, ibx#436: a lookup reply asks, for each record and each of
    // its valid exchanges but BEST, the market rule it does not know yet:
    // one request per (conId, exchange), named as the reference names it
    // (captured 28/09/2026: 20 requests for one AAPL option, AMEX to MX2;
    // 02/10/2026: one, QBALGO, for an ES future whose CME rule came with
    // the reply). The rows wait for every answer; a second lookup of the
    // same contracts asks nothing.
    #[test]
    fn fan_out_asks_each_unknown_exchange_rule_of_each_record() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let reply = |rid: &str| pipe_msg(&format!(
            "35=d|320={rid}|323=4|55=AAPL|167=OPT|207=BEST|6008=1|6031=32|55=AAPL|167=OPT|207=BEST|6008=2|6031=32|\
             146=0|6344=2|6008=1|6046=BEST,AMEX,CBOE,|6008=2|6046=BEST,AMEX,CBOE,"));
        ccp.send_secdef_request_by_symbol(41, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&reply("41"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        let asked: Vec<(String, String, String)> = sent.iter().map(|m| {
            let field = |tag: &str| m.split('|').find_map(|f| f.strip_prefix(tag)).unwrap_or("").to_string();
            (field("320="), field("6008="), field("6004="))
        }).collect();
        assert_eq!(asked.len(), 4, "{sent:?}");
        assert!(asked.iter().all(|(rid, _, _)| rid.starts_with("getECsForConidExchangePairsReqByConid")), "{asked:?}");
        let pairs: Vec<(&str, &str)> = asked.iter().map(|(_, c, e)| (c.as_str(), e.as_str())).collect();
        assert_eq!(pairs, [("1", "AMEX"), ("1", "CBOE"), ("2", "AMEX"), ("2", "CBOE")]);
        assert!(sent.iter().all(|m| m.contains("|321=2|146=1|")), "{sent:?}");
        assert!(shared.reference.drain_contract_details().is_empty(), "the rows wait for the answers");
        for (i, (rid, con_id, exch)) in asked.iter().enumerate() {
            let answer = pipe_msg(&format!(
                "35=d|320={rid}|323=4|55=AAPL|167=OPT|207={exch}|6008={con_id}|6031={}|146=0|6344=0", 100 + i));
            ccp.process_ccp_message(&answer, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        }
        let rows = shared.reference.drain_contract_details();
        let ids: Vec<(i64, String)> = rows.iter().map(|(_, d)| (d.con_id, d.market_rule_ids.clone())).collect();
        assert_eq!(ids, [(1, "32,100,101".to_string()), (2, "32,102,103".to_string())]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![41]);
        // The same contracts again: every rule is known, no request.
        ccp.send_secdef_request_by_symbol(42, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&reply("42"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).is_empty());
        assert_eq!(shared.reference.drain_contract_details().len(), 2);
    }

    // ibx#689 (the reference 10.51's recording of a whole option chain,
    // paper 14/10/2026: 2229 rows in the lookup's order, each after its own
    // answers): the rows leave in the lookup's order as their answers come,
    // a row waits for the rows before it, and the end comes with the last.
    #[test]
    fn fan_out_rows_leave_in_order_as_their_answers_come() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let reply = pipe_msg("35=d|320=43|323=4|55=AAPL|167=OPT|207=BEST|6008=1|6031=32|55=AAPL|167=OPT|207=BEST|6008=2|6031=32|55=AAPL|167=OPT|207=BEST|6008=3|6031=32|146=0|6344=3|6008=1|6046=BEST,AMEX,|6008=2|6046=BEST,AMEX,|6008=3|6046=BEST,AMEX,");
        ccp.send_secdef_request_by_symbol(43, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&reply, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let asked: Vec<(String, String)> = ccp_messages_sent(&mut server).iter().map(|m| {
            let field = |tag: &str| m.split('|').find_map(|f| f.strip_prefix(tag)).unwrap_or("").to_string();
            (field("320="), field("6008="))
        }).collect();
        assert_eq!(asked.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(), ["1", "2", "3"]);
        let mut answer = |i: usize| {
            let (rid, con_id) = &asked[i];
            let m = pipe_msg(&format!("35=d|320={rid}|323=4|55=AAPL|167=OPT|207=AMEX|6008={con_id}|6031=100|146=0|6344=0"));
            ccp.process_ccp_message(&m, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        };
        let rows = |shared: &SharedState| shared.reference.drain_contract_details().iter().map(|(_, d)| d.con_id).collect::<Vec<_>>();
        // The second contract's answer: it waits for the first.
        answer(1);
        assert!(rows(&shared).is_empty());
        // The first's: the first two leave, in order, with no end yet.
        answer(0);
        assert_eq!(rows(&shared), [1, 2]);
        assert!(shared.reference.drain_contract_details_end().is_empty(), "the third row still waits");
        answer(2);
        assert_eq!(rows(&shared), [3]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![43]);
    }

    // ibx#689: on a paced session the fan-out's requests leave at the
    // reference's pace (about 2,670 a second), and no row leaves before the
    // last of them is sent, as the reference.
    #[test]
    fn paced_fan_out_requests_leave_at_the_reference_pace() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.fanout_paced = true;
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let reply = pipe_msg("35=d|320=44|323=4|55=AAPL|167=OPT|207=BEST|6008=1|6031=32|55=AAPL|167=OPT|207=BEST|6008=2|6031=32|146=0|6344=2|6008=1|6046=BEST,AMEX,CBOE,ISE,|6008=2|6046=BEST,AMEX,");
        ccp.send_secdef_request_by_symbol(44, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&reply, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).is_empty(), "nothing before its turn");
        let t0 = Instant::now();
        ccp.pump_fanout(t0, &mut conn, &mut hb);
        let first = ccp_messages_sent(&mut server);
        assert_eq!(first.len(), 1, "the first at once");
        ccp.pump_fanout(t0 + FANOUT_SEND_GAP, &mut conn, &mut hb);
        let second = ccp_messages_sent(&mut server);
        assert_eq!(second.len(), 1, "the next one gap later");
        let field = |m: &str, tag: &str| m.split('|').find_map(|f| f.strip_prefix(tag)).unwrap_or("").to_string();
        // Answer the first two: contract 1 still has requests to send, so
        // no row leaves yet.
        for m in first.iter().chain(&second) {
            let a = pipe_msg(&format!("35=d|320={}|323=4|55=AAPL|167=OPT|207={}|6008={}|6031=100|146=0|6344=0",
                field(m, "320="), field(m, "6004="), field(m, "6008=")));
            ccp.process_ccp_message(&a, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        }
        assert!(shared.reference.drain_contract_details().is_empty());
        ccp.pump_fanout(t0 + FANOUT_SEND_GAP * 10, &mut conn, &mut hb);
        let rest = ccp_messages_sent(&mut server);
        assert_eq!(rest.len(), 2, "the two left");
        for m in &rest {
            let a = pipe_msg(&format!("35=d|320={}|323=4|55=AAPL|167=OPT|207={}|6008={}|6031=100|146=0|6344=0",
                field(m, "320="), field(m, "6004="), field(m, "6008=")));
            ccp.process_ccp_message(&a, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        }
        assert_eq!(shared.reference.drain_contract_details().len(), 2);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![44]);
    }

    #[test]
    fn a_derivative_row_gets_the_industry_of_its_looked_up_company() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let option = |rid: &str| pipe_msg(&format!(
            "35=d|320={rid}|323=4|55=AAPL|167=OPT|207=BEST|6008=926735346|6031=32|146=0|6344=1|6008=926735346|6346=265598|306=APPLE INC"));

        ccp.send_secdef_request_by_symbol(31, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option("31"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        // The row waits for the company (ibx#733).
        assert!(shared.reference.drain_contract_details().is_empty());
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("|146=1|6008=265598|6004=ANYEXCH"), "{}", sent[0]);
        let rid = sent[0].split('|').find_map(|f| f.strip_prefix("320=")).unwrap().to_string();
        assert!(rid.starts_with("UnderlyingECNoDupsNDReqByConid"), "{rid}");

        // The company reply is not a row.
        let mut company = pipe_msg(&format!(
            "35=d|320={rid}|323=4|55=AAPL|167=STK|207=BEST|6008=265598|146=0|6344=1|6008=265598|6623=0|6622=1|6623=0|6624=Technology"));
        let at = company.windows(15).position(|w| w == b"6624=Technology").unwrap() + 15;
        company.splice(at..at, b"|Computers|Computers".iter().copied());
        ccp.process_ccp_message(&company, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.drain_contract_details().is_empty());
        // The underlying was not known: its preferred definition, and the
        // row waits for it (ibx#733).
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "{sent:?}");
        let pid = sent[0].split('|').find_map(|f| f.strip_prefix("320=")).unwrap().to_string();
        assert!(pid.starts_with("PreferredReqByConid"), "{pid}");
        let preferred = pipe_msg(&format!("35=d|320={pid}|323=4|55=AAPL|167=STK|207=BEST|6008=265598|146=0|6344=1|6008=265598"));
        ccp.process_ccp_message(&preferred, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.industry, "", "the first row has no industry");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![31]);

        ccp.send_secdef_request_by_symbol(32, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option("32"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows = shared.reference.drain_contract_details();
        let d = &rows[0].1;
        assert_eq!((d.industry.as_str(), d.category.as_str(), d.subcategory.as_str()), ("Technology", "Computers", "Computers"));
        assert!(ccp_messages_sent(&mut server).is_empty(), "no second company lookup");
    }

    /// An option's answer with two valid exchanges and its underlying, and
    /// the answers of the underlying's lookups (ibx#733 tests).
    fn option_with_underlying(rid: &str) -> Vec<u8> {
        let record = "146=0|6344=1|6008=692807782|6046=BEST,AMEX,CBOE,|6346=265598";
        pipe_msg(&format!("35=d|320={rid}|323=4|55=AAPL|167=OPT|207=BEST|6008=692807782|6031=32|{record}"))
    }

    fn underlying_answer(rid: &str) -> Vec<u8> {
        pipe_msg(&format!("35=d|320={rid}|323=4|55=AAPL|167=STK|207=BEST|6008=265598|146=0|6344=1|6008=265598|6624=Technology"))
    }

    fn sent_name(m: &str) -> String {
        m.split('|').find_map(|f| f.strip_prefix("320=")).unwrap_or("").to_string()
    }

    // ibx#733 (observed on paper, recordings of 11-15/10/2026: 11 contract
    // details requests of 11 that asked a company; 3 of 3 whose underlying
    // was not known asked its preferred definition 0 to 6 ms after the
    // company's answer; the per-exchange lookups after the last answer):
    // the company alone, then the underlying's preferred definition alone,
    // then the per-exchange lookups, then the row.
    #[test]
    fn an_unknown_underlying_has_its_preferred_definition_asked_after_its_company() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.send_secdef_request_by_symbol(51, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option_with_underlying("51"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "the company alone: {sent:?}");
        assert!(sent_name(&sent[0]).starts_with("UnderlyingECNoDupsNDReqByConid"), "{sent:?}");
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[0])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "the preferred definition alone: {sent:?}");
        let pid = sent_name(&sent[0]);
        assert_eq!(sent[0], format!("35=c|320={pid}|321=2|146=1|6008=265598|6004=ANYEXCH"));
        assert!(pid.starts_with("PreferredReqByConid"), "{pid}");
        assert!(shared.reference.drain_contract_details().is_empty(), "the row waits");
        assert!(!context.agg_groups.contains_key(&265598));
        ccp.process_ccp_message(&underlying_answer(&pid), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(context.agg_groups.contains_key(&265598), "known from its answer");
        assert!(ccp.pending_preferred.is_empty() && ccp.held_details.is_empty());
        let sent = ccp_messages_sent(&mut server);
        let asked: Vec<(String, String)> = sent.iter().map(|m| (sent_name(m), m.split('|').find_map(|f| f.strip_prefix("6004=")).unwrap_or("").to_string())).collect();
        assert_eq!(asked.len(), 2, "{sent:?}");
        assert!(asked.iter().all(|(n, _)| n.starts_with("getECsForConidExchangePairsReqByConid")), "{asked:?}");
        assert_eq!(asked.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(), ["AMEX", "CBOE"]);
        for (rid, exch) in &asked {
            let answer = pipe_msg(&format!("35=d|320={rid}|323=4|55=AAPL|167=OPT|207={exch}|6008=692807782|6031=100|146=0|6344=0"));
            ccp.process_ccp_message(&answer, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        }
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.industry, "", "the row as read before the company");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![51]);
    }

    // ibx#733 (observed on paper, recordings of 12-15/10/2026: 8 contract
    // details requests of 8 whose underlying was known asked no preferred
    // definition; their per-exchange lookups or trading hours came 1 to 35
    // ms after the company's answer): with the underlying known, the
    // per-exchange lookups follow the company's answer.
    #[test]
    fn a_known_underlying_asks_no_preferred_definition_after_its_company() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        context.agg_groups.insert(265598, 1);
        ccp.send_secdef_request_by_symbol(52, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option_with_underlying("52"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "the company alone: {sent:?}");
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[0])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent.iter().all(|m| sent_name(m).starts_with("getECsForConidExchangePairsReqByConid")), "{sent:?}");
        assert!(ccp.preferred_after_company.is_empty() && ccp.held_details.is_empty());
    }

    // ibx#733 (observed on paper, 14/10/2026: two contract details requests
    // of the same option 12 ms apart; one company lookup, one preferred
    // definition, both rows after them): every request on the underlying
    // waits for the same answers and they leave in their order.
    #[test]
    fn requests_on_one_underlying_wait_for_one_company_and_one_preferred() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let option = |rid: &str| pipe_msg(&format!(
            "35=d|320={rid}|323=4|55=AAPL|167=OPT|207=BEST|6008=692808239|6031=32|146=0|6344=1|6008=692808239|6346=265598"));
        for req_id in [61, 62] {
            ccp.send_secdef_request_by_symbol(req_id, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        }
        let _ = ccp_messages_sent(&mut server);
        for req_id in ["61", "62"] {
            ccp.process_ccp_message(&option(req_id), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        }
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "one company lookup: {sent:?}");
        assert_eq!(ccp.held_details.len(), 2);
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[0])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "one preferred definition: {sent:?}");
        assert!(shared.reference.drain_contract_details().is_empty());
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[0])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows: Vec<ReqId> = shared.reference.drain_contract_details().iter().map(|(id, _)| *id).collect();
        assert_eq!(rows, [61, 62]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![61, 62]);
        assert!(ccp_messages_sent(&mut server).is_empty());
    }

    // ibx#589 (paper 11/10/2026, market closed, first request of the
    // session on both sides): the definition of the option of a
    // calculation is asked by conId on the contract's exchange, as the
    // reference asks it. Asked with no exchange, the server answered with
    // no definition and the calculation ended with error 200.
    #[test]
    fn the_option_of_a_calculation_is_asked_on_its_exchange() {
        let (mut ccp, _context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let kind = crate::control::optcalc::CalcKind::Price { volatility: 0.3 };
        ccp.optcalc.start(9100, 692807782, "SMART", kind, 250.0, &mut conn, &mut hb, &shared);
        ccp.optcalc.start(9101, 692807783, "CBOE", kind, 250.0, &mut conn, &mut hb, &shared);
        // Known or asked already: nothing more is sent.
        ccp.optcalc.start(9102, 692807782, "SMART", kind, 250.0, &mut conn, &mut hb, &shared);
        let first = super::super::optcalc::OPTCALC_LOOKUP_FIRST_ID;
        // Named in the session series of the by-conId lookups (ibx#698).
        assert_eq!(ccp_messages_sent(&mut server), [
            "35=c|320=SecDefReqMsgReqByConid1|321=2|6088=Socket|146=1|6008=692807782|6004=BEST",
            "35=c|320=SecDefReqMsgReqByConid2|321=2|6088=Socket|146=1|6008=692807783|6004=CBOE",
        ]);
        // The answer is read by the name of the request.
        let reply = pipe_msg(&format!(
            "35=d|43=N|320=SecDefReqMsgReqByConid{first}|322=*|323=4|55=AAPL|167=OPT|207=BEST|6008=692807782|15=USD|58=AAPL|\
             6035=AAPL  261218C00250000|6058=AAPL|541=20261218|200=202612|201=1|202=250|231=100|146=1|6038=Y|6019=1|\
             6031=32|6026=1|6023=0|6027=0.01|6030=1|6344=1|6008=692807782|6346=265598|310=STK"));
        assert!(ccp.optcalc.secdef_reply(&format!("SecDefReqMsgReqByConid{first}"), &reply, &mut conn, &mut hb, &shared));
        assert!(shared.orders.drain_order_errors().iter().all(|e| e.1 != 200), "the option is known: no error 200");
    }

    // ibx#699: each kind of lookup ibx sends on its own has a range of
    // request numbers no other kind uses, above the caller's ids and below
    // ibx's internal lookups; an answer is matched on its number alone.
    #[test]
    fn the_lookup_ranges_do_not_overlap() {
        use crate::engine::hot_loop::{farm, optcalc, optparams, order_builder};
        let mut ranges = [
            ("option calculation", optcalc::OPTCALC_LOOKUP_FIRST_ID, optcalc::OPTCALC_LOOKUP_IDS),
            ("option parameters", optparams::OPTPARAMS_LOOKUP_FIRST_ID, optparams::OPTPARAMS_LOOKUP_IDS),
            ("order", order_builder::ORDER_LOOKUP_FIRST_ID, order_builder::ORDER_LOOKUP_IDS),
            ("historical data", HIST_LOOKUP_FIRST_ID, HIST_LOOKUP_IDS),
            ("market data", farm::MD_LOOKUP_FIRST_ID, farm::MD_LOOKUP_IDS),
            ("order's option model", farm::MODEL_LOOKUP_FIRST_ID, farm::MODEL_LOOKUP_IDS),
            ("definition a request or a model waits for", farm::DEFINITION_LOOKUP_FIRST_ID, farm::DEFINITION_LOOKUP_IDS),
        ];
        ranges.sort_by_key(|r| r.1);
        // A caller's request id is a positive i32.
        assert!(ranges[0].1 > i32::MAX as u32, "{} meets the caller's ids", ranges[0].0);
        for pair in ranges.windows(2) {
            assert!(u64::from(pair[0].1) + u64::from(pair[0].2) <= u64::from(pair[1].1), "{} meets {}", pair[0].0, pair[1].0);
        }
        let last = ranges[ranges.len() - 1];
        assert!(u64::from(last.1) + u64::from(last.2) <= 0xF000_0000, "{} meets the internal lookups", last.0);
        assert!(!internal_lookup(ReqId::from(last.1 + last.2 - 1)));
    }

    // ibx#699: an order placed by symbol and an option calculation, both
    // lookups waiting at once: each answer reaches its own request, in
    // either order (the order's matcher is asked first).
    #[test]
    fn an_order_lookup_and_an_option_calculation_lookup_get_their_own_answers() {
        use crate::engine::hot_loop::order_builder::ORDER_LOOKUP_FIRST_ID;
        use crate::types::{OrderRequest, Side, PRICE_SCALE};
        let (mut ccp, mut context, shared) = u186_test_state();
        let mut hb = HeartbeatState::new();
        // The order: a slot without a conId, its lookup the first of its range.
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.rth_parked.push(OrderRequest::SubmitLimit { order_id: 7, instrument: slot, side: Side::Buy, qty: 1, price: 100 * PRICE_SCALE });
        context.order_lookups.push((ORDER_LOOKUP_FIRST_ID, slot));
        // The calculation: its lookup the first of its own range.
        let kind = crate::control::optcalc::CalcKind::Price { volatility: 0.3 };
        ccp.optcalc.start(9100, 692807782, "SMART", kind, 250.0, &mut None, &mut hb, &shared);
        let first = super::super::optcalc::OPTCALC_LOOKUP_FIRST_ID;
        // The option's answer first: the order does not take it.
        let option = pipe_msg(&format!(
            "35=d|43=N|320=SecDefReqMsgReqByConid{first}|322=*|323=4|55=AAPL|167=OPT|207=BEST|6008=692807782|15=USD|58=AAPL|6035=AAPL  261218C00250000|6058=AAPL|541=20261218|200=202612|201=1|202=250|231=100|146=1|6038=Y|6019=1|6031=32|6026=1|6023=0|6027=0.01|6030=1|6344=1|6008=692807782|6346=265598|310=STK"));
        ccp.process_ccp_message(&option, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp.optcalc.knows_option(692807782), "the calculation has its option");
        assert_eq!(context.market.con_id(slot), Some(0), "the order's slot has no conId yet");
        assert_eq!(context.order_lookups.len(), 1, "the order still waits");
        // Then the order's answer: the stock.
        let stock = pipe_msg(&format!(
            "35=d|43=N|320=FixSecDefReqBySymbol{ORDER_LOOKUP_FIRST_ID}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|146=1|6344=1|6008=265598"));
        ccp.process_ccp_message(&stock, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.market.con_id(slot), Some(265598), "the order is on the stock");
        assert!(context.order_lookups.is_empty());
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ibx#666 (the reference 10.51, paper, pre-market 15/10/2026, a SAP
    // warrant by conId): the openOrder of an order given by its conId shows
    // the contract of its definition, a derivative's terms included.
    #[test]
    fn a_definition_gives_a_derivative_contract_its_terms() {
        let (mut ccp, _context, shared) = u186_test_state();
        let msg = pipe_msg("35=d|43=N|320=SecDefReqMsgReqByConid7|322=*|323=4|55=SAP|167=WAR|207=FWB|6008=566386640|15=EUR|58=SAP|6035=HE0DGD|6058=SAP|541=20261215|200=202612|201=1|202=85|231=0.1|146=1|6344=1|6008=566386640");
        let defs = crate::control::contracts::parse_secdef_records(&msg).unwrap();
        ccp.cache_definition(&defs[0], &shared);
        let c = shared.reference.get_contract(566386640).unwrap();
        assert_eq!((c.symbol.as_str(), c.sec_type.as_str(), c.last_trade_date_or_contract_month.as_str(), c.strike, c.right.as_str(),
            c.multiplier.as_str(), c.local_symbol.as_str(), c.currency.as_str()),
            ("SAP", "WAR", "20261215", 85.0, "C", "0.1", "HE0DGD", "EUR"));
    }

    // ibx#666 (the reference 10.51, paper, 17/10/2026, market closed,
    // `.tmp/666/t5/case5.py` cases I and L): the openOrder contract of a
    // structured product given by its conId shows its perpetual date,
    // strike, right and multiplier; a treasury bill's its maturity and no
    // multiplier.
    #[test]
    fn a_definition_gives_a_product_and_a_bill_their_terms() {
        let (mut ccp, _context, shared) = u186_test_state();
        let msg = pipe_msg("35=d|43=N|320=SecDefReqMsgReqByConid2|322=*|323=4|55=DAX|167=IOPT|207=SBF|6008=411098772|15=EUR|58=DAX|6035=24W1Z|6058=DAX|541=NOEXP|200=NOEXP|201=1|202=11088.4204|231=0.01|146=1|6344=1|6008=411098772");
        let defs = crate::control::contracts::parse_secdef_records(&msg).unwrap();
        ccp.cache_definition(&defs[0], &shared);
        let c = shared.reference.get_contract(411098772).unwrap();
        assert_eq!((c.symbol.as_str(), c.sec_type.as_str(), c.last_trade_date_or_contract_month.as_str(), c.strike, c.right.as_str(),
            c.multiplier.as_str(), c.local_symbol.as_str(), c.trading_class.as_str()),
            ("DAX", "IOPT", "NOEXP", 11088.4204, "C", "0.01", "24W1Z", "DAX"));
        let msg = pipe_msg("35=d|43=N|320=SecDefReqMsgReqByConid5|322=*|323=4|55=US-T|167=BILL|207=BEST|6008=819166829|15=USD|58=TBILL|6035=IBCID819166829|6058=TBILL|146=1|6344=1|6008=819166829|541=20261001|6504=1000|6503=GOVT");
        let defs = crate::control::contracts::parse_secdef_records(&msg).unwrap();
        ccp.cache_definition(&defs[0], &shared);
        let c = shared.reference.get_contract(819166829).unwrap();
        assert_eq!((c.symbol.as_str(), c.sec_type.as_str(), c.last_trade_date_or_contract_month.as_str(), c.right.as_str(),
            c.multiplier.as_str(), c.local_symbol.as_str(), c.trading_class.as_str()),
            ("US-T", "BILL", "20261001", "", "", "IBCID819166829", "TBILL"));
    }

    // ibx#700: what is kept per order goes when the order is gone for good
    // (out of the ring of ended orders), not before: an ended parent's
    // ClOrdID is still read for its children's reports.
    #[test]
    fn the_records_of_an_order_go_with_it() {
        use crate::engine::context::FINISHED_ORDERS_MAX;
        let (mut ccp, mut context, shared) = u186_test_state();
        let total = FINISHED_ORDERS_MAX as i64 + 10;
        for id in 1..=total {
            context.modify_versions.insert(id, 1);
            context.last_clord.insert(id, format!("{id}.1"));
            ccp.held_cancel_dates.insert(id, "20261015-16:00:00".into());
            ccp.order_messages_sent.insert(id, vec!["text".into()]);
            context.finish_unsent(id, crate::types::OrderStatus::Cancelled);
        }
        assert_eq!(context.last_clord.len(), FINISHED_ORDERS_MAX);
        assert_eq!(context.modify_versions.len(), FINISHED_ORDERS_MAX);
        assert!(!context.last_clord.contains_key(&10) && context.last_clord.contains_key(&11), "the oldest go first");
        assert_eq!(context.finished_status(11), Some(crate::types::OrderStatus::Cancelled));
        // The order connection drops its own records at its next report.
        let report = pipe_msg("35=8|11=99999999.0|17=x|150=0|39=0|55=AAPL|54=1|38=1|151=1|14=0");
        let parsed = crate::protocol::fix::fix_parse(&report);
        ccp.handle_exec_report(&fix::field_view(&parsed), &mut context, &shared, &None, "DU1");
        assert!(context.purged_orders.is_empty());
        assert_eq!((ccp.held_cancel_dates.len(), ccp.order_messages_sent.len()), (FINISHED_ORDERS_MAX, FINISHED_ORDERS_MAX));
        assert!(!ccp.held_cancel_dates.contains_key(&1) && ccp.order_messages_sent.contains_key(&total));
    }

    #[test]
    fn a_quantity_of_an_order_message_is_grouped_by_thousands() {
        for (given, shown) in [("1", "1"), ("100", "100"), ("999", "999"), ("1000", "1,000"), ("12345", "12,345"),
            ("5000000", "5,000,000"), ("1234.5", "1,234.5"), ("0.5", "0.5"), ("", "")] {
            assert_eq!(grouped_quantity(given), shown);
        }
    }

    #[test]
    fn records_sharing_a_join_key_ask_their_schedule_once() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.send_secdef_request_by_symbol(22, "MNQ", "FUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);

        ccp.process_ccp_message(&five_future_records("22", "CME/FUT"), &mut conn, &mut context, &shared, &None,
            &mut hb, "DU1");
        // No company for futures: the class once, then the underlying's
        // preferred definition, and the rest waits for it (ibx#735).
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0], "35=U|6040=65|6410=0|6411=CME/FUT/GLOBEX");
        assert!(sent[1].contains("320=PreferredReqByConid") && sent[1].contains("6008=362687422")
            && sent[1].contains("6004=ANYEXCH"), "{sent:?}");
        let name = |m: &str| m.split('|').find_map(|f| f.strip_prefix("320=")).unwrap_or("").to_string();
        let preferred = pipe_msg(&format!("35=d|320={}|323=4|55=NQ|167=IND|207=CME|6008=362687422|146=0|6344=1|6008=362687422",
            name(&sent[1])));
        ccp.process_ccp_message(&preferred, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        let schedules: Vec<&String> = sent.iter().filter(|m| m.contains("6040=106")).collect();
        assert_eq!(schedules.len(), 1, "one schedule request for the shared key: {sent:?}");
        assert!(schedules[0].contains("6256=CME/FUT"), "{}", schedules[0]);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(shared.reference.drain_contract_details().is_empty(), "rows wait for the schedule");
        assert!(shared.reference.drain_contract_details_end().is_empty());

        let schedule = pipe_msg("35=U|6040=107|6256=CME/FUT");
        ccp.process_ccp_message(&schedule, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shared.reference.drain_contract_details().len(), 5);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![22]);
        assert!(ccp.pending_schedule_pair.is_empty());
    }

    /// The class named by a permission question, or None for another
    /// message (ibx#734 tests).
    fn asked_class(m: &str) -> Option<&str> {
        if !m.starts_with("35=U|6040=65|") {
            return None;
        }
        m.split('|').find_map(|f| f.strip_prefix("6411="))
    }

    // ibx#734 (observed on paper, recordings of 11-15/10/2026: option
    // contract details by description and by conId, the class 0 to 6 ms
    // after the company's answer and before the preferred definition or the
    // per-exchange lookups; each per-exchange answer's class within 2 ms;
    // none asked twice in a session): the option's class once the company
    // answered, then the preferred definition; each exchange's class with
    // its answer; nothing again for a second request.
    #[test]
    fn option_details_ask_their_class_after_the_company_and_each_exchange_class_at_once() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let option = |rid: &str| pipe_msg(&format!("35=d|320={rid}|323=4|55=AAPL|167=OPT|207=BEST|6008=692807782|6411=2/OPT/R|\
            6031=32|146=0|6344=1|6008=692807782|6046=BEST,AMEX,CBOE,|6346=265598"));
        ccp.send_secdef_request_by_symbol(71, "AAPL", "OPT", "SMART", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option("71"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 1, "the company alone: {sent:?}");
        assert!(sent_name(&sent[0]).starts_with("UnderlyingECNoDupsNDReqByConid"), "{sent:?}");
        assert_eq!(context.classes_after_company, [(265598, "2/OPT/R".to_string())]);
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[0])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(asked_class(&sent[0]), Some("2/OPT/R"), "the class first: {sent:?}");
        assert!(sent_name(&sent[1]).starts_with("PreferredReqByConid"), "{sent:?}");
        assert!(context.classes_after_company.is_empty());
        ccp.process_ccp_message(&underlying_answer(&sent_name(&sent[1])), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let lookups = ccp_messages_sent(&mut server);
        assert_eq!(lookups.len(), 2, "{lookups:?}");
        for (m, exch) in lookups.iter().zip(["AMEX", "CBOE"]) {
            let answer = pipe_msg(&format!("35=d|320={}|323=4|55=AAPL|167=OPT|207={exch}|6008=692807782|6411={exch}/OPT/R|\
                6031=100|146=0|6344=0", sent_name(m)));
            ccp.process_ccp_message(&answer, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            let sent = ccp_messages_sent(&mut server);
            assert_eq!(sent.len(), 1, "{sent:?}");
            assert_eq!(asked_class(&sent[0]), Some(format!("{exch}/OPT/R").as_str()));
        }
        assert_eq!(shared.reference.drain_contract_details().len(), 1);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![71]);

        // A second request of the session: no class asked again.
        ccp.send_contract_details_by_con_id(72, 692807782, "SMART", &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&option("72"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).iter().all(|m| asked_class(m).is_none()));
        assert_eq!(context.permissions_asked.len(), 3);
    }

    // ibx#734 (observed on paper, 11/10/2026: MSFT bonds by description,
    // 54 rows naming 3 classes, each asked once in the order the rows name
    // them; 16/10/2026: BTC on PAXOS, its class 3 ms after the answer; 7
    // market data requests on SPX: an index's class never asked): every
    // class of the rows once, at once, an index's never.
    #[test]
    fn every_class_the_rows_name_is_asked_once_in_their_order_but_an_index() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let row = |con_id: u32, key: &str| format!("55=MSFT|167=BOND|207=BEST|6008={con_id}|6411={key}|15=USD|");
        let answer = |rid: &str| pipe_msg(&format!("35=d|320={rid}|323=4|{}{}{}{}146=0",
            row(60611156, "7/BOND/USA_DEBT"), row(127248523, "7/BOND/EU_DEBT"), row(139592938, "7/BOND/USA_DEBT"),
            row(421799264, "CORPACT/BOND/USA_DEBT")));
        ccp.send_secdef_request_by_symbol(81, "MSFT", "BOND", "", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&answer("81"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let classes: Vec<String> = ccp_messages_sent(&mut server).iter().filter_map(|m| asked_class(m).map(String::from)).collect();
        assert_eq!(classes, ["7/BOND/USA_DEBT", "7/BOND/EU_DEBT", "CORPACT/BOND/USA_DEBT"]);
        ccp.send_secdef_request_by_symbol(82, "MSFT", "BOND", "", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&answer("82"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).iter().all(|m| asked_class(m).is_none()), "asked once in the session");

        ccp.send_secdef_request_by_symbol(83, "SPX", "IND", "CBOE", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        let index = pipe_msg("35=d|320=83|323=4|55=SPX|167=IND|207=CBOE|6008=416904|6411=CBOE/IND/NONE|15=USD|146=0");
        ccp.process_ccp_message(&index, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ccp_messages_sent(&mut server).iter().all(|m| asked_class(m).is_none()), "an index's class is not asked");
    }

    // ibx#735 (observed on paper, 13/10/2026: MES contract details, 3 of 3,
    // the class 0 to 3 ms after the future's answer and no company): a
    // future whose underlying is known asks its class, then its trading
    // hours; no company, no preferred definition.
    #[test]
    fn a_future_with_a_known_underlying_asks_its_class_then_its_hours() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        context.agg_groups.insert(362687422, -1);
        ccp.send_secdef_request_by_symbol(23, "MNQ", "FUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        let _ = ccp_messages_sent(&mut server);
        ccp.process_ccp_message(&five_future_records("23", "CME/FUT"), &mut conn, &mut context, &shared, &None,
            &mut hb, "DU1");
        let sent = ccp_messages_sent(&mut server);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(asked_class(&sent[0]), Some("CME/FUT/GLOBEX"));
        assert!(sent[1].contains("6040=106") && sent[1].contains("6256=CME/FUT"), "{sent:?}");
        assert!(ccp.pending_company.is_empty() && ccp.pending_preferred.is_empty() && ccp.held_details.is_empty());
    }

    // ibx#734 (the reference 10.51, paper, 13/10/2026: after contract
    // details asked the classes of an AAPL CFD and of MES, three CFD orders
    // and an MES order asked nothing and were refused for the answer): the
    // answer is kept for the orders; an order's question on that class is
    // not sent again.
    #[test]
    fn a_class_asked_for_contract_details_is_not_asked_again_by_an_order() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        context.agg_groups.insert(362687422, -1);
        ccp.send_secdef_request_by_symbol(24, "MNQ", "FUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        ccp.process_ccp_message(&five_future_records("24", "CME/FUT"), &mut conn, &mut context, &shared, &None,
            &mut hb, "DU1");
        let _ = ccp_messages_sent(&mut server);
        let answer = pipe_msg("35=U|43=N|6040=66|6410=0|6411=CME/FUT/GLOBEX|6412=1|6413=DU1|6414=0");
        ccp.process_ccp_message(&answer, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(context.trading_permissions.get("CME/FUT/GLOBEX"), Some(&false));
        assert!(context.permissions_asked.is_empty());
        super::super::order_builder::ask_permission(&mut context, conn.as_mut().unwrap(), &mut hb, "CME/FUT/GLOBEX");
        assert!(ccp_messages_sent(&mut server).is_empty(), "not asked again");
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
    // ibx#636: after an order's definition, its trading hours are asked
    // once per schedule key and session, without holding the order; their
    // zone becomes the contract's (the reference 10.51, paper 12/10/2026:
    // one request for five AAPL orders, its answer in US/Eastern).
    #[test]
    fn an_orders_definition_asks_its_trading_hours_once() {
        let (mut ccp, _context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let key = "1/STK/NASDAQ#LITE";
        let def = pipe_msg(&format!("35=d|43=N|320=ord1|322=*|323=4|{}6008=265598|6046=BEST,NASDAQ,",
            listing("AAPL", "265598", "BEST", key, "26")));
        let defs = crate::control::contracts::parse_secdef_records(&def).unwrap();
        ccp.ask_order_schedule(defs.first(), &shared, &mut conn, &mut hb);
        ccp.ask_order_schedule(defs.first(), &shared, &mut conn, &mut hb);
        let sent: Vec<String> = ccp_messages_sent(&mut server).into_iter().filter(|m| m.contains("6040=106")).collect();
        assert_eq!(sent.len(), 1, "one request for the key: {sent:?}");
        assert!(sent[0].contains(&format!("6256={key}")), "{}", sent[0]);
        // Named as the reference names it, in the series of the option
        // model's queries (ibx#670).
        assert!(sent[0].contains("320=SubMsgXMLQuery1"), "{}", sent[0]);
        assert_eq!(ccp.optcalc.next_query_id(), "SubMsgXMLQuery2");
        assert_eq!(shared.reference.time_zone_id(265598), None, "the order does not wait for it");
        let schedule = pipe_msg(&format!("35=U|6040=107|6256={key}|6734=US/Eastern|6840=0"));
        ccp.handle_schedule_reply(&schedule, &shared, &None);
        assert_eq!(shared.reference.time_zone_id(265598).as_deref(), Some("US/Eastern"));
        assert!(ccp.order_zone_waits.is_empty());
        // Known for the session: the next order asks nothing.
        ccp.ask_order_schedule(defs.first(), &shared, &mut conn, &mut hb);
        assert!(ccp_messages_sent(&mut server).iter().all(|m| !m.contains("6040=106")));
    }

    // ibx#728: from an order's contract answer, the trading hours wait for
    // the order's turn (after its permission question): nothing written,
    // one request due for the key, named now in its series.
    #[test]
    fn an_orders_trading_hours_wait_for_the_order() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let key = "1/STK/NASDAQ#LITE";
        let def = pipe_msg(&format!("35=d|43=N|320=ord1|322=*|323=4|{}6008=265598|6046=BEST,NASDAQ,",
            listing("AAPL", "265598", "BEST", key, "26")));
        let defs = crate::control::contracts::parse_secdef_records(&def).unwrap();
        assert_eq!(ccp.defer_order_schedule(defs.first(), &shared, &mut context), Some(265598));
        assert_eq!(ccp.defer_order_schedule(defs.first(), &shared, &mut context), Some(265598));
        assert_eq!(context.order_hours_due, [(265598, key.to_string(), "SubMsgXMLQuery1".to_string())]);
        assert_eq!(ccp.optcalc.next_query_id(), "SubMsgXMLQuery2");
        // Its answer gives the zone, as when it is sent at once.
        let schedule = pipe_msg(&format!("35=U|6040=107|6256={key}|6734=US/Eastern|6840=0"));
        ccp.handle_schedule_reply(&schedule, &shared, &None);
        assert_eq!(shared.reference.time_zone_id(265598).as_deref(), Some("US/Eastern"));
        assert!(ccp.order_zone_waits.is_empty());
    }

    #[test]
    fn concurrent_lookups_sharing_a_schedule_key_keep_their_own_rows() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let key = "1/STK/NASDAQ#LITE";
        ccp.send_contract_details_by_con_id(100, 756733, "SMART", &mut conn, &mut hb);
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

    // ibx#485: a caller's lookup waits for its per-exchange replies with
    // no deadline: nothing goes out before them.
    #[test]
    fn a_callers_fanout_waits_for_every_reply() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.send_secdef_request_by_symbol(51, "AAPL", "STK", "SMART", "USD", &Default::default(), &mut None,
            &mut HeartbeatState::new());
        reply(&mut ccp, &mut context, &shared, &stock_reply("51"));
        assert_eq!(ccp.pending_fanout.len(), 1);
        ccp.pending_fanout[0].deadline = Instant::now() - std::time::Duration::from_secs(1);

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert_eq!(ccp.pending_fanout.len(), 1, "still waiting");
        assert!(shared.reference.drain_contract_details().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
    }

    // ── ibx#438: CONTFUT, by conId with an exchange, bond issuer ──

    /// A future record of a definition reply, without a schedule key.
    fn future_listing(con_id: &str, local: &str, month: &str) -> String {
        format!("55=ES|167=FUT|207=CME|6008={con_id}|6031=67|15=USD|58=ES|6035={local}|6058=ES|200={month}|231=50|")
    }

    fn future_reply(req: &str, listings: &[String]) -> Vec<u8> {
        let mut text = format!("35=d|43=N|320={req}|322=*|323=4|");
        for l in listings {
            text.push_str(l);
        }
        text.push_str("146=1|6038=Y|6019=1|6031=67|6026=1|6023=0|6027=0.25|6030=1|6344=1|");
        pipe_msg(&text)
    }

    // ibx#527 (recording of 02/10/2026, bond lookups by issuer; paper runs
    // of 08/10/2026): the rows that wait for a schedule leave in record
    // order, whatever the order of the schedule answers, and a row whose
    // schedule came earlier in the session is given at once, before the
    // rows that wait, with no new schedule request.
    #[test]
    fn waiting_rows_leave_in_record_order_and_a_known_schedule_does_not_wait() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let records = |keys: &[&str]| {
            let listings: Vec<String> = (0..keys.len())
                .map(|i| future_listing(&(100 + i).to_string(), "ESZ6", "202612")).collect();
            let mut defs = crate::control::contracts::parse_secdef_records(&future_reply("1", &listings)).unwrap();
            assert_eq!(defs.len(), keys.len());
            for (def, key) in defs.iter_mut().zip(keys) {
                def.join_key = key.to_string();
            }
            defs
        };
        let schedule = |key: &str| pipe_msg(&format!("35=U|6040=107|6256={key}"));
        let asked = |server: &mut _| ccp_messages_sent(server).iter().filter(|m| m.contains("6040=106")).count();
        let ids = |shared: &SharedState| shared.reference.drain_contract_details().iter()
            .map(|(req, d)| (*req, d.con_id)).collect::<Vec<_>>();

        ccp.deliver_contract_rows(41, records(&["A", "B", "A", "C"]), None, &shared, &None, &mut conn, &mut hb);
        assert_eq!(asked(&mut server), 3, "one request per key");
        // The answers of the later rows first: nothing leaves before the first row.
        ccp.process_ccp_message(&schedule("C"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.process_ccp_message(&schedule("B"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(ids(&shared).is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
        ccp.process_ccp_message(&schedule("A"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ids(&shared), [(41, 100), (41, 101), (41, 102), (41, 103)]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![41]);

        // A second lookup: the rows of the known keys at once, the new key waits.
        ccp.deliver_contract_rows(42, records(&["D", "A", "B"]), None, &shared, &None, &mut conn, &mut hb);
        assert_eq!(asked(&mut server), 1, "the new key only");
        assert_eq!(ids(&shared), [(42, 101), (42, 102)]);
        assert!(shared.reference.drain_contract_details_end().is_empty());
        ccp.process_ccp_message(&schedule("D"), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(ids(&shared), [(42, 100)]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![42]);
        assert!(ccp.pending_schedule_pair.is_empty());

        // A row out of time leaves without its schedule, and frees the rows behind it.
        ccp.deliver_contract_rows(43, records(&["E", "A"]), None, &shared, &None, &mut conn, &mut hb);
        assert_eq!(ids(&shared), [(43, 101)]);
        ccp.pending_schedule_pair[0].deadline = Instant::now();
        ccp.sweep_pending_schedule_pairs(&shared, &None);
        assert_eq!(ids(&shared), [(43, 100)]);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![43]);
    }

    // ibx#754 (the reference 10.51, paper, 18/10/2026, market closed): an
    // answer whose records trade on a price with a factor gets 2130 once,
    // naming the product for one record and the count for several; it
    // comes before the rows that wait for their schedule, after the end
    // when none waits.
    #[test]
    fn records_with_a_factor_get_one_warning() {
        let (mut ccp, _context, shared) = u186_test_state();
        let (client, _server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let def = |symbol: &str, ev: &str, key: &str| crate::control::contracts::ContractDefinition {
            con_id: 1, symbol: symbol.into(), ev_rule: ev.into(), join_key: key.into(), ..Default::default()
        };
        assert_eq!(factor_warning(&[def("SIEGR", "factor", ""), def("SIEGR", "", "")]).as_deref(),
            Some("Warning: SIEGR product is trading on the basis of currency price with factor"));
        assert_eq!(factor_warning(&[def("BACR", "factor", ""), def("BACR", "factor", "")]).as_deref(),
            Some("Warning: 2 products are trading on the basis of currency price with factor"));
        assert_eq!(factor_warning(&[def("IBM", "", "")]), None);

        let records = vec![def("SIEGR", "factor", "")];
        let factor = factor_warning(&records);
        ccp.deliver_contract_rows(51, records, factor, &shared, &None, &mut conn, &mut hb);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![51], "no row waits: the end is given");
        assert_eq!(shared.reference.drain_historical_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(51, 2130)]);

        let records = vec![def("BACR", "factor", ""), def("BACR", "factor", "K")];
        let factor = factor_warning(&records);
        ccp.deliver_contract_rows(52, records, factor, &shared, &None, &mut conn, &mut hb);
        assert!(shared.reference.drain_contract_details_end().is_empty(), "a row waits for its schedule");
        assert_eq!(shared.reference.drain_historical_errors(),
            [(52, 2130, "Warning: 2 products are trading on the basis of currency price with factor".to_string())]);
    }

    // Captured 02/10/2026 (b1_438_lookups): CONTFUT ES CME goes out as the
    // continuous future lookup, without the source; the one row is the
    // front month with secType CONTFUT.
    #[test]
    fn contfut_lookup_as_the_reference() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.market_rule_by_exchange.insert((515416632, "CME".into()), 67);
        ccp.send_secdef_request_by_symbol(9480, "ES", "CONTFUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server), ["35=c|320=FixSecDefReqBySymbol1|321=2|55=ES|167=FUT|6857=2|100=CME|15=USD"]);
        let reply = future_reply("FixSecDefReqBySymbol9480", &[future_listing("515416632", "ESZ6", "202612")]);
        ccp.process_ccp_message(&reply, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1.con_id, rows[0].1.continuous), (9480, 515416632, true));
        assert_eq!(crate::api::types::ContractDetails::from_definition(&rows[0].1).contract.sec_type, "CONTFUT");
        assert_eq!(shared.reference.drain_contract_details_end(), [9480]);
        assert!(ccp.pending_continuous.is_empty() && ccp.pending_secdef.is_empty());
    }

    // Captured 02/10/2026: FUT+CONTFUT sends the continuous lookup, then,
    // once it is answered, the futures lookup; the CONTFUT row comes first,
    // then every future (the front month again, as FUT).
    #[test]
    fn fut_and_contfut_lookup_as_the_reference() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        for con_id in [515416632, 586139767] {
            ccp.market_rule_by_exchange.insert((con_id, "CME".into()), 67);
        }
        ccp.send_secdef_request_by_symbol(9481, "ES", "FUT+CONTFUT", "CME", "USD", &Default::default(), &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server), ["35=c|320=FixSecDefReqBySymbol1|321=2|55=ES|167=FUT|6857=2|100=CME|15=USD"]);
        let front = future_listing("515416632", "ESZ6", "202612");
        ccp.process_ccp_message(&future_reply("FixSecDefReqBySymbol9481", std::slice::from_ref(&front)), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.drain_contract_details().is_empty(), "the rows wait for the futures lookup");
        assert_eq!(ccp_messages_sent(&mut server), ["35=c|320=FixSecDefReqBySymbol2|321=2|6088=Socket|55=ES|167=FUT|100=CME|15=USD"]);
        let futures = future_reply("FixSecDefReqBySymbol9481", &[front, future_listing("586139767", "ESZ7", "202712")]);
        ccp.process_ccp_message(&futures, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows: Vec<(i64, String)> = shared.reference.drain_contract_details().iter()
            .map(|(_, d)| (d.con_id, crate::api::types::ContractDetails::from_definition(d).contract.sec_type)).collect();
        assert_eq!(rows, [(515416632, "CONTFUT".to_string()), (515416632, "FUT".to_string()), (586139767, "FUT".to_string())]);
        assert_eq!(shared.reference.drain_contract_details_end(), [9481]);
        assert!(ccp.pending_continuous.is_empty());
    }

    // The reference's decoding of the security type: CONTFUT+FUT too; the
    // text is matched exactly.
    #[test]
    fn contfut_decoding() {
        let f = crate::types::SecDefFilters::default();
        assert_eq!(lookup_sec_type("CONTFUT", &f), ("FUT", true, false));
        assert_eq!(lookup_sec_type("FUT+CONTFUT", &f), ("FUT", true, true));
        assert_eq!(lookup_sec_type("CONTFUT+FUT", &f), ("FUT", true, true));
        assert_eq!(lookup_sec_type("contfut", &f), ("contfut", false, false));
        let bond = crate::types::SecDefFilters { issuer_id: "e1400789".into(), ..Default::default() };
        assert_eq!(lookup_sec_type("BOND", &bond), ("FIXED", false, false));
        assert_eq!(lookup_sec_type("CONTFUT", &bond), ("FIXED", false, false));
    }

    // Captured 02/10/2026: a bond issuer lookup is a fixed income lookup
    // with the issuer last; no symbol, no empty currency.
    #[test]
    fn bond_issuer_lookup_as_the_reference() {
        let (mut ccp, _context, _shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let f = crate::types::SecDefFilters { issuer_id: "e1400789".into(), ..Default::default() };
        ccp.send_secdef_request_by_symbol(9488, "", "BOND", "", "USD", &f, &mut conn, &mut hb);
        ccp.send_secdef_request_by_symbol(9489, "", "", "", "", &f, &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server), [
            "35=c|320=FixSecDefReqBySymbol1|321=2|6088=Socket|167=FIXED|15=USD|6454=e1400789",
            "35=c|320=FixSecDefReqBySymbol2|321=2|6088=Socket|167=FIXED|6454=e1400789",
        ]);
    }

    // Captured 02/10/2026: a conId with an exchange is asked by conId on
    // that exchange; without an exchange, the preferred contract of the
    // conId; the reply gives the row and the end.
    #[test]
    fn lookup_by_con_id_as_the_reference() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.send_contract_details_by_con_id(9483, 265598, "ISLAND", &mut conn, &mut hb);
        ccp.send_contract_details_by_con_id(9484, 265598, "", &mut conn, &mut hb);
        ccp.send_contract_details_by_con_id(9485, 265598, "SMART", &mut conn, &mut hb);
        assert_eq!(ccp_messages_sent(&mut server), [
            // Named in the session series of the by-conId lookups (ibx#698).
            "35=c|320=socket-reqContractDetailsReqByConid1|321=2|6088=Socket|6320=1|146=1|6008=265598|6004=ISLAND",
            "35=c|320=PreferredReqByConid2|321=2|146=1|6008=265598|6004=ANYEXCH",
            "35=c|320=socket-reqContractDetailsReqByConid3|321=2|6088=Socket|6320=1|146=1|6008=265598|6004=BEST",
        ]);
        let reply = pipe_msg(
            "35=d|43=N|320=socket-reqContractDetailsReqByConid9483|322=*|323=4|\
             55=AAPL|167=STK|207=NASDAQ|6008=265598|6031=4563|15=USD|58=NMS|6035=AAPL|6058=NMS|\
             146=1|6038=Y|6019=1|6031=4563|6026=1|6023=0|6027=0.01|6030=1|6344=1|");
        ccp.process_ccp_message(&reply, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
        let rows = shared.reference.drain_contract_details();
        assert_eq!(rows.iter().map(|(r, d)| (*r, d.con_id, d.exchange.clone())).collect::<Vec<_>>(),
            [(9483, 265598, "NASDAQ".to_string())]);
        assert_eq!(shared.reference.drain_contract_details_end(), [9483]);
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

    /// The pending mark that precedes the data frame (captured
    /// 02/10/2026: a matching symbols custom message for request 41 with the
    /// pending mark set).
    fn matching_symbols_ack(req_id: &str) -> Vec<u8> {
        crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (320, req_id),
            (8164, "1"),
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
        });
        let key = "1/STK/ARCA#LITE";
        ccp.send_contract_details_by_con_id(1001, 756733, "SMART", &mut None, &mut hb);
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

    // ibx#729 (the reference 10.51, paper, 13/10/2026: the logon's answer
    // of four kinds): the list of the stored order presets is kept, a
    // pair's currency kind as named; an answer to a save changes nothing.
    #[test]
    fn the_logons_preset_list_is_kept() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let list = pipe_msg("35=U|43=N|6040=194|6556=OPR.2|8166=L|8176=1|8167=4|8168=s=CASH|8169=v=1|8170=1756999922.206|8168=s=CASH&tc=EUR|8169=v=1|8170=1730816660.961|8168=s=COMB|8169=v=1|8170=1757274907.710|8168=s=STK|8169=v=1|8170=1776711654.126");
        ccp.process_ccp_message(&list, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        let stored = context.presets_stored.clone().expect("the list");
        let mut kinds: Vec<&str> = stored.iter().map(String::as_str).collect();
        kinds.sort();
        assert_eq!(kinds, ["CASH", "CASH&tc=EUR", "COMB", "STK"]);
        let saved = pipe_msg("35=U|43=N|6040=194|6556=OPR.19|8166=S|8168=s=CFD|8170=1791896948.081");
        ccp.process_ccp_message(&saved, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(context.order_presets.is_empty());
        assert_eq!(context.presets_stored, Some(stored));
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

    /// The event channel gets each answer once, an empty one included, and
    /// nothing for the pending mark or an answer with an error text
    /// (ibx#387).
    #[test]
    fn matching_symbols_answers_go_to_the_event_channel() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let event_tx = Some(event_tx);
        let mut hb = HeartbeatState::new();
        ccp.pending_matching_symbols.extend([(1, 11), (2, 12), (3, 13)]);

        let failed = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "U"), (6040, "186"), (320, "3"), (58, "no service"),
        ], 1);
        for msg in [matching_symbols_ack("1"), matching_symbols_msg("1", &[]),
                    matching_symbols_msg("2", &[("AAPL", "265598")]), failed] {
            ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &event_tx, &mut hb, "DU1");
        }

        let events: Vec<String> = event_rx.try_iter().map(|e| match e {
            Event::SymbolSamples { req_id, matches } => format!(
                "{}:{}", req_id, matches.iter().map(|m| m.symbol.as_str()).collect::<Vec<_>>().join(","),
            ),
            other => panic!("only matching-symbols answers are expected: {other:?}"),
        }).collect();
        assert_eq!(events, ["11:", "12:AAPL"]);
        // The same answers stay readable from the shared state.
        assert_eq!(shared.reference.drain_matching_symbols().iter().map(|d| d.0).collect::<Vec<_>>(), [11, 12]);
        assert_eq!(shared.reference.drain_historical_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(13, 10159)]);
    }

    #[test]
    fn matching_symbols_ack_frame_does_not_consume_the_request() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push((1, 11));

        // The not-ready ack (no count of matches) arrives first — it must not pop the
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

    fn sent_frames(server: &mut crate::protocol::connection::MemTransport) -> String {
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
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.send_matching_symbols_request(500, "AAPL", &mut conn, &mut hb, &shared);
        // The second one waits for the answer of the first and the pause.
        ccp.send_matching_symbols_request(501, "MSFT", &mut conn, &mut hb, &shared);
        let msg = matching_symbols_msg("1", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.pump_matching_symbols(Instant::now() + MATCHING_SYMBOLS_SEND_GAP, &mut conn, &mut hb, &shared);
        let wire = sent_frames(&mut server);
        assert!(wire.contains("|320=1|58=AAPL|") && wire.contains("|320=2|58=MSFT|"), "{}", wire);
        assert!(!wire.contains("320=500"));
        assert_eq!(ccp.pending_matching_symbols, vec![(2, 501)]);
        assert_eq!(shared.reference.drain_matching_symbols().iter().map(|d| d.0).collect::<Vec<_>>(), vec![500]);
        assert!(shared.reference.drain_historical_errors().is_empty());

        let msg = matching_symbols_msg("2", &[("MSFT", "272093")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.iter().map(|d| d.0).collect::<Vec<_>>(), vec![501]);
    }

    /// A connected auth link for the pacing tests, with its server end.
    fn paced_link() -> (Option<Connection>, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (Some(Connection::new_mem(client)), server)
    }

    fn sent_patterns(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        sent_frames(server).split('|').filter_map(|f| f.strip_prefix("58=").map(String::from)).collect()
    }

    // ibx#369, captured 02/10/2026 (b1_369_matching_pacing): AA, AAP, MSF
    // and the fourth pattern of the test back to back, NVD 1.5 s later. The
    // reference sent AA at once, the fourth pattern 1 s later (AAP and MSF
    // were replaced and got nothing), NVD 1 s after it.
    #[test]
    fn matching_symbols_pacing_as_captured() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (mut conn, mut server) = paced_link();
        let mut hb = HeartbeatState::new();
        let t0 = Instant::now();
        for (req, pattern) in [(9550, "AA"), (9551, "AAP"), (9552, "MSF"), (9553, "IB")] {
            ccp.send_matching_symbols_request(req, pattern, &mut conn, &mut hb, &shared);
        }
        assert_eq!(sent_patterns(&mut server), ["AA"]);
        assert_eq!(ccp.matching_waiting.as_ref().map(|w| w.0), Some(9553), "only the latest waits");
        // The pending mark gives the permit back; the pause still holds the
        // fourth pattern.
        ccp.process_ccp_message(&matching_symbols_ack("1"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.pump_matching_symbols(t0 + std::time::Duration::from_millis(500), &mut conn, &mut hb, &shared);
        assert!(sent_patterns(&mut server).is_empty());
        ccp.process_ccp_message(&matching_symbols_msg("1", &[("AA", "251962528")]), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        let t1 = t0 + MATCHING_SYMBOLS_SEND_GAP + std::time::Duration::from_millis(5);
        ccp.pump_matching_symbols(t1, &mut conn, &mut hb, &shared);
        assert_eq!(sent_patterns(&mut server), ["IB"]);
        ccp.send_matching_symbols_request(9554, "NVD", &mut conn, &mut hb, &shared);
        ccp.process_ccp_message(&matching_symbols_ack("2"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.process_ccp_message(&matching_symbols_msg("2", &[("IBM", "8314")]), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.pump_matching_symbols(t1 + std::time::Duration::from_millis(999), &mut conn, &mut hb, &shared);
        assert!(sent_patterns(&mut server).is_empty(), "1 s after the last send");
        ccp.pump_matching_symbols(t1 + MATCHING_SYMBOLS_SEND_GAP, &mut conn, &mut hb, &shared);
        assert_eq!(sent_patterns(&mut server), ["NVD"]);
        ccp.process_ccp_message(&matching_symbols_msg("3", &[("NVDA", "4815747")]), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        let answered: Vec<ReqId> = shared.reference.drain_matching_symbols().iter().map(|d| d.0).collect();
        assert_eq!(answered, vec![9550, 9553, 9554]);
        assert!(shared.reference.drain_historical_errors().is_empty(), "the replaced requests get no error");
    }

    // ibx#369: without a pending mark the permit comes back with the
    // answer only; the next request waits for it even after the pause.
    #[test]
    fn matching_symbols_wait_for_the_answer_without_pending_mark() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (mut conn, mut server) = paced_link();
        let mut hb = HeartbeatState::new();
        let later = Instant::now() + std::time::Duration::from_secs(5);
        ccp.send_matching_symbols_request(1, "AA", &mut conn, &mut hb, &shared);
        ccp.send_matching_symbols_request(2, "IB", &mut conn, &mut hb, &shared);
        ccp.pump_matching_symbols(later, &mut conn, &mut hb, &shared);
        assert_eq!(sent_patterns(&mut server), ["AA"]);
        ccp.process_ccp_message(&matching_symbols_msg("1", &[]), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.pump_matching_symbols(later, &mut conn, &mut hb, &shared);
        assert_eq!(sent_patterns(&mut server), ["IB"]);
    }

    // ibx#369: an answer with an error text is error 10159 with that text,
    // and gives the permit back.
    #[test]
    fn matching_symbols_reply_with_error_text() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let mut hb = HeartbeatState::new();
        ccp.pending_matching_symbols.push((4, 40));
        ccp.matching_permits = 0;
        let msg = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "U"), (6040, "186"), (320, "4"), (58, "Too many requests"),
        ], 1);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shared.reference.drain_historical_errors(),
            vec![(40, 10159, "Failed to request matching symbols:Too many requests".to_string())]);
        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert!(ccp.pending_matching_symbols.is_empty());
        assert_eq!(ccp.matching_permits, 1);
    }

    // ibx#369: the loss of the auth link gives the permit back; the
    // waiting request stays and goes out when it can.
    #[test]
    fn matching_symbols_link_loss_keeps_the_waiting_request() {
        let (mut ccp, mut context, shared) = u186_test_state();
        let (mut conn, mut server) = paced_link();
        let mut hb = HeartbeatState::new();
        let later = Instant::now() + MATCHING_SYMBOLS_SEND_GAP + std::time::Duration::from_millis(5);
        ccp.send_matching_symbols_request(1, "AA", &mut conn, &mut hb, &shared);
        ccp.send_matching_symbols_request(2, "IB", &mut conn, &mut hb, &shared);
        assert_eq!(ccp.matching_permits, 0);
        ccp.handle_disconnect(&mut context, &None);
        assert_eq!(ccp.matching_permits, 1);
        ccp.disconnected = false;
        ccp.pump_matching_symbols(later, &mut conn, &mut hb, &shared);
        assert_eq!(sent_patterns(&mut server), ["AA", "IB"]);
        assert_eq!(ccp.pending_matching_symbols.iter().map(|p| p.1).collect::<Vec<_>>(), vec![2]);
    }

    // ibx#369: with the auth link down, or when the send fails, 10159 at
    // once and nothing kept.
    #[test]
    fn matching_symbols_not_sent_gives_10159_at_once() {
        let (mut ccp, _context, shared) = u186_test_state();
        let mut hb = HeartbeatState::new();
        ccp.send_matching_symbols_request(7, "AAPL", &mut None, &mut hb, &shared);

        let (client, _server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
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
        ccp.pending_fanout.push(PendingFanout::new(9, vec![("ibxfan-9-0".to_string(), 1, "IEX".to_string())], Vec::new(), future));

        ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());

        assert_eq!(ccp.pending_secdef.len(), 1);
        assert_eq!(ccp.pending_fanout.len(), 1);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    // A session-start recovery entry (execution type and status New) for an order this session
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
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(900_010).expect("order info");
        assert_eq!(info.order.use_price_mgmt_algo, 1);
        assert_eq!(context.bracket_keys.get(&900_010).map(|k| k.to_string()).as_deref(), Some("7/0/-6183061"));
        assert_eq!(context.bracket_groups, 7);

        let entry = recovery_frame(900_011, 1_005);
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(900_011).expect("order info").order.use_price_mgmt_algo, 0);

        // ibx#605: a first report without the flag (a refusal) shows the
        // order as sent with it.
        context.price_mgmt_sent.insert(900_012);
        let entry = recovery_frame(900_012, 1_005);
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(900_012).expect("order info").order.use_price_mgmt_algo, 1);
    }

    // ibx#605 (the reference 10.51, paper, regular hours, 19/10/2026):
    // the compete values show 100 and 0.02 until a report gives the order
    // as held, then unset, also on its later reports.
    #[test]
    fn compete_values_show_until_a_report_holds_the_order() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let compete = |shared: &SharedState| {
            let o = shared.orders.get_order_info(900_020).expect("order info").order;
            (o.min_compete_size, o.compete_against_best_offset)
        };
        let entry = recovery_frame(900_020, 1_005);
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert_eq!(compete(&shared), (100, 0.02));
        let mut held = recovery_frame(900_020, 1_005);
        held.insert(6205, "1".into());
        ccp.handle_exec_report(&fix::field_view(&held), &mut context, &shared, &None, "");
        assert_eq!(compete(&shared), (i32::MAX, f64::MAX));
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert_eq!(compete(&shared), (i32::MAX, f64::MAX));
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
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert!(context.order(15).is_some(), "kept under its API order id");
        assert!(context.order(900_001).is_none());

        // A later report of the server's id reaches the same order.
        let mut cancelled = recovery_frame(900_001, 1_005);
        cancelled.insert(150, "4".into());
        cancelled.insert(39, "4".into());
        cancelled.insert(11, "900001.0".into());
        ccp.handle_exec_report(&fix::field_view(&cancelled), &mut context, &shared, &None, "");
        assert_eq!(context.finished_status(15), Some(crate::types::OrderStatus::Cancelled));
        assert!(context.order(900_001).is_none(), "no second order");

        // An API order id an order of this session has is not taken.
        context.insert_order(crate::types::Order::new(16, 0, Side::Buy, 1, 0, b'2', b'0', 0));
        let mut other = recovery_frame(900_002, 1_005);
        other.insert(6121, "16".into());
        ccp.handle_exec_report(&fix::field_view(&other), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&entry), &mut context, &shared, &None, "");
        assert!(context.order(-2).is_some(), "kept under its API order id");
        assert!(context.order(900_003).is_none());

        let mut cancelled = recovery_frame(900_003, 1_005);
        cancelled.insert(150, "4".into());
        cancelled.insert(39, "4".into());
        ccp.handle_exec_report(&fix::field_view(&cancelled), &mut context, &shared, &None, "");
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

        ccp.handle_exec_report(&fix::field_view(&recovery_frame(77, 999_999)), &mut context, &shared, &None, "");
        assert!(context.order(77).is_none(), "no slot, so the order is not tracked");

        // A recovered order on a contract that already has a slot is still tracked.
        ccp.handle_exec_report(&fix::field_view(&recovery_frame(78, 1_005)), &mut context, &shared, &None, "");
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

        ccp.handle_exec_report(&fix::field_view(&fill_frame("e1", 100, "10", 100, "10", 200)), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&fill_frame("e2", 100, "12", 200, "11", 100)), &mut context, &shared, &None, "");

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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");

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

        ccp.handle_exec_report(&fix::field_view(&fill_frame("e-final", 300, "10", 300, "10", 0)), &mut context, &shared, &None, "");

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

        ccp.handle_exec_report(&fix::field_view(&untracked_fill_frame("0000e0d5.6ab9ea82.01.01")), &mut context, &shared, &None, "DU123");

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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "DU123");
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "DU123");

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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_untracked_executions().is_empty());
        assert!(shared.portfolio.position_info(265598).is_none());
        assert!(!ccp.seen_exec_ids.contains("e-noside"));

        ccp.handle_exec_report(&fix::field_view(&untracked_fill_frame("e-noside")), &mut context, &shared, &None, "");
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
            (11u32, "93.0"), (150, "0"), (39, "0"), (6008, "1005"),
            (55, "TEST"), (54, "1"), (38, "1.5"), (44, "15"), (40, "2"),
        ].into_iter().collect(), &mut context, &shared, &None, "");
        let order = context.order(93).copied().expect("listed order tracked");
        assert_eq!(order.qty_fixed, 3 * q / 2, "1.5 shares kept");

        let fill: std::collections::HashMap<u32, String> = [
            (11u32, "93.0".to_string()), (17, "e93".into()), (150, "1".into()), (39, "1".into()),
            (55, "TEST".into()), (54, "1".into()), (38, "1.5".into()), (40, "2".into()), (44, "15".into()),
            (32, "0.5".into()), (31, "15".into()), (14, "0.5".into()), (6, "15".into()), (151, "1".into()),
            (6008, "1005".into()),
        ].into_iter().collect();
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");

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

        ccp.handle_exec_report(&fix::field_view(&frame("91", "A")), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty(), "no error on the acknowledgement");
        ccp.handle_exec_report(&fix::field_view(&frame("91", "8")), &mut context, &shared, &None, "");
        // A notice given after the report's status (ibx#486).
        assert!(shared.orders.drain_order_errors().is_empty());
        assert_eq!(shared.orders.drain_order_notices(), vec![
            (91, 201, "Order rejected - reason:Display size should be a multiple of lot size".to_string()),
        ]);

        // A repeat, and a reject for an order this session does not track.
        ccp.handle_exec_report(&fix::field_view(&frame("91", "8")), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&frame("92", "8")), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
        assert!(shared.orders.drain_order_notices().is_empty());
    }

    // A recovered order on a new contract took a slot without raising the
    // shared instrument count, so a cancel-all walking the ids below it
    // never reached the order.
    #[test]
    fn recovered_order_is_inside_the_global_cancel_range() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        context.market.try_register(265598).unwrap();
        shared.market.set_instrument_count(context.market.count());

        ccp.handle_exec_report(&fix::field_view(&recovery_frame(79, 756733)), &mut context, &shared, &None, "");

        let order = context.order(79).copied().expect("recovered order tracked");
        let count = shared.market.instrument_count();
        assert!(order.instrument < count, "instrument {} outside 0..{}", order.instrument, count);
        for instrument in 0..count {
            context.cancel_all(instrument);
        }
        assert!(context.pending_orders.drain().any(|r| matches!(r,
            crate::types::OrderRequest::CancelAll { instrument } if instrument == order.instrument)));
    }

    /// A report in pipe form, as `handle_exec_report` reads it.
    fn report_of(text: &str) -> std::collections::HashMap<u32, String> {
        text.split('|').filter_map(|kv| kv.split_once('='))
            .filter_map(|(t, v)| Some((t.parse::<u32>().ok()?, v.to_string())))
            .collect()
    }

    /// An order of an earlier session in the logon replay, not routed yet
    /// (captured 01/10/2026, fix-agent-gw.20261001-171159 seq 1124, account
    /// masked): a status report with execution type and status pending new,
    /// no API order id or client id.
    const REPLAY_NOT_ROUTED: &str = "35=8|11=1790862363895062.0|17=140781.1790867253.0|150=A|20=3|39=A|167=CS|55=SPY|6210=BEST|38=1|44=0.00|32=0|31=0.00|14=0|151=1|6=0|54=1|37=00cf16ed.000225ed.6abde767.0001|1=DUXXXXXXX|60=20261001-15:07:33|6571=20261001-13:46:05|40=5|59=0|6008=756733|15=USD|6004=BEST|6122=c|6205=1|198=NONE|6115=0|6088=Socket|6035=SPY|6419=IB";

    // The logon replay gives an order of an earlier session that is not
    // routed yet as pending new: it is in the book, PreSubmitted, of client 0
    // (no 6119, read as 0 by the reference), at the server's version.
    // Paper 04/10/2026: such orders were listed and then not found by a
    // cancel (10147), and a global cancel sent nothing.
    #[test]
    fn a_replayed_order_not_routed_yet_is_in_the_book() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        ccp.handle_exec_report(&fix::field_view(&report_of(REPLAY_NOT_ROUTED)), &mut context, &shared, &None, "");
        let order = context.order(1790862363895062).copied().expect("in the book");
        assert_eq!(order.status, crate::types::OrderStatus::PreSubmitted);
        assert_eq!(context.book.get(&1790862363895062).and_then(|e| e.owner), Some(0));
        assert_eq!(context.modify_versions.get(&1790862363895062), Some(&0));
        assert_eq!(context.last_clord.get(&1790862363895062).map(String::as_str), Some("1790862363895062.0"));
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1, "its status, as for every report of a held order");
        assert_eq!(updates[0].status, crate::types::OrderStatus::PreSubmitted);
    }

    // An order of another client, with its API ids: kept under its API
    // order id, of its client (6119), at the version the server gives; its
    // parent and OCA group as reported. A finished report of an unknown
    // order puts nothing in the book.
    #[test]
    fn a_replayed_order_keeps_its_client_version_and_links() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let parent = "35=8|11=57311390.0|17=e.1|150=0|20=3|39=0|55=AAPL|100=NASDAQ|38=1|44=238.44|14=0|151=1|54=1|37=x|40=2|6119=198|6121=35|59=0|6008=265598";
        let child = "35=8|11=57311391.2|17=e.2|150=5|20=3|39=5|55=AAPL|38=1|44=250|14=0|151=1|54=2|37=y|40=2|6119=198|6121=36|59=1|6008=265598|583=57311390|6107=57311390.0";
        ccp.handle_exec_report(&fix::field_view(&report_of(parent)), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&report_of(child)), &mut context, &shared, &None, "");
        assert_eq!(context.order(35).map(|o| o.status), Some(crate::types::OrderStatus::Submitted));
        assert_eq!(context.order(36).map(|o| o.status), Some(crate::types::OrderStatus::PreSubmitted));
        assert_eq!((context.server_id(35), context.server_id(36)), (57311390, 57311391));
        assert_eq!(context.modify_versions.get(&36), Some(&2));
        let entry = context.book.get(&36).cloned().unwrap();
        assert_eq!((entry.owner, entry.parent, entry.oca_group.as_str()), (Some(198), 35, "57311390"));

        let filled = "35=8|11=57311399.0|17=e.3|150=2|20=0|39=2|55=AAPL|38=1|32=1|31=1|14=1|151=0|54=1|37=z|40=2|59=0|6008=265598";
        ccp.handle_exec_report(&fix::field_view(&report_of(filled)), &mut context, &shared, &None, "");
        assert!(context.order(57311399).is_none());
    }

    // The reports of an order go to its client, else to client 0
    //; its notices only to its client.
    // Captured 01/10/2026: client 193 got nothing of client 0's orders.
    #[test]
    fn reports_of_another_clients_order_reach_only_its_client_or_client_0() {
        let cancelled = "35=8|11=57311390.1|41=57311390.0|17=e.9|150=4|20=0|39=4|55=AAPL|38=1|44=238.44|14=0|151=0|54=1|37=x|40=2|6119=198|6121=35|59=0|6008=265598";
        let fill = "35=8|11=57311390.0|17=e.8|150=1|20=0|39=1|55=AAPL|38=2|44=238.44|32=1|31=238.4|14=1|151=1|6=238.4|54=1|37=x|40=2|6119=198|6121=35|59=0|6008=265598";
        let parent = "35=8|11=57311390.0|17=e.1|150=0|20=3|39=0|55=AAPL|100=NASDAQ|38=2|44=238.44|14=0|151=2|54=1|37=x|40=2|6119=198|6121=35|59=0|6008=265598";
        for (me, delivered) in [(193, false), (0, true), (198, true)] {
            let mut ccp = CcpState::new();
            let mut context = Context::new();
            let shared = SharedState::new();
            shared.reference.set_api_client_id(me);
            ccp.handle_exec_report(&fix::field_view(&report_of(parent)), &mut context, &shared, &None, "");
            ccp.handle_exec_report(&fix::field_view(&report_of(fill)), &mut context, &shared, &None, "");
            ccp.handle_exec_report(&fix::field_view(&report_of(cancelled)), &mut context, &shared, &None, "");
            let updates = shared.orders.drain_order_updates();
            assert_eq!(!updates.is_empty(), delivered, "client {me}: statuses");
            assert_eq!(shared.orders.drain_fills_with_exec().len(), usize::from(delivered), "client {me}: live fill");
            let untracked = shared.orders.drain_untracked_executions();
            assert_eq!(untracked.len(), usize::from(!delivered), "client {me}: fill kept with no callback");
            assert!(untracked.iter().all(|(_, e, fe)| fe.other_client && e.order_id == 35));
            // 202 only to the order's own client.
            assert_eq!(!shared.orders.drain_order_notices().is_empty(), me == 198, "client {me}: 202");
            // The order is in the book whoever gets its reports: listed,
            // and in a global cancel.
            assert!(shared.orders.get_order_info(35).is_some());
        }
    }

    // The permId is the id part of the ClOrdID, as the reference
    // (captured 01/10/2026: permId 1790865742870063 for
    // 11=1790865742870063.0); an order of another session with no 6121 is
    // shown with order id 0.
    #[test]
    fn perm_id_is_the_clordid_id_and_no_6121_shows_order_id_0() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        ccp.handle_exec_report(&fix::field_view(&report_of(REPLAY_NOT_ROUTED)), &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates[0].perm_id, 1790862363895062);
        assert_eq!(shared.orders.api_order_id(1790862363895062), 0);
        let info = shared.orders.get_order_info(1790862363895062).unwrap();
        assert_eq!((info.order.perm_id, info.order.client_id), (1790862363895062, 0));
        assert_eq!(shared.orders.book_place(1790862363895062), (Some(0), 1));
    }

    // ibx#475: account frames captured on paper 25/09/2026 (account masked,
    // rows shortened), '|' for SOH.
    const UT_IMAGE: &str = "8=O|9=000000|35=UT|6529=AR.1|8001=AccountType|6066=1790323947|8004=INDIVIDUAL|6288=0|8001=AccountCode|6066=1790323947|8004=DU0000001|6288=0|8001=AccountReady|6066=1790323947|8004=true|6288=0|8001=DepositOnCreditHold|6066=1790323947|6288=0|8001=Leverage-S|6066=1790323947|8004=0.07|6288=0|8001=SettledCashByDate|8002=BASE|6066=1790323947|8004=20260925:932758.72933742;20260928:899133.49933742|6288=0|";
    const UM_IMAGE: &str = "8=O|9=000000|35=UM|6529=AR.1|8001=AccruedCash|15=USD|6066=1790323947|6288=0|8004=1893.50|8001=FullMaintMarginReq|15=USD|6066=1790323947|6288=0|8004=11647.75|8001=MaintMarginReq|15=USD|6066=1790323947|6288=0|8004=11647.75|8001=NetLiquidation|15=USD|6066=1790323947|6288=0|8004=953633.06|";
    const RL_IMAGE: &str = "8=O|9=000000|35=RL|6529=AR.1|8001=LedgerList|8002=BASE|15=BASE|9806=899133.4993|9819=953633.0601|9818=899133.4993|6711=0.0000|9807=52606.06|9808=0.00|9809=0.00|8007=0.00|9810=0.00|6099=26.57|6100=-791.71|9820=1|6242=1893.5|6066=1790338477|6288=0|8001=LedgerList|8002=USD|15=USD|9806=899133.4993|9819=953633.0601|9818=899133.4993|9807=52606.06|9808=0.00|9809=0.00|9810=0.00|6099=26.57|6100=-791.71|9820=1|6242=1893.5|6066=1790338477|6288=0|";
    const UT_PERIODIC: &str = "8=O|9=000105|35=UT|6529=AR.1|8001=AccountCode|6066=1790338657|8004=|8001=Cushion|6066=1790338657|8004=0.987782|6288=0|";

    fn soh(s: &str) -> String { s.replace('|', "\x01") }

    fn rows_of(frame: &str) -> Vec<(String, String, String)> {
        parse_account_rows(&soh(frame), false).0
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
            // The currency the frame gives, as 10.51 shows it (10.40 gave
            // none, ibx#637).
            row("SettledCashByDate", "BASE", "20260925:932758.72933742;20260928:899133.49933742"),
        ], "a row with no value is skipped");
        let um = rows_of(UM_IMAGE);
        assert_eq!(um, [
            row("AccruedCash", "USD", "1893.50"),
            row("FullMaintMarginReq", "USD", "11647.75"),
            row("MaintMarginReq", "USD", "11647.75"),
            row("NetLiquidation", "USD", "953633.06"),
        ], "each key as sent, the Full keys too");
        assert_eq!(parse_account_rows(&soh(UM_IMAGE), false).1, Some(1790323947));
        // The 10.40 form: no currency on the settled cash row.
        assert!(parse_account_rows(&soh(UT_IMAGE), true).0.contains(&row("SettledCashByDate", "", "20260925:932758.72933742;20260928:899133.49933742")));
    }

    // ibx#637 (the reference 10.51, paper, 13/10/2026): the server now sends
    // SettledCashByDate and its -S key once per currency (BASE and USD);
    // the reference gives each row with its currency.
    #[test]
    fn settled_cash_rows_keep_their_currency() {
        let ut = rows_of("8=O|35=UT|6529=AR.4|8001=TradingType-S|6066=1791875523|8004=STKNOPT|6288=0            |8001=SettledCashByDate|8002=BASE|6066=1791875523|8004=20261013:904151.11|6288=0            |8001=SettledCashByDate|8002=USD|6066=1791875523|8004=20261013:904151.11|6288=0            |8001=SettledCashByDate-S|8002=BASE|6066=1791875523|8004=20261013:904151.11|6288=0|");
        assert_eq!(ut, [
            row("TradingType-S", "", "STKNOPT"),
            row("SettledCashByDate", "BASE", "20261013:904151.11"),
            row("SettledCashByDate", "USD", "20261013:904151.11"),
            row("SettledCashByDate-S", "BASE", "20261013:904151.11"),
        ]);
    }

    #[test]
    fn ledger_rows_give_the_keys_per_currency() {
        let rl = rows_of(RL_IMAGE);
        assert!(rl.contains(&row("Currency", "BASE", "BASE")));
        assert!(rl.contains(&row("CashBalance", "BASE", "899133.4993")));
        assert!(rl.contains(&row("NetLiquidationByCurrency", "USD", "953633.0601")));
        assert!(rl.contains(&row("RealizedPnL", "USD", "26.57")));
        assert!(rl.contains(&row("UnrealizedPnL", "BASE", "-791.71")));
        assert!(rl.contains(&row("ExchangeRate", "USD", "1.00")));
        assert!(rl.contains(&row("RealCurrency", "BASE", "BASE")));
        assert!(!rl.iter().any(|(k, _, _)| k == "LedgerList"));
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
        // The ledger rows as numbers, by currency (ibx#486).
        let usd = events[1].ledgers.iter().find(|r| r.currency == "USD").expect("the USD row");
        assert_eq!((usd.real_currency.as_str(), usd.value(9806), usd.value(9820)), ("USD", Some(899133.4993), Some(1.0)));
        assert_eq!(events[1].ledgers.iter().map(|r| r.currency.as_str()).collect::<Vec<_>>(), ["BASE", "USD"]);
    }

    // ibx#486: a ledger frame of a summary keeps the frame's account and
    // leaves a value Java does not read as a number unset.
    #[test]
    fn ledger_rows_of_a_summary_frame() {
        let rows = parse_ledger_rows(&soh("8=O|35=RL|6529=SR.Socket.39|8001=AccountCode|8004=DU1|8001=LedgerList|8002=BASE|15=BASE|9806=933115.0500|8174=nan|"));
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].account.as_str(), rows[0].value(9806), rows[0].value(8174)), ("DU1", Some(933115.05), None));
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
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "");
        // The fill's cash, for the daily P&L: a buy of 1 at 770.56.
        let cash = shared.portfolio.money_since_seed().get(&756733).copied().unwrap();
        assert!((cash + 770.56).abs() < 1e-9, "cash={cash}");
        // The position moves with the fill; a new position takes the fill
        // price as average cost.
        let pi = shared.portfolio.position_info(756733).unwrap();
        assert_eq!(pi.position_fixed, QTY_SCALE);
        assert_eq!(pi.avg_cost, 770.56);
        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "00025b49.6ab659f3.01.01"), (6099, "5.645252")])), &shared);
        assert_eq!(shared.portfolio.realized_since_seed().get(&756733).copied(), Some(5.645252));

        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "00025b49.6ab659f3.01.02"), (6099, "6.0")])), &shared);
        let total = shared.portfolio.realized_since_seed().get(&756733).copied().unwrap();
        assert!((total - 6.0).abs() < 1e-9, "the revision replaces: {total}");

        ccp.handle_commission_report(&fix::field_view(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01"), (6099, "3.0")])), &shared);
        let total = shared.portfolio.realized_since_seed().get(&756733).copied().unwrap();
        assert!((total - 6.0).abs() < 1e-9, "an earlier session's fill is not counted");
    }

    // ibx#465: every cancel of an order of this session gives 202 with the
    // server's reason (empty for a user cancel, captured 25/09/2026).
    #[test]
    fn a_cancel_gives_202_with_the_reason() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&fix::field_view(&routed), &mut context, &shared, &None, "");
        let done = exec_report_frame(&[(39, "4"), (150, "4")]);
        ccp.handle_exec_report(&fix::field_view(&done), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_notices(), [(42, 202, "Order Canceled - reason:".to_string())]);

        let (mut ccp, mut context, shared) = ord_status_test_state();
        let done = exec_report_frame(&[(39, "4"), (150, "4"), (58, "Order expired")]);
        ccp.handle_exec_report(&fix::field_view(&done), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_notices(), [(42, 202, "Order Canceled - reason:Order expired".to_string())]);
    }

    // ibx#487: the order message of a combo names the combo's symbol and
    // "Combo" (captured 26/09/2026, i105_combo_stock_smart: "BUY 1 QQQ,SPY
    // Combo"), not the report's 55=IECombo and exchange.
    #[test]
    fn a_combo_order_message_names_the_combo() {
        let (_, context, shared) = ord_status_test_state();
        let text = "Warning: your order will not be placed at the exchange until 2026-09-28 04:00:00 US/Eastern";
        let report = exec_report_frame(&[(54, "1"), (38, "1"), (55, "IECombo"), (167, "BAG"), (6008, "28812380"), (207, "BEST")]);
        let combo = api::Contract { con_id: 28812380, symbol: "QQQ,SPY".into(), sec_type: "BAG".into(), exchange: "SMART".into(), ..Default::default() };
        assert_eq!(order_message_399(&fix::field_view(&report), text, &context, 42, &shared, Some(&combo)),
            format!("Order Message:
BUY 1 QQQ,SPY Combo
{}", text));
    }

    // ibx#666 (the reference 10.51, paper, 17/10/2026, market closed,
    // `.tmp/666/t5/case5.py` cases I and L): the order message of a
    // structured product and of a treasury bill names them by the
    // reference's own text from their definition; a bill gives the order's
    // amount in place of its quantity.
    #[test]
    fn a_structured_product_and_a_bill_order_message_name_them() {
        let (_, mut context, shared) = ord_status_test_state();
        let text = "Warning: your order will not be placed at the exchange until 2026-10-19 08:00:00 MET";
        context.rth_types.insert((411098772, "SBF".into()), crate::engine::outside_rth::RthTypes {
            order_text: "DAX Perpetual 11088.4204 Call (CBK,0.01,11088.4204 EUR,No Rebate) (24W1Z) ".into(), ..Default::default() });
        let report = exec_report_frame(&[(54, "1"), (38, "1"), (55, "DAX"), (167, "IOPT"), (6008, "411098772"), (207, "SBF")]);
        assert_eq!(order_message_399(&fix::field_view(&report), text, &context, 42, &shared, None),
            format!("Order Message:\nBUY 1 DAX Perpetual 11088.4204 Call (CBK,0.01,11088.4204 EUR,No Rebate) (24W1Z) \n{text}"));
        context.rth_types.insert((819166829, "BEST".into()), crate::engine::outside_rth::RthTypes {
            order_text: "US-T Govt Oct01'26 912797SA6".into(), face_value: Some(1000), ..Default::default() });
        let report = exec_report_frame(&[(54, "1"), (38, "1"), (55, "US-T"), (167, "BILL"), (6008, "819166829"), (6004, "BEST")]);
        assert_eq!(order_message_399(&fix::field_view(&report), text, &context, 42, &shared, None),
            format!("Order Message:\nBUY $1K US-T Govt Oct01'26 912797SA6\n{text}"));
    }

    // ibx#666 (the reference 10.51, paper, 17/10/2026, market closed,
    // `.tmp/666/t5/case5.py` case L): a treasury bill order sent with its
    // class's permission asked; the answer, no permission, came before the
    // server's reject of reason 901. The reference gave the Inactive
    // status, 460 in place of 201, then the order Inactive once more under
    // the next permId. A reject of another reason keeps 201.
    #[test]
    fn a_reject_of_reason_901_without_the_permission_gives_460() {
        for (reason, allowed, code) in [("901", false, 460), ("901", true, 201), ("0", false, 201)] {
            let (mut ccp, mut context, shared) = ord_status_test_state();
            context.rth_types.insert((756733, "BEST".into()), crate::engine::outside_rth::RthTypes {
                permission_key: "12/BILL/USA_DEBT".into(), ..Default::default() });
            context.trading_permissions.insert("12/BILL/USA_DEBT".into(), allowed);
            let reject = exec_report_frame(&[(39, "8"), (150, "8"), (103, reason), (58, "Calculated yield to worst is negative")]);
            ccp.handle_exec_report(&fix::field_view(&reject), &mut context, &shared, &None, "");
            let reports = shared.orders.drain_order_reports();
            let shown: Vec<String> = reports.iter().map(|r| match r {
                crate::bridge::OrderReport::Update(u) => format!("{:?} {}", u.status, u.perm_id),
                crate::bridge::OrderReport::Notice(_, code, _) => code.to_string(),
            }).collect();
            if code == 460 {
                assert_eq!(shown.len(), 3, "{shown:?}");
                assert_eq!((shown[0].as_str(), shown[1].as_str()), ("Rejected 42", "460"));
                assert!(shown[2].starts_with("Rejected ") && shown[2] != "Rejected 42", "a new permId: {shown:?}");
            } else {
                assert_eq!(shown, ["Rejected 42", "201"], "{reason} {allowed}");
            }
        }
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
        ccp.handle_exec_report(&fix::field_view(&ack), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&ack), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(),
            [(42, 399, format!("Order Message:\nBUY 1 AAPL NASDAQ.NMS\n{}", text))], "once");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PreSubmitted, "the order keeps working");

        // Another message type did not reach the API in the capture.
        let price_cap = exec_report_frame(&[(39, "A"), (150, "A"), (6360, "PRICECAP"), (6361, "long text")]);
        ccp.handle_exec_report(&fix::field_view(&price_cap), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ib-agent#194: the server reports the offset of a TRAIL LIMIT set by
    // its limit price (6370 = trailStopPrice - lmtPrice); it is kept for the
    // replace and shown as lmtPriceOffset.
    #[test]
    fn the_server_trail_limit_offset_is_kept() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL"), (6370, "5"), (44, "745.66"), (6117, "750.66")]);
        ccp.handle_exec_report(&fix::field_view(&ack), &mut context, &shared, &None, "");
        assert_eq!(context.trail_limit_reported.get(&42).copied(), Some(crate::engine::context::TrailLimitReported {
            offset: 5 * PRICE_SCALE, limit: 74566 * PRICE_SCALE / 100, stop: 75066 * PRICE_SCALE / 100,
        }));
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.lmt_price_offset, 5.0);
        assert_eq!(info.order.lmt_price, 745.66);
        assert_eq!(info.order.trail_stop_price, 750.66);

        // A short report without them (seen on paper 25/09/2026) keeps them.
        let short = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL")]);
        ccp.handle_exec_report(&fix::field_view(&short), &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.lmt_price_offset, 5.0);
        assert_eq!(info.order.lmt_price, 745.66);
        assert_eq!(info.order.trail_stop_price, 750.66);

        // ibx#491: the server moves the stop and the limit with the market
        // (ib-agent#195: 6117=752.01, 44=747.01); the moved values show.
        let moved = exec_report_frame(&[(39, "0"), (150, "0"), (40, "TSL"), (6370, "5"), (44, "747.01"), (6117, "752.01")]);
        ccp.handle_exec_report(&fix::field_view(&moved), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fix::field_view(&short), &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(42).unwrap();
        assert_eq!(info.order.trail_stop_price, 752.01);
        assert_eq!(info.order.lmt_price, 747.01);
    }

    // ibx#467: a report with a good-till-cancel time in force and the DTC
    // flag is a DTC order.
    #[test]
    fn a_report_with_the_dtc_flag_is_dtc() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "2"), (59, "1"), (6436, "1")]);
        ccp.handle_exec_report(&fix::field_view(&ack), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.get_order_info(42).unwrap().order.tif, "DTC");
        let ack = exec_report_frame(&[(39, "0"), (150, "0"), (40, "2"), (59, "1")]);
        ccp.handle_exec_report(&fix::field_view(&ack), &mut context, &shared, &None, "");
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
            ccp.handle_exec_report(&fix::field_view(&frame), &mut context, &shared, &None, "");
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
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "DU1");
        assert_eq!(shared.orders.drain_fills_with_exec().len(), 1, "a fill of the gap takes the normal path");
        assert_eq!(ccp.last_exec, Some(("00025b49.6abe16e9.01.01.01".into(), "20260930-19:04:31".into())));
        // The same execution again is a duplicate and changes nothing.
        ccp.last_exec = None;
        ccp.handle_exec_report(&fix::field_view(&fill), &mut context, &shared, &None, "DU1");
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
        ccp.handle_exec_report(&fix::field_view(&trades_end), &mut context, &shared, &None, "DU1");
        assert!(ccp.status_replay_end_at.is_none(), "not the status replay end");

        shared.orders.set_open_orders_held(true);
        let status_end = frame(&[(11, "*"), (55, "*"), (37, "*"), (20, "3"), (150, "0"), (39, "0"), (6008, "265598"), (38, "1")]);
        ccp.handle_exec_report(&fix::field_view(&status_end), &mut context, &shared, &None, "DU1");
        assert!(!shared.orders.open_orders_held(), "open-order requests are answered from here (ibx#251)");

        assert!(context.order(0).is_none());
        assert_eq!(context.market.count(), 0, "no instrument registered for a marker");
        assert!(shared.orders.drain_fills_with_exec().is_empty());
        assert!(shared.orders.drain_order_updates().is_empty());
        assert!(!ccp.awaiting_status_replay);
        assert!(ccp.status_replay_end_at.is_some(), "the status replay end is seen");
    }

    // ibx#251: the end frame of the logon's order status replay lets the
    // open-order requests through, with no restored-link report (frame of
    // the cold start of 28/09/2026, captures/192/d1/replay.txt).
    #[test]
    fn login_replay_end_releases_the_open_order_requests() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.awaiting_login_replay = true;
        shared.orders.set_open_orders_held(true);
        let end = frame(&[(34, "000141"), (43, "N"), (52, "20260928-15:20:15"), (11, "*"), (17, "140781.1790608815.2"),
            (150, "0"), (20, "3"), (39, "0"), (55, "*"), (38, "0"), (32, "0"), (31, "0.00"), (14, "0"), (151, "0"),
            (6, "0"), (54, "1"), (37, "*"), (60, "20260928-15:20:15"), (40, "2"), (59, "0")]);
        ccp.handle_exec_report(&fix::field_view(&end), &mut context, &shared, &None, "DU1");
        assert!(!shared.orders.open_orders_held(), "answered from the end of the logon replay");
        assert!(!ccp.awaiting_login_replay);
        assert!(ccp.status_replay_end_at.is_none(), "no restored-link report at the logon");
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    // ibx#251: a link lost before the logon replay ended leaves the
    // requests to the replay of the new logon.
    #[test]
    fn link_loss_before_the_login_replay_leaves_it_to_the_reconnect() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        ccp.awaiting_login_replay = true;
        ccp.handle_disconnect(&mut context, &None);
        assert!(!ccp.awaiting_login_replay);
    }

    // The fill of an order at the bid made while the link was lost, as the
    // trades reply after the reconnect gave it (captures/192/netcut2,
    // i192_c4_cut60 frame 200, account masked); the order is this
    // session's order 46.
    fn outage_fill_frame(order: &str, leaves: &str) -> std::collections::HashMap<u32, String> {
        frame(&[(34, "000004"), (43, "N"), (97, "Y"), (52, "20260930-19:04:10"), (11, order),
            (17, "0000e0d5.6abd4f9d.01.01"), (6010, "fourleg"), (150, "2"), (20, "0"), (39, "2"), (167, "CS"),
            (55, "AAPL"), (100, "NASDAQ"), (207, "NASDAQ"), (38, "1"), (44, "336.58"), (32, "1"), (30, "NASDAQ"),
            (31, "336.58"), (14, "1"), (151, leaves), (851, "1"), (6, "336.58"), (54, "1"),
            (37, "00cf16ed.000225ed.6abc8fb0.0001"), (1, "DUXXXXXXX"), (60, "20260930-19:04:10"),
            (6571, "20260930-19:03:59"), (40, "2"), (6119, "198"), (6121, "46"), (59, "0"), (6008, "265598"),
            (15, "USD"), (6122, "c"), (6088, "Socket"), (6035, "AAPL"), (6419, "IB")])
    }

    // ibx#251: a fill made while the link was lost comes in the trades
    // reply after the reconnect. As the reference (ib-agent#192 C4): the
    // execution is booked and the position moves, but there is no fill
    // event and no status; the filled order is dropped and not kept as
    // finished, so a later cancel of its id gets 10147.
    #[test]
    fn outage_fill_is_booked_without_callbacks() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(265598).unwrap();
        context.insert_order(crate::types::Order::new(46, instrument, Side::Buy, 1, 33658 * PRICE_SCALE / 100, b'2', b'0', 0));
        context.set_order_status_forced(46, crate::types::OrderStatus::Submitted);
        ccp.fill_up_pending = Some("todayfillup88".into());

        ccp.handle_exec_report(&fix::field_view(&outage_fill_frame("46.0", "0")), &mut context, &shared, &None, "DUXXXXXXX");
        assert!(shared.orders.drain_fills_with_exec().is_empty(), "no execDetails");
        assert!(shared.orders.drain_order_updates().is_empty(), "no orderStatus");
        let stored = shared.orders.drain_untracked_executions();
        assert_eq!(stored.len(), 1, "kept for reqExecutions and its commission report");
        assert_eq!(stored[0].1.exec_id, "0000e0d5.6abd4f9d.01.01");
        assert_eq!(stored[0].1.order_id, 46);
        assert_eq!(stored[0].0.symbol, "AAPL");
        assert_eq!(context.position_fixed(instrument), QTY_SCALE, "the position moves");
        assert!(context.order(46).is_none());
        assert_eq!(context.finished_status(46), None, "unknown, not finished: a cancel gets 10147");
        assert_eq!(shared.orders.drain_forgotten_orders(), vec![46]);
        assert_eq!(ccp.last_exec.as_ref().map(|(id, _)| id.as_str()), Some("0000e0d5.6abd4f9d.01.01"));

        // The same execution again is not booked twice.
        context.insert_order(crate::types::Order::new(46, instrument, Side::Buy, 1, 33658 * PRICE_SCALE / 100, b'2', b'0', 0));
        ccp.handle_exec_report(&fix::field_view(&outage_fill_frame("46.0", "0")), &mut context, &shared, &None, "DUXXXXXXX");
        assert!(shared.orders.drain_untracked_executions().is_empty());

        // The end frame of the trades reply ends the window.
        let end = frame(&[(34, "000006"), (43, "N"), (52, "20260930-19:05:27"), (6556, "todayfillup88"),
            (17, "140781.1790795127.0"), (32, "*")]);
        ccp.handle_exec_report(&fix::field_view(&end), &mut context, &shared, &None, "DUXXXXXXX");
        assert_eq!(ccp.fill_up_pending, None);
    }

    // ibx#251: a part of an order filled during the outage is booked the
    // same way; the order keeps working, the status replay sets it.
    #[test]
    fn outage_partial_fill_keeps_the_order() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(265598).unwrap();
        context.insert_order(crate::types::Order::new(46, instrument, Side::Buy, 2, 33658 * PRICE_SCALE / 100, b'2', b'0', 0));
        context.set_order_status_forced(46, crate::types::OrderStatus::Submitted);
        ccp.fill_up_pending = Some("todayfillup88".into());
        let mut partial = outage_fill_frame("46.0", "1");
        partial.insert(150, "1".into());
        partial.insert(39, "1".into());
        ccp.handle_exec_report(&fix::field_view(&partial), &mut context, &shared, &None, "DUXXXXXXX");
        assert!(shared.orders.drain_fills_with_exec().is_empty());
        assert!(shared.orders.drain_order_updates().is_empty());
        assert_eq!(shared.orders.drain_untracked_executions().len(), 1);
        assert_eq!(context.order(46).unwrap().status, crate::types::OrderStatus::Submitted);
        assert!(shared.orders.drain_forgotten_orders().is_empty());
    }

    // Outside the trades reply of a reconnect a fill is a live fill.
    #[test]
    fn a_fill_outside_the_fill_up_reply_is_live() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(265598).unwrap();
        context.insert_order(crate::types::Order::new(46, instrument, Side::Buy, 1, 33658 * PRICE_SCALE / 100, b'2', b'0', 0));
        context.set_order_status_forced(46, crate::types::OrderStatus::Submitted);
        ccp.handle_exec_report(&fix::field_view(&outage_fill_frame("46.0", "0")), &mut context, &shared, &None, "DUXXXXXXX");
        assert_eq!(shared.orders.drain_fills_with_exec().len(), 1);
        assert!(shared.orders.drain_forgotten_orders().is_empty());
        assert_eq!(context.finished_status(46), Some(crate::types::OrderStatus::Filled));
    }

    // Outside a reconnect the status replay end marks nothing.
    #[test]
    fn status_replay_end_outside_a_reconnect_marks_nothing() {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        shared.orders.set_open_orders_held(true);
        ccp.handle_exec_report(&fix::field_view(&frame(&[(11, "*"), (55, "*")])), &mut context, &shared, &None, "DU1");
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
        ccp.handle_exec_report(&fix::field_view(&working("90.0")), &mut context, &shared, &None, "DU1");
        ccp.handle_exec_report(&fix::field_view(&working("91.0")), &mut context, &shared, &None, "DU1");
        ccp.handle_exec_report(&fix::field_view(&frame(&[(11, "*"), (55, "*")])), &mut context, &shared, &None, "DU1");

        assert_eq!(context.order(91).unwrap().status, OrderStatus::Submitted, "the server's status wins");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(),
            vec![(90, OrderStatus::Submitted), (91, OrderStatus::Submitted)], "one status per replayed order");

        // After the replay the status guard applies again.
        context.set_order_status_forced(91, OrderStatus::PendingCancel);
        ccp.handle_exec_report(&fix::field_view(&working("91.0")), &mut context, &shared, &None, "DU1");
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
                what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: false, include_expired: true, format_date: 1,
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

        // ibx#734 (observed on paper, 15/10/2026: historical requests by
        // description on VOD, 7203 and BHP, the class 1 to 3 ms after the
        // lookup's answer; nothing for a US stock, whose answer names no
        // class): the class of the one contract found, at once; none for a
        // display group's lookup nor for several contracts.
        #[test]
        fn a_contract_found_for_a_client_request_asks_its_class() {
            let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let mut hb = HeartbeatState::new();
            // The messages written, without framing, sequence and time.
            let ccp_messages_sent = |server: &mut crate::protocol::connection::MemTransport| -> Vec<String> {
                use std::io::Read;
                server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                while let Ok(n) = server.read(&mut chunk) {
                    if n == 0 { break; }
                    buf.extend_from_slice(&chunk[..n]);
                }
                String::from_utf8_lossy(&buf).split("8=FIX.4.1").filter(|m| !m.is_empty())
                    .map(|m| m.split('').filter(|f| !f.is_empty())
                        .filter(|f| !["9=", "34=", "52=", "10="].iter().any(|p| f.starts_with(p)))
                        .collect::<Vec<_>>().join("|"))
                    .collect()
            };
            ccp.start_contract_resolve(5, lookup(), bars(5), &mut conn, &mut hb);
            ccp.start_display_group_lookup(6, 140148322, &mut conn, &mut hb);
            ccp.start_contract_resolve(7, lookup(), bars(7), &mut conn, &mut hb);
            let _ = ccp_messages_sent(&mut server);
            let vod = |n: u32| pipe_msg(&format!("35=d|43=N|320=FixSecDefReqBySymbol{n}|322=*|323=4|55=VOD|167=STK|207=BEST|\
                6008=140148322|6411=3/STK/NONE|15=GBP|55=VOD|167=STK|207=LSE|6008=140148322|6411=LSE/STK/NONE|15=GBP"));
            // The display group's lookup first: no class.
            ccp.process_ccp_message(&vod(3489660929), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            assert!(ccp_messages_sent(&mut server).is_empty());
            // Several contracts: error 200, no class.
            let several = pipe_msg("35=d|43=N|320=FixSecDefReqBySymbol3489660930|322=*|323=4|55=VOD|167=STK|207=BEST|\
                6008=140148322|6411=3/STK/NONE|15=GBP|55=VOD|167=STK|207=BEST|6008=1|6411=X/STK/NONE|15=USD");
            ccp.process_ccp_message(&several, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            assert!(ccp_messages_sent(&mut server).is_empty());
            // One contract: the class of its record on the lookup's exchange.
            ccp.process_ccp_message(&vod(3489660928), &mut conn, &mut context, &shared, &None, &mut hb, "DU1");
            let sent = ccp_messages_sent(&mut server);
            assert_eq!(sent, ["35=U|6040=65|6410=0|6411=3/STK/NONE"]);
            assert_eq!(ccp.resolved_requests.len(), 1);
        }

        // ibx#424: the conId of a display group update that is not a
        // contract gets 473; a contract is kept and gives no answer.
        #[test]
        fn display_group_lookup_gives_473_for_no_contract_only() {
            let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
            ccp.start_display_group_lookup(2, 999999999, &mut None, &mut HeartbeatState::new());
            ccp.start_display_group_lookup(3, 8314, &mut None, &mut HeartbeatState::new());
            let none = pipe_msg("35=d|43=N|320=SecDefReqMsgReqByConid3489660928|322=*|323=4|6038=Y|6019=0|6344=0");
            let one = pipe_msg("35=d|43=N|320=SecDefReqMsgReqByConid3489660929|322=*|323=4|55=IBM|167=STK|207=BEST|6008=8314|15=USD");
            ccp.process_ccp_message(&none, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
            ccp.process_ccp_message(&one, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
            assert!(ccp.pending_resolves.is_empty());
            assert!(ccp.resolved_requests.is_empty());
            assert_eq!(shared.reference.drain_historical_errors(), vec![(2, 473, "No Financial Instrument defined".to_string())]);
            assert_eq!(shared.reference.get_contract(8314).map(|c| c.symbol), Some("IBM".to_string()));
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
            for other in ["FixSecDefReqBySymbol3758096384", "FixSecDefReqBySymbol4026531840", "FixSecDefReqBySymbol0", "SecDefReqMsgReqByConid2147483648"] {
                assert!(!ccp.contract_resolve_reply(other, &crate::control::contracts::parse_secdef_records(&reply(other)).unwrap(), &shared), "{}", other);
            }
            assert_eq!(ccp.pending_resolves.len(), 1);
            assert!(HIST_LOOKUP_FIRST_ID + HIST_LOOKUP_IDS <= crate::engine::hot_loop::farm::MD_LOOKUP_FIRST_ID);
        }

        // ibx#702 (the reference 10.51, paper 15/10/2026): a tick-by-tick
        // request with no exchange whose lookup lists AAPL on several
        // exchanges gets 200 "ambiguous" and nothing is subscribed.
        #[test]
        fn a_tick_by_tick_request_with_no_exchange_is_ambiguous() {
            let (mut ccp, shared) = (CcpState::new(), SharedState::new());
            let request = crate::types::ControlCommand::SubscribeTbt {
                req_id: 7113, con_id: 0, symbol: "AAPL".into(), exchange: String::new(), sec_type: "STK".into(),
                tbt_type: crate::types::TbtType::Last, number_of_ticks: 0, ignore_size: false, reply_tx: None,
            };
            let lookup = crate::types::ContractLookup { symbol: "AAPL".into(), sec_type: "STK".into(), currency: "USD".into(), ..Default::default() };
            ccp.start_contract_resolve(7113, lookup, request, &mut None, &mut HeartbeatState::new());
            let id = format!("FixSecDefReqBySymbol{}", ccp.pending_resolves[0].lookup_id);
            let reply = pipe_msg(&format!("35=d|43=N|320={id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|\
55=AAPL|167=STK|207=NASDAQ|6008=265598|15=USD|55=AAPL|167=STK|207=ARCA|6008=265598|15=USD"));
            assert!(ccp.contract_resolve_reply(&id, &crate::control::contracts::parse_secdef_records(&reply).unwrap(), &shared));
            assert!(ccp.resolved_requests.is_empty(), "nothing subscribed");
            let errors = shared.reference.drain_historical_errors();
            assert_eq!(errors.iter().map(|e| (e.0, e.1, e.2.as_str())).collect::<Vec<_>>(),
                [(7113, 200, "The contract description specified for AAPL is ambiguous.")]);
        }

        // ibx#703 (the reference 10.51, paper 15/10/2026): a daily-bar
        // request by symbol on a contract of market 1 (schedule key "1/...")
        // takes the zone US/Eastern; another market's stays unknown.
        #[test]
        fn a_us_contract_resolved_for_bars_has_the_us_eastern_zone() {
            let (mut ccp, shared) = (CcpState::new(), SharedState::new());
            for (req, symbol, key, con_id) in [(7300, "AAPL", "1/STK/NASDAQ#LITE", "265598"),
                (7301, "SAP", "6/STK/IBIS#LTH2RTH", "14204")]
            {
                ccp.start_contract_resolve(req, lookup(), bars(req), &mut None, &mut HeartbeatState::new());
                let id = format!("FixSecDefReqBySymbol{}", ccp.pending_resolves.last().unwrap().lookup_id);
                let reply = pipe_msg(&format!("35=d|43=N|320={id}|322=*|323=4|55={symbol}|167=STK|207=BEST|6008={con_id}|6256={key}|15=USD"));
                assert!(ccp.contract_resolve_reply(&id, &crate::control::contracts::parse_secdef_records(&reply).unwrap(), &shared));
            }
            assert_eq!(shared.reference.time_zone_id(265598).as_deref(), Some("US/Eastern"));
            assert_eq!(shared.reference.time_zone_id(14204), None);
        }

        // ibx#485: no deadline, as the reference: the request waits for the
        // lookup's answer (ibx#427 gave error 200 with a text of ibx's own).
        #[test]
        fn a_lookup_with_no_answer_keeps_waiting() {
            let (mut ccp, shared) = (CcpState::new(), SharedState::new());
            ccp.start_contract_resolve(8, lookup(), bars(8), &mut None, &mut HeartbeatState::new());
            ccp.sweep_contract_details(&shared, &None, &mut None, &mut HeartbeatState::new());
            assert_eq!(ccp.pending_resolves.len(), 1);
            assert!(shared.reference.drain_historical_errors().is_empty());
        }

        #[test]
        fn every_historical_request_kind_gets_the_con_id() {
            let kinds = vec![
                ControlCommand::FetchHeadTimestamp { req_id: 1, con_id: 0, sec_type: String::new(), exchange: String::new(), what_to_show: "TRADES".into(), use_rth: true, format_date: 1 },
                ControlCommand::FetchHistogramData { req_id: 2, con_id: 0, sec_type: String::new(), exchange: String::new(), use_rth: true, period: "1 week".into() },
                ControlCommand::FetchHistoricalTicks { req_id: 3, con_id: 0, symbol: String::new(), sec_type: String::new(), exchange: String::new(), start_date_time: String::new(), end_date_time: String::new(), number_of_ticks: 10, what_to_show: "TRADES".into(), use_rth: true, ignore_size: false },
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

#[cfg(test)]
mod logon_update_tests {
    use super::*;
    use crate::engine::hot_loop::HeartbeatState;

    fn ord_status_test_state() -> (CcpState, Context, SharedState) {
        (CcpState::new(), Context::new(), SharedState::new())
    }

    fn pipe_frame(text: &str) -> Vec<u8> {
        text.replace('|', "\x01").into_bytes()
    }

    // ibx#421: a logon update without a session epoch that turns DENYAPI
    // on stops the API; with an epoch it is ignored.
    #[test]
    fn logon_update_with_denyapi_stops_the_api() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let mut hb = HeartbeatState::new();
        ccp.process_ccp_message(&pipe_frame("35=A|52=20261002-06:20:00|6059=1790914646|6542=DENYAPI|"),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(!shared.take_connection_lost(), "a solicited logon is ignored");
        ccp.process_ccp_message(&pipe_frame("35=A|52=20261002-06:20:00|6542=APIELOG,SECDEFTA|"),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(!shared.take_connection_lost());
        let (tx, rx) = crossbeam_channel::unbounded();
        ccp.process_ccp_message(&pipe_frame("35=A|52=20261002-06:20:00|6542=APIELOG,DENYAPI|"),
            &mut None, &mut context, &shared, &Some(tx.clone()), &mut hb, "DU1");
        assert!(shared.take_connection_lost());
        assert!(matches!(rx.try_recv(), Ok(Event::Disconnected)));
        // The same list again is no change.
        ccp.process_ccp_message(&pipe_frame("35=A|52=20261002-06:20:00|6542=DENYAPI|"),
            &mut None, &mut context, &shared, &Some(tx), &mut hb, "DU1");
        assert!(!shared.take_connection_lost());
    }

    // ibx#421: the data permission stamp of a logon update is kept when it
    // changed.
    #[test]
    fn logon_update_data_permission_stamp() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        ccp.data_permissions = Some("1788356313".into());
        ccp.process_ccp_message(&pipe_frame("35=A|6764=1788356313|"),
            &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert_eq!(ccp.data_permissions.as_deref(), Some("1788356313"));
        assert!(ccp.data_permissions_seen("1788999999"));
        assert!(!ccp.data_permissions_seen("1788999999"));
    }

    // ibx#421: a logon update refreshes the pending accounts (8092, only
    // when present), the private label misc URLs (6321, only when not
    // empty) and gives its SSL farm list (8449, empty when absent) to the
    // hot loop; a solicited logon does none of it.
    #[test]
    fn logon_update_refreshes_accounts_urls_and_ssl_farms() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let mut hb = HeartbeatState::new();
        ccp.process_ccp_message(&pipe_frame("35=A|6059=1790914646|8092=DUXXXXXX1|8449=allmd|"),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(!shared.reference.account_pending("DUXXXXXX1"));
        assert_eq!(ccp.ssl_farms_update, None);
        ccp.process_ccp_message(&pipe_frame("35=A|8092=DUXXXXXX1,DUXXXXXX2|6321=account_partitions=https://example/p|8449=allmd|"),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.account_pending("DUXXXXXX1") && shared.reference.account_pending("DUXXXXXX2"));
        assert_eq!(shared.reference.misc_url("account_partitions").as_deref(), Some("https://example/p"));
        assert_eq!(ccp.ssl_farms_update.take().as_deref(), Some("allmd"));
        ccp.process_ccp_message(&pipe_frame("35=A|6321=|"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(shared.reference.account_pending("DUXXXXXX1"), "no 8092: kept");
        assert_eq!(shared.reference.misc_url("account_partitions").as_deref(), Some("https://example/p"), "empty 6321: kept");
        assert_eq!(ccp.ssl_farms_update.take().as_deref(), Some(""), "no 8449: empty list");
        ccp.process_ccp_message(&pipe_frame("35=A|8092=|"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert!(!shared.reference.account_pending("DUXXXXXX1"));
    }

    // ibx#421: a test request refreshes the clock offset from its server
    // time; a custom message carrying an account does not.
    #[test]
    fn test_request_refreshes_the_clock_offset() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let mut hb = HeartbeatState::new();
        let mut ahead = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(3600);
        ahead = ahead.round(jiff::Unit::Second).unwrap();
        let t52 = ahead.strftime("%Y%m%d-%H:%M:%S").to_string();
        ccp.process_ccp_message(&pipe_frame(&format!("35=U|52={t52}|6040=75|1=DU1|")),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        assert_eq!(shared.reference.clock().offset_ms(), 0);
        ccp.process_ccp_message(&pipe_frame(&format!("35=1|52={t52}|112=x|")),
            &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        let offset = shared.reference.clock().offset_ms();
        assert!((offset - 3_600_000).abs() < 2_000, "{offset}");
    }
}
