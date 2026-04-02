use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{InstrumentId, TbtType, PRICE_SCALE, MAX_INSTRUMENTS};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, find_body_after_tag, extract_raw_tag};

pub(crate) struct HmdsState {
    pub(crate) next_tbt_req_id: u32,
    pub(crate) tbt_subscriptions: Vec<(InstrumentId, String, TbtType)>,
    pub(crate) tbt_price_state: [(i64, i64, i64); MAX_INSTRUMENTS],
    pub(crate) next_hmds_query_id: u32,
    pub(crate) disconnected: bool,
    pub(crate) pending_historical: Vec<(String, u32)>,
    pub(crate) pending_head_ts: Vec<(String, u32)>,
    pub(crate) pending_scanner_params: bool,
    pub(crate) pending_scanner: Vec<(String, u32)>,
    pub(crate) next_scanner_id: u32,
    pub(crate) pending_news: Vec<(String, u32)>,
    pub(crate) pending_articles: Vec<(String, u32)>,
    pub(crate) pending_fundamental: Vec<(String, u32)>,
    pub(crate) pending_histogram: Vec<(String, u32)>,
    pub(crate) pending_schedule: Vec<(String, u32)>,
    pub(crate) pending_ticks: Vec<(String, u32, String)>,
    pub(crate) rtbar_subs: Vec<(String, u32, Option<u32>, f64)>,
    /// req_ids that should keep streaming after initial batch (keepUpToDate=True).
    pub(crate) keep_up_to_date_reqs: std::collections::HashSet<u32>,
}

