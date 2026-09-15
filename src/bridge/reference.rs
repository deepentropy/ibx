//! What the venue has answered about contracts, history and news.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::collections::HashMap;
use crate::control::historical::{HistoricalResponse, HeadTimestampResponse};
use crate::control::contracts::{ContractDefinition, OptionChainScope, SymbolMatch};
use crate::control::scanner::ScannerResult;
use crate::control::news::NewsHeadline;
use crate::control::histogram::HistogramEntry;
use crate::control::contracts::MarketRule;
use crate::types::*;
use crate::types::model as api;

/// A contract's corporate actions as the venue stated them.
type StatedActions = (
    crate::control::adjustments::AdjustedContract,
    Vec<crate::control::adjustments::Adjustment>,
);

/// What the venue states about a contract's issuer, by its id for the
/// contract and the series the statement arrived on.
type CompanyData = std::collections::HashMap<(u32, u32), Vec<(String, String)>>;

/// Which of the venue's records a number is held for.
///
/// A request number means one thing per kind of request: a caller may hold the
/// same number for bars and for a scan at once, and each kind is answered on a
/// queue of its own. Recorded by the number alone, the first reader to claim it
/// held it against every kind — so another kind's records were withheld from
/// the dispatch loop that was going to deliver them, and nothing else read them
/// either, because the reader holding the number reads somewhere else.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecordKind {
    /// Bars, as the venue prints them.
    Bars,
    /// A contract's book: its entries, and the notice that ends one.
    Depth,
    /// A scan's rows.
    Scanner,
    /// The one answer to a question this client asked on its own account.
    Answer,
}

impl RecordKind {
    /// Every kind, for the one queue that cannot say which it belongs to.
    const ALL: [Self; 4] = [Self::Bars, Self::Depth, Self::Scanner, Self::Answer];
}

/// Historical data, contract definitions, scanners, news archives, market rules,
/// contract cache.
pub struct ReferenceState {
    /// The numbers this session is itself reading records under, each against
    /// the kind of record it is reading — a number means one thing per kind of
    /// request, so holding one for bars holds it for bars alone.
    ///
    /// Held here rather than beside the counter that hands them out: the
    /// queues these guard belong to a session, and two sessions in one process
    /// count from the same number, so a set shared between them would let one
    /// release what the other is waiting on.
    ours_in_flight: Mutex<std::collections::HashSet<(RecordKind, i64)>>,
    historical_data: Mutex<Vec<(u32, HistoricalResponse)>>,
    head_timestamps: Mutex<Vec<(u32, HeadTimestampResponse)>>,
    /// Set while the smart-component table is this client's own rather than
    /// the venue's.
    smart_components_provisional: AtomicBool,
    contract_details: Mutex<Vec<(u32, ContractDefinition)>>,
    contract_details_end: Mutex<Vec<u32>>,
    matching_symbols: Mutex<Vec<(u32, Vec<SymbolMatch>)>>,
    /// Orders this session did not place, paired with the number it reaches
    /// them under. Drained to `order_bound` on each surface.
    orders_bound: Mutex<Vec<(i64, i64, i64)>>,
    /// Which of those pairings have been stated, for as long as the session
    /// lasts. Kept apart from the queue above because the queue is emptied
    /// every time a caller reads it, and a set that forgets on being read
    /// cannot say whether something was said before.
    orders_bound_said: Mutex<std::collections::HashSet<i64>>,
    /// The calendar's answers, as the venue wrote them. Two shapes on one
    /// envelope — what event types exist, and the events themselves — kept
    /// apart so a caller waiting on one is not handed the other.
    calendar_meta_data: Mutex<Vec<(u32, String)>>,
    calendar_events: Mutex<Vec<(u32, String)>>,
    /// A whole option chain answer: the underlying's conId, and one entry per
    /// scope the venue listed. The list is what the dispatcher reports before
    /// ending the request, so an empty one still ends it.
    option_params: Mutex<Vec<(u32, i64, Vec<OptionChainScope>)>>,
    scanner_params: Mutex<Vec<String>>,
    /// A partition of the advisor's configuration the venue has stated, as the
    /// number it is asked for under and the document itself.
    advisor_config: Mutex<Vec<(i32, String)>>,
    /// The end of a replacement, as the caller's number for it and what the
    /// venue said about it.
    advisor_replaced: Mutex<Vec<(i64, String)>>,
    /// A replacement the venue refused, as the caller's number for it, the
    /// code it is reported under and what the venue said.
    advisor_refused: Mutex<Vec<(i64, i32, String)>>,
    scanner_data: Mutex<Vec<(u32, ScannerResult)>>,
    historical_news: Mutex<Vec<(u32, Vec<NewsHeadline>, bool)>>,
    news_articles: Mutex<Vec<(u32, i32, String)>>,
    fundamental_data: Mutex<Vec<(u32, String)>>,
    histogram_data: Mutex<Vec<(u32, Vec<HistogramEntry>)>>,
    historical_ticks: Mutex<Vec<(u32, HistoricalTickData, String, bool)>>,
    historical_schedules: Mutex<Vec<(u32, HistoricalScheduleResponse)>>,
    /// A contract's corporate actions, against the contract they belong to.
    ///
    /// Keyed by the contract and not by a request, because that is how the
    /// venue sends them: the reply names the contract it is about, and one
    /// arrives per contract rather than per question asked.
    adjustments: Mutex<std::collections::HashMap<String, StatedActions>>,
    /// A slot for each request somebody is waiting on, holding its answer once
    /// one arrives.
    ///
    /// Separate from the record above because that one holds what arrived last
    /// for a contract, and two questions about one contract would otherwise
    /// share a slot — the second answer replacing the first before the first
    /// caller had looked.
    ///
    /// A slot exists only while somebody waits: whoever asks makes one and
    /// gives it up with a guard that runs on every way out of the wait, so an
    /// answer to a request nobody is waiting on is dropped rather than kept.
    /// The guard matters more than it looks — the request can fail to go out at
    /// all, and a slot left by a call that never waited is one nothing
    /// reclaims.
    adjustments_by_request: Mutex<std::collections::HashMap<u32, Option<Vec<crate::control::adjustments::Adjustment>>>>,
    /// Errors surfaced by HMDS for in-flight reference queries (req_id, code, message).
    /// Drained by the dispatcher and forwarded to `Wrapper::error`.
    historical_errors: Mutex<Vec<(u32, i32, String)>>,
    market_rules: Mutex<Vec<MarketRule>>,
    depth_exchanges_cache: Mutex<Vec<DepthMktDataDescription>>,
    depth_exchanges_pending: Mutex<bool>,
    /// Contract cache from CCP exec reports (con_id -> api::Contract).
    contract_cache: Mutex<HashMap<i64, api::Contract>>,
    /// The entries above that are the venue's own definitions, not seeds.
    defined_contracts: Mutex<std::collections::HashSet<i64>>,
    /// Gateway-local init data (populated during connection, read-only after).
    smart_components: Mutex<Vec<crate::types::SmartComponent>>,
    news_providers: Mutex<Vec<crate::types::NewsProvider>>,
    soft_dollar_tiers: Mutex<Vec<crate::types::SoftDollarTier>>,
    family_codes: Mutex<Vec<crate::types::FamilyCode>>,
    white_branding_id: Mutex<String>,
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    ccp_session_id: Mutex<String>,
    /// Logical-name → host URL map pushed by the venue during logon.
    misc_urls: Mutex<HashMap<String, String>>,
    /// Another session already on this account at connect: address, login time,
    /// and whether this one is held to reading only.
    competing_session: Mutex<Option<(String, String, bool)>>,
    /// Why this session ended, once it has ended for good.
    session_over: Mutex<Option<&'static str>>,
    /// Why the trading connection stopped for good, where it has.
    trading_over: Mutex<Option<&'static str>>,
    /// Security type → the order types the venue permits for it, from logon tag 6652.
    order_permissions: Mutex<HashMap<String, Vec<String>>>,
    /// Feature tokens the venue enables for this account, from logon tag 6542
    /// and from the account configuration that follows it.
    enabled_features: Mutex<Vec<String>>,
    /// Whether the venue granted the older spelling of Nasdaq. Settled when
    /// the grants are, because a contract definition is parsed under it and a
    /// lock and a scan per definition is not what that path is for.
    island_granted: AtomicBool,
    /// Which algorithms the venue offers, by provider and security type.
    algorithms: Mutex<HashMap<String, Vec<String>>>,
    /// The order presets the account holds, by the key the venue names each
    /// set under, with the version it is on.
    order_presets: Mutex<Vec<(String, String, String)>>,
    /// What a derivative's underlying is, by the venue's id for the
    /// derivative.
    ///
    /// Stated on the definition, and held because the schedule an option is
    /// priced against belongs to its underlying rather than to the option.
    under_con_ids: Mutex<std::collections::HashMap<u32, u32>>,
    /// What each contract pays out, by the venue's id for it.
    ///
    /// Kept rather than drained: a schedule is a fact about the contract that
    /// every option on it is priced against, and the option that needs it next
    /// is not the one whose question fetched it.
    dividend_schedules: Mutex<std::collections::HashMap<u32, crate::control::dividends::Schedule>>,
    /// What the venue states about the issuer, by its id for the contract and
    /// the series the statement arrived on.
    ///
    /// Kept rather than drained, as the schedules above are: these are facts
    /// about the contract and the caller that reads one is not the arrival
    /// that brought it. Each series is replaced whole when it is restated,
    /// because that is how the venue states it — one message carries the
    /// series' whole set of pairs.
    company_data: Mutex<CompanyData>,
}

