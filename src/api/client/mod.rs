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
//! client.req_mkt_data(1, &Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() },
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
mod stubs;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};

use crate::api::types::{
    Contract as ApiContract, Order as ApiOrder, TagValue as ApiTagValue,
};
use crate::bridge::{Event, SharedState};
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
///
/// # Live logins block on second-factor approval
///
/// With `paper: false`, [`connect()`](EClient::connect) enters a second-factor
/// approval window and **blocks** until the factor is approved (mobile push) or
/// the server ends the wait (~18 min); as in the reference there is no client
/// timeout by default. This is expected — it is a human
/// approval gate, not a hang. Bound or avoid it by using `paper: true`, setting
/// a timeout (via [`GatewayConfig::ib_key_timeout_secs`] when building through
/// the lower-level API), or supplying a `code_provider`. Paper logins skip the
/// gate entirely. An `info`-level log line is emitted when the wait begins
/// (`RUST_LOG=info`). See ibx#203 / ibx#207.
///
/// # Multiple engines per process
///
/// Multiple `EClient` instances can run concurrently in one process. Each owns
/// its own state, sockets, and `ib-engine-hotloop` thread; nothing is shared
/// between them, and `connect()` does not serialize across instances. If you
/// pin engines with `core_id`, give each a **distinct** value — pinning two hot
/// loops to the same core makes them busy-poll the same CPU and starve each
/// other (degraded throughput, not a hang). With `core_id: None` (the default)
/// no pinning happens and there is no conflict.
pub struct EClientConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    /// `false` enters the live second-factor approval gate on connect (blocking).
    /// `true` skips it. See the type-level docs.
    pub paper: bool,
    /// CPU core to pin this engine's hot loop to. `None` = no pinning. When
    /// running multiple engines, use a **distinct** core per engine.
    pub core_id: Option<usize>,
}

/// Give the engine the caller's host and credentials for auto-reconnect. The
/// gateway leaves them empty for the caller to fill; without this every
/// reconnect was skipped for the Rust client (ibx#399).
fn cache_reconnect_credentials(hot_loop: &mut crate::engine::hot_loop::HotLoop, config: &EClientConfig) {
    hot_loop.update_reconnect_auth(
        config.host.clone(),
        config.username.clone(),
        zeroize::Zeroizing::new(config.password.clone()),
        config.paper,
    );
}

