//! Event dispatch: drains SharedState queues and fires Python wrapper callbacks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use crate::bridge::{Event, SharedState};
use crate::client_core::order_status_str;
use crate::types::*;

use crate::api::types::{
    Contract as ApiContract, Execution as ApiExecution,
    CommissionAndFeesReport as ApiCommissionAndFeesReport,
};
use super::EClient;
use super::super::contract::{Contract, ContractDescription, ContractDetails, BarData, CommissionAndFeesReport, DepthMktDataDescriptionPy, Execution, Order, OrderState};
use super::super::tick_types::*;
use super::super::super::types::{PRICE_SCALE_F, QTY_SCALE_F};

/// Call a Python wrapper method, catching and logging any exception instead of propagating.
/// This prevents user callback exceptions from killing the dispatch loop.
macro_rules! call_wrapper {
    ($wrapper:expr, $py:expr, $method:expr, $args:expr) => {
        if let Err(e) = $wrapper.call_method($py, $method, $args, None) {
            log::error!("Python callback {}() raised: {}", $method, e);
            e.restore($py);
            unsafe { pyo3::ffi::PyErr_Clear(); }
        }
    };
}

impl EClient {
    /// Rows of the running multi-account requests (ibx#476).
    pub(crate) fn dispatch_multi(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        let own = self.account();
        for batch in self.core.prepare_account_multi(shared) {
            let account = if batch.account.is_empty() { own.as_str() } else { batch.account.as_str() };
            for row in &batch.rows {
                call_wrapper!(self.wrapper, py, "account_update_multi",
                    (batch.req_id, account, batch.model_code.as_str(), row.key.as_str(), row.value.as_str(), row.currency.as_str()));
            }
            if batch.end {
                call_wrapper!(self.wrapper, py, "account_update_multi_end", (batch.req_id,));
            }
        }
        for (req_id, account, model_code, batch) in self.core.prepare_positions_multi(shared) {
            let account = if account.is_empty() { own.clone() } else { account };
            for pi in &batch.rows {
                let ac = self.core.position_contract(pi.con_id, shared);
                let mut c = Contract::default();
                c.con_id = ac.con_id;
                c.symbol = ac.symbol;
                c.sec_type = ac.sec_type;
                c.exchange = ac.exchange;
                c.primary_exchange = ac.primary_exchange;
                c.currency = ac.currency;
                c.local_symbol = ac.local_symbol;
                c.trading_class = ac.trading_class;
                c.multiplier = ac.multiplier;
                let c_py = Py::new(py, c)?.into_any();
                call_wrapper!(self.wrapper, py, "position_multi",
                    (req_id, account.as_str(), model_code.as_str(), &c_py,
                     pi.position_fixed as f64 / QTY_SCALE_F, pi.avg_cost as f64 / PRICE_SCALE_F));
            }
            if batch.end {
                call_wrapper!(self.wrapper, py, "position_multi_end", (req_id,));
            }
            if let Some((code, message)) = batch.error {
                call_wrapper!(self.wrapper, py, "error", (req_id, code, message.as_str(), ""));
            }
        }
        Ok(())
    }

    /// Position rows of a running req_positions (ibx#477).
    pub(crate) fn dispatch_positions(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        let Some(batch) = self.core.prepare_positions(shared) else { return Ok(()) };
        let account = self.account();
        for pi in &batch.rows {
            let ac = self.core.position_contract(pi.con_id, shared);
            let mut c = Contract::default();
            c.con_id = ac.con_id;
            c.symbol = ac.symbol;
            c.sec_type = ac.sec_type;
            c.exchange = ac.exchange;
            c.primary_exchange = ac.primary_exchange;
            c.currency = ac.currency;
            c.local_symbol = ac.local_symbol;
            c.trading_class = ac.trading_class;
            c.multiplier = ac.multiplier;
            let c_py = Py::new(py, c)?.into_any();
            call_wrapper!(self.wrapper, py, "position",
                (account.as_str(), &c_py, pi.position_fixed as f64 / QTY_SCALE_F, pi.avg_cost as f64 / PRICE_SCALE_F));
        }
        if batch.end {
            call_wrapper!(self.wrapper, py, "position_end", ());
        }
        if let Some((code, message)) = batch.error {
            call_wrapper!(self.wrapper, py, "error", (-1i64, code, message.as_str(), ""));
        }
        Ok(())
    }

