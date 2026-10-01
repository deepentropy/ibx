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

/// A head timestamp query with no answer for this long fails with
/// "Request Timed Out", as the reference (ibx#428).
const HEAD_TIMESTAMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Error text of a server rejection of a historical ticks request (10187).
const HISTORICAL_TICKS_ERROR: &str = "Failed to request historical ticks";

/// Error text of a server rejection of a histogram request (10188).
const HISTOGRAM_ERROR: &str = "Failed to request histogram data";

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
    /// In-flight head timestamp queries: (window id, req_id, deadline).
    pub(crate) pending_head_ts: Vec<(String, u32, Instant)>,
    /// Running numbers of the window ids of head timestamp, histogram and
    /// fundamentals queries (ibx#428).
    pub(crate) next_head_ts_window: u32,
    pub(crate) next_histogram_window: u32,
    pub(crate) next_fundamental_window: u32,
    pub(crate) pending_scanner_params: bool,
    pub(crate) pending_scanner: Vec<(String, u32)>,
    pub(crate) next_scanner_id: u32,
    pub(crate) pending_news: Vec<(String, u32)>,
    pub(crate) pending_articles: Vec<(String, u32)>,
    pub(crate) pending_fundamental: Vec<(String, u32)>,
    /// In-flight histogram queries, summed over their frames until the
    /// last one (ibx#433).
    pub(crate) pending_histogram: Vec<PendingHistogram>,
    pub(crate) pending_schedule: Vec<(String, u32)>,
    pub(crate) pending_ticks: Vec<(String, u32, String)>,
    /// Real-time bar subscriptions, and the keepUpToDate queries whose
    /// ticker id came: 5-second bars are routed by ticker id.
    pub(crate) rtbar_subs: Vec<RtBarSub>,
    /// Most real-time bar requests at once (ibx#454), from the logon.
    pub(crate) max_real_time_requests: u32,
    /// req_ids that should keep streaming after initial batch (keepUpToDate=True).
    pub(crate) keep_up_to_date_reqs: std::collections::HashSet<u32>,
    /// Bar requests answered from more than one server query (BID_ASK is a
    /// Bid query plus an Ask query, ibx#408). Each leg also has its own
    /// `pending_historical` entry; the bars of a leg are held here instead of
    /// being delivered, until every leg has finished.
    pub(crate) multi_leg: Vec<MultiLegBars>,
    /// Scanner results parked for contract-detail enrichment before dispatch.
    /// Drained by the engine top-level after each hmds.poll, then handed to
    /// `CcpState::start_scanner_enrichment`.
    pub(crate) cold_scanner_results: Vec<(u32, crate::control::scanner::ScannerResult)>,
}

/// A stream of 5-second bars (ibx#454).
#[derive(Debug, Clone)]
pub(crate) struct RtBarSub {
    /// Window id of the query.
    pub(crate) query_id: String,
    pub(crate) req_id: u32,
    /// Ticker id given by the server's acknowledgement.
    pub(crate) ticker_id: Option<u32>,
    pub(crate) min_tick: f64,
    /// A keepUpToDate bar query, not a real-time bar request.
    pub(crate) keep_up_to_date: bool,
}

/// Most real-time bar requests when the logon gives no limit, as the
/// reference (ibx#454).
pub(crate) const DEFAULT_MAX_REAL_TIME_REQUESTS: u32 = 40;

/// Error text of a rejected real-time bar query (420).
const INVALID_REAL_TIME_QUERY: &str = "Invalid Real-time Query";

/// A histogram request in flight (ibx#428, ibx#433).
#[derive(Debug)]
pub(crate) struct PendingHistogram {
    pub(crate) window_id: String,
    pub(crate) req_id: u32,
    /// Idle deadline, pushed out by every frame.
    pub(crate) deadline: Instant,
    pub(crate) sum: crate::control::histogram::HistogramSum,
}

/// State of one leg of a multi-query bar request (ibx#408).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegState {
    Pending,
    Done,
    Failed,
}

/// One leg of a multi-query bar request: its query id and its state.
#[derive(Debug)]
pub(crate) struct BarLeg {
    pub(crate) query_id: String,
    pub(crate) data_type: crate::control::historical::BarDataType,
    pub(crate) state: LegState,
}

/// A bar request answered from several server queries (BID_ASK, ibx#408).
#[derive(Debug)]
pub(crate) struct MultiLegBars {
    pub(crate) req_id: u32,
    pub(crate) legs: Vec<BarLeg>,
    /// Bar frames of every leg, in arrival order: the combined bars are
    /// built from them once no leg is pending.
    pub(crate) frames: Vec<(crate::control::historical::BarDataType, Vec<crate::control::historical::LegBar>)>,
    /// Time zone of the replies.
    pub(crate) timezone: String,
    /// Chart name of the request, for the no-data error.
    pub(crate) name: String,
}

