use std::sync::Arc;
use std::time::Instant;

use crate::bridge::SharedState;
use crate::config::{chrono_free_timestamp, unix_to_ib_utc_dash};
use crate::engine::context::Context;
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{AlgoParams, OrderCondition, OrderId, OrderRequest, OrderStatus, OrderUpdate, Side};

use super::{HeartbeatState, format_price, format_price_ref, format_qty, format_uint};

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

    let orders: Vec<OrderRequest> = context.drain_pending_orders().collect();
    context.api_client_id = shared.reference.api_client_id();
    let conn = match ccp_conn.as_mut() {
        Some(c) => c,
        None => return,
    };
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
            | OrderRequest::Modify { .. } | OrderRequest::SubmitBracket { .. } | OrderRequest::SubmitWhatIf { .. })
        {
            log::warn!("What-if of order {} dropped: only a single new order can be previewed", oid);
            continue;
        }
        // A request that depends on one waiting for its contract definition
        // waits behind it, so the orders go out in the order they were
        // placed, as the reference sends them: a child behind its parent, an
        // order behind another of its OCA group, a request of an order
        // behind that order. A child sent ahead of its parent is refused by
        // the server (201 "Can't find parent order", paper 01/10/2026).
        if waits_behind(&order_req, &context.rth_parked) {
            context.rth_parked.push(rewrap(order_req));
            continue;
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
                        format!("Error validating request.-'bH' : cause - {}", cause));
                    continue;
                }
            }
        }
        // A pegged or protection type the contract's list does not allow
        // on the order's exchange is refused, as the reference (ibx#414,
        // ibx#493).
        match pegged_type_refusal(&order_req, context, conn, hb, shared) {
            Some(false) => { context.rth_parked.push(rewrap(order_req)); continue; }
            Some(true) => continue,
            None => {}
        }
        // Outside RTH: kept only where the reference keeps it, from the
        // contract definition of the order's exchange (ibx#465).
        if !apply_outside_rth(&mut order_req, context, conn, hb, shared) {
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
        // The contract's currency (tag 15), USD when unknown (ibx#466).
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
        }
        // The reference refuses a cancel it cannot apply and sends nothing
        // (ibx#464; captured 23/09/2026): 10147 for an order it does not
        // know, 10148 with the state for one that is finished or has a
        // cancel pending.
        if let OrderRequest::Cancel { order_id } = &order_req {
            if let Some((code, message)) = cancel_refusal(context, *order_id) {
                log::warn!("Cancel of order {} refused: {}", order_id, message);
                shared.orders.push_order_error(*order_id, code, message);
                continue;
            }
        }
        // Snap every price to the contract's tick grid before encoding
        // (ibx#216). The tick comes from the market-data subscription ack;
        // without one it is 0 and prices pass through unchanged.
        if let Some(instrument) = order_req.instrument() {
            order_req.snap_prices(context.market.min_tick_scaled(instrument));
        }
        // A what-if goes out under a ClOrdID of its own and stays out of
        // the order table: an order with the same id is left as it is
        // (ibx#462).
        let held = what_if.then(|| (context.order(oid).copied(), context.modify_versions.get(&oid).copied()));
        if what_if {
            let clord = format!("{}.{}", oid, WHAT_IF_CLORD_BASE + context.next_what_if);
            context.next_what_if = context.next_what_if.wrapping_add(1);
            let instrument = order_req.instrument().unwrap_or(0);
            context.what_ifs.insert(clord.clone(), (oid, instrument));
            context.what_if_send = Some(clord);
        }
        let result = match order_req {
            OrderRequest::SubmitLimit { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let trail_str = format_price(trail_amt);
                let trail_stop_str = format_price(trail_stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#136 capture: amount-based trailing stop carries
                // the trail amount in both 99 (StopPx) and 211 (PegOffset),
                // and requires 18=a (ExecInst = TrailingStop). Without 18,
                // the gateway rejects with "Invalid value in field # 18".
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
                // Optional initial stop trigger (tag 6117), only when set
                // (ib-agent#173).
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, lmt_offset, lmt_price, trail_amt, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, lmt_price.unwrap_or(lmt_offset), b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price(lmt_offset);
                let price_str = lmt_price.map(format_price);
                let trail_str = format_price(trail_amt);
                let trail_stop_str = format_price(trail_stop_price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#136 capture: TRAIL LIMIT uses OrdType=TSL (not P),
                // does NOT carry tag 44 (gateway derives the limit price from
                // 6370 + the activation reference), does NOT carry tag 18, and
                // carries the trail amount in both 99 and 211. The 6370
                // LimitPriceOffset is the limit-vs-trail offset.
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
                // The absolute limit price in 44 and no 6370, or the offset
                // in 6370 (ib-agent#194).
                match &price_str {
                    Some(price) => fields.push((44, price)),
                    None => fields.push((6370, &offset_str)),
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                // Per ib-agent#156 capture: percent-trail mirrors 99/211 as the
                // percent in decimal form (1.00 for 1%) with 18=a (ExecInst=
                // TrailingStop); without 99/211/18 the gateway rejects with
                // "Invalid value in field # 18". 6268 is the trail unit, 100 =
                // percent, at every percentage (ib-agent#192 B7, ibx#339); it
                // was sent as basis points, right only at exactly 1%.
                let pct_decimal = format!("{:.2}", trail_pct as f64 / 100.0);
                let trail_stop_str = format_price(trail_stop_price);
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
                    (211, &pct_decimal),    // PegOffset = percent as decimal (mirror of 99)
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
            OrderRequest::SubmitTrailingStopPctEx { order_id, instrument, side, qty, trail_pct, tif, attrs, trail_stop_price } => {
                send_order_ex(conn, context, account_id, order_id, instrument, side, qty,
                    crate::types::OrderKind::TrailPct { trail_pct, trail_stop_price }, tif, &attrs)
            }
            OrderRequest::SubmitMoc { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'5', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
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
                let clord = |id: OrderId| format!("{}.{}", id, context.modify_versions.get(&id).copied().unwrap_or(0));
                let parent_str = clord(parent_id);
                let tp_str = clord(tp_id);
                let sl_str = clord(sl_id);
                let entry_str = format_price(entry_price);
                let tp_price_str = format_price(take_profit);
                let sl_price_str = format_price(stop_loss);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price(offset);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                // Per ib-agent#138 capture: Relative shares OrdType=P with
                // Trail and is disambiguated by ExecInst=R. Peg offset goes
                // on tag 211 (not 99 outbound), and there is no tag 44.
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                    (11, format!("{}.{}", order_id, ver)),
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_qty(qty);
                let price_str = format_price(price);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                    cap_str = format_price(price_cap);
                    fields.push((44, &cap_str)); // Price cap
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MKT, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                let clord_str = format!("{}.{}", order_id, ver);
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
                    (11, format!("{}.{}", order_id, ver)),
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
                let clord_str = format!("{}.{}", order_id, new_ver);
                // OrigClOrdID matches whatever the server last recorded for
                // this order (which may pre-date the versioned scheme — ibx#179).
                let orig_clord = context.last_clord.get(&order_id).cloned()
                    .unwrap_or_else(|| format!("{}.{}", order_id, prev_ver));
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
                        if let crate::types::OrderKind::TrailingStopLimit { trail_stop_price, .. } = kind {
                            if trail_stop_price > 0 { r.stop = trail_stop_price; }
                        }
                        Some(r.offset)
                    }
                    None => {
                        let computed = computed_trail_limit_offset(kind, orig.map(|o| o.side));
                        if let (Some(offset), crate::types::OrderKind::TrailingStopLimit { lmt_price: Some(price), trail_stop_price, .. }) = (computed, kind) {
                            context.trail_limit_reported.insert(order_id, crate::engine::context::TrailLimitReported {
                                offset, limit: price, stop: trail_stop_price,
                            });
                        }
                        computed
                    }
                };
                let bracket_key = context.bracket_keys.get(&order_id).map(|k| k.to_string());
                let fields = modify_fields(
                    &clord_str, &orig_clord, account_id, qty, side_str, &symbol,
                    &sec_type_str, &con_id_str, kind, tif, &attrs, trail_limit_offset,
                    bracket_key.as_deref(), context.price_mgmt_send);
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                conn.send_fix(&refs)
            }
        };
        context.short_sale_send = None;
        if let Some((order, version)) = held {
            let clord = context.what_if_send.take().unwrap_or_default();
            match order {
                Some(o) => context.insert_order(o),
                None => context.remove_order(oid),
            }
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
                    context.remove_order(oid);
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

/// A cancel as the reference writes it (ibx#464; captured 25/09/2026):
/// `11` the order's next ClOrdID version, `41` the ClOrdID the server has on
/// record, then quantity, side, account, contract and `6944`: `SEL` for a
/// cancel of one order, `ALL` for the global cancel. The new version and the
/// cancel's id are recorded, so its reports are read as the cancel's.
fn cancel_fields(context: &mut Context, account_id: &str, order_id: crate::types::OrderId, scope: &str) -> Vec<(u32, String)> {
    let prev_ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
    let new_ver = prev_ver + 1;
    context.modify_versions.insert(order_id, new_ver);
    let clord = format!("{}.{}", order_id, new_ver);
    // OrigClOrdID must match exactly what the server has on record: the
    // string last seen on the wire (ibx#179 — orders recorded without a
    // `.{ver}` suffix), else the versioned scheme (a cancel right after
    // the place, before its ack).
    let orig_clord = context.last_clord.get(&order_id).cloned()
        .unwrap_or_else(|| format!("{}.{}", order_id, prev_ver));
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

/// The reference's answer to a cancel it does not send (ibx#464): the order
/// is unknown (10147), or finished or already pending cancel (10148, with
/// the state). `None` when the cancel goes out.
fn cancel_refusal(context: &Context, order_id: crate::types::OrderId) -> Option<(i64, String)> {
    let state = match context.order(order_id) {
        Some(o) if o.status == OrderStatus::PendingCancel => OrderStatus::PendingCancel,
        Some(_) => return None,
        None => match context.finished_status(order_id) {
            Some(status) => status,
            None => return Some((
                10147,
                format!("OrderId {} that needs to be cancelled is not found.", order_id),
            )),
        },
    };
    Some((
        10148,
        format!(
            "OrderId {} that needs to be cancelled can not be cancelled, state: {}.",
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
fn clearing_away(intent: &str, account_id: &str, super_user: bool) -> bool {
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
fn short_sale_check(
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

/// Convert Side to FIX tag 54 value.
fn fix_side(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
        Side::ShortSell => "5",
    }
}

/// Synthesize the PendingCancel phase when a cancel request goes out
/// (ibx#211): the server acks a normal cancel with the terminal code only —
/// it never sends the pending-cancel code — so without this local
/// transition consumers jump straight from Submitted to Cancelled. The
/// server's ack (or a fill that raced the cancel) then advances the status;
/// a cancel reject restores the working status via the forced setter.
fn synthesize_pending_cancel(
    context: &mut Context,
    shared: &Arc<SharedState>,
    order_id: crate::types::OrderId,
) {
    if !context.update_order_status(order_id, OrderStatus::PendingCancel) {
        return; // unknown order, already terminal, or already pending-cancel
    }
    if let Some(order) = context.order(order_id).copied() {
        shared.orders.push_order_update(OrderUpdate {
            avg_fill_price: 0,
            order_id,
            instrument: order.instrument,
            status: OrderStatus::PendingCancel,
            filled_qty_fixed: order.filled_fixed,
            remaining_qty_fixed: order.qty_fixed - order.filled_fixed,
            perm_id: 0,
            parent_id: 0,
            timestamp_ns: context.now_ns(),
        });
    }
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
        format!("{}.{}", parent_id, context.modify_versions.get(&parent_id).copied().unwrap_or(0))
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

/// Map the OCA type code (1..=4) to its tag 6209 wire label. 0/unset and
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
    context: &Context,
    instrument: crate::types::InstrumentId,
    fields: &[(u32, &str)],
) -> std::io::Result<()> {
    let short_sale: Fields = match &context.short_sale_send {
        Some(s) if fields.iter().any(|&(t, v)| t == 54 && v == "5") => short_sale_fields(s),
        _ => Vec::new(),
    };
    let con_id = context.market.con_id(instrument).unwrap_or(0);
    let con_id_str = if con_id > 0 { con_id.to_string() } else { String::new() };
    let what_if = context.what_if_send.as_deref();
    // The API order id, the id part of the order's own ClOrdID; it is an
    // int in the API, so a larger id is left out.
    let order_id = fields.iter().find(|&&(t, _)| t == 11)
        .and_then(|&(_, v)| v.split('.').next())
        .filter(|id| id.parse::<i32>().is_ok_and(|n| n >= 0));
    let client_id = context.api_client_id.to_string();
    let stock = fields.iter().any(|&(t, v)| t == 167 && v == "STK");
    let mut out: Vec<(u32, &str)> = Vec::with_capacity(fields.len() + short_sale.len() + 10);
    for &(tag, value) in fields {
        match (tag, what_if) {
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
    // The touched trigger of MIT and LIT, their stop price (ibx#263).
    let touched_trigger = touched_type(&out).then(|| out.iter().find(|&&(t, _)| t == 99).map(|&(_, v)| v)).flatten();
    if let Some(v) = touched_trigger.filter(|_| !out.iter().any(|&(t, _)| t == 6117)) { out.push((6117, v)); }
    if con_id > 0 { out.push((6008, &con_id_str)); }
    if what_if.is_some() { out.push((6091, "1")); }
    out.push((6122, "c"));
    // The price management flag, decided for the request before encoding;
    // never on the order types it does not go with (ibx#492).
    if context.price_mgmt_send && !crate::engine::price_mgmt::excluded_frame(&out) { out.push((8339, "1")); }
    if let Some(id) = order_id { out.push((6121, id)); }
    out.push((6119, &client_id));
    // A stock's multiplier, as the reference writes it.
    if stock { out.push((231, "1.00")); }
    out.push((6088, "Socket"));
    out.push((6211, ""));
    out.push((6238, ""));
    in_reference_order(&mut out);
    conn.send_fix(&out)
}

/// Whether a new order of this frame's type writes the trigger method
/// (6115), as the reference (ibx#263): it writes it on a new order whose
/// type is a stop or touched type (`jattrib.attribs.TriggerMethod.b`, true
/// for `jibtypes.s.aw()`): STP, STP LMT, STP PRT, TRAIL, TRAIL LIMIT, MIT,
/// LIT, TRAIL MIT and TRAIL LIT (captured on all but STP PRT), never on a
/// replace. A trailing stop is the pegged type with the trailing letter.
fn has_trigger_method(fields: &[(u32, &str)]) -> bool {
    matches!(field(fields, 40), Some("3" | "4" | "SP" | "J" | "LT")) || trailing_type(fields)
}

/// Whether the frame's type is a trailing one, which writes the trail
/// unit (6268) on a new order and a replace (`jattrib.attribs.TrailingUnit.b`,
/// true for `jibtypes.s.as()`): TRAIL, TRAIL LIMIT, TRAIL MIT, TRAIL LIT
/// (ibx#263; captured with 6268 on all four, new orders and replaces).
fn trailing_type(fields: &[(u32, &str)]) -> bool {
    match field(fields, 40) {
        Some("TSL" | "TMIT" | "TLIT") => true,
        Some("P") => field(fields, 18).is_some_and(|v| v.split(' ').next() == Some("a")),
        _ => false,
    }
}

/// Whether the frame's type is a touched one, whose stop price the
/// reference keeps as the touched trigger and writes in 6117 among the
/// order attributes (`jclient.pe.D(double)` for `jibtypes.s.v()`,
/// `jattrib.attribs.TouchedTrigger.b`): MIT and LIT (ibx#263, captured in
/// ib-agent#199 on new orders and replaces).
fn touched_type(fields: &[(u32, &str)]) -> bool {
    matches!(field(fields, 40), Some("J" | "LT"))
}

fn field<'a>(fields: &[(u32, &'a str)], tag: u32) -> Option<&'a str> {
    fields.iter().find(|&&(t, _)| t == tag).map(|&(_, v)| v)
}

/// Put new-order fields in the order of the reference's new-order writer
/// (ibx#375; the writer read from the code, its order checked against the
/// captured frames of ib-agent#192 B8 and B9). The sort is stable: the
/// pairs of a repeating group (algo parameters, conditions) share one
/// place and keep their order. The order attributes are written by the
/// reference in no fixed order; they get one fixed place each here, as
/// seen in captured frames where it could be.
fn in_reference_order(fields: &mut [(u32, &str)]) {
    let touched = touched_type(fields);
    fields.sort_by_key(|&(tag, _)| rank_in_frame(tag, touched));
}

/// The place of a field, the touched trigger of MIT and LIT among the
/// order attributes (after outside-RTH, as captured in ib-agent#199), the
/// stop trigger of the other types before them.
fn rank_in_frame(tag: u32, touched: bool) -> u16 {
    if touched && tag == 6117 { 76 } else { reference_rank(tag) }
}

/// The place of a field in the reference's new-order writer (ibx#375).
fn reference_rank(tag: u32) -> u16 {
    match tag {
        fix::TAG_MSG_TYPE => 0,
        fix::TAG_SENDING_TIME => 1,
        11 => 10,
        44 => 20,
        99 => 21,
        1 => 30,
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
        // Order attributes.
        583 => 70,
        6010 => 71,
        6122 => 72,
        // The bracket key, after the origin (ib-agent captures/pd-orders,
        // 01/10/2026).
        6531 => 73,
        6433 => 75,
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
        5920 => 89,
        6941 => 90,
        6938 => 91,
        6939 => 92,
        6580 => 93,
        6942 => 94,
        // The price management flag, most often last of the attributes in
        // the reference's frames (captures of 28/09 and 01/10/2026).
        8339 => 95,
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
        55 => 124,
        167 => 125,
        231 => 126,
        54 => 127,
        59 => 128,
        6436 => 129,
        100 => 131,
        6210 => 132,
        6008 => 133,
        6209 => 134,
        6088 => 135,
        6091 => 136,
        6107 => 137,
        6128 => 138,
        6151 => 139,
        // The condition group.
        6136 | 6222 | 6137 | 6126 | 6123 | 6124 | 6127 | 6125 | 6223 | 6245 | 6263 | 6246 | 6947 => 140,
        15 => 150,
        6211 => 152,
        6238 => 153,
        _ => u16::MAX,
    }
}

/// Version part of the what-if ClOrdIDs, far above any replace count, so
/// a preview never takes an id an order of the same number uses (ibx#462).
const WHAT_IF_CLORD_BASE: u32 = 1_000_000;

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
        (6258, format_price(trigger_price).to_string()),    // Trigger price
        (6259, format_price(adjusted_stop_price).to_string()), // Adjusted stop price
    ];
    if adjusted_stop_limit_price > 0 {
        tags.push((6262, format_price(adjusted_stop_limit_price).to_string())); // Adjusted stop limit price
    }
    // When the stop converts to a trailing type, carry the trailing
    // amount (6260) and its unit (6269: 0=amount, 100=percent).
    // Captured in ib-agent#167 (ibx#225).
    if matches!(adjusted_order_type,
        crate::types::AdjustedOrderType::Trail
        | crate::types::AdjustedOrderType::TrailLimit)
    {
        tags.push((6260, format_price(adjusted_trailing_amount).to_string()));
        tags.push((6269, adjustable_trailing_unit.to_string()));
    }
    tags
}

/// Order fields as (tag, value) pairs.
type Fields = Vec<(u32, String)>;

/// The fields of a pegged-to-market or pegged-to-midpoint order, as the
/// reference writes them (ibx#414; pegged to midpoint captured in
/// ib-agent#192 B8c, pegged to market from the code read): the limit
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
    bracket_key: Option<&str>,
    price_mgmt: bool,
) -> Vec<(u32, String)> {
    use crate::types::OrderKind as K;
    let p = |v: crate::types::Price| format_price(v).to_string();

    // Prices go before the account; type-specific fields after the order
    // type. A trail value rides both the stop field and the trail field.
    let mut before_account: Vec<(u32, String)> = Vec::new();
    let mut stop_trigger: Option<String> = None; // restated for STP / STP LMT
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
        K::TrailingStop { trail_amt, .. } => {
            before_account.push((99, p(trail_amt)));
            trail_unit = Some("0");
            after_type.push((211, p(trail_amt)));
            "P"
        }
        K::TrailPct { trail_pct, .. } => {
            let pct = format!("{:.2}", trail_pct as f64 / 100.0);
            before_account.push((99, pct.clone()));
            trail_unit = Some("100");
            after_type.push((211, pct));
            "P"
        }
        K::TrailingStopLimit { lmt_offset, lmt_price, trail_amt, .. } => {
            // An absolute limit price: 44 with the order's offset, as the
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
        K::Mit { stop_price } => {
            before_account.push((99, p(stop_price)));
            touched_trigger = Some(p(stop_price));
            "J"
        }
        K::Lit { price, stop_price } => {
            before_account.push((44, p(price)));
            before_account.push((99, p(stop_price)));
            touched_trigger = Some(p(stop_price));
            "LT"
        }
        K::Mtl => "K",
        K::MktPrt => "U",
        K::StpPrt { stop_price } => { before_account.push((99, p(stop_price))); "SP" }
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
    // The instruction field as on the new order (`jclient.pe.gI()`, also
    // written by the replace writer `jclient.pe.d`): the type letter, G for
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
    f.push((6122, "c".to_string()));
    // The bracket key, restated on every replace (ibx#248), as the
    // reference.
    if let Some(key) = bracket_key { f.push((6531, key.to_string())); }
    // Outside-RTH only when the order has it: a replace without it leaves
    // the order regular-hours only (ibx#247).
    if attrs.outside_rth { f.push((6433, "1".to_string())); }
    // The touched trigger of MIT and LIT, restated (ib-agent#199, ibx#263).
    if let Some(t) = touched_trigger { f.push((6117, t)); }
    if let Some(u) = trail_unit { f.push((6268, u.to_string())); }
    f.extend(bench_attrs);
    // The price management flag, last of the attributes (ibx#492).
    if price_mgmt && !crate::engine::price_mgmt::excluded_kind(&kind) { f.push((8339, "1".to_string())); }
    // The algo block, restated as on the new order (`jclient.pe.d` writes
    // it through the same `jclient.pe.j`; ibx#263).
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
    f.push((6088, "Socket".to_string()));
    f.push((6211, String::new()));
    f.push((6238, String::new()));
    f
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
    let Some(con_id) = context.market.con_id(instrument) else { return Definition::NoContract };
    let (_, destination) = context.market.order_routing(instrument);
    let key = (con_id, destination);
    if !context.rth_types.contains_key(&key) {
        if !context.rth_lookups.iter().any(|(_, k, _)| *k == key) {
            let id = format!("ibxrth{}", context.next_rth_lookup);
            context.next_rth_lookup = context.next_rth_lookup.wrapping_add(1);
            let ts = chrono_free_timestamp();
            let con_id_str = con_id.to_string();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &id),
                (321, "2"),
                (146, "1"),
                (6008, &con_id_str),
                (6004, &key.1),
            ]);
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
            Some(true)
        }
        Definition::Known(..) => None,
    }
}

/// Outside RTH on a request (ibx#465). A request of an order that already
/// waits, or with outside-RTH whose contract definition is not known yet,
/// waits (false): the definition is asked once. Otherwise outside-RTH is
/// dropped where the reference drops it, with warning 2109 on a new order
/// (none on a replace), and the request goes on (true).
fn apply_outside_rth(
    req: &mut OrderRequest,
    context: &mut Context,
    conn: &mut Connection,
    hb: &mut HeartbeatState,
    shared: &Arc<SharedState>,
) -> bool {
    use crate::engine::outside_rth as rth;
    let oid = req.order_id();
    // A cancel-all waits behind the waiting orders, so it reaches them.
    if !context.rth_parked.is_empty()
        && (matches!(req, OrderRequest::CancelAll { .. }) || context.rth_parked.iter().any(|r| r.order_id() == oid))
    {
        return false;
    }
    let is_new = !matches!(req, OrderRequest::Modify { .. });
    let Some((instrument, kind, tif, _, outside_rth)) = rth::rth_parts(req) else { return true };
    if !*outside_rth { return true; }
    let Some(instrument) = instrument.or_else(|| context.order(oid).map(|o| o.instrument)) else { return true };
    let ibkrats = context.market.exchange(instrument) == "IBKRATS";
    let (types, routed) = match definition(context, conn, hb, instrument) {
        Definition::NoContract => return true,
        Definition::Waiting => return false,
        Definition::Known(types, routed) => (types, routed),
    };
    let exchange = if ibkrats { "IBKRATS" } else { routed.as_str() };
    if !rth::outside_rth_applies(kind, tif, exchange, types) {
        *outside_rth = false;
        if is_new {
            shared.orders.push_order_error(oid, 2109, rth::OUTSIDE_RTH_IGNORED.to_string());
        }
    }
    true
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
    match definition(context, conn, hb, instrument) {
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
        OrderRequest::CancelAll { .. } => Vec::new(),
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
    let tags = fix::fix_parse(msg);
    let tokens: Vec<String> = tags.get(&6431).map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
    let def = crate::control::contracts::parse_secdef_response(msg);
    let types = crate::engine::outside_rth::RthTypes::from_definition(
        &tokens,
        tags.get(&6523).map(String::as_str).unwrap_or(""),
        def.as_ref().map(|d| d.sec_type.to_api_str()).unwrap_or(""),
        def.as_ref().map(|d| d.currency.as_str()).unwrap_or(""),
    );
    log::info!("Outside RTH definition for con_id {} on {}: {:?}", key.0, key.1, types);
    log::debug!("Order-type list for con_id {} on {}: {:?}", key.0, key.1, tokens);
    context.rth_types.insert(key, types);
    release_rth_parked(context);
    true
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

/// The waiting requests go back ahead of the queue, in their order.
fn release_rth_parked(context: &mut Context) {
    let parked = std::mem::take(&mut context.rth_parked);
    context.pending_orders.prepend(parked);
}

/// The limit offset of a TRAIL LIMIT set by its limit price, as the
/// reference computes it when the order has none (ib-agent#195): the stop
/// price minus the limit price for a sell, the limit price minus the stop
/// price for a buy.
fn computed_trail_limit_offset(kind: crate::types::OrderKind, side: Option<Side>) -> Option<crate::types::Price> {
    match kind {
        crate::types::OrderKind::TrailingStopLimit { lmt_price: Some(price), trail_stop_price, .. }
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
    // allow them (ibx#425, from the code read, not captured).
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
    // GTD expiry: date-only -> tag 432; time-precise -> tag 126 (UTC).
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
        fields.push((9813, format_price(attrs.discretionary_amt).to_string()));
    }
    if attrs.sweep_to_fill {
        fields.push((6102, "1".to_string()));
    }
    if attrs.cash_qty > 0 {
        fields.push((5920, format_price(attrs.cash_qty).to_string()));
    }
    // Condition tags (6136+ framework). The reference always sends both
    // flags, 0 or 1, before the count: ignore-RTH rides 6128 and cancel-order
    // 6151. ibx had them the other way round and sent them only when set
    // (ib-agent#192 B2, ibx#327).
    if !attrs.conditions.is_empty() {
        let flag = |on: bool| if on { "1" } else { "0" }.to_string();
        fields.push((6128, flag(attrs.conditions_ignore_rth)));
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
    };
    context.insert_order(crate::types::Order::new(
        order_id, instrument, side, qty, track_price, ord_type_byte, tif, track_stop,
    ));

    let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
    let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
    let now = chrono_free_timestamp().to_string();
    let tif_str = tif_str(tif);

    let mut fields: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
        (fix::TAG_SENDING_TIME, now.clone()),
        (11, format!("{}.{}", order_id, ver)),
        (1, account_id.to_string()),
        (55, symbol),
        (54, fix_side(side).to_string()),
        (38, format_uint(qty as u64).to_string()),
    ];

    match kind {
        K::Market => fields.push((40, "1".to_string())),
        K::Limit { price } => {
            fields.push((40, "2".to_string()));
            fields.push((44, format_price(price).to_string()));
        }
        K::Stop { stop_price } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price(stop_price).to_string()));
        }
        K::StopLimit { price, stop_price } => {
            fields.push((40, "4".to_string()));
            fields.push((44, format_price(price).to_string()));
            fields.push((99, format_price(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price(stop_price).to_string()));
        }
        K::TrailingStop { trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: amount-based trailing stop carries
            // the trail amount in both 99 and 211 and requires 18=a.
            let t = format_price(trail_amt).to_string();
            fields.push((40, "P".to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            // Optional initial stop trigger (tag 6117), only when set (ib-agent#173).
            if trail_stop_price > 0 { fields.push((6117, format_price(trail_stop_price).to_string())); }
        }
        K::TrailingStopLimit { lmt_offset, lmt_price, trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: TRAIL LIMIT uses OrdType=TSL, no
            // tag 18; trail amount in both 99 and 211; 6370 is the
            // limit-vs-trail offset. An absolute limit price goes in 44
            // instead, with no 6370 (ib-agent#194).
            let t = format_price(trail_amt).to_string();
            fields.push((40, "TSL".to_string()));
            if let Some(price) = lmt_price { fields.push((44, format_price(price).to_string())); }
            fields.push((99, t.clone()));
            if lmt_price.is_none() { fields.push((6370, format_price(lmt_offset).to_string())); }
            fields.push((211, t));
            if trail_stop_price > 0 { fields.push((6117, format_price(trail_stop_price).to_string())); }
        }
        K::TrailPct { trail_pct, trail_stop_price } => {
            // Per ib-agent#156 capture: percent-trail mirrors 99/211 as the
            // percent in decimal form (1.00 for 1%), with 18=a. 6268 is the
            // trail unit, 100 = percent (ib-agent#192 B7, ibx#339).
            let pct_decimal = format!("{:.2}", trail_pct as f64 / 100.0);
            fields.push((40, "P".to_string()));
            fields.push((99, pct_decimal.clone()));
            fields.push((211, pct_decimal));
            fields.push((6268, "100".to_string()));
            if trail_stop_price > 0 { fields.push((6117, format_price(trail_stop_price).to_string())); }
        }
        K::Moc => fields.push((40, "5".to_string())),
        K::Loc { price } => {
            fields.push((40, "B".to_string()));
            fields.push((44, format_price(price).to_string()));
        }
        K::Mit { stop_price } => {
            fields.push((40, "J".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
        K::Lit { price, stop_price } => {
            fields.push((40, "LT".to_string())); // per ib-agent#138
            fields.push((44, format_price(price).to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
        K::Mtl => fields.push((40, "K".to_string())),
        K::MktPrt => fields.push((40, "U".to_string())),
        K::StpPrt { stop_price } => {
            fields.push((40, "SP".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
            // Stop trigger, as the reference (ibx#466).
            fields.push((6117, format_price(stop_price).to_string()));
        }
        K::MidPrice { price_cap } => {
            fields.push((40, "MIDPX".to_string()));
            if price_cap > 0 {
                fields.push((44, format_price(price_cap).to_string()));
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
            // Per ib-agent#138 capture: Relative shares OrdType=P and is
            // disambiguated by 18=R; peg offset on 211; the price cap in 44
            // when set (ib-agent#199, ibx#263).
            fields.push((40, "P".to_string()));
            if price > 0 { fields.push((44, format_price(price).to_string())); }
            fields.push((211, format_price(offset).to_string()));
        }
        // The adjustable tags themselves follow the common block below.
        K::AdjustableStop { stop_price, .. } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
        // As the reference (ibx#415): the starting price in the stop price
        // field, no limit price, the peg instruction; the benchmark
        // attributes follow the common block below.
        K::PegBench { starting_price, .. } => {
            fields.push((40, "PB".to_string()));
            if starting_price > 0 { fields.push((99, format_price_ref(starting_price).to_string())); }
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

/// Tag 18 as the reference builds it (`jclient.pe.gI()`): the letter of a
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

/// The algo block, as the reference writes it after the order attributes
/// (`jattrib.algo.AlgoAttributeMap.a`): the strategy (847, after 849 when
/// the algo has a maximum percentage of volume), then the parameter count
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
        OrderAlgo::Params(params) => {
            let (algo_name, param_strs) = build_algo_tags(params);
            // Tag 849 (maxPctVol) for algos that use it
            match params {
                AlgoParams::Vwap { max_pct_vol, .. }
                | AlgoParams::ArrivalPx { max_pct_vol, .. }
                | AlgoParams::ClosePx { max_pct_vol, .. } => fields.push((849, format!("{}", max_pct_vol))),
                _ => {}
            }
            fields.push((847, algo_name.to_string()));
            // Only parameters that have a value: the reference leaves out
            // one that was not given (an unset start/end time), and the
            // server refuses an empty one with "Invalid value in field
            // # 5957" (ib-agent#192 B9, ibx#405).
            let pairs: Vec<&[String]> = param_strs.chunks(2).filter(|p| !p[1].is_empty()).collect();
            fields.push((5957, pairs.len().to_string()));
            // Emit key/value pairs: 5958=key, 5960=value (repeated)
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

/// The condition block after the two flags, as the reference writes it
/// (ib-agent ORDER-SUBMIT.md 4.2, 4.3; ibx#416): the count, then per
/// condition its type, conjunction and own tags in the reference's order,
/// then the padding. A time condition carries its time right after the
/// operator; numbers have two decimals at least.
fn condition_tags(conditions: &[OrderCondition]) -> Vec<(u32, String)> {
    let mut out = vec![(6136, conditions.len().to_string())];
    for (i, cond) in conditions.iter().enumerate() {
        let conj = if i == conditions.len() - 1 { "n" } else { "a" };
        let op = |is_more: bool| if is_more { ">=" } else { "<=" }.to_string();
        let own: Vec<(u32, String)> = match cond {
            OrderCondition::Price { con_id, exchange, price, is_more, trigger_method } => vec![
                (6126, op(*is_more)),
                (6123, con_id.to_string()),
                (6124, condition_exchange(exchange)),
                (6127, trigger_method.to_string()),
                (6125, format_price_ref(*price).to_string()),
            ],
            OrderCondition::Time { time, is_more } => vec![
                (6126, op(*is_more)),
                (6223, time.clone()),
            ],
            OrderCondition::Margin { percent, is_more } => vec![
                (6126, op(*is_more)),
                (6245, percent.to_string()),
            ],
            OrderCondition::Execution { symbol, exchange, sec_type } => {
                let exch = if exchange == "SMART" { "*" } else { exchange.as_str() };
                vec![(6246, format!("symbol={};exchange={};securityType={};", symbol, exch, sec_type))]
            }
            OrderCondition::Volume { con_id, exchange, volume, is_more } => vec![
                (6126, op(*is_more)),
                (6123, con_id.to_string()),
                (6124, condition_exchange(exchange)),
                (6263, volume.to_string()),
            ],
            OrderCondition::PercentChange { con_id, exchange, percent, is_more } => vec![
                (6126, op(*is_more)),
                (6123, con_id.to_string()),
                (6124, condition_exchange(exchange)),
                (6245, format_price_ref((percent * crate::types::PRICE_SCALE as f64).round() as crate::types::Price).to_string()),
            ],
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Order;

    fn order(oid: OrderId, filled: u32, status: OrderStatus) -> Order {
        Order {
            order_id: oid, instrument: 0, side: Side::Buy, price: 100,
            qty_fixed: (10) as i64 * crate::types::QTY_SCALE, filled_fixed: filled as i64 * crate::types::QTY_SCALE, status, ord_type: b'2', tif: b'0', stop_price: 0,
        }
    }

    // ibx#211: an outbound cancel synthesizes the PendingCancel phase the
    // server never sends for a normal cancel.
    #[test]
    fn synthesize_pending_cancel_updates_and_notifies() {
        let mut context = Context::new();
        let shared = Arc::new(SharedState::new());
        context.insert_order(order(7, 3, OrderStatus::PartiallyFilled));

        synthesize_pending_cancel(&mut context, &shared, 7);

        assert_eq!(context.order(7).unwrap().status, OrderStatus::PendingCancel);
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, OrderStatus::PendingCancel);
        assert_eq!(updates[0].filled_qty_fixed / crate::types::QTY_SCALE, 3);
        assert_eq!(updates[0].remaining_qty_fixed / crate::types::QTY_SCALE, 7);
    }

    /// Encode one order request through `drain_and_send_orders` over a
    /// loopback socket and return the sent tags in wire order.
    fn wire_tags(req: OrderRequest) -> Vec<(u32, String)> {
        wire_tags_with(|_| {}, req)
    }

    /// Same as `wire_tags`, with a hook to set up the engine state first.
    fn wire_tags_with(setup: impl FnOnce(&mut Context), req: OrderRequest) -> Vec<(u32, String)> {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

        let mut context = Context::new();
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
        let mut conn = Some(Connection::new_raw(client).unwrap());
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);

        let mut buf = vec![0u8; 8192];
        let n = server.read(&mut buf).unwrap();
        buf[..n].split(|&b| b == fix::SOH)
            .filter_map(|f| {
                let s = std::str::from_utf8(f).ok()?;
                let (t, v) = s.split_once('=')?;
                Some((t.parse().ok()?, v.to_string()))
            })
            .collect()
    }

    /// Encode one order request and return each sent frame's tags, for
    /// requests that send several orders.
    fn wire_frames(req: OrderRequest, frames: usize) -> Vec<Vec<(u32, String)>> {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

        let mut context = Context::new();
        context.market.register(265598);
        context.pending_orders.push(req);
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_raw(client).unwrap());
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

    // ibx#311 ibx#329: the bracket children carry the parent's order id
    // with its version, the parent's id as the OCA group, and cancel on
    // fill. The bracket's own ids are versioned like every other order.
    #[test]
    fn bracket_children_link_to_the_parent_like_the_reference() {
        let frames = wire_frames(OrderRequest::SubmitBracket {
            parent_id: 3, tp_id: 4, sl_id: 5, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P,
        }, 3);
        assert_eq!(frames.len(), 3);
        assert_eq!(tag(&frames[0], 11), Some("3.0"));
        for absent in [583, 6107, 6209] {
            assert!(tag(&frames[0], absent).is_none(), "parent sends no field {}", absent);
        }
        for (f, id) in [(&frames[1], "4.0"), (&frames[2], "5.0")] {
            assert_eq!(tag(f, 11), Some(id));
            assert_eq!(tag(f, 6107), Some("3.0"));
            assert_eq!(tag(f, 583), Some("3"));
            assert_eq!(tag(f, 6209), Some("ReduceOnFillNonBlock"));
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
        assert_eq!(tag(&tags, 6260), Some("0.5"));
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
        assert_eq!(tag(&tags, 99), Some("11"));
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6107), Some("100.0"));
        // A child's OCA group is its parent's id (ibx#329).
        assert_eq!(tag(&tags, 583), Some("100"));
        assert_eq!(tag(&tags, 6257), Some("1"));
        assert_eq!(tag(&tags, 6261), Some("3"));
        assert_eq!(tag(&tags, 6258), Some("12"));
        assert_eq!(tag(&tags, 6259), Some("10"));
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

    // An instrument registered without a contract id keeps the symbol-only form.
    #[test]
    fn a_new_order_without_a_contract_id_sends_none() {
        let tags = wire_tags_with(
            |ctx| { ctx.market.register(0); },
            OrderRequest::SubmitLimit { order_id: 6, instrument: 1, side: Side::Buy, qty: 1, price: 100 * P },
        );
        assert!(tag(&tags, 6210).is_some());
        assert!(tag(&tags, 6008).is_none());
    }

    // ── Replace (ibx#247 ibx#324 ibx#334 ibx#349) ──
    //
    // Each case replays one replace the reference sent (ib-agent#192 group
    // A, account id replaced) and checks ibx sends the same fields in the
    // same order with the same values. Left out on purpose: 6205 (only when
    // the original order carried a price cap, meaning unknown; the server
    // accepts replaces without it) and 6531 (bracket group index, which ibx
    // does not send on submit either).

    fn parse_frame(s: &str) -> Vec<(u32, String)> {
        s.split('|')
            .filter_map(|f| {
                let (t, v) = f.split_once('=')?;
                Some((t.parse().ok()?, v.to_string()))
            })
            .filter(|(t, _)| !matches!(t, 6205 | 6531))
            .collect()
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
        .filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 10))
        .collect()
    }

    fn assert_same_replace(ours: &[(u32, String)], reference: &str) {
        let want = parse_frame(reference);
        let ours_tags: Vec<u32> = ours.iter().map(|(t, _)| *t).collect();
        let want_tags: Vec<u32> = want.iter().map(|(t, _)| *t).collect();
        assert_eq!(ours_tags, want_tags, "field order differs\n ours: {:?}\n want: {:?}", ours, want);
        for ((t, a), (_, b)) in ours.iter().zip(want.iter()) {
            let same = match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(x), Ok(y)) => (x - y).abs() < 1e-9,
                _ => a == b,
            };
            assert!(same, "field {}: ours {:?}, reference {:?}", t, a, b);
        }
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
            crate::types::OrderKind::TrailPct { trail_pct: 3100, trail_stop_price: 0 }, b'0', attrs_rth(false));
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
        for (bp, pct) in [(50u32, "0.50"), (100, "1.00"), (125, "1.25"), (200, "2.00")] {
            let plain = wire_tags(OrderRequest::SubmitTrailingStopPct {
                order_id: 7, instrument: 0, side: Side::Sell, qty: 1, trail_pct: bp, trail_stop_price: 0,
            });
            let ext = wire_tags(OrderRequest::SubmitEx {
                order_id: 8, instrument: 0, side: Side::Sell, qty: 1,
                kind: crate::types::OrderKind::TrailPct { trail_pct: bp, trail_stop_price: 0 },
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
        assert_eq!(tag(&ours, 11), Some("100.1000000"), "the preview's own ClOrdID");
        assert_eq!(pos(&ours, 6091) + 1, pos(&ours, 15));

        // A MKT preview is a MKT order (captured in ib-agent#160), not a limit at 0.
        let mkt = wire_tags(what_if_of(crate::api::types::Order { action: "SELL".into(), total_quantity: 1.0,
            order_type: "MKT".into(), ..Default::default() }, 101));
        assert_eq!((tag(&mkt, 40), tag(&mkt, 44), tag(&mkt, 6091)), (Some("1"), None, Some("1")));

        // An algo preview keeps the algo fields (ibx#462: it went out as a
        // real algo order).
        let vwap = wire_tags(what_if_of(crate::api::types::Order { action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 100.0, algo_strategy: "Vwap".into(),
            algo_params: vec![crate::api::types::TagValue { tag: "maxPctVol".into(), value: "0.1".into() }],
            ..Default::default() }, 102));
        assert_eq!((tag(&vwap, 847), tag(&vwap, 18), tag(&vwap, 6091)), (Some("Vwap"), Some("e"), Some("1")));
        let adaptive = wire_tags(what_if_of(crate::api::types::Order { action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 100.0, algo_strategy: "Adaptive".into(), ..Default::default() }, 103));
        assert_eq!((tag(&adaptive, 847), tag(&adaptive, 6091)), (Some("Adaptive"), Some("1")));
    }

    // The preview drops the OCA group and type and keeps the parent link
    // (ORDER-WHATIF.md 4, from the code read).
    #[test]
    fn what_if_drops_the_oca_fields() {
        let child = crate::api::types::Order { action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
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
        let lmt = crate::api::types::Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 101.0, ..Default::default() };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let shared = Arc::new(SharedState::new());
        let mut context = Context::new();
        context.market.register(265598);
        let working = Order::new(105, 0, Side::Buy, 1, 100 * P, b'2', b'0', 0);
        context.insert_order(working);
        context.modify_versions.insert(105, 2);
        context.last_clord.insert(105, "105.2".into());
        context.pending_orders.push(what_if_of(lmt, 105));
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(frames.len(), 1);
        assert_eq!(tag(&frames[0], 35), Some("D"), "a new preview, never a replace");
        assert_eq!(tag(&frames[0], 11), Some("105.1000000"));
        assert!(tag(&frames[0], 41).is_none());
        let order = context.order(105).copied().expect("the working order is kept");
        assert_eq!((order.price, order.status), (100 * P, OrderStatus::PendingSubmit));
        assert_eq!(context.modify_versions.get(&105), Some(&2));
        assert_eq!(context.last_clord.get(&105).map(String::as_str), Some("105.2"));
        assert_eq!(context.what_ifs.len(), 1);
        // A preview of a new id leaves no order behind.
        let new_id = what_if_of(crate::api::types::Order { action: "BUY".into(), total_quantity: 1.0,
            order_type: "LMT".into(), lmt_price: 1.0, ..Default::default() }, 106);
        context.pending_orders.push(new_id);
        let frames = drain_frames(&mut context, &shared, &mut conn, &mut server);
        assert_eq!(tag(&frames[0], 11), Some("106.1000001"));
        assert!(context.order(106).is_none());
        assert!(context.modify_versions.get(&106).is_none());
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
    // Reference (ib-agent#192 B2b/B2c, one price condition, account removed).
    #[test]
    fn condition_block_with_cancel_order_matches_reference() {
        let want = parse_frame("6128=0|6151=1|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=509.62|6223=|6245=|6263=|6246=|6947=");
        assert_eq!(condition_block(true, false), want);
    }

    #[test]
    fn condition_block_with_ignore_rth_matches_reference() {
        let want = parse_frame("6128=1|6151=0|6136=1|6222=1|6137=n|6126=>=|6123=265598|6124=BEST|6127=0|6125=509.62|6223=|6245=|6263=|6246=|6947=");
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

    #[test]
    fn synthesize_pending_cancel_skips_terminal_and_unknown_orders() {
        let mut context = Context::new();
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();

        let mut context = Context::new();
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
        let mut conn = Some(Connection::new_raw(client).unwrap());
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();

        let mut context = Context::new();
        context.market.register(265598);
        setup(&mut context);
        context.pending_orders.push(OrderRequest::Cancel { order_id: 5 });
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_raw(client).unwrap());
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
            "OrderId 5 that needs to be cancelled can not be cancelled, state: PendingCancel.".to_string())]);
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

    #[test]
    fn cancel_of_a_working_order_is_sent() {
        let (sent, errors) = cancel_of(|ctx| ctx.insert_order(order(5, 0, OrderStatus::Submitted)));
        assert!(sent > 0);
        assert!(errors.is_empty());
    }

    // ibx#464: the cancel the reference sends (captured 25/09/2026, account
    // masked): 35=F|11=258354710.2|41=258354710.1|38=1|54=2|1=DU...|6008=265598|
    // 6088=Socket|6944=SEL. ibx sent 11=C{id}|41|60 only.
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

    // ibx#466: the orderRef rides 6010 on the order and on each replace, as
    // the reference (captured 25/09/2026: 6010=pm0925-fill-BUY, and 6010
    // before 6122=c on the replace).
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

    // ibx#466: the reference writes the stop trigger 6117 with the stop
    // price on STP, STP LMT and STP PRT (captured 25/09/2026:
    // 99=235.14 ... 6117=235.14).
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
        // MIT writes its stop price in 6117 too, as its touched trigger
        // among the attributes (ib-agent#199, ibx#263).
        assert_eq!(tag(&mit, 6117), Some("235"));
    }

    // ib-agent#194, captured 25/09/2026: a TRAIL LIMIT with lmtPrice only
    // sends the price in 44 and no 6370:
    // 35=D|44=745.66|99=20.00|...|6117=750.66|...|40=TSL|211=20.00.
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

        // The offset form keeps 6370 and no 44.
        let offset = wire_tags(OrderRequest::SubmitTrailingStopLimit {
            order_id: 22, instrument: 0, side: Side::Sell, qty: 1, lmt_offset: P / 2,
            lmt_price: None, trail_amt: 20 * P, trail_stop_price: 75066 * P / 100,
        });
        assert_eq!(tag(&offset, 6370), Some("0.5"));
        assert_eq!(tag(&offset, 44), None);
    }

    // ib-agent#194: the replace restates 44 and the offset the server
    // reported (35=G|...|44=745.76|99=20.00|...|6370=5.00|...).
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
        assert_eq!(tag(&tags, 6370), Some("5"), "ibx writes prices without trailing zeros");
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
    // price: 35=G|...|44=746.99|...|6370=4.90|... for a sell with
    // 6117=751.89. For a buy it is the limit price minus the stop price.
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
        assert_eq!(tag(&sell, 6370), Some("4.9"));
        let buy = replace(Side::Buy, 79699 * P / 100, 79189 * P / 100);
        assert_eq!(tag(&buy, 6370), Some("5.1"));
    }

    /// Every frame the engine sends for the queued requests, split per frame.
    fn drain_frames(context: &mut Context, shared: &Arc<SharedState>, conn: &mut Option<Connection>,
                    server: &mut std::net::TcpStream) -> Vec<Vec<(u32, String)>> {
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
            if let Some(f) = frames.last_mut() { f.push((t, v.to_string())); }
        }
        frames
    }

    // ibx#465 (ib-agent#199): a request with outside RTH waits for the
    // definition of its exchange, asked once; then outside RTH follows the
    // reference's rule: a STP on a US stock loses it with warning 2109, a
    // LMT keeps it, and a replace drops it without a second warning.
    #[test]
    fn outside_rth_waits_for_the_definition_then_follows_the_rule() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let shared = Arc::new(SharedState::new());
        let mut context = Context::new();
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
        assert_eq!(tag(&frames[0], 11), Some("50.0"));
        assert_eq!(tag(&frames[0], 6433), None, "STP on a US stock: no outside RTH");
        assert_eq!(tag(&frames[1], 11), Some("51.0"));
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let shared = Arc::new(SharedState::new());
        let mut context = Context::new();
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
    // engine's own request; a decrease goes out negative (from the code
    // read, not captured).
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
        let order = crate::api::types::Order {
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

    // Pegged to market (from the code read, not captured on the wire:
    // ORDER-SUBMIT.md 3.5): the limit price when
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
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let mut conn = Some(Connection::new_raw(client).unwrap());
            let shared = Arc::new(SharedState::new());
            let mut context = Context::new();
            context.market.register(265598);
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
        let with_prot = format!("{BEST_LIST},MKTPROT/1,STPPROT/1");
        let (frames, errors) = run("SMART", &with_prot, mkt_prt(97));
        assert_eq!((frames.len(), tag(&frames[0], 40)), (1, Some("U")));
        assert!(errors.is_empty());
    }

    // A definition with no order-type list, or none in time: the type is
    // not checked and the order goes out.
    #[test]
    fn pegged_types_without_a_list_are_sent() {
        let tags = wire_tags(OrderRequest::SubmitPegMkt { order_id: 96, instrument: 0, side: Side::Buy, qty: 1, price: 0, offset: 0 });
        assert_eq!(tag(&tags, 35), Some("D"));
    }

    // ibx#425: customer account 6207 and professional customer 6636.
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let shared = Arc::new(SharedState::new());
        shared.reference.set_short_sale_flags(super_user, omnibus);
        let mut context = Context::new();
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
        vec![(order_id, 321, format!("Error validating request.-'bH' : cause - {}", cause))]
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
        tags.into_iter().filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 60 | 10)).collect()
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
            (OrderRequest::SubmitTrailingStopPct { order_id: id, instrument: 0, side, qty, trail_pct: 150, trail_stop_price: 90 * P },
                K::TrailPct { trail_pct: 150, trail_stop_price: 90 * P }, b'0', Default::default()),
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

    // ibx#466 (captured 25/09/2026, paper, AAPL, account masked): a new
    // order is the reference's frame field for field, the origin, the API
    // order and client ids, the multiplier, the source and the two empty
    // trailing fields included, and no field the reference does not write.
    #[test]
    fn new_order_is_the_captured_frame() {
        const LMT: &str = "35=D|11=7.0|44=337.93|1=DU1|6010=pm0925-fill-BUY|6122=c|6433=1|6121=7|6119=250|38=100|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut context = Context::new();
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
        let mut conn = Some(Connection::new_raw(client).unwrap());
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let mut buf = vec![0u8; 8192];
        let n = server.read(&mut buf).unwrap();
        let mut ours: Vec<(u32, String)> = order_body(buf[..n].split(|&b| b == fix::SOH)
            .filter_map(|f| {
                let (t, v) = std::str::from_utf8(f).ok()?.split_once('=')?;
                Some((t.parse().ok()?, v.to_string()))
            })
            .collect());
        ours.retain(|(t, _)| *t != 35);
        let mut want = parse_frame(LMT);
        want.retain(|(t, _)| *t != 35);
        assert_eq!(ours, want);
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
        let order = ApiOrder {
            action: "BUY".into(), total_quantity: 1.0, order_type: "STP".into(), aux_price: 495.48,
            algo_strategy: "Adaptive".into(), order_ref: "fourleg".into(),
            algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
            ..Default::default()
        };
        let Ok(crate::types::ControlCommand::Order(req)) = crate::client_core::ClientCore::build_order_request(&order, 81, 0) else {
            panic!("not an order");
        };
        assert!(matches!(req, OrderRequest::SubmitEx { kind: crate::types::OrderKind::Stop { .. }, .. }), "{req:?}");
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut context = Context::new();
        context.market.register(265598);
        context.set_symbol(0, "AAPL".to_string());
        context.market.set_min_tick(0, 0.01);
        context.pending_orders.push(req);
        let shared = Arc::new(SharedState::new());
        shared.reference.set_api_client_id(198);
        let mut conn = Some(Connection::new_raw(client).unwrap());
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut HeartbeatState::new(), false, &shared);
        let mut buf = vec![0u8; 8192];
        let n = server.read(&mut buf).unwrap();
        let mut ours: Vec<(u32, String)> = order_body(buf[..n].split(|&b| b == fix::SOH)
            .filter_map(|f| {
                let (t, v) = std::str::from_utf8(f).ok()?.split_once('=')?;
                Some((t.parse().ok()?, v.to_string()))
            })
            .collect());
        ours.retain(|(t, _)| !matches!(t, 35 | 11));
        let mut want = parse_frame(STP);
        want.retain(|(t, _)| !matches!(t, 35 | 11));
        for (t, v) in want.iter_mut() {
            if *t == 1 { *v = "DU1".into(); }
        }
        let (mine, theirs) = common_order(&ours, STP);
        assert_eq!(mine, theirs, "field order");
        let sorted = |mut v: Vec<(u32, String)>| { v.sort(); v };
        assert_eq!(sorted(ours), sorted(want));
    }

    // ibx#263: every algo keeps the order's own type, its fields and its
    // instruction letter, the algo letter after it (`jclient.pe.gI()`).
    #[test]
    fn algo_orders_keep_their_order_type() {
        use crate::types::{AdaptivePriority, OrderAlgo, OrderAttrs, OrderKind as K};
        let adaptive = || OrderAttrs { algo: Some(OrderAlgo::Adaptive(AdaptivePriority::Urgent)), ..Default::default() };
        let ex = |kind: K, attrs: OrderAttrs| OrderRequest::SubmitEx {
            order_id: 5, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'0', attrs };
        let stop_limit = wire_tags(ex(K::StopLimit { price: px(99.5), stop_price: px(100.0) }, adaptive()));
        assert_eq!((tag(&stop_limit, 40), tag(&stop_limit, 44), tag(&stop_limit, 99), tag(&stop_limit, 6117)),
            (Some("4"), Some("99.5"), Some("100"), Some("100")));
        assert_eq!((tag(&stop_limit, 18), tag(&stop_limit, 847), tag(&stop_limit, 5960)), (Some("e"), Some("Adaptive"), Some("Urgent")));
        let market = wire_tags(ex(K::Market, adaptive()));
        assert_eq!((tag(&market, 40), tag(&market, 44), tag(&market, 18)), (Some("1"), None, Some("e")));
        let trail = wire_tags(ex(K::TrailingStop { trail_amt: px(1.0), trail_stop_price: 0 }, adaptive()));
        assert_eq!((tag(&trail, 40), tag(&trail, 211), tag(&trail, 18)), (Some("P"), Some("1"), Some("a e")));
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

    // ibx#263 (captured 23/09/2026, ib-agent#192 A2a and A2b, and
    // 28/09/2026, captures/0928; paper, AAPL, account masked): a new stop
    // order is the reference's frame, the default trigger method 6115=0
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
            (TRAIL_PCT, ex(K::TrailPct { trail_pct: 3000, trail_stop_price: 0 }, b'1',
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
    // touched types only (`jattrib.attribs.TriggerMethod.b`, true for
    // `jibtypes.s.aw()`), the caller's value or the default 0; never on the
    // other types, whatever the caller set. Captured with 6115=0: STP,
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
            K::TrailPct { trail_pct: 150, trail_stop_price: 0 },
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
    // on a new order only; no captured 35=G has it) and restates the
    // instruction field as the new order does (`jclient.pe.gI()` in the
    // replace writer `jclient.pe.d`): the type letter, then G for
    // all-or-none, after the trail field.
    #[test]
    fn replace_restates_all_or_none_and_no_trigger_method() {
        use crate::types::{OrderAttrs, OrderKind as K};
        let aon = OrderAttrs { all_or_none: true, trigger_method: 2, ..Default::default() };
        let cases = [
            (K::TrailingStop { trail_amt: px(1.0), trail_stop_price: 0 }, Some("a G")),
            (K::TrailPct { trail_pct: 150, trail_stop_price: 0 }, Some("a G")),
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
    // unit 6268=0 of an amount TRAIL and a TRAIL LIMIT, the touched trigger
    // 6117 of MIT and LIT (their stop price, among the attributes), and the
    // price cap 44 of a REL, through both encoders. The ids (ClOrdID,
    // account, API order and client ids) are left out; prices compare as
    // numbers (ibx writes 450 and 0.5 where the reference writes 450.00
    // and 0.50).
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
        let as_numbers = |fields: Vec<(u32, String)>| -> Vec<(u32, String)> {
            fields.into_iter().map(|(t, v)| match v.parse::<f64>() {
                Ok(n) if v.contains('.') => (t, n.to_string()),
                _ => (t, v),
            }).collect()
        };
        for (captured, price_mgmt, req) in cases {
            let label = format!("{req:?}");
            let sent = order_body(wire_tags_with(|ctx| session(ctx, price_mgmt), req));
            let (mine, theirs) = common_order(&sent, captured);
            assert_eq!(mine, theirs, "{label}: field order");
            let mut ours = as_numbers(sent);
            ours.retain(not_id);
            let mut want = as_numbers(parse_frame(captured));
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
        // A REL without a price cap has no 44.
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
        .filter(|(t, _)| !matches!(t, 8 | 9 | 34 | 52 | 10))
        .collect()
    }

    // ibx#263 (captured 28/09/2026, ib-agent#199, paper, AAPL, account
    // masked): a replace of MIT and LIT restates the touched trigger 6117
    // among the attributes, a replace of REL its price cap 44; field for
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
            assert_same_replace(&ours, &captured.replace("DUXXXXXXX", "DU1"));
        }
    }

    // ibx#263: a replace of an algo order restates the algo letter in 18
    // and the algo block after the attributes, as the new order: the
    // reference's replace writer `jclient.pe.d` writes them through the
    // same `jclient.pe.j` and `jclient.pe.gI()`. Not captured.
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

    // ibx#466: the API order id is an int; a larger order id is not sent.
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut context = Context::new();
        context.market.register(265598);
        setup(&mut context);
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_raw(client).unwrap());
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

    /// A session that allows price management, with the paper exclusion
    /// list and a stock definition that has the price check key.
    fn price_mgmt_session(context: &mut Context) {
        context.price_mgmt_feature = true;
        context.price_mgmt_exclusions = Some(crate::engine::price_mgmt::parse_exclusions(
            "*/CMDTY;*/CRYPTO;*/FUND;*/IOPT;*/SLB"));
        context.rth_types.insert((265598, "BEST".to_string()), crate::engine::outside_rth::RthTypes {
            sec_type: "STK".into(), types_known: true, price_chk: true, ..Default::default()
        });
    }

    fn priced(value: Option<bool>) -> crate::types::OrderAttrs {
        crate::types::OrderAttrs { use_price_mgmt_algo: value, ..Default::default() }
    }

    // ibx#492, ib-agent captures/200 (28/09/2026): a stock limit order
    // with the value unset or on carries the flag, last of the attributes,
    // on the new order and on the replace; with the value off it
    // has none, and the reference never writes it off.
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
                    let next = if tag(f, 35) == Some("D") { 6121 } else { 38 };
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
            (ex(5, K::TrailPct { trail_pct: 100, trail_stop_price: 0 }), false),
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let mut context = Context::new();
        context.market.register(265598);
        price_mgmt_session(&mut context);
        let types = context.rth_types.remove(&(265598, "BEST".to_string())).unwrap();
        let shared = Arc::new(SharedState::new());
        let mut conn = Some(Connection::new_raw(client).unwrap());
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
            .map(|f| if f.starts_with('c') { "c" } else { f.split("|11=").nth(1).and_then(|r| r.split('|').next()).unwrap_or("?") })
            .collect();
        assert_eq!(sent, vec!["c", "14.0", "10.0", "11.0", "12.0", "13.0"]);
    }
}
