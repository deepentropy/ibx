//! Bridge module: shared state and events between the HotLoop and external callers.
//!
//! Architecture:
//! - `SharedState` composes four domain-specific containers:
//!   - `MarketDataState` — lock-free quotes (SeqLock), TBT, real-time bars, news ticks.
//!   - `OrderState` — fills, order updates, cancel rejects, what-if, order cache.
//!   - `ReferenceState` — historical data, contracts, scanners, news archives, market rules.
//!   - `PortfolioState` — account snapshot, position info, atomic positions.
//! - `Event` enum carries all events through a crossbeam channel for the `EClient` API.
//! - The HotLoop pushes to SharedState sub-containers directly.
//! - External callers read snapshots and poll events without blocking the hot loop.

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use std::collections::HashMap;
use crate::control::historical::{HistoricalBar, HistoricalResponse, HeadTimestampResponse};
use crate::control::contracts::{ContractDefinition, SymbolMatch};
use crate::control::scanner::ScannerResult;
use crate::control::news::NewsHeadline;
use crate::control::histogram::HistogramEntry;
use crate::control::contracts::MarketRule;
use crate::types::*;
use crate::api::types as api;

/// Enriched order info from CCP execution reports, for open_order / completed_order callbacks.
#[derive(Clone, Debug)]
pub struct RichOrderInfo {
    pub contract: api::Contract,
    pub order: api::Order,
    pub order_state: api::OrderState,
    /// Last execution details from this order's exec reports.
    pub last_exec: api::Execution,
}

/// What a fill report says about its execution, beyond the `Fill` numbers
/// (ibx#471 ibx#474). Carried with each fill, so two fills of one order in
/// the same batch keep their own values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FillExec {
    /// Server execution id (tag 17).
    pub exec_id: String,
    /// Execution time, Unix seconds: tag 6699, else 60, else 52.
    pub time_secs: Option<i64>,
    /// Tag 100, else 207.
    pub exchange: String,
    /// Placing client (tag 6119); 0 when absent.
    pub client_id: i64,
    /// Tag 6700.
    pub model_code: String,
    /// Tag 6010.
    pub order_ref: String,
    /// Tag 851 (lastLiquidity); 0 when absent.
    pub last_liquidity: i32,
    /// A report of a combo order (ibx#470): the contract its execution
    /// shows, and for a leg report the leg's own execution values.
    pub combo: Option<Box<ComboExec>>,
    /// An execution of an order of another API client: kept for
    /// `req_executions`, with no live callback and no commission report
    /// (`jextend.ba.a(dq, aQ)`: the reports go to the order's client).
    pub other_client: bool,
    /// An execution the server gave again (97=Y: the replay of the day at
    /// the logon, the fill-up after a reconnect): kept for
    /// `req_executions`, with no live callback and no commission report,
    /// as the reference's API clients get none for them (ibx#487, captured
    /// 07/10/2026 and 30/09/2026).
    pub replayed: bool,
}

/// The execution of a combo report (ibx#470): the reference shows the
/// combo contract on the report of the combo, the leg's contract and its
/// own side, size and prices on the report of a leg.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComboExec {
    pub contract: api::Contract,
    pub leg: Option<LegExec>,
}

/// What the report of one leg of a combo fill says (ibx#470).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LegExec {
    /// BOT or SLD.
    pub side: String,
    pub shares: f64,
    pub price: f64,
    pub cum_qty: f64,
    pub avg_price: f64,
}

/// What the reference shows of a combo order in openOrder (ibx#470): the
/// combo contract with its legs, and the per-leg prices in the contract's
/// leg order, f64::MAX for a leg without one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComboView {
    pub contract: api::Contract,
    pub leg_prices: Vec<f64>,
}

/// The event channel as the error queues see it (ibx#498): unset, and
/// free, until a channel is attached.
#[derive(Default)]
struct ErrorTap {
    events: std::sync::OnceLock<crossbeam_channel::Sender<Event>>,
    /// Wakes the consumer that waits for data: an error is written by the
    /// engine or by the thread of the caller (a request refused locally),
    /// and the second has no pass of the engine behind it (ibx#530).
    notifier: std::sync::OnceLock<Arc<Notifier>>,
    /// Keeps an error written by the thread of the caller behind the
    /// answers to the requests made before it (ibx#529).
    clock: std::sync::OnceLock<Arc<CommandClock>>,
}

impl ErrorTap {
    /// The event of an error, built only when a channel is attached. Called
    /// before the error goes to its queue: an error written by the thread
    /// of the caller first lets the engine answer the requests sent before
    /// it, so the answers come in the order of the requests (ibx#529).
    #[inline]
    fn event(&self, req_id: i64, code: i64, message: &str) -> Option<Event> {
        if let Some(clock) = self.clock.get() {
            clock.wait_for_earlier_commands();
        }
        self.events.get().map(|_| Event::Error { req_id, code, message: message.to_string() })
    }

    /// Send it once the error is in its queue, so the event never shows
    /// before the queue has it. Non-blocking, as every event: dropped when
    /// the channel is full. The waiting consumer is woken.
    /// An error written by the thread of the caller while the requests are
    /// paced: the engine takes it and writes it at its turn (ibx#561).
    #[inline]
    fn defer(&self, queue: ErrorQueue, req_id: i64, code: i64, message: &str) -> bool {
        self.clock.get().is_some_and(|clock| clock.defer_error(queue, req_id, code, message))
    }

    #[inline]
    fn send(&self, event: Option<Event>) {
        if let (Some(tx), Some(event)) = (self.events.get(), event) {
            let _ = tx.try_send(event);
        }
        if let Some(notifier) = self.notifier.get() {
            notifier.notify();
        }
    }

    fn attach(&self, tx: &crossbeam_channel::Sender<Event>) {
        let _ = self.events.set(tx.clone());
    }

    fn wakes(&self, notifier: &Arc<Notifier>) {
        let _ = self.notifier.set(notifier.clone());
    }

    fn follows(&self, clock: &Arc<CommandClock>) {
        let _ = self.clock.set(clock.clone());
    }
}

