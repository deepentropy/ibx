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

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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

/// One answer of reqCompletedOrders (ibx#695): a finished order, or the end
/// of the list (None).
pub type CompletedRow = Option<Box<(api::Contract, api::Order, api::OrderState)>>;

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
    /// Server execution id.
    pub exec_id: String,
    /// Execution time, Unix seconds: the execution time field, else the
    /// transaction time, else the sending time.
    pub time_secs: Option<i64>,
    /// The routing exchange, else the security exchange.
    pub exchange: String,
    /// Placing client id; 0 when absent.
    pub client_id: i64,
    /// The model code field.
    pub model_code: String,
    /// The order reference field.
    pub order_ref: String,
    /// The last liquidity indicator (lastLiquidity); 0 when absent.
    pub last_liquidity: i32,
    /// A report of a combo order (ibx#470): the contract its execution
    /// shows, and for a leg report the leg's own execution values.
    pub combo: Option<Box<ComboExec>>,
    /// An execution of an order of another API client: kept for
    /// `req_executions`, with no live callback and no commission report
    /// (the reference gives the reports to the order's client only).
    pub other_client: bool,
    /// An execution the server gave again (replay flag set: the replay of the day at
    /// the logon, the fill-up after a reconnect): kept for
    /// `req_executions`, with no live callback and no commission report,
    /// as the reference's API clients get none for them (ibx#487, captured
    /// 07/10/2026 and 30/09/2026).
    pub replayed: bool,
    /// The position right after this fill, for the reqPositions row given
    /// with it (ibx#707).
    pub position_after: Option<(i64, Qty, f64)>,
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

/// An answer queue whose items keep the place they were written in among
/// all the answers of the session (ibx#532): each item takes the next
/// count of the session's answer clock when it is written, under the
/// queue's lock. A client gives the answers of all its queues in that
/// order, as the reference gives the messages of its one connection.
pub struct SeqQueue<T> {
    items: Mutex<Vec<(u64, T)>>,
    clock: Arc<AtomicU64>,
}

impl<T> SeqQueue<T> {
    fn new(clock: &Arc<AtomicU64>) -> Self {
        Self { items: Mutex::new(Vec::new()), clock: clock.clone() }
    }

    #[inline]
    fn push(&self, item: T) {
        let mut items = self.items.lock().unwrap();
        let at = self.clock.fetch_add(1, Ordering::Relaxed);
        items.push((at, item));
    }

    /// The items, without their place.
    fn drain(&self) -> Vec<T> {
        self.items.lock().unwrap().drain(..).map(|(_, item)| item).collect()
    }

    /// The items with their place in the session's answers.
    pub fn drain_stamped(&self) -> Vec<(u64, T)> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    /// The items with their place, moved into `into`: both keep their
    /// buffers.
    pub fn drain_into(&self, into: &mut std::collections::VecDeque<(u64, T)>) {
        let mut items = self.items.lock().unwrap();
        if !items.is_empty() {
            into.extend(items.drain(..));
        }
    }

    fn retain(&self, mut keep: impl FnMut(&T) -> bool) {
        self.items.lock().unwrap().retain(|(_, item)| keep(item));
    }

    /// Take the items `take` maps to a value; the others stay, in order.
    fn take_some<U>(&self, mut take: impl FnMut(T) -> Result<U, T>) -> Vec<U> {
        let mut items = self.items.lock().unwrap();
        let mut out = Vec::new();
        let mut kept = Vec::with_capacity(items.len());
        for (at, item) in items.drain(..) {
            match take(item) {
                Ok(value) => out.push(value),
                Err(item) => kept.push((at, item)),
            }
        }
        *items = kept;
        out
    }
}

/// The server's answer to a calendar data request (ibx#443): its JSON
/// when it succeeded, else the text of its error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WshAnswer {
    pub req_id: i64,
    pub meta: bool,
    pub ok: bool,
    pub text: String,
}

/// The session's answer clock (ibx#532): the next count an answer takes,
/// shared by every answer queue and by the answers a client makes itself.
pub fn answer_clock() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(1))
}

/// Where the answers read from the session's state stand among the answers
/// written (ibx#662): the count taken at the first change of that state
/// since a client last read it. The reference gives every answer in the
/// order its connection received it; a client reads the state at that
/// count, not after the other answers of its pass.
pub struct StateMark {
    clock: Arc<AtomicU64>,
    /// The count plus one; 0: no change since the last read.
    at: AtomicU64,
}

impl StateMark {
    fn new(clock: &Arc<AtomicU64>) -> Self {
        Self { clock: clock.clone(), at: AtomicU64::new(0) }
    }

