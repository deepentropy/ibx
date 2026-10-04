//! Event dispatch: drains SharedState queues and fires Wrapper callbacks.

use crate::api::types::{
    BarData, ContractDetails, ContractDescription, Execution,
    Order as ApiOrder, TickAttribLast, TickAttribBidAsk, PRICE_SCALE_F, QTY_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::client_core::{order_status_str, ClientCore, MdOut, MdTick};
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Message Processing ──

    /// Drain all SharedState queues and dispatch to the Wrapper.
    /// Call this in a loop — it is the Rust equivalent of C++ `EReader::processMsgs()`.
    pub fn process_msgs(&self, wrapper: &mut impl Wrapper) {
        self.dispatch_orders(wrapper);
        self.dispatch_quotes(wrapper);
        self.dispatch_data(wrapper);
        self.dispatch_connection(wrapper);
    }

    // ── Connection Dispatch ──

    /// Surface the end of the session: `connection_closed`, once, with no error
    /// callback. Covers both an engine-side loss and an explicit
    /// [`disconnect()`](EClient::disconnect) (ibx#242).
    ///
    /// Queued data is dispatched before this fires, so a caller that stops
    /// polling on `connection_closed` still sees everything the engine had
    /// already produced.
    fn dispatch_connection(&self, wrapper: &mut impl Wrapper) {
        use std::sync::atomic::Ordering;
        // Link lost / restored and farm status (ibx#399): errors with id -1.
        // The session stays open across a lost link.
        for (code, msg) in self.shared.drain_connection_notices() {
            wrapper.error(-1, code, &msg, "");
        }
        if self.shared.take_connection_lost() {
            self.connected.store(false, Ordering::Release);
        }
        if !self.connected.load(Ordering::Acquire)
            && !self.close_notified.swap(true, Ordering::AcqRel)
        {
            wrapper.connection_closed();
        }
    }

    // ── Order / Fill Dispatch ──

    fn dispatch_orders(&self, wrapper: &mut impl Wrapper) {
        // Open-order requests held while the auth link was lost: taken
        // before the order updates and answered after them, so the answer
        // has the replayed statuses (ibx#251).
        let released = self.core.released_open_orders(&self.shared);
        // Fills → order_status + exec_details. The commission report comes
        // later, from its own server frame (ibx#471).
        for (fill, fill_exec) in self.shared.orders.drain_fills_with_exec() {
            let price_f = fill.price as f64 / PRICE_SCALE_F;
            let status = if fill.remaining_fixed == 0 { "Filled" } else { self.core.partial_fill_status(fill.order_id) };
            let (perm_id, parent_id) = self.shared.orders.get_order_info(fill.order_id)
                .map(|info| (info.order.perm_id, info.order.parent_id))
                .unwrap_or((0, 0));
            // filled and avgFillPrice are the order totals, lastFillPrice
            // is this print (ibx#315). Quantities are fixed-point; the
            // callbacks take decimal shares (ibx#313).
            let filled_f = fill.filled_so_far_fixed() as f64 / QTY_SCALE_F;
            let remaining_f = fill.remaining_fixed as f64 / QTY_SCALE_F;
            let shares_f = fill.qty_fixed as f64 / QTY_SCALE_F;
            let avg_f = fill.average_price() as f64 / PRICE_SCALE_F;

            let side_str = match fill.side {
                Side::Buy => "BOT",
                Side::Sell => "SLD",
                Side::ShortSell => "SLD",
            };
            let (c, mut exec) = if let Some(info) = self.shared.orders.get_order_info(fill.order_id) {
                let mut ex = info.last_exec;
                ex.perm_id = perm_id;
                ex.side = side_str.into();
                ex.shares = shares_f;
                ex.price = price_f;
                ex.order_id = fill.order_id;
                ex.cum_qty = filled_f;
                ex.avg_price = avg_f;
                let contract = if info.contract.con_id != 0 {
                    self.core.get_contract(info.contract.con_id, &self.shared).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                (contract, ex)
            } else {
                (Contract::default(), Execution {
                    side: side_str.into(),
                    shares: shares_f,
                    price: price_f,
                    order_id: fill.order_id,
                    cum_qty: filled_f,
                    avg_price: avg_f,
                    ..Default::default()
                })
            };
            self.core.apply_fill_exec(&mut exec, &fill_exec, fill.order_id);
            // A combo's report shows the combo or the leg (ibx#470).
            let mut c = c;
            ClientCore::apply_combo_exec(&fill_exec, &mut c, &mut exec);
            // The execution first, then openOrder and orderStatus, as the
            // reference (captured 30/09/2026 on a stock and a combo fill).
            // A live execution has no request: reqId -1 (ibx#474).
            wrapper.exec_details(-1, &c, &exec);

            // Store for req_executions replay; a commission report that came
            // first is sent now.
            if let Some(report) = self.core.push_execution(-1, c, exec, fill_exec.time_secs) {
                wrapper.commission_and_fees_report(&report);
            }

            // openOrder then orderStatus for every report of a known order
            // (ibx#473).
            let mut view = self.core.order_view(fill.order_id, &self.shared, status);
            ClientCore::report_client(&mut view, &fill_exec);
            let client_id = match &view {
                Some(view) => {
                    wrapper.open_order(fill.order_id, &view.contract, &view.order, &view.state);
                    view.client_id
                }
                None => 0,
            };
            let why_held = self.core.why_held(status, view.as_ref().map_or("", |v| v.order.order_type.as_str()), parent_id);
            wrapper.order_status(
                fill.order_id, status, filled_f, remaining_f,
                avg_f, perm_id, parent_id, price_f, client_id, &why_held, 0.0,
            );
            self.core.remember_report(fill.order_id, crate::client_core::OrderReport {
                view, status: status.into(), filled: filled_f, remaining: remaining_f, avg_fill_price: avg_f,
                perm_id, parent_id, last_fill_price: price_f, client_id, why_held,
            });
            self.core.record_last_fill_price(fill.order_id, price_f);

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, filled_f, remaining_f);
        }

        // Orders filled while the auth link was lost: no longer known to
        // the client, with no callback (ibx#251).
        for order_id in self.shared.orders.drain_forgotten_orders() {
            self.core.forget_order(order_id);
        }

        // Executions of orders this session does not track: stored for
        // req_executions, with no live callback (ibx#314).
        for (contract, mut exec, fill_exec) in self.shared.orders.drain_untracked_executions() {
            let order_id = exec.order_id;
            self.core.apply_fill_exec(&mut exec, &fill_exec, order_id);
            if let Some(report) = self.core.push_execution(-1, contract, exec, fill_exec.time_secs) {
                wrapper.commission_and_fees_report(&report);
            }
        }

        // Commission reports, sent once their execution is known (ibx#471),
        // after the order's openOrder and orderStatus once more (ibx#486).
        for report in self.shared.orders.drain_commission_reports() {
            if self.core.apply_commission(&report) {
                if let Some((order_id, last)) = self.core.report_of_commission(&report) {
                    Self::repeat_order_report(wrapper, order_id, &last);
                }
                wrapper.commission_and_fees_report(&report);
            }
        }

        // Order errors (refused before sending, warnings of a report) →
        // error, ahead of the status.
        for (order_id, code, msg) in self.shared.orders.drain_order_errors() {
            self.core.note_order_error(order_id, code);
            wrapper.error(order_id, code, &msg, "");
        }

        // The cancel of an order in the reference's API pending map: the
        // ApiCancelled status, permId 0, nothing filled, the prices unset
        // (ibx#487).
        for order_id in self.shared.orders.drain_api_cancelled() {
            let (remaining, client_id) = self.core.api_cancelled(order_id);
            wrapper.order_status(order_id, "ApiCancelled", 0.0, remaining, f64::MAX, 0, 0, f64::MAX, client_id, "", f64::MAX);
        }

        // Order updates → open_order + order_status for every report of a
        // known order; a cancel gives order_status only (ibx#473).
        let mut reported = std::collections::HashMap::new();
        for update in self.shared.orders.drain_order_updates() {
            self.report_order_update(wrapper, &update);
            reported.insert(update.order_id, update);
        }

        // A server reject (201) and a cancel (202) after the status of
        // their report, as the reference writes them; a reject then gives
        // the order and its status once more (ibx#486; every four-leg
        // recording of 26/09 to 02/10/2026).
        for (order_id, code, msg) in self.shared.orders.drain_order_notices() {
            wrapper.error(order_id, code, &msg, "");
            if let Some(update) = reported.get(&order_id).filter(|_| code == 201) {
                self.report_order_update(wrapper, update);
            }
        }

        // A server reject of a cancel or modify gives no callback, as the
        // reference: no error, no status; the order status that answers the
        // engine's status request sets the state (ibx#252).
        self.shared.orders.drain_cancel_rejects();

        // What-if → open_order(contract, order, OrderState) only, as the
        // reference answers a preview; a refused one then gets error 201
        // (ibx#462).
        for wi in self.shared.orders.drain_what_if_responses() {
            let state = ClientCore::what_if_order_state(&wi.state);
            let tracked = if wi.final_reply {
                self.core.take_what_if(wi.order_id)
            } else {
                self.core.peek_what_if(wi.order_id)
            };
            let (mut contract, mut order) = tracked.unwrap_or_else(|| (Contract::default(), ApiOrder::default()));
            // The order as the reference shows it (its unset values); a
            // combo shows its combo (ibx#470).
            crate::client_core::reported_unset_values(&mut order);
            ClientCore::apply_combo_view(wi.order_id, &mut contract, &mut order, &self.shared);
            // The preview's order carries the account and the client id,
            // as the reference's (ibx#486, b1_462_whatif of 02/10/2026).
            if order.account.is_empty() { order.account = self.account_id.clone(); }
            order.client_id = self.core.client_id.load(std::sync::atomic::Ordering::Relaxed) as i32;
            order.perm_id = wi.state.perm_id;
            wrapper.open_order(wi.order_id, &contract, &order, &state);
            if !wi.state.reject_reason.is_empty() {
                wrapper.error(wi.order_id, 201, &format!("Order rejected - reason:{}", wi.state.reject_reason), "");
            }
        }

        for _ in released {
            self.answer_open_orders(wrapper);
        }
    }

    /// openOrder (not for a cancel) and orderStatus of an order update
    /// (ibx#473).
    fn report_order_update(&self, wrapper: &mut impl Wrapper, update: &OrderUpdate) {
        let status = order_status_str(update.status);
        let filled_f = update.filled_qty_fixed as f64 / QTY_SCALE_F;
        let remaining_f = update.remaining_qty_fixed as f64 / QTY_SCALE_F;
        let view = self.core.order_view(update.order_id, &self.shared, status);
        let (last_fill_price, client_id) = view.as_ref().map(|v| (v.last_fill_price, v.client_id)).unwrap_or((0.0, 0));
        let why_held = self.core.why_held(status, view.as_ref().map_or("", |v| v.order.order_type.as_str()), update.parent_id);
        let view = view.filter(|_| status != "Cancelled");
        let report = crate::client_core::OrderReport {
            view, status: status.into(), filled: filled_f, remaining: remaining_f,
            avg_fill_price: update.avg_fill_price as f64 / PRICE_SCALE_F,
            perm_id: update.perm_id, parent_id: update.parent_id, last_fill_price, client_id, why_held,
        };
        Self::repeat_order_report(wrapper, update.order_id, &report);
        self.core.remember_report(update.order_id, report);
        self.core.update_order_status(update.order_id, status, filled_f, remaining_f);
    }

    /// openOrder (when the report has one) and orderStatus of a report.
    fn repeat_order_report(wrapper: &mut impl Wrapper, order_id: i64, r: &crate::client_core::OrderReport) {
        if let Some(v) = &r.view {
            wrapper.open_order(order_id, &v.contract, &v.order, &v.state);
        }
        wrapper.order_status(
            order_id, &r.status, r.filled, r.remaining, r.avg_fill_price,
            r.perm_id, r.parent_id, r.last_fill_price, r.client_id, &r.why_held, 0.0,
        );
    }

    // ── Quote Dispatch ──

    fn dispatch_quotes(&self, wrapper: &mut impl Wrapper) {
        // Requests that joined a subscription, and subscriptions the
        // server rejected (ibx#444, ibx#447).
        let (notices, commands) = self.core.take_md_rejects(&self.shared);
        Self::md_notices(wrapper, notices);
        for command in commands {
            let _ = self.control_tx.send(command);
        }
        // Requests that waited for a market data line (101) take the lines
        // set free (ibx#444).
        self.core.promote_waiting_md(&self.shared, &self.control_tx);

        // Regulatory snapshots that ended (ibx#446).
        let attrib = crate::api::types::TickAttrib::default();
        for (req_id, result) in self.core.poll_regulatory_snapshots(&self.shared, &self.control_tx) {
            use crate::control::regsnapshot::SnapshotTick;
            match result {
                Ok(ticks) => {
                    for t in ticks {
                        match t {
                            SnapshotTick::Price { tick_type, price } => wrapper.tick_price(req_id, tick_type, price, &attrib),
                            SnapshotTick::Size { tick_type, size } => wrapper.tick_size(req_id, tick_type, size),
                            SnapshotTick::Text { tick_type, value } => wrapper.tick_string(req_id, tick_type, &value),
                        }
                    }
                    wrapper.tick_snapshot_end(req_id);
                }
                Err((code, text)) => wrapper.error(req_id, code, &text, ""),
            }
        }

        // What the requests that joined a running subscription get at once
        // (ibx#444).
        Self::md_notices(wrapper, self.core.take_md_joins());

        // Request parameters, once per request (ibx#449), after the market
        // data type (ibx#446).
        for (req_id, mdt, min_tick, bbo_exchange, permissions) in self.core.take_tick_req_params(&self.shared) {
            if let Some(mdt) = mdt {
                wrapper.market_data_type(req_id, mdt);
            }
            wrapper.tick_req_params(req_id, min_tick, &bbo_exchange, permissions);
        }

        // The market data steps queued by the engine → tick callbacks, in
        // their order (ibx#446).
        let mut out = std::mem::take(&mut *self.core.md_out.lock().unwrap());
        self.core.poll_market_data(&self.shared, None, &mut out);
        let mut snapshot_done: Vec<i64> = Vec::new();
        let mut text = [0u8; 24];
        for item in out.drain(..) {
            match item {
                MdOut::Tick(req_id, tick) => match tick {
                    MdTick::Price { tick_type, value, can_auto_execute } => {
                        let attrib = crate::api::types::TickAttrib { can_auto_execute, ..Default::default() };
                        wrapper.tick_price(req_id, tick_type, value, &attrib);
                    }
                    MdTick::Size { tick_type, value } => wrapper.tick_size(req_id, tick_type, value),
                    MdTick::Text { tick_type, value } => wrapper.tick_string(req_id, tick_type, &value),
                    MdTick::Time { tick_type, secs } =>
                        wrapper.tick_string(req_id, tick_type, crate::client_core::epoch_text(secs, &mut text)),
                    MdTick::Generic { tick_type, value } => wrapper.tick_generic(req_id, tick_type, value),
                },
                MdOut::MarketDataType(req_id, mdt) => wrapper.market_data_type(req_id, mdt),
                MdOut::SnapshotEnd(req_id) => {
                    wrapper.tick_snapshot_end(req_id);
                    snapshot_done.push(req_id);
                }
            }
        }
        *self.core.md_out.lock().unwrap() = out;
        // A snapshot cancelled by the client before its end was read is
        // gone already.
        for req_id in snapshot_done {
            if self.core.req_to_instrument.lock().unwrap().contains_key(&req_id) {
                let _ = self.cancel_mkt_data(req_id);
            }
        }

        // Tick-by-tick requests that ended with an error (10189, 10190):
        // the engine already let them go (ibx#455).
        for (req_id, code, text) in self.shared.market.drain_tbt_errors() {
            wrapper.error(req_id, code as i64, &text, "");
            self.core.unregister_tbt(req_id);
        }

        // TBT trades → tick_by_tick_all_last, tickType 1 for Last and 2 for
        // AllLast, with the entry's attributes (ibx#404, ibx#455)
        for trade in self.shared.market.drain_tbt_trades() {
            let attrib_last = TickAttribLast { past_limit: trade.past_limit, unreported: trade.unreported };
            wrapper.tick_by_tick_all_last(
                trade.req_id, trade.tbt_type.api_tick_type(), trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F, trade.size as f64,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }

        // TBT quotes → tick_by_tick_bid_ask
        for quote in self.shared.market.drain_tbt_quotes() {
            let attrib_ba = TickAttribBidAsk { bid_past_low: quote.bid_past_low, ask_past_high: quote.ask_past_high };
            wrapper.tick_by_tick_bid_ask(
                quote.req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64, quote.ask_size as f64, &attrib_ba,
            );
        }

        // TBT midpoints → tick_by_tick_mid_point (ibx#404)
        for mid in self.shared.market.drain_tbt_mid_points() {
            wrapper.tick_by_tick_mid_point(mid.req_id, mid.timestamp as i64, mid.mid_point as f64 / PRICE_SCALE_F);
        }

        // Depth updates → update_mkt_depth / update_mkt_depth_l2, as the
        // book says (#451)
        for du in self.shared.market.drain_depth_updates() {
            if !du.l2 {
                wrapper.update_mkt_depth(du.req_id, du.position, du.operation, du.side, du.price, du.size);
            } else {
                wrapper.update_mkt_depth_l2(du.req_id, du.position, &du.market_maker, du.operation, du.side, du.price, du.size, du.is_smart_depth);
            }
        }
    }

    /// The callbacks of market data requests beside their ticks (ibx#444).
    fn md_notices(wrapper: &mut impl Wrapper, notices: Vec<crate::client_core::MdNotice>) {
        use crate::client_core::MdNotice;
        for notice in notices {
            match notice {
                MdNotice::MarketDataType { req_id, market_data_type } => wrapper.market_data_type(req_id, market_data_type),
                MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions } =>
                    wrapper.tick_req_params(req_id, min_tick, &bbo_exchange, permissions),
                MdNotice::Error { req_id, code, text } => wrapper.error(req_id, code, &text, ""),
                MdNotice::News { req_id, news } => wrapper.tick_news(
                    req_id, news.timestamp, &news.provider_code, &news.article_id, &news.headline, &news.extra_data,
                ),
            }
        }
    }

    // ── Historical / News / Account Dispatch ──

    fn dispatch_data(&self, wrapper: &mut impl Wrapper) {
        // News → tick_news, to each request of the contract whose news
        // key has the provider (ibx#444)
        for news in self.shared.market.drain_tick_news() {
            for req_id in self.core.route_tick_news(&news) {
                wrapper.tick_news(
                    req_id, news.timestamp,
                    &news.provider_code, &news.article_id, &news.headline, &news.extra_data,
                );
            }
        }

        // News bulletins → update_news_bulletin (popups always, every type
        // when subscribed, ibx#461)
        for b in self.core.bulletins_to_deliver(&self.shared) {
            wrapper.update_news_bulletin(b.msg_id as i64, b.msg_type, &b.message, &b.exchange);
        }

        // HMDS query errors → error (ibx#186). Drain before historical_data so a
        // local failure that also queued an empty terminal HistoricalResponse
        // fires wrapper.error first, then wrapper.historical_data_end. A
        // server-side rejection queues no terminal response (ibx#408).
        for (req_id, code, msg) in self.shared.reference.drain_historical_errors() {
            wrapper.error(req_id, code as i64, &msg, "");
        }

        // Historical data → historical_data + historical_data_end
        for (req_id, response) in self.shared.reference.drain_historical_data() {
            for bar in &response.bars {
                let bd = BarData {
                    date: bar.time.clone(),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                    wap: bar.wap,
                    bar_count: bar.count,
                    timezone: response.timezone.clone(),
                };
                wrapper.historical_data(req_id, &bd);
            }
            if response.is_complete {
                wrapper.historical_data_end(req_id, &response.start, &response.end);
            }
        }

        // keepUpToDate: the whole current bar each time (ibx#429).
        for (req_id, bar) in self.shared.reference.drain_historical_updates() {
            let bd = BarData {
                date: bar.time,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
                wap: bar.wap,
                bar_count: bar.count,
                timezone: String::new(),
            };
            wrapper.historical_data_update(req_id, &bd);
        }

        // Head timestamps → head_timestamp
        for (req_id, response) in self.shared.reference.drain_head_timestamps() {
            wrapper.head_timestamp(req_id, &response.head_timestamp);
        }

        // Smart components that waited for their exchange map (ibx#441).
        for (req_id, answer) in self.core.take_smart_components(&self.shared) {
            match answer {
                Ok(components) => wrapper.smart_components(req_id, &components),
                Err((code, msg)) => wrapper.error(req_id, code, &msg, ""),
            }
        }

        // Contract details → contract_details + contract_details_end
        for (req_id, def) in self.shared.reference.drain_contract_details() {
            let details = ContractDetails::from_definition(&def);
            // A bond row is a bond contract details message (ibx#438).
            if def.is_bond() {
                wrapper.bond_contract_details(req_id, &details);
            } else {
                wrapper.contract_details(req_id, &details);
            }
        }
        for req_id in self.shared.reference.drain_contract_details_end() {
            wrapper.contract_details_end(req_id);
        }

        // Matching symbols → symbol_samples
        for (req_id, matches) in self.shared.reference.drain_matching_symbols() {
            let descriptions: Vec<ContractDescription> = matches.iter().map(|m| {
                ContractDescription {
                    con_id: m.con_id,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.clone(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                    description: m.description.clone(),
                    issuer_id: m.issuer_id.clone(),
                }
            }).collect();
            wrapper.symbol_samples(req_id, &descriptions);
        }

        // Option chains → one security_definition_option_parameter per row,
        // then the end (ibx#440).
        for (req_id, rows) in self.shared.reference.drain_option_chains() {
            for r in &rows {
                wrapper.security_definition_option_parameter(req_id, &r.exchange, r.underlying_con_id,
                    &r.trading_class, &r.multiplier, &r.expirations, &r.strikes);
            }
            wrapper.security_definition_option_parameter_end(req_id);
        }

        // Scanner params
        for xml in self.shared.reference.drain_scanner_params() {
            wrapper.scanner_parameters(&xml);
        }

        // Scanner data. Cache is populated by the engine before dispatch
        // (see `CcpState::start_scanner_enrichment`), so cold con_ids that
        // arrived in `<ScanResponse>` have already been resolved via 35=d.
        // The Some-arm fills the rich fields; the fallback covers deadline-
        // flushed partials where a secdef reply never arrived.
        for (req_id, result) in self.shared.reference.drain_scanner_data() {
            for (rank, entry) in result.entries.iter().enumerate() {
                let mut contract = Contract { con_id: entry.con_id, ..Default::default() };
                if let Some(ac) = self.core.get_contract(entry.con_id, &self.shared) {
                    contract.symbol = ac.symbol;
                    contract.sec_type = ac.sec_type;
                    contract.exchange = ac.exchange;
                    contract.currency = ac.currency;
                    contract.local_symbol = ac.local_symbol;
                    contract.primary_exchange = ac.primary_exchange;
                    contract.trading_class = ac.trading_class;
                }
                let details = ContractDetails { contract, ..Default::default() };
                wrapper.scanner_data(req_id, rank as i32, &details,
                    &entry.distance, &entry.benchmark, &entry.projection, &entry.legs);
            }
            wrapper.scanner_data_end(req_id);
        }

        // Historical news
        for (req_id, headlines, has_more) in self.shared.reference.drain_historical_news() {
            for h in &headlines {
                wrapper.historical_news(req_id, &h.time, &h.provider_code, &h.article_id, &h.headline);
            }
            wrapper.historical_news_end(req_id, has_more);
        }

        // News articles
        for (req_id, article_type, text) in self.shared.reference.drain_news_articles() {
            wrapper.news_article(req_id, article_type, &text);
        }

        // Fundamental data
        // reqMktDepthExchanges, answered from the routing table (#453).
        if let Some(descriptions) = self.shared.reference.drain_depth_exchanges() {
            wrapper.mkt_depth_exchanges(&descriptions);
        }
        for a in self.shared.reference.drain_option_computations() {
            wrapper.tick_option_computation(
                a.req_id, a.tick_type, a.tick_attrib, a.implied_vol, a.delta, a.opt_price,
                a.pv_dividend, a.gamma, a.vega, a.theta, a.und_price,
            );
        }
        for (req_id, data) in self.shared.reference.drain_fundamental_data() {
            wrapper.fundamental_data(req_id, &data);
        }

        // Histogram data
        for (req_id, entries) in self.shared.reference.drain_histogram_data() {
            let items: Vec<(f64, i64)> = entries.iter().map(|e| (e.price, e.count)).collect();
            wrapper.histogram_data(req_id, &items);
        }

        // Historical ticks — route to the variant-specific callback (iso ibapi).
        for (req_id, data, _query_id, done) in self.shared.reference.drain_historical_ticks() {
            match &data {
                HistoricalTickData::Midpoint(_) => wrapper.historical_ticks(req_id, &data, done),
                HistoricalTickData::Last(_) => wrapper.historical_ticks_last(req_id, &data, done),
                HistoricalTickData::BidAsk(_) => wrapper.historical_ticks_bid_ask(req_id, &data, done),
            }
        }

        // Real-time bars
        for (req_id, bar) in self.shared.market.drain_real_time_bars() {
            wrapper.real_time_bar(
                req_id, bar.timestamp as i64,
                bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count,
            );
        }

        // Historical schedules
        for (req_id, schedule) in self.shared.reference.drain_historical_schedules() {
            let sessions: Vec<(String, String, String)> = schedule.sessions.iter()
                .map(|s| (s.ref_date.clone(), s.open_time.clone(), s.close_time.clone()))
                .collect();
            wrapper.historical_schedule(
                req_id, &schedule.start_date_time, &schedule.end_date_time,
                &schedule.timezone, &sessions,
            );
        }

        // PnL → pnl callback (change-detected via ClientCore)
        // Quotes the P&L needs, subscribed by ibx itself when the caller has
        // none; they never reach the tick callbacks.
        self.core.maintain_pnl_quotes(&self.shared, &self.control_tx);
        for update in self.core.poll_pnl(&self.shared) {
            wrapper.pnl(update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl);
        }

        // PnL single → pnl_single callback (via ClientCore)
        for update in self.core.poll_pnl_single(&self.shared) {
            wrapper.pnl_single(update.req_id, update.pos, update.daily_pnl, update.unrealized_pnl, update.realized_pnl, update.value);
        }

        // Positions of a running req_positions (ibx#477).
        self.dispatch_positions(wrapper);
        // Multi-account requests (ibx#476).
        self.dispatch_multi(wrapper);

        // Account updates (ibx#475): values, portfolio rows each followed by
        // the account time, the time after the batch, and for the first image
        // the end, once per subscription.
        if let Some(batch) = self.core.prepare_account_updates(&self.shared) {
            for field in &batch.fields {
                wrapper.update_account_value(&field.key, &field.value, &field.currency, &self.account_id);
            }
            let portfolio = self.core.prepare_portfolio_updates(&self.shared);
            for entry in &portfolio {
                let ac = self.core.position_contract(entry.con_id, &self.shared);
                let c = Contract {
                    con_id: ac.con_id, symbol: ac.symbol, sec_type: ac.sec_type,
                    exchange: ac.exchange, primary_exchange: ac.primary_exchange,
                    currency: ac.currency, local_symbol: ac.local_symbol,
                    trading_class: ac.trading_class, multiplier: ac.multiplier,
                    ..Default::default()
                };
                wrapper.update_portfolio(
                    &c, entry.position, entry.market_price, entry.market_value,
                    entry.avg_cost, entry.unrealized_pnl, entry.realized_pnl, &self.account_id,
                );
                wrapper.update_account_time(&batch.time);
            }
            if !batch.fields.is_empty() || !portfolio.is_empty() {
                wrapper.update_account_time(&batch.time);
            }
            if batch.download_end {
                wrapper.account_download_end(&self.account_id);
            }
        }

        // Account summary rows as the server sends them; the end at each of
        // its end markers (ibx#479).
        for batch in self.core.prepare_account_summary(&self.shared) {
            let account = batch.account.as_deref().unwrap_or(&self.account_id);
            for row in &batch.rows {
                wrapper.account_summary(batch.req_id, account, &row.key, &row.value, &row.currency);
            }
            if batch.end {
                wrapper.account_summary_end(batch.req_id);
            }
        }
    }
}
