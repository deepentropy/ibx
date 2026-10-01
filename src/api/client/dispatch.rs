//! Event dispatch: drains SharedState queues and fires Wrapper callbacks.

use crate::api::types::{
    BarData, ContractDetails, ContractDescription, Execution,
    Order as ApiOrder, OrderState, TickAttribLast, TickAttribBidAsk, PRICE_SCALE_F, QTY_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::client_core::order_status_str;
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
            // openOrder then orderStatus for every report of a known order
            // (ibx#473).
            let client_id = match self.core.order_view(fill.order_id, &self.shared, status) {
                Some(view) => {
                    wrapper.open_order(fill.order_id, &view.contract, &view.order, &view.state);
                    view.client_id
                }
                None => 0,
            };
            wrapper.order_status(
                fill.order_id, status, filled_f, remaining_f,
                avg_f, perm_id, parent_id, price_f, client_id, "", 0.0,
            );
            self.core.record_last_fill_price(fill.order_id, price_f);

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
            // A live execution has no request: reqId -1 (ibx#474).
            wrapper.exec_details(-1, &c, &exec);

            // Store for req_executions replay; a commission report that came
            // first is sent now.
            if let Some(report) = self.core.push_execution(-1, c, exec, fill_exec.time_secs) {
                wrapper.commission_and_fees_report(&report);
            }

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, filled_f, remaining_f);
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

        // Commission reports, sent once their execution is known (ibx#471).
        for report in self.shared.orders.drain_commission_reports() {
            if self.core.apply_commission(&report) {
                wrapper.commission_and_fees_report(&report);
            }
        }

        // Order errors (refused before sending, or rejected by the server)
        // → error, ahead of the status: the reference reports a server
        // reject as error 201 before the Inactive status (ibx#250).
        for (order_id, code, msg) in self.shared.orders.drain_order_errors() {
            wrapper.error(order_id, code, &msg, "");
        }

        // Order updates → open_order + order_status for every report of a
        // known order; a cancel gives order_status only (ibx#473).
        for update in self.shared.orders.drain_order_updates() {
            let status = order_status_str(update.status);
            let filled_f = update.filled_qty_fixed as f64 / QTY_SCALE_F;
            let remaining_f = update.remaining_qty_fixed as f64 / QTY_SCALE_F;
            let view = self.core.order_view(update.order_id, &self.shared, status);
            if let Some(v) = view.as_ref().filter(|_| status != "Cancelled") {
                wrapper.open_order(update.order_id, &v.contract, &v.order, &v.state);
            }
            let (last_fill_price, client_id) = view.map(|v| (v.last_fill_price, v.client_id)).unwrap_or((0.0, 0));
            wrapper.order_status(
                update.order_id, status, filled_f,
                remaining_f, update.avg_fill_price as f64 / PRICE_SCALE_F,
                update.perm_id, update.parent_id, last_fill_price, client_id, "", 0.0,
            );
            self.core.update_order_status(update.order_id, status, filled_f, remaining_f);
        }

        // A server reject of a cancel or modify gives no callback, as the
        // reference: no error, no status; the order status that answers the
        // engine's status request sets the state (ibx#252).
        self.shared.orders.drain_cancel_rejects();

        // What-if → open_order(contract, order, OrderState) only, as the
        // reference answers a preview (ibx#462).
        for wi in self.shared.orders.drain_what_if_responses() {
            let fmt = |p: Price| format!("{:.2}", p as f64 / PRICE_SCALE_F);
            let state = OrderState {
                status: "PreSubmitted".into(),
                init_margin_before: fmt(wi.init_margin_before),
                maint_margin_before: fmt(wi.maint_margin_before),
                equity_with_loan_before: fmt(wi.equity_with_loan_before),
                init_margin_change: fmt(wi.init_margin_after - wi.init_margin_before),
                maint_margin_change: fmt(wi.maint_margin_after - wi.maint_margin_before),
                equity_with_loan_change: fmt(wi.equity_with_loan_after - wi.equity_with_loan_before),
                init_margin_after: fmt(wi.init_margin_after),
                maint_margin_after: fmt(wi.maint_margin_after),
                equity_with_loan_after: fmt(wi.equity_with_loan_after),
                commission_and_fees: wi.commission as f64 / PRICE_SCALE_F,
                ..Default::default()
            };
            let (contract, order) = self.core.take_what_if(wi.order_id)
                .unwrap_or_else(|| (Contract::default(), ApiOrder::default()));
            wrapper.open_order(wi.order_id, &contract, &order, &state);
        }

        for _ in released {
            self.answer_open_orders(wrapper);
        }
    }

    // ── Quote Dispatch ──

    fn dispatch_quotes(&self, wrapper: &mut impl Wrapper) {
        // Subscriptions the server rejected (ibx#444, ibx#447).
        for reject in self.shared.market.drain_md_rejects() {
            let instrument = reject.instrument();
            let req_id = self.core.req_id_for_instrument(instrument);
            if req_id < 0 { continue; }
            let (code, text, gone) = crate::client_core::ClientCore::md_reject_error(&reject);
            if !gone {
                self.core.set_delayed(req_id);
                wrapper.market_data_type(req_id, 3);
            }
            wrapper.error(req_id, code, text, "");
            if gone {
                let (instrument, needs_news) = self.core.unregister_mkt_data(req_id);
                if let Some(instrument) = instrument {
                    let _ = self.control_tx.send(ControlCommand::Unsubscribe { instrument });
                    if needs_news { let _ = self.control_tx.send(ControlCommand::UnsubscribeNews { instrument }); }
                }
            }
        }

        // Request parameters, once per request (ibx#449).
        for (req_id, min_tick, bbo_exchange, permissions) in self.core.take_tick_req_params(&self.shared) {
            wrapper.tick_req_params(req_id, min_tick, &bbo_exchange, permissions);
        }

        // Quote polling → tick_price / tick_size (via ClientCore)
        let instruments = self.core.snapshot_instruments();
        let attrib = crate::api::types::TickAttrib::default();
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let result = self.core.poll_instrument_ticks(&self.shared, iid, req_id);
            // Fire market_data_type once per subscription on first tick delivery
            if let Some(mdt) = self.core.check_mdt_needed(req_id, result.delivered) {
                wrapper.market_data_type(req_id, mdt);
            }
            for tick in &result.ticks {
                if tick.is_price {
                    wrapper.tick_price(tick.req_id, tick.tick_type, tick.value, &attrib);
                } else {
                    wrapper.tick_size(tick.req_id, tick.tick_type, tick.value);
                }
            }
            for st in &result.string_ticks {
                wrapper.tick_string(st.req_id, st.tick_type, &st.value);
            }
            if let Some(ts) = &result.timestamp {
                let ts_secs = ts.timestamp_ns / 1_000_000_000;
                wrapper.tick_string(ts.req_id, 45, &ts_secs.to_string());
            }
            if self.core.check_snapshot_done(req_id, result.delivered) {
                wrapper.tick_snapshot_end(req_id);
                snapshot_done.push(req_id);
            }
        }
        for req_id in snapshot_done {
            let _ = self.cancel_mkt_data(req_id);
        }

        // Tick-by-tick requests the server refused: 10189 with its text,
        // and the request ends (ibx#455).
        for (instrument, tbt_type, text) in self.shared.market.drain_tbt_errors() {
            if let Some(req_id) = self.core.tbt_req_of_type(instrument, tbt_type) {
                wrapper.error(req_id, 10189, &format!("Failed to request tick-by-tick data.{}", text), "");
                if let Some(instrument) = self.core.unregister_tbt(req_id) {
                    let _ = self.control_tx.send(ControlCommand::UnsubscribeTbt { instrument });
                }
            }
        }

        // TBT trades → tick_by_tick_all_last, tickType 1 for Last and 2 for
        // AllLast (ibx#455)
        for trade in self.shared.market.drain_tbt_trades() {
            let (req_id, tick_type) = self.core.tbt_req_for(trade.instrument, true)
                .map_or((-1, 1), |(r, t)| (r, t.api_tick_type()));
            let attrib_last = TickAttribLast::default();
            wrapper.tick_by_tick_all_last(
                req_id, tick_type, trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F, trade.size as f64,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }

        // TBT quotes → tick_by_tick_bid_ask
        for quote in self.shared.market.drain_tbt_quotes() {
            let req_id = self.core.tbt_req_for(quote.instrument, false).map_or(-1, |(r, _)| r);
            let attrib_ba = TickAttribBidAsk::default();
            wrapper.tick_by_tick_bid_ask(
                req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64, quote.ask_size as f64, &attrib_ba,
            );
        }

        // Depth updates → update_mkt_depth / update_mkt_depth_l2
        for du in self.shared.market.drain_depth_updates() {
            if du.market_maker.is_empty() {
                wrapper.update_mkt_depth(du.req_id, du.position, du.operation, du.side, du.price, du.size);
            } else {
                wrapper.update_mkt_depth_l2(du.req_id, du.position, &du.market_maker, du.operation, du.side, du.price, du.size, du.is_smart_depth);
            }
        }
    }

    // ── Historical / News / Account Dispatch ──

    fn dispatch_data(&self, wrapper: &mut impl Wrapper) {
        // News → tick_news
        for news in self.shared.market.drain_tick_news() {
            let req_id = self.core.req_id_for_instrument(news.instrument);
            wrapper.tick_news(
                req_id, news.timestamp as i64,
                &news.provider_code, &news.article_id, &news.headline, "",
            );
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
                    bar_count: bar.count as i32,
                    timezone: response.timezone.clone(),
                };
                wrapper.historical_data(req_id, &bd);
            }
            if response.is_complete {
                wrapper.historical_data_end(req_id, "", "");
            }
        }

        // Head timestamps → head_timestamp
        for (req_id, response) in self.shared.reference.drain_head_timestamps() {
            wrapper.head_timestamp(req_id, &response.head_timestamp);
        }

        // Contract details → contract_details + contract_details_end
        for (req_id, def) in self.shared.reference.drain_contract_details() {
            let details = ContractDetails::from_definition(&def);
            wrapper.contract_details(req_id, &details);
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
            for row in &batch.rows {
                wrapper.account_summary(batch.req_id, &self.account_id, &row.key, &row.value, &row.currency);
            }
            if batch.end {
                wrapper.account_summary_end(batch.req_id);
            }
        }
    }
}
