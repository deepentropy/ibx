use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::engine::context::Context;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{FarmSlot, InstrumentId};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, fast_extract_msg_type, find_body_after_tag};

pub(crate) struct FarmState {
    pub(crate) next_md_req_id: u32,
    pub(crate) md_req_to_instrument: Vec<(u32, InstrumentId)>,
    pub(crate) instrument_md_reqs: Vec<(InstrumentId, FarmSlot, Vec<u32>)>,
    /// Active depth subscriptions: (req_id, farm_slot, is_smart_depth).
    pub(crate) depth_subs: Vec<(u32, FarmSlot, bool)>,
    /// Maps server_tag → (depth_req_id, is_smart_depth, min_tick) for active depth subscriptions.
    pub(crate) depth_tag_to_req: Vec<(u32, u32, bool, f64)>,
    /// SmartDepth fan-out: maps internal sub_req → user's original req_id.
    depth_fanout_map: Vec<(u32, u32)>,
    /// Primary depth subscription params for reconnect: (req_id, con_id, exchange, sec_type, num_rows, is_smart_depth).
    depth_resub_info: Vec<(u32, i64, String, String, i32, bool)>,
    /// Option resub info: (instrument, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier).
    md_resub_info: Vec<(InstrumentId, String, String, String, String, f64, String, String)>,
    pub(crate) disconnected: bool,
    pub(crate) tick_buf: Vec<tick_decoder::RawTick>,
    pub(crate) farm_msg_buf: Vec<Vec<u8>>,
}