thread_local! {
    /// This thread runs the loop of an engine.
    static ON_ENGINE_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The commands sent to the engine and those it has handled, counted
/// (ibx#529). The reference answers the requests of a client in the order
/// they were made. Here a request refused locally is answered by the thread
/// of the caller and the others by the engine: the first waits until the
/// engine has handled every command sent before it.
#[derive(Default)]
pub struct CommandClock {
    sent: AtomicU64,
    handled: AtomicU64,
    /// The loop of the engine runs on its thread.
    running: AtomicBool,
    /// The engine paces the requests: an answer of the client itself is
    /// given to it and comes out at the turn of its request (ibx#561).
    paced: AtomicBool,
    /// Those answers, each with the count of commands sent before it.
    deferred: Mutex<Vec<(u64, ControlCommand)>>,
    waker: std::sync::OnceLock<Arc<crate::engine::park::Waker>>,
}

impl CommandClock {
    /// Longest wait for the engine. Past it the error is written anyway: an
    /// answer late is better than a caller held.
    const LIMIT: std::time::Duration = std::time::Duration::from_millis(20);

    /// A command went to the engine.
    #[inline]
    pub(crate) fn note_sent(&self) {
        self.sent.fetch_add(1, Ordering::Release);
    }

    /// The engine handled `count` commands, their answers written.
    #[inline]
    pub(crate) fn note_handled(&self, count: u64) {
        if count > 0 {
            self.handled.fetch_add(count, Ordering::Release);
        }
    }

    /// The calling thread starts (`true`) or ends (`false`) the loop of the
    /// engine.
    pub(crate) fn set_running(&self, on: bool) {
        ON_ENGINE_THREAD.with(|flag| flag.set(on));
        self.running.store(on, Ordering::Release);
        if !on {
            // Nothing takes them any more: a call that waits for its turn
            // goes on.
            self.deferred.lock().unwrap().clear();
        }
    }

    /// The engine paces the requests, and is woken by `waker`.
    pub(crate) fn set_paced(&self, waker: Option<Arc<crate::engine::park::Waker>>) {
        if let Some(waker) = waker {
            let _ = self.waker.set(waker);
        }
        self.paced.store(true, Ordering::Release);
    }

    /// Whether an answer of the calling thread goes through the engine: the
    /// requests are paced, the loop runs, and this is not its thread.
    #[inline]
    fn takes_turns(&self) -> bool {
        self.paced.load(Ordering::Acquire) && self.running.load(Ordering::Acquire)
            && !ON_ENGINE_THREAD.with(|flag| flag.get())
    }

    /// Give the engine a command of the client that follows the commands
    /// sent so far.
    fn defer(&self, cmd: ControlCommand) {
        let after = self.sent.load(Ordering::Acquire);
        self.deferred.lock().unwrap().push((after, crate::engine::park::scoped(cmd)));
        if let Some(waker) = self.waker.get() {
            waker.wake();
        }
    }

    /// The commands given by `defer` since the last call.
    pub(crate) fn take_deferred(&self) -> Vec<(u64, ControlCommand)> {
        let mut deferred = self.deferred.lock().unwrap();
        if deferred.is_empty() { Vec::new() } else { std::mem::take(&mut *deferred) }
    }

    /// An error of the client itself: true when the engine takes it and
    /// writes it at the turn of its request.
    #[inline]
    fn defer_error(&self, queue: ErrorQueue, req_id: i64, code: i64, message: &str) -> bool {
        if !self.takes_turns() {
            return false;
        }
        self.defer(ControlCommand::LocalError { queue, req_id, code, message: message.to_string() });
        true
    }

    /// Longest wait for a turn: a queue of fifty requests is emptied in a
    /// few seconds.
    const TURN_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// A request answered by the calling thread waits here for its turn
    /// among the requests made before it, as on the reference, where every
    /// request goes through one paced queue (ibx#561). At once when the
    /// requests are not paced.
    pub fn take_turn(&self) {
        if !self.takes_turns() {
            return;
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.defer(ControlCommand::Turn { reply_tx });
        let _ = reply_rx.recv_timeout(Self::TURN_LIMIT);
    }

    /// Wait until the engine has handled the commands sent so far. At once
    /// on the thread of the engine, with no engine running, and with
    /// nothing waiting, which is the usual case.
    #[inline]
    fn wait_for_earlier_commands(&self) {
        let target = self.sent.load(Ordering::Acquire);
        if self.handled.load(Ordering::Acquire) >= target || !self.running.load(Ordering::Acquire) {
            return;
        }
        self.wait_until_handled(target);
    }

    #[cold]
    fn wait_until_handled(&self, target: u64) {
        if ON_ENGINE_THREAD.with(|flag| flag.get()) {
            return;
        }
        let deadline = Instant::now() + Self::LIMIT;
        loop {
            for _ in 0..64 {
                if self.handled.load(Ordering::Acquire) >= target || !self.running.load(Ordering::Acquire) {
                    return;
                }
                std::hint::spin_loop();
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::yield_now();
        }
    }
}

/// The wake of a consumer that waits for data (e.g. the Python event loop).
#[derive(Default)]
struct Notifier {
    pending: Mutex<bool>,
    condvar: Condvar,
}

impl Notifier {
    fn notify(&self) {
        let mut pending = self.pending.lock().unwrap();
        *pending = true;
        self.condvar.notify_one();
    }

    /// Returns true if notified, false if timed out.
    fn wait(&self, timeout: std::time::Duration) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if *pending {
            *pending = false;
            return true;
        }
        let (mut pending, result) = self.condvar.wait_timeout(pending, timeout).unwrap();
        let had_data = std::mem::take(&mut *pending);
        had_data || !result.timed_out()
    }
}

/// Events emitted by the IB engine.
///
/// New variants may be added: match with a wildcard arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// Market data tick received. Read the latest quote via `Client::quote()`.
    Tick(InstrumentId),
    /// Order filled (partial or full).
    Fill(Fill),
    /// Order status changed.
    OrderUpdate(OrderUpdate),
    /// Cancel or modify request rejected.
    CancelReject(CancelReject),
    /// Tick-by-tick trade data.
    TbtTrade(TbtTrade),
    /// Tick-by-tick bid/ask quote.
    TbtQuote(TbtQuote),
    /// Tick-by-tick midpoint.
    TbtMidPoint(TbtMidPoint),
    /// What-if order response (margin/commission preview).
    WhatIf(WhatIfResponse),
    /// Real-time news headline.
    News(TickNews),
    /// Historical bar data.
    HistoricalData { req_id: ReqId, data: HistoricalResponse },
    /// Head timestamp response.
    HeadTimestamp { req_id: ReqId, data: HeadTimestampResponse },
    /// Contract details response.
    ContractDetails { req_id: ReqId, details: ContractDefinition },
    /// End of contract details for a request.
    ContractDetailsEnd(ReqId),
    /// Matching symbols answer (ibx#387): one per answered request, an empty
    /// list when nothing matches. A refused or failed request comes as an
    /// error and a replaced or lost one gets no answer, so none of them
    /// shows here.
    SymbolSamples { req_id: ReqId, matches: Vec<SymbolMatch> },
    /// An error or a notice, as the wrapper's `error` gets it (ibx#498):
    /// `req_id` is the request id, the order id (as in `OrderUpdate`) for
    /// an order, and -1 for a link status message. A refused market data
    /// request is not given here.
    Error { req_id: i64, code: i64, message: String },
    /// Position update.
    /// `position` is fixed-point (QTY_SCALE).
    PositionUpdate { instrument: InstrumentId, con_id: i64, position_fixed: Qty, avg_cost: Price },
    /// Connection lost.
    Disconnected,
    /// Gateway logon completed. `ccp_session_id` matches the `x-ccp-session-id` header
    /// expected by webapp REST endpoints. `misc_urls` maps logical names (e.g. `region_dam`)
    /// to host URLs as pushed by the gateway during logon. The map is empty when the
    /// gateway does not push a URL set; callers should fall back to a documented literal
    /// (e.g. `api.ibkr.com`) in that case.
    GatewayLogon {
        ccp_session_id: String,
        misc_urls: HashMap<String, String>,
    },
}

/// Number of 8-byte words in a `Quote` payload (its fields, not its padding).
const QUOTE_WORDS: usize = 15;

/// Lists every `Quote` field once with its word slot. The load side builds
/// the `Quote` with a struct literal, so a field missing here fails to compile.
macro_rules! quote_words {
    ($m:ident) => {
        $m!(
            0 bid i64, 1 ask i64, 2 last i64,
            3 bid_size i64, 4 ask_size i64, 5 last_size i64, 6 volume i64,
            7 open i64, 8 high i64, 9 low i64, 10 close i64,
            11 timestamp_ns u64,
            12 bid_exch_mask i64, 13 ask_exch_mask i64, 14 last_exch_mask i64
        )
    };
}

/// SeqLock-protected quote slot. Writer (hot loop) never blocks.
/// Reader retries if it catches a write in progress.
///
/// The payload is held as atomic words accessed with `Relaxed` ordering. On
/// the usual targets these compile to plain loads and stores; a reader that
/// overlaps a write reads stale or mixed words, which the version check
/// rejects, instead of racing a non-atomic access.
///
/// Ordering (single writer, any number of readers):
/// - writer: odd version (Relaxed), Release fence, payload (Relaxed), even
///   version (Release). The fence keeps the payload stores after the odd mark.
/// - reader: version v1 (Acquire), payload (Relaxed), Acquire fence, version
///   v2 (Relaxed). If any payload word read comes from a write in progress,
///   the fence pair makes that write's odd mark visible to the v2 load, so
///   v2 != v1 and the snapshot is retried. An accepted snapshot (v1 even and
///   v1 == v2) is the full payload of one write.
#[repr(C, align(64))]
pub struct SeqQuote {
    version: AtomicU64,
    data: [AtomicU64; QUOTE_WORDS],
}

impl SeqQuote {
    pub fn new() -> Self {
        let s = Self {
            version: AtomicU64::new(0),
            data: std::array::from_fn(|_| AtomicU64::new(0)),
        };
        s.store_payload(&Quote::default());
        s
    }

    #[inline(always)]
    fn store_payload(&self, q: &Quote) {
        macro_rules! store {
            ($($i:literal $f:ident $t:ty),*) => {
                $( self.data[$i].store(q.$f as u64, Ordering::Relaxed); )*
            };
        }
        quote_words!(store);
    }

    #[inline(always)]
    fn load_payload(&self) -> Quote {
        macro_rules! load {
            ($($i:literal $f:ident $t:ty),*) => {
                Quote { $( $f: self.data[$i].load(Ordering::Relaxed) as $t, )* }
            };
        }
        quote_words!(load)
    }

    /// Write a quote (hot loop side). Never blocks. Single writer only.
    #[inline]
    pub fn write(&self, quote: &Quote) {
        let v = self.version.load(Ordering::Relaxed);
        self.version.store(v.wrapping_add(1), Ordering::Relaxed); // odd = writing
        fence(Ordering::Release);
        self.store_payload(quote);
        self.version.store(v.wrapping_add(2), Ordering::Release); // even = stable
    }

    /// Read a consistent quote snapshot (reader side). Spins on conflict.
    #[inline]
    pub fn read(&self) -> Quote {
        loop {
            let v1 = self.version.load(Ordering::Acquire);
            if v1 & 1 != 0 { // writer active
                std::hint::spin_loop();
                continue;
            }
            let q = self.load_payload();
            fence(Ordering::Acquire);
            let v2 = self.version.load(Ordering::Relaxed);
            if v1 == v2 { return q; }
        }
    }
}

// ── Domain-specific state containers ──

/// News bulletins of the session (ibx#461): the store of the day, replayed
/// on request, and the ones not yet handed to the client.
#[derive(Default)]
struct BulletinStore {
    store: Vec<NewsBulletin>,
    queue: Vec<NewsBulletin>,
    day: Option<jiff::civil::Date>,
}

/// The API ticks of one generic tick block of a contract (ibx#450), for the
/// requests that asked that tick: `at` is the market data queue position
/// when it was read, so the requests get it between the steps of the farm
/// messages around it.
#[derive(Debug, Clone)]
pub struct GenericTicks {
    pub at: u64,
    pub instrument: InstrumentId,
    /// The request code of the tick (233 and 375 go to the requests of
    /// either).
    pub code: i32,
    pub ticks: Vec<crate::control::generic_values::GenTick>,
}

/// Lock-free quotes, TBT streams, real-time bars, depth updates, and news ticks.
pub struct MarketDataState {
    quotes: Box<[SeqQuote; MAX_INSTRUMENTS]>,
    /// The steps of each farm message for the API client's market data
    /// requests, in their order (ibx#446).
    pub md_events: crate::md_events::MdQueue,
    /// InstrumentId counter — set by hot loop on RegisterInstrument.
    instrument_count: AtomicU64,
    tbt_trades: Mutex<Vec<TbtTrade>>,
    tbt_quotes: Mutex<Vec<TbtQuote>>,
    tbt_mid_points: Mutex<Vec<TbtMidPoint>>,
    real_time_bars: Mutex<Vec<(ReqId, RealTimeBar)>>,
    depth_updates: Mutex<Vec<DepthUpdate>>,
    tick_news: Mutex<Vec<TickNews>>,
    /// The API ticks of generic tick blocks (ibx#450), with their place in
    /// `md_events`.
    generic_ticks: Mutex<Vec<GenericTicks>>,
    news_bulletins: Mutex<BulletinStore>,
    /// Subscriptions the market data server rejected (ibx#444, ibx#447).
    md_rejects: Mutex<Vec<MdReject>>,
    /// Requests given without a conId whose contract another request had
    /// subscribed: (their own slot, the slot they joined) (ibx#444).
    md_merges: Mutex<Vec<(InstrumentId, InstrumentId, u64)>>,
    /// Slots of requests without a conId whose contract was found, with
    /// its conId (ibx#444).
    md_resolved: Mutex<Vec<(InstrumentId, i64)>>,
    /// The request parameters of acked subscriptions (ibx#449).
    tick_req_params: Mutex<Vec<TickReqParams>>,
    snapshot_acks: Mutex<Vec<TickReqParams>>,
    /// Tick-by-tick requests that ended with an error: the request, the
    /// code and the whole text (ibx#455).
    tbt_errors: Mutex<Vec<(ReqId, i32, String)>>,
    error_tap: ErrorTap,
}

/// What a client reports as tickReqParams for a subscription, from its
/// bid/ask ack (ibx#449): the minimum tick, the BBO exchange code with the
/// security type code the reference appends, and the snapshot permissions
/// (0 irrelevant, 1 no top, 2 snapshot, 3 real-time top, 4 snapshot, no
/// API).
#[derive(Debug, Clone, PartialEq)]
pub struct TickReqParams {
    pub instrument: InstrumentId,
    pub min_tick: f64,
    pub bbo_exchange: String,
    pub snapshot_permissions: i32,
}

/// A top-of-book subscription the server rejected, and what the client
/// reports for it (ibx#444, ibx#447).
#[derive(Debug, Clone, PartialEq)]
pub enum MdReject {
    /// Delayed data enabled and available: the subscription went on with
    /// delayed data (marketDataType 3 and error 10167).
    Delayed { instrument: InstrumentId },
    /// The subscription stopped: error 354 (with the "delayed available"
    /// text when the server says so) or 10089 (an API subscription is
    /// needed). `description`: the contract as the error names it
    /// (`jclient.dy.cU()`), empty when not known (ibx#444).
    /// `kept_params`: the request parameters (minimum tick, BBO exchange,
    /// permissions) the contract's record kept from an earlier subscription,
    /// which the requests get before the error (captured 05/10/2026).
    NotSubscribed {
        instrument: InstrumentId, delayed_available: bool, needs_api_subscription: bool, description: String,
        kept_params: Option<(f64, String, i64)>,
    },
    /// A subscription given without a conId whose lookup found no single
    /// contract: error 200, the subscription is gone (ibx#278).
    NoSecurityDefinition { instrument: InstrumentId },
    /// A request with the news tick refused once its contract was known:
    /// error 10094 with this text, nothing was sent (ibx#458). `con_id`:
    /// its contract.
    NewsRefused { instrument: InstrumentId, con_id: i64, text: String },
    /// The contract's data is frozen (ibx#447): its requests go on with
    /// the frozen top of book of slot `frozen` (marketDataType 2).
    Frozen { instrument: InstrumentId, frozen: InstrumentId },
    /// The contract of the frozen slot `instrument` has real-time data
    /// again (ibx#447): its requests go back to slot `live`
    /// (marketDataType 1) and the frozen slot is let go.
    Live { instrument: InstrumentId, live: InstrumentId },
}

impl MdReject {
    /// The instrument of the rejected subscription.
    pub fn instrument(&self) -> InstrumentId {
        match *self {
            MdReject::Delayed { instrument }
            | MdReject::NotSubscribed { instrument, .. }
            | MdReject::NoSecurityDefinition { instrument }
            | MdReject::NewsRefused { instrument, .. }
            | MdReject::Frozen { instrument, .. }
            | MdReject::Live { instrument, .. } => instrument,
        }
    }
}

impl MarketDataState {
    fn new() -> Self {
        Self {
            quotes: Box::new(std::array::from_fn(|_| SeqQuote::new())),
            md_events: crate::md_events::MdQueue::new(),
            instrument_count: AtomicU64::new(0),
            tbt_trades: Mutex::new(Vec::with_capacity(256)),
            tbt_quotes: Mutex::new(Vec::with_capacity(256)),
            tbt_mid_points: Mutex::new(Vec::with_capacity(64)),
            real_time_bars: Mutex::new(Vec::with_capacity(64)),
            depth_updates: Mutex::new(Vec::with_capacity(64)),
            tick_news: Mutex::new(Vec::with_capacity(32)),
            generic_ticks: Mutex::new(Vec::new()),
            news_bulletins: Mutex::new(BulletinStore::default()),
            md_rejects: Mutex::new(Vec::new()),
            md_merges: Mutex::new(Vec::new()),
            md_resolved: Mutex::new(Vec::new()),
            tick_req_params: Mutex::new(Vec::new()),
            snapshot_acks: Mutex::new(Vec::new()),
            tbt_errors: Mutex::new(Vec::new()),
            error_tap: ErrorTap::default(),
        }
    }

    #[doc(hidden)] pub fn push_tbt_error(&self, req_id: ReqId, code: i32, text: String) {
        if self.error_tap.defer(ErrorQueue::TickByTick, req_id, code as i64, &text) { return; }
        let event = self.error_tap.event(req_id, code as i64, &text);
        self.tbt_errors.lock().unwrap().push((req_id, code, text));
        self.error_tap.send(event);
    }

    pub fn drain_tbt_errors(&self) -> Vec<(ReqId, i32, String)> {
        self.tbt_errors.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_tick_req_params(&self, params: TickReqParams) {
        self.tick_req_params.lock().unwrap().push(params);
    }

    pub fn drain_tick_req_params(&self) -> Vec<TickReqParams> {
        self.tick_req_params.lock().unwrap().drain(..).collect()
    }

    /// Acknowledgement of a regulatory snapshot request (ibx#446): the
    /// permission and the raw BBO exchange code of its instrument.
    #[doc(hidden)] pub fn push_snapshot_ack(&self, ack: TickReqParams) {
        self.snapshot_acks.lock().unwrap().push(ack);
    }

    pub fn drain_snapshot_acks(&self) -> Vec<TickReqParams> {
        self.snapshot_acks.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_md_reject(&self, reject: MdReject) {
        self.md_rejects.lock().unwrap().push(reject);
    }

    pub fn drain_md_rejects(&self) -> Vec<MdReject> {
        self.md_rejects.lock().unwrap().drain(..).collect()
    }

    /// A request on slot `from` joined the subscription of slot `into`
    /// (ibx#444), at this point of the market data queue (ibx#446).
    #[doc(hidden)] pub fn push_md_merge(&self, from: InstrumentId, into: InstrumentId) {
        let at = self.md_events.position();
        self.md_merges.lock().unwrap().push((from, into, at));
    }

    /// The lookup of a request without a conId found its contract (ibx#444).
    #[doc(hidden)] pub fn push_md_resolved(&self, instrument: InstrumentId, con_id: i64) {
        self.md_resolved.lock().unwrap().push((instrument, con_id));
    }

    pub fn drain_md_resolved(&self) -> Vec<(InstrumentId, i64)> {
        let mut resolved = self.md_resolved.lock().unwrap();
        if resolved.is_empty() { return Vec::new(); }
        resolved.drain(..).collect()
    }

    pub fn drain_md_merges(&self) -> Vec<(InstrumentId, InstrumentId, u64)> {
        let mut merges = self.md_merges.lock().unwrap();
        if merges.is_empty() { return Vec::new(); }
        merges.drain(..).collect()
    }

    /// Read a quote snapshot (lock-free via SeqLock).
    /// Unchecked hot-path accessor: `id` must be a registered InstrumentId
    /// (< MAX_INSTRUMENTS) or this panics. External surfaces go through
    /// `try_quote` (ibx#234).
    #[inline]
    pub fn quote(&self, id: InstrumentId) -> Quote {
        self.quotes[id as usize].read()
    }

    /// Bounds-checked quote read for user-supplied instrument ids: an
    /// out-of-range id is a caller error, not a reason to panic the process
    /// through the language boundary (ibx#234).
    #[inline]
    pub fn try_quote(&self, id: InstrumentId) -> Option<Quote> {
        if (id as usize) < MAX_INSTRUMENTS {
            Some(self.quotes[id as usize].read())
        } else {
            None
        }
    }

    /// Number of registered instruments.
    pub fn instrument_count(&self) -> u32 {
        self.instrument_count.load(Ordering::Relaxed) as u32
    }

    pub fn drain_tbt_trades(&self) -> Vec<TbtTrade> {
        self.tbt_trades.lock().unwrap().drain(..).collect()
    }

    pub fn drain_tbt_quotes(&self) -> Vec<TbtQuote> {
        self.tbt_quotes.lock().unwrap().drain(..).collect()
    }

    pub fn drain_tbt_mid_points(&self) -> Vec<TbtMidPoint> {
        self.tbt_mid_points.lock().unwrap().drain(..).collect()
    }

    pub fn drain_real_time_bars(&self) -> Vec<(ReqId, RealTimeBar)> {
        self.real_time_bars.lock().unwrap().drain(..).collect()
    }

    pub fn drain_depth_updates(&self) -> Vec<DepthUpdate> {
        self.depth_updates.lock().unwrap().drain(..).collect()
    }

    pub fn drain_tick_news(&self) -> Vec<TickNews> {
        self.tick_news.lock().unwrap().drain(..).collect()
    }

    /// The generic tick blocks decoded before queue position `head`
    /// (ibx#450); the later ones stay.
    pub fn take_generic_ticks(&self, head: u64) -> Vec<GenericTicks> {
        let mut q = self.generic_ticks.lock().unwrap();
        if q.is_empty() {
            return Vec::new();
        }
        let n = q.iter().take_while(|g| g.at <= head).count();
        q.drain(..n).collect()
    }

    pub fn drain_news_bulletins(&self) -> Vec<NewsBulletin> {
        self.news_bulletins.lock().unwrap().queue.drain(..).collect()
    }

    /// The bulletins arrived since the last call; with `replay`, the whole
    /// store of the day instead (ibx#461).
    pub fn take_news_bulletins(&self, replay: bool) -> Vec<NewsBulletin> {
        let mut store = self.news_bulletins.lock().unwrap();
        let queued: Vec<NewsBulletin> = store.queue.drain(..).collect();
        if replay { store.store.clone() } else { queued }
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)]
    pub fn push_quote(&self, id: InstrumentId, quote: &Quote) {
        self.quotes[id as usize].write(quote);
    }

    /// A farm message for a quote, as a test gives it (ibx#446): the quote
    /// is published, and its steps (`md_events::TestMessage`) are handed to
    /// the API client.
    #[doc(hidden)]
    pub fn push_test_message(&self, id: InstrumentId, quote: &Quote, message: &crate::md_events::TestMessage) {
        self.push_quote(id, quote);
        self.md_events.push_message(&message.events(id, quote));
    }

    #[doc(hidden)] pub fn push_tbt_trade(&self, trade: TbtTrade) {
        self.tbt_trades.lock().unwrap().push(trade);
    }

    #[doc(hidden)] pub fn push_tbt_quote(&self, quote: TbtQuote) {
        self.tbt_quotes.lock().unwrap().push(quote);
    }

    #[doc(hidden)] pub fn push_tbt_mid_point(&self, mid: TbtMidPoint) {
        self.tbt_mid_points.lock().unwrap().push(mid);
    }


    #[doc(hidden)] pub fn push_real_time_bar(&self, req_id: ReqId, bar: RealTimeBar) {
        self.real_time_bars.lock().unwrap().push((req_id, bar));
    }

    #[doc(hidden)] pub fn push_depth_update(&self, update: DepthUpdate) {
        self.depth_updates.lock().unwrap().push(update);
    }

    /// Remove all buffered depth updates for a given req_id (called on cancel).
    #[doc(hidden)] pub fn purge_depth_updates(&self, req_id: ReqId) {
        self.depth_updates.lock().unwrap().retain(|u| u.req_id != req_id);
    }

    #[doc(hidden)] pub fn push_generic_ticks(&self, ticks: GenericTicks) {
        self.generic_ticks.lock().unwrap().push(ticks);
    }

    #[doc(hidden)] pub fn push_tick_news(&self, news: TickNews) {
        self.tick_news.lock().unwrap().push(news);
    }

    /// Store a bulletin received today (local day) (ibx#461).
    #[doc(hidden)] pub fn push_news_bulletin(&self, bulletin: NewsBulletin) -> bool {
        self.push_news_bulletin_on(bulletin, jiff::Zoned::now().date())
    }

    /// Store a bulletin received on `day`, as the reference does
    /// (ibx#461): the store is emptied when a bulletin arrives on a new
    /// day, and a message id already stored is dropped. Returns false
    /// when dropped.
    #[doc(hidden)] pub fn push_news_bulletin_on(&self, bulletin: NewsBulletin, day: jiff::civil::Date) -> bool {
        let mut store = self.news_bulletins.lock().unwrap();
        if store.day != Some(day) {
            store.day = Some(day);
            store.store.clear();
        }
        if store.store.iter().any(|b| b.msg_id == bulletin.msg_id) {
            return false;
        }
        store.store.push(bulletin.clone());
        store.queue.push(bulletin);
        true
    }

    #[doc(hidden)] pub fn set_instrument_count(&self, count: u32) {
        self.instrument_count.store(count as u64, Ordering::Relaxed);
    }
}

