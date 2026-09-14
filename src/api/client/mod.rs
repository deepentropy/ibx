//! ibapi-compatible EClient — Rust equivalent of C++ `EClientSocket`.
//!
//! Connects to IB, provides ibapi-matching method signatures, and dispatches
//! events to a [`Wrapper`](crate::api::wrapper::Wrapper) via `process_msgs()`.
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
//!     ..Default::default()
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

pub(crate) mod ask;
mod simple;
pub use ask::{AccountValue, OptionChain, OrderReport, PositionRow, ScanRow, Schedule};
mod market_data;
mod orders;
mod account;
pub(crate) mod reference;
pub(crate) mod dispatch;
mod stubs;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::control::adjustments::{AdjustedContract, Adjustment};
use crate::error_codes::Refusal;
use std::sync::{Arc, Mutex};
use std::thread;

use std::sync::mpsc::{Receiver, SyncSender};

use crate::types::model::{
    Contract as ApiContract, Order as ApiOrder, TagValue as ApiTagValue,
};
use crate::bridge::{Event, SharedState};
use crate::engine::hot_loop::EventSink;
use crate::client_core::ClientCore;
use crate::gateway::{Gateway, GatewayConfig, Session};
use crate::types::*;

// Re-export as public type names for the API surface
/// The contract type both surfaces share.
pub type Contract = ApiContract;
/// The order type both surfaces share.
pub type Order = ApiOrder;
/// One named option carried by a request.
pub type TagValue = ApiTagValue;

// Re-export public items from submodules
// Reads an order the caller composed, so it lives with the order model;
// reachable here because that is the path callers know it by.
pub use crate::client_core::parse_algo_params;

/// Configuration for connecting to IB via EClient.
///
/// # Live logins block on second-factor approval
///
/// With `paper: false`, [`connect()`](EClient::connect) enters a second-factor
/// approval window and **blocks** until the factor is approved or the attempt
/// runs out of time. This is expected — it is a human approval gate, not a
/// hang. How long the venue allows has not been timed here. Bound or avoid it by using `paper: true`, lowering
/// the timeout (via [`GatewayConfig::ib_key_timeout_secs`] when building through
/// the lower-level API), or supplying a `code_provider`. Paper logins skip the
/// gate entirely. An `info`-level log line is emitted when the wait begins
/// (`RUST_LOG=info`).
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
#[derive(Default)]
pub struct EClientConfig {
    /// The login.
    pub username: String,
    /// Its password. Held only for the length of the logon.
    pub password: String,
    /// Where to start the session.
    ///
    /// Leave it empty. A login is enough: the venue answers the first message
    /// by naming which server this account belongs on, and the session moves
    /// there — so what is named here is only where to knock. Name one for a
    /// test, or to knock at a particular region.
    pub host: String,
    /// Refuse to send anything that places, changes or withdraws an order.
    ///
    /// The gateway had this as a setting of its own, and the other client here
    /// takes it on connect. A Rust caller could not state it at all, so a
    /// session meant to only look could still trade.
    pub readonly: bool,
    /// What a gateway holds in its own configuration file.
    ///
    /// A gateway is a process configured by a file beside it; this client is a
    /// library, so those settings are stated here instead of in a file nobody
    /// writes. Applied as the session opens, and for the whole process
    /// [`GatewaySettings`](crate::settings::GatewaySettings).
    pub gateway: crate::settings::GatewaySettings,
    /// `false` enters the live second-factor approval gate on connect (blocking).
    /// `true` skips it. See the type-level docs.
    pub paper: bool,
    /// CPU core to pin this engine's hot loop to. `None` = no pinning. When
    /// running multiple engines, use a **distinct** core per engine.
    pub core_id: Option<usize>,
    /// Supplies the second-factor code. Required for accounts whose factor is
    /// an authenticator code — those have no push to fall back to, and connect
    /// fails without it. For IBKey accounts it selects Challenge/Response over
    /// waiting for a mobile push, so `None` is fine there.
    pub code_provider: Option<crate::auth::session::CodeProvider>,
    /// Offer a session captured earlier, instead of logging in again.
    ///
    /// Take it from [`session()`](EClient::session). The connect names that
    /// session and the server chooses: a challenge it can answer from the
    /// session alone, or the ordinary login. Whatever it chooses, this connects
    /// — a session it will not take costs a login, never an error.
    ///
    /// **The servers reached from here have not yet chosen the challenge.**
    /// Every observed login, including one offering a session left by a process
    /// that was killed rather than closed, has been answered with the ordinary
    /// one. The client asks and handles both answers, because the protocol
    /// carries both and this library already answers the challenge when a
    /// dropped connection is rebuilt. Set this and lose nothing; do not plan
    /// around it skipping a second factor until you have seen it do so.
    pub resume: Option<crate::auth::resume::ResumableSession>,
    /// What to do about a dropped connection.
    ///
    /// The default recovers on its own and keeps trying, which is what a
    /// process that must stay up wants and what having no gateway makes this
    /// library's job. Set it to bound the effort, or to be told about a loss
    /// and decide yourself.
    /// [`ReconnectConfig`](crate::reliability::ReconnectConfig).
    pub reconnect: crate::reliability::ReconnectConfig,
    /// Keep the session in this file, so a restart can offer it without a
    /// person present.
    ///
    /// Off unless set, and worth leaving off for now. Nothing about the session
    /// touches disk otherwise: it is held in memory for the life of the
    /// process, which is all a reconnect needs. Setting this writes a
    /// credential to disk, sealed under the account password and readable only
    /// by its owner, and buys whatever [`resume`](EClientConfig::resume) buys —
    /// which today, on the servers reached from here, is nothing. That is a
    /// cost with no measured return, so it is a decision rather than a default.
    pub session_file: Option<std::path::PathBuf>,
}