impl ReferenceState {
    pub(super) fn new() -> Self {
        Self {
            ours_in_flight: Mutex::new(Default::default()),
            historical_data: Mutex::new(Vec::with_capacity(16)),
            head_timestamps: Mutex::new(Vec::with_capacity(8)),
            smart_components_provisional: AtomicBool::new(false),
            contract_details: Mutex::new(Vec::with_capacity(16)),
            contract_details_end: Mutex::new(Vec::with_capacity(8)),
            matching_symbols: Mutex::new(Vec::with_capacity(8)),
            orders_bound: Mutex::new(Vec::new()),
            orders_bound_said: Mutex::new(std::collections::HashSet::new()),
            calendar_meta_data: Mutex::new(Vec::new()),
            calendar_events: Mutex::new(Vec::new()),
            option_params: Mutex::new(Vec::with_capacity(4)),
            scanner_params: Mutex::new(Vec::new()),
            advisor_config: Mutex::new(Vec::new()),
            advisor_replaced: Mutex::new(Vec::new()),
            advisor_refused: Mutex::new(Vec::new()),
            scanner_data: Mutex::new(Vec::with_capacity(8)),
            historical_news: Mutex::new(Vec::with_capacity(8)),
            news_articles: Mutex::new(Vec::with_capacity(8)),
            fundamental_data: Mutex::new(Vec::with_capacity(4)),
            histogram_data: Mutex::new(Vec::with_capacity(4)),
            historical_ticks: Mutex::new(Vec::with_capacity(4)),
            historical_schedules: Mutex::new(Vec::with_capacity(4)),
            adjustments: Mutex::new(std::collections::HashMap::new()),
            adjustments_by_request: Mutex::new(std::collections::HashMap::new()),
            historical_errors: Mutex::new(Vec::with_capacity(4)),
            market_rules: Mutex::new(Vec::new()),
            depth_exchanges_cache: Mutex::new(Vec::new()),
            depth_exchanges_pending: Mutex::new(false),
            contract_cache: Mutex::new(HashMap::new()),
            defined_contracts: Mutex::new(std::collections::HashSet::new()),
            smart_components: Mutex::new(Vec::new()),
            news_providers: Mutex::new(Vec::new()),
            soft_dollar_tiers: Mutex::new(Vec::new()),
            family_codes: Mutex::new(Vec::new()),
            white_branding_id: Mutex::new(String::new()),
            ccp_session_id: Mutex::new(String::new()),
            misc_urls: Mutex::new(HashMap::new()),
            competing_session: Mutex::new(None),
            session_over: Mutex::new(None),
            trading_over: Mutex::new(None),
            order_permissions: Mutex::new(HashMap::new()),
            enabled_features: Mutex::new(Vec::new()),
            island_granted: AtomicBool::new(false),
            algorithms: Mutex::new(HashMap::new()),
            order_presets: Mutex::new(Vec::new()),
            under_con_ids: Mutex::new(std::collections::HashMap::new()),
            dividend_schedules: Mutex::new(std::collections::HashMap::new()),
            company_data: Mutex::new(CompanyData::new()),
        }
    }

    /// Take every historical data waiting, leaving none.
    pub fn drain_historical_data(&self) -> Vec<(u32, HistoricalResponse)> {
        self.historical_data.lock().unwrap().drain(..).collect()
    }

    /// Take every head timestamps waiting, leaving none.
    pub fn drain_head_timestamps(&self) -> Vec<(u32, HeadTimestampResponse)> {
        self.head_timestamps.lock().unwrap().drain(..).collect()
    }

    /// Take every contract details waiting, leaving none.
    pub fn drain_contract_details(&self) -> Vec<(u32, ContractDefinition)> {
        self.contract_details.lock().unwrap().drain(..).collect()
    }

    /// Whether the smart-component table came from the venue or is this
    /// client's own list.
    ///
    /// The bit numbers in it decide which exchange a quote's bid, ask and last
    /// are attributed to. The venue assigns them; a list written here can only
    /// guess, and a guess that renders confidently is indistinguishable from
    /// knowledge.
    pub fn smart_components_are_provisional(&self) -> bool {
        self.smart_components_provisional.load(Ordering::Relaxed)
    }

    /// Whether the venue map held is this client's guess or
    /// the venue's statement.
    pub fn note_smart_components_provisional(&self, provisional: bool) {
        self.smart_components_provisional
            .store(provisional, Ordering::Relaxed);
    }

    /// The definitions a dispatch loop should deliver, leaving an answering
    /// call's own where that call will find them.
    pub fn drain_contract_details_for_dispatch(&self) -> Vec<(u32, ContractDefinition)> {
        self.drain_dispatchable(&self.contract_details)
    }

