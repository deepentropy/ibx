//! Calls that answer.
//!
//! The rest of this client follows the reference client's shape: a request
//! goes out under an id and the answer arrives later on a callback, which is
//! the right shape for a program with its own event loop. It is a poor shape
//! for asking one question. These ask, wait, and hand back the answer.
//!
//! They drive `process_msgs` themselves. That drains everything the session
//! has queued, and a question keeps what carries its own request id — but what
//! it does not keep is no longer thrown away. A session installs the record it
//! keeps, and everything a question does not want goes there, so a fill or a
//! tick arriving while a question runs still reaches
//! [`Client`](crate::api::Client)'s own view of it.
//!
//! A bare client with no record of its own has nowhere to put them, and there
//! a question does still consume what it does not keep. Where nothing may be
//! missed, take the events from the channel
//! [`connect_with_events`](super::EClient::connect_with_events) hands back,
//! which the engine fills whether anything is pumping or not.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};


use crate::error_codes::Refusal;
use crate::types::model::{BarData, ContractDetails};
use crate::api::wrapper::Wrapper;

use super::{Contract, EClient};

/// How long a question waits for its answer.
const ANSWER_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS);

/// The same, for a lookup that can name a whole class of contracts.
const LOOKUP_TIMEOUT: Duration =
    Duration::from_secs(crate::config::LOOKUP_TIMEOUT_SECS);

/// Ids for questions this layer asks on the caller's behalf. Far above what a
/// caller is likely to use, so an answer to one of these is never mistaken for
/// an answer to theirs.
static NEXT_ASK_ID: AtomicI64 = AtomicI64::new(crate::bridge::ReferenceState::ASK_ID_BASE as i64);

/// An id this client asked a question under, held while the answer is
/// outstanding.
///
/// Recorded where it is handed out rather than where it is waited on: the
/// request goes out first, and an answer arriving before the wait began would
/// otherwise be taken by a caller's own dispatch. Released on drop, so a
/// question given up on — timed out, refused, or cut short by an early return
/// — stops being held.
pub(crate) struct AskId {
    id: i64,
    /// The session that is waiting, so releasing it releases it there and not
    /// on another session that happens to count from the same number.
    ///
    /// Given up by [`AskId::keep`], which is how an id outlives the call that
    /// took it without the session outliving anything: forgetting the guard
    /// whole would hold this reference for as long as the process runs.
    shared: Option<std::sync::Arc<crate::bridge::SharedState>>,
    /// Which of the venue's records the id is held for, so releasing it
    /// releases the hold that was taken and not another kind's.
    kind: crate::bridge::RecordKind,
}

impl AskId {
    /// The number the question went out under.
    pub(crate) fn get(&self) -> i64 {
        self.id
    }

    /// Take the number and stop releasing it on drop.
    ///
    /// For a subscription, which outlives the call that opened it: the id has
    /// to keep being this client's own until the caller withdraws it, and
    /// whoever withdraws it releases it with
    /// `ReferenceState::forget_ours`.
    pub(crate) fn keep(mut self) -> i64 {
        self.shared = None;
        self.id
    }
}

impl Drop for AskId {
    fn drop(&mut self) {
        if let Some(shared) = &self.shared {
            shared.reference.forget_ours(self.kind, self.id);
        }
    }
}

pub(crate) fn ask_id(shared: &std::sync::Arc<crate::bridge::SharedState>) -> AskId {
    hold_id(
        shared,
        crate::bridge::RecordKind::Answer,
        NEXT_ASK_ID.fetch_add(1, Ordering::Relaxed),
    )
}

/// Hold a number as this session's own until the guard is dropped.
///
/// For a stream, whose number is taken before the request goes out: the engine
/// can queue the stream's first record — or the venue's refusal of it — before
/// the call that sent it has built anything to read them with, and a dispatch
/// loop reaching them first sees a number nobody has claimed and hands them to
/// a callback. The first positional entry of a book is the worst one to lose:
/// every entry after it describes a book that never had a start.
///
/// `kind` says which of the venue's records the number is held for: a number
/// means one thing per kind of request, so holding it for bars must not
/// withhold the scan a caller is running under the same number.
///
/// Released on drop, so a request that could not be sent does not leave its
/// number held; [`AskId::keep`] passes the hold to the stream, which states
/// the same kind and gives it up when it ends.
pub(crate) fn hold_id(
    shared: &std::sync::Arc<crate::bridge::SharedState>,
    kind: crate::bridge::RecordKind,
    id: i64,
) -> AskId {
    shared.reference.note_ours(kind, id);
    AskId { id, shared: Some(std::sync::Arc::clone(shared)), kind }
}

#[derive(Default)]
struct Answer {
    details: Vec<ContractDetails>,
    error: Option<Refusal>,
    done: bool,
}

struct Collector {
    req_id: i64,
    answer: Arc<Mutex<Answer>>,
}

impl Wrapper for Collector {
    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
        if req_id == self.req_id {
            self.answer.lock().unwrap().details.push(details.clone());
        }
    }
    fn contract_details_end(&mut self, req_id: i64) {
        if req_id == self.req_id {
            self.answer.lock().unwrap().done = true;
        }
    }
    fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
        // The connection notices are not answers to anything.
        if matches!(code, 2104 | 2106 | 2107 | 2119 | 2158) {
            return;
        }
        if req_id == self.req_id {
            let mut a = self.answer.lock().unwrap();
            a.error = Some(Refusal::stated(code as i32, message));
            a.done = true;
        }
    }
}

/// What a venue lists for options on one underlying.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionChain {
    /// The venue listing them.
    pub exchange: String,
    /// The contract the options are on.
    pub underlying_con_id: i64,
    /// Which class of the chain these strikes belong to.
    pub trading_class: String,
    /// How many units one contract of it is worth.
    pub multiplier: String,
    /// Every expiry this venue lists.
    pub expirations: Vec<String>,
    /// Every strike it lists.
    pub strikes: Vec<f64>,
}

/// One headline the venue holds, and what an article request needs to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headline {
    pub time: String,
    pub provider_code: String,
    pub article_id: String,
    pub headline: String,
}

/// A holding, as the account states it.
#[derive(Debug, Clone)]
pub struct PositionRow {
    /// Which account it is on.
    pub account: String,
    /// What is held.
    pub contract: Contract,
    /// How much of it.
    pub position: f64,
    /// What it cost on average.
    pub avg_cost: f64,
}

