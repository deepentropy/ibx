use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// How long a matching-symbols request is held here before it is given up on.
///
/// Shorter than the caller's own wait, which is the rule every deadline the
/// engine keeps has to follow and the reason `NAMING_TIMEOUT` below is written
/// the same way. Held longer, as these two were, the caller gives up first and
/// is told nothing arrived; the reply then arrives, is matched to the entry the
/// caller abandoned, and is delivered under a number it never issued — while
/// the retry it made in the meantime waits behind that entry and is given up on
/// in its turn. A single timeout left the request unanswerable for as long as
/// the difference lasted.
const MATCHING_SYMBOLS_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS - 3);

/// The same, for an option chain: a reply carrying every class of an
/// underlying at once, and the same caller's wait behind it.
const OPTION_CHAIN_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS - 3);

/// The same, for a question or a replacement put to the advisor
/// configuration.
///
/// It had none: a request the venue took and never answered was given up on
/// only when the connection next went, which on a session that stays up is
/// never. A caller reading a partition waited for as long as the process ran,
/// and one replacing a partition was never told whether its document had been
/// taken.
const ADVISOR_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS - 3);

/// Whether an answer being handed over is the whole of it.
///
/// The window can outlive the caller's patience: it is shut by the wire and
/// the caller is released by a clock, so an answer may be handed over while
/// the reports that finish it are still coming. Handed over and then dropped,
/// the terminal report of an order already given to the caller would start a
/// fresh record with none of its fields.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Handover {
    /// Everything held, kept so later reports still merge onto it.
    SoFar,
    /// Everything held, and there is no more coming.
    Final,
}

/// How many finished orders are assembled before the answer is handed over
/// whether or not the venue has said it is done.
///
/// A window the venue never ends stays open, which is right — but it must not
/// mean an unbounded number of orders held in the engine for the rest of a
/// connection that never drops.
const FINISHED_ORDERS_HELD: usize = 4_096;

/// One order the venue has finished, as the reports about it are merged.
///
/// Its status and what it filled are the latest any report stated; every other
/// field is the last report that stated it, because a report states what
/// changed and says nothing about the rest.
#[derive(Clone)]
pub(crate) struct FinishedOrder {
    /// The id every report about this order names it by.
    pub(crate) order_id: u64,
    /// The contract, as far as the reports have stated it.
    pub(crate) contract: crate::types::model::Contract,
    /// The order itself, the same way.
    pub(crate) order: crate::types::model::Order,
    /// What became of it, from the latest report that said.
    pub(crate) status: crate::types::OrderStatus,
    /// And how much of it filled.
    pub(crate) filled: i64,
    /// What became of it, as the venue stated it: the time it finished and the
    /// reason it was refused, both of which only the report carries.
    pub(crate) state: crate::types::model::OrderState,
    /// The order type as the wire states it, kept because four kinds travel
    /// under one letter and the instruction below is what tells them apart.
    pub(crate) ord_type: String,
    /// That instruction, kept for the same reason.
    pub(crate) exec_inst: String,
}

/// How many given-up-on dividend queries are remembered, so a late answer can
/// still be attributed. A second chance for the few most recent, not a record.
const GIVEN_UP_ON_DIVIDEND_QUERIES: usize = 64;

/// The same again, for the question of what the venue has finished — both
/// halves of it. A question held for the session's own replay to end waits
/// this long for that and then goes out anyway, because the replay of an
/// account with nothing working ends without naming an order and there is
/// nothing else to wait for. And a window that has been opened is shut after
/// this long whether or not the sentinel that ends it ever comes, because the
/// caller waiting on that sentinel is the one thing holding the answer.
const COMPLETED_ORDERS_TIMEOUT: Duration =
    Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS - 6);

/// How long the question waits for the replay of what the account is working.
///
/// The wait every other reader of that replay keeps, and for the same reason:
/// an account with nothing working never names an order, so the replay ends
/// without saying so and this is what says it has had long enough. Held for as
/// long as the *window* instead, one question spent twelve seconds waiting and
/// the window behind it another twelve, against a caller that waits fifteen —
/// so the caller was handed an empty answer, or paid twelve seconds for one the
/// venue gives at once, on every call and on exactly the accounts most likely
/// to ask.
const COMPLETED_ORDERS_HOLD: Duration = crate::bridge::REPLAY_WAIT;

/// Neither may outlive the wait the caller keeps. Stated here so a change to
/// either constant, or to the caller's wait, stops the build rather than
/// quietly making a whole request unanswerable again.
const _: () = assert!(
    MATCHING_SYMBOLS_TIMEOUT.as_secs() < crate::config::ANSWER_TIMEOUT_SECS
        && OPTION_CHAIN_TIMEOUT.as_secs() < crate::config::ANSWER_TIMEOUT_SECS
        && COMPLETED_ORDERS_TIMEOUT.as_secs() < crate::config::ANSWER_TIMEOUT_SECS
        // And the two that run one behind the other, as their sum: the
        // question is held for the replay and then its window is opened, and
        // a caller waiting on the end of the second gave up during it.
        && COMPLETED_ORDERS_HOLD.as_secs() + COMPLETED_ORDERS_TIMEOUT.as_secs()
            < crate::config::ANSWER_TIMEOUT_SECS,
    "an engine deadline must be shorter than the wait the caller keeps",
);

pub(crate) use crate::config::OPENING_ACCOUNT_REQUEST;
use crate::bridge::{Event, SharedState};
use crate::engine::context::Context;
use crate::protocol::datetime::chrono_free_timestamp;
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::{
    InstrumentId, NewsBulletin,
    PositionInfo,
};

use super::{HeartbeatState, emit, clone_for_event, parse_price_tag, decode_tif, EventSink};

/// How long a contract-details request may go unanswered before it is ended.
///
/// A request always finishes, because something is waiting on it: ended here,
/// the caller gets error 200 and the end of the request rather than a wait
/// with nothing at the end of it.
///
/// Refreshed whenever the venue speaks on the request — a definition, or
/// fan-out activity — so it measures silence rather than the length of an
/// answer. A class naming every expiry sends for as long as that takes.
///
/// This client's own number, not one the venue states. Held below the wait of
/// whatever is covering the request: a lookup this session took to name a
/// contract is covered by [`CcpState::NAMING_TIMEOUT`], which reports in its
/// place and must not report first.
const SECDEF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What a caller is told when a replacement of the advisor's configuration
/// stands, and the code a refused one is reported under.
///
/// Both are the reference client's own, in its own words: the venue states no
/// text at all on a replacement that stands, and a program written against
/// that client reads these.
const ADVISOR_SAVED: &str = "FA data saved";
const ADVISOR_SAVE_REFUSED: i32 = 10229;

/// The same, for a lookup a caller asked for.
///
/// A caller can name a whole class, which the venue takes about ten seconds to
/// begin answering, so this is twice that. Nothing covers such a lookup: the
/// caller waits on it directly, under a wait of its own that this is held
/// below.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long this request may go unanswered, by who is waiting on it.
///
/// A request numbered in the internal band is one this session took for
/// itself, and the fallback that covers it reports sooner than this. One a
/// caller asked for has nothing covering it and can name a whole class.
fn unanswered_after(req_id: u32) -> std::time::Duration {
    if req_id >= crate::bridge::ENGINE_ID_BASE { SECDEF_TIMEOUT } else { LOOKUP_TIMEOUT }
}

/// Number of most-recent ExecIDs retained for fill deduplication. Bounds the
/// memory of `seen_exec_ids` while staying large enough that a server replay
/// after a reconnect burst still hits the window.
const EXEC_ID_WINDOW: usize = 1024;

/// How many of the venue's own names for recovered orders are held at once.
///
/// One is learned per order the venue replays, and a caller may ask for what
/// the account has finished as often as it likes, so this is a window rather
/// than a record of everything: the oldest goes when a new one arrives. The
/// names serve a withdrawal or a replacement of an order that is still
/// working, and an order that finished long ago needs none.
const WIRE_NAME_WINDOW: usize = 4_096;

/// Convert a FIX OrderID hex string (e.g. "00cf16ed.000225ed.69ca0941.0001") to a
/// stable i64 permId.
/// Uses FNV-1a hash of the first 3 dot-segments (the stable prefix) so that permId
/// remains constant across modifications (the last segment increments on each modify).
/// Extract the value of a single FIX tag from a raw message.
/// `prefix` should include the tag number and `=` (e.g. `b"6256="`).
fn extract_tag_value(msg: &[u8], prefix: &[u8]) -> Option<String> {
    use crate::protocol::fix::SOH;
    for part in msg.split(|&b| b == SOH) {
        if part.starts_with(prefix) {
            return Some(String::from_utf8_lossy(&part[prefix.len()..]).into_owned());
        }
    }
    None
}

/// What a fill cost, as the venue states it.
///
/// The execution report carries no commission tag — captured against real
/// fills on two instruments, it simply is not there — so a charge taken from
/// the report is always nothing. The venue states it on a record of its own
/// that follows the report, naming the execution it belongs to, the amount
/// and the currency it is charged in.
///
/// The quantities and the price on this record are the same fill the
/// execution report already carried, and are left alone: booking the fill
/// from both is how it would be counted twice. Only what it cost is taken.
fn handle_trade_charge(parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
    let Some(exec_id) = parsed.get(&fix::TAG_EXEC_ID).filter(|s| !s.is_empty()) else {
        return;
    };
    // Absent is not nothing: a charge the venue did not state is unstated,
    // and reporting a zero for it is the number this was written to stop.
    let Some(charged) = parsed.get(&fix::TAG_TRADE_CHARGE)
        .and_then(|s| s.parse::<f64>().ok())
    else {
        return;
    };
    shared.orders.push_charge(crate::types::model::CommissionAndFeesReport::charged(
        exec_id,
        charged,
        parsed.get(&fix::TAG_TRADE_CHARGE_CURRENCY).map(String::as_str).unwrap_or(""),
    ));
}

/// What the venue says went wrong.
///
/// It states the trouble as text and gives it no code and no severity, and for
/// all but a narrow family of requests it does not say which request failed.
/// So this reports the text against no request, which is what it is, rather
/// than guessing at an owner for it.
fn handle_venue_error(parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
    let text = parsed.get(&58).map(String::as_str).unwrap_or("");
    if text.is_empty() {
        // The venue reported trouble and stated nothing about it. There is
        // nothing to hand a caller, so this is recorded and not forwarded.
        log::warn!("The venue reported trouble and stated nothing about it");
        return;
    }
    // A code-like identifier travels separately from the text where it travels
    // at all, so it is carried along rather than parsed into a number the
    // venue never stated.
    let told = match parsed.get(&149).map(String::as_str).filter(|id| !id.is_empty()) {
        Some(id) => format!("{text} ({id})"),
        None => text.to_string(),
    };
    // And the account it is about, where the venue named one. A login holding
    // several is told about each of them on this channel, and a margin notice
    // or a corporate-action notice that does not say which account it concerns
    // is close to useless to the person it was written for. The venue said
    // which one; this dropped it.
    let told = match parsed.get(&1).map(String::as_str).filter(|a| !a.is_empty()) {
        Some(account) => format!("{account}: {told}"),
        None => told,
    };
    log::warn!("The venue reported: {told}");
    shared.market.push_venue_error(told);
}

/// The venue restating a working order's terms.
///
/// It names the order and states only what it has changed — the venue it is
/// working on, its limit, or both — so what it does not state is what the
/// order already held. The revised order goes back to the caller the way any
/// other change to it does.
impl CcpState {
fn handle_order_revision(
    &self,
    parsed: &std::collections::HashMap<u32, String>,
    context: &Context,
    shared: &SharedState,
) {
    // Named the way every other report names an order: a cancel carries a
    // leading C and a liquidation a leading L, a revision chain carries a
    // suffix, and a recovered order is named by the venue's own permanent
    // name rather than by the number a caller addresses it under. Parsed as a
    // plain number instead, a revision for a recovered order — which is every
    // order the account already had — either resolved to nothing and was
    // dropped, or resolved to an unrelated live order that happened to be
    // numbered the venue's permanent name for this one, and wrote this order's
    // venue and limit onto that one.
    let Some(named) = parsed.get(&11) else { return };
    let stripped = named.strip_prefix('C').or_else(|| named.strip_prefix('L')).unwrap_or(named);
    let base = stripped.split('.').next().unwrap_or(stripped);
    let Some(named) = executions::stated_order_id(base) else {
        return;
    };
    // Read back through the names this session learned at recovery, and only
    // where nothing is working under the number itself: an order under that
    // number is the order that number means.
    let order_id = match context.order(named) {
        Some(_) => named,
        None => self.the_order_named(named).unwrap_or(named),
    };
    let Some(mut held) = shared.orders.get_order_info(order_id) else {
        // An order this session holds no account of. The venue states the
        // change against the order, and there is nothing here to state it on.
        log::debug!("the venue revised order {order_id}, which is not held here");
        return;
    };
    let mut changed = false;
    if let Some(exchange) = parsed.get(&30).filter(|at| !at.is_empty()) {
        // The venue an order works on is the contract's, which is what a
        // caller reads it off.
        held.contract.exchange = exchange.clone();
        changed = true;
    }
    if let Some(price) = parsed.get(&44).and_then(|p| p.parse::<f64>().ok()) {
        held.order.lmt_price = price;
        changed = true;
    }
    if !changed {
        return;
    }
    log::info!("the venue revised order {order_id}");
    shared.orders.push_order_info(order_id, held);
}
}

/// Why a message this client receives is deliberately not read.
///
/// Told apart from one nobody has looked at yet. Both are discarded, but only
/// one of them is a gap, and a diagnostic that cannot tell them apart is one
/// nobody keeps listening to.
///
/// Only what carries nothing this client needs belongs here. The order presets
/// were named here once as a user interface's defaults; they are not. The
/// venue fills the compete size and the offset of a pegged-best order from
/// them when the caller states neither, so an order sent from here differs
/// from the same order sent elsewhere. They are unread, and now counted as the
/// gap that is.
fn known_unread(subtype: &str) -> Option<&'static str> {
    match subtype {
        "93" => Some(
            "it answers the account and position subscription this client sends, and carries \
             the account, that request's own id and two flags — nothing the subscription does \
             not deliver itself. Named in the vendor's own inventory as a dimension response, \
             which is what it would carry on an account that had dimensions",
        ),
        _ => None,
    }
}

/// The order presets a session is told about, as `(key, version, changed at)`.
///
/// The venue states how many follow on 8167 and then repeats three fields for
/// each: the key it names the set by on 8168, the version on 8169, and the
/// moment it last changed on 8170. The values in a set are not here; asking
/// for those is a request of its own.
///
/// The moment is carried. The version says *that* a set changed and the moment
/// says *when*, which is what tells a caller whether an order it sent at a
/// given time was filled in from the old defaults or the new — and these
/// defaults fill in terms the caller left unstated on orders already placed.
/// Read past, the question could not be asked. A set the venue states no
/// moment for carries none rather than being dropped for want of it.
///
/// Read by walking the tags in the order the message states them rather than
/// by looking each up, because three of them repeat and a keyed read answers
/// with whichever came last.
///
/// Nothing where the venue's own count and what arrived disagree, and nothing
/// where it stated no count. The count is what says the message is whole, so
/// its absence is not proof of anything: read without it, a truncated answer
/// published however many pairs happened to parse — as the account's defaults,
/// beside a number the venue itself said was larger — and one carrying neither
/// count nor pairs cleared what the account holds.
fn parse_order_presets(msg: &[u8]) -> Option<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    let mut key: Option<String> = None;
    let mut stated: Option<usize> = None;
    for (tag, value) in crate::control::contracts::tag_sequence(msg) {
        match tag {
            8167 => stated = value.parse().ok(),
            8168 => key = Some(value),
            8169 => {
                if let Some(k) = key.take() {
                    out.push((k, value, String::new()));
                }
            }
            // Written onto the set the version just opened, because it follows
            // it. A set the venue states no moment for keeps the empty one it
            // was pushed with rather than taking the previous set's.
            8170 => {
                if let Some(held) = out.last_mut() {
                    held.2 = value;
                }
            }
            _ => {}
        }
    }
    // The count is what says the message is whole, so a message that does not
    // state one is not whole either. Accepted without it, a truncated answer
    // published however many pairs happened to parse — and one carrying
    // neither count nor pairs cleared the sets this account holds as though
    // the venue had said it holds none.
    match stated {
        Some(n) if n == out.len() => Some(out),
        _ => None,
    }
}

/// The algorithms the venue offers, keyed `PROVIDER/SECTYPE`.
///
/// Stated once, unasked, after logon. Nothing here read it, so a caller had no
/// way to know which algorithms this account may use and would find out by
/// having an order refused.
///
/// `FOXRIVER/STK:FOXRIVER-AE,FOXRIVER-AL-COMMON;IBALGO/BAG:IBALGO-AE`
fn parse_algorithms(raw: &str) -> std::collections::HashMap<String, Vec<String>> {
    raw.split(';')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let (key, names) = entry.split_once(':')?;
            let names: Vec<String> = names
                .split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect();
            Some((key.to_string(), names))
        })
        .collect()
}

fn handle_algorithms(parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
    let Some(raw) = parsed.get(&6597) else { return };
    let offered = parse_algorithms(raw);
    if offered.is_empty() {
        return;
    }
    log::info!("Algorithms offered on {} provider and security type pairs", offered.len());
    shared.reference.set_algorithms(offered);
}

/// The account's own configuration, which states further feature tokens on the
/// same tag the logon used. Read only from the logon, the list is short by
/// whatever this adds.
fn handle_account_config(parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
    let Some(raw) = parsed.get(&6542) else { return };
    let more: Vec<String> = raw.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect();
    if more.is_empty() {
        return;
    }
    log::info!("Account configuration states {} further features: {raw}", more.len());
    shared.reference.add_enabled_features(more);
}

/// An advisor request the venue has not answered yet.
///
/// The reply states nothing about what was asked — not which partition, not
/// whether it was a question or a replacement — beyond the number the request
/// went out under, so what the caller is owed is remembered here.
#[derive(Debug, Clone)]
pub(crate) struct PendingAdvisor {
    /// The caller's number for a replacement, carried back on its end.
    pub(crate) req_id: i64,
    /// Which partition, as the reference client numbers it.
    pub(crate) fa_data_type: i32,
    /// Whether the caller was writing rather than reading. A question is
    /// answered with the configuration; a replacement with its end.
    pub(crate) replacing: bool,
    /// When this client stops waiting for the venue to answer it.
    pub(crate) deadline: Instant,
}

/// A market data subscription held back until the venue names its contract.
///
/// The venue answers a subscription only when it is named by contract id, and
/// says nothing at all to one named any other way. A caller who names a
/// contract the ordinary way — symbol, security type, exchange — is owed the
/// lookup that turns it into an id.
#[derive(Debug, Clone)]
pub(crate) struct PendingSubscribe {
    pub(crate) instrument: crate::types::InstrumentId,
    /// Where the caller's request falls in the order of everything the client
    /// has asked for, carried through the lookup so the subscription that
    /// follows is recorded under it.
    pub(crate) issued: u64,
    /// The contract's own id, where the caller gave one. A lookup by id names
    /// the contract whatever else the caller left out; a lookup by symbol
    /// needs the venue to be told where it is listed, which is one of the
    /// things a caller who left it out does not have.
    pub(crate) con_id: i64,
    pub(crate) symbol: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    pub(crate) currency: String,
    /// What narrows the lookup that names the contract: the month, the strike,
    /// the right, the multiplier, and the class, the local name and the
    /// listing venue that tell one listing of a symbol from another. Carried
    /// whole rather than field by field, so that what a caller states is what
    /// the lookup asks.
    pub(crate) filters: crate::types::SecDefFilters,
    pub(crate) mode_9887: i32,
    /// Whether the caller asked for the venue's chargeable one-shot snapshot.
    pub(crate) regulatory_snapshot: bool,
}

/// Why a contract described by symbol is not served: the venue named none
/// for the description, or several, and what could not be done for it.
fn unnamed(sec_type: &str, symbol: &str, exchange: &str, listings: usize, so: &str) -> String {
    if listings > 1 {
        format!(
            "{sec_type} {symbol} on {exchange} matches {listings} contracts, so it names none and \
             {so}: state the currency or the exchange",
        )
    } else {
        format!("no security definition has been found for {sec_type} {symbol} on {exchange}, so {so}")
    }
}

