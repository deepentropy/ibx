use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::bridge::{Event, RichOrderInfo, SharedState};
use crate::api::types as api;
use crate::engine::context::Context;
use crate::config::chrono_free_timestamp;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::types::{
    CompletedOrder, Fill, InstrumentId, MidnightSeed, NewsBulletin,
    PositionInfo, Price, Qty, Side, PRICE_SCALE, QTY_SCALE,
};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, clone_for_event, parse_price_tag, parse_qty, decode_tif};

/// Bound for an in-flight contract-details request (secdef reply or
/// per-exchange fan-out). Refreshed on fan-out activity; on expiry the
/// request surfaces error 200 + contract_details_end instead of hanging
/// forever (ibx#227). A gateway rejection arrives in well under a second,
/// and a full 27-exchange fan-out completes within a few.
const SECDEF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Number of most-recent ExecIDs retained for fill deduplication. Bounds the
/// memory of `seen_exec_ids` while staying large enough that a server replay
/// after a reconnect burst still hits the window.
const EXEC_ID_WINDOW: usize = 1024;

/// Convert a FIX OrderID hex string (e.g. "00cf16ed.000225ed.69ca0941.0001") to a stable i64 permId.
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

/// A UTC time as the reports write it, `YYYYMMDD-HH:MM:SS` with optional
/// `.sss`, to Unix seconds.
pub(crate) fn fix_utc_to_unix_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 17 || b[8] != b'-' || b[11] != b':' || b[14] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, m, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hh, mm, ss) = (num(9..11)?, num(12..14)?, num(15..17)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil date (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Split an execution id into the id without its revision and the
/// revision, the last dot segment read as hex ("0000e0d5.6ab5f36f.01.01" →
/// ("0000e0d5.6ab5f36f.01", 1)). An id with no dot has revision 0.
pub(crate) fn split_exec_revision(exec_id: &str) -> (&str, u64) {
    match exec_id.rsplit_once('.') {
        Some((base, rev)) => match u64::from_str_radix(rev, 16) {
            Ok(r) => (base, r),
            Err(_) => (exec_id, 0),
        },
        None => (exec_id, 0),
    }
}

fn perm_id_from_fix_order_id(s: &str) -> i64 {
    // Hash only the stable prefix: "00cf16ed.000225ed.69ca0941" (drop ".0001")
    let stable = match s.rmatch_indices('.').next() {
        Some((idx, _)) if s[..idx].contains('.') => &s[..idx],
        _ => s, // no dots or only one segment — hash entire string
    };
    let mut h: u64 = 0xcbf29ce484222325;
    for b in stable.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h >> 1) as i64
}

pub(crate) struct CcpState {
    pub(crate) seen_exec_ids: HashSet<String>,
    /// Insertion order for `seen_exec_ids`, oldest at the front. Used to evict
    /// one entry at a time once the dedup window is full, instead of clearing
    /// the whole set — a wholesale clear would let a post-reconnect server
    /// replay of a recently-seen ExecID double-count a fill (ibx#198).
    pub(crate) exec_id_order: VecDeque<String>,
    /// Commission reports seen, by execution id without its revision:
    /// the highest revision (ibx#471). Bounded like `seen_exec_ids`.
    pub(crate) commission_revisions: std::collections::HashMap<String, u64>,
    pub(crate) commission_order: VecDeque<String>,
    pub(crate) bulletin_next_id: i32,
    pub(crate) news_subscriptions: Vec<(InstrumentId, u32)>,
    pub(crate) disconnected: bool,
    /// (req_id, is_single_shot). Single-shot = known-conId lookup whose
    /// first 35=d reply is also the last (server emits no 323=5/6 terminator
    /// for these). Multi-record by-symbol/matching-symbols requests push
    /// `false` and rely on the response-type sentinel for is_last.
    /// In-flight secdef requests: (req_id, single_shot, deadline). The
    /// deadline is swept by `sweep_contract_details` so a request the
    /// gateway never answers (SessionReject, dead socket, lost reply)
    /// surfaces error 200 + contract_details_end instead of hanging
    /// forever (ibx#227).
    pub(crate) pending_secdef: Vec<(u32, bool, Instant)>,
    pub(crate) pending_matching_symbols: Vec<u32>,
    /// keepUpToDate historical queries routed through CCP: (query_id, req_id)
    pub(crate) pending_kut_historical: Vec<(String, u32)>,
    /// tickerId → req_id mapping for keepUpToDate 35=G bar updates
    pub(crate) kut_ticker_map: std::collections::HashMap<u32, u32>,
    /// tickerId → minTick for bar decoding
    pub(crate) kut_min_tick: std::collections::HashMap<u32, f64>,
    /// HMAC signing key for XML-carrying CCP messages (selective signing).
    pub(crate) ccp_sign_key: Vec<u8>,
    /// HMAC signing IV — advances only for signed messages, independent of unsigned ones.
    pub(crate) ccp_sign_iv: std::sync::Mutex<Vec<u8>>,
    /// Secdef replies awaiting paired schedule reply (joined by tag 6256).
    pub(crate) pending_schedule_pair: Vec<PendingSchedulePair>,
    /// Counter for internal schedule subscribe req IDs.
    pub(crate) next_schedule_sub_id: u32,
    /// Fan-out state for by-symbol secdef requests. Each entry tracks the
    /// per-exchange `35=c` requests we issued in response to the master
    /// `35=d|320={api_req_id}|6046={list}` reply, and counts the per-exchange
    /// `35=d` replies as they arrive. `contract_details_end` fires for
    /// `api_req_id` once `received >= fanout_req_ids.len()`.
    pub(crate) pending_fanout: Vec<PendingFanout>,
    /// Counter for internal fan-out req IDs (tag 320 on per-exchange `35=c`).
    pub(crate) next_fanout_id: u32,
    /// Counter for internal secdef req IDs (auto-fetch on cold-cache positions).
    pub(crate) next_internal_secdef_id: u32,
    /// conIds we've already auto-fetched secdef for, keyed by con_id (dedup).
    pub(crate) auto_fetched_conids: HashSet<i64>,
    /// Scanner results awaiting per-conId contract-detail enrichment.
    /// Each entry parks a parsed `<ScanResponse>` until every cache-miss
    /// con_id has been resolved via the same 35=d path that user-initiated
    /// `reqContractDetails` uses. See ibx#156, ib-agent#142.
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

/// In-flight by-symbol fan-out: per-exchange `35=c` requests we sent after
/// the master `35=d` reply. Each per-exchange `35=d` reply (matched by tag
/// 320 string) is forwarded to `api_req_id` as one `contract_details`.
pub(crate) struct PendingFanout {
    pub api_req_id: u32,
    pub fanout_req_ids: Vec<String>,
    pub received: usize,
    /// Idle deadline, refreshed on every per-exchange reply. One lost or
    /// unparseable fan-out reply out of ~27 previously left the counter
    /// short forever and contract_details_end never fired (ibx#227).
    pub deadline: Instant,
}

impl CcpState {
    pub(crate) fn new() -> Self {
        Self {
            seen_exec_ids: HashSet::with_capacity(256),
            exec_id_order: VecDeque::with_capacity(256),
            commission_revisions: std::collections::HashMap::with_capacity(256),
            commission_order: VecDeque::with_capacity(256),
            bulletin_next_id: 0,
            news_subscriptions: Vec::new(),
            disconnected: false,
            pending_secdef: Vec::new(),
            pending_matching_symbols: Vec::new(),
            pending_kut_historical: Vec::new(),
            kut_ticker_map: std::collections::HashMap::new(),
            kut_min_tick: std::collections::HashMap::new(),
            ccp_sign_key: Vec::new(),
            ccp_sign_iv: std::sync::Mutex::new(Vec::new()),
            pending_schedule_pair: Vec::new(),
            next_schedule_sub_id: 1,
            pending_fanout: Vec::new(),
            next_fanout_id: 1,
            next_internal_secdef_id: 0xF000_0000,
            auto_fetched_conids: HashSet::new(),
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
    /// fill and corrupt the position (ibx#198).
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

    /// Record a commission report for `exec_id`. Returns `false` for a
    /// report to skip, as the reference does (ibx#471): the same execution
    /// id again, or a lower revision than one already reported. The revision
    /// is the last dot segment, in hex; a higher one replaces the report.
    pub(crate) fn record_commission(&mut self, exec_id: &str) -> bool {
        let (base, revision) = split_exec_revision(exec_id);
        match self.commission_revisions.get(base) {
            Some(&seen) if revision <= seen => return false,
            Some(_) => {}
            None => {
                self.commission_order.push_back(base.to_string());
                while self.commission_order.len() > EXEC_ID_WINDOW {
                    if let Some(old) = self.commission_order.pop_front() {
                        self.commission_revisions.remove(&old);
                    }
                }
            }
        }
        self.commission_revisions.insert(base.to_string(), revision);
        true
    }

    /// The server's commission frame for one execution (ibx#471). The fill
    /// report carries no commission; this frame follows it (captured
    /// 25/09/2026, 25 ms after the fill). A frame with neither the
    /// commission nor tag 8189 is dropped, like the reference.
    fn handle_commission_report(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        let Some(exec_id) = parsed.get(&17).filter(|s| !s.is_empty()) else {
            log::debug!("Commission report without an execution id: dropped");
            return;
        };
        let commission = parsed.get(&6378).and_then(|s| s.parse::<f64>().ok());
        if commission.is_none() && !parsed.contains_key(&8189) {
            log::debug!("Commission report for {} without a commission: dropped", exec_id);
            return;
        }
        if !self.record_commission(exec_id) {
            log::debug!("Commission report for {} skipped: already reported", exec_id);
            return;
        }
        let unset_when_zero = |v: Option<f64>| v.filter(|x| *x != 0.0).unwrap_or(f64::MAX);
        let report = api::CommissionAndFeesReport {
            exec_id: exec_id.clone(),
            commission_and_fees: commission.unwrap_or(0.0),
            currency: parsed.get(&6381).cloned().unwrap_or_default(),
            realized_pnl: unset_when_zero(parsed.get(&6099).and_then(|s| s.parse().ok())),
            yield_amount: parsed.get(&236).and_then(|s| s.parse().ok()).unwrap_or(f64::MAX),
            yield_redemption_date: parsed.get(&696).filter(|s| s.len() == 8).cloned().unwrap_or_default(),
        };
        log::info!("Commission report: exec={} commission={} {}", report.exec_id, report.commission_and_fees, report.currency);
        shared.orders.push_commission_report(report);
    }

    pub(crate) fn poll_executions(
        &mut self,
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        if self.disconnected { return; }
        let messages = match ccp_conn.as_mut() {
            None => return,
            Some(conn) => {
                match conn.try_recv() {
                    Ok(0) if !conn.has_buffered_data() => return,
                    Ok(0) => {}
                    Err(e) => {
                        log::error!("CCP connection lost: {}", e);
                        self.handle_disconnect(context, event_tx);
                        return;
                    }
                    Ok(_) => {
                        hb.last_ccp_recv = Instant::now();
                        // RTT sample (ibx#158): interval from the test request
                        // to the first inbound traffic after it. On a quiet
                        // link (the ping use case) that is the echo itself.
                        if let Some((_, sent_at)) = hb.pending_ccp_test.take() {
                            shared.set_ccp_rtt(hb.last_ccp_recv.duration_since(sent_at));
                        }
                    }
                }
                let frames = conn.extract_frames();
                let mut msgs = Vec::new();
                for frame in frames {
                    match frame {
                        Frame::FixComp(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            match fixcomp::fixcomp_decompress(&unsigned) {
                                Ok(inner) => {
                                    if log::log_enabled!(log::Level::Trace) {
                                        for m in &inner {
                                            log::trace!("WIRE< ccp/comp {}", fix::fmt_pipe(m));
                                        }
                                    }
                                    msgs.extend(inner);
                                }
                                Err(e) => {
                                    log::warn!(
                                        "CCP: dropping malformed FIXCOMP frame ({} bytes): {}",
                                        unsigned.len(), e,
                                    );
                                }
                            }
                        }
                        Frame::Fix(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/fix {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Binary(raw) => {
                            let (unsigned, _) = conn.unsign(&raw);
                            if log::log_enabled!(log::Level::Trace) {
                                log::trace!("WIRE< ccp/bin {}", fix::fmt_pipe(&unsigned));
                            }
                            msgs.push(unsigned);
                        }
                        Frame::Control(_) => {
                            // 8=1 / 8=X control state — not consumed on the order path (ibx#185).
                        }
                    }
                }
                msgs
            }
        };
        for msg in &messages {
            self.process_ccp_message(msg, ccp_conn, context, shared, event_tx, hb, account_id);
        }
    }

    pub(crate) fn process_ccp_message(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        let parsed = fix::fix_parse(msg);
        let msg_type = match parsed.get(&fix::TAG_MSG_TYPE) {
            Some(t) => t.as_str(),
            None => return,
        };
        match msg_type {
            fix::MSG_EXEC_REPORT => self.handle_exec_report(&parsed, context, shared, event_tx, account_id),
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
                log::warn!("SessionReject: reason='{}' refTag={}", reason, ref_tag);
                // ibx#229: a rejection of an in-flight contract-details
                // request was warn-only — the caller saw neither error()
                // nor end (a hang until the ibx#227 sweep, and before that
                // forever). The reject carries no request id, so attribute
                // it only when it cannot be ambiguous: exactly one pending
                // lookup. Otherwise the sweep bounds the damage.
                if self.pending_secdef.len() == 1 && self.pending_fanout.is_empty() {
                    let (req_id, _, _) = self.pending_secdef.remove(0);
                    if req_id < 0xF000_0000 {
                        shared.reference.push_historical_error(
                            req_id, 200,
                            format!("contract details request rejected: {}", reason),
                        );
                        shared.reference.push_contract_details_end(req_id);
                        emit(event_tx, Event::ContractDetailsEnd(req_id));
                    }
                }
            }
            "U" => {
                if let Some(comm) = parsed.get(&6040) {
                    match comm.as_str() {
                        "75" => {
                            // Position + market price feed (init burst + after each fill)
                            self.handle_position_feed(msg, ccp_conn, context, shared, event_tx, hb);
                            shared.portfolio.set_account_download_complete();
                        }
                        "77" => {
                            self.handle_account_summary(&parsed, context, shared);
                            shared.portfolio.set_account_download_complete();
                        }
                        "143" => {
                            // P&L midnight seed — store for client-side daily P&L computation
                            handle_pnl_response(msg, shared);
                        }
                        "186" => {
                            if let Some(matches) = crate::control::contracts::parse_matching_symbols_response(msg) {
                                // A 186 frame is the real answer only when it
                                // carries the match-count tag 146 — present
                                // even when the count is zero. Frames without
                                // it are not-ready acks: popping on one would
                                // deliver a bogus empty answer and orphan the
                                // data frame that follows (observed live; the
                                // same ack-then-data shape as the what-if
                                // path). See ibx#228.
                                if extract_tag_value(msg, b"146=").is_none() {
                                    log::debug!("matching-symbols ack frame (no tag 146) — awaiting data frame");
                                } else {
                                // Match the reply to its request by the req_id
                                // the server echoes in tag 320, NOT by queue
                                // order: FIFO cross-attributes out-of-order
                                // replies (ibx#228, same fix as pending_secdef).
                                let echoed = extract_tag_value(msg, b"320=")
                                    .and_then(|v| v.parse::<u32>().ok());
                                let pos = match echoed {
                                    Some(rid) => self.pending_matching_symbols.iter().position(|p| *p == rid),
                                    // No echo on the wire: attribution is only
                                    // safe with a single request in flight.
                                    None if self.pending_matching_symbols.len() == 1 => Some(0),
                                    None => None,
                                };
                                if let Some(pos) = pos {
                                    let req_id = self.pending_matching_symbols.remove(pos);
                                    // An empty result is a legitimate answer
                                    // ("no such symbol") and MUST be delivered:
                                    // dropping it left the caller waiting forever
                                    // and the stale queue head misattributed
                                    // every later reply (ibx#228).
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
                        "60" => self.handle_commission_report(&parsed, shared),
                        "102" => self.handle_exchange_list(msg, shared),
                        "107" => self.handle_schedule_reply(msg, shared, event_tx),
                        _ => {}
                    }
                }
            }
            "W" => {
                // keepUpToDate historical data responses routed through CCP
                if let Some(xml_tag) = parsed.get(&6118) {
                    if let Some(resp) = crate::control::historical::parse_bar_response(xml_tag) {
                        if let Some(pos) = self.pending_kut_historical.iter().position(|(qid, _)| *qid == resp.query_id) {
                            let (_, req_id) = self.pending_kut_historical[pos];
                            shared.reference.push_historical_data(req_id, resp.clone());
                            if resp.is_complete {
                                // Initial batch done — keep entry for streaming
                            }
                        }
                    }
                    else if let Some(ticker_id_str) = crate::control::historical::parse_ticker_id(xml_tag) {
                        let ticker_id: u32 = ticker_id_str.parse().unwrap_or(0);
                        let min_tick = crate::control::historical::extract_xml_tag(xml_tag, "minTick")
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.01);
                        // Match ticker to a pending keepUpToDate query
                        for (qid, req_id) in &self.pending_kut_historical {
                            if xml_tag.contains(qid) {
                                self.kut_ticker_map.insert(ticker_id, *req_id);
                                self.kut_min_tick.insert(ticker_id, min_tick);
                                break;
                            }
                        }
                    }
                }
            }
            "G" => {
                // keepUpToDate streaming bar updates (same binary format as rtbar)
                let body = match super::find_body_after_tag(msg, b"35=G\x01") {
                    Some(b) => b,
                    None => return,
                };
                let sig_pos = body.windows(6).position(|w| w == b"\x018349=");
                let body = if let Some(pos) = sig_pos { &body[..pos] } else { body };
                if body.len() >= 11 {
                    let ticker_id = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                    let timestamp = u32::from_be_bytes([body[6], body[7], body[8], body[9]]);
                    let payload_len = body[10] as usize;
                    if body.len() >= 11 + payload_len {
                        if let Some(&req_id) = self.kut_ticker_map.get(&ticker_id) {
                            let min_tick = self.kut_min_tick.get(&ticker_id).copied().unwrap_or(0.01);
                            let payload = &body[11..11 + payload_len];
                            if let Some(mut bar) = crate::control::historical::decode_bar_payload(payload, min_tick) {
                                bar.timestamp = timestamp;
                                let hist_bar = crate::control::historical::HistoricalBar {
                                    time: format!("{}", timestamp),
                                    open: bar.open,
                                    high: bar.high,
                                    low: bar.low,
                                    close: bar.close,
                                    volume: bar.volume as i64,
                                    wap: bar.wap,
                                    count: bar.count as u32,
                                };
                                let resp = crate::control::historical::HistoricalResponse {
                                    query_id: String::new(),
                                    timezone: String::new(),
                                    bars: vec![hist_bar],
                                    is_complete: true,
                                };
                                shared.reference.push_historical_data(req_id, resp);
                            }
                        }
                    }
                }
            }
            "UT" | "UM" | "RL" => handle_account_update(msg, context, shared),
            "UP" => handle_portfolio_message(msg, context, shared, event_tx),
            "d" => {
                let response_req_id = crate::control::contracts::secdef_response_req_id(msg);
                let fanout_idx = response_req_id.as_ref().and_then(|rid| {
                    self.pending_fanout.iter().position(|p| {
                        p.fanout_req_ids.iter().any(|id| id == rid)
                    })
                });
                if let Some(idx) = fanout_idx {
                    if let Some(def) = crate::control::contracts::parse_secdef_response(msg) {
                        let api_req_id = self.pending_fanout[idx].api_req_id;
                        if def.con_id != 0 {
                            let sec_type_str = def.sec_type.to_api_str();
                            shared.reference.cache_contract(def.con_id as i64, api::Contract {
                                con_id: def.con_id as i64,
                                symbol: def.symbol.clone(),
                                sec_type: sec_type_str.to_string(),
                                exchange: def.exchange.clone(),
                                currency: def.currency.clone(),
                                local_symbol: def.local_symbol.clone(),
                                primary_exchange: def.primary_exchange.clone(),
                                trading_class: def.trading_class.clone(),
                                ..Default::default()
                            });
                            self.try_release_scanner_enrichments(def.con_id as i64, shared);
                        }
                        let for_event = clone_for_event(event_tx, &def);
                        shared.reference.push_contract_details(api_req_id, def);
                        if let Some(details) = for_event {
                            emit(event_tx, Event::ContractDetails { req_id: api_req_id, details });
                        }
                        self.pending_fanout[idx].received += 1;
                        self.pending_fanout[idx].deadline = Instant::now() + SECDEF_TIMEOUT;
                        if self.pending_fanout[idx].received >= self.pending_fanout[idx].fanout_req_ids.len() {
                            shared.reference.push_contract_details_end(api_req_id);
                            emit(event_tx, Event::ContractDetailsEnd(api_req_id));
                            self.pending_fanout.swap_remove(idx);
                        }
                    }
                    let rules = crate::control::contracts::parse_market_rules(msg);
                    if !rules.is_empty() {
                        shared.reference.push_market_rules(rules);
                    }
                    return;
                }

                if let Some(def) = crate::control::contracts::parse_secdef_response(msg) {
                    let is_last_wire = crate::control::contracts::secdef_response_is_last(msg);
                    if def.con_id != 0 {
                        let sec_type_str = def.sec_type.to_api_str();
                        shared.reference.cache_contract(def.con_id as i64, api::Contract {
                            con_id: def.con_id as i64,
                            symbol: def.symbol.clone(),
                            sec_type: sec_type_str.to_string(),
                            exchange: def.exchange.clone(),
                            currency: def.currency.clone(),
                            local_symbol: def.local_symbol.clone(),
                            primary_exchange: def.primary_exchange.clone(),
                            trading_class: def.trading_class.clone(),
                            ..Default::default()
                        });
                        self.try_release_scanner_enrichments(def.con_id as i64, shared);
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
                        }
                        let con_id = def.con_id as i64;
                        if join_key.is_empty() {
                            // No join key — emit immediately without schedule data.
                            if !is_internal {
                                let for_event = clone_for_event(event_tx, &def);
                                shared.reference.push_contract_details(req_id, def);
                                if let Some(details) = for_event {
                                    emit(event_tx, Event::ContractDetails { req_id, details });
                                }
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
                        if is_by_symbol && !is_last_wire {
                            self.pending_secdef.retain(|(rid, ss, _)| !(*rid == req_id && !*ss));
                            if fanout_exchanges.is_empty() || con_id == 0 {
                                // The master row may be parked awaiting its
                                // schedule pair; firing end now would order
                                // end BEFORE the row (ibx#227). Defer it to
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
                                    received: 0,
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
            other => {
                log::debug!("CCP unhandled 35={}: {} bytes", other, msg.len());
            }
        }
    }

    fn handle_exec_report(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        account_id: &str,
    ) {
        // CCP recovery push format A (ib-agent#155, captured against live):
        // 35=8 with 150=0/39=0, tag 11 carries `<permId>.0`, the originating
        // orderId is in tag 6121. For these, prefer 6121 as the local key so
        // cancel_order(<prior-session orderId>) finds the right ClOrdID.
        // Format B (paper account, observed live): tag 11 carries the
        // originating orderId directly with `.0` suffix, tags 6119/6121
        // absent — the existing tag-11 split below already gives the right
        // value. The unwrap_or_else fallback handles both.
        let recovery_origin_order_id: Option<u64> = if parsed.get(&150).map(|s| s.as_str()) == Some("0")
            && parsed.get(&39).map(|s| s.as_str()) == Some("0")
            && parsed.contains_key(&6121)
        {
            parsed.get(&6121).and_then(|s| s.parse::<u64>().ok())
        } else {
            None
        };

        let clord_id = recovery_origin_order_id.unwrap_or_else(|| {
            parsed.get(&11).and_then(|s| {
                let stripped = s.strip_prefix('C').unwrap_or(s);
                // Strip versioned suffix (.0, .1, .2) from modify-chained ClOrdIDs
                let base = stripped.split('.').next().unwrap_or(stripped);
                base.parse::<u64>().ok()
            }).unwrap_or(0)
        });

        // Recovery insert: a 35=8 with status New/New (150=0/39=0) for an order
        // that is NOT in this session's context is a cross-session recovery entry
        // pushed by CCP on session establishment. Insert into context.open_orders
        // so subsequent cancel/modify ACKs at ~line 668 can match via
        // context.order(clord_id) and emit OrderUpdate events to the user. ibx#191.
        let is_new_ack = parsed.get(&150).map(|s| s.as_str()) == Some("0")
            && parsed.get(&39).map(|s| s.as_str()) == Some("0");
        if is_new_ack && context.order(clord_id).is_none() {
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let side = match parsed.get(&54).map(|s| s.as_str()) {
                Some("1") => Side::Buy,
                Some("5") => Side::ShortSell,
                _ => Side::Sell,
            };
            let qty: Qty = parsed.get(&38).and_then(|s| parse_qty(s)).unwrap_or(0);
            let limit_price_i64: i64 = parsed.get(&44)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * PRICE_SCALE as f64) as i64)
                .unwrap_or(0);
            let stop_price_i64: i64 = parsed.get(&99)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * PRICE_SCALE as f64) as i64)
                .unwrap_or(0);
            let ord_type_byte: u8 = parsed.get(&40).and_then(|s| s.bytes().next()).unwrap_or(b'2');
            let tif_byte: u8 = parsed.get(&59).and_then(|s| s.bytes().next()).unwrap_or(b'1');
            // A full instrument table must not stop the engine: register()
            // panics there, and the panic ended the hot loop (ibx#257).
            let instrument = if con_id != 0 && qty > 0 {
                let slot = context.market.try_register(con_id);
                if slot.is_none() {
                    log::error!("CCP recovery: instrument table full ({} contracts), order {} on con_id {} is not tracked",
                        crate::types::MAX_INSTRUMENTS, clord_id, con_id);
                }
                slot
            } else {
                None
            };
            if let Some(instrument) = instrument {
                // req_global_cancel walks ids below this count; without it a
                // recovered order on a new contract was never cancelled.
                shared.market.set_instrument_count(context.market.count());
                if let Some(sym) = parsed.get(&55) {
                    context.set_symbol(instrument, sym.clone());
                }
                context.insert_order(crate::types::Order {
                    order_id: clord_id,
                    instrument,
                    side,
                    price: limit_price_i64,
                    qty_fixed: qty,
                    filled_fixed: 0,
                    status: crate::types::OrderStatus::Submitted,
                    ord_type: ord_type_byte,
                    tif: tif_byte,
                    stop_price: stop_price_i64,
                });
                log::info!("CCP recovery: inserted orderId={} sym={:?} side={:?} qty={} px={}",
                    clord_id, parsed.get(&55), side, qty as f64 / QTY_SCALE as f64,
                    limit_price_i64 as f64 / PRICE_SCALE as f64);
            }
        }

        // Drop the sentinel/end-of-stream record (ClOrdID="*"/"0"/absent → parses
        // to 0). Real orders are assigned monotonic IDs via next_order_id and
        // never collide with 0. The recovery-push terminator (11='*') lands here.
        if clord_id == 0 {
            log::debug!("ExecReport: dropping sentinel record (ClOrdID=0/*) sym={:?} status={:?}",
                parsed.get(&55), parsed.get(&39));
            return;
        }

        // Record the ClOrdID exactly as the server reports it so subsequent
        // cancel/modify can echo back the same string. Skip cancel-ack frames
        // (tag 11 starts with 'C' there) — those carry the cancel request's
        // own id, not the original order's. See ibx#179.
        if let Some(raw_clord) = parsed.get(&11) {
            if !raw_clord.starts_with('C') && raw_clord != "*" {
                context.last_clord.insert(clord_id, raw_clord.clone());
            }
        }

        // What-If response: tag 6091=1 with margin data (tag 6092+).
        // The gateway emits a not-ready ack frame whose margin fields carry the
        // literal string "n/a" (parse fails), then a data frame with numbers.
        // Discriminate on parse-success, NOT positivity: a margin-reducing
        // preview (closing a position, cash-account sell) legitimately resolves
        // to init_margin_after == 0, and the gateway sends that as a numeric "0"
        // which must be delivered. Guarding on `> 0.0` silently dropped those
        // and left the caller's pending what-if to time out (ibx#205).
        // The not-ready ack is not always emitted — close/reject previews send a
        // single data frame — so accept the first data frame with no assumption
        // that an ack precedes it. A frame is the real preview when ANY of the
        // six margin fields (6826/6827/6828 before, 6092/6093/6094 after)
        // parses as a finite number, mirroring the gateway's own real-frame
        // test: each field is "set" when it parses, unset on nan/unparseable,
        // and the frame is real when any field is set (ibx#213, ibx#214). The
        // ack carries "n/a" in all six, so it never matches. Captured
        // byte-level in ib-agent#160.
        if parsed.get(&6091).map(|s| s.as_str()) == Some("1") {
            const MARGIN_TAGS: [u32; 6] = [6826, 6827, 6828, 6092, 6093, 6094];
            let is_data_frame = MARGIN_TAGS.iter().any(|tag| {
                parsed.get(tag)
                    .and_then(|s| s.parse::<f64>().ok())
                    .is_some_and(|f| f.is_finite())
            });
            if is_data_frame {
                if let Some(order) = context.order(clord_id).copied() {
                    let response = crate::types::WhatIfResponse {
                        order_id: clord_id,
                        instrument: order.instrument,
                        init_margin_before: parse_price_tag(parsed.get(&6826)),
                        maint_margin_before: parse_price_tag(parsed.get(&6827)),
                        equity_with_loan_before: parse_price_tag(parsed.get(&6828)),
                        init_margin_after: parse_price_tag(parsed.get(&6092)),
                        maint_margin_after: parse_price_tag(parsed.get(&6093)),
                        equity_with_loan_after: parse_price_tag(parsed.get(&6094)),
                        commission: parse_price_tag(parsed.get(&6378)),
                    };
                    log::info!("WhatIf response: clord={} initMargin={:.2}->{:.2} commission={:.2}",
                        clord_id,
                        response.init_margin_before as f64 / PRICE_SCALE as f64,
                        response.init_margin_after as f64 / PRICE_SCALE as f64,
                        response.commission as f64 / PRICE_SCALE as f64);
                    context.remove_order(clord_id);
                    shared.orders.push_what_if(response);
                    emit(event_tx, Event::WhatIf(response));
                }
            }
            return;
        }

        let ord_status = parsed.get(&39).map(|s| s.as_str()).unwrap_or("");
        let exec_type = parsed.get(&150).map(|s| s.as_str()).unwrap_or("");
        let exec_id = parsed.get(&17).map(|s| s.as_str()).unwrap_or("");
        let last_px = parsed.get(&31).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        // Fixed-point: a fraction of a share is kept (ibx#313).
        let last_shares: Qty = parsed.get(&32).and_then(|s| parse_qty(s)).unwrap_or(0);
        let leaves_qty: Qty = parsed.get(&151).and_then(|s| parse_qty(s)).unwrap_or(0);
        let commission = parsed.get(&12).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

        if ord_status == "8" {
            log::warn!("ExecReport REJECTED: clord={} reason='{}' 103={}",
                clord_id,
                parsed.get(&58).map(|s| s.as_str()).unwrap_or("?"),
                parsed.get(&103).map(|s| s.as_str()).unwrap_or("?"));
        } else {
            log::info!("ExecReport: 39={} 150={} 11={} 58={} 103={}",
                ord_status, exec_type, clord_id,
                parsed.get(&58).map(|s| s.as_str()).unwrap_or(""),
                parsed.get(&103).map(|s| s.as_str()).unwrap_or(""));
        }

        // 39=0 is New on the wire, but the gateway reports PreSubmitted
        // until the order is actually routed to and acknowledged by an
        // exchange (for example a limit order resting pre-market). Routing
        // shows up on the same exec report as a non-empty ExDestination
        // (tag 100) plus an exec ref (tag 198) other than "NONE"; before
        // routing both are absent/"NONE". Captured in ib-agent#162 (ibx#210).
        let working = || {
            let routed = parsed.get(&100).is_some_and(|s| !s.is_empty())
                || parsed.get(&198).is_some_and(|s| s != "NONE" && !s.is_empty());
            if routed {
                crate::types::OrderStatus::Submitted
            } else {
                crate::types::OrderStatus::PreSubmitted
            }
        };
        // A cancel request carries a "C"-prefixed ClOrdID; a replace carries
        // the order's next version.
        let is_cancel_request = parsed.get(&11).is_some_and(|s| s.starts_with('C'));
        let status = match ord_status {
            "0" => working(),
            // Replaced: back to working, by the same routing rule. The
            // reference reports no status change across a replace
            // (ib-agent#192 A1a: PreSubmitted before and after).
            "5" => working(),
            "A" => crate::types::OrderStatus::PreSubmitted,
            // Not a status the reference reports: it logs the frame and
            // keeps the order's status (ibx#472). A fill on the same frame
            // still counts.
            "E" | "I" => match context.order(clord_id) {
                Some(o) => o.status,
                None => {
                    log::info!("ExecReport: 39={} for order {} not tracked, no status change", ord_status, clord_id);
                    return;
                }
            },
            // Pending on a replace: the reference reports nothing, so keep
            // the current status. Reporting it as PendingCancel left the
            // order looking cancelled, since nothing moves it back (ibx#247).
            "6" if !is_cancel_request => match context.order(clord_id) {
                Some(o) => o.status,
                None => crate::types::OrderStatus::PendingReplace,
            },
            "6" => crate::types::OrderStatus::PendingCancel,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" => crate::types::OrderStatus::Cancelled,
            // Pending cancel: the order stays open until 39=4 or a fill
            // (ibx#472, a server-cancelled IOC sends 39=D then 39=4).
            "D" => crate::types::OrderStatus::PendingCancel,
            "8" => crate::types::OrderStatus::Rejected,
            _ => {
                log::warn!("Unknown order status 39={} for order {}", ord_status, clord_id);
                return;
            }
        };

        // The guard's verdict (ibx#212): a stale frame the guard rejects
        // must not surface as an order_status. A frame that restates the
        // current status does: the reference reports every report of a known
        // order (ibx#473), except the statuses it reads as invalid (39=E,
        // 39=I, ibx#472).
        let change = context.apply_order_status(clord_id, status);
        let status_changed = change == crate::engine::context::StatusChange::Changed;
        let report_status = matches!(change,
            crate::engine::context::StatusChange::Changed | crate::engine::context::StatusChange::Same)
            && !matches!(ord_status, "E" | "I");

        // The reference reports a server reject as error 201 with the
        // server's reason (ib-agent#192 C1, ibx#250). Only on the first
        // transition of an order this session tracks: the "No such order"
        // frames that follow a cancel of an order already gone are not
        // shown by the reference (C2).
        if status == crate::types::OrderStatus::Rejected && status_changed {
            let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("");
            shared.orders.push_order_error(clord_id, 201, format!("Order rejected - reason:{}", reason));
        }

        // The fill or status update of this frame, pushed once the order
        // cache below holds this frame's order state (ibx#473).
        let mut fill_out: Option<(Fill, crate::bridge::FillExec)> = None;
        let mut update_out: Option<crate::types::OrderUpdate> = None;
        let mut had_fill = false;
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
            if !exec_id.is_empty() && !self.record_exec_id(exec_id) {
                log::warn!("Duplicate ExecID={} — skipping fill", exec_id);
                return;
            }
            if let Some(order) = context.order(clord_id).copied() {
                context.update_order_filled_fixed(clord_id, last_shares);
                // Order totals ride on every fill report next to the print
                // (ib-agent#192 C3): the callback's filled and average price
                // are these, not the print (ibx#315).
                let cum_qty: Qty = parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(0);
                let avg_px = parsed.get(&6).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let fill = Fill {
                    instrument: order.instrument,
                    order_id: clord_id,
                    side: order.side,
                    price: (last_px * PRICE_SCALE as f64) as i64,
                    qty_fixed: last_shares,
                    remaining_fixed: leaves_qty,
                    cum_qty_fixed: cum_qty,
                    avg_price: (avg_px * PRICE_SCALE as f64).round() as i64,
                    commission: (commission * PRICE_SCALE as f64) as i64,
                    timestamp_ns: context.now_ns(),
                };
                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell | Side::ShortSell => -last_shares,
                };
                context.update_position_fixed(order.instrument, delta);
                // notify_fill inlined
                let tag = |t: u32| parsed.get(&t).filter(|s| !s.is_empty());
                let exec = crate::bridge::FillExec {
                    exec_id: exec_id.to_string(),
                    time_secs: tag(6699).or_else(|| tag(60)).or_else(|| tag(52))
                        .and_then(|s| fix_utc_to_unix_secs(s)),
                    exchange: tag(100).or_else(|| tag(207)).cloned().unwrap_or_default(),
                    client_id: tag(6119).and_then(|s| s.parse().ok()).unwrap_or(0),
                    model_code: tag(6700).cloned().unwrap_or_default(),
                    order_ref: tag(6010).cloned().unwrap_or_default(),
                };
                shared.portfolio.set_position_fixed(fill.instrument, context.position_fixed(fill.instrument));
                fill_out = Some((fill, exec));
                had_fill = true;
            }
        }

        if report_status && !had_fill {
            if let Some(order) = context.order(clord_id).copied() {
                let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                let parent_id: i64 = parsed.get(&583).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                // Average fill price rides on status reports too (ibx#315).
                let avg_px = parsed.get(&6).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                // Filled so far as the server counts it (tag 14): an order
                // recovered at session start has no prints in this session.
                let filled = parsed.get(&14).and_then(|s| parse_qty(s)).unwrap_or(order.filled_fixed);
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status: order.status,
                    filled_qty_fixed: filled,
                    remaining_qty_fixed: leaves_qty,
                    avg_fill_price: (avg_px * PRICE_SCALE as f64).round() as i64,
                    perm_id,
                    parent_id,
                    timestamp_ns: context.now_ns(),
                };
                update_out = Some(update);
            }
        }

        // Enrich order/contract caches block
        {
            let account = parsed.get(&1).cloned().unwrap_or_default();
            let symbol = parsed.get(&55).cloned().unwrap_or_default();
            let exchange = parsed.get(&207).cloned().unwrap_or_default();
            let sec_type = parsed.get(&167).cloned().unwrap_or_default();
            let currency = parsed.get(&15).cloned().unwrap_or_default();
            let con_id: i64 = parsed.get(&6008).and_then(|s| s.parse().ok()).unwrap_or(0);
            let local_symbol = parsed.get(&6035).cloned().unwrap_or_default();
            let _routing_exchange = parsed.get(&6004).cloned().unwrap_or_default();
            let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
            let total_qty: f64 = parsed.get(&38).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let ord_type_tag = parsed.get(&40).map(|s| s.as_str()).unwrap_or("");
            let limit_price: f64 = parsed.get(&44).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let tif_tag = parsed.get(&59).map(|s| s.as_str()).unwrap_or("");
            let stop_px: f64 = parsed.get(&99).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let outside_rth = parsed.get(&6433).map(|s| s == "1").unwrap_or(false);
            let clearing_intent = parsed.get(&6419).cloned().unwrap_or_default();
            let auto_cancel_date = parsed.get(&6596).cloned().unwrap_or_default();
            let exec_exchange = parsed.get(&30).cloned().unwrap_or_default();
            let transact_time = parsed.get(&60).cloned().unwrap_or_default();
            let avg_px: f64 = parsed.get(&6).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let cum_qty: f64 = parsed.get(&14).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let last_liq: i32 = parsed.get(&851).and_then(|s| s.parse().ok()).unwrap_or(0);

            let sec_type_str = match sec_type.as_str() {
                "CS" | "COMMON" => "STK",
                "FUT" => "FUT",
                "OPT" => "OPT",
                "FOR" | "CASH" => "CASH",
                "IND" => "IND",
                "FOP" => "FOP",
                "WAR" => "WAR",
                "BAG" => "BAG",
                "BOND" => "BOND",
                "CMDTY" => "CMDTY",
                "NEWS" => "NEWS",
                "FUND" => "FUND",
                _ => &sec_type,
            };

            let order_type_str = match ord_type_tag {
                "1" => "MKT", "2" => "LMT", "3" => "STP", "4" => "STP LMT",
                "P" => "TRAIL", "5" => "MOC", "B" => "LOC", "J" => "MIT",
                "K" => "MTL", "R" => "REL", _ => ord_type_tag,
            };

            let tif_str = match tif_tag {
                "0" => "DAY", "1" => "GTC", "3" => "IOC", "4" => "FOK",
                "2" => "OPG", "6" => "GTD", "8" => "AUC", _ => "DAY",
            };

            let action = match parsed.get(&54).map(|s| s.as_str()) {
                Some("1") => "BUY",
                Some("2") => "SELL",
                Some("5") => "SSHORT",
                _ => if let Some(order) = context.order(clord_id) {
                    match order.side {
                        Side::Buy => "BUY",
                        Side::Sell => "SELL",
                        Side::ShortSell => "SSHORT",
                    }
                } else { "" },
            };

            let status_str = crate::client_core::order_status_str(status);

            let resolved_con_id = if con_id != 0 {
                con_id
            } else if let Some(order) = context.order(clord_id) {
                context.market.con_id(order.instrument).unwrap_or(0)
            } else {
                0
            };

            let contract = if resolved_con_id != 0 {
                if let Some(mut cached) = shared.reference.get_contract(resolved_con_id) {
                    if !symbol.is_empty() { cached.symbol = symbol.clone(); }
                    if !sec_type_str.is_empty() { cached.sec_type = sec_type_str.to_string(); }
                    if !exchange.is_empty() { cached.exchange = exchange.clone(); }
                    if !currency.is_empty() { cached.currency = currency.clone(); }
                    if !local_symbol.is_empty() { cached.local_symbol = local_symbol.clone(); }
                    cached
                } else {
                    api::Contract {
                        con_id: resolved_con_id,
                        symbol: symbol.clone(),
                        sec_type: sec_type_str.to_string(),
                        exchange: exchange.clone(),
                        currency: currency.clone(),
                        local_symbol: local_symbol.clone(),
                        ..Default::default()
                    }
                }
            } else {
                api::Contract {
                    symbol: symbol.clone(),
                    sec_type: sec_type_str.to_string(),
                    exchange: exchange.clone(),
                    currency: currency.clone(),
                    local_symbol: local_symbol.clone(),
                    ..Default::default()
                }
            };

            let (fb_action, fb_tif, fb_ord_type) = if let Some(ctx_order) = context.order(clord_id) {
                let a = match ctx_order.side {
                    crate::types::Side::Buy => "BUY",
                    crate::types::Side::Sell | crate::types::Side::ShortSell => "SELL",
                };
                let t = decode_tif(ctx_order.tif);
                let o = match ctx_order.ord_type {
                    b'1' => "MKT", b'2' => "LMT", b'3' => "STP", b'4' => "STP LMT",
                    b'P' => "TRAIL", _ => "",
                };
                (a, t, o)
            } else {
                ("", "", "")
            };

            // Derive 3 order-dependent fields from FIX tags
            let oca_type: i32 = match parsed.get(&6209).map(|s| s.as_str()) {
                Some("CancelOnFillWBlock") => 1,
                Some("ReduceOnFillWBlock") => 2,
                Some("ReduceOnFillNonBlock") => 3,
                Some("ReduceOnFillWBlockFromTotal") => 4,
                _ => 3, // default
            };
            let algo_strategy = parsed.get(&847).cloned().unwrap_or_default();
            let use_price_mgmt_algo: i32 = if algo_strategy == "Adaptive" { 1 } else { 0 };
            let trail_stop_price: f64 = parsed.get(&6117)
                .and_then(|s| s.parse().ok())
                .unwrap_or(f64::MAX);

            let order = api::Order {
                order_id: clord_id as i64,
                action: if action.is_empty() { fb_action.to_string() } else { action.to_string() },
                total_quantity: total_qty,
                order_type: if order_type_str.is_empty() { fb_ord_type.to_string() } else { order_type_str.to_string() },
                lmt_price: limit_price,
                aux_price: stop_px,
                tif: if tif_str.is_empty() { fb_tif.to_string() } else { tif_str.to_string() },
                account: if account.is_empty() { account_id.to_string() } else { account.clone() },
                perm_id,
                // Filled so far, not the quantity still working (ibx#309).
                filled_quantity: cum_qty,
                outside_rth,
                clearing_intent,
                auto_cancel_date,
                submitter: account_id.to_string(),
                oca_type,
                use_price_mgmt_algo,
                trail_stop_price,
                algo_strategy,
                ..Default::default()
            };

            let completed_time = if matches!(status,
                crate::types::OrderStatus::Filled |
                crate::types::OrderStatus::Cancelled |
                crate::types::OrderStatus::Rejected
            ) {
                parsed.get(&52).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            let completed_status = match status {
                crate::types::OrderStatus::Filled => "Filled".to_string(),
                crate::types::OrderStatus::Cancelled => "Cancelled".to_string(),
                crate::types::OrderStatus::Rejected => {
                    parsed.get(&58).cloned().unwrap_or_else(|| "Rejected".to_string())
                }
                _ => String::new(),
            };

            let order_state = api::OrderState {
                status: status_str.to_string(),
                commission_and_fees: commission,
                completed_time,
                completed_status,
                ..Default::default()
            };

            let last_exec = api::Execution {
                exec_id: exec_id.to_string(),
                time: transact_time,
                acct_number: account,
                exchange: exec_exchange,
                side: if let Some(o) = context.order(clord_id) {
                    match o.side { Side::Buy => "BOT", Side::Sell | Side::ShortSell => "SLD" }.to_string()
                } else { String::new() },
                shares: last_shares as f64,
                price: last_px,
                order_id: clord_id as i64,
                cum_qty,
                avg_price: avg_px,
                last_liquidity: last_liq,
                ..Default::default()
            };

            if con_id != 0 {
                shared.reference.cache_contract(con_id, contract.clone());
            }

            shared.orders.push_order_info(clord_id, RichOrderInfo {
                contract, order, order_state, last_exec,
            });
        }

        if let Some((fill, exec)) = fill_out {
            shared.orders.push_fill_with_exec(fill, exec);
            emit(event_tx, Event::Fill(fill));
        }
        if let Some(update) = update_out {
            shared.orders.push_order_update(update);
            emit(event_tx, Event::OrderUpdate(update));
        }

        if matches!(status,
            crate::types::OrderStatus::Filled |
            crate::types::OrderStatus::Cancelled |
            crate::types::OrderStatus::Rejected
        ) {
            if let Some(order) = context.order(clord_id).copied() {
                shared.orders.push_completed_order(CompletedOrder {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty_fixed: order.filled_fixed,
                    timestamp_ns: context.now_ns(),
                });
            }
            context.remove_order(clord_id);
        }
    }

    fn handle_cancel_reject(
        &mut self,
        parsed: &std::collections::HashMap<u32, String>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        // Match handle_exec_report's tag-11 parsing: strip the gateway's
        // "C" prefix and any ".0/.1/.2" modify-chain suffix.
        let orig_clord = parsed.get(&41).and_then(|s| {
            let stripped = s.strip_prefix('C').unwrap_or(s);
            let base = stripped.split('.').next().unwrap_or(stripped);
            base.parse::<u64>().ok()
        });
        let reason = parsed.get(&58).map(|s| s.as_str()).unwrap_or("Cancel rejected");
        let reject_type: u8 = parsed.get(&434).and_then(|s| s.parse().ok()).unwrap_or(1);
        let reason_code: i32 = parsed.get(&102).and_then(|s| s.parse().ok()).unwrap_or(-1);
        log::warn!("CancelReject: origClOrd={:?} type={} code={} reason={}",
            orig_clord, reject_type, reason_code, reason);

        let Some(oid) = orig_clord else { return };

        // Update local context only if we tracked the order in this session.
        let instrument = if let Some(order) = context.order(oid).copied() {
            let restore_status = if order.filled_fixed > 0 {
                crate::types::OrderStatus::PartiallyFilled
            } else {
                crate::types::OrderStatus::Submitted
            };
            // Deliberate regression (PendingCancel back to working) — the
            // ibx#212 guard would rightly block it on the ordinary path.
            context.set_order_status_forced(oid, restore_status);
            order.instrument
        } else {
            0
        };

        // FIX CxlRejReason 1 = UnknownOrder. The gateway is telling us the
        // order it just listed in the mass-status burst doesn't exist on its
        // side — drop the stale cache entry so subsequent req_open_orders
        // stops returning it. Other reasons (TooLate, OrderInProcess, ...)
        // leave the cache alone; a follow-up exec report will reconcile.
        if reason_code == 1 {
            shared.orders.remove_order_info(oid);
        }

        let reject = crate::types::CancelReject {
            order_id: oid,
            instrument,
            reject_type,
            reason_code,
            timestamp_ns: context.now_ns(),
        };
        shared.orders.push_cancel_reject(reject);
        emit(event_tx, Event::CancelReject(reject));
    }

    fn handle_news_bulletin(&mut self, parsed: &std::collections::HashMap<u32, String>, shared: &SharedState) {
        static BULLETIN_TYPE_MAP: &[(i32, i32)] = &[
            (1, 1), (2, 2), (3, 3), (8, 1), (9, 1), (10, 1),
        ];
        let fix_type: i32 = parsed.get(&fix::TAG_URGENCY)
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        let api_type = BULLETIN_TYPE_MAP.iter()
            .find(|(k, _)| *k == fix_type)
            .map(|(_, v)| *v);
        let api_type = match api_type {
            Some(t) => t,
            None => return,
        };
        let message = parsed.get(&fix::TAG_HEADLINE).cloned().unwrap_or_default();
        let exchange = parsed.get(&fix::TAG_SECURITY_EXCHANGE).cloned().unwrap_or_default();
        self.bulletin_next_id += 1;
        let bulletin = NewsBulletin {
            msg_id: self.bulletin_next_id,
            msg_type: api_type,
            message,
            exchange,
        };
        shared.market.push_news_bulletin(bulletin);
    }

    fn handle_account_summary(&mut self, parsed: &std::collections::HashMap<u32, String>, context: &mut Context, shared: &SharedState) {
        if let Some(val) = parsed.get(&9806).and_then(|s| s.parse::<f64>().ok()) {
            context.account.net_liquidation = (val * PRICE_SCALE as f64) as Price;
        }
        shared.portfolio.set_account(context.account());
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
            let sub_id_str = format!("SchedSub.{}", sub_id);
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
    /// Fail contract-details requests whose deadline has passed (ibx#227):
    /// both plain/by-symbol secdef lookups the gateway never answered and
    /// by-symbol fan-outs missing one or more per-exchange replies. On
    /// expiry the caller gets error 200 plus contract_details_end, so a
    /// blocked wait unblocks with no API change. Internal sentinel req_ids
    /// (>= 0xF000_0000: cache auto-fetch, scanner enrichment) are dropped
    /// silently — no user is waiting on them.
    pub(crate) fn sweep_contract_details(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
    ) {
        if self.pending_secdef.is_empty() && self.pending_fanout.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut expired: Vec<u32> = Vec::new();
        self.pending_secdef.retain(|(req_id, _, deadline)| {
            if now >= *deadline {
                if *req_id < 0xF000_0000 {
                    expired.push(*req_id);
                } else {
                    log::warn!("Internal secdef timeout: req_id={:#x}", req_id);
                }
                false
            } else {
                true
            }
        });
        self.pending_fanout.retain(|p| {
            if now >= p.deadline {
                log::warn!(
                    "Contract-details fan-out timeout: api_req_id={} received {} of {}",
                    p.api_req_id, p.received, p.fanout_req_ids.len(),
                );
                expired.push(p.api_req_id);
                false
            } else {
                true
            }
        });
        for req_id in expired {
            log::warn!("Contract-details timeout: req_id={} — no gateway reply within {:?}",
                req_id, SECDEF_TIMEOUT);
            shared.reference.push_historical_error(
                req_id, 200,
                "contract details request timed out — no reply from the gateway".to_string(),
            );
            shared.reference.push_contract_details_end(req_id);
            emit(event_tx, Event::ContractDetailsEnd(req_id));
        }
    }

    pub(crate) fn sweep_pending_schedule_pairs(
        &mut self,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
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
            let for_event = clone_for_event(event_tx, &p.def);
            shared.reference.push_contract_details(p.api_req_id, p.def);
            if let Some(details) = for_event {
                emit(event_tx, Event::ContractDetails { req_id: p.api_req_id, details });
            }
            if p.is_last {
                shared.reference.push_contract_details_end(p.api_req_id);
                emit(event_tx, Event::ContractDetailsEnd(p.api_req_id));
            }
        }
    }

    /// Match a 6040=107 schedule reply to a pending secdef pair and emit merged details.
    fn handle_schedule_reply(
        &mut self,
        msg: &[u8],
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
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
            pair.def.trading_hours = Some(
                crate::control::contracts::format_sessions_string(&sched.trading_hours)
            );
            pair.def.liquid_hours = Some(
                crate::control::contracts::format_sessions_string(&sched.liquid_hours)
            );
        }
        let for_event = clone_for_event(event_tx, &pair.def);
        shared.reference.push_contract_details(pair.api_req_id, pair.def);
        if let Some(details) = for_event {
            emit(event_tx, Event::ContractDetails { req_id: pair.api_req_id, details });
        }
        if pair.is_last {
            shared.reference.push_contract_details_end(pair.api_req_id);
            emit(event_tx, Event::ContractDetailsEnd(pair.api_req_id));
        }
    }

    /// Send P&L subscribe: 6040=142 with 6529=PLR.{N}|1={account}|
    pub(crate) fn send_pnl_subscribe(
        &mut self,
        req_id: i64,
        account: &str,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        if let Some(conn) = ccp_conn.as_mut() {
            let pnl_payload = format!("PLR.{}|1={}|", req_id, account);
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "142"),
                (6529, &pnl_payload),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent P&L subscribe: req_id={} account={}", req_id, account);
        }
    }

    pub(crate) fn send_news_subscribe(
        &mut self,
        con_id: i64,
        instrument: InstrumentId,
        providers: &str,
        req_id: u32,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        self.news_subscriptions.push((instrument, req_id));
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let con_id_str = (con_id as u32).to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (fix::TAG_SENDING_TIME, &ts),
                (263, "1"),
                (146, "1"),
                (262, &req_id_str),
                (6008, &con_id_str),
                (207, "NEWS"),
                (167, "CS"),
                (264, "292"),
                (6472, providers),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent news subscribe: con_id={} req_id={} providers={}", con_id, req_id, providers);
        }
    }

    pub(crate) fn send_news_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let req_id = match self.news_subscriptions.iter().position(|(id, _)| *id == instrument) {
            Some(pos) => {
                let (_, rid) = self.news_subscriptions.remove(pos);
                rid
            }
            None => return,
        };
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (262, &req_id_str),
                (263, "2"),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent news unsubscribe: instrument={:?} req_id={}", instrument, req_id);
        }
    }

    pub(crate) fn send_secdef_request(&mut self, req_id: u32, con_id: i64, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let con_id_str = con_id.to_string();
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (crate::control::contracts::TAG_SECURITY_REQ_ID, &req_id_str),
                (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                (crate::control::contracts::TAG_IB_SOURCE, "Socket"),
            ]);
            log::info!("Sent secdef request: req_id={} con_id={}", req_id, con_id);
            hb.last_ccp_sent = Instant::now();
        } else {
            // No CCP socket: the entry still gets a deadline, so the caller
            // receives error 200 + end via the sweep instead of silence (ibx#227).
            log::warn!("secdef request req_id={} queued with no CCP socket", req_id);
        }
        // Known-conId lookup: single record, no paginated terminator.
        self.pending_secdef.push((req_id, true, Instant::now() + SECDEF_TIMEOUT));
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: u32, symbol: &str, sec_type: &str, exchange: &str, currency: &str, filters: &crate::types::SecDefFilters, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let fix_exchange = if exchange == "SMART" { "BEST" } else { exchange };
            let fix_sec_type = match sec_type {
                "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND", other => other,
            };
            // Identifier lookup (ISIN/CUSIP): SecurityIDSource is the standard FIX
            // code, 1 = CUSIP, 4 = ISIN (ib-agent#174). When a known one is set the
            // lookup rides the identifier and drops the symbol/secType/filters.
            let sec_id_source = match filters.sec_id_type.to_uppercase().as_str() {
                "ISIN" => "4",
                "CUSIP" => "1",
                _ => "",
            };
            let identifier_lookup = !filters.sec_id.is_empty() && !sec_id_source.is_empty();

            let strike_str = if filters.strike > 0.0 { format!("{}", filters.strike) } else { String::new() };
            // PutOrCall: Call = 1, Put = 0 (ib-agent#171).
            let right_code = match filters.right.to_uppercase().as_str() {
                "C" | "CALL" => "1",
                "P" | "PUT" => "0",
                _ => "",
            };
            // Exchange rides tag 100; primaryExchange (when set) rides tag 207 —
            // the two were previously conflated onto 207. localSymbol replaces the
            // plain symbol; the derivative/disambiguation filters are added only
            // when set. Captured in ib-agent#171 (ibx#229).
            let mut fields: Vec<(u32, &str)> = vec![
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &req_id_str),
                (321, "2"),
            ];
            if identifier_lookup {
                // Identifier lookup: the identifier and its source replace the
                // symbol/secType/filters; exchange and currency still ride
                // (ib-agent#174).
                fields.push((22, sec_id_source));
                fields.push((48, &filters.sec_id));
            } else {
                if !filters.local_symbol.is_empty() {
                    fields.push((6035, &filters.local_symbol));
                } else {
                    fields.push((55, symbol));
                }
                if !filters.trading_class.is_empty() {
                    fields.push((6058, &filters.trading_class));
                }
                fields.push((167, fix_sec_type));
                if !filters.last_trade_date_or_contract_month.is_empty() {
                    fields.push((200, &filters.last_trade_date_or_contract_month));
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
            fields.push((100, fix_exchange));
            if !identifier_lookup && !filters.primary_exchange.is_empty() {
                fields.push((207, &filters.primary_exchange));
            }
            fields.push((15, currency));
            fields.push((6088, "Socket"));
            let _ = conn.send_fix(&fields);
            log::info!("Sent secdef lookup: req_id={} symbol={} sec_type={} identifier={}", req_id, symbol, sec_type, identifier_lookup);
            hb.last_ccp_sent = Instant::now();
        } else {
            // See send_secdef_request: sweep converts this to a visible error.
            log::warn!("secdef-by-symbol request req_id={} queued with no CCP socket", req_id);
        }
        // By-symbol lookup: master reply carries `6046={exch_list}`. The
        // server never emits a 323=5/6 terminator; completion is detected
        // by counting per-exchange fan-out replies (see `pending_fanout`).
        self.pending_secdef.push((req_id, false, Instant::now() + SECDEF_TIMEOUT));
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

    pub(crate) fn send_matching_symbols_request(&mut self, req_id: u32, pattern: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "185"),
                (320, &req_id_str),
                (58, pattern),
            ]);
            hb.last_ccp_sent = Instant::now();
            log::info!("Sent matching symbols request: req_id={} pattern='{}'", req_id, pattern);
        }
        self.pending_matching_symbols.push(req_id);
    }

    pub(crate) fn send_mkt_depth_exchanges_request(&mut self, _ccp_conn: &mut Option<Connection>, _hb: &mut HeartbeatState, shared: &SharedState) {
        // Depth exchanges are derived from the 6040=102 exchange list received during init.
        // No separate server request needed — just signal the shared state to deliver cached data.
        shared.reference.notify_depth_exchanges();
    }

    /// Parse 6040=102 exchange directory from CCP init into DepthMktDataDescription entries.
    fn handle_exchange_list(&self, msg: &[u8], shared: &SharedState) {
        use crate::types::DepthMktDataDescription;
        let raw = String::from_utf8_lossy(msg);
        let fields: Vec<&str> = raw.split('\x01').collect();

        // The message has repeating 100=EXCHANGE|6813=NAME pairs grouped by sections.
        // Sections: 6523=category|6811=category_name for stock categories,
        //           8128=N and 8129=N separate stock/derivative sections.
        // We parse all 100/6813 pairs into DepthMktDataDescription entries.
        let mut descs: Vec<DepthMktDataDescription> = Vec::new();
        let mut current_sec_type = "STK".to_string();
        let mut current_agg_group: i32 = 0;

        let mut i = 0;
        while i < fields.len() {
            let f = fields[i];
            if let Some(val) = f.strip_prefix("8128=") {
                // Section separator — exchanges above are stocks, below are derivatives
                current_sec_type = "STK".to_string();
                current_agg_group = val.parse().unwrap_or(0);
            } else if let Some(val) = f.strip_prefix("8129=") {
                current_sec_type = "FUT".to_string();
                current_agg_group = val.parse().unwrap_or(0);
            } else if let Some(exch) = f.strip_prefix("100=") {
                // Next field should be 6813=name
                let name = if i + 1 < fields.len() {
                    fields[i + 1].strip_prefix("6813=").unwrap_or("")
                } else {
                    ""
                };
                descs.push(DepthMktDataDescription {
                    exchange: exch.to_string(),
                    sec_type: current_sec_type.clone(),
                    listing_exch: name.to_string(),
                    service_data_type: if current_sec_type == "STK" { "L1".to_string() } else { "L1".to_string() },
                    agg_group: current_agg_group,
                });
                i += 1; // skip the 6813= field
            }
            i += 1;
        }
        log::info!("Parsed {} exchanges from 6040=102", descs.len());
        shared.reference.push_depth_exchanges(descs);
    }

    pub(crate) fn handle_disconnect(&mut self, context: &mut Context, _event_tx: &Option<Sender<Event>>) {
        self.disconnected = true;
        context.mark_orders_uncertain();
        // Don't emit Event::Disconnected — auto-reconnect handles CCP drops transparently.
        // Python is only notified if reconnect exhausts retries.
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        ccp_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        account_id: &str,
    ) {
        *ccp_conn = Some(conn);
        self.disconnected = false;
        hb.last_ccp_sent = Instant::now();
        hb.last_ccp_recv = Instant::now();
        hb.pending_ccp_test = None;

        if let Some(conn) = ccp_conn.as_mut() {
            let ts = chrono_free_timestamp();

            // Re-subscribe to account/position data so server pushes fresh UP/UT/UM messages.
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "91"), (1, account_id), (6556, "DR.1"), (6712, "1"),
            ]);
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts),
                (6040, "6"), (6036, "1"), (6095, account_id), (6529, "AR.3"),
            ]);

            // Resting open orders are pushed unsolicited by CCP as 35=8 with
            // 150=0/39=0 carrying originating clientId (6119) and orderId (6121),
            // terminated by 11='*' sentinel. See ib-agent#155, ibx#191.
            hb.last_ccp_sent = Instant::now();
            log::info!("CCP reconnected, sent account/position re-subscribe");
        }
    }
}