/// Fills, order status updates, cancel rejects, what-if responses, and order cache.
pub struct OrderState {
    /// Fills with what the report says about each execution.
    fills: Mutex<Vec<(Fill, FillExec)>>,
    /// Commission reports from the server's commission frame (ibx#471).
    commission_reports: Mutex<Vec<api::CommissionAndFeesReport>>,
    /// Executions of orders the engine does not track, for the execution
    /// store only: no live fill callback without a known order (ibx#314).
    untracked_executions: Mutex<Vec<(api::Contract, api::Execution, FillExec)>>,
    order_updates: Mutex<Vec<OrderUpdate>>,
    cancel_rejects: Mutex<Vec<CancelReject>>,
    /// Errors raised before sending: (request or order id as the API gives
    /// it, -1 for none; code; message) (ibx#349, ibx#285).
    order_errors: Mutex<Vec<(i64, i64, String)>>,
    /// Notices of a server report given after the status of that report:
    /// the reject 201 and the cancel 202 (ibx#486).
    order_notices: Mutex<Vec<(i64, i64, String)>>,
    error_tap: ErrorTap,
    what_if_responses: Mutex<Vec<WhatIfResponse>>,
    completed_orders: Mutex<Vec<CompletedOrder>>,
    /// Enriched order info from CCP exec reports (order_id -> RichOrderInfo).
    order_cache: Mutex<HashMap<OrderId, RichOrderInfo>>,
    /// Set from the logon, or from a lost auth link, to the end of the order
    /// replay of the logon: open-order requests wait for the replay (ibx#251).
    open_orders_held: AtomicBool,
    /// The order replay of the logon just ended: the client's working
    /// orders it listed are followed by the end of the list (ibx#487).
    login_orders_end: AtomicBool,
    /// The combo of each combo order sent this session (ibx#470).
    combo_views: Mutex<HashMap<OrderId, ComboView>>,
    /// Orders the engine dropped with no status for the client: filled
    /// while the auth link was lost (ibx#251).
    forgotten_orders: Mutex<Vec<OrderId>>,
    /// The API order id of an order of another session whose id differs
    /// from the engine's key: the report's 6121, 0 when it has none.
    api_order_ids: Mutex<HashMap<OrderId, OrderId>>,
    /// The place of each order in the reference's book (insertion number)
    /// and the most orders the book held, for the order of the open-order
    /// listings (`jclient.jv.w()`).
    book_seqs: Mutex<HashMap<OrderId, u64>>,
    book_peak: std::sync::atomic::AtomicUsize,
    /// The highest API order id (6121) the server's reports gave for each
    /// API client (6119): the ids a client used in earlier sessions, as far
    /// as the server's replays show them.
    reported_order_ids: Mutex<HashMap<i64, OrderId>>,
}

impl OrderState {
    fn new() -> Self {
        Self {
            combo_views: Mutex::new(HashMap::new()),
            fills: Mutex::new(Vec::with_capacity(64)),
            commission_reports: Mutex::new(Vec::with_capacity(64)),
            untracked_executions: Mutex::new(Vec::new()),
            order_updates: Mutex::new(Vec::with_capacity(64)),
            cancel_rejects: Mutex::new(Vec::with_capacity(16)),
            order_errors: Mutex::new(Vec::new()),
            order_notices: Mutex::new(Vec::new()),
            error_tap: ErrorTap::default(),
            what_if_responses: Mutex::new(Vec::with_capacity(8)),
            completed_orders: Mutex::new(Vec::with_capacity(64)),
            order_cache: Mutex::new(HashMap::new()),
            open_orders_held: AtomicBool::new(false),
            login_orders_end: AtomicBool::new(false),
            forgotten_orders: Mutex::new(Vec::new()),
            api_order_ids: Mutex::new(HashMap::new()),
            book_seqs: Mutex::new(HashMap::new()),
            book_peak: std::sync::atomic::AtomicUsize::new(0),
            reported_order_ids: Mutex::new(HashMap::new()),
        }
    }

    /// Note an API order id a report gave for an API client (engine side).
    #[doc(hidden)] pub fn note_reported_order_id(&self, client_id: i64, order_id: OrderId) {
        let mut ids = self.reported_order_ids.lock().unwrap();
        let highest = ids.entry(client_id).or_insert(0);
        *highest = (*highest).max(order_id);
    }

    /// The highest API order id the server's reports gave for an API
    /// client, 0 for none.
    pub fn reported_order_id(&self, client_id: i64) -> OrderId {
        self.reported_order_ids.lock().unwrap().get(&client_id).copied().unwrap_or(0)
    }

    /// The API order id the client sees for an order: the engine's key,
    /// or for an order of another session the id its report gave (0 when
    /// none), as the reference shows it.
    pub fn api_order_id(&self, order_id: OrderId) -> OrderId {
        self.api_order_ids.lock().unwrap().get(&order_id).copied().unwrap_or(order_id)
    }

    #[doc(hidden)] pub fn set_api_order_id(&self, order_id: OrderId, api_id: OrderId) {
        self.api_order_ids.lock().unwrap().insert(order_id, api_id);
    }

    /// The engine now holds an order of an earlier session under `to`
    /// instead of `from` (ibx#538): what is kept for it moves, and it keeps
    /// the API order id it shows.
    #[doc(hidden)] pub fn rekey_order(&self, from: OrderId, to: OrderId) {
        let shown = self.api_order_id(from);
        {
            let mut ids = self.api_order_ids.lock().unwrap();
            ids.remove(&from);
            ids.insert(to, shown);
        }
        let mut cache = self.order_cache.lock().unwrap();
        if let Some(info) = cache.remove(&from) { cache.insert(to, info); }
        drop(cache);
        let mut seqs = self.book_seqs.lock().unwrap();
        if let Some(seq) = seqs.remove(&from) { seqs.insert(to, seq); }
        drop(seqs);
        let mut views = self.combo_views.lock().unwrap();
        if let Some(view) = views.remove(&from) { views.insert(to, view); }
    }

    /// An order's place in the reference's book, and the most orders the
    /// book held (engine side).
    #[doc(hidden)] pub fn note_book(&self, order_id: OrderId, seq: u64, peak: usize) {
        self.book_seqs.lock().unwrap().insert(order_id, seq);
        self.book_peak.store(peak, std::sync::atomic::Ordering::Relaxed);
    }

    /// The book place of an order, and the most orders the book held.
    pub fn book_place(&self, order_id: OrderId) -> (Option<u64>, usize) {
        (self.book_seqs.lock().unwrap().get(&order_id).copied(),
            self.book_peak.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Hold the open-order requests (`true`, at the logon or when the auth
    /// link is lost) or let them be answered (`false`, the order replay has
    /// ended), as the reference does (ibx#251). Hot-loop side.
    #[doc(hidden)]
    pub fn set_open_orders_held(&self, held: bool) {
        self.open_orders_held.store(held, Ordering::Release);
    }

    /// The order replay of the logon ended (ibx#487). Hot-loop side.
    #[doc(hidden)]
    pub fn set_login_orders_end(&self) {
        self.login_orders_end.store(true, Ordering::Release);
    }

    /// True once after the order replay of the logon ended (ibx#487).
    pub fn take_login_orders_end(&self) -> bool {
        self.login_orders_end.swap(false, Ordering::AcqRel)
    }

    /// True while open-order requests wait for the order replay (ibx#251).
    pub fn open_orders_held(&self) -> bool {
        self.open_orders_held.load(Ordering::Acquire)
    }

    pub fn drain_fills(&self) -> Vec<Fill> {
        self.fills.lock().unwrap().drain(..).map(|(fill, _)| fill).collect()
    }

    /// Fills with their execution details (empty when injected without).
    pub fn drain_fills_with_exec(&self) -> Vec<(Fill, FillExec)> {
        self.fills.lock().unwrap().drain(..).collect()
    }

    /// Executions of untracked orders, to store for `req_executions`
    /// (ibx#314).
    pub fn drain_untracked_executions(&self) -> Vec<(api::Contract, api::Execution, FillExec)> {
        self.untracked_executions.lock().unwrap().drain(..).collect()
    }

    pub fn drain_commission_reports(&self) -> Vec<api::CommissionAndFeesReport> {
        self.commission_reports.lock().unwrap().drain(..).collect()
    }

    pub fn drain_order_updates(&self) -> Vec<OrderUpdate> {
        self.order_updates.lock().unwrap().drain(..).collect()
    }

    pub fn drain_cancel_rejects(&self) -> Vec<CancelReject> {
        self.cancel_rejects.lock().unwrap().drain(..).collect()
    }

    /// Order errors raised before anything was sent: (order id, code, message).
    pub fn drain_order_errors(&self) -> Vec<(i64, i64, String)> {
        self.order_errors.lock().unwrap().drain(..).collect()
    }

    /// The notices to give after the order statuses (ibx#486).
    pub fn drain_order_notices(&self) -> Vec<(i64, i64, String)> {
        self.order_notices.lock().unwrap().drain(..).collect()
    }

    pub fn drain_what_if_responses(&self) -> Vec<WhatIfResponse> {
        self.what_if_responses.lock().unwrap().drain(..).collect()
    }

    pub fn drain_completed_orders(&self) -> Vec<CompletedOrder> {
        self.completed_orders.lock().unwrap().drain(..).collect()
    }

    /// Snapshot enriched entries whose latest status is an open IB state.
    /// Terminal entries (Filled / Cancelled / Inactive / etc.) are filtered out
    /// so `req_open_orders` does not leak historical orders that are still cached
    /// for `req_completed_orders` lookups.
    pub fn drain_open_orders(&self) -> Vec<(OrderId, RichOrderInfo)> {
        let lock = self.order_cache.lock().unwrap();
        lock.iter()
            .filter(|(_, v)| crate::client_core::is_open_status(&v.order_state.status))
            .map(|(&k, v)| (k, v.clone()))
            .collect()
    }

    /// The combo of a combo order (ibx#470), None for any other order.
    pub fn combo_view(&self, order_id: OrderId) -> Option<ComboView> {
        self.combo_views.lock().unwrap().get(&order_id).cloned()
    }

    #[doc(hidden)] pub fn set_combo_view(&self, order_id: OrderId, view: ComboView) {
        self.combo_views.lock().unwrap().insert(order_id, view);
    }

    /// The per-leg prices a report of a combo order carries (ibx#470).
    #[doc(hidden)] pub fn set_combo_leg_prices(&self, order_id: OrderId, leg_prices: Vec<f64>) {
        if let Some(view) = self.combo_views.lock().unwrap().get_mut(&order_id) {
            view.leg_prices = leg_prices;
        }
    }

    /// Get enriched order info by order_id.
    pub fn get_order_info(&self, order_id: OrderId) -> Option<RichOrderInfo> {
        self.order_cache.lock().unwrap().get(&order_id).cloned()
    }

    /// Remove an enriched entry. Called after a completed order has been
    /// delivered to the user, to bound `order_cache` growth in long sessions.
    pub fn remove_order_info(&self, order_id: OrderId) {
        self.order_cache.lock().unwrap().remove(&order_id);
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn push_fill(&self, fill: Fill) {
        self.fills.lock().unwrap().push((fill, FillExec::default()));
    }

    #[doc(hidden)] pub fn push_fill_with_exec(&self, fill: Fill, exec: FillExec) {
        self.fills.lock().unwrap().push((fill, exec));
    }

    /// An order the client no longer knows, with no callback (ibx#251).
    #[doc(hidden)] pub fn push_forgotten_order(&self, order_id: OrderId) {
        self.forgotten_orders.lock().unwrap().push(order_id);
    }

    /// The orders the engine dropped with no status since the last call
    /// (ibx#251).
    pub fn drain_forgotten_orders(&self) -> Vec<OrderId> {
        std::mem::take(&mut *self.forgotten_orders.lock().unwrap())
    }

    #[doc(hidden)] pub fn push_untracked_execution(&self, contract: api::Contract, execution: api::Execution, exec: FillExec) {
        self.untracked_executions.lock().unwrap().push((contract, execution, exec));
    }

    #[doc(hidden)] pub fn push_commission_report(&self, report: api::CommissionAndFeesReport) {
        self.commission_reports.lock().unwrap().push(report);
    }

    #[doc(hidden)] pub fn push_order_update(&self, update: OrderUpdate) {
        self.order_updates.lock().unwrap().push(update);
    }

    #[doc(hidden)] pub fn push_cancel_reject(&self, reject: CancelReject) {
        self.cancel_rejects.lock().unwrap().push(reject);
    }

    #[doc(hidden)] pub fn push_order_error(&self, order_id: i64, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::Order, order_id, code, &message) { return; }
        let event = self.error_tap.event(order_id, code, &message);
        self.order_errors.lock().unwrap().push((order_id, code, message));
        self.error_tap.send(event);
    }

    /// A notice of a server report (201, 202), given after the status the
    /// same report gives, as the reference writes them (ibx#486).
    #[doc(hidden)] pub fn push_order_notice(&self, order_id: i64, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::OrderNotice, order_id, code, &message) { return; }
        let event = self.error_tap.event(order_id, code, &message);
        self.order_notices.lock().unwrap().push((order_id, code, message));
        self.error_tap.send(event);
    }

    #[doc(hidden)] pub fn push_what_if(&self, response: WhatIfResponse) {
        self.what_if_responses.lock().unwrap().push(response);
    }

    #[doc(hidden)] pub fn push_completed_order(&self, order: CompletedOrder) {
        self.completed_orders.lock().unwrap().push(order);
    }

    #[doc(hidden)] pub fn push_order_info(&self, order_id: OrderId, info: RichOrderInfo) {
        self.order_cache.lock().unwrap().insert(order_id, info);
    }
}