/// The contract a request names, when it carries one.
///
/// Only the requests that must be sent under an id are listed: everything else
/// either carries no contract or is resolved by the venue from the symbol.
pub(crate) fn contract_named(cmd: &crate::types::ControlCommand) -> Option<&crate::types::ContractRef> {
    let contract = contract_of(cmd)?;
    // Only one the venue has not named yet: the rest already carry its id.
    (contract.con_id == 0).then_some(contract)
}

/// The contract a request names, whether or not the venue has numbered it.
pub(crate) fn contract_of(cmd: &crate::types::ControlCommand) -> Option<&crate::types::ContractRef> {
    use crate::types::ControlCommand as C;
    match cmd {
        C::FetchHistorical { contract, .. }
        | C::FetchHeadTimestamp { contract, .. }
        | C::FetchHistoricalTicks { contract, .. }
        | C::FetchHistoricalSchedule { contract, .. }
        | C::SubscribeRealTimeBar { contract, .. }
        | C::SubscribeDepth { contract, .. } => Some(contract),
        _ => None,
    }
}

/// The filters that go with that contract.
fn filters_named(cmd: &crate::types::ControlCommand) -> crate::types::SecDefFilters {
    use crate::types::ControlCommand as C;
    match cmd {
        C::FetchHistorical { filters, .. }
        | C::FetchHeadTimestamp { filters, .. }
        | C::FetchHistoricalTicks { filters, .. }
        | C::FetchHistoricalSchedule { filters, .. }
        | C::SubscribeRealTimeBar { filters, .. }
        | C::SubscribeDepth { filters, .. } => filters.clone(),
        _ => crate::types::SecDefFilters::default(),
    }
}

/// Fill in the id the venue has given the contract a request named.
fn name_the_contract(cmd: &mut crate::types::ControlCommand, id: i64) {
    use crate::types::ControlCommand as C;
    match cmd {
        C::FetchHistorical { contract, .. }
        | C::FetchHeadTimestamp { contract, .. }
        | C::FetchHistoricalTicks { contract, .. }
        | C::FetchHistoricalSchedule { contract, .. }
        | C::SubscribeRealTimeBar { contract, .. }
        | C::SubscribeDepth { contract, .. } => contract.con_id = id,
        _ => {}
    }
}

/// Which request a caller is waiting on.
pub(crate) fn request_id(cmd: &crate::types::ControlCommand) -> Option<u32> {
    match cmd {
        crate::types::ControlCommand::FetchHistorical { req_id, .. }
        | crate::types::ControlCommand::FetchHeadTimestamp { req_id, .. }
        | crate::types::ControlCommand::FetchHistoricalTicks { req_id, .. }
        | crate::types::ControlCommand::FetchHistoricalSchedule { req_id, .. }
        | crate::types::ControlCommand::SubscribeRealTimeBar { req_id, .. }
        | crate::types::ControlCommand::SubscribeDepth { req_id, .. } => Some(*req_id),
        _ => None,
    }
}

/// Which tag carries a maturity.
///
/// A full expiry date is MaturityDate (541) and a contract month is
/// MaturityMonthYear (200); they are not interchangeable, and an option asked
/// for by date on tag 200 matches nothing at all. Anything too short to be
/// either is left off rather than sent on a guess.
pub(crate) fn maturity_tag(maturity: &str) -> Option<u32> {
    match maturity.len() {
        6 => Some(200),
        n if n >= 8 => Some(541),
        _ => None,
    }
}

/// How long a reconnect waits for the recovery push before judging the orders
/// it did not mention. Generous, because a push that says nothing at all is
/// indistinguishable from one that has not started.
const RECOVERY_PUSH_GRACE: Duration = Duration::from_secs(30);

/// The same wait once the push has sent its own terminator. What is coming has
/// come; this only covers a fill report arriving just behind it.
const RECOVERY_TERMINATOR_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct CcpState {
    pub(crate) seen_exec_ids: HashSet<String>,
    /// Insertion order for `seen_exec_ids`, oldest at the front. Used to evict
    /// one entry at a time once the dedup window is full, instead of clearing
    /// the whole set — a wholesale clear would let a post-reconnect server
    /// replay of a recently-seen ExecID double-count a fill.
    pub(crate) exec_id_order: VecDeque<String>,
    pub(crate) disconnected: bool,
    /// When to account for orders the reconnect did not explain.
    ///
    /// An order that terminated while the connection was down leaves no
    /// message behind — its evidence is an absence, and absence only means
    /// something once the recovery push is known to be complete. Armed
    /// generously at reconnect so the sweep still runs when the push says
    /// nothing at all, and re-armed tightly when the push's own terminator
    /// arrives. Cleared on a disconnect so a second drop before the
    /// sweep cancels it rather than reaping against a dead session.
    pub(crate) recovery_sweep_at: Option<Instant>,
    /// Whether this connection has hydrated an order from the server's account
    /// of what is working. Separates the replay's terminator from the echo that
    /// looks like it.
    pub(crate) hydrated_any: bool,
    /// (req_id, is_single_shot). Single-shot = known-conId lookup whose
    /// first 35=d reply is also the last (server emits no 323=5/6 terminator
    /// for these). Multi-record by-symbol/matching-symbols requests push
    /// `false` and rely on the response-type sentinel for is_last.
    /// In-flight secdef requests: (req_id, single_shot, deadline). The
    /// deadline is swept by `sweep_contract_details` so a request the
    /// gateway never answers (SessionReject, dead socket, lost reply)
    /// surfaces error 200 + contract_details_end instead of hanging
    /// forever.
    pub(crate) pending_secdef: Vec<(u32, bool, Instant)>,
    /// Requests awaiting a matching-symbols reply, with the deadline after
    /// which one is given up on. Recorded only for a request that actually went
    /// out, and expired so a stale head cannot absorb a later reply.
    pub(crate) pending_matching_symbols: Vec<(u32, Instant)>,
    /// Whether the venue is in the middle of stating what it has finished.
    ///
    /// The answer to that question is a run of ordinary execution reports for
    /// orders this session never placed, ending with the sentinel that ends
    /// the opening replay. Inside the window they are filed as history; the
    /// live path never sees them, because through it a report that states a
    /// fill is a fill and a fill moves a position.
    pub(crate) completed_orders_open: bool,
    /// The answer being assembled, one entry per order the venue has finished.
    ///
    /// Each order's life arrives as several reports, each stating what changed
    /// and leaving the rest out, and they are not always adjacent. So the
    /// answer is built here and handed over whole when the venue says it has
    /// finished — rather than one record per report, which made a caller
    /// choose between the first report's fields and the last report's status
    /// and could not give them both.
    ///
    /// In the order the venue stated them, so that is the order a caller hears
    /// about them in.
    finished_orders: Vec<FinishedOrder>,
    /// The wire name an order is reported under, against the number this
    /// session knows it by.
    ///
    /// The venue names a recovered order two ways. Its recovery report states
    /// the permanent name on tag 11 and the number an API gave it beside
    /// that, and this session takes the second, because that is the number a
    /// caller withdraws it by. Every later report about the order states only
    /// the first. Looked up under that, the order this session is tracking was
    /// not found: a fill on it was booked against nothing, and while a
    /// finished-orders window was open it was filed as history instead.
    ///
    /// A rolling window, oldest out first, the way the execution ids beside it
    /// are held: a caller asking repeatedly for what the account has finished
    /// teaches this session a name per replayed order, and unbounded it grew
    /// for as long as the connection lasted.
    wire_name_to_order: std::collections::HashMap<u64, u64>,
    /// The order those names were learned in, so the oldest can go first.
    wire_names_learned: VecDeque<u64>,
    /// Whether the caller waiting on this window has already been told the
    /// answer is complete.
    ///
    /// The window outlives that: it is shut by the wire, and the caller is
    /// released when it has waited long enough. Told twice — once on the wait
    /// and once on the sentinel — the second signal was left standing for the
    /// next caller, who read it as the answer to a question the venue had not
    /// begun to answer.
    completed_orders_answered: bool,
    /// Whether the answer being assembled has taken all the orders it can.
    ///
    /// Said once rather than per report: past the bound every report the venue
    /// states about an order the answer does not already hold is left out of
    /// it, and that is one thing that happened, not thousands.
    the_answer_is_full: bool,
    /// Every order this answer has taken, handed over or still being
    /// assembled.
    ///
    /// The bound is on the answer, not on what is waiting to be handed over: a
    /// handover empties the records and the next order was taken beside them,
    /// so an answer with no end in sight grew without limit however often it
    /// was handed over. Kept here, an order already taken is still merged as
    /// the venue states more about it, and only an order the answer has never
    /// seen is left out.
    orders_in_this_answer: std::collections::HashSet<u64>,
    /// A caller asked what the venue has finished before the session's own
    /// replay was over, so the question is held until it is.
    ///
    /// The two answers end with the same sentinel and the wire correlates
    /// neither, so a window opened across the replay is closed by the replay's
    /// own ending — the caller is told the answer is complete before it has
    /// begun, and the reports that follow take the live path, where a report
    /// that states a fill is a fill. Worse, the replayed orders themselves
    /// arrive inside that window and are filed as history instead of being
    /// recovered into the book a withdrawal walks.
    completed_orders_wanted: Option<Instant>,
    /// Which turn of the question the window that is open belongs to.
    ///
    /// The answer is a run of ordinary reports and says nothing about which
    /// question it answers, so the turn the caller asked on travels with the
    /// question and comes back on its end: a caller that gave up leaves its
    /// answer on its way, and the next caller must not be released by it.
    completed_orders_asked_on: u64,
    /// And the turn of a question held behind that window.
    completed_orders_queued_on: Option<u64>,
    /// When the question stops waiting for the replay of the working orders.
    ///
    /// Held against the connection rather than against the question: the
    /// replay happens once per connection, so a question asked after it has
    /// had its time is not held at all. Armed against each question instead,
    /// every call paid the hold again.
    replay_hold_until: Option<Instant>,
    /// When an open window is shut whether or not its sentinel has come.
    ///
    /// Nothing on the wire obliges the venue to send one, and a window with no
    /// deadline stayed open for the life of a connection that never dropped:
    /// the caller waited out its own clock for a sentinel nobody was sending,
    /// every later question queued behind it for ever, and every report for an
    /// order this session does not hold went on being filed as history.
    completed_orders_deadline: Option<Instant>,
    /// In-flight option chain requests: (req_id, symbol, underlying conId,
    /// deadline). The request states no id of its own, so the symbol is what
    /// ties a reply back to it, and the conId is held because the callback
    /// names the underlying the caller asked about.
    pub(crate) pending_option_params: Vec<(u32, String, i64, Instant)>,
    /// Dividend queries, as `(the id it went out under, the contract it is
    /// about, when to stop waiting)`.
    ///
    /// The query goes out as text and the answer echoes only the id, so this
    /// is what says which contract an answer is about. Two queries in flight
    /// on one contract cannot happen — a second is not sent while the first is
    /// outstanding — but two on different contracts can, and read by contract
    /// rather than by id the second schedule would be filed under the first.
    ///
    /// An entry is given up on at its deadline. Kept until the answer came, a
    /// query the venue never answered held that contract for the life of the
    /// session and one more entry here for every underlying it happened to —
    /// so the list grew without bound and the contract could never be asked
    /// about again.
    pending_dividends: Vec<(String, u32, Instant)>,
    /// The contracts already asked about and answered.
    ///
    /// A schedule is a fact about the contract rather than a subscription, so
    /// it is asked for once. Told apart from the list above because that one
    /// empties as answers arrive: with only that, the next option written on
    /// the same underlying asked the same question again and was answered with
    /// the schedule this session already held.
    dividends_answered: std::collections::HashSet<u32>,
    /// The ids of queries given up on, and the contract each was about.
    ///
    /// Given up on says this session may ask again. It does not say the venue
    /// will not answer, and the echoed id is the only thing that says which
    /// contract an answer belongs to — dropped with the entry, a schedule that
    /// arrived a moment late was thrown away with nothing else to attribute it
    /// by. Bounded, oldest first: this is a second chance, not a record.
    dividends_given_up_on: std::collections::VecDeque<(String, u32)>,
    /// The id the next query of this kind goes out under. This client's own
    /// number, distinct from every other request's because the venue echoes
    /// only this one tag back.
    next_xml_query_id: u64,
    /// Secdef replies awaiting paired schedule reply (joined by tag 6256).
    pub(crate) pending_schedule_pair: Vec<PendingSchedulePair>,
    /// Profit-and-loss subscriptions standing, by request number and account.
    ///
    /// The venue serves one on the connection that asked for it, so a rebuilt
    /// connection is asked for each again, as it is for the account and the
    /// positions. A subscription the caller withdrew is taken out of here and
    /// not renewed.
    pub(crate) pnl_subscriptions: Vec<(i64, String)>,
    /// Counter for internal schedule subscribe req IDs.
    pub(crate) next_schedule_sub_id: u32,
    /// Fan-out state for by-symbol secdef requests. Each entry tracks the
    /// per-exchange `35=c` requests issued in response to the master
    /// `35=d|320={api_req_id}|6046={list}` reply, and counts the per-exchange
    /// `35=d` replies as they arrive. `contract_details_end` fires for
    /// `api_req_id` once `received >= fanout_req_ids.len()`.
    pub(crate) pending_fanout: Vec<PendingFanout>,
    /// Contracts already handed to each caller's request. A lookup on a
    /// smart-routed symbol is answered once by the request itself and again by
    /// every venue it fans out to, and every one of those answers describes the
    /// same contract with a different `exchange` — the full venue list already
    /// rides inside each. Delivering them all reported one contract as
    /// twenty-seven listings of itself. Cleared when the request ends.
    pub(crate) details_delivered: std::collections::HashMap<u32, HashSet<i64>>,
    /// Counter for internal fan-out req IDs (tag 320 on per-exchange `35=c`).
    pub(crate) next_fanout_id: u32,
    /// Counter for internal secdef req IDs (auto-fetch on cold-cache positions).
    pub(crate) next_internal_secdef_id: u32,
    /// The number the next advisor-configuration request states as its own.
    ///
    /// These are counted from one for the session, and the request sends the
    /// count as a string, so a reply can be matched to the question that asked
    /// it.
    pub(crate) next_advisor_request: u32,
    /// The advisor requests waiting on an answer, by the number they went out
    /// under. The venue carries that number back on its reply and nothing
    /// else identifies which question was asked, so a reply arriving with no
    /// entry here belongs to nobody.
    pub(crate) pending_advisor: std::collections::HashMap<String, PendingAdvisor>,
    /// The key the open account subscription was asked for under, so the
    /// withdrawal can name it. Tag 6036 carries whether the request opens the
    /// subscription or closes it; a session that only ever opens them holds one
    /// per loop for the life of the connection.
    pub(crate) account_request_key: Option<String>,
    /// User-message subtypes the venue has sent that nothing here reads, so
    /// each is named once rather than on every arrival.
    unread_subtypes: std::collections::HashSet<String>,
    /// Message types the venue has sent that nothing here reads.
    unread_types: std::collections::HashSet<String>,
    /// Market data subscriptions waiting on the lookup that will name their
    /// contract, keyed by that lookup's request id.
    pub(crate) pending_md_subscribe: Vec<(u32, PendingSubscribe, Instant)>,
    /// Those whose contract the venue has now named.
    pub(crate) resolved_md_subscribe: Vec<(i64, PendingSubscribe)>,
    /// Requests that named a contract the venue has not given an id to yet,
    /// keyed by the lookup asking for that id.
    ///
    /// A subscription is sent by symbol and the venue resolves it. Everything
    /// on the historical farm is asked for by id, so a caller passing the
    /// contract it wrote down — which is what a program written against the
    /// reference client does — sent a request under id zero, and the venue
    /// answered a complete series with nothing in it.
    pub(crate) pending_named: Vec<(u32, crate::types::ControlCommand, Instant)>,
    /// Those the venue has now named, ready to be handled as though the id had
    /// been there all along.
    pub(crate) resolved_named: Vec<crate::types::ControlCommand>,
    /// conIds a secdef has been fetched for without a caller asking, and the
    /// request that fetched each. The request is kept so a fetch that is never
    /// answered can be forgotten; held indefinitely, one lost request leaves
    /// that contract unasked for the life of the session and every position on
    /// it unnamed.
    pub(crate) auto_fetched_conids: HashMap<i64, u32>,
    /// Scanner results awaiting per-conId contract-detail enrichment.
    /// Each entry parks a parsed `<ScanResponse>` until every con_id the
    /// cache missed has been resolved via the same 35=d path that user-initiated
    /// `reqContractDetails` uses.
    pub(crate) pending_scanner_enrichment: Vec<PendingScannerEnrichment>,
}

/// Scanner result parked for contract-detail fan-out.
pub(crate) struct PendingScannerEnrichment {
    pub api_req_id: u32,
    pub result: crate::control::scanner::ScannerResult,
    pub awaiting: HashSet<i64>,
    pub deadline: Instant,
}

/// State for a secdef reply awaiting its paired schedule reply.
pub(crate) struct PendingSchedulePair {
    pub api_req_id: u32,
    pub join_key: String,
    pub def: crate::control::contracts::ContractDefinition,
    pub is_last: bool,
    pub deadline: Instant,
}

/// In-flight by-symbol fan-out: per-exchange `35=c` requests sent after
/// the master `35=d` reply. Each per-exchange `35=d` reply (matched by tag
/// 320 string) is forwarded to `api_req_id` as one `contract_details`.
pub(crate) struct PendingFanout {
    pub api_req_id: u32,
    pub fanout_req_ids: Vec<String>,
    /// Which legs have answered.
    ///
    /// The exchanges that have answered. A fan-out ends when every exchange it
    /// asked has answered. Counting frames instead completes it twice over for
    /// a leg answered with more than one row, and drops the legs still
    /// outstanding.
    pub answered: Vec<String>,
    /// Idle deadline, refreshed on every per-exchange reply.
    ///
    /// A fan-out asks each exchange the contract lists on and ends when every
    /// one has answered. One reply lost or unreadable would leave the count
    /// short for good, so this bounds the wait — but reaching it is a failed
    /// request, reported as one, not the ordinary way a fan-out finishes.
    pub deadline: Instant,
}

impl CcpState {
    pub(crate) fn new() -> Self {
        Self {
            seen_exec_ids: HashSet::with_capacity(256),
            exec_id_order: VecDeque::with_capacity(256),
            disconnected: false,
            recovery_sweep_at: None,
            hydrated_any: false,
            pending_secdef: Vec::new(),
            pending_matching_symbols: Vec::new(),
            completed_orders_open: false,
            finished_orders: Vec::new(),
            wire_name_to_order: std::collections::HashMap::new(),
            wire_names_learned: VecDeque::new(),
            completed_orders_answered: false,
            the_answer_is_full: false,
            orders_in_this_answer: std::collections::HashSet::new(),
            completed_orders_wanted: None,
            replay_hold_until: None,
            completed_orders_asked_on: 0,
            completed_orders_queued_on: None,
            completed_orders_deadline: None,
            pending_option_params: Vec::new(),
            pending_dividends: Vec::new(),
            dividends_answered: std::collections::HashSet::new(),
            dividends_given_up_on: std::collections::VecDeque::new(),
            next_xml_query_id: 1,
            pending_schedule_pair: Vec::new(),
            pnl_subscriptions: Vec::new(),
            next_schedule_sub_id: 1,
            pending_fanout: Vec::new(),
            details_delivered: std::collections::HashMap::new(),
            next_fanout_id: 1,
            next_internal_secdef_id: 0xF000_0000,
            next_advisor_request: 1,
            pending_advisor: std::collections::HashMap::new(),
            account_request_key: None,
            unread_subtypes: std::collections::HashSet::new(),
            unread_types: std::collections::HashSet::new(),
            pending_md_subscribe: Vec::new(),
            resolved_md_subscribe: Vec::new(),
            pending_named: Vec::new(),
            resolved_named: Vec::new(),
            auto_fetched_conids: HashMap::new(),
            pending_scanner_enrichment: Vec::new(),
        }
    }

