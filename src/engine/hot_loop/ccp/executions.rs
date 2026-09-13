use super::{CcpState, RECOVERY_TERMINATOR_GRACE};
use std::time::Instant;

use crate::bridge::{Event, RichOrderInfo, SharedState};
use crate::types::model as api;
use crate::engine::context::Context;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::types::{
    CompletedOrder, Fill, InstrumentId, Side, PRICE_SCALE,
};

use super::{HeartbeatState, emit, parse_price_tag, decode_tif, EventSink};
use crate::engine::hot_loop::parse_qty_tag;
use crate::types::qty_to_f64;

/// Synthetic ibapi error code for a parked (39=I) order's reason, delivered
/// through `Wrapper::error` since ibapi has no callback dedicated to an order
/// held with a reason. Mirrors IB's generic order-message code (399) rather
/// than the reject code (201) — an Inactive order is not rejected, it can
/// still reactivate.
const ORDER_INACTIVE_ERROR_CODE: i32 = 399;

/// IB error code 201: the venue refused the order. Distinct from the generic
/// order-message code above, so a caller can classify a refusal apart from a
/// message about an order that is still live.
const ORDER_REJECTED_ERROR_CODE: i32 = 201;

/// How long an order stands, spelled the way a caller reads it.
///
/// Stated but unmapped is reported as stated: the venue is authoritative when
/// it says anything, and a code this does not name is still better seen than
/// replaced by an unrelated local value.
fn tif_api_name(stated: &str) -> &str {
    match stated {
        "0" => "DAY", "1" => "GTC", "3" => "IOC", "4" => "FOK",
        "2" => "OPG", "6" => "GTD", "8" => "AUC",
        other => other,
    }
}

/// Whether a flag the venue states is set.
///
/// The venue writes these `Y`, `1` or `true` and chooses per field, and this
/// client writes some of them one way and read them back another: an order
/// that asked to be routed to the best offer was written `1` and read with a
/// parser that refuses `1`, and one marked a block order was written `Y` and
/// read as though only `1` counted. Both came back saying the order had asked
/// for nothing. One reader for all of them, so the two sides cannot drift.
fn flag(stated: &str) -> bool {
    stated == "1" || stated.eq_ignore_ascii_case("true") || stated.eq_ignore_ascii_case("y")
}

/// Everything else the report says about the order.
///
/// A report carries the whole order, not the handful of terms that identify
/// it, and each of these is a field a caller already has on the order handed
/// back. Left unread, an order read back from the venue came back as the
/// defaults for all of them — no display size, no trigger method, not hidden,
/// no discretionary amount — whatever the venue actually held.
///
/// Only what the report states is taken. A term it does not mention is one the
/// order does not carry, and the default already says that.
pub(crate) fn read_stated_attributes(
    order: &mut api::Order,
    parsed: &std::collections::HashMap<u32, String>,
) {
    if let Some(v) = parsed.get(&77) { order.open_close = v.clone(); }
    // The smallest quantity the order may fill. This client writes it and
    // read it back from nowhere, so an order came back saying it would take
    // any fill at all.
    if let Some(v) = parsed.get(&110).and_then(|v| v.parse::<i32>().ok()) { order.min_qty = v; }
    if let Some(v) = parsed.get(&111).and_then(|v| v.parse::<i32>().ok()) { order.display_size = v; }
    if let Some(v) = parsed.get(&126) { order.good_till_date = v.clone(); }
    if let Some(v) = parsed.get(&168) { order.good_after_time = v.clone(); }
    if let Some(v) = parsed.get(&440) { order.clearing_account = v.clone(); }
    // Written `Y` or `N` and read as a number, which neither of them is. Three
    // states and not two: manual, automated, and nothing said. Read through
    // the flag reader, a spelling this does not know turned "nothing said"
    // into the assertion that a program entered the order, which is a
    // statement a regulator reads.
    if let Some(v) = parsed.get(&1028) {
        match v.trim() {
            named if named.eq_ignore_ascii_case("y") => order.manual_order_indicator = 1,
            named if named.eq_ignore_ascii_case("n") => order.manual_order_indicator = 0,
            _ => {}
        }
    }
    if let Some(v) = parsed.get(&3055) { order.account = v.clone(); }
    if let Some(v) = parsed.get(&6102) { order.sweep_to_fill = flag(v); }
    if let Some(v) = parsed.get(&6115).and_then(|v| v.parse::<i32>().ok()) { order.trigger_method = v; }
    if let Some(v) = parsed.get(&6135) { order.hidden = flag(v); }
    if let Some(v) = parsed.get(&6152).and_then(|v| v.parse::<f64>().ok()) { order.stock_range_lower = v; }
    if let Some(v) = parsed.get(&6153).and_then(|v| v.parse::<f64>().ok()) { order.stock_range_upper = v; }
    if let Some(v) = parsed.get(&6154).and_then(|v| v.parse::<f64>().ok()) { order.delta = v; }
    if let Some(v) = parsed.get(&6207) { order.customer_account = v.clone(); }
    if let Some(v) = parsed.get(&6259).and_then(|v| v.parse::<f64>().ok()) { order.adjusted_stop_price = v; }
    if let Some(v) = parsed.get(&6260).and_then(|v| v.parse::<f64>().ok()) { order.adjusted_trailing_amount = v; }
    if let Some(v) = parsed.get(&6261) { order.adjusted_order_type = v.clone(); }
    if let Some(v) = parsed.get(&6262).and_then(|v| v.parse::<f64>().ok()) { order.adjusted_stop_limit_price = v; }
    if let Some(v) = parsed.get(&6269).and_then(|v| v.parse::<i32>().ok()) { order.adjustable_trailing_unit = v; }
    if let Some(v) = parsed.get(&6275) { order.continuous_update = flag(v); }
    if let Some(v) = parsed.get(&6279).and_then(|v| v.parse::<i32>().ok()) { order.reference_price_type = v; }
    if let Some(v) = parsed.get(&6280).and_then(|v| v.parse::<i32>().ok()) { order.volatility_type = v; }
    if let Some(v) = parsed.get(&6287) { order.not_held = flag(v); }
    if let Some(v) = parsed.get(&6290) { order.delta_neutral_order_type = v.clone(); }
    if let Some(v) = parsed.get(&6291).and_then(|v| v.parse::<f64>().ok()) { order.delta_neutral_aux_price = v; }
    // Tag 6532, which is where this client writes it. Read off 6300, an
    // order's stated time never came back at all.
    if let Some(v) = parsed.get(&6532) { order.manual_order_time = v.clone(); }
    if let Some(v) = parsed.get(&6446).and_then(|v| v.parse::<f64>().ok()) { order.scale_profit_offset = v; }
    if let Some(v) = parsed.get(&6461) { order.scale_auto_reset = flag(v); }
    if let Some(v) = parsed.get(&6488) { order.solicited = flag(v); }
    if let Some(v) = parsed.get(&6526).and_then(|v| v.parse::<i32>().ok()) { order.scale_price_adjust_interval = v; }
    if let Some(v) = parsed.get(&6527).and_then(|v| v.parse::<f64>().ok()) { order.scale_price_adjust_value = v; }
    if let Some(v) = parsed.get(&6564).and_then(|v| v.parse::<i32>().ok()) { order.ref_futures_con_id = v; }
    if let Some(v) = parsed.get(&6580).and_then(|v| v.parse::<f64>().ok()) { order.stock_ref_price = v; }
    if let Some(v) = parsed.get(&6605) { order.post_only = flag(v); }
    if let Some(v) = parsed.get(&6636) { order.professional_customer = flag(v); }
    if let Some(v) = parsed.get(&6670) { order.active_start_time = v.clone(); }
    // And where the window closes, which this client writes and read from
    // nowhere: an order handed back without it is one whose watch has no end.
    if let Some(v) = parsed.get(&6671) { order.active_stop_time = v.clone(); }
    if let Some(v) = parsed.get(&6737) { order.imbalance_only = flag(v); }
    if let Some(v) = parsed.get(&6965) { order.auto_cancel_parent = flag(v); }
    if let Some(v) = parsed.get(&8089) { order.ext_operator = v.clone(); }
    if let Some(v) = parsed.get(&8229) { order.advanced_error_override = v.clone(); }
    // Written `1`, which Rust's own boolean parser refuses, so an order that
    // asked for this came back saying nothing about it.
    if let Some(v) = parsed.get(&8265) { order.route_marketable_to_bbo = Some(flag(v)); }
    if let Some(v) = parsed.get(&8402).and_then(|v| v.parse::<i32>().ok()) { order.duration = v; }
    if let Some(v) = parsed.get(&8403).and_then(|v| v.parse::<f64>().ok()) { order.mid_offset_at_whole = v; }
    if let Some(v) = parsed.get(&8404).and_then(|v| v.parse::<f64>().ok()) { order.mid_offset_at_half = v; }
    if let Some(v) = parsed.get(&8405).and_then(|v| v.parse::<i32>().ok()) { order.post_to_ats = v; }
    if let Some(v) = parsed.get(&8411).and_then(|v| v.parse::<i32>().ok()) { order.min_compete_size = v; }
    if let Some(v) = parsed.get(&8412).and_then(|v| v.parse::<f64>().ok()) { order.compete_against_best_offset = v; }
    if let Some(v) = parsed.get(&8415).and_then(|v| v.parse::<i32>().ok()) { order.min_trade_qty = v; }
    if let Some(v) = parsed.get(&8534) { order.include_overnight = flag(v); }
    if let Some(v) = parsed.get(&9801) { order.block_order = flag(v); }
    if let Some(v) = parsed.get(&9813).and_then(|v| v.parse::<f64>().ok()) { order.discretionary_amt = v; }
    if let Some(v) = parsed.get(&9816).and_then(|v| v.parse::<f64>().ok()) { order.volatility = v; }
    if let Some(v) = parsed.get(&9822).and_then(|v| v.parse::<f64>().ok()) { order.percent_offset = v; }
}

/// Which model within the account a report is about.
///
/// The venue states it beside the account, on the same tag an order states it
/// on going out. A report that names an account-only specification states no
/// model, whatever else it carries, so the field is left empty rather than
/// filled from a tag that is about something else.
///
/// The default sleeve is not stated on an order going out, but a report that
/// names it is naming it, so what arrives is what a caller is told.
fn stated_model(parsed: &std::collections::HashMap<u32, String>) -> String {
    if parsed.contains_key(&8065) {
        return String::new();
    }
    parsed.get(&6700).cloned().unwrap_or_default()
}

/// What identifies one execution, for the window that tells a repeat from a
/// new one.
///
/// The venue's own id where it states one. Where it does not — which is the
/// shape a replay takes, and so exactly when the window matters — the fields
/// that identify the execution stand in for it, cumulative quantity among
/// them, because that is what separates two otherwise identical slices of one
/// order.
///
/// One function because every reader of this report has to agree. The
/// recovery ahead of the booking used to ask the window only when the venue
/// stated an id, so a repeated correction with none was new to the recovery
/// and a repeat to the booking: the booking refused it, and the recovery had
/// already brought a finished order back to life.
fn execution_key(parsed: &std::collections::HashMap<u32, String>, clord_id: u64) -> String {
    match parsed.get(&17).map(String::as_str).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => format!(
            "{}|{}|{}|{}|{}",
            clord_id,
            parsed.get(&60).map(|s| s.as_str()).unwrap_or(""),
            parse_qty_tag(parsed.get(&32)).unwrap_or(0),
            parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
            parse_qty_tag(parsed.get(&14)).unwrap_or(0),
        ),
    }
}

/// Convert a FIX OrderID hex string (e.g. "00cf16ed.000225ed.69ca0941.0001") to a
/// stable i64 permId.
/// Uses FNV-1a hash of the first 3 dot-segments (the stable prefix) so that permId
/// remains constant across modifications (the last segment increments on each modify).
/// Extract the value of a single FIX tag from a raw message.
/// `prefix` should include the tag number and `=` (e.g. `b"6256="`).
/// Where to book a fill whose order this session does not track.
///
/// Both the contract and the side come off the report, and both are required:
/// a guessed side would move the position the wrong way, which is worse than
/// reporting that the fill could not be placed.
pub(crate) fn untracked_fill_target(
    context: &mut Context,
    shared: &SharedState,
    parsed: &std::collections::HashMap<u32, String>,
) -> Option<(InstrumentId, Side)> {
    // A replayed execution restates history rather than reporting something
    // new. On a fresh process the venue resends prior fills with 97=Y and
    // their original ExecIDs, for orders no session tracks; booking those
    // would build a position out of the past on top of the one the position
    // feed already reports. Within a process the ExecID window catches the
    // reconnect burst, so only the untracked case needs this.
    let replayed = |tag| parsed.get(&tag).map(|v| v.eq_ignore_ascii_case("Y")).unwrap_or(false);
    if replayed(97) || replayed(43) {
        log::debug!("Untracked fill is a replay, leaving the position alone");
        return None;
    }
    let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
    if con_id == 0 {
        log::warn!("Untracked fill carries no ContractID, position not updated");
        return None;
    }
    let side = match parsed.get(&54).map(|s| s.as_str()) {
        Some("1") => Side::Buy,
        Some("2") => Side::Sell,
        Some("5") => Side::ShortSell,
        other => {
            log::warn!("Untracked fill has Side={other:?}, position not updated");
            return None;
        }
    };
    // Fallible: a full instrument table must not abort the engine on an
    // inbound message.
    let is_new_slot = context.market.con_id(
        context.market.instrument_by_con_id(con_id).unwrap_or(0),
    ) != Some(con_id);
    let Some(instrument) = context.try_register_instrument(con_id) else {
        log::warn!("Untracked fill for conId {con_id}: instrument table full, position not updated");
        return None;
    };
    // The fill changes what the account already holds. A new slot takes that
    // holding first; an existing slot already includes any fills since it.
    crate::engine::hot_loop::take_what_the_account_already_holds(
        context, shared, con_id, instrument, is_new_slot,
    );
    if let Some(symbol) = parsed.get(&55) {
        context.set_symbol(instrument, symbol.clone());
    }
    Some((instrument, side))
}

/// Which revision of an order a ClOrdID names.
///
/// A revision is chained on the number: `90`, then `90.1`, then `90.2`. One
/// with no suffix is the original, which is revision nought.
fn revision_of(clord: &str) -> u32 {
    clord.rsplit_once('.').and_then(|(_, v)| v.parse().ok()).unwrap_or(0)
}

/// The venue's stated reason for a parked or rejected order: the tag 58 text
/// with the tag 103 reason code. Either alone is ambiguous — the text is often
/// generic and the code alone names no instrument — so both are reported when
/// the report carries both. Empty when it carries neither.
pub(crate) fn stated_reason(parsed: &std::collections::HashMap<u32, String>) -> String {
    let text = parsed.get(&58).map(|s| s.as_str()).unwrap_or("");
    let code = parsed.get(&103).map(|s| s.as_str()).unwrap_or("");
    match (text.is_empty(), code.is_empty()) {
        (false, false) => format!("{text} (reason code {code})"),
        (false, true) => text.to_string(),
        (true, false) => format!("reason code {code}"),
        (true, true) => String::new(),
    }
}

