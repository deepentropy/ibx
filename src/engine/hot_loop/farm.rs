use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::engine::context::Context;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::engine::depth_book::{Change, DeepBook, Scale, SingleView, SmartMerge, SmartStatus, TopQuote, RESET_TEXT, SMART_STATUS_WAIT};
use crate::protocol::depth_decoder;
use crate::md_events::MdEvent;
use crate::types::{DepthUpdate, InstrumentId, ReqId};
use std::sync::Arc;
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, fast_extract_msg_type, find_body_after_tag};
use super::pool::{FarmId, FixSink, PRIMARY_MD};

/// A market data subscription as the control command gives it, kept while
/// it waits for the contract's round lot (ibx#287).
#[derive(Debug, Clone)]
pub(crate) struct MdSubscribe {
    pub(crate) con_id: i64,
    pub(crate) symbol: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    pub(crate) last_trade_date: String,
    pub(crate) strike: f64,
    pub(crate) right: String,
    pub(crate) multiplier: String,
    pub(crate) instrument: InstrumentId,
    pub(crate) mode_9887: i32,
    /// Asked once, as a snapshot, instead of a stream (ibx#446).
    pub(crate) snapshot: bool,
}

/// How long a subscription waits for the definition its round lot needs;
/// then it goes out with sizes as on the wire.
pub(crate) const LOT_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A definition reply for a round-lot lookup (ibx#287): keep the lot for
/// the conId, set it on its instruments and release their waiting
/// subscriptions. False when the reply is not for such a lookup.
pub(crate) fn round_lot_reply(context: &mut Context, req_id: &str, msg: &[u8]) -> bool {
    let Some(idx) = context.lot_lookups.iter().position(|(id, _, _)| id == req_id) else { return false };
    let (_, con_id, _) = context.lot_lookups.swap_remove(idx);
    let lot = crate::control::contracts::round_lot_from_secdef(msg);
    log::info!("Round lot for con_id {}: {}", con_id, lot);
    context.round_lots.insert(con_id, lot);
    note_definition(context, con_id, msg);
    release_lot_parked(context, con_id, lot);
    true
}

/// Keep what routing needs from a contract definition (#445, #452): its
/// aggregate group (-1 when absent), its SMART component exchanges, its
/// valid exchanges and its description in depth refusals.
pub(crate) fn note_definition(context: &mut Context, con_id: i64, msg: &[u8]) {
    let group = crate::control::contracts::agg_group_from_secdef(msg).unwrap_or(-1);
    context.agg_groups.insert(con_id, group);
    if let Some(listing) = crate::protocol::fix::fix_parse(msg).get(&crate::control::contracts::TAG_IB_PRIMARY_EXCHANGE)
        .filter(|v| !v.is_empty())
    {
        context.listing_exchanges.insert(con_id, listing.clone());
    }
    let components = crate::control::contracts::smart_components_from_secdef(msg);
    if !components.is_empty() {
        context.smart_components.insert(con_id, components);
    }
    let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
    if let Some(def) = records.iter().find(|d| d.con_id == con_id) {
        if !def.valid_exchanges.is_empty() {
            context.valid_exchanges.insert(con_id, def.valid_exchanges.clone());
        }
        if let Some(description) = depth_description(def) {
            context.depth_descriptions.insert(con_id, description);
        }
    }
}

/// Lookups with no reply in time: the subscriptions go out with a round
/// lot of 1, the lot the reference gives a contract it holds no
/// definition for (ibx#287).
pub(crate) fn sweep_round_lot_lookups(context: &mut Context) {
    if context.lot_lookups.is_empty() { return; }
    let now = Instant::now();
    let mut expired = Vec::new();
    context.lot_lookups.retain(|(id, con_id, deadline)| {
        if *deadline <= now { expired.push((id.clone(), *con_id)); false } else { true }
    });
    for (id, con_id) in expired {
        // The group stays unknown: routed as a contract with none.
        context.agg_groups.entry(con_id).or_insert(-1);
        log::warn!(
            "No definition for con_id {} within {:?} ({}): subscribing with a round lot of 1, so its bid, ask and last sizes are not in round lots",
            con_id, LOT_LOOKUP_TIMEOUT, id,
        );
        release_lot_parked(context, con_id, 1);
    }
}

/// How long a market data request without a conId waits for its lookup;
/// then error 200, as for a contract lookup with no reply (ibx#278).
pub(crate) const MD_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Request numbers of those lookups: a range of their own, below the
/// internal lookups' range and far above caller request ids (ibx#278).
pub(crate) const MD_LOOKUP_FIRST_ID: u32 = 0xE000_0000;
pub(crate) const MD_LOOKUP_IDS: u32 = 0x1000_0000;

/// A definition reply for the conId lookup of a market data request
/// (ibx#278). Exactly one contract: its conId is set on the instrument
/// and the subscription goes on; else error 200 and it ends, as the
/// reference. False when the reply is not for such a lookup.
pub(crate) fn md_contract_reply(context: &mut Context, shared: &SharedState, req_id: &str, msg: &[u8]) -> bool {
    // The reply names the lookup as it was asked; its number is the key.
    let Some(number) = crate::control::contracts::secdef_request_number(req_id) else { return false };
    let Some(idx) = context.md_lookups.iter().position(|(id, _, _)| ReqId::from(*id) == number) else { return false };
    let (_, mut sub, _) = context.md_lookups.remove(idx);
    // A reply can list one contract once per exchange.
    let mut con_ids: Vec<i64> = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default()
        .iter().map(|d| d.con_id).filter(|c| *c != 0).collect();
    con_ids.sort_unstable();
    con_ids.dedup();
    if let [con_id] = con_ids[..] {
        log::info!("Market data for {} {}: conId {} ({})", sub.symbol, sub.sec_type, con_id, req_id);
        note_definition(context, con_id, msg);
        // The reference subscribes from this one definition: its round lot
        // is known, no second lookup (ibx#486, b1_441 of 02/10/2026).
        context.round_lots.entry(con_id).or_insert_with(|| crate::control::contracts::round_lot_from_secdef(msg));
        context.market.resolve_con_id(sub.instrument, con_id);
        sub.con_id = con_id;
        context.md_resolved.push(sub);
    } else {
        log::warn!("Market data for {} {}: the lookup found {} contracts ({}): error 200",
            sub.symbol, sub.sec_type, con_ids.len(), req_id);
        shared.market.push_md_reject(crate::bridge::MdReject::NoSecurityDefinition { instrument: sub.instrument });
    }
    true
}

/// conId lookups with no reply in time end their subscription with error
/// 200 (ibx#278).
pub(crate) fn sweep_md_lookups(context: &mut Context, shared: &SharedState) {
    if context.md_lookups.is_empty() { return; }
    let now = Instant::now();
    context.md_lookups.retain(|(id, sub, deadline)| {
        if *deadline > now { return true; }
        log::warn!("Market data for {} {}: no lookup reply within {:?} ({}): error 200",
            sub.symbol, sub.sec_type, MD_LOOKUP_TIMEOUT, id);
        shared.market.push_md_reject(crate::bridge::MdReject::NoSecurityDefinition { instrument: sub.instrument });
        false
    });
}

fn release_lot_parked(context: &mut Context, con_id: i64, lot: i64) {
    // Requests that waited for the definition go back to the loop.
    let (ready, parked): (Vec<_>, Vec<_>) = std::mem::take(&mut context.def_parked).into_iter().partition(|(c, _)| *c == con_id);
    context.def_parked = parked;
    context.def_ready.extend(ready.into_iter().map(|(_, cmd)| cmd));
    let (ready, parked): (Vec<MdSubscribe>, Vec<MdSubscribe>) =
        std::mem::take(&mut context.lot_parked).into_iter().partition(|s| s.con_id == con_id);
    context.lot_parked = parked;
    // A lookup made for the route alone leaves the sizes as on the wire.
    let lot = if context.scale_us_lots { lot } else { 1 };
    for sub in ready {
        context.market.set_round_lot(sub.instrument, lot);
        context.lot_ready.push(sub);
    }
}

/// One entry of a top-of-book request on the wire (#445): where it went
/// and the values its cancel repeats.
#[derive(Debug, Clone)]
pub(crate) struct MdEntry {
    pub(crate) req_id: u32,
    pub(crate) instrument: InstrumentId,
    pub(crate) farm: FarmId,
    pub(crate) con_id: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    pub(crate) req_type: &'static str,
    pub(crate) mode_9887: i32,
}

/// The security type as the request writes it: a stock has a code of its
/// own, other types keep their name (#445).
pub(crate) fn fix_sec_type(sec_type: &str) -> &str {
    match sec_type {
        "" | "STK" => "CS",
        other => other,
    }
}

/// The routing exchange of a request, as the reference writes it: SMART
/// (or none) becomes the smart-routing name, or the currency exchange for
/// a currency pair; any other exchange is kept (#445).
pub(crate) fn routing_exchange<'a>(exchange: &'a str, sec_type: &str) -> &'a str {
    match exchange {
        "" | "SMART" if sec_type == "CASH" => "IDEALPRO",
        "" | "SMART" => "BEST",
        other => other,
    }
}

/// The exchange of the bid/ask entry: a currency pair asks the
/// high-precision book there, its last entry keeps the pair's exchange
/// (captured 23/09/2026, #445).
fn bid_ask_exchange<'a>(exchange: &'a str, sec_type: &str) -> &'a str {
    if sec_type == "CASH" && exchange == "IDEALPRO" { "FXSUBPIP" } else { exchange }
}

/// The news entry of a market data request (ibx#458): its own request id
/// on the farm of the contract's route, with the provider key.
#[derive(Debug, Clone)]
pub(crate) struct NewsEntry {
    pub(crate) farm_req: u32,
    pub(crate) instrument: InstrumentId,
    pub(crate) farm: FarmId,
    pub(crate) con_id: String,
    pub(crate) sec_type: String,
    /// Provider codes, sorted, comma separated; may be empty.
    pub(crate) providers: String,
    /// The server tag of its ack.
    pub(crate) tag: Option<u32>,
    /// On the wire now (false while its farm is down).
    pub(crate) live: bool,
    /// Article ids already given to the client: a repeat is not given
    /// again (captured 02/10/2026).
    pub(crate) seen: std::collections::HashSet<String>,
    /// The requests that use it: requests on one contract with the same
    /// provider key share one entry, as the reference shares its news
    /// subscription per key (ibx#444).
    pub(crate) refs: u32,
}

/// A generic tick entry of a contract (ibx#450): its own request id on the
/// farm of the contract's route, shared by the requests that asked it, as
/// the reference keeps one per tick in the contract's record
/// (`generictick.bp`).
#[derive(Debug, Clone)]
pub(crate) struct GenericEntry {
    /// 0 while it waits for the top of book's acknowledgement.
    pub(crate) farm_req: u32,
    pub(crate) instrument: InstrumentId,
    pub(crate) farm: FarmId,
    /// Its request code (264).
    pub(crate) code: i32,
    pub(crate) con_id: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    /// The server tag of its ack.
    pub(crate) tag: Option<u32>,
    /// The price tick of its ack.
    pub(crate) min_tick: f64,
    /// The requests that use it.
    pub(crate) refs: u32,
}

/// The market data status entry of a contract (ibx#447): the reference
/// asks it for a request made in frozen mode, and its value tells whether
/// the contract's data is frozen (`generictick.ad`, request code 398).
#[derive(Debug, Clone)]
pub(crate) struct StatusEntry {
    pub(crate) farm_req: u32,
    /// The slot of the contract's real-time top of book.
    pub(crate) instrument: InstrumentId,
    pub(crate) farm: FarmId,
    pub(crate) con_id: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    /// The server tag of its ack.
    pub(crate) tag: Option<u32>,
    /// While the contract is frozen: the slot of its frozen top of book.
    pub(crate) frozen: Option<InstrumentId>,
}

/// The request code of the market data status entry (ibx#447).
pub(crate) const MARKET_DATA_STATUS: i32 = 398;

/// A generic tick request of an instrument whose top of book has not gone
/// out yet (a lookup, a round lot): it goes after it (ibx#450).
#[derive(Debug, Clone)]
pub(crate) struct GenericWaiting {
    pub(crate) instrument: InstrumentId,
    pub(crate) codes: Vec<i32>,
}

/// One entry of a depth request (#452): a book on an exchange, or one
/// half of a top-of-book pair for a SmartDepth component without a book.
#[derive(Debug, Clone)]
pub(crate) struct DepthEntry {
    pub(crate) farm_req: u32,
    pub(crate) farm: FarmId,
    pub(crate) con_id: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    pub(crate) req_type: &'static str,
    /// On the wire now (false while its farm is down).
    pub(crate) live: bool,
    /// The farm's tag for it, from its acknowledgement (#451).
    pub(crate) server_tag: Option<u32>,
    /// Price tick and size unit, from its acknowledgement (#451).
    scale: Option<Scale>,
    /// The book of a book entry (#451).
    book: Option<DeepBook>,
    /// The bid and ask of a top-of-book entry (#451).
    top: TopQuote,
    /// The exchange, as SmartDepth rows name it.
    name: Arc<str>,
    /// After a reset, the entry goes out again at this time (#451).
    pub(crate) resubscribe_at: Option<Instant>,
    /// What the farm said of it (#452).
    pub(crate) access: DepthAccess,
    /// Farm ids the reference spends after this one: the LAST entry of a
    /// top of book that went into the request's one BEST entry (#452).
    skip_ids: u32,
}

/// The farm's answer to a depth entry (#452): none yet, its
/// acknowledgement, or its refusal (an API subscription needed or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DepthAccess {
    Unknown,
    Allowed,
    Refused { api_subscription: bool },
}

/// What a depth request subscribes (#452): the book of an exchange, or its
/// top of book, the LAST entry of which goes to `last` (farm, exchange)
/// when it has one.
#[derive(Debug, Clone)]
pub(crate) enum DepthSpec {
    Book { farm: FarmId, exchange: String },
    Top { farm: FarmId, exchange: String, last: Option<(FarmId, String)> },
}

/// How a depth request's callbacks are made (#451).
#[derive(Debug, Clone)]
enum DepthView {
    Single(SingleView),
    Smart(SmartMerge),
}

/// The reference's own subscription of generic tick 626, the exchange map
/// of a BBO exchange (ibx#441): sent at the first acknowledgement that
/// names the BBO exchange, cancelled once the map came.
#[derive(Debug, Clone)]
pub(crate) struct ExchangeMapSub {
    pub(crate) req_id: u32,
    pub(crate) farm: FarmId,
    pub(crate) code: String,
    pub(crate) sec_type_id: u8,
    pub(crate) con_id: String,
    pub(crate) exchange: String,
    pub(crate) sec_type: String,
    /// From its acknowledgement.
    pub(crate) server_tag: Option<u32>,
    /// The instrument whose acknowledgement asked it.
    pub(crate) instrument: InstrumentId,
}

/// The generic tick of the exchange map ("EXCH MAP").
const EXCHANGE_MAP_TICK: &str = "626";

/// A depth request of the client (#452).
#[derive(Debug, Clone)]
pub(crate) struct DepthReq {
    pub(crate) req_id: ReqId,
    pub(crate) con_id: i64,
    pub(crate) smart: bool,
    pub(crate) entries: Vec<DepthEntry>,
    /// Round lot of the contract's sizes (#451).
    lot: i64,
    view: DepthView,
    /// SmartDepth: the components' answers, for warning 2152 (#452).
    status: Option<SmartStatus>,
    /// The contract as a refusal (354, 10089) names it: description and
    /// `/DEEP` (#452); empty when the description is not known.
    refusal_suffix: String,
}

/// The definition of each SmartDepth component exchange, asked before its
/// books (#452): the reference looks up the contract on each of its valid
/// exchanges and subscribes once all answered, or after 10 s.
#[derive(Debug, Clone)]
pub(crate) struct DepthGather {
    pub(crate) req_id: ReqId,
    pub(crate) con_id: i64,
    pub(crate) sec_type: String,
    pub(crate) num_rows: i32,
    pub(crate) components: Vec<DepthComponent>,
    pub(crate) deadline: Instant,
}