    /// Record `exec_id` in the fill-dedup window. Returns `true` if it is new
    /// (the fill should be processed) and `false` if it was already seen (a
    /// duplicate to skip).
    ///
    /// Backed by a bounded rolling window: once `EXEC_ID_WINDOW` IDs are held,
    /// the oldest is evicted one at a time. This replaces a previous wholesale
    /// `clear()` that dropped the entire history at the cap, which let a
    /// post-reconnect server replay of a recently-seen ExecID double-count the
    /// fill and corrupt the position.
    pub(crate) fn record_exec_id(&mut self, exec_id: &str) -> bool {
        if !self.seen_exec_ids.insert(exec_id.to_string()) {
            return false;
        }
        self.exec_id_order.push_back(exec_id.to_string());
        while self.exec_id_order.len() > EXEC_ID_WINDOW {
            if let Some(old) = self.exec_id_order.pop_front() {
                self.seen_exec_ids.remove(&old);
            }
        }
        true
    }

    /// Learn the venue's own name for an order this session numbers itself.
    ///
    /// Held for as long as the order is working, because that is what the name
    /// is for: every later report about it states only this name, and a
    /// revision, a fill or a withdrawal that cannot be resolved through it
    /// reaches the wrong order or none. One name is learned per order the
    /// venue replays, though, and a caller may ask what the account has
    /// finished as often as it likes — so the oldest name of an order that is
    /// no longer working goes when a new one arrives, and a name is dropped
    /// only when its order is not in the book.
    pub(crate) fn remember_the_venues_name_for(
        &mut self, wire_name: u64, order_id: u64, context: &Context,
    ) {
        if self.wire_name_to_order.insert(wire_name, order_id).is_none() {
            self.wire_names_learned.push_back(wire_name);
        }
        while self.wire_names_learned.len() > WIRE_NAME_WINDOW {
            // The name just learned is never the one forgotten. The order it
            // belongs to is recovered into the book behind this, so at this
            // moment nothing is working under it — which made it the first
            // name the sweep below found to forget, and the order went live
            // with no name at all.
            let forgettable = self.wire_names_learned.iter().position(|name| {
                *name != wire_name
                    && self.wire_name_to_order
                        .get(name)
                        .is_none_or(|order| context.order(*order).is_none())
            });
            // Every name held belongs to an order still working, so there is
            // nothing to forget: the window is what bounds the names of
            // orders that have finished, not what bounds the account.
            let Some(at) = forgettable else { break };
            if let Some(oldest) = self.wire_names_learned.remove(at) {
                self.wire_name_to_order.remove(&oldest);
            }
        }
    }

    /// Which order the venue means by one of its own names, where this session
    /// has been told.
    pub(crate) fn the_order_named(&self, wire_name: u64) -> Option<u64> {
        self.wire_name_to_order.get(&wire_name).copied()
    }

    /// How many of those names are held.
    #[cfg(test)]
    pub(crate) fn how_many_venue_names_are_held(&self) -> usize {
        self.wire_name_to_order.len()
    }

    /// Assemble one finished order, as the reports about it would.
    #[cfg(test)]
    pub(crate) fn hold_a_finished_order_for_test(
        &mut self, order_id: u64, status: crate::types::OrderStatus,
    ) {
        self.orders_in_this_answer.insert(order_id);
        self.finished_orders.push(FinishedOrder {
            order_id,
            contract: Default::default(),
            order: crate::types::model::Order { order_id: order_id as i64, ..Default::default() },
            status,
            filled: 0,
            state: Default::default(),
            ord_type: String::new(),
            exec_inst: String::new(),
        });
    }

    /// Whether the window has already seen this execution, asked without
    /// spending its key: the booking that spends it runs further along the
    /// same report, and the readers before it need the same answer.
    pub(crate) fn already_recorded_exec_id(&self, exec_id: &str) -> bool {
        self.seen_exec_ids.contains(exec_id)
    }