    /// The state changed (called after the change). One load when a change
    /// is already marked.
    #[inline]
    pub fn note(&self) {
        if self.at.load(Ordering::Relaxed) == 0 {
            let at = self.clock.fetch_add(1, Ordering::Relaxed);
            let _ = self.at.compare_exchange(0, at + 1, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// The count of the first change not read yet.
    #[inline]
    pub fn first(&self) -> Option<u64> {
        self.at.load(Ordering::Relaxed).checked_sub(1)
    }

    /// The state is read now: a change counted below `below` is read; one
    /// counted after stays marked, for its place in the next pass.
    #[inline]
    pub fn read_below(&self, below: u64) {
        let at = self.at.load(Ordering::Relaxed);
        if at != 0 && at - 1 < below {
            let _ = self.at.compare_exchange(at, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
}

/// The event channel as the queues of errors and answers see it (ibx#498):
/// unset, and free, until a channel is attached.
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

    /// A copy of an answer for the event channel, made only when a channel
    /// is attached. Taken before the answer goes to its queue.
    #[inline]
    fn copy<T: Clone>(&self, value: &T) -> Option<T> {
        self.events.get().map(|_| value.clone())
    }

    /// A notify-only event (ibx#568), built only when a channel is
    /// attached. Non-blocking: dropped when the channel is full.
    #[inline]
    fn notify(&self, event: impl FnOnce() -> Event) {
        if let Some(tx) = self.events.get() {
            let _ = tx.try_send(event());
        }
    }

    /// Give an answer to the event channel once its queue has it.
    /// Non-blocking: dropped when the channel is full.
    #[inline]
    fn emit(&self, event: Option<Event>) {
        if let (Some(tx), Some(event)) = (self.events.get(), event) {
            let _ = tx.try_send(event);
        }
    }

    /// An error with no queue behind it, answered by the client itself.
    #[inline]
    fn emit_error(&self, req_id: i64, code: i64, message: &str) {
        if let Some(tx) = self.events.get() {
            let _ = tx.try_send(Event::Error { req_id, code, message: message.to_string() });
        }
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

    /// Put back commands taken by `take_deferred` and not handled, ahead
    /// of those given since.
    #[cfg(any(test, feature = "test-support", feature = "python"))]
    pub fn give_back_deferred(&self, mut rest: Vec<(u64, ControlCommand)>) {
        let mut deferred = self.deferred.lock().unwrap();
        rest.append(&mut deferred);
        *deferred = rest;
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

    /// As `take_turn`, without waiting: the receiver tells when the turn
    /// has come. `None` when the requests are not paced: the turn is now.
    pub fn turn_ticket(&self) -> Option<crossbeam_channel::Receiver<()>> {
        if !self.takes_turns() {
            return None;
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.defer(ControlCommand::Turn { reply_tx });
        Some(reply_rx)
    }

    /// The requests are paced and an engine runs, on another thread than
    /// the caller's: for a test that plays the engine itself (ibx#563).
    #[cfg(any(test, feature = "test-support", feature = "python"))]
    pub fn pace_for_test(&self) {
        self.paced.store(true, Ordering::Release);
        self.running.store(true, Ordering::Release);
    }

    /// The calling thread is (or is no longer) the one that runs the
    /// engine, for a test that steps the engine itself (ibx#563).
    #[cfg(any(test, feature = "test-support", feature = "python"))]
    pub fn set_engine_thread(&self, on: bool) {
        ON_ENGINE_THREAD.with(|flag| flag.set(on));
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

/// Events emitted by the engine.
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
    /// an order, and -1 for a link status message. The errors the client
    /// answers itself (a refused market data request, a failed snapshot,
    /// a lookup answered from kept data) are given when the client gives
    /// them to its wrapper, so they need its `process_msgs`; a refused
    /// market data subscription also comes at once as `MarketDataError`.
    Error { req_id: i64, code: i64, message: String },
    /// A market data subscription the server refused or moved to delayed
    /// data, by instrument as in `Tick`, with the code and text the client
    /// reports for it (ibx#575). Given by the engine: it does not need the
    /// client to process its messages. The client gives the same refusal
    /// as `Error` with the request id when it does.
    MarketDataError { instrument: InstrumentId, code: i64, message: String },
    /// Option chain parameters (ibx#498): every row of a request at once;
    /// the request has no other answer, this is also its end.
    OptionChains { req_id: ReqId, chains: Vec<crate::control::optparams::OptionChain> },
    /// Market rules received with contract definitions and not known
    /// before. The market rule request is answered from them.
    MarketRules(Vec<MarketRule>),
    /// Historical ticks of a request; `done` on its last part.
    HistoricalTicks { req_id: ReqId, data: HistoricalTickData, what_to_show: String, done: bool },
    /// Historical trading schedule.
    HistoricalSchedule { req_id: ReqId, data: HistoricalScheduleResponse },
    /// Histogram of a request.
    HistogramData { req_id: ReqId, entries: Vec<HistogramEntry> },
    /// A bar of a historical request kept up to date. None comes after
    /// the request's cancel answer (its `Error` 162); one written between
    /// the cancel call and that answer is on the channel before it, as the
    /// reference's socket gives what it wrote before it took the cancel
    /// (ibx#576). The `historicalDataUpdate` callback drops those.
    HistoricalUpdate { req_id: ReqId, bar: HistoricalBar },
    /// Commission report of an execution, given after the `Fill` of that
    /// execution only (ibx#644): the reports of executions of earlier
    /// sessions, which the server sends after the login, are not given.
    CommissionReport(api::CommissionAndFeesReport),
    /// An order that reached its end (filled, cancelled, rejected).
    CompletedOrder(CompletedOrder),
    /// An order as the client reports it open, after each of its reports.
    OpenOrder { order_id: OrderId, info: RichOrderInfo },
    /// Scanner parameters, as XML: the text the `scannerParameters`
    /// callback gives, rebuilt as the reference hands it to a client
    /// (ibx#571, ibx#574). The rebuild takes over 10 ms, so it is made on a
    /// thread of its own: the event comes that much after the answer, and
    /// can come after events written in the meantime. A text the rebuild
    /// refuses gives no event, as no callback.
    ScannerParameters(String),
    /// Rows of a scanner subscription. None come after the cancel answer
    /// (its `Error` 162); rows written between the cancel call and that
    /// answer are on the channel before it (ibx#576). The `scannerData`
    /// callback drops those.
    ScannerData { req_id: ReqId, result: ScannerResult },
    /// Fundamental data of a request.
    FundamentalData { req_id: ReqId, data: String },
    /// A replace of FA data whose saves all succeeded (ibx#507): the
    /// `replaceFAEnd` callback, text "FA data saved". A failed one comes as
    /// `Error` 10229.
    ReplaceFaEnd { req_id: i64 },
    /// Historical news headlines; `has_more` when the server has more.
    HistoricalNews { req_id: ReqId, headlines: Vec<NewsHeadline>, has_more: bool },
    /// A news article.
    NewsArticle { req_id: ReqId, article_type: i32, text: String },
    /// A news bulletin, once per message id and day, whether or not
    /// bulletins are subscribed.
    NewsBulletin(NewsBulletin),
    /// Account summary rows and ends, as they arrive.
    AccountSummary(AccountSummaryEvent),
    /// A 5 second real-time bar.
    RealTimeBar { req_id: ReqId, bar: RealTimeBar },
    /// Answer of an option price or implied volatility calculation.
    OptionComputation(crate::control::optcalc::OptionComputation),
    /// Position update.
    /// `position` is fixed-point (QTY_SCALE).
    PositionUpdate { instrument: InstrumentId, con_id: i64, position_fixed: Qty, avg_cost: f64 },
    /// A market depth update of a request was queued (ibx#568): notify
    /// only, the row comes with the next `process_msgs` (`updateMktDepth`).
    DepthUpdate { req_id: ReqId },
    /// Generic ticks of an instrument were queued (ibx#568): notify only,
    /// they come with the next `process_msgs` (`tickGeneric`,
    /// `tickString` and the others of the generic list).
    GenericTicks(InstrumentId),
    /// The request parameters of an instrument's subscription came
    /// (ibx#568): notify only, `tickReqParams` and the market data type
    /// come with the next `process_msgs`.
    MarketDataParams(InstrumentId),
    /// The account's values or portfolio rows changed (ibx#568): notify
    /// only; read them with `SharedState::portfolio.account_rows()` and
    /// `position_infos()`, or wait for `updateAccountValue` and
    /// `updatePortfolio`.
    AccountUpdate,
    /// A P&L price of a position changed (ibx#568): notify only; the
    /// prices are in `SharedState::portfolio.pnl_prices()`, the P&L
    /// callbacks come with the next `process_msgs`.
    PnlPrice { con_id: i64 },
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
    /// The state the client reads changed (ibx#662).
    pub(crate) state: Arc<StateMark>,
    /// InstrumentId counter — set by hot loop on RegisterInstrument.
    instrument_count: AtomicU64,
    pub(crate) tbt_trades: SeqQueue<TbtTrade>,
    pub(crate) tbt_quotes: SeqQueue<TbtQuote>,
    pub(crate) tbt_mid_points: SeqQueue<TbtMidPoint>,
    pub(crate) real_time_bars: SeqQueue<(ReqId, RealTimeBar)>,
    pub(crate) depth_updates: SeqQueue<DepthUpdate>,
    pub(crate) tick_news: SeqQueue<TickNews>,
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
    /// The security type of a snapshot's contract taken from its
    /// definition (ibx#676).
    md_kinds: Mutex<Vec<(InstrumentId, String)>>,
    /// The acknowledgements of top-of-book subscriptions, with their time:
    /// a snapshot's time limit counts from it (ibx#494).
    top_acks: Mutex<Vec<(InstrumentId, std::time::Instant)>>,
    /// The request parameters of acked subscriptions (ibx#449).
    tick_req_params: Mutex<Vec<TickReqParams>>,
    /// Request parameters known before the acknowledgement, and whether the
    /// contract is an option (ibx#535).
    early_tick_req_params: Mutex<Vec<(TickReqParams, bool)>>,
    snapshot_acks: Mutex<Vec<TickReqParams>>,
    /// Tick-by-tick requests that ended with an error: the request, the
    /// code and the whole text (ibx#455).
    pub(crate) tbt_errors: SeqQueue<(ReqId, i32, String)>,
    error_tap: ErrorTap,
    /// The size and price increments of each slot's contract, from its
    /// definition, packed (`md_events::Increment`; 0 when not known): the
    /// API client gives the last size and price in their steps from
    /// session level 222 (ibx#613). Written when a subscription goes out.
    last_increments: Box<[[AtomicU64; 2]; MAX_INSTRUMENTS]>,
    /// The last price and last size precisions of the contracts whose
    /// definition was read, by contract id, as the definition's text: the
    /// protobuf form of the Python client's tickReqParams carries them
    /// (ibx#725). Written when a definition is read, never on a tick.
    precisions: Mutex<HashMap<i64, (String, String)>>,
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
    /// (as the reference writes it), empty when not known (ibx#444).
    /// `kept_params`: the request parameters (minimum tick, BBO exchange,
    /// permissions) the contract's record kept from an earlier subscription,
    /// which the requests get before the error (captured 05/10/2026).
    /// `part`: the side refused (BID_ASK or LAST) when the other side was
    /// confirmed: 10090 (10091) names it before 354 (10089), which then
    /// ends with `farm_text`, the farm's own text (ibx#650).
    NotSubscribed {
        instrument: InstrumentId, delayed_available: bool, needs_api_subscription: bool, description: String,
        kept_params: Option<(f64, String, i64)>, part: Option<&'static str>, farm_text: String,
    },
    /// Refused for a contract that needs a subscription for the API, with
    /// delayed data available and the client in delayed mode (ibx#674):
    /// 10089, marketDataType 3, then 354 with the farm's text; the
    /// subscription is gone.
    DelayedFallback {
        instrument: InstrumentId, description: String, farm_text: String,
        kept_params: Option<(f64, String, i64)>,
    },
    /// Asked delayed at once, its contract known to need delayed data in
    /// delayed mode (ibx#674): its requests observe delayed data, as the
    /// requests that join a contract known so.
    DelayedAtOnce { instrument: InstrumentId },
    /// A subscription given without a conId whose lookup found no single
    /// contract: error 200, the subscription is gone (ibx#278).
    NoSecurityDefinition { instrument: InstrumentId },
    /// A subscription given without a conId, cancelled while its lookup
    /// was on its way: when the lookup answers, error 354 for the request
    /// and nothing subscribed; its cancel then gets 300 (ibx#662).
    CancelledDuringLookup { instrument: InstrumentId },
    /// A stream given without a conId whose contract, once found, needs a
    /// line while every API line was taken when it was asked: error 101,
    /// nothing was sent (ibx#501).
    MaxTickers { instrument: InstrumentId },
    /// A request with the news tick refused once its contract was known:
    /// error 10094 with this text, nothing was sent (ibx#458). `con_id`:
    /// its contract.
    NewsRefused { instrument: InstrumentId, con_id: i64, text: String },
    /// The contract's data is frozen (ibx#447): its requests go on with
    /// the frozen top of book of slot `frozen` (marketDataType 2).
    Frozen { instrument: InstrumentId, frozen: InstrumentId },
    /// The same, onto a frozen slot that has its data already: the
    /// option model's frozen pair, when type 2 is set while the requests
    /// run (ibx#494).
    FrozenWithData { instrument: InstrumentId, frozen: InstrumentId },
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
            | MdReject::DelayedFallback { instrument, .. }
            | MdReject::DelayedAtOnce { instrument }
            | MdReject::NoSecurityDefinition { instrument }
            | MdReject::CancelledDuringLookup { instrument }
            | MdReject::MaxTickers { instrument }
            | MdReject::NewsRefused { instrument, .. }
            | MdReject::Frozen { instrument, .. }
            | MdReject::FrozenWithData { instrument, .. }
            | MdReject::Live { instrument, .. } => instrument,
        }
    }
}

impl MarketDataState {
    fn new(stamp: &Arc<AtomicU64>, state: &Arc<StateMark>) -> Self {
        Self {
            quotes: Box::new(std::array::from_fn(|_| SeqQuote::new())),
            md_events: crate::md_events::MdQueue::with_clock(stamp.clone()),
            state: state.clone(),
            instrument_count: AtomicU64::new(0),
            tbt_trades: SeqQueue::new(&stamp),
            tbt_quotes: SeqQueue::new(&stamp),
            tbt_mid_points: SeqQueue::new(&stamp),
            real_time_bars: SeqQueue::new(&stamp),
            depth_updates: SeqQueue::new(&stamp),
            tick_news: SeqQueue::new(&stamp),
            generic_ticks: Mutex::new(Vec::new()),
            news_bulletins: Mutex::new(BulletinStore::default()),
            md_rejects: Mutex::new(Vec::new()),
            md_merges: Mutex::new(Vec::new()),
            md_resolved: Mutex::new(Vec::new()),
            md_kinds: Mutex::new(Vec::new()),
            top_acks: Mutex::new(Vec::new()),
            tick_req_params: Mutex::new(Vec::new()),
            early_tick_req_params: Mutex::new(Vec::new()),
            snapshot_acks: Mutex::new(Vec::new()),
            tbt_errors: SeqQueue::new(&stamp),
            error_tap: ErrorTap::default(),
            last_increments: Box::new(std::array::from_fn(|_| [AtomicU64::new(0), AtomicU64::new(0)])),
            precisions: Mutex::new(HashMap::new()),
        }
    }

    #[doc(hidden)] pub fn push_tbt_error(&self, req_id: ReqId, code: i32, text: String) {
        if self.error_tap.defer(ErrorQueue::TickByTick, req_id, code as i64, &text) { return; }
        let event = self.error_tap.event(req_id, code as i64, &text);
        self.tbt_errors.push((req_id, code, text));
        self.error_tap.send(event);
    }

    pub fn drain_tbt_errors(&self) -> Vec<(ReqId, i32, String)> {
        self.tbt_errors.drain()
    }

    #[doc(hidden)] pub fn push_tick_req_params(&self, params: TickReqParams) {
        let instrument = params.instrument;
        self.tick_req_params.lock().unwrap().push(params);
        self.error_tap.notify(|| Event::MarketDataParams(instrument));
        self.state.note();
    }

    pub fn drain_tick_req_params(&self) -> Vec<TickReqParams> {
        self.tick_req_params.lock().unwrap().drain(..).collect()
    }

    /// The request parameters of a contract asked before, given when a new
    /// subscription of it starts, before any acknowledgement; its market
    /// data type waits for the acknowledgement (ibx#535).
    #[doc(hidden)] pub fn push_early_tick_req_params(&self, params: TickReqParams, option: bool) {
        self.early_tick_req_params.lock().unwrap().push((params, option));
        self.state.note();
    }

    pub fn drain_early_tick_req_params(&self) -> Vec<(TickReqParams, bool)> {
        self.early_tick_req_params.lock().unwrap().drain(..).collect()
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
        // The refusal as the engine knows it, by instrument: a program
        // that reads only the event channel gets it without the client
        // (ibx#575). Built only when a channel is attached.
        let event = self.error_tap.copy(&reject).and_then(|reject| {
            let (code, message, _) = crate::client_core::ClientCore::md_reject_error(&reject);
            (code != 0).then(|| Event::MarketDataError { instrument: reject.instrument(), code, message })
        });
        self.md_rejects.lock().unwrap().push(reject);
        self.error_tap.emit(event);
        self.state.note();
    }

    pub fn drain_md_rejects(&self) -> Vec<MdReject> {
        self.md_rejects.lock().unwrap().drain(..).collect()
    }

    /// A request on slot `from` joined the subscription of slot `into`
    /// (ibx#444), at this point of the market data queue (ibx#446).
    #[doc(hidden)] pub fn push_md_merge(&self, from: InstrumentId, into: InstrumentId) {
        let at = self.md_events.position();
        self.md_merges.lock().unwrap().push((from, into, at));
        self.state.note();
    }

    /// The lookup of a request without a conId found its contract (ibx#444).
    #[doc(hidden)] pub fn push_md_resolved(&self, instrument: InstrumentId, con_id: i64) {
        self.md_resolved.lock().unwrap().push((instrument, con_id));
        self.state.note();
    }

    /// A snapshot's contract given by its conId alone has this type, from
    /// its definition (ibx#676).
    #[doc(hidden)] pub fn push_md_kind(&self, instrument: InstrumentId, sec_type: &str) {
        self.md_kinds.lock().unwrap().push((instrument, sec_type.to_string()));
    }

    /// A top-of-book subscription of the instrument was acknowledged now
    /// (ibx#494).
    pub fn push_top_ack(&self, instrument: InstrumentId, at: std::time::Instant) {
        self.top_acks.lock().unwrap().push((instrument, at));
    }

    pub fn drain_top_acks(&self) -> Vec<(InstrumentId, std::time::Instant)> {
        let mut acks = self.top_acks.lock().unwrap();
        if acks.is_empty() { return Vec::new(); }
        acks.drain(..).collect()
    }

    pub fn drain_md_kinds(&self) -> Vec<(InstrumentId, String)> {
        let mut kinds = self.md_kinds.lock().unwrap();
        if kinds.is_empty() { return Vec::new(); }
        kinds.drain(..).collect()
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
        self.tbt_trades.drain()
    }

    pub fn drain_tbt_quotes(&self) -> Vec<TbtQuote> {
        self.tbt_quotes.drain()
    }

    pub fn drain_tbt_mid_points(&self) -> Vec<TbtMidPoint> {
        self.tbt_mid_points.drain()
    }

    pub fn drain_real_time_bars(&self) -> Vec<(ReqId, RealTimeBar)> {
        self.real_time_bars.drain()
    }

    pub fn drain_depth_updates(&self) -> Vec<DepthUpdate> {
        self.depth_updates.drain()
    }

    pub fn drain_tick_news(&self) -> Vec<TickNews> {
        self.tick_news.drain()
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
        self.tbt_trades.push(trade);
    }

    #[doc(hidden)] pub fn push_tbt_quote(&self, quote: TbtQuote) {
        self.tbt_quotes.push(quote);
    }