/// One value the account states about itself.
#[derive(Debug, Clone)]
pub struct AccountValue {
    /// Which account it is on.
    pub account: String,
    /// Which figure this is.
    pub tag: String,
    /// What the venue states it as.
    pub value: String,
    /// What currency it is stated in.
    pub currency: String,
}

/// Where an order stands.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderReport {
    /// The order this is about.
    pub order_id: i64,
    /// The venue's word for it: `Filled`, `Cancelled`, `Inactive`, and so on.
    pub status: String,
    /// How much has filled.
    pub filled: f64,
    /// How much has not.
    pub remaining: f64,
    /// What it has paid on average, zero until something fills.
    pub avg_price: f64,
    /// Why the venue would not work it, where it said.
    pub reason: Option<String>,
}

impl OrderReport {
    /// Whether the venue has finished with this order, one way or another.
    pub fn is_done(&self) -> bool {
        matches!(self.status.as_str(), "Filled" | "Cancelled" | "ApiCancelled" | "Inactive")
    }

    /// Whether it filled in full.
    pub fn is_filled(&self) -> bool {
        self.status == "Filled" && self.remaining == 0.0
    }
}

/// One question's answer as it accumulates.
struct Pending<T> {
    rows: Vec<T>,
    /// Under the number the venue gave it. Flattened into a sentence, a caller
    /// could only match on prose for something the reference client hands it
    /// to branch on.
    error: Option<Refusal>,
    done: bool,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self { rows: Vec::new(), error: None, done: false }
    }
}

/// The connection notices are not answers to anything.
fn is_connection_notice(code: i64) -> bool {
    matches!(code, 2104 | 2106 | 2107 | 2119 | 2158)
}

/// Whether the answering-call guard re-raises a loss on drop: the connection
/// went during the wait, the caller kept no record of its own to have heard
/// it, and no restore has landed since the last pump to make the loss stale.
fn reraise_the_loss(went_during: bool, heard_by_the_caller: bool, restored_since: bool) -> bool {
    went_during && !heard_by_the_caller && !restored_since
}

/// The venue's naming of a description, with what the caller stated put back
/// around it.
///
/// What the venue names is a description of one contract, and a description
/// carries no hedge and no legs. Sent in place of what the caller stated, a
/// delta-neutral order loses the contract it hedges against and a combination
/// loses every leg, and each goes to the venue as something else entirely. The
/// naming supplies what the caller left out; it does not take away what they
/// said.
///
/// [`EClient::place_order`] does this around the naming it does itself. A call
/// that names ahead of it — because the lookup takes a turn of its own, and
/// asked while this call held that turn neither would ever run again — hands
/// it a contract that already carries an id, so that restore does not run and
/// this stands in for it.
pub(super) fn named_with_what_the_caller_stated(mut named: Contract, stated: &Contract) -> Contract {
    named.delta_neutral_contract = stated.delta_neutral_contract.clone();
    if !stated.combo_legs.is_empty() {
        named.combo_legs = stated.combo_legs.clone();
    }
    named
}

/// Holds the caller's right to be told the session closed, across pumping that
/// this client does on its own behalf.
///
/// An answering call runs the dispatch into a collector of its own, and the
/// notice that the session went away is delivered once and then latched.
/// Delivered into that collector, the caller's wrapper never hears it and
/// nothing says so again until a reconnect — the program goes on believing it
/// is connected. Restored on the way out, and only where this call is what
/// took it, so a caller that had already been told is not told twice.
struct LeaveTheCloseNoticeForTheCaller<'a> {
    client: &'a EClient,
    told_before: bool,
    connected_before: bool,
}

impl<'a> LeaveTheCloseNoticeForTheCaller<'a> {
    fn new(client: &'a EClient) -> Self {
        let told_before = client.close_notified.load(std::sync::atomic::Ordering::Acquire);
        let connected_before = client.connected.load(std::sync::atomic::Ordering::Acquire);
        Self { client, told_before, connected_before }
    }
}

impl Drop for LeaveTheCloseNoticeForTheCaller<'_> {
    fn drop(&mut self) {
        if !self.told_before {
            self.client.close_notified.store(false, std::sync::atomic::Ordering::Release);
        }
        // A loss said under 1100 during the wait was said to the collector.
        // Where no record of the caller's own was pumped beside it, the flag
        // is raised again so the caller's next pass hears it; where one was,
        // the caller's record already holds it.
        let went_during = self.connected_before
            && !self.client.connected.load(std::sync::atomic::Ordering::Acquire);
        let heard_by_the_caller = self.client.kept.lock().map(|k| k.is_some()).unwrap_or(false);
        // A restore that landed after the last pump has already set the
        // flag; re-raising the loss would clear it, and the caller would
        // read 1100 and never the 1102 the engine recovered with.
        let restored_since = self.client.shared.peek_connection_restored();
        if reraise_the_loss(went_during, heard_by_the_caller, restored_since) {
            self.client.shared.set_connection_lost();
        }
    }
}

impl EClient {
    /// Pump the queues into a question's collector, and into the record the
    /// session keeps as well.
    ///
    /// The queues empty as they are read. Read into a collector alone, every
    /// callback that collector does not implement — a fill, a trade, an
    /// order's new status — was taken off the queue and dropped, and the
    /// record a caller reads those back from never saw it. A question can run
    /// for the whole answer timeout, so that is a whole timeout of them.
    pub(crate) fn pump_for_ask(&self, collector: &mut impl crate::api::wrapper::Wrapper) {
        // Locked per pump rather than for the length of the question, so a
        // caller reading the record from another thread waits for one pass and
        // not for the answer.
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match kept {
            Some(record) => {
                let mut held = record.lock().unwrap_or_else(|e| e.into_inner());
                let mut both = crate::api::wrapper::Tee { asked: collector, kept: &mut *held };
                self.read_the_session(&mut both, true);
            }
            None => self.read_the_session(collector, false),
        }
    }