    pub(crate) fn process_ccp_message(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };
        if *crate::engine::hot_loop::CAPTURE_WIRE {
            let hex: String = msg.iter().map(|b| format!("{b:02x}")).collect();
            shared.market.note_unread_wire("trading-msg", hex);
        }
        match msg_type {
            fix::MSG_EXEC_REPORT => self.handle_exec_report(&parsed, msg, context, shared, event_tx, account_id),
            fix::MSG_CANCEL_REJECT => self.handle_cancel_reject(&parsed, context, shared, event_tx),
            fix::MSG_NEWS => self.handle_news_bulletin(&parsed, shared),
            fix::MSG_HEARTBEAT => {}
            fix::MSG_TEST_REQUEST => {
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = ccp_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    hb.last_ccp_sent = Instant::now();
                }
            }
            "3" => {
                let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                let ref_tag = parsed.get(&371).map(|s| s.as_str()).unwrap_or("?");
                let stated = parsed.get(&320).and_then(|s| s.parse::<u32>().ok());
                // Fan-out and schedule requests carry names rather than
                // numbers. An unreadable number still names a request.
                let named_nothing = !parsed.contains_key(&320);
                log::warn!("SessionReject: reason='{reason}' refTag={ref_tag} request={stated:?}");
                // A refusal of an option-chain request. The venue rejects one
                // asked for an underlying it cannot number with "Unknown
                // contract", naming the request; attributed to nothing, the
                // caller waited out the chain's deadline for an answer that
                // had arrived fifteen milliseconds after the request.
                let refused_chain = stated
                    .and_then(|rid| self.pending_option_params.iter().position(|(pid, ..)| *pid == rid))
                    .or_else(|| {
                        (named_nothing
                            && self.pending_option_params.len() == 1
                            && self.pending_secdef.is_empty()
                            && self.pending_matching_symbols.is_empty())
                        .then_some(0)
                    });
                if let Some(at) = refused_chain {
                    let (req_id, symbol, _, _) = self.pending_option_params.remove(at);
                    log::warn!("Option chain request req_id={req_id} symbol={symbol} rejected: {reason}");
                    shared.reference.push_historical_error(
                        req_id, crate::error_codes::Refusal::NO_DEFINITION,
                        format!("option chain request rejected: {reason}"),
                    );
                    return;
                }
                // A refusal of a symbol search, which states its own number on
                // this tag as the two beside it do. Matched against nothing,
                // the caller waited out the sweep's timeout and was told the
                // venue had never replied — when it had replied at once, and
                // said why.
                if let Some(at) = stated.and_then(|rid| {
                    self.pending_matching_symbols.iter().position(|(pid, _)| *pid == rid)
                }) {
                    let (req_id, _) = self.pending_matching_symbols.remove(at);
                    log::warn!("Matching symbols request req_id={req_id} rejected: {reason}");
                    // Refused rather than answered empty: an empty answer is a
                    // search the venue ran and nothing matched, which is not
                    // what this is.
                    shared.reference.push_historical_error(
                        req_id, crate::error_codes::Refusal::NO_DEFINITION,
                        format!("matching symbols request rejected: {reason}"),
                    );
                    return;
                }
                // The venue names the request it is refusing, on tag 320 —
                // the same tag its answers are matched on below. Attributed
                // by count instead, only a lone request was ever told: five
                // asked together drew five rejects inside a tenth of a
                // second, and all five callers waited out the sweep and were
                // told the request had timed out with no reply, when the
                // reply had arrived at once and said why.
                //
                // Falling back on the count where the venue names nothing,
                // which is the case this stood on.
                let named = stated
                    .and_then(|rid| {
                        self.pending_secdef.iter().position(|(pid, _, _)| *pid == rid)
                    })
                    .or_else(|| {
                        // Only where it named nothing, as the chain above reads
                        // it. This tag is not this request's alone — a symbol
                        // search states its own number on it — so a refusal
                        // that named a request of another kind matched no
                        // definition lookup here and fell through to the count,
                        // which answered a lookup that had not been refused at
                        // all: that caller was handed somebody else's refusal
                        // and its own definitions later had nothing waiting.
                        (named_nothing
                            && self.pending_secdef.len() == 1
                            && self.pending_fanout.is_empty()
                            && self.pending_matching_symbols.is_empty()
                            // And the chains, which the branch above defers to
                            // this one for. Left out, the two were not
                            // symmetric: a chain waiting with a lone lookup
                            // deferred there and was taken here, so the lookup
                            // was told the chain's refusal and the chain waited
                            // out its own deadline to be told nothing came.
                            && self.pending_option_params.is_empty())
                        .then_some(0)
                    });
                if let Some(at) = named {
                    let (req_id, _, _) = self.pending_secdef.remove(at);
                    if req_id < 0xF000_0000 {
                        shared.reference.push_historical_error(
                            req_id, 200,
                            format!("contract details request rejected: {reason}"),
                        );
                        shared.reference.push_contract_details_end(req_id);
                        emit(event_tx, Event::ContractDetailsEnd(req_id));
                    } else {
                        // A lookup the engine made for itself: forgotten, so
                        // the next report naming that contract asks again, as
                        // the deadline sweep already arranges.
                        self.auto_fetched_conids.retain(|_, rid| *rid != req_id);
                    }
                }
            }
            "U" => {
                if let Some(comm) = parsed.get(&6040) {
                    match comm.as_str() {
                        "75" => {
                            // Position + market price feed (init burst + after each
                            // fill). Not the end of the batch: the account's own
                            // figures follow it, and calling the download
                            // complete here answered a caller before they
                            // arrived. The venue ends the batch itself, below.
                            self.handle_position_feed(msg, ccp_conn, context, shared, event_tx, hb);
                        }
                        "77" => self.handle_account_summary(&parsed, shared),
                        "143" => {
                            // P&L midnight seed — store for client-side daily P&L
                            // computation
                            positions::handle_pnl_response(msg, shared);
                        }
                        "152" => handle_pnl_prices(msg, shared),
                        "186" => {
                            if let Some(matches) = crate::control::contracts::parse_matching_symbols_response(msg) {
                                // A 186 frame is the real answer only when it
                                // carries the match-count tag 146 — present
                                // even when the count is zero. Frames without
                                // it are not-ready acks: popping on one would
                                // deliver a bogus empty answer and orphan the
                                // data frame that follows (observed live; the
                                // same ack-then-data shape as the what-if
                                // path).
                                if extract_tag_value(msg, b"146=").is_none() {
                                    log::debug!("matching-symbols ack frame (no tag 146) — awaiting data frame");
                                } else {
                                // Match the reply to its request by the req_id
                                // the server echoes in tag 320, NOT by queue
                                // order: FIFO cross-attributes out-of-order
                                // replies (, same fix as pending_secdef).
                                let echoed = extract_tag_value(msg, b"320=")
                                    .and_then(|v| v.parse::<u32>().ok());
                                let pos = match echoed {
                                    Some(rid) => self.pending_matching_symbols.iter().position(|(p, _)| *p == rid),
                                    // No echo on the wire: attribution is only
                                    // safe with a single request in flight.
                                    None if self.pending_matching_symbols.len() == 1 => Some(0),
                                    None => None,
                                };
                                if let Some(pos) = pos {
                                    let (req_id, _) = self.pending_matching_symbols.remove(pos);
                                    // An empty result is an answer in its own
                                    // right ("no such symbol") and is
                                    // delivered. Dropped, the caller waits
                                    // indefinitely and the stale queue head
                                    // misattributes every later reply.
                                    shared.reference.push_matching_symbols(req_id, matches);
                                } else {
                                    log::warn!(
                                        "matching-symbols reply not attributable: echoed={:?} pending={:?}",
                                        echoed, self.pending_matching_symbols,
                                    );
                                }
                                }
                            }
                        }
                        // The venue's error channel. Two subtypes, one
                        // channel: which number it arrives under depends only
                        // on a capability the session negotiated at logon, not
                        // on the error.
                        "60" => handle_trade_charge(&parsed, shared),
                        "192" | "278" => handle_venue_error(&parsed, shared),
                        // The venue speaking to the account holder rather than
                        // about a request: it states the matter as text and
                        // names the account it concerns, and it belongs to no
                        // request anyone made — which is what the channel
                        // above already reports, so it is reported there.
                        // Read by nothing, it reached the account holder
                        // nowhere at all.
                        "42" => handle_venue_error(&parsed, shared),
                        "81" => handle_algorithms(&parsed, shared),
                        // The venue moving a working order: it names the order
                        // and states the terms it has changed — where it is
                        // now working, what its limit now is, or both. Read by
                        // nothing, a caller's own account of the order stayed
                        // at what it was placed with while the venue worked a
                        // different one.
                        "110" => self.handle_order_revision(&parsed, context, shared),
                        "210" => handle_account_config(&parsed, shared),
                        "117" => self.handle_advisor_config(&parsed, shared),
                        "139" => self.handle_option_chain(msg, shared),
                        "102" => self.handle_exchange_list(msg, shared),
                        "107" => self.handle_schedule_reply(msg, shared, event_tx),
                        "18" => {
                            // The venue restating its own clock, unasked. It is
                            // never asked for it — this wire carries no such
                            // request — so a caller wanting it is answered from
                            // this machine's clock and the difference the venue
                            // has stated, of which this is the second and last
                            // statement, after the one on the logon.
                            if let Some(seconds) = parsed.get(&6114).and_then(|v| v.parse::<i64>().ok()) {
                                // Saturating, as the reference conversion is:
                                // a second count near the end of what the type
                                // holds carries past it in milliseconds, and
                                // the product wrapped to a clock a thousand
                                // years behind the one the venue stated.
                                shared.market.note_venue_millis(seconds.saturating_mul(1_000));
                            }
                        }
                        // The order presets this account holds, which this
                        // session asks for at logon and then threw away. The
                        // venue keeps a set of order defaults per security
                        // type and fills parts of an order the caller left
                        // unstated from them, so what sets exist is a fact
                        // What a contract pays out, answered under the id
                        // the query went out with. The reference terminal's
                        // own option model reads its dividend schedule off
                        // this, and nothing here had ever asked for one.
                        "20" => self.handle_dividends_answer(&parsed, shared),
                        // about every order placed from here.
                        //
                        // It states the sets and their versions rather than
                        // the values in them: asking for those is a request of
                        // its own, and nothing on the reference client's
                        // surface makes it.
                        "194" => match parse_order_presets(msg) {
                            // Stored whether or not any came. An account that
                            // holds none says so, and refusing to store that
                            // left the last account's sets published for the
                            // life of the session.
                            Some(presets) => {
                                log::info!(
                                    "the account holds {} sets of order defaults",
                                    presets.len(),
                                );
                                shared.reference.set_order_presets(presets);
                            }
                            None => log::debug!(
                                "the order defaults did not arrive whole, so what was published \
                                 before them stands",
                            ),
                        },
                        // Something the venue said that nothing here reads.
                        // Dropped in silence it is indistinguishable from the
                        // venue saying nothing, which is how an answer that had
                        // been arriving all along went unnoticed. Named once,
                        // the first time each is seen, so a session that meets
                        // one leaves a record without repeating itself.
                        other => {
                            if self.unread_subtypes.insert(other.to_string()) {
                                match known_unread(other) {
                                    Some(why) => log::debug!("Subtype {other} is not read: {why}"),
                                    None => {
                                        shared.market.note_unread_wire(
                                            "trading", format!("user message {other}"),
                                        );
                                        log::info!(
                                        "Unread user message: subtype {other}. Nothing here reads \
                                         it, so whatever it carries is being discarded"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // The end of a batch the venue was sending. An account request is
            // otherwise only known to be finished when its rows arrive, and an
            // account holding nothing sends no rows — so a caller waiting on
            // the download would wait for something that was already over.
            "EB" => {
                let ends = parsed.get(&6529).map(String::as_str).unwrap_or("");
                if ends.starts_with("AR") {
                    log::info!("Account request {ends} is complete");
                    // Only where this end is the outstanding request's. Any
                    // other request's end squares nothing, and declaring the
                    // download over on it lets a caller through to the answer
                    // the squaring exists to prevent.
                    if let Some(unstated) = shared.portfolio.set_account_download_complete(ends) {
                        for con_id in unstated {
                            let avg_cost = shared.portfolio.position_info(con_id)
                                .map(|i| i.avg_cost).unwrap_or_default();
                            let Some(instrument) = context.market.instrument_by_con_id(con_id)
                            else { continue };
                            let standing = context.position(instrument);
                            if standing != 0.0 { context.update_position(instrument, -standing); }
                            shared.portfolio.set_position(instrument, 0.0);
                            emit(event_tx, Event::PositionUpdate {
                                instrument, con_id, position: 0.0, avg_cost,
                            });
                        }
                        // Last, so a caller waiting on the download is let
                        // through to an account that has been squared with
                        // what the venue just said rather than to what it held
                        // before.
                        shared.portfolio.account_download_is_settled();
                    }
                }
            }
            "UT" | "UM" => positions::handle_account_update(msg, context, shared),
            // The per-currency figures, which are not a name-and-value stream
            // the way the two above are.
            "RL" => positions::handle_ledger_update(msg, shared),
            // The same figures, for the sets of holdings the account does not
            // hold itself. Applied to the account's own they would overstate
            // what it is worth, so they are kept where the holdings they
            // describe are kept.
            "AL" => handle_account_update_elsewhere(msg, shared, crate::types::HeldElsewhere::Away),
            "UL" => handle_account_update_elsewhere(msg, shared, crate::types::HeldElsewhere::Aside),
            // One frame, every holding it names — see `split_position_entries`.
            "UP" => {
                for one in positions::split_position_entries(msg) {
                    positions::handle_position_update(&one, context, shared, event_tx);
                }
            }
            // The venue keeps three sets of holdings and this client read one.
            // The others carry the same fields in the same tags — they differ
            // only in which set they belong to — and were discarded, so a
            // caller could not learn the account held anything away at all.
            //
            // One frame, every holding it names, the same way the account's own
            // set is read. Handed the flat map instead, a frame naming several
            // holdings arrived as one: the generic parser keeps the last value
            // of a repeated tag, so every holding but the last was gone before
            // anything could see it.
            "AP" | "DO" | "DP" => {
                let held = match msg_type {
                    "AP" => crate::types::HeldElsewhere::Away,
                    "DO" => crate::types::HeldElsewhere::DisplayOnly,
                    _ => crate::types::HeldElsewhere::Aside,
                };
                for one in positions::split_position_entries(msg) {
                    positions::handle_position_elsewhere(&one, shared, held);
                }
            }
            "d" => {
                let response_req_id = crate::control::contracts::secdef_response_req_id(msg);
                // A reply can describe several contracts: a symbol asked for
                // without a currency is answered with every listing that
                // carries it. Read as one contract it keeps whichever came
                // last, and the venue fan-out then follows that one, so the
                // rest are lost before anything can see them. Deliver them all
                // here; the row the path below delivers is deduplicated
                // against these by contract id.
                let listings = {
                    // What a definition carried that nothing here reads. The
                    // point of asking about a contract is to be told about it,
                    // and a field that arrives and is dropped is a fact about
                    // the contract nobody can Recorded rather than guessed
                    // at, so the gap is measurable from a real reply.
                    let unread = crate::control::contracts::unread_definition_tags(msg);
                    if !unread.is_empty() {
                        shared.market.note_unread_wire(
                            "definition",
                            unread.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","),
                        );
                    }
                    let all = crate::control::contracts::parse_secdef_responses(msg, shared.island_for_nasdaq());
                    let listings = all.len();
                    // The venue states which venues SMART routes to, in the
                    // order a quote's exchange bitmask refers to. Taking it
                    // replaces this client's own list, whose order was its own
                    // and bore no resemblance to this.
                    if let Some(venues) = all.iter().map(|d| &d.smart_venues).find(|v| !v.is_empty()) {
                        shared.reference.set_smart_components(
                            venues
                                .iter()
                                .enumerate()
                                .map(|(i, exchange)| crate::types::SmartComponent {
                                    bit_number: i as i32,
                                    exchange: exchange.clone(),
                                    exchange_letter: crate::types::exchange_letter(exchange).to_string(),
                                })
                                .collect(),
                        );
                        shared.reference.note_smart_components_provisional(false);
                    }
                    if listings > 1
                        && let Some(rid) = response_req_id.as_ref().and_then(|r| r.parse::<u32>().ok())
                        && rid < 0xF000_0000
                    {
                        // The listing the flat parse below produces is that
                        // path's to deliver: it is the one paired with the
                        // trading hours, and claimed here it reached the
                        // caller without them while the enriched row was
                        // dropped at the gate.
                        let flat = crate::control::contracts::parse_secdef_response(msg, shared.island_for_nasdaq())
                            .map(|d| d.con_id);
                        for def in all.into_iter().filter(|d| d.con_id != 0 && Some(d.con_id) != flat) {
                            self.note_what_it_is_written_on(&def, shared, ccp_conn, hb);
                            shared.reference.cache_definition(
                            def.con_id as i64,
                            // Mapped where every other reader of a
                            // definition maps it. Written out here, an
                            // option was cached without its strike, its
                            // right, its expiry or its multiplier — which
                            // is all that tells two options on one
                            // underlying apart.
                            crate::types::model::ContractDetails::from_definition(&def).contract,
                        );
                            if self.details_delivered.entry(rid).or_default().insert(def.con_id as i64) {
                                let for_event = clone_for_event(event_tx, &def);
                                shared.reference.push_contract_details(rid, def);
                                if let Some(details) = for_event {
                                    emit(event_tx, Event::ContractDetails { req_id: rid, details: Box::new(details) });
                                }
                            }
                        }
                    }
                    listings
                };

                let fanout_idx = response_req_id.as_ref().and_then(|rid| {
                    self.pending_fanout.iter().position(|p| {
                        p.fanout_req_ids.iter().any(|id| id == rid)
                    })
                });
                if let (Some(idx), Some(rid)) = (fanout_idx, response_req_id.as_ref()) {
                    let api_req_id = self.pending_fanout[idx].api_req_id;
                    let parsed = crate::control::contracts::parse_secdef_response(msg, shared.island_for_nasdaq());
                    if parsed.is_none() {
                        // Counted below all the same: a leg whose reply cannot
                        // be read is answered, and left uncounted the request
                        // waited out its deadline for a reply already in.
                        log::warn!("lookup {api_req_id}: a leg answered with a definition that cannot be read");
                    }
                    if let Some(def) = parsed {
                        // No con_id is "no definition for this
                        // exchange" — cache nothing and emit no row. The leg
                        // still counts toward the fan-out below, so the
                        // request completes.
                        if def.con_id != 0 {
                            shared.reference.cache_definition(
                            def.con_id as i64,
                            // Mapped where every other reader of a
                            // definition maps it. Written out here, an
                            // option was cached without its strike, its
                            // right, its expiry or its multiplier — which
                            // is all that tells two options on one
                            // underlying apart.
                            crate::types::model::ContractDetails::from_definition(&def).contract,
                        );
                            self.note_what_it_is_written_on(&def, shared, ccp_conn, hb);
                            identify_position(shared, &def);
                            self.try_release_scanner_enrichments(def.con_id as i64, shared);
                            // The master row for this same contract may be
                            // parked waiting for its trading hours. It is the
                            // richer of the two and claims the same dedup slot,
                            // so delivering this one first hands the caller a
                            // contract with no trading or liquid hours and
                            // drops the enriched row when it arrives.
                            let awaiting_schedule = self.pending_schedule_pair.iter().any(|p| {
                                p.api_req_id == api_req_id && p.def.con_id == def.con_id
                            });
                            if !awaiting_schedule
                                && self.details_delivered.entry(api_req_id).or_default().insert(def.con_id as i64)
                            {
                                let for_event = clone_for_event(event_tx, &def);
                                shared.reference.push_contract_details(api_req_id, def);
                                if let Some(details) = for_event {
                                    emit(event_tx, Event::ContractDetails { req_id: api_req_id, details: Box::new(details) });
                                }
                            }
                        }
                    }
                    // A leg the gateway cannot resolve carries no contract:
                    // it still completes the fan-out, but a zeroed row is
                    // not a listing.
                    if !self.pending_fanout[idx].answered.iter().any(|id| id == rid) {
                        self.pending_fanout[idx].answered.push(rid.clone());
                    }
                    self.pending_fanout[idx].deadline = Instant::now() + SECDEF_TIMEOUT;
                    if self.pending_fanout[idx].answered.len() >= self.pending_fanout[idx].fanout_req_ids.len() {
                        self.pending_fanout.swap_remove(idx);
                        // The master row may still be parked awaiting its
                        // schedule. Ending here would order the end before
                        // the row, so the pair carries it — the same way the
                        // single-exchange case above hands the end over.
                        match self.pending_schedule_pair.iter_mut()
                            .find(|p| p.api_req_id == api_req_id)
                        {
                            Some(pair) => pair.is_last = true,
                            None => {
                                shared.reference.push_contract_details_end(api_req_id);
                                emit(event_tx, Event::ContractDetailsEnd(api_req_id));
                            }
                        }
                    }
                    let rules = crate::control::contracts::parse_market_rules(msg);
                    if !rules.is_empty() {
                        shared.reference.push_market_rules(rules);
                    }
                    return;
                }

                if let Some(def) = crate::control::contracts::parse_secdef_response(msg, shared.island_for_nasdaq()) {
                    let is_last_wire = crate::control::contracts::secdef_response_is_last(msg);
                    if def.con_id != 0 {
                        self.note_what_it_is_written_on(&def, shared, ccp_conn, hb);
                        shared.reference.cache_definition(
                            def.con_id as i64,
                            // Mapped where every other reader of a
                            // definition maps it. Written out here, an
                            // option was cached without its strike, its
                            // right, its expiry or its multiplier — which
                            // is all that tells two options on one
                            // underlying apart.
                            crate::types::model::ContractDetails::from_definition(&def).contract,
                        );
                        identify_position(shared, &def);
                        self.try_release_scanner_enrichments(def.con_id as i64, shared);
                        // A subscription held back for want of an id. Answered
                        // here rather than alongside the caller-facing rows: a
                        // by-symbol lookup fans out and its rows do not all
                        // take the same path, and this one only needs the first
                        // definition that names the contract.
                        if let Some(rid) = response_req_id.as_ref().and_then(|r| r.parse::<u32>().ok())
                            && let Some(at) = self.pending_md_subscribe.iter().position(|(pid, ..)| *pid == rid)
                        {
                            let (_, pending, _) = self.pending_md_subscribe.remove(at);
                            // A symbol stated without a currency is answered
                            // with every listing that carries it. Sent for the
                            // one this reply happened to state last, the caller
                            // was subscribed to a contract it did not name and
                            // told nothing; a lookup of the same description is
                            // refused as naming none.
                            if listings > 1 {
                                Self::abandon_subscribe(pending, listings, context, shared);
                            } else {
                                self.resolved_md_subscribe.push((def.con_id as i64, pending));
                            }
                        }
                        // And a request held for the same reason.
                        if let Some(rid) = response_req_id.as_ref().and_then(|r| r.parse::<u32>().ok())
                            && let Some(at) = self.pending_named.iter().position(|(pid, ..)| *pid == rid)
                        {
                            let (_, mut cmd, _) = self.pending_named.remove(at);
                            if listings > 1 {
                                Self::abandon_named(&cmd, listings, shared);
                            } else {
                                name_the_contract(&mut cmd, def.con_id as i64);
                                self.resolved_named.push(cmd);
                            }
                        }
                    }
                    // Match the response to its originating pending_secdef entry
                    // by tag 320 (response_req_id). Without this, an internal
                    // auto-fetch reply (e.g. position-driven secdef for SPY)
                    // landing while a user request is in flight would be
                    // attributed to `pending_secdef.first()` and leak as a
                    // bogus contract_details callback on the user's req_id.
                    let matched_idx: Option<usize> = response_req_id.as_ref()
                        .and_then(|rid| rid.parse::<u32>().ok())
                        .and_then(|rid_u32| {
                            self.pending_secdef.iter().position(|(pid, _, _)| *pid == rid_u32)
                        });
                    let single_shot = matched_idx
                        .map(|i| self.pending_secdef[i].1).unwrap_or(false);
                    let is_by_symbol = matched_idx
                        .map(|i| !self.pending_secdef[i].1).unwrap_or(false);
                    let is_last = is_last_wire || single_shot;
                    // Fan-out detection: by-symbol master reply carries the full
                    // exchange list in tag 6046. Drop SMART/BEST and dispatch
                    // one per-exchange `35=c` per remaining entry. The per-
                    // exchange replies arrive on new req_ids and route through
                    // the `pending_fanout` branch above.
                    let fanout_exchanges: Vec<String> = if is_by_symbol && !is_last_wire {
                        def.valid_exchanges.iter()
                            .filter(|e| !matches!(e.as_str(), "" | "SMART" | "BEST"))
                            .cloned()
                            .collect()
                    } else {
                        Vec::new()
                    };
                    if let Some(idx) = matched_idx {
                        let req_id = self.pending_secdef[idx].0;
                        // Internal sentinel req_ids (auto-fetch for cold-cache
                        // positions, scanner enrichment) start at 0xF000_0000.
                        // Their replies must populate the contract cache but
                        // never surface as user-visible contract_details
                        // callbacks.
                        let is_internal = req_id >= 0xF000_0000;
                        let join_key = def.join_key.clone();
                        if is_last {
                            self.pending_secdef.remove(idx);
                        } else {
                            // The venue is mid-answer, so the bound is on how
                            // long it stays quiet, not on how long the answer
                            // runs: a class naming every expiry sends for
                            // longer than one contract does, and cut at a
                            // fixed total it reads as a class the venue does
                            // not carry. Same rule the fan-out leg keeps.
                            self.pending_secdef[idx].2 = Instant::now() + unanswered_after(req_id);
                        }
                        let con_id = def.con_id as i64;
                        if con_id == 0 {
                            // Con_id=0 is the gateway saying "no
                            // security definition", not a contract. Pushed as
                            // a row it is indistinguishable from a hit —
                            // empty symbol, and min_tick carrying its 0.01
                            // default. Report it the way the reject and
                            // timeout paths do. The by-symbol leg gets its
                            // end from the fan-out branch below.
                            // The same reply also answers a symbol the gateway
                            // cannot resolve, which arrives contract-less (live:
                            // "BRK.A" for the "BRK A" listing). Drop the pending
                            // entry so the single-shot leg is not left parked
                            // waiting for a definition that will not come.
                            self.pending_secdef.retain(|(rid, ss, _)| *rid != req_id || *ss);
                            if !is_internal {
                                shared.reference.push_historical_error(
                                    req_id, 200,
                                    "No security definition has been found for the request".to_string(),
                                );
                                // Unconditional: the pending entry was dropped
                                // just above, so the fan-out branch below can no
                                // longer supply the end for the by-symbol leg and
                                // a caller blocked on it would wait forever.
                                shared.reference.push_contract_details_end(req_id);
                                emit(event_tx, Event::ContractDetailsEnd(req_id));
                            } else {
                                self.abandon_holders_of(req_id, context, shared);
                            }
                        } else if join_key.is_empty() {
                            // No join key — emit immediately without schedule data.
                            if !is_internal {
                                if self.details_delivered.entry(req_id).or_default().insert(def.con_id as i64) {
                                    let for_event = clone_for_event(event_tx, &def);
                                    shared.reference.push_contract_details(req_id, def);
                                    if let Some(details) = for_event {
                                        emit(event_tx, Event::ContractDetails { req_id, details: Box::new(details) });
                                    }
                                }
                                // The end is the lookup's, not the row's: a
                                // number reused for a contract it was already
                                // handed got neither, and waited out its
                                // deadline.
                                if is_last {
                                    shared.reference.push_contract_details_end(req_id);
                                    emit(event_tx, Event::ContractDetailsEnd(req_id));
                                }
                            }
                        } else if is_internal {
                            // Skip schedule pairing for internal sentinels — no
                            // user is awaiting the trading_hours enrichment.
                        } else {
                            self.pending_schedule_pair.push(PendingSchedulePair {
                                api_req_id: req_id,
                                join_key: join_key.clone(),
                                def,
                                is_last,
                                deadline: Instant::now() + std::time::Duration::from_secs(3),
                            });
                            self.send_schedule_subscribe(&join_key, ccp_conn, hb);
                        }
                        // Dispatch fan-out (or fire end immediately if the
                        // symbol resolves to a single exchange and there's
                        // nothing to fan out to).
                        if is_by_symbol && !is_last_wire && con_id != 0 {
                            self.pending_secdef.retain(|(rid, ss, _)| *rid != req_id || *ss);
                            if is_internal {
                                // A naming lookup of the engine's own needed
                                // the one definition that names the contract,
                                // released above, and nothing after it. Asked
                                // per venue as a caller's lookup is, its rows
                                // and its end reached the wrapper under a
                                // number nobody asked with.
                            } else if fanout_exchanges.is_empty() {
                                // The master row may be parked awaiting its
                                // schedule pair; firing end now would order
                                // end BEFORE the row. Defer it to
                                // the pair's resolution (or its 3s sweep).
                                if let Some(pair) = self.pending_schedule_pair.iter_mut()
                                    .find(|p| p.api_req_id == req_id)
                                {
                                    pair.is_last = true;
                                } else {
                                    shared.reference.push_contract_details_end(req_id);
                                    emit(event_tx, Event::ContractDetailsEnd(req_id));
                                }
                            } else {
                                let mut fanout_req_ids = Vec::with_capacity(fanout_exchanges.len());
                                for exch in &fanout_exchanges {
                                    let fid = format!("ibxfan-{}-{}", req_id, self.next_fanout_id);
                                    self.next_fanout_id = self.next_fanout_id.wrapping_add(1);
                                    let fix_exch = if exch == "SMART" { "BEST" } else { exch.as_str() };
                                    self.send_fanout_secdef_request(&fid, con_id, fix_exch, ccp_conn, hb);
                                    fanout_req_ids.push(fid);
                                }
                                log::info!(
                                    "Secdef by-symbol fan-out: api_req_id={} con_id={} exchanges={}",
                                    req_id, con_id, fanout_req_ids.len(),
                                );
                                self.pending_fanout.push(PendingFanout {
                                    api_req_id: req_id,
                                    fanout_req_ids,
                                    answered: Vec::new(),
                                    deadline: Instant::now() + SECDEF_TIMEOUT,
                                });
                            }
                        }
                    }
                }
                let rules = crate::control::contracts::parse_market_rules(msg);
                if !rules.is_empty() {
                    shared.reference.push_market_rules(rules);
                }
            }
            // A message type nothing here reads. Named once, like an unread
            // user message: the out-of-band types carry position and account
            // data, and the vendor's own client treats an unrecognised one as
            // an error rather than as nothing.
            other => {
                if self.unread_types.insert(other.to_string()) {
                    shared.market.note_unread_wire("trading", format!("type {other}"));
                    log::info!(
                        "Unread message: type {other}, {} bytes. Nothing here reads it, so \
                         whatever it carries is being discarded",
                        msg.len(),
                    );
                }
            }
        }
        // Every contract the messages above registered is counted where the
        // API reads the count. A recovery record registers the contract it
        // names, and only a subscription refreshed this mirror — so an account
        // whose working orders were on contracts this session never subscribed
        // to counted none of them, and a global cancel, composed by instrument
        // from the count, withdrew nothing and returned without an error.
        shared.market.set_instrument_count(context.market.count());
    }

    fn handle_news_bulletin(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        // The urgency the venue states, and the kind a caller is told about.
        // They are not the same numbering and they are not in the same order:
        // the venue's second kind is an exchange that has stopped trading and
        // its third is one that has started, while a caller reads those the
        // other way round. Passed straight through, a caller halting on an
        // exchange going down acted on one coming up. The last three are
        // kinds of their own — plain text, a message meant to be shown, and
        // one written as markup — and were all reported as ordinary news.
        static BULLETIN_TYPE_MAP: &[(i32, i32)] = &[
            (1, 1), (2, 3), (3, 2), (8, 4), (9, 5), (10, 6),
        ];
        /// The kind that claims least about what a bulletin is: text, meant to
        /// be read. What an urgency this does not name goes out under.
        const BULLETIN_PLAIN_TEXT: i32 = 4;
        let fix_type: i32 = parsed.get(&fix::TAG_URGENCY)
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        let api_type = BULLETIN_TYPE_MAP.iter()
            .find(|(k, _)| *k == fix_type)
            .map(|(_, v)| *v);
        let api_type = match api_type {
            Some(t) => t,
            None => {
                // Delivered as plain text, and the urgency recorded. A bulletin
                // whose urgency this does not name is still a bulletin the
                // venue sent, and dropping it here threw away the headline with
                // the number nobody could read — which is the only part a
                // person reads. The kind it goes out under is the one that
                // claims least: the venue said something, and this does not
                // know what sort of something.
                shared.market.note_unread_wire(
                    "trading",
                    format!("news bulletin urgency {fix_type}"),
                );
                log::warn!(
                    "news bulletin states urgency {fix_type}, which names no bulletin type \
                     here — delivered as plain text",
                );
                BULLETIN_PLAIN_TEXT
            }
        };
        let message = parsed.get(&fix::TAG_HEADLINE).cloned().unwrap_or_default();
        let exchange = parsed.get(&fix::TAG_SECURITY_EXCHANGE).cloned().unwrap_or_default();
        // The venue numbers its own bulletins and states the number here.
        // Counted locally instead, the numbering started again at every
        // connect and named nothing the venue would recognise, so the same
        // bulletin arriving twice across a reconnect could not be told from
        // two. Absent, it stands at the widest number a bulletin id is
        // carried under, which is what says nothing was stated.
        let msg_id = parsed.get(&fix::TAG_BULLETIN_ID)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(i32::MAX);
        let bulletin = NewsBulletin {
            msg_id,
            msg_type: api_type,
            message,
            exchange,
        };
        shared.market.push_news_bulletin(bulletin);
    }

    /// The account's cash, by currency.
    ///
    /// Tag 6566 counts the entries that follow and each is a currency on tag 15
    /// with its balance on 9806. Read as a single number under a selector, this
    /// took the last balance on the frame — the one belonging to whichever
    /// currency came last — and published it as net liquidation, so an account
    /// holding the better part of a million in cash reported a net liquidation
    /// of minus fourteen hundred.
    ///
    /// The account states these balances under names of their own, so there is
    /// nothing here a caller does not already have. That is what this session
    /// was sent and read back against the account; it is not a claim that no
    /// account, in no state, ever states more on this frame.
    fn handle_account_summary(&mut self, parsed: &std::collections::HashMap<u32, String>, _shared: &SharedState) {
        // Tag 6566 counts the entries that follow, and each is a currency on
        // tag 15 with its cash balance on 9806. Captured whole from a live
        // session:
        //
        //     6040=77 1=DU… 6566=3 15=BASE 9806=1492751.1917
        //                          15=GBP  9806=1105714.0700
        //                          15=USD  9806=0.0000
        //
        // and the sterling figure is this account's `TotalCashBalance` in
        // sterling to the penny. So the frame states what the account already
        // states under names, and there is nothing here a caller does not have.
        //
        // Read as a kind rather than a count, it had this client publishing
        // "an account figure of kind 3" whose value was the last of the three —
        // a number belonging to whichever currency happened to be last on the
        // wire, under a label that named nothing.
        let _ = parsed;
        // Nothing is written from here, so nothing is published from here
        // either. Writing the unchanged snapshot back had one effect: it
        // raised the flag that says this connection has stated the account.
        // A frame this function's own doc records as unread then marked a
        // pre-drop snapshot current on a rebuilt connection.
    }

    /// Subscribe to the schedule paired with a secdef reply, joined on tag 6256.
    /// Internal subscription (no API-client req_id exposed); reply arrives as
    /// 35=U|6040=107 and is matched back to the secdef via 6256.
    fn send_schedule_subscribe(
        &mut self,
        join_key: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let sub_id = self.next_schedule_sub_id;
            self.next_schedule_sub_id += 1;
            let sub_id_str = format!("SchedSub.{sub_id}");
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SUB_PROTOCOL,
                    crate::control::contracts::SUB_PROTOCOL_SCHEDULE_SUBSCRIBE),
                (320, &sub_id_str),
                (crate::control::contracts::TAG_SCHEDULE_JOIN_KEY, join_key),
            ]);
            hb.last_ccp_sent = Instant::now();
        }
    }

    /// Drop pending schedule pairs past their deadline, emitting partial details.
    /// Fail contract-details requests whose deadline has passed:
    /// both plain/by-symbol secdef lookups the gateway never answered and
    /// by-symbol fan-outs missing one or more per-exchange replies. On
    /// expiry the caller gets error 200 plus contract_details_end, so a
    /// blocked wait unblocks with no API change. Internal sentinel req_ids
    /// (>= 0xF000_0000: cache auto-fetch, scanner enrichment) are dropped
    /// silently — no user is waiting on them.
    /// How long a subscription waits for the lookup that would name its
    /// contract.
    ///
    /// Between two deadlines, and it has to stay between them. Longer than the
    /// lookup's own, so a definition or a refusal from the venue is preferred
    /// to this. Shorter than the caller's, because for a held request this is
    /// the only report there is: the lookup behind it is asked under an
    /// internal id, and an internal id is dropped silently. Sitting past the
    /// caller's wait, as it did, meant the caller was told nothing arrived
    /// while the reason was still being held, and heard it never.
    const NAMING_TIMEOUT: Duration =
        Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS - 3);