    /// Take every historical data a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_historical_data_for_dispatch(&self) -> Vec<(u32, HistoricalResponse)> {
        self.drain_dispatchable(&self.historical_data)
    }

    /// Take every head timestamps a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_head_timestamps_for_dispatch(&self) -> Vec<(u32, HeadTimestampResponse)> {
        self.drain_dispatchable(&self.head_timestamps)
    }

    /// Take every calendar meta data a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_calendar_meta_data_for_dispatch(&self) -> Vec<(u32, String)> {
        self.drain_dispatchable(&self.calendar_meta_data)
    }

    /// Take every calendar events a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_calendar_events_for_dispatch(&self) -> Vec<(u32, String)> {
        self.drain_dispatchable(&self.calendar_events)
    }

    /// Take every matching symbols a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_matching_symbols_for_dispatch(&self) -> Vec<(u32, Vec<SymbolMatch>)> {
        self.drain_dispatchable(&self.matching_symbols)
    }

    /// Take every histogram data a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_histogram_data_for_dispatch(&self) -> Vec<(u32, Vec<HistogramEntry>)> {
        self.drain_dispatchable(&self.histogram_data)
    }

    /// Take every fundamental data a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_fundamental_data_for_dispatch(&self) -> Vec<(u32, String)> {
        self.drain_dispatchable(&self.fundamental_data)
    }

    /// Take every historical schedules a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_historical_schedules_for_dispatch(&self) -> Vec<(u32, HistoricalScheduleResponse)> {
        self.drain_dispatchable(&self.historical_schedules)
    }

    /// Take every contract details end a dispatch loop should deliver, leaving behind
    /// what a waiting answering call will take.
    pub fn drain_contract_details_end_for_dispatch(&self) -> Vec<u32> {
        let mut g = self.contract_details_end.lock().unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while i < g.len() {
            if self.is_ours(RecordKind::Answer, g[i] as i64) {
                i += 1;
            } else {
                out.push(g.remove(i));
            }
        }
        out
    }

    /// What a refusal against no request at all is carried as.
    ///
    /// The queue holds the request id unsigned, and a refusal that answers no
    /// request has none to hold — the reference client states those under -1,
    /// and reporting one under 0 puts it on a caller's own request instead.
    /// Read back by [`request_id_reported`].
    pub const NO_REQUEST: u32 = u32::MAX;

    /// The request id to report an error under, as the caller numbers them.
    pub fn request_id_reported(stored: u32) -> i64 {
        if stored == Self::NO_REQUEST { -1 } else { i64::from(stored) }
    }

    /// Take the refusals a dispatch loop should deliver, leaving behind those a
    /// reader is going to take by id. `mine` says which ids those are, because
    /// the two dispatch loops answer their own questions differently.
    pub fn drain_historical_errors_for_dispatch(
        &self, mine: impl Fn(u32) -> bool,
    ) -> Vec<(u32, i32, String)> {
        let mut g = self.historical_errors.lock().unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while i < g.len() {
            if mine(g[i].0) { i += 1; } else { out.push(g.remove(i)); }
        }
        out
    }

    /// The first id this client's own answering calls ask under.
    ///
    /// A dispatch loop tells these apart from a caller's own requests and
    /// leaves them where the waiting call will find them, so the band has to
    /// sit above every id a caller can present. "Far above what a caller is
    /// likely to use" was not that: a session numbers its orders from the
    /// account's own counter so a restart does not reissue an id the account
    /// has already used, which puts a caller's ids near the epoch in seconds —
    /// past `0x6A00_0000` today and climbing, where the old base was
    /// `0x3000_0000`. Every request a program made through that counter was
    /// read as this client's own and its answer withheld, so the request never
    /// completed and nothing said why.
    ///
    /// Above that counter, and below [`ENGINE_ID_BASE`], where the engine's own
    /// requests (cache auto-fetch, scanner enrichment) start.
    pub const ASK_ID_BASE: u32 = 0xC000_0000;

    /// Whether a request id belongs to one of this client's answering calls.
    ///
    /// The band, not everything above its floor: the engine's own requests are
    /// nobody's answering call, and their replies are taken by the engine
    /// rather than left for a call that is going to want them.
    pub fn is_ask_id(req_id: u32) -> bool {
        (Self::ASK_ID_BASE..ENGINE_ID_BASE).contains(&req_id)
    }

    /// Record that this session is reading records of one kind under this id.
    pub fn note_ours(&self, kind: RecordKind, req_id: i64) {
        self.ours_in_flight.lock().unwrap().insert((kind, req_id));
    }

    /// Stop holding an id, whether the question was answered or given up on.
    pub fn forget_ours(&self, kind: RecordKind, req_id: i64) {
        self.ours_in_flight.lock().unwrap().remove(&(kind, req_id));
    }

    /// Whether records of this kind under this id belong to a reader of this
    /// session's.
    ///
    /// Read from what was recorded when the id was handed out, so a caller's
    /// own number — however large, and whatever counter it came from — is
    /// never mistaken for one of these. Asked one kind at a time, because a
    /// number means one thing per kind: a stream reading bars under 7 says
    /// nothing about the scan a caller is running under 7.
    pub fn is_ours(&self, kind: RecordKind, req_id: i64) -> bool {
        self.ours_in_flight.lock().unwrap().contains(&(kind, req_id))
    }

    /// Whether any reader of this session's holds this id, whatever it holds
    /// it for.
    ///
    /// For the refusals, which share one queue keyed by the number alone: the
    /// calendar, the contract lookups, the trading connection and the scanner
    /// all write to it, so alone among the queues it cannot say which kind of
    /// request failed. Until it can, the most that may be asked of a number
    /// there is whether anybody is reading under it at all.
    pub fn held_under_any_kind(&self, req_id: i64) -> bool {
        let held = self.ours_in_flight.lock().unwrap();
        RecordKind::ALL.iter().any(|kind| held.contains(&(*kind, req_id)))
    }

    /// Whether a refusal under this id belongs to a reader that takes it out
    /// of the queue itself, so a dispatch loop has to leave it there.
    ///
    /// The streams the answering shape hands back read by id and hold no turn,
    /// because a stream outlives any one read of the session. This client's own
    /// answering calls are recorded the same way and are not that: they pump a
    /// dispatch loop themselves and receive through it, so their answers must go
    /// on being delivered to it or they are withheld from the call waiting for
    /// them. The band the second are numbered in is what tells the two apart.
    ///
    /// For the refusals alone. Every other queue names one kind of record and
    /// asks about that kind; see [`held_under_any_kind`](Self::held_under_any_kind).
    pub fn left_for_its_reader(&self, req_id: u32) -> bool {
        !Self::is_ask_id(req_id) && self.held_under_any_kind(i64::from(req_id))
    }

    /// Drain what a dispatch loop should deliver, leaving behind what a waiting
    /// answering call is going to take.
    ///
    /// Only for a dispatch loop whose answering calls take their replies out of
    /// these queues by id. A dispatch loop that *is* how its answering calls
    /// receive must use the plain drain, or it withholds from itself — which is
    /// what happened, and the tests of the day could not see it: they filled
    /// the queues by hand, with ids of their own choosing, so the band was
    /// never the one a session hands out.
    pub fn drain_dispatchable<T>(&self, q: &Mutex<Vec<(u32, T)>>) -> Vec<(u32, T)> {
        let mut g = q.lock().unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while i < g.len() {
            if self.is_ours(RecordKind::Answer, g[i].0 as i64) {
                i += 1;
            } else {
                out.push(g.remove(i));
            }
        }
        out
    }

    /// Take the one answer belonging to a request, leaving the rest.
    fn take_one<T>(q: &Mutex<Vec<(u32, T)>>, req_id: u32) -> Option<T> {
        let mut g = q.lock().unwrap();
        let at = g.iter().position(|(id, _)| *id == req_id)?;
        Some(g.remove(at).1)
    }

    // Withdrawing a request stops the venue sending more; it does not unsend
    // what already arrived and is waiting to be read. Left there, the next
    // request under the same number is answered with the previous one's, and
    // where one of those said it was the last, terminated before its own
    // answer arrives.
    //
    // One per kind, not one for all of them. A request number means one thing
    // per kind of request, so a caller may hold the same number for bars and
    // for a head timestamp at once — and withdrawing either must not take the
    // other's answers with it.

    // The refusals are not purged with them, and cannot be as things stand.
    // They share one queue keyed by request number alone — written by the
    // calendar, the contract lookups, the trading connection and the scanner
    // as well — so throwing away "the refusals under 7" throws away another
    // kind's reason for failing, which is the very thing the paragraph above
    // forbids. A reason left standing is read back as the next request's, and
    // that is the lesser of the two until the queue carries which kind it
    // belongs to.

    /// Throw away bars still queued under a request.
    pub fn purge_historical_for(&self, req_id: u32) {
        self.historical_data.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Throw away a head timestamp still queued under a request.
    pub fn purge_head_timestamp_for(&self, req_id: u32) {
        self.head_timestamps.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Throw away calendar answers still queued under a request.
    pub fn purge_calendar_for(&self, req_id: u32) -> bool {
        let mut meta = self.calendar_meta_data.lock().unwrap();
        let mut events = self.calendar_events.lock().unwrap();
        let before = meta.len() + events.len();
        meta.retain(|(id, _)| *id != req_id);
        events.retain(|(id, _)| *id != req_id);
        meta.len() + events.len() != before
    }

    /// Throw away a report still queued under a request.
    pub fn purge_fundamental_for(&self, req_id: u32) {
        self.fundamental_data.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Throw away headlines still queued under a request.
    pub fn purge_historical_news_for(&self, req_id: u32) {
        self.historical_news.lock().unwrap().retain(|(id, ..)| *id != req_id);
    }

    /// Throw away a histogram still queued under a request.
    pub fn purge_histogram_for(&self, req_id: u32) {
        self.histogram_data.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    /// Bars answering one request. The venue may answer in several parts, so
    /// this takes every part waiting and the caller stops on the one that says
    /// it is the last.
    pub fn take_historical_for(&self, req_id: u32) -> Vec<HistoricalResponse> {
        let mut q = self.historical_data.lock().unwrap();
        let mut mine = Vec::new();
        let mut i = 0;
        while i < q.len() {
            if q[i].0 == req_id { mine.push(q.remove(i).1); } else { i += 1; }
        }
        mine
    }

    /// Take the head timestamp answering one request, leaving the rest.
    pub fn take_head_timestamp_for(&self, req_id: u32) -> Option<HeadTimestampResponse> {
        Self::take_one(&self.head_timestamps, req_id)
    }

    /// Take the matching symbols answering one request, leaving the rest.
    pub fn take_matching_symbols_for(&self, req_id: u32) -> Option<Vec<SymbolMatch>> {
        Self::take_one(&self.matching_symbols, req_id)
    }

    /// The option chains answered for one request.
    pub fn take_option_params_for(&self, req_id: u32) -> Option<(i64, Vec<OptionChainScope>)> {
        let mut held = self.option_params.lock().unwrap();
        let at = held.iter().position(|(id, ..)| *id == req_id)?;
        let (_, underlying, scopes) = held.remove(at);
        Some((underlying, scopes))
    }

    /// Take the histogram answering one request, leaving the rest.
    pub fn take_histogram_for(&self, req_id: u32) -> Option<Vec<HistogramEntry>> {
        Self::take_one(&self.histogram_data, req_id)
    }

    /// Take the fundamental answering one request, leaving the rest.
    pub fn take_fundamental_for(&self, req_id: u32) -> Option<String> {
        Self::take_one(&self.fundamental_data, req_id)
    }

    /// What the venue last stated for a contract, over whatever range was last
    /// asked about.
    ///
    /// Not every action of the contract's life: each question replaces what the
    /// one before it left, and a narrower question leaves a narrower answer.
    /// Read rather than taken, so a caller adjusting one series does not spend
    /// it for the next.
    pub fn adjustments_for(&self, con_id: &str)
        -> Option<(crate::control::adjustments::AdjustedContract, Vec<crate::control::adjustments::Adjustment>)>
    {
        self.adjustments.lock().unwrap().get(con_id).cloned()
    }

    /// Say that an answer to this request is going to be waited for.
    ///
    /// Nothing is filed for a request nobody said they would wait on. Without
    /// that, every answer to every request leaves a slot behind: the requests
    /// sent by the fire-and-forget call are never taken by anyone, and an
    /// answer arriving after its asker gave up recreates the slot it had just
    /// removed. Either grows for as long as the session lasts.
    ///
    /// Paired with [`stop_waiting_for_adjustments`](Self::stop_waiting_for_adjustments),
    /// which every path out of a wait goes through.
    pub fn expect_adjustments(&self, req_id: u32) {
        self.adjustments_by_request.lock().unwrap().insert(req_id, None);
    }

    /// Give up on an answer, whether or not one arrived.
    pub fn stop_waiting_for_adjustments(&self, req_id: u32) {
        self.adjustments_by_request.lock().unwrap().remove(&req_id);
    }

    /// The answer to one request, if it has arrived.
    ///
    /// Kept apart from the contract's own record, which holds whatever arrived
    /// last and is what a caller reads to ask "what does this session know
    /// about this contract". A caller waiting on an answer is asking something
    /// narrower — "what answered the question I asked" — and the two must not
    /// share a slot: an answer filed for one request and then overwritten by a
    /// late answer to another is an answer that arrived and was lost, and the
    /// caller waiting on it is told nothing came.
    pub fn take_adjustments_answering(&self, req_id: u32)
        -> Option<Vec<crate::control::adjustments::Adjustment>>
    {
        let mut waiting = self.adjustments_by_request.lock().unwrap();
        match waiting.get_mut(&req_id) {
            Some(slot @ Some(_)) => slot.take(),
            _ => None,
        }
    }

    /// Forget what a contract's actions were, so the next answer is the next
    /// answer.
    ///
    /// This record is what a caller reads to ask what the session knows about
    /// a contract, and it holds whatever arrived last. Clearing it before
    /// asking is what stops that reader stating a previous question's answer
    /// once a newer one has been asked for.
    ///
    /// It is not what makes a wait its own. A caller waiting on an answer
    /// watches the slot its request made, which no other question can fill.
    pub fn forget_adjustments(&self, con_id: &str) {
        self.adjustments.lock().unwrap().remove(con_id);
    }

    #[doc(hidden)] pub fn note_adjustments(
        &self,
        contract: crate::control::adjustments::AdjustedContract,
        actions: Vec<crate::control::adjustments::Adjustment>,
        answering: u32,
    ) {
        if contract.con_id.is_empty() {
            return;
        }
        // Filed only where somebody said they would wait for it. An answer to
        // a request nobody is waiting on has nowhere to go, and giving it one
        // is how this map would grow for the life of the session.
        if let Some(slot) = self.adjustments_by_request.lock().unwrap().get_mut(&answering) {
            *slot = Some(actions.clone());
        }
        self.adjustments.lock().unwrap()
            .insert(contract.con_id.clone(), (contract, actions));
    }

    /// Take the historical schedule answering one request, leaving the rest.
    pub fn take_historical_schedule_for(&self, req_id: u32) -> Option<HistoricalScheduleResponse> {
        Self::take_one(&self.historical_schedules, req_id)
    }

    /// Take only the definitions answering one request, leaving every other
    /// request's alone.
    ///
    /// The plain drain empties the queue for whoever calls it first. A caller
    /// asking one question needs its own answer without swallowing the answers
    /// belonging to a dispatch loop running beside it.
    pub fn take_contract_details_for(&self, req_id: u32) -> Vec<ContractDefinition> {
        let mut q = self.contract_details.lock().unwrap();
        let mut mine = Vec::new();
        q.retain(|(id, def)| {
            if *id == req_id { mine.push(def.clone()); false } else { true }
        });
        mine
    }

    /// Whether the venue has said it has no more to say about one request.
    pub fn take_contract_details_end_for(&self, req_id: u32) -> bool {
        let mut q = self.contract_details_end.lock().unwrap();
        let before = q.len();
        q.retain(|id| *id != req_id);
        q.len() != before
    }

    pub fn take_error_for(&self, req_id: u32) -> Option<(i32, String)> {
        let mut q = self.historical_errors.lock().unwrap();
        let at = q.iter().position(|(id, _, _)| *id == req_id)?;
        let (_, code, msg) = q.remove(at);
        Some((code, msg))
    }

    /// Take the book reset queued under `req_id`, if one is, and nothing else.
    ///
    /// A stream reads it apart from the refusals so it can be noted before
    /// the level that follows it is handed over, while a refusal still ends
    /// the stream only after the levels that preceded it.
    pub fn take_reset_for(&self, req_id: u32) -> Option<(i32, String)> {
        let mut q = self.historical_errors.lock().unwrap();
        let at = q.iter().position(|(id, code, _)| {
            *id == req_id && *code == crate::error_codes::DEPTH_BOOK_RESET
        })?;
        let (_, code, msg) = q.remove(at);
        Some((code, msg))
    }

    /// Take every contract details end waiting, leaving none.
    pub fn drain_contract_details_end(&self) -> Vec<u32> {
        self.contract_details_end.lock().unwrap().drain(..).collect()
    }

    /// Take every calendar meta data waiting, leaving none.
    pub fn drain_calendar_meta_data(&self) -> Vec<(u32, String)> {
        self.calendar_meta_data.lock().unwrap().drain(..).collect()
    }

    /// Take every calendar events waiting, leaving none.
    pub fn drain_calendar_events(&self) -> Vec<(u32, String)> {
        self.calendar_events.lock().unwrap().drain(..).collect()
    }

    /// Take every matching symbols waiting, leaving none.
    pub fn drain_matching_symbols(&self) -> Vec<(u32, Vec<SymbolMatch>)> {
        self.matching_symbols.lock().unwrap().drain(..).collect()
    }

    /// Take every option params waiting, leaving none.
    pub fn drain_option_params(&self) -> Vec<(u32, i64, Vec<OptionChainScope>)> {
        self.option_params.lock().unwrap().drain(..).collect()
    }

    /// The chains meant for a callback, leaving those a caller is waiting on.
    ///
    /// Draining everything hands an answering call's own answer to the
    /// callback pump instead, and the call then waits out its timeout for
    /// something that has already been delivered somewhere else.
    pub fn drain_option_params_for_dispatch(&self) -> Vec<(u32, i64, Vec<OptionChainScope>)> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.option_params.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(self.is_ours(RecordKind::Answer, e.0 as i64)));
        *held = kept;
        out
    }

    /// Take every scanner params waiting, leaving none.
    pub fn drain_scanner_params(&self) -> Vec<String> {
        self.scanner_params.lock().unwrap().drain(..).collect()
    }

    /// Take every advisor partition waiting, leaving none.
    pub fn drain_advisor_config(&self) -> Vec<(i32, String)> {
        self.advisor_config.lock().unwrap().drain(..).collect()
    }

    /// Take every finished replacement waiting, leaving none.
    pub fn drain_advisor_replaced(&self) -> Vec<(i64, String)> {
        self.advisor_replaced.lock().unwrap().drain(..).collect()
    }

    /// Take every refused replacement waiting, leaving none.
    pub fn drain_advisor_refused(&self) -> Vec<(i64, i32, String)> {
        self.advisor_refused.lock().unwrap().drain(..).collect()
    }

    /// Take every scanner data waiting, leaving none.
    pub fn drain_scanner_data(&self) -> Vec<(u32, ScannerResult)> {
        self.scanner_data.lock().unwrap().drain(..).collect()
    }

    /// Take the scan results a dispatch loop should deliver, leaving behind
    /// those a stream is going to read by id.
    ///
    /// A scan answers repeatedly for as long as it runs, and the stream that
    /// opened it reads its batches out of this queue. Drained whole beside one,
    /// a batch went to a callback and the stream waited for the next.
    pub fn drain_scanner_data_for_dispatch(
        &self, mine: impl Fn(u32) -> bool,
    ) -> Vec<(u32, ScannerResult)> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.scanner_data.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(mine(e.0)));
        *held = kept;
        out
    }

    /// The scan results arrived under one request, leaving the rest.
    pub fn take_scanner_data_for(&self, req_id: u32) -> Vec<ScannerResult> {
        let mut held = self.scanner_data.lock().unwrap();
        let drained: Vec<(u32, ScannerResult)> = held.drain(..).collect();
        let mut taken = Vec::new();
        for (id, result) in drained {
            if id == req_id {
                taken.push(result);
            } else {
                held.push((id, result));
            }
        }
        taken
    }

    /// Take every historical news waiting, leaving none.
    pub fn drain_historical_news(&self) -> Vec<(u32, Vec<NewsHeadline>, bool)> {
        self.historical_news.lock().unwrap().drain(..).collect()
    }

    /// The headlines answering one request, leaving anything a dispatch loop
    /// is going to deliver where it is.
    pub fn take_historical_news_for(&self, req_id: u32) -> Option<(Vec<NewsHeadline>, bool)> {
        let mut held = self.historical_news.lock().unwrap();
        let at = held.iter().position(|(id, ..)| *id == req_id)?;
        let (_, headlines, has_more) = held.remove(at);
        Some((headlines, has_more))
    }

    /// What a dispatch loop should deliver, leaving what an answering call is
    /// waiting to take.
    pub fn drain_historical_news_for_dispatch(&self) -> Vec<(u32, Vec<NewsHeadline>, bool)> {
        // Partitioned rather than removed one at a time: each `remove` shifts
        // the tail, so a pass over a queue that has grown costs the square of
        // it — under the lock the hot loop pushes into, on exactly the path a
        // stalled reader takes when it resumes. The `take_*_for` siblings were
        // already changed for this; these were not.
        let mut held = self.historical_news.lock().unwrap();
        let (out, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *held).into_iter().partition(|e| !(self.is_ours(RecordKind::Answer, e.0 as i64)));
        *held = kept;
        out
    }

    /// Take every news articles waiting, leaving none.
    pub fn drain_news_articles(&self) -> Vec<(u32, i32, String)> {
        self.news_articles.lock().unwrap().drain(..).collect()
    }

    /// Take every fundamental data waiting, leaving none.
    pub fn drain_fundamental_data(&self) -> Vec<(u32, String)> {
        self.fundamental_data.lock().unwrap().drain(..).collect()
    }

    /// Take every histogram data waiting, leaving none.
    pub fn drain_histogram_data(&self) -> Vec<(u32, Vec<HistogramEntry>)> {
        self.histogram_data.lock().unwrap().drain(..).collect()
    }

    /// Take every historical ticks waiting, leaving none.
    pub fn drain_historical_ticks(&self) -> Vec<(u32, HistoricalTickData, String, bool)> {
        self.historical_ticks.lock().unwrap().drain(..).collect()
    }

    /// Take every historical schedules waiting, leaving none.
    pub fn drain_historical_schedules(&self) -> Vec<(u32, HistoricalScheduleResponse)> {
        self.historical_schedules.lock().unwrap().drain(..).collect()
    }

    /// Take every historical errors waiting, leaving none.
    pub fn drain_historical_errors(&self) -> Vec<(u32, i32, String)> {
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

    // ── Hot-loop-side writers ──

    #[doc(hidden)] pub fn push_historical_data(&self, req_id: u32, response: HistoricalResponse) {
        self.historical_data.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_head_timestamp(&self, req_id: u32, response: HeadTimestampResponse) {
        self.head_timestamps.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_contract_details(&self, req_id: u32, def: ContractDefinition) {
        self.contract_details.lock().unwrap().push((req_id, def));
    }

    #[doc(hidden)] pub fn push_contract_details_end(&self, req_id: u32) {
        self.contract_details_end.lock().unwrap().push(req_id);
    }

    #[doc(hidden)] pub fn push_calendar_meta_data(&self, req_id: u32, json: String) {
        self.calendar_meta_data.lock().unwrap().push((req_id, json));
    }

    #[doc(hidden)] pub fn push_calendar_events(&self, req_id: u32, json: String) {
        self.calendar_events.lock().unwrap().push((req_id, json));
    }

    /// Say that an order this session did not place is reachable here.
    ///
    /// The venue replays what the account is working when a session opens, and
    /// this client gives each of those a number of its own. That pairing —
    /// the permanent id the venue keeps and the number a caller uses here — is
    /// what `order_bound` carries, and it was worked out and never said.
    #[doc(hidden)] pub fn push_order_bound(&self, perm_id: i64, client_id: i64, order_id: i64) {
        if perm_id == 0 || order_id == 0 {
            return;
        }
        // Once per pairing, for the life of the session. Checked against what
        // has been said rather than against what is waiting to be said: the
        // queue is emptied every time a caller reads it, so a check against it
        // only covers pairings nobody has read yet — and the venue replays
        // these orders on every reconnect, and on a drop each is marked
        // uncertain and recovered again. A caller counting them would have
        // been counting reconnects.
        if !self.orders_bound_said.lock().unwrap().insert(perm_id) {
            return;
        }
        self.orders_bound.lock().unwrap().push((perm_id, client_id, order_id));
    }

    /// The pairings not yet handed over.
    pub fn drain_orders_bound(&self) -> Vec<(i64, i64, i64)> {
        self.orders_bound.lock().unwrap().drain(..).collect()
    }

    #[doc(hidden)] pub fn push_matching_symbols(&self, req_id: u32, matches: Vec<SymbolMatch>) {
        self.matching_symbols.lock().unwrap().push((req_id, matches));
    }

    #[doc(hidden)] pub fn push_option_params(&self, req_id: u32, underlying_con_id: i64, scopes: Vec<OptionChainScope>) {
        self.option_params.lock().unwrap().push((req_id, underlying_con_id, scopes));
    }

    #[doc(hidden)] pub fn push_advisor_config(&self, fa_data_type: i32, xml: String) {
        self.advisor_config.lock().unwrap().push((fa_data_type, xml));
    }

    #[doc(hidden)] pub fn push_advisor_replaced(&self, req_id: i64, text: String) {
        self.advisor_replaced.lock().unwrap().push((req_id, text));
    }

    #[doc(hidden)] pub fn push_advisor_refused(&self, req_id: i64, code: i32, text: String) {
        self.advisor_refused.lock().unwrap().push((req_id, code, text));
    }

    #[doc(hidden)] pub fn push_scanner_params(&self, xml: String) {
        self.scanner_params.lock().unwrap().push(xml);
    }

    /// Throw away scan rows still queued under a request.
    ///
    /// Separate from the answers above because a request number means one
    /// thing per kind of request: a caller may hold the same number for a scan
    /// and for a set of bars, and withdrawing one must not take the other's.
    pub fn purge_scanner_data_for(&self, req_id: u32) {
        self.scanner_data.lock().unwrap().retain(|(id, _)| *id != req_id);
    }

    #[doc(hidden)] pub fn push_scanner_data(&self, req_id: u32, result: ScannerResult) {
        // A retained stream may never be read or dropped. Shed whole refreshes
        // so every surviving batch still carries all the rows the venue sent.
        super::market_data::push_bounded(
            &self.scanner_data, (req_id, result), STREAM_BACKLOG_LIMIT, "scanner_data",
        );
    }

    #[doc(hidden)] pub fn push_historical_news(&self, req_id: u32, headlines: Vec<NewsHeadline>, has_more: bool) {
        self.historical_news.lock().unwrap().push((req_id, headlines, has_more));
    }

    #[doc(hidden)] pub fn push_news_article(&self, req_id: u32, article_type: i32, article_text: String) {
        self.news_articles.lock().unwrap().push((req_id, article_type, article_text));
    }

    #[doc(hidden)] pub fn push_fundamental_data(&self, req_id: u32, data: String) {
        self.fundamental_data.lock().unwrap().push((req_id, data));
    }

    #[doc(hidden)] pub fn push_histogram_data(&self, req_id: u32, entries: Vec<HistogramEntry>) {
        self.histogram_data.lock().unwrap().push((req_id, entries));
    }

    #[doc(hidden)] pub fn push_historical_ticks(&self, req_id: u32, data: HistoricalTickData, what_to_show: String, done: bool) {
        self.historical_ticks.lock().unwrap().push((req_id, data, what_to_show, done));
    }

    #[doc(hidden)] pub fn push_historical_schedule(&self, req_id: u32, response: HistoricalScheduleResponse) {
        self.historical_schedules.lock().unwrap().push((req_id, response));
    }

    #[doc(hidden)] pub fn push_historical_error(&self, req_id: u32, code: i32, message: String) {
        self.historical_errors.lock().unwrap().push((req_id, code, message));
    }

    #[doc(hidden)] pub fn push_market_rules(&self, rules: Vec<MarketRule>) {
        let mut lock = self.market_rules.lock().unwrap();
        for rule in rules {
            if !lock.iter().any(|r| r.rule_id == rule.rule_id) {
                lock.push(rule);
            }
        }
    }

    /// The depth exchanges an ask is waiting for, once the venue has named
    /// them.
    ///
    /// The venue names the directory once, unprompted, after logon, so an ask
    /// made before it lands stays open until it does: spent on the empty list,
    /// the ask was answered with nothing and the directory answered nobody.
    pub fn drain_depth_exchanges(&self) -> Vec<DepthMktDataDescription> {
        let mut pending = self.depth_exchanges_pending.lock().unwrap();
        if !*pending {
            return Vec::new();
        }
        let cache = self.depth_exchanges_cache.lock().unwrap();
        if cache.is_empty() {
            return Vec::new();
        }
        *pending = false;
        cache.clone()
    }

    /// Every exchange the venue named at logon, as it named them.
    ///
    /// Read rather than drained: a caller reading the list must not empty it.
    pub fn depth_exchanges(&self) -> Vec<DepthMktDataDescription> {
        self.depth_exchanges_cache.lock().unwrap().clone()
    }

    /// The venue states the whole directory in one message, unprompted, every
    /// time the session logs on. Added to what was already held, a reconnect
    /// leaves every exchange in it twice — and the list is cloned out on each
    /// subscribe.
    #[doc(hidden)] pub fn push_depth_exchanges(&self, descs: Vec<DepthMktDataDescription>) {
        *self.depth_exchanges_cache.lock().unwrap() = descs;
    }

    #[doc(hidden)] pub fn notify_depth_exchanges(&self) {
        *self.depth_exchanges_pending.lock().unwrap() = true;
    }

    #[doc(hidden)] pub fn cache_contract(&self, con_id: i64, contract: api::Contract) {
        let mut cache = self.contract_cache.lock().unwrap();
        if let Some(existing) = cache.get_mut(&con_id) {
            // Merged field by field: what the incoming states stands, what it
            // leaves empty is kept. Merged on seven names alone, a definition
            // arriving over an entry a fill had seeded lost its strike, its
            // right, its expiry and its multiplier — which is all that tells
            // two options on one underlying apart.
            let api::Contract {
                symbol, sec_type, exchange, currency, local_symbol, primary_exchange, trading_class,
                last_trade_date_or_contract_month, last_trade_date, strike, right, multiplier,
                sec_id_type, sec_id, description, issuer_id, combo_legs_descrip, ..
            } = contract;
            for (field, stated) in [
                (&mut existing.symbol, symbol), (&mut existing.sec_type, sec_type),
                (&mut existing.exchange, exchange), (&mut existing.currency, currency),
                (&mut existing.local_symbol, local_symbol), (&mut existing.primary_exchange, primary_exchange),
                (&mut existing.trading_class, trading_class),
                (&mut existing.last_trade_date_or_contract_month, last_trade_date_or_contract_month),
                (&mut existing.last_trade_date, last_trade_date), (&mut existing.right, right),
                (&mut existing.multiplier, multiplier), (&mut existing.sec_id_type, sec_id_type),
                (&mut existing.sec_id, sec_id), (&mut existing.description, description),
                (&mut existing.issuer_id, issuer_id), (&mut existing.combo_legs_descrip, combo_legs_descrip),
            ] {
                if !stated.is_empty() { *field = stated; }
            }
            if strike != 0.0 { existing.strike = strike; }
        } else {
            cache.insert(con_id, contract);
        }
    }

    /// Cache what the venue's own definition of a contract states, and mark
    /// the entry as defined. An entry seeded from a fill or an order names the
    /// contract without defining it, and the lookup that would define it was
    /// skipped for the entry being there at all.
    #[doc(hidden)] pub fn cache_definition(&self, con_id: i64, contract: api::Contract) {
        self.cache_contract(con_id, contract);
        self.defined_contracts.lock().unwrap().insert(con_id);
    }

    /// Whether the venue's own definition of this contract has been cached,
    /// as against an entry that merely names it.
    pub fn has_definition(&self, con_id: i64) -> bool {
        self.defined_contracts.lock().unwrap().contains(&con_id)
    }

    // ── Gateway-local init data ──

    /// Which venue each bit of a quote's exchange mask refers to.
    pub fn smart_components(&self) -> Vec<crate::types::SmartComponent> {
        self.smart_components.lock().unwrap().clone()
    }

    /// Every provider this account may read.
    pub fn news_providers(&self) -> Vec<crate::types::NewsProvider> {
        self.news_providers.lock().unwrap().clone()
    }

    /// Every soft dollar tier it may direct commission to.
    pub fn soft_dollar_tiers(&self) -> Vec<crate::types::SoftDollarTier> {
        self.soft_dollar_tiers.lock().unwrap().clone()
    }

    /// Every account family this login belongs to.
    pub fn family_codes(&self) -> Vec<crate::types::FamilyCode> {
        self.family_codes.lock().unwrap().clone()
    }

    /// How the venue brands this login.
    pub fn white_branding_id(&self) -> String {
        self.white_branding_id.lock().unwrap().clone()
    }

    /// Session ID surfaced to webapp REST clients as the `x-ccp-session-id` header.
    /// Empty until gateway logon completes.
    pub fn ccp_session_id(&self) -> String {
        self.ccp_session_id.lock().unwrap().clone()
    }

    /// Logical-name → host URL map pushed by the venue during logon. Empty when
    /// no URL set was pushed; consumers should fall back to a documented literal
    /// (e.g. `api.ibkr.com` for `region_dam`).
    pub fn misc_urls(&self) -> HashMap<String, String> {
        self.misc_urls.lock().unwrap().clone()
    }

    /// Single lookup against the URL map. Returns `None` when missing.
    pub fn misc_url(&self, key: &str) -> Option<String> {
        self.misc_urls.lock().unwrap().get(key).cloned()
    }

    /// Security type → the order types the venue permits for it. Stated by the
    /// venue at logon; empty until logon completes.
    pub fn order_permissions(&self) -> HashMap<String, Vec<String>> {
        self.order_permissions.lock().unwrap().clone()
    }

    /// The order types permitted for one security type, or `None` when the venue
    /// does not permit the type at all. A combination is named `COMB`.
    pub fn permitted_order_types(&self, sec_type: &str) -> Option<Vec<String>> {
        let key = if matches!(sec_type, "BAG" | "COMBO") { "COMB" } else { sec_type };
        self.order_permissions.lock().unwrap().get(key).cloned()
    }

    /// Feature tokens the venue enables for this account.
    pub fn enabled_features(&self) -> Vec<String> {
        self.enabled_features.lock().unwrap().clone()
    }

    /// Which algorithms the venue offers, keyed `PROVIDER/SECTYPE`.
    ///
    /// The venue states this on the session; it is not a property of a
    /// contract. An algorithm absent here is one this account may not use.
    pub fn algorithms(&self) -> HashMap<String, Vec<String>> {
        self.algorithms.lock().unwrap().clone()
    }

    /// The algorithms offered for one security type, across every provider.
    pub fn algorithms_for(&self, sec_type: &str) -> Vec<String> {
        let want = format!("/{}", sec_type.to_ascii_uppercase());
        let mut out: Vec<String> = self.algorithms.lock().unwrap()
            .iter()
            .filter(|(k, _)| k.to_ascii_uppercase().ends_with(&want))
            .flat_map(|(_, v)| v.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The order presets the account holds, as `(key, version)`.
    ///
    /// The venue keeps a set of order defaults per security type and fills
    /// parts of an order the caller left unstated from them — the size a
    /// compete order competes with and the offset it competes by, where the
    /// caller named neither. So two identical calls on two accounts are not
    /// the same order, and nothing in the reference client's surface says so.
    ///
    /// The key is the venue's own, `s=STK` or `s=CASH&tc=EUR`. The version is
    /// what that set is on; the values behind it are asked for separately and
    /// are not carried here.
    pub fn order_presets(&self) -> Vec<(String, String, String)> {
        self.order_presets.lock().unwrap().clone()
    }

    #[doc(hidden)] pub fn set_order_presets(&self, presets: Vec<(String, String, String)>) {
        *self.order_presets.lock().unwrap() = presets;
    }

    /// What this derivative is written on, where the venue has said.
    pub fn under_con_id(&self, con_id: u32) -> Option<u32> {
        self.under_con_ids.lock().unwrap().get(&con_id).copied()
    }

    #[doc(hidden)] pub fn note_under_con_id(&self, con_id: u32, under_con_id: u32) {
        if con_id != 0 && under_con_id != 0 {
            self.under_con_ids.lock().unwrap().insert(con_id, under_con_id);
        }
    }

    /// What a contract pays out over the life of an option on it, where the
    /// venue has stated it for this contract.
    pub fn dividend_schedule(
        &self, con_id: u32,
    ) -> Option<crate::control::dividends::Schedule> {
        self.dividend_schedules.lock().unwrap().get(&con_id).cloned()
    }

    #[doc(hidden)] pub fn set_dividend_schedule(
        &self, con_id: u32, schedule: crate::control::dividends::Schedule,
    ) {
        self.dividend_schedules.lock().unwrap().insert(con_id, schedule);
    }

    /// What the venue states about a contract's company or its terms on one
    /// series, as the pairs it wrote.
    ///
    /// Seventeen series carry it, each asked for by the venue's own number for
    /// it on the market data request:
    ///
    /// | Series | What it states |
    /// | --- | --- |
    /// | 386 | What the company has coming, and when |
    /// | 434, 548 | The two analyst ratings |
    /// | 454 | What institutions and insiders hold, and the shares on issue |
    /// | 505 | What a fund will take part in |
    /// | 628, 633 | A fund family's figures, and the same by financial year |
    /// | 631 | The technical readings taken on the contract |
    /// | 669 | The ratios the venue keeps a history of |
    /// | 678, 699 | Two scores worked out from what is written about the company |
    /// | 700 | How the company scores against the principles an account can screen on |
    /// | 703 | What margin dealing in the contract takes |
    /// | 705 | The same screening principles, as the venue's own fields |
    /// | 726 | The lens the venue publishes over the company's accounts |
    /// | 750 | The price the venue holds the contract against for reference |
    /// | 752 | Whether the contract passes a religious screen |
    ///
    /// Empty until one of them has been asked for and answered, and empty for
    /// a contract whose subscriptions do not cover it — the venue answers a
    /// series the account cannot see with silence rather than with a refusal.
    ///
    /// The keys are the venue's own, unchanged. They are the venue's to add to
    /// and to rename, so nothing here reads them or promises a set of them.
    ///
    /// What one contract states is kept for as long as the session runs, and
    /// is not dropped when the subscription that fetched it ends — it is a fact
    /// about the contract rather than about the watch. That costs memory in
    /// proportion to the contracts a session has watched, not to the ones it is
    /// watching now: measured against one ordinary share stating every series,
    /// about twenty-three kilobytes for it, so a session that has watched ten
    /// thousand contracts holds a couple of hundred megabytes of this. A
    /// program watching a few hundred will not notice; one sweeping a scanner
    /// across thousands should know where the memory went.
    pub fn company_data(&self, con_id: u32, series: u32) -> Vec<(String, String)> {
        self.company_data.lock().unwrap().get(&(con_id, series)).cloned().unwrap_or_default()
    }

    /// Which of those series have been stated for a contract, in order.
    pub fn company_data_series(&self, con_id: u32) -> Vec<u32> {
        let mut series: Vec<u32> = self.company_data.lock().unwrap()
            .keys()
            .filter(|(held, _)| *held == con_id)
            .map(|(_, series)| *series)
            .collect();
        series.sort_unstable();
        series
    }

    #[doc(hidden)] pub fn note_company_data(
        &self, con_id: u32, series: u32, pairs: Vec<(String, String)>,
    ) {
        if con_id == 0 {
            return;
        }
        self.company_data.lock().unwrap().insert((con_id, series), pairs);
    }

    #[doc(hidden)] pub fn set_algorithms(&self, algorithms: HashMap<String, Vec<String>>) {
        *self.algorithms.lock().unwrap() = algorithms;
    }

    /// Add feature tokens the venue states after logon. What logon already
    /// stated is kept; this only ever adds.
    #[doc(hidden)] pub fn add_enabled_features(&self, more: Vec<String>) {
        let mut have = self.enabled_features.lock().unwrap();
        for token in more {
            if !have.contains(&token) {
                have.push(token);
            }
        }
        self.settle_island_grant(&have);
    }

    /// The token that grants the older spelling of Nasdaq, as the venue
    /// reads it: off the granted list at logon, held on its own from then on.
    fn settle_island_grant(&self, granted: &[String]) {
        self.island_granted.store(
            granted.iter().any(|t| t == ISLAND_FOR_NASDAQ_GRANT),
            Ordering::Relaxed,
        );
    }

    /// Whether the venue grants the older spelling of Nasdaq to this account.
    pub fn island_granted(&self) -> bool {
        self.island_granted.load(Ordering::Relaxed)
    }

    #[doc(hidden)] pub fn set_order_permissions(&self, perms: HashMap<String, Vec<String>>) {
        *self.order_permissions.lock().unwrap() = perms;
    }

    #[doc(hidden)] pub fn set_enabled_features(&self, features: Vec<String>) {
        self.settle_island_grant(&features);
        *self.enabled_features.lock().unwrap() = features;
    }

    #[doc(hidden)] pub fn set_smart_components(&self, components: Vec<crate::types::SmartComponent>) {
        *self.smart_components.lock().unwrap() = components;
    }

    #[doc(hidden)] pub fn set_news_providers(&self, providers: Vec<crate::types::NewsProvider>) {
        *self.news_providers.lock().unwrap() = providers;
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

    #[doc(hidden)] pub fn set_ccp_session_id(&self, id: String) {
        *self.ccp_session_id.lock().unwrap() = id;
    }

    /// Another session that already held this account when this one connected,
    /// as the venue named it: where it connected from, when it logged in, and
    /// whether this session may look but not trade.
    ///
    /// `None` when this session is alone. The venue permits one logon at a time
    /// and takes the account from the older session without saying which it
    /// dropped, so a caller that wants to know before it starts work asks here.
    pub fn competing_session(&self) -> Option<(String, String, bool)> {
        self.competing_session.lock().unwrap().clone()
    }

    /// Why this session ended, if it has. `Some` means no request can be
    /// answered any more: the transports are down and nothing is trying to
    /// bring them back, so a caller that keeps asking is waiting out a timeout
    /// per call for an answer that cannot arrive.
    pub fn session_over(&self) -> Option<&'static str> {
        *self.session_over.lock().unwrap()
    }

    /// Why the trading connection is gone for good, where it is.
    ///
    /// Its own, apart from the session's: one flag for every transport says a
    /// quote feed the venue will not serve again has ended the trading
    /// connection too, and an order refused on that reading is an order the
    /// venue would have taken. This is set only where the trading connection
    /// itself has stopped being retried.
    pub fn trading_over(&self) -> Option<&'static str> {
        *self.trading_over.lock().unwrap()
    }

    /// Record why the trading connection stopped for good. The first stands.
    #[doc(hidden)] pub fn set_trading_over(&self, why: &'static str) {
        let mut over = self.trading_over.lock().unwrap();
        if over.is_none() {
            *over = Some(why);
        }
    }

    #[doc(hidden)] pub fn clear_trading_over(&self) {
        *self.trading_over.lock().unwrap() = None;
    }

    /// Record why this session ended. The first reason stands.
    ///
    /// A session ends once, and what ended it is the first thing that did. The
    /// tidying that follows is not a second reason: a session taken away by a
    /// login elsewhere, or refused by the venue, is shut down afterwards like
    /// any other. A shutdown overwriting the reason reports "the caller asked
    /// to stop" for a session the caller did not stop, and discards the reason
    /// there was something to say about.
    #[doc(hidden)] pub fn set_session_over(&self, why: &'static str) {
        let mut over = self.session_over.lock().unwrap();
        if over.is_none() {
            *over = Some(why);
        }
    }

    #[doc(hidden)] pub fn clear_session_over(&self) {
        *self.session_over.lock().unwrap() = None;
    }

    #[doc(hidden)] pub fn set_competing_session(&self, other: Option<(String, String, bool)>) {
        *self.competing_session.lock().unwrap() = other;
    }

    #[doc(hidden)] pub fn set_misc_urls(&self, urls: HashMap<String, String>) {
        *self.misc_urls.lock().unwrap() = urls;
    }
}

