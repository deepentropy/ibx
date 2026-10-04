//! Event dispatch: drains SharedState queues and fires Python wrapper callbacks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use crate::bridge::{Event, SharedState};
use crate::client_core::{order_status_str, MdOut, MdTick};
use crate::types::*;

use crate::api::types::{
    Contract as ApiContract, Execution as ApiExecution,
    CommissionAndFeesReport as ApiCommissionAndFeesReport,
};
use super::{send_cmd, EClient};
use super::super::contract::{Contract, ContractDescription, ContractDetails, BarData, CommissionAndFeesReport, DepthMktDataDescriptionPy, Execution, Order, OrderState};
use super::super::tick_types::*;
use super::super::super::types::{PRICE_SCALE_F, QTY_SCALE_F};

/// Call a Python wrapper method. An `Exception` it raises is logged and
/// dropped, so one failing callback does not stop the dispatch loop. Any
/// other `BaseException` (KeyboardInterrupt, SystemExit) returns from the
/// enclosing function and reaches the caller of run() (ibx#270).
macro_rules! call_wrapper {
    ($wrapper:expr, $py:expr, $method:expr, $args:expr) => {
        if let Err(e) = $wrapper.call_method($py, $method, $args, None) {
            callback_raised($py, $method, e)?;
        }
    };
}

/// What a callback exception does: see `call_wrapper!` (ibx#270).
pub(crate) fn callback_raised(py: Python<'_>, method: &str, e: PyErr) -> PyResult<()> {
    if e.is_instance_of::<pyo3::exceptions::PyException>(py) {
        log::error!("Python callback {}() raised: {}", method, e);
        Ok(())
    } else {
        Err(e)
    }
}