/// Handle account update messages (cross-cutting, called from CCP message processing).
pub(crate) fn handle_account_update(msg: &[u8], context: &mut Context, shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut key: Option<&str> = None;
    for part in text.split('\x01') {
        if let Some(val) = part.strip_prefix("8001=") {
            key = Some(val);
        } else if let Some(val) = part.strip_prefix("8004=") {
            if let Some(k) = key {
                match k {
                    "NetLiquidation" => { if let Ok(v) = val.parse::<f64>() { context.account.net_liquidation = (v * PRICE_SCALE as f64) as Price; } }
                    "BuyingPower" => { if let Ok(v) = val.parse::<f64>() { context.account.buying_power = (v * PRICE_SCALE as f64) as Price; } }
                    "MaintMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.margin_used = (v * PRICE_SCALE as f64) as Price; } }
                    "UnrealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.unrealized_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    "RealizedPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.realized_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    "TotalCashValue" => { if let Ok(v) = val.parse::<f64>() { context.account.total_cash_value = (v * PRICE_SCALE as f64) as Price; } }
                    "SettledCash" => { if let Ok(v) = val.parse::<f64>() { context.account.settled_cash = (v * PRICE_SCALE as f64) as Price; } }
                    "AccruedCash" => { if let Ok(v) = val.parse::<f64>() { context.account.accrued_cash = (v * PRICE_SCALE as f64) as Price; } }
                    "EquityWithLoanValue" => { if let Ok(v) = val.parse::<f64>() { context.account.equity_with_loan = (v * PRICE_SCALE as f64) as Price; } }
                    "GrossPositionValue" => { if let Ok(v) = val.parse::<f64>() { context.account.gross_position_value = (v * PRICE_SCALE as f64) as Price; } }
                    "InitMarginReq" | "FullInitMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.init_margin_req = (v * PRICE_SCALE as f64) as Price; } }
                    "FullMaintMarginReq" => { if let Ok(v) = val.parse::<f64>() { context.account.maint_margin_req = (v * PRICE_SCALE as f64) as Price; } }
                    "AvailableFunds" | "FullAvailableFunds" => { if let Ok(v) = val.parse::<f64>() { context.account.available_funds = (v * PRICE_SCALE as f64) as Price; } }
                    "ExcessLiquidity" | "FullExcessLiquidity" => { if let Ok(v) = val.parse::<f64>() { context.account.excess_liquidity = (v * PRICE_SCALE as f64) as Price; } }
                    "Cushion" => { if let Ok(v) = val.parse::<f64>() { context.account.cushion = (v * PRICE_SCALE as f64) as Price; } }
                    "SMA" => { if let Ok(v) = val.parse::<f64>() { context.account.sma = (v * PRICE_SCALE as f64) as Price; } }
                    "DayTradesRemaining" => { if let Ok(v) = val.parse::<i64>() { context.account.day_trades_remaining = v; } }
                    "Leverage-S" | "Leverage" => { if let Ok(v) = val.parse::<f64>() { context.account.leverage = (v * PRICE_SCALE as f64) as Price; } }
                    "DailyPnL" => { if let Ok(v) = val.parse::<f64>() { context.account.daily_pnl = (v * PRICE_SCALE as f64) as Price; } }
                    _ => {}
                }
                key = None;
            }
        }
    }
    shared.portfolio.set_account(context.account());
}