/// An order id as a report states it, where this client can carry it.
///
/// An id reaches a caller as a signed number, and the id to place under next
/// is one past the highest the venue has named — so an id at the end of that
/// range is one that can be neither reported nor counted past. Taken as it
/// stood, such a report named its order under a negative id and left the
/// next id to hand out at zero, which the venue refuses as one it has used.
pub(crate) fn stated_order_id(field: &str) -> Option<u64> {
    let id = field.parse::<u64>().ok()?;
    if id > crate::bridge::MAX_ORDER_ID {
        log::warn!("a report names order {id}, which is past the highest id this client carries");
        return None;
    }
    Some(id)
}

pub(crate) fn perm_id_from_fix_order_id(s: &str) -> i64 {
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

/// The update that says an order's state is no longer known. Emitted when the
/// connection drops with it working, and again if the recovery does not
/// account for it.
pub(crate) fn uncertain_update(
    order: &crate::types::Order,
    cached: Option<crate::bridge::RichOrderInfo>,
) -> crate::types::OrderUpdate {
    crate::types::OrderUpdate {
                order_id: order.order_id,
                instrument: order.instrument,
                status: crate::types::OrderStatus::Uncertain,
                filled_qty: qty_to_f64(order.filled),
                // A fractional order deliberately tracks `qty` as zero — the
                // decimal it was submitted with lives only in the enriched
                // record. Both quantity fields are floating point end to
                // end — the dispatchers already hand them to the callback
                // as f64 — so the fraction itself survives exactly here
                // rather than being rounded to a whole unit.
                remaining_qty: {
                    let outstanding = |total: f64| (total - qty_to_f64(order.filled)).max(0.0);
                    if order.qty > 0 {
                        outstanding(qty_to_f64(order.qty))
                    } else if let Some(c) = cached.as_ref() {
                        outstanding(c.order.total_quantity)
                    } else {
                        // No exec report has reached this order yet, so
                        // neither its quantity nor a fill is known — both
                        // arrive on the same message — and there is no
                        // honest quantity to give. ibapi's own "value not
                        // set" sentinel, rather than a guessed number.
                        f64::MAX
                    }
                },
                // Nothing here states what it paid, and this update exists to
                // say what is no longer known.
                avg_price: 0,
                perm_id: cached.as_ref().map(|c| c.order.perm_id).unwrap_or(0),
                parent_id: cached.as_ref().map(|c| c.order.parent_id).unwrap_or(0),
                timestamp_ns: 0,
    }
}

/// Every tag the execution-report handler reads.
///
/// Derived from the handler itself so it cannot fall behind as fields are
/// added, the same way a definition's is.
pub(crate) static READ_FROM_AN_EXECUTION:
    std::sync::LazyLock<std::collections::HashSet<u32>> = std::sync::LazyLock::new(|| {
    // Every file this module is written across, because the reading is done
    // across all of them: the report handler is here, the routing that reads a
    // few tags of its own is next door, and the position and P&L handlers read
    // more. A scan of one names fewer tags than are read, and every tag it
    // misses is then reported as a field the venue sent that nothing read.
    let source = concat!(
        include_str!("mod.rs"),
        include_str!("executions.rs"),
        include_str!("positions.rs"),
    );
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for cap in source.split("parsed.get(&").skip(1) {
        let token: String = cap.chars().take_while(|c| *c != ')').collect();
        let token = token.trim();
        let tag = token.parse::<u32>().ok().or_else(|| {
            let needle = format!("pub const {token}: u32 = ");
            let at = crate::protocol::fix::SOURCE.find(&needle)? + needle.len();
            crate::protocol::fix::SOURCE[at..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        });
        if let Some(tag) = tag {
            seen.insert(tag);
        }
    }
    seen
});

/// What a report stated that nothing here reads, in the order stated.
///
/// Read from the bytes rather than the parsed map: a map holds one value per
/// tag, and a report repeats them.
pub fn unnamed_execution_fields(data: &[u8]) -> Vec<(u32, String)> {
    let read = &*READ_FROM_AN_EXECUTION;
    let mut out = Vec::new();
    for part in data.split(|&b| b == crate::protocol::fix::SOH) {
        if part.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(part);
        let Some((tag_str, value)) = text.split_once('=') else { continue };
        let Ok(tag) = tag_str.parse::<u32>() else { continue };
        // The message's own fields are not the fill's.
        if read.contains(&tag) || matches!(tag, 8 | 9 | 10 | 34 | 35 | 43 | 49 | 52 | 56 | 115) {
            continue;
        }
        out.push((tag, value.to_string()));
    }
    out
}

/// A price the venue stated, or nothing where it stated none.
fn stated_price(val: Option<&String>) -> Option<crate::types::Price> {
    val.and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::types::price_from_f64)
}

/// Answer a preview, and say whether it was the whole of this report.
///
/// The venue prices an order it has not placed on the same message as one it
/// has, and marks it with 6091. A preview it refuses carries no figures at all
/// — it arrives shaped exactly like the not-ready acknowledgement, every field
/// "n/a", and says why on 58 — so a refusal answers `false` and is reported
/// like any other report rather than read as a preview with nothing in it.
fn take_what_if(
    parsed: &std::collections::HashMap<u32, String>,
    clord_id: u64,
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<EventSink>,
) -> bool {
        // A preview the venue refuses: no figures, the reject status and a
        // reason. Answered as a refusal of the preview and nothing else —
        // read through the ordinary report path it became a rejected order,
        // a status said for an order never placed, and the number the caller
        // previewed under read as spent.
        if parsed.get(&39).map(String::as_str) == Some("8") && context.order(clord_id).is_some() {
            let reason = stated_reason(parsed);
            log::warn!("WhatIf refused: clord={clord_id} reason='{reason}'");
            shared.orders.push_order_inactive(clord_id, ORDER_REJECTED_ERROR_CODE, reason);
            context.retire_order(clord_id);
            return true;
        }
        const MARGIN_TAGS: [u32; 6] = [6826, 6827, 6828, 6092, 6093, 6094];
        let is_data_frame = MARGIN_TAGS.iter().any(|tag| {
            parsed.get(tag)
                .and_then(|s| s.parse::<f64>().ok())
                .is_some_and(|f| f.is_finite())
        });
        if is_data_frame
            && let Some(order) = context.order(clord_id).copied() {
                let response = crate::types::WhatIfResponse {
                    order_id: clord_id,
                    instrument: order.instrument,
                    init_margin_before: parse_price_tag(parsed.get(&6826)),
                    maint_margin_before: parse_price_tag(parsed.get(&6827)),
                    equity_with_loan_before: parse_price_tag(parsed.get(&6828)),
                    init_margin_after: parse_price_tag(parsed.get(&6092)),
                    maint_margin_after: parse_price_tag(parsed.get(&6093)),
                    equity_with_loan_after: parse_price_tag(parsed.get(&6094)),
                    commission: stated_price(parsed.get(&6378)),
                    // Stated or not: a bound the venue did not state is not a
                    // bound of nought.
                    min_commission: stated_price(parsed.get(&6379)),
                    max_commission: stated_price(parsed.get(&6380)),
                    commission_currency: parsed.get(&6381).cloned().unwrap_or_default(),
                    // Tag 6361 carries the warning, not the order's text.
                    warning_text: parsed.get(&6361).cloned().unwrap_or_default(),
                };
                log::info!("WhatIf response: clord={} initMargin={:.2}->{:.2} commission={:.2}",
                    clord_id,
                    response.init_margin_before as f64 / PRICE_SCALE as f64,
                    response.init_margin_after as f64 / PRICE_SCALE as f64,
                    response.commission.map_or(f64::NAN, |c| c as f64 / PRICE_SCALE as f64));
                context.retire_order(clord_id);
                shared.orders.push_what_if(response.clone());
                emit(event_tx, Event::WhatIf(response));
            }
    parsed.get(&39).map(|s| s.as_str()) != Some("8")
}

/// What a report says the order's state now is.
///
/// The tag 39 code is not always the status a caller is given: 39=0 is New on
/// the wire and PreSubmitted until an exchange has acknowledged the order,
/// which shows up on the same report as a destination and an exec
/// reference. Reading the wire value straight through told a caller an order
/// was working while it was still being routed.
fn status_of(
    ord_status: &str,
    clord_id: u64,
    parsed: &std::collections::HashMap<u32, String>,
) -> crate::types::OrderStatus {
    match ord_status {
        "0" => {
            // 39=0 is New on the wire and reports as PreSubmitted until the
            // order is routed to and acknowledged by an exchange (for example a
            // limit order resting pre-market). Routing
            // shows up on the same exec report as a non-empty ExDestination
            // (tag 100) plus an exec ref (tag 198) other than "NONE"; before
            // routing both are absent/"NONE". Captured in.
            let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
                || parsed.get(&198).is_some_and(|s| s != "NONE" && !s.is_empty());
            if routed {
                crate::types::OrderStatus::Submitted
            } else {
                crate::types::OrderStatus::PreSubmitted
            }
        }
        "5" => crate::types::OrderStatus::Submitted,
        "A" => crate::types::OrderStatus::PreSubmitted,
        "E" => crate::types::OrderStatus::PendingReplace,
        "6" => crate::types::OrderStatus::PendingCancel,
        "1" => crate::types::OrderStatus::PartiallyFilled,
        "2" => crate::types::OrderStatus::Filled,
        "4" | "C" => crate::types::OrderStatus::Cancelled,
        // Not cancelled. The terminal groups D with pending-cancel and its
        // own "is this terminal" test names only 2, 4, C and 8 — reading it
        // as cancelled retired an order that was still working.
        "D" => crate::types::OrderStatus::PendingCancel,
        "8" => crate::types::OrderStatus::Rejected,
        "I" => crate::types::OrderStatus::Inactive,
        // The record that closes the replay of the working set carries no
        // status at all, and no symbol and no order: it is dropped a step
        // later as the sentinel it is. Read as unknown, it was warned about
        // on every connect for the life of the client.
        "" => crate::types::OrderStatus::Uncertain,
        other => {
            // A status this does not know is not a reason to drop the
            // report: it may carry a fill, and returning here threw the
            // fill away with it. Say so and carry on to the execution.
            log::warn!("Unknown order status 39={other} for order {clord_id} — \
                        the report is still read for its execution");
            crate::types::OrderStatus::Uncertain
        }
    }
}

impl CcpState {
    /// Book what a report says was filled.
    ///
    /// A fill can arrive for an order this session does not track: one that
    /// raced its own cancel-ack out of the book, one placed from another
    /// client, one left from an earlier session. The report names the contract
    /// and the side, so it is booked from those rather than dropped — a
    /// position the account actually holds is not this client's to forget.
    ///
    /// The figures arrive as arguments rather than being read here, because
    /// the caller reads several of them again after this returns and one of
    /// them — the order's cumulative quantity — has to be read before the
    /// booking below moves it.
    fn book_fill(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        clord_id: u64,
        dedup_key: &str,
        is_resend: bool,
        // Whether the report undoes or restates an execution rather than
        // repeating one. Only those may take the order's cumulative quantity
        // down; a replay restates an earlier moment and must not.
        restates_history: bool,
        last_px: f64,
        last_shares: i64,
        // Tag 14 as the report states it, or `None` where it is absent. Absent
        // is not 0: `14=0` is a bust of everything the order held.
        report_cum_qty: Option<i64>,
        leaves_qty: i64,
        order_cum_qty: i64,
        order_avg_px: f64,
        context: &mut Context,
        shared: &SharedState,
    ) -> Option<Fill> {
        // A fill can arrive for an order this session does not track: one
        // that raced its own cancel-ack out of the book, one placed from
        // another client, or one left from an earlier session. The report
        // names the contract and the side, so book it from that rather
        // than dropping a position the account actually holds.
        //
        // What such an order has already filled is not known here, which is
        // different from its being zero. The reconciliation below works out
        // what to book by subtracting what is held from what the report
        // states, and against a baseline of zero that subtraction returns the
        // whole cumulative figure — so a bust, which restates that figure
        // downwards, booked the entire trade again as a purchase. The
        // baseline is stated as unknown instead, and an unknown one is not
        // reconciled against.
        let target = match context.order(clord_id).copied() {
            Some(order) => Some((order.instrument, order.side, Some(order.filled))),
            None => untracked_fill_target(context, shared, parsed).map(|(i, s)| (i, s, None)),
        };
        if let Some((instrument, side, already_filled)) = target {
            let booked = if is_resend {
                // A repeated correction carries the cumulative figure from
                // before any later fills, so reconciling it again undoes them.
                // Recorded here so a delivery with nothing to book against
                // does not spend the execution's key.
                if !self.record_exec_id(dedup_key) {
                    return None;
                }
                let Some(report_cum_qty) = report_cum_qty.filter(|c| *c >= 0) else {
                    // Nothing to reconcile against. Booking the increment
                    // would double what the recovery record already seeded.
                    log::debug!("Resent execution for order {clord_id} carries no CumQty — not booked");
                    return None;
                };
                let Some(already_filled) = already_filled else {
                    // Nothing here knows what this order had already filled,
                    // so nothing here can say what changed. The position feed
                    // states what the account holds and is what settles it.
                    log::warn!(
                        "Resent execution for order {clord_id}, which this session does not \
                         track: what it had already filled is unknown, so no quantity is \
                         booked from it and the position feed stands",
                    );
                    return None;
                };
                // Signed. A bust restates tag 14 downwards, and the
                // difference is what the account no longer holds.
                let delta = report_cum_qty - already_filled;
                if restates_history { delta } else { delta.max(0) }
            } else if !self.record_exec_id(dedup_key) {
                // A duplicate suppresses the fill and nothing else: the
                // report still carries a status to apply and terminal
                // bookkeeping to run, and returning here skipped both.
                log::warn!("Duplicate execution key={dedup_key} — the fill is already booked");
                0
            } else {
                // The report's own increment, which stands on its own and
                // needs no baseline.
                last_shares
            };
            if booked != 0 {
                context.adjust_order_filled(clord_id, booked);
                let fill = Fill {
                    instrument,
                    order_id: clord_id,
                    side,
                    price: crate::types::price_from_f64(last_px),
                    qty: last_shares,
                    remaining: leaves_qty,
                    timestamp_ns: context.now_ns(),
                    cum_qty: order_cum_qty,
                    avg_price: crate::types::price_from_f64(order_avg_px),
                };
                let delta = match side {
                    Side::Buy => booked,
                    Side::Sell | Side::ShortSell => -booked,
                };
                context.update_position(instrument, qty_to_f64(delta));
                shared.portfolio.set_position(fill.instrument, context.position(fill.instrument));
                // The holding the caller reads is not touched here. The broker
                // restates it on the position feed after a fill, quantity and
                // basis both, and that statement is the one the caller gets.
                // Returned rather than announced here. A caller told about a
                // fill reads the order it belongs to, and that record is written
                // further along this same report.
                return Some(fill);
            }
        }
        None
    }

    /// One order the venue has finished, filed for a caller that asked.
    ///
    /// Built from the report and nothing else: no slot is registered, no order
    /// is put in the engine's book and no position moves. What the venue is
    /// telling us is what it did, not what it is doing, and the two are read
    /// from the same message — so the difference has to be made here.
    ///
    /// Every event in an order's life arrives as its own report, so the same
    /// order arrives several times. The last one wins, which is the one
    /// carrying its final state.
    fn file_finished_order(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        raw: &[u8],
        clord_id: u64,
        status: crate::types::OrderStatus,
        shared: &SharedState,
    ) {
        // What the earlier reports about this order already said. Each report
        // states what changed and leaves the rest out, so a record rebuilt
        // from nothing every time keeps only the last report's fields.
        let at = self.finished_orders.iter().position(|held| held.order_id == clord_id);
        let was = at.map(|at| &self.finished_orders[at]);
        let kept = |stated: Option<&String>, before: Option<&str>| -> String {
            stated
                .map(String::as_str)
                .filter(|v| !v.is_empty())
                .or(before.filter(|v| !v.is_empty()))
                .unwrap_or_default()
                .to_string()
        };
        let contract = api::Contract {
            con_id: parsed
                .get(&6008)
                .and_then(|s| s.parse().ok())
                .or_else(|| was.map(|w| w.contract.con_id).filter(|id| *id != 0))
                .unwrap_or(0),
            symbol: kept(parsed.get(&55), was.map(|w| w.contract.symbol.as_str())),
            // The name a caller of the reference client knows the type by, not
            // the wire's own: a stock is written CS there and STK everywhere a
            // caller reads it, and a finished order answered CS was a type no
            // program written against that client recognises.
            sec_type: parsed
                .get(&167)
                .map(|stated| {
                    crate::control::contracts::SecurityType::from_fix(stated).to_api_str().to_string()
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    was.map(|w| w.contract.sec_type.clone()).unwrap_or_default()
                }),
            currency: kept(parsed.get(&15), was.map(|w| w.contract.currency.as_str())),
            exchange: kept(parsed.get(&100), was.map(|w| w.contract.exchange.as_str())),
            local_symbol: kept(parsed.get(&6035), was.map(|w| w.contract.local_symbol.as_str())),
            last_trade_date_or_contract_month: kept(
                parsed.get(&541).or_else(|| parsed.get(&200)),
                was.map(|w| w.contract.last_trade_date_or_contract_month.as_str()),
            ),
            strike: parsed
                .get(&202)
                .and_then(|s| s.parse().ok())
                .or_else(|| was.map(|w| w.contract.strike).filter(|k| *k != 0.0))
                .unwrap_or(0.0),
            right: kept(parsed.get(&201), was.map(|w| w.contract.right.as_str())),
            ..Default::default()
        };
        // A buy only where something said so. Read as "a sell if it says
        // sell", a report that said nothing turned a recovered sell into a
        // buy, which moves the position the wrong way by twice the fill — and
        // where nothing has ever said, the side is left unstated rather than
        // invented.
        let side = match parsed.get(&54).map(String::as_str) {
            Some("1") => "BUY".to_string(),
            Some("2") => "SELL".to_string(),
            Some("5") => "SSHORT".to_string(),
            _ => was.map(|w| w.order.action.clone()).unwrap_or_default(),
        };
        // The two halves the order type is named from, each replaced only
        // where this report states it.
        let stated_type = kept(parsed.get(&40), was.map(|w| w.ord_type.as_str()));
        let stated_inst = kept(parsed.get(&18), was.map(|w| w.exec_inst.as_str()));
        let number = |tag: u32, before: Option<f64>, unset: f64| -> f64 {
            parsed
                .get(&tag)
                .and_then(|s| s.parse().ok())
                .or_else(|| before.filter(|v| *v != unset))
                .unwrap_or(unset)
        };
        let order = api::Order {
            // The venue's own number for the order where it states one, and
            // the number it is known by here either way.
            order_id: parsed
                .get(&6121)
                .and_then(|s| s.parse().ok())
                .or_else(|| was.map(|w| w.order.order_id).filter(|id| *id != 0))
                .unwrap_or(clord_id as i64),
            client_id: parsed
                .get(&6119)
                .and_then(|s| s.parse().ok())
                .or_else(|| was.map(|w| w.order.client_id).filter(|id| *id != 0))
                .unwrap_or(0),
            // The venue's own permanent name for the order where it states
            // one. The key this is assembled under is this session's, and
            // answering it as the permanent id said the venue had named
            // something it had not.
            perm_id: parsed
                .get(&37)
                .map(|stated| perm_id_from_fix_order_id(stated))
                .filter(|id| *id != 0)
                .or_else(|| was.map(|w| w.order.perm_id).filter(|id| *id != 0))
                .unwrap_or(0),
            action: side,
            total_quantity: number(38, was.map(|w| w.order.total_quantity), 0.0),
            filled_quantity: number(14, was.map(|w| w.order.filled_quantity), 0.0),
            // And the order type the same way: the wire writes a limit order 2
            // and four kinds travel as P, told apart by the instruction beside
            // them. Passed through as stated, a finished limit order was
            // answered "2".
            // Named from both halves the venue states it in. Four kinds
            // travel as `P` and the instruction beside it is what tells them
            // apart, so the two are kept as stated and the name derived from
            // whichever pair is current — translated and then merged, a later
            // report repeating the type and not the instruction turned a
            // midpoint peg into a trailing stop.
            order_type: crate::types::orders::ord_type_api_name(&stated_type, &stated_inst)
                .to_string(),
            tif: parsed
                .get(&59)
                .map(|stated| tif_api_name(stated).to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| was.map(|w| w.order.tif.clone()).unwrap_or_default()),
            lmt_price: number(44, was.map(|w| w.order.lmt_price), f64::MAX),
            aux_price: number(99, was.map(|w| w.order.aux_price), f64::MAX),
            account: kept(parsed.get(&1), was.map(|w| w.order.account.as_str())),
            // The venue says an order is for an account and no model by
            // naming the one tag that means it, whatever it says beside.
            // Read from the model tag alone, an order explicitly cleared of
            // its model kept the one an earlier report had named.
            model_code: if parsed.contains_key(&8065) {
                String::new()
            } else if parsed.contains_key(&6700) {
                stated_model(parsed)
            } else {
                was.map(|w| w.order.model_code.clone()).unwrap_or_default()
            },
            // Everything this does not name is what an earlier report about
            // the same order said. Started from nothing each time, a terminal
            // report that restated only the outcome dropped every term the
            // venue had stated once and not repeated.
            ..was.map(|w| w.order.clone()).unwrap_or_default()
        };
        // The latest anyone said, which for these two is the report in hand:
        // a status is what the order is now and a cumulative figure is what it
        // has filled altogether.
        let filled = parse_qty_tag(parsed.get(&14))
            .or_else(|| was.map(|w| w.filled))
            .unwrap_or(0);
        // And everything else the report says about it, read the same way the
        // ordinary path reads it. Left out, a finished order came back saying
        // it was not outside regular hours, stood alone in no group, carried
        // no reference the caller gave it and waited on no condition — none of
        // which the venue had said.
        let mut order = order;
        order.oca_group = kept(parsed.get(&583), was.map(|w| w.order.oca_group.as_str()));
        order.order_ref = kept(parsed.get(&6010), was.map(|w| w.order.order_ref.as_str()));
        order.rule80a = kept(parsed.get(&47), was.map(|w| w.order.rule80a.as_str()));
        order.good_till_date = kept(parsed.get(&432), was.map(|w| w.order.good_till_date.as_str()));
        order.fa_group = kept(parsed.get(&6160), was.map(|w| w.order.fa_group.as_str()));
        order.fa_method = kept(parsed.get(&6159), was.map(|w| w.order.fa_method.as_str()));
        order.fa_percentage =
            kept(parsed.get(&6164), was.map(|w| w.order.fa_percentage.as_str()));
        order.submitter = kept(parsed.get(&109), was.map(|w| w.order.submitter.as_str()));
        order.algo_strategy =
            kept(parsed.get(&847), was.map(|w| w.order.algo_strategy.as_str()));
        // And the terms the ordinary path decodes, each replaced only where
        // this report states it.
        if let Some(stated) = parsed.get(&6433) {
            order.outside_rth = stated == "1" || stated.eq_ignore_ascii_case("true");
        }
        if let Some(stated) = parsed.get(&6370).and_then(|s| s.parse().ok()) {
            order.lmt_price_offset = stated;
        }
        if let Some(stated) = parsed.get(&6117).and_then(|s| s.parse().ok()) {
            order.trail_stop_price = stated;
        }
        if let Some(stated) = parsed.get(&6419) {
            order.clearing_intent = stated.clone();
        }
        if let Some(stated) = parsed.get(&6596) {
            order.auto_cancel_date = stated.clone();
        }
        if let Some(stated) = parsed.get(&6209).map(String::as_str) {
            order.oca_type = match stated {
                "CancelOnFillWBlock" => 1,
                "ReduceOnFillWBlock" => 2,
                "ReduceOnFillNonBlock" => 3,
                "ReduceOnFillWBlockFromTotal" => 4,
                _ => 3,
            };
        }
        if let Some(stated) = parsed.get(&8339) {
            order.use_price_mgmt_algo =
                Some(i32::from(stated == "1" || stated.eq_ignore_ascii_case("true")));
        }
        // What the order waits for. It arrives as repeating groups the
        // flattened map cannot hold, so it is read off the report itself —
        // and an order read back with its waits dropped is one that goes live
        // at once when it is placed again.
        //
        // The count is the statement, not the groups: a report stating none
        // has removed them, and read as "nothing to carry over" it left the
        // ones an earlier report had named. An order handed back still waiting
        // on a condition the venue had removed is one that waits again when it
        // is placed.
        if parsed.contains_key(&6136) {
            order.conditions = decode_conditions(raw);
        }
        if let Some(stated) = parsed.get(&6128) {
            order.conditions_cancel_order = stated == "1";
        }
        if let Some(stated) = parsed.get(&6151) {
            order.conditions_ignore_rth = stated == "1";
        }
        read_stated_attributes(&mut order, parsed);

        // What became of it, as the venue stated it — the time it finished and
        // the reason it was refused, both of which the ordinary path keeps and
        // this had been throwing away and replacing with the flat word.
        let state = api::OrderState {
            status: crate::types::order_status::order_status_str(status).to_string(),
            completed_time: match status {
                crate::types::OrderStatus::Filled
                | crate::types::OrderStatus::Cancelled
                | crate::types::OrderStatus::Rejected => kept(
                    parsed.get(&52),
                    was.map(|w| w.state.completed_time.as_str()),
                ),
                _ => was.map(|w| w.state.completed_time.clone()).unwrap_or_default(),
            },
            completed_status: match status {
                crate::types::OrderStatus::Filled => "Filled".to_string(),
                crate::types::OrderStatus::Cancelled => "Cancelled".to_string(),
                // An empty reason still marks a refusal: Inactive with no
                // completed status is the venue holding it, not refusing it.
                crate::types::OrderStatus::Rejected => parsed
                    .get(&58)
                    .filter(|why| !why.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "Rejected".to_string()),
                _ => String::new(),
            },
            // And the venue's own code for the refusal, which it states
            // beside the words.
            // Only while the order is still refused. A later report stating
            // that it filled says the refusal is not what became of it, and a
            // record carrying both said the order filled and was rejected.
            reject_reason: match status {
                crate::types::OrderStatus::Filled | crate::types::OrderStatus::Cancelled => {
                    String::new()
                }
                _ => match stated_reason(parsed) {
                    why if !why.is_empty() => why,
                    _ => was.map(|w| w.state.reject_reason.clone()).unwrap_or_default(),
                },
            },
            ..Default::default()
        };
        let merged = super::FinishedOrder {
            order_id: clord_id,
            contract,
            order,
            status,
            filled,
            state,
            ord_type: stated_type,
            exec_inst: stated_inst,
        };
        if let Some(at) = at {
            self.finished_orders[at] = merged;
            return;
        }
        // An order this answer has never seen. A window the venue never ends
        // stays open, which is right, and must not mean an unbounded answer:
        // the bound is on the orders the answer has taken, not on the records
        // waiting to be handed over, because a handover empties those and the
        // next order was simply taken beside them.
        if !self.orders_in_this_answer.contains(&clord_id)
            && self.orders_in_this_answer.len() >= super::FINISHED_ORDERS_HELD
        {
            if !self.the_answer_is_full {
                self.the_answer_is_full = true;
                log::warn!(
                    "this answer has taken {} orders and the venue has not said it is \
                     done; what it states about other orders is not part of it",
                    self.orders_in_this_answer.len(),
                );
            }
            return;
        }
        // What is finished is handed over and let go of; a later report about
        // one of them is merged into the record this answer already holds for
        // it, which is the old behaviour and bounded.
        if self.finished_orders.len() >= super::FINISHED_ORDERS_HELD {
            log::warn!(
                "the venue has stated {} finished orders without saying it is done; \
                 they are handed over as they stand",
                self.finished_orders.len(),
            );
            self.deliver_finished_orders(shared, super::Handover::Final);
        }
        // And where that freed nothing, the oldest record the answer before
        // this one left behind makes room. It belongs to no answer now: this
        // one has not taken that order, and nothing is finishing it. Kept
        // instead, the records an answer leaves behind hold the room the next
        // answer needs and it cannot take an order the venue states at all.
        while self.finished_orders.len() >= super::FINISHED_ORDERS_HELD
            && let Some(at) = self.finished_orders
                .iter()
                .position(|held| !self.orders_in_this_answer.contains(&held.order_id))
        {
            let given_up = self.finished_orders.remove(at);
            log::debug!(
                "order {} was left half-stated by an earlier answer and makes room for \
                 what the venue is stating now",
                given_up.order_id,
            );
        }
        self.orders_in_this_answer.insert(clord_id);
        self.finished_orders.push(merged);
    }

    /// Hand over the answer to what the venue has finished, whole.
    ///
    /// Built up report by report and given to the caller in one piece, because
    /// an order's events are not always adjacent and each states only what
    /// changed. Published one per report instead, a caller had to choose
    /// between the first report's fields and the last report's status.
    pub(crate) fn deliver_finished_orders(
        &mut self, shared: &SharedState, handover: super::Handover,
    ) {
        let held_now: Vec<super::FinishedOrder> = match handover {
            // Everything the venue has finished stating, and nothing else:
            // being the last handover does not make a record that says the
            // order is working into one that says it has finished. Taken
            // whole, a record still being built — which is what a window
            // handed over at its bound, or on a connection going away, is full
            // of — reached the caller as a completed order the venue never
            // said was complete.
            super::Handover::Final => {
                let (finished, still_working) = std::mem::take(&mut self.finished_orders)
                    .into_iter()
                    .partition::<Vec<_>, _>(|held| held.status.is_terminal());
                if !still_working.is_empty() {
                    log::info!(
                        "{} of the orders assembled for this answer are still being stated, \
                         so they are not part of it and are kept",
                        still_working.len(),
                    );
                }
                // Kept, not dropped: a report states what changed and leaves
                // the rest out, so the record is what every later report about
                // that order is read against. Dropped here, the terminal
                // report that finished it rebuilt the order from nothing and
                // the caller was handed it without its contract, its terms or
                // its quantities.
                self.finished_orders = still_working;
                finished
            }
            // Only the ones the venue has finished stating. A record still
            // being built says the order is working, and a record saying that
            // is one this client reads as an order the venue is holding — so a
            // caller released early was handed live orders out of an answer
            // about finished ones, and could aim a withdrawal at one.
            super::Handover::SoFar => self
                .finished_orders
                .iter()
                .filter(|held| held.status.is_terminal())
                .cloned()
                .collect(),
        };
        for held in held_now {
            // Handed over under the number a caller addresses the order by,
            // not under the name the venue files it under. The two are the
            // same on some accounts and not on others, and the second is a
            // permanent name that belongs to no client: published as an order
            // id it raised the mark this session issues its own numbers above,
            // and marked that number as finished — so an unrelated order whose
            // own number the venue happened to use as a permanent name could
            // not be cached as open again.
            let known_as = held.order.order_id.max(0) as u64;
            shared.orders.push_order_info(known_as, crate::bridge::RichOrderInfo {
                contract: held.contract,
                order: held.order,
                order_state: held.state,
                last_exec: Default::default(),
            });
            shared.orders.refile_completed_order(crate::types::CompletedOrder {
                order_id: known_as,
                instrument: 0,
                status: held.status,
                filled_qty: held.filled,
                timestamp_ns: 0,
            });
        }
    }

    /// Build an order this session never saw from the venue's account of it.
    ///
    /// At session start the venue replays what it holds as ordinary
    /// acknowledgements, and a fresh process has nothing to match them against.
    /// The record built here is what every later report for that order is read
    /// against, so a field guessed here is wrong for the order's whole life — the
    /// side most of all, since a recovered buy recorded as a sell moves the
    /// position the wrong way by twice the fill.
    fn recover_order(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        clord_id: u64,
        prior: Option<crate::types::Order>,
        context: &mut Context,
        shared: &SharedState,
    ) {
        let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
        // The side has to be stated. A guess does not stay in the recovered
        // record: every later fill for the order books through the tracked
        // path and takes its side from here, so a recovered buy recorded as
        // a sell moves the position down by the filled quantity instead of
        // up — wrong by twice the fill, and indistinguishable afterwards
        // from a side the report actually carried.
        let side = match parsed.get(&54).map(|s| s.as_str()) {
            Some("1") => Some(Side::Buy),
            Some("2") => Some(Side::Sell),
            Some("5") => Some(Side::ShortSell),
            other => {
                // The sentinel that terminates a recovery burst, and the
                // mass-status echo, both parse to id 0 and carry no side.
                // Warning about those once per connect would cry wolf on
                // the one signal that matters when a real record is
                // refused.
                if clord_id != 0 {
                    log::warn!(
                        "Recovery record for order {clord_id} has Side={other:?}; not tracking it",
                    );
                }
                None
            }
        };
        let qty = parse_qty_tag(parsed.get(&38))
            .unwrap_or_else(|| prior.map_or(0, |o| o.qty));
        // The shares this very report books, where the booking below adds
        // them, so the recovered figure must not count them too. Only a
        // positive print on an execution that is neither a correction nor a
        // bust is booked that way: those are booked by reconciling the
        // cumulative figure against the record, where the same subtraction
        // turned a forty-share correction into a forty-share purchase; and a
        // negative print books nothing, so the cumulative figure stands whole.
        let corrects = matches!(parsed.get(&20).map(String::as_str), Some("1" | "2"));
        let own_shares = match parsed.get(&150).map(String::as_str) {
            Some("F" | "1" | "2") if !corrects => parse_qty_tag(parsed.get(&32)).unwrap_or(0).max(0),
            _ => 0,
        };
        let limit_price_i64: i64 = parsed.get(&44)
            .and_then(|s| s.parse::<f64>().ok())
            .map(crate::types::price_from_f64)
            .unwrap_or_else(|| prior.map_or(0, |o| o.price));
        let stop_price_i64: i64 = parsed.get(&99)
            .and_then(|s| s.parse::<f64>().ok())
            .map(crate::types::price_from_f64)
            .unwrap_or_else(|| prior.map_or(0, |o| o.stop_price));
        // The whole name, not its first byte: `MIDPX` read as `M` names no
        // type, and the order comes back as a plain limit on the next replace.
        let ord_type_byte: u8 = parsed
            .get(&40)
            .map(|named| {
                crate::types::ord_type_from_fix(
                    named,
                    parsed.get(&18).map(String::as_str).unwrap_or_default(),
                )
            })
            .unwrap_or_else(|| prior.map_or(b'2', |o| o.ord_type));
        // A recovery record with no tag 59 states no time-in-force, and this
        // order was not placed by this session, so there is nothing to
        // recover it from. Recorded as unstated rather than guessed: either
        // guess is restated as a real instruction on the next replace, and
        // an invented DAY would expire a resting GTC order at the close.
        let tif_byte: u8 = parsed.get(&59)
            .and_then(|s| s.bytes().next())
            .unwrap_or_else(|| prior.map_or(crate::types::TIF_UNSTATED, |o| o.tif));
        if let (Some(side), true) = (side, con_id != 0 && qty > 0) {
            // Recovery is fed by gateway frames, so a full instrument
            // table must degrade to a missing order rather than take the
            // engine down. The reconnect burst replays every
            // resting order, which is exactly when the table fills.
            // Skipping only the insert keeps the order in last_clord and
            // the rich-order cache, so req_open_orders still shows it —
            // but it is NOT in the engine book, so a later fill or
            // terminal status for it is dropped and no OrderUpdate
            // reaches the caller. A missing order beats taking
            // the engine down; it is not a complete answer.
            // Counted, not only logged: a withdrawal of everything composes
            // its cancels from the book, so an order that never reached the
            // book is one it silently skips, and the caller who asked for the
            // account to be flattened is answered as though it were.
            // Whether the slot is this registration's to seed. Looking a
            // live contract up returns the slot it already has, and the
            // account's row is older than any fill booked since.
            let is_new_slot = context.market.con_id(
                context.market.instrument_by_con_id(con_id).unwrap_or(0),
            ) != Some(con_id);
            match context.try_register_instrument(con_id) {
                None => {
                    shared.orders.note_an_order_without_a_slot();
                    log::warn!(
                        "recovery: instrument table full, order clord={clord_id} con_id={con_id} not tracked in the engine book",
                    );
                }
                Some(instrument) => {
            if let Some(sym) = parsed.get(&55) {
                context.set_symbol(instrument, sym.clone());
            }
            // And what the account already said it holds on this contract. The
            // control path takes it when it makes a slot; this one made a slot
            // without it, so the book read the contract as flat while the
            // account held it — and the guard that keeps a slot resident for a
            // holding read that zero and gave the slot away.
            crate::engine::hot_loop::take_what_the_account_already_holds(
                context, shared, con_id, instrument, is_new_slot,
            );
            context.insert_order(crate::types::Order {
                order_id: clord_id,
                instrument,
                side,
                price: limit_price_i64,
                qty,
                // Seeded from the recovery push rather than assumed zero.
                // Without it a fresh process believes nothing has filled,
                // and the replayed executions behind this record all look
                // like new quantity.
                // What the order had filled before this report. A report that
                // both recovers the order and books a fill counts its own
                // shares in the cumulative figure, and the booking that
                // follows adds them again: taken whole, an order that had
                // filled forty read as eighty.
                filled: parse_qty_tag(parsed.get(&14))
                    .map(|cum| cum.saturating_sub(own_shares).max(0))
                    .unwrap_or_else(|| prior.map_or(0, |o| o.filled)),
                // An order this session never saw is working by the fact of
                // being in the push. One whose state was not known stays
                // not known here, so the status this very message carries
                // moves it, and the caller who was told it was unknown is
                // told what it is.
                status: if prior.is_some() {
                    crate::types::OrderStatus::Uncertain
                } else {
                    crate::types::OrderStatus::Submitted
                },
                ord_type: ord_type_byte,
                tif: tif_byte,
                stop_price: stop_price_i64,
            });
            self.hydrated_any = true;
            shared.orders.note_naming_began();
            log::info!("CCP recovery: inserted orderId={} sym={:?} side={:?} qty={} px={}",
                clord_id, parsed.get(&55), side, qty,
                limit_price_i64 as f64 / PRICE_SCALE as f64);
            // Published, not just tracked. The engine knowing an order is
            // working does the caller no good on its own: `req_open_orders`
            // reads what has been published, so an order the server named
            // at connect went unreported until some later message about it
            // happened to arrive. A caller asking what it already has on,
            // at the moment it starts, was told nothing.
            let sec_type_str = context.market.order_routing(instrument).0;
            shared.orders.push_order_info(clord_id, crate::bridge::RichOrderInfo {
                contract: api::Contract {
                    con_id,
                    symbol: parsed.get(&55).cloned().unwrap_or_default(),
                    sec_type: sec_type_str,
                    currency: parsed.get(&15).cloned().unwrap_or_default(),
                    ..Default::default()
                },
                order: api::Order {
                    order_id: clord_id as i64,
                    action: match side {
                        Side::Buy => "BUY".to_string(),
                        _ => "SELL".to_string(),
                    },
                    total_quantity: qty_to_f64(qty),
                    order_type: crate::types::ord_type_api_name(
                        parsed.get(&40).map_or_else(|| crate::types::ord_type_fix_str(ord_type_byte), String::as_str),
                        parsed.get(&18).map(String::as_str).unwrap_or_default(),
                    ).to_string(),
                    lmt_price: limit_price_i64 as f64 / PRICE_SCALE as f64,
                    aux_price: stop_price_i64 as f64 / PRICE_SCALE as f64,
                    lmt_price_offset: parsed.get(&6370).and_then(|s| s.parse().ok()).unwrap_or(f64::MAX),
                    account: parsed.get(&1).cloned().unwrap_or_default(),
                    // Tag 583, the OCA group. A recovered order without it
                    // reads as standing alone, and resubmitting it drops the
                    // cancellation the group exists for.
                    oca_group: parsed.get(&583).cloned().unwrap_or_default(),
                    // Tag 109, who entered the order. Not a client number:
                    // read as one, a report naming a person left the order
                    // under client nought and lost the name as well.
                    submitter: parsed.get(&109).cloned().unwrap_or_default(),
                    ..Default::default()
                },
                order_state: api::OrderState {
                    status: "Submitted".to_string(),
                    ..Default::default()
                },
                last_exec: Default::default(),
            });
                }
            }
        }
    }

    pub(crate) fn poll_executions(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        if self.disconnected { return; }
        let messages = match ccp_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {}
                    Err(e) => {
                        log::error!("CCP connection lost: {e}");
                        self.handle_disconnect(ccp_conn, context, shared, event_tx);
                        return;
                    }
                    Ok(_) => {
                        hb.last_ccp_recv = Instant::now();
                        // RTT sample: interval from the test request
                        // to the first inbound traffic after it. On a quiet
                        // link (the ping use case) that is the echo itself.
                        if let Some((_, sent_at)) = hb.pending_ccp_test.take() {
                            shared.set_ccp_rtt(hb.last_ccp_recv.duration_since(sent_at));
                        }
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let Some(unsigned) = conn.unsign(&raw) else { continue };
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
                            let Some(unsigned) = conn.unsign(&raw) else { continue };
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/fix {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Binary(raw) => {
                            let Some(unsigned) = conn.unsign(&raw) else { continue };
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/bin {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Control(_) => {
                        // 8=1 / 8=X control state — not consumed on the order path.
                        }
                    }
                }
                msgs
            }
        };
        for msg in &messages {
            self.process_ccp_message(msg, ccp_conn, context, shared, event_tx, hb, account_id);
        }
    }

    pub(crate) fn handle_exec_report(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        raw: &[u8],
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        account_id: &str,
    ) {
        // CCP recovery push format A (, captured against live):
        // 35=8 with 150=0/39=0, tag 11 carries `<permId>.0`, the originating
        // orderId is in tag 6121. For these, prefer 6121 as the local key so
        // cancel_order(<prior-session orderId>) finds the right ClOrdID.
        // Format B (paper account, observed live): tag 11 carries the
        // originating orderId directly with `.0` suffix, tags 6119/6121
        // absent — the existing tag-11 split below already gives the right
        // value. The unwrap_or_else fallback handles both.
        let recovery_origin_order_id: Option<u64> = if parsed.get(&150).map(|s| s.as_str()) == Some("0")
            && parsed.get(&39).map(|s| s.as_str()) == Some("0")
            && parsed.contains_key(&6121)
        {
            parsed.get(&6121).and_then(|s| stated_order_id(s))
        } else {
            None
        };

        let wire_name = parsed.get(&11).and_then(|s| {
            let stripped = s.strip_prefix('C').or_else(|| s.strip_prefix('L')).unwrap_or(s);
            stated_order_id(stripped.split('.').next().unwrap_or(stripped))
        });
        // The recovery report states both names, so it is the one chance to
        // learn that they are the same order. Every later report states only
        // the permanent one.
        if let (Some(origin), Some(wire)) = (recovery_origin_order_id, wire_name)
            && origin != wire
        {
            self.remember_the_venues_name_for(wire, origin, context);
        }
        let clord_id = recovery_origin_order_id.unwrap_or_else(|| {
            parsed.get(&11).and_then(|s| {
                // A cancel names the order with a leading C, and a position the
                // broker liquidated with a leading L. Only the first was taken
                // off, so every report on a liquidated position parsed to no
                // order at all and the fill reached nobody: a forced
                // liquidation was the one fill a caller could not
                let stripped = s.strip_prefix('C').or_else(|| s.strip_prefix('L')).unwrap_or(s);
                // Strip versioned suffix (.0, .1, .2) from modify-chained ClOrdIDs
                let base = stripped.split('.').next().unwrap_or(stripped);
                stated_order_id(base)
            }).unwrap_or(0)
        });
        // And read back, so a report naming the order the venue's way reaches
        // the order this session is tracking — but only where nothing is
        // working under that number already. The venue's permanent name for
        // one order can be another order's own number, and an order under that
        // number is the order that number means. Read the other way round, a
        // report for a live order was redirected to an unrelated one.
        //
        // Only for a report that did not state its own: a recovery report
        // names both, so there is nothing to resolve, and resolving it anyway
        // gave one order's recovery to another order that happened to be
        // numbered the venue's permanent name for the first.
        let clord_id = match (recovery_origin_order_id, context.order(clord_id)) {
            (None, None) => self.wire_name_to_order.get(&clord_id).copied().unwrap_or(clord_id),
            _ => clord_id,
        };

        // An order placed through an API carries the number that API gave it,
        // and one typed in by hand carries none. That is the whole of what
        // tells them apart — the venue states no origin beside it — and it is
        // read here, before the report takes any of the paths below, because
        // an order the venue numbered is one whatever became of it. Recorded
        // on the history path alone, an order another API placed and finished
        // while this session watched was left out of the answer to the caller
        // who asked for the API orders.
        if let Some(numbered) = parsed.get(&6121).and_then(|s| stated_order_id(s))
            && numbered != 0
        {
            // Under both names. The number the venue states is the one a
            // finished order is reported by, and the one this session resolved
            // the report to is the one the live path knows it as — recorded
            // under one alone, an order the venue itself had marked was left
            // out of the answer to a caller asking for the API's own.
            shared.orders.note_api_numbered(clord_id);
            shared.orders.note_api_numbered(numbered);
        }

        // A report that arrived because a caller asked what the venue has
        // finished. Every event in such an order's life arrives as its own
        // ordinary report, so through the path below each one is a fill: it
        // registers the contract, opens the order in the book a withdrawal
        // walks, and moves a position. None of that may happen for an order
        // that finished days ago.
        //
        // Filed as history instead and gone no further. Narrowed to an order
        // this session does not hold rather than to everything arriving in the
        // window, so a report on an order this session is working takes its
        // ordinary path whatever else is in flight beside it. The sentinel is
        // let through, because closing the window is its job.
        //
        // And narrowed by what this session put on the wire, not only by what
        // the book still holds. A fill retires an order from the book, so a
        // bust or a correction for it afterwards reads as an order nobody here
        // placed — filed as history, it took back nothing, and the position
        // the correction was undoing stayed where it was.
        // The id every report about one order names it by. The recovery push
        // states an order under the number an API gave it, which is right for
        // an order this session must be able to address — and wrong here,
        // because only some of an order's reports carry it, so its first
        // report and its last would be filed as two different orders.
        let history_id = parsed
            .get(&11)
            .and_then(|stated| {
                let stripped = stated.strip_prefix('C').or_else(|| stated.strip_prefix('L'))
                    .unwrap_or(stated);
                let base = stripped.split('.').next().unwrap_or(stripped);
                stated_order_id(base)
            })
            .unwrap_or(clord_id);
        if self.completed_orders_open
            && clord_id != 0
            && context.order(clord_id).is_none()
            && !shared.orders.the_order_went_out(clord_id)
        {
            let finished = status_of(
                parsed.get(&39).map(String::as_str).unwrap_or(""), clord_id, parsed,
            );
            self.file_finished_order(parsed, raw, history_id, finished, shared);
            return;
        }

        // Recovery insert: a 35=8 with exec type New (150=0) for an order
        // that is NOT in this session's context is a cross-session recovery entry
        // pushed by CCP on session establishment. Insert into context.open_orders
        // so subsequent cancel/modify ACKs at ~line 668 can match via
        // context.order(clord_id) and emit OrderUpdate events to the user.
        //
        // The venue names what it holds, not only what is working: a held
        // order is named the same way with its status as it stands — 39=I
        // captured — and the book is what a cancel-all iterates, so a named
        // order that never reached the book is one the kill switch silently
        // skips. Any non-terminal status is recovered; a terminal one states
        // the order finished and is recorded below, not brought back. A report
        // the venue marks as restating history is the past of an order that
        // finished — the naming at connect arrives unmarked — and must not be
        // recovered as working either.
        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let status = status_of(ord_status, clord_id, parsed);
        let replayed = |tag: u32| {
            parsed.get(&tag).map(|v| v.eq_ignore_ascii_case("Y")).unwrap_or(false)
        };
        let marked_resend = replayed(97) || replayed(43);
        // The sentinel is dropped further down, but this recovery insert runs
        // first — without the guard, a `11='*'` terminator registers a conId
        // and inserts the reserved order id 0 before being "discarded".
        // An order whose state is not known is also hydrated from this echo,
        // not just an absent one. A replace overwrites the tracked record
        // before it goes out, so a replace that failed left the attempted
        // definition in place; the server's account of the order is the
        // authority and replaces it. Anything with a status the engine still
        // believes is left alone.
        // What the engine already holds for this order, where it holds
        // anything. The push states what the broker has and omits the rest —
        // tag 59 among them — so an unstated field keeps what was known rather
        // than taking a default meant for an order this session never saw.
        let prior = context.order(clord_id)
            .filter(|o| o.status == crate::types::OrderStatus::Uncertain)
            .copied();
        let unknown = prior.is_some();
        // An order that already finished this session is not brought back by a
        // frame that arrives behind it. The gateway echoes a working status
        // after a fill, and the tracked record is gone by then — retired when
        // the order finished — so its absence reads as "never seen" and the
        // echo would insert it as live, with none of the fill on it.
        let already_finished = shared.orders.recently_completed(clord_id);
        // The venue naming an order it holds, rather than answering something
        // this session sent. Kept, because the name it states is the
        // authority the recorded one is reconciled against below.
        //
        // Whatever it names it as. Taken only from a report of a new order,
        // an order the last session had replaced was named to this one as
        // replaced, under the version the replace gave it, and never reached
        // the book — so when this session withdrew it, the venue's cancelled
        // report matched nothing and no caller heard the order was gone.
        //
        // And not from a refusal. A drop marks every order uncertain, and a
        // refusal of the revision outstanding at the drop arrives as a
        // non-terminal report on an uncertain order; taken as the venue
        // naming what it holds, it reconciled the revision — dropping the
        // fallback kept against exactly this refusal — before the handler
        // below could put the terms back, so the record kept the refused
        // terms and the name moved to the refused revision.
        let revision_refused = matches!(parsed.get(&378).map(String::as_str), Some("102" | "103"));
        // A trade cancel or correction restates an execution already reported,
        // so it is the one report that may legitimately return a completed
        // order to a working quantity. The record a caller reads already
        // accepts it; the engine's own book did not, and the two then
        // disagreed: the caller was shown an open order, a cancel-all walked
        // the book and did not reach it, and the quantity the correction gave
        // back had no cumulative baseline to be booked against.
        //
        // The venue states it two ways, either one on its own: on the
        // transaction type, 20=1 for a cancelled execution and 20=2 for a
        // corrected one, or on the report type, 150=H and 150=G. Read from the
        // report type alone here while the booking below read both, a bust
        // arriving under the transaction type took its quantity off the account
        // and left the order finished — out of the book a withdrawal walks, out
        // of the names one is sent under, and out of the correction the caller
        // reads.
        let restates_a_trade = matches!(parsed.get(&20).map(String::as_str), Some("1" | "2"))
            || matches!(exec_type, "G" | "H");
        // A restatement the window has already seen is that same one again.
        // The booking further along refuses it on its key, but the recovery
        // here and the correction published after it read the report on its
        // own and took it as a second one: an order this session had already
        // finished came back working, holding the quantity the first copy had
        // given back and short the fills that followed it.
        // Asked on the same key the booking will spend, so that a repeat with
        // no stated id is a repeat here too. Asked only of a report that
        // restates a trade, as before: an ordinary fill is deduped by the
        // booking and has no business reopening anything.
        let restated_twice =
            restates_a_trade && self.already_recorded_exec_id(&execution_key(parsed, clord_id));
        let recovering = !status.is_terminal() && !marked_resend && !revision_refused && !restated_twice
            && clord_id != 0 && (!already_finished || restates_a_trade)
            && (context.order(clord_id).is_none() || unknown);
        if recovering {
            self.recover_order(parsed, clord_id, prior, context, shared);
        }

        // Drop the sentinel/end-of-stream record (ClOrdID="*"/"0"/absent → parses
        // to 0). Real orders are assigned monotonic IDs via next_order_id and
        // never collide with 0. The recovery-push terminator (11='*') lands here.
        // The terminator is a record the venue writes as such: it names the
        // order `*` or `0`, or names none at all. A number this cannot read is
        // not that — read as the terminator, one bad row shut the window and
        // handed the rest of the answer to the live path, where a report
        // stating a fill is a fill.
        let is_the_terminator = match parsed.get(&11) {
            None => true,
            Some(named) => {
                let named = named.trim();
                named.is_empty()
                    || named == "*"
                    || named.split('.').next().unwrap_or(named) == "0"
            }
        };
        if clord_id == 0 && !is_the_terminator {
            log::debug!(
                "ExecReport: a report names an order this cannot read ({:?}); it is dropped, \
                 and it is not the end of anything",
                parsed.get(&11),
            );
            return;
        }
        if clord_id == 0 {
            log::debug!("ExecReport: dropping sentinel record (ClOrdID=0/*) sym={:?} status={:?}",
                parsed.get(&55), parsed.get(&39));
            // The same sentinel ends the answer to what the venue has
            // finished. A caller waiting on that waits on this: the answer is
            // a run of ordinary reports and nothing else says it is over.
            if self.completed_orders_open {
                self.completed_orders_open = false;
                self.completed_orders_deadline = None;
                self.deliver_finished_orders(shared, super::Handover::Final);
                // Only where nobody has been told yet. A caller released on
                // its own wait has had its answer, and a second signal left
                // standing was read by the next caller as the answer to a
                // question the venue had not begun.
                if !self.completed_orders_answered {
                    shared.orders.note_completed_orders_end_on(self.completed_orders_asked_on);
                }
                self.completed_orders_answered = false;
                log::info!("The venue has stated everything it has finished");
            }
            // Everything already working has now been named. The same record
            // shape also carries a mass-status echo that arrives before any
            // order, so this only counts once at least one has come through —
            // otherwise a caller is told the replay is over before it starts.
            // Shortening the sweep is gated the same way, for the same reason:
            // an echo that precedes every order is not the push saying it is
            // finished, and the sweep releasing the hold is what lets the
            // queued cancels and modifies go out — released on the strength
            // of that echo, they name ids the push has not confirmed and the
            // venue refuses, leaving the orders live there.
            if self.hydrated_any {
                shared.orders.set_replay_done();
                // The push said everything it was going to say, so the orders
                // it left out can be judged without waiting out the whole
                // grace. Only ever brought forward: this arm is reached by any
                // report whose order id does not read, not by the terminator
                // alone, so assigning the deadline outright pushed it back
                // every time one arrived and a steady trickle of them meant
                // the sweep never ran.
                if let Some(at) = self.recovery_sweep_at {
                    self.recovery_sweep_at =
                        Some(at.min(Instant::now() + RECOVERY_TERMINATOR_GRACE));
                }
            }
            return;
        }

        // The venue has named this id, and that is true of every report it can
        // send about one — a working order, a fill, a refusal, the replayed
        // history of an order that went before this session opened, a
        // correction of one. Said here rather than where a record is kept: the
        // id a partly filled and then cancelled order spent is named only by
        // its replayed history, which is exactly the record this client does
        // not keep, so the next session counted past the working set and
        // walked straight onto an id the venue would refuse.
        shared.orders.note_the_venue_named(clord_id);

        // Record the ClOrdID exactly as the server reports it so subsequent
        // cancel/modify can echo back the same string. Skip cancel-ack frames
        // (tag 11 starts with 'C' there) — those carry the cancel request's
        // own id, not the original order's. The same holds for the 'L' the
        // venue puts on a report for a position it liquidated: the prefix is
        // taken off to find the order, and recording it back would make the
        // next cancel name a string the venue does not know.
        // The prefixes the id parser strips are not part of the caller's
        // number, and this is the number a later cancel names as the original.
        // Recorded with the liquidation prefix still on it, that cancel named
        // an id the venue does not know: the venue refused it, the refusal
        // retired the order here, and the order went on working there —
        // absent from the open orders, out of reach of a withdrawal of
        // everything, its fills arriving against nothing.
        //
        // Forward only. Reports do not have to arrive in the order the
        // revisions were sent, and one for an earlier revision arriving behind
        // a later one moved this back to a name the venue had already
        // superseded — so the next cancel named that superseded revision as
        // the original, and the venue answered that it knew no such order.
        //
        // Except where the venue is naming what it holds. That report is not
        // an answer that can arrive out of turn: it is the venue's account of
        // the order, and it stands whichever revision it names. A replace
        // whose answer the dropped connection took with it left the attempt's
        // own name recorded, the naming at the next connect said the earlier
        // revision was still working, and the forward-only rule refused to put
        // it back — so every later cancel named a revision the venue does not
        // know and the order went on working out of reach of a withdrawal.
        if let Some(raw_clord) = parsed.get(&11)
            && !raw_clord.starts_with('C')
            && !raw_clord.starts_with('L')
            && raw_clord != "*"
        {
            let reported = revision_of(raw_clord);
            // Not for an order that has finished. Retiring one drops the two
            // name maps precisely because they only serve orders that can
            // still be cancelled or replaced — and then a late working echo,
            // or a replayed partial fill for an order this session never
            // tracked, wrote the entry straight back with nothing left to
            // remove it. A process left running held one per order it had ever
            // seen. A correction is the exception, because it puts the order
            // back and the name is how a withdrawal reaches it.
            if (!already_finished || restates_a_trade)
                && (recovering
                    || context.last_clord.get(&clord_id).is_none_or(|held| {
                        reported >= revision_of(held)
                    }))
            {
                context.last_clord.insert(clord_id, raw_clord.clone());
            }
            if recovering {
                context.reconcile_recovered_revision(clord_id, reported);
            }
        }

        // What-If response: tag 6091=1 with margin data (tag 6092+).
        // The gateway emits a not-ready ack frame whose margin fields carry the
        // literal string "n/a" (parse fails), then a data frame with numbers.
        // Discriminate on parse-success, NOT positivity: a margin-reducing
        // preview (closing a position, cash-account sell) legitimately resolves
        // to init_margin_after == 0, and that arrives as a numeric "0"
        // which must be delivered. Guarding on `> 0.0` silently dropped those
        // and left the caller's pending what-if to time out.
        // The not-ready ack is not always emitted — close/reject previews send a
        // single data frame — so accept the first data frame with no assumption
        // that an ack precedes it. A frame is the real preview when ANY of the
        // six margin fields (6826/6827/6828 before, 6092/6093/6094 after)
        // parses as a finite number: each field is set when it parses, unset on
        // nan or unparseable, and the frame is real when any field is set. The
        // ack carries "n/a" in all six, so it never matches. Captured
        // byte-level in.
        if parsed.get(&6091).map(|s| s.as_str()) == Some("1")
            && take_what_if(parsed, clord_id, context, shared, event_tx)
        {
            return;
        }

        let exec_id = parsed.get(&17).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        // Tag 32, the quantity of this print, held fixed-point. A fractional
        // order fills in fractions, so the decimal is carried rather than
        // rounded: read as an integer, `32=0.5` is a fill of nothing and the
        // position never moves.
        let last_shares = match parsed.get(&32) {
            None => 0,
            Some(stated) => parse_qty_tag(Some(stated)).unwrap_or_else(|| {
                log::error!(
                    "order {clord_id} states an unreadable fill quantity {stated} — nothing is booked",
                );
                0
            }),
        };
        // Absent is not zero. Without 151 the caller was told nothing was left
        // on an order that was still working; the terminal falls back to the
        // order quantity less what has filled, and so does this.
        let leaves_qty = parse_qty_tag(parsed.get(&151)).unwrap_or_else(|| {
            let ordered = parse_qty_tag(parsed.get(&38)).unwrap_or(0);
            let done = parse_qty_tag(parsed.get(&14)).unwrap_or(0);
            ordered.saturating_sub(done).max(0)
        });
        // 14 CumQty and 6 AvgPx describe the order as a whole; 32 and 31
        // describe this print alone. The gateway sends all four on every
        // execution report.
        //
        // When the cumulative quantity is absent, the print alone is not a
        // substitute: on the second fill of an order it is smaller than what
        // was already reported, so `filled` would go backwards. Add the print
        // to what the order has already accumulated instead. The average price
        // is not reconstructible that way, so it falls back to the print — and
        // a negative average is a real value for a spread, so only an absent
        // or unparseable tag falls back at all.
        //
        // A stated zero is a value, not an absence: a bust of everything the
        // order held states one. Filtering the figure on being positive read
        // it as missing, and the fallback added the bust's own print to what
        // was already booked — the caller was told the order had doubled its
        // quantity filled while everything was still remaining.
        let order_cum_qty = parse_qty_tag(parsed.get(&14))
            .unwrap_or_else(|| {
                context.order(clord_id)
                    .map_or(last_shares, |o| o.filled.saturating_add(last_shares))
            });
        let order_avg_px = parsed.get(&6)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(last_px);

        if ord_status == "8" {
            // The venue says why it refused an order, and that was written to a
            // log where no caller could read it, leaving the caller with the
            // order not working and no reason to act on.
            let reason = stated_reason(parsed);
            log::warn!("ExecReport REJECTED: clord={clord_id} reason='{reason}'");
            if !reason.is_empty() {
                shared.orders.push_order_inactive(clord_id, ORDER_REJECTED_ERROR_CODE, reason);
            }
        } else {
            log::info!("ExecReport: 39={} 150={} 11={} 58={} 103={}",
                ord_status, exec_type, clord_id,
                parsed.get(&58).map(|s| s.as_str()).unwrap_or(""),
                parsed.get(&103).map(|s| s.as_str()).unwrap_or(""));
        }

        // A replace is acknowledged as 39=5, reached through 39=6 first: a
        // modify runs PendingCancel then Replaced. Confirmed live.
        // A pending cancel does not outrank the working states, so the
        // acknowledgement applies the ordinary way. An order already working
        // when the modify is accepted changes no status, and the caller is still
        // told the change was made.
        //
        // Applied under the guard, not forced past it: an acknowledgement
        // arriving behind a fill must not move a finished order back to
        // working.
        // A report can carry the reason it restates the order, and two of those
        // reasons are refusals: a revision the venue will not make and a cancel
        // it will not make arrive on the same message shape as a successful one.
        // Read as an acknowledgement, a refused revision left
        // the caller believing an order had been changed that had not been.
        // Which revision this report answers. The venue takes a second
        // revision before it has answered the first, so an acknowledgement or
        // a refusal belongs to the one it names and not simply to the order.
        let reported_revision = parsed.get(&11)
            .map(|c| revision_of(c))
            .unwrap_or_else(|| *context.modify_versions.get(&clord_id).unwrap_or(&0));
        let restatement_reason = parsed.get(&378).map(|s| s.as_str()).unwrap_or("");
        let is_replace_ack = ord_status == "5" && !revision_refused;
        if is_replace_ack {
            // The venue holds what the attempt stated, so the fallback kept
            // against a refusal is spent. A stale refusal arriving behind the
            // acceptance must not put the old terms back over it.
            let ours = context.pre_replace.remove(&(clord_id, reported_revision)).is_some();
            // And every revision below it: the venue holds the accepted terms,
            // so a refusal of an earlier revision arriving behind the
            // acceptance has nothing left to put back. Spent one at a time, it
            // put the terms from before the earlier revision over the accepted
            // ones and moved the name back to a revision the venue had
            // superseded.
            context.pre_replace.retain(|(id, ver), _| *id != clord_id || *ver > reported_revision);
            // And said to the surfaces, which keep a copy of their own. Only
            // where the change was this session's: an acknowledgement of one
            // made elsewhere, or replayed at connect, spends nothing here and
            // must spend nothing there. They
            // read the acceptance off a status before this: a fill landing
            // between the attempt and the answer took the more advanced
            // status, the acknowledgement behind it was dropped as stale, and
            // the copy outlived the replacement the venue had taken — so the
            // next refusal put back terms from before it.
            //
            // And only where nothing later is still in flight. What is left in
            // the map after the retain above is exactly that. The surfaces
            // keep one fallback for an order where this side keeps one per
            // revision, so an acceptance spending it left the revision still
            // outstanding with nothing to fall back to: refused in its turn,
            // the record went on stating the terms the venue had just
            // turned down, and every later cancel and replace restated from
            // those.
            if ours && !context.pre_replace.keys().any(|(id, _)| *id == clord_id) {
                shared.orders.note_replacement_taken(clord_id);
            }
        }
        // Whether the caller had withdrawn this order before the refusal put
        // its terms back. Read here, because the restore below writes the
        // snapshot's status into the book and every later read sees that one.
        let withdrawn_before_the_refusal = revision_refused
            && context.order(clord_id)
                .is_some_and(|o| o.status == crate::types::OrderStatus::PendingCancel);
        if revision_refused {
            // A revision the venue will not make leaves the order on the terms
            // it had, and the record must follow: it took the attempt ahead of
            // the answer. The refusal of a cancellation is not touched — it
            // changed no terms, and the revision it may be waiting on has its
            // own answer coming.
            let mut answered_a_live_revision = false;
            if restatement_reason == "102" {
                answered_a_live_revision =
                    context.pre_replace.contains_key(&(clord_id, reported_revision));
                context.restore_pre_replace(clord_id, reported_revision);
            }
            // The order stands as it was, so it has no new status to report —
            // but the caller asked for a change and has to learn it did not
            // happen. Reported the way a refused order is, on the channel a
            // caller already watches, rather than only to a log.
            let reason = stated_reason(parsed);
            log::warn!(
                "Order {clord_id}: the venue refused the request (378={restatement_reason}) — \
                 the order stands as it was: {reason}",
            );
            let told = if reason.is_empty() {
                "the venue refused the change and the order stands as it was".to_string()
            } else {
                reason
            };
            shared.orders.push_order_inactive(clord_id, ORDER_INACTIVE_ERROR_CODE, told);
            // And on the channel a refusal already travels on, so the record
            // the surfaces read goes back with the engine's. Said only in the
            // message above, the surfaces kept the terms of an attempt the
            // venue had turned down: the caller was told the change did not
            // happen and their own book went on stating that it had.
            // Where the order stands, as the engine's own book has it after
            // the restore above — not guessed from what it usually is. An
            // order the caller had already withdrawn stands at pending cancel,
            // and reporting it as working said the withdrawal had come undone.
            let stood = context.order(clord_id).map(|order| (order.instrument, order.status));
            if let Some((instrument, status)) = stood {
                let reject = crate::types::CancelReject {
                    order_id: clord_id,
                    instrument,
                    // 102 refuses the revision, 103 the cancellation.
                    reject_type: if restatement_reason == "102" { 2 } else { 1 },
                    // The report carries no tag 102, and this says as much.
                    reason_code: -1,
                    still_working: Some(status),
                    // The revision it names is the one the restore above acted
                    // on, or none was outstanding and there is nothing to put
                    // back either way.
                    answers_a_live_change: answered_a_live_revision,
                    timestamp_ns: context.now_ns(),
                };
                shared.orders.push_cancel_reject(reject);
                emit(event_tx, Event::CancelReject(reject));
            }
        }
        // The gateway marks a report that restates history: 97=Y is PossResend
        // and 43=Y is PossDupFlag. Neither was read anywhere, and the only
        // thing standing between a replayed execution and a second booking was
        // the ExecID window — which a fresh process does not have, because it
        // has never seen the ID. At session start the venue replays
        // recent executions, so a restart with open partially-filled orders
        // emitted a fill for something that happened before it started.
        //
        // Read before the status is applied, because it decides that too: a
        // replay does not move an order back out of an in-flight cancel.
        let is_resend = ["Y", "y"].contains(&parsed.get(&97).map(|v| v.as_str()).unwrap_or(""))
            || ["Y", "y"].contains(&parsed.get(&43).map(|v| v.as_str()).unwrap_or(""));

        // The guard's verdict doubles as the change flag: a frame it rejects
        // surfaces no order_status. A refusal states no new status; the order
        // stands on the terms it has. Any execution on the report is still read
        // below.
        // A refusal of the CHANGE says nothing about a cancel sent over it, and
        // the status it carries is the order's terms as they stand — which the
        // guard reads as the order working again, because that is what it means
        // everywhere else. So the report resumed a withdrawal the venue still
        // owes a verdict on, and the flag that suppresses the announcement left
        // the book saying it too.
        //
        // The same rule the cancel-reject path keeps. That one is `35=9`; this
        // is the execution report carrying the refusal, and it was the other
        // half of the same defect.
        let applied = if withdrawn_before_the_refusal && restatement_reason == "102" {
            false
        } else {
            context.update_order_status(clord_id, status, is_resend)
        };
        // An accepted modify is announced even where it changed no status: an
        // order already working when the change lands stays working. Only where
        // the order is in the state being announced, so a status the guard
        // rejected is not reported over it.
        // Compared as the caller is told them, not as this engine holds them.
        // A partly filled working order is reported as submitted — the two
        // quantities carry the distinction, which is why the vocabulary
        // collapses them — so an acknowledgement stating submitted, on a book
        // holding partly filled, is the same status stated twice. Compared as
        // enums it was two, and an accepted replace on an order that had filled
        // before the answer landed announced nothing at all.
        let says_the_same = |held: crate::types::OrderStatus| {
            crate::types::order_status::order_status_str(held)
                == crate::types::order_status::order_status_str(status)
        };
        let acknowledged_in_place =
            is_replace_ack && context.order(clord_id).is_some_and(|o| says_the_same(o.status));
        let status_changed = !revision_refused && (applied || acknowledged_in_place);

        // A report can also undo or restate an execution rather than announce a
        // new one: a busted trade and a corrected one both arrive as executions,
        // and adding their quantity booked a fill the account no longer has.
        // The cumulative figure is the truth on those, which is the same
        // arithmetic a replayed execution needs: both restate what the account
        // holds and may restate it downwards, which a replay never does. Which
        // reports those are was settled above, on the two tags the venue states
        // it on, and is read here rather than worked out a second time — the
        // two answers drifted apart, and a bust one recognised and the other
        // did not came off the account without reopening the order it was on.
        let is_resend = is_resend || restates_a_trade;

        // CumQty — the order's cumulative filled quantity as of this report.
        // Held in the same fixed-point unit as what the order has already
        // booked, because a resend books the difference between the two.
        let report_cum_qty = parse_qty_tag(parsed.get(&14));

        // Dedup key. An execution with no ExecID skipped the window entirely,
        // so a replayed copy booked a second time — and an absent tag 17 is the
        // shape a replay takes, which is precisely when the window matters. Falling
        // back to the fields that identify an execution
        // dedups it on its content instead of trusting it.
        //
        // CumQty is what separates two otherwise identical slices: it advances
        // with every execution on the order, including across a replacement
        // that raised the total, where LastShares, price, LeavesQty and the
        // timestamp tick can all repeat.
        let dedup_key = execution_key(parsed, clord_id);

        // A trade cancel and a trade correction carry a quantity and restate
        // what the account holds, exactly as a fill does; the reconciliation
        // below works from the cumulative figure and moves it either way.
        let is_execution = matches!(exec_type, "F" | "1" | "2" | "G" | "H") && last_shares > 0;
        let filled = if is_execution {
            self.book_fill(
                parsed, clord_id, &dedup_key, is_resend, restates_a_trade, last_px,
                last_shares, report_cum_qty, leaves_qty, order_cum_qty,
                order_avg_px, context, shared,
            )
        } else {
            None
        };

        // A report that fills an order states its new status on the same
        // report, and suppressing the status because the fill was on it meant
        // the one transition that matters most was the one never announced: a
        // caller watching order status was told about the execution and left
        // believing the order was still working. The two are different
        // questions — what traded, and where the order stands — and a report
        // that answers both is not a reason to drop one.
        // Held until the caches below are written. A caller acting on the
        // notification queries this session for the order it names, so the
        // record must exist before the announcement.
        let mut announce: Option<crate::types::OrderUpdate> = None;
        if status_changed
            && let Some(order) = context.order(clord_id).copied() {
                let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                // Tag 583 is the link id this engine sends the OCA group on, not
                // a parent order. Hashing it produced a stable non-zero value
                // shared by every order in a group, none of which has a parent,
                // and nothing distinguished it from a real link.
                //
                // 6107 is not the way to recover one either, though an order
                // *sends* its parent there: the tag is message-scoped, and the
                // vendor's own audit renderer names the inbound one
                // ParentClientId. That is what the shared non-zero value above
                // was — one client id echoed to every order in the account.
                // Reading it back as a parent gives each of them a parent that
                // does not exist. Nothing on this report carries a parent order
                // id, so report none.
                let parent_id: i64 = 0;
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty: qty_to_f64(order.filled),
                    remaining_qty: qty_to_f64(leaves_qty),
                    avg_price: crate::types::price_from_f64(order_avg_px),
                    perm_id,
                    parent_id,
                    timestamp_ns: context.now_ns(),
                };
                announce = Some(update);

                // A parked (39=I) order carries its reason on the same tags
                // 58/103 as a reject, but OrderState.completedStatus stays
                // empty for Inactive — it is not completed and may
                // reactivate, so there is no snapshot field to carry the
                // reason on. Route it through the same error() path a
                // cancel/modify reject already uses instead.
                if status == crate::types::OrderStatus::Inactive {
                    let reason = stated_reason(parsed);
                    if !reason.is_empty() {
                        shared.orders.push_order_inactive(clord_id, ORDER_INACTIVE_ERROR_CODE, reason);
                    }
                }
            }

        // The report the fill below was booked off, kept for it. Read back off
        // the order afterwards instead, a pass carrying two prints of one order
        // reported both under the later print's execution.
        let booked_off: Option<RichOrderInfo>;

        // The state filed under an order is the state the engine holds for it.
        //
        // A report the status guard refused states some other one, and there
        // are several ways to arrive at that: a rejection that answers a
        // replace rather than the cancel it raced, and history replayed behind
        // a cancel that the order has since moved past. Filed anyway, the
        // engine went on working the order — the guard says so — while what a
        // caller asking for its working orders read carried the refused
        // report's status and its reason.
        //
        // Read against the order as it stands after the guard, rather than
        // against the one shape this took first: the cache is answering what
        // the order is, and that is the same question the guard just settled.
        // An order this session does not hold is not one the guard has an
        // opinion on, and is left to the two rules below it.
        let states_the_order =
            context.order(clord_id).is_none_or(|o| says_the_same(o.status));

        // Enrich order/contract caches block
        {
            let account = parsed.get(&1).cloned().unwrap_or_default();
            let symbol = parsed.get(&55).cloned().unwrap_or_default();
            // Where the order is working. The report states it on 207 when it
            // says so at all, and on 6004 as the destination it was routed to;
            // failing both, this client knows where it sent the order and says
            // that. An empty exchange on a completed order is a contract a
            // caller cannot re-place, and the reference client never returns one.
            let exchange = parsed.get(&207).cloned()
                .filter(|e| !e.is_empty())
                .or_else(|| parsed.get(&6004).cloned().filter(|e| !e.is_empty()))
                .or_else(|| {
                    context.order(clord_id).copied()
                        .map(|o| context.market.order_routing(o.instrument).1)
                        .filter(|e| !e.is_empty())
                })
                .unwrap_or_default();
            let sec_type = parsed.get(&167).cloned().unwrap_or_default();
            let currency = parsed.get(&15).cloned().unwrap_or_default();
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let local_symbol = parsed.get(&6035).cloned().unwrap_or_default();
            let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
            let total_qty: f64 = parsed.get(&38).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let ord_type_tag = parsed.get(&40).map(|s| s.as_str()).unwrap_or("");
            let limit_price: f64 = parsed.get(&44).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let tif_tag = parsed.get(&59).map(|s| s.as_str()).unwrap_or("");
            let stop_px: f64 = parsed.get(&99).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let outside_rth = parsed.get(&6433).map(|s| s == "1").unwrap_or(false);
            let clearing_intent = parsed.get(&6419).cloned().unwrap_or_default();
            let auto_cancel_date = parsed.get(&6596).cloned().unwrap_or_default();
            let exec_exchange = parsed.get(&30).cloned().unwrap_or_default();
            let transact_time = parsed.get(&60).cloned().unwrap_or_default();
            let avg_px: f64 = parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            // Absent is not zero. This value is written into a row that
            // persists, so a later report that omits the tag — a pending
            // cancel, say — would otherwise wipe a real filled quantity back
            // to nothing, which is the symptom this is correcting.
            let cum_qty: Option<f64> = parsed.get(&14).and_then(|s| s.parse().ok());
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

            let order_type_str = crate::types::ord_type_api_name(
                ord_type_tag,
                parsed.get(&18).map(String::as_str).unwrap_or_default(),
            );

            // Unknown maps to empty, which is what `decode_tif` means by it and
            // what makes the fallback below reachable. A catch-all of `DAY`
            // reported a perfectly ordinary value for a code this does not know
            // and for an absent tag alike, so a caller reconciling its own
            // orders saw a plausible answer that disagreed with what it sent
            // and nothing said so.
            //
            // The sibling above passes the raw tag through instead; that works
            // there because an absent tag leaves it empty, while any non-empty
            // TIF code would suppress the fallback that knows the real answer.
            let tif_str = tif_api_name(tif_tag);

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

            let status_str = crate::types::order_status::order_status_str(status);

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

            let (fb_tif, fb_ord_type) = if let Some(ctx_order) = context.order(clord_id) {
                let t = decode_tif(ctx_order.tif);
                let o = crate::types::ord_type_api_name(
                    crate::types::ord_type_fix_str(ctx_order.ord_type),
                    crate::types::ord_type_instruction(ctx_order.ord_type),
                );
                (t, o)
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
            // Tag 8339 is its own field, not derived from the algo strategy on
            // tag 847; a report that does not carry it states nothing about it.
            let use_price_mgmt_algo = parsed.get(&8339)
                .map(|v| i32::from(flag(v)));
            let trail_stop_price: f64 = parsed.get(&6117)
                .and_then(|s| s.parse().ok())
                .unwrap_or(f64::MAX);

            let mut order = api::Order {
                order_id: clord_id as i64,
                model_code: stated_model(parsed),
                // What the venue says the order waits for. Read from the
                // report rather than left empty, so an order read back and
                // placed again waits for what it waited for the first time
                // instead of going live at once.
                conditions: decode_conditions(raw),
                conditions_cancel_order: parsed.get(&6128).map(|v| v == "1").unwrap_or(false),
                conditions_ignore_rth: parsed.get(&6151).map(|v| v == "1").unwrap_or(false),
                action: action.to_string(),
                total_quantity: total_qty,
                order_type: if order_type_str.is_empty() { fb_ord_type.to_string() } else { order_type_str.to_string() },
                lmt_price: limit_price,
                aux_price: stop_px,
                // A trailing stop limit's limit offset, which the venue states
                // on its own tag and which was read from nowhere.
                lmt_price_offset: parsed.get(&6370).and_then(|s| s.parse().ok()).unwrap_or(f64::MAX),
                tif: if tif_str.is_empty() { fb_tif.to_string() } else { tif_str.to_string() },
                account: if account.is_empty() { account_id.to_string() } else { account.clone() },
                perm_id,
                // Tag 14 (CumQty), not tag 151 (LeavesQty). The two are
                // complements, so reporting the remainder as the filled amount
                // makes a completed order read as entirely unfilled.
                filled_quantity: cum_qty.unwrap_or_else(|| {
                    shared.orders.get_order_info(clord_id)
                        .map_or(0.0, |info| info.order.filled_quantity)
                }),
                outside_rth,
                clearing_intent,
                auto_cancel_date,
                // Tag 109, who entered the order, which the report states and
                // the account does not: an account holds many people.
                submitter: parsed.get(&109).cloned().unwrap_or_default(),
                oca_type,
                use_price_mgmt_algo,
                trail_stop_price,
                algo_strategy,
                // The report restates the order, and a caller asking what its
                // orders are is answered from it. Everything below arrived on
                // every report and was read from none of them, so an order came
                // back naming neither the reference the caller gave it, nor the
                // client that placed it, nor how it allocates.
                // The group that cancels together. The recovery record reads
                // it and so must this one: an ordinary report that omits it
                // replaces the cached row with one saying the order stands
                // alone, and the order placed from that row carries none of
                // the cancellation the group exists for.
                oca_group: parsed.get(&583).cloned().unwrap_or_default(),
                order_ref: parsed.get(&6010).cloned().unwrap_or_default(),
                rule80a: parsed.get(&47).cloned().unwrap_or_default(),
                good_till_date: parsed.get(&432).cloned().unwrap_or_default(),
                // How an advisor's order is divided, which is the whole of what
                // an advisor's order is.
                fa_group: parsed.get(&6160).cloned().unwrap_or_default(),
                fa_method: parsed.get(&6159).cloned().unwrap_or_default(),
                fa_percentage: parsed.get(&6164).cloned().unwrap_or_default(),
                ..Default::default()
            };
            // And everything else the report says about it.
            read_stated_attributes(&mut order, parsed);

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
                    // An empty reason still marks a refusal: Inactive with
                    // no completed status means the venue is holding it.
                    parsed.get(&58).filter(|s| !s.is_empty())
                        .cloned().unwrap_or_else(|| "Rejected".to_string())
                }
                _ => String::new(),
            };

            // What the fill cost is not on this report; it arrives on a
            // record of its own and is reported from there. Read off a tag the
            // report does not carry, every order stated that it cost exactly
            // nothing, which a program written against the reference records
            // as a cost because it is not the unset value.
            let order_state = api::OrderState {
                status: status_str.to_string(),
                completed_time,
                completed_status,
                // `completed_status` is the reject text alone, which is what
                // ibapi's field means. The reason code lives here, where a
                // caller telling a venue's refusal from a bad request can
                // reach it.
                reject_reason: if status == crate::types::OrderStatus::Rejected {
                    stated_reason(parsed)
                } else {
                    String::new()
                },
                ..Default::default()
            };

            let last_exec = api::Execution {
                model_code: stated_model(parsed),
                // What the report stated that nothing above names. A report
                // carries far more than any one client reads, and what is not
                // read is kept rather than dropped.
                unnamed_fields: unnamed_execution_fields(raw),
                exec_id: exec_id.to_string(),
                time: transact_time,
                acct_number: account,
                exchange: exec_exchange,
                // The venue's word for the side, read off the report as the
                // action above is. Read off the order this session tracks, an
                // execution restated for an order it never tracked — one that
                // finished before a restart — stated no side at all.
                side: match action {
                    "BUY" => "BOT",
                    "SELL" | "SSHORT" => "SLD",
                    _ => "",
                }.to_string(),
                shares: qty_to_f64(last_shares),
                price: last_px,
                order_id: clord_id as i64,
                // The order's permanent number and the client that placed it,
                // as the report states them. Left at zero, a restated execution
                // named no client, and a request filtered by client matched
                // none of them.
                perm_id,
                client_id: i64::from(order.client_id),
                // Who entered it, which is the order's own and is stated on
                // the report the fill came on.
                submitter: order.submitter.clone(),
                // Read off the report, which restates it every time, rather
                // than looked up against the order this client remembers: a
                // fill on an order placed in another session is still
                // labelled, and this client remembers no such order.
                order_ref: parsed.get(&6010).cloned().unwrap_or_default(),
                // The execution record describes this report, so an absent
                // cumulative is zero here rather than the cached total.
                cum_qty: cum_qty.unwrap_or(0.0),
                avg_price: avg_px,
                last_liquidity: last_liq,
                // Not a field of its own: the broker says it liquidated the
                // position by naming the order with a leading L rather than by
                // setting anything. Read as a flag it was never set at all, and
                // a caller could not tell a liquidation from any other fill.
                liquidation: i32::from(parsed.get(&11).is_some_and(|s| s.starts_with('L'))),
                // What the instrument's economic value is reckoned by, where it
                // has one, and what the reckoning is multiplied by.
                ev_rule: parsed.get(&6858).cloned().unwrap_or_default(),
                // The multiplier is the tag beside the rule, and the venue
                // states it as a number. It was read off 6892, which the venue
                // states as text — so it parsed to nothing and every fill
                // carried a multiplier of zero. A contract whose value follows
                // something other than its own price is then valued at nothing.
                ev_multiplier: parsed.get(&6859)
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0.0),
                // The price on this report may yet be revised.
                pending_price_revision: parsed.get(&8497)
                    .is_some_and(|v| flag(v)),
            };

            if con_id != 0 {
                // An execution report states a subset of a definition: it names
                // the contract, not its long name, its trading class or the
                // venues it may trade on. Caching it whole replaced a definition
                // already fetched with a poorer one, leaving a later reader a
                // contract missing fields. Fill, do not replace.
                let merged = match shared.reference.get_contract(con_id) {
                    Some(mut known) => {
                        if !contract.symbol.is_empty() { known.symbol = contract.symbol.clone(); }
                        if !contract.sec_type.is_empty() { known.sec_type = contract.sec_type.clone(); }
                        if !contract.exchange.is_empty() { known.exchange = contract.exchange.clone(); }
                        if !contract.currency.is_empty() { known.currency = contract.currency.clone(); }
                        if !contract.local_symbol.is_empty() {
                            known.local_symbol = contract.local_symbol.clone();
                        }
                        known
                    }
                    None => contract.clone(),
                };
                shared.reference.cache_contract(con_id, merged);
            }

            // An execution the venue restated and nothing above booked: the
            // quantity is already held, or the order was never this session's.
            // No fill, and announced as none — but one of the day's executions,
            // which a caller asking for those is owed.
            if is_resend && is_execution && filled.is_none() {
                shared.orders.push_restated_execution(contract.clone(), last_exec.clone());
            }

            // A trade cancel or a trade correction restates an execution
            // already reported, so it may legitimately return a completed order
            // to a working quantity. Every other report that would do that is a
            // replay.
            let info = RichOrderInfo { contract, order, order_state, last_exec };
            booked_off = filled.is_some().then(|| info.clone());
            // A replay is not a correction, whatever it restates. The venue
            // states the kind on nearly every report it sends again at connect,
            // so a widened reading that does not say so routes the whole replay
            // burst through here: an order that finished this session comes
            // back as a correction, is listed as working by a book a cancel-all
            // walks and can never reach, and its completion is purged from what
            // the caller reads. The recovery test beside this one keeps the
            // same condition for the same reason.
            if restates_a_trade && !marked_resend && !restated_twice {
                shared.orders.push_order_correction(clord_id, info);
            } else {
                // A late duplicate of an earlier partial must not rewrite a
                // completed order back to open. The cache is what
                // `req_open_orders` reads, so a caller polling between the two
                // frames would see a finished order listed as working.
                // Inactive is not terminal in general — the venue still holds
                // such an order and it can return to working, so a cancel-all
                // reaches one — but one that also carries a completed status
                // is a refusal, and a refusal is finished.
                let already_terminal = shared.orders.get_order_info(clord_id).is_some_and(|prev| {
                    crate::types::order_status::is_terminal_status(
                        &prev.order_state.status,
                        &prev.order_state.completed_status,
                    )
                });
                let finishes = matches!(
                    status,
                    crate::types::OrderStatus::Filled
                        | crate::types::OrderStatus::Cancelled
                        | crate::types::OrderStatus::Rejected
                );
                // A report the venue marks as restating history, on an order
                // this session does not hold as working, is the past of an
                // order that finished. The venue names what is working once
                // at connect, unmarked; the marked reports are the executions
                // behind everything else, and the cancel or expiry that ended
                // those orders is not an execution and is never replayed.
                // Cached as the order's state, a partial fill from days ago —
                // an immediate-or-cancel that filled part and lapsed, a day
                // order that expired — listed an order the venue was not
                // working, and a caller asking what it had on was told so. A
                // restated report that finishes an order is still filed, for
                // the caller asking what completed.
                let history_of_a_finished_order =
                    is_resend && !finishes && context.order(clord_id).is_none();
                if (!already_terminal || finishes)
                    && !history_of_a_finished_order
                    && states_the_order
                {
                    shared.orders.push_order_info(clord_id, info);
                }
            }
        }

        if matches!(status,
            crate::types::OrderStatus::Filled |
            crate::types::OrderStatus::Cancelled |
            crate::types::OrderStatus::Rejected
        ) {
            let tracked = context.order(clord_id).copied();
            // Whether this report is how the order FINISHED, which is not the
            // question the cache above asks — that one is what the order IS.
            //
            // A rejection the guard left standing is the venue's answer to the
            // request that raced the cancel, not an answer to the cancel: the
            // venue still owes the cancel its own verdict, and retiring here
            // would leave that verdict nothing to announce against when it
            // lands. The order stays in the book until a verdict the guard
            // accepts finishes it.
            //
            // As the guard beside the status: only where a replace is
            // outstanding does a rejection behind a cancel answer something
            // other than the cancel itself.
            let cancel_still_owed = status == crate::types::OrderStatus::Rejected
                && tracked.is_some_and(|o| o.status == crate::types::OrderStatus::PendingCancel)
                && context.replace_is_outstanding(clord_id);
            // Filed under the guard that retires it, and for the same reason:
            // an answer that is not this order's outcome is not how it
            // finished either. Filed ahead of the guard, the rejection that
            // raced the cancel was recorded as the order's completion — and
            // that record stands, so when the cancel's own verdict arrived it
            // was refused as a completion already filed, leaving the caller
            // told Cancelled on the status and Rejected in the completed
            // orders. For as long as the memory lasts it also drops every
            // status behind it, so an order that goes back to working after a
            // refused cancel stops reporting at all.
            if !cancel_still_owed {
                // Recorded whether or not the order was being tracked. A market
                // order can finish before its acknowledgement has been handled,
                // so requiring a tracked record meant the fastest orders — the
                // ones that fill immediately — left no memory of having
                // finished, and the working status echoed behind the fill had
                // nothing to be refused by.
                shared.orders.push_completed_order(CompletedOrder {
                    order_id: clord_id,
                    instrument: tracked.map_or(0, |o| o.instrument),
                    status,
                    filled_qty: tracked.map_or(0, |o| o.filled),
                    timestamp_ns: context.now_ns(),
                });
                context.retire_order(clord_id);
            }
        }

        // Announced after everything this report changed is written. A caller
        // acts on a notification the moment it arrives, and each of those
        // actions reads a record this report writes.
        if let Some(fill) = filled {
            match booked_off {
                Some(report) => shared.orders.push_fill_reported(fill, report),
                None => shared.orders.push_fill(fill),
            }
            emit(event_tx, Event::Fill(fill));
        }
        if let Some(update) = announce {
            shared.orders.push_order_update(update);
            emit(event_tx, Event::OrderUpdate(update));
        }
    }

    pub(crate) fn handle_cancel_reject(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        // A replace records its new name before the answer arrives, so the
        // recovered name on tag 41 may no longer be in last_clord. Tag 11
        // names the local order and revision kept against that answer.
        let replacement = parsed.get(&11).filter(|_| reject_type == 2).and_then(|stated| {
            let (base, _) = stated.split_once('.')?;
            let id = stated_order_id(base)?;
            context.pre_replace.contains_key(&(id, revision_of(stated))).then_some(id)
        });
        // Tag 41 is the name this client put on the cancel, echoed back. It is
        // resolved through the record it was taken from rather than read as
        // digits: the name an order carries is not always its number. An order
        // recovered from a prior session is keyed here by the id the venue
        // stated beside it, while the name it answers to is built from the
        // permanent one — so the cancel went out naming that, and reading the
        // digits back pointed the refusal at an order whose number happened to
        // match the permanent id. The order the caller cancelled stayed
        // pending, and some other order was retired in its place.
        let orig_clord = replacement.or_else(|| parsed.get(&41).and_then(|stated| {
            context
                .last_clord
                .iter()
                .find(|(_, name)| *name == stated)
                .map(|(id, _)| *id)
                .or_else(|| parsed.get(&11).and_then(|sent| {
                    // The cancel's own name, which this client built from the
                    // local order id. Read ahead of tag 41's digits because a
                    // recovered order answers to the permanent id the venue
                    // stated beside it: once the first refusal retires it and
                    // drops its record, those digits name whichever live order
                    // happens to carry that number, and the second refusal
                    // retired it in place of the one that was cancelled.
                    let stripped = sent.strip_prefix('C').or_else(|| sent.strip_prefix('L')).unwrap_or(sent);
                    stated_order_id(stripped.split('.').next().unwrap_or(stripped))
                }))
                .or_else(|| {
                    // No record of having sent that name. A cancel issued
                    // before anything was observed states the versioned form,
                    // which reads as the number it is built from.
                    let stripped = stated.strip_prefix('C').unwrap_or(stated);
                    let base = stripped.split('.').next().unwrap_or(stripped);
                    // Through the same range check every other id on the wire
                    // takes. Read bare, a number past the highest this client
                    // can carry reached the record it names and was reported
                    // back as a negative one, which is no order at all.
                    stated_order_id(base)
                })
        }));
        // An empty tag is as good as an absent one. Kept as the empty string,
        // it travelled as the completed status a refusal is told apart by —
        // and an order whose status reads "Inactive" with nothing beside it is
        // one the venue is merely holding, so a refused order this side had
        // already retired and filed as finished came back out of the working
        // list.
        let reason = parsed.get(&58)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Cancel rejected");
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        log::warn!("CancelReject: origClOrd={orig_clord:?} type={reject_type} code={reason_code} reason={reason}");

        let Some(oid) = orig_clord else { return };

        // FIX CxlRejReason 1 = UnknownOrder: the venue is stating that the
        // order does not exist on its side. Restoring it to working asserted
        // the opposite of the message being handled, and the engine's own view
        // governs subsequent cancels, modifies and reconnect bookkeeping — so a
        // phantom order persisted there while the cache row that would have
        // surfaced it was removed.
        //
        // Read as a positive statement, not as an absence: a missing or
        // unparseable tag 102 is synthesized as -1 here and says nothing, so it
        // takes the same path as the reasons that do mean the order is working.
        let unknown_order = reason_code == 1;

        // The answer to a replace arrives on this message too, and the record
        // took the attempt ahead of it. Where the venue refuses the attempt
        // and the order still stands, put back what the venue is known to
        // hold. An order the venue says is gone has no terms to fall back to,
        // and the refusal of a cancellation changed none — the revision it may
        // be waiting on has its own answer coming.
        // Whether it answers a revision still outstanding. The venue takes a
        // second revision before it has answered the first, and a cancel can
        // be sent over both, so a refusal of a revision already answered says
        // nothing about where the order stands now.
        // Meaningful for a refused change alone, and read only there. Left
        // true by default, the two sites that set it disagreed about what a
        // refused cancellation carries.
        let mut answers_a_live_revision = false;
        // Whether the caller had withdrawn this order before the venue
        // answered the change, read before anything is written back.
        let mut withdrawn_before_the_restore = false;
        if reject_type == 2 && !unknown_order {
            let refused_revision = parsed.get(&11)
                .map(|c| revision_of(c))
                .unwrap_or_else(|| *context.modify_versions.get(&oid).unwrap_or(&0));
            answers_a_live_revision = context.pre_replace.contains_key(&(oid, refused_revision));
            // Read before the restore writes the snapshot's status back over
            // it. The copy taken below is of the book as it stands after that,
            // so asking it whether a cancel was outstanding asks the wrong
            // moment — the answer is always the snapshot's.
            withdrawn_before_the_restore = context
                .order(oid)
                .is_some_and(|o| o.status == crate::types::OrderStatus::PendingCancel);
            context.restore_pre_replace(oid, refused_revision);
        }

        // Update local context only for an order tracked in this session.
        let mut restored: Option<crate::types::OrderStatus> = None;
        // Where the refusal says the order finished rather than that it stands.
        let mut finished_by_the_refusal: Option<crate::types::OrderStatus> = None;
        let instrument = if let Some(order) = context.order(oid).copied() {
            if unknown_order {
                // Terminal and removed, which is what the reject states.
                // Holding the record in a non-working status instead is not an
                // option here: those are excluded from the open-order count
                // that guards instrument reclamation, so the slot could be
                // handed to another contract while a retained order still
                // pointed at it, and a late fill would move the wrong position.
                //
                // A fill that races the rejection is not lost with the order:
                // the untracked-fill path books it and moves the position.
                context.retire_order(oid);
            // A refused cancellation always leaves the order standing, so its
            // status always goes back. A refused change does too — but only
            // where it is the change the order is still waiting on: one the
            // venue answered before a cancel went out says nothing about where
            // the order stands now, and forcing it back to working undid the
            // withdrawal the caller had been told about.
            } else if reject_type != 2 || answers_a_live_revision {
                // The reject states where the order stands, and a cancel the
                // venue refuses is very often refused BECAUSE the order
                // finished — which is what it says on that tag. Read past it,
                // the restore put a finished order back to working and did
                // none of the cleanup a finish does, so the caller was told an
                // order was live that the venue had already filled.
                let stated = parsed.get(&39)
                    .map(|s| status_of(s, oid, parsed))
                    .filter(|s| crate::types::order_status::is_terminal(*s));
                let restore_status = match stated {
                    Some(finished) => finished,
                    None if order.filled > 0 => crate::types::OrderStatus::PartiallyFilled,
                    None => crate::types::OrderStatus::Submitted,
                };
                // A refusal of the CHANGE says nothing about a cancel sent
                // over it. The venue takes a cancel while a revision is still
                // outstanding — the branch above is here because it does — and
                // refusing the revision leaves that cancel exactly where it
                // was: still owed a verdict of its own. Forced back to working
                // anyway, the withdrawal the caller had been told about was
                // undone, and both books reported a live order with its cancel
                // in flight until the venue answered it.
                //
                // A refused CANCELLATION is the other case and keeps the
                // regression: there the venue has said the withdrawal will not
                // happen, so pending cancel has stopped being true.
                let answers_the_cancel = reject_type != 2;
                if answers_the_cancel || !withdrawn_before_the_restore {
                    // Deliberate regression (PendingCancel back to working) —
                    // the guard would rightly block it on the ordinary path.
                    context.set_order_status_forced(oid, restore_status);
                    restored = Some(restore_status);
                    if crate::types::order_status::is_terminal(restore_status) {
                        finished_by_the_refusal = Some(restore_status);
                    }
                }
                // And said, not only recorded. The engine's book went back to
                // working while the record the surfaces read stayed on the
                // cancel that was refused: `req_open_orders` reported an order
                // as leaving that the venue had said would not leave, and
                // nothing later corrected it, because the refusal is the last
                // message this order draws.
            }
            order.instrument
        } else {
            0
        };

        // Drop the stale cache entry so subsequent req_open_orders stops
        // returning it. Other reasons leave the cache alone; a follow-up exec
        // report will reconcile.
        //
        // No synthetic status update is queued alongside it. The cancel-reject
        // below is the report, and both dispatchers drain fills ahead of order
        // updates — so an update queued here would reach a caller after the
        // fill that raced it, stating the order was gone when it had just been
        // told the order filled.
        if unknown_order {
            shared.orders.remove_order_info(oid);
        }

        // A cancel is very often refused because the order finished, and the
        // refusal states which on tag 39. Taken as a status and nothing more,
        // the order kept its place in the book with a terminal status written
        // on it — a cancel-all still walked to it, and a replace still named
        // it — no completion was filed, and the row a caller reads stayed the
        // working one it had, so `req_open_orders` went on listing an order
        // the venue had said was filled. Nothing later corrected any of it:
        // the refusal is the last message this order draws.
        if let Some(status) = finished_by_the_refusal {
            shared.orders.push_completed_order(CompletedOrder {
                order_id: oid,
                instrument,
                status,
                filled_qty: context.order(oid).map_or(0, |o| o.filled),
                timestamp_ns: context.now_ns(),
            });
            context.retire_order(oid);
            // Restated, not removed. What the order was is what the
            // completed-orders reader asks for next — the contract, the
            // quantity, the price, the venue's own number — and taking the
            // entry away filed an order carrying nothing but its id. The union
            // that lists working orders reads the status, so restating it is
            // what stops the order being listed.
            shared.orders.note_order_finished(
                oid,
                crate::types::order_status::order_status_str(status),
                // Which of the two an "Inactive" is. Filled and Cancelled say
                // so on their own; a refusal shares its word with an order the
                // venue is merely holding, and only the reason beside it
                // separates them.
                if status == crate::types::OrderStatus::Rejected { reason } else { "" },
            );
        }

        // Tag 58 carries the venue's text. The structured reject has tags 434
        // and 102 and no text, which cannot separate "the order does not exist"
        // from "it is too late to cancel". Delivered on the channel a refused
        // order's reason already uses.
        if let Some(text) = parsed.get(&58).filter(|t| !t.is_empty()) {
            shared.orders.push_order_inactive(
                oid, ORDER_INACTIVE_ERROR_CODE, text.clone(),
            );
        }

        let reject = crate::types::CancelReject {
            order_id: oid,
            instrument,
            reject_type,
            reason_code,
            still_working: restored,
            answers_a_live_change: answers_a_live_revision,
            timestamp_ns: context.now_ns(),
        };
        shared.orders.push_cancel_reject(reject);
        emit(event_tx, Event::CancelReject(reject));
    }
}

