//! Order placement, cancellation, open orders, executions, completed orders.

use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::client_core::ClientCore;
use crate::types::*;
use super::{EClient, StoredOrder, StoredExecution};
use super::super::contract::{Contract, Order, CommissionReport};

#[pymethods]
impl EClient {
    /// Place an order.
    fn place_order(&self, py: Python<'_>, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        let tx = self.tx()?;

        let oid = if order_id > 0 {
            order_id as u64
        } else {
            self.next_order_id.fetch_add(1, Ordering::Relaxed)
        };

        let instrument = self.find_or_register_instrument(contract)?;

        // Convert Python Order to Rust API Order and use shared routing logic
        let mut api_order = order.to_api();
        api_order.conditions = order.convert_conditions(py);

        let cmd = ClientCore::build_order_request(&api_order, oid, instrument)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        tx.send(cmd)
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;

        // Store order info for req_open_orders / req_completed_orders
        let mut stored_order = order.clone();
        stored_order.order_id = oid as i64;
        self.open_orders.lock().unwrap().insert(oid, StoredOrder {
            contract: contract.clone(),
            order: stored_order,
            status: "PendingSubmit".to_string(),
            filled: 0.0,
            remaining: api_order.total_quantity,
            instrument,
        });
        self.contract_cache.lock().unwrap().insert(contract.con_id, contract.clone());

        Ok(())
    }