    /// open_order for an order after a server report (ibx#473).
    fn send_open_order(&self, py: Python<'_>, order_id: u64, view: &crate::client_core::OrderView) -> PyResult<()> {
        let c = Contract {
            con_id: view.contract.con_id,
            symbol: view.contract.symbol.clone(),
            sec_type: view.contract.sec_type.clone(),
            exchange: view.contract.exchange.clone(),
            primary_exchange: view.contract.primary_exchange.clone(),
            currency: view.contract.currency.clone(),
            local_symbol: view.contract.local_symbol.clone(),
            trading_class: view.contract.trading_class.clone(),
            ..Default::default()
        };
        let src = &view.order;
        let mut o = Order::default();
        o.order_id = order_id as i64;
        o.action = src.action.clone();
        o.total_quantity = src.total_quantity;
        o.order_type = src.order_type.clone();
        o.lmt_price = src.lmt_price;
        o.aux_price = src.aux_price;
        o.tif = src.tif.clone();
        o.account = src.account.clone();
        o.perm_id = src.perm_id;
        o.parent_id = src.parent_id;
        o.oca_type = src.oca_type;
        o.outside_rth = src.outside_rth;
        o.order_ref = src.order_ref.clone();
        o.use_price_mgmt_algo = src.use_price_mgmt_algo;
        o.trail_stop_price = src.trail_stop_price;
        o.algo_strategy = src.algo_strategy.clone();
        o.what_if = src.what_if;
        let mut state = OrderState::default();
        state.status = view.state.status.clone();
        state.commission_and_fees = view.state.commission_and_fees;
        state.completed_time = view.state.completed_time.clone();
        state.completed_status = view.state.completed_status.clone();
        let c_py = Py::new(py, c)?.into_any();
        let o_py = Py::new(py, o)?.into_any();
        let state_py = Py::new(py, state)?.into_any();
        call_wrapper!(self.wrapper, py, "open_order", (order_id as i64, &c_py, &o_py, &state_py));
        Ok(())
    }

    fn send_commission_report(&self, py: Python<'_>, cr: &ApiCommissionAndFeesReport) -> PyResult<()> {
        let report = CommissionAndFeesReport {
            exec_id: cr.exec_id.clone(),
            commission_and_fees: cr.commission_and_fees,
            currency: cr.currency.clone(),
            realized_pnl: cr.realized_pnl,
            yield_amount: cr.yield_amount,
            yield_redemption_date: cr.yield_redemption_date.clone(),
        };
        let report_py = Py::new(py, report)?.into_any();
        call_wrapper!(self.wrapper, py, "commission_and_fees_report", (&report_py,));
        Ok(())
    }