    #[doc(hidden)] pub fn push_tbt_mid_point(&self, mid: TbtMidPoint) {
        self.tbt_mid_points.push(mid);
    }


    #[doc(hidden)] pub fn push_real_time_bar(&self, req_id: ReqId, bar: RealTimeBar) {
        let copy = self.error_tap.copy(&bar);
        self.real_time_bars.push((req_id, bar));
        self.error_tap.emit(copy.map(|bar| Event::RealTimeBar { req_id, bar }));
    }

    #[doc(hidden)] pub fn push_depth_update(&self, update: DepthUpdate) {
        let req_id = update.req_id;
        self.depth_updates.push(update);
        self.error_tap.notify(|| Event::DepthUpdate { req_id });
    }

    /// Remove all buffered depth updates for a given req_id (called on cancel).
    #[doc(hidden)] pub fn purge_depth_updates(&self, req_id: ReqId) {
        self.depth_updates.retain(|u| u.req_id != req_id);
    }

    #[doc(hidden)] pub fn push_generic_ticks(&self, ticks: GenericTicks) {
        let instrument = ticks.instrument;
        self.generic_ticks.lock().unwrap().push(ticks);
        self.error_tap.notify(|| Event::GenericTicks(instrument));
    }

    #[doc(hidden)] pub fn push_tick_news(&self, news: TickNews) {
        self.tick_news.push(news);
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
        let copy = self.error_tap.copy(&bulletin);
        store.store.push(bulletin.clone());
        store.queue.push(bulletin);
        drop(store);
        self.state.note();
        self.error_tap.emit(copy.map(Event::NewsBulletin));
        true
    }

    #[doc(hidden)] pub fn set_instrument_count(&self, count: u32) {
        self.instrument_count.store(count as u64, Ordering::Relaxed);
    }

    /// The size and price increments of a slot's contract, packed, 0 when
    /// not known (ibx#613). Set before its subscription goes out, so before
    /// any step of it is queued.
    #[doc(hidden)] pub fn set_last_increments(&self, id: InstrumentId, size: u64, price: u64) {
        if let Some([s, p]) = self.last_increments.get(id as usize) {
            s.store(size, Ordering::Relaxed);
            p.store(price, Ordering::Relaxed);
        }
    }

    /// The last price and last size precisions of a contract's definition
    /// (ibx#725); an empty text for one it does not give.
    #[doc(hidden)] pub fn set_precisions(&self, con_id: i64, price: Option<f64>, size: Option<f64>) {
        if price.is_some() || size.is_some() {
            let text = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
            self.precisions.lock().unwrap().insert(con_id, (text(price), text(size)));
        }
    }

    /// The precisions of a contract whose definition was read (ibx#725).
    pub fn precisions(&self, con_id: i64) -> Option<(String, String)> {
        self.precisions.lock().unwrap().get(&con_id).cloned()
    }

    /// The size and price increments of a slot's contract (ibx#613): two
    /// loads, no lock.
    #[inline]
    pub fn last_increments(&self, id: InstrumentId) -> (u64, u64) {
        match self.last_increments.get(id as usize) {
            Some([s, p]) => (s.load(Ordering::Relaxed), p.load(Ordering::Relaxed)),
            None => (0, 0),
        }
    }
}

/// An order status, or a notice given after a status (201, 202, 161), as
/// [`drain_order_reports`](OrderState::drain_order_reports) gives them.
#[derive(Debug, Clone)]
pub enum OrderReport {
    Update(OrderUpdate),
    /// Order id, code and text.
    Notice(i64, i64, String),
}

/// Fills, order status updates, cancel rejects, what-if responses, and order cache.
/// Bits of `OrderState::dropped_flags` (ibx#607).
pub const DROPPED_ALLOW_PRE_OPEN: u8 = 1;
pub const DROPPED_IGNORE_OPEN_AUCTION: u8 = 2;
pub const DROPPED_DEACTIVATE: u8 = 4;
pub const DROPPED_SEEK_PRICE_IMPROVEMENT: u8 = 8;

pub struct OrderState {
    /// Fills with what the report says about each execution.
    pub(crate) fills: SeqQueue<(Fill, FillExec)>,
    /// Commission reports from the server's commission frame (ibx#471).
    pub(crate) commission_reports: SeqQueue<api::CommissionAndFeesReport>,
    /// Executions of orders the engine does not track, for the execution
    /// store only: no live fill callback without a known order (ibx#314).
    pub(crate) untracked_executions: SeqQueue<(api::Contract, api::Execution, FillExec)>,
    /// The order statuses and the notices of a server report given after
    /// the status of that report (the reject 201, the cancel 202, ibx#486),
    /// in the order they were written (ibx#532).
    pub(crate) order_reports: SeqQueue<OrderReport>,
    cancel_rejects: Mutex<Vec<CancelReject>>,
    /// Errors raised before sending: (request or order id as the API gives
    /// it, -1 for none; code; message) (ibx#349, ibx#285).
    pub(crate) order_errors: SeqQueue<(i64, i64, String)>,
    /// The flags the engine dropped from an order before sending it, as
    /// `DROPPED_*` bits: the order's openOrder shows them off (ibx#607).
    dropped_flags: Mutex<HashMap<i64, u8>>,
    /// The count of the session's answers when a report of the server
    /// first gave each order as held: the openOrder of a report written
    /// before shows the order as it was then.
    held_since: Mutex<HashMap<OrderId, u64>>,
    /// The orders held with transmit off whose placeOrder's lookups have
    /// not all answered yet: the session does not know them as parents yet
    /// (ibx#718). False until the engine took the order.
    held_unknown: Mutex<HashMap<OrderId, bool>>,
    held_unknown_changed: Condvar,
    error_tap: ErrorTap,
    pub(crate) what_if_responses: SeqQueue<WhatIfResponse>,
    completed_orders: Mutex<Vec<CompletedOrder>>,
    /// The answers of reqCompletedOrders, from the server (ibx#695).
    pub(crate) completed_rows: SeqQueue<CompletedRow>,
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
    pub(crate) forgotten_orders: SeqQueue<OrderId>,
    /// The API order id of an order of another session whose id differs
    /// from the engine's key: the report's 6121, 0 when it has none.
    api_order_ids: Mutex<HashMap<OrderId, OrderId>>,
    /// The place of each order in the reference's book (insertion number)
    /// and the most orders the book held, for the order of the open-order
    /// listings.
    book_seqs: Mutex<HashMap<OrderId, u64>>,
    book_peak: std::sync::atomic::AtomicUsize,
    /// The lowest negative API order id a report of the session gave (0:
    /// none): the profit taker of a scale order is numbered below it
    /// (ibx#751).
    lowest_child_id: std::sync::atomic::AtomicI64,
    /// The highest API order id the server's reports gave for each
    /// API client: the ids a client used in earlier sessions, as far
    /// as the server's replays show them.
    reported_order_ids: Mutex<HashMap<i64, OrderId>>,
}

impl OrderState {
    fn new(stamp: &Arc<AtomicU64>) -> Self {
        Self {
            combo_views: Mutex::new(HashMap::new()),
            fills: SeqQueue::new(&stamp),
            commission_reports: SeqQueue::new(&stamp),
            untracked_executions: SeqQueue::new(&stamp),
            order_reports: SeqQueue::new(stamp),
            cancel_rejects: Mutex::new(Vec::with_capacity(16)),
            order_errors: SeqQueue::new(&stamp),
            dropped_flags: Mutex::new(HashMap::new()),
            held_since: Mutex::new(HashMap::new()),
            held_unknown: Mutex::new(HashMap::new()),
            held_unknown_changed: Condvar::new(),
            error_tap: ErrorTap::default(),
            what_if_responses: SeqQueue::new(&stamp),
            completed_orders: Mutex::new(Vec::with_capacity(64)),
            completed_rows: SeqQueue::new(&stamp),
            order_cache: Mutex::new(HashMap::new()),
            open_orders_held: AtomicBool::new(false),
            login_orders_end: AtomicBool::new(false),
            forgotten_orders: SeqQueue::new(&stamp),
            api_order_ids: Mutex::new(HashMap::new()),
            book_seqs: Mutex::new(HashMap::new()),
            book_peak: std::sync::atomic::AtomicUsize::new(0),
            lowest_child_id: std::sync::atomic::AtomicI64::new(0),
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

    /// A negative API order id a report gave (engine side, ibx#751).
    #[doc(hidden)] pub fn note_child_id(&self, id: i64) {
        self.lowest_child_id.fetch_min(id, std::sync::atomic::Ordering::Relaxed);
    }

    /// The lowest negative API order id the session's reports gave, 0 for
    /// none.
    pub fn lowest_child_id(&self) -> i64 {
        self.lowest_child_id.load(std::sync::atomic::Ordering::Relaxed)
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
        self.fills.drain().into_iter().map(|(fill, _)| fill).collect()
    }

    /// Fills with their execution details (empty when injected without).
    pub fn drain_fills_with_exec(&self) -> Vec<(Fill, FillExec)> {
        self.fills.drain()
    }

    /// Executions of untracked orders, to store for `req_executions`
    /// (ibx#314).
    pub fn drain_untracked_executions(&self) -> Vec<(api::Contract, api::Execution, FillExec)> {
        self.untracked_executions.drain()
    }

    pub fn drain_commission_reports(&self) -> Vec<api::CommissionAndFeesReport> {
        self.commission_reports.drain()
    }

    /// The order statuses written; the notices stay.
    pub fn drain_order_updates(&self) -> Vec<OrderUpdate> {
        self.order_reports.take_some(|r| match r {
            OrderReport::Update(update) => Ok(update),
            notice => Err(notice),
        })
    }

    /// The order statuses and the notices that follow a status (201, 202,
    /// 161), in the order they were written (ibx#532). They were once two
    /// queues read one after the other: a notice written while the statuses
    /// were being given came out before the status written just before it
    /// (a 202 before its Cancelled, paper 11/10/2026).
    pub fn drain_order_reports(&self) -> Vec<OrderReport> {
        self.order_reports.drain()
    }

    /// [`drain_order_reports`](Self::drain_order_reports) with their place
    /// among all the answers.
    pub fn drain_order_reports_stamped(&self) -> Vec<(u64, OrderReport)> {
        self.order_reports.drain_stamped()
    }

    pub fn drain_cancel_rejects(&self) -> Vec<CancelReject> {
        self.cancel_rejects.lock().unwrap().drain(..).collect()
    }

    /// Note flags dropped from an order before sending (`DROPPED_*` bits,
    /// ibx#607).
    #[doc(hidden)] pub fn note_dropped_flags(&self, order_id: i64, bits: u8) {
        *self.dropped_flags.lock().unwrap().entry(order_id).or_default() |= bits;
    }

