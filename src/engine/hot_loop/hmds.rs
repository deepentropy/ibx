use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{InstrumentId, TbtType, PRICE_SCALE, MAX_INSTRUMENTS};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, clone_for_event, find_body_after_tag, extract_raw_tag};

/// Idle bound for an in-flight historical query: if no bar segment, error,
/// or completion arrives for this long, the request is failed with error 162
/// and a terminal sentinel instead of hanging forever (ibx#231). The
/// gateway's pacing limiter drops requests silently, which is otherwise
/// indistinguishable from a permanent hang.
const HISTORICAL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) struct HmdsState {
    pub(crate) next_tbt_req_id: u32,
    pub(crate) tbt_subscriptions: Vec<(InstrumentId, String, TbtType)>,
    pub(crate) tbt_price_state: [(i64, i64, i64); MAX_INSTRUMENTS],
    pub(crate) next_hmds_query_id: u32,
    pub(crate) disconnected: bool,
    /// In-flight historical bar queries: (query_id, req_id, idle deadline).
    /// The deadline is refreshed on every matched bar segment and swept by
    /// `sweep_pending_historical` — a gateway that goes silent (e.g. the
    /// pacing limiter tripping) no longer hangs the request forever
    /// (ibx#231). keepUpToDate entries are exempt: they stay resident by
    /// design and their bars flow on a different path.
    pub(crate) pending_historical: Vec<(String, u32, Instant)>,
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
    /// Scanner results parked for contract-detail enrichment before dispatch.
    /// Drained by the engine top-level after each hmds.poll, then handed to
    /// `CcpState::start_scanner_enrichment`.
    pub(crate) cold_scanner_results: Vec<(u32, crate::control::scanner::ScannerResult)>,
}

/// Wire security type for a historical query. Empty falls back to the stock
/// encoding, which is what every caller got unconditionally before (ibx#305).
fn hist_sec_type(sec_type: &str) -> String {
    if sec_type.is_empty() {
        return "CS".to_string();
    }
    crate::control::contracts::SecurityType::from_fix(sec_type).to_fix().to_string()
}