/// Handle 6040=143 P&L midnight seed response.
/// Repeating group: 146={count} × (6008=conId, 6064=qtyMidnight, 6822=moneyTraded, 6099=realizedPnl).
/// These are midnight seeds for client-side daily P&L computation — NOT live P&L values.
fn handle_pnl_response(msg: &[u8], shared: &SharedState) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut seeds = Vec::new();
    let mut con_id: i64 = 0;
    let mut qty_midnight: Qty = 0;
    let mut money_traded: f64 = 0.0;
    let mut realized_pnl: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            if count > 0 && con_id != 0 {
                seeds.push(MidnightSeed { con_id, qty_midnight_fixed: qty_midnight, money_traded, realized_pnl });
            }
            con_id = v.parse().unwrap_or(0);
            qty_midnight = 0;
            money_traded = 0.0;
            realized_pnl = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty_midnight = parse_qty(v).unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("6822=") {
            // moneyTradedSinceMidnight: signed net cash, SELL positive / BUY
            // negative. Stored with the wire sign; poll_pnl adds it (ib-agent#163).
            money_traded = v.parse().unwrap_or(0.0);
        } else if let Some(v) = part.strip_prefix("6099=") {
            realized_pnl = v.parse().unwrap_or(0.0);
        }
    }
    if count > 0 && con_id != 0 {
        seeds.push(MidnightSeed { con_id, qty_midnight_fixed: qty_midnight, money_traded, realized_pnl });
    }
    shared.portfolio.set_midnight_seeds(seeds);
}