    /// The flags dropped from an order before sending (`DROPPED_*` bits).
    pub fn dropped_flags(&self, order_id: i64) -> u8 {
        self.dropped_flags.lock().unwrap().get(&order_id).copied().unwrap_or(0)
    }

    /// An order just held with transmit off: not known as a parent until
    /// the lookups of its placeOrder answered (client side, ibx#718).
    #[doc(hidden)] pub fn mark_held_unknown(&self, order_id: OrderId) {
        self.held_unknown.lock().unwrap().insert(order_id, false);
    }

    /// The engine took a held order whose lookups have not all answered
    /// (engine side, ibx#718).
    #[doc(hidden)] pub fn held_waiting(&self, order_id: OrderId) {
        if let Some(taken) = self.held_unknown.lock().unwrap().get_mut(&order_id)
            && !*taken
        {
            *taken = true;
            self.held_unknown_changed.notify_all();
        }
    }

    /// The lookups of a held order's placeOrder answered, or the order left
    /// the held ones: it is known (engine side, ibx#718).
    #[doc(hidden)] pub fn held_known(&self, order_id: OrderId) {
        if self.held_unknown.lock().unwrap().remove(&order_id).is_some() {
            self.held_unknown_changed.notify_all();
        }
    }

    /// Whether a held order is still noted as unknown, without waiting
    /// (engine side, ibx#718): the engine decides for it, it never waits
    /// for itself.
    #[doc(hidden)] pub fn held_pending(&self, order_id: OrderId) -> bool {
        self.held_unknown.lock().unwrap().contains_key(&order_id)
    }

    /// Whether a held order still waits for the lookups of its placeOrder
    /// (ibx#718). An order the engine has not taken yet is waited for (at
    /// most a second; the engine decides as it takes the order, so the wait
    /// is that of the command's way to it), so that a later request reads
    /// what the engine decided for it.
    pub fn is_held_unknown(&self, order_id: OrderId) -> bool {
        let held = self.held_unknown.lock().unwrap();
        let (held, _) = self.held_unknown_changed
            .wait_timeout_while(held, std::time::Duration::from_secs(1), |h| h.get(&order_id) == Some(&false))
            .unwrap();
        held.contains_key(&order_id)
    }

    /// Order errors raised before anything was sent: (order id, code, message).
    pub fn drain_order_errors(&self) -> Vec<(i64, i64, String)> {
        self.order_errors.drain()
    }

    /// The notices to give after the order statuses (ibx#486).
    pub fn drain_order_notices(&self) -> Vec<(i64, i64, String)> {
        self.order_reports.take_some(|r| match r {
            OrderReport::Notice(order_id, code, message) => Ok((order_id, code, message)),
            update => Err(update),
        })
    }

    pub fn drain_what_if_responses(&self) -> Vec<WhatIfResponse> {
        self.what_if_responses.drain()
    }

    pub fn drain_completed_orders(&self) -> Vec<CompletedOrder> {
        self.completed_orders.lock().unwrap().drain(..).collect()
    }

    /// Snapshot enriched entries whose latest status is an open broker order state.
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
        self.fills.push((fill, FillExec::default()));
    }

    #[doc(hidden)] pub fn push_fill_with_exec(&self, fill: Fill, exec: FillExec) {
        self.fills.push((fill, exec));
    }

    /// An order the client no longer knows, with no callback (ibx#251).
    #[doc(hidden)] pub fn push_forgotten_order(&self, order_id: OrderId) {
        self.forgotten_orders.push(order_id);
    }

    /// The orders the engine dropped with no status since the last call
    /// (ibx#251).
    pub fn drain_forgotten_orders(&self) -> Vec<OrderId> {
        self.forgotten_orders.drain()
    }

    #[doc(hidden)] pub fn push_untracked_execution(&self, contract: api::Contract, execution: api::Execution, exec: FillExec) {
        self.untracked_executions.push((contract, execution, exec));
    }

    #[doc(hidden)] pub fn push_commission_report(&self, report: api::CommissionAndFeesReport) {
        self.push_commission_report_to(report, true);
    }

    /// A commission report for the client's queue, and for the event
    /// channel only with `to_events`: the engine gives it there after the
    /// `Fill` of its execution only (ibx#644), as the callbacks give no
    /// commission report without its execution.
    #[doc(hidden)] pub fn push_commission_report_to(&self, report: api::CommissionAndFeesReport, to_events: bool) {
        let copy = if to_events { self.error_tap.copy(&report) } else { None };
        self.commission_reports.push(report);
        self.error_tap.emit(copy.map(|report| Event::CommissionReport(report)));
    }

    /// Whether an event channel is attached.
    #[doc(hidden)] pub fn events_attached(&self) -> bool {
        self.error_tap.events.get().is_some()
    }

    #[doc(hidden)] pub fn push_order_update(&self, update: OrderUpdate) {
        self.order_reports.push(OrderReport::Update(update));
    }

    #[doc(hidden)] pub fn push_cancel_reject(&self, reject: CancelReject) {
        self.cancel_rejects.lock().unwrap().push(reject);
    }

    #[doc(hidden)] pub fn push_order_error(&self, order_id: i64, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::Order, order_id, code, &message) { return; }
        let event = self.error_tap.event(order_id, code, &message);
        self.order_errors.push((order_id, code, message));
        self.error_tap.send(event);
    }

    /// A notice of a server report (201, 202), given after the status the
    /// same report gives, as the reference writes them (ibx#486).
    #[doc(hidden)] pub fn push_order_notice(&self, order_id: i64, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::OrderNotice, order_id, code, &message) { return; }
        let event = self.error_tap.event(order_id, code, &message);
        {
            self.order_reports.push(OrderReport::Notice(order_id, code, message));
        }
        self.error_tap.send(event);
    }

    #[doc(hidden)] pub fn push_what_if(&self, response: WhatIfResponse) {
        self.what_if_responses.push(response);
    }

    #[doc(hidden)] pub fn push_completed_order(&self, order: CompletedOrder) {
        let copy = self.error_tap.copy(&order);
        {
            // reqCompletedOrders asks the server (ibx#695): this list only
            // feeds the event channel and the tests; it keeps the latest.
            let mut kept = self.completed_orders.lock().unwrap();
            if kept.len() >= 1024 {
                kept.drain(..512);
            }
            kept.push(order);
        }
        self.error_tap.emit(copy.map(|order| Event::CompletedOrder(order)));
    }

    /// A row of the answer of reqCompletedOrders, or its end (ibx#695).
    #[doc(hidden)] pub fn push_completed_row(&self, row: CompletedRow) {
        self.completed_rows.push(row);
    }

    /// Note that a report gave the order as held: every order report
    /// written from now on counts at or after it. The first time stays.
    #[doc(hidden)] pub fn note_held(&self, order_id: OrderId) {
        let at = self.order_reports.clock.load(Ordering::Relaxed);
        self.held_since.lock().unwrap().entry(order_id).or_insert(at);
    }

    /// The count of the session's answers from which the order is held;
    /// None while no report gave it as held.
    pub fn held_since(&self, order_id: OrderId) -> Option<u64> {
        self.held_since.lock().unwrap().get(&order_id).copied()
    }

    #[doc(hidden)] pub fn push_order_info(&self, order_id: OrderId, info: RichOrderInfo) {
        let copy = self.error_tap.copy(&info);
        self.order_cache.lock().unwrap().insert(order_id, info);
        self.error_tap.emit(copy.map(|info| Event::OpenOrder { order_id, info }));
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
    /// The state the client reads changed (ibx#662).
    pub(crate) state: Arc<StateMark>,
    pub(crate) historical_data: SeqQueue<(ReqId, HistoricalResponse)>,
    /// Bars of keepUpToDate requests, the whole current bar each time
    /// (ibx#429), for historicalDataUpdate.
    pub(crate) historical_updates: SeqQueue<(ReqId, HistoricalBar)>,
    pub(crate) head_timestamps: SeqQueue<(ReqId, HeadTimestampResponse)>,
    pub(crate) contract_details: SeqQueue<(ReqId, ContractDefinition)>,
    pub(crate) contract_details_end: SeqQueue<ReqId>,
    pub(crate) matching_symbols: SeqQueue<(ReqId, Vec<SymbolMatch>)>,
    /// Option chain answers (ibx#440): the rows of a request, then its end.
    pub(crate) option_chains: SeqQueue<(ReqId, Vec<crate::control::optparams::OptionChain>)>,
    scanner_params: Mutex<Vec<String>>,
    pub(crate) scanner_data: SeqQueue<(ReqId, ScannerResult)>,
    pub(crate) historical_news: SeqQueue<(ReqId, Vec<NewsHeadline>, bool)>,
    pub(crate) news_articles: SeqQueue<(ReqId, i32, String)>,
    pub(crate) fundamental_data: SeqQueue<(ReqId, String)>,
    pub(crate) option_computations: SeqQueue<crate::control::optcalc::OptionComputation>,
    pub(crate) histogram_data: SeqQueue<(ReqId, Vec<HistogramEntry>)>,
    pub(crate) historical_ticks: SeqQueue<(ReqId, HistoricalTickData, String, bool)>,
    pub(crate) historical_schedules: SeqQueue<(ReqId, HistoricalScheduleResponse)>,
    /// Errors surfaced by HMDS for in-flight reference queries (req_id, code, message).
    /// Drained by the dispatcher and forwarded to `Wrapper::error`. ibx#186.
    pub(crate) historical_errors: SeqQueue<(ReqId, i32, String)>,
    error_tap: ErrorTap,
    market_rules: Mutex<Vec<MarketRule>>,
    /// The market rule of each contract whose definition came (ibx#615).
    rule_of_contract: Mutex<HashMap<i64, i32>>,
    depth_exchanges_cache: Mutex<Vec<DepthMktDataDescription>>,
    depth_exchanges_pending: Mutex<bool>,
    /// Contract cache from CCP exec reports (con_id -> api::Contract).
    contract_cache: Mutex<HashMap<i64, api::Contract>>,
    /// The primary exchange and trading class the portfolio's batch lookup
    /// gave for a contract already known from a trade report of the day:
    /// what a position row shows (the contract cache keeps the report's).
    position_venues: Mutex<HashMap<i64, (String, String)>>,
    /// The local symbol of each contract whose definition was read.
    definition_symbols: Mutex<HashMap<i64, String>>,
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
    /// The feature list of the last logon reply that had one, as it came
    /// (ibx#571): the scanner parameters of a client depend on its names.
    logon_features: Mutex<String>,
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
    /// The account ids of the logon's account list, in logon order
    /// (ibx#420).
    managed_accounts: Mutex<Vec<String>>,
    /// The accounts whose application is not approved (the pending accounts
    /// field of the last logon reply or logon update, ibx#421).
    pending_accounts: Mutex<Vec<String>>,
    /// FA session, from the FA flag of the server's logon (ibx#481).
    fa_session: std::sync::atomic::AtomicBool,
    /// The FA data of an FA session and its saves not answered (ibx#507).
    fa: Mutex<crate::control::fa::FaData>,
    /// The replaces whose saves all succeeded, for `replace_fa_end`
    /// (ibx#507).
    fa_saved: Mutex<Vec<i64>>,
    /// The answers of the calendar data requests as the client gets them,
    /// and the state of those requests (ibx#443).
    pub(crate) wsh_answers: SeqQueue<crate::client_core::WshOut>,
    pub(crate) wsh: Mutex<crate::client_core::WshState>,
    /// A paper session, from the logon (ibx#444).
    paper_session: std::sync::atomic::AtomicBool,
    /// The logon's super user and omnibus flags (ibx#417): either one lets
    /// a short-side order pass the side check.
    super_user: AtomicBool,
    omnibus: AtomicBool,
    /// The smart combo conId of each currency, from the logon's smart
    /// combo field (ibx#470).
    smart_combo_con_ids: Mutex<HashMap<String, i64>>,
    /// The algo providers of the logon (ibx#505).
    algo_providers: Mutex<Vec<String>>,
    /// The API client id the new orders carry (ibx#466); 0 until set.
    api_client_id: std::sync::atomic::AtomicI64,
    /// The form of the reference 10.40, for the replays of its
    /// recordings: the overnight times in force taken (ibx#660), the
    /// settled cash rows without their currency (ibx#637).
    form_10_40: std::sync::atomic::AtomicBool,
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
    /// Account config (the account config message): feature list and MiFID config id; None
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
    /// Most years of a historical data request, from the logon's backfill limit;
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
    /// The logon features that change the entries of a market data
    /// request (ibx#648), shared with the engine's request writers.
    md_entry_marks: Arc<AtomicU8>,
    /// The last server text of the scanner parameters rebuilt for the event
    /// channel, and its rebuild (ibx#574): one rebuild for the same text.
    scanner_params_rebuilt: Arc<Mutex<Option<(String, String)>>>,
}

