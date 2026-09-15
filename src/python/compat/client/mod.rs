//! ibapi-compatible EClient class that wraps IbEngine.

mod market_data;
mod orders;
mod account;
mod reference;
mod ask;
mod dispatch;
mod stubs;
// Not in a wheel. See the `test-helpers` feature.
#[cfg(feature = "test-helpers")]
mod test_helpers;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use std::sync::mpsc::SyncSender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::auth::session::{CodeProvider, IbKeyChallenge, SecondFactor};
use crate::bridge::{Event, SharedState};
use crate::client_core::ClientCore;

/// What the reference client reports for a request made before connecting.
const NOT_CONNECTED_CODE: i64 = 504;
use crate::gateway::{Gateway, GatewayConfig, Session};
use crate::types::*;
use super::contract::Contract;

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
/// the `Drop` impl sends `Shutdown` and joins the thread. That holds for a
/// client that is its own wrapper, as `App(EWrapper, EClient)` built with
/// `wrapper=self` is: the wrapper is visited on traversal and released on
/// clear, so the cyclic collector finds that cycle and the drop runs.
///
/// The client is **reconnectable**: calling `disconnect()` resets all session
/// state so that a subsequent `connect()` on the same instance works correctly.
#[pyclass(frozen, subclass)]
pub struct EClient {
    /// The EWrapper the callbacks go to, from the moment `__init__` says which
    /// (typically `self` in the `App(EWrapper, EClient)` pattern) until the
    /// cyclic collector clears it, which is how that pattern's cycle is broken.
    pub(crate) wrapper: RwLock<Option<Py<PyAny>>>,
    /// The callable belongs to this client so the collector sees its bound
    /// application. Reconnect credentials hold only a weak reference to it.
    code_provider: Mutex<Option<Arc<Py<PyAny>>>>,
    /// Set by connect(), cleared by disconnect.
    pub(crate) shared: Mutex<Option<Arc<SharedState>>>,
    /// Set by connect(), cleared by disconnect.
    pub(crate) control_tx: Mutex<Option<SyncSender<ControlCommand>>>,
    pub(crate) next_order_id: AtomicU64,
    /// Where the last id handed out is kept, and under which key. Empty when
    /// the caller asked for no file, which makes the counter this session's
    /// alone and lets it collide with what an earlier one used.
    pub(crate) _thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Set by connect(), cleared by disconnect.
    pub(crate) account_id: Mutex<Option<String>>,
    /// Every account this login holds, the first being `account_id`.
    pub(crate) accounts: Mutex<Vec<String>>,
    pub(crate) connected: AtomicBool,
    /// How many times this client has been disconnected.
    ///
    /// A connect claims the flag, then releases the GIL for the whole logon —
    /// which is what the documented pattern asks for, a connect on a worker
    /// thread the caller times out itself. A disconnect landing in that window
    /// finds nothing installed to stop, clears what is already clear, and says
    /// the session closed. The connect behind it then installs a live session
    /// and says it is connected, so the caller was told the session closed
    /// while a logged-in venue session went on running.
    ///
    /// Read either side of the logon: a change across it means a disconnect
    /// answered for this connect, and the session it produced is not one the
    /// caller asked to keep.
    pub(crate) disconnects: AtomicU64,
    /// Whether the caller asked for positions and has not withdrawn the ask.
    ///
    /// `reqPositions` subscribes to a real-time feed, so a holding that moves
    /// afterwards is reported as it moves. Answering only the set held when
    /// the call was made left a caller tracking positions from a snapshot
    /// that went stale on the next fill.
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
    /// Whether this session is finished rather than merely disconnected.
    ///
    /// The engine announces a loss it is still working on and a loss it has
    /// given up on with the same event, and only records a reason for the
    /// second. Kept apart from `connected` because the two answer different
    /// questions: a caller asks the first whether a request will reach the
    /// venue now, and the pumps ask this one whether there is any point
    /// waiting for it to.
    pub(crate) session_ended: AtomicBool,
    /// Whether the caller has been told this session closed.
    ///
    /// Delivered once, and from wherever the caller drives the dispatch from:
    /// a program with an event loop of its own drives it a pass at a time and
    /// never reaches the end of `run`, which is where the notice used to be
    /// sent — so it was never told the session had ended at all.
    pub(crate) close_notified: AtomicBool,
    /// Answers built on the caller's thread, waiting for a dispatch pass.
    ///
    /// The reference client answers every request from its own loop, so a
    /// program written against it may hold a lock across a request and take it
    /// again in the callback. Answered inside the request, as these were, that
    /// program stops there and neither side ever moves. What is answered is
    /// built when the request is made, so it states what was true then; it
    /// reaches the wrapper on the next pass, in the order the answers were
    /// made.
    pub(crate) waiting_answers: Mutex<std::collections::VecDeque<(&'static str, Py<pyo3::types::PyTuple>)>>,
    /// The number this session connected under, as the caller gave it.
    ///
    /// One session holds the account here, so this does not route anything.
    /// It is kept because some calls are answered by client id: binding orders
    /// entered elsewhere is refused for any client id but 0.
    pub(crate) client_id: AtomicI32,
    /// The server this session connected to: the one it knocked on, which is
    /// the one the caller named. Held only while there is a session.
    pub(crate) auth_host: Mutex<Option<String>>,
    /// When the venue says this session logged in, by its own clock and in
    /// its own spelling. Held only while there is a session.
    pub(crate) logged_in_at: Mutex<Option<String>>,
    /// Receiver for engine events (disconnects, etc.).
    pub(crate) event_rx: Mutex<Option<std::sync::mpsc::Receiver<Event>>>,
    /// What this session's event channel discarded because it was full.
    ///
    /// Kept rather than handed away: counted into a total nobody holds, a
    /// program that acted on every event it saw had no way to tell that from
    /// every event there was.
    pub(crate) events_lost: Arc<std::sync::atomic::AtomicU64>,
    /// Sender for test-injected events (test-only).
    #[doc(hidden)]
    pub(crate) _test_event_tx: Mutex<Option<std::sync::mpsc::SyncSender<Event>>>,
    /// Holds the control channel's receiving end for a test-connected client.
    /// Dropping it closed the channel, so a client that reported itself
    /// connected failed every request that sends one.
    pub(crate) _test_control_rx: Mutex<Option<std::sync::mpsc::Receiver<ControlCommand>>>,
    /// Which kind of trade stream each tick-by-tick request asked for, as the
    /// number the callback states it under: 1 for the exchange's own prints
    /// and 2 for every print including those reported away from it.
    ///
    /// The trade record does not carry the kind, so it is kept per request id.
    /// Without it a caller holding both subscriptions cannot tell the two
    /// streams apart.
    pub(crate) tbt_kind: Mutex<HashMap<i64, i32>>,
    /// Option calculations asked for before the venue had stated a model for
    /// the contract, kept until it does.
    ///
    /// The venue states a model only for a contract something is watching, so
    /// a question about one nobody watches opens the watch and waits rather
    /// than being refused for having been asked first. Answered on each
    /// dispatch pass and dropped when the caller withdraws it.
    pub(crate) pending_option_calcs:
        Mutex<HashMap<i64, crate::api::client::PendingOptionCalc>>,
    /// The orders this session has seen the venue finish with, kept.
    ///
    /// The queue they arrive on empties as it is read and the venue does not
    /// send them again, so without this a second request is answered with none
    /// of them and the account reads as having completed nothing. The Rust
    /// surface keeps the same
    /// archive for the same reason.
    #[allow(clippy::type_complexity)]
    pub(crate) completed: Mutex<Vec<(
        crate::types::model::Contract,
        crate::types::model::Order,
        crate::types::model::OrderState,
    )>>,
    /// Shared subscription tracking and dispatch preparation.
    pub(crate) core: ClientCore,
}

impl Drop for EClient {
    fn drop(&mut self) {
        // Both taken out of their locks first. A guard built in the scrutinee
        // is held for the whole body, and these bodies block: the sends are
        // bounded, and the join waits on the engine thread.
        let tx = self.control_tx.lock().unwrap().clone();
        if let Some(tx) = tx {
            // Dropping the client ends the session, so the venue is told. The
            // channel is bounded and these wait on a hot loop that is behind,
            // and dealloc runs with the GIL held — so detach for them, the same
            // way the join below does, and for the same reason.
            let sent = Python::try_attach(|py| {
                py.detach(|| {
                    let _ = tx.send(ControlCommand::Logout);
                    let _ = tx.send(ControlCommand::Shutdown);
                });
            });
            if sent.is_none() {
                let _ = tx.send(ControlCommand::Logout);
                let _ = tx.send(ControlCommand::Shutdown);
            }
        }
        let thread = self._thread.lock().unwrap().take();
        if let Some(h) = thread {
            // A wedged engine never returns from join. Detach so
            // that stall parks this thread, not the whole interpreter
            // try_attach, not attach: dealloc also runs during
            // interpreter shutdown (and `wrapper` commonly points back at
            // the object embedding this EClient, so the cyclic GC — not
            // just refcounting — can be the one calling drop), and attach
            // is unsound there. Fall back to a plain join in that case,
            // same as before this fix.
            let mut h = Some(h);
            Python::try_attach(|py| {
                if let Some(h) = h.take() {
                    py.detach(|| { let _ = h.join(); });
                }
            });
            if let Some(h) = h {
                let _ = h.join();
            }
        }
    }
}

/// Narrow a caller's req_id to the width the request carries on the wire,
/// refusing rather than truncating.
///
/// What need not fit is an id a caller numbers themselves, and
/// a truncated one answers under a number they never used.
pub(crate) fn wire_req_id(req_id: i64) -> PyResult<u32> {
    crate::api::client::wire_req_id(req_id)
        .map_err(|refusal| PyRuntimeError::new_err(refusal.message))
}

/// Narrow another signed value the caller stated to the width its request
/// carries, refusing rather than wrapping.
///
/// Same reason as `wire_req_id`, for the fields beside it. A contract id or a
/// count below zero wraps to a value above four billion, which asks about the
/// wrong contract or for every row the venue holds.
pub(crate) fn wire_u32(what: &str, value: i64) -> PyResult<u32> {
    u32::try_from(value).map_err(|_| {
        PyRuntimeError::new_err(format!(
            "{what} {value} is outside the range this request can carry (0..={})", u32::MAX,
        ))
    })
}

/// Narrow a contract id the way the request surface narrows it, refusing a
/// zero or a negative one the same way: this surface and that one carry the
/// same number on the same wire, and agreeing on one and not the other asks
/// the venue about a contract that does not exist.
pub(crate) fn wire_con_id(what: &str, con_id: i64) -> PyResult<u32> {
    crate::api::client::reference::wire_con_id(con_id, what)
        .map_err(|refusal| PyRuntimeError::new_err(refusal.message))
}

/// Check a value a caller stated the way the request surface checks it, so a
/// byte that separates fields on the wire is refused here as it is there.
pub(crate) fn wire_text(what: &str, value: &str) -> PyResult<()> {
    crate::api::client::wire_text(what, value)
        .map_err(|refusal| PyRuntimeError::new_err(refusal.message))
}

/// The client id, given under this client's spelling or the reference
/// client's. Given under both, the caller has said two things, and is told so
/// rather than having one of them picked.
fn client_id_under_either_spelling(client_id: i32, reference_spelling: Option<i32>) -> PyResult<i32> {
    match reference_spelling {
        Some(_) if client_id != 0 => Err(pyo3::exceptions::PyTypeError::new_err(
            "connect() was given the client id under both spellings, client_id and clientId",
        )),
        Some(id) => Ok(id),
        None => Ok(client_id),
    }
}

impl EClient {
    /// Keep the callable where the collector can see it. The login gate runs
    /// the adapter on its own thread, taking the GIL only to call Python.
    fn code_provider_from_py(&self, cb: Option<Py<PyAny>>) -> Option<CodeProvider> {
        let cb = cb.map(Arc::new);
        let weak = cb.as_ref().map(Arc::downgrade);
        let previous = std::mem::replace(&mut *self.code_provider.lock().unwrap(), cb);
        drop(previous);
        weak.map(|weak| Arc::new(move |challenge: IbKeyChallenge| {
            Python::attach(|py| {
                let cb = weak.upgrade().ok_or_else(|| std::io::Error::other(
                    "Authentication provider is no longer available",
                ))?.clone_ref(py);
                let factor = match challenge.factor {
                    SecondFactor::IbKeyChallengeResponse => "ibkey",
                    SecondFactor::AuthenticatorCode => "authenticator",
                };
                cb.call1(py, (factor, challenge.display_id, challenge.avth_url))
                    .and_then(|code| code.extract::<String>(py))
                    .map_err(|e| std::io::Error::other(e.to_string()))
            })
        }) as CodeProvider)
    }
}

#[pymethods]
impl EClient {

    /// Takes whatever the caller passed, `_args` and `_kwargs`, and binds
    /// nothing with them: the interpreter hands the same arguments to
    /// `__init__`, which is where the wrapper is bound — and a program built
    /// the way the reference sample is, `App()` with
    /// `EClient.__init__(self, wrapper=self)` inside, reaches this with none.
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn __new__(_args: &Bound<'_, pyo3::types::PyTuple>, _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>) -> Self {
        Self {
            client_id: AtomicI32::new(0),
            auth_host: Mutex::new(None),
            logged_in_at: Mutex::new(None),
            wrapper: RwLock::new(None),
            code_provider: Mutex::new(None),
            shared: Mutex::new(None),
            control_tx: Mutex::new(None),
            next_order_id: AtomicU64::new(0),
            _thread: Mutex::new(None),
            account_id: Mutex::new(None),
            accounts: Mutex::new(Vec::new()),
            disconnects: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            positions_requested: AtomicBool::new(false),
            deferred_evictions: Mutex::new(std::collections::HashSet::new()),
            positions_multi_requested: Mutex::new(std::collections::HashSet::new()),
            account_updates_multi_requested: Mutex::new(std::collections::HashSet::new()),
            session_ended: AtomicBool::new(false),
            close_notified: AtomicBool::new(false),
            waiting_answers: Mutex::new(std::collections::VecDeque::new()),
            event_rx: Mutex::new(None),
            events_lost: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            _test_event_tx: Mutex::new(None),
            _test_control_rx: Mutex::new(None),
            tbt_kind: Mutex::new(HashMap::new()),
            pending_option_calcs: Mutex::new(HashMap::new()),
            completed: Mutex::new(Vec::new()),
            core: ClientCore::new(),
        }
    }

    /// Bind the wrapper the callbacks are delivered to.
    ///
    /// `EClient(wrapper)` lands here through the interpreter; a subclass with a
    /// constructor of its own calls `EClient.__init__(self, wrapper)`, as the
    /// reference sample does with `wrapper=self`. Bound once: a second,
    /// different wrapper is refused, since callbacks may already be on their
    /// way to the first.
    fn __init__(&self, wrapper: Py<PyAny>) -> PyResult<()> {
        // A refused `wrapper` is dropped on the way out, after the guard: what
        // its drop runs can reach back here, and would wait on this lock.
        let mut slot = self.wrapper.write().unwrap();
        match &*slot {
            None => *slot = Some(wrapper),
            Some(bound) if bound.is(&wrapper) => {}
            Some(_) => return Err(pyo3::exceptions::PyTypeError::new_err("EClient already has a wrapper")),
        }
        Ok(())
    }

    /// Show the cyclic collector the wrapper and authentication provider,
    /// which commonly refer back to the application holding this client.
    ///
    /// Non-blocking: a collection must never wait on a lock a callback can be
    /// holding. A slot found held is skipped for this pass, which only keeps
    /// the object until a later one.
    fn __traverse__(&self, visit: pyo3::PyVisit<'_>) -> Result<(), pyo3::PyTraverseError> {
        if let Ok(slot) = self.wrapper.try_read() {
            visit.call(&*slot)?;
        }
        if let Ok(slot) = self.code_provider.try_lock()
            && let Some(provider) = slot.as_ref()
        {
            visit.call(&**provider)?;
        }
        Ok(())
    }

    /// Release the wrapper and authentication callable, breaking their cycles
    /// through the application. Later calls say the callback is unavailable.
    fn __clear__(&self) {
        // Bound first so the guard is gone before the wrapper is dropped: its
        // drop can run the wrapper's own teardown, which can reach back here.
        let wrapper = self.wrapper.write().unwrap().take();
        let provider = self.code_provider.lock().unwrap().take();
        drop(wrapper);
        drop(provider);
    }

