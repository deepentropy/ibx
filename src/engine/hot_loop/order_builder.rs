use std::sync::Arc;
use std::time::Instant;

use crate::bridge::SharedState;
use crate::config::{chrono_free_timestamp, unix_to_ib_datetime, unix_to_ib_utc_dash};
use crate::engine::context::Context;
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{AlgoParams, OrderCondition, OrderRequest, OrderStatus, OrderUpdate, Side};

use super::{HeartbeatState, format_price, format_qty, format_uint};

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
    let conn = match ccp_conn.as_mut() {
        Some(c) => c,
        None => return,
    };
    for mut order_req in orders {
        let oid = order_req.order_id();
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
                    (21, "2"),          // HandlInst = Automated
                    (55, &symbol),      // Symbol
                    (54, side_str),     // Side
                    (38, &qty_str),     // OrderQty
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),   // Price
                    (59, "0"),          // TIF = DAY
                    (60, &now),         // TransactTime
                    (167, &sec_type_str),       // SecurityType = CommonStock
                    (100, &destination),
                    (6210, &destination),     // ExDestination
                    (15, "USD"),        // Currency
                    (204, "0"),         // CustomerOrFirm
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "4"),          // OrdType = Stop Limit
                    (44, &price_str),   // Limit Price
                    (99, &stop_str),    // StopPx
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "1"),          // TIF = GTC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),          // HandlInst = Automated
                    (55, &symbol),      // Symbol
                    (54, side_str),
                    (38, &qty_str),
                    (40, "1"),          // OrdType = Market
                    (59, "0"),          // TIF = DAY
                    (60, &now),         // TransactTime
                    (167, &sec_type_str),       // SecurityType
                    (100, &destination),
                    (6210, &destination),     // ExDestination
                    (15, "USD"),        // Currency
                    (204, "0"),         // CustomerOrFirm
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
                    (21, "2"),          // HandlInst = Automated
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),          // OrdType = Stop
                    (99, &stop_str),    // StopPx
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),          // OrdType = Stop
                    (99, &stop_str),
                    (59, "1"),          // TIF = GTC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "4"),          // OrdType = Stop Limit
                    (44, &price_str),
                    (99, &stop_str),
                    (59, "1"),          // TIF = GTC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "3"),          // TIF = IOC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // OrdType = Limit
                    (44, &price_str),
                    (59, "4"),          // TIF = FOK
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),          // OrdType = Stop (used for trailing too)
                    (99, &trail_str),   // StopPx = trail amount
                    (211, &trail_str), // PegOffset = trail amount
                    (18, "a"),          // ExecInst = TrailingStop
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ];
                // Optional initial stop trigger (tag 6117), only when set
                // (ib-agent#173).
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, lmt_offset, trail_amt, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, lmt_offset, b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price(lmt_offset);
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "TSL"),         // OrdType = Trailing Stop Limit
                    (99, &trail_str),    // StopPx = trail amount
                    (6370, &offset_str), // LimitPriceOffset
                    (211, &trail_str),   // PegOffset = trail amount
                    (59, "0"),           // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ];
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),              // OrdType = Trailing Stop
                    (99, &pct_decimal),     // StopPx = percent as decimal
                    (211, &pct_decimal),    // PegOffset = percent as decimal (mirror of 99)
                    (18, "a"),              // ExecInst = TrailingStop
                    (6268, "100"),          // Trail unit = percent
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "5"),          // OrdType = Market on Close
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "B"),          // OrdType = Limit on Close
                    (44, &price_str),   // Limit price
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "J"),          // OrdType = Market if Touched
                    (99, &stop_str),    // StopPx = trigger price
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "LT"),         // OrdType = Limit If Touched (per ib-agent#138)
                    (44, &price_str),   // Limit price
                    (99, &stop_str),    // StopPx = trigger price
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitBracket { parent_id, tp_id, sl_id, instrument, side, qty, entry_price, take_profit, stop_loss } => {
                let exit_side = match side { Side::Buy => Side::Sell, Side::Sell | Side::ShortSell => Side::Buy };
                let exit_side_str = fix_side(exit_side);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let parent_str = parent_id.to_string();
                let tp_str = tp_id.to_string();
                let sl_str = sl_id.to_string();
                let entry_str = format_price(entry_price);
                let tp_price_str = format_price(take_profit);
                let sl_price_str = format_price(stop_loss);
                let oca_group = format!("OCA_{}", parent_id);

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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),          // Limit
                    (44, &entry_str),
                    (59, "0"),          // DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, exit_side_str),
                    (38, &qty_str),
                    (40, "2"),          // Limit
                    (44, &tp_price_str),
                    (59, "1"),          // GTC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                    (6107, &parent_str),       // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, "ReduceOnFillNonBlock"), // OCA type: gateway default 3 (ibx#215)
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
                    (21, "2"),
                    (55, &symbol),
                    (54, exit_side_str),
                    (38, &qty_str),
                    (40, "3"),          // Stop
                    (99, &sl_price_str),
                    (59, "1"),          // GTC
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                    (6107, &parent_str),       // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, "ReduceOnFillNonBlock"), // OCA type: gateway default 3 (ibx#215)
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "P"),              // OrdType = Pegged (used for Relative too)
                    (211, &offset_str),     // PegOffset
                    (18, "R"),              // ExecInst = Relative
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),              // OrdType = Limit
                    (44, &price_str),
                    (59, "2"),              // TIF = OPG (At the Opening)
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitAdaptive { order_id, instrument, side, qty, price, priority, tif, attrs } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', tif, 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp().to_string();
                // Per ib-agent#136 capture: Adaptive needs 18=e (ExecInst =
                // Adaptive algo wrapper). Without it, gateway rejects with
                // "Invalid value in field # 18".
                let mut fields: Vec<(u32, String)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
                    (fix::TAG_SENDING_TIME, now.clone()),
                    (11, format!("{}.{}", order_id, ver)),
                    (1, account_id.to_string()),
                    (21, "2".to_string()),
                    (55, symbol),
                    (54, fix_side(side).to_string()),
                    (38, format_uint(qty as u64).to_string()),
                    (40, "2".to_string()),              // OrdType = Limit
                    (44, format_price(price).to_string()),
                    (18, "e".to_string()),              // ExecInst = algo
                    (59, tif_str(tif)),
                    (60, now),
                    (167, sec_type_str),
                    (100, destination.clone()),
                    (6210, destination),
                    (15, "USD".to_string()),
                    (204, "0".to_string()),
                ];
                // Parent link, OCA group and the other attributes (ibx#318).
                push_extended_attrs(&mut fields, &attrs, true);
                fields.push((847, "Adaptive".to_string()));      // AlgoStrategy
                fields.push((5957, "1".to_string()));            // AlgoParamCount
                fields.push((5958, "adaptivePriority".to_string())); // AlgoParamTag
                fields.push((5960, priority.as_str().to_string()));  // AlgoParamValue
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                send_new_order(conn, context, instrument, &refs)
            }
            OrderRequest::SubmitAlgo { order_id, instrument, side, qty, price, algo, tif, attrs } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', tif, 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp().to_string();
                // Every algo type rides the same algo instruction as Adaptive:
                // the reference sends 18=e on all six (ib-agent#192 B9, ibx#405).
                let mut fields: Vec<(u32, String)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
                    (fix::TAG_SENDING_TIME, now.clone()),
                    (11, format!("{}.{}", order_id, ver)),
                    (1, account_id.to_string()),
                    (21, "2".to_string()),
                    (55, symbol),
                    (54, fix_side(side).to_string()),
                    (38, format_uint(qty as u64).to_string()),
                    (40, "2".to_string()),              // OrdType = Limit
                    (44, format_price(price).to_string()),
                    (18, "e".to_string()),              // ExecInst = algo
                    (59, tif_str(tif)),
                    (60, now),
                    (167, sec_type_str),
                    (100, destination.clone()),
                    (6210, destination),
                    (15, "USD".to_string()),
                    (204, "0".to_string()),
                ];
                // Parent link, OCA group and the other attributes (ibx#318).
                push_extended_attrs(&mut fields, &attrs, true);
                let (algo_name, param_strs) = build_algo_tags(&algo);
                fields.push((847, algo_name.to_string()));
                // Tag 849 (maxPctVol) for algos that use it
                match &algo {
                    AlgoParams::Vwap { max_pct_vol, .. }
                    | AlgoParams::ArrivalPx { max_pct_vol, .. }
                    | AlgoParams::ClosePx { max_pct_vol, .. } => fields.push((849, format!("{}", max_pct_vol))),
                    _ => {}
                }
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
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                send_new_order(conn, context, instrument, &refs)
            }
            OrderRequest::SubmitPegBench { order_id, instrument, side, qty, price,
                ref_con_id, is_peg_decrease, pegged_change_amount, ref_change_amount } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, crate::types::ORD_PEG_BENCH, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let ref_con_str = ref_con_id.to_string();
                let peg_decrease_str = if is_peg_decrease { "1" } else { "0" };
                let peg_change_str = format_price(pegged_change_amount);
                let ref_change_str = format_price(ref_change_amount);
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "PB"),          // OrdType = Pegged to Benchmark
                    (44, &price_str),    // Limit price
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                    (6941, &ref_con_str),      // referenceContractId
                    (6938, peg_decrease_str),   // isPeggedChangeAmountDecrease
                    (6939, &peg_change_str),    // peggedChangeAmount
                    (6942, &ref_change_str),    // referenceChangeAmount
                ])
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "2"),           // OrdType = Limit
                    (44, &price_str),    // Limit price
                    (59, "8"),           // TIF = Auction
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "K"),           // OrdType = Market to Limit
                    (59, "8"),           // TIF = Auction
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitWhatIf { order_id, instrument, side, qty, price, tif, attrs } => {
                // What-if: insert with ORD_WHAT_IF marker so we can detect the response
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, crate::types::ORD_WHAT_IF, tif, 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp().to_string();
                let mut fields: Vec<(u32, String)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
                    (fix::TAG_SENDING_TIME, now.clone()),
                    (11, format!("{}.{}", order_id, ver)),
                    (1, account_id.to_string()),
                    (21, "2".to_string()),
                    (55, symbol),
                    (54, fix_side(side).to_string()),
                    (38, format_uint(qty as u64).to_string()),
                    (40, "2".to_string()),           // OrdType = Limit
                    (44, format_price(price).to_string()),
                    (59, tif_str(tif)),
                    (60, now),
                    (167, sec_type_str),
                    (100, destination.clone()),
                    (6210, destination),
                    (15, "USD".to_string()),
                    (204, "0".to_string()),
                    (6091, "1".to_string()),         // What-If flag
                ];
                // The preview is for the order as it would be placed: its
                // time-in-force and attributes go too (ibx#318).
                push_extended_attrs(&mut fields, &attrs, false);
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                send_new_order(conn, context, instrument, &refs)
            }
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),      // Decimal qty (e.g., "0.5")
                    (40, "2"),           // OrdType = Limit
                    (44, &price_str),
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "3"),              // OrdType = Stop
                    (99, &stop_str),        // StopPx
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "K"),          // OrdType = Market to Limit
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "U"),          // OrdType = Market with Protection
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SP"),         // OrdType = Stop with Protection
                    (99, &stop_str),    // StopPx
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
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
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "MIDPX"),      // OrdType = Mid-Price
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                ];
                let cap_str;
                if price_cap > 0 {
                    cap_str = format_price(price_cap);
                    fields.push((44, &cap_str)); // Price cap
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MKT, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SMKT"),       // OrdType = Snap to Market
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitSnapMid { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MID, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SMID"),       // OrdType = Snap to Midpoint
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitSnapPri { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_PRI, b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                send_new_order(conn, context, instrument, &[
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "SREL"),       // OrdType = Snap to Primary
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_PEG_MKT, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "E"),          // OrdType = Pegged (no mid-offset tags = PEGMKT)
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                ];
                let offset_str;
                if offset > 0 {
                    offset_str = format_price(offset);
                    fields.push((211, &offset_str)); // PegOffsetValue
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::SubmitPegMid { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_PEG_MID, b'0', offset,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, _destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),
                    (1, account_id),
                    (21, "2"),
                    (55, &symbol),
                    (54, side_str),
                    (38, &qty_str),
                    (40, "E"),          // OrdType = Pegged (tags 8403/8404 = PEGMID)
                    (59, "0"),
                    (60, &now),
                    (167, &sec_type_str),
                    (100, "ISLAND"),    // Requires directed exchange
                    (6210, "ISLAND"),
                    (15, "USD"),
                    (204, "0"),
                    (8403, "0.0"),      // midOffsetAtWhole — differentiates PEGMID from PEGMKT
                    (8404, "0.0"),      // midOffsetAtHalf
                ];
                let offset_str;
                if offset > 0 {
                    offset_str = format_price(offset);
                    fields.push((211, &offset_str)); // PegOffsetValue
                }
                send_new_order(conn, context, instrument, &fields)
            }
            OrderRequest::Cancel { order_id } => {
                // OrigClOrdID must match exactly what the server has on record.
                // Prefer the string we last observed on the wire (see ibx#179 —
                // legacy orders recorded without a `.{ver}` suffix won't match
                // a computed `{id}.0`). Fall back to the versioned scheme when
                // we have no observation yet (fresh-order place→immediate-cancel
                // before the ack round-trip).
                let orig_clord = context.last_clord.get(&order_id).cloned()
                    .unwrap_or_else(|| {
                        let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                        format!("{}.{}", order_id, ver)
                    });
                let clord_str = format!("C{}", order_id);
                let now = chrono_free_timestamp();
                let result = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),   // ClOrdID (cancel)
                    (41, &orig_clord),  // OrigClOrdID (latest version)
                    (60, &now),         // TransactTime
                ]);
                if result.is_ok() {
                    synthesize_pending_cancel(context, shared, order_id);
                }
                result
            }
            OrderRequest::CancelAll { instrument } => {
                let open_ids: Vec<u64> = context.open_orders_for(instrument)
                    .iter()
                    .map(|o| o.order_id)
                    .collect();
                let mut last_result = Ok(());
                for oid in open_ids {
                    let orig_clord = context.last_clord.get(&oid).cloned()
                        .unwrap_or_else(|| {
                            let ver = *context.modify_versions.get(&oid).unwrap_or(&0);
                            format!("{}.{}", oid, ver)
                        });
                    let clord_str = format!("C{}", oid);
                    let now = chrono_free_timestamp();
                    last_result = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (41, &orig_clord),
                        (60, &now),
                    ]);
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
                let fields = modify_fields(
                    &clord_str, &orig_clord, account_id, qty, side_str, &symbol,
                    &sec_type_str, &con_id_str, kind, tif, &attrs,
                );
                let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
                conn.send_fix(&refs)
            }
        };
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