/// Error text of a server-side rejection of a historical bar query, as the
/// official API reports it with code 162 (ibx#408).
fn historical_service_error(server_text: &str) -> String {
    crate::control::historical::join_error_text("Historical Market Data Service error message", server_text)
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
            next_head_ts_window: 1,
            next_histogram_window: 0,
            next_fundamental_window: 1,
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
            max_real_time_requests: DEFAULT_MAX_REAL_TIME_REQUESTS,
            keep_up_to_date_reqs: std::collections::HashSet::new(),
            multi_leg: Vec::new(),
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
        let (messages, bad_signature) = match hmds_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {}
                    Err(e) => {
                        log::error!("HMDS connection lost: {}", e);
                        self.disconnected = true;
                        // Drop the dead socket so the HMDS reconnect loop,
                        // which only runs with no connection held, re-dials
                        // it (ibx#399).
                        *hmds_conn = None;
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
                let mut bad_signature = false;
                for frame in &frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, valid) = conn.unsign(raw);
                            if !valid { bad_signature = true; break; }
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
                            let (unsigned, valid) = conn.unsign(raw);
                            if !valid { bad_signature = true; break; }
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< hmds/bin {}", crate::protocol::fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, valid) = conn.unsign(raw);
                            if !valid { bad_signature = true; break; }
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
                (msgs, bad_signature)
            }
        };
        for msg in &messages {
            self.process_hmds_message(msg, hmds_conn, shared, event_tx, hb);
        }
        // A signature mismatch drops the connection, as the reference does;
        // with no socket held the reconnect loop re-dials it (ibx#275).
        if bad_signature {
            log::error!("HMDS frame signature mismatch: connection dropped, reconnecting");
            if let Some(conn) = hmds_conn.as_mut() {
                conn.shutdown();
            }
            self.disconnected = true;
            *hmds_conn = None;
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
                        let wid = crate::control::historical::window_id(&resp.query_id);
                        if let Some(pos) = self.pending_historical.iter().position(|(qid, _, _)| qid == wid) {
                            let (_, req_id, _) = self.pending_historical[pos];
                            let is_complete = resp.is_complete;
                            // Activity on this query — push the idle deadline out (ibx#231).
                            self.pending_historical[pos].2 = Instant::now() + HISTORICAL_IDLE_TIMEOUT;
                            if self.multi_leg.iter().any(|m| m.req_id == req_id) {
                                let leg_qid = self.pending_historical[pos].0.clone();
                                if is_complete {
                                    self.pending_historical.remove(pos);
                                }
                                self.on_leg_bars(req_id, &leg_qid, &resp, xml_tag, shared, event_tx);
                                return;
                            }
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
                        let wid = reply_window_id(xml_tag);
                        if let Some(pos) = self.pending_head_ts.iter().position(|(q, _, _)| q == wid) {
                            let (_, req_id, _) = self.pending_head_ts.remove(pos);
                            let for_event = clone_for_event(event_tx, &resp);
                            shared.reference.push_head_timestamp(req_id, resp);
                            if let Some(data) = for_event {
                                emit(event_tx, Event::HeadTimestamp { req_id, data });
                            }
                        } else {
                            log::warn!("HMDS head timestamp reply for no pending request: id={:?}", wid);
                        }
                    }
                    else if let Some(frame) = crate::control::histogram::parse_histogram_frame(xml_tag) {
                        // One frame per trading day: all are summed, and the
                        // histogram goes out once, after the last (ibx#433).
                        let wid = reply_window_id(xml_tag);
                        if let Some(pos) = self.pending_histogram.iter().position(|h| h.window_id == wid) {
                            let h = &mut self.pending_histogram[pos];
                            h.sum.add(&frame);
                            h.deadline = Instant::now() + HISTORICAL_IDLE_TIMEOUT;
                            if frame.is_complete {
                                let h = self.pending_histogram.remove(pos);
                                shared.reference.push_histogram_data(h.req_id, h.sum.entries());
                            }
                        } else {
                            log::warn!("HMDS histogram reply for no pending request: id={:?}", wid);
                        }
                    }
                    else if xml_tag.contains("<ResultSetTick>") {
                        let wid = reply_window_id(xml_tag);
                        if let Some(pos) = self.pending_ticks.iter().position(|(qid, _, _)| qid == wid) {
                            let what_to_show = self.pending_ticks[pos].2.clone();
                            let req_id = self.pending_ticks[pos].1;
                            if let Some((_, data, done)) = crate::control::historical::parse_tick_response(xml_tag, &what_to_show) {
                                // Every frame is delivered; the last one ends the
                                // request, as the reference.
                                if done {
                                    self.pending_ticks.remove(pos);
                                }
                                shared.reference.push_historical_ticks(req_id, data, what_to_show, done);
                            }
                        } else {
                            log::warn!("HMDS ticks reply for no pending request: id={:?}", wid);
                        }
                    }
                    else if let Some(resp) = crate::control::historical::parse_schedule_response(xml_tag) {
                        let wid = crate::control::historical::window_id(&resp.query_id).to_string();
                        if let Some(pos) = self.pending_schedule.iter().position(|(qid, _)| *qid == wid) {
                            let (_, req_id) = self.pending_schedule.remove(pos);
                            shared.reference.push_historical_schedule(req_id, resp);
                        }
                    }
                    else if let Some(ticker_id_str) = crate::control::historical::parse_ticker_id(xml_tag) {
                        let min_tick = crate::control::historical::extract_xml_tag(xml_tag, "minTick")
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.01);
                        let ticker_id: u32 = ticker_id_str.trim().parse().unwrap_or(0);
                        let mut matched = false;
                        // The acknowledgement of a real-time bar request, by the
                        // exact window id (ibx#454). A ticker id that is not
                        // above 0 is error 420 and ends the request.
                        let wid = reply_window_id(xml_tag);
                        if let Some(pos) = self.rtbar_subs.iter().position(|s| !s.keep_up_to_date && s.query_id == wid) {
                            matched = true;
                            if ticker_id == 0 {
                                let sub = self.rtbar_subs.remove(pos);
                                log::warn!("HMDS rtbar req_id={}: invalid ticker id {:?}", sub.req_id, ticker_id_str);
                                shared.reference.push_historical_error(sub.req_id, 420, INVALID_REAL_TIME_QUERY.to_string());
                            } else {
                                let sub = &mut self.rtbar_subs[pos];
                                sub.ticker_id = Some(ticker_id);
                                sub.min_tick = min_tick;
                                log::info!("HMDS rtbar ticker_id={} min_tick={} for req_id={}", ticker_id, min_tick, sub.req_id);
                            }
                        }
                        if !matched {
                            // Check keepUpToDate historical queries
                            let wid = reply_window_id(xml_tag);
                            for (qid, req_id, _) in &self.pending_historical {
                                if qid == wid && self.keep_up_to_date_reqs.contains(req_id) {
                                    // Store as rtbar subscription so 35=G bars get dispatched
                                    self.rtbar_subs.push(RtBarSub {
                                        query_id: qid.clone(),
                                        req_id: *req_id,
                                        ticker_id: Some(ticker_id),
                                        min_tick,
                                        keep_up_to_date: true,
                                    });
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
                        // The error goes to the request of the same window id
                        // (ibx#428), with the reference code of its kind: 162
                        // for bars, schedules and head timestamps, 10188 for a
                        // histogram, 10187 for historical ticks.
                        let mut released: Option<(u32, i32, String)> = None;
                        if let Some(qid) = &query_id {
                            let wid = crate::control::historical::window_id(qid);
                            if let Some(pos) = self.pending_historical.iter().position(|(q, _, _)| q == wid) {
                                let (leg_qid, req_id, _) = self.pending_historical.remove(pos);
                                self.keep_up_to_date_reqs.remove(&req_id);
                                self.on_leg_failed(req_id, &leg_qid);
                                released = Some((req_id, 162, historical_service_error(&error_msg)));
                            } else if let Some(pos) = self.pending_head_ts.iter().position(|(q, _, _)| q == wid) {
                                let (_, req_id, _) = self.pending_head_ts.remove(pos);
                                released = Some((req_id, 162, historical_service_error(&error_msg)));
                            } else if let Some(pos) = self.pending_histogram.iter().position(|h| h.window_id == wid) {
                                let req_id = self.pending_histogram.remove(pos).req_id;
                                released = Some((req_id, 10188, crate::control::historical::join_error_text(HISTOGRAM_ERROR, &error_msg)));
                            } else if let Some(pos) = self.pending_ticks.iter().position(|(q, _, _)| q == wid) {
                                let (_, req_id, _) = self.pending_ticks.remove(pos);
                                released = Some((req_id, 10187, crate::control::historical::join_error_text(HISTORICAL_TICKS_ERROR, &error_msg)));
                            } else if let Some(pos) = self.pending_schedule.iter().position(|(q, _)| q == wid) {
                                let (_, req_id) = self.pending_schedule.remove(pos);
                                released = Some((req_id, 162, historical_service_error(&error_msg)));
                            } else if let Some(pos) = self.rtbar_subs.iter().position(|s| !s.keep_up_to_date && s.query_id == wid) {
                                // A rejected real-time bar query: 420 with the server
                                // text, and the request ends (ibx#454).
                                let req_id = self.rtbar_subs.remove(pos).req_id;
                                released = Some((req_id, 420, crate::control::historical::join_error_text(INVALID_REAL_TIME_QUERY, &error_msg)));
                            } else if let Some(pos) = self.pending_scanner.iter().position(|(q, _)| q == qid) {
                                let (_, req_id) = self.pending_scanner.remove(pos);
                                released = Some((req_id, 162, error_msg.clone()));
                            } else if let Some(pos) = self.tbt_subscriptions.iter().position(|(_, q, _)| q == qid) {
                                // A refused tick-by-tick request ends with
                                // 10189 and the server's text (ibx#455).
                                let (instrument, _, tbt_type) = self.tbt_subscriptions.remove(pos);
                                self.tbt_price_state[instrument as usize] = (0, 0, 0);
                                log::warn!("HMDS QueryError for tick-by-tick {} query_id={}: {}", tbt_type.as_str(), qid, error_msg);
                                shared.market.push_tbt_error(instrument, tbt_type, error_msg);
                                return;
                            }
                        }
                        match released {
                            Some((req_id, code, text)) => {
                                log::warn!(
                                    "HMDS QueryError req_id={} query_id={:?}: {}",
                                    req_id, query_id, error_msg
                                );
                                // A rejected bar query ends with the error alone: the
                                // official API sends no historical_data_end after it
                                // (ibx#408). Each rejected leg of a multi-query request
                                // reports its own error under the same req_id.
                                shared.reference.push_historical_error(req_id, code, text);
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
                                let wid = crate::control::fundamental::parse_fundamental_response_id(xml)
                                    .map(|id| crate::control::historical::window_id(&id).to_string())
                                    .unwrap_or_default();
                                if let Some(pos) = self.pending_fundamental.iter().position(|(q, _)| *q == wid) {
                                    let (_, req_id) = self.pending_fundamental.remove(pos);
                                    shared.reference.push_fundamental_data(req_id, data);
                                } else {
                                    log::warn!("HMDS fundamentals reply for no pending request: id={:?}", wid);
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
            "G" => self.handle_rtbar_data(msg, shared, hmds_conn, hb),
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
            // A price or size out of range ends the message, as a
            // malformed block does (ibx#272).
            match entry {
                tick_decoder::TbtEntry::Trade { timestamp, price_cents_delta, size, exchange, conditions } => {
                    let (Some(cents), Ok(size)) = (self.update_tbt_price(instrument, *price_cents_delta), i64::try_from(*size)) else {
                        log::warn!("Tick-by-tick trade out of range dropped with the rest of its message");
                        return;
                    };
                    let Some(price) = cents.checked_mul(PRICE_SCALE / 100) else {
                        log::warn!("Tick-by-tick trade out of range dropped with the rest of its message");
                        return;
                    };
                    let trade = crate::types::TbtTrade {
                        instrument,
                        price,
                        size,
                        timestamp: *timestamp,
                        exchange: exchange.clone(),
                        conditions: conditions.clone(),
                    };
                    shared.market.push_tbt_trade(trade.clone());
                    emit(event_tx, Event::TbtTrade(trade));
                }
                tick_decoder::TbtEntry::Quote { timestamp, bid_cents_delta, ask_cents_delta, bid_size, ask_size } => {
                    let scaled = self.update_tbt_bid_ask(instrument, *bid_cents_delta, *ask_cents_delta)
                        .and_then(|(b, a)| Some((b.checked_mul(PRICE_SCALE / 100)?, a.checked_mul(PRICE_SCALE / 100)?)));
                    let (Some((bid, ask)), Ok(bid_size), Ok(ask_size)) = (scaled, i64::try_from(*bid_size), i64::try_from(*ask_size)) else {
                        log::warn!("Tick-by-tick quote out of range dropped with the rest of its message");
                        return;
                    };
                    let quote = crate::types::TbtQuote {
                        instrument,
                        bid,
                        ask,
                        bid_size,
                        ask_size,
                        timestamp: *timestamp,
                    };
                    shared.market.push_tbt_quote(quote);
                    emit(event_tx, Event::TbtQuote(quote));
                }
            }
        }
    }

    /// A 5-second bar frame holds several bars (ibx#454). A bar for a
    /// ticker id with no subscription is answered with a cancel of that
    /// ticker id, as the reference.
    fn handle_rtbar_data(&mut self, msg: &[u8], shared: &SharedState, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };
        let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
        let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
        let mut unknown: Vec<u32> = Vec::new();
        for (ticker_id, timestamp, payload) in rtbar_entries(body) {
            let sub = self.rtbar_subs.iter().find(|s| s.ticker_id == Some(ticker_id));
            let (req_id, min_tick) = match sub {
                Some(s) => (s.req_id, s.min_tick),
                None => {
                    if !unknown.contains(&ticker_id) {
                        unknown.push(ticker_id);
                    }
                    continue;
                }
            };
            if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
                bar.timestamp = timestamp;
                shared.market.push_real_time_bar(req_id, bar);
            }
        }
        for ticker_id in unknown {
            log::info!("HMDS 5-second bar for unknown ticker id {}: cancelling it", ticker_id);
            self.send_historical_cancel(&ticker_id.to_string(), hmds_conn, hb);
        }
    }

    #[inline]
    /// The running last price plus a delta; None, with the state kept, when
    /// the sum is out of range (ibx#272).
    fn update_tbt_price(&mut self, instrument: InstrumentId, delta: i64) -> Option<i64> {
        let entry = &mut self.tbt_price_state[instrument as usize];
        entry.0 = entry.0.checked_add(delta)?;
        Some(entry.0)
    }

    /// The running bid and ask plus their deltas; None, with the state
    /// kept, when a sum is out of range (ibx#272).
    #[inline]
    fn update_tbt_bid_ask(&mut self, instrument: InstrumentId, bid_delta: i64, ask_delta: i64) -> Option<(i64, i64)> {
        let entry = &mut self.tbt_price_state[instrument as usize];
        let bid = entry.1.checked_add(bid_delta)?;
        let ask = entry.2.checked_add(ask_delta)?;
        entry.1 = bid;
        entry.2 = ask;
        Some((bid, ask))
    }

    pub(crate) fn send_tbt_subscribe(
        &mut self,
        con_id: i64,
        instrument: InstrumentId,
        tbt_type: TbtType,
        number_of_ticks: i32,
        ignore_size: bool,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let req_id = self.next_tbt_req_id;
        self.next_tbt_req_id += 1;
        // Each type under its own API name; the past ticks only when asked
        // for; elements in the reference's order (ibx#455). The size
        // filter's form is not known, so it is not sent.
        let tbt_type_str = tbt_type.as_str();
        let time_length = if number_of_ticks > 0 {
            format!("<timeLength>{number_of_ticks} t</timeLength>")
        } else {
            String::new()
        };
        if ignore_size {
            log::warn!("Tick-by-tick {} con_id={}: ignoreSize is not sent", tbt_type_str, con_id);
        }
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
             <data>{tbt_type_str}</data>\
             <refresh>ticks</refresh>\
             {time_length}\
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
        sec_type: &str,
        exchange: &str,
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        use_rth: bool,
        keep_up_to_date: bool,
        symbol: &str,
        hmds_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        // The reference checks, refused with its codes and texts and no
        // end (ibx#430). The client checks first; this is the engine-side
        // backstop for raw control-channel callers.
        let checked = match crate::control::historical::check_bar_request(
            end_date_time, duration, bar_size, what_to_show, None,
        ) {
            Ok(c) => c,
            Err((code, text)) => {
                log::error!("historical req_id={}: {} {}", req_id, code, text);
                shared.reference.push_historical_error(req_id, code, text);
                return;
            }
        };
        let data_type = checked.data_type;
        let bs = checked.bar_size;
        let duration = checked.duration.as_str();
        if data_type == crate::control::historical::BarDataType::Schedule {
            self.send_schedule_request(req_id, con_id, sec_type, exchange, end_date_time, duration, use_rth, hmds_conn, hb);
            return;
        }
        let end_date_time = if end_date_time.is_empty() {
            crate::gateway::chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let end_date_time = end_date_time.as_str();
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;

        // One server query per leg: BID_ASK is a Bid query plus an Ask
        // query answered as one request (ibx#408).
        let legs = data_type.legs();
        let mut leg_states = Vec::with_capacity(legs.len());
        for (i, &leg_type) in legs.iter().enumerate() {
            let qid = if i == 0 {
                qid
            } else {
                let q = self.next_hmds_query_id;
                self.next_hmds_query_id += 1;
                q
            };
            let query_id = format!("hist_{}", qid);
            let req = crate::control::historical::HistoricalRequest {
                query_id: query_id.clone(),
                con_id: con_id as u32,
                symbol: symbol.to_string(),
                sec_type: sec_type.to_string(),
                exchange: exchange.to_string(),
                data_type: leg_type,
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
                log::info!("Sent historical request: req_id={} con_id={} bar_size={} data={}",
                    req_id, con_id, bar_size, leg_type.as_str());
                hb.last_hmds_sent = Instant::now();
            }
            if legs.len() > 1 {
                leg_states.push(BarLeg {
                    query_id: query_id.clone(),
                    data_type: leg_type,
                    state: LegState::Pending,
                });
            }
            self.pending_historical.push((query_id, req_id, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
        }
        if !leg_states.is_empty() {
            // Chart name as the reference: symbol, API exchange and the
            // name of the first leg.
            let exchange = if exchange.trim().is_empty() { "SMART" } else { exchange.trim() };
            let name = format!("{}@{} {}", symbol, exchange, legs[0].as_str());
            self.multi_leg.push(MultiLegBars {
                req_id,
                legs: leg_states,
                frames: Vec::new(),
                timezone: String::new(),
                name,
            });
        }
    }

    /// Bars of one leg of a multi-query request: held until every leg has
    /// finished (ibx#408).
    fn on_leg_bars(
        &mut self,
        req_id: u32,
        leg_qid: &str,
        resp: &crate::control::historical::HistoricalResponse,
        xml: &str,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        let Some(pos) = self.multi_leg.iter().position(|m| m.req_id == req_id) else { return };
        let m = &mut self.multi_leg[pos];
        if let Some(leg) = m.legs.iter_mut().find(|l| l.query_id == leg_qid) {
            let bars = crate::control::historical::parse_leg_bars(xml);
            if !bars.is_empty() {
                m.frames.push((leg.data_type, bars));
            }
            if resp.is_complete && leg.state == LegState::Pending {
                leg.state = LegState::Done;
            }
            if m.timezone.is_empty() {
                m.timezone = resp.timezone.clone();
            }
        }
        self.finish_multi_leg(pos, shared, event_tx);
    }

    /// A leg of a multi-query request was rejected by the server. The caller
    /// reports its error; the request gives no end (ibx#408).
    fn on_leg_failed(&mut self, req_id: u32, leg_qid: &str) {
        let Some(pos) = self.multi_leg.iter().position(|m| m.req_id == req_id) else { return };
        let m = &mut self.multi_leg[pos];
        if let Some(leg) = m.legs.iter_mut().find(|l| l.query_id == leg_qid) {
            leg.state = LegState::Failed;
        }
        if m.legs.iter().all(|l| l.state != LegState::Pending) {
            self.multi_leg.remove(pos);
        }
    }

    /// Answer a multi-query request once no leg is pending: the combined
    /// bars and the end, as the reference does after all its queries end.
    /// If a leg failed, its error was the answer and nothing else is
    /// delivered. No bar in any leg gives error 162 with the no-data text
    /// of the reference, and no end.
    fn finish_multi_leg(&mut self, pos: usize, shared: &SharedState, event_tx: &Option<Sender<Event>>) {
        if self.multi_leg[pos].legs.iter().any(|l| l.state == LegState::Pending) {
            return;
        }
        let m = self.multi_leg.remove(pos);
        if m.legs.iter().any(|l| l.state == LegState::Failed) {
            return;
        }
        let bars = crate::control::historical::combine_bid_ask(&m.frames);
        if bars.is_empty() {
            log::warn!("historical req_id={}: no bar in any leg", m.req_id);
            shared.reference.push_historical_error(
                m.req_id, 162,
                historical_service_error(&format!("HMDS query returned no data: {}", m.name)),
            );
            return;
        }
        let resp = crate::control::historical::HistoricalResponse {
            query_id: m.legs[0].query_id.clone(),
            timezone: m.timezone,
            bars,
            is_complete: true,
        };
        let for_event = clone_for_event(event_tx, &resp);
        shared.reference.push_historical_data(m.req_id, resp);
        if let Some(data) = for_event {
            emit(event_tx, Event::HistoricalData { req_id: m.req_id, data });
        }
    }

    /// Cancel every in-flight query of a bar request (a BID_ASK request has
    /// two) and drop its held legs.
    pub(crate) fn cancel_historical(&mut self, req_id: u32, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        self.keep_up_to_date_reqs.remove(&req_id);
        self.multi_leg.retain(|m| m.req_id != req_id);
        let mut cancelled = Vec::new();
        self.pending_historical.retain(|(qid, rid, _)| {
            if *rid == req_id {
                cancelled.push(qid.clone());
                false
            } else {
                true
            }
        });
        for query_id in cancelled {
            self.send_historical_cancel(&query_id, hmds_conn, hb);
        }
    }

    /// Send keepUpToDate historical request via CCP (FIXCOMP compressed).
    /// Responses arrive on HMDS, not CCP (cross-connection routing).
    pub(crate) fn send_historical_request_via_ccp(
        &mut self,
        req_id: u32,
        con_id: i64,
        sec_type: &str,
        exchange: &str,
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
        shared: &SharedState,
    ) -> bool {
        // The reference checks of every bar request (ibx#430).
        let duration = match crate::control::historical::check_bar_request(
            end_date_time, duration, bar_size, what_to_show, None,
        ) {
            Ok(c) => c.duration,
            Err((code, text)) => {
                log::error!("keepUpToDate req_id={}: {} {}", req_id, code, text);
                shared.reference.push_historical_error(req_id, code, text);
                return false;
            }
        };
        // Reuse the same request builder but with keep_up_to_date=true
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
            Ok(dt) if dt.legs().len() > 1 => {
                // A two-query request has no live updates: the reference
                // refuses BID_ASK with keepUpToDate (ibx#408).
                let e = format!(
                    "what_to_show '{}' is not supported with keep_up_to_date=true",
                    what_to_show,
                );
                log::error!("keepUpToDate req_id={}: {}", req_id, e);
                super::push_hmds_error(shared, req_id, e, true);
                return false;
            }
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
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
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

    pub(crate) fn send_head_timestamp_request(&mut self, req_id: u32, con_id: i64, sec_type: &str, exchange: &str, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // Same shared table as the bar paths — this was a third divergent
        // copy with a silent TRADES fallback (ibx#232).
        let data_type = match crate::control::historical::BarDataType::from_api_str(what_to_show) {
            Ok(dt) => dt,
            Err(e) => {
                // 321 with the text of the reference (ibx#430).
                log::error!("head timestamp req_id={}: {}", req_id, e);
                shared.reference.push_historical_error(
                    req_id, 321, format!("Error validating request.-'bN' : cause - {}", e),
                );
                return;
            }
        };
        let window_id = format!("TickHeadClient{}", self.next_head_ts_window);
        self.next_head_ts_window = self.next_head_ts_window.wrapping_add(1);
        let req = crate::control::historical::HeadTimestampRequest {
            window_id: window_id.clone(),
            con_id: con_id as u32,
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            data_type,
            use_rth,
        };
        let xml = crate::control::historical::build_head_timestamp_xml(&req);
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
        self.pending_head_ts.push((window_id, req_id, Instant::now() + HEAD_TIMESTAMP_TIMEOUT));
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
        let window_id = format!("{}{}", rt.provider(), self.next_fundamental_window);
        self.next_fundamental_window = self.next_fundamental_window.wrapping_add(1);
        let req = crate::control::fundamental::FundamentalRequest {
            window_id: window_id.clone(),
            con_id,
            sec_type: "STK",
            currency: "USD",
            report_type: rt,
        };
        let xml = crate::control::fundamental::build_fundamental_request_xml(&req);
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
        self.pending_fundamental.push((window_id, req_id));
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_histogram_request(&mut self, req_id: u32, con_id: u32, sec_type: &str, exchange: &str, use_rth: bool, period: &str, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // An unreadable period is refused locally, as the reference (ibx#433).
        if crate::control::histogram::parse_period(period).is_none() {
            log::error!("histogram req_id={}: invalid time period {:?}", req_id, period);
            shared.reference.push_historical_error(
                req_id, 321,
                "Error validating request.-'bO' : cause - Invalid time period".to_string(),
            );
            return;
        }
        let window_id = format!("histogramQuery{}", self.next_histogram_window);
        self.next_histogram_window = self.next_histogram_window.wrapping_add(1);
        let req = crate::control::histogram::HistogramRequest {
            window_id: window_id.clone(),
            con_id,
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            use_rth,
            period: period.to_string(),
            end_time: chrono_free_timestamp().to_string(),
        };
        let xml = crate::control::histogram::build_histogram_request_xml(&req);
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
        self.pending_histogram.push(PendingHistogram {
            window_id,
            req_id,
            deadline: Instant::now() + HISTORICAL_IDLE_TIMEOUT,
            sum: Default::default(),
        });
    }

    pub(crate) fn send_historical_ticks_request(&mut self, req_id: u32, con_id: i64, sec_type: &str, exchange: &str, start_date_time: &str, end_date_time: &str, number_of_ticks: u32, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let query_id = format!("tk_{}", qid);
        let xml = crate::control::historical::build_tick_query_xml(
            &query_id, con_id, sec_type, exchange, start_date_time, end_date_time, number_of_ticks, what_to_show, use_rth,
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_realtime_bar_subscribe(&mut self, req_id: u32, con_id: i64, sec_type: &str, exchange: &str, _symbol: &str, what_to_show: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        // The reference checks, in its order (ibx#454): whatToShow (321),
        // a request id already streaming (102), the request limit (456).
        if crate::control::historical::realtime_bar_data(what_to_show).is_none() {
            log::error!("rtbar req_id={}: whatToShow {:?} refused", req_id, what_to_show);
            shared.reference.push_historical_error(req_id, 321,
                "Error validating request.-'bS' : cause - What to show field is missing or incorrect.".to_string());
            return;
        }
        if self.rtbar_subs.iter().any(|s| !s.keep_up_to_date && s.req_id == req_id) {
            shared.reference.push_historical_error(req_id, 102, "Duplicate ticker id".to_string());
            return;
        }
        if self.rtbar_subs.len() as u64 + 1 > self.max_real_time_requests as u64 {
            log::warn!("rtbar req_id={}: {} streams of {} allowed", req_id, self.rtbar_subs.len(), self.max_real_time_requests);
            shared.reference.push_historical_error(req_id, 456,
                "Max number of real time requests has been reached".to_string());
            return;
        }
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        let query_id = format!("rt_{}", qid);
        let xml = crate::control::historical::build_realtime_bar_xml(&query_id, con_id, sec_type, exchange, what_to_show, use_rth);
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
        self.rtbar_subs.push(RtBarSub {
            query_id,
            req_id,
            ticker_id: None,
            min_tick: 0.01,
            keep_up_to_date: false,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_schedule_request(&mut self, req_id: u32, con_id: i64, sec_type: &str, exchange: &str, end_date_time: &str, duration: &str, use_rth: bool, hmds_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let qid = self.next_hmds_query_id;
        self.next_hmds_query_id += 1;
        // Duration in the reference form (ibx#430); an unreadable one is
        // sent lower-cased as before.
        let duration = crate::control::historical::normalize_duration(duration)
            .unwrap_or_else(|_| duration.to_lowercase());
        let end_date_time = if end_date_time.is_empty() {
            chrono_free_timestamp().to_string()
        } else {
            end_date_time.to_string()
        };
        let query_id = format!("sched_{}", qid);
        let xml = crate::control::historical::build_schedule_xml(&query_id, con_id, sec_type, exchange, &end_date_time, &duration, use_rth);
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
    /// Surfaces error 162 plus a terminal
    /// `is_complete=true` sentinel so a consumer blocked on
    /// historical_data_end unblocks with no API change. keepUpToDate
    /// subscriptions are exempt — they stay resident by design and their
    /// live bars flow on the rtbar path.
    pub(crate) fn sweep_pending_historical(&mut self, shared: &SharedState) {
        self.sweep_head_ts_and_histogram(shared);
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
        // A timed-out leg of a multi-query request ends the whole request
        // once: its other legs are dropped with it (ibx#408).
        expired.sort_by_key(|(_, req_id)| *req_id);
        expired.dedup_by_key(|(_, req_id)| *req_id);
        for (_, req_id) in &expired {
            if let Some(pos) = self.multi_leg.iter().position(|m| m.req_id == *req_id) {
                self.multi_leg.remove(pos);
                self.pending_historical.retain(|(_, rid, _)| rid != req_id);
            }
        }
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

impl HmdsState {
    /// Fail head timestamp and histogram queries past their deadline, as
    /// bar queries are (ibx#428): a head timestamp gets 162 "Request Timed
    /// Out" after 5 s, as the reference; a histogram gets 10188 after the
    /// bar idle time.
    fn sweep_head_ts_and_histogram(&mut self, shared: &SharedState) {
        if self.pending_head_ts.is_empty() && self.pending_histogram.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<(u32, i32, String)> = Vec::new();
        self.pending_head_ts.retain(|(wid, req_id, deadline)| {
            if now >= *deadline {
                log::warn!("HMDS head timestamp timeout: req_id={} id={}", req_id, wid);
                expired.push((*req_id, 162, historical_service_error("Request Timed Out")));
                false
            } else {
                true
            }
        });
        self.pending_histogram.retain(|h| {
            if now >= h.deadline {
                log::warn!("HMDS histogram timeout: req_id={} id={}", h.req_id, h.window_id);
                expired.push((h.req_id, 10188, crate::control::historical::join_error_text(
                    HISTOGRAM_ERROR, "histogram request timed out — no response from the gateway")));
                false
            } else {
                true
            }
        });
        for (req_id, code, text) in expired {
            shared.reference.push_historical_error(req_id, code, text);
        }
    }
}

/// The bars of a 5-second bar frame body, read as the reference reads
/// them (ibx#454): ticker id, bar time and payload of each.
fn rtbar_entries(body: &[u8]) -> Vec<(u32, u32, &[u8])> {
    let mut entries = Vec::new();
    if body.len() < 2 {
        return entries;
    }
    let mut bits = u16::from_be_bytes([body[0], body[1]]) as usize;
    let available = (body.len() - 2) * 8;
    while bits + 65536 <= available {
        bits += 65536;
    }
    let end = (2 + bits.div_ceil(8)).min(body.len());
    let mut pos = 2;
    while pos + 9 <= end {
        let ticker_id = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        let time = u32::from_be_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]]);
        let len = body[pos + 8] as usize;
        let start = pos + 9;
        if start + len > end {
            break;
        }
        entries.push((ticker_id, time, &body[start..start + len]));
        pos = start + len;
    }
    entries
}

/// Window id of the `<id>` of a reply (ibx#428).
fn reply_window_id(xml: &str) -> &str {
    crate::control::historical::extract_xml_tag(xml, "id")
        .map(crate::control::historical::window_id)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ibx#272: a running tick-by-tick price that would leave the range is
    // refused and the state kept.
    #[test]
    fn tbt_price_sums_are_checked() {
        let mut hmds = HmdsState::new();
        assert_eq!(hmds.update_tbt_price(0, 100), Some(100));
        assert_eq!(hmds.update_tbt_price(0, i64::MAX), None);
        assert_eq!(hmds.update_tbt_price(0, 1), Some(101));
        assert_eq!(hmds.update_tbt_bid_ask(0, i64::MIN, 0), Some((i64::MIN, 0)));
        assert_eq!(hmds.update_tbt_bid_ask(0, -1, 5), None);
        assert_eq!(hmds.update_tbt_bid_ask(0, 1, 5), Some((i64::MIN + 1, 5)));
    }

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
    fn query_error_releases_historical_with_error_and_no_end() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_historical.push(("hist_1003".to_string(), 11, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
        hmds.keep_up_to_date_reqs.insert(11);

        let msg = make_query_error_msg("hist_1003", "invalid step: 1");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_historical.is_empty(), "pending entry should be drained");
        assert!(!hmds.keep_up_to_date_reqs.contains(&11), "kut flag should be cleared");

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, vec![(
            11, 162,
            "Historical Market Data Service error message:invalid step: 1".to_string(),
        )]);

        // ibx#408: a server rejection is answered by the error alone, no end.
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    // ── ibx#408: BID_ASK is two server queries answered as one request ──

    fn send_bid_ask(hmds: &mut HmdsState, shared: &SharedState, req_id: u32) -> (String, String) {
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_request_ex(req_id, 416904, "IND", "CBOE", "", "3600 S", "1 min", "BID_ASK",
            true, false, "SPX", &mut conn, &mut hb, shared);
        let legs: Vec<&(String, u32, Instant)> =
            hmds.pending_historical.iter().filter(|(_, r, _)| *r == req_id).collect();
        assert_eq!(legs.len(), 2, "BID_ASK must go out as two queries");
        (legs[0].0.clone(), legs[1].0.clone())
    }

    #[test]
    fn bid_ask_sends_two_legs_with_their_own_ids() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let (bid, ask) = send_bid_ask(&mut hmds, &shared, 5);
        assert_ne!(bid, ask);
        assert_eq!(hmds.multi_leg.len(), 1);
        let m = &hmds.multi_leg[0];
        assert_eq!(m.req_id, 5);
        let types: Vec<_> = m.legs.iter().map(|l| l.data_type).collect();
        assert_eq!(types, vec![
            crate::control::historical::BarDataType::Bid,
            crate::control::historical::BarDataType::Ask,
        ]);
    }

    #[test]
    fn bid_ask_each_failed_leg_reports_its_own_error_and_no_end() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let (bid, ask) = send_bid_ask(&mut hmds, &shared, 6);

        hmds.process_hmds_message(
            &make_query_error_msg(&bid, "No historical market data for SPX/IND@CBOE Bid 3600"),
            &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(
            &make_query_error_msg(&ask, "No historical market data for SPX/IND@CBOE Ask 3600"),
            &mut conn, &shared, &None, &mut hb);

        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, vec![
            (6, 162, "Historical Market Data Service error message:No historical market data for SPX/IND@CBOE Bid 3600".to_string()),
            (6, 162, "Historical Market Data Service error message:No historical market data for SPX/IND@CBOE Ask 3600".to_string()),
        ]);
        assert!(shared.reference.drain_historical_data().is_empty(), "no end after a rejection");
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
    }

    #[test]
    fn bid_ask_one_failed_leg_delivers_nothing_for_the_other() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let (bid, ask) = send_bid_ask(&mut hmds, &shared, 7);

        hmds.process_hmds_message(&make_query_error_msg(&bid, "Boom"), &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&make_bar_msg(&ask, true), &mut conn, &shared, &None, &mut hb);

        assert_eq!(shared.reference.drain_historical_errors().len(), 1);
        assert!(shared.reference.drain_historical_data().is_empty(),
            "the Ask leg alone must not be delivered as the BID_ASK answer");
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
    }

    fn make_leg_msg(query_id: &str, eoq: bool, bars: &[(&str, f64, f64, f64)]) -> Vec<u8> {
        let mut xml = format!(
            "<ResultSetBar><id>{}</id><eoq>{}</eoq><tz>US/Eastern</tz><Events>",
            query_id, if eoq { "true" } else { "false" },
        );
        for (time, high, low, avg) in bars {
            xml.push_str(&format!(
                "<Bar><time>{}</time><open>0</open><close>0</close><high>{}</high>                 <low>{}</low><timeAvg>{}</timeAvg></Bar>",
                time, high, low, avg,
            ));
        }
        xml.push_str("</Events></ResultSetBar>");
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W6118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    }

    #[test]
    fn bid_ask_legs_are_held_until_both_finish_then_combined() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let (bid, ask) = send_bid_ask(&mut hmds, &shared, 8);

        hmds.process_hmds_message(&make_leg_msg(&bid, false, &[("20260227-20:30:00", 266.63, 266.30, 266.466)]),
            &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&make_leg_msg(&bid, true, &[("20260227-20:31:00", 266.38, 266.00, 266.154)]),
            &mut conn, &shared, &None, &mut hb);
        assert!(shared.reference.drain_historical_data().is_empty(), "one leg must not be delivered");
        assert_eq!(hmds.multi_leg[0].legs[0].state, LegState::Done);
        assert_eq!(hmds.multi_leg[0].frames.len(), 2);
        assert_eq!(hmds.pending_historical.len(), 1, "the Ask leg is still in flight");

        hmds.process_hmds_message(&make_leg_msg(&ask, true, &[
            ("20260227-20:30:00", 266.70, 266.40, 266.520),
            ("20260227-20:32:00", 266.20, 266.00, 266.100),
        ]), &mut conn, &shared, &None, &mut hb);
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1, "one answer with the end");
        let (rid, resp) = &hist[0];
        assert_eq!(*rid, 8);
        assert!(resp.is_complete);
        assert_eq!(resp.timezone, "US/Eastern");
        let b: Vec<_> = resp.bars.iter()
            .map(|b| (b.time.as_str(), b.open, b.high, b.low, b.close)).collect();
        assert_eq!(b, vec![
            // Both legs: open and low from Bid, close and high from Ask.
            ("20260227-20:30:00", 266.466, 266.70, 266.30, 266.520),
            // Bid only.
            ("20260227-20:31:00", 266.154, 266.154, 266.00, 266.154),
            // Ask only.
            ("20260227-20:32:00", 266.100, 266.20, 266.100, 266.100),
        ]);
    }

    #[test]
    fn bid_ask_with_no_bar_in_any_leg_gives_the_no_data_error() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let (bid, ask) = send_bid_ask(&mut hmds, &shared, 11);
        hmds.process_hmds_message(&make_leg_msg(&bid, true, &[]), &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&make_leg_msg(&ask, true, &[]), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_historical_errors(), vec![(
            11, 162,
            "Historical Market Data Service error message:HMDS query returned no data: SPX@CBOE Bid".to_string(),
        )]);
        assert!(shared.reference.drain_historical_data().is_empty(), "no end after the error");
        assert!(hmds.multi_leg.is_empty());
    }

    #[test]
    fn bid_ask_cancel_drops_both_legs() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        send_bid_ask(&mut hmds, &shared, 9);
        hmds.cancel_historical(9, &mut conn, &mut hb);
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
    }

    #[test]
    fn bid_ask_timeout_ends_the_request_once() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        send_bid_ask(&mut hmds, &shared, 10);
        for entry in &mut hmds.pending_historical {
            entry.2 = Instant::now() - std::time::Duration::from_secs(1);
        }
        hmds.sweep_pending_historical(&shared);
        assert!(hmds.pending_historical.is_empty());
        assert!(hmds.multi_leg.is_empty());
        assert_eq!(shared.reference.drain_historical_errors().len(), 1);
        assert_eq!(shared.reference.drain_historical_data().len(), 1);
    }

    #[test]
    fn bid_ask_keep_up_to_date_is_rejected() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let sent = hmds.send_historical_request_via_ccp(12, 416904, "IND", "CBOE", "", "3600 S", "5 secs", "BID_ASK",
            true, "SPX", &mut conn, &mut hb, &[], &std::sync::Mutex::new(Vec::new()), &shared);
        assert!(!sent);
        assert!(hmds.pending_historical.is_empty());
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].2.contains("keep_up_to_date"), "got: {}", errors[0].2);
    }

    #[test]
    fn query_error_releases_head_timestamp_without_sentinel() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_head_ts.push(("TickHeadClient4".to_string(), 42, Instant::now() + HEAD_TIMESTAMP_TIMEOUT));

        let msg = make_query_error_msg("TickHeadClient4;;265598@BEST Last;;0;;true;;0;;U", "No head timestamp");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_head_ts.is_empty());
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, vec![(42, 162, "Historical Market Data Service error message:No head timestamp".to_string())]);
        // Head-ts is not a bar request — no historical_data sentinel should fire.
        assert!(shared.reference.drain_historical_data().is_empty());
    }

    // ── ibx#428: replies and errors go to the request of the same window id ──

    fn make_w_msg(xml: &str) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    }

    fn head_ts_reply(id: &str, ts: &str) -> Vec<u8> {
        make_w_msg(&format!(
            "<ResultSetHeadTimeStamp><id>{}</id><eoq>true</eoq><headTS>{}</headTS><tz>US/Eastern</tz></ResultSetHeadTimeStamp>",
            id, ts,
        ))
    }

    #[test]
    fn head_timestamps_in_flight_get_their_own_replies() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_head_timestamp_request(1, 265598, "STK", "SMART", "TRADES", true, &mut conn, &mut hb, &shared);
        hmds.send_head_timestamp_request(2, 756733, "STK", "SMART", "TRADES", true, &mut conn, &mut hb, &shared);
        let ids: Vec<&str> = hmds.pending_head_ts.iter().map(|(w, _, _)| w.as_str()).collect();
        assert_eq!(ids, vec!["TickHeadClient1", "TickHeadClient2"]);

        // The second request is answered first.
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient2;;756733@BEST Last;;0;;true;;0;;U", "19930129-14:30:00"),
            &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient1;;265598@BEST Last;;0;;true;;0;;U", "19801212-14:30:00"),
            &mut conn, &shared, &None, &mut hb);
        let got: Vec<(u32, String)> = shared.reference.drain_head_timestamps().into_iter()
            .map(|(r, h)| (r, h.head_timestamp)).collect();
        assert_eq!(got, vec![(2, "19930129-14:30:00".to_string()), (1, "19801212-14:30:00".to_string())]);
        assert!(hmds.pending_head_ts.is_empty());
    }

    #[test]
    fn a_reply_for_an_unknown_window_id_is_not_given_to_another_request() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_head_ts.push(("TickHeadClient1".to_string(), 1, Instant::now() + HEAD_TIMESTAMP_TIMEOUT));
        hmds.process_hmds_message(&head_ts_reply("TickHeadClient9;;1@BEST Last;;0;;true;;0;;U", "20000101-00:00:00"),
            &mut conn, &shared, &None, &mut hb);
        assert!(shared.reference.drain_head_timestamps().is_empty());
        assert_eq!(hmds.pending_head_ts.len(), 1);
    }

    #[test]
    fn bar_replies_match_the_whole_window_id_not_a_prefix() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let deadline = Instant::now() + HISTORICAL_IDLE_TIMEOUT;
        hmds.pending_historical.push(("hist_100".to_string(), 1, deadline));
        hmds.pending_historical.push(("hist_1000".to_string(), 2, deadline));
        hmds.process_hmds_message(&make_bar_msg("hist_1000", true), &mut conn, &shared, &None, &mut hb);
        let hist = shared.reference.drain_historical_data();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, 2, "hist_1000 belongs to request 2, not to the prefix hist_100");
        assert_eq!(hmds.pending_historical.len(), 1);
        assert_eq!(hmds.pending_historical[0].1, 1);
    }

    #[test]
    fn histogram_and_ticks_errors_use_their_own_codes() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_histogram_request(3, 265598, "STK", "SMART", true, "1 week", &mut conn, &mut hb, &shared);
        hmds.send_historical_ticks_request(4, 265598, "STK", "SMART", "", "20260312-15:00:00", 100, "TRADES", true, &mut conn, &mut hb);
        let hg = hmds.pending_histogram[0].window_id.clone();
        let tk = hmds.pending_ticks[0].0.clone();
        assert_eq!(hg, "histogramQuery0");

        hmds.process_hmds_message(&make_query_error_msg(&format!("{};;265598@BEST Histogram;;0;;true;;0;;U", hg), "No data"),
            &mut conn, &shared, &None, &mut hb);
        hmds.process_hmds_message(&make_query_error_msg(&tk, "No ticks"), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_historical_errors(), vec![
            (3, 10188, "Failed to request histogram data:No data".to_string()),
            (4, 10187, "Failed to request historical ticks:No ticks".to_string()),
        ]);
        assert!(hmds.pending_histogram.is_empty() && hmds.pending_ticks.is_empty());
    }

    #[test]
    fn tick_frames_are_delivered_until_the_last_one() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_ticks.push(("tk_7".to_string(), 5, "TRADES".to_string()));
        hmds.pending_ticks.push(("tk_70".to_string(), 6, "TRADES".to_string()));
        let frame = |eoq: bool| make_w_msg(&format!(
            "<ResultSetTick><id>tk_70</id><eoq>{}</eoq><Events><Tick><time>20260312-14:30:01</time>\
             <price>1.5</price><size>100</size></Tick></Events></ResultSetTick>", eoq));
        hmds.process_hmds_message(&frame(false), &mut conn, &shared, &None, &mut hb);
        assert_eq!(hmds.pending_ticks.len(), 2, "not done yet");
        hmds.process_hmds_message(&frame(true), &mut conn, &shared, &None, &mut hb);
        let got: Vec<(u32, bool)> = shared.reference.drain_historical_ticks().into_iter().map(|t| (t.0, t.3)).collect();
        assert_eq!(got, vec![(6, false), (6, true)]);
        assert_eq!(hmds.pending_ticks.len(), 1);
        assert_eq!(hmds.pending_ticks[0].0, "tk_7");
    }

    #[test]
    fn fundamentals_reply_goes_to_its_window_id() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_fundamental_data_request(1, 265598, "ReportSnapshot", &mut conn, &mut hb);
        hmds.send_fundamental_data_request(2, 272093, "ReportSnapshot", &mut conn, &mut hb);
        assert_eq!(hmds.pending_fundamental[1].0, "Fundamentals2");
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10012\x016118=<FundResponse><id>Fundamentals2;; COMPANY_FUNDAMENTALS;;0;;true;;0;;U</id></FundResponse>\x01");
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        let got = shared.reference.drain_fundamental_data();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2);
        assert_eq!(hmds.pending_fundamental.len(), 1);
        assert_eq!(hmds.pending_fundamental[0].1, 1);
    }

    #[test]
    fn head_timestamp_and_histogram_time_out() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        hmds.pending_head_ts.push(("TickHeadClient1".to_string(), 1, past));
        let pending = |w: &str, req_id: u32, deadline: Instant| PendingHistogram {
            window_id: w.to_string(), req_id, deadline, sum: Default::default(),
        };
        hmds.pending_histogram.push(pending("histogramQuery0", 2, past));
        hmds.pending_histogram.push(pending("histogramQuery1", 3, Instant::now() + HISTORICAL_IDLE_TIMEOUT));
        hmds.sweep_pending_historical(&shared);
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], (1, 162, "Historical Market Data Service error message:Request Timed Out".to_string()));
        assert_eq!((errors[1].0, errors[1].1), (2, 10188));
        assert!(errors[1].2.starts_with("Failed to request histogram data:"));
        assert!(hmds.pending_head_ts.is_empty());
        assert_eq!(hmds.pending_histogram.len(), 1);
        assert!(shared.reference.drain_historical_data().is_empty(), "no bar end for these requests");
    }

    // ── ibx#433: a histogram is summed over all frames, sent once ──

    #[test]
    fn histogram_frames_are_summed_and_sent_once_at_the_end() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_histogram_request(7, 265598, "STK", "SMART", true, "1 week", &mut conn, &mut hb, &shared);
        let frame = |eoq: bool, ticks: &[(f64, u32)]| {
            let mut xml = format!(
                "<ResultSetHistogram><id>histogramQuery0;;265598@BEST Histogram;;0;;true;;0;;U</id>\
                 <eoq>{}</eoq><data>Last</data><minTick>0.01</minTick><sizeMinTick>1</sizeMinTick><Events>", eoq);
            for (p, s) in ticks {
                xml.push_str(&format!("<Tick><time>20260227-14:30:00</time><price>{}</price><size>{}</size></Tick>", p, s));
            }
            xml.push_str("</Events></ResultSetHistogram>");
            make_w_msg(&xml)
        };
        // Five trading days, newest first, the last one with the end flag.
        let days: [&[(f64, u32)]; 5] = [
            &[(270.5, 100), (271.0, 10)],
            &[(270.5, 200)],
            &[(269.0, 5)],
            &[(271.0, 20), (272.0, 1)],
            &[(270.5, 300)],
        ];
        for (i, ticks) in days.iter().enumerate() {
            hmds.process_hmds_message(&frame(i == 4, ticks), &mut conn, &shared, &None, &mut hb);
            if i < 4 {
                assert!(shared.reference.drain_histogram_data().is_empty(), "nothing before the last frame");
            }
        }
        let got = shared.reference.drain_histogram_data();
        assert_eq!(got.len(), 1, "one answer");
        assert_eq!(got[0].0, 7);
        let entries: Vec<(f64, i64)> = got[0].1.iter().map(|e| (e.price, e.count)).collect();
        assert_eq!(entries, vec![(269.0, 5), (270.5, 600), (271.0, 30), (272.0, 1)]);
        assert!(hmds.pending_histogram.is_empty());
    }

    #[test]
    fn histogram_with_an_unreadable_period_is_refused_with_321() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_histogram_request(8, 265598, "STK", "SMART", true, "abc", &mut conn, &mut hb, &shared);
        assert!(hmds.pending_histogram.is_empty(), "no query");
        assert_eq!(shared.reference.drain_historical_errors(), vec![(
            8, 321, "Error validating request.-'bO' : cause - Invalid time period".to_string(),
        )]);
    }

    // ── ibx#454: real-time bars ──

    fn rtbar_frame(entries: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (tid, time, payload) in entries {
            body.extend_from_slice(&tid.to_be_bytes());
            body.extend_from_slice(&time.to_be_bytes());
            body.push(payload.len() as u8);
            body.extend_from_slice(payload);
        }
        let bits = (body.len() * 8) as u16;
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=G\x01");
        msg.extend_from_slice(&bits.to_be_bytes());
        msg.extend_from_slice(&body);
        msg
    }

    fn rt_sub(query_id: &str, req_id: u32, ticker_id: Option<u32>) -> RtBarSub {
        RtBarSub { query_id: query_id.to_string(), req_id, ticker_id, min_tick: 0.01, keep_up_to_date: false }
    }

    #[test]
    fn rtbar_entries_reads_every_entry_of_a_frame() {
        let p1: &[u8] = &[1, 2, 3, 4];
        let p2: &[u8] = &[9, 9, 9, 9, 9, 9, 9, 9];
        let msg = rtbar_frame(&[(5, 1_781_772_220, p1), (6, 1_781_772_220, p2), (5, 1_781_772_225, p1)]);
        let body = &msg[5..];
        let got = rtbar_entries(body);
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].0, got[0].1, got[0].2), (5, 1_781_772_220, p1));
        assert_eq!((got[1].0, got[1].2.len()), (6, 8));
        assert_eq!((got[2].0, got[2].1), (5, 1_781_772_225));
        // Bytes past the declared length are padding, not a bar.
        let mut padded = body.to_vec();
        padded.extend_from_slice(&[0u8; 12]);
        let bits = (body.len() - 2) * 8;
        padded[0..2].copy_from_slice(&(bits as u16).to_be_bytes());
        assert_eq!(rtbar_entries(&padded).len(), 3);
    }

    #[test]
    fn rtbar_frame_with_two_tickers_feeds_both_requests_and_skips_an_unknown_one() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.rtbar_subs.push(rt_sub("rt_1", 11, Some(5)));
        hmds.rtbar_subs.push(rt_sub("rt_2", 12, Some(6)));
        // A bar with one trade at 150.00, volume 100.
        let payload = single_price_payload(15000, 100);
        let msg = rtbar_frame(&[(5, 100, &payload), (6, 105, &payload), (7, 110, &payload)]);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        let bars = shared.market.drain_real_time_bars();
        let got: Vec<(u32, u32)> = bars.iter().map(|(r, b)| (*r, b.timestamp)).collect();
        assert_eq!(got, vec![(11, 100), (12, 105)]);
        assert!((bars[0].1.close - 150.0).abs() < 1e-9, "{:?}", bars[0].1);
    }

    /// Payload of a bar with one trade, at `low_ticks` price increments
    /// and `volume`.
    fn single_price_payload(low_ticks: u32, volume: u32) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        let mut put = |v: u32, n: usize| for i in 0..n { bits.push(((v >> i) & 1) as u8) };
        put(0, 4);
        put(1, 1);
        put(1, 8);
        put(low_ticks, 31);
        put(1, 1);
        put(volume, 16);
        let mut bytes = vec![0u8; bits.len().div_ceil(32) * 4];
        for (i, b) in bits.iter().enumerate() {
            bytes[i / 8] |= b << (i % 8);
        }
        bytes.chunks(4).flat_map(|c| c.iter().rev().copied().collect::<Vec<_>>()).collect()
    }

    #[test]
    fn rtbar_ack_matches_the_exact_window_id_and_refuses_ticker_zero() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.rtbar_subs.push(rt_sub("rt_1", 1, None));
        hmds.rtbar_subs.push(rt_sub("rt_12", 2, None));
        let ack = |id: &str, tid: u32| make_w_msg(&format!(
            "<ResultSetTickerId><id>{}</id><tickerId>{}</tickerId><minTick>0.01</minTick><eoq>false</eoq></ResultSetTickerId>", id, tid));
        hmds.process_hmds_message(&ack("rt_12", 9), &mut conn, &shared, &None, &mut hb);
        assert_eq!(hmds.rtbar_subs[0].ticker_id, None, "rt_1 is not rt_12");
        assert_eq!(hmds.rtbar_subs[1].ticker_id, Some(9));
        hmds.process_hmds_message(&ack("rt_1", 0), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_historical_errors(), vec![(1, 420, "Invalid Real-time Query".to_string())]);
        assert_eq!(hmds.rtbar_subs.len(), 1);
    }

    #[test]
    fn rtbar_query_error_is_420_with_the_server_text() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.rtbar_subs.push(rt_sub("rt_3", 3, None));
        hmds.process_hmds_message(&make_query_error_msg("rt_3", "No market data permissions"), &mut conn, &shared, &None, &mut hb);
        assert_eq!(shared.reference.drain_historical_errors(),
            vec![(3, 420, "Invalid Real-time Query:No market data permissions".to_string())]);
        assert!(hmds.rtbar_subs.is_empty());
    }

    #[test]
    fn rtbar_request_checks_what_to_show_duplicate_and_limit() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.max_real_time_requests = 2;
        hmds.send_realtime_bar_subscribe(1, 265598, "STK", "SMART", "AAPL", "BID_ASK", true, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(2, 265598, "STK", "SMART", "AAPL", "TRADES", true, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(2, 265598, "STK", "SMART", "AAPL", "TRADES", true, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(3, 272093, "STK", "SMART", "MSFT", "MIDPOINT", true, &mut conn, &mut hb, &shared);
        hmds.send_realtime_bar_subscribe(4, 756733, "STK", "SMART", "SPY", "TRADES", true, &mut conn, &mut hb, &shared);
        assert_eq!(shared.reference.drain_historical_errors(), vec![
            (1, 321, "Error validating request.-'bS' : cause - What to show field is missing or incorrect.".to_string()),
            (2, 102, "Duplicate ticker id".to_string()),
            (4, 456, "Max number of real time requests has been reached".to_string()),
        ]);
        let reqs: Vec<u32> = hmds.rtbar_subs.iter().map(|s| s.req_id).collect();
        assert_eq!(reqs, vec![2, 3]);
    }

    // ── ibx#232: unknown bar_size rejects at the engine too (backstop for
    // raw control-channel callers; the client validates synchronously) ──

    #[test]
    fn engine_rejects_unknown_bar_size_with_321_and_no_end() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;

        hmds.send_historical_request_ex(9, 756733, "STK", "SMART", "", "2 d", "1 sec", "TRADES",
            true, false, "SPY", &mut conn, &mut hb, &shared);

        assert!(hmds.pending_historical.is_empty(), "rejected request must not go pending");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1, 321);
        assert!(errors[0].2.starts_with("Error validating request.-'bM' : cause - Historical data bar size setting is invalid."),
            "got: {}", errors[0].2);
        assert!(shared.reference.drain_historical_data().is_empty(), "a refusal has no end");
    }

    // ── ibx#430: the reference forms on the wire ──

    #[test]
    fn engine_sends_reference_duration_and_bar_size_and_routes_schedule() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_request_ex(1, 756733, "STK", "SMART", "", "3600", "1 Min", "trades",
            true, false, "SPY", &mut conn, &mut hb, &shared);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert_eq!(hmds.pending_historical.len(), 1);
        hmds.send_historical_request_ex(2, 756733, "STK", "SMART", "", "1 M", "1 day", "SCHEDULE",
            true, false, "SPY", &mut conn, &mut hb, &shared);
        assert_eq!(hmds.pending_historical.len(), 1, "a schedule is not a bar query");
        assert_eq!(hmds.pending_schedule.len(), 1);
        assert_eq!(hmds.pending_schedule[0].1, 2);
        hmds.send_historical_request_ex(3, 756733, "STK", "SMART", "", "1 M", "1 hour", "SCHEDULE",
            true, false, "SPY", &mut conn, &mut hb, &shared);
        assert_eq!(hmds.pending_schedule.len(), 1);
        assert_eq!(shared.reference.drain_historical_errors()[0].1, 321);
    }

    #[test]
    fn head_timestamp_with_unknown_what_to_show_is_refused_with_321() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_head_timestamp_request(4, 265598, "STK", "SMART", "TRADE", true, &mut conn, &mut hb, &shared);
        assert!(hmds.pending_head_ts.is_empty());
        assert_eq!(shared.reference.drain_historical_errors(), vec![(
            4, 321, "Error validating request.-'bN' : cause - What to show value of TRADE rejected.".to_string(),
        )]);
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