    /// Connect to IB and start the engine.
    ///
    /// Live logins (``paper=False``) enter a second-factor approval window and
    /// **block** until the factor is approved (mobile push) or the deadline
    /// fires (``ib_key_timeout_secs``, default ~18 min). This is a human
    /// approval gate, not a hang. To bound or avoid it: use ``paper=True``, pass
    /// a smaller ``ib_key_timeout_secs``, or run ``connect()`` on a worker
    /// thread with your own timeout. Paper logins skip the gate entirely. Set
    /// ``RUST_LOG=info`` to see a log line when the wait begins.
    ///
    /// ``code_provider`` answers that factor with a typed code instead:
    /// ``code_provider(factor, display_id, avth_url) -> str``, where ``factor``
    /// is ``"ibkey"`` (return the code shown for ``display_id``) or
    /// ``"authenticator"`` (return the account's current code; ``display_id``
    /// and ``avth_url`` are empty). An authenticator account has no push to
    /// fall back to and cannot log in without this. It is called once, on a
    /// thread of its own, and holds the GIL while it runs — return the code,
    /// don't block on input. It is asked once and the login carries whatever it
    /// returns; what the venue does with a wrong code has not been exercised
    /// from here.
    ///
    /// Multiple ``EClient`` instances can run concurrently in one process; each
    /// owns its own state, sockets, and engine thread, and ``connect()`` does
    /// not serialize across instances. If you pin engines via ``core_id``, give
    /// each a distinct value.
    ///
    /// `port` is taken and not applied. The session connects to the venue
    /// directly, so there is no local socket to name a port on.
    #[pyo3(signature = (host=crate::config::CCP_HOSTS[0].to_string(), port=0, client_id=0, username="".to_string(), password="".to_string(), paper=true, core_id=None, ib_key_timeout_secs=None, ib_key_token_sub_type=None, code_provider=None, readonly=false, settings=None, session_file=None, *, clientId=None))]
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
        ib_key_timeout_secs: Option<u64>,
        ib_key_token_sub_type: Option<String>,
        code_provider: Option<Py<PyAny>>,
        readonly: bool,
        // What this session runs under, by the names `ibx.configure` uses.
        // Stated here it belongs to this session; stated there it is the
        // process's, and is what a session that states nothing falls back to.
        settings: Option<std::collections::HashMap<String, String>>,
        // Where to keep this session so the next start does not need a new
        // logon. A request naming a session the venue still holds is answered
        // with a challenge rather than a full handshake, so a restart needs no
        // approval and does not count as a new login. Owner-only, sealed with
        // the password, and bound to this account and this kind of session.
        session_file: Option<String>,
        // The reference client's spelling, so a program written against it
        // keeps its connect line. The lint allowance is on this one name: the
        // spelling is the whole of what the parameter is for.
        #[allow(non_snake_case)] clientId: Option<i32>,
    ) -> PyResult<()> {
        let client_id = client_id_under_either_spelling(client_id, clientId)?;
        let username_for_resume = username.clone();
        let username_for_session = username.clone();
        let password_for_resume = password.clone();
        // Claimed, not read. Read and then set, two callers racing here both
        // find it clear and both build an engine — and the second replaces the
        // first, which goes on running with a live socket and a second logon
        // the account did not ask for. The venue bumps the older session when
        // that happens, so a caller racing itself knocks over its own.
        if self
            .connected
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        // Given back on every way out that is not a session, including the
        // ones a later edit adds.
        let mut claim = Connecting { flag: &self.connected, kept: false };
        // Past that refusal there can still be an engine running: a
        // market-data farm that went away flips this flag while the trading
        // connection is healthy and the ladder is still climbing, and
        // answering that notice by connecting again is how callers of the
        // reference client are written. Left running, the old engine loses its
        // sender and takes the path that sends no logout and withdraws nothing
        // it opened, so a session stays open at the venue while this one runs.
        // Stopped and joined before the state is reset, so the engine that is
        // going cannot write into what the new session has been given.
        self.stop_engine(py);
        self.forget_last_session();
        // Stated before the logon rather than after it, so a caller reading it
        // during `connect` reads this session's number and not the last one's.
        self.client_id.store(client_id, Ordering::Release);

        // Set before anything is sent, so there is no window in which the
        // session is open and the refusal is not yet in force.
        self.core.set_readonly(readonly);

        let code_provider = self.code_provider_from_py(code_provider);

        let config = GatewayConfig {
            settings: std::sync::Arc::new(
                crate::python::settings_from(settings.unwrap_or_default())
                    .map_err(PyRuntimeError::new_err)?,
            ),
            username,
            password: zeroize::Zeroizing::new(password),
            host,
            paper,
            accept_invalid_certs: false,
            ib_key_timeout_secs: ib_key_timeout_secs
                .unwrap_or(crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS),
            ib_key_token_sub_type: ib_key_token_sub_type
                .unwrap_or_else(|| crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into()),
            code_provider,
            // Whatever was left in the file the caller named. A file that
            // cannot be read is a slower start, not a failed one: the password
            // is still here, and a file whose whole point is to avoid needing
            // a person defeats itself by raising at one.
            resume: session_file.as_ref().and_then(|path| {
                crate::auth::resume::load(
                    std::path::Path::new(path), &username_for_resume, &password_for_resume, paper,
                )
            }),
        };

        // Read before the GIL goes, compared after it comes back.
        let disconnects_before = self.disconnects.load(Ordering::Acquire);
        let result = py.detach(|| Gateway::connect(&config));
        // A disconnect answered for this connect while it was in the venue's
        // hands. It found nothing installed to stop and said the session
        // closed; going on to install this one would leave the caller holding
        // a client it was told had closed, in front of a logged-in venue
        // session. The session that did open is closed here instead, which is
        // what the caller asked for.
        if self.disconnects.load(Ordering::Acquire) != disconnects_before {
            // disconnect() already gave back this claim; the flag may now
            // belong to a newer connection.
            claim.kept = true;
            // Dropped rather than installed, which closes every socket it
            // opened. Detached because those closes talk to the venue.
            py.detach(move || drop(result));
            return Err(PyRuntimeError::new_err(
                "Connection abandoned: disconnect() was called while it was still logging in",
            ));
        }
        let Session { gateway: gw, market_data: farm_conn, trading: ccp_conn, historical: hmds_conn, security_definition: secdef_conn } = result
            .map_err(|e| PyRuntimeError::new_err(format!("Connection failed: {e}")))?;

        crate::client_core::remember_session(
            session_file.as_deref().map(std::path::Path::new),
            &password_for_resume,
            &gw,
            &username_for_session,
            paper,
        );

        // Here rather than on the way in: a session can end without being
        // closed, so this is reached with the last one's state still held, but
        // until the connection above answered there was still a session in
        // recovery whose routing this would have taken away from it.
        self.forget_last_session();

        *self.account_id.lock().unwrap() = Some(gw.account_id.clone());
        *self.accounts.lock().unwrap() = gw.accounts.clone();
        *self.auth_host.lock().unwrap() = gw.auth_hosts.first().cloned();
        *self.logged_in_at.lock().unwrap() =
            Some(gw.logged_in_at.clone()).filter(|stamp| !stamp.is_empty());
        let shared = Arc::new(SharedState::new());
        shared.set_settings(config.settings.clone());
        self.core.set_registration_timeout(config.settings.registration_timeout);
        gw.populate_init_data(&shared);

        let connect_host = config.host.clone();
        let connect_username = config.username.clone();
        let connect_password = config.password.clone();
        let connect_paper = config.paper;
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(256);
        let (mut hot_loop, control_tx) = crate::engine::hot_loop::HotLoop::for_session(
            gw,
            shared.clone(),
            // This session's own count of what it discarded, so a program with
            // two sessions is not told about the other one's — and held here,
            // so it can be asked.
            Some(crate::engine::hot_loop::EventSink::new(
                event_tx,
                Arc::clone(&self.events_lost),
            )),
            farm_conn, ccp_conn, hmds_conn, secdef_conn, core_id,
            crate::gateway::CallerAuth {
                // The settings the session opened under, not the defaults. Left
                // default here, a reconnect announced a different build, locale
                // and timezone than the connect did, and asked for every
                // execution the account holds where the caller asked for
                // today's. The Rust surface states the same thing in
                // `caller_auth`; this path had to state it too.
                settings: config.settings.clone(),
                host: connect_host,
                username: connect_username,
                password: connect_password,
                paper: connect_paper,
                code_provider: config.code_provider.clone(),
                ib_key_timeout_secs: config.ib_key_timeout_secs,
                ib_key_token_sub_type: config.ib_key_token_sub_type.clone(),
            },
        );
        // The recovery switch a gateway carries. Never applied here, a Python
        // session recovered on its own whatever the setting said, so a program
        // that would rather fail loudly than carry on quietly could not say so
        // on this surface at all.
        if !config.settings.reconnect_on_socket_err {
            hot_loop.set_reconnect_config(crate::reliability::ReconnectConfig::manual());
        }

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || {
                hot_loop.run_with_panic_recovery();
            })
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn hot loop: {e}")))?;

        *self.shared.lock().unwrap() = Some(shared.clone());
        *self.control_tx.lock().unwrap() = Some(control_tx);
        *self.event_rx.lock().unwrap() = Some(event_rx);
        // Counted from whatever the venue names as working, once it has;
        // nothing is carried over from the last run.
        self.next_order_id.store(0, Ordering::Relaxed);
        *self._thread.lock().unwrap() = Some(handle);
        self.session_ended.store(false, Ordering::Release);
        self.close_notified.store(false, Ordering::Release);
        // Stated again here, where the session exists, and not only where the
        // claim was taken above.
        //
        // The claim is taken before the previous engine is stopped, and
        // stopping it raises the old session's loss flag. A dispatch running
        // on another thread -- the ordinary shape, `run()` on one thread and
        // the reconnect on another -- reads that flag and stores this client
        // disconnected, seconds before the new session is installed here.
        // Nothing put it back: the new session was live with the client
        // reading disconnected, `run()` refused to start, and the next
        // reconnect tore a working session down to build another.
        self.connected.store(true, Ordering::Release);
        claim.kept = true;

        let _ = port; // kept for the reference client's signature

        self.announce_the_new_session(py, &shared)
    }

    /// Answer to the name the reference client gives a method as well as the
    /// name this one gives it.
    ///
    /// The reference client names its methods with the words run together, and
    /// code written against it calls them that way. Every method here is named
    /// with underscores, so that code stopped at its first call with the method
    /// simply not there. Rather than write a second name for each of ninety
    /// methods, the run-together name is translated back and the method this
    /// client already has is returned.
    ///
    /// Only reached when the attribute was not found, so it costs nothing on
    /// the name this client has always used.
    fn __getattr__(slf: Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        super::contract::by_reference_name(slf.as_any(), name, REFERENCE_ALIASES)
    }

    /// Disconnect from IB.
    fn disconnect(&self, py: Python<'_>) -> PyResult<()> {
        // A session that was held has ended; a client that never connected
        // has no session to be told the close of.
        let had_a_session = self.shared.lock().unwrap().is_some();
        // Counted before anything is torn down, so a connect still inside its
        // logon sees this one on the way out however the two interleave.
        self.disconnects.fetch_add(1, Ordering::AcqRel);
        self.stop_engine(py);
        let provider = self.code_provider.lock().unwrap().take();
        drop(provider);
        self.connected.store(false, Ordering::Release);
        if had_a_session {
            self.session_ended.store(true, Ordering::Release);
        }
        // Reset per-session state so connect() can be called again.
        *self.shared.lock().unwrap() = None;
        *self.control_tx.lock().unwrap() = None;
        *self.event_rx.lock().unwrap() = None;
        self.forget_last_session();
        Ok(())
    }

    /// Whether there is a session to make requests on.
    ///
    /// False before `connect` and after `disconnect`, and false from the
    /// moment the engine gives the session up — which it writes down itself.
    /// The notice saying so goes out on a channel that drops what it cannot
    /// hold, and a program that drives its own loop, or none at all, is told
    /// nowhere else: it read connected on a session that was over, and went
    /// on issuing requests into it. The Rust surface answers this the same
    /// way.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) && !self.the_engine_gave_the_session_up()
    }

    // ── What the reference client holds after connecting ──
    //
    // Each answers what this client has. Where the reference client's value is
    // its gateway's — the process it speaks to, of which there is none here,
    // the session being opened against the venue itself — the answer is `None`
    // and the doc says why. Not a stand-in: a number nothing stated is worse
    // than a clear absence, because a program decides on it.

    /// No session: before `connect`, after `disconnect`, and after a loss.
    #[classattr]
    const DISCONNECTED: i32 = 0;
    /// The length of `connect`: the connection is claimed before the logon and
    /// the session's state handed over after it.
    #[classattr]
    const CONNECTING: i32 = 1;
    /// A session, with its engine running.
    #[classattr]
    const CONNECTED: i32 = 2;

    /// Which of `DISCONNECTED`, `CONNECTING` and `CONNECTED` this client is in.
    ///
    /// `CONNECTING` is the span of `connect`, as read from another thread: the
    /// connection is claimed at the top of that call and the session's state
    /// handed over at the bottom, and `is_connected` reads true for the whole
    /// of it. A session that ended without being closed is connecting again
    /// from the moment `connect` is called on it, though what it held stays in
    /// place until the logon answers.
    ///
    /// A session the engine has given up is `DISCONNECTED` from the moment it
    /// writes that down, however the caller drives its loop — read from the
    /// cached flags alone it stayed `CONNECTED` for as long as the program
    /// went without dispatching.
    #[getter]
    fn conn_state(&self) -> i32 {
        if !self.connected.load(Ordering::Acquire) || self.the_engine_gave_the_session_up() {
            Self::DISCONNECTED
        } else if self.session_ended.load(Ordering::Acquire)
            || self.shared.lock().unwrap().is_none()
        {
            Self::CONNECTING
        } else {
            Self::CONNECTED
        }
    }

    /// The number this session connected under, as the caller gave it, and
    /// `None` when there is no session.
    #[getter]
    fn client_id(&self) -> Option<i32> {
        if self.is_connected() { Some(self.client_id.load(Ordering::Acquire)) } else { None }
    }

    /// The server this session connected to, and `None` when there is no
    /// session.
    ///
    /// The one it knocked on, which is the one the caller named — the venue
    /// may then send it elsewhere, and the rest of that list is where. The
    /// reference client holds what its caller passed here too.
    #[getter]
    fn host(&self) -> Option<String> {
        if self.is_connected() { self.auth_host.lock().unwrap().clone() } else { None }
    }

    /// `None`: there is no port. The reference client's is the one its gateway
    /// listens on for it. This session speaks to the venue's servers on the
    /// ports the venue names, one per connection, and none of them is a number
    /// the caller gave — `connect` takes one and does not apply it.
    #[getter]
    fn port(&self) -> Option<i32> {
        None
    }

    /// `None`: there is no socket to hold. The reference client keeps the one
    /// to its gateway here and shares it with its reader thread. This client's
    /// connections are opened, read and kept alive inside its engine, and a
    /// caller has no hand on any of them.
    #[getter]
    fn conn(&self) -> Option<Py<PyAny>> {
        None
    }

    /// `False`: there is no asynchronous mode. The reference client sets this
    /// nowhere either; its sample program reads it in `connect_ack` to decide
    /// whether to start the exchange itself. Here `connect` returns with the
    /// session up and its engine running, and `connect_ack` is announced from
    /// inside it, so there is nothing left for a caller to start.
    #[getter]
    fn asynchronous(&self) -> bool {
        false
    }

    /// The protocol level this client implements: 217, the reference client's
    /// `MIN_SERVER_VER_ADDITIONAL_ORDER_PARAMS_2`. `None` before a session, as
    /// the reference client answers before its greeting.
    ///
    /// In the reference architecture this number is the API level of the
    /// process a program is talking to. That process was the vendor's gateway,
    /// which announced it and had every request gated on it; here it is this
    /// client, so the number is a statement about this client and not a
    /// reading off the venue, whose logon names no such level.
    ///
    /// 217 is the newest gate whose feature is carried here. Nothing above it
    /// is: attached orders (218), the configuration requests (219, 221),
    /// `hedgeMaxSize` (223) and `conditionsIncludeOvernight` (226) are absent.
    /// Below it, a program that believes the number is wrong about the
    /// following, and every one fails loudly on use rather than quietly:
    ///
    /// * Order fields this protocol does not carry, refused by name on `error`
    ///   under 321 when the order is placed: `optOutSmartRouting` (56),
    ///   `smartComboRoutingParams` (57), the delta-neutral settling, clearing
    ///   and open/close fields (58, 66), `scaleInitFillQty` (60), `scaleTable`
    ///   (69), `orderMiscOptions` (70), `algoId` (71), `randomizePrice` (76),
    ///   `dontUseAutoPriceForHedge` (141), `whatIfType` (217).
    /// * Requests and fields that do not exist here, an `AttributeError`: the
    ///   four `verify*` calls (70), `cancelContractData` and
    ///   `cancelHistoricalTicks` (215).
    /// * A withdrawal stating a manual time, an operator or who entered it
    ///   (169, 192), and an execution filter stating `lastNDays` or
    ///   `specificDates` (200): refused by name on `error`.
    /// * One callback nothing fires, said on the call that would produce it:
    ///   `orderBound` (144) after `reqAutoOpenOrders`. It is the one case that
    ///   is quiet at run time: a program that waits on it waits, and only the
    ///   doc says why.
    ///
    /// Every other gate at or below 217 names a request, field or callback
    /// that is here and carried.
    fn server_version(&self) -> Option<i32> {
        self.is_connected().then_some(crate::client_core::PROTOCOL_LEVEL)
    }

    /// When the venue says this session logged in, by its own clock and in its
    /// own spelling; `None` when there is no session.
    ///
    /// The reference client answers the time its gateway stamped on its
    /// greeting. The venue stamps every message it sends with the time it sent
    /// it, the answer to the logon included, and this is that stamp — the
    /// clock `competing_session` reads the other session's logon off. Where
    /// the venue stamped none, `connect` holds this machine's clock instead
    /// and says so in the log.
    fn tws_connection_time(&self) -> Option<String> {
        if self.is_connected() { self.logged_in_at.lock().unwrap().clone() } else { None }
    }

    /// The reference client carries these on its greeting to its gateway,
    /// which reads them; there is no gateway between this client and the venue
    /// to read them, so an option a caller states cannot be carried.
    ///
    /// Said rather than swallowed. Every one of these options changes how the
    /// session behaves — how fast it may ask, what it is told — and a caller
    /// who set one and heard nothing has a session that is not the one they
    /// asked for and no way to know it. Stating none is stating nothing, which
    /// is what the reference client's own is on an ordinary call.
    fn set_connect_options(&self, py: Python<'_>, opts: &str) -> PyResult<()> {
        if opts.is_empty() {
            return Ok(());
        }
        self.report_refusal(py, -1, crate::error_codes::Refusal::validation(format!(
            "connect options {opts:?} were stated and cannot be carried: the reference \
             client hands these to a gateway to read, and there is no gateway between \
             this client and the venue",
        )))
    }

    /// Nothing to start, once there is a session. The reference client sends
    /// its client id here and its gateway begins the exchange on receiving it.
    /// Here the id is kept on the client and never sent — one session holds
    /// the account — and `connect` announces `connect_ack`, the accounts and
    /// the next order id itself before it returns. Before a session exists
    /// this is reported the way the reference client reports it: on `error`,
    /// under 504.
    fn start_api(&self) -> PyResult<()> {
        self.tx_or_report(-1)?;
        Ok(())
    }

    /// Raises when there is a session, as the reference client's does, with
    /// the message `connect` refuses a second call under. Nothing otherwise.
    fn check_connected(&self) -> PyResult<()> {
        if self.is_connected() {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        Ok(())
    }

    /// Forget the session — which here is closing it. The reference client
    /// drops its hold on the socket and leaves the socket to its reader
    /// thread; a session here is an engine that stays logged in at the venue
    /// until it is stopped, so this is `disconnect`. Nothing to forget is
    /// fine.
    fn reset(&self, py: Python<'_>) -> PyResult<()> {
        self.disconnect(py)
    }

    /// How many engine events this session's channel discarded.
    ///
    /// The engine never waits on a reader — a session that stalled on one would
    /// stop carrying market data — so an event arriving at a full channel is
    /// dropped. A program that acted on every fill it saw needs to know the
    /// difference between that and every fill there was. Zero for a session
    /// whose reader kept up.
    fn events_lost(&self) -> u64 {
        self.events_lost.load(Ordering::Acquire)
    }

    /// Run the event loop.
    /// Deliver everything waiting, once, and return.
    ///
    /// `run` owns the thread it is called on, which a program with an event
    /// loop of its own cannot give it: an asyncio framework has to drive the
    /// callbacks from its own loop, and a blocking loop leaves it nowhere to
    /// stand. This is one pass of the same dispatch.
    fn poll(&self, py: Python<'_>) -> PyResult<()> {
        // Not gated on the connection. A session the engine is still
        // rebuilding is disconnected and not over, and the event saying it
        // came back arrives on this same pump — so a pump that stopped at the
        // loss could never deliver it, and the caller stayed stood down on a
        // session that had recovered.
        // A pass an interrupt ended is over, as it is in `run`: no more of the
        // caller's code runs on it, and the close is said on the next pass.
        // Said whether or not a session stands: `disconnect()` takes the
        // session away, and a program driving its own loop is told the close
        // on the pass after, as the reference client tells it.
        // The session taken out from under its lock before the pass runs:
        // the pass locks the same mutex to check the session is still the
        // client's, and a guard kept alive across it deadlocked every poll.
        let held = self.shared.lock().unwrap().clone();
        if let Some(shared) = held {
            self.dispatch_once(py, &shared)?;
        }
        self.tell_the_caller_it_closed(py)
    }

    /// Deliver callbacks until the session ends.
    ///
    /// Blocks the calling thread. Everything a program receives arrives from
    /// here, so it runs on a thread of its own or is the last call a program
    /// makes. `poll` does one pass instead, for a program that owns its loop.
    fn run(&self, py: Python<'_>) -> PyResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Not connected. Call connect() first."));
        }

        // Event loop — wake immediately on data, or check signals every 1ms.
        //
        // Ends when the session does, not when it goes quiet. The engine keeps
        // rebuilding a lost connection past the 1100 notice and announces 1102
        // when the transports carry again, so the loop must outlive 1100 to
        // deliver the recovery.
        while !self.session_ended.load(Ordering::Relaxed) {
            py.check_signals()?;

            let shared = match self.shared.lock().unwrap().clone() {
                Some(s) => s,
                None => break,
            };

            self.dispatch_once(py, &shared)?;

            // Wait for hot loop notification instead of fixed sleep.
            // Releases GIL while waiting; wakes immediately when data arrives.
            let shared_ref = shared.clone();
            py.detach(move || {
                shared_ref.wait_for_data(std::time::Duration::from_millis(1));
            });
        }

        // The same place `poll` says it from, so a caller driving this a pass
        // at a time hears it too, and neither hears it twice.
        self.tell_the_caller_it_closed(py)
    }

    /// Get the account ID.
    fn get_account_id(&self) -> String {
        self.account()
    }

    /// Another session that already held this account when this one connected.
    ///
    /// `None` when this session is alone. Otherwise where the other one
    /// connected from, when it logged in, and whether this session is held to
    /// reading only because the other has the account.
    ///
    /// Worth asking before starting work: the venue permits one logon at a time
    /// and takes the account from the older session without saying which it
    /// dropped, so a second client reads as data that stops arriving.
    fn competing_session(&self) -> PyResult<Option<(String, String, bool)>> {
        Ok(self.shared_state()?.reference.competing_session())
    }

    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    fn ccp_session_id(&self) -> PyResult<String> {
        Ok(self.shared_state()?.reference.ccp_session_id())
    }

    /// Logical-name → host URL lookup from the MiscUrls block of the logon
    /// response (e.g. `region_dam`). `None` when the logon did not carry the
    /// key.
    fn misc_url(&self, key: &str) -> PyResult<Option<String>> {
        Ok(self.shared_state()?.reference.misc_url(key))
    }
}