/// One component exchange of a SmartDepth request and its definition.
#[derive(Debug, Clone)]
pub(crate) struct DepthComponent {
    pub(crate) exchange: String,
    /// The name of its lookup (320).
    pub(crate) lookup: String,
    pub(crate) definition: ComponentDefinition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComponentDefinition {
    Waiting,
    /// The lookup found no contract.
    Missing,
    /// Found; `not2d`: its order types hold NOT2D, which keeps its top of
    /// book out of SmartDepth.
    Found { not2d: bool },
}

/// How long the reference waits for the components' definitions.
pub(crate) const DEPTH_GATHER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A definition reply for a SmartDepth component lookup (#452). False
/// when the reply is not for one.
pub(crate) fn depth_component_reply(context: &mut Context, req_id: &str, msg: &[u8]) -> bool {
    let Some(c) = context.depth_gathers.iter_mut().flat_map(|g| g.components.iter_mut()).find(|c| c.lookup == req_id) else {
        return false;
    };
    let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
    c.definition = match records.first() {
        Some(def) => ComponentDefinition::Found {
            not2d: def.order_types.iter().any(|t| t.split('/').next() == Some("NOT2D")),
        },
        None => ComponentDefinition::Missing,
    };
    log::info!("SmartDepth component {} ({}): {:?}", c.exchange, req_id, c.definition);
    true
}

/// How long a reset book waits before it is asked again, as the
/// reference (#451).
pub(crate) const DEPTH_RESUBSCRIBE_DELAY: std::time::Duration = std::time::Duration::from_millis(3000);

/// What a block of a 35=P message is, for the market data queue (ibx#446):
/// on a trade stream tag a trade, or daily figures when the block is a
/// daily-stats one; on a quote tag a book update (a daily-stats block on a
/// quote tag is dropped by the reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind { Quote, Trade, Daily }

/// A block being read: what it set.
#[derive(Debug, Clone, Copy)]
struct MdBlock {
    instrument: InstrumentId,
    kind: BlockKind,
    fields: u16,
    /// A trade gives its time, its exchange.
    time: bool,
    exchange: bool,
    /// The trade's status.
    halted: Option<u8>,
}

impl MdBlock {
    /// A block of an instrument an API request listens to; None otherwise.
    #[inline(always)]
    fn start(instrument: InstrumentId, trade: bool, stats: bool, queue: &crate::md_events::MdQueue) -> Option<Self> {
        let kind = match (trade, stats) {
            (true, true) => BlockKind::Daily,
            (true, false) => BlockKind::Trade,
            (false, false) => BlockKind::Quote,
            (false, true) => return None,
        };
        if !queue.listened(instrument) || queue.needs_catch_up(instrument) {
            return None;
        }
        Some(Self { instrument, kind, fields: 0, time: false, exchange: false, halted: None })
    }

    /// A tick of the block, as `MarketState::apply_tick` applies it.
    #[inline(always)]
    fn note(&mut self, tick: &tick_decoder::RawTick) {
        use tick_decoder as td;
        let m = tick.magnitude;
        if let Some(f) = crate::md_events::wire_field(tick.tick_type, m, tick.stats_block) {
            self.fields |= 1 << f;
        }
        if self.kind == BlockKind::Trade {
            match tick.tick_type {
                td::O_TIMESTAMP_BASE | td::O_TIMESTAMP_DELTA if m > 0 => self.time = true,
                td::O_LAST_EXCH if m >= 0 => self.exchange = true,
                td::O_ATTRIBUTES if m != -1 => self.halted = Some((m & 3) as u8),
                _ => {}
            }
        }
    }
}

pub(crate) struct FarmState {
    pub(crate) next_md_req_id: u32,
    pub(crate) md_req_to_instrument: Vec<(u32, InstrumentId)>,
    /// Farm request ids of regulatory snapshots (ibx#446), with their instrument.
    pub(crate) snapshot_reqs: Vec<(u32, InstrumentId)>,
    pub(crate) instrument_md_reqs: Vec<(InstrumentId, Vec<u32>)>,
    /// The client's market data modes from reqMarketDataType (ibx#447).
    pub(crate) md_modes: crate::types::MarketDataModes,
    /// The market data status entries of the contracts asked in frozen
    /// mode (ibx#447).
    pub(crate) status: Vec<StatusEntry>,
    /// Frozen slots whose contract went back to real-time data away from a
    /// farm message (its farm was lost): (frozen slot, real-time slot),
    /// given out by `take_thawed` (ibx#447).
    thawed: Vec<(InstrumentId, InstrumentId)>,
    /// Depth requests of the client and their entries (#452).
    pub(crate) depth_reqs: Vec<DepthReq>,
    /// The session's logon turns the user book on (`6247=demo`, as on
    /// paper): depth books are diffed by index (#451).
    pub(crate) user_book: bool,
    /// Depth callbacks and changes being built (#451).
    depth_out: Vec<DepthUpdate>,
    depth_changes: Vec<Change>,
    /// The entries of a server tag being handled (#452).
    depth_hits: Vec<(usize, usize)>,
    /// Depth callbacks and reset errors made away from a farm message (a
    /// farm lost), given out at the next sweep (#451).
    depth_pending: Vec<DepthUpdate>,
    depth_pending_resets: Vec<ReqId>,
    /// Option resub info: (instrument, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, snapshot).
    md_resub_info: Vec<(InstrumentId, String, String, String, String, f64, String, String, i32, bool)>,
    pub(crate) disconnected: bool,
    pub(crate) tick_buf: Vec<tick_decoder::RawTick>,
    pub(crate) farm_msg_buf: Vec<Vec<u8>>,
    /// The market data routing table of the logon (#445); None until it
    /// came.
    pub(crate) routing: Option<crate::engine::routing::RoutingTable>,
    /// Top-of-book entries on the wire, by request id (#445).
    pub(crate) md_entries: Vec<MdEntry>,
    /// The farm the messages being handled came from (#445): server tags
    /// are numbered by each farm.
    pub(crate) rx_farm: FarmId,
    /// Exchange map subscriptions on the wire (ibx#441).
    pub(crate) exchange_map_subs: Vec<ExchangeMapSub>,
    /// News entries of market data requests (ibx#458).
    pub(crate) news: Vec<NewsEntry>,
    /// News ticks given before their request's top of book went out
    /// (ibx#458), one per request, with the provider key and the 10094
    /// text of a refused one: sent, or refused, when it goes.
    pub(crate) news_waiting: Vec<(InstrumentId, String, Option<String>)>,
    /// The API subscriptions a refusal named since the last data
    /// permission change (ibx#421, the reference's `jextend.F.d`).
    pub(crate) refused_api_subscriptions: Vec<String>,
    /// Generic tick entries of the contracts (ibx#450).
    pub(crate) generic: Vec<GenericEntry>,
    /// Generic tick requests waiting for their top of book to go out.
    pub(crate) generic_waiting: Vec<GenericWaiting>,
    /// The generic tick fields of each contract, as its record keeps them.
    generic_records: std::collections::HashMap<InstrumentId, crate::control::generic_values::GenericRecord>,
    /// The contracts whose top of book was acknowledged: their generic
    /// ticks go at once.
    top_confirmed: Vec<InstrumentId>,
    /// The last request parameters of each contract (conId): minimum tick,
    /// BBO exchange, permissions. The reference's record of the contract
    /// keeps them after its requests ended (ibx#444).
    pub(crate) con_params: std::collections::HashMap<i64, (f64, String, i64)>,
}

impl FarmState {
    pub(crate) fn new() -> Self {
        Self {
            next_md_req_id: 1,
            md_req_to_instrument: Vec::new(),
            snapshot_reqs: Vec::new(),
            instrument_md_reqs: Vec::new(),
            md_modes: crate::types::MarketDataModes::default(),
            status: Vec::new(),
            thawed: Vec::new(),
            depth_reqs: Vec::new(),
            user_book: false,
            depth_out: Vec::new(),
            depth_changes: Vec::new(),
            depth_hits: Vec::new(),
            depth_pending: Vec::new(),
            depth_pending_resets: Vec::new(),
            md_resub_info: Vec::new(),
            disconnected: false,
            tick_buf: Vec::with_capacity(16),
            farm_msg_buf: Vec::with_capacity(32),
            routing: None,
            md_entries: Vec::new(),
            rx_farm: PRIMARY_MD,
            exchange_map_subs: Vec::new(),
            news: Vec::new(),
            news_waiting: Vec::new(),
            refused_api_subscriptions: Vec::new(),
            generic: Vec::new(),
            generic_waiting: Vec::new(),
            generic_records: std::collections::HashMap::new(),
            top_confirmed: Vec::new(),
            con_params: std::collections::HashMap::new(),
        }
    }

    pub(crate) fn poll_market_data(
        &mut self,
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        // Catch-ups of the market data queue, once it has room (ibx#446).
        let queue = &shared.market.md_events;
        if queue.catch_up_wanted() {
            queue.write_catch_ups(|id| MdEvent::catch_up(id, context.quote(id), context.market.marks(id)));
        }
        if self.disconnected {
            return;
        }
        self.farm_msg_buf.clear();
        let mut bad_signature = false;
        {
            let conn = match farm_conn.as_mut() {
                None => return,
                Some(c) => c,
            };
            match conn.try_recv() {
                Ok(0) => return,
                Err(e) => {
                    log::error!("Farm connection lost: {}", e);
                    self.handle_disconnect(context, event_tx);
                    return;
                }
                Ok(n) => {
                    log::trace!("Farm recv: {} bytes, buffered: {}", n, conn.buffered());
                    let now = Instant::now();
                    hb.last_farm_recv = now;
                    context.recv_at = now;
                    hb.pending_farm_test = None;
                }
            }
            let frames = conn.extract_frames();
            log::trace!("Farm frames: {}", frames.len());
            for frame in &frames {
                match frame {
                    Frame::FixComp(raw) => {
                        let (unsigned, valid) = conn.unsign(raw);
                        if !valid { bad_signature = true; break; }
                        match fixcomp::fixcomp_decompress(&unsigned) {
                            Ok(inner) => {
                                if log::log_enabled!(log::Level::Trace) {
                                    for m in &inner {
                                        log::trace!("WIRE< farm/comp {}", fix::fmt_pipe(m));
                                    }
                                }
                                self.farm_msg_buf.extend(inner);
                            }
                            Err(e) => {
                                log::warn!(
                                    "Farm: dropping malformed FIXCOMP frame ({} bytes): {}",
                                    unsigned.len(), e,
                                );
                            }
                        }
                    }
                    Frame::Binary(raw) => {
                        let (unsigned, valid) = conn.unsign(raw);
                        if !valid { bad_signature = true; break; }
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!("WIRE< farm/bin {}", fix::fmt_pipe(&unsigned));
                        }
                        self.farm_msg_buf.push(unsigned);
                    }
                    Frame::Fix(raw) => {
                        let (unsigned, valid) = conn.unsign(raw);
                        if !valid { bad_signature = true; break; }
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!("WIRE< farm/fix {}", fix::fmt_pipe(&unsigned));
                        }
                        self.farm_msg_buf.push(unsigned);
                    }
                    Frame::Control(_) => {
                        // 8=1 / 8=X control state — not consumed on the farm path (ibx#185).
                    }
                }
            }
        }

        let mut msgs = std::mem::take(&mut self.farm_msg_buf);
        for msg in &msgs {
            self.process_farm_message(msg, farm_conn, context, shared, event_tx, hb);
        }
        msgs.clear();
        self.farm_msg_buf = msgs;

        // A signature mismatch drops the connection, as the reference does;
        // the frames before it were handled, the reconnect path follows
        // (ibx#275).
        if bad_signature {
            log::error!("Farm frame signature mismatch: connection dropped, reconnecting");
            if let Some(conn) = farm_conn.as_mut() {
                conn.shutdown();
            }
            self.handle_disconnect(context, event_tx);
        }
    }