/// The first id the engine asks its own questions under — a cold-cache
/// auto-fetch, a scanner enrichment. Nothing a caller or an answering call
/// issues reaches this far.
pub const ENGINE_ID_BASE: u32 = 0xF000_0000;

/// The band stays clear of the engine's own requests. Ordered by construction,
/// so a base moved into them fails the build rather than a test that might not
/// be run.
const _: () = assert!(ReferenceState::ASK_ID_BASE < ENGINE_ID_BASE);

#[cfg(test)]
mod ask_id_band {
    use super::{RecordKind, ReferenceState};

    /// A caller's id is not one of this client's own.
    ///
    /// A session numbers its requests from the account's order counter, which
    /// is seeded near the epoch in seconds. Those ids sat inside the old band,
    /// so every answer to them was held back for a waiting internal call that
    /// did not exist, and the request hung with nothing reported. The failure
    /// was invisible offline because the queues were filled by hand.
    #[test]
    fn an_id_seeded_from_the_account_counter_is_the_callers() {
        // What a session hands out today, and will for decades.
        for seeded in [1_786_766_504_u32, 1_900_000_000, 2_500_000_000] {
            assert!(
                !ReferenceState::is_ask_id(seeded),
                "{seeded} is a caller's id, so its answer must be delivered",
            );
        }
        // The floor itself, and the id just under it, which is still theirs.
        assert!(ReferenceState::is_ask_id(ReferenceState::ASK_ID_BASE));
        assert!(!ReferenceState::is_ask_id(ReferenceState::ASK_ID_BASE - 1));
        // And the ceiling: the engine's own questions are not answering calls,
        // so their replies are not withheld for one.
        assert!(!ReferenceState::is_ask_id(super::ENGINE_ID_BASE));
        assert!(ReferenceState::is_ask_id(super::ENGINE_ID_BASE - 1));
    }

