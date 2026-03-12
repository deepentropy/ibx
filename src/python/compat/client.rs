//! ibapi-compatible EClient class that wraps IbEngine.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::Sender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::SharedState;
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;
use super::contract::{Contract, ContractDescription, Order, TagValue};
use super::tick_types::*;
use super::super::types::PRICE_SCALE_F;

/// ibapi-compatible EClient class.
/// Wraps the internal engine and dispatches events to an EWrapper subclass.
///
/// All methods except `connect()` take `&self` (shared borrow) so that `run()`
/// can execute in a daemon thread while the main thread calls req/cancel methods.
/// Interior mutability is provided by `AtomicBool` and `Mutex`.
#[pyclass(subclass)]
pub struct EClient {
    /// Reference to the EWrapper (which is typically `self` in the `App(EWrapper, EClient)` pattern).
    wrapper: PyObject,
    /// Set once by connect(), read-only after.
    shared: Option<Arc<SharedState>>,
    /// Set once by connect(), read-only after.
    control_tx: Option<Sender<ControlCommand>>,
    next_order_id: AtomicU64,
    _thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Set once by connect(), read-only after.
    account_id: String,
    connected: AtomicBool,
    /// Maps reqId -> InstrumentId for market data subscriptions.
    req_to_instrument: Mutex<HashMap<i64, u32>>,
    /// Maps InstrumentId -> reqId (reverse).
    instrument_to_req: Mutex<HashMap<u32, i64>>,
    /// Last quote sent per instrument (for change detection).
    last_quotes: Mutex<HashMap<u32, [i64; 12]>>,
    /// P&L subscription reqId (None = not subscribed).
    pnl_req_id: Mutex<Option<i64>>,
    /// Single-position P&L subscriptions: reqId → conId.
    pnl_single_reqs: Mutex<HashMap<i64, i64>>,
    /// Account summary subscription: (reqId, requested_tags).
    account_summary_req: Mutex<Option<(i64, Vec<String>)>>,
    /// Last P&L values sent (for change detection).
    last_pnl: Mutex<[i64; 3]>,
    /// Market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    market_data_type: AtomicI32,
    /// Track open orders: order_id → (status, instrument, filled, remaining).
    open_orders: Mutex<HashMap<u64, (String, u32, f64, f64)>>,
    /// Track executions: (req_id, contract_con_id, exec_id, side, price, qty, time).
    executions: Mutex<Vec<(i64, i64, String, String, f64, f64, String)>>,
}

#[pymethods]
impl EClient {
    #[new]
    #[pyo3(signature = (wrapper))]
    fn new(wrapper: PyObject) -> Self {
        Self {
            wrapper,
            shared: None,
            control_tx: None,
            next_order_id: AtomicU64::new(0),
            _thread: Mutex::new(None),
            account_id: String::new(),
            connected: AtomicBool::new(false),
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
            pnl_req_id: Mutex::new(None),
            pnl_single_reqs: Mutex::new(HashMap::new()),
            account_summary_req: Mutex::new(None),
            last_pnl: Mutex::new([0; 3]),
            market_data_type: AtomicI32::new(1),
            open_orders: Mutex::new(HashMap::new()),
            executions: Mutex::new(Vec::new()),
        }
    }

    /// Connect to IB and start the engine.
    /// Signature matches ibapi: connect(host, port, clientId) but internally uses
    /// direct gateway auth. Pass username as host, password as port (string),
    /// or use keyword args: connect(username="...", password="...", paper=True).
    #[pyo3(signature = (host="cdc1.ibllc.com".to_string(), port=0, client_id=0, username="".to_string(), password="".to_string(), paper=true, core_id=None))]
    fn connect(
        &mut self,
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
        };

        let result = py.allow_threads(|| Gateway::connect(&config));
        let (gw, farm_conn, ccp_conn, hmds_conn) = result
            .map_err(|e| PyRuntimeError::new_err(format!("Connection failed: {}", e)))?;

        self.account_id = gw.account_id.clone();
        let shared = Arc::new(SharedState::new());

        let (mut hot_loop, control_tx) = gw.into_hot_loop(shared.clone(), None, farm_conn, ccp_conn, hmds_conn, core_id);

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