    pub(crate) fn process_farm_message(
        &mut self,
        msg: &[u8],
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        let msg_type = match fast_extract_msg_type(msg) {
            Some(t) => t,
            None => return,
        };
        match msg_type {
            b"P" => self.handle_tick_data(msg, context, shared, event_tx),
            b"Q" => {
                log::info!("Farm 35=Q subscription ack received");
                self.handle_subscription_ack(msg, farm_conn, context, shared, hb);
            }
            b"0" => {}
            b"1" => {
                let parsed = fix::fix_parse(msg);
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = farm_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let result = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    log::info!("Farm TestReq '{}' -> heartbeat response seq={} result={:?}",
                        test_id, conn.seq, result);
                    hb.last_farm_sent = Instant::now();
                }
            }
            b"L" => self.handle_ticker_setup(msg, context),
            b"UT" | b"UM" | b"RL" => super::ccp::handle_account_update(msg, context, shared),
            b"UP" => super::ccp::handle_portfolio_message(msg, context, shared, event_tx),
            b"Y" | b"Z" => self.handle_depth(msg, farm_conn, shared),
            b"G" => self.handle_generic_frame(msg, farm_conn, context, shared, event_tx, hb),
            b"3" => self.handle_md_reject(msg, context, shared, farm_conn, hb),
            b"T" => {
                // The routing table, when it came after the logon (#445).
                if let Some(text) = crate::engine::routing::table_text(msg) {
                    let table = crate::engine::routing::RoutingTable::parse(&text, crate::engine::routing::TableKind::MarketData);
                    log::info!("Market data routing table: {} farms", table.routes().len());
                    self.routing = Some(table);
                }
            }
            other => {
                log::debug!("Farm unhandled 35={}: {} bytes", String::from_utf8_lossy(other), msg.len());
            }
        }
    }

    fn handle_tick_data(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=P\x01") {
            Some(b) => b,
            None => return,
        };

        let mut ticks = std::mem::take(&mut self.tick_buf);
        if tick_decoder::decode_ticks_35p_into(body, &mut ticks) {
            // The session goes on, as the reference's (ibx#272).
            log::warn!("Farm tick message: malformed block dropped, {} ticks of earlier blocks kept", ticks.len());
        }
        // A 35=P message carries no depth rows: only the top of book of
        // SmartDepth components without a book (#451).
        if !self.depth_reqs.is_empty() {
            self.depth_tops(&ticks, shared);
        }
        let mut notified = [0u64; crate::types::MAX_INSTRUMENTS / 64];
        let queue = &shared.market.md_events;
        if queue.catch_up_wanted() {
            queue.write_catch_ups(|id| MdEvent::catch_up(id, context.quote(id), context.market.marks(id)));
        }
        // The block being read, for the market data queue (ibx#446).
        let mut block: Option<MdBlock> = None;

        // Phase 1: Apply all ticks to internal quotes before publishing.
        for tick in &ticks {
            let route = context.market.route_farm_tag(self.rx_farm, tick.server_tag);
            if tick.first {
                if let Some(b) = block.take() {
                    self.end_md_block(&b, context, queue);
                }
                block = route.as_ref().and_then(|r| MdBlock::start(r.instrument, r.trade, tick.stats_block, queue));
            }
            let Some(route) = route else { continue };
            let instrument = route.instrument;
            if let Some(b) = block.as_mut() {
                b.note(tick);
            }

            context.market.apply_tick_sized(instrument, route.price_tick, route.size_tick, route.trade, tick);

            notified[(instrument >> 6) as usize] |= 1u64 << (instrument & 63);
        }
        if let Some(b) = block.take() {
            self.end_md_block(&b, context, queue);
        }
        // The message's steps go together (the reader takes its book
        // updates after its other steps).
        queue.publish();

        // Phase 2: Publish complete quotes after all ticks in the batch are applied.
        for (word_idx, &word) in notified.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let instrument = (word_idx as u32) * 64 + remaining.trailing_zeros();
                remaining &= remaining - 1;
                shared.market.push_quote(instrument, context.quote(instrument));
                emit(event_tx, Event::Tick(instrument));
            }
        }
        self.tick_buf = ticks;
    }

    /// The steps of a block that ended (ibx#446): a trade gives its time
    /// and its exchange when it has them, then its price and size; daily
    /// figures and a book update give one step. Each with its values at
    /// the end of the block.
    fn end_md_block(&self, b: &MdBlock, context: &Context, queue: &crate::md_events::MdQueue) {
        use crate::md_events::{field, MdStep};
        let q = context.quote(b.instrument);
        match b.kind {
            BlockKind::Daily => {
                queue.offer_quote(b.instrument, MdStep::Daily, b.fields, q, None, 0);
            }
            BlockKind::Trade => {
                let mut rest = b.fields;
                if b.time {
                    queue.offer_quote(b.instrument, MdStep::Time, 1 << field::TIME, q, None, 0);
                    rest &= !(1 << field::TIME);
                }
                if b.exchange {
                    queue.offer_quote(b.instrument, MdStep::Exchange, 1 << field::LAST_EXCH, q, None, 0);
                    rest &= !(1 << field::LAST_EXCH);
                }
                queue.offer_quote(b.instrument, MdStep::Last, rest, q, b.halted, 0);
            }
            BlockKind::Quote => {
                let auto = context.market.marks(b.instrument).auto_word();
                queue.offer_quote(b.instrument, MdStep::Quote, b.fields, q, None, auto);
            }
        }
    }

    fn handle_subscription_ack(
        &mut self,
        msg: &[u8],
        sink: &mut dyn FixSink,
        context: &mut Context,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        let body = match find_body_after_tag(msg, b"35=Q\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        // An ack dropped here leaves the subscription with no server tag, so
        // its ticks are never routed: say why.
        if parts.len() < 3 {
            log::warn!("Farm 35=Q ack not understood (fewer than 3 fields): {:?}", text);
            return;
        }
        let (server_tag, req_id): (u32, u32) = match (parts[0].parse(), parts[1].parse()) {
            (Ok(t), Ok(r)) => (t, r),
            _ => {
                log::warn!("Farm 35=Q ack not understood (tag or request id): {:?}", text);
                return;
            }
        };
        let min_tick: f64 = parts[2].parse().unwrap_or(0.01);

        // The acknowledgement of an exchange map subscription (ibx#441).
        let rx_farm = self.rx_farm;
        if let Some(sub) = self.exchange_map_subs.iter_mut().find(|s| s.req_id == req_id && s.farm == rx_farm) {
            log::info!("Exchange map {}:{} subscribed: server_tag {}", sub.code, sub.sec_type_id, server_tag);
            sub.server_tag = Some(server_tag);
            return;
        }
        // A news entry's ack (ibx#458): its tag carries the headlines.
        if let Some(e) = self.news.iter_mut().find(|e| e.live && e.farm == rx_farm && e.farm_req == req_id) {
            e.tag = Some(server_tag);
            log::info!("News ack: server_tag {} -> instrument {} ({})", server_tag, e.instrument, e.providers);
            return;
        }
        // A generic tick entry's ack (ibx#450): its tag carries the tick's
        // blocks.
        if let Some(e) = self.generic.iter_mut().find(|e| e.farm_req != 0 && e.farm == rx_farm && e.farm_req == req_id) {
            e.tag = Some(server_tag);
            e.min_tick = min_tick;
            log::info!("Generic tick {} ack: server_tag {} -> instrument {}", e.code, server_tag, e.instrument);
            return;
        }

        // The market data status entry's ack (ibx#447).
        if let Some(e) = self.status.iter_mut().find(|e| e.farm == rx_farm && e.farm_req == req_id) {
            e.tag = Some(server_tag);
            log::info!("Market data status ack: server_tag {} -> instrument {}", server_tag, e.instrument);
            return;
        }

        // A depth entry: its tag, price tick and size increment (#451).
        if self.depth_ack(req_id, server_tag, min_tick, parts.get(8).and_then(|v| v.parse::<f64>().ok()), shared) {
            return;
        }

        // L1 ack
        let instrument = match self.md_req_to_instrument.iter()
            .position(|(id, _)| *id == req_id)
        {
            Some(idx) => {
                let (_, instr) = self.md_req_to_instrument.remove(idx);
                instr
            }
            None => {
                log::warn!("Farm 35=Q ack for request id {} (server tag {}) matches no pending subscription; pending: {:?}",
                    req_id, server_tag, self.md_req_to_instrument.iter().map(|(id, _)| *id).collect::<Vec<_>>());
                return;
            }
        };

        // The tick scales the prices of this server tag only; the bid/ask
        // entry's valid tick is also the contract's, as in the reference.
        // So does the size increment (ibx#287, ibx#446); absent from older
        // acks.
        let size_min_tick = parts.get(8).and_then(|v| v.parse::<f64>().ok());
        context.market.register_farm_tag_sized(self.rx_farm, server_tag, instrument, min_tick, size_min_tick);
        let bid_ask = self.instrument_md_reqs.iter().find(|(id, _)| *id == instrument)
            .and_then(|(_, reqs)| reqs.iter().position(|r| *r == req_id))
            .is_some_and(|p| p % 2 == 0);
        if bid_ask && min_tick.is_finite() && min_tick > 0.0 {
            context.market.set_min_tick(instrument, min_tick);
        }
        // The top of book is confirmed: the generic ticks that waited for it
        // go, with the exchange map entry when its key is new, in one
        // message (ibx#450, ibx#441; captured 05/10/2026).
        let generic = self.confirm_top(instrument);
        let map = self.observe_exchange_map(instrument, parts.get(5).copied(), context, shared);
        self.send_after_ack(&generic, map, sink, hb);
        // A regulatory snapshot (ibx#446): its permission and BBO exchange
        // go to its fetcher, never as tickReqParams.
        if self.snapshot_reqs.iter().any(|(id, _)| *id == req_id) {
            let permissions = parts.get(4).and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(0);
            let bbo = parts.get(5).map(|s| s.trim().to_string()).unwrap_or_default();
            log::info!("Regulatory snapshot ack: instrument {} server_tag {} permissions {} bbo {:?}", instrument, server_tag, permissions, bbo);
            shared.market.push_snapshot_ack(crate::bridge::TickReqParams {
                instrument, min_tick, bbo_exchange: bbo, snapshot_permissions: permissions,
            });
            return;
        }
        log::info!("Subscribed instrument {} -> server_tag {}, minTick {}", instrument, server_tag, min_tick);

        // The bid/ask entry of the pair (the first of each pair of ids)
        // gives the request parameters (ibx#449).
        if bid_ask {
            let sec_type = context.market.order_routing(instrument).0;
            if let Some(params) = tick_req_params(instrument, min_tick, &parts, &sec_type) {
                if let Some(con_id) = context.market.con_id(instrument) {
                    self.con_params.insert(con_id, (params.min_tick, params.bbo_exchange.clone(), params.snapshot_permissions as i64));
                }
                shared.market.push_tick_req_params(params);
            }
        }
    }

    /// The BBO exchange code of a market data acknowledgement (ibx#441), as
    /// the reference keeps it: the "no exchange" value is ignored; the
    /// contract's key (code, security type) becomes known, and the first
    /// time a key with a code is seen the exchange map is asked with the
    /// gateway's own generic tick 626 on the farm of the acknowledgement.
    fn observe_exchange_map(
        &mut self,
        instrument: InstrumentId,
        code: Option<&str>,
        context: &Context,
        shared: &SharedState,
    ) -> Option<ExchangeMapSub> {
        let code = code.map(str::trim)?;
        if code == "ffffffff" {
            return None;
        }
        let (sec_type, exchange) = context.market.order_routing(instrument);
        let sec_type_id = crate::types::sec_type_id(&sec_type)?;
        shared.reference.observe_exchange_map_at(instrument, code, sec_type_id, shared.market.md_events.position());
        // Asked once per key; again only when the farm that was asked it
        // was lost before the map came.
        let waiting = matches!(shared.reference.exchange_map(code, Some(sec_type_id)), crate::bridge::ExchangeMapState::Waiting);
        if code.is_empty() || !waiting
            || self.exchange_map_subs.iter().any(|s| s.code == code && s.sec_type_id == sec_type_id)
        {
            return None;
        }
        let con_id = context.market.con_id(instrument)?;
        let sub = ExchangeMapSub {
            req_id: self.next_md_req_id,
            farm: self.rx_farm,
            code: code.to_string(),
            sec_type_id,
            con_id: con_id.to_string(),
            exchange,
            sec_type: fix_sec_type(&sec_type).to_string(),
            server_tag: None,
            instrument,
        };
        self.next_md_req_id += 1;
        log::info!("Starting to observe the exchange map {}:{} (instrument {}, id {})", code, sec_type, instrument, sub.req_id);
        Some(sub)
    }

    /// Subscribe ("1") or cancel ("2") an exchange map, as the reference
    /// writes it (captured 02/10/2026):
    /// `35=V|263=1|146=1|262=3|6008=265598|207=BEST|167=CS|264=626|6088=Socket`.
    /// 6088=Socket only while a streaming API request of the contract
    /// runs, the rule of every entry of the contract's record (the top
    /// observer's priority is 8, MKTDATA-L1 3 row 8; ibx#487): none when
    /// the map was asked by a depth request (depth_single_iex of
    /// 28/09/2026) or when the request was cancelled before the map came
    /// (rth_order_types of 28/09/2026, QQQ).
    fn send_exchange_map_request(&self, sub: &ExchangeMapSub, action: &str, sink: &mut dyn FixSink, hb: &mut HeartbeatState) -> bool {
        let mut tags = md_message_head(action, 1);
        tags.extend(self.exchange_map_entry(sub));
        let sent = sink.send_comp(&borrow_tags(&tags));
        if sent && sub.farm == PRIMARY_MD {
            hb.last_farm_sent = Instant::now();
        }
        sent
    }

    /// The entry of an exchange map subscription.
    fn exchange_map_entry(&self, sub: &ExchangeMapSub) -> Vec<(u32, String)> {
        let mut tags = vec![
            (262, sub.req_id.to_string()),
            (6008, sub.con_id.clone()),
            (207, sub.exchange.clone()),
            (167, sub.sec_type.clone()),
            (264, EXCHANGE_MAP_TICK.to_string()),
        ];
        if self.top_snapshot(sub.instrument) == Some(false) {
            tags.push((6088, "Socket".into()));
        }
        tags
    }

    /// The message after a top of book acknowledgement: the generic tick
    /// entries that waited for it, then the exchange map entry (ibx#450;
    /// captured 05/10/2026, AAPL: ten generic entries then 626 in one
    /// 35=V). Nothing when there is neither.
    fn send_after_ack(&mut self, generic: &[usize], map: Option<ExchangeMapSub>, sink: &mut dyn FixSink, hb: &mut HeartbeatState) {
        if generic.is_empty() && map.is_none() {
            return;
        }
        let farm = generic.first().map(|&i| self.generic[i].farm).or(map.as_ref().map(|m| m.farm)).unwrap_or(PRIMARY_MD);
        let mut tags = md_message_head("1", generic.len() + map.is_some() as usize);
        for &i in generic {
            tags.extend(generic_entry(&self.generic[i], true));
        }
        if let Some(m) = &map {
            tags.extend(self.exchange_map_entry(m));
        }
        if sink.send_comp(&borrow_tags(&tags)) {
            if let Some(m) = map {
                self.exchange_map_subs.push(m);
            }
            if farm == PRIMARY_MD {
                hb.last_farm_sent = Instant::now();
            }
        }
    }

    /// A 35=G generic tick frame (ibx#450): a bit count, then blocks of a
    /// 32-bit server tag, a length and the payload (`jmdclient.br.a(byte[])`).
    /// Each block goes to what its tag is: an exchange map subscription
    /// (ibx#441), a news entry (ibx#458) or a generic tick entry. A tag of
    /// nothing known ends the reading (its block's length is not known).
    fn handle_generic_frame(
        &mut self, msg: &[u8], sink: &mut dyn FixSink, context: &mut Context, shared: &SharedState,
        event_tx: &Option<Sender<Event>>, hb: &mut HeartbeatState,
    ) {
        let Some(body) = find_body_after_tag(msg, b"35=G") else { return };
        let rx_farm = self.rx_farm;
        let blocks = crate::control::generic_values::blocks(body, |tag| {
            if self.exchange_map_subs.iter().any(|s| s.farm == rx_farm && s.server_tag == Some(tag)) {
                Some(626)
            } else if self.news.iter().any(|e| e.live && e.farm == rx_farm && e.tag == Some(tag)) {
                Some(292)
            } else if self.status.iter().any(|e| e.farm == rx_farm && e.tag == Some(tag)) {
                Some(MARKET_DATA_STATUS)
            } else {
                self.generic.iter().find(|e| e.farm == rx_farm && e.tag == Some(tag)).map(|e| e.code)
            }
        });
        for (tag, code, payload) in blocks {
            match code {
                Some(626) => self.handle_exchange_map(tag, payload, sink, shared, hb),
                Some(292) => self.handle_tick_news(tag, payload, shared, event_tx),
                Some(MARKET_DATA_STATUS) => self.handle_md_status(tag, payload, sink, context, shared, hb),
                Some(code) => self.handle_generic_block(tag, code, payload, context, shared),
                None => log::warn!("Generic tick for server tag {} of no known request: dropped", tag),
            }
        }
    }

    /// A whole 35=G frame for the exchange map tests: true when it carried
    /// the map of a subscription.
    #[cfg(test)]
    fn handle_exchange_map_frame(&mut self, msg: &[u8], sink: &mut dyn FixSink, shared: &SharedState, hb: &mut HeartbeatState) -> bool {
        let before = self.exchange_map_subs.len();
        self.handle_generic_frame(msg, sink, &mut Context::new(), shared, &None, hb);
        self.exchange_map_subs.len() < before
    }

    /// An exchange map block (ibx#441): the map is kept for its key and
    /// the subscription cancelled.
    fn handle_exchange_map(&mut self, tag: u32, payload: &[u8], sink: &mut dyn FixSink, shared: &SharedState, hb: &mut HeartbeatState) {
        let rx_farm = self.rx_farm;
        let Some(pos) = self.exchange_map_subs.iter().position(|s| s.farm == rx_farm && s.server_tag == Some(tag)) else {
            return;
        };
        let sub = self.exchange_map_subs.remove(pos);
        let map = parse_exchange_map(&exchange_map_text(payload));
        log::info!("Exchange map {}:{}: {} exchanges", sub.code, sub.sec_type_id, map.len());
        shared.reference.set_exchange_map_at(&sub.code, sub.sec_type_id, map, shared.market.md_events.position());
        self.send_exchange_map_request(&sub, "2", sink, hb);
    }

    /// A generic tick block (ibx#450): its values go into the contract's
    /// record, and the API ticks of the fields that changed to the
    /// requests that asked the tick, at their place among the farm
    /// messages.
    fn handle_generic_block(&mut self, tag: u32, code: i32, payload: &[u8], context: &Context, shared: &SharedState) {
        let rx_farm = self.rx_farm;
        let Some((instrument, entry_min_tick)) = self.generic.iter().find(|e| e.farm == rx_farm && e.tag == Some(tag))
            .map(|e| (e.instrument, e.min_tick)) else {
            return;
        };
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let ctx = crate::control::generic_values::DecodeCtx {
            min_tick: context.market.min_tick(instrument), now_ms,
            round_lot: context.market.round_lot(instrument), entry_min_tick,
        };
        let rec = self.generic_records.entry(instrument).or_default();
        let ticks = crate::control::generic_values::decode(code, payload, rec, ctx);
        if !ticks.is_empty() {
            shared.market.push_generic_ticks(crate::bridge::GenericTicks {
                at: shared.market.md_events.position(), instrument, code, ticks,
            });
        }
    }

    /// A market data status block (ibx#447): one 32-bit value, 1 when the
    /// contract's data is frozen (`generictick.ad.a(int,a,byte[],pa,int,
    /// MarketDataType,a)@24-74`; captured 07/10/2026: 1 for an option
    /// before the open, 2 for a stock in the pre-market, which is not
    /// frozen). Frozen: the frozen top of book is asked, the bid/ask and
    /// last pair with mode 2 and without the API source, on a slot of its
    /// own, and the requests of the contract go on with it (marketDataType
    /// 2). Not frozen any more: they go back to the real-time slot
    /// (marketDataType 1); the frozen pair is cancelled when they left it.
    fn handle_md_status(
        &mut self, tag: u32, payload: &[u8], sink: &mut dyn FixSink, context: &mut Context, shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        let rx_farm = self.rx_farm;
        let Some(pos) = self.status.iter().position(|e| e.farm == rx_farm && e.tag == Some(tag)) else { return };
        let Some(value) = payload.get(..4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]])) else {
            log::warn!("Market data status of server tag {} too short: {} bytes", tag, payload.len());
            return;
        };
        let frozen = value == 1;
        let (live, twin, farm) = (self.status[pos].instrument, self.status[pos].frozen, self.status[pos].farm);
        log::info!("Market data status of instrument {}: {} ({})", live, value, if frozen { "frozen" } else { "not frozen" });
        match (frozen, twin) {
            (true, None) => {
                let Some(info) = self.md_resub_info.iter().find(|(id, ..)| *id == live).cloned() else { return };
                let Ok(con_id) = self.status[pos].con_id.parse::<i64>() else { return };
                let Some(twin) = context.market.try_register_unresolved() else {
                    log::error!("Instrument table full: no slot for the frozen data of con_id {}", con_id);
                    return;
                };
                let (_, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, _, _) = info;
                context.market.set_symbol(twin, symbol.clone());
                context.market.set_routing(twin, &sec_type, &exchange);
                context.market.set_round_lot(twin, context.market.round_lot(live));
                shared.market.set_instrument_count(context.market.count());
                self.status[pos].frozen = Some(twin);
                let sub = MdSubscribe {
                    con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, instrument: twin,
                    mode_9887: crate::types::MarketDataModes::entry_mode(true, false), snapshot: false,
                };
                self.subscribe_top(&sub, farm, sink, hb);
                // Sent again from the status when the farm comes back.
                self.md_resub_info.retain(|(id, ..)| *id != twin);
                shared.market.push_md_reject(crate::bridge::MdReject::Frozen { instrument: live, frozen: twin });
            }
            (false, Some(twin)) => {
                self.status[pos].frozen = None;
                shared.market.push_md_reject(crate::bridge::MdReject::Live { instrument: twin, live });
            }
            _ => {}
        }
    }

    /// The status entry a top-of-book subscription of `instrument` starts
    /// in frozen mode (ibx#447), with its message; None when the contract
    /// has one.
    fn start_status(&mut self, sub: &MdSubscribe, farm: FarmId) -> Option<Vec<(u32, String)>> {
        if self.status.iter().any(|e| e.instrument == sub.instrument) {
            return None;
        }
        let farm_req = self.next_md_req_id;
        self.next_md_req_id += 1;
        let entry = StatusEntry {
            farm_req, instrument: sub.instrument, farm, con_id: sub.con_id.to_string(),
            exchange: routing_exchange(&sub.exchange, &sub.sec_type).to_string(),
            sec_type: fix_sec_type(&sub.sec_type).to_string(), tag: None, frozen: None,
        };
        let msg = status_message(&entry, true);
        self.status.push(entry);
        Some(msg)
    }

    /// The real-time slot of a frozen slot (ibx#447).
    pub(crate) fn frozen_live(&self, frozen: InstrumentId) -> Option<InstrumentId> {
        self.status.iter().find(|e| e.frozen == Some(frozen)).map(|e| e.instrument)
    }

    /// The frozen slot of a real-time slot whose contract is frozen
    /// (ibx#447).
    pub(crate) fn frozen_slot(&self, live: InstrumentId) -> Option<InstrumentId> {
        self.status.iter().find(|e| e.instrument == live).and_then(|e| e.frozen)
    }

    /// The top of book of an instrument ended (ibx#447): its status entry
    /// goes, with its cancel when it is on the wire, after the pair as the
    /// reference (captured 07/10/2026).
    pub(crate) fn stop_status(&mut self, instrument: InstrumentId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let Some(pos) = self.status.iter().position(|e| e.instrument == instrument) else { return Vec::new() };
        let e = self.status.remove(pos);
        vec![(e.farm, status_message(&e, false))]
    }

    /// The status entries of a lost farm (ibx#447): they are asked again
    /// with their top of book; a frozen contract goes back to its
    /// real-time slot until the status says frozen again.
    fn status_farm_lost(&mut self, farm: FarmId) {
        let mut thawed = Vec::new();
        self.status.retain(|e| {
            if e.farm != farm { return true; }
            if let Some(twin) = e.frozen {
                thawed.push((twin, e.instrument));
            }
            false
        });
        self.thawed.extend(thawed);
    }

    /// The frozen slots left without a farm message: (frozen slot,
    /// real-time slot).
    pub(crate) fn take_thawed(&mut self) -> Vec<(InstrumentId, InstrumentId)> {
        std::mem::take(&mut self.thawed)
    }

    fn handle_ticker_setup(&mut self, msg: &[u8], context: &mut Context) {
        let body = match find_body_after_tag(msg, b"35=L\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 { return; }
        let con_id: i64 = match parts[0].parse() { Ok(v) => v, Err(_) => return };
        let min_tick: f64 = parts[1].parse().unwrap_or(0.01);
        let server_tag: u32 = match parts[2].parse() { Ok(v) => v, Err(_) => return };

        // The slot of the contract's market data subscription: a contract
        // asked without a conId has a slot of its own, apart from the slot
        // its orders registered before (ibx#487, premarket_order_types of
        // 28/09/2026: the trade fields went to the orders' slot).
        let subscribed = context.market.active_instruments()
            .filter(|&(id, c)| c == con_id && self.has_md_subscription(id))
            .map(|(id, _)| id).next();
        if let Some(instrument) = subscribed.or_else(|| context.market.instrument_by_con_id(con_id)) {
            // A trade stream tag, kept apart from the quote tags (#292),
            // with the tick and the size increment (when present, ibx#287)
            // its trades are scaled by.
            let size_min_tick = parts.get(4).and_then(|v| v.parse::<f64>().ok());
            context.market.register_trade_tag_sized(self.rx_farm, server_tag, instrument, min_tick, size_min_tick);
            log::info!("Ticker setup: con_id {} -> server_tag {}, minTick {}", con_id, server_tag, min_tick);
        }
    }

    /// The top-of-book subscription of an instrument, sent or kept for the
    /// next reconnect: whether it is a snapshot; None without one (ibx#444).
    pub(crate) fn top_snapshot(&self, instrument: InstrumentId) -> Option<bool> {
        self.md_resub_info.iter().find(|(id, ..)| *id == instrument).map(|info| info.9)
    }

    /// A live market data subscription uses the instrument: sent, or kept
    /// for the next reconnect while the farm is down (ibx#291).
    pub(crate) fn has_md_subscription(&self, instrument: InstrumentId) -> bool {
        self.instrument_md_reqs.iter().any(|(id, _)| *id == instrument)
            || self.md_resub_info.iter().any(|(id, ..)| *id == instrument)
            || self.news.iter().any(|e| e.instrument == instrument)
            || self.news_waiting.iter().any(|(id, ..)| *id == instrument)
    }

    /// Start the news entry of a request on `farm` (ibx#458) and build its
    /// message, as the reference writes it: `207=NEWS`, the contract's
    /// security type, `264=292`, the provider key in `6472` when not empty,
    /// the streaming-client mark and the API flag. A request whose key the
    /// contract already has shares that entry: nothing is sent (ibx#444).
    pub(crate) fn start_news(&mut self, instrument: InstrumentId, con_id: i64, sec_type: &str, providers: &str, farm: FarmId)
        -> Vec<(FarmId, Vec<(u32, String)>)>
    {
        if let Some(e) = self.news.iter_mut().find(|e| e.instrument == instrument && e.providers == providers) {
            e.refs += 1;
            log::info!("News entry {} shared by {} requests: con_id={} providers={}", e.farm_req, e.refs, con_id, providers);
            return Vec::new();
        }
        let farm_req = self.next_md_req_id;
        self.next_md_req_id += 1;
        let entry = NewsEntry {
            farm_req, instrument, farm, con_id: con_id.to_string(), sec_type: fix_sec_type(sec_type).to_string(),
            providers: providers.to_string(), tag: None, live: true, seen: Default::default(), refs: 1,
        };
        let msg = news_message(&entry, true);
        log::info!("News entry {} on farm {}: con_id={} providers={}", farm_req, farm, con_id, providers);
        self.news.push(entry);
        vec![(farm, msg)]
    }

    /// Forget the news ticks of an instrument and build the cancels of
    /// those on the wire (ibx#458).
    pub(crate) fn stop_news(&mut self, instrument: InstrumentId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        self.news_waiting.retain(|(id, ..)| *id != instrument);
        let mut out = Vec::new();
        self.news.retain(|e| {
            if e.instrument != instrument { return true; }
            if e.live { out.push((e.farm, news_message(e, false))); }
            false
        });
        out
    }

    /// One request with this provider key left an instrument that others
    /// still use (ibx#444): its share of the news entry goes, and the entry
    /// is cancelled when no request uses it.
    pub(crate) fn release_news(&mut self, instrument: InstrumentId, providers: &str) -> Vec<(FarmId, Vec<(u32, String)>)> {
        if let Some(pos) = self.news_waiting.iter().position(|(id, p, _)| *id == instrument && p == providers) {
            self.news_waiting.remove(pos);
            return Vec::new();
        }
        let Some(pos) = self.news.iter().position(|e| e.instrument == instrument && e.providers == providers) else {
            return Vec::new();
        };
        self.news[pos].refs = self.news[pos].refs.saturating_sub(1);
        if self.news[pos].refs > 0 {
            return Vec::new();
        }
        let e = self.news.remove(pos);
        if e.live { vec![(e.farm, news_message(&e, false))] } else { Vec::new() }
    }

    /// Every news entry's cancel (shutdown).
    pub(crate) fn stop_all_news(&mut self) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut ids: Vec<InstrumentId> = self.news.iter().map(|e| e.instrument).collect();
        ids.dedup();
        ids.into_iter().flat_map(|id| self.stop_news(id)).collect()
    }

    /// A farm's connection was lost: its news entries are no longer on the
    /// wire and go out again, with new ids, when it is back.
    pub(crate) fn news_farm_lost(&mut self, farm: FarmId) {
        for e in self.news.iter_mut().filter(|e| e.farm == farm) {
            e.live = false;
            e.tag = None;
        }
    }

    /// Send again the news entries of a farm that came back.
    pub(crate) fn resend_news(&mut self, farm: FarmId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut out = Vec::new();
        for i in 0..self.news.len() {
            if self.news[i].farm != farm || self.news[i].live { continue; }
            self.news[i].farm_req = self.next_md_req_id;
            self.next_md_req_id += 1;
            self.news[i].live = true;
            out.push((farm, news_message(&self.news[i], true)));
        }
        out
    }

    /// Start the generic ticks of a request on `farm` (ibx#450): request
    /// codes, of a contract of this security type, asked on `exchange`,
    /// listed on `primary`. A tick the contract has already is shared; a
    /// tick that is not sent or not valid for the contract is left out; the
    /// others are new entries, in request code order: those that go at
    /// once, and all of them once the top of book was acknowledged, are in
    /// the returned message; the others wait for that acknowledgement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_generic(
        &mut self, instrument: InstrumentId, con_id: i64, exchange: &str, sec_type: &str, primary: &str,
        codes: &[i32], farm: FarmId,
    ) -> Vec<(FarmId, Vec<(u32, String)>)> {
        use crate::control::generic_values as gv;
        let mut codes: Vec<i32> = codes.iter().map(|&c| if c == 104 { gv::HISTORICAL_VOLATILITY } else { c }).collect();
        codes.sort_unstable();
        codes.dedup();
        let confirmed = self.top_confirmed.contains(&instrument);
        let mut now: Vec<usize> = Vec::new();
        for code in codes {
            if !gv::sent(code) || !gv::valid_for(code, sec_type) {
                log::info!("Generic tick {} not sent for a {} contract", code, sec_type);
                continue;
            }
            if let Some(e) = self.generic.iter_mut().find(|e| e.instrument == instrument && e.code == code) {
                e.refs += 1;
                continue;
            }
            let routing = routing_exchange(exchange, sec_type);
            self.generic.push(GenericEntry {
                farm_req: 0, instrument, farm, code, con_id: con_id.to_string(),
                exchange: gv::entry_exchange(code, sec_type, routing, primary).to_string(),
                sec_type: fix_sec_type(sec_type).to_string(), tag: None, min_tick: 0.0, refs: 1,
            });
            if gv::at_once(code) || confirmed {
                let i = self.generic.len() - 1;
                self.generic[i].farm_req = self.next_md_req_id;
                self.next_md_req_id += 1;
                now.push(i);
            }
        }
        if now.is_empty() {
            return Vec::new();
        }
        let mut msg = md_message_head("1", now.len());
        for &i in &now {
            msg.extend(generic_entry(&self.generic[i], true));
        }
        vec![(farm, msg)]
    }

    /// The top of book of an instrument was acknowledged (ibx#450): its
    /// generic tick entries that waited for it get their request ids, in
    /// request code order; their indexes.
    fn confirm_top(&mut self, instrument: InstrumentId) -> Vec<usize> {
        if !self.top_confirmed.contains(&instrument) {
            self.top_confirmed.push(instrument);
        }
        let mut waiting: Vec<usize> = (0..self.generic.len())
            .filter(|&i| self.generic[i].instrument == instrument && self.generic[i].farm_req == 0)
            .collect();
        waiting.sort_by_key(|&i| self.generic[i].code);
        for &i in &waiting {
            self.generic[i].farm_req = self.next_md_req_id;
            self.next_md_req_id += 1;
        }
        waiting
    }

    /// A request with generic ticks left an instrument other requests use
    /// (ibx#450): the entries no request needs any more are cancelled.
    pub(crate) fn release_generic(&mut self, instrument: InstrumentId, codes: &[i32]) -> Vec<(FarmId, Vec<(u32, String)>)> {
        use crate::control::generic_values as gv;
        let codes: Vec<i32> = codes.iter().map(|&c| if c == 104 { gv::HISTORICAL_VOLATILITY } else { c }).collect();
        for e in self.generic.iter_mut().filter(|e| e.instrument == instrument && codes.contains(&e.code)) {
            e.refs = e.refs.saturating_sub(1);
        }
        self.cancel_generic(|e| e.instrument == instrument && e.refs == 0)
    }

    /// Forget the generic ticks of an instrument (its last request went)
    /// and build the cancels of its entries on the wire (ibx#450).
    pub(crate) fn stop_generic(&mut self, instrument: InstrumentId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        self.generic_waiting.retain(|w| w.instrument != instrument);
        self.top_confirmed.retain(|i| *i != instrument);
        self.generic_records.remove(&instrument);
        self.cancel_generic(|e| e.instrument == instrument)
    }

    /// Remove the entries `which` takes; the cancel of those on the wire,
    /// one message per farm, in descending request code order as the
    /// reference's (captured 05/10/2026).
    fn cancel_generic(&mut self, which: impl Fn(&GenericEntry) -> bool) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut gone: Vec<GenericEntry> = Vec::new();
        self.generic.retain(|e| {
            if which(e) {
                gone.push(e.clone());
                false
            } else {
                true
            }
        });
        gone.retain(|e| e.farm_req != 0);
        gone.sort_by_key(|e| std::cmp::Reverse(e.code));
        let mut farms: Vec<FarmId> = gone.iter().map(|e| e.farm).collect();
        farms.sort_unstable();
        farms.dedup();
        farms.into_iter().map(|farm| {
            let mine: Vec<&GenericEntry> = gone.iter().filter(|e| e.farm == farm).collect();
            let mut msg = md_message_head("2", mine.len());
            for e in mine {
                msg.extend(generic_entry(e, false));
            }
            (farm, msg)
        }).collect()
    }

    /// A farm's connection was lost (ibx#450): its generic tick entries
    /// are off the wire; they go again once their top of book is
    /// acknowledged again.
    pub(crate) fn generic_farm_lost(&mut self, farm: FarmId) {
        let mut lost: Vec<InstrumentId> = Vec::new();
        for e in self.generic.iter_mut().filter(|e| e.farm == farm) {
            e.farm_req = 0;
            e.tag = None;
            lost.push(e.instrument);
        }
        self.top_confirmed.retain(|i| !lost.contains(i));
    }

    /// `subscribe_top` to the primary farm, from the fields of a
    /// subscription.
    #[cfg(test)]
    pub(crate) fn send_mktdata_subscribe(
        &mut self,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        last_trade_date: &str,
        strike: f64,
        right: &str,
        multiplier: &str,
        instrument: InstrumentId,
        mode_9887: i32,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let sub = MdSubscribe {
            con_id, symbol: symbol.to_string(), exchange: exchange.to_string(), sec_type: sec_type.to_string(),
            last_trade_date: last_trade_date.to_string(), strike, right: right.to_string(),
            multiplier: multiplier.to_string(), instrument, mode_9887, snapshot: false,
        };
        self.subscribe_top(&sub, PRIMARY_MD, farm_conn, hb);
    }

    /// Subscribe to the top of book of a contract on `farm`, the farm its
    /// routing row names (#445). Always the bid/ask and last pair, as the
    /// reference; a frozen / delayed mode rides on each entry (ibx#447,
    /// captured 28/09/2026). Each entry carries the contract's own
    /// exchange and security type, as the reference writes them.
    pub(crate) fn subscribe_top(&mut self, sub: &MdSubscribe, farm: FarmId, sink: &mut dyn FixSink, hb: &mut HeartbeatState) {
        // The reference always subscribes by conId; one without it is
        // resolved first (ibx#278).
        let (con_id, instrument, mode_9887) = (sub.con_id, sub.instrument, sub.mode_9887);
        if con_id <= 0 {
            log::error!("Market data subscribe for instrument {} without a conId: not sent", instrument);
            return;
        }
        let realtime = mode_9887 == 0;
        // In frozen mode the contract's market data status is asked first,
        // in a message of its own, without the API source (ibx#447): the
        // reference's frozen observer registers before the request does
        // (`jextend.s.a(dy,ec,Set)@490-500`, `jclient.record.o.a(b_,int)`
        // -> `o.a0()`). Its sender sometimes puts the entry after the pair,
        // then with the API source (captured 05/10/2026 and 07/10/2026).
        if realtime && !sub.snapshot && self.md_modes.frozen {
            if let Some(msg) = self.start_status(sub, farm) {
                sink.send_comp(&borrow_tags(&msg));
            }
        }
        let bid_ask_id = self.next_md_req_id;
        let last_id = self.next_md_req_id + 1;
        self.next_md_req_id += 2;

        self.md_req_to_instrument.push((bid_ask_id, instrument));
        self.md_req_to_instrument.push((last_id, instrument));

        match self.instrument_md_reqs.iter_mut().find(|(id, _)| *id == instrument) {
            Some((_, reqs)) => {
                reqs.push(bid_ask_id);
                reqs.push(last_id);
            }
            None => {
                self.instrument_md_reqs.push((instrument, vec![bid_ask_id, last_id]));
            }
        }
        if self.md_resub_info.iter().all(|(id, ..)| *id != instrument) {
            self.md_resub_info.push((instrument, sub.symbol.clone(), sub.exchange.clone(), sub.sec_type.clone(),
                sub.last_trade_date.clone(), sub.strike, sub.right.clone(), sub.multiplier.clone(), mode_9887, sub.snapshot));
        }

        let con_id_str = con_id.to_string();
        let exchange = routing_exchange(&sub.exchange, &sub.sec_type);
        let sec_type = fix_sec_type(&sub.sec_type);
        let entries = [
            (bid_ask_id, bid_ask_exchange(exchange, &sub.sec_type), "442"),
            (last_id, exchange, "443"),
        ];
        for (req_id, exch, req_type) in entries {
            self.md_entries.push(MdEntry {
                req_id, instrument, farm, con_id: con_id_str.clone(), exchange: exch.to_string(),
                sec_type: sec_type.to_string(), req_type, mode_9887,
            });
        }

        let ids: Vec<String> = entries.iter().map(|(r, ..)| r.to_string()).collect();
        let mode_str = mode_9887.to_string();
        let frozen_mode = mode_9887 == crate::types::MarketDataModes::entry_mode(true, false)
            || mode_9887 == crate::types::MarketDataModes::entry_mode(true, true);
        let ts = chrono_free_timestamp();
        // A snapshot is asked with the snapshot action and without the
        // streaming-client mark, as the reference asks it (ibx#446).
        let mut tags: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (fix::TAG_SENDING_TIME, &ts),
            (263, if sub.snapshot { "3" } else { "1" }),
            (146, "2"),
        ];
        for ((_, exch, req_type), id) in entries.iter().zip(&ids) {
            tags.push((262, id));
            tags.push((6008, &con_id_str));
            tags.push((207, exch));
            tags.push((167, sec_type));
            tags.push((264, req_type));
            // The frozen pair is not asked by the API client's request
            // (captured 07/10/2026: no API source on it).
            if !sub.snapshot && !frozen_mode { tags.push((6088, "Socket")); }
            if !realtime { tags.push((9887, &mode_str)); }
            tags.push((9830, "1"));
        }
        if sink.send_comp(&tags) {
            log::info!("Sent 35=V {} (9887={}) on farm {}: con_id={} {} {} ids={},{}",
                if sub.snapshot { "snapshot" } else { "subscribe" },
                mode_9887, farm, con_id, exchange, sec_type, bid_ask_id, last_id);
            if farm == PRIMARY_MD {
                hb.last_farm_sent = Instant::now();
            }
        }
    }

    /// Regulatory snapshot request (ibx#446): one entry with action
    /// SNAPSHOT and request type 624, flagged for an API client, without
    /// the streaming-client mark.
    pub(crate) fn subscribe_snapshot(&mut self, sub: &MdSubscribe, farm: FarmId, sink: &mut dyn FixSink, hb: &mut HeartbeatState) {
        if sub.con_id <= 0 {
            log::error!("Regulatory snapshot for instrument {} without a conId: not sent", sub.instrument);
            return;
        }
        let id = self.next_md_req_id;
        self.next_md_req_id += 1;
        self.md_req_to_instrument.push((id, sub.instrument));
        self.snapshot_reqs.push((id, sub.instrument));
        let con_id_str = sub.con_id.to_string();
        let id_str = id.to_string();
        let exchange = routing_exchange(&sub.exchange, &sub.sec_type);
        let sec_type = fix_sec_type(&sub.sec_type);
        let ts = chrono_free_timestamp();
        let tags: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (fix::TAG_SENDING_TIME, &ts),
            (263, "3"),
            (146, "1"),
            (262, &id_str),
            (6008, &con_id_str),
            (207, exchange),
            (167, sec_type),
            (264, "624"),
            (9830, "1"),
        ];
        if sink.send_comp(&tags) {
            log::info!("Sent 35=V regulatory snapshot on farm {}: con_id={} {} {} id={}", farm, sub.con_id, exchange, sec_type, id);
            if farm == PRIMARY_MD {
                hb.last_farm_sent = Instant::now();
            }
        }
    }

    /// Forget the request of a regulatory snapshot (ibx#446).
    pub(crate) fn drop_snapshot(&mut self, instrument: InstrumentId) {
        let ids: Vec<u32> = self.snapshot_reqs.iter().filter(|(_, i)| *i == instrument).map(|(id, _)| *id).collect();
        self.snapshot_reqs.retain(|(_, i)| *i != instrument);
        self.md_req_to_instrument.retain(|(id, _)| !ids.contains(id));
    }

    /// Whether a live top-of-book or news request uses `farm` (#445).
    pub(crate) fn uses_farm(&self, farm: FarmId) -> bool {
        self.md_entries.iter().any(|e| e.farm == farm) || self.news.iter().any(|e| e.farm == farm)
    }

    /// The subscriptions that have no entry on the wire (their farm was
    /// lost), to be sent again by their route (#445).
    pub(crate) fn unsent_subscriptions(&self, context: &Context) -> Vec<MdSubscribe> {
        self.md_resub_info.iter()
            .filter(|(id, ..)| self.instrument_md_reqs.iter().all(|(i, _)| i != id))
            .filter_map(|(id, symbol, exchange, sec_type, ltd, strike, right, mult, mode, snapshot)| {
                context.market.con_id(*id).map(|con_id| MdSubscribe {
                    con_id, symbol: symbol.clone(), exchange: exchange.clone(), sec_type: sec_type.clone(),
                    last_trade_date: ltd.clone(), strike: *strike, right: right.clone(), multiplier: mult.clone(),
                    instrument: *id, mode_9887: *mode, snapshot: *snapshot,
                })
            })
            .collect()
    }

    /// The connection of `farm` was lost (#445): its entries and server
    /// tags are gone; the subscriptions stay, to be sent again. Quotes of
    /// the contracts it served are zeroed.
    pub(crate) fn farm_lost(&mut self, farm: FarmId, context: &mut Context) {
        // An exchange map not received yet is asked again at the next
        // acknowledgement of its code (ibx#441).
        self.exchange_map_subs.retain(|s| s.farm != farm);
        self.generic_farm_lost(farm);
        self.status_farm_lost(farm);
        let lost: Vec<MdEntry> = self.md_entries.iter().filter(|e| e.farm == farm).cloned().collect();
        self.md_entries.retain(|e| e.farm != farm);
        for e in &lost {
            self.md_req_to_instrument.retain(|(r, _)| *r != e.req_id);
            for (_, reqs) in self.instrument_md_reqs.iter_mut() {
                reqs.retain(|r| *r != e.req_id);
            }
            context.market.zero_quote(e.instrument);
        }
        self.instrument_md_reqs.retain(|(_, reqs)| !reqs.is_empty());
        context.market.clear_farm_tags(farm);
    }

    /// A market data reject (35=3) on the farm (ibx#444, ibx#447): 262 is
    /// the `;` list of rejected request ids, 9887 the per-id "delayed data
    /// available" flag, 6763 the per-id API access. For each top-of-book
    /// subscription hit: with delayed data enabled (reqMarketDataType 3 or
    /// 4) and available, it goes on with delayed data, asked again with
    /// 9887=1 on new ids, as the reference (captured 28/09/2026); else it
    /// stops, 354 or 10089. Other ids (depth) are left to their handlers.
    fn handle_md_reject(
        &mut self,
        msg: &[u8],
        context: &mut Context,
        shared: &SharedState,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let parsed = fix::fix_parse(msg);
        let list = |tag: u32| -> Vec<String> {
            parsed.get(&tag).map(|v| v.split(';').map(String::from).collect()).unwrap_or_default()
        };
        let ids = list(262);
        let delayed_flags = list(9887);
        let access = list(6763);
        log::warn!("Farm market data reject: ids={:?} 9887={:?} 6763={:?} 58={:?}",
            ids, delayed_flags, access, parsed.get(&58));
        // (instrument, delayed available, API subscription needed), in order.
        let mut hit: Vec<(InstrumentId, bool, bool)> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            if let Some(a) = access.get(i).filter(|a| api_subscription_needed(a)) {
                self.note_refused_api_subscription(a);
            }
            let Ok(id) = id.parse::<u32>() else { continue };
            let Some(&(_, instrument)) = self.md_req_to_instrument.iter().find(|(r, _)| *r == id) else {
                // A depth entry (#452).
                let needs_sub = access.get(i).is_some_and(|a| api_subscription_needed(a));
                self.depth_rejected(id, needs_sub, shared, farm_conn);
                continue;
            };
            let delayed = delayed_flags.get(i).is_some_and(|f| f == "1");
            let needs_sub = access.get(i).is_some_and(|a| api_subscription_needed(a));
            match hit.iter_mut().find(|(inst, ..)| *inst == instrument) {
                Some(h) => { h.1 |= delayed; h.2 |= needs_sub; }
                None => hit.push((instrument, delayed, needs_sub)),
            }
        }
        let delayed_enabled = self.md_modes.delayed;
        for (instrument, delayed_available, needs_api_subscription) in hit {
            if delayed_enabled && delayed_available {
                // Asked again with delayed data; the rejected ids stay with
                // the subscription, so a cancel covers them too.
                let Some(info) = self.md_resub_info.iter_mut().find(|(id, ..)| *id == instrument) else { continue };
                // The delayed entry mode, as captured for type 3. With type
                // 4 too: when the reference asks for delayed-frozen data
                // instead is not known (ibx#447).
                info.8 = crate::types::MarketDataModes::entry_mode(false, true);
                let (_, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, snapshot) = info.clone();
                let Some(con_id) = context.market.con_id(instrument) else { continue };
                let sub = MdSubscribe {
                    con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, instrument, mode_9887, snapshot,
                };
                // Asked again on the farm that rejected it.
                self.subscribe_top(&sub, self.rx_farm, farm_conn, hb);
                shared.market.push_md_reject(crate::bridge::MdReject::Delayed { instrument });
            } else {
                // The subscription stops; its entries stay with the
                // contract's record until its request ends, which cancels
                // them (captured 05/10/2026: 354, then 263=2 of the rejected
                // entries and of the generic ones).
                self.md_resub_info.retain(|(id, ..)| *id != instrument);
                let con_id = context.market.con_id(instrument);
                let description = con_id.and_then(|c| context.depth_descriptions.get(&c).cloned()).unwrap_or_default();
                let kept_params = con_id.and_then(|c| self.con_params.get(&c).cloned());
                shared.market.push_md_reject(crate::bridge::MdReject::NotSubscribed {
                    instrument, delayed_available, needs_api_subscription, description, kept_params,
                });
            }
        }
    }

    /// Cancel the top of book of an instrument on its primary-farm
    /// connection; the routed form is `unsubscribe_top`.
    #[cfg(test)]
    pub(crate) fn send_mktdata_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        for (farm, msg) in self.unsubscribe_top(instrument) {
            let fields: Vec<(u32, &str)> = msg.iter().map(|(t, v)| (*t, v.as_str())).collect();
            if farm == PRIMARY_MD && farm_conn.send_comp(&fields) {
                hb.last_farm_sent = Instant::now();
            }
        }
    }

    /// Forget the top-of-book subscription of an instrument and build its
    /// cancels: one message per farm, with the entries of the subscribe,
    /// as the reference cancels (#445).
    pub(crate) fn unsubscribe_top(&mut self, instrument: InstrumentId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        // Before the lookup: while the farm is down the request ids are
        // already cleared, and the subscription must still not come back on
        // reconnect (ibx#288).
        self.md_resub_info.retain(|(id, ..)| *id != instrument);
        self.cancel_top_entries(instrument)
    }

    /// The cancels of an instrument's top-of-book entries on the wire,
    /// which are forgotten; the subscription itself is left as it is.
    fn cancel_top_entries(&mut self, instrument: InstrumentId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let reqs = match self.instrument_md_reqs.iter()
            .position(|(id, _)| *id == instrument)
        {
            Some(idx) => {
                let (_, reqs) = self.instrument_md_reqs.remove(idx);
                reqs
            }
            None => return Vec::new(),
        };
        // A late ack for a cancelled request id then matches nothing and is
        // dropped, as the reference does: it can never bind to the contract
        // that reuses this slot (ibx#289).
        self.md_req_to_instrument.retain(|(r, _)| !reqs.contains(r));
        let mut entries: Vec<MdEntry> = self.md_entries.iter().filter(|e| reqs.contains(&e.req_id)).cloned().collect();
        self.md_entries.retain(|e| !reqs.contains(&e.req_id));
        // By request type, the newest entry first: a subscription asked
        // again with delayed data cancels its delayed and rejected entries
        // as 442 delayed, 442, 443 delayed, 443 (captured 05/10/2026).
        entries.sort_by_key(|e| (e.req_type, std::cmp::Reverse(e.req_id)));

        let mut out: Vec<(FarmId, Vec<(u32, String)>)> = Vec::new();
        let mut farms: Vec<FarmId> = entries.iter().map(|e| e.farm).collect();
        farms.sort_unstable();
        farms.dedup();
        for farm in farms {
            let mine: Vec<&MdEntry> = entries.iter().filter(|e| e.farm == farm).collect();
            let mut msg: Vec<(u32, String)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
                (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
                (263, "2".into()),
                (146, mine.len().to_string()),
            ];
            for e in mine {
                msg.push((262, e.req_id.to_string()));
                msg.push((6008, e.con_id.clone()));
                msg.push((207, e.exchange.clone()));
                msg.push((167, e.sec_type.clone()));
                msg.push((264, e.req_type.to_string()));
                if e.mode_9887 != 0 {
                    msg.push((9887, e.mode_9887.to_string()));
                }
                msg.push((9830, "1".into()));
            }
            out.push((farm, msg));
        }
        out
    }

    /// The API subscription names of a refusal's API access value (6763)
    /// that needs one, kept until the next data permission change
    /// (ibx#421): the items of a `,` or `#` list, or the value itself,
    /// without "", "0" and "none" (`jextend.F.a(String, Collection,
    /// String)`, `jextend.F.b(Collection)`).
    fn note_refused_api_subscription(&mut self, access: &str) {
        let names: Vec<&str> = if access.contains(',') {
            access.split(',').collect()
        } else if access.contains('#') {
            access.split('#').collect()
        } else {
            vec![access]
        };
        for name in names {
            if matches!(name, "" | "0" | "none") || self.refused_api_subscriptions.iter().any(|n| n == name) {
                continue;
            }
            self.refused_api_subscriptions.push(name.to_string());
        }
    }

    #[cfg(test)]
    pub(crate) fn note_refused_api_subscription_for_test(&mut self, access: &str) {
        self.note_refused_api_subscription(access);
    }

    /// A data permission change (ibx#421): true when a refusal named an
    /// API subscription since the last change (the reference then sends
    /// 2134 to its API clients, `jextend.F.onPermissionsChanged()`,
    /// `jextend.dL.K()`); the names are cleared.
    pub(crate) fn take_refused_api_subscriptions(&mut self) -> bool {
        let any = !self.refused_api_subscriptions.is_empty();
        self.refused_api_subscriptions.clear();
        any
    }

    /// The desubscription of the reference's market data resubscription
    /// after a data permission change (ibx#421, `jclient.ij.b(boolean)`,
    /// `jclient.is.v()`): every streaming top-of-book subscription has its
    /// entries cancelled on their farms, and every news entry on the wire
    /// is cancelled; the subscriptions stay, and go out again, on new
    /// ids, with [`Self::unsent_subscriptions`] and
    /// [`Self::resend_news`]. Snapshots are left out. Returns the cancels.
    pub(crate) fn desubscribe_all(&mut self) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let streaming: Vec<InstrumentId> = self.md_resub_info.iter()
            .filter(|(.., snapshot)| !snapshot)
            .map(|(id, ..)| *id)
            .collect();
        let mut out = Vec::new();
        for instrument in streaming {
            out.extend(self.cancel_top_entries(instrument));
        }
        for e in self.news.iter_mut().filter(|e| e.live) {
            out.push((e.farm, news_message(e, false)));
            e.live = false;
            e.tag = None;
        }
        out
    }

    /// The farms with news entries off the wire (ibx#421).
    pub(crate) fn news_farms_to_resend(&self) -> Vec<FarmId> {
        let mut farms: Vec<FarmId> = self.news.iter().filter(|e| !e.live).map(|e| e.farm).collect();
        farms.sort_unstable();
        farms.dedup();
        farms
    }

    /// Start a depth request (#452) with what it subscribes, each entry on
    /// the farm of its route: a book entry for each book; for each top of
    /// book a bid/ask entry and its LAST entry where `last` says (one LAST
    /// entry per farm and exchange in a request: the others only take a
    /// farm id, as in the reference). The caller made the local checks and
    /// picked the farms. `lot` is the round lot of the contract's sizes;
    /// `l2` says a single book has market makers (updateMktDepthL2) (#451);
    /// `description` is the contract as a refusal names it. An entry that
    /// another request has on the wire is shared, as the reference shares
    /// its book: not sent again, and what the farm said of it applies at
    /// once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_depth(
        &mut self,
        req_id: ReqId,
        con_id: i64,
        sec_type: &str,
        smart: bool,
        num_rows: i32,
        lot: i64,
        l2: bool,
        specs: Vec<DepthSpec>,
        description: Option<&str>,
        shared: &SharedState,
    ) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let view = if smart {
            DepthView::Smart(SmartMerge::new(req_id, num_rows))
        } else {
            DepthView::Single(SingleView::new(req_id, num_rows, l2))
        };
        let fix_type = fix_sec_type(sec_type).to_string();
        let con = con_id.to_string();
        let entry = |farm: FarmId, exchange: &str, req_type: &'static str, name: &Arc<str>| DepthEntry {
            farm_req: 0, farm, con_id: con.clone(), exchange: exchange.to_string(), sec_type: fix_type.clone(), req_type,
            live: false, server_tag: None, scale: None, book: None, top: TopQuote::default(), name: name.clone(),
            resubscribe_at: None, access: DepthAccess::Unknown, skip_ids: 0,
        };
        let mut entries: Vec<DepthEntry> = Vec::new();
        let (mut books, mut tops) = (Vec::new(), Vec::new());
        for spec in specs {
            match spec {
                DepthSpec::Book { farm, exchange } => {
                    let name: Arc<str> = Arc::from(exchange.as_str());
                    entries.push(entry(farm, &exchange, "0", &name));
                    books.push(name);
                }
                DepthSpec::Top { farm, exchange, last } => {
                    let name: Arc<str> = Arc::from(exchange.as_str());
                    entries.push(entry(farm, &exchange, "442", &name));
                    if let Some((last_farm, last_exchange)) = last {
                        if entries.iter().any(|e| e.req_type == "443" && e.farm == last_farm && e.exchange == last_exchange) {
                            if let Some(e) = entries.last_mut() {
                                e.skip_ids = 1;
                            }
                        } else {
                            entries.push(entry(last_farm, &last_exchange, "443", &name));
                        }
                    }
                    tops.push(name);
                }
            }
        }
        let status = smart.then(|| SmartStatus::new(books, tops, Instant::now() + SMART_STATUS_WAIT));
        let refusal_suffix = description.map(|d| format!("{d}/DEEP")).unwrap_or_default();
        self.depth_reqs.push(DepthReq { req_id, con_id, smart, entries, lot: lot.max(1), view, status, refusal_suffix });
        let idx = self.depth_reqs.len() - 1;
        let (msgs, joined) = self.depth_messages(idx, None);
        if !joined.is_empty() {
            self.depth_joined(idx, &joined, shared);
        }
        msgs
    }

    /// Entries of request `idx` that joined entries already on the wire
    /// (#452): what the farm said of them applies at once, as for a new
    /// listener of the reference's book. A single book the farm accepted
    /// gives its whole book now; one it refused ends the request (354 or
    /// 10089). A SmartDepth component gives its rows now and is answered.
    fn depth_joined(&mut self, idx: usize, joined: &[usize], shared: &SharedState) {
        let mut out = std::mem::take(&mut self.depth_out);
        let mut refused = None;
        let DepthReq { req_id, entries, view, status, .. } = &mut self.depth_reqs[idx];
        for &ei in joined {
            let e = &entries[ei];
            match (&mut *view, e.access) {
                (_, DepthAccess::Unknown) => {}
                (DepthView::Single(v), DepthAccess::Allowed) => {
                    if let Some(book) = &e.book {
                        v.on_change(book, &[], &mut out);
                    }
                }
                (DepthView::Single(_), DepthAccess::Refused { api_subscription }) => refused = Some(api_subscription),
                (DepthView::Smart(m), access) => {
                    let ok = access == DepthAccess::Allowed;
                    if ok {
                        if let Some(book) = &e.book {
                            m.set_book(&e.name, book, &mut out);
                        } else if let (true, Some(scale)) = (e.req_type == "442", e.scale.as_ref()) {
                            m.set_top(&e.name, &e.top, scale, &mut out);
                        }
                    }
                    if matches!(e.req_type, "0" | "442")
                        && let Some(text) = status.as_mut().and_then(|s| s.answer(&e.name, e.req_type == "0", ok))
                    {
                        shared.orders.push_order_error(*req_id, 2152, text);
                    }
                }
            }
        }
        for u in out.drain(..) {
            shared.market.push_depth_update(u);
        }
        self.depth_out = out;
        if let Some(api_subscription) = refused {
            // The entry stays with the request that has it.
            let req = self.depth_reqs.remove(idx);
            let (code, text) = depth_refusal(api_subscription, &req.refusal_suffix);
            shared.orders.push_order_error(req.req_id, code, text);
        }
    }

    /// Send the entries of a depth request that are not on the wire (all of
    /// them, or those of one farm after it came back), with new farm ids,
    /// for the caller to send: each book in a message of its own, then the
    /// top-of-book entries in one message per farm, as the reference sends
    /// them. An entry waiting after a reset waits on. An entry that another
    /// request has on the wire is shared, not sent (#452): the indexes of
    /// those come back too.
    fn depth_messages(&mut self, idx: usize, only_farm: Option<FarmId>) -> (Vec<(FarmId, Vec<(u32, String)>)>, Vec<usize>) {
        let mut joined = Vec::new();
        let mut sent: Vec<usize> = Vec::new();
        for ei in 0..self.depth_reqs[idx].entries.len() {
            let e = &self.depth_reqs[idx].entries[ei];
            if e.live || e.resubscribe_at.is_some() || only_farm.is_some_and(|f| f != e.farm) {
                continue;
            }
            let same = self.depth_reqs.iter().enumerate()
                .filter(|(ri, _)| *ri != idx)
                .flat_map(|(_, r)| r.entries.iter())
                .find(|o| o.live && o.farm == e.farm && o.req_type == e.req_type && o.exchange == e.exchange
                    && o.con_id == e.con_id && o.sec_type == e.sec_type)
                .map(|o| (o.farm_req, o.server_tag, o.scale, o.book.clone(), o.top, o.access));
            let e = &mut self.depth_reqs[idx].entries[ei];
            e.live = true;
            if let Some((farm_req, server_tag, scale, book, top, access)) = same {
                e.farm_req = farm_req;
                e.server_tag = server_tag;
                e.scale = scale;
                e.book = book;
                e.top = top;
                e.access = access;
                joined.push(ei);
                continue;
            }
            e.farm_req = self.next_md_req_id;
            self.next_md_req_id += 1 + e.skip_ids;
            e.server_tag = None;
            sent.push(ei);
        }
        let entries = &self.depth_reqs[idx].entries;
        let mut groups: Vec<(FarmId, Vec<usize>)> = sent.iter()
            .filter(|ei| entries[**ei].req_type == "0")
            .map(|ei| (entries[*ei].farm, vec![*ei]))
            .collect();
        let books = groups.len();
        for &ei in sent.iter().filter(|ei| entries[**ei].req_type != "0") {
            match groups[books..].iter_mut().find(|(f, _)| *f == entries[ei].farm) {
                Some((_, list)) => list.push(ei),
                None => groups.push((entries[ei].farm, vec![ei])),
            }
        }
        let mut out = Vec::new();
        for (farm, list) in groups {
            let mut msg: Vec<(u32, String)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
                (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
                (263, "1".into()),
                (146, list.len().to_string()),
            ];
            for ei in list {
                let e = &entries[ei];
                msg.push((262, e.farm_req.to_string()));
                msg.push((6008, e.con_id.clone()));
                msg.push((207, e.exchange.clone()));
                msg.push((167, e.sec_type.clone()));
                msg.push((264, e.req_type.to_string()));
                if e.req_type != "0" {
                    msg.push((6088, "Socket".into()));
                }
                msg.push((9830, "1".into()));
            }
            out.push((farm, msg));
        }
        if !out.is_empty() || !joined.is_empty() {
            log::info!("Depth req {}: {} entries sent, {} shared", self.depth_reqs[idx].req_id, sent.len(), joined.len());
        }
        (out, joined)
    }

    /// The cancel of some entries, as they were sent: the top-of-book
    /// entries in one message per farm, then each book in a message of its
    /// own, as the reference cancels them (#452).
    fn depth_cancels(entries: &[&DepthEntry]) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut groups: Vec<(FarmId, Vec<&DepthEntry>)> = Vec::new();
        for e in entries.iter().filter(|e| e.req_type != "0") {
            match groups.iter_mut().find(|(f, _)| *f == e.farm) {
                Some((_, list)) => list.push(e),
                None => groups.push((e.farm, vec![e])),
            }
        }
        groups.extend(entries.iter().filter(|e| e.req_type == "0").map(|e| (e.farm, vec![*e])));
        let mut out = Vec::new();
        for (farm, mine) in groups {
            let mut msg: Vec<(u32, String)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
                (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
                (263, "2".into()),
                (146, mine.len().to_string()),
            ];
            for e in mine {
                msg.push((262, e.farm_req.to_string()));
                msg.push((6008, e.con_id.clone()));
                msg.push((207, e.exchange.clone()));
                msg.push((167, e.sec_type.clone()));
                msg.push((264, e.req_type.to_string()));
                msg.push((9830, "1".into()));
            }
            out.push((farm, msg));
        }
        out
    }

    /// A live entry of some request has this farm id (#452).
    fn depth_wire_used(&self, farm: FarmId, farm_req: u32) -> bool {
        self.depth_reqs.iter().flat_map(|r| r.entries.iter()).any(|e| e.live && e.farm == farm && e.farm_req == farm_req)
    }

    /// Contracts with a depth request, for the reference's limit (#452).
    pub(crate) fn depth_contracts(&self) -> Vec<i64> {
        let mut c: Vec<i64> = self.depth_reqs.iter().map(|r| r.con_id).collect();
        c.sort_unstable();
        c.dedup();
        c
    }

    /// A depth request of that id and kind is live (#452).
    pub(crate) fn has_depth_req(&self, req_id: ReqId, smart: bool) -> bool {
        self.depth_reqs.iter().any(|r| r.req_id == req_id && r.smart == smart)
    }

    /// Whether a live depth entry uses `farm`.
    pub(crate) fn depth_uses_farm(&self, farm: FarmId) -> bool {
        self.depth_reqs.iter().any(|r| r.entries.iter().any(|e| e.farm == farm))
    }

    /// End a depth request: its cancels, for the entries that were sent
    /// and that no other request has, as the reference cancels (#452).
    /// None for an unknown request id.
    pub(crate) fn stop_depth(&mut self, req_id: ReqId) -> Option<Vec<(FarmId, Vec<(u32, String)>)>> {
        let pos = self.depth_reqs.iter().position(|r| r.req_id == req_id)?;
        let req = self.depth_reqs.remove(pos);
        let live: Vec<&DepthEntry> = req.entries.iter().filter(|e| e.live && !self.depth_wire_used(e.farm, e.farm_req)).collect();
        Some(Self::depth_cancels(&live))
    }

    /// A farm's connection was lost: its depth entries are no longer on the
    /// wire and go out again when it is back (`resend_depth`). Their books
    /// and tops are emptied, as the reference does on a farm change: a
    /// single book gets 317 and its whole book again with the next data,
    /// SmartDepth drops their rows (#451).
    pub(crate) fn depth_farm_lost(&mut self, farm: FarmId) {
        let mut out = std::mem::take(&mut self.depth_pending);
        for r in &mut self.depth_reqs {
            let mut hit = false;
            for e in r.entries.iter_mut().filter(|e| e.farm == farm && e.live) {
                e.live = false;
                e.server_tag = None;
                hit = true;
                if let Some(book) = e.book.as_mut() {
                    book.clear();
                    if let DepthView::Smart(m) = &mut r.view {
                        m.set_book(&e.name, book, &mut out);
                    }
                }
                if e.req_type == "442" {
                    e.top = TopQuote::default();
                    if let (DepthView::Smart(m), Some(scale)) = (&mut r.view, e.scale.as_ref()) {
                        m.set_top(&e.name, &e.top, scale, &mut out);
                    }
                }
            }
            if let (true, DepthView::Single(v)) = (hit, &mut r.view) {
                v.reset();
                self.depth_pending_resets.push(r.req_id);
            }
        }
        self.depth_pending = out;
    }

    /// Send again the depth entries of a farm that came back.
    pub(crate) fn resend_depth(&mut self, farm: FarmId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        (0..self.depth_reqs.len()).flat_map(|idx| self.depth_messages(idx, Some(farm)).0).collect()
    }

    /// Depth work due now (#451): the callbacks and reset errors made
    /// when a farm was lost, the SmartDepth warnings whose wait is over
    /// (#452), and the entries a reset took off the wire, asked again
    /// after the reference's delay. The messages to send.
    pub(crate) fn sweep_depth(&mut self, shared: &SharedState) -> Vec<(FarmId, Vec<(u32, String)>)> {
        if self.depth_reqs.is_empty() && self.depth_pending.is_empty() && self.depth_pending_resets.is_empty() {
            return Vec::new();
        }
        for req_id in self.depth_pending_resets.drain(..) {
            shared.orders.push_order_error(req_id, 317, RESET_TEXT.to_string());
        }
        for u in self.depth_pending.drain(..) {
            shared.market.push_depth_update(u);
        }
        let now = Instant::now();
        for r in &mut self.depth_reqs {
            if let Some(text) = r.status.as_mut().and_then(|s| s.expire(now)) {
                shared.orders.push_order_error(r.req_id, 2152, text);
            }
        }
        let mut out = Vec::new();
        if self.depth_reqs.iter().all(|r| r.entries.iter().all(|e| e.resubscribe_at.is_none())) {
            return out;
        }
        for idx in 0..self.depth_reqs.len() {
            let mut due = false;
            for e in &mut self.depth_reqs[idx].entries {
                if e.resubscribe_at.is_some_and(|t| t <= now) {
                    e.resubscribe_at = None;
                    due = true;
                }
            }
            if due {
                out.extend(self.depth_messages(idx, None).0);
            }
        }
        out
    }

    /// The acknowledgement of a depth entry (#451): its tag, price tick
    /// and size increment, in every request that has it; a book entry gets
    /// its book. A SmartDepth component is accepted (#452). False when the
    /// request id is not a depth entry's.
    fn depth_ack(&mut self, farm_req: u32, server_tag: u32, min_tick: f64, size_increment: Option<f64>, shared: &SharedState) -> bool {
        let user_book = self.user_book;
        let mut found = false;
        for r in &mut self.depth_reqs {
            let DepthReq { req_id, entries, lot, status, .. } = r;
            for e in entries.iter_mut().filter(|e| e.live && e.farm_req == farm_req) {
                found = true;
                let unit = size_increment.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0) * *lot as f64;
                let scale = Scale::new(min_tick, unit);
                e.server_tag = Some(server_tag);
                e.scale = Some(scale);
                e.access = DepthAccess::Allowed;
                if e.req_type == "0" {
                    e.book = Some(DeepBook::new(user_book, scale));
                }
                log::info!("Depth ack: server_tag {} -> req_id {} {} {} (min_tick={})",
                    server_tag, req_id, e.exchange, e.req_type, min_tick);
                if matches!(e.req_type, "0" | "442")
                    && let Some(text) = status.as_mut().and_then(|s| s.answer(&e.name, e.req_type == "0", true))
                {
                    shared.orders.push_order_error(*req_id, 2152, text);
                }
            }
        }
        found
    }

    /// The live entries of the farm being read with this tag, as
    /// (request, entry) indexes.
    #[inline]
    fn depth_entries_of(&self, server_tag: u32, hits: &mut Vec<(usize, usize)>) {
        let farm = self.rx_farm;
        hits.clear();
        for (ri, r) in self.depth_reqs.iter().enumerate() {
            for (ei, e) in r.entries.iter().enumerate() {
                if e.live && e.farm == farm && e.server_tag == Some(server_tag) {
                    hits.push((ri, ei));
                }
            }
        }
    }

    /// A depth entry the farm refused (#452), in every request that has
    /// it: a single-exchange request ends with 354, or 10089 when an API
    /// subscription is needed, and its entry is cancelled unless another
    /// request has it, as the reference; a SmartDepth component stays on
    /// the wire until the request's cancel and is answered as refused.
    fn depth_rejected(&mut self, farm_req: u32, needs_api_subscription: bool, shared: &SharedState, farm_conn: &mut Option<Connection>) -> bool {
        let access = DepthAccess::Refused { api_subscription: needs_api_subscription };
        let mut found = false;
        let mut ended: Vec<usize> = Vec::new();
        for (ri, r) in self.depth_reqs.iter_mut().enumerate() {
            let DepthReq { req_id, entries, status, smart, .. } = r;
            for e in entries.iter_mut().filter(|e| e.live && e.farm_req == farm_req) {
                found = true;
                e.access = access;
                if !*smart {
                    ended.push(ri);
                    continue;
                }
                log::warn!("SmartDepth req {}: the farm refused {} {}", req_id, e.exchange, e.req_type);
                if matches!(e.req_type, "0" | "442")
                    && let Some(text) = status.as_mut().and_then(|s| s.answer(&e.name, e.req_type == "0", false))
                {
                    shared.orders.push_order_error(*req_id, 2152, text);
                }
            }
        }
        ended.dedup();
        for ri in ended.into_iter().rev() {
            let req = self.depth_reqs.remove(ri);
            let (code, text) = depth_refusal(needs_api_subscription, &req.refusal_suffix);
            shared.orders.push_order_error(req.req_id, code, text);
            let live: Vec<&DepthEntry> = req.entries.iter().filter(|e| e.live && !self.depth_wire_used(e.farm, e.farm_req)).collect();
            for (_, msg) in Self::depth_cancels(&live) {
                let fields: Vec<(u32, &str)> = msg.iter().map(|(t, v)| (*t, v.as_str())).collect();
                farm_conn.send_comp(&fields);
            }
        }
        found
    }

    /// A depth message (35=Y, or 35=Z with extended fields) (#451): each
    /// group goes to the book of its tag in every request that has it, as
    /// one change; the request's callbacks follow. A group the book refuses
    /// resets it: the book is emptied, a single book gets 317, its entry is
    /// cancelled and asked again after the reference's delay, and its next
    /// data sends the whole book.
    fn handle_depth(&mut self, msg: &[u8], farm_conn: &mut Option<Connection>, shared: &SharedState) {
        let Some((body, extended)) = depth_decoder::depth_body(msg) else { return };
        let groups = depth_decoder::decode_depth(body, extended);
        let mut out = std::mem::take(&mut self.depth_out);
        let mut changes = std::mem::take(&mut self.depth_changes);
        let mut hits = std::mem::take(&mut self.depth_hits);
        let mut cancels: Vec<DepthEntry> = Vec::new();
        for group in &groups {
            self.depth_entries_of(group.server_tag, &mut hits);
            if hits.is_empty() {
                log::debug!("Depth group for server tag {}: no book", group.server_tag);
                continue;
            }
            for &(ri, ei) in &hits {
                let req = &mut self.depth_reqs[ri];
                let entry = &mut req.entries[ei];
                let Some(book) = entry.book.as_mut() else { continue };
                match book.apply(&group.entries, &mut changes) {
                    Ok(()) => match &mut req.view {
                        DepthView::Single(v) => v.on_change(book, &changes, &mut out),
                        DepthView::Smart(m) => m.set_book(&entry.name, book, &mut out),
                    },
                    Err(_) => {
                        log::warn!("Depth req {} {}: the farm sent an entry the book cannot take: reset", req.req_id, entry.exchange);
                        book.clear();
                        match &mut req.view {
                            DepthView::Single(v) => {
                                v.reset();
                                shared.orders.push_order_error(req.req_id, 317, RESET_TEXT.to_string());
                            }
                            DepthView::Smart(m) => m.set_book(&entry.name, book, &mut out),
                        }
                        if !cancels.iter().any(|c| c.farm == entry.farm && c.farm_req == entry.farm_req) {
                            cancels.push(entry.clone());
                        }
                        entry.live = false;
                        entry.server_tag = None;
                        entry.resubscribe_at = Some(Instant::now() + DEPTH_RESUBSCRIBE_DELAY);
                    }
                }
            }
        }
        for u in out.drain(..) {
            shared.market.push_depth_update(u);
        }
        self.depth_out = out;
        self.depth_changes = changes;
        self.depth_hits = hits;
        if !cancels.is_empty() {
            let refs: Vec<&DepthEntry> = cancels.iter().collect();
            for (_, msg) in Self::depth_cancels(&refs) {
                let fields: Vec<(u32, &str)> = msg.iter().map(|(t, v)| (*t, v.as_str())).collect();
                farm_conn.send_comp(&fields);
            }
        }
    }

    /// The ticks of a 35=P message for the top-of-book entries of
    /// SmartDepth requests (#451): each block of such a tag sets the
    /// exchange's bid and ask, then its SmartDepth rows, in every request
    /// that has the entry.
    fn depth_tops(&mut self, ticks: &[tick_decoder::RawTick], shared: &SharedState) {
        let mut out = std::mem::take(&mut self.depth_out);
        let mut hits = std::mem::take(&mut self.depth_hits);
        let mut i = 0;
        while i < ticks.len() {
            // One block: its first tick, then the ticks that follow it.
            let end = ticks[i + 1..].iter().position(|t| t.first).map_or(ticks.len(), |p| i + 1 + p);
            self.depth_entries_of(ticks[i].server_tag, &mut hits);
            for &(ri, ei) in &hits {
                let req = &mut self.depth_reqs[ri];
                let entry = &mut req.entries[ei];
                if let (true, Some(scale), DepthView::Smart(m)) = (entry.req_type == "442", entry.scale, &mut req.view) {
                    for t in &ticks[i..end] {
                        match t.tick_type {
                            tick_decoder::O_BID_PRICE => entry.top.bid = Some(t.magnitude),
                            tick_decoder::O_ASK_PRICE => entry.top.ask = Some(t.magnitude),
                            tick_decoder::O_BID_SIZE => entry.top.bid_size = Some(t.magnitude),
                            tick_decoder::O_ASK_SIZE => entry.top.ask_size = Some(t.magnitude),
                            _ => {}
                        }
                    }
                    m.set_top(&entry.name, &entry.top, &scale, &mut out);
                }
            }
            i = end;
        }
        for u in out.drain(..) {
            shared.market.push_depth_update(u);
        }
        self.depth_out = out;
        self.depth_hits = hits;
    }

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, _event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        // Entries and tags of the farms opened on demand stay (#445).
        self.farm_lost(PRIMARY_MD, context);
        // Its depth entries go out again when it is back (#452).
        self.depth_farm_lost(PRIMARY_MD);
        self.news_farm_lost(PRIMARY_MD);
        // Don't emit Event::Disconnected — auto-reconnect handles farm drops transparently.
        // Python is only notified if reconnect exhausts retries.
    }

    /// Test-only: set disconnected without clearing state or emitting events.
    #[cfg(any(test, feature = "test-support"))]
    pub fn handle_disconnect_for_test(&mut self) {
        self.disconnected = true;
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        hb: &mut HeartbeatState,
    ) {
        *farm_conn = Some(conn);
        self.disconnected = false;
        hb.farm_connected(Instant::now());

        // The top-of-book subscriptions without an entry on the wire are
        // sent again by the loop, each to the farm of its route
        // (`unsent_subscriptions`, #445, ibx#288).
        let _ = context;

        log::info!("Farm reconnected");
    }

    /// A news block (ibx#458): its headlines, decoded as the reference's
    /// news reader.
    fn handle_tick_news(&mut self, server_tag: u32, payload: &[u8], shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let farm = self.rx_farm;
        let Some(pos) = self.news.iter().position(|e| e.live && e.farm == farm && e.tag == Some(server_tag)) else {
            return;
        };
        let instrument = self.news[pos].instrument;
        for item in decode_news(payload) {
            // A removal (7 or more) gives no headline; an article the
            // client already has, from this entry or another of the
            // contract, is not given again.
            let known = self.news.iter().any(|e| e.instrument == instrument && e.seen.contains(&item.article_id));
            if item.action >= 7 || known || !self.news[pos].seen.insert(item.article_id.clone()) {
                log::debug!("News {} {} not given (action {})", item.provider_code, item.article_id, item.action);
                continue;
            }
            let (headline, extra_data) = split_headline(&item.raw_headline);
            let news = crate::types::TickNews {
                instrument,
                provider_code: item.provider_code,
                article_id: item.article_id,
                headline,
                timestamp: item.time as i64 * 1000,
                extra_data,
            };
            shared.market.push_tick_news(news.clone());
            emit(event_tx, Event::News(news));
        }
    }
}