    /// And such an answer actually leaves the queue a dispatch loop drains.
    #[test]
    fn the_dispatch_loop_delivers_it() {
        let state = ReferenceState::new();
        // Recorded, because that is what makes it this session's own now. Read
        // off its size, the test passed only when some other test in the same
        // run had happened to record it — and failed on its own.
        state.note_ours(RecordKind::Answer, ReferenceState::ASK_ID_BASE as i64);
        let q = std::sync::Mutex::new(vec![(1_786_766_504_u32, "theirs"),
                                           (ReferenceState::ASK_ID_BASE, "ours")]);
        let out = state.drain_dispatchable(&q);
        assert_eq!(out.len(), 1, "the caller's answer is delivered");
        assert_eq!(out[0].1, "theirs");
        assert_eq!(q.lock().unwrap().len(), 1, "and ours is left for the waiting call");
    }

    /// Two sessions in one process do not hold each other's ids.
    ///
    /// Both count their own questions from the same number, so a record shared
    /// between them would let one release what the other is waiting on, and an
    /// answer meant for a waiting call would be handed to a dispatch loop that
    /// never asked.
    #[test]
    fn one_session_does_not_release_what_another_is_waiting_on() {
        let mine = ReferenceState::new();
        let theirs = ReferenceState::new();
        let id = ReferenceState::ASK_ID_BASE as i64;

        mine.note_ours(RecordKind::Answer, id);
        theirs.note_ours(RecordKind::Answer, id);
        theirs.forget_ours(RecordKind::Answer, id);

        assert!(
            mine.is_ours(RecordKind::Answer, id),
            "another session's release took mine with it",
        );
        assert!(
            !theirs.is_ours(RecordKind::Answer, id),
            "and its own release still took effect",
        );
    }