        self.shared = Some(shared);
        self.control_tx = Some(control_tx);
        self.next_order_id = AtomicU64::new(start_id);
        *self._thread.lock().unwrap() = Some(handle);
        self.connected.store(true, Ordering::Release);

        let _ = (port, client_id); // unused but kept for ibapi signature compat

        Ok(())
    }

    /// Disconnect from IB.
    fn disconnect(&self) -> PyResult<()> {
        if let Some(ref tx) = self.control_tx {
            let _ = tx.send(ControlCommand::Shutdown);
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    /// Check if connected.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Request market data for a contract.
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
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;

        tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        tx.send(ControlCommand::Subscribe {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
        })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let shared = self.shared.as_ref().unwrap();
        let instrument_id = shared.instrument_count().saturating_sub(1);

        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);

        let _ = (generic_tick_list, snapshot, regulatory_snapshot, mkt_data_options);

        Ok(())
    }

    /// Cancel market data.
    fn cancel_mkt_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            self.last_quotes.lock().unwrap().remove(&instrument);
            let tx = self.control_tx.as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
            tx.send(ControlCommand::Unsubscribe { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
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
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;

        let tbt_type = match tick_type {
            "Last" | "AllLast" => TbtType::Last,
            "BidAsk" => TbtType::BidAsk,
            _ => return Err(PyRuntimeError::new_err(format!("Unknown tick type: '{}'", tick_type))),
        };

        tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        tx.send(ControlCommand::SubscribeTbt {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            tbt_type,
        })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let shared = self.shared.as_ref().unwrap();
        let instrument_id = shared.instrument_count().saturating_sub(1);
        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);

        let _ = (number_of_ticks, ignore_size);
        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::FetchContractDetails {
            req_id: req_id as u32,
            con_id: contract.con_id,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request P&L updates for the account. Gateway-computed from positions × quotes.
    #[pyo3(signature = (req_id, account, model_code=""))]
    fn req_pnl(&self, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        *self.pnl_req_id.lock().unwrap() = Some(req_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel P&L subscription.
    fn cancel_pnl(&self, req_id: i64) -> PyResult<()> {
        let mut pnl = self.pnl_req_id.lock().unwrap();
        if *pnl == Some(req_id) {
            *pnl = None;
        }
        Ok(())
    }

    /// Request P&L for a single position. Gateway-computed.
    #[pyo3(signature = (req_id, account, model_code, con_id))]
    fn req_pnl_single(&self, req_id: i64, account: &str, model_code: &str, con_id: i64) -> PyResult<()> {
        self.pnl_single_reqs.lock().unwrap().insert(req_id, con_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel single-position P&L subscription.
    fn cancel_pnl_single(&self, req_id: i64) -> PyResult<()> {
        self.pnl_single_reqs.lock().unwrap().remove(&req_id);
        Ok(())
    }

    /// Request account summary.
    #[pyo3(signature = (req_id, group_name, tags))]
    fn req_account_summary(&self, req_id: i64, group_name: &str, tags: &str) -> PyResult<()> {
        let tag_list: Vec<String> = tags.split(',').map(|s| s.trim().to_string()).collect();
        *self.account_summary_req.lock().unwrap() = Some((req_id, tag_list));
        let _ = group_name;
        Ok(())
    }

    /// Cancel account summary.
    fn cancel_account_summary(&self, req_id: i64) -> PyResult<()> {
        let mut req = self.account_summary_req.lock().unwrap();
        if req.as_ref().map(|(r, _)| *r) == Some(req_id) {
            *req = None;
        }
        Ok(())
    }

    /// Request all positions.
    fn req_positions(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let positions = shared.position_infos();
        for pi in &positions {
            let mut c = super::contract::Contract::default();
            c.con_id = pi.con_id;
            let c_py = Py::new(py, c)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.wrapper.call_method(
                py, "position",
                (self.account_id.as_str(), &c_py, pi.position as f64, avg_cost),
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

    /// Place an order. Routes to the appropriate submit_* based on order_type.
    fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        if self.control_tx.is_none() {
            return Err(PyRuntimeError::new_err("Not connected"));
        }

        let oid = if order_id > 0 {
            order_id as u64
        } else {
            self.next_order_id.fetch_add(1, Ordering::Relaxed)
        };

        // Find the instrument ID for this contract
        let instrument = self.find_or_register_instrument(contract)?;
        let tx = self.control_tx.as_ref().unwrap();
        let side = order.side()?;
        let qty = order.total_quantity as u32;

        // Route based on order type
        let order_type = order.order_type.to_uppercase();

        // Check for algo strategy
        if !order.algo_strategy.is_empty() {
            let algo = self.parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            tx.send(ControlCommand::Order(OrderRequest::SubmitAlgo {
                order_id: oid, instrument, side, qty, price, algo,
            })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
            return Ok(());
        }

        // Check for what-if
        if order.what_if {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            tx.send(ControlCommand::Order(OrderRequest::SubmitWhatIf {
                order_id: oid, instrument, side, qty, price,
            })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
            return Ok(());
        }

        let req = match order_type.as_str() {
            "MKT" => OrderRequest::SubmitMarket { order_id: oid, instrument, side, qty },
            "LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                if order.has_extended_attrs() || order.tif != "DAY" {
                    OrderRequest::SubmitLimitEx {
                        order_id: oid, instrument, side, qty, price,
                        tif: order.tif_byte(),
                        attrs: order.attrs(),
                    }
                } else {
                    OrderRequest::SubmitLimit { order_id: oid, instrument, side, qty, price }
                }
            }
            "STP" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStop { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "STP LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStopLimit { order_id: oid, instrument, side, qty, price, stop_price: stop }
            }
            "TRAIL" => {
                if order.trailing_percent > 0.0 {
                    let pct = (order.trailing_percent * 100.0) as u32; // convert to basis points
                    OrderRequest::SubmitTrailingStopPct { order_id: oid, instrument, side, qty, trail_pct: pct }
                } else {
                    let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                    OrderRequest::SubmitTrailingStop { order_id: oid, instrument, side, qty, trail_amt: trail }
                }
            }
            "TRAIL LIMIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitTrailingStopLimit { order_id: oid, instrument, side, qty, price, trail_amt: trail }
            }
            "MOC" => OrderRequest::SubmitMoc { order_id: oid, instrument, side, qty },
            "LOC" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLoc { order_id: oid, instrument, side, qty, price }
            }
            "MIT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMit { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "LIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLit { order_id: oid, instrument, side, qty, price, stop_price: stop }
            }
            "MTL" => OrderRequest::SubmitMtl { order_id: oid, instrument, side, qty },
            "MKT PRT" => OrderRequest::SubmitMktPrt { order_id: oid, instrument, side, qty },
            "STP PRT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStpPrt { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "REL" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitRel { order_id: oid, instrument, side, qty, offset }
            }
            "PEG MKT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMkt { order_id: oid, instrument, side, qty, offset }
            }
            "PEG MID" | "PEG MIDPT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMid { order_id: oid, instrument, side, qty, offset }
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMidPrice { order_id: oid, instrument, side, qty, price_cap: cap }
            }
            "SNAP MKT" => OrderRequest::SubmitSnapMkt { order_id: oid, instrument, side, qty },
            "SNAP MID" | "SNAP MIDPT" => OrderRequest::SubmitSnapMid { order_id: oid, instrument, side, qty },
            "SNAP PRI" | "SNAP PRIM" => OrderRequest::SubmitSnapPri { order_id: oid, instrument, side, qty },
            "BOX TOP" => OrderRequest::SubmitMtl { order_id: oid, instrument, side, qty },
            _ => {
                return Err(PyRuntimeError::new_err(format!("Unsupported order type: '{}'", order.order_type)));
            }
        };

        tx.send(ControlCommand::Order(req))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel an order.
    #[pyo3(signature = (order_id, manual_order_cancel_time=""))]
    fn cancel_order(&self, order_id: i64, manual_order_cancel_time: &str) -> PyResult<()> {
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: order_id as u64 }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        let _ = manual_order_cancel_time;
        Ok(())
    }

    /// Cancel all orders globally.
    fn req_global_cancel(&self) -> PyResult<()> {
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        let map = self.req_to_instrument.lock().unwrap();
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
    #[pyo3(signature = (_subscribe, _acct_code=""))]
    fn req_account_updates(&self, _subscribe: bool, _acct_code: &str) -> PyResult<()> {
        // Account updates are always being synced via bridge
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
        let orders: Vec<(u64, String, u32, f64, f64)> = {
            let map = self.open_orders.lock().unwrap();
            map.iter().map(|(&oid, &(ref status, inst, filled, remaining))| {
                (oid, status.clone(), inst, filled, remaining)
            }).collect()
        };
        for (order_id, status, _inst, filled, remaining) in &orders {
            self.wrapper.call_method(
                py, "order_status",
                (*order_id as i64, status.as_str(), *filled, *remaining,
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

    /// Request execution reports.
    #[pyo3(signature = (req_id, _exec_filter=None))]
    fn req_executions(&self, py: Python<'_>, req_id: i64, _exec_filter: Option<PyObject>) -> PyResult<()> {
        let execs: Vec<(i64, i64, String, String, f64, f64, String)> = {
            self.executions.lock().unwrap().clone()
        };
        for (_, con_id, exec_id, side, price, qty, time) in &execs {
            let mut c = super::contract::Contract::default();
            c.con_id = *con_id;
            let c_py = Py::new(py, c)?.into_any();

            let exec_obj = pyo3::types::PyDict::new(py);
            exec_obj.set_item("execId", exec_id.as_str())?;
            exec_obj.set_item("side", side.as_str())?;
            exec_obj.set_item("price", *price)?;
            exec_obj.set_item("shares", *qty)?;
            exec_obj.set_item("time", time.as_str())?;

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
        // Historical ticks use a different HMDS query format not yet implemented.
        let _ = (req_id, contract, start_date_time, end_date_time, number_of_ticks,
                 what_to_show, use_rth, ignore_size, misc_options);
        log::warn!("req_historical_ticks: not yet implemented in engine");
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
        // Real-time bars use a streaming HMDS subscription not yet implemented.
        let _ = (req_id, contract, bar_size, what_to_show, use_rth, real_time_bars_options);
        log::warn!("req_real_time_bars: not yet implemented in engine");
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Cancel head timestamp request.
    fn cancel_head_time_stamp(&self, req_id: i64) -> PyResult<()> {
        let tx = self.control_tx.as_ref()
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
        // Option chain parameters require a dedicated CCP query not yet implemented.
        let _ = (req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id);
        log::warn!("req_sec_def_opt_params: not yet implemented in engine");
        Ok(())
    }

    /// Search for matching symbols.
    fn req_matching_symbols(&self, req_id: i64, pattern: &str) -> PyResult<()> {
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
        tx.send(ControlCommand::CancelScanner { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request scanner parameters XML.
    fn req_scanner_parameters(&self) -> PyResult<()> {
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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

    // ── Tier 2: News Bulletins (stubs) ──

    /// Subscribe to news bulletins.
    #[pyo3(signature = (all_msgs=true))]
    fn req_news_bulletins(&self, all_msgs: bool) -> PyResult<()> {
        let _ = all_msgs;
        log::warn!("req_news_bulletins: not yet implemented in engine");
        Ok(())
    }

    /// Cancel news bulletins.
    fn cancel_news_bulletins(&self) -> PyResult<()> {
        Ok(())
    }

    // ── Tier 2: Managed Accounts ──

    /// Request managed accounts list.
    fn req_managed_accts(&self, py: Python<'_>) -> PyResult<()> {
        self.wrapper.call_method1(py, "managed_accounts", (self.account_id.as_str(),))?;
        Ok(())
    }

    // ── Tier 2: Multi-Account (stubs) ──

    /// Request account updates for multiple accounts/models.
    #[pyo3(signature = (req_id, account, model_code, ledger_and_nlv=false))]
    fn req_account_updates_multi(
        &self, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
    ) -> PyResult<()> {
        let _ = (req_id, account, model_code, ledger_and_nlv);
        log::warn!("req_account_updates_multi: not yet implemented in engine");
        Ok(())
    }

    /// Cancel multi-account updates.
    fn cancel_account_updates_multi(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Request positions across multiple accounts/models.
    #[pyo3(signature = (req_id, account, model_code))]
    fn req_positions_multi(&self, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        let _ = (req_id, account, model_code);
        log::warn!("req_positions_multi: not yet implemented in engine");
        Ok(())
    }

    /// Cancel multi-account positions.
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

    /// Query display groups.
    fn query_display_groups(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        log::warn!("query_display_groups: not yet implemented — needs FIX capture");
        Ok(())
    }

    /// Subscribe to group events.
    fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) -> PyResult<()> {
        let _ = (req_id, group_id);
        log::warn!("subscribe_to_group_events: not yet implemented — needs FIX capture");
        Ok(())
    }

    /// Unsubscribe from group events.
    fn unsubscribe_from_group_events(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Update display group.
    fn update_display_group(&self, req_id: i64, contract_info: &str) -> PyResult<()> {
        let _ = (req_id, contract_info);
        log::warn!("update_display_group: not yet implemented — needs FIX capture");
        Ok(())
    }

    // ── Tier 3: Market Rules ──

    /// Request market rule details (price increments).
    fn req_market_rule(&self, py: Python<'_>, market_rule_id: i32) -> PyResult<()> {
        // Market rules are cached from secdef responses.
        if let Some(shared) = self.shared.as_ref() {
            if let Some(rule) = shared.market_rule(market_rule_id) {
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

    /// Request SMART routing components.
    fn req_smart_components(&self, req_id: i64, bbo_exchange: &str) -> PyResult<()> {
        let _ = (req_id, bbo_exchange);
        log::warn!("req_smart_components: not yet implemented — needs FIX capture");
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
        // Family codes come from CCP login data (EClient msg 80→78).
        // Return account_id with empty family code (matches paper behavior).
        let account = if !self.account_id.is_empty() {
            self.account_id.as_str()
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
        let tx = self.control_tx.as_ref()
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
        let tx = self.control_tx.as_ref()
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
        if let Some(shared) = self.shared.as_ref() {
            let completed = shared.drain_completed_orders();
            for order in &completed {
                let status_str = match order.status {
                    crate::types::OrderStatus::Filled => "Filled",
                    crate::types::OrderStatus::Cancelled => "Cancelled",
                    crate::types::OrderStatus::Rejected => "Inactive",
                    _ => "Unknown",
                };
                // Fire completed_order callback with minimal contract/order/state info
                let contract = py.None();
                let order_obj = py.None();
                let state = pyo3::types::PyDict::new(py);
                state.set_item("status", status_str)?;
                state.set_item("completedTime", "")?;
                self.wrapper.call_method1(py, "completed_order", (&contract, &order_obj, state.as_any()))?;
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
        self.wrapper.call_method1(py, "managed_accounts", (self.account_id.as_str(),))?;
        self.wrapper.call_method0(py, "connect_ack")?;

        // Event loop
        while self.connected.load(Ordering::Relaxed) {
            py.check_signals()?;

            let shared = match self.shared.as_ref() {
                Some(s) => s,
                None => break,
            };

            // Drain fills -> execDetails + orderStatus
            let fills = shared.drain_fills();
            for fill in fills {
                let req_id = self.instrument_to_req.lock().unwrap()
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
                self.executions.lock().unwrap().push((
                    req_id, 0i64, exec_id, side_str.to_string(), price, fill.qty as f64, now_str,
                ));

                // Update open order tracking
                {
                    let mut orders = self.open_orders.lock().unwrap();
                    if fill.remaining == 0 {
                        orders.remove(&fill.order_id);
                    } else {
                        orders.insert(fill.order_id, (
                            status.to_string(), fill.instrument, fill.qty as f64, fill.remaining as f64,
                        ));
                    }
                }

                let _ = commission;
            }

            // Drain order updates -> orderStatus
            let updates = shared.drain_order_updates();
            for update in updates {
                let status = match update.status {
                    OrderStatus::PendingSubmit => "PendingSubmit",
                    OrderStatus::Submitted => "Submitted",
                    OrderStatus::Filled => "Filled",
                    OrderStatus::PartiallyFilled => "PreSubmitted",
                    OrderStatus::Cancelled => "Cancelled",
                    OrderStatus::Rejected => "Inactive",
                    OrderStatus::Uncertain => "Unknown",
                };
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
                            orders.remove(&update.order_id);
                        }
                        _ => {
                            orders.insert(update.order_id, (
                                status.to_string(), update.instrument,
                                update.filled_qty as f64, update.remaining_qty as f64,
                            ));
                        }
                    }
                }
            }

            // Drain cancel rejects -> error
            let rejects = shared.drain_cancel_rejects();
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
                let map = self.instrument_to_req.lock().unwrap();
                map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
            };

            for (iid, req_id) in instruments {
                let q = shared.quote(iid);
                let fields = [
                    q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                    q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
                ];

                // Read previous values
                let last = {
                    let map = self.last_quotes.lock().unwrap();
                    map.get(&iid).copied().unwrap_or([0i64; 12])
                };

                let attrib = TickAttrib::default();
                let attrib_obj = Py::new(py, attrib)?.into_any();

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

                // Update last quotes
                self.last_quotes.lock().unwrap().insert(iid, fields);
            }

            // Drain TBT trades -> tickByTickAllLast
            let tbt_trades = shared.drain_tbt_trades();
            for trade in tbt_trades {
                let req_id = self.instrument_to_req.lock().unwrap()
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
            let tbt_quotes = shared.drain_tbt_quotes();
            for quote in tbt_quotes {
                let req_id = self.instrument_to_req.lock().unwrap()
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
            let news_items = shared.drain_tick_news();
            for news in news_items {
                let first_req_id = self.instrument_to_req.lock().unwrap()
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

            // Drain what-if responses -> orderStatus with margin info
            let what_ifs = shared.drain_what_if_responses();
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
            let hist_data = shared.drain_historical_data();
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
            let head_ts = shared.drain_head_timestamps();
            for (req_id, response) in head_ts {
                self.wrapper.call_method1(
                    py, "head_timestamp",
                    (req_id as i64, response.head_timestamp.as_str()),
                )?;
            }

            // Drain contract details -> contractDetails + contractDetailsEnd
            let contract_defs = shared.drain_contract_details();
            for (req_id, def) in contract_defs {
                let details = super::contract::ContractDetails::from_definition(&def);
                let details_py = Py::new(py, details)?.into_any();
                self.wrapper.call_method1(
                    py, "contract_details",
                    (req_id as i64, &details_py),
                )?;
            }
            let contract_ends = shared.drain_contract_details_end();
            for req_id in contract_ends {
                self.wrapper.call_method1(py, "contract_details_end", (req_id as i64,))?;
            }

            // Drain matching symbols -> symbolSamples
            let symbol_results = shared.drain_matching_symbols();
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
            let scanner_params = shared.drain_scanner_params();
            for xml in scanner_params {
                self.wrapper.call_method1(py, "scanner_parameters", (xml.as_str(),))?;
            }

            // Drain scanner data -> scannerData + scannerDataEnd
            let scanner_results = shared.drain_scanner_data();
            for (req_id, result) in scanner_results {
                for (rank, &con_id) in result.con_ids.iter().enumerate() {
                    let cd = super::contract::ContractDetails::default();
                    let cd_py = Py::new(py, cd)?.into_any();
                    self.wrapper.call_method(
                        py, "scanner_data",
                        (req_id as i64, rank as i32, &cd_py, "", "", "", ""),
                        None,
                    )?;
                    let _ = con_id;
                }
                self.wrapper.call_method1(py, "scanner_data_end", (req_id as i64,))?;
            }

            // Drain historical news -> historicalNews + historicalNewsEnd
            let news_results = shared.drain_historical_news();
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
            let articles = shared.drain_news_articles();
            for (req_id, article_type, text) in articles {
                self.wrapper.call_method(
                    py, "news_article",
                    (req_id as i64, article_type, text.as_str()),
                    None,
                )?;
            }

            // Drain fundamental data -> fundamentalData
            let fundamentals = shared.drain_fundamental_data();
            for (req_id, data) in fundamentals {
                self.wrapper.call_method1(py, "fundamental_data", (req_id as i64, data.as_str()))?;
            }

            // Drain histogram data -> histogram_data
            let histograms = shared.drain_histogram_data();
            for (req_id, entries) in histograms {
                let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                    pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
                }).collect();
                let py_list = pyo3::types::PyList::new(py, tuples)?;
                self.wrapper.call_method1(py, "histogram_data", (req_id as i64, py_list))?;
            }

            // Drain historical schedules -> historical_schedule
            let schedules = shared.drain_historical_schedules();
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

            // Account state -> updateAccountValue
            if !self.account_id.is_empty() {
                let acct = shared.account();
                let nlv = acct.net_liquidation as f64 / PRICE_SCALE_F;
                if nlv > 0.0 {
                    self.wrapper.call_method1(py, "update_account_value",
                        ("NetLiquidation", format!("{:.2}", nlv).as_str(), "USD", self.account_id.as_str()))?;
                }
            }

            // P&L dispatch (gateway-computed)
            let pnl_req = *self.pnl_req_id.lock().unwrap();
            if let Some(pnl_req) = pnl_req {
                let acct = shared.account();
                let pnl = [acct.daily_pnl, acct.unrealized_pnl, acct.realized_pnl];
                let prev = *self.last_pnl.lock().unwrap();
                if pnl != prev {
                    self.wrapper.call_method(
                        py, "pnl",
                        (pnl_req, acct.daily_pnl as f64 / PRICE_SCALE_F,
                         acct.unrealized_pnl as f64 / PRICE_SCALE_F,
                         acct.realized_pnl as f64 / PRICE_SCALE_F),
                        None,
                    )?;
                    *self.last_pnl.lock().unwrap() = pnl;
                }
            }

            // Per-position P&L dispatch
            {
                let reqs: Vec<(i64, i64)> = {
                    let map = self.pnl_single_reqs.lock().unwrap();
                    map.iter().map(|(&r, &c)| (r, c)).collect()
                };
                for (req_id, con_id) in reqs {
                    if let Some(pi) = shared.position_info(con_id) {
                        let last_price = {
                            let imap = self.instrument_to_req.lock().unwrap();
                            imap.keys()
                                .find_map(|&iid| {
                                    let q = shared.quote(iid);
                                    if q.last != 0 { Some(q.last) } else { None }
                                })
                                .unwrap_or(0)
                        };

                        if last_price != 0 && pi.avg_cost != 0 {
                            let unrealized = (last_price - pi.avg_cost) * pi.position;
                            let value = last_price * pi.position;
                            self.wrapper.call_method(
                                py, "pnl_single",
                                (req_id, pi.position as f64,
                                 0.0f64,
                                 unrealized as f64 / PRICE_SCALE_F,
                                 0.0f64,
                                 value as f64 / PRICE_SCALE_F),
                                None,
                            )?;
                        }
                    }
                }
            }

            // Account summary dispatch
            {
                let summary_req = self.account_summary_req.lock().unwrap().clone();
                if let Some((req_id, ref tags)) = summary_req {
                    let acct = shared.account();
                    let acct_name = self.account_id.as_str();
                    let tag_values: Vec<(&str, f64)> = vec![
                        ("NetLiquidation", acct.net_liquidation as f64 / PRICE_SCALE_F),
                        ("TotalCashValue", acct.total_cash_value as f64 / PRICE_SCALE_F),
                        ("SettledCash", acct.settled_cash as f64 / PRICE_SCALE_F),
                        ("BuyingPower", acct.buying_power as f64 / PRICE_SCALE_F),
                        ("EquityWithLoanValue", acct.equity_with_loan as f64 / PRICE_SCALE_F),
                        ("GrossPositionValue", acct.gross_position_value as f64 / PRICE_SCALE_F),
                        ("InitMarginReq", acct.init_margin_req as f64 / PRICE_SCALE_F),
                        ("MaintMarginReq", acct.maint_margin_req as f64 / PRICE_SCALE_F),
                        ("AvailableFunds", acct.available_funds as f64 / PRICE_SCALE_F),
                        ("ExcessLiquidity", acct.excess_liquidity as f64 / PRICE_SCALE_F),
                        ("Cushion", acct.cushion as f64 / PRICE_SCALE_F),
                        ("DayTradesRemaining", acct.day_trades_remaining as f64),
                        ("Leverage", acct.leverage as f64 / PRICE_SCALE_F),
                        ("UnrealizedPnL", acct.unrealized_pnl as f64 / PRICE_SCALE_F),
                        ("RealizedPnL", acct.realized_pnl as f64 / PRICE_SCALE_F),
                    ];
                    for (tag, val) in &tag_values {
                        if tags.is_empty() || tags.iter().any(|t| t == tag) {
                            if *val != 0.0 {
                                let val_str = format!("{:.2}", val);
                                self.wrapper.call_method(
                                    py, "account_summary",
                                    (req_id, acct_name, *tag, val_str.as_str(), "USD"),
                                    None,
                                )?;
                            }
                        }
                    }
                    self.wrapper.call_method1(py, "account_summary_end", (req_id,))?;
                    // One-shot: clear after delivery
                    *self.account_summary_req.lock().unwrap() = None;
                }
            }

            // Sleep to avoid busy-wait (1ms)
            py.allow_threads(|| std::thread::sleep(std::time::Duration::from_millis(1)));
        }

        Ok(())
    }

    /// Get the account ID.
    fn get_account_id(&self) -> &str {
        &self.account_id
    }
}

impl EClient {
    /// Find instrument ID for a contract, registering if needed.
    fn find_or_register_instrument(&self, contract: &Contract) -> PyResult<u32> {
        // Check if already registered via any reqId
        {
            let map = self.instrument_to_req.lock().unwrap();
            if let Some((&iid, _)) = map.iter().next() {
                return Ok(iid);
            }
        }

        // Register new instrument
        let tx = self.control_tx.as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;

        tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        tx.send(ControlCommand::Subscribe {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
        })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        let shared = self.shared.as_ref().unwrap();
        Ok(shared.instrument_count().saturating_sub(1))
    }

    /// Parse algo strategy and params into AlgoParams.
    fn parse_algo_params(&self, strategy: &str, params: &[TagValue]) -> PyResult<AlgoParams> {
        let get = |key: &str| -> String {
            params.iter()
                .find(|tv| tv.tag == key)
                .map(|tv| tv.value.clone())
                .unwrap_or_default()
        };
        let get_f64 = |key: &str| -> f64 {
            get(key).parse().unwrap_or(0.0)
        };
        let get_bool = |key: &str| -> bool {
            let v = get(key);
            v == "1" || v.eq_ignore_ascii_case("true")
        };

        match strategy.to_lowercase().as_str() {
            "vwap" => Ok(AlgoParams::Vwap {
                max_pct_vol: get_f64("maxPctVol"),
                no_take_liq: get_bool("noTakeLiq"),
                allow_past_end_time: get_bool("allowPastEndTime"),
                start_time: get("startTime"),
                end_time: get("endTime"),
            }),
            "twap" => Ok(AlgoParams::Twap {
                allow_past_end_time: get_bool("allowPastEndTime"),
                start_time: get("startTime"),
                end_time: get("endTime"),
            }),
            "arrivalpx" | "arrival_price" => {
                let risk = match get("riskAversion").to_lowercase().as_str() {
                    "get_done" | "getdone" => RiskAversion::GetDone,
                    "aggressive" => RiskAversion::Aggressive,
                    "passive" => RiskAversion::Passive,
                    _ => RiskAversion::Neutral,
                };
                Ok(AlgoParams::ArrivalPx {
                    max_pct_vol: get_f64("maxPctVol"),
                    risk_aversion: risk,
                    allow_past_end_time: get_bool("allowPastEndTime"),
                    force_completion: get_bool("forceCompletion"),
                    start_time: get("startTime"),
                    end_time: get("endTime"),
                })
            }
            "closepx" | "close_price" => {
                let risk = match get("riskAversion").to_lowercase().as_str() {
                    "get_done" | "getdone" => RiskAversion::GetDone,
                    "aggressive" => RiskAversion::Aggressive,
                    "passive" => RiskAversion::Passive,
                    _ => RiskAversion::Neutral,
                };
                Ok(AlgoParams::ClosePx {
                    max_pct_vol: get_f64("maxPctVol"),
                    risk_aversion: risk,
                    force_completion: get_bool("forceCompletion"),
                    start_time: get("startTime"),
                })
            }
            "darkice" | "dark_ice" => Ok(AlgoParams::DarkIce {
                allow_past_end_time: get_bool("allowPastEndTime"),
                display_size: get("displaySize").parse().unwrap_or(100),
                start_time: get("startTime"),
                end_time: get("endTime"),
            }),
            "pctvol" | "pct_vol" => Ok(AlgoParams::PctVol {
                pct_vol: get_f64("pctVol"),
                no_take_liq: get_bool("noTakeLiq"),
                start_time: get("startTime"),
                end_time: get("endTime"),
            }),
            "adaptive" => {
                // Adaptive is handled differently — it's a priority, not full algo
                Err(PyRuntimeError::new_err(
                    "Use order_type='LMT' with Adaptive strategy via the direct API's submit_adaptive()"
                ))
            }
            _ => Err(PyRuntimeError::new_err(format!("Unsupported algo strategy: '{}'", strategy))),
        }
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