    /// A subscription whose contract the venue never named. Reported rather
    /// than left waiting: silence is what this whole path exists to remove.
    pub(crate) fn sweep_pending_subscribes(
        &mut self, context: &mut Context, shared: &SharedState,
    ) {
        if self.pending_md_subscribe.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut gave_up = Vec::new();
        self.pending_md_subscribe.retain(|(_, pending, asked_at)| {
            if now.duration_since(*asked_at) < Self::NAMING_TIMEOUT {
                return true;
            }
            gave_up.push(pending.clone());
            false
        });
        for p in gave_up {
            Self::abandon_subscribe(p, 0, context, shared);
        }
    }

    /// A subscription that cannot be made for the contract described, with
    /// how many listings the venue named for it: the caller told why, and the
    /// slot the registration took offered back. Nothing else will ask for it:
    /// a caller told the venue knows no such contract has no reason to
    /// withdraw a subscription that never opened, and until now a chain
    /// naming a few dead strikes spent one slot on each until the table ran
    /// out.
    fn abandon_subscribe(p: PendingSubscribe, listings: usize, context: &mut Context, shared: &SharedState) {
        let reason = unnamed(
            &p.sec_type, &p.symbol, &p.exchange, listings,
            "no market data subscription could be made for it",
        );
        log::warn!("Subscription abandoned: {reason}");
        shared.market.push_subscription_failure(p.instrument, reason);
        context.slots_to_reconsider.push(p.instrument);
    }

    /// A held request that cannot be sent for the contract described, told to
    /// its caller under its own number. Nothing the venue holds matches the
    /// contract described, which is what the code says word for word.
    fn abandon_named(cmd: &crate::types::ControlCommand, listings: usize, shared: &SharedState) {
        let Some(named) = contract_named(cmd) else { return };
        let reason = unnamed(
            &named.sec_type, &named.symbol, &named.exchange, listings,
            "the request could not be sent",
        );
        log::warn!("Request abandoned: {reason}");
        if let Some(req_id) = request_id(cmd) {
            super::push_hmds_refusal(
                shared, req_id, crate::error_codes::Refusal::NO_DEFINITION, reason,
                matches!(cmd, crate::types::ControlCommand::FetchHistorical { .. }),
            );
        }
    }

    /// End what waited on a naming lookup of the engine's own that the venue
    /// answered with no definition. Dropped with the lookup alone, the
    /// subscription or request learnt of it only when its own wait ran out.
    fn abandon_holders_of(&mut self, req_id: u32, context: &mut Context, shared: &SharedState) {
        if let Some(at) = self.pending_md_subscribe.iter().position(|(pid, ..)| *pid == req_id) {
            let (_, p, _) = self.pending_md_subscribe.remove(at);
            Self::abandon_subscribe(p, 0, context, shared);
        }
        if let Some(at) = self.pending_named.iter().position(|(pid, ..)| *pid == req_id) {
            let (_, cmd, _) = self.pending_named.remove(at);
            Self::abandon_named(&cmd, 0, shared);
        }
    }

