use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::engine::context::Context;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{InstrumentId, ReqId};
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
/// aggregate group (-1 when absent) and its SMART component exchanges.
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
}

/// A depth request of the client (#452).
#[derive(Debug, Clone)]
pub(crate) struct DepthReq {
    pub(crate) req_id: ReqId,
    pub(crate) con_id: i64,
    pub(crate) smart: bool,
    pub(crate) num_rows: i32,
    pub(crate) entries: Vec<DepthEntry>,
}

pub(crate) struct FarmState {
    pub(crate) next_md_req_id: u32,
    pub(crate) md_req_to_instrument: Vec<(u32, InstrumentId)>,
    /// Farm request ids of regulatory snapshots (ibx#446), with their instrument.
    pub(crate) snapshot_reqs: Vec<(u32, InstrumentId)>,
    pub(crate) instrument_md_reqs: Vec<(InstrumentId, Vec<u32>)>,
    /// The client's market data modes from reqMarketDataType (ibx#447).
    pub(crate) md_modes: crate::types::MarketDataModes,
    /// Active depth subscriptions: (req_id, is_smart_depth).
    pub(crate) depth_subs: Vec<(u32, bool)>,
    /// Maps (server_tag, farm) → (depth_req_id, is_smart_depth, min_tick) for active depth subscriptions.
    pub(crate) depth_tag_to_req: Vec<(u32, ReqId, bool, f64, FarmId)>,
    /// SmartDepth fan-out: maps internal sub_req → user's original req_id.
    depth_fanout_map: Vec<(u32, ReqId)>,
    /// Depth requests of the client and their entries (#452).
    pub(crate) depth_reqs: Vec<DepthReq>,
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
}