impl EClient {
    /// The callbacks of market data requests beside their ticks (ibx#444).
    fn md_notices(&self, py: Python<'_>, notices: Vec<crate::client_core::MdNotice>) -> PyResult<()> {
        use crate::client_core::MdNotice;
        for notice in notices {
            match notice {
                MdNotice::MarketDataType { req_id, market_data_type } =>
                    call_wrapper!(self.wrapper, py, "market_data_type", (req_id, market_data_type)),
                MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions } =>
                    call_wrapper!(self.wrapper, py, "tick_req_params", (req_id, min_tick, bbo_exchange.as_str(), permissions)),
                MdNotice::Error { req_id, code, text } =>
                    call_wrapper!(self.wrapper, py, "error", (req_id, code, text.as_str(), "")),
                MdNotice::News { req_id, news } =>
                    call_wrapper!(self.wrapper, py, "tick_news", (req_id, news.timestamp, news.provider_code.as_str(),
                        news.article_id.as_str(), news.headline.as_str(), news.extra_data.as_str())),
            }
        }
        Ok(())
    }

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
                let c_py = Py::new(py, Contract::from_api(py, &ac)?)?.into_any();
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
            let c_py = Py::new(py, Contract::from_api(py, &ac)?)?.into_any();
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
    fn send_open_order(&self, py: Python<'_>, order_id: OrderId, view: &crate::client_core::OrderView) -> PyResult<()> {
        // A combo's contract with its legs (ibx#470).
        let c = Contract::from_api(py, &view.contract)?;
        // The order as the Rust client's openOrder shows it (ibx#487), a
        // combo's per-leg prices and routing with it (ibx#470).
        let mut o = Order::from_api(py, &view.order)?;
        o.order_id = order_id;
        let mut state = OrderState::default();
        state.status = view.state.status.clone();
        state.commission_and_fees = view.state.commission_and_fees;
        state.completed_time = view.state.completed_time.clone();
        state.completed_status = view.state.completed_status.clone();
        let c_py = Py::new(py, c)?.into_any();
        let o_py = Py::new(py, o)?.into_any();
        let state_py = Py::new(py, state)?.into_any();
        call_wrapper!(self.wrapper, py, "open_order", (order_id, &c_py, &o_py, &state_py));
        Ok(())
    }

    /// open_order + order_status for every report of a known order; a
    /// cancel gives order_status only (ibx#473).
    fn report_order_update(&self, py: Python<'_>, shared: &Arc<SharedState>, update: &crate::types::OrderUpdate) -> PyResult<()> {
        let status = order_status_str(update.status);
        let filled = update.filled_qty_fixed as f64 / QTY_SCALE_F;
        let remaining = update.remaining_qty_fixed as f64 / QTY_SCALE_F;
        let view = self.core.order_view(update.order_id, shared, status);
        let (last_fill_price, client_id) = view.as_ref().map(|v| (v.last_fill_price, v.client_id)).unwrap_or((0.0, 0));
        let why_held = self.core.why_held(status, view.as_ref().map_or("", |v| v.order.order_type.as_str()), update.parent_id);
        // A cancel, and an order that never left, give the status only.
        let view = view.filter(|_| !matches!(status, "Cancelled" | "ApiCancelled"));
        let report = crate::client_core::OrderReport {
            view, status: status.into(), filled, remaining,
            avg_fill_price: update.avg_fill_price as f64 / PRICE_SCALE_F,
            perm_id: update.perm_id, parent_id: update.parent_id, last_fill_price, client_id, why_held,
        };
        self.repeat_order_report(py, update.order_id, &report)?;
        self.core.remember_report(update.order_id, report);
        // Track open orders
        self.core.update_order_status(update.order_id, status, filled, remaining);
        Ok(())
    }

    /// open_order (when the report has one) and order_status of a report.
    fn repeat_order_report(&self, py: Python<'_>, order_id: OrderId, r: &crate::client_core::OrderReport) -> PyResult<()> {
        let order_id = self.shared_state().map(|s| s.orders.api_order_id(order_id)).unwrap_or(order_id);
        if let Some(v) = &r.view {
            self.send_open_order(py, order_id, v)?;
        }
        let (avg, last, mkt_cap) = crate::client_core::status_prices(r);
        call_wrapper!(self.wrapper, py, "order_status", (order_id, r.status.as_str(), r.filled, r.remaining,
             avg, r.perm_id, r.parent_id, last, r.client_id, r.why_held.as_str(), mkt_cap));
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
        // Drain engine events. The lock is held only to drain, never across
        // a callback, so a callback may call disconnect() or connect()
        // (ibx#268). A stopped engine ends the session: run() exits and
        // fires connection_closed, with no error code. A lost link is not
        // this event; it comes as a connection notice below.
        let engine_stopped = {
            let guard = self.event_rx.lock().unwrap();
            guard.as_ref().is_some_and(|rx| {
                rx.try_iter().fold(false, |stopped, event| stopped | matches!(event, Event::Disconnected))
            })
        };
        if engine_stopped {
            self.connected.store(false, Ordering::Release);
        }

        // Link lost / restored and farm status (ibx#399): errors with id -1.
        // The session stays open across a lost link.
        for (code, msg) in shared.drain_connection_notices() {
            call_wrapper!(self.wrapper, py, "error", (-1i64, code, msg.as_str(), ""));
        }

        // Open-order requests held while the auth link was lost: taken
        // before the order updates and answered after them, so the answer
        // has the replayed statuses (ibx#251).
        let released = self.core.released_open_orders(shared);

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
                order_id: fill.order_id,
                cum_qty,
                avg_price,
                ..Default::default()
            };
            self.core.apply_fill_exec(&mut api_exec, &fill_exec, fill.order_id);
            // The order id the reference shows for the order.
            let shown = shared.orders.api_order_id(fill.order_id);
            api_exec.order_id = shown;
            // A combo's report shows the combo or the leg (ibx#470).
            let mut api_contract = api_contract;
            crate::client_core::ClientCore::apply_combo_exec(&fill_exec, &mut api_contract, &mut api_exec);

            // Build Python contract for callback
            let exec_contract = Contract::from_api(py, &api_contract)?;

            let c_py = Py::new(py, exec_contract)?.into_any();
            let exec_obj = Execution {
                exec_id: api_exec.exec_id.clone(),
                time: api_exec.time.clone(),
                acct_number: api_exec.acct_number.clone(),
                exchange: api_exec.exchange.clone(),
                side: api_exec.side.clone(),
                shares: api_exec.shares,
                price: api_exec.price,
                perm_id,
                client_id: api_exec.client_id,
                order_id: shown,
                liquidation: 0,
                cum_qty: api_exec.cum_qty,
                avg_price: api_exec.avg_price,
                order_ref: api_exec.order_ref.clone(),
                model_code: api_exec.model_code.clone(),
                last_liquidity: api_exec.last_liquidity,
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

            // The execution first, then openOrder and orderStatus for every
            // report of a known order, as the reference (ibx#473; captured
            // 30/09/2026 on a stock and a combo fill).
            let mut view = self.core.order_view(fill.order_id, shared, status);
            crate::client_core::ClientCore::report_client(&mut view, &fill_exec);
            let client_id = match &view {
                Some(view) => {
                    self.send_open_order(py, shown, view)?;
                    view.client_id
                }
                None => 0,
            };
            let why_held = self.core.why_held(status, view.as_ref().map_or("", |v| v.order.order_type.as_str()), parent_id);
            call_wrapper!(self.wrapper, py, "order_status", (shown, status, cum_qty, remaining,
                 avg_price, perm_id, parent_id, price, client_id, why_held.as_str(), 0.0f64));
            self.core.remember_report(fill.order_id, crate::client_core::OrderReport {
                view, status: status.into(), filled: cum_qty, remaining, avg_fill_price: avg_price,
                perm_id, parent_id, last_fill_price: price, client_id, why_held,
            });
            self.core.record_last_fill_price(fill.order_id, price);

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, cum_qty, remaining);
        }

        // Orders filled while the auth link was lost: no longer known to
        // the client, with no callback (ibx#251).
        for order_id in shared.orders.drain_forgotten_orders() {
            self.core.forget_order(order_id);
        }

        // Executions of orders this session does not track: stored for
        // req_executions, with no live callback (ibx#314).
        for (contract, mut exec, fill_exec) in shared.orders.drain_untracked_executions() {
            let order_id = exec.order_id;
            self.core.apply_fill_exec(&mut exec, &fill_exec, order_id);
            // An execution of another client's order: nothing for this one.
            if fill_exec.other_client {
                self.core.push_silent_execution(contract, exec, fill_exec.time_secs);
                continue;
            }
            if let Some(cr) = self.core.push_execution(-1, contract, exec, fill_exec.time_secs) {
                self.send_commission_report(py, &cr)?;
            }
        }

        // Commission reports, sent once their execution is known (ibx#471).
        for cr in shared.orders.drain_commission_reports() {
            if self.core.apply_commission(&cr) {
                // The order's openOrder and orderStatus once more first
                // (ibx#486).
                if let Some((order_id, last)) = self.core.report_of_commission(&cr) {
                    self.repeat_order_report(py, order_id, &last)?;
                }
                self.send_commission_report(py, &cr)?;
            }
        }

        // Order errors (refused before sending, warnings of a report) ->
        // error, ahead of the status.
        for (order_id, code, msg) in shared.orders.drain_order_errors() {
            self.core.note_order_error(order_id, code);
            call_wrapper!(self.wrapper, py, "error", (shared.orders.api_order_id(order_id), code, msg.as_str(), ""));
        }

        // Drain order updates -> orderStatus
        let mut reported = std::collections::HashMap::new();
        for update in shared.orders.drain_order_updates() {
            self.report_order_update(py, shared, &update)?;
            reported.insert(update.order_id, update);
        }

        // A server reject (201) and a cancel (202) after the status of
        // their report, as the reference writes them; a reject then gives
        // the order and its status once more (ibx#486).
        for (order_id, code, msg) in shared.orders.drain_order_notices() {
            call_wrapper!(self.wrapper, py, "error", (shared.orders.api_order_id(order_id), code, msg.as_str(), ""));
            if let Some(update) = reported.get(&order_id).filter(|_| code == 201) {
                self.report_order_update(py, shared, update)?;
            }
        }

        // A server reject of a cancel or modify gives no callback, as the
        // reference: no error, no status; the order status that answers the
        // engine's status request sets the state (ibx#252).
        shared.orders.drain_cancel_rejects();

        for request in released {
            if let Err(e) = self.answer_open_orders(py, shared, request) {
                callback_raised(py, "open_order", e)?;
            }
        }

        // Requests that joined a subscription, and subscriptions the
        // server rejected (ibx#444, ibx#447).
        let (notices, commands) = self.core.take_md_rejects(shared);
        self.md_notices(py, notices)?;
        if let Ok(tx) = self.tx() {
            for command in commands {
                let _ = send_cmd(py, &tx, command);
            }
            // Requests that waited for a market data line (101) take the
            // lines set free (ibx#444).
            py.detach(|| self.core.promote_waiting_md(shared, &tx));
        }

        // Regulatory snapshots that ended (ibx#446).
        if let Ok(tx) = self.tx() {
            for (req_id, result) in self.core.poll_regulatory_snapshots(shared, &tx) {
                use crate::control::regsnapshot::SnapshotTick;
                match result {
                    Ok(ticks) => {
                        for t in ticks {
                            match t {
                                SnapshotTick::Price { tick_type, price } => {
                                    let attrib_obj = Py::new(py, TickAttrib::default())?.into_any();
                                    call_wrapper!(self.wrapper, py, "tick_price", (req_id, tick_type, price, &attrib_obj));
                                }
                                SnapshotTick::Size { tick_type, size } =>
                                    call_wrapper!(self.wrapper, py, "tick_size", (req_id, tick_type, size)),
                                SnapshotTick::Text { tick_type, value } =>
                                    call_wrapper!(self.wrapper, py, "tick_string", (req_id, tick_type, value.as_str())),
                            }
                        }
                        call_wrapper!(self.wrapper, py, "tick_snapshot_end", (req_id,));
                    }
                    Err((code, text)) => call_wrapper!(self.wrapper, py, "error", (req_id, code, text.as_str(), "")),
                }
            }
        }

        // What the requests that joined a running subscription get at once
        // (ibx#444).
        self.md_notices(py, self.core.take_md_joins())?;

        // Request parameters, once per request (ibx#449), after the market
        // data type (ibx#446).
        for (req_id, mdt, min_tick, bbo_exchange, permissions) in self.core.take_tick_req_params(shared) {
            if let Some(mdt) = mdt {
                call_wrapper!(self.wrapper, py, "market_data_type", (req_id, mdt));
            }
            call_wrapper!(self.wrapper, py, "tick_req_params", (req_id, min_tick, bbo_exchange.as_str(), permissions));
        }

        // The market data steps queued by the engine -> tick callbacks, in
        // their order (via ClientCore, as the Rust dispatch; ibx#446).
        let mut out = std::mem::take(&mut *self.core.md_out.lock().unwrap());
        self.core.poll_market_data(shared, None, &mut out);
        let mut snapshot_done: Vec<i64> = Vec::new();
        let mut text = [0u8; 24];
        for item in out.drain(..) {
            match item {
                MdOut::Tick(req_id, tick) => match tick {
                    MdTick::Price { tick_type, value, can_auto_execute } => {
                        let attrib = TickAttrib { can_auto_execute, past_limit: false, pre_open: false };
                        let attrib_obj = Py::new(py, attrib)?.into_any();
                        call_wrapper!(self.wrapper, py, "tick_price", (req_id, tick_type, value, &attrib_obj));
                    }
                    MdTick::Size { tick_type, value } =>
                        call_wrapper!(self.wrapper, py, "tick_size", (req_id, tick_type, value)),
                    MdTick::Text { tick_type, value } =>
                        call_wrapper!(self.wrapper, py, "tick_string", (req_id, tick_type, value.as_str())),
                    MdTick::Time { tick_type, secs } => {
                        let value = crate::client_core::epoch_text(secs, &mut text);
                        call_wrapper!(self.wrapper, py, "tick_string", (req_id, tick_type, value));
                    }
                    MdTick::Generic { tick_type, value } =>
                        call_wrapper!(self.wrapper, py, "tick_generic", (req_id, tick_type, value)),
                },
                MdOut::MarketDataType(req_id, mdt) =>
                    call_wrapper!(self.wrapper, py, "market_data_type", (req_id, mdt)),
                MdOut::SnapshotEnd(req_id) => {
                    call_wrapper!(self.wrapper, py, "tick_snapshot_end", (req_id,));
                    snapshot_done.push(req_id);
                }
            }
        }
        *self.core.md_out.lock().unwrap() = out;
        // A snapshot cancelled by the client before its end was read is
        // gone already.
        for req_id in snapshot_done {
            if self.core.req_to_instrument.lock().unwrap().contains_key(&req_id) {
                self.cancel_mkt_data(py, req_id)?;
            }
        }

        // Tick-by-tick requests that ended with an error (10189, 10190):
        // the engine already let them go (ibx#455).
        for (req_id, code, text) in shared.market.drain_tbt_errors() {
            call_wrapper!(self.wrapper, py, "error", (req_id, code as i64, text.as_str(), ""));
            self.core.unregister_tbt(req_id);
        }

        // Drain TBT trades -> tickByTickAllLast, tickType 1 for Last and 2
        // for AllLast, with the entry's attributes (ibx#404, ibx#455)
        let tbt_trades = shared.market.drain_tbt_trades();
        for trade in tbt_trades {
            let price = trade.price as f64 / PRICE_SCALE_F;
            let size = trade.size as f64;
            let attrib = super::super::tick_types::TickAttribLast { past_limit: trade.past_limit, unreported: trade.unreported };
            let attrib_obj = Py::new(py, attrib)?.into_any();
            call_wrapper!(self.wrapper, py, "tick_by_tick_all_last", (trade.req_id, trade.tbt_type.api_tick_type(), trade.timestamp as i64, price, size,
                 &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()));
        }

        // Drain TBT quotes -> tickByTickBidAsk
        let tbt_quotes = shared.market.drain_tbt_quotes();
        for quote in tbt_quotes {
            let attrib = super::super::tick_types::TickAttribBidAsk { bid_past_low: quote.bid_past_low, ask_past_high: quote.ask_past_high };
            let attrib_obj = Py::new(py, attrib)?.into_any();
            call_wrapper!(self.wrapper, py, "tick_by_tick_bid_ask", (quote.req_id, quote.timestamp as i64,
                 quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                 quote.bid_size as f64, quote.ask_size as f64, &attrib_obj));
        }

        // Drain TBT midpoints -> tickByTickMidPoint (ibx#404)
        for mid in shared.market.drain_tbt_mid_points() {
            call_wrapper!(self.wrapper, py, "tick_by_tick_mid_point", (mid.req_id, mid.timestamp as i64,
                 mid.mid_point as f64 / PRICE_SCALE_F));
        }

        // Drain depth updates -> updateMktDepth / updateMktDepthL2, as the
        // book says (#451)
        let depth_updates = shared.market.drain_depth_updates();
        for du in depth_updates {
            if !du.l2 {
                call_wrapper!(self.wrapper, py, "update_mkt_depth", (du.req_id, du.position, du.operation, du.side, du.price, du.size));
            } else {
                call_wrapper!(self.wrapper, py, "update_mkt_depth_l2", (du.req_id, du.position, du.market_maker.as_str(),
                     du.operation, du.side, du.price, du.size, du.is_smart_depth));
            }
        }

        // Drain news -> tickNews, to each request of the contract whose
        // news key has the provider (ibx#444)
        let news_items = shared.market.drain_tick_news();
        for news in news_items {
            for req_id in self.core.route_tick_news(&news) {
                call_wrapper!(self.wrapper, py, "tick_news", (req_id, news.timestamp, news.provider_code.as_str(),
                     news.article_id.as_str(), news.headline.as_str(), news.extra_data.as_str()));
            }
        }

        // Drain news bulletins -> updateNewsBulletin
        // (popups always, every type when subscribed, ibx#461)
        {
            let bulletins = self.core.bulletins_to_deliver(shared);
            for b in bulletins {
                call_wrapper!(self.wrapper, py, "update_news_bulletin", (b.msg_id as i64, b.msg_type, b.message.as_str(), b.exchange.as_str()));
            }
        }

        // Drain what-if responses -> open_order(contract, order, OrderState)
        // only, as the reference answers a preview; a refused one then gets
        // error 201 (ibx#462).
        let what_ifs = shared.orders.drain_what_if_responses();
        for wi in what_ifs {
            let s = crate::client_core::ClientCore::what_if_order_state(&wi.state);
            let state = OrderState {
                status: s.status,
                init_margin_before: s.init_margin_before,
                maint_margin_before: s.maint_margin_before,
                equity_with_loan_before: s.equity_with_loan_before,
                init_margin_change: s.init_margin_change,
                maint_margin_change: s.maint_margin_change,
                equity_with_loan_change: s.equity_with_loan_change,
                init_margin_after: s.init_margin_after,
                maint_margin_after: s.maint_margin_after,
                equity_with_loan_after: s.equity_with_loan_after,
                commission_and_fees: s.commission_and_fees,
                min_commission_and_fees: s.min_commission_and_fees,
                max_commission_and_fees: s.max_commission_and_fees,
                commission_and_fees_currency: s.commission_and_fees_currency,
                warning_text: s.warning_text,
                margin_currency: s.margin_currency,
                init_margin_before_outside_rth: s.init_margin_before_outside_rth,
                maint_margin_before_outside_rth: s.maint_margin_before_outside_rth,
                equity_with_loan_before_outside_rth: s.equity_with_loan_before_outside_rth,
                init_margin_change_outside_rth: s.init_margin_change_outside_rth,
                maint_margin_change_outside_rth: s.maint_margin_change_outside_rth,
                equity_with_loan_change_outside_rth: s.equity_with_loan_change_outside_rth,
                init_margin_after_outside_rth: s.init_margin_after_outside_rth,
                maint_margin_after_outside_rth: s.maint_margin_after_outside_rth,
                equity_with_loan_after_outside_rth: s.equity_with_loan_after_outside_rth,
                suggested_size: s.suggested_size,
                ..Default::default()
            };

            let tracked = if wi.final_reply {
                self.core.take_what_if(wi.order_id)
            } else {
                self.core.peek_what_if(wi.order_id)
            };
            let (contract_py, order_py) = if let Some((mut contract, mut order)) = tracked {
                // A preview placed without a conId shows the contract
                // looked up (ibx#486).
                if contract.con_id == 0 && wi.state.con_id != 0 && !contract.sec_type.eq_ignore_ascii_case("BAG") {
                    contract = self.core.get_contract(wi.state.con_id, shared).unwrap_or(contract);
                }
                // The order as the reference shows it (its unset values); a
                // combo shows its combo (ibx#470).
                crate::client_core::reported_unset_values(&mut order);
                crate::client_core::ClientCore::apply_combo_view(wi.order_id, &mut contract, &mut order, shared);
                let c = Contract::from_api(py, &contract)?;
                let mut o = Order::from_api(py, &order)?;
                // The account and the client id, as the reference's (ibx#486).
                o.account = if order.account.is_empty() { self.account() } else { order.account };
                o.client_id = self.core.client_id.load(std::sync::atomic::Ordering::Relaxed) as i32;
                // The permId of the preview's order (ibx#486).
                o.perm_id = wi.state.perm_id;
                (Py::new(py, c)?.into_any(), Py::new(py, o)?.into_any())
            } else {
                (Py::new(py, Contract::default())?.into_any(),
                 Py::new(py, Order::default())?.into_any())
            };
            let state_py = Py::new(py, state)?.into_any();
            call_wrapper!(self.wrapper, py, "open_order",
                (wi.order_id, &contract_py, &order_py, &state_py));
            if !wi.state.reject_reason.is_empty() {
                let text = format!("Order rejected - reason:{}", wi.state.reject_reason);
                call_wrapper!(self.wrapper, py, "error", (wi.order_id, 201i64, text.as_str(), ""));
            }
        }

        // Drain HMDS query errors -> error (ibx#186). Surface gateway-side validation
        // failures (e.g. "Invalid time length") that previously vanished silently.
        for (req_id, code, msg) in shared.reference.drain_historical_errors() {
            call_wrapper!(self.wrapper, py, "error", (req_id, code as i64, msg.as_str(), ""));
        }

        // Smart components that waited for their exchange map (ibx#441).
        for (req_id, answer) in self.core.take_smart_components(shared) {
            self.deliver_smart_components(py, req_id, answer)?;
        }

        // Drain historical data -> historicalData + historicalDataEnd
        let hist_data = shared.reference.drain_historical_data();
        for (req_id, response) in hist_data {
            for bar in &response.bars {
                let bar_obj = BarData::reported(
                    bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count,
                    response.timezone.clone(),
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                call_wrapper!(self.wrapper, py, "historical_data", (req_id, &bar_py));
            }
            if response.is_complete {
                call_wrapper!(self.wrapper, py, "historical_data_end", (req_id, response.start.as_str(), response.end.as_str()));
            }
        }

        // keepUpToDate: the whole current bar each time (ibx#429).
        for (req_id, bar) in shared.reference.drain_historical_updates() {
            let bar_obj = BarData::reported(
                bar.time, bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count, String::new(),
            );
            let bar_py = Py::new(py, bar_obj)?.into_any();
            call_wrapper!(self.wrapper, py, "historical_data_update", (req_id, &bar_py));
        }

        // Drain head timestamps -> headTimestamp
        let head_ts = shared.reference.drain_head_timestamps();
        for (req_id, response) in head_ts {
            call_wrapper!(self.wrapper, py, "head_timestamp",
                (req_id, response.head_timestamp.as_str()));
        }

        // Drain contract details -> contractDetails + contractDetailsEnd
        let contract_defs = shared.reference.drain_contract_details();
        for (req_id, def) in contract_defs {
            let details = ContractDetails::from_definition(py, &def);
            let details_py = Py::new(py, details)?.into_any();
            // A bond row is a bond contract details message (ibx#438).
            let callback = if def.is_bond() {
                "bond_contract_details"
            } else {
                "contract_details"
            };
            call_wrapper!(self.wrapper, py, callback, (req_id, &details_py));
        }
        let contract_ends = shared.reference.drain_contract_details_end();
        for req_id in contract_ends {
            call_wrapper!(self.wrapper, py, "contract_details_end", (req_id,));
        }

        // Drain matching symbols -> symbolSamples
        let symbol_results = shared.reference.drain_matching_symbols();
        for (req_id, matches) in symbol_results {
            let descriptions: Vec<Py<ContractDescription>> = matches.iter().map(|m| {
                Py::new(py, ContractDescription {
                    con_id: m.con_id,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.clone(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                    description: m.description.clone(),
                    issuer_id: m.issuer_id.clone(),
                }).unwrap()
            }).collect();
            let list = pyo3::types::PyList::new(py, &descriptions)?;
            call_wrapper!(self.wrapper, py, "symbol_samples", (req_id, list.as_any()));
        }

        // Option chains -> securityDefinitionOptionParameter per row, with
        // sets as the reference client, then the end (ibx#440).
        for (req_id, rows) in shared.reference.drain_option_chains() {
            for r in &rows {
                let expirations = pyo3::types::PySet::new(py, &r.expirations)?;
                let strikes = pyo3::types::PySet::new(py, &r.strikes)?;
                call_wrapper!(self.wrapper, py, "security_definition_option_parameter",
                    (req_id, r.exchange.as_str(), r.underlying_con_id, r.trading_class.as_str(), r.multiplier.as_str(),
                     expirations.as_any(), strikes.as_any()));
            }
            call_wrapper!(self.wrapper, py, "security_definition_option_parameter_end", (req_id,));
        }

        // Drain depth exchanges -> mktDepthExchanges
        if let Some(depth_exchanges) = shared.reference.drain_depth_exchanges() {
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
                    contract.con_id = entry.con_id;
                    // Look up cached contract for symbol info
                    if let Some(ac) = self.core.get_contract(entry.con_id, shared) {
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
                call_wrapper!(self.wrapper, py, "scanner_data", (req_id, rank as i32, &cd_py,
                    entry.distance.as_str(), entry.benchmark.as_str(), entry.projection.as_str(), entry.legs.as_str()));
            }
            call_wrapper!(self.wrapper, py, "scanner_data_end", (req_id,));
        }

        // Drain historical news -> historicalNews + historicalNewsEnd
        let news_results = shared.reference.drain_historical_news();
        for (req_id, headlines, has_more) in news_results {
            for h in &headlines {
                call_wrapper!(self.wrapper, py, "historical_news", (req_id, h.time.as_str(), h.provider_code.as_str(),
                     h.article_id.as_str(), h.headline.as_str()));
            }
            call_wrapper!(self.wrapper, py, "historical_news_end", (req_id, has_more));
        }

        // Drain news articles -> newsArticle
        let articles = shared.reference.drain_news_articles();
        for (req_id, article_type, text) in articles {
            call_wrapper!(self.wrapper, py, "news_article", (req_id, article_type, text.as_str()));
        }

        // Drain option calculations -> tickOptionComputation; a field that
        // is not computed is None, as the official Python client gives it.
        for a in shared.reference.drain_option_computations() {
            let v = |x: f64| (x != f64::MAX).then_some(x);
            call_wrapper!(self.wrapper, py, "tick_option_computation", (
                a.req_id, a.tick_type, a.tick_attrib, v(a.implied_vol), v(a.delta), v(a.opt_price),
                v(a.pv_dividend), v(a.gamma), v(a.vega), v(a.theta), v(a.und_price),
            ));
        }

        // Drain fundamental data -> fundamentalData
        let fundamentals = shared.reference.drain_fundamental_data();
        for (req_id, data) in fundamentals {
            call_wrapper!(self.wrapper, py, "fundamental_data", (req_id, data.as_str()));
        }

        // Drain histogram data -> histogram_data
        let histograms = shared.reference.drain_histogram_data();
        for (req_id, entries) in histograms {
            let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
            }).collect();
            let py_list = pyo3::types::PyList::new(py, tuples)?;
            call_wrapper!(self.wrapper, py, "histogram_data", (req_id, py_list));
        }

        // Drain historical ticks -> the official tick objects (ibx#432)
        let hist_ticks = shared.reference.drain_historical_ticks();
        for (req_id, data, _what, done) in hist_ticks {
            use super::super::tick_types::{HistoricalTick, HistoricalTickBidAsk, HistoricalTickLast, TickAttribBidAsk, TickAttribLast};
            match data {
                crate::types::HistoricalTickData::Midpoint(ticks) => {
                    let objs = ticks.iter().map(|t| Py::new(py, HistoricalTick { time: t.time, price: t.price, size: t.size }))
                        .collect::<PyResult<Vec<_>>>()?;
                    let list = pyo3::types::PyList::new(py, objs)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks", (req_id, list, done));
                }
                crate::types::HistoricalTickData::Last(ticks) => {
                    let objs = ticks.iter().map(|t| {
                        let attrib = Py::new(py, TickAttribLast {
                            past_limit: t.tick_attrib_last.past_limit,
                            unreported: t.tick_attrib_last.unreported,
                        })?;
                        Py::new(py, HistoricalTickLast {
                            time: t.time, tick_attrib_last: attrib, price: t.price, size: t.size,
                            exchange: t.exchange.clone(), special_conditions: t.special_conditions.clone(),
                        })
                    }).collect::<PyResult<Vec<_>>>()?;
                    let list = pyo3::types::PyList::new(py, objs)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks_last", (req_id, list, done));
                }
                crate::types::HistoricalTickData::BidAsk(ticks) => {
                    let objs = ticks.iter().map(|t| {
                        let attrib = Py::new(py, TickAttribBidAsk {
                            bid_past_low: t.tick_attrib_bid_ask.bid_past_low,
                            ask_past_high: t.tick_attrib_bid_ask.ask_past_high,
                        })?;
                        Py::new(py, HistoricalTickBidAsk {
                            time: t.time, tick_attrib_bid_ask: attrib, price_bid: t.price_bid, price_ask: t.price_ask,
                            size_bid: t.size_bid, size_ask: t.size_ask,
                        })
                    }).collect::<PyResult<Vec<_>>>()?;
                    let list = pyo3::types::PyList::new(py, objs)?;
                    call_wrapper!(self.wrapper, py, "historical_ticks_bid_ask", (req_id, list, done));
                }
            }
        }

        // Drain real-time bars -> real_time_bar
        let rtbars = shared.market.drain_real_time_bars();
        for (req_id, bar) in rtbars {
            call_wrapper!(self.wrapper, py, "real_time_bar", (
                req_id,
                bar.timestamp as i64,
                bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count,
            ));
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
                req_id,
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
                let c = crate::python::compat::contract::Contract::from_api(py, &ac)?;
                let c_py = pyo3::Py::new(py, c)?.into_any();
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
        // Its sends may wait: interpreter lock released, and only when a
        // P&L request is running (ibx#271).
        if !self.core.pnl_quotes_idle() {
            if let Ok(tx) = self.tx() {
                py.detach(|| self.core.maintain_pnl_quotes(shared, &tx));
            }
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
                let account = batch.account.as_deref().unwrap_or(acct_name.as_str());
                for row in &batch.rows {
                    call_wrapper!(self.wrapper, py, "account_summary", (batch.req_id, account, row.key.as_str(), row.value.as_str(), row.currency.as_str()));
                }
                if batch.end {
                    call_wrapper!(self.wrapper, py, "account_summary_end", (batch.req_id,));
                }
            }
        }

        Ok(())
    }
}
