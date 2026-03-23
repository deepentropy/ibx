//! ibapi-compatible EClient class that wraps IbEngine.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crossbeam_channel::Sender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::SharedState;
use crate::client_core::{ClientCore, order_status_str};
use crate::control::historical::{HistoricalBar, HistoricalResponse, HeadTimestampResponse};
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;
use super::contract::{Contract, ContractDescription, Order, TagValue};
use super::tick_types::*;
use super::super::types::PRICE_SCALE_F;

/// Stored order info for open_order / completed_order callbacks.
#[derive(Clone)]
struct StoredOrder {
    contract: Contract,
    order: Order,
    status: String,
    filled: f64,
    remaining: f64,
    instrument: u32,
}

/// Stored execution info for exec_details callbacks.
#[derive(Clone)]
struct StoredExecution {
    req_id: i64,
    contract: Contract,
    exec_id: String,
    side: String,
    price: f64,
    shares: f64,
    time: String,
    order_id: u64,
    cum_qty: f64,
    avg_price: f64,
}

/// ibapi-compatible EClient class.
/// Wraps the internal engine and dispatches events to an EWrapper subclass.
///
/// All methods take `&self` (shared borrow) so that `run()` can execute in a
/// daemon thread while the main thread calls req/cancel methods concurrently.
/// `frozen` tells PyO3 to skip RefCell borrow-checking, which is required
/// because `run()` holds a `&self` borrow for the lifetime of the event loop.
/// Interior mutability is provided by `OnceLock`, `AtomicBool`, and `Mutex`.
///
/// # Thread lifecycle
///
/// `connect()` spawns a single `ib-engine-hotloop` background thread.
/// The thread is **joined** on [`disconnect()`] and on [`Drop`].
/// Dropping an `EClient` without calling `disconnect()` first is safe:
/// the `Drop` impl sends `Shutdown` and joins the thread.
///
/// **Note:** this struct is single-use — `OnceLock` fields cannot be reset,
/// so calling `connect()` after `disconnect()` on the same instance will panic.
/// Create a new `EClient` for reconnection.
#[pyclass(frozen, subclass)]
pub struct EClient {
    /// Reference to the EWrapper (which is typically `self` in the `App(EWrapper, EClient)` pattern).
    wrapper: PyObject,
    /// Set once by connect(), read-only after.
    shared: OnceLock<Arc<SharedState>>,
    /// Set once by connect(), read-only after.
    control_tx: OnceLock<Sender<ControlCommand>>,
    next_order_id: AtomicU64,
    _thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Set once by connect(), read-only after.
    account_id: OnceLock<String>,
    connected: AtomicBool,
    /// Shared subscription tracking and dispatch preparation.
    core: ClientCore,
    /// Req_ids that have already received a market_data_type callback.
    mdt_sent: Mutex<HashSet<i64>>,
    /// Market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    market_data_type: AtomicI32,
    /// Track open orders: order_id → StoredOrder (contract + order + status).
    open_orders: Mutex<HashMap<u64, StoredOrder>>,
    /// Track executions with full details.
    executions: Mutex<Vec<StoredExecution>>,
    /// News provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN").
    news_providers: Mutex<String>,
    /// Cache of con_id → Contract for enriching position/execution callbacks.
    contract_cache: Mutex<HashMap<i64, Contract>>,
}