impl ReferenceState {
    fn new(stamp: &Arc<AtomicU64>, state: &Arc<StateMark>) -> Self {
        Self {
            state: state.clone(),
            scanner_params_rebuilt: Arc::new(Mutex::new(None)),
            historical_data: SeqQueue::new(&stamp),
            historical_updates: SeqQueue::new(&stamp),
            head_timestamps: SeqQueue::new(&stamp),
            contract_details: SeqQueue::new(&stamp),
            contract_details_end: SeqQueue::new(&stamp),
            matching_symbols: SeqQueue::new(&stamp),
            scanner_params: Mutex::new(Vec::new()),
            scanner_data: SeqQueue::new(&stamp),
            historical_news: SeqQueue::new(&stamp),
            news_articles: SeqQueue::new(&stamp),
            fundamental_data: SeqQueue::new(&stamp),
            option_computations: SeqQueue::new(&stamp),
            histogram_data: SeqQueue::new(&stamp),
            historical_ticks: SeqQueue::new(&stamp),
            historical_schedules: SeqQueue::new(&stamp),
            historical_errors: SeqQueue::new(&stamp),
            error_tap: ErrorTap::default(),
            market_rules: Mutex::new(Vec::new()),
            rule_of_contract: Mutex::new(HashMap::new()),
            depth_exchanges_cache: Mutex::new(Vec::new()),
            depth_exchanges_pending: Mutex::new(false),
            contract_cache: Mutex::new(HashMap::new()),
            position_venues: Mutex::new(HashMap::new()),
            definition_symbols: Mutex::new(HashMap::new()),
            market_names: Mutex::new(HashMap::new()),
            time_zone_ids: Mutex::new(HashMap::new()),
            exchange_maps: Mutex::new(Vec::new()),
            instrument_exchange_maps: Mutex::new(HashMap::new()),
            exchange_maps_at: Mutex::new(HashMap::new()),
            news_providers: Mutex::new(Vec::new()),
            logon_features: Mutex::new(String::new()),
            news_sources: Mutex::new(Vec::new()),
            news_sources_unsubscribed: Mutex::new(Vec::new()),
            soft_dollar_tiers: Mutex::new(Vec::new()),
            family_codes: Mutex::new(Vec::new()),
            white_branding_id: Mutex::new(String::new()),
            managed_accounts: Mutex::new(Vec::new()),
            pending_accounts: Mutex::new(Vec::new()),
            fa_session: std::sync::atomic::AtomicBool::new(false),
            fa: Mutex::new(crate::control::fa::FaData::default()),
            fa_saved: Mutex::new(Vec::new()),
            wsh_answers: SeqQueue::new(&stamp),
            wsh: Mutex::new(Default::default()),
            paper_session: std::sync::atomic::AtomicBool::new(false),
            super_user: AtomicBool::new(false),
            omnibus: AtomicBool::new(false),
            smart_combo_con_ids: Mutex::new(HashMap::new()),
            algo_providers: Mutex::new(Vec::new()),
            api_client_id: std::sync::atomic::AtomicI64::new(0),
            form_10_40: std::sync::atomic::AtomicBool::new(false),
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
            md_entry_marks: Arc::new(AtomicU8::new(0)),
            option_chains: SeqQueue::new(&stamp),
        }
    }

    pub fn drain_historical_data(&self) -> Vec<(ReqId, HistoricalResponse)> {
        self.historical_data.drain()
    }

    pub fn drain_historical_updates(&self) -> Vec<(ReqId, HistoricalBar)> {
        self.historical_updates.drain()
    }

    pub fn drain_head_timestamps(&self) -> Vec<(ReqId, HeadTimestampResponse)> {
        self.head_timestamps.drain()
    }

    pub fn drain_contract_details(&self) -> Vec<(ReqId, ContractDefinition)> {
        self.contract_details.drain()
    }

    pub fn drain_contract_details_end(&self) -> Vec<ReqId> {
        self.contract_details_end.drain()
    }

    pub fn drain_matching_symbols(&self) -> Vec<(ReqId, Vec<SymbolMatch>)> {
        self.matching_symbols.drain()
    }

    /// Option chain answers (ibx#440): the rows of each request, for one
    /// SECURITY_DEFINITION_OPTION_PARAMETER each, then its end.
    pub fn drain_option_chains(&self) -> Vec<(ReqId, Vec<crate::control::optparams::OptionChain>)> {
        self.option_chains.drain()
    }

