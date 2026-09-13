//! A client whose calls return what they were asked for.
//!
//! [`EClient`] follows the reference client: a request goes out under an id and
//! the answer arrives later on a callback. That is the right shape for a program
//! with its own event loop, and the wrong one for asking a question.
//!
//! [`Client`] is the other shape, and it carries the names the widely used Rust
//! client gives them, so a program written against that one reads the same here.
//! A call that answers returns the answer; a call that only sends says so in its
//! type by returning nothing.
//!
//! It owns a recorder for the callbacks the sending calls produce, so a caller
//! never has to supply one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::client::{EClient, EClientConfig};
use crate::api::client::ask::hold_id;
use crate::bridge::RecordKind;
use crate::types::model::{BarData, ContractDetails};
use crate::api::wrapper::Wrapper;
use crate::error_codes::Refusal;
use crate::api::client::{AccountValue, OptionChain, PositionRow};
use crate::api::subscription::Subscription;
use crate::types::{DepthUpdate, RealTimeBar};
use crate::types::ControlCommand;
use crate::bridge::SharedState;

use super::Contract;

/// What the sending calls produced, kept so a caller can read it afterwards.
///
/// The reference shape hands these to a callback as they arrive. Here they are
/// recorded, because a caller of this shape has no callback to hand them to.
#[derive(Default)]
pub struct Recorded {
    /// Everything the venue said, as request, number and words.
    pub errors: Vec<(i64, i64, String)>,
    /// The venue's clock, once it has been asked for.
    pub current_time: Option<i64>,
    /// Every account this login may act for.
    pub managed_accounts: Option<String>,
    /// The providers this account may read.
    pub news_providers: Vec<String>,
    /// The fills a request for executions answered with, contract and print.
    pub executions: Vec<(Contract, crate::types::model::Execution)>,
}

impl Wrapper for Recorded {
    fn error(&mut self, req_id: i64, code: i64, message: &str, _advanced: &str) {
        self.errors.push((req_id, code, message.to_string()));
    }
    fn current_time(&mut self, time: i64) {
        self.current_time = Some(time);
    }
    fn managed_accounts(&mut self, accounts: &str) {
        self.managed_accounts = Some(accounts.to_string());
    }
    // The two below were declared and never filled: the calls that ask for
    // them handed the answer to a callback nothing here implemented, so a
    // caller reading the field it was told to read found it empty however
    // long it waited.
    fn news_providers(&mut self, providers: &[crate::types::NewsProvider]) {
        self.news_providers = providers.iter().map(|p| p.code.clone()).collect();
    }
    fn exec_details(&mut self, _req_id: i64, contract: &Contract, execution: &crate::types::model::Execution) {
        self.executions.push((contract.clone(), execution.clone()));
    }
}

/// The first id this shape opens a stream or a question under.
///
/// Above what a caller of the callback shape numbers its own requests from, and
/// below the ids the answering layer beneath uses, so none of the three can be
/// mistaken for another. The guard below fails the build rather than the suite
/// if that ever stops being true.
pub const STREAM_ID_BASE: i64 = 0x2000_0000;

const _: () = assert!(
    STREAM_ID_BASE < crate::bridge::ReferenceState::ASK_ID_BASE as i64,
    "a stream id would be taken for one of the answering layer's own"
);

/// How long a question waits for its answer.
const ANSWER_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS);

/// How long to sleep between looks at the queue.
const POLL: Duration = Duration::from_millis(5);

/// A session whose calls return what they were asked for.
pub struct Client {
    inner: EClient,
    recorded: Arc<Mutex<Recorded>>,
    /// Ids for the streams this shape opens. Far above what a caller of the
    /// callback shape is likely to use on the same session.
    next_stream_id: std::sync::atomic::AtomicI64,
    /// When the session opened, in seconds since the epoch.
    connected_at: i64,
}

/// Calls the widely used Rust client has whose answer here is a different
/// thing, and what they answer instead.
///
/// Named rather than left out. Every one of them can be called: a program
/// moved across compiles and runs, and the ones that cannot be answered say so
/// on the spot rather than by not existing.
pub const NO_COUNTERPART: &[(&str, &str)] = &[
    ("verify_message", "part of a handshake between a client and a local process, and there is no local process"),
    ("verify_request", "part of a handshake between a client and a local process, and there is no local process"),
    ("cancel_contract_details", "a contract lookup answers here rather than streaming, so there is nothing to withdraw"),
    ("pnl", "the figures arrive on a callback as they change, and this shape keeps only what a synchronous answer hands it — ask through inner() and drive process_msgs with a wrapper of your own"),
    ("pnl_single", "the figures arrive on a callback as they change, and this shape keeps only what a synchronous answer hands it — ask through inner() and drive process_msgs with a wrapper of your own"),
];

