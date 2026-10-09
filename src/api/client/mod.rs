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
use std::io;
use std::time::{Duration, Instant};
use crate::lifecycle::ConnectionControl;

use crate::engine::park::ControlSender;
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
    ///
    /// A pinned engine polls its connections and its commands without
    /// pause and keeps that core busy. Without pinning the engine thread
    /// rests while there is nothing to do and is woken by the first byte
    /// received or the first command sent (ibx#530).
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
/// The thread is **joined** on [`disconnect()`](EClient::disconnect) and on [`Drop`].
/// The additive `connect_once()` path instead uses caller-owned out-of-band stop.
/// Call `disconnect_checked()` on an owned worker for confirmation. Its Drop signals stop and
/// transfers any unfinished engine handle to the supplied ConnectionControl;
/// it never blocks on command admission or join and is not cleanup confirmation.
///
/// Dropping a legacy `EClient` without calling `disconnect()` first is safe:
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
    pub(crate) control_tx: ControlSender,
    pub(crate) thread: Mutex<Option<thread::JoinHandle<()>>>,
    connection_control: Option<ConnectionControl>,
    shutdown_failure: Mutex<Option<String>>,
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
        if let Some(control) = &self.connection_control {
            control.cancel();
            if let Some(handle) = self.thread.get_mut().unwrap().take() {
                // Ownership moves to the cancellation scope, never a detached task.
                // Drop is best effort; explicit checked shutdown is the proof.
                control.retain_worker(handle);
            }
            return;
        }
        // Ensure the legacy hot-loop thread is stopped and joined.
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl EClient {
    /// Connect to IB and start the engine.
    pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_inner(config, None, None)
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
        let client = Self::connect_inner(config, Some(event_tx), None)?;
        Ok((client, event_rx))
    }

    /// Perform one caller-controlled login attempt without autonomous recovery.
    /// Live accounts use broker-selected mobile push approval; no code callback worker is spawned.
    /// `control` must include all allowed pre-resolved login/farm hosts. This
    /// blocking method belongs on an owned worker, not a Tokio executor thread.
    /// On error, cancel and join the scope before starting any replacement.
    pub fn connect_once(
        config: &EClientConfig,
        control: &ConnectionControl,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_inner(config, None, Some(control))
    }

    fn connect_inner(
        config: &EClientConfig,
        event_tx: Option<Sender<Event>>,
        connection_control: Option<&ConnectionControl>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Validate before duplicating controlled plaintext/configuration storage.
        if let Some(control) = connection_control {
            control.check()?;
            if config.username.trim().is_empty() || config.username.len() > 256 || config.password.is_empty() || config.password.len() > 4096 || config.host.trim().is_empty() || config.host.len() > 253 {
                control.cancel();
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "controlled login configuration exceeds supported bounds").into());
            }
        }
        // Covers post-Gateway initialization, control installation and spawning,
        // including unwinding before a client can own the engine handle.
        struct Setup<'a> { control: Option<&'a ConnectionControl>, succeeded: bool }
        impl Drop for Setup<'_> {
            fn drop(&mut self) { if !self.succeeded { if let Some(control) = self.control { control.cancel(); } } }
        }
        let mut setup = Setup { control: connection_control, succeeded: false };
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

        let (gw, farm_conn, ccp_conn, hmds_conn) = if let Some(control) = connection_control {
            Gateway::connect_once(&gw_config, control)?
        } else {
            Gateway::connect(&gw_config)?
        };
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
        if let Some(control) = connection_control {
            // Install before run: no native reconnect worker can start.
            hot_loop.set_connection_control(control.clone())?;
        } else {
            cache_reconnect_credentials(&mut hot_loop, config);
        }

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || { hot_loop.run_with_panic_recovery(); })?;

        // The highest order id of this client's earlier sessions (ibx#518).
        let core = ClientCore::new();
        core.keep_order_ids(&account_id);

        let client = Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            connection_control: connection_control.cloned(),
            shutdown_failure: Mutex::new(None),
            account_id,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            core,
            session_token_bytes,
            token_type,
            connection_time: crate::client_core::connection_time_now(),
        };
        if let Some(control) = connection_control {
            control.finish_login()?;
        }
        setup.succeeded = true;
        Ok(client)
    }

    /// Construct from pre-built components (for testing or custom setups).
    #[doc(hidden)]
    pub fn from_parts(
        shared: Arc<SharedState>,
        control_tx: impl Into<ControlSender>,
        handle: thread::JoinHandle<()>,
        account_id: String,
    ) -> Self {
        Self {
            shared,
            control_tx: control_tx.into(),
            thread: Mutex::new(Some(handle)),
            connection_control: None,
            shutdown_failure: Mutex::new(None),
            account_id,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            core: ClientCore::new(),
            session_token_bytes: Vec::new(),
            token_type: String::new(),
            connection_time: crate::client_core::connection_time_now(),
        }
    }

    /// Construct an externally controlled engine for offline fixtures/custom setups.
    /// The supplied worker must already use this same control; this constructor
    /// cannot make an unrelated blocking worker interruptible.
    #[doc(hidden)]
    pub fn from_parts_controlled(
        shared: Arc<SharedState>,
        control_tx: Sender<ControlCommand>,
        handle: thread::JoinHandle<()>,
        account_id: String,
        control: ConnectionControl,
    ) -> Self {
        let mut client = Self::from_parts(shared, control_tx, handle, account_id);
        client.connection_control = Some(control);
        client
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
        if self.connection_control.is_some() {
            self.try_send_control(cmd).map_err(|error| format!("Engine admission: {error}"))
        } else {
            self.control_tx.send(cmd).map_err(|e| format!("Engine stopped: {e}"))
        }
    }

    /// Nonblocking control admission. Queue acceptance is not wire transmission.
    /// Controlled cancellation rejects new commands; the engine also checks the
    /// out-of-band stop before dispatching queued work.
    pub fn try_send_control(
        &self,
        cmd: ControlCommand,
    ) -> Result<(), crossbeam_channel::TrySendError<ControlCommand>> {
        if self.connection_control.as_ref().is_some_and(ConnectionControl::is_cancelled) {
            return Err(crossbeam_channel::TrySendError::Disconnected(cmd));
        }
        self.control_tx.try_send(cmd)
    }

    /// Cancel a controlled session and confirm its engine and retained workers
    /// finished within `timeout`. Timeout retains the unfinished engine handle
    /// for a later join; panic remains a reported cleanup failure on later calls.
    /// Legacy sessions are rejected because they have no out-of-band stop.
    pub fn disconnect_checked(&self, timeout: Duration) -> io::Result<()> {
        let control = self.connection_control.as_ref().ok_or_else(||
            io::Error::new(io::ErrorKind::Unsupported, "legacy session has no checked stop"))?;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(||
            io::Error::new(io::ErrorKind::InvalidInput, "shutdown deadline overflow"))?;
        control.cancel();
        self.connected.store(false, Ordering::Release);
        let mut handle = self.thread.try_lock().map_err(|_|
            io::Error::new(io::ErrorKind::WouldBlock, "another shutdown owns the engine handle"))?;
        while handle.as_ref().is_some_and(|worker| !worker.is_finished()) {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "engine join unconfirmed"));
            }
            thread::sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())));
        }
        if let Some(worker) = handle.take() {
            if worker.join().is_err() {
                control.record_cleanup_failure(io::ErrorKind::Other);
                *self.shutdown_failure.lock().unwrap() = Some("controlled engine panicked".into());
            }
        }
        drop(handle);
        control.join_workers(deadline.saturating_duration_since(Instant::now()))?;
        if let Some(failure) = self.shutdown_failure.lock().unwrap().as_ref() {
            return Err(io::Error::other(failure.clone()));
        }
        self.core.reset();
        Ok(())
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

    /// Disconnect a legacy session by sending `Shutdown` and joining its engine.
    /// Controlled sessions signal stop promptly and retain unfinished handles;
    /// use `disconnect_checked` to confirm their actual engine/worker disposal.
    pub fn disconnect(&self) {
        if self.connection_control.is_some() {
            // Signal promptly, retaining unfinished handles. Controlled callers
            // must use disconnect_checked to establish physical-close proof.
            let _ = self.disconnect_checked(Duration::ZERO);
            return;
        }
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
        self.connected.store(false, Ordering::Release);
        self.core.stop_keeping_order_ids();
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

#[cfg(test)]
mod controlled_lifecycle_tests {
    use super::*;
    use crate::engine::hot_loop::HotLoop;
    use std::collections::BTreeMap;

    fn control() -> ConnectionControl {
        ConnectionControl::new(Duration::from_secs(5), BTreeMap::from([("localhost".into(), vec![std::net::IpAddr::from([127, 0, 0, 1])])]), "offline-hardware".into()).unwrap()
    }

    #[test]
    fn controlled_checked_close_bypasses_full_queue_and_joins_actual_hotloop() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(64);
        for _ in 0..64 { tx.try_send(ControlCommand::Ping).unwrap(); }
        let control = control();
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.set_connection_control(control.clone()).unwrap();
        let (start_tx, start_rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(move || {
            start_rx.recv().unwrap();
            engine.run_with_panic_recovery();
        });
        let client = EClient::from_parts_controlled(shared.clone(), tx, handle, "DU-fixture".into(), control.clone());
        assert!(matches!(client.try_send_control(ControlCommand::Ping), Err(crossbeam_channel::TrySendError::Full(_))));
        control.cancel();
        assert!(matches!(client.try_send_control(ControlCommand::Ping), Err(crossbeam_channel::TrySendError::Disconnected(_))));
        start_tx.send(()).unwrap();
        client.disconnect_checked(Duration::from_secs(1)).unwrap();
        assert!(client.thread.lock().unwrap().is_none());
        assert!(!client.is_connected());
        assert!(shared.take_connection_lost());
        client.disconnect_checked(Duration::ZERO).unwrap();
    }

    #[test]
    fn controlled_timeout_retains_handle_and_panic_stays_visible() {
        let control = control();
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(move || { release_rx.recv_timeout(Duration::from_secs(2)).unwrap(); });
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let client = EClient::from_parts_controlled(Arc::new(SharedState::new()), tx, handle, "DU-fixture".into(), control);
        assert_eq!(client.disconnect_checked(Duration::ZERO).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(client.thread.lock().unwrap().is_some());
        release_tx.send(()).unwrap();
        client.disconnect_checked(Duration::from_secs(1)).unwrap();

        let (tx, _rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(|| { panic!("offline engine panic"); });
        let panicked_control = self::control();
        let client = EClient::from_parts_controlled(Arc::new(SharedState::new()), tx, handle, "DU-fixture".into(), panicked_control.clone());
        for _ in 0..2 {
            assert!(client.disconnect_checked(Duration::from_secs(1)).unwrap_err().to_string().contains("panicked"));
        }
        drop(client);
        assert!(panicked_control.join_workers(Duration::from_secs(1)).unwrap_err().to_string().contains("panicked"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn controlled_drop_does_not_join_stalled_worker_on_executor() {
        let control = control();
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(move || { release_rx.recv_timeout(Duration::from_secs(2)).unwrap(); });
        let (tx, _rx) = crossbeam_channel::bounded(1);
        tx.try_send(ControlCommand::Ping).unwrap();
        let client = EClient::from_parts_controlled(Arc::new(SharedState::new()), tx, handle, "DU-fixture".into(), control.clone());
        drop(client);
        // If Drop blocked on full Shutdown admission or the worker join, this
        // task could never execute the release; Tokio runs on this same thread.
        tokio::task::yield_now().await;
        assert!(control.is_cancelled());
        assert_eq!(control.join_workers(Duration::ZERO).unwrap_err().kind(), io::ErrorKind::TimedOut);
        release_tx.send(()).unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }

    #[test]
    #[cfg(debug_assertions)]
    fn actual_controlled_hotloop_panic_is_not_swallowed_by_legacy_recovery() {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = crossbeam_channel::bounded(64);
        let control = control();
        let mut engine = HotLoop::new(shared.clone(), None, None);
        engine.set_control_rx(rx);
        engine.set_connection_control(control.clone()).unwrap();
        // Exhaust the real debug counter so the panic occurs inside run(), then
        // crosses run_with_panic_recovery and the checked worker-join boundary.
        engine.context_mut().loop_iterations = u64::MAX;
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            engine.run_with_panic_recovery();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !handle.is_finished() && Instant::now() < deadline { thread::yield_now(); }
        if !handle.is_finished() { control.cancel(); }
        assert!(handle.is_finished(), "actual engine panic must terminate its worker");
        let client = EClient::from_parts_controlled(shared.clone(), tx, handle, "DU-fixture".into(), control);
        assert!(client.disconnect_checked(Duration::from_secs(1)).unwrap_err().to_string().contains("panicked"));
        assert!(shared.take_connection_lost());
    }
}
