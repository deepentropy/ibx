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
    PositionInfo, Price, Side, PRICE_SCALE,
};
use crossbeam_channel::Sender;

use super::{HeartbeatState, emit, parse_price_tag};

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

fn sec_type_to_str(sec_type: crate::control::contracts::SecurityType) -> &'static str {
    match sec_type {
        crate::control::contracts::SecurityType::Stock => "STK",
        crate::control::contracts::SecurityType::Option => "OPT",
        crate::control::contracts::SecurityType::Future => "FUT",
        crate::control::contracts::SecurityType::Forex => "CASH",
        crate::control::contracts::SecurityType::Index => "IND",
        crate::control::contracts::SecurityType::Bond => "BOND",
        crate::control::contracts::SecurityType::Warrant => "WAR",
        _ => "STK",
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
    pub(crate) bulletin_next_id: i32,
    pub(crate) news_subscriptions: Vec<(InstrumentId, u32)>,
    pub(crate) disconnected: bool,
    /// (req_id, is_single_shot). Single-shot = known-conId lookup whose
    /// first 35=d reply is also the last (server emits no 323=5/6 terminator
    /// for these). Multi-record by-symbol/matching-symbols requests push
    /// `false` and rely on the response-type sentinel for is_last.
    pub(crate) pending_secdef: Vec<(u32, bool)>,
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
}