/// The names the reference client opens with an `e`, and what they reach here.
/// Its `eConnect` takes a host, a port and a client id, which are `connect`'s
/// first three arguments in that order, so a program written against it
/// reaches the same call the same way.
pub(super) const REFERENCE_ALIASES: &[(&str, &str)] =
    &[("eConnect", "connect"), ("eDisconnect", "disconnect")];

/// Call a callback on a wrapper under the reference client's name for it where
/// the caller defined one, and under this client's name otherwise. A client
/// whose `__init__` was never reached has no wrapper, nor has one the collector
/// has cleared, and either says so.
pub(crate) fn call_named<'py, A>(
    py: Python<'py>,
    wrapper: &RwLock<Option<Py<PyAny>>>,
    name: &str,
    args: A,
) -> PyResult<()>
where
    A: pyo3::call::PyCallArgs<'py> + Clone,
{
    // A reference of its own, and the lock let go before anything is called:
    // the call runs the caller's code, which can reach `__init__`, and the
    // collector, which can reach `__clear__`, and both write under this lock.
    let wrapper = wrapper.read().unwrap().as_ref().map(|bound| bound.clone_ref(py));
    let Some(wrapper) = wrapper else {
        return Err(PyRuntimeError::new_err(
            "EClient has no wrapper: EClient.__init__ was not called, or the client is being collected",
        ));
    };
    let alias = ibapi_name(name);
    if alias != name
        && let Ok(f) = wrapper.getattr(py, alias.as_str())
        && !answered_by_the_base(py, &f, &alias)
    {
        f.call1(py, args.clone())?;
        return Ok(());
    }
    wrapper.call_method1(py, name, args)?;
    Ok(())
}

/// Whether what a wrapper answered under the reference client's name is the
/// base class's own answer rather than the caller's.
///
/// The base answers to that name too, so that `super().tickPrice(...)` finds
/// something — `super` reads each class's own contents and asks no hook, so the
/// name has to be there. That answer also stands in front of a caller who
/// overrode only this client's spelling, and taking it would report the
/// callback delivered when nobody received it.
///
/// Known by `__func__`, which the base answers with a Python function to carry
/// and which the do-nothing's own bound form has none of — that missing link is
/// what an earlier guard here tried to read and never could. Anything else a
/// wrapper answers with, its own method or something put on the instance, is
/// the caller's.
fn answered_by_the_base(py: Python<'_>, f: &Py<PyAny>, alias: &str) -> bool {
    let Ok(func) = f.getattr(py, pyo3::intern!(py, "__func__")) else {
        return false;
    };
    py.get_type::<super::wrapper::EWrapper>()
        .getattr(alias)
        .is_ok_and(|base| func.is(&base))
}

/// The reference client's spelling of a callback name: its words run together
/// with each after the first capitalised.
fn ibapi_name(snake: &str) -> String {
    // Three it spells with its letters run together instead. Built the ordinary
    // way `real_time_bar` is `realTimeBar`, which that client does not declare,
    // so a wrapper written against it declares `realtimeBar`, nothing answers to
    // the name built here, and the bar goes to this crate's own do-nothing
    // rather than to the caller. Nothing is raised and nothing is logged.
    match snake {
        "real_time_bar" => return "realtimeBar".to_string(),
        "receive_fa" => return "receiveFA".to_string(),
        "replace_fa_end" => return "replaceFAEnd".to_string(),
        _ => {}
    }
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}


/// Holds the connected flag for the length of a connect that has not finished.
///
/// The refusal and the flag it reads are one step, so the flag is set before
/// the session exists. Anything short of a session gives it back.
struct Connecting<'a> {
    flag: &'a AtomicBool,
    kept: bool,
}

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        if !self.kept {
            self.flag.store(false, Ordering::Release);
        }
    }
}

impl EClient {
    /// Stop the engine this client is running and wait for it.
    ///
    /// Both taken out of their locks first, as in `Drop`: the sends are bounded
    /// and the join waits on the engine, so a guard spanning either blocks
    /// every other thread that needs the same lock. Detached for both — the
    /// channel is bounded and a wedged engine never returns from a join, so
    /// holding the GIL across either stalls every Python thread rather than
    /// this call alone.
    fn stop_engine(&self, py: Python<'_>) {
        let tx = self.control_tx.lock().unwrap().take();
        if let Some(tx) = tx {
            // The session is ending, so the venue is told before the engine
            // stops. Left to notice its sender went away, the loop takes the
            // path that sends no logout and withdraws nothing it opened.
            py.detach(|| {
                let _ = tx.send(ControlCommand::Logout);
                let _ = tx.send(ControlCommand::Shutdown);
            });
        }
        let thread = self._thread.lock().unwrap().take();
        if let Some(h) = thread {
            py.detach(|| {
                let _ = h.join();
            });
        }
    }

    /// Drop everything the last session held.
    ///
    /// Called both where a session is closed and where the next one is opened,
    /// because a session can end without being closed and what it held would
    /// otherwise be answered under request ids the next session never gave
    /// out: a model arriving for a contract nobody here watches, positions
    /// nobody here asked for, another account's completed orders, an eviction
    /// aimed at an order record this session now owns.
    fn forget_last_session(&self) {
        *self.account_id.lock().unwrap() = None;
        self.accounts.lock().unwrap().clear();
        *self.auth_host.lock().unwrap() = None;
        *self.logged_in_at.lock().unwrap() = None;
        self.positions_requested.store(false, Ordering::Release);
        self.positions_multi_requested.lock().unwrap().clear();
        // And the account-figure watchers beside them. Left standing, a second
        // session on the same client delivers its own figures under a request
        // number the caller made on a session that has gone.
        self.account_updates_multi_requested.lock().unwrap().clear();
        self.deferred_evictions.lock().unwrap().clear();
        self.tbt_kind.lock().unwrap().clear();
        self.pending_option_calcs.lock().unwrap().clear();
        self.completed.lock().unwrap().clear();
        // Counted per session, so a caller reading it is told what this
        // session lost rather than a total carried over from the last.
        self.events_lost.store(0, Ordering::Relaxed);
        // Answers owed to the session that ended. Left queued, the next
        // session's first pass would hand over another session's positions and
        // orders under request numbers nobody there gave out.
        self.waiting_answers.lock().unwrap().clear();
        self.core.reset();
    }

    /// Tell the caller a request will not be sent, under the number the
    /// reference client reports that class of refusal under.
    ///
    /// Reported rather than raised: the reference client answers a request it
    /// refuses on the error callback and returns, so a program written against
    /// it handles refusals there and nowhere else. What that handler raises is
    /// answered for as any callback's is: logged if ordinary, and otherwise
    /// what the call raises.
    pub(crate) fn report_refusal(
        &self,
        py: Python<'_>,
        req_id: i64,
        refusal: crate::error_codes::Refusal,
    ) -> PyResult<()> {
        self.notify(py, "error", (req_id, raised_now(), refusal.code, refusal.message, ""))
    }