/// BBO exchange code and security type id of an exchange map (ibx#441).
pub type ExchangeMapKey = (String, u8);

/// What is known of the exchange map of a BBO exchange (ibx#441).
#[derive(Debug, Clone, PartialEq)]
pub enum ExchangeMapState {
    /// No market data acknowledgement gave this code.
    Unknown,
    /// The code is known, its map is asked.
    Waiting,
    Ready(Vec<crate::types::SmartComponent>),
}

/// Historical data, contract definitions, scanners, news archives, market rules, contract cache.
pub struct ReferenceState {
    historical_data: Mutex<Vec<(ReqId, HistoricalResponse)>>,
    /// Bars of keepUpToDate requests, the whole current bar each time
    /// (ibx#429), for historicalDataUpdate.
    historical_updates: Mutex<Vec<(ReqId, HistoricalBar)>>,
    head_timestamps: Mutex<Vec<(ReqId, HeadTimestampResponse)>>,
    contract_details: Mutex<Vec<(ReqId, ContractDefinition)>>,
    contract_details_end: Mutex<Vec<ReqId>>,
    matching_symbols: Mutex<Vec<(ReqId, Vec<SymbolMatch>)>>,
    /// Option chain answers (ibx#440): the rows of a request, then its end.
    option_chains: Mutex<Vec<(ReqId, Vec<crate::control::optparams::OptionChain>)>>,
    scanner_params: Mutex<Vec<String>>,
    scanner_data: Mutex<Vec<(ReqId, ScannerResult)>>,
    historical_news: Mutex<Vec<(ReqId, Vec<NewsHeadline>, bool)>>,
    news_articles: Mutex<Vec<(ReqId, i32, String)>>,
    fundamental_data: Mutex<Vec<(ReqId, String)>>,
    option_computations: Mutex<Vec<crate::control::optcalc::OptionComputation>>,
    histogram_data: Mutex<Vec<(ReqId, Vec<HistogramEntry>)>>,
    historical_ticks: Mutex<Vec<(ReqId, HistoricalTickData, String, bool)>>,
    historical_schedules: Mutex<Vec<(ReqId, HistoricalScheduleResponse)>>,
    /// Errors surfaced by HMDS for in-flight reference queries (req_id, code, message).
    /// Drained by the dispatcher and forwarded to `Wrapper::error`. ibx#186.
    historical_errors: Mutex<Vec<(ReqId, i32, String)>>,
    error_tap: ErrorTap,
    market_rules: Mutex<Vec<MarketRule>>,
    depth_exchanges_cache: Mutex<Vec<DepthMktDataDescription>>,
    depth_exchanges_pending: Mutex<bool>,
    /// Contract cache from CCP exec reports (con_id -> api::Contract).
    contract_cache: Mutex<HashMap<i64, api::Contract>>,
    /// Market names from contract details, by conId.
    market_names: Mutex<HashMap<i64, String>>,
    /// Zone of the trading hours from contract details, by conId (ibx#335).
    time_zone_ids: Mutex<HashMap<i64, String>>,
    /// Exchange maps of the BBO exchanges (ibx#441), by BBO exchange code
    /// and security type id, in the order they were first seen: `None`
    /// while the map is asked.
    exchange_maps: Mutex<Vec<(ExchangeMapKey, Option<Vec<crate::types::SmartComponent>>)>>,
    /// The exchange map key of each contract with market data (ibx#441),
    /// with where it was set in the market data queue: a reused slot has
    /// the key of its earlier contract for the steps queued before
    /// (ibx#446). The last few, oldest first.
    instrument_exchange_maps: Mutex<HashMap<InstrumentId, Vec<(u64, ExchangeMapKey)>>>,
    /// Where each exchange map came in the market data queue: the steps
    /// written before it had no letters (ibx#446).
    exchange_maps_at: Mutex<HashMap<ExchangeMapKey, u64>>,
    /// Gateway-local init data (populated during connection, read-only after).
    news_providers: Mutex<Vec<crate::types::NewsProvider>>,
    /// Subscribed API news source codes of the logon, in logon order
    /// (ibx#460): the provider check and the all-subscribed form of the
    /// historical news request use them.
    news_sources: Mutex<Vec<String>>,
    /// API news source codes of the logon that are listed but not
    /// subscribed (ibx#443).
    news_sources_unsubscribed: Mutex<Vec<String>>,
    soft_dollar_tiers: Mutex<Vec<crate::types::SoftDollarTier>>,
    family_codes: Mutex<Vec<crate::types::FamilyCode>>,
    white_branding_id: Mutex<String>,
    /// The account ids of the logon's account list (6095), in logon order
    /// (ibx#420).
    managed_accounts: Mutex<Vec<String>>,
    /// The accounts whose application is not approved (8092 of the last
    /// logon reply or logon update, ibx#421).
    pending_accounts: Mutex<Vec<String>>,
    /// FA session, from CCP logon tag 6108 (ibx#481).
    fa_session: std::sync::atomic::AtomicBool,
    /// A paper session, from the logon (ibx#444).
    paper_session: std::sync::atomic::AtomicBool,
    /// The logon's super user and omnibus flags (ibx#417): either one lets
    /// a short-side order pass the side check.
    super_user: AtomicBool,
    omnibus: AtomicBool,
    /// The smart combo conId of each currency, from logon tag 6611
    /// (ibx#470).
    smart_combo_con_ids: Mutex<HashMap<String, i64>>,
    /// The API client id the new orders carry (ibx#466); 0 until set.
    api_client_id: std::sync::atomic::AtomicI64,
    /// User name of the session: the submitter of the orders it places (ibx#519).
    user_name: Mutex<String>,
    /// The algo definitions the server sent (ibx#263).
    algo_definitions: Mutex<crate::control::algo::AlgoDefinitions>,
    /// Most contracts with tick-by-tick data at once, from the logon;
    /// u64::MAX until known (ibx#455).
    tick_by_tick_limit: AtomicU64,
    /// The logon turns tick-by-tick data off (ibx#455).
    tick_by_tick_off: AtomicBool,
    /// Most snapshot requests per second, the API ticker limit of the
    /// logon as the reference sets it; 100 until known (ibx#446).
    snapshot_rate_limit: AtomicU32,
    /// Account config (6040=210): feature list and MiFID config id; None
    /// until known (ibx#425).
    account_config: Mutex<Option<(Vec<String>, String)>>,
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    ccp_session_id: Mutex<String>,
    /// Logical-name → host URL map pushed by the gateway during logon.
    misc_urls: Mutex<HashMap<String, String>>,
    /// Offset of the local clock to the server clock, from the logon
    /// replies and later server messages (ibx#421).
    clock: crate::control::logon::ClockOffset,
    /// The logon feature list allows matching symbols requests
    /// (SECDEFTA, ibx#421); true until a logon says otherwise.
    matching_symbols_allowed: AtomicBool,
    /// Most years of a historical data request, from the logon (6774);
    /// 0 until known (ibx#421).
    max_backfill_years: AtomicU32,
    /// The logon feature list has NIGHTLY: no years limit (ibx#421).
    nightly: AtomicBool,
    /// The logon feature list has NOMAGNFIX: option chain strikes as the
    /// server sends them (ibx#440).
    no_magnifier_fix: AtomicBool,
    /// The logon feature list has ISLAND2NASDAQ: NASDAQ is not left out of
    /// the option chains (ibx#440).
    island_to_nasdaq: AtomicBool,
    /// BONDAPI and EVAPI of the logon (ibx#436).
    bond_api: AtomicBool,
    ev_api: AtomicBool,
}

impl ReferenceState {
    fn new() -> Self {
        Self {
            historical_data: Mutex::new(Vec::with_capacity(16)),
            historical_updates: Mutex::new(Vec::with_capacity(16)),
            head_timestamps: Mutex::new(Vec::with_capacity(8)),
            contract_details: Mutex::new(Vec::with_capacity(16)),
            contract_details_end: Mutex::new(Vec::with_capacity(8)),
            matching_symbols: Mutex::new(Vec::with_capacity(8)),
            scanner_params: Mutex::new(Vec::new()),
            scanner_data: Mutex::new(Vec::with_capacity(8)),
            historical_news: Mutex::new(Vec::with_capacity(8)),
            news_articles: Mutex::new(Vec::with_capacity(8)),
            fundamental_data: Mutex::new(Vec::with_capacity(4)),
            option_computations: Mutex::new(Vec::new()),
            histogram_data: Mutex::new(Vec::with_capacity(4)),
            historical_ticks: Mutex::new(Vec::with_capacity(4)),
            historical_schedules: Mutex::new(Vec::with_capacity(4)),
            historical_errors: Mutex::new(Vec::with_capacity(4)),
            error_tap: ErrorTap::default(),
            market_rules: Mutex::new(Vec::new()),
            depth_exchanges_cache: Mutex::new(Vec::new()),
            depth_exchanges_pending: Mutex::new(false),
            contract_cache: Mutex::new(HashMap::new()),
            market_names: Mutex::new(HashMap::new()),
            time_zone_ids: Mutex::new(HashMap::new()),
            exchange_maps: Mutex::new(Vec::new()),
            instrument_exchange_maps: Mutex::new(HashMap::new()),
            exchange_maps_at: Mutex::new(HashMap::new()),
            news_providers: Mutex::new(Vec::new()),
            news_sources: Mutex::new(Vec::new()),
            news_sources_unsubscribed: Mutex::new(Vec::new()),
            soft_dollar_tiers: Mutex::new(Vec::new()),
            family_codes: Mutex::new(Vec::new()),
            white_branding_id: Mutex::new(String::new()),
            managed_accounts: Mutex::new(Vec::new()),
            pending_accounts: Mutex::new(Vec::new()),
            fa_session: std::sync::atomic::AtomicBool::new(false),
            paper_session: std::sync::atomic::AtomicBool::new(false),
            super_user: AtomicBool::new(false),
            omnibus: AtomicBool::new(false),
            smart_combo_con_ids: Mutex::new(HashMap::new()),
            api_client_id: std::sync::atomic::AtomicI64::new(0),
            user_name: Mutex::new(String::new()),
            algo_definitions: Mutex::new(Default::default()),
            tick_by_tick_limit: AtomicU64::new(u64::MAX),
            tick_by_tick_off: AtomicBool::new(false),
            snapshot_rate_limit: AtomicU32::new(100),
            account_config: Mutex::new(None),
            ccp_session_id: Mutex::new(String::new()),
            misc_urls: Mutex::new(HashMap::new()),
            clock: Default::default(),
            matching_symbols_allowed: AtomicBool::new(true),
            max_backfill_years: AtomicU32::new(0),
            nightly: AtomicBool::new(false),
            no_magnifier_fix: AtomicBool::new(false),
            island_to_nasdaq: AtomicBool::new(false),
            bond_api: AtomicBool::new(false),
            ev_api: AtomicBool::new(false),
            option_chains: Mutex::new(Vec::new()),
        }
    }