    /// Single iteration of event dispatch: drain all shared queues and fire Python callbacks.
    pub(crate) fn dispatch_once(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        // Drain engine events — surface disconnects as error callbacks.
        if let Some(rx) = self.event_rx.lock().unwrap().as_ref() {
            while let Ok(event) = rx.try_recv() {
                if matches!(event, Event::Disconnected) {
                    call_wrapper!(self.wrapper, py, "error", (-1i64, 1100i64, "Connectivity between client and server has been lost", ""));
                    self.connected.store(false, Ordering::Release);
                }
            }
        }

        // Drain fills -> execDetails + orderStatus. The commission report
        // comes later, from its own server frame (ibx#471).
        let fills = shared.orders.drain_fills_with_exec();
        for (fill, fill_exec) in fills {
            // As the reference: BOT / SLD (ibx#474).
            let side_str = match fill.side {
                Side::Buy => "BOT",
                Side::Sell | Side::ShortSell => "SLD",
            };
            let price = fill.price as f64 / PRICE_SCALE_F;

            let status = if fill.remaining_fixed == 0 { "Filled" } else { self.core.partial_fill_status(fill.order_id) };
            let (perm_id, parent_id) = shared.orders.get_order_info(fill.order_id)
                .map(|info| (info.order.perm_id, info.order.parent_id))
                .unwrap_or((0, 0));
            // filled and avgFillPrice are the order totals carried on the
            // fill, lastFillPrice is this print (ibx#315). Quantities are
            // fixed-point; the callbacks take decimal shares (ibx#313).
            let cum_qty = fill.filled_so_far_fixed() as f64 / QTY_SCALE_F;
            let remaining = fill.remaining_fixed as f64 / QTY_SCALE_F;
            let shares = fill.qty_fixed as f64 / QTY_SCALE_F;
            let avg_price = fill.average_price() as f64 / PRICE_SCALE_F;
            // openOrder then orderStatus for every report of a known order
            // (ibx#473).
            let client_id = match self.core.order_view(fill.order_id, shared, status) {
                Some(view) => {
                    self.send_open_order(py, fill.order_id, &view)?;
                    view.client_id
                }
                None => 0,
            };
            call_wrapper!(self.wrapper, py, "order_status", (fill.order_id as i64, status, cum_qty, remaining,
                 avg_price, perm_id, parent_id, price, client_id, "", 0.0f64));
            self.core.record_last_fill_price(fill.order_id, price);

            let rich_info = shared.orders.get_order_info(fill.order_id);
            // Build api-level contract for shared storage
            let api_contract = self.core.open_orders.lock().unwrap()
                .get(&fill.order_id).map(|o| o.contract.clone())
                .or_else(|| {
                    rich_info.map(|info| info.contract)
                })
                .unwrap_or_default();

            // A fill injected with no execution details (tests) gets a local
            // id and time; a real fill carries the server's (ibx#471 ibx#474).
            let mut api_exec = ApiExecution {
                exec_id: format!("{}.{}", fill.order_id, fill.timestamp_ns),
                time: format!("{}", fill.timestamp_ns),
                acct_number: self.account(),
                side: side_str.to_string(),
                shares,
                price,
                perm_id,
                order_id: fill.order_id as i64,
                cum_qty,
                avg_price,
                ..Default::default()
            };
            self.core.apply_fill_exec(&mut api_exec, &fill_exec, fill.order_id);

            // Build Python contract for callback
            let exec_contract = Contract {
                con_id: api_contract.con_id,
                symbol: api_contract.symbol.clone(),
                sec_type: api_contract.sec_type.clone(),
                exchange: api_contract.exchange.clone(),
                currency: api_contract.currency.clone(),
                ..Default::default()
            };

            let c_py = Py::new(py, exec_contract)?.into_any();
            let exec_obj = Execution {
                exec_id: api_exec.exec_id.clone(),
                time: api_exec.time.clone(),
                acct_number: api_exec.acct_number.clone(),
                exchange: api_exec.exchange.clone(),
                side: api_exec.side.clone(),
                shares,
                price,
                perm_id,
                client_id: api_exec.client_id,
                order_id: fill.order_id as i64,
                liquidation: 0,
                cum_qty,
                avg_price,
                order_ref: api_exec.order_ref.clone(),
                model_code: api_exec.model_code.clone(),
                last_liquidity: 0,
                pending_price_revision: false,
                ..Default::default()
            };

            // Store for req_executions replay via shared core; a commission
            // report that came first is sent after exec_details.
            let early_report = self.core.push_execution(-1, api_contract, api_exec, fill_exec.time_secs);

            let exec_py = Py::new(py, exec_obj)?.into_any();
            // A live execution has no request: reqId -1 (ibx#474).
            call_wrapper!(self.wrapper, py, "exec_details", (-1i64, &c_py, &exec_py));

            if let Some(cr) = early_report {
                self.send_commission_report(py, &cr)?;
            }

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, cum_qty, remaining);
        }

        // Commission reports, sent once their execution is known (ibx#471).
        for cr in shared.orders.drain_commission_reports() {
            if self.core.apply_commission(&cr) {
                self.send_commission_report(py, &cr)?;
            }
        }

        // Order errors (refused before sending, or rejected by the server)
        // -> error, ahead of the status: the reference reports a server
        // reject as error 201 before the Inactive status (ibx#250).
        for (order_id, code, msg) in shared.orders.drain_order_errors() {
            call_wrapper!(self.wrapper, py, "error", (order_id as i64, code, msg.as_str(), ""));
        }