/// A calculation asked for before the venue had stated a model, kept until it
/// does.
#[derive(Clone)]
pub(crate) struct PendingOptionCalc {
    /// The contract it is on.
    pub(crate) contract: crate::types::model::Contract,
    /// Whether it inverts a price or prices a volatility.
    pub(crate) wants_volatility: bool,
    /// The price the caller supplied, where it supplied one.
    pub(crate) option_price: f64,
    /// The underlying price the caller supplied.
    pub(crate) under_price: f64,
    /// Whether the venue has since stated a model and this has been answered.
    ///
    /// Answered questions are kept rather than dropped, because the watch this
    /// client opened to obtain the model is withdrawn where the caller
    /// withdraws the calculation — and a question that is gone cannot be
    /// withdrawn. Nothing is sent on the strength of this; it only stops the
    /// entry being solved twice.
    pub(crate) answered: bool,
}

/// ibapi-compatible EClient. Matches C++ `EClientSocket` method signatures.
///
/// # Thread lifecycle
///
/// `connect()` spawns a single `ib-engine-hotloop` background thread.
/// The thread is **joined** on [`disconnect()`](EClient::disconnect) and on [`Drop`].
/// Dropping an `EClient` without calling `disconnect()` first is safe:
/// the `Drop` impl sends `Shutdown` and joins the thread.
///
/// # Losing the connection
///
/// When the engine stops — connection lost, reconnect exhausted, or the hot
/// loop panicked — the next [`process_msgs()`](EClient::process_msgs) call
/// fires [`connection_closed`](crate::api::wrapper::Wrapper::connection_closed) once
/// and
/// [`is_connected()`](EClient::is_connected) turns false. No error callback is
/// raised for this: the connectivity error codes are pushed by the server, not
/// synthesized locally.
pub struct EClient {
    pub(crate) shared: Arc<SharedState>,
    pub(crate) control_tx: SyncSender<ControlCommand>,
    pub(crate) thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// The account this session acts for.
    pub account_id: String,
    /// Every account this login holds, the first being [`EClient::account_id`].
    pub accounts: Vec<String>,
    pub(crate) connected: AtomicBool,
    /// True once `connection_closed` has been delivered, so it fires at most
    /// once per session.
    pub(crate) close_notified: AtomicBool,
    /// Whether this client asked for the session to end. The engine records
    /// the same, after the fact; this is known the moment the caller asks, so
    /// the end is said as the caller's own and not as a loss.
    pub(crate) stopped_by_caller: AtomicBool,
    /// Whether the caller asked for positions and has not withdrawn the ask.
    ///
    /// `req_positions` subscribes to a real-time feed, so a holding that
    /// moves afterwards is reported as it moves rather than only in the set
    /// held when the call was made.
    pub(crate) positions_requested: AtomicBool,
    /// Order records kept back because a fill for them was still queued.
    ///
    /// A fill is read against the record, so the record cannot be freed while
    /// one is waiting. Freed on the next read of the completed orders, which
    /// is when the fill has been delivered — so the deferral costs a pass and
    /// not the rest of the session.
    pub(crate) deferred_evictions: Mutex<std::collections::HashSet<u64>>,
    /// The requests watching holdings per account or model.
    ///
    /// `positionMulti` is the same live feed as `position`, asked for under a
    /// request id and withdrawn under it. Held apart from that flag because
    /// both may be watching at once and each is answered on its own callback.
    pub(crate) positions_multi_requested: Mutex<std::collections::HashSet<i64>>,
    /// The requests watching the account's figures per account or model.
    ///
    /// `accountUpdateMulti` is a subscription, not a question: a figure that
    /// moves after the first batch is reported again under the same request
    /// until the caller withdraws it. Held apart from the plain account
    /// subscription beside it, because both may be open at once and each is
    /// answered on its own callback.
    pub(crate) account_updates_multi_requested: Mutex<std::collections::HashSet<i64>>,
    /// Option calculations waiting on the venue to state a model.
    ///
    /// The calculation is answered from the venue's model for the contract,
    /// and a contract nobody is watching has no model stated for it. Asking
    /// opens the watch and the answer follows, rather than the question being
    /// refused because it was asked first.
    pub(crate) pending_option_calcs: Mutex<std::collections::HashMap<i64, PendingOptionCalc>>,
    /// Zero until the working orders the venue names at connect have been
    /// read and an id above them settled on.
    pub(crate) next_order_id: AtomicU64,
    /// One question at a time.
    ///
    /// A question drives the message pump itself, and the pump hands every
    /// message it drains to the collector that is running. A collector keeps
    /// what carries its own request id and discards the rest, so a second
    /// question asked while the first is pumping has its answer read and thrown
    /// away by the first, and waits out its timeout for a reply that already
    /// came. Held across the sending too, so the second question is not on the
    /// wire while the first is listening.
    pub(crate) asking: Mutex<()>,
    /// Where the callbacks a question does not care about are also delivered.
    ///
    /// A question holds the read turn and pumps into its own collector, and
    /// the queues empty as they are read — so a fill or a trade arriving while
    /// one runs reached a collector that ignores it and was gone. A session
    /// keeping a running record installs it here, and both are fed.
    ///
    /// Empty on a bare client, which has no record of its own to keep.
    pub(crate) kept: Mutex<Option<std::sync::Arc<Mutex<dyn crate::api::wrapper::Wrapper + Send>>>>,
    /// How many events the channel above discarded, if one is attached.
    pub(crate) discarded: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The orders the venue has finished with, as they were reported.
    ///
    /// The queue they arrive on empties as it is read and the venue does not
    /// send them again, so what has been read once is kept here: asked a
    /// second time, this client answered with none of them, which reads as an
    /// account that completed nothing today.
    pub(crate) completed: Mutex<Vec<(ApiContract, ApiOrder, crate::types::model::OrderState)>>,
    /// Which kind of trade stream each tick-by-tick request asked for.
    ///
    /// Every trade and the exchange's own are two streams, and the callback
    /// names which one it is carrying. Nothing on the record the venue sends
    /// says which, because the subscription decided it, so what the caller
    /// asked for is kept here and read back when the trades arrive.
    pub(crate) tbt_kinds: Mutex<std::collections::HashMap<i64, TbtType>>,
    pub(crate) core: ClientCore,
    pub(crate) session_token_bytes: Vec<u8>,
    pub(crate) session: crate::auth::resume::ResumableSession,
}