/// The conditions an order waits on, as the report states them.
///
/// Conditions arrive as one repeating group per condition, so they are read
/// from the raw frame: a flat parse keeps only the last value of each tag.
///
/// A condition this cannot name is omitted and logged rather than guessed at.
/// An order that reads back holding fewer conditions than it was placed with
/// can be resubmitted as one that waits for nothing.
pub(crate) fn decode_conditions(msg: &[u8]) -> Vec<crate::types::OrderCondition> {
    crate::protocol::fix::fix_parse_repeating(msg, COND_TYPE)
        .into_iter()
        .filter_map(|c| {
            let kind = c.get(&COND_TYPE).map(|s| s.trim().to_string()).unwrap_or_default();
            let built = decode_condition(&c);
            if built.is_none() {
                // Dropped and logged: an order reading back with fewer
                // conditions than it was placed with can be resubmitted as one
                // that waits for nothing.
                log::warn!("dropping an order condition of type {kind:?} — it did not read");
            }
            built
        })
        .collect()
}

/// A condition begins where it says what kind it is.
const COND_TYPE: u32 = 6222;

/// One condition, or nothing if the venue's fields for it did not read.
fn decode_condition(c: &std::collections::HashMap<u32, String>) -> Option<crate::types::OrderCondition> {
    use crate::types::OrderCondition;

    const CON_ID: u32 = 6123;
    const EXCHANGE: u32 = 6124;
    const PRICE: u32 = 6125;
    const OPERATOR: u32 = 6126;
    const TIME: u32 = 6223;
    const PERCENT: u32 = 6245;
    const VOLUME: u32 = 6263;
    const EXECUTION: u32 = 6246;
    const CONJUNCTION: u32 = 6137;

    // Tag 6126 carries the comparison itself: `>=` or `<=`, and no others. A
    // condition stating neither has no direction this can name, so it is omitted
    // like any other unreadable field. Reading it as `<=` states a trigger the
    // report did not, and inverts the condition on resubmission.
    //
    // Read lazily: an execution condition has no direction, so it is not
    // refused for want of one.
    let is_more = || match c.get(&OPERATOR).map(|op| op.trim()) {
        Some(">=") => Some(true),
        Some("<=") => Some(false),
        other => {
            log::warn!("order condition states operator {other:?}, which is neither >= nor <=");
            None
        }
    };
    let text = |tag: u32| c.get(&tag).map(|s| s.trim().to_string()).unwrap_or_default();
    let number = |tag: u32| c.get(&tag).and_then(|s| s.trim().parse::<f64>().ok());
    let con_id = c.get(&CON_ID).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
    // `a` joins this condition to the next with AND. The reference client
    // reads any other spelling as not AND — `o`, and the `n` the last one
    // carries — and an order read back here reads the same.
    let is_conjunction_connection = c.get(&CONJUNCTION).map(|s| s.trim()) == Some("a");

    match c.get(&COND_TYPE).map(|s| s.trim()) {
        Some("1") => Some(OrderCondition::Price {
            con_id,
            is_conjunction_connection,
            exchange: text(EXCHANGE),
            price: crate::types::price_from_f64(number(PRICE)?),
            is_more: is_more()?,
            // Tag 6127 is written outbound and absent inbound, so the trigger
            // method reads as the venue's rather than the caller's.
            trigger_method: 0,
        }),
        Some("3") => Some(OrderCondition::Time { time: text(TIME), is_more: is_more()?, is_conjunction_connection }),
        Some("4") => Some(OrderCondition::Margin {
            is_conjunction_connection,
            percent: number(PERCENT)? as u32,
            is_more: is_more()?,
        }),
        Some("5") => {
            // Packed into one field as `symbol=..;exchange=..;securityType=..;`
            let packed = text(EXECUTION);
            let field = |name: &str| {
                packed.split(';')
                    .find_map(|p| p.strip_prefix(&format!("{name}=")))
                    .unwrap_or("")
                    .to_string()
            };
            Some(OrderCondition::Execution {
                is_conjunction_connection,
                symbol: field("symbol"),
                exchange: field("exchange"),
                sec_type: field("securityType"),
            })
        }
        Some("6") => Some(OrderCondition::Volume {
            con_id,
            is_conjunction_connection,
            exchange: text(EXCHANGE),
            volume: number(VOLUME)? as i64,
            is_more: is_more()?,
        }),
        Some("7") => Some(OrderCondition::PercentChange {
            con_id,
            is_conjunction_connection,
            exchange: text(EXCHANGE),
            percent: number(PERCENT)?,
            is_more: is_more()?,
        }),
        _ => None,
    }
}