        // Drain order updates -> orderStatus
        let updates = shared.orders.drain_order_updates();
        for update in updates {
            let status = order_status_str(update.status);
            let filled = update.filled_qty_fixed as f64 / QTY_SCALE_F;
            let remaining = update.remaining_qty_fixed as f64 / QTY_SCALE_F;
            // open_order + order_status for every report of a known order;
            // a cancel gives order_status only (ibx#473).
            let view = self.core.order_view(update.order_id, shared, status);
            if let Some(v) = view.as_ref().filter(|_| status != "Cancelled") {
                self.send_open_order(py, update.order_id, v)?;
            }
            let (last_fill_price, client_id) = view.map(|v| (v.last_fill_price, v.client_id)).unwrap_or((0.0, 0));
            call_wrapper!(self.wrapper, py, "order_status", (update.order_id as i64, status, filled,
                 remaining, update.avg_fill_price as f64 / PRICE_SCALE_F,
                 update.perm_id, update.parent_id, last_fill_price, client_id, "", 0.0f64));

            // Track open orders
            self.core.update_order_status(update.order_id, status, filled, remaining);
        }

        // Drain cancel rejects -> error
        let rejects = shared.orders.drain_cancel_rejects();
        for reject in rejects {
            let code = if reject.reject_type == 1 { 202i64 } else { 10147i64 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            call_wrapper!(self.wrapper, py, "error", (reject.order_id as i64, code, msg.as_str(), ""));
        }

        // Poll quotes for changes -> tickPrice/tickSize
        // Poll quotes via shared ClientCore (same logic as Rust dispatch)
        let instruments = self.core.snapshot_instruments();
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let result = self.core.poll_instrument_ticks(shared, iid, req_id);

            // Fire market_data_type once per subscription on first tick delivery
            if let Some(mdt) = self.core.check_mdt_needed(req_id, result.delivered) {
                call_wrapper!(self.wrapper, py, "market_data_type", (req_id, mdt));
            }

            let attrib = TickAttrib::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            for tick in &result.ticks {
                if tick.is_price {
                    call_wrapper!(self.wrapper, py, "tick_price", (tick.req_id, tick.tick_type, tick.value, &attrib_obj));
                } else {
                    call_wrapper!(self.wrapper, py, "tick_size", (tick.req_id, tick.tick_type, tick.value));
                }
            }
            for st in &result.string_ticks {
                call_wrapper!(self.wrapper, py, "tick_string", (st.req_id, st.tick_type, st.value.as_str()));
            }
            if let Some(ts) = &result.timestamp {
                let ts_secs = ts.timestamp_ns / 1_000_000_000;
                call_wrapper!(self.wrapper, py, "tick_string", (ts.req_id, TICK_LAST_TIMESTAMP, ts_secs.to_string().as_str()));
            }
            if self.core.check_snapshot_done(req_id, result.delivered) {
                call_wrapper!(self.wrapper, py, "tick_snapshot_end", (req_id,));
                snapshot_done.push(req_id);
            }
        }
        for req_id in snapshot_done {
            self.cancel_mkt_data(req_id)?;
        }

