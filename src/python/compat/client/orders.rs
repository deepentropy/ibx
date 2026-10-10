//! Order placement, cancellation, open orders, executions, completed orders.

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

impl EClient {
    /// An order placed with transmit off (ibx#509): nothing is sent but the
    /// lookup of a contract given without a conId, and nothing is answered.
    /// A working order placed again with transmit off stays as it is
    /// (paper 09/10/2026).
    fn hold_order(&self, py: Python<'_>, oid: i64, contract: &ApiContract, order: &ApiOrder) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let tx = self.tx()?;
        let shared = self.shared_state()?;
        if self.core.working_order(&shared, oid).is_some() {
            return Ok(());
        }
        // The order is read and checked now, as the reference does; one
        // that is refused is not held (ibx#547).
        let session_account = self.account_id.lock().unwrap().clone().unwrap_or_default();
        let (notices, refusal) = self.core.order_read_checks(
            oid, contract.con_id, &contract.exchange, order, &shared, &session_account);
        let combo = ClientCore::combo_order(contract, order, &shared.reference, &session_account);
        for (code, message) in notices {
            shared.orders.push_order_error(oid, code, message);
        }
        if let Some((code, message)) = refusal.or(combo.err()) {
            shared.orders.push_order_error(oid, code, message);
            return Ok(());
        }
        let instrument = if contract.sec_type.eq_ignore_ascii_case("BAG") { 0 } else {
            py.detach(|| self.core.order_instrument(&tx, oid, false, contract.con_id, &contract.symbol,
                &contract.exchange, &contract.sec_type, &contract.currency)).map_err(PyRuntimeError::new_err)?
        };
        send_cmd(py, &tx, ControlCommand::HoldOrder {
            order_id: oid, instrument,
            qty: (order.total_quantity * crate::types::QTY_SCALE as f64).round() as crate::types::Qty,
            parent_id: order.parent_id,
        })?;
        let mut held = order.clone();
        held.order_id = oid;
        self.core.hold_order(oid, contract.clone(), held, instrument);
        Ok(())
    }

