use std::sync::Arc;
use std::time::Instant;

use crate::bridge::SharedState;
use crate::config::{chrono_free_timestamp, unix_to_ib_utc_dash};
use crate::engine::context::{book_bucket, book_table_size, Context};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{AlgoParams, OrderCondition, OrderId, OrderRequest, OrderStatus, OrderUpdate, Side};

use super::{HeartbeatState, format_price_ref, format_qty, format_uint};

pub(crate) fn drain_and_send_orders(
    ccp_conn: &mut Option<Connection>,
    context: &mut Context,
    account_id: &str,
    hb: &mut HeartbeatState,
    disconnected: bool,
    shared: &Arc<SharedState>,
) {
    // If CCP is disconnected, leave orders in the pending buffer for retry after reconnect.
    // See: https://github.com/deepentropy/ibx/issues/116
    if disconnected {
        return;
    }
    // A copied wait of a stock's first order ended (ibx#729): the orders
    // that wait are handled again.
    if let Some(wake) = context.order_wake && context.gap_now() >= wake {
        context.order_wake = None;
        release_rth_parked(context);
    }

    let orders: Vec<OrderRequest> = context.drain_pending_orders().collect();
    context.api_client_id = shared.reference.api_client_id();
    let conn = match ccp_conn.as_mut() {
        Some(c) => c,
        None => return,
    };
    // The lookups of the orders held with transmit off (ibx#718).
    if !context.held_lookups.is_empty() {
        advance_held_lookups(context, conn, hb, shared);
    }
    // Several orders of one pass (a bracket, the orders one placeOrder
    // transmits) leave in one write, as the reference writes them
    // (ibx#548).
    let together = orders.len() > 1;
    if together {
        conn.hold();
    }
    send_orders(conn, orders, context, account_id, hb, shared);
    if together && let Some(conn) = ccp_conn.as_mut() && let Err(e) = conn.release() {
        log::warn!("Orders of one pass not written: {}", e);
    }
    if let Some(conn) = ccp_conn.as_mut() {
        flush_order_hours(context, conn, hb);
        send_preset_saves(context, conn, hb);
    }
}

/// The orders of one pass, each checked and written in its turn.
fn send_orders(
    conn: &mut Connection,
    orders: Vec<OrderRequest>,
    context: &mut Context,
    account_id: &str,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) {
    for mut order_req in orders {
        let oid = order_req.order_id();
        // A what-if preview is the order as it would be placed, written by
        // its own encoder (ibx#462); it is unwrapped here and wrapped again
        // if it has to wait.
        let what_if = matches!(order_req, OrderRequest::SubmitWhatIf { .. });
        if let OrderRequest::SubmitWhatIf { request } = order_req {
            order_req = *request;
        }
        let rewrap = |req: OrderRequest| if what_if {
            OrderRequest::SubmitWhatIf { request: Box::new(req) }
        } else {
            req
        };
        if what_if && matches!(order_req, OrderRequest::Cancel { .. } | OrderRequest::CancelAll { .. }
            | OrderRequest::GlobalCancel | OrderRequest::Modify { .. } | OrderRequest::SubmitBracket { .. }
            | OrderRequest::SubmitWhatIf { .. })
        {
            log::warn!("What-if of order {} dropped: only a single new order can be previewed", oid);
            continue;
        }
        // A preview is never sent deactivated (ibx#607, 10.51:
        // the attribute applies to an order that is not a what-if).
        if what_if && let Some(attrs) = crate::engine::outside_rth::flag_attrs(&mut order_req) {
            attrs.deactivate = false;
        }
        // The global cancel waits for nothing: it ends the orders that
        // wait and cancels the book (`global_cancel`).
        if matches!(order_req, OrderRequest::GlobalCancel) {
            global_cancel(conn, context, account_id, hb, shared);
            continue;
        }
        // A new order under the id of one that was never sent (held with
        // transmit off, now transmitted, ibx#509) is that order, on the
        // contract its lookup found while it was held.
        if !what_if && order_req.new_order_qty().is_some() {
            for id in request_order_ids(&order_req) {
                if let Some((held_on, ..)) = context.unsent.remove(&id)
                    && let Some(instrument) = order_req.new_order_instrument_mut()
                {
                    *instrument = held_on;
                }
            }
            // The orders transmitted together leave together (ibx#547):
            // each waits until the contract of every one is known.
            if let Some(unknown) = group_contracts_unknown(context, oid) {
                for instrument in unknown {
                    look_up_order_contract(context, conn, hb, instrument);
                }
                context.rth_parked.push(rewrap(order_req));
                continue;
            }
        }
        // A request that depends on one waiting for its contract definition
        // waits behind it, so the orders go out in the order they were
        // placed, as the reference sends them: a child behind its parent, an
        // order behind another of its OCA group, a request of an order
        // behind that order. A child sent ahead of its parent is refused by
        // the server (201 "Can't find parent order", paper 01/10/2026).
        // The cancel of an order that never left (it waits, or the
        // reference kept it pending after a refusal): ApiCancelled,
        // nothing on the wire.
        if let OrderRequest::Cancel { order_id } = order_req && api_cancel(context, shared, order_id) {
            continue;
        }
        // Placed again while its new order still waits here (for a
        // lookup): 103, the order stays as placed, as the reference answers
        // a re-place of an order it has not sent yet (ibx#688; paper,
        // 18/10/2026, market closed).
        // A what-if with that id too (ibx#462, captured 02/10/2026: the
        // live order waiting for its contract's lookup).
        let replaced = match &order_req {
            OrderRequest::Modify { order_id, .. } => Some(*order_id),
            OrderRequest::SubmitWhatIf { request } => Some(request.order_id()),
            _ => None,
        };
        if let Some(order_id) = &replaced
            && context.rth_parked.iter().any(|r| !matches!(r, OrderRequest::Modify { .. } | OrderRequest::SubmitWhatIf { .. })
                && r.order_id() == *order_id)
        {
            let (code, message) = crate::client_core::duplicate_order_id(*order_id);
            log::warn!("Modify of order {} refused: the order is not sent yet", order_id);
            shared.orders.push_order_error(*order_id, code, message);
            continue;
        }
        if waits_behind(&order_req, &context.rth_parked) {
            context.rth_parked.push(rewrap(order_req));
            continue;
        }
        // A new order on a contract given without a conId waits for the
        // contract's lookup, as the reference looks each API order's
        // contract up before the order (ibx#486).
        if order_req.combo().is_none()
            && let Some(instrument) = order_req.instrument().filter(|_| !matches!(order_req, OrderRequest::CancelAll { .. }))
            && context.market.con_id(instrument) == Some(0)
        {
            look_up_order_contract(context, conn, hb, instrument);
            context.rth_parked.push(rewrap(order_req));
            continue;
        }
        // A stock order, once its contract is known: its trading hours, the
        // order preset of stocks and its records, in the reference's order;
        // it waits for their answers, and the orders of that contract placed
        // meanwhile with it (ibx#706, ibx#728).
        if order_req.new_order_qty().is_some() && order_req.combo().is_none()
            && let Some(instrument) = order_req.instrument()
            && let Some(con_id) = context.market.con_id(instrument).filter(|c| *c > 0)
            && !stock_order_lookups(context, conn, hb, shared, oid, instrument, con_id)
        {
            context.rth_parked.push(rewrap(order_req));
            continue;
        }
        // Once the contract is known: what the reference checks then, read
        // by the client when the order was placed (the algo checks with
        // their warnings, the price, the time zones), the order dropped
        // when refused (ibx#665).
        if let Some(attrs) = order_req.new_order_attrs_mut()
            && !attrs.after_lookup.is_empty()
        {
            let refused = attrs.after_lookup_refused;
            for (code, message) in std::mem::take(&mut attrs.after_lookup) {
                shared.orders.push_order_error(oid, code, message);
            }
            if refused {
                log::warn!("Order {} refused once its contract was known", oid);
                continue;
            }
        }
        // A selling beta hedge without its max size, once its contract is
        // known (ibx#614).
        match hedge_max_size_refusal(&order_req, context, conn, hb, shared) {
            Some(true) => continue,
            // Its quantity waits for the order preset (ibx#748).
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            None => {}
        }
        // A beta hedge that is sent carries that quantity too, when it has one
        // (ibx#748; the reference 10.51, paper: 379 in the order of a MSFT
        // hedge with MSFT held short, 19/10/2026; 9 in an AAPL hedge with 9
        // held, 12/11/2026); with none, its parent's, as before.
        if let Some((side, Some(attrs))) = order_req.new_order_side()
            && attrs.hedge.as_ref().is_some_and(|h| h.kind == 4) && attrs.parent_id != 0
            && let Some(instrument) = order_req.instrument()
        {
            match hedge_default_size(context, conn, hb, shared, instrument, side) {
                None => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(size) if size > 0 => {
                    if let OrderRequest::SubmitEx { qty, .. } = &mut order_req {
                        *qty = (size / crate::types::QTY_SCALE) as u32;
                    }
                }
                Some(_) => {}
            }
        }
        // A refusal the client decided, once the contract is known: the
        // order kept for its cancel (ibx#688).
        if kept_refusal(&mut order_req, context, shared) {
            continue;
        }
        // The time zones the client could not check: against the zone of
        // the contract's trading hours, once they came; the order waits for
        // them when they were asked (ibx#668; the reference 10.51, paper,
        // 13/10/2026 pre-market: 337 for a start time in Asia/Tokyo on
        // MSFT, after its trading hours, nothing sent).
        let con_id = order_req.instrument().and_then(|i| context.market.con_id(i)).unwrap_or(0);
        if let Some(attrs) = order_req.new_order_attrs_mut()
            && !attrs.zone_checks.is_empty()
        {
            // The replays of the 10.40 recordings do not wait: they hold no
            // answer to the trading hours ibx asks.
            match shared.reference.time_zone_id(con_id) {
                None if context.zone_pending.contains(&con_id) && !shared.reference.form_10_40() => {
                    send_order_hours(context, conn, hb, con_id);
                    context.rth_parked.push(rewrap(order_req));
                    continue;
                }
                None => attrs.zone_checks.clear(),
                Some(zone) => {
                    let failed = attrs.zone_checks.iter()
                        .find(|(zones, _, _)| zones.iter().any(|z| *z != zone))
                        .map(|(_, code, message)| (*code, message.clone()));
                    attrs.zone_checks.clear();
                    if let Some((code, message)) = failed {
                        log::warn!("Order {} refused: a time zone that is not its contract's ({})", oid, zone);
                        shared.orders.push_order_error(oid, code, message);
                        continue;
                    }
                }
            }
        }
        // A quantity that is not whole: refused with 10243 and nothing
        // sent, as the reference refuses it for an API client (ib-agent#192
        // B3). Sent with the API client fields of every new order, it was
        // rejected by the server on paper (01/10/2026).
        if let OrderRequest::SubmitLimitFractional { qty, .. } = &order_req {
            if qty % crate::types::QTY_SCALE != 0 {
                let (code, message) = crate::client_core::FRACTIONAL_VIA_API;
                log::warn!("Order {} refused: fractional quantity", oid);
                shared.orders.push_order_error(oid, code, message.into());
                continue;
            }
        }
        // A scale order's fields (ibx#751); a preview goes without them.
        context.scale_send = if what_if { None } else {
            order_req.new_order_side().and_then(|(_, a)| a).and_then(|a| a.scale.clone())
        };
        // A short-side order: refused with 321 and nothing sent, as the
        // reference, or sent with its short-sale fields (ibx#417).
        context.short_sale_send = None;
        if let Some((Side::ShortSell, attrs)) = order_req.new_order_side() {
            let (super_user, omnibus) = shared.reference.short_sale_flags();
            let unset = crate::types::OrderAttrs::default();
            match short_sale_check(attrs.unwrap_or(&unset), account_id, super_user, omnibus) {
                Ok(short_sale) => context.short_sale_send = Some(short_sale),
                Err(cause) => {
                    log::warn!("Order {} refused: {}", oid, cause);
                    shared.orders.push_order_error(oid, 321,
                        format!("Error validating request.-'PlaceOrder(3)' : cause - {}", cause));
                    continue;
                }
            }
        }
        // A combo order goes out once its combo is built: the reference's
        // set-up requests the first time, the session's combo after; the
        // built combo's checks refuse it with their code (ibx#470).
        context.combo_send = None;
        let mut combo_track: Option<(crate::engine::combo::ComboOrder, crate::bridge::ComboView)> = None;
        if let Some(spec) = order_req.combo().cloned() {
            use crate::engine::combo::{self, Ready};
            match context.combos.ready(oid, &spec, &chrono_free_timestamp()) {
                Ready::Waiting(frames) => {
                    send_frames(conn, hb, &frames);
                    context.rth_parked.push(rewrap(order_req));
                    continue;
                }
                Ready::Refused(code, message) => {
                    log::warn!("Combo order {} refused: {} {}", oid, code, message);
                    shared.orders.push_order_error(oid, code, message);
                    // No definition: the reference keeps the order id in
                    // its API pending map, with no order made; a later
                    // cancel ends it as ApiCancelled (scenario
                    // i105_combo_directed of 26/09/2026).
                    if code == crate::engine::combo::NO_DEFINITION.0 && !what_if {
                        context.api_pending.insert(oid, order_req);
                    }
                    continue;
                }
                Ready::Built(built) => {
                    if let Some((code, message)) = combo::order_refusal(&built, &spec) {
                        log::warn!("Combo order {} refused: {} {}", oid, code, message);
                        shared.orders.push_order_error(oid, code, message);
                        continue;
                    }
                    // The preset of a combo is asked once its set-up is
                    // done, right before the order (capture of
                    // 28/09/2026, rth_order_types).
                    if !preset_tif(context, conn, hb, shared, &mut order_req, "COMB") {
                        context.rth_parked.push(rewrap(order_req));
                        continue;
                    }
                    // The trading permission of the legs' option classes,
                    // once the combo is built and its preset asked: 460,
                    // nothing sent, the order kept for its cancel
                    // (ibx#510; the reference 10.51, paper, regular hours
                    // 19/10/2026: an SPY call vertical, the set-up, the
                    // preset, then 460; its cancel Cancelled, 202, 161).
                    match combo_permission(&built, context, conn, hb) {
                        Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                        Some(true) => {
                            log::warn!("Combo order {} refused: no trading permission for a leg (460)", oid);
                            shared.orders.push_order_error(oid, 460, NO_TRADING_PERMISSIONS.to_string());
                            if let Some(instrument) = order_req.instrument().filter(|_| !what_if) {
                                let parent = order_req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
                                context.keep_unsent(oid, (instrument, order_req.new_order_qty().unwrap_or(0), parent));
                            }
                            continue;
                        }
                        None => {}
                    }
                    // A SMART combo with no NonGuaranteed value, on a
                    // session whose server routes no SMART combo as
                    // guaranteed (REJGNONAT): 10043 with two legs, 10045
                    // with another number; the order stays in the API
                    // pending map, its cancel ApiCancelled (ibx#677;
                    // observed 13/10/2026, paper, a two-leg stock
                    // combo). After the legs' permission: an option
                    // vertical without it gets 460, not this (ibx#510).
                    if spec.smart_con_id > 0 && !spec.routing_attrs.iter().any(|(tag, _)| *tag == 6248)
                        && crate::control::logon::has_feature(&shared.reference.logon_features(), "REJGNONAT")
                    {
                        let (code, message) = if spec.legs.len() == 2 {
                            (10043, "Missing or invalid NonGuaranteed value. REL+MKT, LMT+MKT, and REL+LMT order of two legs can only be set as non-guaranteed.")
                        } else {
                            (10045, "This combo order is not supported.")
                        };
                        log::warn!("Combo order {} refused: {} {}", oid, code, message);
                        shared.orders.push_order_error(oid, code, message.to_string());
                        if !what_if {
                            context.api_pending.insert(oid, order_req);
                        }
                        continue;
                    }
                    combo_track = Some(combo_order_setup(context, shared, &mut order_req, built, spec));
                }
            }
        }
        // A contract other than a stock or a combo, or one given by its
        // conId alone: its definition first, for the fields its order
        // message carries (ibx#666).
        if let Some(instrument) = order_req.new_order_qty().and(order_req.instrument()).filter(|_| order_req.combo().is_none()) {
            let (sec_type, _) = context.market.order_routing(instrument);
            // An index is refused on its type, known from its lookup
            // (ibx#666).
            if (!matches!(sec_type.as_str(), "" | "STK" | "BAG" | "IND") || context.market.blank_symbol(instrument))
                && matches!(definition(context, conn, hb, instrument), Definition::Waiting)
            {
                context.rth_parked.push(rewrap(order_req));
                continue;
            }
            let (sec_type, _) = context.market.order_routing(instrument);
            let sec_type = if matches!(sec_type.as_str(), "" | "CS") { "STK".to_string() } else { sec_type };
            // A type known from the definition (a contract given by its
            // conId) whose order is refused or not written yet (ibx#666).
            // A bond's permission comes first (460); its own refusal once
            // the permission is there (ibx#666).
            // A fund bought by cash: its amount stands for its quantity in
            // its reports (ibx#666).
            if sec_type == "FUND"
                && let Some((_, Some(attrs))) = order_req.new_order_side()
                && attrs.cash_qty > 0 && order_req.new_order_qty() == Some(0)
            {
                let qty = (attrs.cash_qty as f64 / crate::types::PRICE_SCALE as f64 * crate::types::QTY_SCALE as f64).round() as i64;
                context.cash_orders.insert(order_req.order_id(), qty);
            }
            // A pegged-to-stock order, as the reference 10.51 (ibx#745;
            // paper, 17/10 and 18/10/2026, market closed): on a stock,
            // whose order types do not include it, refused with 387 at
            // once; on an option on BOX with a delta outside 1 to 100,
            // refused with 341; both kept for their cancel (Cancelled, 202,
            // 161). An option order goes on to its permission check (460
            // without it); other contracts, and an option with the
            // permission, are not written yet.
            if let OrderRequest::SubmitEx { kind: crate::types::OrderKind::PegStk { delta }, .. } = &order_req {
                let oid = order_req.order_id();
                let exchange = context.market.exchange(instrument).to_ascii_uppercase();
                let keep = |context: &mut Context| {
                    let parent = order_req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
                    context.keep_unsent(oid, (instrument, order_req.new_order_qty().unwrap_or(0), parent));
                };
                if sec_type == "STK" {
                    log::warn!("Order {} refused: PEG STK on a stock", oid);
                    shared.orders.push_order_error(oid, 387, crate::engine::outside_rth::UNSUPPORTED_ORDER_TYPE.to_string());
                    keep(context);
                    continue;
                }
                if sec_type == "OPT" && exchange == "BOX" && !(1.0..=100.0).contains(delta) {
                    log::warn!("Order {} refused: PEG STK delta {} outside 1 to 100", oid, delta);
                    shared.orders.push_order_error(oid, 341, PEG_STK_DELTA_INVALID.to_string());
                    keep(context);
                    continue;
                }
                if sec_type != "OPT" {
                    log::warn!("Order {} refused: PEG STK order on {}", oid, sec_type);
                    shared.orders.push_order_error(oid, 321, format!(
                        "Error validating request.-'PlaceOrder(3)' : cause - PEG STK orders on {sec_type} are not written by ibx yet (ibx#745)"));
                    continue;
                }
            }
            // An order below the contract's minimum size, once its definition
            // gave one: 388, nothing sent and nothing kept (ibx#688; the
            // reference 10.51, paper, 19/10/2026, market closed: one share of
            // a Tokyo stock, its cancel 10147).
            let min_size = context.market.con_id(instrument).and_then(|c| context.min_sizes.get(&c).copied());
            if let (Some(min), Some(qty)) = (min_size, order_req.new_order_qty())
                && (qty as u64) < min * crate::types::QTY_SCALE as u64
            {
                let oid = order_req.order_id();
                log::warn!("Order {} refused: below the minimum size {}", oid, min);
                shared.orders.push_order_error(oid, 388, format!("Order size {} is smaller than the minimum required size of {}.",
                    qty as f64 / crate::types::QTY_SCALE as f64, min));
                continue;
            }
            // A display size or a minimum quantity below that size: 10083
            // and 10084, nothing kept (the same runs: a Tokyo stock order of
            // 200 showing 50, or with a minimum quantity of 50).
            if let (Some(min), Some((_, Some(attrs)))) = (min_size, order_req.new_order_side()) {
                let refusal = if attrs.display_size > 0 && u64::from(attrs.display_size) < min {
                    Some((10083, "Display size is smaller than the minimum requirement."))
                } else if attrs.min_qty > 0 && u64::from(attrs.min_qty) < min {
                    Some((10084, "Min. quantity is smaller than the minimum requirement."))
                } else {
                    None
                };
                if let Some((code, text)) = refusal {
                    let oid = order_req.order_id();
                    log::warn!("Order {} refused: {} below the minimum size {}", oid, code, min);
                    shared.orders.push_order_error(oid, code, text.to_string());
                    continue;
                }
            }
            // A request for quote, once its contract is known: 416, nothing
            // sent and nothing kept (a cancel gives 10147), as the reference
            // 10.51 (paper, 19/10/2026, market closed: an AAPL option given
            // by its terms, after its lookup, and the stock by conId, at
            // once). A contract on which the reference takes one is not
            // observed yet.
            if let OrderRequest::SubmitEx { kind: crate::types::OrderKind::Quote, .. } = &order_req {
                let oid = order_req.order_id();
                log::warn!("Order {} refused: request for quote", oid);
                shared.orders.push_order_error(oid, crate::client_core::RFQ_NOT_APPLICABLE,
                    format!("RFQ is not applicable for the contract. Order ID:{}", oid));
                continue;
            }
            // A volatility order on a contract that is no option: a limit
            // order priced by the account's preset (ibx#673).
            match volatility_as_limit(&mut order_req, &sec_type, context, conn, hb) {
                Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(true) => continue,
                None => {}
            }
            // A crypto order's permission comes first too: without it the
            // order stays pending with no error (ibx#666).
            if let Some((code, text)) = unwritten_type_refusal(&order_req, &sec_type).filter(|_| !matches!(sec_type.as_str(), "BOND" | "CRYPTO")) {
                let oid = order_req.order_id();
                log::warn!("Order {} refused: {} order ({})", oid, sec_type, code);
                shared.orders.push_order_error(oid, code, text);
                continue;
            }
            // The types of `TYPE_PRESETS` ask theirs after their permission
            // question, below (ibx#729).
            if !TYPE_PRESETS.contains(&sec_type.as_str()) && !preset_tif(context, conn, hb, shared, &mut order_req, &sec_type) {
                context.rth_parked.push(rewrap(order_req));
                continue;
            }
        }
        // An overnight order on a contract that has no overnight trading
        // (ibx#660).
        match overnight_refusal(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // A pegged or protection type the contract's list does not allow
        // on the order's exchange is refused, as the reference (ibx#414,
        // ibx#493). A stock order directed away from SMART meets the
        // redirect precaution first, and these two checks after it
        // (ibx#688; the reference 10.51, paper, 18/10/2026, market closed:
        // STP PRT on ISLAND discarded with 10311, on SMART refused with 387).
        let redirect_first = !what_if && redirect_applies(&order_req, context);
        if !redirect_first {
            match pegged_type_refusal(&order_req, context, conn, hb, shared) {
                Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(true) => continue,
                None => {}
            }
            // All-or-none where the contract's list does not allow it on
            // the order's exchange (ibx#263).
            match all_or_none_refusal(&order_req, context, conn, hb, shared) {
                Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(true) => continue,
                None => {}
            }
        }
        // Outside RTH: kept only where the reference keeps it, from the
        // contract definition of the order's exchange (ibx#465).
        // Allow pre-open and ignore opening auction with it (ibx#607).
        match apply_outside_rth(&mut order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // Post-only with sweep-to-fill, or on pegged to market (ibx#607).
        if post_only_refusal(&order_req, context, shared) {
            continue;
        }
        // Not-held on a stock order not for the ATS venue (ibx#690).
        if not_held_refusal(&order_req, context, shared) {
            continue;
        }
        // An option, futures option or warrant order without the trading
        // permission of its class (ibx#666).
        match permission_refusal(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // The attributes and market on close the contract's list does not
        // allow, after the permission question (ibx#688; the reference
        // 10.51, paper, 19/10/2026: an AAPL option without the permission,
        // with sweep-to-fill, imbalance-only or market on close, got 460).
        match attribute_list_refusal(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // The order preset of a future, a CFD, a warrant, a metal, a fund or
        // a forex pair, after the permission question (ibx#729).
        if !type_preset(&mut order_req, context, conn, hb, shared) {
            context.rth_parked.push(rewrap(order_req));
            continue;
        }
        // The price management flag, where the reference writes it; the
        // contract definition of the order's exchange is asked once when
        // the flag could apply (ibx#492).
        match price_mgmt_flag(&order_req, context, conn, hb) {
            Some(on) => context.price_mgmt_send = on,
            None => { context.rth_parked.push(rewrap(order_req)); continue; }
        }
        // MiFID on a session with it (ibx#500): the order's values, and
        // the refusal of a covered order without a Decision Maker or Algo.
        match mifid_check(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // An order on an FA group (ibx#507): it waits for the FA saves not
        // answered, and the unified mode's method checks.
        match fa_check(&order_req, context, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // The soft dollar tier: refused when not in the account's tiers of
        // the contract's market, else sent (ibx#506).
        match soft_dollar_check(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // A stock order directed away from SMART: discarded by the
        // reference's redirect precaution unless it is bypassed (ibx#486).
        if !what_if {
            match redirect_precaution(&order_req, context, conn, hb, shared) {
                Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(true) => continue,
                None => {}
            }
            if redirect_first {
                match pegged_type_refusal(&order_req, context, conn, hb, shared) {
                    Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                    Some(true) => continue,
                    None => {}
                }
                match all_or_none_refusal(&order_req, context, conn, hb, shared) {
                    Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                    Some(true) => continue,
                    None => {}
                }
            }
            // routeMarketableToBbo: discarded unless bypassed (ibx#608).
            if route_bbo_precaution(&order_req, context, shared) {
                continue;
            }
            // The parent of a beta hedge whose max size is below the
            // hedge's size: discarded by the hedge-size precaution (ibx#602).
            if hedge_size_precaution(&order_req, context, shared) {
                continue;
            }
            // Attached orders the order preset does not define (ibx#609),
            // once the preset of the contract's type is known: the
            // reference asks it first (ibx#717; paper, 16/10/2026: its
            // answer 0.2 s after the order, 10355 0.79 s after it).
            if order_req.new_order_side().and_then(|(_, a)| a).is_some_and(|a| a.attached_from_preset != (false, false))
                && let Some(instrument) = order_req.instrument()
            {
                let sec_type = context.market.order_routing(instrument).0;
                let sec_type = if matches!(sec_type.as_str(), "" | "CS") { "STK".to_string() } else { sec_type };
                if !ask_order_presets(context, conn, hb, &preset_kinds(&sec_type, context.market.symbol(instrument))) {
                    context.rth_parked.push(rewrap(order_req));
                    continue;
                }
            }
            if attached_preset_refusal(&order_req, context, shared) {
                continue;
            }
            // Directed to an exchange the contract does not list (ibx#630).
            match invalid_exchange_refusal(&order_req, context, conn, hb, shared) {
                Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
                Some(true) => continue,
                None => {}
            }
            drop_seek_off_smart(&mut order_req, context, shared);
        }
        // The contract's currency, USD when unknown (ibx#466).
        let currency: String = order_req.instrument()
            .map(|i| context.market.currency(i).to_string())
            .unwrap_or_else(|| "USD".to_string());
        // The reference refuses a modify of an order that is no longer
        // working, or whose cancel is pending, and sends nothing (ibx#463).
        // Checked here, where the order's status is current.
        if let OrderRequest::Modify { order_id, .. } = &order_req {
            let working = context.order(*order_id)
                .is_some_and(|o| o.status != OrderStatus::PendingCancel);
            if !working {
                log::warn!("Modify of order {} refused: the order is not working", order_id);
                let (code, message) = crate::client_core::MODIFY_OF_FINISHED_ORDER;
                shared.orders.push_order_error(*order_id, code, message.into());
                continue;
            }
            // A replace sends a deactivated order on: its openOrder shows
            // deactivate off from then on (ibx#607, paper 12/10/2026).
            shared.orders.note_dropped_flags(*order_id, crate::bridge::DROPPED_DEACTIVATE);
        }
        // The reference refuses a cancel it cannot apply and sends nothing
        // (ibx#464; captured 23/09/2026): 10147 for an order it does not
        // know, 10148 with the state for one that is finished or has a
        // cancel pending.
        // The cancel of an order refused with 387 before it was sent ends
        // it as the reference does (ibx#542, paper 09/10/2026): orderStatus
        // Cancelled with nothing filled, 202 "Order was discarded", then
        // 161, the answer to a cancel of an order that cannot be cancelled.
        // A second cancel finds it cancelled (10148).
        // An order held with transmit off ends the same way (ibx#509).
        if let OrderRequest::Cancel { order_id } = &order_req && end_unsent(context, shared, *order_id) {
            continue;
        }
        if let OrderRequest::Cancel { order_id } = &order_req {
            if let Some((code, message)) = cancel_refusal(context, *order_id) {
                log::warn!("Cancel of order {} refused: {}", order_id, message);
                shared.orders.push_order_error(*order_id, code, message);
                continue;
            }
        }
        // A price off the contract's price grid: refused with 110 and
        // nothing sent, as the reference; it does not round it (ibx#263,
        // replacing the snapping of ibx#216). The tick comes from the
        // market-data subscription ack; without one only a negative price
        // is refused. A combo (BAG) is checked with its own rule, which
        // allows a price at or below 0; its tick is the
        // combo's, which ibx does not have, so only the sign rule applies
        // and a combo passes (ibx#470).
        let combo = order_req.combo().is_some()
            || matches!(&order_req, OrderRequest::Modify { order_id, .. } if context.combos.orders.contains_key(order_id));
        let instrument = order_req.instrument().or_else(|| context.order(oid).map(|o| o.instrument));
        let tick = if combo { 0 } else { instrument.map_or(0, |i| context.market.min_tick_scaled(i)) };
        if let Some(id) = order_req.off_grid_order(tick, combo) {
            let (code, message) = crate::client_core::PRICE_VARIATION;
            log::warn!("Order {} refused: a price off the price grid", id);
            shared.orders.push_order_error(id, code, message.into());
            continue;
        }
        // A what-if goes out under a ClOrdID of its own and stays out of
        // the order table: an order with the same id is left as it is
        // (ibx#462). As the reference's preview copy, a new id of the order
        // id generator with version 0 (ibx#486, b1_462_whatif of
        // 02/10/2026: 1288736441.0, 1288736443.0 for the orders 74 and 75).
        let held = what_if.then(|| (context.order(oid).copied(), context.modify_versions.get(&oid).copied(), context.book_peak));
        if what_if {
            let clord = format!("{}.0", context.new_server_id());
            let instrument = order_req.instrument().unwrap_or(0);
            context.what_ifs.insert(clord.clone(), (oid, instrument));
            context.what_if_send = Some(clord);
        }
        // A new order goes out under a server id of the reference's order
        // id generator; its API order id is its key, sent in its own field.
        if !what_if {
            // An order of an earlier session can be held under the id of
            // this new order (ibx#538). Another client's goes back under
            // its own server id: the reference keeps the orders of each
            // client id apart, and the new order must not go out under
            // that order's identity (the server refused it as a duplicate
            // and the caller was told nothing). This client's own working
            // order is the order with that id: a new order under its id is
            // refused, and the order is left as it is.
            let mut own = None;
            for id in order_req.new_order_ids() {
                match context.earlier_session_order(id) {
                    Some(true) => own = Some(id),
                    Some(false) => {
                        if let Some(server) = context.release_recovered_key(id) {
                            shared.orders.rekey_order(id, server);
                            log::info!("Order id {} of this session: the order of another client held under it is now under {}", id, server);
                        }
                    }
                    None => {}
                }
            }
            if let Some(id) = own {
                log::warn!("Order {} refused: an order of this client with that id is working", id);
                let (code, message) = crate::client_core::duplicate_order_id(id);
                shared.orders.push_order_error(id, code, message);
                continue;
            }
            for id in order_req.new_order_ids() {
                context.assign_server_id(id);
            }
        }
        // The trading hours still due for the order's contract leave before
        // it (ibx#728; a future and a CFD order, paper 13/10/2026: the hours
        // 3 to 4 ms before the order message).
        if let Some(con_id) = order_req.instrument().and_then(|i| context.market.con_id(i)).filter(|c| *c > 0) {
            send_order_hours(context, conn, hb, con_id);
        }
        let result = match order_req {
            OrderRequest::SubmitLimit { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),   // ClOrdID
                    (1, account_id),    // Account
                    (55, &symbol),      // Symbol
                    (54, side_str),     // Side
                    (38, &qty_str),     // OrderQty
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),   // Price
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),       // SecurityType = CommonStock
                    (100, &destination),
                    (6210, &destination),     // ExDestination
                    (15, currency.as_str()),        // Currency
                ])
            }
            OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'4', b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "4"),          // OrdType = Stop Limit
                    (44, &price_str),   // Limit Price
                    (99, &stop_str),    // StopPx
                    (6117, &stop_str), // stop trigger, as the reference (ibx#466)
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitLimitGtc { order_id, instrument, side, qty, price, outside_rth } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'1', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "1"),          // TIF = GTC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                if outside_rth {
                    fields.push((6433, "1")); // OutsideRTH
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitLimitEx { order_id, instrument, side, qty, price, tif, attrs } => {
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    crate::types::OrderKind::Limit { price }, tif, &attrs)
            }
            OrderRequest::SubmitEx { order_id, instrument, side, qty, kind, tif, attrs } => {
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    kind, tif, &attrs)
            }
            OrderRequest::SubmitMarket { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'1', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                log::info!("Sending MKT order: clord={} acct={} sym={} side={} qty={}",
                    clord_str, account_id, symbol, side_str, qty_str);
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),    // Account
                    (55, &symbol),      // Symbol
                    (54, side_str),
                    (38, &qty_str),
                    (40, "1"),          // OrdType = Market
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),       // SecurityType
                    (100, &destination),
                    (6210, &destination),     // ExDestination
                    (15, currency.as_str()),        // Currency
                ])
            }
            OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'3', b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),          // OrdType = Stop
                    (99, &stop_str),    // StopPx
                    (6117, &stop_str), // stop trigger, as the reference (ibx#466)
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitStopGtc { order_id, instrument, side, qty, stop_price, outside_rth } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'3', b'1', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),          // OrdType = Stop
                    (99, &stop_str),
                    (6117, &stop_str), // stop trigger, as the reference (ibx#466)
                    (59, "1"),          // TIF = GTC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                if outside_rth {
                    fields.push((6433, "1"));
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitStopLimitGtc { order_id, instrument, side, qty, price, stop_price, outside_rth } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'4', b'1', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "4"),          // OrdType = Stop Limit
                    (44, &price_str),
                    (99, &stop_str),
                    (6117, &stop_str), // stop trigger, as the reference (ibx#466)
                    (59, "1"),          // TIF = GTC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                if outside_rth {
                    fields.push((6433, "1"));
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitLimitIoc { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'3', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "3"),          // TIF = IOC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitLimitFok { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'4', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "4"),          // TIF = FOK
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let trail_str = format_price_ref(trail_amt);
                let trail_stop_str = format_price_ref(trail_stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#136 capture: amount-based trailing stop carries
                // the trail amount in both the stop price and the peg offset,
                // and requires the trailing-stop execution instruction. Without it,
                // the gateway rejects with an invalid-value error on the instruction field.
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),          // OrdType = Stop (used for trailing too)
                    (99, &trail_str),   // StopPx = trail amount
                    (211, &trail_str), // PegOffset = trail amount
                    (18, "a"),          // ExecInst = TrailingStop
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                // Optional initial stop trigger, only when set
                // (ib-agent#173).
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, lmt_offset, lmt_price, trail_amt, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, lmt_price.unwrap_or(lmt_offset), b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price_ref(lmt_offset);
                let price_str = lmt_price.map(format_price_ref);
                let trail_str = format_price_ref(trail_amt);
                let trail_stop_str = format_price_ref(trail_stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#136 capture: TRAIL LIMIT uses the trailing stop limit
                // order type (not the trailing stop's), does NOT carry the limit price
                // (the gateway derives it from the limit offset + the activation
                // reference), does NOT carry the execution instruction, and carries
                // the trail amount in both the stop price and the peg offset. The
                // limit offset is the limit-vs-trail offset.
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "TSL"),         // OrdType = Trailing Stop Limit
                    (99, &trail_str),    // StopPx = trail amount
                    (211, &trail_str),   // PegOffset = trail amount
                    (59, "0"),           // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                // The absolute limit price in the limit price field and no limit
                // offset, or the offset in the limit offset field (ib-agent#194).
                match &price_str {
                    Some(price) => fields.push((44, price)),
                    None => fields.push((6370, &offset_str)),
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_percent, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                // Per ib-agent#156 capture: percent-trail mirrors the stop price and
                // the peg offset as the percent in decimal form (1.00 for 1%) with
                // the trailing-stop execution instruction; without these three the
                // gateway rejects with an invalid-value error on the instruction
                // field. The trail unit is 100 = percent, at every percentage
                // (ib-agent#192 B7, ibx#339); it
                // was sent as basis points, right only at exactly 1%.
                let pct_decimal = format_price_ref(trail_percent).to_string();
                let trail_stop_str = format_price_ref(trail_stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),              // OrdType = Trailing Stop
                    (99, &pct_decimal),     // StopPx = percent as decimal
                    (211, &pct_decimal),    // PegOffset = percent as decimal (= stop price)
                    (18, "a"),              // ExecInst = TrailingStop
                    (6268, "100"),          // Trail unit = percent
                    (59, "0"),              // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopPctEx { order_id, instrument, side, qty, trail_percent, tif, attrs, trail_stop_price } => {
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    crate::types::OrderKind::TrailPct { trail_percent, trail_stop_price }, tif, &attrs)
            }
            OrderRequest::SubmitMoc { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'5', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "5"),          // OrdType = Market on Close
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitLoc { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'B', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "B"),          // OrdType = Limit on Close
                    (44, &price_str),   // Limit price
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'J', b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "J"),          // OrdType = Market if Touched
                    (99, &stop_str),    // StopPx = trigger price
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'K', b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "LT"),         // OrdType = Limit If Touched (per ib-agent#138)
                    (44, &price_str),   // Limit price
                    (99, &stop_str),    // StopPx = trigger price
                    (59, "0"),          // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitBracket { parent_id, tp_id, sl_id, instrument, side, qty, entry_price, take_profit, stop_loss } => {
                let exit_side = match side { Side::Buy => Side::Sell, Side::Sell | Side::ShortSell => Side::Buy };
                let exit_side_str = fix_side(exit_side);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                // Order ids in the versioned form of every other order, so
                // a cancel or a later child refers to what the server holds.
                let clord = |id: OrderId| format!("{}.{}", context.server_id(id), context.modify_versions.get(&id).copied().unwrap_or(0));
                let parent_str = clord(parent_id);
                let tp_str = clord(tp_id);
                let sl_str = clord(sl_id);
                let entry_str = format_price_ref(entry_price);
                let tp_price_str = format_price_ref(take_profit);
                let sl_price_str = format_price_ref(stop_loss);
                // The children's parent link and OCA group, as the reference
                // sends them (ibx#311, ibx#329).
                let (parent_link, oca_group) = bracket_parent_link(context, parent_id);
                // The bracket key of the three orders (ibx#248).
                let tp_key = context.brackets().attach_child(parent_id, tp_id).to_string();
                let sl_key = context.brackets().attach_child(parent_id, sl_id).to_string();
                let parent_key = context.bracket_keys.get(&parent_id).map(|k| k.to_string()).unwrap_or_default();

                // 1. Parent order: limit entry
                context.insert_order(crate::types::Order::new(
                    parent_id, instrument, side, qty, entry_price, b'2', b'0', 0,
                ));
                let now = chrono_free_timestamp();
                let _ = send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &parent_str),
                    (1, account_id),
                    (6531, &parent_key),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // Limit
                    (44, &entry_str),
                    (59, "0"),          // DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ]);

                // 2. Take-profit child: limit exit, linked to parent, in OCA group
                context.insert_order(crate::types::Order::new(
                    tp_id, instrument, exit_side, qty, take_profit, b'2', b'1', 0,
                ));
                let now = chrono_free_timestamp();
                let _ = send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &tp_str),
                    (1, account_id),
                    (6531, &tp_key),
                    (55, &symbol),
                    (54, exit_side_str),
                    (38, &qty_str),
                    (40, "2"),          // Limit
                    (44, &tp_price_str),
                    (59, "1"),          // GTC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                    (6107, &parent_link),      // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, BRACKET_CHILD_OCA_TYPE),
                ]);

                // 3. Stop-loss child: stop exit, linked to parent, in OCA group
                context.insert_order(crate::types::Order::new(
                    sl_id, instrument, exit_side, qty, stop_loss, b'3', b'1', stop_loss,
                ));
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &sl_str),
                    (1, account_id),
                    (6531, &sl_key),
                    (55, &symbol),
                    (54, exit_side_str),
                    (38, &qty_str),
                    (40, "3"),          // Stop
                    (99, &sl_price_str),
                    (6117, &sl_price_str), // stop trigger, as on every stop (ibx#466)
                    (59, "1"),          // GTC
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                    (6107, &parent_link),      // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, BRACKET_CHILD_OCA_TYPE),
                ])
            }
            OrderRequest::SubmitRel { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'R', b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price_ref(offset);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#138 capture: Relative shares its order type with
                // Trail and is disambiguated by the relative execution instruction.
                // Peg offset goes in the peg offset field (not the stop price
                // outbound), and there is no limit price.
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),              // OrdType = Pegged (used for Relative too)
                    (211, &offset_str),     // PegOffset
                    (18, "R"),              // ExecInst = Relative
                    (59, "0"),              // TIF = DAY
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitLimitOpg { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'2', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),              // OrdType = Limit
                    (44, &price_str),
                    (59, "2"),              // TIF = OPG (At the Opening)
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            // The engine's own Adaptive and algo requests: a limit order
            // with the algo, through the one encoder of every order type
            // (ibx#263).
            OrderRequest::SubmitAdaptive { order_id, instrument, side, qty, price, priority, tif, attrs } => {
                let attrs = crate::types::OrderAttrs { algo: Some(crate::types::OrderAlgo::Adaptive(priority)), ..attrs };
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    crate::types::OrderKind::Limit { price }, tif, &attrs)
            }
            OrderRequest::SubmitAlgo { order_id, instrument, side, qty, price, algo, tif, attrs } => {
                let attrs = crate::types::OrderAttrs { algo: Some(crate::types::OrderAlgo::Params(algo)), ..attrs };
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    crate::types::OrderKind::Limit { price }, tif, &attrs)
            }
            OrderRequest::SubmitPegBench { order_id, instrument, side, qty, price,
                ref_con_id, is_peg_decrease, pegged_change_amount, ref_change_amount,
                stock_ref_price, ref_exchange } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, crate::types::ORD_PEG_BENCH, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp().to_string();
                // As the reference (ibx#415): the starting price in the stop
                // price field, no limit price, the peg instruction, then the
                // benchmark attributes.
                let mut fields: Vec<(u32, String)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
                    (fix::TAG_SENDING_TIME, now.clone()),
                    (11, format!("{}.{}", context.server_id(order_id), ver)),
                ];
                if price > 0 { fields.push((99, format_price_ref(price).to_string())); }
                fields.extend([
                    (1, account_id.to_string()),
                    (55, symbol),
                    (54, fix_side(side).to_string()),
                    (38, format_uint(qty as u64).to_string()),
                    (40, "PB".to_string()),     // OrdType = Pegged to Benchmark
                    (18, "R".to_string()),
                    (59, "0".to_string()),
                    (167, sec_type_str),
                    (100, destination.clone()),
                    (6210, destination),
                    (15, currency.clone()),
                ]);
                fields.extend(peg_bench_attrs(stock_ref_price, ref_con_id, is_peg_decrease,
                    pegged_change_amount, ref_change_amount, &ref_exchange, true));
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                send_new_order(conn, context, instrument, &refs)
            }
            OrderRequest::SubmitLimitAuc { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'8', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),           // OrdType = Limit
                    (44, &price_str),    // Limit price
                    (59, "8"),           // TIF = Auction
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitMtlAuc { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'K', b'8', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "K"),           // OrdType = Market to Limit
                    (59, "8"),           // TIF = Auction
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            // Unwrapped above; a nested preview is dropped there.
            OrderRequest::SubmitWhatIf { .. } => Ok(()),
            OrderRequest::SubmitLimitFractional { order_id, instrument, side, qty, price } => {
                // The tracked quantity is fixed-point like `qty` (it was 0).
                let mut tracked = crate::types::Order::new(
                    order_id, instrument, side, 0, price, b'2', b'0', 0,
                );
                tracked.qty_fixed = qty;
                context.insert_order(tracked);
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_qty(qty);
                let price_str = format_price_ref(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),      // Decimal qty (e.g., "0.5")
                    (40, "2"),           // OrdType = Limit
                    (44, &price_str),
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitAdjustableStop { order_id, instrument, side, qty,
                stop_price, trigger_price, adjusted_order_type,
                adjusted_stop_price, adjusted_stop_limit_price,
                adjusted_trailing_amount, adjustable_trailing_unit } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'3', b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price_ref(stop_price);
                let adjustable = adjustable_stop_tags(trigger_price, adjusted_order_type,
                    adjusted_stop_price, adjusted_stop_limit_price,
                    adjusted_trailing_amount, adjustable_trailing_unit);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),              // OrdType = Stop
                    (99, &stop_str),        // StopPx
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                fields.extend(adjustable.iter().map(|(t, s)| (*t, s.as_str())));
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitMtl { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'K', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "K"),          // OrdType = Market to Limit
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitMktPrt { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'U', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "U"),          // OrdType = Market with Protection
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_STP_PRT, b'0', stop_price,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price_ref(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SP"),         // OrdType = Stop with Protection
                    (99, &stop_str),    // StopPx
                    (6117, &stop_str), // stop trigger, as the reference (ibx#466)
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price_cap, crate::types::ORD_MIDPX, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                // The contract's own routing, as the reference (captured
                // 25/09/2026 on SMART, ibx#414).
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "MIDPX"),      // OrdType = Mid-Price
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ];
                let cap_str;
                if price_cap > 0 {
                    cap_str = format_price_ref(price_cap);
                    fields.push((44, &cap_str)); // Price cap
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MKT, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                // The offset in both price fields, 0.00 when unset, and the
                // contract's own routing, as the reference (ibx#413).
                let offset_str = format_price_ref(offset);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (99, &offset_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SMKT"),       // OrdType = Snap to Market
                    (211, &offset_str),
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitSnapMid { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MID, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                // The offset in both price fields, 0.00 when unset, and the
                // contract's own routing, as the reference (ibx#413).
                let offset_str = format_price_ref(offset);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (99, &offset_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SMID"),       // OrdType = Snap to Midpoint
                    (211, &offset_str),
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitSnapPri { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_PRI, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", context.server_id(order_id), ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                // The offset in both price fields, 0.00 when unset, and the
                // contract's own routing, as the reference (ibx#413).
                let offset_str = format_price_ref(offset);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (99, &offset_str),
                    (1, account_id),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SREL"),       // OrdType = Snap to Primary
                    (211, &offset_str),
                    (59, "0"),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, currency.as_str()),
                ])
            }
            OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, price, offset }
            | OrderRequest::SubmitPegMid { order_id, instrument, side, qty, price, offset } => {
                let mid = matches!(order_req, OrderRequest::SubmitPegMid { .. });
                let ord_type = if mid { crate::types::ORD_PEG_MID } else { crate::types::ORD_PEG_MKT };
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, ord_type, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp().to_string();
                let mut fields: Vec<(u32, String)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
                    (fix::TAG_SENDING_TIME, now.clone()),
                    (11, format!("{}.{}", context.server_id(order_id), ver)),
                ];
                let (price_tags, type_tags) = pegged_tags(mid, price, offset);
                fields.extend(price_tags);
                fields.extend([
                    (1, account_id.to_string()),
                    (55, symbol),
                    (54, fix_side(side).to_string()),
                    (38, format_uint(qty as u64).to_string()),
                ]);
                fields.extend(type_tags);
                fields.extend([
                    (59, "0".to_string()),
                    (167, sec_type_str),
                    (100, destination.clone()),
                    (6210, destination),
                    (15, currency.clone()),
                ]);
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                send_new_order(conn, context, instrument, &refs)
            }
            OrderRequest::Cancel { order_id } => {
                let fields = cancel_fields(context, account_id, order_id, "SEL");
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                let result = conn.send_fix(&refs);
                if result.is_ok() {
                    synthesize_pending_cancel(context, shared, order_id);
                }
                result
            }
            OrderRequest::GlobalCancel => Ok(()),
            OrderRequest::CancelAll { instrument } => {
                let open_ids: Vec<OrderId> = context.open_orders_for(instrument)
                    .iter()
                    .map(|o| o.order_id)
                    .collect();
                let mut last_result = Ok(());
                for oid in open_ids {
                    let fields = cancel_fields(context, account_id, oid, "ALL");
                    let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                    last_result = conn.send_fix(&refs);
                    if last_result.is_ok() {
                        synthesize_pending_cancel(context, shared, oid);
                    }
                }
                last_result
            }
            OrderRequest::Modify { new_order_id: _, order_id, qty, kind, tif, attrs } => {
                let orig = context.order(order_id).copied();
                let prev_ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let new_ver = prev_ver + 1;
                context.modify_versions.insert(order_id, new_ver);
                // Under the server's id of the order: an order of another
                // session may be kept under its API order id.
                let server_id = context.server_id(order_id);
                let clord_str = format!("{}.{}", server_id, new_ver);
                // OrigClOrdID matches whatever the server last recorded for
                // this order (which may pre-date the versioned scheme — ibx#179).
                let orig_clord = context.last_clord.get(&order_id).cloned()
                    .unwrap_or_else(|| format!("{}.{}", server_id, prev_ver));
                // Pre-seed `last_clord` with what we're about to emit so a
                // subsequent cancel before the modify-ack still references the
                // right version.
                context.last_clord.insert(order_id, clord_str.clone());

                let side_str = orig.map(|o| fix_side(o.side)).unwrap_or("1");
                let symbol = orig.map(|o| context.market.symbol(o.instrument).to_string())
                    .unwrap_or_default();
                let (sec_type_str, _destination) = orig
                    .map(|o| context.market.order_routing(o.instrument))
                    .unwrap_or_else(|| ("STK".to_string(), "SMART".to_string()));
                let con_id_str = orig.and_then(|o| context.market.con_id(o.instrument))
                    .map(|c| c.to_string()).unwrap_or_default();
                // A TRAIL LIMIT set by its limit price restates the offset the
                // server reported; before any report, the reference computes
                // it from the stop price and the new limit price, and keeps it
                // (ib-agent#195, ibx#490).
                let trail_limit_offset = match context.trail_limit_reported.get_mut(&order_id) {
                    Some(r) => {
                        // Until the server reports again, the order has the
                        // stop price of the replace, as the reference's
                        // openOrder shows (ib-agent#195, ibx#491).
                        if let crate::types::OrderKind::TrailingStopLimit { trail_stop_price, .. }
                            | crate::types::OrderKind::TrailLit { trail_stop_price, .. } = kind {
                            if trail_stop_price > 0 { r.stop = trail_stop_price; }
                        }
                        Some(r.offset)
                    }
                    None => {
                        let computed = computed_trail_limit_offset(kind, orig.map(|o| o.side));
                        if let (Some(offset), crate::types::OrderKind::TrailingStopLimit { lmt_price: Some(price), trail_stop_price, .. }
                            | crate::types::OrderKind::TrailLit { price, trail_stop_price, .. }) = (computed, kind) {
                            context.trail_limit_reported.insert(order_id, crate::engine::context::TrailLimitReported {
                                offset, limit: price, stop: trail_stop_price,
                            });
                        }
                        computed
                    }
                };
                let bracket_key = context.bracket_keys.get(&order_id).map(|k| k.to_string());
                let reported_stop = context.reported_stop.get(&order_id).copied();
                let mut fields = modify_fields(
                    &clord_str, &orig_clord, account_id, qty, side_str, &symbol,
                    &sec_type_str, &con_id_str, kind, tif, &attrs, trail_limit_offset,
                    reported_stop, bracket_key.as_deref(), context.price_mgmt_send);
                // A modification restates the order's MiFID values; only a
                // new, non-empty Decision Maker replaces the old one
                // (ibx#500, as the reference 10.51; not observed yet).
                if context.mifid_manager && let Some((values, covered)) = context.mifid_orders.get_mut(&order_id) {
                    if !attrs.mifid[0].is_empty() {
                        values[0] = attrs.mifid[0].clone();
                    }
                    let tags = mifid_tags(values, *covered);
                    let at = fields.iter().position(|(t, _)| *t == 6122).unwrap_or(fields.len());
                    fields.splice(at..at, tags);
                }
                // A quantity the fixed point cannot hold, as the caller
                // wrote it (ibx#655).
                if !attrs.qty_text.is_empty()
                    && let Some(field) = fields.iter_mut().find(|(t, _)| *t == 38)
                {
                    field.1 = attrs.qty_text.clone();
                }
                // The FA allocation, restated on every modification
                // (ibx#507; not observed yet).
                if context.fa_session {
                    let at = fields.iter().position(|(t, _)| *t == 6122).unwrap_or(fields.len());
                    fa_fields(&mut fields, at, &attrs.fa, &context.fa);
                }
                // The order's soft dollar tier, restated (ibx#506; not
                // observed yet).
                if let Some(tags) = context.soft_dollar_orders.get(&order_id) {
                    let at = fields.iter().position(|(t, _)| *t == 6122).unwrap_or(fields.len());
                    fields.splice(at..at, tags.iter().cloned());
                }
                if let Some(combo) = context.combos.orders.get(&order_id) {
                    // The per-leg prices of the modify, as 10.51 sends them;
                    // the 10.40 form restates the placed ones (ibx#604).
                    let new_prices = attrs.combo.as_ref().filter(|_| !shared.reference.form_10_40())
                        .map(|c| c.leg_prices.as_slice());
                    combo_modify_fields(&mut fields, combo, new_prices);
                }
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                conn.send_fix(&refs)
            }
        };
        // The presets the order's type saves, after its message (ibx#729).
        send_preset_saves(context, conn, hb);
        context.short_sale_send = None;
        context.combo_send = None;
        context.scale_send = None;
        if let Some((combo_order, view)) = combo_track && result.is_ok() {
            shared.orders.set_combo_view(oid, view);
            if !what_if { context.combos.track(oid, combo_order); }
        }
        if let Some((order, version, peak)) = held {
            let clord = context.what_if_send.take().unwrap_or_default();
            match order {
                Some(o) => context.insert_order(o),
                None => context.remove_order(oid),
            }
            context.book_peak = peak;
            if version.is_none() { context.modify_versions.remove(&oid); }
            if let Err(e) = &result {
                log::error!("Failed to send the what-if of order {}: {}", oid, e);
                context.what_ifs.remove(&clord);
                continue;
            }
        }
        match result {
            Ok(()) => hb.last_ccp_sent = Instant::now(),
            Err(e) => {
                // Order failed to send — remove from engine state and notify the application.
                // See: https://github.com/deepentropy/ibx/issues/116
                log::error!("Failed to send order {}: {} — notifying application", oid, e);
                if oid != 0 {
                    context.drop_unsent_order(oid);
                    shared.orders.push_order_update(OrderUpdate {
                        avg_fill_price: 0,
                        order_id: oid,
                        instrument: 0,
                        status: OrderStatus::Rejected,
                        filled_qty_fixed: 0,
                        remaining_qty_fixed: 0,
                        perm_id: 0,
                        parent_id: 0,
                        timestamp_ns: 0,
                    });
                }
            }
        }
    }
}

/// Send the frames of a combo set-up (ibx#470).
pub(crate) fn send_frames(conn: &mut Connection, hb: &mut HeartbeatState, frames: &[crate::engine::combo::Fields]) {
    for f in frames {
        let refs: Vec<(u32, &str)> = f.iter().map(|(t, v)| (*t, v.as_str())).collect();
        match conn.send_fix(&refs) {
            Ok(()) => hb.last_ccp_sent = Instant::now(),
            Err(e) => log::error!("Combo set-up request not sent: {}", e),
        }
    }
}

/// The answer of a combo set-up request (ibx#470): its next requests go
/// out; once the combo is built, or its set-up failed, the orders that
/// waited are handled again.
pub(crate) fn combo_progress(
    context: &mut Context,
    conn: &mut Option<Connection>,
    hb: &mut HeartbeatState,
    progress: crate::engine::combo::Progress,
) {
    if let Some(c) = conn.as_mut() {
        send_frames(c, hb, &progress.send);
    }
    if progress.release {
        release_rth_parked(context);
    }
}

/// A built combo order before it is encoded (ibx#470): it goes out on the
/// BAG's conId (a directed combo's is known only now), its outside-RTH
/// rule reads the BAG definition, and the encoder gets the combo's
/// symbol, block and per-leg prices. Returns what is kept once it is sent.
fn combo_order_setup(
    context: &mut Context,
    shared: &Arc<SharedState>,
    req: &mut OrderRequest,
    combo: crate::engine::combo::Combo,
    spec: crate::types::ComboSpec,
) -> (crate::engine::combo::ComboOrder, crate::bridge::ComboView) {
    if let Some(instrument) = req.ex_instrument_mut()
        && context.market.con_id(*instrument) != Some(combo.bag_con_id)
        && let Some(id) = context.market.try_register(combo.bag_con_id)
    {
        context.market.set_symbol(id, combo.symbol());
        context.market.set_routing(id, "BAG", &spec.exchange);
        context.market.set_currency(id, &spec.currency);
        shared.market.set_instrument_count(context.market.count());
        *instrument = id;
    }
    if let Some(types) = context.combos.bag_types(&combo).cloned() {
        context.rth_types.entry((combo.bag_con_id, combo.routed.clone())).or_insert(types);
    }
    let leg_prices = combo.leg_prices(&spec);
    let price = combo.price_of_legs(&leg_prices);
    context.combo_send = Some(crate::engine::combo::ComboSend {
        symbol: combo.symbol(),
        block: combo.order_block(),
        leg_prices: leg_prices.clone(),
        price,
    });
    let view = crate::bridge::ComboView {
        contract: combo.api_contract(&spec),
        leg_prices: if leg_prices.is_empty() {
            vec![f64::MAX; combo.legs.len()]
        } else {
            leg_prices.iter().map(|p| *p as f64 / crate::types::PRICE_SCALE as f64).collect()
        },
    };
    let order = crate::engine::combo::ComboOrder {
        combo, spec, leg_prices, price,
        cum_qty: 0, leaves_qty: 0, avg_price: 0, last_price: 0,
    };
    (order, view)
}

/// A cancel as the reference writes it (ibx#464; captured 25/09/2026):
/// the order's next ClOrdID version, the original ClOrdID the server has on
/// record, then quantity, side, account, contract and the cancel scope: `SEL` for a
/// cancel of one order, `ALL` for the global cancel. The new version and the
/// cancel's id are recorded, so its reports are read as the cancel's.
fn cancel_fields(context: &mut Context, account_id: &str, order_id: crate::types::OrderId, scope: &str) -> Vec<(u32, String)> {
    let prev_ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
    let new_ver = prev_ver + 1;
    context.modify_versions.insert(order_id, new_ver);
    // The next version of the order's ClOrdID, under the server's id of
    // the order (as the reference builds it): an order of another session is kept
    // under its API order id, and its version is the one the server gave.
    let server_id = context.server_id(order_id);
    let clord = format!("{}.{}", server_id, new_ver);
    // OrigClOrdID must match exactly what the server has on record: the
    // string last seen on the wire (ibx#179 — orders recorded without a
    // `.{ver}` suffix), else the versioned scheme (a cancel right after
    // the place, before its ack).
    let orig_clord = context.last_clord.get(&order_id).cloned()
        .unwrap_or_else(|| format!("{}.{}", server_id, prev_ver));
    context.cancel_clord.insert(order_id, clord.clone());

    let mut fields = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL.to_string()),
        (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
        (11, clord),
        (41, orig_clord),
    ];
    if let Some(order) = context.order(order_id).copied() {
        fields.push((38, format_qty(order.qty_fixed).to_string()));
        fields.push((54, fix_side(order.side).to_string()));
        fields.push((1, account_id.to_string()));
        if let Some(con_id) = context.market.con_id(order.instrument) {
            fields.push((6008, con_id.to_string()));
        }
    } else {
        fields.push((1, account_id.to_string()));
    }
    fields.push((6088, "Socket".to_string()));
    fields.push((6944, scope.to_string()));
    fields
}

/// The reference's global cancel (API message 58):
/// - the orders of this client that wait to be sent end as ApiCancelled,
///   nothing on the wire (the client's pending orders; captured
///   25/09/2026: orderStatus ApiCancelled, permId 0, all remaining); the
///   requests about them go with them;
/// - every order of the book, whatever its client or session (no client
///   filter), gets a cancel tagged ALL, in the book's order with the orders
///   that have a parent last; only the first order of
///   an OCA group, and not a child whose parent goes in this cancel;
/// - an order with a cancel or a replace pending is not sent: 161 to its
///   client; so is an ended order the
///   book still holds ([`crate::engine::context::EndedOrder`], ibx#545).
///
/// Captured 01/10/2026 (17:11:41, paper): 8 orders of earlier sessions,
/// known from the logon replay, cancelled in one write, in the book's
/// order.
fn global_cancel(
    conn: &mut Connection,
    context: &mut Context,
    account_id: &str,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) {
    // The orders that were never sent (held with transmit off, ibx#509)
    // are in the book with the others (ibx#595): they end in its walk, as
    // their own cancel ends them (paper 09/10/2026).
    let unsent: Vec<OrderId> = context.unsent.keys().copied().collect();
    let held: Vec<(OrderId, crate::engine::context::BookEntry)> = unsent.iter().map(|&id| {
        let entry = context.held_book.get(&id).cloned().unwrap_or_else(|| crate::engine::context::BookEntry {
            parent: context.unsent.get(&id).map_or(0, |h| h.2), ..Default::default()
        });
        (id, entry)
    }).collect();
    // The orders that wait (a what-if is not an order: it keeps waiting),
    // and those the reference kept pending.
    let parked = std::mem::take(&mut context.rth_parked);
    let mut ended: Vec<OrderId> = Vec::new();
    for req in &parked {
        if req.new_order_qty().is_some() && !matches!(req, OrderRequest::SubmitWhatIf { .. }) {
            ended.extend(api_cancelled(context, shared, req));
        }
    }
    context.rth_parked = parked.into_iter()
        .filter(|r| !request_order_ids(r).iter().any(|id| ended.contains(id)))
        .collect();
    let mut pending: Vec<(OrderId, OrderRequest)> = context.api_pending.drain().collect();
    pending.sort_by_key(|(id, _)| *id);
    for (_, req) in pending {
        api_cancelled(context, shared, &req);
    }

    // The book, in its order: the working orders, the ended ones the
    // reference still holds (ibx#545) and the held ones (ibx#595).
    #[derive(PartialEq)]
    enum Kind { Working, Ended, Held }
    let kept = context.ended_orders(Instant::now());
    let table = book_table_size(context.book_peak);
    let mut book: Vec<(OrderId, OrderId, crate::engine::context::BookEntry, Kind)> = context.book.iter()
        .filter(|(id, _)| context.order(**id).is_some())
        .map(|(&id, e)| (id, context.server_id(id), e.clone(), Kind::Working))
        .chain(kept.into_iter().map(|e| (e.order_id, e.server_id, e.entry, Kind::Ended)))
        .chain(held.into_iter().map(|(id, e)| (id, context.server_id(id), e, Kind::Held)))
        .collect();
    book.sort_by_key(|(_, server, e, _)| (book_bucket(*server, table), e.seq));
    book.sort_by_key(|(_, _, e, _)| e.parent != 0);

    // The first order of an OCA group met in the walk, whatever its state,
    // stands for its group: the others are passed over, and one held stays
    // held (ibx#595; paper 11/10 and 15/10/2026, held orders of one group:
    // the one met first ended, the other left held at each global cancel).
    let mut groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut taken: std::collections::HashSet<OrderId> = std::collections::HashSet::new();
    for (id, server, entry, kind) in book {
        if !entry.oca_group.is_empty() && !groups.insert(entry.oca_group.clone()) {
            continue;
        }
        if entry.parent != 0 && taken.contains(&entry.parent) {
            // A held child ends with its parent (not observed yet: ibx
            // keeps what it did before ibx#595).
            if kind == Kind::Held && end_unsent(context, shared, id) {
                taken.insert(id);
            }
            continue;
        }
        if kind == Kind::Held {
            if end_unsent(context, shared, id) {
                taken.insert(id);
            }
            continue;
        }
        // An ended order cannot be cancelled: 161 to its client, after
        // the callbacks of the orders this cancel ended. It does not
        // stand for its children: paper
        // 09/10/2026, a bracket cancelled 5 s before gave 161 for its
        // parent and for the first of its two children.
        if kind == Kind::Ended {
            if entry.owner.is_none_or(|owner| owner == context.api_client_id) {
                shared.orders.push_order_notice(id, 161, format!(
                    "Cancel attempted when order is not in a cancellable state.  Order permId ={}", server));
            }
            continue;
        }
        let Some(status) = context.order(id).map(|o| o.status) else { continue };
        if status.is_terminal() || status == OrderStatus::Inactive {
            continue;
        }
        taken.insert(id);
        if matches!(status, OrderStatus::PendingCancel | OrderStatus::PendingReplace) {
            log::warn!("Global cancel: order {} has a cancel or replace pending, not sent", id);
            if context.owned(id) {
                shared.orders.push_order_error(id, 161, format!(
                    "Cancel attempted when order is not in a cancellable state.  Order permId ={}", server));
            }
            continue;
        }
        let fields = cancel_fields(context, account_id, id, "ALL");
        let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
        match conn.send_fix(&refs) {
            Ok(()) => {
                hb.last_ccp_sent = Instant::now();
                synthesize_pending_cancel(context, shared, id);
            }
            Err(e) => log::error!("Global cancel: the cancel of order {} was not sent: {}", id, e),
        }
    }
}

/// The orders of a new-order request end as ApiCancelled: orderStatus
/// with permId 0, nothing filled and the whole quantity remaining, the
/// prices unset (as the reference writes it; captured 25/09/2026
/// and in i105_combo_directed of 26/09/2026). Their ids.
fn api_cancelled(context: &Context, shared: &Arc<SharedState>, req: &OrderRequest) -> Vec<OrderId> {
    let Some(qty) = req.new_order_qty() else { return Vec::new() };
    let bracket = matches!(req, OrderRequest::SubmitBracket { .. });
    let parent = match req {
        OrderRequest::SubmitBracket { parent_id, .. } => *parent_id,
        _ => req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id),
    };
    let ids = request_order_ids(req);
    for (i, &id) in ids.iter().enumerate() {
        log::info!("Order {} never left: ApiCancelled", id);
        shared.orders.push_order_update(OrderUpdate {
            order_id: id,
            instrument: req.instrument().unwrap_or(0),
            status: OrderStatus::ApiCancelled,
            filled_qty_fixed: 0,
            remaining_qty_fixed: qty,
            avg_fill_price: 0,
            perm_id: 0,
            parent_id: if bracket && i == 0 { 0 } else { parent },
            timestamp_ns: context.now_ns(),
        });
    }
    ids
}

/// A cancel of an order that never left: one the reference keeps pending
/// after a refusal, or a single new order that waits (a bracket waits
/// whole). It ends as ApiCancelled with the requests about it. False when
/// the order is not such an order.
fn api_cancel(context: &mut Context, shared: &Arc<SharedState>, order_id: OrderId) -> bool {
    if let Some(req) = context.api_pending.remove(&order_id) {
        api_cancelled(context, shared, &req);
        return true;
    }
    let waiting = context.rth_parked.iter().position(|r| {
        r.new_order_qty().is_some() && !matches!(r, OrderRequest::SubmitWhatIf { .. } | OrderRequest::SubmitBracket { .. })
            && r.order_id() == order_id
    });
    let Some(at) = waiting else { return false };
    let req = context.rth_parked.remove(at);
    api_cancelled(context, shared, &req);
    context.rth_parked.retain(|r| !request_order_ids(r).contains(&order_id));
    true
}

/// The reference's answer to a cancel it does not send (ibx#464): the order
/// is unknown (10147), or finished or already pending cancel (10148, with
/// the state; "cannot" as in every captured 10148, where the gateway's
/// message text says "can not", ibx#485). `None` when the cancel goes out.
///
/// A cancelled order and an order the server refused are known for the
/// time the reference keeps them (ibx#551, paper 11/10/2026, market
/// closed): 10148 "state: Cancelled" for both at 3 s and 8 s after their
/// end, 10147 at 12 s and 19 s. A filled order likewise while it is kept
/// (paper 12/10/2026, regular hours: 10148 "state: Filled" at 5, 10, 14
/// and 24 s after the fill, 10147 at 40 and 70 s).
fn cancel_refusal(context: &Context, order_id: crate::types::OrderId) -> Option<(i64, String)> {
    let not_found = || Some((
        10147,
        format!("OrderId {} that needs to be cancelled is not found.", order_id),
    ));
    let state = match context.order(order_id) {
        // An order the server reported for another client: the reference
        // finds an API order by this client's id and the order id
        //, so not this one
        // (ib-agent#151: an order of another client gets 10147).
        Some(_) if !context.owned(order_id) => return not_found(),
        Some(o) if o.status == OrderStatus::PendingCancel => OrderStatus::PendingCancel,
        Some(_) => return None,
        None => match context.finished_status(order_id) {
            Some(OrderStatus::Cancelled | OrderStatus::Rejected) => {
                if !context.ended_is_kept(order_id, Instant::now()) {
                    return not_found();
                }
                OrderStatus::Cancelled
            }
            Some(OrderStatus::Filled) => {
                if !context.ended_is_kept(order_id, Instant::now()) {
                    return not_found();
                }
                OrderStatus::Filled
            }
            Some(status) => status,
            None => return not_found(),
        },
    };
    // "cannot" as the reference's API message (every capture, 23/09 to
    // 02/10/2026; the catalog text says "can not", ibx#486).
    Some((
        10148,
        format!(
            "OrderId {} that needs to be cancelled cannot be cancelled, state: {}.",
            order_id, crate::client_core::order_status_str(state),
        ),
    ))
}

/// The rule texts of the reference's short-side checks (ibx#417), sent as
/// the cause of a 321 refusal.
const INVALID_SIDE: &str = "Invalid side field was entered";
const NOT_INSTITUTIONAL: &str = "Not an institutional account, or an away clearing order";
const BAD_SHORT_SLOT: &str = "Short sale slot value must be 1 (broker holds shares) or 2 (delivered from elsewhere)";
const NOT_SHORT_SALE_EXEMPT: &str = "Order not a short sale exempt -- type must be SSHORTX to specify short sale slot.";
const SLOT_2_NEEDS_LOCATION: &str = "Short sale slot value of 2 (delivered from elsewhere) requires location.";
const SLOT_1_NO_LOCATION: &str = "Short sale slot value of 1 requires no location be specified.";

/// The clearing of an order is away from the broker (ibx#417): an Away or
/// PTA intent. An order without one takes Away for a super user, for an
/// account id that starts with T, and for one with G as its second or
/// third character; else the broker's own clearing, as the reference.
pub(crate) fn clearing_away(intent: &str, account_id: &str, super_user: bool) -> bool {
    if !intent.is_empty() {
        return !intent.eq_ignore_ascii_case("IB");
    }
    let id = account_id.as_bytes();
    super_user || id.first() == Some(&b'T') || id.get(1) == Some(&b'G') || id.get(2) == Some(&b'G')
}

/// The reference's checks of a short-side order (ibx#417), in its order:
/// the side is refused unless the logon is a super user or omnibus logon,
/// or the order clears away; then the short-sale slot rules. Ok with the
/// short-sale instructions the order carries, or the rule text.
pub(crate) fn short_sale_check(
    attrs: &crate::types::OrderAttrs,
    account_id: &str,
    super_user: bool,
    omnibus: bool,
) -> Result<crate::types::ShortSale, &'static str> {
    let away = clearing_away(&attrs.clearing_intent, account_id, super_user);
    if !(super_user || omnibus || away) {
        return Err(INVALID_SIDE);
    }
    let s = &attrs.short_sale;
    if !super_user && !away {
        // An omnibus logon only: no slot and no location may be given,
        // and then no other slot rule applies.
        if s.slot != 0 || !s.location.is_empty() {
            return Err(NOT_INSTITUTIONAL);
        }
        return Ok(s.clone());
    }
    if s.exempt_reason_given() {
        return Err(NOT_SHORT_SALE_EXEMPT);
    }
    if !matches!(s.slot, 1 | 2) {
        return Err(BAD_SHORT_SLOT);
    }
    if s.slot == 1 && !s.location.is_empty() {
        return Err(SLOT_1_NO_LOCATION);
    }
    if s.slot == 2 && s.location.is_empty() {
        return Err(SLOT_2_NEEDS_LOCATION);
    }
    Ok(s.clone())
}

/// The short-sale fields of a short-side order, as the reference writes
/// them (ibx#417): the located flag (Y when the shares are at one of the
/// two broker locations), the location for slot 2, the slot, and the
/// exempt code when it names a reason.
fn short_sale_fields(s: &crate::types::ShortSale) -> Fields {
    let located = matches!(s.location.as_str(), "IBKR" | "TMBR");
    let mut fields = vec![(114, if located { "Y" } else { "N" }.to_string())];
    if s.slot == 2 {
        fields.push((5700, s.location.clone()));
    }
    fields.push((6086, s.slot.to_string()));
    if s.exempt_reason_given() {
        fields.push((1688, s.exempt_code.to_string()));
    }
    fields
}

/// Convert Side to the wire side code.
fn fix_side(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
        Side::ShortSell => "5",
    }
}

/// The PendingCancel phase when a cancel request goes out (ibx#211): the
/// server acks a normal cancel with the terminal code only, so the order is
/// set PendingCancel here and the server's next report shows it. No
/// callback at the cancel itself: the reference gives none until a report
/// comes (every cancel of the four-leg recordings of 26/09 to 02/10/2026,
/// ibx#486). A cancel reject restores the working status via the forced
/// setter.
fn synthesize_pending_cancel(
    context: &mut Context,
    _shared: &Arc<SharedState>,
    order_id: crate::types::OrderId,
) {
    // Unknown order, already terminal, or already pending-cancel: no change.
    let _ = context.update_order_status(order_id, OrderStatus::PendingCancel);
}

/// OCA type of a bracket child with no type given: the default type, as the
/// reference sends it for a bracket whose children leave the type unset
/// (ib-agent captures/pd-orders, 01/10/2026). A child whose type differs
/// from its siblings' is refused by the server as a mismatch (ibx#311).
const BRACKET_CHILD_OCA_TYPE: &str = "ReduceOnFillNonBlock";

/// The parent link and OCA group of a bracket child (ibx#311, ibx#329):
/// the parent's current order id, with the version of its last replace,
/// and the id part of it as the OCA group, as the reference does. The
/// current id is the one the server holds for the parent, else the
/// versioned form ibx sends.
fn bracket_parent_link(context: &Context, parent_id: crate::types::OrderId) -> (String, String) {
    let link = context.last_clord.get(&parent_id).cloned().unwrap_or_else(|| {
        format!("{}.{}", context.server_id(parent_id), context.modify_versions.get(&parent_id).copied().unwrap_or(0))
    });
    let group = link.split('.').next().unwrap_or(&link).to_string();
    (link, group)
}

/// The bracket key of a new child order (ibx#248): the parent's group with
/// the parent's next child index, the parent getting its key when it has
/// none. A preview takes no key.
fn push_bracket_key(
    fields: &mut Vec<(u32, String)>,
    context: &mut Context,
    order_id: crate::types::OrderId,
    attrs: &crate::types::OrderAttrs,
) {
    if attrs.parent_id <= 0 || context.what_if_send.is_some() { return; }
    let key = context.brackets().attach_child(attrs.parent_id, order_id);
    fields.push((6531, key.to_string()));
}

/// Map the OCA type code (1..=4) to its wire label. 0/unset and
/// out-of-range coerce to 3 (ReduceOnFillNonBlock), the gateway default
/// (ibx#215).
fn oca_type_str(oca_type: u8) -> &'static str {
    match oca_type {
        1 => "CancelOnFillWBlock",
        2 => "ReduceOnFillWBlock",
        4 => "ReduceOnFillWBlockFromTotal",
        _ => "ReduceOnFillNonBlock",
    }
}

/// Send a new order as the reference writes it: every field in the place
/// its single new-order writer gives it (ibx#375), whatever encoder built
/// the list. The contract id goes with the routing fields on every new
/// order (ib-agent#192 B4, ibx#328); an instrument registered without one
/// keeps the symbol-only form. Every new order carries the fields the
/// reference adds to all of them: the origin, the API order and client
/// ids, the contract multiplier, the source and the two empty trailing
/// fields (ibx#466). A short-side order adds its short-sale fields
/// (ibx#417); a what-if goes out under its own order id with the preview
/// flag and without the OCA fields (ibx#462).
fn send_new_order(
    conn: &mut Connection,
    context: &mut Context,
    instrument: crate::types::InstrumentId,
    fields: &[(u32, &str)],
) -> std::io::Result<()> {
    // The order's parent and OCA group, as the reference's book holds
    // them for its global cancel; a what-if is not in the book.
    let (link, oca_group) = (field(fields, 6107), field(fields, 583));
    if (link.is_some() || oca_group.is_some()) && context.what_if_send.is_none() {
        let id_of = |v: &str| v.split('.').next().and_then(|i| i.parse::<crate::types::OrderId>().ok());
        if let Some(oid) = field(fields, 11).and_then(id_of) {
            let oid = context.key_of(oid);
            let parent = link.and_then(id_of).map(|p| context.key_of(p));
            context.set_links(oid, parent, oca_group);
        }
    }
    // A scale order's profit taker: its own order, ids and bracket key,
    // before the parent's message names its key (ibx#751).
    let scale = context.scale_send.clone();
    let profit = scale.as_ref().and_then(|s| s.profit_child_id.map(|c| (c, s.profit_offset.unwrap_or(0))));
    let parent_oid = field(fields, 11).and_then(|v| v.split('.').next()).and_then(|i| i.parse::<crate::types::OrderId>().ok())
        .map(|s| context.key_of(s));
    let scale_keys = match (profit, parent_oid) {
        (Some((child, _)), Some(parent)) => {
            context.assign_server_id(child);
            let child_key = context.brackets().attach_child(parent, child);
            let parent_key = context.bracket_keys.get(&parent).copied();
            parent_key.map(|p| (p.to_string(), child_key.to_string(), format!("{}.0", context.server_id(child))))
        }
        _ => None,
    };
    if let (Some((child, offset)), Some(parent)) = (profit, parent_oid)
        && let Some(order) = context.order(parent).copied()
    {
        let side = if order.side == crate::types::Side::Buy { crate::types::Side::Sell } else { crate::types::Side::Buy };
        let price = if order.side == crate::types::Side::Buy { order.price + offset } else { order.price - offset };
        let qty = scale.as_ref().map_or(order.qty_fixed, |s| s.total_qty) / crate::types::QTY_SCALE;
        context.insert_order(crate::types::Order::new(child, instrument, side, qty as u32, price, b'2', order.tif, 0));
        // Its reports name its parent, also the reject that carries no link.
        context.set_links(child, Some(parent), None);
    }
    // A scale order with an initial position is held, and reported, with
    // the quantity it goes out with: its whole quantity less the position
    // (ibx#751; the reference 10.51, paper, 18/10/2026: 4 less 5, -1).
    if let (Some(s), Some(parent)) = (scale.as_ref(), parent_oid)
        && let Some(position) = s.init_position
    {
        context.set_order_qty(parent, s.total_qty - i64::from(position) * crate::types::QTY_SCALE);
    }
    // The price management flag goes out below on the same condition:
    // remembered for the order's reports that leave it out (ibx#605).
    if context.price_mgmt_send && !crate::engine::price_mgmt::excluded_frame(fields)
        && let Some(oid) = field(fields, 11).and_then(|v| v.split('.').next()).and_then(|i| i.parse::<crate::types::OrderId>().ok())
    {
        let key = context.key_of(oid);
        context.price_mgmt_sent.insert(key);
    }
    let context: &Context = context;
    let short_sale: Fields = match &context.short_sale_send {
        Some(s) if fields.iter().any(|&(t, v)| t == 54 && v == "5") => short_sale_fields(s),
        _ => Vec::new(),
    };
    let con_id = context.market.con_id(instrument).unwrap_or(0);
    let con_id_str = if con_id > 0 { con_id.to_string() } else { String::new() };
    let what_if = context.what_if_send.as_deref();
    // The API order id, the key of the order the ClOrdID names; it is an
    // int in the API, so a larger id (an engine order) is left out.
    let order_id = fields.iter().find(|&&(t, _)| t == 11)
        .and_then(|&(_, v)| v.split('.').next())
        .and_then(|id| id.parse::<crate::types::OrderId>().ok())
        .map(|server| context.key_of(server))
        .filter(|&id| i32::try_from(id).is_ok_and(|n| n >= 0))
        .map(|id| id.to_string());
    let client_id = context.api_client_id.to_string();
    let stock = fields.iter().any(|&(t, v)| t == 167 && v == "STK");
    // A future, a CFD, a forex pair, an option, a futures option or a
    // warrant: its contract month, right and strike, local symbol and
    // multiplier from its definition, and a forex quantity with two
    // decimals (the reference 10.51, paper: MES, AAPL CFD, EUR.USD on
    // 13/10/2026, a SAP warrant on 15/10/2026; ibx#666). An option's and a
    // futures option's message is written as the warrant's: not observed,
    // the paper account has no permission for them.
    let sec_type = field(fields, 167).unwrap_or("");
    // A structured product is written as a warrant, a treasury bill as a
    // metal (the reference 10.51, paper, 17/10/2026: a DAX turbo on SBF,
    // a bill on SMART).
    let option_like = matches!(sec_type, "OPT" | "FOP" | "WAR" | "IOPT");
    let terms = (matches!(sec_type, "FUT" | "CFD" | "CASH" | "CMDTY" | "FUND" | "BILL") || option_like).then(|| {
        let (_, destination) = context.market.order_routing(instrument);
        context.rth_types.get(&(con_id, destination))
    }).flatten();
    let cash_qty = (sec_type == "CASH").then(|| field(fields, 38).map(|q| format!("{q}.00"))).flatten();
    // An order for an algo provider's destination goes through SMART with
    // the provider as its second destination, as the reference (ibx#505;
    // observed 07/10 and 13/10/2026, AAPL for FOXRIVER).
    let algo_destination = field(fields, 100)
        .is_some_and(|d| context.algo_providers.iter().any(|p| p.eq_ignore_ascii_case(d)));
    // A limit order at a price of 0 goes without its price, as the
    // reference writes it (ibx#688; the reference 10.51, paper, 17/10/2026,
    // market closed: an AAPL limit order at 0, the server's answer naming
    // the missing price). A combo keeps its price, which may be 0.
    let zero_limit = field(fields, 40) == Some("2") && context.combo_send.is_none();
    let mut out: Vec<(u32, &str)> = Vec::with_capacity(fields.len() + short_sale.len() + 10);
    for &(tag, value) in fields {
        match (tag, what_if) {
            (44, _) if zero_limit && value.parse::<f64>().is_ok_and(|p| p == 0.0) => {}
            (100, _) if algo_destination => out.push((100, "BEST")),
            (38, _) if cash_qty.is_some() => out.push((38, cash_qty.as_deref().unwrap_or(value))),
            // A fund order goes with a quantity of 1, its amount in the cash
            // quantity (the reference 10.51, paper, 16/10/2026: a VFIAX buy
            // of 100.00 with a quantity of 0 given; ibx#666).
            (38, _) if sec_type == "FUND" => out.push((38, "1")),
            (11, Some(clord)) => out.push((11, clord)),
            (583 | 6209, Some(_)) => {}
            _ => out.push((tag, value)),
        }
    }
    out.extend(short_sale.iter().map(|(t, v)| (*t, v.as_str())));
    // The default trigger method on the stop types, when the order did not
    // carry its own (ibx#263).
    if !out.iter().any(|&(t, _)| t == 6115) && has_trigger_method(&out) { out.push((6115, "0")); }
    // The trail unit of a trailing type, amount (0) when the order did not
    // carry percent (ibx#263).
    if !out.iter().any(|&(t, _)| t == 6268) && trailing_type(&out) { out.push((6268, "0")); }
    // The stop price in the stop trigger field: the stop trigger of every non-trailing stop
    // type (the adjustable stop included,
    // captured ib-agent#192 B11), the touched trigger of MIT and LIT
    // (ibx#263).
    let stop_trigger = (touched_type(&out) || matches!(field(&out, 40), Some("3" | "4" | "SP")))
        .then(|| field(&out, 99)).flatten();
    if let Some(v) = stop_trigger.filter(|_| !out.iter().any(|&(t, _)| t == 6117)) { out.push((6117, v)); }
    if con_id > 0 { out.push((6008, &con_id_str)); }
    // A combo's block after its conId (ibx#470).
    if let Some(combo) = &context.combo_send {
        out.extend(combo.block.iter().map(|(t, v)| (*t, v.as_str())));
    }
    if what_if.is_some() { out.push((6091, "1")); }
    out.push((6122, "c"));
    // The price management flag, decided for the request before encoding;
    // never on the order types it does not go with (ibx#492).
    if context.price_mgmt_send && !crate::engine::price_mgmt::excluded_frame(&out) {
        out.push((8339, "1"));
    }
    if let Some(id) = &order_id { out.push((6121, id)); }
    out.push((6119, &client_id));
    // A stock's multiplier, as the reference writes it; an option's terms.
    if stock { out.push((231, "1.00")); }
    if let Some(t) = terms {
        if (sec_type == "FUT" || option_like) && !t.contract_month.is_empty() { out.push((200, t.contract_month.as_str())); }
        if option_like && !t.right.is_empty() { out.extend([(201, t.right.as_str()), (202, t.strike.as_str())]); }
        if !t.local_symbol.is_empty() { out.extend([(48, t.local_symbol.as_str()), (22, "101")]); }
    }
    if let Some(t) = terms {
        out.push((231, if t.multiplier.is_empty() { "1.00" } else { t.multiplier.as_str() }));
    }
    out.push((6088, "Socket"));
    out.push((6211, ""));
    out.push((6238, ""));
    // An order for the broker's ATS venue goes to BEST with the venue mark
    // (the reference 10.51, paper, pre-market 14/10/2026; ibx#690).
    if context.market.exchange(instrument) == "IBKRATS" { out.push((8032, "1")); }
    in_reference_order(&mut out);
    let Some(scale) = scale else {
        let result = conn.send_fix(&out);
        // When each new order leaves, for a comparison of timings (ibx#548).
        log::debug!("New order sent: order {}", order_id.as_deref().unwrap_or("-"));
        return result;
    };
    let mut owned: Vec<(u32, String)> = out.iter().map(|(t, v)| (*t, v.to_string())).collect();
    let child = scale_fields(&mut owned, &scale, scale_keys.as_ref().map(|k| k.0.as_str()));
    let refs: Vec<(u32, &str)> = owned.iter().map(|(t, v)| (*t, v.as_str())).collect();
    // The order and its profit taker leave in one write, as the reference
    // writes them: written apart, an order that trades at once had filled
    // before the server read its profit taker, which it then refused
    // ("Parent order is partially or fully filled"; paper, regular hours
    // 12/11/2026, ibx#751).
    let pair = child.is_some() && !conn.holding();
    if pair {
        conn.hold();
    }
    let mut result = conn.send_fix(&refs);
    log::debug!("New scale order sent: order {}", order_id.as_deref().unwrap_or("-"));
    if let (Some(mut child), Some((_, child_key, child_clord)), Some(child_id)) = (child, scale_keys, scale.profit_child_id) {
        let parent_clord = field(&refs, 11).unwrap_or("").to_string();
        profit_child_fields(&mut child, &scale, &child_key, &child_clord, child_id, &parent_clord);
        let refs: Vec<(u32, &str)> = child.iter().map(|(t, v)| (*t, v.as_str())).collect();
        let sent = conn.send_fix(&refs);
        log::debug!("Profit taker sent: order {}", child_id);
        result = result.and(sent);
    }
    if pair {
        result = result.and(conn.release());
    }
    result
}

/// The scale fields of a new order, as the reference 10.51 writes them
/// (ibx#751; paper, 18/10/2026, market closed, twelve scale orders): after
/// the account, the price management flag, the bracket key of a profit
/// taker, the price increment, the price adjustment value, the initial
/// level size, the adjustment interval, the initial position and filled
/// quantity, auto-reset, the size randomization (55), the order's whole
/// quantity, the origin, then the subsequent level size; the source is
/// the scale one, and the quantity sent is the whole quantity less the
/// initial position. Gives the frame the profit taker starts from.
fn scale_fields(out: &mut Vec<(u32, String)>, scale: &crate::types::Scale, parent_key: Option<&str>) -> Option<Vec<(u32, String)>> {
    let p = |v: crate::types::Price| format_price_ref(v).to_string();
    let base: Vec<(u32, String)> = out.clone();
    for (tag, value) in out.iter_mut() {
        if *tag == 6088 { *value = "Scale".to_string(); }
        if *tag == 38 && let Some(position) = scale.init_position {
            *value = (scale.total_qty / crate::types::QTY_SCALE - i64::from(position)).to_string();
        }
    }
    let price_mgmt = out.iter().any(|(t, _)| *t == 8339);
    out.retain(|(t, _)| !matches!(*t, 8339 | 6122 | 6531));
    let mut block: Vec<(u32, String)> = Vec::new();
    if price_mgmt { block.push((8339, "1".to_string())); }
    if let Some(key) = parent_key { block.push((6531, key.to_string())); }
    if let Some(v) = scale.price_increment { block.push((6405, p(v))); }
    if let Some(v) = scale.price_adjust_value { block.push((6527, p(v))); }
    block.push((6403, scale.init_level_size.to_string()));
    if let Some(v) = scale.price_adjust_interval { block.push((6526, v.to_string())); }
    if let Some(v) = scale.init_position { block.push((6485, v.to_string())); }
    if let Some(v) = scale.init_fill_qty { block.push((6486, v.to_string())); }
    if scale.auto_reset { block.push((6461, "1".to_string())); }
    if scale.random_percent { block.push((6795, "55.00".to_string())); }
    block.push((6534, (scale.total_qty / crate::types::QTY_SCALE).to_string()));
    block.push((6122, "c".to_string()));
    if let Some(v) = scale.subs_level_size { block.push((6445, v.to_string())); }
    let at = out.iter().position(|(t, _)| *t == 1).map_or(out.len(), |i| i + 1);
    out.splice(at..at, block);
    scale.profit_child_id.map(|_| base)
}

/// The profit taker of a scale order, as the reference 10.51 writes it with
/// its parent (ibx#751): the opposite side at the parent's price moved by
/// the offset, the parent's whole quantity, after the account its bracket
/// key, the offset, auto-cancel of the parent, auto-reset and the size
/// randomization as the parent's, the origin and the price management
/// flag; its negative API order id; the scale source and the parent link.
fn profit_child_fields(out: &mut Vec<(u32, String)>, scale: &crate::types::Scale, key: &str, clord: &str, child_id: crate::types::OrderId, parent_clord: &str) {
    let offset = scale.profit_offset.unwrap_or(0);
    let buy = out.iter().any(|(t, v)| *t == 54 && v == "1");
    for (tag, value) in out.iter_mut() {
        match *tag {
            11 => *value = clord.to_string(),
            44 => if let Ok(price) = value.parse::<f64>() {
                let moved = price + if buy { 1.0 } else { -1.0 } * offset as f64 / crate::types::PRICE_SCALE as f64;
                *value = format!("{:.2}", moved);
            },
            54 => *value = if buy { "2" } else { "1" }.to_string(),
            38 => *value = (scale.total_qty / crate::types::QTY_SCALE).to_string(),
            6121 => *value = child_id.to_string(),
            6088 => *value = "Scale".to_string(),
            _ => {}
        }
    }
    let price_mgmt = out.iter().any(|(t, _)| *t == 8339);
    out.retain(|(t, _)| !matches!(*t, 8339 | 6122 | 6531));
    if !out.iter().any(|(t, _)| *t == 6121) {
        let at = out.iter().position(|(t, _)| *t == 6119).unwrap_or(out.len());
        out.insert(at, (6121, child_id.to_string()));
    }
    let mut block: Vec<(u32, String)> = vec![
        (6531, key.to_string()),
        (6446, format_price_ref(offset).to_string()),
        (6965, "1".to_string()),
    ];
    if scale.auto_reset { block.push((6461, "1".to_string())); }
    if scale.random_percent { block.push((6795, "55.00".to_string())); }
    block.push((6122, "c".to_string()));
    if price_mgmt { block.push((8339, "1".to_string())); }
    let at = out.iter().position(|(t, _)| *t == 1).map_or(out.len(), |i| i + 1);
    out.splice(at..at, block);
    let at = out.iter().position(|(t, _)| *t == 6088).map_or(out.len(), |i| i + 1);
    out.insert(at, (6107, parent_clord.to_string()));
}

/// Whether a new order of this frame's type writes the trigger method,
/// as the reference (ibx#263): it writes it on a new order whose
/// type is a stop or touched type: STP,
/// STP LMT, STP PRT, TRAIL, TRAIL LIMIT, MIT,
/// LIT, TRAIL MIT and TRAIL LIT (captured on all but STP PRT), never on a
/// replace. A trailing stop is the pegged type with the trailing letter.
fn has_trigger_method(fields: &[(u32, &str)]) -> bool {
    matches!(field(fields, 40), Some("3" | "4" | "SP" | "J" | "LT")) || trailing_type(fields)
}

/// Whether the frame's type is a trailing one, which writes the trail
/// unit on a new order and a replace:
/// TRAIL, TRAIL LIMIT, TRAIL MIT, TRAIL LIT
/// (ibx#263; captured with the trail unit on all four, new orders and replaces).
fn trailing_type(fields: &[(u32, &str)]) -> bool {
    match field(fields, 40) {
        Some("TSL" | "TMIT" | "TLIT") => true,
        Some("P") => field(fields, 18).is_some_and(|v| v.split(' ').next() == Some("a")),
        _ => false,
    }
}

/// Whether the frame's type is a touched one, whose stop price the
/// reference keeps as the touched trigger and writes in the stop trigger field among the
/// order attributes: MIT and LIT (ibx#263, captured in
/// ib-agent#199 on new orders and replaces).
fn touched_type(fields: &[(u32, &str)]) -> bool {
    matches!(field(fields, 40), Some("J" | "LT"))
}

fn field<'a>(fields: &[(u32, &'a str)], tag: u32) -> Option<&'a str> {
    fields.iter().find(|&&(t, _)| t == tag).map(|&(_, v)| v)
}

/// Put new-order fields in the order of the reference's new-order writer
/// (ibx#375; its order checked against the
/// captured frames of ib-agent#192 B8 and B9). The sort is stable: the
/// pairs of a repeating group (algo parameters, conditions) share one
/// place and keep their order. The order attributes are written by the
/// reference in no fixed order; they get one fixed place each here, as
/// seen in captured frames where it could be.
fn in_reference_order(fields: &mut [(u32, &str)]) {
    // TRAIL MIT and TRAIL LIT write their trigger there too (ibx#469,
    // captured 28/09/2026).
    let touched = touched_type(fields) || matches!(field(fields, 40), Some("TMIT" | "TLIT"));
    fields.sort_by_key(|&(tag, _)| rank_in_frame(tag, touched));
    // With no other order attribute, the price management flag comes
    // before the origin field (the reference 10.51, paper, 13/10/2026: nine
    // orders; a plain limit order has the flag then the origin; with a
    // start time, an origin or the price-cap attribute, the origin comes
    // first and the flag after the other attributes).
    let attributes = fields.iter().any(|&(tag, _)| (73..95).contains(&rank_in_frame(tag, touched)));
    if !attributes
        && let (Some(c), Some(p)) = (fields.iter().position(|&(t, _)| t == 6122), fields.iter().position(|&(t, _)| t == 8339))
        && p > c
    {
        fields[c..=p].rotate_right(1);
    }
}

/// The place of a field, the touched trigger of MIT and LIT among the
/// order attributes (after outside-RTH, as captured in ib-agent#199), the
/// stop trigger of the other types before them.
fn rank_in_frame(tag: u32, touched: bool) -> u16 {
    if touched && tag == 6117 { 76 } else { reference_rank(tag) }
}

/// The place of a field in the reference's new-order writer (ibx#375).
pub(crate) fn reference_rank(tag: u32) -> u16 {
    match tag {
        fix::TAG_MSG_TYPE => 0,
        fix::TAG_SENDING_TIME => 1,
        11 => 10,
        // The per-leg prices of a combo follow the limit price (ibx#470).
        44 | 6879 => 20,
        99 => 21,
        1 => 30,
        // Not-held, right after the account (ibx#690).
        6287 => 31,
        126 | 432 => 32,
        // Short-sale fields.
        114 => 40,
        5700 => 41,
        6086 => 42,
        1688 => 43,
        // Adjustable stop.
        6257 => 50,
        6261 => 51,
        6258 => 52,
        6259 => 53,
        6262 => 54,
        6260 => 55,
        6117 => 60,
        // The hedge of a hedge order, after the account (ibx#614).
        6965 => 61,
        6690 => 62,
        6665 => 63,
        // Order attributes.
        583 => 70,
        6010 => 71,
        6122 => 72,
        // The bracket key, after the origin (ib-agent captures/pd-orders,
        // 01/10/2026).
        6531 => 73,
        // A hedge's beta or ratio, after the bracket key (ibx#614).
        6703 | 6666 => 74,
        // The smartComboRoutingParams of a combo (ibx#470; captured after
        // the origin and the orderRef).
        6248 | 6851 | 6852 | 6860 | 6861 | 6862 | 6866 | 6867 | 6876 | 6877 | 6878 => 74,
        6433 => 75,
        // Allow pre-open and ignore opening auction, where outside RTH
        // would be (ibx#607; the reference writes the attributes in no
        // fixed order: allow pre-open came after the origin in its frames).
        6524 | 6562 => 75,
        6115 => 76,
        6370 => 77,
        6268 => 78,
        6269 => 79,
        111 => 80,
        110 => 81,
        6135 => 82,
        8534 => 83,
        6207 => 84,
        6636 => 85,
        168 => 86,
        9813 => 87,
        6102 => 88,
        // Cash quantity.
        152 => 89,
        6941 => 90,
        6938 => 91,
        6939 => 92,
        6580 => 93,
        6942 => 94,
        // Post-only and deactivate, before the price management flag as
        // in the reference's frames of 12/10/2026 (ibx#607).
        6605 | 6521 => 94,
        // Seek price improvement, before the price management flag as in
        // the reference's frames of 12/10/2026; route marketable to BBO
        // with it (ibx#608).
        6557 | 8265 => 94,
        // The duration of a GTD order, before the price management flag as
        // in the reference's frame of 17/10/2026 (ibx#688).
        8402 => 94,
        // The price management flag, most often last of the attributes in
        // the reference's frames (captures of 28/09 and 01/10/2026).
        8339 => 95,
        // The compete attributes of pegged to best (ibx#469).
        8411 => 96,
        8412 => 97,
        // Algo: its strategy fields, then the parameter group.
        849 => 100,
        847 => 101,
        5957 | 5958 | 5960 => 102,
        6121 => 110,
        6119 => 111,
        38 => 120,
        40 => 121,
        211 => 122,
        18 => 123,
        // An option's maturity, right and strike follow the symbol.
        55 | 200 | 201 | 202 => 124,
        // The local symbol and its source after the security type (ibx#666).
        167 | 48 | 22 => 125,
        231 => 126,
        54 => 127,
        59 => 128,
        6436 => 129,
        // The venue mark of an order for the broker's ATS venue (ibx#690).
        8032 => 130,
        100 => 131,
        6210 => 132,
        // The combo block follows the conId (ibx#470).
        6008 | 6079 | 6080 | 6081 | 6082 | 6175 | 6134 => 133,
        6209 => 134,
        6088 => 135,
        6091 => 136,
        6107 => 137,
        6128 => 138,
        // The conditions' overnight flag, between the two other flags
        // (ibx#726, the reference 10.51, paper, 17/10/2026).
        8612 => 139,
        6151 => 140,
        // The condition group.
        6136 | 6222 | 6137 | 6126 | 6123 | 6124 | 6127 | 6125 | 6223 | 6245 | 6263 | 6246 | 6947 => 141,
        15 => 150,
        6211 => 152,
        6238 => 153,
        _ => u16::MAX,
    }
}

/// A beta hedge whose hedge sells and that has no max size (ibx#614; the
/// reference 10.51, paper, 15/10/2026, 0.23 s after the order): 10364 on
/// the hedge, nothing sent; the order is kept for its cancel, which ends it
/// as an order never sent (Cancelled, 202, 161), and its held parent stays
/// held.
///
/// The hedge of a parent the hedge-size precaution discards (ibx#602) is
/// kept the same way, without a message.
///
/// Such a hedge is kept with the quantity the reference gives an order
/// placed with none (`hedge_default_size`), not the parent's it is sent
/// with: its cancel gives Cancelled with that quantity remaining.
/// Some(true): kept; Some(false): its quantity waits for the order preset.
fn hedge_max_size_refusal(
    req: &OrderRequest, context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, shared: &Arc<SharedState>,
) -> Option<bool> {
    let (side, Some(attrs)) = req.new_order_side()? else { return None };
    let discarded = attrs.hedge_precaution == Some(false);
    if !discarded && !attrs.hedge.as_ref().is_some_and(|h| h.max_size_missing) {
        return None;
    }
    let instrument = req.instrument()?;
    let parent = attrs.parent_id;
    let oid = req.order_id();
    let Some(qty) = hedge_default_size(context, conn, hb, shared, instrument, side) else { return Some(false) };
    if discarded {
        log::warn!("Hedge order {} kept unsent: its parent is discarded (hedge-size precaution)", oid);
    } else {
        log::warn!("Hedge order {} refused: no max size", oid);
        shared.orders.push_order_error(oid, 10364, "Max Hedging Size value must be defined for the hedge order.".into());
    }
    context.keep_unsent(oid, (instrument, qty, parent));
    Some(true)
}

/// The quantity the reference gives a hedge placed with none, from the
/// account's position in the hedge's contract (ibx#748; the reference
/// 10.51, paper, 18/10, 19/10 and 12/11/2026, held parents with a beta
/// hedge kept unsent, whatever the parent's contract, size, price and side
/// and the beta): an order that closes the position has its size (a sell
/// with 74 SPY held: 74; with 8 then 14 AAPL: 8 then 14); one that adds to
/// it has the default size of the order preset of the contract's type, an
/// amount of money, over the position's price in the portfolio, to the
/// nearest share (200000 over MSFT at 521.92: 383; over IWM at 278.64:
/// 718), also for a position closed that day, whose row stays in the
/// portfolio with its price (AAPL at 354.6: 564); a contract that is not
/// in the portfolio, 0. None: the preset is asked, the order waits.
fn hedge_default_size(
    context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, shared: &Arc<SharedState>,
    instrument: crate::types::InstrumentId, side: Side,
) -> Option<crate::types::Qty> {
    let Some(position) = context.market.con_id(instrument).filter(|c| *c > 0).and_then(|c| shared.portfolio.position_info(c)) else {
        return Some(0);
    };
    let held = position.position_fixed;
    let sell = !matches!(side, Side::Buy);
    if held != 0 && (held > 0) == sell {
        return Some(held.abs());
    }
    let sec_type = if position.sec_type.is_empty() { "STK" } else { position.sec_type.as_str() };
    let kinds = preset_kinds(sec_type, context.market.symbol(instrument));
    if !ask_order_presets(context, conn, hb, &kinds) {
        return None;
    }
    let price = position.market_price as f64 / crate::types::PRICE_SCALE as f64;
    let amount = kinds.iter().find_map(|k| context.preset_amounts.get(k).copied());
    Some(match amount {
        Some(amount) if price > 0.0 => (amount / price).round() as crate::types::Qty * crate::types::QTY_SCALE,
        _ => 0,
    })
}

/// The parent of a beta hedge whose max size is below the hedge's size
/// (`ClientCore::note_hedge_precaution`, ibx#602; the reference 10.51,
/// paper, regular hours 16/10/2026: a parent of 10 AAPL, its hedge of beta
/// 1 with a max size of 2): 2185 on the parent, then orderStatus Cancelled
/// with nothing filled and a permId, then 201 "Order was discarded.";
/// nothing sent. Its hedge is kept unsent for its cancel.
fn hedge_size_precaution(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    let Some((_, Some(attrs))) = req.new_order_side() else { return false };
    if attrs.hedge_precaution != Some(true) { return false; }
    let (Some(qty), Some(instrument)) = (req.new_order_qty(), req.instrument()) else { return false };
    let oid = req.order_id();
    log::warn!("Order {} discarded: its beta hedge's max size is below the hedge size (precaution)", oid);
    let (code, text) = crate::client_core::HEDGE_SIZE_WARNING;
    shared.orders.push_order_error(oid, code, text.to_string());
    let perm_id = context.new_server_id();
    shared.orders.push_order_update(OrderUpdate {
        order_id: oid,
        instrument,
        status: OrderStatus::Cancelled,
        filled_qty_fixed: 0,
        remaining_qty_fixed: qty,
        avg_fill_price: 0,
        perm_id,
        parent_id: attrs.parent_id,
        timestamp_ns: context.now_ns(),
    });
    shared.orders.push_order_notice(oid, 201, "Order rejected - reason:Order was discarded.".into());
    true
}

/// A new order the client found refused by a rule the reference checks
/// once the contract is known (`ClientCore::kept_refusal`, ibx#688): its
/// error, nothing sent; the order is kept for its cancel, which ends it as
/// an order never sent (Cancelled, 202, 161).
fn kept_refusal(req: &mut OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    let Some(instrument) = req.instrument() else { return false };
    let qty = req.new_order_qty().unwrap_or(0);
    let Some(attrs) = req.new_order_attrs_mut() else { return false };
    let Some((code, text)) = attrs.kept_refusal.take() else { return false };
    let oid = req.order_id();
    log::warn!("Order {} refused: {}", oid, code);
    shared.orders.push_order_error(oid, code, text);
    let parent = req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
    context.keep_unsent(oid, (instrument, qty, parent));
    true
}

/// A hedge's beta or ratio as the reference writes it (ibx#614): at least
/// two decimals, up to eight ("1.00", "0.50", "1.2345").
fn hedge_number(v: f64) -> String {
    let mut text = format!("{:.8}", v);
    while text.ends_with('0') && text.split('.').nth(1).is_some_and(|d| d.len() > 2) {
        text.pop();
    }
    text
}

/// The adjustable-stop tags (ib-agent#49), shared by the plain and extended
/// paths so both emit the same values in the same order.
fn adjustable_stop_tags(
    trigger_price: crate::types::Price,
    adjusted_order_type: crate::types::AdjustedOrderType,
    adjusted_stop_price: crate::types::Price,
    adjusted_stop_limit_price: crate::types::Price,
    adjusted_trailing_amount: crate::types::Price,
    adjustable_trailing_unit: i32,
) -> Vec<(u32, String)> {
    let mut tags = vec![
        (6257, "1".to_string()),                            // Has adjustable params flag
        (6261, adjusted_order_type.fix_code().to_string()), // Adjusted order type
        (6258, format_price_ref(trigger_price).to_string()),    // Trigger price
        (6259, format_price_ref(adjusted_stop_price).to_string()), // Adjusted stop price
    ];
    if adjusted_stop_limit_price > 0 {
        tags.push((6262, format_price_ref(adjusted_stop_limit_price).to_string())); // Adjusted stop limit price
    }
    // When the stop converts to a trailing type, carry the trailing
    // amount and its unit (0 = amount, 100 = percent).
    // Captured in ib-agent#167 (ibx#225).
    if matches!(adjusted_order_type,
        crate::types::AdjustedOrderType::Trail
        | crate::types::AdjustedOrderType::TrailLimit)
    {
        tags.push((6260, format_price_ref(adjusted_trailing_amount).to_string()));
        tags.push((6269, adjustable_trailing_unit.to_string()));
    }
    tags
}

/// Order fields as (tag, value) pairs.
type Fields = Vec<(u32, String)>;

/// The fields of a pegged-to-market or pegged-to-midpoint order, as the
/// reference writes them (ibx#414; pegged to midpoint captured in
/// ib-agent#192 B8c, pegged to market not observed yet): the limit
/// price when set and, for pegged to market, the offset in the stop price
/// field; then the pegged order type, the offset (always 0.00 for pegged
/// to midpoint) and the peg instruction. Returned as the price fields
/// (before the account) and the order-type fields.
fn pegged_tags(mid: bool, price: crate::types::Price, offset: crate::types::Price) -> (Fields, Fields) {
    let offset = if mid { 0 } else { offset };
    let mut prices = Vec::with_capacity(2);
    if price > 0 { prices.push((44, format_price_ref(price).to_string())); }
    if !mid { prices.push((99, format_price_ref(offset).to_string())); }
    let types = vec![
        (40, "P".to_string()),
        (211, format_price_ref(offset).to_string()),
        (18, if mid { "M" } else { "P" }.to_string()),
    ];
    (prices, types)
}

/// The benchmark attributes of a pegged-to-benchmark order, as the
/// reference writes them (ibx#415; captured 25/09/2026 and 26/09/2026):
/// the pegged change, negative for a decrease, the reference change, the
/// stock reference price when set, and on a new order only the reference
/// contract and its exchange (SMART as BEST) when set.
fn peg_bench_attrs(
    stock_ref_price: crate::types::Price,
    ref_con_id: i64,
    is_peg_decrease: bool,
    pegged_change_amount: crate::types::Price,
    ref_change_amount: crate::types::Price,
    ref_exchange: &str,
    new_order: bool,
) -> Vec<(u32, String)> {
    let signed = if is_peg_decrease { -pegged_change_amount } else { pegged_change_amount };
    let mut tags = Vec::with_capacity(5);
    if new_order { tags.push((6941, ref_con_id.to_string())); }
    tags.push((6938, format_price_ref(signed).to_string()));
    tags.push((6939, format_price_ref(ref_change_amount).to_string()));
    if stock_ref_price > 0 { tags.push((6580, format_price_ref(stock_ref_price).to_string())); }
    if new_order && !ref_exchange.is_empty() { tags.push((6942, condition_exchange(ref_exchange))); }
    tags
}

/// The compete attributes of a pegged-to-best order (ibx#469; captured
/// 26/09/2026 and 28/09/2026): the minimum compete size and the offset
/// against the best price. The client sets neither; the reference writes
/// these two values itself.
fn peg_best_attrs() -> Vec<(u32, String)> {
    vec![(8411, "100".to_string()), (8412, "0.02".to_string())]
}

/// The replace message for a working order, in the reference's field order
/// (ib-agent#192 group A). The identity fields the reference leaves out of a
/// replace (exchange, currency, routing) are left out; the order type, its
/// prices and the time-in-force are restated from the wanted state.
///
/// Captured: LMT, STP, STP LMT, TRAIL (amount and percent), TRAIL LIMIT,
/// outside-RTH (sent only when set), time-in-force and the good-till date.
/// Other kinds restate their prices in the same fields as their submission;
/// their replace has not been captured.
/// Not restated: the OCA group and parent link (the reference leaves them
/// out and the server keeps them), and the other extended attributes.
#[allow(clippy::too_many_arguments)]
fn modify_fields(
    clord: &str,
    orig_clord: &str,
    account_id: &str,
    qty: u32,
    side: &str,
    symbol: &str,
    sec_type: &str,
    con_id: &str,
    kind: crate::types::OrderKind,
    tif: u8,
    attrs: &crate::types::OrderAttrs,
    trail_limit_offset: Option<crate::types::Price>,
    reported_stop: Option<crate::types::Price>,
    bracket_key: Option<&str>,
    price_mgmt: bool,
) -> Vec<(u32, String)> {
    use crate::types::OrderKind as K;
    let p = |v: crate::types::Price| format_price_ref(v).to_string();

    // Prices go before the account; type-specific fields after the order
    // type. A trail value rides both the stop field and the trail field.
    let mut before_account: Vec<(u32, String)> = Vec::new();
    let mut stop_trigger: Option<String> = None; // restated for STP / STP LMT
    // A trailing type restates its stop price only when it differs from the
    // last one the server reported, and not before any report
    // (no reported stop, or an equal one, is not written; captured
    // ib-agent#195 S2 with the stop trigger, ib-agent#192 A3b without).
    let trailing_stop = |stop: crate::types::Price| (stop > 0 && reported_stop.is_some_and(|r| r != stop)).then(|| p(stop));
    let mut touched_trigger: Option<String> = None; // MIT / LIT, an attribute
    let mut trail_offset: Option<String> = None; // TRAIL LIMIT limit offset
    let mut trail_unit: Option<&str> = None;     // 0 = amount, 100 = percent
    let mut after_type: Vec<(u32, String)> = Vec::new();
    let mut bench_attrs: Vec<(u32, String)> = Vec::new();
    let ord_type: &str = match kind {
        K::Market => "1",
        K::Limit { price } => { before_account.push((44, p(price))); "2" }
        K::Stop { stop_price } => {
            before_account.push((99, p(stop_price)));
            stop_trigger = Some(p(stop_price));
            "3"
        }
        K::StopLimit { price, stop_price } => {
            before_account.push((44, p(price)));
            before_account.push((99, p(stop_price)));
            stop_trigger = Some(p(stop_price));
            "4"
        }
        K::TrailingStop { trail_amt, trail_stop_price } => {
            before_account.push((99, p(trail_amt)));
            stop_trigger = trailing_stop(trail_stop_price);
            trail_unit = Some("0");
            after_type.push((211, p(trail_amt)));
            "P"
        }
        K::TrailPct { trail_percent, trail_stop_price } => {
            let pct = format_price_ref(trail_percent).to_string();
            before_account.push((99, pct.clone()));
            stop_trigger = trailing_stop(trail_stop_price);
            trail_unit = Some("100");
            after_type.push((211, pct));
            "P"
        }
        K::TrailingStopLimit { lmt_offset, lmt_price, trail_amt, trail_stop_price } => {
            stop_trigger = trailing_stop(trail_stop_price);
            // An absolute limit price: the limit price with the order's offset, as the
            // reference restates both (ib-agent#194, ib-agent#195).
            if let Some(price) = lmt_price { before_account.push((44, p(price))); }
            before_account.push((99, p(trail_amt)));
            trail_offset = match lmt_price {
                Some(_) => trail_limit_offset.map(p),
                None => Some(p(lmt_offset)),
            };
            trail_unit = Some("0");
            after_type.push((211, p(trail_amt)));
            "TSL"
        }
        K::Moc => "5",
        K::Loc { price } => { before_account.push((44, p(price))); "B" }
        // A trigger price of 0 is not written (ibx#688).
        K::Mit { stop_price } => {
            if stop_price != 0 {
                before_account.push((99, p(stop_price)));
                touched_trigger = Some(p(stop_price));
            }
            "J"
        }
        K::Lit { price, stop_price } => {
            before_account.push((44, p(price)));
            if stop_price != 0 {
                before_account.push((99, p(stop_price)));
                touched_trigger = Some(p(stop_price));
            }
            "LT"
        }
        K::Mtl => "K",
        K::MktPrt => "U",
        K::StpPrt { stop_price } => {
            before_account.push((99, p(stop_price)));
            stop_trigger = Some(p(stop_price));
            "SP"
        }
        K::MidPrice { price_cap } => {
            if price_cap > 0 { before_account.push((44, p(price_cap))); }
            "MIDPX"
        }
        K::SnapMkt { offset } | K::SnapMid { offset } | K::SnapPri { offset } => {
            before_account.push((99, format_price_ref(offset).to_string()));
            after_type.push((211, format_price_ref(offset).to_string()));
            match kind { K::SnapMkt { .. } => "SMKT", K::SnapMid { .. } => "SMID", _ => "SREL" }
        }
        K::PegMkt { price, offset } | K::PegMid { price, offset } => {
            let (prices, mut types) = pegged_tags(matches!(kind, K::PegMid { .. }), price, offset);
            before_account.extend(prices);
            types.remove(0); // the order type, pushed below
            after_type.extend(types.into_iter().filter(|(t, _)| *t != 18));
            "P"
        }
        K::Rel { price, offset } => {
            // The price cap, restated (ib-agent#199).
            if price > 0 { before_account.push((44, p(price))); }
            after_type.push((211, p(offset)));
            "P"
        }
        K::AdjustableStop { stop_price, .. } => {
            before_account.push((99, p(stop_price)));
            stop_trigger = Some(p(stop_price));
            "3"
        }
        // TRAIL MIT and TRAIL LIT (ib-agent#197, captured 26/09/2026,
        // 28/09/2026 and 07/10/2026): the trailing value in both fields, the trigger
        // restated when the order has one, and for TRAIL LIT the limit
        // price with the offset the server reported.
        K::TrailMit { trail, percent, trail_stop_price } => {
            before_account.push((99, p(trail)));
            if trail_stop_price > 0 { touched_trigger = Some(p(trail_stop_price)); }
            trail_unit = Some(if percent { "100" } else { "0" });
            after_type.push((211, p(trail)));
            "TMIT"
        }
        K::TrailLit { price, trail, percent, trail_stop_price } => {
            before_account.push((44, p(price)));
            before_account.push((99, p(trail)));
            if trail_stop_price > 0 { touched_trigger = Some(p(trail_stop_price)); }
            trail_offset = trail_limit_offset.map(p);
            trail_unit = Some(if percent { "100" } else { "0" });
            after_type.push((211, p(trail)));
            "TLIT"
        }
        // Not captured: a pegged-to-best order was never accepted, and the
        // retail price improvement one was cancelled at once. The prices
        // are restated in the fields of the new order.
        K::PegBest { price } => {
            if price > 0 { before_account.push((44, p(price))); }
            bench_attrs = peg_best_attrs();
            "E2M"
        }
        K::Rpi { price, offset } => {
            before_account.push((44, p(price)));
            before_account.push((99, p(offset)));
            after_type.push((211, p(offset)));
            "RPI"
        }
        // Refused before anything is sent (ibx#673, ibx#745): never written.
        K::Volatility { .. } => "VOL",
        K::PegStk { .. } => "PEG STK",
        K::Quote => "QUOTE",
        K::PassvRel { price, offset } => {
            if price > 0 { before_account.push((44, p(price))); }
            before_account.push((99, p(offset)));
            after_type.push((211, p(offset)));
            "PSVR"
        }
        // The starting price and the benchmark attributes, without the
        // reference contract and exchange (ib-agent#197, 26/09/2026).
        K::PegBench { starting_price, stock_ref_price, ref_con_id, is_peg_decrease,
            pegged_change_amount, ref_change_amount } => {
            if starting_price > 0 { before_account.push((99, format_price_ref(starting_price).to_string())); }
            bench_attrs = peg_bench_attrs(stock_ref_price, ref_con_id, is_peg_decrease,
                pegged_change_amount, ref_change_amount, "", false);
            "PB"
        }
    };
    // The instruction field as on the new order (the reference's replace
    // writes it the same way): the type letter, G for
    // all-or-none, e for an algo (ibx#263).
    let inst = exec_inst(&kind, attrs.all_or_none, attrs.algo.is_some());
    if !inst.is_empty() { after_type.push((18, inst)); }

    let tif_str = tif_str(tif);
    let mut f: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE.to_string()),
        (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
        (11, clord.to_string()),
        (41, orig_clord.to_string()),
    ];
    f.extend(before_account);
    f.push((1, account_id.to_string()));
    // Good-till: the reference restates the expiry right after the account.
    if attrs.good_till_date_ymd > 0 {
        f.push((432, format!("{:08}", attrs.good_till_date_ymd)));
    } else if attrs.good_till > 0 {
        f.push((126, unix_to_ib_utc_dash(attrs.good_till)));
    }
    if let Some(s) = stop_trigger { f.push((6117, s)); }
    if let Some(o) = trail_offset { f.push((6370, o)); }
    // The orderRef, restated on every replace (ibx#466).
    if !attrs.order_ref.is_empty() { f.push((6010, attrs.order_ref.clone())); }
    // includeOvernight, restated on a replace (ibx#604; the reference 10.51,
    // paper, 19/10/2026 overnight).
    if attrs.include_overnight { f.push((8534, "1".to_string())); }
    f.push((6122, "c".to_string()));
    // The bracket key, restated on every replace (ibx#248), as the
    // reference.
    if let Some(key) = bracket_key { f.push((6531, key.to_string())); }
    // Outside-RTH only when the order has it: a replace without it leaves
    // the order regular-hours only (ibx#247).
    if attrs.outside_rth { f.push((6433, "1".to_string())); }
    // Allow pre-open and ignore opening auction, restated where they
    // apply (ibx#607, paper 12/10/2026).
    if attrs.allow_pre_open { f.push((6524, "1".to_string())); }
    if attrs.ignore_open_auction { f.push((6562, "1".to_string())); }
    // The touched trigger of MIT and LIT, restated (ib-agent#199, ibx#263).
    if let Some(t) = touched_trigger { f.push((6117, t)); }
    if let Some(u) = trail_unit { f.push((6268, u.to_string())); }
    f.extend(bench_attrs);
    // Post-only, restated; deactivate never is: the replace sends the
    // order on (ibx#607, paper 12/10/2026).
    if attrs.post_only { f.push((6605, "1".to_string())); }
    // Seek price improvement, restated (ibx#608, paper 12/10/2026).
    if attrs.seek_price_improvement { f.push((6557, "1".to_string())); }
    // The price management flag, last of the attributes (ibx#492).
    if price_mgmt && !crate::engine::price_mgmt::excluded_kind(&kind) { f.push((8339, "1".to_string())); }
    // The algo block, restated as on the new order (the reference's replace
    // writes it the same way; ibx#263).
    if let Some(algo) = &attrs.algo { f.extend(algo_fields(algo)); }
    f.push((38, format_uint(qty as u64).to_string()));
    f.push((54, side.to_string()));
    f.push((40, ord_type.to_string()));
    f.extend(after_type);
    f.push((55, symbol.to_string()));
    f.push((167, sec_type.to_string()));
    f.push((6035, symbol.to_string()));
    f.push((59, tif_str));
    push_dtc_flag(&mut f, tif);
    f.push((6008, con_id.to_string()));
    // The conditions of the order, after its conId (ibx#726, the reference
    // 10.51 on paper, 17/10/2026): the overnight flag, then the conditions
    // in the replace's own form. A replace without them was not tried.
    if !attrs.conditions.is_empty() {
        f.push((8612, "0".to_string()));
        f.extend(replace_condition_tags(&attrs.conditions));
    }
    f.push((6088, "Socket".to_string()));
    f.push((6211, String::new()));
    f.push((6238, String::new()));
    f
}

/// The replace of a combo order as the reference writes it (ibx#470;
/// captured 26/09/2026): the legs' symbols, the smartComboRoutingParams
/// after the other attributes, and no combo block, except for an order
/// with per-leg prices, which restates the price and the leg prices and
/// the combo block after the conId. The leg prices are the modify's,
/// `new_prices` in the caller's leg order, and the price they give (the
/// reference 10.51, recorded 18/10/2026, ibx#604); without them, and in
/// the 10.40 form, the ones the order was placed with (a new leg price did
/// not reach the wire on 10.40).
fn combo_modify_fields(fields: &mut Vec<(u32, String)>, combo: &crate::engine::combo::ComboOrder, new_prices: Option<&[crate::types::Price]>) {
    let symbol = combo.combo.symbol();
    for (tag, value) in fields.iter_mut() {
        if matches!(*tag, 55 | 6035) { *value = symbol.clone(); }
    }
    if let Some(at) = fields.iter().position(|(t, _)| *t == 38) {
        for (i, attr) in combo.spec.routing_attrs.iter().enumerate() {
            fields.insert(at + i, attr.clone());
        }
    }
    if combo.leg_prices.is_empty() { return; }
    let modified = new_prices.map(|p| combo.combo.leg_prices(&crate::types::ComboSpec {
        legs: combo.spec.legs.clone(), leg_prices: p.to_vec(), ..Default::default()
    })).filter(|p| p.len() == combo.leg_prices.len());
    let (leg_prices, price) = match &modified {
        Some(p) => (p.as_slice(), combo.combo.price_of_legs(p)),
        None => (combo.leg_prices.as_slice(), combo.price),
    };
    if let Some(price) = price {
        match fields.iter().position(|(t, _)| *t == 44) {
            Some(i) => fields[i].1 = format_price_ref(price).to_string(),
            None => if let Some(i) = fields.iter().position(|(t, _)| *t == 41) {
                fields.insert(i + 1, (44, format_price_ref(price).to_string()));
            },
        }
    }
    if let Some(at) = fields.iter().position(|(t, _)| *t == 44) {
        for (i, p) in leg_prices.iter().enumerate() {
            fields.insert(at + 1 + i, (6879, format_price_ref(*p).to_string()));
        }
    }
    if let Some(at) = fields.iter().position(|(t, _)| *t == 6008) {
        // After the order's conditions, which follow the conId (the
        // reference's replace table; a combo with conditions not observed).
        let at = at + fields[at + 1..].iter().take_while(|(t, _)| REPLACE_CONDITION_TAGS.contains(t)).count();
        for (i, f) in combo.combo.order_block().into_iter().enumerate() {
            fields.insert(at + 1 + i, f);
        }
    }
}

/// The definition an order rule reads for an instrument on its exchange
/// (ibx#465, ibx#414).
enum Definition<'a> {
    /// The instrument has no contract id: no rule applies.
    NoContract,
    /// Not known yet; the lookup was asked once and the request waits.
    Waiting,
    /// Known, with the exchange it was asked for.
    Known(&'a crate::engine::outside_rth::RthTypes, String),
}

/// The definition of `instrument` on the exchange it is routed to: by
/// conId for that exchange, as the reference asks for the order-type list
/// (ib-agent#199).
fn definition<'a>(
    context: &'a mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    instrument: crate::types::InstrumentId,
) -> Definition<'a> {
    definition_on(context, conn, hb, instrument, None)
}

/// [`definition`] on another exchange than the order's (`on`), BEST for
/// the contract's SMART definition.
fn definition_on<'a>(
    context: &'a mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    instrument: crate::types::InstrumentId,
    on: Option<&str>,
) -> Definition<'a> {
    let Some(con_id) = context.market.con_id(instrument) else { return Definition::NoContract };
    let destination = match on {
        Some(exchange) => exchange.to_string(),
        None => context.market.order_routing(instrument).1,
    };
    let key = (con_id, destination);
    if !context.rth_types.contains_key(&key) {
        if !context.rth_lookups.iter().any(|(_, k, _)| *k == key) {
            // The reference's name of this lookup (ibx#706); the session's
            // number is given on the wire. A contract given by its conId
            // alone is looked up as a contract not seen yet, from the
            // client's source (ibx#730; the reference 10.51, paper,
            // observed in the recordings of 13-16/10/2026: 12 first orders
            // of 12 given by their conId, every type).
            let by_con_id = on.is_none() && context.market.blank_symbol(instrument);
            let id = if by_con_id {
                let number = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
                context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
                format!("{}{}", crate::control::contracts::SECDEF_MSG_NAME, number)
            } else {
                let id = format!("{}{}", crate::control::contracts::SECDEF_ORDER_TYPES_NAME, context.next_rth_lookup);
                context.next_rth_lookup = context.next_rth_lookup.wrapping_add(1);
                id
            };
            let ts = chrono_free_timestamp();
            let con_id_str = con_id.to_string();
            let mut fields: Vec<(u32, &str)> = vec![(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &id), (321, "2")];
            if by_con_id {
                fields.push((6088, "Socket"));
            }
            fields.extend([(146, "1"), (6008, con_id_str.as_str()), (6004, key.1.as_str())]);
            let _ = conn.send_fix(&fields);
            hb.last_ccp_sent = std::time::Instant::now();
            log::info!("Definition of con_id {} on {} asked for the order rules ({})", con_id, key.1, id);
            context.rth_lookups.push((id, key, std::time::Instant::now() + RTH_LOOKUP_TIMEOUT));
        }
        return Definition::Waiting;
    }
    let exchange = key.1.clone();
    match context.rth_types.get(&key) {
        Some(types) => Definition::Known(types, exchange),
        None => Definition::Waiting,
    }
}

/// A new pegged-to-market or pegged-to-midpoint order whose type is not
/// in the contract's order-type list for its exchange: the reference
/// refuses it with 387 and sends nothing (ib-agent#192 B8b, ibx#414).
/// `Some(false)` while the list is asked for, `Some(true)` when refused,
/// `None` to go on. A definition without a list, or none in time, is
/// not checked.
fn pegged_type_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    let (instrument, allowed) = crate::engine::outside_rth::pegged_type_check(req)?;
    match definition(context, conn, hb, instrument) {
        Definition::NoContract => None,
        Definition::Waiting => Some(false),
        Definition::Known(types, _) if types.types_known && !allowed(types) => {
            let oid = req.order_id();
            log::warn!("Order {} refused: its order type is not in the list of this exchange", oid);
            shared.orders.push_order_error(oid, 387, crate::engine::outside_rth::UNSUPPORTED_ORDER_TYPE.to_string());
            // Kept, unlisted, for its cancel (ibx#542).
            let parent = req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
            context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
            Some(true)
        }
        Definition::Known(..) => None,
    }
}

/// An order with all-or-none whose contract's order-type list for its
/// exchange has no AON key: the reference refuses it with 10257 and sends
/// nothing (ibx#263). `Some(false)` while the list is asked for,
/// `Some(true)` when refused, `None` to go on. A definition without a
/// list, or none in time, is not checked, as the reference with no list.
fn all_or_none_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    let oid = req.order_id();
    let instrument = crate::engine::outside_rth::all_or_none_check(req)?
        .or_else(|| context.order(oid).map(|o| o.instrument))?;
    match definition(context, conn, hb, instrument) {
        Definition::NoContract => None,
        Definition::Waiting => Some(false),
        Definition::Known(types, _) if types.types_known && !types.aon => {
            log::warn!("Order {} refused: all-or-none is not in the list of this exchange", oid);
            shared.orders.push_order_error(oid, 10257, crate::engine::outside_rth::ALL_OR_NONE_NOT_ALLOWED.to_string());
            Some(true)
        }
        Definition::Known(..) => None,
    }
}

/// A new order with a minimum quantity (10256) or a display size (10255)
/// whose contract's list for its exchange has no key for it: refused and
/// kept for its cancel, as the reference 10.51 (ibx#688; paper, 19/10/2026,
/// market closed: AAPL with a minimum quantity, EUR.USD with either; the
/// cancel gives Cancelled, 202 and 161). `Some(false)` while the list is
/// asked for.
fn attribute_list_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    let moc = matches!(req, OrderRequest::SubmitMoc { .. } | OrderRequest::SubmitEx { kind: crate::types::OrderKind::Moc, .. });
    let (_, attrs) = req.new_order_side()?;
    let attrs = attrs.cloned().unwrap_or_default();
    if attrs.min_qty == 0 && attrs.display_size == 0 && !attrs.block_order && !attrs.sweep_to_fill && !attrs.imbalance_only && !moc {
        return None;
    }
    let (min_qty, display, parent) = (attrs.min_qty > 0, attrs.display_size > 0, attrs.parent_id);
    let (block, sweep, imbalance) = (attrs.block_order, attrs.sweep_to_fill, attrs.imbalance_only);
    let instrument = req.instrument()?;
    let oid = req.order_id();
    let refusal = match definition(context, conn, hb, instrument) {
        Definition::NoContract => return None,
        Definition::Waiting => return Some(false),
        Definition::Known(types, _) if types.types_known && min_qty && !types.paon =>
            (10256, crate::engine::outside_rth::MIN_QTY_NOT_ALLOWED),
        Definition::Known(types, _) if types.types_known && display && !types.ice =>
            (10255, crate::engine::outside_rth::DISPLAY_SIZE_NOT_ALLOWED),
        // A block order, sweep-to-fill and imbalance-only the same way
        // (ibx#688; paper, 19/10/2026: an AAPL block order of 50, EUR.USD
        // with sweep-to-fill or imbalance-only, each kept).
        Definition::Known(types, _) if types.types_known && block && !types.block =>
            (10266, crate::engine::outside_rth::BLOCK_NOT_ALLOWED),
        Definition::Known(types, _) if types.types_known && sweep && !types.sweep =>
            (10267, crate::engine::outside_rth::SWEEP_NOT_ALLOWED),
        Definition::Known(types, _) if types.types_known && imbalance && !types.imb =>
            (10342, crate::engine::outside_rth::IMBALANCE_NOT_ALLOWED),
        // Market on close: 387 (a EUR.USD order on IDEALPRO, kept).
        Definition::Known(types, _) if types.types_known && moc && !types.moc =>
            (387, crate::engine::outside_rth::UNSUPPORTED_ORDER_TYPE),
        Definition::Known(..) => return None,
    };
    log::warn!("Order {} refused: {} is not in the list of this exchange", oid, refusal.0);
    shared.orders.push_order_error(oid, refusal.0, refusal.1.to_string());
    context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
    Some(true)
}

/// Outside RTH on a request (ibx#465), with allow pre-open and ignore
/// opening auction (ibx#607). A request of an order that already waits,
/// or with one of them whose contract definition is not known yet, waits
/// (`Some(false)`): the definition is asked once. A new order with outside
/// RTH and allow pre-open where both apply is refused with 437
/// (`Some(true)`). Otherwise each is dropped where the reference drops it,
/// with warning 2109 on a new order (none on a replace), and the request
/// goes on (None).
fn apply_outside_rth(
    req: &mut OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    use crate::engine::outside_rth as rth;
    let oid = req.order_id();
    // A cancel-all waits behind the waiting orders, so it reaches them.
    if !context.rth_parked.is_empty()
        && (matches!(req, OrderRequest::CancelAll { .. }) || context.rth_parked.iter().any(|r| r.order_id() == oid))
    {
        return Some(false);
    }
    let is_new = !matches!(req, OrderRequest::Modify { .. });
    let Some((instrument, kind, tif, _, outside_rth)) = rth::rth_parts(req) else { return None };
    let rth_on = *outside_rth;
    // Allow pre-open and ignore opening auction follow the same rule
    // (ibx#607).
    let (pre_open, ignore_opening) = rth::flag_attrs(req)
        .map_or((false, false), |a| (a.allow_pre_open, a.ignore_open_auction));
    if !rth_on && !pre_open && !ignore_opening { return None; }
    let Some(instrument) = instrument.or_else(|| context.order(oid).map(|o| o.instrument)) else { return None };
    let ibkrats = context.market.exchange(instrument) == "IBKRATS";
    let (types, routed) = match definition(context, conn, hb, instrument) {
        Definition::NoContract => return None,
        Definition::Waiting => return Some(false),
        Definition::Known(types, routed) => (types, routed),
    };
    let exchange = if ibkrats { "IBKRATS" } else { routed.as_str() };
    let rth_kept = rth_on && rth::outside_rth_applies(kind, tif, exchange, types);
    let pre_open_kept = pre_open && rth::open_flag_applies(kind, tif, exchange, types, types.pre_open);
    let ignore_kept = ignore_opening && rth::open_flag_applies(kind, tif, exchange, types, types.ignore_opening);
    // Both flags where both apply: refused with 437, nothing sent; the
    // order is kept for its cancel (paper 12/10/2026: Cancelled, 202
    // "Order was discarded", 161).
    if rth_kept && pre_open_kept && is_new {
        log::warn!("Order {} refused: outside RTH and allow pre-open together", oid);
        shared.orders.push_order_error(oid, 437, rth::PRE_OPEN_AND_OUTSIDE_RTH.to_string());
        let parent = req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
        context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
        return Some(true);
    }
    if let Some((_, _, _, _, outside_rth)) = rth::rth_parts(req) { *outside_rth = rth_kept; }
    if let Some(attrs) = rth::flag_attrs(req) {
        attrs.allow_pre_open = pre_open_kept;
        attrs.ignore_open_auction = ignore_kept;
    }
    // One 2109 on a new order, naming the last attribute dropped; ignore
    // opening auction is dropped without one (paper 12/10/2026).
    let text = if pre_open && !pre_open_kept {
        Some(rth::ALLOW_PRE_OPEN_IGNORED)
    } else if rth_on && !rth_kept {
        Some(rth::OUTSIDE_RTH_IGNORED)
    } else {
        None
    };
    if is_new && let Some(text) = text {
        shared.orders.push_order_error(oid, 2109, text.to_string());
    }
    let mut dropped = 0;
    if pre_open && !pre_open_kept { dropped |= crate::bridge::DROPPED_ALLOW_PRE_OPEN; }
    if ignore_opening && !ignore_kept { dropped |= crate::bridge::DROPPED_IGNORE_OPEN_AUCTION; }
    if dropped != 0 { shared.orders.note_dropped_flags(oid, dropped); }
    None
}

/// A new order with post-only that the reference refuses as it checks the
/// order, after the contract lookup (ibx#607): on pegged to market, an
/// order type that takes post-only but cannot keep it (496 on 10.51, not
/// observed: on AAPL the type's own refusal 387 comes first, paper
/// 12/10/2026), with sweep-to-fill (497, paper 12/10/2026). Nothing is
/// sent; the order is kept for its cancel, as one refused with 387.
fn post_only_refusal(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    let Some((_, Some(attrs))) = req.new_order_side() else { return false };
    if !attrs.post_only { return false; }
    let (code, text) = if matches!(req, OrderRequest::SubmitEx { kind: crate::types::OrderKind::PegMkt { .. }, .. }) {
        (496, "The Post-Only attribute cannot be used with order type \"PEG MKT\".")
    } else if attrs.sweep_to_fill {
        (497, "Post-Only and Sweep-to-Fill attributes cannot be used together.")
    } else {
        return false;
    };
    let Some(instrument) = req.instrument() else { return false };
    let oid = req.order_id();
    log::warn!("Order {} refused: post-only ({})", oid, code);
    shared.orders.push_order_error(oid, code, text.to_string());
    context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), attrs.parent_id));
    true
}

/// A new stock order with not-held that is not for the broker's ATS venue:
/// refused with 10297 and nothing sent; the order is kept for its cancel
/// (the reference 10.51, paper, pre-market 14/10/2026: AAPL on SMART, then
/// Cancelled, 202 "Order was discarded" and 161 at the cancel; ibx#690).
/// Other security types are sent as given (not observed yet).
fn not_held_refusal(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    let Some((_, Some(attrs))) = req.new_order_side() else { return false };
    if !attrs.not_held { return false; }
    let Some(instrument) = req.instrument() else { return false };
    let (sec_type, _) = context.market.order_routing(instrument);
    if !matches!(sec_type.as_str(), "STK" | "CS") || context.market.exchange(instrument) == "IBKRATS" { return false; }
    let oid = req.order_id();
    log::warn!("Order {} refused: not-held (10297)", oid);
    shared.orders.push_order_error(oid, 10297, NOT_HELD_INVALID.to_string());
    context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), attrs.parent_id));
    true
}

/// A new option, futures option or warrant order on a contract whose
/// permission class the account does not trade: refused with 460 and
/// nothing sent; the order is kept for its cancel (Cancelled, 202 "Order
/// was discarded" and 161). The reference asks the server about the class
/// named in the contract's definition once per session, and refuses when
/// the answer has no permission (paper: an AAPL call by description, then
/// by conId, regular hours 14/10/2026; an ES futures option by conId and
/// by description, pre-market 15/10/2026). An option or a futures option
/// waits for the answer; a warrant does not: the first one went out with
/// the question on its way, the next one of the class was refused (a SAP
/// warrant on FWB, pre-market 15/10/2026). A future and a CFD ask their
/// class's permission too and go out with the question on its way, the
/// server refusing them without the permission; once the answer says no
/// permission, the next order of the class is refused with 460 and nothing
/// sent (ibx#729; the reference 10.51, paper, 13/10/2026: MES on CME and an
/// AAPL CFD, then three AAPL CFD orders and an MES order of a session that
/// had the answers).
/// A crypto order waits for the answer too; without the permission it gets
/// no error and nothing is sent: it stays pending, and its cancel ends it
/// as ApiCancelled, nothing filled and its whole quantity remaining, its
/// prices unset (the reference 10.51, paper, regular hours 16/10/2026: a
/// BTC limit order on PAXOS by conId, the permission and the trading hours
/// asked after the lookup, the answer no permission, then nothing until
/// the cancel 5 s later). With the permission, ibx refuses it: its message
/// is not written yet.
/// `Some(false)` while an answer the order waits for is on its way.
fn permission_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    let (_, attrs) = req.new_order_side()?;
    if req.combo().is_some() { return None; }
    let instrument = req.instrument()?;
    let sec_type = context.market.order_routing(instrument).0;
    // A stock whose known definition names a permission class (a Tokyo
    // stock, "16/STK/NONE"; a US stock names none) is asked for it too,
    // and waits for the answer (ibx#688; the reference 10.51, paper,
    // 19/10/2026, market closed: 460, the order kept).
    let classed_stock = matches!(sec_type.as_str(), "STK" | "CS")
        && context.market.con_id(instrument).is_some_and(|c| context.rth_types.iter().any(|(k, t)| k.0 == c && !t.permission_key.is_empty()));
    if !classed_stock && !matches!(sec_type.as_str(), "OPT" | "FOP" | "WAR" | "IOPT" | "CMDTY" | "FUND" | "BOND" | "BILL" | "CRYPTO" | "FUT" | "CFD") { return None; }
    // A warrant, a metal and a fund go out with the question on its way
    // (paper, 16/10/2026: XAUUSD and a VFIAX buy by cash sent at once, the
    // server refusing them; the next fund order then refused with 460); a
    // future and a CFD too (ibx#729, paper 13/10/2026); a structured
    // product and a treasury bill too (paper, 17/10/2026: a DAX turbo on
    // SBF and a bill by conId sent, the server refusing them; the next
    // turbo order, by description, refused with 460).
    let waits = !matches!(sec_type.as_str(), "WAR" | "IOPT" | "CMDTY" | "FUND" | "BILL" | "FUT" | "CFD");
    // A bond or a crypto order not stopped for its permission is refused
    // for its message, which ibx does not write yet.
    let own_message = matches!(sec_type.as_str(), "BOND" | "CRYPTO");
    let unwritten = |shared: &Arc<SharedState>| {
        let oid = req.order_id();
        let (code, text) = unwritten_type_refusal(req, &sec_type).unwrap_or_default();
        log::warn!("Order {} refused: {} order ({})", oid, sec_type, code);
        shared.orders.push_order_error(oid, code, text);
        Some(true)
    };
    let (key, under_con_id) = match definition(context, conn, hb, instrument) {
        Definition::NoContract if own_message => return unwritten(shared),
        Definition::NoContract => return None,
        Definition::Waiting => return Some(false),
        Definition::Known(types, _) => (types.permission_key.clone(), types.under_con_id),
    };
    // A derivative's underlying is looked up first (ibx#730).
    let derivative = matches!(sec_type.as_str(), "OPT" | "FOP" | "WAR" | "IOPT") && under_con_id > 0;
    if derivative && !underlying_known(context, conn, hb, under_con_id) {
        return Some(false);
    }
    // Past this check, refused or not, an option or futures option order
    // starts the market data of its option, for the engine itself
    // (ibx#693); a fund order its top of book, when it does not wait for
    // the answer too (ibx#666).
    let past_check = key.is_empty() || !waits || context.trading_permissions.contains_key(&key);
    if past_check {
        note_order_model(context, instrument, &sec_type);
    }
    if key.is_empty() {
        return if own_message { unwritten(shared) } else { None };
    }
    // The question on the permission, then the trading hours due for the
    // contract (ibx#728): the reference 10.51 asks them in that order
    // (paper, 13-16/10/2026: fourteen first orders of eight types, the
    // hours 1 to 17 ms after the question).
    ask_permission(context, conn, hb, &key);
    if let Some(con_id) = context.market.con_id(instrument).filter(|c| *c > 0) {
        send_order_hours(context, conn, hb, con_id);
    }
    // An option order's underlying as the underlying of index options
    // (ibx#730, `ask_index_option_under`).
    if sec_type == "OPT" && derivative && !is_volatility(req) {
        ask_index_option_under(context, conn, hb, under_con_id);
    }
    // Past the check, the underlying's preferred definition (ibx#730), but
    // for a volatility order, whose refusal starts the CFD check instead
    // (ibx#729).
    if past_check && derivative && !is_volatility(req) {
        ask_underlying_preferred(context, conn, hb, under_con_id);
    }
    // An order whose type's preset is on its way reads the answer once the
    // preset came (ibx#729; the reference 10.51, paper, a future 13/10/2026,
    // a metal and a fund 16/10/2026: the permission's answer first, 460 after
    // the preset's).
    let kinds = preset_kinds(&sec_type, context.market.symbol(instrument));
    if TYPE_PRESETS.contains(&sec_type.as_str()) && kinds.iter().any(|k| context.presets_asked.contains(k)) {
        if context.order_gaps {
            context.preset_waiters.insert(req.order_id());
        }
        return Some(false);
    }
    match context.trading_permissions.get(&key) {
        // A bond or a crypto order with the permission: its message is not
        // written yet.
        Some(true) if own_message => unwritten(shared),
        // A volatility order with the permission: its message is not
        // written yet (ibx#673).
        Some(true) if is_volatility(req) => {
            let oid = req.order_id();
            log::warn!("Order {} refused: VOL order", oid);
            shared.orders.push_order_error(oid, 321,
                "Error validating request.-'PlaceOrder(3)' : cause - VOL orders are not written by ibx yet (ibx#673)".to_string());
            Some(true)
        }
        // A pegged-to-stock option order with the permission: its message
        // is not written yet (ibx#745).
        Some(true) if matches!(req, OrderRequest::SubmitEx { kind: crate::types::OrderKind::PegStk { .. }, .. }) => {
            let oid = req.order_id();
            log::warn!("Order {} refused: PEG STK order", oid);
            shared.orders.push_order_error(oid, 321,
                "Error validating request.-'PlaceOrder(3)' : cause - PEG STK orders are not written by ibx yet (ibx#745)".to_string());
            Some(true)
        }
        Some(true) => None,
        // A crypto order without the permission: no error, nothing sent;
        // it stays pending until its cancel (ApiCancelled).
        Some(false) if sec_type == "CRYPTO" => {
            let oid = req.order_id();
            log::warn!("Order {} kept pending: no trading permission for {}", oid, key);
            if !matches!(req, OrderRequest::SubmitWhatIf { .. }) {
                context.api_pending.insert(oid, req.clone());
            }
            Some(true)
        }
        Some(false) => {
            let oid = req.order_id();
            // After the preset it waited for, the refusal keeps the
            // reference's gap (ibx#729, `REFUSAL_AFTER_PRESET`).
            if context.preset_waiters.contains(&oid) {
                let answered = kinds.iter().filter_map(|k| context.preset_answered.get(k).copied()).max();
                if !wait_until(context, answered.map(|at| at + REFUSAL_AFTER_PRESET)) {
                    return Some(false);
                }
                context.preset_waiters.remove(&oid);
            }
            log::warn!("Order {} refused: no trading permission for {} (460)", oid, key);
            shared.orders.push_order_error(oid, 460, NO_TRADING_PERMISSIONS.to_string());
            // A volatility option order: the CFD of its underlying is
            // checked after the refusal (ibx#729, `start_cfd_check`).
            if sec_type == "OPT" && is_volatility(req) {
                start_cfd_check(context, conn, hb, instrument);
            }
            let qty = match req.new_order_qty().unwrap_or(0) {
                0 => context.cash_orders.get(&oid).copied().unwrap_or(0),
                q => q,
            };
            context.keep_unsent(oid, (instrument, qty, attrs.map_or(0, |a| a.parent_id)));
            Some(true)
        }
        // Asked above: the order waits for the answer, or goes with the
        // question on its way.
        None => waits.then_some(false),
    }
}

/// An option or futures option order past its permission check: the
/// engine starts the option's market data and its model, as the reference
/// does whether the order is then refused or sent (ibx#693; observed in
/// the recordings of 13-16/10/2026, paper: 5 option orders and 1 futures
/// option order, each the first of its contract in the session, all
/// refused for the permission; none for stocks, warrants and funds, nor for
/// an option order refused before this check). Kept for the session; the
/// engine drops a second start of a contract already started.
///
/// A fund order past the check starts the fund's top of book alone, for
/// the engine (ibx#666; the reference 10.51, paper, regular hours
/// 16/10/2026, VFIAX bought by cash on FUNDSERV, the first fund order of
/// two sessions: once sent with the permission asked, once refused with
/// 460 after the answer; the fund's farm connected with its notices and
/// the pair asked there, kept to the end of the recordings. Nothing for a
/// fund order refused before the check, nor for a second order of the
/// fund in the session).
fn note_order_model(context: &mut Context, instrument: crate::types::InstrumentId, sec_type: &str) {
    if !matches!(sec_type, "OPT" | "FOP" | "FUND") { return; }
    let Some(con_id) = context.market.con_id(instrument).filter(|c| *c > 0) else { return };
    if context.order_models.iter().any(|(_, c, _)| *c == con_id) { return; }
    let exchange = context.market.exchange(instrument).to_string();
    context.order_models.push((instrument, con_id, exchange));
}

/// An order on a type the definition named (a contract given by its
/// conId) that is refused before anything is sent (ibx#666):
/// - a fund bought without a cash quantity, or with a quantity: 10203, at
///   once after the lookup, the order not kept (its cancel: 10147), as the
///   reference 10.51 (paper, pre-market 15/10/2026, VFIAX by conId);
/// - an index: 10345 "You cannot trade a Index." once its definition is
///   known, the order not kept (its cancel: 10147), as the reference 10.51
///   (paper, 17/10/2026, market closed: SPX on CBOE by conId and by
///   description, +115 ms);
/// - a type whose order message ibx does not write yet (bonds and crypto
///   with their permission; a forward, a stock loan and the other types
///   the server found no contract for on paper): refused with this text
///   of ibx's own rather than sent without the fields it needs.
fn unwritten_type_refusal(req: &OrderRequest, sec_type: &str) -> Option<(i64, String)> {
    const WRITTEN: [&str; 11] = ["STK", "BAG", "FUT", "CFD", "CASH", "OPT", "FOP", "WAR", "IOPT", "CMDTY", "BILL"];
    if WRITTEN.contains(&sec_type) { return None; }
    if sec_type == "IND" {
        return Some((10345, "You cannot trade a Index.".to_string()));
    }
    if sec_type == "FUND"
        && let Some((crate::types::Side::Buy, attrs)) = req.new_order_side()
        && (attrs.is_none_or(|a| a.cash_qty == 0) || req.new_order_qty().is_some_and(|q| q != 0))
    {
        return Some((10203, "A buy order for FUND must contain non-zero cashQty, and may not contain non-zero totalQuantity.".to_string()));
    }
    if sec_type == "FUND" { return None; }
    Some((321, format!("Error validating request.-'PlaceOrder(3)' : cause - {} orders are not written by ibx yet (ibx#666)", sec_type)))
}

/// Text of the refusal 341 of a pegged-to-stock order whose delta is
/// outside 1 to 100 (the reference 10.51, paper, 18/10/2026).
pub(crate) const PEG_STK_DELTA_INVALID: &str = "The delta is invalid. Valid values are from 1 to 100.\n\nYou can set the delta from the \"Pegged to Stock\" section of the Order Ticket Panel,\nor by selecting Page/Layout from the main menu and adding the Delta column.";

/// A new volatility order (ibx#673).
fn is_volatility(req: &OrderRequest) -> bool {
    matches!(req, OrderRequest::SubmitEx { kind: crate::types::OrderKind::Volatility { .. }, .. })
}

/// The limit price an order preset names (ibx#673): its kind of price,
/// its offset (a percent of the price when `percent`), and whether a sell
/// takes the other side of the book.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PresetLimit {
    pub(crate) price_type: u32,
    pub(crate) offset: f64,
    pub(crate) percent: bool,
    pub(crate) reverse: bool,
}

/// The kinds of price of a preset's limit price.
const PRICE_BID: u32 = 0;
const PRICE_ASK: u32 = 1;
const PRICE_LAST: u32 = 2;
const PRICE_DEFAULT: u32 = 5;
const PRICE_MIDPOINT: u32 = 12;

impl Default for PresetLimit {
    /// The reference's own preset: the ask, no offset, a sell on the bid.
    fn default() -> Self {
        Self { price_type: PRICE_ASK, offset: 0.0, percent: false, reverse: true }
    }
}

/// The limit price of an order preset's answer (ibx#673): the group of the
/// limit price field, its kind of price, offset and unit, in the order the
/// answer gives them; the side reversal flag.
pub(crate) fn preset_limit_reply(context: &mut Context, parsed: &fix::FieldMap<'_>, msg: &[u8]) {
    let Some(kind) = parsed.get(&8168).and_then(|k| k.strip_prefix("s=")) else { return };
    let mut limit = PresetLimit::default();
    let mut in_limit = false;
    let (mut size, mut size_unit) = (None::<f64>, String::new());
    for part in msg.split(|&c| c == 1) {
        let text = String::from_utf8_lossy(part);
        let Some((tag, value)) = text.split_once('=') else { continue };
        match tag {
            "4084" => in_limit = value == "4058",
            "4088" if in_limit => limit.price_type = value.parse().unwrap_or(PRICE_ASK),
            "4085" if in_limit => limit.offset = value.parse().unwrap_or(0.0),
            "4086" if in_limit => limit.percent = value == "%",
            "4050" => limit.reverse = !value.eq_ignore_ascii_case("false"),
            "4020" => size = value.parse().ok(),
            "4041" => size_unit = value.to_string(),
            _ => {}
        }
    }
    context.preset_limits.insert(kind.to_string(), limit);
    // The default order size, when it is an amount of money (ibx#748).
    match size.filter(|a| size_unit == "Amount" && *a > 0.0) {
        Some(amount) => { context.preset_amounts.insert(kind.to_string(), amount); }
        None => { context.preset_amounts.remove(kind); }
    }
}

/// A volatility order (VOL, or a type pegged to volatility) on a contract
/// that is no option, as the reference 10.51 takes it (ibx#673): a limit
/// order, with no volatility field; a fund's a market order. The limit is
/// the client's for a pegged type that gives one, else the price of the
/// account's preset of the contract's type (bid, ask, last or the midpoint
/// of the book), from the quote the session holds, on the tick: a buy's
/// midpoint rounded down, a sell's up (observed: an AAPL stock order went
/// out as a limit order at the midpoint, paper, regular hours 16/10/2026).
/// With no quote to price it, the limit order goes without a price, and the
/// server refuses it for the missing price (observed: a MSFT order with no
/// quote held, paper, regular hours 12/11/2026). Some(false): it waits for
/// the preset.
fn volatility_as_limit(
    req: &mut OrderRequest, sec_type: &str, context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState,
) -> Option<bool> {
    let OrderRequest::SubmitEx { order_id, instrument, side, kind: crate::types::OrderKind::Volatility { price: given, .. }, .. } = req else {
        return None;
    };
    if matches!(sec_type, "OPT" | "FOP") {
        return None;
    }
    let (oid, slot, side, given) = (*order_id, *instrument, *side, *given);
    if sec_type == "FUND" {
        set_kind(req, crate::types::OrderKind::Market);
        return None;
    }
    let price = if given > 0 {
        Some(given)
    } else {
        let kinds = preset_kinds(sec_type, context.market.symbol(slot));
        if !ask_order_presets(context, conn, hb, &kinds) {
            return Some(false);
        }
        let limit = kinds.iter().find_map(|k| context.preset_limits.get(k).copied()).unwrap_or_default();
        preset_price(context, slot, side, limit)
    };
    match price {
        Some(price) => {
            log::info!("Order {}: volatility order on {} sent as a limit order at {}", oid, sec_type, price);
            set_kind(req, crate::types::OrderKind::Limit { price });
            None
        }
        None => {
            log::info!("Order {}: no quote to price the volatility order on {}: sent as a limit order without a price", oid, sec_type);
            set_kind(req, crate::types::OrderKind::Limit { price: 0 });
            None
        }
    }
}

fn set_kind(req: &mut OrderRequest, new: crate::types::OrderKind) {
    if let OrderRequest::SubmitEx { kind, .. } = req {
        *kind = new;
    }
}

/// The price a preset's limit price gives from the quote the session holds
/// for the order's contract (ibx#673), on the contract's tick; None with no
/// such quote.
fn preset_price(context: &Context, slot: crate::types::InstrumentId, side: Side, limit: PresetLimit) -> Option<crate::types::Price> {
    let con_id = context.market.con_id(slot).filter(|c| *c > 0)?;
    let quote = context.market.active_instruments()
        .filter(|&(_, c)| c == con_id)
        .map(|(s, _)| (s, context.market.quote(s)))
        .find(|(_, q)| q.bid > 0 || q.ask > 0 || q.last > 0)?;
    let (quote_slot, q) = quote;
    let tick = [context.market.min_tick_scaled(quote_slot), context.market.min_tick_scaled(slot)].into_iter().find(|t| *t > 0).unwrap_or(0);
    let down = |p: i64| if tick > 0 { p.div_euclid(tick) * tick } else { p };
    let up = |p: i64| if tick > 0 { -(-p).div_euclid(tick) * tick } else { p };
    let buy = matches!(side, Side::Buy);
    let valid = |p: i64| (p > 0).then_some(p);
    let mut price_type = limit.price_type;
    if !buy && limit.reverse {
        price_type = match price_type {
            PRICE_BID => PRICE_ASK,
            PRICE_ASK => PRICE_BID,
            other => other,
        };
    }
    let base = match price_type {
        PRICE_BID => valid(q.bid)?,
        PRICE_ASK => valid(q.ask)?,
        PRICE_LAST => valid(q.last)?,
        PRICE_DEFAULT => if buy { valid(q.ask)? } else { valid(q.bid)? },
        PRICE_MIDPOINT => {
            let (bid, ask) = (down(valid(q.bid)?), up(valid(q.ask)?));
            let mid = (bid + ask) / 2;
            return Some(if buy { down(mid) } else { up(mid) });
        }
        _ => return None,
    };
    if limit.offset == 0.0 {
        return Some(base);
    }
    let mut offset = if limit.percent {
        (base as f64 * limit.offset / 100.0).round() as i64
    } else {
        crate::api::types::price_from_f64(limit.offset)
    };
    if !buy && limit.reverse {
        offset = -offset;
    }
    Some(if buy { up(base + offset) } else { down(base + offset) })
}

/// The answer of a trading permission request (ibx#666): whether the
/// account trades the class; the orders that waited for it go on.
pub(crate) fn permission_reply(context: &mut Context, parsed: &fix::FieldMap<'_>) {
    let Some(&key) = parsed.get(&6411) else { return };
    let allowed = parsed.get(&6414).is_some_and(|v| *v != "0");
    log::info!("Trading permission of {}: {}", key, allowed);
    context.permissions_asked.remove(key);
    context.trading_permissions.insert(key.to_string(), allowed);
    release_rth_parked(context);
}

/// Permission requests with no answer in time: the order goes out, and
/// the server's own answer applies.
pub(crate) fn sweep_permission_requests(context: &mut Context) {
    if context.permissions_asked.is_empty() { return; }
    let now = Instant::now();
    let expired: Vec<String> = context.permissions_asked.iter()
        .filter(|(_, deadline)| **deadline <= now).map(|(k, _)| k.clone()).collect();
    if expired.is_empty() { return; }
    for key in expired {
        log::warn!("No answer on the trading permission of {}: the order goes out", key);
        context.permissions_asked.remove(&key);
        context.trading_permissions.insert(key, true);
    }
    release_rth_parked(context);
}

/// The question on the trading permission of a class, once per session
/// until its answer (ibx#666). Orders and the other requests share it: a
/// class a contract details or market data request asked is not asked
/// again by an order, which reads its answer (ibx#734; the reference 10.51,
/// paper, 13/10/2026: three CFD orders and a future order after contract
/// details of the same classes asked nothing and were refused with 460).
pub(crate) fn ask_permission(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, key: &str) {
    if context.trading_permissions.contains_key(key) || context.permissions_asked.contains_key(key) {
        return;
    }
    let seq = context.next_permission_request.to_string();
    context.next_permission_request += 1;
    let now = chrono_free_timestamp();
    match conn.send_fix(&[(35, "U"), (52, &now), (6040, "65"), (6410, &seq), (6411, key)]) {
        Ok(()) => hb.last_ccp_sent = Instant::now(),
        Err(e) => log::error!("Trading permission request not sent: {}", e),
    }
    log::info!("Trading permission of {} asked ({})", key, seq);
    context.permissions_asked.insert(key.to_string(), Instant::now() + RTH_LOOKUP_TIMEOUT);
}

/// The check of the CFD on an option's underlying that the reference 10.51
/// makes after refusing a volatility option order for its permission
/// (ibx#729; paper, 13/10/2026 regular hours and 16/10/2026, two AAPL
/// option VOL orders, the first of their session, each refused with 460):
/// the CFD looked up by the underlying's symbol and currency 1 to 2 ms after
/// the refusal; once answered, its definition by conId on SMART (1 and 37
/// ms after); once that answered, the question on the permission of the
/// class it names (CFD on stocks), within 1 ms. The answer, no permission,
/// changed nothing seen. Once per underlying in the session (a second VOL
/// order of the session: nothing). Not seen for an option order of another
/// type (three orders refused with 460, 13-14/10/2026) nor for a futures
/// option order.
fn start_cfd_check(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, instrument: crate::types::InstrumentId) {
    let symbol = context.market.symbol(instrument).to_string();
    if symbol.is_empty() || symbol == "?" || !context.cfd_checked.insert(symbol.clone()) {
        return;
    }
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{}{}", crate::control::contracts::SECDEF_BY_SYMBOL_NAME, lookup_id);
    let currency = context.market.currency(instrument).to_string();
    let ts = chrono_free_timestamp();
    let mut fields: Vec<(u32, &str)> = vec![(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"),
        (55, &symbol), (167, "CFD")];
    if !currency.is_empty() {
        fields.push((15, &currency));
    }
    let _ = conn.send_fix(&fields);
    hb.last_ccp_sent = Instant::now();
    log::info!("CFD of {} looked up after a volatility order's refusal ({})", symbol, name);
    context.cfd_requests.push(name);
}

/// How long a derivative order waits for the lookup of its underlying, and
/// its option model for the underlying's preferred definition; then they
/// go on without them.
const UNDERLYING_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the underlying of a derivative order was looked up in the
/// session; asked now when not (ibx#730): the reference 10.51 asks the
/// company lookup of the underlying, by conId on any exchange, once the
/// order's contract is known, and asks the permission of the order's class
/// once it answered (paper, observed in the recordings of 13-16/10/2026: 5
/// first orders of 5 on an option, a futures option or a warrant, the
/// lookup 1 to 3 ms after the contract's answer, the permission 1 to 3 ms
/// after the lookup's answer; nothing for the 7 later orders on the same
/// underlyings, nor for a future, a stock or other types). False while its
/// answer is on its way.
fn underlying_known(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, under_con_id: i64) -> bool {
    match context.order_underlyings.get(&under_con_id) {
        Some(None) => return true,
        Some(Some(deadline)) => return Instant::now() >= *deadline,
        None => {}
    }
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{}{}", crate::control::contracts::SECDEF_UNDERLYING_NAME, lookup_id);
    let con = under_con_id.to_string();
    let ts = chrono_free_timestamp();
    let _ = conn.send_fix(&[(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
        (6008, &con), (6004, "ANYEXCH")]);
    hb.last_ccp_sent = Instant::now();
    log::info!("Underlying {} of an order looked up ({})", under_con_id, name);
    context.order_underlyings.insert(under_con_id, Some(Instant::now() + UNDERLYING_LOOKUP_TIMEOUT));
    context.underlying_requests.push((name, under_con_id));
    false
}

/// The preferred definition of a derivative order's underlying, past the
/// order's permission check, once per underlying in the session (ibx#730;
/// the reference 10.51, paper, observed in the recordings of
/// 13-16/10/2026: by conId on any exchange, for two option orders and a
/// futures option order 2 ms after their refusal for the permission, and
/// for a warrant order 12 ms after the question on its permission; not for
/// a volatility option order nor for a later order on the same
/// underlying). The order's option model waits for its answer: the model's
/// market data was asked 5 to 10 ms after it, three times of three.
fn ask_underlying_preferred(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, under_con_id: i64) {
    if context.under_preferred.contains_key(&under_con_id) {
        return;
    }
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{}{}", crate::control::contracts::SECDEF_PREFERRED_NAME, lookup_id);
    let con = under_con_id.to_string();
    let ts = chrono_free_timestamp();
    let _ = conn.send_fix(&[(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
        (6008, &con), (6004, "ANYEXCH")]);
    hb.last_ccp_sent = Instant::now();
    log::info!("Preferred definition of underlying {} asked for an order ({})", under_con_id, name);
    context.under_preferred.insert(under_con_id, Some(Instant::now() + UNDERLYING_LOOKUP_TIMEOUT));
    context.underlying_requests.push((name, under_con_id));
}

/// The name of the lookup of an option's underlying as the underlying of
/// index options (ibx#730).
const INDEX_OPTION_UNDER_NAME: &str = "IndexOptionUnderConidReqByConid";

/// An option order's underlying, by conId on any exchange, under the name
/// the reference gives the lookup of the underlying of index options, once
/// per underlying in the session (ibx#730; the reference 10.51, paper,
/// 13/10 and 14/10/2026: two first AAPL option orders, 61 and 80 ms after
/// the trading hours asked after the question on the permission, before
/// its answer; not for a volatility option order nor a futures option
/// order, nor for a later order on the same underlying). What it is for
/// was not seen: its answer is read and dropped; nothing waits for it.
fn ask_index_option_under(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, under_con_id: i64) {
    if !context.index_option_unders.insert(under_con_id) {
        return;
    }
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{INDEX_OPTION_UNDER_NAME}{lookup_id}");
    let con = under_con_id.to_string();
    let ts = chrono_free_timestamp();
    let _ = conn.send_fix(&[(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
        (6008, &con), (6004, "ANYEXCH")]);
    hb.last_ccp_sent = Instant::now();
    log::info!("Underlying {} of an option order asked as the underlying of index options ({})", under_con_id, name);
    context.underlying_requests.push((name, under_con_id));
}

/// An answer to an order's lookup of its underlying (`underlying_known`,
/// `ask_underlying_preferred`). The company lookup's answer lets the order
/// go on. The preferred definition is kept for the order's option model;
/// a future's asks the permission of its class (ibx#730; the reference
/// 10.51, paper, 15/10/2026: CME/FUT/GLOBEX 1 ms after the ES future's
/// answer; nothing for a stock underlying). False when the reply is not
/// for one.
pub(crate) fn underlying_reply(context: &mut Context, conn: Option<&mut Connection>, hb: &mut HeartbeatState, req_id: &str, msg: &[u8]) -> bool {
    let Some(at) = context.underlying_requests.iter().position(|(name, _)| name == req_id) else { return false };
    let (_, under_con_id) = context.underlying_requests.remove(at);
    if req_id.starts_with(INDEX_OPTION_UNDER_NAME) {
        return true;
    }
    if req_id.starts_with(crate::control::contracts::SECDEF_PREFERRED_NAME) {
        super::farm::note_definition(context, under_con_id, msg);
        context.under_preferred.insert(under_con_id, None);
        let tags = fix::fix_parse(msg);
        if let Some(conn) = conn
            && tags.get(&167).is_some_and(|t| t == "FUT")
            && let Some(key) = tags.get(&6411).filter(|k| !k.is_empty())
        {
            ask_permission(context, conn, hb, key);
        }
        return true;
    }
    context.order_underlyings.insert(under_con_id, None);
    release_rth_parked(context);
    true
}

/// The underlying lookups with no answer in time: their orders go on
/// (ibx#730).
pub(crate) fn sweep_underlying_lookups(context: &mut Context) {
    let now = Instant::now();
    let mut released = false;
    for d in context.order_underlyings.values_mut() {
        if d.is_some_and(|at| at <= now) {
            *d = None;
            released = true;
        }
    }
    for d in context.under_preferred.values_mut() {
        if d.is_some_and(|at| at <= now) {
            *d = None;
        }
    }
    if released {
        release_rth_parked(context);
    }
}

/// The name of the CFD's definition by conId (ibx#729).
const CFD_CLUSTER_NAME: &str = "DirectedSmartClusterReqByConid";

/// An answer to a step of a CFD check (`start_cfd_check`): the lookup's
/// answer asks the CFD's definition on SMART, the definition's answer the
/// permission of the class it names. False when the reply is not for one.
pub(crate) fn cfd_check_reply(context: &mut Context, conn: Option<&mut Connection>, hb: &mut HeartbeatState, req_id: &str, msg: &[u8]) -> bool {
    let Some(at) = context.cfd_requests.iter().position(|name| name == req_id) else { return false };
    context.cfd_requests.remove(at);
    let Some(conn) = conn else { return true };
    let tags = fix::fix_parse(msg);
    if req_id.starts_with(CFD_CLUSTER_NAME) {
        if let Some(key) = tags.get(&6411).filter(|k| !k.is_empty()) {
            ask_permission(context, conn, hb, key);
        }
        return true;
    }
    let Some(con_id) = tags.get(&6008).filter(|c| c.parse::<i64>().is_ok_and(|c| c > 0)) else { return true };
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{CFD_CLUSTER_NAME}{lookup_id}");
    let ts = chrono_free_timestamp();
    let _ = conn.send_fix(&[(fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
        (6008, con_id), (6004, "BEST")]);
    hb.last_ccp_sent = Instant::now();
    log::info!("CFD {} definition asked ({})", con_id, name);
    context.cfd_requests.push(name);
    true
}

/// The trading permission of a combo's option legs (ibx#510): each
/// class their definitions name is asked once per session, as for an
/// option order. `Some(true)` when a class has no permission,
/// `Some(false)` while an answer is on its way, `None` when every class is
/// allowed (or no leg names one).
///
/// Past the check, refused or not, each option leg starts its option
/// model, as an option order does (ibx#693; the reference 10.51, paper,
/// pre-market 20/10/2026: an SPY call vertical, the underlying's and both
/// legs' data asked right before 460, the option farm's notices after it).
fn combo_permission(built: &crate::engine::combo::Combo, context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState) -> Option<bool> {
    let options = || built.legs.iter().filter(|l| matches!(l.sec_type.as_str(), "OPT" | "FOP"));
    let mut waiting = false;
    let mut refused = false;
    for leg in options().filter(|l| !l.permission_key.is_empty()) {
        match context.trading_permissions.get(&leg.permission_key) {
            Some(false) => refused = true,
            Some(true) => {}
            None => {
                ask_permission(context, conn, hb, &leg.permission_key);
                waiting = true;
            }
        }
    }
    if waiting && !refused {
        return Some(false);
    }
    for leg in options() {
        let Some(instrument) = context.market.try_register(leg.con_id) else { continue };
        if context.market.order_routing(instrument).0 != leg.sec_type {
            context.market.set_routing(instrument, &leg.sec_type, "SMART");
        }
        note_order_model(context, instrument, &leg.sec_type);
    }
    refused.then_some(true)
}

/// Text of error 460 (paper 14/10/2026).
pub(crate) const NO_TRADING_PERMISSIONS: &str = "No trading permissions.";

/// Text of error 10297 (ibx#690, paper 14/10/2026).
const NOT_HELD_INVALID: &str = "Not Held attribute is invalid for this order.";

/// The text of 10234 as the reference's message catalog has it; the link
/// it names is not known (not observed yet, ibx#500).
const MIFID_NO_DECISION_MAKER: &str = "<html>The Decision Maker field is required and not set for this order. You can set default value for the Decision Maker in MiFIR settings of desktop TWS 969.1 or newer.<br><br> <a href=\"\" target=\"_blank\">Learn more about MiFIR</a></html>";

/// MiFID on a new order (ibx#500), on a session with the logon's MIFID2
/// feature that is not a super user's, as the reference 10.51 applies it
/// (not observed yet: the paper account has no MiFID):
/// - an order is covered when the account has a MiFID config id, its
///   destination is not AVGCOST, and the contract's MiFID group is in the
///   account's covered groups (or they hold `*`);
/// - a covered order with neither a Decision Maker nor a Decision Algo is
///   refused with 10234 and not sent (`Some(true)`);
/// - otherwise it goes on with its Decision Maker when covered, and its
///   Decision Algo, Execution Trader and Execution Algo when set.
/// `Some(false)`: the definition it needs is asked; the order waits.
/// Without the feature the values are dropped, as the reference (paper,
/// 11/10/2026).
fn mifid_check(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    context.mifid_send.clear();
    if !context.mifid_manager {
        return None;
    }
    let (_, attrs) = req.new_order_side()?;
    let values = attrs.map(|a| a.mifid.clone()).unwrap_or_default();
    let parent = attrs.map_or(0, |a| a.parent_id);
    let instrument = req.instrument()?;
    let oid = req.order_id();
    let covered = if context.mifid_config_id.is_empty()
        || context.market.order_routing(instrument).1.eq_ignore_ascii_case("AVGCOST")
    {
        false
    } else {
        let group = match definition(context, conn, hb, instrument) {
            Definition::NoContract => None,
            Definition::Waiting => return Some(false),
            Definition::Known(types, _) => Some(types.mifid_group.clone()),
        };
        group.is_some_and(|g| context.mifid_groups.iter().any(|c| c == "*" || (!g.is_empty() && *c == g)))
    };
    if covered && values[0].is_empty() && values[1].is_empty() {
        log::warn!("Order {} refused: MiFID covered with no Decision Maker or Decision Algo", oid);
        shared.orders.push_order_error(oid, 10234, MIFID_NO_DECISION_MAKER.to_string());
        context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
        return Some(true);
    }
    context.mifid_send = mifid_tags(&values, covered);
    if !matches!(req, OrderRequest::SubmitWhatIf { .. }) {
        let key = context.key_of(oid);
        context.mifid_orders.insert(key, (values, covered));
    }
    None
}

/// The soft dollar tier of a new order (ibx#506), as the reference 10.51
/// checks it (not observed yet: the paper account has no tiers): an order
/// with a tier, on an account whose tiers for the contract's market type
/// (the key of the logon's tier list) are not empty, is refused with 10155
/// "Invalid soft dollar tier" when the tier (name and value) is not one of
/// them, and nothing is sent (`Some(true)`); one of them goes with its name
/// and value. With no tiers for that market, the tier is dropped, as the
/// reference on paper (11/10/2026). `Some(false)`: the definition it needs
/// is asked; the order waits.
fn soft_dollar_check(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    context.soft_dollar_send.clear();
    let (_, attrs) = req.new_order_side()?;
    let attrs = attrs?;
    let (name, value) = attrs.soft_dollar_tier.clone();
    if (name.is_empty() && value.is_empty()) || context.soft_dollar_tiers.is_empty() {
        return None;
    }
    let parent = attrs.parent_id;
    let instrument = req.instrument()?;
    let key = match definition(context, conn, hb, instrument) {
        Definition::NoContract => return None,
        Definition::Waiting => return Some(false),
        Definition::Known(types, _) => types.market_type.to_uppercase(),
    };
    let tiers = context.soft_dollar_tiers.get(&key).cloned().unwrap_or_default();
    if tiers.is_empty() {
        return None;
    }
    let oid = req.order_id();
    if !tiers.iter().any(|(n, v)| *n == name && *v == value) {
        log::warn!("Order {} refused: soft dollar tier {}@{} not in the {} tiers", oid, name, value, key);
        shared.orders.push_order_error(oid, 10155, "Invalid soft dollar tier".to_string());
        context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
        return Some(true);
    }
    context.soft_dollar_send = vec![(6519, name), (6520, value)];
    if !matches!(req, OrderRequest::SubmitWhatIf { .. }) {
        let key = context.key_of(oid);
        context.soft_dollar_orders.insert(key, context.soft_dollar_send.clone());
    }
    None
}

/// An order on an FA group, on an FA session (ibx#507), as the reference
/// 10.51 takes it (not observed yet: no FA account). While a save of FA
/// data is not answered it waits (`Some(false)`), and is refused with
/// 10228 when the save failed. In the unified mode, 10226 for a method
/// other than the group's own, a method with a profile (but a percentage
/// change with its percentage), and no method for a group that has none.
fn fa_check(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> Option<bool> {
    if !context.fa_session {
        return None;
    }
    let (_, attrs) = req.new_order_side()?;
    let attrs = attrs?;
    let group = fa_group(&attrs.fa, &context.fa);
    let [_, method, pct, ..] = &attrs.fa;
    if group.is_empty() {
        return None;
    }
    let oid = req.order_id();
    let parent = attrs.parent_id;
    let refuse = |context: &mut Context, code: i64, text: &str| {
        log::warn!("Order {} refused: {} {}", oid, code, text);
        shared.orders.push_order_error(oid, code, text.to_string());
        if let Some(instrument) = req.instrument() {
            context.keep_unsent(oid, (instrument, req.new_order_qty().unwrap_or(0), parent));
        }
        Some(true)
    };
    if context.fa.pending() {
        context.fa.wait(oid);
        return Some(false);
    }
    if context.fa.take_failed(oid) {
        return refuse(context, 10228, crate::control::fa::ORDER_SAVE_FAILED);
    }
    if context.fa.unified {
        let invalid = match context.fa.groups.get(group) {
            Some(Some(own)) => !method.is_empty() && method != own,
            Some(None) => method.is_empty(),
            None => context.fa.profiles.contains(group) && !method.is_empty()
                && !(method == "PctChange" && !pct.is_empty()),
        };
        if invalid {
            return refuse(context, 10226, "Invalid FA method specified");
        }
    }
    None
}

/// The group of an FA order (ibx#507): its group, or its account when
/// that names a stored group or profile, `All` or `AllNonProp`, as the
/// reference takes it.
fn fa_group<'a>(fa: &'a [String; 5], state: &super::fa::FaState) -> &'a str {
    let [group, .., account] = fa;
    if !group.is_empty() {
        return group;
    }
    if crate::control::fa::is_all(account) || state.groups.contains_key(account.as_str()) || state.profiles.contains(account.as_str()) {
        return account;
    }
    ""
}

/// The FA fields of an order on an FA session (ibx#507): its account in
/// place of the logon's when it is allocated to that one account (a group
/// goes to the logon's account), and the allocation tags at `at`.
fn fa_fields(fields: &mut Vec<(u32, String)>, at: usize, fa: &[String; 5], state: &super::fa::FaState) {
    let group = fa_group(fa, state);
    let account = &fa[4];
    if group.is_empty() && !account.is_empty()
        && let Some(field) = fields.iter_mut().find(|(t, _)| *t == 1)
    {
        field.1 = account.clone();
    }
    let at = at.min(fields.len());
    fields.splice(at..at, fa_tags(fa, group, state));
}

/// The FA allocation of an order (ibx#507), as the reference 10.51 writes
/// it on an FA session (not observed yet: no FA account): the group, the
/// method as given, the percentage for the two percentage methods, and the
/// model code unless it is `Core`. A group with no method takes its own
/// stored one, else AvailableEquity; a profile takes none.
fn fa_tags(fa: &[String; 5], group: &str, state: &super::fa::FaState) -> Vec<(u32, String)> {
    let [_, method, pct, model, _] = fa;
    let mut tags = Vec::new();
    if !group.is_empty() {
        tags.push((6160, group.to_string()));
    }
    let own = || match state.groups.get(group) {
        Some(Some(own)) => Some(own.as_str()),
        None if state.profiles.contains(group) => None,
        _ => Some("AvailableEquity"),
    };
    let method = if method.is_empty() && !group.is_empty() { own().unwrap_or("") } else { method.as_str() };
    if !method.is_empty() {
        tags.push((6159, method.to_string()));
    }
    if !model.is_empty() && model != "Core" {
        tags.push((6700, model.clone()));
    }
    if matches!(method, "PctChange" | "PctRoll") && !pct.is_empty() {
        tags.push((6164, pct.clone()));
    }
    tags
}

/// The MiFID tags of an order: the Decision Maker when covered, the other
/// three when set (ibx#500).
fn mifid_tags(values: &[String; 4], covered: bool) -> Vec<(u32, String)> {
    let mut tags = Vec::new();
    if covered && !values[0].is_empty() {
        tags.push((8237, values[0].clone()));
    }
    for (tag, v) in [(8243, &values[1]), (8254, &values[2]), (8255, &values[3])] {
        if !v.is_empty() {
            tags.push((tag, v.clone()));
        }
    }
    tags
}

/// Whether the orders of a request carry the price management flag
/// (ibx#492), None while the contract definition it needs is asked for.
/// Cheap checks first: the value, the session feature, combos, the
/// exclusion list; the definition's price check key last.
fn price_mgmt_flag(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
) -> Option<bool> {
    use crate::engine::price_mgmt as pm;
    if context.rth_parked.iter().any(|r| r.order_id() == req.order_id()) { return None; }
    let Some((instrument, value)) = pm::parts(req) else { return Some(false) };
    let Some(instrument) = instrument.or_else(|| context.order(req.order_id()).map(|o| o.instrument)) else { return Some(false) };
    let (sec_type, exchange) = context.market.order_routing(instrument);
    if !value.unwrap_or_else(|| pm::preset(&sec_type)) || !context.price_mgmt_feature || sec_type == "BAG" {
        return Some(false);
    }
    let smart = context.market.con_id(instrument).and_then(|c| context.smart_components.get(&c)).map(Vec::as_slice);
    if !pm::allowed(context.price_mgmt_exclusions.as_ref(), &exchange, &sec_type, smart) {
        return Some(false);
    }
    let (own, smart) = match definition(context, conn, hb, instrument) {
        Definition::NoContract => return Some(false),
        Definition::Waiting => return None,
        Definition::Known(types, exchange) => (types.price_chk, types.smart && exchange != "BEST"),
    };
    if own || !smart {
        return Some(own);
    }
    // A directed order whose exchange's list lacks the key takes the
    // contract's SMART definition (ibx#546; observed 13/10/2026,
    // regular hours, paper: the flag on orders for ARCA and FOXRIVER).
    match definition_on(context, conn, hb, instrument, Some("BEST")) {
        Definition::NoContract => Some(false),
        Definition::Waiting => None,
        Definition::Known(types, _) => Some(types.price_chk),
    }
}

/// The orders of a request, a bracket's three included.
fn request_order_ids(req: &OrderRequest) -> Vec<crate::types::OrderId> {
    match req {
        OrderRequest::SubmitWhatIf { request } => request_order_ids(request),
        OrderRequest::SubmitBracket { parent_id, tp_id, sl_id, .. } => vec![*parent_id, *tp_id, *sl_id],
        OrderRequest::CancelAll { .. } | OrderRequest::GlobalCancel => Vec::new(),
        other => vec![other.order_id()],
    }
}

/// The OCA group of a new order, None when it has none.
fn request_oca_group(req: &OrderRequest) -> Option<String> {
    let attrs = req.new_order_side()?.1?;
    if !attrs.oca_group_str.is_empty() {
        Some(attrs.oca_group_str.clone())
    } else {
        (attrs.oca_group > 0).then(|| attrs.oca_group.to_string())
    }
}

/// A request must wait behind the waiting ones: a global cancel; a request
/// of an order that waits; a child whose parent waits; an order of the OCA
/// group of one that waits.
fn waits_behind(req: &OrderRequest, parked: &[OrderRequest]) -> bool {
    if parked.is_empty() { return false; }
    if matches!(req, OrderRequest::CancelAll { .. }) { return true; }
    let waiting: Vec<crate::types::OrderId> = parked.iter().flat_map(request_order_ids).collect();
    if request_order_ids(req).iter().any(|id| waiting.contains(id)) { return true; }
    let parent = req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
    if parent > 0 && waiting.contains(&parent) { return true; }
    request_oca_group(req).is_some_and(|group| parked.iter().any(|p| request_oca_group(p).as_deref() == Some(&group)))
}

/// How long an order waits for the definition its outside-RTH needs; then
/// the empty list is used, as the reference when it finds none.
const RTH_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A definition reply for an outside-RTH lookup (ibx#465): keep what the
/// rule needs and release the waiting requests. False when the reply is
/// not for such a lookup.
pub(crate) fn rth_definition_reply(context: &mut Context, req_id: &str, msg: &[u8]) -> bool {
    let Some(idx) = context.rth_lookups.iter().position(|(id, _, _)| id == req_id) else { return false };
    let (_, key, _) = context.rth_lookups.swap_remove(idx);
    // A record of the contract on the exchange the answer names (ibx#706).
    let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
    for def in &records {
        if def.con_id == key.0 && !def.exchange.is_empty() {
            context.order_records.insert((key.0, def.exchange.clone()));
        }
    }
    let not_found = records.is_empty() && crate::control::contracts::parse_secdef_response(msg).is_none();
    store_rth_types(context, key.clone(), msg);
    if not_found && let Some(types) = context.rth_types.get_mut(&key) {
        types.not_found = true;
    }
    release_rth_parked(context);
    true
}

/// The order rules of a contract on an exchange, from a definition reply
/// of that exchange (ibx#465, ibx#669).
/// The minimum order size a definition gives (ibx#688).
fn note_min_size(context: &mut Context, con_id: i64, tags: &std::collections::HashMap<u32, String>) {
    if let Some(min) = tags.get(&6581).and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0) {
        context.min_sizes.insert(con_id, min);
    }
}

fn store_rth_types(context: &mut Context, key: (i64, String), msg: &[u8]) {
    let tags = fix::fix_parse(msg);
    let tokens: Vec<String> = tags.get(&6431).map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
    let def = crate::control::contracts::parse_secdef_response(msg);
    let mut types = crate::engine::outside_rth::RthTypes::from_definition(
        &tokens,
        tags.get(&6523).map(String::as_str).unwrap_or(""),
        &def.as_ref().map(|d| d.api_type_name()).unwrap_or_default(),
        def.as_ref().map(|d| d.currency.as_str()).unwrap_or(""),
    );
    types.smart = tags.get(&6046).is_some_and(|v| v.split(',').any(|e| e == "BEST" || e == "SMART"));
    note_min_size(context, key.0, &tags);
    types.under_sec_type = tags.get(&310).cloned().unwrap_or_default();
    types.permission_key = tags.get(&6411).cloned().unwrap_or_default();
    types.mifid_group = tags.get(&8242).cloned().unwrap_or_default();
    if let Some(d) = def.as_ref() {
        // A contract given by its conId alone takes its symbol, security
        // type and currency from its definition, as the reference resolves
        // it (ibx#666; paper 13/10/2026: an MES future by conId and
        // exchange went out as a stock with no symbol). The type is the
        // definition's own name: a futures option, a fund or a CFD has no
        // type of its own in the parsed definition and went out as a stock
        // (paper 15/10/2026).
        if let Some(id) = context.market.instrument_by_con_id(key.0).filter(|&id| context.market.blank_symbol(id)) {
            let exchange = context.market.exchange(id).to_string();
            context.market.set_symbol(id, d.symbol.clone());
            context.market.set_routing(id, &d.api_type_name(), if exchange.is_empty() { "SMART" } else { &exchange });
            context.market.set_currency(id, &d.currency);
            // Its first order keeps shorter gaps (ibx#729).
            context.by_con_id_contracts.insert(key.0);
        }
        types.under_con_id = d.under_con_id;
        types.local_symbol = d.local_symbol.clone();
        types.contract_month = d.contract_month.clone();
        types.multiplier = format!("{:.2}", if d.multiplier > 0.0 { d.multiplier } else { 1.0 });
        if let Some(right) = d.right {
            types.right = if matches!(right, crate::control::contracts::OptionRight::Call) { "1" } else { "0" }.to_string();
            types.strike = format!("{}", d.strike);
        }
        (types.order_text, types.face_value) = order_text(&tags, d);
    }
    types.valid_exchanges = tags.get(&6046).map(|v| v.split(',')
        .map(|e| if e == "BEST" { "SMART".to_string() } else { e.to_string() }).collect());
    log::info!("Outside RTH definition for con_id {} on {}: {:?}", key.0, key.1, types);
    log::debug!("Order-type list for con_id {} on {}: {:?}", key.0, key.1, tokens);
    context.rth_types.insert(key, types);
}

/// How the reference names a structured product or a treasury bill in an
/// order message's text (399), from its definition, and a bill's face
/// value (ibx#666; the reference 10.51, paper, 17/10/2026, market closed):
/// - a perpetual turbo without rebate: "DAX Perpetual 11088.4204 Call
///   (CBK,0.01,11088.4204 EUR,No Rebate) (24W1Z) ", after "BUY 1": the
///   symbol, the strike, the right, then the issuer, the multiplier, the
///   barrier with the currency, and the local symbol;
/// - a treasury bill: "US-T Govt Oct01'26 912797SA6", after "BUY $1K"
///   (one bill of face value 1000): the symbol, the kind, the maturity
///   and the CUSIP.
///
/// Other shapes (a dated product, one with a rebate, a put) are not
/// observed yet: they keep the symbol and listing form.
fn order_text(tags: &std::collections::HashMap<u32, String>, d: &crate::control::contracts::ContractDefinition) -> (String, Option<u64>) {
    let tag = |t: u32| tags.get(&t).map(String::as_str).unwrap_or("");
    match d.api_type_name().as_str() {
        "IOPT" if tag(541) == "NOEXP" && tag(6805) == "F" && tag(201) == "1" && !tag(106).is_empty() => (format!(
            "{} Perpetual {} Call ({},{},{} {},No Rebate) ({}) ",
            d.symbol, tag(202), tag(106), d.multiplier_text, tag(6804), d.currency, d.local_symbol), None),
        "BILL" if tag(541).len() == 8 && !d.cusip.is_empty() && !tag(6503).is_empty() => {
            const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            let date = tag(541);
            let Some(month) = date[4..6].parse::<usize>().ok().and_then(|m| MONTHS.get(m.wrapping_sub(1))) else {
                return (String::new(), None);
            };
            let mut kind = tag(6503).to_ascii_lowercase();
            if let Some(first) = kind.get_mut(..1) { first.make_ascii_uppercase(); }
            let face = tag(6504).parse::<u64>().ok().filter(|f| *f > 0);
            (format!("{} {} {}{}'{} {}", d.symbol, kind, month, &date[6..8], &date[2..4], d.cusip), face)
        }
        _ => (String::new(), None),
    }
}

/// Lookups with no reply in time: the empty list, as the reference when it
/// finds no definition (ibx#465).
pub(crate) fn sweep_rth_lookups(context: &mut Context) {
    if context.rth_lookups.is_empty() { return; }
    let now = std::time::Instant::now();
    let mut expired = Vec::new();
    context.rth_lookups.retain(|(id, key, deadline)| {
        if *deadline <= now { expired.push((id.clone(), key.clone())); false } else { true }
    });
    if expired.is_empty() { return; }
    for (id, key) in expired {
        log::warn!("No definition for con_id {} on {} ({}): outside RTH uses the empty list", key.0, key.1, id);
        context.rth_types.insert(key, crate::engine::outside_rth::RthTypes::default());
    }
    release_rth_parked(context);
}

/// The end of an order that was never sent, at its cancel (ibx#542,
/// ibx#509): orderStatus Cancelled with nothing filled, 202 "Order was
/// discarded", then 161. False when `order_id` is not such an order.
fn end_unsent(context: &mut Context, shared: &Arc<SharedState>, order_id: OrderId) -> bool {
    let Some((instrument, qty, parent_id)) = context.unsent.remove(&order_id) else { return false };
    // The id it was given when it was placed, as the reference's permId.
    let perm_id = match context.server_id(order_id) {
        id if id != order_id => id,
        _ => context.new_server_id(),
    };
    shared.orders.push_order_update(OrderUpdate {
        order_id,
        instrument,
        status: OrderStatus::Cancelled,
        filled_qty_fixed: 0,
        remaining_qty_fixed: qty,
        avg_fill_price: 0,
        perm_id,
        parent_id,
        timestamp_ns: context.now_ns(),
    });
    shared.orders.push_order_notice(order_id, 202, "Order Canceled - reason:Order was discarded.".into());
    shared.orders.push_order_notice(order_id, 161,
        format!("Cancel attempted when order is not in a cancellable state.  Order permId ={}", perm_id));
    context.finish_unsent(order_id, OrderStatus::Cancelled);
    // The reference keeps it in its book for the session: a later global
    // cancel answers 161 for it each time (ibx#545), in the place and with
    // the OCA group it had when held (ibx#595).
    match context.held_book.remove(&order_id) {
        Some(entry) => context.keep_held_ended(order_id, perm_id, entry),
        None => {
            let entry = crate::engine::context::BookEntry { parent: parent_id, ..Default::default() };
            context.keep_ended(order_id, perm_id, entry, None);
        }
    }
    true
}

/// The new orders one placeOrder transmits together (ibx#547; the
/// reference's messages of 26/09 and 09/10/2026: a held parent, its held
/// children and the order that transmits them leave in one write, after
/// the contract lookup of the transmitting order came back). They join
/// the queue in their order; none goes out while a contract is unknown.
pub(crate) fn take_order_group(context: &mut Context, requests: Vec<OrderRequest>) {
    let group: Vec<(OrderId, crate::types::InstrumentId)> = requests.iter().filter_map(|r| {
        let id = r.order_id();
        let instrument = context.unsent.get(&id).map(|h| h.0).or_else(|| r.instrument())?;
        Some((id, instrument))
    }).collect();
    if group.len() > 1 {
        context.order_groups.push(group);
    }
    for request in requests {
        context.pending_orders.push(request);
    }
}

/// The instruments of the group of `order_id` whose contract is not known
/// yet, `None` when the order is in no group or the group can go. A group
/// that can go is forgotten.
fn group_contracts_unknown(context: &mut Context, order_id: OrderId) -> Option<Vec<crate::types::InstrumentId>> {
    let at = context.order_groups.iter().position(|g| g.iter().any(|(id, _)| *id == order_id))?;
    let unknown: Vec<crate::types::InstrumentId> = context.order_groups[at].iter()
        .map(|(_, i)| *i).filter(|i| context.market.con_id(*i) == Some(0)).collect();
    if unknown.is_empty() {
        // Gone once its last order is through.
        let group = &mut context.order_groups[at];
        group.retain(|(id, _)| *id != order_id);
        if group.is_empty() {
            context.order_groups.remove(at);
        }
        return None;
    }
    Some(unknown)
}

/// An order placed with transmit off (ibx#509; the bracket recordings of
/// 26/09/2026 and the paper run of 09/10/2026): the reference sends
/// nothing for it but the lookup of a contract given without a conId,
/// lists it nowhere, and ends it at its cancel as an order that was never
/// sent.
pub(crate) fn hold_order(
    conn: &mut Option<Connection>, context: &mut Context, hb: &mut HeartbeatState,
    order_id: OrderId, instrument: crate::types::InstrumentId, qty: crate::types::Qty, parent_id: i64, oca_group: &str,
) {
    // Held again with new values: the order stays on the contract it has,
    // which its lookup may have moved to another slot since.
    let instrument = context.unsent.get(&order_id).map_or(instrument, |h| h.0);
    context.keep_unsent(order_id, (instrument, qty, parent_id));
    context.hold_entry(order_id, parent_id, oca_group);
    // Its permId is the next id when it is placed, not when it is sent
    // (paper 09/10/2026: the held orders 37333 and 37334 took the two ids
    // before the next order placed).
    context.assign_server_id(order_id);
    if context.market.con_id(instrument) == Some(0) && let Some(conn) = conn.as_mut() {
        look_up_order_contract(context, conn, hb, instrument);
    }
    // Its other lookups follow its contract's answer (ibx#718).
    if !context.held_lookups.iter().any(|h| h.0 == order_id) {
        context.held_lookups.push((order_id, false));
    }
}

/// Where the lookups of a held order's placeOrder are (ibx#718).
enum HeldLookup {
    Wait,
    Asked,
    Known,
}

/// The lookups the reference makes at the placeOrder of an order held with
/// transmit off, and the moment the order is known as a parent (ibx#718;
/// the reference 10.51, paper, 16/10/2026, pre-market and regular hours,
/// the first held AAPL order of the session, three runs): its contract by
/// symbol; once answered, the trading hours and the order preset of its
/// type when the session has none; once the preset answered (the hours'
/// answer is not waited for, ibx#728), its record on the primary exchange
/// (the lookup of ibx#706); the order is known once that answered, about
/// 0.57 s after the placeOrder. A later held order of a
/// contract whose records the session has asks only its contract (eleven
/// runs) and is known at once (a hedge 0.1 s after it passed the reading;
/// observed on paper, 17/10/2026, market closed). A held order given by
/// its conId asks at most its record (one definition lookup by conId on
/// the primary exchange, answered in 0.12 s; a hedge 0.5 s after passed the
/// reading, same run). Until then a hedge order under it is refused (see
/// the client's hedge reading). A held order the client did not note as
/// unknown (placed again while held) has nothing to wait for. The decision
/// is made as the engine takes the order, so the client's wait for it is
/// that of the command's way.
pub(crate) fn advance_held_lookups(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, shared: &Arc<SharedState>) {
    let mut i = 0;
    while i < context.held_lookups.len() {
        let (order_id, asked) = context.held_lookups[i];
        match held_lookup_step(context, conn, hb, shared, order_id, asked) {
            HeldLookup::Wait => {
                shared.orders.held_waiting(order_id);
                i += 1;
            }
            HeldLookup::Asked => {
                shared.orders.held_waiting(order_id);
                // The other held orders of that contract wait for the same
                // answer.
                let con_id = context.unsent.get(&order_id).and_then(|h| context.market.con_id(h.0));
                for j in 0..context.held_lookups.len() {
                    let other = context.held_lookups[j].0;
                    if context.unsent.get(&other).and_then(|h| context.market.con_id(h.0)) == con_id {
                        context.held_lookups[j].1 = true;
                    }
                }
                i += 1;
            }
            HeldLookup::Known => {
                context.held_lookups.remove(i);
                shared.orders.held_known(order_id);
            }
        }
    }
}

fn held_lookup_step(
    context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, shared: &Arc<SharedState>,
    order_id: OrderId, asked: bool,
) -> HeldLookup {
    // Not waited on: the engine decides for its own orders (ibx#718).
    if !shared.orders.held_pending(order_id) {
        return HeldLookup::Known;
    }
    // Transmitted or cancelled meanwhile: no longer held.
    let Some(&(slot, ..)) = context.unsent.get(&order_id) else { return HeldLookup::Known };
    let con_id = match context.market.con_id(slot) {
        // Its contract's lookup on its way; a lookup refused or with no
        // single contract leaves the order held as before (not observed).
        // A contract an earlier order lookup of the session named, whose
        // records the session has, is known at once, its lookup still on
        // its way (observed on paper, 17/10/2026, market closed: a beta
        // hedge 0.1 s after a held parent of a contract ordered and
        // cancelled before passed the reading).
        Some(0) if context.order_lookups.iter().any(|(_, s)| *s == slot) => {
            let key = order_contract_key(context, slot);
            let named = context.order_contracts_named.iter().find(|(k, _)| *k == key).map(|(_, c)| *c);
            return match named {
                Some(c) if !context.order_record_lookups.iter().any(|(_, k, _)| *k == c)
                    && missing_order_record(context, shared, slot, c).is_none() => HeldLookup::Known,
                _ => HeldLookup::Wait,
            };
        }
        Some(c) if c > 0 => c,
        _ => return HeldLookup::Known,
    };
    // The first step a while after the lookup's answer (ibx#729).
    if !first_step_gap(context, con_id) {
        return HeldLookup::Wait;
    }
    // The trading hours its contract's answer called for (ibx#728).
    send_order_hours(context, conn, hb, con_id);
    if context.order_record_lookups.iter().any(|(_, c, _)| *c == con_id) {
        return HeldLookup::Wait;
    }
    if asked {
        return HeldLookup::Known;
    }
    let Some((exchange, _)) = missing_order_record(context, shared, slot, con_id) else { return HeldLookup::Known };
    // The preset first, as the reference: its record lookup left 27 to 41
    // ms after the preset's answer. The trading hours are not waited for:
    // with the preset known, the reference asks the record with them
    // (ibx#728).
    if !ask_order_presets(context, conn, hb, &["STK".to_string()]) {
        if context.order_gaps {
            context.preset_waiters.insert(order_id);
        }
        return HeldLookup::Wait;
    }
    // The record a while after the preset's answer (ibx#729).
    if !first_order_gap(context, order_id, con_id, true) {
        return HeldLookup::Wait;
    }
    context.preset_waiters.remove(&order_id);
    ask_order_record(context, conn, hb, con_id, &exchange);
    HeldLookup::Asked
}

/// What an order gave of the contract of its slot, as the lookup by
/// symbol asks it (ibx#718).
fn order_contract_key(context: &Context, slot: crate::types::InstrumentId) -> crate::engine::context::OrderContractKey {
    let m = &context.market;
    (m.symbol(slot).to_string(), m.order_routing(slot), m.exchange(slot).to_string(), m.currency(slot).to_string(),
        context.order_lookup_filters.get(&slot).cloned().unwrap_or_default())
}

/// Request numbers of the contract lookups of orders (ibx#486): a range
/// of their own, below the historical-data lookups (0xD000_0000).
pub(crate) const ORDER_LOOKUP_FIRST_ID: u32 = 0xC000_0000;
pub(crate) const ORDER_LOOKUP_IDS: u32 = 0x1000_0000;

/// The exchange whose record of the contract an order needs and the
/// session does not have (ibx#706, ibx#709). The reference 10.51 keeps a
/// contract's records under the exchange their answer names and needs two
/// before an order: the record of the order's own exchange (BEST for a
/// SMART order, which the order's symbol lookup brings) and the record of
/// the contract's primary exchange; it asks the missing one by conId
/// (observed, paper, 15/10/2026: directed to ARCA, the symbol lookup named
/// ARCA, the primary NASDAQ asked; to NYSE next, nothing; to ISLAND, the
/// symbol lookup named NASDAQ, ISLAND asked, its answer naming NASDAQ; to
/// NASDAQ, nothing; 38 recordings of SMART orders: the primary asked once).
/// None when it has them, or when the primary exchange is not known; with
/// whether the exchange is the primary one.
fn missing_order_record(context: &Context, shared: &SharedState, instrument: crate::types::InstrumentId, con_id: i64) -> Option<(String, bool)> {
    if context.order_records_any.contains(&con_id) {
        return None;
    }
    // Stocks only: a future, a forex pair, a CFD and a warrant went out
    // with no such lookup (paper, 15/10/2026, `.tmp/666`); a contract given
    // by its conId alone, whose type its definition gives, is not seen yet.
    let (sec_type, destination) = context.market.order_routing(instrument);
    if !matches!(sec_type.as_str(), "STK" | "CS") || context.market.blank_symbol(instrument) {
        return None;
    }
    let missing = |exchange: &str| !context.order_records.contains(&(con_id, exchange.to_string()));
    let primary = shared.reference.get_contract(con_id).map(|c| c.primary_exchange).filter(|e| !e.is_empty());
    if !destination.is_empty() && destination != "BEST" && missing(&destination) {
        let is_primary = primary.as_deref() == Some(destination.as_str());
        return Some((destination, is_primary));
    }
    let primary = primary?;
    missing(&primary).then_some((primary, true))
}

/// The lookups of a stock order once its contract is known, as the
/// reference 10.51 makes them (ibx#728; paper, 12-16/10/2026, the first
/// stock order of the session on a contract new to it: 15 orders sent and
/// 6 held with transmit off, a DAY order included):
/// - the trading hours its contract's answer called for, 3 to 17 ms after
///   that answer;
/// - the record of the order's own exchange when the symbol lookup named
///   another one (ISLAND, named NASDAQ: the record asked with the hours, the
///   preset after its answer; one order, 13/10/2026);
/// - the order preset of stocks, at the first stock order of the session
///   whatever its time in force, 1 to 5 ms after the hours (two slow runs
///   aside); the order waits for its answer, not for the hours' (the record
///   left 248 ms or more after the hours in every run, 30 to 60 ms after
///   the preset's answer);
/// - the record of the primary exchange (ibx#706), then the order message
///   after its answer (64 to 153 ms after it in the 15 runs).
/// With the preset known (the next stock contract of the session), the
/// hours and the record leave together and the order 4 to 11 ms after the
/// record's answer (five orders, 13-15/10/2026). Without a record to ask,
/// the order goes after the preset's answer (three orders, 13/10/2026). On a
/// session opened by a logon, the first step, the record and the order keep
/// the reference's gaps (ibx#729, `first_step_gap`, `first_order_gap`).
/// A contract given by its conId alone goes through the same steps once its
/// definition named it, with shorter gaps (ibx#729). Other types: nothing
/// here. The preset as the logon's list says (`ask_order_presets`).
/// False while the order waits for an answer or a gap.
fn stock_order_lookups(
    context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, shared: &Arc<SharedState>,
    order_id: OrderId, instrument: crate::types::InstrumentId, con_id: i64,
) -> bool {
    let sec_type = context.market.order_routing(instrument).0;
    if !matches!(sec_type.as_str(), "STK" | "CS") || context.market.blank_symbol(instrument) {
        return true;
    }
    // The first step a while after the lookup's answer (ibx#729).
    if !first_step_gap(context, con_id) {
        return false;
    }
    send_order_hours(context, conn, hb, con_id);
    // The order's record lookup answered: it goes on, after the preset.
    // Without the copied waits the order does not wait for the records:
    // the server takes a first order without them (ibx#564).
    let missing = if context.order_records_cleared.contains(&order_id) || !context.order_records_wait {
        None
    } else {
        missing_order_record(context, shared, instrument, con_id)
    };
    if let Some((exchange, false)) = &missing {
        ask_order_record_once(context, conn, hb, con_id, exchange);
        return false;
    }
    if !ask_order_presets(context, conn, hb, &["STK".to_string()]) {
        if context.order_gaps {
            context.preset_waiters.insert(order_id);
        }
        return false;
    }
    if let Some((exchange, true)) = &missing {
        // The record a while after the preset's answer (ibx#729).
        if first_order_gap(context, order_id, con_id, true) {
            ask_order_record_once(context, conn, hb, con_id, exchange);
        }
        return false;
    }
    // The order a while after the last answer it waited for (ibx#729).
    if !first_order_gap(context, order_id, con_id, false) {
        return false;
    }
    context.order_records_cleared.remove(&order_id);
    context.preset_waiters.remove(&order_id);
    context.record_answered.remove(&con_id);
    context.lookup_answered.remove(&con_id);
    true
}

/// The gaps the reference 10.51 leaves in a stock's first order, copied
/// with fixed values on a session opened by a logon, as the other copied
/// waits (ibx#729, the owner's rule of ibx#564; paper, 13-16/10/2026, stock
/// orders by symbol, the first of their contract in the session; each
/// value the median of the observed range):
/// - its record 40 ms after the stock order preset's answer (30 to 60 ms,
///   21 orders, held ones included);
const RECORD_AFTER_PRESET: std::time::Duration = std::time::Duration::from_millis(40);
/// - the order 91 ms after the record's answer, when the order waited for
///   the preset (64 to 153 ms, 15 orders);
const ORDER_AFTER_RECORD_FIRST: std::time::Duration = std::time::Duration::from_millis(91);
/// - 5 ms after it with the preset known (4 to 11 ms, 5 orders);
const ORDER_AFTER_RECORD: std::time::Duration = std::time::Duration::from_millis(5);
/// - 130 ms after the preset's answer when no record was asked (121 to
///   197 ms, 3 orders);
const ORDER_AFTER_PRESET: std::time::Duration = std::time::Duration::from_millis(130);
/// - its first step (the trading hours, or the record of its own exchange,
///   the stock order preset 1 to 5 ms after) 10 ms after the symbol
///   lookup's answer, when that step asks the preset (3 to 17 ms in 27 of
///   28 orders, one at 53 ms; with the preset known, 1 to 2 ms, 6 orders).
///   A contract given by its conId: 2 ms after its definition's answer (one
///   order), so no wait.
const FIRST_STEP_AFTER_LOOKUP: std::time::Duration = std::time::Duration::from_millis(10);
/// For a stock given by its conId alone, the first of its contract in the
/// session (13/10/2026 and 16/10/2026, two orders, the second a volatility
/// order on the stock): its record 4 ms after the preset's answer (3.5 and
/// 4.0 ms), the order 31 ms after the record's answer (30.8 and 31.5 ms).
const RECORD_AFTER_PRESET_BY_CON_ID: std::time::Duration = std::time::Duration::from_millis(4);
const ORDER_AFTER_RECORD_BY_CON_ID: std::time::Duration = std::time::Duration::from_millis(31);
/// Other orders' gaps after a preset they waited for (ibx#729; the
/// reference 10.51, paper, 13-16/10/2026, the first order of its type in the
/// session, its preset stored):
/// - a refusal for the permission 93 ms after the preset's answer (a future
///   and a metal limit order 111 and 93 ms, a fund bought at market 11 ms;
///   the trading hours had answered 0.5 to 3.5 ms before the preset in all
///   three, nothing else seen in between);
const REFUSAL_AFTER_PRESET: std::time::Duration = std::time::Duration::from_millis(93);
/// - a forex pair's order 17 ms after its second preset's answer (EUR.USD,
///   13/10/2026, one order).
const PAIR_ORDER_AFTER_PRESET: std::time::Duration = std::time::Duration::from_millis(17);

/// Whether a copied wait ending at `due` is over (always with no wait, or
/// on a session not opened by a logon); when not, the time it ends is noted
/// for the loop (ibx#729).
fn wait_until(context: &mut Context, due: Option<Instant>) -> bool {
    let Some(due) = due.filter(|_| context.order_gaps) else { return true };
    if context.gap_now() >= due {
        return true;
    }
    context.order_wake = Some(context.order_wake.map_or(due, |wake| wake.min(due)));
    false
}

/// Whether a stock's first order is past the copied wait before its first
/// step (`FIRST_STEP_AFTER_LOOKUP`): only when that step asks the stock
/// order preset.
fn first_step_gap(context: &mut Context, con_id: i64) -> bool {
    let Some(&answered) = context.lookup_answered.get(&con_id) else { return true };
    let asks_preset = !context.order_presets.contains_key("STK") && !context.presets_asked.contains("STK")
        && context.presets_stored.as_ref().is_none_or(|stored| stored.contains("STK"));
    if asks_preset && !wait_until(context, Some(answered + FIRST_STEP_AFTER_LOOKUP)) {
        return false;
    }
    context.lookup_answered.remove(&con_id);
    true
}

/// Whether a stock's first order is past the copied gap before its record
/// (`record_next`) or before its message (see `RECORD_AFTER_PRESET`); when
/// not, the time it goes on is noted for the loop. Always past on a session
/// not opened by a logon.
fn first_order_gap(context: &mut Context, order_id: OrderId, con_id: i64, record_next: bool) -> bool {
    if !context.order_gaps {
        return true;
    }
    let preset = context.preset_answered.get("STK").copied().filter(|_| context.preset_waiters.contains(&order_id));
    let (record_after_preset, order_after_record) = if context.by_con_id_contracts.contains(&con_id) {
        (RECORD_AFTER_PRESET_BY_CON_ID, ORDER_AFTER_RECORD_BY_CON_ID)
    } else {
        (RECORD_AFTER_PRESET, ORDER_AFTER_RECORD_FIRST)
    };
    let due = if record_next {
        preset.map(|at| at + record_after_preset)
    } else {
        match (context.record_answered.get(&con_id).copied(), preset) {
            (Some(record), Some(preset)) if record >= preset => Some(record + order_after_record),
            (Some(record), None) => Some(record + ORDER_AFTER_RECORD),
            (_, Some(preset)) => Some(preset + ORDER_AFTER_PRESET),
            (None, None) => None,
        }
    };
    wait_until(context, due)
}

/// A contract's record on an exchange for its orders, unless already on its
/// way (ibx#706).
fn ask_order_record_once(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, con_id: i64, exchange: &str) {
    if !context.order_record_lookups.iter().any(|(_, c, x)| *c == con_id && x == exchange) {
        ask_order_record(context, conn, hb, con_id, exchange);
    }
}

/// The trading hours due for an order's contract (ibx#728), written now,
/// under the name taken when they were found missing.
fn send_order_hours(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, con_id: i64) {
    if !context.order_hours_due.iter().any(|(c, ..)| *c == con_id) {
        return;
    }
    let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut context.order_hours_due).into_iter().partition(|(c, ..)| *c == con_id);
    context.order_hours_due = rest;
    for (_, key, name) in due {
        let ts = chrono_free_timestamp();
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
            (crate::control::contracts::TAG_SUB_PROTOCOL, crate::control::contracts::SUB_PROTOCOL_SCHEDULE_SUBSCRIBE),
            (320, &name), (crate::control::contracts::TAG_SCHEDULE_JOIN_KEY, &key),
        ]);
        hb.last_ccp_sent = Instant::now();
        log::info!("Trading hours of con_id {} asked for its orders ({}, {})", con_id, key, name);
    }
}

/// The trading hours due for contracts no order waits on any more (an order
/// refused or cancelled on the way, a definition asked for a check): written
/// at the end of the pass, so they are never left unasked.
fn flush_order_hours(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState) {
    if context.order_hours_due.is_empty() {
        return;
    }
    let due: Vec<i64> = context.order_hours_due.iter().map(|(c, ..)| *c).collect();
    for con_id in due {
        let of_contract = |r: &OrderRequest| r.new_order_qty().is_some()
            && r.instrument().and_then(|i| context.market.con_id(i)) == Some(con_id);
        let waiting = context.rth_parked.iter().any(of_contract)
            || context.held_lookups.iter().any(|(id, _)| context.unsent.get(id).and_then(|h| context.market.con_id(h.0)) == Some(con_id));
        if !waiting {
            send_order_hours(context, conn, hb, con_id);
        }
    }
}

/// Ask a contract's record on an exchange for an order (ibx#706).
fn ask_order_record(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, con_id: i64, exchange: &str) {
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let name = format!("{}{}", crate::control::contracts::SECDEF_ORDER_TYPES_NAME, lookup_id);
    let con = con_id.to_string();
    let ts = chrono_free_timestamp();
    let _ = conn.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
        (6008, &con), (6004, exchange),
    ]);
    hb.last_ccp_sent = std::time::Instant::now();
    log::info!("Order contract {}: record on {} asked ({})", con_id, exchange, name);
    context.order_record_lookups.push((name, con_id, exchange.to_string()));
}

/// The clearing rule lookups an order's record answer asked for (ibx#709),
/// by conId on the exchange asked, named as the reference names them
/// (`ClearingRule<k>ND`, k counting them in the session).
pub(crate) fn send_clearing_rules(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState) {
    for (con_id, exchange) in std::mem::take(&mut context.order_clearing_to_send) {
        let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
        context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
        let name = format!("ClearingRule{}NDReqByConid{}", context.next_clearing_rule, lookup_id);
        context.next_clearing_rule = context.next_clearing_rule.wrapping_add(1);
        let con = con_id.to_string();
        let ts = chrono_free_timestamp();
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "c"), (fix::TAG_SENDING_TIME, &ts), (320, &name), (321, "2"), (146, "1"),
            (6008, &con), (6004, &exchange),
        ]);
        hb.last_ccp_sent = std::time::Instant::now();
        log::info!("Order contract {}: clearing rule on {} asked ({})", con_id, exchange, name);
        context.order_clearing_lookups.push(name);
    }
}

/// The answer to a clearing rule lookup (ibx#709): taken, nothing waits
/// for it. False when the reply is not for one.
pub(crate) fn order_clearing_reply(context: &mut Context, req_id: &str) -> bool {
    let Some(at) = context.order_clearing_lookups.iter().position(|name| name == req_id) else { return false };
    context.order_clearing_lookups.remove(at);
    true
}

/// The answer to an order's record lookup (ibx#706): the records it holds
/// are kept by the exchange they name, its order rules under the exchange
/// asked, and the orders of the contract that waited go on. False when the
/// reply is not for such a lookup.
pub(crate) fn order_record_reply(context: &mut Context, req_id: &str, msg: &[u8]) -> bool {
    let Some(at) = context.order_record_lookups.iter().position(|(name, ..)| name == req_id) else { return false };
    let (_, con_id, asked) = context.order_record_lookups.remove(at);
    // For the gap before the order (ibx#729).
    if context.order_gaps {
        let now = context.gap_now();
        context.record_answered.insert(con_id, now);
    }
    if let Some(named) = fix::fix_parse(msg).get(&207).filter(|e| !e.is_empty()) {
        context.order_records.insert((con_id, named.clone()));
    }
    // An answer naming another exchange than the one asked (ISLAND, answered
    // as NASDAQ): the reference then asks the clearing rule of the exchange
    // asked, and goes on without its answer (ibx#709; nine orders directed
    // to ISLAND, 12/10 to 15/10/2026; none after an answer naming the
    // exchange asked).
    let named = fix::fix_parse(msg).get(&207).cloned().unwrap_or_default();
    if !named.is_empty() && named != asked {
        context.order_clearing_to_send.push((con_id, asked.clone()));
    }
    for def in crate::control::contracts::parse_secdef_records(msg).unwrap_or_default() {
        if def.con_id == con_id && !def.exchange.is_empty() {
            context.order_records.insert((con_id, def.exchange.clone()));
        }
    }
    let key = (con_id, asked);
    if !context.rth_types.contains_key(&key) {
        store_rth_types(context, key, msg);
    }
    let waiting: Vec<crate::types::OrderId> = context.rth_parked.iter()
        .filter(|r| r.new_order_qty().is_some() && r.instrument().and_then(|i| context.market.con_id(i)) == Some(con_id))
        .flat_map(request_order_ids).collect();
    context.order_records_cleared.extend(waiting);
    release_rth_parked(context);
    true
}

/// Ask the contract of an order's slot by symbol, once per slot, as the
/// reference asks it before each API order (ibx#486; captured: the
/// `FixSecDefReqBySymbol` of contract details, source `Socket`, exchange `BEST`).
fn look_up_order_contract(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, instrument: crate::types::InstrumentId) {
    if context.order_lookups.iter().any(|(_, i)| *i == instrument) {
        return;
    }
    // An order placed while the lookup of the same contract is on its way
    // takes its answer (ibx#547; the bracket recordings of 26/09/2026:
    // three orders placed at once, one lookup; placed three seconds apart
    // on 09/10/2026, one lookup each).
    let filters = context.order_lookup_filters.get(&instrument).cloned().unwrap_or_default();
    let same = |c: &Context, a, b| {
        let m = &c.market;
        m.symbol(a) == m.symbol(b) && m.order_routing(a) == m.order_routing(b)
            && m.exchange(a) == m.exchange(b) && m.currency(a) == m.currency(b)
            && c.order_lookup_filters.get(&a) == c.order_lookup_filters.get(&b)
    };
    if let Some(&(in_flight, _)) = context.order_lookups.iter().find(|(_, other)| same(context, *other, instrument)) {
        context.order_lookups.push((in_flight, instrument));
        return;
    }
    let lookup_id = ORDER_LOOKUP_FIRST_ID + context.next_order_lookup % ORDER_LOOKUP_IDS;
    context.next_order_lookup = context.next_order_lookup.wrapping_add(1);
    let (sec_type, _) = context.market.order_routing(instrument);
    let sec_type = if sec_type == "CS" { "STK".to_string() } else { sec_type };
    let lookup = super::ccp::SymbolLookup {
        symbol: context.market.symbol(instrument).to_string(),
        sec_type,
        // An order for the broker's ATS venue is looked up on SMART, as
        // the reference looks it up (ibx#667; observed 13/10/2026,
        // pre-market, paper: AAPL for the venue, the lookup on BEST).
        exchange: match context.market.exchange(instrument) {
            "IBKRATS" => "SMART".to_string(),
            other => other.to_string(),
        },
        currency: context.market.currency(instrument).to_string(),
        filters,
        continuous: false,
    };
    super::ccp::send_order_lookup_on(conn, crate::types::ReqId::from(lookup_id), &lookup);
    hb.last_ccp_sent = std::time::Instant::now();
    log::info!("Order contract lookup {} for {} {}", lookup_id, lookup.symbol, lookup.sec_type);
    context.order_lookups.push((lookup_id, instrument));
}

/// An order refused with 200 at its contract's lookup was never sent and
/// is not an open order: the client forgets it, after the error, so no
/// open-order request lists it (ibx#738; the reference 10.51, paper,
/// 17/10/2026, market closed: an order refused with 200 at its lookup, its
/// cancel 10147, then reqOpenOrders listed nothing for it). A what-if, or
/// a request on an order the engine holds, leaves the order as it is.
fn drop_refused_at_lookup(context: &Context, shared: &SharedState, req: &OrderRequest) {
    if matches!(req, OrderRequest::SubmitWhatIf { .. }) {
        return;
    }
    for oid in request_order_ids(req) {
        if context.order(oid).is_none() {
            shared.orders.push_forgotten_order(oid);
        }
    }
}

/// The server refused an order's contract lookup (ibx#710): the orders that
/// waited for it get 200 with the server's text and are not kept (their
/// cancel gives 10147), as the reference. False when the refusal is not of
/// such a lookup.
pub(crate) fn order_lookup_refused(context: &mut Context, shared: &SharedState, req_id: &str, text: &str) -> bool {
    let Some(number) = crate::control::contracts::secdef_request_number(req_id) else { return false };
    let slots: Vec<crate::types::InstrumentId> = context.order_lookups.iter()
        .filter(|(id, _)| crate::types::ReqId::from(*id) == number).map(|(_, slot)| *slot).collect();
    if slots.is_empty() {
        return false;
    }
    context.order_lookups.retain(|(id, _)| crate::types::ReqId::from(*id) != number);
    for slot in slots {
        log::warn!("Order contract lookup {} refused: {}", req_id, text);
        let (refused, kept): (Vec<OrderRequest>, Vec<OrderRequest>) = std::mem::take(&mut context.rth_parked).into_iter()
            .partition(|r| r.instrument() == Some(slot) && !matches!(r, OrderRequest::CancelAll { .. }));
        context.rth_parked = kept;
        for r in refused {
            shared.orders.push_order_error(r.order_id(), 200, text.to_string());
            drop_refused_at_lookup(context, shared, &r);
        }
        for group in context.order_groups.iter_mut() {
            group.retain(|m| m.1 != slot);
        }
        if !context.unsent.values().any(|h| h.0 == slot) {
            context.market.unregister(slot);
        }
    }
    release_rth_parked(context);
    true
}

/// The answer to an order's contract lookup (ibx#486): one contract gives
/// the slot its conId (or the waiting orders the slot of that conId when
/// there is one already); none or several give error 200 to the waiting
/// orders, which are not kept (their cancel gives 10147). False when the
/// reply is not for such a lookup. The cache of definitions takes the
/// reply's records as any other reply.
pub(crate) fn order_contract_reply(context: &mut Context, shared: &SharedState, req_id: &str, msg: &[u8]) -> bool {
    let Some(number) = crate::control::contracts::secdef_request_number(req_id) else { return false };
    // The slots that wait for this answer: the one that asked, and those
    // of the orders placed while it was on its way.
    let slots: Vec<crate::types::InstrumentId> = context.order_lookups.iter()
        .filter(|(id, _)| crate::types::ReqId::from(*id) == number).map(|(_, slot)| *slot).collect();
    if slots.is_empty() { return false; }
    context.order_lookups.retain(|(id, _)| crate::types::ReqId::from(*id) != number);
    let mut con_ids: Vec<i64> = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default()
        .iter().map(|d| d.con_id).filter(|c| *c != 0).collect();
    con_ids.sort_unstable();
    con_ids.dedup();
    if let [con_id] = con_ids[..] {
        note_min_size(context, con_id, &fix::fix_parse(msg));
    }
    for slot in slots {
        if let [con_id] = con_ids[..] {
            // Named for the session: a later held order of the same
            // contract is known at once (ibx#718).
            let key = order_contract_key(context, slot);
            match context.order_contracts_named.iter_mut().find(|(k, _)| *k == key) {
                Some(named) => named.1 = con_id,
                None => context.order_contracts_named.push((key, con_id)),
            }
            // The slot of that conId when it routes the same way: the
            // waiting orders take it and this slot is freed; else this slot
            // keeps the conId.
            // The broker's ATS venue is written as the smart route on the
            // wire and is another exchange all the same: an order for it
            // keeps its own slot (the reference 10.51, paper, regular hours
            // 13/11/2026: a not-held KO order given by its symbol for that
            // venue after KO orders on SMART, discarded by the redirect
            // precaution naming the venue).
            let given = |m: &crate::engine::market_state::MarketState, i| {
                let e = m.exchange(i);
                if e.is_empty() { "SMART".to_string() } else { e.to_ascii_uppercase() }
            };
            let same_route = |m: &crate::engine::market_state::MarketState, a, b| {
                m.order_routing(a) == m.order_routing(b) && m.currency(a) == m.currency(b) && given(m, a) == given(m, b)
            };
            match context.market.instrument_by_con_id(con_id).filter(|&known| known != slot && same_route(&context.market, known, slot)) {
                Some(known) => {
                    for req in context.rth_parked.iter_mut() {
                        if let Some(i) = req.new_order_instrument_mut().filter(|i| **i == slot) {
                            *i = known;
                        }
                    }
                    // An order held with transmit off takes it too (ibx#509).
                    for held in context.unsent.values_mut().filter(|h| h.0 == slot) {
                        held.0 = known;
                    }
                    for member in context.order_groups.iter_mut().flatten().filter(|m| m.1 == slot) {
                        member.1 = known;
                    }
                    context.market.unregister(slot);
                }
                None => context.market.resolve_con_id(slot, con_id),
            }
            // For the gap before a stock's first step (ibx#729).
            if context.order_gaps {
                let now = context.gap_now();
                context.lookup_answered.insert(con_id, now);
            }
            // The reply holds the contract's record on the order's
            // exchange: its order rules are taken from it, no second
            // definition is asked, as the reference uses its lookup's
            // record (ibx#669; the reference's lookup replies carry the
            // order types and valid exchanges, 13/10/2026).
            let instrument = context.market.instrument_by_con_id(con_id).unwrap_or(slot);
            let destination = context.market.order_routing(instrument).1;
            let replied = fix::fix_parse(msg).get(&207).cloned().unwrap_or_default();
            // The session has the contract's record on the exchange the
            // answer names (ibx#706).
            if !replied.is_empty() {
                context.order_records.insert((con_id, replied.clone()));
            }
            let key = (con_id, destination);
            if replied == key.1 && !context.rth_types.contains_key(&key) {
                store_rth_types(context, key, msg);
            }
            log::info!("Order contract lookup {}: conId {}", req_id, con_id);
        } else {
            log::warn!("Order contract lookup {}: {} contracts: error 200", req_id, con_ids.len());
            let (refused, kept): (Vec<OrderRequest>, Vec<OrderRequest>) = std::mem::take(&mut context.rth_parked).into_iter()
                .partition(|r| r.instrument() == Some(slot) && !matches!(r, OrderRequest::CancelAll { .. }));
            context.rth_parked = kept;
            // The order is not kept: its cancel finds nothing, 10147, as
            // the reference (paper, 13/10/2026, regular hours: an AAPL
            // order for JONES, 200 then 10147).
            for r in refused {
                let oid = r.order_id();
                shared.orders.push_order_error(oid, crate::engine::combo::NO_DEFINITION.0, crate::engine::combo::NO_DEFINITION.1.to_string());
                drop_refused_at_lookup(context, shared, &r);
            }
            // The orders of a group on another contract go on without it.
            for group in context.order_groups.iter_mut() {
                group.retain(|m| m.1 != slot);
            }
            // A held order keeps the slot: its contract is asked again when
            // the order is transmitted, and refused then (ibx#509).
            if !context.unsent.values().any(|h| h.0 == slot) {
                context.market.unregister(slot);
            }
        }
    }
    release_rth_parked(context);
    true
}

/// Text of the redirect precaution, as the reference writes it: 10311
/// "Routing_warning", 10329 "Routing_warning_for_overnight" for OVERNIGHT
/// and IBEOS (the reference's overnight venues), then the API precaution line
/// (`API_Precautionary_Settings_Specified`), as captured on 28/09/2026.
pub(crate) fn redirect_warning(exchange: &str) -> (i64, String) {
    const API_PRECAUTION: &str = "Restriction is specified in Precautionary Settings of Global Configuration/API.";
    match exchange {
        "OVERNIGHT" | "IBEOS" => (10329, format!("This order will be directly routed to {}.\n{}", exchange, API_PRECAUTION)),
        _ => (10311, format!("This order will be directly routed to {}. Direct routed orders may result in higher trade fees.\n{}",
            exchange, API_PRECAUTION)),
    }
}

/// The reference's redirect precaution for API orders
/// (ibx#486): a stock
/// order whose exchange is set and is not SMART, IBDESK or ZERO, on a
/// contract that trades on SMART, with the setting
/// "Bypass Redirect Order warning for Stock API Orders" off (the default;
/// `IBX_BYPASS_REDIRECT_ORDER_WARNING`): 10311 (10329 for OVERNIGHT and
/// IBEOS), then the order is discarded: orderStatus Cancelled with nothing
/// filled, then 201 "Order was discarded." (captured 28/09/2026 on OVERNIGHT, 25/09/2026 on ISLAND, named NASDAQ by
/// 10.40; 10.51 names it ISLAND, 15/10/2026).
/// Its permId is the id it would have gone out under. `Some(false)` while
/// the contract's definition on that exchange is asked, `Some(true)` when
/// discarded, `None` to go on.
/// Whether the redirect precaution looks at this order: a stock order
/// directed away from SMART, not to an algo provider, the precaution on.
fn redirect_applies(req: &OrderRequest, context: &Context) -> bool {
    if context.bypass_redirect_warning || req.combo().is_some()
        || matches!(req, OrderRequest::SubmitBracket { .. } | OrderRequest::CancelAll { .. })
        || req.new_order_qty().is_none()
    {
        return false;
    }
    let Some(instrument) = req.instrument() else { return false };
    let exchange = context.market.exchange(instrument);
    context.market.order_routing(instrument).0 == "STK"
        && !matches!(exchange, "" | "SMART" | "IBDESK" | "ZERO")
        && !context.algo_providers.iter().any(|p| p.eq_ignore_ascii_case(exchange))
}

fn redirect_precaution(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    if context.bypass_redirect_warning || req.combo().is_some()
        || matches!(req, OrderRequest::SubmitBracket { .. } | OrderRequest::CancelAll { .. })
    {
        return None;
    }
    let qty = req.new_order_qty()?;
    let instrument = req.instrument()?;
    let (sec_type, _) = context.market.order_routing(instrument);
    let exchange = context.market.exchange(instrument).to_string();
    if sec_type != "STK" || matches!(exchange.as_str(), "" | "SMART" | "IBDESK" | "ZERO") {
        return None;
    }
    // An algo provider's destination goes through SMART: no precaution
    // (ibx#505; observed 13/10/2026, regular hours, reference 10.51 with
    // the precaution on: FOXRIVER sent, ARCA discarded with 10311).
    if context.algo_providers.iter().any(|p| p.eq_ignore_ascii_case(&exchange)) {
        return None;
    }
    match definition(context, conn, hb, instrument) {
        Definition::NoContract => None,
        Definition::Waiting => Some(false),
        Definition::Known(types, _) if !types.smart => None,
        Definition::Known(..) => {
            let oid = req.order_id();
            // 10.51 names the exchange as the order gives it (paper,
            // 15/10/2026, regular hours: "routed to ISLAND", twice); 10.40
            // named ISLAND as NASDAQ (captured 25/09/2026).
            let shown = if exchange == "ISLAND" && shared.reference.form_10_40() { "NASDAQ" } else { exchange.as_str() };
            let (code, text) = redirect_warning(shown);
            log::warn!("Order {} discarded: directed to {} (redirect precaution)", oid, exchange);
            shared.orders.push_order_error(oid, code, text);
            // The order is numbered when it is placed, sent or not: its
            // Cancelled status has that permId (the reference 10.51,
            // paper, regular hours 13/11/2026: three orders discarded in a
            // row, the three ids after the last order sent).
            let perm_id = context.new_server_id();
            shared.orders.push_order_update(OrderUpdate {
                order_id: oid,
                instrument,
                status: OrderStatus::Cancelled,
                filled_qty_fixed: 0,
                remaining_qty_fixed: qty,
                avg_fill_price: 0,
                perm_id,
                parent_id: req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id),
                timestamp_ns: context.now_ns(),
            });
            shared.orders.push_order_notice(oid, 201, "Order rejected - reason:Order was discarded.".into());
            Some(true)
        }
    }
}

/// An order directed to an exchange its contract does not list among its
/// valid exchanges (from the contract's definition on that exchange, BEST
/// counted as SMART) is refused with 464 "Invalid exchange" and nothing is
/// sent; kept, not working, its cancel ends it with Cancelled, 202 and 161
/// (ibx#630; the reference 10.51 and 10.40 check it after the redirect and
/// route-to-BBO precautions; paper
/// 12/10 and 13/10/2026: AAPL to ISLAND refused, to NASDAQ sent). Not
/// checked: the broker's ATS venue, AVGCOST and IBDESK, as the reference,
/// and a definition without a list. `Some(false)` while the definition is
/// asked, `Some(true)` when refused, `None` to go on.
fn invalid_exchange_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    if req.combo().is_some() || matches!(req, OrderRequest::SubmitBracket { .. } | OrderRequest::CancelAll { .. }) {
        return None;
    }
    let qty = req.new_order_qty()?;
    let instrument = req.instrument()?;
    let exchange = context.market.exchange(instrument).to_ascii_uppercase();
    if matches!(exchange.as_str(), "" | "SMART" | "IBKRATS" | "AVGCOST" | "IBDESK") {
        return None;
    }
    match definition(context, conn, hb, instrument) {
        Definition::NoContract => None,
        Definition::Waiting => Some(false),
        Definition::Known(types, _) => {
            if types.not_found {
                // The contract has no definition on that exchange: 200 and
                // nothing sent, the order gone (its cancel 10147), as the
                // reference 10.51 (ibx#743; paper, 17/10/2026, market
                // closed: AAPL by conId on a made-up exchange).
                let oid = req.order_id();
                log::warn!("Order {} refused: no definition of its contract on {}", oid, exchange);
                shared.orders.push_order_error(oid, crate::engine::combo::NO_DEFINITION.0, crate::engine::combo::NO_DEFINITION.1.to_string());
                drop_refused_at_lookup(context, shared, req);
                return Some(true);
            }
            let listed = types.valid_exchanges.as_ref()?.iter().any(|e| e.eq_ignore_ascii_case(&exchange));
            if listed {
                return None;
            }
            let oid = req.order_id();
            log::warn!("Order {} refused: {} is not a valid exchange of its contract", oid, exchange);
            shared.orders.push_order_error(oid, 464, "Invalid exchange".to_string());
            let parent = req.new_order_side().and_then(|(_, a)| a).map_or(0, |a| a.parent_id);
            context.keep_unsent(oid, (instrument, qty, parent));
            Some(true)
        }
    }
}

/// An overnight order (includeOvernight, or the OVERNIGHT or IBEOS
/// exchange) on a contract whose valid exchanges have no overnight one,
/// or no SMART, BEST or ZERO: 10362 and nothing sent, the order gone (its
/// cancel 10147), as the reference 10.51 (ibx#660; paper, 13/10/2026, an
/// AAPL option with includeOvernight, after the contract's lookup).
fn overnight_refusal(
    req: &OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> Option<bool> {
    if req.combo().is_some() || matches!(req, OrderRequest::SubmitBracket { .. } | OrderRequest::CancelAll { .. }) {
        return None;
    }
    req.new_order_qty()?;
    let instrument = req.instrument()?;
    let exchange = context.market.exchange(instrument).to_ascii_uppercase();
    let include = req.new_order_side().and_then(|(_, attrs)| attrs).is_some_and(|a| a.include_overnight);
    if !include && !matches!(exchange.as_str(), "OVERNIGHT" | "IBEOS") {
        return None;
    }
    match definition(context, conn, hb, instrument) {
        Definition::NoContract => None,
        Definition::Waiting => Some(false),
        Definition::Known(types, _) => {
            let valid = types.valid_exchanges.as_ref()?;
            let has = |names: &[&str]| valid.iter().any(|e| names.iter().any(|n| e.eq_ignore_ascii_case(n)));
            if has(&["OVERNIGHT", "IBEOS"]) && has(&["SMART", "BEST", "ZERO"]) {
                return None;
            }
            let oid = req.order_id();
            log::warn!("Order {} refused: its contract has no overnight trading", oid);
            shared.orders.push_order_error(oid, OVERNIGHT_NOT_SUPPORTED.0, OVERNIGHT_NOT_SUPPORTED.1.to_string());
            Some(true)
        }
    }
}

/// Code and text of the refusal of an overnight order (the reference
/// 10.51, paper 13/10/2026; with its final period).
pub(crate) const OVERNIGHT_NOT_SUPPORTED: (i64, &str) = (10362, "OVERNIGHT order is not supported.");

/// The reference's precaution for API orders with routeMarketableToBbo
/// (ibx#608, 10.51, the order checks of the confirmation;
/// paper 12/10/2026, on LMT and MKT): with the setting "Bypass Route
/// Marketable to BBO Warning" off (the default;
/// `IBX_BYPASS_ROUTE_MARKETABLE_TO_BBO_WARNING`), warning 10353, then the
/// order is discarded as by the redirect precaution: orderStatus Cancelled
/// with nothing filled, then 201 "Order was discarded.". Nothing is sent.
fn route_bbo_precaution(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    if context.bypass_route_bbo_warning { return false; }
    let Some((_, Some(attrs))) = req.new_order_side() else { return false };
    if !attrs.route_marketable_to_bbo { return false; }
    let (Some(qty), Some(instrument)) = (req.new_order_qty(), req.instrument()) else { return false };
    let oid = req.order_id();
    log::warn!("Order {} discarded: route marketable to BBO (precaution)", oid);
    shared.orders.push_order_error(oid, 10353, ROUTE_BBO_WARNING.to_string());
    // Numbered as any order placed (see the redirect precaution).
    let perm_id = context.new_server_id();
    shared.orders.push_order_update(OrderUpdate {
        order_id: oid,
        instrument,
        status: OrderStatus::Cancelled,
        filled_qty_fixed: 0,
        remaining_qty_fixed: qty,
        avg_fill_price: 0,
        perm_id,
        parent_id: attrs.parent_id,
        timestamp_ns: context.now_ns(),
    });
    shared.orders.push_order_notice(oid, 201, "Order rejected - reason:Order was discarded.".into());
    true
}

/// Attached orders asked from the order preset when the preset does not
/// auto-attach them (ibx#609; the reference 10.51, paper, 16/10/2026,
/// both legs: 10355 for the profit taker 0.79 s after the order, then
/// orderStatus Cancelled with nothing filled and a permId, then 202 "Order
/// was discarded."; nothing sent). The profit-taker leg is named first,
/// then the stop-loss leg; one refusal only. The presets of an account
/// have both auto-attach switches off unless set by hand; ibx does not
/// build the attached orders a preset would define (#717).
fn attached_preset_refusal(req: &OrderRequest, context: &mut Context, shared: &Arc<SharedState>) -> bool {
    let Some((_, Some(attrs))) = req.new_order_side() else { return false };
    let leg = match attrs.attached_from_preset {
        (true, _) => "Profit Taker",
        (false, true) => "Stop Loss",
        (false, false) => return false,
    };
    let (Some(qty), Some(instrument)) = (req.new_order_qty(), req.instrument()) else { return false };
    let oid = req.order_id();
    log::warn!("Order {} discarded: {} from a preset that does not define it", oid, leg);
    shared.orders.push_order_error(oid, 10355, format!("Cannot auto-attach {}. Preset is not defined.", leg));
    let perm_id = context.new_server_id();
    shared.orders.push_order_update(OrderUpdate {
        order_id: oid,
        instrument,
        status: OrderStatus::Cancelled,
        filled_qty_fixed: 0,
        remaining_qty_fixed: qty,
        avg_fill_price: 0,
        perm_id,
        parent_id: attrs.parent_id,
        timestamp_ns: context.now_ns(),
    });
    shared.orders.push_order_notice(oid, 202, "Order Canceled - reason:Order was discarded.".into());
    true
}

/// Seek price improvement is written only on an order routed to SMART
/// (ibx#608, 10.51: the attribute's own rule); elsewhere it is
/// left out without a warning, and the order's openOrder shows it off.
fn drop_seek_off_smart(req: &mut OrderRequest, context: &Context, shared: &Arc<SharedState>) {
    let oid = req.order_id();
    let instrument = req.instrument().or_else(|| context.order(oid).map(|o| o.instrument));
    let Some(attrs) = crate::engine::outside_rth::flag_attrs(req) else { return };
    if !attrs.seek_price_improvement { return; }
    let Some(instrument) = instrument else { return };
    if !matches!(context.market.exchange(instrument), "" | "SMART") {
        attrs.seek_price_improvement = false;
        shared.orders.note_dropped_flags(oid, crate::bridge::DROPPED_SEEK_PRICE_IMPROVEMENT);
    }
}

/// Text of warning 10353 (paper 12/10/2026).
pub(crate) const ROUTE_BBO_WARNING: &str = "Warning: Setting \"Route marketable orders to exchanges at BBO only\" \
    order attribute can result in higher total cost of the trade after exchange fees and commissions.";

/// An order placed with no time in force takes the one of the account's
/// order preset for its security type, with notice 10349, as the
/// reference at the session level ibx gives (observed 13/10/2026, paper:
/// "Order TIF was set to GTC based on order preset.", then the order goes
/// out GTC). The preset is asked from the server at the first such order
/// of a type and kept for the session (ibx#660). A preset with no time in
/// force leaves DAY. False while the preset is on its way.
fn preset_tif(
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
    req: &mut OrderRequest,
    sec_type: &str,
) -> bool {
    let oid = req.order_id();
    let Some(tif) = req.preset_tif_mut() else { return true };
    if !ask_order_presets(context, conn, hb, &[sec_type.to_string()]) {
        return false;
    }
    let name = context.order_presets.get(sec_type).cloned().flatten();
    set_tif_from_preset(tif, name.as_deref(), oid, shared);
    true
}

/// An order placed with no time in force takes its preset's, with notice
/// 10349 (ibx#660); a preset with none leaves DAY.
fn set_tif_from_preset(tif: &mut u8, name: Option<&str>, oid: OrderId, shared: &Arc<SharedState>) {
    let Some(name) = name else { return };
    let code = match name {
        "GTC" => b'1',
        "IOC" => b'3',
        "FOK" => b'4',
        "OPG" => b'2',
        "DAY" => b'0',
        _ => return,
    };
    *tif = code;
    shared.orders.push_order_error(oid, 10349, format!("Order TIF was set to {name} based on order preset."));
}

/// The types whose first order asks its preset after its permission
/// question (`type_preset`, ibx#729).
const TYPE_PRESETS: [&str; 8] = ["FUT", "CFD", "WAR", "IOPT", "CMDTY", "FUND", "BILL", "CASH"];

/// The order preset kinds of an order's type, in the order the reference
/// asks them: a forex pair's currency, then the forex one (ibx#729; the
/// reference 10.51, paper, 13/10/2026: EUR.USD); one for another type.
fn preset_kinds(sec_type: &str, symbol: &str) -> Vec<String> {
    if sec_type == "CASH" && !symbol.is_empty() {
        vec![format!("CASH&tc={symbol}"), "CASH".to_string()]
    } else {
        vec![sec_type.to_string()]
    }
}

/// Whether the order presets of `kinds` are known (ibx#729). A kind the
/// account has stored (the logon's list) is asked once and the order waits
/// for its answer; one it has not stored is known as the default, with no
/// time in force, and saved once the order's message is out (the order does
/// not wait). Before the list answered, a kind is asked as a stored one.
/// The kinds are taken in turn: the next is asked once the one before
/// answered.
fn ask_order_presets(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, kinds: &[String]) -> bool {
    for kind in kinds {
        if context.order_presets.contains_key(kind) {
            continue;
        }
        if context.presets_stored.as_ref().is_some_and(|stored| !stored.contains(kind)) {
            log::info!("Order preset of {} not stored: saved after the order", kind);
            context.order_presets.insert(kind.clone(), None);
            context.preset_saves_due.push(kind.clone());
            continue;
        }
        ask_order_preset(context, conn, hb, kind);
        return false;
    }
    true
}

/// The order preset of a future, a CFD, a warrant, a metal, a fund or a
/// forex pair order, once its permission question is out (ibx#729; the
/// reference 10.51, paper, 13-16/10/2026, the first order of each type in
/// its session, DAY orders):
/// - a type whose preset the account has stored (the logon's list): asked
///   after the permission question and the trading hours, and waited for
///   (a future 13/10/2026, a metal and a fund 16/10/2026; the permission's
///   answer, no permission, came meanwhile: 460 and nothing sent);
/// - a type it has not stored: the order goes with the permission question
///   on its way, and the type's preset is saved 1 to 6 ms after the order's
///   message, not waited for (a CFD and a future 13/10/2026, a warrant
///   15/10/2026, a metal and a fund 16/10/2026);
/// - a forex pair: the preset of the pair's currency, then the forex one
///   once that answered, the trading hours with the first, the order once
///   both answered (both stored; EUR.USD, 13/10/2026).
/// Once per kind in the session. An order with no time in force takes the
/// preset's, the pair's currency first. False while it waits for an answer.
fn type_preset(
    req: &mut OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> bool {
    if req.combo().is_some() {
        return true;
    }
    let Some(instrument) = req.new_order_qty().and(req.instrument()) else { return true };
    let sec_type = context.market.order_routing(instrument).0;
    if !TYPE_PRESETS.contains(&sec_type.as_str()) {
        return true;
    }
    let kinds = preset_kinds(&sec_type, context.market.symbol(instrument));
    if kinds.iter().any(|k| !context.order_presets.contains_key(k))
        && let Some(con_id) = context.market.con_id(instrument).filter(|c| *c > 0)
    {
        send_order_hours(context, conn, hb, con_id);
    }
    let oid = req.order_id();
    if !ask_order_presets(context, conn, hb, &kinds) {
        if context.order_gaps {
            context.preset_waiters.insert(oid);
        }
        return false;
    }
    // A forex pair's order a while after the preset it waited for
    // (ibx#729, `PAIR_ORDER_AFTER_PRESET`).
    if context.preset_waiters.contains(&oid) {
        let answered = kinds.iter().filter_map(|k| context.preset_answered.get(k).copied()).max();
        if sec_type == "CASH" && !wait_until(context, answered.map(|at| at + PAIR_ORDER_AFTER_PRESET)) {
            return false;
        }
        context.preset_waiters.remove(&oid);
    }
    if let Some(tif) = req.preset_tif_mut() {
        let name = kinds.iter().find_map(|k| context.order_presets.get(k).cloned().flatten());
        set_tif_from_preset(tif, name.as_deref(), oid, shared);
    }
    true
}

/// The order presets due to be saved (ibx#729): each with the default
/// content and no time in force, under the next request number of the
/// presets, as the reference 10.51 saves the preset of a type the account
/// has not stored (paper, 13-16/10/2026, five types). Its answer is not
/// waited for.
fn send_preset_saves(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState) {
    if context.preset_saves_due.is_empty() {
        return;
    }
    for kind in std::mem::take(&mut context.preset_saves_due) {
        let request_id = format!("OPR.{}", context.next_preset_request);
        context.next_preset_request += 1;
        let named = format!("s={kind}");
        let now = chrono_free_timestamp();
        match conn.send_fix(&[(35, "U"), (52, &now), (6556, &request_id), (6040, "193"),
            (8166, "S"), (8168, &named), (8169, "v=1&a=1")])
        {
            Ok(()) => hb.last_ccp_sent = Instant::now(),
            Err(e) => log::error!("Order preset save not sent: {}", e),
        }
        log::info!("Order preset of {} saved ({})", kind, request_id);
        if let Some(stored) = context.presets_stored.as_mut() {
            stored.insert(kind);
        }
    }
}

/// The logon's list of the order presets the account has stored
/// (ibx#729): the kinds a first order asks and waits for.
pub(crate) fn preset_list_reply<'a>(context: &mut Context, kinds: impl Iterator<Item = &'a str>) {
    let stored: std::collections::HashSet<String> = kinds.filter_map(|k| k.strip_prefix("s=")).map(str::to_string).collect();
    log::info!("Order presets stored for the account: {:?}", stored);
    context.presets_stored = Some(stored);
}

/// Whether the order preset of a kind (a security type, or a forex pair's
/// currency) is known; when not, it is asked once (ibx#660, ibx#717) and
/// the order waits for the answer.
fn ask_order_preset(context: &mut Context, conn: &mut Connection, hb: &mut HeartbeatState, sec_type: &str) -> bool {
    if context.order_presets.contains_key(sec_type) {
        return true;
    }
    if context.presets_asked.insert(sec_type.to_string()) {
        let request_id = format!("OPR.{}", context.next_preset_request);
        context.next_preset_request += 1;
        let kind = format!("s={sec_type}");
        let now = chrono_free_timestamp();
        match conn.send_fix(&[(35, "U"), (52, &now), (6556, &request_id), (6040, "193"),
            (8166, "G"), (8176, "1"), (8168, &kind)])
        {
            Ok(()) => hb.last_ccp_sent = Instant::now(),
            Err(e) => log::error!("Order preset request not sent: {}", e),
        }
    }
    false
}

/// The answer of an order preset request (ibx#660): the time in force it
/// sets for its kind (a security type, or a forex pair's currency,
/// ibx#729); the orders that waited for it go on.
pub(crate) fn preset_reply(context: &mut Context, parsed: &fix::FieldMap<'_>) {
    let Some(kind) = parsed.get(&8168).and_then(|k| k.strip_prefix("s=")) else { return };
    let kind = kind.to_string();
    let tif = parsed.get(&4026).map(|s| s.to_string());
    log::info!("Order preset of {}: time in force {:?}", kind, tif);
    // For the gaps of the orders that waited for it (ibx#729).
    if context.order_gaps {
        let now = context.gap_now();
        context.preset_answered.insert(kind.clone(), now);
    }
    context.presets_asked.remove(&kind);
    context.order_presets.insert(kind, tif);
    release_rth_parked(context);
}

/// The trading hours of order contracts came (ibx#668): the orders that
/// waited for their zone go on.
pub(crate) fn zones_known(context: &mut Context, still_waiting: impl Fn(i64) -> bool) {
    let before = context.zone_pending.len();
    context.zone_pending.retain(|con_id| still_waiting(*con_id));
    if context.zone_pending.len() != before {
        release_rth_parked(context);
    }
}

/// The waiting requests go back ahead of the queue, in their order.
pub(crate) fn release_rth_parked(context: &mut Context) {
    let parked = std::mem::take(&mut context.rth_parked);
    context.pending_orders.prepend(parked);
}

/// The limit offset of a TRAIL LIMIT set by its limit price, or of a
/// TRAIL LIT, as the reference computes it when the order has none
/// (ib-agent#195): the stop price minus the limit price for a sell, the
/// limit price minus the stop price for a buy.
fn computed_trail_limit_offset(kind: crate::types::OrderKind, side: Option<Side>) -> Option<crate::types::Price> {
    match kind {
        crate::types::OrderKind::TrailingStopLimit { lmt_price: Some(price), trail_stop_price, .. }
        | crate::types::OrderKind::TrailLit { price, trail_stop_price, .. }
            if trail_stop_price > 0 =>
        {
            Some(match side? {
                Side::Buy => price - trail_stop_price,
                Side::Sell | Side::ShortSell => trail_stop_price - price,
            })
        }
        _ => None,
    }
}

/// The time-in-force byte as its wire string. DTC goes out as GTC, and
/// OVERNIGHT and OVERNIGHT + DAY as DAY, as the reference (ibx#467).
fn tif_str(tif: u8) -> String {
    if tif == crate::types::TIF_DTC { return "1".to_string(); }
    if matches!(tif, b'j' | b'b') { return "0".to_string(); }
    let b = [tif];
    std::str::from_utf8(&b).unwrap_or("0").to_string()
}

/// A DTC order: the DTC flag right after the time in force, as the
/// reference (ibx#467).
fn push_dtc_flag(fields: &mut Vec<(u32, String)>, tif: u8) {
    if tif != crate::types::TIF_DTC { return; }
    let at = fields.iter().position(|(t, _)| *t == 59).map_or(fields.len(), |i| i + 1);
    fields.insert(at, (6436, "1".to_string()));
}

/// The extended-attribute block (display size, outside-RTH, hidden, good-after,
/// good-till, OCA group, parent link, conditions, ...), shared by every order
/// path that carries attributes so the emission cannot drift between order
/// types (ibx#224, ibx#318). All-or-none rides the instruction field
/// (`exec_inst`).
fn push_extended_attrs(
    fields: &mut Vec<(u32, String)>,
    context: &Context,
    attrs: &crate::types::OrderAttrs,
) {
    if !attrs.order_ref.is_empty() {
        fields.push((6010, attrs.order_ref.clone()));
    }
    // A quantity the fixed point cannot hold, as the caller wrote it
    // (ibx#655).
    if !attrs.qty_text.is_empty()
        && let Some(field) = fields.iter_mut().find(|(t, _)| *t == 38)
    {
        field.1 = attrs.qty_text.clone();
    }
    // The MiFID values and the soft dollar tier decided for the request
    // (ibx#500, ibx#506).
    fields.extend(context.mifid_send.iter().cloned());
    fields.extend(context.soft_dollar_send.iter().cloned());
    // The FA allocation on an FA session (ibx#507).
    if context.fa_session {
        fa_fields(fields, usize::MAX, &attrs.fa, &context.fa);
    }
    // The smartComboRoutingParams of a combo, as order attributes (ibx#470).
    if let Some(combo) = &attrs.combo {
        fields.extend(combo.routing_attrs.iter().cloned());
    }
    if attrs.display_size > 0 {
        fields.push((111, format_uint(attrs.display_size as u64).to_string()));
    }
    if attrs.min_qty > 0 {
        fields.push((110, format_uint(attrs.min_qty as u64).to_string()));
    }
    if attrs.outside_rth {
        fields.push((6433, "1".to_string()));
    }
    if attrs.hidden {
        fields.push((6135, "1".to_string()));
    }
    // includeOvernight, an order attribute of the reference (captured
    // 28/09/2026, ibx#467).
    if attrs.include_overnight {
        fields.push((8534, "1".to_string()));
    }
    // Customer account and professional customer, as the reference's order
    // attributes; the account config check keeps them to accounts that
    // allow them (ibx#425, not observed yet).
    if !attrs.customer_account.is_empty() {
        fields.push((6207, attrs.customer_account.clone()));
    }
    if attrs.professional_customer {
        fields.push((6636, "1".to_string()));
    }
    // goodAfterTime in UTC, "YYYYMMDD-HH:MM:SS", as the reference writes
    // it (ibx#467).
    if attrs.good_after > 0 {
        fields.push((168, unix_to_ib_utc_dash(attrs.good_after)));
    }
    // GTD expiry: date-only -> the expire date; time-precise -> the expire time (UTC).
    // Mutually exclusive — never both (gateway rejects both together).
    if attrs.good_till_date_ymd > 0 {
        fields.push((432, format!("{:08}", attrs.good_till_date_ymd)));
    } else if attrs.good_till > 0 {
        fields.push((126, unix_to_ib_utc_dash(attrs.good_till)));
    }
    if attrs.parent_id > 0 {
        // A bracket child, as the reference sends it (ibx#311, ibx#329):
        // the OCA group is the parent's id (a caller's group name is
        // replaced), the caller's type or the default one, and the link is
        // the parent's current order id.
        let (link, group) = bracket_parent_link(context, attrs.parent_id);
        let oca_type = oca_type_str(attrs.oca_type);
        fields.push((583, group));
        fields.push((6209, oca_type.to_string()));
        fields.push((6107, link));
    } else {
        let oca_str = if !attrs.oca_group_str.is_empty() {
            attrs.oca_group_str.clone()
        } else if attrs.oca_group > 0 {
            format!("OCA_{}", attrs.oca_group)
        } else {
            String::new()
        };
        if !oca_str.is_empty() {
            fields.push((583, oca_str));
            fields.push((6209, oca_type_str(attrs.oca_type).to_string()));
        }
    }
    if attrs.discretionary_amt > 0 {
        fields.push((9813, format_price_ref(attrs.discretionary_amt).to_string()));
    }
    if attrs.sweep_to_fill {
        fields.push((6102, "1".to_string()));
    }
    // The order attributes of ibx#607, as the reference writes them where
    // they apply (paper 12/10/2026); the ones that do not apply were
    // dropped before.
    if attrs.allow_pre_open { fields.push((6524, "1".to_string())); }
    if attrs.ignore_open_auction { fields.push((6562, "1".to_string())); }
    if attrs.post_only { fields.push((6605, "1".to_string())); }
    if attrs.deactivate { fields.push((6521, "1".to_string())); }
    // The duration of a GTD order (the reference 10.51, paper, 17/10/2026,
    // market closed: 3600 written after the origin, before the price
    // management flag; ibx#688).
    if let Some(d) = attrs.duration { fields.push((8402, d.to_string())); }
    // Seek price improvement, and route marketable to BBO where the
    // precaution let it through (ibx#608, paper 12/10/2026).
    if attrs.seek_price_improvement { fields.push((6557, "1".to_string())); }
    if attrs.route_marketable_to_bbo { fields.push((8265, "1".to_string())); }
    // Not-held, after the account (the reference 10.51, paper, pre-market
    // 14/10/2026, an order for the broker's ATS venue; ibx#690).
    if attrs.not_held { fields.push((6287, "1".to_string())); }
    if attrs.cash_qty > 0 {
        // The cash quantity attribute of the reference, written
        // with its price formatter (ibx#263; 5920 is only its column id).
        fields.push((152, format_price_ref(attrs.cash_qty).to_string()));
    }
    // Condition fields. The reference always sends both
    // flags, 0 or 1, before the count: ignore-RTH and cancel-order each ride
    // their own field. ibx had the two fields the other way round and sent them only when set
    // (ib-agent#192 B2, ibx#327). The reference 10.51 writes a third flag
    // between them, whether the conditions include the overnight session:
    // 0 unless the order asks for it, which an order at session level 214
    // cannot (ibx#726, observed on paper, 17/10/2026).
    if !attrs.conditions.is_empty() {
        let flag = |on: bool| if on { "1" } else { "0" }.to_string();
        fields.push((6128, flag(attrs.conditions_ignore_rth)));
        fields.push((8612, flag(false)));
        fields.push((6151, flag(attrs.conditions_cancel_order)));
        fields.extend(condition_tags(&attrs.conditions));
    }
}

/// One shared encoder for every extended order submission (ibx#224): the
/// order-type-specific tags come from `kind`; the TIF and the full
/// `OrderAttrs` block are emitted identically for all kinds.
/// `SubmitLimitEx`, `SubmitTrailingStopPctEx` and `SubmitEx` all route
/// through here so the attrs emission cannot drift between order types.
#[allow(clippy::too_many_arguments)]
fn send_order_ex(
    conn: &mut Connection,
    context: &mut Context,
    account_id: &str,
    order_id: crate::types::OrderId,
    instrument: crate::types::InstrumentId,
    side: Side,
    qty: u32,
    kind: crate::types::OrderKind,
    tif: u8,
    attrs: &crate::types::OrderAttrs,
) -> std::io::Result<()> {
    use crate::types::OrderKind as K;

    // Engine-state entry: ord_type byte, tracked price, and tracked stop
    // price per kind — mirrors the corresponding plain variants exactly.
    let (ord_type_byte, track_price, track_stop) = match kind {
        K::Market => (b'1', 0, 0),
        K::Limit { price } => (b'2', price, 0),
        K::Stop { stop_price } => (b'3', stop_price, stop_price),
        K::StopLimit { price, stop_price } => (b'4', price, stop_price),
        K::TrailingStop { .. } => (b'P', 0, 0),
        K::TrailingStopLimit { lmt_offset, lmt_price, .. } => (b'P', lmt_price.unwrap_or(lmt_offset), 0),
        K::TrailPct { .. } => (b'P', 0, 0),
        K::Moc => (b'5', 0, 0),
        K::Loc { price } => (b'B', price, 0),
        K::Mit { stop_price } => (b'J', stop_price, stop_price),
        K::Lit { price, stop_price } => (b'K', price, stop_price),
        K::Mtl => (b'K', 0, 0),
        K::MktPrt => (b'U', 0, 0),
        K::StpPrt { stop_price } => (crate::types::ORD_STP_PRT, 0, stop_price),
        K::MidPrice { price_cap } => (crate::types::ORD_MIDPX, price_cap, 0),
        K::SnapMkt { offset } => (crate::types::ORD_SNAP_MKT, 0, offset),
        K::SnapMid { offset } => (crate::types::ORD_SNAP_MID, 0, offset),
        K::SnapPri { offset } => (crate::types::ORD_SNAP_PRI, 0, offset),
        K::PegMkt { price, offset } => (crate::types::ORD_PEG_MKT, price, offset),
        K::PegMid { price, offset } => (crate::types::ORD_PEG_MID, price, offset),
        K::Rel { price, offset } => (b'R', price, offset),
        K::AdjustableStop { stop_price, .. } => (b'3', 0, stop_price),
        K::PegBench { starting_price, .. } => (crate::types::ORD_PEG_BENCH, starting_price, 0),
        K::TrailMit { .. } => (crate::types::ORD_TRAIL_MIT, 0, 0),
        K::TrailLit { price, .. } => (crate::types::ORD_TRAIL_LIT, price, 0),
        K::PegBest { price } => (crate::types::ORD_PEG_BEST, price, 0),
        K::Rpi { price, offset } => (crate::types::ORD_RPI, price, offset),
        K::PassvRel { price, offset } => (crate::types::ORD_PASSV_REL, price, offset),
        K::Volatility { .. } => (b'V', 0, 0),
        K::PegStk { .. } | K::Quote => (b'V', 0, 0),
    };
    context.insert_order(crate::types::Order::new(
        order_id, instrument, side, qty, track_price, ord_type_byte, tif, track_stop,
    ));

    let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
    // A combo goes out under its legs' symbols (ibx#470).
    let symbol = match &context.combo_send {
        Some(combo) => combo.symbol.clone(),
        None => context.market.symbol(instrument).to_string(),
    };
    let (sec_type_str, destination) = context.market.order_routing(instrument);
    let now = chrono_free_timestamp().to_string();
    let tif_str = tif_str(tif);

    let mut fields: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
        (fix::TAG_SENDING_TIME, now.clone()),
        (11, format!("{}.{}", context.server_id(order_id), ver)),
        (1, account_id.to_string()),
        (55, symbol),
        (54, fix_side(side).to_string()),
        // A hedge's quantity with two decimals, as the reference writes it
        // (ibx#614: "1.00"; a bracket child's is "1").
        (38, if attrs.hedge.is_some() { format!("{}.00", qty) } else { format_uint(qty as u64).to_string() }),
    ];
    // The hedge of a hedge order (ibx#614; the reference 10.51, paper,
    // 16/10/2026): the auto-cancel-parent flag, the kind's number, the
    // beta or the ratio with two decimals at least, the max size.
    if let Some(hedge) = &attrs.hedge {
        fields.push((6965, "1".to_string()));
        fields.push((6665, hedge.kind.to_string()));
        if let Some(beta) = hedge.beta { fields.push((6703, hedge_number(beta))); }
        if let Some(ratio) = hedge.ratio { fields.push((6666, hedge_number(ratio))); }
        if let Some(size) = hedge.max_size { fields.push((6690, size.to_string())); }
    }

    match kind {
        K::Market => fields.push((40, "1".to_string())),
        K::Limit { price } => {
            fields.push((40, "2".to_string()));
            // A combo with per-leg prices: the price they give, then each
            // leg's price in leg order (ibx#470, captured 26/09/2026:
            // the price -73.15, then the leg prices 721.35 and 794.50).
            let price = context.combo_send.as_ref().and_then(|c| c.price).unwrap_or(price);
            fields.push((44, format_price_ref(price).to_string()));
            if let Some(combo) = &context.combo_send {
                fields.extend(combo.leg_prices.iter().map(|p| (6879, format_price_ref(*p).to_string())));
            }
        }
        K::Stop { stop_price } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price_ref(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price_ref(stop_price).to_string()));
        }
        K::StopLimit { price, stop_price } => {
            fields.push((40, "4".to_string()));
            fields.push((44, format_price_ref(price).to_string()));
            fields.push((99, format_price_ref(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price_ref(stop_price).to_string()));
        }
        K::TrailingStop { trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: amount-based trailing stop carries
            // the trail amount in both the stop price and the peg offset and
            // requires the trailing-stop execution instruction.
            let t = format_price_ref(trail_amt).to_string();
            fields.push((40, "P".to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            // Optional initial stop trigger, only when set (ib-agent#173).
            if trail_stop_price > 0 { fields.push((6117, format_price_ref(trail_stop_price).to_string())); }
        }
        K::TrailingStopLimit { lmt_offset, lmt_price, trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: TRAIL LIMIT uses the trailing stop limit
            // order type, no execution instruction; trail amount in both the stop
            // price and the peg offset; the limit offset is the
            // limit-vs-trail offset. An absolute limit price goes in the limit price
            // instead, with no limit offset (ib-agent#194).
            let t = format_price_ref(trail_amt).to_string();
            fields.push((40, "TSL".to_string()));
            if let Some(price) = lmt_price { fields.push((44, format_price_ref(price).to_string())); }
            fields.push((99, t.clone()));
            if lmt_price.is_none() { fields.push((6370, format_price_ref(lmt_offset).to_string())); }
            fields.push((211, t));
            if trail_stop_price > 0 { fields.push((6117, format_price_ref(trail_stop_price).to_string())); }
        }
        K::TrailPct { trail_percent, trail_stop_price } => {
            // Per ib-agent#156 capture: percent-trail mirrors the stop price and the
            // peg offset as the percent in decimal form (1.00 for 1%), with the
            // trailing-stop execution instruction. The
            // trail unit, 100 = percent (ib-agent#192 B7, ibx#339).
            let pct_decimal = format_price_ref(trail_percent).to_string();
            fields.push((40, "P".to_string()));
            fields.push((99, pct_decimal.clone()));
            fields.push((211, pct_decimal));
            fields.push((6268, "100".to_string()));
            if trail_stop_price > 0 { fields.push((6117, format_price_ref(trail_stop_price).to_string())); }
        }
        K::Moc => fields.push((40, "5".to_string())),
        K::Loc { price } => {
            fields.push((40, "B".to_string()));
            fields.push((44, format_price_ref(price).to_string()));
        }
        // A trigger price of 0 is not written, nor its touched trigger, as
        // the reference 10.51 sends it (ibx#688; paper, 18/10/2026, market
        // closed: the server refuses the order).
        K::Mit { stop_price } => {
            fields.push((40, "J".to_string()));
            if stop_price != 0 { fields.push((99, format_price_ref(stop_price).to_string())); }
        }
        K::Lit { price, stop_price } => {
            fields.push((40, "LT".to_string())); // per ib-agent#138
            fields.push((44, format_price_ref(price).to_string()));
            if stop_price != 0 { fields.push((99, format_price_ref(stop_price).to_string())); }
        }
        K::Mtl => fields.push((40, "K".to_string())),
        K::MktPrt => fields.push((40, "U".to_string())),
        K::StpPrt { stop_price } => {
            fields.push((40, "SP".to_string()));
            fields.push((99, format_price_ref(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price_ref(stop_price).to_string()));
        }
        K::MidPrice { price_cap } => {
            fields.push((40, "MIDPX".to_string()));
            if price_cap > 0 {
                fields.push((44, format_price_ref(price_cap).to_string()));
            }
        }
        K::SnapMkt { offset } | K::SnapMid { offset } | K::SnapPri { offset } => {
            // The offset in both price fields, 0.00 when unset (ibx#413).
            let code = match kind { K::SnapMkt { .. } => "SMKT", K::SnapMid { .. } => "SMID", _ => "SREL" };
            let o = format_price_ref(offset).to_string();
            fields.push((40, code.to_string()));
            fields.push((99, o.clone()));
            fields.push((211, o));
        }
        K::PegMkt { price, offset } | K::PegMid { price, offset } => {
            // As the reference (ibx#414): the pegged order type with the
            // peg instruction, no mid-offset fields.
            let (prices, types) = pegged_tags(matches!(kind, K::PegMid { .. }), price, offset);
            fields.extend(types.into_iter().filter(|(t, _)| *t != 18));
            fields.extend(prices);
        }
        K::Rel { price, offset } => {
            // Per ib-agent#138 capture: Relative shares the trailing stop's order type and is
            // disambiguated by the relative execution instruction; the peg offset in
            // its field; the price cap in the limit price
            // when set (ib-agent#199, ibx#263).
            fields.push((40, "P".to_string()));
            if price > 0 { fields.push((44, format_price_ref(price).to_string())); }
            fields.push((211, format_price_ref(offset).to_string()));
        }
        // The adjustable tags themselves follow the common block below.
        K::AdjustableStop { stop_price, .. } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price_ref(stop_price).to_string()));
        }
        // As the reference (ibx#415): the starting price in the stop price
        // field, no limit price, the peg instruction; the benchmark
        // attributes follow the common block below.
        K::PegBench { starting_price, .. } => {
            fields.push((40, "PB".to_string()));
            if starting_price > 0 { fields.push((99, format_price_ref(starting_price).to_string())); }
        }
        // As the reference (ibx#469, captured 26/09/2026 and 28/09/2026):
        // the trailing amount or percent in both fields, the trigger when
        // set, no instruction; the unit is added when the order is sent
        // unless it is a percent.
        K::TrailMit { trail, percent, trail_stop_price } => {
            let t = format_price_ref(trail).to_string();
            fields.push((40, "TMIT".to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            if percent { fields.push((6268, "100".to_string())); }
            if trail_stop_price > 0 { fields.push((6117, format_price_ref(trail_stop_price).to_string())); }
        }
        K::TrailLit { price, trail, percent, trail_stop_price } => {
            let t = format_price_ref(trail).to_string();
            fields.push((40, "TLIT".to_string()));
            fields.push((44, format_price_ref(price).to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            if percent { fields.push((6268, "100".to_string())); }
            if trail_stop_price > 0 { fields.push((6117, format_price_ref(trail_stop_price).to_string())); }
        }
        // Pegged to best: the limit price; its compete attributes follow
        // the common block below.
        K::PegBest { price } => {
            fields.push((40, "E2M".to_string()));
            if price > 0 { fields.push((44, format_price_ref(price).to_string())); }
        }
        // Retail price improvement (captured 07/10/2026): the limit price,
        // the offset in both fields, 0.00 when unset.
        K::Rpi { price, offset } => {
            let o = format_price_ref(offset).to_string();
            fields.push((40, "RPI".to_string()));
            fields.push((44, format_price_ref(price).to_string()));
            fields.push((99, o.clone()));
            fields.push((211, o));
        }
        // Passive relative (not observed yet): its own order type, the limit
        // price when set, the offset in both fields like the retail price
        // improvement type, no instruction.
        // Refused before anything is sent (ibx#673, ibx#745): never written.
        K::Volatility { .. } => fields.push((40, "VOL".to_string())),
        K::PegStk { .. } => fields.push((40, "PEG STK".to_string())),
        K::Quote => fields.push((40, "QUOTE".to_string())),
        K::PassvRel { price, offset } => {
            let o = format_price_ref(offset).to_string();
            fields.push((40, "PSVR".to_string()));
            if price > 0 { fields.push((44, format_price_ref(price).to_string())); }
            fields.push((99, o.clone()));
            fields.push((211, o));
        }
    }

    fields.push((59, tif_str));
    push_dtc_flag(&mut fields, tif);
    fields.push((167, sec_type_str.clone()));
    // Every type keeps the contract's routing, as the reference (ibx#413,
    // ibx#414): a forced directed exchange is refused for some of them.
    fields.push((100, destination.clone()));
    // Secondary routing field — the reference encoder always writes it
    // alongside the destination (ib-agent#165).
    fields.push((6210, destination));
    fields.push((15, context.market.currency(instrument).to_string()));

    // Adjustable-stop tags in the same place as on the plain path (ibx#240).
    if let K::AdjustableStop { trigger_price, adjusted_order_type, adjusted_stop_price,
        adjusted_stop_limit_price, adjusted_trailing_amount, adjustable_trailing_unit, .. } = kind
    {
        fields.extend(adjustable_stop_tags(trigger_price, adjusted_order_type,
            adjusted_stop_price, adjusted_stop_limit_price,
            adjusted_trailing_amount, adjustable_trailing_unit));
    }

    if let K::PegBench { stock_ref_price, ref_con_id, is_peg_decrease,
        pegged_change_amount, ref_change_amount, .. } = kind
    {
        fields.extend(peg_bench_attrs(stock_ref_price, ref_con_id, is_peg_decrease,
            pegged_change_amount, ref_change_amount, &attrs.reference_exchange, true));
    }
    if matches!(kind, K::PegBest { .. }) { fields.extend(peg_best_attrs()); }

    push_bracket_key(&mut fields, context, order_id, attrs);
    // Extended attributes — same tag order as the historical SubmitLimitEx
    // block.
    push_extended_attrs(&mut fields, context, attrs);
    let exec_inst = exec_inst(&kind, attrs.all_or_none, attrs.algo.is_some());
    if !exec_inst.is_empty() {
        fields.push((18, exec_inst));
    }
    if let Some(algo) = &attrs.algo {
        fields.extend(algo_fields(algo));
    }

    let mut refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
    // The caller's trigger method, only on the types that write it
    // (ibx#263); the default is added when the order is sent.
    let trigger_method = attrs.trigger_method.to_string();
    if attrs.trigger_method > 0 && has_trigger_method(&refs) { refs.push((6115, &trigger_method)); }
    send_new_order(conn, context, instrument, &refs)
}

/// The execution instruction field as the reference builds it: the letter of a
/// pegged or trailing type (a trailing stop, R relative, P pegged to
/// market, M pegged to midpoint), G for all-or-none, e for an algo order
/// whatever its type (ib-agent#192 B9, ibx#405, ibx#263), R for pegged to
/// benchmark, in this order, separated by a space. Empty: not written.
fn exec_inst(kind: &crate::types::OrderKind, all_or_none: bool, algo: bool) -> String {
    use crate::types::OrderKind as K;
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    match kind {
        K::TrailingStop { .. } | K::TrailPct { .. } => parts.push("a"),
        K::Rel { .. } => parts.push("R"),
        K::PegMkt { .. } => parts.push("P"),
        K::PegMid { .. } => parts.push("M"),
        _ => {}
    }
    if all_or_none { parts.push("G"); }
    if algo { parts.push("e"); }
    if matches!(kind, K::PegBench { .. }) { parts.push("R"); }
    parts.join(" ")
}

/// The algo block, as the reference writes it after the order attributes:
/// the strategy (after the maximum percentage of volume when
/// the algo has one), then the parameter count
/// and its pairs.
fn algo_fields(algo: &crate::types::OrderAlgo) -> Fields {
    use crate::types::OrderAlgo;
    let mut fields: Fields = Vec::with_capacity(12);
    match algo {
        OrderAlgo::Adaptive(priority) => {
            fields.push((847, "Adaptive".to_string()));
            fields.push((5957, "1".to_string()));
            fields.push((5958, "adaptivePriority".to_string()));
            fields.push((5960, priority.as_str().to_string()));
        }
        // Another strategy: its pairs as given, already in the
        // reference's order (ibx#622).
        OrderAlgo::Params(AlgoParams::Other { strategy, params }) => {
            fields.push((847, strategy.clone()));
            fields.push((5957, params.len().to_string()));
            for (key, value) in params {
                fields.push((5958, key.clone()));
                fields.push((5960, value.clone()));
            }
        }
        OrderAlgo::Params(params) => {
            let (algo_name, param_strs) = build_algo_tags(params);
            // The maxPctVol parameter for algos that use it, only when it was
            // given: the reference writes the parameters the order has, and
            // the server refused the zero value ibx sent for a Vwap without it
            // (an invalid-value error on that field, paper 05/10/2026). A value of
            // 0 is refused before (441, its minimum is 0.01), so 0 here is a
            // parameter not given.
            match params {
                AlgoParams::Vwap { max_pct_vol, .. }
                | AlgoParams::ArrivalPx { max_pct_vol, .. }
                | AlgoParams::ClosePx { max_pct_vol, .. } if *max_pct_vol != 0.0 => {
                    fields.push((849, format!("{}", max_pct_vol)))
                }
                _ => {}
            }
            fields.push((847, algo_name.to_string()));
            // Only parameters that have a value: the reference leaves out
            // one that was not given (an unset start/end time), and the
            // server refuses an empty one with an invalid-value error on the
            // parameter count (ib-agent#192 B9, ibx#405).
            let pairs: Vec<&[String]> = param_strs.chunks(2).filter(|p| !p[1].is_empty()).collect();
            fields.push((5957, pairs.len().to_string()));
            // Emit the key/value pairs, the key then the value (repeated)
            for pair in pairs {
                fields.push((5958, pair[0].clone()));
                fields.push((5960, pair[1].clone()));
            }
        }
    }
    fields
}

fn build_algo_tags(algo: &AlgoParams) -> (&'static str, Vec<String>) {
    match algo {
        AlgoParams::Vwap { no_take_liq, allow_past_end_time, start_time, end_time, .. } => {
            ("Vwap", vec![
                "noTakeLiq".into(), if *no_take_liq { "1" } else { "0" }.into(),
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::Twap { allow_past_end_time, start_time, end_time } => {
            ("Twap", vec![
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::ArrivalPx { risk_aversion, allow_past_end_time, force_completion, start_time, end_time, .. } => {
            ("ArrivalPx", vec![
                "riskAversion".into(), risk_aversion.as_str().into(),
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "forceCompletion".into(), if *force_completion { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::ClosePx { risk_aversion, force_completion, start_time, .. } => {
            ("ClosePx", vec![
                "riskAversion".into(), risk_aversion.as_str().into(),
                "forceCompletion".into(), if *force_completion { "1" } else { "0" }.into(),
                "startTime".into(), start_time.clone(),
            ])
        }
        AlgoParams::DarkIce { allow_past_end_time, display_size, start_time, end_time } => {
            ("DarkIce", vec![
                "allowPastEndTime".into(), if *allow_past_end_time { "1" } else { "0" }.into(),
                "displaySize".into(), display_size.to_string(),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::PctVol { pct_vol, no_take_liq, start_time, end_time } => {
            ("PctVol", vec![
                "noTakeLiq".into(), if *no_take_liq { "1" } else { "0" }.into(),
                "pctVol".into(), format!("{}", pct_vol),
                "startTime".into(), start_time.clone(),
                "endTime".into(), end_time.clone(),
            ])
        }
        AlgoParams::Other { .. } => unreachable!("written by algo_fields"),
    }
}

/// Exchange of a price-type condition: the reference sends SMART as BEST, as it
/// does for the order's own destination (ib-agent#165, ib-agent#192 B2).
fn condition_exchange(exchange: &str) -> String {
    if exchange.eq_ignore_ascii_case("SMART") { "BEST".to_string() } else { exchange.to_string() }
}

/// Tags every condition is padded with, in this order: each one the
/// condition did not write goes out empty (ib-agent ORDER-SUBMIT.md 4.2).
/// The available-funds tag of the list is left out: ibx has no such
/// condition.
const CONDITION_PADDING: [u32; 10] = [6123, 6124, 6127, 6126, 6125, 6223, 6245, 6263, 6246, 6947];

/// The operator of a condition that has one: at least or at most.
fn condition_operator(cond: &OrderCondition) -> Option<String> {
    let is_more = match cond {
        OrderCondition::Price { is_more, .. }
        | OrderCondition::Time { is_more, .. }
        | OrderCondition::Margin { is_more, .. }
        | OrderCondition::Volume { is_more, .. }
        | OrderCondition::PercentChange { is_more, .. } => *is_more,
        OrderCondition::Execution { .. } => return None,
    };
    Some(if is_more { ">=" } else { "<=" }.to_string())
}

/// The field that carries a condition's value, and the value as the
/// reference writes it: numbers with two decimals at least, an execution
/// as its symbol, exchange (SMART as any) and security type.
fn condition_value(cond: &OrderCondition) -> (u32, String) {
    match cond {
        OrderCondition::Price { price, .. } => (6125, format_price_ref(*price).to_string()),
        OrderCondition::Time { time, .. } => (6223, time.clone()),
        OrderCondition::Margin { percent, .. } => (6245, percent.to_string()),
        OrderCondition::Execution { symbol, exchange, sec_type } => {
            let exch = if exchange == "SMART" { "*" } else { exchange.as_str() };
            (6246, format!("symbol={};exchange={};securityType={};", symbol, exch, sec_type))
        }
        OrderCondition::Volume { volume, .. } => (6263, volume.to_string()),
        OrderCondition::PercentChange { percent, .. } => {
            (6245, format_price_ref((percent * crate::types::PRICE_SCALE as f64).round() as crate::types::Price).to_string())
        }
    }
}

/// The condition block after the two flags, as the reference writes it
/// (ib-agent ORDER-SUBMIT.md 4.2, 4.3; ibx#416): the count, then per
/// condition its type, conjunction and own tags in the reference's order,
/// then the padding. A time condition carries its time right after the
/// operator; numbers have two decimals at least.
fn condition_tags(conditions: &[OrderCondition]) -> Vec<(u32, String)> {
    let mut out = vec![(6136, conditions.len().to_string())];
    for (i, cond) in conditions.iter().enumerate() {
        let conj = if i == conditions.len() - 1 { "n" } else { "a" };
        let mut own: Vec<(u32, String)> = condition_operator(cond).map(|op| (6126, op)).into_iter().collect();
        match cond {
            OrderCondition::Price { con_id, exchange, trigger_method, .. } => {
                own.push((6123, con_id.to_string()));
                own.push((6124, condition_exchange(exchange)));
                own.push((6127, trigger_method.to_string()));
            }
            OrderCondition::Volume { con_id, exchange, .. } | OrderCondition::PercentChange { con_id, exchange, .. } => {
                own.push((6123, con_id.to_string()));
                own.push((6124, condition_exchange(exchange)));
            }
            _ => {}
        }
        own.push(condition_value(cond));
        let kind = match cond {
            OrderCondition::Price { .. } => "1",
            OrderCondition::Time { .. } => "3",
            OrderCondition::Margin { .. } => "4",
            OrderCondition::Execution { .. } => "5",
            OrderCondition::Volume { .. } => "6",
            OrderCondition::PercentChange { .. } => "7",
        };
        out.push((6222, kind.to_string()));
        out.push((6137, conj.to_string()));
        let written: Vec<u32> = own.iter().map(|(t, _)| *t).collect();
        out.extend(own);
        out.extend(CONDITION_PADDING.iter().filter(|t| !written.contains(t)).map(|&t| (t, String::new())));
    }
    out
}

/// The fields of the conditions in a replace, after the overnight flag;
/// the combo block of a combo's replace comes after them.
const REPLACE_CONDITION_TAGS: [u32; 14] = [8612, 6136, 6222, 6137, 6126, 6123, 6124, 6127, 6125, 6223, 6245, 6263, 6246, 6947];

/// The conditions of an order in its replace (ibx#726): the count, then
/// per condition a marker 2 in the type field, its value, its operator
/// and, for a price condition, its trigger method, then the padding in the
/// list order of the new order, the contract id and exchange left empty.
/// No conjunction. Observed on paper, 17/10/2026, for one price condition
/// only (the reference 10.51, an order with one price condition, its
/// limit price modified); the other kinds are written in the same form,
/// not observed yet.
fn replace_condition_tags(conditions: &[OrderCondition]) -> Vec<(u32, String)> {
    let mut out = vec![(6136, conditions.len().to_string())];
    for cond in conditions {
        let mut own = vec![condition_value(cond)];
        if let Some(op) = condition_operator(cond) { own.push((6126, op)); }
        if let OrderCondition::Price { trigger_method, .. } = cond {
            own.push((6127, trigger_method.to_string()));
        }
        out.push((6222, "2".to_string()));
        let written: Vec<u32> = own.iter().map(|(t, _)| *t).collect();
        out.extend(own);
        out.extend(CONDITION_PADDING.iter().filter(|t| !written.contains(t)).map(|&t| (t, String::new())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session that has the stock order preset, with no time in force:
    /// the lookups of the session's first stock order have their own tests
    /// (ibx#728). The logon's list of stored presets is the paper account's
    /// of 13/10/2026 before the first orders of other types (ibx#729).
    fn order_context() -> Context {
        let mut context = Context::new();
        context.order_presets.insert("STK".to_string(), None);
        context.presets_stored = Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect());
        context
    }

    /// The order presets the paper account had stored, from the logon's
    /// list (the reference 10.51, paper, 13/10/2026, `.tmp/666` probe).
    const PAPER_PRESETS: [&str; 4] = ["CASH", "CASH&tc=EUR", "COMB", "STK"];

    /// The first server id the order id generator gives in these tests.
    const FIRST: OrderId = 1_000_000_001;
    use crate::types::Order;

    fn order(oid: OrderId, filled: u32, status: OrderStatus) -> Order {
        Order {
            order_id: oid, instrument: 0, side: Side::Buy, price: 100,
            qty_fixed: (10) as i64 * crate::types::QTY_SCALE, filled_fixed: filled as i64 * crate::types::QTY_SCALE, status, ord_type: b'2', tif: b'0', stop_price: 0,
        }
    }

    // ibx#211: an outbound cancel sets the PendingCancel phase the server
    // never sends for a normal cancel; ibx#486: with no callback until the
    // server's next report, as the reference.
    #[test]
    fn synthesize_pending_cancel_sets_the_status_without_a_callback() {
        let mut context = order_context();
        let shared = Arc::new(SharedState::new());
        context.insert_order(order(7, 3, OrderStatus::PartiallyFilled));

        synthesize_pending_cancel(&mut context, &shared, 7);

        assert_eq!(context.order(7).unwrap().status, OrderStatus::PendingCancel);
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    /// Encode one order request through `drain_and_send_orders` to a
    /// scripted peer and return the sent tags in wire order.
    fn wire_tags(req: OrderRequest) -> Vec<(u32, String)> {
        wire_tags_with(|_| {}, req)
    }

    /// Same as `wire_tags`, with a hook to set up the engine state first.
    fn wire_tags_with(setup: impl FnOnce(&mut Context), req: OrderRequest) -> Vec<(u32, String)> {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        // A definition that keeps outside RTH for every type, so these
        // tests see the encoding only; the rule has its own tests (ibx#465).
        for exch in ["BEST", "ISLAND"] {
            context.rth_types.insert((265598, exch.to_string()), crate::engine::outside_rth::RthTypes {
                rth: true, sec_type: "STK".into(), ..Default::default()
            });
        }
        setup(&mut context);
        context.pending_orders.push(req);
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        // Every message sent, its fields one after the other.
        peer.messages().iter().flat_map(|m| crate::test_support::parse_fields(m)).collect()
    }

    // ibx#668: an order with a zone to check waits for the trading hours
    // asked for its contract, then is refused when the zone is not the
    // contract's; nothing sent.
    #[test]
    fn a_zone_check_waits_for_the_trading_hours() {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        let instrument = context.market.register(265598);
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "STK".into(), ..Default::default()
        });
        let start = "Start Time: The date, time, or time-zone entered is invalid.".to_string();
        let order = OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE, tif: b'0',
            attrs: crate::types::OrderAttrs { zone_checks: vec![(vec!["Asia/Tokyo".into()], 337, start.clone())], ..Default::default() },
        };
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        let mut hb = HeartbeatState::new();
        context.zone_pending.insert(265598);
        context.pending_orders.push(order);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        assert_eq!(context.rth_parked.len(), 1, "waits for the trading hours");
        shared.reference.cache_time_zone_id(265598, "US/Eastern");
        zones_known(&mut context, |_| false);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        assert!(peer.messages().is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(FIRST, 337, start)]);
    }

    // ibx#660 (the reference 10.51, paper, 13/10/2026): includeOvernight on
    // a contract whose valid exchanges have no overnight one is refused
    // with 10362, nothing sent; with one, the order goes out.
    #[test]
    fn include_overnight_needs_an_overnight_exchange() {
        let order = OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument: 0, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE,
            tif: b'0', attrs: crate::types::OrderAttrs { include_overnight: true, ..Default::default() },
        };
        let with = |valid: &'static [&'static str]| move |c: &mut Context| {
            for exch in ["BEST", "ISLAND"] {
                c.rth_types.get_mut(&(265598, exch.to_string())).unwrap().valid_exchanges =
                    Some(valid.iter().map(|e| e.to_string()).collect());
            }
        };
        let tags = wire_tags_with(with(&["SMART", "CBOE", "AMEX"]), order.clone());
        assert!(tags.is_empty(), "nothing sent: {tags:?}");
        let tags = wire_tags_with(with(&["SMART", "NASDAQ", "OVERNIGHT"]), order);
        assert!(tags.contains(&(35, "D".to_string())), "{tags:?}");
    }

    // ibx#751 (the reference 10.51, paper, 18/10/2026, market closed): a
    // scale order goes out with its scale fields after the account and the
    // scale source; with a profit offset, its profit taker follows in the
    // same pass: the opposite side at the price moved by the offset, its
    // negative API order id, the bracket key of the pair and the parent link.
    #[test]
    fn a_scale_order_and_its_profit_taker_as_the_reference() {
        let scale = crate::types::Scale {
            init_level_size: 1, subs_level_size: Some(1), price_increment: Some(px(0.1)),
            profit_offset: Some(px(0.5)), auto_reset: true, total_qty: 2 * crate::types::QTY_SCALE,
            profit_child_id: Some(-2), ..Default::default()
        };
        let tags = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument: 0, side: Side::Buy, qty: 2, price: px(60.0), tif: b'0',
            attrs: crate::types::OrderAttrs { scale: Some(Box::new(scale)), ..Default::default() },
        });
        let frames: Vec<Vec<(u32, String)>> = tags.split(|(t, v)| *t == 35 && v == "D").skip(1).map(|f| f.to_vec()).collect();
        assert_eq!(frames.len(), 2, "the order and its profit taker: {tags:?}");
        let order: Vec<u32> = |f: &[(u32, String)]| -> Vec<u32> {
            f.iter().map(|(t, _)| *t).skip_while(|t| *t != 44).filter(|t| !matches!(*t, 8339 | 10 | 8 | 9 | 34 | 52)).collect()
        }(&frames[0]);
        assert_eq!(order, [44, 1, 6531, 6405, 6403, 6461, 6534, 6122, 6445, 6121, 6119, 38, 40, 55, 167, 231, 54, 59,
            100, 6210, 6008, 6088, 15, 6211, 6238]);
        let child: Vec<u32> = frames[1].iter().map(|(t, _)| *t).skip_while(|t| *t != 44).filter(|t| !matches!(*t, 8339 | 10 | 8 | 9 | 34 | 52)).collect();
        assert_eq!(child, [44, 1, 6531, 6446, 6965, 6461, 6122, 6121, 6119, 38, 40, 55, 167, 231, 54, 59,
            100, 6210, 6008, 6088, 6107, 15, 6211, 6238]);
        let value = |f: &[(u32, String)], t: u32| f.iter().find(|(x, _)| *x == t).map(|(_, v)| v.clone()).unwrap_or_default();
        assert_eq!((value(&frames[0], 6088), value(&frames[0], 6405), value(&frames[0], 6534), value(&frames[0], 6403)),
            ("Scale".into(), "0.10".into(), "2".into(), "1".into()));
        assert_eq!((value(&frames[1], 44), value(&frames[1], 54), value(&frames[1], 6121), value(&frames[1], 6446)),
            ("60.50".into(), "2".into(), "-2".into(), "0.50".into()));
        assert_eq!(value(&frames[1], 6107), value(&frames[0], 11), "the parent link");
        let (parent_key, child_key) = (value(&frames[0], 6531), value(&frames[1], 6531));
        assert_eq!(parent_key.split('/').nth(1), Some("0"));
        assert_eq!(child_key.split('/').nth(1), Some("1"));
        assert_eq!(parent_key.split('/').nth(2), child_key.split('/').nth(2));

        // With an initial position, the quantity sent is the whole quantity
        // less that position (an order of 4 with a position of 5: -1).
        let scale = crate::types::Scale {
            init_level_size: 2, subs_level_size: Some(1), price_increment: Some(px(0.2)), init_position: Some(5),
            init_fill_qty: Some(1), total_qty: 4 * crate::types::QTY_SCALE, ..Default::default()
        };
        let tags = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument: 0, side: Side::Buy, qty: 4, price: px(50.0), tif: b'0',
            attrs: crate::types::OrderAttrs { scale: Some(Box::new(scale)), ..Default::default() },
        });
        assert_eq!(tags.iter().filter(|(t, v)| *t == 35 && v == "D").count(), 1);
        let value = |t: u32| tags.iter().find(|(x, _)| *x == t).map(|(_, v)| v.clone()).unwrap_or_default();
        assert_eq!((value(38), value(6485), value(6486), value(6534)), ("-1".into(), "5".into(), "1".into(), "4".into()));
    }

    // ibx#660 (the reference 10.51, paper, 13/10/2026): a Minutes order
    // goes out with time in force p.
    #[test]
    fn minutes_time_in_force_is_p_on_the_wire() {
        let tags = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument: 0, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE,
            tif: b'p', attrs: crate::types::OrderAttrs::default(),
        });
        assert!(tags.contains(&(59, "p".to_string())), "{tags:?}");
    }

    // ibx#665: what the client read for after the contract's lookup is
    // given once the contract is known: a refused order sends nothing, an
    // order with warnings only goes out after them.
    #[test]
    fn checks_after_the_lookup_are_given_once_the_contract_is_known() {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        let instrument = context.market.register(265598);
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "STK".into(), ..Default::default()
        });
        let order = |id: OrderId, after: Vec<(i64, String)>, refused: bool| OrderRequest::SubmitLimitEx {
            order_id: id, instrument, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE, tif: b'0',
            attrs: crate::types::OrderAttrs { after_lookup: after, after_lookup_refused: refused, ..Default::default() },
        };
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        let mut hb = HeartbeatState::new();
        let refusal = (443, "Order processing failed. Unknown algo attribute:giveUp".to_string());
        context.pending_orders.push(order(FIRST, vec![refusal.clone()], true));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        assert!(peer.messages().is_empty(), "nothing sent for the refused order");
        assert_eq!(shared.orders.drain_order_errors(), [(FIRST, refusal.0, refusal.1)]);
        assert!(context.order(FIRST).is_none());

        let warning = (2174, "a warning".to_string());
        context.pending_orders.push(order(FIRST + 1, vec![warning.clone()], false));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        assert_eq!(shared.orders.drain_order_errors(), [(FIRST + 1, warning.0, warning.1)]);
        let sent: Vec<Vec<(u32, String)>> = peer.messages().iter().map(|m| crate::test_support::parse_fields(m)).collect();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].iter().any(|(t, v)| *t == 35 && v == "D"));
    }

    // ibx#660 (observed 13/10/2026, paper, the reference at level 214): an
    // order placed with no time in force asks the account's preset of its
    // security type, then goes out with the preset's GTC and notice 10349;
    // the next order of the type takes the kept preset without asking.
    #[test]
    fn empty_tif_takes_the_order_preset() {
        let mut context = Context::new();
        context.order_ids.start_at(FIRST as i32);
        let instrument = context.market.register(265598);
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "STK".into(), ..Default::default()
        });
        let order = |id: OrderId| OrderRequest::SubmitLimitEx {
            order_id: id, instrument, side: Side::Buy, qty: 1, price: 240 * crate::types::PRICE_SCALE,
            tif: b'0', attrs: crate::types::OrderAttrs { tif_preset: true, ..Default::default() },
        };
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        let mut hb = HeartbeatState::new();
        context.pending_orders.push(order(FIRST));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        let asked: Vec<Vec<(u32, String)>> = peer.messages().iter().map(|m| crate::test_support::parse_fields(m)).collect();
        assert_eq!(asked.len(), 1, "the preset request only: {asked:?}");
        let field = |f: &[(u32, String)], t: u32| f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.clone());
        assert_eq!((field(&asked[0], 35), field(&asked[0], 6040), field(&asked[0], 6556), field(&asked[0], 8166),
            field(&asked[0], 8176), field(&asked[0], 8168)),
            (Some("U".into()), Some("193".into()), Some("OPR.3".into()), Some("G".into()), Some("1".into()), Some("s=STK".into())));
        assert!(shared.orders.drain_order_errors().is_empty());

        let reply: std::collections::HashMap<u32, String> =
            [(8166, "G"), (8168, "s=STK"), (8169, "v=1"), (4026, "GTC")].into_iter().map(|(t, v)| (t, v.to_string())).collect();
        preset_reply(&mut context, &fix::field_view(&reply));
        context.pending_orders.push(order(FIRST + 1));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        let sent: Vec<Vec<(u32, String)>> = peer.messages().iter().map(|m| crate::test_support::parse_fields(m)).collect();
        assert_eq!(sent.len(), 2, "both orders, no second preset request: {sent:?}");
        for f in &sent {
            assert_eq!((field(f, 35), field(f, 59)), (Some("D".into()), Some("1".into())));
        }
        let notice = "Order TIF was set to GTC based on order preset.".to_string();
        assert_eq!(shared.orders.drain_order_errors(), [(FIRST, 10349, notice.clone()), (FIRST + 1, 10349, notice)]);
    }

    /// Encode one order request and return each sent frame's tags, for
    /// requests that send several orders.
    fn wire_frames(req: OrderRequest, frames: usize) -> Vec<Vec<(u32, String)>> {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        context.pending_orders.push(req);
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);

        let mut out: Vec<Vec<(u32, String)>> = Vec::new();
        let mut buf = vec![0u8; 16384];
        let mut len = 0;
        while out.len() < frames {
            let n = server.read(&mut buf[len..]).unwrap();
            assert!(n > 0, "connection closed");
            len += n;
            out = buf[..len].split(|&b| b == fix::SOH)
                .filter_map(|f| {
                    let s = std::str::from_utf8(f).ok()?;
                    let (t, v) = s.split_once('=')?;
                    Some((t.parse::<u32>().ok()?, v.to_string()))
                })
                .fold(Vec::new(), |mut acc: Vec<Vec<(u32, String)>>, field| {
                    if field.0 == 8 { acc.push(Vec::new()); }
                    if let Some(last) = acc.last_mut() { last.push(field); }
                    acc
                });
            // A frame is whole once its checksum is read.
            out.retain(|f| f.iter().any(|(t, _)| *t == 10));
        }
        out
    }

    // ibx#311 ibx#329: the bracket children carry the parent's server id
    // with its version, the parent's server id as the OCA group, and
    // cancel on fill. The bracket's own ids are versioned like every other
    // order, each under its own server id, its API order id in its own field.
    #[test]
    fn bracket_children_link_to_the_parent_like_the_reference() {
        let frames = wire_frames(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        }, 3);
        assert_eq!(frames.len(), 3);
        let server = |f: &Vec<(u32, String)>| tag(f, 11).and_then(|c| c.strip_suffix(".0")).unwrap().to_string();
        let parent = server(&frames[0]);
        assert_eq!(tag(&frames[0], 6121), Some("3"));
        for absent in [583, 6107, 6209] {
            assert!(tag(&frames[0], absent).is_none(), "parent sends no field {}", absent);
        }
        for (f, id) in [(&frames[1], "4"), (&frames[2], "5")] {
            assert_eq!(tag(f, 6121), Some(id));
            assert_ne!(server(f), parent);
            assert_eq!(tag(f, 6107), Some(format!("{parent}.0").as_str()));
            assert_eq!(tag(f, 583), Some(parent.as_str()));
            assert_eq!(tag(f, 6209), Some("ReduceOnFillNonBlock"));
        }
    }

    // The three frames of a bracket against the reference's (four-leg
    // bracket of 26/09/2026, ib-agent captures/four-leg/20260926, a DAY
    // parent LMT, a LMT take-profit and a STP stop-loss on AAPL), field by
    // field and in order. The ids are masked as their role (parent, child);
    // left out: the account, the client id, the bracket colour, and
    // what the bracket request does not carry (the orderRef, the price
    // management flag). The children of the bracket request are GTC (the GTC time in force, the
    // reference's form for a GTC child, pd_bracket_keys of 01/10/2026);
    // the recorded ones were DAY (ibx#486, phase 51 of 05/10/2026).
    #[test]
    fn bracket_frames_as_the_reference() {
        const REFERENCE: [&str; 3] = [
            "35=D|11=1339547416.0|44=272.86|1=DUXXXXXXX|8339=1|6122=c|6010=fourleg|6531=1/0/-9911633|6121=3|6119=198|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=",
            "35=D|11=1339547417.0|44=300.14|1=DUXXXXXXX|8339=1|583=1339547416|6122=c|6010=fourleg|6531=1/1/-9911633|6121=4|6119=198|38=1|40=2|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6209=ReduceOnFillNonBlock|6088=Socket|6107=1339547416.0|15=USD|6211=|6238=",
            "35=D|11=1339547418.0|99=245.57|1=DUXXXXXXX|6117=245.57|583=1339547416|6122=c|6010=fourleg|6531=1/2/-9911633|6115=0|6121=5|6119=198|38=1|40=3|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6209=ReduceOnFillNonBlock|6088=Socket|6107=1339547416.0|15=USD|6211=|6238=",
        ];
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        let inst = context.market.register(265598);
        context.set_symbol(inst, "AAPL".to_string());
        context.pending_orders.push(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: inst, side: Side::Buy, qty: 1,
            entry_price: px(272.86), take_profit: px(300.14), stop_loss: px(245.57),
        });
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 16384];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 { break; }
            bytes.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&bytes).replace('\x01', "|");
        let ours: Vec<&str> = text.split("8=FIX.4.1|").filter(|m| !m.is_empty()).collect();
        assert_eq!(ours.len(), 3, "{text}");
        // The parent's ClOrdID base on each side, for the role masks.
        let base = |frame: &str| frame.split('|').find_map(|f| f.strip_prefix("11=")).and_then(|c| c.split('.').next()).unwrap().to_string();
        let form = |frame: &str, parent: &str| -> Vec<String> {
            frame.split('|').filter_map(|f| {
                let (t, v) = f.split_once('=')?;
                let t: u32 = t.parse().ok()?;
                if matches!(t, 8 | 9 | 34 | 52 | 10 | 1 | 6119 | 6010 | 8339) { return None; }
                let v = match t {
                    11 => if v.starts_with(&format!("{parent}.")) { "{parent}.0".to_string() } else { "{child}.0".to_string() },
                    6107 => v.replacen(parent, "{parent}", 1),
                    583 => v.replacen(parent, "{parent}", 1),
                    6531 => v.rsplitn(2, '/').nth(1).unwrap_or(v).to_string(),
                    _ => v.to_string(),
                };
                Some(format!("{t}={v}"))
            }).collect()
        };
        let (pr, po) = (base(REFERENCE[0]), base(ours[0]));
        for (k, (r, o)) in REFERENCE.iter().zip(&ours).enumerate() {
            let mut want = form(r, &pr);
            if k > 0 {
                for f in want.iter_mut() { if f == "59=0" { *f = "59=1".to_string(); } }
            }
            assert_eq!(form(o, &po), want, "frame {k}");
        }
    }

    // A bracket child with the OCA type unset gets the default type, as the
    // reference's child of 01/10/2026 (ib-agent captures/pd-orders,
    // pd_bracket_keys); a type the caller gives is kept (ib-agent#192 A5,
    // B1, B11, all with the type set to cancel on fill).
    #[test]
    fn a_bracket_child_without_an_oca_type_gets_the_default_one() {
        const CAPTURED: &str = "35=D|11=55485701.0|44=494.03|1=DU1|6010=fourleg|6122=c|583=55485700|6531=1/1/-4293003|8339=1|6121=57|6119=198|38=1|40=2|55=AAPL|167=STK|231=1.00|54=2|59=1|100=BEST|6210=BEST|6008=265598|6209=ReduceOnFillNonBlock|6088=Socket|6107=55485700.0|15=USD|6211=|6238=";
        let want = captured(CAPTURED, &[583, 6209, 6107]);
        let child = |oca_type| OrderRequest::SubmitLimitEx {
            order_id: 55485701, instrument: 0, side: Side::Sell, qty: 1, price: px(494.03), tif: b'1',
            attrs: crate::types::OrderAttrs { parent_id: 55485700, oca_type, ..Default::default() },
        };
        let ours = wire_tags(child(0));
        assert_eq!(ours_as(&ours, &want), want);
        assert_eq!(tag(&wire_tags(child(1)), 6209), Some("CancelOnFillWBlock"));
    }

    fn child_of(parent_id: OrderId, oca_type: u8) -> OrderRequest {
        OrderRequest::SubmitEx {
            order_id: 7, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 110 * P }, tif: b'1',
            attrs: crate::types::OrderAttrs {
                parent_id, oca_group_str: "MY-GROUP".into(), oca_type, ..Default::default()
            },
        }
    }

    // ibx#311: a child placed after its parent was modified once refers to
    // the parent's current version, not the first one.
    #[test]
    fn a_child_refers_to_the_current_version_of_its_parent() {
        let fresh = wire_tags(child_of(100, 0));
        assert_eq!(tag(&fresh, 6107), Some("100.0"));

        let modified = wire_tags_with(|ctx| {
            ctx.modify_versions.insert(100, 1);
            ctx.last_clord.insert(100, "100.1".into());
        }, child_of(100, 0));
        assert_eq!(tag(&modified, 6107), Some("100.1"));
        assert_eq!(tag(&modified, 583), Some("100"), "the caller's group name is replaced");
        assert_eq!(tag(&modified, 6209), Some("ReduceOnFillNonBlock"), "default for a child");

        // Before the server echoes the replace, the version ibx sent.
        let pending = wire_tags_with(|ctx| { ctx.modify_versions.insert(100, 2); }, child_of(100, 0));
        assert_eq!(tag(&pending, 6107), Some("100.2"));
    }

    // A parent held by the server under another id (an order of another
    // session): the link and the group are that id, as the server has it.
    #[test]
    fn a_child_of_a_recovered_parent_uses_the_id_the_server_holds() {
        let tags = wire_tags_with(|ctx| {
            ctx.last_clord.insert(15, "1626578573.0".into());
        }, child_of(15, 0));
        assert_eq!(tag(&tags, 6107), Some("1626578573.0"));
        assert_eq!(tag(&tags, 583), Some("1626578573"));
    }

    // An OCA type the caller sets explicitly is kept; an order in an OCA
    // group with no parent keeps its group name and type.
    #[test]
    fn oca_type_and_group_without_a_parent_are_kept() {
        let explicit = wire_tags(child_of(100, 2));
        assert_eq!(tag(&explicit, 6209), Some("ReduceOnFillWBlock"));

        let plain = wire_tags(child_of(0, 0));
        assert_eq!(tag(&plain, 583), Some("MY-GROUP"));
        assert_eq!(tag(&plain, 6209), Some("ReduceOnFillNonBlock"));
        assert!(tag(&plain, 6107).is_none());
    }

    fn tag<'a>(tags: &'a [(u32, String)], t: u32) -> Option<&'a str> {
        tags.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
    }

    fn pos(tags: &[(u32, String)], t: u32) -> usize {
        tags.iter().position(|(k, _)| *k == t).unwrap_or_else(|| panic!("tag {} missing", t))
    }

    const P: i64 = crate::types::PRICE_SCALE;

    // The plain adjustable stop keeps its values (ibx#240); its fields are
    // where the reference's writer puts them, right after the account,
    // the trailing unit with the order attributes (ibx#375).
    #[test]
    fn plain_adjustable_stop_keeps_its_captured_shape() {
        let tags = wire_tags(OrderRequest::SubmitAdjustableStop {
            order_id: 5, instrument: 0, side: Side::Sell, qty: 1,
            stop_price: 11 * P, trigger_price: 12 * P,
            adjusted_order_type: crate::types::AdjustedOrderType::Trail,
            adjusted_stop_price: 10 * P, adjusted_stop_limit_price: 0,
            adjusted_trailing_amount: P / 2, adjustable_trailing_unit: 0,
        });
        let at = pos(&tags, 1) + 1;
        let block: Vec<u32> = tags[at..at + 5].iter().map(|(t, _)| *t).collect();
        assert_eq!(block, vec![6257, 6261, 6258, 6259, 6260]);
        assert!(pos(&tags, 6269) > pos(&tags, 6260) && pos(&tags, 6269) < pos(&tags, 38));
        assert_eq!(tag(&tags, 59), Some("0"));
        assert_eq!(tag(&tags, 6261), Some("T"));
        assert_eq!(tag(&tags, 6260), Some("0.50"));
        assert!(tag(&tags, 6107).is_none() && tag(&tags, 583).is_none());
    }

    // ibx#240: an adjustable stop used as a bracket child shipped with no
    // parent link, no OCA group and a forced DAY tif.
    #[test]
    fn extended_adjustable_stop_carries_parent_oca_and_tif() {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 6, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::AdjustableStop {
                stop_price: 11 * P, trigger_price: 12 * P,
                adjusted_order_type: crate::types::AdjustedOrderType::Stop,
                adjusted_stop_price: 10 * P, adjusted_stop_limit_price: 0,
                adjusted_trailing_amount: 0, adjustable_trailing_unit: 0,
            },
            tif: b'1',
            attrs: crate::types::OrderAttrs {
                parent_id: 100, oca_group_str: "BR1".into(), oca_type: 1,
                ..Default::default()
            },
        });
        assert_eq!(tag(&tags, 40), Some("3"));
        assert_eq!(tag(&tags, 99), Some("11.00"));
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6107), Some("100.0"));
        // A child's OCA group is its parent's id (ibx#329).
        assert_eq!(tag(&tags, 583), Some("100"));
        assert_eq!(tag(&tags, 6257), Some("1"));
        assert_eq!(tag(&tags, 6261), Some("3"));
        assert_eq!(tag(&tags, 6258), Some("12.00"));
        assert_eq!(tag(&tags, 6259), Some("10.00"));
        // Same placement as the plain path: right after the account,
        // before the attributes (ibx#375).
        assert_eq!(pos(&tags, 6257), pos(&tags, 1) + 1);
        assert!(pos(&tags, 6259) < pos(&tags, 583));
        assert!(tag(&tags, 6260).is_none(), "trail tags only for a trail conversion");
    }

    /// Every secondary routing field in `tags` is followed by the contract id.
    fn contract_id_follows_routing(tags: &[(u32, String)], con_id: &str) -> bool {
        tags.iter().enumerate().filter(|(_, (t, _))| *t == 6210)
            .all(|(i, _)| tags.get(i + 1) == Some(&(6008, con_id.to_string())))
    }

    // ibx#328: the reference sends the contract id right after the secondary
    // routing field on every new order (ib-agent#192 B4, 21/21 frames and
    // all 63 of groups A and B); ibx sent it only on a replace.
    #[test]
    fn new_orders_carry_the_contract_id_after_the_routing_field() {
        let limit = wire_tags(OrderRequest::SubmitLimit {
            order_id: 1, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P,
        });
        assert_eq!(tag(&limit, 6008), Some("265598"));
        assert!(contract_id_follows_routing(&limit, "265598"));

        let extended = wire_tags(OrderRequest::SubmitEx {
            order_id: 2, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P }, tif: b'1',
            attrs: crate::types::OrderAttrs { outside_rth: true, ..Default::default() },
        });
        assert_eq!(tag(&extended, 6008), Some("265598"));
        assert!(contract_id_follows_routing(&extended, "265598"));

        // Parent and both children: every frame read carries it.
        let bracket = wire_tags(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        });
        assert!(bracket.iter().any(|(t, _)| *t == 6210));
        assert!(contract_id_follows_routing(&bracket, "265598"));
    }

    // ibx#486: the redirect precaution (`OrderChecker$6`): a stock order
    // directed away from SMART, on a contract that trades on SMART, is
    // discarded (10311, or 10329 for OVERNIGHT; Cancelled; 201) and nothing
    // goes out; with the bypass on, or a contract not on SMART, it goes.
    #[test]
    fn a_directed_stock_order_is_discarded_unless_bypassed() {
        let run_with = |exchange: &str, smart: bool, bypass: bool, stp_prt: bool| {
            let mut context = order_context();
            context.bypass_redirect_warning = bypass;
            context.algo_providers = vec!["FOXRIVER".into()];
            let id = context.market.register(265598);
            context.order_records_any.insert(265598);
            context.market.set_symbol(id, "AAPL".into());
            context.market.set_routing(id, "STK", exchange);
            context.rth_types.insert((265598, exchange.to_string()), crate::engine::outside_rth::RthTypes {
                rth: true, sec_type: "STK".into(), smart, types_known: true, ..Default::default()
            });
            context.pending_orders.push(if stp_prt {
                OrderRequest::SubmitEx { order_id: 8, instrument: id, side: Side::Buy, qty: 1,
                    kind: crate::types::OrderKind::StpPrt { stop_price: 400 * P }, tif: b'0', attrs: Default::default() }
            } else {
                OrderRequest::SubmitLimit { order_id: 8, instrument: id, side: Side::Buy, qty: 1, price: 100 * P }
            });
            let shared = Arc::new(SharedState::new());
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let sent = frames.iter().any(|f| tag(f, 35) == Some("D"));
            let errors: Vec<(i64, i64, String)> = shared.orders.drain_order_errors();
            let statuses: Vec<(OrderStatus, i64, i64)> = shared.orders.drain_order_updates().iter()
                .map(|u| (u.status, u.remaining_qty_fixed, u.perm_id)).collect();
            let notices = shared.orders.drain_order_notices();
            (sent, errors, statuses, notices)
        };
        let run = |exchange: &str, smart: bool, bypass: bool| run_with(exchange, smart, bypass, false);
        let (sent, errors, statuses, notices) = run("OVERNIGHT", true, false);
        assert!(!sent);
        assert_eq!(errors, [(8, 10329, "This order will be directly routed to OVERNIGHT.\nRestriction is specified in Precautionary Settings of Global Configuration/API.".to_string())]);
        // Cancelled with a permId of the session's sequence, not the
        // order id (the reference 10.51, paper, regular hours 13/11/2026).
        assert_eq!(statuses.len(), 1);
        assert_eq!((statuses[0].0, statuses[0].1), (OrderStatus::Cancelled, crate::types::QTY_SCALE));
        assert_ne!(statuses[0].2, 8);
        assert_eq!(notices, [(8, 201, "Order rejected - reason:Order was discarded.".to_string())]);
        let (sent, errors, ..) = run("ISLAND", true, false);
        assert!(!sent);
        assert_eq!(errors[0].1, 10311);
        // 10.51 names the exchange as given (ibx#709; 10.40 named it NASDAQ).
        assert!(errors[0].2.starts_with("This order will be directly routed to ISLAND. Direct routed orders may result in higher trade fees.\n"));
        let (sent, errors, statuses, _) = run("ISLAND", true, true);
        assert!(sent && errors.is_empty() && statuses.is_empty(), "bypassed");
        let (sent, errors, ..) = run("ISLAND", false, false);
        assert!(sent && errors.is_empty(), "a contract that does not trade on SMART");
        // ibx#505: an algo provider's destination goes through SMART.
        let (sent, errors, ..) = run("FOXRIVER", true, false);
        assert!(sent && errors.is_empty(), "an algo provider");
        // ibx#688: before the order-type check (the reference 10.51, paper,
        // 18/10/2026: STP PRT on ISLAND discarded with 10311, not 387).
        let (sent, errors, ..) = run_with("ISLAND", true, false, true);
        assert!(!sent);
        assert_eq!(errors.iter().map(|e| e.1).collect::<Vec<_>>(), [10311]);
        let (sent, errors, ..) = run_with("ISLAND", true, true, true);
        assert!(!sent);
        assert_eq!(errors.iter().map(|e| e.1).collect::<Vec<_>>(), [387], "bypassed: the type check");
    }

    // ibx#486: an order on a contract given without a conId waits for the
    // contract's lookup by symbol, as the reference looks each API order's
    // contract up (captured: the symbol lookup `FixSecDefReqBySymbol200`, source Socket,
    // AAPL, a stock, BEST, USD, before the new order); the answer gives the
    // order its conId. A second order of the same contract is looked up
    // again and takes the slot of that conId; no contract gives 200.
    // ibx#669: the order rules come from the order's own lookup reply when
    // it holds the record of the order's exchange: after the reply, the
    // order goes out with no second definition asked.
    #[test]
    fn the_lookup_reply_gives_the_order_rules() {
        let mut context = order_context();
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "SMART");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 6, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        assert_eq!(drain_frames(&mut context, &shared, &mut conn, &mut server).len(), 1, "the lookup");
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        let reply = format!("8=FIX.4.1|35=d|320={id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|6431=LMT/3,MKT/1,STP/1,RTH/1,PRICECHK/1|6046=BEST,NASDAQ,ARCA|146=1|6344=1|6008=265598|10=000|").replace('|', "");
        assert!(order_contract_reply(&mut context, &shared, &id, reply.as_bytes()));
        assert!(context.rth_types.get(&(265598, "BEST".to_string())).is_some_and(|t| t.price_chk && t.smart));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.iter().all(|f| !tag(f, 320).is_some_and(|v| v.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME))), "{frames:?}");
    }

    // ibx#688: placed again while its new order waits for a lookup: 103,
    // the order stays as placed and goes out alone (the reference 10.51,
    // paper, 18/10/2026: 103 to a re-place before the order went out).
    #[test]
    fn a_re_place_of_an_order_not_sent_yet_is_a_duplicate() {
        let mut context = order_context();
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "SMART");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 6, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        assert_eq!(drain_frames(&mut context, &shared, &mut conn, &mut server).len(), 1, "the lookup");
        context.pending_orders.push(OrderRequest::Modify {
            new_order_id: 6, order_id: 6, qty: 1, kind: crate::types::OrderKind::Limit { price: 101 * P }, tif: b'0', attrs: Default::default(),
        });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(shared.orders.drain_order_errors(), [(6, 103, "Duplicate order id: 6".to_string())]);
        assert!(context.rth_parked.iter().all(|r| !matches!(r, OrderRequest::Modify { .. })), "the modify is dropped");
    }

    // ibx#667: an order for the broker's ATS venue given by symbol is
    // looked up on SMART, as the reference (pre-market 13/10/2026).
    #[test]
    fn an_order_for_the_ats_venue_is_looked_up_on_smart() {
        let mut context = order_context();
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "IBKRATS");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 6, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!((tag(&frames[0], 35), tag(&frames[0], 100), tag(&frames[0], 207)), (Some("c"), Some("BEST"), None));
        // The order then goes to BEST with the venue mark after its time in
        // force (the reference 10.51, paper, pre-market 14/10/2026).
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        let reply = format!("8=FIX.4.1|35=d|320={id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|6431=LMT/3,MKT/1,STP/1,RTH/1|6046=BEST,NASDAQ,IBKRATS|146=1|6344=1|6008=265598|10=000|").replace('|', "");
        assert!(order_contract_reply(&mut context, &shared, &id, reply.as_bytes()));
        context.bypass_redirect_warning = true;
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let order = frames.iter().find(|f| tag(f, 35) == Some("D")).unwrap_or_else(|| panic!("no order: {frames:?}"));
        let tags: Vec<u32> = order.iter().map(|(t, _)| *t).filter(|t| matches!(t, 59 | 8032 | 100)).collect();
        assert_eq!(tags, [59, 8032, 100], "{order:?}");
        assert_eq!((tag(order, 8032), tag(order, 100)), (Some("1"), Some("BEST")));
    }

    // The reference 10.51, paper, regular hours 13/11/2026: an order for
    // the venue given by its symbol, after orders of the contract on SMART,
    // keeps the venue: the redirect precaution discards it and names it;
    // and a discarded order takes the next id of the session's orders.
    #[test]
    fn an_order_for_the_ats_venue_by_symbol_keeps_its_venue_when_the_contract_is_known_on_smart() {
        let mut context = order_context();
        let known = context.market.register(265598);
        context.market.set_symbol(known, "AAPL".into());
        context.market.set_routing(known, "STK", "SMART");
        context.market.set_currency(known, "USD");
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "IBKRATS");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 6, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        let reply = format!("8=FIX.4.1|35=d|320={id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|6431=LMT/3,MKT/1,STP/1,RTH/1|6046=BEST,NASDAQ,IBKRATS|146=1|6344=1|6008=265598|10=000|").replace('|', "\x01");
        assert!(order_contract_reply(&mut context, &shared, &id, reply.as_bytes()));
        assert_eq!(context.market.exchange(slot), "IBKRATS", "the order keeps its own slot");
        assert_eq!(context.market.con_id(slot), Some(265598));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "{frames:?}");
        let errors = shared.orders.drain_order_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!((errors[0].0, errors[0].1), (6, 10311));
        assert!(errors[0].2.starts_with("This order will be directly routed to IBKRATS."), "{}", errors[0].2);
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, OrderStatus::Cancelled);
        assert_ne!(updates[0].perm_id, 6);
        assert_eq!(context.new_server_id(), updates[0].perm_id + 1, "the next order's id follows");
    }

    #[test]
    fn a_new_order_without_a_contract_id_waits_for_its_lookup() {
        let mut context = order_context();
        let known = context.market.register(265598);
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "SMART");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 6, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let lookup: Vec<(u32, String)> = frames[0].iter().filter(|(t, _)| !matches!(*t, 8 | 9 | 34 | 52 | 10)).cloned().collect();
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        // Named in the session series on the wire (ibx#698).
        let want: Vec<(u32, String)> = [(35, "c"), (320, "FixSecDefReqBySymbol1"), (321, "2"), (6088, "Socket"), (55, "AAPL"), (167, "CS"), (100, "BEST"), (15, "USD")]
            .iter().map(|(t, v)| (*t, v.to_string())).collect();
        assert_eq!(lookup, want);
        assert_eq!(context.rth_parked.len(), 1);
        // The answer: AAPL, a conId another slot routes the same way.
        let reply = "8=FIX.4.1|9=0100|35=d|320=FixSecDefReqBySymbol3221225472|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|146=1|6344=1|6008=265598|10=000|".replace('|', "\x01");
        assert!(order_contract_reply(&mut context, &shared, &id, reply.as_bytes()));
        assert_eq!(context.market.con_id(slot), None, "the slot is freed");
        assert_eq!(context.drain_pending_orders().next().and_then(|r| r.instrument()), Some(known));
        // No contract: 200 to the waiting order, which is not kept.
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "XYZ".into());
        context.rth_parked.push(OrderRequest::SubmitLimit { order_id: 7, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        context.order_lookups.push((ORDER_LOOKUP_FIRST_ID + 1, slot));
        let empty = "8=FIX.4.1|35=d|320=FixSecDefReqBySymbol3221225473|322=*|323=4|6038=Y|6019=0|6344=0|".replace('|', "\x01");
        assert!(order_contract_reply(&mut context, &shared, "FixSecDefReqBySymbol3221225473", empty.as_bytes()));
        assert!(context.rth_parked.is_empty() && !context.api_pending.contains_key(&7));
        assert_eq!(shared.orders.drain_order_errors(), [(7, 200, "No security definition has been found for the request".to_string())]);
        // The client forgets it: no open-order request lists it (ibx#738).
        assert_eq!(shared.orders.drain_forgotten_orders(), vec![7]);
    }

    // ibx#710: an order whose contract gives a primary exchange carries it
    // in its lookup, after the exchange, as the reference wrote it (paper,
    // 15/10/2026: AAPL on ARCA with primary exchange NASDAQ); the server's
    // refusal of the lookup gives the order 200 with its text, and the
    // order is not kept.
    #[test]
    fn an_order_lookup_carries_the_primary_exchange_and_its_refusal_drops_the_order() {
        let mut context = order_context();
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "ARCA");
        context.market.set_currency(slot, "USD");
        context.order_lookup_filters.insert(slot, crate::types::SecDefFilters { primary_exchange: "NASDAQ".into(), ..Default::default() });
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 9, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let lookup: Vec<(u32, String)> = frames[0].iter().filter(|(t, _)| !matches!(*t, 8 | 9 | 34 | 52 | 10)).cloned().collect();
        let want: Vec<(u32, String)> = [(35, "c"), (320, "FixSecDefReqBySymbol1"), (321, "2"), (6088, "Socket"), (55, "AAPL"), (167, "CS"),
            (100, "ARCA"), (207, "NASDAQ"), (15, "USD")].iter().map(|(t, v)| (*t, v.to_string())).collect();
        assert_eq!(lookup, want);
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        assert!(order_lookup_refused(&mut context, &shared, &id, "Invalid destination exchange specified"));
        assert!(context.rth_parked.is_empty() && context.order_lookups.is_empty());
        assert_eq!(shared.orders.drain_order_errors(), [(9, 200, "Invalid destination exchange specified".to_string())]);
        assert_eq!(shared.orders.drain_forgotten_orders(), vec![9], "forgotten by the client (ibx#738)");
        assert!(!order_lookup_refused(&mut context, &shared, &id, "again"), "a lookup refused once");
    }

    // ── Replace (ibx#247 ibx#324 ibx#334 ibx#349) ──
    //
    // Each case replays one replace the reference sent (ib-agent#192 group
    // A, account id replaced) and checks ibx sends the same fields in the
    // same order with the same values. Left out on purpose: a field written only when
    // the original order carried a price cap (meaning unknown; the server
    // accepts replaces without it) and the bracket group index (which ibx
    // does not send on submit either).

    fn parse_frame(s: &str) -> Vec<(u32, String)> {
        use crate::test_support::normalise::{parse_pipe, ORDER_UNSTABLE};
        parse_pipe(s).into_iter().filter(|(t, _)| !ORDER_UNSTABLE.contains(t)).collect()
    }

    /// Send one Modify for a working order `order_id` (AAPL, the given side)
    /// and return the fields, framing and timestamps removed.
    fn replace_fields(order_id: OrderId, side: Side, qty: u32, kind: crate::types::OrderKind,
                      tif: u8, attrs: crate::types::OrderAttrs) -> Vec<(u32, String)> {
        wire_tags_with(
            |ctx| {
                ctx.set_symbol(0, "AAPL".to_string());
                ctx.insert_order(Order::new(order_id, 0, side, qty, 0, b'2', b'0', 0));
            },
            OrderRequest::Modify { new_order_id: order_id, order_id, qty, kind, tif, attrs },
        )
        .into_iter()
        .filter(|(t, _)| !crate::test_support::normalise::FRAMING.contains(t))
        .collect()
    }

    fn assert_same_replace(ours: &[(u32, String)], reference: &str) {
        use crate::test_support::normalise::{assert_same_fields, Normaliser, ORDER_UNSTABLE};
        let n = Normaliser::framing().drop(ORDER_UNSTABLE);
        assert_same_fields(&n.apply(ours), &n.pipe(reference));
    }

    fn px(v: f64) -> i64 { (v * crate::types::PRICE_SCALE as f64).round() as i64 }

    fn attrs_rth(outside_rth: bool) -> crate::types::OrderAttrs {
        crate::types::OrderAttrs { outside_rth, ..Default::default() }
    }

    #[test]
    fn replace_limit_without_outside_rth_matches_reference() {
        let ours = replace_fields(9000000553, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: px(241.22) }, b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000553.1|41=9000000553.0|44=241.22|1=DU1|6205=1|6122=c|38=1|54=1|40=2|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_limit_with_outside_rth_matches_reference() {
        let ours = replace_fields(9000000554, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: px(241.22) }, b'0', attrs_rth(true));
        assert_same_replace(&ours, "35=G|11=9000000554.1|41=9000000554.0|44=241.22|1=DU1|6122=c|6433=1|38=1|54=1|40=2|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_stop_moves_the_trigger_like_reference() {
        let ours = replace_fields(9000000555, Side::Sell, 1,
            crate::types::OrderKind::Stop { stop_price: px(234.43) }, b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000555.1|41=9000000555.0|99=234.43|1=DU1|6117=234.43|6122=c|38=1|54=2|40=3|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_stop_limit_moves_both_prices_like_reference() {
        let ours = replace_fields(9000000556, Side::Sell, 1,
            crate::types::OrderKind::StopLimit { price: px(227.63), stop_price: px(231.03) }, b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000556.1|41=9000000556.0|44=227.63|99=231.03|1=DU1|6117=231.03|6205=1|6122=c|38=1|54=2|40=4|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_trailing_amount_matches_reference() {
        let ours = replace_fields(9000000557, Side::Sell, 1,
            crate::types::OrderKind::TrailingStop { trail_amt: px(105.32), trail_stop_price: 0 }, b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000557.1|41=9000000557.0|99=105.32|1=DU1|6122=c|6268=0|38=1|54=2|40=P|211=105.32|18=a|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_trailing_limit_matches_reference() {
        // The initial stop trigger is not restated on a replace.
        let ours = replace_fields(9000000568, Side::Sell, 1,
            crate::types::OrderKind::TrailingStopLimit { lmt_offset: px(0.50), lmt_price: None, trail_amt: px(105.32), trail_stop_price: px(237.82) },
            b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000568.1|41=9000000568.0|99=105.32|1=DU1|6370=0.50|6205=1|6122=c|6268=0|38=1|54=2|40=TSL|211=105.32|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_trailing_percent_matches_reference() {
        let ours = replace_fields(9000000558, Side::Sell, 1,
            crate::types::OrderKind::TrailPct { trail_percent: px(31.0), trail_stop_price: 0 }, b'0', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000558.1|41=9000000558.0|99=31.00|1=DU1|6122=c|6268=100|38=1|54=2|40=P|211=31.00|18=a|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_carries_the_new_time_in_force_like_reference() {
        let ours = replace_fields(9000000559, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: px(237.82) }, b'1', attrs_rth(false));
        assert_same_replace(&ours, "35=G|11=9000000559.1|41=9000000559.0|44=237.82|1=DU1|6205=1|6122=c|38=1|54=1|40=2|55=AAPL|167=STK|6035=AAPL|59=1|6008=265598|6088=Socket|6211=|6238=");
    }

    #[test]
    fn replace_good_till_stop_matches_reference() {
        let attrs = crate::types::OrderAttrs { good_till: 1_790_798_400, ..Default::default() }; // 20260930 20:00:00 UTC
        let ours = replace_fields(9000000575, Side::Sell, 200,
            crate::types::OrderKind::Stop { stop_price: px(200.45) }, b'6', attrs);
        assert_same_replace(&ours, "35=G|11=9000000575.1|41=9000000575.0|99=200.45|1=DU1|126=20260930-20:00:00|6117=200.45|6122=c|6531=4/2/-1|38=200|54=2|40=3|55=AAPL|167=STK|6035=AAPL|59=6|6008=265598|6088=Socket|6211=|6238=");
    }

    // ibx#339: the reference sends a percent trail as the percent in the
    // price fields and the trail unit set to percent at every percentage
    // (ib-agent#192 B7: 0.5%, 1%, 1.25%, 2%). ibx sent basis points in the
    // unit field, which only matched at exactly 1%.
    #[test]
    fn percent_trail_submit_matches_reference_at_every_percentage() {
        // A percent with more decimals goes out as given (ibx#263): the
        // reference writes it with its price formatter.
        for (percent, pct) in [(0.5, "0.50"), (1.0, "1.00"), (1.25, "1.25"), (2.0, "2.00"), (1.239, "1.239"), (0.12345678, "0.12345678")] {
            let bp = crate::api::types::price_from_f64(percent);
            let plain = wire_tags(OrderRequest::SubmitTrailingStopPct {
                order_id: 7, instrument: 0, side: Side::Sell, qty: 1, trail_percent: bp, trail_stop_price: 0,
            });
            let ext = wire_tags(OrderRequest::SubmitEx {
                order_id: 8, instrument: 0, side: Side::Sell, qty: 1,
                kind: crate::types::OrderKind::TrailPct { trail_percent: bp, trail_stop_price: 0 },
                tif: b'1', attrs: crate::types::OrderAttrs::default(),
            });
            for tags in [&plain, &ext] {
                assert_eq!(tag(tags, 40), Some("P"));
                assert_eq!(tag(tags, 99), Some(pct), "{} bp", bp);
                assert_eq!(tag(tags, 211), Some(pct), "{} bp", bp);
                assert_eq!(tag(tags, 18), Some("a"));
                assert_eq!(tag(tags, 6268), Some("100"), "unit is percent at {} bp", bp);
            }
        }
    }

    fn bracket_child_attrs() -> crate::types::OrderAttrs {
        crate::types::OrderAttrs { parent_id: 9000000577, oca_group_str: "BR1".into(), oca_type: 1, ..Default::default() }
    }

    // ibx#318: an algo, adaptive or what-if order used as a bracket child
    // shipped DAY with no parent link and no OCA group. The reference sends
    // them like any other child (ib-agent#192 B1).
    #[test]
    fn algo_bracket_child_carries_parent_oca_and_gtc() {
        let tags = wire_tags(OrderRequest::SubmitAlgo {
            order_id: 9, instrument: 0, side: Side::Sell, qty: 1, price: 509 * crate::types::PRICE_SCALE,
            algo: AlgoParams::Twap { allow_past_end_time: true, start_time: String::new(), end_time: String::new() },
            tif: b'1', attrs: bracket_child_attrs(),
        });
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6107), Some("9000000577.0"));
        assert_eq!(tag(&tags, 583), Some("9000000577"));
        assert_eq!(tag(&tags, 6209), Some("CancelOnFillWBlock"));
        assert_eq!(tag(&tags, 847), Some("Twap"));
        // Unset start/end times are left out, as the reference does.
        assert_eq!(tag(&tags, 5957), Some("1"));
        assert!(!tags.iter().any(|(t, v)| *t == 5958 && (v == "startTime" || v == "endTime")), "{:?}", tags);
        // ibx#405: every algo type rides the algo instruction, not only Adaptive.
        assert_eq!(tag(&tags, 18), Some("e"));
        assert_eq!(tags.iter().filter(|(t, _)| *t == 18).count(), 1, "no second instruction");
    }

    #[test]
    fn adaptive_bracket_child_carries_parent_oca_and_gtc() {
        let tags = wire_tags(OrderRequest::SubmitAdaptive {
            order_id: 10, instrument: 0, side: Side::Sell, qty: 1, price: 509 * crate::types::PRICE_SCALE,
            priority: crate::types::AdaptivePriority::Normal,
            tif: b'1', attrs: bracket_child_attrs(),
        });
        assert_eq!(tag(&tags, 18), Some("e"));
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6107), Some("9000000577.0"));
        assert_eq!(tag(&tags, 583), Some("9000000577"));
        assert_eq!(tag(&tags, 847), Some("Adaptive"));
    }

    #[test]
    fn what_if_carries_its_time_in_force() {
        let tags = wire_tags(OrderRequest::SubmitWhatIf { request: Box::new(OrderRequest::SubmitLimitEx {
            order_id: 11, instrument: 0, side: Side::Buy, qty: 1, price: 237 * crate::types::PRICE_SCALE,
            tif: b'1', attrs: crate::types::OrderAttrs::default(),
        }) });
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6091), Some("1"));
    }

    /// The what-if request the API mapping builds for `order`.
    fn what_if_of(order: crate::api::types::Order, order_id: OrderId) -> OrderRequest {
        api_request(&crate::api::types::Order { what_if: true, ..order }, order_id)
    }

    // ibx#462 (ib-agent#192 B1e, captured 23/09/2026, account masked): the
    // preview of a LMT GTC order is the order's own frame plus the preview
    // flag, under a ClOrdID of its own.
    #[test]
    fn what_if_is_the_real_order_plus_the_preview_flag() {
        let reference = "35=D|11=1626578592.0|44=237.82|1=DU1|6122=c|6121=54|6119=192|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|6091=1|15=USD|6211=|6238=";
        let lmt = crate::api::types::Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 237.82, tif: "GTC".into(), ..Default::default() };
        let req = what_if_of(lmt, 100);
        assert!(matches!(&req, OrderRequest::SubmitWhatIf { request } if matches!(**request, OrderRequest::SubmitLimitEx { .. })));
        let ours = wire_tags(req);
        let want = captured(reference, &[35, 40, 59, 6091, 100, 6210, 6008]);
        assert_eq!(ours_as(&ours, &want), want);
        assert_eq!(tag(&ours, 44).and_then(|v| v.parse::<f64>().ok()), Some(237.82));
        assert_eq!(tag(&ours, 11), Some(format!("{FIRST}.0").as_str()), "the preview's own ClOrdID, of the order id generator");
        assert_eq!(tag(&ours, 6121), Some("100"));
        assert_eq!(pos(&ours, 6091) + 1, pos(&ours, 15));

        // A MKT preview is a MKT order (captured in ib-agent#160), not a limit at 0.
        let mkt = wire_tags(what_if_of(crate::api::types::Order { tif: "DAY".into(), action: "SELL".into(), total_quantity: 1.0,
            order_type: "MKT".into(), ..Default::default() }, 101));
        assert_eq!((tag(&mkt, 40), tag(&mkt, 44), tag(&mkt, 6091)), (Some("1"), None, Some("1")));

        // An algo preview keeps the algo fields (ibx#462: it went out as a
        // real algo order).
        let vwap = wire_tags(what_if_of(crate::api::types::Order { tif: "DAY".into(), action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 100.0, algo_strategy: "Vwap".into(),
            algo_params: vec![crate::api::types::TagValue { tag: "maxPctVol".into(), value: "0.1".into() }],
            ..Default::default() }, 102));
        assert_eq!((tag(&vwap, 847), tag(&vwap, 18), tag(&vwap, 6091)), (Some("Vwap"), Some("e"), Some("1")));
        let adaptive = wire_tags(what_if_of(crate::api::types::Order { tif: "DAY".into(), action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 100.0, algo_strategy: "Adaptive".into(), ..Default::default() }, 103));
        assert_eq!((tag(&adaptive, 847), tag(&adaptive, 6091)), (Some("Adaptive"), Some("1")));
    }

    // The preview drops the OCA group and type and keeps the parent link
    // (ORDER-WHATIF.md 4).
    #[test]
    fn what_if_drops_the_oca_fields() {
        let child = crate::api::types::Order { tif: "DAY".into(), action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 110.0, parent_id: 7, oca_group: "G".into(), oca_type: 1, ..Default::default() };
        let tags = wire_tags(what_if_of(child, 104));
        assert!(tag(&tags, 583).is_none() && tag(&tags, 6209).is_none(), "{tags:?}");
        assert_eq!(tag(&tags, 6107), Some("7.0"));
        assert_eq!(tag(&tags, 6091), Some("1"));
    }

    // ibx#462: a what-if with the id of a working order is a new preview:
    // the working order, its versions and its ClOrdID are left as they are.
    #[test]
    fn what_if_on_a_working_order_id_does_not_touch_it() {
        let lmt = crate::api::types::Order { tif: "DAY".into(), action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 101.0, ..Default::default() };
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        let working = Order::new(105, 0, Side::Buy, 1, 100 * P, b'2', b'0', 0);
        context.insert_order(working);
        context.modify_versions.insert(105, 2);
        context.last_clord.insert(105, "105.2".into());
        context.pending_orders.push(what_if_of(lmt, 105));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1);
        assert_eq!(tag(&frames[0], 35), Some("D"), "a new preview, never a replace");
        assert_eq!(tag(&frames[0], 11), Some(format!("{FIRST}.0").as_str()));
        assert_eq!(tag(&frames[0], 6121), Some("105"));
        assert!(tag(&frames[0], 41).is_none());
        let order = context.order(105).copied().expect("the working order is kept");
        assert_eq!((order.price, order.status), (100 * P, OrderStatus::PendingSubmit));
        assert_eq!(context.modify_versions.get(&105), Some(&2));
        assert_eq!(context.last_clord.get(&105).map(String::as_str), Some("105.2"));
        assert_eq!(context.what_ifs.len(), 1);
        // A preview of a new id leaves no order behind.
        let new_id = what_if_of(crate::api::types::Order { tif: "DAY".into(), action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() }, 106);
        context.pending_orders.push(new_id);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 11), Some(format!("{}.0", FIRST + 1).as_str()));
        assert!(context.order(106).is_none());
        assert!(context.modify_versions.get(&106).is_none());
        assert!(!context.server_ids.contains_key(&106));
    }

    // A plain algo order (no attributes, DAY) is unchanged apart from the
    // instruction field.
    #[test]
    fn plain_algo_order_sends_no_attribute_fields() {
        let tags = wire_tags(OrderRequest::SubmitAlgo {
            order_id: 12, instrument: 0, side: Side::Buy, qty: 1, price: 237 * crate::types::PRICE_SCALE,
            algo: AlgoParams::Twap { allow_past_end_time: true, start_time: String::new(), end_time: String::new() },
            tif: b'0', attrs: crate::types::OrderAttrs::default(),
        });
        assert_eq!(tag(&tags, 59), Some("0"));
        for absent in [583, 6107, 6209, 6433] {
            assert!(tag(&tags, absent).is_none(), "field {} must not be sent", absent);
        }
    }

    /// The condition block of one order: from the first flag to the last
    /// per-condition field.
    fn condition_block(cancel_order: bool, ignore_rth: bool) -> Vec<(u32, String)> {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 13, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 237 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs {
                conditions: vec![OrderCondition::Price {
                    con_id: 265598, exchange: "SMART".into(),
                    price: (509.62 * crate::types::PRICE_SCALE as f64).round() as i64,
                    is_more: true, trigger_method: 0,
                }],
                conditions_cancel_order: cancel_order,
                conditions_ignore_rth: ignore_rth,
                ..Default::default()
            },
        });
        let start = pos(&tags, 6128);
        let end = pos(&tags, 6947);
        tags[start..=end].to_vec()
    }

    // ibx#327: the two condition flags were swapped and sent only when set.
    // Reference (ib-agent#192 B2b/B2c, one price condition, account removed),
    // with the overnight flag the reference 10.51 writes between the two
    // (ibx#726, observed on paper, 17/10/2026).
    #[test]
    fn condition_block_with_cancel_order_matches_reference() {
        let want = parse_frame("6128=0|8612=0|6151=1|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=509.62|6223=|6245=|6263=|6246=|6947=");
        assert_eq!(condition_block(true, false), want);
    }

    #[test]
    fn condition_block_with_ignore_rth_matches_reference() {
        let want = parse_frame("6128=1|8612=0|6151=0|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=509.62|6223=|6245=|6263=|6246=|6947=");
        assert_eq!(condition_block(false, true), want);
    }

    /// The condition tags of a limit order with these conditions.
    fn conditions_of(conditions: Vec<OrderCondition>) -> Vec<(u32, String)> {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 13, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 237 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs { conditions, ..Default::default() },
        });
        let start = pos(&tags, 6136);
        let end = tags.iter().rposition(|(t, _)| *t == 6947).unwrap();
        tags[start..=end].to_vec()
    }

    // ibx#416: a time condition carries its time right after the operator,
    // then the padding in the reference's list order (ib-agent
    // ORDER-SUBMIT.md 4.2-4.4, static prediction of the reference frame).
    #[test]
    fn time_condition_matches_the_reference_layout() {
        let got = conditions_of(vec![OrderCondition::Time { time: "20991231-23:59:59".into(), is_more: false }]);
        let want = parse_frame("6136=1|6222=3|6137=n|6126=<=|6223=20991231-23:59:59|6123=|6124=|6127=|6125=|6245=|6263=|6246=|6947=");
        assert_eq!(got, want);
    }

    // The other condition types in the reference's order: own tags, then
    // the padding; SMART goes out as BEST, numbers with two decimals.
    #[test]
    fn volume_percent_and_execution_conditions_match_the_reference_layout() {
        let got = conditions_of(vec![
            OrderCondition::Volume { con_id: 756733, exchange: "SMART".into(), volume: 1000, is_more: true },
            OrderCondition::PercentChange { con_id: 756733, exchange: "SMART".into(), percent: 5.0, is_more: true },
            OrderCondition::Execution { symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into() },
        ]);
        let want = parse_frame(concat!(
            "6136=3",
            "|6222=6|6137=a|6126=>=|6123=756733|6124=BEST|6263=1000|6127=|6125=|6223=|6245=|6246=|6947=",
            "|6222=7|6137=a|6126=>=|6123=756733|6124=BEST|6245=5.00|6127=|6125=|6223=|6263=|6246=|6947=",
            "|6222=5|6137=n|6246=symbol=SPY;exchange=*;securityType=STK;|6123=|6124=|6127=|6126=|6125=|6223=|6245=|6263=|6947=",
        ));
        assert_eq!(got, want);
    }

    // ibx#726: the reference 10.51 on paper, 17/10/2026, market closed, an
    // official client at session level 214: an AAPL limit order, good till
    // cancelled, with one price condition, placed, its limit price
    // modified with the conditions unchanged, then cancelled. The three
    // messages it sent (account masked), compared field by field with
    // ibx's: the new order has the overnight flag between the two other
    // flags, the replace the flag and the conditions in the replace's form
    // after the conId, the cancel nothing about the conditions.
    const CONDITION_NEW_1051: &str = "35=D|11=496786905.0|44=50.00|1=DUXXXXXXX|8339=1|6122=c|6121=58118|6119=198|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|6128=0|8612=0|6151=0|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=900.00|6223=|6245=|6263=|6246=|6947=|15=USD|6211=|6238=";
    const CONDITION_REPLACE_1051: &str = "35=G|11=496786905.1|41=496786905.0|44=51.00|1=DUXXXXXXX|6205=1|8339=1|6122=c|38=1|54=1|40=2|55=AAPL|167=STK|6035=AAPL|59=1|6008=265598|8612=0|6136=1|6222=2|6125=900.00|6126=>=|6127=0|6123=|6124=|6223=|6245=|6263=|6246=|6947=|6088=Socket|6211=|6238=";
    const CONDITION_CANCEL_1051: &str = "35=F|11=496786905.2|41=496786905.1|38=1|54=1|1=DUXXXXXXX|6008=265598|6088=Socket|6944=SEL";

    /// The messages ibx sends for these requests, one after the other on
    /// one engine state, each as its fields.
    fn messages_of(requests: Vec<OrderRequest>) -> Vec<Vec<(u32, String)>> {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        // With price management, as the paper session of the recording.
        session(&mut context, true);
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        let mut out = Vec::new();
        for req in requests {
            context.pending_orders.push(req);
            drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
            out.extend(peer.messages().iter().map(|m| crate::test_support::parse_fields(m)));
        }
        out
    }

    #[test]
    fn an_order_with_a_price_condition_placed_modified_and_cancelled_as_the_reference_10_51() {
        use crate::test_support::normalise::{assert_same_fields, Normaliser};
        use crate::types::OrderKind as K;
        let attrs = crate::types::OrderAttrs {
            conditions: vec![OrderCondition::Price { con_id: 265598, exchange: "SMART".into(), price: px(900.0), is_more: true, trigger_method: 0 }],
            ..Default::default()
        };
        let messages = messages_of(vec![
            OrderRequest::SubmitEx { order_id: 58118, instrument: 0, side: Side::Buy, qty: 1, kind: K::Limit { price: px(50.0) }, tif: b'1', attrs: attrs.clone() },
            OrderRequest::Modify { new_order_id: 58118, order_id: 58118, qty: 1, kind: K::Limit { price: px(51.0) }, tif: b'1', attrs },
            OrderRequest::Cancel { order_id: 58118 },
        ]);
        let of_type = |t: &str| messages.iter().filter(|m| m.iter().any(|(tag, v)| *tag == 35 && v == t)).cloned().collect::<Vec<_>>();
        let (new, replace, cancel) = (of_type("D"), of_type("G"), of_type("F"));
        assert_eq!((new.len(), replace.len(), cancel.len()), (1, 1, 1), "{messages:?}");
        // The client id is the session's; the order ids are masked and the
        // attribute left out of every replace comparison is dropped.
        let n = Normaliser::session().drop(&[6119]);
        assert_same_fields(&n.apply(&new[0]), &n.pipe(CONDITION_NEW_1051));
        // The order attributes of a replace come in no fixed order (here the
        // price management flag before the origin field, after it in the
        // replaces recorded on 28/09/2026): compared as a set, the other
        // fields in their order.
        let split = |f: Vec<(u32, String)>| {
            let (mut attributes, rest): (Vec<_>, Vec<_>) = f.into_iter().partition(|(t, _)| is_attribute(*t) || matches!(*t, 6122 | 8339));
            attributes.sort();
            (attributes, rest)
        };
        let (ours, theirs) = (split(n.apply(&replace[0])), split(n.pipe(CONDITION_REPLACE_1051)));
        assert_eq!(ours.0, theirs.0, "the attributes");
        assert_same_fields(&ours.1, &theirs.1);
        assert_same_fields(&n.apply(&cancel[0]), &n.pipe(CONDITION_CANCEL_1051));
    }

    /// The condition fields of a limit order's replace with these conditions.
    fn replace_conditions_of(conditions: Vec<OrderCondition>) -> Vec<(u32, String)> {
        let attrs = crate::types::OrderAttrs { conditions, ..Default::default() };
        let tags = replace_fields(9000000600, Side::Buy, 1, crate::types::OrderKind::Limit { price: px(51.0) }, b'1', attrs);
        let start = pos(&tags, 8612);
        let end = tags.iter().rposition(|(t, _)| *t == 6947).unwrap();
        assert_eq!(tags[start - 1].0, 6008, "after the conId");
        assert_eq!(tags[end + 1].0, 6088, "before the source");
        tags[start..=end].to_vec()
    }

    // ibx#726: the other kinds in the replace's form of the price condition
    // (a marker 2, the value, the operator, then the padding with empty
    // contract fields): not observed yet, only the price kind was.
    #[test]
    fn replace_conditions_of_the_other_kinds_take_the_price_form() {
        let got = replace_conditions_of(vec![
            OrderCondition::Time { time: "20991231-23:59:59".into(), is_more: false },
            OrderCondition::Margin { percent: 30, is_more: true },
            OrderCondition::Execution { symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into() },
            OrderCondition::Volume { con_id: 756733, exchange: "SMART".into(), volume: 1000, is_more: true },
            OrderCondition::PercentChange { con_id: 756733, exchange: "SMART".into(), percent: 5.0, is_more: false },
            OrderCondition::Price { con_id: 265598, exchange: "SMART".into(), price: px(900.0), is_more: false, trigger_method: 2 },
        ]);
        let want = parse_frame(concat!(
            "8612=0|6136=6",
            "|6222=2|6223=20991231-23:59:59|6126=<=|6123=|6124=|6127=|6125=|6245=|6263=|6246=|6947=",
            "|6222=2|6245=30|6126=>=|6123=|6124=|6127=|6125=|6223=|6263=|6246=|6947=",
            "|6222=2|6246=symbol=SPY;exchange=*;securityType=STK;|6123=|6124=|6127=|6126=|6125=|6223=|6245=|6263=|6947=",
            "|6222=2|6263=1000|6126=>=|6123=|6124=|6127=|6125=|6223=|6245=|6246=|6947=",
            "|6222=2|6245=5.00|6126=<=|6123=|6124=|6127=|6125=|6223=|6263=|6246=|6947=",
            "|6222=2|6125=900.00|6126=<=|6127=2|6123=|6124=|6223=|6245=|6263=|6246=|6947=",
        ));
        assert_eq!(got, want);
    }

    // A replace of an order without conditions writes none of their fields.
    #[test]
    fn a_replace_without_conditions_writes_no_condition_field() {
        let tags = replace_fields(9000000601, Side::Buy, 1, crate::types::OrderKind::Limit { price: px(51.0) }, b'1', Default::default());
        assert!(tags.iter().all(|(t, _)| !REPLACE_CONDITION_TAGS.contains(t)), "{tags:?}");
    }

    #[test]
    fn synthesize_pending_cancel_skips_terminal_and_unknown_orders() {
        let mut context = order_context();
        let shared = Arc::new(SharedState::new());
        // Late cancel racing a fill: the order is done, no phase to report.
        context.insert_order(order(8, 10, OrderStatus::Filled));

        synthesize_pending_cancel(&mut context, &shared, 8);
        synthesize_pending_cancel(&mut context, &shared, 999);

        assert_eq!(context.order(8).unwrap().status, OrderStatus::Filled);
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    /// Run one Modify of order 5 through `drain_and_send_orders`; return the
    /// bytes sent and the order errors raised.
    fn modify_of(existing: Option<Order>) -> (usize, Vec<(i64, i64, String)>) {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();

        let mut context = order_context();
        context.market.register(265598);
        if let Some(o) = existing {
            context.insert_order(o);
        }
        context.pending_orders.push(OrderRequest::Modify {
            new_order_id: 5, order_id: 5, qty: 10,
            kind: crate::types::OrderKind::Limit { price: 101 * P },
            tif: b'0', attrs: Default::default(),
        });
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        drop(conn);

        let mut buf = Vec::new();
        let _ = server.read_to_end(&mut buf);
        (buf.len(), shared.orders.drain_order_errors())
    }

    // ibx#463: the reference refuses a modify of an order that is no longer
    // working with error 104 and sends nothing (captured 25/09/2026). The
    // engine no longer holds a filled or cancelled order; a replace built
    // without it went out with side BUY and no symbol.
    #[test]
    fn modify_of_an_order_the_engine_no_longer_holds_is_refused() {
        let (sent, errors) = modify_of(None);
        assert_eq!(sent, 0, "nothing is sent");
        assert_eq!(errors, [(5, 104, "Cannot modify a filled order.".to_string())]);
    }

    #[test]
    fn modify_of_an_order_with_a_pending_cancel_is_refused() {
        let (sent, errors) = modify_of(Some(order(5, 0, OrderStatus::PendingCancel)));
        assert_eq!(sent, 0, "nothing is sent");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1, 104);
    }

    #[test]
    fn modify_of_a_working_order_is_sent() {
        let (sent, errors) = modify_of(Some(order(5, 0, OrderStatus::Submitted)));
        assert!(sent > 0, "the replace is sent");
        assert!(errors.is_empty());
    }

    /// Run one Cancel of order 5 through `drain_and_send_orders` after
    /// `setup`; return the bytes sent and the order errors raised.
    fn cancel_of(setup: impl FnOnce(&mut Context)) -> (usize, Vec<(i64, i64, String)>) {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();

        let mut context = order_context();
        context.market.register(265598);
        setup(&mut context);
        context.pending_orders.push(OrderRequest::Cancel { order_id: 5 });
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        drop(conn);

        let mut buf = Vec::new();
        let _ = server.read_to_end(&mut buf);
        (buf.len(), shared.orders.drain_order_errors())
    }

    // ibx#464: the reference answers a cancel of an unknown id with 10147
    // and sends nothing (captured 23/09/2026).
    #[test]
    fn cancel_of_an_unknown_order_is_refused_with_10147() {
        let (sent, errors) = cancel_of(|_| {});
        assert_eq!(sent, 0);
        assert_eq!(errors, [(5, 10147, "OrderId 5 that needs to be cancelled is not found.".to_string())]);
    }

    // A second cancel while the first is pending: 10148 with the state
    // (captured 23/09/2026: "state: PendingCancel").
    #[test]
    fn cancel_of_an_order_with_a_pending_cancel_is_refused_with_10148() {
        let (sent, errors) = cancel_of(|ctx| ctx.insert_order(order(5, 0, OrderStatus::PendingCancel)));
        assert_eq!(sent, 0);
        assert_eq!(errors, [(5, 10148,
            "OrderId 5 that needs to be cancelled cannot be cancelled, state: PendingCancel.".to_string())]);
    }

    // ibx#551, paper 11/10/2026 (market closed): the cancel of an order
    // that was cancelled, or that the server refused, is answered 10148
    // "state: Cancelled" while the reference keeps the order (seen at 3 s
    // and 8 s), and 10147 once it dropped it (seen at 12 s and 19 s).
    #[test]
    fn cancel_of_an_ended_order_finds_it_for_ten_seconds() {
        for status in [OrderStatus::Cancelled, OrderStatus::Rejected] {
            let (sent, errors) = cancel_of(|ctx| {
                ctx.insert_order(order(5, 0, OrderStatus::Submitted));
                ctx.finish_order(5, status);
            });
            assert_eq!(sent, 0);
            assert_eq!(errors, [(5, 10148,
                "OrderId 5 that needs to be cancelled cannot be cancelled, state: Cancelled.".to_string())], "{status:?}");

            let (sent, errors) = cancel_of(|ctx| {
                ctx.insert_order(order(5, 0, OrderStatus::Submitted));
                ctx.finish_order(5, status);
                // Its time is over.
                let kept = ctx.ended_orders(Instant::now());
                ctx.keep_ended(5, kept[0].server_id, kept[0].entry.clone(), Some(Instant::now()));
            });
            assert_eq!(sent, 0);
            assert_eq!(errors, [(5, 10147, "OrderId 5 that needs to be cancelled is not found.".to_string())], "{status:?}");
        }
    }

    #[test]
    fn cancel_of_a_finished_order_gives_its_state() {
        let (sent, errors) = cancel_of(|ctx| {
            ctx.insert_order(order(5, 10, OrderStatus::Filled));
            ctx.finish_order(5, OrderStatus::Filled);
        });
        assert_eq!(sent, 0);
        assert_eq!(errors[0].1, 10148);
        assert!(errors[0].2.ends_with("state: Filled."), "{}", errors[0].2);
    }

    // ibx#551, paper 12/10/2026 (regular hours, reference 10.51): a filled
    // order is answered 10148 "state: Filled" up to 24 s after its fill,
    // 10147 at 40 s.
    #[test]
    fn cancel_of_a_filled_order_finds_it_while_it_is_kept() {
        let (sent, errors) = cancel_of(|ctx| {
            ctx.insert_order(order(5, 10, OrderStatus::Filled));
            ctx.finish_order(5, OrderStatus::Filled);
            let kept = ctx.ended_orders(Instant::now());
            let left = kept[0].until.unwrap() - Instant::now();
            assert!(left > std::time::Duration::from_secs(29) && left <= std::time::Duration::from_secs(30), "{left:?}");
            // Its time is over.
            ctx.keep_ended(5, kept[0].server_id, kept[0].entry.clone(), Some(Instant::now()));
        });
        assert_eq!(sent, 0);
        assert_eq!(errors, [(5, 10147, "OrderId 5 that needs to be cancelled is not found.".to_string())]);
    }

    #[test]
    fn cancel_of_a_working_order_is_sent() {
        let (sent, errors) = cancel_of(|ctx| ctx.insert_order(order(5, 0, OrderStatus::Submitted)));
        assert!(sent > 0);
        assert!(errors.is_empty());
    }

    // ibx#464: the cancel the reference sends (captured 25/09/2026, account
    // masked): a cancel message with the new ClOrdID 258354710.2, the original
    // 258354710.1, quantity 1, sell, the account, conId 265598, source Socket
    // and scope SEL. ibx sent a `C{id}` ClOrdID, the original ClOrdID and the
    // transaction time only.
    #[test]
    fn cancel_is_written_like_the_reference() {
        let tags = wire_tags_with(
            |ctx| {
                ctx.insert_order(Order::new(5, 0, Side::Sell, 1, 0, b'2', b'0', 0));
                ctx.last_clord.insert(5, "5.1".to_string());
                ctx.modify_versions.insert(5, 1);
            },
            OrderRequest::Cancel { order_id: 5 },
        );
        let body: Vec<(u32, &str)> = tags.iter()
            .filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 10))
            .map(|(t, v)| (*t, v.as_str()))
            .collect();
        assert_eq!(body, [
            (35, "F"), (11, "5.2"), (41, "5.1"), (38, "1"), (54, "2"), (1, "DU1"),
            (6008, "265598"), (6088, "Socket"), (6944, "SEL"),
        ]);
    }

    #[test]
    fn cancel_right_after_the_place_uses_the_first_version() {
        let tags = wire_tags_with(
            |ctx| ctx.insert_order(Order::new(6, 0, Side::Buy, 2, 0, b'2', b'0', 0)),
            OrderRequest::Cancel { order_id: 6 },
        );
        assert_eq!(tag(&tags, 11), Some("6.1"));
        assert_eq!(tag(&tags, 41), Some("6.0"));
        assert_eq!(tag(&tags, 60), None, "the reference sends no TransactTime");
    }

    #[test]
    fn global_cancel_is_marked_all() {
        let tags = wire_tags_with(
            |ctx| ctx.insert_order(Order::new(7, 0, Side::Buy, 1, 0, b'2', b'0', 0)),
            OrderRequest::CancelAll { instrument: 0 },
        );
        assert_eq!(tag(&tags, 11), Some("7.1"));
        assert_eq!(tag(&tags, 6944), Some("ALL"));
    }

    /// One global cancel through `drain_and_send_orders` after `setup`:
    /// each frame sent (its fields), the order errors and the order
    /// updates raised.
    #[allow(clippy::type_complexity)]
    fn global_cancel_of(setup: impl FnOnce(&mut Context)) -> (Vec<Vec<(u32, String)>>, Vec<(i64, i64, String)>, Vec<OrderUpdate>) {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        context.api_client_id = 7;
        setup(&mut context);
        context.pending_orders.push(OrderRequest::GlobalCancel);
        let shared = Arc::new(SharedState::new());
        shared.reference.set_api_client_id(7);
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let frames = peer.messages().iter().map(|m| crate::test_support::parse_fields(m)).collect();
        (frames, shared.orders.drain_order_errors(), shared.orders.drain_order_updates())
    }

    /// An order of another session in the book, as the logon replay puts
    /// it: under `key`, its server id `server`, its ClOrdID version and
    /// API client.
    fn replayed(ctx: &mut Context, key: OrderId, server: OrderId, version: u32, owner: i64) {
        replayed_on(ctx, key, server, version, owner, 0);
    }

    fn replayed_on(ctx: &mut Context, key: OrderId, server: OrderId, version: u32, owner: i64, instrument: u32) {
        ctx.insert_order(Order { status: OrderStatus::PreSubmitted, ..Order::new(key, instrument, Side::Buy, 1, 100, b'2', b'0', 0) });
        ctx.bind_server_id(key, server);
        ctx.modify_versions.insert(key, version);
        ctx.last_clord.insert(key, format!("{server}.{version}"));
        ctx.book.get_mut(&key).unwrap().owner = Some(owner);
    }

    fn sent_ids(frames: &[Vec<(u32, String)>]) -> Vec<String> {
        frames.iter().filter(|f| tag(f, 35) == Some("F")).map(|f| tag(f, 11).unwrap().to_string()).collect()
    }

    // The global cancel reaches every order of the book: this session's,
    // and those of other clients and earlier sessions, on any contract
    // (no client filter). Paper
    // 04/10/2026: orders of earlier sessions got nothing.
    #[test]
    fn global_cancel_reaches_every_order_of_the_book() {
        let (frames, errors, _) = global_cancel_of(|ctx| {
            let other = ctx.market.register(756733);
            ctx.insert_order(Order::new(5, 0, Side::Buy, 1, 100, b'2', b'0', 0));
            replayed(ctx, 1790862363895062, 1790862363895062, 0, 0);
            replayed_on(ctx, 15, 1183455398, 2, 99, other);
        });
        assert!(errors.is_empty());
        let mut ids = sent_ids(&frames);
        ids.sort();
        assert_eq!(ids, ["1183455398.3", "1790862363895062.1", "5.1"]);
        assert!(frames.iter().all(|f| tag(f, 6944) == Some("ALL") && tag(f, 6088) == Some("Socket")));
    }

    // The book's order: a hash table keyed by permId, walked bucket by
    // bucket, each in the order the orders came. Captured 01/10/2026: the
    // logon replay gave 8 orders in this order, the global cancel sent
    // them in the order below.
    #[test]
    fn global_cancel_goes_in_the_books_order() {
        let replay = [1790862363895062, 1790862423941063, 1790863660514062, 1790863720603063,
            1790865682753062, 1790865742870063, 1790866970032000, 1790867030123001];
        let (frames, _, _) = global_cancel_of(|ctx| {
            for id in replay { replayed(ctx, id, id, 0, 0); }
        });
        let captured = [1790865742870063i64, 1790865682753062, 1790862423941063, 1790866970032000,
            1790863660514062, 1790867030123001, 1790862363895062, 1790863720603063];
        assert_eq!(sent_ids(&frames), captured.map(|id| format!("{id}.1")));
        let orig: Vec<&str> = frames.iter().filter_map(|f| tag(f, 41)).collect();
        assert_eq!(orig, captured.map(|id| format!("{id}.0")));
    }

    #[test]
    fn the_books_table_grows_as_a_java_hash_map() {
        assert_eq!(book_table_size(0), 16);
        assert_eq!(book_table_size(12), 16);
        assert_eq!(book_table_size(13), 32);
        assert_eq!(book_table_size(25), 64);
        // Long.hashCode, spread: (int)(v ^ v >>> 32), then h ^ h >>> 16.
        assert_eq!(book_bucket(1790865742870063, 16), 0);
        assert_eq!(book_bucket(1790865682753062, 64), 52);
    }

    // Only the first order of an OCA group gets the cancel; the server
    // cancels the group.
    #[test]
    fn global_cancel_sends_one_order_of_an_oca_group() {
        let (frames, _, _) = global_cancel_of(|ctx| {
            for id in [21, 22, 23] { ctx.insert_order(Order::new(id, 0, Side::Buy, 1, 100, b'2', b'0', 0)); }
            ctx.set_links(21, None, Some("G1"));
            ctx.set_links(22, None, Some("G1"));
        });
        let mut ids = sent_ids(&frames);
        ids.sort();
        assert_eq!(ids, ["21.1", "23.1"], "22 is in the group of 21, which comes first in the book");
    }

    // A child whose parent goes in the same cancel is left to the server
    //; the parents go first. A child
    // whose parent is not in the book gets its own cancel.
    #[test]
    fn global_cancel_leaves_a_child_to_its_parent() {
        let (frames, _, _) = global_cancel_of(|ctx| {
            for id in [31, 32, 33, 34] { ctx.insert_order(Order::new(id, 0, Side::Buy, 1, 100, b'2', b'0', 0)); }
            ctx.set_links(32, Some(31), Some("31"));
            ctx.set_links(33, Some(31), Some("31"));
            ctx.set_links(34, Some(999), None);
        });
        let ids = sent_ids(&frames);
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&"31.1".to_string()) && ids.contains(&"34.1".to_string()));
        assert_eq!(ids.last().map(String::as_str), Some("34.1"), "an order with a parent comes last");
    }

    // A bracket of this session: its children carry their parent and OCA
    // group as sent, so the global cancel sends the parent's cancel only.
    #[test]
    fn global_cancel_of_a_bracket_cancels_its_parent() {
        let (frames, _, _) = global_cancel_of(|ctx| {
            let shared = Arc::new(SharedState::new());
            let (conn, _peer) = crate::test_support::Peer::pair();
            let mut conn = Some(conn);
            ctx.pending_orders.push(OrderRequest::SubmitBracket {
                parent_id: 51, tp_id: 52, sl_id: 53, instrument: 0, side: Side::Buy, qty: 1,
                entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
            });
            drain_and_send_orders(&mut conn, ctx, "DU1", &mut HeartbeatState::new(), false, &shared);
            // The OCA group is the parent's server id, as the reference's
            // permId.
            let child = ctx.book.get(&52).cloned().unwrap();
            assert_eq!((child.parent, child.oca_group), (51, FIRST.to_string()));
        });
        assert_eq!(sent_ids(&frames), [format!("{FIRST}.1")]);
    }

    // The cancel of an order that waits to be sent: ApiCancelled, nothing
    // on the wire, its other requests dropped. A
    // combo refused with 200 stays pending: its cancel and a global cancel
    // end it as ApiCancelled too (i105_combo_directed, 26/09/2026).
    #[test]
    fn cancel_of_an_order_that_never_left_is_api_cancelled() {
        let mut context = order_context();
        context.market.register(265598);
        context.rth_parked.push(OrderRequest::SubmitLimit { order_id: 9, instrument: 0, side: Side::Buy, qty: 2, price: 100 });
        context.rth_parked.push(OrderRequest::Modify {
            new_order_id: 9, order_id: 9, qty: 3, kind: crate::types::OrderKind::Limit { price: 100 },
            tif: b'0', attrs: Default::default(),
        });
        context.api_pending.insert(31, OrderRequest::SubmitLimit { order_id: 31, instrument: 0, side: Side::Buy, qty: 1, price: 100 });
        context.api_pending.insert(32, OrderRequest::SubmitLimit { order_id: 32, instrument: 0, side: Side::Buy, qty: 1, price: 100 });
        context.pending_orders.push(OrderRequest::Cancel { order_id: 9 });
        context.pending_orders.push(OrderRequest::Cancel { order_id: 31 });
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        assert!(peer.messages().is_empty());
        assert!(shared.orders.drain_order_errors().is_empty());
        let ended: Vec<(OrderId, OrderStatus, i64)> = shared.orders.drain_order_updates().iter()
            .map(|u| (u.order_id, u.status, u.remaining_qty_fixed)).collect();
        assert_eq!(ended, [(9, OrderStatus::ApiCancelled, 2 * crate::types::QTY_SCALE),
            (31, OrderStatus::ApiCancelled, crate::types::QTY_SCALE)]);
        assert!(context.rth_parked.is_empty(), "the replace of 9 goes with it");

        context.pending_orders.push(OrderRequest::GlobalCancel);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let ended: Vec<OrderId> = shared.orders.drain_order_updates().iter().map(|u| u.order_id).collect();
        assert_eq!(ended, [32]);
        assert!(context.api_pending.is_empty());
    }

    // An order whose cancel is pending is not sent again: 161 at its
    // client, with the order's permId (the text as captured 01/10/2026 in
    // pd-orders). An order of another client gives nothing to this one.
    #[test]
    fn global_cancel_refuses_an_order_with_a_pending_cancel() {
        let (frames, errors, _) = global_cancel_of(|ctx| {
            ctx.insert_order(order(41, 0, OrderStatus::PendingCancel));
            replayed(ctx, 42, 42, 1, 3);
            ctx.set_order_status_forced(42, OrderStatus::PendingCancel);
            replayed(ctx, 43, 7043, 0, 7);
            ctx.set_order_status_forced(43, OrderStatus::PendingCancel);
        });
        assert!(sent_ids(&frames).is_empty());
        let mut errors = errors;
        errors.sort();
        assert_eq!(errors, [
            (41, 161, "Cancel attempted when order is not in a cancellable state.  Order permId =41".to_string()),
            (43, 161, "Cancel attempted when order is not in a cancellable state.  Order permId =7043".to_string()),
        ]);
    }

    // The orders of this client that wait to be sent end as ApiCancelled,
    // nothing on the wire (captured 25/09/2026: permId
    // 0, the whole quantity remaining); the requests about them go too.
    #[test]
    fn global_cancel_ends_the_orders_that_wait() {
        let (frames, errors, updates) = global_cancel_of(|ctx| {
            ctx.rth_parked.push(OrderRequest::SubmitLimit { order_id: 9, instrument: 0, side: Side::Buy, qty: 3, price: 100 });
            ctx.rth_parked.push(OrderRequest::Cancel { order_id: 9 });
            ctx.rth_parked.push(OrderRequest::Modify {
                new_order_id: 5, order_id: 5, qty: 1, kind: crate::types::OrderKind::Limit { price: 100 },
                tif: b'0', attrs: Default::default(),
            });
        });
        assert!(frames.is_empty() && errors.is_empty());
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        assert_eq!((u.order_id, u.status, u.filled_qty_fixed, u.remaining_qty_fixed, u.perm_id),
            (9, OrderStatus::ApiCancelled, 0, 3 * crate::types::QTY_SCALE, 0));
    }

    /// The 161 notices of one global cancel after `setup`, and the
    /// context after it.
    fn global_cancel_notices(setup: impl FnOnce(&mut Context)) -> (Vec<(i64, i64)>, Context) {
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        context.api_client_id = 7;
        setup(&mut context);
        context.pending_orders.push(OrderRequest::GlobalCancel);
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        assert!(peer.messages().is_empty(), "nothing is sent for an ended order");
        assert!(shared.orders.drain_order_errors().is_empty());
        (shared.orders.drain_order_notices().into_iter().map(|n| (n.0, n.1)).collect(), context)
    }

    fn ended(parent: OrderId, oca_group: &str) -> crate::engine::context::BookEntry {
        crate::engine::context::BookEntry { parent, oca_group: oca_group.to_string(), ..Default::default() }
    }

    // ibx#545, paper 09/10/2026: a global cancel answers 161 for a
    // cancelled order the reference's book still holds (seen 6 and 7 s
    // after the cancel, not 17 s after), with the order's permId.
    #[test]
    fn global_cancel_answers_161_for_an_order_cancelled_just_before() {
        let (notices, mut context) = global_cancel_notices(|ctx| {
            ctx.insert_order(Order::new(61, 0, Side::Buy, 1, 100, b'2', b'0', 0));
            ctx.bind_server_id(61, 7061);
            ctx.finish_order(61, OrderStatus::Cancelled);
        });
        assert_eq!(notices, [(61, 161)]);
        let kept = context.ended_orders(Instant::now());
        assert_eq!(kept.iter().map(|e| (e.order_id, e.server_id)).collect::<Vec<_>>(), [(61, 7061)]);
        let until = kept[0].until.unwrap();
        assert!(until <= Instant::now() + crate::engine::context::CANCELLED_ORDER_KEPT);
        // Its time over, it is out of the book: nothing.
        assert!(context.ended_orders(until).is_empty());
        let (notices, _) = global_cancel_notices(|ctx| {
            ctx.keep_ended(61, 7061, ended(0, ""), Some(Instant::now()));
        });
        assert!(notices.is_empty());
    }

    // ibx#595, the reference 10.51 on paper, 15/10/2026, market closed:
    // three cases in one session, orders held with transmit off on AAPL.
    // The held orders are in the book under their permIds and are walked
    // with the others; the first order of an OCA group met in the walk
    // stands for its group, so the other one stays held (N: the first
    // placed; AE: the second placed, met after the fourth).
    #[test]
    fn global_cancel_walks_the_held_orders_in_the_book() {
        fn cancel(context: &mut Context) -> Vec<(i64, i64)> {
            context.pending_orders.push(OrderRequest::GlobalCancel);
            let shared = Arc::new(SharedState::new());
            let (conn, mut peer) = crate::test_support::Peer::pair();
            drain_and_send_orders(&mut Some(conn), context, "DU1", &mut HeartbeatState::new(), false, &shared);
            assert!(peer.messages().is_empty());
            shared.orders.drain_order_notices().into_iter().map(|n| (n.0, n.1)).collect()
        }
        fn hold(context: &mut Context, first: OrderId, perm: OrderId, n: i64, group: &[i64]) {
            for k in 0..n {
                let id = first + k;
                context.unsent.insert(id, (0, crate::types::QTY_SCALE, 0));
                context.bind_server_id(id, perm + k);
                let oca = if group.contains(&(k + 1)) { format!("c551-{first}") } else { String::new() };
                context.hold_entry(id, 0, &oca);
            }
        }
        let ended = |ids: &[i64]| -> Vec<(i64, i64)> { ids.iter().flat_map(|&i| [(i, 202), (i, 161)]).collect() };
        let again = |ids: &[i64]| -> Vec<(i64, i64)> { ids.iter().map(|&i| (i, 161)).collect() };
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.api_client_id = 7;

        // N: four held, the first two in one group.
        hold(&mut context, 57714, 976194598, 4, &[1, 2]);
        assert_eq!(cancel(&mut context), ended(&[57717, 57716, 57715]));
        assert!(context.unsent.contains_key(&57714), "the first of the group stays held");
        assert_eq!(cancel(&mut context), again(&[57717, 57716, 57715]));

        // AD: six held alone.
        hold(&mut context, 57718, 976194602, 6, &[]);
        let mut want = ended(&[57723, 57722, 57721, 57720, 57719, 57718]);
        want.extend(again(&[57717, 57716, 57715]));
        assert_eq!(cancel(&mut context), want);
        assert_eq!(cancel(&mut context), again(&[57723, 57722, 57721, 57720, 57719, 57718, 57717, 57716, 57715]));

        // AE: five held, the second and the fourth in one group; the book
        // has grown to 32 places.
        hold(&mut context, 57724, 976194608, 5, &[2, 4]);
        let mut want = again(&[57723, 57722, 57721, 57720, 57719, 57718, 57717, 57716, 57715]);
        want.extend(ended(&[57728, 57727, 57726, 57724]));
        assert_eq!(cancel(&mut context), want);
        assert!(context.unsent.contains_key(&57725), "the second placed stays held");
        assert_eq!(cancel(&mut context),
            again(&[57723, 57722, 57721, 57720, 57719, 57718, 57717, 57716, 57715, 57728, 57727, 57726, 57724]));
        assert!(context.unsent.contains_key(&57714) && context.unsent.contains_key(&57725));
    }

    // ibx#545, the reference's run of 09/10/2026 (cases of ibx#509): a
    // held order ended by the global cancel, then 161 for the ended
    // orders of the book in the book's order, a child last. 37347 had
    // been cancelled 7 s before; the three others were never sent and
    // got 161 again at the next global cancel.
    #[test]
    fn global_cancel_answers_161_in_the_books_order() {
        let setup = |ctx: &mut Context| {
            ctx.keep_ended(37333, 1219931939, ended(0, ""), None);
            ctx.keep_ended(37334, 1219931940, ended(37333, ""), None);
            ctx.keep_ended(37342, 1219931948, ended(0, ""), None);
            ctx.keep_ended(37347, 1219931953, ended(0, ""), Some(Instant::now() + std::time::Duration::from_secs(3)));
        };
        let (notices, mut context) = global_cancel_notices(|ctx| {
            setup(ctx);
            ctx.unsent.insert(37348, (0, crate::types::QTY_SCALE, 0));
            ctx.bind_server_id(37348, 1219931954);
        });
        assert_eq!(notices, [(37348, 202), (37348, 161), (37333, 161), (37347, 161), (37342, 161), (37334, 161)]);
        // The next one: the order that was never sent is still there.
        context.ended.retain(|e| e.order_id != 37347);
        let kept = context.ended.clone();
        let (notices, _) = global_cancel_notices(|ctx| ctx.ended = kept);
        assert_eq!(notices, [(37348, 161), (37333, 161), (37342, 161), (37334, 161)]);
    }

    // ibx#545, the reference's run of 09/10/2026 (cases of ibx#547): a
    // bracket cancelled through its parent 5 s before. 161 for the parent
    // and for the first child of the book; the other child is of the same
    // OCA group. An ended parent does not stand for its children.
    #[test]
    fn global_cancel_answers_161_for_one_child_of_an_ended_bracket() {
        let (notices, _) = global_cancel_notices(|ctx| {
            let soon = Some(Instant::now() + std::time::Duration::from_secs(5));
            ctx.keep_ended(39086, 771315648, ended(0, ""), None);
            ctx.keep_ended(39092, 771315653, ended(0, ""), soon);
            ctx.keep_ended(39093, 771315654, ended(39092, "771315653"), soon);
            ctx.keep_ended(39094, 771315655, ended(39092, "771315653"), soon);
        });
        assert_eq!(notices, [(39086, 161), (39092, 161), (39094, 161)]);
    }

    // ibx#549, paper 10/10/2026: an order the server refused is answered
    // for in its 10 s too. A global cancel does not take it out (ibx#551,
    // paper 11/10/2026: 161 at 3 s and again at 6 s after the refusal).
    #[test]
    fn global_cancel_answers_161_for_a_refused_order_while_it_is_kept() {
        let (notices, mut context) = global_cancel_notices(|ctx| {
            ctx.insert_order(Order::new(81, 0, Side::Buy, 1, 100, b'2', b'0', 0));
            ctx.finish_order(81, OrderStatus::Rejected);
        });
        assert_eq!(notices, [(81, 161)]);
        let kept = context.ended_orders(Instant::now());
        assert_eq!(kept.len(), 1, "still there for the next global cancel");
        assert!(context.ended_orders(kept[0].until.unwrap()).is_empty());
    }

    // ibx#551, paper 12/10/2026 (regular hours, reference 10.51): 161 for
    // a filled order at 3, 8, 12 and 20 s after its fill, none at 35 s.
    #[test]
    fn global_cancel_answers_161_for_a_filled_order_while_it_is_kept() {
        let (notices, mut context) = global_cancel_notices(|ctx| {
            ctx.insert_order(Order::new(81, 0, Side::Buy, 1, 100, b'2', b'0', 0));
            ctx.finish_order(81, OrderStatus::Filled);
        });
        assert_eq!(notices, [(81, 161)]);
        let kept = context.ended_orders(Instant::now());
        assert_eq!(kept.len(), 1, "still there for the next global cancel");
        assert!(context.ended_orders(kept[0].until.unwrap()).is_empty());
    }

    // An ended order of another client gives nothing to this one.
    #[test]
    fn global_cancel_gives_no_161_for_an_ended_order_of_another_client() {
        let (notices, _) = global_cancel_notices(|ctx| {
            let entry = crate::engine::context::BookEntry { owner: Some(3), ..Default::default() };
            ctx.keep_ended(71, 7071, entry, None);
        });
        assert!(notices.is_empty());
    }

    // An order of another session kept under its API order id: its
    // cancel goes under the server's id, at the version the server gave
    //.
    #[test]
    fn a_replayed_order_is_cancelled_under_its_server_id() {
        let tags = wire_tags_with(
            |ctx| { ctx.api_client_id = 0; replayed(ctx, 15, 1183455398, 2, 0); },
            OrderRequest::Cancel { order_id: 15 },
        );
        assert_eq!(tag(&tags, 11), Some("1183455398.3"));
        assert_eq!(tag(&tags, 41), Some("1183455398.2"));
        assert_eq!(tag(&tags, 6944), Some("SEL"));
    }

    // The reference finds an API order by the client's id and the order
    // id: an order of another client is not found
    // (10147, ib-agent#151) and nothing is sent.
    #[test]
    fn cancel_of_an_order_of_another_client_is_refused_with_10147() {
        let (sent, errors) = cancel_of(|ctx| replayed(ctx, 5, 5, 0, 250));
        assert_eq!(sent, 0);
        assert_eq!(errors, [(5, 10147, "OrderId 5 that needs to be cancelled is not found.".to_string())]);
        let (sent, errors) = cancel_of(|ctx| replayed(ctx, 5, 5, 0, 0));
        assert!(sent > 0, "an order of this client's id (0) is found");
        assert!(errors.is_empty());
    }

    // ibx#466: the orderRef rides its own field on the order and on each replace, as
    // the reference (captured 25/09/2026: the orderRef pm0925-fill-BUY, just
    // before the origin field on the replace).
    #[test]
    fn order_ref_is_sent_on_the_order_and_the_replace() {
        let attrs = crate::types::OrderAttrs { order_ref: "pm0925-fill-BUY".into(), ..Default::default() };
        let tags = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: 7, instrument: 0, side: Side::Buy, qty: 100, price: 337 * P, tif: b'0', attrs: attrs.clone(),
        });
        assert_eq!(tag(&tags, 6010), Some("pm0925-fill-BUY"));

        let replace = replace_fields(7, Side::Buy, 100, crate::types::OrderKind::Limit { price: 338 * P }, b'0', attrs);
        assert!(pos(&replace, 6010) + 1 == pos(&replace, 6122), "6010 just before 6122=c");
    }

    #[test]
    fn the_currency_is_the_contracts() {
        let usd = wire_tags(OrderRequest::SubmitLimit { order_id: 8, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P });
        assert_eq!(tag(&usd, 15), Some("USD"), "USD when unknown");
        let eur = wire_tags_with(|ctx| ctx.market.set_currency(0, "EUR"),
            OrderRequest::SubmitLimit { order_id: 9, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P });
        assert_eq!(tag(&eur, 15), Some("EUR"));
        let eur_ex = wire_tags_with(|ctx| ctx.market.set_currency(0, "EUR"), OrderRequest::SubmitEx {
            order_id: 10, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P }, tif: b'0',
            attrs: crate::types::OrderAttrs { hidden: true, ..Default::default() },
        });
        assert_eq!(tag(&eur_ex, 15), Some("EUR"));
    }

    // ibx#466: the reference writes the stop trigger with the stop
    // price on STP, STP LMT and STP PRT (captured 25/09/2026:
    // stop price 235.14 ... stop trigger 235.14).
    #[test]
    fn stop_orders_carry_the_stop_trigger() {
        let plain = wire_tags(OrderRequest::SubmitStop { order_id: 11, instrument: 0, side: Side::Sell, qty: 1, stop_price: 23514 * P / 100 });
        assert_eq!(tag(&plain, 99), Some("235.14"));
        assert_eq!(tag(&plain, 6117), Some("235.14"));
        let stop_limit = wire_tags(OrderRequest::SubmitStopLimit {
            order_id: 12, instrument: 0, side: Side::Sell, qty: 1, price: 234 * P, stop_price: 235 * P,
        });
        assert_eq!(tag(&stop_limit, 6117), tag(&stop_limit, 99));
        let ex = wire_tags(OrderRequest::SubmitEx {
            order_id: 13, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::Stop { stop_price: 23514 * P / 100 }, tif: b'1',
            attrs: crate::types::OrderAttrs { outside_rth: true, ..Default::default() },
        });
        assert_eq!(tag(&ex, 6117), Some("235.14"));
        let mit = wire_tags(OrderRequest::SubmitMit { order_id: 14, instrument: 0, side: Side::Sell, qty: 1, stop_price: 235 * P });
        // MIT writes its stop price in the stop trigger field too, as its touched trigger
        // among the attributes (ib-agent#199, ibx#263).
        assert_eq!(tag(&mit, 6117), Some("235.00"));
    }

    // ibx#688, the reference 10.51 on paper, 18/10/2026, market closed: an
    // offset of 0 is sent; the stop types write it in their stop price and
    // stop trigger, MIT and LIT write neither, RPI writes it in its offset.
    #[test]
    fn zero_offset_orders_are_written_as_the_reference() {
        use crate::types::OrderKind as K;
        let ex = |kind| wire_tags(OrderRequest::SubmitEx {
            order_id: 15, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'0', attrs: Default::default(),
        });
        for kind in [K::Stop { stop_price: 0 }, K::StopLimit { price: 100 * P, stop_price: 0 }, K::StpPrt { stop_price: 0 }] {
            let tags = ex(kind);
            assert_eq!((tag(&tags, 99), tag(&tags, 6117)), (Some("0.00"), Some("0.00")), "{kind:?}");
        }
        for kind in [K::Mit { stop_price: 0 }, K::Lit { price: 100 * P, stop_price: 0 }] {
            let tags = ex(kind);
            assert_eq!((tag(&tags, 99), tag(&tags, 6117)), (None, None), "{kind:?}");
            assert_eq!(tag(&tags, 6115), Some("0"), "{kind:?}");
        }
        let rpi = ex(K::Rpi { price: 100 * P, offset: 0 });
        assert_eq!((tag(&rpi, 44), tag(&rpi, 99), tag(&rpi, 211)), (Some("100.00"), Some("0.00"), Some("0.00")));
    }

    // ib-agent#194, captured 25/09/2026: a TRAIL LIMIT with lmtPrice only
    // sends the limit price and no limit offset: a new order with the limit
    // price 745.66, stop price 20.00, stop trigger 750.66, type TSL, peg offset 20.00.
    #[test]
    fn trail_limit_with_a_limit_price_sends_44_and_no_offset() {
        let plain = wire_tags(OrderRequest::SubmitTrailingStopLimit {
            order_id: 20, instrument: 0, side: Side::Sell, qty: 1, lmt_offset: 0,
            lmt_price: Some(74566 * P / 100), trail_amt: 20 * P, trail_stop_price: 75066 * P / 100,
        });
        assert_eq!(tag(&plain, 44), Some("745.66"));
        assert_eq!(tag(&plain, 6370), None);
        assert_eq!(tag(&plain, 40), Some("TSL"));
        assert_eq!(tag(&plain, 6117), Some("750.66"));

        let ex = wire_tags(OrderRequest::SubmitEx {
            order_id: 21, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::TrailingStopLimit {
                lmt_offset: 0, lmt_price: Some(74566 * P / 100), trail_amt: 20 * P, trail_stop_price: 75066 * P / 100,
            },
            tif: b'1', attrs: crate::types::OrderAttrs { outside_rth: true, ..Default::default() },
        });
        assert_eq!(tag(&ex, 44), Some("745.66"));
        assert_eq!(tag(&ex, 6370), None);

        // The offset form keeps the limit offset and no limit price.
        let offset = wire_tags(OrderRequest::SubmitTrailingStopLimit {
            order_id: 22, instrument: 0, side: Side::Sell, qty: 1, lmt_offset: P / 2,
            lmt_price: None, trail_amt: 20 * P, trail_stop_price: 75066 * P / 100,
        });
        assert_eq!(tag(&offset, 6370), Some("0.50"));
        assert_eq!(tag(&offset, 44), None);
    }

    // ib-agent#194: the replace restates the limit price and the offset the server
    // reported (a replace with limit price 745.76, stop price 20.00, limit offset 5.00).
    #[test]
    fn trail_limit_replace_restates_the_price_and_the_server_offset() {
        let kind = crate::types::OrderKind::TrailingStopLimit {
            lmt_offset: 0, lmt_price: Some(74576 * P / 100), trail_amt: 20 * P, trail_stop_price: 0,
        };
        let tags = wire_tags_with(
            |ctx| {
                ctx.set_symbol(0, "SPY".to_string());
                ctx.insert_order(Order::new(23, 0, Side::Sell, 1, 0, b'P', b'0', 0));
                ctx.trail_limit_reported.insert(23, crate::engine::context::TrailLimitReported {
                    offset: 5 * P, limit: 74566 * P / 100, stop: 75066 * P / 100,
                });
            },
            OrderRequest::Modify { new_order_id: 23, order_id: 23, qty: 1, kind, tif: b'0', attrs: Default::default() },
        );
        assert_eq!(tag(&tags, 44), Some("745.76"));
        assert_eq!(tag(&tags, 6370), Some("5.00"), "the reference writes two decimals at least");
        assert!(pos(&tags, 44) < pos(&tags, 99));
    }

    // ibx#467: DTC goes out as GTC with the DTC flag right after it, on the
    // new order and on the replace, with no expiry.
    #[test]
    fn dtc_is_gtc_with_the_dtc_flag() {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 30, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P },
            tif: crate::types::TIF_DTC, attrs: Default::default(),
        });
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6436), Some("1"));
        assert_eq!(pos(&tags, 6436), pos(&tags, 59) + 1);
        assert_eq!(tag(&tags, 126), None);
        assert_eq!(tag(&tags, 432), None);

        let replace = wire_tags_with(
            |ctx| {
                ctx.set_symbol(0, "SPY".to_string());
                ctx.insert_order(Order::new(31, 0, Side::Buy, 1, 100 * P, b'2', crate::types::TIF_DTC, 0));
            },
            OrderRequest::Modify {
                new_order_id: 31, order_id: 31, qty: 1,
                kind: crate::types::OrderKind::Limit { price: 101 * P },
                tif: crate::types::TIF_DTC, attrs: Default::default(),
            },
        );
        assert_eq!(tag(&replace, 59), Some("1"));
        assert_eq!(tag(&replace, 6436), Some("1"));

        // Other time-in-force values have no flag.
        let gtc = wire_tags(OrderRequest::SubmitEx {
            order_id: 32, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P },
            tif: b'1', attrs: Default::default(),
        });
        assert_eq!(tag(&gtc, 6436), None);
    }

    // ibx#467: goodAfterTime is written in UTC with a dash.
    #[test]
    fn good_after_time_is_utc_with_a_dash() {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 33, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P },
            tif: b'0', attrs: crate::types::OrderAttrs { good_after: 1_798_641_000, ..Default::default() },
        });
        assert_eq!(tag(&tags, 168), Some("20261230-14:30:00"));
    }

    // ib-agent#195 (captured 25/09/2026): a replace sent before any server
    // report computes the offset from the stop price and the new limit
    // price: a replace with limit price 746.99 and limit offset 4.90 for a sell with
    // stop trigger 751.89. For a buy it is the limit price minus the stop price.
    #[test]
    fn trail_limit_replace_before_any_report_computes_the_offset() {
        let replace = |side: Side, lmt: i64, stop: i64| {
            let kind = crate::types::OrderKind::TrailingStopLimit {
                lmt_offset: 0, lmt_price: Some(lmt), trail_amt: 20 * P, trail_stop_price: stop,
            };
            wire_tags_with(
                |ctx| {
                    ctx.set_symbol(0, "SPY".to_string());
                    ctx.insert_order(Order::new(40, 0, side, 1, 0, b'P', b'0', 0));
                },
                OrderRequest::Modify { new_order_id: 40, order_id: 40, qty: 1, kind, tif: b'0', attrs: Default::default() },
            )
        };
        let sell = replace(Side::Sell, 74699 * P / 100, 75189 * P / 100);
        assert_eq!(tag(&sell, 44), Some("746.99"));
        assert_eq!(tag(&sell, 6370), Some("4.90"));
        let buy = replace(Side::Buy, 79699 * P / 100, 79189 * P / 100);
        assert_eq!(tag(&buy, 6370), Some("5.10"));
    }

    /// Every frame the engine sends for the queued requests, split per frame.
    fn drain_frames(context: &mut Context, shared: &Arc<SharedState>, conn: &mut Option<Connection>,
                    server: &mut crate::protocol::connection::MemTransport) -> Vec<Vec<(u32, String)>> {
        use std::io::Read;
        drain_and_send_orders(conn, context, "DU1", &mut HeartbeatState::new(), false, shared);
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 16384];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 { break; }
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut frames: Vec<Vec<(u32, String)>> = Vec::new();
        for field in bytes.split(|&b| b == fix::SOH) {
            let Ok(text) = std::str::from_utf8(field) else { continue };
            let Some((t, v)) = text.split_once('=') else { continue };
            let Ok(t) = t.parse::<u32>() else { continue };
            if t == 8 { frames.push(Vec::new()); }
            // The definition by conId on an exchange goes out under the
            // session's number (ibx#706); its answer comes back under ibx's
            // own, as the connection maps it.
            let v = match (t, conn.as_ref()) {
                (320, Some(c)) if v.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME) || v.starts_with("ClearingRule")
                    || v.starts_with(crate::control::contracts::SECDEF_MSG_NAME) || v.starts_with(crate::control::contracts::SECDEF_UNDERLYING_NAME)
                    || v.starts_with(crate::control::contracts::SECDEF_PREFERRED_NAME) => c.internal_name(v),
                _ => v.to_string(),
            };
            if let Some(f) = frames.last_mut() { f.push((t, v)); }
        }
        frames
    }

    // ibx#465 (ib-agent#199): a request with outside RTH waits for the
    // definition of its exchange, asked once; then outside RTH follows the
    // reference's rule: a STP on a US stock loses it with warning 2109, a
    // LMT keeps it, and a replace drops it without a second warning.
    #[test]
    fn outside_rth_waits_for_the_definition_then_follows_the_rule() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        let rth = crate::types::OrderAttrs { outside_rth: true, ..Default::default() };
        let stop = crate::types::OrderKind::Stop { stop_price: 200 * P };
        context.pending_orders.push(OrderRequest::SubmitEx {
            order_id: 50, instrument: 0, side: Side::Sell, qty: 1, kind: stop, tif: b'1', attrs: rth.clone() });
        context.pending_orders.push(OrderRequest::SubmitEx {
            order_id: 51, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P }, tif: b'1', attrs: rth.clone() });

        // One definition request, nothing sent for the orders yet.
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(tag(&frames[0], 35), Some("c"));
        assert_eq!(tag(&frames[0], 6008), Some("265598"));
        assert_eq!(tag(&frames[0], 6004), Some("BEST"));
        assert_eq!(tag(&frames[0], 146), Some("1"));
        let id = tag(&frames[0], 320).unwrap().to_string();
        assert_eq!(context.rth_parked.len(), 2);

        // The reply (AAPL on BEST, paper 28/09/2026) releases them.
        let reply = fix::fix_build(&[(35, "d"), (320, &id), (6008, "265598"), (55, "AAPL"), (167, "STK"),
            (15, "USD"), (207, "BEST"), (6523, "USSTK"), (6431, "ACTIVETIM/1,AD/5,RTH/1,AON/1")], 1);
        assert!(rth_definition_reply(&mut context, &id, &reply));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(tag(&frames[0], 6121), Some("50"));
        assert_eq!(tag(&frames[0], 6433), None, "STP on a US stock: no outside RTH");
        assert_eq!(tag(&frames[1], 6121), Some("51"));
        assert_eq!(tag(&frames[1], 6433), Some("1"), "LMT keeps it");
        let errors = shared.orders.drain_order_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!((errors[0].0, errors[0].1), (50, 2109));
        assert!(errors[0].2.starts_with("Order Event Warning:Attribute 'Outside Regular Trading Hours' is ignored"));

        // The replace of the STP has no outside RTH and no second 2109.
        context.pending_orders.push(OrderRequest::Modify {
            new_order_id: 50, order_id: 50, qty: 1, kind: crate::types::OrderKind::Stop { stop_price: 199 * P },
            tif: b'1', attrs: rth.clone() });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(tag(&frames[0], 35), Some("G"));
        assert_eq!(tag(&frames[0], 6433), None);
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ibx#465: no definition in time: the empty list, as the reference when
    // it finds none, so outside RTH is dropped with 2109.
    #[test]
    fn outside_rth_without_a_definition_uses_the_empty_list() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.pending_orders.push(OrderRequest::SubmitLimitEx {
            order_id: 60, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0',
            attrs: crate::types::OrderAttrs { outside_rth: true, ..Default::default() } });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 35), Some("c"));
        for lookup in context.rth_lookups.iter_mut() { lookup.2 = std::time::Instant::now(); }
        sweep_rth_lookups(&mut context);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1);
        assert_eq!(tag(&frames[0], 6433), None);
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(60, 2109)]);
    }

    // ibx#465: an order without outside RTH does not wait.
    #[test]
    fn an_order_without_outside_rth_does_not_wait() {
        let tags = wire_tags_with(|ctx| { ctx.rth_types.clear(); }, OrderRequest::SubmitLimitEx {
            order_id: 61, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0',
            attrs: Default::default() });
        assert_eq!(tag(&tags, 35), Some("D"));
    }

    // ibx#607: the AAPL list on BEST (paper 12/10/2026) has PREOPGRTH,
    // POSTONLY and DEACT, not RTHIGNOPG.
    const AAPL_FLAGS_LIST: &str = "ACTIVETIM/1,AD/5,RTH/1,AON/1,PREOPGRTH/1,POSTONLY/1,DEACT/1,DEACTDIS/1,DEACTEOD/1";

    /// Each request, once the AAPL definition with `AAPL_FLAGS_LIST` came
    /// back: the frames sent, and the order errors.
    fn flag_frames(reqs: Vec<OrderRequest>) -> (Vec<Vec<(u32, String)>>, Vec<(i64, i64, String)>, Context, Arc<SharedState>) {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        for r in reqs { context.pending_orders.push(r); }
        let mut frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        if let Some(first) = frames.first().filter(|f| tag(f, 35) == Some("c")) {
            let id = tag(first, 320).unwrap().to_string();
            assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, "BEST", AAPL_FLAGS_LIST)));
            frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        }
        let errors = shared.orders.drain_order_errors();
        (frames, errors, context, shared)
    }

    fn lmt_with(order_id: OrderId, attrs: crate::types::OrderAttrs) -> OrderRequest {
        OrderRequest::SubmitEx { order_id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P }, tif: b'0', attrs }
    }

    // ibx#614 (the reference 10.51, paper, 15/10/2026, case A): a selling
    // beta hedge without its max size: 10364, nothing sent, kept for its
    // cancel (Cancelled, 202, 161).
    #[test]
    fn a_selling_beta_hedge_without_its_max_size_is_refused_and_kept() {
        let attrs = crate::types::OrderAttrs {
            parent_id: 57731,
            hedge: Some(crate::types::Hedge { kind: 4, beta: Some(1.0), max_size_missing: true, ..Default::default() }),
            ..Default::default()
        };
        let mut context = order_context();
        context.market.register(265598);
        context.pending_orders.push(OrderRequest::SubmitEx { order_id: 57732, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::Market, tif: b'0', attrs });
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        assert!(peer.messages().is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(57732, 10364, "Max Hedging Size value must be defined for the hedge order.".to_string())]);
        assert_eq!(context.unsent.get(&57732).map(|h| h.1), Some(0), "kept for its cancel, with its own quantity (#602)");
    }

    // ibx#748 (the reference 10.51, paper, 12/11/2026 regular hours: SPY
    // 74 held, a selling hedge kept with 74; MSFT -11 held at 521.92 in
    // the portfolio and a stock preset whose default size is an amount of
    // 200000: 383; no position: 0): the quantity of a hedge kept unsent.
    #[test]
    fn a_hedge_kept_unsent_has_the_presets_default_size() {
        let kept = |position: i64, price: f64, amount: Option<f64>| -> Option<i64> {
            let mut context = order_context();
            context.market.register(272093);
            // The stock preset is known to the session.
            context.order_presets.insert("STK".into(), None);
            if let Some(a) = amount {
                context.preset_amounts.insert("STK".into(), a);
            }
            let shared = Arc::new(SharedState::new());
            if position != 0 {
                shared.portfolio.set_position_info(crate::types::PositionInfo {
                    con_id: 272093, position_fixed: position * crate::types::QTY_SCALE, symbol: "MSFT".into(), sec_type: "STK".into(),
                    currency: "USD".into(), ..Default::default() });
                shared.portfolio.set_position_marks(272093, px(price), 0, 0, 0);
            }
            let attrs = crate::types::OrderAttrs {
                parent_id: 9, hedge: Some(crate::types::Hedge { kind: 4, beta: Some(1.0), max_size_missing: true, ..Default::default() }),
                ..Default::default() };
            context.pending_orders.push(OrderRequest::SubmitEx { order_id: 58200, instrument: 0, side: Side::Sell, qty: 10,
                kind: crate::types::OrderKind::Market, tif: b'0', attrs });
            let (conn, mut peer) = crate::test_support::Peer::pair();
            let mut conn = Some(conn);
            drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
            assert!(peer.messages().is_empty(), "nothing sent");
            context.unsent.get(&58200).map(|h| h.1 / crate::types::QTY_SCALE)
        };
        assert_eq!(kept(0, 0.0, Some(200000.0)), Some(0), "not in the portfolio");
        assert_eq!(kept(74, 756.93, Some(200000.0)), Some(74), "a sell that closes the position");
        assert_eq!(kept(-11, 521.9246826, Some(200000.0)), Some(383), "a sell that adds to a short position");
        assert_eq!(kept(-20, 278.63601685, Some(200000.0)), Some(718));
        assert_eq!(kept(-11, 521.9246826, None), Some(0), "a preset without an amount");
    }

    // ibx#602 (the reference 10.51, paper, regular hours 16/10/2026: a
    // parent of 10 AAPL and its beta hedge with a max size of 2): the
    // parent gets 2185, Cancelled with nothing filled, then 201; the hedge
    // is kept unsent without a message; nothing is sent.
    #[test]
    fn the_hedge_size_precaution_discards_the_parent_and_keeps_the_hedge() {
        let mut context = order_context();
        context.market.register(265598);
        context.pending_orders.push(OrderRequest::SubmitEx { order_id: 58102, instrument: 0, side: Side::Buy, qty: 10,
            kind: crate::types::OrderKind::Limit { price: 400 * P }, tif: b'0',
            attrs: crate::types::OrderAttrs { hedge_precaution: Some(true), ..Default::default() } });
        context.pending_orders.push(OrderRequest::SubmitEx { order_id: 58103, instrument: 0, side: Side::Sell, qty: 10,
            kind: crate::types::OrderKind::Market, tif: b'0', attrs: crate::types::OrderAttrs {
                parent_id: 58102, hedge_precaution: Some(false),
                hedge: Some(crate::types::Hedge { kind: 4, beta: Some(1.0), max_size: Some(2), ..Default::default() }),
                ..Default::default() } });
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        assert!(peer.messages().is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(58102, 2185, crate::client_core::HEDGE_SIZE_WARNING.1.to_string())]);
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status, u.filled_qty_fixed, u.remaining_qty_fixed)).collect::<Vec<_>>(),
            [(58102, OrderStatus::Cancelled, 0, 10 * crate::types::QTY_SCALE)]);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1, n.2.as_str())).collect::<Vec<_>>(),
            [(58102, 201, "Order rejected - reason:Order was discarded.")]);
        assert!(!context.unsent.contains_key(&58102), "the parent is gone: its cancel finds nothing");
        assert_eq!(context.unsent.get(&58103).map(|h| h.1), Some(0), "the hedge is kept for its cancel, with its own quantity");
    }

    // ibx#614, the reference's frames of 16/10/2026 (paper, AAPL MKT
    // hedges of a LMT parent): a beta hedge with its max size, a pair
    // hedge of ratio 2, a bracket child with no hedge.
    #[test]
    fn hedge_fields_as_the_reference() {
        let mkt = |order_id: OrderId, side: Side, attrs: crate::types::OrderAttrs| OrderRequest::SubmitEx {
            order_id, instrument: 0, side, qty: 1, kind: crate::types::OrderKind::Market, tif: b'0', attrs };
        let hedge = |kind, beta, ratio, max_size| Some(crate::types::Hedge { kind, beta, ratio, max_size, max_size_missing: false });
        let fields = |f: &[(u32, String)]| f.iter().filter(|(t, _)| [6965, 6665, 6703, 6666, 6690, 38].contains(t))
            .map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|");
        let beta = wire_tags(mkt(1, Side::Sell, crate::types::OrderAttrs { hedge: hedge(4, Some(0.5), None, Some(100)), ..Default::default() }));
        assert_eq!(fields(&beta), "6965=1|6690=100|6665=4|6703=0.50|38=1.00");
        let pair = wire_tags(mkt(2, Side::Sell, crate::types::OrderAttrs { hedge: hedge(3, None, Some(2.0), None), ..Default::default() }));
        assert_eq!(fields(&pair), "6965=1|6665=3|6666=2.00|38=1.00");
        let child = wire_tags(mkt(3, Side::Sell, crate::types::OrderAttrs::default()));
        assert_eq!(fields(&child), "38=1");
        assert_eq!((hedge_number(1.0), hedge_number(1.2345), hedge_number(0.123456789)), ("1.00".into(), "1.2345".into(), "0.12345679".into()));
    }

    fn tags_of(frame: &[(u32, String)]) -> Vec<u32> {
        frame.iter().map(|f| f.0).filter(|t| [6433, 6521, 6524, 6562, 6605].contains(t)).collect()
    }

    // ibx#607, the reference's frames of 12/10/2026 (AAPL, SMART, DAY):
    // allow pre-open on a LMT without outside RTH; post-only
    // and deactivate with outside RTH; ignore opening auction never
    // (RTHIGNOPG is not in the list), without a warning; each only as on (1).
    #[test]
    fn the_order_flags_go_out_where_they_apply() {
        use crate::types::OrderAttrs as A;
        let (frames, errors, ..) = flag_frames(vec![
            lmt_with(70, A { allow_pre_open: true, ..Default::default() }),
            lmt_with(71, A { outside_rth: true, post_only: true, deactivate: true, ..Default::default() }),
            lmt_with(72, A { ignore_open_auction: true, ..Default::default() }),
            lmt_with(73, A { allow_pre_open: true, ignore_open_auction: true, post_only: true, deactivate: true, ..Default::default() }),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
        let sent: Vec<(Option<&str>, Vec<u32>)> = frames.iter().map(|f| (tag(f, 6121), tags_of(f))).collect();
        assert_eq!(sent, [
            (Some("70"), vec![6524]),
            (Some("71"), vec![6433, 6605, 6521]),
            (Some("72"), vec![]),
            (Some("73"), vec![6524, 6605, 6521]),
        ]);
        for f in &frames {
            for (t, v) in f.iter().filter(|(t, _)| [6521, 6524, 6605].contains(t)) {
                assert_eq!(v, "1", "tag {t}");
            }
        }
    }

    // ibx#607, paper 12/10/2026: allow pre-open on a MKT does not apply:
    // dropped with one 2109 naming it, the order sent; with outside RTH on
    // a LMT, where both apply: 437 and nothing sent, its cancel ends it
    // (Cancelled, 202 "Order was discarded", 161).
    #[test]
    fn allow_pre_open_is_dropped_with_2109_or_refused_with_437() {
        use crate::types::OrderAttrs as A;
        let (frames, errors, mut context, shared) = flag_frames(vec![
            OrderRequest::SubmitEx { order_id: 80, instrument: 0, side: Side::Buy, qty: 1,
                kind: crate::types::OrderKind::Market, tif: b'0', attrs: A { allow_pre_open: true, ..Default::default() } },
            lmt_with(81, A { outside_rth: true, allow_pre_open: true, ..Default::default() }),
        ]);
        assert_eq!(frames.iter().map(|f| (tag(f, 6121), tags_of(f))).collect::<Vec<_>>(), [(Some("80"), vec![])]);
        assert_eq!(errors, [
            (80, 2109, "Order Event Warning:Attribute 'Allow pre-open' is ignored based on the order type and destination. PlaceOrder is now being processed.".to_string()),
            (81, 437, "Order can have only one flag Outside RTH or Allow PreOpen.".to_string()),
        ]);
        assert_eq!(shared.orders.dropped_flags(80), crate::bridge::DROPPED_ALLOW_PRE_OPEN);

        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        context.pending_orders.push(OrderRequest::Cancel { order_id: 81 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(81, 202), (81, 161)]);
    }

    // ibx#607: ignore opening auction is dropped without a warning where
    // it does not apply, and noted for the openOrder; a list with its key
    // keeps it (10.51; no such list seen yet).
    #[test]
    fn ignore_opening_auction_follows_its_key() {
        use crate::engine::outside_rth::{open_flag_applies, RthKind, RthTypes};
        let lmt = RthKind::of(&crate::types::OrderKind::Limit { price: 1 });
        let tokens = |s: &str| s.split(',').map(String::from).collect::<Vec<_>>();
        let aapl = RthTypes::from_definition(&tokens(AAPL_FLAGS_LIST), "USSTK", "STK", "USD");
        assert!(!open_flag_applies(lmt, b'0', "BEST", &aapl, aapl.ignore_opening));
        assert!(open_flag_applies(lmt, b'0', "BEST", &aapl, aapl.pre_open));
        let with_key = RthTypes::from_definition(&tokens("RTH/1,RTHIGNOPG/1"), "USSTK", "STK", "USD");
        assert!(open_flag_applies(lmt, b'0', "BEST", &with_key, with_key.ignore_opening));
        // Not on OVERNIGHT + DAY, and not where outside RTH does not apply.
        assert!(!open_flag_applies(lmt, b'b', "BEST", &with_key, with_key.ignore_opening));
        assert!(!open_flag_applies(RthKind::of(&crate::types::OrderKind::Market), b'0', "BEST", &aapl, aapl.pre_open));

        let (frames, errors, _, shared) = flag_frames(vec![
            lmt_with(85, crate::types::OrderAttrs { ignore_open_auction: true, ..Default::default() })]);
        assert_eq!(tags_of(&frames[0]), Vec::<u32>::new());
        assert!(errors.is_empty());
        assert_eq!(shared.orders.dropped_flags(85), crate::bridge::DROPPED_IGNORE_OPEN_AUCTION);
    }

    // ibx#607, paper 12/10/2026: post-only with sweep-to-fill is refused
    // with 497 and nothing sent; on PEG MKT with 496 (AAPL's
    // own list refuses the type first with 387, as here).
    #[test]
    fn post_only_refusals_after_the_lookup() {
        use crate::types::OrderAttrs as A;
        let (frames, errors, ..) = flag_frames(vec![
            lmt_with(90, A { post_only: true, sweep_to_fill: true, ..Default::default() })]);
        assert!(frames.is_empty(), "{frames:?}");
        assert_eq!(errors, [(90, 497, "Post-Only and Sweep-to-Fill attributes cannot be used together.".to_string())]);

        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        let peg = OrderRequest::SubmitEx { order_id: 91, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::PegMkt { price: 0, offset: px(0.05) }, tif: b'0',
            attrs: A { post_only: true, ..Default::default() } };
        assert!(post_only_refusal(&peg, &mut context, &shared));
        assert_eq!(shared.orders.drain_order_errors(),
            [(91, 496, "The Post-Only attribute cannot be used with order type \"PEG MKT\".".to_string())]);
        assert!(context.unsent.contains_key(&91));
        assert!(!post_only_refusal(&lmt_with(92, A { post_only: true, ..Default::default() }), &mut context, &shared));
    }

    // ibx#690 (the reference 10.51, paper, pre-market 14/10/2026): not-held
    // on a stock order for SMART is refused with 10297, nothing sent; on an
    // order for the broker's ATS venue it goes right after the account.
    #[test]
    fn not_held_is_refused_on_smart_and_sent_for_the_ats_venue() {
        use crate::types::OrderAttrs as A;
        let (frames, errors, context, _) = flag_frames(vec![lmt_with(93, A { not_held: true, ..Default::default() })]);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "{frames:?}");
        assert_eq!(errors, [(93, 10297, "Not Held attribute is invalid for this order.".to_string())]);
        assert!(context.unsent.contains_key(&93));

        let mut context = order_context();
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "IBKRATS");
        context.market.set_currency(slot, "USD");
        context.bypass_redirect_warning = true;
        context.pending_orders.push(OrderRequest::SubmitEx { order_id: 94, instrument: slot, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Limit { price: 100 * P }, tif: b'0', attrs: A { not_held: true, ..Default::default() } });
        let shared = Arc::new(SharedState::new());
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 35), Some("c"), "{frames:?}");
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        let reply = format!("8=FIX.4.1|35=d|320={id}|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|6431=LMT/3,MKT/1,STP/1,RTH/1|6046=BEST,NASDAQ,IBKRATS|146=1|6344=1|6008=265598|10=000|").replace('|', "\x01");
        assert!(order_contract_reply(&mut context, &shared, &id, reply.as_bytes()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let order = frames.iter().find(|f| tag(f, 35) == Some("D")).unwrap_or_else(|| panic!("no order: {frames:?}"));
        let at = |t: u32| order.iter().position(|f| f.0 == t);
        assert_eq!(at(6287), at(1).map(|a| a + 1), "{order:?}");
        assert_eq!(tag(order, 6287), Some("1"));
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ibx#607, the reference's replaces of 12/10/2026: allow pre-open and
    // post-only restated, deactivate never (the order goes on to
    // Submitted); the openOrder then shows deactivate off.
    #[test]
    fn a_replace_restates_the_flags_but_deactivate() {
        let attrs = crate::types::OrderAttrs {
            allow_pre_open: true, post_only: true, deactivate: true, ..Default::default() };
        let ours = replace_fields(1655484157, Side::Buy, 1, crate::types::OrderKind::Limit { price: px(101.0) }, b'0', attrs);
        let reference = parse_frame("35=G|11=1655484157.1|41=1655484157.0|44=101.00|1=DU1|6524=1|8339=1|6605=1|6122=c|38=1|54=1|40=2|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=");
        assert_eq!(tags_of(&reference), [6524, 6605]);
        let mut ours_tags = tags_of(&ours);
        ours_tags.sort();
        assert_eq!(ours_tags, [6524, 6605]);
    }

    // ibx#608, the reference's frames of 12/10/2026 (AAPL, SMART): seek
    // price improvement goes out as its attribute on LMT and MKT, only
    // when on.
    #[test]
    fn seek_price_improvement_goes_out_when_on() {
        use crate::types::OrderAttrs as A;
        let reference = parse_frame("35=D|11=135481483.0|44=100.00|1=DU1|6122=c|8339=1|6557=1|6433=1|6121=57616|6119=198|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=");
        assert_eq!(tag(&reference, 6557), Some("1"));
        let on = wire_tags(lmt_with(100, A { seek_price_improvement: true, ..Default::default() }));
        assert_eq!(tag(&on, 6557), Some("1"));
        let mkt = wire_tags(OrderRequest::SubmitEx { order_id: 101, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Market, tif: b'0', attrs: A { seek_price_improvement: true, ..Default::default() } });
        assert_eq!(tag(&mkt, 6557), Some("1"));
        assert_eq!(tag(&wire_tags(lmt_with(102, A::default())), 6557), None);
        // Restated on a replace, as the reference's replace of 12/10/2026.
        let replace = replace_fields(481510639, Side::Buy, 1, crate::types::OrderKind::Limit { price: px(101.0) }, b'0',
            A { seek_price_improvement: true, ..Default::default() });
        assert_eq!(tag(&replace, 6557), Some("1"));
        // Directed away from SMART: left out, and noted for the openOrder
        // (the attribute's own rule, 10.51).
        let mut context = order_context();
        context.market.register(265598);
        context.market.set_routing(0, "STK", "ISLAND");
        let shared = Arc::new(SharedState::new());
        let mut req = lmt_with(103, A { seek_price_improvement: true, ..Default::default() });
        drop_seek_off_smart(&mut req, &context, &shared);
        assert!(!req.new_order_side().unwrap().1.unwrap().seek_price_improvement);
        assert_eq!(shared.orders.dropped_flags(103), crate::bridge::DROPPED_SEEK_PRICE_IMPROVEMENT);
    }

    // ibx#608, paper 12/10/2026: an order with routeMarketableToBbo gets
    // 10353 and is discarded (Cancelled, 201 "Order was discarded."),
    // nothing sent; with the precaution bypassed it goes out with its
    // attribute.
    #[test]
    fn route_marketable_to_bbo_is_discarded_unless_bypassed() {
        let attrs = crate::types::OrderAttrs { route_marketable_to_bbo: true, ..Default::default() };
        let mut context = order_context();
        context.market.register(265598);
        context.bypass_route_bbo_warning = false;
        context.pending_orders.push(lmt_with(110, attrs.clone()));
        let shared = Arc::new(SharedState::new());
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        assert!(peer.messages().is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(110, 10353, ROUTE_BBO_WARNING.to_string())]);
        assert!(ROUTE_BBO_WARNING.starts_with("Warning: Setting \"Route marketable orders to exchanges at BBO only\" order attribute can"));
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(), [(110, OrderStatus::Cancelled)]);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1, n.2.as_str())).collect::<Vec<_>>(),
            [(110, 201, "Order rejected - reason:Order was discarded.")]);

        let sent = wire_tags_with(|ctx| ctx.bypass_route_bbo_warning = true, lmt_with(111, attrs));
        assert_eq!(tag(&sent, 8265), Some("1"));
    }

    // ibx#609 (the reference 10.51, paper, 16/10/2026, case E of
    // `.tmp/609/case.py`): attached orders asked from a preset that does
    // not auto-attach them: 10355 naming the profit taker, Cancelled with
    // a permId, 202; nothing sent. The stop-loss leg alone names itself.
    #[test]
    fn attached_orders_from_an_undefined_preset_are_discarded() {
        for (legs, id, leg) in [((true, true), 58081, "Profit Taker"), ((false, true), 58090, "Stop Loss")] {
            let attrs = crate::types::OrderAttrs { attached_from_preset: legs, ..Default::default() };
            let mut context = Context::new();
            context.market.register(265598);
            context.pending_orders.push(lmt_with(id, attrs));
            let shared = Arc::new(SharedState::new());
            let (conn, mut peer) = crate::test_support::Peer::pair();
            let mut conn = Some(conn);
            drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
            // The preset of the type is asked first, the order waits
            // (ibx#717).
            let asked: Vec<String> = peer.messages().iter().map(|m| String::from_utf8_lossy(m).replace('', "|")).collect();
            assert!(asked.len() == 1 && asked[0].contains("6040=193") && asked[0].contains("8168=s=STK"), "{asked:?}");
            assert!(shared.orders.drain_order_errors().is_empty(), "it waits for the preset");
            let reply: std::collections::HashMap<u32, String> =
                [(8168, "s=STK"), (4026, "DAY")].into_iter().map(|(t, v)| (t, v.to_string())).collect();
            preset_reply(&mut context, &fix::field_view(&reply));
            drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
            assert!(peer.messages().is_empty(), "nothing sent");
            assert_eq!(shared.orders.drain_order_errors(), [(id, 10355, format!("Cannot auto-attach {leg}. Preset is not defined."))]);
            let updates = shared.orders.drain_order_updates();
            assert_eq!(updates.iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(), [(id, OrderStatus::Cancelled)]);
            assert_ne!(updates[0].perm_id, id, "a permId of its own");
            assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1, n.2.as_str())).collect::<Vec<_>>(),
                [(id, 202, "Order Canceled - reason:Order was discarded.")]);
        }
    }

    /// The captured frame's fields named in `tags`, in its order.
    fn captured(frame: &str, tags: &[u32]) -> Vec<(u32, String)> {
        parse_frame(frame).into_iter().filter(|(t, _)| tags.contains(t)).collect()
    }

    /// Our frame's fields named in `tags`, in the order of `want`.
    fn ours_as(ours: &[(u32, String)], want: &[(u32, String)]) -> Vec<(u32, String)> {
        want.iter().map(|(t, _)| (*t, tag(ours, *t).unwrap_or("<absent>").to_string())).collect()
    }

    // ibx#413 (captured 25/09/2026, BUY 1 AAPL SMART, account masked): the
    // reference writes the offset in both price fields, 0.00 when the
    // caller gives none, and keeps the contract's routing.
    const SNAP_MKT_005: &str = "35=D|11=x|99=0.05|1=DU1|6122=c|6121=45|6119=250|38=1|40=SMKT|211=0.05|55=AAPL|167=STK|231=1.00|54=1|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";

    #[test]
    fn snap_orders_carry_the_offset_in_both_price_fields_like_the_reference() {
        const PRICE_TAGS: [u32; 7] = [40, 99, 211, 44, 18, 100, 6210];
        for (code, offset, ex) in [("SMKT", 5 * P / 100, false), ("SMID", 5 * P / 100, true),
                                   ("SREL", 0, false), ("SMKT", 0, true)] {
            let reference = SNAP_MKT_005.replace("40=SMKT", &format!("40={code}"))
                .replace("=0.05", if offset == 0 { "=0.00" } else { "=0.05" });
            let want = captured(&reference, &PRICE_TAGS);
            let req = if ex {
                let kind = match code {
                    "SMKT" => crate::types::OrderKind::SnapMkt { offset },
                    "SMID" => crate::types::OrderKind::SnapMid { offset },
                    _ => crate::types::OrderKind::SnapPri { offset },
                };
                OrderRequest::SubmitEx { order_id: 80, instrument: 0, side: Side::Buy, qty: 1, kind, tif: b'1',
                    attrs: Default::default() }
            } else {
                match code {
                    "SMKT" => OrderRequest::SubmitSnapMkt { order_id: 80, instrument: 0, side: Side::Buy, qty: 1, offset },
                    "SMID" => OrderRequest::SubmitSnapMid { order_id: 80, instrument: 0, side: Side::Buy, qty: 1, offset },
                    _ => OrderRequest::SubmitSnapPri { order_id: 80, instrument: 0, side: Side::Buy, qty: 1, offset },
                }
            };
            let ours = wire_tags(req);
            assert_eq!(ours_as(&ours, &want), want, "{code} offset {offset} extended {ex}");
            for absent in [44, 18] {
                assert!(tag(&ours, absent).is_none(), "{code}: field {absent} is not sent");
            }
        }
    }

    // A replace of a snap order restates the offset in both fields.
    #[test]
    fn snap_replace_restates_the_offset() {
        let ours = replace_fields(81, Side::Buy, 1, crate::types::OrderKind::SnapMid { offset: 10 * P / 100 },
            b'0', Default::default());
        assert_eq!(tag(&ours, 40), Some("SMID"));
        assert_eq!(tag(&ours, 99), Some("0.10"));
        assert_eq!(tag(&ours, 211), Some("0.10"));
        assert!(pos(&ours, 99) < pos(&ours, 1) && pos(&ours, 211) == pos(&ours, 40) + 1);
    }

    /// The request the API mapping builds for `order` on instrument 0.
    fn api_request(order: &crate::api::types::Order, order_id: OrderId) -> OrderRequest {
        match crate::client_core::ClientCore::build_order_request(order, order_id, 0) {
            Ok(crate::types::ControlCommand::Order(req)) => req,
            other => panic!("unexpected {other:?}"),
        }
    }

    const PEG_BENCH_TAGS: [u32; 11] = [40, 18, 44, 99, 211, 6941, 6938, 6939, 6942, 6580, 59];

    // ibx#415 (captured 25/09/2026, BUY 1 AAPL SMART, reference SPY on
    // ARCA, account masked). The attribute fields have no fixed order in
    // the reference, so the values are compared field by field.
    #[test]
    fn peg_bench_from_the_api_matches_the_reference() {
        let reference = "35=D|11=x|99=272.88|1=DU1|6938=0.50|6941=756733|6122=c|6580=272.88|6939=0.50|6942=ARCA|6121=55|6119=250|38=1|40=PB|18=R|55=AAPL|167=STK|231=1.00|54=1|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let order = crate::api::types::Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "PEG BENCH".into(), tif: "GTC".into(),
            starting_price: 272.88, stock_ref_price: 272.88, reference_contract_id: 756733,
            pegged_change_amount: 0.5, reference_change_amount: 0.5, reference_exchange_id: "ARCA".into(),
            ..Default::default()
        };
        let ours = wire_tags(api_request(&order, 82));
        let want = captured(reference, &PEG_BENCH_TAGS);
        assert_eq!(ours_as(&ours, &want), want);
        for absent in [44, 211] {
            assert!(tag(&ours, absent).is_none(), "field {absent} is not sent");
        }
        assert_eq!(tag(&ours, 100), Some("BEST"));
    }

    // The 26/09/2026 capture (SPY, reference QQQ on NASDAQ, DAY), on the
    // engine's own request; a decrease goes out negative (not observed
    // yet).
    #[test]
    fn peg_bench_request_matches_the_reference_and_signs_a_decrease() {
        let reference = "35=D|11=x|99=721.35|1=DU1|6942=NASDAQ|6941=320227571|6939=0.10|6122=c|6938=0.10|6580=744.50|38=1|40=PB|18=R|55=SPY|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=";
        let req = |decrease: bool| OrderRequest::SubmitPegBench {
            order_id: 83, instrument: 0, side: Side::Buy, qty: 1, price: px(721.35),
            ref_con_id: 320227571, is_peg_decrease: decrease, pegged_change_amount: px(0.10),
            ref_change_amount: px(0.10), stock_ref_price: px(744.50), ref_exchange: "NASDAQ".into(),
        };
        let want = captured(reference, &PEG_BENCH_TAGS);
        assert_eq!(ours_as(&wire_tags(req(false)), &want), want);
        assert_eq!(tag(&wire_tags(req(true)), 6938), Some("-0.10"));
        // SMART as the reference writes it.
        let smart = OrderRequest::SubmitPegBench {
            order_id: 84, instrument: 0, side: Side::Buy, qty: 1, price: px(721.35), ref_con_id: 1,
            is_peg_decrease: false, pegged_change_amount: 0, ref_change_amount: 0, stock_ref_price: 0,
            ref_exchange: "SMART".into(),
        };
        let tags = wire_tags(smart);
        assert_eq!(tag(&tags, 6942), Some("BEST"));
        assert_eq!(tag(&tags, 6580), None, "unset stock reference price");
    }

    // The replace (captured 26/09/2026) restates the starting price and the
    // benchmark changes, not the reference contract or its exchange.
    #[test]
    fn peg_bench_replace_restates_the_starting_price() {
        let kind = crate::types::OrderKind::PegBench {
            starting_price: px(721.45), stock_ref_price: px(744.50), ref_con_id: 320227571,
            is_peg_decrease: false, pegged_change_amount: px(0.10), ref_change_amount: px(0.10),
        };
        let ours = replace_fields(85, Side::Buy, 1, kind, b'0', Default::default());
        let reference = "35=G|99=721.45|1=DU1|6939=0.10|6938=0.10|6580=744.50|38=1|54=1|40=PB|18=R|59=0";
        let want = captured(reference, &[99, 6939, 6938, 6580, 40, 18, 59, 44, 6941, 6942]);
        assert_eq!(ours_as(&ours, &want), want);
        for absent in [44, 6941, 6942] {
            assert!(tag(&ours, absent).is_none(), "field {absent} is not restated");
        }
    }

    // ibx#469 (captured 26/09/2026, 28/09/2026 and 07/10/2026, SMART,
    // account masked): TRAIL MIT by amount and by percent, TRAIL LIT, PEG
    // BEST with the compete values the reference writes itself, and RPI
    // with its offset unset and set. The values are compared field by
    // field; none carries an instruction or a field of another type.
    #[test]
    fn trail_mit_trail_lit_peg_best_and_rpi_from_the_api_match_the_reference() {
        const TAGS: [u32; 11] = [40, 44, 99, 211, 6117, 6268, 6115, 8411, 8412, 59, 100];
        let order = |order_type: &str, action: &str| crate::api::types::Order {
            action: action.into(), total_quantity: 1.0, order_type: order_type.into(), tif: "DAY".into(),
            ..Default::default()
        };
        let cases = [
            ("35=D|11=x|99=20.00|1=DU1|6122=c|6117=748.26|6115=0|6268=0|6121=1|6119=262|38=1|40=TMIT|211=20.00|55=SPY|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { aux_price: 20.0, trail_stop_price: 748.26, ..order("TRAIL MIT", "SELL") }),
            ("35=D|11=x|99=3.00|1=DU1|6122=c|6115=0|6268=100|6121=2|6119=262|38=1|40=TMIT|211=3.00|55=SPY|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { trailing_percent: 3.0, ..order("TRAIL MIT", "SELL") }),
            ("35=D|11=x|44=743.26|99=20.00|1=DU1|6122=c|8339=1|6117=748.26|6115=0|6268=0|6121=3|6119=262|38=1|40=TLIT|211=20.00|55=SPY|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { lmt_price: 743.26, aux_price: 20.0, trail_stop_price: 748.26, ..order("TRAIL LIT", "SELL") }),
            // By percent (captured 07/10/2026).
            ("35=D|11=x|44=789.98|99=3.00|1=DU1|6010=fourleg|6115=0|6122=c|8339=1|6268=100|6117=794.98|6121=139|6119=198|38=1|40=TLIT|211=3.00|55=SPY|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { lmt_price: 789.98, trailing_percent: 3.0, trail_stop_price: 794.98, ..order("TRAIL LIT", "SELL") }),
            ("35=D|11=x|44=718.26|1=DU1|6122=c|8339=1|8411=100|8412=0.02|6121=7|6119=262|38=1|40=E2M|55=SPY|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { lmt_price: 718.26, ..order("PEG BEST", "BUY") }),
            ("35=D|11=x|44=198.90|99=0.00|1=DU1|6122=c|8339=1|6121=132|6119=198|38=1|40=RPI|211=0.00|55=IBM|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=8314|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { lmt_price: 198.9, ..order("RPI", "BUY") }),
            ("35=D|11=x|44=198.90|99=0.01|1=DU1|6122=c|8339=1|6121=133|6119=198|38=1|40=RPI|211=0.01|55=IBM|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=8314|6088=Socket|15=USD|6211=|6238=",
             crate::api::types::Order { lmt_price: 198.9, aux_price: 0.01, ..order("RPI", "BUY") }),
        ];
        for (id, (reference, order)) in cases.into_iter().enumerate() {
            crate::client_core::ClientCore::validate_order(&order).unwrap();
            let ours = wire_tags(api_request(&order, 90 + id as OrderId));
            let want = captured(reference, &TAGS);
            assert_eq!(ours_as(&ours, &want), want, "{reference}");
            for absent in TAGS.iter().chain(&[18]).filter(|t| !want.iter().any(|(w, _)| w == *t)) {
                assert!(tag(&ours, *absent).is_none(), "field {absent} is not sent: {reference}");
            }
        }
    }

    // ibx#469 (captured 26/09/2026, SPY SELL 1, account masked): a TRAIL
    // MIT replace restates its trigger although the server reported the
    // same one, and a TRAIL LIT replace the offset the server reported
    // (5, from a trigger of 751.35 and a limit of 746.35) next to the new
    // limit price. The attributes are compared as a set.
    #[test]
    fn trail_mit_and_trail_lit_replaces_match_the_reference() {
        use crate::types::{OrderAttrs, OrderKind as K};
        const TMIT: &str = "35=G|11=7.1|41=7.0|99=20.10|1=|6117=751.35|6122=c|6010=fourleg|6268=0|38=1|54=2|40=TMIT|211=20.10|55=SPY|167=STK|6035=SPY|59=0|6008=265598|6088=Socket|6211=|6238=";
        const TLIT: &str = "35=G|11=7.1|41=7.0|44=746.25|99=20.00|1=|6117=751.35|6122=c|6010=fourleg|6370=5.00|6268=0|38=1|54=2|40=TLIT|211=20.00|55=SPY|167=STK|6035=SPY|59=0|6008=265598|6088=Socket|6211=|6238=";
        let replace = |kind, reported: bool| wire_tags_with(|ctx| {
            ctx.set_symbol(0, "SPY".to_string());
            ctx.insert_order(Order::new(7, 0, Side::Sell, 1, 0, b'2', b'0', 0));
            ctx.reported_stop.insert(7, px(751.35));
            if reported {
                ctx.trail_limit_reported.insert(7, crate::engine::context::TrailLimitReported {
                    offset: px(5.0), limit: px(746.35), stop: px(751.35),
                });
            }
        }, OrderRequest::Modify {
            new_order_id: 7, order_id: 7, qty: 1, kind, tif: b'0',
            attrs: OrderAttrs { order_ref: "fourleg".into(), ..Default::default() },
        }).into_iter().filter(|(t, _)| !crate::test_support::normalise::FRAMING.contains(t)).collect::<Vec<_>>();
        let sorted = |mut v: Vec<(u32, String)>| { v.sort(); v };
        let same = |ours: Vec<(u32, String)>, reference: &str| {
            let (mine, theirs) = common_order(&ours, reference);
            assert_eq!(mine, theirs, "{reference}");
            let mut want = parse_frame(reference);
            // The ids and the account of this test's session.
            for (t, v) in want.iter_mut() {
                if matches!(*t, 11 | 41 | 1) { *v = tag(&ours, *t).unwrap_or("").to_string(); }
            }
            assert_eq!(sorted(ours), sorted(want), "{reference}");
        };
        same(replace(K::TrailMit { trail: px(20.1), percent: false, trail_stop_price: px(751.35) }, false), TMIT);
        let lit = K::TrailLit { price: px(746.25), trail: px(20.0), percent: false, trail_stop_price: px(751.35) };
        same(replace(lit, true), TLIT);
        // By percent (captured 07/10/2026): the percent, its unit, the
        // offset and the trigger.
        const TLIT_PCT: &str = "35=G|11=7.1|41=7.0|44=746.25|99=3.10|1=|6010=fourleg|6122=c|6370=5.00|6268=100|6117=751.35|38=1|54=2|40=TLIT|211=3.10|55=SPY|167=STK|6035=SPY|59=0|6008=265598|6088=Socket|6211=|6238=";
        same(replace(K::TrailLit { price: px(746.25), trail: px(3.1), percent: true, trail_stop_price: px(751.35) }, true), TLIT_PCT);
        // Before any report the offset is the trigger minus the new limit
        // price of a sell, as for a TRAIL LIMIT.
        assert_eq!(tag(&replace(lit, false), 6370), Some("5.10"));
        // A percent keeps its unit, and no trigger is restated when the
        // order has none (captured 28/09/2026).
        let pct = replace(K::TrailMit { trail: px(3.1), percent: true, trail_stop_price: 0 }, false);
        assert_eq!((tag(&pct, 99), tag(&pct, 211), tag(&pct, 6268), tag(&pct, 6117)),
            (Some("3.10"), Some("3.10"), Some("100"), None));
    }

    // ibx#469: a passive relative order as the reference writes it
    // (not observed yet): its own type, the limit price when set, the offset
    // in both offset fields, 0.00 when unset, no instruction.
    #[test]
    fn passive_relative_from_the_api_follows_the_reference() {
        let order = |lmt_price: f64, aux_price: f64| crate::api::types::Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "PASSV REL".into(), tif: "DAY".into(),
            lmt_price, aux_price, ..Default::default()
        };
        let ours = wire_tags(api_request(&order(718.26, 0.5), 96));
        assert_eq!((tag(&ours, 40), tag(&ours, 44), tag(&ours, 99), tag(&ours, 211), tag(&ours, 18)),
            (Some("PSVR"), Some("718.26"), Some("0.50"), Some("0.50"), None));
        let unset = wire_tags(api_request(&order(0.0, 0.0), 97));
        assert_eq!((tag(&unset, 44), tag(&unset, 99), tag(&unset, 211)), (None, Some("0.00"), Some("0.00")));
    }

    // ibx#469: PEG BEST and RPI are checked against the order-type list
    // of the contract on the order's exchange (captured 28/09/2026 and
    // 07/10/2026: RPI refused with 387 on SPY, sent on IBM); TRAIL MIT and
    // TRAIL LIT are not: every recorded exchange list has their keys but
    // one, and the reference sent both types to that one too (07/10/2026).
    #[test]
    fn peg_best_and_rpi_are_checked_against_the_order_type_list() {
        use crate::engine::outside_rth::{pegged_type_check, RthTypes};
        use crate::types::{OrderAttrs, OrderKind as K};
        let list = |tokens: &str| {
            let tokens: Vec<String> = tokens.split(',').map(String::from).collect();
            RthTypes::from_definition(&tokens, "USSTK", "STK", "USD")
        };
        let (spy, ibm) = (list("LMT/3,REL2MID/1,TRAILLIT/1,TRAILMIT/1"), list("LMT/3,REL2MID/1,RPI/1"));
        let req = |kind| OrderRequest::SubmitEx {
            order_id: 1, instrument: 0, side: Side::Buy, qty: 1, kind, tif: b'0', attrs: OrderAttrs::default(),
        };
        let allowed = |kind, types: &RthTypes| pegged_type_check(&req(kind)).map(|(_, check)| check(types));
        let rpi = K::Rpi { price: P, offset: 0 };
        assert_eq!((allowed(rpi, &spy), allowed(rpi, &ibm)), (Some(false), Some(true)));
        // Passive relative: its key is in no recorded list.
        let passive = K::PassvRel { price: P, offset: P / 2 };
        assert_eq!((allowed(passive, &spy), allowed(passive, &ibm), allowed(passive, &list("LMT/3,PASSVREL/1"))),
            (Some(false), Some(false), Some(true)));
        let best = K::PegBest { price: P };
        assert_eq!((allowed(best, &spy), allowed(best, &list("LMT/3"))), (Some(true), Some(false)));
        assert!(pegged_type_check(&req(K::TrailMit { trail: P, percent: false, trail_stop_price: 0 })).is_none());
        assert!(pegged_type_check(&req(K::TrailLit { price: P, trail: P, percent: false, trail_stop_price: 0 })).is_none());
    }

    // ibx#467 (captured 28/09/2026 in the overnight session, SPY BUY 1 LMT
    // 600 SMART, account masked): OVERNIGHT and OVERNIGHT + DAY go out as
    // a DAY order; DAY with includeOvernight adds the overnight attribute.
    #[test]
    fn overnight_orders_are_sent_like_the_reference() {
        const OV: &str = "35=D|11=x|44=600.00|1=DU1|6010=ib196-ov_smart|6122=c|8339=1|6121=1|6119=260|38=1|40=2|55=SPY|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=";
        const INCL: &str = "35=D|11=x|44=600.00|1=DU1|6010=ib196-day_incl|6122=c|8534=1|8339=1|6121=4|6119=260|38=1|40=2|55=SPY|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=756733|6088=Socket|15=USD|6211=|6238=";
        // The limit price is written without trailing zeros by ibx (not
        // part of this change), so it is compared as a number.
        const TAGS: [u32; 5] = [40, 59, 8534, 100, 6210];
        let cases = [("OVERNIGHT", false, OV), ("OVERNIGHT + DAY", false, OV), ("DAY", true, INCL)];
        for (tif, include_overnight, reference) in cases {
            let order = crate::api::types::Order {
                action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 600.0,
                tif: tif.into(), include_overnight, ..Default::default()
            };
            crate::client_core::ClientCore::validate_order(&order).unwrap();
            let ours = wire_tags(api_request(&order, 86));
            let want = captured(reference, &TAGS);
            assert_eq!(ours_as(&ours, &want), want, "{tif} includeOvernight {include_overnight}");
            assert_eq!(tag(&ours, 44).and_then(|v| v.parse::<f64>().ok()), Some(600.0));
            if !include_overnight {
                assert!(tag(&ours, 8534).is_none(), "{tif}: no overnight attribute");
            }
        }
    }

    const PEG_TAGS: [u32; 9] = [40, 18, 44, 99, 211, 8403, 8404, 100, 6210];

    // ibx#414 (ib-agent#192 B8c, captured 23/09/2026, account masked): a
    // pegged-to-midpoint order with lmtPrice 237.82 and auxPrice 0.05 goes
    // out as the pegged type with the midpoint instruction, a zero offset,
    // no stop price, no mid-offset fields, on the contract's routing.
    #[test]
    fn peg_mid_matches_the_reference() {
        let reference = "35=D|11=1626578609.0|44=237.82|1=DU1|6122=c|6121=76|6119=192|38=1|40=P|211=0.00|18=M|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let order = crate::api::types::Order { tif: "DAY".into(),
            action: "BUY".into(), total_quantity: 1.0, order_type: "PEG MID".into(),
            lmt_price: 237.82, aux_price: 0.05, ..Default::default()
        };
        let want = captured(reference, &PEG_TAGS);
        let ex = crate::api::types::Order { tif: "GTC".into(), ..order.clone() };
        for (label, req) in [("plain", api_request(&order, 87)), ("extended", api_request(&ex, 88))] {
            let ours = wire_tags(req);
            assert_eq!(ours_as(&ours, &want), want, "{label}");
            for absent in [99, 8403, 8404] {
                assert!(tag(&ours, absent).is_none(), "{label}: field {absent} is not sent");
            }
        }
        let replace = replace_fields(89, Side::Buy, 1,
            crate::types::OrderKind::PegMid { price: px(237.92), offset: px(0.05) }, b'0', Default::default());
        assert_eq!((tag(&replace, 40), tag(&replace, 211), tag(&replace, 18), tag(&replace, 44), tag(&replace, 99)),
            (Some("P"), Some("0.00"), Some("M"), Some("237.92"), None));
    }

    // Pegged to market (not observed yet on the wire: ORDER-SUBMIT.md
    // 3.5): the limit price when
    // given, the offset in the stop price and the offset fields, the
    // market instruction.
    #[test]
    fn peg_mkt_is_written_like_the_reference() {
        let req = |price: i64| OrderRequest::SubmitPegMkt {
            order_id: 90, instrument: 0, side: Side::Buy, qty: 1, price, offset: px(0.05) };
        let tags = wire_tags(req(px(240.0)));
        assert_eq!((tag(&tags, 40), tag(&tags, 18), tag(&tags, 44), tag(&tags, 99), tag(&tags, 211)),
            (Some("P"), Some("P"), Some("240.00"), Some("0.05"), Some("0.05")));
        assert_eq!(tag(&tags, 100), Some("BEST"));
        assert!(tag(&wire_tags(req(0)), 44).is_none(), "no limit price when unset");
        let unset = wire_tags(OrderRequest::SubmitEx { order_id: 91, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::PegMkt { price: 0, offset: 0 }, tif: b'1', attrs: Default::default() });
        assert_eq!((tag(&unset, 99), tag(&unset, 211)), (Some("0.00"), Some("0.00")));
        assert_eq!(unset.iter().filter(|(t, _)| *t == 18).count(), 1);
    }

    // ibx#414 (ib-agent#192 B8a, captured 23/09/2026): MIDPRICE on SMART
    // keeps the contract's routing.
    #[test]
    fn midprice_keeps_the_contract_routing() {
        let reference = "35=D|11=1626578607.0|44=237.82|1=DU1|6122=c|6121=74|6119=192|38=1|40=MIDPX|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let want = captured(reference, &[40, 100, 6210]);
        for req in [
            OrderRequest::SubmitMidPrice { order_id: 92, instrument: 0, side: Side::Buy, qty: 1, price_cap: px(237.82) },
            OrderRequest::SubmitEx { order_id: 93, instrument: 0, side: Side::Buy, qty: 1,
                kind: crate::types::OrderKind::MidPrice { price_cap: px(237.82) }, tif: b'1', attrs: Default::default() },
        ] {
            let ours = wire_tags(req);
            assert_eq!(ours_as(&ours, &want), want);
            assert_eq!(tag(&ours, 44).and_then(|v| v.parse::<f64>().ok()), Some(237.82));
        }
    }

    /// The definition reply for a lookup, with the order-type list `tokens`.
    fn definition_reply(id: &str, exchange: &str, tokens: &str) -> Vec<u8> {
        fix::fix_build(&[(35, "d"), (320, id), (6008, "265598"), (55, "AAPL"), (167, "STK"),
            (15, "USD"), (207, exchange), (6523, "USSTK"), (6431, tokens)], 1)
    }

    // ibx#688 (the reference 10.51, paper, 19/10/2026, market closed): a
    // minimum quantity on a contract whose list has no PAON key is 10256, a
    // display size where it has no ICE key 10255, each kept for its cancel;
    // with the keys the order goes out.
    #[test]
    fn minimum_quantity_and_display_size_follow_the_list() {
        let run = |list: &str, attrs: crate::types::OrderAttrs| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(265598);
            context.order_records_any.insert(265598);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "STK", "SMART");
            context.pending_orders.push(OrderRequest::SubmitEx { order_id: 31, instrument: 0, side: Side::Buy, qty: 10,
                kind: crate::types::OrderKind::Limit { price: px(50.0) }, tif: b'0', attrs });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let id = tag(&frames[0], 320).unwrap().to_string();
            assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, tag(&frames[0], 6004).unwrap(), list)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames.iter().any(|f| tag(f, 35) == Some("D")), shared.orders.drain_order_errors(), context.unsent.contains_key(&31))
        };
        let min_qty = crate::types::OrderAttrs { min_qty: 10, ..Default::default() };
        let display = crate::types::OrderAttrs { display_size: 1, ..Default::default() };
        let (sent, errors, kept) = run("DAY/3,LMT/3,ICE/1", min_qty.clone());
        assert!(!sent && kept);
        assert_eq!(errors, [(31, 10256, "The 'Minimum Quantity' order attribute may not be specified for this order.".to_string())]);
        let (sent, errors, kept) = run("DAY/3,LMT/3,PAON/1", display.clone());
        assert!(!sent && kept);
        assert_eq!(errors, [(31, 10255, "The 'Display Size' order attribute may not be specified for this order.".to_string())]);
        let (sent, errors, _) = run("DAY/3,LMT/3,PAON/1,ICE/1", crate::types::OrderAttrs { min_qty: 10, display_size: 1, ..Default::default() });
        assert!(sent && errors.is_empty(), "{errors:?}");
        // A block order (no list seen with its key), sweep-to-fill without
        // SWEEP and imbalance-only without IMB: 10266, 10267, 10342, kept
        // (an AAPL block order of 50, EUR.USD on IDEALPRO).
        for (attrs, code, text) in [
            (crate::types::OrderAttrs { block_order: true, ..Default::default() }, 10266, "The 'BlockOrder' order attribute may not be specified for this order."),
            (crate::types::OrderAttrs { sweep_to_fill: true, ..Default::default() }, 10267, "The 'SweepToFill' order attribute may not be specified for this order."),
            (crate::types::OrderAttrs { imbalance_only: true, ..Default::default() }, 10342, "The 'ImbalanceOnly' order attribute may not be specified for this order."),
        ] {
            let (sent, errors, kept) = run("DAY/3,LMT/3", attrs);
            assert!(!sent && kept, "{code}");
            assert_eq!(errors, [(31, code, text.to_string())]);
        }
        let (sent, errors, _) = run("DAY/3,LMT/3,SWEEP/1,IMB/1", crate::types::OrderAttrs { sweep_to_fill: true, imbalance_only: true, ..Default::default() });
        assert!(sent && errors.is_empty(), "{errors:?}");
    }

    // ibx#688 (the reference 10.51, paper, 19/10/2026, market closed): a
    // market-on-close order on a contract whose list has no MOC key is 387,
    // kept (EUR.USD on IDEALPRO); with the key it goes out.
    #[test]
    fn market_on_close_follows_the_list() {
        let run = |list: &str| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(265598);
            context.order_records_any.insert(265598);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "STK", "SMART");
            context.pending_orders.push(OrderRequest::SubmitEx { order_id: 33, instrument: 0, side: Side::Buy, qty: 1,
                kind: crate::types::OrderKind::Moc, tif: b'0', attrs: Default::default() });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let id = tag(&frames[0], 320).unwrap().to_string();
            assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, tag(&frames[0], 6004).unwrap(), list)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames.iter().any(|f| tag(f, 35) == Some("D")), shared.orders.drain_order_errors(), context.unsent.contains_key(&33))
        };
        let (sent, errors, kept) = run("DAY/3,LMT/3");
        assert!(!sent && kept);
        assert_eq!(errors, [(33, 387, "Unsupported order type for this exchange and security type.".to_string())]);
        let (sent, errors, _) = run("DAY/3,LMT/3,MOC/1");
        assert!(sent && errors.is_empty(), "{errors:?}");
    }

    // ibx#688 (the reference 10.51, paper, 19/10/2026, market closed): an
    // order below the minimum size its contract's definition gives (100
    // for a Tokyo stock) is 388, nothing sent, nothing kept.
    #[test]
    fn an_order_below_the_minimum_size_is_388() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(13905804);
        context.order_records_any.insert(13905804);
        context.set_symbol(0, "7203".to_string());
        context.market.set_routing(0, "STK", "SMART");
        context.min_sizes.insert(13905804, 100);
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 32, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "{frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(32, 388, "Order size 1 is smaller than the minimum required size of 100.".to_string())]);
        assert!(!context.unsent.contains_key(&32));
        // A display size or a minimum quantity below that size: 10083,
        // 10084 (orders of 200 showing 50, or with a minimum of 50).
        for (attrs, code, text) in [
            (crate::types::OrderAttrs { display_size: 50, ..Default::default() }, 10083, "Display size is smaller than the minimum requirement."),
            (crate::types::OrderAttrs { min_qty: 50, ..Default::default() }, 10084, "Min. quantity is smaller than the minimum requirement."),
        ] {
            context.pending_orders.push(OrderRequest::SubmitEx { order_id: 34, instrument: 0, side: Side::Buy, qty: 200,
                kind: crate::types::OrderKind::Limit { price: px(100.0) }, tif: b'0', attrs });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "{frames:?}");
            assert_eq!(shared.orders.drain_order_errors(), [(34, code, text.to_string())]);
        }
    }

    // ibx#414: the order-type lists of AAPL captured 28/09/2026 (on BEST:
    // PEGMID, no PEGMKT; on ISLAND: neither). The reference refuses pegged
    // to market on SMART (ib-agent#192 B8b) and pegged to midpoint on
    // ISLAND (25/09/2026) with 387, nothing sent; pegged to midpoint on
    // SMART goes out (B8c).
    #[test]
    fn pegged_types_not_in_the_list_are_refused_with_387() {
        const BEST_LIST: &str = "ACTIVETIM/1,AD/5,ADDONT/1,ADJUST/1,ALERT/1,ALGO/1,ALLOC/1,AON/1,AVGCOST/1,BASKET/1,BENCHPX/1,CASHQTY/1,COND/1,CONDORDER/1,DARKONLY/1,DARKPOLL/1,DAY/3,DEACT/1,DEACTDIS/1,DEACTEOD/1,DIS/1,DUR/1,GAT/1,GTC/1,GTD/1,GTT/1,HID/1,IBKRATS/1,ICE/1,IMB/1,IOC/1,LIT/1,LMT/3,LOC/1,MIDPX/1,MIT/1,MKT/1,MOC/1,MTL/1,NGCOMB/1,NODARK/1,NONALGO/3,OCA/1,OPG/1,OPGREROUT/1,PEGBENCH/1,PEGMID/1,POSTATS/1,POSTONLY/1,PREOPGRTH/1,PRICECHK/1,REL/1,REL2MID/1,RELPCTOFS/1,RPI/1,RTH/1,SCALE/1,SCALEODD/1,SCALERST/1,SIZECHK/1,SMARTSTG/1,SNAPMID/1,SNAPMKT/1,SNAPREL/1,STP/1,STPLMT/1,SWEEP/1,TRAIL/1,TRAILLIT/1,TRAILLMT/1,TRAILMIT/1,WHATIF/1";
        const ISLAND_LIST: &str = "ACTIVETIM/1,AD/5,ADJUST/1,ALERT/1,ALGOCLS/1,ALGOOPG/1,ALLOC/1,AON/3,AVGCOST/1,BASKET/1,BENCHPX/1,CASHQTY/1,COND/1,CONDORDER/1,DAY/3,DEACT/1,DEACTDIS/1,DEACTEOD/1,DIS/1,GAT/1,GTC/1,GTD/1,GTT/1,HID/1,IOC/3,LIT/1,LMT/3,LOC/1,MIT/1,MKT/1,MOC/1,MTL/1,NGCOMB/1,NONALGO/3,OCA/1,OPG/1,PEGBENCH/1,RELPCTOFS/1,RTH/1,SCALE/1,SCALERST/1,SNAPMID/1,SNAPMKT/1,SNAPREL/1,STP/1,STPLMT/1,TRAIL/1,TRAILLIT/1,TRAILLMT/1,TRAILMIT/1,WHATIF/1";
        let run = |exchange: &str, list: &str, req: OrderRequest| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(265598);
            context.order_records_any.insert(265598);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "STK", exchange);
            context.pending_orders.push(req);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(frames.len(), 1, "only the definition is asked: {frames:?}");
            assert_eq!(tag(&frames[0], 35), Some("c"));
            let id = tag(&frames[0], 320).unwrap().to_string();
            assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, tag(&frames[0], 6004).unwrap(), list)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames, shared.orders.drain_order_errors())
        };
        let peg_mkt = |id| OrderRequest::SubmitPegMkt { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: 0, offset: px(0.05) };
        let peg_mid = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::PegMid { price: px(237.82), offset: 0 }, tif: b'0', attrs: Default::default() };
        let refused = (94, 387, "Unsupported order type for this exchange and security type.".to_string());

        let (frames, errors) = run("SMART", BEST_LIST, peg_mkt(94));
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(errors, [refused.clone()]);

        let (frames, errors) = run("ISLAND", ISLAND_LIST, peg_mid(94));
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(errors, [refused]);

        let (frames, errors) = run("SMART", BEST_LIST, peg_mid(95));
        assert_eq!(frames.len(), 1);
        assert_eq!((tag(&frames[0], 35), tag(&frames[0], 18)), (Some("D"), Some("M")));
        assert!(errors.is_empty());

        // ibx#493: market and stop with protection, keys MKTPROT and
        // STPPROT, absent from the SMART list (the SPY list on BEST of
        // 01/10/2026 lacks them too).
        let mkt_prt = |id| OrderRequest::SubmitMktPrt { order_id: id, instrument: 0, side: Side::Buy, qty: 1 };
        let stp_prt = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::StpPrt { stop_price: px(200.0) }, tif: b'0', attrs: Default::default() };
        for req in [mkt_prt(94), stp_prt(94)] {
            let (frames, errors) = run("SMART", BEST_LIST, req);
            assert!(frames.is_empty(), "nothing sent: {frames:?}");
            assert_eq!(errors, [(94, 387, "Unsupported order type for this exchange and security type.".to_string())]);
        }
        // ibx#688: with a stop price of 0, sent (the reference 10.51,
        // paper, 18/10/2026).
        let stp_prt_zero = OrderRequest::SubmitEx { order_id: 98, instrument: 0, side: Side::Sell, qty: 1,
            kind: crate::types::OrderKind::StpPrt { stop_price: 0 }, tif: b'0', attrs: Default::default() };
        assert!(crate::engine::outside_rth::pegged_type_check(&stp_prt_zero).is_none());
        let with_prot = format!("{BEST_LIST},MKTPROT/1,STPPROT/1");
        let (frames, errors) = run("SMART", &with_prot, mkt_prt(97));
        assert_eq!((frames.len(), tag(&frames[0], 40)), (1, Some("U")));
        assert!(errors.is_empty());
    }

    // ibx#630 (the reference 10.51, paper, pre-market 12/10 and 13/10/2026,
    // redirect precaution bypassed): AAPL directed to ISLAND, which its
    // valid exchanges do not list, is refused with 464 and nothing is sent;
    // its cancel ends it with Cancelled, 202 and 161. Directed to NASDAQ,
    // which they list, it is sent; so is it when the definition has no list.
    #[test]
    fn a_directed_order_to_an_exchange_the_contract_does_not_list_gets_464() {
        const VALID: &str = "BEST,AMEX,NYSE,CBOE,PHLX,ISE,CHX,ARCA,NASDAQ,DRCTEDGE,BEX,BATS,EDGEA,BYX,IEX,EDGX,FOXRIVER,PEARL,NYSENAT,LTSE,MEMX,IBEOS,OVERNIGHT,TPLUS0,PSX";
        let run = |exchange: &str, valid: Option<&str>, order_id: OrderId| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.bypass_redirect_warning = true;
            context.market.register(265598);
            context.order_records_any.insert(265598);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "STK", exchange);
            context.pending_orders.push(OrderRequest::SubmitLimit {
                order_id, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(tag(&frames[0], 35), Some("c"), "the definition is asked: {frames:?}");
            let id = tag(&frames[0], 320).unwrap().to_string();
            let mut fields = vec![(35, "d"), (320, id.as_str()), (6008, "265598"), (55, "AAPL"), (167, "STK"),
                (15, "USD"), (207, exchange), (6523, "USSTK"), (6431, "DAY/3,LMT/3,MKT/1,RTH/1")];
            if let Some(v) = valid {
                fields.push((6046, v));
            }
            assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames, shared, context, conn, server)
        };
        let (frames, shared, mut context, mut conn, mut server) = run("ISLAND", Some(VALID), 70);
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(70, 464, "Invalid exchange".to_string())]);
        assert!(shared.orders.drain_order_updates().is_empty(), "no status with the refusal");
        context.pending_orders.push(OrderRequest::Cancel { order_id: 70 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_updates().iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(),
            [(70, OrderStatus::Cancelled)]);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(70, 202), (70, 161)]);

        for (exchange, valid) in [("NASDAQ", Some(VALID)), ("ISLAND", None)] {
            let (frames, shared, ..) = run(exchange, valid, 71);
            assert_eq!(frames.iter().filter(|f| tag(f, 35) == Some("D")).count(), 1, "{exchange}: sent: {frames:?}");
            assert!(shared.orders.drain_order_errors().is_empty(), "{exchange}");
        }
    }

    // ibx#743 (the reference 10.51, paper, 17/10/2026, market closed): AAPL
    // by conId on an exchange it has no definition on is refused with 200,
    // nothing sent, the order gone: its cancel gives 10147.
    #[test]
    fn an_order_on_an_exchange_without_a_definition_gets_200() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.bypass_redirect_warning = true;
        context.market.register(265598);
        context.order_records_any.insert(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "XYZ");
        context.pending_orders.push(OrderRequest::SubmitLimit {
            order_id: 72, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 35), Some("c"), "the definition is asked: {frames:?}");
        let id = tag(&frames[0], 320).unwrap().to_string();
        assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&[(35, "d"), (320, id.as_str())], 1)));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(),
            [(72, 200, "No security definition has been found for the request".to_string())]);
        assert!(shared.orders.drain_order_updates().is_empty(), "no status with the refusal");
        assert_eq!(shared.orders.drain_forgotten_orders(), [72], "not an open order any more");
        context.pending_orders.push(OrderRequest::Cancel { order_id: 72 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(72, 10147)]);
    }

    // ibx#666 (the reference 10.51, paper, regular hours 14/10/2026): an
    // option order asks the trading permission of the class its definition
    // names, once; without it the order is refused with 460, nothing sent,
    // and its cancel gives Cancelled, 202 and 161. A second order of the
    // class is refused at once. With the permission the order goes out.
    #[test]
    fn an_option_order_without_the_class_permission_is_refused_with_460() {
        let run = |allowed: &str| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(692808239);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "OPT", "SMART");
            context.pending_orders.push(OrderRequest::SubmitLimit {
                order_id: 80, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(tag(&frames[0], 35), Some("c"), "the definition is asked: {frames:?}");
            let id = tag(&frames[0], 320).unwrap().to_string();
            let fields = vec![(35, "d"), (320, id.as_str()), (6008, "692808239"), (55, "AAPL"), (167, "OPT"),
                (15, "USD"), (207, "BEST"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3,MKT/1")];
            assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(frames.len(), 1, "{frames:?}");
            assert_eq!((tag(&frames[0], 35), tag(&frames[0], 6040), tag(&frames[0], 6410), tag(&frames[0], 6411)),
                (Some("U"), Some("65"), Some("0"), Some("2/OPT/R")));
            let answer: std::collections::HashMap<u32, String> = [(6040, "66"), (6410, "0"), (6411, "2/OPT/R"),
                (6412, "1"), (6414, allowed)].into_iter().map(|(t, v)| (t, v.to_string())).collect();
            permission_reply(&mut context, &fix::field_view(&answer));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames, shared, context, conn, server)
        };
        let (frames, shared, mut context, mut conn, mut server) = run("0");
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(80, 460, "No trading permissions.".to_string())]);
        context.pending_orders.push(OrderRequest::Cancel { order_id: 80 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_updates().iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(),
            [(80, OrderStatus::Cancelled)]);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(80, 202), (80, 161)]);
        context.pending_orders.push(OrderRequest::SubmitLimit {
            order_id: 81, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "not asked again");
        assert_eq!(shared.orders.drain_order_errors(), [(81, 460, "No trading permissions.".to_string())]);

        let (frames, shared, ..) = run("1");
        assert_eq!(frames.iter().filter(|f| tag(f, 35) == Some("D")).count(), 1, "sent: {frames:?}");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    /// A new order on a contract given by its conId (slot 0), its
    /// definition answered with `def`; the frames sent after the answer.
    fn order_by_con_id(con_id: &str, exchange: &str, order: OrderRequest, def: &[(u32, &str)])
        -> (Vec<Vec<(u32, String)>>, Arc<SharedState>, Context, Option<Connection>, crate::protocol::connection::MemTransport)
    {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(con_id.parse().unwrap());
        context.market.set_symbol(0, String::new());
        context.market.set_routing(0, "", exchange);
        context.pending_orders.push(order);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 35), Some("c"), "the definition is asked: {frames:?}");
        let id = tag(&frames[0], 320).unwrap().to_string();
        let mut fields = vec![(35, "d"), (320, id.as_str()), (6008, con_id)];
        fields.extend_from_slice(def);
        assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        (frames, shared, context, conn, server)
    }

    fn permission_answer(key: &str, allowed: &str) -> std::collections::HashMap<u32, String> {
        [(6040, "66"), (6410, "0"), (6411, key), (6412, "1"), (6414, allowed)].into_iter().map(|(t, v)| (t, v.to_string())).collect()
    }

    const ES_PUT: [(u32, &str); 12] = [(55, "ES"), (167, "FOP"), (15, "USD"), (207, "CME"), (200, "202612"), (541, "20261218"),
        (201, "0"), (202, "5000"), (231, "50"), (6035, "ESZ6 P5000"), (6411, "CME/FOP/GLOBEX"), (6431, "DAY/3,LMT/3,MKT/1")];
    const SAP_WARRANT: [(u32, &str); 12] = [(55, "SAP"), (167, "WAR"), (15, "EUR"), (207, "FWB"), (200, "202612"), (541, "20261215"),
        (201, "1"), (202, "85"), (231, "0.1"), (6035, "HE0DGD"), (6411, "FWB/WAR/STK"), (6431, "DAY/3,LMT/3,MKT/1")];

    // ibx#666 (the reference 10.51, paper, pre-market 15/10/2026, `.tmp/666`
    // case F): a futures option order asks the permission of its class and
    // waits for the answer, as an option order; without it, 460 and
    // nothing sent. With it, the order carries the option's terms as a
    // warrant's message does.
    #[test]
    fn a_futures_option_order_waits_for_the_class_permission() {
        let order = || OrderRequest::SubmitLimit { order_id: 82, instrument: 0, side: Side::Buy, qty: 1, price: px(0.25) };
        let run = |allowed: &str| {
            let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("515974959", "CME", order(), &ES_PUT);
            assert_eq!(frames.len(), 1, "the permission is asked and the order waits: {frames:?}");
            assert_eq!(tag(&frames[0], 6411), Some("CME/FOP/GLOBEX"));
            permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FOP/GLOBEX", allowed)));
            (drain_frames(&mut context, &shared, &mut conn, &mut server), shared)
        };
        let (frames, shared) = run("0");
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(82, 460, "No trading permissions.".to_string())]);
        let (frames, _) = run("1");
        let sent = frames.iter().find(|f| tag(f, 35) == Some("D")).expect("sent");
        let terms: Vec<String> = sent.iter().filter(|(t, _)| matches!(t, 55 | 200 | 201 | 202 | 167 | 48 | 22 | 231 | 100 | 15))
            .map(|(t, v)| format!("{t}={v}")).collect();
        assert_eq!(terms.join("|"), "55=ES|200=202612|201=0|202=5000|167=FOP|48=ESZ6 P5000|22=101|231=50.00|100=CME|15=USD");
    }

    // ibx#693 (the reference 10.51, paper, observed in the recordings of
    // 13-16/10/2026): an option or futures option order past its
    // permission check, refused or sent, starts the market data of its
    // option for the engine, once per contract; not while the answer is on
    // its way, and not for a warrant or a stock order.
    #[test]
    fn an_option_order_starts_the_market_data_of_its_option() {
        let option = |id| OrderRequest::SubmitLimit { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) };
        for allowed in ["0", "1"] {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(692808239);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "OPT", "SMART");
            context.pending_orders.push(option(90));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let id = tag(&frames[0], 320).unwrap().to_string();
            let fields = vec![(35, "d"), (320, id.as_str()), (6008, "692808239"), (55, "AAPL"), (167, "OPT"),
                (15, "USD"), (207, "BEST"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3,MKT/1")];
            assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
            let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert!(context.order_models.is_empty(), "the permission's answer is on its way");
            permission_reply(&mut context, &fix::field_view(&permission_answer("2/OPT/R", allowed)));
            let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(context.order_models, [(0, 692808239, "SMART".to_string())], "permission {allowed}");
            context.pending_orders.push(option(91));
            let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(context.order_models.len(), 1, "once per contract");
        }

        let fop = OrderRequest::SubmitLimit { order_id: 92, instrument: 0, side: Side::Buy, qty: 1, price: px(0.25) };
        let (_, shared, mut context, mut conn, mut server) = order_by_con_id("515974959", "CME", fop, &ES_PUT);
        assert!(context.order_models.is_empty());
        permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FOP/GLOBEX", "0")));
        let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(context.order_models, [(0, 515974959, "CME".to_string())]);

        let warrant = OrderRequest::SubmitLimit { order_id: 93, instrument: 0, side: Side::Buy, qty: 1, price: px(0.01) };
        let (frames, _, context, ..) = order_by_con_id("566386640", "FWB", warrant, &SAP_WARRANT);
        assert!(frames.iter().any(|f| tag(f, 35) == Some("D")), "sent: {frames:?}");
        assert!(context.order_models.is_empty(), "no model for a warrant order");

        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.order_records_any.insert(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 94, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        if let Some(id) = frames.first().and_then(|f| tag(f, 320)).map(str::to_string) {
            let fields = vec![(35, "d"), (320, id.as_str()), (6008, "265598"), (55, "AAPL"), (167, "STK"),
                (15, "USD"), (207, "BEST"), (6523, "USSTK"), (6431, "DAY/3,LMT/3,MKT/1,RTH/1")];
            assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
            let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
        }
        assert!(context.order_models.is_empty(), "no model for a stock order");
    }

    /// The fields of a frame but its framing, sequence and time, joined.
    fn body(frame: &[(u32, String)]) -> String {
        frame.iter().filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 10)).map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|")
    }

    // ibx#730 (the reference 10.51, paper, observed in the recordings of
    // 13-16/10/2026: 12 first orders of 12 given by their conId, every
    // type): the definition of a contract given by its conId alone is asked
    // as a contract not seen yet, from the client's source, on the order's
    // exchange (BEST for SMART); a contract given by its symbol keeps the
    // order-type lookup.
    #[test]
    fn an_order_given_by_con_id_alone_asks_a_contract_not_seen_yet() {
        for (con_id, exchange, wire) in [("515974959", "CME", "CME"), ("692806985", "SMART", "BEST")] {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(con_id.parse().unwrap());
            context.market.set_symbol(0, String::new());
            context.market.set_routing(0, "", exchange);
            context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 95, instrument: 0, side: Side::Buy, qty: 1, price: px(0.25) });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(frames.len(), 1, "{frames:?}");
            let id = tag(&frames[0], 320).unwrap().to_string();
            assert!(id.starts_with(crate::control::contracts::SECDEF_MSG_NAME), "{id}");
            assert_eq!(body(&frames[0]), format!("35=c|320={id}|321=2|6088=Socket|146=1|6008={con_id}|6004={wire}"));
        }
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(692808239);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "OPT", "SMART");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 96, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(tag(&frames[0], 320).unwrap().starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME), "{frames:?}");
        assert_eq!(tag(&frames[0], 6088), None);
    }

    /// The internal name of the lookup of a frame, and an answer to it with
    /// `fields` after the name.
    fn answer(frame: &[(u32, String)], fields: &[(u32, &str)]) -> (String, Vec<u8>) {
        let id = tag(frame, 320).unwrap().to_string();
        let mut all = vec![(35, "d"), (320, id.as_str()), (322, "*"), (323, "4")];
        all.extend_from_slice(fields);
        let msg = fix::fix_build(&all, 1);
        (id, msg)
    }

    // ibx#730 (the reference 10.51, paper, observed in the recordings of
    // 13-16/10/2026): the first order on an option, a futures option or a
    // warrant looks up its underlying (by conId on any exchange) once its
    // contract is known, and asks its class's permission once that answer
    // came (5 first orders of 5). Past the permission check, the
    // underlying's preferred definition is asked (two option orders and a
    // futures option order, after the refusal; a warrant, after the
    // question on its permission); a future's answer asks the permission of
    // its class. A later order on the same underlying asks neither (7 of 7).
    // An option order also asks its underlying as the underlying of index
    // options after the question on its permission (2 of 2 option orders
    // but a volatility one; no futures option order).
    #[test]
    fn a_derivative_order_looks_up_its_underlying() {
        const AAPL_CALL: [(u32, &str); 9] = [(55, "AAPL"), (167, "OPT"), (15, "USD"), (207, "BEST"), (6346, "265598"),
            (310, "STK"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3,MKT/1"), (6178, "2")];
        let option = |id| OrderRequest::SubmitLimit { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("692806985", "SMART", option(97), &AAPL_CALL);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let lookup = &frames[0];
        assert!(tag(lookup, 320).unwrap().starts_with(crate::control::contracts::SECDEF_UNDERLYING_NAME));
        assert_eq!((tag(lookup, 6008), tag(lookup, 6004), tag(lookup, 146), tag(lookup, 6088)),
            (Some("265598"), Some("ANYEXCH"), Some("1"), None));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the order waits");
        let aapl = [(55, "AAPL"), (167, "STK"), (207, "BEST"), (6008, "265598"), (15, "USD"), (6178, "1")];
        let (id, msg) = answer(lookup, &aapl);
        assert!(underlying_reply(&mut context, conn.as_mut(), &mut HeartbeatState::new(), &id, &msg));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!((tag(&frames[0], 6040), tag(&frames[0], 6411)), (Some("65"), Some("2/OPT/R")));
        // Then the underlying as the underlying of index options.
        assert!(tag(&frames[1], 320).unwrap().starts_with("IndexOptionUnderConidReqByConid"), "{frames:?}");
        assert_eq!((tag(&frames[1], 6008), tag(&frames[1], 6004)), (Some("265598"), Some("ANYEXCH")));
        permission_reply(&mut context, &fix::field_view(&permission_answer("2/OPT/R", "0")));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(shared.orders.drain_order_errors(), [(97, 460, "No trading permissions.".to_string())]);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert!(tag(&frames[0], 320).unwrap().starts_with(crate::control::contracts::SECDEF_PREFERRED_NAME));
        assert_eq!((tag(&frames[0], 6008), tag(&frames[0], 6004)), (Some("265598"), Some("ANYEXCH")));
        assert_eq!(context.order_models, [(0, 692806985, "SMART".to_string())]);
        assert!(matches!(context.under_preferred.get(&265598), Some(Some(_))), "the model waits for it");
        let (id, msg) = answer(&frames[0], &aapl);
        assert!(underlying_reply(&mut context, conn.as_mut(), &mut HeartbeatState::new(), &id, &msg));
        assert_eq!(context.under_preferred.get(&265598), Some(&None));
        assert_eq!(context.agg_groups.get(&265598), Some(&1), "kept for the model's route");
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "no permission for a stock");

        // A later order on another option of the same underlying: neither.
        context.market.register(692808239);
        context.market.set_symbol(1, String::new());
        context.market.set_routing(1, "", "SMART");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 98, instrument: 1, side: Side::Buy, qty: 1, price: px(0.05) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let mut other = vec![(6008, "692808239")];
        other.extend_from_slice(&AAPL_CALL);
        let (id, msg) = answer(&frames[0], &other);
        assert!(rth_definition_reply(&mut context, &id, &msg));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(shared.orders.drain_order_errors(), [(98, 460, "No trading permissions.".to_string())]);

        // A futures option: its future's preferred definition asks the
        // permission of the future's class.
        let mut es_put = ES_PUT.to_vec();
        es_put.extend([(6346, "515416632"), (310, "FUT")]);
        let fop = OrderRequest::SubmitLimit { order_id: 99, instrument: 0, side: Side::Buy, qty: 1, price: px(0.25) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("515974959", "CME", fop, &es_put);
        let future = [(55, "ES"), (167, "FUT"), (207, "CME"), (6008, "515416632"), (15, "USD"), (6411, "CME/FUT/GLOBEX"),
            (6346, "11004968"), (310, "IND")];
        let (id, msg) = answer(&frames[0], &future);
        assert!(underlying_reply(&mut context, conn.as_mut(), &mut HeartbeatState::new(), &id, &msg));
        let _ = drain_frames(&mut context, &shared, &mut conn, &mut server);
        permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FOP/GLOBEX", "0")));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(tag(&frames[0], 320).unwrap().starts_with(crate::control::contracts::SECDEF_PREFERRED_NAME), "{frames:?}");
        let (id, msg) = answer(&frames[0], &future);
        assert!(underlying_reply(&mut context, conn.as_mut(), &mut HeartbeatState::new(), &id, &msg));
        assert!(context.def_links.get(&515416632).is_some_and(|l| l.under_con_id == 11004968), "the future's index for the model");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!((tag(&frames[0], 6040), tag(&frames[0], 6411)), (Some("65"), Some("CME/FUT/GLOBEX")));

        // A warrant goes out without waiting for its permission: the
        // preferred definition after the question.
        let mut warrant = SAP_WARRANT.to_vec();
        warrant.extend([(6346, "14204"), (310, "STK")]);
        let war = OrderRequest::SubmitLimit { order_id: 100, instrument: 0, side: Side::Buy, qty: 1, price: px(0.01) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("566386640", "FWB", war, &warrant);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let sap = [(55, "SAP"), (167, "STK"), (207, "IBIS"), (6008, "14204"), (15, "EUR")];
        let (id, msg) = answer(&frames[0], &sap);
        assert!(underlying_reply(&mut context, conn.as_mut(), &mut HeartbeatState::new(), &id, &msg));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let kinds: Vec<String> = frames.iter().map(|f| match (tag(f, 35), tag(f, 6040)) {
            (Some("U"), Some("65")) => "permission".to_string(),
            (Some("c"), _) => tag(f, 320).unwrap_or("").trim_end_matches(|c: char| c.is_ascii_digit()).to_string(),
            (Some(t), _) => t.to_string(),
            _ => String::new(),
        }).collect();
        let at = |k: &str| kinds.iter().position(|x| x == k);
        assert!(at("permission").is_some() && at("permission") < at(crate::control::contracts::SECDEF_PREFERRED_NAME), "{kinds:?}");
        assert!(at("D").is_some(), "sent: {kinds:?}");
    }

    // ibx#730: an underlying lookup with no answer in time lets its order go
    // on.
    #[test]
    fn an_order_goes_on_without_its_underlying_lookup_in_time() {
        let mut def = ES_PUT.to_vec();
        def.extend([(6346, "515416632"), (310, "FUT")]);
        let fop = OrderRequest::SubmitLimit { order_id: 101, instrument: 0, side: Side::Buy, qty: 1, price: px(0.25) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("515974959", "CME", fop, &def);
        assert!(tag(&frames[0], 320).unwrap().starts_with(crate::control::contracts::SECDEF_UNDERLYING_NAME));
        context.order_underlyings.insert(515416632, Some(Instant::now()));
        sweep_underlying_lookups(&mut context);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 6411), Some("CME/FOP/GLOBEX"), "{frames:?}");
    }

    // ibx#666 (the reference 10.51, paper, pre-market 15/10/2026, `.tmp/666`
    // cases W then V): a warrant order goes out with the permission of its
    // class asked at the same time, not waited for; the message is the
    // reference's. Once the answer says no permission, the next warrant
    // order of the class is refused with 460 and nothing sent.
    #[test]
    fn a_warrant_order_goes_out_while_its_permission_is_asked() {
        let order = |id| OrderRequest::SubmitLimit { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: px(0.01) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("566386640", "FWB", order(83), &SAP_WARRANT);
        assert_eq!(asked_kinds(&frames), ["permission FWB/WAR/STK", "D", "save s=WAR"], "{frames:?}");
        assert_eq!(tag(&frames[0], 6411), Some("FWB/WAR/STK"));
        let message: Vec<String> = frames[1].iter().filter(|(t, _)| ![8, 9, 10, 34, 52, 11, 1, 6121, 6119].contains(t))
            .map(|(t, v)| format!("{t}={v}")).collect();
        assert_eq!(message.join("|"), concat!("35=D|44=0.01|6122=c|38=1|40=2|55=SAP|200=202612|201=1|202=85|167=WAR|48=HE0DGD|22=101|",
            "231=0.10|54=1|59=0|100=FWB|6210=FWB|6008=566386640|6088=Socket|15=EUR|6211=|6238="));
        permission_reply(&mut context, &fix::field_view(&permission_answer("FWB/WAR/STK", "0")));
        context.pending_orders.push(order(84));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(84, 460, "No trading permissions.".to_string())]);
    }

    const DAX_TURBO: [(u32, &str); 16] = [(55, "DAX"), (167, "IOPT"), (15, "EUR"), (207, "SBF"), (200, "NOEXP"), (541, "NOEXP"),
        (201, "1"), (202, "11088.4204"), (231, "0.01"), (6035, "24W1Z"), (106, "CBK"), (6804, "11088.4204"), (6805, "F"),
        (6411, "SBF/IOPT/IND"), (8179, "FR/IOPT"), (6431, "DAY/3,LMT/3,MKT/1")];
    const US_BILL: [(u32, &str); 13] = [(55, "US-T"), (167, "BILL"), (15, "USD"), (207, "BEST"), (6035, "IBCID819166829"),
        (541, "20261001"), (6504, "1000"), (6503, "GOVT"), (454, "1"), (455, "912797SA6"), (456, "1"),
        (6411, "12/BILL/USA_DEBT"), (6431, "DAY/3,LMT/3")];

    // ibx#666 (the reference 10.51, paper, 17/10/2026, market closed,
    // `.tmp/666/t5/case5.py` cases I, J and L): a structured product order
    // and a treasury bill order go out with the permission of their class
    // asked at the same time, in the reference's messages (the product's
    // as a warrant's, the bill's as a metal's); once the answer says no
    // permission, the next product order is refused with 460, nothing
    // sent. Their order message texts come from their definitions.
    #[test]
    fn structured_product_and_bill_orders_as_the_reference() {
        let order = |id, price| OrderRequest::SubmitLimit { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: px(price) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("411098772", "SBF", order(96, 0.01), &DAX_TURBO);
        assert_eq!(asked_kinds(&frames), ["permission SBF/IOPT/IND", "D", "save s=IOPT"], "{frames:?}");
        assert_eq!(message_of(&frames[1]), concat!("35=D|44=0.01|6122=c|38=1|40=2|55=DAX|200=NOEXP|201=1|202=11088.4204|167=IOPT|",
            "48=24W1Z|22=101|231=0.01|54=1|59=0|100=SBF|6210=SBF|6008=411098772|6088=Socket|15=EUR|6211=|6238="));
        let types = context.rth_types.values().next().unwrap();
        assert_eq!((types.order_text.as_str(), types.face_value), ("DAX Perpetual 11088.4204 Call (CBK,0.01,11088.4204 EUR,No Rebate) (24W1Z) ", None));
        permission_reply(&mut context, &fix::field_view(&permission_answer("SBF/IOPT/IND", "0")));
        context.pending_orders.push(order(97, 0.01));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(97, 460, "No trading permissions.".to_string())]);

        let (frames, _, context, ..) = order_by_con_id("819166829", "SMART", order(98, 50.0), &US_BILL);
        assert_eq!(asked_kinds(&frames), ["permission 12/BILL/USA_DEBT", "D", "save s=BILL"], "{frames:?}");
        assert_eq!(message_of(&frames[1]), concat!("35=D|44=50.00|6122=c|38=1|40=2|55=US-T|167=BILL|48=IBCID819166829|22=101|",
            "231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=819166829|6088=Socket|15=USD|6211=|6238="));
        let types = context.rth_types.values().next().unwrap();
        assert_eq!((types.order_text.as_str(), types.face_value), ("US-T Govt Oct01'26 912797SA6", Some(1000)));
    }

    // ibx#666 (the reference 10.51, paper, 17/10/2026, market closed,
    // `.tmp/666/t5/case5.py` case X): an index order is refused with 10345
    // once its definition is known, nothing sent, the order not kept.
    #[test]
    fn an_index_order_is_refused_with_10345() {
        let spx = [(55, "SPX"), (167, "IND"), (15, "USD"), (207, "CBOE"), (6411, "CBOE/IND/NONE"), (6431, "DAY/3,LMT/3")];
        let (frames, shared, context, ..) = order_by_con_id("416904", "CBOE",
            OrderRequest::SubmitLimit { order_id: 99, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) }, &spx);
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(99, 10345, "You cannot trade a Index.".to_string())]);
        assert!(!context.unsent.contains_key(&99), "not kept");
    }

    // ibx#666 (the reference 10.51, paper, pre-market 15/10/2026, `.tmp/666`
    // case U): a fund bought with a quantity and no cash quantity is
    // refused with 10203 once its definition is known, nothing sent. A type
    // whose message ibx does not write is refused before sending.
    #[test]
    fn a_fund_and_an_unwritten_type_given_by_con_id_are_refused() {
        let fund = [(55, "VFIAX"), (167, "FUND"), (15, "USD"), (207, "FUNDSERV"), (6035, "922908710"),
            (6411, "FUNDSERV/FUND/NONE"), (6431, "DAY/3,MKT/1")];
        let (frames, shared, ..) = order_by_con_id("48013549", "FUNDSERV",
            OrderRequest::SubmitMarket { order_id: 85, instrument: 0, side: Side::Buy, qty: 1 }, &fund);
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(), [(85, 10203,
            "A buy order for FUND must contain non-zero cashQty, and may not contain non-zero totalQuantity.".to_string())]);
        let bond = [(55, "IBM"), (167, "BOND"), (15, "USD"), (207, "SMART"), (6431, "DAY/3,LMT/3")];
        let (frames, shared, ..) = order_by_con_id("123456", "SMART",
            OrderRequest::SubmitLimit { order_id: 86, instrument: 0, side: Side::Buy, qty: 1, price: px(90.0) }, &bond);
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        let errors = shared.orders.drain_order_errors();
        assert_eq!((errors.len(), errors[0].0, errors[0].1), (1, 86, 321), "{errors:?}");
    }

    const XAUUSD: [(u32, &str); 7] = [(55, "XAUUSD"), (167, "CMDTY"), (15, "USD"), (207, "BEST"), (6035, "XAUUSD"),
        (6411, "14/CMDTY/NONE"), (6431, "DAY/3,LMT/3,MKT/1")];
    const VFIAX: [(u32, &str); 7] = [(55, "VFIAX"), (167, "FUND"), (15, "USD"), (207, "FUNDSERV"), (6035, "922908710"),
        (6411, "FUNDSERV/FUND/NONE"), (6431, "DAY/3,MKT/1")];
    const IBM_BOND: [(u32, &str); 6] = [(55, "IBM"), (167, "BOND"), (15, "USD"), (207, "BEST"),
        (6411, "7/BOND/USA_DEBT"), (6431, "DAY/3,LMT/3")];

    /// The message of a new order, without the session's values.
    fn message_of(frame: &[(u32, String)]) -> String {
        frame.iter().filter(|(t, _)| ![8, 9, 10, 34, 52, 11, 1, 6121, 6119].contains(t))
            .map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|")
    }

    // ibx#666 (the reference 10.51, paper, regular hours 16/10/2026,
    // `.tmp/666/case3.py` cases M, Q and S): a metal order and a fund
    // bought by cash go out with the permission asked at the same time, in
    // the reference's message (the fund with a quantity of 1); the next fund
    // order of the class, once the answer says no permission, gets 460 and
    // its cancel reports the cash amount as its remaining quantity.
    #[test]
    fn metal_and_fund_orders_as_the_reference() {
        let (frames, ..) = order_by_con_id("69067924", "SMART",
            OrderRequest::SubmitLimit { order_id: 87, instrument: 0, side: Side::Buy, qty: 1, price: px(1000.0) }, &XAUUSD);
        assert_eq!(asked_kinds(&frames), ["permission 14/CMDTY/NONE", "D", "save s=CMDTY"], "{frames:?}");
        assert_eq!(message_of(&frames[1]), concat!("35=D|44=1000.00|6122=c|38=1|40=2|55=XAUUSD|167=CMDTY|48=XAUUSD|22=101|",
            "231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=69067924|6088=Socket|15=USD|6211=|6238="));
        let cash = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 0,
            kind: crate::types::OrderKind::Market, tif: b'0', attrs: crate::types::OrderAttrs { cash_qty: px(100.0), ..Default::default() } };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("48013549", "FUNDSERV", cash(88), &VFIAX);
        assert_eq!(asked_kinds(&frames), ["permission FUNDSERV/FUND/NONE", "D", "save s=FUND"], "{frames:?}");
        assert_eq!(message_of(&frames[1]), concat!("35=D|6122=c|152=100.00|38=1|40=1|55=VFIAX|167=FUND|48=922908710|22=101|",
            "231=1.00|54=1|59=0|100=FUNDSERV|6210=FUNDSERV|6008=48013549|6088=Socket|15=USD|6211=|6238="));
        permission_reply(&mut context, &fix::field_view(&permission_answer("FUNDSERV/FUND/NONE", "0")));
        context.pending_orders.push(cash(89));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(89, 460, "No trading permissions.".to_string())]);
        assert_eq!(context.unsent.get(&89).map(|u| u.1), Some(100 * crate::types::QTY_SCALE), "the cash amount");
    }

    // ibx#673 (the reference 10.51, paper, regular hours 16/10/2026,
    // `.tmp/673/case.py`): a volatility order on an option waits for the
    // permission of the option's class: without it 460, nothing sent; with
    // it ibx refuses it (its message is not written). On a stock ibx
    // refuses it (the reference prices it from the market).
    // ibx#745 (the reference 10.51, paper, 17/10 and 18/10/2026, market
    // closed): PEG STK on AAPL is refused with 387 at once, nothing sent;
    // its cancel gives Cancelled, 202 and 161. On a contract that takes it
    // its message is not written yet.
    #[test]
    fn a_pegged_to_stock_order_on_a_stock_is_refused_with_387_and_kept() {
        let peg_stk = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::PegStk { delta: f64::MAX }, tif: b'0', attrs: crate::types::OrderAttrs::default() };
        let stock = [(55, "AAPL"), (167, "STK"), (15, "USD"), (207, "SMART"), (6431, "DAY/3,LMT/3")];
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("265598", "SMART", peg_stk(93), &stock);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors(),
            [(93, 387, "Unsupported order type for this exchange and security type.".to_string())]);
        context.pending_orders.push(OrderRequest::Cancel { order_id: 93 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_updates().iter().map(|u| (u.order_id, u.status)).collect::<Vec<_>>(),
            [(93, OrderStatus::Cancelled)]);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(93, 202), (93, 161)]);

        let (frames, shared, ..) = order_by_con_id("123456", "FWB", peg_stk(94), &SAP_WARRANT);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "nothing sent: {frames:?}");
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(94, 321)]);

        // An AAPL option: on BOX with a delta of 0.5, 341 and kept; on
        // SMART, the permission check first (460 without it).
        let call = [(55, "AAPL"), (167, "OPT"), (15, "USD"), (207, "SMART"), (200, "202611"), (541, "20261120"),
            (201, "1"), (202, "250"), (231, "100"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3")];
        let with_delta = |id, delta| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::PegStk { delta }, tif: b'0', attrs: crate::types::OrderAttrs::default() };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("700000001", "BOX", with_delta(95, 0.5), &call);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("D")), "nothing sent: {frames:?}");
        let errors = shared.orders.drain_order_errors();
        assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(95, 341)]);
        assert!(errors[0].2.starts_with("The delta is invalid. Valid values are from 1 to 100.

"));
        context.pending_orders.push(OrderRequest::Cancel { order_id: 95 });
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(shared.orders.drain_order_notices().iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(95, 202), (95, 161)]);
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("700000001", "SMART", with_delta(96, 0.5), &call);
        assert!(frames.iter().any(|f| tag(f, 6411) == Some("2/OPT/R")), "the permission is asked: {frames:?}");
        permission_reply(&mut context, &fix::field_view(&permission_answer("2/OPT/R", "0")));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).iter().all(|f| tag(f, 35) != Some("D")));
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(96, 460)]);
    }

    #[test]
    fn volatility_orders_as_far_as_written() {
        let vol = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Volatility { volatility: px(30.0), vol_type: 2, peg: 0, price: 0 }, tif: b'0',
            attrs: crate::types::OrderAttrs::default() };
        for (allowed, code) in [("0", 460), ("1", 321)] {
            let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("515974959", "CME", vol(91), &ES_PUT);
            assert_eq!(frames.iter().map(|f| tag(f, 35).unwrap_or("")).collect::<Vec<_>>(), ["U"], "it waits: {frames:?}");
            permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FOP/GLOBEX", allowed)));
            assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
            let errors = shared.orders.drain_order_errors();
            assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(91, code)], "{errors:?}");
        }
        // On a stock with no quote held: a limit order without a price,
        // which the server refuses (ibx#673; the reference 10.51, paper,
        // regular hours 12/11/2026).
        let stock = [(55, "AAPL"), (167, "STK"), (15, "USD"), (207, "SMART"), (6431, "DAY/3,LMT/3")];
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("265598", "SMART", vol(92), &stock);
        let mut frames = frames;
        for _ in 0..3 {
            frames.extend(drain_frames(&mut context, &shared, &mut conn, &mut server));
        }
        let order = frames.iter().find(|f| tag(f, 35) == Some("D")).unwrap_or_else(|| panic!("sent: {frames:?}"));
        assert_eq!((tag(order, 40), tag(order, 44)), (Some("2"), None), "a limit order without its price");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // ibx#673 (the reference 10.51, paper, regular hours 16/10/2026: an
    // AAPL stock VOL order went out as a limit order at the midpoint, the
    // stock preset's limit price): a volatility order on a contract that is
    // no option is a limit order at the preset's price from the quote held,
    // a buy's midpoint rounded down and a sell's up; the reference's own
    // preset is the ask (the bid for a sell); a pegged type's own limit
    // price is taken; a fund's is a market order.
    #[test]
    fn a_volatility_order_on_a_stock_is_a_limit_order_at_the_presets_price() {
        let stock = [(55, "AAPL"), (167, "STK"), (15, "USD"), (207, "SMART"), (6431, "DAY/3,LMT/3")];
        let sent = |side: Side, peg: u8, price: f64, preset: Option<PresetLimit>| -> (Option<String>, Option<String>) {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            if let Some(p) = preset {
                context.preset_limits.insert("STK".into(), p);
            }
            context.market.register(265598);
            context.market.set_symbol(0, String::new());
            context.market.set_routing(0, "", "SMART");
            context.market.set_min_tick(0, 0.01);
            context.market.quote_mut(0).bid = px(345.90);
            context.market.quote_mut(0).ask = px(346.01);
            context.market.quote_mut(0).last = px(345.97);
            context.pending_orders.push(OrderRequest::SubmitEx { order_id: 7, instrument: 0, side, qty: 1,
                kind: crate::types::OrderKind::Volatility { volatility: px(30.0), vol_type: 2, peg, price: px(price) },
                tif: b'0', attrs: crate::types::OrderAttrs::default() });
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let id = tag(&frames[0], 320).unwrap().to_string();
            let mut fields = vec![(35, "d"), (320, id.as_str()), (6008, "265598")];
            fields.extend_from_slice(&stock);
            assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
            let mut frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            for _ in 0..3 {
                frames.extend(drain_frames(&mut context, &shared, &mut conn, &mut server));
            }
            assert!(shared.orders.drain_order_errors().is_empty());
            let order = frames.iter().find(|f| tag(f, 35) == Some("D")).unwrap_or_else(|| panic!("sent: {frames:?}"));
            assert!(tag(order, 9816).is_none() && tag(order, 6280).is_none(), "no volatility field");
            (tag(order, 40).map(str::to_string), tag(order, 44).map(str::to_string))
        };
        let mid = Some(PresetLimit { price_type: PRICE_MIDPOINT, ..Default::default() });
        let lmt = |p: &str| (Some("2".to_string()), Some(p.to_string()));
        assert_eq!(sent(Side::Buy, 0, 0.0, mid), lmt("345.95"));
        assert_eq!(sent(Side::Sell, 0, 0.0, mid), lmt("345.96"));
        assert_eq!(sent(Side::Buy, 0, 0.0, None), lmt("346.01"), "the ask");
        assert_eq!(sent(Side::Sell, 0, 0.0, None), lmt("345.90"), "the bid");
        assert_eq!(sent(Side::Buy, b'P', 340.0, mid), lmt("340.00"), "a pegged type's own price");
    }

    #[test]
    fn the_limit_price_of_a_preset_answer() {
        let mut context = order_context();
        let msg = fix::fix_build(&[(35, "U"), (8166, "G"), (8168, "s=STK"), (4020, "200000.0"), (4041, "Amount"), (4033, "MIDPX"), (4084, "4058"), (4088, "12"),
            (4074, "1"), (4084, "4077"), (4085, "20.0"), (4086, "%")], 1);
        let fields = fix::fix_parse(&msg);
        let view: std::collections::HashMap<u32, &str> = fields.iter().map(|(t, v)| (*t, v.as_str())).collect();
        preset_limit_reply(&mut context, &view, &msg);
        assert_eq!(context.preset_limits.get("STK"), Some(&PresetLimit { price_type: PRICE_MIDPOINT, offset: 0.0, percent: false, reverse: true }));
        assert_eq!(context.preset_amounts.get("STK"), Some(&200000.0), "the default size, an amount (ibx#748)");
    }

    // ibx#666 (the reference 10.51, paper, regular hours 16/10/2026,
    // `.tmp/666/case3.py` case B): a bond order waits for the permission of
    // its class; without it, 460 and nothing sent. With it, ibx refuses it:
    // its message is not written yet.
    #[test]
    fn a_bond_order_waits_for_its_permission() {
        let order = || OrderRequest::SubmitLimit { order_id: 90, instrument: 0, side: Side::Buy, qty: 1, price: px(10.0) };
        for (allowed, code) in [("0", 460), ("1", 321)] {
            let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("29105555", "SMART", order(), &IBM_BOND);
            assert_eq!(frames.iter().map(|f| tag(f, 35).unwrap_or("")).collect::<Vec<_>>(), ["U"], "it waits: {frames:?}");
            permission_reply(&mut context, &fix::field_view(&permission_answer("7/BOND/USA_DEBT", allowed)));
            assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
            let errors = shared.orders.drain_order_errors();
            assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(90, code)], "{errors:?}");
        }
    }

    const BTC: [(u32, &str); 7] = [(55, "BTC"), (167, "CRYPTO"), (15, "USD"), (207, "PAXOS"), (6035, "BTC.USD"),
        (6411, "PAXOS/CRYPTO/NONE"), (6431, "DAY/3,LMT/3,MKT/1")];

    // ibx#666 (the reference 10.51, paper, regular hours 16/10/2026,
    // `.tmp/666/case3.py` case Y): a crypto order waits for the permission
    // of its class; without it, no error, no status and nothing sent; its
    // cancel ends it as ApiCancelled, nothing filled, its quantity
    // remaining, permId 0, with nothing sent either. The next crypto order
    // of the class stays pending the same way. With the permission ibx
    // refuses it: its message is not written yet.
    #[test]
    fn a_crypto_order_without_the_permission_stays_pending() {
        let order = |id| OrderRequest::SubmitLimit { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: px(1000.0) };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("479624278", "PAXOS", order(95), &BTC);
        assert_eq!(frames.iter().map(|f| tag(f, 35).unwrap_or("")).collect::<Vec<_>>(), ["U"], "it waits: {frames:?}");
        assert_eq!(tag(&frames[0], 6411), Some("PAXOS/CRYPTO/NONE"));
        permission_reply(&mut context, &fix::field_view(&permission_answer("PAXOS/CRYPTO/NONE", "0")));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert!(shared.orders.drain_order_errors().is_empty(), "no error");
        assert!(shared.orders.drain_order_updates().is_empty(), "no status");
        context.pending_orders.push(OrderRequest::Cancel { order_id: 95 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent at the cancel");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status, u.filled_qty_fixed, u.remaining_qty_fixed, u.perm_id)).collect::<Vec<_>>(),
            [(95, OrderStatus::ApiCancelled, 0, crate::types::QTY_SCALE, 0)]);
        assert!(shared.orders.drain_order_errors().is_empty() && shared.orders.drain_order_notices().is_empty());

        context.pending_orders.push(order(96));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "not asked again, nothing sent");
        assert!(shared.orders.drain_order_errors().is_empty());
        assert!(context.api_pending.contains_key(&96));

        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("479624278", "PAXOS", order(97), &BTC);
        assert_eq!(frames.len(), 1, "it waits: {frames:?}");
        permission_reply(&mut context, &fix::field_view(&permission_answer("PAXOS/CRYPTO/NONE", "1")));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        let errors = shared.orders.drain_order_errors();
        assert_eq!(errors.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(97, 321)], "{errors:?}");
        assert!(context.api_pending.is_empty());
    }

    // ibx#666 (the reference 10.51, paper, regular hours 16/10/2026,
    // `.tmp/666/wire_gw_types3.jsonl` and `wire_gw_types4.jsonl`): a fund
    // order past its permission check starts the fund's top of book for the
    // engine: sent with the question on its way, or refused with 460 once
    // the answer is known. A fund order refused with 10203 before the check
    // starts nothing.
    #[test]
    fn a_fund_order_past_its_permission_check_starts_the_fund_top_of_book() {
        let cash = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 0,
            kind: crate::types::OrderKind::Market, tif: b'0', attrs: crate::types::OrderAttrs { cash_qty: px(100.0), ..Default::default() } };
        let (frames, shared, context, ..) = order_by_con_id("48013549", "FUNDSERV",
            OrderRequest::SubmitMarket { order_id: 98, instrument: 0, side: Side::Buy, qty: 1 }, &VFIAX);
        assert!(frames.is_empty() && context.order_models.is_empty(), "nothing for an order refused before the check");
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| e.1).collect::<Vec<_>>(), [10203]);

        // Sent at once, the question on its way.
        let (frames, _, context, ..) = order_by_con_id("48013549", "FUNDSERV", cash(99), &VFIAX);
        assert_eq!(asked_kinds(&frames), ["permission FUNDSERV/FUND/NONE", "D", "save s=FUND"], "{frames:?}");
        assert_eq!(context.order_models, [(0, 48013549, "FUNDSERV".to_string())]);

        // Refused with 460 once the answer is known.
        let (_, shared, mut context, mut conn, mut server) = order_by_con_id("48013549", "FUNDSERV", cash(100), &VFIAX);
        context.order_models.clear();
        permission_reply(&mut context, &fix::field_view(&permission_answer("FUNDSERV/FUND/NONE", "0")));
        context.pending_orders.push(cash(101));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(101, 460)]);
        assert_eq!(context.order_models, [(0, 48013549, "FUNDSERV".to_string())]);
    }

    // ibx#542, paper 09/10/2026 (the reference and ibx on the same case):
    // an order refused with 387 is kept, unlisted; its cancel gives
    // orderStatus Cancelled, 202 "Order was discarded" and 161; a second
    // cancel finds it cancelled.
    #[test]
    fn the_cancel_of_an_order_refused_with_387_ends_it() {
        const LIST: &str = "DAY/3,LMT/3,MKT/1,STP/1";
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        context.pending_orders.push(OrderRequest::SubmitPegMkt { order_id: 94, instrument: 0, side: Side::Buy, qty: 1, price: 0, offset: px(0.05) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let id = tag(&frames[0], 320).unwrap().to_string();
        assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, tag(&frames[0], 6004).unwrap(), LIST)));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors().iter().map(|e| e.1).collect::<Vec<_>>(), [387]);
        assert!(shared.orders.drain_order_updates().is_empty(), "no status with the refusal");

        context.pending_orders.push(OrderRequest::Cancel { order_id: 94 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status, u.filled_qty_fixed, u.remaining_qty_fixed)).collect::<Vec<_>>(),
            [(94, OrderStatus::Cancelled, 0, crate::types::QTY_SCALE)]);
        let notices = shared.orders.drain_order_notices();
        assert_eq!(notices.iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(94, 202), (94, 161)]);
        assert_eq!(notices[0].2, "Order Canceled - reason:Order was discarded.");
        assert_eq!(notices[1].2, format!("Cancel attempted when order is not in a cancellable state.  Order permId ={}", updates[0].perm_id));
        assert!(shared.orders.drain_order_errors().is_empty());

        context.pending_orders.push(OrderRequest::Cancel { order_id: 94 });
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(shared.orders.drain_order_errors(),
            [(94, 10148, "OrderId 94 that needs to be cancelled cannot be cancelled, state: Cancelled.".to_string())]);
    }

    // ibx#509, paper 09/10/2026: an order held with transmit off sends
    // nothing; its cancel, or a global cancel, ends it as an order that was
    // never sent (Cancelled, 202 "Order was discarded", 161), and a second
    // cancel finds it cancelled; sent as a new order, it is held no more.
    #[test]
    fn an_order_held_with_transmit_off_ends_at_its_cancel() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        let mut hb = HeartbeatState::new();
        let one = crate::types::QTY_SCALE;
        for id in [40, 41, 42] {
            hold_order(&mut conn, &mut context, &mut hb, id, 0, one, if id == 41 { 40 } else { 0 }, "");
        }
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert!(shared.orders.drain_order_updates().is_empty() && shared.orders.drain_order_errors().is_empty());

        // The cancel of a held child.
        context.pending_orders.push(OrderRequest::Cancel { order_id: 41 });
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.iter().map(|u| (u.order_id, u.status, u.remaining_qty_fixed, u.parent_id)).collect::<Vec<_>>(),
            [(41, OrderStatus::Cancelled, one, 40)]);
        let notices = shared.orders.drain_order_notices();
        assert_eq!(notices.iter().map(|n| (n.0, n.1)).collect::<Vec<_>>(), [(41, 202), (41, 161)]);
        assert_eq!(notices[0].2, "Order Canceled - reason:Order was discarded.");
        context.pending_orders.push(OrderRequest::Cancel { order_id: 41 });
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(shared.orders.drain_order_errors(),
            [(41, 10148, "OrderId 41 that needs to be cancelled cannot be cancelled, state: Cancelled.".to_string())]);

        // Sent as a new order: no longer an order that was never sent.
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 40, instrument: 0, side: Side::Buy, qty: 1, price: px(100.0) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.iter().filter(|f| tag(f, 35) == Some("D")).count(), 1);
        assert!(!context.unsent.contains_key(&40));

        // A held order by symbol: its lookup goes out at once; the answer
        // names a contract the engine has, the order takes that slot and
        // goes out on it when it is transmitted.
        let slot = context.market.try_register_unresolved().unwrap();
        context.set_symbol(slot, "AAPL".to_string());
        context.market.set_routing(slot, "STK", "SMART");
        hold_order(&mut conn, &mut context, &mut hb, 43, slot, one, 0, "");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        // The internal name of the lookup, as its answer maps back (ibx#698).
        let lookup = conn.as_ref().unwrap().internal_name(tag(&frames[0], 320).unwrap());
        assert_eq!((frames.len(), tag(&frames[0], 35), tag(&frames[0], 55)), (1, Some("c"), Some("AAPL")));
        let reply = format!("35=d|320={lookup}|322=*|323=4|55=AAPL|167=STK|6008=265598|");
        assert!(order_contract_reply(&mut context, &shared, &lookup, reply.replace('|', "\x01").as_bytes()));
        assert_eq!(context.unsent.get(&43).map(|h| h.0), Some(0), "the slot of the contract found");
        // Held again with another quantity, under the slot it first had.
        hold_order(&mut conn, &mut context, &mut hb, 43, slot, 2 * one, 0, "");
        assert_eq!(context.unsent.get(&43), Some(&(0, 2 * one, 0)));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "no second lookup");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 43, instrument: slot, side: Side::Buy, qty: 1, price: px(100.0) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let sent: Vec<_> = frames.iter().filter(|f| tag(f, 35) == Some("D")).collect();
        assert_eq!((sent.len(), tag(sent[0], 6121), tag(sent[0], 6008)), (1, Some("43"), Some("265598")));

        // The global cancel ends the held order left.
        shared.orders.drain_order_updates();
        shared.orders.drain_order_notices();
        context.pending_orders.push(OrderRequest::GlobalCancel);
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        let ended: Vec<_> = shared.orders.drain_order_updates().into_iter()
            .filter(|u| u.order_id == 42).map(|u| u.status).collect();
        assert_eq!(ended, [OrderStatus::Cancelled]);
        let notices: Vec<_> = shared.orders.drain_order_notices().into_iter().filter(|n| n.0 == 42).map(|n| n.1).collect();
        assert_eq!(notices, [202, 161]);
    }

    // ibx#547, the reference's messages of 26/09 and 09/10/2026: the
    // orders one placeOrder transmits leave together, after the contract
    // of each is known; orders placed while the lookup of their contract
    // is on its way share it.
    #[test]
    fn the_orders_of_a_group_leave_together_after_one_lookup() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        let by_symbol = |context: &mut Context| {
            let slot = context.market.try_register_unresolved().unwrap();
            context.set_symbol(slot, "AAPL".to_string());
            context.market.set_routing(slot, "STK", "SMART");
            slot
        };
        // The parent on a contract that is known, two children by symbol.
        let (a, b) = (by_symbol(&mut context), by_symbol(&mut context));
        let limit = |order_id, instrument| OrderRequest::SubmitLimit { order_id, instrument, side: Side::Buy, qty: 1, price: px(100.0) };
        take_order_group(&mut context, vec![limit(50, 0), limit(51, a), limit(52, b)]);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.iter().map(|f| tag(f, 35).unwrap().to_string()).collect::<Vec<_>>(), ["c"],
            "one lookup, and the parent waits for it too");
        // The internal name of the lookup, as its answer maps back (ibx#698).
        let lookup = conn.as_ref().unwrap().internal_name(tag(&frames[0], 320).unwrap());
        let reply = format!("35=d|320={lookup}|322=*|323=4|55=AAPL|167=STK|6008=265598|");
        assert!(order_contract_reply(&mut context, &shared, &lookup, reply.replace('|', "").as_bytes()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let sent: Vec<_> = frames.iter().filter(|f| tag(f, 35) == Some("D")).map(|f| tag(f, 6121).unwrap().to_string()).collect();
        assert_eq!(sent, ["50", "51", "52"]);
        assert!(frames.iter().filter(|f| tag(f, 35) == Some("D")).all(|f| tag(f, 6008) == Some("265598")));
        assert!(context.order_groups.is_empty() && context.order_lookups.is_empty());
    }

    // ibx#263: all-or-none is refused with 10257 and nothing is sent when
    // the contract's order-type list for the order's exchange has no AON
    // key, or has it in state 4; with
    // the key (the captured AAPL lists of 28/09/2026: AON/1 on BEST,
    // AON/3 on ISLAND) the order goes out with its all-or-none flag.
    #[test]
    fn all_or_none_not_in_the_list_is_refused_with_10257() {
        let run = |exchange: &str, list: &str, req: OrderRequest| {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.market.register(265598);
            context.order_records_any.insert(265598);
            context.set_symbol(0, "AAPL".to_string());
            context.market.set_routing(0, "STK", exchange);
            context.pending_orders.push(req);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(frames.len(), 1, "only the definition is asked: {frames:?}");
            let id = tag(&frames[0], 320).unwrap().to_string();
            assert!(rth_definition_reply(&mut context, &id, &definition_reply(&id, tag(&frames[0], 6004).unwrap(), list)));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            (frames, shared.orders.drain_order_errors())
        };
        let aon = |id| OrderRequest::SubmitLimitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P,
            tif: b'0', attrs: crate::types::OrderAttrs { all_or_none: true, ..Default::default() } };
        let refused = (80, 10257, "The 'All or None' order attribute may not be specified for this order.".to_string());
        for list in ["ACTIVETIM/1,AD/5,LMT/3,RTH/1", "ACTIVETIM/1,AON/4,LMT/3,RTH/1"] {
            let (frames, errors) = run("SMART", list, aon(80));
            assert!(frames.is_empty(), "nothing sent: {frames:?}");
            assert_eq!(errors, std::slice::from_ref(&refused));
        }
        for (exchange, list) in [("SMART", "ACTIVETIM/1,AON/1,LMT/3,RTH/1"), ("ISLAND", "ACTIVETIM/1,AON/3,LMT/3,RTH/1")] {
            let (frames, errors) = run(exchange, list, aon(81));
            assert_eq!(frames.len(), 1, "{exchange}");
            assert_eq!((tag(&frames[0], 35), tag(&frames[0], 18)), (Some("D"), Some("G")));
            assert!(errors.is_empty());
        }
    }

    // A definition with no order-type list, or none in time: the type is
    // not checked and the order goes out.
    #[test]
    fn pegged_types_without_a_list_are_sent() {
        let tags = wire_tags(OrderRequest::SubmitPegMkt { order_id: 96, instrument: 0, side: Side::Buy, qty: 1, price: 0, offset: 0 });
        assert_eq!(tag(&tags, 35), Some("D"));
    }

    // ibx#425: the customer account and professional customer fields.
    #[test]
    fn customer_account_tags() {
        let tags = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: 70, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0',
            attrs: crate::types::OrderAttrs { customer_account: "C123".into(), professional_customer: true, ..Default::default() } });
        assert_eq!(tag(&tags, 6207), Some("C123"));
        assert_eq!(tag(&tags, 6636), Some("1"));
        let none = wire_tags(OrderRequest::SubmitLimitEx {
            order_id: 71, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0', attrs: Default::default() });
        assert_eq!(tag(&none, 6207), None);
        assert_eq!(tag(&none, 6636), None);
    }

    /// Drain one request on a session with the given logon flags and
    /// account: the frames sent and the errors given.
    fn short_sale_run(super_user: bool, omnibus: bool, account: &str, req: OrderRequest)
        -> (Vec<Vec<(u32, String)>>, Vec<(i64, i64, String)>)
    {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.set_short_sale_flags(super_user, omnibus);
        let mut context = order_context();
        context.market.register(265598);
        context.pending_orders.push(req);
        drain_and_send_orders(&mut conn, &mut context, account, &mut HeartbeatState::new(), false, &shared);
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 16384];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 { break; }
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut frames: Vec<Vec<(u32, String)>> = Vec::new();
        for field in bytes.split(|&b| b == fix::SOH) {
            let Ok(text) = std::str::from_utf8(field) else { continue };
            let Some((t, v)) = text.split_once('=') else { continue };
            let Ok(t) = t.parse::<u32>() else { continue };
            if t == 8 { frames.push(Vec::new()); }
            if let Some(f) = frames.last_mut() { f.push((t, v.to_string())); }
        }
        (frames, shared.orders.drain_order_errors())
    }

    fn short_limit(order_id: OrderId, attrs: crate::types::OrderAttrs) -> OrderRequest {
        OrderRequest::SubmitLimitEx { order_id, instrument: 0, side: Side::ShortSell, qty: 1,
            price: 100 * P, tif: b'0', attrs }
    }

    fn short_attrs(intent: &str, slot: i32, location: &str, exempt_code: i32) -> crate::types::OrderAttrs {
        crate::types::OrderAttrs {
            clearing_intent: intent.into(),
            short_sale: crate::types::ShortSale { slot, location: location.into(), exempt_code },
            ..Default::default()
        }
    }

    fn side_refusal(order_id: OrderId, cause: &str) -> Vec<(i64, i64, String)> {
        vec![(order_id, 321, format!("Error validating request.-'PlaceOrder(3)' : cause - {}", cause))]
    }

    // ibx#417: a retail or paper session (no super user, no omnibus, an
    // account id of the broker's clearing) refuses the short side with
    // 321 and sends nothing, on every order path.
    #[test]
    fn short_side_is_refused_without_the_logon_flags_or_an_away_clearing() {
        for req in [
            short_limit(1, Default::default()),
            OrderRequest::SubmitMarket { order_id: 1, instrument: 0, side: Side::ShortSell, qty: 1 },
            short_limit(1, short_attrs("IB", 1, "", -1)),
            OrderRequest::SubmitWhatIf { request: Box::new(short_limit(1, Default::default())) },
        ] {
            let (frames, errors) = short_sale_run(false, false, "DU1", req);
            assert!(frames.is_empty(), "nothing sent: {frames:?}");
            assert_eq!(errors, side_refusal(1, INVALID_SIDE));
        }
        // A plain sell is not checked.
        let (frames, errors) = short_sale_run(false, false, "DU1",
            OrderRequest::SubmitMarket { order_id: 2, instrument: 0, side: Side::Sell, qty: 1 });
        assert_eq!(frames.len(), 1);
        assert!(errors.is_empty());
        assert!(tag(&frames[0], 114).is_none() && tag(&frames[0], 6086).is_none(), "no short-sale fields on a sell");
    }

    // ibx#417: the clearing decides for an order without intent: Away for
    // an account id starting with T or with G second or third.
    #[test]
    fn clearing_away_follows_the_intent_and_the_account_id() {
        assert!(clearing_away("Away", "DU1", false));
        assert!(clearing_away("PTA", "DU1", false));
        assert!(clearing_away("away", "DU1", false));
        assert!(!clearing_away("IB", "T123", true));
        assert!(!clearing_away("", "DU1", false));
        assert!(!clearing_away("", "U1234", false));
        assert!(clearing_away("", "DU1", true));
        assert!(clearing_away("", "T1234", false));
        assert!(clearing_away("", "UG123", false));
        assert!(clearing_away("", "UXG12", false));
    }

    // ibx#417: an away clearing passes the side check; the slot rules then
    // apply, each refused with its rule text, nothing sent.
    #[test]
    fn short_sale_slot_rules_follow_the_reference() {
        let cases = [
            (short_attrs("Away", 0, "", -1), BAD_SHORT_SLOT),
            (short_attrs("Away", 3, "", -1), BAD_SHORT_SLOT),
            (short_attrs("Away", 1, "", 0), NOT_SHORT_SALE_EXEMPT),
            (short_attrs("Away", 1, "TMBR", -1), SLOT_1_NO_LOCATION),
            (short_attrs("Away", 2, "", -1), SLOT_2_NEEDS_LOCATION),
        ];
        for (attrs, cause) in cases {
            let (frames, errors) = short_sale_run(false, false, "DU1", short_limit(5, attrs));
            assert!(frames.is_empty(), "{cause}: nothing sent");
            assert_eq!(errors, side_refusal(5, cause));
        }
        // An omnibus logon only: no slot and no location may be given.
        let (frames, errors) = short_sale_run(false, true, "DU1", short_limit(6, short_attrs("", 1, "", -1)));
        assert!(frames.is_empty());
        assert_eq!(errors, side_refusal(6, NOT_INSTITUTIONAL));
    }

    // ibx#417: an allowed short side goes out as the short side with the
    // short-sale fields.
    #[test]
    fn allowed_short_side_carries_the_short_sale_fields() {
        let (frames, errors) = short_sale_run(false, false, "DU1", short_limit(7, short_attrs("Away", 1, "", -1)));
        assert!(errors.is_empty(), "{errors:?}");
        let f = &frames[0];
        assert_eq!((tag(f, 54), tag(f, 114), tag(f, 6086), tag(f, 5700), tag(f, 1688)),
            (Some("5"), Some("N"), Some("1"), None, None));

        let (frames, _) = short_sale_run(true, false, "DU1", short_limit(8, short_attrs("", 2, "TMBR", -1)));
        let f = &frames[0];
        assert_eq!((tag(f, 114), tag(f, 5700), tag(f, 6086)), (Some("Y"), Some("TMBR"), Some("2")));
        assert!(pos(f, 114) < pos(f, 5700) && pos(f, 5700) < pos(f, 6086));

        // An omnibus logon with no slot: sent with slot 0.
        let (frames, errors) = short_sale_run(false, true, "DU1",
            OrderRequest::SubmitMarket { order_id: 9, instrument: 0, side: Side::ShortSell, qty: 1 });
        assert!(errors.is_empty());
        assert_eq!((tag(&frames[0], 54), tag(&frames[0], 114), tag(&frames[0], 6086)), (Some("5"), Some("N"), Some("0")));
    }

    /// A new order's fields without the session fields (framing, sequence,
    /// times, checksum).
    fn order_body(tags: Vec<(u32, String)>) -> Vec<(u32, String)> {
        use crate::test_support::normalise::{FRAMING, SESSION_TIMES};
        tags.into_iter().filter(|(t, _)| !FRAMING.contains(t) && !SESSION_TIMES.contains(t)).collect()
    }

    // ibx#375: every order type goes out field for field the same through
    // its own request and through the extended one, in the order of the
    // reference's single new-order writer.
    #[test]
    fn both_encoders_write_one_order_like_the_reference() {
        use crate::types::{AdjustedOrderType, OrderKind as K};
        let (id, side, qty) = (40, Side::Sell, 3);
        let cases: Vec<(OrderRequest, K, u8, crate::types::OrderAttrs)> = vec![
            (OrderRequest::SubmitLimit { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Limit { price: 100 * P }, b'0', Default::default()),
            (OrderRequest::SubmitMarket { order_id: id, instrument: 0, side, qty }, K::Market, b'0', Default::default()),
            (OrderRequest::SubmitStop { order_id: id, instrument: 0, side, qty, stop_price: 90 * P }, K::Stop { stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitStopLimit { order_id: id, instrument: 0, side, qty, price: 89 * P, stop_price: 90 * P },
                K::StopLimit { price: 89 * P, stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitLimitGtc { order_id: id, instrument: 0, side, qty, price: 100 * P, outside_rth: true },
                K::Limit { price: 100 * P }, b'1', crate::types::OrderAttrs { outside_rth: true, ..Default::default() }),
            (OrderRequest::SubmitStopGtc { order_id: id, instrument: 0, side, qty, stop_price: 90 * P, outside_rth: false },
                K::Stop { stop_price: 90 * P }, b'1', Default::default()),
            (OrderRequest::SubmitStopLimitGtc { order_id: id, instrument: 0, side, qty, price: 89 * P, stop_price: 90 * P, outside_rth: true },
                K::StopLimit { price: 89 * P, stop_price: 90 * P }, b'1', crate::types::OrderAttrs { outside_rth: true, ..Default::default() }),
            (OrderRequest::SubmitLimitIoc { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Limit { price: 100 * P }, b'3', Default::default()),
            (OrderRequest::SubmitLimitFok { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Limit { price: 100 * P }, b'4', Default::default()),
            (OrderRequest::SubmitLimitOpg { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Limit { price: 100 * P }, b'2', Default::default()),
            (OrderRequest::SubmitLimitAuc { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Limit { price: 100 * P }, b'8', Default::default()),
            (OrderRequest::SubmitMtlAuc { order_id: id, instrument: 0, side, qty }, K::Mtl, b'8', Default::default()),
            (OrderRequest::SubmitTrailingStop { order_id: id, instrument: 0, side, qty, trail_amt: P / 2, trail_stop_price: 90 * P },
                K::TrailingStop { trail_amt: P / 2, trail_stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitTrailingStopLimit { order_id: id, instrument: 0, side, qty, lmt_offset: P / 10, lmt_price: None, trail_amt: P / 2, trail_stop_price: 90 * P },
                K::TrailingStopLimit { lmt_offset: P / 10, lmt_price: None, trail_amt: P / 2, trail_stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitTrailingStopPct { order_id: id, instrument: 0, side, qty, trail_percent: px(1.5), trail_stop_price: 90 * P },
                K::TrailPct { trail_percent: px(1.5), trail_stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitMoc { order_id: id, instrument: 0, side, qty }, K::Moc, b'0', Default::default()),
            (OrderRequest::SubmitLoc { order_id: id, instrument: 0, side, qty, price: 100 * P }, K::Loc { price: 100 * P }, b'0', Default::default()),
            (OrderRequest::SubmitMit { order_id: id, instrument: 0, side, qty, stop_price: 90 * P }, K::Mit { stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitLit { order_id: id, instrument: 0, side, qty, price: 89 * P, stop_price: 90 * P },
                K::Lit { price: 89 * P, stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitRel { order_id: id, instrument: 0, side, qty, offset: P / 10 }, K::Rel { price: 0, offset: P / 10 }, b'0', Default::default()),
            (OrderRequest::SubmitMtl { order_id: id, instrument: 0, side, qty }, K::Mtl, b'0', Default::default()),
            (OrderRequest::SubmitMktPrt { order_id: id, instrument: 0, side, qty }, K::MktPrt, b'0', Default::default()),
            (OrderRequest::SubmitStpPrt { order_id: id, instrument: 0, side, qty, stop_price: 90 * P }, K::StpPrt { stop_price: 90 * P }, b'0', Default::default()),
            (OrderRequest::SubmitMidPrice { order_id: id, instrument: 0, side, qty, price_cap: 100 * P }, K::MidPrice { price_cap: 100 * P }, b'0', Default::default()),
            (OrderRequest::SubmitSnapMkt { order_id: id, instrument: 0, side, qty, offset: P / 20 }, K::SnapMkt { offset: P / 20 }, b'0', Default::default()),
            (OrderRequest::SubmitSnapMid { order_id: id, instrument: 0, side, qty, offset: P / 20 }, K::SnapMid { offset: P / 20 }, b'0', Default::default()),
            (OrderRequest::SubmitSnapPri { order_id: id, instrument: 0, side, qty, offset: P / 20 }, K::SnapPri { offset: P / 20 }, b'0', Default::default()),
            (OrderRequest::SubmitPegMkt { order_id: id, instrument: 0, side, qty, price: 100 * P, offset: P / 20 },
                K::PegMkt { price: 100 * P, offset: P / 20 }, b'0', Default::default()),
            (OrderRequest::SubmitPegMid { order_id: id, instrument: 0, side, qty, price: 100 * P, offset: 0 },
                K::PegMid { price: 100 * P, offset: 0 }, b'0', Default::default()),
            (OrderRequest::SubmitPegBench { order_id: id, instrument: 0, side, qty, price: 100 * P, ref_con_id: 7,
                is_peg_decrease: true, pegged_change_amount: P / 10, ref_change_amount: P / 5, stock_ref_price: 99 * P, ref_exchange: "ARCA".into() },
                K::PegBench { starting_price: 100 * P, stock_ref_price: 99 * P, ref_con_id: 7, is_peg_decrease: true,
                    pegged_change_amount: P / 10, ref_change_amount: P / 5 }, b'0',
                crate::types::OrderAttrs { reference_exchange: "ARCA".into(), ..Default::default() }),
            (OrderRequest::SubmitAdjustableStop { order_id: id, instrument: 0, side, qty, stop_price: 90 * P, trigger_price: 95 * P,
                adjusted_order_type: AdjustedOrderType::Trail, adjusted_stop_price: 91 * P, adjusted_stop_limit_price: 0,
                adjusted_trailing_amount: P / 2, adjustable_trailing_unit: 0 },
                K::AdjustableStop { stop_price: 90 * P, trigger_price: 95 * P, adjusted_order_type: AdjustedOrderType::Trail,
                    adjusted_stop_price: 91 * P, adjusted_stop_limit_price: 0, adjusted_trailing_amount: P / 2, adjustable_trailing_unit: 0 },
                b'0', Default::default()),
        ];
        for (plain, kind, tif, attrs) in cases {
            let label = format!("{plain:?}");
            let ours = order_body(wire_tags(plain));
            let ex = order_body(wire_tags(OrderRequest::SubmitEx { order_id: id, instrument: 0, side, qty, kind, tif, attrs }));
            assert_eq!(ours, ex, "{label}");
            let touched = touched_type(&ours.iter().map(|(t, v)| (*t, v.as_str())).collect::<Vec<_>>());
            let ranks: Vec<u16> = ours.iter().map(|(t, _)| rank_in_frame(*t, touched)).collect();
            assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{label}: {ours:?}");
            assert!(ranks.iter().all(|r| *r != u16::MAX), "{label}: a field with no place: {ours:?}");
        }
    }

    /// Fields the reference writes in no fixed order (its order
    /// attributes).
    fn is_attribute(tag: u32) -> bool {
        (70..100).contains(&reference_rank(tag))
    }

    /// The fields of `ours` that `captured` also has, attributes left out,
    /// in our order and in the captured order.
    fn common_order(ours: &[(u32, String)], captured: &str) -> (Vec<u32>, Vec<u32>) {
        let reference: Vec<u32> = parse_frame(captured).into_iter().map(|(t, _)| t)
            .filter(|t| !is_attribute(*t)).collect();
        let mine: Vec<u32> = ours.iter().map(|(t, _)| *t)
            .filter(|t| !is_attribute(*t) && reference.contains(t)).collect();
        let theirs: Vec<u32> = reference.into_iter().filter(|t| mine.contains(t)).collect();
        (mine, theirs)
    }

    // ibx#375: the fields come in the order of the captured reference
    // frames (ib-agent#192 B8, B9, B2, A5; the stop order of ibx#466).
    #[test]
    fn new_orders_follow_the_captured_field_order() {
        use crate::types::OrderKind as K;
        const MIDPX: &str = "35=D|11=1.0|44=237.82|1=DU1|6122=c|6121=74|6119=192|38=1|40=MIDPX|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const PEG_MID: &str = "35=D|11=1.0|44=237.82|1=DU1|6122=c|6121=76|6119=192|38=1|40=P|211=0.00|18=M|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const TRAIL: &str = "35=D|11=1.0|99=101.92|1=DU1|6115=0|6122=c|6268=0|6121=77|6119=192|38=1|40=P|211=101.92|18=a|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const TRAIL_LIMIT: &str = "35=D|11=1.0|99=101.92|1=DU1|6117=237.82|6115=0|6370=0.50|6122=c|6268=0|6121=78|6119=192|38=1|40=TSL|211=101.92|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const VWAP: &str = "35=D|11=1.0|44=237.82|1=DU1|6122=c|849=0.1|847=Vwap|5957=2|5958=noTakeLiq|5960=0|5958=allowPastEndTime|5960=1|6121=80|6119=192|38=1|40=2|18=e|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const CONDITION: &str = "35=D|11=1.0|44=237.82|1=DU1|6122=c|6121=55|6119=192|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|6128=0|6151=0|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=509.62|6223=|6245=|6263=|6246=|6947=|15=USD|6211=|6238=";
        const CHILD: &str = "35=D|11=2.0|44=509.62|1=DU1|583=1|6122=c|6121=39|6119=192|38=200|40=2|55=AAPL|167=STK|231=1.00|54=2|59=1|100=BEST|6210=BEST|6008=265598|6209=CancelOnFillWBlock|6088=Socket|6107=1.0|15=USD|6211=|6238=";
        const STP: &str = "35=D|11=1.0|99=235.14|1=DU1|6117=235.14|6010=x|6122=c|6115=0|6121=10|6119=250|38=1|40=3|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let ex = |side: Side, kind: K, tif: u8, attrs: crate::types::OrderAttrs| OrderRequest::SubmitEx {
            order_id: 1, instrument: 0, side, qty: 1, kind, tif, attrs };
        let condition = crate::types::OrderAttrs {
            conditions: vec![OrderCondition::Price { con_id: 265598, exchange: "SMART".into(), price: px(509.62), is_more: true, trigger_method: 0 }],
            ..Default::default()
        };
        let cases: Vec<(&str, OrderRequest)> = vec![
            (MIDPX, ex(Side::Buy, K::MidPrice { price_cap: px(237.82) }, b'0', Default::default())),
            (PEG_MID, ex(Side::Buy, K::PegMid { price: px(237.82), offset: 0 }, b'0', Default::default())),
            (TRAIL, ex(Side::Sell, K::TrailingStop { trail_amt: px(101.92), trail_stop_price: 0 }, b'0', Default::default())),
            (TRAIL_LIMIT, ex(Side::Sell, K::TrailingStopLimit { lmt_offset: px(0.5), lmt_price: None, trail_amt: px(101.92), trail_stop_price: px(237.82) }, b'0', Default::default())),
            (VWAP, OrderRequest::SubmitAlgo { order_id: 1, instrument: 0, side: Side::Buy, qty: 1, price: px(237.82),
                algo: AlgoParams::Vwap { max_pct_vol: 0.1, no_take_liq: false, allow_past_end_time: true, start_time: String::new(), end_time: String::new() },
                tif: b'0', attrs: Default::default() }),
            (CONDITION, ex(Side::Buy, K::Limit { price: px(237.82) }, b'0', condition)),
            // A5 set the OCA type to cancel on fill.
            (CHILD, ex(Side::Sell, K::Limit { price: px(509.62) }, b'1', crate::types::OrderAttrs { parent_id: 1, oca_type: 1, ..Default::default() })),
            (STP, ex(Side::Sell, K::Stop { stop_price: px(235.14) }, b'0', crate::types::OrderAttrs { order_ref: "x".into(), ..Default::default() })),
        ];
        for (captured, req) in cases {
            let ours = wire_tags(req);
            let (mine, theirs) = common_order(&ours, captured);
            assert!(mine.len() > 10, "{captured}");
            assert_eq!(mine, theirs, "{captured}");
        }
    }

    // ibx#263: maxPctVol goes out only when the order has it; a Vwap
    // without it got an invalid-value error on that field from the server for the
    // zero ibx sent (paper 05/10/2026).
    #[test]
    fn max_pct_vol_only_when_given() {
        let vwap = |max_pct_vol: f64| wire_tags(OrderRequest::SubmitAlgo { order_id: 1, instrument: 0, side: Side::Buy, qty: 1,
            price: px(237.82),
            algo: AlgoParams::Vwap { max_pct_vol, no_take_liq: false, allow_past_end_time: true,
                start_time: "09:00:00".into(), end_time: String::new() },
            tif: b'0', attrs: Default::default() });
        let without = vwap(0.0);
        assert_eq!(tag(&without, 849), None, "{without:?}");
        assert_eq!(tag(&without, 847), Some("Vwap"));
        assert_eq!(tag(&vwap(0.1), 849), Some("0.1"));
    }

    // ibx#466 (captured 25/09/2026, paper, AAPL, account masked): a new
    // order is the reference's frame field for field, the origin, the API
    // order and client ids, the multiplier, the source and the two empty
    // trailing fields included, and no field the reference does not write.
    #[test]
    fn new_order_is_the_captured_frame() {
        const LMT: &str = "35=D|11=7.0|44=337.93|1=DU1|6010=pm0925-fill-BUY|6122=c|6433=1|6121=7|6119=250|38=100|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let mut context = order_context();
        // The server id of the frame, from the order id generator.
        context.order_ids.start_at(7);
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "STK".into(), ..Default::default()
        });
        context.pending_orders.push(OrderRequest::SubmitLimitEx { order_id: 7, instrument: 0, side: Side::Buy, qty: 100,
            price: px(337.93), tif: b'0',
            attrs: crate::types::OrderAttrs { order_ref: "pm0925-fill-BUY".into(), outside_rth: true, ..Default::default() } });
        let shared = Arc::new(SharedState::new());
        shared.reference.set_api_client_id(250);
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        // The session fields and the message type out, the account masked on both sides.
        let n = crate::test_support::Normaliser::framing().keep_ids().drop(&[35]);
        let sent = peer.messages();
        assert_eq!(sent.len(), 1);
        crate::test_support::assert_same_fields(&n.msg(&sent[0]), &n.pipe(LMT));
    }

    // ibx#263 (captured 02/10/2026, paper, AAPL, account masked,
    // ib-agent captures/four-leg/20261002-b1 b1_263_algo_refusals, held
    // PreSubmitted by the server): an Adaptive STP goes out as a stop, its
    // stop price in the stop price field and the stop trigger, with the
    // Adaptive block and the algo instruction, the default trigger
    // method; no limit price. The reference's ClOrdID is its own
    // (1288736453.0 for API order 81): left out of the comparison.
    #[test]
    fn adaptive_stop_is_the_captured_frame() {
        const STP: &str = "35=D|11=1288736453.0|99=495.48|1=DUXXXXXXX|6117=495.48|6115=0|6122=c|6010=fourleg|847=Adaptive|5957=1|5958=adaptivePriority|5960=Normal|6121=81|6119=198|38=1|40=3|18=e|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        use crate::api::types::{Order as ApiOrder, TagValue};
        let order = ApiOrder { tif: "DAY".into(),
            action: "BUY".into(), total_quantity: 1.0, order_type: "STP".into(), aux_price: 495.48,
            algo_strategy: "Adaptive".into(), order_ref: "fourleg".into(),
            algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
            ..Default::default()
        };
        let Ok(crate::types::ControlCommand::Order(req)) = crate::client_core::ClientCore::build_order_request(&order, 81, 0) else {
            panic!("not an order");
        };
        assert!(matches!(req, OrderRequest::SubmitEx { kind: crate::types::OrderKind::Stop { .. }, .. }), "{req:?}");
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_min_tick(0, 0.01);
        context.pending_orders.push(req);
        let shared = Arc::new(SharedState::new());
        shared.reference.set_api_client_id(198);
        let (conn, mut peer) = crate::test_support::Peer::pair();
        let mut conn = Some(conn);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        // The reference's ClOrdID is its own: left out with the session fields.
        let n = crate::test_support::Normaliser::framing().drop(&[35, 11]);
        let ours = n.msg(&peer.messages()[0]);
        let want = n.pipe(STP);
        let (mine, theirs) = common_order(&ours, STP);
        assert_eq!(mine, theirs, "field order");
        let sorted = |mut v: Vec<(u32, String)>| { v.sort(); v };
        assert_eq!(sorted(ours), sorted(want));
    }

    // ibx#263: every algo keeps the order's own type, its fields and its
    // instruction letter, the algo letter after it (as the reference
    // builds the instruction field).
    #[test]
    fn algo_orders_keep_their_order_type() {
        use crate::types::{AdaptivePriority, OrderAlgo, OrderAttrs, OrderKind as K};
        let adaptive = || OrderAttrs { algo: Some(OrderAlgo::Adaptive(AdaptivePriority::Urgent)), ..Default::default() };
        let ex = |kind: K, attrs: OrderAttrs| OrderRequest::SubmitEx {
            order_id: 5, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'0', attrs };
        let stop_limit = wire_tags(ex(K::StopLimit { price: px(99.5), stop_price: px(100.0) }, adaptive()));
        assert_eq!((tag(&stop_limit, 40), tag(&stop_limit, 44), tag(&stop_limit, 99), tag(&stop_limit, 6117)),
            (Some("4"), Some("99.50"), Some("100.00"), Some("100.00")));
        assert_eq!((tag(&stop_limit, 18), tag(&stop_limit, 847), tag(&stop_limit, 5960)), (Some("e"), Some("Adaptive"), Some("Urgent")));
        let market = wire_tags(ex(K::Market, adaptive()));
        assert_eq!((tag(&market, 40), tag(&market, 44), tag(&market, 18)), (Some("1"), None, Some("e")));
        let trail = wire_tags(ex(K::TrailingStop { trail_amt: px(1.0), trail_stop_price: 0 }, adaptive()));
        assert_eq!((tag(&trail, 40), tag(&trail, 211), tag(&trail, 18)), (Some("P"), Some("1.00"), Some("a e")));
        let rel = wire_tags(ex(K::Rel { price: 0, offset: px(0.05) }, OrderAttrs { all_or_none: true, ..adaptive() }));
        assert_eq!((tag(&rel, 40), tag(&rel, 18)), (Some("P"), Some("R G e")));
        let bench = wire_tags(ex(K::PegBench { starting_price: px(100.0), stock_ref_price: 0, ref_con_id: 1,
            is_peg_decrease: false, pegged_change_amount: px(0.1), ref_change_amount: px(0.1) }, adaptive()));
        assert_eq!((tag(&bench, 40), tag(&bench, 18)), (Some("PB"), Some("e R")));
        // All-or-none on a limit, with and without an algo.
        let aon = |algo| wire_tags(ex(K::Limit { price: px(100.0) }, OrderAttrs { all_or_none: true, algo, ..Default::default() }));
        assert_eq!(tag(&aon(None), 18), Some("G"));
        assert_eq!(tag(&aon(Some(OrderAlgo::Adaptive(AdaptivePriority::Normal))), 18), Some("G e"));
        // One instruction field, never two.
        assert_eq!(rel.iter().filter(|(t, _)| *t == 18).count(), 1);
        // A trailing stop with an algo is still a trailing stop for the
        // price management flag.
        let trail: Vec<(u32, &str)> = trail.iter().map(|(t, v)| (*t, v.as_str())).collect();
        assert!(crate::engine::price_mgmt::excluded_frame(&trail));
    }

    // ibx#622 (paper 12/10/2026, .tmp/607 case b): an AccuDistr order
    // goes out with its parameters as given, in the reference's order, and
    // the algo block of the reference's recording.
    #[test]
    fn an_algo_outside_the_list_is_sent_as_the_reference_writes_it() {
        use crate::api::types::TagValue;
        use crate::types::{OrderAlgo, OrderAttrs, OrderKind as K};
        let given = [("componentSize", "1"), ("timeBetweenOrders", "60"), ("randomizeTime20", "0"),
            ("randomizeSize55", "0"), ("unset", "")];
        let params: Vec<TagValue> = given.iter()
            .map(|(t, v)| TagValue { tag: t.to_string(), value: v.to_string() }).collect();
        let algo = crate::api::client::parse_algo_params("AccuDistr", &params).unwrap();
        let attrs = OrderAttrs { algo: Some(OrderAlgo::Params(algo)), ..Default::default() };
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 5, instrument: 0, side: Side::Buy, qty: 1, kind: K::Limit { price: px(1.0) }, tif: b'0', attrs });
        let block: Vec<String> = tags.iter()
            .filter(|(t, _)| matches!(t, 847 | 849 | 5957 | 5958 | 5960))
            .map(|(t, v)| format!("{t}={v}")).collect();
        assert_eq!(block.join("|"), "847=AccuDistr|5957=4|5958=timeBetweenOrders|5960=60|5958=randomizeSize55|5960=0\
            |5958=componentSize|5960=1|5958=randomizeTime20|5960=0");
        assert_eq!((tag(&tags, 40), tag(&tags, 18)), (Some("2"), Some("e")));
    }

    // ibx#622: the reference's parameter order fits every algo block of
    // its recordings, and keeps the given order within a bin.
    #[test]
    fn the_reference_param_order_fits_its_recordings() {
        let order = |keys: &[&str]| {
            let mut pairs: Vec<(String, String)> = keys.iter().map(|k| (k.to_string(), String::new())).collect();
            crate::types::reference_param_order(&mut pairs);
            pairs.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        };
        assert_eq!(order(&["allowPastEndTime", "noTakeLiq"]), ["noTakeLiq", "allowPastEndTime"]);
        assert_eq!(order(&["forceCompletion", "allowPastEndTime", "riskAversion"]),
            ["riskAversion", "allowPastEndTime", "forceCompletion"]);
        assert_eq!(order(&["pctVol", "noTakeLiq"]), ["noTakeLiq", "pctVol"]);
        assert_eq!(order(&["displaySize", "allowPastEndTime"]), ["allowPastEndTime", "displaySize"]);
        assert_eq!(order(&["endTime", "startTime"]), ["startTime", "endTime"]);
        // maxPctVol and endTime share a bin: the given order is kept.
        assert_eq!(order(&["endTime", "maxPctVol"]), ["endTime", "maxPctVol"]);
        assert_eq!(order(&["maxPctVol", "endTime"]), ["maxPctVol", "endTime"]);
    }

    // ibx#263 (captured 23/09/2026, ib-agent#192 A2a and A2b, and
    // 28/09/2026, captures/0928; paper, AAPL, account masked): a new stop
    // order is the reference's frame, the default trigger method 0
    // included, through both encoders. The ids (ClOrdID, account, API order
    // and client ids) are left out.
    #[test]
    fn new_stop_orders_are_the_captured_frames() {
        use crate::types::{OrderAttrs, OrderKind as K};
        const STP: &str = "35=D|11=1626578555.0|99=237.82|1=DUXXXXXXX|6117=237.82|6115=0|6122=c|6121=7|6119=192|38=1|40=3|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const STP_LMT: &str = "35=D|11=1626578556.0|44=234.43|99=237.82|1=DUXXXXXXX|6117=237.82|6115=0|6122=c|6121=8|6119=192|38=1|40=4|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const TRAIL_PCT: &str = "35=D|11=1183455387.0|99=30.00|1=DUXXXXXXX|6010=c0928-a22_stp_trail|6115=0|6268=100|6122=c|6121=2|6119=261|38=1|40=P|211=30.00|18=a|55=AAPL|167=STK|231=1.00|54=2|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let (id, side, qty) = (7, Side::Sell, 1);
        let ex = |kind, tif, attrs| OrderRequest::SubmitEx { order_id: id, instrument: 0, side, qty, kind, tif, attrs };
        let cases: Vec<(&str, OrderRequest)> = vec![
            (STP, ex(K::Stop { stop_price: px(237.82) }, b'0', OrderAttrs::default())),
            (STP, OrderRequest::SubmitStop { order_id: id, instrument: 0, side, qty, stop_price: px(237.82) }),
            (STP_LMT, ex(K::StopLimit { price: px(234.43), stop_price: px(237.82) }, b'0', OrderAttrs::default())),
            (STP_LMT, OrderRequest::SubmitStopLimit { order_id: id, instrument: 0, side, qty, price: px(234.43), stop_price: px(237.82) }),
            (TRAIL_PCT, ex(K::TrailPct { trail_percent: px(30.0), trail_stop_price: 0 }, b'1',
                OrderAttrs { order_ref: "c0928-a22_stp_trail".into(), ..Default::default() })),
        ];
        let not_id = |(t, _): &(u32, String)| !matches!(t, 35 | 11 | 1 | 6121 | 6119);
        for (captured, req) in cases {
            let label = format!("{req:?}");
            let mut ours = order_body(wire_tags_with(|ctx| ctx.set_symbol(0, "AAPL".to_string()), req));
            ours.retain(not_id);
            let mut want = parse_frame(captured);
            want.retain(not_id);
            ours.sort();
            want.sort();
            assert_eq!(ours, want, "{label}");
        }
    }

    // ibx#263: the trigger method goes on a new order of the stop and
    // touched types only, the caller's value
    // or the default 0; never on the
    // other types, whatever the caller set. Captured with the default 0: STP,
    // STP LMT, TRAIL, TRAIL LIMIT, MIT, LIT (and an Adaptive STP and a
    // bracket's stop child); without: LMT, MKT, REL, PEG MID, PEG BENCH,
    // MIDPX and the snap types.
    #[test]
    fn trigger_method_goes_with_the_stop_types_only() {
        use crate::types::{AdjustedOrderType, OrderAttrs, OrderKind as K};
        let stops = [
            K::Stop { stop_price: px(90.0) },
            K::StopLimit { price: px(89.0), stop_price: px(90.0) },
            K::StpPrt { stop_price: px(90.0) },
            K::TrailingStop { trail_amt: px(0.5), trail_stop_price: 0 },
            K::TrailPct { trail_percent: px(1.5), trail_stop_price: 0 },
            K::TrailingStopLimit { lmt_offset: px(0.1), lmt_price: None, trail_amt: px(0.5), trail_stop_price: px(90.0) },
            K::Mit { stop_price: px(90.0) },
            K::Lit { price: px(89.0), stop_price: px(90.0) },
            K::AdjustableStop { stop_price: px(90.0), trigger_price: px(95.0), adjusted_order_type: AdjustedOrderType::Trail,
                adjusted_stop_price: px(91.0), adjusted_stop_limit_price: 0, adjusted_trailing_amount: px(0.5), adjustable_trailing_unit: 0 },
        ];
        let others = [
            K::Market,
            K::Limit { price: px(100.0) },
            K::Moc,
            K::Loc { price: px(100.0) },
            K::Mtl,
            K::MktPrt,
            K::MidPrice { price_cap: px(100.0) },
            K::SnapMkt { offset: px(0.05) },
            K::Rel { price: 0, offset: px(0.05) },
            K::PegMkt { price: px(100.0), offset: px(0.05) },
            K::PegMid { price: px(100.0), offset: 0 },
            K::PegBench { starting_price: px(100.0), stock_ref_price: 0, ref_con_id: 1, is_peg_decrease: false,
                pegged_change_amount: px(0.1), ref_change_amount: px(0.1) },
        ];
        let ex = |kind, attrs| OrderRequest::SubmitEx { order_id: 8, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'0', attrs };
        let kinds = stops.iter().map(|k| (*k, true)).chain(others.iter().map(|k| (*k, false)));
        for (kind, stop) in kinds {
            for (trigger_method, want) in [(0u8, "0"), (2, "2"), (8, "8")] {
                let tags = wire_tags(ex(kind, OrderAttrs { trigger_method, ..Default::default() }));
                assert_eq!(tag(&tags, 6115), stop.then_some(want), "{kind:?} {trigger_method}");
                assert!(tags.iter().filter(|(t, _)| *t == 6115).count() <= 1, "{kind:?}");
            }
        }
        // All-or-none keeps a trailing stop a trailing stop.
        let aon = OrderAttrs { all_or_none: true, ..Default::default() };
        let trail = wire_tags(ex(K::TrailingStop { trail_amt: px(0.5), trail_stop_price: 0 }, aon.clone()));
        assert_eq!((tag(&trail, 18), tag(&trail, 6115)), (Some("a G"), Some("0")));
        let rel = wire_tags(ex(K::Rel { price: 0, offset: px(0.05) }, aon));
        assert_eq!((tag(&rel, 18), tag(&rel, 6115)), (Some("R G"), None));
    }

    // ibx#263: a replace writes no trigger method (the reference writes it
    // on a new order only; no captured replace has it) and restates the
    // instruction field as the new order does (the reference's replace
    // builds it the same way): the type letter, then G for
    // all-or-none, after the trail field.
    #[test]
    fn replace_restates_all_or_none_and_no_trigger_method() {
        use crate::types::{OrderAttrs, OrderKind as K};
        let aon = OrderAttrs { all_or_none: true, trigger_method: 2, ..Default::default() };
        let cases = [
            (K::TrailingStop { trail_amt: px(1.0), trail_stop_price: 0 }, Some("a G")),
            (K::TrailPct { trail_percent: px(1.5), trail_stop_price: 0 }, Some("a G")),
            (K::Rel { price: 0, offset: px(0.05) }, Some("R G")),
            (K::Limit { price: px(100.0) }, Some("G")),
            (K::Stop { stop_price: px(90.0) }, Some("G")),
            (K::PegMid { price: px(100.0), offset: 0 }, Some("M G")),
        ];
        for (kind, want) in cases {
            let ours = replace_fields(61, Side::Sell, 1, kind, b'0', aon.clone());
            assert_eq!((tag(&ours, 18), tag(&ours, 6115)), (want, None), "{kind:?}");
            assert_eq!(ours.iter().filter(|(t, _)| *t == 18).count(), 1, "{kind:?}");
            let at = |tag: u32| ours.iter().position(|(t, _)| *t == tag);
            assert!(at(40) < at(18) && at(211) < at(18) && at(18) < at(55), "{kind:?}: {ours:?}");
        }
        // Without all-or-none, as before.
        let rel = replace_fields(62, Side::Sell, 1, K::Rel { price: 0, offset: px(0.05) }, b'0', OrderAttrs::default());
        assert_eq!(tag(&rel, 18), Some("R"));
        let lmt = replace_fields(63, Side::Sell, 1, K::Limit { price: px(100.0) }, b'0', OrderAttrs::default());
        assert_eq!(tag(&lmt, 18), None);
    }

    // ibx#263 (captured 23/09/2026, ib-agent#192 B8d and A3b, and
    // 28/09/2026, ib-agent#199; paper, AAPL, account masked): the trail
    // unit 0 (amount) of an amount TRAIL and a TRAIL LIMIT, the touched trigger
    // of MIT and LIT (their stop price, among the attributes), and the
    // price cap of a REL (the limit price), through both encoders. The ids (ClOrdID,
    // account, API order and client ids) are left out.
    #[test]
    fn new_trailing_touched_and_relative_orders_are_the_captured_frames() {
        use crate::types::{OrderAttrs, OrderKind as K};
        const TRAIL: &str = "35=D|11=1626578557.0|99=101.92|1=DUXXXXXXX|6115=0|6122=c|6268=0|6121=9|6119=192|38=1|40=P|211=101.92|18=a|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const TRAIL_LIMIT: &str = "35=D|11=1626578568.0|99=101.92|1=DUXXXXXXX|6117=237.82|6115=0|6370=0.50|6122=c|6268=0|6121=29|6119=192|38=1|40=TSL|211=101.92|55=AAPL|167=STK|231=1.00|54=2|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const MIT: &str = "35=D|11=117065670.0|99=450.00|1=DUXXXXXXX|6115=0|6010=c199-mit_gtc|6122=c|6117=450.00|6121=5|6119=199|38=1|40=J|55=AAPL|167=STK|231=1.00|54=2|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const LIT: &str = "35=D|11=117065671.0|44=450.00|99=450.00|1=DUXXXXXXX|6115=0|6010=c199-lit_gtc|6122=c|6433=1|6117=450.00|8339=1|6121=6|6119=199|38=1|40=LT|55=AAPL|167=STK|231=1.00|54=2|59=1|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        const REL: &str = "35=D|11=117065675.0|44=250.00|1=DUXXXXXXX|6010=c199-rel_day|6122=c|6433=1|8339=1|6121=10|6119=199|38=1|40=P|211=0.01|18=R|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        let qty = 1;
        let ex = |side, kind, tif, attrs| OrderRequest::SubmitEx { order_id: 7, instrument: 0, side, qty, kind, tif, attrs };
        let order_ref = |r: &str, outside_rth: bool| OrderAttrs { order_ref: r.into(), outside_rth, ..Default::default() };
        let trail = K::TrailingStop { trail_amt: px(101.92), trail_stop_price: 0 };
        let trail_limit = K::TrailingStopLimit { lmt_offset: px(0.5), lmt_price: None, trail_amt: px(101.92), trail_stop_price: px(237.82) };
        let cases: Vec<(&str, bool, OrderRequest)> = vec![
            (TRAIL, false, ex(Side::Sell, trail, b'0', OrderAttrs::default())),
            (TRAIL, false, OrderRequest::SubmitTrailingStop { order_id: 7, instrument: 0, side: Side::Sell, qty, trail_amt: px(101.92), trail_stop_price: 0 }),
            (TRAIL_LIMIT, false, ex(Side::Sell, trail_limit, b'0', OrderAttrs::default())),
            (TRAIL_LIMIT, false, OrderRequest::SubmitTrailingStopLimit { order_id: 7, instrument: 0, side: Side::Sell, qty,
                lmt_offset: px(0.5), lmt_price: None, trail_amt: px(101.92), trail_stop_price: px(237.82) }),
            (MIT, false, ex(Side::Sell, K::Mit { stop_price: px(450.0) }, b'1', order_ref("c199-mit_gtc", false))),
            (LIT, true, ex(Side::Sell, K::Lit { price: px(450.0), stop_price: px(450.0) }, b'1', order_ref("c199-lit_gtc", true))),
            (REL, true, ex(Side::Buy, K::Rel { price: px(250.0), offset: px(0.01) }, b'0', order_ref("c199-rel_day", true))),
        ];
        let not_id = |(t, _): &(u32, String)| !matches!(t, 35 | 11 | 1 | 6121 | 6119);
        for (captured, price_mgmt, req) in cases {
            let label = format!("{req:?}");
            let sent = order_body(wire_tags_with(|ctx| session(ctx, price_mgmt), req));
            let (mine, theirs) = common_order(&sent, captured);
            assert_eq!(mine, theirs, "{label}: field order");
            let mut ours = sent;
            ours.retain(not_id);
            let mut want = parse_frame(captured);
            want.retain(not_id);
            ours.sort();
            want.sort();
            assert_eq!(ours, want, "{label}");
        }
        // The MIT's touched trigger is among the attributes, after
        // outside-RTH, not after the account like a stop's.
        let lit = wire_tags(ex(Side::Sell, K::Lit { price: px(450.0), stop_price: px(450.0) }, b'1', order_ref("x", true)));
        let at = |tag: u32| lit.iter().position(|(t, _)| *t == tag);
        assert!(at(6122) < at(6117) && at(6433) < at(6117), "{lit:?}");
        // A REL without a price cap has no limit price.
        let rel = wire_tags(ex(Side::Buy, K::Rel { price: 0, offset: px(0.01) }, b'0', OrderAttrs::default()));
        assert_eq!(tag(&rel, 44), None);
    }

    /// AAPL, and when asked a session with price management on and a
    /// definition that keeps outside RTH.
    fn session(ctx: &mut Context, price_mgmt: bool) {
        ctx.set_symbol(0, "AAPL".to_string());
        if price_mgmt {
            price_mgmt_session(ctx);
            ctx.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
                rth: true, sec_type: "STK".into(), types_known: true, price_chk: true, ..Default::default()
            });
        }
    }

    /// Send one Modify as `replace_fields`, in `session`.
    fn replace_fields_mgmt(order_id: OrderId, side: Side, kind: crate::types::OrderKind, tif: u8,
                           attrs: crate::types::OrderAttrs, price_mgmt: bool) -> Vec<(u32, String)> {
        wire_tags_with(
            |ctx| {
                session(ctx, price_mgmt);
                ctx.insert_order(Order::new(order_id, 0, side, 1, 0, b'2', b'0', 0));
            },
            OrderRequest::Modify { new_order_id: order_id, order_id, qty: 1, kind, tif, attrs },
        )
        .into_iter()
        .filter(|(t, _)| !crate::test_support::normalise::FRAMING.contains(t))
        .collect()
    }

    // ibx#263 (captured 28/09/2026, ib-agent#199, paper, AAPL, account
    // masked): a replace of MIT and LIT restates the touched trigger
    // among the attributes, a replace of REL its price cap; field for
    // field, in the captured order.
    #[test]
    fn replace_of_touched_and_relative_orders_matches_reference() {
        use crate::types::{OrderAttrs, OrderKind as K};
        const MIT: &str = "35=G|11=117065670.1|41=117065670.0|99=451.00|1=DUXXXXXXX|6010=c199-mit_gtc|6122=c|6117=451.00|38=1|54=2|40=J|55=AAPL|167=STK|6035=AAPL|59=1|6008=265598|6088=Socket|6211=|6238=";
        const LIT: &str = "35=G|11=117065671.1|41=117065671.0|44=450.00|99=451.00|1=DUXXXXXXX|6010=c199-lit_gtc|6122=c|6433=1|6117=451.00|8339=1|38=1|54=2|40=LT|55=AAPL|167=STK|6035=AAPL|59=1|6008=265598|6088=Socket|6211=|6238=";
        const REL: &str = "35=G|11=117065675.1|41=117065675.0|44=249.00|1=DUXXXXXXX|6010=c199-rel_day|6122=c|6433=1|8339=1|38=1|54=1|40=P|211=0.01|18=R|55=AAPL|167=STK|6035=AAPL|59=0|6008=265598|6088=Socket|6211=|6238=";
        let attrs = |r: &str, outside_rth: bool| OrderAttrs { order_ref: r.into(), outside_rth, ..Default::default() };
        let cases = [
            (MIT, 117065670, Side::Sell, K::Mit { stop_price: px(451.0) }, b'1', attrs("c199-mit_gtc", false), false),
            (LIT, 117065671, Side::Sell, K::Lit { price: px(450.0), stop_price: px(451.0) }, b'1', attrs("c199-lit_gtc", true), true),
            (REL, 117065675, Side::Buy, K::Rel { price: px(249.0), offset: px(0.01) }, b'0', attrs("c199-rel_day", true), true),
        ];
        for (captured, id, side, kind, tif, attrs, price_mgmt) in cases {
            let ours = replace_fields_mgmt(id, side, kind, tif, attrs, price_mgmt);
            assert_same_replace(&ours, captured);
        }
    }

    // ibx#263: a replace of an algo order restates the algo letter in the instruction field
    // and the algo block after the attributes, as the new order: the
    // reference's replace writes them the same way as its new order
    //. Not captured.
    #[test]
    fn replace_of_an_algo_order_restates_the_algo() {
        use crate::types::{AdaptivePriority, OrderAlgo, OrderAttrs, OrderKind as K};
        let attrs = OrderAttrs { algo: Some(OrderAlgo::Adaptive(AdaptivePriority::Normal)), ..Default::default() };
        let stop = replace_fields(64, Side::Buy, 1, K::Stop { stop_price: px(495.48) }, b'0', attrs.clone());
        let tags: Vec<u32> = stop.iter().map(|(t, _)| *t).collect();
        assert_eq!(tags, vec![35, 11, 41, 99, 1, 6117, 6122, 847, 5957, 5958, 5960, 38, 54, 40, 18, 55, 167, 6035, 59, 6008, 6088, 6211, 6238]);
        assert_eq!((tag(&stop, 18), tag(&stop, 847), tag(&stop, 5958), tag(&stop, 5960)),
            (Some("e"), Some("Adaptive"), Some("adaptivePriority"), Some("Normal")));
        let aon = replace_fields(65, Side::Buy, 1, K::Limit { price: px(100.0) }, b'0', OrderAttrs { all_or_none: true, ..attrs });
        assert_eq!(tag(&aon, 18), Some("G e"));
    }

    /// The API order a captured gateway order frame was placed from, read
    /// back from the frame; None for a type or contract ibx does not
    /// place (combos).
    fn api_order_of_frame(f: &[(u32, String)]) -> Option<crate::api::types::Order> {
        use crate::api::types::{Order as ApiOrder, TagValue};
        let get = |tag: u32| f.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_str());
        let num = |tag: u32| get(tag).and_then(|v| v.parse::<f64>().ok());
        if get(167) != Some("STK") { return None; }
        let inst = get(18).unwrap_or("");
        let order_type = match (get(40)?, inst.split(' ').next().unwrap_or("")) {
            ("1", _) => "MKT", ("2", _) => "LMT", ("3", _) => "STP", ("4", _) => "STP LMT",
            ("5", _) => "MOC", ("B", _) => "LOC", ("K", _) => "MTL", ("U", _) => "MKT PRT",
            ("J", _) => "MIT", ("LT", _) => "LIT", ("SP", _) => "STP PRT", ("TSL", _) => "TRAIL LIMIT",
            ("MIDPX", _) => "MIDPRICE", ("SMKT", _) => "SNAP MKT", ("SMID", _) => "SNAP MID",
            ("SREL", _) => "SNAP PRIM", ("PB", _) => "PEG BENCH",
            ("TMIT", _) => "TRAIL MIT", ("TLIT", _) => "TRAIL LIT", ("E2M", _) => "PEG BEST", ("RPI", _) => "RPI",
            ("P", "a") => "TRAIL", ("P", "R") => "REL", ("P", "M") => "PEG MID", ("P", "P") => "PEG MKT",
            _ => return None,
        };
        let mut o = ApiOrder {
            action: match get(54)? { "1" => "BUY", "5" => "SSHORT", _ => "SELL" }.into(),
            total_quantity: num(38)?,
            order_type: order_type.into(),
            tif: match get(59)? {
                "0" => "DAY", "1" => "GTC", "2" => "OPG", "3" => "IOC", "4" => "FOK", "6" => "GTD", "8" => "AUC",
                _ => return None,
            }.into(),
            outside_rth: get(6433) == Some("1"),
            order_ref: get(6010).unwrap_or("").into(),
            all_or_none: inst.split(' ').any(|p| p == "G"),
            display_size: num(111).map_or(0, |v| v as i32),
            hidden: get(6135) == Some("1"),
            include_overnight: get(8534) == Some("1"),
            what_if: get(6091) == Some("1"),
            trigger_method: num(6115).map_or(0, |v| v as i32),
            discretionary_amt: num(9813).unwrap_or(0.0),
            ..Default::default()
        };
        if get(6436) == Some("1") { o.tif = "DTC".into(); }
        if let Some(t) = get(126) { o.good_till_date = format!("{} UTC", t.replace('-', " ")); }
        if let Some(t) = get(168) { o.good_after_time = format!("{} UTC", t.replace('-', " ")); }
        match order_type {
            "LMT" | "LOC" | "MIDPRICE" => o.lmt_price = num(44).unwrap_or(0.0),
            "STP" | "MIT" | "STP PRT" => o.aux_price = num(99)?,
            "STP LMT" | "LIT" => { o.lmt_price = num(44)?; o.aux_price = num(99)?; }
            "TRAIL" | "TRAIL MIT" => {
                if get(6268) == Some("100") { o.trailing_percent = num(99)?; } else { o.aux_price = num(99)?; }
                if let Some(v) = num(6117) { o.trail_stop_price = v; }
            }
            "TRAIL LIT" => {
                o.lmt_price = num(44)?;
                o.aux_price = num(99)?;
                if let Some(v) = num(6117) { o.trail_stop_price = v; }
            }
            "PEG BEST" => o.lmt_price = num(44).unwrap_or(0.0),
            "RPI" => { o.lmt_price = num(44)?; o.aux_price = num(99).unwrap_or(0.0); }
            "TRAIL LIMIT" => {
                o.aux_price = num(99)?;
                match num(44) { Some(v) => o.lmt_price = v, None => o.lmt_price_offset = num(6370)? }
                if let Some(v) = num(6117) { o.trail_stop_price = v; }
            }
            "REL" | "PEG MID" | "PEG MKT" => {
                if let Some(v) = num(44) { o.lmt_price = v; }
                o.aux_price = num(211).unwrap_or(0.0);
            }
            "SNAP MKT" | "SNAP MID" | "SNAP PRIM" => o.aux_price = num(99).unwrap_or(0.0),
            "PEG BENCH" => {
                if let Some(v) = num(99) { o.starting_price = v; }
                if let Some(v) = num(6580) { o.stock_ref_price = v; }
                let change = num(6938).unwrap_or(0.0);
                o.is_pegged_change_amount_decrease = change < 0.0;
                o.pegged_change_amount = change.abs();
                o.reference_change_amount = num(6939).unwrap_or(0.0);
                o.reference_contract_id = num(6941).map_or(0, |v| v as i32);
                o.reference_exchange_id = get(6942).unwrap_or("").into();
            }
            _ => {}
        }
        if get(6257) == Some("1") {
            o.adjusted_order_type = match get(6261)? { "3" => "STP", "4" => "STP LMT", "T" => "TRAIL", _ => "TRAIL LIMIT" }.into();
            o.trigger_price = num(6258)?;
            o.adjusted_stop_price = num(6259).unwrap_or(0.0);
            o.adjusted_stop_limit_price = num(6262).unwrap_or(0.0);
            if let Some(v) = num(6260) { o.adjusted_trailing_amount = v; }
            o.adjustable_trailing_unit = num(6269).map_or(0, |v| v as i32);
        }
        match (get(6107), get(583)) {
            (Some(parent), _) => o.parent_id = parent.split('.').next()?.parse().ok()?,
            (None, Some(group)) => {
                o.oca_group = group.into();
                o.oca_type = match get(6209) { Some("CancelOnFillWBlock") => 1, Some("ReduceOnFillWBlock") => 2, _ => 3 };
            }
            _ => {}
        }
        if let Some(strategy) = get(847) {
            o.algo_strategy = strategy.into();
            let keys = f.iter().filter(|(t, _)| *t == 5958).map(|(_, v)| v.clone());
            let values = f.iter().filter(|(t, _)| *t == 5960).map(|(_, v)| v.clone());
            o.algo_params = keys.zip(values).map(|(tag, value)| TagValue { tag, value }).collect();
            if let Some(v) = get(849) { o.algo_params.push(TagValue { tag: "maxPctVol".into(), value: v.into() }); }
        }
        // A price condition; the other kinds carry no price and are left out.
        if get(6136).is_some() && get(6222) == Some("1") {
            o.conditions_ignore_rth = get(6128) == Some("1");
            o.conditions_cancel_order = get(6151) == Some("1");
            o.conditions = vec![OrderCondition::Price {
                con_id: num(6123)? as i64,
                exchange: match get(6124)? { "BEST" => "SMART", e => e }.into(),
                price: crate::api::types::price_from_f64(num(6125)?),
                is_more: get(6126) == Some(">="),
                trigger_method: num(6127).map_or(0, |v| v as u8),
            }];
        }
        Some(o)
    }

    // ibx#263: the reference writes every price of its order messages with
    // one formatter (`#0.00######`, HALF_EVEN, US symbols):
    // `0.50`, `450.00`, `-0.10`. Every gateway new order and replace on disk
    // (tests/fixtures/gw1040/order_frames, from the ib-agent captures of
    // 23/09 to 02/10/2026) is read back into its API order, encoded by ibx,
    // and each price field compared as text. A trailing replace's stop
    // price depends on the last server report and is checked on its own.
    #[test]
    fn every_captured_order_frame_has_the_reference_price_text() {
        const PRICE_TAGS: [u32; 15] = [44, 99, 6117, 211, 6370, 9813, 6125, 6258, 6259, 6260, 6262, 6580, 6938, 6939, 231];
        let path = format!("{}/tests/fixtures/gw1040/order_frames/frames.tsv", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap();
        let (mut checked, mut skipped) = (0, Vec::new());
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let (src, frame) = line.split_once('\t').unwrap();
            let captured: Vec<(u32, String)> = frame.split('|')
                .filter_map(|kv| { let (t, v) = kv.split_once('=')?; Some((t.parse().ok()?, v.to_string())) })
                .collect();
            let get = |tags: &[(u32, String)], tag: u32| tags.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone());
            let Some(order) = api_order_of_frame(&captured) else {
                skipped.push(format!("{} {}", get(&captured, 40).unwrap_or_default(), get(&captured, 167).unwrap_or_default()));
                continue;
            };
            let replace = get(&captured, 35).as_deref() == Some("G");
            let id: OrderId = 70;
            let req = if replace {
                match crate::client_core::ClientCore::build_modify_request(&order, id, &order, false) {
                    Ok(crate::client_core::ModifyPlan::Send(crate::types::ControlCommand::Order(r))) => r,
                    Ok(crate::client_core::ModifyPlan::Refused { code, message }) => panic!("{src}: {frame}: {code} {message}"),
                    Ok(_) => panic!("{src}: {frame}: not a replace"),
                    Err(e) => panic!("{src}: {frame}: {e}"),
                }
            } else {
                match crate::client_core::ClientCore::build_order_request(&order, id, 0) {
                    Ok(crate::types::ControlCommand::Order(r)) => r,
                    other => panic!("{src}: {frame}: {other:?}"),
                }
            };
            let side = if order.action == "BUY" { Side::Buy } else { Side::Sell };
            let symbol = get(&captured, 55).unwrap_or_default();
            let limit_and_offset = (get(&captured, 44), get(&captured, 6370));
            let ours = wire_tags_with(|ctx| {
                ctx.set_symbol(0, symbol);
                if replace {
                    ctx.insert_order(Order::new(id, 0, side, 1, 0, b'2', b'0', 0));
                    // A TRAIL LIMIT replace restates the offset the server
                    // reported (ib-agent#194).
                    if let (Some(limit), Some(offset)) = limit_and_offset {
                        let px = |v: String| crate::api::types::price_from_f64(v.parse().unwrap());
                        ctx.trail_limit_reported.insert(id, crate::engine::context::TrailLimitReported {
                            offset: px(offset), limit: px(limit), stop: 0,
                        });
                    }
                }
            }, req);
            let trailing = matches!(get(&captured, 40).as_deref(), Some("TSL" | "TMIT" | "TLIT"))
                || get(&captured, 18).is_some_and(|v| v.starts_with('a'));
            for t in PRICE_TAGS {
                if replace && trailing && t == 6117 { continue; }
                // A condition other than a price one (left out of the order).
                if t == 6125 && get(&captured, 6222).as_deref() != Some("1") { continue; }
                assert_eq!(get(&ours, t), get(&captured, t), "tag {t}, {src}:\n  captured {frame}\n  ours {ours:?}");
            }
            checked += 1;
        }
        println!("{checked} frames checked, skipped: {skipped:?}");
        assert!(skipped.iter().all(|s| s.ends_with("BAG")), "{skipped:?}");
        assert!(checked >= 210, "{checked}");
    }

    // ibx#263 (captured 25/09/2026, ib-agent#195 S2; account masked): a
    // TRAIL LIMIT replace restates the stop price when the server last
    // reported another one (it had moved the stop to 752.01, the order
    // still had 751.89), and not when it is the same or before any report
    //.
    #[test]
    fn trailing_replace_restates_the_stop_price_only_when_the_server_moved_it() {
        use crate::types::{OrderAttrs, OrderKind as K};
        const REPLACE: &str = "35=G|11=1941360730.1|41=1941360730.0|44=747.11|99=20.00|1=DU1|6117=751.89|6205=1|6268=0|6122=c|6370=5.00|38=1|54=2|40=TSL|211=20.00|55=SPY|167=STK|6035=SPY|59=0|6008=756733|6088=Socket|6211=|6238=";
        let kind = K::TrailingStopLimit { lmt_offset: 0, lmt_price: Some(px(747.11)), trail_amt: px(20.0), trail_stop_price: px(751.89) };
        let replace = |reported: Option<f64>| wire_tags_with(|ctx| {
            ctx.market.register(756733);
            ctx.set_symbol(1, "SPY".to_string());
            ctx.insert_order(Order::new(1941360730, 1, Side::Sell, 1, 0, b'2', b'0', 0));
            ctx.trail_limit_reported.insert(1941360730, crate::engine::context::TrailLimitReported {
                offset: px(5.0), limit: px(746.89), stop: px(751.89),
            });
            if let Some(stop) = reported { ctx.reported_stop.insert(1941360730, px(stop)); }
        }, OrderRequest::Modify { new_order_id: 1941360730, order_id: 1941360730, qty: 1, kind, tif: b'0', attrs: OrderAttrs::default() })
            .into_iter().filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 10)).collect::<Vec<_>>();
        // The attributes come in the reference's hash order: compared as a
        // set, the other fields in order.
        let ours = replace(Some(752.01));
        let (mine, theirs) = common_order(&ours, REPLACE);
        assert_eq!(mine, theirs);
        let sorted = |mut v: Vec<(u32, String)>| { v.sort(); v };
        assert_eq!(sorted(ours), sorted(parse_frame(REPLACE)));
        assert_eq!(tag(&replace(Some(751.89)), 6117), None, "same as the server's");
        assert_eq!(tag(&replace(None), 6117), None, "before any report");
        // A trailing amount and percent the same way; STP types always.
        let trail = |kind, reported: f64| wire_tags_with(|ctx| {
            ctx.insert_order(Order::new(5, 0, Side::Sell, 1, 0, b'2', b'0', 0));
            ctx.reported_stop.insert(5, px(reported));
        }, OrderRequest::Modify { new_order_id: 5, order_id: 5, qty: 1, kind, tif: b'0', attrs: OrderAttrs::default() });
        let amount = K::TrailingStop { trail_amt: px(1.0), trail_stop_price: px(99.0) };
        assert_eq!(tag(&trail(amount, 98.5), 6117), Some("99.00"));
        assert_eq!(tag(&trail(amount, 99.0), 6117), None);
        let pct = K::TrailPct { trail_percent: px(1.5), trail_stop_price: px(99.0) };
        assert_eq!(tag(&trail(pct, 98.5), 6117), Some("99.00"));
        assert_eq!(tag(&trail(K::StpPrt { stop_price: px(90.0) }, 0.0), 6117), Some("90.00"));
    }

    // ibx#263: the cash quantity is the reference's cash quantity attribute
    // (a double attribute
    // written with the price formatter); 5920 is only its column id.
    #[test]
    fn cash_quantity_goes_out_in_152() {
        let tags = wire_tags(OrderRequest::SubmitEx {
            order_id: 9, instrument: 0, side: Side::Buy, qty: 0, kind: crate::types::OrderKind::Market, tif: b'0',
            attrs: crate::types::OrderAttrs { cash_qty: px(1000.0), ..Default::default() },
        });
        assert_eq!(tag(&tags, 152), Some("1000.00"));
        assert_eq!(tag(&tags, 5920), None);
        assert!((70..100).contains(&reference_rank(152)), "an order attribute");
    }

    // ibx#263: a price off the contract's grid is refused with 110 and
    // nothing is sent, as the reference (ib-agent#192 B5); ibx snapped it
    // to the tick (ibx#216).
    #[test]
    fn a_price_off_the_grid_is_refused_with_110() {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_nonblocking(true).unwrap();
        let mut context = order_context();
        context.market.register(265598);
        context.market.set_min_tick(0, 0.01);
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 31, instrument: 0, side: Side::Buy, qty: 1, price: px(100.005) });
        context.pending_orders.push(OrderRequest::SubmitEx { order_id: 32, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Rel { price: 0, offset: px(-0.5) }, tif: b'0', attrs: Default::default() });
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let mut buf = [0u8; 256];
        assert!(server.read(&mut buf).is_err(), "nothing sent");
        let errors = shared.orders.drain_order_errors();
        let text = "The price does not conform to the minimum price variation for this contract.".to_string();
        assert_eq!(errors, vec![(31, 110, text.clone()), (32, 110, text)]);
        assert!(context.order(31).is_none() && context.order(32).is_none());
    }

    // ibx#466: a new order goes out under a server id of the reference's
    // order id generator, as the reference's ClOrdID is its permId
    //, with its API order id in its own field; two
    // orders get two ids. Its replace and its cancel go under the same
    // server id.
    #[test]
    fn a_new_order_goes_out_under_a_server_id_of_the_generator() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        context.pending_orders.push(limit_ex(7, Side::Buy, 100 * P, Default::default()));
        context.pending_orders.push(limit_ex(8, Side::Buy, 100 * P, Default::default()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let ids: Vec<_> = frames.iter().map(|f| (tag(f, 11).unwrap().to_string(), tag(f, 6121).unwrap().to_string())).collect();
        assert_eq!(ids, [(format!("{FIRST}.0"), "7".to_string()), (format!("{}.0", FIRST + 1), "8".to_string())]);
        assert!(context.order(7).is_some() && context.order(FIRST).is_none(), "held under the API order id");

        context.pending_orders.push(modify_limit(7, 101 * P, Default::default()));
        context.pending_orders.push(OrderRequest::Cancel { order_id: 7 });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let ids: Vec<_> = frames.iter().map(|f| (tag(f, 35).unwrap(), tag(f, 11).unwrap(), tag(f, 41).unwrap())).collect();
        let (v0, v1, v2) = (format!("{FIRST}.0"), format!("{FIRST}.1"), format!("{FIRST}.2"));
        assert_eq!(ids, [("G", v1.as_str(), v0.as_str()), ("F", v2.as_str(), v1.as_str())]);
    }

    // ibx#466: the API order id is an int; a larger order id (an order of
    // the engine's own interface, not of the API) is not sent.
    #[test]
    fn an_order_id_beyond_the_api_range_is_not_sent() {
        let tags = wire_tags(OrderRequest::SubmitMarket { order_id: 1_790_000_000_000, instrument: 0, side: Side::Buy, qty: 1 });
        assert!(tag(&tags, 6121).is_none());
        assert_eq!(tag(&tags, 6119), Some("0"));
        assert_eq!(tag(&tags, 6122), Some("c"));
    }

    // ibx#466: the stop child of a bracket carries the stop trigger, as
    // every stop order.
    #[test]
    fn bracket_stop_child_carries_the_stop_trigger() {
        let frames = wire_frames(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        }, 3);
        assert_eq!(tag(&frames[2], 6117), tag(&frames[2], 99));
        assert!(tag(&frames[1], 6117).is_none());
    }

    // A fractional quantity is refused with 10243 and nothing is sent, as
    // the reference refuses it (ib-agent#192 B3); a whole one goes out.
    #[test]
    fn fractional_quantity_is_refused_with_10243() {
        let frac = |order_id, qty| OrderRequest::SubmitLimitFractional { order_id, instrument: 0, side: Side::Buy, qty, price: P };
        let (frames, errors) = short_sale_run(false, false, "DU1", frac(1, crate::types::QTY_SCALE / 2));
        assert!(frames.is_empty(), "nothing sent: {frames:?}");
        assert_eq!(errors, vec![(1, 10243, crate::client_core::FRACTIONAL_VIA_API.1.to_string())]);
        let (frames, errors) = short_sale_run(false, false, "DU1", frac(2, 2 * crate::types::QTY_SCALE));
        assert!(errors.is_empty());
        assert_eq!(tag(&frames[0], 38), Some("2"));
    }

    /// Encode several requests in one session and return the sent frames
    /// (frame count given) and the engine state after them.
    fn session_frames(
        setup: impl FnOnce(&mut Context),
        reqs: Vec<OrderRequest>,
        frames: usize,
    ) -> (Vec<Vec<(u32, String)>>, Context) {
        use std::io::Read;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut context = order_context();
        context.market.register(265598);
        setup(&mut context);
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        for req in reqs {
            context.pending_orders.push(req);
            drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        }
        let mut out: Vec<Vec<(u32, String)>> = Vec::new();
        let mut buf = vec![0u8; 65536];
        let mut len = 0;
        while out.len() < frames {
            let n = server.read(&mut buf[len..]).unwrap();
            assert!(n > 0, "connection closed");
            len += n;
            out = buf[..len].split(|&b| b == fix::SOH)
                .filter_map(|f| {
                    let s = std::str::from_utf8(f).ok()?;
                    let (t, v) = s.split_once('=')?;
                    Some((t.parse::<u32>().ok()?, v.to_string()))
                })
                .fold(Vec::new(), |mut acc: Vec<Vec<(u32, String)>>, field| {
                    if field.0 == 8 { acc.push(Vec::new()); }
                    if let Some(last) = acc.last_mut() { last.push(field); }
                    acc
                });
            out.retain(|f| f.iter().any(|(t, _)| *t == 10));
        }
        (out, context)
    }

    fn limit_ex(order_id: OrderId, side: Side, price: i64, attrs: crate::types::OrderAttrs) -> OrderRequest {
        OrderRequest::SubmitLimitEx { order_id, instrument: 0, side, qty: 1, price, tif: b'1', attrs }
    }

    fn modify_limit(order_id: OrderId, price: i64, attrs: crate::types::OrderAttrs) -> OrderRequest {
        OrderRequest::Modify {
            new_order_id: order_id, order_id, qty: 1,
            kind: crate::types::OrderKind::Limit { price }, tif: b'1', attrs,
        }
    }

    fn key_of(frame: &[(u32, String)]) -> Option<crate::engine::bracket::BracketKey> {
        tag(frame, 6531).and_then(crate::engine::bracket::BracketKey::parse)
    }

    // ibx#248: the bracket of three gets one key, the parent index 0 and
    // the children 1 and 2, right after the origin field, as the
    // reference's bracket (ib-agent captures/192 A5, pd-orders).
    #[test]
    fn bracket_orders_carry_one_key_with_their_index() {
        let frames = wire_frames(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        }, 3);
        let keys: Vec<_> = frames.iter().map(|f| key_of(f).expect("key")).collect();
        assert_eq!(keys.iter().map(|k| k.child).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(keys.iter().all(|k| k.group == 1 && k.rgb == keys[0].rgb));
        for f in &frames {
            assert_eq!(pos(f, 6531), pos(f, 6122) + 1, "{f:?}");
        }
    }

    // ibx#248, captured 01/10/2026 (ib-agent captures/pd-orders,
    // pd_parent_child_modify): a parent sent alone has no key; its child
    // gets the next group with index 1; the parent's replace restates
    // group/0 and the child's replace its own key, with no parent or OCA
    // field.
    #[test]
    fn children_of_a_sent_parent_and_their_replaces_carry_the_key() {
        let child = || crate::types::OrderAttrs { parent_id: 11, ..Default::default() };
        let (frames, _) = session_frames(|_| {}, vec![
            limit_ex(11, Side::Buy, 230 * P, Default::default()),
            limit_ex(12, Side::Sell, 494 * P, child()),
            limit_ex(13, Side::Sell, 495 * P, child()),
            modify_limit(11, 231 * P, Default::default()),
            modify_limit(12, 497 * P, child()),
        ], 5);
        assert!(key_of(&frames[0]).is_none(), "the parent's new order has no key");
        let (c1, c2) = (key_of(&frames[1]).unwrap(), key_of(&frames[2]).unwrap());
        assert_eq!((c1.group, c1.child, c2.group, c2.child), (1, 1, 1, 2));
        assert_eq!(c1.rgb, c2.rgb);
        let parent_replace = key_of(&frames[3]).unwrap();
        assert_eq!(tag(&frames[3], 35), Some("G"));
        assert_eq!((parent_replace.group, parent_replace.child, parent_replace.rgb), (1, 0, c1.rgb));
        assert_eq!(tag(&frames[4], 35), Some("G"));
        assert_eq!(key_of(&frames[4]), Some(c1));
        for absent in [583, 6107, 6209] {
            assert!(tag(&frames[4], absent).is_none(), "a child's replace leaves out {absent}");
        }
        assert_eq!(pos(&frames[4], 6531), pos(&frames[4], 6122) + 1);
    }

    // ibx#248 (ib-agent captures/192 A5, pd-orders pd_bracket_keys): a
    // child added to a working bracket gets the next index of its group.
    #[test]
    fn a_child_added_to_a_working_bracket_gets_the_next_index() {
        let (frames, _) = session_frames(|_| {}, vec![
            OrderRequest::SubmitBracket {
                parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
                entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
            },
            limit_ex(6, Side::Sell, 120 * P, crate::types::OrderAttrs { parent_id: 3, ..Default::default() }),
        ], 4);
        let late = key_of(&frames[3]).unwrap();
        assert_eq!((late.group, late.child), (1, 3));
        assert_eq!(late.rgb, key_of(&frames[0]).unwrap().rgb);
    }

    // ibx#505: an order for an algo provider's destination (FOXRIVER on
    // the paper logon of 13/10/2026) goes through SMART with the provider
    // as its second destination; another directed exchange keeps both.
    #[test]
    fn an_algo_provider_destination_goes_through_smart() {
        for (exchange, routing, second) in [("FOXRIVER", "BEST", "FOXRIVER"), ("ARCA", "ARCA", "ARCA")] {
            let (frames, _) = session_frames(|c| {
                c.market.set_routing(0, "STK", exchange);
                // Records known: this test is about another rule (ibx#706).
                c.order_records_any.insert(265598);
                c.rth_types.insert((265598, exchange.to_string()), crate::engine::outside_rth::RthTypes {
                    sec_type: "STK".into(), types_known: true, ..Default::default()
                });
                c.algo_providers = crate::bridge::algo_provider_names("FOXRIVER/STK:A;IBALGO/STK:B;JONES/STK:C");
            }, vec![limit_ex(1, Side::Buy, 230 * P, Default::default())], 1);
            assert_eq!((tag(&frames[0], 100), tag(&frames[0], 6210)), (Some(routing), Some(second)), "{exchange}");
        }
    }

    // ibx#706: the record of the order's exchange, as the reference 10.51
    // asks it (38 recordings, 12/10 to 15/10/2026): a SMART order on AAPL
    // asks it on NASDAQ (the primary exchange) once and waits for it; an
    // order directed to ISLAND asks it each time, the answer naming NASDAQ;
    // an order directed to NASDAQ then asks nothing.
    #[test]
    fn an_order_asks_the_record_of_its_exchange_when_the_session_has_none() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        let answer = |id: &str| fix::fix_build(&[(35, "d"), (320, id), (6008, "265598"), (55, "AAPL"), (167, "STK"),
            (15, "USD"), (207, "NASDAQ"), (6523, "USSTK")], 1);
        let place = |context: &mut Context, conn: &mut Option<Connection>, server: &mut crate::protocol::connection::MemTransport,
                     exchange: &str, id: OrderId| -> Vec<Vec<(u32, String)>> {
            context.market.set_routing(0, "STK", exchange);
            context.pending_orders.push(limit_ex(id, Side::Buy, px(100.0), Default::default()));
            drain_frames(context, &shared, conn, server)
        };
        let lookup = |frames: &[Vec<(u32, String)>]| -> Option<(String, String)> {
            frames.iter().find(|f| tag(f, 35) == Some("c") && tag(f, 320).is_some_and(|n| n.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME)))
                .map(|f| (tag(f, 320).unwrap().to_string(), tag(f, 6004).unwrap_or("").to_string()))
        };
        let sent = |frames: &[Vec<(u32, String)>]| frames.iter().filter(|f| tag(f, 35) == Some("D")).count();

        // SMART: the record on NASDAQ asked, the order waits, then goes.
        let frames = place(&mut context, &mut conn, &mut server, "SMART", 1);
        let (id, on) = lookup(&frames).expect("the record asked");
        assert_eq!((on.as_str(), sent(&frames)), ("NASDAQ", 0), "{frames:?}");
        assert!(order_record_reply(&mut context, &id, &answer(&id)));
        assert_eq!(sent(&drain_frames(&mut context, &shared, &mut conn, &mut server)), 1);
        // A second SMART order: nothing asked.
        let frames = place(&mut context, &mut conn, &mut server, "SMART", 2);
        assert_eq!((lookup(&frames), sent(&frames)), (None, 1), "{frames:?}");
        // ISLAND, twice: asked each time, the answer naming NASDAQ; then the
        // clearing rule of ISLAND (ibx#709), not waited for.
        for (k, id) in [(0, 3), (1, 4)] {
            let frames = place(&mut context, &mut conn, &mut server, "ISLAND", id);
            let (name, on) = lookup(&frames).expect("the record asked");
            assert_eq!(on, "ISLAND");
            assert!(order_record_reply(&mut context, &name, &answer(&name)));
            assert_eq!(context.order_clearing_to_send, [(265598, "ISLAND".to_string())]);
            send_clearing_rules(&mut context, conn.as_mut().unwrap(), &mut HeartbeatState::new());
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let clearing = frames.iter().find(|f| tag(f, 320).is_some_and(|n| n.starts_with("ClearingRule")))
                .expect("the clearing rule asked");
            assert!(tag(clearing, 320).unwrap().starts_with(&format!("ClearingRule{k}NDReqByConid")), "{clearing:?}");
            assert_eq!(tag(clearing, 6004), Some("ISLAND"));
            assert!(lookup(&frames).is_none(), "the order goes on after its answer: {frames:?}");
            assert!(order_clearing_reply(&mut context, tag(clearing, 320).unwrap()));
        }
        // NASDAQ: the record is there, nothing asked.
        let frames = place(&mut context, &mut conn, &mut server, "NASDAQ", 5);
        assert_eq!(lookup(&frames), None, "{frames:?}");
    }

    // ibx#564: without the copied waits a transmitted stock order does not
    // wait for its contract's records: a SMART order goes at once with
    // nothing asked (the server took a first order without them, paper,
    // 17/10/2026, market closed). An order directed to ISLAND still asks
    // its order rules there, which its checks read, and no record.
    #[test]
    fn without_the_copied_waits_an_order_does_not_ask_its_records() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = order_context();
        context.order_records_wait = false;
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        let place = |context: &mut Context, conn: &mut Option<Connection>, server: &mut crate::protocol::connection::MemTransport,
                     exchange: &str, id: OrderId| -> Vec<Vec<(u32, String)>> {
            context.market.set_routing(0, "STK", exchange);
            context.pending_orders.push(limit_ex(id, Side::Buy, px(100.0), Default::default()));
            drain_frames(context, &shared, conn, server)
        };
        let frames = place(&mut context, &mut conn, &mut server, "SMART", 1);
        assert!(frames.iter().all(|f| tag(f, 35) != Some("c")), "{frames:?}");
        assert_eq!(frames.iter().filter(|f| tag(f, 35) == Some("D")).count(), 1, "{frames:?}");
        place(&mut context, &mut conn, &mut server, "ISLAND", 2);
        assert!(context.order_record_lookups.is_empty(), "no record asked");
        assert!(context.rth_lookups.iter().any(|(_, (_, exchange), _)| exchange == "ISLAND"), "the rules on ISLAND asked");
    }

    // ibx#709: an order directed to ARCA whose symbol lookup named ARCA
    // asks the record of the primary exchange (the reference 10.51, paper,
    // 15/10/2026, regular hours: NASDAQ asked, its answer naming NASDAQ, no
    // clearing rule).
    #[test]
    fn a_directed_order_asks_the_primary_record_when_its_own_is_there() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = order_context();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "ARCA");
        // The symbol lookup named ARCA: the record and its order rules.
        context.order_records.insert((265598, "ARCA".to_string()));
        context.rth_types.insert((265598, "ARCA".to_string()), crate::engine::outside_rth::RthTypes {
            sec_type: "STK".into(), types_known: true, ..Default::default()
        });
        context.pending_orders.push(limit_ex(1, Side::Buy, px(100.0), Default::default()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let asked = frames.iter().find(|f| tag(f, 320).is_some_and(|n| n.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME)))
            .expect("the primary's record asked");
        assert_eq!(tag(asked, 6004), Some("NASDAQ"));
        let name = tag(asked, 320).unwrap().to_string();
        let answer = fix::fix_build(&[(35, "d"), (320, &name), (6008, "265598"), (55, "AAPL"), (167, "STK"), (207, "NASDAQ")], 1);
        assert!(order_record_reply(&mut context, &name, &answer));
        assert!(context.order_clearing_to_send.is_empty(), "the answer named the exchange asked");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.iter().all(|f| !tag(f, 320).is_some_and(|n| n.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME))), "{frames:?}");
    }

    // ibx#718 (the reference 10.51, paper, 16/10/2026, pre-market and
    // regular hours): a parent held with transmit off as the first order of
    // its contract in the session is known once the lookups of its
    // placeOrder answered (its contract, then the trading hours and the
    // order preset, then its record on the primary exchange). A beta hedge
    // placed before (0.5 s after the parent on paper) is refused with 320 as
    // under no parent, nothing sent, the parent held; placed after (3.5 s),
    // it goes with its parent. A held parent of a contract whose records the
    // session has is known at once.
    #[test]
    fn a_held_parent_is_known_once_the_lookups_of_its_placing_answered() {
        use crate::api::types::{Contract as ApiContract, Order as ApiOrder};
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, ApiContract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = Context::new();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        // The contract's lookup answered on BEST, its trading hours known.
        context.order_records.insert((265598, "BEST".to_string()));
        let core = crate::client_core::ClientCore::new();
        let aapl = ApiContract {
            con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
            ..Default::default()
        };
        let parent = ApiOrder {
            action: "BUY".into(), order_type: "LMT".into(), lmt_price: 400.0, total_quantity: 10.0, tif: "DAY".into(),
            transmit: false, ..Default::default()
        };
        let hedge = |parent_id: i64| ApiOrder {
            action: "SELL".into(), order_type: "MKT".into(), total_quantity: 0.0, lmt_price: f64::MAX, tif: "DAY".into(),
            parent_id, hedge_type: "B".into(), hedge_param: "1".into(), hedge_max_size: 100, transmit: true,
            ..Default::default()
        };
        let hold = |context: &mut Context, conn: &mut Option<Connection>, id: OrderId| {
            core.note_held_placed(&shared, id);
            hold_order(conn, context, &mut HeartbeatState::new(), id, 0, 10 * crate::types::QTY_SCALE, 0, "");
            core.hold_order(id, aapl.clone(), parent.clone(), 0);
        };
        let reading = |id: OrderId, parent_id: i64| core.order_read_checks(
            id, 265598, "STK", "SMART", "", &hedge(parent_id), &shared, "DU1", false).1;
        let transmitted = |id: OrderId, parent_id: i64| core.orders_to_transmit_checked(id, &aapl, &hedge(parent_id), &shared, "DU1")
            .iter().map(|o| o.0).collect::<Vec<_>>();
        let record = |frames: &[Vec<(u32, String)>]| frames.iter()
            .find(|f| tag(f, 320).is_some_and(|n| n.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME)))
            .map(|f| (tag(f, 320).unwrap().to_string(), tag(f, 6004).unwrap_or("").to_string()));

        // The first held order of the contract: its trading hours (due
        // from its contract's answer, ibx#728), then the preset, not yet the
        // record; a hedge now is refused, its parent held.
        context.order_hours_due.push((265598, "1/STK/NASDAQ#LITE".to_string(), "SubMsgXMLQuery15".to_string()));
        hold(&mut context, &mut conn, 10);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"]);
        assert_eq!(record(&frames), None, "the record waits for the preset: {frames:?}");
        let missing = Some((320, "Error reading request. Missing parent order: server: 0 order: 11 parent: 10 hedge:[B]".to_string()));
        assert_eq!(reading(11, 10), missing);
        assert_eq!(transmitted(11, 10), [11], "the hedge alone, refused");
        assert!(context.unsent.contains_key(&10) && shared.orders.is_held_unknown(10));

        // The preset answered: the record on the primary exchange asked;
        // still not known.
        let reply: std::collections::HashMap<u32, String> =
            [(8166, "G"), (8168, "s=STK"), (8169, "v=1"), (4026, "GTC")].into_iter().map(|(t, v)| (t, v.to_string())).collect();
        preset_reply(&mut context, &fix::field_view(&reply));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let (name, on) = record(&frames).expect("the record asked");
        assert_eq!(on, "NASDAQ");
        assert_eq!(reading(11, 10), missing);

        // Its answer: the parent is known, the hedge goes with it.
        let answer = fix::fix_build(&[(35, "d"), (320, &name), (6008, "265598"), (55, "AAPL"), (167, "STK"), (207, "NASDAQ")], 1);
        assert!(order_record_reply(&mut context, &name, &answer));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.is_empty(), "{frames:?}");
        assert!(!shared.orders.is_held_unknown(10) && context.held_lookups.is_empty());
        assert_eq!(reading(11, 10), None);
        assert_eq!(transmitted(11, 10), [10, 11]);

        // A held order of a contract whose records the session has: known
        // at once, nothing asked.
        hold(&mut context, &mut conn, 20);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.is_empty(), "{frames:?}");
        assert!(!shared.orders.is_held_unknown(20));
        assert_eq!(reading(21, 20), None);
        assert_eq!(transmitted(21, 20), [20, 21]);

        // Cancelled while it waits: no longer waited for.
        context.order_records.clear();
        hold(&mut context, &mut conn, 30);
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(shared.orders.is_held_unknown(30));
        context.unsent.remove(&30);
        drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(!shared.orders.is_held_unknown(30) && context.held_lookups.is_empty());
    }

    /// The client side of the held-parent tests (ibx#718): a held order
    /// noted, taken by the engine as its command handler takes it (decided
    /// at once), then kept by the client; the reading of a beta hedge under
    /// it.
    struct HeldClient {
        core: crate::client_core::ClientCore,
        shared: Arc<SharedState>,
    }

    impl HeldClient {
        fn hold(&self, context: &mut Context, conn: &mut Option<Connection>, id: OrderId, slot: crate::types::InstrumentId,
                contract: &crate::api::types::Contract) {
            let parent = crate::api::types::Order {
                action: "BUY".into(), order_type: "LMT".into(), lmt_price: 50.0, total_quantity: 10.0, tif: "DAY".into(),
                transmit: false, ..Default::default()
            };
            self.core.note_held_placed(&self.shared, id);
            hold_order(conn, context, &mut HeartbeatState::new(), id, slot, 10 * crate::types::QTY_SCALE, 0, "");
            let at = std::time::Instant::now();
            advance_held_lookups(context, conn.as_mut().unwrap(), &mut HeartbeatState::new(), &self.shared);
            assert!(at.elapsed() < std::time::Duration::from_millis(100), "the engine took {:?} to decide", at.elapsed());
            self.core.hold_order(id, contract.clone(), parent, slot);
        }

        /// The reading of a hedge under `parent_id`, and how long the
        /// client waited for the engine's decision.
        fn hedge_reading(&self, id: OrderId, parent_id: i64, con_id: i64) -> (Option<(i64, String)>, std::time::Duration) {
            let hedge = crate::api::types::Order {
                action: "SELL".into(), order_type: "MKT".into(), total_quantity: 0.0, lmt_price: f64::MAX, tif: "DAY".into(),
                parent_id, hedge_type: "B".into(), hedge_param: "1".into(), hedge_max_size: 100, transmit: true,
                ..Default::default()
            };
            let at = std::time::Instant::now();
            let refusal = self.core.order_read_checks(id, con_id, "STK", "SMART", "", &hedge, &self.shared, "DU1", con_id == 0).1;
            (refusal, at.elapsed())
        }
    }

    fn missing_parent(order_id: OrderId, parent_id: OrderId) -> Option<(i64, String)> {
        Some((320, format!("Error reading request. Missing parent order: server: 0 order: {} parent: {} hedge:[B]", order_id, parent_id)))
    }

    // ibx#718, observed on paper, 17/10/2026, market closed (case A): a held
    // parent by symbol of a contract the session has ordered before (a far
    // AAPL order placed and cancelled) is known at once, its own lookup
    // still on its way: a beta hedge 0.1 s after it passed the reading. The
    // first held order of a contract by symbol still waits for its lookups
    // (the rule of 16/10/2026). Either way the engine decides as it takes
    // the order: the client does not wait for it.
    #[test]
    fn a_held_parent_of_a_contract_the_session_knows_is_known_at_once() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = order_context();
        let by_symbol = |context: &mut Context, symbol: &str| {
            let slot = context.market.try_register_unresolved().unwrap();
            context.market.set_symbol(slot, symbol.into());
            context.market.set_routing(slot, "STK", "SMART");
            context.market.set_currency(slot, "USD");
            slot
        };
        let stock = |symbol: &str| crate::api::types::Contract {
            symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
        };
        // The far order before: its lookup named AAPL, its record answered.
        let warm = by_symbol(&mut context, "AAPL");
        context.order_lookups.push((ORDER_LOOKUP_FIRST_ID, warm));
        let reply = "8=FIX.4.1|9=0100|35=d|320=FixSecDefReqBySymbol3221225472|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|146=1|6344=1|6008=265598|10=000|".replace('|', "\x01");
        assert!(order_contract_reply(&mut context, &shared, &format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID), reply.as_bytes()));
        context.order_records.insert((265598, "NASDAQ".to_string()));
        let client = HeldClient { core: crate::client_core::ClientCore::new(), shared: shared.clone() };

        // The held parent: its lookup asked as the reference asks it, the
        // parent known at once, the hedge read with no wait.
        let parent_slot = by_symbol(&mut context, "AAPL");
        client.hold(&mut context, &mut conn, 40, parent_slot, &stock("AAPL"));
        assert!(!shared.orders.held_pending(40) && context.held_lookups.is_empty());
        let (refusal, waited) = client.hedge_reading(41, 40, 0);
        assert_eq!(refusal, None);
        assert!(waited < std::time::Duration::from_millis(100), "the client waited {waited:?}");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["definition"], "its own lookup only: {frames:?}");
        assert!(!shared.orders.is_held_unknown(40));

        // The first held order of another contract by symbol: unknown until
        // its lookups answered, decided as unknown at once.
        let ibm = by_symbol(&mut context, "IBM");
        client.hold(&mut context, &mut conn, 50, ibm, &stock("IBM"));
        assert!(shared.orders.held_pending(50));
        let (refusal, waited) = client.hedge_reading(51, 50, 0);
        assert_eq!(refusal, missing_parent(51, 50));
        assert!(waited < std::time::Duration::from_millis(100), "the client waited {waited:?}");
    }

    // ibx#718, observed on paper, 17/10/2026, market closed (case B): a held
    // parent given by its conId (MSFT, the first order of its contract in
    // the session, the stock order preset known) asks one definition at
    // once, by its conId on its primary exchange, answered 0.12 s later; a
    // beta hedge 0.5 s after the parent passed the reading. The parent is
    // known once that definition answered; the engine's decisions come at
    // once, the client does not wait for them.
    #[test]
    fn a_held_parent_by_con_id_is_known_once_its_definition_answered() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(272093, crate::api::types::Contract {
            con_id: 272093, symbol: "MSFT".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = order_context();
        let slot = context.market.register(272093);
        context.set_symbol(slot, "MSFT".to_string());
        context.market.set_routing(slot, "STK", "SMART");
        let msft = crate::api::types::Contract {
            con_id: 272093, symbol: "MSFT".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
            ..Default::default()
        };
        let client = HeldClient { core: crate::client_core::ClientCore::new(), shared: shared.clone() };

        client.hold(&mut context, &mut conn, 60, slot, &msft);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["record NASDAQ"], "{frames:?}");
        let (refusal, waited) = client.hedge_reading(61, 60, 272093);
        assert_eq!(refusal, missing_parent(61, 60));
        assert!(waited < std::time::Duration::from_millis(100), "the client waited {waited:?}");

        // Its answer: known, the hedge passes the reading.
        let name = frames[0].iter().find(|(t, _)| *t == 320).map(|(_, v)| v.clone()).unwrap();
        let answer = fix::fix_build(&[(35, "d"), (320, &name), (6008, "272093"), (55, "MSFT"), (167, "STK"), (207, "NASDAQ")], 1);
        assert!(order_record_reply(&mut context, &name, &answer));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(frames.is_empty(), "{frames:?}");
        assert!(!shared.orders.held_pending(60) && context.held_lookups.is_empty());
        let (refusal, waited) = client.hedge_reading(62, 60, 272093);
        assert_eq!(refusal, None);
        assert!(waited < std::time::Duration::from_millis(100), "the client waited {waited:?}");
    }

    /// A session at its first stock order: AAPL, its symbol lookup answered
    /// on BEST, its trading hours due from that answer (ibx#728).
    fn first_stock_order_session(destination: &str) -> (Context, Arc<SharedState>, Option<Connection>, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        let conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = Context::new();
        context.order_ids.start_at(FIRST as i32);
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", destination);
        context.order_records.insert((265598, "BEST".to_string()));
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "STK".into(), ..Default::default()
        });
        context.order_hours_due.push((265598, "1/STK/NASDAQ#LITE".to_string(), "SubMsgXMLQuery15".to_string()));
        context.zone_pending.insert(265598);
        (context, shared, conn, server)
    }

    /// What each frame asks, in the order sent.
    fn asked_kinds(frames: &[Vec<(u32, String)>]) -> Vec<String> {
        frames.iter().map(|f| match (tag(f, 35), tag(f, 6040)) {
            (Some("U"), Some("106")) => "hours".to_string(),
            (Some("U"), Some("193")) if tag(f, 8166) == Some("S") => format!("save {}", tag(f, 8168).unwrap_or("")),
            (Some("U"), Some("193")) => format!("preset {}", tag(f, 8168).unwrap_or("")),
            (Some("U"), Some("65")) => format!("permission {}", tag(f, 6411).unwrap_or("")),
            (Some("c"), _) if tag(f, 320).is_some_and(|n| n.starts_with(crate::control::contracts::SECDEF_ORDER_TYPES_NAME)) =>
                format!("record {}", tag(f, 6004).unwrap_or("")),
            (Some("c"), _) => "definition".to_string(),
            (Some(t), _) => t.to_string(),
            _ => "?".to_string(),
        }).collect()
    }

    fn day_order(order_id: OrderId) -> OrderRequest {
        OrderRequest::SubmitLimitEx {
            order_id, instrument: 0, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0', attrs: Default::default(),
        }
    }

    fn record_answer(context: &mut Context, frame: &[(u32, String)], named: &str) {
        let name = tag(frame, 320).expect("a record lookup").to_string();
        let answer = fix::fix_build(&[(35, "d"), (320, &name), (6008, "265598"), (55, "AAPL"), (167, "STK"), (207, named)], 1);
        assert!(order_record_reply(context, &name, &answer));
    }

    fn stock_preset_answer(context: &mut Context) {
        let reply: std::collections::HashMap<u32, String> =
            [(8166, "G"), (8168, "s=STK"), (8169, "v=1"), (4026, "GTC")].into_iter().map(|(t, v)| (t, v.to_string())).collect();
        preset_reply(context, &fix::field_view(&reply));
    }

    // ibx#728 (the reference 10.51, paper, 12-16/10/2026, 15 first stock
    // orders sent): the trading hours and the stock preset after the
    // contract's answer, the preset asked for a DAY order too; the record of
    // the primary exchange after the preset's answer, not before; the order
    // after the record's answer, not waiting for the hours' answer, its DAY
    // kept; the next order of the contract asks nothing.
    #[test]
    fn a_first_stock_order_asks_hours_and_preset_then_its_record_then_goes() {
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"]);
        assert_eq!(tag(&frames[0], 6256), Some("1/STK/NASDAQ#LITE"));
        assert!(context.order_hours_due.is_empty());
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "it waits for the preset");

        stock_preset_answer(&mut context);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["record NASDAQ"], "the record after the preset's answer");

        record_answer(&mut context, &frames[0], "NASDAQ");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["D"]);
        assert_eq!(tag(&frames[0], 59), Some("0"), "the DAY order keeps its time in force");
        assert!(shared.orders.drain_order_errors().is_empty(), "no preset notice for an order with a time in force");
        assert!(context.zone_pending.contains(&265598), "the order did not wait for the hours' answer");

        context.pending_orders.push(day_order(FIRST + 1));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["D"], "a second order of the contract asks nothing");
    }

    // ibx#728 (the reference 10.51, paper, 13-14/10/2026, three orders on a
    // stock new to a session that had the preset): the hours and the record
    // leave together; the order goes after the record's answer.
    #[test]
    fn with_the_preset_known_the_hours_and_the_record_leave_together() {
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
        context.order_presets.insert("STK".to_string(), Some("GTC".to_string()));
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "record NASDAQ"]);
        record_answer(&mut context, &frames[1], "NASDAQ");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["D"]);
    }

    // ibx#728 (the reference 10.51, paper, 13/10/2026, AAPL directed to
    // ISLAND, the symbol lookup naming NASDAQ): the record of the order's
    // own exchange leaves with the hours, the stock preset after its answer.
    #[test]
    fn a_record_of_the_orders_own_exchange_comes_before_the_preset() {
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("ISLAND");
        context.order_records.insert((265598, "NASDAQ".to_string()));
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "record ISLAND"]);
        record_answer(&mut context, &frames[1], "NASDAQ");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["preset s=STK"], "the preset after the record's answer");
        stock_preset_answer(&mut context);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert!(asked_kinds(&frames).iter().all(|k| !k.starts_with("record") && !k.starts_with("preset")), "{frames:?}");
    }

    // ibx#728 (the reference 10.51, paper, 13-16/10/2026, fourteen first
    // orders of eight types): the question on the class's permission first,
    // the trading hours due for the contract after it.
    #[test]
    fn the_permission_question_goes_before_the_trading_hours() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(692808239);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "OPT", "SMART");
        context.pending_orders.push(OrderRequest::SubmitLimit {
            order_id: 80, instrument: 0, side: Side::Buy, qty: 1, price: px(0.05) });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!((frames.len(), tag(&frames[0], 35)), (1, Some("c")), "the definition: {frames:?}");
        let id = tag(&frames[0], 320).unwrap().to_string();
        let fields = vec![(35, "d"), (320, id.as_str()), (6008, "692808239"), (55, "AAPL"), (167, "OPT"),
            (15, "USD"), (207, "BEST"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3,MKT/1")];
        assert!(rth_definition_reply(&mut context, &id, &fix::fix_build(&fields, 1)));
        // The definition's answer called for the hours (the engine's part).
        context.order_hours_due.push((692808239, "2/OPT/AAPL".to_string(), "SubMsgXMLQuery20".to_string()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["permission 2/OPT/R", "hours"]);
    }

    // ibx#728: hours due for a contract no order waits on any more (here
    // none) are written at the end of the pass, never left unasked.
    #[test]
    fn trading_hours_no_order_waits_for_still_leave() {
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours"]);
        assert!(context.order_hours_due.is_empty());
    }

    /// A session at the first order of a future (MES on CME) or a CFD (AAPL
    /// on SMART), its symbol lookup answered with the class's permission
    /// key, its trading hours due (ibx#729).
    fn first_order_of(sec_type: &str) -> (Context, Arc<SharedState>, Option<Connection>, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        let conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        let (con_id, symbol, exchange, destination, key, local, month, mult) = match sec_type {
            "FUT" => (815824257, "MES", "CME", "CME", "CME/FUT/GLOBEX", "MESZ6", "202612", "5"),
            _ => (120549942, "AAPL", "SMART", "BEST", "20/CFD/STK", "AAPLn", "", "1"),
        };
        context.market.register(con_id);
        context.set_symbol(0, symbol.to_string());
        context.market.set_routing(0, sec_type, exchange);
        context.rth_types.insert((con_id, destination.to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: sec_type.into(), local_symbol: local.into(), contract_month: month.into(),
            multiplier: mult.into(), permission_key: key.into(), ..Default::default()
        });
        context.order_hours_due.push((con_id, format!("{key}#hours"), "SubMsgXMLQuery30".to_string()));
        (context, shared, conn, server)
    }

    fn preset_answer(context: &mut Context, kind: &str, tif: Option<&str>) {
        let mut reply: std::collections::HashMap<u32, String> =
            [(8166, "G"), (8168, kind), (8169, "v=1")].into_iter().map(|(t, v)| (t, v.to_string())).collect();
        if let Some(tif) = tif {
            reply.insert(4026, tif.to_string());
        }
        preset_reply(context, &fix::field_view(&reply));
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026, `.tmp/666` probe: an
    // MES order on CME and an AAPL CFD order, DAY, the first of their type,
    // neither type in the logon's list of stored presets): the permission
    // of the class, the trading hours, the order with the question on its
    // way, then the type's preset saved, not waited for; once per type.
    // Once the answer says no permission, the next order of the class is
    // refused with 460 and nothing sent (paper, 13/10/2026, a session that
    // had the answers: three CFD orders and an MES order).
    #[test]
    fn a_future_and_a_cfd_ask_their_permission_and_save_their_preset_after_the_order() {
        for (sec_type, key) in [("FUT", "CME/FUT/GLOBEX"), ("CFD", "20/CFD/STK")] {
            let (mut context, shared, mut conn, mut server) = first_order_of(sec_type);
            context.pending_orders.push(day_order(FIRST));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let save = format!("save s={sec_type}");
            assert_eq!(asked_kinds(&frames), [format!("permission {key}").as_str(), "hours", "D", save.as_str()], "{sec_type}");
            let save = frames.last().unwrap();
            assert_eq!((tag(save, 6556), tag(save, 8169), tag(save, 8176)), (Some("OPR.3"), Some("v=1&a=1"), None));
            assert_eq!(tag(&frames[2], 59), Some("0"), "the DAY order kept");
            assert!(context.presets_stored.as_ref().unwrap().contains(sec_type));

            // A second order before the answer: no question, no save.
            context.pending_orders.push(day_order(FIRST + 1));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["D"], "{sec_type}");

            permission_reply(&mut context, &fix::field_view(&permission_answer(key, "0")));
            context.pending_orders.push(day_order(FIRST + 2));
            assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
            assert_eq!(shared.orders.drain_order_errors(), [(FIRST + 2, 460, "No trading permissions.".to_string())]);

            // With the permission, the next order goes.
            let (mut context, shared, mut conn, mut server) = first_order_of(sec_type);
            permission_reply(&mut context, &fix::field_view(&permission_answer(key, "1")));
            context.pending_orders.push(day_order(FIRST));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["hours", "D", format!("save s={sec_type}").as_str()]);
        }
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026, `.tmp/666` yk: an MES
    // order by conId, DAY, the future's preset stored on the account): the
    // permission, the hours, then the preset asked and waited for; the
    // permission's answer, no permission, came first: 460, nothing sent.
    // With the permission the order goes once the preset answered.
    #[test]
    fn a_stored_preset_is_asked_after_the_permission_and_waited_for() {
        for allowed in ["0", "1"] {
            let (mut context, shared, mut conn, mut server) = first_order_of("FUT");
            context.presets_stored.as_mut().unwrap().insert("FUT".to_string());
            context.pending_orders.push(day_order(FIRST));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["permission CME/FUT/GLOBEX", "hours", "preset s=FUT"]);
            assert_eq!(tag(&frames[2], 8176), Some("1"));
            permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FUT/GLOBEX", allowed)));
            assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "it waits for the preset");
            assert!(shared.orders.drain_order_errors().is_empty(), "the answer is read once the preset came");
            preset_answer(&mut context, "s=FUT", None);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            let errors = shared.orders.drain_order_errors();
            if allowed == "0" {
                assert!(frames.is_empty(), "{frames:?}");
                assert_eq!(errors, [(FIRST, 460, "No trading permissions.".to_string())]);
            } else {
                assert_eq!(asked_kinds(&frames), ["D"]);
                assert!(errors.is_empty(), "{errors:?}");
            }
        }
        // Before the logon's list answered, a preset is asked as a stored one.
        let (mut context, shared, mut conn, mut server) = first_order_of("CFD");
        context.presets_stored = None;
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["permission 20/CFD/STK", "hours", "preset s=CFD"]);
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026, `.tmp/666` probe: a
    // EUR.USD order on IDEALPRO, DAY, both presets stored): the trading
    // hours and the preset of the pair's currency, then the forex preset
    // once that answered, then the order once both answered. No permission
    // is asked for a forex pair. With no time in force, the pair's
    // currency's preset gives it.
    #[test]
    fn a_forex_order_asks_the_currency_preset_then_the_forex_one() {
        let order = |tif: Option<u8>| match tif {
            Some(tif) => OrderRequest::SubmitLimitEx { order_id: FIRST, instrument: 0, side: Side::Buy, qty: 20000, price: P, tif,
                attrs: Default::default() },
            None => OrderRequest::SubmitLimitEx { order_id: FIRST, instrument: 0, side: Side::Buy, qty: 20000, price: P, tif: b'0',
                attrs: crate::types::OrderAttrs { tif_preset: true, ..Default::default() } },
        };
        for tif in [Some(b'0'), None] {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = order_context();
            context.order_ids.start_at(FIRST as i32);
            context.market.register(12087792);
            context.set_symbol(0, "EUR".to_string());
            context.market.set_routing(0, "CASH", "IDEALPRO");
            context.rth_types.insert((12087792, "IDEALPRO".to_string()), crate::engine::outside_rth::RthTypes {
                rth: true, sec_type: "CASH".into(), local_symbol: "EUR.USD".into(), multiplier: "1".into(), ..Default::default()
            });
            context.order_hours_due.push((12087792, "4/CASH/IBFX".to_string(), "SubMsgXMLQuery24".to_string()));
            context.pending_orders.push(order(tif));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["hours", "preset s=CASH&tc=EUR"]);
            preset_answer(&mut context, "s=CASH&tc=EUR", Some("GTC"));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["preset s=CASH"]);
            preset_answer(&mut context, "s=CASH", None);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["D"]);
            let errors = shared.orders.drain_order_errors();
            match tif {
                Some(_) => {
                    assert_eq!(tag(&frames[0], 59), Some("0"));
                    assert!(errors.is_empty(), "{errors:?}");
                }
                None => {
                    assert_eq!(tag(&frames[0], 59), Some("1"), "GTC from the currency's preset");
                    assert_eq!(errors, [(FIRST, 10349, "Order TIF was set to GTC based on order preset.".to_string())]);
                }
            }
        }
    }

    // ibx#729: the logon's list names the stored kinds; a preset answer of
    // a pair's currency keeps its own kind.
    #[test]
    fn the_logons_list_and_a_currency_preset_keep_their_kinds() {
        let mut context = Context::new();
        preset_list_reply(&mut context, ["s=CASH", "s=CASH&tc=EUR", "s=COMB", "s=STK"].into_iter());
        assert_eq!(context.presets_stored, Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect()));
        preset_answer(&mut context, "s=CASH&tc=EUR", Some("GTC"));
        assert_eq!(context.order_presets.get("CASH&tc=EUR"), Some(&Some("GTC".to_string())));
        assert!(!context.order_presets.contains_key("CASH"));
    }

    /// The copied gaps of a first stock order on, by a clock the test sets.
    fn gapped(context: &mut Context, at: Instant) {
        context.order_gaps = true;
        context.gap_clock = Some(at);
    }

    // ibx#729 (the reference 10.51, paper, 13-16/10/2026, the first stock
    // order of a session, by symbol): on a session opened by a logon, the
    // record 40 ms after the preset's answer, the order 91 ms after the
    // record's answer; the loop is told when each wait ends. A second
    // contract, the preset known: the order 5 ms after its record's answer.
    #[test]
    fn a_first_stock_order_keeps_the_references_gaps() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
        gapped(&mut context, start);
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"]);
        stock_preset_answer(&mut context);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the record waits");
        assert_eq!(context.order_wake, Some(start + ms(40)));
        context.gap_clock = Some(start + ms(39));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        context.gap_clock = Some(start + ms(40));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["record NASDAQ"]);
        context.gap_clock = Some(start + ms(150));
        record_answer(&mut context, &frames[0], "NASDAQ");
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the order waits");
        assert_eq!(context.order_wake, Some(start + ms(241)));
        context.gap_clock = Some(start + ms(241));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["D"]);
        assert!(context.preset_waiters.is_empty() && context.record_answered.is_empty());
        // A second order of the contract: at once.
        context.pending_orders.push(day_order(FIRST + 1));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["D"]);

        // The preset known: the record with the hours, the order 5 ms after
        // the record's answer.
        let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
        gapped(&mut context, start);
        context.order_presets.insert("STK".to_string(), None);
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "record NASDAQ"]);
        record_answer(&mut context, &frames[1], "NASDAQ");
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        context.gap_clock = Some(start + ms(5));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["D"]);
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026, three first stock
    // orders whose records the session had): the order 130 ms after the
    // preset's answer. Off on a session not opened by a logon: at once.
    #[test]
    fn a_first_stock_order_without_a_record_goes_130_ms_after_the_preset() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        for gaps in [true, false] {
            let (mut context, shared, mut conn, mut server) = first_stock_order_session("SMART");
            if gaps {
                gapped(&mut context, start);
            }
            context.order_records_any.insert(265598);
            context.pending_orders.push(day_order(FIRST));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"]);
            stock_preset_answer(&mut context);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            if !gaps {
                assert_eq!(asked_kinds(&frames), ["D"], "no wait off a logon's session");
                assert_eq!(context.order_wake, None);
                continue;
            }
            assert!(frames.is_empty());
            assert_eq!(context.order_wake, Some(start + ms(130)));
            context.gap_clock = Some(start + ms(130));
            assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["D"]);
        }
    }

    // ibx#729: a held order's record keeps the gap after the preset's
    // answer (the reference 10.51, paper, 16/10/2026: 36 to 60 ms in six
    // held first orders).
    #[test]
    fn a_held_first_orders_record_keeps_the_gap() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let mut context = Context::new();
        context.presets_stored = Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect());
        gapped(&mut context, start);
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_routing(0, "STK", "SMART");
        context.order_records.insert((265598, "BEST".to_string()));
        shared.orders.mark_held_unknown(10);
        hold_order(&mut conn, &mut context, &mut HeartbeatState::new(), 10, 0, crate::types::QTY_SCALE, 0, "");
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["preset s=STK"]);
        stock_preset_answer(&mut context);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the record waits");
        context.gap_clock = Some(start + ms(40));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["record NASDAQ"]);
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026 and 16/10/2026, two
    // AAPL orders given by conId, the first of the contract in the session):
    // the definition, the hours and the preset at once after its answer, the
    // record 4 ms after the preset's answer, the order 31 ms after the
    // record's answer.
    #[test]
    fn a_first_stock_order_by_con_id_keeps_its_shorter_gaps() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = Context::new();
        context.presets_stored = Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect());
        context.order_ids.start_at(FIRST as i32);
        gapped(&mut context, start);
        context.market.register(265598);
        context.market.set_symbol(0, String::new());
        context.market.set_routing(0, "", "SMART");
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "the definition first: {frames:?}");
        let id = tag(&frames[0], 320).unwrap().to_string();
        // The engine caches the answer's contract, its primary exchange too.
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        let reply = fix::fix_build(&[(35, "d"), (320, id.as_str()), (6008, "265598"), (55, "AAPL"), (167, "STK"), (15, "USD"), (207, "BEST")], 1);
        assert!(rth_definition_reply(&mut context, &id, &reply));
        assert!(context.by_con_id_contracts.contains(&265598));
        context.order_hours_due.push((265598, "1/STK/NASDAQ#LITE".to_string(), "SubMsgXMLQuery15".to_string()));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"], "no wait after a definition's answer");
        stock_preset_answer(&mut context);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the record waits");
        assert_eq!(context.order_wake, Some(start + ms(4)));
        context.gap_clock = Some(start + ms(4));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["record NASDAQ"]);
        context.gap_clock = Some(start + ms(114));
        record_answer(&mut context, &frames[0], "NASDAQ");
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "the order waits");
        assert_eq!(context.order_wake, Some(start + ms(145)));
        context.gap_clock = Some(start + ms(145));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["D"]);
    }

    /// A session at its first stock order by symbol, AAPL with no conId
    /// yet: the order's lookup is out.
    fn stock_order_by_symbol(context: &mut Context, shared: &Arc<SharedState>, conn: &mut Option<Connection>,
                             server: &mut crate::protocol::connection::MemTransport) {
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "SMART");
        context.market.set_currency(slot, "USD");
        context.pending_orders.push(OrderRequest::SubmitLimitEx {
            order_id: FIRST, instrument: slot, side: Side::Buy, qty: 1, price: 100 * P, tif: b'0', attrs: Default::default(),
        });
        let frames = drain_frames(context, shared, conn, server);
        assert_eq!(frames.len(), 1, "the lookup: {frames:?}");
    }

    fn stock_lookup_answer(context: &mut Context, shared: &SharedState) {
        let id = format!("FixSecDefReqBySymbol{}", ORDER_LOOKUP_FIRST_ID);
        let reply = "8=FIX.4.1|35=d|320=x|322=*|323=4|55=AAPL|167=STK|207=BEST|6008=265598|15=USD|146=1|10=000|".replace('|', "\x01");
        assert!(order_contract_reply(context, shared, &id, reply.as_bytes()));
        // The answer called for the contract's hours (the engine's part).
        context.order_hours_due.push((265598, "1/STK/NASDAQ#LITE".to_string(), "SubMsgXMLQuery15".to_string()));
    }

    // ibx#729 (the reference 10.51, paper, 12-16/10/2026, 28 first stock
    // orders by symbol): the first step, here the trading hours with the
    // preset after them, 10 ms after the lookup's answer when the stock
    // preset is asked; with the preset known, at once (6 orders). Off a
    // logon's session: at once.
    #[test]
    fn a_first_stock_orders_first_step_waits_after_the_lookup() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        for (gaps, preset_known) in [(true, false), (true, true), (false, false)] {
            let (client, mut server) = crate::protocol::connection::mem_pair();
            let mut conn = Some(Connection::new_mem(client));
            let shared = Arc::new(SharedState::new());
            let mut context = Context::new();
            context.presets_stored = Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect());
            context.order_ids.start_at(FIRST as i32);
            if gaps {
                gapped(&mut context, start);
            }
            if preset_known {
                context.order_presets.insert("STK".to_string(), None);
            }
            stock_order_by_symbol(&mut context, &shared, &mut conn, &mut server);
            stock_lookup_answer(&mut context, &shared);
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            if !gaps || preset_known {
                assert_eq!(asked_kinds(&frames)[0], "hours", "{gaps} {preset_known}: at once");
                continue;
            }
            assert!(frames.is_empty(), "{frames:?}");
            assert_eq!(context.order_wake, Some(start + ms(10)));
            context.gap_clock = Some(start + ms(9));
            assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
            context.gap_clock = Some(start + ms(10));
            let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
            assert_eq!(asked_kinds(&frames), ["hours", "preset s=STK"]);
            assert!(context.lookup_answered.is_empty());
        }
    }

    // ibx#729: a held first order by symbol keeps the same wait before its
    // first step (held ones among the 28 orders, 16/10/2026).
    #[test]
    fn a_held_first_orders_first_step_waits_after_the_lookup() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = Context::new();
        context.presets_stored = Some(PAPER_PRESETS.iter().map(|k| k.to_string()).collect());
        gapped(&mut context, start);
        let slot = context.market.try_register_unresolved().unwrap();
        context.market.set_symbol(slot, "AAPL".into());
        context.market.set_routing(slot, "STK", "SMART");
        context.market.set_currency(slot, "USD");
        shared.reference.cache_contract(265598, crate::api::types::Contract {
            con_id: 265598, symbol: "AAPL".into(), primary_exchange: "NASDAQ".into(), ..Default::default()
        });
        shared.orders.mark_held_unknown(10);
        hold_order(&mut conn, &mut context, &mut HeartbeatState::new(), 10, slot, crate::types::QTY_SCALE, 0, "");
        assert_eq!(drain_frames(&mut context, &shared, &mut conn, &mut server).len(), 1, "the lookup");
        stock_lookup_answer(&mut context, &shared);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(context.order_wake, Some(start + ms(10)));
        context.gap_clock = Some(start + ms(10));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["hours", "preset s=STK"]);
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026 and 16/10/2026, a
    // future and a metal limit order, their preset stored, no permission):
    // the refusal 93 ms after the preset's answer; nothing sent.
    #[test]
    fn a_refusal_after_a_stored_preset_keeps_the_references_gap() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (mut context, shared, mut conn, mut server) = first_order_of("FUT");
        context.presets_stored.as_mut().unwrap().insert("FUT".to_string());
        gapped(&mut context, start);
        context.pending_orders.push(day_order(FIRST));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["permission CME/FUT/GLOBEX", "hours", "preset s=FUT"]);
        permission_reply(&mut context, &fix::field_view(&permission_answer("CME/FUT/GLOBEX", "0")));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        preset_answer(&mut context, "s=FUT", None);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert!(shared.orders.drain_order_errors().is_empty(), "not yet");
        assert_eq!(context.order_wake, Some(start + ms(93)));
        context.gap_clock = Some(start + ms(93));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty(), "nothing sent");
        assert_eq!(shared.orders.drain_order_errors(), [(FIRST, 460, "No trading permissions.".to_string())]);
        assert!(context.preset_waiters.is_empty());
    }

    // ibx#729 (the reference 10.51, paper, 13/10/2026, EUR.USD, both
    // presets stored): the order 17 ms after the second preset's answer.
    #[test]
    fn a_forex_order_goes_17_ms_after_its_second_preset() {
        let ms = std::time::Duration::from_millis;
        let start = Instant::now();
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.order_ids.start_at(FIRST as i32);
        gapped(&mut context, start);
        context.market.register(12087792);
        context.set_symbol(0, "EUR".to_string());
        context.market.set_routing(0, "CASH", "IDEALPRO");
        context.rth_types.insert((12087792, "IDEALPRO".to_string()), crate::engine::outside_rth::RthTypes {
            rth: true, sec_type: "CASH".into(), local_symbol: "EUR.USD".into(), multiplier: "1".into(), ..Default::default()
        });
        context.pending_orders.push(OrderRequest::SubmitLimitEx { order_id: FIRST, instrument: 0, side: Side::Buy, qty: 20000, price: P,
            tif: b'0', attrs: Default::default() });
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["preset s=CASH&tc=EUR"]);
        preset_answer(&mut context, "s=CASH&tc=EUR", None);
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["preset s=CASH"], "no wait between them");
        context.gap_clock = Some(start + ms(100));
        preset_answer(&mut context, "s=CASH", None);
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(context.order_wake, Some(start + ms(117)));
        context.gap_clock = Some(start + ms(117));
        assert_eq!(asked_kinds(&drain_frames(&mut context, &shared, &mut conn, &mut server)), ["D"]);
        assert!(context.preset_waiters.is_empty());
    }

    const AAPL_CALL: [(u32, &str); 12] = [(55, "AAPL"), (167, "OPT"), (15, "USD"), (207, "BEST"), (200, "202612"), (541, "20261218"),
        (201, "1"), (202, "230"), (231, "100"), (6035, "AAPL  261218C00230000"), (6411, "2/OPT/R"), (6431, "DAY/3,LMT/3,VOL/3")];

    // ibx#729 (the reference 10.51, paper, 13/10/2026 regular hours and
    // 16/10/2026, two AAPL option volatility orders refused with 460): after
    // the refusal, the CFD of the underlying by symbol and currency, its
    // definition by conId on SMART once answered, then the permission of
    // the class it names; once per underlying, and not for an option order
    // of another type.
    #[test]
    fn a_refused_volatility_option_order_checks_the_underlyings_cfd() {
        let vol = |id| OrderRequest::SubmitEx { order_id: id, instrument: 0, side: Side::Buy, qty: 1,
            kind: crate::types::OrderKind::Volatility { volatility: px(30.0), vol_type: 2, peg: 0, price: 0 }, tif: b'0',
            attrs: crate::types::OrderAttrs::default() };
        let (frames, shared, mut context, mut conn, mut server) = order_by_con_id("692808239", "SMART", vol(91), &AAPL_CALL);
        assert_eq!(asked_kinds(&frames), ["permission 2/OPT/R"]);
        permission_reply(&mut context, &fix::field_view(&permission_answer("2/OPT/R", "0")));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(shared.orders.drain_order_errors(), [(91, 460, "No trading permissions.".to_string())]);
        assert_eq!(frames.len(), 1, "{frames:?}");
        let lookup: Vec<(u32, &str)> = frames[0].iter().filter(|(t, _)| !matches!(*t, 8 | 9 | 34 | 52 | 10))
            .map(|(t, v)| (*t, v.as_str())).collect();
        assert_eq!(lookup, [(35, "c"), (320, "FixSecDefReqBySymbol1"), (321, "2"), (55, "AAPL"), (167, "CFD"), (15, "USD")]);
        // Its answer: the CFD's definition by conId on SMART.
        let name = context.cfd_requests[0].clone();
        let answer = fix::fix_build(&[(35, "d"), (320, name.as_str()), (55, "AAPL"), (167, "CFD"), (207, "BEST"), (6008, "120549942"),
            (6411, "20/CFD/STK")], 1);
        let mut hb = HeartbeatState::new();
        assert!(cfd_check_reply(&mut context, conn.as_mut(), &mut hb, &name, &answer));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert!(tag(&frames[0], 320).is_some_and(|n| n.starts_with("DirectedSmartClusterReqByConid")), "{frames:?}");
        assert_eq!((tag(&frames[0], 6008), tag(&frames[0], 6004), tag(&frames[0], 146)), (Some("120549942"), Some("BEST"), Some("1")));
        // Its answer: the permission of the class it names.
        let name = context.cfd_requests[0].clone();
        let answer = fix::fix_build(&[(35, "d"), (320, name.as_str()), (55, "AAPL"), (167, "CFD"), (207, "BEST"), (6008, "120549942"),
            (6411, "20/CFD/STK")], 1);
        assert!(cfd_check_reply(&mut context, conn.as_mut(), &mut hb, &name, &answer));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(asked_kinds(&frames), ["permission 20/CFD/STK"]);
        assert!(context.cfd_requests.is_empty());
        assert!(!cfd_check_reply(&mut context, conn.as_mut(), &mut hb, &name, &answer), "answered once");
        permission_reply(&mut context, &fix::field_view(&permission_answer("20/CFD/STK", "0")));
        // A second volatility order of the underlying: 460, no check.
        context.pending_orders.push(vol(92));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(shared.orders.drain_order_errors(), [(92, 460, "No trading permissions.".to_string())]);

        // A limit option order: 460, no check.
        let limit = OrderRequest::SubmitLimit { order_id: 93, instrument: 0, side: Side::Buy, qty: 1, price: px(1.0) };
        let (_, shared, mut context, mut conn, mut server) = order_by_con_id("692808239", "SMART", limit, &AAPL_CALL);
        permission_reply(&mut context, &fix::field_view(&permission_answer("2/OPT/R", "0")));
        assert!(drain_frames(&mut context, &shared, &mut conn, &mut server).is_empty());
        assert_eq!(shared.orders.drain_order_errors(), [(93, 460, "No trading permissions.".to_string())]);
        assert!(context.cfd_checked.is_empty());
    }

    /// A session with MiFID (ibx#500): the account's config id and its
    /// covered groups, and a stock definition of the group `EQ`.
    fn mifid_session(context: &mut Context, manager: bool, groups: &str) {
        context.order_records_any.insert(265598);
        context.mifid_manager = manager;
        context.mifid_config_id = "CFG1".into();
        context.mifid_groups = groups.split('+').map(String::from).collect();
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            sec_type: "STK".into(), types_known: true, mifid_group: "EQ".into(), ..Default::default()
        });
    }

    fn mifid_order(order_id: OrderId, values: [&str; 4]) -> OrderRequest {
        OrderRequest::SubmitLimitEx {
            order_id, instrument: 0, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE, tif: b'0',
            attrs: crate::types::OrderAttrs { mifid: values.map(String::from), ..Default::default() },
        }
    }

    // ibx#500 (the reference 10.51's rules, not observed yet): on a session
    // with MiFID, a covered order carries its Decision Maker and the three
    // other values; one with a Decision Algo but no Decision Maker goes;
    // one with neither is refused (10234) and not sent; an order of a
    // group not covered carries the three other values only; without the
    // feature nothing is sent.
    #[test]
    fn mifid_values_and_the_decision_maker_rule() {
        let mifid = |f: &Vec<(u32, String)>| f.iter().filter(|(t, _)| matches!(*t, 8237 | 8243 | 8254 | 8255)).cloned().collect::<Vec<_>>();
        let pair = |t: u32, v: &str| (t, v.to_string());
        let (frames, context) = session_frames(|c| mifid_session(c, true, "EQ+BOND"), vec![
            mifid_order(1, ["DM1", "DA1", "ET1", "EA1"]),
            mifid_order(2, ["", "", "", ""]),
            mifid_order(3, ["", "DA3", "", ""]),
        ], 2);
        assert_eq!(mifid(&frames[0]), [pair(8237, "DM1"), pair(8243, "DA1"), pair(8254, "ET1"), pair(8255, "EA1")]);
        assert_eq!(mifid(&frames[1]), [pair(8243, "DA3")]);
        assert!(context.unsent.contains_key(&2), "refused, not sent");
        let (frames, _) = session_frames(|c| mifid_session(c, true, "BOND"), vec![mifid_order(4, ["DM4", "", "ET4", ""])], 1);
        assert_eq!(mifid(&frames[0]), [pair(8254, "ET4")], "not covered: no Decision Maker");
        let (frames, _) = session_frames(|c| mifid_session(c, true, "*"), vec![mifid_order(5, ["DM5", "", "", ""])], 1);
        assert_eq!(mifid(&frames[0]), [pair(8237, "DM5")], "all groups covered");
        let (frames, _) = session_frames(|c| mifid_session(c, false, "*"), vec![mifid_order(6, ["DM6", "DA6", "ET6", "EA6"])], 1);
        assert!(mifid(&frames[0]).is_empty(), "no MiFID on the session: dropped");
        // A modification restates the values; only a new Decision Maker
        // replaces the old one.
        let new_values = crate::types::OrderAttrs { mifid: ["DM2", "DA2", "ET2", "EA2"].map(String::from), ..Default::default() };
        let (frames, _) = session_frames(|c| mifid_session(c, true, "EQ"), vec![
            mifid_order(7, ["DM1", "DA1", "ET1", ""]),
            modify_limit(7, 101 * P, new_values),
        ], 2);
        assert_eq!(tag(&frames[1], 35), Some("G"));
        assert_eq!(mifid(&frames[1]), [pair(8237, "DM2"), pair(8243, "DA1"), pair(8254, "ET1")]);
    }

    fn tier_order(order_id: OrderId, name: &str, value: &str) -> OrderRequest {
        OrderRequest::SubmitLimitEx {
            order_id, instrument: 0, side: Side::Buy, qty: 1, price: 100 * crate::types::PRICE_SCALE, tif: b'0',
            attrs: crate::types::OrderAttrs { soft_dollar_tier: (name.into(), value.into()), ..Default::default() },
        }
    }

    // ibx#506 (the reference 10.51's rules, not observed yet): with tiers
    // for the contract's market type, an order with one of them carries
    // its name and value; one with another tier is refused (10155) and not
    // sent; one with no tier goes as before. With no tiers for that market
    // the tier is dropped. A modification restates the tier.
    #[test]
    fn soft_dollar_tiers_are_sent_or_refused() {
        let tier = |f: &Vec<(u32, String)>| f.iter().filter(|(t, _)| matches!(*t, 6519 | 6520)).cloned().collect::<Vec<_>>();
        let pair = |t: u32, v: &str| (t, v.to_string());
        let session = |market: &'static str| move |c: &mut Context| {
            c.order_records_any.insert(265598);
            c.soft_dollar_tiers.insert("USSTK".into(), vec![("0".into(), "ABC".into()), ("1".into(), "DEF".into())]);
            c.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
                sec_type: "STK".into(), types_known: true, market_type: market.into(), ..Default::default()
            });
        };
        let (frames, context) = session_frames(session("USSTK"), vec![
            tier_order(1, "1", "DEF"),
            tier_order(2, "2", "XYZ"),
            tier_order(3, "", ""),
            modify_limit(1, 101 * P, Default::default()),
        ], 3);
        assert_eq!(tier(&frames[0]), [pair(6519, "1"), pair(6520, "DEF")]);
        assert!(tier(&frames[1]).is_empty(), "no tier, nothing");
        assert_eq!(tag(&frames[2], 35), Some("G"));
        assert_eq!(tier(&frames[2]), [pair(6519, "1"), pair(6520, "DEF")], "restated");
        assert!(context.unsent.contains_key(&2), "refused, not sent");
        let (frames, _) = session_frames(session("EUSTK"), vec![tier_order(4, "9", "ZZZ")], 1);
        assert!(tier(&frames[0]).is_empty(), "no tiers for that market: dropped");
    }

    // ibx#507 (the reference 10.51's rules, not observed yet): on an FA
    // session an order carries its group, method (AvailableEquity for a
    // group without one), model code and, for a percentage method, its
    // percentage, restated on a modification; elsewhere nothing.
    #[test]
    fn fa_allocations_go_out_on_an_fa_session_only() {
        let fa = |f: &Vec<(u32, String)>| f.iter().filter(|(t, _)| matches!(*t, 6160 | 6159 | 6164 | 6700)).cloned().collect::<Vec<_>>();
        let pair = |t: u32, v: &str| (t, v.to_string());
        let order = |id: OrderId, values: [&str; 5]| limit_ex(id, Side::Buy, 100 * P,
            crate::types::OrderAttrs { fa: values.map(String::from), ..Default::default() });
        let (frames, _) = session_frames(|c| c.fa_session = true, vec![
            order(1, ["Grp", "PctChange", "50", "M1", ""]),
            order(2, ["Grp", "", "", "Core", ""]),
            modify_limit(1, 101 * P, crate::types::OrderAttrs { fa: ["Grp", "PctChange", "50", "M1", ""].map(String::from), ..Default::default() }),
            order(4, ["", "", "", "", "U7"]),
            order(5, ["", "", "", "", "All"]),
        ], 5);
        assert_eq!(fa(&frames[0]), [pair(6160, "Grp"), pair(6159, "PctChange"), pair(6700, "M1"), pair(6164, "50")]);
        assert_eq!(fa(&frames[1]), [pair(6160, "Grp"), pair(6159, "AvailableEquity")], "the default method; Core is no model");
        assert_eq!(tag(&frames[2], 35), Some("G"));
        assert_eq!(fa(&frames[2]), [pair(6160, "Grp"), pair(6159, "PctChange"), pair(6700, "M1"), pair(6164, "50")]);
        // The order's own account; a group goes to the logon's.
        assert_eq!(tag(&frames[3], 1), Some("U7"));
        assert!(fa(&frames[3]).is_empty());
        assert_eq!(tag(&frames[4], 1), Some("DU1"));
        assert_eq!(fa(&frames[4]), [pair(6160, "All"), pair(6159, "AvailableEquity")]);
        assert_eq!(tag(&frames[0], 1), Some("DU1"));
        let (frames, _) = session_frames(|_| {}, vec![order(3, ["Grp", "NetLiq", "", "M1", "U7"])], 1);
        assert_eq!(tag(&frames[0], 1), Some("DU1"), "not an FA session: the logon's account");
        assert!(fa(&frames[0]).is_empty(), "not an FA session: dropped");
    }

    // ibx#655: a quantity the fixed point cannot hold goes out as the
    // caller wrote it, on the order and on its modification.
    #[test]
    fn a_quantity_text_goes_out_as_given() {
        let attrs = || crate::types::OrderAttrs { qty_text: "0.123456789".into(), ..Default::default() };
        let (frames, _) = session_frames(|_| {}, vec![
            limit_ex(1, Side::Buy, 100 * P, attrs()),
            modify_limit(1, 101 * P, attrs()),
        ], 2);
        assert_eq!(tag(&frames[0], 38), Some("0.123456789"));
        assert_eq!(tag(&frames[1], 38), Some("0.123456789"));
        let (frames, _) = session_frames(|_| {}, vec![limit_ex(2, Side::Buy, 100 * P, Default::default())], 1);
        assert_eq!(tag(&frames[0], 38), Some("1"), "the fixed point otherwise");
    }

    // ibx#507 (the reference 10.51's rules, not observed yet): with the
    // session's groups, an order with no method takes its group's own,
    // AvailableEquity for a group not stored, none for a profile; in the
    // unified mode 10226 for another method than the group's, no method
    // for a group without one, a method with a profile. While a save is
    // not answered, an order on a group waits.
    #[test]
    fn fa_orders_with_the_stored_groups() {
        let fa = |f: &Vec<(u32, String)>| f.iter().filter(|(t, _)| matches!(*t, 6160 | 6159)).cloned().collect::<Vec<_>>();
        let pair = |t: u32, v: &str| (t, v.to_string());
        let order = |id: OrderId, group: &str, method: &str| limit_ex(id, Side::Buy, 100 * P,
            crate::types::OrderAttrs { fa: [group, method, "", "", ""].map(String::from), ..Default::default() });
        let setup = |c: &mut Context| {
            c.fa_session = true;
            c.fa.unified = true;
            c.fa.groups.insert("G".into(), Some("NetLiq".into()));
            c.fa.groups.insert("E".into(), None);
            c.fa.profiles.insert("P".into());
        };
        let (frames, _) = session_frames(setup, vec![
            order(1, "G", ""),
            order(2, "G", "PctRoll"),
            order(3, "E", ""),
            order(4, "P", "NetLiq"),
            order(5, "P", ""),
            order(6, "Other", ""),
        ], 3);
        assert_eq!(frames.len(), 3, "2, 3 and 4 refused");
        assert_eq!(fa(&frames[0]), [pair(6160, "G"), pair(6159, "NetLiq")]);
        assert_eq!(fa(&frames[1]), [pair(6160, "P")]);
        assert_eq!(fa(&frames[2]), [pair(6160, "Other"), pair(6159, "AvailableEquity")]);

        let (frames, context) = session_frames(|c| { setup(c); c.fa.pend_for_test(); }, vec![order(7, "G", ""), order(8, "", "")], 1);
        assert_eq!(frames.len(), 1, "the order with no group goes");
        assert_eq!(context.rth_parked.len(), 1, "the order on a group waits");
    }

    /// A session that allows price management, with the paper exclusion
    /// list and a stock definition that has the price check key.
    fn price_mgmt_session(context: &mut Context) {
        context.price_mgmt_feature = true;
        context.price_mgmt_exclusions = Some(crate::engine::price_mgmt::parse_exclusions(
            "*/CMDTY;*/CRYPTO;*/FUND;*/IOPT;*/SLB"));
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            sec_type: "STK".into(), types_known: true, price_chk: true, moc: true, ..Default::default()
        });
    }

    // ibx#546: an order directed to ARCA, whose list there lacks the price
    // check key, carries the flag from the contract's SMART definition,
    // as the reference sent it (paper, 13/10/2026).
    #[test]
    fn a_directed_order_takes_the_flag_of_the_smart_definition() {
        let (frames, _) = session_frames(|c| {
            // Records known: this test is about another rule (ibx#706).
            c.order_records_any.insert(265598);
            price_mgmt_session(c);
            c.market.set_routing(0, "STK", "ARCA");
            c.bypass_redirect_warning = true;
            c.rth_types.insert((265598, "ARCA".to_string()), crate::engine::outside_rth::RthTypes {
                sec_type: "STK".into(), types_known: true, price_chk: false, smart: true, ..Default::default()
            });
        }, vec![limit_ex(1, Side::Buy, 230 * P, priced(None))], 1);
        assert_eq!((tag(&frames[0], 100), tag(&frames[0], 8339)), (Some("ARCA"), Some("1")));
    }

    fn priced(value: Option<bool>) -> crate::types::OrderAttrs {
        crate::types::OrderAttrs { use_price_mgmt_algo: value, ..Default::default() }
    }

    // ibx#492, ib-agent captures/200 (28/09/2026): a stock limit order
    // with the value unset or on carries the flag, last of the attributes,
    // on the new order and on the replace; with the value off it
    // has none, and the reference never writes it off. With no other
    // attribute, the reference 10.51 writes it before the origin field on a new order
    // (paper, 13/10/2026, ibx#666; 10.40 wrote it after).
    #[test]
    fn a_stock_limit_carries_the_flag_unless_set_off() {
        for (value, on) in [(None, true), (Some(true), true), (Some(false), false)] {
            let (frames, _) = session_frames(price_mgmt_session, vec![
                limit_ex(1, Side::Buy, 230 * P, priced(value)),
                modify_limit(1, 231 * P, priced(value)),
            ], 2);
            for f in &frames {
                assert_eq!(tag(f, 8339), on.then_some("1"), "{value:?} {f:?}");
                if on {
                    let next = if tag(f, 35) == Some("D") { 6122 } else { 38 };
                    assert_eq!(pos(f, 8339) + 1, pos(f, next), "last of the attributes: {f:?}");
                }
            }
        }
        // A plain limit order is unset.
        let (frames, _) = session_frames(price_mgmt_session, vec![OrderRequest::SubmitLimit {
            order_id: 2, instrument: 0, side: Side::Buy, qty: 1, price: 230 * P,
        }], 1);
        assert_eq!(tag(&frames[0], 8339), Some("1"));
    }

    // ibx#492 (ORDER-PRICEMGMT.md 4.2, captures 28/09 and 01/10/2026):
    // market, stop and trailing stop orders have no flag, nor the stop
    // child of a bracket; MOC and stop limit have it.
    #[test]
    fn the_flag_skips_market_stop_and_trailing_types() {
        use crate::types::OrderKind as K;
        let ex = |order_id, kind| OrderRequest::SubmitEx { order_id, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'0', attrs: priced(Some(true)) };
        let cases = [
            (ex(1, K::Market), false),
            (ex(2, K::Stop { stop_price: 90 * P }), false),
            (ex(3, K::Mit { stop_price: 90 * P }), false),
            (ex(4, K::TrailingStop { trail_amt: P, trail_stop_price: 0 }), false),
            (ex(5, K::TrailPct { trail_percent: px(1.0), trail_stop_price: 0 }), false),
            (ex(6, K::StopLimit { price: 89 * P, stop_price: 90 * P }), true),
            (ex(7, K::Moc), true),
        ];
        for (req, on) in cases {
            let (frames, _) = session_frames(price_mgmt_session, vec![req], 1);
            assert_eq!(tag(&frames[0], 8339).is_some(), on, "{:?}", tag(&frames[0], 40));
        }
        let (frames, _) = session_frames(price_mgmt_session, vec![OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        }], 3);
        let flags: Vec<bool> = frames.iter().map(|f| tag(f, 8339).is_some()).collect();
        assert_eq!(flags, vec![true, true, false]);
    }

    // ibx#492 (ORDER-PRICEMGMT.md 4.1, 4.3): no flag without the session
    // feature, for an excluded security type, for a type whose preset is
    // not known, or when the definition lacks the price check key.
    #[test]
    fn the_flag_needs_the_feature_the_list_and_the_price_check_key() {
        let lmt = || limit_ex(1, Side::Buy, 230 * P, priced(None));
        let run = |setup: fn(&mut Context)| session_frames(setup, vec![lmt()], 1).0;
        assert!(tag(&run(price_mgmt_session)[0], 8339).is_some());
        assert!(tag(&run(|c| { price_mgmt_session(c); c.price_mgmt_feature = false; })[0], 8339).is_none());
        assert!(tag(&run(|c| {
            price_mgmt_session(c);
            c.price_mgmt_exclusions = Some(crate::engine::price_mgmt::parse_exclusions("*/STK"));
        })[0], 8339).is_none());
        assert!(tag(&run(|c| {
            price_mgmt_session(c);
            c.rth_types.get_mut(&(265598, "BEST".to_string())).unwrap().price_chk = false;
        })[0], 8339).is_none());
        // An option with the value unset: its preset is not known.
        assert!(tag(&run(|c| { price_mgmt_session(c); c.market.set_routing(0, "OPT", "SMART"); })[0], 8339).is_none());
    }

    // ibx#492: an order whose definition is not known waits for it; the
    // definition is asked once, then the order goes with the flag.
    #[test]
    fn the_flag_waits_for_the_contract_definition() {
        let (frames, context) = session_frames(|c| {
            price_mgmt_session(c);
            c.rth_types.clear();
        }, vec![limit_ex(1, Side::Buy, 230 * P, priced(None))], 1);
        assert_eq!(tag(&frames[0], 35), Some("c"), "the definition is asked first");
        assert_eq!(context.rth_parked.len(), 1);
    }

    // The first bracket of a session (paper 01/10/2026): the limit parent
    // waits for its contract definition, so its stop child, which needs
    // none, went out first and the server refused it with 201 "Can't find
    // parent order". The child and an order of the parent's OCA group now
    // wait behind the parent; an order that depends on nothing goes at once.
    #[test]
    fn orders_that_depend_on_a_waiting_order_go_out_after_it() {
        use std::io::Read;
        use crate::types::OrderKind as K;
        let (client, mut server) = crate::protocol::connection::mem_pair();
        server.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let mut context = order_context();
        context.market.register(265598);
        price_mgmt_session(&mut context);
        let types = context.rth_types.remove(&(265598, "BEST".to_string())).unwrap();
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let child = |order_id, kind, oca: &str| OrderRequest::SubmitEx {
            order_id, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'1',
            attrs: crate::types::OrderAttrs { parent_id: if oca.is_empty() { 0 } else { 10 }, oca_group_str: oca.into(), ..Default::default() },
        };
        for req in [
            limit_ex(10, Side::Buy, 100 * P, crate::types::OrderAttrs { oca_group_str: "G".into(), ..Default::default() }),
            child(11, K::Stop { stop_price: 50 * P }, "G"),
            OrderRequest::SubmitEx { order_id: 12, instrument: 0, side: Side::Sell, qty: 1,
                kind: K::Stop { stop_price: 50 * P }, tif: b'0',
                attrs: crate::types::OrderAttrs { oca_group_str: "G".into(), ..Default::default() } },
            child(13, K::Limit { price: 200 * P }, "G"),
            OrderRequest::SubmitEx { order_id: 14, instrument: 0, side: Side::Sell, qty: 1,
                kind: K::Stop { stop_price: 50 * P }, tif: b'0', attrs: Default::default() },
        ] {
            context.pending_orders.push(req);
        }
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);
        assert_eq!(context.rth_parked.iter().map(|r| r.order_id()).collect::<Vec<_>>(), vec![10, 11, 12, 13]);
        // The definition comes: the waiting orders go in their order.
        context.rth_lookups.clear();
        context.rth_types.insert((265598, "BEST".to_string()), types);
        release_rth_parked(&mut context);
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared);

        let mut buf = vec![0u8; 65536];
        let mut len = 0;
        while let Ok(n) = server.read(&mut buf[len..]) {
            if n == 0 { break; }
            len += n;
        }
        let text = String::from_utf8_lossy(&buf[..len]).replace('\x01', "|");
        let sent: Vec<&str> = text.split("|35=").skip(1)
            .map(|f| if f.starts_with('c') { "c" } else { f.split("|6121=").nth(1).and_then(|r| r.split('|').next()).unwrap_or("?") })
            .collect();
        assert_eq!(sent, vec!["c", "14", "10", "11", "12", "13"]);
    }

    // An option order carries the option's terms after its symbol and its
    // multiplier, as the reference's new-order writer: the contract month,
    // the right (1 for a call), the strike, the security type OPT and the
    // multiplier. Without them the server refused
    // the option's conId sent as a stock (201 "Contract does not match
    // supplied contract parameters", paper 03/10/2026).
    // ibx#666 (paper, 13/10/2026): an order on a future given by its conId
    // and exchange alone waits for the contract's definition, and goes out
    // with its symbol, security type and fields, as the reference's message
    // of the same future given by its description.
    #[test]
    fn an_order_given_by_con_id_alone_takes_its_contract_from_the_definition() {
        let (client, mut server) = crate::protocol::connection::mem_pair();
        let mut conn = Some(Connection::new_mem(client));
        let shared = Arc::new(SharedState::new());
        let mut context = order_context();
        context.market.register(815824257);
        context.order_records_any.insert(815824257);
        context.market.set_symbol(0, String::new());
        context.market.set_routing(0, "", "CME");
        context.pending_orders.push(OrderRequest::SubmitLimit { order_id: 7, instrument: 0, side: Side::Buy, qty: 1, price: 1000 * P });
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!((frames.len(), tag(&frames[0], 35)), (1, Some("c")), "the definition first: {frames:?}");
        let id = tag(&frames[0], 320).unwrap().to_string();
        let reply = fix::fix_build(&[(35, "d"), (320, id.as_str()), (6008, "815824257"), (55, "MES"), (167, "FUT"),
            (15, "USD"), (207, "CME"), (200, "202612"), (6035, "MESZ6"), (231, "5"), (6523, "USFUT"), (6431, "LMT/3,LTH/1")], 1);
        assert!(rth_definition_reply(&mut context, &id, &reply));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        let order: Vec<String> = frames.iter().find(|f| tag(f, 35) == Some("D")).expect("sent").iter()
            .filter(|(t, _)| matches!(t, 55 | 200 | 167 | 48 | 22 | 231 | 100 | 15)).map(|(t, v)| format!("{t}={v}")).collect();
        assert_eq!(order.join("|"), "55=MES|200=202612|167=FUT|48=MESZ6|22=101|231=5.00|100=CME|15=USD");
    }

    // ibx#666: the order messages of a future, a CFD and a forex pair, as
    // the reference 10.51 wrote them (paper, 13/10/2026, `.tmp/666`): the
    // contract month of a future after the symbol, the local symbol and
    // its source after the security type, the multiplier, a forex
    // quantity with two decimals.
    #[test]
    fn future_cfd_and_forex_orders_as_the_reference() {
        let run = |sec_type: &str, symbol: &str, exchange: &str, destination: &str, local: &str, month: &str, mult: &str, qty: u32, price: i64| {
            let tags = wire_tags_with(|c| {
                // The pair's presets known: their order has its own test (ibx#729).
                c.order_presets.insert("CASH&tc=EUR".to_string(), None);
                c.order_presets.insert("CASH".to_string(), None);
                c.market.set_symbol(0, symbol.into());
                c.market.set_routing(0, sec_type, exchange);
                c.rth_types.insert((265598, destination.to_string()), crate::engine::outside_rth::RthTypes {
                    rth: true, sec_type: sec_type.into(), local_symbol: local.into(), contract_month: month.into(),
                    multiplier: mult.into(), ..Default::default()
                });
            }, OrderRequest::SubmitLimit { order_id: 9, instrument: 0, side: Side::Buy, qty, price });
            tags.iter().filter(|(t, _)| matches!(t, 38 | 40 | 55 | 200 | 167 | 48 | 22 | 231 | 54 | 59 | 100 | 6210))
                .map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|")
        };
        assert_eq!(run("FUT", "MES", "CME", "CME", "MESZ6", "202612", "5.00", 1, 1000 * P),
            "38=1|40=2|55=MES|200=202612|167=FUT|48=MESZ6|22=101|231=5.00|54=1|59=0|100=CME|6210=CME");
        assert_eq!(run("CFD", "AAPL", "SMART", "BEST", "AAPLn", "", "1.00", 1, 100 * P),
            "38=1|40=2|55=AAPL|167=CFD|48=AAPLn|22=101|231=1.00|54=1|59=0|100=BEST|6210=BEST");
        assert_eq!(run("CASH", "EUR", "IDEALPRO", "IDEALPRO", "EUR.USD", "", "1.00", 20000, P),
            "38=20000.00|40=2|55=EUR|167=CASH|48=EUR.USD|22=101|231=1.00|54=1|59=0|100=IDEALPRO|6210=IDEALPRO");
    }

    // The option's terms from its definition, written as the reference
    // writes a warrant's (ibx#666).
    #[test]
    fn option_orders_carry_the_option_terms() {
        let tags = wire_tags_with(|c| {
            c.market.set_symbol(0, "SPY".into());
            c.market.set_routing(0, "OPT", "SMART");
            c.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
                rth: true, sec_type: "OPT".into(), contract_month: "202610".into(), right: "1".into(), strike: "768".into(),
                multiplier: "100.00".into(), ..Default::default()
            });
        }, OrderRequest::SubmitLimitGtc { order_id: 9, instrument: 0, side: Side::Buy, qty: 1, price: P / 100, outside_rth: false });
        let order: Vec<(u32, &str)> = tags.iter().map(|(t, v)| (*t, v.as_str()))
            .filter(|(t, _)| matches!(t, 55 | 200 | 201 | 202 | 167 | 231 | 54 | 100 | 6008)).collect();
        assert_eq!(order, [(55, "SPY"), (200, "202610"), (201, "1"), (202, "768"), (167, "OPT"), (231, "100.00"),
            (54, "1"), (100, "BEST"), (6008, "265598")]);
    }
}
