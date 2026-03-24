use std::time::Instant;

use crate::config::{chrono_free_timestamp, unix_to_ib_datetime};
use crate::engine::context::Context;
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{AlgoParams, OrderCondition, OrderRequest, Side};

use super::{HeartbeatState, format_price, format_qty, format_int, format_uint};

pub(crate) fn drain_and_send_orders(
    ccp_conn: &mut Option<Connection>,
    context: &mut Context,
    account_id: &str,
    hb: &mut HeartbeatState,
) {
    let orders: Vec<OrderRequest> = context.drain_pending_orders().collect();
    let conn = match ccp_conn.as_mut() {
        Some(c) => c,
        None => return,
    };
    for order_req in orders {
        let result = match order_req {
            OrderRequest::SubmitLimit { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),       // SecurityType = CommonStock
                    (100, "SMART"),     // ExDestination
                    (15, "USD"),        // Currency
                    (204, "0"),         // CustomerOrFirm
                ])
            }
            OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'4', b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitLimitGtc { order_id, instrument, side, qty, price, outside_rth } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'1', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ];
                if outside_rth {
                    fields.push((6433, "1")); // OutsideRTH
                }
                conn.send_fix(&fields)
            }
            OrderRequest::SubmitLimitEx { order_id, instrument, side, qty, price, tif, attrs } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', tif, 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let tif_byte = [tif];
                let tif_str = std::str::from_utf8(&tif_byte).unwrap_or("0");
                let symbol = context.market.symbol(instrument).to_string();
                let now = chrono_free_timestamp();
                let display_str = format_uint(attrs.display_size as u64);
                let min_qty_str = format_uint(attrs.min_qty as u64);
                let gat_str = if attrs.good_after > 0 { unix_to_ib_datetime(attrs.good_after) } else { String::new() };
                let gtd_str = if attrs.good_till > 0 { unix_to_ib_datetime(attrs.good_till) } else { String::new() };
                let oca_str = if attrs.oca_group > 0 { format!("OCA_{}", attrs.oca_group) } else { String::new() };
                let mut fields: Vec<(u32, &str)> = vec![
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
                    (59, tif_str),
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ];
                if attrs.display_size > 0 {
                    fields.push((111, &display_str));
                }
                if attrs.min_qty > 0 {
                    fields.push((110, &min_qty_str));
                }
                if attrs.outside_rth {
                    fields.push((6433, "1"));
                }
                if attrs.hidden {
                    fields.push((6135, "1"));
                }
                if attrs.good_after > 0 {
                    fields.push((168, &gat_str));
                }
                if attrs.good_till > 0 {
                    fields.push((126, &gtd_str));
                }
                if attrs.oca_group > 0 {
                    fields.push((583, &oca_str));
                    fields.push((6209, "CancelOnFillWBlock"));
                }
                let disc_str;
                if attrs.discretionary_amt > 0 {
                    disc_str = format_price(attrs.discretionary_amt);
                    fields.push((9813, &disc_str));
                }
                if attrs.sweep_to_fill {
                    fields.push((6102, "1"));
                }
                if attrs.all_or_none {
                    fields.push((18, "G"));
                }
                let trigger_str;
                if attrs.trigger_method > 0 {
                    trigger_str = attrs.trigger_method.to_string();
                    fields.push((6115, &trigger_str));
                }
                let cash_qty_str;
                if attrs.cash_qty > 0 {
                    cash_qty_str = format_price(attrs.cash_qty);
                    fields.push((5920, &cash_qty_str));
                }
                // Condition tags (6136+ framework)
                let cond_strs = build_condition_strings(&attrs.conditions);
                if !attrs.conditions.is_empty() {
                    let count_str = &cond_strs[0]; // first element is count
                    fields.push((6136, count_str));
                    if attrs.conditions_cancel_order {
                        fields.push((6128, "1"));
                    }
                    if attrs.conditions_ignore_rth {
                        fields.push((6151, "1"));
                    }
                    // Per-condition tags start at index 1, 11 strings per condition
                    for i in 0..attrs.conditions.len() {
                        let base = 1 + i * 11;
                        fields.push((6222, &cond_strs[base]));      // condType
                        fields.push((6137, &cond_strs[base + 1]));  // conjunction
                        fields.push((6126, &cond_strs[base + 2]));  // operator
                        fields.push((6123, &cond_strs[base + 3]));  // conId
                        fields.push((6124, &cond_strs[base + 4]));  // exchange
                        fields.push((6127, &cond_strs[base + 5]));  // triggerMethod
                        fields.push((6125, &cond_strs[base + 6]));  // price
                        fields.push((6223, &cond_strs[base + 7]));  // time
                        fields.push((6245, &cond_strs[base + 8]));  // percent
                        fields.push((6263, &cond_strs[base + 9]));  // volume
                        fields.push((6246, &cond_strs[base + 10])); // execution
                    }
                }
                conn.send_fix(&fields)
            }
            OrderRequest::SubmitMarket { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'1', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),       // SecurityType
                    (100, "SMART"),     // ExDestination
                    (15, "USD"),        // Currency
                    (204, "0"),         // CustomerOrFirm
                ])
            }
            OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'3', b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitStopGtc { order_id, instrument, side, qty, stop_price, outside_rth } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'3', b'1', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
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
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
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
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitLimitFok { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'4', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let trail_str = format_price(trail_amt);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "P"),          // OrdType = Trailing Stop
                    (99, &trail_str),   // StopPx = trail amount
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, price, trail_amt } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'P', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let trail_str = format_price(trail_amt);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "P"),          // OrdType = Trailing Stop (IB uses P for both)
                    (44, &price_str),   // Limit price
                    (99, &trail_str),   // StopPx = trail amount
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_pct } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'P', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let pct_str = trail_pct.to_string(); // basis points: 100 = 1%
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "P"),              // OrdType = Trailing Stop
                    (6268, &pct_str),       // TrailingPercent (basis points)
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitMoc { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'5', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitLoc { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'B', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, stop_price, b'J', b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'K', b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "K"),          // OrdType = Limit if Touched
                    (44, &price_str),   // Limit price
                    (99, &stop_str),    // StopPx = trigger price
                    (59, "0"),          // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
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
                    (167, "STK"),
                    (100, "SMART"),
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                    (6107, &parent_str),       // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, "CancelOnFillWBlock"), // OCA cancel-on-fill
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                    (6107, &parent_str),       // ParentOrderID
                    (583, &oca_group),         // OCAGroup
                    (6209, "CancelOnFillWBlock"), // OCA cancel-on-fill
                ])
            }
            OrderRequest::SubmitRel { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'R', b'0', offset,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let offset_str = format_price(offset);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "R"),              // OrdType = Relative
                    (99, &offset_str),      // Peg offset (auxPrice)
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitLimitOpg { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'2', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitAdaptive { order_id, instrument, side, qty, price, priority } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
                let now = chrono_free_timestamp();
                let priority_str = priority.as_str();
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
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                    (847, "Adaptive"),      // AlgoStrategy
                    (5957, "1"),            // AlgoParamCount
                    (5958, "adaptivePriority"), // AlgoParamTag
                    (5960, priority_str),   // AlgoParamValue
                ])
            }
            OrderRequest::SubmitAlgo { order_id, instrument, side, qty, price, algo } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, b'2', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (40, "2"),              // OrdType = Limit
                    (44, &price_str),
                    (59, "0"),              // TIF = DAY
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ];
                let (algo_name, param_strs) = build_algo_tags(&algo);
                fields.push((847, algo_name));
                // Tag 849 (maxPctVol) for algos that use it
                let pct_str = match &algo {
                    AlgoParams::Vwap { max_pct_vol, .. }
                    | AlgoParams::ArrivalPx { max_pct_vol, .. }
                    | AlgoParams::ClosePx { max_pct_vol, .. } => format!("{}", max_pct_vol),
                    _ => String::new(),
                };
                if !pct_str.is_empty() {
                    fields.push((849, &pct_str));
                }
                let count_str = (param_strs.len() / 2).to_string();
                fields.push((5957, &count_str));
                // Emit key/value pairs: 5958=key, 5960=value (repeated)
                let mut i = 0;
                while i < param_strs.len() {
                    fields.push((5958, &param_strs[i]));
                    fields.push((5960, &param_strs[i + 1]));
                    i += 2;
                }
                conn.send_fix(&fields)
            }
            OrderRequest::SubmitPegBench { order_id, instrument, side, qty, price,
                ref_con_id, is_peg_decrease, pegged_change_amount, ref_change_amount } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, crate::types::ORD_PEG_BENCH, b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
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
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitMtlAuc { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'K', b'8', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitWhatIf { order_id, instrument, side, qty, price } => {
                // What-if: insert with ORD_WHAT_IF marker so we can detect the response
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price, crate::types::ORD_WHAT_IF, b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (44, &price_str),
                    (59, "0"),
                    (60, &now),
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                    (6091, "1"),         // What-If flag
                ])
            }
            OrderRequest::SubmitLimitFractional { order_id, instrument, side, qty, price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, 0, price, b'2', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_qty(qty);
                let price_str = format_price(price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitAdjustableStop { order_id, instrument, side, qty,
                stop_price, trigger_price, adjusted_order_type,
                adjusted_stop_price, adjusted_stop_limit_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'3', b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
                let trigger_str = format_price(trigger_price);
                let adj_stop_str = format_price(adjusted_stop_price);
                let adj_limit_str = format_price(adjusted_stop_limit_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                    (6257, "1"),            // Has adjustable params flag
                    (6261, adjusted_order_type.fix_code()), // Adjusted order type
                    (6258, &trigger_str),   // Trigger price
                    (6259, &adj_stop_str),  // Adjusted stop price
                ];
                if adjusted_stop_limit_price > 0 {
                    fields.push((6262, &adj_limit_str)); // Adjusted stop limit price
                }
                conn.send_fix(&fields)
            }
            OrderRequest::SubmitMtl { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'K', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitMktPrt { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, b'U', b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_STP_PRT, b'0', stop_price,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let stop_str = format_price(stop_price);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "SMART"),
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, price_cap, crate::types::ORD_MIDPX, b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange (not SMART)
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
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitSnapMid { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_MID, b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitSnapPri { order_id, instrument, side, qty } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_SNAP_PRI, b'0', 0,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange
                    (15, "USD"),
                    (204, "0"),
                ])
            }
            OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, offset } => {
                context.insert_order(crate::types::Order::new(
                    order_id, instrument, side, qty, 0, crate::types::ORD_PEG_MKT, b'0', offset,
                ));
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange
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
                let clord_str = format_int(order_id as i64);
                let side_str = fix_side(side);
                let qty_str = format_uint(qty as u64);
                let symbol = context.market.symbol(instrument).to_string();
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
                    (167, "STK"),
                    (100, "ISLAND"),    // Requires directed exchange
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
                let clord_str = format!("C{}", order_id);
                let orig_clord = format_int(order_id as i64);
                let now = chrono_free_timestamp();
                conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),   // ClOrdID (cancel)
                    (41, &orig_clord),  // OrigClOrdID
                    (60, &now),         // TransactTime
                ])
            }
            OrderRequest::CancelAll { instrument } => {
                let open_ids: Vec<u64> = context.open_orders_for(instrument)
                    .iter()
                    .map(|o| o.order_id)
                    .collect();
                let mut last_result = Ok(());
                for oid in open_ids {
                    let clord_str = format!("C{}", oid);
                    let orig_clord = oid.to_string();
                    let now = chrono_free_timestamp();
                    last_result = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_ORDER_CANCEL),
                        (fix::TAG_SENDING_TIME, &now),
                        (11, &clord_str),
                        (41, &orig_clord),
                        (60, &now),
                    ]);
                }
                last_result
            }
            OrderRequest::Modify { new_order_id, order_id, price, qty } => {
                let orig = context.order(order_id).copied();
                if let Some(orig) = orig {
                    context.insert_order(crate::types::Order::new(
                        new_order_id, orig.instrument, orig.side, qty, price,
                        orig.ord_type, orig.tif, orig.stop_price,
                    ));
                }
                let clord_str = format_int(order_id as i64);
                let orig_clord = format_int(order_id as i64);
                let qty_str = format_uint(qty as u64);
                let price_str = format_price(price);
                let now = chrono_free_timestamp();
                let side_str = orig.map(|o| fix_side(o.side)).unwrap_or("1");
                let symbol = orig.map(|o| context.market.symbol(o.instrument).to_string())
                    .unwrap_or_default();
                let ord_type_str = crate::types::ord_type_fix_str(orig.map(|o| o.ord_type).unwrap_or(b'2')).to_string();
                let tif_str = std::str::from_utf8(&[orig.map(|o| o.tif).unwrap_or(b'0')]).unwrap_or("0").to_string();
                let mut fields: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_ORDER_REPLACE),
                    (fix::TAG_SENDING_TIME, &now),
                    (11, &clord_str),   // ClOrdID
                    (41, &orig_clord),  // OrigClOrdID
                    (1, account_id),    // Account
                    (21, "2"),          // HandlInst = Automated
                    (55, &symbol),      // Symbol
                    (54, side_str),     // Side
                    (38, &qty_str),     // OrderQty
                    (40, &ord_type_str), // OrdType from original order
                    (44, &price_str),   // Price
                    (59, &tif_str),     // TIF from original order
                    (60, &now),         // TransactTime
                    (167, "STK"),       // SecurityType
                    (100, "SMART"),     // ExDestination
                    (15, "USD"),        // Currency
                    (204, "0"),         // CustomerOrFirm
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
            Err(e) => log::error!("Failed to send order: {}", e),
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