    /// Dispatch leaves a retained scan's batches for its reader, even when
    /// that reader never advances. The oldest refreshes leave whole.
    #[test]
    fn an_unread_scan_keeps_a_bounded_backlog_of_whole_batches() {
        let state = ReferenceState::new();
        let limit = super::STREAM_BACKLOG_LIMIT;
        state.note_ours(RecordKind::Scanner, 7);
        for batch in 0..=limit {
            let result = crate::control::scanner::parse_scanner_response(&format!(
                "<ScanResponse><scanTime>20260908 14:30:00</scanTime>\
                 <Contract><contractID>{}</contractID></Contract>\
                 <Contract><contractID>{}</contractID></Contract></ScanResponse>",
                batch + 1, batch + 2,
            )).expect("a complete refresh");
            state.push_scanner_data(7, result);
        }
        assert!(state.drain_scanner_data_for_dispatch(
            |id| state.is_ours(RecordKind::Scanner, id as i64),
        ).is_empty(), "the stream's batches stay queued");

        let batches = state.take_scanner_data_for(7);
        let shed = limit / 10;
        assert_eq!(batches.len(), limit - shed + 1, "unread refreshes stay bounded");
        for (offset, batch) in batches.iter().enumerate() {
            let first = (shed + offset + 1) as u32;
            assert_eq!(batch.con_ids, vec![first, first + 1]);
            assert_eq!(
                batch.entries.iter().map(|entry| entry.con_id).collect::<Vec<_>>(),
                batch.con_ids,
                "each surviving refresh keeps every row in order",
            );
            assert_eq!(batch.scan_time, "20260908 14:30:00");
            assert!(batch.error_text.is_empty());
        }
        assert!(state.take_scanner_data_for(7).is_empty());
    }
}