    pub fn drain_historical_data(&self) -> Vec<(ReqId, HistoricalResponse)> {
        self.historical_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_updates(&self) -> Vec<(ReqId, HistoricalBar)> {
        self.historical_updates.lock().unwrap().drain(..).collect()
    }

    pub fn drain_head_timestamps(&self) -> Vec<(ReqId, HeadTimestampResponse)> {
        self.head_timestamps.lock().unwrap().drain(..).collect()
    }

    pub fn drain_contract_details(&self) -> Vec<(ReqId, ContractDefinition)> {
        self.contract_details.lock().unwrap().drain(..).collect()
    }

    pub fn drain_contract_details_end(&self) -> Vec<ReqId> {
        self.contract_details_end.lock().unwrap().drain(..).collect()
    }

    pub fn drain_matching_symbols(&self) -> Vec<(ReqId, Vec<SymbolMatch>)> {
        self.matching_symbols.lock().unwrap().drain(..).collect()
    }

    /// Option chain answers (ibx#440): the rows of each request, for one
    /// SECURITY_DEFINITION_OPTION_PARAMETER each, then its end.
    pub fn drain_option_chains(&self) -> Vec<(ReqId, Vec<crate::control::optparams::OptionChain>)> {
        self.option_chains.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_option_chains(&self, req_id: ReqId, rows: Vec<crate::control::optparams::OptionChain>) {
        self.option_chains.lock().unwrap().push((req_id, rows));
    }

    pub fn drain_scanner_params(&self) -> Vec<String> {
        self.scanner_params.lock().unwrap().drain(..).collect()
    }

    pub fn drain_scanner_data(&self) -> Vec<(ReqId, ScannerResult)> {
        self.scanner_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_news(&self) -> Vec<(ReqId, Vec<NewsHeadline>, bool)> {
        self.historical_news.lock().unwrap().drain(..).collect()
    }

    pub fn drain_news_articles(&self) -> Vec<(ReqId, i32, String)> {
        self.news_articles.lock().unwrap().drain(..).collect()
    }

    pub fn drain_fundamental_data(&self) -> Vec<(ReqId, String)> {
        self.fundamental_data.lock().unwrap().drain(..).collect()
    }

    /// Answers of option calculations, in arrival order.
    pub fn drain_option_computations(&self) -> Vec<crate::control::optcalc::OptionComputation> {
        self.option_computations.lock().unwrap().drain(..).collect()
    }

    pub fn drain_histogram_data(&self) -> Vec<(ReqId, Vec<HistogramEntry>)> {
        self.histogram_data.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_ticks(&self) -> Vec<(ReqId, HistoricalTickData, String, bool)> {
        self.historical_ticks.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_schedules(&self) -> Vec<(ReqId, HistoricalScheduleResponse)> {
        self.historical_schedules.lock().unwrap().drain(..).collect()
    }

    pub fn drain_historical_errors(&self) -> Vec<(ReqId, i32, String)> {
        self.historical_errors.lock().unwrap().drain(..).collect()
    }

    /// Get cached market rules.
    pub fn market_rules(&self) -> Vec<MarketRule> {
        self.market_rules.lock().unwrap().clone()
    }

    /// Get a market rule by ID.
    pub fn market_rule(&self, rule_id: i32) -> Option<MarketRule> {
        self.market_rules.lock().unwrap().iter().find(|r| r.rule_id == rule_id).cloned()
    }

    /// Get cached contract by con_id.
    pub fn get_contract(&self, con_id: i64) -> Option<api::Contract> {
        self.contract_cache.lock().unwrap().get(&con_id).cloned()
    }

    /// Market name of a contract from its contract details (for example
    /// NMS for AAPL), when they were received.
    pub fn market_name(&self, con_id: i64) -> Option<String> {
        self.market_names.lock().unwrap().get(&con_id).cloned()
    }

    #[doc(hidden)] pub fn cache_market_name(&self, con_id: i64, market_name: &str) {
        if !market_name.is_empty() {
            self.market_names.lock().unwrap().insert(con_id, market_name.to_string());
        }
    }

    /// Zone of a contract's trading hours (for example US/Eastern for
    /// AAPL), when its contract details were received.
    pub fn time_zone_id(&self, con_id: i64) -> Option<String> {
        self.time_zone_ids.lock().unwrap().get(&con_id).cloned()
    }

    #[doc(hidden)] pub fn cache_time_zone_id(&self, con_id: i64, zone: &str) {
        if !zone.is_empty() {
            self.time_zone_ids.lock().unwrap().insert(con_id, zone.to_string());
        }
    }

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn push_historical_data(&self, req_id: ReqId, response: HistoricalResponse) {
        self.historical_data.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_historical_update(&self, req_id: ReqId, bar: HistoricalBar) {
        self.historical_updates.lock().unwrap().push((req_id, bar));
    }

    /// Drop the updates of a request not delivered yet: none goes out
    /// after its cancel (ibx#429).
    #[doc(hidden)] pub fn purge_historical_updates(&self, req_id: ReqId) {
        self.historical_updates.lock().unwrap().retain(|(r, _)| *r != req_id);
    }

    #[doc(hidden)] pub fn push_head_timestamp(&self, req_id: ReqId, response: HeadTimestampResponse) {
        self.head_timestamps.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_contract_details(&self, req_id: ReqId, def: ContractDefinition) {
        self.contract_details.lock().unwrap().push((req_id, def));
    }

    #[doc(hidden)] pub fn push_contract_details_end(&self, req_id: ReqId) {
        self.contract_details_end.lock().unwrap().push(req_id);
    }

    #[doc(hidden)] pub fn push_matching_symbols(&self, req_id: ReqId, matches: Vec<SymbolMatch>) {
        self.matching_symbols.lock().unwrap().push((req_id, matches));
    }

    #[doc(hidden)] pub fn push_scanner_params(&self, xml: String) {
        self.scanner_params.lock().unwrap().push(xml);
    }