/// ibapi-compatible EClient. Matches C++ `EClientSocket` method signatures.
///
/// # Thread lifecycle
///
/// `connect()` spawns a single `ib-engine-hotloop` background thread.
/// The thread is **joined** on [`disconnect()`] and on [`Drop`].
/// Dropping an `EClient` without calling `disconnect()` first is safe:
/// the `Drop` impl sends `Shutdown` and joins the thread.
///
/// # Losing the connection
///
/// When the engine stops — connection lost, reconnect exhausted, or the hot
/// loop panicked — the next [`process_msgs()`](EClient::process_msgs) call
/// fires [`connection_closed`](crate::api::wrapper::Wrapper::connection_closed) once and
/// [`is_connected()`](EClient::is_connected) turns false. No error callback is
/// raised for this: the connectivity error codes are pushed by the server, not
/// synthesized locally (ibx#242).
///
/// # Ids
///
/// Request, ticker and order ids and conIds are `i64` everywhere: in the
/// methods, the [`Wrapper`](crate::api::wrapper::Wrapper) callbacks and the
/// [`Event`]s. Negative ids are kept as given; `-1` is the id of an error that
/// belongs to no request. The reference reads request ids, ticker ids and
/// conIds as 32-bit ints, so a request with one outside that range is dropped
/// with a log line and no error, as the reference drops it (ibx#285).
pub struct EClient {
    pub(crate) shared: Arc<SharedState>,
    pub(crate) control_tx: Sender<ControlCommand>,
    pub(crate) thread: Mutex<Option<thread::JoinHandle<()>>>,
    pub account_id: String,
    pub(crate) connected: AtomicBool,
    /// True once `connection_closed` has been delivered, so it fires at most
    /// once per session.
    pub(crate) close_notified: AtomicBool,
    pub(crate) core: ClientCore,
    pub(crate) session_token_bytes: Vec<u8>,
    pub(crate) token_type: String,
    /// Connection time, fixed when the session started (ibx#426).
    pub(crate) connection_time: String,
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
        Self::connect_inner(config, None)
    }

    /// Connect to IB and start the engine with an [`Event`] channel attached.
    ///
    /// Returns the client plus a receiver carrying every [`Event`] the engine
    /// produces. This is a second, optional delivery path that runs alongside
    /// [`process_msgs()`](EClient::process_msgs) — it does not replace it, and
    /// nothing is removed from the wrapper callbacks when it is in use.
    ///
    /// The channel is bounded by `capacity`; the engine never blocks on it, so
    /// a consumer that falls behind loses events rather than slowing the hot
    /// loop. Drain it from a thread that is not the one calling
    /// `process_msgs()`, or keep `capacity` generous.
    ///
    /// Attaching a channel makes the engine build events it would otherwise
    /// skip, which for bar batches and contract definitions means one deep copy
    /// each. Use [`connect()`](EClient::connect) when you only need the wrapper
    /// callbacks (ibx#242).
    pub fn connect_with_events(
        config: &EClientConfig,
        capacity: usize,
    ) -> Result<(Self, Receiver<Event>), Box<dyn std::error::Error>> {
        let (event_tx, event_rx) = crossbeam_channel::bounded(capacity.max(1));
        let client = Self::connect_inner(config, Some(event_tx))?;
        Ok((client, event_rx))
    }

    fn connect_inner(
        config: &EClientConfig,
        event_tx: Option<Sender<Event>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let gw_config = GatewayConfig {
            username: config.username.clone(),
            password: zeroize::Zeroizing::new(config.password.clone()),
            host: config.host.clone(),
            paper: config.paper,
            accept_invalid_certs: false,
            ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            code_provider: None,
        };

        let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&gw_config)?;
        let account_id = gw.account_id.clone();
        let session_token_bytes = crate::auth::crypto::strip_leading_zeros(
            &gw.session_token.to_bytes_be(),
        ).to_vec();
        let token_type = String::new();
        let shared = Arc::new(SharedState::new());
        gw.populate_init_data(&shared);

        let (mut hot_loop, control_tx) = gw.into_hot_loop_with_farms(
            shared.clone(), event_tx, farm_conn, ccp_conn, hmds_conn, config.core_id,
        );
        cache_reconnect_credentials(&mut hot_loop, config);

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || { hot_loop.run_with_panic_recovery(); })?;

        Ok(Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            account_id,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            core: ClientCore::new(),
            session_token_bytes,
            token_type,
            connection_time: crate::client_core::connection_time_now(),
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
        Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            account_id,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            core: ClientCore::new(),
            session_token_bytes: Vec::new(),
            token_type: String::new(),
            connection_time: crate::client_core::connection_time_now(),
        }
    }

    /// Map a reqId to an InstrumentId (for testing without a live engine).
    /// The request takes every tick of the instrument, its headlines too.
    #[doc(hidden)]
    pub fn map_req_instrument(&self, req_id: i64, instrument: InstrumentId) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().entry(instrument).or_default().push(req_id);
        self.core.md_news.lock().unwrap().insert(req_id, String::new());
    }

    /// Pre-populate the order tracker (for testing the dispatcher path
    /// without going through the engine's place-order flow).
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn track_order_for_test(
        &self,
        order_id: OrderId,
        contract: ApiContract,
        order: ApiOrder,
        instrument: InstrumentId,
    ) {
        self.core.track_order(order_id, contract, order, instrument);
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

    /// API level the session follows: 214, the level the reference gives a
    /// current client (ibx#426). Matches `serverVersion()` in C++, for code
    /// that tests it before using a feature.
    pub fn server_version(&self) -> i32 {
        crate::client_core::SERVER_VERSION
    }

    /// Time the session started, as `yyyyMMdd HH:mm:ss {zone}` in the
    /// machine's local time (ibx#426). The connection time of the C++
    /// client.
    pub fn tws_connection_time(&self) -> String {
        self.connection_time.clone()
    }

    /// False after [`disconnect()`](EClient::disconnect), and after a
    /// `process_msgs()` call that observed the engine stopping (ibx#242).
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

impl EClient {
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    pub fn ccp_session_id(&self) -> String {
        self.shared.reference.ccp_session_id()
    }

    /// Logical-name → host URL lookup from the gateway logon MiscUrls push
    /// (e.g. `region_dam`). Returns `None` when the gateway did not push this key.
    pub fn misc_url(&self, key: &str) -> Option<String> {
        self.shared.reference.misc_url(key)
    }

    /// Canonical big-endian session-token bytes (leading zeros stripped) captured
    /// at connect. Round-trips through `BigUint::from_bytes_be` to the SRP shared
    /// secret K and is the second SHA-1 input for SSO `Authenticate-TWS` bodies.
    pub fn session_token_bytes(&self) -> &[u8] {
        &self.session_token_bytes
    }

    /// `stoken_type` discriminator captured at connect (`"st"`, `"tst"`, `"zenith"`,
    /// or empty for the SRP-only path). Sent verbatim in SSO authenticator bodies.
    pub fn token_type(&self) -> &str {
        &self.token_type
    }
}