/// The head of a market data message (35=V) of `n` entries.
fn md_message_head(action: &str, n: usize) -> Vec<(u32, String)> {
    vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
        (263, action.to_string()),
        (146, n.to_string()),
    ]
}

/// The market data status entry as the reference writes it (ibx#447,
/// captured 05/10/2026 and 07/10/2026): `262|6008|207|167|264=398`.
fn status_message(e: &StatusEntry, subscribe: bool) -> Vec<(u32, String)> {
    let mut msg = md_message_head(if subscribe { "1" } else { "2" }, 1);
    msg.extend([
        (262, e.farm_req.to_string()),
        (6008, e.con_id.clone()),
        (207, e.exchange.clone()),
        (167, e.sec_type.clone()),
        (264, MARKET_DATA_STATUS.to_string()),
    ]);
    msg
}

fn borrow_tags(tags: &[(u32, String)]) -> Vec<(u32, &str)> {
    tags.iter().map(|(t, v)| (*t, v.as_str())).collect()
}

/// A generic tick entry as the reference writes it (captured 05/10/2026):
/// `262|6008|207|167|264={code}|6088=Socket|9830=1`, the streaming-client
/// mark only in a subscribe.
fn generic_entry(e: &GenericEntry, subscribe: bool) -> Vec<(u32, String)> {
    let mut tags = vec![
        (262, e.farm_req.to_string()),
        (6008, e.con_id.clone()),
        (207, e.exchange.clone()),
        (167, e.sec_type.clone()),
        (264, e.code.to_string()),
    ];
    if subscribe {
        tags.push((6088, "Socket".into()));
    }
    tags.push((9830, "1".into()));
    tags
}