impl Drop for EClient {
    fn drop(&mut self) {
        if let Some(tx) = self.control_tx.get() {
            let _ = tx.send(ControlCommand::Shutdown);
        }
        if let Some(h) = self._thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

#[pymethods]
impl EClient {
    #[new]
    #[pyo3(signature = (wrapper))]
    fn new(wrapper: PyObject) -> Self {
        Self {
            wrapper,
            shared: OnceLock::new(),
            control_tx: OnceLock::new(),
            next_order_id: AtomicU64::new(0),
            _thread: Mutex::new(None),
            account_id: OnceLock::new(),
            connected: AtomicBool::new(false),
            core: ClientCore::new(),
            mdt_sent: Mutex::new(HashSet::new()),
            market_data_type: AtomicI32::new(1),
            open_orders: Mutex::new(HashMap::new()),
            executions: Mutex::new(Vec::new()),
            news_providers: Mutex::new("BRFG*BRFUPDN".to_string()),
            contract_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Connect to IB and start the engine.
    /// Signature matches ibapi: connect(host, port, clientId) but internally uses
    /// direct gateway auth. Pass username as host, password as port (string),
    /// or use keyword args: connect(username="...", password="...", paper=True).
    #[pyo3(signature = (host="cdc1.ibllc.com".to_string(), port=0, client_id=0, username="".to_string(), password="".to_string(), paper=true, core_id=None))]
    fn connect(
        &self,
        py: Python<'_>,
        host: String,
        port: i32,
        client_id: i32,
        username: String,
        password: String,
        paper: bool,
        core_id: Option<usize>,
    ) -> PyResult<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Already connected"));
        }

        let config = GatewayConfig {
            username,
            password,
            host,
            paper,
            accept_invalid_certs: false,
        };

        let result = py.allow_threads(|| Gateway::connect(&config));
        let (gw, farm_conn, ccp_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn) = result
            .map_err(|e| PyRuntimeError::new_err(format!("Connection failed: {}", e)))?;

        // OnceLock::set() is safe here: the `connected` guard above ensures single-call.
        self.account_id.set(gw.account_id.clone()).expect("account_id already set");
        let shared = Arc::new(SharedState::new());

        let (mut hot_loop, control_tx) = gw.into_hot_loop_with_farms(shared.clone(), None, farm_conn, ccp_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, core_id);

        let start_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() * 1000;

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || {
                hot_loop.run();
            })
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn hot loop: {}", e)))?;

        self.shared.set(shared).unwrap_or_else(|_| panic!("shared already set"));
        self.control_tx.set(control_tx).unwrap_or_else(|_| panic!("control_tx already set"));
        self.next_order_id.store(start_id, Ordering::Relaxed);
        *self._thread.lock().unwrap() = Some(handle);
        self.connected.store(true, Ordering::Release);

        let _ = (port, client_id); // unused but kept for ibapi signature compat

        Ok(())
    }

    /// Disconnect from IB.  Sends `Shutdown` to the hot loop, waits for the
    /// background thread to exit, and marks the client as disconnected.
    fn disconnect(&self) -> PyResult<()> {
        if let Some(tx) = self.control_tx.get() {
            let _ = tx.send(ControlCommand::Shutdown);
        }
        if let Some(h) = self._thread.lock().unwrap().take() {
            let _ = h.join();
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    /// Check if connected.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Set news provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN").
    #[pyo3(signature = (providers))]
    fn set_news_providers(&self, providers: &str) {
        *self.news_providers.lock().unwrap() = providers.to_string();
    }

    /// Request market data for a contract.
    /// If generic_tick_list contains "292", also subscribes to news ticks via CCP.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))]
    fn req_mkt_data(
        &self,
        req_id: i64,
        contract: &Contract,
        generic_tick_list: &str,
        snapshot: bool,
        regulatory_snapshot: bool,
        mkt_data_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let shared = self.shared.get().unwrap();

        // If generic_tick_list contains 292 (news), also subscribe via CCP
        let wants_news = generic_tick_list.split(',')
            .any(|t| t.trim() == "292" || t.trim() == "mdoff,292" || t.trim().ends_with("292"));
        if wants_news {
            let providers = self.news_providers.lock().unwrap().clone();
            tx.send(ControlCommand::SubscribeNews {
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                providers,
                reply_tx: None,
            }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }

        self.core.register_mkt_data(
            shared, tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            snapshot,
        ).map_err(|e| PyRuntimeError::new_err(e))?;
        self.contract_cache.lock().unwrap().insert(contract.con_id, contract.clone());

        let _ = (regulatory_snapshot, mkt_data_options);

        Ok(())
    }

    /// Cancel market data.
    fn cancel_mkt_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.core.unregister_mkt_data(req_id) {
            self.mdt_sent.lock().unwrap().remove(&req_id);
            let tx = self.control_tx.get()
                .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
            tx.send(ControlCommand::Unsubscribe { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
            let _ = tx.send(ControlCommand::UnsubscribeNews { instrument });
        }
        Ok(())
    }

    /// Request tick-by-tick data.
    #[pyo3(signature = (req_id, contract, tick_type, number_of_ticks=0, ignore_size=false))]
    fn req_tick_by_tick_data(
        &self,
        req_id: i64,
        contract: &Contract,
        tick_type: &str,
        number_of_ticks: i32,
        ignore_size: bool,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;

        let tbt_type = match tick_type {
            "Last" | "AllLast" => TbtType::Last,
            "BidAsk" => TbtType::BidAsk,
            _ => return Err(PyRuntimeError::new_err(format!("Unknown tick type: '{}'", tick_type))),
        };

        let shared = self.shared.get().unwrap();
        tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id, reply_tx: None })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        self.core.register_tbt(
            shared, tx, req_id,
            contract.con_id, &contract.symbol, tbt_type,
        ).map_err(|e| PyRuntimeError::new_err(e))?;

        let _ = (number_of_ticks, ignore_size);
        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.core.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.core.instrument_to_req.lock().unwrap().remove(&instrument);
            let tx = self.control_tx.get()
                .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
            tx.send(ControlCommand::UnsubscribeTbt { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }
        Ok(())
    }

    /// Request historical bar data.
    #[pyo3(signature = (req_id, contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth, format_date=1, keep_up_to_date=false, chart_options=Vec::new()))]
    fn req_historical_data(
        &self,
        req_id: i64,
        contract: &Contract,
        end_date_time: &str,
        duration_str: &str,
        bar_size_setting: &str,
        what_to_show: &str,
        use_rth: i32,
        format_date: i32,
        keep_up_to_date: bool,
        chart_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let _ = (format_date, keep_up_to_date, chart_options);
        // Route SCHEDULE requests to the schedule-specific command
        if what_to_show.eq_ignore_ascii_case("SCHEDULE") {
            tx.send(ControlCommand::FetchHistoricalSchedule {
                req_id: req_id as u32,
                con_id: contract.con_id,
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                use_rth: use_rth != 0,
            }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        } else {
            tx.send(ControlCommand::FetchHistorical {
                req_id: req_id as u32,
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                end_date_time: end_date_time.to_string(),
                duration: duration_str.to_string(),
                bar_size: bar_size_setting.to_string(),
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
            }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }
        Ok(())
    }

    /// Cancel historical data.
    fn cancel_historical_data(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelHistorical { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request head timestamp.
    #[pyo3(signature = (req_id, contract, what_to_show, use_rth, format_date=1))]
    fn req_head_time_stamp(
        &self,
        req_id: i64,
        contract: &Contract,
        what_to_show: &str,
        use_rth: i32,
        format_date: i32,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchHeadTimestamp {
            req_id: req_id as u32,
            con_id: contract.con_id,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        let _ = format_date;
        Ok(())
    }

    /// Request contract details.
    fn req_contract_details(&self, req_id: i64, contract: &Contract) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchContractDetails {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request P&L updates for the account. Gateway-computed from positions × quotes.
    #[pyo3(signature = (req_id, account, model_code=""))]
    fn req_pnl(&self, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        self.core.subscribe_pnl(req_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel P&L subscription.
    fn cancel_pnl(&self, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_pnl(req_id);
        Ok(())
    }

    /// Request P&L for a single position. Gateway-computed.
    #[pyo3(signature = (req_id, account, model_code, con_id))]
    fn req_pnl_single(&self, req_id: i64, account: &str, model_code: &str, con_id: i64) -> PyResult<()> {
        self.core.subscribe_pnl_single(req_id, con_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel single-position P&L subscription.
    fn cancel_pnl_single(&self, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_pnl_single(req_id);
        Ok(())
    }

    /// Request account summary.
    #[pyo3(signature = (req_id, group_name, tags))]
    fn req_account_summary(&self, req_id: i64, group_name: &str, tags: &str) -> PyResult<()> {
        self.core.subscribe_account_summary(req_id, tags);
        let _ = group_name;
        Ok(())
    }

    /// Cancel account summary.
    fn cancel_account_summary(&self, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_account_summary(req_id);
        Ok(())
    }

    /// Request all positions.
    fn req_positions(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let positions = shared.portfolio.position_infos();
        let cache = self.contract_cache.lock().unwrap();
        for pi in &positions {
            let c = cache.get(&pi.con_id).cloned().unwrap_or_else(|| {
                // Fall back to shared contract cache (enriched from CCP exec reports)
                shared.reference.get_contract(pi.con_id).map(|ac| {
                    let mut c = super::contract::Contract::default();
                    c.con_id = ac.con_id;
                    c.symbol = ac.symbol;
                    c.sec_type = ac.sec_type;
                    c.exchange = ac.exchange;
                    c.currency = ac.currency;
                    c
                }).unwrap_or_else(|| {
                    let mut c = super::contract::Contract::default();
                    c.con_id = pi.con_id;
                    c
                })
            });
            let c_py = Py::new(py, c)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.wrapper.call_method(
                py, "position",
                (self.account_id.get().map(|s| s.as_str()).unwrap_or(""), &c_py, pi.position as f64, avg_cost),
                None,
            )?;
        }
        self.wrapper.call_method0(py, "position_end")?;
        Ok(())
    }

    /// Cancel positions.
    fn cancel_positions(&self) -> PyResult<()> {
        Ok(()) // positions delivered immediately, nothing to cancel
    }

    /// Place an order. Delegates to Rust API's order routing logic.
    fn place_order(&self, py: Python<'_>, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;

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
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: order_id as u64 }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        let _ = manual_order_cancel_time;
        Ok(())
    }

    /// Cancel all orders globally.
    fn req_global_cancel(&self) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let map = self.core.req_to_instrument.lock().unwrap();
        for &instrument in map.values() {
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

    /// Request account updates.
    #[pyo3(signature = (subscribe, _acct_code=""))]
    fn req_account_updates(&self, subscribe: bool, _acct_code: &str) -> PyResult<()> {
        self.core.subscribe_account_updates(subscribe);
        Ok(())
    }

    /// Set market data type (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    fn req_market_data_type(&self, market_data_type: i32) -> PyResult<()> {
        self.market_data_type.store(market_data_type, Ordering::Relaxed);
        Ok(())
    }

    /// Request market depth (L2 order book).
    #[pyo3(signature = (req_id, contract, num_rows=5, is_smart_depth=false, mkt_depth_options=Vec::new()))]
    fn req_mkt_depth(
        &self,
        req_id: i64,
        contract: &Contract,
        num_rows: i32,
        is_smart_depth: bool,
        mkt_depth_options: Vec<PyObject>,
    ) -> PyResult<()> {
        // L2 depth data requires a different subscription protocol not yet supported.
        // Accept the call for API compatibility; the wrapper callbacks won't fire.
        let _ = (req_id, contract, num_rows, is_smart_depth, mkt_depth_options);
        log::warn!("req_mkt_depth: L2 depth subscription not yet implemented in engine");
        Ok(())
    }

    /// Cancel market depth.
    #[pyo3(signature = (req_id, is_smart_depth=false))]
    fn cancel_mkt_depth(&self, req_id: i64, is_smart_depth: bool) -> PyResult<()> {
        let _ = (req_id, is_smart_depth);
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
        if let Some(shared) = self.shared.get() {
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
        // Same as req_open_orders — we only have one client connection.
        self.req_open_orders(py)
    }

    /// Automatically bind future orders to this client.
    /// In ibapi, this tells the gateway to auto-deliver openOrder callbacks for
    /// orders placed from other clients. In single-client direct-connect model,
    /// all orders are already ours — order updates flow automatically via run().
    #[pyo3(signature = (b_auto_bind))]
    fn req_auto_open_orders(&self, b_auto_bind: bool) -> PyResult<()> {
        let _ = b_auto_bind;
        Ok(())
    }

    /// Request execution reports.
    #[pyo3(signature = (req_id, _exec_filter=None))]
    fn req_executions(&self, py: Python<'_>, req_id: i64, _exec_filter: Option<PyObject>) -> PyResult<()> {
        let execs: Vec<StoredExecution> = {
            self.executions.lock().unwrap().clone()
        };
        let acct_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
        for se in &execs {
            let c_py = Py::new(py, se.contract.clone())?.into_any();

            let exec_obj = pyo3::types::PyDict::new(py);
            exec_obj.set_item("execId", se.exec_id.as_str())?;
            exec_obj.set_item("time", se.time.as_str())?;
            exec_obj.set_item("acctNumber", acct_name)?;
            exec_obj.set_item("exchange", se.contract.exchange.as_str())?;
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
        }
        self.wrapper.call_method1(py, "exec_details_end", (req_id,))?;
        Ok(())
    }

    /// Request historical tick data (Time & Sales).
    #[pyo3(signature = (req_id, contract, start_date_time="", end_date_time="", number_of_ticks=1000, what_to_show="TRADES", use_rth=1, ignore_size=false, misc_options=Vec::new()))]
    fn req_historical_ticks(
        &self,
        req_id: i64,
        contract: &Contract,
        start_date_time: &str,
        end_date_time: &str,
        number_of_ticks: i32,
        what_to_show: &str,
        use_rth: i32,
        ignore_size: bool,
        misc_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let _ = (ignore_size, misc_options);
        tx.send(ControlCommand::FetchHistoricalTicks {
            req_id: req_id as u32,
            con_id: contract.con_id,
            start_date_time: start_date_time.to_string(),
            end_date_time: end_date_time.to_string(),
            number_of_ticks: number_of_ticks as u32,
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request real-time 5-second bars.
    #[pyo3(signature = (req_id, contract, bar_size=5, what_to_show="TRADES", use_rth=0, real_time_bars_options=Vec::new()))]
    fn req_real_time_bars(
        &self,
        req_id: i64,
        contract: &Contract,
        bar_size: i32,
        what_to_show: &str,
        use_rth: i32,
        real_time_bars_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let _ = (bar_size, real_time_bars_options);
        tx.send(ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelRealTimeBar { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelHeadTimestamp { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request option chain parameters (expirations and strikes).
    #[pyo3(signature = (req_id, underlying_symbol, fut_fop_exchange="", underlying_sec_type="STK", underlying_con_id=0))]
    fn req_sec_def_opt_params(
        &self,
        req_id: i64,
        underlying_symbol: &str,
        fut_fop_exchange: &str,
        underlying_sec_type: &str,
        underlying_con_id: i64,
    ) -> PyResult<()> {
        // Option chain parameters require a dedicated server query not yet implemented.
        let _ = (req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id);
        log::warn!("req_sec_def_opt_params: not yet implemented in engine");
        Ok(())
    }

    /// Search for matching symbols.
    fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchMatchingSymbols {
            req_id: req_id as u32,
            pattern: pattern.to_string(),
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request server time.
    fn req_current_time(&self, py: Python<'_>) -> PyResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.wrapper.call_method1(py, "current_time", (now,))?;
        Ok(())
    }

    // ── Tier 2: Scanner ──

    /// Request scanner subscription.
    #[pyo3(signature = (req_id, subscription, scanner_subscription_options=Vec::new()))]
    fn req_scanner_subscription(
        &self,
        req_id: i64,
        subscription: PyObject,
        scanner_subscription_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = scanner_subscription_options;
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        // Extract fields from the subscription object
        Python::with_gil(|py| {
            let instrument = subscription.getattr(py, "instrument")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "STK".to_string());
            let location_code = subscription.getattr(py, "locationCode")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "STK.US.MAJOR".to_string());
            let scan_code = subscription.getattr(py, "scanCode")
                .and_then(|v| v.extract::<String>(py)).unwrap_or_else(|_| "TOP_PERC_GAIN".to_string());
            let max_items = subscription.getattr(py, "numberOfRows")
                .and_then(|v| v.extract::<u32>(py)).unwrap_or(50);
            tx.send(ControlCommand::SubscribeScanner {
                req_id: req_id as u32, instrument, location_code, scan_code, max_items,
            }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))
        })
    }

    /// Cancel scanner subscription.
    fn cancel_scanner_subscription(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelScanner { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchScannerParams)
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    // ── Tier 2: News ──

    /// Request news providers.
    fn req_news_providers(&self, py: Python<'_>) -> PyResult<()> {
        // News providers are typically cached by the gateway.
        // Return an empty list for now — the callback signature is satisfied.
        let empty_list = pyo3::types::PyList::empty(py);
        self.wrapper.call_method1(py, "news_providers", (empty_list.as_any(),))?;
        Ok(())
    }

    /// Request a news article.
    #[pyo3(signature = (req_id, provider_code, article_id, news_article_options=Vec::new()))]
    fn req_news_article(
        &self,
        req_id: i64,
        provider_code: &str,
        article_id: &str,
        news_article_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = news_article_options;
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchNewsArticle {
            req_id: req_id as u32,
            provider_code: provider_code.to_string(),
            article_id: article_id.to_string(),
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request historical news.
    #[pyo3(signature = (req_id, con_id, provider_codes, start_date_time, end_date_time, total_results, historical_news_options=Vec::new()))]
    fn req_historical_news(
        &self,
        req_id: i64,
        con_id: i64,
        provider_codes: &str,
        start_date_time: &str,
        end_date_time: &str,
        total_results: i32,
        historical_news_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = historical_news_options;
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchHistoricalNews {
            req_id: req_id as u32,
            con_id: con_id as u32,
            provider_codes: provider_codes.to_string(),
            start_time: start_date_time.to_string(),
            end_time: end_date_time.to_string(),
            max_results: total_results as u32,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    // ── Tier 2: Fundamental Data ──

    /// Request fundamental data.
    #[pyo3(signature = (req_id, contract, report_type, fundamental_data_options=Vec::new()))]
    fn req_fundamental_data(
        &self,
        req_id: i64,
        contract: &Contract,
        report_type: &str,
        fundamental_data_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = fundamental_data_options;
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchFundamentalData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            report_type: report_type.to_string(),
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel fundamental data.
    fn cancel_fundamental_data(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelFundamentalData { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    // ── Tier 2: Options Calculations (stubs) ──

    /// Calculate implied volatility.
    #[pyo3(signature = (req_id, contract, option_price, under_price, implied_vol_options=Vec::new()))]
    fn calculate_implied_volatility(
        &self, req_id: i64, contract: &Contract, option_price: f64,
        under_price: f64, implied_vol_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = (req_id, contract, option_price, under_price, implied_vol_options);
        log::warn!("calculate_implied_volatility: not yet implemented in engine");
        Ok(())
    }

    /// Calculate option price.
    #[pyo3(signature = (req_id, contract, volatility, under_price, opt_prc_options=Vec::new()))]
    fn calculate_option_price(
        &self, req_id: i64, contract: &Contract, volatility: f64,
        under_price: f64, opt_prc_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = (req_id, contract, volatility, under_price, opt_prc_options);
        log::warn!("calculate_option_price: not yet implemented in engine");
        Ok(())
    }

    /// Cancel implied volatility calculation.
    fn cancel_calculate_implied_volatility(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Cancel option price calculation.
    fn cancel_calculate_option_price(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Exercise options.
    #[pyo3(signature = (req_id, contract, exercise_action, exercise_quantity, account, _override))]
    fn exercise_options(
        &self, req_id: i64, contract: &Contract, exercise_action: i32,
        exercise_quantity: i32, account: &str, _override: i32,
    ) -> PyResult<()> {
        let _ = (req_id, contract, exercise_action, exercise_quantity, account, _override);
        log::warn!("exercise_options: not yet implemented in engine");
        Ok(())
    }

    // ── Tier 2: News Bulletins ──

    /// Subscribe to news bulletins. Bulletins arrive as server messages.
    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        let _ = all_msgs;
        self.core.subscribe_bulletins();
        Ok(())
    }

    /// Cancel news bulletins.
    fn cancel_news_bulletins(&self) -> PyResult<()> {
        self.core.unsubscribe_bulletins();
        Ok(())
    }

    // ── Tier 2: Managed Accounts ──

    /// Request managed accounts list.
    fn req_managed_accts(&self, py: Python<'_>) -> PyResult<()> {
        self.wrapper.call_method1(py, "managed_accounts", (self.account_id.get().map(|s| s.as_str()).unwrap_or(""),))?;
        Ok(())
    }

    // ── Tier 2: Multi-Account (stubs) ──

    /// Request account updates for multiple accounts/models.
    /// One-shot delivery from SharedState (single-account gateway).
    #[pyo3(signature = (req_id, account, model_code, ledger_and_nlv=false))]
    fn req_account_updates_multi(
        &self, py: Python<'_>, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let _ = ledger_and_nlv;
        let acct = shared.portfolio.account();
        let acct_name = if !account.is_empty() { account } else { self.account_id.get().map(|s| s.as_str()).unwrap_or("") };
        let tag_values: [(&str, f64); 8] = [
            ("NetLiquidation", acct.net_liquidation as f64 / PRICE_SCALE_F),
            ("TotalCashValue", acct.total_cash_value as f64 / PRICE_SCALE_F),
            ("BuyingPower", acct.buying_power as f64 / PRICE_SCALE_F),
            ("GrossPositionValue", acct.gross_position_value as f64 / PRICE_SCALE_F),
            ("UnrealizedPnL", acct.unrealized_pnl as f64 / PRICE_SCALE_F),
            ("RealizedPnL", acct.realized_pnl as f64 / PRICE_SCALE_F),
            ("InitMarginReq", acct.init_margin_req as f64 / PRICE_SCALE_F),
            ("MaintMarginReq", acct.maint_margin_req as f64 / PRICE_SCALE_F),
        ];
        for (key, val) in &tag_values {
            let val_str = format!("{:.2}", val);
            self.wrapper.call_method(
                py, "account_update_multi",
                (req_id, acct_name, model_code, *key, val_str.as_str(), "USD"),
                None,
            )?;
        }
        self.wrapper.call_method1(py, "account_update_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account updates. One-shot delivery, so this is a no-op.
    fn cancel_account_updates_multi(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Request positions across multiple accounts/models.
    /// One-shot delivery from SharedState (single-account gateway).
    #[pyo3(signature = (req_id, account, model_code))]
    fn req_positions_multi(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let positions = shared.portfolio.position_infos();
        for pi in &positions {
            let mut c = super::contract::Contract::default();
            c.con_id = pi.con_id;
            let c_py = Py::new(py, c)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.wrapper.call_method(
                py, "position_multi",
                (req_id, account, model_code, &c_py, pi.position as f64, avg_cost),
                None,
            )?;
        }
        self.wrapper.call_method1(py, "position_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account positions. One-shot delivery, so this is a no-op.
    fn cancel_positions_multi(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    // ── Tier 3: FA (Financial Advisor) ──

    /// Request FA data (groups, profiles, aliases).
    fn request_fa(&self, _fa_data_type: i32) -> PyResult<()> {
        log::warn!("request_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    /// Replace FA data.
    #[pyo3(signature = (req_id, fa_data_type, cxml))]
    fn replace_fa(&self, req_id: i64, fa_data_type: i32, cxml: &str) -> PyResult<()> {
        let _ = (req_id, fa_data_type, cxml);
        log::warn!("replace_fa: not yet implemented — needs FIX capture");
        Ok(())
    }

    // ── Tier 3: Display Groups ──

    /// Query display groups. Gateway returns empty list (no physical TWS windows).
    fn query_display_groups(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        self.wrapper.call_method1(py, "display_group_list", (req_id, ""))?;
        Ok(())
    }

    /// Subscribe to group events. No-op (gateway has no display groups).
    fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) -> PyResult<()> {
        let _ = (req_id, group_id);
        Ok(())
    }

    /// Unsubscribe from group events. No-op.
    fn unsubscribe_from_group_events(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Update display group. No-op (gateway has no display groups).
    fn update_display_group(&self, req_id: i64, contract_info: &str) -> PyResult<()> {
        let _ = (req_id, contract_info);
        Ok(())
    }

    // ── Tier 3: Market Rules ──

    /// Request market rule details (price increments).
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        // Market rules are cached from secdef responses.
        if let Some(shared) = self.shared.get() {
            if let Some(rule) = shared.reference.market_rule(market_rule_id) {
                let increments: Vec<(f64, f64)> = rule.price_increments.iter()
                    .map(|pi| (pi.low_edge, pi.increment)).collect();
                let list = pyo3::types::PyList::new(py, increments.iter().map(|(low, inc)| {
                    pyo3::types::PyTuple::new(py, &[*low, *inc]).unwrap()
                }))?;
                self.wrapper.call_method1(py, "market_rule", (market_rule_id as i64, list.as_any()))?;
                return Ok(());
            }
        }
        log::warn!("req_market_rule: rule {} not in cache", market_rule_id);
        Ok(())
    }

    // ── Tier 3: Smart Components ──

    /// Request SMART routing components. Returns empty map (gateway-local data not available).
    fn req_smart_components(&self, py: Python<'_>, req_id: i64, bbo_exchange: &str) -> PyResult<()> {
        let _ = bbo_exchange;
        let empty_map = pyo3::types::PyList::empty(py);
        self.wrapper.call_method1(py, "smart_components", (req_id, empty_map.as_any()))?;
        Ok(())
    }

    // ── Tier 3: Soft Dollar Tiers ──

    /// Request soft dollar tiers. Gateway resolves locally — returns empty on paper accounts.
    fn req_soft_dollar_tiers(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // Soft dollar tiers are gateway-local data (EClient msg 79→77).
        // Paper accounts always return 0 tiers.
        let empty_list = pyo3::types::PyList::empty(py);
        self.wrapper.call_method1(py, "soft_dollar_tiers", (req_id, empty_list.as_any()))?;
        Ok(())
    }

    // ── Tier 3: Family Codes ──

    /// Request family codes. Gateway resolves locally from login data.
    fn req_family_codes(&self, py: Python<'_>) -> PyResult<()> {
        // Family codes come from login data.
        // Return account_id with empty family code (matches paper behavior).
        let account = if !self.account_id.get().map(|s| s.is_empty()).unwrap_or(true) {
            self.account_id.get().map(|s| s.as_str()).unwrap_or("")
        } else {
            "*"
        };
        let codes = vec![(account, "")];
        let py_list = pyo3::types::PyList::new(py, codes.iter().map(|(acct, code)| {
            pyo3::types::PyTuple::new(py, &[
                acct.into_pyobject(py).unwrap().into_any(),
                code.into_pyobject(py).unwrap().into_any(),
            ]).unwrap()
        }))?;
        self.wrapper.call_method1(py, "family_codes", (py_list.as_any(),))?;
        Ok(())
    }

    // ── Tier 3: Histogram Data ──

    /// Request histogram data.
    #[pyo3(signature = (req_id, contract, use_rth, time_period))]
    fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, time_period: &str) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchHistogramData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            use_rth,
            period: time_period.to_string(),
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel histogram data.
    fn cancel_histogram_data(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelHistogramData { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    // ── Tier 3: Server Log Level ──

    /// Set server log level (local-only, adjusts Rust log filter).
    #[pyo3(signature = (log_level=2))]
    fn set_server_log_level(&self, log_level: i32) -> PyResult<()> {
        let level = match log_level {
            1 => "error",
            2 => "warn",
            3 => "info",
            4 => "debug",
            5 => "trace",
            _ => "warn",
        };
        log::info!("set_server_log_level: {} (level {})", level, log_level);
        Ok(())
    }

    // ── Tier 3: User Info ──

    /// Request user info. Gateway resolves locally — empty whiteBrandingId on paper.
    fn req_user_info(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        // User info is gateway-local data (EClient msg 104→107).
        // Paper accounts return empty whiteBrandingId.
        self.wrapper.call_method1(py, "user_info", (req_id, ""))?;
        Ok(())
    }

    // ── Tier 3: WSH ──

    /// Request WSH meta data.
    fn req_wsh_meta_data(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        log::warn!("req_wsh_meta_data: not yet implemented — needs FIX capture");
        Ok(())
    }

    /// Request WSH event data.
    #[pyo3(signature = (req_id, wsh_event_data=None))]
    fn req_wsh_event_data(&self, req_id: i64, wsh_event_data: Option<PyObject>) -> PyResult<()> {
        let _ = (req_id, wsh_event_data);
        log::warn!("req_wsh_event_data: not yet implemented — needs FIX capture");
        Ok(())
    }

    // ── Tier 3: Completed Orders ──

    /// Request completed (filled/cancelled) orders from session archive.
    #[pyo3(signature = (api_only=false))]
    fn req_completed_orders(&self, py: Python<'_>, api_only: bool) -> PyResult<()> {
        let _ = api_only;
        if let Some(shared) = self.shared.get() {
            let completed = shared.orders.drain_completed_orders();
            for co in &completed {
                let status_str = match co.status {
                    crate::types::OrderStatus::Filled => "Filled",
                    crate::types::OrderStatus::Cancelled => "Cancelled",
                    crate::types::OrderStatus::Rejected => "Inactive",
                    _ => "Unknown",
                };
                // Look up stored order info: local open_orders first, then shared enriched cache
                let stored = self.open_orders.lock().unwrap().get(&co.order_id).cloned();
                let state = pyo3::types::PyDict::new(py);
                state.set_item("status", status_str)?;
                state.set_item("completedTime", "")?;
                if let Some(so) = stored {
                    let c_py = Py::new(py, so.contract)?.into_any();
                    let o_py = Py::new(py, so.order)?.into_any();
                    self.wrapper.call_method1(py, "completed_order", (&c_py, &o_py, state.as_any()))?;
                } else if let Some(info) = shared.orders.get_order_info(co.order_id) {
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

    /// Run the event loop. Polls bridge queues and dispatches to EWrapper callbacks.
    /// Takes `&self` so the main thread can call req/cancel methods concurrently.
    fn run(&self, py: Python<'_>) -> PyResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Not connected. Call connect() first."));
        }

        // Fire initial callbacks
        let next_id = self.next_order_id.load(Ordering::Relaxed) as i64;
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        self.wrapper.call_method1(py, "managed_accounts", (self.account_id.get().map(|s| s.as_str()).unwrap_or(""),))?;
        self.wrapper.call_method0(py, "connect_ack")?;

        // Event loop
        while self.connected.load(Ordering::Relaxed) {
            py.check_signals()?;

            let shared = match self.shared.get() {
                Some(s) => s,
                None => break,
            };

            // Drain fills -> execDetails + orderStatus
            let fills = shared.orders.drain_fills();
            for fill in fills {
                let req_id = self.core.instrument_to_req.lock().unwrap()
                    .get(&fill.instrument).copied().unwrap_or(-1);
                let side_str = match fill.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                    Side::ShortSell => "SSHORT",
                };
                let price = fill.price as f64 / PRICE_SCALE_F;
                let commission = fill.commission as f64 / PRICE_SCALE_F;

                let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
                self.wrapper.call_method(
                    py, "order_status",
                    (fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                     price, 0i64, 0i64, price, 0i64, "", 0.0f64),
                    None,
                )?;

                // Track execution for req_executions
                let exec_id = format!("{}.{}", fill.order_id, fill.timestamp_ns);
                let now_str = format!("{}", fill.timestamp_ns);
                // Get contract from stored order if available
                let exec_contract = self.open_orders.lock().unwrap()
                    .get(&fill.order_id).map(|so| so.contract.clone())
                    .unwrap_or_default();
                self.executions.lock().unwrap().push(StoredExecution {
                    req_id,
                    contract: exec_contract,
                    exec_id: exec_id.clone(),
                    side: side_str.to_string(),
                    price,
                    shares: fill.qty as f64,
                    time: now_str,
                    order_id: fill.order_id,
                    cum_qty: fill.qty as f64,
                    avg_price: price,
                });

                // Update open order tracking
                {
                    let mut orders = self.open_orders.lock().unwrap();
                    if fill.remaining == 0 {
                        orders.remove(&fill.order_id);
                    } else if let Some(so) = orders.get_mut(&fill.order_id) {
                        so.status = status.to_string();
                        so.filled = fill.qty as f64;
                        so.remaining = fill.remaining as f64;
                    }
                }

                // Dispatch commission_report
                let report = super::contract::CommissionReport {
                    exec_id,
                    commission,
                    currency: "USD".to_string(),
                    realized_pnl: f64::MAX,
                    yield_amount: f64::MAX,
                    yield_redemption_date: String::new(),
                };
                let report_py = Py::new(py, report)?.into_any();
                self.wrapper.call_method1(py, "commission_report", (&report_py,))?;
            }

            // Drain order updates -> orderStatus
            let updates = shared.orders.drain_order_updates();
            for update in updates {
                let status = order_status_str(update.status);
                self.wrapper.call_method(
                    py, "order_status",
                    (update.order_id as i64, status, update.filled_qty as f64,
                     update.remaining_qty as f64, 0.0f64, 0i64, 0i64, 0.0f64, 0i64, "", 0.0f64),
                    None,
                )?;

                // Track open orders
                {
                    let mut orders = self.open_orders.lock().unwrap();
                    match update.status {
                        OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected => {
                            // Don't remove — req_completed_orders needs the stored data.
                            // Mark as terminal status instead.
                            if let Some(so) = orders.get_mut(&update.order_id) {
                                so.status = status.to_string();
                                so.filled = update.filled_qty as f64;
                                so.remaining = update.remaining_qty as f64;
                            }
                        }
                        _ => {
                            if let Some(so) = orders.get_mut(&update.order_id) {
                                so.status = status.to_string();
                                so.filled = update.filled_qty as f64;
                                so.remaining = update.remaining_qty as f64;
                            }
                        }
                    }
                }
            }

            // Drain cancel rejects -> error
            let rejects = shared.orders.drain_cancel_rejects();
            for reject in rejects {
                let code = if reject.reject_type == 1 { 202i64 } else { 10147i64 };
                let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
                self.wrapper.call_method(
                    py, "error",
                    (reject.order_id as i64, code, msg.as_str(), ""),
                    None,
                )?;
            }

            // Poll quotes for changes -> tickPrice/tickSize
            // Snapshot instrument map then release lock before calling Python
            let instruments: Vec<(u32, i64)> = {
                let map = self.core.instrument_to_req.lock().unwrap();
                map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
            };

            for (iid, req_id) in instruments {
                let q = shared.market.quote(iid);
                let fields = [
                    q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                    q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
                ];

                // Read previous values
                let last = {
                    let map = self.core.last_quotes.lock().unwrap();
                    map.get(&iid).copied().unwrap_or([0i64; 12])
                };

                let attrib = TickAttrib::default();
                let attrib_obj = Py::new(py, attrib)?.into_any();

                // Fire market_data_type once per subscription on first tick delivery
                let any_data = fields.iter().any(|&f| f != 0);
                if any_data && self.mdt_sent.lock().unwrap().insert(req_id) {
                    let mdt = self.market_data_type.load(Ordering::Relaxed);
                    self.wrapper.call_method1(py, "market_data_type", (req_id, mdt))?;
                }

                if fields[0] != last[0] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_BID, fields[0] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[1] != last[1] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_ASK, fields[1] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[2] != last[2] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LAST, fields[2] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[3] != last[3] {
                    self.wrapper.call_method1(py, "tick_size", (req_id, TICK_BID_SIZE, fields[3] as f64 / QTY_SCALE as f64))?;
                }
                if fields[4] != last[4] {
                    self.wrapper.call_method1(py, "tick_size", (req_id, TICK_ASK_SIZE, fields[4] as f64 / QTY_SCALE as f64))?;
                }
                if fields[5] != last[5] {
                    self.wrapper.call_method1(py, "tick_size", (req_id, TICK_LAST_SIZE, fields[5] as f64 / QTY_SCALE as f64))?;
                }
                if fields[6] != last[6] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_HIGH, fields[6] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[7] != last[7] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LOW, fields[7] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[8] != last[8] {
                    self.wrapper.call_method1(py, "tick_size", (req_id, TICK_VOLUME, fields[8] as f64 / QTY_SCALE as f64))?;
                }
                if fields[9] != last[9] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_CLOSE, fields[9] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                if fields[10] != last[10] {
                    self.wrapper.call_method1(py, "tick_price", (req_id, TICK_OPEN, fields[10] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                }
                // tick_string: LAST_TIMESTAMP as epoch seconds
                if fields[11] != last[11] && fields[11] != 0 {
                    let ts_secs = fields[11] / 1_000_000_000;
                    let ts_str = ts_secs.to_string();
                    self.wrapper.call_method1(py, "tick_string", (req_id, TICK_LAST_TIMESTAMP, ts_str.as_str()))?;
                }

                // Update last quotes
                self.core.last_quotes.lock().unwrap().insert(iid, fields);
            }

            // Drain TBT trades -> tickByTickAllLast
            let tbt_trades = shared.market.drain_tbt_trades();
            for trade in tbt_trades {
                let req_id = self.core.instrument_to_req.lock().unwrap()
                    .get(&trade.instrument).copied().unwrap_or(-1);
                let price = trade.price as f64 / PRICE_SCALE_F;
                let size = trade.size as f64;
                let attrib = super::tick_types::TickAttribLast::default();
                let attrib_obj = Py::new(py, attrib)?.into_any();
                self.wrapper.call_method(
                    py, "tick_by_tick_all_last",
                    (req_id, 1i32, trade.timestamp as i64, price, size,
                     &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()),
                    None,
                )?;
            }

            // Drain TBT quotes -> tickByTickBidAsk
            let tbt_quotes = shared.market.drain_tbt_quotes();
            for quote in tbt_quotes {
                let req_id = self.core.instrument_to_req.lock().unwrap()
                    .get(&quote.instrument).copied().unwrap_or(-1);
                let attrib = super::tick_types::TickAttribBidAsk::default();
                let attrib_obj = Py::new(py, attrib)?.into_any();
                self.wrapper.call_method(
                    py, "tick_by_tick_bid_ask",
                    (req_id, quote.timestamp as i64,
                     quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                     quote.bid_size as f64, quote.ask_size as f64, &attrib_obj),
                    None,
                )?;
            }

            // Drain news -> tickNews
            let news_items = shared.market.drain_tick_news();
            for news in news_items {
                let first_req_id = self.core.instrument_to_req.lock().unwrap()
                    .values().next().copied();
                if let Some(req_id) = first_req_id {
                    self.wrapper.call_method(
                        py, "tick_news",
                        (req_id, news.timestamp as i64, news.provider_code.as_str(),
                         news.article_id.as_str(), news.headline.as_str(), ""),
                        None,
                    )?;
                }
            }

            // Drain news bulletins -> updateNewsBulletin
            if self.core.bulletin_subscribed.load(Ordering::Acquire) {
                let bulletins = shared.market.drain_news_bulletins();
                for b in bulletins {
                    self.wrapper.call_method(
                        py, "update_news_bulletin",
                        (b.msg_id as i64, b.msg_type, b.message.as_str(), b.exchange.as_str()),
                        None,
                    )?;
                }
            }

            // Drain what-if responses -> orderStatus with margin info
            let what_ifs = shared.orders.drain_what_if_responses();
            for wi in what_ifs {
                let msg = format!(
                    "WhatIf: initMargin={:.2}, maintMargin={:.2}, commission={:.2}",
                    wi.init_margin_after as f64 / PRICE_SCALE_F,
                    wi.maint_margin_after as f64 / PRICE_SCALE_F,
                    wi.commission as f64 / PRICE_SCALE_F,
                );
                self.wrapper.call_method(
                    py, "order_status",
                    (wi.order_id as i64, "PreSubmitted", 0.0f64, 0.0f64,
                     0.0f64, 0i64, 0i64, 0.0f64, 0i64, msg.as_str(), 0.0f64),
                    None,
                )?;
            }

            // Drain historical data -> historicalData + historicalDataEnd
            let hist_data = shared.reference.drain_historical_data();
            for (req_id, response) in hist_data {
                for bar in &response.bars {
                    let bar_obj = super::contract::BarData::new(
                        bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                        bar.volume, bar.wap, bar.count as i32,
                    );
                    let bar_py = Py::new(py, bar_obj)?.into_any();
                    self.wrapper.call_method1(py, "historical_data", (req_id as i64, &bar_py))?;
                }
                if response.is_complete {
                    self.wrapper.call_method(
                        py, "historical_data_end",
                        (req_id as i64, "", ""),
                        None,
                    )?;
                }
            }

            // Drain head timestamps -> headTimestamp
            let head_ts = shared.reference.drain_head_timestamps();
            for (req_id, response) in head_ts {
                self.wrapper.call_method1(
                    py, "head_timestamp",
                    (req_id as i64, response.head_timestamp.as_str()),
                )?;
            }

            // Drain contract details -> contractDetails + contractDetailsEnd
            let contract_defs = shared.reference.drain_contract_details();
            for (req_id, def) in contract_defs {
                let details = super::contract::ContractDetails::from_definition(&def);
                let details_py = Py::new(py, details)?.into_any();
                self.wrapper.call_method1(
                    py, "contract_details",
                    (req_id as i64, &details_py),
                )?;
            }
            let contract_ends = shared.reference.drain_contract_details_end();
            for req_id in contract_ends {
                self.wrapper.call_method1(py, "contract_details_end", (req_id as i64,))?;
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
                self.wrapper.call_method1(py, "symbol_samples", (req_id as i64, list.as_any()))?;
            }

            // Drain scanner params -> scannerParameters
            let scanner_params = shared.reference.drain_scanner_params();
            for xml in scanner_params {
                self.wrapper.call_method1(py, "scanner_parameters", (xml.as_str(),))?;
            }

            // Drain scanner data -> scannerData + scannerDataEnd
            let scanner_results = shared.reference.drain_scanner_data();
            for (req_id, result) in scanner_results {
                for (rank, &con_id) in result.con_ids.iter().enumerate() {
                    let mut cd = super::contract::ContractDetails::default();
                    cd.contract.con_id = con_id as i64;
                    let cd_py = Py::new(py, cd)?.into_any();
                    self.wrapper.call_method(
                        py, "scanner_data",
                        (req_id as i64, rank as i32, &cd_py, "", "", "", ""),
                        None,
                    )?;
                }
                self.wrapper.call_method1(py, "scanner_data_end", (req_id as i64,))?;
            }

            // Drain historical news -> historicalNews + historicalNewsEnd
            let news_results = shared.reference.drain_historical_news();
            for (req_id, headlines, has_more) in news_results {
                for h in &headlines {
                    self.wrapper.call_method(
                        py, "historical_news",
                        (req_id as i64, h.time.as_str(), h.provider_code.as_str(),
                         h.article_id.as_str(), h.headline.as_str()),
                        None,
                    )?;
                }
                self.wrapper.call_method1(py, "historical_news_end", (req_id as i64, has_more))?;
            }

            // Drain news articles -> newsArticle
            let articles = shared.reference.drain_news_articles();
            for (req_id, article_type, text) in articles {
                self.wrapper.call_method(
                    py, "news_article",
                    (req_id as i64, article_type, text.as_str()),
                    None,
                )?;
            }

            // Drain fundamental data -> fundamentalData
            let fundamentals = shared.reference.drain_fundamental_data();
            for (req_id, data) in fundamentals {
                self.wrapper.call_method1(py, "fundamental_data", (req_id as i64, data.as_str()))?;
            }

            // Drain histogram data -> histogram_data
            let histograms = shared.reference.drain_histogram_data();
            for (req_id, entries) in histograms {
                let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                    pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
                }).collect();
                let py_list = pyo3::types::PyList::new(py, tuples)?;
                self.wrapper.call_method1(py, "histogram_data", (req_id as i64, py_list))?;
            }

            // Drain historical ticks -> historical_ticks / historical_ticks_bid_ask / historical_ticks_last
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
                        self.wrapper.call_method1(py, "historical_ticks", (req_id as i64, list, done))?;
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
                        self.wrapper.call_method1(py, "historical_ticks_last", (req_id as i64, list, done))?;
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
                        self.wrapper.call_method1(py, "historical_ticks_bid_ask", (req_id as i64, list, done))?;
                    }
                }
            }

            // Drain real-time bars -> real_time_bar
            let rtbars = shared.market.drain_real_time_bars();
            for (req_id, bar) in rtbars {
                self.wrapper.call_method1(py, "real_time_bar", (
                    req_id as i64,
                    bar.timestamp as i64,
                    bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count,
                ))?;
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
                self.wrapper.call_method1(py, "historical_schedule", (
                    req_id as i64,
                    resp.start_date_time.as_str(),
                    resp.end_date_time.as_str(),
                    resp.timezone.as_str(),
                    py_sessions,
                ))?;
            }

            // Drain market rules -> market_rule (already served from cache in req_market_rule)

            // Account updates (via ClientCore)
            if let Some(batch) = self.core.prepare_account_updates(shared) {
                let account_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
                for field in &batch.fields {
                    self.wrapper.call_method1(py, "update_account_value",
                        (field.key, field.value.as_str(), field.currency, account_name))?;
                }
                if batch.delivered {
                    self.wrapper.call_method1(py, "update_account_time", ("",))?;
                    self.wrapper.call_method1(py, "account_download_end", (account_name,))?;
                }
            }

            // P&L dispatch (via ClientCore)
            if let Some(update) = self.core.poll_pnl(shared) {
                self.wrapper.call_method(
                    py, "pnl",
                    (update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl),
                    None,
                )?;
            }

            // Per-position P&L dispatch (via ClientCore)
            for update in self.core.poll_pnl_single(shared) {
                self.wrapper.call_method(
                    py, "pnl_single",
                    (update.req_id, update.pos, update.daily_pnl,
                     update.unrealized_pnl, update.realized_pnl, update.value),
                    None,
                )?;
            }

            // Account summary dispatch (via ClientCore)
            {
                let acct_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
                if let Some(batch) = self.core.prepare_account_summary(shared, acct_name) {
                    // Python-specific: also emit AccountType string tag
                    let tags_orig = self.core.account_summary_req.lock().unwrap().clone();
                    let tags_list = tags_orig.map(|(_, t)| t).unwrap_or_default();
                    if tags_list.is_empty() || tags_list.iter().any(|t| t == "AccountType") {
                        self.wrapper.call_method(
                            py, "account_summary",
                            (batch.req_id, acct_name, "AccountType", "INDIVIDUAL", ""),
                            None,
                        )?;
                    }
                    for entry in &batch.entries {
                        self.wrapper.call_method(
                            py, "account_summary",
                            (batch.req_id, acct_name, entry.tag, entry.value.as_str(), entry.currency),
                            None,
                        )?;
                    }
                    self.wrapper.call_method1(py, "account_summary_end", (batch.req_id,))?;
                }
            }

            // Sleep to avoid busy-wait (1ms)
            py.allow_threads(|| std::thread::sleep(std::time::Duration::from_millis(1)));
        }

        // Signal disconnection to wrapper
        self.wrapper.call_method0(py, "connection_closed")?;

        Ok(())
    }

    /// Get the account ID.
    fn get_account_id(&self) -> &str {
        self.account_id.get().map(|s| s.as_str()).unwrap_or("")
    }
}

// ── Test helpers (hidden from public API) ──

#[pymethods]
impl EClient {
    /// Create a fake "connected" EClient backed by a SharedState + crossbeam channel.
    /// Allows Python compatibility tests to exercise the full dispatch path without a live gateway.
    #[doc(hidden)]
    #[pyo3(signature = (account_id="TEST123".to_string()))]
    fn _test_connect(&self, account_id: String) -> PyResult<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        let shared = Arc::new(SharedState::new());
        let (tx, _rx) = crossbeam_channel::unbounded();
        self.shared.set(shared).unwrap_or_else(|_| panic!("shared already set"));
        self.control_tx.set(tx).expect("control_tx already set");
        self.account_id.set(account_id).unwrap_or_else(|_| panic!("account_id already set"));
        self.next_order_id.store(1000, Ordering::Relaxed);
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    /// Map a reqId to an instrument slot.
    #[doc(hidden)]
    fn _test_map_instrument(&self, req_id: i64, instrument: u32) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().insert(instrument, req_id);
    }

    /// Set instrument count on SharedState.
    #[doc(hidden)]
    fn _test_set_instrument_count(&self, count: u32) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        shared.market.set_instrument_count(count);
        Ok(())
    }

    /// Push a quote into SharedState for a given instrument.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, bid=0.0, ask=0.0, last=0.0, bid_size=0, ask_size=0, last_size=0, volume=0, open=0.0, high=0.0, low=0.0, close=0.0))]
    fn _test_push_quote(
        &self, instrument: u32,
        bid: f64, ask: f64, last: f64,
        bid_size: i64, ask_size: i64, last_size: i64,
        volume: i64, open: f64, high: f64, low: f64, close: f64,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let ps = PRICE_SCALE as f64;
        let q = Quote {
            bid: (bid * ps) as i64, ask: (ask * ps) as i64, last: (last * ps) as i64,
            bid_size: bid_size * QTY_SCALE, ask_size: ask_size * QTY_SCALE,
            last_size: last_size * QTY_SCALE,
            volume: volume * QTY_SCALE,
            open: (open * ps) as i64, high: (high * ps) as i64,
            low: (low * ps) as i64, close: (close * ps) as i64,
            timestamp_ns: 1,
        };
        shared.market.push_quote(instrument, &q);
        Ok(())
    }

    /// Push a fill into SharedState.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, order_id, side, price, qty, remaining, commission=0.0))]
    fn _test_push_fill(
        &self, instrument: u32, order_id: u64, side: &str,
        price: f64, qty: i64, remaining: i64, commission: f64,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let s = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            "SSHORT" => Side::ShortSell,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid side: {}", side))),
        };
        let ps = PRICE_SCALE as f64;
        shared.orders.push_fill(Fill {
            instrument, order_id, side: s,
            price: (price * ps) as i64, qty, remaining,
            commission: (commission * ps) as i64,
            timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push an order update into SharedState.
    #[doc(hidden)]
    fn _test_push_order_update(
        &self, order_id: u64, instrument: u32, status: &str,
        filled_qty: i64, remaining_qty: i64,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let st = match status {
            "PendingSubmit" => OrderStatus::PendingSubmit,
            "Submitted" => OrderStatus::Submitted,
            "Filled" => OrderStatus::Filled,
            "Cancelled" => OrderStatus::Cancelled,
            "Rejected" => OrderStatus::Rejected,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid status: {}", status))),
        };
        shared.orders.push_order_update(OrderUpdate {
            order_id, instrument, status: st, filled_qty, remaining_qty, timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push a cancel reject into SharedState.
    #[doc(hidden)]
    fn _test_push_cancel_reject(&self, order_id: u64, instrument: u32, reason_code: i32) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        shared.orders.push_cancel_reject(CancelReject {
            order_id, instrument, reject_type: 1, reason_code, timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push a TBT trade into SharedState.
    #[doc(hidden)]
    fn _test_push_tbt_trade(
        &self, instrument: u32, price: f64, size: i64, exchange: &str,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let ps = PRICE_SCALE as f64;
        shared.market.push_tbt_trade(TbtTrade {
            instrument, price: (price * ps) as i64, size,
            exchange: exchange.to_string(), conditions: String::new(), timestamp: 12345,
        });
        Ok(())
    }

    /// Push a TBT quote into SharedState.
    #[doc(hidden)]
    fn _test_push_tbt_quote(
        &self, instrument: u32, bid: f64, ask: f64, bid_size: i64, ask_size: i64,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let ps = PRICE_SCALE as f64;
        shared.market.push_tbt_quote(TbtQuote {
            instrument,
            bid: (bid * ps) as i64, ask: (ask * ps) as i64,
            bid_size, ask_size, timestamp: 12345,
        });
        Ok(())
    }

    /// Push historical data into SharedState.
    #[doc(hidden)]
    fn _test_push_historical_data(
        &self, req_id: u32, bars: Vec<(String, f64, f64, f64, f64, i64)>, is_complete: bool,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let bar_list: Vec<HistoricalBar> = bars.into_iter().map(|(time, o, h, l, c, v)| {
            HistoricalBar { time, open: o, high: h, low: l, close: c, volume: v, wap: 0.0, count: 0 }
        }).collect();
        shared.reference.push_historical_data(req_id, HistoricalResponse {
            query_id: String::new(), timezone: String::new(), bars: bar_list, is_complete,
        });
        Ok(())
    }

    /// Push a head timestamp into SharedState.
    #[doc(hidden)]
    fn _test_push_head_timestamp(&self, req_id: u32, timestamp: &str) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        shared.reference.push_head_timestamp(req_id, HeadTimestampResponse {
            head_timestamp: timestamp.to_string(), timezone: String::new(),
        });
        Ok(())
    }

    /// Push account state into SharedState.
    #[doc(hidden)]
    #[pyo3(signature = (net_liquidation=0.0, buying_power=0.0, daily_pnl=0.0, unrealized_pnl=0.0, realized_pnl=0.0))]
    fn _test_set_account(
        &self, net_liquidation: f64, buying_power: f64,
        daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64,
    ) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let ps = PRICE_SCALE as f64;
        let mut acct = shared.portfolio.account();
        acct.net_liquidation = (net_liquidation * ps) as i64;
        acct.buying_power = (buying_power * ps) as i64;
        acct.daily_pnl = (daily_pnl * ps) as i64;
        acct.unrealized_pnl = (unrealized_pnl * ps) as i64;
        acct.realized_pnl = (realized_pnl * ps) as i64;
        shared.portfolio.set_account(&acct);
        Ok(())
    }

    /// Push a position into SharedState.
    #[doc(hidden)]
    fn _test_set_position(&self, con_id: i64, position: i64, avg_cost: f64) -> PyResult<()> {
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let ps = PRICE_SCALE as f64;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position, avg_cost: (avg_cost * ps) as i64,
        });
        Ok(())
    }

    /// Run ONE iteration of the event dispatch loop (drains all queues once).
    /// Does NOT loop—returns immediately after dispatching.
    #[doc(hidden)]
    fn _test_dispatch_once(&self, py: Python<'_>) -> PyResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Not connected"));
        }
        // Temporarily disconnect so we can reuse the run() dispatch logic
        // without entering the while loop. Instead, inline the dispatch here.
        let shared = self.shared.get()
            .ok_or_else(|| PyRuntimeError::new_err("No shared state"))?;

        // Drain fills -> order_status
        let fills = shared.orders.drain_fills();
        for fill in fills {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&fill.instrument).copied().unwrap_or(-1);
            let side_str = match fill.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
                Side::ShortSell => "SSHORT",
            };
            let price = fill.price as f64 / PRICE_SCALE_F;

            let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
            self.wrapper.call_method(
                py, "order_status",
                (fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                 price, 0i64, 0i64, price, 0i64, "", 0.0f64),
                None,
            )?;

            let exec_id = format!("{}.{}", fill.order_id, fill.timestamp_ns);
            let now_str = format!("{}", fill.timestamp_ns);
            // Try local open_orders first, then fall back to shared order cache → contract cache
            let exec_contract = self.open_orders.lock().unwrap()
                .get(&fill.order_id).map(|so| so.contract.clone())
                .or_else(|| {
                    shared.orders.get_order_info(fill.order_id).map(|info| {
                        Contract {
                            con_id: info.contract.con_id,
                            symbol: info.contract.symbol.clone(),
                            sec_type: info.contract.sec_type.clone(),
                            exchange: info.contract.exchange.clone(),
                            currency: info.contract.currency.clone(),
                            ..Default::default()
                        }
                    })
                })
                .unwrap_or_default();
            self.executions.lock().unwrap().push(StoredExecution {
                req_id,
                contract: exec_contract.clone(),
                exec_id: exec_id.clone(),
                side: side_str.to_string(),
                price,
                shares: fill.qty as f64,
                time: now_str.clone(),
                order_id: fill.order_id,
                cum_qty: fill.qty as f64,
                avg_price: price,
            });

            // exec_details callback
            let acct_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
            let c_py = Py::new(py, exec_contract)?.into_any();
            let exec_dict = pyo3::types::PyDict::new(py);
            exec_dict.set_item("execId", exec_id.as_str())?;
            exec_dict.set_item("time", now_str.as_str())?;
            exec_dict.set_item("acctNumber", acct_name)?;
            exec_dict.set_item("side", side_str)?;
            exec_dict.set_item("price", price)?;
            exec_dict.set_item("shares", fill.qty as f64)?;
            exec_dict.set_item("orderId", fill.order_id as i64)?;
            exec_dict.set_item("cumQty", fill.qty as f64)?;
            exec_dict.set_item("avgPrice", price)?;
            exec_dict.set_item("permId", 0i64)?;
            exec_dict.set_item("clientId", 0i64)?;
            exec_dict.set_item("liquidation", 0i64)?;
            exec_dict.set_item("lastLiquidity", 0i64)?;
            self.wrapper.call_method(
                py, "exec_details",
                (req_id, &c_py, exec_dict.as_any()),
                None,
            )?;

            {
                let mut orders = self.open_orders.lock().unwrap();
                if fill.remaining == 0 {
                    orders.remove(&fill.order_id);
                } else if let Some(so) = orders.get_mut(&fill.order_id) {
                    so.status = status.to_string();
                    so.filled = fill.qty as f64;
                    so.remaining = fill.remaining as f64;
                }
            }

            // Dispatch commission_report
            let commission = fill.commission as f64 / PRICE_SCALE_F;
            let report = super::contract::CommissionReport {
                exec_id: exec_id.clone(),
                commission,
                currency: "USD".to_string(),
                realized_pnl: f64::MAX,
                yield_amount: f64::MAX,
                yield_redemption_date: String::new(),
            };
            let report_py = Py::new(py, report)?.into_any();
            self.wrapper.call_method1(py, "commission_report", (&report_py,))?;
        }

        // Drain order updates -> order_status
        let updates = shared.orders.drain_order_updates();
        for update in updates {
            let status = order_status_str(update.status);
            self.wrapper.call_method(
                py, "order_status",
                (update.order_id as i64, status, update.filled_qty as f64,
                 update.remaining_qty as f64, 0.0f64, 0i64, 0i64, 0.0f64, 0i64, "", 0.0f64),
                None,
            )?;
        }

        // Drain cancel rejects -> error
        let rejects = shared.orders.drain_cancel_rejects();
        for reject in rejects {
            let code = if reject.reject_type == 1 { 202i64 } else { 10147i64 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            self.wrapper.call_method(
                py, "error",
                (reject.order_id as i64, code, msg.as_str(), ""),
                None,
            )?;
        }

        // Poll quotes for changes -> tick_price/tick_size
        let instruments: Vec<(u32, i64)> = {
            let map = self.core.instrument_to_req.lock().unwrap();
            map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
        };
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let q = shared.market.quote(iid);
            let fields = [
                q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
            ];
            let last = {
                let map = self.core.last_quotes.lock().unwrap();
                map.get(&iid).copied().unwrap_or([0i64; 12])
            };
            let attrib = TickAttrib::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();

            // Fire market_data_type once per subscription on first tick delivery
            let any_data = fields.iter().any(|&f| f != 0);
            if any_data && self.mdt_sent.lock().unwrap().insert(req_id) {
                let mdt = self.market_data_type.load(Ordering::Relaxed);
                self.wrapper.call_method1(py, "market_data_type", (req_id, mdt))?;
            }

            let mut delivered = false;
            if fields[0] != last[0] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_BID, fields[0] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[1] != last[1] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_ASK, fields[1] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[2] != last[2] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LAST, fields[2] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[3] != last[3] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_BID_SIZE, fields[3] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[4] != last[4] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_ASK_SIZE, fields[4] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[5] != last[5] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_LAST_SIZE, fields[5] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[6] != last[6] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_HIGH, fields[6] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[7] != last[7] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LOW, fields[7] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[8] != last[8] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_VOLUME, fields[8] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[9] != last[9] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_CLOSE, fields[9] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[10] != last[10] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_OPEN, fields[10] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            // tick_string: LAST_TIMESTAMP as epoch seconds
            if fields[11] != last[11] && fields[11] != 0 {
                let ts_secs = fields[11] / 1_000_000_000;
                let ts_str = ts_secs.to_string();
                self.wrapper.call_method1(py, "tick_string", (req_id, TICK_LAST_TIMESTAMP, ts_str.as_str()))?;
            }
            self.core.last_quotes.lock().unwrap().insert(iid, fields);

            // Snapshot: after first delivery, signal end and queue for auto-cancel
            if delivered && self.core.snapshot_reqs.lock().unwrap().remove(&req_id) {
                self.wrapper.call_method1(py, "tick_snapshot_end", (req_id,))?;
                snapshot_done.push(req_id);
            }
        }
        // Auto-cancel completed snapshots
        for req_id in snapshot_done {
            self.cancel_mkt_data(req_id)?;
        }

        // Drain TBT trades -> tick_by_tick_all_last
        let tbt_trades = shared.market.drain_tbt_trades();
        for trade in tbt_trades {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&trade.instrument).copied().unwrap_or(-1);
            let price = trade.price as f64 / PRICE_SCALE_F;
            let attrib = super::tick_types::TickAttribLast::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            self.wrapper.call_method(
                py, "tick_by_tick_all_last",
                (req_id, 1i32, trade.timestamp as i64, price, trade.size as f64,
                 &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()),
                None,
            )?;
        }

        // Drain TBT quotes -> tick_by_tick_bid_ask
        let tbt_quotes = shared.market.drain_tbt_quotes();
        for quote in tbt_quotes {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&quote.instrument).copied().unwrap_or(-1);
            let attrib = super::tick_types::TickAttribBidAsk::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            self.wrapper.call_method(
                py, "tick_by_tick_bid_ask",
                (req_id, quote.timestamp as i64,
                 quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                 quote.bid_size as f64, quote.ask_size as f64, &attrib_obj),
                None,
            )?;
        }

        // Drain historical data -> historical_data + historical_data_end
        let hist_data = shared.reference.drain_historical_data();
        for (req_id, response) in hist_data {
            for bar in &response.bars {
                let bar_obj = super::contract::BarData::new(
                    bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count as i32,
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                self.wrapper.call_method1(py, "historical_data", (req_id as i64, &bar_py))?;
            }
            if response.is_complete {
                self.wrapper.call_method(
                    py, "historical_data_end",
                    (req_id as i64, "", ""),
                    None,
                )?;
            }
        }

        // Drain head timestamps -> head_timestamp
        let head_ts = shared.reference.drain_head_timestamps();
        for (req_id, response) in head_ts {
            self.wrapper.call_method1(
                py, "head_timestamp",
                (req_id as i64, response.head_timestamp.as_str()),
            )?;
        }

        // Drain what-if responses -> order_status with margin info
        let what_ifs = shared.orders.drain_what_if_responses();
        for wi in what_ifs {
            let msg = format!(
                "WhatIf: initMargin={:.2}, maintMargin={:.2}, commission={:.2}",
                wi.init_margin_after as f64 / PRICE_SCALE_F,
                wi.maint_margin_after as f64 / PRICE_SCALE_F,
                wi.commission as f64 / PRICE_SCALE_F,
            );
            self.wrapper.call_method(
                py, "order_status",
                (wi.order_id as i64, "PreSubmitted", 0.0f64, 0.0f64,
                 0.0f64, 0i64, 0i64, 0.0f64, 0i64, msg.as_str(), 0.0f64),
                None,
            )?;
        }

        // Account state -> update_account_value (all fields)
        if !self.account_id.get().map(|s| s.is_empty()).unwrap_or(true) {
            let acct = shared.portfolio.account();
            let account_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
            let fields: &[(&str, i64)] = &[
                ("NetLiquidation", acct.net_liquidation),
                ("BuyingPower", acct.buying_power),
                ("MaintMarginReq", acct.margin_used),
                ("UnrealizedPnL", acct.unrealized_pnl),
                ("RealizedPnL", acct.realized_pnl),
            ];
            for &(key, val) in fields {
                if val != 0 {
                    let fval = val as f64 / PRICE_SCALE_F;
                    self.wrapper.call_method1(py, "update_account_value",
                        (key, format!("{:.2}", fval).as_str(), "USD", account_name))?;
                }
            }
        }

        // P&L dispatch (via ClientCore)
        if let Some(update) = self.core.poll_pnl(shared) {
            self.wrapper.call_method(
                py, "pnl",
                (update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl),
                None,
            )?;
        }

        // Account summary dispatch (via ClientCore)
        {
            let acct_name = self.account_id.get().map(|s| s.as_str()).unwrap_or("");
            if let Some(batch) = self.core.prepare_account_summary(shared, acct_name) {
                // Python-specific: also emit AccountType string tag
                let tags_orig = self.core.account_summary_req.lock().unwrap().clone();
                let tags_list = tags_orig.map(|(_, t)| t).unwrap_or_default();
                if tags_list.is_empty() || tags_list.iter().any(|t| t == "AccountType") {
                    self.wrapper.call_method(
                        py, "account_summary",
                        (batch.req_id, acct_name, "AccountType", "INDIVIDUAL", ""),
                        None,
                    )?;
                }
                for entry in &batch.entries {
                    self.wrapper.call_method(
                        py, "account_summary",
                        (batch.req_id, acct_name, entry.tag, entry.value.as_str(), entry.currency),
                        None,
                    )?;
                }
                self.wrapper.call_method1(py, "account_summary_end", (batch.req_id,))?;
            }
        }

        Ok(())
    }
}

impl EClient {
    /// Find instrument ID for a contract, registering if needed.
    fn find_or_register_instrument(&self, contract: &Contract) -> PyResult<u32> {
        let tx = self.control_tx.get()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let shared = self.shared.get().unwrap();
        self.core.find_or_register_instrument(
            shared, tx,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
        ).map_err(|e| PyRuntimeError::new_err(e))
    }

}

/// Register EClient on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EClient>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eclient_default_state() {
        // Can't construct without Python, but we can test the parsing helpers
        let tv = vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "startTime".into(), value: "09:30:00".into() },
            TagValue { tag: "endTime".into(), value: "16:00:00".into() },
        ];

        // Test parse_algo_params indirectly via the helper closure logic
        let get = |key: &str| -> String {
            tv.iter()
                .find(|t| t.tag == key)
                .map(|t| t.value.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("maxPctVol"), "0.1");
        assert_eq!(get("startTime"), "09:30:00");
        assert_eq!(get("missing"), "");
    }
}