impl Drop for EClient {
    fn drop(&mut self) {
        // Ensure the hot-loop thread is stopped and joined. Dropping this
        // client ends the session — its connections go with the engine, so
        // there is nothing left to reuse — and a session that ends is logged
        // out, the way `disconnect` ends one. Stopping the loop and ending the
        // session are otherwise separate on purpose: a caller that stops the
        // engine and keeps its connections must not have the session logged
        // out from under it.
        let _ = self.control_tx.send(ControlCommand::Logout);
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        // Taken out before the join, not across it: the guard in an `if let`
        // scrutinee lives to the end of the body, and a second thread reaching
        // here would wait on this lock for as long as the engine takes to
        // stop — with no bound on that wait if the engine is wedged.
        let running = self.thread.lock().unwrap().take();
        if let Some(h) = running {
            let _ = h.join();
        }
    }
}

thread_local! {
    /// Whether this thread is inside one of the calls that answer.
    ///
    /// Those number themselves in a band of their own and reach the wire
    /// through the same requests a caller does, so the number alone cannot say
    /// which of the two is asking. Set for the length of the call and read
    /// where a caller's number is narrowed.
    static ANSWERING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread is inside a call that answers.
pub(crate) fn answering_now() -> bool {
    ANSWERING.with(std::cell::Cell::get)
}

/// Mark this thread as inside a call that answers, until dropped.
pub(crate) struct Answering(bool);

impl Answering {
    pub(crate) fn begin() -> Self {
        Self(ANSWERING.replace(true))
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        ANSWERING.set(self.0);
    }
}

thread_local! {
    /// Which session this thread is inside a read of, if any.
    ///
    /// A read hands every message it drains to a wrapper, and a wrapper is a
    /// caller's own code. A question asked from inside one waits for the turn
    /// that same thread is already holding, on a lock that is not re-entrant,
    /// and the read that would have answered it is the one it is inside — so
    /// neither ever runs again, and the deadline that would have said so is
    /// set inside a wait that never starts.
    ///
    /// Held as the session being read rather than as a flag, so a callback on
    /// one session may still ask a question of another.
    static READING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Which session this thread is inside a read of. Zero: none.
pub(crate) fn reading_now() -> usize {
    READING.with(std::cell::Cell::get)
}

/// Mark this thread as inside a read of one session, until dropped.
pub(crate) struct Reading(usize);

impl Reading {
    pub(crate) fn begin(session: usize) -> Self {
        Self(READING.replace(session))
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        READING.set(self.0);
    }
}

/// Narrow a caller's req_id to the width the request carries on the wire.
///
/// `EClient` takes req_id as `i64` for ibapi parity, but these requests encode
/// it as a `u32`, and the callbacks report back whatever was encoded. A cast
/// would answer under an id the caller never used — and `next_order_id()`
/// hands out ids well past `u32::MAX`, so the ibapi idiom of one counter for
/// orders and requests hit it on the first call. Refuse instead.
pub(crate) fn wire_req_id(req_id: i64) -> Result<u32, Refusal> {
    let id = u32::try_from(req_id).map_err(|_| {
        Refusal::validation(format!(
            "req_id {req_id} is outside the range this request can carry (0..={})", u32::MAX,
        ))
    })?;
    // The band the answering calls number themselves in is not a caller's to
    // use. An answer is handed to whoever is waiting under its number, so a
    // request numbered inside it has its answer taken by one of those calls,
    // about something it did not ask for — and that call loses its own.
    //
    // Told apart by who is asking rather than by the number, because the number
    // is held precisely while the collision is possible: an answering call
    // marks itself for the length of its own call, and nothing else can.
    if crate::bridge::ReferenceState::is_ask_id(id) && !answering_now() {
        return Err(Refusal::validation(format!(
            "req_id {req_id} is inside the range this client numbers its own answering \
             calls in, and an answer under it would be taken for one of theirs: number \
             the request below {}",
            crate::bridge::ReferenceState::ASK_ID_BASE,
        )));
    }
    // The top of the range already means something: it is what this client
    // reports when a message names no request at all, and it reaches a caller
    // as minus one. A request numbered with it is answered under a number that
    // is not the one it asked under.
    if id == crate::bridge::ReferenceState::NO_REQUEST {
        return Err(Refusal::validation(format!(
            "req_id {req_id} is the number this client uses for a message that names no \
             request, and an answer under it reaches a caller as -1: number the request \
             below it",
        )));
    }
    // Above this the engine numbers the lookups it takes for itself, and the
    // answers to those are kept rather than handed on. A caller numbering a
    // request here is answered by nobody: the reply is read as internal and
    // its callbacks are suppressed, which reads as a request that vanished.
    if id >= crate::bridge::ENGINE_ID_BASE {
        return Err(Refusal::validation(format!(
            "req_id {req_id} is inside the range this client numbers the lookups it takes \
             for itself in, and an answer under it is kept rather than handed on: number \
             the request below {}",
            crate::bridge::ENGINE_ID_BASE,
        )));
    }
    Ok(id)
}

/// Check a value a caller stated before it rides the wire as one field.
///
/// The byte that separates fields cannot sit inside one: carried anyway, the
/// value stops where the byte sits and everything after it arrives as fields
/// the caller never wrote. Nothing this protocol sends can carry the byte in
/// a value, so the request is refused rather than sent stating something
/// else.
pub(crate) fn wire_text(what: &str, value: &str) -> Result<(), Refusal> {
    if value.contains(crate::protocol::fix::SOH as char) {
        return Err(Refusal::validation(format!(
            "{what} carries the byte that separates fields on the wire, and a field \
             cannot hold it: what follows the byte would go out as fields this request \
             never stated",
        )));
    }
    Ok(())
}

/// The request a refusal is reported against, or the mark for none.
///
/// A number too wide to carry is reported against no request rather than
/// against its own low half. Narrowed, a refusal for one request is delivered
/// under another that a caller may well be waiting on — and this account hands
/// out order ids far wider than a request number, so a caller numbering both
/// from one counter reaches this with every refusal it gets.
pub(crate) fn carried_under(req_id: i64) -> u32 {
    match u32::try_from(req_id) {
        Ok(id) if id != crate::bridge::ReferenceState::NO_REQUEST => id,
        _ => crate::bridge::ReferenceState::NO_REQUEST,
    }
}

/// The gateway's view of an [`EClientConfig`].
///
/// Extracted so the forwarding is checkable without opening a socket: the
/// second-factor provider reaching the gateway is the whole of what makes the
/// feature usable from this client, and it is one line that a refactor can
/// drop silently.
fn gateway_config(config: &EClientConfig) -> GatewayConfig {
    GatewayConfig {
        // Settled here, once, on the caller's thread: everything downstream
        // reads a value rather than the process it happens to run in.
        settings: std::sync::Arc::new(config.gateway.resolve()),
        username: config.username.clone(),
        password: zeroize::Zeroizing::new(config.password.clone()),
        // A caller with a login should not have to know a hostname. The one
        // it would name is where every session starts anyway, and the venue
        // answers the first message by naming which server to go to — every
        // session in this codebase's logs is redirected within a second of
        // connecting. Naming one stays possible, for a test or a region.
        host: if config.host.trim().is_empty() {
            crate::config::CCP_HOSTS[0].to_string()
        } else {
            config.host.clone()
        },
        paper: config.paper,
        accept_invalid_certs: false,
        ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: config.code_provider.clone(),
        // What the caller handed back, or what was left in the file they named.
        // A file that cannot be read is a slower start, not a failed one: the
        // password is still here, and the whole point of the file is to avoid
        // needing a person, which an error thrown at one defeats.
        resume: config.resume.clone().or_else(|| {
            config.session_file.as_ref().and_then(|path| {
                crate::auth::resume::load(path, &config.username, &config.password, config.paper)
            })
        }),
    }
}

/// What a reconnect logs in with.
///
/// Extracted for the reason [`gateway_config`] is: the settings a session
/// opened under have to reach the reconnect, and building this inline left
/// them as `Default::default()`. Nothing failed until a connection went away,
/// and then the session came back announcing a different build, locale and
/// timezone, and asked the venue for every execution it holds where the caller
/// had asked for today's.
fn caller_auth(config: &EClientConfig, gateway: &GatewayConfig) -> crate::gateway::CallerAuth {
    crate::gateway::CallerAuth {
        settings: gateway.settings.clone(),
        host: config.host.clone(),
        username: config.username.clone(),
        password: zeroize::Zeroizing::new(config.password.clone()),
        paper: config.paper,
        code_provider: gateway.code_provider.clone(),
        ib_key_timeout_secs: gateway.ib_key_timeout_secs,
        ib_key_token_sub_type: gateway.ib_key_token_sub_type.clone(),
    }
}

impl EClient {
    /// Connect to IB and start the engine.
    pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_inner(config, None)
    }

    /// Connect to IB and start the engine with an event channel attached.
    ///
    /// A second, optional delivery path for a program that would rather own a
    /// queue than be called back. It is bounded, and an event arriving at a
    /// full one is discarded rather than made to wait — a session that stalled
    /// on a slow reader would stop carrying market data. Read
    /// [`events_lost`](EClient::events_lost) to learn whether that happened.
    ///
    /// One reader, and it is told what it drains and nothing else. For a
    /// program that wants none of it dropped, drive
    /// [`process_msgs`](EClient::process_msgs) with a
    /// [`Wrapper`](crate::api::wrapper::Wrapper) instead: its callbacks are
    /// called with the message rather than sent a copy of it, so there is no
    /// queue to fill and nothing to fall out of one.
    ///
    /// This is a second, optional delivery path that runs alongside
    /// `process_msgs` — it does not replace it, and nothing is removed from the
    /// wrapper callbacks when it is in use.
    ///
    /// The channel is bounded by `capacity`; the engine never blocks on it, so
    /// a consumer that falls behind loses events rather than slowing the hot
    /// loop. Drain it from a thread that is not the one calling
    /// `process_msgs()`, or keep `capacity` generous.
    ///
    /// Attaching a channel makes the engine build events it would otherwise
    /// skip, which for bar batches and contract definitions means one deep copy
    /// each. Use [`connect()`](EClient::connect) when you only need the wrapper
    /// callbacks.
    pub fn connect_with_events(
        config: &EClientConfig,
        capacity: usize,
    ) -> Result<(Self, Receiver<Event>), Box<dyn std::error::Error>> {
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(capacity.max(1));
        let lost = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut client =
            Self::connect_inner(config, Some(EventSink::new(event_tx, std::sync::Arc::clone(&lost))))?;
        client.discarded = lost;
        Ok((client, event_rx))
    }

    fn connect_inner(
        config: &EClientConfig,
        event_tx: Option<EventSink>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Before anything is logged, because the settings that say where it
        // goes are among them.
        crate::logging::apply(&config.gateway);
        let gw_config = gateway_config(config);

        let Session { gateway: gw, market_data: farm_conn, trading: ccp_conn, historical: hmds_conn, security_definition: secdef_conn } = Gateway::connect(&gw_config)?;
        let account_id = gw.account_id.clone();
        let accounts = gw.accounts.clone();
        let session = crate::client_core::remember_session(
            config.session_file.as_deref(),
            &config.password,
            &gw,
            &config.username,
            config.paper,
        );
        let session_token_bytes = session.token.clone();

        let shared = Arc::new(SharedState::new());
        // Before the engine's threads exist, so nothing reads a setting that
        // can still change.
        shared.set_settings(gw_config.settings.clone());
        gw.populate_init_data(&shared);

        let (mut hot_loop, control_tx) = crate::engine::hot_loop::HotLoop::for_session(
            gw,
            shared.clone(), event_tx, farm_conn, ccp_conn, hmds_conn, secdef_conn, config.core_id,
            caller_auth(config, &gw_config),
        );
        // The switch a gateway carries turns recovery off wherever it is
        // stated — in code, or in the environment a program migrating from a
        // gateway already sets. What is left of `reconnect` still stands: it
        // says how hard to try, and this says whether to.
        let mut recovery = config.reconnect.clone();
        if !gw_config.settings.reconnect_on_socket_err {
            recovery.policy = crate::reliability::ReconnectPolicy::Manual;
        }
        hot_loop.set_reconnect_config(recovery);

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || { hot_loop.run_with_panic_recovery(); })?;

        let core = ClientCore::new();
        // Stated before the client is handed back, so a caller cannot place
        // anything between the session opening and the setting taking hold.
        core.set_readonly(config.readonly);
        core.set_registration_timeout(gw_config.settings.registration_timeout);

        Ok(Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            account_id,
            accounts,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            stopped_by_caller: AtomicBool::new(false),
            positions_requested: AtomicBool::new(false),
            deferred_evictions: Mutex::new(std::collections::HashSet::new()),
            positions_multi_requested: Mutex::new(std::collections::HashSet::new()),
            account_updates_multi_requested: Mutex::new(std::collections::HashSet::new()),
            pending_option_calcs: Mutex::new(std::collections::HashMap::new()),
            next_order_id: AtomicU64::new(0),
            asking: Mutex::new(()),
            kept: Mutex::new(None),
            discarded: Default::default(),
            completed: Mutex::new(Vec::new()),
            tbt_kinds: Mutex::new(std::collections::HashMap::new()),
            core,
            session_token_bytes,
            session,
        })
    }

    /// Construct from pre-built components (for testing or custom setups).
    #[doc(hidden)]
    pub fn from_parts(
        shared: Arc<SharedState>,
        control_tx: SyncSender<ControlCommand>,
        handle: thread::JoinHandle<()>,
        account_id: String,
    ) -> Self {
        Self {
            shared,
            control_tx,
            thread: Mutex::new(Some(handle)),
            accounts: vec![account_id.clone()],
            account_id,
            connected: AtomicBool::new(true),
            close_notified: AtomicBool::new(false),
            stopped_by_caller: AtomicBool::new(false),
            positions_requested: AtomicBool::new(false),
            deferred_evictions: Mutex::new(std::collections::HashSet::new()),
            positions_multi_requested: Mutex::new(std::collections::HashSet::new()),
            account_updates_multi_requested: Mutex::new(std::collections::HashSet::new()),
            pending_option_calcs: Mutex::new(std::collections::HashMap::new()),
            next_order_id: AtomicU64::new(0),
            asking: Mutex::new(()),
            kept: Mutex::new(None),
            discarded: Default::default(),
            completed: Mutex::new(Vec::new()),
            tbt_kinds: Mutex::new(std::collections::HashMap::new()),
            core: {
                // A client assembled from parts carries no settings of its
                // own, and the constructors that connect take this from
                // theirs. Read the same way here, so a caller that states a
                // registration timeout is answered however its client was
                // built rather than only when the client connected for
                // itself.
                let core = ClientCore::new();
                core.set_registration_timeout(
                    crate::settings::GatewaySettings::default().resolve().registration_timeout,
                );
                core
            },
            session_token_bytes: Vec::new(),
            session: Default::default(),
        }
    }

    /// Map a reqId to an InstrumentId (for testing without a live engine).
    #[doc(hidden)]
    pub fn map_req_instrument(&self, req_id: i64, instrument: InstrumentId) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().insert(instrument, req_id);
    }

    /// Pre-populate the order tracker (for testing the dispatcher path
    /// without going through the engine's place-order flow).
    #[doc(hidden)]
    pub fn track_order_for_test(
        &self,
        order_id: u64,
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
    pub(crate) fn send(&self, cmd: ControlCommand) -> Result<(), Refusal> {
        self.control_tx
            .send(cmd)
            .map_err(|e| Refusal::not_connected(format!("Engine stopped: {e}")))
    }

    // ── Connection ──

    /// How many events the channel from
    /// [`connect_with_events`](EClient::connect_with_events) discarded.
    ///
    /// The engine never waits on a reader — a session that stalled on one
    /// would stop carrying market data — so an event arriving at a full
    /// channel is dropped. A program that acted on every fill it saw needs to
    /// know the difference between that and every fill there was. Zero for a
    /// session with no channel attached, and for one whose reader kept up.
    pub fn events_lost(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }

    /// False after [`disconnect()`](EClient::disconnect), after the engine
    /// has ended the session, and after a `process_msgs()` call that
    /// observed the engine stopping.
    ///
    /// The engine records an ended session itself. A shape that never pumps
    /// `process_msgs` hears of it nowhere else, and kept saying connected
    /// after the session underneath it was over.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
            && self.shared.reference.session_over().is_none()
    }

    /// Whether this session is finished rather than merely disconnected:
    /// closed by [`disconnect()`](EClient::disconnect), or given up on by the
    /// engine, which records why. A loss the engine is still working on is
    /// neither — `is_connected()` reads false between the 1100 and the 1102,
    /// and a request made then is carried when the transports come back.
    /// Refused under 504 there, a withdrawal was left unapplied and the feed
    /// came back with the session; the reference client serves that window.
    pub fn session_over(&self) -> bool {
        self.shared.reference.session_over().is_some()
    }

    /// Disconnect from IB.  Sends `Shutdown` to the hot loop, waits for the
    /// background thread to exit, and marks the client as disconnected.
    pub fn disconnect(&self) {
        self.stopped_by_caller.store(true, Ordering::Release);
        // The session is ending, so the venue is told before the engine stops.
        let _ = self.control_tx.send(ControlCommand::Logout);
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        // Taken out before the join, not across it: the guard in an `if let`
        // scrutinee lives to the end of the body, and a second thread reaching
        // here would wait on this lock for as long as the engine takes to
        // stop — with no bound on that wait if the engine is wedged.
        let running = self.thread.lock().unwrap().take();
        if let Some(h) = running {
            let _ = h.join();
        }
        self.connected.store(false, Ordering::Release);
        self.core.reset();
    }
}