impl FarmState {
    pub(crate) fn new() -> Self {
        Self {
            next_md_req_id: 1,
            md_req_to_instrument: Vec::new(),
            snapshot_reqs: Vec::new(),
            instrument_md_reqs: Vec::new(),
            md_modes: crate::types::MarketDataModes::default(),
            depth_subs: Vec::new(),
            depth_tag_to_req: Vec::new(),
            depth_fanout_map: Vec::new(),
            depth_reqs: Vec::new(),
            md_resub_info: Vec::new(),
            disconnected: false,
            tick_buf: Vec::with_capacity(16),
            farm_msg_buf: Vec::with_capacity(32),
            routing: None,
            md_entries: Vec::new(),
            rx_farm: PRIMARY_MD,
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
                self.handle_subscription_ack(msg, context, shared);
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
            b"Y" => self.handle_depth_35y(msg, shared),
            b"G" => self.handle_tick_news(msg, context, shared, event_tx),
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

        // Depth 35=P entries may be interleaved with L1 tick entries in the same body.
        if !self.depth_tag_to_req.is_empty() {
            let mut has_depth = false;
            let mut off = 0;
            while off + 3 < body.len() {
                if body[off] == 0x00 {
                    let stag = ((body[off+1] as u32) << 16) | ((body[off+2] as u32) << 8) | (body[off+3] as u32);
                    if self.depth_tag_to_req.iter().any(|(s, .., f)| *s == stag && *f == self.rx_farm) {
                        has_depth = true;
                        break;
                    }
                }
                off += 1;
            }
            if has_depth {
                self.handle_depth_35p(body, shared);
                // Don't return — also process L1 ticks from same body below
            }
        }

        let mut ticks = std::mem::take(&mut self.tick_buf);
        if tick_decoder::decode_ticks_35p_into(body, &mut ticks) {
            // The session goes on, as the reference's (ibx#272).
            log::warn!("Farm tick message: malformed block dropped, {} ticks of earlier blocks kept", ticks.len());
        }
        let mut notified = [0u64; crate::types::MAX_INSTRUMENTS / 64];
        // Instruments whose trade stream ticked in this message: the first
        // such tick tells whether the daily figures came before the trade
        // (ibx#446).
        let mut traded = [0u64; crate::types::MAX_INSTRUMENTS / 64];

        // Phase 1: Apply all ticks to internal quotes before publishing.
        for tick in &ticks {
            let route = match context.market.route_farm_tag(self.rx_farm, tick.server_tag) {
                Some(r) => r,
                None => continue,
            };
            let instrument = route.instrument;
            let (word, bit) = ((instrument >> 6) as usize, 1u64 << (instrument & 63));
            // The updates of the message, in its order (ibx#446).
            if notified[word] & bit == 0 {
                context.market.marks_mut(instrument).begin_message();
            }
            if route.trade && traded[word] & bit == 0 {
                traded[word] |= bit;
                context.market.set_daily_first(instrument, tick.stats_block);
            }
            if tick.first {
                let marks = context.market.marks_mut(instrument);
                match (route.trade, tick.stats_block) {
                    (true, true) => marks.begin_daily(),
                    (true, false) => marks.begin_trade(),
                    (false, false) => marks.note_quote_update(),
                    (false, true) => {}
                }
            }

            context.market.apply_tick(instrument, route.price_tick, route.trade, tick);

            notified[word] |= bit;
        }

        // Phase 2: Publish complete quotes after all ticks in the batch are applied.
        for (word_idx, &word) in notified.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let instrument = (word_idx as u32) * 64 + remaining.trailing_zeros();
                remaining &= remaining - 1;
                shared.market.push_quote(instrument, context.quote(instrument));
                shared.market.push_marks(instrument, context.market.marks(instrument));
                emit(event_tx, Event::Tick(instrument));
            }
        }
        self.tick_buf = ticks;
    }

    fn handle_subscription_ack(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState) {
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

        // Depth ack: always map the server_tag if this req_id is a depth subscription,
        // even when depth_levels=0 (book empty now but updates may arrive later).
        let depth_levels: i32 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
        if let Some((_, is_smart)) = self.depth_subs.iter().find(|(id, _)| *id == req_id) {
            let is_smart = *is_smart;
            // For SmartDepth fan-out, map back to the user's original req_id
            let user_req = self.depth_fanout_map.iter()
                .find(|(sub, _)| *sub == req_id)
                .map(|(_, user)| *user)
                .unwrap_or(ReqId::from(req_id));
            self.depth_tag_to_req.push((server_tag, user_req, is_smart, min_tick, self.rx_farm));
            log::info!("Depth ack: server_tag {} -> req_id {} (levels={}, smart={}, min_tick={})",
                server_tag, user_req, depth_levels, is_smart, min_tick);
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
        context.market.register_farm_tag(self.rx_farm, server_tag, instrument, min_tick);
        let bid_ask = self.instrument_md_reqs.iter().find(|(id, _)| *id == instrument)
            .and_then(|(_, reqs)| reqs.iter().position(|r| *r == req_id))
            .is_some_and(|p| p % 2 == 0);
        if bid_ask && min_tick.is_finite() && min_tick > 0.0 {
            context.market.set_min_tick(instrument, min_tick);
        }
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
        // The size increment (ibx#287); absent from older acks.
        if let Some(size_min_tick) = parts.get(8).and_then(|v| v.parse::<f64>().ok()) {
            context.market.set_size_min_tick(instrument, size_min_tick);
        }
        log::info!("Subscribed instrument {} -> server_tag {}, minTick {}", instrument, server_tag, min_tick);

        // The bid/ask entry of the pair (the first of each pair of ids)
        // gives the request parameters (ibx#449).
        if bid_ask {
            let sec_type = context.market.order_routing(instrument).0;
            if let Some(params) = tick_req_params(instrument, min_tick, &parts, &sec_type) {
                shared.market.push_tick_req_params(params);
            }
        }
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

        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            // A trade stream tag, kept apart from the quote tags (#292),
            // with the tick its trades are scaled by.
            context.market.register_trade_tag(self.rx_farm, server_tag, instrument, min_tick);
            // The size increment, when present (ibx#287).
            if let Some(size_min_tick) = parts.get(4).and_then(|v| v.parse::<f64>().ok()) {
                context.market.set_size_min_tick(instrument, size_min_tick);
            }
            log::info!("Ticker setup: con_id {} -> server_tag {}, minTick {}", con_id, server_tag, min_tick);
        }
    }

    /// A live market data subscription uses the instrument: sent, or kept
    /// for the next reconnect while the farm is down (ibx#291).
    pub(crate) fn has_md_subscription(&self, instrument: InstrumentId) -> bool {
        self.instrument_md_reqs.iter().any(|(id, _)| *id == instrument)
            || self.md_resub_info.iter().any(|(id, ..)| *id == instrument)
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
            if !sub.snapshot { tags.push((6088, "Socket")); }
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

    /// Whether a live top-of-book request uses `farm` (#445).
    pub(crate) fn uses_farm(&self, farm: FarmId) -> bool {
        self.md_entries.iter().any(|e| e.farm == farm)
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
            let Ok(id) = id.parse::<u32>() else { continue };
            let Some(&(_, instrument)) = self.md_req_to_instrument.iter().find(|(r, _)| *r == id) else {
                // A depth entry (#452).
                let needs_sub = access.get(i).is_some_and(|a| api_subscription_needed(a));
                self.depth_rejected(id, needs_sub, shared);
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
                // The subscription stops: nothing is left to cancel.
                if let Some(idx) = self.instrument_md_reqs.iter().position(|(id, _)| *id == instrument) {
                    let (_, reqs) = self.instrument_md_reqs.remove(idx);
                    self.md_req_to_instrument.retain(|(r, _)| !reqs.contains(r));
                    self.md_entries.retain(|e| !reqs.contains(&e.req_id));
                }
                self.md_resub_info.retain(|(id, ..)| *id != instrument);
                shared.market.push_md_reject(crate::bridge::MdReject::NotSubscribed {
                    instrument, delayed_available, needs_api_subscription,
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
        let entries: Vec<MdEntry> = self.md_entries.iter().filter(|e| reqs.contains(&e.req_id)).cloned().collect();
        self.md_entries.retain(|e| !reqs.contains(&e.req_id));

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

    /// Start a depth request (#452) with its entries, each on the farm of
    /// its route: a book entry for each `deep` exchange, a top-of-book
    /// pair for each `top` exchange (SmartDepth components with no book).
    /// The caller made the local checks and picked the farms.
    pub(crate) fn start_depth(
        &mut self,
        req_id: ReqId,
        con_id: i64,
        sec_type: &str,
        smart: bool,
        num_rows: i32,
        entries: Vec<(FarmId, String, bool)>,
    ) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut req = DepthReq {
            req_id, con_id, smart, num_rows, entries: Vec::new(),
        };
        let fix_type = fix_sec_type(sec_type).to_string();
        for (farm, exchange, book) in entries {
            let kinds: &[&'static str] = if book { &["0"] } else { &["442", "443"] };
            for kind in kinds {
                req.entries.push(DepthEntry {
                    farm_req: 0, farm, con_id: con_id.to_string(), exchange: exchange.clone(),
                    sec_type: fix_type.clone(), req_type: kind, live: false,
                });
            }
        }
        self.depth_reqs.push(req);
        let idx = self.depth_reqs.len() - 1;
        self.depth_messages(idx, None)
    }

    /// Send the entries of a depth request that are not on the wire (all of
    /// them, or those of one farm after it came back), with new farm ids;
    /// one message per farm, for the caller to send.
    fn depth_messages(&mut self, idx: usize, only_farm: Option<FarmId>) -> Vec<(FarmId, Vec<(u32, String)>)> {
        let mut out = Vec::new();
        let (user_req, smart) = (self.depth_reqs[idx].req_id, self.depth_reqs[idx].smart);
        let mut farms: Vec<FarmId> = self.depth_reqs[idx].entries.iter()
            .filter(|e| !e.live && only_farm.is_none_or(|f| f == e.farm))
            .map(|e| e.farm).collect();
        farms.sort_unstable();
        farms.dedup();
        for farm in farms {
            let mut msg: Vec<(u32, String)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
                (fix::TAG_SENDING_TIME, chrono_free_timestamp().to_string()),
                (263, "1".into()),
            ];
            let mut body: Vec<(u32, String)> = Vec::new();
            let mut count = 0;
            for e in self.depth_reqs[idx].entries.iter_mut().filter(|e| !e.live && e.farm == farm) {
                let id = self.next_md_req_id;
                self.next_md_req_id += 1;
                e.farm_req = id;
                e.live = true;
                count += 1;
                self.depth_subs.push((id, smart));
                self.depth_fanout_map.push((id, user_req));
                body.push((262, id.to_string()));
                body.push((6008, e.con_id.clone()));
                body.push((207, e.exchange.clone()));
                body.push((167, e.sec_type.clone()));
                body.push((264, e.req_type.to_string()));
                if e.req_type != "0" {
                    body.push((6088, "Socket".into()));
                }
                body.push((9830, "1".into()));
            }
            msg.push((146, count.to_string()));
            msg.extend(body);
            log::info!("Depth req {}: {} entries for farm {}", user_req, count, farm);
            out.push((farm, msg));
        }
        out
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

    /// End a depth request: its cancels, one message per farm with the
    /// entries that were sent, as the reference cancels (#452). None for
    /// an unknown request id.
    pub(crate) fn stop_depth(&mut self, req_id: ReqId) -> Option<Vec<(FarmId, Vec<(u32, String)>)>> {
        let pos = self.depth_reqs.iter().position(|r| r.req_id == req_id)?;
        let req = self.depth_reqs.remove(pos);
        let ids: Vec<u32> = req.entries.iter().filter(|e| e.live).map(|e| e.farm_req).collect();
        self.depth_subs.retain(|(id, _)| !ids.contains(id));
        self.depth_fanout_map.retain(|(_, user)| *user != req_id);
        self.depth_tag_to_req.retain(|(_, rid, ..)| *rid != req_id);
        let mut farms: Vec<FarmId> = req.entries.iter().filter(|e| e.live).map(|e| e.farm).collect();
        farms.sort_unstable();
        farms.dedup();
        let mut out = Vec::new();
        for farm in farms {
            let mine: Vec<&DepthEntry> = req.entries.iter().filter(|e| e.live && e.farm == farm).collect();
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
        Some(out)
    }

    /// A farm's connection was lost: its depth entries are no longer on the
    /// wire and go out again when it is back (`resend_depth`).
    pub(crate) fn depth_farm_lost(&mut self, farm: FarmId) {
        let mut gone = Vec::new();
        for r in &mut self.depth_reqs {
            for e in r.entries.iter_mut().filter(|e| e.farm == farm && e.live) {
                e.live = false;
                gone.push(e.farm_req);
            }
        }
        self.depth_subs.retain(|(id, _)| !gone.contains(id));
        self.depth_fanout_map.retain(|(id, _)| !gone.contains(id));
        self.depth_tag_to_req.retain(|(.., f)| *f != farm);
    }

    /// Send again the depth entries of a farm that came back.
    pub(crate) fn resend_depth(&mut self, farm: FarmId) -> Vec<(FarmId, Vec<(u32, String)>)> {
        (0..self.depth_reqs.len()).flat_map(|idx| self.depth_messages(idx, Some(farm))).collect()
    }

    /// A depth entry the farm refused (#452): a single-exchange request ends
    /// with 354, or 10089 when an API subscription is needed, as the
    /// reference; a SmartDepth component is dropped from its request.
    fn depth_rejected(&mut self, farm_req: u32, needs_api_subscription: bool, shared: &SharedState) -> bool {
        let Some(pos) = self.depth_reqs.iter().position(|r| r.entries.iter().any(|e| e.live && e.farm_req == farm_req)) else {
            return false;
        };
        let req = &mut self.depth_reqs[pos];
        if req.smart {
            req.entries.retain(|e| e.farm_req != farm_req);
            self.depth_subs.retain(|(id, _)| *id != farm_req);
            self.depth_fanout_map.retain(|(id, _)| *id != farm_req);
            log::warn!("SmartDepth req {}: a component was refused", req.req_id);
            return true;
        }
        let req = self.depth_reqs.remove(pos);
        let ids: Vec<u32> = req.entries.iter().map(|e| e.farm_req).collect();
        self.depth_subs.retain(|(id, _)| !ids.contains(id));
        self.depth_fanout_map.retain(|(_, user)| *user != req.req_id);
        self.depth_tag_to_req.retain(|(_, rid, ..)| *rid != req.req_id);
        let (code, text) = if needs_api_subscription {
            (10089, "Requested market data requires additional subscription for API. See link in 'Market Data Connections' dialog for more details.")
        } else {
            (354, "Requested market data is not subscribed.")
        };
        shared.orders.push_order_error(i64::from(req.req_id), code, text.to_string());
        true
    }

    /// Parse 35=P depth entries (byte-aligned: [00][3B stag][field tags...][58 terminator]).
    /// SmartDepth entries may contain multiple price+size pairs (bid then ask).
    /// Field tag encoding: bit 5(0x20)=size, bit 3(0x08)=ask, bit 2(0x04)=snapshot, bit 0(0x01)=2-byte.
    fn handle_depth_35p(&self, body: &[u8], shared: &SharedState) {
        use crate::types::DepthUpdate;
        let mut pos = 0;
        let mut bid_position: i32 = 0;
        let mut ask_position: i32 = 0;

        while pos < body.len() {
            if body[pos] != 0x00 { pos += 1; continue; }
            pos += 1;
            if pos + 3 > body.len() { break; }

            let stag = ((body[pos] as u32) << 16) | ((body[pos+1] as u32) << 8) | (body[pos+2] as u32);
            pos += 3;

            let (req_id, is_smart, min_tick) = match self.depth_tag_to_req.iter()
                .find(|(s, .., f)| *s == stag && *f == self.rx_farm)
                .map(|(_, r, sm, mt, _)| (*r, *sm, *mt))
            {
                Some(v) => v,
                None => { continue; }
            };

            // Parse field tags, pushing a depth update each time we complete a price+size pair.
            let mut price: f64 = 0.0;
            let mut size: f64 = 0.0;
            let mut side: i32 = 1;
            let mut is_snapshot = false;
            let mut has_price = false;
            let mut has_size = false;

            while pos < body.len() && body[pos] != 0x58 && body[pos] != 0x00 {
                let tag = body[pos];
                // Only recognize tags with known bits (0x20, 0x08, 0x04, 0x01).
                // Bit 7 (0x80) or bit 6 (0x40) set → unknown encoding, stop.
                if tag & 0xC0 != 0 { break; }
                pos += 1;

                let is_size_field = tag & 0x20 != 0;
                let is_ask = tag & 0x08 != 0;
                let snapshot = tag & 0x04 != 0;
                let two_byte = tag & 0x01 != 0;

                let new_side = if is_ask { 0 } else { 1 };
                if snapshot { is_snapshot = true; }

                // If side changes and we have a pending pair, flush it first
                if has_price && has_size && new_side != side {
                    let position = if side == 0 { let p = ask_position; ask_position += 1; p }
                                  else { let p = bid_position; bid_position += 1; p };
                    let operation = if is_snapshot { 0 } else { 1 };
                    shared.market.push_depth_update(DepthUpdate {
                        req_id, position, market_maker: String::new(),
                        operation, side, price, size, is_smart_depth: is_smart,
                    });
                    has_price = false;
                    has_size = false;
                }
                side = new_side;

                if two_byte {
                    if pos + 2 > body.len() { break; }
                    let val = ((body[pos] as u16) << 8) | (body[pos+1] as u16);
                    pos += 2;
                    if is_size_field { size = val as f64; has_size = true; }
                    else { price = val as f64 * min_tick; has_price = true; }
                } else {
                    if pos >= body.len() { break; }
                    let val = body[pos];
                    pos += 1;
                    if is_size_field { size = val as f64 * 100.0; has_size = true; }
                    else { price = val as f64 * min_tick; has_price = true; }
                }

                // Flush complete pair immediately
                if has_price && has_size {
                    let position = if side == 0 { let p = ask_position; ask_position += 1; p }
                                  else { let p = bid_position; bid_position += 1; p };
                    let operation = if is_snapshot { 0 } else { 1 };
                    shared.market.push_depth_update(DepthUpdate {
                        req_id, position, market_maker: String::new(),
                        operation, side, price, size, is_smart_depth: is_smart,
                    });
                    has_price = false;
                    has_size = false;
                }
            }

            if pos < body.len() && body[pos] == 0x58 { pos += 1; }
        }
    }

    /// Parse 35=Y depth entries (NASDAQ TotalView market-maker level).
    /// Wire format (from wire capture):
    ///   Header: [2B misc][2B stag_uint16_be]
    ///   Stag switch sentinel: [80 00][2B stag_uint16_be]
    ///   Snapshot entry: [C4|44][4B market_maker][1B position][field_tags...]
    ///   Compact entry:  [80|00][1B position][field_tags...]
    ///     C4/80 = continuation, 44/00 = terminal (last entry for this stag section).
    /// Field tag encoding: bit 7=size, bit 5=ask, bit 2=snapshot, bits 0-1=value_len (00=1B,01=2B,10=3B).
    fn handle_depth_35y(&self, msg: &[u8], shared: &SharedState) {
        use crate::types::DepthUpdate;
        let body = match find_body_after_tag(msg, b"35=Y\x01") {
            Some(b) => b,
            None => return,
        };

        // Header: 2 bytes misc. The stag is set by the first 80 00 [2B stag] sentinel.
        if body.len() < 4 { return; }

        // Try header stag at body[2..4] (common case).
        let mut req_id: ReqId = 0;
        let mut is_smart = false;
        let mut min_tick: f64 = 0.01;
        let mut pos = 2;

        let hdr_stag = ((body[2] as u32) << 8) | (body[3] as u32);
        if let Some((r, sm, mt)) = self.lookup_depth_stag(hdr_stag) {
            req_id = r;
            is_smart = sm;
            min_tick = mt;
            pos = 4;
        }
        // If header stag didn't match, start scanning from pos=2;
        // the first stag switch sentinel will set req_id.

        while pos < body.len() {
            let b = body[pos];

            // Stag switch sentinel: 80 00 [2B stag] — bid_size=0 repurposed.
            // Also detect 00 00 [2B stag] (3-byte stag with high byte 0x00, at message boundaries).
            if (b == 0x80 || b == 0x00) && pos + 4 <= body.len() && body[pos + 1] == 0x00 {
                let candidate = ((body[pos + 2] as u32) << 8) | (body[pos + 3] as u32);
                if let Some((r, sm, mt)) = self.lookup_depth_stag(candidate) {
                    req_id = r;
                    is_smart = sm;
                    min_tick = mt;
                    pos += 4;
                    continue;
                }
            }

            // Snapshot entry: [C4|44][4B market_maker][1B position][field_tags...]
            if b == 0xC4 || b == 0x44 {
                pos += 1;
                if pos + 5 > body.len() { break; }
                let mm = String::from_utf8_lossy(&body[pos..pos + 4]).trim().to_string();
                pos += 4;
                let book_position = body[pos] as i32;
                pos += 1;

                if let Some((price, size, side, is_snapshot)) = self.parse_depth_fields(body, &mut pos, min_tick) {
                    shared.market.push_depth_update(DepthUpdate {
                        req_id, position: book_position, market_maker: mm,
                        operation: if is_snapshot { 0 } else { 1 },
                        side, price, size, is_smart_depth: is_smart,
                    });
                }
                continue;
            }

            // Compact entry: [80|00][1B position][field_tags...]  (no market maker)
            // 80 = continuation, 00 = terminal for this stag section.
            // Guard: stag switch sentinel already checked above.
            // Validate: position must be 0-29 and next byte must be a valid field tag.
            if (b == 0x80 || b == 0x00) && pos + 2 < body.len() {
                let candidate_pos = body[pos + 1];
                let candidate_tag = body[pos + 2];
                // Valid field tags: only bits 7,5,2,1,0 set (mask 0xAF). Reject bits 6,4,3.
                if candidate_pos < 30 && candidate_tag & 0x50 == 0 && candidate_tag & 0x08 == 0 {
                    pos += 1;
                    let book_position = body[pos] as i32;
                    pos += 1;

                    if let Some((price, size, side, is_snapshot)) = self.parse_depth_fields(body, &mut pos, min_tick) {
                        shared.market.push_depth_update(DepthUpdate {
                            req_id, position: book_position, market_maker: String::new(),
                            operation: if is_snapshot { 0 } else { 1 },
                            side, price, size, is_smart_depth: is_smart,
                        });
                    }
                    continue;
                }
            }

            // Unknown byte — skip
            pos += 1;
        }
    }

    /// Look up a depth server_tag → (req_id, is_smart, min_tick).
    fn lookup_depth_stag(&self, stag: u32) -> Option<(ReqId, bool, f64)> {
        self.depth_tag_to_req.iter()
            .find(|(s, .., f)| *s == stag && *f == self.rx_farm)
            .map(|(_, r, sm, mt, _)| (*r, *sm, *mt))
    }

    /// Parse one price + one size field tag pair. Returns (price, size, side, is_snapshot).
    /// Advances `pos` past consumed bytes.
    fn parse_depth_fields(&self, body: &[u8], pos: &mut usize, min_tick: f64) -> Option<(f64, f64, i32, bool)> {
        let mut price: f64 = 0.0;
        let mut size: f64 = 0.0;
        let mut side: i32 = 1; // default bid
        let mut is_snapshot = false;
        let mut has_price = false;
        let mut has_size = false;

        // Parse up to 2 field tags (one price + one size).
        for _ in 0..2 {
            if *pos >= body.len() { break; }
            let tag = body[*pos];
            // Valid field tags use bits 7,5,2,1,0. Reject if bit 6 or bit 4 set.
            if tag & 0x50 != 0 { break; }
            // Reject entry/stag prefixes that would start a new entry.
            if tag == 0xC4 || tag == 0x44 { break; }
            *pos += 1;

            let is_size_field = tag & 0x80 != 0;
            let is_ask = tag & 0x20 != 0;
            if tag & 0x04 != 0 { is_snapshot = true; }
            if is_ask { side = 0; } else { side = 1; }

            let val_len = tag & 0x03;
            let val: u32 = match val_len {
                0 => {
                    if *pos >= body.len() { break; }
                    let v = body[*pos] as u32; *pos += 1; v
                }
                1 => {
                    if *pos + 2 > body.len() { break; }
                    let v = ((body[*pos] as u32) << 8) | (body[*pos + 1] as u32);
                    *pos += 2; v
                }
                _ => {
                    if *pos + 3 > body.len() { break; }
                    let v = ((body[*pos] as u32) << 16) | ((body[*pos + 1] as u32) << 8) | (body[*pos + 2] as u32);
                    *pos += 3; v
                }
            };

            if is_size_field {
                size = val as f64;
                has_size = true;
            } else {
                price = val as f64 * min_tick;
                has_price = true;
            }
        }

        if has_price || has_size { Some((price, size, side, is_snapshot)) } else { None }
    }

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, _event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        // Entries and tags of the farms opened on demand stay (#445).
        self.farm_lost(PRIMARY_MD, context);
        // Its depth entries go out again when it is back (#452).
        self.depth_farm_lost(PRIMARY_MD);
        // Don't emit Event::Disconnected — auto-reconnect handles farm drops transparently.
        // Python is only notified if reconnect exhausts retries.
    }

    /// Test-only: set disconnected without clearing state or emitting events.
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

    fn handle_tick_news(&mut self, msg: &[u8], context: &Context, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };

        if body.len() < 12 { return; }

        let tick_type = u16::from_be_bytes([body[0], body[1]]);
        if tick_type != 0x1E90 { return; }

        let server_tag = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
        // A tag of no known request is dropped, as the reference does: it
        // is never given to another contract (#292).
        let Some(instrument) = context.market.instrument_by_farm_tag(self.rx_farm, server_tag) else {
            log::warn!("News tick for server tag {} of no known request: dropped", server_tag);
            return;
        };

        let batch_count = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;
        let mut pos = 12;

        for _ in 0..batch_count {
            if pos + 4 > body.len() { break; }
            let prov_len = u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as usize;
            pos += 4;
            if pos + prov_len > body.len() { break; }
            let provider = String::from_utf8_lossy(&body[pos..pos+prov_len]).to_string();
            pos += prov_len;

            if pos + 4 > body.len() { break; }
            pos += 4;

            if pos + 2 > body.len() { break; }
            let aid_len = u16::from_be_bytes([body[pos], body[pos+1]]) as usize;
            pos += 2;
            if pos + aid_len > body.len() { break; }
            let article_id = String::from_utf8_lossy(&body[pos..pos+aid_len]).to_string();
            pos += aid_len;

            if pos + 8 > body.len() { break; }
            pos += 4;
            let timestamp = u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as u64;
            pos += 4;

            if pos + 4 > body.len() { break; }
            let hl_len = u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as usize;
            pos += 4;
            if pos + hl_len > body.len() { break; }
            let raw_headline = String::from_utf8_lossy(&body[pos..pos+hl_len]).to_string();
            pos += hl_len;

            let headline = if raw_headline.starts_with('{') {
                match raw_headline.find('}') {
                    Some(i) => raw_headline[i+1..].to_string(),
                    None => raw_headline,
                }
            } else {
                raw_headline
            };

            let news = crate::types::TickNews {
                instrument,
                provider_code: provider,
                article_id,
                headline,
                timestamp,
            };
            shared.market.push_tick_news(news.clone());
            emit(event_tx, Event::News(news));
        }
    }
}

/// The reference's security type code (`SecType` value), appended in hex
/// to a short BBO exchange code (ibx#449).
fn sec_type_code(sec_type: &str) -> Option<u8> {
    Some(match sec_type {
        "STK" => 1, "CFD" => 2, "OPT" => 3, "FOP" => 4, "WAR" => 5, "FUT" => 6, "FWD" => 7,
        "BAG" => 8, "CASH" => 10, "IND" => 11, "BOND" => 12, "BILL" => 13, "FIXED" => 14,
        "FUND" => 15, "SLB" => 16, "NEWS" => 17, "CMDTY" => 18, "BSK" => 19, "IOPT" => 20,
        "ICU" => 21, "ICS" => 22, "PHYSS" => 23, "CRYPTO" => 24,
        _ => return None,
    })
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
            farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
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

    /// EUR.USD acked and set up as captured 02/10/2026, then the given
    /// tick messages; the marks the client sees.
    fn eur_usd_marks(messages: &[Vec<u8>]) -> crate::types::QuoteMarks {
        let shared = SharedState::new();
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let id = context.market.register(12087792);
        context.market.set_routing(id, "CASH", "IDEALPRO");
        farm.send_mktdata_subscribe(12087792, "EUR", "IDEALPRO", "CASH", "", 0.0, "", "", id, 0, &mut None, &mut hb);
        let bid_ask = farm.next_md_req_id - 2;
        for ack in [
            format!("8=O35=Q8,{bid_ask},1e-05,0,3,ffffffff,,1,1"),
            format!("8=O35=Q6,{},5e-05,0,1,ffffffff,,1,1", bid_ask + 1),
        ] {
            farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
        }
        farm.handle_ticker_setup(b"8=O35=L12087792,5e-05,7,,1", &mut context);
        for msg in messages {
            farm.handle_tick_data(msg, &mut context, &shared, &None);
        }
        shared.market.marks(id)
    }

    const EUR_USD_QUOTE: Block<'static> = (false, 8, &[(0, 112_547), (4, 4_000_000), (1, 112_549), (5, 12_000_000), (11, 0)]);
    const EUR_USD_TRADE: Block<'static> = (false, 7, &[(2, 22_510), (6, 0), (13, 0), (20, 1_790_921_652), (21, 126)]);
    const EUR_USD_DAILY: Block<'static> = (true, 7, &[(3, 22_486), (20, 20_261_001), (8, 22_517), (9, 22_464), (10, 0), (12, 0)]);

    // ibx#446: the captured first EUR.USD message (quote, trade, daily
    // figures): the trade came with status 0, before the daily figures; no
    // auto-execution flag.
    #[test]
    fn eur_usd_snapshot_message_gives_status_and_block_order() {
        let marks = eur_usd_marks(&[tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY])]);
        assert_eq!(marks.halted(), Some(0));
        assert!(!marks.daily_first());
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (None, None));
        // The daily figures first in another message.
        let marks = eur_usd_marks(&[
            tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY]),
            tick_message(&[EUR_USD_DAILY, EUR_USD_TRADE]),
        ]);
        assert!(marks.daily_first());
        // A trade with no status keeps the last one.
        let marks = eur_usd_marks(&[
            tick_message(&[EUR_USD_TRADE]),
            tick_message(&[(false, 7, &[(2, 22_511)])]),
        ]);
        assert_eq!(marks.halted(), Some(0));
        let marks = eur_usd_marks(&[tick_message(&[(false, 7, &[(2, 22_511), (13, 3)])])]);
        assert_eq!(marks.halted(), Some(3));
    }

    // ibx#446: the updates of a message, in its order, for the stream: the
    // captured first EUR.USD message (book, trade with its time, daily
    // figures), then a book-only message; each message counts.
    #[test]
    fn message_updates_in_their_order() {
        use crate::types::{Pass, SizeKind};
        let marks = eur_usd_marks(&[tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY])]);
        assert_eq!(marks.passes().collect::<Vec<_>>(), [Pass::Trade { time: true, exchange: false }, Pass::Daily]);
        assert!(marks.quote_update());
        assert_eq!(marks.message_seq(), 1);
        for kind in [SizeKind::Bid, SizeKind::Ask, SizeKind::Last, SizeKind::Volume] {
            assert!(marks.seen(kind), "{kind:?}");
        }
        let marks = eur_usd_marks(&[
            tick_message(&[EUR_USD_QUOTE, EUR_USD_TRADE, EUR_USD_DAILY]),
            tick_message(&[(false, 8, &[(5, 6_000_000)])]),
        ]);
        assert_eq!(marks.passes().count(), 0);
        assert!(marks.quote_update());
        assert_eq!(marks.message_seq(), 2);
        // Daily figures first, then a trade with its exchange only, then
        // one with its time (a delta) and its exchange; no book update.
        let marks = eur_usd_marks(&[tick_message(&[
            (true, 7, &[(10, 5)]), (false, 7, &[(27, 8)]), (false, 7, &[(6, 2), (21, 92), (27, 1024)]),
        ])]);
        assert_eq!(marks.passes().collect::<Vec<_>>(), [
            Pass::Daily, Pass::Trade { time: false, exchange: true }, Pass::Trade { time: true, exchange: true },
        ]);
        assert!(!marks.quote_update());
    }

    // ibx#446: the auto-execution bits of a quote (both set on the
    // captured SPY quote of 02/10/2026), from either attribute type; a
    // trade's attribute is its status, not these bits.
    #[test]
    fn quote_auto_execution_bits() {
        let marks = eur_usd_marks(&[tick_message(&[(false, 8, &[(0, 112_547), (7, 12)])])]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(true), Some(true)));
        let marks = eur_usd_marks(&[tick_message(&[(false, 8, &[(0, 112_547), (13, 4)])])]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(true), Some(false)));
        let marks = eur_usd_marks(&[
            tick_message(&[(false, 8, &[(7, 12)])]),
            tick_message(&[(false, 8, &[(7, 0)])]),
        ]);
        assert_eq!((marks.bid_auto(), marks.ask_auto()), (Some(false), Some(false)));
        let marks = eur_usd_marks(&[tick_message(&[(false, 7, &[(2, 22_511), (13, 12)])])]);
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
            farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
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
        farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
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
        farm.handle_subscription_ack(b"8=O\x0135=Q\x011101,5,0.01,0,3,9c,,1,1", &mut context, &shared);
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
            farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
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
                farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
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
        farm.handle_subscription_ack(ack.as_bytes(), &mut context, &SharedState::new());
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