    /// Cancel an order.
    #[pyo3(signature = (order_id, manual_order_cancel_time=""))]
    fn cancel_order(&self, order_id: i64, manual_order_cancel_time: &str) -> PyResult<()> {
        let tx = self.tx()?;
        tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: order_id as u64 }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        let _ = manual_order_cancel_time;
        Ok(())
    }

    /// Cancel all orders globally.
    fn req_global_cancel(&self) -> PyResult<()> {
        let tx = self.tx()?;
        let shared = self.shared_state()?;
        let count = shared.market.instrument_count();
        for instrument in 0..count {
            let _ = tx.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }));
        }
        Ok(())
    }

    /// Request next valid order ID.
    #[pyo3(signature = (num_ids=1))]
    fn req_ids(&self, py: Python<'_>, num_ids: i32) -> PyResult<()> {
        let next_id = self.next_order_id.load(Ordering::Relaxed) as i64;
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        let _ = num_ids;
        Ok(())
    }

    /// Request all open orders for this client.
    fn req_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        // Merge local open_orders with enriched data from shared order cache
        let orders: Vec<(u64, StoredOrder)> = {
            let map = self.open_orders.lock().unwrap();
            map.iter()
                .filter(|(_, so)| !matches!(so.status.as_str(), "Filled" | "Cancelled" | "Inactive"))
                .map(|(&oid, so)| (oid, so.clone()))
                .collect()
        };
        // Also merge in any orders from shared cache not in our local map
        if let Some(shared) = self.shared.lock().unwrap().clone() {
            for (oid, info) in shared.orders.drain_open_orders() {
                if !matches!(info.order_state.status.as_str(), "Filled" | "Cancelled" | "Inactive") {
                    // Update local contract cache from enriched data
                    if info.contract.con_id != 0 {
                        self.contract_cache.lock().unwrap().insert(info.contract.con_id, Contract {
                            con_id: info.contract.con_id,
                            symbol: info.contract.symbol.clone(),
                            sec_type: info.contract.sec_type.clone(),
                            exchange: info.contract.exchange.clone(),
                            currency: info.contract.currency.clone(),
                            ..Default::default()
                        });
                    }
                    // Enrich local stored order if present
                    if let Some(so) = self.open_orders.lock().unwrap().get_mut(&oid) {
                        if so.order.account.is_empty() {
                            so.order.account = info.order.account.clone();
                        }
                        if so.order.perm_id == 0 {
                            so.order.perm_id = info.order.perm_id;
                        }
                    }
                }
            }
        }
        for (order_id, so) in &orders {
            let c_py = Py::new(py, so.contract.clone())?.into_any();
            let o_py = Py::new(py, so.order.clone())?.into_any();
            let state = pyo3::types::PyDict::new(py);
            state.set_item("status", so.status.as_str())?;
            state.set_item("completedTime", "")?;
            self.wrapper.call_method(
                py, "open_order",
                (*order_id as i64, &c_py, &o_py, state.as_any()),
                None,
            )?;
            self.wrapper.call_method(
                py, "order_status",
                (*order_id as i64, so.status.as_str(), so.filled, so.remaining,
                 0.0f64, 0i64, 0i64, 0.0f64, 0i64, "", 0.0f64),
                None,
            )?;
        }
        self.wrapper.call_method0(py, "open_order_end")?;
        Ok(())
    }

    /// Request all open orders across all clients.
    fn req_all_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        self.req_open_orders(py)
    }

    /// Automatically bind future orders to this client.
    #[pyo3(signature = (b_auto_bind))]
    fn req_auto_open_orders(&self, b_auto_bind: bool) -> PyResult<()> {
        let _ = b_auto_bind;
        Ok(())
    }

    /// Request execution reports.
    #[pyo3(signature = (req_id, exec_filter=None))]
    fn req_executions(&self, py: Python<'_>, req_id: i64, exec_filter: Option<PyObject>) -> PyResult<()> {
        let (f_symbol, f_sec_type, f_exchange, f_side, f_acct_code) = if let Some(ref fobj) = exec_filter {
            let get = |attr: &str| -> String {
                fobj.getattr(py, pyo3::types::PyString::new(py, attr))
                    .and_then(|v| v.extract::<String>(py))
                    .unwrap_or_default()
            };
            (get("symbol"), get("secType"), get("exchange"), get("side"), get("acctCode"))
        } else {
            Default::default()
        };

        let execs: Vec<StoredExecution> = {
            self.executions.lock().unwrap().clone()
        };
        let acct_name = self.account();
        for se in &execs {
            if !f_symbol.is_empty() && !se.contract.symbol.eq_ignore_ascii_case(&f_symbol) { continue; }
            if !f_sec_type.is_empty() && !se.contract.sec_type.eq_ignore_ascii_case(&f_sec_type) { continue; }
            if !f_exchange.is_empty() && !se.exchange.eq_ignore_ascii_case(&f_exchange) { continue; }
            if !f_side.is_empty() && !se.side.eq_ignore_ascii_case(&f_side) { continue; }
            if !f_acct_code.is_empty() && !acct_name.eq_ignore_ascii_case(&f_acct_code) { continue; }

            let c_py = Py::new(py, se.contract.clone())?.into_any();

            let exec_obj = pyo3::types::PyDict::new(py);
            exec_obj.set_item("execId", se.exec_id.as_str())?;
            exec_obj.set_item("time", se.time.as_str())?;
            exec_obj.set_item("acctNumber", acct_name.as_str())?;
            exec_obj.set_item("exchange", se.exchange.as_str())?;
            exec_obj.set_item("side", se.side.as_str())?;
            exec_obj.set_item("shares", se.shares)?;
            exec_obj.set_item("price", se.price)?;
            exec_obj.set_item("permId", 0i64)?;
            exec_obj.set_item("clientId", 0i64)?;
            exec_obj.set_item("orderId", se.order_id as i64)?;
            exec_obj.set_item("liquidation", 0i64)?;
            exec_obj.set_item("cumQty", se.cum_qty)?;
            exec_obj.set_item("avgPrice", se.avg_price)?;
            exec_obj.set_item("orderRef", "")?;
            exec_obj.set_item("evRule", "")?;
            exec_obj.set_item("evMultiplier", 0.0f64)?;
            exec_obj.set_item("modelCode", "")?;
            exec_obj.set_item("lastLiquidity", 0i64)?;
            exec_obj.set_item("pendingPriceRevision", false)?;

            self.wrapper.call_method(
                py, "exec_details",
                (req_id, &c_py, exec_obj.as_any()),
                None,
            )?;

            let report = CommissionReport {
                exec_id: se.exec_id.clone(),
                commission: se.commission,
                currency: "USD".to_string(),
                realized_pnl: f64::MAX,
                yield_amount: f64::MAX,
                yield_redemption_date: String::new(),
            };
            let report_py = Py::new(py, report)?.into_any();
            self.wrapper.call_method1(py, "commission_report", (&report_py,))?;
        }
        self.wrapper.call_method1(py, "exec_details_end", (req_id,))?;
        Ok(())
    }

    /// Request completed orders.
    #[pyo3(signature = (api_only=false))]
    fn req_completed_orders(&self, py: Python<'_>, api_only: bool) -> PyResult<()> {
        let _ = api_only;
        if let Some(shared) = self.shared.lock().unwrap().clone() {
            let completed = shared.orders.drain_completed_orders();
            for co in &completed {
                let status_str = match co.status {
                    crate::types::OrderStatus::Filled => "Filled",
                    crate::types::OrderStatus::Cancelled => "Cancelled",
                    crate::types::OrderStatus::Rejected => "Inactive",
                    _ => "Unknown",
                };
                let rich_info = shared.orders.get_order_info(co.order_id);
                let state = pyo3::types::PyDict::new(py);
                state.set_item("status", status_str)?;
                state.set_item("completedTime",
                    rich_info.as_ref().map(|i| i.order_state.completed_time.as_str()).unwrap_or(""))?;
                state.set_item("completedStatus",
                    rich_info.as_ref().map(|i| i.order_state.completed_status.as_str()).unwrap_or(""))?;

                let stored = self.open_orders.lock().unwrap().get(&co.order_id).cloned();
                if let Some(so) = stored {
                    let c_py = Py::new(py, so.contract)?.into_any();
                    let o_py = Py::new(py, so.order)?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, state.as_any()))?;
                } else if let Some(info) = rich_info {
                    let c = Contract {
                        con_id: info.contract.con_id,
                        symbol: info.contract.symbol,
                        sec_type: info.contract.sec_type,
                        exchange: info.contract.exchange,
                        currency: info.contract.currency,
                        ..Default::default()
                    };
                    let mut o = Order::default();
                    o.order_id = info.order.order_id;
                    o.action = info.order.action;
                    o.total_quantity = info.order.total_quantity;
                    o.order_type = info.order.order_type;
                    o.lmt_price = info.order.lmt_price;
                    o.aux_price = info.order.aux_price;
                    o.tif = info.order.tif;
                    o.account = info.order.account;
                    o.perm_id = info.order.perm_id;
                    let c_py = Py::new(py, c)?.into_any();
                    let o_py = Py::new(py, o)?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, state.as_any()))?;
                } else {
                    let c_py = Py::new(py, Contract::default())?.into_any();
                    let o_py = Py::new(py, Order::default())?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, state.as_any()))?;
                }
            }
            self.wrapper.call_method0(py, "completed_orders_end")?;
        }
        Ok(())
    }
}