    /// Whether the engine has given this session up for good.
    ///
    /// Read from the session's own state rather than from the flags this
    /// client caches: those are written by the dispatch pass, and the engine
    /// records why it stopped the moment it stops. A caller that has not
    /// dispatched since is holding flags from before the session ended.
    pub(crate) fn the_engine_gave_the_session_up(&self) -> bool {
        self.shared
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|shared| shared.reference.session_over().is_some())
    }

    /// Whether the engine has given the trading connection up for good.
    ///
    /// Its own state, apart from the session's: a quote feed the venue will
    /// not serve again does not end the connection orders travel on.
    pub(crate) fn the_engine_gave_the_trading_connection_up(&self) -> bool {
        self.shared
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|shared| shared.reference.trading_over().is_some())
    }

    /// The control channel, or nothing and the caller told why.
    ///
    /// A request issued before connecting is answered on the error callback
    /// and the call returns normally, which is what the reference client does.
    /// Raising instead made a caller written against that client take a
    /// different path here than it takes there. What the handler raises is
    /// logged if ordinary, as `notify` logs it, and otherwise ends the call.
    pub(crate) fn tx_or_report(&self, req_id: i64) -> PyResult<Option<SyncSender<ControlCommand>>> {
        self.tx_unless_it_is_over(req_id, self.the_engine_gave_the_session_up())
    }

    /// The channel an order goes down, or nothing and the caller told why.
    ///
    /// Read from the trading connection's own state rather than the session's,
    /// which is what the other surface reads before it takes an order. The
    /// session flag is raised by any transport being given up on, the quote
    /// feed among them, and an order refused on that reading is one the live
    /// trading connection would have carried — a caller could not withdraw a
    /// working order because the prices had stopped.
    pub(crate) fn tx_or_report_for_trading(
        &self,
        req_id: i64,
    ) -> PyResult<Option<SyncSender<ControlCommand>>> {
        self.tx_unless_it_is_over(req_id, self.the_engine_gave_the_trading_connection_up())
    }

    fn tx_unless_it_is_over(
        &self,
        req_id: i64,
        over: bool,
    ) -> PyResult<Option<SyncSender<ControlCommand>>> {
        // Taken before the arms run. The `None` arm calls user code, and a
        // handler that disconnects or issues another request would wait on
        // this same lock while holding the GIL.
        let tx = self.control_tx.lock().unwrap().clone();
        // A sender outlives the session it was made for: the engine gives the
        // session up and stops, and this end still holds the channel. Queued
        // into that, a request is neither sent nor refused, and the caller
        // waits out its own timeout for an answer nobody is going to make.
        // Refused under the number a request made with no session is refused
        // under, which is what this has become.
        let tx = tx.filter(|_| !over);
        match tx {
            Some(tx) => Ok(Some(tx)),
            None => {
                // What the handler raised is handed back rather than logged
                // and dropped: an ordinary exception is the caller's own
                // business and is logged there, but an interrupt has to end
                // the call it was raised in. Left set instead of returned, it
                // comes out as a `SystemError` at whatever the interpreter
                // calls next, nowhere near the handler.
                Python::attach(|py| {
                    self.notify(py, "error", (req_id, raised_now(), NOT_CONNECTED_CODE, "Not connected", ""))
                })?;
                Ok(None)
            }
        }
    }

    pub(crate) fn tx(&self) -> PyResult<SyncSender<ControlCommand>> {
        self.control_tx.lock().unwrap().clone()
            .ok_or_else(|| PyRuntimeError::new_err("Not connected"))
    }

    /// Hold until the venue has named what the account already has, and say
    /// whether it did.
    ///
    /// The working orders and the executions behind the rest arrive unprompted
    /// after a connect, and both raise the mark an id is floored at. Asked
    /// before they land, the floor is nothing and the id handed out is one a
    /// fill spent long ago, which the venue refuses as a duplicate — so the
    /// first order of a session is the one that cannot be placed. Bounded, and
    /// paid once per connection, because an account with nothing working never
    /// sees the replay end. The interpreter is released for the wait. Answers
    /// `false` where the bound ran out first, or where no session has come up
    /// to name anything.
    pub(crate) fn wait_for_the_replay(&self, py: Python<'_>) -> bool {
        let Ok(shared) = self.shared_state() else { return false };
        py.detach(|| shared.orders.wait_for_replay())
    }

    /// The id a caller may next place under, without taking it.
    ///
    /// One past the highest id the venue has named an order under. Stated
    /// rather than reserved, as the reference client states it: the caller
    /// places under it, and the taking happens then.
    ///
    /// An id is spent for good once a fill has spent it — only a withdrawn one
    /// comes free. The venue names the live orders at connect and replays the
    /// executions behind the rest, and both raise the mark, so what this
    /// answers is safe once that has arrived. Asked before it has, it answers
    /// from a mark of nothing and names an id a fill spent long ago, which the
    /// venue refuses as a duplicate. Seen on a session that connected and
    /// asked in the same breath.
    pub(crate) fn stated_order_id(&self) -> u64 {
        let floor = self.shared.lock().unwrap().as_ref()
            .map(|shared| shared.orders.working_id_watermark().saturating_add(1))
            .unwrap_or(1);
        // Held under the end of the range the venue's reports can name back.
        // An id past it names an order this client could never reconcile
        // against the answers to it, and the counter that carried it handed
        // every caller behind it a number that reads as negative.
        let id = self.next_order_id.load(Ordering::Acquire)
            .max(floor)
            .min(crate::bridge::MAX_ORDER_ID);
        crate::bridge::say_if_past_a_request_id(id);
        id
    }

    /// Hand out the next order id.
    ///
    /// Floored at one past the highest id the venue has named an order under,
    /// from any session. Nothing is kept between runs, because there is
    /// nothing a run knows that the next one will not be told — but it is told
    /// after connecting rather than during it, so an id asked for in the same
    /// breath as the connection is floored at nothing. See `stated_order_id`.
    pub(crate) fn take_order_id(&self, py: Python<'_>) -> u64 {
        self.wait_for_the_replay(py);
        // Past the highest the venue has named, uncapped. Taken from the
        // number this client *states* instead, which is clamped so a caller is
        // never told an id the wire cannot carry, the clamp handed back the
        // very id the venue is working when the account reaches that end —
        // where the other surface refuses. The refusal below is what states
        // there is none left.
        let floor = self
            .shared_state()
            .map(|shared| shared.orders.working_id_watermark().saturating_add(1))
            .unwrap_or(1);
        let mut held = self.next_order_id.load(Ordering::Acquire);
        loop {
            let id = held.max(floor);
            // Zero where the account has no id left the venue's reports can
            // name back, which every placement path refuses: an id carries no
            // reason with it, so the reason is logged and the number says
            // there is none. Taken unchecked, the counter went past the end of
            // the signed range and the ids behind it read as negative — which
            // the paths that carry one unsigned turned into an order number
            // above nine quintillion.
            if id > crate::bridge::MAX_ORDER_ID {
                let why = format!(
                    "this account has no order id left: the ids in use reach {id}, and an \
                     order above {} cannot be named back by the venue's own reports",
                    crate::bridge::MAX_ORDER_ID,
                );
                log::error!("{why}");
                // And on the channel a caller already watches. Told only in
                // the log, a caller reads a zero and has nowhere to learn why.
                if let Ok(shared) = self.shared_state() {
                    shared.reference.push_historical_error(
                        crate::bridge::ReferenceState::NO_REQUEST,
                        crate::error_codes::Refusal::VALIDATION,
                        why,
                    );
                }
                return 0;
            }
            match self.next_order_id.compare_exchange_weak(
                held, id + 1, Ordering::AcqRel, Ordering::Acquire,
            ) {
                // Against what is handed out, not against the floor it was
                // taken from: the counter can already be past the floor, and
                // two callers racing here take ids either side of the line.
                Ok(_) => {
                    crate::bridge::say_if_past_a_request_id(id);
                    return id;
                }
                Err(seen) => held = seen,
            }
        }
    }

    /// Send a control command to the engine. `control_tx` is a sync_channel(64)
    /// channel: a full queue is normal backpressure (the hot loop is behind,
    /// not gone) and `send` is meant to wait for it to drain, so the send
    /// itself stays blocking. What must not happen is waiting with the GIL
    /// held, stalling every Python thread instead of just this call
 ///, so the wait runs detached, and only the actual send
    /// crosses that boundary; `cmd` must already be a plain owned value by
    /// the time it's built (never touching Python state once detached).
    pub(crate) fn send_control(py: Python<'_>, tx: &SyncSender<ControlCommand>, cmd: ControlCommand) -> PyResult<()> {
        // The error carries the command back, which is the whole command by
        // value. Nothing here wants it returned, so it is described and dropped
        // while still detached rather than moved across the boundary.
        py.detach(|| tx.send(cmd).map_err(|e| e.to_string()))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {e}")))
    }

    /// Announce the session that has just opened: the socket, the accounts the
    /// venue named, and the id to number orders from.
    fn announce_the_new_session(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        // Fire initial callbacks synchronously, matching official Python ibapi
        // where connect_ack signals "socket ready" before run() is called.
        // Announcements, not permission. The session is up and its engine is
        // running; a handler that raises here — starting work in
        // `next_valid_id` is the ordinary way to write one — must not make
        // this report failure on a session that is live. Only an interrupt
        // ends this call, as it ends any.
        //
        // And each is read against the session it was announced for. A handler
        // is a caller's own code and may close the session from inside one —
        // `connect_ack` calling `disconnect` is the ordinary way to give up on
        // a connection — after which the rest of these announce a session that
        // has been torn down, and a handler that reconnects has the startup of
        // the session it just opened repeated over it. The dispatch surface
        // reads the same identity for the same reason.
        self.notify(py, "connect_ack", ())?;
        if !self.is_current_session(shared) { return Ok(()); }
        self.notify(py, "managed_accounts", (self.accounts_csv().as_str(),))?;
        if !self.is_current_session(shared) { return Ok(()); }
        // The id is announced once and a program starts numbering from it, so
        // it is worth the wait: the venue names what the account has used just
        // after the connection is made, and announced before that lands this
        // is one a fill spent long ago. A program that trusts the announcement
        // — which is the ordinary way to write one — then has its first order
        // refused as a duplicate, and nothing about the refusal points here.
        self.wait_for_the_replay(py);
        if !self.is_current_session(shared) { return Ok(()); }
        self.notify(py, "next_valid_id", (self.stated_order_id() as i64,))
    }

    /// A callback can replace the session. Its old pass ends before reading
    /// more data or routing from the session the callback installed.
    fn is_current_session(&self, shared: &Arc<SharedState>) -> bool {
        self.shared.lock().unwrap().as_ref().is_some_and(|held| Arc::ptr_eq(held, shared))
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

    /// Call a callback on the caller's wrapper, under the name the reference
    /// client gives it as well as the name this one does.
    ///
    /// The two clients spell callback names differently. A wrapper written
    /// against the reference client defines only that client's names, and a
    /// call under this client's names reaches the base-class no-op instead.
    ///
    /// The reference name is tried first and this client's name second, so a
    /// wrapper written against either is reached.
    pub(crate) fn callback<'py, A>(
        &self,
        py: Python<'py>,
        name: &str,
        args: A,
    ) -> PyResult<()>
    where
        A: pyo3::call::PyCallArgs<'py> + Clone,
    {
        call_named(py, &self.wrapper, name, args)
    }

    /// Say the session closed, once, however the caller drives the dispatch.
    ///
    /// Not a method on the Python object: it is this client telling the
    /// caller, not something the caller calls.
    fn tell_the_caller_it_closed(&self, py: Python<'_>) -> PyResult<()> {
        if self.session_ended.load(Ordering::Acquire)
            && !self.close_notified.swap(true, Ordering::AcqRel)
        {
            return self.notify(py, "connection_closed", ());
        }
        Ok(())
    }

    /// Hand a caller one of the answers to a call it made, and let the rest of
    /// the answer through whatever it does with it.
    ///
    /// The dispatch loop already treats a raising callback this way: what the
    /// caller wrote is the caller's problem, and the batch behind it is still
    /// owed. Delivered with `?` instead, one raising callback abandoned the
    /// answers behind it and the end-of-answer that closes it — so a program
    /// waiting to be told the answer was complete waited for good, and the
    /// exception came back out of a request call, which is somewhere the
    /// reference client never raises one.
    ///
    /// Not an ordinary exception — an interrupt, the interpreter going down —
    /// still ends the call, because nothing here can carry on through that.
    pub(crate) fn deliver<'py, A>(
        &self,
        py: Python<'py>,
        name: &'static str,
        args: A,
    ) -> PyResult<()>
    where
        A: IntoPyObject<'py, Target = pyo3::types::PyTuple, Output = Bound<'py, pyo3::types::PyTuple>>,
        PyErr: From<A::Error>,
    {
        self.waiting_answers
            .lock()
            .unwrap()
            .push_back((name, args.into_pyobject(py)?.unbind()));
        Ok(())
    }

    /// Hand over everything a request answered, oldest first.
    ///
    /// A callback that raises is logged and the rest still go, which is how the
    /// dispatch loop already treats one: what the caller wrote is the caller's
    /// problem and the answers behind it are still owed. An interrupt ends the
    /// pass, because nothing can carry on through one — what is left stays
    /// queued for the pass after it.
    pub(crate) fn hand_over_what_is_waiting(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared.lock().unwrap().clone();
        // Answers requested by a callback wait for the next pass, so a caller
        // asking again cannot keep engine events behind this phase forever.
        let waiting = self.waiting_answers.lock().unwrap().len();
        for _ in 0..waiting {
            // Taken out before it is handed over: the callback is the caller's
            // code and may issue another request, which queues behind what is
            // left rather than being lost or handed over twice.
            let Some((name, args)) = self.waiting_answers.lock().unwrap().pop_front() else {
                return Ok(());
            };
            self.notify(py, name, args.bind(py).clone())?;
            if shared.as_ref().is_some_and(|shared| !self.is_current_session(shared)) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Tell the caller something, and do not let what it raises decide the
    /// call that is telling it.
    ///
    /// For the notices a session sends on its way up, the answers a request
    /// owes, and the refusals it reports. They are announcements, not
    /// permission: the session is already open and its engine already running
    /// by the time they go out, so a handler that raises must not make opening
    /// the session report failure. A caller told that would not close what it
    /// believes it never opened, and would find the next attempt refused for
    /// being connected already. An ordinary exception is logged, and the
    /// dispatch loop holds the same rule for the same reason, so one raising
    /// handler cannot take the loop down with it.
    ///
    /// Anything else — an interrupt, the interpreter going down — is handed
    /// back for the call to raise, because nothing here can carry on through
    /// that. Handed back, not left set: a call that returns normally with an
    /// exception set is reported by the interpreter as a `SystemError` at
    /// whatever call comes next, which is nowhere near the handler.
    pub(crate) fn notify<'py, A>(&self, py: Python<'py>, name: &str, args: A) -> PyResult<()>
    where
        A: pyo3::call::PyCallArgs<'py> + Clone,
    {
        match self.callback(py, name, args) {
            Err(e) if e.is_instance_of::<pyo3::exceptions::PyException>(py) => {
                log::error!("Python callback {name}() raised: {e}");
                Ok(())
            }
            other => other,
        }
    }

    /// Every account this login holds, comma separated, which is the shape
    /// the reference client answers `managed_accounts` in. Falls back to the
    /// default account so a client built by hand still answers something.
    pub(crate) fn accounts_csv(&self) -> String {
        let accounts = self.accounts.lock().unwrap();
        if accounts.is_empty() { self.account() } else { accounts.join(",") }
    }

    /// Find instrument ID for a contract, registering if needed. The hot
    /// loop can take up to `REGISTRATION_TIMEOUT` to reply, so the round
    /// trip runs with the GIL released: otherwise a slow reply stalls every
    /// Python thread, not just this call.
    ///
    /// Answers a refusal rather than raising one: a session that went away
    /// mid-request and a wait that ran out are both refusals, and each
    /// surface states its own way of reporting them.
    pub(crate) fn find_or_register_instrument(
        &self, py: Python<'_>, contract: &Contract,
    ) -> Result<u32, crate::error_codes::Refusal> {
        let Some(tx) = self.control_tx.lock().unwrap().clone() else {
            return Err(crate::error_codes::Refusal::not_connected("Not connected"));
        };
        let shared = self.shared_state()
            .map_err(|_| crate::error_codes::Refusal::not_connected("Not connected"))?;
        let con_id = contract.con_id;
        let symbol = contract.symbol.clone();
        let exchange = contract.exchange.clone();
        let sec_type = contract.sec_type.clone();
        let identity = crate::types::model::contract_identity(
            &contract.last_trade_date_or_contract_month, contract.strike,
            &contract.right, &contract.multiplier, &contract.currency,
        );
        py.detach(|| self.core.find_or_register_instrument(
            &shared, &tx, con_id, &symbol, &exchange, &sec_type, &identity,
        ))
    }
}

/// Register EClient on the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EClient>()?;
    Ok(())
}