    #[doc(hidden)] pub fn push_scanner_data(&self, req_id: ReqId, result: ScannerResult) {
        self.scanner_data.lock().unwrap().push((req_id, result));
    }

    /// Drop the queued results of a cancelled scanner (ibx#457).
    #[doc(hidden)] pub fn discard_scanner_data(&self, req_id: ReqId) {
        self.scanner_data.lock().unwrap().retain(|(r, _)| *r != req_id);
    }

    #[doc(hidden)] pub fn push_historical_news(&self, req_id: ReqId, headlines: Vec<NewsHeadline>, has_more: bool) {
        self.historical_news.lock().unwrap().push((req_id, headlines, has_more));
    }

    #[doc(hidden)] pub fn push_news_article(&self, req_id: ReqId, article_type: i32, article_text: String) {
        self.news_articles.lock().unwrap().push((req_id, article_type, article_text));
    }

    #[doc(hidden)] pub fn push_option_computation(&self, answer: crate::control::optcalc::OptionComputation) {
        self.option_computations.lock().unwrap().push(answer);
    }

    #[doc(hidden)] pub fn push_fundamental_data(&self, req_id: ReqId, data: String) {
        self.fundamental_data.lock().unwrap().push((req_id, data));
    }

    #[doc(hidden)] pub fn push_histogram_data(&self, req_id: ReqId, entries: Vec<HistogramEntry>) {
        self.histogram_data.lock().unwrap().push((req_id, entries));
    }

    #[doc(hidden)] pub fn push_historical_ticks(&self, req_id: ReqId, data: HistoricalTickData, what_to_show: String, done: bool) {
        self.historical_ticks.lock().unwrap().push((req_id, data, what_to_show, done));
    }

    #[doc(hidden)] pub fn push_historical_schedule(&self, req_id: ReqId, response: HistoricalScheduleResponse) {
        self.historical_schedules.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_historical_error(&self, req_id: ReqId, code: i32, message: String) {
        if self.error_tap.defer(ErrorQueue::Historical, req_id, code as i64, &message) { return; }
        let event = self.error_tap.event(req_id, code as i64, &message);
        self.historical_errors.lock().unwrap().push((req_id, code, message));
        self.error_tap.send(event);
    }

    #[doc(hidden)] pub fn push_market_rules(&self, rules: Vec<MarketRule>) {
        let mut lock = self.market_rules.lock().unwrap();
        for rule in rules {
            if !lock.iter().any(|r| r.rule_id == rule.rule_id) {
                lock.push(rule);
            }
        }
    }

    /// The answer to a reqMktDepthExchanges, once per request: the depth
    /// routes (an empty list is an answer too, #453).
    pub fn drain_depth_exchanges(&self) -> Option<Vec<DepthMktDataDescription>> {
        let mut pending = self.depth_exchanges_pending.lock().unwrap();
        if *pending {
            *pending = false;
            Some(self.depth_exchanges_cache.lock().unwrap().clone())
        } else {
            None
        }
    }

    /// The depth routes of the routing table (#453).
    #[doc(hidden)] pub fn set_depth_exchanges(&self, descs: Vec<DepthMktDataDescription>) {
        *self.depth_exchanges_cache.lock().unwrap() = descs;
    }

    #[doc(hidden)] pub fn notify_depth_exchanges(&self) {
        *self.depth_exchanges_pending.lock().unwrap() = true;
    }

    #[doc(hidden)] pub fn cache_contract(&self, con_id: i64, contract: api::Contract) {
        let mut cache = self.contract_cache.lock().unwrap();
        if let Some(existing) = cache.get_mut(&con_id) {
            // Merge: only overwrite fields that are non-empty in the new contract
            if !contract.symbol.is_empty() { existing.symbol = contract.symbol; }
            if !contract.sec_type.is_empty() { existing.sec_type = contract.sec_type; }
            if !contract.exchange.is_empty() { existing.exchange = contract.exchange; }
            if !contract.currency.is_empty() { existing.currency = contract.currency; }
            if !contract.local_symbol.is_empty() { existing.local_symbol = contract.local_symbol; }
            if !contract.primary_exchange.is_empty() { existing.primary_exchange = contract.primary_exchange; }
            if !contract.trading_class.is_empty() { existing.trading_class = contract.trading_class; }
        } else {
            cache.insert(con_id, contract);
        }
    }

    // ── Gateway-local init data ──

    /// The exchange map of a BBO exchange code (ibx#441), as the reference
    /// finds it: by code and security type id, or with no security type
    /// the first map of that code.
    pub fn exchange_map(&self, code: &str, sec_type_id: Option<u8>) -> ExchangeMapState {
        let maps = self.exchange_maps.lock().unwrap();
        let found = maps.iter().find(|((c, t), _)| c == code && sec_type_id.is_none_or(|id| id == *t));
        match found {
            None => ExchangeMapState::Unknown,
            Some((_, None)) => ExchangeMapState::Waiting,
            Some((_, Some(map))) => ExchangeMapState::Ready(map.clone()),
        }
    }

    /// The exchange map of a contract's BBO exchange, once received
    /// (ibx#441).
    pub fn instrument_exchange_map(&self, instrument: InstrumentId) -> Option<Vec<crate::types::SmartComponent>> {
        self.instrument_exchange_map_at(instrument, u64::MAX)
    }

    /// The exchange map of a contract's BBO exchange as known at a step of
    /// the market data queue: None for a step written before it came
    /// (ibx#446).
    pub fn instrument_exchange_map_at(&self, instrument: InstrumentId, seq: u64) -> Option<Vec<crate::types::SmartComponent>> {
        let key = {
            let keys = self.instrument_exchange_maps.lock().unwrap();
            let keys = keys.get(&instrument)?;
            keys.iter().rev().find(|(at, _)| *at <= seq).or(keys.first()).map(|(_, k)| k.clone())?
        };
        if self.exchange_maps_at.lock().unwrap().get(&key).is_some_and(|&at| at > seq) {
            return None;
        }
        match self.exchange_map(&key.0, Some(key.1)) {
            ExchangeMapState::Ready(map) => Some(map),
            _ => None,
        }
    }

    pub fn news_providers(&self) -> Vec<crate::types::NewsProvider> {
        self.news_providers.lock().unwrap().clone()
    }

    /// Subscribed API news source codes of the logon (ibx#460).
    pub fn news_sources(&self) -> Vec<String> {
        self.news_sources.lock().unwrap().clone()
    }

    /// API news source codes of the logon that are listed but not
    /// subscribed (ibx#443).
    pub fn news_sources_unsubscribed(&self) -> Vec<String> {
        self.news_sources_unsubscribed.lock().unwrap().clone()
    }

    pub fn soft_dollar_tiers(&self) -> Vec<crate::types::SoftDollarTier> {
        self.soft_dollar_tiers.lock().unwrap().clone()
    }

    pub fn family_codes(&self) -> Vec<crate::types::FamilyCode> {
        self.family_codes.lock().unwrap().clone()
    }

    pub fn white_branding_id(&self) -> String {
        self.white_branding_id.lock().unwrap().clone()
    }

    /// The account ids of the logon's account list, in logon order
    /// (ibx#420).
    pub fn managed_accounts(&self) -> Vec<String> {
        self.managed_accounts.lock().unwrap().clone()
    }

    /// The text of the managed accounts callback (ibx#420): every account
    /// of the logon's list, comma separated; `logon_account` when the
    /// logon had no list.
    pub fn managed_accounts_text(&self, logon_account: &str) -> String {
        let accounts = self.managed_accounts.lock().unwrap();
        if accounts.is_empty() {
            logon_account.to_string()
        } else {
            crate::control::logon::managed_accounts_text(&accounts)
        }
    }

    /// Whether `account`'s application is not approved yet (8092,
    /// ibx#421; `jextend.bi.g(String)`).
    pub fn account_pending(&self, account: &str) -> bool {
        self.pending_accounts.lock().unwrap().iter().any(|a| a == account)
    }

    /// The accounts of the logon's list whose application is not
    /// approved, in list order (ibx#421).
    pub fn pending_managed_accounts(&self) -> Vec<String> {
        let pending = self.pending_accounts.lock().unwrap();
        self.managed_accounts.lock().unwrap().iter().filter(|a| pending.contains(a)).cloned().collect()
    }

    /// True when the logon says this is an FA session (tag 6108, ibx#481).
    pub fn fa_session(&self) -> bool {
        self.fa_session.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Session ID surfaced to webapp REST clients as the `x-ccp-session-id` header.
    /// Empty until gateway logon completes.
    pub fn ccp_session_id(&self) -> String {
        self.ccp_session_id.lock().unwrap().clone()
    }

    /// Logical-name → host URL map pushed by the gateway during logon. Empty when
    /// no URL set was pushed; consumers should fall back to a documented literal
    /// (e.g. `api.ibkr.com` for `region_dam`).
    pub fn misc_urls(&self) -> HashMap<String, String> {
        self.misc_urls.lock().unwrap().clone()
    }

    /// Single lookup against the URL map. Returns `None` when missing.
    pub fn misc_url(&self, key: &str) -> Option<String> {
        self.misc_urls.lock().unwrap().get(key).cloned()
    }

    /// A contract's BBO exchange came (ibx#441): its key is kept for the
    /// contract, and the key is known from now on. True when the key is
    /// new (its map is then to be asked).
    #[doc(hidden)] pub fn observe_exchange_map(&self, instrument: InstrumentId, code: &str, sec_type_id: u8) -> bool {
        self.observe_exchange_map_at(instrument, code, sec_type_id, 0)
    }

    /// [`Self::observe_exchange_map`] when the engine was to write the step
    /// `at` of the market data queue (ibx#446).
    #[doc(hidden)] pub fn observe_exchange_map_at(&self, instrument: InstrumentId, code: &str, sec_type_id: u8, at: u64) -> bool {
        const KEPT: usize = 4;
        let key: ExchangeMapKey = (code.to_string(), sec_type_id);
        {
            let mut keys = self.instrument_exchange_maps.lock().unwrap();
            let keys = keys.entry(instrument).or_default();
            if keys.last().is_none_or(|(_, k)| *k != key) {
                if keys.len() >= KEPT {
                    keys.remove(0);
                }
                keys.push((at, key.clone()));
            }
        }
        let mut maps = self.exchange_maps.lock().unwrap();
        if maps.iter().any(|(k, _)| *k == key) {
            return false;
        }
        maps.push((key, None));
        true
    }

    /// The exchange map of a BBO exchange arrived (ibx#441).
    #[doc(hidden)] pub fn set_exchange_map(&self, code: &str, sec_type_id: u8, map: Vec<crate::types::SmartComponent>) {
        self.set_exchange_map_at(code, sec_type_id, map, 0);
    }

    /// The exchange map of a BBO exchange arrived when the engine was to
    /// write the step `at` of the market data queue (ibx#446).
    #[doc(hidden)] pub fn set_exchange_map_at(&self, code: &str, sec_type_id: u8, map: Vec<crate::types::SmartComponent>, at: u64) {
        let key: ExchangeMapKey = (code.to_string(), sec_type_id);
        // The first one counts: a map asked again is the same map.
        self.exchange_maps_at.lock().unwrap().entry(key.clone()).or_insert(at);
        let mut maps = self.exchange_maps.lock().unwrap();
        match maps.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = Some(map),
            None => maps.push((key, Some(map))),
        }
    }

    /// Forget a key whose map was asked and never came (its farm was
    /// lost), so the next acknowledgement asks it again.
    #[doc(hidden)] pub fn forget_waiting_exchange_map(&self, code: &str, sec_type_id: u8) {
        let key: ExchangeMapKey = (code.to_string(), sec_type_id);
        self.exchange_maps.lock().unwrap().retain(|(k, map)| *k != key || map.is_some());
    }

    #[doc(hidden)] pub fn set_news_providers(&self, providers: Vec<crate::types::NewsProvider>) {
        *self.news_providers.lock().unwrap() = providers;
    }

    #[doc(hidden)] pub fn set_news_sources(&self, codes: Vec<String>) {
        *self.news_sources.lock().unwrap() = codes;
    }

    #[doc(hidden)] pub fn set_news_sources_unsubscribed(&self, codes: Vec<String>) {
        *self.news_sources_unsubscribed.lock().unwrap() = codes;
    }

    #[doc(hidden)] pub fn set_soft_dollar_tiers(&self, tiers: Vec<crate::types::SoftDollarTier>) {
        *self.soft_dollar_tiers.lock().unwrap() = tiers;
    }

    #[doc(hidden)] pub fn set_family_codes(&self, codes: Vec<crate::types::FamilyCode>) {
        *self.family_codes.lock().unwrap() = codes;
    }

    #[doc(hidden)] pub fn set_white_branding_id(&self, id: String) {
        *self.white_branding_id.lock().unwrap() = id;
    }

    #[doc(hidden)] pub fn set_managed_accounts(&self, accounts: Vec<String>) {
        *self.managed_accounts.lock().unwrap() = accounts;
    }

    #[doc(hidden)] pub fn set_pending_accounts(&self, accounts: Vec<String>) {
        *self.pending_accounts.lock().unwrap() = accounts;
    }

    /// The account's feature list from the account config (6542), None
    /// until the config is known (ibx#425).
    pub fn account_features(&self) -> Option<Vec<String>> {
        self.account_config.lock().unwrap().as_ref().map(|(f, _)| f.clone())
    }

    #[doc(hidden)] pub fn set_account_config(&self, features: Vec<String>, mifid_config_id: String) {
        *self.account_config.lock().unwrap() = Some((features, mifid_config_id));
    }

    /// Tick-by-tick limits of the session (ibx#455): the most contracts at
    /// once (None until the logon is read) and whether the logon turns it
    /// off.
    pub fn tick_by_tick_limits(&self) -> (Option<usize>, bool) {
        let limit = self.tick_by_tick_limit.load(Ordering::Relaxed);
        ((limit != u64::MAX).then_some(limit as usize), self.tick_by_tick_off.load(Ordering::Relaxed))
    }

    /// Most snapshot requests per second (ibx#446).
    pub fn snapshot_rate_limit(&self) -> u32 {
        self.snapshot_rate_limit.load(Ordering::Relaxed)
    }

    /// The API ticker limit of the logon; 0 or less keeps 100, as the
    /// reference's limiter does.
    #[doc(hidden)] pub fn set_snapshot_rate_limit(&self, limit: u32) {
        self.snapshot_rate_limit.store(if limit > 0 { limit } else { 100 }, Ordering::Relaxed);
    }

    #[doc(hidden)] pub fn set_tick_by_tick_limits(&self, limit: usize, off: bool) {
        self.tick_by_tick_limit.store(limit as u64, Ordering::Relaxed);
        self.tick_by_tick_off.store(off, Ordering::Relaxed);
    }

    #[doc(hidden)] pub fn set_fa_session(&self, fa: bool) {
        self.fa_session.store(fa, std::sync::atomic::Ordering::Relaxed);
    }

    /// True when the logon says this is a paper session (ibx#444).
    pub fn paper_session(&self) -> bool {
        self.paper_session.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[doc(hidden)] pub fn set_paper_session(&self, paper: bool) {
        self.paper_session.store(paper, std::sync::atomic::Ordering::Relaxed);
    }

    /// The server clock offset of the session (ibx#421).
    pub fn clock(&self) -> &crate::control::logon::ClockOffset {
        &self.clock
    }

    /// Current server time in seconds, as the reference answers the
    /// current time request: the local clock plus the offset of the logon
    /// (ibx#421).
    pub fn server_time_secs(&self) -> i64 {
        self.clock.now_ms().div_euclid(1000)
    }

    /// Current server time in milliseconds, from the same clock (ibx#516).
    pub fn server_time_millis(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Whether the logon feature list allows matching symbols requests
    /// (SECDEFTA, ibx#421).
    pub fn matching_symbols_allowed(&self) -> bool {
        self.matching_symbols_allowed.load(Ordering::Relaxed)
    }

    /// The feature tokens of a logon that gate API requests (ibx#421).
    #[doc(hidden)] pub fn set_api_features(&self, features: crate::control::logon::ApiFeatures) {
        self.matching_symbols_allowed.store(features.matching_symbols, Ordering::Relaxed);
        self.nightly.store(features.nightly, Ordering::Relaxed);
        self.no_magnifier_fix.store(features.no_magnifier_fix, Ordering::Relaxed);
        self.island_to_nasdaq.store(features.island_to_nasdaq, Ordering::Relaxed);
        self.bond_api.store(features.bond_api, Ordering::Relaxed);
        self.ev_api.store(features.ev_api, Ordering::Relaxed);
    }

    /// The contract details features of the logon (ibx#436): BONDAPI and
    /// EVAPI.
    pub fn contract_details_features(&self) -> (bool, bool) {
        (self.bond_api.load(Ordering::Relaxed), self.ev_api.load(Ordering::Relaxed))
    }

    /// The option chain features of the logon (ibx#440): NOMAGNFIX and
    /// ISLAND2NASDAQ.
    pub fn option_chain_features(&self) -> (bool, bool) {
        (self.no_magnifier_fix.load(Ordering::Relaxed), self.island_to_nasdaq.load(Ordering::Relaxed))
    }

    #[doc(hidden)] pub fn set_max_backfill_years(&self, years: i32) {
        self.max_backfill_years.store(years.max(0) as u32, Ordering::Relaxed);
    }

    /// Most years of a historical data request; None when not checked:
    /// before the logon, or with the NIGHTLY feature (ibx#421).
    pub fn backfill_years_limit(&self) -> Option<i32> {
        let years = self.max_backfill_years.load(Ordering::Relaxed);
        (years > 0 && !self.nightly.load(Ordering::Relaxed)).then_some(years as i32)
    }

    /// The logon's super user and omnibus flags (ibx#417).
    pub fn short_sale_flags(&self) -> (bool, bool) {
        (self.super_user.load(Ordering::Relaxed), self.omnibus.load(Ordering::Relaxed))
    }

    /// The refusal of an algo order by the algo definitions the server
    /// sent (ibx#263); None when it passes or no definition came yet.
    /// The warnings 2174 of the time parameters with no zone go to
    /// `warnings`.
    pub fn algo_refusal(&self, algorithm: &str, values: &[(&str, &str)], overnight: bool,
        warnings: &mut Vec<(i64, String)>) -> Option<(i64, String)>
    {
        crate::control::algo::check(&self.algo_definitions.lock().unwrap(), algorithm, values, overnight, warnings)
    }

    /// Keep one algo definition answer (ibx#263).
    pub fn add_algo_definitions(&self, xml: &str) {
        self.algo_definitions.lock().unwrap().add(xml);
    }

    /// The API client id the new orders carry (ibx#466).
    pub fn api_client_id(&self) -> i64 {
        self.api_client_id.load(Ordering::Relaxed)
    }

    /// User name of the session; empty when not known.
    pub fn user_name(&self) -> String {
        self.user_name.lock().unwrap().clone()
    }

    pub fn set_user_name(&self, name: &str) {
        *self.user_name.lock().unwrap() = name.to_string();
    }

    #[doc(hidden)] pub fn set_api_client_id(&self, client_id: i64) {
        self.api_client_id.store(client_id, Ordering::Relaxed);
    }

    /// The smart combo conId of a currency, from logon tag 6611 (ibx#470);
    /// None when the logon has none for it.
    pub fn smart_combo_con_id(&self, currency: &str) -> Option<i64> {
        self.smart_combo_con_ids.lock().unwrap().get(currency).copied()
    }

    /// Keep logon tag 6611, `CUR:conId,...` (ibx#470).
    #[doc(hidden)] pub fn set_smart_combo_con_ids(&self, raw: &str) {
        let table = raw.split(',').filter_map(|entry| {
            let (currency, con_id) = entry.split_once(':')?;
            Some((currency.trim().to_string(), con_id.trim().parse::<i64>().ok().filter(|&c| c > 0)?))
        }).collect();
        *self.smart_combo_con_ids.lock().unwrap() = table;
    }

    #[doc(hidden)] pub fn set_short_sale_flags(&self, super_user: bool, omnibus: bool) {
        self.super_user.store(super_user, Ordering::Relaxed);
        self.omnibus.store(omnibus, Ordering::Relaxed);
    }

    #[doc(hidden)] pub fn set_ccp_session_id(&self, id: String) {
        *self.ccp_session_id.lock().unwrap() = id;
    }

    #[doc(hidden)] pub fn set_misc_urls(&self, urls: HashMap<String, String>) {
        *self.misc_urls.lock().unwrap() = urls;
    }
}

/// Account snapshot, per-position info, and atomic instrument positions.
/// One account value as the server sends it (ibx#475): the key, the value
/// text unchanged, and the row currency (empty when the row has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub key: String,
    pub value: String,
    pub currency: String,
    /// A per-currency ledger key (from a ledger frame).
    pub ledger: bool,
}