/// Exchange for a historical query, defaulting to the previous constant.
fn hist_exchange(exchange: &str) -> String {
    if exchange.is_empty() { "SMART".to_string() } else { exchange.to_string() }
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
            cold_scanner_results: Vec::new(),
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
                // ibx#183 follow-up: frame-extraction tracer. If a recv produces
                // 0 frames AND buffered_after > 0, the bytes are stuck waiting for
                // more (incomplete FIXCOMP frame — declared tag-9 length exceeds
                // received bytes). If buffered_after == 0, the bytes were dropped
                // outright (no recognized header — buf.clear() path).
                log::warn!(
                    "HMDS poll: extracted={} frames, buffered_after={}B",
                    frames.len(),
                    conn.buffered(),
                );
                let mut msgs = Vec::new();
                for frame in &frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            match fixcomp::fixcomp_decompress(&unsigned) {
                                Ok(inner) => {
                                    if log::log_enabled!(log::Level::Trace) {
                                        for m in &inner {
                                            log::trace!("WIRE< hmds/comp {}", crate::protocol::fix::fmt_pipe(m));
                                        }
                                    }
                                    msgs.extend(inner);
                                }
                                Err(e) => {
                                    log::warn!(
                                        "HMDS: dropping malformed FIXCOMP frame ({} bytes): {}",
                                        unsigned.len(), e,
                                    );
                                }
                            }
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< hmds/bin {}", crate::protocol::fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _valid) = conn.unsign(raw);
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< hmds/fix {}", crate::protocol::fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Control(_) => {
                            // 8=1 / 8=X control state — not consumed on the data path (ibx#185).
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
                    // Per-frame XML root tracer (kept at debug: fires on every
                    // W/6118 payload). Unmatched payloads still warn below.
                    log::debug!(
                        "HMDS W xml head (len={}): {:?}",
                        xml_tag.len(),
                        &xml_tag[..xml_tag.len().min(200)],
                    );
                    if let Some(resp) = crate::control::historical::parse_bar_response(xml_tag) {
                        if let Some(pos) = self.pending_historical.iter().position(|(qid, _, _)| resp.query_id.starts_with(qid.as_str())) {
                            let (_, req_id, _) = self.pending_historical[pos];
                            let is_complete = resp.is_complete;
                            // Activity on this query — push the idle deadline out (ibx#231).
                            self.pending_historical[pos].2 = Instant::now() + HISTORICAL_IDLE_TIMEOUT;
                            // Bar completion rides <eoq>true> in the final segmented
                            // ResultSetBar; earlier segments carry <eoq>false>
                            // (ib-agent#169). Kept at debug: fires per bar batch.
                            log::debug!(
                                "HMDS W matched: req_id={} query_id={:?} eoq={} bars={}",
                                req_id, resp.query_id, is_complete, resp.bars.len()
                            );
                            // Clone only when someone is listening on the event
                            // channel — a bar batch is a deep copy (ibx#242).
                            let for_event = clone_for_event(event_tx, &resp);
                            shared.reference.push_historical_data(req_id, resp);
                            if let Some(data) = for_event {
                                emit(event_tx, Event::HistoricalData { req_id, data });
                            }
                            if is_complete && !self.keep_up_to_date_reqs.contains(&req_id) {
                                self.pending_historical.remove(pos);
                            }
                        } else {
                            // ibx#182 follow-up: diagnostic bisect — when parse_bar_response
                            // returns Some but the query_id doesn't match any in-flight
                            // pending_historical, the response is silently dropped.
                            log::warn!(
                                "HMDS W parsed but no pending_historical match: resp.query_id={:?} eoq={} bars={} pending={:?}",
                                resp.query_id, resp.is_complete, resp.bars.len(), self.pending_historical
                            );
                        }
                    }
                    else if let Some(resp) = crate::control::historical::parse_head_timestamp_response(xml_tag) {
                        if let Some(pos) = self.pending_head_ts.iter().position(|_| true) {
                            let (_, req_id) = self.pending_head_ts.remove(pos);
                            let for_event = clone_for_event(event_tx, &resp);
                            shared.reference.push_head_timestamp(req_id, resp);
                            if let Some(data) = for_event {
                                emit(event_tx, Event::HeadTimestamp { req_id, data });
                            }
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
                            for (qid, req_id, _) in &self.pending_historical {
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
                    else if xml_tag.contains("<QueryError>") {
                        // ibx#186: gateway rejected the query (e.g. "Invalid time length").
                        // Without this branch the pending entry leaks forever and the
                        // consumer sees no completion or error event.
                        let query_id = crate::control::historical::extract_xml_tag(xml_tag, "id")
                            .map(|s| s.to_string());
                        let error_msg = crate::control::historical::extract_xml_tag(xml_tag, "error")
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        // IB canonical error code for HMDS-side validation/rejection.
                        const HMDS_ERROR_CODE: i32 = 162;
                        let mut released_req_id: Option<u32> = None;
                        let mut from_historical = false;
                        if let Some(qid) = &query_id {
                            if let Some(pos) = self.pending_historical.iter().position(|(q, _, _)| q == qid) {
                                let (_, req_id, _) = self.pending_historical.remove(pos);
                                self.keep_up_to_date_reqs.remove(&req_id);
                                released_req_id = Some(req_id);
                                from_historical = true;
                            } else if let Some(pos) = self.pending_head_ts.iter().position(|(q, _)| q == qid) {
                                let (_, req_id) = self.pending_head_ts.remove(pos);
                                released_req_id = Some(req_id);
                            } else if let Some(pos) = self.pending_histogram.iter().position(|(q, _)| q == qid) {
                                let (_, req_id) = self.pending_histogram.remove(pos);
                                released_req_id = Some(req_id);
                            } else if let Some(pos) = self.pending_ticks.iter().position(|(q, _, _)| q == qid) {
                                let (_, req_id, _) = self.pending_ticks.remove(pos);
                                released_req_id = Some(req_id);
                            } else if let Some(pos) = self.pending_schedule.iter().position(|(q, _)| q == qid) {
                                let (_, req_id) = self.pending_schedule.remove(pos);
                                released_req_id = Some(req_id);
                            } else if let Some(pos) = self.pending_scanner.iter().position(|(q, _)| q == qid) {
                                let (_, req_id) = self.pending_scanner.remove(pos);
                                released_req_id = Some(req_id);
                            }
                        }
                        match released_req_id {
                            Some(req_id) => {
                                log::warn!(
                                    "HMDS QueryError req_id={} query_id={:?}: {}",
                                    req_id, query_id, error_msg
                                );
                                shared.reference.push_historical_error(req_id, HMDS_ERROR_CODE, error_msg.clone());
                                // Surface a terminal sentinel for historical-bar consumers
                                // that wait on historical_data_end. Empty response with
                                // is_complete=true unblocks the existing dispatch path.
                                if from_historical {
                                    shared.reference.push_historical_data(
                                        req_id,
                                        crate::control::historical::HistoricalResponse {
                                            query_id: query_id.clone().unwrap_or_default(),
                                            timezone: String::new(),
                                            is_complete: true,
                                            bars: Vec::new(),
                                        },
                                    );
                                }
                            }
                            None => {
                                log::warn!(
                                    "HMDS QueryError for unknown query_id={:?}: {}",
                                    query_id, error_msg
                                );
                            }
                        }
                    }
                    else {
                        // ibx#182 follow-up: bumped from debug to warn so silent
                        // drops in the W cascade surface at Info-level apps.
                        log::warn!("HMDS unmatched W response (len={}): {:?}", xml_tag.len(), xml_tag);
                    }
                } else {
                    // ibx#183 follow-up: W message with no 6118 payload — fourth
                    // silent-drop path missed in the original cascade audit.
                    log::warn!("HMDS W with no tag 6118 (msg_len={})", msg.len());
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
                                        // ScanResponse only carries con_ids; contract metadata must be
                                        // resolved via 35=c on CCP. Park results with cache-miss con_ids
                                        // for the engine to enrich before dispatch (see ibx#156, ib-agent#142).
                                        let any_cold = result.entries.iter().any(|e| {
                                            e.con_id != 0
                                                && shared.reference.get_contract(e.con_id as i64).is_none()
                                        });
                                        if any_cold {
                                            self.cold_scanner_results.push((req_id, result));
                                        } else {
                                            shared.reference.push_scanner_data(req_id, result);
                                        }
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
                        "10022" => {
                            // ConAdjResponse: corporate-actions / dividend history,
                            // pushed once per contract per session on the first
                            // historical request (any bar size). Not a bar frame and
                            // not a completion sentinel — bar completion rides
                            // <eoq>true> in the ResultSetBar. Recognized and skipped
                            // (ib-agent#169, ibx#183).
                        }
                        _ => {}
                    }
                }
            }
            "G" => self.handle_rtbar_data(msg, shared),
            other => {
                // ibx#183 follow-up: was a silent _ => {} arm — log unhandled
                // msg_types so we can catch frames that bypass the W cascade
                // entirely (e.g. completion sentinels delivered as a different type).
                log::warn!("HMDS unhandled msg_type={:?} (msg_len={})", other, msg.len());
            }
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
        sec_type: &str,
        exchange: &str,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
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

        // One shared table, rejection instead of a silent Min5/TRADES
        // fallback (ibx#232). The client validates synchronously before the
        // command is sent; this is the engine-side backstop for raw
        // control-channel callers.
        let data_type = match crate::control::historical::BarDataType::from_api_str(what_to_show) {
            Ok(dt) => dt,
            Err(e) => {
                log::error!("historical req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return;
            }
        };
        let bs = match crate::control::historical::BarSize::from_api_str(bar_size) {
            Ok(bs) => bs,
            Err(e) => {
                log::error!("historical req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return;
            }
        };

        let query_id = format!("hist_{}", qid);
        let req = crate::control::historical::HistoricalRequest {
            query_id: query_id.clone(),
            con_id: con_id as u32,
            symbol: symbol.to_string(),
            sec_type: hist_sec_type(sec_type),
            exchange: hist_exchange(exchange),
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
        self.pending_historical.push((query_id, req_id, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
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
        sec_type: &str,
        exchange: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        sign_key: &[u8],
        sign_iv: &std::sync::Mutex<Vec<u8>>,
        shared: &SharedState,
    ) -> bool {
        // Reuse the same request builder but with keep_up_to_date=true
        let duration = duration.to_lowercase();
        let end_date_time = if end_date_time.is_empty() {
            crate::gateway::chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;

        // Same shared table as the batch path — the second, five-entry copy
        // silently downgraded "1 min" (and 16 other sizes) to Min5 on this
        // path only (ibx#232). Unsupported streaming sizes reject loudly.
        let data_type = match crate::control::historical::BarDataType::from_api_str(what_to_show) {
            Ok(dt) => dt,
            Err(e) => {
                log::error!("keepUpToDate req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return false;
            }
        };
        let bs = match crate::control::historical::BarSize::from_api_str(bar_size) {
            Ok(bs) if bs.supports_keep_up_to_date() => bs,
            Ok(_) => {
                let e = format!(
                    "bar_size '{}' is not supported with keep_up_to_date=true: \
                     supported sizes are 1 secs, 5 secs, 5 mins, 1 hour, 1 day",
                    bar_size,
                );
                log::error!("keepUpToDate req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return false;
            }
            Err(e) => {
                log::error!("keepUpToDate req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return false;
            }
        };

        let query_id = format!("hist_{}", qid);
        let req = crate::control::historical::HistoricalRequest {
            query_id: query_id.clone(),
            con_id: con_id as u32,
            symbol: symbol.to_string(),
            sec_type: hist_sec_type(sec_type),
            exchange: hist_exchange(exchange),
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
            // Debug: dump first 80 bytes hex for wire comparison
            let hex: String = to_send.iter().take(80).map(|b| format!("{:02x}", b)).collect();
            log::info!("CCP keepUpToDate 35=W: {} bytes, hex={}", to_send.len(), hex);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("D:/RustroverProjects/ibx/ibx_kut_debug.log") {
                use std::io::Write;
                let full_hex: String = to_send.iter().map(|b| format!("{:02x}", b)).collect();
                let _ = writeln!(f, "LEN={} HEX={}", to_send.len(), full_hex);
            }
            let _ = conn.send_raw(&to_send);
            hb.last_ccp_sent = Instant::now();
        }
        self.pending_historical.push((query_id, req_id, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
        true
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

    pub(crate) fn send_head_timestamp_request(&mut self, req_id: u32, con_id: i64, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // Same shared table as the bar paths — this was a third divergent
        // copy with a silent TRADES fallback (ibx#232).
        let data_type = match crate::control::historical::BarDataType::from_api_str(what_to_show) {
            Ok(dt) => dt,
            Err(e) => {
                log::error!("head timestamp req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, false);
                return;
            }
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

    /// Fail historical queries whose idle deadline has passed (ibx#231).
    /// Mirrors the `<QueryError>` path: surfaces error 162 plus a terminal
    /// `is_complete=true` sentinel so a consumer blocked on
    /// historical_data_end unblocks with no API change. keepUpToDate
    /// subscriptions are exempt — they stay resident by design and their
    /// live bars flow on the rtbar path.
    pub(crate) fn sweep_pending_historical(&mut self, shared: &SharedState) {
        if self.pending_historical.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<(String, u32)> = Vec::new();
        let kut = &self.keep_up_to_date_reqs;
        self.pending_historical.retain(|(qid, req_id, deadline)| {
            if now >= *deadline && !kut.contains(req_id) {
                expired.push((qid.clone(), *req_id));
                false
            } else {
                true
            }
        });
        for (query_id, req_id) in expired {
            log::warn!(
                "HMDS historical timeout: req_id={} query_id={} — no response within {:?}",
                req_id, query_id, HISTORICAL_IDLE_TIMEOUT,
            );
            shared.reference.push_historical_error(
                req_id, 162,
                "historical request timed out — no response from the gateway".to_string(),
            );
            shared.reference.push_historical_data(
                req_id,
                crate::control::historical::HistoricalResponse {
                    query_id,
                    timezone: String::new(),
                    is_complete: true,
                    bars: Vec::new(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_query_error_msg(query_id: &str, error: &str) -> Vec<u8> {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<QueryError>\n\t<id>{}</id>\n\t<error>{}</error>\n</QueryError>\n",
            query_id, error,
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    }

    fn make_bar_msg(query_id: &str, eoq: bool) -> Vec<u8> {
        let xml = format!(
            "<ResultSetBar><id>{}</id><eoq>{}</eoq><tz>UTC</tz><Events>\
             <Bar><time>20260714-13:30:00</time><open>100.0</open><close>100.5</close>\
             <high>100.7</high><low>99.9</low><weightedAvg>100.2</weightedAvg>\
             <volume>1000</volume><count>10</count></Bar></Events></ResultSetBar>",
            query_id, if eoq { "true" } else { "false" },
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    }

    #[test]
    fn segmented_bar_reply_completes_on_eoq_true() {
        // ibx#183 / ib-agent#169: a segmented bar reply carries <eoq>false> on
        // early frames and <eoq>true> on the final one. The pending entry must
        // persist through the false frames and be released on the true frame.
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("q7".to_string(), 21, Instant::now() + HISTORICAL_IDLE_TIMEOUT));

        hmds.process_hmds_message(&make_bar_msg("q7", false), &mut conn, &shared, &None, &mut hb);
        assert_eq!(hmds.pending_historical.len(), 1, "entry must persist through eoq=false");

        hmds.process_hmds_message(&make_bar_msg("q7", true), &mut conn, &shared, &None, &mut hb);
        assert!(hmds.pending_historical.is_empty(), "eoq=true must release the pending entry");

        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 2);
        assert!(!hist[0].1.is_complete, "first segment incomplete");
        assert!(hist[1].1.is_complete, "final segment complete");
    }

    #[test]
    fn conadj_response_frame_is_skipped_without_disturbing_pending() {
        // ibx#183 / ib-agent#169: the 6040=10022 ConAdjResponse (corporate
        // actions) is pushed once per contract on the first historical request.
        // It must be recognized and skipped, not treated as bar or completion.
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("q8".to_string(), 22, Instant::now() + HISTORICAL_IDLE_TIMEOUT));

        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10022\x016118=");
        msg.extend_from_slice(b"<ConAdjResponse><id>ContractAdjustment1</id></ConAdjResponse>");
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert_eq!(hmds.pending_historical.len(), 1, "ConAdjResponse must not touch pending historical");
        assert!(shared.reference.drain_historical_data().is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
    }

    #[test]
    fn query_error_releases_historical_and_emits_error_and_end_sentinel() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("hist_1003".to_string(), 11, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
        hmds.keep_up_to_date_reqs.insert(11);

        let msg = make_query_error_msg("hist_1003", "Invalid time length");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_historical.is_empty(), "pending entry should be drained");
        assert!(!hmds.keep_up_to_date_reqs.contains(&11), "kut flag should be cleared");

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 11);
        assert_eq!(errors[0].1, 162);
        assert_eq!(errors[0].2, "Invalid time length");

        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel must be queued for historical req");
        assert_eq!(hist[0].0, 11);
        assert!(hist[0].1.is_complete);
        assert!(hist[0].1.bars.is_empty());
    }

    #[test]
    fn query_error_releases_head_timestamp_without_sentinel() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_head_ts.push(("hts_1004".to_string(), 42));

        let msg = make_query_error_msg("hts_1004", "No head timestamp");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_head_ts.is_empty());
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, vec![(42, 162, "No head timestamp".to_string())]);
        // Head-ts is not a bar request — no historical_data sentinel should fire.
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    // ── ibx#232: unknown bar_size rejects at the engine too (backstop for
    // raw control-channel callers; the client validates synchronously) ──

    #[test]
    fn engine_rejects_unknown_bar_size_with_error_and_sentinel() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;

        hmds.send_historical_request_ex(9, 756733, "", "2 d", "1 Min", "TRADES",
            true, false, "SPY", "STK", "SMART", &mut conn, &mut hb, &shared);

        assert!(hmds.pending_historical.is_empty(), "rejected request must not go pending");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1, 162);
        assert!(errors[0].2.contains("bar_size"), "got: {}", errors[0].2);
        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel must unblock waiters");
        assert!(hist[0].1.is_complete);
    }

    // ── ibx#231: idle-deadline sweep ──

    #[test]
    fn sweep_times_out_idle_historical_with_error_and_end_sentinel() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        // Deadline already in the past — the gateway went silent.
        hmds.pending_historical.push(("hist_1010".to_string(), 21, Instant::now() - std::time::Duration::from_secs(1)));

        hmds.sweep_pending_historical(&shared);

        assert!(hmds.pending_historical.is_empty(), "expired entry must be reclaimed");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 21);
        assert_eq!(errors[0].1, 162);
        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "terminal sentinel must unblock historical_data_end waiters");
        assert_eq!(hist[0].0, 21);
        assert!(hist[0].1.is_complete);
        assert!(hist[0].1.bars.is_empty());
    }

    #[test]
    fn sweep_spares_keep_up_to_date_and_live_entries() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        // keepUpToDate entry: resident by design, even past its deadline.
        hmds.pending_historical.push(("hist_kut".to_string(), 30, Instant::now() - std::time::Duration::from_secs(1)));
        hmds.keep_up_to_date_reqs.insert(30);
        // Live entry: deadline in the future.
        hmds.pending_historical.push(("hist_live".to_string(), 31, Instant::now() + HISTORICAL_IDLE_TIMEOUT));

        hmds.sweep_pending_historical(&shared);

        assert_eq!(hmds.pending_historical.len(), 2, "neither entry may be swept");
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    #[test]
    fn query_error_for_unknown_query_id_drops_nothing_and_emits_no_error() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("hist_1003".to_string(), 11, Instant::now() + HISTORICAL_IDLE_TIMEOUT));

        let msg = make_query_error_msg("hist_9999", "Boom");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert_eq!(hmds.pending_historical.len(), 1, "unrelated entry must stay");
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_historical_data().is_empty());
    }
}