impl HmdsState {
    pub(crate) fn new() -> Self {
        Self {
            next_tbt_req_id: 1,
            tbt_subscriptions: Vec::new(),
            tbt_price_state: [(0, 0, 0); MAX_INSTRUMENTS],
            next_hmds_query_id: 1000,
            disconnected: false,
            pending_historical: Vec::new(),
            pending_head_ts: Vec::new(),
            pending_scanner_params: false,
            pending_scanner: Vec::new(),
            next_scanner_id: 1,
            pending_news: Vec::new(),
            pending_articles: Vec::new(),
            pending_fundamental: Vec::new(),
            pending_histogram: Vec::new(),
            pending_schedule: Vec::new(),
            pending_ticks: Vec::new(),
            rtbar_subs: Vec::new(),
            keep_up_to_date_reqs: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn poll(
        &mut self,
        hmds_conn: &mut Option<Connection>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        if self.disconnected { return; }
        let messages = match hmds_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {}
                    Err(e) => {
                        log::error!("HMDS connection lost: {}", e);
                        self.disconnected = true;
                        return;
                    }
                    Ok(n) => {
                        log::info!("HMDS recv: {} bytes", n);
                        hb.last_hmds_recv = Instant::now();
                        hb.pending_hmds_test = None;
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in &frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            let inner = fixcomp::fixcomp_decompress(&unsigned);
                            msgs.extend(inner);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            msgs.push(unsigned);
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            msgs.push(unsigned);
                        }
                    }
                }
                msgs
            }
        };
        for msg in &messages {
            self.process_hmds_message(msg, hmds_conn, shared, event_tx, hb);
        }
    }

    pub(crate) fn process_hmds_message(
        &mut self,
        msg: &[u8],
        hmds_conn: &mut Option<Connection>,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };
        match msg_type {
            "E" => self.handle_tbt_data(msg, shared, event_tx),
            "0" => {}
            "1" => {
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = hmds_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    hb.last_hmds_sent = Instant::now();
                }
            }
            "W" => {
                if let Some(xml_tag) = parsed.get(&6118) {
                    if let Some(resp) = crate::control::historical::parse_bar_response(xml_tag) {
                        if let Some(pos) = self.pending_historical.iter().position(|(qid, _)| resp.query_id.starts_with(qid.as_str())) {
                            let (_, req_id) = self.pending_historical[pos];
                            let is_complete = resp.is_complete;
                            shared.reference.push_historical_data(req_id, resp.clone());
                            emit(event_tx, Event::HistoricalData { req_id, data: resp });
                            if is_complete && !self.keep_up_to_date_reqs.contains(&req_id) {
                                self.pending_historical.remove(pos);
                            }
                        }
                    }
                    else if let Some(resp) = crate::control::historical::parse_head_timestamp_response(xml_tag) {
                        if let Some(pos) = self.pending_head_ts.iter().position(|_| true) {
                            let (_, req_id) = self.pending_head_ts.remove(pos);
                            shared.reference.push_head_timestamp(req_id, resp.clone());
                            emit(event_tx, Event::HeadTimestamp { req_id, data: resp });
                        }
                    }
                    else if let Some(entries) = crate::control::histogram::parse_histogram_response(xml_tag) {
                        if let Some(pos) = self.pending_histogram.iter().position(|_| true) {
                            let (_, req_id) = self.pending_histogram.remove(pos);
                            shared.reference.push_histogram_data(req_id, entries);
                        }
                    }
                    else if xml_tag.contains("<ResultSetTick>") {
                        if let Some(pos) = self.pending_ticks.iter().position(|(qid, _, _)| xml_tag.contains(qid.as_str())) {
                            let (_, req_id, what_to_show) = self.pending_ticks.remove(pos);
                            if let Some((_, data, done)) = crate::control::historical::parse_tick_response(xml_tag, &what_to_show) {
                                shared.reference.push_historical_ticks(req_id, data, what_to_show, done);
                            }
                        }
                    }
                    else if let Some(resp) = crate::control::historical::parse_schedule_response(xml_tag) {
                        if let Some(pos) = self.pending_schedule.iter().position(|(qid, _)| *qid == resp.query_id) {
                            let (_, req_id) = self.pending_schedule.remove(pos);
                            shared.reference.push_historical_schedule(req_id, resp);
                        }
                    }
                    else if let Some(ticker_id_str) = crate::control::historical::parse_ticker_id(xml_tag) {
                        let min_tick = crate::control::historical::extract_xml_tag(xml_tag, "minTick")
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.01);
                        let ticker_id: u32 = ticker_id_str.parse().unwrap_or(0);
                        let mut matched = false;
                        for sub in &mut self.rtbar_subs {
                            if xml_tag.contains(&sub.0) {
                                sub.2 = Some(ticker_id);
                                sub.3 = min_tick;
                                log::info!("HMDS rtbar ticker_id={} min_tick={} for req_id={}", ticker_id, min_tick, sub.1);
                                matched = true;
                                break;
                            }
                        }
                        if !matched {
                            // Check keepUpToDate historical queries
                            for (qid, req_id) in &self.pending_historical {
                                if xml_tag.contains(qid.as_str()) && self.keep_up_to_date_reqs.contains(req_id) {
                                    // Store as rtbar subscription so 35=G bars get dispatched
                                    self.rtbar_subs.push((qid.clone(), *req_id, Some(ticker_id), min_tick));
                                    matched = true;
                                    break;
                                }
                            }
                        }
                        if !matched {
                            log::info!("HMDS TBT ticker_id assigned: {}", ticker_id_str);
                        }
                    }
                    else {
                        log::debug!("HMDS unmatched W response (len={}): {:?}", xml_tag.len(), xml_tag);
                    }
                }
            }
            "U" => {
                if let Some(comm) = parsed.get(&6040) {
                    match comm.as_str() {
                        "10002" => {
                            if let Some(xml) = parsed.get(&6118) {
                                self.pending_scanner_params = false;
                                shared.reference.push_scanner_params(xml.clone());
                            }
                        }
                        "10005" => {
                            if let Some(xml) = parsed.get(&6118) {
                                if let Some(result) = crate::control::scanner::parse_scanner_response(xml) {
                                    if let Some((_, req_id)) = self.pending_scanner.first() {
                                        let req_id = *req_id;
                                        shared.reference.push_scanner_data(req_id, result);
                                    }
                                }
                            }
                        }
                        "10032" => {
                            let raw_bytes = extract_raw_tag(msg, 96);
                            if let Some(xml) = parsed.get(&6118) {
                                let is_article = xml.contains("article_file");
                                if is_article {
                                    if let Some(pos) = self.pending_articles.iter().position(|_| true) {
                                        let (_, req_id) = self.pending_articles.remove(pos);
                                        if let Some(raw) = &raw_bytes {
                                            if let Some((atype, text)) = crate::control::news::parse_article_payload(raw) {
                                                shared.reference.push_news_article(req_id, atype, text);
                                            }
                                        }
                                    }
                                } else if let Some(pos) = self.pending_news.iter().position(|_| true) {
                                    let (_, req_id) = self.pending_news.remove(pos);
                                    if let Some(raw) = &raw_bytes {
                                        let (headlines, has_more) = crate::control::news::parse_news_payload(raw);
                                        shared.reference.push_historical_news(req_id, headlines, has_more);
                                    } else {
                                        shared.reference.push_historical_news(req_id, Vec::new(), false);
                                    }
                                }
                            }
                        }
                        "10012" => {
                            if let Some(xml) = parsed.get(&6118) {
                                let data = if let Some(raw) = parsed.get(&96) {
                                    crate::control::fundamental::decompress_fundamental_data(raw.as_bytes())
                                        .unwrap_or_else(|| raw.clone())
                                } else {
                                    xml.clone()
                                };
                                if let Some(pos) = self.pending_fundamental.iter().position(|_| true) {
                                    let (_, req_id) = self.pending_fundamental.remove(pos);
                                    shared.reference.push_fundamental_data(req_id, data);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "G" => self.handle_rtbar_data(msg, shared),
            _ => {}
        }
    }

    fn handle_tbt_data(&mut self, msg: &[u8], shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        let body = match find_body_after_tag(msg, b"35=E\x01") {
            Some(b) => b,
            None => return,
        };
        let entries = tick_decoder::decode_ticks_35e(body);
        for entry in &entries {
            let instrument = match self.tbt_subscriptions.first() {
                Some((id, _, _)) => *id,
                None => return,
            };
            match entry {
                tick_decoder::TbtEntry::Trade { timestamp, price_cents_delta, size, exchange, conditions } => {
                    let cents = self.update_tbt_price(instrument, *price_cents_delta, 0);
                    let price = cents * (PRICE_SCALE / 100);
                    let trade = crate::types::TbtTrade {
                        instrument,
                        price,
                        size: *size as i64,
                        timestamp: *timestamp,
                        exchange: exchange.clone(),
                        conditions: conditions.clone(),
                    };
                    shared.market.push_tbt_trade(trade.clone());
                    emit(event_tx, Event::TbtTrade(trade));
                }
                tick_decoder::TbtEntry::Quote { timestamp, bid_cents_delta, ask_cents_delta, bid_size, ask_size } => {
                    let (bid_cents, ask_cents) = self.update_tbt_bid_ask(instrument, *bid_cents_delta, *ask_cents_delta);
                    let quote = crate::types::TbtQuote {
                        instrument,
                        bid: bid_cents * (PRICE_SCALE / 100),
                        ask: ask_cents * (PRICE_SCALE / 100),
                        bid_size: *bid_size as i64,
                        ask_size: *ask_size as i64,
                        timestamp: *timestamp,
                    };
                    shared.market.push_tbt_quote(quote);
                    emit(event_tx, Event::TbtQuote(quote));
                }
            }
        }
    }

    fn handle_rtbar_data(&mut self, msg: &[u8], shared: &SharedState) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };
        let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
        let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
        if body.len() < 11 { return; }
        let ticker_id = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
        let timestamp = u32::from_be_bytes([body[6], body[7], body[8], body[9]]);
        let payload_len = body[10] as usize;
        if body.len() < 11 + payload_len { return; }
        let sub = self.rtbar_subs.iter().find(|(_, _, tid, _)| *tid == Some(ticker_id));
        let (req_id, min_tick) = match sub {
            Some((_, rid, _, mt)) => (*rid, *mt),
            None => return,
        };
        let payload = &body[11..11 + payload_len];
        if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
            bar.timestamp = timestamp;
            shared.market.push_real_time_bar(req_id, bar);
        }
    }

    #[inline]
    fn update_tbt_price(&mut self, instrument: InstrumentId, delta: i64, _: i64) -> i64 {
        let entry = &mut self.tbt_price_state[instrument as usize];
        entry.0 += delta;
        entry.0
    }

    #[inline]
    fn update_tbt_bid_ask(&mut self, instrument: InstrumentId, bid_delta: i64, ask_delta: i64) -> (i64, i64) {
        let entry = &mut self.tbt_price_state[instrument as usize];
        entry.1 += bid_delta;
        entry.2 += ask_delta;
        (entry.1, entry.2)
    }

    pub(crate) fn send_tbt_subscribe(
        &mut self,
        con_id: i64,
        instrument: InstrumentId,
        tbt_type: TbtType,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let req_id = self.next_tbt_req_id;
        self.next_tbt_req_id += 1;
        let tbt_type_str = match tbt_type {
            TbtType::Last => "AllLast",
            TbtType::BidAsk => "BidAsk",
        };
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListOfQueries>\
             <Query>\
             <id>tbt_{req_id}</id>\
             <contractID>{con_id}</contractID>\
             <exchange>BEST</exchange>\
             <secType>CS</secType>\
             <expired>no</expired>\
             <type>TickData</type>\
             <refresh>ticks</refresh>\
             <data>{tbt_type_str}</data>\
             <source>API</source>\
             </Query>\
             </ListOfQueries>"
        );
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent TBT subscribe: con_id={} type={} req_id={}", con_id, tbt_type_str, req_id);
            hb.last_hmds_sent = Instant::now();
        }
        let ticker_id = format!("tbt_{}", req_id);
        self.tbt_subscriptions.push((instrument, ticker_id, tbt_type));
    }

    pub(crate) fn send_tbt_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let idx = match self.tbt_subscriptions.iter().position(|(id, _, _)| *id == instrument) {
            Some(i) => i,
            None => return,
        };
        let (_, ticker_id, _) = self.tbt_subscriptions.remove(idx);
        if let Some(conn) = hmds_conn.as_mut() {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListOfCancelQueries>\
                 <CancelQuery>\
                 <id>ticker:{tid}</id>\
                 </CancelQuery>\
                 </ListOfCancelQueries>",
                tid = ticker_id,
            );
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "Z"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent TBT unsubscribe: instrument={} ticker_id={}", instrument, ticker_id);
            hb.last_hmds_sent = Instant::now();
        }
        self.tbt_price_state[instrument as usize] = (0, 0, 0);
    }

    pub(crate) fn send_historical_request(
        &mut self,
        req_id: u32,
        con_id: i64,
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        use_rth: bool,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        self.send_historical_request_ex(req_id, con_id, end_date_time, duration, bar_size, what_to_show, use_rth, false, "", hmds_conn, hb);
    }

    pub(crate) fn send_historical_request_ex(
        &mut self,
        req_id: u32,
        con_id: i64,
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        use_rth: bool,
        keep_up_to_date: bool,
        symbol: &str,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let duration = duration.to_lowercase();
        let duration = duration.as_str();
        let end_date_time = if end_date_time.is_empty() {
            crate::gateway::chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let end_date_time = end_date_time.as_str();
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;

        let data_type = match what_to_show.to_uppercase().as_str() {
            "MIDPOINT" => crate::control::historical::BarDataType::Midpoint,
            "BID" => crate::control::historical::BarDataType::Bid,
            "ASK" => crate::control::historical::BarDataType::Ask,
            "BID_ASK" => crate::control::historical::BarDataType::BidAsk,
            "ADJUSTED_LAST" => crate::control::historical::BarDataType::AdjustedLast,
            "HISTORICAL_VOLATILITY" => crate::control::historical::BarDataType::HistoricalVolatility,
            "OPTION_IMPLIED_VOLATILITY" => crate::control::historical::BarDataType::ImpliedVolatility,
            _ => crate::control::historical::BarDataType::Trades,
        };

        let bs = match bar_size {
            "1 secs" | "1 sec" => crate::control::historical::BarSize::Sec1,
            "5 secs" => crate::control::historical::BarSize::Sec5,
            "10 secs" => crate::control::historical::BarSize::Sec10,
            "15 secs" => crate::control::historical::BarSize::Sec15,
            "30 secs" => crate::control::historical::BarSize::Sec30,
            "1 min" => crate::control::historical::BarSize::Min1,
            "2 mins" => crate::control::historical::BarSize::Min2,
            "3 mins" => crate::control::historical::BarSize::Min3,
            "5 mins" => crate::control::historical::BarSize::Min5,
            "10 mins" => crate::control::historical::BarSize::Min10,
            "15 mins" => crate::control::historical::BarSize::Min15,
            "20 mins" => crate::control::historical::BarSize::Min20,
            "30 mins" => crate::control::historical::BarSize::Min30,
            "1 hour" => crate::control::historical::BarSize::Hour1,
            "2 hours" => crate::control::historical::BarSize::Hour2,
            "3 hours" => crate::control::historical::BarSize::Hour3,
            "4 hours" => crate::control::historical::BarSize::Hour4,
            "8 hours" => crate::control::historical::BarSize::Hour8,
            "1 day" => crate::control::historical::BarSize::Day1,
            "1 week" | "1W" => crate::control::historical::BarSize::Week1,
            "1 month" | "1M" => crate::control::historical::BarSize::Month1,
            _ => crate::control::historical::BarSize::Min5,
        };

        let query_id = format!("hist_{}", qid);
        let req = crate::control::historical::HistoricalRequest {
            query_id: query_id.clone(),
            con_id: con_id as u32,
            symbol: symbol.to_string(),
            sec_type: "CS",
            exchange: "SMART",
            data_type,
            end_time: end_date_time.to_string(),
            duration: duration.to_string(),
            bar_size: bs,
            use_rth,
            keep_up_to_date,
        };

        let xml = crate::control::historical::build_query_xml(&req);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent historical request: req_id={} con_id={} bar_size={}", req_id, con_id, bar_size);
            hb.last_hmds_sent = Instant::now();
        }
        self.pending_historical.push((query_id, req_id));
    }

    /// Send keepUpToDate historical request via CCP (FIXCOMP compressed).
    /// Responses arrive on HMDS, not CCP (cross-connection routing).
    pub(crate) fn send_historical_request_via_ccp(
        &mut self,
        req_id: u32,
        con_id: i64,
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        use_rth: bool,
        symbol: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        sign_key: &[u8],
        sign_iv: &std::sync::Mutex<Vec<u8>>,
    ) {
        // Reuse the same request builder but with keep_up_to_date=true
        let duration = duration.to_lowercase();
        let end_date_time = if end_date_time.is_empty() {
            crate::gateway::chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;

        let data_type = match what_to_show.to_uppercase().as_str() {
            "MIDPOINT" => crate::control::historical::BarDataType::Midpoint,
            "BID" => crate::control::historical::BarDataType::Bid,
            "ASK" => crate::control::historical::BarDataType::Ask,
            "BID_ASK" => crate::control::historical::BarDataType::BidAsk,
            "ADJUSTED_LAST" => crate::control::historical::BarDataType::AdjustedLast,
            "HISTORICAL_VOLATILITY" => crate::control::historical::BarDataType::HistoricalVolatility,
            "OPTION_IMPLIED_VOLATILITY" => crate::control::historical::BarDataType::ImpliedVolatility,
            _ => crate::control::historical::BarDataType::Trades,
        };

        let bs = match bar_size {
            "1 secs" | "1 sec" => crate::control::historical::BarSize::Sec1,
            "5 secs" => crate::control::historical::BarSize::Sec5,
            "5 mins" => crate::control::historical::BarSize::Min5,
            "1 hour" => crate::control::historical::BarSize::Hour1,
            "1 day" => crate::control::historical::BarSize::Day1,
            _ => crate::control::historical::BarSize::Min5,
        };

        let query_id = format!("hist_{}", qid);
        let req = crate::control::historical::HistoricalRequest {
            query_id: query_id.clone(),
            con_id: con_id as u32,
            symbol: symbol.to_string(),
            sec_type: "CS",
            exchange: "SMART",
            data_type,
            end_time: end_date_time,
            duration: duration.to_string(),
            bar_size: bs,
            use_rth,
            keep_up_to_date: true,
        };

        let xml = crate::control::historical::build_query_xml(&req);
        if let Some(conn) = ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();
            // FIXCOMP compress + selective HMAC 8349 signing
            let raw = fix::fix_build(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ], 0);
            let compressed = fixcomp::fixcomp_build(&raw);
            let to_send = if !sign_key.is_empty() {
                let mut iv_guard = sign_iv.lock().unwrap();
                let (signed, new_iv) = fix::fix_sign(&compressed, sign_key, &iv_guard);
                *iv_guard = new_iv;
                signed
            } else {
                compressed
            };
            let _ = conn.send_raw(&to_send);
            hb.last_ccp_sent = Instant::now();
        }
        self.pending_historical.push((query_id, req_id));
    }

    pub(crate) fn send_historical_cancel(&mut self, query_id: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = hmds_conn.as_mut() {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListOfCancelQueries>\
                 <CancelQuery>\
                 <id>ticker:{tid}</id>\
                 </CancelQuery>\
                 </ListOfCancelQueries>",
                tid = query_id,
            );
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "Z"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
        }
    }

    pub(crate) fn send_head_timestamp_request(&mut self, req_id: u32, con_id: i64, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let data_type = match what_to_show.to_uppercase().as_str() {
            "MIDPOINT" => crate::control::historical::BarDataType::Midpoint,
            "BID" => crate::control::historical::BarDataType::Bid,
            "ASK" => crate::control::historical::BarDataType::Ask,
            _ => crate::control::historical::BarDataType::Trades,
        };
        let req = crate::control::historical::HeadTimestampRequest {
            con_id: con_id as u32,
            sec_type: "CS",
            exchange: "SMART",
            data_type,
            use_rth,
        };
        let xml = crate::control::historical::build_head_timestamp_xml(&req);
        let query_id = format!("hts_{}", self.next_hmds_query_id);
        self.next_hmds_query_id += 1;
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            log::info!("Sent head timestamp request: req_id={} con_id={}", req_id, con_id);
            hb.last_hmds_sent = Instant::now();
        }
        self.pending_head_ts.push((query_id, req_id));
    }

    pub(crate) fn send_scanner_params_request(&mut self, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::scanner::TAG_SUB_PROTOCOL, "10001"),
            ]);
            self.pending_scanner_params = true;
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent scanner params request");
        }
    }

    pub(crate) fn send_scanner_subscribe(&mut self, req_id: u32, instrument: &str, location_code: &str, scan_code: &str, max_items: u32, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let sub = crate::control::scanner::ScannerSubscription {
            instrument: instrument.to_string(),
            location_code: location_code.to_string(),
            scan_code: scan_code.to_string(),
            max_items,
        };
        let scan_id = format!("APISCAN{}:{}", self.next_scanner_id, req_id);
        self.next_scanner_id += 1;
        let xml = crate::control::scanner::build_scanner_subscribe_xml(&sub, &scan_id);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10003"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent scanner subscribe: req_id={} scan_code={}", req_id, scan_code);
        }
        self.pending_scanner.push((scan_id, req_id));
    }

    pub(crate) fn send_scanner_cancel(&mut self, scan_id: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let xml = crate::control::scanner::build_scanner_cancel_xml(scan_id);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10004"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent scanner cancel: scan_id={}", scan_id);
        }
    }

    pub(crate) fn send_historical_news_request(&mut self, req_id: u32, con_id: u32, provider_codes: &str, start_time: &str, end_time: &str, max_results: u32, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let query_id = format!("news_{}", self.next_hmds_query_id);
        let req = crate::control::news::HistoricalNewsRequest {
            query_id: query_id.clone(),
            con_id,
            provider_codes: provider_codes.to_string(),
            start_time: start_time.to_string(),
            end_time: end_time.to_string(),
            max_results,
        };
        let xml = crate::control::news::build_historical_news_xml(&req);
        self.next_hmds_query_id += 1;
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10030"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent historical news request: req_id={} con_id={}", req_id, con_id);
        }
        self.pending_news.push((query_id, req_id));
    }

    pub(crate) fn send_news_article_request(&mut self, req_id: u32, provider_code: &str, article_id: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let query_id = format!("art_{}", self.next_hmds_query_id);
        let req = crate::control::news::NewsArticleRequest {
            query_id: query_id.clone(),
            provider_code: provider_code.to_string(),
            article_id: article_id.to_string(),
        };
        let xml = crate::control::news::build_article_request_xml(&req);
        self.next_hmds_query_id += 1;
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10030"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent news article request: req_id={} article={}", req_id, article_id);
        }
        self.pending_articles.push((query_id, req_id));
    }

    pub(crate) fn send_fundamental_data_request(&mut self, req_id: u32, con_id: u32, report_type: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let rt = match report_type {
            "ReportSnapshot" | "snapshot" => crate::control::fundamental::ReportType::Snapshot,
            "ReportFinSummary" | "finsum" => crate::control::fundamental::ReportType::FinancialSummary,
            "ReportsFinStatements" | "finstat" => crate::control::fundamental::ReportType::FinancialStatements,
            _ => crate::control::fundamental::ReportType::Snapshot,
        };
        let req = crate::control::fundamental::FundamentalRequest {
            con_id,
            sec_type: "STK",
            currency: "USD",
            report_type: rt,
        };
        let xml = crate::control::fundamental::build_fundamental_request_xml(&req);
        let query_id = format!("fund_{}", self.next_hmds_query_id);
        self.next_hmds_query_id += 1;
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "10010"),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent fundamental data request: req_id={} con_id={}", req_id, con_id);
        }
        self.pending_fundamental.push((query_id, req_id));
    }

    pub(crate) fn send_histogram_request(&mut self, req_id: u32, con_id: u32, use_rth: bool, period: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let req = crate::control::histogram::HistogramRequest {
            con_id,
            use_rth,
            period: period.to_string(),
            end_time: chrono_free_timestamp().to_string(),
        };
        let xml = crate::control::histogram::build_histogram_request_xml(&req);
        let query_id = format!("hg_{}", self.next_hmds_query_id);
        self.next_hmds_query_id += 1;
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent histogram request: req_id={} con_id={}", req_id, con_id);
        }
        self.pending_histogram.push((query_id, req_id));
    }

    pub(crate) fn send_historical_ticks_request(&mut self, req_id: u32, con_id: i64, start_date_time: &str, end_date_time: &str, number_of_ticks: u32, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let query_id = format!("tk_{}", qid);
        let xml = crate::control::historical::build_tick_query_xml(
            &query_id, con_id, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth,
        );
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent historical ticks request: req_id={} con_id={} what={}", req_id, con_id, what_to_show);
        }
        self.pending_ticks.push((query_id, req_id, what_to_show.to_string()));
    }

    pub(crate) fn send_realtime_bar_subscribe(&mut self, req_id: u32, con_id: i64, _symbol: &str, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let query_id = format!("rt_{}", qid);
        let xml = crate::control::historical::build_realtime_bar_xml(&query_id, con_id, what_to_show, use_rth);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent rtbar subscribe: req_id={} con_id={} what={}", req_id, con_id, what_to_show);
        }
        self.rtbar_subs.push((query_id, req_id, None, 0.01));
    }

    pub(crate) fn send_schedule_request(&mut self, req_id: u32, con_id: i64, end_date_time: &str, duration: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let duration = duration.to_lowercase();
        let end_date_time = if end_date_time.is_empty() {
            chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let query_id = format!("sched_{}", qid);
        let xml = crate::control::historical::build_schedule_xml(&query_id, con_id, &end_date_time, &duration, use_rth);
        if let Some(conn) = hmds_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "W"),
                (fix::TAG_SENDING_TIME, &ts),
                (6118, &xml),
            ]);
            hb.last_hmds_sent = Instant::now();
            log::info!("Sent schedule request: req_id={} con_id={}", req_id, con_id);
        }
        self.pending_schedule.push((query_id, req_id));
    }
}