    #[doc(hidden)] pub fn push_option_chains(&self, req_id: ReqId, rows: Vec<crate::control::optparams::OptionChain>) {
        let copy = self.error_tap.copy(&rows);
        self.option_chains.push((req_id, rows));
        self.error_tap.emit(copy.map(|chains| Event::OptionChains { req_id, chains }));
    }

    pub fn drain_scanner_params(&self) -> Vec<String> {
        self.scanner_params.lock().unwrap().drain(..).collect()
    }

    pub fn drain_scanner_data(&self) -> Vec<(ReqId, ScannerResult)> {
        self.scanner_data.drain()
    }

    pub fn drain_historical_news(&self) -> Vec<(ReqId, Vec<NewsHeadline>, bool)> {
        self.historical_news.drain()
    }

    pub fn drain_news_articles(&self) -> Vec<(ReqId, i32, String)> {
        self.news_articles.drain()
    }

    pub fn drain_fundamental_data(&self) -> Vec<(ReqId, String)> {
        self.fundamental_data.drain()
    }

    /// Answers of option calculations, in arrival order.
    pub fn drain_option_computations(&self) -> Vec<crate::control::optcalc::OptionComputation> {
        self.option_computations.drain()
    }

    pub fn drain_histogram_data(&self) -> Vec<(ReqId, Vec<HistogramEntry>)> {
        self.histogram_data.drain()
    }

    pub fn drain_historical_ticks(&self) -> Vec<(ReqId, HistoricalTickData, String, bool)> {
        self.historical_ticks.drain()
    }

    pub fn drain_historical_schedules(&self) -> Vec<(ReqId, HistoricalScheduleResponse)> {
        self.historical_schedules.drain()
    }

    pub fn drain_historical_errors(&self) -> Vec<(ReqId, i32, String)> {
        self.historical_errors.drain()
    }

    /// The market rule of a contract, from its definition (ibx#615).
    #[doc(hidden)] pub fn note_contract_rule(&self, con_id: i64, rule_id: i32) {
        self.rule_of_contract.lock().unwrap().insert(con_id, rule_id);
    }

    /// True when the market rule of a contract is known (ibx#615).
    #[doc(hidden)] pub fn knows_contract_rule(&self, con_id: i64) -> bool {
        self.rule_of_contract.lock().unwrap().contains_key(&con_id)
    }

    /// The price step of a contract's market rule at `price` (ibx#615):
    /// the increment of the last tier whose low edge is at or below the
    /// price. None when the contract's rule is not known.
    pub fn price_step(&self, con_id: i64, price: f64) -> Option<f64> {
        let rule_id = *self.rule_of_contract.lock().unwrap().get(&con_id)?;
        let rules = self.market_rules.lock().unwrap();
        let rule = rules.iter().find(|r| r.rule_id == rule_id)?;
        rule.price_increments.iter().filter(|t| t.low_edge <= price.abs()).last()
            .or(rule.price_increments.first()).map(|t| t.increment).filter(|s| *s > 0.0)
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
        self.historical_data.push((req_id, response));
    }

    #[doc(hidden)] pub fn push_historical_update(&self, req_id: ReqId, bar: HistoricalBar) {
        let copy = self.error_tap.copy(&bar);
        self.historical_updates.push((req_id, bar));
        self.error_tap.emit(copy.map(|bar| Event::HistoricalUpdate { req_id, bar }));
    }

    /// Drop the updates of a request not delivered yet: none goes out
    /// after its cancel (ibx#429).
    #[doc(hidden)] pub fn purge_historical_updates(&self, req_id: ReqId) {
        self.historical_updates.retain(|(r, _)| *r != req_id);
    }

    #[doc(hidden)] pub fn push_head_timestamp(&self, req_id: ReqId, response: HeadTimestampResponse) {
        self.head_timestamps.push((req_id, response));
    }

    #[doc(hidden)] pub fn push_contract_details(&self, req_id: ReqId, def: ContractDefinition) {
        self.contract_details.push((req_id, def));
    }

    #[doc(hidden)] pub fn push_contract_details_end(&self, req_id: ReqId) {
        self.contract_details_end.push(req_id);
    }

    #[doc(hidden)] pub fn push_matching_symbols(&self, req_id: ReqId, matches: Vec<SymbolMatch>) {
        self.matching_symbols.push((req_id, matches));
    }

    #[doc(hidden)] pub fn push_scanner_params(&self, xml: String) {
        // The channel gets the text of the callback (ibx#574), rebuilt off
        // the engine thread.
        if let Some(tx) = self.error_tap.events.get().cloned() {
            let providers = self.news_providers();
            let session = crate::control::scanner_params::Session::new(
                &self.logon_features(), providers.iter().map(|p| p.code.as_str()))
                .with_clock_offset(self.clock().offset_ms());
            let server_xml = xml.clone();
            let cache = self.scanner_params_rebuilt.clone();
            let _ = std::thread::Builder::new().name("ibx-scanner-params".into()).spawn(move || {
                let rebuilt = {
                    let mut kept = cache.lock().unwrap();
                    match kept.as_ref() {
                        Some((text, done)) if *text == server_xml => Some(done.clone()),
                        _ => {
                            let done = crate::control::scanner_params::for_client(&server_xml, &session);
                            *kept = done.clone().map(|d| (server_xml, d));
                            done
                        }
                    }
                };
                if let Some(rebuilt) = rebuilt {
                    let _ = tx.try_send(Event::ScannerParameters(rebuilt));
                }
            });
        }
        self.scanner_params.lock().unwrap().push(xml);
        self.state.note();
    }

    #[doc(hidden)] pub fn push_scanner_data(&self, req_id: ReqId, result: ScannerResult) {
        let copy = self.error_tap.copy(&result);
        self.scanner_data.push((req_id, result));
        self.error_tap.emit(copy.map(|result| Event::ScannerData { req_id, result }));
    }

    /// Drop the queued results of a cancelled scanner (ibx#457).
    #[doc(hidden)] pub fn discard_scanner_data(&self, req_id: ReqId) {
        self.scanner_data.retain(|(r, _)| *r != req_id);
    }

    #[doc(hidden)] pub fn push_historical_news(&self, req_id: ReqId, headlines: Vec<NewsHeadline>, has_more: bool) {
        let copy = self.error_tap.copy(&headlines);
        self.historical_news.push((req_id, headlines, has_more));
        self.error_tap.emit(copy.map(|headlines| Event::HistoricalNews { req_id, headlines, has_more }));
    }

    #[doc(hidden)] pub fn push_news_article(&self, req_id: ReqId, article_type: i32, article_text: String) {
        let copy = self.error_tap.copy(&article_text);
        self.news_articles.push((req_id, article_type, article_text));
        self.error_tap.emit(copy.map(|text| Event::NewsArticle { req_id, article_type, text }));
    }

    #[doc(hidden)] pub fn push_option_computation(&self, answer: crate::control::optcalc::OptionComputation) {
        let copy = self.error_tap.copy(&answer);
        self.option_computations.push(answer);
        self.error_tap.emit(copy.map(|answer| Event::OptionComputation(answer)));
    }

    #[doc(hidden)] pub fn push_fundamental_data(&self, req_id: ReqId, data: String) {
        let copy = self.error_tap.copy(&data);
        self.fundamental_data.push((req_id, data));
        self.error_tap.emit(copy.map(|data| Event::FundamentalData { req_id, data }));
    }

    #[doc(hidden)] pub fn push_histogram_data(&self, req_id: ReqId, entries: Vec<HistogramEntry>) {
        let copy = self.error_tap.copy(&entries);
        self.histogram_data.push((req_id, entries));
        self.error_tap.emit(copy.map(|entries| Event::HistogramData { req_id, entries }));
    }

    #[doc(hidden)] pub fn push_historical_ticks(&self, req_id: ReqId, data: HistoricalTickData, what_to_show: String, done: bool) {
        let copy = self.error_tap.copy(&data).zip(self.error_tap.copy(&what_to_show));
        self.historical_ticks.push((req_id, data, what_to_show, done));
        self.error_tap.emit(copy.map(|(data, what_to_show)| Event::HistoricalTicks { req_id, data, what_to_show, done }));
    }

    #[doc(hidden)] pub fn push_historical_schedule(&self, req_id: ReqId, response: HistoricalScheduleResponse) {
        let copy = self.error_tap.copy(&response);
        self.historical_schedules.push((req_id, response));
        self.error_tap.emit(copy.map(|data| Event::HistoricalSchedule { req_id, data }));
    }

    #[doc(hidden)] pub fn push_historical_error(&self, req_id: ReqId, code: i32, message: String) {
        if self.error_tap.defer(ErrorQueue::Historical, req_id, code as i64, &message) { return; }
        let event = self.error_tap.event(req_id, code as i64, &message);
        self.historical_errors.push((req_id, code, message));
        self.error_tap.send(event);
    }

    #[doc(hidden)] pub fn push_market_rules(&self, rules: Vec<MarketRule>) {
        let mut lock = self.market_rules.lock().unwrap();
        let known = lock.len();
        for rule in rules {
            if !lock.iter().any(|r| r.rule_id == rule.rule_id) {
                lock.push(rule);
            }
        }
        // The rules not known before, once (ibx#498).
        if lock.len() > known {
            self.error_tap.emit(self.error_tap.copy(&lock[known..].to_vec()).map(Event::MarketRules));
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
        self.state.note();
    }

    /// The primary exchange and trading class of a position's contract as
    /// the portfolio's batch lookup gave them (engine side).
    #[doc(hidden)] pub fn note_position_venue(&self, con_id: i64, primary_exchange: &str, trading_class: &str) {
        self.position_venues.lock().unwrap().insert(con_id, (primary_exchange.to_string(), trading_class.to_string()));
    }

    /// The primary exchange and trading class the portfolio's batch lookup
    /// gave for `con_id`, when the contract cache holds another view of it.
    pub fn position_venue(&self, con_id: i64) -> Option<(String, String)> {
        self.position_venues.lock().unwrap().get(&con_id).cloned()
    }

    /// The local symbol a contract's definition gives (engine side).
    #[doc(hidden)] pub fn note_definition_symbol(&self, con_id: i64, local_symbol: &str) {
        if con_id != 0 && !local_symbol.is_empty() {
            self.definition_symbols.lock().unwrap().insert(con_id, local_symbol.to_string());
        }
    }

    /// The local symbol of a contract's definition, once it was read: the
    /// one a position and an execution show, where a trade report of a
    /// stock of another market carries another code (the reference 10.51,
    /// paper, 13/11/2026: "BMW" for BMW on IBIS).
    pub fn definition_local_symbol(&self, con_id: i64) -> Option<String> {
        self.definition_symbols.lock().unwrap().get(&con_id).cloned()
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

    /// Whether `account`'s application is not approved yet (the pending
    /// accounts of the logon, ibx#421).
    pub fn account_pending(&self, account: &str) -> bool {
        self.pending_accounts.lock().unwrap().iter().any(|a| a == account)
    }

    /// The accounts of the logon's list whose application is not
    /// approved, in list order (ibx#421).
    pub fn pending_managed_accounts(&self) -> Vec<String> {
        let pending = self.pending_accounts.lock().unwrap();
        self.managed_accounts.lock().unwrap().iter().filter(|a| pending.contains(a)).cloned().collect()
    }

    /// The FA data of the session (ibx#507).
    pub fn fa_data(&self) -> std::sync::MutexGuard<'_, crate::control::fa::FaData> {
        self.fa.lock().unwrap()
    }

    /// The server's answer to a calendar data request (ibx#443).
    #[doc(hidden)] pub fn push_wsh_answer(&self, answer: WshAnswer) {
        if let Some(out) = crate::client_core::wsh_answer(&self.wsh, answer) {
            self.wsh_answers.push(out);
        }
    }

    /// A replace whose saves all succeeded (ibx#507).
    pub fn push_fa_saved(&self, req_id: i64) {
        self.fa_saved.lock().unwrap().push(req_id);
        self.error_tap.emit(self.error_tap.copy(&()).map(|_| Event::ReplaceFaEnd { req_id }));
        self.state.note();
    }

    /// The replaces to answer with `replace_fa_end` (ibx#507).
    pub fn drain_fa_saved(&self) -> Vec<i64> {
        let mut saved = self.fa_saved.lock().unwrap();
        if saved.is_empty() { Vec::new() } else { saved.drain(..).collect() }
    }

    /// True when the logon says this is an FA session (its FA flag, ibx#481).
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

    /// A BBO exchange code known from an entry that is no instrument's (the
    /// option model's underlying, ibx#664): true when the key is new.
    #[doc(hidden)] pub fn observe_exchange_map_key(&self, code: &str, sec_type_id: u8) -> bool {
        let key: ExchangeMapKey = (code.to_string(), sec_type_id);
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
        drop(maps);
        // A smart components request waiting for it (ibx#662).
        self.state.note();
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

    /// The account's feature list from the account config, None
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
        self.md_entry_marks.store(features.entry_marks(), Ordering::Relaxed);
    }

    /// The logon features that change the entries of a market data
    /// request (ibx#648): the engine keeps this handle.
    pub(crate) fn md_entry_marks(&self) -> Arc<AtomicU8> {
        self.md_entry_marks.clone()
    }

    /// The feature list of the logon, as it came: a comma list, empty
    /// until a logon reply had one (ibx#571).
    pub fn logon_features(&self) -> String {
        self.logon_features.lock().unwrap().clone()
    }

    /// Whether the logon allows fundamentals to an API client (the
    /// server feature ALLOW_FNDT_SOMEAPI); without it the reference
    /// refuses them with 10358 (ibx#674).
    pub fn fundamentals_allowed(&self) -> bool {
        // The 10.40 form sends them (its recordings ask 258 of the farm).
        self.form_10_40() || crate::control::logon::has_feature(&self.logon_features(), "ALLOW_FNDT_SOMEAPI")
    }

    /// Keep the feature list of a logon reply (ibx#571). The same list
    /// again, as on a new logon of the same session, copies nothing.
    #[doc(hidden)] pub fn set_logon_features(&self, features: &str) {
        let mut kept = self.logon_features.lock().unwrap();
        if *kept != features {
            kept.clear();
            kept.push_str(features);
        }
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

    /// The form of the reference 10.40, for the replays of its recordings
    /// (ibx#660, ibx#637).
    #[doc(hidden)] pub fn set_form_10_40(&self, on: bool) {
        self.form_10_40.store(on, Ordering::Relaxed);
    }

    pub fn form_10_40(&self) -> bool {
        self.form_10_40.load(Ordering::Relaxed)
    }

    /// The smart combo conId of a currency, from the logon's smart combo field (ibx#470);
    /// None when the logon has none for it.
    pub fn smart_combo_con_id(&self, currency: &str) -> Option<i64> {
        self.smart_combo_con_ids.lock().unwrap().get(currency).copied()
    }

    /// Keep the logon's smart combo field, `CUR:conId,...` (ibx#470).
    /// The logon's algo providers (ibx#505).
    #[doc(hidden)] pub fn set_algo_providers(&self, raw: &str) {
        *self.algo_providers.lock().unwrap() = algo_provider_names(raw);
    }

    /// Whether an order exchange names an algo provider: the reference
    /// routes such an order through SMART (ibx#505).
    pub fn is_algo_provider(&self, exchange: &str) -> bool {
        !exchange.is_empty() && self.algo_providers.lock().unwrap().iter().any(|p| p.eq_ignore_ascii_case(exchange))
    }

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

/// One row of a ledger frame (a ledger list message, a `LedgerList` row) as
/// the reference keeps it in its ledger record (ibx#486): the account of the
/// frame, the row currency, the currency field, and each numeric field with
/// its value. A value that is not a number (`nan`) is not set.
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
    /// The state the client reads changed (ibx#662).
    pub(crate) state: Arc<StateMark>,
    account: Mutex<AccountState>,
    /// Account summary rows and ends, in arrival order (ibx#479).
    account_summary_events: Mutex<Vec<AccountSummaryEvent>>,
    error_tap: ErrorTap,
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
    /// The size each contract's last portfolio row gave, and a running
    /// number bumped when a row gives another size than the contract's row
    /// before it (ibx#723): the reference computes the account's P&L at
    /// once then.
    row_sizes: Mutex<HashMap<i64, Qty>>,
    row_size_moves: AtomicU64,
    /// The first P&L computation waits as the reference's (ibx#723); off
    /// for a replay, whose client has no clock of the recording.
    pnl_first_wait_off: AtomicBool,
    /// The wait before the first P&L computation of a replayed recording,
    /// in microseconds; 0 for none set.
    pnl_first_wait_us: std::sync::atomic::AtomicU64,
    /// The positions whose quantity the server's position feed moved, in
    /// order, with a running number (ibx#707): a reqPositions row each. A
    /// fill of this session moves its position itself, so the feed's
    /// report of it moves nothing.
    feed_moves: Mutex<Vec<(u64, i64)>>,
    positions: [AtomicU64; MAX_INSTRUMENTS],
    /// Midnight seeds from the P&L seed answer for client-side daily P&L computation.
    midnight_seeds: Mutex<HashMap<i64, MidnightSeed>>,
    /// Contracts only the P&L seeds carry that a positions request listed
    /// (ibx#737): from then on they are rows of the account's P&L.
    seeds_listed: Mutex<std::collections::HashSet<i64>>,
    /// Realized P&L of this session's fills since the last seed, by conId,
    /// from the commission frames (ibx#478). A new seed includes them.
    realized_since_seed: Mutex<HashMap<i64, f64>>,
    /// Signed cash of this session's fills since the last seed, by conId
    /// (sell positive, buy negative, like the seed's money traded).
    money_since_seed: Mutex<HashMap<i64, f64>>,
    /// The price at midnight of each position, sent by the server after
    /// the seeds (ibx#511): the daily P&L of a position held at midnight.
    midnight_prices: Mutex<HashMap<i64, f64>>,
    /// The P&L price of each position from the session's own P&L quotes
    /// (ibx#511); `f64::MAX` while the server says it is not valid.
    pnl_prices: Mutex<HashMap<i64, f64>>,
}