/// Handle 6040=75 position + market price feed.
/// Fires at init and after each fill. Contains repeating group: 146=count × (6008=conId, 6064=qty, 6101=avgCost).
/// The wire only carries conId/qty/avgCost — no symbol/secType. For any held conId not yet in the
/// reference cache, we issue an internal secdef request so the wrapper-facing Contract is populated
/// by the time `req_positions` is called (#154).
impl CcpState {
    pub(crate) fn handle_position_feed(
        &mut self,
        msg: &[u8],
        ccp_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<Sender<Event>>,
        hb: &mut HeartbeatState,
    ) {
    let text = match std::str::from_utf8(msg) {
        Ok(t) => t,
        Err(_) => return,
    };
    // Parse repeating group by scanning for 6008= boundaries
    let mut con_id: i64 = 0;
    let mut qty: Qty = 0;
    let mut avg_cost_raw: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            // Flush previous position if any
            if count > 0 && con_id != 0 {
                let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
                shared.portfolio.set_position_info(PositionInfo {
                    con_id, position_fixed: qty, avg_cost, ..Default::default()
                });
                if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                    shared.portfolio.set_position_fixed(instrument, qty);
                    emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
                }
                self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
            }
            con_id = v.parse().unwrap_or(0);
            qty = 0;
            avg_cost_raw = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty = parse_qty(v).unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("6101=") {
            avg_cost_raw = v.parse().unwrap_or(0.0);
        }
    }
    // Flush last position
    if count > 0 && con_id != 0 {
        let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position_fixed: qty, avg_cost, ..Default::default()
        });
        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            shared.portfolio.set_position_fixed(instrument, qty);
            emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: qty, avg_cost });
        }
        self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
    }
    }

    /// Issue an internal secdef request for `con_id` if the reference cache is cold and we
    /// haven't already auto-fetched it this session. The reply path populates the cache via
    /// the existing 35=d handler; we don't track the response.
    fn auto_fetch_secdef_if_cold(
        &mut self,
        con_id: i64,
        ccp_conn: &mut Option<Connection>,
        shared: &SharedState,
        hb: &mut HeartbeatState,
    ) {
        if con_id == 0 { return; }
        if self.auto_fetched_conids.contains(&con_id) { return; }
        if shared.reference.get_contract(con_id).is_some() { return; }
        let req_id = self.next_internal_secdef_id;
        self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
        self.auto_fetched_conids.insert(con_id);
        self.send_secdef_request(req_id, con_id, ccp_conn, hb);
    }

    /// Park a scanner result and dispatch concurrent secdef requests for every cache-miss
    /// con_id. Once all replies arrive (via `try_release_scanner_enrichments`) the result
    /// is pushed to the dispatch queue with the now-warm cache. Mirrors what the gateway
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
        if awaiting.is_empty() {
            shared.reference.push_scanner_data(api_req_id, result);
            return;
        }
        // Issue one secdef request per cold con_id. If another flow has already
        // requested the same con_id (auto_fetched_conids contains it) we skip
        // the send but still wait — its reply will populate the cache and
        // release this entry via try_release_scanner_enrichments.
        for &con_id in &awaiting {
            if !self.auto_fetched_conids.contains(&con_id) {
                let req_id = self.next_internal_secdef_id;
                self.next_internal_secdef_id = self.next_internal_secdef_id.wrapping_add(1);
                self.auto_fetched_conids.insert(con_id);
                self.send_secdef_request(req_id, con_id, ccp_conn, hb);
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
    /// enrichment's awaiting set; entries whose set becomes empty are
    /// dispatched to the scanner_data queue.
    pub(crate) fn try_release_scanner_enrichments(&mut self, con_id: i64, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        let mut idx = 0;
        while idx < self.pending_scanner_enrichment.len() {
            self.pending_scanner_enrichment[idx].awaiting.remove(&con_id);
            if self.pending_scanner_enrichment[idx].awaiting.is_empty() {
                let pe = self.pending_scanner_enrichment.swap_remove(idx);
                shared.reference.push_scanner_data(pe.api_req_id, pe.result);
            } else {
                idx += 1;
            }
        }
    }

    /// Flush scanner enrichments past their deadline, dispatching whatever
    /// entries we have (some may still have blank fields if the secdef reply
    /// never arrived). Prevents indefinite hangs on a missing reply.
    pub(crate) fn sweep_scanner_enrichments(&mut self, shared: &SharedState) {
        if self.pending_scanner_enrichment.is_empty() { return; }
        let now = Instant::now();
        let mut idx = 0;
        while idx < self.pending_scanner_enrichment.len() {
            if self.pending_scanner_enrichment[idx].deadline <= now {
                let pe = self.pending_scanner_enrichment.swap_remove(idx);
                log::warn!(
                    "scanner enrichment timeout: req_id={} missing={} con_ids; dispatching partial",
                    pe.api_req_id,
                    pe.awaiting.len(),
                );
                shared.reference.push_scanner_data(pe.api_req_id, pe.result);
            } else {
                idx += 1;
            }
        }
    }
}

/// Handle position update messages (cross-cutting, called from CCP message processing).
/// A portfolio message carries one row per position, each row starting with
/// the symbol field and repeating the same fields (ib-agent#192 D4:
/// most messages carry 2 or 3 rows). Parsing the whole message into one map
/// kept only the last row (ibx#411). A message with no symbol field is one row.
pub(crate) fn handle_portfolio_message(
    msg: &[u8],
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<Sender<Event>>,
) {
    let mut header = std::collections::HashMap::new();
    let mut rows: Vec<std::collections::HashMap<u32, String>> = Vec::new();
    for field in msg.split(|&b| b == fix::SOH) {
        let Some(eq) = field.iter().position(|&b| b == b'=') else { continue };
        let Some(tag) = std::str::from_utf8(&field[..eq]).ok().and_then(|t| t.parse::<u32>().ok()) else { continue };
        let value = String::from_utf8_lossy(&field[eq + 1..]).into_owned();
        if tag == 6068 {
            rows.push(std::collections::HashMap::new());
        }
        match rows.last_mut() {
            Some(row) => { row.insert(tag, value); }
            None => { header.insert(tag, value); }
        }
    }
    if rows.is_empty() {
        rows.push(header);
    }
    for row in &rows {
        handle_position_update(row, context, shared, event_tx);
    }
}

pub(crate) fn handle_position_update(
    parsed: &std::collections::HashMap<u32, String>,
    context: &mut Context,
    shared: &SharedState,
    event_tx: &Option<Sender<Event>>,
) {
    let con_id: i64 = match parsed.get(&6008).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    // Fixed-point: a fractional position is kept (ibx#313).
    let position: Qty = parsed.get(&6064).and_then(|s| parse_qty(s)).unwrap_or(0);
    // Tag map verified against the updatePortfolio callback (ib-agent#172):
    // 6101 = averageCost, 6065 = marketPrice (per share), 6067 = marketValue,
    // 6100 = unrealizedPNL, 6099 = realizedPNL. Earlier code read 6065 as the
    // average cost, which is actually the market price.
    let price_tag = |tag: u32| parsed.get(&tag)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| (v * PRICE_SCALE as f64) as Price)
        .unwrap_or(0);
    let avg_cost: Price = price_tag(6101);
    let market_price: Price = price_tag(6065);
    let market_value: Price = price_tag(6067);
    let unrealized_pnl: Price = price_tag(6100);
    let realized_pnl: Price = price_tag(6099);
    // Symbol arrives space-padded; trim trailing whitespace.
    let symbol = parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default();
    let sec_type = parsed.get(&167).cloned().unwrap_or_default();
    let currency = parsed.get(&15).cloned().unwrap_or_default();
    // Only the multiplier part of the row key; the whole key used to be
    // reported as the contract's multiplier.
    let multiplier = parsed.get(&8002)
        .and_then(|key| {
            let parts: Vec<&str> = key.split('/').collect();
            (parts.len() == 4).then(|| parts[2].to_string())
        })
        .unwrap_or_default();

    // Always store position info for reqPositions/pnlSingle, regardless of instrument registry.
    shared.portfolio.set_position_info(PositionInfo {
        con_id, position_fixed: position, avg_cost,
        symbol, sec_type, currency, multiplier,
        ..Default::default()
    });
    shared.portfolio.set_position_marks(con_id, market_price, market_value, unrealized_pnl, realized_pnl);

    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
        let current = context.position_fixed(instrument);
        let delta = position - current;
        if delta != 0 {
            context.update_position_fixed(instrument, delta);
        }
        shared.portfolio.set_position_fixed(instrument, position);
        emit(event_tx, Event::PositionUpdate { instrument, con_id, position_fixed: position, avg_cost });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression for ibx#198: the fill-dedup set must NOT be wiped wholesale
    // when it reaches its cap. A recently-seen ExecID has to stay deduplicated
    // so a post-reconnect server replay can't double-count the fill.
    /// The commission frame captured 25/09/2026, 25 ms after a BUY 100 AAPL
    /// fill that carried no commission (ibx#471).
    fn commission_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m: std::collections::HashMap<u32, String> = [
            (35u32, "U"), (6040, "60"), (17, "0000e0d5.6ab5f36f.01.01"), (37, "1"),
            (6381, "USD"), (6378, "1.0003"), (6099, "0"),
        ].iter().map(|(t, v)| (*t, v.to_string())).collect();
        for (tag, val) in pairs {
            m.insert(*tag, val.to_string());
        }
        m
    }

    #[test]
    fn commission_frame_gives_the_commission_report() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[]), &shared);
        let reports = shared.orders.drain_commission_reports();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.exec_id, "0000e0d5.6ab5f36f.01.01");
        assert_eq!(r.commission_and_fees, 1.0003);
        assert_eq!(r.currency, "USD");
        assert_eq!(r.realized_pnl, f64::MAX, "a realized P&L of 0 is sent as unset");
        assert_eq!(r.yield_amount, f64::MAX);
        assert_eq!(r.yield_redemption_date, "");
    }

    #[test]
    fn commission_frame_carries_realized_pnl_and_yield() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[(6099, "-12.5"), (236, "4.25"), (696, "20301231")]), &shared);
        let r = shared.orders.drain_commission_reports().remove(0);
        assert_eq!(r.realized_pnl, -12.5);
        assert_eq!(r.yield_amount, 4.25);
        assert_eq!(r.yield_redemption_date, "20301231");
    }

    #[test]
    fn commission_frame_duplicates_and_lower_revisions_are_skipped() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")]), &shared);
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.02")]), &shared);
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.01")]), &shared);
        assert_eq!(shared.orders.drain_commission_reports().len(), 1);
        // A higher revision replaces the report.
        ccp.handle_commission_report(&commission_frame(&[(17, "0000e0d5.6ab5f36f.01.0a"), (6378, "1.5")]), &shared);
        let r = shared.orders.drain_commission_reports();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].commission_and_fees, 1.5);
    }

    #[test]
    fn commission_frame_without_a_commission_is_dropped() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut frame = commission_frame(&[]);
        frame.remove(&6378);
        ccp.handle_commission_report(&frame, &shared);
        assert!(shared.orders.drain_commission_reports().is_empty());
    }

    #[test]
    fn a_fill_carries_its_execution_id() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let fills = shared.orders.drain_fills_with_exec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].1.exec_id, "0000e0d5.6ab5f36f.01.01");
    }

    // ibx#474: the captured fill of 25/09/2026 (clientId 250, orderRef
    // pm0925-fill-BUY): time from tag 60 when 6699 is absent, exchange from
    // tag 100, clientId from 6119, orderRef from 6010.
    #[test]
    fn a_fill_carries_its_execution_details() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "0000e0d5.6ab5f36f.01.01"),
            (31, "336.25"), (32, "1"), (14, "1"), (151, "0"), (6, "336.25"),
            (100, "ARCA"), (207, "ARCA"), (30, "ARCA"), (6119, "250"), (6010, "pm0925-fill-BUY"),
            (60, "20260925-08:49:54"), (52, "20260925-08:49:55.123")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let (_, exec) = shared.orders.drain_fills_with_exec().remove(0);
        assert_eq!(exec.time_secs, Some(1790326194));
        assert_eq!(exec.exchange, "ARCA");
        assert_eq!(exec.client_id, 250);
        assert_eq!(exec.order_ref, "pm0925-fill-BUY");
        assert_eq!(exec.model_code, "");
    }

    #[test]
    fn a_fill_time_prefers_6699_and_exchange_falls_back_to_207() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "e2"),
            (31, "1"), (32, "1"), (14, "1"), (151, "0"), (207, "NASDAQ"),
            (6699, "20260925-08:49:53.500"), (60, "20260925-08:49:54")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        let (_, exec) = shared.orders.drain_fills_with_exec().remove(0);
        assert_eq!(exec.time_secs, Some(1790326193));
        assert_eq!(exec.exchange, "NASDAQ");
    }

    #[test]
    fn fix_utc_time_to_unix_seconds() {
        assert_eq!(fix_utc_to_unix_secs("19700101-00:00:00"), Some(0));
        assert_eq!(fix_utc_to_unix_secs("20000301-12:30:15.250"), Some(951913815));
        assert_eq!(fix_utc_to_unix_secs("20260925 08:49:54"), None);
        assert_eq!(fix_utc_to_unix_secs("2026"), None);
    }

    #[test]
    fn split_exec_revision_reads_the_last_segment_as_hex() {
        assert_eq!(split_exec_revision("0000e0d5.6ab5f36f.01.0a"), ("0000e0d5.6ab5f36f.01", 10));
        assert_eq!(split_exec_revision("plain"), ("plain", 0));
    }

    #[test]
    fn record_exec_id_dedupes_within_window() {
        let mut ccp = CcpState::new();
        assert!(ccp.record_exec_id("exec-A"), "first sighting is new");
        assert!(!ccp.record_exec_id("exec-A"), "immediate replay is a duplicate");
    }

    #[test]
    fn record_exec_id_evicts_oldest_not_whole_set() {
        let mut ccp = CcpState::new();
        // The very first ExecID — the one a reconnect is most likely to replay.
        assert!(ccp.record_exec_id("exec-first"));
        // Push the window exactly to its cap. Together with "exec-first" this is
        // EXEC_ID_WINDOW + 1 inserts, which evicts exactly one entry: the oldest
        // ("exec-first"). Every other recent ID must remain deduplicated.
        for i in 0..EXEC_ID_WINDOW {
            assert!(ccp.record_exec_id(&format!("exec-{i}")));
        }
        assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
        // Oldest was evicted, so a replay now reads as new (unavoidable past the
        // window) — but the most recent IDs are still caught as duplicates.
        assert!(!ccp.record_exec_id("exec-0"), "recent ID still deduped");
        assert!(!ccp.record_exec_id(&format!("exec-{}", EXEC_ID_WINDOW - 1)),
            "newest ID still deduped");
    }

    // A wholesale clear() would have made "exec-first" re-insertable as new
    // after just one extra fill past the cap; assert the rolling window keeps
    // the bound without that cliff.
    #[test]
    fn record_exec_id_window_is_bounded() {
        let mut ccp = CcpState::new();
        for i in 0..(EXEC_ID_WINDOW * 3) {
            ccp.record_exec_id(&format!("exec-{i}"));
        }
        assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
        assert_eq!(ccp.exec_id_order.len(), EXEC_ID_WINDOW);
    }

    // Build a what-if (6091=1) ExecReport map for order 42. `margin_fields`
    // holds (tag, literal wire value) pairs exactly as the gateway puts them
    // on the wire (ib-agent#160).
    fn what_if_frame(margin_fields: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(11u32, "42".to_string()); // ClOrdID
        m.insert(6091u32, "1".to_string()); // what-if marker
        for (tag, val) in margin_fields {
            m.insert(*tag, val.to_string());
        }
        m
    }

    // The full six margin fields of the captured true-zero close preview
    // (ib-agent#160 scenario 2b).
    const ZERO_CLOSE_FIELDS: [(u32, &str); 6] = [
        (6826, "976.07"), (6827, "887.34"), (6828, "945924.53"),
        (6092, "0"), (6093, "0"), (6094, "945923.47"),
    ];

    fn what_if_test_state() -> (CcpState, Context, SharedState) {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order {
            order_id: 42,
            instrument,
            side: Side::Buy,
            price: 0,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (0) as i64 * crate::types::QTY_SCALE,
            status: crate::types::OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        (CcpState::new(), context, SharedState::new())
    }

    // ibx#205: a margin-reducing preview (close, cash-account sell) resolves to a
    // post-trade init margin of exactly 0, which the gateway sends as numeric "0"
    // (ib-agent#160). The old `> 0.0` guard dropped it and the caller timed out.
    #[test]
    fn what_if_zero_init_margin_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&ZERO_CLOSE_FIELDS);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1, "zero-margin preview must be delivered");
        assert_eq!(responses[0].init_margin_after, 0);
        // The completed preview consumes the pending order.
        assert!(context.order(42).is_none());
    }

    // The not-ready ack carries the literal "n/a" in all six margin fields
    // (ib-agent#160); it must be skipped so only the real data frame surfaces.
    #[test]
    fn what_if_not_ready_ack_is_skipped() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (6826, "n/a"), (6827, "n/a"), (6828, "n/a"),
            (6092, "n/a"), (6093, "n/a"), (6094, "n/a"),
        ]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(),
            "n/a ack must not surface as a response");
        // The order stays pending for the subsequent data frame.
        assert!(context.order(42).is_some());
    }

    // ibx#213: the gateway's real-frame test is "any of the six margin fields
    // is set", not "6092 is set". A preview that omits 6092 but carries
    // numeric siblings must be delivered, with the absent field read as 0.
    #[test]
    fn what_if_without_6092_but_numeric_siblings_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[(6093, "0"), (6094, "945923.47")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1, "sibling-only preview must be delivered");
        assert_eq!(responses[0].init_margin_after, 0);
        assert_eq!(responses[0].equity_with_loan_after,
            (945923.47 * PRICE_SCALE as f64) as Price);
        assert!(context.order(42).is_none());
    }

    // ibx#214: "nan" parses as f64::NAN, so it passed the old parse-success
    // gate and surfaced as a bogus zero-margin preview. The gateway treats
    // nan as unset, so an all-nan frame is not a data frame.
    #[test]
    fn what_if_nan_sentinels_are_skipped() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[
            (6826, "nan"), (6827, "nan"), (6828, "nan"),
            (6092, "nan"), (6093, "nan"), (6094, "nan"),
        ]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(),
            "all-nan frame must not surface as a response");
        assert!(context.order(42).is_some());
    }

    // Mixed frame: a nan field is unset, but one finite sibling makes the
    // frame real. The nan field itself must read as 0, not poison the price.
    #[test]
    fn what_if_nan_field_with_finite_sibling_is_delivered() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame(&[(6092, "nan"), (6094, "945923.47")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].init_margin_after, 0, "nan field reads as unset/0");
        assert_eq!(responses[0].equity_with_loan_after,
            (945923.47 * PRICE_SCALE as f64) as Price);
    }

    // ibx#210: a working order carries wire 39=0 whether it is routed or not.
    // The gateway reports PreSubmitted while it waits (e.g. placed pre-market)
    // and Submitted only once routed to an exchange. The discriminator is the
    // routing tags on the same exec report, not a distinct wire status
    // (ib-agent#162).
    fn ord_status_test_state() -> (CcpState, Context, SharedState) {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            42, instrument, Side::Buy, 1, 100 * PRICE_SCALE, b'2', b'0', 0,
        )); // starts at PendingSubmit
        (CcpState::new(), context, SharedState::new())
    }

    fn exec_report_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(11u32, "42".to_string()); // ClOrdID
        for (tag, val) in pairs {
            m.insert(*tag, val.to_string());
        }
        m
    }

    #[test]
    fn ord_status_new_unrouted_is_presubmitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // 39=0, no ExDestination, exec ref "NONE" — waiting, not yet routed.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
    }

    #[test]
    fn ord_status_new_routed_is_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        // 39=0 with the order routed to ARCA — working.
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_presubmitted_then_routed_advances_to_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&waiting, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::PreSubmitted);
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status,
            crate::types::OrderStatus::Submitted);
    }

    // ibx#247: a limit replace is acked with a pending report (39=6 on the
    // new version) and then Replaced (39=5). The reference reports no status
    // change across it (ib-agent#192 A1a); ibx used to report PendingCancel
    // and never move back, so the order looked cancelled.
    #[test]
    fn ord_status_replace_keeps_the_working_status() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
        ccp.handle_exec_report(&waiting, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();

        let pending = exec_report_frame(&[(11, "42.1"), (39, "6"), (150, "6"), (198, "NONE")]);
        ccp.handle_exec_report(&pending, &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (41, "42.0"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&replaced, &mut context, &shared, &None, "");

        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PreSubmitted);
        // Each report is reported with the unchanged status, as the
        // reference does for a modify confirm (ibx#473).
        let statuses: Vec<_> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [crate::types::OrderStatus::PreSubmitted; 2]);
    }

    // ibx#473: a stale report is still not reported (ibx#212), and the
    // update carries the server's filled quantity (tag 14).
    #[test]
    fn a_stale_report_is_not_reported_and_filled_is_the_server_count() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        let stale = exec_report_frame(&[(39, "A"), (150, "A")]);
        ccp.handle_exec_report(&stale, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_updates().is_empty(), "stale PreSubmitted after Submitted");
        let restated = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (14, "0.5"), (151, "0.5")]);
        ccp.handle_exec_report(&restated, &mut context, &shared, &None, "");
        let updates = shared.orders.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, crate::types::OrderStatus::Submitted);
        assert_eq!(updates[0].filled_qty_fixed, QTY_SCALE / 2);
    }

    // ibx#473: the update is pushed after the order cache holds this
    // report, so the client reads the same frame's order state.
    #[test]
    fn the_order_cache_holds_the_report_before_the_update() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (44, "101.5")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_updates().len(), 1);
        let info = shared.orders.get_order_info(42).expect("cached");
        assert_eq!(info.order_state.status, "Submitted");
        assert_eq!(info.order.lmt_price, 101.5);
    }

    #[test]
    fn ord_status_replace_of_a_routed_order_stays_submitted() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        let replaced = exec_report_frame(&[(11, "42.1"), (39, "5"), (150, "5"), (198, "NONE")]);
        ccp.handle_exec_report(&replaced, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::Submitted);
    }

    #[test]
    fn ord_status_pending_on_a_cancel_request_is_pending_cancel() {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        let pending = exec_report_frame(&[(11, "C42"), (39, "6"), (150, "6")]);
        ccp.handle_exec_report(&pending, &mut context, &shared, &None, "");
        assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::PendingCancel);
    }

    // ibx#472: the captured server cancel of an IOC order (25/09/2026):
    // 39=A, 39=0, 150=D|39=D, then 150=4|39=4. The 39=D report is a pending
    // cancel, and the order stays open, so a fill between the two reports is
    // applied.
    #[test]
    fn pending_cancel_report_keeps_the_order_until_cancelled() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        for pairs in [
            &[(20, "3"), (39, "A"), (150, "A")][..],
            &[(20, "0"), (39, "0"), (150, "0"), (100, "ARCA")][..],
            &[(20, "3"), (39, "D"), (150, "D"), (378, "5")][..],
        ] {
            ccp.handle_exec_report(&exec_report_frame(pairs), &mut context, &shared, &None, "");
        }
        assert_eq!(context.order(42).unwrap().status, OrderStatus::PendingCancel);
        let statuses: Vec<OrderStatus> = shared.orders.drain_order_updates().iter().map(|u| u.status).collect();
        assert_eq!(statuses, [OrderStatus::PreSubmitted, OrderStatus::Submitted, OrderStatus::PendingCancel]);

        let instrument = context.order(42).unwrap().instrument;
        let fill = exec_report_frame(&[(20, "0"), (39, "2"), (150, "F"), (17, "e1"),
            (31, "100"), (32, "1"), (14, "1"), (151, "0"), (6, "100")]);
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_fills().len(), 1, "the fill after 39=D is applied");
        assert_eq!(context.position_fixed(instrument), QTY_SCALE);
    }

    #[test]
    fn cancelled_report_after_pending_cancel_ends_the_order() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        for pairs in [
            &[(20, "0"), (39, "0"), (150, "0"), (100, "ARCA")][..],
            &[(20, "3"), (39, "D"), (150, "D")][..],
            &[(20, "3"), (39, "4"), (150, "4")][..],
        ] {
            ccp.handle_exec_report(&exec_report_frame(pairs), &mut context, &shared, &None, "");
        }
        assert!(context.order(42).is_none(), "a cancelled order leaves the engine");
        let last = shared.orders.drain_order_updates().last().map(|u| u.status);
        assert_eq!(last, Some(OrderStatus::Cancelled));
    }

    // ibx#472: the reference handles 39=E and 39=I as invalid statuses: no
    // status change and no callback.
    #[test]
    fn pending_replace_and_inactive_reports_keep_the_status() {
        use crate::types::OrderStatus;
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&routed, &mut context, &shared, &None, "");
        shared.orders.drain_order_updates();
        for code in ["E", "I"] {
            let frame = exec_report_frame(&[(20, "3"), (39, code), (150, code)]);
            ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
            assert_eq!(context.order(42).unwrap().status, OrderStatus::Submitted, "39={}", code);
        }
        assert!(shared.orders.drain_order_updates().is_empty());
    }

    // ibx#411: one portfolio message carries a row per position (ib-agent#192
    // D4). This one is captured, 3 rows; only the last was applied.
    #[test]
    fn a_portfolio_message_applies_every_position_row() {
        let mut context = Context::new();
        let shared = SharedState::new();
        let msft = context.market.try_register(272093).unwrap();
        let captured = "8=FIX.4.1|9=000999|35=UP|6529=AR.3|\
            6068=AXTI                  |6288=0|8001=PositionList|8002=AXTI/USD/1/4726868|6064=1|6067=107.50|6065=107.5|6066=1781529403|15=USD|6008=4726868|167=STK|6101=117.79|6235=107.5|6099=0.00|6100=-10.29|9821=0|6627=0|6920=0|8136=107.485466|8152=107.5|\
            6068=MSFT                  |6288=0|8001=PositionList|8002=MSFT/USD/1/272093|6064=-10|6067=-3964.40|6065=396.44000245|6066=1781529403|15=USD|6008=272093|167=STK|6101=423.53108|6235=396.44000245|6099=0.00|6100=270.91|9821=0|6627=0|6920=0|8136=396.4232483|8152=-3964.4000244|\
            6068=SPY                   |6288=0|8001=PositionList|8002=SPY/USD/1/756733|6064=18|6067=13530.35|6065=751.6859131|6066=1781529403|15=USD|6008=756733|167=STK|6101=723.43166665|6235=751.6859131|6099=0.00|6100=508.58|9821=0|6627=0|6920=0|8136=-1|8152=13530.34643555|10=000|";
        let msg = captured.replace('|', "\x01");

        handle_portfolio_message(msg.as_bytes(), &mut context, &shared, &None);

        let row = |con_id: i64| shared.portfolio.position_info(con_id).expect("row applied");
        assert_eq!((row(4726868).position_fixed / crate::types::QTY_SCALE, row(4726868).symbol.as_str()), (1, "AXTI"));
        assert_eq!(row(4726868).avg_cost, (117.79 * PRICE_SCALE as f64) as Price);
        assert_eq!((row(272093).position_fixed / crate::types::QTY_SCALE, row(272093).symbol.as_str()), (-10, "MSFT"));
        assert_eq!(row(272093).unrealized_pnl, (270.91 * PRICE_SCALE as f64) as Price);
        assert_eq!((row(756733).position_fixed / crate::types::QTY_SCALE, row(756733).symbol.as_str()), (18, "SPY"));
        // The multiplier is one part of the row key, not the whole key.
        assert_eq!(row(4726868).multiplier, "1");
        assert_eq!(context.position_fixed(msft) / crate::types::QTY_SCALE, -10, "a registered instrument's position follows its row");
    }

    // ibx#238 / ib-agent#172: in the UP portfolio snapshot the average cost is
    // tag 6101 and 6065 is the market price. The handler previously read 6065 as
    // the average cost. Verify the mapping and that all marks are stored.
    #[test]
    fn position_update_maps_marks_and_avg_cost_from_correct_tags() {
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut m = std::collections::HashMap::new();
        m.insert(6008u32, "756733".to_string());   // conId
        m.insert(6064u32, "10".to_string());        // position
        m.insert(6101u32, "100.50".to_string());    // averageCost
        m.insert(6065u32, "110.25".to_string());    // marketPrice
        m.insert(6067u32, "1102.50".to_string());   // marketValue
        m.insert(6100u32, "97.50".to_string());     // unrealizedPNL
        m.insert(6099u32, "5.00".to_string());      // realizedPNL
        handle_position_update(&m, &mut context, &shared, &None);

        let pi = shared.portfolio.position_info(756733).expect("position stored");
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 10);
        assert_eq!(pi.avg_cost, (100.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.market_price, (110.25 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.market_value, (1102.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.unrealized_pnl, (97.50 * PRICE_SCALE as f64) as Price);
        assert_eq!(pi.realized_pnl, (5.00 * PRICE_SCALE as f64) as Price);
    }

    // The lean position feed carries no marks; it must not zero the marks the
    // portfolio snapshot set (ibx#238).
    #[test]
    fn lean_position_feed_does_not_clobber_marks() {
        let shared = SharedState::new();
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 1, position_fixed: (10) as i64 * crate::types::QTY_SCALE, avg_cost: 100 * PRICE_SCALE, ..Default::default()
        });
        shared.portfolio.set_position_marks(1, 110 * PRICE_SCALE, 1100 * PRICE_SCALE, 100 * PRICE_SCALE, 5 * PRICE_SCALE);
        // Lean feed updates position + avg_cost only.
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 1, position_fixed: (12) as i64 * crate::types::QTY_SCALE, avg_cost: 101 * PRICE_SCALE, ..Default::default()
        });
        let pi = shared.portfolio.position_info(1).unwrap();
        assert_eq!(pi.position_fixed / crate::types::QTY_SCALE, 12);
        assert_eq!(pi.avg_cost, 101 * PRICE_SCALE);
        assert_eq!(pi.market_price, 110 * PRICE_SCALE, "marks survive the lean feed");
        assert_eq!(pi.market_value, 1100 * PRICE_SCALE);
        assert_eq!(pi.unrealized_pnl, 100 * PRICE_SCALE);
    }

    // ibx#220: the TIF decoder must be the exact inverse of the outbound
    // encoder. The old map decoded '7' (never emitted) as OPG and dropped
    // OPG and AUC to "".
    #[test]
    fn tif_round_trips_through_encoder_and_decoder() {
        for tif in ["DAY", "GTC", "OPG", "IOC", "FOK", "GTD", "AUC"] {
            let order = api::Order { tif: tif.to_string(), ..Default::default() };
            assert_eq!(decode_tif(order.tif_byte()), tif,
                "TIF {tif} must survive encode->decode");
        }
        // DTC shares the GTD wire byte and decodes as GTD.
        let dtc = api::Order { tif: "DTC".to_string(), ..Default::default() };
        assert_eq!(decode_tif(dtc.tif_byte()), "GTD");
        // Unknown bytes decode to empty, not a wrong TIF.
        assert_eq!(decode_tif(b'7'), "");
    }

    // ── ibx#227: contract-details deadline sweep ──

    #[test]
    fn sweep_times_out_pending_secdef_with_error_and_end() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        ccp.pending_secdef.push((7, true, past));

        ccp.sweep_contract_details(&shared, &None);

        assert!(ccp.pending_secdef.is_empty(), "expired entry must be reclaimed");
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 7);
        assert_eq!(errors[0].1, 200);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![7],
            "end must fire so a blocked wait unblocks");
    }

    #[test]
    fn sweep_drops_internal_secdef_silently() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let past = Instant::now() - std::time::Duration::from_secs(1);
        // Internal sentinel (cache auto-fetch): no user is waiting on it.
        ccp.pending_secdef.push((0xF000_0001, true, past));

        ccp.sweep_contract_details(&shared, &None);

        assert!(ccp.pending_secdef.is_empty());
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    #[test]
    fn sweep_times_out_incomplete_fanout() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        ccp.pending_fanout.push(PendingFanout {
            api_req_id: 9,
            fanout_req_ids: (0..27).map(|i| format!("ibxfan-9-{i}")).collect(),
            received: 26, // one reply lost — previously hung forever
            deadline: Instant::now() - std::time::Duration::from_secs(1),
        });

        ccp.sweep_contract_details(&shared, &None);

        assert!(ccp.pending_fanout.is_empty());
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, 9);
        assert_eq!(errors[0].1, 200);
        assert_eq!(shared.reference.drain_contract_details_end(), vec![9]);
    }

    // ── ibx#228: matching-symbols attribution ──

    fn matching_symbols_msg(req_id: &str, symbols: &[(&str, &str)]) -> Vec<u8> {
        let count = symbols.len().to_string();
        let mut fields: Vec<(u32, &str)> = vec![
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (320, req_id),
            (146, &count), // match count — marks a data frame (even when 0)
        ];
        for (sym, con_id) in symbols {
            fields.push((55, sym));
            fields.push((167, "CS"));
            fields.push((15, "USD"));
            fields.push((6008, con_id));
        }
        crate::protocol::fix::fix_build(&fields, 1)
    }

    /// A 186 frame with no match-count tag: the not-ready ack that precedes
    /// the data frame.
    fn matching_symbols_ack(req_id: &str) -> Vec<u8> {
        crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "U"),
            (6040, "186"),
            (320, req_id),
        ], 1)
    }

    fn u186_test_state() -> (CcpState, Context, SharedState) {
        (CcpState::new(), Context::new(), SharedState::new())
    }

    #[test]
    fn matching_symbols_matched_by_echoed_req_id_not_fifo() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push(1);
        ccp.pending_matching_symbols.push(2);

        // Request 2's reply arrives FIRST (out of order).
        let msg = matching_symbols_msg("2", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 2, "reply must land on the echoed req_id, not the queue head");
        assert_eq!(delivered[0].1.len(), 1);
        assert_eq!(ccp.pending_matching_symbols, vec![1]);
    }

    #[test]
    fn matching_symbols_empty_result_pops_and_delivers() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push(1);
        ccp.pending_matching_symbols.push(2);

        // Unknown pattern: zero matches. Must still pop req 1 and deliver
        // the empty answer — previously this poisoned the queue head and
        // every later reply was off by one, forever (ibx#228).
        let msg = matching_symbols_msg("1", &[]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 1);
        assert!(delivered[0].1.is_empty(), "empty result is a legitimate answer");
        assert_eq!(ccp.pending_matching_symbols, vec![2],
            "queue must not be poisoned by an empty result");

        // The next reply attributes correctly.
        let msg = matching_symbols_msg("2", &[("MSFT", "272093")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered[0].0, 2);
        assert!(ccp.pending_matching_symbols.is_empty());
    }

    #[test]
    fn matching_symbols_ack_frame_does_not_consume_the_request() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push(1);

        // The not-ready ack (no tag 146) arrives first — it must not pop the
        // request; delivering it as an empty answer orphans the data frame
        // that follows (observed live, ibx#228).
        let msg = matching_symbols_ack("1");
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert_eq!(ccp.pending_matching_symbols, vec![1]);

        // The data frame then delivers.
        let msg = matching_symbols_msg("1", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
        let delivered = shared.reference.drain_matching_symbols();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, 1);
        assert_eq!(delivered[0].1.len(), 1);
        assert!(ccp.pending_matching_symbols.is_empty());
    }

    #[test]
    fn matching_symbols_unattributable_reply_is_dropped_not_misattributed() {
        let (mut ccp, mut context, shared) = u186_test_state();
        ccp.pending_matching_symbols.push(1);
        ccp.pending_matching_symbols.push(2);

        // Echoed id matches nothing pending: with two in flight, guessing
        // would cross-attribute — drop with a warn instead.
        let msg = matching_symbols_msg("99", &[("AAPL", "265598")]);
        ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

        assert!(shared.reference.drain_matching_symbols().is_empty());
        assert_eq!(ccp.pending_matching_symbols, vec![1, 2]);
    }

    #[test]
    fn sweep_spares_live_entries() {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let future = Instant::now() + SECDEF_TIMEOUT;
        ccp.pending_secdef.push((7, true, future));
        ccp.pending_fanout.push(PendingFanout {
            api_req_id: 9,
            fanout_req_ids: vec!["ibxfan-9-0".to_string()],
            received: 0,
            deadline: future,
        });

        ccp.sweep_contract_details(&shared, &None);

        assert_eq!(ccp.pending_secdef.len(), 1);
        assert_eq!(ccp.pending_fanout.len(), 1);
        assert!(shared.reference.drain_historical_errors().is_empty());
        assert!(shared.reference.drain_contract_details_end().is_empty());
    }

    // A session-start recovery entry (150=0/39=0) for an order this session
    // does not know.
    fn recovery_frame(order_id: u64, con_id: i64) -> std::collections::HashMap<u32, String> {
        [
            (11u32, format!("{}.0", order_id)), (150, "0".into()), (39, "0".into()),
            (6008, con_id.to_string()), (55, "TEST".into()), (54, "1".into()),
            (38, "1".into()), (44, "1".into()), (40, "2".into()), (59, "1".into()),
        ].into_iter().collect()
    }

    // ibx#257: a recovered order on a new contract, with the instrument table
    // full, panicked in register() and the panic stopped the engine.
    #[test]
    fn recovery_on_a_full_instrument_table_does_not_panic() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        for i in 0..crate::types::MAX_INSTRUMENTS as i64 {
            context.market.try_register(1_000 + i).unwrap();
        }

        ccp.handle_exec_report(&recovery_frame(77, 999_999), &mut context, &shared, &None, "");
        assert!(context.order(77).is_none(), "no slot, so the order is not tracked");

        // A recovered order on a contract that already has a slot is still tracked.
        ccp.handle_exec_report(&recovery_frame(78, 1_005), &mut context, &shared, &None, "");
        let order = context.order(78).expect("tracked on its existing slot");
        assert_eq!(context.market.con_id(order.instrument), Some(1_005));
    }

    // A fill report for order 90 (BUY 300), as the server sends it: the
    // print and the order totals side by side.
    fn fill_frame(exec_id: &str, last_qty: u32, last_px: &str, cum_qty: u32, avg_px: &str, leaves: u32)
        -> std::collections::HashMap<u32, String>
    {
        let status = if leaves == 0 { "2" } else { "1" };
        [
            (11u32, "90.0".to_string()), (17, exec_id.into()), (150, status.into()), (39, status.into()),
            (55, "TEST".into()), (54, "1".into()), (38, "300".into()), (40, "2".into()), (44, "15".into()),
            (32, last_qty.to_string()), (31, last_px.into()), (14, cum_qty.to_string()),
            (6, avg_px.into()), (151, leaves.to_string()), (6008, "1005".into()),
        ].into_iter().collect()
    }

    // ibx#315: the fill carries the order totals next to the print.
    // ibx#309: the order cache's filled quantity is the filled total, not
    // the quantity still working.
    #[test]
    fn a_multi_print_fill_carries_the_order_totals() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(90, instrument, Side::Buy, 300, 15 * PRICE_SCALE, b'2', b'0', 0));

        ccp.handle_exec_report(&fill_frame("e1", 100, "10", 100, "10", 200), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&fill_frame("e2", 100, "12", 200, "11", 100), &mut context, &shared, &None, "");

        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 2);
        let second = fills[1];
        assert_eq!((second.qty_fixed / crate::types::QTY_SCALE, second.price), (100, 12 * PRICE_SCALE), "the print");
        assert_eq!((second.cum_qty_fixed / crate::types::QTY_SCALE, second.avg_price), (200, 11 * PRICE_SCALE), "the order totals");
        let info = shared.orders.get_order_info(90).expect("order cached");
        assert_eq!(info.order.filled_quantity, 200.0);
    }

    // ibx#313: fill quantities were read as whole numbers, so a fill of a
    // fraction of a share parsed to 0 and was dropped: no fill, no position
    // change. Quantities are fixed-point (QTY_SCALE) end to end.
    #[test]
    fn a_fractional_fill_is_applied() {
        let q = crate::types::QTY_SCALE;
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        // An order of 1.5 shares listed at connect (placed in another application).
        ccp.handle_exec_report(&[
            (11u32, "93.0".to_string()), (150, "0".into()), (39, "0".into()), (6008, "1005".into()),
            (55, "TEST".into()), (54, "1".into()), (38, "1.5".into()), (44, "15".into()), (40, "2".into()),
        ].into_iter().collect(), &mut context, &shared, &None, "");
        let order = context.order(93).copied().expect("listed order tracked");
        assert_eq!(order.qty_fixed, 3 * q / 2, "1.5 shares kept");

        let fill: std::collections::HashMap<u32, String> = [
            (11u32, "93.0".to_string()), (17, "e93".into()), (150, "1".into()), (39, "1".into()),
            (55, "TEST".into()), (54, "1".into()), (38, "1.5".into()), (40, "2".into()), (44, "15".into()),
            (32, "0.5".into()), (31, "15".into()), (14, "0.5".into()), (6, "15".into()), (151, "1".into()),
            (6008, "1005".into()),
        ].into_iter().collect();
        ccp.handle_exec_report(&fill, &mut context, &shared, &None, "");

        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 1, "the fill is not dropped");
        assert_eq!((fills[0].qty_fixed, fills[0].cum_qty_fixed, fills[0].remaining_fixed), (q / 2, q / 2, q));
        assert_eq!(context.order(93).unwrap().filled_fixed, q / 2);
        assert_eq!(context.position_fixed(order.instrument), q / 2, "position moved by half a share");
        assert_eq!(shared.portfolio.position_fixed(order.instrument), q / 2);
    }

    // ibx#313: a fractional position from the server is kept, in both the
    // portfolio rows and the lean position feed.
    #[test]
    fn a_fractional_position_is_kept() {
        let q = crate::types::QTY_SCALE;
        let mut context = Context::new();
        let shared = SharedState::new();
        let msg = "8=FIX.4.1|35=UP|6068=TEST|6008=1005|6064=2.5|6101=10|6065=11|15=USD|167=STK|10=000|".replace('|', "\x01");
        handle_portfolio_message(msg.as_bytes(), &mut context, &shared, &None);
        assert_eq!(shared.portfolio.position_info(1005).unwrap().position_fixed, 5 * q / 2);

        let mut ccp = CcpState::new();
        let feed = "8=FIX.4.1|35=U|6040=75|146=1|6008=1006|6064=0.25|6101=10|10=000|".replace('|', "\x01");
        ccp.handle_position_feed(feed.as_bytes(), &mut None, &mut context, &shared, &None, &mut HeartbeatState::new());
        assert_eq!(shared.portfolio.position_info(1006).unwrap().position_fixed, q / 4);
    }

    // ibx#250: a server reject reaches the caller as error 201 with the
    // server's reason (ib-agent#192 C1), once, and only for an order this
    // session tracks.
    #[test]
    fn a_server_reject_is_reported_as_error_201_with_the_reason() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.try_register(1005).unwrap();
        context.insert_order(crate::types::Order::new(91, instrument, Side::Buy, 1000, 15 * PRICE_SCALE, b'2', b'0', 0));
        let frame = |order: &str, status: &str| -> std::collections::HashMap<u32, String> {
            let mut m: std::collections::HashMap<u32, String> = [
                (11u32, format!("{}.0", order)), (150, status.to_string()), (39, status.to_string()),
                (55, "TEST".into()), (38, "1000".into()), (151, "1000".into()),
            ].into_iter().collect();
            if status == "8" {
                m.insert(58, "Display size should be a multiple of lot size".into());
            }
            m
        };

        ccp.handle_exec_report(&frame("91", "A"), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty(), "no error on the acknowledgement");
        ccp.handle_exec_report(&frame("91", "8"), &mut context, &shared, &None, "");
        assert_eq!(shared.orders.drain_order_errors(), vec![
            (91, 201, "Order rejected - reason:Display size should be a multiple of lot size".to_string()),
        ]);

        // A repeat, and a reject for an order this session does not track.
        ccp.handle_exec_report(&frame("91", "8"), &mut context, &shared, &None, "");
        ccp.handle_exec_report(&frame("92", "8"), &mut context, &shared, &None, "");
        assert!(shared.orders.drain_order_errors().is_empty());
    }

    // req_global_cancel sends a cancel-all for each id below the shared
    // instrument count. A recovered order on a new contract took a slot
    // without raising that count, so the global cancel never reached it.
    #[test]
    fn recovered_order_is_inside_the_global_cancel_range() {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        context.market.try_register(265598).unwrap();
        shared.market.set_instrument_count(context.market.count());

        ccp.handle_exec_report(&recovery_frame(79, 756733), &mut context, &shared, &None, "");

        let order = context.order(79).copied().expect("recovered order tracked");
        let count = shared.market.instrument_count();
        assert!(order.instrument < count, "instrument {} outside 0..{}", order.instrument, count);
        for instrument in 0..count {
            context.cancel_all(instrument);
        }
        assert!(context.pending_orders.drain().any(|r| matches!(r,
            crate::types::OrderRequest::CancelAll { instrument } if instrument == order.instrument)));
    }
}