impl EClient {
    /// Which slot a contract holds on this session, if it holds one.
    pub fn instrument_of(&self, con_id: i64) -> Option<crate::types::InstrumentId> {
        // Through the same lookup every other reader uses, which drops what the
        // engine has given back before it answers. Read from the map directly,
        // this named a slot that had already gone to the next contract.
        self.core.cached_instrument(&self.shared, con_id)
    }

    /// The session's own state, for reading what has arrived.
    pub fn shared_state(&self) -> &Arc<SharedState> {
        &self.shared
    }

    /// The turn, for the thread that reads the session on its own.
    ///
    /// `process_msgs` takes this and then reads. A reader that must order the
    /// turn ahead of the record it reads into cannot use that — the turn is
    /// not re-entrant — so it takes the turn here and calls the read directly.
    pub(crate) fn turn_for_reading(&self) -> std::sync::MutexGuard<'_, ()> {
        self.asking.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What tells this session apart from another in the same process.
    pub(crate) fn which_session(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// Take the session's turn to ask one question.
    ///
    /// One question at a time: see [`EClient::asking`]. Refused rather than
    /// waited on where this thread is the one reading the session — a question
    /// asked from inside a callback waits for a turn its own thread holds, and
    /// the read that would answer it is the read it is inside. Told on the
    /// spot, a caller can do something about it; left waiting, nothing about
    /// that program runs again.
    pub(crate) fn take_the_turn(&self) -> Result<std::sync::MutexGuard<'_, ()>, Refusal> {
        if reading_now() == self.which_session() {
            return Err(Refusal::validation(
                "a question cannot be asked from inside a callback: this thread is the \
                 one reading the session, and the answer would arrive on the read it is \
                 inside. Ask from another thread, or keep what you need and ask once the \
                 read has returned".to_string(),
            ));
        }
        Ok(self.asking.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// What the venue last stated for a contract, and the contract as it named
    /// it.
    ///
    /// Not every action of the contract's life: each question replaces what the
    /// one before it left, so this states the answer to the last range asked
    /// about.
    ///
    /// The venue serves no adjusted series of its own: asked for one by name it
    /// answers that it has no such data, and the trades it does serve are raw.
    /// A series that crosses a split steps by the split's ratio with nothing in
    /// it saying so, which is a wrong number rather than a missing one.
    ///
    /// This is what a caller adjusts with.
    /// [`scale_before`](crate::scale_before) turns a date
    /// and these actions into the factor a price from that date carries, so a
    /// caller can put a series on one scale. Three kinds move a price: a stock
    /// dividend, a split and a spin-off. The factor is the value the action
    /// states, and a spin-off states its reciprocal — established against a
    /// contract that split ten for one, where the closes either side were
    /// 1208.88 and 121.79, and every close before it folds to a tenth.
    ///
    /// A cash dividend is stated here and moves nothing, and neither does a
    /// rights offer; a future rollover carries no value to move anything by.
    /// That is the scale the adjusted series is stated in, not a gap in this
    /// client: a series that took dividends off as well would be on a second
    /// scale, and nothing the venue serves beside it would be on that one.
    ///
    /// Empty until the venue has stated them for the contract, which it does
    /// once per contract on a historical request.
    pub fn adjustments(&self, con_id: &str) -> Option<(AdjustedContract, Vec<Adjustment>)> {
        self.shared.reference.adjustments_for(con_id)
    }

    /// Frames this session kept exactly as the venue sent them, by connection.
    ///
    /// Empty unless `IBX_CAPTURE_WIRE` is set. A reading checked only against
    /// frames this client made up says nothing about the ones that arrive.
    pub fn unread_wire(&self) -> Vec<(&'static str, String)> {
        self.shared.market.unread_wire()
    }

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

    /// The session this connection established, for a caller that wants to
    /// resume from it later.
    ///
    /// Hand it back through [`EClientConfig::resume`] on a subsequent connect.
    /// Keep it wherever the process keeps secrets — it is a credential, and
    /// where it lives is the caller's decision, which is why nothing here
    /// writes it anywhere by default.
    pub fn session(&self) -> &crate::auth::resume::ResumableSession {
        &self.session
    }

}

#[cfg(test)]
mod host_default_tests {
    use super::*;

    /// A caller with a login and nothing else gets a session. The hostname it
    /// would otherwise have to name is where every session starts anyway, and
    /// the venue redirects from there.
    #[test]
    fn a_config_without_a_host_still_knows_where_to_knock() {
        let config = EClientConfig {
            username: "someone".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        };
        assert!(config.host.is_empty(), "a caller stated none");
        let resolved = gateway_config(&config);
        assert_eq!(resolved.host, crate::config::CCP_HOSTS[0]);
    }

    /// And a reconnect logs in under the settings the session opened under.
    ///
    /// Built inline, they were `Default::default()`: the first login announced
    /// what the caller stated and every login after a drop announced whatever
    /// the environment held, on the same session.
    #[test]
    fn a_reconnect_states_what_the_session_opened_under() {
        let config = EClientConfig {
            username: "someone".to_string(),
            password: "secret".to_string(),
            gateway: crate::settings::GatewaySettings {
                timezone: Some("America/New_York".to_string()),
                build: Some("10999".to_string()),
                execution_reports: Some(crate::settings::ExecutionReportScope::Today),
                ..Default::default()
            },
            ..Default::default()
        };
        let auth = caller_auth(&config, &gateway_config(&config));
        assert_eq!(auth.settings.timezone, "America/New_York");
        assert_eq!(auth.settings.build, "10999");
        assert_eq!(
            auth.settings.execution_reports,
            crate::settings::ExecutionReportScope::Today,
            "a reconnect asking for every execution the venue holds is a request \
             the caller did not make",
        );
    }

    /// One that is named is used as given.
    #[test]
    fn a_host_that_is_named_is_used() {
        let config = EClientConfig {
            username: "someone".to_string(),
            password: "secret".to_string(),
            host: "ndc1.ibllc.com".to_string(),
            ..Default::default()
        };
        assert_eq!(gateway_config(&config).host, "ndc1.ibllc.com");
    }
}

#[cfg(test)]
mod readonly_tests {
    use super::*;

    /// And the calls a caller actually makes are the ones that refuse.
    ///
    /// The guard existed and every trading call on this surface reached the
    /// venue without consulting it: the flag was stored, the helper was tested
    /// directly, and a session opened read-only placed orders.
    #[test]
    fn the_trading_calls_are_the_ones_that_refuse() {
        let (client, _rx, _shared) = crate::api::client::tests::test_client();
        client.core.set_readonly(true);
        let spy = Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() };
        let order = crate::types::model::Order::limit("BUY", 1.0, 1.00);

        assert!(client.place_order(1, &spy, &order).is_err(), "an order is refused");
        assert!(client.cancel_order(1, "").is_err(), "a cancel is refused");
        assert!(client.cancel_order_by_perm_id(1).is_err(), "a cancel by permanent id is refused");
        assert!(client.req_global_cancel().is_err(), "a global cancel is refused");
        assert!(
            client.exercise_options(1, &spy, 1, 1, "", false, Default::default()).is_err(),
            "an exercise is refused",
        );
        // Three orders under one call, and the only trading call that reached
        // the engine without consulting the flag: it sends through a path of
        // its own, which the guard did not sit on.
        assert!(
            client.place_bracket(&spy, "BUY", 1.0, 100.0, 110.0, 90.0).is_err(),
            "a bracket is refused",
        );
    }

    /// An order is named on the wire by its number, and a cancel that names
    /// one the venue cannot have holds nothing back: cast unchecked, -1 left
    /// as a cancel for the largest number there is.
    #[test]
    fn a_cancel_names_a_number_the_venue_could_have_handed_out() {
        let (client, _rx, _shared) = crate::api::client::tests::test_client();
        for absurd in [-1_i64, 0, i64::MIN] {
            assert!(client.cancel_order(absurd, "").is_err(), "{absurd} was sent");
        }
        // A withdrawal names an order this client is working, or it is
        // answered rather than sent.
        client.core.track_order(
            1, crate::types::model::Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() }, crate::types::model::Order::default(), 0,
        );
        assert!(client.cancel_order(1, "").is_ok(), "an ordinary one still goes");
    }

    /// A session meant only to look refuses to place, change or withdraw an
    /// order. The other client here has taken this on connect all along; a
    /// Rust caller could not state it, so a read-only session could still
    /// trade.
    #[test]
    fn a_read_only_session_refuses_to_trade() {
        let core = ClientCore::new();
        core.set_readonly(true);
        assert!(core.refuse_if_readonly("place an order").is_err());
        assert!(
            core.refuse_if_readonly("place an order")
                .unwrap_err()
                .to_lowercase()
                .contains("read"),
            "the refusal does not say why",
        );
    }

    /// And one that was not asked for does not.
    #[test]
    fn an_ordinary_session_is_not_refused() {
        let core = ClientCore::new();
        core.set_readonly(false);
        assert!(core.refuse_if_readonly("place an order").is_ok());
    }
}