    pub(crate) fn sweep_contract_details(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        // A schedule pairing delivers after the lookup that asked for it has
        // been retired, so the record of what a request was already handed has
        // to outlive both. Dropping it at the lookup's end let the venue copy
        // through behind the one carrying the trading hours.
        let idle = self.pending_secdef.is_empty()
            && self.pending_fanout.is_empty()
            && self.pending_schedule_pair.is_empty();
        if idle {
            self.details_delivered.clear();
        }
        if self.pending_secdef.is_empty() && self.pending_fanout.is_empty() {
            return;
        }
        let now = Instant::now();
        // A session that has ended answers nothing, so every request waiting on
        // it is already finished — waiting out its deadline only delays the
        // caller learning that, once per request.
        let over = shared.reference.session_over();
        let mut expired: Vec<u32> = Vec::new();
        let mut lost_auto_fetch: Vec<u32> = Vec::new();
        self.pending_secdef.retain(|(req_id, _, deadline)| {
            if now >= *deadline || over.is_some() {
                if *req_id < 0xF000_0000 {
                    expired.push(*req_id);
                } else {
                    log::warn!("Internal secdef timeout: req_id={req_id:#x}");
                    // Forgotten, so the next report naming that contract asks
                    // again. Held, one lost request leaves the contract unnamed
                    // for the life of the session.
                    lost_auto_fetch.push(*req_id);
                }
                false
            } else {
                true
            }
        });
        if !lost_auto_fetch.is_empty() {
            self.auto_fetched_conids.retain(|_, rid| !lost_auto_fetch.contains(rid));
        }
        self.pending_fanout.retain(|p| {
            if now >= p.deadline || over.is_some() {
                log::warn!(
                    "Contract-details fan-out timeout: api_req_id={} received {} of {}",
                    p.api_req_id, p.answered.len(), p.fanout_req_ids.len(),
                );
                expired.push(p.api_req_id);
                false
            } else {
                true
            }
        });
        for req_id in expired {
            let (code, why) = match over {
                Some(reason) => (
                    crate::error_codes::Refusal::NOT_CONNECTED,
                    format!("the session is over: {reason}"),
                ),
                None => (
                    crate::error_codes::Refusal::NO_ANSWER,
                    "contract details request timed out — no reply from the gateway".to_string(),
                ),
            };
            log::warn!("Contract-details unanswered: req_id={req_id} ({why})");
            shared.reference.push_historical_error(req_id, code, why);
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
    }

    pub(crate) fn sweep_pending_schedule_pairs(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        let now = Instant::now();
        let mut emit_now: Vec<PendingSchedulePair> = Vec::new();
        self.pending_schedule_pair.retain(|p| {
            if now >= p.deadline {
                let mut def = p.def.clone();
                def.trading_hours = None;
                def.liquid_hours = None;
                def.time_zone_id = None;
                emit_now.push(PendingSchedulePair {
                    api_req_id: p.api_req_id,
                    join_key: p.join_key.clone(),
                    def,
                    is_last: p.is_last,
                    deadline: p.deadline,
                });
                log::warn!("Schedule pair timeout: api_req_id={} join_key={}",
                    p.api_req_id, p.join_key);
                false
            } else {
                true
            }
        });
        for p in emit_now {
            // Same gate as every other way a contract reaches the caller: the
            // venue fan-out describes one contract many times over.
            if !self.details_delivered.entry(p.api_req_id).or_default().insert(p.def.con_id as i64) {
                if p.is_last {
                    shared.reference.push_contract_details_end(p.api_req_id);
                    emit(event_tx, Event::ContractDetailsEnd(p.api_req_id));
                }
                continue;
            }
            let for_event = clone_for_event(event_tx, &p.def);
            shared.reference.push_contract_details(p.api_req_id, p.def);
            if let Some(details) = for_event {
                emit(event_tx, Event::ContractDetails { req_id: p.api_req_id, details: Box::new(details) });
            }
            if p.is_last {
                shared.reference.push_contract_details_end(p.api_req_id);
                emit(event_tx, Event::ContractDetailsEnd(p.api_req_id));
            }
        }
    }

    /// Match a 6040=107 schedule reply to a pending secdef pair and emit merged
    /// details.
    fn handle_schedule_reply(
        &mut self,
        msg: &[u8],
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        // Extract 6256 from the reply to locate the matching pair.
        let join_key = match extract_tag_value(msg, b"6256=") {
            Some(v) => v,
            None => return,
        };
        let pos = match self.pending_schedule_pair.iter().position(|p| p.join_key == join_key) {
            Some(p) => p,
            None => return,
        };
        let mut pair = self.pending_schedule_pair.swap_remove(pos);
        if let Some(sched) = crate::control::contracts::parse_schedule_response(msg) {
            pair.def.time_zone_id = if sched.timezone.is_empty() {
                None
            } else {
                Some(sched.timezone.clone())
            };
            // On the clock the venue names beside them. Where that zone is
            // one no database here answers to, the hours stay as the wire
            // carried them and the zone is reported as the UTC they are on,
            // so a caller is never given a name the times do not match.
            let named = sched.timezone.as_str();
            let stated_on_the_named_clock =
                crate::control::contracts::sessions_are_stated_on(named);
            if !stated_on_the_named_clock {
                pair.def.time_zone_id = Some("UTC".to_string());
            }
            pair.def.trading_hours = Some(
                crate::control::contracts::format_sessions_string(&sched.trading_hours, named)
            );
            pair.def.liquid_hours = Some(
                crate::control::contracts::format_sessions_string(&sched.liquid_hours, named)
            );
        }
        let for_event = clone_for_event(event_tx, &pair.def);
        // The schedule reply completes the pairing, and this is where the row
        // that carries the trading hours reaches the caller. Same gate as every
        // other path: one contract, delivered once.
        if !self.details_delivered.entry(pair.api_req_id).or_default().insert(pair.def.con_id as i64) {
            if pair.is_last {
                shared.reference.push_contract_details_end(pair.api_req_id);
                emit(event_tx, Event::ContractDetailsEnd(pair.api_req_id));
            }
            return;
        }
        shared.reference.push_contract_details(pair.api_req_id, pair.def);
        if let Some(details) = for_event {
            emit(event_tx, Event::ContractDetails { req_id: pair.api_req_id, details: Box::new(details) });
        }
        if pair.is_last {
            shared.reference.push_contract_details_end(pair.api_req_id);
            emit(event_tx, Event::ContractDetailsEnd(pair.api_req_id));
        }
    }

    /// What the venue answers an advisor request with.
    ///
    /// The reply states the number the request went out under and nothing else
    /// about what was asked, so what the caller is owed is read from what was
    /// remembered when the question left. A question is answered with the
    /// configuration itself; a replacement with its end, or, where the venue
    /// states trouble, with that trouble under the code the reference client
    /// reports a failed replacement on.
    ///
    /// A reply carrying no number at all is the venue volunteering a change to
    /// a model nobody asked about. Nothing here asked for it and no caller is
    /// waiting on it, so it is logged rather than delivered to whichever
    /// request happens to be open.
    fn handle_advisor_config(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        shared: &SharedState,
    ) {
        let Some(key) = parsed.get(&6158).filter(|k| !k.is_empty()) else {
            log::info!("The venue stated an advisor configuration nobody asked for");
            return;
        };
        let Some(asked) = self.pending_advisor.remove(key.as_str()) else {
            log::warn!("The venue answered advisor request {key}, which nothing here asked");
            return;
        };
        // Tag 58 is the venue's own account of what went wrong. Empty is the
        // only shape that means the request stands.
        let trouble = parsed.get(&58).map(String::as_str).unwrap_or("");
        if !trouble.is_empty() {
            log::warn!("The venue refused advisor request {key}: {trouble}");
            shared.reference.push_advisor_refused(
                asked.req_id, ADVISOR_SAVE_REFUSED, trouble.to_string(),
            );
            return;
        }
        if asked.replacing {
            shared.reference.push_advisor_replaced(asked.req_id, ADVISOR_SAVED.to_string());
            return;
        }
        // A partition the venue holds nothing for is stated as nothing rather
        // than left out, and an advisor with no groups is an answer: delivered
        // empty, the caller learns there are none; dropped, they wait.
        let document = parsed.get(&6118).cloned().unwrap_or_default();
        shared.reference.push_advisor_config(asked.fa_data_type, document);
    }

    /// Ask for, or replace, the advisor's own configuration.
    ///
    /// An advisor's groups, allocation profiles and models are held by the
    /// venue, not by this client, and are asked for one partition at a time.
    /// The command says which of asking, replacing or removing is meant; a
    /// replacement carries the configuration as its own document.
    ///
    /// An account that is not an advisor's holds none of this, and the venue
    /// says so rather than answering with an empty one.
    pub(crate) fn send_advisor_config(
        &mut self,
        req_id: i64,
        command: i32,
        partition: &str,
        fa_data_type: i32,
        document: Option<&str>,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        // Refused where it cannot be sent, as every sibling request on this
        // connection is. Nothing is recorded for a request that never went
        // out, and the connection that replaces this one is asked nothing
        // this one was — so there is no later moment at which an answer
        // arrives. Silent, a caller reading a partition waited for ever, and
        // one replacing a partition held a document it believed the venue had
        // taken.
        let Some(conn) = ccp_conn.as_mut() else {
            log::warn!("Advisor configuration request req_id={req_id} not sent: no CCP transport");
            shared.reference.push_advisor_refused(
                req_id, ADVISOR_SAVE_REFUSED,
                "the advisor configuration request could not be sent: no connection to the \
                 venue".to_string(),
            );
            return;
        };
        let ts = chrono_free_timestamp();
        let command = command.to_string();
        // Which partition, and which request. The partition rides 6906 and
        // 6158 is the request's own number, counted from one for the
        // session and sent as a string, which the reply carries back so an
        // answer can be matched to its question. Writing the partition
        // into 6158 and omitting 6906 leaves every advisor request naming
        // no partition, so a replacement carries a document for a partition
        // none of them named. The two are written in this order.
        let key = self.next_advisor_request.to_string();
        self.next_advisor_request = self.next_advisor_request.wrapping_add(1);
        let mut fields: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "116"),
            (6905, &command),
            (6158, &key),
            (6906, partition),
        ];
        // Only a replacement carries a document; asking for one that states
        // a document would be asking and telling at once.
        if let Some(xml) = document {
            fields.push((6118, xml));
        }
        let _ = conn.send_fix(&fields);
        // Held before the frame is called sent: the reply states only this
        // number, so a question nobody remembers asking is a reply nobody
        // can be given.
        self.pending_advisor.insert(
            key.clone(),
            PendingAdvisor {
                req_id,
                fa_data_type,
                replacing: document.is_some(),
                deadline: Instant::now() + ADVISOR_TIMEOUT,
            },
        );
        hb.last_ccp_sent = Instant::now();
        log::info!("Sent advisor configuration request: command={command} partition={partition}");
    }

    /// Ask the venue to state the account's figures now.
    ///
    /// The same pair the connection sends when it re-establishes itself: the
    /// keyed account request on 6040=6, and the display request on 6040=91 that
    /// carries the positions beside it. Subscribing alone does not produce
    /// them — the venue restates them on its own schedule, and a session that
    /// has just opened waits tens of seconds for its first set.
    pub(crate) fn send_account_refresh(
        &mut self,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        let Some(conn) = ccp_conn.as_mut() else { return };
        let ts = chrono_free_timestamp();
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "91"),
            (1, account),
            (6556, "DR.1"),
            (6712, "1"),
        ]);
        // The key is unique for the life of the process, not of this state.
        // The venue keys the subscription on 6529 and answers a key it is
        // already serving with nothing; a connection outlives the loops that
        // use it, so a counter reset with each loop asks under a key the
        // connection has already seen and is not answered at all. The opening
        // sequence has used AR.1.
        let key = self.next_account_request_key();
        shared.portfolio.holdings_restated_under(&key);
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "6"),
            (6036, "1"),
            (6095, account),
            (6529, &key),
        ]);
        hb.last_ccp_sent = Instant::now();
    }

    /// The next key to ask for account and position data under, recorded as
    /// the one this state now holds.
    ///
    /// Unique for the life of the process, not of this state. The venue keys
    /// the subscription on 6529 and answers a key it is already serving with
    /// nothing, and a connection outlives the loops that use it — so every
    /// place that asks draws from here rather than naming a key of its own.
    /// A reconnect that named one directly asked under a key a refresh had
    /// already spent, was answered with nothing, and the position pushes did
    /// not resume.
    fn next_account_request_key(&mut self) -> String {
        static NEXT_ACCOUNT_REQUEST: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(2);
        let n = NEXT_ACCOUNT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = format!("AR.{n}");
        self.account_request_key = Some(key.clone());
        key
    }

    /// life of the connection and the venue stops answering new ones.
    /// Send P&L subscribe: 6040=142, 6529=PLR.{N}, 1={account}.
    /// Close the account subscription this state opened.
    ///
    /// Tag 6036 states whether the request opens the subscription or closes it,
    /// and the key on 6529 names which. Left open, each loop holds one for the
    /// life of the connection and the venue stops answering new ones.
    pub(crate) fn send_account_unsubscribe(
        &mut self,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let Some(key) = self.account_request_key.take() else { return };
        let Some(conn) = ccp_conn.as_mut() else { return };
        let ts = chrono_free_timestamp();
        let _ = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "6"),
            (6036, "0"),
            (6095, account),
            (6529, &key),
        ]);
        hb.last_ccp_sent = Instant::now();
    }

    pub(crate) fn send_pnl_subscribe(
        &mut self,
        req_id: i64,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // Recorded whether or not the transport is up: a rebuilt connection
        // asks for every standing subscription again.
        self.pnl_subscriptions.retain(|(id, _)| *id != req_id);
        self.pnl_subscriptions.push((req_id, account.to_string()));
        if let Some(conn) = ccp_conn.as_mut() {
            // The key names the request; the account rides tag 1 beside it, as
            // the protocol defines it and as the opening sequence in
            // `logon::send_post_burst_grace` already wrote it. Written into the
            // key instead, the account rides as literal bytes inside a field
            // that names a request, and tag 1 is not sent at all.
            let pnl_key = format!("PLR.{req_id}");
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "142"),
                (6529, &pnl_key),
                (1, account),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent P&L subscribe: req_id={req_id} account={account}");
        }
    }

    /// Forget a profit-and-loss subscription, so a reconnect does not renew it.
    ///
    /// Nothing withdraws one at the venue on this wire; what stops is the
    /// renewal, and the reporting once the session ends.
    pub(crate) fn withdraw_pnl_subscription(&mut self, req_id: i64) {
        self.pnl_subscriptions.retain(|(id, _)| *id != req_id);
    }

    /// Say goodbye before going.
    ///
    /// A session dropped without this is one the venue has to time out, and
    /// this account permits only one at a time: the next connection then races
    /// a session the venue still believes is live. The vendor's own client
    /// sends it, and states why it is going.
    pub(crate) fn send_logout(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let Some(conn) = ccp_conn.as_mut() else { return };
        let ts = chrono_free_timestamp();
        let sent = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, fix::MSG_LOGOUT),
            (fix::TAG_SENDING_TIME, &ts),
            // The vendor states a reason here; "S" is what it sends when the
            // session is being shut down rather than lost.
            (8372, "S"),
        ]);
        if sent.is_ok() {
            hb.last_ccp_sent = Instant::now();
            log::info!("Logout sent");
        }
    }

    pub(crate) fn send_secdef_request(&mut self, req_id: u32, con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // A lookup sent afresh under a number forgets what that number was
        // handed before, as the lookup by symbol does.
        self.details_delivered.remove(&req_id);
        let sent = match ccp_conn.as_mut() {
            Some(conn) => {
                let con_id_str = con_id.to_string();
                let req_id_str = req_id.to_string();
                let ts = chrono_free_timestamp();
                conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, "c"),
                    (fix::TAG_SENDING_TIME, &ts),
                    (crate::control::contracts::TAG_SECURITY_REQ_ID, &req_id_str),
                    (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                    (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                    (crate::control::contracts::TAG_IB_SOURCE, "Socket"),
                ])
                .map_err(|e| e.to_string())
            }
            None => Err("no connection to the venue".to_string()),
        };
        match sent {
            Ok(()) => {
                log::info!("Sent secdef request: req_id={req_id} con_id={con_id}");
                hb.last_ccp_sent = Instant::now();
            }
            Err(why) => {
                if Self::refuse_unsent_lookup(req_id, &why, shared) {
                    return;
                }
            }
        }
        // Known-conId lookup: single record, no paginated terminator.
        self.pending_secdef.push((req_id, true, Instant::now() + unanswered_after(req_id)));
    }

    /// A caller's lookup that did not reach the venue, refused now rather
    /// than queued. Queued as if sent, it was reported twenty seconds later as
    /// a request the venue did not answer, when the venue never received it,
    /// while a matching-symbols or option-chain request in the same state is
    /// refused at once. A lookup of the engine's own is queued all the same
    /// and answers `false`: what waits on it is told by its own sweep.
    fn refuse_unsent_lookup(req_id: u32, why: &str, shared: &SharedState) -> bool {
        if req_id >= crate::bridge::ENGINE_ID_BASE {
            log::warn!("secdef request req_id={req_id:#x} queued unsent: {why}");
            return false;
        }
        log::warn!("Contract details request req_id={req_id} not sent: {why}");
        shared.reference.push_historical_error(
            req_id, crate::error_codes::Refusal::NOT_CONNECTED,
            format!("contract details request could not be sent: {why}"),
        );
        shared.reference.push_contract_details_end(req_id);
        true
    }

    /// Ask the venue to name a contract so a subscription can be sent for it.
    pub(crate) fn resolve_for_subscribe(
        &mut self,
        pending: PendingSubscribe,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        let filters = pending.filters.clone();
        let (symbol, sec_type, exchange, currency) = (
            pending.symbol.clone(), pending.sec_type.clone(),
            pending.exchange.clone(), pending.currency.clone(),
        );
        let con_id = pending.con_id;
        let instrument = pending.instrument;
        let outcome = if con_id != 0 {
            self.send_secdef_request(req_id, con_id, ccp_conn, hb, shared);
            Ok(())
        } else {
            self.send_secdef_request_by_symbol(
                req_id, &symbol, &sec_type, &exchange, &currency, &filters, ccp_conn, hb, shared,
            )
        };
        match outcome {
            Ok(()) => self.pending_md_subscribe.push((req_id, pending, Instant::now())),
            Err(reason) => {
                log::warn!("subscription lookup refused: {reason}");
                shared.market.push_subscription_failure(instrument, reason);
            }
        }
    }

    /// Ask the venue to name the contract a request wants, and hold the
    /// request until it does.
    ///
    /// Answers `false` when the request names nothing that can be looked up,
    /// so the caller sends it as it stands rather than holding it for ever.
    pub(crate) fn hold_until_named(
        &mut self,
        cmd: crate::types::ControlCommand,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) -> Option<crate::types::ControlCommand> {
        // Cloned rather than borrowed: the command is moved onto the pending
        // list below, and what it named has to outlive it.
        let named = match contract_named(&cmd) {
            Some(c) if !c.symbol.is_empty() => c.clone(),
            _ => return Some(cmd),
        };
        let filters = filters_named(&cmd);
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        if let Err(reason) = self.send_secdef_request_by_symbol(
            req_id, &named.symbol, &named.sec_type, &named.exchange, &named.currency,
            &filters, ccp_conn, hb, shared,
        ) {
            // The lookup it would be held for cannot be made, so it is not
            // held: told the reason under its own number, the way the sweep
            // tells a request the venue never named.
            log::warn!("Request abandoned: {reason}");
            if let Some(caller_req_id) = request_id(&cmd) {
                // The lookup this request needs could not be made, which is a
                // fault in the request rather than one the data service had
                // with a query it answered.
                super::push_hmds_refusal(
                    shared, caller_req_id, crate::error_codes::Refusal::VALIDATION, reason,
                    matches!(cmd, crate::types::ControlCommand::FetchHistorical { .. }),
                );
            }
            return None;
        }
        self.pending_named.push((req_id, cmd, Instant::now()));
        None
    }

    /// Withdraw a request that is still waiting to be named.
    ///
    /// A request naming its contract by symbol is parked whole while the
    /// venue is asked what that contract is, and at that moment it is in
    /// neither the in-flight record nor the held one -- so a cancel arriving
    /// in that window found nothing, sent nothing, and returned. The naming
    /// answer then arrived, the request was re-injected and sent, and the
    /// caller was served a full answer to a request it had withdrawn.
    ///
    /// Called from the cancel of every request that can be parked this way.
    /// The second list covers a naming answer re-injected in a pass before the
    /// cancel was read; once the request has been sent it is in the in-flight
    /// record and the ordinary withdrawal reaches it.
    pub(crate) fn withdraw_named(&mut self, req_id: u32) -> bool {
        let before = self.pending_named.len() + self.resolved_named.len();
        self.pending_named.retain(|(_, cmd, _)| request_id(cmd) != Some(req_id));
        self.resolved_named.retain(|cmd| request_id(cmd) != Some(req_id));
        self.pending_named.len() + self.resolved_named.len() != before
    }

    /// A held request whose contract the venue never named. Told to the caller
    /// rather than left waiting.
    pub(crate) fn sweep_pending_named(&mut self, shared: &SharedState) {
        if self.pending_named.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut gave_up = Vec::new();
        self.pending_named.retain(|(_, cmd, asked_at)| {
            if now.duration_since(*asked_at) < Self::NAMING_TIMEOUT {
                return true;
            }
            gave_up.push(cmd.clone());
            false
        });
        for cmd in gave_up {
            Self::abandon_named(&cmd, 0, shared);
        }
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: u32, symbol: &str, sec_type: &str, exchange: &str, currency: &str, filters: &crate::types::SecDefFilters, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) -> Result<(), String> {
        // A public identifier and the tags it rides on. Each kind has its
        // own: a CUSIP goes out as 454=1|455=<id>|456=1, and 22/48 carry an
        // ISIN or a FIGI under the character that names the source rather
        // than a number. Sent as 22=1|48=<cusip>, a CUSIP named a source on
        // the pair that carries an ISIN, and a FIGI was not
        // sent at all — the lookup fell through to whatever the symbol
        // matched. When one is set the lookup rides the identifier and
        // drops the symbol/secType/filters.
        //
        // A kind this wire carries no source for is refused rather than
        // asked by symbol: that is a different question, and its answer
        // would read as the one the caller put.
        let sec_id = filters.sec_id.as_str();
        let identifier_fields: Vec<(u32, &str)> = if sec_id.is_empty() {
            Vec::new()
        } else {
            match filters.sec_id_type.to_uppercase().as_str() {
                "CUSIP" => vec![(454, "1"), (455, sec_id), (456, "1")],
                "ISIN" => vec![(22, "4"), (48, sec_id)],
                "FIGI" => vec![(22, "S"), (48, sec_id)],
                // The caller named an identifier of a kind this client
                // carries no wire source for. Asking by symbol instead is a
                // different question, and answering it under the caller's
                // number hands back whatever the symbol matches — so the
                // lookup is refused rather than made.
                other => {
                    let kind = if other.is_empty() {
                        "an identifier of no stated kind".to_string()
                    } else {
                        format!("a {other} identifier")
                    };
                    return Err(format!(
                        "the lookup names {kind} this client carries no wire \
                         source for, and asked by symbol it would answer a \
                         different question: name the contract by its symbol \
                         or by the venue's id",
                    ));
                }
            }
        };
        let identifier_lookup = !identifier_fields.is_empty();

        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let fix_exchange = if exchange == "SMART" { "BEST" } else { exchange };
            // The protocol has no security type of its own for a continuous
            // future. The contract goes out as a future and a separate field
            // asks for the current lead month instead of a listed one; sent as
            // its own type the whole message is refused. The spelling that
            // asks for the expiring months as well asks for the continuous one
            // the same way — the listed months are a second lookup, which this
            // client does not yet make.
            let continuous_future = matches!(sec_type, "CONTFUT" | "FUT+CONTFUT" | "CONTFUT+FUT");
            // A contract named only by its issuer is answered as fixed income,
            // whatever type the caller stated: the issuer rides a field of its
            // own and the type it is looked up under is not the caller's to
            // choose.
            let issuer_id = filters.issuer_id.as_str();
            let fix_sec_type = if continuous_future {
                "FUT"
            } else if !issuer_id.is_empty() {
                "FIXED"
            } else {
                match sec_type {
                    "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND", other => other,
                }
            };
            // A news feed states its provider where every other contract states
            // a venue, and the protocol carries the provider under a field of
            // its own: without it the message is refused outright. Where the
            // exchange names provider and feed together the feed is the half
            // that is wanted. The provider having moved, neither a venue nor a
            // currency rides with it, and a feed carries no trading class.
            let news_source = if sec_type == "NEWS" {
                match exchange.split(':').collect::<Vec<_>>()[..] {
                    [_, feed] => feed,
                    [provider, ..] => provider,
                    [] => "",
                }
            } else {
                ""
            };
            let strike_str = if filters.strike > 0.0 { format!("{}", filters.strike) } else { String::new() };
            // PutOrCall: Call = 1, Put = 0.
            let right_code = match filters.right.to_uppercase().as_str() {
                "C" | "CALL" => "1",
                "P" | "PUT" => "0",
                _ => "",
            };
            // Exchange rides tag 100; primaryExchange (when set) rides tag 207 —
            // the two were previously conflated onto 207. localSymbol replaces the
            // plain symbol; the derivative/disambiguation filters are added only
            // when set. Captured in.
            let mut fields: Vec<(u32, &str)> = vec![
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &req_id_str),
                (321, "2"),
            ];
            if !news_source.is_empty() {
                fields.push((6825, news_source));
            }
            if identifier_lookup {
                // Identifier lookup: the identifier and its source replace the
                // symbol/secType/filters; exchange and currency still ride.
                fields.extend_from_slice(&identifier_fields);
            } else {
                // Both, where the caller stated both. The protocol carries
                // the symbol and then the venue's local symbol from
                // separate fields of the request, and neither suppresses the
                // other. Sending only
                // the local symbol asked a narrower question than the caller
                // put, and a symbol that disagrees with it — which the venue
                // would refuse — matched whatever the local symbol named.
                if !symbol.is_empty() {
                    fields.push((55, symbol));
                }
                if !filters.local_symbol.is_empty() && !continuous_future {
                    fields.push((6035, &filters.local_symbol));
                }
                // A continuous future is not looked up by its listed months, so
                // its class rides a field of its own; a news feed is not looked
                // up by class at all.
                if !filters.trading_class.is_empty() && news_source.is_empty() {
                    fields.push((if continuous_future { 8362 } else { 6058 }, &filters.trading_class));
                }
                fields.push((167, fix_sec_type));
                if let Some(tag) = maturity_tag(&filters.last_trade_date_or_contract_month) {
                    fields.push((tag, &filters.last_trade_date_or_contract_month));
                }
                if continuous_future {
                    fields.push((6857, "2"));
                }
                if !right_code.is_empty() {
                    fields.push((201, right_code));
                }
                if !strike_str.is_empty() {
                    fields.push((202, &strike_str));
                }
                if !filters.multiplier.is_empty() {
                    fields.push((231, &filters.multiplier));
                }
            }
            if news_source.is_empty() {
                fields.push((100, fix_exchange));
            }
            if !identifier_lookup && !filters.primary_exchange.is_empty() {
                fields.push((207, &filters.primary_exchange));
            }
            if news_source.is_empty() {
                fields.push((15, currency));
            }
            if !issuer_id.is_empty() {
                fields.push((6454, issuer_id));
            }
            fields.push((6088, "Socket"));
            if let Err(e) = conn.send_fix(&fields) {
                if Self::refuse_unsent_lookup(req_id, &e.to_string(), shared) {
                    return Ok(());
                }
            } else {
                log::info!("Sent secdef lookup: req_id={req_id} symbol={symbol} sec_type={sec_type} identifier={identifier_lookup}");
                hb.last_ccp_sent = Instant::now();
            }
        } else if Self::refuse_unsent_lookup(req_id, "no connection to the venue", shared) {
            return Ok(());
        }
        // By-symbol lookup: master reply carries `6046={exch_list}`. The
        // server never emits a 323=5/6 terminator; completion is detected
        // by counting per-exchange fan-out replies (see `pending_fanout`).
        self.details_delivered.remove(&req_id);
        self.pending_secdef.push((req_id, false, Instant::now() + unanswered_after(req_id)));
        Ok(())
    }

    /// Send a per-exchange fan-out request after a by-symbol master reply.
    /// Wire: `35=c|320={fanout_id}|321=2|146=1|6008={conid}|6004={exch}|`
    pub(crate) fn send_fanout_secdef_request(
        &mut self,
        fanout_req_id: &str,
        con_id: i64,
        exchange: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let con_id_str = con_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, fanout_req_id),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (146, "1"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (6004, exchange),
            ]);
            hb.last_ccp_sent = Instant::now();
        }
    }

    pub(crate) fn send_matching_symbols_request(&mut self, req_id: u32, pattern: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // Recorded only where the request went out, so a request issued while
        // the transport is down is not queued as pending with nothing on the
        // wire to answer it.
        let Some(conn) = ccp_conn.as_mut() else {
            log::warn!("Matching symbols request req_id={req_id} pattern='{pattern}' not sent: no CCP transport");
            // Refused rather than answered empty: an empty answer is a search
            // the venue ran and nothing matched, which is not what happened.
            shared.reference.push_historical_error(
                req_id, crate::error_codes::Refusal::NOT_CONNECTED,
                "matching symbols request could not be sent: no connection to the venue".to_string(),
            );
            return;
        };
        let req_id_str = req_id.to_string();
        let ts = chrono_free_timestamp();
        if let Err(e) = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "185"),
            (320, &req_id_str),
            (58, pattern),
        ]) {
            log::warn!("Matching symbols request req_id={req_id} pattern='{pattern}' not sent: {e}");
            shared.reference.push_historical_error(
                req_id, crate::error_codes::Refusal::NOT_CONNECTED,
                format!("matching symbols request could not be sent: {e}"),
            );
            return;
        }
        hb.last_ccp_sent = Instant::now();
        log::info!("Sent matching symbols request: req_id={req_id} pattern='{pattern}'");
        self.pending_matching_symbols.push((req_id, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    }

    /// Give up on matching-symbols requests the gateway never answered.
    ///
    /// Nothing expired them, so an unanswered request stayed in the queue for
    /// the life of the process — and the reply matcher falls back to the head
    /// of that queue when a reply carries no echoed request id, so a stale entry
    /// could absorb a later request's answer.
    pub(crate) fn sweep_pending_matching_symbols(&mut self, shared: &SharedState) {
        if self.pending_matching_symbols.is_empty() {
            return;
        }
        let now = Instant::now();
        self.pending_matching_symbols.retain(|(req_id, deadline)| {
            if now >= *deadline {
                log::warn!("Matching symbols request req_id={req_id} unanswered after {MATCHING_SYMBOLS_TIMEOUT:?} — giving up");
                // The timeout is answered, not merely recorded: a caller
                // told nothing waits on a request this session has abandoned.
                // Refused rather than answered empty: an empty answer is a
                // search the venue ran and nothing matched, and the venue
                // never answered this one at all.
                shared.reference.push_historical_error(
                    *req_id, crate::error_codes::Refusal::NO_ANSWER,
                    "matching symbols request timed out — no reply from the gateway".to_string(),
                );
                false
            } else {
                true
            }
        });
    }

    /// Give up on an advisor request the venue never answered.
    ///
    /// The reply states only this client's own number for the question, so a
    /// question given up on is a reply nobody can be given — the entry goes
    /// with the refusal rather than staying behind to match a late one onto a
    /// caller who has already been told.
    pub(crate) fn sweep_pending_advisor(&mut self, shared: &SharedState) {
        if self.pending_advisor.is_empty() {
            return;
        }
        let now = Instant::now();
        self.pending_advisor.retain(|key, asked| {
            if now >= asked.deadline {
                log::warn!(
                    "Advisor configuration request {key} unanswered after {ADVISOR_TIMEOUT:?} \
                     — giving up",
                );
                shared.reference.push_advisor_refused(
                    asked.req_id, crate::error_codes::Refusal::NO_ANSWER,
                    "the advisor configuration request timed out — no reply from the venue"
                        .to_string(),
                );
                false
            } else {
                true
            }
        });
    }

    /// Note what a definition says its contract is written on, and ask what
    /// that pays out.
    ///
    /// Every definition, not the ones on one path: a contract is looked up by
    /// its own id, by symbol, for a subscription, for a scanner row and for
    /// the engine's own reasons, and the underlying is stated on all of them.
    /// Done on one of those paths only, an option asked for by id alone could
    /// never reach its schedule and was priced on the fallback for the life of
    /// the session.
    fn note_what_it_is_written_on(
        &mut self,
        def: &crate::control::contracts::ContractDefinition,
        shared: &SharedState,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        shared.reference.note_under_con_id(def.con_id, def.under_con_id);
        if def.under_con_id != 0 {
            self.send_dividends_query(def.under_con_id, ccp_conn, hb);
        }
    }

    /// Ask the venue what a contract pays out.
    ///
    /// Not a document like the other reference queries: the venue takes a line
    /// of text on tag 58 and answers with a small XML document on 6118, under
    /// the id this client asked with. That id is the only thing tying an answer
    /// to a question — the answer names neither the contract nor the query —
    /// so it is kept here against the contract until it comes back.
    ///
    /// Asked once per contract, and not again while one is outstanding: the
    /// answer is a fact about the contract rather than a subscription, and a
    /// second question would be answered with the same schedule under an id
    /// nothing was waiting on.
    pub(crate) fn send_dividends_query(
        &mut self,
        con_id: u32,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if con_id == 0
            || self.dividends_answered.contains(&con_id)
            || self.pending_dividends.iter().any(|(_, held, _)| *held == con_id)
        {
            return;
        }
        let Some(conn) = ccp_conn.as_mut() else {
            log::debug!("what {con_id} pays out could not be asked: no connection to the venue");
            return;
        };
        let query_id = format!("div_{}", self.next_xml_query_id);
        self.next_xml_query_id += 1;
        let query = crate::control::dividends::query_for(con_id);
        let ts = chrono_free_timestamp();
        match conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "27"),
            (320, &query_id),
            (58, &query),
        ]) {
            Ok(()) => {
                hb.last_ccp_sent = Instant::now();
                log::info!("Asked what {con_id} pays out, under {query_id}");
                self.pending_dividends.push((
                    query_id, con_id, Instant::now() + COMPLETED_ORDERS_TIMEOUT,
                ));
            }
            // Registered as outstanding only if it went out. Recorded either
            // way, the contract would never be asked about again.
            Err(e) => log::warn!("what {con_id} pays out could not be asked: {e}"),
        }
    }

    /// One answer to that question, filed against the contract it is about.
    ///
    /// The id is what says which contract, because the answer states no
    /// contract of its own. An id nothing is waiting on is an answer to a
    /// question this session did not ask, which is not a schedule to file.
    fn handle_dividends_answer(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        shared: &SharedState,
    ) {
        let Some(query_id) = parsed.get(&320) else { return };
        let con_id = match self.pending_dividends.iter().position(|(id, _, _)| id == query_id) {
            Some(at) => self.pending_dividends.remove(at).1,
            // Late, and still an answer. The id this session asked under is
            // the only thing that says which contract it is about, so a
            // schedule that arrived a moment after the wait ran out is filed
            // rather than thrown away.
            None => match self
                .dividends_given_up_on
                .iter()
                .position(|(id, _)| id == query_id)
            {
                Some(at) => {
                    let (_, con_id) = self.dividends_given_up_on.remove(at).expect("just found");
                    // Unless a later question about the same contract has
                    // already been answered. This one was asked first and
                    // arrived last, so filing it would put the older schedule
                    // over the newer one.
                    if self.dividends_answered.contains(&con_id) {
                        log::debug!("a later answer for {con_id} already stands");
                        return;
                    }
                    log::debug!("what {con_id} pays out arrived after the wait ran out");
                    con_id
                }
                None => {
                    log::debug!("an answer arrived under {query_id}, which nothing here asked");
                    return;
                }
            },
        };
        let Some(body) = parsed.get(&6118) else {
            // Not marked answered. The mark is what stops this being asked
            // again, and it is never lifted — so a reply carrying no schedule
            // at all, set down as an answer, left that underlying without one
            // for as long as the process ran and nothing asked again.
            log::debug!("the answer for {con_id} states no schedule");
            return;
        };
        self.dividends_answered.insert(con_id);
        let schedule = crate::control::dividends::parse(body);
        log::info!(
            "{con_id} pays out {} times over the venue's books",
            schedule.payments.len(),
        );
        shared.reference.set_dividend_schedule(con_id, schedule);
    }

    /// Ask for the option chain of an underlying.
    ///
    /// A request that cannot go out is refused rather than left unanswered,
    /// because the caller is waiting on the end of a request nothing on the
    /// wire will ever end.
    pub(crate) fn send_option_params_request(
        &mut self,
        req_id: u32,
        symbol: &str,
        fut_fop_exchange: &str,
        underlying_sec_type: &str,
        underlying_con_id: i64,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        let symbol = symbol.to_uppercase();
        // The request names the UNDERLYING's own type, not the derivative being
        // enumerated. Naming the derivative is answered "Unknown contract":
        // there is no option contract by that symbol, only a stock that has
        // options on it.
        //
        // A caller who states nothing claims nothing. An unstated type sent as
        // STK asks about a stock of that symbol where the caller meant an index
        // or a future. Tag 310 is omitted for an empty
        // security type rather than standing one in: its chain-request writer
        // states the type only when the caller gave one, and treats its own
        // empty-named type as absent.
        let underlying = underlying_sec_type;
        // A futures option whose underlying is not itself a future names that
        // underlying on a tag of its own.
        let futures_option = !fut_fop_exchange.is_empty() && underlying != "FUT";
        let con_id_tag = if futures_option { 6457 } else { 6346 };
        let Some(conn) = ccp_conn.as_mut() else {
            log::warn!("Option chain request req_id={req_id} symbol={symbol} not sent: no CCP transport");
            // Refused rather than answered empty: an empty answer is a chain
            // the venue enumerated and found nothing in, which is not what
            // happened.
            shared.reference.push_historical_error(
                req_id, crate::error_codes::Refusal::NOT_CONNECTED,
                "option chain request could not be sent: no connection to the venue".to_string(),
            );
            return;
        };
        let con_id_str = underlying_con_id.to_string();
        let ts = chrono_free_timestamp();
        let mut fields: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &ts),
            (6040, "138"),
            (55, &symbol),
        ];
        if !underlying.is_empty() {
            fields.push((310, underlying));
        }
        fields.push((con_id_tag, &con_id_str));
        fields.push((6320, "1"));
        fields.push((6994, "1"));
        if underlying_sec_type == "FUT" {
            fields.push((6995, fut_fop_exchange));
        }
        if let Err(e) = conn.send_fix(&fields) {
            log::warn!("Option chain request req_id={req_id} symbol={symbol} not sent: {e}");
            shared.reference.push_historical_error(
                req_id, crate::error_codes::Refusal::NOT_CONNECTED,
                format!("option chain request could not be sent: {e}"),
            );
            return;
        }
        hb.last_ccp_sent = Instant::now();
        log::info!("Sent option chain request: req_id={req_id} symbol={symbol} con_id={underlying_con_id}");
        self.pending_option_params.push((req_id, symbol, underlying_con_id, Instant::now() + OPTION_CHAIN_TIMEOUT));
    }

    /// A chain reply names its underlying by symbol and echoes no request id,
    /// so it answers the oldest request outstanding for that symbol.
    fn handle_option_chain(&mut self, msg: &[u8], shared: &SharedState) {
        let Some(scopes) = crate::control::contracts::parse_option_chain_response(msg) else { return };

        // One reply can carry the venues of more than one underlying. Handing
        // the lot to the first one's request gives that caller another
        // underlying's strikes, and leaves the other request to time out with
        // an empty chain — so each underlying is answered to the request that
        // asked for it.
        let mut by_underlying: Vec<(String, Vec<_>)> = Vec::new();
        for scope in scopes {
            match by_underlying.iter_mut().find(|(sym, _)| sym.eq_ignore_ascii_case(&scope.symbol)) {
                Some((_, group)) => group.push(scope),
                None => by_underlying.push((scope.symbol.clone(), vec![scope])),
            }
        }
        // An underlying the venue lists nothing for still answers the request,
        // and then the symbol tag is all there is to attribute it by.
        if by_underlying.is_empty() {
            let symbol = extract_tag_value(msg, b"55=").unwrap_or_default();
            by_underlying.push((symbol, Vec::new()));
        }

        for (symbol, scopes) in by_underlying {
            // Matched on the underlying the reply names, not on its ticker
            // alone: the reply echoes no request id and does carry the
            // underlying's own id, so two chains for one ticker — a share and
            // the future on it, asked for at once — are told apart by the only
            // thing that distinguishes them. Where the reply states none, the
            // ticker is all there is.
            let stated = scopes.iter().map(|s| s.underlying_con_id).find(|id| *id != 0);
            let Some(pos) = self.pending_option_params.iter()
                .position(|(_, pending, con_id, _)| {
                    pending.eq_ignore_ascii_case(&symbol)
                        && stated.is_none_or(|named| *con_id == 0 || named == *con_id)
                })
            else {
                log::warn!("Option chain reply for '{symbol}' matches no request");
                continue;
            };
            let (req_id, _, asked_under, _) = self.pending_option_params.remove(pos);
            // The underlying as the venue names it, where it names one; a
            // caller who asked without the id is answered with it.
            let con_id = stated.unwrap_or(asked_under);
            log::info!("Option chain reply: req_id={req_id} symbol={symbol} scopes={}", scopes.len());
            shared.reference.push_option_params(req_id, con_id, scopes);
        }
    }

    /// Give up on chain requests the gateway never answered. The request is
    /// ended so the caller stops waiting, and the entry goes with it so it
    /// cannot absorb a later reply for the same underlying.
    pub(crate) fn sweep_pending_option_params(&mut self, shared: &SharedState) {
        if self.pending_option_params.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<u32> = Vec::new();
        self.pending_option_params.retain(|(req_id, symbol, _con_id, deadline)| {
            if now >= *deadline {
                log::warn!("Option chain request req_id={req_id} symbol={symbol} unanswered after {OPTION_CHAIN_TIMEOUT:?} — giving up");
                expired.push(*req_id);
                false
            } else {
                true
            }
        });
        // Refused rather than answered empty: an empty answer is a chain the
        // venue enumerated and found nothing in, and the venue never answered
        // these at all.
        for req_id in expired {
            shared.reference.push_historical_error(
                req_id, crate::error_codes::Refusal::NO_ANSWER,
                "option chain request timed out — no reply from the gateway".to_string(),
            );
        }
    }

    pub(crate) fn send_mkt_depth_exchanges_request(&mut self, _ccp_conn: &mut Option<Connection>, _hb: &mut HeartbeatState, shared: &SharedState) {
        // Depth exchanges are derived from the 6040=102 exchange list received during
        // init.
        // No separate server request needed — just signal the shared state to deliver
        // cached data.
        shared.reference.notify_depth_exchanges();
    }

    /// Ask the venue for the orders it has finished.
    ///
    /// The same message the session opens with — the mass status request,
    /// which asks for everything still working — with the mode that asks for
    /// what is done instead, and the window it covers. There is no request of
    /// its own for this: one tag turns the one already being sent into it.
    ///
    /// What comes back is not a document. It is a run of ordinary execution
    /// reports, one per event in each order's life, ending with the sentinel
    /// that ends the opening replay. So the window between the ask and that
    /// sentinel is the whole mechanism, and it is held here.
    pub(crate) fn send_completed_orders_request(
        &mut self,
        turn: u64,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        // Said first, whatever becomes of the question below: what a caller
        // waits on is that the engine has taken its question off the queue.
        shared.orders.note_completed_orders_asked();
        // Not while the session's own replay is still running. Both answers
        // end with the same sentinel and nothing on the wire says which
        // question a sentinel answers, so a window opened across the replay is
        // shut by the replay's ending and the history that follows is read as
        // live. Held instead, and sent the moment the replay is over.
        //
        // Nor while an earlier question is still being answered. Two of them
        // at once share one window and one sentinel: the first sentinel to
        // arrive shuts the window on both, so the second answer took the live
        // path and only one of the two callers was ever released. Asked one at
        // a time, each has a sentinel of its own.
        //
        // The replay half of that wait has an end: an account with nothing
        // working ends its replay without naming an order, and naming one is
        // what says the replay has begun — so a question held for it would
        // wait for ever on exactly the accounts most likely to ask. Held for
        // as long as a replay could take, and then asked anyway. The window is
        // not on a clock here: it has one of its own, and the sweep shuts it
        // before it sends anything behind it.
        let hold_until = *self
            .replay_hold_until
            .get_or_insert_with(|| Instant::now() + COMPLETED_ORDERS_HOLD);
        let the_replay_could_still_be_running =
            !shared.orders.replay_done() && Instant::now() < hold_until;
        if the_replay_could_still_be_running || self.completed_orders_open {
            self.completed_orders_wanted
                .get_or_insert_with(|| Instant::now() + COMPLETED_ORDERS_TIMEOUT);
            // Held under its own turn, not the open window's: the window that
            // is open belongs to the caller before this one, and its end is
            // that caller's answer. Written over it, this caller would have
            // been released by the answer to the question it is queued behind.
            self.completed_orders_queued_on = Some(turn);
            log::debug!(
                "holding the question of what the venue has finished until the one before it                  is answered",
            );
            return;
        }
        // Past the hold, so whatever was waiting is no longer waiting.
        self.completed_orders_wanted = None;
        self.completed_orders_queued_on = None;
        // Whose question this is, carried through to the end that answers it.
        self.completed_orders_asked_on = turn;
        let Some(conn) = ccp_conn.as_mut() else {
            shared.orders.note_completed_orders_end_on(self.completed_orders_asked_on);
            return;
        };
        let ts = chrono_free_timestamp();
        // The day the venue keeps, stated as it states every other time. Its
        // own cutoff decides what falls inside, so the window runs from the
        // start of yesterday to the start of tomorrow: that covers the day
        // whichever side of the cutoff this session is on.
        let from = crate::protocol::datetime::midnight_days_away(-1);
        let to = crate::protocol::datetime::midnight_days_away(1);
        let (from, to) = (from.to_string(), to.to_string());
        let sent = conn.send_fix(&[
            (fix::TAG_MSG_TYPE, "H"),
            (fix::TAG_SENDING_TIME, &ts),
            (11, "*"), (55, "*"), (54, "*"),
            (6533, "1"), (6536, &from), (6537, &to),
        ]);
        match sent {
            Ok(()) => {
                hb.last_ccp_sent = Instant::now();
                // A new question, so a new answer: this one takes its own
                // orders up to its own bound. The half-built records the answer
                // before it left stay, because a report states what changed and
                // leaves the rest out and those records are what a later report
                // about the same order is read against — but they are not this
                // answer's orders, so they are not counted against its bound
                // and are given up, oldest first, if it needs the room.
                self.orders_in_this_answer.clear();
                self.the_answer_is_full = false;
                self.completed_orders_open = true;
                self.completed_orders_deadline = Some(Instant::now() + COMPLETED_ORDERS_TIMEOUT);
                log::info!("Asked the venue for what it has finished, {from} to {to}");
            }
            Err(e) => {
                log::warn!("the request for finished orders could not be sent: {e}");
                shared.orders.note_completed_orders_end_on(self.completed_orders_asked_on);
            }
        }
    }

    /// The exchange directory the session opens with, as the rows a caller
    /// asking which exchanges exist is answered with.
    ///
    /// The venue states an exchange and the name it goes by, in two sections —
    /// shares and derivatives. It states neither what kind of data each
    /// carries nor which group each aggregates into, so neither is stated
    /// here: a field this client filled in itself was read as though the venue
    /// had said it, and a book was gathered from sixty-six venues on four
    /// continents because a section count had been recorded as an aggregation
    /// group. Which venues a book is gathered from comes from the contract's
    /// own definition.
    fn handle_exchange_list(&self, msg: &[u8], shared: &SharedState) {
        use crate::types::DepthMktDataDescription;
        let raw = String::from_utf8_lossy(msg);
        let fields: Vec<&str> = raw.split('\x01').collect();

        // The message has repeating 100=EXCHANGE|6813=NAME pairs grouped by
        // sections. Sections: 6523=category|6811=category_name for the share
        // categories, then 8128=N and 8129=N, each stating how many venues
        // follow it. What follows 8128 is the index venues and what follows
        // 8129 the futures venues; the venues before either are the shares.
        // Labelled shares, every index venue was handed to a caller under a
        // type it does not trade, and a caller asking which venues carry depth
        // for an index was told none do.
        // Every 100/6813 pair becomes a DepthMktDataDescription entry.
        let mut descs: Vec<DepthMktDataDescription> = Vec::new();
        let mut current_sec_type = "STK".to_string();

        let mut i = 0;
        while i < fields.len() {
            let f = fields[i];
            if f.starts_with("8128=") {
                current_sec_type = "IND".to_string();
            } else if f.starts_with("8129=") {
                current_sec_type = "FUT".to_string();
            } else if let Some(exch) = f.strip_prefix("100=") {
                // The name follows the code, and only a field that carries it
                // is the name. Taken as whatever followed, an exchange the
                // venue named nothing for was published under an empty name
                // and the field behind it was swallowed — and where that field
                // was the next exchange, or the marker opening the futures
                // section, what it said was lost with it.
                let named = fields.get(i + 1).and_then(|f| f.strip_prefix("6813="));
                if let Some(name) = named {
                    descs.push(DepthMktDataDescription {
                        exchange: exch.to_string(),
                        sec_type: current_sec_type.clone(),
                        listing_exch: name.to_string(),
                        // Not stated by the venue here.
                        service_data_type: String::new(),
                        // Nor is the aggregation group. Nought is a group of
                        // its own, so a caller read these venues as grouped
                        // together; the largest an integer carries is what a
                        // program written against the reference client reads
                        // as a group nobody stated.
                        agg_group: i32::MAX,
                    });
                    i += 1; // the name is this entry's, so step over it
                }
            }
            i += 1;
        }
        log::info!("Parsed {} exchanges from 6040=102", descs.len());
        shared.reference.push_depth_exchanges(descs);
    }

    /// Give the trading connection up, and the socket it was carried on with
    /// it.
    ///
    /// The connection goes here rather than at the reconnect that replaces it.
    /// A liveness timeout says the venue has stopped answering, not that the
    /// socket is closed, and this account permits one session at a time: kept
    /// through the outage, the reconnect competes with a session this client
    /// is still holding open. On a hard error it is a descriptor held for as
    /// long as the outage lasts.
    ///
    /// Every lookup waiting on the connection is failed here, as the
    /// historical connection fails its own: the connection that replaces this
    /// one is asked nothing this one was asked.
    pub(crate) fn handle_disconnect(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        self.disconnected = true;
        *ccp_conn = None;
        self.recovery_sweep_at = None;
        // The wait for the replay belongs to the connection that replays. Left
        // armed, the next connection inherited a hold that had already expired
        // and asked what the venue has finished while its own replay was still
        // arriving, which reads the replay as history.
        self.replay_hold_until = None;
        // A question about what the venue has finished dies with the
        // connection that carried it. The answer ends with a sentinel and no
        // sentinel is coming, so the caller was left waiting out its whole
        // deadline — and the window stayed open across the reconnect, where
        // the replay that follows is read as history rather than recovered.
        // The queries that went out on this connection will not be answered on
        // the next one, and an entry nobody will answer holds its contract.
        self.pending_dividends.clear();
        // The names one connection's recovery taught this session mean nothing
        // on the next one, which recovers the account again and says them
        // afresh. Kept, they grew for the life of the engine and went on
        // redirecting reports to orders long finished.
        self.wire_name_to_order.clear();
        self.wire_names_learned.clear();
        if self.completed_orders_open || self.completed_orders_wanted.is_some() {
            self.completed_orders_open = false;
            self.completed_orders_deadline = None;
            // Whether anyone is still owed an answer, read before the queue
            // is cleared. The flag below is about the request whose window is
            // open; a question queued behind it has never been answered at
            // all, and suppressing its signal left it waiting out its own
            // clock for a connection the engine already knew was gone.
            let a_queued_question_dies_here = self.completed_orders_wanted.is_some();
            self.completed_orders_wanted = None;
            // A question queued behind the window dies with it, on its own
            // turn: the caller waiting on it is told the answer is over rather
            // than waiting for a connection that has gone. Said after the
            // handover below, not before it — the end is what wakes that
            // caller, and woken first it drained the answer before the orders
            // the venue had already stated were in it.
            let queued_dies_here = self.completed_orders_queued_on.take();
            self.deliver_finished_orders(shared, Handover::Final);
            if let Some(queued) = queued_dies_here {
                shared.orders.note_completed_orders_end_on(queued);
            }
            if a_queued_question_dies_here || !self.completed_orders_answered {
                shared.orders.note_completed_orders_end_on(self.completed_orders_asked_on);
            }
            self.completed_orders_answered = false;
        }
        // The engine stops believing these statuses here, and said so to
        // nobody — so the API layer went on reporting the pre-disconnect
        // status and `req_open_orders` kept asserting it.
        context.mark_orders_uncertain();
        // And what the account holds, for the same reason and at the same
        // moment. Marked only when the next connection arrived, the flag said
        // the download had finished for the whole of a backoff — or for ever,
        // where the retries ran out — so a caller asking what the account
        // holds was answered at once from the pre-drop book with nothing to
        // say the venue had not been heard from since.
        shared.portfolio.account_download_is_pending();
        for order in context.uncertain_orders() {
            let update = executions::uncertain_update(&order, shared.orders.get_order_info(order.order_id));
            shared.orders.push_order_update(update);
            emit(event_tx, Event::OrderUpdate(update));
        }
        self.fail_pending_lookups(context, shared, event_tx);
        // Don't emit Event::Disconnected — auto-reconnect handles CCP drops
        // transparently.
        // Python is only notified if reconnect exhausts retries.
    }

    /// Report every lookup still waiting on the connection as failed, and
    /// forget it.
    ///
    /// The connection that replaces this one is asked nothing this one was
    /// asked, so a lookup outstanding at the drop could only run its deadline
    /// out — and it was then reported as a request the venue never answered,
    /// or a contract the venue does not know, ten to twenty seconds after the
    /// connection went. The historical connection has failed its own at once
    /// all along.
    fn fail_pending_lookups(&mut self, context: &mut Context, shared: &SharedState, event_tx: &Option<EventSink>) {
        const WHY: &str = "the trading connection went away before the venue answered";
        // A caller's details request is failed and ended, as one that ran its
        // deadline out is. A fetch of the engine's own is forgotten, so the
        // next report naming that contract asks again.
        let mut ended: Vec<u32> = Vec::new();
        let mut lost_auto_fetch: Vec<u32> = Vec::new();
        for (req_id, _, _) in self.pending_secdef.drain(..) {
            if req_id < crate::bridge::ENGINE_ID_BASE { ended.push(req_id) } else { lost_auto_fetch.push(req_id) }
        }
        self.auto_fetched_conids.retain(|_, rid| !lost_auto_fetch.contains(rid));
        ended.extend(self.pending_fanout.drain(..).map(|p| p.api_req_id));
        // A contract the venue did name, whose trading hours it now will not
        // state: delivered without them, the way the pairing's own deadline
        // delivers it.
        for p in &mut self.pending_schedule_pair { p.deadline = Instant::now(); }
        self.sweep_pending_schedule_pairs(shared, event_tx);
        self.details_delivered.clear();
        // What the advisor was asked, which only the connection that was asked
        // can answer: the one that replaces it is asked nothing this one was.
        // Left standing, a caller reading a partition of the configuration
        // waited for ever, and one replacing a partition never learned whether
        // the venue had taken its document — no end, and no refusal either.
        for (_, asked) in self.pending_advisor.drain() {
            shared.reference.push_advisor_refused(
                asked.req_id, ADVISOR_SAVE_REFUSED, WHY.to_string(),
            );
        }
        // A scan parked behind the naming of its rows is released with the
        // rows it has, the way its own deadline releases it: the lookups it
        // waited on went with the connection.
        for pe in &mut self.pending_scanner_enrichment { pe.deadline = Instant::now(); }
        self.sweep_scanner_enrichments(shared);
        let mut refused: Vec<u32> = self.pending_matching_symbols.drain(..).map(|(rid, _)| rid).collect();
        refused.extend(self.pending_option_params.drain(..).map(|(rid, ..)| rid));
        // A subscription waiting on the naming of its contract is told, and
        // the slot its registration took is offered back; a request waiting on
        // the same is refused under its own number.
        for (_, p, _) in self.pending_md_subscribe.drain(..) {
            shared.market.push_subscription_failure(p.instrument, WHY.to_string());
            context.slots_to_reconsider.push(p.instrument);
        }
        let named: Vec<(u32, bool)> = self.pending_named.drain(..)
            .filter_map(|(_, cmd, _)| request_id(&cmd).map(|rid| {
                (rid, matches!(cmd, crate::types::ControlCommand::FetchHistorical { .. }))
            }))
            .collect();
        if ended.is_empty() && refused.is_empty() && named.is_empty() {
            return;
        }
        log::warn!(
            "{} lookup(s) were still unanswered when the trading connection went",
            ended.len() + refused.len() + named.len(),
        );
        let code = crate::error_codes::Refusal::NOT_CONNECTED;
        for req_id in ended {
            shared.reference.push_historical_error(req_id, code, WHY.to_string());
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
        for req_id in refused {
            shared.reference.push_historical_error(req_id, code, WHY.to_string());
        }
        for (req_id, bars) in named {
            super::push_hmds_refusal(shared, req_id, code, WHY.to_string(), bars);
        }
    }

    /// Bring both of the completed-order deadlines forward to now.
    ///
    /// Only a test calls these: the two waits are measured against a clock,
    /// and a test that slept them out would be a test that takes the wait.
    #[cfg(test)]
    pub(crate) fn give_up_waiting_for_the_replay(&mut self) {
        self.completed_orders_wanted = Some(Instant::now());
        self.replay_hold_until = Some(Instant::now());
    }

    /// The same for the sentinel that ends an open window.
    #[cfg(test)]
    pub(crate) fn give_up_waiting_for_the_sentinel(&mut self) {
        self.completed_orders_deadline = Some(Instant::now());
    }

    /// And for every dividend query outstanding.
    ///
    /// Takes the query as already sent, because a test has no socket to send
    /// it down and what is under test is what becomes of the answer.
    #[cfg(test)]
    pub(crate) fn give_up_on_a_dividend_query(&mut self, query_id: &str, con_id: u32) {
        self.pending_dividends.push((query_id.to_string(), con_id, Instant::now()));
    }

    /// Send a held question about what the venue has finished, if the
    /// session's own replay is over by now.
    pub(crate) fn sweep_completed_orders_request(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        // A question the venue never answered. Kept, it held its contract for
        // the life of the session and stayed on this list for ever; given up
        // on, the next option written on that underlying asks again.
        let mut given_up_on = Vec::new();
        self.pending_dividends.retain(|(query_id, con_id, until)| {
            let waiting = Instant::now() < *until;
            if !waiting {
                log::debug!("what {con_id} pays out went unanswered under {query_id}");
                given_up_on.push((query_id.clone(), *con_id));
            }
            waiting
        });
        for entry in given_up_on {
            self.dividends_given_up_on.push_back(entry);
            while self.dividends_given_up_on.len() > GIVEN_UP_ON_DIVIDEND_QUERIES {
                self.dividends_given_up_on.pop_front();
            }
        }

        // A caller who has waited long enough is told so. The window is not
        // shut with them.
        //
        // The two are different things and were one. A clock here says when a
        // caller has waited long enough; it says nothing about where the
        // venue's answer ends, and the answer is a run of ordinary reports
        // that carry no mark of which question they answer. Shutting the
        // window on the clock hands the rest of that answer to the live path,
        // where a report stating a fill is a fill and moves a position — and
        // it says the venue finished when the venue said no such thing.
        //
        // So the caller is released with what arrived and the window stays
        // open until the wire closes it: the sentinel, or the connection
        // going away. A question queued behind it waits, which is what the
        // reference client does with a second question while one is pending.
        if self.completed_orders_open
            && self.completed_orders_deadline.is_some_and(|at| Instant::now() >= at)
        {
            self.completed_orders_deadline = None;
            // A copy, because the window is still open and the reports that
            // finish these orders have not arrived. Drained here, the terminal
            // report of an order already handed over would start a fresh
            // record with none of its fields and the caller would be left with
            // the half-built one for good.
            self.deliver_finished_orders(shared, Handover::SoFar);
            shared.orders.note_completed_orders_end_on(self.completed_orders_asked_on);
            // Said, so the sentinel does not say it again to nobody and leave
            // the next caller reading it as its own answer.
            self.completed_orders_answered = true;
            log::warn!(
                "the venue has not said it has finished; the caller is answered with what \
                 arrived and the rest is still read as history",
            );
        }
        let Some(waited_since) = self.completed_orders_wanted else { return };
        if self.completed_orders_open {
            return;
        }
        // The replay of an account with nothing working ends without naming an
        // order, and it is naming one that says the replay has begun — so a
        // question held for it would wait for ever on such an account. Held
        // for as long as the replay could take and then asked anyway.
        if !shared.orders.replay_done() && Instant::now() < waited_since {
            return;
        }
        // Left set, so the guard inside sees the wait it has already run out —
        // clearing it first makes that guard read "nothing has waited yet" and
        // hold the question all over again. It is cleared there, past the hold.
        // On the turn the question that was held was asked on, which is not
        // the open window's: this is that question going out at last.
        let asked_on = self.completed_orders_queued_on.unwrap_or(self.completed_orders_asked_on);
        self.send_completed_orders_request(asked_on, ccp_conn, hb, shared);
    }

    /// Report the orders the recovery push did not account for.
    ///
    /// Their status stays Uncertain: the engine watched the connection die
    /// with them working and has been told nothing since, so it does not know
    /// whether they filled, were pulled, or are still resting. What it does
    /// know — and what it had no way to say before — is that the recovery is
    /// over and they were not in it. A caller waiting on the reconciliation
    /// that `Uncertain` promises was otherwise waiting on nothing.
    pub(crate) fn sweep_recovery(
        &mut self,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        match self.recovery_sweep_at {
            Some(at) if Instant::now() >= at => self.recovery_sweep_at = None,
            _ => return,
        }
        let stranded = context.uncertain_orders();
        if stranded.is_empty() {
            log::info!("Recovery complete — every order the drop left working is accounted for");
            return;
        }
        log::error!(
            "Recovery complete — {} order(s) it did not account for: {:?}. Their state is not known; \
             reconcile from executions before acting on them.",
            stranded.len(),
            stranded.iter().map(|o| o.order_id).collect::<Vec<_>>(),
        );
        for order in stranded {
            let update = executions::uncertain_update(&order, shared.orders.get_order_info(order.order_id));
            shared.orders.push_order_update(update);
            emit(event_tx, Event::OrderUpdate(update));
        }
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        account_id: &str,
        shared: &SharedState,
    ) {
        *ccp_conn = Some(conn);
        self.disconnected = false;
        // This connection has not yet named what it has working, and neither
        // "none" nor the last connection's answer is that. Both of these said
        // otherwise from the connection before it, so a caller asking what it
        // had on was answered at once from the pre-drop book — every order in
        // it Uncertain — while the venue's account was still on its way, which
        // is how the same order is placed twice. Cleared here, that caller
        // waits for the new push the way it waited for the first.
        self.hydrated_any = false;
        shared.orders.replay_is_pending();
        // And the account itself. The same flag, for the same reason: a
        // caller asking what the account holds was answered from the pre-drop
        // snapshot the moment the connection came back, before the venue had
        // restated a single holding.
        shared.portfolio.account_download_is_pending();
        self.recovery_sweep_at = Some(Instant::now() + RECOVERY_PUSH_GRACE);
        hb.last_ccp_sent = Instant::now();
        hb.last_ccp_recv = Instant::now();
        hb.pending_ccp_test = None;

        if let Some(conn) = ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();

            // Re-subscribe to account/position data so server pushes fresh UP/UT/UM
            // messages.
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "91"), (1, account_id), (6556, "DR.1"), (6712, "1"),
            ]);
            // Drawn from the counter, not named here. The venue answers a key
            // it is already serving with nothing, and the refreshes on this
            // connection have been spending keys from that counter since the
            // session opened — so a fixed one is a key that has very likely
            // already been used, and the account and position pushes this asks
            // for simply do not resume. Recorded too, so the unsubscribe that
            // follows closes the key this connection is actually served under.
            // The download a rebuilt connection carries arrives under the key
            // its own opening asked with, which is the one a first logon uses
            // — the handshake sends it before this loop is handed the
            // connection. Recorded before the counter is drawn, so the end
            // that squares the account is the end of the request carrying it;
            // the counter's key below is a second subscribe on the same
            // account, and first-wins leaves it measuring nothing.
            shared.portfolio.holdings_restated_under(OPENING_ACCOUNT_REQUEST);
            let key = self.next_account_request_key();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "6"), (6036, "1"), (6095, account_id), (6529, &key),
            ]);

            // Every profit-and-loss subscription standing, asked for again on
            // this connection: the venue serves one on the connection that
            // asked, and the marks a caller reads otherwise stop moving with
            // nothing said.
            for (req_id, account) in &self.pnl_subscriptions {
                let pnl_key = format!("PLR.{req_id}");
                let _ = conn.send_fix(&[
                    (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                    (6040, "142"), (6529, &pnl_key), (1, account),
                ]);
            }

            // Resting open orders are pushed unsolicited by CCP as 35=8 with
            // 150=0/39=0 carrying originating clientId (6119) and orderId (6121),
            // terminated by 11='*' sentinel.
            hb.last_ccp_sent = Instant::now();
            log::info!(
                "CCP reconnected, sent account/position re-subscribe and {} P&L renewal(s)",
                self.pnl_subscriptions.len(),
            );
        }

    }
}