impl CcpState {
    pub(crate) fn new() -> Self {
        Self {
            seen_exec_ids: HashSet::with_capacity(256),
            exec_id_order: VecDeque::with_capacity(256),
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
                        hb.pending_ccp_test = None;
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
                                if !matches.is_empty() {
                                    if let Some(req_id) = self.pending_matching_symbols.first().copied() {
                                        self.pending_matching_symbols.remove(0);
                                        shared.reference.push_matching_symbols(req_id, matches);
                                    }
                                }
                            }
                        }
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
            "UP" => handle_position_update(&parsed, context, shared, event_tx),
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
                            let sec_type_str = sec_type_to_str(def.sec_type);
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
                        shared.reference.push_contract_details(api_req_id, def.clone());
                        emit(event_tx, Event::ContractDetails { req_id: api_req_id, details: def });
                        self.pending_fanout[idx].received += 1;
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
                        let sec_type_str = sec_type_to_str(def.sec_type);
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
                            self.pending_secdef.iter().position(|(pid, _)| *pid == rid_u32)
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
                                shared.reference.push_contract_details(req_id, def.clone());
                                emit(event_tx, Event::ContractDetails { req_id, details: def });
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
                            self.pending_secdef.retain(|(rid, ss)| !(*rid == req_id && !*ss));
                            if fanout_exchanges.is_empty() || con_id == 0 {
                                shared.reference.push_contract_details_end(req_id);
                                emit(event_tx, Event::ContractDetailsEnd(req_id));
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
            let qty: u32 = parsed.get(&38).and_then(|s| s.parse::<f64>().ok()).map(|q| q as u32).unwrap_or(0);
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
            if con_id != 0 && qty > 0 {
                let instrument = context.register_instrument(con_id);
                if let Some(sym) = parsed.get(&55) {
                    context.set_symbol(instrument, sym.clone());
                }
                context.insert_order(crate::types::Order {
                    order_id: clord_id,
                    instrument,
                    side,
                    price: limit_price_i64,
                    qty,
                    filled: 0,
                    status: crate::types::OrderStatus::Submitted,
                    ord_type: ord_type_byte,
                    tif: tif_byte,
                    stop_price: stop_price_i64,
                });
                log::info!("CCP recovery: inserted orderId={} sym={:?} side={:?} qty={} px={}",
                    clord_id, parsed.get(&55), side, qty,
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
        // single data frame — so accept the first frame whose 6092 parses, with
        // no assumption that an ack precedes it. Mirrors the gateway's own
        // real-frame test (a field is "set" when it parses). Captured byte-level
        // in ib-agent#160; 6094 is a present-and-numeric fallback if ever needed.
        if parsed.get(&6091).map(|s| s.as_str()) == Some("1") {
            if parsed.get(&6092).and_then(|s| s.parse::<f64>().ok()).is_some() {
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
        let last_shares = parsed.get(&32).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let leaves_qty = parsed.get(&151).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
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

        let status = match ord_status {
            "0" | "5" => crate::types::OrderStatus::Submitted,
            "A" => crate::types::OrderStatus::PreSubmitted,
            "E" => crate::types::OrderStatus::PendingReplace,
            "6" => crate::types::OrderStatus::PendingCancel,
            "1" => crate::types::OrderStatus::PartiallyFilled,
            "2" => crate::types::OrderStatus::Filled,
            "4" | "C" | "D" => crate::types::OrderStatus::Cancelled,
            "8" => crate::types::OrderStatus::Rejected,
            "I" => crate::types::OrderStatus::Inactive,
            _ => {
                log::warn!("Unknown order status 39={} for order {}", ord_status, clord_id);
                return;
            }
        };

        let prev_status = context.order(clord_id).map(|o| o.status);
        let status_changed = prev_status != Some(status);
        context.update_order_status(clord_id, status);

        let mut had_fill = false;
        if matches!(exec_type, "F" | "1" | "2") && last_shares > 0 {
            if !exec_id.is_empty() && !self.record_exec_id(exec_id) {
                log::warn!("Duplicate ExecID={} — skipping fill", exec_id);
                return;
            }
            if let Some(order) = context.order(clord_id).copied() {
                context.update_order_filled(clord_id, last_shares as u32);
                let fill = Fill {
                    instrument: order.instrument,
                    order_id: clord_id,
                    side: order.side,
                    price: (last_px * PRICE_SCALE as f64) as i64,
                    qty: last_shares,
                    remaining: leaves_qty,
                    commission: (commission * PRICE_SCALE as f64) as i64,
                    timestamp_ns: context.now_ns(),
                };
                let delta = match order.side {
                    Side::Buy => last_shares,
                    Side::Sell | Side::ShortSell => -last_shares,
                };
                context.update_position(order.instrument, delta);
                // notify_fill inlined
                shared.orders.push_fill(fill);
                shared.portfolio.set_position(fill.instrument, context.position(fill.instrument));
                emit(event_tx, Event::Fill(fill));
                had_fill = true;
            }
        }

        if status_changed && !had_fill {
            if let Some(order) = context.order(clord_id).copied() {
                let perm_id: i64 = parsed.get(&37).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                let parent_id: i64 = parsed.get(&583).map(|s| perm_id_from_fix_order_id(s)).unwrap_or(0);
                let update = crate::types::OrderUpdate {
                    order_id: clord_id,
                    instrument: order.instrument,
                    status,
                    filled_qty: order.filled as i64,
                    remaining_qty: leaves_qty,
                    perm_id,
                    parent_id,
                    timestamp_ns: context.now_ns(),
                };
                shared.orders.push_order_update(update);
                emit(event_tx, Event::OrderUpdate(update));
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
                let t = match ctx_order.tif {
                    b'0' => "DAY", b'1' => "GTC", b'3' => "IOC", b'4' => "FOK",
                    b'7' => "OPG", b'6' => "GTD", _ => "",
                };
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
                filled_quantity: leaves_qty as f64,
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
                    filled_qty: order.filled as i64,
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
            let restore_status = if order.filled > 0 {
                crate::types::OrderStatus::PartiallyFilled
            } else {
                crate::types::OrderStatus::Submitted
            };
            context.update_order_status(oid, restore_status);
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
            shared.reference.push_contract_details(p.api_req_id, p.def.clone());
            emit(event_tx, Event::ContractDetails {
                req_id: p.api_req_id,
                details: p.def,
            });
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
        shared.reference.push_contract_details(pair.api_req_id, pair.def.clone());
        emit(event_tx, Event::ContractDetails {
            req_id: pair.api_req_id,
            details: pair.def,
        });
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
        }
        // Known-conId lookup: single record, no paginated terminator.
        self.pending_secdef.push((req_id, true));
    }

    pub(crate) fn send_secdef_request_by_symbol(&mut self, req_id: u32, symbol: &str, sec_type: &str, exchange: &str, currency: &str, ccp_conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        if let Some(conn) = ccp_conn.as_mut() {
            let req_id_str = req_id.to_string();
            let ts = chrono_free_timestamp();
            let fix_exchange = if exchange == "SMART" { "BEST" } else { exchange };
            let fix_sec_type = match sec_type {
                "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND", other => other,
            };
            let _ = conn.send_fix(&[
                (fix::TAG_MSG_TYPE, "c"),
                (fix::TAG_SENDING_TIME, &ts),
                (320, &req_id_str),
                (321, "2"),
                (55, symbol),
                (167, fix_sec_type),
                (207, fix_exchange),
                (15, currency),
                (6088, "Socket"),
            ]);
            log::info!("Sent secdef-by-symbol: req_id={} symbol={} sec_type={}", req_id, symbol, sec_type);
            hb.last_ccp_sent = Instant::now();
        }
        // By-symbol lookup: master reply carries `6046={exch_list}`. The
        // server never emits a 323=5/6 terminator; completion is detected
        // by counting per-exchange fan-out replies (see `pending_fanout`).
        self.pending_secdef.push((req_id, false));
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
    let mut qty_midnight: i64 = 0;
    let mut money_traded: f64 = 0.0;
    let mut realized_pnl: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            if count > 0 && con_id != 0 {
                seeds.push(MidnightSeed { con_id, qty_midnight, money_traded, realized_pnl });
            }
            con_id = v.parse().unwrap_or(0);
            qty_midnight = 0;
            money_traded = 0.0;
            realized_pnl = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty_midnight = v.parse::<f64>().unwrap_or(0.0) as i64;
        } else if let Some(v) = part.strip_prefix("6822=") {
            money_traded = v.parse().unwrap_or(0.0);
        } else if let Some(v) = part.strip_prefix("6099=") {
            realized_pnl = v.parse().unwrap_or(0.0);
        }
    }
    if count > 0 && con_id != 0 {
        seeds.push(MidnightSeed { con_id, qty_midnight, money_traded, realized_pnl });
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
    let mut qty: i64 = 0;
    let mut avg_cost_raw: f64 = 0.0;
    let mut count = 0;
    for part in text.split('\x01') {
        if let Some(v) = part.strip_prefix("6008=") {
            // Flush previous position if any
            if count > 0 && con_id != 0 {
                let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
                shared.portfolio.set_position_info(PositionInfo {
                    con_id, position: qty, avg_cost, ..Default::default()
                });
                if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                    shared.portfolio.set_position(instrument, qty);
                    emit(event_tx, Event::PositionUpdate { instrument, con_id, position: qty, avg_cost });
                }
                self.auto_fetch_secdef_if_cold(con_id, ccp_conn, shared, hb);
            }
            con_id = v.parse().unwrap_or(0);
            qty = 0;
            avg_cost_raw = 0.0;
            count += 1;
        } else if let Some(v) = part.strip_prefix("6064=") {
            qty = v.parse::<f64>().unwrap_or(0.0) as i64;
        } else if let Some(v) = part.strip_prefix("6101=") {
            avg_cost_raw = v.parse().unwrap_or(0.0);
        }
    }
    // Flush last position
    if count > 0 && con_id != 0 {
        let avg_cost = (avg_cost_raw * PRICE_SCALE as f64) as Price;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position: qty, avg_cost, ..Default::default()
        });
        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            shared.portfolio.set_position(instrument, qty);
            emit(event_tx, Event::PositionUpdate { instrument, con_id, position: qty, avg_cost });
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
    let position: i64 = parsed.get(&6064)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v as i64)
        .unwrap_or(0);
    let avg_cost: Price = parsed.get(&6065)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| (v * PRICE_SCALE as f64) as Price)
        .unwrap_or(0);
    // Symbol arrives space-padded; trim trailing whitespace.
    let symbol = parsed.get(&6068).map(|s| s.trim_end().to_string()).unwrap_or_default();
    let sec_type = parsed.get(&167).cloned().unwrap_or_default();
    let currency = parsed.get(&15).cloned().unwrap_or_default();
    let multiplier = parsed.get(&8002).cloned().unwrap_or_default();

    // Always store position info for reqPositions/pnlSingle, regardless of instrument registry.
    shared.portfolio.set_position_info(PositionInfo {
        con_id, position, avg_cost,
        symbol, sec_type, currency, multiplier,
    });

    if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
        let current = context.position(instrument);
        let delta = position - current;
        if delta != 0 {
            context.update_position(instrument, delta);
        }
        shared.portfolio.set_position(instrument, position);
        emit(event_tx, Event::PositionUpdate { instrument, con_id, position, avg_cost });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression for ibx#198: the fill-dedup set must NOT be wiped wholesale
    // when it reaches its cap. A recently-seen ExecID has to stay deduplicated
    // so a post-reconnect server replay can't double-count the fill.
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

    // Build a what-if (6091=1) ExecReport map for order 42 with the given
    // post-trade init-margin literal exactly as the gateway puts it on the wire.
    fn what_if_frame(init_margin_after: &str) -> std::collections::HashMap<u32, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(11u32, "42".to_string()); // ClOrdID
        m.insert(6091u32, "1".to_string()); // what-if marker
        m.insert(6826u32, "976.07".to_string()); // init_margin_before
        m.insert(6092u32, init_margin_after.to_string()); // init_margin_after
        m
    }

    fn what_if_test_state() -> (CcpState, Context, SharedState) {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order {
            order_id: 42,
            instrument,
            side: Side::Buy,
            price: 0,
            qty: 100,
            filled: 0,
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
        let frame = what_if_frame("0");
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        let responses = shared.orders.drain_what_if_responses();
        assert_eq!(responses.len(), 1, "zero-margin preview must be delivered");
        assert_eq!(responses[0].init_margin_after, 0);
        // The completed preview consumes the pending order.
        assert!(context.order(42).is_none());
    }

    // The not-ready ack carries the literal "n/a" in the margin fields; it must
    // be skipped (parse fails) so only the real data frame surfaces.
    #[test]
    fn what_if_not_ready_ack_is_skipped() {
        let (mut ccp, mut context, shared) = what_if_test_state();
        let frame = what_if_frame("n/a");
        ccp.handle_exec_report(&frame, &mut context, &shared, &None, "");
        assert!(shared.orders.drain_what_if_responses().is_empty(),
            "n/a ack must not surface as a response");
        // The order stays pending for the subsequent data frame.
        assert!(context.order(42).is_some());
    }
}