#[cfg(test)]
mod adjustments_store_tests {
    use super::*;

    /// The actions a session was answered with are held against their contract.
    ///
    /// Read rather than taken, because a caller adjusting one series must not
    /// spend them for the next: two questions about the same contract are
    /// answered from the one reply the venue sent for it.
    #[test]
    fn the_actions_stay_against_the_contract_they_name() {
        let state = ReferenceState::new();
        let answered = "conc\n756733,-1,-1\nconexch\n756733,AMEX,20090223\nCD\n\
20240315,1.594937,USD,20240314,20240318,20240430,R,NA\n";
        let (contract, actions) = crate::control::adjustments::parse_adjustments(answered);
        state.note_adjustments(contract, actions, 1);

        let (held, acts) = state.adjustments_for("756733").expect("held against its contract");
        assert_eq!(held.exchange, "AMEX");
        assert_eq!(acts.len(), 1);
        assert!(state.adjustments_for("756733").is_some(), "and still held after reading");
        assert!(state.adjustments_for("999").is_none(), "a contract with none says so");
    }


    /// An answer about a contract is cleared before that contract is asked again.
    ///
    /// The record is kept against the contract, so a second question over a
    /// different range finds the first question's answer waiting. Whoever waits
    /// on the record must clear it first, or they are handed an answer to a
    /// question they did not ask — and over a narrower range that is a series
    /// adjusted by fewer actions than moved it.
    #[test]
    fn an_old_answer_is_cleared_before_the_same_contract_is_asked_again() {
        use crate::control::adjustments::{AdjustedContract, Adjustment, AdjustmentKind};
        let state = ReferenceState::new();
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            split, 1,
        );
        assert!(state.adjustments_for("4815747").is_some(), "the answer is held");

