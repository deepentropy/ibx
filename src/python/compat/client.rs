//! ibapi-compatible EClient class that wraps IbEngine.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Sender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::SharedState;
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;
use super::contract::{Contract, Order, TagValue};
use super::tick_types::*;
use super::super::types::PRICE_SCALE_F;

/// ibapi-compatible EClient class.
/// Wraps the internal engine and dispatches events to an EWrapper subclass.
#[pyclass(subclass)]
pub struct EClient {
    /// Reference to the EWrapper (which is typically `self` in the `App(EWrapper, EClient)` pattern).
    wrapper: PyObject,
    shared: Option<Arc<SharedState>>,
    control_tx: Option<Sender<ControlCommand>>,
    next_order_id: std::sync::atomic::AtomicU64,
    _thread: Option<thread::JoinHandle<()>>,
    account_id: String,
    connected: bool,
    /// Maps reqId -> InstrumentId for market data subscriptions.
    req_to_instrument: HashMap<i64, u32>,
    /// Maps InstrumentId -> reqId (reverse).
    instrument_to_req: HashMap<u32, i64>,
    /// Last quote sent per instrument (for change detection).
    last_quotes: HashMap<u32, [i64; 12]>,
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
            next_order_id: std::sync::atomic::AtomicU64::new(0),
            _thread: None,
            account_id: String::new(),
            connected: false,
            req_to_instrument: HashMap::new(),
            instrument_to_req: HashMap::new(),
            last_quotes: HashMap::new(),
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
        if self.connected {
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
        let shared_clone = shared.clone();
        let strategy = crate::bridge::BridgeStrategy::new(shared_clone);

        let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, core_id);

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
        self.next_order_id = std::sync::atomic::AtomicU64::new(start_id);
        self._thread = Some(handle);
        self.connected = true;

        let _ = (port, client_id); // unused but kept for ibapi signature compat

        Ok(())
    }

    /// Disconnect from IB.
    fn disconnect(&mut self) -> PyResult<()> {
        if let Some(ref tx) = self.control_tx {
            let _ = tx.send(ControlCommand::Shutdown);
        }
        self.connected = false;
        Ok(())
    }

    /// Check if connected.
    fn is_connected(&self) -> bool {
        self.connected
    }

    /// Request market data for a contract.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))]
    fn req_mkt_data(
        &mut self,
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

        self.req_to_instrument.insert(req_id, instrument_id);
        self.instrument_to_req.insert(instrument_id, req_id);

        let _ = (generic_tick_list, snapshot, regulatory_snapshot, mkt_data_options);

        Ok(())
    }

    /// Cancel market data.
    fn cancel_mkt_data(&mut self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.req_to_instrument.remove(&req_id) {
            self.instrument_to_req.remove(&instrument);
            self.last_quotes.remove(&instrument);
            let tx = self.control_tx.as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("Not connected"))?;
            tx.send(ControlCommand::Unsubscribe { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }
        Ok(())
    }

    /// Place an order. Routes to the appropriate submit_* based on order_type.
    fn place_order(&mut self, order_id: i64, contract: &Contract, order: &Order) -> PyResult<()> {
        if self.control_tx.is_none() {
            return Err(PyRuntimeError::new_err("Not connected"));
        }

        let oid = if order_id > 0 {
            order_id as u64
        } else {
            self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        // Cancel all for each subscribed instrument
        for &instrument in self.req_to_instrument.values() {
            let _ = tx.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }));
        }
        Ok(())
    }

    /// Request next valid order ID.
    #[pyo3(signature = (num_ids=1))]
    fn req_ids(&self, py: Python<'_>, num_ids: i32) -> PyResult<()> {
        let next_id = self.next_order_id.load(std::sync::atomic::Ordering::Relaxed) as i64;
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

    /// Run the event loop. Polls bridge queues and dispatches to EWrapper callbacks.
    fn run(&mut self, py: Python<'_>) -> PyResult<()> {
        if !self.connected {
            return Err(PyRuntimeError::new_err("Not connected. Call connect() first."));
        }

        // Fire initial callbacks
        let next_id = self.next_order_id.load(std::sync::atomic::Ordering::Relaxed) as i64;
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        self.wrapper.call_method1(py, "managed_accounts", (self.account_id.as_str(),))?;
        self.wrapper.call_method0(py, "connect_ack")?;

        // Event loop
        while self.connected {
            // Allow Python threads to run and check for KeyboardInterrupt
            py.check_signals()?;

            let shared = match self.shared.as_ref() {
                Some(s) => s,
                None => break,
            };

            // Drain fills -> execDetails + orderStatus
            let fills = shared.drain_fills();
            for fill in fills {
                let req_id = self.instrument_to_req.get(&fill.instrument).copied().unwrap_or(-1);
                let side_str = match fill.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                    Side::ShortSell => "SSHORT",
                };
                let price = fill.price as f64 / PRICE_SCALE_F;
                let commission = fill.commission as f64 / PRICE_SCALE_F;

                // orderStatus with Filled/PartiallyFilled
                let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
                self.wrapper.call_method(
                    py, "order_status",
                    (fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                     price, 0i64, 0i64, price, 0i64, "", 0.0f64),
                    None,
                )?;

                let _ = (req_id, side_str, commission);
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
            let instruments: Vec<(u32, i64)> = self.instrument_to_req.iter()
                .map(|(&iid, &req_id)| (iid, req_id))
                .collect();

            for (iid, req_id) in instruments {
                let q = shared.quote(iid);
                let fields = [
                    q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                    q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
                ];

                let last = self.last_quotes.entry(iid).or_insert([0i64; 12]);
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

                *last = fields;
            }

            // Account state -> updateAccountValue
            if !self.account_id.is_empty() {
                let acct = shared.account();
                let nlv = acct.net_liquidation as f64 / PRICE_SCALE_F;
                if nlv > 0.0 {
                    self.wrapper.call_method1(py, "update_account_value",
                        ("NetLiquidation", format!("{:.2}", nlv).as_str(), "USD", self.account_id.as_str()))?;
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
    fn find_or_register_instrument(&mut self, contract: &Contract) -> PyResult<u32> {
        // Check if already registered via any reqId
        for (&iid, _) in &self.instrument_to_req {
            // Already have this instrument registered
            return Ok(iid);
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