        // Drain TBT trades -> tickByTickAllLast
        let tbt_trades = shared.market.drain_tbt_trades();
        for trade in tbt_trades {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&trade.instrument).copied().unwrap_or(-1);
            let price = trade.price as f64 / PRICE_SCALE_F;
            let size = trade.size as f64;
            let attrib = super::super::tick_types::TickAttribLast::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            call_wrapper!(self.wrapper, py, "tick_by_tick_all_last", (req_id, 1i32, trade.timestamp as i64, price, size,
                 &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()));
        }

        // Drain TBT quotes -> tickByTickBidAsk
        let tbt_quotes = shared.market.drain_tbt_quotes();
        for quote in tbt_quotes {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&quote.instrument).copied().unwrap_or(-1);
            let attrib = super::super::tick_types::TickAttribBidAsk::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            call_wrapper!(self.wrapper, py, "tick_by_tick_bid_ask", (req_id, quote.timestamp as i64,
                 quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                 quote.bid_size as f64, quote.ask_size as f64, &attrib_obj));
        }

        // Drain depth updates -> updateMktDepth / updateMktDepthL2
        let depth_updates = shared.market.drain_depth_updates();
        for du in depth_updates {
            if du.market_maker.is_empty() {
                call_wrapper!(self.wrapper, py, "update_mkt_depth", (du.req_id as i64, du.position, du.operation, du.side, du.price, du.size));
            } else {
                call_wrapper!(self.wrapper, py, "update_mkt_depth_l2", (du.req_id as i64, du.position, du.market_maker.as_str(),
                     du.operation, du.side, du.price, du.size, du.is_smart_depth));
            }
        }

        // Drain news -> tickNews
        let news_items = shared.market.drain_tick_news();
        for news in news_items {
            let req_id = self.core.req_id_for_instrument(news.instrument);
            call_wrapper!(self.wrapper, py, "tick_news", (req_id, news.timestamp as i64, news.provider_code.as_str(),
                 news.article_id.as_str(), news.headline.as_str(), ""));
        }

        // Drain news bulletins -> updateNewsBulletin
        if self.core.bulletin_subscribed.load(Ordering::Acquire) {
            let bulletins = shared.market.drain_news_bulletins();
            for b in bulletins {
                call_wrapper!(self.wrapper, py, "update_news_bulletin", (b.msg_id as i64, b.msg_type, b.message.as_str(), b.exchange.as_str()));
            }
        }

        // Drain what-if responses -> open_order(contract, order, OrderState) + order_status
        // (iso with official ibapi: server delivers margin via openOrder.orderState)
        let what_ifs = shared.orders.drain_what_if_responses();
        for wi in what_ifs {
            let fmt = |p: Price| format!("{:.2}", p as f64 / PRICE_SCALE_F);
            let mut state = OrderState::default();
            state.status = "PreSubmitted".into();
            state.init_margin_before = fmt(wi.init_margin_before);
            state.maint_margin_before = fmt(wi.maint_margin_before);
            state.equity_with_loan_before = fmt(wi.equity_with_loan_before);
            state.init_margin_change = fmt(wi.init_margin_after - wi.init_margin_before);
            state.maint_margin_change = fmt(wi.maint_margin_after - wi.maint_margin_before);
            state.equity_with_loan_change = fmt(wi.equity_with_loan_after - wi.equity_with_loan_before);
            state.init_margin_after = fmt(wi.init_margin_after);
            state.maint_margin_after = fmt(wi.maint_margin_after);
            state.equity_with_loan_after = fmt(wi.equity_with_loan_after);
            state.commission_and_fees = wi.commission as f64 / PRICE_SCALE_F;

            let tracked = self.core.open_orders.lock().unwrap().get(&wi.order_id).cloned();
            let (contract_py, order_py) = if let Some(t) = tracked {
                let c = Contract {
                    con_id: t.contract.con_id,
                    symbol: t.contract.symbol,
                    sec_type: t.contract.sec_type,
                    exchange: t.contract.exchange,
                    currency: t.contract.currency,
                    ..Default::default()
                };
                let mut o = Order::default();
                o.order_id = t.order.order_id;
                o.action = t.order.action;
                o.total_quantity = t.order.total_quantity;
                o.order_type = t.order.order_type;
                o.lmt_price = t.order.lmt_price;
                o.aux_price = t.order.aux_price;
                o.tif = t.order.tif;
                o.what_if = t.order.what_if;
                (Py::new(py, c)?.into_any(), Py::new(py, o)?.into_any())
            } else {
                (Py::new(py, Contract::default())?.into_any(),
                 Py::new(py, Order::default())?.into_any())
            };
            let state_py = Py::new(py, state)?.into_any();
            call_wrapper!(self.wrapper, py, "open_order",
                (wi.order_id as i64, &contract_py, &order_py, &state_py));
            call_wrapper!(self.wrapper, py, "order_status", (wi.order_id as i64, "PreSubmitted", 0.0f64, 0.0f64,
                 0.0f64, 0i64, 0i64, 0.0f64, 0i64, "", 0.0f64));
            self.core.open_orders.lock().unwrap().remove(&wi.order_id);
        }

        // Drain HMDS query errors -> error (ibx#186). Surface gateway-side validation
        // failures (e.g. "Invalid time length") that previously vanished silently.
        for (req_id, code, msg) in shared.reference.drain_historical_errors() {
            call_wrapper!(self.wrapper, py, "error", (req_id as i64, code as i64, msg.as_str(), ""));
        }

        // Drain historical data -> historicalData + historicalDataEnd / historicalDataUpdate
        let hist_data = shared.reference.drain_historical_data();
        for (req_id, response) in hist_data {
            let is_update = self.core.hist_initial_complete.lock().unwrap().contains(&req_id);
            for bar in &response.bars {
                let bar_obj = BarData::new(
                    bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count as i32,
                    response.timezone.clone(),
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                if is_update {
                    call_wrapper!(self.wrapper, py, "historical_data_update", (req_id as i64, &bar_py));
                } else {
                    call_wrapper!(self.wrapper, py, "historical_data", (req_id as i64, &bar_py));
                }
            }
            if response.is_complete && !is_update {
                self.core.hist_initial_complete.lock().unwrap().insert(req_id);
                call_wrapper!(self.wrapper, py, "historical_data_end", (req_id as i64, "", ""));
            }
        }

        // Drain head timestamps -> headTimestamp
        let head_ts = shared.reference.drain_head_timestamps();
        for (req_id, response) in head_ts {
            call_wrapper!(self.wrapper, py, "head_timestamp",
                (req_id as i64, response.head_timestamp.as_str()));
        }

        // Drain contract details -> contractDetails + contractDetailsEnd
        let contract_defs = shared.reference.drain_contract_details();
        for (req_id, def) in contract_defs {
            let details = ContractDetails::from_definition(py, &def);
            let details_py = Py::new(py, details)?.into_any();
            call_wrapper!(self.wrapper, py, "contract_details",
                (req_id as i64, &details_py));
        }
        let contract_ends = shared.reference.drain_contract_details_end();
        for req_id in contract_ends {
            call_wrapper!(self.wrapper, py, "contract_details_end", (req_id as i64,));
        }

        // Drain matching symbols -> symbolSamples
        let symbol_results = shared.reference.drain_matching_symbols();
        for (req_id, matches) in symbol_results {
            let descriptions: Vec<Py<ContractDescription>> = matches.iter().map(|m| {
                Py::new(py, ContractDescription {
                    con_id: m.con_id as i64,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.to_fix().to_string(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                }).unwrap()
            }).collect();
            let list = pyo3::types::PyList::new(py, &descriptions)?;
            call_wrapper!(self.wrapper, py, "symbol_samples", (req_id as i64, list.as_any()));
        }

        // Drain depth exchanges -> mktDepthExchanges
        let depth_exchanges = shared.reference.drain_depth_exchanges();
        if !depth_exchanges.is_empty() {
            let descriptions: Vec<Py<DepthMktDataDescriptionPy>> = depth_exchanges.iter().map(|d| {
                Py::new(py, DepthMktDataDescriptionPy {
                    exchange: d.exchange.clone(),
                    sec_type: d.sec_type.clone(),
                    listing_exch: d.listing_exch.clone(),
                    service_data_type: d.service_data_type.clone(),
                    agg_group: d.agg_group,
                }).unwrap()
            }).collect();
            let list = pyo3::types::PyList::new(py, &descriptions)?;
            call_wrapper!(self.wrapper, py, "mkt_depth_exchanges", (list.as_any(),));
        }

        // Drain scanner params -> scannerParameters
        let scanner_params = shared.reference.drain_scanner_params();
        for xml in scanner_params {
            call_wrapper!(self.wrapper, py, "scanner_parameters", (xml.as_str(),));
        }

        // Drain scanner data -> scannerData + scannerDataEnd
        let scanner_results = shared.reference.drain_scanner_data();
        for (req_id, result) in scanner_results {
            for (rank, entry) in result.entries.iter().enumerate() {
                let cd = ContractDetails::new_default(py);
                {
                    let mut contract = cd.contract.borrow_mut(py);
                    contract.con_id = entry.con_id as i64;
                    // Look up cached contract for symbol info
                    if let Some(ac) = self.core.get_contract(entry.con_id as i64, shared) {
                        contract.symbol = ac.symbol;
                        contract.sec_type = ac.sec_type;
                        contract.exchange = ac.exchange;
                        contract.currency = ac.currency;
                        contract.local_symbol = ac.local_symbol;
                        contract.primary_exchange = ac.primary_exchange;
                        contract.trading_class = ac.trading_class;
                    }
                }
                let cd_py = Py::new(py, cd)?.into_any();
                call_wrapper!(self.wrapper, py, "scanner_data", (req_id as i64, rank as i32, &cd_py, "", "", "", ""));
            }
            call_wrapper!(self.wrapper, py, "scanner_data_end", (req_id as i64,));
        }

        // Drain historical news -> historicalNews + historicalNewsEnd
        let news_results = shared.reference.drain_historical_news();
        for (req_id, headlines, has_more) in news_results {
            for h in &headlines {
                call_wrapper!(self.wrapper, py, "historical_news", (req_id as i64, h.time.as_str(), h.provider_code.as_str(),
                     h.article_id.as_str(), h.headline.as_str()));
            }
            call_wrapper!(self.wrapper, py, "historical_news_end", (req_id as i64, has_more));
        }

        // Drain news articles -> newsArticle
        let articles = shared.reference.drain_news_articles();
        for (req_id, article_type, text) in articles {
            call_wrapper!(self.wrapper, py, "news_article", (req_id as i64, article_type, text.as_str()));
        }

        // Drain fundamental data -> fundamentalData
        let fundamentals = shared.reference.drain_fundamental_data();
        for (req_id, data) in fundamentals {
            call_wrapper!(self.wrapper, py, "fundamental_data", (req_id as i64, data.as_str()));
        }

        // Drain histogram data -> histogram_data
        let histograms = shared.reference.drain_histogram_data();
        for (req_id, entries) in histograms {
            let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
            }).collect();
            let py_list = pyo3::types::PyList::new(py, tuples)?;
            call_wrapper!(self.wrapper, py, "histogram_data", (req_id as i64, py_list));
        }

        // Drain historical ticks
        let hist_ticks = shared.reference.drain_historical_ticks();
        for (req_id, data, _what, done) in hist_ticks {
            match data {
                crate::types::HistoricalTickData::Midpoint(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.price.into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks", (req_id as i64, list, done));
                }
                crate::types::HistoricalTickData::Last(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.price.into_pyobject(py).unwrap().into_any(),
                            t.size.into_pyobject(py).unwrap().into_any(),
                            t.exchange.as_str().into_pyobject(py).unwrap().into_any(),
                            t.special_conditions.as_str().into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks_last", (req_id as i64, list, done));
                }
                crate::types::HistoricalTickData::BidAsk(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.bid_price.into_pyobject(py).unwrap().into_any(),
                            t.ask_price.into_pyobject(py).unwrap().into_any(),
                            t.bid_size.into_pyobject(py).unwrap().into_any(),
                            t.ask_size.into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks_bid_ask", (req_id as i64, list, done));
                }
            }
        }

        // Drain real-time bars -> real_time_bar or historical_data_update (keepUpToDate)
        let rtbars = shared.market.drain_real_time_bars();
        for (req_id, bar) in rtbars {
            if self.core.hist_initial_complete.lock().unwrap().contains(&req_id) {
                // keepUpToDate bar → dispatch as historical_data_update
                let bar_obj = BarData::new(
                    format!("{}", bar.timestamp), bar.open, bar.high, bar.low, bar.close,
                    bar.volume as i64, bar.wap, bar.count,
                    String::new(), // streaming bars carry no timezone (ibx#234)
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                call_wrapper!(self.wrapper, py, "historical_data_update", (req_id as i64, &bar_py));
            } else {
                call_wrapper!(self.wrapper, py, "real_time_bar", (
                    req_id as i64,
                    bar.timestamp as i64,
                    bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count,
                ));
            }
        }

        // Drain historical schedules -> historical_schedule
        let schedules = shared.reference.drain_historical_schedules();
        for (req_id, resp) in schedules {
            let sessions: Vec<Bound<'_, pyo3::types::PyTuple>> = resp.sessions.iter().map(|s| {
                pyo3::types::PyTuple::new(py, &[
                    s.ref_date.as_str().into_pyobject(py).unwrap().into_any(),
                    s.open_time.as_str().into_pyobject(py).unwrap().into_any(),
                    s.close_time.as_str().into_pyobject(py).unwrap().into_any(),
                ]).unwrap()
            }).collect();
            let py_sessions = pyo3::types::PyList::new(py, sessions)?;
            call_wrapper!(self.wrapper, py, "historical_schedule", (
                req_id as i64,
                resp.start_date_time.as_str(),
                resp.end_date_time.as_str(),
                resp.timezone.as_str(),
                py_sessions,
            ));
        }

        // Positions of a running req_positions (ibx#477).
        self.dispatch_positions(py, shared)?;
        // Multi-account requests (ibx#476).
        self.dispatch_multi(py, shared)?;

        // Account updates (ibx#475): values, portfolio rows each followed by
        // the account time, the time after the batch, and for the first image
        // the end, once per subscription.
        if let Some(batch) = self.core.prepare_account_updates(shared) {
            let account_name = self.account();
            for field in &batch.fields {
                call_wrapper!(self.wrapper, py, "update_account_value", (field.key.as_str(), field.value.as_str(), field.currency.as_str(), account_name.as_str()));
            }

            let portfolio = self.core.prepare_portfolio_updates(shared);
            for entry in &portfolio {
                let ac = self.core.position_contract(entry.con_id, shared);
                let mut c = crate::python::compat::contract::Contract::default();
                c.con_id = ac.con_id;
                c.symbol = ac.symbol;
                c.sec_type = ac.sec_type;
                c.exchange = ac.exchange;
                c.primary_exchange = ac.primary_exchange;
                c.currency = ac.currency;
                c.local_symbol = ac.local_symbol;
                c.trading_class = ac.trading_class;
                c.multiplier = ac.multiplier;
                let c_py = pyo3::Py::new(py, c).unwrap().into_any();
                call_wrapper!(self.wrapper, py, "update_portfolio",
                    (&c_py, entry.position, entry.market_price, entry.market_value,
                     entry.avg_cost, entry.unrealized_pnl, entry.realized_pnl, account_name.as_str()));
                call_wrapper!(self.wrapper, py, "update_account_time", (batch.time.as_str(),));
            }

            if !batch.fields.is_empty() || !portfolio.is_empty() {
                call_wrapper!(self.wrapper, py, "update_account_time", (batch.time.as_str(),));
            }
            if batch.download_end {
                call_wrapper!(self.wrapper, py, "account_download_end", (account_name.as_str(),));
            }
        }

        // P&L dispatch (via ClientCore)
        // Quotes the P&L needs, subscribed by ibx itself when the caller has
        // none; they never reach the tick callbacks.
        if let Ok(tx) = self.tx() {
            self.core.maintain_pnl_quotes(shared, &tx);
        }
        for update in self.core.poll_pnl(shared) {
            call_wrapper!(self.wrapper, py, "pnl", (update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl));
        }

        // Per-position P&L dispatch (via ClientCore)
        for update in self.core.poll_pnl_single(shared) {
            call_wrapper!(self.wrapper, py, "pnl_single", (update.req_id, update.pos, update.daily_pnl,
                 update.unrealized_pnl, update.realized_pnl, update.value));
        }

        // Account summary rows as the server sends them; the end at each of
        // its end markers (ibx#479).
        {
            let acct_name = self.account();
            for batch in self.core.prepare_account_summary(shared) {
                for row in &batch.rows {
                    call_wrapper!(self.wrapper, py, "account_summary", (batch.req_id, acct_name.as_str(), row.key.as_str(), row.value.as_str(), row.currency.as_str()));
                }
                if batch.end {
                    call_wrapper!(self.wrapper, py, "account_summary_end", (batch.req_id,));
                }
            }
        }

        Ok(())
    }
}