/// Send a new order with the contract id right after the secondary routing
/// field, where the reference puts it on every new order (ib-agent#192 B4,
/// ibx#328). An instrument registered without a contract id keeps the
/// symbol-only form.
fn send_new_order(
    conn: &mut Connection,
    context: &Context,
    instrument: crate::types::InstrumentId,
    fields: &[(u32, &str)],
) -> std::io::Result<()> {
    let con_id = context.market.con_id(instrument).unwrap_or(0);
    if con_id <= 0 {
        return conn.send_fix(fields);
    }
    let con_id_str = con_id.to_string();
    let mut out: Vec<(u32, &str)> = Vec::with_capacity(fields.len() + 1);
    for &(tag, value) in fields {
        out.push((tag, value));
        if tag == 6210 {
            out.push((6008, &con_id_str));
        }
    }
    conn.send_fix(&out)
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
) -> Vec<(u32, String)> {
    use crate::types::OrderKind as K;
    let p = |v: crate::types::Price| format_price(v).to_string();

    // Prices go before the account; type-specific fields after the order
    // type. A trail value rides both the stop field and the trail field.
    let mut before_account: Vec<(u32, String)> = Vec::new();
    let mut stop_trigger: Option<String> = None; // restated for STP / STP LMT
    let mut trail_offset: Option<String> = None; // TRAIL LIMIT limit offset
    let mut trail_unit: Option<&str> = None;     // 0 = amount, 100 = percent
    let mut after_type: Vec<(u32, String)> = Vec::new();
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
            after_type.push((18, "a".to_string()));
            "P"
        }
        K::TrailPct { trail_pct, .. } => {
            let pct = format!("{:.2}", trail_pct as f64 / 100.0);
            before_account.push((99, pct.clone()));
            trail_unit = Some("100");
            after_type.push((211, pct));
            after_type.push((18, "a".to_string()));
            "P"
        }
        K::TrailingStopLimit { lmt_offset, trail_amt, .. } => {
            before_account.push((99, p(trail_amt)));
            trail_offset = Some(p(lmt_offset));
            trail_unit = Some("0");
            after_type.push((211, p(trail_amt)));
            "TSL"
        }
        K::Moc => "5",
        K::Loc { price } => { before_account.push((44, p(price))); "B" }
        K::Mit { stop_price } => { before_account.push((99, p(stop_price))); "J" }
        K::Lit { price, stop_price } => {
            before_account.push((44, p(price)));
            before_account.push((99, p(stop_price)));
            "LT"
        }
        K::Mtl => "K",
        K::MktPrt => "U",
        K::StpPrt { stop_price } => { before_account.push((99, p(stop_price))); "SP" }
        K::MidPrice { price_cap } => {
            if price_cap > 0 { before_account.push((44, p(price_cap))); }
            "MIDPX"
        }
        K::SnapMkt => "SMKT",
        K::SnapMid => "SMID",
        K::SnapPri => "SREL",
        K::PegMkt { offset } => {
            if offset > 0 { after_type.push((211, p(offset))); }
            "E"
        }
        K::PegMid { offset } => {
            after_type.push((8403, "0.0".to_string()));
            after_type.push((8404, "0.0".to_string()));
            if offset > 0 { after_type.push((211, p(offset))); }
            "E"
        }
        K::Rel { offset } => {
            after_type.push((211, p(offset)));
            after_type.push((18, "R".to_string()));
            "P"
        }
        K::AdjustableStop { stop_price, .. } => {
            before_account.push((99, p(stop_price)));
            stop_trigger = Some(p(stop_price));
            "3"
        }
    };

    let tif_byte = [tif];
    let tif_str = std::str::from_utf8(&tif_byte).unwrap_or("0").to_string();
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
    f.push((6122, "c".to_string()));
    // Outside-RTH only when the order has it: a replace without it leaves
    // the order regular-hours only (ibx#247).
    if attrs.outside_rth { f.push((6433, "1".to_string())); }
    if let Some(u) = trail_unit { f.push((6268, u.to_string())); }
    f.push((38, format_uint(qty as u64).to_string()));
    f.push((54, side.to_string()));
    f.push((40, ord_type.to_string()));
    f.extend(after_type);
    f.push((55, symbol.to_string()));
    f.push((167, sec_type.to_string()));
    f.push((6035, symbol.to_string()));
    f.push((59, tif_str));
    f.push((6008, con_id.to_string()));
    f.push((6088, "Socket".to_string()));
    f.push((6211, String::new()));
    f.push((6238, String::new()));
    f
}

