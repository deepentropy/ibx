//! ibapi-compatible EClient — Rust equivalent of C++ `EClientSocket`.
//!
//! Connects to IB, provides ibapi-matching method signatures, and dispatches
//! events to a [`Wrapper`] via `process_msgs()`.
//!
//! ```no_run
//! use ibx::api::{EClient, EClientConfig, Wrapper, Contract, Order};
//! use ibx::api::types::TickAttrib;
//!
//! struct MyWrapper;
//! impl Wrapper for MyWrapper {
//!     fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {
//!         println!("tick_price: req_id={req_id} type={tick_type} price={price}");
//!     }
//! }
//!
//! let mut client = EClient::connect(&EClientConfig {
//!     username: "user".into(),
//!     password: "pass".into(),
//!     host: "your_ib_host".into(),
//!     paper: true,
//!     core_id: None,
//! }).unwrap();
//!
//! client.req_mkt_data(1, &Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() },
//!     "", false, false).unwrap();
//!
//! let mut wrapper = MyWrapper;
//! loop {
//!     client.process_msgs(&mut wrapper);
//! }
//! ```

mod market_data;
mod orders;
mod account;
mod reference;
mod dispatch;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::Sender;

use crate::api::types::{
    Contract as ApiContract, Order as ApiOrder, TagValue as ApiTagValue,
};
use crate::bridge::SharedState;
use crate::client_core::ClientCore;
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;

// Re-export as public type names for the API surface
pub type Contract = ApiContract;
pub type Order = ApiOrder;
pub type TagValue = ApiTagValue;

// Re-export public items from submodules
pub use orders::parse_algo_params;

/// Configuration for connecting to IB via EClient.
pub struct EClientConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
    pub core_id: Option<usize>,
}

/// ibapi-compatible EClient. Matches C++ `EClientSocket` method signatures.
///
/// # Thread lifecycle
///
/// `connect()` spawns a single `ib-engine-hotloop` background thread.
/// The thread is **joined** on [`disconnect()`] and on [`Drop`].
/// Dropping an `EClient` without calling `disconnect()` first is safe:
/// the `Drop` impl sends `Shutdown` and joins the thread.
pub struct EClient {
    pub(crate) shared: Arc<SharedState>,
    pub(crate) control_tx: Sender<ControlCommand>,
    pub(crate) thread: Mutex<Option<thread::JoinHandle<()>>>,
    pub account_id: String,
    pub(crate) connected: AtomicBool,
    pub(crate) next_order_id: AtomicU64,
    pub(crate) core: ClientCore,
}

impl Drop for EClient {
    fn drop(&mut self) {
        // Ensure the hot-loop thread is stopped and joined.
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl EClient {
    /// Connect to IB and start the engine.
    pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let gw_config = GatewayConfig {
            username: config.username.clone(),
            password: config.password.clone(),
            host: config.host.clone(),
            paper: config.paper,
            accept_invalid_certs: false,
        };

        let (gw, farm_conn, ccp_conn, hmds_conn, cashfarm, usfuture, eufarm, jfarm) = Gateway::connect(&gw_config)?;
        let account_id = gw.account_id.clone();
        let shared = Arc::new(SharedState::new());

        let (mut hot_loop, control_tx) = gw.into_hot_loop_with_farms(
            shared.clone(), None, farm_conn, ccp_conn, hmds_conn,
            cashfarm, usfuture, eufarm, jfarm, config.core_id,
        );

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || { hot_loop.run(); })?;

        let start_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() * 1000;

        Ok(Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            account_id,
            connected: AtomicBool::new(true),
            next_order_id: AtomicU64::new(start_id),
            core: ClientCore::new(),
        })
    }

    /// Construct from pre-built components (for testing or custom setups).
    #[doc(hidden)]
    pub fn from_parts(
        shared: Arc<SharedState>,
        control_tx: Sender<ControlCommand>,
        handle: thread::JoinHandle<()>,
        account_id: String,
    ) -> Self {
        let start_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() * 1000;
        Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            account_id,
            connected: AtomicBool::new(true),
            next_order_id: AtomicU64::new(start_id),
            core: ClientCore::new(),
        }
    }

    /// Map a reqId to an InstrumentId (for testing without a live engine).
    #[doc(hidden)]
    pub fn map_req_instrument(&self, req_id: i64, instrument: InstrumentId) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().insert(instrument, req_id);
    }

    /// Pre-seed a con_id → InstrumentId mapping (for testing without a live engine).
    #[doc(hidden)]
    pub fn seed_instrument(&self, con_id: i64, instrument: InstrumentId) {
        self.core.con_id_to_instrument.lock().unwrap().insert(con_id, instrument);
    }

    /// Send a control command to the engine. Returns `Err` if the engine has shut down.
    pub(crate) fn send(&self, cmd: ControlCommand) -> Result<(), String> {
        self.control_tx.send(cmd).map_err(|e| format!("Engine stopped: {e}"))
    }

    // ── Connection ──

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Disconnect from IB.  Sends `Shutdown` to the hot loop, waits for the
    /// background thread to exit, and marks the client as disconnected.
    pub fn disconnect(&self) {
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
        self.connected.store(false, Ordering::Release);
        self.core.reset();
    }
}
