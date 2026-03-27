//! Order placement, cancellation, open orders, executions, completed orders.

use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::api::types::{
    Contract as ApiContract, Order as ApiOrder, ExecutionFilter,
};
use crate::client_core::ClientCore;
use crate::types::*;
use super::EClient;
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

        // Track order in shared core
        let api_contract = ApiContract {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            ..Default::default()
        };
        let mut tracked_order = api_order.clone();
        tracked_order.order_id = oid as i64;
        self.core.cache_contract(contract.con_id, api_contract.clone());
        self.core.track_order(oid, api_contract, tracked_order, instrument);

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

    /// Get the next order ID (local counter, auto-increments).
    fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed) as i64
    }

    /// Request all open orders for this client.
    fn req_open_orders(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared_state()?;
        let orders = self.core.collect_open_orders(&shared);
        for (order_id, tracked) in &orders {
            let c_py = Py::new(py, Contract {
                con_id: tracked.contract.con_id,
                symbol: tracked.contract.symbol.clone(),
                sec_type: tracked.contract.sec_type.clone(),
                exchange: tracked.contract.exchange.clone(),
                currency: tracked.contract.currency.clone(),
                ..Default::default()
            })?.into_any();
            let mut o = Order::default();
            o.order_id = tracked.order.order_id;
            o.action = tracked.order.action.clone();
            o.total_quantity = tracked.order.total_quantity;
            o.order_type = tracked.order.order_type.clone();
            o.lmt_price = tracked.order.lmt_price;
            o.aux_price = tracked.order.aux_price;
            o.tif = tracked.order.tif.clone();
            o.account = tracked.order.account.clone();
            o.perm_id = tracked.order.perm_id;
            o.oca_type = tracked.order.oca_type;
            o.use_price_mgmt_algo = tracked.order.use_price_mgmt_algo;
            o.trail_stop_price = tracked.order.trail_stop_price;
            o.algo_strategy = tracked.order.algo_strategy.clone();
            let o_py = Py::new(py, o)?.into_any();
            let state = pyo3::types::PyDict::new(py);
            state.set_item("status", tracked.status.as_str())?;
            state.set_item("completedTime", "")?;
            self.wrapper.call_method(
                py, "open_order",
                (*order_id as i64, &c_py, &o_py, state.as_any()),
                None,
            )?;
            self.wrapper.call_method(
                py, "order_status",
                (*order_id as i64, tracked.status.as_str(), tracked.filled, tracked.remaining,
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
                ..Default::default()
            }
        } else {
            ExecutionFilter::default()
        };

        let indices = self.core.filter_executions(&filter);
        let execs = self.core.executions.lock().unwrap();
        for i in indices {
            let se = &execs[i];
            let c_py = Py::new(py, Contract {
                con_id: se.contract.con_id,
                symbol: se.contract.symbol.clone(),
                sec_type: se.contract.sec_type.clone(),
                exchange: se.contract.exchange.clone(),
                currency: se.contract.currency.clone(),
                ..Default::default()
            })?.into_any();

            let exec_obj = pyo3::types::PyDict::new(py);
            exec_obj.set_item("execId", se.execution.exec_id.as_str())?;
            exec_obj.set_item("time", se.execution.time.as_str())?;
            exec_obj.set_item("acctNumber", se.execution.acct_number.as_str())?;
            exec_obj.set_item("exchange", se.execution.exchange.as_str())?;
            exec_obj.set_item("side", se.execution.side.as_str())?;
            exec_obj.set_item("shares", se.execution.shares)?;
            exec_obj.set_item("price", se.execution.price)?;
            exec_obj.set_item("permId", se.execution.perm_id)?;
            exec_obj.set_item("clientId", se.execution.client_id)?;
            exec_obj.set_item("orderId", se.execution.order_id)?;
            exec_obj.set_item("liquidation", se.execution.liquidation as i64)?;
            exec_obj.set_item("cumQty", se.execution.cum_qty)?;
            exec_obj.set_item("avgPrice", se.execution.avg_price)?;
            exec_obj.set_item("orderRef", "")?;
            exec_obj.set_item("evRule", se.execution.ev_rule.as_str())?;
            exec_obj.set_item("evMultiplier", se.execution.ev_multiplier)?;
            exec_obj.set_item("modelCode", se.execution.model_code.as_str())?;
            exec_obj.set_item("lastLiquidity", se.execution.last_liquidity as i64)?;
            exec_obj.set_item("pendingPriceRevision", se.execution.pending_price_revision)?;

            self.wrapper.call_method(
                py, "exec_details",
                (req_id, &c_py, exec_obj.as_any()),
                None,
            )?;

            let report = CommissionReport {
                exec_id: se.commission.exec_id.clone(),
                commission: se.commission.commission,
                currency: se.commission.currency.clone(),
                realized_pnl: se.commission.realized_pnl,
                yield_amount: se.commission.yield_amount,
                yield_redemption_date: se.commission.yield_redemption_date.clone(),
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

                let tracked = self.core.open_orders.lock().unwrap().get(&co.order_id).map(|o| {
                    (Contract {
                        con_id: o.contract.con_id,
                        symbol: o.contract.symbol.clone(),
                        sec_type: o.contract.sec_type.clone(),
                        exchange: o.contract.exchange.clone(),
                        currency: o.contract.currency.clone(),
                        ..Default::default()
                    }, {
                        let mut ord = Order::default();
                        ord.order_id = o.order.order_id;
                        ord.action = o.order.action.clone();
                        ord.total_quantity = o.order.total_quantity;
                        ord.order_type = o.order.order_type.clone();
                        ord.lmt_price = o.order.lmt_price;
                        ord.aux_price = o.order.aux_price;
                        ord.tif = o.order.tif.clone();
                        ord.account = o.order.account.clone();
                        ord.perm_id = o.order.perm_id;
                        ord
                    })
                });
                if let Some((c, o)) = tracked {
                    let c_py = Py::new(py, c)?.into_any();
                    let o_py = Py::new(py, o)?.into_any();
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