impl FarmState {
    pub(crate) fn new() -> Self {
        Self {
            next_md_req_id: 1,
            md_req_to_instrument: Vec::new(),
            instrument_md_reqs: Vec::new(),
            depth_subs: Vec::new(),
            depth_tag_to_req: Vec::new(),
            depth_fanout_map: Vec::new(),
            depth_resub_info: Vec::new(),
            md_resub_info: Vec::new(),
            disconnected: false,
            tick_buf: Vec::with_capacity(16),
            farm_msg_buf: Vec::with_capacity(32),
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
                        let (unsigned, _valid) = conn.unsign(raw);
                        let inner = fixcomp::fixcomp_decompress(&unsigned);
                        self.farm_msg_buf.extend(inner);
                    }
                    Frame::Binary(raw) => {
                        let (unsigned, _valid) = conn.unsign(raw);
                        self.farm_msg_buf.push(unsigned);
                    }
                    Frame::Fix(raw) => {
                        let (unsigned, _valid) = conn.unsign(raw);
                        self.farm_msg_buf.push(unsigned);
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
    }

    pub(crate) fn poll_secondary_farm(
        &mut self,
        secondary_conn: &mut Option<Connection>,
        slot: &FarmSlot,
        _farm_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        let conn = match secondary_conn.as_mut() {
            Some(c) => c,
            None => return,
        };
        match conn.try_recv() {
            Ok(0) => return,
            Err(e) => {
                log::error!("{:?} connection lost: {}", slot, e);
                self.handle_secondary_disconnect(secondary_conn, slot, context, shared, event_tx);
                return;
            }
            Ok(_) => {
                let shb = hb.secondary_hb_mut(slot);
                shb.last_recv = Instant::now();
                shb.pending_test = None;
            }
        }
        let frames = conn.extract_frames();
        let mut msgs = Vec::new();
        for frame in &frames {
            match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = conn.unsign(raw);
                    msgs.extend(fixcomp::fixcomp_decompress(&unsigned));
                }
                Frame::Binary(raw) => {
                    let (unsigned, _) = conn.unsign(raw);
                    msgs.push(unsigned);
                }
                Frame::Fix(raw) => {
                    let (unsigned, _) = conn.unsign(raw);
                    msgs.push(unsigned);
                }
            }
        }
        for msg in &msgs {
            self.process_farm_message(msg, secondary_conn, context, shared, event_tx, hb);
        }
    }

    /// Handle disconnect of a secondary farm: drop connection, clear subscriptions for that slot.
    pub(crate) fn handle_secondary_disconnect(
        &mut self,
        secondary_conn: &mut Option<Connection>,
        slot: &FarmSlot,
        _context: &mut Context,
        _shared: &SharedState,
        _event_tx: &Option<Sender<Event>>,
    ) {
        *secondary_conn = None;
        // Collect req IDs and instrument IDs for the disconnected farm slot
        let mut stale_req_ids = Vec::new();
        let mut affected_count = 0usize;
        self.instrument_md_reqs.retain(|(_, farm, reqs)| {
            if farm == slot {
                stale_req_ids.extend(reqs.iter().copied());
                affected_count += 1;
                false
            } else {
                true
            }
        });
        self.md_req_to_instrument.retain(|(rid, _)| !stale_req_ids.contains(rid));
        if affected_count > 0 {
            log::warn!("{:?} disconnected, cleared {} instrument subscriptions", slot, affected_count);
            // Don't emit Event::Disconnected — secondary farm drops are not session-level failures.
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
                self.handle_subscription_ack(msg, context);
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
            b"UP" => {
                let parsed = fix::fix_parse(msg);
                super::ccp::handle_position_update(&parsed, context, shared, event_tx);
            }
            b"Y" => self.handle_depth_35y(msg, shared),
            b"G" => self.handle_tick_news(msg, context, shared, event_tx),
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
                    if self.depth_tag_to_req.iter().any(|(s, _, _, _)| *s == stag) {
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
        tick_decoder::decode_ticks_35p_into(body, &mut ticks);
        let mut notified: u32 = 0;

        // Phase 1: Apply all ticks to internal quotes before publishing.
        for tick in &ticks {
            let instrument = match context.market.instrument_by_server_tag(tick.server_tag) {
                Some(id) => id,
                None => continue,
            };

            let mts = context.market.min_tick_scaled(instrument);
            let q = context.market.quote_mut(instrument);

            match tick.tick_type {
                tick_decoder::O_BID_PRICE => { q.bid = tick.magnitude * mts; }
                tick_decoder::O_ASK_PRICE => { q.ask = tick.magnitude * mts; }
                tick_decoder::O_LAST_PRICE => { q.last = tick.magnitude * mts; }
                tick_decoder::O_HIGH_PRICE => { q.high = tick.magnitude * mts; }
                tick_decoder::O_LOW_PRICE => { q.low = tick.magnitude * mts; }
                tick_decoder::O_OPEN_PRICE => { q.open = tick.magnitude * mts; }
                tick_decoder::O_CLOSE_PRICE => { q.close = tick.magnitude * mts; }
                tick_decoder::O_BID_SIZE => { q.bid_size = tick.magnitude; }
                tick_decoder::O_ASK_SIZE => { q.ask_size = tick.magnitude; }
                tick_decoder::O_LAST_SIZE => { q.last_size = tick.magnitude; }
                tick_decoder::O_VOLUME => { q.volume = tick.magnitude; }
                tick_decoder::O_TIMESTAMP | tick_decoder::O_LAST_TS => { q.timestamp_ns = tick.magnitude as u64; }
                _ => {}
            }

            notified |= 1u32 << instrument;
        }

        // Phase 2: Publish complete quotes after all ticks in the batch are applied.
        let mut remaining = notified;
        while remaining != 0 {
            let instrument = remaining.trailing_zeros();
            remaining &= remaining - 1;
            shared.market.push_quote(instrument, context.quote(instrument));
            emit(event_tx, Event::Tick(instrument));
        }
        self.tick_buf = ticks;
    }

    fn handle_subscription_ack(&mut self, msg: &[u8], context: &mut Context) {
        let body = match find_body_after_tag(msg, b"35=Q\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 { return; }
        let server_tag: u32 = match parts[0].parse() { Ok(v) => v, Err(_) => return };
        let req_id: u32 = match parts[1].parse() { Ok(v) => v, Err(_) => return };
        let min_tick: f64 = parts[2].parse().unwrap_or(0.01);

        // Depth ack: always map the server_tag if this req_id is a depth subscription,
        // even when depth_levels=0 (book empty now but updates may arrive later).
        let depth_levels: i32 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
        if let Some((_, _, is_smart)) = self.depth_subs.iter().find(|(id, _, _)| *id == req_id) {
            let is_smart = *is_smart;
            // For SmartDepth fan-out, map back to the user's original req_id
            let user_req = self.depth_fanout_map.iter()
                .find(|(sub, _)| *sub == req_id)
                .map(|(_, user)| *user)
                .unwrap_or(req_id);
            self.depth_tag_to_req.push((server_tag, user_req, is_smart, min_tick));
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
            None => return,
        };

        context.market.register_server_tag(server_tag, instrument);
        context.market.set_min_tick(instrument, min_tick);
        log::info!("Subscribed instrument {} -> server_tag {}, minTick {}", instrument, server_tag, min_tick);
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
            context.market.register_server_tag(server_tag, instrument);
            context.market.set_min_tick(instrument, min_tick);
            log::info!("Ticker setup: con_id {} -> server_tag {}, minTick {}", con_id, server_tag, min_tick);
        }
    }

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
        farm: FarmSlot,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        eufarm_conn: &mut Option<Connection>,
        jfarm_conn: &mut Option<Connection>,
        usopt_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let bid_ask_id = self.next_md_req_id;
        let last_id = self.next_md_req_id + 1;
        self.next_md_req_id += 2;

        self.md_req_to_instrument.push((bid_ask_id, instrument));
        self.md_req_to_instrument.push((last_id, instrument));

        match self.instrument_md_reqs.iter_mut().find(|(id, _, _)| *id == instrument) {
            Some((_, _, reqs)) => { reqs.push(bid_ask_id); reqs.push(last_id); }
            None => self.instrument_md_reqs.push((instrument, farm, vec![bid_ask_id, last_id])),
        }
        if self.md_resub_info.iter().all(|(id, ..)| *id != instrument) {
            self.md_resub_info.push((instrument, symbol.to_string(), exchange.to_string(), sec_type.to_string(), last_trade_date.to_string(), strike, right.to_string(), multiplier.to_string()));
        }

        if let Some(conn) = farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn) {
            let bid_ask_str = bid_ask_id.to_string();
            let last_str = last_id.to_string();
            let con_id_str = (con_id as u32).to_string();
            let ts = chrono_free_timestamp();

            // When con_id is known, use the proven minimal format (con_id + BEST + CS).
            // The server resolves the full contract details from con_id regardless of sec_type.
            // When con_id is 0, include descriptive fields so the server can resolve by description.
            if con_id > 0 {
                let _ = conn.send_fixcomp(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                    (fix::TAG_SENDING_TIME, &ts),
                    (263, "1"),
                    (146, "2"),
                    (262, &bid_ask_str),
                    (6008, &con_id_str),
                    (207, "BEST"),
                    (167, "CS"),
                    (264, "442"),
                    (6088, "Socket"),
                    (9830, "1"),
                    (9839, "1"),
                    (262, &last_str),
                    (6008, &con_id_str),
                    (207, "BEST"),
                    (167, "CS"),
                    (264, "443"),
                    (6088, "Socket"),
                    (9830, "1"),
                    (9839, "1"),
                ]);
            } else {
                // No con_id — send descriptive fields
                let fix_exchange = crate::control::contracts::exchange_to_fix(exchange);
                let fix_sec_type = match sec_type {
                    "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND",
                    "CASH" => "CASH", other => other,
                };
                let strike_str = if strike > 0.0 { strike.to_string() } else { String::new() };
                let mut tags: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                    (fix::TAG_SENDING_TIME, &ts),
                    (263, "1"),
                    (146, "2"),
                ];
                for (req_str, depth) in [(&bid_ask_str, "442"), (&last_str, "443")] {
                    tags.push((262, req_str));
                    tags.push((55, symbol));
                    tags.push((207, fix_exchange));
                    tags.push((167, fix_sec_type));
                    if !last_trade_date.is_empty() { tags.push((200, last_trade_date)); }
                    if strike > 0.0 { tags.push((202, &strike_str)); }
                    if !right.is_empty() { tags.push((201, right)); }
                    if !multiplier.is_empty() { tags.push((231, multiplier)); }
                    tags.push((264, depth));
                    tags.push((6088, "Socket"));
                    tags.push((9830, "1"));
                    tags.push((9839, "1"));
                }
                let _ = conn.send_fixcomp(&tags);
            }
            log::info!("Sent 35=V subscribe: con_id={} sec_type={} farm={:?} ids={},{} seq={}",
                con_id, sec_type, farm, bid_ask_id, last_id, conn.seq);
            hb.last_farm_sent = Instant::now();
        }
    }

    pub(crate) fn send_mktdata_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        eufarm_conn: &mut Option<Connection>,
        jfarm_conn: &mut Option<Connection>,
        usopt_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let (farm, reqs) = match self.instrument_md_reqs.iter()
            .position(|(id, _, _)| *id == instrument)
        {
            Some(idx) => {
                let (_, farm, reqs) = self.instrument_md_reqs.remove(idx);
                (farm, reqs)
            }
            None => return,
        };
        self.md_resub_info.retain(|(id, ..)| *id != instrument);

        let conn = match farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn) {
            Some(c) => c,
            None => return,
        };

        for req_id in reqs {
            let req_id_str = req_id.to_string();
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "2"),
            ]);
        }
        hb.last_farm_sent = Instant::now();
    }

    pub(crate) fn send_depth_subscribe(
        &mut self,
        req_id: u32,
        con_id: i64,
        exchange: &str,
        sec_type: &str,
        _num_rows: i32,
        is_smart_depth: bool,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        eufarm_conn: &mut Option<Connection>,
        jfarm_conn: &mut Option<Connection>,
        usopt_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let farm = crate::types::farm_for_instrument(exchange, sec_type);
        let fix_sec_type = match sec_type {
            "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND",
            "CASH" => "CASH", other => other,
        };
        self.depth_subs.push((req_id, farm, is_smart_depth));
        self.depth_resub_info.push((req_id, con_id, exchange.to_string(), sec_type.to_string(), _num_rows, is_smart_depth));

        // ib-agent#86: SmartDepth requires per-exchange fan-out. The server ACKs a BEST/SMART
        // subscribe but never sends data for it. Data only arrives for individual exchanges.
        // Auto-enable fan-out when exchange is SMART/BEST (aggregated routing), since
        // single-exchange depth to SMART returns nothing.
        let needs_fanout = is_smart_depth || matches!(exchange, "SMART" | "BEST" | "");
        let exchanges: &[&str] = if needs_fanout {
            // US equity exchanges that the gateway fans out to (ib-agent#86 capture)
            &["NASDAQ", "IEX", "BATS", "ARCA", "BEX", "NYSE", "BYX", "NYSENAT", "T24X",
              "DRCTEDGE", "MEMX", "PEARL", "AMEX", "CHX", "LTSE", "PSX", "ISE", "EDGEA"]
        } else {
            // Single exchange subscribe
            static SINGLE: [&str; 0] = [];
            &SINGLE
        };

        if let Some(conn) = farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn) {
            let con_id_str = (con_id as u32).to_string();

            if !exchanges.is_empty() {
                // SmartDepth: fan-out to individual exchanges.
                // Each sub gets a unique req_id tracked as a depth subscription.
                for exch in exchanges {
                    let sub_req = self.next_md_req_id;
                    self.next_md_req_id += 1;
                    self.depth_subs.push((sub_req, farm, true));
                    self.depth_fanout_map.push((sub_req, req_id));
                    let sub_req_str = sub_req.to_string();
                    self.send_depth_one(conn, &sub_req_str, &con_id_str, exch, fix_sec_type);
                }
                log::info!("SmartDepth fan-out: req={} con_id={} -> {} exchanges", req_id, con_id, exchanges.len());
            } else {
                // Single exchange
                let fix_exchange = match exchange {
                    "ISLAND" => "NASDAQ",
                    other => other,
                };
                let req_id_str = req_id.to_string();
                self.send_depth_one(conn, &req_id_str, &con_id_str, fix_exchange, fix_sec_type);
                log::info!("Depth subscribe: req={} con_id={} exchange={}", req_id, con_id, fix_exchange);
            }
            hb.last_farm_sent = Instant::now();
        }
    }

    pub(crate) fn send_depth_unsubscribe(
        &mut self,
        req_id: u32,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        eufarm_conn: &mut Option<Connection>,
        jfarm_conn: &mut Option<Connection>,
        usopt_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let farm = match self.depth_subs.iter().position(|(id, _, _)| *id == req_id) {
            Some(idx) => {
                let (_, f, _) = self.depth_subs.remove(idx);
                f
            }
            None => return,
        };

        // Remove reconnect params
        self.depth_resub_info.retain(|(id, _, _, _, _, _)| *id != req_id);

        // Collect SmartDepth fan-out sub_reqs that map to this user req_id
        let fanout_reqs: Vec<u32> = self.depth_fanout_map.iter()
            .filter(|(_, user)| *user == req_id)
            .map(|(sub, _)| *sub)
            .collect();

        // Remove fan-out entries from depth_subs and depth_fanout_map
        self.depth_subs.retain(|(id, _, _)| !fanout_reqs.contains(id));
        self.depth_fanout_map.retain(|(_, user)| *user != req_id);

        // Clear server_tag mappings for this req_id
        self.depth_tag_to_req.retain(|(_, rid, _, _)| *rid != req_id);

        if let Some(conn) = farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn) {
            // Send unsub for each fan-out sub_req (SmartDepth per-exchange)
            for sub_req in &fanout_reqs {
                let sub_req_str = sub_req.to_string();
                let _ = conn.send_fixcomp(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                    (262, &sub_req_str),
                    (263, "2"),
                ]);
            }
            // Send unsub for the primary req_id
            let req_id_str = req_id.to_string();
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "2"),
            ]);
            hb.last_farm_sent = Instant::now();
            log::info!("Sent depth unsubscribe: req_id={} (+ {} fan-out)", req_id, fanout_reqs.len());
        }
    }

    /// Send a single depth subscribe for one exchange.
    fn send_depth_one(&self, conn: &mut Connection, req_id_str: &str, con_id_str: &str, exchange: &str, sec_type: &str) {
        let is_direct = matches!(exchange, "NASDAQ" | "BATS" | "ARCA" | "BEX" | "NYSE" | "IEX"
            | "BYX" | "NYSENAT" | "T24X");
        if is_direct {
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (263, "1"), (146, "1"), (262, req_id_str),
                (6008, con_id_str), (207, exchange), (167, sec_type),
                (264, "0"), (9830, "1"),
            ]);
        } else {
            // Socket exchanges (DRCTEDGE, MEMX, PEARL, AMEX, CHX, LTSE, PSX, ISE, EDGEA, etc.)
            let _ = conn.send_fixcomp(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (263, "1"), (146, "1"), (262, req_id_str),
                (6008, con_id_str), (207, exchange), (167, sec_type),
                (264, "442"), (6088, "Socket"), (9830, "1"),
            ]);
        }
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
                .find(|(s, _, _, _)| *s == stag)
                .map(|(_, r, sm, mt)| (*r, *sm, *mt))
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
    /// Wire format (from ib-agent#90 capture):
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
        let mut req_id: u32 = 0;
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
    fn lookup_depth_stag(&self, stag: u32) -> Option<(u32, bool, f64)> {
        self.depth_tag_to_req.iter()
            .find(|(s, _, _, _)| *s == stag)
            .map(|(_, r, sm, mt)| (*r, *sm, *mt))
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
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        // Clear depth wire-state (server_tags become invalid after disconnect).
        // depth_resub_info is preserved for resubscription on reconnect.
        self.depth_subs.clear();
        self.depth_tag_to_req.clear();
        self.depth_fanout_map.clear();
        context.market.clear_server_tags();
        context.market.zero_all_quotes();
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
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        eufarm_conn: &mut Option<Connection>,
        jfarm_conn: &mut Option<Connection>,
        usopt_conn: &mut Option<Connection>,
        context: &mut Context,
        hb: &mut HeartbeatState,
    ) {
        *farm_conn = Some(conn);
        self.disconnected = false;
        hb.last_farm_sent = Instant::now();
        hb.last_farm_recv = Instant::now();
        hb.pending_farm_test = None;

        // Preserve original farm slots and option fields for re-subscription
        let active: Vec<(InstrumentId, FarmSlot, i64, String, String, String, String, f64, String, String)> = self.instrument_md_reqs.iter()
            .filter_map(|(id, slot, _)| {
                context.market.con_id(*id).map(|con_id| {
                    let (sym, exch, st, ltd, strike, right, mult) = self.md_resub_info.iter()
                        .find(|(iid, ..)| *iid == *id)
                        .map(|(_, s, e, st, l, k, r, m)| (s.clone(), e.clone(), st.clone(), l.clone(), *k, r.clone(), m.clone()))
                        .unwrap_or_default();
                    (*id, *slot, con_id, sym, exch, st, ltd, strike, right, mult)
                })
            })
            .collect();
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        let old_resub = std::mem::take(&mut self.md_resub_info);
        for (instrument, farm, con_id, sym, exch, st, ltd, strike, right, mult) in active {
            self.send_mktdata_subscribe(con_id, &sym, &exch, &st, &ltd, strike, &right, &mult, instrument, farm, farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn, hb);
        }
        drop(old_resub);

        // Re-subscribe depth subscriptions (depth_resub_info survived disconnect)
        let depth_params: Vec<_> = self.depth_resub_info.drain(..).collect();
        let depth_count = depth_params.len();
        for (req_id, con_id, exchange, sec_type, num_rows, is_smart_depth) in depth_params {
            self.send_depth_subscribe(
                req_id, con_id, &exchange, &sec_type, num_rows, is_smart_depth,
                farm_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn, usopt_conn, hb,
            );
        }

        log::info!("Farm reconnected, re-subscribed {} instruments + {} depth", self.instrument_md_reqs.len(), depth_count);
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
        let instrument = context.market.instrument_by_server_tag(server_tag).unwrap_or(0);

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

pub(crate) fn farm_conn_for_slot<'a>(
    slot: FarmSlot,
    farm_conn: &'a mut Option<Connection>,
    cashfarm_conn: &'a mut Option<Connection>,
    usfuture_conn: &'a mut Option<Connection>,
    eufarm_conn: &'a mut Option<Connection>,
    jfarm_conn: &'a mut Option<Connection>,
    usopt_conn: &'a mut Option<Connection>,
) -> Option<&'a mut Connection> {
    match slot {
        FarmSlot::UsFarm => farm_conn.as_mut(),
        FarmSlot::UsOpt => {
            if usopt_conn.is_some() {
                usopt_conn.as_mut()
            } else {
                log::warn!("UsOpt unavailable, falling back to UsFarm");
                farm_conn.as_mut()
            }
        }
        FarmSlot::CashFarm => {
            if cashfarm_conn.is_some() {
                cashfarm_conn.as_mut()
            } else {
                log::warn!("CashFarm unavailable, falling back to UsFarm");
                farm_conn.as_mut()
            }
        }
        FarmSlot::UsFuture => {
            if usfuture_conn.is_some() {
                usfuture_conn.as_mut()
            } else {
                log::warn!("UsFuture unavailable, falling back to UsFarm");
                farm_conn.as_mut()
            }
        }
        FarmSlot::EuFarm => {
            if eufarm_conn.is_some() {
                eufarm_conn.as_mut()
            } else {
                log::warn!("EuFarm unavailable, falling back to UsFarm");
                farm_conn.as_mut()
            }
        }
        FarmSlot::JFarm => {
            if jfarm_conn.is_some() {
                jfarm_conn.as_mut()
            } else {
                log::warn!("JFarm unavailable, falling back to UsFarm");
                farm_conn.as_mut()
            }
        }
    }
}