/// Account values of the account stream, by key and currency, in the order
/// first seen (ibx#475).
#[derive(Clone, Debug, Default)]
pub struct AccountRows {
    pub rows: Vec<AccountRow>,
    /// Bumped on every change, so a reader can skip an unchanged store.
    pub generation: u64,
    /// The first full image ended (the stream's end marker came).
    pub image_complete: bool,
    /// Latest row time, Unix seconds.
    pub time_secs: i64,
}

impl AccountRows {
    /// Set a row; returns true when the value is new or changed.
    pub fn set(&mut self, key: &str, currency: &str, value: &str) -> bool {
        self.set_row(key, currency, value, false)
    }

    /// Set a row, marking whether it is a ledger key (ibx#476).
    pub fn set_row(&mut self, key: &str, currency: &str, value: &str, ledger: bool) -> bool {
        match self.rows.iter_mut().find(|r| r.key == key && r.currency == currency) {
            // A key the ledger also sends (AccruedCash) stays a ledger key.
            Some(r) if r.value == value => {
                r.ledger |= ledger;
                false
            }
            Some(r) => {
                r.ledger |= ledger;
                r.value = value.to_string();
                self.generation += 1;
                true
            }
            None => {
                self.rows.push(AccountRow { key: key.into(), value: value.into(), currency: currency.into(), ledger });
                self.generation += 1;
                true
            }
        }
    }
}

/// Rows or the end of a batch for one account summary subscription
/// (ibx#479), keyed by the id the server echoes (`SR.Socket.{n}`).
#[derive(Clone, Debug)]
pub struct AccountSummaryEvent {
    pub sr_id: String,
    pub rows: Vec<AccountRow>,
    /// The rows came from a ledger frame (per-currency keys).
    pub ledger: bool,
    /// The server's end marker of a batch.
    pub end: bool,
    /// The rows of a ledger frame, as numbers (ibx#486); empty for the
    /// other frames.
    pub ledgers: Vec<LedgerRow>,
}

/// One row of a ledger frame (`35=RL`, a `LedgerList` row) as the
/// reference reads it into its ledger record (`jfix.aL`, ibx#486): the
/// account of the frame, the row currency (8002), the currency of tag 15,
/// and each numeric tag with its value. A value that is not a number
/// (`8174=nan`) is not set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LedgerRow {
    pub account: String,
    pub currency: String,
    pub real_currency: String,
    pub values: Vec<(u32, f64)>,
}

impl LedgerRow {
    pub fn value(&self, tag: u32) -> Option<f64> {
        self.values.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v)
    }
}

pub struct PortfolioState {
    account: Mutex<AccountState>,
    /// Account summary rows and ends, in arrival order (ibx#479).
    account_summary_events: Mutex<Vec<AccountSummaryEvent>>,
    /// Account values as the server sends them (ibx#475).
    account_rows: Mutex<AccountRows>,
    /// True once the first gateway account message ("UT"/"UM"/"RL") has been received.
    account_data_received: AtomicBool,
    /// True once the CCP init burst has been fully processed.
    account_download_complete: AtomicBool,
    /// Position info (conId -> PositionInfo) for reqPositions and P&L.
    position_infos: Mutex<HashMap<i64, PositionInfo>>,
    /// Bumped when a position or its average cost changes (ibx#477).
    position_generation: AtomicU64,
    positions: [AtomicU64; MAX_INSTRUMENTS],
    /// Midnight seeds from 6040=143 for client-side daily P&L computation.
    midnight_seeds: Mutex<HashMap<i64, MidnightSeed>>,
    /// Realized P&L of this session's fills since the last seed, by conId,
    /// from the commission frames (ibx#478). A new seed includes them.
    realized_since_seed: Mutex<HashMap<i64, f64>>,
    /// Signed cash of this session's fills since the last seed, by conId
    /// (sell positive, buy negative, like the seed's money traded).
    money_since_seed: Mutex<HashMap<i64, f64>>,
}

impl PortfolioState {
    fn new() -> Self {
        Self {
            account: Mutex::new(AccountState::default()),
            account_rows: Mutex::new(AccountRows::default()),
            account_summary_events: Mutex::new(Vec::new()),
            account_data_received: AtomicBool::new(false),
            account_download_complete: AtomicBool::new(false),
            position_infos: Mutex::new(HashMap::new()),
            position_generation: AtomicU64::new(0),
            positions: std::array::from_fn(|_| AtomicU64::new(0)),
            midnight_seeds: Mutex::new(HashMap::new()),
            realized_since_seed: Mutex::new(HashMap::new()),
            money_since_seed: Mutex::new(HashMap::new()),
        }
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        *self.account.lock().unwrap()
    }

    /// Generation of the account rows (changes whenever a row does),
    /// whether the first image is complete, and the latest row time.
    pub fn account_rows_generation(&self) -> (u64, bool, i64) {
        let rows = self.account_rows.lock().unwrap();
        (rows.generation, rows.image_complete, rows.time_secs)
    }

    #[doc(hidden)]
    pub fn push_account_summary_event(&self, event: AccountSummaryEvent) {
        self.account_summary_events.lock().unwrap().push(event);
    }

    pub fn drain_account_summary_events(&self) -> Vec<AccountSummaryEvent> {
        self.account_summary_events.lock().unwrap().drain(..).collect()
    }

    /// Copy of the account rows.
    pub fn account_rows(&self) -> AccountRows {
        self.account_rows.lock().unwrap().clone()
    }

    #[doc(hidden)]
    pub fn update_account_rows(&self, f: impl FnOnce(&mut AccountRows)) {
        f(&mut self.account_rows.lock().unwrap());
    }

    /// Get all position infos (for reqPositions).
    pub fn position_infos(&self) -> Vec<PositionInfo> {
        self.position_infos.lock().unwrap().values().cloned().collect()
    }

    /// Get position info for a single conId (for pnlSingle).
    pub fn position_info(&self, con_id: i64) -> Option<PositionInfo> {
        self.position_infos.lock().unwrap().get(&con_id).cloned()
    }

    /// Read current position for an instrument.
    /// Fixed-point (QTY_SCALE).
    pub fn position_fixed(&self, id: InstrumentId) -> Qty {
        self.positions[id as usize].load(Ordering::Relaxed) as i64
    }

    // ── Hot-loop-side writers ──

    /// True once at least one gateway account message has been processed.
    pub fn account_data_received(&self) -> bool {
        self.account_data_received.load(Ordering::Acquire)
    }

    #[doc(hidden)] pub fn set_account(&self, account: &AccountState) {
        *self.account.lock().unwrap() = *account;
        self.account_data_received.store(true, Ordering::Release);
    }

    /// Mark account download as complete (init burst processed).
    #[doc(hidden)] pub fn set_account_download_complete(&self) {
        self.account_download_complete.store(true, Ordering::Release);
    }

    /// True once the CCP init burst has been fully processed.
    pub fn account_download_complete(&self) -> bool {
        self.account_download_complete.load(Ordering::Acquire)
    }

    /// Changes whenever a position or its average cost changes (ibx#477).
    pub fn position_generation(&self) -> u64 {
        self.position_generation.load(Ordering::Acquire)
    }

    /// Move a position by a fill of this session (`delta` fixed-point,
    /// signed). The reference's position store moves with the execution, so
    /// the position row and the P&L see the fill at once; the server's
    /// average cost comes with the next position feed. A position opened by
    /// the fill takes the fill price as average cost, as the reference's
    /// first row did (captured 25/09/2026: avgCost 336.25, then 336.260003).
    #[doc(hidden)] pub fn apply_fill_to_position(&self, con_id: i64, delta: Qty, price: Price) {
        if delta == 0 {
            return;
        }
        let mut map = self.position_infos.lock().unwrap();
        let entry = map.entry(con_id).or_insert_with(|| PositionInfo { con_id, ..Default::default() });
        if entry.position_fixed == 0 {
            entry.avg_cost = price;
        }
        entry.position_fixed += delta;
        self.position_generation.fetch_add(1, Ordering::AcqRel);
    }

    #[doc(hidden)] pub fn set_position_info(&self, info: PositionInfo) {
        let mut map = self.position_infos.lock().unwrap();
        let changed = map.get(&info.con_id)
            .is_none_or(|e| e.position_fixed != info.position_fixed || e.avg_cost != info.avg_cost);
        if changed {
            self.position_generation.fetch_add(1, Ordering::AcqRel);
        }
        match map.get_mut(&info.con_id) {
            Some(existing) => {
                existing.position_fixed = info.position_fixed;
                existing.avg_cost = info.avg_cost;
                if !info.symbol.is_empty() { existing.symbol = info.symbol; }
                if !info.sec_type.is_empty() { existing.sec_type = info.sec_type; }
                if !info.currency.is_empty() { existing.currency = info.currency; }
                if !info.multiplier.is_empty() { existing.multiplier = info.multiplier; }
                // Marks are owned by set_position_marks; leave them untouched so
                // the lean position feed can't zero them (ib-agent#172).
            }
            None => { map.insert(info.con_id, info); }
        }
    }

    /// Update the per-position marks (from the account-updates portfolio message).
    /// Kept separate from set_position_info so the lean position feed, which has
    /// no marks, does not overwrite them (ib-agent#172).
    #[doc(hidden)] pub fn set_position_marks(&self, con_id: i64, market_price: Price, market_value: Price, unrealized_pnl: Price, realized_pnl: Price) {
        let mut map = self.position_infos.lock().unwrap();
        let next = map.values().map(|p| p.portfolio_seq).max().unwrap_or(0) + 1;
        let entry = map.entry(con_id).or_insert_with(|| PositionInfo { con_id, ..Default::default() });
        if entry.portfolio_seq == 0 {
            entry.portfolio_seq = next;
        }
        entry.market_price = market_price;
        entry.market_value = market_value;
        entry.unrealized_pnl = unrealized_pnl;
        entry.realized_pnl = realized_pnl;
    }

    #[doc(hidden)] pub fn set_position_fixed(&self, id: InstrumentId, pos: Qty) {
        self.positions[id as usize].store(pos as u64, Ordering::Relaxed);
    }

    /// Store midnight seeds from 6040=143 P&L response.
    #[doc(hidden)] pub fn set_midnight_seeds(&self, seeds: Vec<MidnightSeed>) {
        // The seed's realized P&L includes the fills so far (ibx#478).
        self.realized_since_seed.lock().unwrap().clear();
        self.money_since_seed.lock().unwrap().clear();
        let mut map = self.midnight_seeds.lock().unwrap();
        map.clear();
        for s in seeds {
            map.insert(s.con_id, s);
        }
    }

    /// Add realized P&L of a fill for `con_id` (ibx#478).
    #[doc(hidden)] pub fn add_realized_since_seed(&self, con_id: i64, amount: f64) {
        *self.realized_since_seed.lock().unwrap().entry(con_id).or_insert(0.0) += amount;
    }

    /// Add the signed cash of a fill for `con_id`: sell positive, buy
    /// negative.
    #[doc(hidden)] pub fn add_money_since_seed(&self, con_id: i64, cash: f64) {
        *self.money_since_seed.lock().unwrap().entry(con_id).or_insert(0.0) += cash;
    }

    /// Signed cash of fills since the last seed, by conId.
    pub fn money_since_seed(&self) -> HashMap<i64, f64> {
        self.money_since_seed.lock().unwrap().clone()
    }

    /// Realized P&L of fills since the last seed, by conId (ibx#478).
    pub fn realized_since_seed(&self) -> HashMap<i64, f64> {
        self.realized_since_seed.lock().unwrap().clone()
    }

    /// Read midnight seeds for client-side P&L computation.
    pub fn midnight_seeds(&self) -> Vec<MidnightSeed> {
        self.midnight_seeds.lock().unwrap().values().copied().collect()
    }
}

/// Shared state between hot loop and external caller.
/// Composed of domain-specific containers for clear ownership boundaries.
pub struct SharedState {
    pub market: MarketDataState,
    pub orders: OrderState,
    pub reference: ReferenceState,
    pub portfolio: PortfolioState,
    /// Last measured auth-connection round-trip time in nanoseconds
    /// (0 = never measured). Sampled from the test-request/echo cycle —
    /// see `HotLoop` liveness and `ControlCommand::Ping` (ibx#158).
    ccp_rtt_ns: AtomicU64,
    /// Set by the hot loop when the session is over (connection lost, engine
    /// stopped, or reconnect exhausted). Read-and-clear by the client so the
    /// `connection_closed` callback can fire without an event channel
    /// (ibx#242). The `Event::Disconnected` channel path is optional; this
    /// flag is always populated.
    connection_lost: AtomicBool,
    /// Link status messages for every client, as errors with id -1: link
    /// lost / restored and farm broken (ibx#399). (code, message).
    connection_notices: Mutex<Vec<(i64, String)>>,
    error_tap: ErrorTap,
    /// Notifier for waking consumers (e.g. Python event loop) when data arrives.
    notifier: Arc<Notifier>,
    /// Commands sent to the engine and handled by it (ibx#529).
    clock: Arc<CommandClock>,
}

impl SharedState {
    pub fn new() -> Self {
        let shared = Self {
            market: MarketDataState::new(),
            orders: OrderState::new(),
            reference: ReferenceState::new(),
            portfolio: PortfolioState::new(),
            ccp_rtt_ns: AtomicU64::new(0),
            connection_lost: AtomicBool::new(false),
            connection_notices: Mutex::new(Vec::new()),
            error_tap: ErrorTap::default(),
            notifier: Arc::new(Notifier::default()),
            clock: Arc::new(CommandClock::default()),
        };
        for tap in [&shared.market.error_tap, &shared.orders.error_tap, &shared.reference.error_tap, &shared.error_tap] {
            tap.wakes(&shared.notifier);
            tap.follows(&shared.clock);
        }
        shared
    }

    /// The count of the commands sent to the engine and handled by it.
    #[doc(hidden)]
    pub fn command_clock(&self) -> &Arc<CommandClock> {
        &self.clock
    }

    /// Signal that the session is over. Hot-loop side (ibx#242).
    #[doc(hidden)]
    #[inline]
    pub fn set_connection_lost(&self) {
        self.connection_lost.store(true, Ordering::Release);
        self.notify();
    }