        state.forget_adjustments("4815747");
        assert!(
            state.adjustments_for("4815747").is_none(),
            "cleared, so the next look can only find the next answer",
        );
        // Another contract's answer is untouched by it.
        state.note_adjustments(
            AdjustedContract { con_id: "756733".into(), ..Default::default() },
            Vec::new(), 1,
        );
        state.forget_adjustments("4815747");
        assert!(state.adjustments_for("756733").is_some(), "one contract at a time");
    }


    /// A late answer to a question already given up on is not the next one's.
    ///
    /// Clearing the record before asking is not enough on its own. A caller
    /// that waited and gave up leaves its question outstanding, and the answer
    /// can still arrive afterwards and be filed — sitting there for the next
    /// question about the same contract to pick up, over a range it never asked
    /// about. What each answer belongs to is kept beside it, so the next caller
    /// can see that this one is not theirs.
    #[test]
    fn a_late_answer_belongs_to_the_question_that_asked_it() {
        use crate::control::adjustments::{AdjustedContract, Adjustment, AdjustmentKind};
        let state = ReferenceState::new();
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        // The first question is given up on; its answer arrives anyway.
        state.expect_adjustments(7);
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            split, 7,
        );
        // The second question, over some other range, must not take it.
        assert!(
            state.take_adjustments_answering(8).is_none(),
            "an answer to request 7 is not the answer to request 8",
        );
        assert!(
            state.take_adjustments_answering(7).is_some(),
            "and it is still the answer to the one that did ask it",
        );
        assert!(
            state.take_adjustments_answering(7).is_none(),
            "taken, so it is not there to be taken twice",
        );

        // An answer to one question does not displace an answer to another
        // that is still waiting to be read.
        state.expect_adjustments(9);
        state.expect_adjustments(10);
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            Vec::new(), 9,
        );
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            Vec::new(), 10,
        );
        assert!(
            state.take_adjustments_answering(9).is_some(),
            "the first answer survives the second arriving before anyone read it",
        );

        // An answer nobody said they would wait for is dropped, so a session
        // that keeps asking without waiting does not keep growing.
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            Vec::new(), 11,
        );
        assert!(
            state.take_adjustments_answering(11).is_none(),
            "nobody waited on request 11, so its answer had nowhere to go",
        );
        // A slot made and then given up before any answer arrives leaves
        // nothing behind — which is the case a caller whose request failed to
        // go out at all lands in, and the reason giving up is a guard rather
        // than a line at the end of the happy path.
        state.expect_adjustments(12);
        state.stop_waiting_for_adjustments(12);
        state.note_adjustments(
            AdjustedContract { con_id: "4815747".into(), ..Default::default() },
            Vec::new(), 12,
        );
        assert!(
            state.take_adjustments_answering(12).is_none(),
            "the wait was given up, so the late answer is dropped rather than kept",
        );
        // The plain reader is unchanged: it states what is known about the
        // contract, which is a different question from whose answer it is.
        assert!(state.adjustments_for("4815747").is_some());
    }
}