/// The text of an exchange map tick (ibx#441): a 4-byte length, then that
/// many one-byte characters.
fn exchange_map_text(data: &[u8]) -> String {
    let Some(len) = data.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else { return String::new() };
    data[4..].iter().take(len).map(|&b| b as char).collect()
}

/// An exchange map as the reference reads it (ibx#441): `;`-separated
/// `{bit}/{letter}/{exchange}` items; an item without three parts or with
/// a bit that is not a number is skipped, a later bit replaces an earlier
/// one. The components come sorted by bit.
pub(crate) fn parse_exchange_map(text: &str) -> Vec<crate::types::SmartComponent> {
    let mut map: std::collections::BTreeMap<i32, crate::types::SmartComponent> = std::collections::BTreeMap::new();
    if text.is_empty() {
        log::warn!("Incorrect or empty exchange mapping: {:?}", text);
    }
    for item in text.split(';').filter(|i| !i.is_empty()) {
        let parts: Vec<&str> = item.split('/').collect();
        let [bit, letter, exchange] = parts.as_slice() else {
            log::warn!("Incorrect exchange info in the mapping: {}", item);
            continue;
        };
        match bit.parse::<i32>() {
            Ok(bit) => {
                map.insert(bit, crate::types::SmartComponent {
                    bit_number: bit, exchange: exchange.to_string(), exchange_letter: letter.to_string(),
                });
            }
            Err(_) => log::error!("Incorrect bit in exchange info: {}", item),
        }
    }
    map.into_values().collect()
}