/// Handle account update messages (cross-cutting, called from CCP message processing).
/// Account figures describing holdings the account does not hold itself.
///
/// Stated the same way as the account's own — a name and a value — and read
/// the same way. What differs is only which set of holdings they describe, and
/// mixing them into the account's own would overstate what it is worth.
fn handle_account_update_elsewhere(
    msg: &[u8],
    shared: &SharedState,
    held: crate::types::HeldElsewhere,
) {
    let Ok(text) = std::str::from_utf8(msg) else { return };
    // The name opens a group; its currency and its value follow inside it,
    // and the group is filed when the next name opens or the frame ends. On
    // the account's own figures the currency comes before the value on every
    // group; read as value-then-currency, the currency closed the group before
    // its value arrived and every figure was dropped. A figure stated in two
    // currencies is two figures.
    let mut name: Option<&str> = None;
    let mut value: Option<&str> = None;
    let mut currency: &str = "";
    let mut stated = 0usize;
    let mut file = |name: &mut Option<&str>, value: &mut Option<&str>, currency: &mut &str| {
        if let (Some(n), Some(v)) = (name.take(), value.take()) {
            shared.portfolio.set_value_elsewhere(held, n.to_string(), v.to_string(), currency.to_string());
            stated += 1;
        }
        *currency = "";
    };
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("8001=") {
            file(&mut name, &mut value, &mut currency);
            name = Some(v);
        } else if let Some(v) = part.strip_prefix("8004=") {
            value = Some(v);
        } else if let Some(c) = part.strip_prefix("15=") {
            currency = c;
        }
    }
    file(&mut name, &mut value, &mut currency);
    if stated > 0 {
        log::info!("{stated} account figures for holdings {held:?}");
    }
}