/// When this client raised something itself, in milliseconds.
///
/// The reference client stamps the trouble it raises before anything reaches
/// the venue — a call made with no session, a request it will not send — and
/// leaves the field at zero for trouble the venue stated.
pub(crate) fn raised_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::Python;

    /// A callback reaches the caller under the name the reference client gives
    /// it, including the three that client spells with its letters run together.
    ///
    /// Built by capitalising after each underscore, `real_time_bar` is
    /// `realTimeBar`. That client declares `realtimeBar`, so a wrapper written
    /// against it declares `realtimeBar` too, and a bar sent to the name built
    /// here reaches nobody — no exception, no log line, no bar.
    #[test]
    fn a_callback_is_named_the_way_the_reference_client_names_it() {
        assert_eq!(ibapi_name("real_time_bar"), "realtimeBar");
        assert_eq!(ibapi_name("receive_fa"), "receiveFA");
        assert_eq!(ibapi_name("replace_fa_end"), "replaceFAEnd");

        // The ordinary rule still holds for every other callback.
        assert_eq!(ibapi_name("tick_price"), "tickPrice");
        assert_eq!(ibapi_name("connection_closed"), "connectionClosed");
        assert_eq!(ibapi_name("order_status"), "orderStatus");
        assert_eq!(ibapi_name("error"), "error");
    }

    /// A client that has not connected holds no session, and says so on every
    /// question about one.
    #[test]
    fn eclient_default_state() {
        Python::initialize();
        Python::attach(|py| {
            let client = Py::new(py, client_with(py, recording_wrapper(py))).unwrap();
            assert!(!client.get().connected.load(Ordering::Relaxed));
            assert_eq!(client.get().account(), "");
            assert!(client.get().accounts.lock().unwrap().is_empty());
            assert!(client.get().shared.lock().unwrap().is_none());
            assert_eq!(client.get().events_lost.load(Ordering::Acquire), 0);

            // And a pump on it does nothing rather than dispatching against a
            // session that is not there.
            client.call_method0(py, "poll").unwrap();
            let err = client.call_method0(py, "run").unwrap_err();
            assert!(err.to_string().contains("Not connected"), "got {err}");
        });
    }

    /// `conn_state` is `CONNECTING` for the length of `connect`: the flag is
    /// claimed at the top of that call and the session's state handed over at
    /// the bottom. A session that ended without being closed is connecting
    /// again from the moment `connect` is called on it, with what it held
    /// still in place.
    #[test]
    fn conn_state_is_connecting_for_the_length_of_connect() {
        Python::initialize();
        Python::attach(|py| {
            let client = client_with(py, recording_wrapper(py));
            assert_eq!(client.conn_state(), EClient::DISCONNECTED);
            client.connected.store(true, Ordering::Release);
            assert_eq!(client.conn_state(), EClient::CONNECTING);
            *client.shared.lock().unwrap() = Some(Arc::new(SharedState::new()));
            assert_eq!(client.conn_state(), EClient::CONNECTED);
            client.session_ended.store(true, Ordering::Release);
            assert_eq!(client.conn_state(), EClient::CONNECTING);
            client.connected.store(false, Ordering::Release);
            assert_eq!(client.conn_state(), EClient::DISCONNECTED);
        });
    }

    /// The venue's stamp on the logon is the connection time, and nothing is
    /// answered without a session.
    #[test]
    fn the_connection_time_is_the_venues_stamp_on_the_logon() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, _shared, _w) = wired_client(py);
            let client = client.get();
            assert_eq!(client.tws_connection_time(), None);
            *client.logged_in_at.lock().unwrap() = Some("20260902-13:30:00".into());
            assert_eq!(client.tws_connection_time().as_deref(), Some("20260902-13:30:00"));
            client.connected.store(false, Ordering::Release);
            assert_eq!(client.tws_connection_time(), None);
        });
    }

    /// The client id is taken under either spelling, and a call giving both is
    /// told so rather than having one picked.
    #[test]
    fn the_client_id_is_taken_under_either_spelling() {
        assert_eq!(client_id_under_either_spelling(0, Some(3)).unwrap(), 3);
        assert_eq!(client_id_under_either_spelling(2, None).unwrap(), 2);
        Python::initialize();
        Python::attach(|py| {
            let err = client_id_under_either_spelling(2, Some(3)).unwrap_err();
            assert!(err.is_instance_of::<pyo3::exceptions::PyTypeError>(py), "got {err}");
        });
    }

    /// The gate hands the callback the factor it is asking for, and an
    /// authenticator account has no push to fall back on if the answer is
    /// wrong, so the label and the argument order are the whole contract.
    #[test]
    fn code_provider_passes_the_challenge_and_returns_the_code() {
        Python::initialize();
        Python::attach(|py| {
            let echo = py
                .eval(c"lambda factor, display_id, avth_url: f'{factor}/{display_id}/{avth_url}'", None, None)
                .unwrap()
                .unbind();
            let client = client_with(py, recording_wrapper(py));
            let provider = client.code_provider_from_py(Some(echo)).unwrap();

            let code = provider(IbKeyChallenge {
                factor: SecondFactor::AuthenticatorCode,
                display_id: String::new(),
                avth_url: String::new(),
            }).unwrap();
            assert_eq!(code, "authenticator//");

            let code = provider(IbKeyChallenge {
                factor: SecondFactor::IbKeyChallengeResponse,
                display_id: "AB12".into(),
                avth_url: "https://clientam.com/x".into(),
            }).unwrap();
            assert_eq!(code, "ibkey/AB12/https://clientam.com/x");

            // A raising callback comes back as an error carrying its message.
            // Escaping as a panic instead would leave the gate reporting only
            // that the provider died.
            let boom = py.eval(c"lambda *a: (_ for _ in ()).throw(ValueError('no code'))", None, None).unwrap().unbind();
            let err = client.code_provider_from_py(Some(boom)).unwrap()(IbKeyChallenge::default()).unwrap_err();
            assert!(err.to_string().contains("no code"), "got {err}");
        });
    }

    // ── Parity with the Rust EClient ──
    //
    // Most callers reach this engine through Python, so a request the Rust
    // surface carries and this one drops is a defect the majority of users hit.

    use crate::types::model::{Contract as ApiContract, Order as ApiOrder};
    use crate::types::PositionInfo;

    /// A client that is its own wrapper — `App(EWrapper, EClient)` built with
    /// `wrapper=self` — is found by the cyclic collector, and the drop it runs
    /// does its work: the venue is told, the engine is told, and its thread is
    /// waited for.
    #[test]
    fn a_client_that_is_its_own_wrapper_is_collected_and_its_engine_stopped() {
        Python::initialize();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let exited = Arc::new(AtomicBool::new(false));
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            // Stands in for the engine: reads the control channel until told
            // to stop, and takes its time leaving so a drop that does not
            // wait for it is caught.
            let (heard_here, exited_here) = (heard.clone(), exited.clone());
            let engine = thread::spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    let stop = matches!(cmd, ControlCommand::Shutdown);
                    heard_here.lock().unwrap().push(cmd);
                    if stop { break; }
                }
                thread::sleep(std::time::Duration::from_millis(50));
                exited_here.store(true, Ordering::Release);
            });
            *client.get()._thread.lock().unwrap() = Some(engine);
            let was = client.get().wrapper.write().unwrap().replace(client.clone_ref(py).into_any());
            drop(was);

            // Held by nothing but itself now. Only the collector can free it.
            drop(client);
            assert!(heard.lock().unwrap().is_empty());
            crate::python::collect_until(py, || Ok(exited.load(Ordering::Acquire))).unwrap();

            let heard = heard.lock().unwrap();
            assert!(
                matches!(heard[..], [ControlCommand::Logout, ControlCommand::Shutdown]),
                "the drop did not tell the venue and the engine: {heard:?}",
            );
            assert!(exited.load(Ordering::Acquire), "the drop did not wait for the engine thread");
        });
    }

    /// The collector clears the wrapper before the client is dropped. A
    /// callback made in between is told there is no wrapper, not handed one
    /// that is gone.
    #[test]
    fn a_cleared_client_says_it_has_no_wrapper() {
        Python::initialize();
        Python::attach(|py| {
            let client = client_with(py, recording_wrapper(py));
            client.callback(py, "error", (-1i64, 0i64, 504i64, "Not connected", "")).unwrap();
            client.__clear__();
            let err = client.callback(py, "error", (-1i64, 0i64, 504i64, "Not connected", "")).unwrap_err();
            assert!(err.to_string().contains("EClient has no wrapper"), "got {err}");
        });
    }

    /// A wrapper that keeps every callback it is handed, so a test can read
    /// back exactly what crossed the boundary.
    fn recording_wrapper(py: Python<'_>) -> Py<PyAny> {
        let ns = pyo3::types::PyDict::new(py);
        py.run(
            c"class W:
    def __init__(self): self.calls = []
    def __getattr__(self, name):
        return lambda *args: self.calls.append((name,) + args)
w = W()",
            None,
            Some(&ns),
        ).unwrap();
        ns.get_item("w").unwrap().unwrap().unbind()
    }

    /// A client bound to `wrapper`, as `EClient(wrapper)` leaves one.
    fn client_with(py: Python<'_>, wrapper: Py<PyAny>) -> EClient {
        let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
        *client.wrapper.write().unwrap() = Some(wrapper);
        client
    }

    /// A connected client whose engine is a channel the test reads.
    fn wired_client(
        py: Python<'_>,
    ) -> (Py<EClient>, std::sync::mpsc::Receiver<ControlCommand>, Arc<SharedState>, Py<PyAny>) {
        let w = recording_wrapper(py);
        let client = client_with(py, w.clone_ref(py));
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        *client.shared.lock().unwrap() = Some(shared.clone());
        *client.control_tx.lock().unwrap() = Some(tx);
        *client.account_id.lock().unwrap() = Some("DU123".into());
        client.connected.store(true, Ordering::Release);
        // A registration answered from another thread has to outlast being
        // scheduled. The millisecond a test states when it WANTS the wait to
        // fire is a race a loaded suite loses, and losing it reads as an
        // engine that went away; a test that wants the short one says so.
        client.core.set_registration_timeout(std::time::Duration::from_secs(30));
        (Py::new(py, client).unwrap(), rx, shared, w)
    }

    #[test]
    fn an_ordinary_callback_exception_escapes_and_closes_the_session() {
        Python::initialize();
        Python::attach(|py| {
            for method in ["poll", "run"] {
                for exception in ["ValueError", "RuntimeError"] {
                    let (client, rx, shared, w) = wired_client(py);
                    let g = pyo3::types::PyDict::new(py);
                    g.set_item("client", &client).unwrap();
                    g.set_item("w", &w).unwrap();
                    g.set_item("exception", exception).unwrap();
                    py.run(c"
import builtins
failure = getattr(builtins, exception)('callback failed')
def tick_params(req_id, *args):
    w.calls.append(('tickReqParams', req_id))
    if req_id == 7:
        raise failure
    client.disconnect()
w.tickReqParams = tick_params
", Some(&g), None).unwrap();
                    shared.market.push_tick_req_params_for(7, 0.01);
                    shared.market.push_tick_req_params_for(8, 0.02);

                    let err = client.call_method0(py, method).expect_err("the callback raises");
                    assert!(err.value(py).is(g.get_item("failure").unwrap().unwrap()));
                    assert!(!client.get().is_connected());
                    assert!(client.get().shared.lock().unwrap().is_none());
                    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Logout)));
                    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Shutdown)));
                    client.call_method0(py, "poll").unwrap();
                    py.run(c"
assert w.calls == [('tickReqParams', 7), ('connectionClosed',)], w.calls
", Some(&g), None).unwrap();
                }
            }
        });
    }

    #[test]
    fn callback_interrupts_still_escape_without_closing_the_session() {
        Python::initialize();
        Python::attach(|py| {
            for method in ["poll", "run"] {
                for exception in ["KeyboardInterrupt", "SystemExit"] {
                    let (client, _rx, shared, w) = wired_client(py);
                    let g = pyo3::types::PyDict::new(py);
                    g.set_item("w", &w).unwrap();
                    g.set_item("exception", exception).unwrap();
                    py.run(c"
import builtins
failure = getattr(builtins, exception)('stop callback')
def tick_params(req_id, *args):
    w.calls.append(('tickReqParams', req_id))
    raise failure
w.tickReqParams = tick_params
", Some(&g), None).unwrap();
                    shared.market.push_tick_req_params_for(7, 0.01);
                    shared.market.push_tick_req_params_for(8, 0.02);

                    let err = client.call_method0(py, method).unwrap_err();
                    assert!(err.value(py).is(g.get_item("failure").unwrap().unwrap()));
                    assert!(client.get().is_connected());
                    py.run(c"
assert w.calls == [('tickReqParams', 7)], w.calls
", Some(&g), None).unwrap();
                }
            }
        });
    }

    #[test]
    fn option_callbacks_receive_the_reference_decoders_none_values() {
        Python::initialize();
        Python::attach(|py| {
            for answers in [None, Some(7)] {
                let (client, _rx, shared, w) = wired_client(py);
                client.get().core.instrument_to_req.lock().unwrap().insert(0, 7);
                let cases = [
                    (f64::MAX, [None; 8]),
                    (f64::NAN, [None; 8]),
                    (-1.0, [None, Some(-1.0), None, None, Some(-1.0), Some(-1.0), Some(-1.0), None]),
                    (-2.0, [None, None, Some(-2.0), Some(-2.0), None, None, None, Some(-2.0)]),
                    (-0.42, [None, Some(-0.42), Some(-0.42), Some(-0.42), Some(-0.42), Some(-0.42), Some(-0.42), Some(-0.42)]),
                    (0.0, [Some(0.0); 8]),
                    (1.25, [Some(1.25); 8]),
                ];
                for (value, expected) in cases {
                    shared.market.push_option_computation(OptionComputation {
                        answers, implied_vol: value, delta: value,
                        opt_price: value, pv_dividend: value, gamma: value,
                        vega: value, theta: value, und_price: value,
                        ..OptionComputation::solved(7)
                    });
                    client.get().dispatch_once(py, &shared).unwrap();
                    let g = pyo3::types::PyDict::new(py);
                    g.set_item("w", &w).unwrap();
                    g.set_item("expected", expected.to_vec()).unwrap();
                    g.set_item("tick_type", if answers.is_some() { 53 } else { 13 }).unwrap();
                    py.run(c"
assert len(w.calls) == 1, w.calls
call = w.calls.pop()
assert call[:4] == ('tickOptionComputation', 7, tick_type, 0), call
for value, expected_value in zip(call[4:], expected):
    if expected_value is None:
        assert value is None, call
    else:
        assert value == expected_value, call
", Some(&g), None).unwrap();
                }
            }
        });
    }

    /// Queued publications stop naming a request once its watch is withdrawn.
    /// An answer to a calculation still names the question that asked it.
    #[test]
    fn news_and_models_are_delivered_only_to_current_watchers() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            client.get().core.req_to_instrument.lock().unwrap().extend([(1, 0), (2, 0)]);
            client.get().core.instrument_to_req.lock().unwrap().insert(0, 1);
            client.get().core.instrument_followers.lock().unwrap().insert(0, vec![2]);
            let publish = || {
                shared.market.push_tick_news(TickNews {
                    instrument: 0, timestamp: 0, provider_code: "BRFG".into(),
                    article_id: "BRFG$1".into(), headline: "SPY headline".into(),
                });
                shared.market.push_option_computation(OptionComputation {
                    instrument: 0, ..Default::default()
                });
            };
            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            publish();
            client.get().dispatch_once(py, &shared).unwrap();
            py.run(c"
assert [c[1] for c in w.calls if c[0] in ('tickNews', 'tick_news')] == [1, 2]
assert [(c[1], c[2]) for c in w.calls if c[0] in ('tickOptionComputation', 'tick_option_computation')] == [(1, 13), (2, 13)]
w.calls.clear()
", Some(&g), None).unwrap();

            publish();
            client.call_method1(py, "cancel_mkt_data", (1,)).unwrap();
            client.call_method1(py, "cancel_mkt_data", (2,)).unwrap();
            shared.market.push_option_computation(OptionComputation {
                instrument: 0, answers: Some(7), ..Default::default()
            });
            client.get().dispatch_once(py, &shared).unwrap();
            py.run(c"
assert [c[1] for c in w.calls if c[0] in ('tickNews', 'tick_news')] == []
assert [(c[1], c[2]) for c in w.calls if c[0] in ('tickOptionComputation', 'tick_option_computation')] == [(7, 53)]
", Some(&g), None).unwrap();
        });
    }

    /// A pass handed a session that has already been replaced returns before
    /// reading its flags. The live session keeps its own connection state.
    #[test]
    fn a_pass_on_a_replaced_session_leaves_the_new_sessions_flag_alone() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, old, _w) = wired_client(py);
            let client = client.borrow(py);
            old.set_connection_lost();
            // Another session is the client's by the time the pass runs.
            *client.shared.lock().unwrap() = Some(Arc::new(SharedState::new()));
            client.connected.store(true, Ordering::Release);
            client.dispatch_once(py, &old).unwrap();
            assert!(client.connected.load(Ordering::Relaxed), "the live session is not marked lost by the old one's pass");
            assert!(!client.session_ended.load(Ordering::Relaxed));
        });
    }

    /// The engine writes down why a session finished; the notice saying so
    /// goes out on a channel that drops what it cannot hold. A loop ending
    /// only on the notice waits for ever on a session that is already over.
    #[test]
    fn a_session_recorded_as_over_ends_the_loop_without_the_notice() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            // Nothing is queued: this is the notice having been dropped.
            shared.reference.set_session_over(
                crate::reliability::retry::DisconnectReason::EngineStopped.as_str(),
            );
            let client = client.borrow(py);
            client.dispatch_once(py, &shared).unwrap();
            assert!(
                client.session_ended.load(Ordering::Relaxed),
                "run() would go on waiting on a session the engine has finished",
            );
            assert!(
                !client.connected.load(Ordering::Relaxed),
                "a finished session still reads as connected",
            );
        });
    }

    /// A session the engine has given up is over on this surface too, whether
    /// or not the caller has dispatched since.
    ///
    /// `is_connected` and `conn_state` read flags the dispatch pass writes,
    /// and the sender outlives the session it was made for. A program driving
    /// its own loop, or none, was told connected on a session that had ended
    /// and went on queueing requests into it, each waiting out its own timeout
    /// for an answer nobody was going to make — while the Rust surface, which
    /// reads the state the engine writes, reported it gone at once.
    #[test]
    fn a_session_the_engine_gave_up_is_neither_connected_nor_admitting() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, wrapper) = wired_client(py);
            let client = client.borrow(py);
            assert!(client.is_connected());
            assert_eq!(client.conn_state(), EClient::CONNECTED);

            // The engine gives the session up. Nothing is dispatched here:
            // this is the notice dropped, or a caller that never reads for it.
            shared.reference.set_session_over(
                crate::reliability::retry::DisconnectReason::EngineStopped.as_str(),
            );

            assert!(!client.is_connected(), "a finished session reads as connected");
            assert_eq!(
                client.conn_state(), EClient::DISCONNECTED,
                "a finished session is not a session being connected",
            );
            assert!(
                client.tx_or_report(7).unwrap().is_none(),
                "a request was admitted into a session that has ended",
            );
            assert!(
                rx.try_recv().is_err(),
                "and queued for an engine that has stopped",
            );

            // Refused the way a request made with no session is refused, on
            // the error callback and under that number.
            let calls = wrapper.bind(py).getattr("calls").unwrap();
            let heard = calls
                .extract::<Vec<(String, i64, i64, i64, String, String)>>()
                .unwrap();
            assert!(
                heard.iter().any(|(name, req_id, _, code, message, _)| {
                    name == "error"
                        && *req_id == 7
                        && *code == NOT_CONNECTED_CODE
                        && message == "Not connected"
                }),
                "the caller is told on the error callback: {heard:?}",
            );
        });
    }

    /// An answering call waits on the session it holds, so its request has
    /// to go on that session's sender: a reconnect replaces the session in
    /// two writes, and a sender taken after them serves the session that
    /// replaced it, where nobody is waiting. The pair is checked as one,
    /// and a sender whose session was replaced is refused rather than
    /// sent on.
    #[test]
    fn a_sender_is_refused_once_its_session_is_replaced() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            // A reconnect, landing after the state was taken: the slots now
            // hold a second session.
            let successor = Arc::new(SharedState::new());
            let (successor_tx, _successor_rx) =
                std::sync::mpsc::sync_channel::<ControlCommand>(16);
            *client.get().shared.lock().unwrap() = Some(successor.clone());
            *client.get().control_tx.lock().unwrap() = Some(successor_tx);

            let err = client.get().paired_sender(&shared).unwrap_err();
            assert!(
                err.to_string().contains("the session was replaced"),
                "got {err}",
            );

            // The session still held answers for itself.
            assert!(client.get().paired_sender(&successor).is_ok());
        });
    }

    /// An account with nothing working counts from one, and one working order
    /// puts the count past it. The venue holds an id only while its order is
    /// live, so this is the whole of what a new id has to clear.
    #[test]
    fn the_counter_starts_past_what_is_working_and_no_further() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            let client = client.borrow(py);
            assert_eq!(client.stated_order_id(), 1, "nothing is working");

            let working = || crate::bridge::RichOrderInfo {
                contract: Default::default(),
                order: Default::default(),
                order_state: crate::types::model::OrderState {
                    status: "Submitted".into(), ..Default::default()
                },
                last_exec: Default::default(),
            };
            shared.orders.push_order_info(41, working());
            assert_eq!(
                client.stated_order_id(), 42,
                "an order named under 41 is what the next id has to clear",
            );
            assert_eq!(client.take_order_id(py), 42);
            assert_eq!(client.take_order_id(py), 43, "and the count goes on from there");

            shared.orders.push_order_info(7, working());
            assert_eq!(
                client.take_order_id(py), 44,
                "an order named under a lower id does not move the count back",
            );

            // A fill spends its id as surely as a working order holds it: the
            // venue refuses the second placement under it either way.
            let filled = crate::bridge::RichOrderInfo {
                contract: Default::default(),
                order: Default::default(),
                order_state: crate::types::model::OrderState {
                    status: "Filled".into(), ..Default::default()
                },
                last_exec: Default::default(),
            };
            shared.orders.push_order_info(90, filled);
            assert_eq!(
                client.take_order_id(py), 91,
                "an id a fill has spent is counted past, not handed out again",
            );
        });
    }

    /// An id a caller numbers themselves need not fit the width the request
    /// carries. Truncating it answers under an id they never used, so it is
    /// refused instead.
    #[test]
    fn a_req_id_past_u32_is_refused_rather_than_truncated() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            let big = u32::MAX as i64 + 1;
            for method in [
                "cancel_historical_data", "cancel_head_time_stamp", "cancel_scanner_subscription",
                "cancel_fundamental_data", "cancel_histogram_data", "cancel_mkt_depth",
                "cancel_real_time_bars",
            ] {
                let Err(err) = client.call_method1(py, method, (big,)) else {
                    panic!("{method} accepted a req_id it cannot carry");
                };
                assert!(err.to_string().contains("outside the range"), "{method}: got {err}");
            }
            assert!(rx.try_recv().is_err(), "a refused req_id must reach no engine command");
        });
    }

    /// Backfill and then stream on one number is the ordinary way to write
    /// it. The finished lookup left the number marked as one whose bars are
    /// updates to it, and every bar of the stream was routed to
    /// `historical_data_update` — so a caller that overrode `real_time_bar`
    /// read the stream as dead.
    #[test]
    fn a_stream_taking_a_finished_lookups_number_is_delivered_as_a_stream() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            client.get().core.hist_initial_complete.lock().unwrap().insert(1);
            let contract = Py::new(py, Contract { con_id: 756733, ..Default::default() }).unwrap();
            client.call_method1(py, "req_real_time_bars", (1i64, &contract)).unwrap();
            shared.market.push_real_time_bar(1, RealTimeBar { timestamp: 1_700_000_000, ..Default::default() });
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let names: Vec<String> = py
                .eval(c"[c[0] for c in w.calls]", Some(&g), None)
                .unwrap().extract().unwrap();
            assert_eq!(names, ["realtimeBar"], "the stream's bar went somewhere else");
        });
    }

    /// The reference client sends a book request's secType and exchange
    /// straight off the contract. Filling them in here subscribes a contract
    /// naming only an id to the book of a US stock on SMART, under the
    /// caller's own request id.
    #[test]
    fn a_book_request_states_the_contract_it_was_given() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            let contract = Py::new(py, Contract {
                con_id: 495512563,
                sec_type: String::new(),
                exchange: String::new(),
                ..Default::default()
            }).unwrap();
            client.call_method1(py, "req_mkt_depth", (1i64, &contract, 5i32, false)).unwrap();

            let sent = rx.try_recv().expect("the request reached no engine command");
            let ControlCommand::SubscribeDepth { contract, .. } = sent else {
                panic!("a book request sent something else");
            };
            assert_eq!(contract.exchange, "", "an exchange was invented");
            assert_eq!(contract.sec_type, "", "a security type was invented");
            assert_eq!(contract.con_id, 495512563);
        });
    }

    /// An execution the venue restated at logon is announced to nobody and
    /// answers the caller that asks for it — on this surface as on the Rust
    /// one, so a program written against either reads the same record.
    #[test]
    fn a_restated_execution_answers_req_executions_and_nobody_else() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            shared.orders.push_restated_execution(
                ApiContract { symbol: "SPY".into(), ..Default::default() },
                crate::types::model::Execution {
                    exec_id: "0001f4e8.1".into(), side: "BOT".into(), shares: 10.0,
                    ..Default::default()
                },
            );
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let told = || -> Vec<(i64, String)> {
                py.eval(
                    c"[(c[1], c[3].exec_id) for c in w.calls if c[0] in ('exec_details', 'execDetails')]",
                    Some(&g), None,
                ).unwrap().extract().unwrap()
            };
            assert!(told().is_empty(), "nobody asked, so nobody is told");

            client.call_method1(py, "req_executions", (7i64,)).unwrap();
            client.borrow(py).dispatch_once(py, &shared).unwrap();
            assert_eq!(told(), [(7, "0001f4e8.1".to_string())], "the caller that asked is answered");
        });
    }

    /// What a replay states about a fill is what the callback stated.
    ///
    /// The record kept for `req_executions` was built from nothing rather than
    /// from the report, so it dropped everything only the report carries — the
    /// caller's own label for the order, whether the venue closed the position,
    /// which side of the book the fill took. The live callback stated all
    /// three and the replay of the same fill stated none of them, which is the
    /// twin of this surface's own callback, not of the other surface.
    #[test]
    fn a_replayed_fill_states_what_its_callback_stated() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            shared.orders.push_order_info(77, crate::bridge::RichOrderInfo {
                contract: ApiContract { symbol: "SPY".into(), ..Default::default() },
                order: Default::default(),
                order_state: Default::default(),
                last_exec: crate::types::model::Execution {
                    exec_id: "0001f4e8.7".into(),
                    order_ref: "my-strategy".into(),
                    liquidation: 1,
                    last_liquidity: 2,
                    ..Default::default()
                },
            });
            shared.orders.push_fill(crate::types::Fill {
                instrument: 0, order_id: 77, side: crate::types::Side::Buy,
                price: 150 * crate::types::PRICE_SCALE, qty: 10 * crate::types::QTY_SCALE,
                remaining: 0, timestamp_ns: 0,
                cum_qty: 10 * crate::types::QTY_SCALE,
                avg_price: 150 * crate::types::PRICE_SCALE,
            });
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let told = || -> Vec<(String, i32, i32)> {
                py.eval(
                    c"[(c[3].orderRef, c[3].liquidation, c[3].lastLiquidity) \
                       for c in w.calls if c[0] in ('exec_details', 'execDetails')]",
                    Some(&g), None,
                ).unwrap().extract().unwrap()
            };
            assert_eq!(
                told(), [("my-strategy".to_string(), 1, 2)],
                "the callback states what the report stated",
            );

            client.call_method1(py, "req_executions", (7i64,)).unwrap();
            client.borrow(py).dispatch_once(py, &shared).unwrap();
            assert_eq!(
                told(), [
                    ("my-strategy".to_string(), 1, 2),
                    ("my-strategy".to_string(), 1, 2),
                ],
                "and the replay of that same fill states it too",
            );
        });
    }

    /// Two prints of one order in one pass are two executions, here too.
    ///
    /// Everything about a fill beyond the print is on the report it was booked
    /// off. Looked up against the order afterwards, both prints read the record
    /// the later one left.
    #[test]
    fn two_prints_of_one_order_in_one_pass_are_two_executions() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            let report = |exec_id: &str, cum: f64| crate::bridge::RichOrderInfo {
                contract: ApiContract { symbol: "SPY".into(), ..Default::default() },
                order: Default::default(),
                order_state: Default::default(),
                last_exec: crate::types::model::Execution {
                    exec_id: exec_id.into(), cum_qty: cum, ..Default::default()
                },
            };
            let print = |qty: i64, remaining: i64| crate::types::Fill {
                instrument: 0, order_id: 77, side: crate::types::Side::Buy,
                price: 150 * crate::types::PRICE_SCALE, qty, remaining, timestamp_ns: 0, cum_qty: qty,
                avg_price: 150 * crate::types::PRICE_SCALE,
            };
            shared.orders.push_fill_reported(
                print(5 * crate::types::QTY_SCALE, 5 * crate::types::QTY_SCALE),
                report("0001f4e8.1", 5.0),
            );
            shared.orders.push_fill_reported(
                print(5 * crate::types::QTY_SCALE, 0),
                report("0001f4e8.2", 10.0),
            );
            // The order's own record holds the later report, as in a session.
            shared.orders.push_order_info(77, report("0001f4e8.2", 10.0));
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let rows: Vec<(String, f64)> = py.eval(
                c"[(c[3].execId, c[3].cumQty) for c in w.calls \
                   if c[0] in ('exec_details', 'execDetails')]",
                Some(&g), None,
            ).unwrap().extract().unwrap();
            assert_eq!(
                rows,
                [("0001f4e8.1".to_string(), 5.0), ("0001f4e8.2".to_string(), 10.0)],
                "each print is reported under the execution it was booked off",
            );
        });
    }

    /// One pass can carry two reports for the same order: an acknowledgement
    /// and then a fill. Keeping one report per order dropped the earlier one,
    /// and the caller was never told the order had been acknowledged.
    // Drives the client through the methods that push state into it, which
    // a wheel does not carry: see the `test-helpers` feature.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn an_acknowledgement_survives_a_fill_in_the_same_pass() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, _shared, w) = wired_client(py);

            // The venue acknowledges the order, then fills half of it, both
            // before the caller pumps the queue again.
            client.call_method1(py, "_test_push_order_update",
                (88u64, 0u32, "Submitted", 0.0f64, 100.0f64)).unwrap();
            client.call_method1(py, "_test_push_fill",
                (0u32, 88u64, "BUY", 150.0f64, 50i64, 50i64, 0.0f64)).unwrap();
            client.call_method1(py, "_test_push_order_update",
                (88u64, 0u32, "PartiallyFilled", 50.0f64, 50.0f64)).unwrap();

            client.call_method0(py, "_test_dispatch_once").unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let reported: usize = py.eval(
                c"len([c for c in w.calls if c[0] in ('order_status', 'orderStatus')])",
                Some(&g), None,
            ).unwrap().extract().unwrap();
            assert_eq!(
                reported, 2,
                "the acknowledgement was dropped by the fill that followed it",
            );
        });
    }

    /// `reqPositions` subscribes to a real-time feed: a holding that moves
    /// after the call is reported as it moves. Answering only the set held
    /// when the call was made left a caller tracking its positions from a
    /// snapshot that went stale on the next fill.
    // Drives the client through the methods that push state into it, which
    // a wheel does not carry: see the `test-helpers` feature.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn a_holding_that_moves_after_the_request_is_reported() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            shared.portfolio.account_download_is_settled();
            let held = |qty: f64| PositionInfo {
                con_id: 756733, position: qty, symbol: "SPY".into(),
                sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
            };
            shared.portfolio.set_position_info(held(1.0));
            client.call_method0(py, "req_positions").unwrap();
            client.call_method0(py, "_test_dispatch_once").unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let reported = || -> usize {
                py.eval(c"len([c for c in w.calls if c[0] == 'position'])", Some(&g), None)
                    .unwrap().extract().unwrap()
            };
            // The answer comes first in the pass: the holding, then the end.
            // The feed follows it on the same pass with what moved before the
            // ask, which is the same holding again — see `req_positions` —
            // so what moves after is counted from here.
            let opening: Vec<String> = py
                .eval(c"[c[0] for c in w.calls][:2]", Some(&g), None)
                .unwrap().extract().unwrap();
            assert_eq!(opening, ["position", "positionEnd"], "the holding held when it was asked for");
            let asked = reported();

            shared.portfolio.set_position_info(held(3.0));
            client.call_method0(py, "_test_dispatch_once").unwrap();
            assert_eq!(reported(), asked + 1, "and the holding once it moves");

            // Withdrawn, so what moves after is no longer reported.
            client.call_method0(py, "cancel_positions").unwrap();
            shared.portfolio.set_position_info(held(5.0));
            client.call_method0(py, "_test_dispatch_once").unwrap();
            assert_eq!(reported(), asked + 1, "a withdrawn ask is not answered further");
        });
    }

    /// The Rust surface hands `position` the whole contract. An options
    /// position whose strike, right and expiry are dropped names no contract
    /// the caller can act on.
    #[test]
    fn a_position_carries_the_contract_it_is_a_position_in() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            client.get().core.cache_contract(756733, ApiContract {
                con_id: 756733, symbol: "SPY".into(), sec_type: "OPT".into(),
                exchange: "SMART".into(), currency: "USD".into(),
                last_trade_date_or_contract_month: "20260320".into(),
                strike: 600.0, right: "C".into(), multiplier: "100".into(),
                ..Default::default()
            });
            shared.portfolio.set_position_info(PositionInfo {
                con_id: 756733, position: 1.0, ..Default::default()
            });
            shared.portfolio.account_download_is_settled();
            shared.portfolio.set_account(&Default::default());

            client.call_method0(py, "req_positions").unwrap();
            client.call_method1(py, "req_positions_multi", (1i64, "DU123", "")).unwrap();
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            // Under the reference client's own spelling, which is what a
            // wrapper written against it defines and so what these answers are
            // delivered as.
            for (callback, index) in [("position", 2), ("positionMulti", 4)] {
                let expr = format!("[c[{index}] for c in w.calls if c[0] == '{callback}'][0]");
                let c = py.eval(&std::ffi::CString::new(expr).unwrap(), Some(&g), None).unwrap();
                for (field, want) in [
                    ("symbol", "SPY"), ("sec_type", "OPT"), ("right", "C"),
                    ("multiplier", "100"), ("last_trade_date_or_contract_month", "20260320"),
                ] {
                    let got: String = c.getattr(field).unwrap().extract().unwrap();
                    assert_eq!(got, want, "{callback} dropped {field}");
                }
                let strike: f64 = c.getattr("strike").unwrap().extract().unwrap();
                assert_eq!(strike, 600.0, "{callback} dropped strike");
            }
        });
    }

    /// A symbol match states the contract's type in the spelling a request
    /// takes, which is the one the Rust surface hands back. The wire spelling
    /// — a stock is CS there — came back to a caller whose own calls do not
    /// accept it, so the match could not be put back into one.
    #[test]
    fn a_symbol_match_states_its_type_the_way_a_request_takes_it() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            shared.reference.push_matching_symbols(8, vec![
                crate::control::contracts::SymbolMatch {
                    con_id: 265598, symbol: "AAPL".into(),
                    sec_type: crate::control::contracts::SecurityType::Stock,
                    currency: "USD".into(), primary_exchange: "NASDAQ".into(),
                    description: "Apple Inc".into(), derivative_types: vec!["OPT".into()],
                    issuer_id: String::new(),
                },
            ]);
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let c = py.eval(
                c"[c[2][0] for c in w.calls if c[0] == 'symbolSamples'][0]",
                Some(&g), None,
            ).unwrap();
            let sec_type: String = c.getattr("sec_type").unwrap().extract().unwrap();
            assert_eq!(sec_type, "STK", "a stock is stated the way a request takes it");
        });
    }

    /// A symbol match states the contract it found under `contract`, as the
    /// reference client states one: a program written against it reads
    /// `description.contract.conId`, and under flat fields it raised before
    /// it read anything.
    #[test]
    fn a_symbol_match_states_what_it_found_as_a_contract_and_its_derivatives() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            shared.reference.push_matching_symbols(8, vec![
                crate::control::contracts::SymbolMatch {
                    con_id: 265598, symbol: "AAPL".into(),
                    sec_type: crate::control::contracts::SecurityType::Stock,
                    currency: "USD".into(), primary_exchange: "NASDAQ".into(),
                    description: "Apple Inc".into(), derivative_types: vec!["OPT".into()],
                    issuer_id: String::new(),
                },
            ]);
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let c = py.eval(
                c"[c[2][0] for c in w.calls if c[0] == 'symbolSamples'][0]",
                Some(&g), None,
            ).unwrap();
            let con_id: i64 = c.getattr("contract").unwrap()
                .getattr("conId").unwrap().extract().unwrap();
            assert_eq!(con_id, 265598, "the contract found, under the name the reference client gives it");
            let derivatives: Vec<String> = c.getattr("derivativeSecTypes").unwrap().extract().unwrap();
            assert_eq!(derivatives, ["OPT"], "the kinds of derivative the venue lists on it");
        });
    }

    /// A symbol match answered to the caller states what the same match
    /// states on the callback.
    ///
    /// The venue's own words for a contract, and the id it gives an issuer,
    /// are the whole of what a match naming an issuer rather than a contract
    /// carries — a lookup for that issuer's fixed income is made under it.
    /// Dropped from the answer, a caller that read the return value was handed
    /// a match it could not follow, while the same match delivered through the
    /// wrapper carried both.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn a_symbol_match_answered_to_the_caller_states_the_words_and_the_issuer() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            // Seeded before the question is asked, which is the only way to
            // exercise the waiting with no venue on the other end.
            let req_id = super::ask::peek_ask_id(&shared) as u32;
            shared.reference.push_matching_symbols(req_id, vec![
                crate::control::contracts::SymbolMatch {
                    con_id: 0, symbol: "APPLE".into(),
                    sec_type: crate::control::contracts::SecurityType::Bond,
                    currency: "USD".into(), primary_exchange: String::new(),
                    description: "Apple Inc".into(), derivative_types: Vec::new(),
                    issuer_id: "e1234567".into(),
                },
            ]);

            let g = pyo3::types::PyDict::new(py);
            g.set_item("client", &client).unwrap();
            let found = py
                .eval(c"client.matching_symbols('APPLE')[0].contract", Some(&g), None)
                .unwrap();

            let words: String = found.getattr("description").unwrap().extract().unwrap();
            assert_eq!(words, "Apple Inc", "the venue's own words for what it found");
            let issuer: String = found.getattr("issuerId").unwrap().extract().unwrap();
            assert_eq!(issuer, "e1234567", "and the id a bond lookup is made under");
        });
    }

    /// A schedule states the window it covers as well as the sessions in it.
    ///
    /// The venue answers a duration counted in its own trading days, so what
    /// an answer covers is not what was asked for, and the answer says so.
    /// Handed back as the zone and the sessions alone, a program reading the
    /// stretch it was given found no such field, while the same answer
    /// delivered through the wrapper carried both ends of it.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn a_schedule_answered_to_the_caller_states_the_window_it_covers() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            // Seeded before the question is asked, which is the only way to
            // exercise the waiting with no venue on the other end.
            let req_id = super::ask::peek_ask_id(&shared) as u32;
            shared.reference.push_historical_schedule(req_id, crate::types::HistoricalScheduleResponse {
                query_id: String::new(),
                timezone: "US/Eastern".into(),
                start_date_time: "20260907-00:00:00".into(),
                end_date_time: "20260911-23:59:59".into(),
                sessions: vec![crate::types::ScheduleSession {
                    ref_date: "20260908".into(),
                    open_time: "20260908-09:30:00".into(),
                    close_time: "20260908-16:00:00".into(),
                }],
            });

            let g = pyo3::types::PyDict::new(py);
            g.set_item("client", &client).unwrap();
            g.set_item("spy", Py::new(py, crate::python::compat::contract::Contract {
                symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(),
                currency: "USD".into(), ..Default::default()
            }).unwrap()).unwrap();
            let answered: (String, String, String, Vec<(String, String, String)>) = py
                .eval(c"client.trading_schedule(spy, '', '1 W', True)", Some(&g), None)
                .unwrap()
                .extract()
                .unwrap();

            assert_eq!(answered.0, "20260907-00:00:00", "where the stretch the venue answered begins");
            assert_eq!(answered.1, "20260911-23:59:59", "and where it ends");
            assert_eq!(answered.2, "US/Eastern", "the zone the times are stated in");
            assert_eq!(answered.3.len(), 1, "and the sessions in it");
        });
    }

    /// `permId` is what survives a restart; the local order id does not.
    #[test]
    fn an_order_can_be_cancelled_by_its_perm_id() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            client.get().core.track_order(
                77,
                ApiContract { con_id: 756733, symbol: "SPY".into(), ..Default::default() },
                ApiOrder { order_id: 77, total_quantity: 1.0, perm_id: 91011, ..Default::default() },
                0,
            );

            client.call_method1(py, "cancel_order_by_perm_id", (91011i64,)).unwrap();
            match rx.try_recv().expect("a cancel must reach the engine") {
                ControlCommand::Order(OrderRequest::Cancel { order_id }) => assert_eq!(order_id, 77),
                other => panic!("expected a Cancel, got {other:?}"),
            }
        });
    }

    /// The per-request market-data mode is what keeps a thinly-traded name
    /// streaming after hours; without it this surface can only ask for realtime.
    #[test]
    fn req_mkt_data_ex_carries_the_market_data_mode() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            // Stated in full, so the call subscribes rather than asking the
            // venue to name a contract that carries neither type nor exchange.
            let contract = Py::new(py, Contract {
                con_id: 756733, symbol: "SPY".into(),
                sec_type: "STK".into(), exchange: "SMART".into(),
                ..Default::default()
            }).unwrap();
            // The registration reply comes from an engine no test has, so the
            // call itself fails; the commands are on the channel either way.
            let _ = client.call_method1(py, "req_mkt_data_ex", (1i64, &contract, "", false, false, 2i32));

            let mut mode = None;
            while let Ok(cmd) = rx.try_recv() {
                if let ControlCommand::Subscribe { mode_9887, .. } = cmd {
                    mode = Some(mode_9887);
                }
            }
            assert_eq!(mode, Some(2), "the requested market-data mode must reach the subscribe");
        });
    }

    /// Both are read off the same logon push the Rust surface exposes; a webapp
    /// REST call from Python needs them.
    #[test]
    fn the_session_id_and_url_map_from_logon_are_readable() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            shared.reference.set_ccp_session_id("abc.0001".into());
            shared.reference.set_misc_urls(
                [("region_dam".to_string(), "api.ibkr.com".to_string())].into_iter().collect(),
            );

            let id: String = client.call_method0(py, "ccp_session_id").unwrap().extract(py).unwrap();
            assert_eq!(id, "abc.0001");
            let url: Option<String> = client.call_method1(py, "misc_url", ("region_dam",))
                .unwrap().extract(py).unwrap();
            assert_eq!(url.as_deref(), Some("api.ibkr.com"));
            let missing: Option<String> = client.call_method1(py, "misc_url", ("nope",))
                .unwrap().extract(py).unwrap();
            assert_eq!(missing, None);
        });
    }

    /// The chain callback takes seven arguments in an order nothing on this
    /// side of the boundary checks, and a caller reads the strikes it is
    /// handed by position.
    // Drives the client through the methods that push state into it, which
    // a wheel does not carry: see the `test-helpers` feature.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn an_option_chain_crosses_the_boundary_in_the_order_a_caller_reads_it() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, w) = wired_client(py);

            client.call_method1(py, "req_sec_def_opt_params", (9i64, "AAPL", "", "STK", 265598i64)).unwrap();
            match rx.try_recv().expect("the request must reach the engine") {
                ControlCommand::FetchOptionParams { req_id, symbol, underlying_con_id, .. } => {
                    assert_eq!((req_id, symbol.as_str(), underlying_con_id), (9, "AAPL", 265598));
                }
                other => panic!("expected a chain request, got {other:?}"),
            }

            shared.reference.push_option_params(9, 265598, vec![
                crate::control::contracts::OptionChainScope {
                    symbol: "AAPL".into(), exchange: "SMART".into(), trading_class: "AAPL".into(),
                    multiplier: "100".into(), expirations: vec!["20260116".into(), "20260320".into()],
                    strikes: vec![140.0, 145.0], underlying_con_id: 265598,
                },
            ]);
            client.call_method0(py, "_test_dispatch_once").unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            // This wrapper answers to any name at all, so the callback reaches
            // it under the reference client's spelling as readily as under this
            // one's. Either is correct here; a wrapper that names the callback
            // explicitly is what the naming tests cover.
            let call = py.eval(
                c"[c for c in w.calls if c[0] in ('security_definition_option_parameter', 'securityDefinitionOptionParameter')][0]",
                Some(&g), None,
            ).unwrap();
            let arg = |n: usize| call.get_item(n).unwrap();
            assert_eq!(arg(1).extract::<i64>().unwrap(), 9);
            assert_eq!(arg(2).extract::<String>().unwrap(), "SMART");
            assert_eq!(arg(3).extract::<i64>().unwrap(), 265598);
            assert_eq!(arg(4).extract::<String>().unwrap(), "AAPL");
            assert_eq!(arg(5).extract::<String>().unwrap(), "100");
            assert_eq!(arg(6).extract::<Vec<String>>().unwrap(), ["20260116", "20260320"]);
            assert_eq!(arg(7).extract::<Vec<f64>>().unwrap(), [140.0, 145.0]);

            let ended: usize = py.eval(
                c"len([c for c in w.calls if c[0] in ('security_definition_option_parameter_end', 'securityDefinitionOptionParameterEnd')])",
                Some(&g), None,
            ).unwrap().extract().unwrap();
            assert_eq!(ended, 1, "the request ends once");
        });
    }
    /// A restore left in the backlog does not undo the loss that followed it.
    ///
    /// The engine's flags say which way the connection last went; the events
    /// are the announcement, and the channel carrying them drops what it
    /// cannot hold. A restore still queued when the session went again was
    /// read as the state, so this surface reported a dead session as up while
    /// the other read it as down.
    #[test]
    fn a_restore_left_in_the_backlog_is_not_read_as_the_state() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            let client = client.borrow(py);
            let (event_tx, event_rx) = std::sync::mpsc::sync_channel(16);
            *client.event_rx.lock().unwrap() = Some(event_rx);

            // It came back, and went again before anybody read either.
            shared.set_connection_restored();
            event_tx.send(crate::bridge::Event::Reconnected).unwrap();
            shared.set_connection_lost();

            client.dispatch_once(py, &shared).unwrap();

            assert!(
                !client.connected.load(Ordering::Relaxed),
                "the session is down, whatever is still queued behind that",
            );
        });
    }

    /// The engine records which way the connection last went in flags it
    /// always writes, and announces it on a channel that drops what it cannot
    /// hold. A client that reads only the notice is told nothing by an outage
    /// it fell behind on: it goes on reporting a connection that is gone, and
    /// goes on reporting one that came back as gone.
    #[test]
    fn a_connection_that_went_and_came_back_is_read_without_the_notice() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, _w) = wired_client(py);
            let client = client.borrow(py);

            // Nothing is queued in either case: this is the notice dropped.
            shared.set_connection_lost();
            client.dispatch_once(py, &shared).unwrap();
            assert!(
                !client.connected.load(Ordering::Relaxed),
                "an outage nobody was told of still reads as connected",
            );

            shared.set_connection_restored();
            client.dispatch_once(py, &shared).unwrap();
            assert!(
                client.connected.load(Ordering::Relaxed),
                "a session that came back still reads as disconnected",
            );
        });
    }

    /// A stream numbered past what the request carries is refused rather than
    /// narrowed. Narrowed, the venue's refusal of it came back under a number
    /// another request was using, and this one stayed open with nothing to
    /// withdraw it by.
    #[test]
    fn a_tick_by_tick_stream_is_refused_a_req_id_the_request_cannot_carry() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            let contract = Py::new(py, Contract {
                con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
                ..Default::default()
            }).unwrap();
            let err = client
                .call_method1(
                    py, "req_tick_by_tick_data",
                    (u32::MAX as i64 + 1, &contract, "Last", 0i32, false),
                )
                .unwrap_err();
            assert!(err.to_string().contains("outside the range"), "got {err}");
            assert!(rx.try_recv().is_err(), "a refused req_id must reach no engine command");
        });
    }

    /// A withdrawal during registration is taken, and the registration takes
    /// the stream back down rather than publishing it.
    #[test]
    fn a_python_tick_withdrawal_during_registration_takes_the_stream_back_down() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, wrapper) = wired_client(py);
            // The handshake below holds the reply while the withdrawal is
            // tried, and registration waits on that reply. Tests default the
            // wait to a millisecond, which the handshake cannot finish inside:
            // it would time out, drop the reply's receiver, and both threads
            // panic on the send and the join. Wide enough that the wait is the
            // window under test, not a race the test loses.
            client.get().core.set_registration_timeout(std::time::Duration::from_secs(5));
            let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
            let (go_tx, go_rx) = std::sync::mpsc::sync_channel(1);
            let engine = std::thread::spawn(move || {
                match rx.recv().unwrap() {
                    ControlCommand::SubscribeTbt { reply_tx: Some(reply), .. } => {
                        seen_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                        let _ = reply.send(Ok(0));
                    }
                    other => panic!("expected a tick subscription: {other:?}"),
                }
                rx
            });
            std::thread::scope(|scope| {
                let core = &client.get().core;
                let tx = client.get().control_tx.lock().unwrap().clone().unwrap();
                let taking = scope.spawn(move || core.register_tbt(
                    &shared, &tx, 7, 756733, "SPY", "STK", "SMART", TbtType::AllLast, 0, false,
                ));
                py.detach(move || seen_rx.recv().unwrap());
                assert!(
                    !core.withdraw_while_registering(7),
                    "a tick claim is distinct from a quote claim",
                );
                client.call_method1(py, "cancel_tick_by_tick_data", (7,)).unwrap();
                go_tx.send(()).unwrap();
                assert_eq!(
                    taking.join().unwrap().unwrap(), None,
                    "the stream was published under a number whose caller had \
                     already been told it was withdrawn",
                );
            });
            let rx = engine.join().unwrap();
            let heard = wrapper.bind(py).getattr("calls").unwrap()
                .extract::<Vec<(String, i64, i64, i64, String, String)>>().unwrap();
            assert!(
                heard.is_empty(),
                "the withdrawal was refused for arriving early, which the venue \
                 never does: {heard:?}",
            );
            assert!(
                client.get().core.tbt_to_instrument.lock().unwrap().get(&7).is_none(),
                "a record was left behind for a stream the caller withdrew",
            );
            assert!(
                matches!(rx.try_recv().unwrap(), ControlCommand::UnsubscribeTbt { req_id: 7, instrument: 0 }),
                "the stream the registration opened was never taken back down",
            );
        });
    }

    /// A subscription states the contract's security type, and a caller that
    /// gave an id alone stated none. Sent as it stands, the engine takes the
    /// request, finds no type for it, and gives it up — so the caller reads no
    /// quotes and is told nothing under its own request id.
    #[test]
    fn a_quote_request_names_a_contract_given_by_id_alone() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, _w) = wired_client(py);
            let venue = naming_the_contract(rx, shared, a_future_on_cme());
            let contract = Py::new(py, Contract { con_id: 756733, ..Default::default() }).unwrap();

            client
                .call_method1(py, "req_mkt_data", (1i64, &contract, "", false, false))
                .unwrap();

            let (asked, rx) = venue.join().unwrap();
            assert!(
                matches!(asked.last(), Some(ControlCommand::FetchContractDetails { .. })),
                "the venue was never asked to name the contract: {asked:?}",
            );
            let subscribed = std::iter::from_fn(|| rx.try_recv().ok())
                .find_map(|cmd| match cmd {
                    ControlCommand::Subscribe { contract, .. } => Some(contract),
                    _ => None,
                })
                .expect("the quotes were never asked for");
            assert_eq!(subscribed.sec_type, "FUT", "the subscription went out untyped");
            assert_eq!(subscribed.exchange, "CME");
        });
    }

    /// The same for a historical request, which states the type too. An empty
    /// one is asked as a US stock, so a future named by id alone was answered
    /// with some share's bars.
    #[test]
    fn a_historical_request_names_a_contract_given_by_id_alone() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, _w) = wired_client(py);
            let venue = naming_the_contract(rx, shared, a_future_on_cme());
            let contract = Py::new(py, Contract { con_id: 756733, ..Default::default() }).unwrap();

            client
                .call_method1(
                    py, "req_historical_data",
                    (1i64, &contract, "", "1 D", "1 hour", "TRADES", 1i32),
                )
                .unwrap();

            let (asked, rx) = venue.join().unwrap();
            assert!(
                matches!(asked.last(), Some(ControlCommand::FetchContractDetails { .. })),
                "the venue was never asked to name the contract: {asked:?}",
            );
            let ControlCommand::FetchHistorical { contract, .. } =
                rx.try_recv().expect("the bars were never asked for")
            else {
                panic!("a historical request sent something else");
            };
            assert_eq!(contract.sec_type, "FUT", "the query went out as a US stock");
            assert_eq!(contract.exchange, "CME");
        });
    }

    /// The contract a caller names by id alone, as the venue names it back.
    fn a_future_on_cme() -> crate::control::contracts::ContractDefinition {
        crate::control::contracts::ContractDefinition {
            con_id: 756733,
            sec_type: crate::control::contracts::SecurityType::Future,
            exchange: "CME".into(),
            ..Default::default()
        }
    }

    /// Stand in for the venue's contract lookup: answer the first one that
    /// reaches the engine with `def`, and hand back what was sent and the
    /// channel, so the caller can read whatever followed the lookup.
    fn naming_the_contract(
        rx: std::sync::mpsc::Receiver<ControlCommand>,
        shared: Arc<SharedState>,
        def: crate::control::contracts::ContractDefinition,
    ) -> thread::JoinHandle<(Vec<ControlCommand>, std::sync::mpsc::Receiver<ControlCommand>)> {
        thread::spawn(move || {
            let mut sent = Vec::new();
            while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_secs(10)) {
                let lookup = matches!(cmd, ControlCommand::FetchContractDetails { .. });
                if let ControlCommand::FetchContractDetails { req_id, .. } = &cmd {
                    shared.reference.push_contract_details(*req_id, def.clone());
                    shared.reference.push_contract_details_end(*req_id);
                }
                sent.push(cmd);
                if lookup { break; }
            }
            (sent, rx)
        })
    }

    /// `all_msgs=False` asks for the bulletins still to come, and the ones the
    /// session collected before the call are not those. Kept, the first poll
    /// after subscribing opened with a notice published before anyone had
    /// asked for any.
    #[test]
    fn asking_only_for_the_bulletins_to_come_drops_the_ones_already_here() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            let bulletin = |msg_id, message: &str| NewsBulletin {
                msg_id, msg_type: 1, message: message.into(), exchange: "NYSE".into(),
            };
            shared.market.push_news_bulletin(bulletin(1, "published before anyone asked"));

            client.call_method1(py, "req_news_bulletins", (false,)).unwrap();
            client.borrow(py).dispatch_once(py, &shared).unwrap();

            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            let heard = || -> usize {
                py.eval(
                    c"len([c for c in w.calls if c[0] in ('update_news_bulletin', 'updateNewsBulletin')])",
                    Some(&g), None,
                ).unwrap().extract().unwrap()
            };
            assert_eq!(heard(), 0, "a bulletin from before the call was handed over");

            // And what the venue broadcasts after it still reaches the caller.
            shared.market.push_news_bulletin(bulletin(2, "published after"));
            client.borrow(py).dispatch_once(py, &shared).unwrap();
            assert_eq!(heard(), 1, "the subscription answered nothing");
        });
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn a_callback_replacing_the_session_ends_the_old_dispatch_pass() {
        #[pyfunction]
        fn answer_subscriptions(client: &EClient) {
            let rx = client._test_control_rx.lock().unwrap().take().unwrap();
            *client._thread.lock().unwrap() = Some(thread::spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        ControlCommand::Subscribe { reply_tx: Some(reply), .. } => {
                            let _ = reply.send(Ok(0));
                        }
                        ControlCommand::Shutdown => break,
                        _ => {}
                    }
                }
            }));
        }
        Python::initialize();
        Python::attach(|py| {
            for boundary in ["managedAccounts", "tickReqParams"] {
                let (client, rx, _shared, w) = wired_client(py);
                *client.get()._test_control_rx.lock().unwrap() = Some(rx);
                answer_subscriptions(client.get());
                let g = pyo3::types::PyDict::new(py);
                g.set_item("client", &client).unwrap();
                g.set_item("w", &w).unwrap();
                g.set_item("Contract", py.get_type::<Contract>()).unwrap();
                g.set_item("boundary", boundary).unwrap();
                g.set_item("answer_subscriptions", wrap_pyfunction!(answer_subscriptions, py).unwrap()).unwrap();
                py.run(c"
client._test_set_instrument_count(1)
a = Contract()
a.conId, a.symbol, a.secType, a.exchange = 100, 'AAA', 'STK', 'SMART'
client.req_mkt_data(1, a)
client._test_push_quote(0, bid=123)
def replace_session(*args):
    client.disconnect()
    client._test_connect()
    answer_subscriptions(client)
    client._test_set_instrument_count(1)
    b = Contract()
    b.conId, b.symbol, b.secType, b.exchange = 200, 'BBB', 'STK', 'SMART'
    client.req_mkt_data(9, b)
    client.req_managed_accts()
setattr(w, boundary, replace_session)
", Some(&g), None).unwrap();
                if boundary == "managedAccounts" {
                    client.call_method0(py, "req_managed_accts").unwrap();
                } else {
                    client.get().shared_state().unwrap().market.push_tick_req_params_for(1, 0.01);
                }
                client.call_method0(py, "poll").unwrap();
                assert_eq!(client.get().core.watching(9), Some(0), "the callback installs the new subscription");
                assert_eq!(client.get().waiting_answers.lock().unwrap().len(), 1,
                    "the new session's answer waits for its own pass");
                let prices: Vec<(i64, f64)> = py.eval(
                    c"[(c[1], c[3]) for c in w.calls if c[0] in ('tickPrice', 'tick_price')]",
                    Some(&g), None,
                ).unwrap().extract().unwrap();
                assert!(prices.is_empty(), "the old session delivered prices after {boundary}: {prices:?}");
                w.bind(py).delattr(boundary).unwrap();
                client.call_method1(py, "_test_push_quote", (0, 456.0)).unwrap();
                client.call_method0(py, "poll").unwrap();
                let prices: Vec<(i64, f64)> = py.eval(
                    c"[(c[1], c[3]) for c in w.calls if c[0] in ('tickPrice', 'tick_price')]",
                    Some(&g), None,
                ).unwrap().extract().unwrap();
                assert!(prices.contains(&(9, 456.0)), "the next pass delivers the new session's quote");
            }
        });
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn an_abandoned_connect_leaves_the_new_connections_claim_alone() {
        Python::initialize();
        Python::attach(|py| {
            let client = Py::new(py, client_with(py, recording_wrapper(py))).unwrap();
            let listener = std::net::TcpListener::bind(("127.0.0.1", crate::config::AUTH_PORT)).unwrap();
            let reconnecting = client.clone_ref(py);
            let server = thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                Python::attach(|py| {
                    reconnecting.call_method0(py, "disconnect").unwrap();
                    reconnecting.call_method0(py, "_test_connect").unwrap();
                });
                // End the pending TLS login only after the new session stands.
                drop(socket);
            });
            let kwargs = pyo3::types::PyDict::new(py);
            kwargs.set_item("host", "127.0.0.1").unwrap();
            kwargs.set_item("settings", [("hardware_id", "0123456789abcdef")].into_iter().collect::<HashMap<_, _>>()).unwrap();
            let err = client.call_method(py, "connect", (), Some(&kwargs)).unwrap_err();
            py.detach(|| server.join().unwrap());
            assert!(err.to_string().contains("Connection abandoned"), "got {err}");
            assert!(client.get().is_connected(), "the abandoned login cleared the new session's claim");
            client.get().session_ended.store(true, Ordering::Release);
            client.call_method0(py, "run").expect("the new session is admitted to run");
        });
    }

    /// A handler that closes the session stops the rest of the startup.
    ///
    /// The announcements are a caller's own code, and giving up on a
    /// connection from inside `connect_ack` is the ordinary way to write one.
    /// Read on the session they were announced for, the rest go on describing
    /// a session that has been torn down — and a handler that reconnects has
    /// the startup of the session it just opened repeated over it.
    #[test]
    fn a_handler_that_ends_the_session_stops_the_rest_of_the_startup() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            g.set_item("client", &client).unwrap();
            // Named as the reference client names them, which is what the
            // announcement looks for first.
            py.run(c"
w.connectAck = lambda: (w.calls.append(('connectAck',)), client.disconnect())
", Some(&g), None).unwrap();

            client.get().announce_the_new_session(py, &shared).unwrap();

            let said: Vec<String> = py.eval(c"[c[0] for c in w.calls]", Some(&g), None)
                .unwrap().extract().unwrap();
            assert_eq!(
                said, ["connectAck"],
                "a session the handler closed went on announcing itself",
            );
        });
    }

    #[test]
    fn reentrant_local_answers_wait_for_the_next_dispatch_pass() {
        Python::initialize();
        Python::attach(|py| {
            let (client, _rx, shared, w) = wired_client(py);
            let g = pyo3::types::PyDict::new(py);
            g.set_item("client", &client).unwrap();
            g.set_item("w", &w).unwrap();
            py.run(c"
answers = []
def managed(accounts):
    answers.append(accounts)
    if len(answers) < 4:
        client.req_managed_accts()
w.managedAccounts = managed
client.req_managed_accts()
", Some(&g), None).unwrap();
            shared.market.push_tick_req_params_for(7, 0.01);
            for expected in 1..=4 {
                client.call_method0(py, "poll").unwrap();
                assert_eq!(g.get_item("answers").unwrap().unwrap().len().unwrap(), expected,
                    "a callback's request is answered on the next pass");
                assert_eq!(client.get().waiting_answers.lock().unwrap().len(), usize::from(expected < 4));
                assert_eq!(w.getattr(py, "calls").unwrap().bind(py).len().unwrap(), 1,
                    "the engine event behind the local answer is delivered on the first pass");
            }
        });
    }

    #[test]
    fn a_completed_preview_is_taken_before_its_callback_runs() {
        Python::initialize();
        Python::attach(|py| {
            for interrupt in [false, true] {
                let (client, rx, shared, w) = wired_client(py);
                shared.market.set_instrument_count(1);
                shared.orders.set_replay_done();
                client.get().core.con_id_to_instrument.lock().unwrap().insert(756733, 0);
                client.get().core.track_order(
                    42,
                    ApiContract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), ..Default::default() },
                    ApiOrder { order_id: 42, action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 10.0, what_if: true, ..Default::default() },
                    0,
                );
                shared.orders.push_what_if(WhatIfResponse { order_id: 42, ..Default::default() });
                let g = pyo3::types::PyDict::new(py);
                g.set_item("client", &client).unwrap();
                g.set_item("w", &w).unwrap();
                g.set_item("interrupt", interrupt).unwrap();
                py.run(c"
received = []
def preview(order_id, contract, order, state):
    received.append((order_id, contract.conId, order.whatIf))
    if interrupt:
        raise KeyboardInterrupt()
    order.whatIf = False
    client.place_order(order_id, contract, order)
w.openOrder = preview
", Some(&g), None).unwrap();
                let result = client.call_method0(py, "poll");
                let received: Vec<(i64, i64, bool)> = g.get_item("received").unwrap().unwrap().extract().unwrap();
                assert_eq!(received, [(42, 756733, true)], "the callback is built from the completed preview");
                if interrupt {
                    assert!(result.unwrap_err().is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py));
                    assert!(client.get().core.tracked_order(42).is_none(), "an interrupt leaves no completed preview tracked");
                } else {
                    result.unwrap();
                    assert!(matches!(rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { order_id: 42, .. }))),
                        "placing the preview's number must submit a new order");
                    assert!(!client.get().core.tracked_order(42).unwrap().what_if,
                        "the new placement remains tracked after the callback");
                }
            }
        });
    }

    #[test]
    fn a_bound_authentication_provider_is_collected_and_its_engine_stopped() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, _shared, _w) = wired_client(py);
            let app = py.eval(c"type('App', (), {'code': lambda self, *args: '123456'})()", None, None).unwrap();
            app.setattr("client", &client).unwrap();
            let weak_app = py.import("weakref").unwrap().getattr("ref").unwrap().call1((&app,)).unwrap();
            let provider = client.get().code_provider_from_py(Some(app.getattr("code").unwrap().unbind())).unwrap();
            assert_eq!(provider(IbKeyChallenge::default()).unwrap(), "123456");
            let reconnect_provider = provider.clone();
            let heard = Arc::new(Mutex::new(Vec::new()));
            let heard_here = heard.clone();
            *client.get()._thread.lock().unwrap() = Some(thread::spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    let stop = matches!(cmd, ControlCommand::Shutdown);
                    heard_here.lock().unwrap().push(cmd);
                    if stop { break; }
                }
                drop(reconnect_provider);
            }));
            drop(app);
            drop(client);
            crate::python::collect_weakref(py, &weak_app).unwrap();
            assert!(weak_app.call0().unwrap().is_none(), "the bound provider kept its application and client alive");
            assert!(matches!(heard.lock().unwrap()[..], [ControlCommand::Logout, ControlCommand::Shutdown]),
                "collecting the client stops its engine");
            assert!(provider(IbKeyChallenge::default()).unwrap_err().to_string().contains("no longer available"));
        });
    }

    /// A refused model calculation does not claim the watch the request was
    /// already holding.
    ///
    /// A request already watching something is refused a second watch, so a
    /// slot found after the refused subscribe is the other contract's. Read as
    /// this calculation's, the question was kept against a contract the venue
    /// never answered for — and cancelling it withdrew the caller's own
    /// subscription. A contract stated by description carries no id to tell
    /// the two apart, so what the request held before is what says it.
    #[test]
    fn a_refused_model_does_not_claim_the_watch_the_request_already_held() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, shared, w) = wired_client(py);
            shared.market.set_instrument_count(1);
            let engine = thread::spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    if let ControlCommand::Subscribe { reply_tx: Some(reply), .. } = cmd {
                        let _ = reply.send(Ok(0));
                        return rx;
                    }
                }
                panic!("the stock subscription must reach the engine");
            });
            client.get().req_mkt_data(py, 7, &Contract {
                con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
                exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
            }, "", false, false, Vec::new()).unwrap();
            let rx = py.detach(|| engine.join().unwrap());
            assert_eq!(client.get().core.watching(7), Some(0));

            // Stated by description: no id to compare the request's own slot
            // against, and another contract besides.
            let option = Py::new(py, Contract {
                symbol: "QQQ".into(), sec_type: "OPT".into(), exchange: "SMART".into(),
                currency: "USD".into(), last_trade_date_or_contract_month: "20261218".into(),
                strike: 100.0, right: "C".into(), multiplier: "100".into(), ..Default::default()
            }).unwrap();
            let g = pyo3::types::PyDict::new(py);
            g.set_item("w", &w).unwrap();
            py.run(c"
errors = []
w.error = lambda *a: errors.append(a)
", Some(&g), None).unwrap();

            client.call_method1(py, "calculate_option_price", (7, &option, 1.0, 100.0))
                .expect("the refusal is reported to the caller, not raised");
            let codes: Vec<i32> = py.eval(c"[e[2] for e in errors]", Some(&g), None).unwrap().extract().unwrap();
            assert_eq!(codes, [crate::error_codes::DUPLICATE_TICKER_ID]);
            assert!(
                client.get().pending_option_calcs.lock().unwrap().is_empty(),
                "a refused calculation was kept against a watch it never opened",
            );

            client.call_method1(py, "cancel_calculate_option_price", (7,)).unwrap();
            assert_eq!(
                client.get().core.watching(7), Some(0),
                "cancelling the refused calculation withdrew the caller's own subscription",
            );
            assert!(rx.try_recv().is_err(), "and sent its unsubscribe to the engine");
        });
    }

    #[test]
    fn option_calculations_propagate_interrupts_from_opening_the_model_watch() {
        Python::initialize();
        Python::attach(|py| {
            for method in ["calculate_implied_volatility", "calculate_option_price"] {
                for exception in ["KeyboardInterrupt", "SystemExit"] {
                    let (client, rx, shared, w) = wired_client(py);
                    shared.market.set_instrument_count(1);
                    let engine = thread::spawn(move || {
                        while let Ok(cmd) = rx.recv() {
                            if let ControlCommand::Subscribe { reply_tx: Some(reply), .. } = cmd {
                                let _ = reply.send(Ok(0));
                                return rx;
                            }
                        }
                        panic!("the stock subscription must reach the engine");
                    });
                    client.get().req_mkt_data(py, 7, &Contract {
                        con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
                        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
                    }, "", false, false, Vec::new()).unwrap();
                    let rx = py.detach(|| engine.join().unwrap());
                    assert_eq!(client.get().core.watching(7), Some(0));
                    let option = Py::new(py, Contract {
                        con_id: 100, symbol: "SPY".into(), sec_type: "OPT".into(),
                        exchange: "SMART".into(), currency: "USD".into(),
                        last_trade_date_or_contract_month: "20261218".into(), strike: 100.0,
                        right: "C".into(), multiplier: "100".into(), ..Default::default()
                    }).unwrap();
                    let g = pyo3::types::PyDict::new(py);
                    g.set_item("w", &w).unwrap();
                    g.set_item("exception", exception).unwrap();
                    py.run(c"
import builtins
errors = []
def error(*args):
    errors.append(args)
    raise getattr(builtins, exception)('stop option calculation')
w.error = error
", Some(&g), None).unwrap();
                    let err = client.call_method1(py, method, (7, &option, 1.0, 100.0))
                        .expect_err("the model subscription's interrupt must leave the calculation");
                    assert_eq!(err.get_type(py).name().unwrap(), exception);
                    let codes: Vec<i32> = py.eval(c"[e[2] for e in errors]", Some(&g), None).unwrap().extract().unwrap();
                    assert_eq!(codes, [crate::error_codes::DUPLICATE_TICKER_ID]);
                    assert!(shared.reference.drain_historical_errors_for_dispatch(|_| false).is_empty(),
                        "the interrupt must not queue another refusal");
                    assert!(client.get().pending_option_calcs.lock().unwrap().is_empty());
                    assert_eq!(client.get().core.watching(7), Some(0));
                    assert!(rx.try_recv().is_err());
                }
            }
        });
    }
}