    /// Pump until the collector says the answer is complete, or time runs out.
    fn wait_for<T, W: Wrapper>(
        &self, collector: &mut W, state: &Arc<Mutex<Pending<T>>>, what: &str,
    ) -> Result<Vec<T>, Refusal> {
        let _notice = LeaveTheCloseNoticeForTheCaller::new(self);
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        // Ended with the session, not at the end of the wait: a caller
        // retrying on "no answer" otherwise paid the wait per call for ever
        // while the session had been over the whole time. The streams and
        // the other surface return at once.
        while Instant::now() < deadline {
            self.pump_for_ask(collector);
            if let Some(why) = self.shared.reference.session_over() {
                return Err(Refusal::not_connected(format!("the session is over: {why}")));
            }
            if state.lock().unwrap().done {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once more before giving up. The loop pumps, sleeps, and then tests
        // the deadline, so an answer arriving during that last sleep is still
        // queued when the loop ends — and the caller is told nothing came about
        // an answer that had.
        self.pump_for_ask(collector);
        let mut s = state.lock().unwrap();
        if let Some(e) = s.error.take() {
            return Err(e);
        }
        if !s.done {
            return Err(Refusal::no_answer(
                format!("no answer within {}s to {what}", ANSWER_TIMEOUT.as_secs()),
            ));
        }
        Ok(std::mem::take(&mut s.rows))
    }

    /// Bars for a contract, as `req_historical_data` asks for them.
    ///
    /// The venue has no adjusted series to pass through: what it serves is raw
    /// trades, and the two series the vendor states as adjusted — TRADES and
    /// ADJUSTED_LAST — are those folded with the contract's own actions. The
    /// fold is made once the series is whole and the actions are in hand,
    /// before a bar is handed to anyone. This call waits and hands the series
    /// back in one piece; `req_historical_data` delivers the same bars one at
    /// a time on its callbacks. Both ask for the actions by the venue's id for
    /// the contract, and refuse a contract that does not carry it.
    pub fn historical_data(
        &self, contract: &Contract, end_date_time: &str, duration: &str,
        bar_size: &str, what_to_show: &str, use_rth: bool,
    ) -> Result<Vec<BarData>, Refusal> {
        // One question at a time: see `EClient::asking`.
        // Named before the turn is taken. A contract given by the venue's id
        // alone is filled in by asking the venue for it, which is one of these
        // calls and waits for this same turn — asked while this one holds it,
        // on a lock that is not re-entrant, neither ever runs again and the
        // deadline that would have said so is set inside the wait that never
        // starts. `what_if_order` and `place` name theirs here for the same
        // reason; these four asked inside the turn instead.
        let contract = &*self.named_by_the_venue(contract)?;
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Bars { req_id: i64, state: Arc<Mutex<Pending<BarData>>> }
        impl Wrapper for Bars {
            fn historical_data(&mut self, req_id: i64, bar: &BarData) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().rows.push(bar.clone());
                }
            }
            fn historical_data_end(&mut self, req_id: i64, _: &str, _: &str) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Bars { req_id, state: Arc::clone(&state) };
        self.req_historical_data(
            req_id, contract, end_date_time, duration, bar_size, what_to_show, use_rth, 1, false,
        )?;
        self.wait_for(&mut collector, &state, &format!("{duration} of bars for {}", contract.symbol))
    }

    /// A contract's corporate actions, asked for and waited on.
    ///
    /// The venue answers these per contract rather than per request, which is
    /// enough to file an answer and not enough to know whose question it
    /// answers: two questions about one contract over different ranges are
    /// answered by two replies naming the same contract. The id the request
    /// went out under is carried through, and this takes only the answer to its
    /// own. A contract the venue states nothing for answers empty, which is an
    /// answer: it is how a contract that has never split says so.
    ///
    /// `contract` must carry the venue's id for it, which `qualify_contract`
    /// supplies. Days are `YYYYMMDD`.
    pub fn corporate_actions(
        &self, contract: &Contract, start_date: &str, end_date: &str,
    ) -> Result<Vec<crate::control::adjustments::Adjustment>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        if contract.con_id == 0 {
            return Err(Refusal::validation(
                "corporate actions are asked for by the venue's id for the contract, \
                 which this one does not carry: qualify it first".to_string(),
            ));
        }
        let con_id = contract.con_id.to_string();
        // The actions do not arrive on a callback: the engine files them
        // against the request that asked, and this takes its own. A refusal
        // does arrive on a callback, and without keeping it a rejected request
        // reads as a request nothing answered — the caller waits out the whole
        // deadline and is told no answer came, when the venue said why
        // immediately.
        struct Refused { req_id: i64, why: Arc<Mutex<Option<Refusal>>> }
        /// Gives the wait up on every way out, including an early return and an
        /// unwind, so nothing is kept for a question nobody is asking.
        struct StopWaiting<'a> { shared: &'a Arc<crate::bridge::SharedState>, req_id: u32 }
        impl Drop for StopWaiting<'_> {
            fn drop(&mut self) {
                self.shared.reference.stop_waiting_for_adjustments(self.req_id);
            }
        }
        impl Wrapper for Refused {
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    *self.why.lock().unwrap() = Some(Refusal::stated(code as i32, message));
                }
            }
        }
        // The contract's own record is what `EClient::adjustments` reads, and
        // it is cleared here so that reader states this question's answer
        // rather than a previous one's. What this call waits on is its own
        // slot, which no other question can fill.
        self.shared.reference.forget_adjustments(&con_id);
        let asked = ask_id(&self.shared);
        // Said before the request goes out, so an answer that arrives has
        // somewhere to be put, and taken back on every way out of this call so
        // nothing is kept for a question nobody is still asking.
        self.shared.reference.expect_adjustments(asked.get() as u32);
        let _stop = StopWaiting { shared: &self.shared, req_id: asked.get() as u32 };
        let why = Arc::new(Mutex::new(None));
        let mut refused = Refused { req_id: asked.get(), why: Arc::clone(&why) };
        // Through the private path: the public one refuses this number, which
        // is the point of reserving it.
        self.ask_for_adjustments(
            asked.get() as u32, contract.con_id, &contract.sec_type, &contract.exchange,
            start_date, end_date,
        )?;
        let _notice = LeaveTheCloseNoticeForTheCaller::new(self);
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        while Instant::now() < deadline {
            self.pump_for_ask(&mut refused);
            if let Some(why) = self.shared.reference.session_over() {
                return Err(Refusal::not_connected(format!("the session is over: {why}")));
            }
            if let Some(refusal) = why.lock().unwrap().take() {
                return Err(refusal);
            }
            if let Some(actions) =
                self.shared.reference.take_adjustments_answering(asked.get() as u32)
            {
                return Ok(actions);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once more before giving up. The loop sleeps and then tests the
        // deadline, so an answer arriving during that last sleep is never
        // looked for — and the guard on the way out throws it away. A caller
        // told nothing came, about an answer that had.
        self.pump_for_ask(&mut refused);
        if let Some(refusal) = why.lock().unwrap().take() {
            return Err(refusal);
        }
        if let Some(actions) =
            self.shared.reference.take_adjustments_answering(asked.get() as u32)
        {
            return Ok(actions);
        }
        // Given up on here, so the venue is told: it serves the query until it
        // is withdrawn, and a caller that has stopped waiting leaves one
        // running otherwise.
        let _ = self.send(crate::types::ControlCommand::CancelCorporateActions {
            req_id: asked.get() as u32,
        });
        Err(Refusal::no_answer(format!(
            "no answer within {}s to the corporate actions of {}",
            ANSWER_TIMEOUT.as_secs(), contract.symbol,
        )))
    }

    /// Every expiration and strike each venue lists for an underlying.
    ///
    /// `underlying` must carry the id of the contract the options are on — the
    /// stock, not the option — which `qualify_contract` supplies.
    pub fn option_chain(
        &self, underlying: &Contract,
    ) -> Result<Vec<OptionChain>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Chain { req_id: i64, state: Arc<Mutex<Pending<OptionChain>>> }
        impl Wrapper for Chain {
            fn security_definition_option_parameter(
                &mut self, req_id: i64, exchange: &str, underlying_con_id: i64,
                trading_class: &str, multiplier: &str, expirations: &[String], strikes: &[f64],
            ) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().rows.push(OptionChain {
                        exchange: exchange.to_string(),
                        underlying_con_id,
                        trading_class: trading_class.to_string(),
                        multiplier: multiplier.to_string(),
                        expirations: expirations.to_vec(),
                        strikes: strikes.to_vec(),
                    });
                }
            }
            fn security_definition_option_parameter_end(&mut self, req_id: i64) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        if underlying.con_id == 0 {
            return Err(Refusal::validation(format!(
                "the chain is asked for by the id of the contract the options are on, and {} \
                 carries none: qualify it first",
                underlying.symbol,
            )));
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Chain { req_id, state: Arc::clone(&state) };
        self.req_sec_def_opt_params(
            req_id, &underlying.symbol, "", &underlying.sec_type, underlying.con_id,
        )?;
        self.wait_for(&mut collector, &state, &format!("the option chain on {}", underlying.symbol))
    }

    /// The earliest moment the venue holds data for a contract.
    ///
    /// The same question `req_head_time_stamp` asks.
    pub fn head_timestamp(
        &self, contract: &Contract, what_to_show: &str, use_rth: bool,
    ) -> Result<String, Refusal> {
        // One question at a time: see `EClient::asking`.
        // Named before the turn is taken. A contract given by the venue's id
        // alone is filled in by asking the venue for it, which is one of these
        // calls and waits for this same turn — asked while this one holds it,
        // on a lock that is not re-entrant, neither ever runs again and the
        // deadline that would have said so is set inside the wait that never
        // starts. `what_if_order` and `place` name theirs here for the same
        // reason; these four asked inside the turn instead.
        let contract = &*self.named_by_the_venue(contract)?;
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Head { req_id: i64, state: Arc<Mutex<Pending<String>>> }
        impl Wrapper for Head {
            fn head_timestamp(&mut self, req_id: i64, head_timestamp: &str) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.push(head_timestamp.to_string());
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Head { req_id, state: Arc::clone(&state) };
        self.req_head_time_stamp(req_id, contract, what_to_show, use_rth, 1)?;
        let what = format!("the first data the venue holds for {}", contract.symbol);
        Ok(self.wait_for(&mut collector, &state, &what)?.remove(0))
    }

    /// Contracts whose name or symbol matches a pattern.
    pub fn matching_symbols(
        &self, pattern: &str,
    ) -> Result<Vec<crate::types::model::ContractDescription>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Matches {
            req_id: i64,
            state: Arc<Mutex<Pending<crate::types::model::ContractDescription>>>,
        }
        impl Wrapper for Matches {
            fn symbol_samples(
                &mut self, req_id: i64, descriptions: &[crate::types::model::ContractDescription],
            ) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.extend(descriptions.iter().cloned());
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Matches { req_id, state: Arc::clone(&state) };
        self.req_matching_symbols(req_id, pattern)?;
        self.wait_for(&mut collector, &state, &format!("a search for {pattern}"))
    }

    /// The headlines the venue holds for a contract, up to the number asked
    /// for.
    ///
    /// Each is the time, the provider's code, the article's id and the
    /// headline itself. Reading an article needs the first two.
    ///
    /// The venue states whether it holds more than it sent, and that is
    /// reported through the log rather than in the returned rows: what comes
    /// back is a page, and a full one is not evidence there is no next one.
    /// Ask for more, or narrow the window, to see the rest.
    pub fn news_headlines(
        &self, con_id: i64, provider_codes: &str,
        start_date_time: &str, end_date_time: &str, total_results: i32,
    ) -> Result<Vec<Headline>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Headlines { req_id: i64, state: Arc<Mutex<Pending<Headline>>> }
        impl Wrapper for Headlines {
            fn historical_news(
                &mut self, req_id: i64, time: &str, provider_code: &str,
                article_id: &str, headline: &str,
            ) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().rows.push(Headline {
                        time: time.to_string(),
                        provider_code: provider_code.to_string(),
                        article_id: article_id.to_string(),
                        headline: headline.to_string(),
                    });
                }
            }
            fn historical_news_end(&mut self, req_id: i64, has_more: bool) {
                if req_id == self.req_id {
                    // Said rather than dropped. A caller reading a full page
                    // has nothing else to tell "this is all of them" from
                    // "this is the first of many".
                    if has_more {
                        log::info!(
                            "the venue holds more headlines for this contract than the \
                             {} asked for, so these are the most recent of them",
                            self.state.lock().unwrap().rows.len(),
                        );
                    }
                    self.state.lock().unwrap().done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Headlines { req_id, state: Arc::clone(&state) };
        // A count below zero is not a count: cast unchecked it asked for four
        // billion headlines.
        let total_results = u32::try_from(total_results).map_err(|_| {
            Refusal::validation(format!("total_results {total_results} is negative"))
        })?;
        self.req_historical_news(
            req_id, con_id, provider_codes, start_date_time, end_date_time, total_results,
        )?;
        self.wait_for(&mut collector, &state, &format!("headlines for contract {con_id}"))
    }

    /// How a contract's trades were spread across prices.
    pub fn histogram_data(
        &self, contract: &Contract, use_rth: bool, period: &str,
    ) -> Result<Vec<(f64, i64)>, Refusal> {
        // One question at a time: see `EClient::asking`.
        // Named before the turn is taken. A contract given by the venue's id
        // alone is filled in by asking the venue for it, which is one of these
        // calls and waits for this same turn — asked while this one holds it,
        // on a lock that is not re-entrant, neither ever runs again and the
        // deadline that would have said so is set inside the wait that never
        // starts. `what_if_order` and `place` name theirs here for the same
        // reason; these four asked inside the turn instead.
        let contract = &*self.named_by_the_venue(contract)?;
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Histogram { req_id: i64, state: Arc<Mutex<Pending<(f64, i64)>>> }
        impl Wrapper for Histogram {
            fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.extend(items.iter().copied());
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Histogram { req_id, state: Arc::clone(&state) };
        self.req_histogram_data(req_id, contract, use_rth, period)?;
        let what = format!("how {} traded across prices", contract.symbol);
        self.wait_for(&mut collector, &state, &what)
    }

    /// A fundamental document about a contract, as the venue writes it.
    pub fn fundamental_data(
        &self, contract: &Contract, report_type: &str,
    ) -> Result<String, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Document { req_id: i64, state: Arc<Mutex<Pending<String>>> }
        impl Wrapper for Document {
            fn fundamental_data(&mut self, req_id: i64, data: &str) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.push(data.to_string());
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Document { req_id, state: Arc::clone(&state) };
        self.req_fundamental_data(req_id, contract, report_type)?;
        let what = format!("a {report_type} for {}", contract.symbol);
        Ok(self.wait_for(&mut collector, &state, &what)?.remove(0))
    }

    /// What the venue says an order would cost, without placing it.
    ///
    /// The order is marked as a question rather than an instruction, so
    /// nothing reaches the market.
    ///
    /// The preview states the order's own type, so a security that refuses a
    /// type refuses the preview of it rather than answering about an order
    /// that was not asked about. What it cannot state is the instruction that
    /// separates types sharing one: trailing, relative and the pegged pair are
    /// all sent as "P" and separated by their ExecInst, which a preview does
    /// not carry, so the venue reads any of them as the same peg. The margin is
    /// the same either way — it follows the position the order would leave, not
    /// the instruction that reaches it.
    ///
    /// A type this client states no value for is previewed as a limit at the
    /// same price, which is the only thing left to ask.
    pub fn what_if_order(
        &self, contract: &Contract, order: &crate::types::model::Order,
    ) -> Result<crate::types::model::OrderState, Refusal> {
        // Named before the turn is taken. Placing an order names an unqualified
        // contract by asking the venue for it, which is one of these calls and
        // waits its own turn — asked while this one holds it, on a lock that is
        // not re-entrant, neither ever runs again and the deadline that would
        // have said so never starts.
        //
        // Named here, the contract reaches `place_order` carrying an id, so the
        // restore it does around its own naming does not run and this call
        // carries it instead: the preview of a delta-neutral order or of a
        // combination is a preview of the order that would be placed, not of a
        // bare contract the venue was asked to name.
        let named;
        let contract = if contract.con_id == 0 {
            named = named_with_what_the_caller_stated(self.qualify_contract(contract)?, contract);
            &named
        } else {
            contract
        };
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Preview {
            order_id: i64,
            state: Arc<Mutex<Pending<crate::types::model::OrderState>>>,
        }
        impl Wrapper for Preview {
            fn open_order(
                &mut self, order_id: i64, _c: &Contract, _o: &crate::types::model::Order,
                order_state: &crate::types::model::OrderState,
            ) {
                if order_id == self.order_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.push(order_state.clone());
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.order_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked_under = ask_id(&self.shared);
        let order_id = asked_under.get();
        let asked = crate::types::model::Order { what_if: true, ..order.clone() };
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Preview { order_id, state: Arc::clone(&state) };
        self.place_order(order_id, contract, &asked)?;
        let what = format!("a preview of {} {} {}", asked.action, asked.total_quantity, contract.symbol);
        match self.wait_for(&mut collector, &state, &what) {
            Ok(mut rows) => Ok(rows.remove(0)),
            Err(refused) => {
                // The question ended without an answer the caller could
                // keep — refused or silent — but its record was placed on
                // the book like an order's when it went out. Taken back
                // here, where the ending is known; nothing later sees a
                // preview to retire, and left standing it read as a
                // working order and its number as spent.
                self.core.untrack_order(order_id as u64);
                Err(refused)
            }
        }
    }

    /// Every holding in the account.
    pub fn positions(&self) -> Result<Vec<PositionRow>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Held { state: Arc<Mutex<Pending<PositionRow>>> }
        impl Wrapper for Held {
            fn position(&mut self, account: &str, contract: &Contract, position: f64, avg_cost: f64) {
                self.state.lock().unwrap().rows.push(PositionRow {
                    account: account.to_string(),
                    contract: contract.clone(),
                    position,
                    avg_cost,
                });
            }
            fn position_end(&mut self) {
                self.state.lock().unwrap().done = true;
            }
            // No arm for the venue's errors, deliberately. Holdings are asked
            // for account-wide, so a refusal about them carries no request to
            // match on — and taking every refusal that arrives while this runs
            // takes the ones that do not belong to it: an order reject under
            // its own id, a subscription failure, the venue's unattributed
            // text, a farm reconnect notice. Worse, an error ends the wait,
            // and the wait hands back its rows only on the way out — so a
            // question that stopped on somebody else's refusal returned
            // nothing at all where it used to return the holdings it had.
            //
            // The one refusal that is this question's — that the account had
            // not finished stating its holdings — is said in the log by the
            // call that raises it. A short list is worse than a complete one
            // and better than none.
        }
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Held { state: Arc::clone(&state) };
        self.req_positions(&mut collector);
        self.wait_for(&mut collector, &state, "the account's holdings")
    }

    /// The account values named by `tags`, as `req_account_summary` asks for
    /// them. `tags` is a comma-separated list, or `All`.
    pub fn account_summary(&self, tags: &str) -> Result<Vec<AccountValue>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Values { req_id: i64, state: Arc<Mutex<Pending<AccountValue>>> }
        impl Wrapper for Values {
            fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().rows.push(AccountValue {
                        account: account.to_string(),
                        tag: tag.to_string(),
                        value: value.to_string(),
                        currency: currency.to_string(),
                    });
                }
            }
            fn account_summary_end(&mut self, req_id: i64) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Values { req_id, state: Arc::clone(&state) };
        self.req_account_summary(req_id, "All", tags);
        let rows = self.wait_for(&mut collector, &state, "the account summary");
        self.cancel_account_summary(req_id);
        rows
    }

    /// Wait for an order to reach a state the venue will not move it from.
    ///
    /// Placing an order says only that it was sent. What happened to it arrives
    /// later, on a callback, spread across a status and possibly a refusal.
    /// This waits for the venue to finish with it and reports where it landed —
    /// including the refusal, which is the part a caller most needs and the
    /// part most easily missed.
    ///
    /// A wait that runs out is not a failure of the order: it says only that
    /// the venue had not finished, and the order is still working.
    pub fn await_order(
        &self, order_id: i64, timeout: Duration,
    ) -> Result<OrderReport, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        self.await_order_holding_the_turn(order_id, timeout)
    }

    /// [`await_order`](EClient::await_order) for a caller that already holds
    /// the turn, because it sent the order under it.
    ///
    /// Placing and then waiting under two separate turns leaves a gap between
    /// them, and a question that starts in the gap pumps the order's own reply
    /// into its collector and discards it. The order is then reported as
    /// unanswered although the venue answered.
    pub(crate) fn await_order_holding_the_turn(
        &self, order_id: i64, timeout: Duration,
    ) -> Result<OrderReport, Refusal> {
        struct Watch { order_id: i64, report: Arc<Mutex<Option<OrderReport>>>, done: Arc<Mutex<bool>> }
        impl Wrapper for Watch {
            fn order_status(
                &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
                avg_price: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64,
            ) {
                if order_id != self.order_id {
                    return;
                }
                let mut r = self.report.lock().unwrap();
                let reason = r.as_ref().and_then(|p| p.reason.clone());
                let report = OrderReport {
                    order_id, status: status.to_string(), filled, remaining,
                    // A status arriving after a fill can state no average; the
                    // one already reported is the better answer. Nothing is
                    // what zero means, and only zero: an instrument can trade
                    // at a negative price, and reading anything below zero as
                    // unstated reported the previous average for exactly those.
                    avg_price: if avg_price != 0.0 {
                        avg_price
                    } else {
                        r.as_ref().map_or(0.0, |p| p.avg_price)
                    },
                    reason,
                };
                if report.is_done() {
                    *self.done.lock().unwrap() = true;
                }
                *r = Some(report);
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id != self.order_id || is_connection_notice(code) {
                    return;
                }
                let mut r = self.report.lock().unwrap();
                match r.as_mut() {
                    Some(report) => report.reason = Some(message.to_string()),
                    None => {
                        *r = Some(OrderReport {
                            order_id: req_id,
                            status: String::new(),
                            filled: 0.0,
                            remaining: 0.0,
                            avg_price: 0.0,
                            reason: Some(message.to_string()),
                        });
                    }
                }
            }
        }
        let report = Arc::new(Mutex::new(None));
        let done = Arc::new(Mutex::new(false));
        let mut watch = Watch {
            order_id,
            report: Arc::clone(&report),
            done: Arc::clone(&done),
        };
        // The notice that the session went away is delivered once and then
        // latched. Pumped into this collector, which does not take it, the
        // caller's own wrapper never hears it and nothing says so again until
        // a reconnect — so a caller waiting on an order when the session drops
        // is told the order said nothing, and goes on believing it is
        // connected. Left for them the way the other waits here leave it.
        let _leave_the_notice = LeaveTheCloseNoticeForTheCaller::new(self);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump_for_ask(&mut watch);
            if let Some(why) = self.shared.reference.session_over() {
                return Err(Refusal::not_connected(format!("the session is over: {why}")));
            }
            if *done.lock().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once more before answering. The loop sleeps and then tests its
        // deadline, so a terminal status arriving in that last sleep is still
        // queued — and a caller is handed the working one it replaced, or told
        // the order said nothing when it had.
        self.pump_for_ask(&mut watch);
        report.lock().unwrap().clone().ok_or_else(|| {
            Refusal::no_answer(
                format!("order {order_id} said nothing within {}s", timeout.as_secs()))
        })
    }

    /// Every contract matching the one described.
    ///
    /// The same question `req_contract_details` asks, answered here instead of
    /// on a callback.
    ///
    pub fn contract_details(&self, contract: &Contract) -> Result<Vec<ContractDetails>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let answer = Arc::new(Mutex::new(Answer::default()));
        let mut collector = Collector { req_id, answer: Arc::clone(&answer) };
        self.req_contract_details(req_id, contract)?;

        let _notice = LeaveTheCloseNoticeForTheCaller::new(self);
        // The clock measures silence, not the length of the answer. A class
        // naming every expiry is thousands of definitions and the venue sends
        // for as long as that takes; bounded on the total, an answer that is
        // still arriving is given up on and the class reads as one the venue
        // does not carry.
        let mut quiet_since = Instant::now();
        let mut had = 0usize;
        while quiet_since.elapsed() < LOOKUP_TIMEOUT {
            self.pump_for_ask(&mut collector);
            if let Some(why) = self.shared.reference.session_over() {
                return Err(Refusal::not_connected(format!("the session is over: {why}")));
            }
            let a = answer.lock().unwrap();
            if a.done {
                break;
            }
            if a.details.len() != had {
                had = a.details.len();
                quiet_since = Instant::now();
            }
            drop(a);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once more before giving up, for the reason the shared waiter states:
        // the loop sleeps and then tests its deadline, so an answer arriving in
        // that last sleep is still queued when it ends.
        self.pump_for_ask(&mut collector);

        let mut a = answer.lock().unwrap();
        if let Some(e) = a.error.take() {
            return Err(e);
        }
        if !a.done {
            // What arrived says which of two things happened. Nothing at all
            // is a question the venue has not begun answering; a partial set
            // is one it was still answering when this stopped waiting, and a
            // caller told only "no answer" would read a class the venue serves
            // in full as one it does not carry.
            let so_far = a.details.len();
            return Err(Refusal::no_answer(if so_far == 0 {
                format!(
                    "no answer within {}s to a contract lookup for {} {}",
                    LOOKUP_TIMEOUT.as_secs(), contract.sec_type, contract.symbol,
                )
            } else {
                format!(
                    "a contract lookup for {} {} went quiet for {}s having sent \
                     {so_far} definitions, so what arrived is part of an answer: \
                     ask for less at once, by naming an expiry or a single venue",
                    contract.sec_type, contract.symbol, ANSWER_TIMEOUT.as_secs(),
                )
            }));
        }
        Ok(std::mem::take(&mut a.details))
    }

    /// Fill in what the venue knows about a contract, above all its id.
    ///
    /// Most of what this client sends carries a contract, and a contract with
    /// an id is worth more than one without: market data is answered only for
    /// a contract named by id, and an order that carries one needs to state
    /// nothing else. Ask this first and pass the result around.
    ///
    /// A description matching more than one contract is refused rather than
    /// resolved to whichever came back first — the same symbol on the same
    /// venue exists in more than one currency, and picking one silently is how
    /// an order ends up on the wrong one.
    pub fn qualify_contract(&self, contract: &Contract) -> Result<Contract, Refusal> {
        // No turn taken here: this asks nothing itself, and `contract_details`
        // takes one. Taking a second would wait on the first for ever — the
        // lock is not re-entrant, which is what makes it a lock.
        let mut found = self.contract_details(contract)?;
        match found.len() {
            0 => Err(Refusal::no_definition(format!(
                "no contract matches {} {} on {}",
                contract.sec_type, contract.symbol, contract.exchange,
            ))),
            1 => Ok(found.remove(0).contract),
            n => {
                let mut how: Vec<String> = found.iter()
                    .take(4)
                    .map(|d| format!(
                        "{} {} {} {}",
                        d.contract.con_id, d.contract.local_symbol,
                        d.contract.currency, d.contract.exchange,
                    ))
                    .collect();
                if n > how.len() {
                    how.push(format!("and {} more", n - how.len()));
                }
                Err(Refusal::no_definition(format!(
                    "{} {} matches {n} contracts, so it names none: {}",
                    contract.sec_type, contract.symbol, how.join("; "),
                )))
            }
        }
    }

    /// The contract as the venue names it, where the caller named it by id
    /// alone.
    ///
    /// A request states the contract's security type and its exchange, and the
    /// venue routes on both. A caller that gave neither has stated neither,
    /// and both are the venue's to say: asked for by id, it answers with them.
    /// Stamping in a guess instead sends a future or an option out as a
    /// smart-routed US stock.
    ///
    /// Costs a round trip, so it happens only where the caller left them out.
    pub(crate) fn named_by_the_venue<'a>(
        &self, contract: &'a Contract,
    ) -> Result<std::borrow::Cow<'a, Contract>, Refusal> {
        if contract.con_id != 0
            && (contract.sec_type.is_empty() || contract.exchange.is_empty())
        {
            return Ok(std::borrow::Cow::Owned(self.qualify_contract(contract)?));
        }
        Ok(std::borrow::Cow::Borrowed(contract))
    }

    /// Fill in a batch of contracts, keeping the caller's order.
    ///
    /// Stops at the first that cannot be named, because a caller building a
    /// basket wants to know which one is wrong, not to trade the rest.
    pub fn qualify_contracts(&self, contracts: &[Contract]) -> Result<Vec<Contract>, Refusal> {
        contracts.iter().map(|c| self.qualify_contract(c)).collect()
    }
}