/// Handle 6040=152, the venue's price table: 146={count} with a list of
/// contract ids in 6008 paired positionally with a list of prices in 8057.
/// The price is stored as text and read where it is used, so one that does not
/// parse costs its own contract and not the table.
fn handle_pnl_prices(msg: &[u8], shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    // Both lists are collected in wire order and paired by position, so an
    // unreadable contract id holds its place instead of shifting every price
    // after it onto the wrong contract.
    let mut con_ids: Vec<Option<i64>> = Vec::new();
    let mut prices: Vec<&str> = Vec::new();
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            con_ids.push(v.parse::<i64>().ok().filter(|&id| id != 0));
        } else if let Some(v) = part.strip_prefix("8057=") {
            prices.push(v);
        }
    }
    let table = con_ids.into_iter().zip(prices)
        .filter_map(|(con_id, price)| Some((con_id?, price.to_string())))
        .collect();
    shared.portfolio.set_venue_prices(table);
}

impl CcpState {

    /// Issue an internal secdef request for `con_id` where the reference cache is cold
    /// and
    /// none has been auto-fetched this session. The reply path populates the cache
    /// through
    /// the existing 35=d handler; the response is not tracked.
    fn auto_fetch_secdef_if_cold(
        &mut self,
        con_id: i64,
        ccp_conn: &mut Option<Connection>,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        if con_id == 0 { return; }
        if self.auto_fetched_conids.contains_key(&con_id) { return; }
        // Warm means defined. An entry a fill or an order seeded names the
        // contract and no more, and read as warm it kept the definition from
        // ever being asked for.
        if shared.reference.has_definition(con_id) { return; }
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        self.auto_fetched_conids.insert(con_id, req_id);
        self.send_secdef_request(req_id, con_id, ccp_conn, hb, shared);
    }

    /// Park a scanner result and dispatch concurrent secdef requests for every cache-
    /// miss
    /// con_id. Once all replies arrive (via `try_release_scanner_enrichments`) the
    /// result
    /// is pushed to the dispatch queue with the now-warm cache. Mirrors what the
    /// gateway
    /// does internally for binary-API scanner clients.
    pub(crate) fn start_scanner_enrichment(
        &mut self,
        api_req_id: u32,
        result: crate::control::scanner::ScannerResult,
        ccp_conn: &mut Option<Connection>,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        let mut awaiting: HashSet<i64> = HashSet::new();
        for entry in &result.entries {
            let con_id = entry.con_id as i64;
            if con_id == 0 { continue; }
            if shared.reference.get_contract(con_id).is_some() { continue; }
            awaiting.insert(con_id);
        }
        // A batch needing nothing still waits behind an earlier batch of its
        // scan that does: handed over at once, it overtook the older list and
        // the caller ended on that one.
        if awaiting.is_empty()
            && !self.pending_scanner_enrichment.iter().any(|p| p.api_req_id == api_req_id)
        {
            shared.reference.push_scanner_data(api_req_id, result);
            return;
        }
        // Issue one secdef request per cold con_id. If another flow has already
        // requested the same con_id — auto_fetched_conids holds it — the send
        // is skipped and the wait stands: that reply populates the cache and
        // release this entry via try_release_scanner_enrichments.
        for &con_id in &awaiting {
            if !self.auto_fetched_conids.contains_key(&con_id) {
                let req_id = self.next_internal_secdef_id;
                self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
                self.auto_fetched_conids.insert(con_id, req_id);
                self.send_secdef_request(req_id, con_id, ccp_conn, hb, shared);
            }
        }
        self.pending_scanner_enrichment.push(PendingScannerEnrichment {
            api_req_id,
            result,
            awaiting,
            deadline: Instant::now() + Duration::from_secs(5),
        });
    }

    /// Called from the 35=d reply path after the contract cache has been
    /// populated for `con_id`. Removes `con_id` from any pending scanner
    /// enrichment's awaiting set; entries whose set becomes empty are handed
    /// over, each once every earlier batch of its scan has been.
    pub(crate) fn try_release_scanner_enrichments(&mut self, con_id: i64, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        for pe in &mut self.pending_scanner_enrichment {
            pe.awaiting.remove(&con_id);
        }
        self.release_scanner_batches(shared);
    }

    /// Hand over every batch that waits on nothing and has no earlier batch of
    /// its scan still waiting, in the order the batches arrived.
    fn release_scanner_batches(&mut self, shared: &SharedState) {
        let mut idx = 0;
        while idx < self.pending_scanner_enrichment.len() {
            let pe = &self.pending_scanner_enrichment[idx];
            let behind_its_own = self.pending_scanner_enrichment[..idx]
                .iter()
                .any(|earlier| earlier.api_req_id == pe.api_req_id);
            if pe.awaiting.is_empty() && !behind_its_own {
                let pe = self.pending_scanner_enrichment.remove(idx);
                shared.reference.push_scanner_data(pe.api_req_id, pe.result);
            } else {
                idx += 1;
            }
        }
    }

    /// Flush scanner enrichments past their deadline, dispatching whatever
    /// entries are held, blank fields included where the secdef reply never
    /// arrived. Prevents an indefinite hang on a missing reply.
    pub(crate) fn sweep_scanner_enrichments(&mut self, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        let now = Instant::now();
        let late = self.pending_scanner_enrichment.iter_mut()
            .filter(|pe| pe.deadline <= now && !pe.awaiting.is_empty());
        for pe in late {
            log::warn!(
                "scanner enrichment timeout: req_id={} missing={} con_ids; dispatching partial",
                pe.api_req_id,
                pe.awaiting.len(),
            );
            pe.awaiting.clear();
        }
        self.release_scanner_batches(shared);
    }
}

/// Fill in a holding's contract once its definition arrives.
///
/// The position feed states a contract id, a quantity and often a cost, and
/// little else — so a holding was reported unnamed, or named and priced by
/// the unit, until some richer message happened to arrive first. The
/// definition is already being fetched for exactly this reason; this is what
/// puts what it carries on the row.
fn identify_position(shared: &SharedState, def: &crate::control::contracts::ContractDefinition) {
    let con_id = def.con_id as i64;
    let Some(existing) = shared.portfolio.position_info(con_id) else { return };
    let sec_type = def.sec_type.to_api_str();
    let multiplier = if def.multiplier != 1.0 { format!("{}", def.multiplier) } else { String::new() };
    // Only where the definition still fills something in. The write leaves
    // every field the row already carries standing, so the question is whether
    // any of them is missing. Asked of the symbol alone, a holding the
    // position feed had already named never took the multiplier — which the
    // definition is the only carrier of on that path — and a contract without
    // it is valued a unit at a time.
    let fills_a_gap = |on_row: &str, stated: &str| on_row.is_empty() && !stated.is_empty();
    if !(fills_a_gap(&existing.symbol, &def.symbol)
        || fills_a_gap(&existing.sec_type, sec_type)
        || fills_a_gap(&existing.currency, &def.currency)
        || fills_a_gap(&existing.multiplier, &multiplier))
    {
        return;
    }
    shared.portfolio.set_position_info(PositionInfo {
        con_id,
        position: existing.position,
        avg_cost: existing.avg_cost,
        symbol: def.symbol.clone(),
        sec_type: sec_type.to_string(),
        currency: def.currency.clone(),
        multiplier,
        ..Default::default()
    });
}

/// Take the server's position as the engine's own.
///
/// The callback side reads `context.position`, and a snapshot that reached
/// only the portfolio left it deciding from a number the account had not held
/// since before the connection — flat, on a process that restarted holding
/// stock. The server is the authority here, so the difference is adopted
/// rather than accumulated.
fn adopt_position(context: &mut Context, instrument: InstrumentId, position: f64) {
    let delta = position - context.position(instrument);
    if delta != 0.0 {
        context.update_position(instrument, delta);
    }
}

#[cfg(test)]
mod venue_clock_tests {
    use super::*;

    fn local_seconds() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// The venue pushes its own clock, and that is what a caller asking for it
    /// is answered from.
    ///
    /// This is one of the two things the venue ever says about its clock, and
    /// the only one that arrives after the logon. Unread, a session running
    /// past a correction went on answering from the difference the logon
    /// stated, however long ago that was.
    #[test]
    fn a_pushed_clock_is_what_a_caller_is_answered_from() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();

        // A day ahead of this machine, so this machine's own clock could not
        // be mistaken for the answer.
        let stated = local_seconds() + 86_400;
        let pushed = fix::fix_build(&[
            (fix::TAG_MSG_TYPE, "U"), (6040, "18"), (6114, &stated.to_string()),
        ], 1);
        ccp.process_ccp_message(
            &pushed, &mut None, &mut context, &shared, &None,
            &mut HeartbeatState::new(), "DU1",
        );

        assert!(
            (shared.market.venue_time_millis() - stated * 1_000).abs() < 2_000,
            "the clock the venue pushed, not the one this machine keeps",
        );
    }

    /// A stamp on an ordinary message says nothing about the venue's clock.
    ///
    /// The venue states its clock twice — on the logon, and in the message
    /// above — and every other message merely carries the time it was sent.
    /// Learned from those as well, a caller's answer moved with whatever
    /// traffic happened to arrive, and stopped moving when it stopped.
    #[test]
    fn an_ordinary_message_does_not_move_the_clock() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();

        // What the venue has stated: this machine's clock, exactly.
        shared.market.note_venue_millis(local_seconds() * 1_000);
        let stamped = fix::fix_build(&[
            (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
            (fix::TAG_SENDING_TIME, "20260815-12:00:00"),
        ], 1);
        ccp.process_ccp_message(
            &stamped, &mut None, &mut context, &shared, &None,
            &mut HeartbeatState::new(), "DU1",
        );

        assert!(
            (shared.market.venue_time_millis() - local_seconds() * 1_000).abs() < 2_000,
            "the difference the venue stated still stands",
        );
    }
}

pub(crate) mod executions;
pub(crate) mod positions;

#[cfg(test)]
mod tests;