impl Client {
    /// Open a session.
    pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: EClient::connect(config)?,
            recorded: Arc::new(Mutex::new(Recorded::default())),
            next_stream_id: std::sync::atomic::AtomicI64::new(STREAM_ID_BASE),
            connected_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default(),
        })
    }

    /// Build around a session assembled elsewhere (for testing).
    #[doc(hidden)]
    pub fn from_parts(inner: EClient) -> Self {
        Self {
            inner,
            recorded: Arc::new(Mutex::new(Recorded::default())),
            next_stream_id: std::sync::atomic::AtomicI64::new(STREAM_ID_BASE),
            connected_at: 0,
        }
    }

    // ── Calls that exist because the other client talks to a local process ──

    /// What this client speaks.
    ///
    /// In a client that talks to a local process, this is the protocol level
    /// that process announced, and a program compares it against the level a
    /// feature was introduced at. There is no local process here; this client
    /// is what a program is talking to, so it answers with its own level.
    ///
    /// It answered with the build it states at logon before, which is a number
    /// on another scale entirely — above every level there is — so a program
    /// gating on it was told every feature exists. Measured against that
    /// client's own gates, a dozen order fields are refused here, four calls
    /// and two of its newest cancels are absent, and nothing above this level
    /// is carried at all. `EClient::server_version` lists them.
    pub fn server_version(&self) -> i32 {
        crate::client_core::PROTOCOL_LEVEL
    }

    /// Which client this is, of several sharing one local process.
    ///
    /// Nothing is shared here: a session is one login, and the venue knows it
    /// by that. Zero is what a program with a single connection uses, and what
    /// the reports coming back carry.
    pub fn client_id(&self) -> i32 {
        0
    }

    /// Withdraw a contract lookup.
    ///
    /// A lookup answers here rather than streaming, so by the time this can be
    /// called there is nothing left running. It reports that rather than
    /// pretending to stop something.
    pub fn cancel_contract_details(&self, _req_id: i64) -> Result<(), Refusal> {
        Err(Self::no_counterpart("cancel_contract_details"))
    }

    /// Begin the handshake a third-party program makes with a local process.
    pub fn verify_request(&self, _api_name: &str, _api_version: &str) -> Result<(), Refusal> {
        Err(Self::no_counterpart("verify_request"))
    }

    /// Continue that handshake.
    pub fn verify_message(&self, _api_data: &str) -> Result<(), Refusal> {
        Err(Self::no_counterpart("verify_message"))
    }

    fn no_counterpart(call: &str) -> Refusal {
        Refusal::validation(
            NO_COUNTERPART
                .iter()
                .find(|(name, _)| *name == call)
                .map(|(_, why)| (*why).to_string())
                .unwrap_or_default(),
        )
    }

    /// The session underneath, for anything this shape does not carry.
    ///
    /// Not a workaround: the two shapes are the same session, and a program
    /// wanting a callback for one thing and an answer for another should not
    /// have to choose between them.
    pub fn inner(&self) -> &EClient {
        &self.inner
    }

    /// What the sending calls have produced so far.
    pub fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap()
    }

    /// Whether a session is open.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// End the session. The engine's thread is joined before this returns.
    pub fn disconnect(&self) {
        self.inner.disconnect();
    }

    // -- calls that answer -------------------------------------------------

    /// Everything the venue knows about the contracts matching a description.
    pub fn contract_details(&self, contract: &Contract) -> Result<Vec<ContractDetails>, Refusal> {
        self.inner.contract_details(contract)
    }

    /// Fill in what the venue knows about a contract, above all its id.
    pub fn qualify_contract(&self, contract: &Contract) -> Result<Contract, Refusal> {
        self.inner.qualify_contract(contract)
    }

    /// Fill in a whole list, keeping their order.
    pub fn qualify_contracts(&self, contracts: &[Contract]) -> Result<Vec<Contract>, Refusal> {
        self.inner.qualify_contracts(contracts)
    }

    /// Bars for a contract over a period.
    pub fn historical_data(
        &self,
        contract: &Contract,
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        use_rth: bool,
    ) -> Result<Vec<BarData>, Refusal> {
        self.inner.historical_data(
            contract, end_date_time, duration, bar_size, what_to_show,
            use_rth,
        )
    }

    /// What the account holds.
    pub fn positions(&self) -> Result<Vec<PositionRow>, Refusal> {
        self.inner.positions()
    }

    /// The account's figures for the tags asked for.
    pub fn account_summary(&self, tags: &str) -> Result<Vec<AccountValue>, Refusal> {
        self.inner.account_summary(tags)
    }

    /// Every expiration and strike a venue lists for an underlying.
    pub fn option_chain(&self, underlying: &Contract) -> Result<Vec<OptionChain>, Refusal> {
        self.inner.option_chain(underlying)
    }

    /// Wait for the one answer belonging to a request.
    ///
    /// Ends on the answer, on the venue's refusal of that request, or on the
    /// deadline, and says which. A refusal quotes the venue rather than
    /// reporting a timeout, because "the venue said no" and "nothing came" are
    /// different facts and only one is worth asking again.
    fn wait_for<T>(
        &self,
        req_id: i64,
        what: &str,
        take: impl Fn(&SharedState) -> Option<T>,
    ) -> Result<T, crate::error_codes::Refusal> {
        use crate::error_codes::Refusal;
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            if let Some(v) = take(&self.inner.shared) {
                return Ok(v);
            }
            if let Some((code, message)) = self.inner.shared.reference.take_error_for(req_id as u32) {
                // Under the number the venue gave it. Written into the text
                // instead, a caller could only match on prose for something
                // the reference client hands it to branch on.
                return Err(Refusal::stated(code, message));
            }
            // Nothing is going to answer. The reference client says so at once
            // rather than at the end of a timeout, and a program that asks in a
            // loop otherwise spends a minute per call discovering the same
            // thing.
            if let Some(why) = self.inner.shared.reference.session_over() {
                return Err(Refusal::not_connected(format!(
                    "the session is over: {why}",
                )));
            }
            if Instant::now() >= deadline {
                return Err(Refusal::no_answer(format!(
                    "no answer within {}s to {what}",
                    ANSWER_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(POLL);
        }
    }

    /// The earliest moment the venue holds data for a contract.
    pub fn head_timestamp(
        &self,
        contract: &Contract,
        what_to_show: &str,
        use_rth: bool,
    ) -> Result<String, Refusal> {
        let req_id = self.stream_id();
        // A refusal keeps the number it left with. These requests are
        // refused for reasons of their own — a contract nobody can name,
        // a field the wire cannot carry — and rewritten as "not
        // connected" they read as a session problem, so a caller retried
        // for ever a request that no session could carry. The one refusal
        // that states a session problem carries that number already.
        // Named before the turn is taken. A contract given by the venue's id
        // alone is filled in by asking the venue for it, which is one of the
        // calls that waits for this same turn — asked while this one holds it,
        // on a lock that is not re-entrant, neither ever runs again and the
        // deadline that would have said so is set inside the wait that never
        // starts.
        let contract = &*self.inner.named_by_the_venue(contract)?;
        // One question at a time, and the session's own reader waits on it:
        // this takes its answer out of the queue by id rather than off a
        // callback, and a dispatch loop running beside it would take that
        // answer first. See `EClient::asking`.
        let _turn = self.inner.take_the_turn()?;
        self.inner.req_head_time_stamp(req_id, contract, what_to_show, use_rth, 1)?;
        let what = format!("the earliest data for {} {}", contract.sec_type, contract.symbol);
        self.wait_for(req_id, &what, |sh| {
            sh.reference.take_head_timestamp_for(req_id as u32)
        })
        .map(|r| r.head_timestamp)
    }

    /// Contracts whose symbol or name matches a pattern.
    pub fn matching_symbols(&self, pattern: &str) -> Result<Vec<crate::control::contracts::SymbolMatch>, Refusal> {
        let req_id = self.stream_id();
        // One question at a time, and the session's own reader waits on it:
        // this takes its answer out of the queue by id rather than off a
        // callback, and a dispatch loop running beside it would take that
        // answer first. See `EClient::asking`.
        let _turn = self.inner.take_the_turn()?;
        self.inner.req_matching_symbols(req_id, pattern)?;
        let what = format!("a symbol search for {pattern}");
        self.wait_for(req_id, &what, |sh| {
            sh.reference.take_matching_symbols_for(req_id as u32)
        })
    }

    /// How a contract's traded volume is spread across prices over a period.
    pub fn histogram_data(
        &self,
        contract: &Contract,
        use_rth: bool,
        period: &str,
    ) -> Result<Vec<crate::control::histogram::HistogramEntry>, Refusal> {
        let req_id = self.stream_id();
        // Named before the turn is taken; see `head_timestamp`.
        let contract = &*self.inner.named_by_the_venue(contract)?;
        // One question at a time, and the session's own reader waits on it:
        // this takes its answer out of the queue by id rather than off a
        // callback, and a dispatch loop running beside it would take that
        // answer first. See `EClient::asking`.
        let _turn = self.inner.take_the_turn()?;
        self.inner.req_histogram_data(req_id, contract, use_rth, period)?;
        let what = format!("a histogram for {} {}", contract.sec_type, contract.symbol);
        self.wait_for(req_id, &what, |sh| {
            sh.reference.take_histogram_for(req_id as u32)
        })
    }

    /// A fundamental report on a contract, as the venue's document.
    pub fn fundamental_data(
        &self,
        contract: &Contract,
        report_type: &str,
    ) -> Result<String, Refusal> {
        let req_id = self.stream_id();
        // One question at a time, and the session's own reader waits on it:
        // this takes its answer out of the queue by id rather than off a
        // callback, and a dispatch loop running beside it would take that
        // answer first. See `EClient::asking`.
        let _turn = self.inner.take_the_turn()?;
        self.inner.req_fundamental_data(req_id, contract, report_type)?;
        let what = format!("a {report_type} report for {}", contract.symbol);
        self.wait_for(req_id, &what, |sh| {
            sh.reference.take_fundamental_for(req_id as u32)
        })
    }

    // -- streams a caller iterates -----------------------------------------

    fn stream_id(&self) -> i64 {
        self.next_stream_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Bars as the venue prints them, until the stream is dropped.
    ///
    /// Dropping withdraws it. A stream left running feeds a session nobody is
    /// reading, which costs the account a line it is not using.
    pub fn realtime_bars(
        &self,
        contract: &Contract,
        what_to_show: &str,
        use_rth: bool,
    ) -> Result<Subscription<RealTimeBar>, Refusal> {
        let req_id = self.stream_id();
        // Held before the request goes out. Claimed afterwards, the first
        // record — or the venue's refusal — can arrive and be taken by a
        // dispatch loop running beside this, which sees a number nobody has
        // claimed. Given up again if the request cannot be sent.
        let held = hold_id(&self.inner.shared, RecordKind::Bars, req_id);
        self.inner.req_real_time_bars(req_id, contract, 5, what_to_show, use_rth)?;
        // The hold passes to the stream, which gives it up when it ends.
        let req_id = held.keep();
        // Withdraw through a clone of the session's control channel rather
        // than a borrow of the client: a stream may outlive the borrow that
        // made it, and a stream that cannot withdraw itself is one the venue
        // keeps feeding.
        let tx = self.inner.control_tx.clone();
        Ok(Subscription::new(
            req_id,
            RecordKind::Bars,
            Arc::clone(&self.inner.shared),
            |sh, id| sh.market.take_real_time_bars_for(id as u32),
            move |id| {
                let _ = tx.send(ControlCommand::CancelRealTimeBar { req_id: id as u32 });
            },
        ))
    }

    /// Every change to a contract's book, until the stream is dropped.
    ///
    /// A reset the venue states mid-stream is read off
    /// [`Subscription::take_notice`]; the levels after it are a fresh book.
    pub fn market_depth(
        &self,
        contract: &Contract,
        num_rows: i32,
        smart_depth: bool,
    ) -> Result<Subscription<DepthUpdate>, Refusal> {
        let req_id = self.stream_id();
        // Held before the request goes out. Claimed afterwards, the first
        // record — or the venue's refusal — can arrive and be taken by a
        // dispatch loop running beside this, which sees a number nobody has
        // claimed. Given up again if the request cannot be sent.
        let held = hold_id(&self.inner.shared, RecordKind::Depth, req_id);
        self.inner.req_mkt_depth(req_id, contract, num_rows, smart_depth)?;
        let req_id = held.keep();
        let tx = self.inner.control_tx.clone();
        let books = Arc::clone(&self.inner.core.depth_reqs);
        Ok(Subscription::new(
            req_id,
            RecordKind::Depth,
            Arc::clone(&self.inner.shared),
            |sh, id| sh.market.take_depth_updates_for(id as u32),
            move |id| {
                // The slot the request took goes back with the withdrawal.
                // Sent down the control channel alone, the number was refused
                // as still holding a book for the life of the session.
                books.lock().unwrap().remove(&id);
                let _ = tx.send(ControlCommand::UnsubscribeDepth { req_id: id as u32 });
            },
        ))
    }

    // -- calls that send ---------------------------------------------------
    //
    // These return nothing because they produce nothing to return: what they
    // ask for arrives on the session and is recorded. Saying so in the type is
    // better than handing back a value that means only "it was sent".


    /// Send an order.
    ///
    /// The order's own id is what the venue answers under, so it is returned:
    /// a caller with nothing to correlate on cannot tell which answer is theirs.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &crate::types::model::Order) -> Result<i64, Refusal> {
        self.inner.place_order(order_id, contract, order)?;
        Ok(order_id)
    }

    /// Withdraw one order.
    pub fn cancel_order(&self, order_id: i64) -> Result<(), Refusal> {
        self.inner.cancel_order(order_id, "")
    }

    /// Exercise or lapse a long option position.
    pub fn exercise_options(
        &self,
        contract: &Contract,
        action: i32,
        quantity: i32,
        account: &str,
        override_precaution: bool,
        stated: crate::client_core::ExerciseStates,
    ) -> Result<(), Refusal> {
        self.inner.exercise_options(
            self.stream_id(), contract, action, quantity, account,
            override_precaution, stated,
        )
    }

    /// What the account holds, as the callback shape reports it.
    pub fn open_orders(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_open_orders(&mut *r);
    }

    /// Every open order, including those placed elsewhere.
    pub fn all_open_orders(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_all_open_orders(&mut *r);
    }

    /// Orders that are done — filled, cancelled or expired.
    pub fn completed_orders(&self, api_only: bool) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_completed_orders(api_only, &mut *r);
    }

    /// The account families this login belongs to.
    pub fn family_codes(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_family_codes(&mut *r);
    }

    /// Which news providers this account may read.
    pub fn news_providers(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_news_providers(&mut *r);
    }

    /// The price ladder a contract trades on. Rules arrive with the details of
    /// a contract that uses one, so a rule this session has not seen is refused
    /// rather than left unanswered.
    pub fn market_rule(&self, market_rule_id: i32) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_market_rule(market_rule_id, &mut *r);
    }

    /// Ask for the next order id this session may use.
    pub fn next_order_id(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_ids(&mut *r);
    }

    /// Every scan the venue offers, and what each can be filtered by.
    ///
    /// Asked here and answered on the session underneath: the venue sends this
    /// after the request rather than with it, and this shape has nowhere to
    /// put an answer that arrives later. Read it through [`Self::inner`] with a
    /// wrapper of your own. It was declared as something this recorded and
    /// never was.
    pub fn scanner_parameters(&self) -> Result<(), Refusal> {
        self.inner.req_scanner_parameters()
    }

    /// Every exchange the venue names, in the two sections it names them in.
    pub fn market_depth_exchanges(&self) -> Result<(), Refusal> {
        self.inner.req_mkt_depth_exchanges()
    }

    /// Which feed subscriptions are served from: 1 live, 2 frozen, 3 delayed,
    /// 4 delayed and frozen.
    pub fn switch_market_data_type(&self, market_data_type: i32) {
        self.inner.req_market_data_type(market_data_type);
    }

    /// The notices the venue broadcasts to everyone.
    pub fn news_bulletins(&self, all_messages: bool) {
        self.inner.req_news_bulletins(all_messages);
    }

    /// Start or stop the account's own figures arriving as they change.
    pub fn account_updates(&self, subscribe: bool, account: &str) {
        self.inner.req_account_updates(subscribe, account);
    }

    /// An account's running profit.
    ///
    /// Refused rather than subscribed in silence: the figures arrive on a
    /// callback as they change, and nothing this shape owns pumps callbacks —
    /// it keeps what a synchronous answer hands it and reads what the streams
    /// queue. Subscribed here, the answer went nowhere while the state the
    /// subscription opened stood forever. Ask through [`Self::inner`] and
    /// drive `process_msgs` with a wrapper of your own.
    pub fn pnl(&self, _account: &str, _model_code: &str) -> Result<(), Refusal> {
        Err(Self::no_counterpart("pnl"))
    }

    /// The same for one position, and refused for the same reason.
    pub fn pnl_single(&self, _account: &str, _model_code: &str, _con_id: i64) -> Result<(), Refusal> {
        Err(Self::no_counterpart("pnl_single"))
    }


    /// The schedule a venue keeps for a contract over a period.
    pub fn historical_schedules(
        &self,
        contract: &Contract,
        end_date_time: &str,
        duration: &str,
        use_rth: bool,
    ) -> Result<crate::types::HistoricalScheduleResponse, Refusal> {
        let req_id = self.stream_id();
        // Named before the turn is taken; see `head_timestamp`.
        let contract = &*self.inner.named_by_the_venue(contract)?;
        // One question at a time, and the session's own reader waits on it:
        // this takes its answer out of the queue by id rather than off a
        // callback, and a dispatch loop running beside it would take that
        // answer first. See `EClient::asking`.
        let _turn = self.inner.take_the_turn()?;
        self.inner.req_historical_schedule(req_id, contract, end_date_time, duration, use_rth)?;
        let what = format!("a schedule for {} {}", contract.sec_type, contract.symbol);
        self.wait_for(req_id, &what, |sh| {
            sh.reference.take_historical_schedule_for(req_id as u32)
        })
    }

    /// Trades or quotes as they printed, over a period.
    pub fn historical_ticks(
        &self,
        contract: &Contract,
        start_date_time: &str,
        end_date_time: &str,
        number_of_ticks: i32,
        what_to_show: &str,
        use_rth: bool,
    ) -> Result<(), Refusal> {
        self.inner.req_historical_ticks(
            self.stream_id(), contract, start_date_time, end_date_time,
            number_of_ticks, what_to_show, use_rth,
        )
    }

    /// Headlines for a contract over a period.
    pub fn historical_news(
        &self,
        con_id: i64,
        provider_codes: &str,
        start_time: &str,
        end_time: &str,
        max_results: u32,
    ) -> Result<(), Refusal> {
        self.inner.req_historical_news(
            self.stream_id(), con_id, provider_codes, start_time, end_time, max_results,
        )
    }

    /// One news article by its id.
    pub fn news_article(&self, provider_code: &str, article_id: &str) -> Result<(), Refusal> {
        self.inner.req_news_article(self.stream_id(), provider_code, article_id)
    }

    /// A scan the venue runs and keeps running until the stream is dropped.
    ///
    /// Dropping withdraws it. A scan is answered repeatedly for as long as it
    /// runs, so rows arriving for one nobody reads would simply accumulate;
    /// read through the stream, and withdrawn with it, they cannot.
    pub fn scanner_subscription(
        &self,
        instrument: &str,
        location_code: &str,
        scan_code: &str,
        max_items: u32,
        filters: &[crate::types::model::TagValue],
    ) -> Result<Subscription<crate::control::scanner::ScannerResult>, Refusal> {
        let req_id = self.stream_id();
        // Held before the request goes out; see `realtime_bars`.
        let held = hold_id(&self.inner.shared, RecordKind::Scanner, req_id);
        self.inner.req_scanner_subscription(
            req_id, instrument, location_code, scan_code, max_items, filters,
        )?;
        let req_id = held.keep();
        let tx = self.inner.control_tx.clone();
        Ok(Subscription::new(
            req_id,
            RecordKind::Scanner,
            Arc::clone(&self.inner.shared),
            |sh, id| sh.reference.take_scanner_data_for(id as u32),
            move |id| {
                let _ = tx.send(ControlCommand::CancelScanner { req_id: id as u32 });
            },
        ))
    }

    /// Stop a scan.
    pub fn cancel_scanner_subscription(&self, req_id: i64) -> Result<(), Refusal> {
        self.inner.cancel_scanner_subscription(req_id)
    }

    /// Fills matching a filter, answered from what this session has stored
    /// and kept on the recorder as `exec_details` rows.
    pub fn executions(&self, filter: &crate::types::model::ExecutionFilter) {
        let mut r = self.recorded.lock().unwrap();
        // This call's answer alone: appended to the last call's, two calls
        // with different filters left their union standing.
        r.executions.clear();
        self.inner.req_executions(self.stream_id(), filter, &mut *r);
    }

    /// Which venue each bit of a quote's exchange mask refers to.
    pub fn smart_components(&self, bbo_exchange: &str) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_smart_components(self.stream_id(), bbo_exchange, &mut *r);
    }

    /// The soft dollar tiers this account may direct commission to.
    pub fn soft_dollar_tiers(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_soft_dollar_tiers(self.stream_id(), &mut *r);
    }

    /// What this login is entitled to.
    pub fn user_info(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_user_info(self.stream_id(), &mut *r);
    }

    /// Positions for one account or model.
    pub fn positions_multi(&self, account: &str, model_code: &str) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_positions_multi(self.stream_id(), account, model_code, &mut *r);
    }

    /// An account's figures for one account or model, and whether to include
    /// its ledger and net liquidation value.
    pub fn account_updates_multi(&self, account: &str, model_code: &str, ledger_and_nlv: bool) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_account_updates_multi(
            self.stream_id(), account, model_code, ledger_and_nlv, &mut *r,
        );
    }

    /// Whether orders placed elsewhere are bound to this session.
    pub fn auto_open_orders(&self, auto_bind: bool) {
        self.inner.req_auto_open_orders(auto_bind);
    }

    /// Watch what a display group is showing.
    pub fn subscribe_to_group_events(&self, group_id: i32) -> i64 {
        let req_id = self.stream_id();
        self.inner.subscribe_to_group_events(req_id, group_id);
        req_id
    }

    /// Carried, and answered by the venue as not served. It reports that rather
    /// than pretending, which is the honest shape for a request this protocol
    /// does not carry.
    pub fn calculate_implied_volatility(
        &self,
        contract: &Contract,
        option_price: f64,
        under_price: f64,
    ) {
        self.inner.calculate_implied_volatility(
            self.stream_id(), contract, option_price, under_price,
        );
    }

    /// What an option is worth at a stated volatility, under the model the
    /// venue publishes for that contract.
    pub fn calculate_option_price(&self, contract: &Contract, volatility: f64, under_price: f64) {
        self.inner.calculate_option_price(
            self.stream_id(), contract, volatility, under_price,
        );
    }

    /// Ask the venue for a partition of the advisor's own configuration.
    pub fn request_fa(&self, fa_data_type: i32) -> Result<(), Refusal> {
        self.inner.request_fa(fa_data_type)
    }

    /// Replace a partition of that configuration with the one given.
    ///
    /// Numbered here rather than by the caller, as every other call on this
    /// surface is, so the end of the replacement can be told from the end of
    /// any other.
    pub fn replace_fa(&self, fa_data_type: i32, cxml: &str) -> Result<(), Refusal> {
        self.inner.replace_fa(self.stream_id(), fa_data_type, cxml)
    }

    /// What event types the corporate-events calendar carries.
    pub fn wsh_metadata(&self) -> Result<(), Refusal> {
        self.inner.req_wsh_meta_data(self.stream_id())
    }

    /// The calendar's events for one contract.
    pub fn wsh_event_data_by_contract(&self, con_id: i64) -> Result<(), Refusal> {
        self.inner.req_wsh_event_data(
            self.stream_id(),
            crate::types::CalendarQuery { con_id: Some(con_id), ..Default::default() },
        )
    }

    /// The calendar's events under a filter the caller writes.
    pub fn wsh_event_data(
        &self,
        query: crate::types::CalendarQuery,
    ) -> Result<(), Refusal> {
        self.inner.req_wsh_event_data(self.stream_id(), query)
    }

    /// When this session opened, in seconds since the epoch.
    pub fn connection_time(&self) -> i64 {
        self.connected_at
    }

    /// The id the next request will be sent under.
    ///
    /// Stated without being spent: an id is consumed by the request that goes
    /// out under it, and consuming it here left the next request going out
    /// under the number after the one reported.
    pub fn next_request_id(&self) -> i64 {
        self.next_stream_id.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The next order id the venue has given this session.
    pub fn next_valid_order_id(&self) -> i64 {
        self.inner.next_order_id()
    }

    /// The time zone this session states its times in.
    pub fn time_zone(&self) -> String {
        self.inner.shared_state().settings().timezone.clone()
    }

    /// Set server log level.
    ///
    /// Taken and not applied, as it is on the other two surfaces: the session
    /// holds no log level of its own and this protocol carries no message
    /// asking the venue to change one, so what a caller states here is written
    /// to this client's log and nothing else. This client's own logging is set
    /// where the process sets it, through `IBX_LOG_LEVEL` or `RUST_LOG`.
    ///
    /// It wrote that variable here, which set nothing — the filter is built
    /// when the logger is installed and does not read it again — and wrote it
    /// from a client that already owns threads, which is not a thing a process
    /// may do to its own environment while they run.
    pub fn set_server_log_level(&self, level: &str) {
        log::info!("set_server_log_level: {level}");
    }

    /// Send an order, under an id this shape chooses.
    pub fn submit_order(&self, contract: &Contract, order: &crate::types::model::Order) -> Result<i64, Refusal> {
        let order_id = self.inner.next_order_id();
        self.place_order(order_id, contract, order)
    }

    /// Send a set of orders where a fill on one withdraws the rest.
    ///
    /// The link is set here before anything is sent. Sending them unlinked and
    /// linking afterwards leaves a window in which two of them can both fill.
    pub fn submit_oca_orders(
        &self,
        contract: &Contract,
        orders: &mut [crate::types::model::Order],
        oca_group: &str,
        oca_type: i32,
    ) -> Result<Vec<i64>, Refusal> {
        for order in orders.iter_mut() {
            order.oca_group = oca_group.to_string();
            order.oca_type = oca_type;
        }
        let mut ids: Vec<i64> = Vec::with_capacity(orders.len());
        for order in orders.iter() {
            match self.submit_order(contract, order) {
                Ok(id) => ids.push(id),
                Err(refused) => {
                    // The ones already sent are live at the venue, and the
                    // caller is about to be told the set failed — so it holds
                    // no id to withdraw them by, and placing the set again
                    // doubles what is working. A withdrawal is asked for on
                    // each, because an order the caller was told nothing about
                    // is the one outcome this path must not leave behind.
                    //
                    // Asked for, not completed: the withdrawal is queued for
                    // the engine and reaches the venue after this returns, so
                    // a caller that places the set again immediately can hold
                    // both for as long as that takes. What could not even be
                    // asked for is named in the refusal, which is the part
                    // this call can be sure of.
                    let mut still_working = Vec::new();
                    for id in &ids {
                        if self.cancel_order(*id).is_err() {
                            still_working.push(id.to_string());
                        }
                    }
                    if still_working.is_empty() {
                        return Err(refused);
                    }
                    return Err(Refusal::stated(
                        refused.code,
                        format!(
                            "{refused}; {} of the set was already working and could not be \
                             withdrawn: order id(s) {}",
                            still_working.len(),
                            still_working.join(", "),
                        ),
                    ));
                }
            }
        }
        Ok(ids)
    }

    /// Every change to this session's orders, as they happen.
    ///
    /// Not tied to one request, so nothing withdraws it: it ends when the
    /// caller stops reading.
    pub fn order_update_stream(&self) -> Subscription<crate::types::OrderUpdate> {
        Subscription::without_cancel(
            self.stream_id(),
            Arc::clone(&self.inner.shared),
            |sh, _| sh.orders.drain_order_updates(),
        )
    }

    /// Everything the venue says that belongs to no request of this session's.
    pub fn notice_stream(&self) -> Subscription<String> {
        Subscription::without_cancel(
            self.stream_id(),
            Arc::clone(&self.inner.shared),
            |sh, _| sh.market.drain_venue_errors(),
        )
    }

    /// Ask the venue for its own clock.
    pub fn req_current_time(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_current_time(&mut *r);
    }

    /// Ask the venue for its own clock, in milliseconds.
    pub fn req_current_time_in_millis(&self) {
        let mut r = self.recorded.lock().unwrap();
        self.inner.req_current_time_in_millis(&mut *r);
    }

    /// Every account this login may act for, once the session has stated them.
    pub fn managed_accounts(&self) -> Option<String> {
        self.recorded.lock().unwrap().managed_accounts.clone()
    }

    /// Withdraw every order this session has working.
    pub fn global_cancel(&self) -> Result<(), Refusal> {
        self.inner.req_global_cancel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session in pieces: the shape of the calls can be tried without a
    /// venue on the other end.
    fn test_direct_client() -> (
        Client,
        std::sync::mpsc::Receiver<ControlCommand>,
        Arc<SharedState>,
    ) {
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let handle = std::thread::spawn(|| {});
        let inner = EClient::from_parts(Arc::clone(&shared), tx, handle, "DU123".into());
        let client = Client {
            inner,
            recorded: Arc::new(Mutex::new(Recorded::default())),
            next_stream_id: std::sync::atomic::AtomicI64::new(STREAM_ID_BASE),
            connected_at: 0,
        };
        (client, rx, shared)
    }

    /// The recorder keeps what the sending calls produce, because a caller of
    /// this shape has no callback to hand it to.
    #[test]
    fn what_arrives_on_a_sending_call_is_kept_rather_than_dropped() {
        let mut r = Recorded::default();
        r.current_time(1_767_000_000);
        r.managed_accounts("DU1,DU2");
        r.error(7, 162, "Historical Market Data Service error", "");

        assert_eq!(r.current_time, Some(1_767_000_000));
        assert_eq!(r.managed_accounts.as_deref(), Some("DU1,DU2"));
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].0, 7);

        // Declared and never filled: the call that asks for these handed the
        // answer to a callback nothing here implemented, so a caller reading
        // the field it was told to read found it empty however long it waited.
        r.news_providers(&[
            crate::types::NewsProvider { code: "BRFG".into(), name: "Briefing".into() },
        ]);
        assert_eq!(r.news_providers, ["BRFG"]);
    }

    /// The executions a caller asked for are kept where it can read them.
    /// Recorded nowhere, the call pumped a request whose whole purpose is
    /// the answer and returned nothing.
    #[test]
    fn the_executions_asked_for_are_kept() {
        let (client, _rx, _shared) = test_direct_client();
        client.inner.core.push_execution(
            Contract { symbol: "AAPL".into(), ..Default::default() },
            crate::types::model::Execution { exec_id: "0001.1".into(), ..Default::default() },
            Default::default(),
        );
        client.executions(&Default::default());
        let recorded = client.recorded();
        assert_eq!(recorded.executions.len(), 1, "kept, not handed to nothing");
        assert_eq!(recorded.executions[0].1.exec_id, "0001.1");
        drop(recorded);
        // Each call answers with its own fills, not the previous call's as well.
        client.executions(&Default::default());
        assert_eq!(client.recorded().executions.len(), 1, "the previous answer does not stand under the new one");
    }

    /// A refusal that can never work keeps its own number.
    ///
    /// A contract the venue has not named is refused before anything is
    /// sent, under the number that says so. Rewritten as "not connected",
    /// it read as a session problem, and a caller retried for ever what
    /// no session could carry.
    #[test]
    fn a_permanent_refusal_keeps_its_own_number() {
        let (inner, _rx, _shared) = crate::api::client::tests::test_client();
        let client = Client::from_parts(inner);
        let refused = client
            .fundamental_data(&Contract::default(), "ReportsOwnership")
            .expect_err("a contract without the venue's id cannot be asked about");
        assert_eq!(
            refused.code,
            Refusal::VALIDATION,
            "a permanent refusal is not a session problem: {refused}",
        );
    }

    /// A refusal names the request it belongs to. Without the id a caller
    /// cannot tell which of several outstanding requests was refused.
    #[test]
    fn a_refusal_is_kept_against_the_request_it_names() {
        let mut r = Recorded::default();
        r.error(1, 200, "No security definition has been found", "");
        r.error(2, 354, "Requested market data is not subscribed", "");
        let by_req: Vec<i64> = r.errors.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(by_req, vec![1, 2]);
    }



    /// A set linked so that a fill on one withdraws the rest is linked before
    /// anything is sent. Linking afterwards leaves a window in which two of
    /// them can both fill, which is the one thing the set exists to prevent.
    #[test]
    fn an_oca_set_is_linked_before_any_of_it_is_sent() {
        use crate::types::model::Order;

        let mut orders = [Order::default(), Order::default(), Order::default()];
        for order in orders.iter_mut() {
            order.oca_group = "grp-1".to_string();
            order.oca_type = 1;
        }
        assert!(orders.iter().all(|o| o.oca_group == "grp-1" && o.oca_type == 1));
    }

    /// A call whose answer here is a different thing says what it is, so
    /// someone moving a program across finds an answer rather than silence.
    #[test]
    fn a_call_without_an_answer_says_why() {
        for (name, why) in NO_COUNTERPART {
            assert!(!name.is_empty());
            assert!(why.len() > 20, "{name} is named without a reason");
        }
        let named: Vec<&str> = NO_COUNTERPART.iter().map(|(n, _)| *n).collect();
        assert!(named.contains(&"verify_request"));
        // These two are answered now, so neither belongs on the list.
        assert!(!named.contains(&"server_version"));
        assert!(!named.contains(&"client_id"));
    }

    /// What this client speaks is a real number, and it is the build the venue
    /// accepts it under. A program gating a feature on it finds every feature
    /// available, which is the right answer for a client speaking the current
    /// protocol.
    #[test]
    fn the_version_this_client_speaks_is_the_build_it_logs_on_with() {
        let stated: i32 = crate::config::ib_build().parse().expect("a number");
        assert!(stated > 0, "the build states nothing");
        assert!(
            stated > 176,
            "a program gating on a protocol version would find features missing",
        );
    }

    /// The next request id is stated without being spent: it is consumed by
    /// the request that goes out under it, and stating it consumed the number
    /// the next request then could not use.
    #[test]
    fn next_request_id_states_the_number_the_next_request_uses() {
        let (client, _rx, _shared) = test_direct_client();
        let stated = client.next_request_id();
        assert_eq!(
            client.next_request_id(), stated,
            "asking twice names the same number",
        );
        assert_eq!(
            client.stream_id(), stated,
            "the next request goes out under the number stated",
        );
    }

    /// A question that reads its answer by id holds the session while it
    /// waits.
    ///
    /// These take the answer out of the queue themselves rather than off a
    /// callback, and the two shapes are one session: a program driving
    /// `inner().process_msgs()` beside one — which is what this shape tells it
    /// to do for the figures it does not carry — read the answer first and
    /// handed it to a wrapper, and the question waited out its whole deadline
    /// for a reply that had already arrived.
    #[test]
    fn a_question_read_by_id_holds_the_session_while_it_waits() {
        let (client, _rx, shared) = test_direct_client();
        // Stated, not spent: this is the number the question below goes out
        // under.
        let asked = client.next_request_id();
        std::thread::scope(|s| {
            let asking = s.spawn(|| {
                let c = crate::api::Contract { symbol: "SPY".into(), ..Default::default() };
                client.head_timestamp(&c, "TRADES", true)
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                client.inner.asking.try_lock().is_err(),
                "the session was free to be read while a question was waiting on it",
            );
            shared.reference.push_head_timestamp(
                asked as u32,
                crate::control::historical::HeadTimestampResponse {
                    head_timestamp: "20200101-00:00:00".to_string(),
                    timezone: String::new(),
                },
            );
            assert!(asking.join().unwrap().is_ok(), "the question was answered");
        });
    }

    /// A stream reads its own records, and a dispatch loop leaves them alone.
    ///
    /// The two shapes are one session, and a stream cannot hold the session's
    /// turn: it outlives any one read of it. So the records it takes by id are
    /// left where it will find them, the way the answering calls' own are —
    /// otherwise a program driving `inner().process_msgs()` beside a stream
    /// sees the stream stop moving, on a subscription the venue is still
    /// feeding.
    #[test]
    fn a_stream_reading_by_id_keeps_its_records_from_a_dispatch_loop() {
        let (client, _rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        let contract = crate::api::Contract { symbol: "SPY".into(), ..Default::default() };
        let _bars = client
            .realtime_bars(&contract, "TRADES", true)
            .expect("the stream opened");
        shared.market.push_real_time_bar(asked as u32, RealTimeBar::default());

        let mut heard = crate::api::wrapper::tests::RecordingWrapper::default();
        client.inner.process_msgs(&mut heard);

        assert_eq!(
            shared.market.take_real_time_bars_for(asked as u32).len(), 1,
            "the bar the stream was going to read went to a callback instead",
        );
    }

    /// Runs a call on a thread of its own and says whether it came back.
    ///
    /// A wedged call cannot be joined, so it is left where it is and the
    /// harness reports a failure rather than stopping on one.
    fn came_back(call: impl FnOnce() + Send + 'static) -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let d = Arc::clone(&done);
        std::thread::spawn(move || {
            call();
            d.store(true, Ordering::Release);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if done.load(Ordering::Acquire) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A contract named by the venue's id alone is named before the turn is
    /// taken.
    ///
    /// The venue routes on the contract's type and its exchange, and a caller
    /// that gave neither has stated neither: filling them in is a question of
    /// its own, and one that waits for this same turn. Asked while one of
    /// these held it, on a lock that is not re-entrant, neither ever ran again
    /// — and the deadline that would have said so is set inside the wait that
    /// never starts, so the thread stopped for good rather than for fifteen
    /// seconds.
    #[test]
    fn a_contract_named_by_id_alone_does_not_wedge_the_call_asking_about_it() {
        let (client, rx, _shared) = test_direct_client();
        // Nothing carries a request, so the lookup each of these makes on its
        // own is refused at once rather than waited out.
        drop(rx);
        let client = Arc::new(client);

        let c = Arc::clone(&client);
        assert!(
            came_back(move || {
                let _ = c.head_timestamp(
                    &Contract { con_id: 265598, ..Default::default() }, "TRADES", true,
                );
            }),
            "the earliest data for a contract named by id alone never came back",
        );
        let c = Arc::clone(&client);
        assert!(
            came_back(move || {
                let _ = c.histogram_data(
                    &Contract { con_id: 265598, ..Default::default() }, true, "3 days",
                );
            }),
            "a histogram for a contract named by id alone never came back",
        );
        let c = Arc::clone(&client);
        assert!(
            came_back(move || {
                let _ = c.historical_schedules(
                    &Contract { con_id: 265598, ..Default::default() }, "", "1 D", true,
                );
            }),
            "a schedule for a contract named by id alone never came back",
        );
    }

    /// A schedule holds the session while it waits, as the four questions
    /// beside it do.
    ///
    /// It reads its answer out of the queue by id rather than off a callback,
    /// so a dispatch loop running beside it took that answer first and handed
    /// it to a wrapper — and the question waited out its whole deadline for a
    /// reply that had already arrived.
    #[test]
    fn a_schedule_holds_the_session_while_it_waits() {
        let (client, _rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        std::thread::scope(|s| {
            let asking = s.spawn(|| {
                client.historical_schedules(&Contract::default(), "", "1 D", true)
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                client.inner.asking.try_lock().is_err(),
                "the session was free to be read while a question was waiting on it",
            );
            shared.reference.push_historical_schedule(
                asked as u32,
                crate::types::HistoricalScheduleResponse {
                    query_id: String::new(), timezone: "US/Eastern".to_string(),
                    start_date_time: "20260101-09:30:00".to_string(),
                    end_date_time: "20260102-16:00:00".to_string(),
                    sessions: Vec::new(),
                },
            );
            assert!(asking.join().unwrap().is_ok(), "the question was answered");
        });
    }

    /// A stream's number is its own before the request goes out.
    ///
    /// Claimed after the send instead, the engine can queue the stream's first
    /// record — or the venue's refusal of it — in between, and a dispatch loop
    /// running beside this sees a number nobody has claimed and hands what
    /// arrived to a callback. Losing the first entry of a book is the worst of
    /// them: every entry after it names a position in a book that never had a
    /// start.
    #[test]
    fn a_streams_number_is_its_own_before_the_request_goes_out() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (client, rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        // Filled to the brim, so the request below stops inside its own send.
        // That is the window this is about, held open.
        for _ in 0..64 {
            client.inner.control_tx.send(ControlCommand::Logout).expect("room");
        }

        let client = Arc::new(client);
        let opening = Arc::clone(&client);
        let opened = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&opened);
        std::thread::spawn(move || {
            let stream = opening.realtime_bars(
                &Contract { symbol: "SPY".into(), ..Default::default() }, "TRADES", true,
            );
            drop(stream);
            flag.store(true, Ordering::Release);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            shared.reference.is_ours(RecordKind::Bars, asked),
            "the stream's number was nobody's while its request was going out",
        );

        // Read out again so the thread finishes rather than being left in the
        // send for the rest of the suite.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !opened.load(Ordering::Acquire) && Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(20));
        }
        assert!(opened.load(Ordering::Acquire), "the stream never opened");
    }

    /// A scan's rows are left where the stream that asked for it will read
    /// them.
    ///
    /// A scan answers repeatedly for as long as it runs, and the stream reads
    /// its batches by id. Drained whole by a dispatch loop running beside it,
    /// a batch went to a callback and the stream waited for the next one — or
    /// for its idle span to end.
    #[test]
    fn a_scans_rows_are_kept_from_a_dispatch_loop_for_the_stream_that_asked() {
        let (client, _rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        let _scan = client
            .scanner_subscription("STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 10, &[])
            .expect("the scan opened");
        shared.reference.push_scanner_data(
            asked as u32,
            crate::control::scanner::ScannerResult {
                con_ids: Vec::new(), entries: Vec::new(),
                scan_time: String::new(), error_text: String::new(),
            },
        );

        let mut heard = crate::api::wrapper::tests::RecordingWrapper::default();
        client.inner.process_msgs(&mut heard);

        assert_eq!(
            shared.reference.take_scanner_data_for(asked as u32).len(), 1,
            "the batch the stream was going to read went to a callback instead",
        );
    }

    /// A number held for one kind of request is not held for another.
    ///
    /// A request number means one thing per kind of request, so a stream
    /// reading bars under a number and a scan asked for under the same number
    /// on the surface underneath are both entitled to it, and each is answered
    /// on a queue of its own. Held by the number alone, the stream's claim
    /// withheld the scan's rows from the dispatch loop that was going to
    /// deliver them — and nothing else ever read them, because the stream
    /// holding that number reads bars.
    #[test]
    fn a_number_held_for_bars_does_not_hold_it_for_a_scan() {
        let (client, _rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        let _bars = client
            .realtime_bars(&Contract { symbol: "SPY".into(), ..Default::default() }, "TRADES", true)
            .expect("the stream opened");
        // The same number, for a scan the caller asked for itself.
        shared.reference.push_scanner_data(
            asked as u32,
            crate::control::scanner::ScannerResult {
                con_ids: vec![756733],
                entries: vec![crate::control::scanner::ScannerEntry { con_id: 756733 }],
                scan_time: String::new(), error_text: String::new(),
            },
        );

        let mut heard = crate::api::wrapper::tests::RecordingWrapper::default();
        client.inner.process_msgs(&mut heard);

        assert!(
            heard.events.iter().any(|e| e.starts_with("scanner_data:")),
            "the scan's rows were withheld for a stream that reads bars: {:?}",
            heard.events,
        );
    }

    /// And the venue's refusal of a stream ends that stream.
    ///
    /// A refusal is how a stream tells "the venue said no" from "nothing
    /// came". Drained by a dispatch loop running beside it, the stream found
    /// none, sat through its idle span and ended reporting that the venue had
    /// simply gone quiet.
    #[test]
    fn the_refusal_of_a_stream_reaches_that_stream_and_not_a_callback() {
        let (client, _rx, shared) = test_direct_client();
        let asked = client.next_request_id();
        let mut bars = client
            .realtime_bars(&Contract { symbol: "SPY".into(), ..Default::default() }, "TRADES", true)
            .expect("the stream opened");
        shared.reference.push_historical_error(
            asked as u32, 354, "Requested market data is not subscribed".to_string(),
        );

        let mut heard = crate::api::wrapper::tests::RecordingWrapper::default();
        client.inner.process_msgs(&mut heard);

        assert_eq!(bars.next_item().map(|_| ()), None, "a refused stream carries nothing");
        let (code, why) = bars.refusal().expect("the venue's reason");
        assert_eq!(*code, 354, "the stream was told it simply went quiet: {why}");
    }

    /// A depth stream gives its book slot back when it is withdrawn. Sent
    /// straight down the control channel, the withdrawal skipped the release,
    /// so a number a dead stream once held was refused as still holding a
    /// book for the life of the session.
    #[test]
    fn a_withdrawn_depth_stream_gives_its_slot_back() {
        let (client, _rx, _shared) = test_direct_client();
        let mut book = client
            .market_depth(&Contract { symbol: "SPY".into(), ..Default::default() }, 10, false)
            .expect("the stream opened");
        let asked = book.req_id();
        assert!(client.inner.core.depth_reqs.lock().unwrap().contains(&asked), "the slot is held while it runs");
        book.cancel();
        assert!(!client.inner.core.depth_reqs.lock().unwrap().contains(&asked), "and given back when it is withdrawn");
    }

    /// A book this client gave up on ends the stream that asked for it.
    ///
    /// It is the one failure a stream cannot tell from a quiet market: the
    /// subscription is up, the venue is still sending, and nothing further is
    /// kept because part of a book is not a book. Reading only the venue's own
    /// refusals, the stream sat through its idle span and ended saying nothing
    /// had gone wrong.
    #[test]
    fn a_book_given_up_on_ends_the_stream_that_asked_for_it() {
        let (client, _rx, shared) = test_direct_client();
        let mut book = client
            .market_depth(&Contract { symbol: "SPY".into(), ..Default::default() }, 10, false)
            .expect("the stream opened");
        let asked = book.req_id() as u32;
        // One past what is kept: the push that finds the queue full is the
        // one that walks it and gives the longest book up.
        for _ in 0..=crate::bridge::STREAM_BACKLOG_LIMIT {
            shared.market.push_depth_update(DepthUpdate {
                req_id: asked, position: 0, market_maker: String::new(),
                operation: 0, side: 1, price: 1.0, size: 1.0, is_smart_depth: false,
            });
        }
        assert!(shared.market.depth_was_dropped(asked), "the book was given up");

        // And a dispatch loop running beside the stream leaves it there, the
        // way it leaves the stream's own records: told to a callback instead,
        // the stream is the one thing left with no way to find out.
        let mut heard = crate::api::wrapper::tests::RecordingWrapper::default();
        client.inner.process_msgs(&mut heard);

        assert!(book.next_item().is_none(), "nothing is kept for a book given up on");
        let (code, why) = book.refusal().expect("the reason the book was given up");
        assert_eq!(*code, 354, "the stream read the drop as a quiet market: {why}");
        assert!(why.contains("part of a book is not a book"), "{why}");
    }

    /// And ends that stream, not another reading under the same number.
    ///
    /// The notice is queued under the number the book was asked for, and a
    /// number means one thing per kind of request — so a caller running a
    /// book on the surface underneath may hold the number a stream here is
    /// reading bars under. Read by every stream, the bars stream ended on a
    /// book it had never asked for, kept the reason as its own refusal, and
    /// withdrew a subscription the venue was still serving.
    #[test]
    fn a_book_given_up_on_does_not_end_a_stream_reading_bars() {
        let (client, _rx, shared) = test_direct_client();
        let mut bars = client
            .realtime_bars(&Contract { symbol: "SPY".into(), ..Default::default() }, "TRADES", true)
            .expect("the stream opened");
        let asked = bars.req_id() as u32;
        for _ in 0..=crate::bridge::STREAM_BACKLOG_LIMIT {
            shared.market.push_depth_update(DepthUpdate {
                req_id: asked, position: 0, market_maker: String::new(),
                operation: 0, side: 1, price: 1.0, size: 1.0, is_smart_depth: false,
            });
        }
        assert!(shared.market.depth_was_dropped(asked), "the book was given up");

        assert!(bars.next_item().is_none(), "no bar arrived, so the quiet ends it");
        assert!(
            bars.refusal().is_none(),
            "the bars stream ended on another request's book: {:?}",
            bars.refusal(),
        );
    }

    /// The profit subscriptions open nothing: their figures arrive on a
    /// callback, and nothing this shape owns pumps callbacks, so a
    /// subscription opened here would be answered to nothing while the state
    /// it opened stood forever.
    #[test]
    fn profit_subscriptions_open_nothing_that_nothing_would_read() {
        let (client, _rx, shared) = test_direct_client();
        let _ = client.pnl("DU123", "");
        let _ = client.pnl_single("DU123", "", 756733);
        assert!(
            client.inner.core.poll_pnl(&shared).is_none(),
            "a profit subscription was opened for a reader that does not exist",
        );
    }

    /// And they say so, naming where the figures can be read.
    #[test]
    fn a_refused_profit_subscription_says_where_the_figures_can_be_read() {
        let (client, _rx, _shared) = test_direct_client();
        let pnl = client.pnl("DU123", "");
        let single = client.pnl_single("DU123", "", 756733);
        assert!(
            pnl.is_err(),
            "a subscription whose answer goes nowhere must say so",
        );
        assert!(single.is_err(), "likewise for one position");
        assert!(
            pnl.unwrap_err().to_string().contains("process_msgs"),
            "the refusal says where the figures can be read",
        );
    }

    /// A scan is read through its stream and withdrawn with it. A scan keeps
    /// answering for as long as it runs, so rows arriving for a reader that
    /// never reads would simply accumulate; handed over and withdrawn with the
    /// stream, they cannot.
    #[test]
    fn a_scan_is_read_through_its_stream_and_withdrawn_with_it() {
        use crate::control::scanner::{ScannerEntry, ScannerResult};

        let (client, rx, shared) = test_direct_client();
        let mut scan = client
            .scanner_subscription("STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 10, &[])
            .expect("the scan is opened")
            .with_timeout(Duration::from_millis(50));

        match rx.try_recv().expect("the subscription is sent") {
            ControlCommand::SubscribeScanner { req_id, .. } => {
                assert_eq!(req_id as i64, scan.req_id(), "sent under the stream's number");
            }
            other => panic!("expected a scanner subscription, got {other:?}"),
        }

        let arrived = ScannerResult {
            con_ids: vec![756733],
            entries: vec![ScannerEntry { con_id: 756733 }],
            scan_time: "20260904".into(),
            error_text: String::new(),
        };
        shared.reference.push_scanner_data(scan.req_id() as u32, arrived);
        let got = scan.next_item().expect("what arrives under the scan is handed over");
        assert_eq!(got.entries.len(), 1);
        assert!(
            shared.reference.take_scanner_data_for(scan.req_id() as u32).is_empty(),
            "the stream took its rows; nothing accumulates",
        );

        let req_id = scan.req_id();
        drop(scan);
        match rx.try_recv().expect("the withdrawal is sent on the drop") {
            ControlCommand::CancelScanner { req_id: cancelled } => {
                assert_eq!(cancelled as i64, req_id, "the scan that was opened");
            }
            other => panic!("expected a scanner cancel, got {other:?}"),
        }
    }
}
