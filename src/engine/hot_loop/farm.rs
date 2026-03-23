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
        farm_conn: &mut Option<Connection>,
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
                return;
            }
            Ok(_) => {}
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
            self.process_farm_message(msg, farm_conn, context, shared, event_tx, hb);
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
            b"G" => self.handle_tick_news(msg, shared, event_tx),
            _ => {}
        }
    }

    fn handle_tick_data(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=P\x01") {
            Some(b) => b,
            None => return,
        };

        let mut ticks = std::mem::take(&mut self.tick_buf);
        tick_decoder::decode_ticks_35p_into(body, &mut ticks);
        let mut notified: u32 = 0;

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

            let bit = 1u32 << instrument;
            if notified & bit == 0 {
                notified |= bit;
                shared.market.push_quote(instrument, context.quote(instrument));
                emit(event_tx, Event::Tick(instrument));
            }
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
        instrument: InstrumentId,
        farm: FarmSlot,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
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

        if let Some(conn) = farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn) {
            let bid_ask_str = bid_ask_id.to_string();
            let last_str = last_id.to_string();
            let con_id_str = (con_id as u32).to_string();
            let ts = chrono_free_timestamp();
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
            log::info!("Sent 35=V subscribe: con_id={} ids={},{} seq={}",
                con_id, bid_ask_id, last_id, conn.seq);
            hb.last_farm_sent = Instant::now();
        }
    }

    pub(crate) fn send_mktdata_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
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

        let conn = match farm_conn_for_slot(farm, farm_conn, cashfarm_conn, usfuture_conn) {
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

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        context.market.clear_server_tags();
        context.market.zero_all_quotes();
        emit(event_tx, Event::Disconnected);
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        farm_conn: &mut Option<Connection>,
        cashfarm_conn: &mut Option<Connection>,
        usfuture_conn: &mut Option<Connection>,
        context: &mut Context,
        hb: &mut HeartbeatState,
    ) {
        *farm_conn = Some(conn);
        self.disconnected = false;
        hb.last_farm_sent = Instant::now();
        hb.last_farm_recv = Instant::now();
        hb.pending_farm_test = None;

        let active: Vec<(InstrumentId, i64)> = context.market.active_instruments().collect();
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        for (instrument, con_id) in active {
            self.send_mktdata_subscribe(con_id, instrument, FarmSlot::UsFarm, farm_conn, cashfarm_conn, usfuture_conn, hb);
        }
        log::info!("Farm reconnected, re-subscribed {} instruments", self.instrument_md_reqs.len());
    }

    fn handle_tick_news(&mut self, msg: &[u8], shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };

        if body.len() < 12 { return; }

        let tick_type = u16::from_be_bytes([body[0], body[1]]);
        if tick_type != 0x1E90 { return; }

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
) -> Option<&'a mut Connection> {
    match slot {
        FarmSlot::UsFarm => farm_conn.as_mut(),
        FarmSlot::CashFarm => cashfarm_conn.as_mut().or(farm_conn.as_mut()),
        FarmSlot::UsFuture => usfuture_conn.as_mut().or(farm_conn.as_mut()),
        FarmSlot::EuFarm => farm_conn.as_mut(),
        FarmSlot::JFarm => farm_conn.as_mut(),
    }
}
