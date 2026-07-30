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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&fields)
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&fields)
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
                conn.send_fix(&fields)
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&fields)
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
                conn.send_fix(&fields)
            }
            OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct, trail_stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let pct_str = trail_pct.to_string(); // basis points: 100 = 1%
                // Per ib-agent#156 capture: percent-trail mirrors 99/211 as the
                // percent in decimal form (1.00 for 1%), alongside 6268 in basis
                // points and 18=a (ExecInst=TrailingStop). Without 99/211/18 the
                // gateway rejects with "Invalid value in field # 18".
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
                    (6268, &pct_str),       // TrailingPercent (basis points)
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, &sec_type_str),
                    (100, &destination),
                    (6210, &destination),
                    (15, "USD"),
                    (204, "0"),
                ];
                if trail_stop_price > 0 { fields.push((6117, &trail_stop_str)); }
                conn.send_fix(&fields)
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                let _ = conn.send_fix(&[
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
                let _ = conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
            OrderRequest::SubmitLimitFractional { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, 0, price, b'2', b'0', 0,
                ));
                let ver = *context.modify_versions.get(&order_id).unwrap_or(&0);
                let clord_str = format!("{}.{}", order_id, ver);
                let side_str = fix_side(side);
                let qty_str = format_qty(qty);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
                let (sec_type_str, destination) = context.market.order_routing(instrument);
                let now = chrono_free_timestamp();
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&fields)
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&[
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
                conn.send_fix(&fields)
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
                conn.send_fix(&fields)
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
            OrderRequest::Modify { new_order_id, order_id, price, qty } => {
                let orig = context.order(order_id).copied();
                // Modify carries no instrument; resolve it from the tracked
                // order to snap the new price to the tick grid (ibx#216).
                let price = orig.map_or(price, |o| crate::types::snap_to_tick(
                    price, context.market.min_tick_scaled(o.instrument)));
                if let Some(orig) = orig {
                    context.insert_order(crate::types::Order::new(
                        new_order_id, orig.instrument, orig.side, qty, price,
                        orig.ord_type, orig.tif, orig.stop_price,
                    ));
                }
                // Versioned ClOrdID chaining: orderId.0 → .1 → .2
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

                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let now = chrono_free_timestamp();
                let side_str = orig.map(|o| fix_side(o.side)).unwrap_or("1");
                let symbol = orig.map(|o| context.market.symbol(o.instrument).to_string())
                    .unwrap_or_default();
                let (sec_type_str, _destination) = orig
                    .map(|o| context.market.order_routing(o.instrument))
                    .unwrap_or_else(|| ("STK".to_string(), "SMART".to_string()));
                let ord_type_str = crate::types::ord_type_fix_str(orig.map(|o| o.ord_type).unwrap_or(b'2')).to_string();
                let tif_str = std::str::from_utf8(&[orig.map(|o| o.tif).unwrap_or(b'0')]).unwrap_or("0").to_string();
                let con_id_str = orig.and_then(|o| context.market.con_id(o.instrument))
                    .map(|c| c.to_string()).unwrap_or_default();

                // Lean modify message — omit identity tags (6121, 6119, 231, 100, 15, 204)
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),    // ClOrdID (versioned)
                    (41, &orig_clord),   // OrigClOrdID (previous version)
                    (44, &price_str),    // Price
                    (1, account_id),     // Account
                    (6122, "c"),         // Client version
                    (6433, "1"),         // OutsideRTH (preserve from original)
                    (38, &qty_str),      // OrderQty
                    (54, side_str),      // Side
                    (40, &ord_type_str), // OrdType
                    (55, &symbol),       // Symbol
                    (167, &sec_type_str),        // SecurityType
                    (6035, &symbol),     // LocalSymbol echo
                    (59, &tif_str),      // TIF
                    (6008, &con_id_str), // ConId
                    (6088, "Socket"),    // Connection type
                    (6211, ""),          // Empty (matches reference)
                    (6238, ""),          // Empty (matches reference)
                ];
                // Include stop price for order types that need it
                let stop_str;
                if let Some(o) = orig {
                    if o.stop_price != 0 {
                        stop_str = format_price(o.stop_price);
                        fields.push((99, &stop_str));
                    }
                }
                conn.send_fix(&fields)
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
                        order_id: oid,
                        instrument: 0,
                        status: OrderStatus::Rejected,
                        filled_qty: 0,
                        remaining_qty: 0,
                        perm_id: 0,
                        parent_id: 0,
                        timestamp_ns: 0,
                    });
                }
            }
        }
    }
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
            order_id,
            instrument: order.instrument,
            status: OrderStatus::PendingCancel,
            filled_qty: order.filled as i64,
            remaining_qty: order.qty as i64 - order.filled as i64,
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
        K::Adaptive { price, .. } | K::Algo { price, .. } => (b'2', price, 0),
        // Tracked under the what-if marker so the response is recognised as a
        // preview; it never becomes a live order.
        K::WhatIf { price } => (crate::types::ORD_WHAT_IF, price, 0),
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
        K::AdjustableStop { stop_price, .. } => {
            // Base order type only. The 6257+ adjustable tags are appended after
            // the attribute block below, where the dedicated encoder this path
            // replaced put them.
            fields.push((40, "3".to_string()));                       // OrdType = Stop
            fields.push((99, format_price(stop_price).to_string()));  // StopPx
        }
        K::TrailingStop { trail_amt, .. } => {
            // Per ib-agent#136 capture: amount-based trailing stop carries
            // the trail amount in both 99 and 211 and requires 18=a.
            let t = format_price(trail_amt).to_string();
            fields.push((40, "P".to_string()));
            fields.push((99, t.clone()));
            fields.push((211, t));
            fields.push((18, "a".to_string()));
            has_base_exec_inst = true;
        }
        K::TrailingStopLimit { lmt_offset, trail_amt, .. } => {
            // Per ib-agent#136 capture: TRAIL LIMIT uses OrdType=TSL, no
            // tag 44, no tag 18; trail amount in both 99 and 211; 6370 is
            // the limit-vs-trail offset.
            let t = format_price(trail_amt).to_string();
            fields.push((40, "TSL".to_string()));
            fields.push((99, t.clone()));
            fields.push((6370, format_price(lmt_offset).to_string()));
            fields.push((211, t));
        }
        K::TrailPct { trail_pct, .. } => {
            // Per ib-agent#156 capture: percent-trail mirrors 99/211 as the
            // percent in decimal form (1.00 for 1%), alongside 6268 in
            // basis points and 18=a.
            let pct_decimal = format!("{:.2}", trail_pct as f64 / 100.0);
            fields.push((40, "P".to_string()));
            fields.push((99, pct_decimal.clone()));
            fields.push((211, pct_decimal));
            fields.push((18, "a".to_string()));
            fields.push((6268, trail_pct.to_string()));
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
        K::MidPrice { .. } => fields.push((40, "MIDPX".to_string())),
        K::SnapMkt => fields.push((40, "SMKT".to_string())),
        K::SnapMid => fields.push((40, "SMID".to_string())),
        K::SnapPri => fields.push((40, "SREL".to_string())),
        K::PegMkt { .. } => fields.push((40, "E".to_string())),
        K::PegMid { .. } => fields.push((40, "E".to_string())),
        K::Rel { offset } => {
            // Per ib-agent#138 capture: Relative shares OrdType=P and is
            // disambiguated by 18=R; peg offset on 211, no tag 44.
            fields.push((40, "P".to_string()));
            fields.push((211, format_price(offset).to_string()));
            fields.push((18, "R".to_string()));
            has_base_exec_inst = true;
        }
        K::Adaptive { price, .. } => {
            // Per ib-agent#136 capture: Adaptive needs 18=e (ExecInst = adaptive
            // algo wrapper). Without it the gateway rejects with "Invalid value
            // in field # 18". The strategy and its one parameter are appended
            // after the attribute block, where the encoder this replaced put them.
            fields.push((40, "2".to_string()));
            fields.push((44, format_price(price).to_string()));
            fields.push((18, "e".to_string()));
            has_base_exec_inst = true;
        }
        K::Algo { price, .. } => {
            fields.push((40, "2".to_string()));
            fields.push((44, format_price(price).to_string()));
        }
        K::WhatIf { price } => {
            fields.push((40, "2".to_string()));
            fields.push((44, format_price(price).to_string()));
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

    // Extended attributes — same tag order as the historical SubmitLimitEx
    // block.
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
    // Condition tags (6136+ framework)
    if !attrs.conditions.is_empty() {
        let cond_strs = build_condition_strings(&attrs.conditions);
        fields.push((6136, cond_strs[0].clone())); // first element is count
        if attrs.conditions_cancel_order {
            fields.push((6128, "1".to_string()));
        }
        if attrs.conditions_ignore_rth {
            fields.push((6151, "1".to_string()));
        }
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
        }
    }

    // Adjustable-stop tags last, keeping the position they held in the encoder
    // this path replaced: after 204 and the attribute block, not in among the
    // order-type tags. Values and conditions are unchanged; only the encoder
    // they come from is new (ibx#240).
    if let K::AdjustableStop {
        trigger_price, adjusted_order_type, adjusted_stop_price,
        adjusted_stop_limit_price, adjusted_trailing_amount, adjustable_trailing_unit, ..
    } = &kind {
        fields.push((6257, "1".to_string()));                     // has adjustable params
        fields.push((6261, adjusted_order_type.fix_code().to_string()));
        fields.push((6258, format_price(*trigger_price).to_string()));
        fields.push((6259, format_price(*adjusted_stop_price).to_string()));
        if *adjusted_stop_limit_price > 0 {
            fields.push((6262, format_price(*adjusted_stop_limit_price).to_string()));
        }
        // Trailing amount + unit for a Trail/TrailLimit conversion
        // (ib-agent#167, ibx#225).
        if matches!(adjusted_order_type,
            crate::types::AdjustedOrderType::Trail
            | crate::types::AdjustedOrderType::TrailLimit)
        {
            fields.push((6260, format_price(*adjusted_trailing_amount).to_string()));
            fields.push((6269, adjustable_trailing_unit.to_string()));
        }
    }

    // The optional tags each type appends last, in the position the per-type
    // encoders give them: after 204 and the attribute block, not in among the
    // order-type tags. The values and the conditions are unchanged.
    match &kind {
        K::MidPrice { price_cap } if *price_cap > 0 => {
            fields.push((44, format_price(*price_cap).to_string()));
        }
        K::PegMkt { offset } if *offset > 0 => {
            fields.push((211, format_price(*offset).to_string()));
        }
        K::PegMid { offset } => {
            fields.push((8403, "0.0".to_string())); // midOffsetAtWhole — differentiates PEGMID
            fields.push((8404, "0.0".to_string())); // midOffsetAtHalf
            if *offset > 0 {
                fields.push((211, format_price(*offset).to_string()));
            }
        }
        // Optional initial stop trigger (ib-agent#173).
        K::TrailingStop { trail_stop_price, .. }
        | K::TrailingStopLimit { trail_stop_price, .. }
        | K::TrailPct { trail_stop_price, .. } if *trail_stop_price > 0 => {
            fields.push((6117, format_price(*trail_stop_price).to_string()));
        }
        _ => {}
    }

    // Strategy and preview tags last, in the position they held in the encoders
    // this path replaced: after 204 and the attribute block (ibx#318).
    match &kind {
        K::Adaptive { priority, .. } => {
            fields.push((847, "Adaptive".to_string()));
            fields.push((5957, "1".to_string()));
            fields.push((5958, "adaptivePriority".to_string()));
            fields.push((5960, priority.as_str().to_string()));
        }
        K::Algo { algo, .. } => {
            let (algo_name, param_strs) = build_algo_tags(algo);
            fields.push((847, algo_name.to_string()));
            // Tag 849 (maxPctVol) for the algos that use it.
            if let AlgoParams::Vwap { max_pct_vol, .. }
                | AlgoParams::ArrivalPx { max_pct_vol, .. }
                | AlgoParams::ClosePx { max_pct_vol, .. } = algo
            {
                fields.push((849, format!("{}", max_pct_vol)));
            }
            fields.push((5957, (param_strs.len() / 2).to_string()));
            // Key/value pairs: 5958=key, 5960=value, repeated.
            for pair in param_strs.chunks_exact(2) {
                fields.push((5958, pair[0].clone()));
                fields.push((5960, pair[1].clone()));
            }
        }
        K::WhatIf { .. } => fields.push((6091, "1".to_string())),
        _ => {}
    }

    let refs: Vec<(u32, &str)> = fields.iter().map(|(t, s)| (*t, s.as_str())).collect();
    conn.send_fix(&refs)
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
                out.push(exchange.clone());                        // exchange
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
            qty: 10, filled, status, ord_type: b'2', tif: b'0', stop_price: 0,
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
        assert_eq!(updates[0].filled_qty, 3);
        assert_eq!(updates[0].remaining_qty, 7);
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

    /// Encode one request and return the frame with the parts that cannot be
    /// equal between two sends removed: sequence number, both timestamps, and
    /// the body length and checksum that cover them.
    fn encode_for_test(req: crate::types::OrderRequest) -> String {
        use std::io::Read;
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut conn = Some(conn);
        let mut context = Context::new();
        context.register_instrument(756733);
        context.set_symbol(0, "SPY".to_string());
        context.pending_orders.push(req);
        let mut hb = crate::engine::hot_loop::HeartbeatState::new();
        let shared = std::sync::Arc::new(SharedState::new());
        drain_and_send_orders(&mut conn, &mut context, "DU123456", &mut hb, false, &shared);

        let mut buf = [0u8; 8192];
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n])
            .split('\u{1}')
            .filter(|f| !f.is_empty())
            .filter(|f| !["9=", "10=", "34=", "52=", "60="].iter().any(|t| f.starts_with(t)))
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Every order type is encoded twice: once by its own request variant and
    /// once through the shared encoder carrying the same order with no extended
    /// attributes. The two must be the same message.
    ///
    /// That equality is what makes the per-type variants redundant. While both
    /// exist, it is also what stops them drifting — a tag added to one encoder
    /// and not the other is exactly how an order type ends up shipping without
    /// something the caller set, which is #240 and #318.
    #[test]
    fn the_shared_encoder_restates_every_type_exactly_as_its_own_variant_does() {
        use crate::types::{OrderKind as K, OrderRequest as R, PRICE_SCALE};
        let (id, inst, side, qty) = (7u64, 0u32, Side::Buy, 1u32);
        let px = 100 * PRICE_SCALE;
        let stop = 90 * PRICE_SCALE;
        let off = PRICE_SCALE / 2;

        let cases: Vec<(&str, R, K)> = vec![
            ("MKT", R::SubmitMarket { order_id: id, instrument: inst, side, qty }, K::Market),
            ("LMT", R::SubmitLimit { order_id: id, instrument: inst, side, qty, price: px },
                K::Limit { price: px }),
            ("STP", R::SubmitStop { order_id: id, instrument: inst, side, qty, stop_price: stop },
                K::Stop { stop_price: stop }),
            ("STP LMT", R::SubmitStopLimit { order_id: id, instrument: inst, side, qty, price: px, stop_price: stop },
                K::StopLimit { price: px, stop_price: stop }),
            ("MOC", R::SubmitMoc { order_id: id, instrument: inst, side, qty }, K::Moc),
            ("LOC", R::SubmitLoc { order_id: id, instrument: inst, side, qty, price: px },
                K::Loc { price: px }),
            ("MIT", R::SubmitMit { order_id: id, instrument: inst, side, qty, stop_price: stop },
                K::Mit { stop_price: stop }),
            ("LIT", R::SubmitLit { order_id: id, instrument: inst, side, qty, price: px, stop_price: stop },
                K::Lit { price: px, stop_price: stop }),
            ("MTL", R::SubmitMtl { order_id: id, instrument: inst, side, qty }, K::Mtl),
            ("MKT PRT", R::SubmitMktPrt { order_id: id, instrument: inst, side, qty }, K::MktPrt),
            ("STP PRT", R::SubmitStpPrt { order_id: id, instrument: inst, side, qty, stop_price: stop },
                K::StpPrt { stop_price: stop }),
            ("MIDPX", R::SubmitMidPrice { order_id: id, instrument: inst, side, qty, price_cap: px },
                K::MidPrice { price_cap: px }),
            ("SNAP MKT", R::SubmitSnapMkt { order_id: id, instrument: inst, side, qty }, K::SnapMkt),
            ("SNAP MID", R::SubmitSnapMid { order_id: id, instrument: inst, side, qty }, K::SnapMid),
            ("SNAP PRI", R::SubmitSnapPri { order_id: id, instrument: inst, side, qty }, K::SnapPri),
            ("PEG MKT", R::SubmitPegMkt { order_id: id, instrument: inst, side, qty, offset: off },
                K::PegMkt { offset: off }),
            ("PEG MID", R::SubmitPegMid { order_id: id, instrument: inst, side, qty, offset: off },
                K::PegMid { offset: off }),
            ("REL", R::SubmitRel { order_id: id, instrument: inst, side, qty, offset: off },
                K::Rel { offset: off }),
            ("TRAIL", R::SubmitTrailingStop { order_id: id, instrument: inst, side, qty, trail_amt: off, trail_stop_price: stop },
                K::TrailingStop { trail_amt: off, trail_stop_price: stop }),
            ("TRAIL LIMIT", R::SubmitTrailingStopLimit { order_id: id, instrument: inst, side, qty, lmt_offset: off, trail_amt: off, trail_stop_price: stop },
                K::TrailingStopLimit { lmt_offset: off, trail_amt: off, trail_stop_price: stop }),
            ("TRAIL PCT", R::SubmitTrailingStopPct { order_id: id, instrument: inst, side, qty, trail_pct: 100, trail_stop_price: stop },
                K::TrailPct { trail_pct: 100, trail_stop_price: stop }),
        ];

        let mut differences = Vec::new();
        for (name, plain, kind) in cases {
            let own = encode_for_test(plain);
            let shared = encode_for_test(R::SubmitEx {
                order_id: id, instrument: inst, side, qty, kind,
                tif: b'0', attrs: crate::types::OrderAttrs::default(),
            });
            if own != shared {
                differences.push(format!("{name}\n   own: {own}\n  shrd: {shared}"));
            }
        }
        assert!(differences.is_empty(),
            "{} type(s) encoded differently:\n{}", differences.len(), differences.join("\n"));
    }

    /// ibx#318: adaptive, algo and what-if orders returned early into their own
    /// encoders, which carried no attribute block at all — so outside-RTH, the
    /// parent link and the OCA group were accepted by the API and silently
    /// dropped, and the tif was hard-coded to DAY. Asserted on the bytes,
    /// because the enum-level tests passed throughout.
    #[test]
    fn adaptive_wire_carries_the_attributes_and_keeps_its_algo_tags() {
        let msg = send_kind_for_test(
            crate::types::OrderKind::Adaptive {
                price: 100 * crate::types::PRICE_SCALE,
                priority: crate::types::AdaptivePriority::Urgent,
            },
            b'1',
            bracket_child_attrs(),
        );
        let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {}", msg);
        assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {}", msg);
        assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {}", msg);
        assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {}", msg);

        // And everything the standalone encoder emitted is unchanged.
        assert_eq!(tag("40=").as_deref(), Some("2"));
        assert_eq!(tag("18=").as_deref(), Some("e"), "adaptive wrapper missing: {}", msg);
        assert_eq!(tag("847=").as_deref(), Some("Adaptive"));
        assert_eq!(tag("5957=").as_deref(), Some("1"));
        assert_eq!(tag("5958=").as_deref(), Some("adaptivePriority"));
        assert_eq!(tag("5960=").as_deref(), Some("Urgent"));
        assert!(msg.find("204=").unwrap() < msg.find("847=").unwrap(),
            "the strategy tags keep their position after 204: {}", msg);
    }

    #[test]
    fn algo_wire_carries_the_attributes_and_keeps_its_algo_tags() {
        let msg = send_kind_for_test(
            crate::types::OrderKind::Algo {
                price: 100 * crate::types::PRICE_SCALE,
                algo: AlgoParams::Vwap {
                    max_pct_vol: 0.25,
                    no_take_liq: true,
                    allow_past_end_time: false,
                    start_time: String::new(),
                    end_time: String::new(),
                },
            },
            b'1',
            bracket_child_attrs(),
        );
        let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {}", msg);
        assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {}", msg);
        assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {}", msg);
        assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {}", msg);

        assert_eq!(tag("847=").as_deref(), Some("Vwap"));
        assert_eq!(tag("849=").as_deref(), Some("0.25"), "maxPctVol missing: {}", msg);
        assert_eq!(tag("5957=").as_deref(), Some("4"), "param count: {}", msg);
        assert_eq!(tag("5958=").as_deref(), Some("noTakeLiq"));
        assert_eq!(tag("5960=").as_deref(), Some("1"));
    }

    #[test]
    fn what_if_wire_carries_the_attributes_and_keeps_its_preview_flag() {
        let msg = send_kind_for_test(
            crate::types::OrderKind::WhatIf { price: 100 * crate::types::PRICE_SCALE },
            b'1',
            bracket_child_attrs(),
        );
        let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {}", msg);
        assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {}", msg);
        assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {}", msg);
        assert_eq!(tag("6091=").as_deref(), Some("1"), "what-if flag missing: {}", msg);
        assert!(msg.find("204=").unwrap() < msg.find("6091=").unwrap(),
            "the preview flag keeps its position after 204: {}", msg);
    }

    fn bracket_child_attrs() -> crate::types::OrderAttrs {
        crate::types::OrderAttrs {
            parent_id: 42,
            oca_group_str: "bracket_1".to_string(),
            oca_type: 1,
            outside_rth: true,
            ..Default::default()
        }
    }

    /// Encode one kind and return the frame as text.
    fn send_kind_for_test(
        kind: crate::types::OrderKind,
        tif: u8,
        attrs: crate::types::OrderAttrs,
    ) -> String {
        use std::io::Read;
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        send_order_ex(&mut conn, &mut context, "DU123456", 7, 0, Side::Buy, 1, kind, tif, &attrs)
            .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    /// ibx#240: the tags a bracket child cannot ship without. Asserted on the
    /// bytes `send_order_ex` puts on the wire, not on the request enum — the
    /// enum-level tests passed throughout the period the child shipped naked.
    #[test]
    fn adjustable_stop_wire_carries_parent_oca_and_tif() {
        use std::io::Read;
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        let attrs = crate::types::OrderAttrs {
            parent_id: 42,
            oca_group_str: "bracket_1".to_string(),
            oca_type: 1,
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, "DU123456", 7, 0, Side::Sell, 1,
            crate::types::OrderKind::AdjustableStop {
                stop_price: 11 * crate::types::PRICE_SCALE,
                trigger_price: 12 * crate::types::PRICE_SCALE,
                adjusted_order_type: crate::types::AdjustedOrderType::Stop,
                adjusted_stop_price: 11 * crate::types::PRICE_SCALE + crate::types::PRICE_SCALE / 2,
                adjusted_stop_limit_price: 0,
                adjusted_trailing_amount: 0,
                adjustable_trailing_unit: 0,
            },
            b'1',           // GTC
            &attrs,
        ).unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {}", msg);
        assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {}", msg);
        assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {}", msg);
        // The adjustable-specific tags keep both the values and the position the
        // standalone arm gave them — after 204 and the attribute block — which
        // the sibling test pins by asserting 204 precedes 6257.
        assert_eq!(tag("40=").as_deref(), Some("3"));
        assert_eq!(tag("99="), Some(format_price(11 * crate::types::PRICE_SCALE).to_string()));
        assert_eq!(tag("6257=").as_deref(), Some("1"));
        assert_eq!(tag("6261=").as_deref(), Some(crate::types::AdjustedOrderType::Stop.fix_code()));
        assert_eq!(tag("6258="), Some(format_price(12 * crate::types::PRICE_SCALE).to_string()));
        assert_eq!(tag("6259="),
            Some(format_price(11 * crate::types::PRICE_SCALE + crate::types::PRICE_SCALE / 2).to_string()));
    }

    /// The conditional adjustable tags: 6262 only with a stop-limit conversion,
    /// 6260/6269 only with a trailing one. Same rules as the standalone arm.
    #[test]
    fn adjustable_stop_wire_carries_trail_and_limit_tags() {
        use std::io::Read;
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        send_order_ex(
            &mut conn, &mut context, "DU123456", 8, 0, Side::Sell, 1,
            crate::types::OrderKind::AdjustableStop {
                stop_price: 11 * crate::types::PRICE_SCALE,
                trigger_price: 12 * crate::types::PRICE_SCALE,
                adjusted_order_type: crate::types::AdjustedOrderType::TrailLimit,
                adjusted_stop_price: 11 * crate::types::PRICE_SCALE,
                adjusted_stop_limit_price: 10 * crate::types::PRICE_SCALE,
                adjusted_trailing_amount: crate::types::PRICE_SCALE / 2,
                adjustable_trailing_unit: 0,
            },
            b'0',
            &crate::types::OrderAttrs::default(),
        ).unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("6262="), Some(format_price(10 * crate::types::PRICE_SCALE).to_string()));
        assert_eq!(tag("6260="), Some(format_price(crate::types::PRICE_SCALE / 2).to_string()));
        assert_eq!(tag("6269=").as_deref(), Some("0"));
        // No parent, no OCA set: those tags must be absent, not empty.
        assert_eq!(tag("6107="), None);
        assert_eq!(tag("583="), None);

        // Order, not just presence: the adjustable tags sit after 204 and the
        // base type tags before 59, exactly where the dedicated encoder this
        // path replaced put them. Tag order is not supposed to carry meaning,
        // but this path had a shipped layout and there is no reason to change
        // it as a side effect (ibx#240).
        let pos = |t: &str| msg.split('\u{1}').position(|f| f.starts_with(t));
        assert!(pos("40=") < pos("59="), "base type tags precede tif: {}", msg);
        assert!(pos("99=") < pos("59="), "stop price precedes tif: {}", msg);
        assert!(pos("204=") < pos("6257="), "adjustable tags follow 204: {}", msg);
        assert!(pos("6257=") < pos("6261="), "adjustable tags keep their order: {}", msg);
        assert!(pos("6259=") < pos("6262="), "adjustable tags keep their order: {}", msg);
        assert!(pos("6262=") < pos("6260="), "adjustable tags keep their order: {}", msg);
    }
}