/// One row of a scan: where it ranked and what it is.
#[derive(Debug, Clone)]
pub struct ScanRow {
    /// Where it came in the scan.
    pub rank: i32,
    /// What the venue says the contract is.
    pub details: ContractDetails,
    /// How far it sits from the scan's benchmark. Empty: the venue answers a
    /// scan on this connection with the contracts it found and the time it ran,
    /// and states none of the three below.
    pub distance: String,
    /// What that benchmark is. Empty, as above.
    pub benchmark: String,
    /// What the scan projects. Empty, as above.
    pub projection: String,
}

/// When a contract trades, and when it does not.
#[derive(Debug, Clone)]
pub struct Schedule {
    /// The window this covers.
    pub start: String,
    /// And its end.
    pub end: String,
    /// The zone the times above are stated in.
    pub time_zone: String,
    /// Each session, in the order the venue states it: the day it belongs to,
    /// then when it opens and when it closes.
    pub sessions: Vec<(String, String, String)>,
}

impl EClient {
    /// Run a scan and hand back what it found.
    ///
    /// The subscription is withdrawn before this returns: a scan asked for once
    /// is a question, and left running it keeps answering into a session nobody
    /// is reading.
    pub fn scan(
        &self, instrument: &str, location: &str, scan_code: &str, most: u32,
    ) -> Result<Vec<ScanRow>, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Rows { req_id: i64, state: Arc<Mutex<Pending<ScanRow>>> }
        impl Wrapper for Rows {
            fn scanner_data(
                &mut self, req_id: i64, rank: i32, details: &ContractDetails,
                distance: &str, benchmark: &str, projection: &str, _legs: &str,
            ) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().rows.push(ScanRow {
                        rank, details: details.clone(),
                        distance: distance.to_string(),
                        benchmark: benchmark.to_string(),
                        projection: projection.to_string(),
                    });
                }
            }
            fn scanner_data_end(&mut self, req_id: i64) {
                if req_id == self.req_id {
                    self.state.lock().unwrap().done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Rows { req_id, state: Arc::clone(&state) };
        self.req_scanner_subscription(req_id, instrument, location, scan_code, most, &[])?;
        let found = self.wait_for(&mut collector, &state, &format!("a {scan_code} scan"));
        // Withdrawn, and said so when it is not: a scan left running keeps
        // answering into a session nobody is reading, and this call states that
        // it does not leave one.
        if let Err(e) = self.cancel_scanner_subscription(req_id) {
            log::warn!("scan {req_id} was not withdrawn: {e}");
        }
        found
    }

    /// When a contract trades, over a window ending now.
    pub fn schedule(&self, contract: &Contract, duration: &str) -> Result<Schedule, Refusal> {
        // One question at a time: see `EClient::asking`.
        // Named before the turn is taken. A contract given by the venue's id
        // alone is filled in by asking the venue for it, which is one of these
        // calls and waits for this same turn — asked while this one holds it,
        // on a lock that is not re-entrant, neither ever runs again and the
        // deadline that would have said so is set inside the wait that never
        // starts. `what_if_order` and `place` name theirs here for the same
        // reason; these four asked inside the turn instead.
        let contract = &*self.named_by_the_venue(contract)?;
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct When { req_id: i64, state: Arc<Mutex<Pending<Schedule>>> }
        impl Wrapper for When {
            fn historical_schedule(
                &mut self, req_id: i64, start: &str, end: &str, time_zone: &str,
                sessions: &[(String, String, String)],
            ) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.push(Schedule {
                        start: start.to_string(), end: end.to_string(),
                        time_zone: time_zone.to_string(), sessions: sessions.to_vec(),
                    });
                    s.done = true;
                }
            }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = When { req_id, state: Arc::clone(&state) };
        self.req_historical_schedule(req_id, contract, "", duration, true)?;
        let found = self.wait_for(&mut collector, &state, "a trading schedule")?;
        found.into_iter().next().ok_or_else(|| {
            Refusal::no_answer("the venue stated no schedule for this contract".to_string())
        })
    }

    /// What the corporate-events calendar says it carries.
    ///
    /// As the venue's JSON: it states a schema of its own that changes
    /// without notice, and a shape imposed here would be a shape to keep in
    /// step with it.
    pub fn calendar_schema(&self) -> Result<String, Refusal> {
        self.ask_wsh(None)
    }

    /// The calendar's events for one contract, as the venue's JSON.
    pub fn calendar_events(&self, con_id: i64) -> Result<String, Refusal> {
        self.ask_wsh(Some(con_id))
    }

    fn ask_wsh(&self, con_id: Option<i64>) -> Result<String, Refusal> {
        // One question at a time: see `EClient::asking`.
        let _turn = self.take_the_turn()?;
        // Numbered in the band reserved for these calls, which the request
        // surface refuses to anyone else.
        let _answering = super::Answering::begin();
        struct Json { req_id: i64, state: Arc<Mutex<Pending<String>>> }
        impl Json {
            fn take(&mut self, req_id: i64, data: &str) {
                if req_id == self.req_id {
                    let mut s = self.state.lock().unwrap();
                    s.rows.push(data.to_string());
                    s.done = true;
                }
            }
        }
        impl Wrapper for Json {
            fn wsh_meta_data(&mut self, req_id: i64, data: &str) { self.take(req_id, data) }
            fn wsh_event_data(&mut self, req_id: i64, data: &str) { self.take(req_id, data) }
            fn error(&mut self, req_id: i64, code: i64, message: &str, _: &str) {
                if req_id == self.req_id && !is_connection_notice(code) {
                    let mut s = self.state.lock().unwrap();
                    s.error = Some(Refusal::stated(code as i32, message));
                    s.done = true;
                }
            }
        }
        let asked = ask_id(&self.shared);
        let req_id = asked.get();
        let state = Arc::new(Mutex::new(Pending::default()));
        let mut collector = Json { req_id, state: Arc::clone(&state) };
        match con_id {
            None => self.req_wsh_meta_data(req_id)?,
            Some(con_id) => self.req_wsh_event_data(req_id, crate::types::CalendarQuery {
                con_id: Some(con_id),
                ..Default::default()
            })?,
        }
        let what = if con_id.is_some() { "calendar events" } else { "the calendar's schema" };
        self.wait_for(&mut collector, &state, what)?
            .into_iter()
            .next()
            .ok_or_else(|| Refusal::no_answer(format!("the venue stated no {what}")))
    }
}

