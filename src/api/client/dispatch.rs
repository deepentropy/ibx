//! Event dispatch: drains SharedState queues and fires Wrapper callbacks.

use std::collections::HashMap;

use crate::api::types::{
    BarData, ContractDetails, ContractDescription, Execution,
    Order as ApiOrder, TickAttribLast, TickAttribBidAsk, PRICE_SCALE_F, QTY_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::bridge::{FillExec, OrderReport};
use crate::client_core::inbox::{run, Kind, Run};
use crate::client_core::{order_status_str, ClientCore, MdOut, MdTick};
use crate::control::contracts::{ContractDefinition, SymbolMatch};
use crate::control::histogram::HistogramEntry;
use crate::control::historical::{HistoricalBar, HistoricalResponse};
use crate::control::news::NewsHeadline;
use crate::control::scanner::ScannerResult;
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Message Processing ──

    /// Drain all SharedState queues and dispatch to the Wrapper.
    /// Call this in a loop — it is the Rust equivalent of C++ `EReader::processMsgs()`.
    ///
    /// The answers of the queues come in the order they were written, as
    /// the reference gives the messages of its one connection (ibx#532);
    /// what is read from the session's state (quotes, P&L, account,
    /// positions) at its place among them (ibx#662), and what changed last
    /// after them; then the end of the session.
    pub fn process_msgs(&self, wrapper: &mut impl Wrapper) {
        // Open-order requests held while the auth link was lost: taken
        // before the order answers and answered after them, so the answer
        // has the replayed statuses (ibx#251).
        let released = self.core.released_open_orders(&self.shared);
        let written = self.dispatch_answers(wrapper);
        // The working orders of this client the logon replay listed are
        // followed by the end of the list, as the reference's connect
        // burst; no end when it listed none (ibx#487).
        if self.shared.orders.take_login_orders_end()
            && !self.core.open_orders_listing(&self.shared, crate::client_core::OpenOrdersRequest::Open).is_empty()
        {
            wrapper.open_order_end();
        }
        // A server reject of a cancel or modify gives no callback, as the
        // reference: no error, no status; the order status that answers the
        // engine's status request sets the state (ibx#252).
        self.shared.orders.drain_cancel_rejects();
        for request in released {
            self.answer_open_orders(wrapper, request);
        }
        self.dispatch_state(wrapper, written);
        self.dispatch_connection(wrapper);
    }

    /// What is read from the session's state, for the answers counted
    /// below `until` (ibx#662): the market data steps written before them,
    /// and the rest as it is now.
    fn dispatch_state(&self, wrapper: &mut impl Wrapper, until: u64) {
        let limit = self.core.state_read(&self.shared, until);
        self.dispatch_quotes(wrapper, limit);
        self.dispatch_data(wrapper);
    }

    /// The answers of the queues and the answers the client made itself,
    /// run by run in the order they were written (ibx#532), with what is
    /// read from the state at its place (ibx#662). Returns the count below
    /// which every answer was in the pass.
    fn dispatch_answers(&self, wrapper: &mut impl Wrapper) -> u64 {
        // Out of their locks while the callbacks run: a callback may call
        // the client again.
        let mut local = std::mem::take(&mut *self.local.lock().unwrap());
        let mut inbox = std::mem::take(&mut *self.core.inbox.lock().unwrap());
        let written = inbox.take(&self.shared);
        self.core.state_pass(&self.shared);
        // The last status of each order given in this pass: a reject that
        // follows it gives it again (ibx#486).
        let mut reported: HashMap<OrderId, OrderUpdate> = HashMap::new();
        while let Some(next) = inbox.next_with_state(written, local.front().map(|(at, _)| *at), self.core.state_first(&self.shared)) {
            let (kind, until) = match next {
                Run::State(until) => {
                    self.dispatch_state(wrapper, until);
                    continue;
                }
                Run::Client(until) => {
                    while local.front().is_some_and(|(at, _)| *at < until) {
                        let (_, answer) = local.pop_front().unwrap();
                        answer(self, wrapper);
                    }
                    continue;
                }
                Run::Queue(kind, until) => (kind, until),
            };
            let ib = &mut inbox;
            match kind {
                Kind::fills => self.deliver_fills(wrapper, run(&mut ib.fills, until)),
                Kind::untracked_executions => self.deliver_untracked_executions(wrapper, run(&mut ib.untracked_executions, until)),
                Kind::forgotten_orders => {
                    // Orders filled while the auth link was lost
                    // (ibx#251), or refused at their contract's lookup
                    // (ibx#738): no longer known to the client, with no
                    // callback.
                    for order_id in run(&mut ib.forgotten_orders, until) {
                        self.core.forget_order(order_id);
                    }
                }
                Kind::commission_reports => self.deliver_commission_reports(wrapper, run(&mut ib.commission_reports, until)),
                Kind::order_errors => {
                    // Order errors (refused before sending, warnings of a
                    // report) → error, ahead of the status.
                    for (order_id, code, msg) in run(&mut ib.order_errors, until) {
                        if self.core.md_limit_held(order_id, code) {
                            continue;
                        }
                        self.core.note_order_error(order_id, code, &msg);
                        wrapper.error(self.shared.orders.api_order_id(order_id), code, &msg, "");
                    }
                }
                Kind::order_reports => self.deliver_order_reports(wrapper, run(&mut ib.order_reports, until), &mut reported),
                Kind::what_if_responses => self.deliver_what_ifs(wrapper, run(&mut ib.what_if_responses, until)),
                // The server's finished orders of the day, then the end
                // (ibx#695).
                Kind::completed_orders => {
                    for row in run(&mut ib.completed_orders, until) {
                        match row {
                            Some(row) => {
                                let (contract, mut order, state) = *row;
                                crate::client_core::reported_unset_values(&mut order);
                                wrapper.completed_order(&contract, &order, &state);
                            }
                            None => wrapper.completed_orders_end(),
                        }
                    }
                }
                Kind::historical_ticks => self.deliver_historical_ticks(wrapper, run(&mut ib.historical_ticks, until)),
                Kind::tbt_errors => {
                    // Tick-by-tick requests that ended with an error
                    // (10189, 10190): the engine already let them go
                    // (ibx#455).
                    for (req_id, code, text) in run(&mut ib.tbt_errors, until) {
                        wrapper.error(req_id, code as i64, &text, "");
                        // A duplicate leaves the live request of that id
                        // (ibx#720).
                        if code != 102 {
                            self.core.unregister_tbt(req_id);
                        }
                    }
                }
                Kind::tbt_trades => self.deliver_tbt_trades(wrapper, run(&mut ib.tbt_trades, until)),
                Kind::tbt_quotes => self.deliver_tbt_quotes(wrapper, run(&mut ib.tbt_quotes, until)),
                Kind::tbt_mid_points => {
                    // TBT midpoints → tick_by_tick_mid_point (ibx#404)
                    for mid in run(&mut ib.tbt_mid_points, until) {
                        wrapper.tick_by_tick_mid_point(mid.req_id, mid.timestamp as i64, mid.mid_point as f64 / PRICE_SCALE_F);
                    }
                }
                Kind::depth_updates => self.deliver_depth(wrapper, run(&mut ib.depth_updates, until)),
                Kind::tick_news => self.deliver_tick_news(wrapper, run(&mut ib.tick_news, until)),
                Kind::historical_errors => {
                    // HMDS query errors → error (ibx#186): written before the
                    // empty terminal response of a local failure, so the
                    // error comes before historical_data_end. A server-side
                    // rejection queues no terminal response (ibx#408).
                    for (req_id, code, msg) in run(&mut ib.historical_errors, until) {
                        wrapper.error(req_id, code as i64, &msg, "");
                    }
                }
                Kind::historical_data => self.deliver_historical_data(wrapper, run(&mut ib.historical_data, until)),
                Kind::historical_updates => self.deliver_historical_updates(wrapper, run(&mut ib.historical_updates, until)),
                Kind::head_timestamps => {
                    for (req_id, response) in run(&mut ib.head_timestamps, until) {
                        wrapper.head_timestamp(req_id, &response.head_timestamp);
                    }
                }
                Kind::contract_details => self.deliver_contract_details(wrapper, run(&mut ib.contract_details, until)),
                Kind::contract_details_end => {
                    for req_id in run(&mut ib.contract_details_end, until) {
                        wrapper.contract_details_end(req_id);
                    }
                }
                Kind::matching_symbols => self.deliver_matching_symbols(wrapper, run(&mut ib.matching_symbols, until)),
                Kind::option_chains => self.deliver_option_chains(wrapper, run(&mut ib.option_chains, until)),
                Kind::scanner_data => self.deliver_scanner_data(wrapper, run(&mut ib.scanner_data, until)),
                Kind::historical_news => self.deliver_historical_news(wrapper, run(&mut ib.historical_news, until)),
                Kind::news_articles => {
                    for (req_id, article_type, text) in run(&mut ib.news_articles, until) {
                        wrapper.news_article(req_id, article_type, &text);
                    }
                }
                Kind::option_computations => {
                    for a in run(&mut ib.option_computations, until) {
                        wrapper.tick_option_computation(
                            a.req_id, a.tick_type, a.tick_attrib, a.implied_vol, a.delta, a.opt_price,
                            a.pv_dividend, a.gamma, a.vega, a.theta, a.und_price,
                        );
                    }
                }
                Kind::fundamental_data => {
                    for (req_id, data) in run(&mut ib.fundamental_data, until) {
                        wrapper.fundamental_data(req_id, &data);
                    }
                }
                Kind::histogram_data => self.deliver_histograms(wrapper, run(&mut ib.histogram_data, until)),
                Kind::real_time_bars => {
                    for (req_id, bar) in run(&mut ib.real_time_bars, until) {
                        wrapper.real_time_bar(
                            req_id, bar.timestamp as i64,
                            bar.open, bar.high, bar.low, bar.close,
                            bar.volume, bar.wap, bar.count,
                        );
                    }
                }
                Kind::historical_schedules => self.deliver_schedules(wrapper, run(&mut ib.historical_schedules, until)),
                // The calendar data answers (ibx#443).
                Kind::wsh_answers => {
                    for answer in run(&mut ib.wsh_answers, until) {
                        match answer {
                            crate::client_core::WshOut::Meta(req_id, json) => wrapper.wsh_meta_data(req_id, &json),
                            crate::client_core::WshOut::Event(req_id, json) => wrapper.wsh_event_data(req_id, &json),
                            crate::client_core::WshOut::Error(req_id, code, text) => wrapper.error(req_id, code, &text, ""),
                        }
                    }
                }
                Kind::connection_notices => {
                    // Link lost / restored and farm status (ibx#399):
                    // errors with id -1. The session stays open across a
                    // lost link.
                    for (code, msg) in run(&mut ib.connection_notices, until) {
                        wrapper.error(-1, code, &msg, "");
                    }
                }
            }
        }
        // What waits for the next pass, before what was made meanwhile.
        self.core.inbox.lock().unwrap().merge(inbox);
        let mut waiting = self.local.lock().unwrap();
        for answer in local.into_iter().rev() {
            waiting.push_front(answer);
        }
        written
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

    /// Fills → order_status + exec_details. The commission report comes
    /// later, from its own server frame (ibx#471).
    fn deliver_fills(&self, wrapper: &mut impl Wrapper, fills: impl IntoIterator<Item = (Fill, FillExec)>) {
        for (fill, fill_exec) in fills {
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
            // The order id the reference shows for the order.
            let shown = self.shared.orders.api_order_id(fill.order_id);
            exec.order_id = shown;
            // The contract in full, on the exchange of the execution (ibx#543).
            let c = self.core.execution_contract(&self.shared, fill.order_id, c, &exec.exchange);
            // Its time in the zone of its contract.
            self.core.execution_time_in_zone(&self.shared, c.con_id, &mut exec.time);
            // A combo's report shows the combo or the leg (ibx#470).
            let mut c = c;
            ClientCore::apply_combo_exec(&fill_exec, &mut c, &mut exec);
            // Finished before its callbacks: a modify made from them gets
            // 104 (ibx#544).
            self.core.finish_before_report(fill.order_id, status, remaining_f);
            // The reqPositions row of the fill, before its execution
            // (ibx#707).
            if let Some((con_id, qty, cost)) = fill_exec.position_after {
                let rows = self.core.position_event_rows(&self.shared, con_id, Some((qty, cost)), 1);
                self.position_rows(wrapper, &rows);
                self.position_multi_event(wrapper, con_id, Some((qty, cost)));
            }
            // The execution first, then openOrder and orderStatus, as the
            // reference (captured 30/09/2026 on a stock and a combo fill).
            // A live execution has no request: reqId -1 (ibx#474).
            wrapper.exec_details(-1, &c, &exec);

            // Store for req_executions replay; a commission report that came
            // first is sent now.
            if let Some(report) = self.core.push_execution(-1, c, exec, fill_exec.time_secs) {
                wrapper.commission_and_fees_report(&report);
                self.commission_position_rows(wrapper, &report);
            }

            // openOrder then orderStatus for every report of a known order
            // (ibx#473).
            let mut view = self.core.order_view(fill.order_id, &self.shared, status);
            ClientCore::report_client(&mut view, &fill_exec);
            ClientCore::report_filled(&mut view, filled_f);
            let client_id = match &view {
                Some(view) => {
                    wrapper.open_order(shown, &view.contract, &view.order, &view.state);
                    view.client_id
                }
                None => 0,
            };
            let why_held = self.core.why_held(status, view.as_ref().map_or("", |v| v.order.order_type.as_str()), parent_id);
            wrapper.order_status(
                shown, status, filled_f, remaining_f,
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
    }

    /// Executions of orders this session does not track: stored for
    /// req_executions, with no live callback (ibx#314).
    fn deliver_untracked_executions(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (Contract, Execution, FillExec)>) {
        for (contract, mut exec, fill_exec) in items {
            let order_id = exec.order_id;
            self.core.apply_fill_exec(&mut exec, &fill_exec, order_id);
            // A live fill moves the position: its reqPositions row, also for
            // an order of another client (ibx#707; the capture of
            // 25/09/2026). The day's executions given again at the logon
            // move nothing.
            if !fill_exec.replayed && let Some((con_id, qty, cost)) = fill_exec.position_after {
                let rows = self.core.position_event_rows(&self.shared, con_id, Some((qty, cost)), 1);
                self.position_rows(wrapper, &rows);
                self.position_multi_event(wrapper, con_id, Some((qty, cost)));
                if fill_exec.other_client {
                    self.core.note_observed_fill(&exec.exec_id);
                }
            }
            // An execution of another client's order: nothing for this one.
            if fill_exec.other_client || fill_exec.replayed {
                self.core.push_silent_execution(contract, exec, fill_exec.time_secs);
                continue;
            }
            if let Some(report) = self.core.push_execution(-1, contract, exec, fill_exec.time_secs) {
                wrapper.commission_and_fees_report(&report);
            }
        }
    }

    /// Commission reports, sent once their execution is known (ibx#471),
    /// after the order's openOrder and orderStatus once more (ibx#486).
    fn deliver_commission_reports(&self, wrapper: &mut impl Wrapper, reports: impl IntoIterator<Item = crate::api::types::CommissionAndFeesReport>) {
        for report in reports {
            if self.core.apply_commission(&report) {
                if let Some((order_id, last)) = self.core.report_of_commission(&report) {
                    self.repeat_order_report(wrapper, order_id, &last);
                }
                wrapper.commission_and_fees_report(&report);
                self.commission_position_rows(wrapper, &report);
            } else if self.core.is_observed_fill(&report) {
                // Another client's commission: its position rows only.
                self.commission_position_rows(wrapper, &report);
            }
        }
    }

    /// Order updates → open_order + order_status for every report of a
    /// known order; a cancel gives order_status only (ibx#473).
    ///
    /// A server reject (201) and a cancel (202) after the status of
    /// their report, as the reference writes them; a reject then gives
    /// the order and its status once more (ibx#486; every four-leg
    /// recording of 26/09 to 02/10/2026). The 201 of an order the
    /// redirect precaution discarded (Cancelled) does not.
    fn deliver_order_reports(&self, wrapper: &mut impl Wrapper, reports: impl IntoIterator<Item = OrderReport>, reported: &mut HashMap<OrderId, OrderUpdate>) {
        for report in reports {
            match report {
                OrderReport::Update(update) => {
                    self.report_order_update(wrapper, &update);
                    reported.insert(update.order_id, update);
                }
                OrderReport::Notice(order_id, code, msg) => {
                    wrapper.error(self.shared.orders.api_order_id(order_id), code, &msg, "");
                    if let Some(update) = reported.get(&order_id).filter(|u| code == 201 && u.status != OrderStatus::Cancelled) {
                        self.report_order_update(wrapper, update);
                    }
                }
            }
        }
    }

    /// What-if → open_order(contract, order, OrderState) only, as the
    /// reference answers a preview; a refused one then gets error 201
    /// (ibx#462).
    fn deliver_what_ifs(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = WhatIfResponse>) {
        for wi in items {
            let state = ClientCore::what_if_order_state(&wi.state);
            let tracked = if wi.final_reply {
                self.core.take_what_if(wi.order_id)
            } else {
                self.core.peek_what_if(wi.order_id)
            };
            let (mut contract, mut order) = tracked.unwrap_or_else(|| (Contract::default(), ApiOrder::default()));
            let placed_exchange = contract.exchange.clone();
            // A preview placed without a conId shows the contract looked
            // up (ibx#486).
            if contract.con_id == 0 && wi.state.con_id != 0 && !contract.sec_type.eq_ignore_ascii_case("BAG") {
                contract = self.core.get_contract(wi.state.con_id, &self.shared).unwrap_or(contract);
            }
            // The order as the reference shows it (its unset values); a
            // combo shows its combo (ibx#470).
            crate::client_core::preview_view(&mut contract, &mut order, &placed_exchange, &self.shared);
            crate::client_core::reported_unset_values(&mut order);
            ClientCore::apply_combo_view(wi.order_id, &mut contract, &mut order, &self.shared);
            // The preview's order carries the account and the client id,
            // as the reference's (ibx#486, b1_462_whatif of 02/10/2026).
            if order.account.is_empty() { order.account = self.account_id.clone(); }
            order.client_id = self.core.client_id.load(std::sync::atomic::Ordering::Relaxed) as i32;
            order.perm_id = wi.state.perm_id;
            order.use_price_mgmt_algo = i32::from(wi.state.price_mgmt);
            crate::client_core::preview_compete_values(&contract, &mut order);
            wrapper.open_order(wi.order_id, &contract, &order, &state);
            if !wi.state.reject_reason.is_empty() {
                wrapper.error(wi.order_id, 201, &format!("Order rejected - reason:{}", wi.state.reject_reason), "");
            }
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
        // A cancel, and an order that never left, give the status only.
        let view = view.filter(|_| !matches!(status, "Cancelled" | "ApiCancelled"))
            .map(|v| crate::client_core::with_report_perm_id(v, update.perm_id));
        let report = crate::client_core::OrderReport {
            view, status: status.into(), filled: filled_f, remaining: remaining_f,
            avg_fill_price: update.avg_fill_price as f64 / PRICE_SCALE_F,
            perm_id: update.perm_id, parent_id: update.parent_id, last_fill_price, client_id, why_held,
        };
        // Finished before its callbacks: a modify made from them gets 104
        // (ibx#544).
        self.core.finish_before_report(update.order_id, status, remaining_f);
        self.repeat_order_report(wrapper, update.order_id, &report);
        self.core.remember_report(update.order_id, report);
        self.core.update_order_status(update.order_id, status, filled_f, remaining_f);
    }

    /// openOrder (when the report has one) and orderStatus of a report.
    fn repeat_order_report(&self, wrapper: &mut impl Wrapper, order_id: i64, r: &crate::client_core::OrderReport) {
        let order_id = self.shared.orders.api_order_id(order_id);
        if let Some(v) = &r.view {
            wrapper.open_order(order_id, &v.contract, &v.order, &v.state);
        }
        let (avg, last, mkt_cap) = crate::client_core::status_prices(r);
        wrapper.order_status(
            order_id, &r.status, r.filled, r.remaining, avg,
            r.perm_id, r.parent_id, last, r.client_id, &r.why_held, mkt_cap,
        );
    }

    // ── Quote Dispatch ──

    fn dispatch_quotes(&self, wrapper: &mut impl Wrapper, limit: u64) {
        // Requests that joined a subscription, and subscriptions the
        // server rejected (ibx#444, ibx#447).
        let (notices, commands) = self.core.take_md_rejects(&self.shared);
        self.md_notices(wrapper, notices);
        {
            // The client's own follow-up of a refusal, not an API request:
            // not paced (ibx#555).
            let _internal = crate::engine::park::not_a_request();
            for command in commands {
                let _ = self.control_tx.send(command);
            }
        }

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
                Err((code, text)) => self.local_error(wrapper, req_id, code, &text),
            }
        }

        // What the requests that joined a running subscription get at once
        // (ibx#444).
        self.md_notices(wrapper, self.core.take_md_joins());

        // Request parameters, once per request (ibx#449), after the market
        // data type (ibx#446).
        // Known before the acknowledgement: the parameters first, the type
        // alone at it (ibx#535).
        for (req_id, mdt, params) in self.core.take_tick_req_params(&self.shared) {
            if let Some(mdt) = mdt {
                wrapper.market_data_type(req_id, mdt);
            }
            if let Some((min_tick, bbo_exchange, permissions)) = params {
                wrapper.tick_req_params(req_id, min_tick, &bbo_exchange, permissions);
            }
        }

        // The market data steps queued by the engine → tick callbacks, in
        // their order (ibx#446).
        let mut out = std::mem::take(&mut *self.core.md_out.lock().unwrap());
        self.core.poll_market_data_until(&self.shared, None, limit, &mut out);
        let mut snapshot_done: Vec<i64> = Vec::new();
        let mut text = [0u8; 24];
        for item in out.drain(..) {
            match item {
                MdOut::Tick(req_id, tick) => match tick {
                    MdTick::Price { tick_type, value, can_auto_execute, pre_open } => {
                        let attrib = crate::api::types::TickAttrib { can_auto_execute, pre_open, ..Default::default() };
                        wrapper.tick_price(req_id, tick_type, value, &attrib);
                    }
                    MdTick::Size { tick_type, value } => wrapper.tick_size(req_id, tick_type, value),
                    MdTick::Text { tick_type, value } => wrapper.tick_string(req_id, tick_type, &value),
                    MdTick::Time { tick_type, secs } =>
                        wrapper.tick_string(req_id, tick_type, crate::client_core::epoch_text(secs, &mut text)),
                    MdTick::Generic { tick_type, value } => wrapper.tick_generic(req_id, tick_type, value),
                    MdTick::Option(c) => wrapper.tick_option_computation(
                        req_id, c.tick_type, c.tick_attrib, c.implied_vol, c.delta, c.opt_price,
                        c.pv_dividend, c.gamma, c.vega, c.theta, c.und_price,
                    ),
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
        // Paper: the requests still without data after their wait (10197,
        // ibx#444).
        self.md_notices(wrapper, self.core.take_md_no_data(std::time::Instant::now()));
    }

    /// Historical ticks — route to the variant-specific callback (iso
    /// ibapi). The past ticks of a tick-by-tick request are written before
    /// the ticks held for them (captured 05/10/2026, AllLast with ten past
    /// ticks).
    fn deliver_historical_ticks(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, HistoricalTickData, String, bool)>) {
        for (req_id, data, _query_id, done) in items {
            match &data {
                HistoricalTickData::Midpoint(_) => wrapper.historical_ticks(req_id, &data, done),
                HistoricalTickData::Last(_) => wrapper.historical_ticks_last(req_id, &data, done),
                HistoricalTickData::BidAsk(_) => wrapper.historical_ticks_bid_ask(req_id, &data, done),
            }
        }
    }

    /// TBT trades → tick_by_tick_all_last, tickType 1 for Last and 2 for
    /// AllLast, with the entry's attributes (ibx#404, ibx#455)
    fn deliver_tbt_trades(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = TbtTrade>) {
        for trade in items {
            let attrib_last = TickAttribLast { past_limit: trade.past_limit, unreported: trade.unreported };
            wrapper.tick_by_tick_all_last(
                trade.req_id, trade.tbt_type.api_tick_type(), trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F, trade.size,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }
    }

    /// TBT quotes → tick_by_tick_bid_ask
    fn deliver_tbt_quotes(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = TbtQuote>) {
        for quote in items {
            let attrib_ba = TickAttribBidAsk { bid_past_low: quote.bid_past_low, ask_past_high: quote.ask_past_high };
            wrapper.tick_by_tick_bid_ask(
                quote.req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64, quote.ask_size as f64, &attrib_ba,
            );
        }
    }

    /// Depth updates → update_mkt_depth / update_mkt_depth_l2, as the
    /// book says (#451)
    fn deliver_depth(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = DepthUpdate>) {
        for du in items {
            if !du.l2 {
                wrapper.update_mkt_depth(du.req_id, du.position, du.operation, du.side, du.price, du.size);
            } else {
                wrapper.update_mkt_depth_l2(du.req_id, du.position, &du.market_maker, du.operation, du.side, du.price, du.size, du.is_smart_depth);
            }
        }
    }

    /// The callbacks of market data requests beside their ticks (ibx#444).
    fn md_notices(&self, wrapper: &mut impl Wrapper, notices: Vec<crate::client_core::MdNotice>) {
        use crate::client_core::MdNotice;
        for notice in notices {
            match notice {
                MdNotice::MarketDataType { req_id, market_data_type } => wrapper.market_data_type(req_id, market_data_type),
                MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions } =>
                    wrapper.tick_req_params(req_id, min_tick, &bbo_exchange, permissions),
                MdNotice::Error { req_id, code, text } => self.local_error(wrapper, req_id, code, &text),
                MdNotice::News { req_id, news } => wrapper.tick_news(
                    req_id, news.timestamp, &news.provider_code, &news.article_id, &news.headline, &news.extra_data,
                ),
            }
        }
    }

    // ── Historical / News / Account Dispatch ──

    /// News → tick_news, to each request of the contract whose news key
    /// has the provider (ibx#444)
    fn deliver_tick_news(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = TickNews>) {
        for news in items {
            for req_id in self.core.route_tick_news(&news) {
                wrapper.tick_news(
                    req_id, news.timestamp,
                    &news.provider_code, &news.article_id, &news.headline, &news.extra_data,
                );
            }
        }
    }

    /// Historical data → historical_data + historical_data_end
    fn deliver_historical_data(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, HistoricalResponse)>) {
        for (req_id, response) in items {
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
    }

    /// keepUpToDate: the whole current bar each time (ibx#429).
    fn deliver_historical_updates(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, HistoricalBar)>) {
        for (req_id, bar) in items {
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
    }

    /// Contract details → contract_details (the end is its own answer).
    fn deliver_contract_details(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, ContractDefinition)>) {
        for (req_id, def) in items {
            let details = ContractDetails::from_definition(&def);
            // A bond row is a bond contract details message (ibx#438).
            if def.is_bond() {
                wrapper.bond_contract_details(req_id, &details);
            } else {
                wrapper.contract_details(req_id, &details);
            }
        }
    }

    /// Matching symbols → symbol_samples
    fn deliver_matching_symbols(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, Vec<SymbolMatch>)>) {
        for (req_id, matches) in items {
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
    }

    /// Option chains → one security_definition_option_parameter per row,
    /// then the end (ibx#440).
    fn deliver_option_chains(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, Vec<crate::control::optparams::OptionChain>)>) {
        for (req_id, rows) in items {
            for r in &rows {
                wrapper.security_definition_option_parameter(req_id, &r.exchange, r.underlying_con_id,
                    &r.trading_class, &r.multiplier, &r.expirations, &r.strikes);
            }
            wrapper.security_definition_option_parameter_end(req_id);
        }
    }

    /// Scanner data. Cache is populated by the engine before dispatch
    /// (see `CcpState::start_scanner_enrichment`), so cold con_ids that
    /// arrived in `<ScanResponse>` have already been resolved by a definition request.
    /// The Some-arm fills the rich fields; the fallback covers deadline-
    /// flushed partials where a secdef reply never arrived.
    fn deliver_scanner_data(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, ScannerResult)>) {
        for (req_id, result) in items {
            for (rank, entry) in result.entries.iter().enumerate() {
                let mut contract = Contract { con_id: entry.con_id, ..Default::default() };
                // The row's market name is its trading class (the reference
                // 10.51, paper, 13/11/2026: "SCM" for both).
                let mut market_name = String::new();
                if let Some(ac) = self.core.get_contract(entry.con_id, &self.shared) {
                    contract.symbol = ac.symbol;
                    contract.sec_type = ac.sec_type;
                    contract.exchange = ac.exchange;
                    contract.currency = ac.currency;
                    contract.local_symbol = ac.local_symbol;
                    contract.primary_exchange = ac.primary_exchange;
                    market_name = ac.trading_class.clone();
                    contract.strike = ac.strike;
                    contract.trading_class = ac.trading_class;
                }
                let details = ContractDetails { contract, market_name, ..Default::default() };
                wrapper.scanner_data(req_id, rank as i32, &details,
                    &entry.distance, &entry.benchmark, &entry.projection, &entry.legs);
            }
            wrapper.scanner_data_end(req_id);
        }
    }

    /// Historical news
    fn deliver_historical_news(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, Vec<NewsHeadline>, bool)>) {
        for (req_id, headlines, has_more) in items {
            for h in &headlines {
                wrapper.historical_news(req_id, &h.time, &h.provider_code, &h.article_id, &h.headline);
            }
            wrapper.historical_news_end(req_id, has_more);
        }
    }

    /// Histogram data
    fn deliver_histograms(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, Vec<HistogramEntry>)>) {
        for (req_id, entries) in items {
            let items: Vec<(f64, i64)> = entries.iter().map(|e| (e.price, e.count)).collect();
            wrapper.histogram_data(req_id, &items);
        }
    }

    /// Historical schedules
    fn deliver_schedules(&self, wrapper: &mut impl Wrapper, items: impl IntoIterator<Item = (ReqId, HistoricalScheduleResponse)>) {
        for (req_id, schedule) in items {
            let sessions: Vec<(String, String, String)> = schedule.sessions.iter()
                .map(|s| (s.ref_date.clone(), s.open_time.clone(), s.close_time.clone()))
                .collect();
            wrapper.historical_schedule(
                req_id, &schedule.start_date_time, &schedule.end_date_time,
                &schedule.timezone, &sessions,
            );
        }
    }

    /// What is read from the session's state rather than from a queue.
    fn dispatch_data(&self, wrapper: &mut impl Wrapper) {
        // News bulletins → update_news_bulletin (popups always, every type
        // when subscribed, ibx#461)
        for b in self.core.bulletins_to_deliver(&self.shared) {
            wrapper.update_news_bulletin(b.msg_id as i64, b.msg_type, &b.message, &b.exchange);
        }

        // Smart components that waited for their exchange map (ibx#441).
        for (req_id, answer) in self.core.take_smart_components(&self.shared) {
            match answer {
                Ok(components) => wrapper.smart_components(req_id, &components),
                Err((code, msg)) => self.local_error(wrapper, req_id, code, &msg),
            }
        }

        // Scanner params, rebuilt here as the reference hands them to a
        // client (ibx#571).
        for xml in crate::client_core::drain_scanner_parameters(&self.shared) {
            wrapper.scanner_parameters(&xml);
        }

        // The replaces of FA data whose saves succeeded (ibx#507).
        for req_id in self.shared.reference.drain_fa_saved() {
            wrapper.replace_fa_end(req_id, crate::control::fa::SAVED);
        }

        // reqMktDepthExchanges, answered from the routing table (#453).
        if let Some(descriptions) = self.shared.reference.drain_depth_exchanges() {
            wrapper.mkt_depth_exchanges(&descriptions);
        }

        // PnL → pnl callback (change-detected via ClientCore)
        // The P&L quotes of the positions, asked by the P&L as the
        // reference's own (ibx#511); they never reach the tick callbacks.
        self.core.maintain_pnl_quotes(&self.shared, &self.control_tx);
        // A value not valid comes after error 2150 (ibx#511).
        for update in self.core.poll_pnl(&self.shared) {
            for _ in 0..update.errors {
                self.local_error(wrapper, update.req_id, 2150, crate::client_core::PNL_INVALID_TEXT);
            }
            wrapper.pnl(update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl);
        }

        // PnL single → pnl_single callback (via ClientCore)
        for update in self.core.poll_pnl_single(&self.shared) {
            for _ in 0..update.errors {
                self.local_error(wrapper, update.req_id, 2150, crate::client_core::PNL_INVALID_TEXT);
            }
            wrapper.pnl_single(update.req_id, update.pos, update.daily_pnl, update.unrealized_pnl, update.realized_pnl, update.value);
        }

        // Positions of a running req_positions (ibx#477).
        self.dispatch_positions(wrapper);
        // Multi-account requests (ibx#476).
        self.dispatch_multi(wrapper);

        // Account updates (ibx#475): values, portfolio rows each followed by
        // the account time, the time after the batch, and for the first image
        // the end, once per subscription.
        if let Some(batch) = self.core.prepare_account_updates(&self.shared, &self.account_id) {
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
