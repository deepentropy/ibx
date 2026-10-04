//! Order placement, cancellation, open orders, executions, completed orders.

use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::api::types::{
    Contract as ApiContract, Order as ApiOrder, ExecutionFilter,
};
use crate::client_core::{ClientCore, ModifyPlan};
use crate::bridge::SharedState;
use crate::types::*;
use super::{send_cmd, EClient};
use super::super::contract::{Contract, Order, CommissionAndFeesReport, Execution};

#[pymethods]
impl EClient {
    /// Place an order.
    fn place_order(&self, py: Python<'_>, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        // Convert and validate order params first (fail fast, no connection needed)
        let mut api_order = order.to_api();
        api_order.conditions = order.convert_conditions(py);
        api_order.order_combo_legs = order.convert_order_combo_legs(py);
        // The contract with its combo legs (ibx#470).
        let mut full_contract = contract.to_api();
        full_contract.combo_legs = contract.convert_combo_legs(py);
        // The reference's other names for an order type (ibx#469).
        if let Some(name) = ClientCore::canonical_order_type(&api_order.order_type) {
            api_order.order_type = name.to_string();
        }
        // GTX and NMIN go out as GTC, as the reference (ibx#307).
        if let Some(name) = ClientCore::canonical_tif(&api_order.tif) {
            api_order.tif = name.to_string();
        }
        ClientCore::validate_order(&api_order)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        ClientCore::validate_order_contract(&contract.sec_type)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        // After the checks above, which refuse an invalid order even with no
        // connection (ibx#115).
        if let Some(r) = self.not_connected(order_id) { return r; }

        let tx = self.tx()?;

        // The id as given: the reference refuses 0 with 10149 below.
        let oid = order_id;

        // Warnings the reference sends while it reads the order (ibx#416).
        let shared = self.shared_state()?;
        for (code, message) in ClientCore::implied_zone_warnings(&api_order) {
            shared.orders.push_order_error(oid, code, message);
        }
        // Refused before sending, like the reference: error() only.
        let session_account = self.account_id.lock().unwrap().clone().unwrap_or_default();
        let contract_zone = shared.reference.time_zone_id(contract.con_id);
        let account_pending = ClientCore::order_account_pending(&api_order, &shared.reference, &session_account);
        // The algo check's warnings come before its refusal (ibx#263).
        let mut algo_warnings = Vec::new();
        let refusal = ClientCore::refusal_before_sending_for(&api_order, &contract.exchange, account_pending)
            .or_else(|| ClientCore::algo_definition_refusal(&api_order, &contract.exchange, &shared.reference, &mut algo_warnings))
            .or_else(|| ClientCore::account_config_refusal(
                &api_order, shared.reference.account_features().as_deref(), &session_account))
            .or_else(|| ClientCore::good_till_date_refusal(&api_order, contract_zone.as_deref()))
            .or_else(|| ClientCore::condition_time_zone_refusal(&api_order, contract_zone.as_deref()))
            .or_else(|| ClientCore::price_refusal(&api_order))
            .or_else(|| ClientCore::order_id_refusal(oid))
            .or_else(|| self.core.refusal_for_order_id(oid, &api_order, &shared));
        for (code, message) in algo_warnings {
            shared.orders.push_order_error(oid, code, message);
        }
        if let Some((code, message)) = refusal {
            shared.orders.push_order_error(oid, code, message);
            return Ok(());
        }
        // A combo (BAG) order, read and checked as the reference reads it
        // (ibx#470).
        let combo = match ClientCore::combo_order(&full_contract, &api_order, &shared.reference, &session_account) {
            Ok(combo) => combo,
            Err((code, message)) => {
                shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        };
        // The condition times as the reference sends them (ibx#416); the
        // order is tracked as the caller placed it.
        let sent = ClientCore::with_condition_times(&api_order);

        // A smart combo goes out on its currency's smart combo conId.
        let con_id = combo.as_ref().map(|c| c.smart_con_id).filter(|&c| c > 0).unwrap_or(contract.con_id);
        // A contract without a conId is looked up by the engine before the
        // order goes out (ibx#486).
        let instrument = if con_id == 0 && !contract.sec_type.eq_ignore_ascii_case("BAG") {
            py.detach(|| self.core.order_instrument(&tx, oid, api_order.what_if, con_id, &contract.symbol,
                &contract.exchange, &contract.sec_type, &contract.currency)).map_err(PyRuntimeError::new_err)?
        } else {
            self.find_or_register_con_id(py, con_id, contract)?
        };
        // A send only for a new currency, then with the interpreter lock
        // released (ibx#271).
        if !self.core.currency_noted(con_id, &contract.currency) {
            py.detach(|| self.core.note_currency(&tx, con_id, &contract.currency));
        }

        // If orderId is already tracked, this is a modification: replace it
        // with the full wanted state (ibx#247). A what-if never modifies:
        // it previews a new order (ibx#462).
        let working = if api_order.what_if { None } else { self.core.tracked_order(oid) };
        if working.is_some() {
            let refusal = self.core.tracked_contract(oid)
                .and_then(|placed| ClientCore::combo_modify_refusal(&full_contract, &placed));
            if let Some((code, message)) = refusal {
                shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        }
        let cmd = if let Some(working) = working {
            match ClientCore::build_modify_request(&sent, oid, &working)
                .map_err(|e| PyRuntimeError::new_err(e))?
            {
                ModifyPlan::Send(cmd) => cmd,
                ModifyPlan::Refused { code, message } => {
                    // Refused before sending, like the reference: the caller
                    // gets error() and the tracked order keeps its old state.
                    self.shared_state()?.orders.push_order_error(oid, code, message);
                    return Ok(());
                }
            }
        } else if let Some(combo) = combo {
            ClientCore::build_combo_order_request(&sent, oid, instrument, combo)
                .map_err(|e| PyRuntimeError::new_err(e))?
        } else {
            ClientCore::build_order_request(&sent, oid, instrument)
                .map_err(|e| PyRuntimeError::new_err(e))?
        };
        send_cmd(py, &tx, cmd)?;

        // Track order in shared core
        let api_contract = ApiContract {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            combo_legs: full_contract.combo_legs.clone(),
            ..Default::default()
        };
        let mut tracked_order = api_order.clone();
        tracked_order.order_id = oid;
        self.core.cache_contract(contract.con_id, api_contract.clone());
        if tracked_order.what_if {
            self.core.track_what_if(oid, api_contract, tracked_order);
        } else {
            self.core.track_order(oid, api_contract, tracked_order, instrument);
        }

        Ok(())
    }

    /// Cancel an order.
    #[pyo3(signature = (order_id, manual_order_cancel_time=""))]
    fn cancel_order(&self, py: Python<'_>, order_id: i64, manual_order_cancel_time: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::Order(OrderRequest::Cancel { order_id }))?;
        let _ = manual_order_cancel_time;
        Ok(())
    }

    /// Cancel all orders globally: every order of the account the session
    /// knows, those of other clients and of earlier sessions too, as the
    /// reference cancels them.
    fn req_global_cancel(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::Order(OrderRequest::GlobalCancel))
    }

    /// Request next valid order ID.
    #[pyo3(signature = (num_ids=1))]
    fn req_ids(&self, py: Python<'_>, num_ids: i32) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let next_id = self.next_order_id.load(Ordering::Relaxed);
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        let _ = num_ids;
        Ok(())
    }

    /// Get the next order ID (local counter, auto-increments).
    fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Request all open orders for this client.
    ///
    /// Before the order replay of the logon has ended, and while the auth
    /// link is lost, the request is answered only after the order replay,
    /// from the dispatch loop (ibx#251).
    fn req_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let shared = self.shared_state()?;
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::Open, &shared) {
            return Ok(());
        }
        self.answer_open_orders(py, &shared, crate::client_core::OpenOrdersRequest::Open)
    }

    /// Request all open orders across all clients. Held like
    /// `req_open_orders` until the order replay (ibx#251).
    fn req_all_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let shared = self.shared_state()?;
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::All, &shared) {
            return Ok(());
        }
        self.answer_open_orders(py, &shared, crate::client_core::OpenOrdersRequest::All)
    }

    /// Automatically bind future orders to this client.
    #[pyo3(signature = (b_auto_bind))]
    fn req_auto_open_orders(&self, b_auto_bind: bool) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let _ = b_auto_bind;
        Ok(())
    }

    /// Request execution reports.
    #[pyo3(signature = (req_id, exec_filter=None))]
    fn req_executions(&self, py: Python<'_>, req_id: i64, exec_filter: Option<Py<PyAny>>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_executions", &[req_id]) { return Ok(()); }
        let filter = if let Some(ref fobj) = exec_filter {
            let get = |attr: &str| -> String {
                fobj.getattr(py, pyo3::types::PyString::new(py, attr))
                    .and_then(|v| v.extract::<String>(py))
                    .unwrap_or_default()
            };
            ExecutionFilter {
                symbol: get("symbol"),
                sec_type: get("secType"),
                exchange: get("exchange"),
                side: get("side"),
                acct_code: get("acctCode"),
                time: get("time"),
                client_id: fobj.getattr(py, pyo3::types::PyString::new(py, "clientId"))
                    .and_then(|v| v.extract::<i64>(py))
                    .unwrap_or(0),
            }
        } else {
            ExecutionFilter::default()
        };

        // No lock is held during the callbacks (ibx#265). As the reference:
        // every execution, then the commission reports, then the end.
        let execs = self.core.matching_executions(&filter);
        for se in &execs {
            let c_py = Py::new(py, Contract::from_api(py, &se.contract)?)?.into_any();

            let exec_obj = Execution {
                exec_id: se.execution.exec_id.clone(),
                time: se.execution.time.clone(),
                acct_number: se.execution.acct_number.clone(),
                exchange: se.execution.exchange.clone(),
                side: se.execution.side.clone(),
                shares: se.execution.shares,
                price: se.execution.price,
                perm_id: se.execution.perm_id,
                client_id: se.execution.client_id,
                order_id: se.execution.order_id,
                liquidation: se.execution.liquidation,
                cum_qty: se.execution.cum_qty,
                avg_price: se.execution.avg_price,
                order_ref: se.execution.order_ref.clone(),
                ev_rule: se.execution.ev_rule.clone(),
                ev_multiplier: se.execution.ev_multiplier,
                model_code: se.execution.model_code.clone(),
                last_liquidity: se.execution.last_liquidity,
                pending_price_revision: se.execution.pending_price_revision,
                submitter: String::new(),
            };
            let exec_py = Py::new(py, exec_obj)?.into_any();

            self.wrapper.call_method(
                py, "exec_details",
                (req_id, &c_py, &exec_py),
                None,
            )?;
        }
        // The report exists once the server's commission frame came (ibx#471).
        for cr in execs.iter().filter_map(|se| se.commission_and_fees.as_ref()) {
            let report = CommissionAndFeesReport {
                exec_id: cr.exec_id.clone(),
                commission_and_fees: cr.commission_and_fees,
                currency: cr.currency.clone(),
                realized_pnl: cr.realized_pnl,
                yield_amount: cr.yield_amount,
                yield_redemption_date: cr.yield_redemption_date.clone(),
            };
            let report_py = Py::new(py, report)?.into_any();
            self.wrapper.call_method1(py, "commission_and_fees_report", (&report_py,))?;
        }
        self.wrapper.call_method1(py, "exec_details_end", (req_id,))?;
        Ok(())
    }

    /// Request completed orders.
    #[pyo3(signature = (api_only=false))]
    fn req_completed_orders(&self, py: Python<'_>, api_only: bool) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let _ = api_only;
        if let Some(shared) = self.shared.lock().unwrap().clone() {
            let completed = shared.orders.drain_completed_orders();
            for co in &completed {
                let status_str = crate::client_core::order_status_str(co.status);
                let rich_info = shared.orders.get_order_info(co.order_id);

                // Build OrderState iso with Rust API path (api/client/orders.rs:101-125):
                // start from rich_info.order_state when available, override status with the
                // canonical status_str, fall back to defaults otherwise.
                let state = if let Some(info) = rich_info.as_ref() {
                    let mut s = super::super::contract::OrderState::from_api(py, &info.order_state)?;
                    s.status = status_str.into();
                    s
                } else {
                    let mut s = super::super::contract::OrderState::default();
                    s.status = status_str.into();
                    s
                };
                let state_py = Py::new(py, state)?.into_any();

                let tracked = self.core.open_orders.lock().unwrap().get(&co.order_id)
                    .map(|o| (o.contract.clone(), o.order.clone()));
                // The order as the reference shows it (its unset values).
                if let Some((c, mut o)) = tracked.or_else(|| rich_info.map(|info| (info.contract, info.order))) {
                    crate::client_core::reported_unset_values(&mut o);
                    let c_py = Py::new(py, Contract::from_api(py, &c)?)?.into_any();
                    let o_py = Py::new(py, Order::from_api(py, &o)?)?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, &state_py))?;
                } else {
                    let c_py = Py::new(py, Contract::default())?.into_any();
                    let o_py = Py::new(py, Order::default())?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, &state_py))?;
                }
                // Bound `order_cache` growth: terminal entries are no longer
                // needed once delivered through `completed_order`.
                shared.orders.remove_order_info(co.order_id);
            }
            self.wrapper.call_method0(py, "completed_orders_end")?;
        }
        Ok(())
    }
}