    /// Read and clear the connection-lost flag. Returns `true` at most once per
    /// signal, so the caller can fire `connection_closed` exactly once.
    #[inline]
    pub fn take_connection_lost(&self) -> bool {
        self.connection_lost.swap(false, Ordering::AcqRel)
    }

    /// Queue a link status message for the clients (ibx#399). Hot-loop side.
    #[doc(hidden)]
    /// Give the errors and notices to the event channel too, as
    /// `Event::Error` (ibx#498). The first channel attached stays.
    pub fn attach_event_channel(&self, tx: &crossbeam_channel::Sender<Event>) {
        self.market.error_tap.attach(tx);
        self.orders.error_tap.attach(tx);
        self.reference.error_tap.attach(tx);
        self.error_tap.attach(tx);
    }

    pub fn push_connection_notice(&self, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::Connection, -1, code, &message) { return; }
        let event = self.error_tap.event(-1, code, &message);
        self.connection_notices.lock().unwrap().push((code, message));
        self.error_tap.send(event);
        self.notify();
    }

    /// Link status messages since the last call: (code, message), in order.
    pub fn drain_connection_notices(&self) -> Vec<(i64, String)> {
        std::mem::take(&mut *self.connection_notices.lock().unwrap())
    }

    /// Record an auth-connection RTT sample (ibx#158). Hot-loop side.
    #[inline]
    pub fn set_ccp_rtt(&self, rtt: std::time::Duration) {
        self.ccp_rtt_ns.store(rtt.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    /// Last measured auth-connection round-trip time, if any (ibx#158).
    /// A gauge, not a benchmark: the sample is the interval from a test
    /// request to the first inbound traffic that followed it, which on an
    /// active feed can undercount by racing data already in flight.
    #[inline]
    pub fn last_ccp_rtt(&self) -> Option<std::time::Duration> {
        match self.ccp_rtt_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(std::time::Duration::from_nanos(ns)),
        }
    }

    /// Signal that new data is available. Called by hot loop after pushing data.
    #[inline]
    pub fn notify(&self) {
        self.notifier.notify();
    }

    /// Wait for data notification with a timeout. Returns true if notified, false if timed out.
    pub fn wait_for_data(&self, timeout: std::time::Duration) -> bool {
        self.notifier.wait(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error written by the thread of a caller waits for the engine to
    /// handle the commands sent before it, so the answers come in the
    /// order of the requests (ibx#529).
    #[test]
    fn a_local_error_waits_for_the_commands_sent_before_it() {
        let shared = Arc::new(SharedState::new());
        let clock = shared.command_clock().clone();
        clock.note_sent();
        // The engine: it answers the command a little later.
        let engine = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                shared.command_clock().set_running(true);
                std::thread::sleep(std::time::Duration::from_millis(5));
                shared.reference.push_historical_error(1, 162, "cancelled".into());
                shared.command_clock().note_handled(1);
                std::thread::sleep(std::time::Duration::from_millis(50));
                shared.command_clock().set_running(false);
            })
        };
        while !clock.running.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // The caller: refused locally, right after the command.
        shared.reference.push_historical_error(2, 321, "refused".into());
        let ids: Vec<i64> = shared.reference.drain_historical_errors().iter().map(|e| e.0).collect();
        assert_eq!(ids, [1, 2], "the answer of the engine first");
        engine.join().unwrap();
    }

    #[test]
    fn a_local_error_does_not_wait_without_a_running_engine_or_with_nothing_sent() {
        let shared = SharedState::new();
        let clock = shared.command_clock();
        let start = Instant::now();
        // A command sent, no engine running (a client built by hand).
        clock.note_sent();
        shared.reference.push_historical_error(1, 321, "refused".into());
        // An engine running, every command handled.
        clock.note_handled(1);
        clock.running.store(true, Ordering::Release);
        shared.reference.push_historical_error(2, 321, "refused".into());
        assert!(start.elapsed() < CommandClock::LIMIT / 2);
        assert_eq!(shared.reference.drain_historical_errors().len(), 2);
    }

    #[test]
    fn the_engine_does_not_wait_for_itself_and_a_caller_not_past_the_limit() {
        let shared = SharedState::new();
        let clock = shared.command_clock();
        clock.note_sent();
        // On the thread of the engine: at once, with a command unhandled.
        clock.set_running(true);
        let start = Instant::now();
        shared.orders.push_order_error(1, 201, "rejected".into());
        assert!(start.elapsed() < CommandClock::LIMIT / 2);
        // On another thread, with an engine that never handles it: the
        // error is written once the limit has passed.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let start = Instant::now();
                shared.orders.push_order_error(2, 110, "price".into());
                assert!(start.elapsed() >= CommandClock::LIMIT);
                assert!(start.elapsed() < CommandClock::LIMIT * 20);
            });
        });
        clock.set_running(false);
        assert_eq!(shared.orders.drain_order_errors().len(), 2);
    }

    /// Every error queue gives its entries to an attached event channel,
    /// with the id and the code the queue holds, and nothing before a
    /// channel is attached (ibx#498).
    #[test]
    fn errors_go_to_the_attached_event_channel() {
        let shared = SharedState::new();
        shared.reference.push_historical_error(1, 200, "before".into());
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);

        shared.reference.push_historical_error(7, 200, "no security definition".into());
        shared.market.push_tbt_error(8, 10189, "tick-by-tick".into());
        shared.orders.push_order_error(9, 110, "price".into());
        shared.orders.push_order_notice(9, 201, "rejected".into());
        shared.push_connection_notice(1100, "link lost".into());

        let events: Vec<(i64, i64, String)> = rx.try_iter().map(|e| match e {
            Event::Error { req_id, code, message } => (req_id, code, message),
            other => panic!("only errors are expected: {other:?}"),
        }).collect();
        assert_eq!(events, [
            (7, 200, "no security definition".to_string()), (8, 10189, "tick-by-tick".to_string()),
            (9, 110, "price".to_string()), (9, 201, "rejected".to_string()), (-1, 1100, "link lost".to_string()),
        ]);
        // The queues keep every entry for the wrapper.
        assert_eq!(shared.reference.drain_historical_errors().len(), 2);
        assert_eq!(shared.orders.drain_order_errors().len() + shared.orders.drain_order_notices().len(), 2);
        assert_eq!((shared.market.drain_tbt_errors().len(), shared.drain_connection_notices().len()), (1, 1));
    }

    #[test]
    fn seqquote_write_read_roundtrip() {
        let sq = SeqQuote::new();
        let mut q = Quote::default();
        q.bid = 150 * PRICE_SCALE;
        q.ask = 151 * PRICE_SCALE;
        sq.write(&q);
        let read = sq.read();
        assert_eq!(read.bid, 150 * PRICE_SCALE);
        assert_eq!(read.ask, 151 * PRICE_SCALE);
    }

    #[test]
    fn seqquote_default_is_zero() {
        let sq = SeqQuote::new();
        let q = sq.read();
        assert_eq!(q.bid, 0);
        assert_eq!(q.ask, 0);
    }

    #[test]
    fn shared_state_fills_drain() {
        let ss = SharedState::new();
        ss.orders.push_fill(Fill {
            cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
            instrument: 0, order_id: 1, side: Side::Buy,
            price: 100 * PRICE_SCALE, qty_fixed: (10) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
            commission: 0, timestamp_ns: 0,
        });
        ss.orders.push_fill(Fill {
            cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
            instrument: 0, order_id: 2, side: Side::Sell,
            price: 101 * PRICE_SCALE, qty_fixed: (5) as i64 * crate::types::QTY_SCALE, remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
            commission: 0, timestamp_ns: 0,
        });
        let fills = ss.orders.drain_fills();
        assert_eq!(fills.len(), 2);
        // Second drain should be empty
        assert!(ss.orders.drain_fills().is_empty());
    }

    #[test]
    fn shared_state_order_updates_drain() {
        let ss = SharedState::new();
        ss.orders.push_order_update(OrderUpdate {
            avg_fill_price: 0,
            order_id: 1, instrument: 0, status: OrderStatus::Submitted,
            filled_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, remaining_qty_fixed: (100) as i64 * crate::types::QTY_SCALE, perm_id: 0, parent_id: 0, timestamp_ns: 0,
        });
        let updates = ss.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert!(ss.orders.drain_order_updates().is_empty());
    }

    #[test]
    fn shared_state_position_roundtrip() {
        let ss = SharedState::new();
        assert_eq!(ss.portfolio.position_fixed(0) / crate::types::QTY_SCALE, 0);
        ss.portfolio.set_position_fixed(0, (42) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ss.portfolio.position_fixed(0) / crate::types::QTY_SCALE, 42);
        ss.portfolio.set_position_fixed(0, (-10) as i64 * crate::types::QTY_SCALE);
        assert_eq!(ss.portfolio.position_fixed(0) / crate::types::QTY_SCALE, -10);
    }

    #[test]
    fn shared_state_account_roundtrip() {
        let ss = SharedState::new();
        let mut a = AccountState::default();
        a.net_liquidation = 100_000 * PRICE_SCALE;
        ss.portfolio.set_account(&a);
        let read = ss.portfolio.account();
        assert_eq!(read.net_liquidation, 100_000 * PRICE_SCALE);
    }

    #[test]
    fn reference_state_ccp_session_id_roundtrip() {
        let ss = SharedState::new();
        assert!(ss.reference.ccp_session_id().is_empty());
        ss.reference.set_ccp_session_id("abc.0001".to_string());
        assert_eq!(ss.reference.ccp_session_id(), "abc.0001");
    }

    #[test]
    fn reference_state_misc_urls_roundtrip() {
        let ss = SharedState::new();
        assert!(ss.reference.misc_urls().is_empty());
        assert!(ss.reference.misc_url("region_dam").is_none());
        let mut urls = HashMap::new();
        urls.insert("region_dam".to_string(), "api-east.example.com".to_string());
        urls.insert("margin".to_string(), "margin.example.com".to_string());
        ss.reference.set_misc_urls(urls);
        let map = ss.reference.misc_urls();
        assert_eq!(map.len(), 2);
        assert_eq!(ss.reference.misc_url("region_dam").as_deref(), Some("api-east.example.com"));
        assert_eq!(ss.reference.misc_url("missing"), None);
    }

    #[test]
    fn event_gateway_logon_carries_fields() {
        let mut urls = HashMap::new();
        urls.insert("region_dam".to_string(), "api.example.com".to_string());
        let event = Event::GatewayLogon {
            ccp_session_id: "sid.abcd".to_string(),
            misc_urls: urls,
        };
        match event {
            Event::GatewayLogon { ccp_session_id, misc_urls } => {
                assert_eq!(ccp_session_id, "sid.abcd");
                assert_eq!(misc_urls.get("region_dam").map(String::as_str), Some("api.example.com"));
            }
            _ => panic!("expected GatewayLogon"),
        }
    }

    #[test]
    fn seqquote_concurrent_read_write() {
        use std::sync::Arc;
        use std::thread;

        let sq = Arc::new(SeqQuote::new());
        let sq_writer = sq.clone();
        let sq_reader = sq.clone();

        let writer = thread::spawn(move || {
            for i in 0..1000 {
                let mut q = Quote::default();
                q.bid = i * PRICE_SCALE;
                q.ask = (i + 1) * PRICE_SCALE;
                sq_writer.write(&q);
            }
        });

        let reader = thread::spawn(move || {
            for _ in 0..1000 {
                let q = sq_reader.read();
                // bid and ask should be consistent (ask = bid + PRICE_SCALE)
                if q.bid != 0 {
                    assert_eq!(q.ask, q.bid + PRICE_SCALE);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }

    /// One writer and several readers hammer one slot. Every field of a
    /// written quote derives from the same sequence number, so a torn
    /// snapshot shows up as fields from two writes; a reader must also never
    /// see the sequence go backwards.
    #[test]
    fn seqquote_stress_one_writer_many_readers() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::thread;

        fn quote_for(n: i64) -> Quote {
            Quote {
                bid: n, ask: n + 1, last: n + 2,
                bid_size: n + 3, ask_size: n + 4, last_size: n + 5, volume: n + 6,
                open: n + 7, high: n + 8, low: n + 9, close: n + 10,
                timestamp_ns: (n + 11) as u64,
                bid_exch_mask: n + 12, ask_exch_mask: n + 13, last_exch_mask: !n,
            }
        }

        const WRITES: i64 = 1_000_000;
        const READERS: usize = 4;
        let sq = Arc::new(SeqQuote::new());
        let done = Arc::new(AtomicBool::new(false));

        let readers: Vec<_> = (0..READERS).map(|_| {
            let sq = sq.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut last_seen = 0i64;
                let mut reads = 0u64;
                loop {
                    let finished = done.load(Ordering::Acquire);
                    let q = sq.read();
                    let n = q.bid;
                    let expect = if n == 0 { Quote::default() } else { quote_for(n) };
                    assert!(
                        q.ask == expect.ask && q.last == expect.last
                            && q.bid_size == expect.bid_size && q.ask_size == expect.ask_size
                            && q.last_size == expect.last_size && q.volume == expect.volume
                            && q.open == expect.open && q.high == expect.high
                            && q.low == expect.low && q.close == expect.close
                            && q.timestamp_ns == expect.timestamp_ns
                            && q.bid_exch_mask == expect.bid_exch_mask
                            && q.ask_exch_mask == expect.ask_exch_mask
                            && q.last_exch_mask == expect.last_exch_mask,
                        "torn quote at sequence {n}"
                    );
                    assert!(n >= last_seen, "sequence went back: {n} after {last_seen}");
                    last_seen = n;
                    reads += 1;
                    if finished { break; }
                }
                (last_seen, reads)
            })
        }).collect();

        for n in 1..=WRITES {
            sq.write(&quote_for(n));
        }
        done.store(true, Ordering::Release);

        for r in readers {
            let (last_seen, reads) = r.join().unwrap();
            assert_eq!(last_seen, WRITES, "a read after the last write sees it");
            assert!(reads > 0);
        }
    }
}