/// The time-in-force byte as its wire string.
fn tif_str(tif: u8) -> String {
    let b = [tif];
    std::str::from_utf8(&b).unwrap_or("0").to_string()
}

/// The extended-attribute block (display size, outside-RTH, hidden, good-after,
/// good-till, OCA group, parent link, conditions, ...), shared by every order
/// path that carries attributes so the emission cannot drift between order
/// types (ibx#224, ibx#318). `has_base_exec_inst` is true when the order type
/// already rides the instruction field, which then cannot also carry
/// all-or-none.
fn push_extended_attrs(
    fields: &mut Vec<(u32, String)>,
    attrs: &crate::types::OrderAttrs,
    has_base_exec_inst: bool,
) {
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
    if attrs.good_after > 0 {
        fields.push((168, unix_to_ib_datetime(attrs.good_after)));
    }
    // GTD expiry: date-only -> tag 432; time-precise -> tag 126 (UTC).
    // Mutually exclusive — never both (gateway rejects both together).
    if attrs.good_till_date_ymd > 0 {
        fields.push((432, format!("{:08}", attrs.good_till_date_ymd)));
    } else if attrs.good_till > 0 {
        fields.push((126, unix_to_ib_utc_dash(attrs.good_till)));
    }
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
    if attrs.parent_id > 0 {
        // Match parent ClOrdID format: "{order_id}.{ver}" — assume ver=0
        // for initial submission.
        fields.push((6107, format!("{}.0", attrs.parent_id)));
    }
    if attrs.discretionary_amt > 0 {
        fields.push((9813, format_price(attrs.discretionary_amt).to_string()));
    }
    if attrs.sweep_to_fill {
        fields.push((6102, "1".to_string()));
    }
    if attrs.all_or_none && !has_base_exec_inst {
        fields.push((18, "G".to_string()));
    }
    if attrs.trigger_method > 0 {
        fields.push((6115, attrs.trigger_method.to_string()));
    }
    if attrs.cash_qty > 0 {
        fields.push((5920, format_price(attrs.cash_qty).to_string()));
    }
    // Condition tags (6136+ framework). The reference always sends both
    // flags, 0 or 1, before the count: ignore-RTH rides 6128 and cancel-order
    // 6151. ibx had them the other way round and sent them only when set
    // (ib-agent#192 B2, ibx#327).
    if !attrs.conditions.is_empty() {
        let cond_strs = build_condition_strings(&attrs.conditions);
        let flag = |on: bool| if on { "1" } else { "0" }.to_string();
        fields.push((6128, flag(attrs.conditions_ignore_rth)));
        fields.push((6151, flag(attrs.conditions_cancel_order)));
        fields.push((6136, cond_strs[0].clone())); // first element is count
        // Per-condition tags start at index 1, 11 strings per condition
        for i in 0..attrs.conditions.len() {
            let base = 1 + i * 11;
            fields.push((6222, cond_strs[base].clone()));      // condType
            fields.push((6137, cond_strs[base + 1].clone()));  // conjunction
            fields.push((6126, cond_strs[base + 2].clone()));  // operator
            fields.push((6123, cond_strs[base + 3].clone()));  // conId
            fields.push((6124, cond_strs[base + 4].clone()));  // exchange
            fields.push((6127, cond_strs[base + 5].clone()));  // triggerMethod
            fields.push((6125, cond_strs[base + 6].clone()));  // price
            fields.push((6223, cond_strs[base + 7].clone()));  // time
            fields.push((6245, cond_strs[base + 8].clone()));  // percent
            fields.push((6263, cond_strs[base + 9].clone()));  // volume
            fields.push((6246, cond_strs[base + 10].clone())); // execution
            fields.push((6947, String::new()));                // empty, as the reference sends it
        }
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
        K::TrailingStopLimit { lmt_offset, .. } => (b'P', lmt_offset, 0),
        K::TrailPct { .. } => (b'P', 0, 0),
        K::Moc => (b'5', 0, 0),
        K::Loc { price } => (b'B', price, 0),
        K::Mit { stop_price } => (b'J', stop_price, stop_price),
        K::Lit { price, stop_price } => (b'K', price, stop_price),
        K::Mtl => (b'K', 0, 0),
        K::MktPrt => (b'U', 0, 0),
        K::StpPrt { stop_price } => (crate::types::ORD_STP_PRT, 0, stop_price),
        K::MidPrice { price_cap } => (crate::types::ORD_MIDPX, price_cap, 0),
        K::SnapMkt => (crate::types::ORD_SNAP_MKT, 0, 0),
        K::SnapMid => (crate::types::ORD_SNAP_MID, 0, 0),
        K::SnapPri => (crate::types::ORD_SNAP_PRI, 0, 0),
        K::PegMkt { offset } => (crate::types::ORD_PEG_MKT, 0, offset),
        K::PegMid { offset } => (crate::types::ORD_PEG_MID, 0, offset),
        K::Rel { offset } => (b'R', 0, offset),
        K::AdjustableStop { stop_price, .. } => (b'3', 0, stop_price),
    };
    context.insert_order(crate::types::Order::new(
        order_id, instrument, side, qty, track_price, ord_type_byte, tif, track_stop,
    ));

    let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
    let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
    let now = chrono_free_timestamp().to_string();
    let tif_byte = [tif];
    let tif_str = std::str::from_utf8(&tif_byte).unwrap_or("0");

    let mut fields: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_NEW_ORDER.to_string()),
        (fix::TAG_SENDING_TIME, now.clone()),
        (11, format!("{}.{}", order_id, ver)),
        (1, account_id.to_string()),
        (21, "2".to_string()),
        (55, symbol),
        (54, fix_side(side).to_string()),
        (38, format_uint(qty as u64).to_string()),
    ];

    // Order type (40) plus its price tags and type-specific companions —
    // identical values to the corresponding plain variants. Kinds that put
    // an instruction in tag 18 (TrailingStop/TrailPct = a, Rel = R) cannot
    // also carry all_or_none (18=G); validate_order rejects that
    // combination up front, and the emission below skips 18=G as a second
    // line of defense.
    let mut has_base_exec_inst = false;
    match kind {
        K::Market => fields.push((40, "1".to_string())),
        K::Limit { price } => {
            fields.push((40, "2".to_string()));
            fields.push((44, format_price(price).to_string()));
        }
        K::Stop { stop_price } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
        K::StopLimit { price, stop_price } => {
            fields.push((40, "4".to_string()));
            fields.push((44, format_price(price).to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
        K::TrailingStop { trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: amount-based trailing stop carries
            // the trail amount in both 99 and 211 and requires 18=a.
            let t = format_price(trail_amt).to_string();
            fields.push((40, "P".to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            fields.push((18, "a".to_string()));
            // Optional initial stop trigger (tag 6117), only when set (ib-agent#173).
            if trail_stop_price > 0 { fields.push((6117, format_price(trail_stop_price).to_string())); }
            has_base_exec_inst = true;
        }
        K::TrailingStopLimit { lmt_offset, trail_amt, trail_stop_price } => {
            // Per ib-agent#136 capture: TRAIL LIMIT uses OrdType=TSL, no
            // tag 44, no tag 18; trail amount in both 99 and 211; 6370 is
            // the limit-vs-trail offset.
            let t = format_price(trail_amt).to_string();
            fields.push((40, "TSL".to_string()));
            fields.push((99, t.clone()));
            fields.push((6370, format_price(lmt_offset).to_string()));
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
            fields.push((18, "a".to_string()));
            fields.push((6268, "100".to_string()));
            if trail_stop_price > 0 { fields.push((6117, format_price(trail_stop_price).to_string())); }
            has_base_exec_inst = true;
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
        }
        K::MidPrice { price_cap } => {
            fields.push((40, "MIDPX".to_string()));
            if price_cap > 0 {
                fields.push((44, format_price(price_cap).to_string()));
            }
        }
        K::SnapMkt => fields.push((40, "SMKT".to_string())),
        K::SnapMid => fields.push((40, "SMID".to_string())),
        K::SnapPri => fields.push((40, "SREL".to_string())),
        K::PegMkt { offset } => {
            fields.push((40, "E".to_string()));
            if offset > 0 {
                fields.push((211, format_price(offset).to_string()));
            }
        }
        K::PegMid { offset } => {
            fields.push((40, "E".to_string()));
            fields.push((8403, "0.0".to_string())); // midOffsetAtWhole — differentiates PEGMID
            fields.push((8404, "0.0".to_string())); // midOffsetAtHalf
            if offset > 0 {
                fields.push((211, format_price(offset).to_string()));
            }
        }
        K::Rel { offset } => {
            // Per ib-agent#138 capture: Relative shares OrdType=P and is
            // disambiguated by 18=R; peg offset on 211, no tag 44.
            fields.push((40, "P".to_string()));
            fields.push((211, format_price(offset).to_string()));
            fields.push((18, "R".to_string()));
            has_base_exec_inst = true;
        }
        // The adjustable tags themselves follow the common block below.
        K::AdjustableStop { stop_price, .. } => {
            fields.push((40, "3".to_string()));
            fields.push((99, format_price(stop_price).to_string()));
        }
    }

    fields.push((59, tif_str.to_string()));
    fields.push((60, now));
    fields.push((167, sec_type_str.clone()));
    // MIDPX / SNAP* / PEG* require a directed exchange; everything else
    // routes per the instrument's registered routing (ibx#217).
    let destination = match kind {
        K::MidPrice { .. } | K::SnapMkt | K::SnapMid | K::SnapPri
        | K::PegMkt { .. } | K::PegMid { .. } => "ISLAND".to_string(),
        _ => destination,
    };
    fields.push((100, destination.clone()));
    // Secondary routing field — the reference encoder always writes it
    // alongside the destination (ib-agent#165).
    fields.push((6210, destination));
    fields.push((15, "USD".to_string()));
    fields.push((204, "0".to_string()));

    // Adjustable-stop tags in the same place as on the plain path (ibx#240).
    if let K::AdjustableStop { trigger_price, adjusted_order_type, adjusted_stop_price,
        adjusted_stop_limit_price, adjusted_trailing_amount, adjustable_trailing_unit, .. } = kind
    {
        fields.extend(adjustable_stop_tags(trigger_price, adjusted_order_type,
            adjusted_stop_price, adjusted_stop_limit_price,
            adjusted_trailing_amount, adjustable_trailing_unit));
    }

    // Extended attributes — same tag order as the historical SubmitLimitEx
    // block.
    push_extended_attrs(&mut fields, attrs, has_base_exec_inst);

    let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
    send_new_order(conn, context, instrument, &refs)
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

fn build_condition_strings(conditions: &[OrderCondition]) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + conditions.len() * 11);
    out.push(conditions.len().to_string());
    for (i, cond) in conditions.iter().enumerate() {
        let is_last = i == conditions.len() - 1;
        let conj = if is_last { "n" } else { "a" };
        let op = |is_more: bool| if is_more { ">=" } else { "<=" };
        match cond {
            OrderCondition::Price { con_id, exchange, price, is_more, trigger_method } => {
                out.push("1".into());                              // condType
                out.push(conj.into());                             // conjunction
                out.push(op(*is_more).into());                     // operator
                out.push(con_id.to_string());                      // conId
                out.push(condition_exchange(exchange));            // exchange
                out.push(trigger_method.to_string());              // triggerMethod
                out.push(format_price(*price).to_string());         // price
                out.push(String::new());                           // time (unused)
                out.push(String::new());                           // percent (unused)
                out.push(String::new());                           // volume (unused)
                out.push(String::new());                           // execution (unused)
            }
            OrderCondition::Time { time, is_more } => {
                out.push("3".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(String::new());                           // conId (unused)
                out.push(String::new());                           // exchange (unused)
                out.push(String::new());                           // triggerMethod (unused)
                out.push(String::new());                           // price (unused)
                out.push(time.clone());                            // time
                out.push(String::new());                           // percent (unused)
                out.push(String::new());                           // volume (unused)
                out.push(String::new());                           // execution (unused)
            }
            OrderCondition::Margin { percent, is_more } => {
                out.push("4".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(percent.to_string());                     // percent
                out.push(String::new());
                out.push(String::new());
            }
            OrderCondition::Execution { symbol, exchange, sec_type } => {
                out.push("5".into());
                out.push(conj.into());
                out.push(String::new());                           // operator (unused)
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                let exch = if exchange == "SMART" { "*" } else { exchange.as_str() };
                out.push(format!("symbol={};exchange={};securityType={};", symbol, exch, sec_type));
            }
            OrderCondition::Volume { con_id, exchange, volume, is_more } => {
                out.push("6".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(con_id.to_string());
                out.push(exchange.clone());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(volume.to_string());                      // volume
                out.push(String::new());
            }
            OrderCondition::PercentChange { con_id, exchange, percent, is_more } => {
                out.push("7".into());
                out.push(conj.into());
                out.push(op(*is_more).into());
                out.push(con_id.to_string());
                out.push(exchange.clone());
                out.push(String::new());
                out.push(String::new());
                out.push(String::new());
                out.push(format!("{}", percent));                   // percent
                out.push(String::new());
                out.push(String::new());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Order;

    fn order(oid: u64, filled: u32, status: OrderStatus) -> Order {
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

    fn tag<'a>(tags: &'a [(u32, String)], t: u32) -> Option<&'a str> {
        tags.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
    }

    fn pos(tags: &[(u32, String)], t: u32) -> usize {
        tags.iter().position(|(k, _)| *k == t).unwrap_or_else(|| panic!("tag {} missing", t))
    }

    const P: i64 = crate::types::PRICE_SCALE;

    // The plain adjustable stop must keep its captured shape after the tags
    // moved into a shared helper (ibx#240 refactor).
    #[test]
    fn plain_adjustable_stop_keeps_its_captured_shape() {
        let tags = wire_tags(OrderRequest::SubmitAdjustableStop {
            order_id: 5, instrument: 0, side: Side::Sell, qty: 1,
            stop_price: 11 * P, trigger_price: 12 * P,
            adjusted_order_type: crate::types::AdjustedOrderType::Trail,
            adjusted_stop_price: 10 * P, adjusted_stop_limit_price: 0,
            adjusted_trailing_amount: P / 2, adjustable_trailing_unit: 0,
        });
        let tail: Vec<u32> = tags[pos(&tags, 204) + 1..].iter().map(|(t, _)| *t)
            .filter(|t| *t != 10).collect(); // drop the checksum
        assert_eq!(tail, vec![6257, 6261, 6258, 6259, 6260, 6269]);
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
        assert_eq!(tag(&tags, 583), Some("BR1"));
        assert_eq!(tag(&tags, 6257), Some("1"));
        assert_eq!(tag(&tags, 6261), Some("3"));
        assert_eq!(tag(&tags, 6258), Some("12"));
        assert_eq!(tag(&tags, 6259), Some("10"));
        // Same placement as the plain path: right after 204, before the attrs.
        assert_eq!(pos(&tags, 6257), pos(&tags, 204) + 1);
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
    fn replace_fields(order_id: u64, side: Side, qty: u32, kind: crate::types::OrderKind,
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
            crate::types::OrderKind::TrailingStopLimit { lmt_offset: px(0.50), trail_amt: px(105.32), trail_stop_price: px(237.82) },
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
        assert_eq!(tag(&tags, 583), Some("BR1"));
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
        assert_eq!(tag(&tags, 583), Some("BR1"));
        assert_eq!(tag(&tags, 847), Some("Adaptive"));
    }

    #[test]
    fn what_if_carries_its_time_in_force() {
        let tags = wire_tags(OrderRequest::SubmitWhatIf {
            order_id: 11, instrument: 0, side: Side::Buy, qty: 1, price: 237 * crate::types::PRICE_SCALE,
            tif: b'1', attrs: crate::types::OrderAttrs::default(),
        });
        assert_eq!(tag(&tags, 59), Some("1"));
        assert_eq!(tag(&tags, 6091), Some("1"));
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
    fn modify_of(existing: Option<Order>) -> (usize, Vec<(u64, i64, String)>) {
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
    fn cancel_of(setup: impl FnOnce(&mut Context)) -> (usize, Vec<(u64, i64, String)>) {
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
}