    /// Send one order: a new order, or the change of a working one.
    fn send_order(&self, py: Python<'_>, order_id: i64, contract: &ApiContract, order: &ApiOrder, was_held: bool, built: &mut Vec<ControlCommand>) -> PyResult<()> {
        let tx = self.tx()?;
        let (api_order, full_contract) = (order, contract);

        // The id as given: the reference refuses 0 with 10149 below.
        let oid = order_id;

        // The warnings and the refusal of the order as it is read; the
        // warnings of an order that was held were given when it was placed
        // with transmit off (ibx#547).
        let shared = self.shared_state()?;
        let session_account = self.account_id.lock().unwrap().clone().unwrap_or_default();
        let (notices, refusal) = self.core.order_read_checks(
            oid, contract.con_id, &contract.exchange, api_order, &shared, &session_account);
        if !was_held {
            for (code, message) in notices {
                shared.orders.push_order_error(oid, code, message);
            }
        }
        if let Some((code, message)) = refusal {
            shared.orders.push_order_error(oid, code, message);
            return Ok(());
        }
        // A combo (BAG) order, read and checked as the reference reads it
        // (ibx#470).
        let combo = match ClientCore::combo_order(full_contract, api_order, &shared.reference, &session_account) {
            Ok(combo) => combo,
            Err((code, message)) => {
                shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        };
        // The condition times as the reference sends them (ibx#416); the
        // order is tracked as the caller placed it.
        let sent = ClientCore::with_condition_times(api_order);

        // A smart combo goes out on its currency's smart combo conId.
        let con_id = combo.as_ref().map(|c| c.smart_con_id).filter(|&c| c > 0).unwrap_or(contract.con_id);
        // A contract without a conId is looked up by the engine before the
        // order goes out (ibx#486).
        let instrument = if con_id == 0 && !contract.sec_type.eq_ignore_ascii_case("BAG") {
            py.detach(|| self.core.order_instrument(&tx, oid, api_order.what_if, con_id, &contract.symbol,
                &contract.exchange, &contract.sec_type, &contract.currency)).map_err(PyRuntimeError::new_err)?
        } else {
            let known = self.core.con_id_to_instrument.lock().unwrap().get(&con_id).copied();
            match known {
                Some(id) => id,
                None => py.detach(|| self.core.find_or_register_instrument(
                    &tx, con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
                )).map_err(PyRuntimeError::new_err)?,
            }
        };
        // A send only for a new currency, then with the interpreter lock
        // released (ibx#271).
        if !self.core.currency_noted(con_id, &contract.currency) {
            py.detach(|| self.core.note_currency(&tx, con_id, &contract.currency));
        }

        // If orderId is already tracked, this is a modification: replace it
        // with the full wanted state (ibx#247). A what-if never modifies:
        // it previews a new order (ibx#462).
        let working = if api_order.what_if { None } else { self.core.working_order(&shared, oid) };
        if working.is_some() {
            let refusal = self.core.tracked_contract(oid)
                .and_then(|placed| ClientCore::combo_modify_refusal(full_contract, &placed));
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
        built.push(cmd);

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
}

#[pymethods]
impl EClient {
    /// Place an order.
    fn place_order(&self, py: Python<'_>, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Convert and validate order params first (fail fast, no connection needed)
        let mut api_order = order.to_api();
        // A condition that is not understood ends the request here (ibx#541).
        api_order.conditions = order.convert_conditions(py)?;
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
        if !ClientCore::ids_fit("place_order", &[order_id, contract.con_id]) { return Ok(()); }

        // Transmit off: the order is held (ibx#509). A what-if with
        // transmit off is refused with 321 by the checks of an order.
        if !api_order.transmit && !api_order.what_if {
            return self.hold_order(py, order_id, &full_contract, &api_order);
        }
        let tx = self.tx()?;
        let mut built = Vec::new();
        if api_order.what_if {
            self.send_order(py, order_id, &full_contract, &api_order, false, &mut built)?;
        } else {
            // Transmit on: the order goes out with the held orders of its
            // tree, together (ibx#547). An order that fails ends the list;
            // the ones before it go.
            for (id, contract, order, was_held) in self.core.orders_to_transmit(order_id, &full_contract, &api_order) {
                if let Err(e) = self.send_order(py, id, &contract, &order, was_held, &mut built) {
                    if built.len() > 1 {
                        send_cmd(py, &tx, ClientCore::order_group(built))?;
                    } else if let Some(cmd) = built.pop() {
                        send_cmd(py, &tx, cmd)?;
                    }
                    return Err(e);
                }
            }
        }
        if built.len() > 1 {
            return send_cmd(py, &tx, ClientCore::order_group(built));
        }
        built.pop().map_or(Ok(()), |cmd| send_cmd(py, &tx, cmd))
    }

    /// Cancel an order.
    #[pyo3(signature = (order_id, manual_order_cancel_time=""))]
    fn cancel_order(&self, py: Python<'_>, order_id: i64, manual_order_cancel_time: &str) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if !ClientCore::ids_fit("cancel_order", &[order_id]) { return Ok(()); }
        let tx = self.tx()?;
        self.core.drop_held_order(order_id);
        send_cmd(py, &tx, ControlCommand::Order(OrderRequest::Cancel { order_id }))?;
        let _ = manual_order_cancel_time;
        Ok(())
    }

    /// Cancel all orders globally: every order of the account the session
    /// knows, those of other clients and of earlier sessions too, as the
    /// reference cancels them.
    fn req_global_cancel(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        self.core.drop_held_orders();
        send_cmd(py, &tx, ControlCommand::Order(OrderRequest::GlobalCancel))
    }

    /// Request next valid order ID: the highest order id this client used
    /// + 1, as the reference computes it per client id (1 when none), a
    /// 32-bit id. The ids of the client's earlier sessions count as far as
    /// the server's replays of the logon show them; right after the
    /// connect the answer waits for the order replay of the logon. Nothing
    /// is reserved.
    #[pyo3(signature = (num_ids=1))]
    fn req_ids(&self, py: Python<'_>, num_ids: i32) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_ids(py, num_ids)) { return Ok(()); }
        let shared = self.shared_state()?;
        let next_id = py.detach(|| {
            ClientCore::wait_order_replay(&shared);
            self.core.next_valid_id(&shared)
        });
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        let _ = num_ids;
        Ok(())
    }

    /// The next order id for a new order: the next valid id (see
    /// ``req_ids``), or above the ids this method gave before. Each call
    /// reserves the id it gives.
    fn next_order_id(&self, py: Python<'_>) -> PyResult<i64> {
        let shared = self.shared_state()?;
        Ok(py.detach(|| {
            ClientCore::wait_order_replay(&shared);
            self.core.take_order_id(&shared)
        }))
    }

    /// Request all open orders for this client.
    ///
    /// Before the order replay of the logon has ended, and while the auth
    /// link is lost, the request is answered only after the order replay,
    /// from the dispatch loop (ibx#251).
    fn req_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
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
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
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
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let _ = b_auto_bind;
        Ok(())
    }

    /// Request execution reports.
    #[pyo3(signature = (req_id, exec_filter=None))]
    fn req_executions(&self, py: Python<'_>, req_id: i64, exec_filter: Option<Py<PyAny>>) -> PyResult<()> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        let filter_later = exec_filter.as_ref().map(|f| f.clone_ref(py));
        if self.later(move |this, py| this.req_executions(py, req_id, filter_later)) { return Ok(()); }
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
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if let Some(r) = self.not_connected(-1) { return r; }
        if self.later(move |this, py| this.req_completed_orders(py, api_only)) { return Ok(()); }
        let _ = api_only;
        // The lock is let go before the callbacks: held across one that
        // releases the interpreter lock (a file write, a lock, a sleep), it
        // stops the event loop, which takes it with the interpreter lock
        // held, and with it the whole program.
        let shared = self.shared.lock().unwrap().clone();
        if let Some(shared) = shared {
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
        if self.later(move |this, py| { let shared = this.shared_state()?; this.answer_open_orders(py, &shared, request) }) { return Ok(()); }
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