impl EClient {
    /// The open orders, each as open_order then order_status, then the end
    /// of the list.
    pub(crate) fn answer_open_orders(&self, py: Python<'_>, shared: &SharedState, request: crate::client_core::OpenOrdersRequest) -> PyResult<()> {
        // In the book's order, with the order id and client id the
        // reference shows; OPEN_ORDER then ORDER_STATUS for each.
        let orders = self.core.open_orders_listing(shared, request);
        for (order_id, tracked, client_id) in &orders {
            // A combo with its legs (ibx#470).
            let c_py = Py::new(py, Contract::from_api(py, &tracked.contract)?)?.into_any();
            // A combo's per-leg prices and routing with it (ibx#470).
            let o_py = Py::new(py, Order::from_api(py, &tracked.order)?)?.into_any();
            let mut state = super::super::contract::OrderState::default();
            state.status = tracked.status.clone();
            let state_py = Py::new(py, state)?.into_any();
            self.wrapper.call_method(
                py, "open_order",
                (*order_id, &c_py, &o_py, &state_py),
                None,
            )?;
            let why_held = self.core.why_held(&tracked.status, &tracked.order.order_type, tracked.order.parent_id);
            self.wrapper.call_method(
                py, "order_status",
                (*order_id, tracked.status.as_str(), tracked.filled, tracked.remaining,
                 0.0f64, tracked.order.perm_id, tracked.order.parent_id, tracked.last_fill_price, *client_id, why_held.as_str(), 0.0f64),
                None,
            )?;
        }
        self.wrapper.call_method0(py, "open_order_end")?;
        Ok(())
    }
}