#[cfg(test)]
mod ask_id_holds_nothing_after_it_is_kept {
    use super::*;
    use std::sync::Arc;

    /// An id kept past the call that took it does not keep the session with it.
    ///
    /// Forgetting the guard whole held a strong reference to the whole session
    /// for as long as the process ran, so every stream a caller opened kept a
    /// disconnected session's state alive.
    #[test]
    fn keeping_an_id_releases_the_session() {
        let shared = Arc::new(crate::bridge::SharedState::new());
        let before = Arc::strong_count(&shared);

        let id = ask_id(&shared).keep();
        assert_eq!(
            Arc::strong_count(&shared),
            before,
            "keeping id {id} held a reference to the session it was taken from",
        );
        assert!(
            shared.reference.is_ours(crate::bridge::RecordKind::Answer, id),
            "and the id is still this session's own",
        );
    }

    /// The answering-call guard re-raises a loss only when no restore has
    /// landed since the last pump. A restore that arrives after the last pump
    /// sets the flag; re-raising would clear it and the caller would read 1100
    /// and never the 1102 the engine recovered with.
    #[test]
    fn the_guard_reraises_a_loss_only_when_no_restore_has_landed() {
        assert!(reraise_the_loss(true, false, false), "loss during the wait, no record, no restore");
        assert!(!reraise_the_loss(true, false, true), "a restore since the last pump makes the loss stale");
        assert!(!reraise_the_loss(false, false, false), "no loss during the wait, nothing to re-raise");
        assert!(!reraise_the_loss(true, true, false), "the caller's own record already heard it");
    }

    /// One released the ordinary way gives the session up too.
    #[test]
    fn dropping_a_guard_releases_both() {
        let shared = Arc::new(crate::bridge::SharedState::new());
        let before = Arc::strong_count(&shared);
        let id = {
            let asked = ask_id(&shared);
            assert!(Arc::strong_count(&shared) > before, "the guard holds it while it lives");
            asked.get()
        };
        assert_eq!(Arc::strong_count(&shared), before, "and lets go when it ends");
        assert!(
            !shared.reference.is_ours(crate::bridge::RecordKind::Answer, id),
            "and the id goes with it",
        );
    }
}