impl PortfolioState {
    fn new(state: &Arc<StateMark>) -> Self {
        Self {
            state: state.clone(),
            account: Mutex::new(AccountState::default()),
            account_rows: Mutex::new(AccountRows::default()),
            account_summary_events: Mutex::new(Vec::new()),
            error_tap: ErrorTap::default(),
            account_data_received: AtomicBool::new(false),
            account_download_complete: AtomicBool::new(false),
            position_infos: Mutex::new(HashMap::new()),
            position_generation: AtomicU64::new(0),
            row_sizes: Mutex::new(HashMap::new()),
            row_size_moves: AtomicU64::new(0),
            pnl_first_wait_off: AtomicBool::new(false),
            pnl_first_wait_us: std::sync::atomic::AtomicU64::new(0),
            feed_moves: Mutex::new(Vec::new()),
            positions: std::array::from_fn(|_| AtomicU64::new(0)),
            midnight_seeds: Mutex::new(HashMap::new()),
            seeds_listed: Mutex::new(std::collections::HashSet::new()),
            realized_since_seed: Mutex::new(HashMap::new()),
            money_since_seed: Mutex::new(HashMap::new()),
            midnight_prices: Mutex::new(HashMap::new()),
            pnl_prices: Mutex::new(HashMap::new()),
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
        let copy = self.error_tap.copy(&event);
        self.account_summary_events.lock().unwrap().push(event);
        self.error_tap.emit(copy.map(Event::AccountSummary));
        self.state.note();
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
        self.error_tap.notify(|| Event::AccountUpdate);
        self.state.note();
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
    /// A portfolio row's size (ibx#723): counted as a move when the
    /// contract's row before it gave another size.
    #[doc(hidden)] pub fn note_row_size(&self, con_id: i64, size: Qty) {
        if self.row_sizes.lock().unwrap().insert(con_id, size).is_some_and(|prev| prev != size) {
            self.row_size_moves.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// No wait before the first P&L computation (a replay, ibx#723).
    #[doc(hidden)] pub fn set_pnl_first_wait_off(&self) {
        self.pnl_first_wait_off.store(true, Ordering::Release);
    }

    pub fn pnl_first_wait_off(&self) -> bool {
        self.pnl_first_wait_off.load(Ordering::Acquire)
    }

    /// The wait before the first P&L computation, as the recording being
    /// replayed has it: the replay then computes at the reference's times
    /// (ibx#757).
    #[doc(hidden)] pub fn set_pnl_first_wait_of_recording(&self, wait: std::time::Duration) {
        self.pnl_first_wait_us.store(wait.as_micros() as u64, Ordering::Release);
    }

    pub fn pnl_first_wait_of_recording(&self) -> Option<std::time::Duration> {
        match self.pnl_first_wait_us.load(Ordering::Acquire) {
            0 => None,
            us => Some(std::time::Duration::from_micros(us)),
        }
    }

    /// The running number of the portfolio rows that moved a size.
    pub fn row_size_moves(&self) -> u64 {
        self.row_size_moves.load(Ordering::Acquire)
    }

    pub fn position_generation(&self) -> u64 {
        self.position_generation.load(Ordering::Acquire)
    }

    /// Move a position by a fill of this session (`delta` fixed-point,
    /// signed). The reference's position store moves with the execution, so
    /// the position row and the P&L see the fill at once. Its average cost
    /// (the reference 10.51, paper, 15/10/2026, regular hours, ibx#707): the
    /// fill price for a position the fill opens or turns over (captured
    /// 25/09/2026 too: avgCost 336.25, then 336.260003); the weighted
    /// average when it adds to the position (MSFT -9 at 397.54161, sold 1 at
    /// 539.59: -10 at 411.746449, a double, ibx#708); unchanged when it reduces it; 0 when it
    /// closes it. The position after the fill.
    #[doc(hidden)] pub fn apply_fill_to_position(&self, con_id: i64, delta: Qty, price: f64) -> Option<PositionInfo> {
        if delta == 0 {
            return None;
        }
        let mut map = self.position_infos.lock().unwrap();
        let entry = map.entry(con_id).or_insert_with(|| PositionInfo { con_id, ..Default::default() });
        let old = entry.position_fixed;
        let new = old + delta;
        let qty = |q: Qty| q.unsigned_abs() as f64 / QTY_SCALE as f64;
        entry.avg_cost = if new == 0 {
            0.0
        } else if old == 0 || (old > 0) != (new > 0) {
            price
        } else if (old > 0) == (delta > 0) {
            // As a step from the cost, in this order of operations: the
            // reference's double (SPY 79 at 775.5527304, bought 1 at 769.67:
            // 775.47919627; MSFT -9 at 397.54161, sold 1 at 539.59:
            // 411.746449), which the weighted sum misses in the last digit.
            entry.avg_cost + (price - entry.avg_cost) * qty(delta) / qty(new)
        } else {
            entry.avg_cost
        };
        entry.position_fixed = new;
        self.position_generation.fetch_add(1, Ordering::AcqRel);
        Some(entry.clone())
    }

    /// The commission of a fill that opened or added to a position goes
    /// into its average cost, per share and signed (ibx#707; the reference
    /// 10.51, 15/10/2026: AAPL 1 at 343.39, commission 1.000003, then
    /// 344.390003; MSFT -10 at 411.746449, commission 1.011119, then
    /// 411.6453371). A fill that realized P&L leaves it.
    #[doc(hidden)] pub fn apply_commission_to_position(&self, con_id: i64, commission: f64) {
        let mut map = self.position_infos.lock().unwrap();
        let Some(entry) = map.get_mut(&con_id).filter(|e| e.position_fixed != 0) else { return };
        let per_share = commission / (entry.position_fixed as f64 / QTY_SCALE as f64);
        entry.avg_cost += per_share;
        self.position_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The running number of the last feed move (0: none yet).
    pub fn feed_moves_last(&self) -> u64 {
        self.feed_moves.lock().unwrap().last().map_or(0, |m| m.0)
    }

    /// The feed moves since `after` (running numbers), and the last number.
    pub fn feed_moves_after(&self, after: u64) -> (Vec<i64>, u64) {
        let moves = self.feed_moves.lock().unwrap();
        let last = moves.last().map_or(after, |m| m.0.max(after));
        (moves.iter().filter(|m| m.0 > after).map(|m| m.1).collect(), last)
    }

    #[doc(hidden)] pub fn set_position_info(&self, info: PositionInfo) {
        let mut map = self.position_infos.lock().unwrap();
        let changed = map.get(&info.con_id)
            .is_none_or(|e| e.position_fixed != info.position_fixed || e.avg_cost != info.avg_cost);
        if changed {
            self.position_generation.fetch_add(1, Ordering::AcqRel);
        }
        if map.get(&info.con_id).map_or(info.position_fixed != 0, |e| e.position_fixed != info.position_fixed) {
            let mut moves = self.feed_moves.lock().unwrap();
            let next = moves.last().map_or(1, |m| m.0 + 1);
            moves.push((next, info.con_id));
            // Kept short: a subscription reads them at its next pass.
            if moves.len() > 256 {
                moves.drain(..128);
            }
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
        self.state.note();
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
        self.state.note();
    }

    #[doc(hidden)] pub fn set_position_fixed(&self, id: InstrumentId, pos: Qty) {
        self.positions[id as usize].store(pos as u64, Ordering::Relaxed);
    }

    /// Store midnight seeds from the P&L seed response.
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

    /// A positions request listed these contracts that only the P&L seeds
    /// carry (ibx#737).
    pub fn mark_seeds_listed(&self, con_ids: &[i64]) {
        self.seeds_listed.lock().unwrap().extend(con_ids.iter().copied());
    }

    /// The contracts of [`Self::mark_seeds_listed`].
    pub fn seeds_listed(&self) -> std::collections::HashSet<i64> {
        self.seeds_listed.lock().unwrap().clone()
    }

    /// The prices at midnight of a server message (ibx#511); each message
    /// lists them all.
    #[doc(hidden)] pub fn set_midnight_prices(&self, prices: Vec<(i64, f64)>) {
        let mut map = self.midnight_prices.lock().unwrap();
        map.clear();
        map.extend(prices);
    }

    pub fn midnight_prices(&self) -> HashMap<i64, f64> {
        self.midnight_prices.lock().unwrap().clone()
    }

    /// A P&L price of the session's P&L quotes (ibx#511).
    #[doc(hidden)] pub fn set_pnl_price(&self, con_id: i64, price: f64) {
        self.pnl_prices.lock().unwrap().insert(con_id, price);
        self.error_tap.notify(|| Event::PnlPrice { con_id });
    }

    pub fn pnl_prices(&self) -> HashMap<i64, f64> {
        self.pnl_prices.lock().unwrap().clone()
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
    pub(crate) connection_notices: SeqQueue<(i64, String)>,
    error_tap: ErrorTap,
    /// Notifier for waking consumers (e.g. Python event loop) when data arrives.
    notifier: Arc<Notifier>,
    /// Commands sent to the engine and handled by it (ibx#529).
    clock: Arc<CommandClock>,
    /// The count of the answers written, which orders them for the
    /// clients (ibx#532).
    answer_clock: Arc<AtomicU64>,
    /// The answers read from the session's state (ibx#662).
    state_mark: Arc<StateMark>,
    /// The time of the client's own clocks in a replay: the recording's,
    /// set with the engine's pace clock (ibx#515).
    #[cfg(any(test, feature = "test-support"))]
    test_clock: Mutex<Option<std::time::Instant>>,
}

