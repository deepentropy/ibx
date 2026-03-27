//! ibapi-compatible EClient class that wraps IbEngine.

mod market_data;
mod orders;
mod account;
mod reference;
mod dispatch;
mod stubs;
mod test_helpers;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::{Event, SharedState};
use crate::client_core::ClientCore;
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;
use super::contract::{Contract, Order};

/// ibapi-compatible EClient class.
/// Wraps the internal engine and dispatches events to an EWrapper subclass.
///
/// All methods take `&self` (shared borrow) so that `run()` can execute in a
/// daemon thread while the main thread calls req/cancel methods concurrently.
/// `frozen` tells PyO3 to skip RefCell borrow-checking, which is required
/// because `run()` holds a `&self` borrow for the lifetime of the event loop.
/// Interior mutability is provided by `Mutex`, `AtomicBool`, and atomics.
///
/// # Thread lifecycle
///
/// `connect()` spawns a single `ib-engine-hotloop` background thread.
/// The thread is **joined** on [`disconnect()`] and on [`Drop`].
/// Dropping an `EClient` without calling `disconnect()` first is safe:
/// the `Drop` impl sends `Shutdown` and joins the thread.
///
/// The client is **reconnectable**: calling `disconnect()` resets all session
/// state so that a subsequent `connect()` on the same instance works correctly.
#[pyclass(frozen, subclass)]
pub struct EClient {
    /// Reference to the EWrapper (which is typically `self` in the `App(EWrapper, EClient)` pattern).
    pub(crate) wrapper: PyObject,
    /// Set by connect(), cleared by disconnect().
    pub(crate) shared: Mutex<Option<Arc<SharedState>>>,
    /// Set by connect(), cleared by disconnect().
    pub(crate) control_tx: Mutex<Option<Sender<ControlCommand>>>,
    pub(crate) next_order_id: AtomicU64,
    pub(crate) _thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Set by connect(), cleared by disconnect().
    pub(crate) account_id: Mutex<Option<String>>,
    pub(crate) connected: AtomicBool,
    /// Receiver for engine events (disconnects, etc.).
    pub(crate) event_rx: Mutex<Option<crossbeam_channel::Receiver<Event>>>,
    /// Sender for test-injected events (test-only).
    #[doc(hidden)]
    pub(crate) _test_event_tx: Mutex<Option<crossbeam_channel::Sender<Event>>>,
    /// Shared subscription tracking and dispatch preparation.
    pub(crate) core: ClientCore,
}

impl Drop for EClient {
    fn drop(&mut self) {
        if let Some(tx) = self.control_tx.lock().unwrap().as_ref() {
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
            shared: Mutex::new(None),
            control_tx: Mutex::new(None),
            next_order_id: AtomicU64::new(0),
            _thread: Mutex::new(None),
            account_id: Mutex::new(None),
            connected: AtomicBool::new(false),
            event_rx: Mutex::new(None),
            _test_event_tx: Mutex::new(None),
            core: ClientCore::new(),
        }
    }

    /// Connect to IB and start the engine.
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

        *self.account_id.lock().unwrap() = Some(gw.account_id.clone());
        let shared = Arc::new(SharedState::new());
        gw.populate_init_data(&shared);

        let connect_host = config.host.clone();
        let connect_username = config.username.clone();
        let connect_paper = config.paper;
        let (event_tx, event_rx) = crossbeam_channel::bounded(256);
        let (mut hot_loop, control_tx) = gw.into_hot_loop_with_farms(shared.clone(), Some(event_tx), farm_conn, ccp_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, core_id);
        hot_loop.update_reconnect_auth(connect_host, connect_username, connect_paper);

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

        *self.shared.lock().unwrap() = Some(shared);
        *self.control_tx.lock().unwrap() = Some(control_tx);
        *self.event_rx.lock().unwrap() = Some(event_rx);
        self.next_order_id.store(start_id, Ordering::Relaxed);
        *self._thread.lock().unwrap() = Some(handle);
        self.connected.store(true, Ordering::Release);

        let _ = (port, client_id); // unused but kept for ibapi signature compat

        Ok(())
    }

    /// Disconnect from IB.
    fn disconnect(&self) -> PyResult<()> {
        if let Some(tx) = self.control_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ControlCommand::Shutdown);
        }
        if let Some(h) = self._thread.lock().unwrap().take() {
            let _ = h.join();
        }
        self.connected.store(false, Ordering::Release);
        // Reset per-session state so connect() can be called again.
        *self.shared.lock().unwrap() = None;
        *self.control_tx.lock().unwrap() = None;
        *self.event_rx.lock().unwrap() = None;
        *self.account_id.lock().unwrap() = None;
        self.core.reset();
        Ok(())
    }

    /// Check if connected.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Run the event loop.
    fn run(&self, py: Python<'_>) -> PyResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Not connected. Call connect() first."));
        }

        // Fire initial callbacks
        let next_id = self.next_order_id.load(Ordering::Relaxed) as i64;
        self.wrapper.call_method1(py, "next_valid_id", (next_id,))?;
        self.wrapper.call_method1(py, "managed_accounts", (self.account().as_str(),))?;
        self.wrapper.call_method0(py, "connect_ack")?;

        // Event loop — wake immediately on data, or check signals every 1ms.
        while self.connected.load(Ordering::Relaxed) {
            py.check_signals()?;

            let shared = match self.shared.lock().unwrap().clone() {
                Some(s) => s,
                None => break,
            };

            self.dispatch_once(py, &shared)?;

            // Wait for hot loop notification instead of fixed sleep.
            // Releases GIL while waiting; wakes immediately when data arrives.
            let shared_ref = shared.clone();
            py.allow_threads(move || {
                shared_ref.wait_for_data(std::time::Duration::from_millis(1));
            });
        }

        // Signal disconnection to wrapper
        self.wrapper.call_method0(py, "connection_closed")?;

        Ok(())
    }

    /// Get the account ID.
    fn get_account_id(&self) -> String {
        self.account()
    }
}

impl EClient {
    /// Clone the control channel sender, or return "Not connected".
    pub(crate) fn tx(&self) -> PyResult<Sender<ControlCommand>> {
        self.control_tx.lock().unwrap().clone()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))
    }

    /// Clone the shared state Arc, or return "Not connected".
    pub(crate) fn shared_state(&self) -> PyResult<Arc<SharedState>> {
        self.shared.lock().unwrap().clone()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))
    }

    /// Return the account id (empty string if not connected).
    pub(crate) fn account(&self) -> String {
        self.account_id.lock().unwrap().clone().unwrap_or_default()
    }

    /// Find instrument ID for a contract, registering if needed.
    pub(crate) fn find_or_register_instrument(&self, contract: &Contract) -> PyResult<u32> {
        let tx = self.tx()?;
        self.core.find_or_register_instrument(
            &tx,
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
    use super::super::contract::TagValue;

    #[test]
    fn eclient_default_state() {
        // Can't construct without Python, but we can test the parsing helpers
        let tv = vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "startTime".into(), value: "09:30:00".into() },
            TagValue { tag: "endTime".into(), value: "16:00:00".into() },
        ];

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
