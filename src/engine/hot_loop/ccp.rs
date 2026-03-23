use std::collections::HashSet;
use std::time::Instant;

use crate::bridge::{Event, RichOrderInfo, SharedState};
use crate::api::types as api;
use crate::engine::context::Context;
use crate::config::{chrono_free_timestamp, unix_to_ib_datetime};
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::types::{
    AlgoParams, CompletedOrder, Fill, InstrumentId, NewsBulletin,
    OrderCondition, OrderRequest, PositionInfo, Price, Side, PRICE_SCALE,
};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, format_price, parse_price_tag, format_qty};

pub(crate) struct CcpState {
    pub(crate) seen_exec_ids: HashSet<String>,
    pub(crate) bulletin_next_id: i32,
    pub(crate) news_subscriptions: Vec<(InstrumentId, u32)>,
    pub(crate) disconnected: bool,
    pub(crate) pending_secdef: Vec<u32>,
    pub(crate) pending_matching_symbols: Vec<u32>,
}

impl CcpState {
    pub(crate) fn new() -> Self {
        Self {
            seen_exec_ids: HashSet::with_capacity(256),
            bulletin_next_id: 0,
            news_subscriptions: Vec::new(),
            disconnected: false,
            pending_secdef: Vec::new(),
            pending_matching_symbols: Vec::new(),
        }
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
        let messages = match ccp_conn.as_mut() {
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
                        hb.pending_ccp_test = None;
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.extend(fixcomp::fixcomp_decompress(&unsigned));
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.push(unsigned);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            msgs.push(unsigned);
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
        log::debug!("CCP msg 35={}", msg_type);
        match msg_type {
            fix::MSG_EXEC_REPORT => self.handle_exec_report(&parsed, context, shared, event_tx, account_id),
            fix::MSG_CANCEL_REJECT => self.handle_cancel_reject(&parsed, context, shared, event_tx),
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
                log::warn!("SessionReject: reason='{}' refTag={}", reason, ref_tag);
            }
            "U" => {
                if let Some(comm) = parsed.get(&6040) {
                    log::trace!("CCP U msg: 6040={}", comm);
                    match comm.as_str() {
                        "77" => self.handle_account_summary(&parsed, context, shared),
                        "186" => {
                            if let Some(matches) = crate::control::contracts::parse_matching_symbols_response(msg) {
                                if !matches.is_empty() {
                                    if let Some(req_id) = self.pending_matching_symbols.first().copied() {
                                        self.pending_matching_symbols.remove(0);
                                        shared.reference.push_matching_symbols(req_id, matches);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "UT" | "UM" | "RL" => handle_account_update(msg, context, shared),
            "UP" => handle_position_update(&parsed, context, shared, event_tx),
            "d" => {
                if let Some(def) = crate::control::contracts::parse_secdef_response(msg) {
                    let is_last = crate::control::contracts::secdef_response_is_last(msg);
                    if def.con_id != 0 {
                        let sec_type_str = match def.sec_type {
                            crate::control::contracts::SecurityType::Stock => "STK",
                            crate::control::contracts::SecurityType::Option => "OPT",
                            crate::control::contracts::SecurityType::Future => "FUT",
                            crate::control::contracts::SecurityType::Forex => "CASH",
                            crate::control::contracts::SecurityType::Index => "IND",
                            crate::control::contracts::SecurityType::Bond => "BOND",
                            crate::control::contracts::SecurityType::Warrant => "WAR",
                            _ => "STK",
                        };
                        shared.reference.cache_contract(def.con_id as i64, api::Contract {
                            con_id: def.con_id as i64,
                            symbol: def.symbol.clone(),
                            sec_type: sec_type_str.to_string(),
                            exchange: def.exchange.clone(),
                            currency: def.currency.clone(),
                            local_symbol: def.local_symbol.clone(),
                            primary_exchange: def.primary_exchange.clone(),
                            trading_class: def.trading_class.clone(),
                            ..Default::default()
                        });
                    }
                    if let Some(&req_id) = self.pending_secdef.first() {
                        shared.reference.push_contract_details(req_id, def.clone());
                        emit(event_tx, Event::ContractDetails { req_id, details: def });
                        if is_last {
                            self.pending_secdef.remove(0);
                            shared.reference.push_contract_details_end(req_id);
                            emit(event_tx, Event::ContractDetailsEnd(req_id));
                        }
                    }
                }
                let rules = crate::control::contracts::parse_market_rules(msg);
                if !rules.is_empty() {
                    shared.reference.push_market_rules(rules);
                }
            }
            _ => {}
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
        let clord_id = parsed.get(&11).and_then(|s| {
            let stripped = s.strip_prefix('C').unwrap_or(s);
            stripped.parse::<u64>().ok()
        }).unwrap_or(0);

        // What-If response
        if parsed.get(&6091).map(|s| s.as_str()) == Some("1") {
            let init_margin_after = parsed.get(&6092).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            if init_margin_after > 0.0 {
                if let Some(order) = context.order(clord_id).copied() {
                    let response = crate::types::WhatIfResponse {
                        order_id: clord_id,
                        instrument: order.instrument,
                        init_margin_before: parse_price_tag(parsed.get(&6826)),
                        maint_margin_before: parse_price_tag(parsed.get(&6827)),
                        equity_with_loan_before: parse_price_tag(parsed.get(&6828)),
                        init_margin_after: parse_price_tag(parsed.get(&6092)),
                        maint_margin_after: parse_price_tag(parsed.get(&6093)),
                        equity_with_loan_after: parse_price_tag(parsed.get(&6094)),
                        commission: parse_price_tag(parsed.get(&6378)),
                    };
                    log::info!("WhatIf response: clord={} initMargin={:.2}->{:.2} commission={:.2}",
                        clord_id,
                        response.init_margin_before as f64 / PRICE_SCALE as f64,
                        response.init_margin_after as f64 / PRICE_SCALE as f64,
                        response.commission as f64 / PRICE_SCALE as f64);
                    context.remove_order(clord_id);
                    shared.orders.push_what_if(response);
                    emit(event_tx, Event::WhatIf(response));
                }
            }
            return;
        }

        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let exec_id = parsed.get(&17).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let last_shares = parsed.get(&32).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let leaves_qty = parsed.get(&151).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
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

        let status = match ord_status {
            "0" | "A" | "E" | "5" => crate::types::OrderStatus::Submitted,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" | "D" => crate::types::OrderStatus::Cancelled,
            "8" => crate::types::OrderStatus::Rejected,
            "6" => return,
            _ => return,
        };

        let prev_status = context.order(clord_id).map(|o| o.status);
        let status_changed = prev_status != Some(status);
        context.update_order_status(clord_id, status);

        let mut had_fill = false;
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
            if !exec_id.is_empty() && !self.seen_exec_ids.insert(exec_id.to_string()) {
                log::warn!("Duplicate ExecID={} — skipping fill", exec_id);
                return;
            }
            if self.seen_exec_ids.len() > 1024 {
                self.seen_exec_ids.clear();
            }
            if let Some(order) = context.order(clord_id).copied() {
                context.update_order_filled(clord_id, last_shares as u32);
                let fill = Fill {
                    instrument: order.instrument,
                    order_id: clord_id,
                    side: order.side,
                    price: (last_px * PRICE_SCALE as f64) as i64,
                    qty: last_shares,
                    remaining: leaves_qty,
                    commission: (commission * PRICE_SCALE as f64) as i64,
                    timestamp_ns: context.now_ns(),
                };
                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell | Side::ShortSell => -last_shares,
                };
                context.update_position(order.instrument, delta);
                // notify_fill inlined
                shared.orders.push_fill(fill);
                shared.portfolio.set_position(fill.instrument, context.position(fill.instrument));
                emit(event_tx, Event::Fill(fill));
                had_fill = true;
            }
        }

        if status_changed && !had_fill {
            if let Some(order) = context.order(clord_id).copied() {
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty: order.filled as i64,
                    remaining_qty: leaves_qty,
                    timestamp_ns: context.now_ns(),
                };
                shared.orders.push_order_update(update);
                emit(event_tx, Event::OrderUpdate(update));
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
            let perm_id: i64 = parsed.get(&37).and_then(|s| s.parse().ok()).unwrap_or(0);
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

            let tif_str = match tif_tag {
                "0" => "DAY", "1" => "GTC", "3" => "IOC", "4" => "FOK",
                "2" => "OPG", "6" => "GTD", "8" => "AUC", _ => "DAY",
            };

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

            let status_str = match status {
                crate::types::OrderStatus::PendingSubmit => "PendingSubmit",
                crate::types::OrderStatus::Submitted => "Submitted",
                crate::types::OrderStatus::Filled => "Filled",
                crate::types::OrderStatus::PartiallyFilled => "PreSubmitted",
                crate::types::OrderStatus::Cancelled => "Cancelled",
                crate::types::OrderStatus::Rejected => "Inactive",
                crate::types::OrderStatus::Uncertain => "Unknown",
            };

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

            let (fb_action, fb_tif, fb_ord_type) = if let Some(ctx_order) = context.order(clord_id) {
                let a = match ctx_order.side {
                    crate::types::Side::Buy => "BUY",
                    crate::types::Side::Sell | crate::types::Side::ShortSell => "SELL",
                };
                let t = match ctx_order.tif {
                    b'0' => "DAY", b'1' => "GTC", b'3' => "IOC", b'4' => "FOK",
                    b'7' => "OPG", b'6' => "GTD", _ => "",
                };
                let o = match ctx_order.ord_type {
                    b'1' => "MKT", b'2' => "LMT", b'3' => "STP", b'4' => "STP LMT",
                    b'P' => "TRAIL", _ => "",
                };
                (a, t, o)
            } else {
                ("", "", "")
            };

            let order = api::Order {
                order_id: clord_id as i64,
                action: if action.is_empty() { fb_action.to_string() } else { action.to_string() },
                total_quantity: total_qty,
                order_type: if order_type_str.is_empty() { fb_ord_type.to_string() } else { order_type_str.to_string() },
                lmt_price: limit_price,
                aux_price: stop_px,
                tif: if tif_str.is_empty() { fb_tif.to_string() } else { tif_str.to_string() },
                account: if account.is_empty() { account_id.to_string() } else { account.clone() },
                perm_id,
                filled_quantity: leaves_qty as f64,
                outside_rth,
                clearing_intent,
                auto_cancel_date,
                submitter: account_id.to_string(),
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
                commission,
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
                order_id: clord_id as i64,
                cum_qty,
                avg_price: avg_px,
                last_liquidity: last_liq,
                ..Default::default()
            };

            if con_id != 0 {
                shared.reference.cache_contract(con_id, contract.clone());
            }

            shared.orders.push_order_info(clord_id, RichOrderInfo {
                contract, order, order_state, last_exec,
            });
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
                    filled_qty: order.filled as i64,
                    timestamp_ns: context.now_ns(),
                });
            }
            context.remove_order(clord_id);
        }
    }

    fn handle_cancel_reject(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let orig_clord = parsed.get(&41).and_then(|s| s.parse::<u64>().ok());
        let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("Cancel rejected");
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        log::warn!("CancelReject: origClOrd={:?} type={} code={} reason={}",
            orig_clord, reject_type, reason_code, reason);

        if let Some(oid) = orig_clord {
            if let Some(order) = context.order(oid).copied() {
                let restore_status = if order.filled > 0 {
                    crate::types::OrderStatus::PartiallyFilled
                } else {
                    crate::types::OrderStatus::Submitted
                };
                context.update_order_status(oid, restore_status);
                let reject = crate::types::CancelReject {
                    order_id: oid,
                    instrument: order.instrument,
                    reject_type,
                    reason_code,
                    timestamp_ns: context.now_ns(),
                };
                shared.orders.push_cancel_reject(reject);
                emit(event_tx, Event::CancelReject(reject));
            }
        }
    }

    fn handle_news_bulletin(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        static BULLETIN_TYPE_MAP: &[(i32, i32)] = &[
            (1, 1), (2, 2), (3, 3), (8, 1), (9, 1), (10, 1),
        ];
        let fix_type: i32 = parsed.get(&fix::TAG_URGENCY)
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        let api_type = BULLETIN_TYPE_MAP.iter()
            .find(|(k, _)| *k == fix_type)
            .map(|(_, v)| *v);
        let api_type = match api_type {
            Some(t) => t,
            None => return,
        };
        let message = parsed.get(&fix::TAG_HEADLINE).cloned().unwrap_or_default();
        let exchange = parsed.get(&fix::TAG_SECURITY_EXCHANGE).cloned().unwrap_or_default();
        self.bulletin_next_id += 1;
        let bulletin = NewsBulletin {
            msg_id: self.bulletin_next_id,
            msg_type: api_type,
            message,
            exchange,
        };
        shared.market.push_news_bulletin(bulletin);
    }

    fn handle_account_summary(&mut self, parsed: &std::collections::HashMap<u32, String>, context: &mut Context, shared: &SharedState) {
        if let Some(val) = parsed.get(&9806).and_then(|s| s.parse::<f64>().ok()) {
            context.account.net_liquidation = (val * PRICE_SCALE as f64) as Price;
            log::info!("Account summary: net_liq=${:.2}", val);
        }
        shared.portfolio.set_account(context.account());
    }

    pub(crate) fn drain_and_send_orders(
        &mut self,
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
                    let price_str = format_price(price);
                    let tif_byte = [tif];
                    let tif_str = std::str::from_utf8(&tif_byte).unwrap_or("0");
                    let symbol = context.market.symbol(instrument).to_string();
                    let now = chrono_free_timestamp();
                    let display_str = attrs.display_size.to_string();
                    let min_qty_str = attrs.min_qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let clord_str = order_id.to_string();
                    let side_str = fix_side(side);
                    let qty_str = qty.to_string();
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
                    let orig_clord = order_id.to_string();
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
                    let clord_str = new_order_id.to_string();
                    let orig_clord = order_id.to_string();
                    let qty_str = qty.to_string();
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
            let con_id_str = (con_id as u32).to_string();
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

    pub(crate) fn send_secdef_request(&mut self, req_id: u32, con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
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
        }
        self.pending_secdef.push(req_id);
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: u32, symbol: &str, sec_type: &str, exchange: &str, currency: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let fix_exchange = if exchange == "SMART" { "BEST" } else { exchange };
            let fix_sec_type = match sec_type {
                "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND", other => other,
            };
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &req_id_str),
                (321, "2"),
                (55, symbol),
                (167, fix_sec_type),
                (207, fix_exchange),
                (15, currency),
                (6088, "Socket"),
            ]);
            log::info!("Sent secdef-by-symbol: req_id={} symbol={} sec_type={}", req_id, symbol, sec_type);
            hb.last_ccp_sent = Instant::now();
        }
        self.pending_secdef.push(req_id);
    }

    pub(crate) fn send_matching_symbols_request(&mut self, req_id: u32, pattern: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "185"),
                (320, &req_id_str),
                (58, pattern),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent matching symbols request: req_id={} pattern='{}'", req_id, pattern);
        }
        self.pending_matching_symbols.push(req_id);
    }

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        context.mark_orders_uncertain();
        emit(event_tx, Event::Disconnected);
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        *ccp_conn = Some(conn);
        self.disconnected = false;
        hb.last_ccp_sent = Instant::now();
        hb.last_ccp_recv = Instant::now();
        hb.pending_ccp_test = None;

        if let Some(conn) = ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let result = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "H"),
                (fix::TAG_SENDING_TIME, &ts),
                (11, "*"),
                (54, "*"),
                (55, "*"),
            ]);
            match result {
                Ok(()) => {
                    hb.last_ccp_sent = Instant::now();
                    log::info!("CCP reconnected, sent order mass status request");
                }
                Err(e) => log::error!("CCP reconnected but mass status request failed: {}", e),
            }
        }
    }
}

/// Convert Side to FIX tag 54 value.
pub(super) fn fix_side(side: Side) -> &'static str {
    match side {
        Side::Buy => "1",
        Side::Sell => "2",
        Side::ShortSell => "5",
    }
}

/// Handle account update messages (cross-cutting, called from CCP message processing).
pub(crate) fn handle_account_update(msg: &[u8], context: &mut Context, shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
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
}

/// Handle position update messages (cross-cutting, called from CCP message processing).
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
    let position: i64 = parsed.get(&6064)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v as i64)
        .unwrap_or(0);
    let avg_cost: Price = parsed.get(&6065)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| (v * PRICE_SCALE as f64) as Price)
        .unwrap_or(0);

    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
        let current = context.position(instrument);
        let delta = position - current;
        if delta != 0 {
            context.update_position(instrument, delta);
        }
        shared.portfolio.set_position_info(PositionInfo { con_id, position, avg_cost });
        shared.portfolio.set_position(instrument, position);
        emit(event_tx, Event::PositionUpdate { instrument, con_id, position, avg_cost });
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
                out.push(format_price(*price));                    // price
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