impl SharedState {
    pub fn new() -> Self {
        let stamp = answer_clock();
        let state_mark = Arc::new(StateMark::new(&stamp));
        let shared = Self {
            market: MarketDataState::new(&stamp, &state_mark),
            orders: OrderState::new(&stamp),
            reference: ReferenceState::new(&stamp, &state_mark),
            portfolio: PortfolioState::new(&state_mark),
            ccp_rtt_ns: AtomicU64::new(0),
            connection_lost: AtomicBool::new(false),
            connection_notices: SeqQueue::new(&stamp),
            error_tap: ErrorTap::default(),
            notifier: Arc::new(Notifier::default()),
            clock: Arc::new(CommandClock::default()),
            answer_clock: stamp,
            state_mark,
            #[cfg(any(test, feature = "test-support"))]
            test_clock: Mutex::new(None),
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

    /// The time of the client's own clocks (the P&L pacer): now, or in a
    /// replay the recording's time (ibx#515).
    #[inline]
    pub fn client_now(&self) -> std::time::Instant {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(t) = *self.test_clock.lock().unwrap() {
            return t;
        }
        std::time::Instant::now()
    }

    /// Set the time of the client's own clocks, for a replay.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn set_test_clock(&self, now: Option<std::time::Instant>) {
        *self.test_clock.lock().unwrap() = now;
    }

    /// The next count of the answer clock (ibx#532): the place of an
    /// answer a client makes itself among the answers written.
    #[doc(hidden)]
    pub fn answer_stamp(&self) -> u64 {
        self.answer_clock.fetch_add(1, Ordering::Relaxed)
    }

    /// The answers read from the session's state: where they stand among
    /// the answers written (ibx#662).
    #[doc(hidden)]
    pub fn state_mark(&self) -> &StateMark {
        &self.state_mark
    }

    /// The count the next answer will take: every answer written before
    /// has a smaller one (ibx#532).
    #[doc(hidden)]
    pub fn answers_written(&self) -> u64 {
        self.answer_clock.load(Ordering::Relaxed)
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
        self.portfolio.error_tap.attach(tx);
        self.error_tap.attach(tx);
    }

    /// An error the client answers itself, with no queue behind it (a
    /// refused market data request, a lookup answered from kept data):
    /// given to the event channel when one is attached (ibx#498).
    #[doc(hidden)]
    pub fn emit_error(&self, req_id: i64, code: i64, message: &str) {
        self.error_tap.emit_error(req_id, code, message);
    }

    pub fn push_connection_notice(&self, code: i64, message: String) {
        if self.error_tap.defer(ErrorQueue::Connection, -1, code, &message) { return; }
        let event = self.error_tap.event(-1, code, &message);
        self.connection_notices.push((code, message));
        self.error_tap.send(event);
        self.notify();
    }

    /// Link status messages since the last call: (code, message), in order.
    pub fn drain_connection_notices(&self) -> Vec<(i64, String)> {
        self.connection_notices.drain()
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

    // ibx#568: the streaming market data and the state read data give
    // notify-only events once a channel is attached, and none without one.
    #[test]
    fn notify_only_events_of_queued_and_state_data() {
        let shared = SharedState::new();
        shared.market.push_generic_ticks(GenericTicks { at: 0, instrument: 3, code: 233, ticks: vec![] });
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);
        shared.market.push_depth_update(DepthUpdate { req_id: 7, position: 0, market_maker: String::new(), operation: 0, side: 1,
            price: 1.0, size: 1.0, is_smart_depth: false, l2: false });
        shared.market.push_generic_ticks(GenericTicks { at: 0, instrument: 3, code: 233, ticks: vec![] });
        shared.market.push_tick_req_params(TickReqParams { instrument: 3, min_tick: 0.01, bbo_exchange: String::new(), snapshot_permissions: 3 });
        shared.portfolio.update_account_rows(|_| {});
        shared.portfolio.set_pnl_price(265598, 250.0);
        let kinds: Vec<String> = rx.try_iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(kinds, ["DepthUpdate { req_id: 7 }", "GenericTicks(3)", "MarketDataParams(3)", "AccountUpdate",
            "PnlPrice { con_id: 265598 }"]);
    }

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

    /// The answers written to the queues go to an attached event channel
    /// too, in the order written and with their request id; the queues
    /// keep them for the wrapper, and nothing is given before a channel is
    /// attached (ibx#498).
    #[test]
    fn answers_go_to_the_attached_event_channel() {
        use crate::control::optparams::OptionChain;
        let chain = || OptionChain {
            exchange: "SMART".into(), underlying_con_id: 265598, trading_class: "AAPL".into(),
            multiplier: "100".into(), expirations: vec!["20261016".into()], strikes: vec![250.0],
        };
        let rule = |rule_id| MarketRule { rule_id, ..Default::default() };
        let bulletin = |msg_id| NewsBulletin { msg_id, msg_type: 1, message: "m".into(), exchange: "X".into() };
        let bar = RealTimeBar { timestamp: 1, open: 1.0, high: 2.0, low: 0.5, close: 1.5, volume: 10.0, wap: 1.2, count: 3 };
        let summary = || AccountSummaryEvent { sr_id: "1".into(), rows: vec![], ledger: false, end: true, ledgers: vec![] };

        let shared = SharedState::new();
        shared.reference.push_option_chains(1, vec![chain()]);
        shared.reference.push_market_rules(vec![rule(26)]);
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);

        shared.reference.push_option_chains(2, vec![chain()]);
        // Rule 26 was known before the channel: only 239 is new.
        shared.reference.push_market_rules(vec![rule(26), rule(239)]);
        shared.reference.push_market_rules(vec![rule(239)]);
        shared.reference.push_histogram_data(3, vec![HistogramEntry { price: 250.0, count: 7 }]);
        shared.reference.push_scanner_params("<xml/>".into());
        shared.reference.push_fundamental_data(4, "<report/>".into());
        shared.reference.push_fa_saved(8);
        shared.reference.push_news_article(5, 0, "text".into());
        shared.reference.push_historical_news(6, vec![], true);
        shared.market.push_real_time_bar(7, bar);
        shared.market.push_news_bulletin_on(bulletin(1), jiff::civil::date(2026, 10, 9));
        shared.market.push_news_bulletin_on(bulletin(1), jiff::civil::date(2026, 10, 9));
        shared.orders.push_commission_report(api::CommissionAndFeesReport { exec_id: "e1".into(), ..Default::default() });
        shared.portfolio.push_account_summary_event(summary());

        let events: Vec<String> = rx.try_iter().map(|e| match e {
            Event::OptionChains { req_id, chains } => format!("chains {req_id} {}", chains[0].trading_class),
            Event::MarketRules(rules) => format!("rules {:?}", rules.iter().map(|r| r.rule_id).collect::<Vec<_>>()),
            Event::HistogramData { req_id, entries } => format!("histogram {req_id} {}", entries[0].count),
            Event::ScannerParameters(xml) => format!("scanner {xml}"),
            Event::FundamentalData { req_id, data } => format!("fundamental {req_id} {data}"),
            Event::ReplaceFaEnd { req_id } => format!("fa saved {req_id}"),
            Event::NewsArticle { req_id, article_type, text } => format!("article {req_id} {article_type} {text}"),
            Event::HistoricalNews { req_id, headlines, has_more } => format!("news {req_id} {} {has_more}", headlines.len()),
            Event::RealTimeBar { req_id, bar } => format!("bar {req_id} {}", bar.close),
            Event::NewsBulletin(b) => format!("bulletin {}", b.msg_id),
            Event::CommissionReport(r) => format!("commission {}", r.exec_id),
            Event::AccountSummary(s) => format!("summary {} {}", s.sr_id, s.end),
            other => panic!("not expected: {other:?}"),
        }).collect();
        assert_eq!(events, [
            // No scanner parameters: their event comes from a thread of
            // its own once rebuilt, and "<xml/>" is refused by the rebuild
            // (ibx#574; the client test reads a rebuilt one).
            "chains 2 AAPL", "rules [239]", "histogram 3 7", "fundamental 4 <report/>", "fa saved 8",
            "article 5 0 text", "news 6 0 true", "bar 7 1.5", "bulletin 1", "commission e1", "summary 1 true",
        ]);
        // The queues keep every entry for the wrapper.
        assert_eq!(shared.reference.drain_option_chains().len(), 2);
        assert_eq!(shared.reference.drain_histogram_data().len(), 1);
        assert_eq!(shared.portfolio.drain_account_summary_events().len(), 1);
    }

    /// The answers of orders, historical requests kept open, scanners and
    /// option calculations on the event channel (ibx#498).
    #[test]
    fn order_and_stream_answers_go_to_the_attached_event_channel() {
        let shared = SharedState::new();
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);
        let bar = HistoricalBar { time: "20261009-13:30:00".into(), open: 1.0, high: 2.0, low: 0.5, close: 1.5, volume: 10, wap: 1.2, count: 3 };
        let answer = crate::control::optcalc::OptionComputation {
            req_id: 4, tick_type: 53, tick_attrib: 0, implied_vol: 0.3, delta: 0.5, opt_price: 2.0, pv_dividend: 0.0,
            gamma: 0.1, vega: 0.1, theta: -0.1, und_price: 250.0,
        };

        shared.orders.push_order_info(11, RichOrderInfo {
            contract: api::Contract { symbol: "SPY".into(), ..Default::default() }, order: Default::default(),
            order_state: Default::default(), last_exec: Default::default(),
        });
        shared.orders.push_completed_order(CompletedOrder {
            order_id: 11, instrument: 2, status: OrderStatus::Cancelled, filled_qty_fixed: 0, timestamp_ns: 1,
        });
        shared.reference.push_historical_update(1, bar);
        shared.reference.push_historical_ticks(2, HistoricalTickData::Last(vec![]), "TRADES".into(), true);
        shared.reference.push_scanner_data(3, ScannerResult::default());
        shared.reference.push_option_computation(answer);

        let events: Vec<String> = rx.try_iter().map(|e| match e {
            Event::OpenOrder { order_id, info } => format!("open {order_id} {}", info.contract.symbol),
            Event::CompletedOrder(o) => format!("completed {} {:?}", o.order_id, o.status),
            Event::HistoricalUpdate { req_id, bar } => format!("update {req_id} {}", bar.close),
            Event::HistoricalTicks { req_id, data, what_to_show, done } =>
                format!("ticks {req_id} {what_to_show} {done} {}", matches!(data, HistoricalTickData::Last(_))),
            Event::ScannerData { req_id, .. } => format!("scanner {req_id}"),
            Event::OptionComputation(a) => format!("computation {} {}", a.req_id, a.opt_price),
            other => panic!("not expected: {other:?}"),
        }).collect();
        assert_eq!(events, [
            "open 11 SPY", "completed 11 Cancelled", "update 1 1.5", "ticks 2 TRADES true true", "scanner 3", "computation 4 2",
        ]);
    }

    /// A refused market data subscription goes to the channel by
    /// instrument when the engine writes it, with the code and text the
    /// client reports; the changes of a frozen contract are not errors
    /// and give nothing (ibx#575).
    #[test]
    fn a_refused_market_data_subscription_goes_to_the_event_channel() {
        let shared = SharedState::new();
        let not_subscribed = |instrument| MdReject::NotSubscribed {
            instrument, delayed_available: false, needs_api_subscription: false, description: String::new(), kept_params: None, part: None, farm_text: String::new(),
        };
        shared.market.push_md_reject(not_subscribed(1));
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);

        shared.market.push_md_reject(not_subscribed(2));
        shared.market.push_md_reject(MdReject::Delayed { instrument: 3 });
        shared.market.push_md_reject(MdReject::NoSecurityDefinition { instrument: 4 });
        shared.market.push_md_reject(MdReject::Frozen { instrument: 5, frozen: 6 });
        shared.market.push_md_reject(MdReject::Live { instrument: 6, live: 5 });

        let events: Vec<(InstrumentId, i64)> = rx.try_iter().map(|e| match e {
            Event::MarketDataError { instrument, code, message } => {
                assert!(!message.is_empty());
                (instrument, code)
            }
            other => panic!("not expected: {other:?}"),
        }).collect();
        assert_eq!(events, [(2, 354), (3, 10167), (4, 200)]);
        // The queue keeps every entry for the client.
        assert_eq!(shared.market.drain_md_rejects().len(), 6);
    }

    /// An error the client answers itself has no queue: it goes to the
    /// channel when one is attached, and nowhere before (ibx#498).
    #[test]
    fn a_client_error_goes_to_the_attached_event_channel() {
        let shared = SharedState::new();
        shared.emit_error(1, 322, "before".into());
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.attach_event_channel(&tx);
        shared.emit_error(2, 354, "not subscribed");
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(&events[..], [Event::Error { req_id: 2, code: 354, message }] if message == "not subscribed"));
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
    fn statuses_and_notices_come_out_in_the_order_written() {
        let ss = SharedState::new();
        let update = |order_id| OrderUpdate {
            order_id, instrument: 0, status: OrderStatus::Cancelled, filled_qty_fixed: 0, remaining_qty_fixed: 0,
            avg_fill_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
        };
        ss.orders.push_order_update(update(1));
        ss.orders.push_order_notice(1, 202, "a".into());
        ss.orders.push_order_update(update(2));
        ss.orders.push_order_notice(2, 202, "b".into());
        ss.orders.push_order_notice(2, 161, "c".into());
        ss.orders.push_order_update(update(3));
        let got: Vec<String> = ss.orders.drain_order_reports().into_iter().map(|r| match r {
            OrderReport::Update(u) => format!("status {}", u.order_id),
            OrderReport::Notice(id, code, _) => format!("{code} {id}"),
        }).collect();
        assert_eq!(got, ["status 1", "202 1", "status 2", "202 2", "161 2", "status 3"]);
        assert!(ss.orders.drain_order_reports().is_empty());
        // Each queue alone still drains.
        ss.orders.push_order_update(update(4));
        ss.orders.push_order_notice(4, 202, "d".into());
        assert_eq!(ss.orders.drain_order_updates().len(), 1);
        assert_eq!(ss.orders.drain_order_notices().len(), 1);
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

/// The provider names of the logon's algo provider list (ibx#505): entries
/// separated by ';', each a name then '/' and its security types
/// (`FOXRIVER/STK:...;IBALGO/...;JONES/STK:...`). The reference keeps every
/// name, whatever its security types (13/10/2026).
pub fn algo_provider_names(raw: &str) -> Vec<String> {
    raw.split(';')
        .filter_map(|entry| entry.split('/').next())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}