/// One headline of a news payload, as the reference reads it (ibx#458).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewsItem {
    pub(crate) provider_code: String,
    pub(crate) article_id: String,
    /// The first number: 7 or more removes the article.
    pub(crate) action: i32,
    /// Epoch seconds.
    pub(crate) time: i32,
    pub(crate) raw_headline: String,
}

/// The entries of a news payload (ibx#458): an int32 count N, then one
/// entry when N is 0, else N entries with 4 bytes (the conId) before every
/// entry but the first. An entry is the provider code and the article id,
/// then, when the article id is not empty, two int32 (the first is the
/// action), the time in epoch seconds and the raw headline. A string is an
/// int32 length, its bytes (each byte one character) and zero padding to a
/// multiple of 4. An entry cut short ends the list, as the reference stops
/// on the read error; an entry without article id has no headline.
pub(crate) fn decode_news(payload: &[u8]) -> Vec<NewsItem> {
    let mut out = Vec::new();
    let mut r = NewsReader { buf: payload, pos: 0 };
    let n = if payload.len() < 4 { 0 } else { r.int().unwrap_or(0) };
    for i in 0..n.max(1) {
        if i > 0 && r.skip(4).is_none() { break; }
        let Some(item) = r.item() else { break };
        if !item.article_id.is_empty() {
            out.push(item);
        }
    }
    out
}

struct NewsReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl NewsReader<'_> {
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.buf.len() { return None; }
        self.pos += n;
        Some(())
    }

    fn int(&mut self) -> Option<i32> {
        let b = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.int()?.max(0) as usize;
        let b = self.buf.get(self.pos..self.pos + len)?;
        self.pos = (self.pos + len + (4 - len % 4) % 4).min(self.buf.len());
        Some(b.iter().map(|&c| c as char).collect())
    }

    fn item(&mut self) -> Option<NewsItem> {
        let provider_code = self.string()?;
        let article_id = self.string()?;
        let (mut action, mut time, mut raw_headline) = (-1, 0, String::new());
        if !article_id.is_empty() {
            action = self.int()?;
            self.int()?;
            time = self.int()?;
            raw_headline = self.string()?;
        }
        Some(NewsItem { provider_code, article_id, action, time, raw_headline })
    }
}

/// The tickNews headline and extra data of a raw headline (ibx#458): the
/// extra data is the text between the first `{` and the next `}`; the
/// headline is the text after a leading `{...}` part.
pub(crate) fn split_headline(raw: &str) -> (String, String) {
    let extra = raw.find('{')
        .and_then(|open| raw[open + 1..].find('}').map(|close| raw[open + 1..open + 1 + close].to_string()))
        .unwrap_or_default();
    let headline = match raw.strip_prefix('{').and_then(|r| r.find('}').map(|i| &r[i + 1..])) {
        Some(text) => text.to_string(),
        None => raw.to_string(),
    };
    (headline, extra)
}

/// The message of a news entry: the subscribe with the streaming-client
/// mark, the cancel without it (ibx#458, captured 02/10/2026).
fn news_message(e: &NewsEntry, subscribe: bool) -> Vec<(u32, String)> {
    let mut msg: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
        (263, if subscribe { "1" } else { "2" }.into()),
        (146, "1".into()),
        (262, e.farm_req.to_string()),
        (6008, e.con_id.clone()),
        (207, "NEWS".into()),
        (167, e.sec_type.clone()),
        (264, "292".into()),
    ];
    if !e.providers.is_empty() {
        msg.push((6472, e.providers.clone()));
    }
    if subscribe {
        msg.push((6088, "Socket".into()));
    }
    msg.push((9830, "1".into()));
    msg
}

/// The reference's security type code (`SecType` value), appended in hex
/// to a short BBO exchange code (ibx#449).
fn sec_type_code(sec_type: &str) -> Option<u8> {
    crate::types::sec_type_id(sec_type)
}

/// tickReqParams from the fields of a bid/ask ack, as the reference builds
/// it (ibx#449): the BBO exchange code, unless it is the "no exchange"
/// value (never kept), with the security type code in four hex digits
/// appended when it has four characters or less; the snapshot permissions
/// from the ack when
/// a BBO exchange is kept, else 0. None when there is nothing to report.
fn tick_req_params(instrument: InstrumentId, min_tick: f64, parts: &[&str], sec_type: &str)
    -> Option<crate::bridge::TickReqParams>
{
    let code = parts.get(5).map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "ffffffff");
    let bbo_exchange = match (code, code.and_then(|_| sec_type_code(sec_type))) {
        (Some(c), Some(t)) if c.len() <= 4 => format!("{}{:04X}", c, t),
        (Some(c), _) => c.to_string(),
        (None, _) => String::new(),
    };
    let snapshot_permissions = match code {
        Some(_) => parts.get(4).and_then(|s| s.trim().parse::<i32>().ok()).filter(|p| (0..=4).contains(p)).unwrap_or(0),
        None => 0,
    };
    (min_tick > 0.0 || !bbo_exchange.is_empty() || snapshot_permissions != 0).then(|| crate::bridge::TickReqParams {
        instrument, min_tick, bbo_exchange, snapshot_permissions,
    })
}

/// The reference's reading of a reject's API access value (6763), as
/// `ApiAccess.apiRequiresSubscription`: a list (`,` or `#`) or a single
/// number means an API subscription is needed (10089); empty or `-` does
/// not (354) (ibx#444).
fn api_subscription_needed(access: &str) -> bool {
    access.contains(',') || access.contains('#') || (!access.is_empty() && access.parse::<i64>().is_ok())
}

/// The reference's overnight venues (`jfix.R.C`): a top of book there
/// keeps its LAST entry on the venue (#452).
pub(crate) fn overnight_venue(exchange: &str) -> bool {
    matches!(exchange, "OVERNIGHT" | "IBEOS")
}

/// The error of a refused depth book (#452), as the reference builds it
/// (`jextend.F.a(ApiAccess,dy,DEEP,NONE,false)`): 10089 when an API
/// subscription is needed, else 354, in the texts of the reference's
/// English language file, then the contract and `/DEEP` (`suffix`).
/// Captured: "...for more details.AAPL NASDAQ.NMS/DEEP" (25/09/2026) and
/// "...availability of delayed data.MNQ DEC'26 (MNQZ6) /DEEP" (23/09/2026).
pub(crate) fn depth_refusal(api_subscription: bool, suffix: &str) -> (i64, String) {
    if api_subscription {
        (10089, format!("Requested market data requires additional subscription for API. \
            See link in 'Market Data Connections' dialog for more details.{suffix}"))
    } else {
        (354, format!("Requested market data is not subscribed. Check API status by selecting the Account menu then \
            under Management choose Market Data Subscription Manager and/or availability of delayed data.{suffix}"))
    }
}

/// The contract as a refusal names it (#452, ibx#444,
/// `jclient.dy.cU()`, `dy.a(boolean,FormatHint,boolean)`): a stock is its
/// symbol, then its primary exchange with the market after a dot (`AAPL
/// NASDAQ.NMS`), then its local symbol in parentheses and a space when it
/// differs from the symbol (`7203 TSEJ (7203.T) `, captured 05/10/2026).
/// None for the other security types, whose descriptions are not read
/// yet.
pub(crate) fn depth_description(def: &crate::control::contracts::ContractDefinition) -> Option<String> {
    use crate::control::contracts::SecurityType;
    if def.sec_type != SecurityType::Stock {
        return None;
    }
    // The definition may give the primary exchange with its market
    // already (when that full name is a valid exchange).
    let mut place = def.primary_exchange.clone();
    let full = !def.primary_suffix.is_empty() && place.ends_with(&format!(".{}", def.primary_suffix));
    if !place.is_empty() && !def.primary_suffix.is_empty() && !full {
        place.push('.');
        place.push_str(&def.primary_suffix);
    }
    let mut text = format!("{} {}", def.symbol, place).trim().to_string();
    if !def.local_symbol.is_empty() && def.local_symbol != def.symbol {
        text.push_str(&format!(" ({}) ", def.local_symbol));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PRICE_SCALE, QTY_SCALE};

    /// A tick message with the given blocks: (stats block, server tag,
    /// entries of (type, value)), every value on four bytes.
    type Block<'a> = (bool, u32, &'a [(u64, i64)]);

    fn tick_message(blocks: &[Block]) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        let mut push = |v: u64, n: usize| for i in (0..n).rev() { bits.push(((v >> i) & 1) as u8) };
        for (stats, tag, entries) in blocks {
            push(*stats as u64, 1);
            push(*tag as u64, 31);
            for (k, (tick_type, value)) in entries.iter().enumerate() {
                push(*tick_type, 5);
                push((k + 1 < entries.len()) as u64, 1);
                push(3, 2); // 4 bytes
                push((*value < 0) as u64, 1);
                push(value.unsigned_abs(), 31);
            }
        }
        let mut body = vec![(bits.len() >> 8) as u8, bits.len() as u8];
        body.resize(2 + bits.len().div_ceil(8), 0);
        for (i, b) in bits.iter().enumerate() {
            body[2 + i / 8] |= b << (7 - i % 8);
        }
        let mut msg = b"8=O\x019=0\x0135=P\x01".to_vec();
        msg.extend_from_slice(&body);
        msg
    }

    // ibx#448: a daily-stats block on the trade stream fills close, last
    // size, high, volume and open, and a trade block the last trade time.
    #[test]
    fn stats_block_lands_in_close_last_size_high_volume_open() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(265598);
        context.market.set_min_tick(id, 0.01);
        context.market.register_server_tag(128_516, id, 0.01);
        let msg = tick_message(&[
            (true, 128_516, &[(3, 25_512), (6, 3), (8, 25_730), (10, 1466), (20, 20_260_922), (22, 25_401)]),
            (false, 128_516, &[(2, 25_501), (20, 1_790_159_184), (21, 2)]),
        ]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);
        let q = shared.market.quote(id);
        assert_eq!(q.close, 25_512 * PRICE_SCALE / 100);
        assert_eq!(q.last_size, 3 * QTY_SCALE);
        assert_eq!(q.high, 25_730 * PRICE_SCALE / 100);
        assert_eq!(q.volume, 1466 * QTY_SCALE);
        assert_eq!(q.open, 25_401 * PRICE_SCALE / 100);
        assert_eq!(q.last, 25_501 * PRICE_SCALE / 100);
        assert_eq!(q.low, 0);
        assert_eq!(q.timestamp_ns, 1_790_159_186 * 1_000_000_000);
    }

    /// Subscribe EUR.USD, ack its two entries in the given order (bid/ask
    /// on tag 24 with the high-precision tick, last on tag 25), then its
    /// trade setup (tag 26), as captured 28/09/2026; then one tick message.
    fn eur_usd_quotes(bid_ask_first: bool) -> (crate::types::Quote, f64, Vec<crate::bridge::TickReqParams>) {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let id = context.market.register(12087792);
        context.market.set_routing(id, "CASH", "IDEALPRO");
        farm.send_mktdata_subscribe(12087792, "EUR", "IDEALPRO", "CASH", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        let mut acks = [
            format!("8=O\x0135=Q\x0124,{bid_ask},1e-05,0,3,ffffffff,,1,1"),
            format!("8=O\x0135=Q\x0125,{},5e-05,0,1,ffffffff,,1,1", bid_ask + 1),
        ];
        if !bid_ask_first {
            acks.reverse();
        }
        for ack in &acks {
            farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        }
        farm.handle_ticker_setup(b"8=O\x0135=L\x0112087792,5e-05,26,,1", &mut context);
        let msg = tick_message(&[
            (false, 24, &[(0, 113_634), (1, 113_635)]),
            (false, 26, &[(2, 22_727)]),
            (true, 26, &[(3, 22_782), (8, 22_782), (9, 22_713)]),
        ]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);
        (shared.market.quote(id), context.market.min_tick(id), shared.market.drain_tick_req_params())
    }

    // Each server tag scales its prices by its own tick: bid/ask by the
    // bid/ask entry's 0.00001, trades and daily stats by the trade setup's
    // 0.00005, in either ack order. The contract's tick and the request
    // parameters are the bid/ask entry's.
    #[test]
    fn eur_usd_prices_use_the_tick_of_their_server_tag() {
        let px = |raw: i64, step: i64| raw * step;
        for bid_ask_first in [true, false] {
            let (q, contract_tick, params) = eur_usd_quotes(bid_ask_first);
            assert_eq!((q.bid, q.ask), (px(113_634, 1_000), px(113_635, 1_000)), "order {bid_ask_first}");
            assert_eq!(q.last, px(22_727, 5_000));
            assert_eq!((q.close, q.high, q.low), (px(22_782, 5_000), px(22_782, 5_000), px(22_713, 5_000)));
            assert_eq!(q.bid, 113_634 * PRICE_SCALE / 100_000, "1.13634, not 5.6817");
            assert_eq!(contract_tick, 0.00001);
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].min_tick, 0.00001);
        }
    }

    /// EUR.USD acked and set up as captured 02/10/2026, with an API request
    /// listening, then the given tick messages; what the farm told of the
    /// quote, and the steps queued for the client (ibx#446).
    fn eur_usd_steps(messages: &[Vec<u8>]) -> (crate::types::QuoteMarks, Vec<MdEvent>) {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let id = context.market.register(12087792);
        context.market.set_routing(id, "CASH", "IDEALPRO");
        farm.send_mktdata_subscribe(12087792, "EUR", "IDEALPRO", "CASH", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        for ack in [
            format!("8=O\x0135=Q\x018,{bid_ask},1e-05,0,3,ffffffff,,1,1"),
            format!("8=O\x0135=Q\x016,{},5e-05,0,1,ffffffff,,1,1", bid_ask + 1),
        ] {
            farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        }
        farm.handle_ticker_setup(b"8=O\x0135=L\x0112087792,5e-05,7,,1", &mut context);
        shared.market.md_events.listen(id, true);
        for msg in messages {
            farm.handle_tick_data(msg, &mut context, &shared, &None);
        }
        let queue = &shared.market.md_events;
        let steps = (queue.tail()..queue.head()).map(|seq| queue.read(seq)).collect();
        (context.market.marks(id), steps)
    }

    const EUR_USD_QUOTE: Block<'static> = (false, 8, &[(0, 112_547), (4, 4_000_000), (1, 112_549), (5, 12_000_000), (11, 0)]);
    const EUR_USD_TRADE: Block<'static> = (false, 7, &[(2, 22_510), (6, 0), (13, 0), (20, 1_790_921_652), (21, 126)]);
    const EUR_USD_DAILY: Block<'static> = (true, 7, &[(3, 22_486), (20, 20_261_001), (8, 22_517), (9, 22_464), (10, 0), (12, 0)]);

    /// The steps as (step, fields set).
    fn shape(steps: &[MdEvent]) -> Vec<(crate::md_events::MdStep, u16)> {
        steps.iter().map(|e| (e.step, e.fields)).collect()
    }

    // ibx#446: the captured first EUR.USD message (book, trade, daily
    // figures) gives the book, the trade's time, its price and size with
    // status 0, the daily figures; each step with its values. No
    // auto-execution flag.
    #[test]
    fn eur_usd_message_gives_its_steps_in_order() {
        use crate::md_events::{field::*, MdStep};
        let (marks, steps) = eur_usd_steps(&[tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY])]);
        // In block order, the first marked: the reader takes the book after
        // the other steps.
        assert_eq!(shape(&steps), [
            (MdStep::Quote, 1 << BID | 1 << BID_SIZE | 1 << ASK | 1 << ASK_SIZE),
            (MdStep::Time, 1 << TIME),
            (MdStep::Last, 1 << LAST | 1 << LAST_SIZE),
            (MdStep::Daily, 1 << CLOSE | 1 << HIGH | 1 << LOW | 1 << VOLUME),
        ]);
        assert_eq!(steps.iter().map(|e| e.first).collect::<Vec<_>>(), [true, false, false, false]);
        let px = |raw: i64, step: i64| raw * step;
        assert_eq!(steps[1].values[TIME], 1_790_921_778 * 1_000_000_000);
        assert_eq!((steps[2].values[LAST], steps[2].values[LAST_SIZE], steps[2].halted), (px(22_510, 5_000), 0, Some(0)));
        assert_eq!(steps[3].values[HIGH], px(22_517, 5_000));
        assert_eq!((steps[0].values[BID], steps[0].values[ASK], steps[0].auto), (px(112_547, 1_000), px(112_549, 1_000), 0));
        assert_eq!(marks.halted(), Some(0));
        // The daily figures first in another message: their step first.
        let (_, steps) = eur_usd_steps(&[tick_message(&[EUR_USD_DAILY, EUR_USD_TRADE])]);
        assert_eq!(steps.iter().map(|e| e.step).collect::<Vec<_>>(), [MdStep::Daily, MdStep::Time, MdStep::Last]);
        // A trade with no status gives none; the quote keeps the last one.
        let (marks, steps) = eur_usd_steps(&[
            tick_message(&[EUR_USD_TRADE]),
            tick_message(&[(false, 7, &[(2, 22_511)])]),
        ]);
        assert_eq!(steps.last().map(|e| (e.step, e.halted)), Some((MdStep::Last, None)));
        assert_eq!(marks.halted(), Some(0));
        let (marks, steps) = eur_usd_steps(&[tick_message(&[(false, 7, &[(2, 22_511), (13, 3)])])]);
        assert_eq!(steps[0].halted, Some(3));
        assert_eq!(marks.halted(), Some(3));
    }

    // ibx#446: each message gives its own steps, never merged: two
    // messages, two book updates with their own values. Daily figures
    // first, then a trade with its exchange only (an exchange step, then
    // its price step with nothing set), then one with its time (a delta)
    // and its exchange; no book update.
    #[test]
    fn each_message_gives_its_own_steps() {
        use crate::md_events::{field::*, MdStep};
        use crate::types::SizeKind;
        let (marks, steps) = eur_usd_steps(&[
            tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY]),
            tick_message(&[(false, 8, &[(5, 6_000_000)])]),
            tick_message(&[(false, 8, &[(5, 3_000_000)])]),
        ]);
        let books: Vec<(u16, i64)> = steps.iter().filter(|e| e.step == MdStep::Quote).map(|e| (e.fields, e.values[ASK_SIZE])).collect();
        assert_eq!(books, [
            (1 << BID | 1 << BID_SIZE | 1 << ASK | 1 << ASK_SIZE, 12_000_000 * QTY_SCALE),
            (1 << ASK_SIZE, 6_000_000 * QTY_SCALE),
            (1 << ASK_SIZE, 3_000_000 * QTY_SCALE),
        ]);
        for kind in [SizeKind::Bid, SizeKind::Ask, SizeKind::Last, SizeKind::Volume] {
            assert!(marks.seen(kind), "{kind:?}");
        }
        let (_, steps) = eur_usd_steps(&[tick_message(&[
            (true, 7, &[(10, 5)]), (false, 7, &[(27, 8)]), (false, 7, &[(6, 2), (21, 92), (27, 1024)]),
        ])]);
        assert_eq!(shape(&steps), [
            (MdStep::Daily, 1 << VOLUME),
            (MdStep::Exchange, 1 << LAST_EXCH),
            (MdStep::Last, 0),
            (MdStep::Time, 1 << TIME),
            (MdStep::Exchange, 1 << LAST_EXCH),
            (MdStep::Last, 1 << LAST_SIZE),
        ]);
        assert_eq!((steps[1].values[LAST_EXCH], steps[4].values[LAST_EXCH]), (8, 1024));
    }

    // ibx#446: only the instruments an API request listens to have steps;
    // a daily-stats block on a quote tag gives none (the reference drops
    // it).
    #[test]
    fn steps_only_for_listened_instruments() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(41, id, 0.01);
        farm.handle_tick_data(&tick_message(&[(false, 41, &[(0, 66_000)])]), &mut context, &shared, &None);
        assert_eq!(shared.market.md_events.head(), 0);
        assert_eq!(shared.market.quote(id).bid, 660 * PRICE_SCALE, "the engine's quote is kept as before");
        shared.market.md_events.listen(id, true);
        farm.handle_tick_data(&tick_message(&[(true, 41, &[(3, 66_000)])]), &mut context, &shared, &None);
        assert_eq!(shared.market.md_events.head(), 0);
        farm.handle_tick_data(&tick_message(&[(false, 41, &[(0, 66_001)])]), &mut context, &shared, &None);
        assert_eq!(shared.market.md_events.head(), 1);
    }

    // ibx#446: a full queue drops the instrument's steps; once the client
    // read, the engine writes one catch-up with the whole quote before the
    // instrument's next steps.
    #[test]
    fn a_full_queue_gives_a_catch_up() {
        use crate::md_events::{field, MdStep, MD_QUEUE_CAPACITY};
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(41, id, 0.01);
        let queue = &shared.market.md_events;
        queue.listen(id, true);
        for n in 0..MD_QUEUE_CAPACITY as i64 + 3 {
            farm.handle_tick_data(&tick_message(&[(false, 41, &[(0, 60_000 + n)])]), &mut context, &shared, &None);
        }
        assert_eq!(queue.head(), MD_QUEUE_CAPACITY);
        assert!(queue.needs_catch_up(id));
        queue.release(MD_QUEUE_CAPACITY);
        farm.handle_tick_data(&tick_message(&[(false, 41, &[(1, 70_000)])]), &mut context, &shared, &None);
        let steps: Vec<MdEvent> = (MD_QUEUE_CAPACITY..queue.head()).map(|seq| queue.read(seq)).collect();
        assert_eq!(steps.iter().map(|e| e.step).collect::<Vec<_>>(), [MdStep::CatchUp, MdStep::Quote]);
        let last_bid = (60_000 + MD_QUEUE_CAPACITY as i64 + 2) * PRICE_SCALE / 100;
        assert_eq!((steps[0].fields, steps[0].values[field::BID]), (field::ALL, last_bid));
        assert_eq!(steps[1].values[field::ASK], 700 * PRICE_SCALE);
    }

    // ibx#446: the auto-execution bits of a quote (both set on the
    // captured SPY quote of 02/10/2026), from either attribute type, go
    // with its book update; a trade's attribute is its status, not these
    // bits.
    #[test]
    fn quote_auto_execution_bits() {
        use crate::types::QuoteMarks;
        let (marks, steps) = eur_usd_steps(&[tick_message(&[(false, 8, &[(0, 112_547), (7, 12)])])]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(true), Some(true)));
        assert_eq!(QuoteMarks::from_auto_word(steps[0].auto), QuoteMarks::from_auto_word(marks.auto_word()));
        let (marks, _) = eur_usd_steps(&[tick_message(&[(false, 8, &[(0, 112_547), (13, 4)])])]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(true), Some(false)));
        let (marks, _) = eur_usd_steps(&[
            tick_message(&[(false, 8, &[(7, 12)])]),
            tick_message(&[(false, 8, &[(7, 0)])]),
        ]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(false), Some(false)));
        let (marks, _) = eur_usd_steps(&[tick_message(&[(false, 7, &[(2, 22_511), (13, 12)])])]);
        assert_eq!((marks.bid_auto(), marks.ask_auto(), marks.halted()), (None, None, Some(0)));
    }

    // A stock acks both entries with one tag and one tick: every price is
    // on it.
    #[test]
    fn a_stock_with_one_tick() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let id = context.market.register(265598);
        context.market.set_routing(id, "STK", "SMART");
        farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        for r in [bid_ask, bid_ask + 1] {
            let ack = format!("8=O\x0135=Q\x011101,{r},0.01,0,3,9c,,1,1");
            farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        }
        farm.handle_ticker_setup(b"8=O\x0135=L\x01265598,0.01,1098,,1", &mut context);
        let msg = tick_message(&[(false, 1101, &[(0, 25_500), (1, 25_502)]), (false, 1098, &[(2, 25_501)])]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);
        let q = shared.market.quote(id);
        assert_eq!((q.bid, q.ask, q.last), (255 * PRICE_SCALE, 25_502 * PRICE_SCALE / 100, 25_501 * PRICE_SCALE / 100));
        assert_eq!(context.market.min_tick(id), 0.01);
    }

    struct RecordingSink(Vec<Vec<(u32, String)>>);
    impl FixSink for RecordingSink {
        fn send_plain(&mut self, fields: &[(u32, &str)]) -> bool { self.send_comp(fields) }
        fn send_comp(&mut self, fields: &[(u32, &str)]) -> bool {
            self.0.push(fields.iter().map(|(t, v)| (*t, v.to_string())).collect());
            true
        }
    }

    /// The exchange map of BBO exchange 9c (AAPL), captured 02/10/2026
    /// (b1_441_smart_components, usfarm 35=G on server tag 12708).
    const EXCH_MAP_9C: &str = "0/A/AMEX;15/N/NYSE;4/I/ISE;7/M/CHX;8/P/ARCA;9/Q/NASDAQ;6/K/DRCTEDGE;1/B/BEX;\
        14/Z/BATS;5/J/EDGEA;13/Y/BYX;10/V/IEX;3/D/FINRA;18/H/PEARL;2/C/NYSENAT;16/L/LTSE;17/U/MEMX;12/X/PSX;\
        11/G/T24X;19/F/TXSE";

    fn exchange_map_frame(server_tag: u32, text: &str) -> Vec<u8> {
        let mut data = (text.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(text.as_bytes());
        data.push(0);
        let mut body = ((4 + 1 + data.len()) as u16 * 8).to_be_bytes().to_vec();
        body.extend_from_slice(&server_tag.to_be_bytes());
        body.push(data.len() as u8);
        body.extend_from_slice(&data);
        let mut msg = b"8=O\x019=0227\x0135=G\x01".to_vec();
        msg.extend_from_slice(&body);
        msg.extend_from_slice(b"\x018349=70306FA0\x01");
        msg
    }

    // ibx#487: the exchange map's cancel has 6088=Socket only while the
    // contract's streaming request runs (rth_order_types of 28/09/2026: the
    // QQQ request was cancelled before its map came).
    #[test]
    fn exchange_map_cancel_after_the_request_has_no_source() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let mut sink = RecordingSink(Vec::new());
        let id = context.market.register(320227571);
        context.market.set_routing(id, "STK", "SMART");
        farm.send_mktdata_subscribe(320227571, "QQQ", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        let ack = format!("8=O\x0135=Q\x012486,{bid_ask},0.01,0,0,9c,,0,1");
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        let map_id = bid_ask + 2;
        let has_source = |m: &Vec<(u32, String)>| m.iter().any(|(t, _)| *t == 6088);
        assert!(has_source(&sink.0[0]), "asked while the request runs");
        farm.send_mktdata_unsubscribe(id, &mut None, &mut hb);
        let ack = format!("8=O\x0135=Q\x0112708,{map_id},0.01,0,0,9c,,0,1");
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        assert!(farm.handle_exchange_map_frame(&exchange_map_frame(12708, EXCH_MAP_9C), &mut sink, &shared, &mut hb));
        let cancel = sink.0.last().unwrap();
        assert!(cancel.iter().any(|(t, v)| *t == 263 && v == "2") && !has_source(cancel), "{cancel:?}");
    }

    // ibx#441, captured 02/10/2026: the first ack with BBO exchange 9c
    // makes the gateway subscribe its own generic tick 626 for the
    // contract; the map arrives as a 35=G on the tag of that subscription,
    // then the subscription is cancelled. A second ack asks nothing.
    #[test]
    fn exchange_map_asked_at_the_first_ack_then_kept() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let mut sink = RecordingSink(Vec::new());
        let id = context.market.register(265598);
        context.market.set_routing(id, "STK", "SMART");
        farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        for r in [bid_ask, bid_ask + 1] {
            let ack = format!("8=O\x0135=Q\x01178,{r},0.01,0,3,9c,,1,1");
            farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        }
        let wire = |m: &Vec<(u32, String)>| m.iter().filter(|(t, _)| *t != fix::TAG_SENDING_TIME)
            .map(|(t, v)| format!("{t}={v}|")).collect::<String>();
        let map_id = bid_ask + 2;
        assert_eq!(sink.0.iter().map(wire).collect::<Vec<_>>(), [format!(
            "35=V|263=1|146=1|262={map_id}|6008=265598|207=BEST|167=CS|264=626|6088=Socket|")]);
        assert_eq!(shared.reference.exchange_map("9c", Some(1)), crate::bridge::ExchangeMapState::Waiting);
        // Its ack, then the map.
        let ack = format!("8=O\x0135=Q\x0112708,{map_id},0.01,0,0,9c,,0,1");
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        assert!(farm.handle_exchange_map_frame(&exchange_map_frame(12708, EXCH_MAP_9C), &mut sink, &shared, &mut hb));
        assert_eq!(wire(&sink.0[1]), format!(
            "35=V|263=2|146=1|262={map_id}|6008=265598|207=BEST|167=CS|264=626|6088=Socket|"));
        let map = shared.reference.instrument_exchange_map(id).unwrap();
        assert_eq!(map.len(), 20);
        let entry = |bit: i32| map.iter().find(|c| c.bit_number == bit)
            .map(|c| (c.exchange.as_str(), c.exchange_letter.as_str()));
        assert_eq!((entry(0), entry(9), entry(19)), (Some(("AMEX", "A")), Some(("NASDAQ", "Q")), Some(("TXSE", "F"))));
        assert!(map.windows(2).all(|w| w[0].bit_number < w[1].bit_number), "sorted by bit");
        assert!(farm.exchange_map_subs.is_empty());
        // Another tag's 35=G is not a map.
        assert!(!farm.handle_exchange_map_frame(&exchange_map_frame(12708, EXCH_MAP_9C), &mut sink, &shared, &mut hb));
        // A new subscription of the contract asks nothing more.
        farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let ack = format!("8=O\x0135=Q\x01178,{},0.01,0,3,9c,,1,1", farm.next_md_req_id - 2);
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        assert_eq!(sink.0.len(), 2);
    }

    // ibx#450, captured 05/10/2026 (AAPL, b2_generic): the generic ticks
    // that go at once in a message after the top of book, in request code
    // order; the others at the top's acknowledgement, with the exchange map
    // entry last, in one message; 104 is 512; 411 is not valid for a stock;
    // the auction goes to the primary exchange. A block of a tick gives its
    // API ticks; the cancel is in descending code order, without 6088.
    #[test]
    fn generic_ticks_at_once_then_at_the_acknowledgement() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let mut sink = RecordingSink(Vec::new());
        let id = context.market.register(265598);
        context.market.set_routing(id, "STK", "SMART");
        farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let wire = |m: &Vec<(u32, String)>| m.iter().filter(|(t, _)| *t != fix::TAG_SENDING_TIME)
            .map(|(t, v)| format!("{t}={v}|")).collect::<String>();
        let entry = |id: u32, exch: &str, code: i32| format!("262={id}|6008=265598|207={exch}|167=CS|264={code}|6088=Socket|9830=1|");
        let msgs = farm.start_generic(id, 265598, "SMART", "STK", "NASDAQ", &[456, 236, 101, 225, 104, 411], PRIMARY_MD);
        assert_eq!(msgs.len(), 1);
        assert_eq!(wire(&msgs[0].1), format!("35=V|263=1|146=2|{}{}", entry(3, "BEST", 101), entry(4, "BEST", 456)));
        let ack = "8=O\x0135=Q\x01827,1,0.01,0,3,9c,,1,1";
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        assert_eq!(sink.0.iter().map(wire).collect::<Vec<_>>(), [format!(
            "35=V|263=1|146=4|{}{}{}262=8|6008=265598|207=BEST|167=CS|264=626|6088=Socket|",
            entry(5, "NASDAQ", 225), entry(6, "BEST", 236), entry(7, "BEST", 512))]);
        // The shortable block of frame 9080 on the tag of its ack.
        let ack = "8=O\x0135=Q\x011839,6,0.01,0,0,9c,,0,1";
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        let mut msg = b"8=O\x019=0035\x0135=G\x01".to_vec();
        msg.extend_from_slice(&[0x00, 0x68, 0x00, 0x00, 0x07, 0x2f, 0x08, 0x00, 0x00, 0x00, 0x03, 0x0b, 0x56, 0x6f, 0x50]);
        farm.handle_generic_frame(&msg, &mut sink, &mut context, &shared, &None, &mut hb);
        let got = shared.market.take_generic_ticks(u64::MAX);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].instrument, got[0].code), (id, 236));
        use crate::control::generic_values::GenTick;
        assert_eq!(got[0].ticks, [GenTick::Generic(46, 3.0), GenTick::Size(89, 190213968.0)]);
        // A second request with 236 shares the entry; its release cancels
        // nothing.
        assert!(farm.start_generic(id, 265598, "SMART", "STK", "NASDAQ", &[236], PRIMARY_MD).is_empty());
        assert!(farm.release_generic(id, &[236]).is_empty());
        let cancels = farm.stop_generic(id);
        assert_eq!(cancels.len(), 1);
        let codes: Vec<&str> = cancels[0].1.iter().filter(|(t, _)| *t == 264).map(|(_, v)| v.as_str()).collect();
        assert_eq!(codes, ["512", "456", "236", "225", "101"]);
        assert!(!cancels[0].1.iter().any(|(t, _)| *t == 6088));
        assert!(farm.generic.is_empty());
    }

    // ibx#441: the "no exchange" code of a currency pair is ignored: no key,
    // no map asked.
    #[test]
    fn no_exchange_map_for_the_no_exchange_code() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let mut sink = RecordingSink(Vec::new());
        let id = context.market.register(12087792);
        context.market.set_routing(id, "CASH", "IDEALPRO");
        farm.send_mktdata_subscribe(12087792, "EUR", "IDEALPRO", "CASH", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let ack = format!("8=O\x0135=Q\x013,{},1e-05,0,3,ffffffff,,1,1", farm.next_md_req_id - 2);
        farm.handle_subscription_ack(ack.as_bytes(), &mut sink, &mut context, &shared, &mut hb);
        assert!(sink.0.is_empty());
        assert_eq!(shared.reference.exchange_map("ffffffff", None), crate::bridge::ExchangeMapState::Unknown);
    }

    // ibx#441: the map items as the reference reads them.
    #[test]
    fn exchange_map_items_as_the_reference() {
        let map = parse_exchange_map("3/N/NYSE;x/Q/NASDAQ;0/A;1/B/BEX;3/T/NASDAQ;");
        assert_eq!(map.iter().map(|c| (c.bit_number, c.exchange_letter.as_str(), c.exchange.as_str())).collect::<Vec<_>>(),
            [(1, "B", "BEX"), (3, "T", "NASDAQ")]);
        assert!(parse_exchange_map("").is_empty());
    }

    // ibx#446: the regulatory snapshot asks one entry with action SNAPSHOT
    // and type 624, without the streaming mark; its ack goes to the fetcher
    // and never gives tickReqParams.
    #[test]
    fn regulatory_snapshot_request_and_ack() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(265598);
        let sub = MdSubscribe {
            con_id: 265598, symbol: "AAPL".into(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            instrument: id, mode_9887: 0, snapshot: false,
        };
        let mut sink = RecordingSink(Vec::new());
        let mut hb = HeartbeatState::new();
        farm.subscribe_snapshot(&sub, PRIMARY_MD, &mut sink, &mut hb);
        let msg = &sink.0[0];
        let tags: Vec<u32> = msg.iter().map(|(t, _)| *t).collect();
        assert_eq!(tags, vec![35, 52, 263, 146, 262, 6008, 207, 167, 264, 9830]);
        let value = |t: u32| msg.iter().find(|(x, _)| *x == t).map(|(_, v)| v.clone()).unwrap();
        assert_eq!((value(263), value(146), value(207), value(167), value(264), value(9830)),
            ("3".into(), "1".into(), "BEST".into(), "CS".into(), "624".into(), "1".into()));
        let req: u32 = value(262).parse().unwrap();
        let ack = format!("8=O35=Q1101,{req},0.01,0,2,9c,,1,1");
        farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        assert!(shared.market.drain_tick_req_params().is_empty());
        let acks = shared.market.drain_snapshot_acks();
        assert_eq!(acks.len(), 1);
        assert_eq!((acks[0].instrument, acks[0].snapshot_permissions, acks[0].bbo_exchange.as_str()), (id, 2, "9c"));
        farm.drop_snapshot(id);
        assert!(farm.snapshot_reqs.is_empty());
    }

    // ibx#287 (AAPL, captured with the lots scaling on): wire bid size 57
    // reaches the API as 2280 with a round lot of 40; volume stays as on
    // the wire; the size increment of the ack and of the trade stream
    // setup multiplies every size.
    #[test]
    fn sizes_use_the_size_increment_and_the_round_lot() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(265598);
        farm.md_req_to_instrument.push((5, id));
        farm.handle_subscription_ack(b"8=O\x0135=Q\x011101,5,0.01,0,3,9c,,1,1", &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        farm.handle_ticker_setup(b"8=O\x0135=L\x01265598,0.01,1098,,1", &mut context);
        context.market.set_round_lot(id, 40);
        let msg = tick_message(&[
            (false, 1101, &[(4, 57), (5, 3)]),
            (false, 1098, &[(6, 2), (10, 1466)]),
        ]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);
        let q = shared.market.quote(id);
        assert_eq!(q.bid_size, 2280 * QTY_SCALE);
        assert_eq!(q.ask_size, 120 * QTY_SCALE);
        assert_eq!(q.last_size, 80 * QTY_SCALE);
        assert_eq!(q.volume, 1466 * QTY_SCALE);

        // A fractional size increment on the trade stream setup.
        farm.handle_ticker_setup(b"8=O\x0135=L\x01265598,0.01,1098,,0.01", &mut context);
        let msg = tick_message(&[(false, 1098, &[(6, 250), (10, 5000)])]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);
        let q = shared.market.quote(id);
        assert_eq!(q.last_size, 100 * QTY_SCALE); // 250 x 0.01 x 40
        assert_eq!(q.volume, 50 * QTY_SCALE); // 5000 x 0.01
    }

    // ibx#487: the trade stream of a contract goes to the slot of its market
    // data subscription, not to the slot its orders registered before.
    #[test]
    fn the_trade_stream_goes_to_the_subscribed_slot() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let orders = context.market.register(265598);
        let md = context.market.try_register_unresolved().unwrap();
        context.market.resolve_con_id(md, 265598);
        assert_eq!(context.market.instrument_by_con_id(265598), Some(orders));
        farm.instrument_md_reqs.push((md, vec![11, 12]));
        farm.handle_ticker_setup(b"8=O\x0135=L\x01265598,0.01,186,,1", &mut context);
        assert_eq!(context.market.route_farm_tag(farm.rx_farm, 186).map(|r| r.instrument), Some(md));
    }

    // ibx#449: the bid/ask ack gives the request parameters, with the
    // reference's rules for the BBO exchange (the captured values of AAPL,
    // MNQ and EUR.USD); the last ack gives none.
    // ibx#446: a plain snapshot asks the bid/ask and last pair with the
    // snapshot action and without the streaming mark, as the reference
    // frames captured on 18/06/2026; its acks give the request parameters
    // as a stream's do, and its cancel repeats the entries.
    #[test]
    fn plain_snapshot_request_ack_and_cancel() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let id = context.market.register(265598);
        context.market.set_routing(id, "STK", "");
        let sub = MdSubscribe {
            con_id: 265598, symbol: "AAPL".into(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            instrument: id, mode_9887: 0, snapshot: true,
        };
        let mut sink = RecordingSink(Vec::new());
        let mut hb = HeartbeatState::new();
        farm.subscribe_top(&sub, PRIMARY_MD, &mut sink, &mut hb);
        let body = |msg: &Vec<(u32, String)>| -> String {
            msg.iter().filter(|(t, _)| *t != 52).map(|(t, v)| format!("{t}={v}|")).collect()
        };
        let (a, b) = (farm.next_md_req_id - 2, farm.next_md_req_id - 1);
        // Captured: 35=V|263=3|146=2|262=5|6008=265598|207=BEST|167=CS|264=442|9830=1|262=6|...|264=443|9830=1
        assert_eq!(body(&sink.0[0]), format!(
            "35=V|263=3|146=2|262={a}|6008=265598|207=BEST|167=CS|264=442|9830=1|262={b}|6008=265598|207=BEST|167=CS|264=443|9830=1|"));
        for r in [a, b] {
            let ack = format!("8=O\x0135=Q\x0154,{r},0.01,0,3,9c,,1,1");
            farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
        }
        assert!(shared.market.drain_snapshot_acks().is_empty());
        let params = shared.market.drain_tick_req_params();
        assert_eq!(params.len(), 1);
        assert_eq!((params[0].instrument, params[0].bbo_exchange.as_str(), params[0].snapshot_permissions), (id, "9c0001", 3));
        // Captured cancel of a snapshot: 263=2 with the same entries.
        let cancel = farm.unsubscribe_top(id);
        assert_eq!(body(&cancel[0].1), format!(
            "35=V|263=2|146=2|262={a}|6008=265598|207=BEST|167=CS|264=442|9830=1|262={b}|6008=265598|207=BEST|167=CS|264=443|9830=1|"));
        // A stream keeps the streaming mark and the subscribe action.
        let mut sink = RecordingSink(Vec::new());
        farm.subscribe_top(&MdSubscribe { snapshot: false, ..sub }, PRIMARY_MD, &mut sink, &mut hb);
        let stream = body(&sink.0[0]);
        assert!(stream.starts_with("35=V|263=1|146=2|") && stream.matches("6088=Socket|").count() == 2, "{stream}");
    }

    #[test]
    fn bid_ask_ack_gives_the_request_parameters() {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let mut first_ids = Vec::new();
        for (con_id, sec_type) in [(265598, "STK"), (770561201, "FUT"), (12087792, "CASH")] {
            let id = context.market.register(con_id);
            context.market.set_routing(id, sec_type, "");
            farm.send_mktdata_subscribe(con_id, "", "SMART", sec_type, "", 0.0, "", "", id, 0, &mut None, &mut hb);
            first_ids.push((id, farm.next_md_req_id - 2));
        }
        let acks = [("45", "0.01", "9c"), ("228", "0.25", "5"), ("24", "0.00001", "ffffffff")];
        for ((_, bid_ask), (tag, tick, bbo)) in first_ids.iter().zip(acks) {
            for r in [bid_ask, &(bid_ask + 1)] {
                let ack = format!("8=O\x0135=Q\x01{tag},{r},{tick},0,3,{bbo},,1,1");
                farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &shared, &mut HeartbeatState::new());
            }
        }
        let got = shared.market.drain_tick_req_params();
        let want = |i: usize, min_tick: f64, bbo: &str, perms: i32| crate::bridge::TickReqParams {
            instrument: first_ids[i].0, min_tick, bbo_exchange: bbo.into(), snapshot_permissions: perms,
        };
        assert_eq!(got, [want(0, 0.01, "9c0001", 3), want(1, 0.25, "50006", 3), want(2, 0.00001, "", 0)]);
    }

    // ibx#289: an ack that comes after the unsubscribe binds nothing, also
    // when the slot went to another contract meanwhile.
    #[test]
    fn late_ack_after_unsubscribe_binds_nothing() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let id = context.market.register(265598);
        farm.send_mktdata_subscribe(265598, "AAPL", "SMART", "STK", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let ids: Vec<u32> = farm.md_req_to_instrument.iter().map(|(r, _)| *r).collect();
        assert_eq!(ids.len(), 2);
        farm.send_mktdata_unsubscribe(id, &mut None, &mut hb);
        assert!(farm.md_req_to_instrument.is_empty());

        // The slot is reused by another contract, then the late ack lands.
        context.market.unregister(id);
        let other = context.market.register(4391);
        assert_eq!(other, id);
        let ack = format!("8=O\x0135=Q\x011101,{},0.25,0,3,5,,1,1", ids[0]);
        farm.handle_subscription_ack(ack.as_bytes(), &mut None::<Connection>, &mut context, &SharedState::new(), &mut HeartbeatState::new());
        assert_eq!(context.market.instrument_by_server_tag(1101), None);
        assert_eq!(context.market.min_tick(other), 0.0);
    }

    // ibx#287: a definition reply sets the round lot and releases the
    // subscriptions that wait for it; another conId's stay.
    #[test]
    fn round_lot_reply_releases_the_waiting_subscriptions() {
        let mut context = Context::new();
        context.scale_us_lots = true;
        let aapl = context.market.register(265598);
        let msft = context.market.register(272093);
        let sub = |con_id, instrument| MdSubscribe {
            con_id, symbol: String::new(), exchange: "SMART".into(), sec_type: "STK".into(),
            last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
            instrument, mode_9887: 0, snapshot: false,
        };
        let deadline = Instant::now() + LOT_LOOKUP_TIMEOUT;
        context.lot_lookups.push(("ibxlot0".into(), 265598, deadline));
        context.lot_lookups.push(("ibxlot1".into(), 272093, deadline));
        context.lot_parked.push(sub(265598, aapl));
        context.lot_parked.push(sub(272093, msft));
        let reply = crate::protocol::fix::fix_build(&[(35, "d"), (320, "ibxlot0"), (6008, "265598"), (167, "CS"),
            (6523, "USSTK"), (6030, "1"), (6023, "40"), (6027, "40")], 1);
        assert!(!round_lot_reply(&mut context, "other", &reply), "not a round-lot lookup");
        assert!(round_lot_reply(&mut context, "ibxlot0", &reply));
        assert_eq!(context.market.round_lot(aapl), 40);
        assert_eq!(context.round_lots.get(&265598), Some(&40));
        assert_eq!(context.lot_ready.len(), 1);
        assert_eq!(context.lot_ready[0].instrument, aapl);
        assert_eq!(context.lot_parked.len(), 1);

        // No reply in time: sent with sizes as on the wire.
        context.lot_lookups[0].2 = Instant::now();
        sweep_round_lot_lookups(&mut context);
        assert!(context.lot_lookups.is_empty() && context.lot_parked.is_empty());
        assert_eq!(context.lot_ready.len(), 2);
        assert_eq!(context.market.round_lot(msft), 1);
    }
}
